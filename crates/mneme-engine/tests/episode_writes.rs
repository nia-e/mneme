use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use async_trait::async_trait;
use mneme_body::InlineStore;
use mneme_core::episode::{
    EpisodeGet, EpisodeHistoryRequest, EpisodePageLimit, EpisodeRevisionReason, EpisodeThread,
    EpisodeTime, MAX_EPISODE_BODY_BYTES, MAX_EPISODE_SUMMARY_BYTES, OccurrenceContextRef,
    OccurrenceContexts, OccurrenceSpan,
};
use mneme_core::ports::{
    BodyChunk, BodyStore, Clock, ColdPath, Embedder, Error, GraphStore, Result as PortResult,
    Traversal, VectorIndex,
};
use mneme_core::{BodyRef, CaptureRequestCodec, EdgeKind, EmbeddingFingerprint, OriginCommit};
use mneme_cozo::MemStore;
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::episode::EpisodeWrite;
use mneme_engine::{Capture, CaptureLink, Config, Memory};
use tokio::sync::Barrier;

#[derive(Default)]
struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now(&self) -> u128 {
        u128::from(self.0.load(Ordering::SeqCst))
    }
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

fn memory(
    store: Arc<MemStore>,
    bodies: Arc<dyn BodyStore>,
    clock: Arc<TestClock>,
    calls: Arc<AtomicUsize>,
) -> Memory {
    let graph: Arc<dyn GraphStore> = store.clone();
    let vectors: Arc<dyn VectorIndex> = store.clone();
    let traversal: Arc<dyn Traversal> = store;
    Memory::new(
        graph,
        vectors,
        traversal,
        Arc::new(CountingEmbedder {
            calls,
            inner: HashingEmbedder::new(DEFAULT_DIM),
        }),
        clock,
        Config::default(),
    )
    .with_body_store(bodies)
}

fn request<'a>(key: &'a str, body: &'a [u8]) -> EpisodeWrite<'a> {
    EpisodeWrite::new(
        "codex",
        key,
        "fixture://episode",
        Some("test-session"),
        None,
        "The floodgate attempt failed before we found the leak",
        body,
        &["mindcraft"],
        OccurrenceSpan::Point {
            at: EpisodeTime::new(10).unwrap(),
        },
        Some(EpisodeThread::new("canal-test").unwrap()),
    )
}

fn reason() -> EpisodeRevisionReason {
    EpisodeRevisionReason::new("Correct the chronology after inspecting the build log").unwrap()
}

#[tokio::test]
async fn retry_after_revision_returns_immutable_original_and_original_clock_anchor() {
    const FIRST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const LATER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let bodies = Arc::new(InlineStore::new());
    let clock = Arc::new(TestClock::default());
    clock.0.store(100, Ordering::SeqCst);
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = memory(store.clone(), bodies, clock.clone(), calls.clone());
    let first = mem
        .append_episode(
            request("scene-1", b"We blamed the wrong block")
                .with_origin_commit(Some(FIRST))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(!first.replayed);
    assert_eq!(first.identity.revision.get(), 0);
    clock.0.store(90, Ordering::SeqCst); // Ordinals, not wall-clock ordering.
    let second = mem
        .revise_episode(
            first.identity.episode_id,
            first.identity.edition_id,
            reason(),
            request(
                "scene-1-edit-1",
                b"The log shows the leak began before that block changed",
            ),
        )
        .await
        .unwrap();
    assert_eq!(second.identity.revision.get(), 1);
    let current = mem
        .get_episode(&EpisodeGet {
            episode_id: first.identity.episode_id,
            edition_id: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.identity, second.identity);
    assert_eq!(current.node.created(), 90);
    assert_eq!(current.node.episode().unwrap().recorded_at().get(), 100);
    clock.0.store(200, Ordering::SeqCst);
    let replay = mem
        .append_episode(
            request("scene-1", b"We blamed the wrong block")
                .with_origin_commit(Some(LATER))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(replay.identity, first.identity);
    assert!(replay.replayed);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "exact replay does no inference"
    );
    let original = store
        .get_node(first.identity.edition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.created(), 100);
    assert_eq!(
        original.origin_commit(),
        Some(OriginCommit::parse(FIRST).unwrap())
    );
    assert_eq!(
        mem.resolve_body(&original).await.unwrap(),
        b"We blamed the wrong block"
    );
    let history = mem
        .episode_history(&EpisodeHistoryRequest {
            episode_id: first.identity.episode_id,
            limit: EpisodePageLimit::default(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(
        history
            .items
            .iter()
            .map(|row| row.identity)
            .collect::<Vec<_>>(),
        vec![first.identity, second.identity]
    );
}

#[tokio::test]
async fn authored_facet_reason_and_intent_are_part_of_retry_proof() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = memory(
        store.clone(),
        Arc::new(InlineStore::new()),
        Arc::new(TestClock::default()),
        calls.clone(),
    );
    let first = mem.append_episode(request("scene", b"body")).await.unwrap();
    let mut occurred = request("scene", b"body");
    occurred.occurrence = OccurrenceSpan::Unknown;
    let mut thread = request("scene", b"body");
    thread.thread = Some(EpisodeThread::new("another-thread").unwrap());
    for changed in [occurred, thread, request("scene", b"different")] {
        assert!(matches!(
            mem.append_episode(changed).await,
            Err(Error::Conflict(_))
        ));
    }
    let edit = mem
        .revise_episode(
            first.identity.episode_id,
            first.identity.edition_id,
            reason(),
            request("edit", b"amended"),
        )
        .await
        .unwrap();
    let replay = mem
        .revise_episode(
            first.identity.episode_id,
            first.identity.edition_id,
            reason(),
            request("edit", b"amended"),
        )
        .await
        .unwrap();
    assert_eq!(edit.identity, replay.identity);
    assert!(replay.replayed);
    assert!(matches!(
        mem.revise_episode(
            first.identity.episode_id,
            first.identity.edition_id,
            EpisodeRevisionReason::new("A different explanation").unwrap(),
            request("edit", b"amended")
        )
        .await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        mem.append_episode(request("edit", b"amended")).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn edition_links_are_explicit_and_replay_never_infers_more() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(
        store.clone(),
        Arc::new(InlineStore::new()),
        Arc::new(TestClock::default()),
        Arc::new(AtomicUsize::new(0)),
    );
    let lesson = mem
        .capture(Capture::new(
            "codex",
            "lesson",
            "fixture://lesson",
            None,
            None,
            "Check the upstream seal before replacing blocks",
            b"lesson",
            &["lesson"],
        ))
        .await
        .unwrap();
    let links = [CaptureLink::new(lesson.id, EdgeKind::DerivedFrom, 0.8).unwrap()];
    let first = mem
        .append_episode(request("scene", b"We found the leak").with_links(&links))
        .await
        .unwrap();
    let edges = store.all_edges(ColdPath::acquire()).await.unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].from, first.identity.edition_id);
    assert_eq!(edges[0].to, lesson.id);
    assert_eq!(edges[0].kind, EdgeKind::DerivedFrom);
    assert!(
        mem.append_episode(request("scene", b"We found the leak").with_links(&links))
            .await
            .unwrap()
            .replayed
    );
    assert!(matches!(
        mem.append_episode(request("scene", b"We found the leak"))
            .await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(store.all_edges(ColdPath::acquire()).await.unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_episode_shape_fails_before_embedding_or_publication() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = memory(
        store.clone(),
        Arc::new(InlineStore::new()),
        Arc::new(TestClock::default()),
        calls.clone(),
    );
    let body = vec![b'x'; MAX_EPISODE_BODY_BYTES + 1];
    let summary = "x".repeat(MAX_EPISODE_SUMMARY_BYTES + 1);
    let mut long_summary = request("too-long-summary", b"body");
    long_summary.summary = &summary;
    let mut core = request("core", b"body");
    core.tags = &["core"];
    let mut span = request("reversed-span", b"body");
    span.occurrence = OccurrenceSpan::Range {
        start: EpisodeTime::new(2).unwrap(),
        end: EpisodeTime::new(1).unwrap(),
    };
    let mut source = request("bad-source", b"body");
    source.namespace = "BAD";
    for invalid in [
        request("too-long-body", &body),
        long_summary,
        core,
        span,
        source,
    ] {
        assert!(matches!(
            mem.append_episode(invalid).await,
            Err(Error::InvalidInput(_))
        ));
    }
    let self_id = mneme_core::CaptureSource::new(
        "codex",
        "self-link",
        "fixture://episode",
        Some("test-session"),
        None,
        [0; 32],
    )
    .unwrap()
    .node_id();
    let self_link = [CaptureLink::new(self_id, EdgeKind::Associative, 0.5).unwrap()];
    assert!(matches!(
        mem.append_episode(request("self-link", b"body").with_links(&self_link))
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        store
            .all_nodes(ColdPath::acquire())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn exact_replay_verifies_the_historical_body_not_the_current_edition() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let bodies = Arc::new(InlineStore::new());
    let mem = memory(
        store.clone(),
        bodies.clone(),
        Arc::new(TestClock::default()),
        Arc::new(AtomicUsize::new(0)),
    );
    let root = mem
        .append_episode(request("scene", b"original"))
        .await
        .unwrap();
    mem.revise_episode(
        root.identity.episode_id,
        root.identity.edition_id,
        reason(),
        request("edit", b"new body"),
    )
    .await
    .unwrap();
    let original = store
        .get_node(root.identity.edition_id)
        .await
        .unwrap()
        .unwrap();
    bodies.delete(original.body()).await.unwrap();
    assert!(matches!(
        mem.append_episode(request("scene", b"original")).await,
        Err(Error::Conflict(_))
    ));
    assert!(
        mem.revise_episode(
            root.identity.episode_id,
            root.identity.edition_id,
            reason(),
            request("edit", b"new body")
        )
        .await
        .unwrap()
        .replayed
    );
}

struct RacingBodies {
    inner: Arc<InlineStore>,
    barrier: Barrier,
    deleted: AtomicUsize,
}
#[async_trait]
impl BodyStore for RacingBodies {
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
        let result = self.inner.put(bytes).await?;
        self.barrier.wait().await;
        Ok(result)
    }
    async fn delete(&self, body: &BodyRef) -> PortResult<()> {
        self.deleted.fetch_add(1, Ordering::SeqCst);
        self.inner.delete(body).await
    }
}

#[tokio::test]
async fn racing_editions_cas_once_and_keep_ambiguously_shared_body_refs() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let inline = Arc::new(InlineStore::new());
    let clock = Arc::new(TestClock::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let root = memory(store.clone(), inline.clone(), clock.clone(), calls.clone())
        .append_episode(request("scene", b"initial"))
        .await
        .unwrap();
    let bodies = Arc::new(RacingBodies {
        inner: inline.clone(),
        barrier: Barrier::new(2),
        deleted: AtomicUsize::new(0),
    });
    let left = memory(store.clone(), bodies.clone(), clock.clone(), calls.clone());
    let right = memory(store.clone(), bodies.clone(), clock, calls);
    let (a, b) = tokio::join!(
        left.revise_episode(
            root.identity.episode_id,
            root.identity.edition_id,
            reason(),
            request("edit-a", b"left correction")
        ),
        right.revise_episode(
            root.identity.episode_id,
            root.identity.edition_id,
            reason(),
            request("edit-b", b"right correction")
        ),
    );
    let winner = match (a, b) {
        (Ok(winner), Err(Error::Conflict(_))) | (Err(Error::Conflict(_)), Ok(winner)) => winner,
        result => panic!("expected one winner and one stale-head conflict: {result:?}"),
    };
    assert!(!winner.replayed);
    assert_eq!(
        bodies.deleted.load(Ordering::SeqCst),
        0,
        "the body port does not prove exclusive ownership"
    );
    assert_eq!(store.all_nodes(ColdPath::acquire()).await.unwrap().len(), 2);
    let winning_node = store
        .get_node(winner.identity.edition_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!inline.get(winning_node.body()).await.unwrap().is_empty());
}

#[tokio::test]
async fn racing_identical_requests_verify_the_winning_body() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let bodies = Arc::new(RacingBodies {
        inner: Arc::new(InlineStore::new()),
        barrier: Barrier::new(2),
        deleted: AtomicUsize::new(0),
    });
    let clock = Arc::new(TestClock::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let left = memory(store.clone(), bodies.clone(), clock.clone(), calls.clone());
    let right = memory(store.clone(), bodies.clone(), clock, calls);
    let (a, b) = tokio::join!(
        left.append_episode(request("scene", b"body")),
        right.append_episode(request("scene", b"body"))
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.identity, b.identity);
    assert_ne!(a.replayed, b.replayed);
    assert_eq!(
        bodies.deleted.load(Ordering::SeqCst),
        0,
        "the body port does not prove exclusive ownership"
    );
    let node = store
        .get_node(a.identity.edition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bodies.get(node.body()).await.unwrap(), b"body");
}

#[tokio::test]
async fn exported_source_proof_matches_persisted_original_and_historical_revision() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(
        store.clone(),
        Arc::new(InlineStore::new()),
        Arc::new(TestClock::default()),
        Arc::new(AtomicUsize::new(0)),
    );
    let initial_proof = request("scene", b"original").validated_source().unwrap();
    let root = mem
        .append_episode(request("scene", b"original"))
        .await
        .unwrap();
    let first = mem
        .revise_episode(
            root.identity.episode_id,
            root.identity.edition_id,
            reason(),
            request("edit-1", b"first edit"),
        )
        .await
        .unwrap();
    let revision_proof = request("edit-1", b"first edit")
        .validated_revision_source(
            root.identity.episode_id,
            root.identity.edition_id,
            first.identity.revision,
            &reason(),
        )
        .unwrap();
    mem.revise_episode(
        root.identity.episode_id,
        first.identity.edition_id,
        reason(),
        request("edit-2", b"second edit"),
    )
    .await
    .unwrap();
    let replay = mem
        .revise_episode(
            root.identity.episode_id,
            root.identity.edition_id,
            reason(),
            request("edit-1", b"first edit"),
        )
        .await
        .unwrap();
    assert_eq!(replay.identity, first.identity);
    assert!(replay.replayed);
    for (id, proof) in [
        (root.identity.edition_id, initial_proof),
        (first.identity.edition_id, revision_proof),
    ] {
        let node = store.get_node(id).await.unwrap().unwrap();
        let mneme_core::Provenance::External { source } = node.provenance() else {
            panic!("source-authored edition")
        };
        assert_eq!(source, &proof);
    }
}

#[test]
fn pure_source_proof_canonicalizes_links_but_separates_semantic_capture() {
    let a = mneme_core::NodeId(ulid::Ulid::from(10_u128));
    let b = mneme_core::NodeId(ulid::Ulid::from(20_u128));
    let links = [
        CaptureLink::new(a, EdgeKind::Associative, -0.0).unwrap(),
        CaptureLink::new(b, EdgeKind::Transition, 0.7).unwrap(),
    ];
    let reverse = [
        links[1],
        CaptureLink::new(a, EdgeKind::Associative, 0.0).unwrap(),
    ];
    assert_eq!(
        request("scene", b"body")
            .with_links(&links)
            .validated_source()
            .unwrap(),
        request("scene", b"body")
            .with_links(&reverse)
            .validated_source()
            .unwrap()
    );
    let episode = request("scene", b"body").validated_source().unwrap();
    let semantic = Capture::new(
        "codex",
        "scene",
        "fixture://episode",
        Some("test-session"),
        None,
        "The floodgate attempt failed before we found the leak",
        b"body",
        &["mindcraft"],
    )
    .validated_source()
    .unwrap();
    assert_eq!(
        episode.node_id(),
        semantic.node_id(),
        "source keys share canonical identity across lanes"
    );
    assert_ne!(
        episode.request_digest(),
        semantic.request_digest(),
        "lane is part of the source proof"
    );
}

struct ChangedBodyReader {
    inner: Arc<InlineStore>,
    changed: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl BodyStore for ChangedBodyReader {
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
        let mut chunk = self.inner.get_range(body, offset, max_bytes).await?;
        if self.changed.load(Ordering::SeqCst) && !chunk.bytes.is_empty() {
            chunk.bytes[0] ^= 1;
        }
        Ok(chunk)
    }
    async fn put(&self, bytes: &[u8]) -> PortResult<BodyRef> {
        self.inner.put(bytes).await
    }
    async fn delete(&self, body: &BodyRef) -> PortResult<()> {
        self.inner.delete(body).await
    }
}

#[tokio::test]
async fn matching_source_proof_cannot_hide_changed_body_bytes() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let bodies = Arc::new(ChangedBodyReader {
        inner: Arc::new(InlineStore::new()),
        changed: std::sync::atomic::AtomicBool::new(false),
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = memory(
        store,
        bodies.clone(),
        Arc::new(TestClock::default()),
        calls.clone(),
    );
    mem.append_episode(request("scene", b"original"))
        .await
        .unwrap();
    bodies.changed.store(true, Ordering::SeqCst);
    assert!(
        matches!(mem.append_episode(request("scene", b"original")).await, Err(Error::Conflict(message)) if message.contains("body changed"))
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "body failure does not recapture the source"
    );
}

fn contexts(items: &[(&str, &str, Option<&str>)]) -> OccurrenceContexts {
    OccurrenceContexts::new(
        items
            .iter()
            .map(|(namespace, key, label)| {
                OccurrenceContextRef::new(namespace, key, *label).unwrap()
            })
            .collect(),
    )
    .unwrap()
}

#[test]
fn context_free_v1_proof_is_pinned_and_v2_context_parts_are_unambiguous() {
    // Frozen pre-context request recipe: no context must retain these exact bytes.
    let old = request("context-proof", b"body")
        .validated_source()
        .unwrap();
    assert_eq!(old.request_codec(), CaptureRequestCodec::EpisodeV1);
    assert_eq!(
        old.request_digest(),
        [
            28, 216, 120, 72, 253, 209, 159, 80, 129, 117, 129, 130, 68, 174, 218, 4, 92, 149, 240,
            85, 165, 161, 66, 123, 218, 65, 251, 234, 132, 133, 231, 67,
        ]
    );
    let first = request("context-proof", b"body")
        .with_occurrence_contexts(contexts(&[("ab", "c", None)]))
        .validated_source()
        .unwrap();
    let second = request("context-proof", b"body")
        .with_occurrence_contexts(contexts(&[("a", "bc", None)]))
        .validated_source()
        .unwrap();
    assert_eq!(first.request_codec(), CaptureRequestCodec::EpisodeV2);
    assert_eq!(first.node_id(), old.node_id());
    assert_ne!(first.request_digest(), old.request_digest());
    assert_ne!(first.request_digest(), second.request_digest());
    let labelled = request("context-proof", b"body")
        .with_occurrence_contexts(contexts(&[("ab", "c", Some("scene"))]))
        .validated_source()
        .unwrap();
    assert_ne!(first.request_digest(), labelled.request_digest());
    let host_change = request("context-proof", b"body")
        .with_occurrence_contexts(contexts(&[("ab", "c", None)]))
        .with_origin_commit(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
        .unwrap()
        .validated_source()
        .unwrap();
    assert_eq!(
        host_change, first,
        "host provenance is never authored occurrence context"
    );
}

#[tokio::test]
async fn contexts_reorder_replays_but_changes_and_omission_conflict() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let calls = Arc::new(AtomicUsize::new(0));
    let mem = memory(
        store.clone(),
        Arc::new(InlineStore::new()),
        Arc::new(TestClock::default()),
        calls.clone(),
    );
    let authored = contexts(&[
        ("World", "canal", Some("Floodgate")),
        ("Room", "design-chat", None),
    ]);
    let first = mem
        .append_episode(
            request("context-replay", b"body").with_occurrence_contexts(authored.clone()),
        )
        .await
        .unwrap();
    let replay = mem
        .append_episode(
            request("context-replay", b"body").with_occurrence_contexts(contexts(&[
                ("Room", "design-chat", None),
                ("World", "canal", Some("Floodgate")),
            ])),
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.identity, first.identity);
    for changed in [
        request("context-replay", b"body"),
        request("context-replay", b"body").with_occurrence_contexts(contexts(&[
            ("World", "canal", Some("Renamed")),
            ("Room", "design-chat", None),
        ])),
        request("context-replay", b"body").with_occurrence_contexts(contexts(&[
            ("World", "other-canal", Some("Floodgate")),
            ("Room", "design-chat", None),
        ])),
        request("context-replay", b"body").with_occurrence_contexts(contexts(&[
            ("world", "canal", Some("Floodgate")),
            ("Room", "design-chat", None),
        ])),
        request("context-replay", b"body").with_occurrence_contexts(contexts(&[(
            "World",
            "canal",
            Some("Floodgate"),
        )])),
    ] {
        assert!(matches!(
            mem.append_episode(changed).await,
            Err(Error::Conflict(_))
        ));
    }
    let record = mem
        .get_episode(&EpisodeGet {
            episode_id: first.identity.episode_id,
            edition_id: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        record.node.episode().unwrap().occurrence_contexts(),
        Some(&authored)
    );
    let history = mem
        .episode_history(&EpisodeHistoryRequest {
            episode_id: first.identity.episode_id,
            limit: EpisodePageLimit::default(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(
        history.items[0].occurrence_contexts.as_ref(),
        Some(&authored)
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "reorder and conflicting intent do no inference"
    );
}

#[tokio::test]
async fn revision_replaces_context_and_omission_is_unknown_not_inheritance() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(
        store.clone(),
        Arc::new(InlineStore::new()),
        Arc::new(TestClock::default()),
        Arc::new(AtomicUsize::new(0)),
    );
    let authored = contexts(&[("game", "floodgate", Some("First attempt"))]);
    let first = mem
        .append_episode(request("context-root", b"body").with_occurrence_contexts(authored.clone()))
        .await
        .unwrap();
    let unknown = mem
        .revise_episode(
            first.identity.episode_id,
            first.identity.edition_id,
            reason(),
            request("context-unknown", b"corrected"),
        )
        .await
        .unwrap();
    let current = mem
        .get_episode(&EpisodeGet {
            episode_id: first.identity.episode_id,
            edition_id: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.node.episode().unwrap().occurrence_contexts(), None);
    match current.node.provenance() {
        mneme_core::Provenance::External { source } => {
            assert_eq!(source.request_codec(), CaptureRequestCodec::EpisodeV1)
        }
        _ => panic!("source authored edition"),
    }
    let replacement = contexts(&[("game", "second-gate", None)]);
    let replaced = mem
        .revise_episode(
            first.identity.episode_id,
            unknown.identity.edition_id,
            reason(),
            request("context-replaced", b"reframed").with_occurrence_contexts(replacement.clone()),
        )
        .await
        .unwrap();
    let current = mem
        .get_episode(&EpisodeGet {
            episode_id: first.identity.episode_id,
            edition_id: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.identity, replaced.identity);
    assert_eq!(
        current.node.episode().unwrap().occurrence_contexts(),
        Some(&replacement)
    );
    let exact = mem
        .get_episode(&EpisodeGet {
            episode_id: first.identity.episode_id,
            edition_id: Some(first.identity.edition_id),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        exact.node.episode().unwrap().occurrence_contexts(),
        Some(&authored)
    );
    let replay = mem
        .append_episode(request("context-root", b"body").with_occurrence_contexts(authored))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.identity, first.identity);
}

#[tokio::test]
async fn occurrence_contexts_do_not_derive_from_recorder_provenance_or_thread() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(
        store,
        Arc::new(InlineStore::new()),
        Arc::new(TestClock::default()),
        Arc::new(AtomicUsize::new(0)),
    );
    let authored = contexts(&[("place", "opaque/location", Some("Shared scene"))]);
    let first = mem
        .append_episode(request("recorder-a", b"body").with_occurrence_contexts(authored.clone()))
        .await
        .unwrap();
    let mut other = request("recorder-b", b"body").with_occurrence_contexts(authored.clone());
    other.namespace = "other-recorder";
    other.session = Some("different-session");
    other.reference = "fixture://other-source";
    other.thread = Some(EpisodeThread::new("unrelated-opaque-label").unwrap());
    let second = mem.append_episode(other).await.unwrap();
    for identity in [first.identity, second.identity] {
        let record = mem
            .get_episode(&EpisodeGet {
                episode_id: identity.episode_id,
                edition_id: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            record.node.episode().unwrap().occurrence_contexts(),
            Some(&authored)
        );
    }
    let unknown = mem
        .append_episode(request("recorder-no-context", b"body"))
        .await
        .unwrap();
    let record = mem
        .get_episode(&EpisodeGet {
            episode_id: unknown.identity.episode_id,
            edition_id: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        record.node.episode().unwrap().occurrence_contexts(),
        None,
        "source/session/thread presence does not imply occurrence context"
    );
}
