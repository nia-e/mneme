use super::*;
use mneme_core::{BodyRef, CaptureSource};

pub(super) fn episode_kernel_store() -> CozoStore {
    CozoStore::new(4).unwrap()
}

pub(super) fn episode_node(key: &str, summary: &str, at: Timestamp) -> Node {
    let source = CaptureSource::new_with_codec(
        "episode-test",
        key,
        "file:///synthetic",
        None::<&str>,
        None::<&str>,
        [1; 32],
        mneme_core::CaptureRequestCodec::EpisodeV1,
    )
    .unwrap();
    let node = Node::try_new(
        source.node_id(),
        summary,
        BodyRef::new("inline://synthetic").unwrap(),
        ["scene"],
        Provenance::External { source },
        0.5,
        0.5,
        NodeStatus::Active,
        at,
    )
    .unwrap();
    let facet = EpisodeFacet::initial(
        node.id(),
        OccurrenceSpan::Unknown,
        None,
        EpisodeTime::new(at).unwrap(),
    )
    .unwrap();
    node.with_episode(facet).unwrap()
}

const EMBEDDING: [f32; 4] = [1., 0., 0., 0.];
async fn append(store: &CozoStore, node: &Node) -> Result<EpisodeCommitOutcome> {
    store
        .commit_episode(EpisodeCommit {
            node,
            embedding: &EMBEDDING,
            links: &[],
            expectation: EpisodeWriteExpectation::NewRoot,
        })
        .await
}
fn revision(prior: &Node, key: &str, summary: &str) -> Node {
    let root = prior.episode().unwrap();
    let mut node = episode_node(key, summary, prior.created() + 1);
    let facet = EpisodeFacet::revised(
        root.root(),
        prior.id(),
        root.revision().next().unwrap(),
        OccurrenceSpan::Unknown,
        None,
        root.recorded_at(),
        EpisodeRevisionReason::new("Correction after checking the actual experiment").unwrap(),
    )
    .unwrap();
    // Rebuild from semantic initialization because with_episode rejects replacing a facet.
    let source = episode_source(&node).unwrap().clone();
    node = Node::try_new(
        source.node_id(),
        summary,
        BodyRef::new("inline://synthetic").unwrap(),
        ["scene"],
        Provenance::External { source },
        0.5,
        0.5,
        NodeStatus::Active,
        prior.created() + 1,
    )
    .unwrap();
    node.with_episode(facet).unwrap()
}

#[tokio::test]
async fn episode_atomic_append_revise_replay_and_stale_head() {
    let store = episode_kernel_store();
    let first = episode_node("scene", "I thought the valve was open", 10);
    assert!(matches!(
        append(&store, &first).await.unwrap(),
        EpisodeCommitOutcome::Applied(_)
    ));
    let corrected = revision(&first, "scene-edit", "The valve was shut");
    let write = || EpisodeCommit {
        node: &corrected,
        embedding: &EMBEDDING,
        links: &[],
        expectation: EpisodeWriteExpectation::CurrentEdition(first.id()),
    };
    assert!(matches!(
        store.commit_episode(write()).await.unwrap(),
        EpisodeCommitOutcome::Applied(_)
    ));
    assert!(matches!(
        store.commit_episode(write()).await.unwrap(),
        EpisodeCommitOutcome::AlreadyApplied(_)
    ));
    assert!(matches!(
        append(&store, &first).await.unwrap(),
        EpisodeCommitOutcome::AlreadyApplied(_)
    ));
    let stale = revision(&first, "stale-edit", "This obsolete edit must conflict");
    assert!(matches!(
        store
            .commit_episode(EpisodeCommit {
                node: &stale,
                embedding: &EMBEDDING,
                links: &[],
                expectation: EpisodeWriteExpectation::CurrentEdition(first.id())
            })
            .await,
        Err(Error::Conflict(_))
    ));
    assert!(store.get_node(stale.id()).await.unwrap().is_none());
    assert_eq!(
        store.get_node(first.id()).await.unwrap().unwrap().summary(),
        first.summary()
    );
    let got = store
        .get_episode(&EpisodeGet {
            episode_id: EpisodeId::new(first.id()),
            edition_id: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.node.id(), corrected.id());
    store
        .verify_episode_import(&store.export().await.unwrap())
        .unwrap();
}

#[tokio::test]
async fn episode_every_write_cut_rolls_back_without_proof_or_projection() {
    let store = episode_kernel_store();
    for step in 1..=5 {
        let node = episode_node(
            &format!("failure-{step}"),
            "An episode that must commit all or nothing",
            100 + step as Timestamp,
        );
        failures().lock().unwrap().insert(node.id(), step);
        assert!(
            append(&store, &node)
                .await
                .unwrap_err()
                .to_string()
                .contains("injected episode failure")
        );
        assert!(store.get_node(node.id()).await.unwrap().is_none());
        assert!(
            store
                .lookup_episode(episode_source(&node).unwrap())
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            append(&store, &node).await.unwrap(),
            EpisodeCommitOutcome::Applied(_)
        ));
    }
    store
        .verify_episode_import(&store.export().await.unwrap())
        .unwrap();
}

#[tokio::test]
async fn episode_semantic_corpus_excludes_before_topk_and_vectors_stay_exported() {
    let store = episode_kernel_store();
    let episode = episode_node("exclusion", "lantern scene", 1);
    append(&store, &episode).await.unwrap();
    let semantic = Node::try_new(
        NodeId(Ulid::new()),
        "lantern lesson",
        BodyRef::new("inline://lesson").unwrap(),
        ["scene"],
        Provenance::Conversation {
            session: Ulid::new(),
            turn: 1,
        },
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    store.put_node(&semantic).await.unwrap();
    store
        .upsert(semantic.id(), &[0.9, 0.1, 0., 0.])
        .await
        .unwrap();
    let ann = store.ann(&EMBEDDING, 1, StatusFilter::ALL).await.unwrap();
    assert_eq!(ann[0].id, semantic.id());
    let lexical = store.search("lantern", 1, StatusFilter::ALL).await.unwrap();
    assert_eq!(lexical[0].id, semantic.id());
    assert_eq!(
        store
            .get_node_statuses(&[episode.id(), semantic.id()])
            .await
            .unwrap(),
        vec![None, Some(TaggedPhysicalStatus::Active)]
    );
    assert!(store.remove(episode.id()).await.is_err());
    assert!(store.upsert(episode.id(), &EMBEDDING).await.is_err());
    let export = store.export().await.unwrap();
    assert_eq!(export.nodes.len(), 2);
    assert_eq!(export.vectors.len(), 2);
}

#[tokio::test]
async fn episode_orphan_projection_refuses_instead_of_healing() {
    let store = episode_kernel_store();
    let node = episode_node("orphan", "A scene with an incomplete projection", 1);
    let p = BTreeMap::from([
        ("id".into(), dv_str(&node.id().0.to_string())),
        ("e".into(), dv_float_list(&EMBEDDING)),
    ]);
    store
        .run(
            "?[id,e,status] := id=$id,e=vec($e),status='episode' :put node_vec{id=>e,status}",
            p,
            true,
        )
        .unwrap();
    assert!(matches!(
        append(&store, &node).await,
        Err(Error::Conflict(_))
    ));
    assert!(store.get_node(node.id()).await.unwrap().is_none());
}

#[tokio::test]
async fn episode_authored_links_commit_with_edition_and_replay_never_recreates_removed_link() {
    let store = episode_kernel_store();
    let node = episode_node("links", "A scene anchored to a lesson", 1);
    let lesson = Node::try_new(
        NodeId(Ulid::new()),
        "The anchored lesson",
        BodyRef::new("inline://lesson").unwrap(),
        ["lesson"],
        Provenance::Conversation {
            session: Ulid::new(),
            turn: 1,
        },
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    let edge = Edge::new(node.id(), lesson.id(), 0.7, EdgeKind::DerivedFrom, 1);
    let links = [edge];
    let request = || EpisodeCommit {
        node: &node,
        embedding: &EMBEDDING,
        links: &links,
        expectation: EpisodeWriteExpectation::NewRoot,
    };
    assert!(matches!(
        store.commit_episode(request()).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(store.get_node(node.id()).await.unwrap().is_none());
    store.put_node(&lesson).await.unwrap();
    store.commit_episode(request()).await.unwrap();
    assert!(
        store
            .get_edge(node.id(), lesson.id())
            .await
            .unwrap()
            .is_some()
    );
    store.delete_edge(node.id(), lesson.id()).await.unwrap();
    assert!(matches!(
        store.commit_episode(request()).await.unwrap(),
        EpisodeCommitOutcome::AlreadyApplied(_)
    ));
    assert!(
        store
            .get_edge(node.id(), lesson.id())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn episode_competing_editorial_successors_have_one_winner() {
    let store = episode_kernel_store();
    let root = episode_node("competition", "First observation", 1);
    append(&store, &root).await.unwrap();
    let a = revision(&root, "competition-a", "First correction");
    let b = revision(&root, "competition-b", "Second correction");
    let write = |node| EpisodeCommit {
        node,
        embedding: &EMBEDDING,
        links: &[],
        expectation: EpisodeWriteExpectation::CurrentEdition(root.id()),
    };
    let (a, b) = tokio::join!(
        store.commit_episode(write(&a)),
        store.commit_episode(write(&b))
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(matches!(a, Err(Error::Conflict(_))) || matches!(b, Err(Error::Conflict(_))));
    assert_eq!(store.export().await.unwrap().nodes.len(), 2);
}
