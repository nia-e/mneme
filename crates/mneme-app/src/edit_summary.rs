//! Guarded semantic summary replacement; source provenance is retained, not recertified.
use mneme_core::{
    MAX_NODE_SUMMARY_BYTES, NodeId, NodeSummary, SummarySnapshot, SummarySnapshotDigest,
};
use mneme_engine::Memory;
use serde::Deserialize;
use serde_json::{Value, json};
pub type EditSummaryError = Box<dyn std::error::Error + Send + Sync>;
/// Worst-case JSON escaping plus bounded owner and frame fields.
pub const MAX_EDIT_SUMMARY_REQUEST_BYTES: usize = 6 * MAX_NODE_SUMMARY_BYTES + 4096;
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedSummaryEdit {
    id: NodeId,
    #[serde(deserialize_with = "snapshot_digest")]
    expected_snapshot_sha256: SummarySnapshotDigest,
    summary: NodeSummary,
}
impl PreparedSummaryEdit {
    pub fn parse(raw: &Value) -> Result<Self, EditSummaryError> {
        if serde_json::to_vec(raw)?.len() > MAX_EDIT_SUMMARY_REQUEST_BYTES {
            return Err("edit_summary input exceeds its encoded byte allowance".into());
        }
        let request: Self = serde_json::from_value(raw.clone())?;
        if raw["id"] != json!(request.id.0.to_string()) {
            return Err("edit_summary id must be a canonical ULID".into());
        }
        Ok(request)
    }
    pub fn into_json(self) -> Value {
        json!({"id":self.id.0.to_string(),"expected_snapshot_sha256":self.expected_snapshot_sha256.to_string(),"summary":self.summary.as_str()})
    }
    pub async fn execute(
        &self,
        memory: &Memory,
        db_id: ulid::Ulid,
    ) -> Result<Value, EditSummaryError> {
        let node = memory
            .edit_summary(
                self.id,
                &self.expected_snapshot_sha256,
                self.summary.clone(),
            )
            .await?;
        let result = json!({"id":node.id().0.to_string(),"summary_snapshot_sha256":SummarySnapshot::from_node(db_id, &node)?.digest().to_string()});
        self.validate_response_json(&result)?;
        Ok(result)
    }
    pub fn validate_response_json(&self, raw: &Value) -> Result<(), EditSummaryError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reply {
            id: String,
            #[serde(deserialize_with = "snapshot_digest")]
            summary_snapshot_sha256: SummarySnapshotDigest,
        }
        if serde_json::to_vec(raw)?.len() > 4096 {
            return Err("edit_summary response exceeds its byte allowance".into());
        }
        let reply: Reply = serde_json::from_value(raw.clone())?;
        if reply.id != self.id.0.to_string()
            || reply.summary_snapshot_sha256 == self.expected_snapshot_sha256
        {
            return Err("edit_summary acknowledgement does not match the checked request; inspect before submitting a new intent".into());
        }
        Ok(())
    }
    pub fn validate_routed_response_json(
        &self,
        raw: &Value,
        db: &str,
        expected_db_id: Option<ulid::Ulid>,
    ) -> Result<(), EditSummaryError> {
        if serde_json::to_vec(raw)?.len() > 4096 {
            return Err("edit_summary owner response exceeds its byte allowance".into());
        }
        let mut object = raw
            .as_object()
            .ok_or("edit_summary owner response must be an object")?
            .clone();
        if object.remove("db") != Some(json!(db)) {
            return Err(
                "edit_summary response database mismatch; do not retry against another owner"
                    .into(),
            );
        }
        let value = object
            .remove("db_id")
            .ok_or("edit_summary response database identity missing")?;
        let id: ulid::Ulid = value
            .as_str()
            .ok_or("edit_summary database identity must be a string")?
            .parse()?;
        if value != json!(id.to_string()) || expected_db_id.is_some_and(|expected| expected != id) {
            return Err(
                "edit_summary response database identity mismatch; do not retry against another owner"
                    .into(),
            );
        }
        self.validate_response_json(&Value::Object(object))
    }
}
pub fn input_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["id","expected_snapshot_sha256","summary"],"properties":{"id":{"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"},"expected_snapshot_sha256":{"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$"},"summary":{"type":"string","minLength":1,"maxLength":MAX_NODE_SUMMARY_BYTES,"description":"Nonblank UTF-8 semantic summary, bounded by bytes. Only summary and its derived retrieval embedding change. Inspect stale snapshot guards and submit a new intent; no blind replay."}}})
}
pub fn render_human(raw: &Value) -> String {
    format!(
        "edited summary {}: {}",
        raw["id"].as_str().unwrap_or("?"),
        raw["summary_snapshot_sha256"].as_str().unwrap_or("?")
    )
}

fn snapshot_digest<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SummarySnapshotDigest, D::Error> {
    let value = String::deserialize(deserializer)?;
    SummarySnapshotDigest::from_hex(&value).map_err(serde::de::Error::custom)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn raw() -> Value {
        json!({"id":ulid::Ulid::new().to_string(),"expected_snapshot_sha256":"a".repeat(64),"summary":"replacement"})
    }
    #[test]
    fn closed_bounded_admission() {
        let base = raw();
        assert!(PreparedSummaryEdit::parse(&base).is_ok());
        for key in ["id", "expected_snapshot_sha256", "summary"] {
            let mut value = base.clone();
            value.as_object_mut().unwrap().remove(key);
            assert!(PreparedSummaryEdit::parse(&value).is_err());
        }
        let mut value = base.clone();
        value["extra"] = json!(true);
        assert!(PreparedSummaryEdit::parse(&value).is_err());
        for token in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
            let mut value = base.clone();
            value["expected_snapshot_sha256"] = json!(token);
            assert!(PreparedSummaryEdit::parse(&value).is_err());
        }
        for summary in [
            "".to_owned(),
            " \n".to_owned(),
            "é".repeat(MAX_NODE_SUMMARY_BYTES / 2 + 1),
        ] {
            let mut value = base.clone();
            value["summary"] = json!(summary);
            assert!(PreparedSummaryEdit::parse(&value).is_err());
        }
        let mut value = base.clone();
        value["summary"] = json!("x".repeat(MAX_NODE_SUMMARY_BYTES + 1));
        assert!(PreparedSummaryEdit::parse(&value).is_err());
        value["summary"] = json!("x".repeat(MAX_NODE_SUMMARY_BYTES));
        assert!(PreparedSummaryEdit::parse(&value).is_ok());
    }
    #[test]
    fn acknowledgements_are_closed_and_owner_bound() {
        let base = raw();
        let request = PreparedSummaryEdit::parse(&base).unwrap();
        let reply = json!({"id":base["id"],"summary_snapshot_sha256":"b".repeat(64)});
        assert!(request.validate_response_json(&reply).is_ok());
        let mut wrong_id = reply.clone();
        wrong_id["id"] = json!(ulid::Ulid::new().to_string());
        assert!(request.validate_response_json(&wrong_id).is_err());
        let mut bad = reply.clone();
        bad["summary_snapshot_sha256"] = base["expected_snapshot_sha256"].clone();
        assert!(request.validate_response_json(&bad).is_err());
        let mut bad = reply.clone();
        bad["summary"] = json!("echo");
        assert!(request.validate_response_json(&bad).is_err());
        let db_id = ulid::Ulid::new();
        let mut routed = reply;
        routed["db"] = json!("project");
        routed["db_id"] = json!(db_id.to_string());
        assert!(
            request
                .validate_routed_response_json(&routed, "project", Some(db_id))
                .is_ok()
        );
        assert!(
            request
                .validate_routed_response_json(&routed, "other", Some(db_id))
                .is_err()
        );
        assert!(
            request
                .validate_routed_response_json(&routed, "project", Some(ulid::Ulid::new()))
                .is_err()
        );
    }
}
