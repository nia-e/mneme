use crate::tests::{empty_profile_server, schema_accepts, tool_input_schema};
use crate::*;

fn annotation() -> Value {
    json!({"subject":"A scene, not a generic checklist", "references":[{
        "db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV", "id":"01ARZ3NDEKTSV4RRFFQ69G5FAW",
        "expected_snapshot_sha256":"a".repeat(64)
    }]})
}

#[tokio::test]
async fn indexed_list_profiles_schema_and_raw_transport_agree_before_checkout() {
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        let server = empty_profile_server(profile);
        let schemas = tool_schemas(server.capability);
        let schema = tool_input_schema(&schemas, "list");
        let args = json!({"db":"missing", "kind":"touchstones", "limit":32});
        assert!(schema_accepts(schema, &args));
        for nodes in [
            json!({}),
            json!({"db":"missing"}),
            json!({"db":"missing","kind":"nodes","limit":64,"status":"all","tag":"rare"}),
            json!({"db":"missing","kind":"tags","limit":64,"status":"all","prefix":""}),
            json!({"db":"missing","kind":"tags","status":"archived","prefix":"People"}),
        ] {
            assert!(schema_accepts(schema, &nodes), "{nodes}");
            assert!(ValidatedToolArguments::parse("list", &nodes).is_ok());
        }
        let admitted = ValidatedToolArguments::parse("list", &args).unwrap();
        assert_eq!(
            admitted.kind().capability_class(),
            CapabilityClass::ReadOnly
        );
        assert!(!admitted.kind().requires_explicit_db());
        assert!(server.capability.authorize(admitted.kind()).is_ok());
        // An indexed page must not compete with whole-store maintenance.
        let _held = server.cold_work.admit("held lane").unwrap();
        let response = server
            .handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"list","arguments":args}}))
            .await
            .unwrap();
        let mut stdio = Vec::new();
        response.write_stdio(&mut stdio).unwrap();
        assert_eq!(&stdio[..stdio.len() - 1], response.bytes());
        let response: Value = serde_json::from_slice(response.bytes()).unwrap();
        let error = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(error.contains("unknown database"), "{error}");
        assert!(!error.contains("cold work") && !error.contains("capability profile"));
        for bad in [
            json!({"db":"missing","kind":"note"}),
            json!({"db":"missing","kind":"nodes","limit":65}),
            json!({"db":"missing","kind":"nodes","status":"candidate"}),
            json!({"db":"missing","kind":"nodes","tag":null}),
            json!({"db":"missing","kind":"tags","limit":65}),
            json!({"db":"missing","kind":"tags","prefix":null}),
            json!({"db":"missing","kind":"tags","tag":"people"}),
            json!({"db":"missing","kind":"tags","status":"candidate"}),
            json!({"db":"missing","kind":"touchstones","limit":0}),
            json!({"db":"missing","kind":"touchstones","limit":33}),
            json!({"db":"missing","kind":"touchstones","limit":null}),
            json!({"db":"missing","kind":"touchstones","after":null}),
            json!({"db":"missing","kind":"touchstones","tag":"touchstone"}),
            json!({"db":"missing","kind":"touchstones","status":"active"}),
            json!({"db":"missing","kind":"touchstones","expected_db_id":null}),
        ] {
            assert!(!schema_accepts(schema, &bad), "{bad}");
            assert!(
                ValidatedToolArguments::parse("list", &bad).is_err(),
                "{bad}"
            );
            let response = server
                .dispatch("tools/call", json!({"name":"list","arguments":bad}))
                .await
                .unwrap();
            let error = response["content"][0]["text"].as_str().unwrap();
            assert!(
                !error.contains("unknown database") && !error.contains("capability profile"),
                "{error}"
            );
        }
    }
}

#[test]
fn touchstone_capture_save_are_note_only_strict_and_preserve_existing_authority() {
    for profile in [CapabilityProfile::Curator, CapabilityProfile::Operator] {
        let schemas = tool_schemas(CapabilityPolicy::new(profile, false));
        for tool in ["capture", "save"] {
            let mut args = json!({"db":"project","summary":"Why this scene mattered",
                "source":{"namespace":"test","key":"annotation","reference":"test://annotation"},
                "touchstone":annotation()});
            assert!(schema_accepts(tool_input_schema(&schemas, tool), &args));
            let admitted = ValidatedToolArguments::parse(tool, &args).unwrap();
            assert_eq!(admitted.kind().capability_class(), CapabilityClass::Curator);
            args["touchstone"]["references"][0]["body"] = json!("not archived");
            assert!(!schema_accepts(tool_input_schema(&schemas, tool), &args));
            assert!(ValidatedToolArguments::parse(tool, &args).is_err());
        }
    }
    let mut episode =
        json!({"db":"project","summary":"scene","kind":"episode","touchstone":annotation()});
    assert!(ValidatedToolArguments::parse("save", &episode).is_err());
    episode["kind"] = json!("note");
    assert!(ValidatedToolArguments::parse("save", &episode).is_ok());
    episode["touchstone"] = Value::Null;
    assert!(ValidatedToolArguments::parse("save", &episode).is_err());
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn tag_vocabulary_is_an_observer_read_with_bounded_counts_and_canonical_examples() {
    let (root, mut server, nodes) = crate::concern::tests::fixture().await;
    server.capability = CapabilityPolicy::new(CapabilityProfile::ReadOnly, false);
    let db_id = server.registry.checkout("project").unwrap().db_id;
    let args = json!({"db":"project","expected_db_id":db_id.to_string(),"kind":"tags","prefix":"fa","status":"all","limit":1});
    let result = call_tool_authorized(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        server.capability,
        "list",
        &args,
    )
    .await
    .unwrap();
    assert_eq!(result["kind"], "tags");
    assert_eq!(result["db_id"], db_id.to_string());
    assert_eq!(result["items"].as_array().unwrap().len(), 1);
    assert_eq!(result["items"][0]["name"], "fact");
    assert_eq!(
        result["items"][0]["count"],
        json!({"status":"exact","value":3})
    );
    let mut examples = result["items"][0]["examples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    examples.sort();
    assert_eq!(
        examples,
        nodes
            .iter()
            .map(|node| node.id().0.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(result["coverage"]["semantic_only"], true);
    assert_eq!(result["coverage"]["snapshot"], false);
    let node = server
        .registry
        .checkout("project")
        .unwrap()
        .mem
        .get_node(nodes[0].id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(node.exposure_count(), nodes[0].exposure_count());
    assert_eq!(node.grounded_use_count(), nodes[0].grounded_use_count());
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn mcp_get_capture_save_replay_page_and_deleted_target_preserve_historical_snapshot() {
    let root = std::env::temp_dir().join(format!("mneme-mcp-touchstone-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("memory.json");
    mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM)
        .save(&path)
        .unwrap();
    let mut state = SessionState::default();
    let registry = build_registry(vec![("project".into(), path)], &mut state).unwrap();
    let server = Server {
        registry,
        sessions: std::sync::Arc::new(Mutex::new(state)),
        cold_work: ColdWorkGate::new(Duration::ZERO),
        capability: CapabilityPolicy::operator(),
        library: None,
    };
    async fn call(server: &Server, tool: &str, args: Value) -> Result<Value, AnyErr> {
        call_tool_authorized(
            &server.registry,
            &server.sessions,
            &server.cold_work,
            server.capability,
            tool,
            &args,
        )
        .await
    }
    let target = call(&server,"save",json!({"db":"project","summary":"The actual scene","body":"Not copied into snapshot","operation_id":"scene"})).await.unwrap();
    let target_id = target["id"].as_str().unwrap();
    let fetched = call(
        &server,
        "get",
        json!({"db":"project","id":target_id,"body":true,"edges":true}),
    )
    .await
    .unwrap();
    assert_eq!(fetched["summary_snapshot"]["coverage"], "summary_only");
    assert!(fetched["body"].is_string());
    assert!(fetched["edges"].is_array());
    let mut reference = fetched["summary_snapshot"].clone();
    reference.as_object_mut().unwrap().remove("coverage");
    let mut owners = Vec::new();
    let mut requests = Vec::new();
    for (index, tool) in ["save", "capture"].into_iter().enumerate() {
        let raw = json!({"db":"project","summary":"The annotation","body":"Private author body",
            "source":{"namespace":"test","key":format!("touchstone-{index}"),"reference":"test://annotation"},
            "touchstone":{"subject":"What the scene changed","references":[reference.clone()]}});
        let first = call(&server, tool, raw.clone()).await.unwrap();
        owners.push(first["id"].clone());
        requests.push((tool, raw));
        let full = call(&server, "get", json!({"db":"project","id":first["id"]}))
            .await
            .unwrap();
        assert_eq!(full["touchstone"]["subject"], "What the scene changed");
        assert!(
            !serde_json::to_string(&full["touchstone"])
                .unwrap()
                .contains("Not copied into snapshot")
        );
    }
    let first_page = call(
        &server,
        "list",
        json!({"db":"project","kind":"touchstones","limit":1}),
    )
    .await
    .unwrap();
    // Ordinary inventory is available through the same admitted owner and
    // remains summary-only, while the touchstone projection stays unchanged.
    let ordinary = call(&server, "list", json!({"db":"project","limit":1}))
        .await
        .unwrap();
    assert_eq!(ordinary["kind"], "nodes");
    assert_eq!(ordinary["items"].as_array().unwrap().len(), 1);
    assert!(ordinary["has_more"].as_bool().unwrap());
    let second_ordinary = call(
        &server,
        "list",
        json!({"db":"project","limit":64,"after":ordinary["next_cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(second_ordinary["items"].as_array().unwrap().len(), 2);
    assert!(second_ordinary["next_cursor"].is_null());
    assert!(
        !serde_json::to_string(&ordinary)
            .unwrap()
            .contains("Private author body")
    );
    assert!(
        call(
            &server,
            "list",
            json!({"db":"project","expected_db_id":ulid::Ulid::new().to_string()})
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("expected_db_id mismatch")
    );
    assert_eq!(first_page["items"].as_array().unwrap().len(), 1);
    let next = first_page["next_cursor"].as_str().unwrap();
    let second = call(
        &server,
        "list",
        json!({"db":"project","kind":"touchstones","limit":1,"after":next}),
    )
    .await
    .unwrap();
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    // A full final page conservatively offers continuation; a terminal empty
    // page establishes exhaustion without reading a sentinel beyond the cap.
    let terminal = call(
        &server,
        "list",
        json!({"db":"project","kind":"touchstones","limit":1,"after":second["next_cursor"]}),
    )
    .await
    .unwrap();
    assert!(terminal["items"].as_array().unwrap().is_empty());
    assert!(terminal["next_cursor"].is_null());
    assert_ne!(first_page["items"][0]["id"], second["items"][0]["id"]);
    assert!(
        !serde_json::to_string(&first_page)
            .unwrap()
            .contains("Private author body")
    );
    assert!(call(&server,"list",json!({"db":"project","kind":"touchstones","expected_db_id":ulid::Ulid::new().to_string()})).await.unwrap_err().to_string().contains("expected_db_id mismatch"));
    let before = call(&server, "get", json!({"db":"project","id":owners[0]}))
        .await
        .unwrap();
    call(&server, "forget", json!({"db":"project","id":target_id}))
        .await
        .unwrap();
    for (tool, raw) in requests {
        let replay = call(&server, tool, raw).await.unwrap();
        assert_eq!(replay["replayed"], true);
    }
    let after = call(&server, "get", json!({"db":"project","id":owners[0]}))
        .await
        .unwrap();
    assert_eq!(before["touchstone"], after["touchstone"]);
    assert_ne!(before["touchstone_current"], after["touchstone_current"]);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}
