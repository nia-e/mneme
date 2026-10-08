//! Episodic editions share canonical storage, not semantic lifecycle or budgets.
use std::sync::Arc;

use mneme_body::InlineStore;
use mneme_core::episode::OccurrenceSpan;
use mneme_core::ports::{Budget, ColdPath, GraphStore, StatusFilter, SystemClock};
use mneme_core::{Edge, EdgeKind, Node, NodeId, Provenance, Signal};
use mneme_cozo::MemStore;
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::{Config, EpisodeWrite, Ingest, Memory, ObservedRoute, ReceiptFeedback};

fn fixture(cfg: Config) -> (Memory, Arc<MemStore>) {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        cfg,
    )
    .with_body_store(Arc::new(InlineStore::new()))
    .with_lexical_index(store.clone());
    (mem, store)
}

async fn semantic(mem: &Memory, summary: &str) -> NodeId {
    mem.ingest(Ingest::new(
        summary,
        summary.as_bytes(),
        &[],
        Provenance::derived_empty(),
    ))
    .await
    .unwrap()
}

async fn episode(mem: &Memory, key: &str, summary: &str) -> NodeId {
    mem.append_episode(EpisodeWrite::new(
        "episode-policy-test",
        key,
        "fixture",
        None,
        None,
        summary,
        summary.as_bytes(),
        &[],
        OccurrenceSpan::Unknown,
        None,
    ))
    .await
    .unwrap()
    .identity
    .edition_id
}

#[tokio::test]
async fn saturated_episode_corpus_never_spends_semantic_top_k_or_core_budget() {
    let (mem, _) = fixture(Config {
        ann_k: 1,
        ..Config::default()
    });
    let lesson = semantic(&mem, "velvet telescope lesson").await;
    for index in 0..40 {
        episode(&mem, &format!("scene-{index}"), "velvet telescope").await;
    }
    let hits = mem
        .retrieve_seeded(
            "velvet telescope",
            1,
            Budget {
                max_depth: 0,
                max_nodes: 1,
                min_relevance: 0.0,
                ..Budget::default()
            },
            StatusFilter::ALL,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node.id(), lesson);
    assert!(mem.core(ColdPath::acquire()).await.unwrap().is_empty());
    let status = mem.status(ColdPath::acquire()).await.unwrap();
    assert_eq!(status.nodes, 41);
    assert_eq!(status.active, 1);
    assert_eq!(status.episodes, 40);
    assert_eq!(status.episode_editions, 40);
}

#[tokio::test]
async fn scoped_neighbors_and_expanded_recall_select_semantics_before_fanout() {
    let (mem, _) = fixture(Config {
        ann_k: 1,
        graph_slot_cap: 0,
        budget: Budget {
            max_depth: 0,
            min_relevance: 0.0,
            ..Budget::default()
        },
        ..Config::default()
    });
    let seed = semantic(&mem, "violet observatory").await;
    let target = semantic(&mem, "local archive").await;
    for index in 0..10 {
        let scene = episode(&mem, &format!("neighbor-{index}"), "violet observatory").await;
        mem.link(seed, scene, EdgeKind::Associative, 1.0, None)
            .await
            .unwrap();
    }
    mem.link(seed, target, EdgeKind::Associative, 0.2, None)
        .await
        .unwrap();
    for status in [StatusFilter::ACTIVE, StatusFilter::ALL] {
        let neighbors = mem
            .resolved_neighbors_scoped(seed, 1, status)
            .await
            .unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].node.id(), target);
    }
    let hits = mem
        .recall_expanded("violet observatory", 1, 1)
        .await
        .unwrap();
    assert_eq!(hits[0].node.id(), seed);
    assert_eq!(hits[0].neighbors[0].node.id(), target);
    // Raw inspection intentionally does not share the semantic filter.
    assert!(
        !mem.neighbors_hydrated(seed, 1).await.unwrap()[0]
            .node
            .as_ref()
            .unwrap()
            .is_semantic()
    );
}

#[tokio::test]
async fn episode_cannot_act_as_an_invisible_graph_bridge() {
    let (mem, _) = fixture(Config {
        ann_k: 1,
        lexical_k: 0,
        ..Config::default()
    });
    let start = semantic(&mem, "copper lagoon").await;
    let scene = episode(&mem, "bridge", "unrelated memory").await;
    let end = semantic(&mem, "abstract zoology").await;
    mem.link(start, scene, EdgeKind::Transition, 1.0, None)
        .await
        .unwrap();
    mem.link(scene, end, EdgeKind::Transition, 1.0, None)
        .await
        .unwrap();
    let hits = mem
        .retrieve_seeded(
            "copper lagoon",
            1,
            Budget {
                max_depth: 4,
                max_nodes: 10,
                min_relevance: 0.0,
                query_conditioning: 0.0,
                ..Budget::default()
            },
            StatusFilter::ALL,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.node.id()).collect::<Vec<_>>(),
        vec![start]
    );
    assert!(
        mem.resolved_neighbors_scoped(scene, 8, StatusFilter::ALL)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn feedback_refuses_episode_targets_and_routes_without_prefix_mutation() {
    let (mem, store) = fixture(Config::default());
    let lesson = semantic(&mem, "lesson").await;
    let scene = episode(&mem, "feedback", "scene").await;
    mem.link(lesson, scene, EdgeKind::Associative, 0.4, None)
        .await
        .unwrap();
    let before_lesson = store.get_node(lesson).await.unwrap().unwrap();
    let before_scene = store.get_node(scene).await.unwrap().unwrap();
    let before_edge = store.get_edge(lesson, scene).await.unwrap().unwrap();
    for (prior, target) in [(None, scene), (Some(scene), lesson), (Some(lesson), scene)] {
        assert!(
            mem.apply_feedback(prior, target, Signal::RelevantNew)
                .await
                .is_err()
        );
    }
    let events = [
        ReceiptFeedback::node_used(lesson),
        ReceiptFeedback::routed(
            ObservedRoute::new(scene, lesson, lesson, scene).unwrap(),
            true,
        ),
    ];
    assert!(mem.apply_receipt_feedback(&events).await.is_err());
    let forgotten = semantic(&mem, "forgotten endpoint").await;
    mem.forget(ColdPath::acquire(), forgotten).await.unwrap();
    let stale_events = [
        ReceiptFeedback::node_used(lesson),
        ReceiptFeedback::routed(
            ObservedRoute::new(scene, forgotten, scene, forgotten).unwrap(),
            true,
        ),
    ];
    assert!(
        mem.apply_receipt_feedback(&stale_events).await.is_err(),
        "a forgotten partner must not hide the episode endpoint and commit a valid prefix"
    );
    assert_same_node(
        &store.get_node(lesson).await.unwrap().unwrap(),
        &before_lesson,
    );
    assert_same_node(
        &store.get_node(scene).await.unwrap().unwrap(),
        &before_scene,
    );
    assert_same_edge(
        &store.get_edge(lesson, scene).await.unwrap().unwrap(),
        &before_edge,
    );
}

#[tokio::test]
async fn semantic_maintenance_preserves_episode_editions_and_evidence() {
    let (mem, store) = fixture(Config {
        dense_degree_threshold: 0,
        prune_weight_floor: 0.9,
        bridge_probability: 1.0,
        ..Config::default()
    });
    let lesson = semantic(&mem, "lesson").await;
    let scene = episode(&mem, "maintenance", "scene").await;
    mem.link(scene, lesson, EdgeKind::Associative, 0.1, None)
        .await
        .unwrap();
    let before = store.get_node(scene).await.unwrap().unwrap();
    let mut evidence = store.get_edge(scene, lesson).await.unwrap().unwrap();
    evidence.mark_interference();
    store.put_edge(&evidence).await.unwrap();
    mem.decay_sweep(ColdPath::acquire()).await.unwrap();
    mem.prune_dense(ColdPath::acquire()).await.unwrap();
    assert!(
        mem.consolidate(ColdPath::acquire(), &[scene, lesson])
            .await
            .unwrap()
            .is_empty()
    );
    assert_same_node(&store.get_node(scene).await.unwrap().unwrap(), &before);
    assert_same_edge(
        &store.get_edge(scene, lesson).await.unwrap().unwrap(),
        &evidence,
    );
    assert!(mem.forget(ColdPath::acquire(), scene).await.is_err());
    assert!(mem.forget(ColdPath::acquire(), lesson).await.is_err());
    assert_eq!(mem.resolve_body(&before).await.unwrap(), b"scene");
    let retained = store.get_node(lesson).await.unwrap().unwrap();
    assert_eq!(mem.resolve_body(&retained).await.unwrap(), b"lesson");
}

#[tokio::test]
async fn semantic_merge_preserves_the_original_lesson_evidence_endpoint() {
    let (mem, store) = fixture(Config::default());
    let winner = semantic(&mem, "new understanding").await;
    let loser = semantic(&mem, "initial understanding").await;
    let scene = episode(&mem, "wrong-turn", "what we thought then").await;
    mem.link(scene, loser, EdgeKind::Associative, 0.6, None)
        .await
        .unwrap();
    mem.link(loser, scene, EdgeKind::DerivedFrom, 0.7, None)
        .await
        .unwrap();
    let outgoing = store.get_edge(scene, loser).await.unwrap().unwrap();
    let incoming = store.get_edge(loser, scene).await.unwrap().unwrap();
    store
        .observe_merge_candidate(winner, loser, 1)
        .await
        .unwrap();
    mem.merge_full(ColdPath::acquire(), winner, loser)
        .await
        .unwrap();
    assert!(store.get_node(loser).await.unwrap().unwrap().is_archived());
    assert_same_edge(
        &store.get_edge(scene, loser).await.unwrap().unwrap(),
        &outgoing,
    );
    assert_same_edge(
        &store.get_edge(loser, scene).await.unwrap().unwrap(),
        &incoming,
    );
    assert!(store.get_edge(scene, winner).await.unwrap().is_none());
    assert!(store.get_edge(winner, scene).await.unwrap().is_none());
    assert!(
        mem.merge_full(ColdPath::acquire(), winner, scene)
            .await
            .is_err()
    );
    assert!(
        mem.supersede(ColdPath::acquire(), winner, scene)
            .await
            .is_err()
    );
    assert!(
        mem.observe_contradiction(ColdPath::acquire(), winner, scene)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn semantic_terminal_replay_survives_forgetting_an_endpoint() {
    let (mem, store) = fixture(Config::default());
    let winner = semantic(&mem, "replacement").await;
    let loser = semantic(&mem, "old explanation").await;
    mem.supersede(ColdPath::acquire(), winner, loser)
        .await
        .unwrap();
    assert!(mem.forget(ColdPath::acquire(), loser).await.unwrap());
    mem.supersede(ColdPath::acquire(), winner, loser)
        .await
        .unwrap();

    let duplicate = semantic(&mem, "duplicate").await;
    store
        .observe_merge_candidate(winner, duplicate, 1)
        .await
        .unwrap();
    mem.merge_full(ColdPath::acquire(), winner, duplicate)
        .await
        .unwrap();
    assert!(mem.forget(ColdPath::acquire(), duplicate).await.unwrap());
    mem.merge_full(ColdPath::acquire(), winner, duplicate)
        .await
        .unwrap();
}

fn assert_same_node(actual: &Node, expected: &Node) {
    assert!(actual.same_feedback_static_fields(expected));
    assert_eq!(actual.last_grounded_use(), expected.last_grounded_use());
    assert_eq!(actual.grounded_use_count(), expected.grounded_use_count());
    assert_eq!(actual.status(), expected.status());
    assert_eq!(actual.confidence(), expected.confidence());
    assert_eq!(actual.interference(), expected.interference());
}

fn assert_same_edge(actual: &Edge, expected: &Edge) {
    assert!(actual.same_decay_static_fields(expected));
    assert_eq!(actual.weight(), expected.weight());
    assert_eq!(actual.interference(), expected.interference());
}
