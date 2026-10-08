//! Native lightweight topology and viewport summaries are separate bounded reads.
//! Neither operation is ranked search, and neither loads bodies.
use super::*;
use crate::model::{InventoryPage, InventoryState, InventorySummaries};
pub(crate) const PAGE_ITEMS: usize = 256;
pub(crate) const SUMMARY_ITEMS: usize = 64;
const PAGE_BYTES: usize = 128 * 1024;

fn graph_action(catalog: &[Value], action: &str) -> bool {
    catalog.iter().any(|tool| {
        tool["name"] == "graph" && {
            let schema = &tool["inputSchema"];
            schema["properties"]["action"]["enum"]
                .as_array()
                .is_some_and(|actions| actions.iter().any(|value| value == action))
                || schema["oneOf"].as_array().is_some_and(|branches| {
                    branches
                        .iter()
                        .any(|branch| branch["properties"]["action"]["const"] == action)
                })
        }
    })
}
fn require_graph(connection: &RemoteClient, action: &str) -> Result<(), Error> {
    if !graph_action(connection.tool_catalog(), action)
        || !connection.supports_expected_db_id("graph", Some(action))
    {
        return Err("selected owner does not advertise native lightweight graph reads; update its mneme-mcp server, or use / for explicitly bounded semantic search (library mode has no native graph inventory)".into());
    }
    Ok(())
}
pub(super) async fn page(
    connection: &mut RemoteClient,
    target: &Target,
    identity: &str,
    after: Option<&str>,
) -> Result<Response, Error> {
    require_graph(connection, "topology")?;
    let mut args = json!({"db":target.database,"expected_db_id":identity,"action":"topology","limit":PAGE_ITEMS});
    if let Some(after) = after {
        args["after"] = json!(after);
    }
    let value = connection.call_tool("graph", args).await?;
    Ok(Response::Inventory(parse_page(&value, identity, after)?))
}
pub(super) async fn summaries(
    connection: &mut RemoteClient,
    target: &Target,
    identity: &str,
    ids: &[String],
) -> Result<Response, Error> {
    require_graph(connection, "summaries")?;
    let value = connection
        .call_tool(
            "graph",
            json!({"db":target.database,"expected_db_id":identity,"action":"summaries","ids":ids}),
        )
        .await?;
    Ok(Response::Summaries(parse_summaries(&value, identity, ids)?))
}
fn bytes_and_identity(value: &Value, identity: &str, action: &str) -> Result<usize, Error> {
    let bytes = serde_json::to_vec(value)?.len();
    if bytes > PAGE_BYTES {
        return Err("graph page exceeds 128 KiB bound".into());
    }
    if value["db_id"].as_str() != Some(identity) || value["action"] != action {
        return Err(
            "graph response has a different action or database identity; keeping the field".into(),
        );
    }
    Ok(bytes)
}
fn parse_page(value: &Value, identity: &str, after: Option<&str>) -> Result<InventoryPage, Error> {
    let bytes = bytes_and_identity(value, identity, "topology")?;
    let nodes = value["nodes"].as_array().ok_or("topology omitted nodes")?;
    let edges = value["edges"].as_array().ok_or("topology omitted edges")?;
    if nodes.len() + edges.len() > PAGE_ITEMS {
        return Err("topology page exceeds 256 record bound".into());
    }
    let more = value["has_more"]
        .as_bool()
        .ok_or("topology omitted has_more")?;
    let next = match &value["next_cursor"] {
        Value::Null => None,
        Value::String(cursor) if !cursor.is_empty() && cursor.len() <= 2048 => Some(cursor.clone()),
        _ => return Err("topology returned an invalid continuation".into()),
    };
    if more != next.is_some() || next.as_deref().is_some_and(|next| Some(next) == after) {
        return Err("topology returned inconsistent or nonadvancing continuation".into());
    }
    if value["coverage"]["snapshot"] != false {
        return Err("topology omitted non-snapshot coverage".into());
    }
    let nodes_done = value["coverage"]["nodes_done"]
        .as_bool()
        .ok_or("topology omitted node coverage")?;
    let edges_done = value["coverage"]["edges_done"]
        .as_bool()
        .ok_or("topology omitted edge coverage")?;
    if more == (nodes_done && edges_done) || (!nodes_done && !edges.is_empty()) {
        return Err("topology returned inconsistent node/edge completion".into());
    }
    let mut graph=Graph {partial:more,inventory:Some(InventoryState {db_id:identity.into(),next,bytes,complete:!more,topology_edges_complete:value["coverage"]["edges_done"]==true,..Default::default()}),
        note:"Native lightweight topology · all statuses and historical editions · not an atomic snapshot · summaries load near viewport; bodies only on open".into(),..Default::default()};
    let mut previous: Option<String> = None;
    for item in nodes {
        let id = required_text(item, "id")?;
        validate_id(id)?;
        if previous.as_deref().is_some_and(|last| last >= id) {
            return Err("topology node ids are duplicate or out of order".into());
        }
        let status = required_text(item, "status")?;
        if !matches!(status, "active" | "archived") {
            return Err("topology returned unsupported node status".into());
        }
        let kind = required_text(item, "kind")?;
        if !matches!(kind, "note" | "episode") {
            return Err("topology returned unsupported memory kind".into());
        }
        let (tags, tags_complete) = tag_metadata(item);
        graph.nodes.push(Node {
            id: id.into(),
            status: status.into(),
            tags,
            tags_complete,
            provenance: format!("Native topology · {kind} · summary not loaded"),
            ..Default::default()
        });
        previous = Some(id.into());
    }
    for item in edges {
        let from = required_text(item, "from")?;
        let to = required_text(item, "to")?;
        validate_id(from)?;
        validate_id(to)?;
        let kind = required_text(item, "kind")?;
        if kind.is_empty() || kind.len() > 128 {
            return Err("topology returned invalid edge kind".into());
        }
        let weight = item["weight"]
            .as_f64()
            .filter(|n| n.is_finite())
            .ok_or("topology returned invalid edge weight")?;
        graph.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            kind: kind.into(),
            weight,
        });
    }
    Ok(InventoryPage { graph, bytes })
}
fn parse_summaries(
    value: &Value,
    identity: &str,
    ids: &[String],
) -> Result<InventorySummaries, Error> {
    let bytes = bytes_and_identity(value, identity, "summaries")?;
    let items = value["items"]
        .as_array()
        .ok_or("graph summaries omitted items")?;
    if items.len() != ids.len() || items.len() > SUMMARY_ITEMS {
        return Err("graph summary response does not match requested ids".into());
    }
    let mut result = InventorySummaries {
        bytes,
        ..Default::default()
    };
    for (item, id) in items.iter().zip(ids) {
        if item["id"].as_str() != Some(id) {
            return Err("graph summary response changed requested node identity or order".into());
        }
        let missing = item["missing"]
            .as_bool()
            .ok_or("graph summary response omitted missing marker")?;
        if missing {
            result.missing.push(id.clone());
            continue;
        }
        let mut found = node(item)?;
        found.provenance = "Native viewport summary · exact node edition · body not loaded".into();
        if item["summary_truncated"] == true
            || item["tags_truncated"] == true
            || item["tags"].as_array().is_some_and(|tags| tags.len() > 16)
        {
            found.provenance.push_str(" · bounded excerpt");
        }
        result.nodes.push(found);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    const DB: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    fn fixture(start: usize, count: usize, next: Option<&str>) -> Value {
        json!({"action":"topology","db_id":DB,"nodes":(start..start+count).map(|n|json!({"id":format!("{n:026}"),"status":if n%2==0{"active"}else{"archived"},"kind":"note"})).collect::<Vec<_>>(),"edges":[],"next_cursor":next,"has_more":next.is_some(),"coverage":{"snapshot":false,"nodes_done":next.is_none(),"edges_done":next.is_none()}})
    }
    #[test]
    fn topology_has_128_nodes_and_zero_eager_summaries_and_empty_continuation() {
        let first = parse_page(&fixture(0, 64, Some("next1")), DB, None).unwrap();
        let empty = parse_page(&fixture(64, 0, Some("next2")), DB, Some("next1")).unwrap();
        let last = parse_page(&fixture(64, 64, None), DB, Some("next2")).unwrap();
        assert_eq!(first.graph.nodes.len() + last.graph.nodes.len(), 128);
        assert_eq!(
            empty.graph.inventory.unwrap().next.as_deref(),
            Some("next2")
        );
        assert!(last.graph.inventory.unwrap().complete);
        assert!(
            first
                .graph
                .nodes
                .iter()
                .any(|node| node.status == "archived")
        );
        assert!(
            first
                .graph
                .nodes
                .iter()
                .all(|node| node.summary.is_empty() && node.body.is_empty())
        );
    }

    #[test]
    fn topology_tags_name_specific_pairs_without_hydrating_any_summaries() {
        use crate::clusters::{DisplayGroups, DisplayTopology};
        let mut value = fixture(0, 100, None);
        for (i, card) in value["nodes"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            card["tags"] = if i < 80 { json!(["rust"]) } else { json!([]) };
            card["tags_truncated"] = json!(false);
        }
        let first = value["nodes"][0]["id"].as_str().unwrap().to_owned();
        let second = value["nodes"][1]["id"].as_str().unwrap().to_owned();
        value["edges"] = json!([{"from":first,"to":second,"kind":"associative","weight":0.9}]);
        let label = |graph: &Graph| {
            DisplayTopology::new(graph)
                .groups(&DisplayGroups::default())
                .with_labels(graph)
                .group_for(&first)
                .unwrap()
                .label
                .clone()
        };
        let graph = parse_page(&value, DB, None).unwrap().graph;
        assert_eq!(label(&graph), None); // A corpus-wide Rust tag is not a pair name.
        for card in value["nodes"].as_array_mut().unwrap().iter_mut().take(2) {
            card["tags"] = json!(["rust", "directories"]);
        }
        let graph = parse_page(&value, DB, None).unwrap().graph;
        assert_eq!(label(&graph).as_deref(), Some("directories"));
        assert!(
            graph
                .nodes
                .iter()
                .all(|node| node.summary.is_empty() && node.body.is_empty())
        );
        let state = graph.inventory.as_ref().unwrap();
        assert!(state.hydrated.is_empty() && state.summary_attempted.is_empty());
        assert_eq!(state.summaries_loaded, 0);
        // Tags are evidence only when the complete array was delivered.
        for card in value["nodes"].as_array_mut().unwrap() {
            card["tags_truncated"] = json!(true);
        }
        let graph = parse_page(&value, DB, None).unwrap().graph;
        assert!(graph.nodes.iter().all(|node| !node.tags_complete));
        assert_eq!(label(&graph), None);
        // Older guarded owners can still provide topology, but not tag absence.
        for card in value["nodes"].as_array_mut().unwrap() {
            let card = card.as_object_mut().unwrap();
            card.remove("tags");
            card.remove("tags_truncated");
        }
        let graph = parse_page(&value, DB, None).unwrap().graph;
        assert!(
            graph
                .nodes
                .iter()
                .all(|node| node.tags.is_empty() && !node.tags_complete)
        );
        assert_eq!(label(&graph), None);
    }
    #[test]
    fn refuses_identity_repeated_cursor_oversized_duplicate_and_bad_coverage() {
        assert!(parse_page(&fixture(0, 1, None), "00000000000000000000000000", None).is_err());
        assert!(parse_page(&fixture(0, 1, Some("same")), DB, Some("same")).is_err());
        assert!(parse_page(&fixture(0, 257, None), DB, None).is_err());
        let mut bad = fixture(0, 2, None);
        bad["nodes"][1]["id"] = bad["nodes"][0]["id"].clone();
        assert!(parse_page(&bad, DB, None).is_err());
        let mut bad = fixture(0, 1, None);
        bad["coverage"]["snapshot"] = json!(true);
        assert!(parse_page(&bad, DB, None).is_err());
    }
    #[test]
    fn unsupported_old_catalog_does_not_become_semantic_search() {
        assert!(!graph_action(
            &[json!({"name":"list","inputSchema":{"properties":{"kind":{"const":"nodes"}}}})],
            "topology"
        ));
        assert!(graph_action(
            &[
                json!({"name":"graph","inputSchema":{"oneOf":[{"properties":{"action":{"const":"topology"}}}]}})
            ],
            "topology"
        ));
    }
    #[test]
    fn summaries_are_exact_cached_empty_and_missing_is_not_deletion() {
        let ids = vec![
            "00000000000000000000000000".into(),
            "00000000000000000000000001".into(),
        ];
        let value = json!({"db_id":DB,"action":"summaries","items":[{"id":ids[0],"missing":false,"summary":"","status":"active"},{"id":ids[1],"missing":true}]});
        let found = parse_summaries(&value, DB, &ids).unwrap();
        assert_eq!(found.nodes.len(), 1);
        assert_eq!(found.missing.len(), 1);
        assert!(found.nodes[0].body.is_empty());
        let mut swapped = value;
        swapped["items"][0]["id"] = json!(ids[1]);
        assert!(parse_summaries(&swapped, DB, &ids).is_err());
    }

    #[test]
    fn tag_truncation_is_metadata_not_provenance_inference() {
        let ids = vec!["00000000000000000000000000".into()];
        let mut value = json!({"db_id":DB,"action":"summaries","items":[{"id":ids[0],"missing":false,"summary":"bounded summary","tags":["rust"],"tags_truncated":false,"summary_truncated":true,"status":"active"}]});
        let found = parse_summaries(&value, DB, &ids).unwrap();
        assert!(found.nodes[0].tags_complete); // Summary truncation is independent.
        value["items"][0]["tags_truncated"] = json!(true);
        assert!(!parse_summaries(&value, DB, &ids).unwrap().nodes[0].tags_complete);
    }
}
