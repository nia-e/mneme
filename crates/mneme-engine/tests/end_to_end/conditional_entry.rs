//! Conditional recommendations are semantic entry, not fabricated graph walks.
//! Exact reference ANN isolates policy from approximate-index recall; the native
//! graph owns current meanings, lifecycle and authored edges.

use super::*;
use mneme_core::ports::{
    ClusterId, RoutingBinding, RoutingHint, SignedRoutingBias, TraversalScope,
};

struct NoSpread;
#[async_trait]
impl Traversal for NoSpread {
    async fn spread(
        &self,
        _: &[Scored],
        _: Budget,
        _: Option<&[f32]>,
        _: TraversalScope,
    ) -> PortResult<Vec<Scored>> {
        panic!("conditional target admission must not open the source hub")
    }
    async fn detect_communities(&self, _: ColdPath) -> PortResult<Vec<(NodeId, ClusterId)>> {
        panic!("conditional entry is not whole-graph work")
    }
}

fn node(raw: u128, tags: &[&str]) -> Node {
    Node::try_new(
        NodeId(Ulid::from(raw)),
        format!("conditional node {raw}"),
        BodyRef::new(format!("inline://{raw}")).unwrap(),
        tags.iter().copied(),
        prov(),
        1.0,
        1.0,
        NodeStatus::Active,
        1,
    )
    .unwrap()
}

struct Fixture<S> {
    store: Arc<S>,
    seeds: Arc<MemStore>,
    a: Node,
    b: Node,
    q: Node,
    p: Node,
    c: Node,
    d: Node,
    e: Node,
    ab: RoutingHint,
    pb: RoutingHint,
    aq: RoutingHint,
    ba: RoutingHint,
}

async fn fixture<S: GraphStore + VectorIndex + Traversal + 'static>(store: Arc<S>) -> Fixture<S> {
    let seeds = Arc::new(MemStore::new(4));
    let a = node(970_001, &["source-only"]);
    let b = node(970_002, &["wanted"]);
    let q = node(970_003, &[]);
    let p = node(970_004, &[]);
    let c = node(970_010, &["wanted"]);
    let d = node(970_011, &["wanted"]);
    let e = node(970_012, &["wanted"]);
    for (n, v) in [
        (&a, [0.0, 1.0, 0.0, 0.0]),
        (&b, [0.0, 1.0, 0.0, 0.0]),
        (&p, [0.0, 1.0, 0.0, 0.0]),
        (&c, [1.0, 0.0, 0.0, 0.0]),
        (&d, [0.8, 0.6, 0.0, 0.0]),
        (&e, [0.6, 0.8, 0.0, 0.0]),
    ] {
        store.put_node(n).await.unwrap();
        store.upsert(n.id(), &v).await.unwrap();
        seeds.put_node(n).await.unwrap();
        seeds.upsert(n.id(), &v).await.unwrap();
    }
    // Q has no ANN projection at all, not merely a weak cosine candidate.
    store.put_node(&q).await.unwrap();
    seeds.put_node(&q).await.unwrap();
    let mut hints = Vec::new();
    for (from, to, weight) in [(&a, &b, 0.2), (&p, &b, 0.4), (&a, &q, 0.7), (&b, &a, 0.6)] {
        let edge = Edge::new(from.id(), to.id(), weight, EdgeKind::Transition, 1);
        store.put_edge(&edge).await.unwrap();
        hints.push(RoutingHint {
            route: RoutingBinding::new(from, to, &edge),
            sign: SignedRoutingBias::Boost,
        });
    }
    // A's larger unrelated neighborhood must not be expanded to recommend B/Q.
    for offset in 0..40 {
        let distractor = node(971_000 + offset, &[]);
        store.put_node(&distractor).await.unwrap();
        store
            .put_edge(&Edge::new(
                a.id(),
                distractor.id(),
                0.9,
                EdgeKind::Transition,
                1,
            ))
            .await
            .unwrap();
    }
    Fixture {
        store,
        seeds,
        a,
        b,
        q,
        p,
        c,
        d,
        e,
        ab: hints[0].clone(),
        pb: hints[1].clone(),
        aq: hints[2].clone(),
        ba: hints[3].clone(),
    }
}

impl<S: GraphStore + VectorIndex + Traversal + 'static> Fixture<S> {
    fn memory(&self, config: Config) -> Memory {
        Memory::new(
            self.store.clone(),
            self.seeds.clone(),
            Arc::new(NoSpread),
            Arc::new(RoutingFixtureEmbedder),
            Arc::new(SystemClock),
            config,
        )
    }
    async fn hint(&self, from: &Node, to: &Node, weight: f32) -> RoutingHint {
        let edge = Edge::new(from.id(), to.id(), weight, EdgeKind::Transition, 1);
        self.store.put_edge(&edge).await.unwrap();
        RoutingHint {
            route: RoutingBinding::new(from, to, &edge),
            sign: SignedRoutingBias::Boost,
        }
    }
}

fn budget() -> Budget {
    Budget {
        max_nodes: 5,
        max_depth: 2,
        min_relevance: 0.55,
        relevance_ratio: 0.0,
        dedup_similarity: 1.0,
        query_conditioning: 1.0,
        explore: 0.0,
        ..Budget::default()
    }
}
fn no_graph() -> Config {
    Config {
        graph_seed_cap: 0,
        lexical_k: 0,
        ..Config::default()
    }
}
fn ids(batch: &mneme_engine::RetrievalBatch) -> Vec<NodeId> {
    batch.primary.iter().map(|hit| hit.node.id()).collect()
}

async fn assert_contract<S: GraphStore + VectorIndex + Traversal + 'static>(store: Arc<S>) {
    let f = fixture(store).await;
    let m = f.memory(no_graph());
    let baseline = m
        .retrieve_batch_seeded_observed("query", 3, budget(), StatusFilter::ACTIVE, &[])
        .await
        .unwrap();
    assert_eq!(ids(&baseline), vec![f.c.id(), f.d.id(), f.e.id()]);
    let empty = m
        .retrieve_batch_seeded_routed("query", 3, budget(), StatusFilter::ACTIVE, &[], &[])
        .await
        .unwrap();
    assert_eq!(ids(&empty), ids(&baseline));
    assert_eq!(
        empty
            .primary
            .iter()
            .map(|hit| hit.evidence.clone())
            .collect::<Vec<_>>(),
        baseline
            .primary
            .iter()
            .map(|hit| hit.evidence.clone())
            .collect::<Vec<_>>()
    );

    // Positive recommendation is outside ANN's k and positive-cosine floor,
    // with an authored weight below that floor; it remains distinct from spread.
    let result = m
        .retrieve_batch_seeded_routed(
            "query",
            3,
            budget(),
            StatusFilter::ACTIVE,
            &[],
            &[f.ab.clone(), f.aq.clone()],
        )
        .await
        .unwrap();
    assert_eq!(
        ids(&result),
        vec![f.c.id(), f.b.id(), f.d.id(), f.q.id(), f.e.id()]
    );
    let outcome = result.routing.as_ref().unwrap();
    assert!(outcome.bindings.is_empty());
    assert_eq!(outcome.conditional_bindings.len(), 2);
    for hit in &result.primary {
        assert!(hit.graph_path.is_none());
        assert!(hit.evidence.graph_rank.is_none());
        if [f.b.id(), f.q.id()].contains(&hit.node.id()) {
            assert!(hit.evidence.dense_rank.is_none());
        }
    }
    for config in [
        Config {
            graph_slot_cap: 0,
            ..Config::default()
        },
        Config {
            graph_weight: 0.0,
            ..Config::default()
        },
    ] {
        let result = f
            .memory(config)
            .retrieve_batch_seeded_routed(
                "query",
                3,
                budget(),
                StatusFilter::ACTIVE,
                &[],
                &[f.ab.clone()],
            )
            .await
            .unwrap();
        assert_eq!(ids(&result), vec![f.c.id(), f.b.id(), f.d.id(), f.e.id()]);
    }

    // Independent positive routes to one target do not vote; canonical source A
    // wins representative selection regardless of input order.
    for hints in [
        vec![f.pb.clone(), f.ab.clone()],
        vec![f.ab.clone(), f.pb.clone()],
    ] {
        let result = m
            .retrieve_batch_seeded_routed("query", 3, budget(), StatusFilter::ACTIVE, &[], &hints)
            .await
            .unwrap();
        assert_eq!(ids(&result), vec![f.c.id(), f.b.id(), f.d.id(), f.e.id()]);
        assert_eq!(
            result.routing.as_ref().unwrap().conditional_bindings[&f.b.id()],
            f.ab.route
        );
        assert_eq!(result.routing.as_ref().unwrap().diagnostics.validated, 2);
    }
    let weak = RoutingHint {
        route: f.ab.route.clone(),
        sign: SignedRoutingBias::Weaken,
    };
    for hints in [
        vec![weak.clone()],
        vec![f.ab.clone(), f.ab.clone()],
        vec![f.ab.clone(), weak.clone()],
    ] {
        let result = m
            .retrieve_batch_seeded_routed("query", 3, budget(), StatusFilter::ACTIVE, &[], &hints)
            .await
            .unwrap();
        assert_eq!(ids(&result), ids(&baseline));
        assert!(result.routing.unwrap().conditional_bindings.is_empty());
    }
    let result = m
        .retrieve_batch_seeded_routed(
            "query",
            3,
            budget(),
            StatusFilter::ACTIVE,
            &[],
            &[weak, f.pb.clone()],
        )
        .await
        .unwrap();
    assert_eq!(
        result.routing.unwrap().conditional_bindings[&f.b.id()],
        f.pb.route
    );

    // Mutual routes produce two recommendation targets, not seeded zero-hop
    // activations which hide each other as graph roots.
    let result = m
        .retrieve_batch_seeded_routed(
            "query",
            3,
            budget(),
            StatusFilter::ACTIVE,
            &[],
            &[f.ab.clone(), f.ba.clone()],
        )
        .await
        .unwrap();
    assert_eq!(
        ids(&result),
        vec![f.c.id(), f.a.id(), f.d.id(), f.b.id(), f.e.id()]
    );
    assert_eq!(result.routing.unwrap().conditional_bindings.len(), 2);

    let direct_hint = f.hint(&f.a, &f.c, 0.3).await;
    let direct = m
        .retrieve_batch_seeded_routed(
            "query",
            3,
            budget(),
            StatusFilter::ACTIVE,
            &[],
            &[direct_hint],
        )
        .await
        .unwrap();
    assert_eq!(ids(&direct), ids(&baseline));
    assert!(direct.routing.unwrap().conditional_bindings.is_empty());
    let tail_hint = f.hint(&f.a, &f.e, 0.3).await;
    let tight = Budget {
        max_nodes: 3,
        ..budget()
    };
    let promoted = m
        .retrieve_batch_seeded_routed("query", 3, tight, StatusFilter::ACTIVE, &[], &[tail_hint])
        .await
        .unwrap();
    assert_eq!(ids(&promoted), vec![f.c.id(), f.e.id(), f.d.id()]);
    assert!(
        promoted
            .routing
            .unwrap()
            .conditional_bindings
            .contains_key(&f.e.id())
    );
    let singleton = m
        .retrieve_batch_seeded_routed(
            "query",
            3,
            Budget {
                max_nodes: 1,
                ..budget()
            },
            StatusFilter::ACTIVE,
            &[],
            &[f.ab.clone()],
        )
        .await
        .unwrap();
    assert_eq!(ids(&singleton), vec![f.c.id()]);
    assert!(singleton.routing.unwrap().conditional_bindings.is_empty());
    for (k, b) in [
        (0, budget()),
        (
            3,
            Budget {
                max_nodes: 0,
                ..budget()
            },
        ),
    ] {
        let result = m
            .retrieve_batch_seeded_routed("query", k, b, StatusFilter::ACTIVE, &[], &[f.ab.clone()])
            .await
            .unwrap();
        assert!(result.primary.is_empty());
        assert!(result.routing.unwrap().conditional_bindings.is_empty());
    }
    let depth_zero = m
        .retrieve_batch_seeded_routed(
            "query",
            3,
            Budget {
                max_depth: 0,
                ..budget()
            },
            StatusFilter::ACTIVE,
            &[],
            &[f.ab.clone()],
        )
        .await
        .unwrap();
    assert_eq!(ids(&depth_zero), ids(&baseline));
    assert_eq!(depth_zero.routing.unwrap().diagnostics.ignored, 1);

    // Tags constrain the recommended target, not its endorsing predecessor.
    let tagged = m
        .retrieve_batch_seeded_routed(
            "query",
            3,
            budget(),
            StatusFilter::ACTIVE,
            &["wanted"],
            &[f.ab.clone(), f.aq.clone()],
        )
        .await
        .unwrap();
    assert_eq!(ids(&tagged), vec![f.c.id(), f.b.id(), f.d.id(), f.e.id()]);
    assert_eq!(tagged.routing.unwrap().conditional_bindings.len(), 1);
    let absent_tag = m
        .retrieve_batch_seeded_routed(
            "query",
            3,
            budget(),
            StatusFilter::ACTIVE,
            &["missing"],
            &[f.ab.clone()],
        )
        .await
        .unwrap();
    assert!(absent_tag.primary.is_empty());
    let tagged_zero = m
        .retrieve_batch_seeded_routed(
            "query",
            0,
            budget(),
            StatusFilter::ACTIVE,
            &["wanted"],
            &[f.ab.clone()],
        )
        .await
        .unwrap();
    assert!(tagged_zero.primary.is_empty());
    assert!(tagged_zero.routing.unwrap().conditional_bindings.is_empty());
    let empty_index = Memory::new(
        f.store.clone(),
        Arc::new(MemStore::new(4)),
        Arc::new(NoSpread),
        Arc::new(RoutingFixtureEmbedder),
        Arc::new(SystemClock),
        no_graph(),
    );
    let unseeded = empty_index
        .retrieve_batch_seeded_routed(
            "query",
            3,
            budget(),
            StatusFilter::ACTIVE,
            &[],
            &[f.aq.clone(), f.ab.clone()],
        )
        .await
        .unwrap();
    assert_eq!(ids(&unseeded), vec![f.b.id(), f.q.id()]);
    assert_eq!(unseeded.routing.unwrap().conditional_bindings.len(), 2);

    // Fresh binding to an authored zero edge validates identity but cannot grant
    // entry. Neither that hint nor stale positive meaning opens A's distractors.
    let zero = f.hint(&f.a, &f.b, 0.0).await;
    let zero_result = m
        .retrieve_batch_seeded_routed("query", 3, budget(), StatusFilter::ACTIVE, &[], &[zero])
        .await
        .unwrap();
    assert_eq!(ids(&zero_result), ids(&baseline));
    assert_eq!(zero_result.routing.unwrap().diagnostics.validated, 1);
    let stale = m
        .retrieve_batch_seeded_routed("query", 3, budget(), StatusFilter::ACTIVE, &[], &[f.ab])
        .await
        .unwrap();
    assert_eq!(ids(&stale), ids(&baseline));
    assert_eq!(stale.routing.unwrap().diagnostics.ignored, 1);
    assert_eq!(
        f.store
            .get_edge(f.p.id(), f.b.id())
            .await
            .unwrap()
            .unwrap()
            .weight(),
        0.4
    );
}

async fn assert_revalidation<S: GraphStore + VectorIndex + Traversal + 'static>(
    new_store: impl Fn() -> Arc<S>,
) {
    for changed_endpoint in [Some(false), Some(true), None] {
        let f = fixture(new_store()).await;
        // D is the ordinarily eligible second direct hit, but conditional wins
        // first admission at that position. Expiry must not change its origin
        // into the losing direct-tail alternative and retain the card.
        let hint = f.hint(&f.a, &f.d, 0.3).await;
        let reranker = Arc::new(RoutingPausedReranker {
            started: Notify::new(),
            release: Notify::new(),
        });
        let memory = Arc::new(f.memory(no_graph()).with_reranker(reranker.clone()));
        let reader = memory.clone();
        let task = tokio::spawn(async move {
            reader
                .retrieve_batch_seeded_routed(
                    "query",
                    3,
                    Budget {
                        max_nodes: 2,
                        ..budget()
                    },
                    StatusFilter::ACTIVE,
                    &[],
                    &[hint],
                )
                .await
                .unwrap()
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reranker.started.notified(),
        )
        .await
        .unwrap();
        if let Some(target) = changed_endpoint {
            let old = if target { &f.d } else { &f.a };
            let changed = Node::try_new(
                old.id(),
                "new endpoint meaning",
                old.body().clone(),
                old.tags(),
                prov(),
                1.0,
                1.0,
                NodeStatus::Active,
                1,
            )
            .unwrap();
            f.store.put_node(&changed).await.unwrap();
        } else {
            memory
                .link(f.a.id(), f.d.id(), EdgeKind::Transition, 0.4, None)
                .await
                .unwrap();
        }
        reranker.release.notify_one();
        let result = task.await.unwrap();
        assert_eq!(ids(&result), vec![f.c.id()]);
        assert!(result.routing.unwrap().conditional_bindings.is_empty());
    }
}

#[tokio::test]
async fn reference_conditional_target_entry_contract() {
    assert_contract(Arc::new(MemStore::new(4))).await;
}
#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_conditional_target_entry_contract() {
    assert_contract(Arc::new(CozoStore::new(4).unwrap())).await;
}
#[tokio::test]
async fn reference_conditional_target_entry_revalidates_after_rerank() {
    assert_revalidation(|| Arc::new(MemStore::new(4))).await;
}
#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_conditional_target_entry_revalidates_after_rerank() {
    assert_revalidation(|| Arc::new(CozoStore::new(4).unwrap())).await;
}
