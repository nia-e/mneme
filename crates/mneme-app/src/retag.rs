//! Complete tag-set compare-and-replace; no content, history, or retry promise.
use mneme_core::{BoundedTagSet, NodeId};
use mneme_engine::Memory;
use serde::Deserialize;
use serde_json::{Value, json};
pub type RetagError = Box<dyn std::error::Error + Send + Sync>;
// Six JSON bytes per UTF-8 byte safely bounds escaping; reserve owner/frame fields.
pub const MAX_RETAG_REQUEST_BYTES: usize =
    2 * mneme_core::MAX_NODE_TAGS * (6 * mneme_core::MAX_TAG_BYTES + 3) + 4096;
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedRetag {
    id: NodeId,
    expected_tags: BoundedTagSet,
    tags: BoundedTagSet,
}
impl PreparedRetag {
    pub fn parse(raw: &Value) -> Result<Self, RetagError> {
        if serde_json::to_vec(raw)?.len() > MAX_RETAG_REQUEST_BYTES {
            return Err("retag input exceeds its encoded byte allowance".into());
        }
        let request: Self = serde_json::from_value(raw.clone())?;
        if raw["id"] != json!(request.id.0.to_string()) {
            return Err("retag id must be a canonical ULID".into());
        }
        Ok(request)
    }
    pub fn requires_operator(&self) -> bool {
        self.expected_tags.contains("core") || self.tags.contains("core")
    }
    pub fn into_json(self) -> Value {
        json!({"id":self.id.0.to_string(),"expected_tags":self.expected_tags,"tags":self.tags})
    }
    pub async fn execute(&self, memory: &Memory) -> Result<Value, RetagError> {
        let node = memory
            .compare_replace_node_tags(self.id, &self.expected_tags, &self.tags)
            .await?;
        let result = json!({"id":node.id().0.to_string(),"tags":node.tag_set(),"changed":self.expected_tags != self.tags});
        self.validate_response_json(&result)?;
        Ok(result)
    }
    pub fn validate_response_json(&self, raw: &Value) -> Result<(), RetagError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reply {
            id: String,
            tags: BoundedTagSet,
            changed: bool,
        }
        if serde_json::to_vec(raw)?.len() > MAX_RETAG_REQUEST_BYTES {
            return Err("retag response exceeds its encoded byte allowance".into());
        }
        let reply: Reply = serde_json::from_value(raw.clone())?;
        if reply.id != self.id.0.to_string()
            || reply.tags != self.tags
            || reply.changed != (self.expected_tags != self.tags)
        {
            return Err(
                "retag acknowledgement does not match the checked request; do not blindly retry"
                    .into(),
            );
        }
        Ok(())
    }
    pub fn validate_routed_response_json(
        &self,
        raw: &Value,
        db: &str,
        expected_db_id: Option<ulid::Ulid>,
    ) -> Result<(), RetagError> {
        if serde_json::to_vec(raw)?.len() > MAX_RETAG_REQUEST_BYTES {
            return Err("retag owner response exceeds its encoded byte allowance".into());
        }
        let mut object = raw
            .as_object()
            .ok_or("retag owner response must be an object")?
            .clone();
        if object.remove("db") != Some(json!(db)) {
            return Err(
                "retag response database mismatch; do not retry against another owner".into(),
            );
        }
        let value = object
            .remove("db_id")
            .ok_or("retag response database identity missing")?;
        let id: ulid::Ulid = value
            .as_str()
            .ok_or("retag database identity must be a string")?
            .parse()?;
        if value != json!(id.to_string()) || expected_db_id.is_some_and(|expected| expected != id) {
            return Err(
                "retag response database identity mismatch; do not retry against another owner"
                    .into(),
            );
        }
        self.validate_response_json(&Value::Object(object))
    }
}
pub fn input_schema() -> Value {
    let tags = json!({"type":"array","maxItems":mneme_core::MAX_NODE_TAGS,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":mneme_core::MAX_TAG_BYTES},"description":"Complete tag set; empty is valid. Native admission checks UTF-8 bytes, trim and controls."});
    json!({"type":"object","additionalProperties":false,"required":["id","expected_tags","tags"],"properties":{"id":{"type":"string","minLength":26,"maxLength":26},"expected_tags":tags,"tags":tags}})
}
pub fn render_human(raw: &Value) -> String {
    format!(
        "{} {}: {}",
        if raw["changed"] == true {
            "retagged"
        } else {
            "unchanged"
        },
        raw["id"].as_str().unwrap_or("?"),
        raw["tags"]
            .as_array()
            .map(|tags| tags
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", "))
            .unwrap_or_default()
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    fn raw() -> Value {
        json!({"id":ulid::Ulid::from(1).to_string(),"expected_tags":[],"tags":["possibility","closed"]})
    }
    #[test]
    fn complete_closed_bounded_canonical_request() {
        assert!(PreparedRetag::parse(&raw()).is_ok());
        for field in ["id", "expected_tags", "tags"] {
            let mut r = raw();
            r.as_object_mut().unwrap().remove(field);
            assert!(PreparedRetag::parse(&r).is_err());
        }
        for tags in [
            json!(["duplicate", "duplicate"]),
            json!([" bad"]),
            json!(null),
            json!(["x".repeat(mneme_core::MAX_TAG_BYTES + 1)]),
        ] {
            let mut r = raw();
            r["tags"] = tags;
            assert!(PreparedRetag::parse(&r).is_err());
        }
        let mut r = raw();
        r["extra"] = json!(true);
        assert!(PreparedRetag::parse(&r).is_err());
    }
    #[test]
    fn maximum_valid_escaped_sets_fit_envelope() {
        let tags = (0..mneme_core::MAX_NODE_TAGS)
            .map(|i| format!("{i:04}{}", "\\".repeat(mneme_core::MAX_TAG_BYTES - 4)))
            .collect::<Vec<_>>();
        let value = json!({"id":ulid::Ulid::from(1).to_string(),"expected_tags":tags,"tags":tags});
        assert!(serde_json::to_vec(&value).unwrap().len() <= MAX_RETAG_REQUEST_BYTES);
        assert!(PreparedRetag::parse(&value).is_ok());
    }
    #[test]
    fn core_authority_and_acknowledgement() {
        let mut r = raw();
        r["expected_tags"] = json!(["core"]);
        let request = PreparedRetag::parse(&r).unwrap();
        assert!(request.requires_operator());
        let mut reply = json!({"id":r["id"],"tags":r["tags"],"changed":true,"db":"project","db_id":ulid::Ulid::from(2).to_string()});
        assert!(
            request
                .validate_routed_response_json(&reply, "project", Some(ulid::Ulid::from(2)))
                .is_ok()
        );
        reply["changed"] = json!(false);
        assert!(
            request
                .validate_routed_response_json(&reply, "project", None)
                .is_err()
        );
    }
}
