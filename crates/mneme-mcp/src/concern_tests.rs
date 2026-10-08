use super::*;
use crate::tests::{empty_profile_server, schema_accepts, tool_input_schema};
use crate::*;
use mneme_core::{
    BodyRef, ConcernBinding, ConcernDigest, ConcernEndpoint, ConcernEvidence, ConcernKind,
    ConcernNotice, ConcernRow, ConcernUpdate, NodeStatus, ScopedConcernFinding, ports::GraphStore,
};
use ulid::Ulid;

fn binding() -> ConcernBinding {
    ConcernBinding::new(
        ConcernKind::Disagreement,
        ConcernEndpoint::new(NodeId(Ulid::from(1)), ConcernDigest::of_bytes(b"old")),
        ConcernEndpoint::new(NodeId(Ulid::from(2)), ConcernDigest::of_bytes(b"new")),
    )
    .unwrap()
}
fn notice(binding: ConcernBinding) -> Value {
    serde_json::to_value(ConcernUpdate::Notice(
        ConcernNotice::new(
            binding,
            "Different shutdown claims",
            "Which binary was inspected?",
        )
        .unwrap(),
    ))
    .unwrap()
}
fn record(row: ConcernRow, scope: &str) -> Value {
    serde_json::to_value(ConcernUpdate::RecordScopedFinding {
        expected: row,
        finding: ScopedConcernFinding::new(
            scope,
            "The inspected binary contains the fix",
            vec![ConcernEvidence::new("tool://version", ConcernDigest::of_bytes(b"v2")).unwrap()],
        )
        .unwrap(),
    })
    .unwrap()
}
fn routed(mut raw: Value, db: &str, db_id: Ulid) -> Value {
    raw["db"] = json!(db);
    raw["expected_db_id"] = json!(db_id.to_string());
    raw
}

pub(crate) fn request_cases() -> Vec<(&'static str, Value, CapabilityClass)> {
    let row = ConcernRow::from_notice(
        serde_json::from_value(notice(binding())["notice"].clone()).unwrap(),
    );
    vec![
        (
            "concern",
            json!({"action":"list","db":"missing","endpoint":Ulid::from(1).to_string(),"limit":1}),
            CapabilityClass::ReadOnly,
        ),
        (
            "concern",
            routed(notice(binding()), "missing", Ulid::from(9)),
            CapabilityClass::Curator,
        ),
        (
            "concern",
            routed(record(row, "task v2"), "missing", Ulid::from(9)),
            CapabilityClass::Curator,
        ),
    ]
}

#[tokio::test]
async fn concern_profiles_catalog_and_raw_shared_transport_dispatch_agree_before_checkout() {
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        let server = empty_profile_server(profile);
        let schemas = tool_schemas(server.capability);
        let schema = tool_input_schema(&schemas, "concern");
        let _held = server.cold_work.admit("held lane").unwrap();
        for (name, arguments, class) in request_cases() {
            let admitted = ValidatedToolArguments::parse(name, &arguments).unwrap();
            assert_eq!(admitted.kind().capability_class(), class);
            let allowed = profile.permits(class);
            assert_eq!(
                schema_accepts(schema, &arguments),
                allowed,
                "{profile:?}: {arguments}"
            );
            assert_eq!(
                server.capability.authorize(admitted.kind()).is_ok(),
                allowed
            );
            let response=server.handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})).await.unwrap();
            let mut stdio = Vec::new();
            response.write_stdio(&mut stdio).unwrap();
            assert_eq!(&stdio[..stdio.len() - 1], response.bytes());
            #[cfg(feature = "http")]
            assert_eq!(
                serde_json::to_vec(response.value()).unwrap(),
                response.bytes()
            );
            let decoded: Value = serde_json::from_slice(response.bytes()).unwrap();
            let error = decoded["result"]["content"][0]["text"].as_str().unwrap();
            assert_eq!(error.contains("capability profile"), !allowed, "{error}");
            assert_eq!(error.contains("unknown database"), allowed, "{error}");
            assert!(!error.contains("cold work"), "{error}");
        }
        let implicit = json!({"action":"list","endpoint":Ulid::from(1).to_string()});
        assert!(schema_accepts(schema, &implicit));
        assert!(
            !ValidatedToolArguments::parse("concern", &implicit)
                .unwrap()
                .kind()
                .requires_explicit_db()
        );
    }
}

#[tokio::test]
async fn concern_malformed_and_missing_write_guards_are_rejected_before_authority_or_checkout() {
    let server = empty_profile_server(CapabilityProfile::ReadOnly);
    let mutation = routed(notice(binding()), "missing", Ulid::from(9));
    let mut cases = vec![
        Value::Null,
        json!([]),
        json!({"action":"unknown"}),
        json!({"action":"list","endpoint":Ulid::from(1).to_string(),"limit":0}),
        json!({"action":"list","endpoint":Ulid::from(1).to_string(),"limit":shared::MAX_CONCERN_PUBLIC_PAGE_ROWS+1}),
        json!({"action":"list","endpoint":Ulid::from(1).to_string(),"limit":null}),
        json!({"action":"list","endpoint":Ulid::from(1).to_string(),"notice":{}}),
    ];
    for field in ["db", "expected_db_id"] {
        let mut missing = mutation.clone();
        missing.as_object_mut().unwrap().remove(field);
        cases.push(missing);
        let mut wrong = mutation.clone();
        wrong[field] = Value::Null;
        cases.push(wrong);
    }
    let mut malformed = mutation.clone();
    malformed["notice"]["binding"]["endpoints"][0]["meaning"] = json!("invalid");
    cases.push(malformed);
    let mut wrong = mutation;
    wrong["notice"]["unexpected"] = json!(true);
    cases.push(wrong);
    for arguments in cases {
        assert!(
            ValidatedToolArguments::parse("concern", &arguments).is_err(),
            "{arguments}"
        );
        let response = server
            .dispatch(
                "tools/call",
                json!({"name":"concern","arguments":arguments}),
            )
            .await
            .unwrap();
        let error = response["content"][0]["text"].as_str().unwrap();
        assert!(
            !error.contains("unknown database") && !error.contains("capability profile"),
            "{error}"
        );
    }
    let schemas = tool_schemas(CapabilityPolicy::operator());
    let schema = tool_input_schema(&schemas, "concern");
    for (_, mut arguments, class) in request_cases() {
        if class == CapabilityClass::ReadOnly {
            arguments["after"] = Value::Null;
            assert!(schema_accepts(schema, &arguments));
            assert!(ValidatedToolArguments::parse("concern", &arguments).is_ok());
            continue;
        }
        for field in ["db", "expected_db_id"] {
            let mut missing = arguments.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(!schema_accepts(schema, &missing));
        }
    }
}

#[cfg(not(feature = "fastembed"))]
pub(crate) async fn fixture() -> (std::path::PathBuf, Server, Vec<Node>) {
    let root = std::env::temp_dir().join(format!("mneme-mcp-concern-{}", Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let store = mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM);
    // A populated fixture must carry the same current runtime identity as a
    // real initialized store; never bypass production legacy-index admission.
    use mneme_core::ports::{Embedder, EmbeddingMetadataStore};
    let embedder = mneme_embed::HashingEmbedder::new(mneme_embed::DEFAULT_DIM);
    store
        .set_embedding_fingerprint(&embedder.fingerprint())
        .unwrap();
    let mut nodes = Vec::new();
    for (id, summary) in [
        (1, "Old shutdown claim"),
        (2, "New shutdown claim"),
        (3, "Another meaning"),
    ] {
        let node = Node::try_new(
            NodeId(Ulid::from(id as u128)),
            summary,
            BodyRef::new("file:///unresolved-body").unwrap(),
            ["fact"],
            Provenance::derived_empty(),
            1.0,
            1.0,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        store.put_node(&node).await.unwrap();
        nodes.push(node);
    }
    let path = root.join("memory.json");
    store.save(&path).unwrap();
    let mut state = SessionState::default();
    let registry = build_registry(vec![("project".into(), path)], &mut state).unwrap();
    (
        root,
        Server {
            registry,
            sessions: std::sync::Arc::new(Mutex::new(state)),
            cold_work: ColdWorkGate::new(Duration::ZERO),
            capability: CapabilityPolicy::new(CapabilityProfile::Curator, false),
            library: None,
        },
        nodes,
    )
}
#[cfg(not(feature = "fastembed"))]
async fn call(server: &Server, arguments: &Value) -> Result<Value, AnyErr> {
    call_tool_authorized(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        server.capability,
        "concern",
        arguments,
    )
    .await
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn concern_exact_get_metadata_and_atomic_result_preserve_notice_finding_and_stale_outcome() {
    let (root, server, nodes) = fixture().await;
    let db_id = server
        .registry
        .slot("project")
        .unwrap()
        .status()
        .unwrap()
        .db_id;
    let binding =
        shared::binding_for_nodes(ConcernKind::Disagreement, &nodes[0], &nodes[1]).unwrap();
    let first_raw = routed(notice(binding), "project", db_id);
    let first = call(&server, &first_raw).await.unwrap();
    assert_eq!(first["outcome"]["status"], "applied");
    assert_eq!(first.as_object().unwrap().len(), 4);
    assert_eq!(first["db"], "project");
    assert_eq!(first["db_id"], db_id.to_string());
    let mut claim = first_raw.clone();
    claim.as_object_mut().unwrap().remove("db");
    claim.as_object_mut().unwrap().remove("expected_db_id");
    let proof = PreparedConcernRequest::parse(&claim).unwrap();
    proof
        .validate_routed_response_json(&first, "project", Some(db_id))
        .unwrap();
    let expected: ConcernRow = serde_json::from_value(first["outcome"]["row"].clone()).unwrap();
    let record_raw = routed(record(expected.clone(), "installed v2"), "project", db_id);
    let recorded = call(&server, &record_raw).await.unwrap();
    assert_eq!(recorded["outcome"]["status"], "applied");
    let replay = call(&server, &record_raw).await.unwrap();
    assert_eq!(replay["outcome"]["status"], "unchanged");
    assert_eq!(replay["outcome"]["row"], recorded["outcome"]["row"]);
    let historical = call(&server, &first_raw).await.unwrap();
    assert_eq!(historical["outcome"]["row"], recorded["outcome"]["row"]);
    let competing = routed(record(expected, "another task"), "project", db_id);
    let stale = call(&server, &competing).await.unwrap();
    assert_eq!(stale["outcome"]["status"], "refused");
    assert_eq!(stale["outcome"]["reason"], "stale_row");
    assert_eq!(stale["outcome"]["row"], recorded["outcome"]["row"]);
    let listed=call(&server,&json!({"db":"project","action":"list","endpoint":nodes[0].id().0.to_string(),"after":null})).await.unwrap();
    assert_eq!(listed["page"]["items"], json!([recorded["outcome"]["row"]]));
    assert_eq!(listed.as_object().unwrap().len(), 4);
    let get = call_tool_authorized(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        server.capability,
        "get",
        &json!({"db":"project","id":nodes[0].id().0.to_string()}),
    )
    .await
    .unwrap();
    assert_eq!(
        get["concern_endpoint"],
        serde_json::to_value(shared::endpoint_for_node(&nodes[0])).unwrap()
    );
    assert!(get.get("body").is_none());
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn concern_wrong_target_held_checkout_and_released_owner_remain_fenced() {
    let (root, server, nodes) = fixture().await;
    let db_id = server
        .registry
        .slot("project")
        .unwrap()
        .status()
        .unwrap()
        .db_id;
    let raw =
        notice(shared::binding_for_nodes(ConcernKind::Disagreement, &nodes[0], &nodes[1]).unwrap());
    let before = std::fs::read(root.join("memory.json")).unwrap();
    assert!(
        call(&server, &routed(raw.clone(), "project", Ulid::new()))
            .await
            .unwrap_err()
            .to_string()
            .contains("expected_db_id mismatch")
    );
    assert_eq!(std::fs::read(root.join("memory.json")).unwrap(), before);
    let checkout = server.registry.checkout("project").unwrap();
    let release = call_tool_authorized(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        CapabilityPolicy::operator(),
        "database_control",
        &json!({"db":"project","action":"release"}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        release.contains("in-flight") || release.contains("checkout"),
        "{release}"
    );
    drop(checkout);
    call_tool_authorized(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        CapabilityPolicy::operator(),
        "database_control",
        &json!({"db":"project","action":"release"}),
    )
    .await
    .unwrap();
    for arguments in [
        routed(raw, "project", db_id),
        json!({"db":"project","action":"list","endpoint":nodes[0].id().0.to_string(),"limit":1}),
    ] {
        let error = call(&server, &arguments).await.unwrap_err().to_string();
        assert!(
            error.contains("released") || error.contains("maintenance"),
            "{error}"
        );
    }
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn concern_maximum_escaped_legal_fields_fit_real_tool_text_and_shared_stdio_http_frame() {
    let escaped = |max: usize| format!("x{}", "\u{1}".repeat(max - 1));
    let evidence = vec![
        ConcernEvidence::new(
            escaped(mneme_core::MAX_CONCERN_EVIDENCE_REF_BYTES),
            ConcernDigest::of_bytes(b"first"),
        )
        .unwrap(),
        ConcernEvidence::new(
            escaped(mneme_core::MAX_CONCERN_EVIDENCE_REF_BYTES),
            ConcernDigest::of_bytes(b"second"),
        )
        .unwrap(),
        ConcernEvidence::new(
            escaped(mneme_core::MAX_CONCERN_EVIDENCE_REF_BYTES),
            ConcernDigest::of_bytes(b"third"),
        )
        .unwrap(),
        ConcernEvidence::new(escaped(48), ConcernDigest::of_bytes(b"fourth")).unwrap(),
    ];
    let finding = ScopedConcernFinding::new(
        escaped(mneme_core::MAX_CONCERN_SCOPE_BYTES),
        escaped(mneme_core::MAX_CONCERN_FINDING_BYTES),
        evidence,
    )
    .unwrap();
    let mut rows = Vec::new();
    for other in 2..2 + shared::MAX_CONCERN_PUBLIC_PAGE_ROWS {
        let binding = ConcernBinding::new(
            ConcernKind::Disagreement,
            ConcernEndpoint::new(NodeId(Ulid::from(1)), ConcernDigest::of_bytes(b"a")),
            ConcernEndpoint::new(
                NodeId(Ulid::from(other as u128)),
                ConcernDigest::of_bytes(b"b"),
            ),
        )
        .unwrap();
        let notice = ConcernNotice::new(
            binding,
            escaped(mneme_core::MAX_CONCERN_BYTES),
            escaped(mneme_core::MAX_CONCERN_MISSING_FACT_BYTES),
        )
        .unwrap();
        let mut row = serde_json::to_value(ConcernRow::from_notice(notice)).unwrap();
        row["finding"] = serde_json::to_value(&finding).unwrap();
        rows.push(row);
    }
    let db = "\"".repeat(256);
    let next = mneme_core::ConcernPageCursor::new(
        NodeId(Ulid::from(1)),
        NodeId(Ulid::from(1 + shared::MAX_CONCERN_PUBLIC_PAGE_ROWS as u128)),
        ConcernKind::Disagreement,
    )
    .unwrap();
    let response = json!({"db":db,"db_id":Ulid::from(9).to_string(),"action":"list","page":{"items":rows,"next":next}});
    let request = PreparedConcernRequest::parse(
        &json!({"action":"list","endpoint":Ulid::from(1).to_string()}),
    )
    .unwrap();
    request
        .validate_routed_response_json(&response, &db, Some(Ulid::from(9)))
        .unwrap();
    let result = crate::response::tool_success(&response);
    assert_eq!(result["isError"], false);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.len() < crate::response::MAX_TOOL_TEXT_BYTES);
    let frame = BoundedResponse::new(json!({"jsonrpc":"2.0","id":1,"result":result}));
    let decoded: Value = serde_json::from_slice(frame.bytes()).unwrap();
    assert!(decoded.get("error").is_none());
    let mut stdio = Vec::new();
    frame.write_stdio(&mut stdio).unwrap();
    assert!(stdio.len() <= crate::response::MAX_JSONRPC_FRAME_BYTES);
    #[cfg(feature = "http")]
    assert_eq!(serde_json::to_vec(frame.value()).unwrap(), frame.bytes());
}

#[cfg(all(not(feature = "cozo"), not(feature = "fastembed")))]
#[tokio::test]
async fn concern_absent_registered_snapshot_refuses_before_native_write_or_checkpoint() {
    let root = std::env::temp_dir().join(format!("mneme-mcp-concern-absent-{}", Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("absent.json");
    let mut state = SessionState::default();
    let registry = build_registry(vec![("project".into(), path.clone())], &mut state).unwrap();
    let db_id = registry.slot("project").unwrap().status().unwrap().db_id;
    let sessions = std::sync::Arc::new(Mutex::new(state));
    let cold = ColdWorkGate::new(Duration::ZERO);
    let error = call_tool_authorized(
        &registry,
        &sessions,
        &cold,
        CapabilityPolicy::new(CapabilityProfile::Curator, false),
        "concern",
        &routed(notice(binding()), "project", db_id),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("existing current database"), "{error}");
    assert!(!path.exists());
    let checkout = registry.checkout("project").unwrap();
    let store = checkout.concerns().unwrap();
    assert!(store.get_concern(&binding().key()).await.unwrap().is_none());
    drop(checkout);
    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
}
