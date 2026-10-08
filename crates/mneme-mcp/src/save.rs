//! Canonical SAVE envelope. Domain admission, identity and receipts stay shared;
//! this adapter only selects an explicit existing owner and classifies authority.
use crate::{AnyErr, CapabilityProfile, IngestRequest};
use mneme_app::save as shared;
use mneme_engine::Memory;
use serde_json::{Value, json};
use ulid::Ulid;

pub(crate) struct PreparedSave {
    inner: shared::PreparedSave,
}

impl PreparedSave {
    pub(crate) fn parse(raw: &Value) -> Result<Self, AnyErr> {
        if serde_json::to_vec(raw)?.len() > 1024 * 1024 {
            return Err("save input exceeds 1048576 UTF-8 bytes".into());
        }
        let object = raw.as_object().ok_or("save arguments must be an object")?;
        let db = object
            .get("db")
            .and_then(Value::as_str)
            .ok_or("save field `db` must be a string")?;
        if db.is_empty() || db.len() > 256 || db.trim() != db || db.chars().any(char::is_control) {
            return Err(
                "save field `db` must be 1..=256 UTF-8 bytes, trimmed, without controls".into(),
            );
        }
        crate::optional_expected_db_id(raw)?;
        let mut claim = object.clone();
        claim.remove("db");
        claim.remove("expected_db_id");
        // This nonce is not the JSON-RPC id. Keep the admitted object through
        // execution: reconstructing it later would turn one submission into two.
        let operation_id = if claim.contains_key("source") || claim.contains_key("operation_id") {
            String::new()
        } else {
            Ulid::new().to_string()
        };
        Ok(Self {
            inner: shared::PreparedSave::parse(&Value::Object(claim), &operation_id)?,
        })
    }

    pub(crate) fn authority(&self) -> IngestRequest {
        if self.inner.requires_operator() {
            IngestRequest::Core
        } else {
            IngestRequest::Ordinary
        }
    }

    pub(crate) async fn run(
        self,
        mem: &Memory,
        db_id: Ulid,
        origin_commit: Option<&str>,
    ) -> Result<Value, AnyErr> {
        self.inner.run(mem, db_id, origin_commit).await
    }
}

pub(crate) fn tool_schema(profile: CapabilityProfile) -> Value {
    let mut schema = shared::input_schema();
    schema["properties"]["db"] = crate::db_prop();
    schema["properties"]["expected_db_id"] = crate::expected_db_id_prop();
    schema["required"]
        .as_array_mut()
        .expect("shared SAVE required fields")
        .push(json!("db"));
    if profile == CapabilityProfile::Curator {
        schema["properties"]["tags"]["items"]["not"] = json!({"const":"core"});
    }
    json!({
        "name":"save",
        "description":"Save one note (default) or append one episode to an explicit existing db. Notes use source-keyed capture; episodes are immutable appends, not edits. Ordinary saves require curator; core notes require operator and episodes reject core. Provide source or operation_id for an exact retry; neither means a fresh manual submission. A lost generated receipt leaves an ambiguous outcome: JSON-RPC IDs are not replay identities. Retain the entire authored request unchanged for retry. Receipts include kind/id/replayed/origin and manual operation_id, never the body. No implicit store creation, upgrade or fallback. Similarity links are opt-in for notes; episodes use explicit links. Optional touchstone is note-only immutable subject/references with GET summary snapshot hashes, bound to this local database. Exact replay resolves no targets.",
        "inputSchema":schema,
    })
}

#[cfg(test)]
#[path = "save_tests.rs"]
pub(crate) mod tests;
