//! Canonical inventory and native touchstones share bounded application reads.
use crate::{AnyErr, db_prop, expected_db_id_prop};
use mneme_app::list::PreparedList;
use serde_json::{Value, json};

pub(crate) fn prepare_list(raw: &Value) -> Result<PreparedList, AnyErr> {
    let mut input = raw
        .as_object()
        .ok_or("list arguments must be an object")?
        .clone();
    if let Some(db) = input.get("db") {
        let db = db.as_str().ok_or("list db must be a string")?;
        if db.is_empty() || db.len() > 256 || db.trim() != db || db.chars().any(char::is_control) {
            return Err("list db must be 1..=256 UTF-8 bytes, trimmed, without controls".into());
        }
    }
    crate::optional_expected_db_id(raw)?;
    input.remove("db");
    input.remove("expected_db_id");
    Ok(PreparedList::parse(&Value::Object(input))?)
}

pub(crate) fn list_tool_schema() -> Value {
    let mut schema = mneme_app::list::list_input_schema();
    schema["properties"]["db"] = db_prop();
    schema["properties"]["expected_db_id"] = expected_db_id_prop();
    for branch in schema["oneOf"].as_array_mut().expect("list branches") {
        branch["properties"]["db"] = db_prop();
        branch["properties"]["expected_db_id"] = expected_db_id_prop();
    }
    json!({
        "name":"list",
        "description":"Browse bounded indexed nodes (default kind nodes), semantic tag vocabulary (kind tags), or native touchstone notes (kind touchstones). Nodes/tags default to status all. Tags accept an exact optional prefix, return bounded examples and exact/lower_bound/unavailable count status, and exclude episodes. Pass opaque next_cursor back as after without changing kind/status/tag/prefix; empty filtered pages may still continue. Pages are not transaction snapshots and report work and partial coverage. Nodes are canonical-ID ascending and include exact historical episode editions. db selects one database; an omitted db uses the configured read default, never a private global fallback. Use get for complete records. No body reads, learning or model calls.",
        "inputSchema":schema,
    })
}

#[cfg(test)]
#[path = "touchstone_tests.rs"]
mod tests;
