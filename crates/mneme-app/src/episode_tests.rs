use super::*;
use serde_json::{Value, json};

const ROOT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const EDITION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

fn append() -> Value {
    json!({
        "action": "append",
        "source": {"namespace": "codex", "key": "episode-1", "reference": "codex://thread/1"},
        "summary": "We watched the violet lights."
    })
}

fn revise() -> Value {
    let mut value = append();
    value["action"] = json!("revise");
    value["source"]["key"] = json!("episode-1-correction");
    value["episode_id"] = json!(ROOT);
    value["expected_edition_id"] = json!(EDITION);
    value["reason"] = json!("Correct the occurrence time.");
    value
}

fn examples() -> Vec<(&'static str, Value)> {
    vec![
        ("append", append()),
        ("revise", revise()),
        ("list", json!({"action": "list"})),
        ("search", json!({"action": "search", "cue": "violet"})),
        ("get", json!({"action": "get", "episode_id": ROOT})),
        ("history", json!({"action": "history", "episode_id": ROOT})),
        (
            "references",
            json!({"action": "references", "anchor": EDITION}),
        ),
    ]
}

fn accepts(value: &Value) {
    if let Err(error) = PreparedEpisode::parse(value) {
        panic!("expected accepted input: {value}; got {error}");
    }
}

fn rejects(value: &Value) {
    assert!(
        PreparedEpisode::parse(value).is_err(),
        "accepted invalid input: {value}"
    );
}

#[test]
fn every_action_has_explicit_authority_and_preserves_omissions() {
    for (name, value) in examples() {
        let prepared = PreparedEpisode::parse(&value).unwrap();
        assert_eq!(prepared.action().as_str(), name);
        assert_eq!(prepared.is_mutation(), matches!(name, "append" | "revise"));
        match name {
            "append" => assert!(matches!(prepared.capability(), EpisodeCapability::Curator)),
            "revise" => assert!(matches!(prepared.capability(), EpisodeCapability::Operator)),
            _ => assert!(matches!(prepared.capability(), EpisodeCapability::ReadOnly)),
        }
        assert_eq!(
            prepared.into_json(),
            value,
            "omitted defaults were materialized for {name}"
        );
    }
    let all: Vec<_> = EpisodeAction::ALL
        .iter()
        .map(|action| action.as_str())
        .collect();
    let read_only: Vec<_> = EpisodeAction::READ_ONLY
        .iter()
        .map(|action| action.as_str())
        .collect();
    let curator: Vec<_> = EpisodeAction::CURATOR
        .iter()
        .map(|action| action.as_str())
        .collect();
    assert_eq!(all.len(), 7);
    assert_eq!(read_only.len(), 5);
    assert_eq!(curator.len(), 6);
    assert!(!read_only.contains(&"append") && !read_only.contains(&"revise"));
    assert!(curator.contains(&"append") && !curator.contains(&"revise"));
    for action in all {
        assert!(examples().iter().any(|(name, _)| *name == action));
    }
}

#[test]
fn preflight_rejects_nonobjects_unknown_actions_and_unknown_fields() {
    for value in [
        json!(null),
        json!([]),
        json!("list"),
        json!(1),
        json!({}),
        json!({"action": null}),
        json!({"action": "forget"}),
    ] {
        rejects(&value);
    }
    for (_, mut value) in examples() {
        value["unrecognized"] = json!(true);
        rejects(&value);
        value.as_object_mut().unwrap().remove("unrecognized");
        value["db"] = json!("project");
        rejects(&value); // Database selection belongs to the frontend envelope.
    }
    for field in ["active", "confidence", "stability", "status"] {
        let mut value = append();
        value[field] = json!(true);
        rejects(&value);
    }
}

#[test]
fn variants_reject_each_others_fields_before_checkout() {
    for (mut value, field, extra) in [
        (append(), "expected_edition_id", json!(EDITION)),
        (append(), "reason", json!("Not a revision.")),
        (json!({"action":"list"}), "cue", json!("violet")),
        (
            json!({"action":"search","cue":"violet"}),
            "after",
            json!("cursor"),
        ),
        (json!({"action":"get","episode_id":ROOT}), "limit", json!(8)),
        (
            json!({"action":"get","episode_id":ROOT}),
            "offset",
            json!(0),
        ),
        (
            json!({"action":"get","episode_id":ROOT,"body":false}),
            "max_bytes",
            json!(16384),
        ),
        (
            json!({"action":"history","episode_id":ROOT}),
            "body",
            json!(true),
        ),
        (
            json!({"action":"references","anchor":EDITION}),
            "thread",
            json!("lights"),
        ),
    ] {
        value[field] = extra;
        rejects(&value);
    }
    for field in [
        "episode_id",
        "expected_edition_id",
        "reason",
        "source",
        "summary",
    ] {
        let mut value = revise();
        value.as_object_mut().unwrap().remove(field);
        rejects(&value); // Editorial writes are complete replacements, not patches.
    }
}

#[test]
fn all_present_optionals_refuse_null_instead_of_using_defaults() {
    let groups = [
        (
            append(),
            vec![
                "body",
                "tags",
                "occurred",
                "thread",
                "occurrence_contexts",
                "links",
            ],
        ),
        (
            json!({"action":"list"}),
            vec![
                "axis",
                "order",
                "from",
                "through",
                "thread",
                "occurrence",
                "limit",
                "after",
            ],
        ),
        (
            json!({"action":"search","cue":"violet"}),
            vec!["thread", "occurrence", "limit"],
        ),
        (
            json!({"action":"get","episode_id":ROOT}),
            vec!["edition_id", "body", "offset", "max_bytes"],
        ),
        (
            json!({"action":"history","episode_id":ROOT}),
            vec!["limit", "after"],
        ),
        (
            json!({"action":"references","anchor":ROOT}),
            vec!["limit", "after"],
        ),
    ];
    for (base, fields) in groups {
        for field in fields {
            let mut value = base.clone();
            value[field] = Value::Null;
            rejects(&value);
        }
    }
    for field in ["session", "revision"] {
        let mut value = append();
        value["source"][field] = Value::Null;
        rejects(&value);
    }
    for field in ["kind", "weight"] {
        let mut value = append();
        let mut link = json!({"to":ROOT});
        link[field] = Value::Null;
        value["links"] = json!([link]);
        rejects(&value);
    }
}

#[test]
fn source_identity_is_checked_before_execution() {
    for (field, invalid) in [
        ("namespace", json!("Codex")),
        ("namespace", json!("two words")),
        ("namespace", json!("x".repeat(65))),
        ("key", json!("")),
        ("key", json!("x".repeat(513))),
        ("reference", json!("leading\ncontrol")),
        ("reference", json!("x".repeat(2049))),
        ("session", json!(" ")),
        ("session", json!("x".repeat(513))),
        ("revision", json!("x".repeat(257))),
        ("extra", json!(true)),
    ] {
        let mut value = append();
        value["source"][field] = invalid;
        rejects(&value);
    }
    for field in ["namespace", "key", "reference"] {
        let mut value = append();
        value["source"].as_object_mut().unwrap().remove(field);
        rejects(&value);
    }
}

#[test]
fn text_limits_measure_utf8_bytes_without_banning_body_newlines() {
    for (base, field, max) in [
        (append(), "summary", 2048usize),
        (append(), "thread", 128usize),
        (revise(), "reason", 1024usize),
        (json!({"action":"search","cue":"violet"}), "cue", 4096usize),
    ] {
        let mut value = base.clone();
        value[field] = json!("é".repeat(max / 2));
        accepts(&value);
        value[field] = json!("é".repeat(max / 2 + 1));
        rejects(&value);
        for invalid in [
            "",
            " ",
            " untrimmed",
            "untrimmed ",
            "line\nbreak",
            "nul\0byte",
        ] {
            value[field] = json!(invalid);
            rejects(&value);
        }
    }
    let mut value = append();
    for body in [
        String::new(),
        "line one\nline two\twith a tab".into(),
        "é".repeat(8192),
    ] {
        value["body"] = json!(body);
        accepts(&value);
    }
    value["body"] = json!("é".repeat(8193));
    rejects(&value);
}

#[test]
fn tags_cannot_promote_an_episode_to_core() {
    let mut value = append();
    for tags in [
        json!(["core"]),
        json!(["ordinary", "core"]),
        json!([""]),
        json!([" leading"]),
        json!(["line\nbreak"]),
        json!(["é".repeat(129)]),
        json!(vec!["tag"; 33]),
    ] {
        value["tags"] = tags;
        rejects(&value);
    }
    for tags in [
        json!([]),
        json!(["episode", "agent-continuity"]),
        json!(["é".repeat(128)]),
        json!(
            (0..32)
                .map(|index| format!("tag-{index}"))
                .collect::<Vec<_>>()
        ),
    ] {
        value["tags"] = tags;
        accepts(&value);
    }
}

#[test]
fn occurrence_variants_keep_unknown_distinct_and_reject_invalid_times() {
    let mut value = append();
    for occurred in [
        json!({"kind":"unknown"}),
        json!({"kind":"point","at":0}),
        json!({"kind":"point","at":i64::MAX}),
        json!({"kind":"range","start":0,"end":1}),
    ] {
        value["occurred"] = occurred;
        accepts(&value);
    }
    for occurred in [
        json!({}),
        json!({"kind":"unknown","at":1}),
        json!({"kind":"point"}),
        json!({"kind":"point","at":null}),
        json!({"kind":"point","at":-1}),
        json!({"kind":"point","at":1.5}),
        json!({"kind":"point","at":(i64::MAX as u64)+1}),
        json!({"kind":"point","at":"2026-09-27"}),
        json!({"kind":"point","at":1,"end":2}),
        json!({"kind":"range","start":0}),
        json!({"kind":"range","start":1,"end":1}),
        json!({"kind":"range","start":2,"end":1}),
        json!({"kind":"range","start":0,"end":null}),
    ] {
        value["occurred"] = occurred;
        rejects(&value);
    }
}

#[test]
fn temporal_filters_validate_bounds_and_do_not_invent_unknown_occurrences() {
    for action in ["list", "search"] {
        let mut value = if action == "list" {
            json!({"action":"list"})
        } else {
            json!({"action":"search","cue":"violet"})
        };
        for filter in [
            json!({"kind":"any"}),
            json!({"kind":"unknown"}),
            json!({"kind":"overlaps","from":0}),
            json!({"kind":"overlaps","through":i64::MAX}),
            json!({"kind":"overlaps","from":5,"through":5}),
        ] {
            value["occurrence"] = filter;
            accepts(&value);
        }
        for filter in [
            json!({}),
            json!({"kind":"overlaps"}),
            json!({"kind":"overlaps","from":2,"through":1}),
            json!({"kind":"overlaps","from":null}),
            json!({"kind":"overlaps","through":-1}),
            json!({"kind":"unknown","from":0}),
            json!({"kind":"any","extra":true}),
        ] {
            value["occurrence"] = filter;
            rejects(&value);
        }
    }
    accepts(
        &json!({"action":"list","axis":"occurred","order":"oldest_first","from":5,"through":5}),
    );
    accepts(&json!({"action":"list","axis":"recorded","occurrence":{"kind":"unknown"}}));
    for value in [
        json!({"action":"list","axis":"occurred","occurrence":{"kind":"unknown"}}),
        json!({"action":"list","axis":"created"}),
        json!({"action":"list","order":"ascending"}),
        json!({"action":"list","from":2,"through":1}),
        json!({"action":"list","from":-1}),
        json!({"action":"list","through":(i64::MAX as u64)+1}),
    ] {
        rejects(&value);
    }
}

#[test]
fn ids_and_pagination_bounds_are_checked_without_store_access() {
    for (base, fields) in [
        (revise(), vec!["episode_id", "expected_edition_id"]),
        (
            json!({"action":"get","episode_id":ROOT}),
            vec!["episode_id", "edition_id"],
        ),
        (
            json!({"action":"history","episode_id":ROOT}),
            vec!["episode_id"],
        ),
        (json!({"action":"references","anchor":ROOT}), vec!["anchor"]),
    ] {
        for field in fields {
            for invalid in [
                json!(""),
                json!("not-an-id"),
                json!("ZZZZZZZZZZZZZZZZZZZZZZZZZZ"),
                json!(1),
                json!(null),
            ] {
                let mut value = base.clone();
                value[field] = invalid;
                rejects(&value);
            }
        }
    }
    for (_, base) in examples()
        .into_iter()
        .filter(|(name, _)| matches!(*name, "list" | "search" | "history" | "references"))
    {
        for limit in [json!(1), json!(32)] {
            let mut value = base.clone();
            value["limit"] = limit;
            accepts(&value);
        }
        for limit in [json!(0), json!(33), json!(-1), json!(1.5), json!("8")] {
            let mut value = base.clone();
            value["limit"] = limit;
            rejects(&value);
        }
    }
    for (_, base) in examples()
        .into_iter()
        .filter(|(name, _)| matches!(*name, "list" | "history" | "references"))
    {
        for cursor in [
            json!(""),
            json!("not-a-cursor"),
            json!("x".repeat(1025)),
            json!({"position":1}),
        ] {
            let mut value = base.clone();
            value["after"] = cursor;
            rejects(&value);
        }
    }
}

#[test]
fn authored_links_are_bounded_distinct_and_cannot_smuggle_supersession() {
    let mut value = append();
    for links in [
        json!({}),
        json!([null]),
        json!([{"to":"not-an-id"}]),
        json!([{"to":ROOT,"weight":-0.1}]),
        json!([{"to":ROOT,"weight":1.1}]),
        json!([{"to":ROOT,"kind":"supersedes"}]),
        json!([{"to":ROOT,"kind":"bridge"}]),
        json!([{"to":ROOT,"extra":true}]),
        json!([{"to":ROOT},{"to":ROOT,"kind":"derived_from"}]),
        json!(
            (1u128..=9)
                .map(|id| json!({"to":ulid::Ulid::from(id).to_string()}))
                .collect::<Vec<_>>()
        ),
    ] {
        value["links"] = links;
        rejects(&value);
    }
    for kind in ["associative", "transition", "derived_from"] {
        for weight in [0.0, 0.5, 1.0] {
            value["links"] = json!([{"to":ROOT,"kind":kind,"weight":weight}]);
            accepts(&value);
        }
    }
    value["links"] = json!(
        (1u128..=8)
            .map(|id| json!({"to":ulid::Ulid::from(id).to_string()}))
            .collect::<Vec<_>>()
    );
    accepts(&value);
}

#[test]
fn get_body_controls_are_explicit_bounded_and_byte_based() {
    for value in [
        json!({"action":"get","episode_id":ROOT,"body":false}),
        json!({"action":"get","episode_id":ROOT,"body":true,"offset":0,"max_bytes":16384}),
        json!({"action":"get","episode_id":ROOT,"body":true,"offset":1,"max_bytes":1}),
        json!({"action":"get","episode_id":ROOT,"body":true,"edition_id":EDITION,"max_bytes":16384}),
    ] {
        accepts(&value);
    }
    for value in [
        json!({"action":"get","episode_id":ROOT,"offset":0,"max_bytes":16384}),
        json!({"action":"get","episode_id":ROOT,"offset":1}),
        json!({"action":"get","episode_id":ROOT,"body":false,"max_bytes":16}),
        json!({"action":"get","episode_id":ROOT,"body":true,"offset":-1}),
        json!({"action":"get","episode_id":ROOT,"body":true,"offset":0.5}),
        json!({"action":"get","episode_id":ROOT,"body":true,"max_bytes":0}),
        json!({"action":"get","episode_id":ROOT,"body":true,"max_bytes":16385}),
    ] {
        rejects(&value);
    }
}

#[test]
fn full_inputs_round_trip_without_changing_source_or_editorial_intent() {
    let mut value = revise();
    value["source"]["session"] = json!("session-1");
    value["source"]["revision"] = json!("draft-2");
    value["body"] = json!("A complete replacement.\nNot a patch.");
    value["tags"] = json!(["observation"]);
    value["thread"] = json!("violet-evening");
    value["occurred"] = json!({"kind":"range","start":123,"end":456});
    value["links"] = json!([{"to":ROOT,"kind":"derived_from","weight":0.75}]);
    assert_eq!(PreparedEpisode::parse(&value).unwrap().into_json(), value);
}

// Deliberately small, test-only interpreter for the discovery schema's structural
// subset. Domain rules (UTF-8 byte counts, cursor binding, time-window ordering,
// ULID parsing) remain the Rust parser's job; this is not a JSON Schema engine.
fn schema_shape_accepts(schema: &Value, value: &Value) -> bool {
    if let Some(accepts) = schema.as_bool() {
        return accepts;
    }
    if let Some(expected) = schema.get("const")
        && expected != value
    {
        return false;
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return false;
    }
    if let Some(types) = schema.get("type") {
        let type_matches = |name: &str| match name {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "null" => value.is_null(),
            unknown => panic!("unsupported schema type in test: {unknown}"),
        };
        let matches = if let Some(name) = types.as_str() {
            type_matches(name)
        } else {
            types
                .as_array()
                .unwrap()
                .iter()
                .any(|name| type_matches(name.as_str().unwrap()))
        };
        if !matches {
            return false;
        }
    }
    for (keyword, minimum, maximum) in [
        ("allOf", true, false),
        ("anyOf", false, false),
        ("oneOf", false, true),
    ] {
        if let Some(branches) = schema.get(keyword).and_then(Value::as_array) {
            let count = branches
                .iter()
                .filter(|branch| schema_shape_accepts(branch, value))
                .count();
            if (minimum && count != branches.len())
                || (!minimum && count == 0)
                || (maximum && count != 1)
            {
                return false;
            }
        }
    }
    if let Some(negated) = schema.get("not")
        && schema_shape_accepts(negated, value)
    {
        return false;
    }
    if let Some(condition) = schema.get("if") {
        let branch = if schema_shape_accepts(condition, value) {
            schema.get("then")
        } else {
            schema.get("else")
        };
        if branch.is_some_and(|branch| !schema_shape_accepts(branch, value)) {
            return false;
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array)
            && required
                .iter()
                .any(|field| !object.contains_key(field.as_str().unwrap()))
        {
            return false;
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        for (name, child) in object {
            if let Some(property) = properties.and_then(|properties| properties.get(name)) {
                if !schema_shape_accepts(property, child) {
                    return false;
                }
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return false;
            }
        }
    }
    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if schema
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|min| length < min)
            || schema
                .get("maxLength")
                .and_then(Value::as_u64)
                .is_some_and(|max| length > max)
        {
            return false;
        }
    }
    if let Some(items) = value.as_array() {
        let length = items.len() as u64;
        if schema
            .get("minItems")
            .and_then(Value::as_u64)
            .is_some_and(|min| length < min)
            || schema
                .get("maxItems")
                .and_then(Value::as_u64)
                .is_some_and(|max| length > max)
            || schema.get("items").is_some_and(|item_schema| {
                items
                    .iter()
                    .any(|item| !schema_shape_accepts(item_schema, item))
            })
        {
            return false;
        }
    }
    if let Some(number) = value.as_f64()
        && (schema
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|min| number < min)
            || schema
                .get("maximum")
                .and_then(Value::as_f64)
                .is_some_and(|max| number > max))
    {
        return false;
    }
    true
}

#[test]
fn discovery_schemas_expose_exact_profile_actions() {
    for allowed in [
        &EpisodeAction::ALL[..],
        &EpisodeAction::READ_ONLY[..],
        &EpisodeAction::CURATOR[..],
    ] {
        let schema = input_schema(allowed);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        for (name, value) in examples() {
            let expected = allowed.iter().any(|action| action.as_str() == name);
            assert_eq!(
                schema_shape_accepts(&schema, &value),
                expected,
                "schema action scope diverged for {name}: {schema}"
            );
        }
    }
}

#[test]
fn action_discovery_branches_are_complete_without_root_intersection() {
    let schema = input_schema(&EpisodeAction::ALL);
    for branch in schema["oneOf"].as_array().unwrap() {
        let action = branch["properties"]["action"]["const"].as_str().unwrap();
        assert_eq!(branch["type"], "object");
        assert_eq!(branch["additionalProperties"], false);
        assert!(branch.get("allOf").is_none());
        for field in branch["required"].as_array().unwrap() {
            assert!(branch["properties"].get(field.as_str().unwrap()).is_some());
        }
        for (name, example) in examples() {
            assert_eq!(schema_shape_accepts(branch, &example), name == action);
        }
        if matches!(action, "append" | "revise") {
            assert_eq!(branch["properties"]["source"]["type"], "object");
            assert_eq!(branch["properties"]["summary"]["type"], "string");
            assert_eq!(branch["properties"]["body"]["type"], "string");
        }
    }
}

#[test]
fn schema_and_parser_agree_on_variant_shape_and_presence() {
    let schema = input_schema(&EpisodeAction::ALL);
    let mut cases = Vec::new();
    for (_, base) in examples() {
        cases.push((base.clone(), true));
        let mut missing_action = base.clone();
        missing_action.as_object_mut().unwrap().remove("action");
        cases.push((missing_action, false));
        let mut unknown = base.clone();
        unknown["unrecognized"] = json!(1);
        cases.push((unknown, false));
        for field in base.as_object().unwrap().keys() {
            let mut missing = base.clone();
            missing.as_object_mut().unwrap().remove(field);
            cases.push((missing, false));
            let mut null = base.clone();
            null[field] = Value::Null;
            cases.push((null, false));
        }
    }
    for (mut value, field, extra) in [
        (append(), "body", json!(true)),
        (append(), "occurred", json!({"kind":"unknown","at":1})),
        (append(), "links", json!([{"to":ROOT,"kind":"supersedes"}])),
        (append(), "active", json!(false)),
        (append(), "reason", json!("Not an edit.")),
        (revise(), "body", json!(false)),
        (json!({"action":"list"}), "limit", json!(0)),
        (json!({"action":"list"}), "limit", json!(33)),
        (json!({"action":"list"}), "limit", json!(null)),
        (json!({"action":"list"}), "summary", json!("Not a write.")),
        (
            json!({"action":"search","cue":"violet"}),
            "after",
            json!("cursor"),
        ),
        (
            json!({"action":"get","episode_id":ROOT}),
            "body",
            json!("Not a boolean."),
        ),
        (
            json!({"action":"history","episode_id":ROOT}),
            "body",
            json!(true),
        ),
        (
            json!({"action":"references","anchor":ROOT}),
            "edition_id",
            json!(EDITION),
        ),
    ] {
        value[field] = extra;
        cases.push((value, false));
    }
    for (value, expected) in cases {
        assert_eq!(
            PreparedEpisode::parse(&value).is_ok(),
            expected,
            "parser: {value}"
        );
        assert_eq!(
            schema_shape_accepts(&schema, &value),
            expected,
            "schema: {value}"
        );
    }
}

#[test]
fn escaped_page_budget_uses_last_delivered_row_for_continuation() {
    let items = (0..32).collect::<Vec<_>>();
    let render = |row: &usize| json!({"row":row,"summary":"\"".repeat(2048)});
    let packed = pack_page(
        "list",
        &items,
        Some("backend-lookahead".into()),
        false,
        render,
        |row| Ok(format!("after-{row}")),
    )
    .unwrap();
    assert!(serde_json::to_vec(&packed).unwrap().len() <= MAX_EPISODE_RESPONSE_BYTES);
    let delivered = packed["items"].as_array().unwrap();
    assert!(!delivered.is_empty() && delivered.len() < items.len());
    for (index, row) in delivered.iter().enumerate() {
        assert_eq!(row["row"], index);
    }
    assert_eq!(packed["next"], format!("after-{}", delivered.len() - 1));
    // A backend lookahead or an omitted-row cursor would skip unread history.
    assert_ne!(packed["next"], "backend-lookahead");
    assert_eq!(packed["partial"], false);
}

#[test]
fn small_and_work_limited_pages_keep_backend_progress() {
    let packed = pack_page(
        "history",
        &[1usize, 2],
        Some("backend-next".into()),
        false,
        |row| json!({"row":row}),
        |_| panic!("a complete fetched page must keep its backend continuation"),
    )
    .unwrap();
    assert_eq!(packed["items"], json!([{"row":1},{"row":2}]));
    assert_eq!(packed["next"], "backend-next");
    let empty: [usize; 0] = [];
    let partial = pack_page(
        "list",
        &empty,
        Some("last-inspected".into()),
        true,
        |_| unreachable!(),
        |_| unreachable!(),
    )
    .unwrap();
    assert_eq!(partial["items"], json!([]));
    assert_eq!(partial["next"], "last-inspected");
    assert_eq!(partial["partial"], true);
    assert!(
        pack_page(
            "list",
            &[0usize],
            None,
            false,
            |_| json!({"unbounded":"x".repeat(MAX_EPISODE_RESPONSE_BYTES)}),
            |_| Ok("unused".into()),
        )
        .is_err()
    );
}

fn body_chunk(bytes: &[u8], start: u64, next: Option<u64>) -> mneme_core::ports::BodyChunk {
    mneme_core::ports::BodyChunk {
        bytes: bytes.to_vec(),
        source_start: start,
        source_end: start + bytes.len() as u64,
        next_offset: next,
    }
}

#[test]
fn escaped_body_packing_preserves_exact_byte_continuity_and_json_budget() {
    // Control bytes expand sixfold in JSON. Mixed-width characters distinguish
    // source byte offsets from Unicode scalars and encoded response byte counts.
    let body = "\0\0\0\"\\é🪨".repeat(1400);
    assert!(body.len() <= MAX_EPISODE_BODY_BYTES);
    let mut offset = 0;
    let mut reconstructed = String::new();
    let mut reads = 0;
    while offset < body.len() {
        let mut value = json!({"action":"get","summary":"details","metadata":"m".repeat(6000)});
        pack_body(
            &mut value,
            &body_chunk(&body.as_bytes()[offset..], offset as u64, None),
        )
        .unwrap();
        reads += 1;
        assert!(reads <= 16, "body continuation failed to progress");
        assert!(serde_json::to_vec(&value).unwrap().len() <= MAX_EPISODE_RESPONSE_BYTES);
        let delivered = value["body"].as_str().unwrap();
        let end = value["body_range"]["source_end"].as_u64().unwrap() as usize;
        assert_eq!(value["body_range"]["source_start"], offset);
        assert_eq!(end, offset + delivered.len());
        assert!(end > offset && body.is_char_boundary(end));
        assert_eq!(delivered, &body[offset..end]);
        reconstructed.push_str(delivered);
        if end < body.len() {
            assert_eq!(value["body_range"]["has_more"], true);
            assert_eq!(value["body_range"]["next_offset"], end);
        } else {
            assert_eq!(value["body_range"]["has_more"], false);
            assert!(value["body_range"]["next_offset"].is_null());
        }
        offset = end;
    }
    assert!(reads > 1, "fixture must exercise response-budget trimming");
    assert_eq!(reconstructed, body);
}

#[test]
fn body_packing_handles_backend_continuations_and_utf8_edges_without_loss() {
    let mut value = json!({"action":"get"});
    // A backend chunk can end inside the next character. Leave its leading
    // byte unread so the next request starts at the original UTF-8 boundary.
    let body = "éé".as_bytes();
    pack_body(&mut value, &body_chunk(&body[..3], 10, Some(13))).unwrap();
    assert_eq!(value["body"], "é");
    assert_eq!(
        value["body_range"],
        json!({"source_start":10,"source_end":12,"next_offset":12,"has_more":true})
    );

    let mut value = json!({"action":"get"});
    pack_body(&mut value, &body_chunk("é".as_bytes(), 12, Some(14))).unwrap();
    assert_eq!(value["body_range"]["next_offset"], 14);
    assert_eq!(value["body_range"]["has_more"], true);

    let mut value = json!({"action":"get"});
    assert!(pack_body(&mut value, &body_chunk(&body[1..], 1, None)).is_err());
    assert!(pack_body(&mut value, &body_chunk(&body[..1], 0, Some(1))).is_err());

    let mut empty = json!({"action":"get"});
    pack_body(&mut empty, &body_chunk(&[], 4, None)).unwrap();
    assert_eq!(empty["body"], "");
    assert_eq!(
        empty["body_range"],
        json!({"source_start":4,"source_end":4,"next_offset":null,"has_more":false})
    );

    let mut excessive_details =
        json!({"action":"get","summary":"x".repeat(MAX_EPISODE_RESPONSE_BYTES)});
    assert!(pack_body(&mut excessive_details, &body_chunk(b"a", 0, None)).is_err());
}

fn write_proof_fixture(prepared: &PreparedEpisode) -> (Value, Value) {
    let (write, root, ordinal, revises, reason) = match &prepared.request {
        Request::Append(write) => (
            write,
            EpisodeId::new(prepared.expected_edition_id().unwrap().unwrap()),
            EpisodeRevision::INITIAL,
            None,
            None,
        ),
        Request::Revise {
            root,
            expected,
            reason,
            write,
        } => (
            write,
            *root,
            EpisodeRevision::new(2),
            Some(*expected),
            Some(reason),
        ),
        _ => panic!("fixture requires an episode write"),
    };
    let tags = write
        .input
        .tags
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let request = write.request(&tags, None).unwrap();
    let source = if let (Some(predecessor), Some(reason)) = (revises, reason) {
        request
            .validated_revision_source(root, predecessor, ordinal, reason)
            .unwrap()
    } else {
        request.validated_source().unwrap()
    };
    let edition = source.node_id();
    let receipt = json!({
        "action":prepared.action().as_str(),"episode_id":root,"edition_id":edition,
        "revision":ordinal,"replayed":false,
    });
    let mut readback = json!({
        "action":"get","episode_id":root,"edition_id":edition,"revision":ordinal,
        "summary":write.input.summary,"occurred":occurrence_json(&write.occurrence),
        "thread":write.thread,"revises":revises,"edit_reason":reason,
        "tags":tags,"source":source_json(&source),"recorded_at":12,
        "edition_recorded_at":if ordinal == EpisodeRevision::INITIAL {12} else {30},
        "current_edition_id":edition,"is_current":true,
        "body":std::str::from_utf8(write.body()).unwrap(),
        "body_range":{"source_start":0,"source_end":write.body().len(),"next_offset":null,"has_more":false},
    });
    if let Some(contexts) = &write.occurrence_contexts {
        readback["occurrence_contexts"] = json!(contexts);
    }
    (receipt, readback)
}

#[test]
fn write_receipt_and_readback_prove_exact_append_including_empty_body() {
    for empty_body in [false, true] {
        let mut input = append();
        input["tags"] = json!(["observation", "violet"]);
        input["thread"] = json!("lights");
        input["occurred"] = json!({"kind":"point","at":42});
        if empty_body {
            input["body"] = json!("");
        }
        let prepared = PreparedEpisode::parse(&input).unwrap();
        let (receipt, readback) = write_proof_fixture(&prepared);
        prepared.verify_write_receipt_json(&receipt).unwrap();
        prepared.verify_readback_json(&receipt, &readback).unwrap();
        assert_eq!(
            prepared.expected_body().unwrap(),
            readback["body"].as_str().unwrap().as_bytes()
        );
        for (field, wrong) in [
            ("action", json!("revise")),
            ("episode_id", json!(ROOT)),
            ("edition_id", json!(EDITION)),
            ("revision", json!(1)),
            ("replayed", Value::Null),
        ] {
            let mut bad = receipt.clone();
            bad[field] = wrong;
            assert!(
                prepared.verify_write_receipt_json(&bad).is_err(),
                "receipt accepted: {bad}"
            );
        }
        let mut replay = receipt.clone();
        replay["replayed"] = json!(true);
        prepared.verify_readback_json(&replay, &readback).unwrap();
    }
}

#[test]
fn readback_rejects_changed_authorship_content_and_partial_body() {
    let prepared = PreparedEpisode::parse(&append()).unwrap();
    let (receipt, readback) = write_proof_fixture(&prepared);
    for (field, wrong) in [
        ("summary", json!("Something else happened.")),
        ("body", json!("Truncated")),
        ("occurred", json!({"kind":"point","at":12})),
        ("thread", json!("other")),
        ("tags", json!(["injected"])),
        ("revision", json!(1)),
        ("edition_recorded_at", json!(13)),
        ("is_current", json!(false)),
        (
            "body_range",
            json!({"source_start":0,"source_end":prepared.expected_body().unwrap().len(),"next_offset":1,"has_more":true}),
        ),
    ] {
        let mut bad = readback.clone();
        bad[field] = wrong;
        assert!(
            prepared.verify_readback_json(&receipt, &bad).is_err(),
            "readback accepted: {bad}"
        );
    }
    let mut wrong_source = readback.clone();
    wrong_source["source"]["request_digest_sha256"] = json!("0".repeat(64));
    assert!(
        prepared
            .verify_readback_json(&receipt, &wrong_source)
            .is_err()
    );
    let mut wrong_source = readback.clone();
    wrong_source["source"]["reference"] = json!("not-the-authored-reference");
    assert!(
        prepared
            .verify_readback_json(&receipt, &wrong_source)
            .is_err()
    );
    let mut missing_body = readback.clone();
    missing_body.as_object_mut().unwrap().remove("body");
    assert!(
        prepared
            .verify_readback_json(&receipt, &missing_body)
            .is_err()
    );
}

#[test]
fn editorial_readback_binds_root_predecessor_reason_and_ordinal() {
    let prepared = PreparedEpisode::parse(&revise()).unwrap();
    let (receipt, readback) = write_proof_fixture(&prepared);
    prepared.verify_readback_json(&receipt, &readback).unwrap();
    assert_eq!(receipt["episode_id"], ROOT);
    assert_eq!(readback["revises"], EDITION);
    for (field, wrong) in [
        ("episode_id", json!(EDITION)),
        ("revises", json!(ROOT)),
        ("edit_reason", json!("An invented justification.")),
        ("revision", json!(3)),
    ] {
        let mut bad = readback.clone();
        bad[field] = wrong;
        assert!(
            prepared.verify_readback_json(&receipt, &bad).is_err(),
            "editorial readback accepted: {bad}"
        );
    }
    let mut bad_receipt = receipt.clone();
    let mut bad_readback = readback.clone();
    bad_receipt["revision"] = json!(3);
    bad_readback["revision"] = json!(3);
    assert!(
        prepared
            .verify_readback_json(&bad_receipt, &bad_readback)
            .is_err(),
        "changed ordinal must not verify against original source digest"
    );
    bad_receipt["revision"] = json!(0);
    assert!(prepared.verify_write_receipt_json(&bad_receipt).is_err());

    // An exact retry may name an edition that has since been superseded.
    let mut historical = readback.clone();
    historical["current_edition_id"] = json!(ROOT);
    historical["is_current"] = json!(false);
    prepared
        .verify_readback_json(&receipt, &historical)
        .unwrap();

    let read = PreparedEpisode::parse(&json!({"action":"get","episode_id":ROOT})).unwrap();
    assert_eq!(read.expected_edition_id().unwrap(), None);
    assert_eq!(read.expected_body(), None);
    assert!(read.verify_write_receipt_json(&receipt).is_err());
}

#[test]
fn write_preflight_reserves_encoded_body_room_not_just_individual_field_limits() {
    let mut value = append();
    value["summary"] = json!("\"".repeat(2048));
    value["source"] = json!({
        "namespace":"n".repeat(64),"key":"\"".repeat(512),
        "reference":"\"".repeat(2048),"session":"\"".repeat(512),
        "revision":"\"".repeat(256),
    });
    value["thread"] = json!("\"".repeat(128));
    value["tags"] = json!(
        (0..32)
            .map(|index| format!("{index:02}{}", "\"".repeat(254)))
            .collect::<Vec<_>>()
    );
    value["body"] = json!("\0".repeat(16 * 1024));
    // Every individual value is legal, but JSON escaping plus all metadata
    // would leave too little room to read this edition back economically.
    rejects(&value);
}

#[test]
fn worst_case_escaped_body_finishes_within_eight_bounded_reads() {
    let mut body = "\0".repeat(MAX_EPISODE_BODY_BYTES - 4);
    body.push('🪨');
    let header = json!({"action":"get","metadata":"\"".repeat(8000)});
    assert!(serde_json::to_vec(&header).unwrap().len() < 16 * 1024);
    let mut reconstructed = String::new();
    let mut start = 0;
    let mut reads = 0;
    while start < body.len() {
        reads += 1;
        assert!(
            reads <= 8,
            "body must remain within the native readback envelope"
        );
        let mut value = header.clone();
        pack_body(
            &mut value,
            &body_chunk(&body.as_bytes()[start..], start as u64, None),
        )
        .unwrap();
        assert!(serde_json::to_vec(&value).unwrap().len() <= MAX_EPISODE_RESPONSE_BYTES);
        let text = value["body"].as_str().unwrap();
        assert!(!text.is_empty());
        let end = start + text.len();
        assert!(body.is_char_boundary(end));
        assert_eq!(value["body_range"]["source_start"], start);
        assert_eq!(value["body_range"]["source_end"], end);
        assert_eq!(value["body_range"]["has_more"], end < body.len());
        if end < body.len() {
            assert_eq!(value["body_range"]["next_offset"], end);
        } else {
            assert!(value["body_range"]["next_offset"].is_null());
        }
        reconstructed.push_str(text);
        start = end;
    }
    assert!(
        reads >= 5,
        "fixture must cover multiple JSON-escaping-limited chunks"
    );
    assert_eq!(reconstructed, body);
}

fn contexts() -> Value {
    json!([
        {"namespace":"Workplace","key":"Shared room","label":"The violet room"},
        {"namespace":"Chat","key":"conversation/1"}
    ])
}

#[test]
fn occurrence_context_admission_is_strict_opaque_bounded_and_canonical() {
    for mut value in [append(), revise()] {
        value["occurrence_contexts"] = contexts();
        let prepared = PreparedEpisode::parse(&value).unwrap();
        let frozen = prepared.into_json();
        assert_eq!(frozen["occurrence_contexts"][0]["namespace"], "Chat");
        assert_eq!(frozen["occurrence_contexts"][1]["key"], "Shared room");
        assert_eq!(frozen["source"], value["source"]);
        assert_eq!(PreparedEpisode::parse(&frozen).unwrap().into_json(), frozen);
    }
    for invalid in [
        json!(null),
        json!([]),
        json!({"namespace":"room","key":"a"}),
        json!([null]),
        json!(["room"]),
        json!([{}]),
        json!([{"namespace":"room","key":"a","extra":true}]),
        json!([{"namespace":"room","key":"a","label":null}]),
        json!([{"namespace":"room","key":3}]),
        json!([{"namespace":"room","key":"a"},{"namespace":"room","key":"a","label":"different"}]),
        json!([{"namespace":"room","key":"x".repeat(1024)}]),
    ] {
        let mut value = append();
        value["occurrence_contexts"] = invalid;
        rejects(&value);
    }
    for field in ["namespace", "key", "label"] {
        for invalid in ["", " ", " padded", "padded ", "line\nbreak", "nul\0byte"] {
            let mut value = append();
            value["occurrence_contexts"] = json!([{"namespace":"Room","key":"a","label":"label"}]);
            value["occurrence_contexts"][0][field] = json!(invalid);
            rejects(&value);
        }
    }
    // Namespace/key are not recorder namespace syntax, registries or aliases.
    let mut value = append();
    value["occurrence_contexts"] = json!([
        {"namespace":"Room / 東京","key":"a"},
        {"namespace":"Room / 東京","key":"A"},
        {"namespace":"room / 東京","key":"a"}
    ]);
    accepts(&value);
    let minimum = serde_json::to_vec(&json!([{"namespace":"n","key":""}]))
        .unwrap()
        .len();
    let mut value = append();
    value["occurrence_contexts"] = json!([{"namespace":"n","key":"x".repeat(1024 - minimum)}]);
    accepts(&value); // Exact total compact JSON byte limit, not a field limit.
    value["occurrence_contexts"][0]["key"] = json!("x".repeat(1025 - minimum));
    rejects(&value);
    let mut value = append();
    value["occurrence_contexts"] =
        json!([{"namespace":"n","key":"é".repeat(500),"label":"too much"}]);
    rejects(&value); // UTF-8 bytes, including optional label and JSON overhead.
}

#[test]
fn occurrence_context_schema_is_closed_and_write_only() {
    let schema = input_schema(&EpisodeAction::ALL);
    for (_, mut value) in examples() {
        value["occurrence_contexts"] = contexts();
        let write = matches!(value["action"].as_str(), Some("append" | "revise"));
        assert_eq!(schema_shape_accepts(&schema, &value), write);
        assert_eq!(PreparedEpisode::parse(&value).is_ok(), write);
    }
    for invalid in [
        json!([]),
        json!(null),
        json!([{}]),
        json!([{"namespace":"room","key":"a","label":null}]),
        json!([{"namespace":"room","key":"a","extra":true}]),
    ] {
        let mut value = append();
        value["occurrence_contexts"] = invalid;
        assert!(!schema_shape_accepts(&schema, &value));
    }
}

#[test]
fn occurrence_context_proof_binds_exact_context_and_codec_without_hidden_inheritance() {
    use mneme_core::CaptureRequestCodec;
    for raw in [append(), revise()] {
        let old = PreparedEpisode::parse(&raw).unwrap();
        let (_, old_readback) = write_proof_fixture(&old);
        assert_eq!(
            old_readback["source"]["request_codec"],
            json!(CaptureRequestCodec::EpisodeV1)
        );
        assert!(old_readback.get("occurrence_contexts").is_none());
        let mut contextual = raw.clone();
        contextual["occurrence_contexts"] = contexts();
        let prepared = PreparedEpisode::parse(&contextual).unwrap();
        let (receipt, readback) = write_proof_fixture(&prepared);
        assert_eq!(
            readback["source"]["request_codec"],
            json!(CaptureRequestCodec::EpisodeV2)
        );
        assert_ne!(
            readback["source"]["request_digest_sha256"],
            old_readback["source"]["request_digest_sha256"]
        );
        prepared.verify_readback_json(&receipt, &readback).unwrap();
        assert!(render_human(&readback).contains("occurrence_contexts:"));
        contextual["occurrence_contexts"]
            .as_array_mut()
            .unwrap()
            .reverse();
        let reordered = PreparedEpisode::parse(&contextual).unwrap();
        assert_eq!(
            write_proof_fixture(&reordered).1["source"],
            readback["source"]
        );
        for context in [None, Some(json!(null)), Some(json!([])), Some(contexts())] {
            let mut bad = readback.clone();
            if let Some(context) = context {
                bad["occurrence_contexts"] = context;
            } else {
                bad.as_object_mut().unwrap().remove("occurrence_contexts");
            }
            assert!(prepared.verify_readback_json(&receipt, &bad).is_err());
        }
        let mut bad = readback.clone();
        bad["occurrence_contexts"][0]["label"] = json!("Changed label");
        assert!(prepared.verify_readback_json(&receipt, &bad).is_err());
        let (old_receipt, mut invented) = write_proof_fixture(&old);
        invented["occurrence_contexts"] = readback["occurrence_contexts"].clone();
        assert!(old.verify_readback_json(&old_receipt, &invented).is_err());
        invented["occurrence_contexts"] = Value::Null;
        assert!(old.verify_readback_json(&old_receipt, &invented).is_err());
    }
}

#[test]
fn occurrence_contexts_reserve_real_aggregate_metadata_without_truncation() {
    let mut value = append();
    value["summary"] = json!("\"".repeat(2048));
    value["source"] = json!({
        "namespace":"n".repeat(64), "key":"\"".repeat(512),
        "reference":"\"".repeat(2048), "session":"\"".repeat(512),
        "revision":"\"".repeat(256)
    });
    value["thread"] = json!("\"".repeat(128));
    let mut last_accepted = None;
    for padding in 0..=254 {
        value["tags"] = json!(
            (0..32)
                .map(|index| format!("{index:02}{}", "\"".repeat(padding)))
                .collect::<Vec<_>>()
        );
        if PreparedEpisode::parse(&value).is_ok() {
            last_accepted = Some(value.clone());
        } else {
            break;
        }
    }
    let mut contextual = last_accepted.expect("legal metadata fixture below aggregate budget");
    contextual["occurrence_contexts"] = json!([{"namespace":"Place", "key":"x".repeat(980)}]);
    rejects(&contextual); // Contexts count toward the unchanged 16 KiB metadata budget.
    contextual["tags"] = json!([]);
    let prepared = PreparedEpisode::parse(&contextual).unwrap();
    let (_, readback) = write_proof_fixture(&prepared);
    assert_eq!(
        readback["occurrence_contexts"],
        contextual["occurrence_contexts"]
    );
    let mut metadata = readback;
    metadata["body"] = json!("");
    assert!(serde_json::to_vec(&metadata).unwrap().len() <= MAX_EPISODE_METADATA_BYTES);
}
