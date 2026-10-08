//! Guarded body-only edit. Original provenance remains an account of capture,
//! not certification of the replacement bytes. No replay or history promise.
use mneme_core::{BodyRevision, NodeId};
use mneme_engine::{MAX_CAPTURE_BODY_BYTES, Memory};
use serde::Deserialize;
use serde_json::{Value, json};
pub type EditBodyError = Box<dyn std::error::Error + Send + Sync>;
/// Worst-case JSON escaping plus bounded owner and frame fields.
pub const MAX_EDIT_BODY_REQUEST_BYTES: usize = 6 * MAX_CAPTURE_BODY_BYTES + 4096;
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedBodyEdit {
    id: NodeId,
    #[serde(deserialize_with = "revision")]
    expected_body_revision: BodyRevision,
    body: String,
}
impl PreparedBodyEdit {
    pub fn parse(raw: &Value) -> Result<Self, EditBodyError> {
        if serde_json::to_vec(raw)?.len() > MAX_EDIT_BODY_REQUEST_BYTES {
            return Err("edit_body input exceeds its encoded byte allowance".into());
        }
        let request: Self = serde_json::from_value(raw.clone())?;
        if raw["id"] != json!(request.id.0.to_string()) {
            return Err("edit_body id must be a canonical ULID".into());
        }
        if request.body.len() > MAX_CAPTURE_BODY_BYTES {
            return Err("edit_body body exceeds its UTF-8 byte allowance".into());
        }
        Ok(request)
    }
    pub fn into_json(self) -> Value {
        json!({"id":self.id.0.to_string(),"expected_body_revision":self.expected_body_revision.to_string(),"body":self.body})
    }
    pub async fn execute(&self, memory: &Memory) -> Result<Value, EditBodyError> {
        let node = memory
            .edit_body(self.id, &self.expected_body_revision, self.body.as_bytes())
            .await?;
        let result =
            json!({"id":node.id().0.to_string(),"body_revision":node.body_revision().to_string()});
        self.validate_response_json(&result)?;
        Ok(result)
    }
    pub fn validate_response_json(&self, raw: &Value) -> Result<(), EditBodyError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reply {
            id: String,
            #[serde(deserialize_with = "revision")]
            body_revision: BodyRevision,
        }
        if serde_json::to_vec(raw)?.len() > 4096 {
            return Err("edit_body response exceeds its byte allowance".into());
        }
        let reply: Reply = serde_json::from_value(raw.clone())?;
        if reply.id != self.id.0.to_string() || reply.body_revision == self.expected_body_revision {
            return Err("edit_body acknowledgement does not match the checked request; inspect before submitting a new intent".into());
        }
        Ok(())
    }
    pub fn validate_routed_response_json(
        &self,
        raw: &Value,
        db: &str,
        expected_db_id: Option<ulid::Ulid>,
    ) -> Result<(), EditBodyError> {
        if serde_json::to_vec(raw)?.len() > 4096 {
            return Err("edit_body owner response exceeds its byte allowance".into());
        }
        let mut object = raw
            .as_object()
            .ok_or("edit_body owner response must be an object")?
            .clone();
        if object.remove("db") != Some(json!(db)) {
            return Err(
                "edit_body response database mismatch; do not retry against another owner".into(),
            );
        }
        let value = object
            .remove("db_id")
            .ok_or("edit_body response database identity missing")?;
        let id: ulid::Ulid = value
            .as_str()
            .ok_or("edit_body database identity must be a string")?
            .parse()?;
        if value != json!(id.to_string()) || expected_db_id.is_some_and(|expected| expected != id) {
            return Err(
                "edit_body response database identity mismatch; do not retry against another owner"
                    .into(),
            );
        }
        self.validate_response_json(&Value::Object(object))
    }
}
pub fn input_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["id","expected_body_revision","body"],"properties":{"id":{"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"},"expected_body_revision":{"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$"},"body":{"type":"string","maxLength":MAX_CAPTURE_BODY_BYTES,"description":"UTF-8 body, bounded by bytes; empty is valid. Only body changes. Inspect stale revisions and submit a new intent; no blind replay."}}})
}
pub fn render_human(raw: &Value) -> String {
    format!(
        "edited body {}: {}",
        raw["id"].as_str().unwrap_or("?"),
        raw["body_revision"].as_str().unwrap_or("?")
    )
}

fn revision<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<BodyRevision, D::Error> {
    let value = String::deserialize(deserializer)?;
    BodyRevision::parse(&value).map_err(serde::de::Error::custom)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn raw() -> Value {
        json!({"id":ulid::Ulid::new().to_string(),"expected_body_revision":"a".repeat(64),"body":""})
    }
    #[test]
    fn closed_bounded_admission() {
        let base = raw();
        assert!(PreparedBodyEdit::parse(&base).is_ok());
        for key in ["id", "expected_body_revision", "body"] {
            let mut value = base.clone();
            value.as_object_mut().unwrap().remove(key);
            assert!(PreparedBodyEdit::parse(&value).is_err());
        }
        let mut value = base.clone();
        value["extra"] = json!(true);
        assert!(PreparedBodyEdit::parse(&value).is_err());
        for token in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
            let mut value = base.clone();
            value["expected_body_revision"] = json!(token);
            assert!(PreparedBodyEdit::parse(&value).is_err());
        }
        let mut value = base.clone();
        value["body"] = json!("x".repeat(MAX_CAPTURE_BODY_BYTES + 1));
        assert!(PreparedBodyEdit::parse(&value).is_err());
        value["body"] = json!("\0".repeat(MAX_CAPTURE_BODY_BYTES));
        assert!(PreparedBodyEdit::parse(&value).is_ok());
    }
    #[test]
    fn acknowledgements_are_closed_and_owner_bound() {
        let base = raw();
        let request = PreparedBodyEdit::parse(&base).unwrap();
        let reply = json!({"id":base["id"],"body_revision":"b".repeat(64)});
        assert!(request.validate_response_json(&reply).is_ok());
        let mut bad = reply.clone();
        bad["body_revision"] = base["expected_body_revision"].clone();
        assert!(request.validate_response_json(&bad).is_err());
        let mut bad = reply.clone();
        bad["body"] = json!("echo");
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
