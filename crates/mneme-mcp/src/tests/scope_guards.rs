//! Owner-selection and retained native identity preconditions.

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
use super::support::capability_request_cases;
use crate::*;

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
fn fixture(names: &[&str]) -> (PathBuf, Registry, std::sync::Arc<Mutex<SessionState>>) {
    let root = std::env::temp_dir().join(format!("mneme-mcp-scope-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let mut state = SessionState::default();
    let registry = build_registry(
        names
            .iter()
            .map(|name| ((*name).into(), root.join(format!("{name}.json"))))
            .collect(),
        &mut state,
    )
    .unwrap();
    (root, registry, std::sync::Arc::new(Mutex::new(state)))
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
#[tokio::test]
async fn omitted_read_scope_selects_project_or_sole_owner_and_never_falls_back() {
    for names in [&["user", "project"][..], &["user"][..], &["custom"][..]] {
        let (root, registry, sessions) = fixture(names);
        let selected = if names.contains(&"project") {
            "project"
        } else {
            names[0]
        };
        let gate = ColdWorkGate::new(Duration::ZERO);
        let id = call_tool(
            &registry,
            &sessions,
            &gate,
            "ingest",
            &json!({"db":selected,"summary":"selected owner"}),
        )
        .await
        .unwrap()["id"]
            .clone();
        let db_id = registry.checkout(selected).unwrap().db_id;
        let got = call_tool(
            &registry,
            &sessions,
            &gate,
            "get",
            &json!({"id":id,"expected_db_id":db_id.to_string()}),
        )
        .await
        .unwrap();
        assert_eq!(got["db"], selected);
        assert_eq!(got["summary"], "selected owner");
        assert_eq!(registry.select_read_scope(&json!({})).unwrap(), selected);
        call_tool(
            &registry,
            &sessions,
            &gate,
            "database_control",
            &json!({"db":selected,"action":"release","expected_db_id":db_id.to_string()}),
        )
        .await
        .unwrap();
        let error = call_tool(&registry, &sessions, &gate, "get", &json!({"id":id}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unavailable"), "{error}");
        call_tool(
            &registry,
            &sessions,
            &gate,
            "database_control",
            &json!({"db":selected,"action":"resume","expected_db_id":db_id.to_string()}),
        )
        .await
        .unwrap();
        assert!(
            call_tool(
                &registry,
                &sessions,
                &gate,
                "get",
                &json!({"id":id,"expected_db_id":db_id.to_string()})
            )
            .await
            .is_ok()
        );
        drop(registry);
        std::fs::remove_dir_all(root).unwrap();
    }
    let (root, registry, sessions) = fixture(&["user", "other"]);
    let gate = ColdWorkGate::new(Duration::ZERO);
    assert!(
        call_tool(&registry, &sessions, &gate, "status", &json!({}))
            .await
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
    assert!(
        call_tool(&registry, &sessions, &gate, "status", &json!({"db":"user"}))
            .await
            .is_ok()
    );
    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
#[tokio::test]
async fn every_owner_operation_refuses_wrong_identity_before_work_or_authority_change() {
    let (root, registry, sessions) = fixture(&["user", "project"]);
    let gate = ColdWorkGate::new(Duration::ZERO);
    for db in ["user", "project"] {
        call_tool(
            &registry,
            &sessions,
            &gate,
            "ingest",
            &json!({"db":db,"summary":"unchanged"}),
        )
        .await
        .unwrap();
    }
    let wrong = ulid::Ulid::new().to_string();
    let before =
        ["user", "project"].map(|db| std::fs::read(root.join(format!("{db}.json"))).unwrap());
    let epochs = {
        let mut state = sessions.lock().await;
        ["user", "project"].map(|db| state.feedback_epoch(db))
    };
    for (name, mut arguments, _) in capability_request_cases() {
        let kind = ValidatedToolArguments::parse(name, &arguments)
            .unwrap()
            .kind();
        if !kind.has_request_database_scope() {
            continue;
        }
        for db in ["user", "project"] {
            if kind == ToolRequestKind::Link(LinkRequest::Remote) && db != "user" {
                continue;
            }
            arguments["db"] = json!(db);
            arguments["expected_db_id"] = json!(wrong);
            let error = call_tool(&registry, &sessions, &gate, name, &arguments)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("expected_db_id mismatch"), "{name}: {error}");
            assert_eq!(registry.slot(db).unwrap().status().unwrap().state, "open");
        }
    }
    for (index, db) in ["user", "project"].iter().enumerate() {
        assert_eq!(
            std::fs::read(root.join(format!("{db}.json"))).unwrap(),
            before[index]
        );
        assert_eq!(sessions.lock().await.feedback_epoch(db), epochs[index]);
    }
    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
#[tokio::test]
async fn walk_guards_bind_session_owner_and_do_not_consume_actions_or_receipts() {
    let (root, registry, sessions) = fixture(&["user", "project"]);
    let gate = ColdWorkGate::new(Duration::ZERO);
    let id = call_tool(
        &registry,
        &sessions,
        &gate,
        "ingest",
        &json!({"db":"user","summary":"walk"}),
    )
    .await
    .unwrap()["id"]
        .clone();
    let user = registry.checkout("user").unwrap().db_id.to_string();
    let project = registry.checkout("project").unwrap().db_id.to_string();
    let start = call_tool(
        &registry,
        &sessions,
        &gate,
        "walk",
        &json!({"db":"user","action":"start","start":id,"expected_db_id":user}),
    )
    .await
    .unwrap();
    assert_eq!(start["db"], "user");
    let token = start["session"].as_str().unwrap();
    for action in ["look", "edges", "body", "back", "done", "abort"] {
        let error = call_tool(
            &registry,
            &sessions,
            &gate,
            "walk",
            &json!({"action":action,"session":token,"expected_db_id":project}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("expected_db_id mismatch"),
            "{action}: {error}"
        );
        let state = sessions.lock().await;
        assert_eq!(state.active.get(token).unwrap().actions, 1);
        assert_eq!(state.database_status("user").blocking_receipts, 0);
    }
    let done = call_tool(
        &registry,
        &sessions,
        &gate,
        "walk",
        &json!({"action":"done","session":token,"expected_db_id":user}),
    )
    .await
    .unwrap();
    assert_eq!(done["db_id"], user);
    let receipt = done["receipt"].clone();
    assert!(
        call_tool(
            &registry,
            &sessions,
            &gate,
            "reflect",
            &json!({"db":"user","receipts":[receipt],"used":[id],"expected_db_id":project})
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("expected_db_id mismatch")
    );
    assert_eq!(
        sessions
            .lock()
            .await
            .database_status("user")
            .blocking_receipts,
        1
    );
    let reflected = call_tool(
        &registry,
        &sessions,
        &gate,
        "reflect",
        &json!({"db":"user","receipts":[receipt],"used":[id],"expected_db_id":user}),
    )
    .await
    .unwrap();
    assert_eq!(reflected["db"], "user");
    assert_eq!(reflected["db_id"], user);
    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
#[tokio::test]
async fn guarded_resume_refuses_replaced_owner_and_keeps_maintenance_fence() {
    let (root, registry, sessions) = fixture(&["project"]);
    let gate = ColdWorkGate::new(Duration::ZERO);
    call_tool(
        &registry,
        &sessions,
        &gate,
        "ingest",
        &json!({"db":"project","summary":"old"}),
    )
    .await
    .unwrap();
    let old = registry.checkout("project").unwrap().db_id.to_string();
    call_tool(
        &registry,
        &sessions,
        &gate,
        "database_control",
        &json!({"db":"project","action":"release","expected_db_id":old}),
    )
    .await
    .unwrap();
    let (replacement_root, replacement, replacement_sessions) = fixture(&["project"]);
    call_tool(
        &replacement,
        &replacement_sessions,
        &gate,
        "ingest",
        &json!({"db":"project","summary":"replacement"}),
    )
    .await
    .unwrap();
    let new = replacement.checkout("project").unwrap().db_id.to_string();
    drop(replacement);
    std::fs::copy(
        replacement_root.join("project.json"),
        root.join("project.json"),
    )
    .unwrap();
    let error = call_tool(
        &registry,
        &sessions,
        &gate,
        "database_control",
        &json!({"db":"project","action":"resume","expected_db_id":old}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("expected_db_id mismatch"), "{error}");
    let status = registry.slot("project").unwrap().status().unwrap();
    assert_eq!(status.state, "maintenance");
    assert_eq!(status.db_id.to_string(), old);
    assert!(registry.checkout("project").is_err());
    let resumed = call_tool(
        &registry,
        &sessions,
        &gate,
        "database_control",
        &json!({"db":"project","action":"resume"}),
    )
    .await
    .unwrap();
    assert_eq!(resumed["db_id"], new);
    assert!(
        call_tool(
            &registry,
            &sessions,
            &gate,
            "status",
            &json!({"expected_db_id":old})
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("expected_db_id mismatch")
    );
    assert!(
        call_tool(
            &registry,
            &sessions,
            &gate,
            "status",
            &json!({"expected_db_id":new})
        )
        .await
        .is_ok()
    );
    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(replacement_root).unwrap();
}

#[test]
fn graph_and_neighbor_catalog_match_typed_admission_before_checkout() {
    use super::support::{schema_accepts, tool_input_schema};
    let schemas = tool_schemas(CapabilityPolicy::operator());
    let id = ulid::Ulid::new().to_string();
    let cases = [
        ("graph", json!({"action":"topology"}), true),
        ("graph", json!({"action":"summaries","ids":[id]}), true),
        ("graph", json!({"action":"topology","ids":[id]}), false),
        (
            "graph",
            json!({"action":"summaries","ids":[id],"after":"x"}),
            false,
        ),
        ("graph", json!({"action":"summaries","ids":null}), false),
        ("graph", json!({"action":"topology","limit":0}), false),
        ("neighbors", json!({"id":id,"limit":64}), true),
        ("neighbors", json!({"id":id,"limit":65}), false),
        ("neighbors", json!({"id":id,"limit":null}), false),
        ("neighbors", json!({"id":id,"after":null}), false),
    ];
    for (name, arguments, accepted) in cases {
        assert_eq!(
            schema_accepts(tool_input_schema(&schemas, name), &arguments),
            accepted,
            "{name}: {arguments}"
        );
        assert_eq!(
            ValidatedToolArguments::parse(name, &arguments).is_ok(),
            accepted,
            "{name}: {arguments}"
        );
    }
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
#[tokio::test]
async fn graph_reads_return_lightweight_topology_and_lazy_summaries_without_learning() {
    let (root, registry, sessions) = fixture(&["project"]);
    let gate = ColdWorkGate::new(Duration::ZERO);
    let id = call_tool(
        &registry,
        &sessions,
        &gate,
        "ingest",
        &json!({"db":"project","summary":"visible card","body":"not graph content"}),
    )
    .await
    .unwrap()["id"]
        .clone();
    let other = call_tool(
        &registry,
        &sessions,
        &gate,
        "ingest",
        &json!({"db":"project","summary":"second card"}),
    )
    .await
    .unwrap()["id"]
        .clone();
    let db_id = registry.checkout("project").unwrap().db_id.to_string();
    call_tool(
        &registry,
        &sessions,
        &gate,
        "link",
        &json!({"db":"project","from":id,"to":other,"expected_db_id":db_id}),
    )
    .await
    .unwrap();
    let before = std::fs::read(root.join("project.json")).unwrap();
    let topology = call_tool(
        &registry,
        &sessions,
        &gate,
        "graph",
        &json!({"action":"topology","expected_db_id":db_id}),
    )
    .await
    .unwrap();
    assert_eq!(topology["db"], "project");
    assert_eq!(topology["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(topology["edges"].as_array().unwrap().len(), 1);
    assert!(!topology.to_string().contains("visible card"));
    assert!(!topology.to_string().contains("not graph content"));
    let summaries = call_tool(
        &registry,
        &sessions,
        &gate,
        "graph",
        &json!({"action":"summaries","ids":[id],"expected_db_id":db_id}),
    )
    .await
    .unwrap();
    assert_eq!(summaries["items"][0]["summary"], "visible card");
    assert!(!summaries.to_string().contains("not graph content"));
    let neighbors = call_tool(
        &registry,
        &sessions,
        &gate,
        "neighbors",
        &json!({"id":id,"expected_db_id":db_id}),
    )
    .await
    .unwrap();
    assert_eq!(neighbors["db_id"], db_id);
    assert_eq!(neighbors["returned"], 1);
    assert_eq!(std::fs::read(root.join("project.json")).unwrap(), before);
    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
}
