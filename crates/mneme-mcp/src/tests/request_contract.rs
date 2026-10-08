//! Schema/parser agreement and fail-closed validation before database checkout.

use super::support::{capability_request_cases, empty_server, schema_accepts, tool_input_schema};
use crate::*;

fn contains_schema_keyword(value: &Value, keyword: &str) -> bool {
    match value {
        Value::Object(fields) => {
            fields.contains_key(keyword)
                || fields
                    .values()
                    .any(|value| contains_schema_keyword(value, keyword))
        }
        Value::Array(values) => values
            .iter()
            .any(|value| contains_schema_keyword(value, keyword)),
        _ => false,
    }
}

#[tokio::test]
async fn partial_merge_is_absent_from_schema_and_rejected_before_checkout() {
    let server = empty_server(false);
    let listed = server.dispatch("tools/list", json!({})).await.unwrap();
    let merge = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|schema| schema["name"] == "merge")
        .expect("merge schema");
    assert_eq!(
        merge["inputSchema"]["properties"]["mode"]["enum"],
        json!(["full", "keep"])
    );
    let properties = merge["inputSchema"]["properties"].as_object().unwrap();
    assert!(!properties.contains_key("summary"));
    assert!(!properties.contains_key("body"));
    assert!(!properties.contains_key("tags"));

    let rejected = server
        .dispatch(
            "tools/call",
            json!({
                "name": "merge",
                "arguments": { "db": "user", "mode": "partial" },
            }),
        )
        .await
        .unwrap();
    assert_eq!(rejected["isError"], true);
    assert!(
        rejected["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("partial merge is not exposed"))
    );
}

#[test]
fn schemas_close_arguments_and_require_database_for_every_mutator() {
    let schemas = tool_schemas(CapabilityPolicy::operator_with_direct_feedback());
    for schema in &schemas {
        assert_eq!(
            schema["inputSchema"]["additionalProperties"], false,
            "{} must reject unknown arguments",
            schema["name"]
        );
    }

    for name in [
        "decay",
        "prune",
        "ingest",
        "capture",
        "save",
        "feedback",
        "forget",
        "link",
        "supersede",
        "contradict",
        "reconcile",
        "merge",
        "reflect",
    ] {
        let schema = schemas
            .iter()
            .find(|schema| schema["name"] == name)
            .unwrap_or_else(|| panic!("missing schema for {name}"));
        assert!(
            schema["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("db")),
            "{name} must require an explicit database"
        );
        assert!(mutation_requires_explicit_db(name));
    }

    // Concern is a mixed read/write union, not a flat mutator schema.
    // Its list keeps read selector convenience; both write arms require
    // explicit owner selection and the canonical database identity guard.
    let concern_schema = tool_input_schema(&schemas, "concern");
    assert!(
        !concern_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("db"))
    );
    assert!(!mutation_requires_explicit_db("concern"));
    for (name, arguments, authority) in concern::tests::request_cases() {
        let kind = ValidatedToolArguments::parse(name, &arguments)
            .unwrap()
            .kind();
        if authority == CapabilityClass::ReadOnly {
            let mut implicit = arguments.clone();
            implicit.as_object_mut().unwrap().remove("db");
            assert!(schema_accepts(concern_schema, &implicit));
            assert!(ValidatedToolArguments::parse(name, &implicit).is_ok());
            assert!(!kind.requires_explicit_db());
        } else {
            assert!(kind.requires_explicit_db());
            assert_eq!(kind.capability_class(), CapabilityClass::Curator);
            assert!(schema_accepts(concern_schema, &arguments));
            for field in ["db", "expected_db_id"] {
                let mut missing = arguments.clone();
                missing.as_object_mut().unwrap().remove(field);
                assert!(
                    !schema_accepts(concern_schema, &missing),
                    "{} must require {field}",
                    arguments["action"]
                );
                assert!(ValidatedToolArguments::parse(name, &missing).is_err());
            }
        }
    }

    let database_control = schemas
        .iter()
        .find(|schema| schema["name"] == "database_control")
        .expect("database_control schema");
    assert!(
        !database_control["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("db")),
        "read-only database_control status keeps the safe default"
    );
    assert_eq!(
        database_control["inputSchema"]["allOf"][0]["then"]["required"],
        json!(["db"])
    );
    assert!(mutation_requires_explicit_db("database_control"));

    for name in [
        "activity",
        "databases",
        "status",
        "query",
        "recall_context",
        "recall",
        "get",
        "list",
        "graph",
        "episode",
        "neighbors",
        "remote_edges",
        "core",
        "contradictions",
        "merges",
        "walk",
    ] {
        assert!(
            !mutation_requires_explicit_db(name),
            "read-only tool {name} should retain safe selector convenience"
        );
    }

    let link = schemas
        .iter()
        .find(|schema| schema["name"] == "link")
        .expect("link schema");
    let link_properties = &link["inputSchema"]["properties"];
    assert_eq!(link_properties["weight"]["minimum"], 0);
    assert_eq!(link_properties["weight"]["maximum"], 1);
    assert_eq!(link_properties["anchor_start"]["minimum"], 0);
    assert_eq!(link_properties["anchor_start"]["maximum"], u32::MAX);
    assert_eq!(link_properties["anchor_end"]["minimum"], 0);
    assert_eq!(link_properties["anchor_end"]["maximum"], u32::MAX);
}

#[tokio::test]
async fn mutation_dispatch_rejects_omitted_and_misspelled_database_selectors() {
    let server = empty_server(false);
    let id = ulid::Ulid::new().to_string();

    for (arguments, expected) in [
        (json!({ "id": id }), "missing required argument `db`"),
        (
            json!({ "bd": "project", "id": id }),
            "unknown argument `bd`",
        ),
        (
            json!({ "db": null, "id": id }),
            "argument `db` must be a string",
        ),
        (
            json!({ "db": 7, "id": id }),
            "argument `db` must be a string",
        ),
    ] {
        let response = server
            .dispatch(
                "tools/call",
                json!({ "name": "forget", "arguments": arguments }),
            )
            .await
            .unwrap();
        assert_eq!(response["isError"], true);
        let error = response["content"][0]["text"].as_str().unwrap();
        assert!(error.contains(expected), "{error}");
        assert!(
            !error.contains("unknown database \"user\""),
            "selector validation must run before any implicit user checkout: {error}"
        );
    }

    for db in ["user", "project"] {
        let response = server
            .dispatch(
                "tools/call",
                json!({
                    "name": "forget",
                    "arguments": { "db": db, "id": id },
                }),
            )
            .await
            .unwrap();
        let error = response["content"][0]["text"].as_str().unwrap();
        assert!(
            error.contains(&format!("unknown database {db:?}")),
            "{error}"
        );
    }

    let bad_target = server
        .dispatch(
            "tools/call",
            json!({
                "name": "link",
                "arguments": {
                    "db": "user",
                    "from": id,
                    "to": ulid::Ulid::new().to_string(),
                    "to_db": null,
                },
            }),
        )
        .await
        .unwrap();
    assert_eq!(bad_target["isError"], true);
    assert!(
        bad_target["content"][0]["text"]
            .as_str()
            .is_some_and(|error| error.contains("argument `to_db` must be a string")),
        "{bad_target}"
    );
}

#[tokio::test]
async fn typed_request_boundary_rejects_invalid_lossy_integer_and_irrelevant_values_before_checkout()
 {
    let registry = Registry {
        activity: crate::activity::ActivityRing::default(),
        dbs: BTreeMap::new(),
    };
    let sessions = std::sync::Arc::new(Mutex::new(SessionState::default()));
    let gate = ColdWorkGate::new(Duration::ZERO);
    let id = ulid::Ulid::new().to_string();
    let other = ulid::Ulid::new().to_string();
    let too_large_anchor = u64::from(u32::MAX) + 1;

    let rejected = vec![
        (
            "ingest",
            json!({ "db": "missing", "summary": "s", "body": null }),
            "argument `body` must be a string",
        ),
        (
            "ingest",
            json!({ "db": "missing", "summary": "s", "active": "false" }),
            "unknown argument `active`",
        ),
        (
            "ingest",
            json!({ "db": "missing", "summary": "s", "stability": "0.5" }),
            "argument `stability` must be a finite number",
        ),
        (
            "ingest",
            json!({ "db": "missing", "summary": "s", "confidence": 1.01 }),
            "confidence must be finite and between 0 and 1",
        ),
        (
            "ingest",
            json!({ "db": "missing", "summary": "s", "confidence": 1e-50 }),
            "confidence is too close to a unit-interval endpoint",
        ),
        (
            "feedback",
            json!({ "db": "missing", "from": null, "to": id, "signal": "relevant" }),
            "argument `from` must be a string",
        ),
        (
            "recall",
            json!({ "db": "missing", "text": "q", "expand_top": "3" }),
            "argument `expand_top` must be a non-negative integer",
        ),
        (
            "recall",
            json!({ "db": "missing", "text": "q", "neighbors_each": 17 }),
            "argument `neighbors_each` must be in 1..=16",
        ),
        (
            "get",
            json!({ "db": "missing", "id": id, "body_offset": 1 }),
            "argument `body_offset` is not valid for get without body:true",
        ),
        (
            "get",
            json!({ "db": "missing", "id": id, "body": true, "max_body_bytes": null }),
            "argument `max_body_bytes` must be a non-negative integer",
        ),
        (
            "remote_edges",
            json!({ "db": "missing", "id": id, "resolve": 1 }),
            "argument `resolve` must be a boolean",
        ),
        (
            "remote_edges",
            json!({ "db": "missing", "id": id, "after": null }),
            "argument `after` must be an object",
        ),
        (
            "link",
            json!({ "db": "missing", "from": id, "to": other, "weight": 1.01 }),
            "weight must be finite and between 0 and 1",
        ),
        (
            "link",
            json!({
                "db": "missing", "from": id, "to": other,
                "anchor_start": too_large_anchor, "anchor_end": too_large_anchor,
            }),
            "must be in 0..=4294967295",
        ),
        (
            "link",
            json!({ "db": "missing", "from": id, "to": other, "anchor_start": 4 }),
            "anchor_start and anchor_end must be supplied together",
        ),
        (
            "link",
            json!({
                "db": "missing", "from": id, "to": other,
                "anchor_start": 9, "anchor_end": 4,
            }),
            "anchor_start must not exceed anchor_end",
        ),
        (
            "link",
            json!({
                "db": "user", "from": id, "to": other, "to_db": "missing",
                "kind": "associative",
            }),
            "argument `kind` is not valid for a cross-database link",
        ),
        (
            "merge",
            json!({ "db": "missing", "mode": "full", "a": id, "b": other }),
            "argument `a` is not valid for merge mode full",
        ),
        (
            "merge",
            json!({ "db": "missing", "mode": "keep", "winner": id, "loser": other }),
            "argument `winner` is not valid for merge mode keep",
        ),
        (
            "walk",
            json!({ "action": "start", "db": "missing", "start": id, "budget": 65 }),
            "argument `budget` must be in 1..=64",
        ),
        (
            "walk",
            json!({ "action": "start", "db": "missing", "start": id, "session": "ignored" }),
            "argument `session` is not valid for walk action start",
        ),
        (
            "walk",
            json!({ "action": "look", "session": "missing", "budget": 2 }),
            "argument `budget` is not valid for walk action look",
        ),
        (
            "walk",
            json!({ "action": "go", "session": "missing", "to": "0", "body_offset": 0 }),
            "argument `body_offset` is not valid for walk action go",
        ),
        (
            "walk",
            json!({ "action": "go", "session": "missing", "to": "not-an-index-or-id" }),
            "argument `to` must be a neighbor index or node id",
        ),
        (
            "reflect",
            json!({ "db": "missing", "receipts": ["receipt"], "used": [7] }),
            "argument `used[0]` must be a string",
        ),
        (
            "reflect",
            json!({ "db": "missing", "receipts": ["same", "same"], "used": [] }),
            "duplicate walk receipt",
        ),
        (
            "forget",
            json!({ "db": "missing", "id": "not-an-id" }),
            "invalid node id",
        ),
    ];

    for (name, arguments, expected) in rejected {
        let error = call_tool(&registry, &sessions, &gate, name, &arguments)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{name}: {error}");
        assert!(
            !error.contains("unknown database"),
            "{name} reached checkout before rejecting malformed arguments: {error}"
        );
    }
}

#[tokio::test]
async fn remote_cursor_binding_and_weight_fail_before_database_checkout() {
    let registry = Registry {
        activity: crate::activity::ActivityRing::default(),
        dbs: BTreeMap::new(),
    };
    let sessions = std::sync::Arc::new(Mutex::new(SessionState::default()));
    let gate = ColdWorkGate::new(Duration::ZERO);
    let source = NodeId(ulid::Ulid::new());
    let foreign_source = NodeId(ulid::Ulid::new());
    let target_db = ulid::Ulid::new();
    let target = NodeId(ulid::Ulid::new());

    let foreign_cursor = RemoteEdgeCursor::from_edge(&mneme_core::RemoteEdge::new(
        foreign_source,
        target_db,
        target,
        0.5,
    ));
    let error = call_tool(
        &registry,
        &sessions,
        &gate,
        "remote_edges",
        &json!({
            "db": "missing",
            "id": source.0.to_string(),
            "after": foreign_cursor,
        }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("different source"), "{error}");
    assert!(!error.contains("unknown database"), "{error}");

    let cursor =
        RemoteEdgeCursor::from_edge(&mneme_core::RemoteEdge::new(source, target_db, target, 0.5));
    let mut noncanonical = serde_json::to_value(cursor).unwrap();
    noncanonical["weight_bits"] = json!(f32::NAN.to_bits());
    let error = call_tool(
        &registry,
        &sessions,
        &gate,
        "remote_edges",
        &json!({
            "db": "missing",
            "id": source.0.to_string(),
            "after": noncanonical,
        }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("non-canonical weight"), "{error}");
    assert!(!error.contains("unknown database"), "{error}");

    let error = call_tool(
        &registry,
        &sessions,
        &gate,
        "remote_edges",
        &json!({
            "db": "missing",
            "id": source.0.to_string(),
            "after": cursor,
        }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("unknown database \"missing\""), "{error}");
}

#[test]
fn typed_request_boundary_preserves_valid_defaults_and_variant_shapes() {
    let id = ulid::Ulid::new().to_string();
    let other = ulid::Ulid::new().to_string();
    for (name, arguments) in [
        (
            "recall",
            json!({ "text": "q", "expand_top": 1, "neighbors_each": 16 }),
        ),
        (
            "ingest",
            json!({
                "db": "project", "summary": "s", "stability": 0.0,
                "confidence": 1.0,
            }),
        ),
        (
            "link",
            json!({
                "db": "project", "from": id, "to": other,
                "weight": 0.0, "anchor_start": 0, "anchor_end": u32::MAX,
            }),
        ),
        (
            "link",
            json!({
                "db": "user", "from": id, "to": other,
                "to_db": "project", "weight": 1.0,
            }),
        ),
        (
            "walk",
            json!({ "action": "start", "start": id, "budget": 1, "query": "" }),
        ),
        (
            "walk",
            json!({ "action": "body", "session": "token", "body_offset": 0, "max_body_bytes": 1 }),
        ),
        (
            "walk",
            json!({ "action": "go", "session": "token", "to": "0" }),
        ),
        (
            "merge",
            json!({ "db": "project", "mode": "full", "winner": id, "loser": other }),
        ),
        (
            "merge",
            json!({ "db": "project", "mode": "keep", "a": id, "b": other }),
        ),
    ] {
        ValidatedToolArguments::parse(name, &arguments)
            .unwrap_or_else(|error| panic!("{name} valid request rejected: {error}"));
    }
}

#[test]
fn typed_request_classification_retains_every_conditional_variant() {
    let id = ulid::Ulid::new().to_string();
    let other = ulid::Ulid::new().to_string();
    let cases = [
        (
            "database_control",
            json!({ "action": "status" }),
            ToolRequestKind::DatabaseControl(DatabaseControlRequest::Status),
        ),
        (
            "database_control",
            json!({ "db": "project", "action": "release" }),
            ToolRequestKind::DatabaseControl(DatabaseControlRequest::Release),
        ),
        (
            "database_control",
            json!({ "db": "project", "action": "resume" }),
            ToolRequestKind::DatabaseControl(DatabaseControlRequest::Resume),
        ),
        (
            "link",
            json!({ "db": "project", "from": id, "to": other }),
            ToolRequestKind::Link(LinkRequest::Local),
        ),
        (
            "link",
            json!({ "db": "user", "from": id, "to": other, "to_db": "project" }),
            ToolRequestKind::Link(LinkRequest::Remote),
        ),
        (
            "merge",
            json!({ "db": "project", "mode": "full", "winner": id, "loser": other }),
            ToolRequestKind::Merge(MergeRequest::Full),
        ),
        (
            "merge",
            json!({ "db": "project", "mode": "keep", "a": id, "b": other }),
            ToolRequestKind::Merge(MergeRequest::Keep),
        ),
        (
            "walk",
            json!({ "action": "start", "start": id }),
            ToolRequestKind::Walk(WalkRequest::Start),
        ),
        (
            "walk",
            json!({ "action": "look", "session": "token" }),
            ToolRequestKind::Walk(WalkRequest::Look),
        ),
        (
            "walk",
            json!({ "action": "edges", "session": "token" }),
            ToolRequestKind::Walk(WalkRequest::Edges),
        ),
        (
            "walk",
            json!({ "action": "body", "session": "token" }),
            ToolRequestKind::Walk(WalkRequest::Body),
        ),
        (
            "walk",
            json!({ "action": "go", "session": "token", "to": "0" }),
            ToolRequestKind::Walk(WalkRequest::Go),
        ),
        (
            "walk",
            json!({ "action": "back", "session": "token" }),
            ToolRequestKind::Walk(WalkRequest::Back),
        ),
        (
            "walk",
            json!({ "action": "done", "session": "token" }),
            ToolRequestKind::Walk(WalkRequest::Done),
        ),
        (
            "walk",
            json!({ "action": "abort", "session": "token" }),
            ToolRequestKind::Walk(WalkRequest::Abort),
        ),
    ];

    for (name, arguments, expected) in cases {
        let parsed = ValidatedToolArguments::parse(name, &arguments).unwrap();
        assert_eq!(parsed.kind(), expected, "{name}: {arguments}");
        assert_eq!(parsed.kind().tool_name(), name);
    }
}

#[test]
fn conditional_tool_schemas_are_equivalent_to_the_runtime_parser() {
    let schemas = tool_schemas(CapabilityPolicy::operator_with_direct_feedback());
    let id = ulid::Ulid::new().to_string();
    let other = ulid::Ulid::new().to_string();
    let cases = vec![
        ("get", json!({ "id": id }), true),
        ("get", json!({ "id": id, "body": false }), true),
        (
            "get",
            json!({ "id": id, "body": true, "body_offset": 4, "max_body_bytes": 8 }),
            true,
        ),
        ("get", json!({ "id": id, "body_offset": 4 }), false),
        (
            "get",
            json!({ "id": id, "body": false, "max_body_bytes": 8 }),
            false,
        ),
        (
            "link",
            json!({ "db": "project", "from": id, "to": other }),
            true,
        ),
        (
            "link",
            json!({
                "db": "project", "from": id, "to": other,
                "kind": "derived_from", "anchor_start": 1, "anchor_end": 2,
            }),
            true,
        ),
        (
            "link",
            json!({ "db": "user", "from": id, "to": other, "to_db": "project" }),
            true,
        ),
        (
            "link",
            json!({ "db": "project", "from": id, "to": other, "anchor_start": 1 }),
            false,
        ),
        (
            "link",
            json!({
                "db": "user", "from": id, "to": other, "to_db": "project",
                "kind": "associative",
            }),
            false,
        ),
        (
            "merge",
            json!({ "db": "project", "mode": "full", "winner": id, "loser": other }),
            true,
        ),
        (
            "merge",
            json!({ "db": "project", "mode": "keep", "a": id, "b": other }),
            true,
        ),
        (
            "merge",
            json!({
                "db": "project", "mode": "full", "winner": id, "loser": other,
                "a": id,
            }),
            false,
        ),
        (
            "merge",
            json!({ "db": "project", "mode": "keep", "winner": id, "a": id, "b": other }),
            false,
        ),
        (
            "walk",
            json!({ "action": "start", "db": "project", "start": id, "budget": 1 }),
            true,
        ),
        (
            "walk",
            json!({ "action": "look", "session": "token" }),
            true,
        ),
        (
            "walk",
            json!({ "action": "edges", "session": "token" }),
            true,
        ),
        (
            "walk",
            json!({ "action": "body", "session": "token", "body_offset": 0 }),
            true,
        ),
        (
            "walk",
            json!({ "action": "go", "session": "token", "to": "0" }),
            true,
        ),
        (
            "walk",
            json!({ "action": "back", "session": "token" }),
            true,
        ),
        (
            "walk",
            json!({ "action": "done", "session": "token" }),
            true,
        ),
        (
            "walk",
            json!({ "action": "abort", "session": "token" }),
            true,
        ),
        (
            "walk",
            json!({ "action": "start", "start": id, "session": "token" }),
            false,
        ),
        (
            "walk",
            json!({ "action": "look", "db": "project", "session": "token" }),
            false,
        ),
        (
            "walk",
            json!({ "action": "body", "session": "token", "to": "0" }),
            false,
        ),
        (
            "walk",
            json!({ "action": "go", "session": "token", "to": "0", "body_offset": 0 }),
            false,
        ),
    ];

    for (name, arguments, expected) in cases {
        let schema_result = schema_accepts(tool_input_schema(&schemas, name), &arguments);
        let parser_result = ValidatedToolArguments::parse(name, &arguments).is_ok();
        assert_eq!(schema_result, expected, "schema {name}: {arguments}");
        assert_eq!(parser_result, expected, "parser {name}: {arguments}");
    }

    let get = tool_input_schema(&schemas, "get");
    assert!(get.get("allOf").is_some());
    for name in ["link", "merge", "walk"] {
        let schema = tool_input_schema(&schemas, name);
        assert!(schema.get("oneOf").is_some(), "{name}");
        assert!(contains_schema_keyword(schema, "not"), "{name}");
    }
}

#[test]
fn canonical_enum_spellings_are_identical_in_schema_and_parser() {
    let schemas = tool_schemas(CapabilityPolicy::operator_with_direct_feedback());
    let id = ulid::Ulid::new().to_string();
    let other = ulid::Ulid::new().to_string();
    let cases = vec![
        (
            "feedback",
            json!({ "db": "project", "to": id, "signal": "relevant" }),
            true,
        ),
        (
            "feedback",
            json!({ "db": "project", "to": id, "signal": "not-new" }),
            true,
        ),
        (
            "feedback",
            json!({ "db": "project", "to": id, "signal": "irrelevant" }),
            true,
        ),
        (
            "feedback",
            json!({ "db": "project", "to": id, "signal": "rel" }),
            false,
        ),
        (
            "feedback",
            json!({ "db": "project", "to": id, "signal": "Relevant" }),
            false,
        ),
        (
            "link",
            json!({ "db": "project", "from": id, "to": other, "kind": "derived_from" }),
            true,
        ),
        (
            "link",
            json!({ "db": "project", "from": id, "to": other, "kind": "derivedfrom" }),
            false,
        ),
        (
            "reconcile",
            json!({
                "db": "project", "a": id, "b": other,
                "resolution": "context-dependent",
            }),
            true,
        ),
        (
            "reconcile",
            json!({ "db": "project", "a": id, "b": other, "resolution": "context" }),
            false,
        ),
    ];

    for (name, arguments, expected) in cases {
        assert_eq!(
            schema_accepts(tool_input_schema(&schemas, name), &arguments),
            expected,
            "schema {name}: {arguments}"
        );
        assert_eq!(
            ValidatedToolArguments::parse(name, &arguments).is_ok(),
            expected,
            "parser {name}: {arguments}"
        );
    }
}

#[test]
fn expected_db_id_catalog_and_parser_cover_every_database_scoped_tool() {
    let schemas = tool_schemas(CapabilityPolicy::operator_with_direct_feedback());
    for (name, mut arguments, _) in capability_request_cases() {
        arguments["expected_db_id"] = json!("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let scoped = !matches!(name, "activity" | "databases");
        assert_eq!(
            schema_accepts(tool_input_schema(&schemas, name), &arguments),
            scoped,
            "{name}: {arguments}"
        );
        assert_eq!(
            ValidatedToolArguments::parse(name, &arguments).is_ok(),
            scoped,
            "{name}: {arguments}"
        );
        if scoped {
            for invalid in [
                Value::Null,
                json!(true),
                json!(7),
                json!("01arz3ndektsv4rrffq69g5fav"),
                json!("81ARZ3NDEKTSV4RRFFQ69G5FAV"),
            ] {
                arguments["expected_db_id"] = invalid;
                assert!(
                    ValidatedToolArguments::parse(name, &arguments).is_err(),
                    "{name}: {arguments}"
                );
                assert!(
                    !schema_accepts(tool_input_schema(&schemas, name), &arguments),
                    "{name}: {arguments}"
                );
            }
        }
    }
}

#[test]
fn ingest_limits_are_enforced_independently_of_transport() {
    assert!(bounded_required(&json!({ "summary": "ok" }), "summary", 2).is_ok());
    assert!(bounded_required(&json!({ "summary": "toolong" }), "summary", 2).is_err());
    assert!(bounded_required(&json!({ "summary": "   " }), "summary", 8).is_err());
    assert!(validate_tags(&["valid".into(), "two".into()]).is_ok());
    assert!(validate_tags(&["x".repeat(256)]).is_ok());
    assert!(validate_tags(&["x".repeat(257)]).is_err());
    assert!(validate_tags(&["é".repeat(128)]).is_ok());
    assert!(validate_tags(&["é".repeat(129)]).is_err());
    assert!(validate_tags(&[" duplicate".into()]).is_err());
    assert!(validate_tags(&["same".into(), "same".into()]).is_err());
    assert!(unit_interval("confidence", 1.0).is_ok());
    assert!(unit_interval("confidence", 1.1).is_err());
    assert_eq!(unit_interval("confidence", 0.1).unwrap(), 0.1_f32);
    assert!(unit_interval("confidence", 1e-50).is_err());
    assert!(unit_interval("confidence", 0.999_999_999_999_999_9).is_err());
}

#[test]
fn remote_edge_page_limits_fail_closed() {
    assert_eq!(remote_page_limit(&json!({})).unwrap(), 32);
    assert_eq!(
        remote_page_limit(&json!({ "limit": MAX_REMOTE_EDGE_PAGE_SIZE })).unwrap(),
        MAX_REMOTE_EDGE_PAGE_SIZE
    );
    assert!(remote_page_limit(&json!({ "limit": 0 })).is_err());
    assert!(remote_page_limit(&json!({ "limit": MAX_REMOTE_EDGE_PAGE_SIZE + 1 })).is_err());
    assert!(remote_page_limit(&json!({ "limit": "64" })).is_err());

    let remote = tool_schemas(CapabilityPolicy::operator())
        .into_iter()
        .find(|tool| tool["name"] == "remote_edges")
        .expect("remote_edges tool schema");
    assert_eq!(
        remote["inputSchema"]["properties"]["limit"]["maximum"],
        MAX_REMOTE_EDGE_PAGE_SIZE
    );
}
