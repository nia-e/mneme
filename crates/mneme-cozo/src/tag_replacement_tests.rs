use super::*;
use mneme_core::BoundedTagSet;

pub(crate) async fn contract(store: &dyn GraphStore) {
    let mut node = crate::touchstone_tests::target(NodeId(Ulid::new()), "preserve me");
    node.record_exposure(4);
    node.record_grounded_use(5);
    node.set_status(NodeStatus::Archived);
    store.put_node(&node).await.unwrap();
    let peer = crate::touchstone_tests::target(NodeId(Ulid::new()), "linked peer");
    store.put_node(&peer).await.unwrap();
    let edge = Edge::new(
        node.id(),
        peer.id(),
        0.7,
        mneme_core::EdgeKind::Associative,
        3,
    );
    store.put_edge(&edge).await.unwrap();
    let old = node.tag_set().clone();
    let new = BoundedTagSet::try_from_iter(["possibility", "possibility/parked"]).unwrap();
    let replaced = store
        .compare_replace_node_tags(node.id(), &old, &new)
        .await
        .unwrap();
    let mut expected = node.clone();
    expected.replace_tags(new.clone());
    assert_eq!(
        serde_json::to_value(&replaced).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert!(matches!(
        store.compare_replace_node_tags(node.id(), &old, &old).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        serde_json::to_value(store.get_node(node.id()).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(
        store
            .compare_replace_node_tags(node.id(), &new, &new)
            .await
            .unwrap()
            .tag_set(),
        &new
    );
    assert_eq!(
        serde_json::to_value(store.get_edge(node.id(), peer.id()).await.unwrap()).unwrap(),
        serde_json::to_value(Some(&edge)).unwrap()
    );
    let empty = BoundedTagSet::default();
    assert!(
        store
            .compare_replace_node_tags(node.id(), &new, &empty)
            .await
            .unwrap()
            .tag_set()
            .is_empty()
    );
    assert!(matches!(
        store
            .compare_replace_node_tags(NodeId(Ulid::new()), &empty, &new)
            .await,
        Err(Error::NotFound)
    ));

    let source =
        CaptureSource::new("retag", "capture", "test://retag", None, None, [2; 32]).unwrap();
    let captured = Node::try_new(
        source.node_id(),
        "capture replay",
        mneme_core::BodyRef::new("inline://retag").unwrap(),
        ["old"],
        Provenance::External {
            source: source.clone(),
        },
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    let proof = CaptureReplayProof::semantic(source, [0; 32], [1; 32]).unwrap();
    store
        .commit_capture(&captured, &[1.0, 0.0, 0.0, 0.0], &proof)
        .await
        .unwrap();
    store
        .compare_replace_node_tags(captured.id(), captured.tag_set(), &new)
        .await
        .unwrap();
    store
        .commit_capture(&captured, &[1.0, 0.0, 0.0, 0.0], &proof)
        .await
        .unwrap();
    assert_eq!(
        store
            .get_node(captured.id())
            .await
            .unwrap()
            .unwrap()
            .tag_set(),
        &new
    );

    let target = crate::touchstone_tests::target(NodeId(Ulid::new()), "touchstone target");
    store.put_node(&target).await.unwrap();
    let input = crate::touchstone_tests::input(
        store.touchstones().unwrap().database_id().unwrap(),
        &target,
    );
    let (owner, proof) = crate::touchstone_tests::authored("retag-invariant", &input);
    crate::touchstone_tests::capture(store, &owner, &proof, &input)
        .await
        .unwrap();
    assert!(matches!(
        store
            .compare_replace_node_tags(owner.id(), owner.tag_set(), &new)
            .await,
        Err(Error::Conflict(_))
    ));
    store
        .compare_replace_node_tags(owner.id(), owner.tag_set(), owner.tag_set())
        .await
        .unwrap();
}

#[tokio::test]
async fn mem_compare_replace_node_tags_contract() {
    let store = MemStore::new(4);
    contract(&store).await;
    let g = store.lock();
    for node in g.nodes.values() {
        assert!(
            g.tag_projection
                .indexes_node_exactly(node, (stable_tag_sample_hash(node.id()), node.id()))
        );
    }
}
