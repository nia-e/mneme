use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use mneme_body::InlineStore;
use mneme_core::ports::{
    BodyChunk, BodyStore, ColdPath, Embedder, Error, GraphStore, Result, SystemClock, VectorIndex,
};
use mneme_core::touchstone::{
    SummarySnapshotDigest, TouchstoneInput, TouchstonePageRequest, TouchstoneReference,
    TouchstoneReferrersRequest, TouchstoneSubject,
};
use mneme_core::{BodyRef, CaptureRequestCodec, EmbeddingFingerprint, Node, NodeId, NodeStatus};
use mneme_cozo::MemStore;
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::{Capture, CaptureLink, Config, EpisodeWrite, Memory};
use ulid::Ulid;

struct CountingEmbedder(Arc<AtomicUsize>);
#[async_trait]
impl Embedder for CountingEmbedder {
    fn dim(&self) -> usize {
        DEFAULT_DIM
    }
    fn fingerprint(&self) -> EmbeddingFingerprint {
        HashingEmbedder::new(DEFAULT_DIM).fingerprint()
    }
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        HashingEmbedder::new(DEFAULT_DIM).embed(texts).await
    }
}

struct CountingBodies {
    inner: InlineStore,
    writes: Arc<AtomicUsize>,
}
#[async_trait]
impl BodyStore for CountingBodies {
    fn scheme(&self) -> &'static str {
        "inline"
    }
    async fn get(&self, body: &BodyRef) -> Result<Vec<u8>> {
        self.inner.get(body).await
    }
    async fn get_range(&self, body: &BodyRef, offset: u64, max_bytes: usize) -> Result<BodyChunk> {
        self.inner.get_range(body, offset, max_bytes).await
    }
    async fn put(&self, bytes: &[u8]) -> Result<BodyRef> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.put(bytes).await
    }
    async fn delete(&self, body: &BodyRef) -> Result<()> {
        self.inner.delete(body).await
    }
}

fn memory(store: Arc<MemStore>) -> (Memory, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let embeddings = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let memory = Memory::new(
        store.clone(),
        store.clone(),
        store,
        Arc::new(CountingEmbedder(embeddings.clone())),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(Arc::new(CountingBodies {
        inner: InlineStore::new(),
        writes: writes.clone(),
    }));
    (memory, embeddings, writes)
}

fn request(key: &str) -> Capture<'_> {
    Capture::new(
        "touchstone-test",
        key,
        "fixture://touchstone",
        None,
        None,
        "The conversation that changed our checklist",
        b"private full body, deliberately not part of the reference snapshot",
        &["touchstone"],
    )
}

fn input(references: Vec<TouchstoneReference>) -> TouchstoneInput {
    TouchstoneInput::new(TouchstoneSubject::new("project:mneme").unwrap(), references).unwrap()
}

async fn reference(memory: &Memory, id: NodeId) -> TouchstoneReference {
    let snapshot = memory.summary_snapshot(id).await.unwrap().unwrap();
    TouchstoneReference::new(snapshot.db_id(), id, snapshot.digest())
}

#[test]
fn touchstone_request_codec_has_an_explicit_golden_and_no_legacy_replay() {
    let make = |id, byte| {
        TouchstoneReference::new(
            Ulid::from(0u128),
            NodeId(Ulid::from(id)),
            SummarySnapshotDigest::from_hex(&format!("{byte:02x}").repeat(32)).unwrap(),
        )
    };
    let authored = input(vec![make(2u128, 0x22u8), make(1u128, 0x11u8)]);
    let source = request("golden-owner")
        .with_touchstone(authored.clone())
        .validated_source()
        .unwrap();
    assert_eq!(
        source
            .request_digest()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "220fafe273bcc5d7432939422076d0f8d1b42965a699298cd60fbaaecc387749"
    );
    let proof = request("golden-owner")
        .with_touchstone(authored)
        .validated_replay_proof()
        .unwrap();
    assert!(proof.matches_source(&source));
    let bare = request("golden-owner").validated_source().unwrap();
    assert!(!proof.matches_source(&bare));
    let predecessor = mneme_core::CaptureSource::new_with_codec(
        source.namespace(),
        source.key(),
        source.reference(),
        source.session(),
        source.revision(),
        source.request_digest(),
        CaptureRequestCodec::CaptureV1,
    )
    .unwrap();
    assert!(!proof.matches_source(&predecessor));
}

#[tokio::test]
async fn semantic_input_failures_are_pure_and_self_reference_never_enters_io() {
    let subject = TouchstoneSubject::new("project:mneme").unwrap();
    assert!(TouchstoneSubject::new("").is_err());
    assert!(TouchstoneSubject::new(&"x".repeat(257)).is_err());
    assert!(TouchstoneInput::new(subject.clone(), vec![]).is_err());
    let digest = SummarySnapshotDigest::from_hex(&"11".repeat(32)).unwrap();
    let reference = TouchstoneReference::new(Ulid::from(0u128), NodeId(Ulid::from(1u128)), digest);
    assert!(TouchstoneInput::new(subject.clone(), vec![reference.clone(), reference]).is_err());
    let too_large: Vec<_> = (1..=256u128)
        .map(|id| TouchstoneReference::new(Ulid::from(0u128), NodeId(Ulid::from(id)), digest))
        .collect();
    assert!(TouchstoneInput::new(subject, too_large).is_err());
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let (memory, embeddings, writes) = memory(store);
    let owner_id = request("self-ref").validated_source().unwrap().node_id();
    let authored = input(vec![TouchstoneReference::new(
        Ulid::from(0u128),
        owner_id,
        digest,
    )]);
    assert!(matches!(
        memory
            .capture(request("self-ref").with_touchstone(authored))
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(embeddings.load(Ordering::SeqCst), 0);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn canonical_reference_order_preserves_request_identity_and_authored_links() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let (memory, embeddings, writes) = memory(store.clone());
    let a = memory.capture(request("target-a")).await.unwrap().id;
    let b = memory.capture(request("target-b")).await.unwrap().id;
    let references = vec![reference(&memory, a).await, reference(&memory, b).await];
    let mut reordered = references.clone();
    reordered.reverse();
    let links = [CaptureLink::new(a, mneme_core::EdgeKind::DerivedFrom, 0.3).unwrap()];
    let first_source = request("owner")
        .with_links(&links)
        .with_touchstone(input(references.clone()))
        .validated_source()
        .unwrap();
    let retry_source = request("owner")
        .with_links(&links)
        .with_touchstone(input(reordered.clone()))
        .validated_source()
        .unwrap();
    assert_eq!(first_source, retry_source);
    assert_eq!(
        first_source.request_codec(),
        CaptureRequestCodec::TouchstoneV1
    );
    assert_ne!(
        first_source.request_digest(),
        request("owner")
            .with_links(&links)
            .validated_source()
            .unwrap()
            .request_digest()
    );
    let first = memory
        .capture(
            request("owner")
                .with_links(&links)
                .with_touchstone(input(references)),
        )
        .await
        .unwrap();
    assert!(!first.replayed);
    assert_eq!(
        store.get_edge(first.id, a).await.unwrap().unwrap().kind,
        mneme_core::EdgeKind::DerivedFrom
    );
    let retry = memory
        .capture(
            request("owner")
                .with_links(&links)
                .with_touchstone(input(reordered)),
        )
        .await
        .unwrap();
    assert_eq!(retry.id, first.id);
    assert!(retry.replayed);
    assert_eq!(embeddings.load(Ordering::SeqCst), 3);
    assert_eq!(writes.load(Ordering::SeqCst), 3);
    assert!(
        memory.get_touchstone(a).await.unwrap().is_none(),
        "a tag is not typed metadata"
    );
}

#[tokio::test]
async fn exact_replay_after_target_edit_archive_and_delete_retains_original_snapshot() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let (memory, embeddings, writes) = memory(store.clone());
    let target = memory.capture(request("mutable-target")).await.unwrap().id;
    let original_reference = reference(&memory, target).await;
    let authored = input(vec![original_reference]);
    let owner = memory
        .capture(request("historical-owner").with_touchstone(authored.clone()))
        .await
        .unwrap()
        .id;
    let original =
        serde_json::to_value(memory.get_touchstone(owner).await.unwrap().unwrap()).unwrap();
    let node = store.get_node(target).await.unwrap().unwrap();
    let edited = Node::try_new(
        target,
        "A better checklist, not the conversation that mattered",
        node.body().clone(),
        node.tags(),
        node.provenance().clone(),
        0.5,
        0.5,
        NodeStatus::Active,
        node.created(),
    )
    .unwrap();
    store.put_node(&edited).await.unwrap();
    store
        .set_status(target, NodeStatus::Archived)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(memory.get_touchstone(owner).await.unwrap().unwrap()).unwrap(),
        original
    );
    store.delete_node(target).await.unwrap();
    store.remove(target).await.unwrap();
    let retry = memory
        .capture(request("historical-owner").with_touchstone(authored))
        .await
        .unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.id, owner);
    assert_eq!(embeddings.load(Ordering::SeqCst), 2);
    assert_eq!(writes.load(Ordering::SeqCst), 2);
    assert_eq!(
        serde_json::to_value(memory.get_touchstone(owner).await.unwrap().unwrap()).unwrap(),
        original
    );
}

#[tokio::test]
async fn foreign_database_is_rejected_before_inference_or_body_write() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let (memory, embeddings, writes) = memory(store);
    let target = memory.capture(request("local-target")).await.unwrap().id;
    let local = reference(&memory, target).await;
    let foreign = TouchstoneReference::new(
        Ulid::from(u128::from(local.db_id()).wrapping_add(1)),
        target,
        local.expected_snapshot_sha256(),
    );
    assert!(matches!(
        memory
            .capture(request("foreign-owner").with_touchstone(input(vec![foreign])))
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(embeddings.load(Ordering::SeqCst), 1);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stale_snapshot_refusal_is_atomic_and_same_key_can_retry_correctly() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let (memory, _, _) = memory(store.clone());
    let target = memory.capture(request("snapshot-target")).await.unwrap().id;
    let correct = reference(&memory, target).await;
    let wrong = TouchstoneReference::new(
        correct.db_id(),
        target,
        SummarySnapshotDigest::from_hex(&"00".repeat(32)).unwrap(),
    );
    let owner_id = request("refused-owner")
        .validated_source()
        .unwrap()
        .node_id();
    assert!(
        memory
            .capture(request("refused-owner").with_touchstone(input(vec![wrong])))
            .await
            .is_err()
    );
    assert!(store.get_node(owner_id).await.unwrap().is_none());
    assert!(memory.get_touchstone(owner_id).await.unwrap().is_none());
    let incoming = memory
        .touchstone_referrers_page(&TouchstoneReferrersRequest::new(target, None, 32).unwrap())
        .await
        .unwrap();
    assert!(incoming.items.is_empty());
    let saved = memory
        .capture(request("refused-owner").with_touchstone(input(vec![correct])))
        .await
        .unwrap();
    assert!(!saved.replayed);
    let incoming = memory
        .touchstone_referrers_page(&TouchstoneReferrersRequest::new(target, None, 32).unwrap())
        .await
        .unwrap();
    assert_eq!(incoming.items.len(), 1);
    assert_eq!(incoming.items[0].id, saved.id);
}

#[tokio::test]
async fn owner_full_merge_refused_and_delete_cleans_inverse_rows() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let (memory, _, _) = memory(store.clone());
    let target = memory.capture(request("merge-target")).await.unwrap().id;
    let owner = memory
        .capture(
            request("immutable-owner")
                .with_touchstone(input(vec![reference(&memory, target).await])),
        )
        .await
        .unwrap()
        .id;
    assert!(matches!(
        memory.merge_full(ColdPath::acquire(), owner, target).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        memory.merge_full(ColdPath::acquire(), target, owner).await,
        Err(Error::InvalidInput(_))
    ));
    let original = store.get_node(owner).await.unwrap().unwrap();
    let changed = Node::try_new(
        owner,
        "generic replacement",
        original.body().clone(),
        original.tags(),
        original.provenance().clone(),
        0.5,
        0.5,
        NodeStatus::Active,
        original.created(),
    )
    .unwrap();
    assert!(store.put_node(&changed).await.is_err());
    store.delete_node(owner).await.unwrap();
    assert!(memory.get_touchstone(owner).await.unwrap().is_none());
    assert!(
        memory
            .touchstone_referrers_page(&TouchstoneReferrersRequest::new(target, None, 32).unwrap())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        memory
            .touchstones_page(&TouchstonePageRequest::new(None, None, 32).unwrap())
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn episode_correction_does_not_replace_the_referenced_exact_edition() {
    use mneme_core::episode::{EpisodeRevisionReason, OccurrenceSpan};
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let (memory, _, _) = memory(store);
    let scene = |key| {
        EpisodeWrite::new(
            "touchstone-test",
            key,
            "fixture://scene",
            None,
            None,
            "The first attempt at the floodgate",
            b"full scene body",
            &[],
            OccurrenceSpan::Unknown,
            None,
        )
    };
    let first = memory
        .append_episode(scene("scene-original"))
        .await
        .unwrap();
    let original_reference = reference(&memory, first.identity.edition_id).await;
    let owner = memory
        .capture(request("scene-owner").with_touchstone(input(vec![original_reference])))
        .await
        .unwrap()
        .id;
    let original_record =
        serde_json::to_value(memory.get_touchstone(owner).await.unwrap().unwrap()).unwrap();
    let second = memory
        .revise_episode(
            first.identity.episode_id,
            first.identity.edition_id,
            EpisodeRevisionReason::new("The build log changed the chronology").unwrap(),
            scene("scene-correction"),
        )
        .await
        .unwrap();
    assert_ne!(first.identity.edition_id, second.identity.edition_id);
    assert_eq!(
        serde_json::to_value(memory.get_touchstone(owner).await.unwrap().unwrap()).unwrap(),
        original_record
    );
    let original_incoming = memory
        .touchstone_referrers_page(
            &TouchstoneReferrersRequest::new(first.identity.edition_id, None, 32).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(original_incoming.items[0].id, owner);
    let corrected_incoming = memory
        .touchstone_referrers_page(
            &TouchstoneReferrersRequest::new(second.identity.edition_id, None, 32).unwrap(),
        )
        .await
        .unwrap();
    assert!(corrected_incoming.items.is_empty());
}

#[tokio::test]
async fn indexed_collection_and_reverse_reads_page_the_complete_owner_set() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let (memory, _, _) = memory(store);
    let target = memory.capture(request("page-target")).await.unwrap().id;
    let target_reference = reference(&memory, target).await;
    let mut expected = Vec::new();
    for key in ["page-owner-a", "page-owner-b", "page-owner-c"] {
        expected.push(
            memory
                .capture(request(key).with_touchstone(input(vec![target_reference.clone()])))
                .await
                .unwrap()
                .id,
        );
    }
    expected.sort();
    let mut actual = Vec::new();
    let mut after = None;
    loop {
        let page = memory
            .touchstones_page(&TouchstonePageRequest::new(None, after, 1).unwrap())
            .await
            .unwrap();
        assert!(page.items.len() <= 1);
        actual.extend(page.items.into_iter().map(|header| header.id));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(actual, expected);
    let mut actual = Vec::new();
    let mut after = None;
    loop {
        let page = memory
            .touchstone_referrers_page(&TouchstoneReferrersRequest::new(target, after, 1).unwrap())
            .await
            .unwrap();
        assert!(page.items.len() <= 1);
        actual.extend(page.items.into_iter().map(|header| header.id));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(actual, expected);
}
