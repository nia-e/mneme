//! Prepared CLI intent stays coupled to its catalog requirements and receipt
//! validator. Routing may add an owner guard, but cannot replace the operation
//! or its frozen payload after admission.

use mneme_app::{episode::EpisodeAction, query::SummaryMaxBytes};
use serde_json::{Value, json};
use ulid::Ulid;

use crate::{AnyErr, remote_transport::RemoteClient};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestAccess {
    ReadOnly,
    Mutation,
}

pub(super) enum PreparedOperation {
    Status,
    Core,
    Query,
    RecallContext,
    Get,
    Neighbors,
    RemoteEdges,
    Ingest,
    EditBody(crate::cli_edit_body::Prepared),
    EditSummary(crate::cli_edit_summary::Prepared),
    Retag(crate::cli_retag::Prepared),
    Concern(Box<crate::cli_concern::PreparedConcern>),
    Save(Box<mneme_app::save::PreparedSave>),
    Capture { requires_links: bool },
    Episode(EpisodeAction),
    Link,
    Supersede,
    Contradict,
    Contradictions,
    Reconcile,
    Feedback,
    Merges,
    Merge,
    Forget,
    Decay,
    Prune,
    List,
    SnapshotCreate,
    Walk,
    Reflect,
}

impl PreparedOperation {
    fn tool(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Core => "core",
            Self::Query => "query",
            Self::RecallContext => "recall_context",
            Self::Get => "get",
            Self::Neighbors => "neighbors",
            Self::RemoteEdges => "remote_edges",
            Self::Ingest => "ingest",
            Self::EditBody(_) => "edit_body",
            Self::EditSummary(_) => "edit_summary",
            Self::Retag(_) => "retag",
            Self::Concern(_) => "concern",
            Self::Save(_) => "save",
            Self::Capture { .. } => "capture",
            Self::Episode(_) => "episode",
            Self::Link => "link",
            Self::Supersede => "supersede",
            Self::Contradict => "contradict",
            Self::Contradictions => "contradictions",
            Self::Reconcile => "reconcile",
            Self::Feedback => "feedback",
            Self::Merges => "merges",
            Self::Merge => "merge",
            Self::Forget => "forget",
            Self::Decay => "decay",
            Self::Prune => "prune",
            Self::List => "list",
            Self::SnapshotCreate => "snapshot_create",
            Self::Walk => "walk",
            Self::Reflect => "reflect",
        }
    }

    fn access(&self) -> RequestAccess {
        match self {
            Self::Status
            | Self::Core
            | Self::Query
            | Self::RecallContext
            | Self::Get
            | Self::Neighbors
            | Self::RemoteEdges
            | Self::Contradictions
            | Self::Merges
            | Self::List
            | Self::Walk => RequestAccess::ReadOnly,
            Self::Episode(action) => match action {
                EpisodeAction::List
                | EpisodeAction::Search
                | EpisodeAction::Get
                | EpisodeAction::History
                | EpisodeAction::References => RequestAccess::ReadOnly,
                EpisodeAction::Append | EpisodeAction::Revise => RequestAccess::Mutation,
            },
            Self::Concern(prepared) => {
                if prepared.request.is_mutation() {
                    RequestAccess::Mutation
                } else {
                    RequestAccess::ReadOnly
                }
            }
            Self::EditBody(_)
            | Self::EditSummary(_)
            | Self::Retag(_)
            | Self::Save(_)
            | Self::Capture { .. }
            | Self::Ingest
            | Self::Link
            | Self::Supersede
            | Self::Contradict
            | Self::Reconcile
            | Self::Feedback
            | Self::Merge
            | Self::Forget
            | Self::Decay
            | Self::Prune
            | Self::SnapshotCreate
            | Self::Reflect => RequestAccess::Mutation,
        }
    }

    fn verify_receipt(
        &self,
        result: &Value,
        database: &str,
        expected_db_id: Option<Ulid>,
    ) -> Result<(), AnyErr> {
        match self {
            Self::Save(save) => save
                .verify_write_receipt_json(result)
                .map_err(|error| -> AnyErr { error }),
            Self::Concern(prepared) => prepared
                .request
                .validate_routed_response_json(result, database, expected_db_id)
                .map_err(|error| -> AnyErr { error }),
            Self::Retag(prepared) => prepared
                .request
                .validate_routed_response_json(result, database, expected_db_id)
                .map_err(|error| -> AnyErr { error }),
            Self::EditBody(prepared) => prepared
                .request
                .validate_routed_response_json(result, database, expected_db_id)
                .map_err(|error| -> AnyErr { error }),
            Self::EditSummary(prepared) => prepared
                .request
                .validate_routed_response_json(result, database, expected_db_id)
                .map_err(|error| -> AnyErr { error }),
            Self::Status
            | Self::Core
            | Self::Query
            | Self::RecallContext
            | Self::Get
            | Self::Neighbors
            | Self::RemoteEdges
            | Self::Contradictions
            | Self::Merges
            | Self::List
            | Self::Walk
            | Self::Episode(_)
            | Self::Capture { .. }
            | Self::Ingest
            | Self::Link
            | Self::Supersede
            | Self::Contradict
            | Self::Reconcile
            | Self::Feedback
            | Self::Merge
            | Self::Forget
            | Self::Decay
            | Self::Prune
            | Self::SnapshotCreate
            | Self::Reflect => Ok(()),
        }
    }
}

pub(crate) struct Request {
    operation: PreparedOperation,
    arguments: Value,
}

impl Request {
    pub(super) fn new(operation: PreparedOperation, arguments: Value) -> Self {
        Self {
            operation,
            arguments,
        }
    }

    // Walk and reflect share owner admission with one-call commands, but their
    // session protocol and receipt consumption remain owned by remote_repl.
    pub(crate) fn walk(arguments: Value) -> Self {
        Self::new(PreparedOperation::Walk, arguments)
    }

    pub(crate) fn reflection(arguments: Value) -> Self {
        Self::new(PreparedOperation::Reflect, arguments)
    }

    pub(crate) fn tool(&self) -> &'static str {
        self.operation.tool()
    }

    pub(crate) fn access(&self) -> RequestAccess {
        self.operation.access()
    }

    pub(crate) fn arguments(&self) -> &Value {
        &self.arguments
    }

    pub(crate) fn guard_database(&mut self, expected_db_id: Ulid) {
        self.arguments["expected_db_id"] = json!(expected_db_id.to_string());
    }

    pub(crate) fn route_to(&mut self, database: &str) {
        self.arguments["db"] = json!(database);
    }

    pub(crate) fn into_arguments(self) -> Value {
        self.arguments
    }

    /// Move the exact admitted wire payload into the call while retaining only
    /// what response verification needs. The post-routing guard is frozen here.
    pub(crate) fn into_submission(self) -> Result<(Value, ResponseCheck), AnyErr> {
        let expected_db_id = self
            .arguments
            .get("expected_db_id")
            .and_then(Value::as_str)
            .map(str::parse::<Ulid>)
            .transpose()?;
        Ok((
            self.arguments,
            ResponseCheck {
                operation: self.operation,
                expected_db_id,
            },
        ))
    }

    /// Check only the explicit remote route's catalog contract. The configured
    /// route retains ConfiguredClient's own admission and error presentation.
    pub(crate) fn check_catalog(&self, client: &RemoteClient) -> Result<(), AnyErr> {
        match &self.operation {
            PreparedOperation::Save(save) => {
                let kind = save.kind().as_str();
                let has_links = self
                    .arguments
                    .get("links")
                    .and_then(Value::as_array)
                    .is_some_and(|links| !links.is_empty());
                if !client.supports_save_kind(kind)
                    || (has_links && !client.supports_save_links(kind))
                {
                    return Err("remote save is unavailable: the server does not advertise the requested canonical SAVE contract; use a SAVE-capable server (no capture/ingest fallback was attempted)".into());
                }
            }
            PreparedOperation::EditBody(_) if !client.supports_edit_body() => {
                return Err("remote edit-body requires a server advertising the checked edit_body contract; no fallback attempted".into());
            }
            PreparedOperation::EditSummary(_) if !client.supports_edit_summary() => {
                return Err("remote edit-summary requires a server advertising the checked edit_summary contract; no fallback attempted".into());
            }
            PreparedOperation::Retag(retag) => {
                if !client.supports_retag() {
                    return Err("remote retag requires a server advertising the complete checked retag contract; no fallback attempted".into());
                }
                if retag.request.requires_content_guards()
                    && !client.supports_retag_content_guards()
                {
                    return Err("remote retag content guards are unavailable on this owner; update the owner build. No unguarded fallback was attempted".into());
                }
            }
            PreparedOperation::Concern(concern)
                if !client.supports_concern_action(concern.request.action()) =>
            {
                return Err("remote concern requires a server advertising the checked concern contract; no fallback attempted".into());
            }
            PreparedOperation::Query
                if self.arguments.get("summary_max_bytes").is_some()
                    && !SummaryMaxBytes::advertised(client.tool_catalog()) =>
            {
                return Err("remote query summary_max_bytes is unavailable: update the owner build to advertise the summary bound, or omit --summary-max-bytes; no fallback attempted".into());
            }
            PreparedOperation::Capture {
                requires_links: true,
            } if !client.supports_capture_links() => {
                return Err("remote capture links are unavailable: the server does not advertise a compatible `links` schema; use a server that supports atomic capture links, or omit links".into());
            }
            _ => {}
        }
        if (self.arguments.get("touchstone").is_some()
            || (self.tool() == "list" && self.arguments["kind"] == "touchstones"))
            && !crate::remote_touchstone::advertised(client.tool_catalog(), self.tool())
        {
            return Err("remote touchstones are unavailable: the server does not advertise the native touchstone schema; no fallback or write attempted".into());
        }
        if self.tool() == "list"
            && self.arguments["kind"] == "nodes"
            && !crate::remote_touchstone::inventory_advertised(client.tool_catalog())
        {
            return Err("this MCP owner does not support node inventory yet; update the owner build, not just mnemed. No local database fallback was attempted".into());
        }
        if self.tool() == "list" && self.arguments["kind"] == "tags" && !client.supports_list_tags()
        {
            return Err("this MCP owner does not support tag vocabulary; update the owner build. No local database fallback was attempted".into());
        }
        Ok(())
    }
}

pub(crate) struct ResponseCheck {
    operation: PreparedOperation,
    expected_db_id: Option<Ulid>,
}

impl ResponseCheck {
    pub(crate) fn access(&self) -> RequestAccess {
        self.operation.access()
    }

    pub(crate) fn verify_configured(
        &self,
        result: &Value,
        database: &str,
        expected_db_id: Ulid,
    ) -> Result<(), AnyErr> {
        self.operation
            .verify_receipt(result, database, Some(expected_db_id))
            .map_err(|error| error.to_string().into())
    }

    pub(crate) fn verify_remote(&self, result: &Value, database: &str) -> Result<(), AnyErr> {
        // Legacy concern checks intentionally use the caller's original identity;
        // guarded edits use the identity attached to the submitted wire request.
        let expected_db_id = match &self.operation {
            PreparedOperation::Concern(concern) => concern.expected_db_id,
            _ => self.expected_db_id,
        };
        self.operation.verify_receipt(result, database, expected_db_id).map_err(|error| -> AnyErr {
            match &self.operation {
                PreparedOperation::Save(_) => format!("remote SAVE receipt could not be verified; submitted write outcome is unknown: {error}; do not blindly retry").into(),
                PreparedOperation::Concern(_) => format!("remote concern acknowledgement invalid: {error}; a submitted write may have completed; do not blindly retry").into(),
                PreparedOperation::Retag(_) => format!("remote retag acknowledgement invalid: {error}; submitted write outcome is unknown; do not blindly retry").into(),
                PreparedOperation::EditBody(_) => format!("remote edit-body acknowledgement invalid: {error}; submitted outcome is unknown; inspect before a new intent").into(),
                PreparedOperation::EditSummary(_) => format!("remote edit-summary acknowledgement invalid: {error}; submitted outcome is unknown; inspect before a new intent").into(),
                _ => error,
            }
        })?;
        if matches!(&self.operation, PreparedOperation::Save(_))
            && (result["db"].as_str() != Some(database)
                || result["db_id"]
                    .as_str()
                    .is_none_or(|id| id.parse::<Ulid>().is_err()))
        {
            return Err("remote SAVE receipt database mismatch; the submitted write outcome is unknown; do not blindly retry".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn checked_requests() -> Vec<(Request, Value)> {
        let body = crate::cli_edit_body::Prepared {
            request: mneme_app::edit_body::PreparedBodyEdit::parse(&json!({
                "id": NODE, "expected_body_revision": "a".repeat(64), "body": "replacement"
            }))
            .unwrap(),
            expected_db_id: None,
        };
        let summary = crate::cli_edit_summary::Prepared {
            request: mneme_app::edit_summary::PreparedSummaryEdit::parse(&json!({
                "id": NODE, "expected_snapshot_sha256": "a".repeat(64), "summary": "replacement"
            }))
            .unwrap(),
            expected_db_id: None,
        };
        let retag = crate::cli_retag::Prepared {
            request: mneme_app::retag::PreparedRetag::parse(&json!({
                "id": NODE, "expected_tags": ["old"], "tags": ["new"]
            }))
            .unwrap(),
            expected_db_id: None,
        };
        let concern = crate::cli_concern::PreparedConcern {
            request: mneme_app::concern::PreparedConcernRequest::parse(&json!({
                "action": "list", "endpoint": NODE
            }))
            .unwrap(),
            expected_db_id: None,
        };
        let body_payload = body.payload();
        let summary_payload = summary.payload();
        let retag_payload = retag.payload();
        let concern_payload = concern.payload();
        vec![
            (
                Request::new(PreparedOperation::EditBody(body), body_payload),
                json!({"id":NODE,"body_revision":"b".repeat(64)}),
            ),
            (
                Request::new(PreparedOperation::EditSummary(summary), summary_payload),
                json!({"id":NODE,"summary_snapshot_sha256":"b".repeat(64)}),
            ),
            (
                Request::new(PreparedOperation::Retag(retag), retag_payload),
                json!({"id":NODE,"tags":["new"],"changed":true}),
            ),
            (
                Request::new(
                    PreparedOperation::Concern(Box::new(concern)),
                    concern_payload,
                ),
                json!({"action":"list","page":{"items":[],"next":null}}),
            ),
        ]
    }

    #[test]
    fn route_changes_only_owner_fields_and_verifies_the_submitted_guard() {
        let expected = Ulid::from(42);
        for (mut request, mut reply) in checked_requests() {
            let tool = request.tool();
            let mut expected_payload = request.arguments().clone();
            expected_payload["db"] = json!("renamed");
            expected_payload["expected_db_id"] = json!(expected.to_string());
            request.guard_database(expected);
            request.route_to("renamed");
            let (payload, check) = request.into_submission().unwrap();
            assert_eq!(payload, expected_payload, "{tool}");
            reply["db"] = json!("owner-native-name");
            reply["db_id"] = json!(expected.to_string());
            check
                .verify_configured(&reply, "owner-native-name", expected)
                .unwrap();
            check.verify_remote(&reply, "owner-native-name").unwrap();
            assert!(
                check
                    .verify_configured(&reply, "renamed", expected)
                    .is_err()
            );
            reply["db_id"] = json!(Ulid::from(43).to_string());
            assert!(
                check
                    .verify_configured(&reply, "owner-native-name", expected)
                    .is_err()
            );
            // Explicit concern retains its original optional caller guard. The
            // owner wrapper checks enrolled mutation identity separately.
            assert_eq!(
                check.verify_remote(&reply, "owner-native-name").is_ok(),
                tool == "concern"
            );
        }
    }

    #[test]
    fn response_failures_keep_route_specific_recovery_messages() {
        let expected = Ulid::from(42);
        for (mut request, _) in checked_requests() {
            let tool = request.tool();
            request.guard_database(expected);
            let (_, check) = request.into_submission().unwrap();
            let cause = check
                .operation
                .verify_receipt(&json!({}), "project", Some(expected))
                .unwrap_err()
                .to_string();
            assert_eq!(
                check
                    .verify_configured(&json!({}), "project", expected)
                    .unwrap_err()
                    .to_string(),
                cause
            );
            let remote = check
                .verify_remote(&json!({}), "project")
                .unwrap_err()
                .to_string();
            let operation = tool.replace('_', "-");
            assert!(remote.starts_with(&format!(
                "remote {operation} acknowledgement invalid: {cause};"
            )));
            let recovery = match tool {
                "edit_body" | "edit_summary" => {
                    "submitted outcome is unknown; inspect before a new intent"
                }
                "retag" => "submitted write outcome is unknown; do not blindly retry",
                "concern" => "a submitted write may have completed; do not blindly retry",
                _ => unreachable!(),
            };
            assert!(remote.ends_with(recovery), "{remote}");
        }
    }

    #[test]
    fn save_nonce_and_receipt_contract_survive_routing() {
        let save = mneme_app::save::PreparedSave::parse(&json!({"summary":"note"}), "frozen-nonce")
            .unwrap();
        let mut receipt = json!({"kind":"note", "id":save.expected_id().unwrap(),
            "replayed":false, "origin":save.identity().origin.as_str(),
            "operation_id":save.identity().key});
        let payload = save.into_json();
        let save = mneme_app::save::PreparedSave::parse(&payload, "").unwrap();
        let mut request = Request::new(PreparedOperation::Save(Box::new(save)), payload.clone());
        request.guard_database(Ulid::from(42));
        request.route_to("renamed");
        let (mut routed, check) = request.into_submission().unwrap();
        routed.as_object_mut().unwrap().remove("db");
        routed.as_object_mut().unwrap().remove("expected_db_id");
        assert_eq!(routed, payload);
        assert_eq!(receipt["operation_id"], "frozen-nonce");
        // ConfiguredClient owns the database envelope; explicit remote owns its
        // separate db/db_id check after the shared SAVE proof succeeds.
        check
            .verify_configured(&receipt, "project", Ulid::from(42))
            .unwrap();
        assert_eq!(
            check
                .verify_remote(&receipt, "project")
                .unwrap_err()
                .to_string(),
            "remote SAVE receipt database mismatch; the submitted write outcome is unknown; do not blindly retry"
        );
        receipt["db"] = json!("project");
        receipt["db_id"] = json!(Ulid::from(42).to_string());
        check.verify_remote(&receipt, "project").unwrap();
        receipt["operation_id"] = json!("changed-nonce");
        assert_eq!(
            check
                .verify_configured(&receipt, "project", Ulid::from(42))
                .unwrap_err()
                .to_string(),
            "SAVE manual receipt operation ID mismatch"
        );
        assert_eq!(
            check
                .verify_remote(&receipt, "project")
                .unwrap_err()
                .to_string(),
            "remote SAVE receipt could not be verified; submitted write outcome is unknown: SAVE manual receipt operation ID mismatch; do not blindly retry"
        );
    }
}
