//! A malformed structured packet must fail before path resolution or lease
//! creation, even when the requested database path does not yet exist.

use std::path::Path;
use std::process::Command;

use mneme_core::ports::{EmbeddingMetadataStore, GraphStore};
use mneme_core::{BodyRef, CaptureSource, Node, NodeId, NodeStatus, Provenance};
use serde_json::{Value, json};
use ulid::Ulid;

fn run(root: &Path, input: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .current_dir(root)
        .args([
            "--db",
            "would-create/store.db",
            "--json",
            "capture",
            "add",
            "--input",
            input,
        ])
        .output()
        .expect("run capture")
}

#[test]
fn malformed_capture_does_not_create_store_or_lease() {
    let root = std::env::temp_dir().join(format!("mneme-cli-capture-admission-{}", Ulid::new()));
    std::fs::create_dir(&root).unwrap();

    for (name, contents) in [
        ("malformed.json", "{"),
        (
            "wrong-type.json",
            r#"{"source":{"namespace":"codex","key":1,"reference":"codex://x"},"summary":"claim"}"#,
        ),
        (
            "null.json",
            r#"{"source":{"namespace":"codex","key":"k","reference":"codex://x"},"summary":"claim","active":null}"#,
        ),
        (
            "bad-links.json",
            r#"{"source":{"namespace":"codex","key":"k","reference":"codex://x"},"summary":"claim","links":[{"to":"not-a-ulid"}]}"#,
        ),
        ("oversize.json", &"x".repeat(1024 * 1024 + 1)),
    ] {
        std::fs::write(root.join(name), contents).unwrap();
        let output = run(&root, name);
        assert!(!output.status.success(), "{name} unexpectedly succeeded");
        assert!(
            !root.join("would-create").exists(),
            "{name} created store parent or lease"
        );
    }

    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "cozo")]
#[test]
fn capture_init_add_replay_and_shared_store_acceptance() {
    let root = std::env::temp_dir().join(format!("mneme-cli-capture-generation-{}", Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let binary = env!("CARGO_BIN_EXE_mnemed");
    let request = root.join("capture.json");
    std::fs::write(&request, r#"{"source":{"namespace":"codex","key":"claim-1","reference":"codex://session/1"},"summary":"one sourced claim"}"#).unwrap();
    let invoke = |args: &[&str]| {
        Command::new(binary)
            .current_dir(&root)
            .args(args)
            .output()
            .unwrap()
    };

    let missing_parent = invoke(&["--db", "missing/capture.db", "capture", "init"]);
    assert!(!missing_parent.status.success());
    assert!(!root.join("missing").exists());
    let user_target = invoke(&["--db", "capture.db", "--user", "capture", "init"]);
    assert!(!user_target.status.success());
    assert!(!root.join("capture.db").exists());

    let init = invoke(&["--db", "capture.db", "--json", "capture", "init"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let init_again = invoke(&["--db", "capture.db", "capture", "init"]);
    assert!(
        !init_again.status.success(),
        "init must not clobber an existing target"
    );

    let first = invoke(&[
        "--db",
        "capture.db",
        "--json",
        "capture",
        "add",
        "--input",
        "capture.json",
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["replayed"], false);
    let replay = invoke(&[
        "--db",
        "capture.db",
        "--json",
        "capture",
        "add",
        "--input",
        "capture.json",
    ]);
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    let replay: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(replay["id"], first["id"]);
    assert_eq!(replay["replayed"], true);

    let target = first["id"].as_str().unwrap();
    let linked_request = root.join("linked.json");
    std::fs::write(
        &linked_request,
        json!({
            "source": {"namespace":"codex","key":"claim-2","reference":"codex://session/1"},
            "summary":"linked sourced claim",
        "links":[{"to":target,"kind":"transition","weight":0.8}]
        })
        .to_string(),
    )
    .unwrap();
    let linked_add = || {
        invoke(&[
            "--db",
            "capture.db",
            "--json",
            "capture",
            "add",
            "--input",
            "linked.json",
        ])
    };
    let linked = linked_add();
    assert!(
        linked.status.success(),
        "{}",
        String::from_utf8_lossy(&linked.stderr)
    );
    let linked: Value = serde_json::from_slice(&linked.stdout).unwrap();
    assert_eq!(linked["replayed"], false);
    let linked_id = linked["id"].as_str().unwrap();
    let got = invoke(&["--db", "capture.db", "--json", "get", linked_id, "--edges"]);
    assert!(
        got.status.success(),
        "{}",
        String::from_utf8_lossy(&got.stderr)
    );
    let got: Value = serde_json::from_slice(&got.stdout).unwrap();
    assert!(
        got["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["neighbor"] == target),
        "{got}"
    );
    let linked_replay = linked_add();
    assert!(
        linked_replay.status.success(),
        "{}",
        String::from_utf8_lossy(&linked_replay.stderr)
    );
    let linked_replay: Value = serde_json::from_slice(&linked_replay.stdout).unwrap();
    assert_eq!(linked_replay["id"], linked["id"]);
    assert_eq!(linked_replay["replayed"], true);
    std::fs::write(
        &linked_request,
        json!({
            "source": {"namespace":"codex","key":"claim-2","reference":"codex://session/1"},
            "summary":"linked sourced claim",
        "links":[{"to":target,"kind":"transition","weight":0.9}]
        })
        .to_string(),
    )
    .unwrap();
    assert!(
        !linked_add().status.success(),
        "changed links must conflict on replay"
    );

    std::fs::write(&request, r#"{"source":{"namespace":"codex","key":"claim-1","reference":"codex://session/1"},"summary":"changed claim"}"#).unwrap();
    let conflict = invoke(&[
        "--db",
        "capture.db",
        "capture",
        "add",
        "--input",
        "capture.json",
    ]);
    assert!(
        !conflict.status.success(),
        "same source key with changed content must conflict"
    );

    let normal = invoke(&["--db", "normal.db", "--json", "list"]);
    assert!(
        normal.status.success(),
        "{}",
        String::from_utf8_lossy(&normal.stderr)
    );
    let accepted = invoke(&[
        "--db",
        "normal.db",
        "capture",
        "add",
        "--input",
        "capture.json",
    ]);
    assert!(
        accepted.status.success(),
        "an ordinary single-graph store accepts sourced capture: {}",
        String::from_utf8_lossy(&accepted.stderr),
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn external_source_is_visible_in_cli_get_json_and_human_output() {
    let root = std::env::temp_dir().join(format!("mneme-cli-capture-get-{}", Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let store = mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM);
    #[cfg(feature = "fastembed")]
    let fingerprint = mneme_embed::fastembed_fingerprint();
    #[cfg(not(feature = "fastembed"))]
    let fingerprint = mneme_embed::hashing_fingerprint(mneme_embed::DEFAULT_DIM);
    store.ensure_embedding_fingerprint(&fingerprint).unwrap();
    let id = NodeId(Ulid::new());
    let source = CaptureSource::new(
        "codex",
        "claim-1",
        "codex://thread/1",
        Some("session-1"),
        Some("rev-1"),
        [0x12; 32],
    )
    .unwrap();
    let node = Node::try_new(
        id,
        "sourced claim",
        BodyRef::new("inline://capture-get-fixture").unwrap(),
        std::iter::empty::<&str>(),
        Provenance::External { source },
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    store.put_node(&node).await.unwrap();
    store.save(root.join("memory.json")).unwrap();

    let id_text = id.0.to_string();
    let json_output = Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .current_dir(&root)
        .args(["--db", "memory.json", "--json", "get", &id_text])
        .output()
        .unwrap();
    assert!(
        json_output.status.success(),
        "{}",
        String::from_utf8_lossy(&json_output.stderr)
    );
    let value: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(value["provenance"]["type"], json!("external"));
    assert_eq!(value["provenance"]["source"]["key"], json!("claim-1"));
    assert_eq!(
        value["provenance"]["source"]["reference"],
        json!("codex://thread/1")
    );
    assert_eq!(
        value["provenance"]["source"]["request_digest_sha256"],
        json!("12".repeat(32))
    );
    assert_eq!(value["provenance"]["source"]["request_codec"], "capture_v2");

    let human = Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .current_dir(&root)
        .args(["--db", "memory.json", "get", &id_text])
        .output()
        .unwrap();
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("source      codex:claim-1"), "{text}");
    assert!(text.contains("source-ref  codex://thread/1"), "{text}");

    std::fs::remove_dir_all(root).unwrap();
}
