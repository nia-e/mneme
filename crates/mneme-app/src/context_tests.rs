use super::test_support::{episode_write, fixture, fixture_with_store};
use super::*;
use std::{num::NonZeroU32, sync::Arc};

use mneme_body::InlineStore;
use mneme_core::episode::EpisodeRevisionReason;
use mneme_core::ports::SystemClock;
use mneme_core::{NodeId, Provenance};
use mneme_cozo::MemStore;
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::{Config, Ingest, RetrievalBatch};
use mneme_present::{BodyBudget, Lane, LaneBudgets, LaneLimit};
use serde_json::{Value, json};

fn retrieval_budget() -> Budget {
    Budget {
        max_depth: 0,
        max_nodes: 16,
        min_relevance: 0.0,
        ..Budget::default()
    }
}

fn presentation_budget(bytes: u32, items: u16) -> PresentationBudget {
    let disabled = LaneLimit::new(0, 0, 0, 0).unwrap();
    let lanes = LaneBudgets::new(disabled, LaneLimit::new(1, 8, 8, bytes).unwrap(), disabled)
        .with_episodic(LaneLimit::new(0, 2, 4, bytes).unwrap());
    PresentationBudget::new(
        NonZeroU32::new(bytes).unwrap(),
        NonZeroU32::new(PresentationBudget::minimum_control_reserve_bytes()).unwrap(),
        NonZeroU16::new(items).unwrap(),
        NonZeroU16::new(512).unwrap(),
        BodyBudget::disabled(),
        lanes,
    )
    .unwrap()
}

async fn semantic(mem: &Memory, summary: &str, active: bool, tags: &[&str]) -> NodeId {
    let request = Ingest::new(
        summary,
        b"semantic body must never enter ordinary context",
        tags,
        Provenance::derived_empty(),
    );
    let _ = active;
    mem.ingest(request).await.unwrap()
}

fn semantic_signature(batch: &RetrievalBatch) -> Value {
    let lane = |hits: &[mneme_engine::RetrievalHit]| {
        hits.iter()
            .map(|hit| {
                json!({
                    "id":hit.node.id(),"rank":hit.lane_rank,
                    "dense":hit.evidence.dense_rank,"sparse":hit.evidence.sparse_rank,
                    "graph":hit.evidence.graph_rank,"rerank":hit.evidence.rerank_rank,
                })
            })
            .collect::<Vec<_>>()
    };
    json!({"primary":lane(&batch.primary)})
}

fn value(plan: &PackPlan) -> Value {
    serde_json::from_str(plan.rendered_content()).unwrap()
}

#[test]
fn episode_cue_normalization_is_explicit_without_inventing_blank_cues() {
    let (cue, normalized, truncated) = normalize_episode_cue("violet lantern").unwrap();
    assert_eq!(cue.as_str(), "violet lantern");
    assert!(!normalized && !truncated);
    for text in [
        "  violet\tlantern\n",
        "violet\0lantern",
        "violet\u{2003}lantern",
    ] {
        let (cue, normalized, truncated) = normalize_episode_cue(text).unwrap();
        assert_eq!(cue.as_str(), "violet lantern");
        assert!(normalized && !truncated);
    }
    for text in ["", " \t\n", "\0\u{7f}", "\u{2003}\r"] {
        assert!(
            normalize_episode_cue(text).is_none(),
            "invented a cue for {text:?}"
        );
    }
}

#[test]
fn episode_cue_truncation_is_4096_utf8_bytes_not_characters() {
    let exact = "é".repeat(2048);
    let (cue, normalized, truncated) = normalize_episode_cue(&exact).unwrap();
    assert_eq!(cue.as_str(), exact);
    assert!(!normalized && !truncated);

    let longer = "🪨".repeat(1025);
    let (cue, normalized, truncated) = normalize_episode_cue(&longer).unwrap();
    assert_eq!(cue.as_str().len(), 4096);
    assert_eq!(cue.as_str(), "🪨".repeat(1024));
    assert!(!normalized && truncated);

    let split_character = format!("{}é", "a".repeat(4095));
    let (cue, normalized, truncated) = normalize_episode_cue(&split_character).unwrap();
    assert_eq!(cue.as_str(), "a".repeat(4095));
    assert!(!normalized && truncated);

    let split_after_space = format!("{} é", "a".repeat(4094));
    let (cue, _, truncated) = normalize_episode_cue(&split_after_space).unwrap();
    assert_eq!(cue.as_str(), "a".repeat(4094));
    assert!(truncated);
}

#[tokio::test]
async fn mixed_recall_is_typed_current_and_does_not_change_semantic_ranking() {
    let mem = fixture();
    let primary = semantic(&mem, "violet lantern lesson", true, &[]).await;
    let probationary = semantic(&mem, "violet lantern possibility", false, &[]).await;
    let before = mem
        .retrieve_batch_seeded(
            "violet lantern",
            8,
            retrieval_budget(),
            StatusFilter::default(),
            &[],
        )
        .await
        .unwrap();
    let first = mem
        .append_episode(episode_write("evening", "violet lantern original account"))
        .await
        .unwrap();
    let current = mem
        .revise_episode(
            first.identity.episode_id,
            first.identity.edition_id,
            EpisodeRevisionReason::new("Correct the observation, retaining the old account.")
                .unwrap(),
            episode_write("evening-correction", "violet lantern corrected account"),
        )
        .await
        .unwrap();
    let before_nodes = [
        primary,
        probationary,
        first.identity.edition_id,
        current.identity.edition_id,
    ];
    let mut snapshots = Vec::new();
    for id in before_nodes {
        snapshots.push(serde_json::to_value(mem.get_node(id).await.unwrap().unwrap()).unwrap());
    }

    let plan = recall_context(
        &mem,
        "violet lantern",
        8,
        retrieval_budget(),
        &[],
        &presentation_budget(32768, 8),
    )
    .await
    .unwrap();
    let context = value(&plan);
    assert_eq!(context["schema"], "mneme.context.v7");
    assert_eq!(context["episodic_retrieval"]["state"], "searched");
    assert_eq!(context["episodic_retrieval"]["mode"], "lexical");
    let ids = context["primary"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card["id"].clone())
        .collect::<Vec<_>>();
    for card in context["primary"].as_array().unwrap() {
        let stored = snapshots
            .iter()
            .find(|node| node["id"] == card["id"])
            .unwrap();
        assert_eq!(card["created"], stored["created"]);
        let created =
            serde_json::from_value::<mneme_core::Timestamp>(stored["created"].clone()).unwrap();
        assert_eq!(
            card["created_utc"],
            mneme_present::format_timestamp_millis(created)
        );
        assert!(card.get("updated").is_none());
        assert!(card.get("verified_at").is_none());
    }
    assert!(ids.contains(&json!(primary)));
    assert!(ids.contains(&json!(probationary)));
    assert!(context.get("probationary").is_none());
    assert_eq!(context["episodes"].as_array().unwrap().len(), 1);
    let episode = &context["episodes"][0];
    assert_eq!(episode["kind"], "episode");
    assert_eq!(episode["id"], json!(current.identity.edition_id));
    assert_eq!(episode["episode_id"], json!(first.identity.episode_id));
    assert_eq!(episode["edition_id"], json!(current.identity.edition_id));
    assert_eq!(
        episode["current_edition_id"],
        json!(current.identity.edition_id)
    );
    assert_eq!(episode["revision"], 1);
    assert_eq!(episode["occurred"], json!({"kind":"point","at":1234}));
    assert_eq!(
        episode["occurred_utc"],
        json!({"kind":"point","at_utc":"1970-01-01T00:00:01Z"})
    );
    let stored_edition = mem
        .get_node(current.identity.edition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        episode["recorded_at"],
        json!(stored_edition.episode().unwrap().recorded_at())
    );
    assert_eq!(
        episode["edition_recorded_at"],
        json!(stored_edition.created())
    );
    assert!(episode.get("created").is_none());
    assert_eq!(episode["thread"], "violet-evening");
    assert_eq!(
        episode["summary"]["text"],
        "violet lantern corrected account"
    );
    assert!(context["receipt"].is_null());
    assert!(!plan.rendered_content().contains("body must never"));
    let manifest = plan
        .manifest()
        .cards()
        .iter()
        .map(|card| card.node_id())
        .collect::<Vec<_>>();
    assert!(manifest.contains(&current.identity.edition_id));
    assert!(!manifest.contains(&first.identity.edition_id));

    let after = mem
        .retrieve_batch_seeded(
            "violet lantern",
            8,
            retrieval_budget(),
            StatusFilter::default(),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(semantic_signature(&after), semantic_signature(&before));
    for (id, snapshot) in before_nodes.into_iter().zip(snapshots) {
        assert_eq!(
            serde_json::to_value(mem.get_node(id).await.unwrap().unwrap()).unwrap(),
            snapshot,
            "ordinary context mutated node {id:?}"
        );
    }
}

#[tokio::test]
async fn an_episode_only_corpus_is_not_reported_as_empty_semantic_recall() {
    let mem = fixture();
    let episode = mem
        .append_episode(episode_write("only-scene", "violet lantern evening"))
        .await
        .unwrap();
    let plan = recall_context(
        &mem,
        "violet lantern",
        8,
        retrieval_budget(),
        &[],
        &presentation_budget(8192, 4),
    )
    .await
    .unwrap();
    let context = value(&plan);
    assert_eq!(context["primary"], json!([]));
    assert!(context.get("probationary").is_none());
    assert_eq!(context["episodes"].as_array().unwrap().len(), 1);
    assert_eq!(
        context["episodes"][0]["id"],
        json!(episode.identity.edition_id)
    );
    assert_eq!(context["episodic_retrieval"]["state"], "searched");
    assert_eq!(context["usage"]["items"], 1);
    assert_eq!(context["usage"]["episodic"]["items"], 1);
}

#[tokio::test]
async fn a_real_empty_episode_search_is_distinct_from_unavailable_or_skipped() {
    let mem = fixture();
    semantic(&mem, "violet lantern lesson", true, &[]).await;
    let plan = recall_context(
        &mem,
        "violet lantern",
        8,
        retrieval_budget(),
        &[],
        &presentation_budget(8192, 4),
    )
    .await
    .unwrap();
    let context = value(&plan);
    assert_eq!(context["episodes"], json!([]));
    assert_eq!(context["episodic_retrieval"]["state"], "searched");
    assert_eq!(context["episodic_retrieval"]["mode"], "lexical");
    assert!(
        context["episodic_retrieval"]
            .get("unavailable_reason")
            .is_none()
    );
    assert_eq!(
        context["omitted"]["episodic"]["further_tail_unknown"],
        false
    );
    assert_eq!(context["omitted"]["episodic"]["bounded_window_budget"], 0);
}

#[tokio::test]
async fn tagged_context_does_not_silently_ignore_the_selector_for_episodes() {
    let mem = fixture();
    let primary = semantic(&mem, "violet lantern lesson", true, &["scope"]).await;
    mem.append_episode(episode_write("tagged-scene", "violet lantern evening"))
        .await
        .unwrap();
    let plan = recall_context(
        &mem,
        "violet lantern",
        8,
        retrieval_budget(),
        &["scope"],
        &presentation_budget(8192, 4),
    )
    .await
    .unwrap();
    let context = value(&plan);
    assert_eq!(context["primary"][0]["id"], json!(primary));
    assert_eq!(context["episodes"], json!([]));
    assert_eq!(
        context["episodic_retrieval"]["state"],
        "not_searched_tag_filter"
    );
    assert_eq!(context["partial"], true);
}

#[tokio::test]
async fn episodic_cards_share_the_same_global_byte_and_item_budget() {
    let mem = fixture();
    for index in 0..6 {
        semantic(
            &mem,
            &format!("violet lantern semantic {index} {}", "\"".repeat(300)),
            true,
            &[],
        )
        .await;
        mem.append_episode(episode_write(
            &format!("budget-{index}"),
            &format!("violet lantern scene {index} {}", "\"".repeat(300)),
        ))
        .await
        .unwrap();
    }
    let budget = presentation_budget(4096, 3);
    let plan = recall_context(&mem, "violet lantern", 8, retrieval_budget(), &[], &budget)
        .await
        .unwrap();
    let context = value(&plan);
    let items = ["core", "primary", "expansions", "episodes"]
        .into_iter()
        .map(|lane| context[lane].as_array().unwrap().len())
        .sum::<usize>();
    assert!(items <= 3);
    assert!(!context["primary"].as_array().unwrap().is_empty());
    assert!(!context["episodes"].as_array().unwrap().is_empty());
    assert!(plan.rendered_content().len() <= 4096);
    assert_eq!(context["usage"]["items"], items);
    assert_eq!(
        context["usage"]["content_bytes"],
        plan.rendered_content().len()
    );
    assert_eq!(plan.manifest().cards().len(), items);
    assert_eq!(
        plan.envelope().usage().lane(Lane::Episodic).items() as usize,
        context["episodes"].as_array().unwrap().len()
    );
    assert!(
        context["omitted"]["episodic"]["bounded_window_budget"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[tokio::test]
async fn automatic_context_reports_cue_normalization_without_hiding_it() {
    let mem = fixture();
    mem.append_episode(episode_write("normalized-scene", "violet lantern evening"))
        .await
        .unwrap();
    let plan = recall_context(
        &mem,
        "  violet\tlantern\n",
        8,
        retrieval_budget(),
        &[],
        &presentation_budget(8192, 4),
    )
    .await
    .unwrap();
    let context = value(&plan);
    assert_eq!(context["episodes"].as_array().unwrap().len(), 1);
    assert_eq!(context["episodic_retrieval"]["cue_normalized"], true);
    assert_eq!(context["episodic_retrieval"]["cue_truncated"], false);
    assert_eq!(context["partial"], true);
}

#[test]
fn only_typed_episode_unavailability_becomes_an_unavailable_lane() {
    for (reason, expected) in [
        (
            EpisodeUnavailableReason::AdapterUnsupported,
            "adapter_unsupported",
        ),
        (
            EpisodeUnavailableReason::StoreNotUpgraded,
            "store_not_upgraded",
        ),
    ] {
        let (metadata, window) =
            episode_result(Err(Error::EpisodeUnavailable(reason)), false, false, 8).unwrap();
        let metadata = serde_json::to_value(metadata).unwrap();
        assert_eq!(metadata["state"], "unavailable");
        assert_eq!(metadata["unavailable_reason"], expected);
        assert!(window.cards().is_empty());
        assert!(!window.further_tail_unknown());
    }
    for error in [
        Error::Backend("episode index is damaged".into()),
        Error::InvalidInput("episodes not supported".into()),
        Error::Backend("adapter unsupported; store not upgraded".into()),
    ] {
        let expected = error.to_string();
        let Err(actual) = episode_result(Err(error), false, false, 8) else {
            panic!("ordinary error was disguised as unavailable: {expected}");
        };
        assert!(matches!(&actual, ContextError::Retrieval(_)));
        assert_eq!(actual.to_string(), expected);
    }
}

#[test]
fn episodic_result_keeps_backend_partiality_and_adapted_cue_flags() {
    for (has_more, partial) in [(false, false), (true, false), (false, true)] {
        let page = mneme_core::episode::EpisodeCuePage {
            mode: mneme_core::episode::EpisodeCueMode::Lexical,
            items: Vec::new(),
            has_more,
            partial,
        };
        let (metadata, window) = episode_result(Ok(page), true, true, 8).unwrap();
        assert_eq!(window.further_tail_unknown(), has_more || partial);
        assert!(window.cards().is_empty());
        let metadata = serde_json::to_value(metadata).unwrap();
        assert_eq!(metadata["state"], "searched");
        assert_eq!(metadata["cue_normalized"], true);
        assert_eq!(metadata["cue_truncated"], true);
    }
}

#[tokio::test]
async fn episodic_result_refuses_an_adapter_exceeding_the_requested_window() {
    let mem = fixture();
    let result = mem
        .append_episode(episode_write("oversized-page", "violet lantern evening"))
        .await
        .unwrap();
    let node = mem
        .get_node(result.identity.edition_id)
        .await
        .unwrap()
        .unwrap();
    let header = mneme_core::episode::EpisodeHeader::from_node(&node, node.id()).unwrap();
    let page = mneme_core::episode::EpisodeCuePage {
        mode: mneme_core::episode::EpisodeCueMode::Lexical,
        items: vec![header; 9],
        has_more: false,
        partial: false,
    };
    assert!(matches!(
        episode_result(Ok(page), false, false, 8),
        Err(ContextError::Retrieval(Error::Backend(_)))
    ));
}

fn graph_fixture(graph_weight: f32) -> Memory {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            lexical_k: 0,
            graph_seed_cap: 1,
            graph_slot_cap: 1,
            graph_weight,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()))
    .with_lexical_index(store)
}

fn graph_budget() -> Budget {
    Budget {
        max_depth: 2,
        ..retrieval_budget()
    }
}

async fn incoming_graph_fixture() -> (Memory, NodeId, NodeId) {
    let mem = graph_fixture(0.9);
    let root = semantic(&mem, "violet lantern", true, &[]).await;
    let target = semantic(&mem, "optional context should fail softly", true, &[]).await;
    // The association is traversable from root, but the stored arrow points in
    // the opposite direction. Provenance must preserve both facts separately.
    mem.link(target, root, mneme_core::EdgeKind::Associative, 0.95, None)
        .await
        .unwrap();
    (mem, root, target)
}

#[tokio::test]
async fn default_graph_capacity_reaches_context_without_inventing_delivery() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            lexical_k: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let root = semantic(&mem, "violet lantern", true, &[]).await;
    let mut targets = Vec::new();
    // These are deliberately different claims, including opposing statements.
    // No similarity-based equivalence is introduced by capacity-scaled admission.
    for text in [
        "Restart requires closing the local server",
        "Restart does not require closing the local server",
        "A weak analogy can identify a useful escape route",
        "An old failure can explain a new symptom",
    ] {
        let target = semantic(&mem, text, true, &[]).await;
        mem.link(target, root, mneme_core::EdgeKind::Associative, 0.95, None)
            .await
            .unwrap();
        targets.push(target);
    }
    for (max_nodes, expected) in [(3, 3), (5, 5), (8, 5)] {
        let retrieval = Budget {
            max_nodes,
            max_depth: 1,
            ..Config::default().budget
        };
        let budget = presentation_budget(8192, 8);
        let plain = recall_context(&mem, "violet lantern", 1, retrieval, &[], &budget)
            .await
            .unwrap();
        let observed =
            recall_context_routed(&mem, "violet lantern", 1, retrieval, &[], &budget, &[])
                .await
                .unwrap();
        assert_eq!(observed.plan.rendered_content(), plain.rendered_content());
        assert_eq!(observed.plan.manifest(), plain.manifest());
        assert_eq!(observed.observations.len(), expected);
        assert_eq!(observed.plan.manifest().cards()[0].node_id(), root);
        for observation in &observed.observations {
            let card = observed
                .plan
                .manifest()
                .cards()
                .iter()
                .find(|card| card.node_id() == observation.node_id)
                .unwrap();
            assert_eq!(observation.card_sha256, card.card_sha256());
            if observation.node_id == root {
                assert!(observation.graph_path.is_none());
                assert!(observation.routing_binding.is_none());
            } else {
                assert!(targets.contains(&observation.node_id));
                let path = observation.graph_path.as_ref().unwrap();
                assert_eq!(path.len(), 1);
                assert_eq!(path[0].previous, root);
                assert_eq!(path[0].target, observation.node_id);
                assert_eq!(path[0].edge.from, observation.node_id);
                assert_eq!(path[0].edge.to, root);
                let binding = observation.routing_binding.as_ref().unwrap();
                assert_eq!(binding.previous, root);
                assert_eq!(binding.target, observation.node_id);
                assert_eq!(binding.edge_from, observation.node_id);
                assert_eq!(binding.edge_to, root);
            }
        }
        if max_nodes >= 5 {
            for target in &targets {
                assert!(observed.observations.iter().any(|o| o.node_id == *target));
            }
        }

        // Native admission is not emitted context. A smaller final packet must
        // not credit the additional discovered routes as delivered observations.
        let packed = recall_context_routed(
            &mem,
            "violet lantern",
            1,
            retrieval,
            &[],
            &presentation_budget(8192, 2),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(packed.plan.manifest().cards().len(), 2);
        assert_eq!(packed.observations.len(), 2);
        assert_eq!(packed.observations[0].node_id, root);
        assert_eq!(
            packed
                .observations
                .iter()
                .filter(|o| o.graph_path.is_some())
                .count(),
            1
        );
        assert!(packed.plan.rendered_content().len() <= 8192);
    }
}

#[tokio::test]
async fn observed_context_preserves_plain_output_and_actual_incoming_graph_route() {
    let (mem, root, target) = incoming_graph_fixture().await;
    let budget = presentation_budget(8192, 4);
    let plain = recall_context(&mem, "violet lantern", 1, graph_budget(), &[], &budget)
        .await
        .unwrap();
    let observed = recall_context_observed(&mem, "violet lantern", 1, graph_budget(), &[], &budget)
        .await
        .unwrap();
    assert_eq!(observed.plan.rendered_content(), plain.rendered_content());
    assert_eq!(observed.plan.manifest(), plain.manifest());
    assert_eq!(observed.observations.len(), plain.manifest().cards().len());
    assert_eq!(observed.observations.len(), 2);

    let root_observation = observed
        .observations
        .iter()
        .find(|observation| observation.node_id == root)
        .unwrap();
    assert!(root_observation.graph_path.is_none());

    let target_observation = observed
        .observations
        .iter()
        .find(|observation| observation.node_id == target)
        .expect("k=1 admits only the exact-query root directly; target needs the graph");
    let path = target_observation.graph_path.as_ref().unwrap();
    assert_eq!(path.len(), 1);
    assert_eq!(path[0].previous, root);
    assert_eq!(path[0].target, target);
    assert_eq!(path[0].edge.from, target);
    assert_eq!(path[0].edge.to, root);
    assert_eq!(path[0].edge.kind, mneme_core::EdgeKind::Associative);
    assert_eq!(path[0].edge.weight(), 0.95);
    for observation in &observed.observations {
        let card = plain
            .manifest()
            .cards()
            .iter()
            .find(|card| card.node_id() == observation.node_id)
            .unwrap();
        assert_eq!(observation.lane, card.lane());
        assert_eq!(observation.card_sha256, card.card_sha256());
    }
}

#[tokio::test]
async fn observed_context_excludes_graph_cards_dropped_by_final_packing() {
    let (mem, root, target) = incoming_graph_fixture().await;
    let full = recall_context_observed(
        &mem,
        "violet lantern",
        1,
        graph_budget(),
        &[],
        &presentation_budget(8192, 4),
    )
    .await
    .unwrap();
    assert!(
        full.observations
            .iter()
            .any(|observation| observation.node_id == target && observation.graph_path.is_some())
    );

    let budget = presentation_budget(8192, 1);
    let limited = recall_context_observed(&mem, "violet lantern", 1, graph_budget(), &[], &budget)
        .await
        .unwrap();
    let plain = recall_context(&mem, "violet lantern", 1, graph_budget(), &[], &budget)
        .await
        .unwrap();
    assert_eq!(limited.plan.rendered_content(), plain.rendered_content());
    assert_eq!(limited.plan.manifest().cards().len(), 1);
    assert_eq!(limited.plan.manifest().cards()[0].node_id(), root);
    assert_eq!(limited.observations.len(), 1);
    assert_eq!(limited.observations[0].node_id, root);
    assert!(limited.observations[0].graph_path.is_none());
    assert_eq!(
        limited.observations[0].card_sha256,
        limited.plan.manifest().cards()[0].card_sha256()
    );
}

#[tokio::test]
async fn observed_context_excludes_graph_cards_removed_by_semantic_dedup() {
    let mem = graph_fixture(0.9);
    let root = semantic(&mem, "violet lantern", true, &[]).await;
    let target = semantic(&mem, "violet lantern practical lesson", true, &[]).await;
    mem.link(target, root, mneme_core::EdgeKind::Associative, 0.95, None)
        .await
        .unwrap();
    let budget = presentation_budget(8192, 4);
    // k=1 seeds only the exact-query root. With dedup disabled the second card
    // must arrive via the graph, and the generous packing budget retains both.
    let full = recall_context_observed(&mem, "violet lantern", 1, graph_budget(), &[], &budget)
        .await
        .unwrap();
    assert_eq!(full.plan.manifest().cards().len(), 2);
    assert!(
        full.observations
            .iter()
            .any(|observation| observation.node_id == target && observation.graph_path.is_some())
    );

    let dedup_budget = Budget {
        dedup_similarity: 0.6,
        ..graph_budget()
    };
    let observed = recall_context_observed(&mem, "violet lantern", 1, dedup_budget, &[], &budget)
        .await
        .unwrap();
    let plain = recall_context(&mem, "violet lantern", 1, dedup_budget, &[], &budget)
        .await
        .unwrap();
    assert_eq!(observed.plan.rendered_content(), plain.rendered_content());
    assert_eq!(observed.plan.manifest(), plain.manifest());
    assert_eq!(observed.plan.manifest().cards().len(), 1);
    assert_eq!(observed.plan.manifest().cards()[0].node_id(), root);
    assert_eq!(observed.observations.len(), 1);
    assert_eq!(observed.observations[0].node_id, root);
    assert!(observed.observations[0].graph_path.is_none());
    assert_eq!(
        observed.observations[0].card_sha256,
        observed.plan.manifest().cards()[0].card_sha256()
    );
}

#[tokio::test]
async fn observed_context_does_not_invent_routes_for_direct_or_episodic_cards() {
    let mem = fixture();
    let direct = semantic(&mem, "violet lantern lesson", true, &[]).await;
    let episode = mem
        .append_episode(episode_write("observed-scene", "violet lantern evening"))
        .await
        .unwrap();
    let budget = presentation_budget(8192, 4);
    let plain = recall_context(&mem, "violet lantern", 8, retrieval_budget(), &[], &budget)
        .await
        .unwrap();
    let observed =
        recall_context_observed(&mem, "violet lantern", 8, retrieval_budget(), &[], &budget)
            .await
            .unwrap();
    assert_eq!(observed.plan.rendered_content(), plain.rendered_content());
    assert_eq!(observed.plan.manifest(), plain.manifest());
    assert_eq!(observed.observations.len(), 2);
    for (id, lane) in [
        (direct, Lane::Primary),
        (episode.identity.edition_id, Lane::Episodic),
    ] {
        let observation = observed
            .observations
            .iter()
            .find(|observation| observation.node_id == id)
            .unwrap();
        assert_eq!(observation.lane, lane);
        assert!(observation.graph_path.is_none());
        let card = plain
            .manifest()
            .cards()
            .iter()
            .find(|card| card.node_id() == id)
            .unwrap();
        assert_eq!(observation.card_sha256, card.card_sha256());
    }
}

#[tokio::test]
async fn observed_context_does_not_credit_graph_reached_but_unpromoted_direct_hits() {
    let mem = graph_fixture(0.1);
    let root = semantic(&mem, "violet lantern", true, &[]).await;
    let target = semantic(&mem, "violet lantern practical lesson", true, &[]).await;
    mem.link(root, target, mneme_core::EdgeKind::Associative, 0.95, None)
        .await
        .unwrap();
    let batch = mem
        .retrieve_batch_seeded(
            "violet lantern",
            2,
            graph_budget(),
            StatusFilter::default(),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(batch.primary[0].node.id(), root);
    let reached = batch
        .primary
        .iter()
        .find(|hit| hit.node.id() == target)
        .unwrap();
    assert!(reached.evidence.dense_rank.is_some());
    assert!(reached.evidence.graph_rank.is_some());

    let budget = presentation_budget(8192, 4);
    let observed = recall_context_observed(&mem, "violet lantern", 2, graph_budget(), &[], &budget)
        .await
        .unwrap();
    let plain = recall_context(&mem, "violet lantern", 2, graph_budget(), &[], &budget)
        .await
        .unwrap();
    assert_eq!(observed.plan.rendered_content(), plain.rendered_content());
    let observation = observed
        .observations
        .iter()
        .find(|observation| observation.node_id == target)
        .unwrap();
    assert!(
        observation.graph_path.is_none(),
        "being reached is not graph contribution: direct rank wins this fusion"
    );
}

#[tokio::test]
async fn immutable_window_repacking_conserves_candidates_and_provenance() {
    let (mem, root, target) = incoming_graph_fixture().await;
    let window = prepare_context_window(
        &mem,
        "violet lantern",
        1,
        graph_budget(),
        &[],
        true,
        Some(&[]),
    )
    .await
    .unwrap();
    let full = window.pack(&presentation_budget(8192, 4)).unwrap();
    let limited = window.pack(&presentation_budget(8192, 1)).unwrap();
    let full_again = window.pack(&presentation_budget(8192, 4)).unwrap();
    assert_eq!(full.plan, full_again.plan);
    let original = value(&full.plan);
    let smaller = value(&limited.plan);
    for response in [&original, &smaller] {
        assert_eq!(
            response["primary"].as_array().unwrap().len() as u64
                + response["omitted"]["primary"]["bounded_window_budget"]
                    .as_u64()
                    .unwrap(),
            2
        );
        assert_eq!(response["omitted"]["primary"]["further_tail_unknown"], true);
    }
    assert_eq!(limited.observations.len(), 1);
    assert_eq!(limited.observations[0].node_id, root);
    assert_eq!(smaller["omitted"]["primary"]["bounded_window_budget"], 1);
    for context in [&full, &full_again] {
        let observation = context
            .observations
            .iter()
            .find(|row| row.node_id == target)
            .unwrap();
        assert_eq!(observation.graph_path.as_ref().unwrap()[0].target, target);
        assert_eq!(observation.routing_binding.as_ref().unwrap().target, target);
        let card = context
            .plan
            .manifest()
            .cards()
            .iter()
            .find(|row| row.node_id() == target)
            .unwrap();
        assert_eq!(observation.card_sha256, card.card_sha256());
    }
    // Later writes cannot introduce a candidate into an already prepared window.
    semantic(&mem, "violet lantern later arrival", true, &[]).await;
    assert_eq!(
        window.pack(&presentation_budget(8192, 4)).unwrap().plan,
        full.plan
    );
}

#[tokio::test]
async fn conditional_context_observes_only_emitted_winners_without_graph_paths() {
    use mneme_core::ports::{GraphStore, SignedRoutingBias};
    use mneme_core::{Edge, EdgeKind};

    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            lexical_k: 0,
            graph_seed_cap: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let root = semantic(&mem, "violet lantern", true, &[]).await;
    let target = semantic(
        &mem,
        "reserve both climbing anchors before starting",
        true,
        &[],
    )
    .await;
    let edge = Edge::new(root, target, 0.8, EdgeKind::Transition, 1);
    store.put_edge(&edge).await.unwrap();
    let root_node = mem.get_node(root).await.unwrap().unwrap();
    let target_node = mem.get_node(target).await.unwrap().unwrap();
    let binding = RoutingBinding::new(&root_node, &target_node, &edge);
    let hints = [RoutingHint {
        route: binding.clone(),
        sign: SignedRoutingBias::Boost,
    }];
    let window = prepare_context_window(
        &mem,
        "violet lantern",
        1,
        graph_budget(),
        &[],
        true,
        Some(&hints),
    )
    .await
    .unwrap();
    let budget = presentation_budget(8192, 4);
    let full = window.pack(&budget).unwrap();
    assert_eq!(full.observations.len(), 2);
    assert_eq!(full.observations[0].node_id, root);
    assert!(full.observations[0].conditional_binding.is_none());
    let conditional = full
        .observations
        .iter()
        .find(|card| card.node_id == target)
        .unwrap();
    assert_eq!(conditional.lane, Lane::Primary);
    assert_eq!(conditional.conditional_binding.as_ref(), Some(&binding));
    assert!(conditional.graph_path.is_none());
    assert!(conditional.routing_binding.is_none());
    let emitted = full
        .plan
        .manifest()
        .cards()
        .iter()
        .find(|card| card.node_id() == target)
        .unwrap();
    assert_eq!(conditional.card_sha256, emitted.card_sha256());

    let limited = window.pack(&presentation_budget(8192, 1)).unwrap();
    assert_eq!(limited.observations.len(), 1);
    assert_eq!(limited.observations[0].node_id, root);
    assert!(limited.observations[0].conditional_binding.is_none());
    assert_eq!(
        value(&limited.plan)["omitted"]["primary"]["bounded_window_budget"],
        1
    );
    let again = window.pack(&budget).unwrap();
    assert_eq!(again.plan, full.plan);
    assert_eq!(
        again.observations[1].conditional_binding.as_ref(),
        Some(&binding)
    );

    let unobserved = prepare_context_window(
        &mem,
        "violet lantern",
        1,
        graph_budget(),
        &[],
        false,
        Some(&hints),
    )
    .await
    .unwrap()
    .pack(&budget)
    .unwrap();
    assert_eq!(unobserved.plan, full.plan);
    assert!(unobserved.observations.is_empty());
    let plain = recall_context(&mem, "violet lantern", 1, graph_budget(), &[], &budget)
        .await
        .unwrap();
    assert_eq!(plain.manifest().cards().len(), 1);
}

#[tokio::test]
async fn conditional_projection_excludes_episodes_and_losing_graph_paths() {
    let (mem, root, target) = incoming_graph_fixture().await;
    let episode = mem
        .append_episode(episode_write(
            "conditional-projection",
            "violet lantern account",
        ))
        .await
        .unwrap();
    let mut window = prepare_context_window(
        &mem,
        "violet lantern",
        1,
        graph_budget(),
        &[],
        true,
        Some(&[]),
    )
    .await
    .unwrap();
    let budget = presentation_budget(8192, 8);
    let before = window.pack(&budget).unwrap();
    let binding = before
        .observations
        .iter()
        .find(|card| card.node_id == target)
        .unwrap()
        .routing_binding
        .clone()
        .unwrap();
    let routing = window.routing.as_mut().unwrap();
    routing.conditional_bindings.insert(target, binding.clone());
    // Defensive projection: a non-primary entry must never acquire semantic
    // conditional provenance, even if an upstream map contains that ID.
    routing
        .conditional_bindings
        .insert(episode.identity.edition_id, binding.clone());
    let projected = window.pack(&budget).unwrap();
    let root = projected
        .observations
        .iter()
        .find(|card| card.node_id == root)
        .unwrap();
    assert!(root.conditional_binding.is_none());
    let conditional = projected
        .observations
        .iter()
        .find(|card| card.node_id == target)
        .unwrap();
    assert_eq!(conditional.conditional_binding.as_ref(), Some(&binding));
    assert!(conditional.graph_path.is_none());
    assert!(conditional.routing_binding.is_none());
    let episodic = projected
        .observations
        .iter()
        .find(|card| card.node_id == episode.identity.edition_id)
        .unwrap();
    assert_eq!(episodic.lane, Lane::Episodic);
    assert!(episodic.conditional_binding.is_none());
    assert!(episodic.graph_path.is_none());
    assert!(episodic.routing_binding.is_none());
}

#[tokio::test]
async fn request_sized_window_packs_more_than_thirteen_short_cards() {
    let mem = fixture();
    for n in 0..20 {
        semantic(&mem, &format!("short distinct lesson {n}"), true, &[]).await;
    }
    let window = prepare_context_window(
        &mem,
        "short distinct lesson",
        20,
        Budget {
            max_nodes: 20,
            max_depth: 0,
            min_relevance: 0.0,
            ..Budget::default()
        },
        &[],
        true,
        None,
    )
    .await
    .unwrap();
    let disabled = LaneLimit::new(0, 0, 0, 0).unwrap();
    let budget = PresentationBudget::new(
        NonZeroU32::new(32768).unwrap(),
        NonZeroU32::new(PresentationBudget::minimum_control_reserve_bytes()).unwrap(),
        NonZeroU16::new(24).unwrap(),
        NonZeroU16::new(512).unwrap(),
        BodyBudget::disabled(),
        LaneBudgets::new(
            disabled,
            LaneLimit::new(1, 20, 20, 32768).unwrap(),
            disabled,
        )
        .with_episodic(LaneLimit::new(0, 2, 4, 32768).unwrap()),
    )
    .unwrap();
    let packed = window.pack(&budget).unwrap();
    assert_eq!(packed.plan.manifest().cards().len(), 20);
    assert_eq!(packed.observations.len(), 20);
    assert_eq!(
        value(&packed.plan)["omitted"]["primary"]["bounded_window_budget"],
        0
    );
    assert!(
        packed
            .plan
            .manifest()
            .cards()
            .windows(2)
            .all(|pair| pair[0].rank() < pair[1].rank())
    );
}

#[test]
fn linked_capacity_and_interleave_do_not_encode_a_scene_quota() {
    assert_eq!(lexical_capacity(0), 0);
    assert_eq!(lexical_capacity(3), 3);
    assert_eq!(lexical_capacity(16), 16);
    assert_eq!(lexical_capacity(usize::MAX), MAX_EPISODE_PAGE_ITEMS);
    assert_eq!(interleave(vec![1, 3, 5], vec![2, 4]), vec![1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn linked_recall_keeps_exact_old_and_corrected_accounts_and_joke_without_a_lesson() {
    use mneme_core::episode::{OccurrenceContextRef, OccurrenceContexts};
    let (mem, store) = fixture_with_store();
    let a = semantic(&mem, "violet workshop first lesson", true, &[]).await;
    let b = semantic(&mem, "violet workshop second lesson", true, &[]).await;
    let coordinates = OccurrenceContexts::new(vec![
        OccurrenceContextRef::new("device", "Pi", Some("Pi workshop")).unwrap(),
        OccurrenceContextRef::new("room", "shared-workbench", None).unwrap(),
    ])
    .unwrap();
    let mut old_write = episode_write("old-account", "The lamps were red; recorded late on Pi");
    old_write.session = Some("pi-session/exact opaque");
    let old = mem
        .append_episode(old_write.with_occurrence_contexts(coordinates))
        .await
        .unwrap();
    let mut correction_write = episode_write("new-account", "violet workshop: the lamps were blue");
    correction_write.session = Some("mac-session/exact opaque");
    let current = mem
        .revise_episode(
            old.identity.episode_id,
            old.identity.edition_id,
            EpisodeRevisionReason::new("We checked the lamp, not the earlier narrator.").unwrap(),
            correction_write,
        )
        .await
        .unwrap();
    mem.link(
        old.identity.edition_id,
        a,
        mneme_core::EdgeKind::DerivedFrom,
        0.8,
        None,
    )
    .await
    .unwrap();
    mem.link(
        b,
        old.identity.edition_id,
        mneme_core::EdgeKind::Associative,
        0.5,
        None,
    )
    .await
    .unwrap();
    mem.link(
        a,
        current.identity.edition_id,
        mneme_core::EdgeKind::Associative,
        0.5,
        None,
    )
    .await
    .unwrap();
    let joke = mem
        .append_episode(episode_write(
            "violet-joke",
            "violet workshop: the moth filed a grievance",
        ))
        .await
        .unwrap();
    let travelled = mem
        .append_episode(episode_write(
            "travelled-joke",
            "The moth became an operations manager",
        ))
        .await
        .unwrap();
    let beyond = mem
        .append_episode(episode_write("not-recursed", "Its pension is a lamp"))
        .await
        .unwrap();
    mem.link(
        joke.identity.edition_id,
        travelled.identity.edition_id,
        mneme_core::EdgeKind::Transition,
        0.5,
        None,
    )
    .await
    .unwrap();
    mem.link(
        travelled.identity.edition_id,
        beyond.identity.edition_id,
        mneme_core::EdgeKind::Associative,
        0.5,
        None,
    )
    .await
    .unwrap();
    let before = serde_json::to_value(store.export()).unwrap();
    let budget = presentation_budget(32768, 16);
    let window = prepare_context_window(
        &mem,
        "violet workshop",
        8,
        retrieval_budget(),
        &[],
        true,
        None,
    )
    .await
    .unwrap();
    let observed = window.pack(&budget).unwrap();
    let context = value(&observed.plan);
    assert_eq!(context["schema"], "mneme.context.v7");
    let scenes = context["episodes"].as_array().unwrap();
    let old_card = scenes
        .iter()
        .find(|card| card["edition_id"] == json!(old.identity.edition_id))
        .unwrap();
    let new_card = scenes
        .iter()
        .find(|card| card["edition_id"] == json!(current.identity.edition_id))
        .unwrap();
    assert_eq!(old_card["episode_id"], new_card["episode_id"]);
    assert_eq!(old_card["current_edition_id"], new_card["edition_id"]);
    assert_eq!(old_card["recording_session"], "pi-session/exact opaque");
    assert_eq!(new_card["recording_session"], "mac-session/exact opaque");
    assert_eq!(old_card["occurrence_contexts"].as_array().unwrap().len(), 2);
    assert!(new_card.get("occurrence_contexts").is_none());
    assert_eq!(old_card["origins"].as_array().unwrap().len(), 2);
    assert!(
        old_card["origins"]
            .as_array()
            .unwrap()
            .iter()
            .all(|origin| origin["kind"] == "reference")
    );
    assert!(
        old_card["origins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|origin| origin["from"] == json!(old.identity.edition_id)
                && origin["to"] == json!(a))
    );
    assert!(
        new_card["origins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|origin| origin["kind"] == "lexical")
    );
    assert!(
        new_card["origins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|origin| origin["kind"] == "reference")
    );
    let travelled_card = scenes
        .iter()
        .find(|card| card["edition_id"] == json!(travelled.identity.edition_id))
        .unwrap();
    assert_eq!(travelled_card["origins"][0]["anchor"]["kind"], "episode");
    assert_eq!(
        travelled_card["origins"][0]["anchor"]["identity"]["edition_id"],
        json!(joke.identity.edition_id)
    );
    assert!(
        !scenes
            .iter()
            .any(|card| card["edition_id"] == json!(beyond.identity.edition_id)),
        "new discoveries became recursive anchors"
    );
    assert!(!observed.plan.rendered_content().contains("body must never"));
    assert!(
        observed
            .observations
            .iter()
            .filter(|observation| observation.lane == Lane::Episodic)
            .all(|observation| observation.graph_path.is_none()
                && observation.routing_binding.is_none())
    );
    let small = window.pack(&presentation_budget(8192, 2)).unwrap();
    assert!(small.plan.manifest().cards().len() <= 2);
    assert_eq!(
        serde_json::to_value(store.export()).unwrap(),
        before,
        "ordinary linked read/repack mutated graph or feedback"
    );
}

#[tokio::test]
async fn tagged_semantic_anchor_can_yield_indirect_scene_without_claiming_episode_tags() {
    let mem = fixture();
    let anchor = semantic(&mem, "violet tagged lesson", true, &["scope"]).await;
    let scene = mem
        .append_episode(episode_write("indirect", "A small unrelated hallway event"))
        .await
        .unwrap();
    mem.link(
        scene.identity.edition_id,
        anchor,
        mneme_core::EdgeKind::DerivedFrom,
        0.5,
        None,
    )
    .await
    .unwrap();
    let plan = recall_context(
        &mem,
        "violet",
        8,
        retrieval_budget(),
        &["scope"],
        &presentation_budget(8192, 8),
    )
    .await
    .unwrap();
    let context = value(&plan);
    assert_eq!(
        context["episodic_retrieval"]["state"],
        "not_searched_tag_filter"
    );
    assert_eq!(context["episodes"].as_array().unwrap().len(), 1);
    assert_eq!(
        context["episodes"][0]["edition_id"],
        json!(scene.identity.edition_id)
    );
    assert!(
        context["episodes"][0]["origins"]
            .as_array()
            .unwrap()
            .iter()
            .all(|origin| origin["kind"] == "reference")
    );
}
