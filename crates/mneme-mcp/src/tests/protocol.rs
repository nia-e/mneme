//! Discovery text and bounded JSON-RPC/stdio protocol behavior.

use super::support::{empty_profile_server, schema_accepts, tool_input_schema};
use crate::*;

#[tokio::test]
async fn discovery_previews_reach_operation_hints_without_changing_request_contracts() {
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
        assert_eq!(initialize["capabilityProfile"], profile.as_str());
        assert_eq!(
            initialize["serverInfo"]["capabilityProfile"],
            profile.as_str()
        );
        let instructions = initialize["instructions"].as_str().unwrap();
        assert!(instructions.contains(mneme_app::episode::MEMORY_TIME_GUIDANCE));
        let instructions = instructions
            .split(mneme_app::episode::MEMORY_TIME_GUIDANCE)
            .next()
            .unwrap()
            .trim();
        let tools = server.dispatch("tools/list", json!({})).await.unwrap();
        // Model the observed host's instruction + description composition,
        // not CodeMode rendering or whether an actor reads the full schema.
        for (name, hints) in [
            (
                "status",
                &[
                    "Inspect Active/Archived counts",
                    "db",
                    "Omitted db selects project",
                ][..],
            ),
            (
                "recall_context",
                &["Recall task-relevant context", "db and text", "not query"][..],
            ),
            ("get", &["Read one memory", "id", "db"][..]),
            ("save", &["Save one note", "episode", "db"][..]),
            (
                "capture",
                &["Save a sourced", "db", "structured source", "summary"][..],
            ),
            (
                "supersede",
                &["Resolve a contradiction", "db", "winner", "loser"][..],
            ),
        ] {
            let Some(tool) = tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
            else {
                continue; // A hidden operation stays hidden for this profile.
            };
            let preview: String = format!(
                "{instructions}\n\n{}",
                tool["description"].as_str().unwrap()
            )
            .chars()
            .take(180)
            .collect();
            for hint in hints {
                assert!(
                    preview.contains(hint),
                    "{} {name}: {preview}",
                    profile.as_str()
                );
            }
        }
    }

    // Prose guidance must not invent aliases or silently remap defaults.
    let schemas = unfiltered_tool_schemas();
    let status = tool_input_schema(&schemas, "status");
    assert_eq!(status["required"], json!([]));
    assert!(schema_accepts(status, &json!({})));
    assert!(schema_accepts(status, &json!({"db":"project"})));
    assert!(selected_db_name(&json!({})).is_err());
    assert_eq!(
        selected_db_name(&json!({"db":"project"})).unwrap(),
        "project"
    );
    let recall = tool_input_schema(&schemas, "recall_context");
    assert_eq!(recall["required"], json!(["text"]));
    assert!(schema_accepts(
        recall,
        &json!({"db":"project","text":"a task"})
    ));
    assert!(!schema_accepts(
        recall,
        &json!({"db":"project","query":"a task"})
    ));
    assert!(
        validate_tool_arguments("recall_context", &json!({"query":"a task"}))
            .unwrap_err()
            .to_string()
            .contains("unknown argument `query`")
    );
}

#[tokio::test]
async fn oversized_stdio_line_is_drained_without_losing_the_next_request() {
    let mut bytes = vec![b'x'; MAX_STDIO_REQUEST_BYTES + 8];
    bytes.extend_from_slice(b"\n{}\n");
    let mut reader = BufReader::new(bytes.as_slice());
    let mut line = Vec::new();
    assert_eq!(
        read_bounded_line(&mut reader, &mut line).await.unwrap(),
        Some(true)
    );
    assert!(line.is_empty());
    assert_eq!(
        read_bounded_line(&mut reader, &mut line).await.unwrap(),
        Some(false)
    );
    assert_eq!(line, b"{}\n");
}

#[test]
fn protocol_negotiation_echoes_supported_and_offers_latest_otherwise() {
    assert_eq!(
        negotiated_protocol(&json!({ "protocolVersion": "2025-06-18" })).unwrap(),
        "2025-06-18"
    );
    assert_eq!(
        negotiated_protocol(&json!({ "protocolVersion": "2024-11-05" })).unwrap(),
        PROTOCOL_VERSION
    );
    assert!(negotiated_protocol(&json!({})).is_err());
}
