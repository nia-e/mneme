use mneme_core::ports::{GraphStore, IncidentEdgesRequest, SystemClock, Traversal, VectorIndex};
use mneme_core::{Edge, EdgeKind, NodeId};
use mneme_cozo::MemStore;
use mneme_embed::HashingEmbedder;
use mneme_engine::{Config, Memory};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use ulid::Ulid;

fn memory<S: GraphStore + VectorIndex + Traversal + 'static>(store: Arc<S>) -> Memory {
    Memory::new(
        store.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(4)),
        Arc::new(SystemClock),
        Config::default(),
    )
}
async fn populate(store: &dyn GraphStore, hub: NodeId) {
    for index in 1..=80u128 {
        let endpoint = NodeId(Ulid::from(index));
        store
            .put_edge(&Edge::new(hub, endpoint, 0.1, EdgeKind::Transition, 1))
            .await
            .unwrap();
        store
            .put_edge(&Edge::new(endpoint, hub, 0.9, EdgeKind::DerivedFrom, 1))
            .await
            .unwrap();
    }
}
async fn inspect(mem: &Memory, hub: NodeId) -> BTreeSet<(NodeId, bool)> {
    let mut after = None;
    let mut found = BTreeSet::new();
    for _ in 0..30 {
        let request = IncidentEdgesRequest::new(hub, 7, Duration::from_secs(5), after).unwrap();
        let page = mem.inspect_neighbors_page(&request).await.unwrap();
        assert!(page.items.len() <= 7);
        assert!(page.work.rows_scanned <= 7);
        assert!(page.work.indexed_seeks <= 3);
        for item in page.items {
            assert!(item.node.is_none());
            assert!(found.insert((item.neighbor.node, item.neighbor.incoming)));
        }
        after = page.next;
        if after.is_none() {
            assert_eq!(found.len(), 160);
            return found;
        }
    }
    panic!("neighbor cursor did not exhaust");
}

#[tokio::test]
async fn mem_indexed_neighbors_are_bounded_and_read_only() {
    let store = Arc::new(MemStore::new(4));
    let hub = NodeId(Ulid::from(1000u128));
    populate(store.as_ref(), hub).await;
    let before = store.export().canonical_value().unwrap();
    inspect(&memory(store.clone()), hub).await;
    assert_eq!(store.export().canonical_value().unwrap(), before);
}

#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_indexed_neighbors_match_mem_and_do_not_mutate() {
    let native = Arc::new(mneme_cozo::CozoStore::new(4).unwrap());
    let modeled = Arc::new(MemStore::new(4));
    let hub = NodeId(Ulid::from(1000u128));
    populate(native.as_ref(), hub).await;
    populate(modeled.as_ref(), hub).await;
    let before = native.export().await.unwrap().canonical_value().unwrap();
    assert_eq!(
        inspect(&memory(native.clone()), hub).await,
        inspect(&memory(modeled), hub).await
    );
    assert_eq!(
        native.export().await.unwrap().canonical_value().unwrap(),
        before
    );
}
