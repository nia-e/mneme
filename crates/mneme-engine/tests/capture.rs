use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use mneme_body::InlineStore;
use mneme_core::ports::{
    BodyChunk, BodyStore, Embedder, Error, GraphStore, Result as PortResult, StatusFilter,
    SystemClock, Traversal, VectorIndex,
};
use mneme_core::{
    BodyRef, CaptureRequestCodec, CaptureSource, EdgeKind, EmbeddingFingerprint, Node, NodeId,
    NodeStatus, OriginCommit, Provenance, StrengthParams,
};
use mneme_cozo::MemStore;
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::{Capture, CaptureLink, Config, MAX_CAPTURE_BODY_BYTES, Memory};
use tokio::sync::Notify;

fn memory(store: Arc<MemStore>, bodies: Arc<InlineStore>) -> Memory {
    let graph: Arc<dyn GraphStore> = store.clone();
    let vectors: Arc<dyn VectorIndex> = store.clone();
    let traversal: Arc<dyn Traversal> = store;
    Memory::new(
        graph,
        vectors,
        traversal,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(bodies)
}

struct CountingEmbedder {
    calls: Arc<AtomicUsize>,
    inner: HashingEmbedder,
}

#[async_trait]
impl Embedder for CountingEmbedder {
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn fingerprint(&self) -> EmbeddingFingerprint {
        self.inner.fingerprint()
    }
    async fn embed(&self, texts: &[&str]) -> PortResult<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.embed(texts).await
    }
}

fn request<'a>(body: &'a [u8]) -> Capture<'a> {
    Capture::new(
        "codex",
        "session-uuid/turn-7/claim-1",
        "codex://sessions/550e8400-e29b-41d4-a716-446655440000#turn=7",
        Some("550e8400-e29b-41d4-a716-446655440000"),
        Some("turn-7"),
        "A durable fact about the project",
        body,
        &["capture"],
    )
}

fn other_request<'a>(key: &'a str) -> Capture<'a> {
    let mut request = request(b"target");
    request.key = key;
    request
}

#[tokio::test]
async fn fresh_capture_uses_v2_and_incoming_proof_accepts_only_both_named_v1_digests() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store.clone(), Arc::new(InlineStore::new()));
    let first = mem.capture(request(b"evidence")).await.unwrap();
    let node = store.get_node(first.id).await.unwrap().unwrap();
    let Provenance::External { source } = node.provenance() else {
        panic!("capture provenance")
    };
    assert_eq!(
        source
            .request_digest()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "14a537ea82a57319dbe41bc8a30d51f0cadad5276acd72b89c6d115b07337fe9"
    );
    assert_eq!(source.request_codec(), CaptureRequestCodec::CaptureV2);
    let proof = request(b"evidence").validated_replay_proof().unwrap();
    assert!(proof.matches_source(source));
    for golden in [
        "0245017cbc9155fe0339f23dd2b63a5fe8331fba11f9ac34c1d5acad31ae6037",
        "d21529ff5f02c41a54820d5dd2826a07cd18604333615955fc9cacf3c2924fed",
    ] {
        let mut digest = [0; 32];
        for (index, pair) in golden.as_bytes().chunks_exact(2).enumerate() {
            digest[index] = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
        }
        let predecessor = CaptureSource::new_with_codec(
            source.namespace(),
            source.key(),
            source.reference(),
            source.session(),
            source.revision(),
            digest,
            CaptureRequestCodec::CaptureV1,
        )
        .unwrap();
        assert!(proof.matches_source(&predecessor));
        assert!(
            !request(b"changed")
                .validated_replay_proof()
                .unwrap()
                .matches_source(&predecessor)
        );
    }
    assert!(
        mem.capture(request(b"evidence").with_links(&[]))
            .await
            .unwrap()
            .replayed
    );
}

#[tokio::test]
async fn linked_capture_is_atomic_order_independent_and_replay_does_not_repair_edges() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store.clone(), Arc::new(InlineStore::new()));
    let left = mem.capture(other_request("target-left")).await.unwrap().id;
    let right = mem.capture(other_request("target-right")).await.unwrap().id;
    let links = [
        CaptureLink::new(right, EdgeKind::Transition, 0.6).unwrap(),
        CaptureLink::new(left, EdgeKind::DerivedFrom, 0.8).unwrap(),
    ];
    let first = mem
        .capture(request(b"evidence").with_links(&links))
        .await
        .unwrap();
    assert!(!first.replayed);
    assert_eq!(
        store
            .get_edge(first.id, left)
            .await
            .unwrap()
            .unwrap()
            .weight(),
        0.8
    );
    assert_eq!(
        store.get_edge(first.id, right).await.unwrap().unwrap().kind,
        EdgeKind::Transition
    );
    let reordered = [links[1], links[0]];
    assert!(
        mem.capture(request(b"evidence").with_links(&reordered))
            .await
            .unwrap()
            .replayed
    );

    let mut learned = store.get_edge(first.id, left).await.unwrap().unwrap();
    learned.reinforce(learned.last_reinforced() + 1, &StrengthParams::default());
    let learned_weight = learned.weight();
    store.put_edge(&learned).await.unwrap();
    store.delete_edge(first.id, right).await.unwrap();
    assert!(
        mem.capture(request(b"evidence").with_links(&links))
            .await
            .unwrap()
            .replayed
    );
    assert_eq!(
        store
            .get_edge(first.id, left)
            .await
            .unwrap()
            .unwrap()
            .weight(),
        learned_weight
    );
    assert!(store.get_edge(first.id, right).await.unwrap().is_none());

    let changed = [
        CaptureLink::new(right, EdgeKind::Transition, 0.7).unwrap(),
        links[1],
    ];
    assert!(matches!(
        mem.capture(request(b"evidence").with_links(&changed)).await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        mem.capture(request(b"evidence")).await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn invalid_or_missing_capture_links_never_publish_a_node() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let graph: Arc<dyn GraphStore> = store.clone();
    let vectors: Arc<dyn VectorIndex> = store.clone();
    let traversal: Arc<dyn Traversal> = store.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = Memory::new(
        graph,
        vectors,
        traversal,
        Arc::new(CountingEmbedder {
            calls: calls.clone(),
            inner: HashingEmbedder::new(DEFAULT_DIM),
        }),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let missing = NodeId(ulid::Ulid::new());
    let link = CaptureLink::new(missing, EdgeKind::Associative, 0.5).unwrap();
    assert!(
        mem.capture(request(b"evidence").with_links(&[link]))
            .await
            .is_err()
    );
    assert!(
        store
            .all_nodes(mneme_core::ports::ColdPath::acquire())
            .await
            .unwrap()
            .is_empty()
    );
    let self_id = mneme_core::CaptureSource::new(
        "codex",
        "session-uuid/turn-7/claim-1",
        "codex://sessions/550e8400-e29b-41d4-a716-446655440000#turn=7",
        Some("550e8400-e29b-41d4-a716-446655440000"),
        Some("turn-7"),
        [0; 32],
    )
    .unwrap()
    .node_id();
    let self_link = CaptureLink::new(self_id, EdgeKind::Associative, 0.5).unwrap();
    for links in [vec![self_link], vec![link, link], vec![link; 9]] {
        assert!(matches!(
            mem.capture(request(b"evidence").with_links(&links)).await,
            Err(Error::InvalidInput(_))
        ));
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only the valid-shape, missing-target request reached inference"
    );
    assert!(CaptureLink::new(missing, EdgeKind::Bridge, 0.5).is_err());
    assert!(CaptureLink::new(missing, EdgeKind::Associative, f32::NAN).is_err());
    assert!(CaptureLink::new(missing, EdgeKind::Associative, 1.1).is_err());
}

#[tokio::test]
async fn exact_replay_preserves_one_complete_node_and_changed_payload_conflicts() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let bodies = Arc::new(InlineStore::new());
    let mem = memory(store.clone(), bodies);
    let first = mem.capture(request(b"evidence")).await.unwrap();
    assert!(!first.replayed);
    let node = store.get_node(first.id).await.unwrap().unwrap();
    assert_eq!(node.status(), NodeStatus::Active);
    assert!(matches!(node.provenance(), Provenance::External { .. }));
    let replay = mem.capture(request(b"evidence")).await.unwrap();
    assert_eq!(replay.id, first.id);
    assert!(replay.replayed);
    assert_eq!(
        store.get_node(first.id).await.unwrap().unwrap().body(),
        node.body()
    );
    let changed = mem
        .capture(request(b"corrected evidence"))
        .await
        .unwrap_err();
    assert!(matches!(changed, Error::Conflict(_)));
    let hits = store
        .ann(
            &HashingEmbedder::new(DEFAULT_DIM)
                .embed(&["A durable fact about the project"])
                .await
                .unwrap()[0],
            4,
            StatusFilter::ACTIVE,
        )
        .await
        .unwrap();
    assert!(hits.iter().any(|hit| hit.id == first.id));
    let mut second_request = request(b"another source observation");
    second_request.key = "session-uuid/turn-8/claim-2";
    let second = mem.capture(second_request).await.unwrap();
    assert_ne!(first.id, second.id);
    assert!(
        store
            .all_edges(mneme_core::ports::ColdPath::acquire())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn checkout_change_replays_original_anchor_but_source_revision_conflicts() {
    const FIRST_HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const LATER_HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store.clone(), Arc::new(InlineStore::new()));

    let first = mem
        .capture(
            request(b"evidence")
                .with_origin_commit(Some(FIRST_HEAD))
                .unwrap(),
        )
        .await
        .unwrap();
    let retry = mem
        .capture(
            request(b"evidence")
                .with_origin_commit(Some(LATER_HEAD))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retry.id, first.id);
    assert!(retry.replayed);
    assert_eq!(
        store
            .get_node(first.id)
            .await
            .unwrap()
            .unwrap()
            .origin_commit(),
        Some(OriginCommit::parse(FIRST_HEAD).unwrap())
    );

    let mut changed_revision = request(b"evidence");
    changed_revision.revision = Some("turn-8");
    assert!(matches!(
        mem.capture(
            changed_revision
                .with_origin_commit(Some(LATER_HEAD))
                .unwrap()
        )
        .await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn bad_source_and_oversize_body_fail_before_body_or_index_work() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store.clone(), Arc::new(InlineStore::new()));
    let mut bad = request(b"body");
    bad.namespace = "Codex";
    assert!(matches!(
        mem.capture(bad).await,
        Err(Error::InvalidInput(_))
    ));
    let long = vec![b'x'; MAX_CAPTURE_BODY_BYTES + 1];
    assert!(matches!(
        mem.capture(request(&long)).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(
        store
            .all_nodes(mneme_core::ports::ColdPath::acquire())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn missing_vector_and_foreign_identity_never_claim_replay() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store.clone(), Arc::new(InlineStore::new()));
    let first = mem.capture(request(b"evidence")).await.unwrap();
    store.remove(first.id).await.unwrap();
    assert!(matches!(
        mem.capture(request(b"evidence")).await,
        Err(Error::Conflict(_))
    ));

    let another_store = Arc::new(MemStore::new(DEFAULT_DIM));
    let another = memory(another_store.clone(), Arc::new(InlineStore::new()));
    let foreign = Node::try_new(
        first.id,
        "foreign",
        BodyRef::new("inline://foreign").unwrap(),
        [] as [&str; 0],
        Provenance::derived_empty(),
        0.5,
        0.5,
        NodeStatus::Active,
        0,
    )
    .unwrap();
    another_store.put_node(&foreign).await.unwrap();
    assert!(matches!(
        another.capture(request(b"evidence")).await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn missing_external_body_never_claims_successful_replay() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let bodies = Arc::new(InlineStore::new());
    let mem = memory(store.clone(), bodies.clone());
    let first = mem.capture(request(b"evidence")).await.unwrap();
    let node = store.get_node(first.id).await.unwrap().unwrap();
    bodies.delete(node.body()).await.unwrap();
    assert!(matches!(
        mem.capture(request(b"evidence")).await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn two_memory_instances_share_backend_cas() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let bodies = Arc::new(InlineStore::new());
    let a = memory(store.clone(), bodies.clone());
    let b = memory(store, bodies);
    let (left, right) = tokio::join!(
        a.capture(request(b"same evidence")),
        b.capture(request(b"same evidence")),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(left.id, right.id);
    assert_ne!(left.replayed, right.replayed);
}

struct PausingBodyStore {
    inner: Arc<InlineStore>,
    stored: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl BodyStore for PausingBodyStore {
    fn scheme(&self) -> &'static str {
        "inline"
    }
    async fn get(&self, body: &BodyRef) -> PortResult<Vec<u8>> {
        self.inner.get(body).await
    }
    async fn get_range(
        &self,
        body: &BodyRef,
        offset: u64,
        max_bytes: usize,
    ) -> PortResult<BodyChunk> {
        self.inner.get_range(body, offset, max_bytes).await
    }
    async fn put(&self, bytes: &[u8]) -> PortResult<BodyRef> {
        let body = self.inner.put(bytes).await?;
        self.stored.notify_one();
        self.release.notified().await;
        Ok(body)
    }
    async fn delete(&self, body: &BodyRef) -> PortResult<()> {
        self.inner.delete(body).await
    }
}

#[tokio::test]
async fn cancellation_after_body_write_cannot_publish_partial_capture() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let bodies = Arc::new(InlineStore::new());
    let stored = Arc::new(Notify::new());
    let graph: Arc<dyn GraphStore> = store.clone();
    let vectors: Arc<dyn VectorIndex> = store.clone();
    let traversal: Arc<dyn Traversal> = store.clone();
    let paused = Memory::new(
        graph,
        vectors,
        traversal,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(Arc::new(PausingBodyStore {
        inner: bodies.clone(),
        stored: stored.clone(),
        release: Arc::new(Notify::new()),
    }));
    let task = tokio::spawn(async move { paused.capture(request(b"evidence")).await });
    stored.notified().await;
    task.abort();
    assert!(task.await.is_err());
    assert!(
        store
            .all_nodes(mneme_core::ports::ColdPath::acquire())
            .await
            .unwrap()
            .is_empty()
    );
    let retry = memory(store, bodies)
        .capture(request(b"evidence"))
        .await
        .unwrap();
    assert!(!retry.replayed);
}

fn prior_memory(store: Arc<MemStore>, bodies: Arc<InlineStore>, cap: usize) -> Memory {
    Memory::new(
        store.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            similarity_link_cap: cap,
            min_similarity_links: 0,
            similarity_link_threshold: 0.9,
            ..Config::default()
        },
    )
    .with_body_store(bodies)
}

#[tokio::test]
async fn opted_in_capture_priors_share_planner_and_retry_never_relinks() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let bodies = Arc::new(InlineStore::new());
    let disabled = prior_memory(store.clone(), bodies.clone(), 0);
    let target = disabled
        .capture(other_request("prior-target"))
        .await
        .unwrap()
        .id;
    let mem = prior_memory(store.clone(), bodies.clone(), 2);
    let saved = mem.capture(request(b"evidence")).await.unwrap();
    assert_eq!(
        store
            .get_edge(saved.id, target)
            .await
            .unwrap()
            .unwrap()
            .kind,
        EdgeKind::Associative
    );
    store.delete_edge(saved.id, target).await.unwrap();
    disabled
        .capture(other_request("new-neighborhood"))
        .await
        .unwrap();
    assert!(mem.capture(request(b"evidence")).await.unwrap().replayed);
    assert!(
        disabled
            .capture(request(b"evidence"))
            .await
            .unwrap()
            .replayed
    );
    assert!(store.get_edge(saved.id, target).await.unwrap().is_none());
    assert!(store.neighbors(saved.id, 8).await.unwrap().is_empty());
}

#[tokio::test]
async fn capture_explicit_target_kind_wins_over_similarity_prior() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let bodies = Arc::new(InlineStore::new());
    let disabled = prior_memory(store.clone(), bodies.clone(), 0);
    let target = disabled
        .capture(other_request("explicit-target"))
        .await
        .unwrap()
        .id;
    let mem = prior_memory(store.clone(), bodies, 2);
    let links = [CaptureLink::new(target, EdgeKind::Transition, 0.4).unwrap()];
    let saved = mem
        .capture(request(b"evidence").with_links(&links))
        .await
        .unwrap();
    let edge = store.get_edge(saved.id, target).await.unwrap().unwrap();
    assert_eq!(edge.kind, EdgeKind::Transition);
    assert_eq!(edge.weight(), 0.4);
    let bad = [CaptureLink::new(NodeId(ulid::Ulid::new()), EdgeKind::Associative, 0.5).unwrap()];
    assert!(
        mem.capture(other_request("invalid-required").with_links(&bad))
            .await
            .is_err()
    );
}

struct FailOnEmbeddingCall {
    calls: Arc<AtomicUsize>,
    fail_on: usize,
    inner: HashingEmbedder,
}
#[async_trait]
impl Embedder for FailOnEmbeddingCall {
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn fingerprint(&self) -> EmbeddingFingerprint {
        self.inner.fingerprint()
    }
    async fn embed(&self, texts: &[&str]) -> PortResult<Vec<Vec<f32>>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.fail_on {
            return Err(Error::Backend("synthetic embedding failure".into()));
        }
        self.inner.embed(texts).await
    }
}
#[tokio::test]
async fn optional_discovery_failure_saves_claim_but_mandatory_embedding_failure_does_not() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let bodies = Arc::new(InlineStore::new());
    prior_memory(store.clone(), bodies.clone(), 0)
        .capture(other_request("discovery-target"))
        .await
        .unwrap();
    for fail_on in [2, 1] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mem = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(FailOnEmbeddingCall {
                calls: calls.clone(),
                fail_on,
                inner: HashingEmbedder::new(DEFAULT_DIM),
            }),
            Arc::new(SystemClock),
            Config {
                similarity_link_cap: 2,
                min_similarity_links: 0,
                similarity_link_threshold: 0.9,
                ..Config::default()
            },
        )
        .with_body_store(bodies.clone());
        let mut req = request(b"first paragraph\n\nsecond paragraph");
        req.key = if fail_on == 2 {
            "optional-discovery-fails"
        } else {
            "mandatory-embedding-fails"
        };
        let result = mem.capture(req).await;
        if fail_on == 2 {
            let saved = result.unwrap();
            assert!(store.get_node(saved.id).await.unwrap().is_some());
            assert!(store.neighbors(saved.id, 8).await.unwrap().is_empty());
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        } else {
            assert!(matches!(result, Err(Error::Backend(_))));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}

struct SnapshotEmbedder {
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Embedder for SnapshotEmbedder {
    fn dim(&self) -> usize {
        2
    }
    fn fingerprint(&self) -> EmbeddingFingerprint {
        HashingEmbedder::new(2).fingerprint()
    }
    async fn embed(&self, texts: &[&str]) -> PortResult<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(texts
            .iter()
            .map(|text| {
                if *text == "unrelated replacement" {
                    vec![0.0, 1.0]
                } else {
                    vec![1.0, 0.0]
                }
            })
            .collect())
    }
}
struct MutateAfterNominationIndex {
    inner: Arc<MemStore>,
    replacement: Node,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl VectorIndex for MutateAfterNominationIndex {
    fn semantic_id(&self) -> &'static str {
        self.inner.semantic_id()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    async fn upsert(&self, id: NodeId, embedding: &[f32]) -> PortResult<()> {
        self.inner.upsert(id, embedding).await
    }
    async fn remove(&self, id: NodeId) -> PortResult<()> {
        self.inner.remove(id).await
    }
    async fn ann(
        &self,
        query: &[f32],
        k: usize,
        status: StatusFilter,
    ) -> PortResult<Vec<mneme_core::ports::Scored>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let nominations = self.inner.ann(query, k, status).await?;
        assert!(
            nominations
                .iter()
                .any(|hit| hit.id == self.replacement.id() && hit.score == 1.0)
        );
        self.inner.put_node(&self.replacement).await?;
        Ok(nominations)
    }
}

#[tokio::test]
async fn capture_rescores_hydrated_meaning_not_stale_ann_score() {
    let store = Arc::new(MemStore::new(2));
    let bodies = Arc::new(InlineStore::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let disabled = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(SnapshotEmbedder {
            calls: calls.clone(),
        }),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(bodies.clone());
    let target_id = disabled
        .capture(other_request("race-target"))
        .await
        .unwrap()
        .id;
    let target = store.get_node(target_id).await.unwrap().unwrap();
    let replacement = Node::try_new(
        target_id,
        "unrelated replacement",
        target.body().clone(),
        target.tags(),
        target.provenance().clone(),
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    let ann_calls = Arc::new(AtomicUsize::new(0));
    let index = Arc::new(MutateAfterNominationIndex {
        inner: store.clone(),
        replacement,
        calls: ann_calls.clone(),
    });
    let mem = Memory::new(
        store.clone(),
        index,
        store.clone(),
        Arc::new(SnapshotEmbedder {
            calls: calls.clone(),
        }),
        Arc::new(SystemClock),
        Config {
            similarity_link_cap: 2,
            min_similarity_links: 0,
            similarity_link_threshold: 0.9,
            ..Config::default()
        },
    )
    .with_body_store(bodies);
    let saved = mem
        .capture(request(b"paragraph one\n\nparagraph two"))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3); // seed, new claim, bounded snapshot rescore; no anchors
    assert_eq!(ann_calls.load(Ordering::SeqCst), 1);
    assert!(store.get_edge(saved.id, target_id).await.unwrap().is_none());
    assert!(
        mem.capture(request(b"paragraph one\n\nparagraph two"))
            .await
            .unwrap()
            .replayed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(ann_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn disabled_capture_has_no_discovery_embedding_and_enabled_capture_skips_anchor_batch() {
    let store = Arc::new(MemStore::new(2));
    let calls = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(InlineStore::new());
    let disabled = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(SnapshotEmbedder {
            calls: calls.clone(),
        }),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(bodies.clone());
    let target = disabled
        .capture(other_request("cost-target"))
        .await
        .unwrap()
        .id;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let enabled = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(SnapshotEmbedder {
            calls: calls.clone(),
        }),
        Arc::new(SystemClock),
        Config {
            similarity_link_cap: 2,
            min_similarity_links: 0,
            similarity_link_threshold: 0.9,
            ..Config::default()
        },
    )
    .with_body_store(bodies);
    let saved = enabled
        .capture(request(b"paragraph one\n\nparagraph two"))
        .await
        .unwrap();
    assert!(store.get_edge(saved.id, target).await.unwrap().is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 3); // no paragraph-anchor inference
}

struct MalformedRescoreEmbedder {
    calls: AtomicUsize,
    mode: usize,
}
#[async_trait]
impl Embedder for MalformedRescoreEmbedder {
    fn dim(&self) -> usize {
        2
    }
    fn fingerprint(&self) -> EmbeddingFingerprint {
        HashingEmbedder::new(2).fingerprint()
    }
    async fn embed(&self, texts: &[&str]) -> PortResult<Vec<Vec<f32>>> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            return Ok(match self.mode {
                0 => vec![],
                1 => vec![vec![1.0]],
                _ => vec![vec![f32::NAN, 0.0]],
            });
        }
        Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
    }
}
#[tokio::test]
async fn malformed_optional_rescore_batches_save_unlinked_without_panicking() {
    for mode in 0..3 {
        let store = Arc::new(MemStore::new(2));
        let bodies = Arc::new(InlineStore::new());
        let seed = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(SnapshotEmbedder {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(SystemClock),
            Config::default(),
        )
        .with_body_store(bodies.clone());
        seed.capture(other_request("malformed-batch-target"))
            .await
            .unwrap();
        let mem = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(MalformedRescoreEmbedder {
                calls: AtomicUsize::new(0),
                mode,
            }),
            Arc::new(SystemClock),
            Config {
                similarity_link_cap: 2,
                min_similarity_links: 0,
                ..Config::default()
            },
        )
        .with_body_store(bodies);
        let saved = mem.capture(request(b"claim")).await.unwrap();
        assert!(store.get_node(saved.id).await.unwrap().is_some());
        assert!(store.neighbors(saved.id, 8).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn ingest_legacy_floor_is_retained_but_weight_uses_hydrated_meaning() {
    let store = Arc::new(MemStore::new(2));
    let bodies = Arc::new(InlineStore::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let seed = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(SnapshotEmbedder {
            calls: calls.clone(),
        }),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(bodies.clone());
    let target_id = seed
        .capture(other_request("legacy-floor-target"))
        .await
        .unwrap()
        .id;
    let target = store.get_node(target_id).await.unwrap().unwrap();
    let replacement = Node::try_new(
        target_id,
        "unrelated replacement",
        target.body().clone(),
        target.tags(),
        target.provenance().clone(),
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    let index = Arc::new(MutateAfterNominationIndex {
        inner: store.clone(),
        replacement,
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let mem = Memory::new(
        store.clone(),
        index,
        store.clone(),
        Arc::new(SnapshotEmbedder { calls }),
        Arc::new(SystemClock),
        Config {
            similarity_link_cap: 2,
            min_similarity_links: 1,
            similarity_link_threshold: 0.9,
            ..Config::default()
        },
    )
    .with_body_store(bodies);
    let saved = mem
        .ingest(mneme_engine::Ingest::new(
            "related",
            b"body",
            &[],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();
    assert_eq!(
        store
            .get_edge(saved, target_id)
            .await
            .unwrap()
            .unwrap()
            .weight(),
        0.0
    );
}

struct RecordingArrivalEmbedder {
    batches: Arc<std::sync::Mutex<Vec<usize>>>,
}
#[async_trait]
impl Embedder for RecordingArrivalEmbedder {
    fn dim(&self) -> usize {
        2
    }
    fn fingerprint(&self) -> EmbeddingFingerprint {
        HashingEmbedder::new(2).fingerprint()
    }
    async fn embed(&self, texts: &[&str]) -> PortResult<Vec<Vec<f32>>> {
        self.batches.lock().unwrap().push(texts.len());
        Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
    }
}
struct RecordingArrivalIndex {
    inner: Arc<MemStore>,
    ks: Arc<std::sync::Mutex<Vec<usize>>>,
}
#[async_trait]
impl VectorIndex for RecordingArrivalIndex {
    fn semantic_id(&self) -> &'static str {
        self.inner.semantic_id()
    }
    fn dim(&self) -> usize {
        2
    }
    async fn upsert(&self, id: NodeId, vector: &[f32]) -> PortResult<()> {
        self.inner.upsert(id, vector).await
    }
    async fn remove(&self, id: NodeId) -> PortResult<()> {
        self.inner.remove(id).await
    }
    async fn ann(
        &self,
        query: &[f32],
        k: usize,
        status: StatusFilter,
    ) -> PortResult<Vec<mneme_core::ports::Scored>> {
        self.ks.lock().unwrap().push(k);
        self.inner.ann(query, k, status).await
    }
}
fn arrival_target(id: u128) -> Node {
    Node::try_new(
        NodeId(ulid::Ulid::from(id)),
        "related",
        BodyRef::new("inline://related").unwrap(),
        std::iter::empty::<&str>(),
        Provenance::derived_empty(),
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap()
}

#[tokio::test]
async fn nomination_budget_is_separate_from_two_retained_links_and_authored_precedence() {
    let store = Arc::new(MemStore::new(2));
    let targets: Vec<_> = (1..=6).map(arrival_target).collect();
    for target in &targets {
        store.put_node(target).await.unwrap();
        store.upsert(target.id(), &[1.0, 0.0]).await.unwrap();
    }
    let ks = Arc::new(std::sync::Mutex::new(Vec::new()));
    let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
    let memory = Memory::new(
        store.clone(),
        Arc::new(RecordingArrivalIndex {
            inner: store.clone(),
            ks: ks.clone(),
        }),
        store.clone(),
        Arc::new(RecordingArrivalEmbedder {
            batches: batches.clone(),
        }),
        Arc::new(SystemClock),
        Config {
            ann_k: 5,
            similarity_link_cap: 2,
            similarity_link_threshold: 0.9,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let links = [CaptureLink::new(targets[0].id(), EdgeKind::Transition, 0.2).unwrap()];
    let mut input = request(b"grounded evidence");
    input.links = &links;
    let saved = memory.capture(input).await.unwrap();
    assert_eq!(*ks.lock().unwrap(), vec![6]);
    assert_eq!(*batches.lock().unwrap(), vec![1, 5]);
    assert_eq!(
        store
            .get_edge(saved.id, targets[0].id())
            .await
            .unwrap()
            .unwrap()
            .kind,
        EdgeKind::Transition
    );
    assert_eq!(store.neighbors(saved.id, 8).await.unwrap().len(), 3);
    for target in &targets[1..=2] {
        assert!(
            store
                .get_edge(saved.id, target.id())
                .await
                .unwrap()
                .is_some()
        );
    }
    for target in &targets[3..] {
        assert!(
            store
                .get_edge(saved.id, target.id())
                .await
                .unwrap()
                .is_none()
        );
    }
    let mut input = request(b"grounded evidence");
    input.links = &links;
    memory.capture(input).await.unwrap();
    assert_eq!(*ks.lock().unwrap(), vec![6]);
    assert_eq!(*batches.lock().unwrap(), vec![1, 5]);
}

#[tokio::test]
async fn zero_optional_allowances_and_full_authored_batch_do_no_discovery() {
    for (cap, ann_k, authored_count) in [(0, 5, 0), (2, 0, 0), (2, 5, 8)] {
        let store = Arc::new(MemStore::new(2));
        let mut links = Vec::new();
        for i in 1..=authored_count {
            let target = arrival_target(i);
            store.put_node(&target).await.unwrap();
            store.upsert(target.id(), &[1.0, 0.0]).await.unwrap();
            links.push(CaptureLink::new(target.id(), EdgeKind::Transition, 0.2).unwrap());
        }
        let ks = Arc::new(std::sync::Mutex::new(Vec::new()));
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let memory = Memory::new(
            store.clone(),
            Arc::new(RecordingArrivalIndex {
                inner: store.clone(),
                ks: ks.clone(),
            }),
            store,
            Arc::new(RecordingArrivalEmbedder {
                batches: batches.clone(),
            }),
            Arc::new(SystemClock),
            Config {
                ann_k,
                similarity_link_cap: cap,
                ..Config::default()
            },
        )
        .with_body_store(Arc::new(InlineStore::new()));
        let mut input = request(b"evidence");
        input.links = &links;
        memory.capture(input).await.unwrap();
        assert!(ks.lock().unwrap().is_empty());
        assert_eq!(*batches.lock().unwrap(), vec![1]);
    }
}

#[tokio::test]
async fn ingest_ann_one_has_self_allowance_and_selects_only_its_link_budget() {
    let store = Arc::new(MemStore::new(2));
    let target = arrival_target(1);
    store.put_node(&target).await.unwrap();
    store.upsert(target.id(), &[1.0, 0.0]).await.unwrap();
    let ks = Arc::new(std::sync::Mutex::new(Vec::new()));
    let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
    let memory = Memory::new(
        store.clone(),
        Arc::new(RecordingArrivalIndex {
            inner: store.clone(),
            ks: ks.clone(),
        }),
        store.clone(),
        Arc::new(RecordingArrivalEmbedder {
            batches: batches.clone(),
        }),
        Arc::new(SystemClock),
        Config {
            ann_k: 1,
            similarity_link_cap: 2,
            similarity_link_threshold: 0.9,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let saved = memory
        .ingest(mneme_engine::Ingest::new(
            "related",
            b"one body",
            &[],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();
    assert_eq!(*ks.lock().unwrap(), vec![2]);
    assert_eq!(*batches.lock().unwrap(), vec![1, 1]);
    assert!(store.get_edge(saved, target.id()).await.unwrap().is_some());
}

#[tokio::test]
async fn default_floor_abstains_below_threshold_without_discarding_capture() {
    let store = Arc::new(MemStore::new(2));
    let target = Node::try_new(
        NodeId(ulid::Ulid::from(1u128)),
        "unrelated replacement",
        BodyRef::new("inline://unrelated").unwrap(),
        std::iter::empty::<&str>(),
        Provenance::derived_empty(),
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    store.put_node(&target).await.unwrap();
    store.upsert(target.id(), &[0.0, 1.0]).await.unwrap();
    let cfg = Config {
        similarity_link_cap: 2,
        similarity_link_threshold: 0.9,
        ..Config::default()
    };
    assert_eq!(cfg.min_similarity_links, 0);
    let memory = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(SnapshotEmbedder {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        Arc::new(SystemClock),
        cfg,
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let saved = memory
        .capture(request(b"unrelated evidence"))
        .await
        .unwrap();
    assert!(store.get_node(saved.id).await.unwrap().is_some());
    assert!(
        store
            .get_edge(saved.id, target.id())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn nomination_and_rescore_have_independent_hard_work_bounds() {
    let store = Arc::new(MemStore::new(2));
    let cap = mneme_core::ports::MAX_CAPTURE_PRIOR_CANDIDATES;
    for i in 1..=cap + 1 {
        let target = arrival_target(i as u128);
        store.put_node(&target).await.unwrap();
        store.upsert(target.id(), &[1.0, 0.0]).await.unwrap();
    }
    let ks = Arc::new(std::sync::Mutex::new(Vec::new()));
    let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
    let memory = Memory::new(
        store.clone(),
        Arc::new(RecordingArrivalIndex {
            inner: store.clone(),
            ks: ks.clone(),
        }),
        store.clone(),
        Arc::new(RecordingArrivalEmbedder {
            batches: batches.clone(),
        }),
        Arc::new(SystemClock),
        Config {
            ann_k: usize::MAX,
            similarity_link_cap: 2,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let saved = memory.capture(request(b"bounded discovery")).await.unwrap();
    assert_eq!(*ks.lock().unwrap(), vec![cap + 1]);
    assert_eq!(*batches.lock().unwrap(), vec![1, cap]);
    assert_eq!(store.neighbors(saved.id, 8).await.unwrap().len(), 2);
}
