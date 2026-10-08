use super::*;
use session::Session;

#[tokio::test]
async fn packets_exclude_future_phases_and_private_assessment() {
    for episode in ["diagnostics", "async"] {
        let mut s = Session::new(episode, "notes_search", false, "test_actor").unwrap();
        let packet = s.request(json!({"op":"packet"})).await.to_string();
        for forbidden in [
            "PRIVATE_ASSESSMENT_ONLY",
            "legacy_archive",
            "new_search",
            "inspect_existing",
            "native_async",
            "archive handler",
            "archive component",
        ] {
            assert!(
                !packet.contains(forbidden),
                "{forbidden} leaked in {packet}"
            );
        }
        let result = s
            .request(json!({"op":"task","action":"inspect","component":"legacy_archive"}))
            .await;
        assert_eq!(result["ok"], false);
        let result = s.request(json!({"op":"search","query":"correction"})).await;
        assert_eq!(result["result"]["hits"].as_array().unwrap().len(), 0);
    }
}
#[test]
fn diagnostics_grade_effects_and_require_observation() {
    let f = fixture("diagnostics").unwrap();
    let mut e = environment::Environment::new("diagnostics", f.phases[1].clone());
    e.act(&json!({"action":"report","component":"archive","value":"missing"}))
        .unwrap();
    assert_eq!(e.grade(&assessment())["passed"], false, "guess cannot pass");
    e.act(&json!({"action":"inspect","component":"archive"}))
        .unwrap();
    assert_eq!(e.grade(&assessment())["passed"], true);
    e.act(&json!({"action":"open","component":"archive"}))
        .unwrap();
    e.act(&json!({"action":"report","component":"archive","value":"usable"}))
        .unwrap();
    let result = e.grade(&assessment());
    assert_eq!(result["passed"], false);
    assert_eq!(result["components"][0]["repeated_mistake"], true);
}
#[test]
fn shared_loop_blocking_and_unnecessary_worker_have_consequences() {
    let f = fixture("async").unwrap();
    let mut e = environment::Environment::new("async", f.phases[3].clone());
    e.act(&json!({"action":"begin","component":"legacy_archive","mode":"direct"}))
        .unwrap();
    e.act(&json!({"action":"begin","component":"new_search","mode":"direct"}))
        .unwrap();
    assert_eq!(
        e.act(&json!({"action":"tick","component":"new_search"}))
            .unwrap()["heartbeat"],
        "blocked"
    );
    let mut e = environment::Environment::new("async", f.phases[2].clone());
    e.act(&json!({"action":"begin","component":"search","mode":"worker"}))
        .unwrap();
    e.act(&json!({"action":"tick","component":"search"}))
        .unwrap();
    e.act(&json!({"action":"complete","component":"search"}))
        .unwrap();
    e.act(&json!({"action":"report","component":"search","value":"progressed"}))
        .unwrap();
    assert_eq!(e.grade(&assessment())["passed"], false);
    assert_eq!(
        e.grade(&assessment())["components"][0]["obsolete_workaround_proxy"],
        true
    );
}
#[tokio::test]
async fn correction_keeps_the_old_node_readable_and_candidate_in_all_conditions() {
    for condition in ["notes_search", "hybrid_graph_off", "hybrid_authored_graph"] {
        let mut s = Session::new("diagnostics", condition, false, "test_actor").unwrap();
        for _ in 0..3 {
            s.request(json!({"op":"finish"})).await;
            s.request(json!({"control":"advance"})).await;
        }
        let old = s.memory.aliases["lesson"];
        let node = s.memory.engine.get_node(old).await.unwrap().unwrap();
        assert!(matches!(
            node.status(),
            mneme_core::NodeStatus::Candidate { .. }
        ));
        assert_eq!(node.confidence(), 0.25);
        let read = s.request(json!({"op":"read","id":old})).await;
        assert!(read["result"]["body"].as_str().unwrap().contains("created"));
        let artifact = s.artifact();
        let common_ops: Vec<&Value> = artifact["authorship_operations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["kind"] == "common_frozen_inputs")
            .flat_map(|entry| entry["operations"].as_array().unwrap())
            .collect();
        assert_eq!(
            common_ops.len(),
            4,
            "three ingests and a separate supersede call"
        );
        assert!(common_ops.iter().all(|op| op["response"]["ok"] == true));
    }
}
#[tokio::test]
async fn controlled_authorship_rejects_edits_independent_boundary_accepts_them() {
    let note = json!({"op":"note","summary":"Actor chosen note","body":"Actor chosen body"});
    let mut controlled = Session::new("async", "notes_search", false, "test_actor").unwrap();
    assert_eq!(controlled.request(note.clone()).await["ok"], false);
    let mut independent = Session::new("async", "notes_search", true, "test_actor").unwrap();
    let result = independent.request(note).await;
    assert_eq!(result["ok"], true);
    let read = independent
        .request(json!({"op":"read","id":result["result"]["id"]}))
        .await;
    assert_eq!(read["result"]["body"], "Actor chosen body");
}
#[tokio::test]
async fn budgets_charge_packet_errors_and_rejections_and_allow_incomplete_transition() {
    let mut s = Session::new("diagnostics", "notes_search", false, "test_actor").unwrap();
    let request = json!({"op":"packet"});
    let response = s.request(request.clone()).await;
    assert_eq!(s.calls, 1);
    assert_eq!(s.request_bytes, request.to_string().len());
    assert_eq!(s.response_bytes, response.to_string().len());
    for _ in 1..session::MAX_CALLS {
        s.request(json!({"op":"invalid"})).await;
    }
    let rejection = s.request(json!({"op":"finish"})).await;
    assert_eq!(rejection["budget_rejected"], true);
    let artifact = s.artifact();
    assert_eq!(artifact["phases"][0]["budget"]["attempted_calls"], 33);
    assert_eq!(artifact["phases"][0]["budget"]["rejected_calls"], 1);
    assert_eq!(artifact["phases"][0]["phase_passed"], false);
    let advanced = s.request(json!({"control":"advance","force":true})).await;
    assert_eq!(advanced["ok"], true);
    assert_eq!(s.calls, 0);
}
#[tokio::test]
async fn rehearsal_is_repeatable_with_frozen_common_inputs_and_no_required_winner() {
    let a = session::rehearse().await.unwrap();
    let b = session::rehearse().await.unwrap();
    assert_eq!(a, b);
    assert_eq!(a["episodes"].as_array().unwrap().len(), 6);
    assert_eq!(
        a["semantic_digest"], "1dc8f4b1346e0079ce5445fccbe7a3878f6300dae9f03828a71042f3c73fd5f0",
        "v1 full artifact changed"
    );
    for group in a["episodes"].as_array().unwrap().chunks(3) {
        assert!(
            group
                .iter()
                .all(|e| e["common_authorship_hash"] == group[0]["common_authorship_hash"])
        );
        for episode in group {
            assert_eq!(episode["actor_kind"], "scripted_rehearsal");
            assert!(
                episode["phases"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|p| p["phase_passed"] == true),
                "{}",
                episode
            );
            assert_eq!(episode["claims"]["graph_must_win"], false);
        }
    }
}

#[tokio::test]
async fn investigation_current_sources_and_disposable_probes_are_equal_across_arms() {
    for episode in ["diagnostics", "async"] {
        let mut control =
            Session::new_investigation(episode, "empty_memory", "test_actor").unwrap();
        let mut notes = Session::new_investigation(episode, "notes_search", "test_actor").unwrap();
        for phase in ["learn", "transfer", "revise", "return"] {
            let a = control.request(json!({"op":"packet"})).await;
            let b = notes.request(json!({"op":"packet"})).await;
            for key in [
                "components",
                "handoff",
                "task_operations",
                "discovery_operations",
                "budget",
            ] {
                assert_eq!(
                    a["result"][key], b["result"][key],
                    "unequal {key} in {episode}/{phase}"
                );
            }
            let packet = a.to_string();
            for forbidden in [
                "PRIVATE_ASSESSMENT_ONLY",
                "open_or_create",
                "inspect_existing",
                "native_async",
            ] {
                assert!(
                    !packet.contains(forbidden),
                    "answer-bearing {forbidden} leaked in packet"
                );
            }
            if phase == "learn" || phase == "transfer" {
                assert!(!packet.contains("2.0.0"));
                assert!(!packet.contains("new_search"));
            }
            let request = json!({"op":"source_search","query":""});
            let sources = control.request(request.clone()).await;
            assert_eq!(sources, notes.request(request).await);
            let hits = sources["result"]["hits"].as_array().unwrap();
            assert_eq!(hits.len(), if phase == "return" { 4 } else { 2 });
            for source in hits {
                let request = json!({"op":"source_read","path":source["path"]});
                let response = control.request(request.clone()).await;
                assert_eq!(response, notes.request(request).await);
                if source["kind"] == "contract" {
                    let body: Value =
                        serde_json::from_str(response["result"]["body"].as_str().unwrap()).unwrap();
                    assert_eq!(body["version"], source["version"]);
                    let modern = body["version"] == "2.0.0";
                    if episode == "diagnostics" {
                        assert_eq!(
                            body["absent_storage"],
                            if modern {
                                "return missing without creating storage"
                            } else {
                                "create and return usable"
                            }
                        );
                    } else {
                        assert_eq!(
                            body["pending_direct_call"],
                            if modern {
                                "yields so shared-loop heartbeat can progress"
                            } else {
                                "blocks shared-loop heartbeat until completion"
                            }
                        );
                    }
                }
            }
            if phase != "return" {
                let unavailable = if phase == "revise" { "1.4.0" } else { "2.0.0" };
                let request = json!({"op":"source_read","path":format!("{episode}/{unavailable}/dependency-contract.json")});
                let response = control.request(request.clone()).await;
                assert_eq!(response["ok"], false);
                assert_eq!(response, notes.request(request).await);
                let request = json!({"op":"source_search","query":unavailable});
                let response = control.request(request.clone()).await;
                assert_eq!(response["result"]["hits"], json!([]));
                assert_eq!(response, notes.request(request).await);
            }
            let before = json!(control.environment.states);
            for component in a["result"]["components"].as_array().unwrap() {
                let request = if episode == "diagnostics" {
                    json!({"op":"probe","component":component["name"],"initial_present":false})
                } else {
                    json!({"op":"probe","component":component["name"],"mode":"direct"})
                };
                let response = control.request(request.clone()).await;
                assert_eq!(response, notes.request(request).await);
                let modern = component["version"] == "2.0.0";
                if episode == "diagnostics" {
                    assert_eq!(response["result"]["observations"][1]["created"], !modern);
                } else {
                    assert_eq!(
                        response["result"]["observations"][1]["heartbeat"],
                        if modern { "progressed" } else { "blocked" }
                    );
                }
            }
            assert_eq!(
                json!(control.environment.states),
                before,
                "probes changed actual task state"
            );
            assert_eq!(
                control.artifact()["phases"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()["phase_passed"],
                false,
                "discovery alone cannot satisfy task"
            );
            for session in [&mut control, &mut notes] {
                session.request(json!({"op":"finish"})).await;
                if phase != "return" {
                    assert_eq!(
                        session.request(json!({"control":"advance"})).await["ok"],
                        true
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn investigation_natural_authorship_is_charged_and_frozen_with_corrections() {
    let mut s = Session::new_investigation("diagnostics", "notes_search", "test_actor").unwrap();
    let note = json!({"op":"note","summary":"Scoped observed lesson","body":"Release 1.4.0 opener created storage in my observation."});
    let response = s.request(note.clone()).await;
    let old = response["result"]["id"].clone();
    assert_eq!(response["ok"], true);
    let learn = s.artifact();
    assert_eq!(
        learn["phases"][0]["operation_counts"]["authorship"]["attempted"],
        1
    );
    assert_eq!(
        learn["phases"][0]["budget"]["request_bytes"],
        note.to_string().len()
    );
    assert_eq!(
        learn["phases"][0]["budget"]["response_bytes"],
        response.to_string().len()
    );
    assert_ne!(
        learn["phases"][0]["authored_history_at_start"],
        learn["phases"][0]["authored_history_at_end"]
    );
    s.request(json!({"op":"finish"})).await;
    s.request(json!({"control":"advance"})).await;
    let transfer = s.artifact();
    assert_eq!(
        transfer["phases"][1]["authored_history_at_start"],
        learn["phases"][0]["authored_history_at_end"]
    );
    assert_eq!(
        s.request(note.clone()).await["ok"],
        false,
        "transfer must not rewrite history"
    );
    assert_eq!(
        s.artifact()["authored_history_hash"],
        learn["authored_history_hash"]
    );
    s.request(json!({"op":"finish"})).await;
    s.request(json!({"control":"advance"})).await;
    let successor=s.request(json!({"op":"note","summary":"Scoped correction","body":"Release 2.0.0 changed the absent-storage contract; the 1.4.0 caution still applies to 1.4.0."})).await;
    assert_eq!(successor["ok"], true);
    let valid = json!({"op":"supersede","winner":successor["result"]["id"],"loser":old});
    assert_eq!(
        s.request(json!({"op":"supersede","winner":"not-an-id","loser":old}))
            .await["ok"],
        false
    );
    assert_eq!(s.request(valid).await["ok"], true);
    let revised = s.artifact();
    let counts = &revised["phases"][2]["operation_counts"]["authorship"];
    assert_eq!(counts["attempted"], 3);
    assert_eq!(counts["failed_admitted"], 1);
    assert_eq!(
        revised["authorship_operations"].as_array().unwrap().len(),
        3,
        "only successful note/note/supersede form authored history; error retained in phase log"
    );
    let old_read = s.request(json!({"op":"read","id":old})).await;
    assert_eq!(old_read["result"]["confidence"], 0.25);
    assert!(
        old_read["result"]["status"].get("Candidate").is_some(),
        "current-branch supersession semantics must be explicit: {old_read}"
    );
    s.request(json!({"op":"finish"})).await;
    s.request(json!({"control":"advance"})).await;
    let returned = s.artifact();
    assert_eq!(
        returned["phases"][3]["authored_history_at_start"],
        returned["phases"][2]["authored_history_at_end"]
    );
    assert_eq!(
        s.request(json!({"op":"link","from":old,"to":old})).await["ok"],
        false
    );
    let mut empty =
        Session::new_investigation("diagnostics", "empty_memory", "test_actor").unwrap();
    assert_eq!(empty.request(note).await["ok"], false);
    assert_eq!(
        empty.request(json!({"op":"search","query":""})).await["result"]["hits"],
        json!([])
    );
    assert_eq!(empty.artifact()["authorship_operations"], json!([]));
    assert!(Session::new_investigation("diagnostics", "hybrid_graph_off", "test_actor").is_err());
}

#[tokio::test]
async fn investigation_rejections_have_discovery_cost_and_no_probe_effects() {
    let mut s = Session::new_investigation("async", "empty_memory", "test_actor").unwrap();
    for _ in 0..session::MAX_CALLS {
        s.request(json!({"op":"bad"})).await;
    }
    let before = json!(s.environment.states);
    let response = s
        .request(json!({"op":"probe","component":"catalog","mode":"direct"}))
        .await;
    assert_eq!(response["budget_rejected"], true);
    assert_eq!(json!(s.environment.states), before);
    let artifact = s.artifact();
    let counts = &artifact["phases"][0]["operation_counts"]["discovery"];
    assert_eq!(counts["attempted"], 1);
    assert_eq!(counts["admitted"], 0);
    assert_eq!(counts["rejected"], 1);
    assert!(counts["response_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        s.request(json!({"control":"advance","force":true})).await["ok"],
        true
    );
}

#[tokio::test]
async fn investigation_rehearsal_is_deterministic_and_does_not_require_a_winner() {
    let a = investigation::rehearse().await.unwrap();
    let b = investigation::rehearse().await.unwrap();
    assert_eq!(a, b);
    assert_eq!(a["episodes"].as_array().unwrap().len(), 4);
    for e in a["episodes"].as_array().unwrap() {
        assert_eq!(e["actor_kind"], "scripted_rehearsal");
        for p in e["phases"].as_array().unwrap() {
            assert_eq!(p["phase_passed"], true, "{e}");
            let sum: u64 = p["operation_counts"]
                .as_object()
                .unwrap()
                .values()
                .map(|c| c["attempted"].as_u64().unwrap())
                .sum();
            assert_eq!(sum, p["budget"]["attempted_calls"].as_u64().unwrap());
        }
        assert_eq!(
            e["authorship_operations"].as_array().unwrap().len(),
            if e["condition"] == "notes_search" {
                3
            } else {
                0
            }
        );
    }
    assert_eq!(
        a["episodes"][0]["fixture_hash"],
        a["episodes"][1]["fixture_hash"]
    );
    assert_eq!(
        a["episodes"][2]["fixture_hash"],
        a["episodes"][3]["fixture_hash"]
    );
    assert_eq!(
        a["protocol"]["decision_rule"]["minimum_later_discovery_reduction_per_sequence"],
        0.25
    );
}

#[tokio::test]
async fn investigation_control_has_a_one_probe_route_without_source_or_memory_reads() {
    for episode in ["diagnostics", "async"] {
        let mut s = Session::new_investigation(episode, "empty_memory", "test_actor").unwrap();
        for phase_index in 0..4 {
            let packet = s.request(json!({"op":"packet"})).await;
            for component in packet["result"]["components"].as_array().unwrap() {
                let name = &component["name"];
                let report = if episode == "diagnostics" {
                    let may_open = if phase_index == 0 {
                        true
                    } else {
                        let probe = s
                            .request(json!({"op":"probe","component":name,"initial_present":false}))
                            .await;
                        probe["result"]["observations"][1]["created"] == false
                    };
                    let inspection = if may_open {
                        Value::Null
                    } else {
                        s.request(json!({"op":"task","action":"inspect","component":name}))
                            .await
                    };
                    if may_open || inspection["result"]["present"] == true {
                        s.request(json!({"op":"task","action":"open","component":name}))
                            .await["result"]["result"]
                            .clone()
                    } else {
                        json!("missing")
                    }
                } else {
                    let mode = if phase_index == 0 {
                        "direct"
                    } else {
                        let probe = s
                            .request(json!({"op":"probe","component":name,"mode":"direct"}))
                            .await;
                        if probe["result"]["observations"][1]["heartbeat"] == "blocked" {
                            "worker"
                        } else {
                            "direct"
                        }
                    };
                    assert_eq!(
                        s.request(
                            json!({"op":"task","action":"begin","component":name,"mode":mode})
                        )
                        .await["ok"],
                        true
                    );
                    let observation = s
                        .request(json!({"op":"task","action":"tick","component":name}))
                        .await;
                    assert_eq!(
                        s.request(json!({"op":"task","action":"complete","component":name}))
                            .await["ok"],
                        true
                    );
                    observation["result"]["heartbeat"].clone()
                };
                assert_eq!(
                    s.request(
                        json!({"op":"task","action":"report","component":name,"value":report})
                    )
                    .await["ok"],
                    true
                );
            }
            s.request(json!({"op":"finish"})).await;
            assert_eq!(
                s.request(json!({"control":"grade"})).await["result"]["phase_passed"],
                true
            );
            if phase_index < 3 {
                s.request(json!({"control":"advance"})).await;
            }
        }
        let artifact = s.artifact();
        let phases = artifact["phases"].as_array().unwrap();
        assert_eq!(
            phases
                .iter()
                .skip(1)
                .map(|p| p["operation_counts"]["discovery"]["attempted"]
                    .as_u64()
                    .unwrap())
                .sum::<u64>(),
            4
        );
        assert!(
            phases
                .iter()
                .all(|p| p["operation_counts"].get("memory_access").is_none())
        );
    }
}
