//! Core's bounded contradiction notice is explicit; the legacy JSON array stays put.
#[cfg(not(feature = "fastembed"))]
use serde_json::Value;
use std::{
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("mneme-cli-core-notice-{}", ulid::Ulid::new()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mnemed"))
            .current_dir(&self.0)
            .env("HOME", self.0.join("home"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env_remove("MNEME_DB")
            .args(args)
            .output()
            .unwrap()
    }

    #[cfg(not(feature = "fastembed"))]
    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn core_notice_help_and_flag_validation_precede_storage() {
    let fixture = Fixture::new();
    let help = fixture.run(&["core", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--include-contradictions"));
    for extra in [
        "--include-contradictions=true",
        "--include-contradictions=false",
    ] {
        let output = fixture.run(&["--db", "absent-parent/memory.json", "core", extra]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!fixture.0.join("absent-parent").exists());
    }
    assert!(!fixture.0.join(".mneme").exists());
    assert!(!fixture.0.join("data").exists());
}

#[cfg(not(feature = "fastembed"))]
async fn populate(fixture: &Fixture, rows: usize) {
    use mneme_core::{
        BodyRef, Node, NodeId, NodeStatus, Provenance,
        ports::{Embedder, EmbeddingMetadataStore, GraphStore},
    };
    let store = mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM);
    store
        .set_embedding_fingerprint(
            &mneme_embed::HashingEmbedder::new(mneme_embed::DEFAULT_DIM).fingerprint(),
        )
        .unwrap();
    // No core bodies are needed: the notice summarizes the owner's overlay,
    // rather than only contradictions whose endpoints happen to be core.
    for id in 1..=(rows + 1) {
        let node = Node::try_new(
            NodeId(ulid::Ulid::from(id as u128)),
            format!("Claim {id}"),
            BodyRef::new("file:///unopened-test-body").unwrap(),
            ["fact"],
            Provenance::derived_empty(),
            1.0,
            1.0,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        store.put_node(&node).await.unwrap();
    }
    for id in 2..=(rows + 1) {
        store
            .observe_contradiction(
                NodeId(ulid::Ulid::from(1_u128)),
                NodeId(ulid::Ulid::from(id as u128)),
                1,
            )
            .await
            .unwrap();
    }
    store.save(fixture.0.join("memory.json")).unwrap();
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn core_notice_preserves_default_array_and_reports_complete_count_without_mutation() {
    let fixture = Fixture::new();
    populate(&fixture, 1).await;
    let before = std::fs::read(fixture.0.join("memory.json")).unwrap();
    assert_eq!(
        fixture.json(&["--db", "memory.json", "--json", "core"]),
        serde_json::json!([])
    );
    let notice = fixture.json(&[
        "--db",
        "memory.json",
        "--json",
        "core",
        "--include-contradictions",
    ]);
    assert_eq!(
        notice,
        serde_json::json!({"nodes":[],"contradiction_summary":{"status":"complete","open_found":1,"rows_scanned":1}})
    );
    let human = fixture.run(&["--db", "memory.json", "core", "--include-contradictions"]);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("contradiction"), "{text}");
    assert!(text.contains('1'), "{text}");
    assert_eq!(
        std::fs::read(fixture.0.join("memory.json")).unwrap(),
        before
    );
}

#[cfg(not(feature = "fastembed"))]
#[tokio::test]
async fn core_notice_partial_count_never_claims_a_complete_total() {
    let fixture = Fixture::new();
    populate(&fixture, 257).await;
    let notice = fixture.json(&[
        "--db",
        "memory.json",
        "--json",
        "core",
        "--include-contradictions",
    ]);
    assert_eq!(notice["nodes"], serde_json::json!([]));
    let summary = &notice["contradiction_summary"];
    assert_eq!(summary["status"], "partial");
    let scanned = summary["rows_scanned"].as_u64().unwrap();
    assert!(scanned <= 256);
    assert_eq!(summary["open_found"], scanned);
    assert!(summary.get("total").is_none());
    let human = fixture.run(&["--db", "memory.json", "core", "--include-contradictions"]);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let text = String::from_utf8_lossy(&human.stdout).to_lowercase();
    assert!(
        text.contains("partial") || text.contains("at least"),
        "{text}"
    );
}
