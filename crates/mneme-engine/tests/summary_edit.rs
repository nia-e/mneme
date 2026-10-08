//! Shared semantic-edit contract, exercised against both native adapters.
use async_trait::async_trait;
use mneme_body::InlineStore;
use mneme_core::concern::{
    ConcernBinding, ConcernEndpoint, ConcernKind, ConcernNotice, ConcernUpdate,
};
use mneme_core::episode::{EpisodeTime, OccurrenceSpan};
use mneme_core::ports::{
    Embedder, Error, GraphStore, LexicalIndex, Result, StatusFilter, SystemClock, Traversal,
    VectorIndex,
};
use mneme_core::{
    BodyRef, BodySpan, BoundedTagSet, Edge, EdgeKind, EmbeddingFingerprint, NodeStatus,
    NodeSummary, SummarySnapshot, SummarySnapshotDigest, TouchstoneInput, TouchstoneReference,
    TouchstoneStore, TouchstoneSubject,
};
use mneme_cozo::MemStore;
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::{Capture, Config, EpisodeWrite, Memory};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;
use ulid::Ulid;

struct ControlledEmbedder {
    calls: AtomicUsize,
    fail: AtomicBool,
    pause: AtomicBool,
    started: Notify,
    resume: Notify,
}
impl ControlledEmbedder {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            started: Notify::new(),
            resume: Notify::new(),
        }
    }
}
#[async_trait]
impl Embedder for ControlledEmbedder {
    fn dim(&self) -> usize {
        DEFAULT_DIM
    }
    fn fingerprint(&self) -> EmbeddingFingerprint {
        HashingEmbedder::new(DEFAULT_DIM).fingerprint()
    }
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(Error::Backend("injected inference failure".into()));
        }
        if self.pause.swap(false, Ordering::SeqCst) {
            self.started.notify_one();
            self.resume.notified().await;
        }
        HashingEmbedder::new(DEFAULT_DIM).embed(texts).await
    }
}
fn request(key: &str) -> Capture<'_> {
    Capture::new(
        "summary-edit",
        key,
        "fixture://summary-edit",
        Some("session"),
        Some("turn"),
        "oldsummary",
        b"original evidence bytes",
        &["oldtag"],
    )
    .with_origin_commit(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
    .unwrap()
}
fn memory<S>(store: Arc<S>, embedder: Arc<ControlledEmbedder>) -> Arc<Memory>
where
    S: GraphStore + VectorIndex + Traversal + LexicalIndex + 'static,
{
    Arc::new(
        Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            embedder,
            Arc::new(SystemClock),
            Config {
                min_similarity_links: 0,
                ..Config::default()
            },
        )
        .with_body_store(Arc::new(InlineStore::new()))
        .with_lexical_index(store),
    )
}
fn without_summary(node: &mneme_core::Node) -> serde_json::Value {
    let mut value = serde_json::to_value(node).unwrap();
    value.as_object_mut().unwrap().remove("summary");
    value
}

async fn contract<S>(store: Arc<S>)
where
    S: GraphStore + VectorIndex + Traversal + LexicalIndex + TouchstoneStore + 'static,
{
    let embedder = Arc::new(ControlledEmbedder::new());
    let mem = memory(store.clone(), embedder.clone());
    let id = mem.capture(request("target")).await.unwrap().id;
    let mut before = store.get_node(id).await.unwrap().unwrap();
    before.record_exposure(123);
    before.record_grounded_use(456);
    before.try_set_confidence(0.83).unwrap();
    store.put_node(&before).await.unwrap();
    let digest = mem.summary_snapshot(id).await.unwrap().unwrap().digest();
    let target = mem.capture(request("edge-target")).await.unwrap().id;
    let target_node = store.get_node(target).await.unwrap().unwrap();
    let concern_binding = ConcernBinding::new(
        ConcernKind::Disagreement,
        ConcernEndpoint::from_node(&before),
        ConcernEndpoint::from_node(&target_node),
    )
    .unwrap();
    let concerns = store.concerns().unwrap();
    concerns
        .update_concern(&ConcernUpdate::Notice(
            ConcernNotice::new(
                concern_binding,
                "Possible disagreement",
                "Which note is current?",
            )
            .unwrap(),
        ))
        .await
        .unwrap();
    let concern_before = concerns.get_concern(&concern_binding.key()).await.unwrap();
    let mut anchor = Edge::new(id, target, 0.75, EdgeKind::Associative, 10);
    anchor.anchor = Some(BodySpan::new(0, 3));
    anchor.reinforce(789, &mneme_core::StrengthParams::default());
    anchor.mark_interference();
    store.put_edge(&anchor).await.unwrap();
    let learned_edge = serde_json::to_value(store.get_edge(id, target).await.unwrap()).unwrap();
    let edges_before = format!("{:?}", store.neighbors(id, 20).await.unwrap());
    let owner_input = TouchstoneInput::new(
        TouchstoneSubject::new("project:summary-edit").unwrap(),
        vec![TouchstoneReference::new(
            store.database_id().unwrap(),
            id,
            digest,
        )],
    )
    .unwrap();
    let owner = mem
        .capture(request("owner").with_touchstone(owner_input.clone()))
        .await
        .unwrap()
        .id;
    let historic = serde_json::to_value(mem.get_touchstone(owner).await.unwrap()).unwrap();
    let old_query = HashingEmbedder::new(DEFAULT_DIM)
        .embed(&["oldsummary"])
        .await
        .unwrap()
        .remove(0);
    let ann_before =
        serde_json::to_value(store.ann(&old_query, 20, StatusFilter::ALL).await.unwrap()).unwrap();
    let lex_before = serde_json::to_value(
        store
            .search("oldsummary", 20, StatusFilter::ALL)
            .await
            .unwrap(),
    )
    .unwrap();
    let calls = embedder.calls.load(Ordering::SeqCst);
    for (edit_id, expected, text) in [
        (id, digest, "oldsummary"),
        (id, SummarySnapshotDigest::new([0; 32]), "newsummary"),
        (
            owner,
            mem.summary_snapshot(owner).await.unwrap().unwrap().digest(),
            "newsummary",
        ),
        (mneme_core::NodeId(Ulid::new()), digest, "newsummary"),
    ] {
        assert!(
            mem.edit_summary(edit_id, &expected, NodeSummary::new(text).unwrap())
                .await
                .is_err()
        );
    }
    assert_eq!(
        embedder.calls.load(Ordering::SeqCst),
        calls,
        "ineligible edits must not enter inference"
    );
    embedder.fail.store(true, Ordering::SeqCst);
    assert!(
        mem.edit_summary(id, &digest, NodeSummary::new("newsummary").unwrap())
            .await
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(store.get_node(id).await.unwrap()).unwrap(),
        serde_json::to_value(Some(&before)).unwrap()
    );
    assert_eq!(
        serde_json::to_value(store.ann(&old_query, 20, StatusFilter::ALL).await.unwrap()).unwrap(),
        ann_before
    );
    assert_eq!(
        serde_json::to_value(
            store
                .search("oldsummary", 20, StatusFilter::ALL)
                .await
                .unwrap()
        )
        .unwrap(),
        lex_before
    );
    embedder.fail.store(false, Ordering::SeqCst);
    let edited = mem
        .edit_summary(id, &digest, NodeSummary::new("newsummary").unwrap())
        .await
        .unwrap();
    assert_eq!(without_summary(&before), without_summary(&edited));
    assert_eq!(
        serde_json::to_value(store.get_edge(id, target).await.unwrap()).unwrap(),
        learned_edge
    );
    assert_ne!(
        ConcernEndpoint::from_node(&edited),
        ConcernEndpoint::from_node(&before)
    );
    assert_eq!(
        concerns.get_concern(&concern_binding.key()).await.unwrap(),
        concern_before,
        "curation leaves old concern meaning stale, never rewrites its evidence"
    );
    assert_eq!(
        format!("{:?}", store.neighbors(id, 20).await.unwrap()),
        edges_before
    );
    assert_eq!(
        serde_json::to_value(mem.get_touchstone(owner).await.unwrap()).unwrap(),
        historic
    );
    assert!(
        store
            .search("newsummary", 20, StatusFilter::ALL)
            .await
            .unwrap()
            .iter()
            .any(|r| r.id == id)
    );
    assert!(
        !store
            .search("oldsummary", 20, StatusFilter::ALL)
            .await
            .unwrap()
            .iter()
            .any(|r| r.id == id)
    );
    let new_query = HashingEmbedder::new(DEFAULT_DIM)
        .embed(&["newsummary"])
        .await
        .unwrap()
        .remove(0);
    let result = store.ann(&new_query, 20, StatusFilter::ALL).await.unwrap();
    assert!(
        result
            .iter()
            .any(|r| r.id == id && (r.score - 1.0).abs() < 0.001)
    );
    assert_ne!(
        mem.summary_snapshot(id).await.unwrap().unwrap().digest(),
        digest
    );
    assert_eq!(
        mem.capture(request("target")).await.unwrap().id,
        id,
        "original source retry replays original id"
    );
    assert_eq!(
        store.get_node(id).await.unwrap().unwrap().summary(),
        "newsummary"
    );
    assert_eq!(
        mem.capture(request("owner").with_touchstone(owner_input))
            .await
            .unwrap()
            .id,
        owner,
        "historical owner retries do not demand current target equality"
    );
    let wrong_db = SummarySnapshot::from_node(Ulid::new(), &edited)
        .unwrap()
        .digest();
    assert!(matches!(
        mem.edit_summary(id, &wrong_db, NodeSummary::new("another summary").unwrap())
            .await,
        Err(Error::Conflict(_))
    ));
    let episode = mem
        .append_episode(EpisodeWrite::new(
            "summary-edit",
            "episode",
            "fixture://episode",
            None,
            None,
            "fixed episode edition",
            b"incident evidence",
            &[],
            OccurrenceSpan::Point {
                at: EpisodeTime::new(10).unwrap(),
            },
            None,
        ))
        .await
        .unwrap();
    let episode_id = episode.identity.edition_id;
    let episode_digest = mem
        .summary_snapshot(episode_id)
        .await
        .unwrap()
        .unwrap()
        .digest();
    let calls = embedder.calls.load(Ordering::SeqCst);
    assert!(
        mem.edit_summary(
            episode_id,
            &episode_digest,
            NodeSummary::new("rewrite event").unwrap()
        )
        .await
        .is_err()
    );
    assert_eq!(embedder.calls.load(Ordering::SeqCst), calls);
    assert!(
        store
            .compare_replace_node_summary(
                episode_id,
                &episode_digest,
                &NodeSummary::new("rewrite event").unwrap(),
                &new_query
            )
            .await
            .is_err()
    );
    assert!(
        store
            .compare_replace_node_summary(
                owner,
                &mem.summary_snapshot(owner).await.unwrap().unwrap().digest(),
                &NodeSummary::new("rewrite meaning").unwrap(),
                &new_query
            )
            .await
            .is_err()
    );
}

async fn concurrent_fields<S>(store: Arc<S>)
where
    S: GraphStore + VectorIndex + Traversal + LexicalIndex + TouchstoneStore + 'static,
{
    let embedder = Arc::new(ControlledEmbedder::new());
    let mem = memory(store.clone(), embedder.clone());
    let id = mem.capture(request("concurrency")).await.unwrap().id;
    let before = store.get_node(id).await.unwrap().unwrap();
    let digest = mem.summary_snapshot(id).await.unwrap().unwrap().digest();
    embedder.pause.store(true, Ordering::SeqCst);
    let editing = {
        let mem = mem.clone();
        tokio::spawn(async move {
            mem.edit_summary(
                id,
                &digest,
                NodeSummary::new("concurrent new summary").unwrap(),
            )
            .await
        })
    };
    embedder.started.notified().await;
    // A different writer can alter the excluded fields during inference. CAS
    // must hydrate current canonical state, not write the engine preflight copy.
    let replacement_body = BodyRef::new("inline://new-body").unwrap();
    store
        .compare_replace_node_body(id, &before.body_revision(), &replacement_body)
        .await
        .unwrap();
    let tags = BoundedTagSet::try_from_iter(["newtag"]).unwrap();
    store
        .compare_replace_node_tags(id, before.tag_set(), &tags)
        .await
        .unwrap();
    store.set_status(id, NodeStatus::Archived).await.unwrap();
    let fields = store.get_node(id).await.unwrap().unwrap();
    embedder.resume.notify_one();
    let edited = editing.await.unwrap().unwrap();
    assert_eq!(without_summary(&fields), without_summary(&edited));
    assert_eq!(edited.body(), &replacement_body);
    assert_eq!(edited.tag_set(), &tags);
    assert_eq!(edited.status(), NodeStatus::Archived);
    assert!(
        store
            .search("concurrent", 10, StatusFilter::ACTIVE)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .search("concurrent", 10, StatusFilter::ALL)
            .await
            .unwrap()
            .iter()
            .any(|r| r.id == id)
    );
    // A competing summary edit after engine preflight must defeat the backend
    // comparison; inference completing is not write authority.
    let stale = mem.summary_snapshot(id).await.unwrap().unwrap().digest();
    embedder.pause.store(true, Ordering::SeqCst);
    let editing = {
        let mem = mem.clone();
        tokio::spawn(async move {
            mem.edit_summary(id, &stale, NodeSummary::new("losing edit").unwrap())
                .await
        })
    };
    embedder.started.notified().await;
    let vector = HashingEmbedder::new(DEFAULT_DIM)
        .embed(&["winner alpha"])
        .await
        .unwrap()
        .remove(0);
    let won = store
        .compare_replace_node_summary(
            id,
            &stale,
            &NodeSummary::new("racing edit").unwrap(),
            &vector,
        )
        .await
        .unwrap();
    embedder.resume.notify_one();
    assert!(matches!(editing.await.unwrap(), Err(Error::Conflict(_))));
    assert_eq!(
        serde_json::to_value(store.get_node(id).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(won).unwrap()
    );
    // Independent callers with the same snapshot get exactly one winner.
    let digest = mem.summary_snapshot(id).await.unwrap().unwrap().digest();
    let a = NodeSummary::new("winner alpha").unwrap();
    let b = NodeSummary::new("winner beta").unwrap();
    let (a, b) = tokio::join!(
        store.compare_replace_node_summary(id, &digest, &a, &vector),
        store.compare_replace_node_summary(id, &digest, &b, &vector)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(matches!(
        a.as_ref().err().or(b.as_ref().err()),
        Some(Error::Conflict(_))
    ));
    // Deliberately an equality token, not an ABA-resistant revision number.
    let current = mem.summary_snapshot(id).await.unwrap().unwrap().digest();
    store
        .compare_replace_node_summary(
            id,
            &current,
            &NodeSummary::new("racing edit").unwrap(),
            &vector,
        )
        .await
        .unwrap();
    assert_eq!(
        mem.summary_snapshot(id).await.unwrap().unwrap().digest(),
        digest
    );
}

#[tokio::test]
async fn memstore_summary_edit_contract() {
    contract(Arc::new(MemStore::new(DEFAULT_DIM))).await;
}
#[tokio::test]
async fn memstore_summary_edit_concurrent_fields_and_one_winner() {
    concurrent_fields(Arc::new(MemStore::new(DEFAULT_DIM))).await;
}
#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_summary_edit_contract() {
    contract(Arc::new(mneme_cozo::CozoStore::new(DEFAULT_DIM).unwrap())).await;
}
#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_summary_edit_concurrent_fields_and_one_winner() {
    concurrent_fields(Arc::new(mneme_cozo::CozoStore::new(DEFAULT_DIM).unwrap())).await;
}
