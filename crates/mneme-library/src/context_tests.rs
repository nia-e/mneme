use super::tests::{fixture, query};
use super::*;
use std::sync::Mutex;

fn semantic(id: &str, rank: usize) -> Value {
    json!({"id":id,"rank":rank,"summary":{"text":"summary only","complete":true,"source_bytes":12},"content_trust":"untrusted"})
}
fn episode_id(name: &str) -> String {
    // Genuine canonical ULID text without another test-only dependency.
    let value = name.bytes().fold(1_u64, |state, byte| {
        state.wrapping_mul(31).wrapping_add(u64::from(byte))
    });
    format!("{value:026}")
}
fn episode(id: &str, rank: usize) -> Value {
    let id = episode_id(id);
    json!({"kind":"episode","id":id,"rank":rank,"summary":{"text":"episode only","complete":true,"source_bytes":12},"content_trust":"untrusted","episode_id":id,"edition_id":id,"revision":0,"current_edition_id":id,"occurred":{"kind":"point","at":42},"recorded_at":50,"edition_recorded_at":50,"thread":"workshop", "recording_session":null,"origins":[{"kind":"lexical"}]})
}
fn reference_coverage(searched: bool) -> Value {
    json!({"state":if searched {"searched"} else {"not_searched"},
        "anchors_total":0,"anchors_examined":0,"raw_edges_scanned":0,"raw_edge_limit":0,
        "endpoint_reads":0,"endpoint_read_limit":0,"indexed_seeks":0,"edge_point_reads":0,
        "body_anchor_point_reads":0,"missing":0,"non_episode":0,"cache_hits":0,
        "unread_anchors":0,"further_tail_unknown":false})
}
fn touchstone_coverage() -> Value {
    serde_json::to_value(mneme_present::TouchstoneRetrieval::default()).unwrap()
}

fn native_omissions() -> Value {
    let lane = json!({"bounded_window_budget":0,"further_tail_unknown":false});
    json!({"core":lane,"primary":lane,"expansion":lane,"episodic":lane})
}
pub(super) fn owner_context() -> Value {
    json!({"schema":"mneme.context.v7","touchstone_retrieval":touchstone_coverage(),"partial":false,"core":[],"primary":[semantic("node-1",1)],"expansions":[],"episodes":[episode("edition-1",1)],"episodic_retrieval":{"state":"searched","mode":"lexical","cue_normalized":false,"cue_truncated":false},"episode_reference_retrieval":reference_coverage(false),"omitted":native_omissions()})
}

fn maximum_occurrence_contexts() -> Value {
    use mneme_core::episode::{MAX_OCCURRENCE_CONTEXTS_JSON_BYTES, OccurrenceContexts};
    let mut contexts = json!([
        {"namespace":"atelier\\\"🦋", "key":"shared-scene", "label":"x"},
        {"namespace":"conversation", "key":"violet-evening"},
    ]);
    let minimum_bytes = serde_json::to_vec(&contexts).unwrap().len();
    contexts[0]["label"] =
        json!("x".repeat(MAX_OCCURRENCE_CONTEXTS_JSON_BYTES - minimum_bytes + 1));
    let typed: OccurrenceContexts = serde_json::from_value(contexts.clone()).unwrap();
    assert_eq!(typed.compact_json_len(), MAX_OCCURRENCE_CONTEXTS_JSON_BYTES);
    assert_eq!(
        serde_json::to_vec(&contexts).unwrap().len(),
        MAX_OCCURRENCE_CONTEXTS_JSON_BYTES
    );
    contexts
}

#[test]
fn occurrence_contexts_are_typed_canonical_and_optional_not_null() {
    let old = owner_context();
    context::validate(&old, 64).unwrap();
    let projected = context::project(&old["episodes"][0], "p", "d", &json!({}), "episodic");
    assert!(projected.get("occurrence_contexts").is_none());
    let mut value = old.clone();
    value["episodes"][0]["occurrence_contexts"] = maximum_occurrence_contexts();
    context::validate(&value, 64).unwrap();
    let projected = context::project(&value["episodes"][0], "p", "d", &json!({}), "episodic");
    assert_eq!(
        projected["occurrence_contexts"],
        value["episodes"][0]["occurrence_contexts"]
    );
    assert_eq!(projected["thread"], "workshop");
    for invalid in [
        json!(null),
        json!([]),
        json!({"namespace":"conversation", "key":"x"}),
        json!([{"namespace":"conversation", "key":"x", "label":null}]),
        json!([{"namespace":"conversation", "key":"x", "unexpected":true}]),
        json!([{"namespace":" conversation", "key":"x"}]),
        json!([{"namespace":"conversation", "key":" "}]),
        json!([{"namespace":"conversation", "key":"x", "label":"\n"}]),
        json!([{"namespace":"conversation", "key":"x"}, {"namespace":"conversation", "key":"x", "label":"different label"}]),
        json!([{"namespace":"z", "key":"x"}, {"namespace":"a", "key":"x"}]),
        json!([{"namespace":"conversation", "key":"x".repeat(1024)}]),
    ] {
        let mut value = old.clone();
        value["episodes"][0]["occurrence_contexts"] = invalid.clone();
        assert!(context::validate(&value, 64).is_err(), "accepted {invalid}");
    }
    let mut semantic = old;
    semantic["primary"][0]["occurrence_contexts"] = maximum_occurrence_contexts();
    assert!(context::validate(&semantic, 64).is_err());
}

#[tokio::test]
async fn library_projection_preserves_maximum_occurrence_contexts_as_one_account() {
    let (rt, fake, path) = fixture();
    let mut source = owner_context();
    let contexts = maximum_occurrence_contexts();
    source["episodes"][0]["occurrence_contexts"] = contexts.clone();
    fake.contexts.lock().unwrap().insert("owner".into(), source);
    let value = rt.recall_context(query()).await.unwrap();
    assert_eq!(value["episodes"][0]["occurrence_contexts"], contexts);
    assert_eq!(value["episodes"][0]["thread"], "workshop");
    assert!(serde_json::to_vec(&value).unwrap().len() <= MAX_CONTEXT_BYTES);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn invalid_occurrence_context_refuses_owner_without_snapshot_fallback() {
    let (rt, fake, path) = fixture();
    let mut source = owner_context();
    source["episodes"][0]["occurrence_contexts"] = json!([]);
    fake.contexts.lock().unwrap().insert("owner".into(), source);
    let value = rt.recall_context(query()).await.unwrap();
    assert_eq!(value["coverage"][0]["state"], "refused");
    assert!(value["episodes"].as_array().unwrap().is_empty());
    assert!(
        !fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(url, _)| url == "replica")
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn mixed_context_uses_owner_context_and_pins_edition_for_get() {
    let (rt, fake, path) = fixture();
    let value = rt.recall_context(query()).await.unwrap();
    assert_eq!(value["schema"], "mneme.library.context.v5");
    assert_eq!(value["episodes"][0]["kind"], "episode");
    assert_eq!(value["episodes"][0]["thread"], "workshop");
    assert_eq!(value["episodes"][0]["occurred"]["at"], 42);
    assert_eq!(value["episodes"][0]["recorded_at"], 50);
    assert_eq!(
        value["episodes"][0]["id"],
        value["episodes"][0]["edition_id"]
    );
    assert_eq!(value["coverage"][0]["episodic"]["state"], "searched");
    assert_eq!(
        value["primary"][0]["source"],
        value["episodes"][0]["source"]
    );
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, tool)| tool.as_str())
            .collect::<Vec<_>>(),
        ["databases", "recall_context"]
    );
    let request = fake.requests.lock().unwrap()[1].1.clone();
    assert!(request.get("candidates").is_none());
    let episode = &value["episodes"][0];
    let got = rt
        .get(LibraryGet {
            project_id: "p".into(),
            id: episode["id"].as_str().unwrap().into(),
            source_device_id: "owner".into(),
            generation: None,
        })
        .await
        .unwrap();
    assert_eq!(got["node"]["id"], episode_id("edition-1"));
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn old_owner_context_is_refused_without_snapshot_mix() {
    let (rt, fake, path) = fixture();
    let mut old = owner_context();
    old["schema"] = json!("mneme.context.v3");
    old.as_object_mut().unwrap().remove("episodes");
    old.as_object_mut().unwrap().remove("episodic_retrieval");
    fake.contexts.lock().unwrap().insert("owner".into(), old);
    let value = rt.recall_context(query()).await.unwrap();
    assert_eq!(value["coverage"][0]["state"], "refused");
    assert_eq!(value["partial"], true);
    assert!(value["episodes"].as_array().unwrap().is_empty());
    assert!(
        !fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(url, _)| url == "replica")
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn snapshot_fallback_moves_both_lanes_together_but_refusal_never_falls_back() {
    let (rt, fake, path) = fixture();
    fake.errors.lock().unwrap().insert(
        "owner".into(),
        SourceFailure::Availability("offline".into()),
    );
    let value = rt.recall_context(query()).await.unwrap();
    assert_eq!(
        value["primary"][0]["source"],
        value["episodes"][0]["source"]
    );
    assert_eq!(value["episodes"][0]["source"]["generation"], "g1");
    fake.errors.lock().unwrap().clear();
    fake.calls.lock().unwrap().clear();
    let mut bad = owner_context();
    bad["episodes"][0]["body"] = json!("not a summary");
    fake.contexts.lock().unwrap().insert("owner".into(), bad);
    let value = rt.recall_context(query()).await.unwrap();
    assert_eq!(value["coverage"][0]["state"], "refused");
    assert!(value["primary"].as_array().unwrap().is_empty());
    assert!(value["episodes"].as_array().unwrap().is_empty());
    assert!(
        !fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(url, _)| url == "replica")
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn tag_filter_and_unsupported_coverage_are_not_empty_success() {
    let (rt, fake, path) = fixture();
    for state in ["not_searched_tag_filter", "unavailable"] {
        let mut source = owner_context();
        source["episodes"] = json!([]);
        source["episodic_retrieval"]["state"] = json!(state);
        if state == "unavailable" {
            source["episodic_retrieval"]["unavailable_reason"] = json!("store_not_upgraded");
        }
        fake.contexts.lock().unwrap().insert("owner".into(), source);
        let value = rt.recall_context(query()).await.unwrap();
        assert_eq!(value["coverage"][0]["episodic"]["state"], state);
        assert_eq!(value["partial"], true);
        assert!(!value["primary"].as_array().unwrap().is_empty());
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn pooled_budget_reserves_two_episodes_without_increasing_item_limit() {
    for total in 1..=64 {
        let mut primary = vec![semantic("n", 1); 64];
        let mut episodes = vec![episode("e", 1); 64];
        context::trim_pool(&mut primary, &mut episodes, total);
        assert_eq!(primary.len() + episodes.len(), total);
        assert_eq!(
            episodes.len(),
            match total {
                1 => 0,
                2 => 1,
                _ => 2,
            }
        );
    }
    let mut primary = vec![semantic("n", 1); 12];
    let mut episodes = vec![episode("e", 1); 8];
    context::trim_items(&mut primary, &mut episodes);
    assert_eq!((primary.len(), episodes.len()), (11, 2));
    primary.truncate(1);
    episodes = vec![episode("e", 1); 8];
    context::trim_items(&mut primary, &mut episodes);
    assert_eq!(episodes.len(), 8);
}

struct SemanticOnlyRanker(Mutex<Vec<String>>);
#[async_trait]
impl Ranker for SemanticOnlyRanker {
    async fn rank(&self, _: &str, docs: &[String]) -> Result<Vec<f32>, String> {
        self.0.lock().unwrap().extend_from_slice(docs);
        Ok(vec![0.5; docs.len()])
    }
}
#[tokio::test]
async fn common_reranker_never_receives_episode_summaries() {
    let (rt, _, path) = fixture();
    let ranker = Arc::new(SemanticOnlyRanker(Mutex::new(Vec::new())));
    let value = rt
        .with_ranker(ranker.clone())
        .recall_context(query())
        .await
        .unwrap();
    assert_eq!(value["ordering"], "common_rerank");
    assert_eq!(*ranker.0.lock().unwrap(), ["summary only"]);
    assert_eq!(value["episodes"][0]["summary"], "episode only");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn unknown_schema_and_invalid_episode_identity_are_refused() {
    let mut value = owner_context();
    value["schema"] = json!("mneme.context.v4");
    assert!(context::validate(&value, 64).is_err());
    value = owner_context();
    value["episodes"][0]["edition_id"] = json!("other-edition");
    assert!(context::validate(&value, 64).is_err());
}

#[test]
fn empty_compatibility_probation_lane_is_refused() {
    let mut value = owner_context();
    value["probationary"] = json!([]);
    assert!(context::validate(&value, 64).is_err());
    let mut value = owner_context();
    value["omitted"]["probationary"] = json!(0);
    assert!(context::validate(&value, 64).is_err());
}

#[test]
fn revised_episode_can_have_an_earlier_host_timestamp() {
    let mut value = owner_context();
    value["episodes"][0]["episode_id"] = json!(episode_id("root-1"));
    value["episodes"][0]["revision"] = json!(1);
    value["episodes"][0]["edition_recorded_at"] = json!(40);
    context::validate(&value, 64).unwrap();
}

#[tokio::test]
async fn source_window_truncation_is_counted_and_pool_is_bounded_after_reranking() {
    let (mut rt, fake, path) = fixture();
    rt.config.limits.total_candidates = 2;
    let mut source = owner_context();
    source["primary"] = json!([semantic("n1", 1), semantic("n2", 2)]);
    source["episodes"] = json!([episode("e1", 1), episode("e2", 2)]);
    fake.contexts.lock().unwrap().insert("owner".into(), source);
    let value = rt.recall_context(query()).await.unwrap();
    assert_eq!(value["primary"].as_array().unwrap().len(), 1);
    assert_eq!(value["episodes"].as_array().unwrap().len(), 1);
    assert_eq!(value["omitted"], json!({"primary":1,"episodes":1}));
    assert_eq!(value["omission_scope"], "pooled_returned_windows");
    assert_eq!(value["partial"], true);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn metadata_overflow_is_actionable_and_never_silently_drops_coverage() {
    let (rt, _, path) = fixture();
    let mut cat: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for i in 1..64 {
        let mut entry = cat.entries[0].clone();
        entry.project_id = format!("{i}{}", "x".repeat(1000));
        cat.entries.push(entry);
    }
    std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
    let error = rt.recall_context(query()).await.unwrap_err().to_string();
    assert!(error.contains("metadata alone exceeds 32 KiB"));
    assert!(error.contains("--project-id"));
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn escaped_summaries_and_full_coverage_stay_inside_encoded_byte_budget() {
    let (rt, fake, path) = fixture();
    let mut source = owner_context();
    let mut s = semantic("n", 1);
    s["summary"] = json!({"text":"\\\"".repeat(500),"complete":true,"source_bytes":1000});
    source["primary"] = json!([s]);
    let mut e = episode("e", 1);
    e["summary"] = json!({"text":"\\\"".repeat(500),"complete":true,"source_bytes":1000});
    let contexts = maximum_occurrence_contexts();
    e["occurrence_contexts"] = contexts.clone();
    source["episodes"] = json!([e]);
    fake.contexts.lock().unwrap().insert("owner".into(), source);
    let mut cat: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for i in 1..16 {
        let mut entry = cat.entries[0].clone();
        entry.project_id = format!("project-{i:02}");
        cat.entries.push(entry);
    }
    std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
    let value = rt.recall_context(query()).await.unwrap();
    assert_eq!(value["coverage"].as_array().unwrap().len(), 16);
    assert!(serde_json::to_vec(&value).unwrap().len() <= MAX_CONTEXT_BYTES);
    assert_eq!(value["context_truncated"], true);
    assert_eq!(value["partial"], true);
    assert!(value["episodes"].as_array().unwrap().len() <= context::MAX_ITEMS);
    for episode in value["episodes"].as_array().unwrap() {
        assert_eq!(episode["occurrence_contexts"], contexts);
    }
    std::fs::remove_file(path).unwrap();
}

fn reference_episode(name: &str, anchor_name: &str) -> Value {
    let mut card = episode(name, 1);
    card["origins"] = json!([{
        "kind":"reference", "anchor":{"kind":"semantic","node_id":episode_id(anchor_name)},
        "from":episode_id(anchor_name),"to":card["edition_id"],
        "edge_kind":"Transition", "body_anchor":{"start":2,"end":17}
    }]);
    card
}

fn historical_reference_context() -> Value {
    let mut value = owner_context();
    let mut card = reference_episode("old-edition", "semantic-anchor");
    card["current_edition_id"] = json!(episode_id("correction"));
    card["recording_session"] = json!("pi-recorder-original");
    card["occurrence_contexts"] = json!([{"namespace":"conversation","key":"shared-scene"}]);
    value["episodes"] = json!([card]);
    value["episode_reference_retrieval"] = reference_coverage(true);
    value
}

#[test]
fn historical_reference_admission_is_exact_not_a_relaxed_lexical_rule() {
    let historical = historical_reference_context();
    context::validate(&historical, 64).unwrap();
    for case in 0..7 {
        let mut value = historical.clone();
        match case {
            0 => value["episodes"][0]["origins"][0]["to"] = json!(episode_id("wrong-edition")),
            1 => value["episodes"][0]["origins"] = json!([{"kind":"lexical"}]),
            2 => value["episodes"][0]["origins"] = json!([]),
            3 => {
                value["episodes"][0]["origins"][0]["anchor"]["node_id"] =
                    json!(episode_id("wrong-anchor"))
            }
            4 => {
                value["episodes"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("recording_session");
            }
            5 => {
                value["episodes"][0]["origins"][0]["anchor"] = json!({"kind":"episode","identity":{
                "episode_id":episode_id("root"),"edition_id":episode_id("semantic-anchor"),"revision":0}})
            }
            _ => value["episode_reference_retrieval"] = reference_coverage(false),
        }
        assert!(
            context::validate(&value, 64).is_err(),
            "case {case} admitted"
        );
    }
    let mut lexical = owner_context();
    lexical["episodic_retrieval"]["state"] = json!("not_searched");
    assert!(context::validate(&lexical, 64).is_err());
}

#[tokio::test]
async fn tagged_indirect_historical_scene_keeps_both_coverage_reports_and_all_metadata() {
    let (rt, fake, path) = fixture();
    let mut source = historical_reference_context();
    source["episodic_retrieval"]["state"] = json!("not_searched_tag_filter");
    source["episode_reference_retrieval"]["stop_reason"] = json!("budget");
    source["episode_reference_retrieval"]["further_tail_unknown"] = json!(true);
    let contexts = maximum_occurrence_contexts();
    source["episodes"][0]["occurrence_contexts"] = contexts.clone();
    let episode = source["episodes"][0].clone();
    let reference_work = source["episode_reference_retrieval"].clone();
    fake.contexts.lock().unwrap().insert("owner".into(), source);
    let mut request = query();
    request.tags = vec!["topic".into()];
    let result = rt.recall_context(request).await.unwrap();
    assert_eq!(result["schema"], "mneme.library.context.v5");
    assert_eq!(result["partial"], true);
    assert_eq!(
        result["coverage"][0]["episodic"]["state"],
        "not_searched_tag_filter"
    );
    assert_eq!(reconstructed_reference_report(&result, 0), &reference_work);
    assert_eq!(result["coverage"][0]["context_schema"], "mneme.context.v7");
    let projected = &result["episodes"][0];
    for field in [
        "episode_id",
        "edition_id",
        "current_edition_id",
        "revision",
        "recorded_at",
        "edition_recorded_at",
        "occurred",
        "thread",
        "recording_session",
        "occurrence_contexts",
        "origins",
    ] {
        assert_eq!(projected[field], episode[field], "lost {field}");
    }
    assert_ne!(projected["edition_id"], projected["current_edition_id"]);
    assert!(projected.get("tags").is_none());
    assert_eq!(projected["occurrence_contexts"], contexts);
    let mut nullable = historical_reference_context();
    nullable["episodes"][0]["recording_session"] = json!(null);
    context::validate(&nullable, 64).unwrap();
    assert!(
        context::project(&nullable["episodes"][0], "p", "d", &json!({}), "episodic")
            .get("recording_session")
            .unwrap()
            .is_null()
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn reference_coverage_and_origin_order_are_checked_by_the_native_contract() {
    let source = historical_reference_context();
    for field in ["raw_edges_scanned", "endpoint_reads", "unread_anchors"] {
        let mut bad = source.clone();
        bad["episode_reference_retrieval"][field] = json!(1);
        assert!(
            context::validate(&bad, 64).is_err(),
            "accepted impossible {field}"
        );
    }
    let mut bad = source.clone();
    bad["episode_reference_retrieval"]["stop_reason"] = json!("time-ish");
    assert!(context::validate(&bad, 64).is_err());
    let mut bad = source.clone();
    bad.as_object_mut()
        .unwrap()
        .remove("episode_reference_retrieval");
    assert!(context::validate(&bad, 64).is_err());
    let mut bad = source;
    let origin = bad["episodes"][0]["origins"][0].clone();
    bad["episodes"][0]["origins"] = json!([origin.clone(), origin]);
    assert!(context::validate(&bad, 64).is_err());
}

#[test]
fn account_dedup_uses_store_root_edition_keeps_first_view_and_canonical_origin_union() {
    let first = reference_episode("edition", "anchor-one");
    let second = reference_episode("edition", "anchor-two");
    let mut a = context::project(
        &first,
        "project-a",
        "db",
        &json!({"generation":"first"}),
        "episodic",
    );
    a["current_edition_id"] = json!(episode_id("observed-head-one"));
    let mut b = context::project(
        &second,
        "project-b",
        "db",
        &json!({"generation":"second"}),
        "episodic",
    );
    b["current_edition_id"] = json!(episode_id("observed-head-two"));
    let mut independent = b.clone();
    independent["db_id"] = json!("other-db");
    let mut later = context::project(
        &episode("later", 1),
        "project-a",
        "db",
        &json!({}),
        "episodic",
    );
    later["episode_id"] = a["episode_id"].clone();
    later["revision"] = json!(1);
    let mut cards = vec![a.clone(), b, independent, later];
    assert!(context::deduplicate_episodes(&mut cards).is_empty());
    assert_eq!(cards.len(), 3);
    assert_eq!(cards[0]["project_id"], "project-a");
    assert_eq!(cards[0]["source"], a["source"]);
    assert_eq!(cards[0]["current_edition_id"], a["current_edition_id"]);
    assert_eq!(cards[0]["origins"].as_array().unwrap().len(), 2);
    let typed: Vec<mneme_present::EpisodeOrigin> =
        serde_json::from_value(cards[0]["origins"].clone()).unwrap();
    assert_eq!(
        typed,
        mneme_present::canonical_episode_origins(typed.clone())
    );
    let mut conflict = cards[0].clone();
    conflict["project_id"] = json!("bad-owner");
    conflict["recording_session"] = json!("different-immutable-recorder");
    cards.push(conflict);
    assert_eq!(
        context::deduplicate_episodes(&mut cards),
        vec![("bad-owner".into(), "db".into())]
    );
    assert_eq!(cards.len(), 3);
}

#[tokio::test]
async fn owner_can_supply_more_than_four_scenes_inside_shared_packet_resources() {
    let (rt, fake, path) = fixture();
    let mut source = owner_context();
    source["primary"] = json!([]);
    source["episodes"] = json!(
        (0..10)
            .map(|n| episode(&format!("scene-{n}"), n + 1))
            .collect::<Vec<_>>()
    );
    fake.contexts.lock().unwrap().insert("owner".into(), source);
    let result = rt.recall_context(query()).await.unwrap();
    assert_eq!(result["episodes"].as_array().unwrap().len(), 10);
    assert_eq!(
        result["episodic_retrieval"]["max_items"],
        context::MAX_ITEMS
    );
    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_CONTEXT_BYTES);
    std::fs::remove_file(path).unwrap();
}

fn reconstructed_reference_report(packet: &Value, source_index: usize) -> &Value {
    assert_eq!(
        packet["episode_reference_coverage_encoding"],
        "native_report_index_v1"
    );
    let reference = &packet["coverage"][source_index]["episode_reference_retrieval"];
    let index = reference["report_index"]
        .as_u64()
        .expect("typed report index") as usize;
    let reports = packet["episode_reference_reports"].as_array().unwrap();
    assert!(index < reports.len());
    let report = &reports[index];
    for key in ["state", "further_tail_unknown", "stop_reason"] {
        assert_eq!(reference.get(key), report.get(key), "inline {key} differs");
    }
    report
}

#[tokio::test]
async fn sixty_four_realistic_searched_owner_reports_fit_without_losing_native_counters() {
    let (rt, fake, path) = fixture();
    let mut source = owner_context();
    let mut report = reference_coverage(true);
    // Each N=1 owner freezes one semantic + one lexical anchor and performs
    // outgoing/incoming empty seeks for both. No rows/endpoint reads are free.
    report["anchors_total"] = json!(2);
    report["anchors_examined"] = json!(2);
    report["raw_edge_limit"] = json!(2);
    report["endpoint_read_limit"] = json!(1);
    report["indexed_seeks"] = json!(4);
    source["episode_reference_retrieval"] = report.clone();
    fake.contexts.lock().unwrap().insert("owner".into(), source);
    let mut cat: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for i in 1..64 {
        let mut entry = cat.entries[0].clone();
        entry.project_id = format!("p{i}");
        cat.entries.push(entry);
    }
    std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
    let packet = rt.recall_context(query()).await.unwrap();
    assert_eq!(packet["coverage"].as_array().unwrap().len(), 64);
    assert!(!packet["primary"].as_array().unwrap().is_empty());
    assert_eq!(packet["episodes"].as_array().unwrap().len(), 1);
    assert!(
        packet["primary"].as_array().unwrap().len() + packet["episodes"].as_array().unwrap().len()
            <= context::MAX_ITEMS
    );
    assert!(packet["omitted"]["primary"].as_u64().unwrap() > 0);
    assert_eq!(packet["context_truncated"], true);
    assert_eq!(
        packet["episode_reference_reports"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for i in 0..64 {
        assert_eq!(reconstructed_reference_report(&packet, i), &report);
        assert_eq!(
            native_report(&packet, i, "episodic", "episodic_reports"),
            &context::coverage(&owner_context())
        );
        assert_eq!(packet["coverage"][i]["episodic"]["state"], "searched");
        assert_eq!(packet["coverage"][i]["state"], "live");
    }
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, tool)| tool == "recall_context")
            .count(),
        64
    );
    assert!(serde_json::to_vec(&packet).unwrap().len() <= MAX_CONTEXT_BYTES);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn report_interning_is_first_seen_lossless_and_distinguishes_different_actual_work() {
    let unsearched = reference_coverage(false);
    let searched = reference_coverage(true);
    let mut stopped = searched.clone();
    stopped["stop_reason"] = json!("read_error");
    stopped["further_tail_unknown"] = json!(true);
    let mut coverage = vec![
        json!({"source":"a","episode_reference_retrieval":searched}),
        json!({"source":"b","episode_reference_retrieval":unsearched}),
        json!({"source":"c","episode_reference_retrieval":stopped}),
        json!({"source":"d","episode_reference_retrieval":searched}),
    ];
    let reports = context::intern_reference_reports(&mut coverage);
    assert_eq!(reports, vec![searched, unsearched, stopped]);
    let packet = json!({"coverage":coverage,"episode_reference_reports":reports,
        "episode_reference_coverage_encoding":"native_report_index_v1"});
    for (index, expected) in [0, 1, 2, 0].into_iter().enumerate() {
        assert_eq!(
            packet["coverage"][index]["episode_reference_retrieval"]["report_index"],
            expected
        );
        assert_eq!(
            reconstructed_reference_report(&packet, index),
            &packet["episode_reference_reports"][expected]
        );
    }
}

#[test]
fn owner_admission_follows_requested_native_capacity_not_final_thirteen_card_packet() {
    let mut owner = owner_context();
    owner["primary"] = json!(
        (0..16)
            .map(|n| semantic(&format!("semantic-{n}"), n + 1))
            .collect::<Vec<_>>()
    );
    owner["episodes"] = json!(
        (0..16)
            .map(|n| episode(&format!("scene-{n}"), n + 1))
            .collect::<Vec<_>>()
    );
    assert!(serde_json::to_vec(&owner).unwrap().len() <= MAX_CONTEXT_BYTES);
    context::validate(&owner, 16).unwrap();
    assert!(context::validate(&owner, 15).is_err());
    let mut episode_overflow = owner.clone();
    episode_overflow["primary"] = json!([]);
    episode_overflow["episodes"]
        .as_array_mut()
        .unwrap()
        .push(episode("extra", 17));
    assert!(context::validate(&episode_overflow, 16).is_err());
}

#[test]
fn oversized_first_origin_union_is_omitted_whole_without_starving_later_small_scene() {
    let mut first = reference_episode("large-scene", "anchor-one");
    let origins = (0..150)
        .map(|n| {
            json!({"kind":"reference", "anchor":{
        "kind":"semantic","node_id":episode_id(&format!("anchor-{n}"))},
        "from":episode_id(&format!("anchor-{n}")),"to":first["edition_id"],
        "edge_kind":"Associative","body_anchor":{"start":0,"end":12}})
        })
        .collect::<Vec<_>>();
    // This models a same-account union across individually bounded owner views.
    let left = mneme_present::canonicalize_episode_origins(&json!(origins[..75])).unwrap();
    let right = mneme_present::canonicalize_episode_origins(&json!(origins[75..])).unwrap();
    first["origins"] = left;
    assert!(serde_json::to_vec(&first).unwrap().len() <= MAX_CONTEXT_BYTES);
    let mut duplicate = first.clone();
    duplicate["origins"] = right;
    assert!(serde_json::to_vec(&duplicate).unwrap().len() <= MAX_CONTEXT_BYTES);
    let small = reference_episode("small-scene", "cheap-anchor");
    let projected = |card: &Value| context::project(card, "p", "db", &json!({}), "episodic");
    let mut episodes = vec![projected(&first), projected(&duplicate), projected(&small)];
    assert!(context::deduplicate_episodes(&mut episodes).is_empty());
    assert_eq!(episodes.len(), 2);
    assert!(serde_json::to_vec(&episodes[0]).unwrap().len() > MAX_CONTEXT_BYTES);
    let small = episodes[1].clone();
    let mut packet = json!({"schema":"mneme.library.context.v5","primary":[],"episodes":episodes,
        "coverage":[],"episode_reference_reports":[],"episode_reference_coverage_encoding":"native_report_index_v1",
        "partial":false,"context_truncated":false});
    context::remove_unfit_cards(&mut packet, [0, 2]);
    context::trim_result_items(&mut packet);
    context::update_omissions(&mut packet, [0, 2]);
    assert_eq!(packet["episodes"], json!([small]));
    assert_eq!(packet["omitted"]["episodes"], 1);
    assert_eq!(packet["partial"], true);
    assert_eq!(packet["context_truncated"], true);
    assert!(serde_json::to_vec(&packet).unwrap().len() <= MAX_CONTEXT_BYTES);
}

#[tokio::test]
async fn actual_runtime_skips_merged_unfit_scene_before_single_candidate_pool_slot() {
    let (mut rt, fake, path) = fixture();
    rt.config.limits.total_candidates = 1;
    let mut cat: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let original = cat.entries[0].clone();
    cat.entries.clear();
    let report = json!({"state":"searched","anchors_total":2,"anchors_examined":2,
        "raw_edges_scanned":2,"raw_edge_limit":2,"endpoint_reads":1,"endpoint_read_limit":1,
        "indexed_seeks":4,"edge_point_reads":2,"body_anchor_point_reads":2,
        "missing":0,"non_episode":0,"cache_hits":0,"unread_anchors":0,"further_tail_unknown":false});
    // Each owner window is one small, valid card. Different routes expose
    // additional origins for the same account; only their library union grows.
    for i in 0..49 {
        let device = format!("owner-{i:02}");
        rt.config.owners.insert(
            device.clone(),
            Endpoint {
                url: device.clone(),
                ssh_mcp_port: 7337,
                token_env: None,
            },
        );
        let mut entry = original.clone();
        entry.project_id = format!("project-{i:02}");
        entry.owner_device_id = device.clone();
        cat.entries.push(entry);
        let mut source = owner_context();
        source["primary"] = json!([]);
        source["episode_reference_retrieval"] = report.clone();
        let mut card = if i == 48 {
            episode("later-cheap-scene", 1)
        } else {
            reference_episode("shared-large-scene", &format!("semantic-anchor-{i:02}"))
        };
        if i != 48 {
            let episode_anchor = episode_id(&format!("lexical-anchor-{i:02}"));
            let mut origins = card["origins"].as_array().unwrap().clone();
            origins.push(
                json!({"kind":"reference","anchor":{"kind":"episode","identity":{
                "episode_id":episode_anchor,"edition_id":episode_anchor,"revision":0}},
                "from":episode_anchor,"to":card["edition_id"],"edge_kind":"Associative",
                "body_anchor":{"start":1,"end":13}}),
            );
            card["origins"] = mneme_present::canonicalize_episode_origins(&json!(origins)).unwrap();
            card["occurrence_contexts"] = maximum_occurrence_contexts();
        }
        source["episodes"] = json!([card]);
        context::validate(&source, 1).unwrap();
        fake.contexts.lock().unwrap().insert(device, source);
    }
    std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
    let result = rt.recall_context(query()).await.unwrap();
    assert_eq!(result["coverage"].as_array().unwrap().len(), 49);
    assert_eq!(result["episodes"].as_array().unwrap().len(), 1);
    assert_eq!(
        result["episodes"][0]["edition_id"],
        episode_id("later-cheap-scene")
    );
    assert_eq!(
        result["episodes"][0]["origins"],
        json!([{"kind":"lexical"}])
    );
    assert_eq!(result["omitted"]["episodes"], 1);
    assert_eq!(result["context_truncated"], true);
    assert_eq!(result["partial"], true);
    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_CONTEXT_BYTES);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn compatible_repacked_summary_prefixes_merge_without_replacing_the_first_view() {
    let mut first = context::project(
        &reference_episode("same-account", "anchor-one"),
        "first-project",
        "db",
        &json!({"generation":"first"}),
        "episodic",
    );
    first["summary"] = json!("episode");
    first["summary_complete"] = json!(false);
    let later = context::project(
        &reference_episode("same-account", "anchor-two"),
        "later-project",
        "db",
        &json!({"generation":"later"}),
        "episodic",
    );
    let mut cards = vec![first.clone(), later];
    assert!(context::deduplicate_episodes(&mut cards).is_empty());
    assert_eq!(cards.len(), 1);
    for field in [
        "summary",
        "summary_complete",
        "summary_source_bytes",
        "source",
        "current_edition_id",
    ] {
        assert_eq!(cards[0][field], first[field], "first {field} replaced");
    }
    assert_eq!(cards[0]["origins"].as_array().unwrap().len(), 2);
    let mut incompatible = cards[0].clone();
    incompatible["project_id"] = json!("conflicting-project");
    incompatible["summary"] = json!("different");
    cards.push(incompatible);
    assert_eq!(
        context::deduplicate_episodes(&mut cards),
        vec![("conflicting-project".into(), "db".into())]
    );
    assert_eq!(cards.len(), 1);
    let mut different_source_length = cards[0].clone();
    different_source_length["project_id"] = json!("different-length");
    different_source_length["summary_source_bytes"] = json!(13);
    cards.push(different_source_length);
    assert_eq!(
        context::deduplicate_episodes(&mut cards),
        vec![("different-length".into(), "db".into())]
    );
}

fn touchstone_facet(db: &str) -> Value {
    json!({"schema":"mneme.touchstone-view.v1","subject":"the scene worth keeping",
        "coverage":"summary_only","references":[{"db_id":db,"id":episode_id("held-edition"),
        "snapshot_sha256":"a".repeat(64),"summary":{"text":"a frozen scene","complete":true,"source_bytes":14},
        "resolution":"changed_snapshot"}],"references_omitted":2,"origins":[{"kind":"direct"}]})
}

fn touchstone_owner_context(db: &str) -> Value {
    let mut owner = owner_context();
    owner["primary"][0]["id"] = json!(episode_id("touchstone-owner"));
    owner["primary"][0]["touchstone"] = touchstone_facet(db);
    owner["touchstone_retrieval"]["searched"] = json!(true);
    owner["touchstone_retrieval"]["read_limit"] = json!(4);
    owner["touchstone_retrieval"]["record_reads"] = json!(1);
    owner["touchstone_retrieval"]["target_reads"] = json!(1);
    owner["touchstone_retrieval"]["anchors_total"] = json!(2);
    owner["touchstone_retrieval"]["anchors_examined"] = json!(2);
    owner
}

fn scoped_touchstone_fixture() -> (LibraryRuntime, Arc<tests::Fake>, PathBuf, String) {
    let (rt, fake, path) = fixture();
    let db = episode_id("local-logical-db");
    let mut cat: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    cat.entries[0].db_id = db.clone();
    std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
    for route in ["owner", "replica"] {
        fake.database_ids
            .lock()
            .unwrap()
            .insert(route.into(), db.clone());
        fake.contexts
            .lock()
            .unwrap()
            .insert(route.into(), touchstone_owner_context(&db));
    }
    (rt, fake, path, db)
}

fn native_report<'a>(packet: &'a Value, source: usize, field: &str, reports: &str) -> &'a Value {
    let index = packet["coverage"][source][field]["report_index"]
        .as_u64()
        .unwrap() as usize;
    &packet[reports][index]
}

#[test]
fn touchstone_native_admission_is_strict_and_pins_all_references_to_one_database() {
    let db = episode_id("local-logical-db");
    let source = touchstone_owner_context(&db);
    context::validate_source(&source, 64, &db).unwrap();
    assert!(context::validate_source(&source, 64, &episode_id("foreign-db")).is_err());
    for version in ["mneme.context.v6", "mneme.context.v8"] {
        let mut bad = source.clone();
        bad["schema"] = json!(version);
        assert!(context::validate(&bad, 64).is_err(), "accepted {version}");
    }
    for facet in [
        json!(null),
        json!({}),
        {
            let mut facet = touchstone_facet(&db);
            facet["references"][0]["body"] = json!("not a summary");
            facet
        },
        {
            let mut facet = touchstone_facet(&db);
            facet["references"][0]["resolution"] = json!("matches_summary");
            facet
        },
        {
            let mut facet = touchstone_facet(&db);
            facet["snapshots"] = json!([]);
            facet
        },
    ] {
        let mut bad = source.clone();
        bad["primary"][0]["touchstone"] = facet.clone();
        assert!(context::validate(&bad, 64).is_err(), "accepted {facet}");
    }
    let mut wrong_kind = source.clone();
    wrong_kind["episodes"][0]["touchstone"] = touchstone_facet(&db);
    assert!(context::validate(&wrong_kind, 64).is_err());
    for case in 0..6 {
        let mut bad = source.clone();
        match case {
            0 => {
                bad.as_object_mut().unwrap().remove("touchstone_retrieval");
            }
            1 => bad["touchstone_retrieval"]["unexpected"] = json!(true),
            2 => bad["touchstone_retrieval"]["target_reads"] = json!(5),
            3 => bad["touchstone_retrieval"]["anchors_examined"] = json!(3),
            4 => bad["touchstone_retrieval"] = touchstone_coverage(),
            _ => bad["omitted"]["primary"]["body"] = json!("unexpected"),
        }
        assert!(context::validate(&bad, 64).is_err(), "accepted case {case}");
    }
}

#[tokio::test]
async fn touchstone_projection_keeps_exact_facet_source_and_native_coverage_without_receipt() {
    let (rt, fake, path, db) = scoped_touchstone_fixture();
    let mut source = touchstone_owner_context(&db);
    source["touchstone_retrieval"]["stop_reason"] = json!("budget");
    source["touchstone_retrieval"]["further_tail_unknown"] = json!(true);
    source["omitted"]["primary"]["bounded_window_budget"] = json!(3);
    source["omitted"]["primary"]["further_tail_unknown"] = json!(true);
    fake.contexts
        .lock()
        .unwrap()
        .insert("owner".into(), source.clone());
    let result = rt.recall_context(query()).await.unwrap();
    assert_eq!(result["schema"], "mneme.library.context.v5");
    assert_eq!(
        result["primary"][0]["touchstone"],
        source["primary"][0]["touchstone"]
    );
    assert_eq!(result["primary"][0]["db_id"], db);
    assert_eq!(result["primary"][0]["project_id"], "p");
    assert_eq!(
        result["primary"][0]["source"],
        result["episodes"][0]["source"]
    );
    assert_eq!(result["primary"][0]["source"]["kind"], "live");
    assert_eq!(
        native_report(&result, 0, "touchstone_retrieval", "touchstone_reports"),
        &source["touchstone_retrieval"]
    );
    assert_eq!(
        native_report(&result, 0, "native_omitted", "native_omission_reports"),
        &source["omitted"]
    );
    assert_eq!(result["partial"], true);
    assert!(result["receipt"].is_null());
    assert!(result["primary"][0].get("body").is_none());
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, tool)| tool.as_str())
            .collect::<Vec<_>>(),
        ["databases", "recall_context"]
    );
    let args = &fake.requests.lock().unwrap()[1].1;
    assert_eq!(args["db"], "main");
    assert!(args.get("touchstone").is_none());
    assert!(args.get("project_ids").is_none());
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn touchstone_scope_refusal_never_falls_back_and_snapshot_keeps_same_local_scope() {
    let (rt, fake, path, db) = scoped_touchstone_fixture();
    let mut source = touchstone_owner_context(&db);
    source["primary"][0]["touchstone"]["references"][0]["db_id"] = json!(episode_id("foreign-db"));
    fake.contexts.lock().unwrap().insert("owner".into(), source);
    let result = rt.recall_context(query()).await.unwrap();
    assert_eq!(result["coverage"][0]["state"], "refused");
    assert!(result["primary"].as_array().unwrap().is_empty());
    assert!(
        !fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(route, _)| route == "replica")
    );
    fake.errors.lock().unwrap().insert(
        "owner".into(),
        SourceFailure::Availability("offline".into()),
    );
    let result = rt.recall_context(query()).await.unwrap();
    assert_eq!(result["primary"][0]["touchstone"], touchstone_facet(&db));
    assert_eq!(result["primary"][0]["source"]["kind"], "snapshot");
    assert_eq!(result["primary"][0]["source"]["generation"], "g1");
    assert_eq!(result["primary"][0]["db_id"], db);
    assert_eq!(
        result["primary"][0]["source"],
        result["episodes"][0]["source"]
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn library_get_passes_full_native_touchstone_metadata_through_unchanged() {
    let (rt, fake, path, db) = scoped_touchstone_fixture();
    let native = json!({"id":episode_id("touchstone-owner"),"summary":"owner summary","body":"owner body",
        "touchstone":{"schema":"native-test-record","subject":"subject","snapshots":[
            {"db_id":db,"id":episode_id("held-edition"),"summary":"full historical summary",
                "provenance":{"source":"original"},"created_at":42,"memory_kind":"semantic"}]}});
    fake.get_values
        .lock()
        .unwrap()
        .insert("owner".into(), native.clone());
    let result = rt
        .get(LibraryGet {
            project_id: "p".into(),
            id: episode_id("touchstone-owner"),
            source_device_id: "owner".into(),
            generation: None,
        })
        .await
        .unwrap();
    assert_eq!(result["node"], native);
    assert_eq!(result["db_id"], db);
    assert_eq!(
        fake.requests.lock().unwrap().last().unwrap().1["db"],
        "main"
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn unfit_touchstone_card_is_omitted_whole_before_it_can_starve_later_small_card() {
    let db = episode_id("local-logical-db");
    let owner = touchstone_owner_context(&db);
    let mut huge = context::project(&owner["primary"][0], "p", &db, &json!({}), "primary");
    // A valid standalone native facet can still fail once complete library
    // source metadata is present. Never trim just its caveats or references.
    huge["touchstone"]["references"][0]["summary"] =
        json!({"text":"x".repeat(29_000),"complete":true,"source_bytes":29_000});
    mneme_present::validate_touchstone_card_metadata(&huge).unwrap();
    let small = context::project(
        &semantic("small-useful-card", 2),
        "p",
        &db,
        &json!({}),
        "primary",
    );
    let mut result = json!({"primary":[huge,small.clone()],"episodes":[],"partial":false,"context_truncated":false,
        "coverage":[{"source_metadata":"x".repeat(5000)}]});
    context::remove_unfit_cards(&mut result, [2, 0]);
    context::trim_result_pool(&mut result, 1);
    context::update_omissions(&mut result, [2, 0]);
    assert_eq!(result["primary"], json!([small]));
    assert_eq!(result["omitted"]["primary"], 1);
    assert_eq!(result["context_truncated"], true);
    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_CONTEXT_BYTES);
}

#[tokio::test]
async fn sixty_four_touchstone_owner_reports_keep_native_work_and_omissions_losslessly() {
    let (rt, fake, path, db) = scoped_touchstone_fixture();
    let mut source = touchstone_owner_context(&db);
    source["omitted"]["primary"]["further_tail_unknown"] = json!(true);
    fake.contexts
        .lock()
        .unwrap()
        .insert("owner".into(), source.clone());
    let mut cat: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for n in 1..64 {
        let mut entry = cat.entries[0].clone();
        entry.project_id = format!("p{n}");
        cat.entries.push(entry);
    }
    std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
    let result = rt.recall_context(query()).await.unwrap();
    assert_eq!(result["coverage"].as_array().unwrap().len(), 64);
    assert!(!result["primary"].as_array().unwrap().is_empty());
    assert_eq!(result["touchstone_reports"].as_array().unwrap().len(), 1);
    assert_eq!(result["episodic_reports"].as_array().unwrap().len(), 1);
    assert_eq!(
        result["native_omission_reports"].as_array().unwrap().len(),
        1
    );
    for source_index in 0..64 {
        assert_eq!(
            native_report(&result, source_index, "episodic", "episodic_reports"),
            &context::coverage(&source)
        );
        assert_eq!(
            result["coverage"][source_index]["episodic"]["state"],
            "searched"
        );
        assert_eq!(
            native_report(
                &result,
                source_index,
                "touchstone_retrieval",
                "touchstone_reports"
            ),
            &source["touchstone_retrieval"]
        );
        assert_eq!(
            native_report(
                &result,
                source_index,
                "native_omitted",
                "native_omission_reports"
            ),
            &source["omitted"]
        );
    }
    assert!(result["receipt"].is_null());
    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_CONTEXT_BYTES);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn selected_project_scope_does_not_follow_even_enrolled_foreign_touchstone_reference() {
    let (mut rt, fake, path, db) = scoped_touchstone_fixture();
    let foreign_db = episode_id("foreign-db");
    let mut cat: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut foreign = cat.entries[0].clone();
    foreign.project_id = "other-project".into();
    foreign.db_id = foreign_db.clone();
    foreign.owner_device_id = "foreign-owner".into();
    foreign.replicas.clear();
    cat.entries.push(foreign);
    std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
    rt.config.owners.insert(
        "foreign-owner".into(),
        Endpoint {
            url: "foreign-owner".into(),
            ssh_mcp_port: 7337,
            token_env: None,
        },
    );
    fake.database_ids
        .lock()
        .unwrap()
        .insert("foreign-owner".into(), foreign_db.clone());
    fake.contexts.lock().unwrap().insert(
        "foreign-owner".into(),
        touchstone_owner_context(&foreign_db),
    );
    let mut source = touchstone_owner_context(&db);
    source["primary"][0]["touchstone"]["references"][0]["db_id"] = json!(foreign_db);
    fake.contexts.lock().unwrap().insert("owner".into(), source);
    let mut selected = query();
    selected.project_ids = vec!["p".into()];
    let result = rt.recall_context(selected).await.unwrap();
    assert_eq!(result["coverage"].as_array().unwrap().len(), 1);
    assert_eq!(result["coverage"][0]["state"], "refused");
    assert!(result["primary"].as_array().unwrap().is_empty());
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(route, tool)| (route.as_str(), tool.as_str()))
            .collect::<Vec<_>>(),
        [("owner", "databases"), ("owner", "recall_context")]
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn episodic_coverage_factoring_is_lossless_for_cues_omissions_and_unavailable_reason() {
    let searched = context::coverage(&owner_context());
    let mut unknown_tail = searched.clone();
    unknown_tail["cue_truncated"] = json!(true);
    unknown_tail["omitted"]["bounded_window_budget"] = json!(7);
    unknown_tail["omitted"]["further_tail_unknown"] = json!(true);
    let mut unavailable = searched.clone();
    unavailable["state"] = json!("unavailable");
    unavailable["unavailable_reason"] = json!("store_not_upgraded");
    let mut coverage = vec![
        json!({"project_id":"p1","state":"live","episodic":searched}),
        json!({"project_id":"p2","state":"snapshot","episodic":unknown_tail}),
        json!({"project_id":"p3","state":"live","episodic":unavailable}),
        json!({"project_id":"p4","state":"live","episodic":searched}),
        json!({"project_id":"refused-owner","state":"refused"}),
    ];
    let pinned_sources = coverage.clone();
    let reports = context::intern_episodic_reports(&mut coverage);
    assert_eq!(reports, vec![searched, unknown_tail, unavailable]);
    let packet = json!({"coverage":coverage,"episodic_reports":reports,
        "episodic_coverage_encoding":"native_report_index_v1"});
    for index in 0..4 {
        assert_eq!(
            native_report(&packet, index, "episodic", "episodic_reports"),
            &pinned_sources[index]["episodic"]
        );
        assert_eq!(
            packet["coverage"][index]["episodic"]["state"],
            pinned_sources[index]["episodic"]["state"]
        );
        assert_eq!(
            packet["coverage"][index]["project_id"],
            pinned_sources[index]["project_id"]
        );
        assert_eq!(
            packet["coverage"][index]["state"],
            pinned_sources[index]["state"]
        );
    }
    assert_eq!(packet["coverage"][4], pinned_sources[4]);
}
