//! Disposable metadata-only concern owners; no bodies/models/provider calls.
use mneme_app::concern::endpoint_for_node;
use mneme_core::{
    BodyRef, ConcernBinding, ConcernKind, ConcernNotice, ConcernUpdate, Node, NodeId, NodeStatus,
    Provenance,
    ports::{Embedder, EmbeddingMetadataStore, GraphStore},
};
use mneme_cozo::MemStore;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Command, Output},
};
use ulid::Ulid;

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mneme-cli-concern-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        Self {
            root: root.canonicalize().unwrap(),
        }
    }
    fn run(&self, db: &str, raw: &Value, human: bool) -> Output {
        std::fs::write(self.root.join("request.json"), raw.to_string()).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mnemed"));
        cmd.current_dir(&self.root)
            .env_remove("MNEME_DB")
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .args(["--db", db]);
        if !human {
            cmd.arg("--json");
        }
        cmd.args(["concern", "--input", "request.json"])
            .output()
            .unwrap()
    }
    fn ok(&self, db: &str, raw: &Value) -> Value {
        let out = self.run(db, raw, false);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn no_machinery(&self, db: &str) {
        assert!(!self.root.join(format!("{db}.bodies")).exists());
        assert!(!self.root.join("bodies").exists());
        assert!(!self.root.join("memory.bodies").exists());
        assert!(!self.root.join("data").exists());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}
fn node(id: u128) -> Node {
    Node::try_new(
        NodeId(Ulid::from(id)),
        format!("Claim {id}"),
        BodyRef::new("file:///mutable-not-opened").unwrap(),
        ["fact"],
        Provenance::Conversation {
            session: Ulid::from(9),
            turn: id as u32,
        },
        1.0,
        1.0,
        NodeStatus::Active,
        1,
    )
    .unwrap()
}
async fn populated() -> (MemStore, Value) {
    let store = MemStore::new(8);
    let a = node(1);
    let b = node(2);
    store.put_node(&a).await.unwrap();
    store.put_node(&b).await.unwrap();
    let binding = ConcernBinding::new(
        ConcernKind::Disagreement,
        endpoint_for_node(&a),
        endpoint_for_node(&b),
    )
    .unwrap();
    let mut notice = serde_json::to_value(ConcernUpdate::Notice(
        ConcernNotice::new(binding, "Claims disagree", "Which context applies?").unwrap(),
    ))
    .unwrap();
    notice["expected_db_id"] = json!(store.db_id().to_string());
    (store, notice)
}
#[test]
fn early_bad_input_absent_and_guarded_mutations_do_not_create_storage() {
    let f = Fixture::new();
    for raw in [
        json!(null),
        json!({"action":"wat"}),
        json!({"action":"list","endpoint":Ulid::from(1).to_string(),"limit":0}),
        json!({"action":"notice","notice":{}}),
    ] {
        assert!(
            !f.run("missing-parent/store.db", &raw, false)
                .status
                .success()
        );
        assert!(!f.root.join("missing-parent").exists());
    }
    let list = json!({"action":"list","endpoint":Ulid::from(1).to_string()});
    assert!(!f.run("absent.db", &list, false).status.success());
    assert!(!f.root.join("absent.db").exists());
    f.no_machinery("absent.db");
    std::fs::write(f.root.join("malformed.db"), b"not a current store").unwrap();
    assert!(!f.run("malformed.db", &list, false).status.success());
    assert_eq!(
        std::fs::read(f.root.join("malformed.db")).unwrap(),
        b"not a current store"
    );
}
#[tokio::test]
async fn snapshot_exact_native_outcomes_paging_guard_lease_and_no_initialization() {
    let f = Fixture::new();
    let (store, notice) = populated().await;
    store.save(f.root.join("memory.json")).unwrap();
    let original = std::fs::read(f.root.join("memory.json")).unwrap();
    let list = json!({"action":"list","endpoint":Ulid::from(1).to_string()});
    assert_eq!(
        f.ok("memory.json", &list)["page"],
        json!({"items":[],"next":null})
    );
    assert_eq!(std::fs::read(f.root.join("memory.json")).unwrap(), original);
    let mut wrong = notice.clone();
    wrong["expected_db_id"] = json!(Ulid::new().to_string());
    assert!(!f.run("memory.json", &wrong, false).status.success());
    assert_eq!(std::fs::read(f.root.join("memory.json")).unwrap(), original);
    let mut unguarded = notice.clone();
    unguarded.as_object_mut().unwrap().remove("expected_db_id");
    assert!(!f.run("memory.json", &unguarded, false).status.success());
    let lease = mneme_store_path::StoreLease::acquire(&f.root.join("memory.json")).unwrap();
    assert!(!f.run("memory.json", &list, false).status.success());
    drop(lease);
    let applied = f.ok("memory.json", &notice);
    assert_eq!(applied["outcome"]["status"], "applied");
    assert_eq!(applied["db_id"], notice["expected_db_id"]);
    let after = std::fs::read(f.root.join("memory.json")).unwrap();
    assert_ne!(after, original);
    assert_eq!(
        f.ok("memory.json", &notice)["outcome"]["status"],
        "unchanged"
    );
    assert_eq!(std::fs::read(f.root.join("memory.json")).unwrap(), after);
    let mut stale = notice.clone();
    stale["notice"]["binding"]["endpoints"][0]["meaning"] = json!("00".repeat(32));
    assert_eq!(f.ok("memory.json", &stale)["outcome"]["status"], "refused");
    assert_eq!(std::fs::read(f.root.join("memory.json")).unwrap(), after);
    let page = f.ok("memory.json", &list);
    assert_eq!(page["page"]["items"][0], applied["outcome"]["row"]);
    let human = f.run("memory.json", &list, true);
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).contains("Claims disagree"));
    let readback = MemStore::load(f.root.join("memory.json")).unwrap();
    assert!(readback.embedding_fingerprint().unwrap().is_none());
    f.no_machinery("memory.json");
    #[cfg(not(feature = "fastembed"))]
    {
        // GET metadata comes from its actual native row, not a client recomputation.
        readback
            .set_embedding_fingerprint(&mneme_embed::HashingEmbedder::new(8).fingerprint())
            .unwrap();
        readback.save(f.root.join("memory.json")).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_mnemed"))
            .current_dir(&f.root)
            .env_remove("MNEME_DB")
            .args([
                "--db",
                "memory.json",
                "--json",
                "get",
                &Ulid::from(1).to_string(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let got: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            got["concern_endpoint"],
            serde_json::to_value(endpoint_for_node(
                &readback
                    .get_node(NodeId(Ulid::from(1)))
                    .await
                    .unwrap()
                    .unwrap()
            ))
            .unwrap()
        );
    }
}
#[cfg(feature = "cozo")]
#[tokio::test]
async fn persistent_current_concern_is_metadata_only_and_keeps_identity() {
    let f = Fixture::new();
    let (store, notice) = populated().await;
    mneme_cozo::CozoStore::materialize_fresh_current(
        &f.root.join("memory.db"),
        Ulid::new(),
        &store,
    )
    .await
    .unwrap();
    let result = f.ok("memory.db", &notice);
    assert_eq!(result["outcome"]["status"], "applied");
    assert_eq!(result["db_id"], notice["expected_db_id"]);
    assert_eq!(f.ok("memory.db", &notice)["outcome"]["status"], "unchanged");
    let lease = std::sync::Arc::new(
        mneme_store_path::StoreLease::acquire(&f.root.join("memory.db")).unwrap(),
    );
    let loaded =
        mneme_cozo::CozoStore::open_existing_persistent(&f.root.join("memory.db"), 8, lease)
            .unwrap();
    assert!(loaded.embedding_fingerprint().unwrap().is_none());
    f.no_machinery("memory.db");
}
