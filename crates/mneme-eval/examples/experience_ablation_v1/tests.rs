use super::*;

fn scope(name: &str, revision: &str) -> Scope {
    Scope {
        id: name.into(),
        memory_id: "memory0".into(),
        presentation_revision: revision.into(),
        intended_use: "instruction".into(),
        condition: format!("Observed condition {name}"),
        source_refs: vec![format!("source-{name}")],
        refines_scope_id: None,
    }
}

fn event(index: usize, scope: &Scope, judgment: ReportedJudgment) -> Event {
    Event {
        id: format!("event{index}"),
        source_task_id: format!("task{index}"),
        memory_id: scope.memory_id.clone(),
        presentation_revision: scope.presentation_revision.clone(),
        scope_id: scope.id.clone(),
        source_ref: scope.source_refs[0].clone(),
        reported_judgment: judgment,
        note: format!("Independent observed action/result {index}"),
    }
}

fn toy() -> Case {
    let scope = scope("scope0", "r1");
    Case {
        id: "toy".into(),
        current: Current {
            goal: "Choose a procedure".into(),
            intended_use: "instruction".into(),
            facts: vec![Fact {
                id: "f0".into(),
                text: "Current observed fact".into(),
            }],
            options: vec![
                Fact {
                    id: "a".into(),
                    text: "First procedure".into(),
                },
                Fact {
                    id: "b".into(),
                    text: "Second procedure".into(),
                },
            ],
        },
        cards: (0..6)
            .map(|index| Card {
                id: format!("card{index}"),
                memory_id: format!("memory{index}"),
                presentation_revision: "r1".into(),
                summary: format!("Procedure {index}"),
                source_ref: format!("memory-source{index}"),
            })
            .collect(),
        historical_cards: Vec::new(),
        events: vec![event(0, &scope, ReportedJudgment::Helpful)],
        scopes: vec![scope],
    }
}

fn fixture(case: Case) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"schema_version": 1, "cases": [case]})).unwrap()
}

#[test]
fn schema_roundtrip_preserves_shared_notes_and_uses_native_control() {
    let case = toy();
    let output = replay_fixture(&fixture(case.clone())).unwrap();
    let result = &output.cases[0];
    assert_eq!(result.notes.events, case.events);
    assert_eq!(result.notes.scopes, case.scopes);
    assert_eq!(result.ordinary.cards, case.cards);
    assert_eq!(
        result.annotations[0].value,
        ConditionalPreference::NEUTRAL
            .updated(Judgment::Helpful)
            .value()
    );
    assert_eq!(result.replay[0].outcome, "admitted");
    assert!(result.payload_bytes.annotated <= MAX_PAYLOAD_BYTES);
}

#[test]
fn exact_replay_adds_no_evidence_or_duplicate_note() {
    let mut case = toy();
    case.events.push(case.events[0].clone());
    let result = derive(case).unwrap();
    assert_eq!(result.annotations[0].value, 0.625);
    assert_eq!(result.notes.events.len(), 1);
    assert_eq!(result.replay[1].outcome, "duplicate");
}

#[test]
fn changed_event_or_task_replay_is_rejected() {
    for change_id in [false, true] {
        let mut case = toy();
        let mut duplicate = case.events[0].clone();
        duplicate.reported_judgment = ReportedJudgment::ActivelyMisled;
        if change_id {
            duplicate.id = "other-event".into();
        }
        case.events.push(duplicate);
        assert!(derive(case).unwrap_err().contains("changed duplicate"));
    }
}

#[test]
fn changed_duplicate_scope_or_revision_is_rejected() {
    let mut case = toy();
    let other = scope("other", "r1");
    let mut second = event(1, &other, ReportedJudgment::Helpful);
    second.source_task_id = case.events[0].source_task_id.clone();
    case.scopes.push(other);
    case.events.push(second);
    assert!(
        derive(case)
            .unwrap_err()
            .contains("changed duplicate task/memory")
    );
}

#[test]
fn unknown_notes_remain_visible_without_admission() {
    let mut case = toy();
    case.events[0].reported_judgment = ReportedJudgment::Unknown;
    let result = derive(case).unwrap();
    assert!(result.annotations.is_empty() && result.states.is_empty());
    assert_eq!(result.notes.events.len(), 1);
    assert_eq!(result.notes.scopes.len(), 1);
    assert_eq!(result.replay[0].outcome, "unknown");
}

#[test]
fn fifth_rejection_preserves_notes_incumbents_and_correction() {
    let mut case = toy();
    case.scopes = (0..5)
        .map(|index| scope(&format!("s{index}"), "r1"))
        .collect();
    case.events = case
        .scopes
        .iter()
        .enumerate()
        .map(|(i, scope)| event(i, scope, ReportedJudgment::Helpful))
        .collect();
    for index in 5..8 {
        case.events.push(event(
            index,
            &case.scopes[0],
            ReportedJudgment::ActivelyMisled,
        ));
    }
    let expected_notes = case.events.clone();
    let result = derive(case).unwrap();
    assert_eq!(result.annotations.len(), 4);
    assert_eq!(result.notes.events, expected_notes);
    assert_eq!(result.notes.scopes.len(), 5);
    assert_eq!(result.replay[4].outcome, "rejected_full");
    assert!(
        result.replay[5..]
            .iter()
            .all(|row| row.outcome == "updated")
    );
    assert!(result.annotations[0].value < 0.5);
    assert!(result.annotations[1..].iter().all(|row| row.value == 0.625));
}

#[test]
fn historical_values_do_not_annotate_new_revision() {
    let mut case = toy();
    let mut old = case.cards[0].clone();
    old.id = "old-card".into();
    case.historical_cards.push(old);
    case.cards[0].presentation_revision = "r2".into();
    let result = derive(case.clone()).unwrap();
    assert!(result.annotations.is_empty());
    assert_eq!(result.states.len(), 1);
    assert_eq!(result.states[0].presentation_revision, "r1");
    assert_eq!(result.notes.events, case.events);
    let new = scope("new-scope", "r2");
    case.events
        .push(event(1, &new, ReportedJudgment::ActivelyMisled));
    case.scopes.push(new);
    let result = derive(case).unwrap();
    assert_eq!(result.annotations.len(), 1);
    assert_eq!(result.annotations[0].value, 0.375);
    assert_eq!(result.states.len(), 2);
}

#[test]
fn saturation_and_reversal_are_the_shared_rule_not_a_second_formula() {
    let mut case = toy();
    case.events = (0..9)
        .map(|i| event(i, &case.scopes[0], ReportedJudgment::Helpful))
        .collect();
    for i in 9..12 {
        case.events
            .push(event(i, &case.scopes[0], ReportedJudgment::ActivelyMisled));
    }
    let result = derive(case).unwrap();
    assert_eq!(result.replay[8].value, Some(0.95));
    assert!(result.annotations[0].value < 0.5);
}

#[test]
fn current_facts_and_gold_do_not_drive_annotations() {
    let first = fixture(toy());
    let mut second: serde_json::Value = serde_json::from_slice(&first).unwrap();
    second["private_scoring"] =
        serde_json::json!({"secret": "DO_NOT_PROJECT", "preferred_scope": "wrong"});
    let output = replay_fixture(&serde_json::to_vec(&second).unwrap()).unwrap();
    assert_eq!(replay_fixture(&first).unwrap().cases, output.cases);
    assert!(
        !serde_json::to_string(&output)
            .unwrap()
            .contains("DO_NOT_PROJECT")
    );
    let mut changed = toy();
    changed.current.facts[0].text = "Different current facts, still no inferred match".into();
    assert_eq!(
        derive(changed).unwrap().annotations,
        output.cases[0].annotations
    );
}

#[test]
fn valid_refinement_does_not_copy_parent_state() {
    let mut case = toy();
    let mut child = scope("child", "r1");
    child.refines_scope_id = Some(case.scopes[0].id.clone());
    case.events
        .push(event(1, &child, ReportedJudgment::Unknown));
    case.scopes.push(child);
    let result = derive(case).unwrap();
    assert_eq!(result.annotations.len(), 1);
    assert_eq!(result.notes.scopes.len(), 2);
    assert_eq!(result.replay[1].value, None);
}

#[test]
fn invalid_bindings_and_cycles_are_rejected() {
    let base = toy();
    let mut bad = base.clone();
    bad.events[0].scope_id = "absent".into();
    assert!(derive(bad).unwrap_err().contains("missing scope"));
    let mut bad = base.clone();
    bad.events[0].presentation_revision = "r2".into();
    assert!(derive(bad).unwrap_err().contains("revision mismatch"));
    let mut bad = base.clone();
    bad.scopes[0].refines_scope_id = Some(bad.scopes[0].id.clone());
    assert!(derive(bad).unwrap_err().contains("cyclic refinement"));
    let mut bad = base.clone();
    bad.scopes[0].source_refs.push("unsupported".into());
    assert!(derive(bad).unwrap_err().contains("no matching event"));
    let mut bad = base;
    bad.scopes[0].presentation_revision = "undeclared".into();
    assert!(
        derive(bad)
            .unwrap_err()
            .contains("undeclared memory revision")
    );
}

#[test]
fn cardinality_and_identifier_errors_fail_before_projection() {
    let mut bad = toy();
    bad.cards.pop();
    assert!(derive(bad).is_err());
    let mut bad = toy();
    bad.cards[0].id = "has space".into();
    assert!(derive(bad).is_err());
    let mut bad = toy();
    bad.events = (0..19)
        .map(|i| event(i, &bad.scopes[0], ReportedJudgment::Helpful))
        .collect();
    assert!(derive(bad).unwrap_err().contains("too many distinct"));
    let mut bad = toy();
    bad.scopes.push(bad.scopes[0].clone());
    assert!(derive(bad).unwrap_err().contains("duplicate scope"));
}

#[test]
fn oversized_shared_notes_fail_instead_of_being_truncated() {
    let mut bad = toy();
    bad.events = (0..18)
        .map(|i| {
            let mut event = event(i, &bad.scopes[0], ReportedJudgment::Helpful);
            event.note = "a".repeat(700);
            event
        })
        .collect();
    assert!(
        derive(bad)
            .unwrap_err()
            .contains("projected payload exceeds")
    );
}

#[test]
fn json_rejects_nan_bad_enums_unknown_fields_and_oversized_input() {
    let bytes = fixture(toy());
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["cases"][0]["events"][0]["reported_judgment"] = "redundant".into();
    assert!(replay_fixture(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["cases"][0]["current"]["oracle_scope"] = "scope0".into();
    assert!(replay_fixture(&serde_json::to_vec(&value).unwrap()).is_err());
    let invalid = String::from_utf8(bytes).unwrap().replacen(
        "\"schema_version\":1",
        "\"schema_version\":NaN",
        1,
    );
    assert!(replay_fixture(invalid.as_bytes()).is_err());
    assert!(
        replay_fixture(&vec![b' '; MAX_INPUT_BYTES + 1])
            .unwrap_err()
            .contains("1 MiB")
    );
}

#[test]
fn replay_order_is_explicit_and_cases_have_independent_state() {
    let mut a = toy();
    a.events
        .push(event(1, &a.scopes[0], ReportedJudgment::ActivelyMisled));
    let first = derive(a.clone()).unwrap();
    a.events.reverse();
    let second = derive(a).unwrap();
    assert_ne!(first.annotations[0].value, second.annotations[0].value);
    let mut b = toy();
    b.id = "other-case".into();
    let fixture = serde_json::json!({"schema_version":1,"cases":[toy(), b]});
    let output = replay_fixture(&serde_json::to_vec(&fixture).unwrap()).unwrap();
    assert_eq!(output.cases[0].annotations, output.cases[1].annotations);
}
