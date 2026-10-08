use super::*;
use mneme_core::ports::{CaptureCommitOutcome, CapturePriorBudget, MAX_CAPTURE_PRIOR_CANDIDATES};
use mneme_core::{CaptureReplayProof, CaptureRequestCodec, CaptureSource};

/// Fresh current stores include capture and episode facets; no marker rewrite.
fn capture_kernel_store() -> CozoStore {
    CozoStore::new(4).unwrap()
}

// These fresh-v2 fixtures never exercise a legacy digest. The synthetic legacy
// values are deliberately inert, not reconstructed from a stored/edited node.
fn fresh_proof(source: &CaptureSource) -> CaptureReplayProof {
    CaptureReplayProof::semantic(source.clone(), [0; 32], [1; 32]).unwrap()
}

fn fresh_node_proof(incoming: &Node) -> CaptureReplayProof {
    let Provenance::External { source } = incoming.provenance() else {
        panic!("capture fixture needs incoming external provenance")
    };
    fresh_proof(source)
}

fn source(key: &str, digest: u8) -> CaptureSource {
    CaptureSource::new(
        "codex-capture-test",
        key,
        "file:///synthetic/capture.txt",
        Some("test-session"),
        Some("test-revision"),
        [digest; 32],
    )
    .unwrap()
}

fn captured_node(source: CaptureSource) -> Node {
    Node::try_new(
        source.node_id(),
        "A reusable captured fact about the lantern workshop",
        BodyRef::new("inline://synthetic-capture").unwrap(),
        ["lantern", "workshop"],
        Provenance::External { source },
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap()
}

const EMBEDDING: [f32; 4] = [1.0, 0.0, 0.0, 0.0];

struct FreshCaptureFixture {
    root: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl FreshCaptureFixture {
    async fn new() -> Self {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("mneme-capture-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = root.join("memory.db");
        let source = crate::MemStore::new(4);
        CozoStore::materialize_fresh_current(&path, Ulid::new(), &source)
            .await
            .unwrap();
        Self { root, path }
    }

    fn open(&self) -> CozoStore {
        reopen_current_test_store(&self.path).unwrap()
    }
}

impl Drop for FreshCaptureFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn projection_count(store: &CozoStore, relation: &str, id: NodeId) -> usize {
    let mut params = BTreeMap::new();
    params.insert("id".into(), dv_str(&id.0.to_string()));
    let query = match relation {
        "node" => "?[id] := *node{id}, id == $id".to_owned(),
        "node_vec" => "?[id] := *node_vec{id}, id == $id".to_owned(),
        "node_search" => "?[id] := *node_search{id}, id == $id".to_owned(),
        "node_tag_v2" => {
            "?[tag] := *node_tag_v2:by_id{id, tag, status, sample_hash}, id == $id".to_owned()
        }
        _ => panic!("unexpected relation"),
    };
    store.run(&query, params, false).unwrap().rows.len()
}

fn assert_capture_absent(store: &CozoStore, id: NodeId) {
    for relation in ["node", "node_vec", "node_search", "node_tag_v2"] {
        assert_eq!(
            projection_count(store, relation, id),
            0,
            "{relation} leaked"
        );
    }
}

fn declared_edge(from: NodeId, to: NodeId) -> Edge {
    Edge::new(from, to, 0.7, mneme_core::EdgeKind::DerivedFrom, 1)
}

#[tokio::test]
async fn initial_edges_are_atomic_and_replay_does_not_restore_them() {
    let store = capture_kernel_store();
    let target = active_node(NodeId(Ulid::new()));
    let target2 = active_node(NodeId(Ulid::new()));
    store.put_node(&target).await.unwrap();
    store.put_node(&target2).await.unwrap();
    let node = captured_node(source("initial-edges", 61));
    let edge = declared_edge(node.id(), target.id());
    let edge2 = declared_edge(node.id(), target2.id());
    super::super::capture::arm_capture_failure(node.id(), 5);
    assert!(
        store
            .commit_capture_with_edges(
                &node,
                &EMBEDDING,
                &[edge.clone(), edge2.clone()],
                &fresh_node_proof(&node)
            )
            .await
            .is_err()
    );
    assert_capture_absent(&store, node.id());
    assert!(
        store
            .get_edge(node.id(), target.id())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_edge(node.id(), target2.id())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .commit_capture_with_edges(
                &node,
                &EMBEDDING,
                &[edge.clone(), edge2.clone()],
                &fresh_node_proof(&node)
            )
            .await
            .unwrap(),
        CaptureCommitOutcome::Applied
    );
    assert!(
        store
            .get_edge(node.id(), target.id())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .get_edge(node.id(), target2.id())
            .await
            .unwrap()
            .is_some()
    );
    store.delete_edge(node.id(), target.id()).await.unwrap();
    store.delete_node(target.id()).await.unwrap();
    assert_eq!(
        store
            .commit_capture_with_edges(&node, &EMBEDDING, &[edge, edge2], &fresh_node_proof(&node))
            .await
            .unwrap(),
        CaptureCommitOutcome::AlreadyApplied
    );
    assert!(
        store
            .get_edge(node.id(), target.id())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn initial_edge_missing_target_rejects_whole_capture() {
    let store = capture_kernel_store();
    let node = captured_node(source("missing-target", 62));
    let edge = declared_edge(node.id(), NodeId(Ulid::new()));
    assert!(matches!(
        store
            .commit_capture_with_edges(&node, &EMBEDDING, &[edge], &fresh_node_proof(&node))
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert_capture_absent(&store, node.id());
}

#[tokio::test]
async fn initial_edge_shape_and_conflicting_key_reject_before_capture() {
    let store = capture_kernel_store();
    let target = active_node(NodeId(Ulid::new()));
    store.put_node(&target).await.unwrap();
    let node = captured_node(source("edge-shape", 63));
    let edge = declared_edge(node.id(), target.id());
    assert!(matches!(
        store
            .commit_capture_with_edges(
                &node,
                &EMBEDDING,
                &[edge.clone(), edge.clone()],
                &fresh_node_proof(&node)
            )
            .await,
        Err(Error::InvalidInput(_))
    ));
    let too_many = (0..=mneme_core::ports::MAX_CAPTURE_EDGES)
        .map(|_| edge.clone())
        .collect::<Vec<_>>();
    assert!(matches!(
        store
            .commit_capture_with_edges(&node, &EMBEDDING, &too_many, &fresh_node_proof(&node))
            .await,
        Err(Error::CapacityExceeded {
            resource: "capture edges",
            ..
        })
    ));
    store.put_edge(&edge).await.unwrap();
    assert!(matches!(
        store
            .commit_capture_with_edges(&node, &EMBEDDING, &[edge], &fresh_node_proof(&node))
            .await,
        Err(Error::Conflict(_))
    ));
    assert_capture_absent(&store, node.id());
}

#[tokio::test]
async fn initial_edge_persists_and_replay_keeps_deleted_edge_after_restart() {
    let fixture = FreshCaptureFixture::new().await;
    let target = active_node(NodeId(Ulid::new()));
    let node = captured_node(source("edge-restart", 64));
    let edge = declared_edge(node.id(), target.id());
    {
        let store = fixture.open();
        store.put_node(&target).await.unwrap();
        assert_eq!(
            store
                .commit_capture_with_edges(
                    &node,
                    &EMBEDDING,
                    &[edge.clone()],
                    &fresh_node_proof(&node)
                )
                .await
                .unwrap(),
            CaptureCommitOutcome::Applied
        );
    }
    {
        let store = fixture.open();
        assert!(
            store
                .get_edge(node.id(), target.id())
                .await
                .unwrap()
                .is_some()
        );
        store.delete_edge(node.id(), target.id()).await.unwrap();
    }
    let store = fixture.open();
    assert_eq!(
        store
            .commit_capture_with_edges(&node, &EMBEDDING, &[edge], &fresh_node_proof(&node))
            .await
            .unwrap(),
        CaptureCommitOutcome::AlreadyApplied
    );
    assert!(
        store
            .get_edge(node.id(), target.id())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn predecessor_store_refuses_capture_before_lookup_or_commit() {
    let store = legacy_store(4).unwrap();
    let source = source("ordinary-refusal", 7);
    let node = captured_node(source.clone());
    let lookup = store
        .lookup_capture(&fresh_proof(&source))
        .await
        .unwrap_err();
    let commit = store
        .commit_capture(&node, &EMBEDDING, &fresh_node_proof(&node))
        .await
        .unwrap_err();
    for error in [lookup, commit] {
        assert!(
            matches!(&error, Error::InvalidInput(message) if message.contains("capture-enabled")),
            "{error}"
        );
    }
    assert!(store.get_node(node.id()).await.unwrap().is_none());
}

#[tokio::test]
async fn predecessor_put_node_cannot_bypass_capture_generation() {
    let node = captured_node(source("ordinary-put", 9));
    let ordinary = legacy_store(4).unwrap();
    assert!(matches!(
        ordinary.put_node(&node).await,
        Err(Error::InvalidInput(message)) if message.contains("capture-enabled")
    ));
    assert!(ordinary.get_node(node.id()).await.unwrap().is_none());

    // The generation fence does not disable legitimate full-node curation in
    // capture stores. First insertion still uses commit_capture in the product.
    let capture = capture_kernel_store();
    capture.put_node(&node).await.unwrap();
    assert!(capture.get_node(node.id()).await.unwrap().is_some());
}

#[tokio::test]
async fn invalid_vector_preflight_never_mutates_capture_store() {
    let store = CozoStore::new(4).unwrap();
    let source = source("bad-vector", 8);
    let node = captured_node(source);
    let error = store
        .commit_capture(&node, &[1.0, 0.0], &fresh_node_proof(&node))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::DimMismatch { .. }));
    assert!(store.get_node(node.id()).await.unwrap().is_none());
}

#[tokio::test]
async fn capture_commit_is_atomic_at_every_projection_boundary() {
    let store = capture_kernel_store();
    for step in 1..=4 {
        let source = source(&format!("injected-step-{step}"), step as u8);
        let node = captured_node(source.clone());
        super::super::capture::arm_capture_failure(node.id(), step);
        let error = store
            .commit_capture(&node, &EMBEDDING, &fresh_node_proof(&node))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("injected capture failure"));
        assert_capture_absent(&store, node.id());
        assert_eq!(
            store.lookup_capture(&fresh_proof(&source)).await.unwrap(),
            None
        );
        assert_eq!(
            store
                .commit_capture(&node, &EMBEDDING, &fresh_node_proof(&node))
                .await
                .unwrap(),
            CaptureCommitOutcome::Applied
        );
        assert_eq!(
            store.lookup_capture(&fresh_proof(&source)).await.unwrap(),
            Some(node.id())
        );
    }
}

#[tokio::test]
async fn exact_replay_is_noop_but_changed_digest_and_partial_projection_conflict() {
    let store = capture_kernel_store();
    let capture_source = source("same-key", 20);
    let node = captured_node(capture_source.clone());
    assert_eq!(
        store
            .commit_capture(&node, &EMBEDDING, &fresh_node_proof(&node))
            .await
            .unwrap(),
        CaptureCommitOutcome::Applied
    );
    assert_eq!(
        store
            .commit_capture(&node, &EMBEDDING, &fresh_node_proof(&node))
            .await
            .unwrap(),
        CaptureCommitOutcome::AlreadyApplied
    );
    let changed = captured_node(source("same-key", 21));
    assert_eq!(changed.id(), node.id());
    assert!(matches!(
        store
            .commit_capture(&changed, &EMBEDDING, &fresh_node_proof(&changed))
            .await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        store
            .lookup_capture(&fresh_proof(&source("same-key", 21)))
            .await,
        Err(Error::Conflict(_))
    ));
    store.remove(node.id()).await.unwrap();
    assert!(matches!(
        store.lookup_capture(&fresh_proof(&capture_source)).await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        store
            .commit_capture(&node, &EMBEDDING, &fresh_node_proof(&node))
            .await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(projection_count(&store, "node", node.id()), 1);
    assert_eq!(projection_count(&store, "node_vec", node.id()), 0);
}

#[tokio::test]
async fn foreign_identity_and_wrong_derived_id_never_overwrite() {
    let store = capture_kernel_store();
    let capture_source = source("foreign-identity", 25);
    let id = capture_source.node_id();
    let foreign = active_node(id);
    store.put_node(&foreign).await.unwrap();
    let capture = captured_node(capture_source.clone());
    assert!(matches!(
        store.lookup_capture(&fresh_proof(&capture_source)).await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        store
            .commit_capture(&capture, &EMBEDDING, &fresh_node_proof(&capture))
            .await,
        Err(Error::Conflict(_))
    ));
    let retained = store.get_node(id).await.unwrap().unwrap();
    assert_eq!(retained.summary(), foreign.summary());
    assert_eq!(retained.provenance(), foreign.provenance());

    let bad_id = NodeId(Ulid::new());
    let wrong_id = Node::try_new(
        bad_id,
        "wrong deterministic id",
        BodyRef::new("inline://synthetic-capture").unwrap(),
        ["lantern"],
        Provenance::External {
            source: source("wrong-id", 26),
        },
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    assert!(matches!(
        store
            .commit_capture(&wrong_id, &EMBEDDING, &fresh_node_proof(&wrong_id))
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert!(store.get_node(bad_id).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn published_capture_store_survives_reopen_and_lost_ack_replay() {
    let fixture = FreshCaptureFixture::new().await;
    let capture_source = source("persistent-replay", 30);
    let node = captured_node(capture_source.clone());
    {
        let store = fixture.open();
        assert_eq!(
            store
                .lookup_capture(&fresh_proof(&capture_source))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .commit_capture(&node, &EMBEDDING, &fresh_node_proof(&node))
                .await
                .unwrap(),
            CaptureCommitOutcome::Applied
        );
    }
    let reopened = fixture.open();
    assert_eq!(
        reopened
            .lookup_capture(&fresh_proof(&capture_source))
            .await
            .unwrap(),
        Some(node.id())
    );
    assert_eq!(
        reopened
            .commit_capture(&node, &EMBEDDING, &fresh_node_proof(&node))
            .await
            .unwrap(),
        CaptureCommitOutcome::AlreadyApplied
    );
    assert_eq!(projection_count(&reopened, "node", node.id()), 1);
    assert_eq!(projection_count(&reopened, "node_vec", node.id()), 1);
    assert_eq!(projection_count(&reopened, "node_search", node.id()), 1);
    assert_eq!(projection_count(&reopened, "node_tag_v2", node.id()), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_persistent_handles_cannot_overwrite_the_same_source_key() {
    let fixture = FreshCaptureFixture::new().await;
    let lease = Arc::new(mneme_store_path::StoreLease::acquire(&fixture.path).unwrap());
    let first =
        Arc::new(CozoStore::open_leased_current(&fixture.path, Arc::clone(&lease)).unwrap());
    let second = Arc::new(CozoStore::open_leased_current(&fixture.path, lease).unwrap());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for (store, digest) in [(first.clone(), 40), (second.clone(), 41)] {
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let node = captured_node(source("racing-key", digest));
            barrier.wait();
            store
                .commit_capture(&node, &EMBEDDING, &fresh_node_proof(&node))
                .await
        }));
    }
    let outcomes = [
        tasks.remove(0).await.unwrap(),
        tasks.remove(0).await.unwrap(),
    ];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Ok(CaptureCommitOutcome::Applied)))
            .count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Err(Error::Conflict(_))))
            .count(),
        1,
        "{outcomes:?}"
    );
    let id = source("racing-key", 40).node_id();
    assert_eq!(projection_count(&first, "node", id), 1);
    assert_eq!(projection_count(&first, "node_vec", id), 1);
    assert_eq!(projection_count(&first, "node_search", id), 1);
    assert_eq!(projection_count(&first, "node_tag_v2", id), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_waiter_keeps_backend_authority_until_commit_then_replays() {
    let fixture = FreshCaptureFixture::new().await;
    let store = std::sync::Arc::new(fixture.open());
    let capture_source = source("cancelled-waiter", 50);
    let node = captured_node(capture_source.clone());
    super::super::capture::arm_capture_pause(node.id());
    let worker_store = store.clone();
    let worker_node = node.clone();
    let waiter = tokio::spawn(async move {
        worker_store
            .commit_capture(&worker_node, &EMBEDDING, &fresh_node_proof(&worker_node))
            .await
    });
    tokio::task::spawn_blocking(super::super::capture::wait_for_capture_pause)
        .await
        .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_eq!(store.backend_activity().in_flight(), 1);
    super::super::capture::release_capture_pause();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while store.backend_activity().in_flight() != 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(store.backend_activity().in_flight(), 0);
    assert_eq!(
        store
            .lookup_capture(&fresh_proof(&capture_source))
            .await
            .unwrap(),
        Some(node.id()),
        "cancelled waiter must not cancel the backend commit"
    );
    assert_eq!(
        store
            .commit_capture(&node, &EMBEDDING, &fresh_node_proof(&node))
            .await
            .unwrap(),
        CaptureCommitOutcome::AlreadyApplied
    );
}

#[tokio::test]
async fn capture_proof_must_bind_the_exact_incoming_source_before_writes() {
    let store = capture_kernel_store();
    let node = captured_node(source("proof-bound", 70));
    let wrong = fresh_proof(&source("different-key", 70));
    assert!(matches!(
        store.commit_capture(&node, &EMBEDDING, &wrong).await,
        Err(Error::InvalidInput(_))
    ));
    assert_capture_absent(&store, node.id());
    let wrong_digest = fresh_proof(&source("proof-bound", 71));
    assert!(matches!(
        store.commit_capture(&node, &EMBEDDING, &wrong_digest).await,
        Err(Error::InvalidInput(_))
    ));
    assert_capture_absent(&store, node.id());
}

#[tokio::test]
async fn original_legacy_proof_replays_after_current_node_curation_without_rewriting_it() {
    // Kernel replay test for already-imported source metadata, not a claim of
    // predecessor catalog migration. The literal digests represent three
    // distinct incoming encodings; encoder golden tests own their derivation.
    for legacy_digest in [[80; 32], [81; 32]] {
        let store = capture_kernel_store();
        let incoming_source = source("legacy-proof", 82);
        let proof =
            CaptureReplayProof::semantic(incoming_source.clone(), [80; 32], [81; 32]).unwrap();
        let legacy_source = CaptureSource::new_with_codec(
            incoming_source.namespace(),
            incoming_source.key(),
            incoming_source.reference(),
            incoming_source.session(),
            incoming_source.revision(),
            legacy_digest,
            CaptureRequestCodec::CaptureV1,
        )
        .unwrap();
        let mut stored = captured_node(legacy_source);
        stored.set_status(NodeStatus::Archived);
        stored.try_set_confidence(0.9).unwrap();
        store.put_node(&stored).await.unwrap();
        store.upsert(stored.id(), &EMBEDDING).await.unwrap();
        let before = serde_json::to_value(&stored).unwrap();
        let incoming = captured_node(incoming_source.clone());
        assert_eq!(
            store.lookup_capture(&proof).await.unwrap(),
            Some(stored.id())
        );
        assert_eq!(
            store
                .commit_capture(&incoming, &EMBEDDING, &proof)
                .await
                .unwrap(),
            CaptureCommitOutcome::AlreadyApplied
        );
        assert_eq!(
            serde_json::to_value(store.get_node(stored.id()).await.unwrap().unwrap()).unwrap(),
            before
        );
        assert_eq!(raw_node_vector_status(&store, stored.id()), "archived");
        // A changed original request must not gain authority from the edited
        // stored confidence/status or simply from a matching source key.
        let changed_source = source("legacy-proof", 83);
        let changed_proof =
            CaptureReplayProof::semantic(changed_source.clone(), [84; 32], [85; 32]).unwrap();
        assert!(matches!(
            store.lookup_capture(&changed_proof).await,
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            store
                .commit_capture(&captured_node(changed_source), &EMBEDDING, &changed_proof)
                .await,
            Err(Error::Conflict(_))
        ));
        assert_eq!(
            serde_json::to_value(store.get_node(stored.id()).await.unwrap().unwrap()).unwrap(),
            before
        );
    }
}

fn optional_prior(from: NodeId, target: &Node) -> mneme_core::ports::CaptureSimilarityPrior {
    mneme_core::ports::CaptureSimilarityPrior::new(
        Edge::new(from, target.id(), 0.8, mneme_core::EdgeKind::Associative, 1),
        target,
    )
    .unwrap()
}

async fn fill_incident(store: &dyn GraphStore, target: NodeId, count: usize) {
    for _ in 0..count {
        let other = active_node(NodeId(Ulid::new()));
        store.put_node(&other).await.unwrap();
        store
            .put_edge(&declared_edge(target, other.id()))
            .await
            .unwrap();
    }
}

async fn optional_prior_contract(store: &dyn GraphStore) {
    for (index, disposition) in ["kept", "changed", "deleted", "archived", "full"]
        .into_iter()
        .enumerate()
    {
        let target = active_node(NodeId(Ulid::new()));
        store.put_node(&target).await.unwrap();
        let node = captured_node(source(&format!("optional-{disposition}"), 80 + index as u8));
        let prior = optional_prior(node.id(), &target);
        match disposition {
            "changed" => store
                .put_node(&tagged_node(target.id(), &["changed-meaning"]))
                .await
                .unwrap(),
            "deleted" => store.delete_node(target.id()).await.unwrap(),
            "archived" => {
                let mut archived = target.clone();
                archived.set_status(NodeStatus::Archived);
                store.put_node(&archived).await.unwrap();
            }
            "full" => fill_incident(store, target.id(), MAX_INCIDENT_EDGES).await,
            _ => (),
        }
        let proof = fresh_node_proof(&node);
        assert_eq!(
            store
                .commit_capture_with_priors(
                    &node,
                    &EMBEDDING,
                    &[],
                    &[prior.clone()],
                    CapturePriorBudget::new(1),
                    &proof
                )
                .await
                .unwrap(),
            CaptureCommitOutcome::Applied
        );
        assert_eq!(
            store
                .get_edge(node.id(), target.id())
                .await
                .unwrap()
                .is_some(),
            disposition == "kept"
        );
        store.delete_edge(node.id(), target.id()).await.unwrap();
        assert_eq!(
            store
                .commit_capture_with_priors(
                    &node,
                    &EMBEDDING,
                    &[],
                    &[prior],
                    CapturePriorBudget::new(0),
                    &proof
                )
                .await
                .unwrap(),
            CaptureCommitOutcome::AlreadyApplied
        );
        assert!(
            store
                .get_edge(node.id(), target.id())
                .await
                .unwrap()
                .is_none()
        );
    }
    let target = active_node(NodeId(Ulid::new()));
    store.put_node(&target).await.unwrap();
    let node = captured_node(source("optional-authored-wins", 90));
    let authored = declared_edge(node.id(), target.id());
    let prior = optional_prior(node.id(), &target);
    store
        .commit_capture_with_priors(
            &node,
            &EMBEDDING,
            &[authored],
            &[prior.clone()],
            CapturePriorBudget::new(1),
            &fresh_node_proof(&node),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .get_edge(node.id(), target.id())
            .await
            .unwrap()
            .unwrap()
            .kind,
        mneme_core::EdgeKind::DerivedFrom
    );
    let bad = captured_node(source("optional-bad-required", 91));
    let missing = declared_edge(bad.id(), NodeId(Ulid::new()));
    assert!(
        store
            .commit_capture_with_priors(
                &bad,
                &EMBEDDING,
                &[missing],
                &[optional_prior(bad.id(), &target)],
                CapturePriorBudget::new(1),
                &fresh_node_proof(&bad)
            )
            .await
            .is_err()
    );
    assert!(store.get_node(bad.id()).await.unwrap().is_none());

    // Required links retain their own semantics; optional priors consume only
    // remaining first-write slots; there is no semantic-degree admission ceiling.
    let node = captured_node(source("optional-slots", 92));
    let mut authored = Vec::new();
    for _ in 0..8 {
        let target = active_node(NodeId(Ulid::new()));
        store.put_node(&target).await.unwrap();
        authored.push(declared_edge(node.id(), target.id()));
    }
    store
        .commit_capture_with_priors(
            &node,
            &EMBEDDING,
            &authored,
            &[optional_prior(node.id(), &target)],
            CapturePriorBudget::new(2),
            &fresh_node_proof(&node),
        )
        .await
        .unwrap();
    assert!(
        store
            .get_edge(node.id(), target.id())
            .await
            .unwrap()
            .is_none()
    );
    for edge in &authored {
        assert!(store.get_edge(node.id(), edge.to).await.unwrap().is_some());
    }
}

#[tokio::test]
async fn optional_capture_priors_have_mem_cozo_parity() {
    optional_prior_contract(&crate::MemStore::new(4)).await;
    optional_prior_contract(&capture_kernel_store()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn optional_capture_priors_are_cap_safe_between_backend_handles() {
    let fixture = FreshCaptureFixture::new().await;
    let lease = Arc::new(mneme_store_path::StoreLease::acquire(&fixture.path).unwrap());
    let left = CozoStore::open_leased_current(&fixture.path, Arc::clone(&lease)).unwrap();
    let right = CozoStore::open_leased_current(&fixture.path, lease).unwrap();
    let target = active_node(NodeId(Ulid::new()));
    left.put_node(&target).await.unwrap();
    fill_incident(&left, target.id(), MAX_INCIDENT_EDGES - 1).await;
    let a = captured_node(source("optional-concurrent-a", 93));
    let b = captured_node(source("optional-concurrent-b", 94));
    let ap = optional_prior(a.id(), &target);
    let bp = optional_prior(b.id(), &target);
    let aproof = fresh_node_proof(&a);
    let bproof = fresh_node_proof(&b);
    let aps = [ap.clone()];
    let bps = [bp];
    let (ar, br) = tokio::join!(
        left.commit_capture_with_priors(
            &a,
            &EMBEDDING,
            &[],
            &aps,
            CapturePriorBudget::new(1),
            &aproof
        ),
        right.commit_capture_with_priors(
            &b,
            &EMBEDDING,
            &[],
            &bps,
            CapturePriorBudget::new(1),
            &bproof
        )
    );
    ar.unwrap();
    br.unwrap();
    assert_eq!(left.neighbors(target.id(), 8).await.unwrap().len(), 1);
    drop(left);
    drop(right);
    let reopened = fixture.open();
    assert!(reopened.get_node(a.id()).await.unwrap().is_some());
    assert!(reopened.get_node(b.id()).await.unwrap().is_some());
    let winner = if reopened
        .get_edge(a.id(), target.id())
        .await
        .unwrap()
        .is_some()
    {
        &a
    } else {
        &b
    };
    reopened
        .delete_edge(winner.id(), target.id())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .commit_capture_with_priors(
                &a,
                &EMBEDDING,
                &[],
                &[ap],
                CapturePriorBudget::new(1),
                &aproof
            )
            .await
            .unwrap(),
        CaptureCommitOutcome::AlreadyApplied
    );
    assert!(reopened.neighbors(target.id(), 8).await.unwrap().is_empty());
}

#[tokio::test]
async fn optional_capture_priors_roll_back_with_canonical_projections() {
    let store = capture_kernel_store();
    let target = active_node(NodeId(Ulid::new()));
    store.put_node(&target).await.unwrap();
    let node = captured_node(source("optional-rollback", 95));
    let prior = optional_prior(node.id(), &target);
    super::super::capture::arm_capture_failure(node.id(), 5);
    assert!(
        store
            .commit_capture_with_priors(
                &node,
                &EMBEDDING,
                &[],
                &[prior.clone()],
                CapturePriorBudget::new(2),
                &fresh_node_proof(&node)
            )
            .await
            .is_err()
    );
    assert_capture_absent(&store, node.id());
    assert!(
        store
            .get_edge(node.id(), target.id())
            .await
            .unwrap()
            .is_none()
    );
    store
        .commit_capture_with_priors(
            &node,
            &EMBEDDING,
            &[],
            &[prior],
            CapturePriorBudget::new(2),
            &fresh_node_proof(&node),
        )
        .await
        .unwrap();
    assert!(
        store
            .get_edge(node.id(), target.id())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn optional_capture_prior_respects_structural_storage_backstop() {
    let stores: [Box<dyn GraphStore>; 2] = [
        Box::new(crate::MemStore::new(4)),
        Box::new(capture_kernel_store()),
    ];
    for store in stores {
        let target = active_node(NodeId(Ulid::new()));
        store.put_node(&target).await.unwrap();
        for _ in 0..MAX_INCIDENT_EDGES {
            let structural = active_node(NodeId(Ulid::new()));
            store.put_node(&structural).await.unwrap();
            store
                .put_edge(&declared_edge(target.id(), structural.id()))
                .await
                .unwrap();
        }
        let node = captured_node(source("optional-hard-full", 96));
        store
            .commit_capture_with_priors(
                &node,
                &EMBEDDING,
                &[],
                &[optional_prior(node.id(), &target)],
                CapturePriorBudget::new(2),
                &fresh_node_proof(&node),
            )
            .await
            .unwrap();
        assert!(store.get_node(node.id()).await.unwrap().is_some());
        assert!(
            store
                .get_edge(node.id(), target.id())
                .await
                .unwrap()
                .is_none()
        );
        let required = captured_node(source("required-hard-full", 97));
        assert!(
            store
                .commit_capture_with_priors(
                    &required,
                    &EMBEDDING,
                    &[declared_edge(required.id(), target.id())],
                    &[],
                    CapturePriorBudget::new(2),
                    &fresh_node_proof(&required)
                )
                .await
                .is_err()
        );
        assert!(store.get_node(required.id()).await.unwrap().is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn optional_capture_priors_mem_handles_admit_only_one_at_endpoint_limit() {
    let store = Arc::new(crate::MemStore::new(4));
    let target = active_node(NodeId(Ulid::new()));
    store.put_node(&target).await.unwrap();
    fill_incident(store.as_ref(), target.id(), MAX_INCIDENT_EDGES - 1).await;
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for key in ["optional-mem-concurrent-a", "optional-mem-concurrent-b"] {
        let store = store.clone();
        let target = target.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let node = captured_node(source(key, 98));
            let prior = optional_prior(node.id(), &target);
            barrier.wait();
            store
                .commit_capture_with_priors(
                    &node,
                    &EMBEDDING,
                    &[],
                    &[prior],
                    CapturePriorBudget::new(1),
                    &fresh_node_proof(&node),
                )
                .await
                .unwrap();
            assert!(store.get_node(node.id()).await.unwrap().is_some());
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(store.neighbors(target.id(), 8).await.unwrap().len(), 1);
}

#[tokio::test]
async fn prior_budget_refills_after_full_changed_and_authored_prefix_on_both_backends() {
    let stores: [Box<dyn GraphStore>; 2] = [
        Box::new(crate::MemStore::new(4)),
        Box::new(capture_kernel_store()),
    ];
    for store in stores {
        let node = captured_node(source("optional-prefix-refill", 104));
        let full = active_node(NodeId(Ulid::new()));
        let changed = active_node(NodeId(Ulid::new()));
        let explicit = active_node(NodeId(Ulid::new()));
        let kept = [
            active_node(NodeId(Ulid::new())),
            active_node(NodeId(Ulid::new())),
        ];
        for target in [&full, &changed, &explicit, &kept[0], &kept[1]] {
            store.put_node(target).await.unwrap();
        }
        fill_incident(store.as_ref(), full.id(), MAX_INCIDENT_EDGES).await;
        let mut priors = vec![optional_prior(node.id(), &full)];
        priors.extend(vec![optional_prior(node.id(), &changed); 9]);
        priors.push(optional_prior(node.id(), &explicit));
        priors.extend(kept.iter().map(|target| optional_prior(node.id(), target)));
        store
            .put_node(&tagged_node(changed.id(), &["changed-meaning"]))
            .await
            .unwrap();
        let authored = [declared_edge(node.id(), explicit.id())];
        let proof = fresh_node_proof(&node);
        store
            .commit_capture_with_priors(
                &node,
                &EMBEDDING,
                &authored,
                &priors,
                CapturePriorBudget::new(2),
                &proof,
            )
            .await
            .unwrap();
        assert!(
            store
                .get_edge(node.id(), full.id())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_edge(node.id(), changed.id())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .get_edge(node.id(), explicit.id())
                .await
                .unwrap()
                .unwrap()
                .kind,
            EdgeKind::DerivedFrom
        );
        for target in &kept {
            assert!(
                store
                    .get_edge(node.id(), target.id())
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        // Delete learned state, then an oversized optional retry still replays
        // first and never restores those edges.
        store.delete_edge(node.id(), kept[0].id()).await.unwrap();
        let oversized = vec![priors[0].clone(); MAX_CAPTURE_PRIOR_CANDIDATES + 1];
        assert_eq!(
            store
                .commit_capture_with_priors(
                    &node,
                    &EMBEDDING,
                    &authored,
                    &oversized,
                    CapturePriorBudget::new(8),
                    &proof
                )
                .await
                .unwrap(),
            CaptureCommitOutcome::AlreadyApplied
        );
        assert!(
            store
                .get_edge(node.id(), kept[0].id())
                .await
                .unwrap()
                .is_none()
        );
        let fresh = captured_node(source("optional-oversized-fresh", 105));
        assert!(matches!(
            store
                .commit_capture_with_priors(
                    &fresh,
                    &EMBEDDING,
                    &[],
                    &oversized,
                    CapturePriorBudget::new(2),
                    &fresh_node_proof(&fresh)
                )
                .await,
            Err(Error::CapacityExceeded {
                resource: "capture prior candidates",
                ..
            })
        ));
        assert!(store.get_node(fresh.id()).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn arrival_prior_above_old_dense_threshold_does_not_prune_existing_routes() {
    let stores: [Box<dyn GraphStore>; 2] = [
        Box::new(crate::MemStore::new(4)),
        Box::new(capture_kernel_store()),
    ];
    for store in stores {
        let target = active_node(NodeId(Ulid::new()));
        store.put_node(&target).await.unwrap();
        for _ in 0..30 {
            let other = active_node(NodeId(Ulid::new()));
            store.put_node(&other).await.unwrap();
            store
                .put_edge(&Edge::new(
                    target.id(),
                    other.id(),
                    0.8,
                    EdgeKind::Associative,
                    1,
                ))
                .await
                .unwrap();
        }
        let node = captured_node(source("optional-crowded-arrival", 106));
        store
            .commit_capture_with_priors(
                &node,
                &EMBEDDING,
                &[],
                &[optional_prior(node.id(), &target)],
                CapturePriorBudget::new(1),
                &fresh_node_proof(&node),
            )
            .await
            .unwrap();
        assert!(
            store
                .get_edge(node.id(), target.id())
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(store.neighbors(target.id(), 64).await.unwrap().len(), 31);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn body_edit_persistent_competing_cas_preserves_projections_and_replay() {
    let fixture = FreshCaptureFixture::new().await;
    let lease = Arc::new(mneme_store_path::StoreLease::acquire(&fixture.path).unwrap());
    let first = Arc::new(CozoStore::open_leased_current(&fixture.path, lease.clone()).unwrap());
    let second = Arc::new(CozoStore::open_leased_current(&fixture.path, lease).unwrap());
    let node = captured_node(source("body-edit-persistent", 72));
    let proof = fresh_node_proof(&node);
    first
        .commit_capture(&node, &EMBEDDING, &proof)
        .await
        .unwrap();
    let projections = first
        .run(
            "?[id,e,status] := *node_vec{id,e,status}",
            BTreeMap::new(),
            false,
        )
        .unwrap();
    let search = first
        .run(
            "?[id,summary,status] := *node_search{id,summary,status}",
            BTreeMap::new(),
            false,
        )
        .unwrap();
    let tags = first
        .run(
            "?[tag,status,sample_hash,id] := *node_tag_v2{tag,status,sample_hash,id}",
            BTreeMap::new(),
            false,
        )
        .unwrap();
    let revision = node.body_revision();
    let a = BodyRef::new("inline://body-edit-a").unwrap();
    let b = BodyRef::new("inline://body-edit-b").unwrap();
    let (left, right) = tokio::join!(
        first.compare_replace_node_body(node.id(), &revision, &a),
        second.compare_replace_node_body(node.id(), &revision, &b)
    );
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    let changed = first.get_node(node.id()).await.unwrap().unwrap();
    let mut expected = node.clone();
    expected.set_body_reference(changed.body().clone(), mneme_core::BodyOwnership::Managed);
    assert_eq!(
        serde_json::to_value(changed).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(
        first
            .run(
                "?[id,e,status] := *node_vec{id,e,status}",
                BTreeMap::new(),
                false
            )
            .unwrap()
            .rows,
        projections.rows
    );
    assert_eq!(
        first
            .run(
                "?[id,summary,status] := *node_search{id,summary,status}",
                BTreeMap::new(),
                false
            )
            .unwrap()
            .rows,
        search.rows
    );
    assert_eq!(
        first
            .run(
                "?[tag,status,sample_hash,id] := *node_tag_v2{tag,status,sample_hash,id}",
                BTreeMap::new(),
                false
            )
            .unwrap()
            .rows,
        tags.rows
    );
    assert_eq!(
        second.lookup_capture(&proof).await.unwrap(),
        Some(node.id())
    );
    assert_eq!(
        second
            .commit_capture(&node, &EMBEDDING, &proof)
            .await
            .unwrap(),
        CaptureCommitOutcome::AlreadyApplied
    );
}
