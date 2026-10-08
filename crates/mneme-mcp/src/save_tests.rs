use super::*;
use crate::tests::{empty_profile_server, schema_accepts, tool_input_schema};
use crate::*;

pub(crate) fn request_cases() -> Vec<(&'static str, Value, CapabilityClass)> {
    vec![
        (
            "save",
            json!({"db":"missing","summary":"Manual note"}),
            CapabilityClass::Curator,
        ),
        (
            "save",
            json!({"db":"missing","summary":"Sourced note","source":{"namespace":"host","key":"item","reference":"test://item"}}),
            CapabilityClass::Curator,
        ),
        (
            "save",
            json!({"db":"missing","kind":"episode","summary":"A scene"}),
            CapabilityClass::Curator,
        ),
        (
            "save",
            json!({"db":"missing","summary":"Core note","tags":["core"]}),
            CapabilityClass::Operator,
        ),
    ]
}

#[tokio::test]
async fn save_profile_catalog_raw_dispatch_and_shared_stdio_http_handler_agree() {
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        let server = empty_profile_server(profile);
        let schemas = tool_schemas(server.capability);
        let advertised = schemas.iter().find(|tool| tool["name"] == "save");
        assert_eq!(
            advertised.is_some(),
            profile.permits(CapabilityClass::Curator)
        );
        let _cold = server.cold_work.admit("held lane").unwrap();
        for (name, arguments, required) in request_cases() {
            let allowed = profile.permits(required);
            assert_eq!(
                advertised.is_some_and(|schema| schema_accepts(&schema["inputSchema"], &arguments)),
                allowed,
                "{profile:?}: {arguments}"
            );
            let prepared = ValidatedToolArguments::parse(name, &arguments).unwrap();
            assert_eq!(prepared.kind().capability_class(), required);
            assert_eq!(
                server.capability.authorize(prepared.kind()).is_ok(),
                allowed
            );
            let msg = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}});
            // Both transports call handle and consume the same bounded bytes.
            let response = server.handle(msg).await.unwrap();
            let mut stdio = Vec::new();
            response.write_stdio(&mut stdio).unwrap();
            assert_eq!(&stdio[..stdio.len() - 1], response.bytes());
            #[cfg(feature = "http")]
            assert_eq!(
                serde_json::to_vec(response.value()).unwrap(),
                response.bytes()
            );
            let response: Value = serde_json::from_slice(response.bytes()).unwrap();
            assert_eq!(response["result"]["isError"], true);
            let error = response["result"]["content"][0]["text"].as_str().unwrap();
            assert_eq!(error.contains("capability profile"), !allowed, "{error}");
            assert_eq!(error.contains("unknown database"), allowed, "{error}");
            assert!(!error.contains("cold work"), "{error}");
        }
    }
}

#[tokio::test]
async fn save_complete_malformed_admission_precedes_authority_and_checkout() {
    let server = empty_profile_server(CapabilityProfile::ReadOnly);
    let schemas = tool_schemas(CapabilityPolicy::operator());
    let schema = tool_input_schema(&schemas, "save");
    for arguments in [
        json!(null),
        json!([]),
        json!({"summary":"Missing owner"}),
        json!({"db":null,"summary":"Wrong owner"}),
        json!({"db":"missing","summary":"x","kind":null}),
        json!({"db":"missing","summary":"x","kind":"unknown"}),
        json!({"db":"missing","summary":"x","operation_id":null}),
        json!({"db":"missing","summary":"x","source":{},"operation_id":"x"}),
        json!({"db":"missing","summary":"x","expected_db_id":null}),
        json!({"db":"missing","summary":"x","expected_db_id":"01arz3ndektsv4rrffq69g5fav"}),
        json!({"db":"missing","summary":"x","source":{"namespace":"host","key":7,"reference":"test://x"}}),
        json!({"db":"missing","summary":"x","links":[{"to":"invalid"}]}),
        json!({"db":"missing","summary":"x","action":"append"}),
        json!({"db":"missing","summary":"x","unknown":true}),
        json!({"db":"missing","kind":"note","summary":"x","occurred":{"kind":"point","at":1}}),
        json!({"db":"missing","kind":"episode","summary":"x","tags":["core"]}),
        json!({"db":"missing","kind":"episode","summary":"x","stability":0.5}),
        json!({"db":"missing","kind":"episode","summary":"x","occurred":{"kind":"point","at":18446744073709551615_u64}}),
    ] {
        assert!(
            ValidatedToolArguments::parse("save", &arguments).is_err(),
            "{arguments}"
        );
        // Some semantic bounds (canonical ULID/occurrence ranges) exceed the
        // compact schema's syntax, but union/type/closed-shape cases match it.
        if arguments.get("links").is_none()
            && (arguments.get("occurred").is_none() || arguments["kind"] != "episode")
        {
            assert!(!schema_accepts(schema, &arguments), "{arguments}");
        }
        let response = server
            .dispatch("tools/call", json!({"name":"save","arguments":arguments}))
            .await
            .unwrap();
        let error = response["content"][0]["text"].as_str().unwrap();
        assert!(
            !error.contains("capability profile") && !error.contains("unknown database"),
            "{error}"
        );
    }
}

#[test]
fn save_manual_nonce_is_retained_across_classification() {
    let raw = json!({"db":"project","summary":"One manual operation"});
    let mut request = ValidatedToolArguments::parse("save", &raw).unwrap();
    let identity = request
        .prepared_save
        .as_ref()
        .unwrap()
        .inner
        .identity()
        .clone();
    let id = request
        .prepared_save
        .as_ref()
        .unwrap()
        .inner
        .expected_id()
        .unwrap();
    for _ in 0..4 {
        assert_eq!(
            request.kind(),
            ToolRequestKind::Save(IngestRequest::Ordinary)
        );
        CapabilityPolicy::new(CapabilityProfile::Curator, false)
            .authorize(request.kind())
            .unwrap();
        assert_eq!(
            request.prepared_save.as_ref().unwrap().inner.identity(),
            &identity
        );
    }
    let retained = request.prepared_save.take().unwrap();
    assert_eq!(retained.inner.expected_id().unwrap(), id);
    assert_eq!(retained.inner.identity(), &identity);
    let fresh = ValidatedToolArguments::parse("save", &raw).unwrap();
    assert_ne!(fresh.prepared_save.unwrap().inner.identity(), &identity);
}

#[cfg(not(feature = "fastembed"))]
fn fixture() -> (std::path::PathBuf, Server) {
    let root = std::env::temp_dir().join(format!("mneme-mcp-save-{}", Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("memory.json");
    mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM)
        .save(&path)
        .unwrap();
    let mut state = SessionState::default();
    let registry = build_registry(vec![("project".into(), path)], &mut state).unwrap();
    (
        root,
        Server {
            registry,
            sessions: std::sync::Arc::new(Mutex::new(state)),
            cold_work: ColdWorkGate::new(Duration::ZERO),
            capability: CapabilityPolicy::operator(),
            library: None,
        },
    )
}

#[cfg(not(feature = "fastembed"))]
async fn save_call(server: &Server, arguments: &Value) -> Result<Value, AnyErr> {
    call_tool_authorized(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        server.capability,
        "save",
        arguments,
    )
    .await
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn save_manual_and_sourced_replay_conflicts_and_generated_receipt_proof() {
    let (root, server) = fixture();
    for kind in ["note", "episode"] {
        for sourced in [false, true] {
            let key = format!("{kind}-{sourced}");
            let mut raw = json!({"db":"project","kind":kind,"summary":"Keep the claim","body":"The original body"});
            if sourced {
                raw["source"] = json!({"namespace":"host","key":key,"reference":"test://item"});
            } else {
                raw["operation_id"] = json!(key);
            }
            let first = save_call(&server, &raw).await.unwrap();
            assert_eq!(first["kind"], kind);
            assert_eq!(first["db"], "project");
            assert_eq!(first["replayed"], false);
            assert!(first.get("body").is_none() && first.get("action").is_none());
            assert_eq!(
                first["origin"],
                if sourced {
                    "provided_source"
                } else {
                    "manual_submission"
                }
            );
            assert_eq!(first.get("operation_id").is_some(), !sourced);
            if kind == "episode" {
                assert_eq!(first["id"], first["edition_id"]);
                assert_eq!(first["revision"], 0);
            }
            let replay = save_call(&server, &raw).await.unwrap();
            assert_eq!(replay["id"], first["id"]);
            assert_eq!(replay["replayed"], true);
            for (field, value) in [
                ("body", json!("Changed body")),
                (
                    "kind",
                    json!(if kind == "note" { "episode" } else { "note" }),
                ),
            ] {
                let mut changed = raw.clone();
                changed[field] = value;
                let error = save_call(&server, &changed).await.unwrap_err().to_string();
                assert!(error.contains("conflict"), "{error}");
            }
            if sourced {
                let mut changed = raw.clone();
                changed["source"]["reference"] = json!("test://changed");
                let error = save_call(&server, &changed).await.unwrap_err().to_string();
                assert!(error.contains("conflict"), "{error}");
            }
        }
        let raw = json!({"db":"project","kind":kind,"summary":"Fresh submission"});
        let first = save_call(&server, &raw).await.unwrap();
        assert_ne!(first["operation_id"], json!(1));
        let mut retry = raw.clone();
        retry["operation_id"] = first["operation_id"].clone();
        let replay = save_call(&server, &retry).await.unwrap();
        assert_eq!(replay["id"], first["id"]);
        assert_eq!(replay["replayed"], true);
        let fresh = save_call(&server, &raw).await.unwrap();
        assert_ne!(fresh["id"], first["id"]);
    }
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn save_expected_target_release_resume_and_absent_owner_fail_closed() {
    let (root, server) = fixture();
    let db_id = server
        .registry
        .slot("project")
        .unwrap()
        .status()
        .unwrap()
        .db_id;
    let path = root.join("memory.json");
    let before = std::fs::read(&path).unwrap();
    let mut raw = json!({"db":"project","summary":"Guarded","operation_id":"guard","expected_db_id":Ulid::new().to_string()});
    assert!(
        save_call(&server, &raw)
            .await
            .unwrap_err()
            .to_string()
            .contains("expected_db_id mismatch")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    raw["db"] = json!("typo");
    assert!(
        save_call(&server, &raw)
            .await
            .unwrap_err()
            .to_string()
            .contains("unknown database")
    );
    assert!(!root.join("typo").exists());
    raw["db"] = json!("project");
    raw["expected_db_id"] = json!(db_id.to_string());
    let first = save_call(&server, &raw).await.unwrap();
    call_tool_authorized(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        server.capability,
        "database_control",
        &json!({"db":"project","action":"release"}),
    )
    .await
    .unwrap();
    let error = save_call(&server, &raw).await.unwrap_err().to_string();
    assert!(
        error.contains("released") || error.contains("maintenance"),
        "{error}"
    );
    call_tool_authorized(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        server.capability,
        "database_control",
        &json!({"db":"project","action":"resume"}),
    )
    .await
    .unwrap();
    let replay = save_call(&server, &raw).await.unwrap();
    assert_eq!(replay["id"], first["id"]);
    assert_eq!(replay["replayed"], true);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(not(feature = "cozo"), not(feature = "fastembed")))]
#[tokio::test]
async fn save_registered_but_absent_snapshot_is_not_implicitly_created() {
    let root = std::env::temp_dir().join(format!("mneme-mcp-save-absent-{}", Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("absent.json");
    let mut state = SessionState::default();
    let registry = build_registry(vec![("project".into(), path.clone())], &mut state).unwrap();
    let sessions = std::sync::Arc::new(Mutex::new(state));
    let cold = ColdWorkGate::new(Duration::ZERO);
    for kind in ["note", "episode"] {
        let error = call_tool_authorized(
            &registry,
            &sessions,
            &cold,
            CapabilityPolicy::operator(),
            "save",
            &json!({"db":"project","kind":kind,"summary":"No implicit creation"}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("existing database"), "{error}");
        assert!(!path.exists());
        assert_eq!(
            registry
                .checkout("project")
                .unwrap()
                .mem
                .status(ColdPath::acquire())
                .await
                .unwrap()
                .nodes,
            0
        );
    }
    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(feature = "cozo", not(feature = "fastembed")))]
#[tokio::test]
async fn save_current_persistent_owner_checkpoints_and_refuses_noncurrent_resume() {
    let root = std::env::temp_dir().join(format!("mneme-mcp-save-current-{}", Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let path = root.join("memory.db");
    mneme_cozo::CozoStore::materialize_fresh_current(
        &path,
        Ulid::new(),
        &mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM),
    )
    .await
    .unwrap();
    let mut state = SessionState::default();
    let registry = build_registry(vec![("project".into(), path.clone())], &mut state).unwrap();
    let server = Server {
        registry,
        sessions: std::sync::Arc::new(Mutex::new(state)),
        cold_work: ColdWorkGate::new(Duration::ZERO),
        capability: CapabilityPolicy::operator(),
        library: None,
    };
    for kind in ["note", "episode"] {
        let raw = json!({"db":"project","kind":kind,"summary":"Existing current native owner","operation_id":kind});
        let first = save_call(&server, &raw).await.unwrap();
        let replay = save_call(&server, &raw).await.unwrap();
        assert_eq!(first["id"], replay["id"]);
        assert_eq!(replay["replayed"], true);
    }
    call_tool_authorized(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        server.capability,
        "database_control",
        &json!({"db":"project","action":"release"}),
    )
    .await
    .unwrap();
    // A registered logical name is not an upgrade/create capability. Replace
    // its released current store with a non-current artifact and refuse resume.
    std::fs::write(
        &path,
        b"SQLite format 3\0not a supported current generation",
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    let error = call_tool_authorized(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        server.capability,
        "database_control",
        &json!({"db":"project","action":"resume"}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("generation") || error.contains("SQLite") || error.contains("unsupported"),
        "{error}"
    );
    for kind in ["note", "episode"] {
        let error = save_call(
            &server,
            &json!({"db":"project","kind":kind,"summary":"Must not repair"}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("released") || error.contains("maintenance"),
            "{error}"
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn save_success_uses_shared_bounded_transport_result_and_not_rpc_identity() {
    let (root, server) = fixture();
    for kind in ["note", "episode"] {
        let mut raw = json!({"db":"project","kind":kind,"summary":"A transport submission","body":"Unquoted body"});
        if kind == "episode" {
            raw["occurrence_contexts"] =
                json!([{"namespace":"Place", "key":"Shared room", "label":"Room"}]);
        }
        let msg = json!({"jsonrpc":"2.0","id":"repeated-rpc-id","method":"tools/call","params":{"name":"save","arguments":raw}});
        let response = server.handle(msg.clone()).await.unwrap();
        let mut stdio = Vec::new();
        response.write_stdio(&mut stdio).unwrap();
        assert_eq!(&stdio[..stdio.len() - 1], response.bytes());
        #[cfg(feature = "http")]
        assert_eq!(
            serde_json::to_vec(response.value()).unwrap(),
            response.bytes()
        );
        let decoded: Value = serde_json::from_slice(response.bytes()).unwrap();
        assert_eq!(decoded["id"], "repeated-rpc-id");
        assert_eq!(decoded["result"]["isError"], false);
        let receipt: Value =
            serde_json::from_str(decoded["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_ne!(receipt["operation_id"], "repeated-rpc-id");
        let mut claim = raw.clone();
        claim.as_object_mut().unwrap().remove("db");
        claim["operation_id"] = receipt["operation_id"].clone();
        let proof = shared::PreparedSave::parse(&claim, "").unwrap();
        proof.verify_write_receipt_json(&receipt).unwrap();
        assert_eq!(proof.expected_id().unwrap().0.to_string(), receipt["id"]);
        let readback_args = if kind == "note" {
            json!({"db":"project","id":receipt["id"],"body":true})
        } else {
            json!({"db":"project","action":"get","episode_id":receipt["episode_id"],"body":true})
        };
        let readback = call_tool_authorized(
            &server.registry,
            &server.sessions,
            &server.cold_work,
            server.capability,
            if kind == "note" { "get" } else { "episode" },
            &readback_args,
        )
        .await
        .unwrap();
        proof.verify_readback_json(&receipt, &readback).unwrap();
        if kind == "episode" {
            assert_eq!(readback["occurrence_contexts"], raw["occurrence_contexts"]);
            let mut changed = readback.clone();
            changed["occurrence_contexts"][0]["key"] = json!("Different room");
            assert!(proof.verify_readback_json(&receipt, &changed).is_err());
        }
        let fresh = server.handle(msg).await.unwrap();
        let decoded: Value = serde_json::from_slice(fresh.bytes()).unwrap();
        let fresh: Value =
            serde_json::from_str(decoded["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_ne!(fresh["id"], receipt["id"]);
    }
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn save_contexts_use_episode_only_strict_admission_before_checkout() {
    let server = empty_profile_server(CapabilityProfile::ReadOnly);
    let catalog = tool_schemas(CapabilityPolicy::operator());
    let schema = tool_input_schema(&catalog, "save");
    let contexts = json!([{"namespace":"Place", "key":"Shared room"}]);
    for kind in ["note", "episode"] {
        let arguments =
            json!({"db":"missing", "kind":kind, "summary":"Scene", "occurrence_contexts":contexts});
        assert_eq!(schema_accepts(schema, &arguments), kind == "episode");
        assert_eq!(
            ValidatedToolArguments::parse("save", &arguments).is_ok(),
            kind == "episode"
        );
        if kind == "episode" {
            assert_eq!(
                ValidatedToolArguments::parse("save", &arguments)
                    .unwrap()
                    .kind()
                    .capability_class(),
                CapabilityClass::Curator
            );
        }
    }
    for invalid in [
        json!(null),
        json!([]),
        json!([{}]),
        json!([{"namespace":"Place","key":"a","label":null}]),
        json!([{"namespace":"Place","key":"a","unknown":true}]),
        json!([{"namespace":" Place","key":"a"}]),
        json!([{"namespace":"Place","key":"a"},{"namespace":"Place","key":"a","label":"other"}]),
        json!([{"namespace":"Place","key":"x".repeat(1024)}]),
    ] {
        let arguments = json!({"db":"missing", "kind":"episode", "summary":"Scene", "occurrence_contexts":invalid});
        assert!(ValidatedToolArguments::parse("save", &arguments).is_err());
        let response = server
            .dispatch("tools/call", json!({"name":"save", "arguments":arguments}))
            .await
            .unwrap();
        let error = response["content"][0]["text"].as_str().unwrap();
        assert!(
            !error.contains("unknown database") && !error.contains("capability profile"),
            "{error}"
        );
    }
}
