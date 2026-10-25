use super::*;

const PUBLISHER_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const STORE_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

fn registration_catalog() -> Value {
    let mut catalog = discovery_catalog();
    catalog[0]["name"] = json!("discovery_register");
    let schema = &mut catalog[0]["inputSchema"];
    schema["required"] = json!(["db", "expected_db_id", "label", "store"]);
    schema["properties"]["label"] = json!({
        "type":"string", "minLength":1, "maxLength":128, "pattern":"^[A-Za-z0-9_.-]+$"
    });
    // The client recognizes the closed identity-bearing store envelope. The
    // shared server parser owns full nested StoreDeclaration validation.
    schema["properties"]["store"] = json!({
        "type":"object", "additionalProperties":false, "required":["id"],
        "properties":{
            "id":schema["properties"]["expected_db_id"],
            "owner":{"type":"object"}, "discovery":{"type":"boolean"},
            "active":{"type":"object"}, "permissions":{"type":"object"},
            "delivery":{"type":"object"}, "background":{"type":"object"},
            "guidance":{"type":"object"}, "cache":{"type":"object"},
            "retrieval":{"type":"object"}, "applicability":{"type":"object"},
            "capture":{"type":"object"}, "clear":{"type":"array"},
            "devices":{"type":"object"}
        }
    });
    catalog
}

fn registration_arguments() -> Value {
    json!({"db":"hub", "expected_db_id":PUBLISHER_ID, "label":"project", "store":{"id":STORE_ID}})
}

#[test]
fn discovery_register_contract_is_optional_closed_and_structural() {
    let catalog = registration_catalog();
    let client = catalog_client(&catalog);
    let view = CatalogView(catalog.as_array().unwrap());
    assert!(view.supports_discovery_register());
    assert!(client.supports_discovery_register());
    assert!(client.supports_expected_db_id("discovery_register", None));
    assert!(!client.supports_expected_db_id("discovery_register", Some("register")));
    assert!(!known_native_read(
        "discovery_register",
        &registration_arguments()
    ));
    assert!(!catalog_client(&json!([])).supports_discovery_register());
    let name_only = catalog_client(&json!([{"name":"discovery_register"}]));
    assert!(name_only.advertises("discovery_register"));
    assert!(!name_only.supports_discovery_register());

    for (pointer, replacement) in [
        ("/0/inputSchema/type", json!("array")),
        ("/0/inputSchema/additionalProperties", json!(true)),
        (
            "/0/inputSchema/required",
            json!(["expected_db_id", "label", "store"]),
        ),
        ("/0/inputSchema/required", json!(["db", "label", "store"])),
        (
            "/0/inputSchema/required",
            json!(["db", "expected_db_id", "store"]),
        ),
        (
            "/0/inputSchema/required",
            json!(["db", "expected_db_id", "label"]),
        ),
        (
            "/0/inputSchema/required",
            json!([
                "db",
                "expected_db_id",
                "label",
                "store",
                "expected_catalog_sha256"
            ]),
        ),
        ("/0/inputSchema/properties/db/type", json!("integer")),
        ("/0/inputSchema/properties/db/minLength", json!(0)),
        ("/0/inputSchema/properties/db/maxLength", json!(257)),
        (
            "/0/inputSchema/properties/expected_db_id/type",
            json!("integer"),
        ),
        (
            "/0/inputSchema/properties/expected_db_id/minLength",
            json!(25),
        ),
        (
            "/0/inputSchema/properties/expected_db_id/maxLength",
            json!(27),
        ),
        (
            "/0/inputSchema/properties/expected_db_id/pattern",
            json!(".*"),
        ),
        ("/0/inputSchema/properties/label/type", json!("integer")),
        ("/0/inputSchema/properties/label/minLength", json!(0)),
        ("/0/inputSchema/properties/label/maxLength", json!(129)),
        ("/0/inputSchema/properties/label/pattern", json!(".*")),
        ("/0/inputSchema/properties/store/type", json!("array")),
        (
            "/0/inputSchema/properties/store/additionalProperties",
            json!(true),
        ),
        ("/0/inputSchema/properties/store/required", json!([])),
        (
            "/0/inputSchema/properties/store/required",
            json!(["id", "owner"]),
        ),
        (
            "/0/inputSchema/properties/store/properties/id/type",
            json!("integer"),
        ),
        (
            "/0/inputSchema/properties/store/properties/id/minLength",
            json!(25),
        ),
        (
            "/0/inputSchema/properties/store/properties/id/maxLength",
            json!(27),
        ),
        (
            "/0/inputSchema/properties/store/properties/id/pattern",
            json!(".*"),
        ),
        ("/0/inputSchema/properties/store/properties/id", Value::Null),
        (
            "/0/inputSchema/properties/expected_catalog_sha256/type",
            json!("integer"),
        ),
        (
            "/0/inputSchema/properties/expected_catalog_sha256/minLength",
            json!(63),
        ),
        (
            "/0/inputSchema/properties/expected_catalog_sha256/maxLength",
            json!(65),
        ),
        (
            "/0/inputSchema/properties/expected_catalog_sha256/pattern",
            json!(".*"),
        ),
        (
            "/0/inputSchema/properties/expected_catalog_sha256",
            Value::Null,
        ),
    ] {
        let mut malformed = catalog.clone();
        *malformed.pointer_mut(pointer).unwrap() = replacement;
        let client = catalog_client(&malformed);
        assert!(
            !CatalogView(malformed.as_array().unwrap()).supports_discovery_register(),
            "{pointer}"
        );
        assert!(!client.supports_discovery_register(), "{pointer}");
        assert!(
            !client.supports_expected_db_id("discovery_register", None),
            "{pointer}"
        );
        assert!(
            client
                .validate_tool_call(
                    &json!({"name":"discovery_register", "arguments":registration_arguments()})
                )
                .is_err(),
            "{pointer}"
        );
    }
}

#[tokio::test]
async fn discovery_register_rejects_unadvertised_or_malformed_requests_before_send() {
    let args = registration_arguments();
    let mut bad_arguments = vec![("non-object arguments".to_owned(), json!([]))];
    for field in ["db", "expected_db_id", "label", "store"] {
        let mut missing = args.clone();
        missing.as_object_mut().unwrap().remove(field);
        bad_arguments.push((format!("missing {field}"), missing));
        for value in [Value::Null, json!(false), json!(42), json!([])] {
            let mut malformed = args.clone();
            malformed[field] = value;
            bad_arguments.push((format!("wrong {field} type"), malformed));
        }
    }
    for (pointer, replacement) in [
        ("/db", json!("")),
        ("/db", json!("a".repeat(257))),
        ("/label", json!("")),
        ("/label", json!("a".repeat(129))),
        ("/label", json!("project route")),
        ("/label", json!("project/route")),
        ("/label", json!("projéct")),
        ("/store", json!("not an object")),
        ("/store", json!({})),
        ("/store/id", Value::Null),
        ("/store/id", json!(42)),
    ] {
        let mut malformed = args.clone();
        *malformed.pointer_mut(pointer).unwrap() = replacement;
        bad_arguments.push((format!("invalid {pointer}"), malformed));
    }
    for pointer in ["/expected_db_id", "/store/id"] {
        for invalid in [
            String::new(),
            STORE_ID.to_ascii_lowercase(),
            "8ZZZZZZZZZZZZZZZZZZZZZZZZZ".to_owned(),
            "0".repeat(25),
            "0".repeat(27),
            format!("0{}I", "0".repeat(24)),
            format!("0{}U", "0".repeat(24)),
        ] {
            let mut malformed = args.clone();
            *malformed.pointer_mut(pointer).unwrap() = json!(invalid);
            bad_arguments.push((format!("noncanonical {pointer}"), malformed));
        }
    }
    for digest in [
        Value::Null,
        json!(42),
        json!(""),
        json!("a".repeat(63)),
        json!("a".repeat(65)),
        json!("A".repeat(64)),
        json!("g".repeat(64)),
    ] {
        let mut malformed = args.clone();
        malformed["expected_catalog_sha256"] = digest;
        bad_arguments.push(("invalid digest".to_owned(), malformed));
    }
    for field in ["path", "action", "unexpected"] {
        let mut malformed = args.clone();
        malformed[field] = json!("not accepted");
        bad_arguments.push((format!("unknown {field}"), malformed));
    }

    let mut malformed_catalog = registration_catalog();
    malformed_catalog[0]["inputSchema"]["properties"]["store"]["additionalProperties"] =
        json!(true);
    for (catalog, rejected) in [
        (json!([]), vec![("absent tool".to_owned(), args.clone())]),
        (
            json!([{"name":"discovery_register"}]),
            vec![("name-only tool".to_owned(), args.clone())],
        ),
        (
            malformed_catalog,
            vec![("malformed contract".to_owned(), args.clone())],
        ),
        (registration_catalog(), bad_arguments),
    ] {
        let (options, server) = fixture_server_with_catalog("mneme-mcp", catalog, Some(vec![]));
        let mut client = RemoteClient::connect_local(
            &options,
            ClientTimeouts {
                connect: Duration::from_secs(2),
                request: Duration::from_secs(2),
            },
            "mneme-mcp",
        )
        .await
        .unwrap();
        let before = client.next_id;
        for (case, arguments) in rejected {
            for raw in [false, true] {
                let error = if raw {
                    client
                        .raw_rpc(
                            "tools/call",
                            json!({"name":"discovery_register", "arguments":arguments}),
                        )
                        .await
                } else {
                    client
                        .call_tool("discovery_register", arguments.clone())
                        .await
                }
                .unwrap_err();
                assert_eq!(
                    classify_error(error.as_ref()),
                    ClientErrorClass::Protocol,
                    "{case}; raw={raw}"
                );
                assert_eq!(
                    client.next_id, before,
                    "{case}; raw={raw}: rejected call reached request()"
                );
                assert!(
                    client.session.is_some(),
                    "{case}: local refusal must preserve the session"
                );
            }
        }
        client.close().await;
        server.join().unwrap();
    }

    let mut lazy_client = catalog_client(&json!([]));
    lazy_client.lazy_native = true;
    lazy_client.initialize_result = json!({"capabilityProfile":"read-only"});
    let before = lazy_client.next_id;
    assert!(
        lazy_client
            .call_native_read("discovery_register", args)
            .await
            .is_err()
    );
    assert_eq!(lazy_client.next_id, before);
}

#[tokio::test]
async fn discovery_register_forwards_exact_declaration_and_optional_pin_through_raw_and_parsed_calls()
 {
    let base = registration_arguments();
    let mut routed = base.clone();
    routed["label"] = json!("Project_1.test-route");
    routed["store"] = json!({
        "id":STORE_ID,
        "owner":{"url":"http://127.0.0.1:9001/mcp", "db":"project"},
        "discovery":false,
        "permissions":{"read":true, "write":false},
        "retrieval":{"tags":[]},
        "devices":{
            "laptop":{"owner":{"url":"http://127.0.0.1:9002/mcp", "db":"local-project"}, "permissions":{"read":true, "write":false}},
            "workstation":{"owner":{"url":"ssh://fixture.invalid", "db":"remote-project", "ssh_mcp_port":9003}, "discovery":false}
        },
        "clear":["delivery"]
    });
    let mut pinned = routed.clone();
    pinned["expected_catalog_sha256"] = json!("0123456789abcdef".repeat(4));
    let mut bounded = base.clone();
    bounded["db"] = json!("é".repeat(256));
    bounded["label"] = json!("a".repeat(128));
    let calls: Vec<_> = [base, routed, pinned, bounded]
        .into_iter()
        .flat_map(|args| {
            [
                ("discovery_register", args.clone()),
                ("discovery_register", args),
            ]
        })
        .collect();
    let (options, server) =
        fixture_server_with_catalog("mneme-mcp", registration_catalog(), Some(calls.clone()));
    let mut client = RemoteClient::connect_local(
        &options,
        ClientTimeouts {
            connect: Duration::from_secs(2),
            request: Duration::from_secs(2),
        },
        "mneme-mcp",
    )
    .await
    .unwrap();
    assert!(client.supports_discovery_register());
    for (index, (tool, args)) in calls.into_iter().enumerate() {
        let received = if index % 2 == 0 {
            client.call_tool(tool, args.clone()).await.unwrap()
        } else {
            decode_tool_result(
                &client
                    .raw_rpc("tools/call", json!({"name":tool, "arguments":args}))
                    .await
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(received, args);
    }
    client.close().await;
    server.join().unwrap();
}
