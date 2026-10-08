use crate::tests::{empty_profile_server, schema_accepts, tool_input_schema};
use crate::*;

fn append(db: &str, key: &str) -> Value {
    json!({
        "db": db, "action": "append", "summary": "The violet planetarium worked.",
        "body": "A small experience without an obligatory lesson.",
        "source": {"namespace": "episode-mcp-tests", "key": key, "reference": "test://episode/1"},
        "occurred": {"kind": "point", "at": 1000}, "thread": "planetarium",
    })
}

fn revise(db: &str, episode_id: &str, expected_edition_id: &str) -> Value {
    let mut value = append(db, "revised");
    value["action"] = json!("revise");
    value["episode_id"] = json!(episode_id);
    value["expected_edition_id"] = json!(expected_edition_id);
    value["reason"] = json!("Correct the color, not the original evidence.");
    value["summary"] = json!("The blue planetarium worked.");
    value["body"] = json!("The lamps were blue, not violet.");
    value
}

pub(crate) fn request_cases() -> Vec<(&'static str, Value, CapabilityClass)> {
    let id = ulid::Ulid::new().to_string();
    vec![
        (
            "episode",
            json!({"db":"missing", "action":"list"}),
            CapabilityClass::ReadOnly,
        ),
        (
            "episode",
            json!({"db":"missing", "action":"search", "cue":"planetarium"}),
            CapabilityClass::ReadOnly,
        ),
        (
            "episode",
            json!({"db":"missing", "action":"get", "episode_id":id}),
            CapabilityClass::ReadOnly,
        ),
        (
            "episode",
            json!({"db":"missing", "action":"history", "episode_id":id}),
            CapabilityClass::ReadOnly,
        ),
        (
            "episode",
            json!({"db":"missing", "action":"references", "anchor":id}),
            CapabilityClass::ReadOnly,
        ),
        (
            "episode",
            append("missing", "initial"),
            CapabilityClass::Curator,
        ),
        (
            "episode",
            revise("missing", &id, &id),
            CapabilityClass::Operator,
        ),
    ]
}

#[test]
fn episode_catalog_parser_and_action_authority_agree() {
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        let schemas = tool_schemas(CapabilityPolicy::new(profile, false));
        let schema = tool_input_schema(&schemas, "episode");
        for (name, arguments, required) in request_cases() {
            let request = ValidatedToolArguments::parse(name, &arguments).unwrap();
            assert_eq!(request.kind().capability_class(), required);
            assert_eq!(
                request.kind().requires_explicit_db(),
                required != CapabilityClass::ReadOnly
            );
            assert_eq!(
                schema_accepts(schema, &arguments),
                profile.permits(required),
                "{profile:?}: {arguments}",
            );
            let mut implicit_db = arguments.clone();
            implicit_db.as_object_mut().unwrap().remove("db");
            assert_eq!(
                schema_accepts(schema, &implicit_db),
                required == CapabilityClass::ReadOnly && profile.permits(required)
            );
            assert_eq!(
                ValidatedToolArguments::parse(name, &implicit_db).is_ok(),
                required == CapabilityClass::ReadOnly
            );
        }
    }
}

#[test]
fn every_episode_union_arm_exposes_its_complete_database_envelope() {
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        let schemas = tool_schemas(CapabilityPolicy::new(profile, false));
        let schema = tool_input_schema(&schemas, "episode");
        for branch in schema["oneOf"].as_array().unwrap() {
            assert_eq!(branch["type"], "object");
            assert_eq!(branch["properties"]["db"], schema["properties"]["db"]);
            assert_eq!(
                branch["properties"]["expected_db_id"],
                expected_db_id_prop()
            );
            assert_eq!(
                branch["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("db")),
                matches!(
                    branch["properties"]["action"]["const"].as_str(),
                    Some("append" | "revise")
                )
            );
            for field in branch["required"].as_array().unwrap() {
                assert!(branch["properties"].get(field.as_str().unwrap()).is_some());
            }
            for (_, arguments, required) in request_cases() {
                let expected = profile.permits(required)
                    && arguments["action"] == branch["properties"]["action"]["const"];
                assert_eq!(schema_accepts(branch, &arguments), expected);
                let mut implicit = arguments;
                implicit.as_object_mut().unwrap().remove("db");
                assert_eq!(
                    schema_accepts(branch, &implicit),
                    expected && required == CapabilityClass::ReadOnly
                );
            }
        }
    }
}

fn guarded_cases(expected: &Value) -> Vec<(&'static str, Value)> {
    let mut cases = request_cases()
        .into_iter()
        .map(|(name, arguments, _)| (name, arguments))
        .collect::<Vec<_>>();
    cases.push((
        "capture",
        json!({
            "db":"missing", "summary":"A bound semantic claim.",
            "source":{"namespace":"guard-tests", "key":"claim", "reference":"test://guard"},
        }),
    ));
    cases.push((
        "get",
        json!({"db":"missing", "id":ulid::Ulid::new().to_string(), "body":true}),
    ));
    for (_, arguments) in &mut cases {
        arguments["expected_db_id"] = expected.clone();
    }
    cases
}

#[test]
fn expected_db_id_schema_and_typed_envelope_are_canonical_and_optional() {
    let schemas = tool_schemas(CapabilityPolicy::operator());
    let id = json!("01ARZ3NDEKTSV4RRFFQ69G5FAV");
    for (name, arguments) in guarded_cases(&id) {
        let schema = tool_input_schema(&schemas, name);
        assert_eq!(
            schema["properties"]["expected_db_id"],
            expected_db_id_prop()
        );
        assert!(
            !schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("expected_db_id"))
        );
        assert!(schema_accepts(schema, &arguments), "{name}: {arguments}");
        let prepared = ValidatedToolArguments::parse(name, &arguments).unwrap();
        assert_eq!(
            prepared.expected_db_id.unwrap().to_string(),
            id.as_str().unwrap()
        );
        for profile in [
            CapabilityProfile::ReadOnly,
            CapabilityProfile::ReceiptGrounded,
            CapabilityProfile::Curator,
            CapabilityProfile::Operator,
        ] {
            let permitted = profile.permits(prepared.kind().capability_class());
            let catalog = tool_schemas(CapabilityPolicy::new(profile, false));
            let advertised = catalog.iter().find(|tool| tool["name"] == name);
            assert_eq!(
                advertised.is_some_and(|tool| schema_accepts(&tool["inputSchema"], &arguments)),
                permitted,
                "{profile:?}: {name}: {arguments}",
            );
            assert_eq!(
                CapabilityPolicy::new(profile, false)
                    .authorize(prepared.kind())
                    .is_ok(),
                permitted,
            );
        }
        if name == "episode" {
            let branch = schema["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .find(|branch| branch["properties"]["action"]["const"] == arguments["action"])
                .unwrap();
            assert!(schema_accepts(branch, &arguments));
        }
        let mut unguarded = arguments;
        unguarded.as_object_mut().unwrap().remove("expected_db_id");
        assert!(
            ValidatedToolArguments::parse(name, &unguarded)
                .unwrap()
                .expected_db_id
                .is_none()
        );
    }
    // Boundary values, rather than a permissive ULID decoder's aliases.
    for id in ["00000000000000000000000000", "7ZZZZZZZZZZZZZZZZZZZZZZZZZ"] {
        assert!(
            optional_expected_db_id(&json!({"expected_db_id":id}))
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn expected_db_id_malformed_inputs_precede_checkout_and_authority() {
    let server = empty_profile_server(CapabilityProfile::ReadOnly);
    let schemas = tool_schemas(CapabilityPolicy::operator());
    for invalid in [
        Value::Null,
        json!(42),
        json!(false),
        json!({}),
        json!([]),
        json!(""),
        json!("01arz3ndektsv4rrffq69g5fav"),
        json!("81ARZ3NDEKTSV4RRFFQ69G5FAV"),
        json!("01ARZ3NDEKTSV4RRFFQ69G5FAI"),
        json!("01ARZ3NDEKTSV4RRFFQ69G5FAO"),
        json!("01ARZ3NDEKTSV4RRFFQ69G5FAL"),
        json!("01ARZ3NDEKTSV4RRFFQ69G5FAU"),
        json!(" 01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        json!("01ARZ3NDEKTSV4RRFFQ69G5FAV "),
        json!("01ARZ3NDEKTSV4RRFFQ69G5FA"),
        json!("000000000000000000000000é"),
    ] {
        for (name, arguments) in guarded_cases(&invalid) {
            assert!(
                !schema_accepts(tool_input_schema(&schemas, name), &arguments),
                "{name}: {arguments}"
            );
            let response = server
                .dispatch("tools/call", json!({"name":name,"arguments":arguments}))
                .await
                .unwrap();
            assert_eq!(response["isError"], true);
            let error = response["content"][0]["text"].as_str().unwrap();
            assert!(error.contains("expected_db_id"), "{name}: {error}");
            assert!(
                !error.contains("unknown database") && !error.contains("capability profile"),
                "{error}"
            );
        }
    }
    let operator_id = json!(ulid::Ulid::new().to_string());
    for (name, arguments) in guarded_cases(&operator_id) {
        if name == "get"
            || arguments["action"]
                .as_str()
                .is_some_and(|action| !["append", "revise"].contains(&action))
        {
            continue;
        }
        let response = server
            .dispatch("tools/call", json!({"name":name,"arguments":arguments}))
            .await
            .unwrap();
        let error = response["content"][0]["text"].as_str().unwrap();
        assert!(error.contains("capability profile"), "{error}");
        assert!(!error.contains("unknown database"), "{error}");
    }
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn expected_db_id_guards_selected_slot_replay_and_release_resume() {
    let root = std::env::temp_dir().join(format!("mneme-mcp-target-guard-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("memory.json");
    let other = root.join("other.json");
    for path in [&path, &other] {
        mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM)
            .save(path)
            .unwrap();
    }
    let mut state = SessionState::default();
    let registry = build_registry(
        vec![
            ("project".into(), path.clone()),
            ("other".into(), other.clone()),
        ],
        &mut state,
    )
    .unwrap();
    let sessions = std::sync::Arc::new(Mutex::new(state));
    let cold = ColdWorkGate::new(Duration::ZERO);
    let a_id = registry.slot("project").unwrap().status().unwrap().db_id;
    let other_id = registry.slot("other").unwrap().status().unwrap().db_id;
    let before = std::fs::read(&path).unwrap();
    let other_before = std::fs::read(&other).unwrap();
    let bodies_before = std::fs::read_dir(path.with_extension("bodies"))
        .unwrap()
        .count();
    // Every episode action and standalone get fail before their body/read path;
    // capture fails before its generation gate even in the no-Cozo build.
    for (name, mut arguments) in guarded_cases(&json!(other_id.to_string())) {
        arguments["db"] = json!("project");
        let error = call_tool_authorized(
            &registry,
            &sessions,
            &cold,
            CapabilityPolicy::operator(),
            name,
            &arguments,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("expected_db_id mismatch"), "{name}: {error}");
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        std::fs::read(&other).unwrap(),
        other_before,
        "must not redirect to matching alias"
    );
    assert_eq!(
        registry
            .checkout("project")
            .unwrap()
            .mem
            .status(ColdPath::acquire())
            .await
            .unwrap()
            .nodes,
        0,
    );
    assert_eq!(
        std::fs::read_dir(path.with_extension("bodies"))
            .unwrap()
            .count(),
        bodies_before
    );

    let mut initial = append("project", "guarded-initial");
    initial["expected_db_id"] = json!(a_id.to_string());
    let first = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "episode",
        &initial,
    )
    .await
    .unwrap();
    assert_eq!(first["db_id"], a_id.to_string());
    assert_eq!(first["replayed"], false);
    let guarded_replay = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "episode",
        &initial,
    )
    .await
    .unwrap();
    assert_eq!(guarded_replay["replayed"], true);
    assert_eq!(guarded_replay["edition_id"], first["edition_id"]);
    initial.as_object_mut().unwrap().remove("expected_db_id");
    let replay = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "episode",
        &initial,
    )
    .await
    .unwrap();
    assert_eq!(replay["replayed"], true, "guard is outside source digest");
    assert_eq!(replay["edition_id"], first["edition_id"]);
    let id = first["episode_id"].as_str().unwrap();
    let mut revision = revise("project", id, id);
    revision["expected_db_id"] = json!(a_id.to_string());
    let revised = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "episode",
        &revision,
    )
    .await
    .unwrap();
    assert_eq!(revised["revision"], 1);
    for (_, mut arguments, required) in request_cases() {
        if required != CapabilityClass::ReadOnly {
            continue;
        }
        arguments["db"] = json!("project");
        arguments["expected_db_id"] = json!(a_id.to_string());
        if arguments.get("episode_id").is_some() {
            arguments["episode_id"] = json!(id);
        }
        if arguments.get("anchor").is_some() {
            arguments["anchor"] = json!(id);
        }
        let result = call_tool_authorized(
            &registry,
            &sessions,
            &cold,
            CapabilityPolicy::new(CapabilityProfile::ReadOnly, false),
            "episode",
            &arguments,
        )
        .await
        .unwrap();
        assert_eq!(result["db_id"], first["db_id"]);
    }
    let mut get = json!({"db":"project","id":id,"body":true});
    let plain = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "get",
        &get,
    )
    .await
    .unwrap();
    assert!(plain.get("db").is_none() && plain.get("db_id").is_none());
    get["expected_db_id"] = json!(a_id.to_string());
    let mut guarded = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "get",
        &get,
    )
    .await
    .unwrap();
    assert_eq!(guarded["db"], "project");
    assert_eq!(guarded["db_id"], a_id.to_string());
    guarded.as_object_mut().unwrap().remove("db");
    guarded.as_object_mut().unwrap().remove("db_id");
    assert_eq!(guarded, plain, "unguarded get result stays unchanged");

    let held = db_with_expected_id(&registry, &get, Some(a_id)).unwrap();
    let error = registry
        .slot("project")
        .unwrap()
        .begin_release()
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("in-flight MCP operation"), "{error}");
    drop(held);
    registry
        .slot("project")
        .unwrap()
        .begin_release()
        .unwrap()
        .finish()
        .unwrap();
    let replacement = mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM);
    let b_id = replacement.db_id();
    assert_ne!(a_id, b_id);
    replacement.save(&path).unwrap();
    registry
        .slot("project")
        .unwrap()
        .begin_resume(ulid::Ulid::new().to_string())
        .unwrap()
        .finish()
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let bodies_before = std::fs::read_dir(path.with_extension("bodies"))
        .unwrap()
        .count();
    for (name, mut arguments) in guarded_cases(&json!(a_id.to_string())) {
        arguments["db"] = json!("project");
        let error = call_tool_authorized(
            &registry,
            &sessions,
            &cold,
            CapabilityPolicy::operator(),
            name,
            &arguments,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("expected_db_id mismatch"), "{name}: {error}");
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        std::fs::read_dir(path.with_extension("bodies"))
            .unwrap()
            .count(),
        bodies_before
    );
    assert!(
        mneme_cozo::MemStore::load(&path)
            .unwrap()
            .export()
            .nodes
            .is_empty()
    );
    assert_eq!(
        registry
            .checkout("project")
            .unwrap()
            .mem
            .status(ColdPath::acquire())
            .await
            .unwrap()
            .nodes,
        0,
    );
    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn episode_conditional_schemas_and_parser_reject_irrelevant_fields() {
    let schemas = tool_schemas(CapabilityPolicy::operator());
    let schema = tool_input_schema(&schemas, "episode");
    let id = ulid::Ulid::new().to_string();
    for arguments in [
        json!({"db":"project", "action":"list", "summary":"not a read"}),
        json!({"db":"project", "action":"search", "cue":"q", "after":"cursor"}),
        json!({"db":"project", "action":"get", "episode_id":id, "offset":1}),
        json!({"db":"project", "action":"get", "episode_id":id, "body":false, "max_bytes":2}),
        json!({"db":"project", "action":"history", "episode_id":id, "thread":"other"}),
        json!({"db":"project", "action":"references", "anchor":id, "episode_id":id}),
        json!({"db":"project", "action":"erase", "episode_id":id}),
        json!({"db":"project", "action":"list", "limit":0}),
        json!({"db":"project", "action":"list", "limit":33}),
        json!({"db":"project", "action":"list", "limit":null}),
    ] {
        assert!(!schema_accepts(schema, &arguments), "schema: {arguments}");
        assert!(
            ValidatedToolArguments::parse("episode", &arguments).is_err(),
            "parser: {arguments}"
        );
    }
    for field in ["active", "confidence", "stability"] {
        let mut arguments = append("project", "initial");
        arguments[field] = json!(true);
        assert!(!schema_accepts(schema, &arguments), "{field}");
        assert!(ValidatedToolArguments::parse("episode", &arguments).is_err());
    }
}

#[cfg(all(feature = "cozo", not(feature = "fastembed")))]
#[tokio::test]
async fn expected_db_id_capture_is_guarded_before_publication_and_replay_keeps_digest() {
    let root = std::env::temp_dir().join(format!("mneme-mcp-capture-guard-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let path = root.join("capture.db");
    let store = mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM);
    let db_id = store.db_id();
    mneme_cozo::CozoStore::materialize_fresh_current(&path, ulid::Ulid::new(), &store)
        .await
        .unwrap();
    let mut state = SessionState::default();
    let registry = build_registry(vec![("project".into(), path.clone())], &mut state).unwrap();
    let sessions = std::sync::Arc::new(Mutex::new(state));
    let cold = ColdWorkGate::new(Duration::ZERO);
    let mut arguments = json!({
        "db":"project", "summary":"One semantic claim with a guarded target.",
        "body":"The body must not be published before target identity matches.",
        "source":{"namespace":"guard-tests", "key":"semantic", "reference":"test://guard"},
        "expected_db_id":ulid::Ulid::new().to_string(),
    });
    let before = std::fs::read(&path).unwrap();
    let bodies_before = std::fs::read_dir(path.with_extension("bodies"))
        .unwrap()
        .count();
    let error = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "capture",
        &arguments,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("expected_db_id mismatch"), "{error}");
    assert_eq!(
        registry
            .checkout("project")
            .unwrap()
            .mem
            .status(ColdPath::acquire())
            .await
            .unwrap()
            .nodes,
        0,
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        std::fs::read_dir(path.with_extension("bodies"))
            .unwrap()
            .count(),
        bodies_before
    );
    arguments["expected_db_id"] = json!(db_id.to_string());
    let first = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "capture",
        &arguments,
    )
    .await
    .unwrap();
    assert_eq!(first["db_id"], db_id.to_string());
    assert_eq!(first["replayed"], false);
    for guarded in [true, false] {
        if !guarded {
            arguments.as_object_mut().unwrap().remove("expected_db_id");
        }
        let replay = call_tool_authorized(
            &registry,
            &sessions,
            &cold,
            CapabilityPolicy::operator(),
            "capture",
            &arguments,
        )
        .await
        .unwrap();
        assert_eq!(replay["replayed"], true);
        assert_eq!(
            replay["id"], first["id"],
            "guard must not change source digest or identity"
        );
    }
    let result = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "get",
        &json!({
            "db":"project", "id":first["id"], "body":true, "expected_db_id":db_id.to_string(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(result["db_id"], db_id.to_string());
    assert_eq!(result["body"], arguments["body"]);
    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn episode_nested_validation_precedes_checkout_and_capability_denial() {
    let server = empty_profile_server(CapabilityProfile::ReadOnly);
    let mut cases = Vec::new();
    for extra in [
        json!({"tags":["core"]}),
        json!({"source":{"namespace":"episode-mcp-tests", "key":"initial", "reference":"test://episode", "surprise":true}}),
        json!({"occurred":{"kind":"range", "start":3, "end":2}}),
        json!({"occurred":{"kind":"point", "at":18446744073709551615_u64}}),
        json!({"links":[{"to":"invalid"}]}),
        json!({"links":[{"to":ulid::Ulid::new().to_string(), "kind":"supersedes"}]}),
        json!({"thread": " bad "}),
        json!({"occurrence_contexts":null}),
        json!({"occurrence_contexts":[]}),
        json!({"occurrence_contexts":[{"namespace":"Room","key":"a","label":null}]}),
        json!({"occurrence_contexts":[{"namespace":"Room","key":"a","extra":true}]}),
        json!({"occurrence_contexts":[{"namespace":"Room","key":"a"},{"namespace":"Room","key":"a","label":"other"}]}),
        json!({"occurrence_contexts":[{"namespace":"Room","key":"x".repeat(1024)}]}),
        json!({"body":"x".repeat(16 * 1024 + 1)}),
    ] {
        let mut arguments = append("missing", "initial");
        arguments
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        cases.push(arguments);
    }
    cases.extend([
        json!({"db":"missing", "action":"list", "after":"not-a-cursor"}),
        json!({"db":"missing", "action":"list", "from":10, "through":1}),
        json!({"db":"missing", "action":"search", "cue":" "}),
        json!({"db":"missing", "action":"search", "cue":"q", "occurrence":{"kind":"overlaps"}}),
        json!({"db":"missing", "action":"list", "axis":"occurred", "occurrence":{"kind":"unknown"}}),
    ]);
    for arguments in cases {
        let response = server
            .dispatch(
                "tools/call",
                json!({"name":"episode", "arguments":arguments}),
            )
            .await
            .unwrap();
        assert_eq!(response["isError"], true, "{arguments}");
        let error = response["content"][0]["text"].as_str().unwrap();
        assert!(!error.contains("unknown database"), "checkout: {error}");
        assert!(
            !error.contains("capability profile"),
            "capability before full parse: {error}"
        );
    }
}

#[tokio::test]
async fn episode_hot_reads_do_not_take_cold_admission() {
    let server = empty_profile_server(CapabilityProfile::ReadOnly);
    let _permit = server.cold_work.admit("test held cold lane").unwrap();
    for (_, arguments, required) in request_cases() {
        if required != CapabilityClass::ReadOnly {
            continue;
        }
        let response = server
            .dispatch(
                "tools/call",
                json!({"name":"episode", "arguments":arguments}),
            )
            .await
            .unwrap();
        let error = response["content"][0]["text"].as_str().unwrap();
        assert!(error.contains("unknown database"), "{error}");
        assert!(!error.contains("cold work"), "{error}");
    }
}

#[test]
fn episode_activity_ids_are_bounded_deduplicated_and_body_free() {
    let a = ulid::Ulid::new().to_string();
    let b = ulid::Ulid::new().to_string();
    let c = ulid::Ulid::new().to_string();
    let ids = super::returned_ids(&json!({
        "edition_id":a, "body":c, "items":[{"edition_id":a},{"edge":{"from":a,"to":b}}],
    }));
    assert_eq!(
        ids.iter().map(|id| id.0.to_string()).collect::<Vec<_>>(),
        [a, b]
    );
    let many = (0..100)
        .map(|_| json!({"edition_id":ulid::Ulid::new().to_string()}))
        .collect::<Vec<_>>();
    assert_eq!(
        super::returned_ids(&json!({"items":many})).len(),
        activity::MAX_IDS + 1
    );
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn episode_writes_checkpoint_and_reads_preserve_exact_editions() {
    use mneme_core::ports::GraphStore;
    let root = std::env::temp_dir().join(format!("mneme-mcp-episodes-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("memory.json");
    mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM)
        .save(&path)
        .unwrap();
    let mut state = SessionState::default();
    let registry = build_registry(vec![("project".into(), path.clone())], &mut state).unwrap();
    let sessions = std::sync::Arc::new(Mutex::new(state));
    let cold = ColdWorkGate::new(Duration::ZERO);
    let mut initial_args = append("project", "initial");
    initial_args["occurrence_contexts"] = json!([
        {"namespace":"Place", "key":"Planetarium", "label":"Violet room"},
        {"namespace":"Chat", "key":"conversation-1"}
    ]);
    let contexts = json!([
        {"namespace":"Chat", "key":"conversation-1"},
        {"namespace":"Place", "key":"Planetarium", "label":"Violet room"}
    ]);
    let first = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::new(CapabilityProfile::Curator, false),
        "episode",
        &initial_args,
    )
    .await
    .unwrap();
    assert_eq!(first["replayed"], false);
    assert_eq!(first["db"], "project");
    let episode_id = first["episode_id"].as_str().unwrap().to_owned();
    let edition_id = first["edition_id"].as_str().unwrap().to_owned();
    assert_eq!(episode_id, edition_id);
    let revised = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "episode",
        &revise("project", &episode_id, &edition_id),
    )
    .await
    .unwrap();
    assert_eq!(revised["episode_id"], episode_id);
    assert_ne!(revised["edition_id"], edition_id);
    assert_eq!(revised["revision"], 1);
    initial_args["occurrence_contexts"]
        .as_array_mut()
        .unwrap()
        .reverse();
    let replay = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::new(CapabilityProfile::Curator, false),
        "episode",
        &initial_args,
    )
    .await
    .unwrap();
    assert_eq!(replay["edition_id"], edition_id);
    assert_eq!(replay["replayed"], true);

    let before = std::fs::read(&path).unwrap();
    let mut conflicting_replay = initial_args.clone();
    conflicting_replay["summary"] = json!("A different experience under the same key");
    assert!(
        call_tool_authorized(
            &registry,
            &sessions,
            &cold,
            CapabilityPolicy::operator(),
            "episode",
            &conflicting_replay,
        )
        .await
        .is_err()
    );
    let mut stale_revision = revise("project", &episode_id, &edition_id);
    stale_revision["source"]["key"] = json!("stale-editor");
    assert!(
        call_tool_authorized(
            &registry,
            &sessions,
            &cold,
            CapabilityPolicy::operator(),
            "episode",
            &stale_revision,
        )
        .await
        .is_err()
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "failed write checkpointed the store"
    );
    let loaded = mneme_cozo::MemStore::load(&path).unwrap();
    let expected = loaded.export();
    let _permit = cold.admit("test hold").unwrap();
    let current = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::new(CapabilityProfile::ReadOnly, false),
        "episode",
        &json!({"db":"project", "action":"get", "episode_id":episode_id, "body":true}),
    )
    .await
    .unwrap();
    assert_eq!(current["edition_id"], revised["edition_id"]);
    assert_eq!(current["body"], "The lamps were blue, not violet.");
    assert_eq!(current["db_id"], first["db_id"]);
    assert!(
        current.get("occurrence_contexts").is_none(),
        "a full replacement must not inherit context"
    );
    let old = call_tool_authorized(&registry, &sessions, &cold,
        CapabilityPolicy::new(CapabilityProfile::ReadOnly, false), "episode",
        &json!({"db":"project", "action":"get", "episode_id":episode_id, "edition_id":edition_id, "body":true})).await.unwrap();
    assert_eq!(
        old["body"],
        "A small experience without an obligatory lesson."
    );
    assert_eq!(old["is_current"], false);
    assert_eq!(old["occurrence_contexts"], contexts);
    assert_eq!(old["db_id"], first["db_id"]);
    let raw = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::new(CapabilityProfile::ReadOnly, false),
        "get",
        &json!({"db":"project", "id":edition_id, "body":true}),
    )
    .await
    .unwrap();
    assert!(raw["memory_kind"].is_object());
    assert_eq!(raw["body"], old["body"]);
    assert_eq!(
        raw["memory_kind"]["episode"]["occurrence_contexts"],
        contexts
    );
    for request in [
        json!({"db":"project", "action":"list"}),
        json!({"db":"project", "action":"history", "episode_id":episode_id}),
        json!({"db":"project", "action":"search", "cue":"planetarium"}),
        json!({"db":"project", "action":"references", "anchor":episode_id}),
    ] {
        let result = call_tool_authorized(
            &registry,
            &sessions,
            &cold,
            CapabilityPolicy::new(CapabilityProfile::ReadOnly, false),
            "episode",
            &request,
        )
        .await
        .unwrap();
        if request["action"] == "history" {
            let items = result["items"].as_array().unwrap();
            assert_eq!(
                items
                    .iter()
                    .find(|item| item["edition_id"] == edition_id)
                    .unwrap()["occurrence_contexts"],
                contexts
            );
            assert!(
                items
                    .iter()
                    .find(|item| item["edition_id"] == revised["edition_id"])
                    .unwrap()
                    .get("occurrence_contexts")
                    .is_none()
            );
        }
        assert_eq!(result["db"], "project");
        assert_eq!(result["db_id"], first["db_id"]);
        assert!(serde_json::to_vec(&result).unwrap().len() < 33 * 1024);
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "read checkpointed the store"
    );
    let handle = registry.checkout("project").unwrap();
    for node in expected.nodes {
        assert_eq!(
            serde_json::to_value(handle.mem.get_node(node.id()).await.unwrap().unwrap()).unwrap(),
            serde_json::to_value(node).unwrap(),
        );
    }
    drop(handle);
    drop(_permit);
    let status = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "status",
        &json!({"db":"project"}),
    )
    .await
    .unwrap();
    assert_eq!(status["episodes"], 1);
    assert_eq!(status["episode_editions"], 2);
    assert!(status.get("candidates").is_none());
    drop(registry);
    let reopened = mneme_cozo::MemStore::load(&path).unwrap();
    assert!(
        reopened
            .get_node(parse_id(&edition_id).unwrap())
            .await
            .unwrap()
            .unwrap()
            .episode()
            .is_some()
    );
    assert_eq!(reopened.export().nodes.len(), 2);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(feature = "cozo", not(feature = "fastembed")))]
#[tokio::test]
async fn episode_and_capture_share_current_generation_resume_and_snapshot_preserve_editions() {
    use mneme_core::ports::GraphStore;
    use mneme_cozo::{CozoStore, MemStore};
    let root = std::env::temp_dir().join(format!("mneme-mcp-episode-native-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let episodic = root.join("current.db");
    CozoStore::materialize_fresh_current(
        &episodic,
        ulid::Ulid::new(),
        &MemStore::new(mneme_embed::DEFAULT_DIM),
    )
    .await
    .unwrap();
    let mut state = SessionState::default();
    let registry = build_registry(vec![("project".into(), episodic)], &mut state).unwrap();
    let sessions = std::sync::Arc::new(Mutex::new(state));
    let cold = ColdWorkGate::new(Duration::ZERO);
    let first = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "episode",
        &append("project", "initial"),
    )
    .await
    .unwrap();
    let semantic = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "capture",
        &json!({
            "db":"project", "summary":"A semantic lesson remains distinct.",
            "source":{"namespace":"episode-mcp-tests","key":"semantic","reference":"test://lesson"},
        }),
    )
    .await
    .unwrap();
    assert_eq!(semantic["db_id"], first["db_id"]);
    let id = first["episode_id"].as_str().unwrap();
    let second = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "episode",
        &revise("project", id, id),
    )
    .await
    .unwrap();
    for action in ["release", "resume"] {
        let result = call_tool_authorized(
            &registry,
            &sessions,
            &cold,
            CapabilityPolicy::operator(),
            "database_control",
            &json!({"db":"project","action":action}),
        )
        .await
        .unwrap();
        assert_eq!(result["db_id"], first["db_id"]);
    }
    let read = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::new(CapabilityProfile::ReadOnly, false),
        "episode",
        &json!({"db":"project","action":"get","episode_id":id,"body":true}),
    )
    .await
    .unwrap();
    assert_eq!(read["edition_id"], second["edition_id"]);
    assert_eq!(read["db_id"], first["db_id"]);
    let snapshot = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::operator(),
        "snapshot_create",
        &json!({"db":"project"}),
    )
    .await
    .unwrap();
    assert_eq!(snapshot["db_id"], first["db_id"]);
    let bundle = std::path::PathBuf::from(snapshot["bundle"].as_str().unwrap());
    let copy = bundle.join("database.db");
    let lease = std::sync::Arc::new(mneme_store_path::StoreLease::acquire(&copy).unwrap());
    CozoStore::require_existing_current(&copy, &lease).unwrap();
    let copied =
        CozoStore::open_existing_persistent(&copy, mneme_embed::DEFAULT_DIM, lease).unwrap();
    for (result, body) in [
        (&first, "A small experience without an obligatory lesson."),
        (&second, "The lamps were blue, not violet."),
    ] {
        let node = copied
            .get_node(parse_id(result["edition_id"].as_str().unwrap()).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(node.episode().is_some());
        let key = node.body().as_str().strip_prefix("fs://").unwrap();
        assert_eq!(
            std::fs::read_to_string(bundle.join("bodies").join(key)).unwrap(),
            body
        );
    }
    drop(copied);
    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn episode_context_schema_composes_without_changing_authority_or_read_fields() {
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        let catalog = tool_schemas(CapabilityPolicy::new(profile, false));
        let schema = tool_input_schema(&catalog, "episode");
        for (_, mut arguments, required) in request_cases() {
            arguments["occurrence_contexts"] = json!([{"namespace":"Place", "key":"shared room"}]);
            let write = matches!(arguments["action"].as_str(), Some("append" | "revise"));
            assert_eq!(
                schema_accepts(schema, &arguments),
                write && profile.permits(required)
            );
            assert_eq!(
                ValidatedToolArguments::parse("episode", &arguments).is_ok(),
                write
            );
            if write {
                let prepared = ValidatedToolArguments::parse("episode", &arguments).unwrap();
                assert_eq!(prepared.kind().capability_class(), required);
            }
        }
    }
}
