use std::path::Path;
use std::process::Command;

use mneme_core::ports::{EmbeddingMetadataStore, GraphStore};
use mneme_core::{BodyRef, Node, NodeId, NodeStatus, OriginCommit, Provenance};
use serde_json::{Value, json};
use ulid::Ulid;

fn get(root: &Path, id: NodeId, json: bool) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mnemed"));
    command
        .current_dir(root)
        .args(["--db", "memory.json", "get", &id.0.to_string()]);
    if json {
        command.arg("--json");
    }
    let output = command.output().expect("run CLI get");
    assert!(
        output.status.success(),
        "get failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test]
async fn get_preserves_recorded_origin_in_json_and_human_output() {
    let root = std::env::temp_dir().join(format!("mneme-cli-origin-{}", Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let store = mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM);
    // `get` performs no inference. Match the host fingerprint without loading a
    // model, then seed known stored origins independently of the test host HEAD.
    #[cfg(feature = "fastembed")]
    let fingerprint = mneme_embed::fastembed_fingerprint();
    #[cfg(not(feature = "fastembed"))]
    let fingerprint = mneme_embed::hashing_fingerprint(mneme_embed::DEFAULT_DIM);
    store.ensure_embedding_fingerprint(&fingerprint).unwrap();

    let mut cases = Vec::new();
    for origin in [
        None,
        Some("0123456789abcdef0123456789abcdef01234567"),
        Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
    ] {
        let id = NodeId(Ulid::new());
        let node = Node::try_new(
            id,
            "recorded origin fixture",
            BodyRef::new("inline://origin-fixture").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap()
        .with_origin_commit(origin.map(|value| OriginCommit::parse(value).unwrap()));
        store.put_node(&node).await.unwrap();
        cases.push((id, origin));
    }
    store.save(root.join("memory.json")).unwrap();

    for (id, origin) in cases {
        let rendered: Value = serde_json::from_str(&get(&root, id, true)).unwrap();
        assert_eq!(rendered.get("origin_commit"), Some(&json!(origin)));
        let human = get(&root, id, false);
        let expected = format!("origin-commit {}", origin.unwrap_or("(none)"));
        assert!(human.lines().any(|line| line == expected), "{human}");
    }
    std::fs::remove_dir_all(root).unwrap();
}
