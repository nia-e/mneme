// Shared disposable capture-v1 source for upgrade happy-path and fault tests.
use mneme_core::ports::{BodyStore, EmbeddingMetadataStore, GraphStore};
use mneme_core::{CaptureSource, Edge, EdgeKind, Node, NodeStatus, Provenance};
use mneme_cozo::{CozoStore, MemStore, StoreExport};
use mneme_store_path::StoreLease;
use std::path::Path;
use std::sync::Arc;
use ulid::Ulid;

pub(crate) async fn predecessor(path: &Path) -> StoreExport {
    CozoStore::materialize_fresh_capture(path, Ulid::new(), &MemStore::new(4))
        .await
        .unwrap();
    let lease = Arc::new(StoreLease::acquire(path).unwrap());
    let store = CozoStore::open_existing_persistent(path, 4, lease).unwrap();
    store
        .set_embedding_fingerprint(&mneme_embed::hashing_fingerprint(4))
        .unwrap();
    let bodies = mneme_body::FsStore::new(path.with_extension("bodies")).unwrap();
    let mut nodes = Vec::new();
    for (index, status) in [
        NodeStatus::Active,
        NodeStatus::Candidate { use_count: 3 },
        NodeStatus::Archived,
    ]
    .into_iter()
    .enumerate()
    {
        let source = CaptureSource::new(
            "upgrade-test",
            &format!("claim-{index}"),
            "test://upgrade",
            Some("session"),
            None,
            [index as u8 + 1; 32],
        )
        .unwrap();
        let body = bodies
            .put(format!("preserved body {index}\n").as_bytes())
            .await
            .unwrap();
        let mut node = Node::try_new(
            source.node_id(),
            format!("preserved claim {index}"),
            body,
            ["test"],
            Provenance::External { source },
            0.8,
            0.7,
            NodeStatus::Active,
            index as u128 + 1,
        )
        .unwrap();
        store
            .commit_capture(&node, &[0.0, 1.0, 0.0, 0.0])
            .await
            .unwrap();
        node.set_status(status);
        store.put_node(&node).await.unwrap();
        nodes.push(node);
    }
    store
        .put_edge(&Edge::new(
            nodes[0].id(),
            nodes[1].id(),
            0.75,
            EdgeKind::Associative,
            4,
        ))
        .await
        .unwrap();
    let expected = store.export().await.unwrap();
    store.prepare_for_file_move().unwrap();
    drop(store);
    expected
}
