//! Owner/library boundaries, leases, snapshots, activity, and cold-work admission.

use super::support::empty_profile_server;
use crate::*;

#[test]
fn library_mode_cli_is_exclusive_of_owner_authority() {
    let args = parse_args_from(["--library-config", "/tmp/library.json"]).unwrap();
    assert_eq!(
        args.library_config,
        Some(PathBuf::from("/tmp/library.json"))
    );
    assert!(args.dbs.is_empty());
    for extra in [
        vec!["--db", "project=/tmp/project.db"],
        vec!["--capability-profile", "read-only"],
        vec!["--capability-profile", "operator"],
        vec!["--allow-direct-feedback"],
    ] {
        let mut argv = vec!["--library-config", "/tmp/library.json"];
        argv.extend(extra);
        assert!(parse_args_from(argv).is_err());
    }
    assert!(parse_args_from(["--library-config", "one", "--library-config", "two"]).is_err());
}

#[tokio::test]
async fn library_mode_catalog_and_raw_dispatch_are_closed_to_owner_tools() {
    let root = std::env::temp_dir().join(format!("mneme-mcp-library-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let config = root.join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "schema": "mneme.library.config.v1", "library_id": "lib", "device_id": "device",
            "catalog_path": "catalog.json"
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("catalog.json"),
        serde_json::to_vec(&json!({
            "schema": "mneme.library.catalog.v1", "library_id": "lib", "revision": 1, "entries": []
        }))
        .unwrap(),
    )
    .unwrap();
    let server = Server {
        registry: Registry {
            activity: crate::activity::ActivityRing::default(),
            dbs: BTreeMap::new(),
        },
        library: Some(Coordinator::Library(
            library_server::LibraryServer::from_path(&config).unwrap(),
        )),
        sessions: std::sync::Arc::new(Mutex::new(SessionState::default())),
        cold_work: ColdWorkGate::default(),
        capability: CapabilityPolicy::new(CapabilityProfile::ReadOnly, false),
    };
    let initialize = server
        .dispatch("initialize", json!({"protocolVersion": PROTOCOL_VERSION}))
        .await
        .unwrap();
    assert_eq!(initialize["capabilityProfile"], "library-read-only");
    assert_eq!(
        initialize["serverInfo"]["capabilityProfile"],
        "library-read-only"
    );
    let instructions = initialize["instructions"].as_str().unwrap();
    assert!(instructions.contains(mneme_app::episode::MEMORY_TIME_GUIDANCE));
    let instructions = instructions
        .split(mneme_app::episode::MEMORY_TIME_GUIDANCE)
        .next()
        .unwrap()
        .trim();
    let list = server.dispatch("tools/list", json!({})).await.unwrap();
    for (name, hint) in [
        ("library_catalog", "List explicitly enrolled projects"),
        ("library_query", "Read bounded summaries"),
        (
            "library_recall_context",
            "semantic and typed lexical/reference episode cards",
        ),
        ("library_get", "Get one node pinned to its source"),
    ] {
        let tool = list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap();
        let preview: String = format!(
            "{instructions}\n\n{}",
            tool["description"].as_str().unwrap()
        )
        .chars()
        .take(180)
        .collect();
        assert!(preview.contains(hint), "{name}: {preview}");
    }
    let names = list["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "library_catalog",
            "library_query",
            "library_recall_context",
            "library_get"
        ]
    );
    for hidden in [
        "databases",
        "query",
        "get",
        "snapshot_create",
        "database_control",
        "ingest",
    ] {
        let result = server
            .dispatch("tools/call", json!({"name": hidden, "arguments": {}}))
            .await
            .unwrap();
        assert_eq!(result["isError"], true, "{hidden}");
    }
    let null_arguments = server
        .dispatch(
            "tools/call",
            json!({"name":"library_catalog","arguments":null}),
        )
        .await
        .unwrap();
    assert_eq!(null_arguments["isError"], true);
    let catalog = server.handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"library_catalog","arguments":{}}})).await.unwrap();
    let frame: Value = serde_json::from_slice(catalog.bytes()).unwrap();
    assert_eq!(frame["result"]["isError"], false);
    assert!(
        frame["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("mneme.library.catalog.v1")
    );
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
#[tokio::test]
async fn snapshot_create_denial_and_busy_admission_preserve_epoch() {
    let denied = empty_profile_server(CapabilityProfile::ReadOnly)
        .dispatch(
            "tools/call",
            json!({"name":"snapshot_create","arguments":{"db":"missing"}}),
        )
        .await
        .unwrap();
    assert_eq!(denied["isError"], true);
    assert!(
        denied["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("does not authorize")
    );

    let root = std::env::temp_dir().join(format!("mneme-snapshot-admit-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let mut state = SessionState::default();
    let registry = build_registry(
        vec![("project".into(), root.join("project.json"))],
        &mut state,
    )
    .unwrap();
    let sessions = std::sync::Arc::new(Mutex::new(state));
    call_tool_authorized(
        &registry,
        &sessions,
        &ColdWorkGate::new(Duration::ZERO),
        CapabilityPolicy::operator(),
        "ingest",
        &json!({"db":"project","summary":"snapshot fixture"}),
    )
    .await
    .unwrap();
    let before = sessions.lock().await.feedback_epoch("project");
    let checkout = registry.checkout("project").unwrap();
    let busy = call_tool_authorized(
        &registry,
        &sessions,
        &ColdWorkGate::new(Duration::ZERO),
        CapabilityPolicy::operator(),
        "snapshot_create",
        &json!({"db":"project"}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(busy.contains("in-flight"), "{busy}");
    assert_eq!(sessions.lock().await.feedback_epoch("project"), before);
    drop(checkout);
    let created = call_tool_authorized(
        &registry,
        &sessions,
        &ColdWorkGate::new(Duration::ZERO),
        CapabilityPolicy::operator(),
        "snapshot_create",
        &json!({"db":"project"}),
    )
    .await
    .unwrap();
    assert_eq!(created["authority_rotated"], true);
    assert_ne!(sessions.lock().await.feedback_epoch("project"), before);
    let bundle = PathBuf::from(created["bundle"].as_str().unwrap());
    assert!(bundle.starts_with(root.canonicalize().unwrap().join("snapshots")));
    assert!(bundle.join("manifest.json").is_file());
    assert_eq!(
        registry.slot("project").unwrap().status().unwrap().state,
        "open"
    );
    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
#[tokio::test]
async fn oversized_anchor_rejection_cannot_mutate_the_durable_edge() {
    let root =
        std::env::temp_dir().join(format!("mneme-mcp-anchor-validation-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("memory.json");
    let mut sessions = SessionState::default();
    let registry = build_registry(vec![("fixture".into(), path.clone())], &mut sessions).unwrap();
    let server = Server {
        registry,
        sessions: std::sync::Arc::new(Mutex::new(sessions)),
        cold_work: ColdWorkGate::new(Duration::ZERO),
        capability: CapabilityPolicy::operator(),
        library: None,
    };
    let from = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "ingest",
        &json!({ "db": "fixture", "summary": "anchor source" }),
    )
    .await
    .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let to = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "ingest",
        &json!({ "db": "fixture", "summary": "anchor target" }),
    )
    .await
    .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "link",
        &json!({
            "db": "fixture", "from": from, "to": to,
            "anchor_start": 4, "anchor_end": 9,
        }),
    )
    .await
    .unwrap();
    let persisted_before = std::fs::read(&path).unwrap();

    let error = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "link",
        &json!({
            "db": "fixture", "from": from, "to": to, "weight": 1.0,
            "anchor_start": u64::from(u32::MAX) + 5,
            "anchor_end": u64::from(u32::MAX) + 104,
        }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("anchor_"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), persisted_before);

    let handle = server.registry.checkout("fixture").unwrap();
    let from = parse_id(&from).unwrap();
    let to = parse_id(&to).unwrap();
    let edge = handle
        .mem
        .neighbors(from, 8)
        .await
        .unwrap()
        .into_iter()
        .find(|neighbor| neighbor.node == to)
        .expect("original edge remains");
    assert_eq!(edge.edge.weight(), 0.5);
    assert_eq!(edge.edge.anchor, Some(BodySpan::new(4, 9)));

    drop(handle);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn lease_mutation_never_defaults_to_user_and_reports_the_selected_store() {
    let root = std::env::temp_dir().join(format!(
        "mneme-mcp-explicit-mutation-db-{}",
        ulid::Ulid::new()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let mut sessions = SessionState::default();
    let registry = build_registry(
        vec![
            ("user".into(), root.join("user.db")),
            ("project".into(), root.join("project.db")),
        ],
        &mut sessions,
    )
    .unwrap();
    let server = Server {
        registry,
        sessions: std::sync::Arc::new(Mutex::new(sessions)),
        cold_work: ColdWorkGate::new(Duration::ZERO),
        capability: CapabilityPolicy::operator(),
        library: None,
    };

    let status = server
        .dispatch(
            "tools/call",
            json!({
                "name": "database_control",
                "arguments": { "action": "status" },
            }),
        )
        .await
        .unwrap();
    assert_eq!(status["isError"], false, "{status}");
    let status_payload: Value =
        serde_json::from_str(status["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(status_payload["db"], "project");
    assert_eq!(status_payload["state"], "open");

    for arguments in [
        json!({ "action": "release" }),
        json!({ "bd": "project", "action": "release" }),
        json!({ "db": null, "action": "release" }),
        json!({ "db": 7, "action": "release" }),
    ] {
        let rejected = server
            .dispatch(
                "tools/call",
                json!({ "name": "database_control", "arguments": arguments }),
            )
            .await
            .unwrap();
        assert_eq!(rejected["isError"], true);
        assert_eq!(
            server
                .registry
                .slot("user")
                .unwrap()
                .status()
                .unwrap()
                .state,
            "open"
        );
        assert_eq!(
            server
                .registry
                .slot("project")
                .unwrap()
                .status()
                .unwrap()
                .state,
            "open"
        );
    }

    for db in ["user", "project"] {
        for (action, state) in [("release", "maintenance"), ("resume", "open")] {
            let response = server
                .dispatch(
                    "tools/call",
                    json!({
                        "name": "database_control",
                        "arguments": { "db": db, "action": action },
                    }),
                )
                .await
                .unwrap();
            assert_eq!(response["isError"], false, "{response}");
            let payload: Value =
                serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(payload["db"], db);
            assert_eq!(payload["name"], db);
            assert_eq!(payload["state"], state);
            assert_eq!(
                payload["db_id"],
                server
                    .registry
                    .slot(db)
                    .unwrap()
                    .status()
                    .unwrap()
                    .db_id
                    .to_string()
            );
        }

        let mutation = server
            .dispatch(
                "tools/call",
                json!({
                    "name": "forget",
                    "arguments": { "db": db, "id": ulid::Ulid::new().to_string() },
                }),
            )
            .await
            .unwrap();
        assert_eq!(mutation["isError"], false, "{mutation}");
        let payload: Value =
            serde_json::from_str(mutation["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["db"], db);
        assert_eq!(payload["forgotten"], false);
        assert_eq!(
            payload["db_id"],
            server
                .registry
                .slot(db)
                .unwrap()
                .status()
                .unwrap()
                .db_id
                .to_string()
        );
    }

    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn activity_profiles_poll_without_checkout_cold_admission_or_self_events() {
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        let server = empty_profile_server(profile);
        let _held = server.cold_work.admit("test held cold lane").unwrap();
        for _ in 0..2 {
            let frame = server
                .handle(json!({
                    "jsonrpc": "2.0", "id": "activity", "method": "tools/call",
                    "params": {"name": "activity", "arguments": {"after": 0, "limit": 1}},
                }))
                .await
                .unwrap();
            let mut stdio = Vec::new();
            frame.write_stdio(&mut stdio).unwrap();
            assert_eq!(stdio.last(), Some(&b'\n'));
            let response: Value = serde_json::from_slice(&stdio).unwrap();
            assert_eq!(response["result"]["isError"], false);
            let payload: Value =
                serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(payload["schema"], "mneme.activity.v1");
            assert_eq!(payload["events"], json!([]));
            assert_eq!(payload["latest_seq"], 0);
            assert_eq!(payload["busy"], false);
        }
        // Even failed observed operations cannot manufacture a node pulse.
        assert!(
            call_tool_authorized(
                &server.registry,
                &server.sessions,
                &server.cold_work,
                server.capability,
                "get",
                &json!({"id": ulid::Ulid::new().to_string()})
            )
            .await
            .is_err()
        );
        assert_eq!(
            serde_json::to_value(server.registry.activity.page(0, 1)).unwrap()["latest_seq"],
            0
        );
    }
}

#[tokio::test]
async fn activity_admission_rejects_malformed_and_unbounded_requests() {
    let server = empty_profile_server(CapabilityProfile::ReadOnly);
    for args in [
        json!({"limit":0}),
        json!({"limit":65}),
        json!({"limit":-1}),
        json!({"limit":1.5}),
        json!({"limit":null}),
        json!({"after":-1}),
        json!({"after":"1"}),
        json!({"after":1.5}),
        json!({"db":"project"}),
        json!({"text":"never retain me"}),
    ] {
        assert!(
            ValidatedToolArguments::parse("activity", &args).is_err(),
            "{args}"
        );
        let response = server
            .dispatch("tools/call", json!({"name":"activity","arguments":args}))
            .await
            .unwrap();
        assert_eq!(response["isError"], true);
    }
    assert!(
        ValidatedToolArguments::parse("activity", &json!({"after":u64::MAX,"limit":64})).is_ok()
    );
}

#[test]
fn cold_work_gate_rejects_overlap_and_fast_repeats() {
    let gate = ColdWorkGate::new(Duration::from_secs(60));
    let first = gate.admit("status").unwrap();

    let overlap = gate
        .admit("contradictions")
        .err()
        .expect("overlapping cold work must be rejected")
        .to_string();
    assert!(overlap.contains("busy running \"status\""), "{overlap}");

    drop(first);
    let repeated = gate
        .admit("status")
        .err()
        .expect("rapid repeated cold work must be rejected")
        .to_string();
    assert!(repeated.contains("rate limited"), "{repeated}");

    // Move the deterministic test clock boundary without sleeping. This is
    // also a direct assertion that dropping a permit releases `in_flight`.
    gate.inner.state.lock().unwrap().next_admission = Some(Instant::now());
    let after_interval = gate.admit("status").unwrap();
    drop(after_interval);
    assert!(gate.inner.state.lock().unwrap().in_flight.is_none());
}

#[test]
fn only_unbounded_public_tools_use_the_cold_admission_lane() {
    for name in [
        "status",
        "core",
        "forget",
        "contradictions",
        "merges",
        "decay",
        "prune",
    ] {
        assert!(metered_cold_tool(name), "{name}");
    }
    for name in [
        "activity",
        "databases",
        "database_control",
        "query",
        "recall_context",
        "recall",
        "get",
        "neighbors",
        "ingest",
        "capture",
        "episode",
        "feedback",
        "walk",
        // Reflect admits only its optional consolidation leg.
        "reflect",
    ] {
        assert!(!metered_cold_tool(name), "{name}");
    }
}

#[tokio::test]
async fn held_cold_permit_does_not_block_hot_dispatch() {
    let registry = Registry {
        activity: crate::activity::ActivityRing::default(),
        dbs: BTreeMap::new(),
    };
    let sessions = std::sync::Arc::new(Mutex::new(SessionState::default()));
    let gate = ColdWorkGate::new(Duration::ZERO);
    let _held = gate.admit("fixture cold work").unwrap();

    let hot_error = call_tool(
        &registry,
        &sessions,
        &gate,
        "query",
        &json!({ "text": "still admitted" }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        hot_error.contains("database scope is ambiguous"),
        "{hot_error}"
    );

    let cold_error = call_tool(
        &registry,
        &sessions,
        &gate,
        "status",
        &json!({"db":"missing"}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(cold_error.contains("cold work is busy"), "{cold_error}");
}
