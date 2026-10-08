//! Grouped advisory concern envelope, over the already admitted owner only.
//! Domain parsing, native CAS and exact outcome validation remain shared.
use crate::{AnyErr, CapabilityProfile};
use mneme_app::concern::{self as shared, PreparedConcernRequest};
use mneme_core::ports::ConcernStore;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConcernAction {
    List,
    Notice,
    RecordFinding,
}

pub(crate) struct PreparedConcern {
    inner: PreparedConcernRequest,
    action: ConcernAction,
}

impl PreparedConcern {
    pub(crate) fn parse(raw: &Value) -> Result<Self, AnyErr> {
        if serde_json::to_vec(raw)?.len() > 1024 * 1024 {
            return Err("concern input exceeds 1048576 encoded bytes".into());
        }
        let object = raw
            .as_object()
            .ok_or("concern arguments must be an object")?;
        if let Some(db) = object.get("db") {
            let db = db.as_str().ok_or("concern field `db` must be a string")?;
            if db.is_empty()
                || db.len() > 256
                || db.trim() != db
                || db.chars().any(char::is_control)
            {
                return Err(
                    "concern field `db` must be 1..=256 UTF-8 bytes, trimmed, without controls"
                        .into(),
                );
            }
        }
        let expected_db_id = crate::optional_expected_db_id(raw)?;
        let mut domain = object.clone();
        domain.remove("db");
        domain.remove("expected_db_id");
        let inner = PreparedConcernRequest::parse(&Value::Object(domain))?;
        let action = match inner.action() {
            "list" => ConcernAction::List,
            "notice" => ConcernAction::Notice,
            "record_finding" => ConcernAction::RecordFinding,
            action => return Err(format!("unclassified native concern action {action:?}").into()),
        };
        if inner.is_mutation() {
            if !object.contains_key("db") {
                return Err("concern mutations require explicit argument `db`".into());
            }
            if expected_db_id.is_none() {
                return Err("concern mutations require canonical argument `expected_db_id`".into());
            }
        }
        Ok(Self { inner, action })
    }

    pub(crate) fn action(&self) -> ConcernAction {
        self.action
    }

    pub(crate) async fn execute(&self, store: &dyn ConcernStore) -> Result<Value, AnyErr> {
        self.inner.execute(store).await
    }
}

pub(crate) fn tool_schema(profile: CapabilityProfile) -> Value {
    let actions: &[&str] = match profile {
        CapabilityProfile::ReadOnly | CapabilityProfile::ReceiptGrounded => &["list"],
        CapabilityProfile::Curator | CapabilityProfile::Operator => {
            &["list", "notice", "record_finding"]
        }
    };
    let mut schema = shared::input_schema(actions);
    schema["properties"]["db"] = crate::db_prop();
    schema["properties"]["expected_db_id"] = crate::expected_db_id_prop();
    // Keep owner requirements conditional: read selection may default to user,
    // but no mutation can infer either its owner or its database identity.
    let guard = json!({
        "if":{"properties":{"action":{"enum":["notice","record_finding"]}},"required":["action"]},
        "then":{"required":["db","expected_db_id"]}
    });
    schema
        .as_object_mut()
        .expect("shared concern schema object")
        .entry("allOf")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .expect("concern allOf array")
        .push(guard);
    json!({
        "name":"concern",
        "description":"Read or maintain advisory tension between exact inspected native node meanings. list is a bounded indexed endpoint page (read-only); notice and record_finding require curator, explicit db and expected_db_id. GET issues concern_endpoint from its fetched node. Scoped findings are historical evidence, not universal resolutions. Returns the exact native page or atomic outcome, including refused CAS; never rebinds, retries changed intent, reads back over an intervening result, saves a lesson or merges/supersedes memories. Missing native support fails without fallback. No new store, upgrade or cross-store writer.",
        "inputSchema":schema,
    })
}

#[cfg(test)]
#[path = "concern_tests.rs"]
pub(crate) mod tests;
