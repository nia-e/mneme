//! One-call CLI to MCP adapter. Admission here happens before network I/O and,
//! crucially, before the CLI has a chance to resolve or create a local store.
//!
//! MCP has deliberately bounded/paged representations. Remote `--json` prints
//! the native MCP payload rather than pretending those are the unbounded local
//! CLI results (notably `core`, `get`, and `neighbors`).

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::{
    AnyErr, Command, KindArg, MergeAction, SignalArg, cli_body_limit, cli_remote_limit, parse_id,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestAccess {
    ReadOnly,
    Mutation,
}

pub(crate) struct Request {
    pub access: RequestAccess,
    pub tool: &'static str,
    pub arguments: Value,
    /// A server predating atomic capture links would silently discard this
    /// unknown field; the caller must check its advertised capture schema.
    pub requires_capture_links: bool,
    pub save: Option<mneme_app::save::PreparedSave>,
    pub concern: Option<crate::cli_concern::PreparedConcern>,
    pub edit_body: Option<crate::cli_edit_body::Prepared>,
    pub edit_summary: Option<crate::cli_edit_summary::Prepared>,
    pub retag: Option<crate::cli_retag::Prepared>,
}

fn unsupported(operation: &str, reason: &str) -> AnyErr {
    format!(
        "remote {operation} is unavailable: {reason}; select --db PATH without --remote for the offline CLI operation"
    )
    .into()
}

fn id(value: &str) -> Result<&str, AnyErr> {
    parse_id(value)?;
    Ok(value)
}

fn tags(tags: &[String]) -> Result<(), AnyErr> {
    if tags.len() > 32 {
        return Err("remote accepts at most 32 tags".into());
    }
    let mut unique = HashSet::new();
    for tag in tags {
        mneme_core::validate_tag(tag)?;
        if !unique.insert(tag) {
            return Err(format!("duplicate tag {tag:?}").into());
        }
    }
    Ok(())
}

fn query_input(
    text: &str,
    k: Option<usize>,
    depth: Option<u8>,
    max_nodes: Option<usize>,
    min_relevance: Option<f32>,
    tags_input: &[String],
) -> Result<Value, AnyErr> {
    if text.trim().is_empty() {
        return Err("query text must not be blank".into());
    }
    if text.len() > crate::MAX_CLI_QUERY_BYTES {
        return Err("query text exceeds 8192 UTF-8 bytes".into());
    }
    if k.is_some_and(|n| !(1..=crate::MAX_CLI_QUERY_K).contains(&n)) {
        return Err("--k must be between 1 and 64".into());
    }
    if depth.is_some_and(|n| n > crate::MAX_CLI_QUERY_DEPTH) {
        return Err("--depth must be at most 12".into());
    }
    if max_nodes.is_some_and(|n| !(1..=crate::MAX_CLI_QUERY_NODES).contains(&n)) {
        return Err("--max-nodes must be between 1 and 256".into());
    }
    if min_relevance.is_some_and(|n| !n.is_finite() || !(0.0..=1.0).contains(&n)) {
        return Err("--min-relevance must be finite and between 0 and 1".into());
    }
    tags(tags_input)?;
    Ok(json!({
        "text": text,
        "k": k,
        "depth": depth,
        "max_nodes": max_nodes,
        "min_relevance": min_relevance,
        "tags": tags_input,
    }))
}

fn with_db(mut value: Value, db: &str) -> Value {
    let object = value
        .as_object_mut()
        .expect("request arguments are an object");
    object.retain(|_, value| !value.is_null());
    object.insert("db".into(), json!(db));
    value
}

/// A single remote command is a single admitted MCP tool call. Never reinterpret
/// filesystem paths as server-side paths or registry names.
pub(crate) fn prepare(command: &Command, db: &str) -> Result<Request, AnyErr> {
    if db.is_empty() {
        return Err("remote database name must not be empty".into());
    }
    let mut requires_capture_links = false;
    let mut save = None;
    let mut concern = None;
    let mut retag = None;
    let mut edit_body = None;
    let mut edit_summary = None;
    let (tool, arguments) = match command {
        Command::Client(_) => {
            return Err("client is a standalone local process, not a remote CLI operation".into());
        }
        Command::Stores(_) => {
            return Err(unsupported(
                "stores",
                "known-source metadata is handled before remote/store admission",
            ));
        }
        Command::Tui(_) => {
            return Err("tui has its own remote session; not a one-call command".into());
        }
        Command::Snapshot {
            action: crate::SnapshotAction::Create,
        } => ("snapshot_create", json!({})),
        Command::Library(_) => {
            return Err("library reads require --config PATH and cannot use --remote".into());
        }
        Command::Status => ("status", json!({})),
        Command::Core => ("core", json!({})),
        Command::Query(a) => {
            if a.bodies {
                return Err(unsupported(
                    "query --bodies",
                    "MCP query returns no bodies; use remote get ID --body for each selected hit",
                ));
            }
            let mut request =
                query_input(&a.text, a.k, a.depth, a.max_nodes, a.min_relevance, &a.tags)?;
            request["archived"] = json!(a.archived);
            ("query", request)
        }
        Command::RecallContext(a) => {
            if a.max_content_bytes
                .is_some_and(|n| n != crate::DEFAULT_CLI_CONTEXT_BYTES)
            {
                return Err(unsupported(
                    "recall-context --max-content-bytes",
                    "the MCP context budget is fixed at 32768 bytes",
                ));
            }
            (
                "recall_context",
                query_input(&a.text, a.k, a.depth, a.max_nodes, a.min_relevance, &a.tags)?,
            )
        }
        Command::Get(a) => {
            id(&a.id)?;
            if a.body {
                cli_body_limit(a.max_body_bytes)?;
            }
            (
                "get",
                json!({"id": a.id, "body": a.body, "body_offset": a.body_offset, "max_body_bytes": a.max_body_bytes, "edges": a.edges}),
            )
        }
        Command::Body(a) => {
            id(&a.id)?;
            if a.raw {
                return Err(unsupported("body --raw", "MCP bodies are bounded ranges"));
            }
            cli_body_limit(a.max_bytes)?;
            (
                "get",
                json!({"id": a.id, "body": true, "body_offset": a.offset, "max_body_bytes": a.max_bytes}),
            )
        }
        Command::Neighbors(a) => ("neighbors", a.prepare()?.into_json()),
        Command::Remote(a) => {
            id(&a.id)?;
            cli_remote_limit(a.limit)?;
            let after = a
                .after
                .as_deref()
                .map(serde_json::from_str::<mneme_core::RemoteEdgeCursor>)
                .transpose()
                .map_err(|error| format!("invalid --after remote-edge cursor: {error}"))?;
            if let Some(cursor) = after.as_ref() {
                cursor
                    .validate_for(parse_id(&a.id)?)
                    .map_err(|error| format!("invalid --after remote-edge cursor: {error}"))?;
            }
            (
                "remote_edges",
                json!({"id": a.id, "limit": a.limit, "after": after}),
            )
        }
        Command::Ingest(args) => ("ingest", crate::cli_ingest::prepare_remote(args)?),
        Command::EditBody(args) => {
            let frozen = crate::cli_edit_body::Prepared::read(args)?;
            let payload = frozen.payload();
            edit_body = Some(frozen);
            ("edit_body", payload)
        }
        Command::EditSummary(args) => {
            let frozen = crate::cli_edit_summary::Prepared::read(args)?;
            let payload = frozen.payload();
            edit_summary = Some(frozen);
            ("edit_summary", payload)
        }
        Command::Retag(args) => {
            let frozen = crate::cli_retag::Prepared::read(args)?;
            let payload = frozen.payload();
            retag = Some(frozen);
            ("retag", payload)
        }
        Command::Concern(args) => {
            let frozen = crate::cli_concern::PreparedConcern::read(args)?;
            let payload = frozen.payload();
            concern = Some(frozen);
            ("concern", payload)
        }
        Command::Save(args) => {
            let frozen = crate::cli_save::PreparedSave::read(args)?;
            save = Some(frozen.prepared);
            ("save", frozen.payload)
        }
        Command::Capture { action } => match action {
            crate::cli_capture::CaptureAction::Add(args) => {
                let prepared = crate::cli_capture::PreparedCapture::read(args)?;
                requires_capture_links = prepared.has_links();
                ("capture", prepared.into_json())
            }
            crate::cli_capture::CaptureAction::Init
            | crate::cli_capture::CaptureAction::Inspect => {
                return Err(unsupported(
                    "capture init/inspect",
                    "storage setup/verification requires explicit local --db authority; remote owners already register existing databases",
                ));
            }
        },
        Command::Episode { action } => ("episode", action.prepare()?.into_json()),
        Command::Link(a) => {
            id(&a.from)?;
            id(&a.to)?;
            if a.to_db.is_some() {
                return Err(unsupported(
                    "link --to-db",
                    "CLI accepts a local path, but MCP requires a registered database name; no implicit path-to-name conversion is safe",
                ));
            }
            if !a.weight.is_finite() || !(0.0..=1.0).contains(&a.weight) {
                return Err("--weight must be finite and in 0..=1".into());
            }
            if let Some(target) = &a.to_remote_db {
                if db != "user" {
                    return Err(
                        "cross-database links may originate only in registry `user`; select --user"
                            .into(),
                    );
                }
                crate::remote_config::identifier(target, "remote target database")?;
                if target == db {
                    return Err(
                        "--to-remote-db must name another database; omit it for a local link"
                            .into(),
                    );
                }
                if a.anchor.is_some() || !matches!(a.kind, KindArg::Associative) {
                    return Err(
                        "cross-database links do not accept --anchor or a non-associative --kind"
                            .into(),
                    );
                }
                return Ok(Request {
                    access: RequestAccess::Mutation,
                    tool: "link",
                    arguments: with_db(
                        json!({"from":a.from,"to":a.to,"to_db":target,"weight":a.weight}),
                        db,
                    ),
                    requires_capture_links: false,
                    save: None,
                    concern: None,
                    retag: None,
                    edit_body: None,
                    edit_summary: None,
                });
            }
            let anchor = a.anchor.as_deref().map(crate::parse_span).transpose()?;
            if anchor.is_some_and(|span| span.start > span.end) {
                return Err("--anchor end must be at least start".into());
            }
            let kind = match a.kind {
                KindArg::Associative => "associative",
                KindArg::Transition => "transition",
                KindArg::Supersedes => "supersedes",
                KindArg::DerivedFrom => "derived_from",
            };
            (
                "link",
                json!({"from": a.from, "to": a.to, "kind": kind, "weight": a.weight,
                "anchor_start": anchor.map(|span| span.start), "anchor_end": anchor.map(|span| span.end)}),
            )
        }
        Command::Supersede { winner, loser } => (
            "supersede",
            json!({"winner": id(winner)?, "loser": id(loser)?}),
        ),
        Command::Contradict { a, b } => ("contradict", json!({"a": id(a)?, "b": id(b)?})),
        Command::Reconcile { a, b, resolution } => match (a, b, resolution) {
            (None, None, None) => ("contradictions", json!({})),
            (Some(a), Some(b), Some(resolution)) => {
                crate::parse_resolution(resolution)?;
                let canonical = if resolution == "context" {
                    "context-dependent"
                } else {
                    resolution
                };
                (
                    "reconcile",
                    json!({"a": id(a)?, "b": id(b)?, "resolution": canonical}),
                )
            }
            _ => {
                return Err(
                    "remote reconcile requires either no pair or --a, --b, and --as together"
                        .into(),
                );
            }
        },
        Command::Feedback(a) => {
            let signal = match a.signal {
                SignalArg::Relevant => "relevant",
                SignalArg::NotNew => "not-new",
                SignalArg::Irrelevant => "irrelevant",
            };
            (
                "feedback",
                json!({"from": a.from.as_deref().map(id).transpose()?, "to": id(&a.to)?, "signal": signal}),
            )
        }
        Command::Merges => ("merges", json!({})),
        Command::Merge { action } => match action {
            MergeAction::Full { winner, loser } => (
                "merge",
                json!({"mode": "full", "winner": id(winner)?, "loser": id(loser)?}),
            ),
            MergeAction::Keep { a, b } => {
                ("merge", json!({"mode": "keep", "a": id(a)?, "b": id(b)?}))
            }
        },
        Command::Forget { id: node } => ("forget", json!({"id": id(node)?})),
        Command::Decay => ("decay", json!({})),
        Command::Prune => ("prune", json!({})),
        Command::List(a) => ("list", a.prepare()?.into_json()),
        Command::Repl(_) => {
            return Err(unsupported(
                "repl",
                "MCP walk is a session protocol, not a one-call CLI command",
            ));
        }
        Command::Init(_)
        | Command::BootstrapInspect(_)
        | Command::BootstrapCreate(_)
        | Command::Migrate
        | Command::Reembed
        | Command::SingleGraphUpgrade(_)
        | Command::Demo { .. } => {
            return Err(unsupported(
                "offline command",
                "it requires local repository or store authority",
            ));
        }
    };
    // Closed, fail-closed policy for implicit owner routing. Keep read/write
    // meaning here beside the adapter instead of guessing from command names.
    let access = match tool {
        "status" | "core" | "query" | "recall_context" | "get" | "neighbors" | "remote_edges"
        | "contradictions" | "merges" | "list" => RequestAccess::ReadOnly,
        "episode" => match arguments["action"].as_str() {
            Some("list" | "search" | "get" | "history" | "references") => RequestAccess::ReadOnly,
            Some("append" | "revise") => RequestAccess::Mutation,
            _ => return Err("remote episode action has no owner-routing policy".into()),
        },
        "concern"
            if concern
                .as_ref()
                .is_some_and(|prepared| !prepared.request.is_mutation()) =>
        {
            RequestAccess::ReadOnly
        }
        "edit_body" | "edit_summary" | "retag" | "save" | "capture" | "ingest" | "concern"
        | "link" | "supersede" | "contradict" | "reconcile" | "feedback" | "merge" | "forget"
        | "decay" | "prune" | "snapshot_create" => RequestAccess::Mutation,
        _ => return Err("remote tool has no owner-routing policy".into()),
    };
    Ok(Request {
        access,
        tool,
        arguments: with_db(arguments, db),
        requires_capture_links,
        save,
        concern,
        retag,
        edit_body,
        edit_summary,
    })
}

fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn limited_neighbors(result: &Value, limit: usize) -> Result<Value, AnyErr> {
    let items = result
        .get("items")
        .and_then(Value::as_array)
        .ok_or("remote neighbors result lacks items")?;
    if items.len() > limit {
        return Err("remote neighbors exceeded the requested page size; refusing to drop rows behind its continuation cursor".into());
    }
    Ok(result.clone())
}

fn compact_context(result: &Value) -> Result<String, AnyErr> {
    if !result.is_object() {
        return Err("remote recall_context did not return a context object".into());
    }
    Ok(serde_json::to_string(result)?)
}

fn rank_evidence(hit: &Value) -> String {
    [
        ("dense", "dense_rank"),
        ("sparse", "sparse_rank"),
        ("graph", "graph_rank"),
        ("rerank", "rerank_rank"),
    ]
    .into_iter()
    .filter_map(|(label, field)| {
        hit["evidence"][field]
            .as_u64()
            .map(|rank| format!("{label}#{rank}"))
    })
    .collect::<Vec<_>>()
    .join(",")
}

pub(crate) fn render(command: &Command, result: &Value, json_output: bool) -> Result<(), AnyErr> {
    if matches!(command, Command::List(_)) {
        return crate::cli_list::render(result, json_output);
    }
    if matches!(command, Command::EditBody(_)) {
        crate::cli_edit_body::render(result, json_output);
        return Ok(());
    }
    if matches!(command, Command::EditSummary(_)) {
        crate::cli_edit_summary::render(result, json_output);
        return Ok(());
    }
    if matches!(command, Command::Retag(_)) {
        crate::cli_retag::render(result, json_output);
        return Ok(());
    }
    if matches!(command, Command::Concern(_)) {
        crate::cli_concern::render(result, json_output);
        return Ok(());
    }
    if matches!(command, Command::Save(_)) {
        crate::cli_save::render(result, json_output);
        return Ok(());
    }
    if matches!(command, Command::Episode { .. }) {
        crate::cli_episode::render(result, json_output);
        return Ok(());
    }
    if matches!(command, Command::RecallContext(_)) {
        // MCP emits compact JSON as a text block; the transport validates and
        // parses it before reaching this renderer. Re-encode without whitespace.
        println!("{}", compact_context(result)?);
        return Ok(());
    }
    if json_output {
        if let Command::Neighbors(a) = command {
            crate::print_json(&limited_neighbors(result, a.limit)?);
        } else if let Command::Body(_) = command {
            crate::print_json(&json!({
                "body": result["body"],
                "source_start": result["body_range"]["source_start"],
                "source_end": result["body_range"]["source_end"],
                "next_offset": result["body_range"]["next_offset"],
                "has_more": result["body_range"]["has_more"],
            }));
        } else {
            crate::print_json(result);
        }
        return Ok(());
    }
    match command {
        Command::Snapshot {
            action: crate::SnapshotAction::Create,
        } => {
            println!(
                "snapshot created for {} (db {}, generation {}): {}",
                s(result, "db"),
                s(result, "db_id"),
                s(result, "generation"),
                s(result, "bundle")
            );
        }
        Command::Library(_) => unreachable!("library never uses remote adapter"),
        Command::Status => {
            println!(
                "nodes {} (active {}, archived {})",
                number(result, "nodes"),
                number(result, "active"),
                number(result, "archived")
            );
            println!(
                "episodes {} ({} immutable editions)",
                number(result, "episodes"),
                number(result, "episode_editions")
            );
            println!(
                "due:\n  contradictions to reconcile : {}\n  merge candidates            : {}\n  edge decay pending          : {}",
                number(result, "open_contradictions"),
                number(result, "open_merge_candidates"),
                number(result, "edge_decay_pending")
            );
        }
        Command::Query(_) => {
            let mut count = 0;
            for lane in ["primary"] {
                if let Some(hits) = result
                    .get("lanes")
                    .and_then(|lanes| lanes.get(lane))
                    .and_then(|lane| lane.get("hits"))
                    .and_then(Value::as_array)
                {
                    for hit in hits {
                        count += 1;
                        println!(
                            "{:>2}. [{lane}#{}; {}] {} ({}) {}",
                            count,
                            number(hit, "lane_rank"),
                            rank_evidence(hit),
                            s(hit, "id"),
                            s(hit, "status"),
                            s(hit, "summary")
                        );
                    }
                }
            }
            if count == 0 {
                println!("(no results above relevance threshold)");
            }
            if result.get("partial").and_then(Value::as_bool) == Some(true) {
                eprintln!(
                    "[mnemed: remote query result is partial; inspect --json for retrieval or presentation omissions]"
                );
            }
        }
        Command::Get(a) => {
            let status = s(result, "status");
            println!(
                "id          {}\nsummary     {}\nstatus      {}",
                s(result, "id"),
                s(result, "summary"),
                status
            );
            if let Some(kind) = result.get("memory_kind") {
                if kind.get("kind").and_then(Value::as_str) == Some("episode") {
                    println!("memory-kind {kind}");
                }
            }
            println!(
                "stability   {}    confidence {}",
                result["stability"], result["confidence"]
            );
            if let Some(tags) = result.get("tags").and_then(Value::as_array) {
                println!(
                    "tags        {}",
                    tags.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            println!("body-owner  {}", s(result, "body_ownership"));
            if result["provenance"]["type"] == "external" {
                let source = &result["provenance"]["source"];
                println!(
                    "source      {}:{}",
                    s(source, "namespace"),
                    s(source, "key")
                );
                println!("source-ref  {}", s(source, "reference"));
                if let Some(session) = source["session"].as_str() {
                    println!("source-session  {session}");
                }
                if let Some(revision) = source["revision"].as_str() {
                    println!("source-revision {revision}");
                }
            }
            println!(
                "origin-commit {}",
                result["origin_commit"].as_str().unwrap_or("(none)")
            );
            println!(
                "exposures   {} (last {})",
                number(result, "exposure_count"),
                result["last_exposed"]
                    .as_u64()
                    .map_or_else(|| "never".to_owned(), |n| n.to_string())
            );
            println!(
                "grounded    {} (last {})",
                number(result, "grounded_use_count"),
                result["last_grounded_use"]
                    .as_u64()
                    .map_or_else(|| "never".to_owned(), |n| n.to_string())
            );
            if a.body {
                println!("\nbody:\n{}", s(result, "body"));
                if let Some(next) = result["body_range"]["next_offset"].as_u64() {
                    println!("  … body continues at source byte {next}");
                }
            }
            if a.edges {
                if let Some(items) = result.get("edges").and_then(Value::as_array) {
                    println!("\nedges (returned {}):", items.len());
                    for edge in items {
                        println!(
                            "  {} {}  w={}",
                            if edge["incoming"] == true { "<-" } else { "->" },
                            s(edge, "neighbor"),
                            edge["weight"]
                        );
                    }
                    if result["edges_has_more"] == true {
                        println!("  … more; use `neighbors {}`", a.id);
                    }
                }
                if let Some(items) = result["remote"]["items"].as_array()
                    && !items.is_empty()
                {
                    println!("\nremote (returned {}):", items.len());
                    for edge in items {
                        println!(
                            "  -> {}@{}  w={}",
                            s(edge, "target"),
                            s(edge, "target_db"),
                            edge["weight"]
                        );
                    }
                    if !result["remote"]["next"].is_null() {
                        println!(
                            "  … more; use `remote {} --after '{}'`",
                            a.id, result["remote"]["next"]
                        );
                    }
                }
            }
        }
        Command::Body(_) => {
            print!("{}", s(result, "body"));
            if result["body_range"]["has_more"] == true {
                eprintln!(
                    "\n[mnemed: body truncated; continue with --offset {}]",
                    result["body_range"]["next_offset"]
                );
            }
        }
        Command::Neighbors(a) => {
            let page = limited_neighbors(result, a.limit)?;
            let items = page["items"]
                .as_array()
                .ok_or("remote neighbors result lacks items")?;
            if items.is_empty() {
                println!("(no edges)");
            }
            for edge in items {
                println!(
                    "  {} {}  w={}  {}",
                    if edge["incoming"] == true { "<-" } else { "->" },
                    s(edge, "neighbor"),
                    edge["weight"],
                    s(edge, "summary")
                );
            }
            if let Some(cursor) = page["next_cursor"].as_str() {
                println!(
                    "  … more neighbors; repeat with --after {}",
                    serde_json::to_string(cursor)?
                );
            } else if page["has_more"] == true {
                println!("  … more neighbors (this owner did not provide a continuation)");
            }
        }
        Command::Remote(a) => {
            let items = result["items"]
                .as_array()
                .ok_or("remote edges result lacks items")?;
            if items.is_empty() {
                println!("(no remote edges)");
            }
            for edge in items {
                println!(
                    "{} -> {}@{}  w={}",
                    a.id,
                    s(edge, "target"),
                    s(edge, "target_db"),
                    edge["weight"]
                );
            }
            if !result["next"].is_null() {
                println!(
                    "more: mnemed --remote remote {} --limit {} --after '{}'",
                    a.id, a.limit, result["next"]
                );
            }
        }
        Command::Core => {
            let nodes = result["nodes"]
                .as_array()
                .ok_or("remote core result lacks nodes")?;
            if nodes.is_empty() {
                println!("(no core memory — tag a node `core` to bless it as always-loaded)");
            }
            for node in nodes {
                println!("# {}\n{}\n", s(node, "summary"), s(node, "body"));
            }
            if result["truncated"] == true {
                eprintln!("[mnemed: remote core was truncated; inspect --json for limits]");
            }
        }
        Command::Ingest(_)
        | Command::Capture { .. }
        | Command::Save(_)
        | Command::Concern(_)
        | Command::Retag(_)
        | Command::EditBody(_)
        | Command::EditSummary(_) => {
            println!(
                "{}{}",
                s(result, "id"),
                if result["replayed"] == true {
                    " (replayed)"
                } else {
                    ""
                }
            );
        }
        Command::Reconcile { a: None, .. } => {
            let items = result["contradictions"]
                .as_array()
                .ok_or("remote contradictions result lacks list")?;
            if items.is_empty() {
                println!("(no open contradictions)");
            }
            for item in items {
                println!(
                    "{} <-> {}  obs={}",
                    s(item, "a"),
                    s(item, "b"),
                    number(item, "observations")
                );
            }
            if !items.is_empty() {
                let clusters = result["cluster_conflicts"]
                    .as_array()
                    .ok_or("remote contradictions result lacks cluster conflicts")?;
                println!("community conflicts:");
                for conflict in clusters {
                    let pair = conflict["clusters"]
                        .as_array()
                        .ok_or("remote cluster conflict lacks cluster pair")?;
                    if pair.len() != 2 {
                        return Err("remote cluster conflict has invalid cluster pair".into());
                    }
                    println!(
                        "  {} <-> {}  obs={} pairs={}",
                        pair[0],
                        pair[1],
                        number(conflict, "observations"),
                        number(conflict, "pairs")
                    );
                }
            }
        }
        Command::Merges => {
            let items = result
                .as_array()
                .ok_or("remote merges result is not an array")?;
            if items.is_empty() {
                println!("(no open merge candidates)");
            }
            for item in items {
                println!(
                    "{} <~> {}  (x{})",
                    s(item, "a"),
                    s(item, "b"),
                    number(item, "observations")
                );
            }
        }
        Command::Forget { id } => println!(
            "{}",
            if result["forgotten"] == true {
                format!("forgot {id}")
            } else {
                format!("no such node {id}")
            }
        ),
        Command::Decay => println!(
            "decayed {} edge(s); conflicts={}; pages={}",
            number(result, "edges_decayed"),
            number(result, "edge_conflicts"),
            number(result, "edge_pages")
        ),
        Command::Prune => println!(
            "pruned {} edge(s), {} for capacity; weak conflicts={}; contended hubs={}; pages edge={} hub={}, chunks={}",
            number(result, "pruned"),
            number(result, "capacity_pruned"),
            number(result, "weak_conflicts"),
            number(result, "contended_hubs"),
            number(result, "edge_pages"),
            number(result, "hub_pages"),
            number(result, "chunks")
        ),
        Command::Feedback(a) => println!("feedback {}: {}", a.signal.label(), a.to),
        Command::Link(a) => println!("linked {} -> {}", a.from, a.to),
        Command::Supersede { winner, loser } => println!("superseded {loser} with {winner}"),
        Command::Contradict { a, b } => println!("recorded contradiction {a} <-> {b}"),
        Command::Reconcile {
            a: Some(a),
            b: Some(b),
            resolution: Some(resolution),
        } => println!("reconciled {a} <-> {b} as {resolution}"),
        Command::Merge {
            action: MergeAction::Full { winner, loser },
        } => println!("merged {loser} into {winner} (full)"),
        Command::Merge {
            action: MergeAction::Keep { a, b },
        } => println!("kept {a} and {b} separate"),
        _ => crate::print_json(result),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    const A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const B: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

    fn command(args: &[&str]) -> Command {
        let mut argv = vec!["mnemed"];
        argv.extend_from_slice(args);
        crate::Cli::try_parse_from(argv).unwrap().command
    }

    #[test]
    fn direct_mapping_preserves_database_and_intent() {
        let cases: &[(&[&str], &str)] = &[
            (&["status"], "status"),
            (&["query", "hello", "--tag", "rust"], "query"),
            (&["recall-context", "hello"], "recall_context"),
            (&["get", A, "--body", "--edges"], "get"),
            (&["body", A, "--offset", "12"], "get"),
            (&["neighbors", A], "neighbors"),
            (&["remote", A], "remote_edges"),
            (&["core"], "core"),
            (&["ingest", "--summary", "hello"], "ingest"),
            (&["link", "--from", A, "--to", B], "link"),
            (&["supersede", "--winner", A, "--loser", B], "supersede"),
            (&["contradict", "--a", A, "--b", B], "contradict"),
            (&["reconcile"], "contradictions"),
            (
                &["reconcile", "--a", A, "--b", B, "--as", "context"],
                "reconcile",
            ),
            (&["feedback", "relevant", "--to", A], "feedback"),
            (&["merges"], "merges"),
            (&["merge", "full", "--winner", A, "--loser", B], "merge"),
            (&["forget", A], "forget"),
            (&["decay"], "decay"),
            (&["prune"], "prune"),
        ];
        for (argv, expected) in cases {
            let request = prepare(&command(argv), "project").unwrap();
            assert_eq!(request.tool, *expected, "{argv:?}");
            assert_eq!(request.arguments["db"], "project", "{argv:?}");
            assert!(
                request
                    .arguments
                    .as_object()
                    .unwrap()
                    .values()
                    .all(|v| !v.is_null())
            );
        }
        let request = prepare(
            &command(&["reconcile", "--a", A, "--b", B, "--as", "context"]),
            "user",
        )
        .unwrap();
        assert_eq!(request.arguments["resolution"], "context-dependent");
    }

    #[test]
    fn unsupported_semantics_refuse_without_network_or_store() {
        for argv in [
            vec!["body", A, "--raw"],
            vec!["query", "x", "--bodies"],
            vec!["recall-context", "x", "--max-content-bytes", "4096"],
            vec!["link", "--from", A, "--to", B, "--to-db", "/tmp/other.db"],
            vec!["ingest", "--summary", "x", "--body-ref", "fs:///tmp/body"],
        ] {
            let error = prepare(&command(&argv), "project").err().unwrap();
            assert!(error.to_string().contains("remote"), "{argv:?}: {error}");
        }
        assert!(prepare(&command(&["query", "x", "--k", "0"]), "project").is_err());
        assert!(prepare(&command(&["neighbors", A, "--limit", "65"]), "project").is_err());
        assert!(prepare(&command(&["list", "--limit", "65"]), "project").is_err());
        assert!(prepare(&command(&["query", "   "]), "project").is_err());
        assert!(prepare(&command(&["recall-context", "   "]), "project").is_err());
        assert!(prepare(&command(&["ingest", "--summary", "   "]), "project").is_err());
        assert!(
            prepare(
                &command(&["link", "--from", A, "--to", B, "--anchor", "10:2"]),
                "project"
            )
            .is_err()
        );
        assert!(prepare(&command(&["remote", A, "--after", "{}"]), "project").is_err());
        assert!(prepare(&command(&["reconcile", "--a", A]), "project").is_err());
    }

    #[test]
    fn owner_routing_classifies_actual_read_and_write_requests() {
        for argv in [
            vec!["core"],
            vec!["status"],
            vec!["query", "needle"],
            vec!["get", A],
            vec!["body", A],
            vec!["neighbors", A],
            vec!["remote", A],
            vec!["reconcile"],
            vec!["merges"],
            vec!["episode", "list"],
            vec!["episode", "get", A],
            vec!["episode", "history", A],
            vec!["episode", "search", "needle"],
            vec!["episode", "references", A],
        ] {
            assert_eq!(
                prepare(&command(&argv), "project").unwrap().access,
                RequestAccess::ReadOnly,
                "{argv:?}"
            );
        }
        for argv in [
            vec!["save", "note"],
            vec!["save", "--kind", "episode", "scene"],
            vec!["ingest", "--summary", "note"],
            vec!["link", "--from", A, "--to", B],
            vec!["supersede", "--winner", A, "--loser", B],
            vec!["contradict", "--a", A, "--b", B],
            vec!["reconcile", "--a", A, "--b", B, "--as", "context"],
            vec!["feedback", "relevant", "--to", A],
            vec!["merge", "full", "--winner", A, "--loser", B],
            vec!["merge", "keep", "--a", A, "--b", B],
            vec!["forget", A],
            vec!["decay"],
            vec!["prune"],
        ] {
            assert_eq!(
                prepare(&command(&argv), "project").unwrap().access,
                RequestAccess::Mutation,
                "{argv:?}"
            );
        }
    }

    #[test]
    fn snapshot_create_is_one_owner_native_request() {
        let request = prepare(&command(&["snapshot", "create"]), "workshop").unwrap();
        assert_eq!(request.tool, "snapshot_create");
        assert_eq!(request.arguments, json!({"db": "workshop"}));
        assert!(
            prepare(
                &command(&["library", "--config", "x", "catalog"]),
                "project"
            )
            .is_err()
        );
    }

    #[test]
    fn neighbor_result_never_exceeds_cli_limit() {
        let raw = json!({"items": [1, 2, 3], "returned": 3, "has_more": false});
        assert!(limited_neighbors(&raw, 2).is_err());
        assert_eq!(limited_neighbors(&raw, 3).unwrap(), raw);
        assert_eq!(raw["returned"], 3);
    }

    #[test]
    fn context_from_decoded_mcp_text_is_compact_json() {
        let decoded: Value =
            serde_json::from_str(r#"{"schema":"mneme.context.v3","cards":[]}"#).unwrap();
        assert_eq!(
            compact_context(&decoded).unwrap(),
            r#"{"cards":[],"schema":"mneme.context.v3"}"#
        );
        assert!(compact_context(&json!("not a context object")).is_err());
    }

    #[test]
    fn diagnostic_query_rank_evidence_is_kept_in_human_view() {
        let hit = json!({"evidence": {"dense_rank": 2, "sparse_rank": null, "graph_rank": 4, "rerank_rank": 1}});
        assert_eq!(rank_evidence(&hit), "dense#2,graph#4,rerank#1");
    }
}
