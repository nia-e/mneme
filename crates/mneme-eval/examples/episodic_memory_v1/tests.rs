use super::*;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);
struct SessionDir(PathBuf);
impl SessionDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mneme-episodic-v1-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::SeqCst)
        ));
        assert!(
            !path.exists(),
            "refuse to reuse a preexisting test directory"
        );
        Self(path)
    }
}
impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn scripted_rehearsal_is_repeatable_and_does_not_claim_fresh_actors() {
    let a = rehearsal::run().await.unwrap();
    let b = rehearsal::run().await.unwrap();
    assert_eq!(
        a, b,
        "all semantic artifact content should repeat, not just a digest field"
    );
    assert_eq!(a["all_passed"], true);
    assert_eq!(a["actor_kind"], "scripted_rehearsal");
    assert_eq!(a["claims"]["fresh_actor_evidence"], false);
    assert_eq!(a["checks"].as_array().unwrap().len(), 9);
}

#[test]
fn actor_packet_excludes_gold_and_corpus_content() {
    for case in [
        "recent_activity",
        "changed_recommendation",
        "similar_situation",
    ] {
        let dir = SessionDir::new();
        let packet = actor::initialize(&dir.0, case).unwrap();
        let text = packet.to_string();
        for private in [
            "PRIVATE_ASSESSMENT",
            "e01",
            "e05-r1",
            "image decoder",
            "tile manifest",
            "paper kite",
            "required_distinctions",
            "supporting_aliases",
        ] {
            assert!(!text.contains(private), "{private} leaked to fresh actor");
        }
        assert_eq!(packet["budget"]["memory_calls"], 12);
        assert!(
            actor::initialize(&dir.0, case).is_err(),
            "reinitialization must not reset counters"
        );
    }
}

#[tokio::test]
async fn actor_errors_are_charged_and_calls_persist_between_requests() {
    let dir = SessionDir::new();
    actor::initialize(&dir.0, "recent_activity").unwrap();
    for call in 1..=12 {
        let result = actor::request(&dir.0, json!({"request":{"action":"append"}}))
            .await
            .unwrap();
        assert_eq!(result["ok"], false);
        let artifact = actor::artifact(&dir.0).unwrap();
        assert_eq!(artifact["state"]["calls"], call);
    }
    assert!(
        actor::request(&dir.0, json!({"request":{"action":"list"}}))
            .await
            .is_err()
    );
    let artifact = actor::artifact(&dir.0).unwrap();
    assert_eq!(artifact["state"]["calls"], 12);
    assert_eq!(artifact["state"]["exhausted"], true);
    let trace = artifact["state"]["trace"].as_array().unwrap();
    let charged: usize = trace
        .iter()
        .map(|call| serde_json::to_vec(&call["response"]).unwrap().len())
        .sum();
    assert_eq!(artifact["state"]["response_bytes"], charged);
    assert!(charged <= 8192);
    assert_eq!(artifact["actor_kind"], "request_driven_unclassified");
}

#[tokio::test]
async fn actor_shared_reads_are_bounded_and_oversize_is_not_partial_json() {
    let dir = SessionDir::new();
    actor::initialize(&dir.0, "changed_recommendation").unwrap();
    let small = actor::request(
        &dir.0,
        json!({"request":{"action":"list","limit":4},"max_response_bytes":256}),
    )
    .await
    .unwrap();
    assert_eq!(small["ok"], false);
    assert!(small["error"].as_str().unwrap().contains("reserved bytes"));
    let listing = actor::request(&dir.0, json!({"request":{"action":"list","limit":1}}))
        .await
        .unwrap();
    assert_eq!(listing["ok"], true);
    let id = listing["result"]["items"][0]["episode_id"].clone();
    let detail = actor::request(
        &dir.0,
        json!({"request":{"action":"get","episode_id":id,"body":true,"max_bytes":64}}),
    )
    .await
    .unwrap();
    assert_eq!(detail["ok"], true);
    assert!(detail["result"]["body"].as_str().unwrap().len() <= 64);
    assert_eq!(detail["result"]["body_range"]["has_more"], true);
    let artifact = actor::artifact(&dir.0).unwrap();
    assert_eq!(artifact["state"]["calls"], 3);
    for call in artifact["state"]["trace"].as_array().unwrap() {
        assert!(
            call["response_bytes"].as_u64().unwrap() <= call["reserved_bytes"].as_u64().unwrap()
        );
    }
}

#[tokio::test]
async fn actor_byte_budget_survives_repeated_reads_and_can_finish_after_exhaustion() {
    let dir = SessionDir::new();
    actor::initialize(&dir.0, "similar_situation").unwrap();
    for _ in 0..16 {
        if actor::request(
            &dir.0,
            json!({"request":{"action":"list","limit":4},"max_response_bytes":4096}),
        )
        .await
        .is_err()
        {
            break;
        }
    }
    let artifact = actor::artifact(&dir.0).unwrap();
    assert!(artifact["state"]["calls"].as_u64().unwrap() <= 12);
    assert!(artifact["state"]["response_bytes"].as_u64().unwrap() <= 8192);
    assert!(actor::finish(&dir.0,json!({"answer":"Evidence was incomplete within the read envelope.","episode_ids":[],"lesson_ids":[],"uncertainty":"I cannot diagnose the new dashboard."})).is_ok());
    assert!(
        actor::request(&dir.0, json!({"request":{"action":"list"}}))
            .await
            .is_err()
    );
}

#[test]
fn actor_final_words_are_enforced_without_fabricating_a_grade() {
    let dir = SessionDir::new();
    actor::initialize(&dir.0, "recent_activity").unwrap();
    assert!(
        actor::finish(
            &dir.0,
            json!({"answer":"word ".repeat(251),"episode_ids":[],"lesson_ids":[]})
        )
        .is_err()
    );
    let result = actor::finish(&dir.0,json!({"answer":"I have not read enough to give an account yet.","episode_ids":[],"lesson_ids":[]})).unwrap();
    assert_eq!(result["finished"], true);
    assert!(result.get("score").is_none());
    assert!(
        actor::finish(
            &dir.0,
            json!({"answer":"Again.","episode_ids":[],"lesson_ids":[]})
        )
        .is_err()
    );
}

#[tokio::test]
async fn actor_lock_and_fixture_pin_do_not_reset_the_envelope() {
    let dir = SessionDir::new();
    actor::initialize(&dir.0, "recent_activity").unwrap();
    std::fs::write(dir.0.join("request.lock"), b"busy").unwrap();
    assert!(
        actor::request(&dir.0, json!({"request":{"action":"list"}}))
            .await
            .is_err()
    );
    std::fs::remove_file(dir.0.join("request.lock")).unwrap();
    let state_path = dir.0.join("session.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    state["fixture_sha256"] = json!("not-the-fixture");
    std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    assert!(
        actor::request(&dir.0, json!({"request":{"action":"list"}}))
            .await
            .is_err()
    );
}
