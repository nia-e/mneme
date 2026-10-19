//! Opt-in contradiction notices do not change core's default read contract.
use crate::tests::{empty_profile_server, schema_accepts, tool_input_schema};
use crate::*;

#[tokio::test]
async fn core_notice_catalog_matches_boolean_admission_before_database_checkout() {
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        let server = empty_profile_server(profile);
        let initialize = server
            .dispatch("initialize", json!({"protocolVersion": PROTOCOL_VERSION}))
            .await
            .unwrap();
        assert_eq!(
            initialize["capabilities"]["experimental"]["mneme/core-contradiction-summary-v1"],
            true
        );
        let schemas = tool_schemas(server.capability);
        let schema = tool_input_schema(&schemas, "core");
        assert_eq!(
            schema["properties"]["include_contradictions"]["type"],
            "boolean"
        );
        for arguments in [
            json!({"db":"missing"}),
            json!({"db":"missing","include_contradictions":false}),
            json!({"db":"missing","include_contradictions":true}),
        ] {
            assert!(schema_accepts(schema, &arguments), "{arguments}");
            let admitted = ValidatedToolArguments::parse("core", &arguments).unwrap();
            assert_eq!(
                admitted.kind().capability_class(),
                CapabilityClass::ReadOnly
            );
            let response = server
                .dispatch("tools/call", json!({"name":"core","arguments":arguments}))
                .await
                .unwrap();
            let error = response["content"][0]["text"].as_str().unwrap();
            assert!(error.contains("unknown database"), "{error}");
            assert!(!error.contains("cold work"), "{error}");
        }
        let _cold = server.cold_work.admit("unrelated cold operation").unwrap();
        for malformed in [Value::Null, json!("true"), json!(1), json!([]), json!({})] {
            let arguments = json!({"db":"missing","include_contradictions":malformed});
            assert!(!schema_accepts(schema, &arguments), "{arguments}");
            assert!(ValidatedToolArguments::parse("core", &arguments).is_err());
            let response = server
                .dispatch("tools/call", json!({"name":"core","arguments":arguments}))
                .await
                .unwrap();
            let error = response["content"][0]["text"].as_str().unwrap();
            assert!(error.contains("include_contradictions"), "{error}");
            assert!(!error.contains("unknown database"), "{error}");
            assert!(!error.contains("capability profile"), "{error}");
        }
    }
}

#[cfg(not(feature = "fastembed"))]
async fn core(server: &Server, arguments: Value) -> Result<Value, AnyErr> {
    call_tool_authorized(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        server.capability,
        "core",
        &arguments,
    )
    .await
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn core_notice_is_opt_in_complete_and_identity_guarded() {
    let (root, server, nodes) = crate::concern::tests::fixture().await;
    let db_id = {
        let handle = server.registry.checkout("project").unwrap();
        handle
            .mem
            .observe_contradiction(ColdPath::acquire(), nodes[0].id(), nodes[1].id())
            .await
            .unwrap();
        handle
            .mem
            .ingest(Ingest::new(
                "Core anchor",
                b"kept body",
                &["core"],
                Provenance::derived_empty(),
            ))
            .await
            .unwrap();
        handle.db_id
    };
    let default = core(&server, json!({"db":"project"})).await.unwrap();
    let disabled = core(
        &server,
        json!({"db":"project","include_contradictions":false}),
    )
    .await
    .unwrap();
    assert_eq!(default, disabled);
    assert!(default.is_object());
    assert!(default.get("contradiction_summary").is_none());
    assert_eq!(default["nodes"].as_array().unwrap().len(), 1);

    let enriched = core(
        &server,
        json!({"db":"project","expected_db_id":db_id.to_string(),"include_contradictions":true}),
    )
    .await
    .unwrap();
    assert_eq!(enriched["nodes"], default["nodes"]);
    assert_eq!(
        enriched["contradiction_summary"],
        json!({"status":"complete","open_found":1,"rows_scanned":1})
    );
    let wrong = core(&server, json!({"db":"project","expected_db_id":ulid::Ulid::new().to_string(),"include_contradictions":true})).await.unwrap_err();
    assert!(wrong.to_string().contains("expected_db_id"), "{wrong}");

    let rows = server
        .registry
        .checkout("project")
        .unwrap()
        .mem
        .open_contradictions(ColdPath::acquire())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].observations, 1,
        "reading a notice must not reinforce it"
    );
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn core_notice_reports_a_saturated_prefix_as_partial_not_a_total() {
    let (root, server, nodes) = crate::concern::tests::fixture().await;
    {
        let handle = server.registry.checkout("project").unwrap();
        let mut ids: Vec<_> = nodes.iter().map(Node::id).collect();
        for index in 0..21 {
            ids.push(
                handle
                    .mem
                    .ingest(Ingest::new(
                        &format!("Claim {index}"),
                        b"",
                        &[],
                        Provenance::derived_empty(),
                    ))
                    .await
                    .unwrap(),
            );
        }
        let mut count = 0;
        'pairs: for (offset, &a) in ids.iter().enumerate() {
            for &b in &ids[offset + 1..] {
                handle
                    .mem
                    .observe_contradiction(ColdPath::acquire(), a, b)
                    .await
                    .unwrap();
                count += 1;
                if count == 257 {
                    break 'pairs;
                }
            }
        }
        assert_eq!(count, 257);
    }
    let enriched = core(
        &server,
        json!({"db":"project","include_contradictions":true}),
    )
    .await
    .unwrap();
    let summary = &enriched["contradiction_summary"];
    assert_eq!(summary["status"], "partial");
    let scanned = summary["rows_scanned"].as_u64().unwrap();
    assert!(scanned <= 256);
    assert_eq!(summary["open_found"], scanned);
    assert!(summary.get("total").is_none());
    assert_eq!(enriched["nodes"], json!([]));
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}
