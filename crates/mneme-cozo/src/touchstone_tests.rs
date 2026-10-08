use super::*;
use mneme_core::ports::CapturePriorBudget;
use mneme_core::{BodyRef, CaptureRequestCodec};

pub(crate) fn target(id: NodeId, summary: &str) -> Node {
    Node::try_new(
        id,
        summary,
        BodyRef::new("inline://historical-body-not-copied").unwrap(),
        ["scene"],
        Provenance::derived_empty(),
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap()
}
pub(crate) fn authored(key: &str, input: &TouchstoneInput) -> (Node, CaptureReplayProof) {
    use sha2::{Digest, Sha256};
    let digest: [u8; 32] = Sha256::digest(input.canonical_bytes()).into();
    let source = CaptureSource::new_with_codec(
        "touchstone-storage-test",
        key,
        "test://touchstone",
        None,
        None,
        digest,
        CaptureRequestCodec::TouchstoneV1,
    )
    .unwrap();
    let proof = CaptureReplayProof::touchstone(source.clone()).unwrap();
    let node = Node::try_new(
        source.node_id(),
        "The first conversation that made the project feel alive",
        BodyRef::new("inline://touchstone-owner").unwrap(),
        ["touchstone"],
        Provenance::External { source },
        0.5,
        0.5,
        NodeStatus::Active,
        2,
    )
    .unwrap();
    (node, proof)
}
pub(crate) fn input(db: Ulid, node: &Node) -> TouchstoneInput {
    let snapshot = SummarySnapshot::from_node(db, node).unwrap();
    TouchstoneInput::new(
        TouchstoneSubject::new("the project becoming alive").unwrap(),
        vec![TouchstoneReference::new(db, node.id(), snapshot.digest())],
    )
    .unwrap()
}
pub(crate) async fn capture(
    store: &dyn GraphStore,
    node: &Node,
    proof: &CaptureReplayProof,
    input: &TouchstoneInput,
) -> Result<CaptureCommitOutcome> {
    store
        .commit_capture_with_touchstone(
            node,
            &[1.0, 0.0, 0.0, 0.0],
            &[],
            &[],
            CapturePriorBudget::new(0),
            proof,
            Some(input),
        )
        .await
}
async fn preservation(store: &dyn GraphStore) {
    let typed = store.touchstones().unwrap();
    let db = typed.database_id().unwrap();
    let original = target(NodeId(Ulid::new()), "historical target summary");
    store.put_node(&original).await.unwrap();
    let input = input(db, &original);
    let (owner, proof) = authored("preservation", &input);
    assert_eq!(
        capture(store, &owner, &proof, &input).await.unwrap(),
        CaptureCommitOutcome::Applied
    );
    assert!(matches!(
        store
            .compare_replace_node_body(
                owner.id(),
                &owner.body_revision(),
                &BodyRef::new("inline://replacement").unwrap()
            )
            .await,
        Err(Error::Conflict(_))
    ));
    let immutable = typed.get_touchstone(owner.id()).await.unwrap().unwrap();
    let serialized = serde_json::to_value(&immutable).unwrap();
    let text = serde_json::to_string(&serialized).unwrap();
    for forbidden in ["body", "tags", "status", "confidence", "head", "edges"] {
        assert!(serialized["references"][0].get(forbidden).is_none());
    }
    assert!(!text.contains("historical-body-not-copied"));
    assert_eq!(
        immutable.references()[0].summary().as_str(),
        original.summary()
    );

    // Generic overwrite refuses authored content; lifecycle and exact writes work.
    let mut changed = serde_json::to_value(&owner).unwrap();
    changed["summary"] = "rewrite the conversation into a checklist".into();
    assert!(matches!(
        store
            .put_node(&serde_json::from_value(changed).unwrap())
            .await,
        Err(Error::Conflict(_))
    ));
    store.put_node(&owner).await.unwrap();
    store
        .set_status(owner.id(), NodeStatus::Archived)
        .await
        .unwrap();
    assert_eq!(
        typed.get_touchstone(owner.id()).await.unwrap().unwrap(),
        immutable
    );
    let request = TouchstoneReferrersRequest::new(original.id(), None, 1).unwrap();
    let page = typed.touchstone_referrers_page(&request).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, owner.id());
    assert!(page.next.is_some());
    let tail = typed
        .touchstone_referrers_page(
            &TouchstoneReferrersRequest::new(original.id(), page.next, 1).unwrap(),
        )
        .await
        .unwrap();
    assert!(tail.items.is_empty());
    assert!(tail.next.is_none());

    // Filtering consumes a bounded owner slice, not filter-then-limit. An empty
    // filtered page must advance after the examined owner rather than claiming
    // inventory exhaustion.
    let different = TouchstoneSubject::new("not this subject").unwrap();
    let empty = typed
        .touchstones_page(&TouchstonePageRequest::new(Some(different.clone()), None, 1).unwrap())
        .await
        .unwrap();
    assert!(empty.items.is_empty());
    assert!(empty.next.is_some());
    let tail = typed
        .touchstones_page(&TouchstonePageRequest::new(Some(different), empty.next, 1).unwrap())
        .await
        .unwrap();
    assert!(tail.items.is_empty());
    assert!(tail.next.is_none());

    store
        .put_node(&target(original.id(), "current changed summary"))
        .await
        .unwrap();
    assert_eq!(
        capture(store, &owner, &proof, &input).await.unwrap(),
        CaptureCommitOutcome::AlreadyApplied
    );
    store.delete_node(original.id()).await.unwrap();
    assert_eq!(
        capture(store, &owner, &proof, &input).await.unwrap(),
        CaptureCommitOutcome::AlreadyApplied
    );
    assert_eq!(
        store.lookup_capture(&proof).await.unwrap(),
        Some(owner.id())
    );
    assert_eq!(
        typed.get_touchstone(owner.id()).await.unwrap().unwrap(),
        immutable
    );
    assert_eq!(
        typed
            .touchstone_referrers_page(&request)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    store.delete_node(owner.id()).await.unwrap();
    assert!(typed.get_touchstone(owner.id()).await.unwrap().is_none());
    assert!(
        typed
            .touchstone_referrers_page(&request)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
async fn refusals(store: &dyn GraphStore) {
    let typed = store.touchstones().unwrap();
    let db = typed.database_id().unwrap();
    let original = target(NodeId(Ulid::new()), "expected summary");
    store.put_node(&original).await.unwrap();
    let stale = input(db, &original);
    let (owner, proof) = authored("atomic-stale", &stale);
    store
        .put_node(&target(original.id(), "now different"))
        .await
        .unwrap();
    assert!(matches!(
        capture(store, &owner, &proof, &stale).await,
        Err(Error::Conflict(_))
    ));
    assert!(store.get_node(owner.id()).await.unwrap().is_none());
    assert!(typed.get_touchstone(owner.id()).await.unwrap().is_none());
    assert!(
        typed
            .touchstone_referrers_page(
                &TouchstoneReferrersRequest::new(original.id(), None, 32).unwrap()
            )
            .await
            .unwrap()
            .items
            .is_empty()
    );
    store.put_node(&original).await.unwrap();
    assert_eq!(
        capture(store, &owner, &proof, &stale).await.unwrap(),
        CaptureCommitOutcome::Applied
    );
    store
        .observe_merge_candidate(owner.id(), original.id(), 3)
        .await
        .unwrap();
    assert!(matches!(
        store
            .commit_full_merge(&FullMergeCommit::new(owner.id(), original.id(), 4).unwrap())
            .await,
        Err(Error::Conflict(_))
    ));
    let foreign = input(Ulid::new(), &original);
    let (foreign_owner, foreign_proof) = authored("foreign", &foreign);
    assert!(matches!(
        capture(store, &foreign_owner, &foreign_proof, &foreign).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(store.get_node(foreign_owner.id()).await.unwrap().is_none());
}
#[tokio::test]
async fn memory_touchstone_replay_lifecycle_reverse_and_refusals() {
    preservation(&MemStore::new(4)).await;
    refusals(&MemStore::new(4)).await;
}
#[cfg(feature = "cozo")]
#[tokio::test]
async fn native_touchstone_replay_lifecycle_reverse_and_refusals() {
    preservation(&CozoStore::new(4).unwrap()).await;
    refusals(&CozoStore::new(4).unwrap()).await;
}
#[tokio::test]
async fn touchstone_json_successor_roundtrip_and_frozen_predecessor() {
    let store = MemStore::new(4);
    let node = target(NodeId(Ulid::new()), "a historical source");
    store.put_node(&node).await.unwrap();
    let input = input(store.db_id(), &node);
    let (owner, proof) = authored("json-roundtrip", &input);
    capture(&store, &owner, &proof, &input).await.unwrap();
    store.delete_node(node.id()).await.unwrap();
    let path = std::env::temp_dir().join(format!("mneme-touchstones-json-{}.json", Ulid::new()));
    store.save(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["schema"], STORE_EXPORT_SCHEMA_V5);
    assert!(serde_json::from_slice::<StoreExportEnvelopeV4>(&bytes).is_err());
    let reopened = MemStore::load(&path).unwrap();
    assert_eq!(
        store.export().canonical_value().unwrap(),
        reopened.export().canonical_value().unwrap()
    );
    assert_eq!(
        capture(&reopened, &owner, &proof, &input).await.unwrap(),
        CaptureCommitOutcome::AlreadyApplied
    );
    assert!(store.save_episode_context_v4(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let empty = MemStore::new(4);
    empty.save_episode_context_v4(&path).unwrap();
    assert!(MemStore::load(&path).is_err());
    assert_eq!(
        MemStore::load_episode_context_v4(&path).unwrap().db_id(),
        empty.db_id()
    );
    std::fs::remove_file(path).unwrap();
}

async fn edition_and_collection(store: &dyn GraphStore) {
    use mneme_core::episode::*;
    let source = CaptureSource::new_with_codec(
        "touchstone-edition",
        "first",
        "test://edition",
        None,
        None,
        [1; 32],
        CaptureRequestCodec::EpisodeV1,
    )
    .unwrap();
    let scene = Node::try_new(
        source.node_id(),
        "the exact first account",
        BodyRef::new("inline://scene").unwrap(),
        ["scene"],
        Provenance::External { source },
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    let facet = EpisodeFacet::initial(
        scene.id(),
        OccurrenceSpan::Unknown,
        None,
        EpisodeTime::new(1).unwrap(),
    )
    .unwrap();
    let scene = scene.with_episode(facet).unwrap();
    let episodes = store.episodes().unwrap();
    episodes
        .commit_episode(EpisodeCommit {
            node: &scene,
            embedding: &[1., 0., 0., 0.],
            links: &[],
            expectation: EpisodeWriteExpectation::NewRoot,
        })
        .await
        .unwrap();
    let typed = store.touchstones().unwrap();
    let db = typed.database_id().unwrap();
    let input = input(db, &scene);
    let (owner, proof) = authored("edition-before-correction", &input);
    capture(store, &owner, &proof, &input).await.unwrap();
    let original = typed.get_touchstone(owner.id()).await.unwrap().unwrap();
    let source = CaptureSource::new_with_codec(
        "touchstone-edition",
        "second",
        "test://edition",
        None,
        None,
        [2; 32],
        CaptureRequestCodec::EpisodeV1,
    )
    .unwrap();
    let revised = Node::try_new(
        source.node_id(),
        "a corrected account",
        BodyRef::new("inline://scene-v2").unwrap(),
        ["scene"],
        Provenance::External { source },
        0.5,
        0.5,
        NodeStatus::Active,
        2,
    )
    .unwrap();
    let before = scene.episode().unwrap();
    let facet = EpisodeFacet::revised(
        before.root(),
        scene.id(),
        before.revision().next().unwrap(),
        before.occurrence().clone(),
        None,
        before.recorded_at(),
        EpisodeRevisionReason::new("correct this account").unwrap(),
    )
    .unwrap();
    let revised = revised.with_episode(facet).unwrap();
    episodes
        .commit_episode(EpisodeCommit {
            node: &revised,
            embedding: &[1., 0., 0., 0.],
            links: &[],
            expectation: EpisodeWriteExpectation::CurrentEdition(scene.id()),
        })
        .await
        .unwrap();
    assert_eq!(
        typed.get_touchstone(owner.id()).await.unwrap().unwrap(),
        original
    );
    assert_eq!(original.references()[0].memory_kind(), scene.memory_kind());
    assert_eq!(original.references()[0].id(), scene.id());
    assert!(
        typed
            .touchstone_referrers_page(
                &TouchstoneReferrersRequest::new(revised.id(), None, 32).unwrap()
            )
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        capture(store, &owner, &proof, &input).await.unwrap(),
        CaptureCommitOutcome::AlreadyApplied
    );

    // More than one maximum-size collection page: this is not a top-k query.
    for index in 0..34 {
        let (next, proof) = authored(&format!("collection-{index}"), &input);
        capture(store, &next, &proof, &input).await.unwrap();
    }
    let mut after = None;
    let mut seen = BTreeSet::new();
    loop {
        let page = typed
            .touchstones_page(&TouchstonePageRequest::new(None, after, 32).unwrap())
            .await
            .unwrap();
        for header in page.items {
            assert!(seen.insert(header.id));
        }
        match page.next {
            None => break,
            Some(cursor) => after = Some(cursor),
        }
    }
    assert_eq!(seen.len(), 35);
}
#[tokio::test]
async fn memory_touchstone_exact_edition_and_collection_keysets() {
    edition_and_collection(&MemStore::new(4)).await;
}
#[cfg(feature = "cozo")]
#[tokio::test]
async fn native_touchstone_exact_edition_and_collection_keysets() {
    edition_and_collection(&CozoStore::new(4).unwrap()).await;
}

#[tokio::test]
async fn assembled_touchstone_exports_refuse_torn_owner_record_and_vector_joins() {
    let store = MemStore::new(4);
    let scene = target(NodeId(Ulid::new()), "historical source");
    store.put_node(&scene).await.unwrap();
    let input = input(store.db_id(), &scene);
    let (owner, proof) = authored("torn-export", &input);
    capture(&store, &owner, &proof, &input).await.unwrap();
    let complete = store.export();
    mem_touchstones::validate_touchstone_import(&complete).unwrap();
    let mut orphan_record = complete.clone();
    orphan_record.nodes.retain(|node| node.id() != owner.id());
    assert!(mem_touchstones::validate_touchstone_import(&orphan_record).is_err());
    assert!(MemStore::from_export(orphan_record).is_err());
    let mut orphan_codec = complete.clone();
    orphan_codec.touchstones.clear();
    assert!(mem_touchstones::validate_touchstone_import(&orphan_codec).is_err());
    assert!(MemStore::from_export(orphan_codec).is_err());
    let mut missing_vector = complete;
    missing_vector.vectors.retain(|(id, _)| *id != owner.id());
    assert!(mem_touchstones::validate_touchstone_import(&missing_vector).is_err());
    assert!(MemStore::from_export(missing_vector).is_err());
}
