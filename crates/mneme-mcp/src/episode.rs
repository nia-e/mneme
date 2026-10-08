//! MCP episode envelope: explicit owner selection and shared action authority.
//!
//! Parsing, storage operations, bounds and cursor semantics belong to mneme-app.
//! This adapter neither infers an episode from tags nor hides an editorial write
//! inside a read action.
use crate::{AnyErr, CapabilityProfile, NodeId};
use mneme_app::episode::{self as shared, EpisodeAction};
use mneme_engine::Memory;
use serde_json::{Value, json};
use ulid::Ulid;

pub(crate) struct PreparedEpisode {
    inner: shared::PreparedEpisode,
}

impl PreparedEpisode {
    pub(crate) fn parse(raw: &Value) -> Result<Self, AnyErr> {
        let object = raw
            .as_object()
            .ok_or("episode arguments must be an object")?;
        if let Some(db) = object.get("db") {
            let db = db.as_str().ok_or("episode field `db` must be a string")?;
            if db.is_empty()
                || db.len() > 256
                || db.trim() != db
                || db.chars().any(char::is_control)
            {
                return Err(
                    "episode field `db` must be 1..=256 UTF-8 bytes, trimmed, without controls"
                        .into(),
                );
            }
        }
        let mut arguments = object.clone();
        crate::optional_expected_db_id(raw)?;
        arguments.remove("db");
        arguments.remove("expected_db_id");
        let inner = shared::PreparedEpisode::parse(&Value::Object(arguments))?;
        if inner.is_mutation() && !object.contains_key("db") {
            return Err("episode mutations require explicit argument `db`".into());
        }
        Ok(Self { inner })
    }

    pub(crate) fn action(&self) -> EpisodeAction {
        self.inner.action()
    }

    pub(crate) fn is_mutation(&self) -> bool {
        self.inner.is_mutation()
    }

    pub(crate) async fn run(
        self,
        mem: &Memory,
        db_id: Ulid,
        origin_commit: Option<&str>,
    ) -> Result<Value, AnyErr> {
        Ok(self.inner.run(mem, db_id, origin_commit).await?)
    }
}

pub(crate) fn tool_schema(profile: CapabilityProfile) -> Value {
    let allowed: &[EpisodeAction] = match profile {
        CapabilityProfile::ReadOnly | CapabilityProfile::ReceiptGrounded => {
            &EpisodeAction::READ_ONLY
        }
        CapabilityProfile::Curator => &EpisodeAction::CURATOR,
        CapabilityProfile::Operator => &EpisodeAction::ALL,
    };
    let mut schema = shared::input_schema(allowed);
    schema["properties"]["db"] = crate::db_prop();
    schema["properties"]["expected_db_id"] = crate::expected_db_id_prop();
    // Hosts may render each oneOf branch independently, without intersecting
    // the root properties. Keep the explicit owner visible in every action.
    for branch in schema["oneOf"]
        .as_array_mut()
        .expect("episode action union")
    {
        branch["properties"]["db"] = crate::db_prop();
        branch["properties"]["expected_db_id"] = crate::expected_db_id_prop();
        if matches!(
            branch["properties"]["action"]["const"].as_str(),
            Some("append" | "revise")
        ) {
            branch["required"]
                .as_array_mut()
                .expect("episode action required fields")
                .push(json!("db"));
        }
    }
    json!({
        "name": "episode",
        "description": "Selective episodic memory, separate from semantic recall. list/search/get/history/references are bounded reads; search is lexical over current summaries, not semantic retrieval. append records an experience without requiring a lesson (curator). revise writes a full replacement edition under a stable episode identity using an expected current edition (operator); original bodies and evidence links remain available. Optional occurrence_contexts name where an episode happened, independently of its recorder; omission means unknown and revisions never inherit prior context. Source-key retries return the original edition, never a new edit. Times are epoch milliseconds; occurrence is distinct from recording. Writes require explicit db; omitted read scope selects project or the sole registered owner. Pages use best-effort keyset cursors, not snapshot isolation. Read a short recent slice or narrow cue first; use bounded subagent recall for a long history rather than dumping it into context.",
        "inputSchema": schema,
    })
}

/// Extract only IDs in an already prepared bounded read result for disposable
/// visual telemetry. This has no authority or retrieval role.
pub(crate) fn returned_ids(result: &Value) -> Vec<NodeId> {
    let mut ids = Vec::new();
    let mut add = |value: &Value| {
        if ids.len() > crate::activity::MAX_IDS {
            return;
        }
        if let Some(id) = value.as_str().and_then(|value| value.parse::<Ulid>().ok()) {
            let id = NodeId(id);
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    };
    add(&result["edition_id"]);
    if let Some(items) = result["items"].as_array() {
        for item in items.iter().take(crate::activity::MAX_IDS + 1) {
            add(&item["edition_id"]);
            add(&item["edge"]["from"]);
            add(&item["edge"]["to"]);
        }
    }
    ids
}

#[cfg(test)]
#[path = "episode_tests.rs"]
pub(crate) mod tests;
