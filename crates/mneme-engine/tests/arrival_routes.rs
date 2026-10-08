//! Controlled arrival-route consequences, not a claim that priors improve recall.
//! Both ablations use identical source IDs, vectors and retrieval budgets. The
//! fake embedder gives nomination, hydrated-summary rescore and query the same
//! honest fixed vectors; only the committed arrival cap changes (0 versus 2).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use mneme_body::InlineStore;
use mneme_core::ports::{
    Budget, Clock, ColdPath, Embedder, Error, GraphStore, Result as PortResult, RoutingBinding,
    StatusFilter, VectorIndex,
};
use mneme_core::{
    BodyRef, Edge, EdgeKind, EmbeddingFingerprint, Node, NodeId, NodeStatus, Provenance,
    StrengthParams, Timestamp,
};
use mneme_cozo::MemStore;
use mneme_engine::{Capture, CaptureLink, Config, Memory, RetrievalBatch};
use ulid::Ulid;

const DIM: usize = 3;
const NOW: Timestamp = 100;

struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        NOW
    }
}

struct FixedEmbedder(HashMap<String, [f32; DIM]>);
#[async_trait]
impl Embedder for FixedEmbedder {
    fn dim(&self) -> usize {
        DIM
    }

    fn fingerprint(&self) -> EmbeddingFingerprint {
        EmbeddingFingerprint::new("arrival-routes-fixed-v1", DIM, "l2-f32-v1", "symmetric-v1")
    }

    async fn embed(&self, texts: &[&str]) -> PortResult<Vec<Vec<f32>>> {
        texts
            .iter()
            .map(|text| {
                self.0
                    .get(*text)
                    .map(|vector| vector.to_vec())
                    .ok_or_else(|| Error::InvalidInput(format!("unmapped fixture text: {text}")))
            })
            .collect()
    }
}

fn fixture(cap: usize, vectors: &[(&str, [f32; DIM])]) -> (Memory, Arc<MemStore>) {
    let store = Arc::new(MemStore::new(DIM));
    let embedder = FixedEmbedder(
        vectors
            .iter()
            .map(|(text, vector)| ((*text).to_owned(), *vector))
            .collect(),
    );
    let memory = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(embedder),
        Arc::new(FixedClock),
        Config {
            ann_k: 5,
            lexical_k: 0,
            similarity_link_cap: cap,
            min_similarity_links: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    assert_eq!(memory.config().graph_seed_cap, 3);
    assert_eq!(memory.config().budget.query_conditioning, 0.4);
    (memory, store)
}

fn capture<'a>(key: &'a str, summary: &'a str) -> Capture<'a> {
    Capture::new(
        "arrival-routes-fixture",
        key,
        "fixture://arrival-routes",
        None,
        None,
        summary,
        b"",
        &[],
    )
}

async fn seed(store: &MemStore, id: u128, summary: &str, vector: [f32; DIM]) -> NodeId {
    let id = NodeId(Ulid::from(id));
    let node = Node::try_new(
        id,
        summary,
        BodyRef::new(format!("inline://{}", id.0)).unwrap(),
        std::iter::empty::<&str>(),
        Provenance::derived_empty(),
        1.0,
        1.0,
        NodeStatus::Active,
        NOW,
    )
    .unwrap();
    store.put_node(&node).await.unwrap();
    store.upsert(id, &vector).await.unwrap();
    id
}

async fn retrieve(memory: &Memory, query: &str, k: usize, max_nodes: usize) -> RetrievalBatch {
    // Empty hints request route bindings but apply no signed routing change.
    memory
        .retrieve_batch_seeded_routed(
            query,
            k,
            Budget {
                max_nodes,
                max_depth: 1,
                ..memory.config().budget
            },
            StatusFilter::ACTIVE,
            &[],
            &[],
        )
        .await
        .unwrap()
}

fn members(batch: &RetrievalBatch) -> Vec<NodeId> {
    batch.primary.iter().map(|hit| hit.node.id()).collect()
}

async fn assert_bound_route(
    store: &MemStore,
    batch: &RetrievalBatch,
    previous: NodeId,
    target: NodeId,
) {
    let previous_node = store.get_node(previous).await.unwrap().unwrap();
    let target_node = store.get_node(target).await.unwrap().unwrap();
    // Generated capture edges are stored arrival -> prior, while traversal can
    // traverse that edge in either direction. Bind the actual stored endpoints.
    let edge = store
        .get_edge(previous, target)
        .await
        .unwrap()
        .or(store.get_edge(target, previous).await.unwrap())
        .unwrap();
    let binding = RoutingBinding::new(&previous_node, &target_node, &edge);
    assert_eq!(batch.routing.as_ref().unwrap().bindings[&target], binding);
    let hit = batch
        .primary
        .iter()
        .find(|hit| hit.node.id() == target)
        .unwrap();
    let path = hit
        .graph_path
        .as_ref()
        .expect("admitted graph intervention");
    assert_eq!(path.len(), 1);
    assert_eq!(path[0].previous, previous);
    assert_eq!(path[0].target, target);
    assert_eq!(path[0].edge.from, edge.from);
    assert_eq!(path[0].edge.to, edge.to);
    println!("bound route: {binding:?}");
}

#[tokio::test]
async fn identical_vector_opposing_claims_remain_separate_active_members() {
    let vectors = [
        ("The migration is reversible", [1.0, 0.0, 0.0]),
        ("The migration is irreversible", [1.0, 0.0, 0.0]),
        ("migration query", [1.0, 0.0, 0.0]),
    ];
    let mut ablations = Vec::new();
    for cap in [0, 2] {
        let (memory, store) = fixture(cap, &vectors);
        let first = memory
            .capture(capture("claim-a", vectors[0].0))
            .await
            .unwrap()
            .id;
        let before = store.get_node(first).await.unwrap().unwrap();
        let second = memory
            .capture(capture("claim-b", vectors[1].0))
            .await
            .unwrap()
            .id;
        assert_ne!(first, second);
        let edge = store.get_edge(second, first).await.unwrap();
        assert_eq!(edge.is_some(), cap == 2);
        if let Some(edge) = edge {
            assert_eq!(edge.kind, EdgeKind::Associative);
            assert_eq!(edge.weight(), 1.0);
        }
        let first_node = store.get_node(first).await.unwrap().unwrap();
        let second_node = store.get_node(second).await.unwrap().unwrap();
        assert_eq!(first_node.summary(), vectors[0].0);
        assert_eq!(second_node.summary(), vectors[1].0);
        assert_eq!(first_node.confidence(), before.confidence());
        assert_eq!(first_node.status(), NodeStatus::Active);
        assert_eq!(second_node.status(), NodeStatus::Active);
        assert_eq!(store.all_nodes(ColdPath::acquire()).await.unwrap().len(), 2);
        let batch = retrieve(&memory, "migration query", 2, 2).await;
        assert_eq!(
            members(&batch).into_iter().collect::<HashSet<_>>(),
            HashSet::from([first, second])
        );
        let direct_members = members(&batch);
        let one_root = retrieve(&memory, "migration query", 1, 2).await;
        let expected = if cap == 2 {
            direct_members.clone()
        } else {
            vec![direct_members[0]]
        };
        assert_eq!(members(&one_root), expected);
        if cap == 2 {
            assert_bound_route(&store, &one_root, direct_members[0], direct_members[1]).await;
        }
        println!(
            "opposing cap={cap}: direct={direct_members:?}; one-root={:?}",
            members(&one_root)
        );
        ablations.push(direct_members);
    }
    assert_eq!(ablations[0], ablations[1]);
}

#[tokio::test]
async fn grounded_orthogonal_route_survives_arrival_and_replay_after_edge_edit() {
    let vectors = [
        ("Grounded orthogonal consequence", [0.0, 1.0, 0.0]),
        ("Arrival with grounded evidence", [1.0, 0.0, 0.0]),
        ("grounded query", [1.0, 0.0, 0.0]),
    ];
    let mut ablations = Vec::new();
    for cap in [0, 2] {
        let (memory, store) = fixture(cap, &vectors);
        let target = memory
            .capture(capture("orthogonal-target", vectors[0].0))
            .await
            .unwrap()
            .id;
        let links = [CaptureLink::new(target, EdgeKind::Associative, 0.7).unwrap()];
        let request = capture("grounded-arrival", vectors[1].0).with_links(&links);
        let arrival = memory.capture(request).await.unwrap().id;
        let mut edge = store.get_edge(arrival, target).await.unwrap().unwrap();
        assert_eq!(edge.weight(), 0.7, "authored low-cosine link wins");
        edge.reinforce(NOW + 1, &StrengthParams::default());
        let edited_weight = edge.weight();
        store.put_edge(&edge).await.unwrap();
        assert!(
            memory
                .capture(capture("grounded-arrival", vectors[1].0).with_links(&links))
                .await
                .unwrap()
                .replayed
        );
        assert_eq!(
            store
                .get_edge(arrival, target)
                .await
                .unwrap()
                .unwrap()
                .weight(),
            edited_weight
        );
        let batch = retrieve(&memory, "grounded query", 1, 2).await;
        assert_eq!(members(&batch), vec![arrival, target]);
        assert_bound_route(&store, &batch, arrival, target).await;
        println!("grounded cap={cap}: {:?}", members(&batch));
        ablations.push(members(&batch));
    }
    assert_eq!(ablations[0], ablations[1]);
}

#[tokio::test]
async fn prior_into_hub_beyond_24_is_reachable_without_pruning_old_routes() {
    let vectors = [
        ("Existing hub", [0.95, (1.0_f32 - 0.95 * 0.95).sqrt(), 0.0]),
        ("Orthogonal old neighbor", [0.0, 0.0, 1.0]),
        ("New hub arrival", [1.0, 0.0, 0.0]),
        ("hub query", [1.0, 0.0, 0.0]),
    ];
    let mut ablations = Vec::new();
    for cap in [0, 2] {
        let (memory, store) = fixture(cap, &vectors);
        let hub = seed(&store, 100, vectors[0].0, vectors[0].1).await;
        let mut old_routes = Vec::new();
        for offset in 0..25 {
            let target = seed(&store, 200 + offset, vectors[1].0, vectors[1].1).await;
            let edge = Edge::new(hub, target, 0.3, EdgeKind::Associative, NOW);
            store.put_edge(&edge).await.unwrap();
            old_routes.push(edge);
        }
        let arrival = memory
            .capture(capture("hub-arrival", vectors[2].0))
            .await
            .unwrap()
            .id;
        assert_eq!(
            store.get_edge(arrival, hub).await.unwrap().is_some(),
            cap == 2
        );
        for old in old_routes {
            let remaining = store.get_edge(old.from, old.to).await.unwrap().unwrap();
            assert_eq!(remaining.weight(), old.weight());
            assert_eq!(remaining.kind, old.kind);
        }
        let batch = retrieve(&memory, "hub query", 1, 2).await;
        let expected = if cap == 2 {
            vec![arrival, hub]
        } else {
            vec![arrival]
        };
        assert_eq!(members(&batch), expected);
        if cap == 2 {
            assert_bound_route(&store, &batch, arrival, hub).await;
        }
        println!("hub cap={cap}: {:?}", members(&batch));
        ablations.push((arrival, hub));
    }
    assert_eq!(ablations[0], ablations[1]);
}

fn plane(cosine: f32, sign: f32) -> [f32; DIM] {
    [cosine, sign * (1.0 - cosine * cosine).sqrt(), 0.0]
}

#[tokio::test]
async fn known_tradeoff_generated_route_crowds_out_useful_direct_tail() {
    // Semantic usefulness is stipulated by the fixture, not inferred from its
    // vectors. All index and rescore vectors agree. This is expected harm under
    // one controlled four-item budget, not a failure to tune away or a benchmark.
    let vectors = [
        ("Query winner", plane(1.0, 1.0)),
        ("Second dense root", plane(0.99, 1.0)),
        ("Third dense root", plane(0.98, 1.0)),
        ("Useful direct-tail evidence", plane(0.85, -1.0)),
        ("Direct buffer", plane(0.84, -1.0)),
        ("Misleading similar arrival", plane(0.8, 1.0)),
        ("crowding query", plane(1.0, 1.0)),
    ];
    let mut ablations = Vec::new();
    for cap in [0, 2] {
        let (memory, store) = fixture(cap, &vectors);
        // Raw baseline nodes intentionally isolate the one new arrival's routes.
        let mut ids = Vec::new();
        for (offset, (summary, vector)) in vectors[..5].iter().enumerate() {
            ids.push(seed(&store, 1000 + offset as u128, summary, *vector).await);
        }
        let bad = memory
            .capture(capture("misleading-arrival", vectors[5].0))
            .await
            .unwrap()
            .id;
        assert_eq!(
            store.get_edge(bad, ids[1]).await.unwrap().is_some(),
            cap == 2
        );
        assert_eq!(
            store.get_edge(bad, ids[2]).await.unwrap().is_some(),
            cap == 2
        );
        assert!(store.get_edge(bad, ids[0]).await.unwrap().is_none());
        assert!(store.get_edge(bad, ids[3]).await.unwrap().is_none());
        let batch = retrieve(&memory, "crowding query", 4, 4).await;
        let expected = if cap == 0 {
            vec![ids[0], ids[1], ids[2], ids[3]]
        } else {
            vec![ids[0], bad, ids[1], ids[2]]
        };
        assert_eq!(members(&batch), expected);
        if cap == 2 {
            assert_bound_route(&store, &batch, ids[2], bad).await;
            assert!(batch.primary.iter().all(|hit| hit.node.id() != ids[3]));
        }
        println!(
            "crowding cap={cap}: {:?}; useful={:?}; misleading={bad:?}",
            members(&batch),
            ids[3]
        );
        ablations.push(bad);
    }
    assert_eq!(ablations[0], ablations[1]);
}
