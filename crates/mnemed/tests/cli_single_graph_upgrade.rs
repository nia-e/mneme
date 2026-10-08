//! Public CLI admission: discarded lifecycle aliases cannot open a store.

use mneme_core::ports::{EmbeddingMetadataStore, GraphStore, VectorIndex};
use mneme_core::{BodyRef, Node, NodeId, NodeStatus, Provenance};
use std::path::PathBuf;
use std::process::Command;
use ulid::Ulid;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("mneme-single-graph-cli-{}", Ulid::new()));
        std::fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_mnemed"))
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn retired_commands_and_flags_reject_before_database_checkout() {
    let fixture = Fixture::new();
    let source = fixture.path("absent.db");
    let source_str = source.to_str().unwrap();
    for args in [
        vec!["--db", source_str, "episode-upgrade"],
        vec!["--db", source_str, "upgrade-index"],
        vec![
            "--remote",
            "http://127.0.0.1:9",
            "--db",
            source_str,
            "upgrade-index",
        ],
        vec!["--db", source_str, "promote"],
        vec!["--db", source_str, "ingest", "--summary", "x", "--active"],
        vec!["--db", source_str, "query", "x", "--candidates"],
        vec!["--db", source_str, "list", "--status", "candidate"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(!source.exists(), "{args:?} created a store");
    }
}

#[test]
fn successor_command_is_named_and_requires_explicit_source_and_output() {
    let fixture = Fixture::new();
    let help = fixture.run(&["--help"]);
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("single-graph-upgrade"));
    assert!(!help.contains("episode-upgrade"));
    assert!(!help.contains("upgrade-index"));
    let output = fixture.run(&[
        "single-graph-upgrade",
        "--backend",
        "json",
        "--output",
        fixture.path("out.db").to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(!fixture.path("out.db").exists());
}

#[tokio::test]
async fn json_predecessor_publishes_detached_v2_without_replacing_source_or_output() {
    let fixture = Fixture::new();
    let source = fixture.path("old.json");
    let destination = fixture.path("new.json");
    let old = mneme_cozo::MemStore::new(4);
    old.set_embedding_fingerprint(&mneme_embed::hashing_fingerprint(4))
        .unwrap();
    let inline = Node::try_new(
        NodeId(Ulid::new()),
        "An inline body reference survives detached conversion",
        BodyRef::new("inline://old-inline-body").unwrap(),
        ["fixture"],
        Provenance::derived_empty(),
        0.5,
        0.6,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    old.put_node(&inline).await.unwrap();
    old.upsert(inline.id(), &[1.0, 0.0, 0.0, 0.0])
        .await
        .unwrap();
    // This fixture is deliberately historical flat v1, not a current export.
    let mut historical = serde_json::to_value(old.export()).unwrap();
    historical.as_object_mut().unwrap().remove("concerns");
    let old_bytes = serde_json::to_vec_pretty(&historical).unwrap();
    std::fs::write(&source, &old_bytes).unwrap();
    let args = [
        "--json",
        "--db",
        source.to_str().unwrap(),
        "single-graph-upgrade",
        "--backend",
        "json",
        "--output",
        destination.to_str().unwrap(),
    ];
    let first = fixture.run(&args);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(report["status"], "upgraded_copy");
    assert_eq!(report["backend"], "json");
    assert_eq!(report["generation"], "single-graph-v1");
    assert_eq!(report["target_generation"], "single-graph-v1");
    assert_eq!(report["publication_state"], "Published");
    assert_eq!(report["body_files"], 0);
    assert_eq!(report["non_fs_body_refs_retained_unfetched"], 1);
    assert_eq!(report["source_replaced"], false);
    assert_eq!(report["activated"], false);
    assert_eq!(std::fs::read(&source).unwrap(), old_bytes);
    let successor = mneme_cozo::MemStore::load_single_graph_v2(&destination).unwrap();
    assert!(mneme_cozo::MemStore::load(&destination).is_err());
    assert_eq!(successor.db_id(), old.db_id());
    assert_eq!(
        successor
            .export()
            .nodes
            .iter()
            .find(|node| node.id() == inline.id())
            .unwrap()
            .body()
            .as_str(),
        "inline://old-inline-body"
    );
    let published = std::fs::read(&destination).unwrap();
    let retry = fixture.run(&args);
    assert!(!retry.status.success());
    assert_eq!(std::fs::read(&destination).unwrap(), published);
    assert_eq!(std::fs::read(&source).unwrap(), old_bytes);
}

#[tokio::test]
async fn json_context_target_admits_only_frozen_v3_and_publishes_v4_without_source_changes() {
    let fixture = Fixture::new();
    let source = fixture.path("concern-v3.json");
    let target = fixture.path("episode-context-v4.json");
    let predecessor = mneme_cozo::MemStore::new(4);
    predecessor
        .set_embedding_fingerprint(&mneme_embed::hashing_fingerprint(4))
        .unwrap();
    let node = Node::try_new(
        NodeId(Ulid::new()),
        "A semantic lesson stays semantic through the episode metadata upgrade",
        BodyRef::new("inline://unchanged").unwrap(),
        ["fixture"],
        Provenance::derived_empty(),
        0.5,
        0.6,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    predecessor.put_node(&node).await.unwrap();
    predecessor
        .upsert(node.id(), &[1., 0., 0., 0.])
        .await
        .unwrap();
    predecessor.save_concern_v3(&source).unwrap();
    let before = std::fs::read(&source).unwrap();
    assert!(mneme_cozo::MemStore::load(&source).is_err());
    let args = [
        "--json",
        "--db",
        source.to_str().unwrap(),
        "single-graph-upgrade",
        "--backend",
        "json",
        "--target-generation",
        "episode-context-v2",
        "--output",
        target.to_str().unwrap(),
    ];
    let output = fixture.run(&args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["generation"], "episode-context-v2");
    assert_eq!(report["activated"], false);
    assert_eq!(report["source_replaced"], false);
    assert_eq!(std::fs::read(&source).unwrap(), before);
    assert!(mneme_cozo::MemStore::load_concern_v3(&target).is_err());
    assert_eq!(
        mneme_cozo::MemStore::load_episode_context_v4(&target)
            .unwrap()
            .export()
            .canonical_value()
            .unwrap(),
        predecessor.export().canonical_value().unwrap()
    );
    let published = std::fs::read(&target).unwrap();
    assert!(!fixture.run(&args).status.success());
    assert_eq!(std::fs::read(&target).unwrap(), published);
    assert_eq!(std::fs::read(&source).unwrap(), before);
    let refuse = fixture.path("already-current-refused.json");
    let current_args = [
        "--json",
        "--db",
        target.to_str().unwrap(),
        "single-graph-upgrade",
        "--backend",
        "json",
        "--target-generation",
        "episode-context-v2",
        "--output",
        refuse.to_str().unwrap(),
    ];
    assert!(!fixture.run(&current_args).status.success());
    assert!(!refuse.exists());
    assert_eq!(std::fs::read(&target).unwrap(), published);
}

#[test]
fn touchstones_json_target_is_exact_detached_and_historical_targets_stay_frozen() {
    let fixture = Fixture::new();
    let predecessor = mneme_cozo::MemStore::new(4);
    let source = fixture.path("episode-context-v4.json");
    predecessor.save_episode_context_v4(&source).unwrap();
    let before = std::fs::read(&source).unwrap();
    assert!(mneme_cozo::MemStore::load(&source).is_err());
    let destination = fixture.path("touchstones-v5.json");
    let args = [
        "--json",
        "--db",
        source.to_str().unwrap(),
        "single-graph-upgrade",
        "--backend",
        "json",
        "--target-generation",
        "touchstones-v1",
        "--output",
        destination.to_str().unwrap(),
    ];
    let result = fixture.run(&args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["generation"], "touchstones-v1");
    assert_eq!(report["activated"], false);
    assert_eq!(report["source_replaced"], false);
    assert_eq!(std::fs::read(&source).unwrap(), before);
    let upgraded = mneme_cozo::MemStore::load(&destination).unwrap();
    assert_eq!(upgraded.db_id(), predecessor.db_id());
    assert_eq!(
        upgraded.export().canonical_value().unwrap(),
        predecessor.export().canonical_value().unwrap()
    );
    assert!(mneme_cozo::MemStore::load_episode_context_v4(&destination).is_err());
    let published = std::fs::read(&destination).unwrap();
    assert!(!fixture.run(&args).status.success());
    assert_eq!(std::fs::read(&destination).unwrap(), published);
    let old = fixture.path("concern-v3.json");
    predecessor.save_concern_v3(&old).unwrap();
    let old_bytes = std::fs::read(&old).unwrap();
    let refused = fixture.path("skip-generation.json");
    let result = fixture.run(&[
        "--json",
        "--db",
        old.to_str().unwrap(),
        "single-graph-upgrade",
        "--backend",
        "json",
        "--target-generation",
        "touchstones-v1",
        "--output",
        refused.to_str().unwrap(),
    ]);
    assert!(!result.status.success());
    assert!(!refused.exists());
    assert!(!refused.with_extension("bodies").exists());
    assert_eq!(std::fs::read(&old).unwrap(), old_bytes);
    let already_current = fixture.path("already-current-refused.json");
    let result = fixture.run(&[
        "--json",
        "--db",
        destination.to_str().unwrap(),
        "single-graph-upgrade",
        "--backend",
        "json",
        "--target-generation",
        "touchstones-v1",
        "--output",
        already_current.to_str().unwrap(),
    ]);
    assert!(!result.status.success());
    assert!(!already_current.exists());
    assert_eq!(std::fs::read(&destination).unwrap(), published);
}
