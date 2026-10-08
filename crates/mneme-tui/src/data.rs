//! A serial, read-only window into an existing owner. Never opens a store.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use mneme_mcp_client::{ClientTimeouts, ConnectionOptions, RemoteClient};
use serde_json::{Value, json};
use tokio::sync::mpsc::{Receiver, Sender};

use crate::model::{Activity, Edge, Graph, Node, Request, Response, Target, clean};
use crate::{scenes, touchstones};

mod inventory;
mod touchstone_reads;

type Error = Box<dyn std::error::Error + Send + Sync>;
const QUERY_NODES: usize = 16;
const NEIGHBORS: usize = 32;
const SUMMARY_BYTES: usize = 2048;
const BODY_BYTES: usize = 8192;
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Observer {
    instance: Option<String>,
    after: u64,
    dropped: u64,
}

/// Only explicit user requests fetch memory; idle polling reads registry counters.
/// A failed request is never retried. The next request starts a new connection,
/// but remains pinned to the first verified database identity.
pub async fn serve(target: Target, mut requests: Receiver<Request>, responses: Sender<Response>) {
    let mut client = None;
    let mut pinned_id = target.expected_db_id.clone();
    let mut observer = Observer::default();
    let mut verified_connection = false;
    while let Some(request) = requests.recv().await {
        if matches!(request, Request::Shutdown) {
            break;
        }
        if matches!(request, Request::Cancel) {
            // A read-only queue barrier, not transport cancellation. The current
            // call finishes before this acknowledgment; keep its verified session.
            if responses.send(Response::Canceled).await.is_err() {
                break;
            }
            continue;
        }
        let response = match execute(
            &target,
            &request,
            &mut client,
            &mut pinned_id,
            &mut observer,
            &mut verified_connection,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                if let Some(mut connection) = client.take() {
                    connection.close().await;
                }
                Response::Error(text(&error.to_string(), 1024))
            }
        };
        if responses.send(response).await.is_err() {
            break;
        }
    }
    if let Some(mut connection) = client {
        connection.close().await;
    }
}

async fn execute(
    target: &Target,
    request: &Request,
    client: &mut Option<RemoteClient>,
    pinned_id: &mut Option<String>,
    observer: &mut Observer,
    verified_connection: &mut bool,
) -> Result<Response, Error> {
    validate_request(request)?;
    if client.is_none() {
        *verified_connection = false;
        *client = Some(
            RemoteClient::connect_with_timeouts(
                &ConnectionOptions {
                    url: target.url.clone(),
                    ssh_mcp_port: target.ssh_mcp_port,
                    token_env: target.token_env.clone(),
                },
                ClientTimeouts {
                    connect: TIMEOUT,
                    request: TIMEOUT,
                },
            )
            .await?,
        );
    }
    let connection = client.as_mut().ok_or("memory connection unavailable")?;
    // The first successful registry binds alias, identity and selected path.
    // Identity-guarded bulk reads reuse that connection without redundant
    // registry RPCs. Poll still observes live counters/state explicitly.
    let verified_bulk = matches!(
        request,
        Request::Inventory { .. } | Request::Summaries { .. }
    ) && *verified_connection
        && pinned_id.is_some();
    let (identity, mut activity) = if verified_bulk {
        (pinned_id.clone().unwrap(), Activity::default())
    } else {
        let catalog = connection.call_tool("databases", json!({})).await?;
        let (identity, activity) = registry(&catalog, &target.database, pinned_id.as_deref())?;
        if let Some(expected_path) = target.expected_path.as_deref() {
            // Registry validation above established one unambiguous alias. A
            // selected global core additionally pins the configured store path;
            // do not trust an unrelated owner merely because it also has `user`.
            let actual_path = catalog.as_array().and_then(|rows| {
                rows.iter()
                    .find(|row| row["db"] == target.database || row["name"] == target.database)
                    .and_then(|row| row["resolved_path"].as_str())
            });
            if actual_path != Some(expected_path) {
                return Err("selected global database path does not match the owner registry; no memory was read".into());
            }
        }
        (identity, activity)
    };
    *pinned_id = Some(identity.clone());
    *verified_connection = true;
    match request {
        Request::Inventory { after } => {
            inventory::page(connection, target, &identity, after.as_deref()).await
        }
        Request::Summaries { ids } => {
            inventory::summaries(connection, target, &identity, ids).await
        }
        Request::Poll => {
            // Older owners advertise no feed: counters remain useful without
            // probing unsupported tools or changing the server's capabilities.
            if connection.advertises("activity") {
                match connection
                    .call_tool("activity", json!({"after":observer.after,"limit":32}))
                    .await
                {
                    Ok(page) => {
                        if observe(&page, &identity, observer, &mut activity).is_err() {
                            activity.state = "open · activity feed unavailable".into();
                            activity.feed = false;
                            activity.missed = true;
                        }
                    }
                    Err(_) => {
                        activity.state = "open · activity feed unavailable".into();
                        activity.missed = true;
                        // call_tool already closed this failed transport. The
                        // next request may reconnect, preserving identity/cursor.
                        *client = None;
                    }
                }
            }
            Ok(Response::Activity(activity))
        }
        Request::Query(query) => {
            require_guard(connection, "query", None)?;
            require_guard(connection, "neighbors", None)?;
            let result = connection
                .call_tool(
                    "query",
                    json!({"db":target.database,"expected_db_id":identity,"text":query,"k":QUERY_NODES,"depth":0,"max_nodes":QUERY_NODES}),
                )
                .await?;
            let mut graph = query_graph(&result)?;
            if !query_edges(connection, &target.database, &identity, &mut graph, TIMEOUT).await {
                // An optional edge read must not discard successful search
                // hits, nor leave a failed/cancelled exchange reusable.
                *client = None;
            }
            Ok(Response::Loaded(graph))
        }
        Request::Focus(id) | Request::Lens(id) => {
            require_guard(connection, "get", None)?;
            require_guard(connection, "neighbors", None)?;
            let mut args = json!({"db":target.database,"expected_db_id":identity,"id":id,"body":matches!(request,Request::Focus(_))});
            if matches!(request, Request::Focus(_)) {
                args["max_body_bytes"] = json!(BODY_BYTES);
            }
            let node = connection.call_tool("get", args).await?;
            let neighbors = connection
                .call_tool(
                    "neighbors",
                    json!({"db":target.database,"expected_db_id":identity,"id":id}),
                )
                .await?;
            let mut graph = focus_graph(id, &node, &neighbors)?;
            if matches!(request, Request::Lens(_)) {
                for card in &mut graph.nodes {
                    card.body.clear();
                }
            }
            Ok(Response::Loaded(graph))
        }
        Request::Scenes { axis, cue } => {
            if !connection.advertises("episode") {
                return Err("selected owner does not advertise native scene reads".into());
            }
            let mut arguments =
                json!({"db":target.database,"expected_db_id":identity,"limit":scenes::PAGE_ITEMS});
            if let Some(cue) = cue {
                arguments["action"] = json!("search");
                arguments["cue"] = json!(cue);
            } else {
                arguments["action"] = json!("list");
                arguments["axis"] = json!(axis.as_str());
                arguments["order"] = json!("newest_first");
            }
            let page = connection.call_tool("episode", arguments).await?;
            Ok(Response::Scenes(scenes::parse_page(
                &page,
                &identity,
                *axis,
                cue.clone(),
            )?))
        }
        Request::Scene {
            episode_id,
            edition_id,
        } => {
            if !connection.advertises("episode") {
                return Err("selected owner does not advertise native scene reads".into());
            }
            // Open the exact displayed edition. A concurrent revision must not
            // silently rewrite the experience the user chose to inspect.
            let detail = connection
                .call_tool(
                    "episode",
                    json!({
                        "db":target.database,"expected_db_id":identity,"action":"get",
                        "episode_id":episode_id,"edition_id":edition_id,"body":true,
                        "max_bytes":scenes::BODY_BYTES
                    }),
                )
                .await?;
            Ok(Response::Scene(scenes::parse_detail(
                &detail, &identity, episode_id, edition_id,
            )?))
        }
        Request::Touchstones { after } => {
            touchstone_reads::page(connection, target, &identity, after).await
        }
        Request::Touchstone { id } => {
            touchstone_reads::annotation(connection, target, &identity, id).await
        }
        Request::TouchstoneTarget { db_id, id } => {
            touchstone_reads::exact_target(connection, target, &identity, db_id, id).await
        }
        Request::Cancel | Request::Shutdown => {
            unreachable!("shutdown is handled before connecting")
        }
    }
}

/// Enrich only the fixed search field, never growing it or inventing proximity
/// edges. All serial reads share one deadline; this is not a per-node timeout.
/// False means the connection was failed/cancelled and must be discarded.
async fn query_edges(
    connection: &mut RemoteClient,
    database: &str,
    identity: &str,
    graph: &mut Graph,
    budget: Duration,
) -> bool {
    if graph.nodes.len() < 2 {
        return true;
    }
    let mut ids = graph
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    let field = ids.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut edges = BTreeMap::new();
    let mut partial = false;
    let deadline = tokio::time::Instant::now() + budget;
    let result = tokio::time::timeout_at(deadline, async {
        for id in &ids {
            let page = connection
                .call_tool(
                    "neighbors",
                    json!({"db":database,"expected_db_id":identity,"id":id}),
                )
                .await?;
            let (items, more) = neighbor_items(&page)?;
            partial |= more || items.len() > NEIGHBORS;
            for item in items.iter().take(NEIGHBORS) {
                let edge = neighbor_edge(id, item)?;
                if !field.contains(edge.from.as_str()) || !field.contains(edge.to.as_str()) {
                    continue;
                }
                let key = (edge.from.clone(), edge.to.clone(), edge.kind.clone());
                edges.entry(key).or_insert(edge);
            }
        }
        Ok::<(), Error>(())
    })
    .await;
    // At most 16 * 32 candidate rows were read. Canonical ordering makes the
    // displayed 32 independent of each page's adjacency order.
    partial |= edges.len() > NEIGHBORS;
    graph.edges = edges.into_values().take(NEIGHBORS).collect();
    let failure = match result {
        Ok(Ok(())) => None,
        Ok(Err(_)) => Some(" · links incomplete (edge read failed)"),
        Err(_) => Some(" · links incomplete (read budget reached)"),
    };
    if let Some(note) = failure {
        graph.partial = true;
        graph.note.push_str(note);
        // The read budget has expired, but HTTP sessions are not released by
        // Drop (only SSH children are). Give DELETE an independent bounded
        // cleanup phase; this is not more retrieval time or an automatic retry.
        // RemoteClient::close itself bounds DELETE at 3s; the 4s outer guard
        // also protects this caller if that implementation later changes.
        if tokio::time::timeout(Duration::from_secs(4), connection.close())
            .await
            .is_err()
        {
            graph.note.push_str(" · session cleanup timed out");
        }
        false
    } else {
        graph.partial |= partial;
        if partial {
            graph.note.push_str(" · links partial (32-edge bounds)");
        }
        true
    }
}

fn require_guard(connection: &RemoteClient, tool: &str, action: Option<&str>) -> Result<(), Error> {
    if !connection.supports_expected_db_id(tool, action) {
        return Err(format!(
            "selected owner does not advertise identity-guarded {tool}; update its mneme-mcp server"
        )
        .into());
    }
    Ok(())
}

fn validate_request(request: &Request) -> Result<(), Error> {
    match request {
        Request::Query(query) if query.trim().is_empty() || query.len() > 4096 => {
            Err("search text must be nonblank and at most 4096 UTF-8 bytes".into())
        }
        Request::Inventory { after: Some(after) } if after.is_empty() || after.len() > 2048 => {
            Err("inventory cursor must be 1..=2048 UTF-8 bytes".into())
        }
        Request::Summaries { ids } => {
            if ids.is_empty() || ids.len() > inventory::SUMMARY_ITEMS {
                return Err("summary batch must contain 1..=64 node ids".into());
            }
            let mut seen = HashSet::new();
            for id in ids {
                validate_id(id)?;
                if !seen.insert(id) {
                    return Err("summary batch ids must be unique".into());
                }
            }
            Ok(())
        }
        Request::Focus(id) | Request::Lens(id) => validate_id(id),
        Request::Touchstone { id } => touchstones::canonical_id(id),
        Request::TouchstoneTarget { db_id, id } => {
            touchstones::canonical_id(db_id)?;
            touchstones::canonical_id(id)
        }
        Request::Touchstones { after: Some(after) } if after.is_empty() || after.len() > 1024 => {
            Err("touchstone cursor must be 1..=1024 UTF-8 bytes".into())
        }
        Request::Scenes { cue: Some(cue), .. } if cue.trim().is_empty() || cue.len() > 4096 => {
            Err("scene cue must be nonblank and at most 4096 UTF-8 bytes".into())
        }
        Request::Scene {
            episode_id,
            edition_id,
        } => {
            validate_id(episode_id)?;
            validate_id(edition_id)
        }
        _ => Ok(()),
    }
}

fn validate_id(id: &str) -> Result<(), Error> {
    if id.len() != 26
        || !matches!(id.as_bytes()[0], b'0'..=b'7')
        || !id
            .bytes()
            .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b.to_ascii_uppercase()))
    {
        return Err("memory node id must be a ULID".into());
    }
    Ok(())
}

fn registry(
    value: &Value,
    database: &str,
    expected: Option<&str>,
) -> Result<(String, Activity), Error> {
    let rows = value.as_array().ok_or("memory registry is not an array")?;
    let mut matches = rows
        .iter()
        .filter(|row| row["db"] == database || row["name"] == database);
    let row = matches
        .next()
        .ok_or("selected database is not advertised by this owner")?;
    if matches.next().is_some() {
        return Err("owner advertised an ambiguous database alias".into());
    }
    let identity = required_text(row, "db_id")?;
    validate_id(identity)?;
    if expected.is_some_and(|expected| expected != identity) {
        return Err(
            "database identity changed; reopen the observatory to select this source again".into(),
        );
    }
    let state = required_text(row, "state")?;
    if state != "open" {
        return Err(format!(
            "selected database is {}; no memory was read",
            text(state, 64)
        )
        .into());
    }
    Ok((
        identity.to_owned(),
        Activity {
            state: text(state, 64),
            in_flight: row["in_flight"]
                .as_u64()
                .ok_or("registry omitted in-flight counter")?,
            backend_jobs: row["backend_jobs"]
                .as_u64()
                .ok_or("registry omitted backend-jobs counter")?,
            ..Activity::default()
        },
    ))
}

fn observe(
    page: &Value,
    database_id: &str,
    observer: &mut Observer,
    activity: &mut Activity,
) -> Result<(), Error> {
    if page["schema"] != "mneme.activity.v1" {
        return Err("unsupported activity feed schema".into());
    }
    let instance = required_text(page, "instance")?;
    validate_id(instance)?;
    let busy = page["busy"]
        .as_bool()
        .ok_or("activity feed omitted busy marker")?;
    if busy {
        activity.feed = true;
        return Ok(());
    }
    let next = page["next_after"]
        .as_u64()
        .ok_or("activity feed omitted cursor")?;
    let dropped = page["dropped"]
        .as_u64()
        .ok_or("activity feed omitted drop count")?;
    let latest = if page["latest_seq"].is_null() {
        None
    } else {
        Some(
            page["latest_seq"]
                .as_u64()
                .ok_or("activity feed has invalid latest sequence")?,
        )
    };
    let events = page["events"]
        .as_array()
        .ok_or("activity feed omitted events")?;
    let missed = page["missed"]
        .as_bool()
        .ok_or("activity feed omitted missed marker")?;
    let more = page["has_more"]
        .as_bool()
        .ok_or("activity feed omitted has_more marker")?;
    // Opening a viewer or restarting an owner establishes a baseline, not a
    // burst of fictional present-tense activity from the historical ring.
    if observer.instance.as_deref() != Some(instance) {
        observer.instance = Some(instance.to_owned());
        observer.after = latest.unwrap_or(next);
        observer.dropped = dropped;
        activity.feed = true;
        activity.missed = missed || more;
        return Ok(());
    }
    let mut found_ids = Vec::new();
    let mut seen = HashSet::new();
    let mut returns = 0;
    let mut partial = missed || more || dropped > observer.dropped || events.len() > 32;
    for event in events.iter().take(32) {
        if event["db_id"].as_str() != Some(database_id) {
            continue;
        }
        let seq = event["seq"]
            .as_u64()
            .ok_or("activity event omitted sequence")?;
        if seq <= observer.after {
            continue;
        }
        returns += 1;
        partial |= event["node_ids_truncated"] == true || event["db_truncated"] == true;
        let ids = event["node_ids"]
            .as_array()
            .ok_or("activity event omitted node ids")?;
        partial |= ids.len() > 32;
        for id in ids.iter().take(32) {
            let id = id.as_str().ok_or("activity event has invalid node id")?;
            validate_id(id)?;
            if seen.insert(id.to_owned()) {
                if found_ids.len() < 512 {
                    found_ids.push(id.to_owned());
                } else {
                    partial = true;
                }
            }
        }
    }
    observer.after = next;
    observer.dropped = dropped;
    activity.returns = returns;
    activity.node_ids = found_ids;
    activity.feed = true;
    activity.missed = partial;
    Ok(())
}

fn required_text<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("memory response omitted {key}").into())
}

/// Cap UTF-8 on a character boundary before removing terminal controls.
fn text(value: &str, max_bytes: usize) -> String {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    clean(&value[..end])
}

fn optional_text(value: &Value, key: &str, max_bytes: usize) -> String {
    value[key]
        .as_str()
        .map(|v| text(v, max_bytes))
        .unwrap_or_default()
}

/// Cabinet reads retain native bounded summaries rather than the map's excerpt.
pub(crate) fn touchstone_node(value: &Value) -> Result<Node, Error> {
    let mut found = node(value)?;
    touchstones::canonical_id(&found.id)?;
    let summary = required_text(value, "summary")?;
    if summary.len() > 16 * 1024 {
        return Err("touchstone node summary exceeds native bound".into());
    }
    found.summary = clean(summary);
    Ok(found)
}

/// Shared bounded tag evidence for full cards and lightweight topology.
/// Missing older-owner fields remain unknown; no summary/body hydration implied.
fn tag_metadata(value: &Value) -> (Vec<String>, bool) {
    let tags_complete = matches!(value["tags_truncated"], Value::Null | Value::Bool(false))
        && value["tags"].as_array().is_some_and(|tags| {
            tags.len() <= 16
                && tags.iter().all(|tag| {
                    tag.as_str()
                        .is_some_and(|tag| tag.len() <= 128 && !tag.chars().any(char::is_control))
                })
        });
    let tags = value["tags"]
        .as_array()
        .map(|tags| {
            tags.iter()
                .filter_map(Value::as_str)
                .take(16)
                .map(|tag| text(tag, 128))
                .collect()
        })
        .unwrap_or_default();
    (tags, tags_complete)
}

pub(crate) fn node(value: &Value) -> Result<Node, Error> {
    let id = required_text(value, "id")?;
    validate_id(id)?;
    let (tags, tags_complete) = tag_metadata(value);
    let provenance = if value["provenance"].is_null() {
        String::new()
    } else {
        text(&value["provenance"].to_string(), SUMMARY_BYTES)
    };
    Ok(Node {
        id: id.to_owned(),
        summary: text(required_text(value, "summary")?, SUMMARY_BYTES),
        body: optional_text(value, "body", BODY_BYTES),
        tags,
        tags_complete,
        status: optional_text(value, "status", 64),
        confidence: value["confidence"]
            .as_f64()
            .filter(|value| value.is_finite() && (0.0..=1.0).contains(value)),
        provenance,
    })
}

fn query_graph(value: &Value) -> Result<Graph, Error> {
    if value["schema"] != "mneme.query.v3" {
        return Err("unsupported memory query response schema".into());
    }
    let mut graph = Graph {
        partial: value["partial"]
            .as_bool()
            .ok_or("query response omitted partial marker")?,
        note:
            "Search hits · stored links within this field · best-effort sample, not an atomic snapshot"
                .into(),
        ..Graph::default()
    };
    let mut seen = HashSet::new();
    if value["lanes"]
        .as_object()
        .is_none_or(|lanes| lanes.len() != 1 || !lanes.contains_key("primary"))
    {
        return Err("query response has obsolete or unknown lanes".into());
    }
    if value["lanes"]["primary"].get("seed_coverage").is_none() {
        return Err("query response omitted primary seed coverage".into());
    }
    for lane in ["primary"] {
        let hits = value["lanes"][lane]["hits"]
            .as_array()
            .ok_or("query response omitted lane hits")?;
        for hit in hits {
            if graph.nodes.len() >= QUERY_NODES {
                graph.partial = true;
                break;
            }
            if hit["status"] != "active" {
                return Err("query response included nonactive hit".into());
            }
            let mut found = node(hit)?;
            if !seen.insert(found.id.clone()) {
                return Err("query response contains duplicate nodes".into());
            }
            found.provenance = format!(
                "Search hit · {lane} lane · rank {} (lane-local)",
                hit["lane_rank"]
                    .as_u64()
                    .ok_or("query hit omitted lane rank")?
            );
            graph.partial |= hit["summary_truncated"] == true
                || hit["summary"]
                    .as_str()
                    .is_some_and(|v| v.len() > SUMMARY_BYTES);
            graph.nodes.push(found);
        }
    }
    if graph.partial {
        graph.note.push_str(" · bounded/partial results");
    }
    Ok(graph)
}

fn focus_graph(id: &str, value: &Value, neighbors: &Value) -> Result<Graph, Error> {
    let center = node(value)?;
    if center.id != id {
        return Err("owner returned a different node than requested".into());
    }
    let (items, more) = neighbor_items(neighbors)?;
    let body_partial = value["body_range"]["has_more"] == true
        || value["body"]
            .as_str()
            .is_some_and(|body| body.len() > BODY_BYTES);
    let mut graph = Graph {
        nodes: vec![center],
        focus: Some(id.to_owned()),
        partial: more
            || items.len() > NEIGHBORS
            || body_partial
            || value["summary_truncated"] == true
            || value["summary"]
                .as_str()
                .is_some_and(|summary| summary.len() > SUMMARY_BYTES),
        note: "One-hop neighborhood · separate best-effort reads, not an atomic snapshot".into(),
        ..Graph::default()
    };
    let mut seen = HashSet::from([id.to_owned()]);
    for item in items.iter().take(NEIGHBORS) {
        let edge = neighbor_edge(id, item)?;
        let neighbor = required_text(item, "neighbor")?;
        if seen.insert(neighbor.to_owned()) {
            graph.nodes.push(Node {
                id: neighbor.to_owned(),
                summary: text(required_text(item, "summary")?, SUMMARY_BYTES),
                provenance: "Neighbor summary · select and open to inspect this memory".into(),
                ..Node::default()
            });
        }
        graph.partial |= item["summary_truncated"] == true
            || item["summary"]
                .as_str()
                .is_some_and(|summary| summary.len() > SUMMARY_BYTES);
        graph.edges.push(edge);
    }
    if more || items.len() > NEIGHBORS {
        graph
            .note
            .push_str(" · first 32 edges only; no local-edge cursor yet");
    }
    if body_partial {
        graph.note.push_str(" · body excerpt (8 KiB)");
    }
    if graph.partial && !more && !body_partial && items.len() <= NEIGHBORS {
        graph.note.push_str(" · some summaries truncated");
    }
    Ok(graph)
}

fn neighbor_items(value: &Value) -> Result<(&[Value], bool), Error> {
    let items = value["items"]
        .as_array()
        .ok_or("neighbor response omitted items")?;
    let more = value["has_more"]
        .as_bool()
        .ok_or("neighbor response omitted has_more")?;
    Ok((items, more))
}

fn neighbor_edge(id: &str, item: &Value) -> Result<Edge, Error> {
    let neighbor = required_text(item, "neighbor")?;
    validate_id(neighbor)?;
    let kind = required_text(item, "kind")?;
    if !matches!(
        kind,
        "associative" | "transition" | "supersedes" | "derived_from"
    ) {
        return Err("neighbor response contains an unsupported edge kind".into());
    }
    let incoming = item["incoming"]
        .as_bool()
        .ok_or("neighbor omitted edge direction")?;
    let weight = item["weight"]
        .as_f64()
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .ok_or("neighbor omitted valid edge weight")?;
    let (from, to) = if incoming {
        (neighbor, id)
    } else {
        (id, neighbor)
    };
    Ok(Edge {
        from: from.to_owned(),
        to: to.to_owned(),
        kind: kind.to_owned(),
        weight,
    })
}

#[cfg(test)]
mod tests;
