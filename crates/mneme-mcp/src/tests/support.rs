//! Shared empty servers, schema evaluator, and the single public-request inventory.

use crate::*;

pub(super) fn empty_server(allow_direct_feedback: bool) -> Server {
    Server {
        registry: Registry {
            activity: crate::activity::ActivityRing::default(),
            dbs: BTreeMap::new(),
        },
        sessions: std::sync::Arc::new(Mutex::new(SessionState::default())),
        cold_work: ColdWorkGate::new(Duration::ZERO),
        capability: CapabilityPolicy::new(CapabilityProfile::Operator, allow_direct_feedback),
        library: None,
    }
}

pub(crate) fn empty_profile_server(profile: CapabilityProfile) -> Server {
    Server {
        registry: Registry {
            activity: crate::activity::ActivityRing::default(),
            dbs: BTreeMap::new(),
        },
        sessions: std::sync::Arc::new(Mutex::new(SessionState::default())),
        cold_work: ColdWorkGate::new(Duration::ZERO),
        capability: CapabilityPolicy::new(profile, false),
        library: None,
    }
}

/// Small evaluator for the JSON-Schema subset emitted by this binary. It is
/// intentionally test-only: the runtime parser remains the authority, while
/// this proves tools/list describes the same accepted conditional shapes.
pub(crate) fn schema_accepts(schema: &Value, instance: &Value) -> bool {
    if let Some(expected) = schema.get("const")
        && instance != expected
    {
        return false;
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(instance)
    {
        return false;
    }
    if let Some(all) = schema.get("allOf").and_then(Value::as_array)
        && !all.iter().all(|schema| schema_accepts(schema, instance))
    {
        return false;
    }
    if let Some(any) = schema.get("anyOf").and_then(Value::as_array)
        && !any.iter().any(|schema| schema_accepts(schema, instance))
    {
        return false;
    }
    if let Some(one) = schema.get("oneOf").and_then(Value::as_array)
        && one
            .iter()
            .filter(|schema| schema_accepts(schema, instance))
            .count()
            != 1
    {
        return false;
    }
    if let Some(negated) = schema.get("not")
        && schema_accepts(negated, instance)
    {
        return false;
    }
    if let Some(condition) = schema.get("if") {
        let branch = if schema_accepts(condition, instance) {
            schema.get("then")
        } else {
            schema.get("else")
        };
        if branch.is_some_and(|branch| !schema_accepts(branch, instance)) {
            return false;
        }
    }

    match schema.get("type").and_then(Value::as_str) {
        Some("null") if !instance.is_null() => return false,
        Some("object") if !instance.is_object() => return false,
        Some("array") if !instance.is_array() => return false,
        Some("string") if !instance.is_string() => return false,
        Some("boolean") if !instance.is_boolean() => return false,
        Some("integer") if instance.as_u64().is_none() => return false,
        Some("number") if instance.as_f64().is_none_or(|value| !value.is_finite()) => {
            return false;
        }
        Some("null" | "object" | "array" | "string" | "boolean" | "integer" | "number") | None => {}
        Some(_) => return false,
    }

    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
        && instance.as_f64().is_none_or(|value| value < minimum)
    {
        return false;
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
        && instance.as_f64().is_none_or(|value| value > maximum)
    {
        return false;
    }
    if let Some(text) = instance.as_str() {
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
        // This literal is the new routing guard's advertised grammar. Keep
        // its test evaluator independent of the runtime ULID decoder.
        if schema.get("pattern").and_then(Value::as_str) == Some("^[0-7][0-9A-HJKMNP-TV-Z]{25}$")
            && !(text.len() == 26
                && matches!(text.as_bytes()[0], b'0'..=b'7')
                && text
                    .bytes()
                    .all(|byte| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&byte)))
        {
            return false;
        }
        if schema.get("pattern").and_then(Value::as_str) == Some("^[0-9a-f]{64}$")
            && !(text.len() == 64
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        {
            return false;
        }
    }
    if let Some(maximum) = schema.get("maxItems").and_then(Value::as_u64)
        && instance
            .as_array()
            .is_none_or(|values| values.len() as u64 > maximum)
    {
        return false;
    }
    if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64)
        && instance
            .as_array()
            .is_none_or(|values| (values.len() as u64) < minimum)
    {
        return false;
    }
    if let (Some(items), Some(values)) = (schema.get("items"), instance.as_array())
        && !values.iter().all(|value| schema_accepts(items, value))
    {
        return false;
    }

    let Some(object) = instance.as_object() else {
        return true;
    };
    if let Some(dependencies) = schema.get("dependentRequired").and_then(Value::as_object) {
        for (key, required) in dependencies {
            if object.contains_key(key)
                && required.as_array().is_none_or(|fields| {
                    fields.iter().any(|field| {
                        field
                            .as_str()
                            .is_none_or(|field| !object.contains_key(field))
                    })
                })
            {
                return false;
            }
        }
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array)
        && required
            .iter()
            .filter_map(Value::as_str)
            .any(|key| !object.contains_key(key))
    {
        return false;
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        if schema.get("additionalProperties") == Some(&Value::Bool(false))
            && object.keys().any(|key| !properties.contains_key(key))
        {
            return false;
        }
        for (key, value) in object {
            if let Some(property) = properties.get(key)
                && !schema_accepts(property, value)
            {
                return false;
            }
        }
    }
    true
}

pub(crate) fn tool_input_schema<'a>(schemas: &'a [Value], name: &str) -> &'a Value {
    &schemas
        .iter()
        .find(|schema| schema["name"] == name)
        .unwrap_or_else(|| panic!("missing schema {name:?}"))["inputSchema"]
}

pub(super) fn capability_request_cases() -> Vec<(&'static str, Value, CapabilityClass)> {
    let id = ulid::Ulid::new().to_string();
    let other = ulid::Ulid::new().to_string();
    let mut cases = vec![
        (
            "edit_summary",
            json!({"db":"missing","expected_db_id":other,"id":id,"expected_snapshot_sha256":"a".repeat(64),"summary":"replacement"}),
            CapabilityClass::Operator,
        ),
        (
            "edit_body",
            json!({"db":"missing","expected_db_id":other,"id":id,"expected_body_revision":"a".repeat(64),"body":""}),
            CapabilityClass::Operator,
        ),
        (
            "retag",
            json!({"db":"missing","expected_db_id":other,"id":id,"expected_tags":[],"tags":["possibility"]}),
            CapabilityClass::Curator,
        ),
        (
            "retag",
            json!({"db":"missing","expected_db_id":other,"id":id,"expected_tags":["core"],"tags":[]}),
            CapabilityClass::Operator,
        ),
        (
            "retag",
            json!({"db":"missing","expected_db_id":other,"id":id,"expected_tags":[],"tags":["core"]}),
            CapabilityClass::Operator,
        ),
        ("databases", json!({}), CapabilityClass::ReadOnly),
        (
            "graph",
            json!({"db":"missing", "action":"topology"}),
            CapabilityClass::ReadOnly,
        ),
        (
            "graph",
            json!({"db":"missing", "action":"summaries", "ids":[id]}),
            CapabilityClass::ReadOnly,
        ),
        ("activity", json!({}), CapabilityClass::ReadOnly),
        (
            "database_control",
            json!({ "action": "status" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "database_control",
            json!({ "db": "missing", "action": "release" }),
            CapabilityClass::Operator,
        ),
        (
            "database_control",
            json!({ "db": "missing", "action": "resume" }),
            CapabilityClass::Operator,
        ),
        (
            "snapshot_create",
            json!({ "db": "missing" }),
            CapabilityClass::Operator,
        ),
        (
            "status",
            json!({ "db": "missing" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "decay",
            json!({ "db": "missing" }),
            CapabilityClass::Operator,
        ),
        (
            "prune",
            json!({ "db": "missing" }),
            CapabilityClass::Operator,
        ),
        (
            "query",
            json!({ "db": "missing", "text": "q" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "recall_context",
            json!({ "db": "missing", "text": "q" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "recall",
            json!({ "db": "missing", "text": "q" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "get",
            json!({ "db": "missing", "id": id }),
            CapabilityClass::ReadOnly,
        ),
        (
            "neighbors",
            json!({ "db": "missing", "id": id }),
            CapabilityClass::ReadOnly,
        ),
        (
            "remote_edges",
            json!({ "db": "missing", "id": id }),
            CapabilityClass::ReadOnly,
        ),
        (
            "core",
            json!({ "db": "missing" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "ingest",
            json!({ "db": "missing", "summary": "candidate" }),
            CapabilityClass::Curator,
        ),
        (
            "ingest",
            json!({ "db": "missing", "summary": "core", "tags": ["core"] }),
            CapabilityClass::Operator,
        ),
        (
            "capture",
            json!({ "db": "missing", "source": { "namespace": "codex", "key": "claim-1", "reference": "codex://thread/1" }, "summary": "candidate" }),
            CapabilityClass::Curator,
        ),
        (
            "capture",
            json!({ "db": "missing", "source": { "namespace": "codex", "key": "claim-1", "reference": "codex://thread/1" }, "summary": "core", "tags": ["core"] }),
            CapabilityClass::Operator,
        ),
        (
            "feedback",
            json!({ "db": "missing", "to": id, "signal": "relevant" }),
            CapabilityClass::Operator,
        ),
        (
            "forget",
            json!({ "db": "missing", "id": id }),
            CapabilityClass::Operator,
        ),
        (
            "link",
            json!({ "db": "missing", "from": id, "to": other }),
            CapabilityClass::Curator,
        ),
        (
            "link",
            json!({ "db": "user", "from": id, "to": other, "to_db": "project" }),
            CapabilityClass::Curator,
        ),
        (
            "supersede",
            json!({ "db": "missing", "winner": id, "loser": other }),
            CapabilityClass::Operator,
        ),
        (
            "contradict",
            json!({ "db": "missing", "a": id, "b": other }),
            CapabilityClass::Curator,
        ),
        (
            "contradictions",
            json!({ "db": "missing" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "reconcile",
            json!({
                "db": "missing", "a": id, "b": other,
                "resolution": "context-dependent",
            }),
            CapabilityClass::Operator,
        ),
        (
            "merges",
            json!({ "db": "missing" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "merge",
            json!({ "db": "missing", "mode": "full", "winner": id, "loser": other }),
            CapabilityClass::Operator,
        ),
        (
            "merge",
            json!({ "db": "missing", "mode": "keep", "a": id, "b": other }),
            CapabilityClass::Operator,
        ),
        (
            "walk",
            json!({ "action": "start", "db": "missing", "start": id }),
            CapabilityClass::ReadOnly,
        ),
        (
            "walk",
            json!({ "action": "look", "session": "missing" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "walk",
            json!({ "action": "edges", "session": "missing" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "walk",
            json!({ "action": "body", "session": "missing" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "walk",
            json!({ "action": "go", "session": "missing", "to": "0" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "walk",
            json!({ "action": "back", "session": "missing" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "walk",
            json!({ "action": "done", "session": "missing" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "walk",
            json!({ "action": "abort", "session": "missing" }),
            CapabilityClass::ReadOnly,
        ),
        (
            "reflect",
            json!({ "db": "missing", "receipts": ["receipt"], "used": [] }),
            CapabilityClass::ReceiptGrounded,
        ),
    ];
    cases.push((
        "list",
        json!({"db":"missing","kind":"touchstones"}),
        CapabilityClass::ReadOnly,
    ));
    cases.extend(episode::tests::request_cases());
    cases.extend(save::tests::request_cases());
    cases.extend(concern::tests::request_cases());

    // Reads retain the documented default database selector. Keep these shapes
    // beside their explicit-db counterparts, rather than in a second catalog.
    let default_database_cases = cases
        .iter()
        .filter(|(name, arguments, _)| {
            matches!(
                *name,
                "status"
                    | "query"
                    | "recall_context"
                    | "recall"
                    | "get"
                    | "neighbors"
                    | "remote_edges"
                    | "core"
                    | "contradictions"
                    | "merges"
            ) || (*name == "walk" && arguments["action"] == "start")
        })
        .map(|(name, arguments, required)| {
            let mut arguments = arguments.clone();
            arguments.as_object_mut().unwrap().remove("db");
            (*name, arguments, *required)
        })
        .collect::<Vec<_>>();
    cases.extend(default_database_cases);
    cases
}
