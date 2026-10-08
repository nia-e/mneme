//! JSON provenance, bounded text, and exact body-range presentation.

use crate::*;

fn node() -> Node {
    Node::try_new(
        NodeId(ulid::Ulid::new()),
        "telemetry",
        mneme_core::BodyRef::new("inline://telemetry").unwrap(),
        std::iter::empty::<&str>(),
        Provenance::derived_empty(),
        0.5,
        0.5,
        mneme_core::NodeStatus::Active,
        1,
    )
    .unwrap()
}

#[test]
fn node_json_names_exposure_and_grounded_use_explicitly() {
    let mut node = node();
    let initial = node_json(&node);
    assert_eq!(initial["last_exposed"], Value::Null);
    assert_eq!(initial["exposure_count"], 0);
    assert_eq!(initial["last_grounded_use"], Value::Null);
    assert_eq!(initial["grounded_use_count"], 0);
    assert!(initial.get("candidate_use_count").is_none());
    assert!(initial.get("last_activated").is_none());
    assert!(initial.get("activations").is_none());

    node.record_exposure(2);
    node.record_grounded_use(3);
    let current = node_json(&node);
    assert_eq!(current["last_exposed"], 2);
    assert_eq!(current["exposure_count"], 1);
    assert_eq!(current["last_grounded_use"], 3);
    assert_eq!(current["grounded_use_count"], 1);
    assert!(current.get("candidate_use_count").is_none());

    node.set_status(mneme_core::NodeStatus::Active);
    let active = node_json(&node);
    assert!(active.get("candidate_use_count").is_none());
    assert_eq!(active["grounded_use_count"], 1);
}

#[test]
fn get_json_exposes_exact_external_capture_source() {
    let source = mneme_core::CaptureSource::new(
        "codex",
        "claim-1",
        "codex://thread/1",
        Some("session-1"),
        Some("rev-1"),
        [0x12; 32],
    )
    .unwrap();
    let node = Node::try_new(
        NodeId(ulid::Ulid::new()),
        "sourced claim",
        mneme_core::BodyRef::new("inline://capture-get-fixture").unwrap(),
        std::iter::empty::<&str>(),
        Provenance::External { source },
        0.5,
        0.5,
        mneme_core::NodeStatus::Active,
        1,
    )
    .unwrap();
    let rendered = node_json(&node);
    assert_eq!(rendered["provenance"]["type"], "external");
    assert_eq!(rendered["provenance"]["source"]["namespace"], "codex");
    assert_eq!(rendered["provenance"]["source"]["key"], "claim-1");
    assert_eq!(
        rendered["provenance"]["source"]["reference"],
        "codex://thread/1"
    );
    assert_eq!(rendered["provenance"]["source"]["session"], "session-1");
    assert_eq!(rendered["provenance"]["source"]["revision"], "rev-1");
    assert_eq!(
        rendered["provenance"]["source"]["request_digest_sha256"],
        "12".repeat(32)
    );
    assert_eq!(
        rendered["provenance"]["source"]["request_codec"],
        "capture_v2"
    );
}

#[test]
fn body_renderer_preserves_valid_utf8_and_never_amplifies_malformed_bytes() {
    let valid = "hello, λ — 💾\0";
    assert_eq!(render_body_bytes(valid.as_bytes()), valid);

    let invalid = [0xff; 32];
    assert_eq!(render_body_bytes(&invalid), "?".repeat(invalid.len()));

    let scalar = "💩!".as_bytes();
    assert_eq!(render_body_bytes(&scalar[..1]), "?");
    assert_eq!(render_body_bytes(&scalar[..2]), "?");
    assert_eq!(render_body_bytes(&scalar[1..]), "???!");
    assert_eq!(render_body_bytes(&[b'A', 0xf0, 0x9f, 0x92]), "A?");
    assert_eq!(render_body_bytes(&[0xe2, 0x82, b'A']), "?A");

    for cap in 1..=2 {
        let mut offset = 0;
        while offset < scalar.len() {
            let source_end = (offset + cap).min(scalar.len());
            let rendered = render_body_bytes(&scalar[offset..source_end]);
            assert!(rendered.len() <= source_end - offset);
            assert!(source_end > offset, "a nonempty bounded read must progress");
            offset = source_end;
        }
        assert_eq!(offset, scalar.len());
    }
}

#[test]
fn summary_bound_is_exact_and_never_splits_utf8() {
    let exact = "é".repeat(MAX_SUMMARY_BYTES / "é".len());
    assert_eq!(exact.len(), MAX_SUMMARY_BYTES);
    assert_eq!(
        bounded_summary(&exact, MAX_SUMMARY_BYTES),
        (exact.as_str(), false)
    );

    let crosses_boundary = format!("{}é", "x".repeat(MAX_SUMMARY_BYTES - 1));
    let (visible, truncated) = bounded_summary(&crosses_boundary, MAX_SUMMARY_BYTES);
    assert_eq!(visible.len(), MAX_SUMMARY_BYTES - 1);
    assert!(truncated);
    assert_eq!(visible, "x".repeat(MAX_SUMMARY_BYTES - 1));

    assert_eq!(bounded_summary("💩", 1), ("", true));
    assert_eq!(bounded_summary("💩", 2), ("", true));
    assert_eq!(bounded_summary("💩", 4), ("💩", false));
    assert_eq!(bounded_summary("a💩", 2), ("a", true));
}

#[test]
fn body_range_limits_are_validated() {
    assert_eq!(body_limit(&json!({})).unwrap(), DEFAULT_BODY_BYTES);
    assert_eq!(
        body_limit(&json!({ "max_body_bytes": MAX_BODY_BYTES })).unwrap(),
        MAX_BODY_BYTES
    );
    assert!(body_limit(&json!({ "max_body_bytes": 0 })).is_err());
    assert!(body_limit(&json!({ "max_body_bytes": MAX_BODY_BYTES + 1 })).is_err());
    assert!(body_limit(&json!({ "max_body_bytes": "64" })).is_err());
    assert_eq!(body_offset(&json!({})).unwrap(), 0);
    assert_eq!(body_offset(&json!({ "body_offset": 42 })).unwrap(), 42);
    assert!(body_offset(&json!({ "body_offset": -1 })).is_err());
}

#[test]
fn nested_walk_summaries_are_bounded() {
    let crosses_boundary = format!("{}é", "x".repeat(MAX_SUMMARY_BYTES - 1));
    let exact = "é".repeat(MAX_SUMMARY_BYTES / "é".len());
    let mut value = json!({
        "summary": crosses_boundary,
        "edges": [
            { "summary": exact },
            { "nested": { "summary": "💩" } },
        ],
    });
    bound_summary_fields(&mut value);
    assert_eq!(
        value["summary"].as_str().unwrap().len(),
        MAX_SUMMARY_BYTES - 1
    );
    assert_eq!(value["summary_truncated"], true);
    assert_eq!(value["edges"][0]["summary_truncated"], false);
    assert_eq!(
        value["edges"][0]["summary"].as_str().unwrap().len(),
        MAX_SUMMARY_BYTES
    );
    assert_eq!(value["edges"][1]["nested"]["summary"], "💩");
    assert_eq!(value["edges"][1]["nested"]["summary_truncated"], false);
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
#[tokio::test]
async fn binary_body_ranges_keep_exact_source_progress_and_core_allowance() {
    let root = std::env::temp_dir().join(format!("mneme-binary-body-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();

    {
        let path = root.join("memory.json");
        let slot = std::sync::Arc::new(
            DatabaseSlot::open(
                path,
                std::sync::Arc::new(host::InferenceRuntime::new()),
                ulid::Ulid::new().to_string(),
            )
            .unwrap(),
        );
        let handle = slot.checkout().unwrap();
        let ranged_body = "💩!".as_bytes();
        let ranged_id = handle
            .mem
            .ingest(Ingest::new(
                "binary range fixture",
                ranged_body,
                &[],
                Provenance::derived_empty(),
            ))
            .await
            .unwrap();

        let invalid_core_body = vec![0xff; MAX_CORE_BODY_BYTES];
        for summary in ["binary core one", "binary core two"] {
            handle
                .mem
                .ingest(Ingest::new(
                    summary,
                    &invalid_core_body,
                    &["core"],
                    Provenance::derived_empty(),
                ))
                .await
                .unwrap();
        }
        drop(handle);

        let registry = Registry {
            activity: crate::activity::ActivityRing::default(),
            dbs: BTreeMap::from([("fixture".into(), slot)]),
        };
        let sessions = std::sync::Arc::new(Mutex::new(SessionState::default()));
        let cold_work = ColdWorkGate::new(Duration::ZERO);

        for (offset, cap, expected, source_end, next_offset) in [
            (0_u64, 1_usize, "?", 1_u64, Some(1_u64)),
            (0, 2, "?", 2, Some(2)),
            (1, 2, "??", 3, Some(3)),
            (4, 1, "!", 5, None),
        ] {
            let value = call_tool(
                &registry,
                &sessions,
                &cold_work,
                "get",
                &json!({
                    "db": "fixture",
                    "id": ranged_id.0.to_string(),
                    "body": true,
                    "body_offset": offset,
                    "max_body_bytes": cap,
                }),
            )
            .await
            .unwrap();
            assert_eq!(value["body"], expected);
            assert_eq!(value["body_range"]["source_start"], offset);
            assert_eq!(value["body_range"]["source_end"], source_end);
            assert_eq!(value["body_range"]["next_offset"], json!(next_offset));
            assert!(value["body"].as_str().unwrap().len() <= cap);
        }

        let value = call_tool(
            &registry,
            &sessions,
            &cold_work,
            "core",
            &json!({
                "db": "fixture",
                "max_body_bytes": MAX_BODY_BYTES,
            }),
        )
        .await
        .unwrap();
        let nodes = value["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 2);
        let rendered_total: usize = nodes
            .iter()
            .map(|node| node["body"].as_str().unwrap().len())
            .sum();
        assert_eq!(rendered_total, MAX_CORE_BODY_BYTES);
        assert!(nodes.iter().all(|node| {
            node["body"]
                .as_str()
                .is_some_and(|body| body.bytes().all(|byte| byte == b'?'))
        }));
        assert_eq!(value["bodies_truncated"], true);
    }

    std::fs::remove_dir_all(root).unwrap();
}
