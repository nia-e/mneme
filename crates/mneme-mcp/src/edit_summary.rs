//! Native owner adapter; complete admission precedes profile and checkout.
use crate::AnyErr;
use mneme_app::edit_summary::{self as shared, PreparedSummaryEdit as Domain};
use serde_json::{Value, json};
pub(crate) struct PreparedSummaryEdit {
    pub(crate) inner: Domain,
}
impl PreparedSummaryEdit {
    pub(crate) fn parse(raw: &Value) -> Result<Self, AnyErr> {
        if serde_json::to_vec(raw)?.len() > shared::MAX_EDIT_SUMMARY_REQUEST_BYTES {
            return Err("edit_summary input exceeds its encoded byte allowance".into());
        }
        let mut object = raw
            .as_object()
            .ok_or("edit_summary arguments must be an object")?
            .clone();
        let db = object
            .remove("db")
            .ok_or("edit_summary requires explicit argument `db`")?;
        let db = db.as_str().ok_or("edit_summary db must be a string")?;
        if db.is_empty() || db.len() > 256 || db.trim() != db || db.chars().any(char::is_control) {
            return Err(
                "edit_summary db must be 1..=256 UTF-8 bytes, trimmed, without controls".into(),
            );
        }
        if crate::optional_expected_db_id(raw)?.is_none() {
            return Err("edit_summary requires canonical argument `expected_db_id`".into());
        }
        object.remove("expected_db_id");
        Ok(Self {
            inner: Domain::parse(&Value::Object(object))?,
        })
    }
}
pub(crate) fn tool_schema() -> Value {
    let mut schema = shared::input_schema();
    schema["properties"]["db"] = crate::db_prop();
    schema["properties"]["expected_db_id"] = crate::expected_db_id_prop();
    schema["required"] = json!([
        "db",
        "expected_db_id",
        "id",
        "expected_snapshot_sha256",
        "summary"
    ]);
    json!({"name":"edit_summary","description":"Replace a semantic node summary and refresh its derived retrieval embedding atomically using the GET summary_snapshot expected_snapshot_sha256 guard. Operator only; explicit db and expected_db_id required. Nonblank UTF-8 summary up to 16 KiB. Episode editions and touchstone owners refuse. Stale guard refuses: inspect before forming new intent. Original provenance describes original submission, not replacement content. Body, identity, edges and provenance are preserved. No automatic replay.","inputSchema":schema})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    #[cfg(not(feature = "fastembed"))]
    #[tokio::test]
    async fn summary_edit_native_cas_and_admission() {
        let (root, mut server, nodes) = crate::concern::tests::fixture().await;
        server.capability = CapabilityPolicy::new(CapabilityProfile::Operator, false);
        let db_id = server.registry.checkout("project").unwrap().db_id;
        let original = nodes[0].clone();
        let raw = json!({"db":"project","expected_db_id":db_id.to_string(),"id":original.id().0.to_string(),"expected_snapshot_sha256":mneme_core::SummarySnapshot::from_node(db_id, &original).unwrap().digest().to_string(),"summary":"new summary"});
        let call = |args: Value| {
            let server = &server;
            async move {
                call_tool_authorized(
                    &server.registry,
                    &server.sessions,
                    &server.cold_work,
                    server.capability,
                    "edit_summary",
                    &args,
                )
                .await
            }
        };
        let reply = call(raw.clone()).await.unwrap();
        assert_eq!(reply["id"], raw["id"]);
        assert_ne!(
            reply["summary_snapshot_sha256"],
            raw["expected_snapshot_sha256"]
        );
        assert!(reply.get("summary").is_none());
        assert!(call(raw.clone()).await.is_err());
        let read = server
            .registry
            .checkout("project")
            .unwrap()
            .mem
            .get_node(original.id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.summary(), "new summary");
        assert_eq!(read.provenance(), original.provenance());
        assert_eq!(read.tag_set(), original.tag_set());
        let mut malformed = raw;
        malformed["expected_snapshot_sha256"] = json!("A".repeat(64));
        assert!(PreparedSummaryEdit::parse(&malformed).is_err());
        let guarded = json!({"db":"project","expected_db_id":db_id.to_string(),"id":original.id().0.to_string(),"expected_snapshot_sha256":mneme_core::SummarySnapshot::from_node(db_id, &read).unwrap().digest().to_string(),"summary":"next summary"});
        call_tool_authorized(
            &server.registry,
            &server.sessions,
            &server.cold_work,
            server.capability,
            "database_control",
            &json!({"db":"project","action":"release"}),
        )
        .await
        .unwrap();
        assert!(
            call(guarded.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("maintenance")
        );
        let mut missing = guarded.clone();
        missing["db"] = json!("unavailable");
        assert!(call(missing).await.is_err());
        call_tool_authorized(
            &server.registry,
            &server.sessions,
            &server.cold_work,
            server.capability,
            "database_control",
            &json!({"db":"project","action":"resume"}),
        )
        .await
        .unwrap();
        let resumed = call(guarded).await.unwrap();
        assert_ne!(
            resumed["summary_snapshot_sha256"],
            reply["summary_snapshot_sha256"]
        );
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }
}
