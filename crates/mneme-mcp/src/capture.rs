//! MCP capture envelope: database selection and capability mapping only.
use crate::{AnyErr, IngestRequest};
use mneme_app::capture as shared;
use mneme_engine::{CaptureResult, Memory};
use serde_json::Value;

pub(crate) struct PreparedCapture {
    inner: shared::PreparedCapture,
}

impl PreparedCapture {
    pub(crate) fn parse(raw: &Value) -> Result<Self, AnyErr> {
        if serde_json::to_vec(raw)?.len() > 1024 * 1024 {
            return Err("capture input exceeds 1048576 UTF-8 bytes".into());
        }
        let object = raw
            .as_object()
            .ok_or("capture arguments must be an object")?;
        let db = object
            .get("db")
            .and_then(Value::as_str)
            .ok_or("capture field `db` must be a string")?;
        if db.is_empty() || db.len() > 256 || db.trim() != db || db.chars().any(char::is_control) {
            return Err(
                "capture field `db` must be 1..=256 UTF-8 bytes, trimmed, without controls".into(),
            );
        }
        let mut claim = object.clone();
        crate::optional_expected_db_id(raw)?;
        claim.remove("db");
        claim.remove("expected_db_id");
        Ok(Self {
            inner: shared::PreparedCapture::parse(&Value::Object(claim))?,
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
        origin_commit: Option<&str>,
    ) -> Result<CaptureResult, AnyErr> {
        Ok(self.inner.run(mem, origin_commit).await?)
    }
}

pub(crate) fn properties() -> Value {
    let mut properties = shared::properties();
    properties["db"] = crate::db_prop();
    properties["expected_db_id"] = crate::expected_db_id_prop();
    properties
}
