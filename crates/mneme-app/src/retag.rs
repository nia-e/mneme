//! Complete tag-set compare-and-replace with opt-in canonical-content guards.
//! No content edit, revision-history, or automatic retry promise.
use mneme_core::ports::{NodeContentGuard, RetagContentGuards};
use mneme_core::{BoundedTagSet, NodeId};
use mneme_engine::Memory;
use serde::Deserialize;
use serde_json::{Value, json};
pub type RetagError = Box<dyn std::error::Error + Send + Sync>;
// Six JSON bytes per UTF-8 byte safely bounds escaping; reserve owner/frame fields.
pub const MAX_RETAG_REQUEST_BYTES: usize =
    2 * mneme_core::MAX_NODE_TAGS * (6 * mneme_core::MAX_TAG_BYTES + 3)
        + mneme_core::MAX_NODE_HYDRATION_BATCH * 160
        + 4096;
#[derive(Clone, Debug)]
pub struct PreparedRetag {
    id: NodeId,
    expected_tags: BoundedTagSet,
    tags: BoundedTagSet,
    expected_content_fingerprint: Option<String>,
    guard_nodes: Option<Vec<NodeContentGuard>>,
}
impl PreparedRetag {
    pub fn parse(raw: &Value) -> Result<Self, RetagError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            id: NodeId,
            expected_tags: BoundedTagSet,
            tags: BoundedTagSet,
            expected_content_fingerprint: Option<String>,
            guard_nodes: Option<Vec<NodeContentGuard>>,
        }
        if serde_json::to_vec(raw)?.len() > MAX_RETAG_REQUEST_BYTES {
            return Err("retag input exceeds its encoded byte allowance".into());
        }
        let input: Input = serde_json::from_value(raw.clone())?;
        let request = Self {
            id: input.id,
            expected_tags: input.expected_tags,
            tags: input.tags,
            expected_content_fingerprint: input.expected_content_fingerprint,
            guard_nodes: input.guard_nodes,
        };
        if raw["id"] != json!(request.id.0.to_string()) {
            return Err("retag id must be a canonical ULID".into());
        }
        for name in ["expected_content_fingerprint", "guard_nodes"] {
            if raw.get(name).is_some_and(Value::is_null) {
                return Err(format!("retag {name} must not be null").into());
            }
        }
        if request.guard_nodes.is_some() && request.expected_content_fingerprint.is_none() {
            return Err("retag guard_nodes requires expected_content_fingerprint".into());
        }
        if let Some(guards) = request.content_guards() {
            guards.validate()?;
            for (index, guard) in guards.guard_nodes.iter().enumerate() {
                if raw["guard_nodes"][index]["id"] != json!(guard.id.0.to_string()) {
                    return Err("retag guard node id must be a canonical ULID".into());
                }
            }
        }
        Ok(request)
    }
    pub fn requires_operator(&self) -> bool {
        self.expected_tags.contains("core") || self.tags.contains("core")
    }
    pub fn into_json(self) -> Value {
        let mut value =
            json!({"id":self.id.0.to_string(),"expected_tags":self.expected_tags,"tags":self.tags});
        if let Some(fingerprint) = self.expected_content_fingerprint {
            value["expected_content_fingerprint"] = json!(fingerprint);
        }
        if let Some(guards) = self.guard_nodes {
            value["guard_nodes"] = json!(guards);
        }
        value
    }
    pub fn requires_content_guards(&self) -> bool {
        self.expected_content_fingerprint.is_some()
    }
    fn content_guards(&self) -> Option<RetagContentGuards> {
        self.expected_content_fingerprint
            .as_ref()
            .map(|fingerprint| RetagContentGuards {
                target_fingerprint: fingerprint.clone(),
                guard_nodes: self.guard_nodes.clone().unwrap_or_default(),
            })
    }
    pub async fn execute(&self, memory: &Memory) -> Result<Value, RetagError> {
        // The backend checks target meaning and guide guards in the same write
        // snapshot as the tag CAS. A separate get/preflight cannot provide this.
        let node = if let Some(guards) = self.content_guards() {
            memory
                .compare_replace_node_tags_guarded(
                    self.id,
                    &self.expected_tags,
                    &self.tags,
                    &guards,
                )
                .await?
        } else {
            memory
                .compare_replace_node_tags(self.id, &self.expected_tags, &self.tags)
                .await?
        };
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
    let fingerprint =
        json!({"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$"});
    json!({"type":"object","additionalProperties":false,"required":["id","expected_tags","tags"],
        "dependentRequired":{"guard_nodes":["expected_content_fingerprint"]},
        "properties":{"id":{"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"},"expected_tags":tags,"tags":tags,
            "expected_content_fingerprint":fingerprint,
            "guard_nodes":{"type":"array","maxItems":mneme_core::MAX_NODE_HYDRATION_BATCH,
                "description":"Optional same-owner semantic nodes, checked atomically with target fingerprint and expected tags. For policy guides, use the complete bounded canonical summary, not body bytes. Fingerprints bind canonical content and body pointers, not mutable external body bytes.",
                "items":{"type":"object","additionalProperties":false,"required":["id","content_fingerprint"],
                    "properties":{"id":{"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"},"content_fingerprint":fingerprint}}}}})
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

    #[test]
    fn opt_in_content_guards_are_canonical_closed_and_never_silently_dropped() {
        let mut original = raw();
        original["tags"] = json!(["closed", "possibility"]);
        let old = PreparedRetag::parse(&original).unwrap();
        assert!(!old.requires_content_guards());
        assert_eq!(old.into_json(), original);
        let mut strong = original;
        strong["expected_content_fingerprint"] = json!("a".repeat(64));
        strong["guard_nodes"] =
            json!([{"id":ulid::Ulid::from(2).to_string(),"content_fingerprint":"b".repeat(64)}]);
        let request = PreparedRetag::parse(&strong).unwrap();
        assert!(request.requires_content_guards());
        assert_eq!(request.into_json(), strong);
        for bad in [
            json!(null),
            json!(true),
            json!("a".repeat(63)),
            json!("A".repeat(64)),
            json!("g".repeat(64)),
        ] {
            let mut value = strong.clone();
            value["expected_content_fingerprint"] = bad;
            assert!(PreparedRetag::parse(&value).is_err(), "accepted {value}");
        }
        for bad in [
            json!(null),
            json!({}),
            json!([{"id":ulid::Ulid::from(2).to_string()}]),
            json!([{"id":ulid::Ulid::from(2).to_string(),"content_fingerprint":"A".repeat(64)}]),
            json!([{"id":ulid::Ulid::from(2).to_string(),"content_fingerprint":"b".repeat(64),"extra":true}]),
            json!([
                strong["guard_nodes"][0].clone(),
                strong["guard_nodes"][0].clone()
            ]),
            json!(vec![
                strong["guard_nodes"][0].clone();
                mneme_core::MAX_NODE_HYDRATION_BATCH + 1
            ]),
        ] {
            let mut value = strong.clone();
            value["guard_nodes"] = bad;
            assert!(PreparedRetag::parse(&value).is_err());
        }
        strong
            .as_object_mut()
            .unwrap()
            .remove("expected_content_fingerprint");
        assert!(PreparedRetag::parse(&strong).is_err());
        strong["guard_nodes"] = json!([]);
        assert!(PreparedRetag::parse(&strong).is_err());
    }

    #[tokio::test]
    async fn target_and_guide_meaning_are_checked_by_the_mutation_not_preflight() {
        use mneme_core::ports::{GraphStore, SystemClock, routing_content_fingerprint};
        use mneme_core::{
            BodyRef, Confidence, Node, NodeInit, NodeStatus, NodeSummary, Provenance, Stability,
        };
        use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
        use std::sync::Arc;
        let make_node = |id, summary: &str| {
            Node::new(NodeInit {
                id: NodeId(ulid::Ulid(id)),
                summary: NodeSummary::new(summary).unwrap(),
                body: BodyRef::new("file:///never-read-body").unwrap(),
                tags: BoundedTagSet::default(),
                provenance: Provenance::Conversation {
                    session: ulid::Ulid(0),
                    turn: 0,
                },
                stability: Stability::new(0.5).unwrap(),
                confidence: Confidence::new(0.5).unwrap(),
                status: NodeStatus::Active,
                created: id,
            })
        };
        let store = Arc::new(mneme_cozo::MemStore::new(DEFAULT_DIM));
        let memory = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
            Arc::new(SystemClock),
            mneme_engine::Config::default(),
        );
        let target = make_node(1, "Original target meaning");
        let guide = make_node(2, "Original guide meaning");
        store.put_node(&target).await.unwrap();
        store.put_node(&guide).await.unwrap();
        let args = json!({"id":target.id().0.to_string(),"expected_tags":[],"tags":["topic"],
            "expected_content_fingerprint":routing_content_fingerprint(&target),
            "guard_nodes":[{"id":guide.id().0.to_string(),"content_fingerprint":routing_content_fingerprint(&guide)}]});
        let prepared = PreparedRetag::parse(&args).unwrap();
        // Both read snapshots were valid at admission. A newer target must stop
        // this already-prepared write even though the tag set has not changed.
        store
            .put_node(&make_node(1, "New target meaning"))
            .await
            .unwrap();
        assert!(
            prepared
                .execute(&memory)
                .await
                .unwrap_err()
                .to_string()
                .contains("content changed")
        );
        assert!(
            store
                .get_node(target.id())
                .await
                .unwrap()
                .unwrap()
                .tag_set()
                .is_empty()
        );
        store.put_node(&target).await.unwrap();
        store
            .put_node(&make_node(2, "New guide meaning"))
            .await
            .unwrap();
        assert!(
            prepared
                .execute(&memory)
                .await
                .unwrap_err()
                .to_string()
                .contains("guide")
        );
        assert!(
            store
                .get_node(target.id())
                .await
                .unwrap()
                .unwrap()
                .tag_set()
                .is_empty()
        );
        // Even a would-be no-op is not a successful acknowledgement of stale
        // policy. It must reach the atomic guarded contract.
        let mut no_op = args.clone();
        no_op["tags"] = json!([]);
        assert!(
            PreparedRetag::parse(&no_op)
                .unwrap()
                .execute(&memory)
                .await
                .is_err()
        );
        store.put_node(&guide).await.unwrap();
        assert_eq!(
            prepared.execute(&memory).await.unwrap()["tags"],
            json!(["topic"])
        );
        let current = store.get_node(target.id()).await.unwrap().unwrap();
        assert_eq!(current.summary(), target.summary());
        assert_eq!(current.body(), target.body());
        // Existing callers retain their explicit tag-only CAS contract.
        let old = json!({"id":target.id().0.to_string(),"expected_tags":["topic"],"tags":[]});
        assert!(
            PreparedRetag::parse(&old)
                .unwrap()
                .execute(&memory)
                .await
                .is_ok()
        );
    }
}
