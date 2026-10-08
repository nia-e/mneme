//! Successor of the frozen entry-boundary characterization: a current positive
//! route can now recommend its target without entering the source's graph hub.
//!
//! Reuse the existing routing-probe embedder/raw-node fixture. An exact MemStore
//! vector port controls ordinary roots; the chosen backend owns graph traversal
//! and its identically populated conditioning vectors. This isolates entry from
//! approximate-index recall, and says nothing about embedding/model quality.

use super::*;
use mneme_core::ports::{RoutingBinding, RoutingHint, SignedRoutingBias};

async fn populate_routing_entry_fixture<S>(store: &S, reachable: bool) -> RoutingHint
where
    S: GraphStore + VectorIndex + Traversal + 'static,
{
    let a = raw_active_node(NodeId(Ulid::from(930_001u128)));
    let b = raw_active_node(NodeId(Ulid::from(930_002u128)));
    let unrelated: Vec<Node> = (930_003..=930_005u128)
        .map(|id| raw_active_node(NodeId(Ulid::from(id))))
        .collect();
    for node in [&a, &b].into_iter().chain(unrelated.iter()) {
        store.put_node(node).await.unwrap();
    }
    let a_vector = if reachable {
        [1.0, 0.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0, 0.0]
    };
    store.upsert(a.id(), &a_vector).await.unwrap();
    store
        .upsert(b.id(), &[0.5, 0.75_f32.sqrt(), 0.0, 0.0])
        .await
        .unwrap();
    for node in &unrelated {
        let vector = if reachable {
            [0.8, 0.6, 0.0, 0.0]
        } else {
            [1.0, 0.0, 0.0, 0.0]
        };
        store.upsert(node.id(), &vector).await.unwrap();
    }

    let edge = Edge::new(a.id(), b.id(), 0.6, EdgeKind::Transition, 1);
    store.put_edge(&edge).await.unwrap();
    // Same competition as assert_engine_routing_hints: B is below the raw-32
    // cutoff without a positive hint, but has a positive unchanged query-
    // conditioned contribution once admitted. This tests candidate rescue.
    for index in 0..32u128 {
        let node = raw_active_node(NodeId(Ulid::from(930_100 + index)));
        store.put_node(&node).await.unwrap();
        store
            .upsert(node.id(), &[0.0, 1.0, 0.0, 0.0])
            .await
            .unwrap();
        store
            .put_edge(&Edge::new(a.id(), node.id(), 0.7, EdgeKind::Transition, 1))
            .await
            .unwrap();
    }
    RoutingHint {
        route: RoutingBinding::new(&a, &b, &edge),
        sign: SignedRoutingBias::Boost,
    }
}

fn routing_entry_memory<S>(store: Arc<S>, seeds: Arc<MemStore>) -> Memory
where
    S: GraphStore + VectorIndex + Traversal + 'static,
{
    Memory::new(
        store.clone(),
        seeds,
        store.clone(),
        Arc::new(RoutingFixtureEmbedder),
        Arc::new(SystemClock),
        Config {
            graph_seed_cap: 3,
            graph_slot_cap: 8,
            ..Config::default()
        },
    )
}

async fn assert_routing_entry_boundary<S>(new_store: impl Fn() -> Arc<S>)
where
    S: GraphStore + VectorIndex + Traversal + 'static,
{
    let store = new_store();
    let hint = populate_routing_entry_fixture(store.as_ref(), true).await;
    let seeds = Arc::new(MemStore::new(4));
    let seed_hint = populate_routing_entry_fixture(seeds.as_ref(), true).await;
    assert_eq!(seed_hint.route, hint.route);
    let a = hint.route.previous;
    let b = hint.route.target;
    let memory = routing_entry_memory(store.clone(), seeds.clone());
    let budget = Budget {
        max_nodes: 128,
        max_depth: 2,
        min_relevance: 0.05,
        relevance_ratio: 0.0,
        query_conditioning: 1.0,
        dedup_similarity: 1.0,
        explore: 0.0,
        ..Budget::default()
    };

    // No lexical adapter is attached. With k=3 and three graph-root slots, the
    // positive ANN seeds are the ordinary roots. B is NOT a direct candidate.
    let direct = seeds
        .ann(&[1.0, 0.0, 0.0, 0.0], 3, StatusFilter::ACTIVE)
        .await
        .unwrap();
    assert_eq!(direct.len(), 3);
    assert!(direct.iter().any(|hit| hit.id == a));
    assert!(!direct.iter().any(|hit| hit.id == b));

    for sign in [
        None,
        Some(SignedRoutingBias::Boost),
        Some(SignedRoutingBias::Weaken),
    ] {
        let hints: Vec<_> = sign
            .map(|sign| RoutingHint {
                route: hint.route.clone(),
                sign,
            })
            .into_iter()
            .collect();
        let result = memory
            .retrieve_batch_seeded_routed("query", 3, budget, StatusFilter::ACTIVE, &[], &hints)
            .await
            .unwrap();
        let outcome = result.routing.as_ref().unwrap();
        assert_eq!(outcome.diagnostics.validated, hints.len());
        assert_eq!(outcome.diagnostics.ignored, 0);
        let target = result.primary.iter().find(|hit| hit.node.id() == b);
        if sign == Some(SignedRoutingBias::Boost) {
            let target = target.expect("positive hint conditionally recommends non-direct B");
            assert!(target.graph_path.is_none());
            assert!(!outcome.bindings.contains_key(&b));
            assert_eq!(outcome.conditional_bindings.get(&b), Some(&hint.route));
        } else {
            assert!(target.is_none(), "neutral/weaken is not positive rescue");
            assert!(!outcome.bindings.contains_key(&b));
        }
    }

    // Independent topology, identical meaning and edge. Both in-place updates
    // and fresh populations showed unstable Cozo ANN roots on these duplicate-
    // heavy synthetic vectors, before routing ran. Exact reference seeding
    // removes that separate approximation dependency; Cozo still performs its
    // real traversal, vector conditioning, raw shortlist and signed admission.
    let store = new_store();
    let unreachable_hint = populate_routing_entry_fixture(store.as_ref(), false).await;
    assert_eq!(unreachable_hint.route, hint.route);
    assert_eq!(unreachable_hint.sign, hint.sign);
    let seeds = Arc::new(MemStore::new(4));
    let seed_hint = populate_routing_entry_fixture(seeds.as_ref(), false).await;
    assert_eq!(seed_hint.route, hint.route);
    let memory = routing_entry_memory(store.clone(), seeds.clone());

    let direct = seeds
        .ann(&[1.0, 0.0, 0.0, 0.0], 3, StatusFilter::ACTIVE)
        .await
        .unwrap();
    let expected: HashSet<_> = (930_003..=930_005u128)
        .map(|id| NodeId(Ulid::from(id)))
        .collect();
    assert_eq!(
        direct.iter().map(|hit| hit.id).collect::<HashSet<_>>(),
        expected
    );
    let mut baseline_ids = None;
    for sign in [
        None,
        Some(SignedRoutingBias::Boost),
        Some(SignedRoutingBias::Weaken),
    ] {
        let hints: Vec<_> = sign
            .map(|sign| RoutingHint {
                route: hint.route.clone(),
                sign,
            })
            .into_iter()
            .collect();
        let result = memory
            .retrieve_batch_seeded_routed("query", 3, budget, StatusFilter::ACTIVE, &[], &hints)
            .await
            .unwrap();
        let outcome = result.routing.as_ref().unwrap();
        assert_eq!(
            outcome.diagnostics.validated,
            hints.len(),
            "unreachable is not stale"
        );
        assert_eq!(outcome.diagnostics.ignored, 0);
        let ids: Vec<_> = result.primary.iter().map(|hit| hit.node.id()).collect();
        let mut wanted = expected.clone();
        if sign == Some(SignedRoutingBias::Boost) {
            wanted.insert(b);
            assert_eq!(outcome.conditional_bindings.get(&b), Some(&hint.route));
            assert_eq!(ids[1], b, "conditional competes after the ordinary winner");
        } else {
            assert!(outcome.conditional_bindings.is_empty());
        }
        assert_eq!(ids.iter().copied().collect::<HashSet<_>>(), wanted);
        assert!(result.primary.iter().all(|hit| hit.graph_path.is_none()));
        assert!(outcome.bindings.is_empty());
        if sign != Some(SignedRoutingBias::Boost)
            && let Some(baseline_ids) = &baseline_ids
        {
            assert_eq!(
                &ids, baseline_ids,
                "neutral/weaken does not conditionally nominate B"
            );
        } else if sign.is_none() {
            baseline_ids = Some(ids);
        }
    }
    assert_eq!(store.get_edge(a, b).await.unwrap().unwrap().weight(), 0.6);
}

#[tokio::test]
async fn reference_routing_entry_boundary_reachable_and_unreachable_conditional() {
    assert_routing_entry_boundary(|| Arc::new(MemStore::new(4))).await;
}

#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_routing_entry_boundary_reachable_and_unreachable_conditional() {
    assert_routing_entry_boundary(|| Arc::new(CozoStore::new(4).unwrap())).await;
}
