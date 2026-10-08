//! Public reflection admission, evidence application, and direct-feedback schema.

use super::support::{empty_profile_server, schema_accepts, tool_input_schema};
use crate::*;

#[tokio::test]
async fn malformed_reflection_is_rejected_before_checkout_for_every_profile() {
    let id = NodeId(ulid::Ulid::new()).0.to_string();
    let unknown = NodeId(ulid::Ulid::new()).0.to_string();
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        let server = empty_profile_server(profile);
        for arguments in [
            json!({"db":"missing", "used":[]}),
            json!({"db":"missing", "receipts":[], "used":[]}),
            json!({"db":"missing", "receipts":["r"], "used":[id], "unhelpful":[id]}),
            json!({"db":"missing", "receipts":["r"], "used":[], "unhelpful":[7]}),
            json!({"db":"missing", "receipts":["r"], "used":[], "unhelpful":"no"}),
            json!({"db":"missing", "receipts":["r"], "used":vec![id.clone();64], "unhelpful":[unknown]}),
        ] {
            let response = server
                .dispatch(
                    "tools/call",
                    json!({"name":"reflect", "arguments":arguments}),
                )
                .await
                .unwrap();
            assert_eq!(response["isError"], true, "{response}");
            let error = response["content"][0]["text"].as_str().unwrap();
            assert!(!error.contains("unknown database"), "{error}");
            assert!(
                !error.contains("capability profile"),
                "malformed input must fail first: {error}"
            );
        }
    }
    for profile in [
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        let schemas = tool_schemas(CapabilityPolicy::new(profile, false));
        let schema = tool_input_schema(&schemas, "reflect");
        assert!(schema_accepts(
            schema,
            &json!({"db":"project", "receipts":["r"], "used":[], "unhelpful":[id]})
        ));
        assert!(!schema_accepts(
            schema,
            &json!({"db":"project", "receipts":[], "used":[]})
        ));
    }
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
#[tokio::test]
async fn dispatched_reflect_is_explicit_negative_and_retry_safe() {
    let root = std::env::temp_dir().join(format!("mneme-reflect-explicit-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    {
        let mut initial_sessions = SessionState::default();
        let slot = std::sync::Arc::new(
            DatabaseSlot::open(
                root.join("memory.json"),
                std::sync::Arc::new(host::InferenceRuntime::new()),
                initial_sessions.feedback_epoch("fixture"),
            )
            .unwrap(),
        );
        let handle = slot.checkout().unwrap();
        let a = handle
            .mem
            .ingest(Ingest::new("first", b"", &[], Provenance::derived_empty()))
            .await
            .unwrap();
        let b = handle
            .mem
            .ingest(Ingest::new("second", b"", &[], Provenance::derived_empty()))
            .await
            .unwrap();
        handle
            .mem
            .link(a, b, EdgeKind::Associative, 0.4, None)
            .await
            .unwrap();
        drop(handle);
        let registry = Registry {
            activity: crate::activity::ActivityRing::default(),
            dbs: BTreeMap::from([("fixture".into(), slot.clone())]),
        };
        let sessions = std::sync::Arc::new(Mutex::new(initial_sessions));
        let cold = ColdWorkGate::new(Duration::ZERO);
        for explicitly_unhelpful in [true, false] {
            let start = call_tool(
                &registry,
                &sessions,
                &cold,
                "walk",
                &json!({"db":"fixture", "action":"start", "start":a.0.to_string()}),
            )
            .await
            .unwrap();
            let session = start["session"].as_str().unwrap();
            call_tool(
                &registry,
                &sessions,
                &cold,
                "walk",
                &json!({"action":"go", "session":session, "to":b.0.to_string()}),
            )
            .await
            .unwrap();
            let done = call_tool(
                &registry,
                &sessions,
                &cold,
                "walk",
                &json!({"action":"done", "session":session}),
            )
            .await
            .unwrap();
            let unhelpful = if explicitly_unhelpful {
                vec![b.0.to_string()]
            } else {
                Vec::new()
            };
            let args = json!({"db":"fixture", "receipts":[done["receipt"]], "used":[], "unhelpful":unhelpful});
            let first = call_tool(&registry, &sessions, &cold, "reflect", &args)
                .await
                .unwrap();
            assert_eq!(first["reinforced"], 0);
            assert_eq!(first["interfered"], usize::from(explicitly_unhelpful));
            let replay = call_tool(&registry, &sessions, &cold, "reflect", &args)
                .await
                .unwrap();
            assert_eq!(replay["feedback_commit"], "already_applied");
            let handle = slot.checkout().unwrap();
            assert_eq!(
                handle
                    .mem
                    .get_node(b)
                    .await
                    .unwrap()
                    .unwrap()
                    .interference(),
                0
            );
            assert_eq!(
                handle
                    .mem
                    .get_node(a)
                    .await
                    .unwrap()
                    .unwrap()
                    .interference(),
                0
            );
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn feedback_schema_allows_node_only_candidate_evidence_when_explicitly_enabled() {
    let feedback = tool_schemas(CapabilityPolicy::operator_with_direct_feedback())
        .into_iter()
        .find(|tool| tool["name"] == "feedback")
        .expect("feedback tool schema");
    let required = feedback["inputSchema"]["required"]
        .as_array()
        .expect("required fields");
    assert!(required.contains(&json!("to")));
    assert!(required.contains(&json!("signal")));
    assert!(!required.contains(&json!("from")));
    assert_eq!(
        feedback["inputSchema"]["properties"]["from"]["description"],
        "optional route source; omit for node-only evidence"
    );
}
