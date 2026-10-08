//! Checked advisory maintenance over an already admitted native owner.
//!
//! Preparation performs no reads, hashing of Python projections, or rebinding.
//! Hosts own database selection and authority. These operations neither save a
//! semantic lesson nor grant permission to reconcile, supersede, or merge.
//! Inspected external bytes are historical finding evidence, not a body CAS.

use mneme_core::{
    ConcernBinding, ConcernCommitOutcome, ConcernEndpoint, ConcernKey, ConcernKind, ConcernPage,
    ConcernPageRequest, ConcernRefusal, ConcernRow, ConcernTransition, ConcernUpdate, Node,
    ports::ConcernStore, transition_concern,
};
use serde_json::{Value, json};
use std::error::Error;

pub type ConcernError = Box<dyn Error + Send + Sync>;

/// An envelope fuse in addition to constructor-checked domain byte bounds.
/// Includes JSON escaping; it is not a bound on a transport's pre-parsed input.
pub const MAX_CONCERN_REQUEST_BYTES: usize = 64 * 1024;

// MCP frames cap at 512 KiB and text at 256 KiB. A response appears once as
// structuredContent and once as JSON text; escaping that text costs at most a
// further copy. Reserve 16 KiB for transport framing and 32 KiB inside the
// response for routing/control, including conservatively escaped CLI paths.
pub const MAX_CONCERN_RESPONSE_BYTES: usize = (512 * 1024 - 16 * 1024) / 3;
const CONCERN_ROUTING_CONTROL_BYTES: usize = 32 * 1024;
pub const MAX_CONCERN_PUBLIC_PAGE_ROWS: usize = {
    let by_bytes = (MAX_CONCERN_RESPONSE_BYTES - CONCERN_ROUTING_CONTROL_BYTES)
        / (mneme_core::MAX_CONCERN_ROW_JSON_BYTES + 1);
    if by_bytes < mneme_core::MAX_NODE_HYDRATION_BATCH {
        by_bytes
    } else {
        mneme_core::MAX_NODE_HYDRATION_BATCH
    }
};
const _: () = assert!(MAX_CONCERN_PUBLIC_PAGE_ROWS > 0);

/// One grouped native operation. The list bound is a transport chunk with a
/// live continuation cursor, never a bound on relevant cases or stored degree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreparedConcernRequest {
    List(ConcernPageRequest),
    Mutation(PreparedConcern),
}

impl PreparedConcernRequest {
    pub fn parse(raw: &Value) -> Result<Self, ConcernError> {
        if serde_json::to_vec(raw)?.len() > MAX_CONCERN_REQUEST_BYTES {
            return Err("concern input exceeds its encoded byte allowance".into());
        }
        let object = raw
            .as_object()
            .ok_or("concern arguments must be an object")?;
        if object.get("action").and_then(Value::as_str) == Some("list") {
            let mut page = object.clone();
            page.remove("action");
            page.entry("limit".to_owned())
                .or_insert_with(|| json!(MAX_CONCERN_PUBLIC_PAGE_ROWS));
            let request: ConcernPageRequest = serde_json::from_value(Value::Object(page))?;
            if request.limit() > MAX_CONCERN_PUBLIC_PAGE_ROWS {
                return Err(format!(
                    "concern list limit must be 1..={MAX_CONCERN_PUBLIC_PAGE_ROWS} rows to fit the response byte allowance; continue with the returned cursor"
                ).into());
            }
            Ok(Self::List(request))
        } else {
            Ok(Self::Mutation(PreparedConcern::parse(raw)?))
        }
    }

    pub fn action(&self) -> &'static str {
        match self {
            Self::List(_) => "list",
            Self::Mutation(request) => request.action(),
        }
    }

    pub fn is_mutation(&self) -> bool {
        matches!(self, Self::Mutation(_))
    }

    pub fn into_json(self) -> Value {
        match self {
            Self::List(request) => {
                let mut raw =
                    serde_json::to_value(request).expect("checked page request serializes");
                raw["action"] = json!("list");
                raw
            }
            Self::Mutation(request) => request.into_json(),
        }
    }

    /// Domain response only. The transport attaches its selected owner identity.
    pub async fn execute(&self, store: &dyn ConcernStore) -> Result<Value, ConcernError> {
        let result = match self {
            Self::List(request) => {
                json!({"action":"list", "page":read_concerns_for_endpoint(store, request).await?})
            }
            Self::Mutation(request) => {
                json!({"action":request.action(), "outcome":request.execute(store).await?})
            }
        };
        self.validate_response_json(&result)?;
        Ok(result)
    }

    /// Validate exactly the atomic native payload, not a later get. Hosts strip
    /// only db/db_id from their envelope before calling this method.
    pub fn validate_response_json(&self, raw: &Value) -> Result<(), ConcernError> {
        if serde_json::to_vec(raw)?.len() > MAX_CONCERN_RESPONSE_BYTES {
            return Err("concern response exceeds its encoded byte allowance".into());
        }
        let object = raw
            .as_object()
            .ok_or("concern response must be an object")?;
        let field = if self.is_mutation() {
            "outcome"
        } else {
            "page"
        };
        if object.len() != 2 || object.get("action").and_then(Value::as_str) != Some(self.action())
        {
            return Err("concern response action or closed envelope mismatch".into());
        }
        let payload = object
            .get(field)
            .ok_or("concern response payload is missing")?;
        match self {
            Self::List(request) => {
                let page: ConcernPage = serde_json::from_value(payload.clone())?;
                validate_endpoint_page(request, &page)?;
            }
            Self::Mutation(request) => {
                request.validate_result_json(payload)?;
            }
        }
        Ok(())
    }

    /// Validate the single native owner response. Refused outcomes remain
    /// refusals; owner mismatch never permits retry against a replacement.
    pub fn validate_routed_response_json(
        &self,
        raw: &Value,
        db: &str,
        expected_db_id: Option<ulid::Ulid>,
    ) -> Result<(), ConcernError> {
        if serde_json::to_vec(raw)?.len() > MAX_CONCERN_RESPONSE_BYTES {
            return Err("concern owner response exceeds its encoded byte allowance".into());
        }
        let object = raw
            .as_object()
            .ok_or("concern owner response must be an object")?;
        let id = object
            .get("db_id")
            .and_then(Value::as_str)
            .ok_or("concern response database identity is missing")?;
        let id: ulid::Ulid = id.parse()?;
        if object.get("db").and_then(Value::as_str) != Some(db)
            || raw["db_id"] != json!(id.to_string())
            || expected_db_id.is_some_and(|expected| expected != id)
        {
            return Err(
                "concern response database mismatch; do not retry against another owner".into(),
            );
        }
        let mut domain = object.clone();
        domain.remove("db");
        domain.remove("db_id");
        self.validate_response_json(&Value::Object(domain))
    }
}

/// Closed, profile-narrowable schema. Native constructors enforce UTF-8 bytes;
/// JSON Schema maxLength is only a character bound and does not replace them.
pub fn input_schema(actions: &[&str]) -> Value {
    let id = json!({"type":"string", "minLength":26, "maxLength":26});
    let digest = json!({"type":"string", "pattern":"^[0-9a-f]{64}$"});
    let kind = json!({"type":"string", "enum":["disagreement", "redundancy"]});
    let text = |max| {
        json!({"type":"string", "minLength":1, "maxLength":max,
        "description":format!("nonblank; at most {max} UTF-8 bytes (native admission)")})
    };
    let endpoint = json!({"type":"object", "additionalProperties":false,
        "required":["id","meaning"], "properties":{"id":id, "meaning":digest}});
    let key = json!({"type":"object", "additionalProperties":false,
        "required":["kind","lo","hi"], "properties":{"kind":kind, "lo":id, "hi":id}});
    let binding = json!({"type":"object", "additionalProperties":false,
        "required":["key","endpoints"], "properties":{"key":key,
        "endpoints":{"type":"array","minItems":2,"maxItems":2,"items":endpoint}}});
    let notice = json!({"type":"object", "additionalProperties":false,
        "required":["binding","concern","missing_fact"], "properties":{
        "binding":binding, "concern":text(mneme_core::MAX_CONCERN_BYTES),
        "missing_fact":text(mneme_core::MAX_CONCERN_MISSING_FACT_BYTES)}});
    let finding = json!({"type":"object", "additionalProperties":false,
        "required":["scope","observation","evidence"], "properties":{
        "scope":text(mneme_core::MAX_CONCERN_SCOPE_BYTES),
        "observation":text(mneme_core::MAX_CONCERN_FINDING_BYTES),
        "evidence":{"type":"array", "minItems":1,
        "maxItems":(mneme_core::MAX_CONCERN_EVIDENCE_BYTES - 16) / 49,
        "description":"historically inspected evidence; at most 1024 framed canonical bytes in total",
        "items":{"type":"object","additionalProperties":false,"required":["source_ref","digest"],
        "properties":{"source_ref":text(mneme_core::MAX_CONCERN_EVIDENCE_REF_BYTES),"digest":digest}}}}});
    let row = json!({"type":"object","additionalProperties":false,"required":["notice","finding"],
        "properties":{"notice":notice,"finding":{"anyOf":[{"type":"null"},finding]}}});
    let cursor = json!({"type":"object","additionalProperties":false,"required":["endpoint","other","kind"],
        "properties":{"endpoint":id,"other":id,"kind":kind}});
    let branch = |action: &str, required: &[&str], forbidden: &[&str]| {
        json!({
        "properties":{"action":{"const":action}}, "required":required,
        "not":{"anyOf":forbidden.iter().map(|field| json!({"required":[field]})).collect::<Vec<_>>()}})
    };
    let branches = [
        (
            "list",
            branch("list", &["endpoint"], &["notice", "expected", "finding"]),
        ),
        (
            "notice",
            branch(
                "notice",
                &["notice"],
                &["endpoint", "limit", "after", "expected", "finding"],
            ),
        ),
        (
            "record_finding",
            branch(
                "record_finding",
                &["expected", "finding"],
                &["endpoint", "limit", "after", "notice"],
            ),
        ),
    ]
    .into_iter()
    .filter_map(|(action, branch)| actions.contains(&action).then_some(branch))
    .collect::<Vec<_>>();
    json!({"type":"object","additionalProperties":false,"required":["action"],
        "properties":{"action":{"type":"string","enum":actions},
        "endpoint":id,"limit":{"type":"integer","minimum":1,"maximum":MAX_CONCERN_PUBLIC_PAGE_ROWS,"default":MAX_CONCERN_PUBLIC_PAGE_ROWS,
        "description":format!("response-byte-derived page allowance: 1..={MAX_CONCERN_PUBLIC_PAGE_ROWS}; follow next for more cases")},
        "after":{"anyOf":[{"type":"null"},cursor]},"notice":notice,"expected":row,"finding":finding},
        "oneOf":branches})
}

/// Human presentation of the same checked owner/domain envelope.
pub fn render_human(response: &Value) -> String {
    let owner = response["db"].as_str().unwrap_or("(owner absent)");
    let action = response["action"].as_str().unwrap_or("(action absent)");
    if action == "list" {
        let mut lines = vec![format!("concerns in {owner}")];
        if let Some(rows) = response["page"]["items"].as_array() {
            for row in rows {
                let key = &row["notice"]["binding"]["key"];
                lines.push(format!(
                    "{} {} <-> {}: {}",
                    key["kind"].as_str().unwrap_or("?"),
                    key["lo"].as_str().unwrap_or("?"),
                    key["hi"].as_str().unwrap_or("?"),
                    row["notice"]["concern"].as_str().unwrap_or("?")
                ));
                lines.push(format!(
                    "  missing fact: {}",
                    row["notice"]["missing_fact"].as_str().unwrap_or("?")
                ));
                if !row["finding"].is_null() {
                    lines.push(format!(
                        "  scoped [{}]: {}",
                        row["finding"]["scope"].as_str().unwrap_or("?"),
                        row["finding"]["observation"].as_str().unwrap_or("?")
                    ));
                }
            }
            if rows.is_empty() {
                lines.push("(no cases in this page)".into());
            }
        }
        if !response["page"]["next"].is_null() {
            lines.push(format!("next: {}", response["page"]["next"]));
        }
        lines.join("\n")
    } else {
        let outcome = &response["outcome"];
        let status = outcome["status"].as_str().unwrap_or("(status absent)");
        let refusal = outcome["reason"]
            .as_str()
            .map(|reason| {
                format!(": {reason}; inspect current meanings/case before forming new intent")
            })
            .unwrap_or_default();
        format!("concern {action} {status} in {owner}{refusal}")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedConcern {
    update: ConcernUpdate,
}

impl PreparedConcern {
    /// Parse only the domain payload. Hosts must remove their db/identity routing
    /// fields first; unknown fields and fields from another action are rejected.
    pub fn parse(raw: &Value) -> Result<Self, ConcernError> {
        if serde_json::to_vec(raw)?.len() > MAX_CONCERN_REQUEST_BYTES {
            return Err(
                format!("concern input exceeds {MAX_CONCERN_REQUEST_BYTES} encoded bytes").into(),
            );
        }
        Ok(Self {
            update: serde_json::from_value(raw.clone())?,
        })
    }

    pub fn action(&self) -> &'static str {
        match &self.update {
            ConcernUpdate::Notice(_) => "notice",
            ConcernUpdate::RecordScopedFinding { .. } => "record_finding",
        }
    }

    pub fn key(&self) -> ConcernKey {
        self.update.key()
    }

    pub fn binding(&self) -> ConcernBinding {
        self.update.binding()
    }

    pub fn update(&self) -> &ConcernUpdate {
        &self.update
    }

    pub fn into_update(self) -> ConcernUpdate {
        self.update
    }

    pub fn into_json(self) -> Value {
        serde_json::to_value(self.update).expect("checked concern update serializes")
    }

    /// Apply once and validate the exact transaction result. No post-commit
    /// get can replace this result: another writer may already have intervened.
    pub async fn execute(
        &self,
        store: &dyn ConcernStore,
    ) -> Result<ConcernCommitOutcome, ConcernError> {
        let outcome = store.update_concern(&self.update).await?;
        self.validate_result(&outcome)?;
        Ok(outcome)
    }

    /// Native/reflected transport results use the same checked domain serde.
    pub fn validate_result_json(&self, raw: &Value) -> Result<ConcernCommitOutcome, ConcernError> {
        if serde_json::to_vec(raw)?.len() > MAX_CONCERN_REQUEST_BYTES {
            return Err("concern result exceeds its bounded envelope".into());
        }
        let result = serde_json::from_value(raw.clone())?;
        self.validate_result(&result)?;
        Ok(result)
    }

    pub fn validate_result(&self, result: &ConcernCommitOutcome) -> Result<(), ConcernError> {
        let row = match result {
            ConcernCommitOutcome::Applied { row } | ConcernCommitOutcome::Unchanged { row } => {
                Some(row)
            }
            ConcernCommitOutcome::Refused { row, .. } => row.as_ref(),
        };
        if row.is_some_and(|row| row.binding().key() != self.key()) {
            return Err("concern result returned another pair or kind".into());
        }
        match result {
            ConcernCommitOutcome::Applied { row } | ConcernCommitOutcome::Unchanged { row } => {
                let expected = match &self.update {
                    ConcernUpdate::Notice(notice) => {
                        if matches!(result, ConcernCommitOutcome::Unchanged { .. }) {
                            // Same-meaning notice preserves the entire previous
                            // row, including differently worded text and finding.
                            if row.binding() != notice.binding() {
                                return Err("unchanged concern result has stale meanings".into());
                            }
                            return Ok(());
                        }
                        ConcernRow::from_notice(notice.clone())
                    }
                    ConcernUpdate::RecordScopedFinding { expected, .. } => {
                        match transition_concern(&expected.binding(), Some(expected), &self.update)
                        {
                            ConcernTransition::Replace(desired) => desired,
                            ConcernTransition::Unchanged => expected.clone(),
                            ConcernTransition::Refused(_) => {
                                return Err(
                                    "checked concern request cannot form desired row".into()
                                );
                            }
                        }
                    }
                };
                if row != &expected {
                    return Err("concern result does not contain the exact desired row".into());
                }
            }
            ConcernCommitOutcome::Refused { reason, row } => {
                if matches!(&self.update, ConcernUpdate::Notice(_))
                    && matches!(
                        reason,
                        ConcernRefusal::MissingRow | ConcernRefusal::StaleRow
                    )
                {
                    return Err("notice cannot require an existing or expected row".into());
                }
                if matches!(reason, ConcernRefusal::MissingRow) && row.is_some() {
                    return Err("missing-row refusal returned an existing row".into());
                }
                if let (
                    ConcernRefusal::StaleRow,
                    ConcernUpdate::RecordScopedFinding { expected, .. },
                ) = (reason, &self.update)
                {
                    let Some(current) = row else {
                        return Err("stale-row refusal has no current row".into());
                    };
                    if !matches!(
                        transition_concern(&expected.binding(), Some(current), &self.update),
                        ConcernTransition::Refused(ConcernRefusal::StaleRow)
                    ) {
                        return Err("stale-row refusal contradicts its returned row".into());
                    }
                }
            }
        }
        Ok(())
    }
}

/// Issue the canonical meaning inspected by a caller of an existing exact read.
/// This deliberately does not resolve a body or assert its current byte content.
pub fn endpoint_for_node(node: &Node) -> ConcernEndpoint {
    ConcernEndpoint::from_node(node)
}

pub fn binding_for_nodes(
    kind: ConcernKind,
    a: &Node,
    b: &Node,
) -> Result<ConcernBinding, ConcernError> {
    Ok(ConcernBinding::new(
        kind,
        endpoint_for_node(a),
        endpoint_for_node(b),
    )?)
}

/// Fetch an exact advisory row without inventing a new binding or a finding.
pub async fn read_concern(
    store: &dyn ConcernStore,
    key: &ConcernKey,
) -> Result<Option<ConcernRow>, ConcernError> {
    let row = store.get_concern(key).await?;
    if row.as_ref().is_some_and(|row| row.binding().key() != *key) {
        return Err("concern read returned another pair or kind".into());
    }
    Ok(row)
}

/// Read one indexed page beside an existing exact read or retrieval. This is a
/// live keyset window, not a snapshot/completeness claim across later requests.
pub async fn read_concerns_for_endpoint(
    store: &dyn ConcernStore,
    request: &ConcernPageRequest,
) -> Result<ConcernPage, ConcernError> {
    let page = store.concerns_for_endpoint(request).await?;
    validate_endpoint_page(request, &page)?;
    Ok(page)
}

pub fn validate_endpoint_page(
    request: &ConcernPageRequest,
    page: &ConcernPage,
) -> Result<(), ConcernError> {
    if page.items.len() > request.limit() {
        return Err("concern page exceeds requested row allowance".into());
    }
    let mut previous = request
        .after()
        .map(|cursor| (cursor.other(), cursor.kind()));
    for row in &page.items {
        let key = row.binding().key();
        let endpoints = key.endpoints();
        let other = if endpoints[0] == request.endpoint() {
            endpoints[1]
        } else if endpoints[1] == request.endpoint() {
            endpoints[0]
        } else {
            return Err("concern page returned a nonincident pair".into());
        };
        let position = (other, key.kind());
        if previous.is_some_and(|previous| position <= previous) {
            return Err("concern page is duplicated, unordered or before its cursor".into());
        }
        previous = Some(position);
    }
    if let Some(next) = &page.next {
        if next.endpoint() != request.endpoint()
            || page.items.len() != request.limit()
            || page.items.is_empty()
            || previous != Some((next.other(), next.kind()))
        {
            return Err("concern continuation does not name the last full-page row".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mneme_core::{
        BodyRef, ConcernDigest, ConcernEvidence, ConcernNotice, MAX_CONCERN_BYTES,
        MAX_CONCERN_EVIDENCE_REF_BYTES, MAX_CONCERN_FINDING_BYTES, MAX_CONCERN_MISSING_FACT_BYTES,
        MAX_CONCERN_SCOPE_BYTES, NodeId, NodeStatus, Provenance, ScopedConcernFinding,
        ports::routing_content_fingerprint,
    };
    use serde_json::json;
    use ulid::Ulid;

    fn binding() -> ConcernBinding {
        ConcernBinding::new(
            ConcernKind::Disagreement,
            ConcernEndpoint::new(NodeId(Ulid::from(1)), ConcernDigest::of_bytes(b"old")),
            ConcernEndpoint::new(NodeId(Ulid::from(2)), ConcernDigest::of_bytes(b"new")),
        )
        .unwrap()
    }

    fn notice() -> ConcernNotice {
        ConcernNotice::new(
            binding(),
            "Shutdown claims disagree",
            "Which binary is installed?",
        )
        .unwrap()
    }

    fn finding(scope: &str) -> ScopedConcernFinding {
        ScopedConcernFinding::new(
            scope,
            "The inspected binary includes the fix",
            vec![ConcernEvidence::new("tool://version", ConcernDigest::of_bytes(b"v2")).unwrap()],
        )
        .unwrap()
    }

    fn prepare(update: ConcernUpdate) -> PreparedConcern {
        PreparedConcern::parse(&serde_json::to_value(update).unwrap()).unwrap()
    }

    fn notice_input() -> Value {
        serde_json::to_value(ConcernUpdate::Notice(notice())).unwrap()
    }

    fn finding_request() -> PreparedConcern {
        prepare(ConcernUpdate::RecordScopedFinding {
            expected: ConcernRow::from_notice(notice()),
            finding: finding("installed v2"),
        })
    }

    fn desired(request: &PreparedConcern) -> ConcernRow {
        let ConcernUpdate::RecordScopedFinding { expected, .. } = request.update() else {
            panic!("expected finding request")
        };
        match transition_concern(&expected.binding(), Some(expected), request.update()) {
            ConcernTransition::Replace(row) => row,
            other => panic!("expected replacement, got {other:?}"),
        }
    }

    #[test]
    fn grouped_list_defaults_roundtrips_and_rejects_sibling_or_overbudget_fields() {
        let endpoint = binding().key().endpoints()[0];
        let prepared =
            PreparedConcernRequest::parse(&json!({"action":"list","endpoint":endpoint})).unwrap();
        let PreparedConcernRequest::List(page) = &prepared else {
            panic!("list")
        };
        assert_eq!(page.limit(), MAX_CONCERN_PUBLIC_PAGE_ROWS);
        assert_eq!(
            PreparedConcernRequest::parse(&prepared.clone().into_json()).unwrap(),
            prepared
        );
        for raw in [
            json!({"action":"list","endpoint":endpoint,"limit":MAX_CONCERN_PUBLIC_PAGE_ROWS+1}),
            json!({"action":"list","endpoint":endpoint,"limit":0}),
            json!({"action":"list","endpoint":endpoint,"limit":null}),
            json!({"action":"list","endpoint":endpoint,"notice":notice()}),
            json!({"action":"list","endpoint":endpoint,"db":"user"}),
        ] {
            assert!(PreparedConcernRequest::parse(&raw).is_err(), "{raw}");
        }
        let schema = input_schema(&["list"]);
        assert_eq!(
            schema["properties"]["limit"]["maximum"],
            json!(MAX_CONCERN_PUBLIC_PAGE_ROWS)
        );
        assert_eq!(
            schema["properties"]["limit"]["default"],
            json!(page.limit())
        );
        assert_eq!(schema["properties"]["action"]["enum"], json!(["list"]));
    }

    #[test]
    fn owner_envelope_is_closed_canonical_and_not_rebound() {
        let request = PreparedConcernRequest::parse(&notice_input()).unwrap();
        let row = ConcernRow::from_notice(notice());
        let id = Ulid::from(29);
        let ack = json!({"db":"project","db_id":id.to_string(),"action":"notice",
            "outcome":{"status":"applied","row":row}});
        request
            .validate_routed_response_json(&ack, "project", Some(id))
            .unwrap();
        for (field, value) in [
            ("db", json!("other")),
            ("db_id", json!(Ulid::from(30).to_string())),
            ("action", json!("record_finding")),
            ("retryable", json!(true)),
        ] {
            let mut bad = ack.clone();
            bad[field] = value;
            assert!(
                request
                    .validate_routed_response_json(&bad, "project", Some(id))
                    .is_err()
            );
        }
        let rendered = render_human(&ack);
        assert!(rendered.contains("project"));
        assert!(rendered.contains("applied"));
        let refused = json!({"db":"project","db_id":id.to_string(),"action":"notice",
            "outcome":{"status":"refused","reason":"missing_endpoint","row":null}});
        request
            .validate_routed_response_json(&refused, "project", Some(id))
            .unwrap();
        assert!(render_human(&refused).contains("missing_endpoint"));
    }

    #[test]
    fn canonical_actions_roundtrip_without_save_or_owner_fields() {
        for request in [prepare(ConcernUpdate::Notice(notice())), finding_request()] {
            let raw = request.clone().into_json();
            assert_eq!(raw["action"], request.action());
            assert_eq!(PreparedConcern::parse(&raw).unwrap(), request);
            assert_eq!(request.key(), binding().key());
            assert!(raw.get("source").is_none());
            assert!(raw.get("summary").is_none());
            assert!(raw.get("db").is_none());
        }
    }

    #[test]
    fn rejects_unknown_fields_wrong_action_shapes_and_ownership() {
        for field in [
            "db",
            "expected_db_id",
            "summary",
            "finding",
            "expected",
            "semantic_lesson",
        ] {
            let mut raw = notice_input();
            raw[field] = json!("not domain data");
            assert!(PreparedConcern::parse(&raw).is_err(), "accepted {field}");
        }
        for action in [
            "defer",
            "supersede",
            "merge",
            "record_scoped_finding",
            "save",
        ] {
            let mut raw = notice_input();
            raw["action"] = json!(action);
            assert!(PreparedConcern::parse(&raw).is_err(), "accepted {action}");
        }
        let mut raw = notice_input();
        raw["notice"]["authority"] = json!("operator");
        assert!(PreparedConcern::parse(&raw).is_err());
        let mut raw = notice_input();
        raw["notice"]["binding"]["endpoints"][0]["inspected_content"] = json!("00".repeat(32));
        assert!(PreparedConcern::parse(&raw).is_err());
        let mut raw = finding_request().into_json();
        raw["notice"] = notice_input()["notice"].clone();
        assert!(PreparedConcern::parse(&raw).is_err());
        for raw in [
            Value::Null,
            json!([]),
            json!({"action":"notice", "notice":null}),
        ] {
            assert!(PreparedConcern::parse(&raw).is_err());
        }
    }

    #[test]
    fn checked_serde_enforces_utf8_bytes_not_character_counts() {
        for (field, max) in [
            ("concern", MAX_CONCERN_BYTES),
            ("missing_fact", MAX_CONCERN_MISSING_FACT_BYTES),
        ] {
            let mut raw = notice_input();
            raw["notice"][field] = json!("é".repeat(max / 2));
            assert!(PreparedConcern::parse(&raw).is_ok());
            raw["notice"][field] = json!("é".repeat(max / 2 + 1));
            assert!(PreparedConcern::parse(&raw).is_err());
            raw["notice"][field] = json!(" \n ");
            assert!(PreparedConcern::parse(&raw).is_err());
        }
        for (field, max) in [
            ("scope", MAX_CONCERN_SCOPE_BYTES),
            ("observation", MAX_CONCERN_FINDING_BYTES),
        ] {
            let mut raw = finding_request().into_json();
            raw["finding"][field] = json!("é".repeat(max / 2 + 1));
            assert!(PreparedConcern::parse(&raw).is_err());
        }
        let mut raw = finding_request().into_json();
        raw["finding"]["evidence"][0]["source_ref"] =
            json!("x".repeat(MAX_CONCERN_EVIDENCE_REF_BYTES + 1));
        assert!(PreparedConcern::parse(&raw).is_err());
        let mut raw = finding_request().into_json();
        raw["finding"]["evidence"] = json!([]);
        assert!(PreparedConcern::parse(&raw).is_err());
        let mut raw = finding_request().into_json();
        raw["finding"]["evidence"][0]["digest"] = json!("not a digest");
        assert!(PreparedConcern::parse(&raw).is_err());
        let mut raw = notice_input();
        raw["notice"]["concern"] = json!("x".repeat(MAX_CONCERN_REQUEST_BYTES));
        assert!(PreparedConcern::parse(&raw).is_err());
    }

    #[test]
    fn validates_exact_transaction_row_without_post_commit_rebinding() {
        let request = finding_request();
        let row = desired(&request);
        for result in [
            ConcernCommitOutcome::Applied { row: row.clone() },
            ConcernCommitOutcome::Unchanged { row: row.clone() },
        ] {
            assert!(request.validate_result(&result).is_ok());
            assert_eq!(
                request
                    .validate_result_json(&serde_json::to_value(&result).unwrap())
                    .unwrap(),
                result
            );
        }
        assert!(
            request
                .validate_result(&ConcernCommitOutcome::Applied {
                    row: ConcernRow::from_notice(notice())
                })
                .is_err()
        );
        let request = prepare(ConcernUpdate::Notice(notice()));
        assert!(
            request
                .validate_result(&ConcernCommitOutcome::Applied { row: row.clone() })
                .is_err()
        );
        assert!(
            request
                .validate_result(&ConcernCommitOutcome::Unchanged { row })
                .is_ok()
        );
        let reworded = ConcernNotice::new(binding(), "Different phrasing", "Same fact").unwrap();
        assert!(
            request
                .validate_result(&ConcernCommitOutcome::Unchanged {
                    row: ConcernRow::from_notice(reworded)
                })
                .is_ok()
        );
        let result = json!({"status":"applied", "row":null});
        assert!(request.validate_result_json(&result).is_err());
    }

    #[test]
    fn native_refusals_stay_refusals_and_inconsistent_rows_are_rejected() {
        let request = finding_request();
        let missing = ConcernCommitOutcome::Refused {
            reason: ConcernRefusal::MissingRow,
            row: None,
        };
        assert!(request.validate_result(&missing).is_ok());
        assert_eq!(
            request
                .validate_result_json(&serde_json::to_value(&missing).unwrap())
                .unwrap(),
            missing
        );
        assert!(
            request
                .validate_result(&ConcernCommitOutcome::Refused {
                    reason: ConcernRefusal::MissingRow,
                    row: Some(ConcernRow::from_notice(notice()))
                })
                .is_err()
        );
        assert!(
            request
                .validate_result(&ConcernCommitOutcome::Refused {
                    reason: ConcernRefusal::StaleRow,
                    row: Some(desired(&request))
                })
                .is_err()
        );
        let other = prepare(ConcernUpdate::RecordScopedFinding {
            expected: ConcernRow::from_notice(notice()),
            finding: finding("another task"),
        });
        assert!(
            request
                .validate_result(&ConcernCommitOutcome::Refused {
                    reason: ConcernRefusal::StaleRow,
                    row: Some(desired(&other))
                })
                .is_ok()
        );
        let changed_kind = ConcernNotice::new(
            ConcernBinding::new(
                ConcernKind::Redundancy,
                binding().endpoints()[0],
                binding().endpoints()[1],
            )
            .unwrap(),
            "Different kind",
            "What differs?",
        )
        .unwrap();
        assert!(
            request
                .validate_result(&ConcernCommitOutcome::Refused {
                    reason: ConcernRefusal::StaleMeanings,
                    row: Some(ConcernRow::from_notice(changed_kind))
                })
                .is_err()
        );
    }

    #[test]
    fn endpoint_pages_preserve_total_allowance_order_and_continuation() {
        use mneme_core::ConcernPageCursor;
        let endpoint = binding().endpoints()[0].id();
        let other = binding().endpoints()[1].id();
        let cursor = ConcernPageCursor::new(endpoint, other, ConcernKind::Disagreement).unwrap();
        let request = ConcernPageRequest::new(endpoint, 1, None).unwrap();
        let row = ConcernRow::from_notice(notice());
        let page = ConcernPage {
            items: vec![row.clone()],
            next: Some(cursor),
        };
        assert!(validate_endpoint_page(&request, &page).is_ok());
        assert!(
            validate_endpoint_page(
                &request,
                &ConcernPage {
                    items: vec![],
                    next: None
                }
            )
            .is_ok()
        );
        assert!(
            validate_endpoint_page(
                &request,
                &ConcernPage {
                    items: vec![row.clone(), row.clone()],
                    next: None
                }
            )
            .is_err()
        );
        assert!(
            validate_endpoint_page(
                &ConcernPageRequest::new(endpoint, 2, None).unwrap(),
                &ConcernPage {
                    items: vec![row.clone(), row.clone()],
                    next: None
                }
            )
            .is_err()
        );
        assert!(
            validate_endpoint_page(
                &ConcernPageRequest::new(endpoint, 1, Some(cursor)).unwrap(),
                &ConcernPage {
                    items: vec![row.clone()],
                    next: None
                }
            )
            .is_err()
        );
        assert!(
            validate_endpoint_page(
                &request,
                &ConcernPage {
                    items: vec![],
                    next: Some(cursor)
                }
            )
            .is_err()
        );
        let wrong_endpoint = ConcernPageRequest::new(NodeId(Ulid::from(3)), 1, None).unwrap();
        assert!(validate_endpoint_page(&wrong_endpoint, &page).is_err());
        let wrong_cursor =
            ConcernPageCursor::new(endpoint, NodeId(Ulid::from(3)), ConcernKind::Disagreement)
                .unwrap();
        assert!(
            validate_endpoint_page(
                &request,
                &ConcernPage {
                    items: vec![row],
                    next: Some(wrong_cursor)
                }
            )
            .is_err()
        );
    }

    #[test]
    fn read_helpers_issue_native_meaning_without_double_hash_or_external_bytes() {
        let node = Node::try_new(
            NodeId(Ulid::from(1)),
            "Claim",
            BodyRef::new("file:///mutable").unwrap(),
            ["fact"],
            Provenance::Conversation {
                session: Ulid::from(3),
                turn: 1,
            },
            1.0,
            1.0,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        let endpoint = endpoint_for_node(&node);
        assert_eq!(
            serde_json::to_value(endpoint).unwrap()["meaning"],
            routing_content_fingerprint(&node)
        );
        let mut telemetry = node.clone();
        telemetry.record_grounded_use(100);
        assert_eq!(endpoint_for_node(&telemetry), endpoint);
        assert!(binding_for_nodes(ConcernKind::Disagreement, &node, &node).is_err());
        assert_eq!(
            serde_json::to_value(endpoint)
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn native_execution_keeps_advisory_maintenance_independent_of_save() {
        use mneme_core::ports::GraphStore;
        use mneme_cozo::MemStore;
        let node = |id: u128, summary: &str| {
            Node::try_new(
                NodeId(Ulid::from(id)),
                summary,
                BodyRef::new("file:///mutable").unwrap(),
                ["fact"],
                Provenance::derived_empty(),
                1.0,
                1.0,
                NodeStatus::Active,
                1,
            )
            .unwrap()
        };
        let store = MemStore::new(1);
        let a = node(1, "Old shutdown claim");
        let b = node(2, "New shutdown claim");
        store.put_node(&a).await.unwrap();
        store.put_node(&b).await.unwrap();
        let concern_store = store
            .concerns()
            .expect("reference backend supports concerns");
        let request = prepare(ConcernUpdate::Notice(
            ConcernNotice::new(
                binding_for_nodes(ConcernKind::Disagreement, &a, &b).unwrap(),
                "Shutdown claims disagree",
                "Which binary is installed?",
            )
            .unwrap(),
        ));
        let ConcernCommitOutcome::Applied { row } = request.execute(concern_store).await.unwrap()
        else {
            panic!("first notice must apply")
        };
        let record = prepare(ConcernUpdate::RecordScopedFinding {
            expected: row,
            finding: finding("installed v2"),
        });
        let ConcernCommitOutcome::Applied { row } = record.execute(concern_store).await.unwrap()
        else {
            panic!("finding must apply")
        };
        assert_eq!(
            read_concern(concern_store, &request.key()).await.unwrap(),
            Some(row.clone())
        );
        assert_eq!(
            record.execute(concern_store).await.unwrap(),
            ConcernCommitOutcome::Unchanged { row: row.clone() }
        );
        assert_eq!(
            request.execute(concern_store).await.unwrap(),
            ConcernCommitOutcome::Unchanged { row: row.clone() }
        );
        let page = read_concerns_for_endpoint(
            concern_store,
            &ConcernPageRequest::new(a.id(), 1, None).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(page.items, [row.clone()]);
        assert!(page.next.is_none());
        let later = prepare(ConcernUpdate::RecordScopedFinding {
            expected: row,
            finding: finding("another task"),
        });
        assert!(matches!(
            later.execute(concern_store).await.unwrap(),
            ConcernCommitOutcome::Applied { .. }
        ));
        assert!(matches!(
            record.execute(concern_store).await.unwrap(),
            ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::StaleRow,
                ..
            }
        ));
        store
            .put_node(&node(1, "Corrected shutdown claim"))
            .await
            .unwrap();
        assert!(matches!(
            request.execute(concern_store).await.unwrap(),
            ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::StaleMeanings,
                ..
            }
        ));
        // No semantic learning, body resolution, embeddings, or SAVE was needed.
        assert_eq!(
            store.get_node(a.id()).await.unwrap().unwrap().summary(),
            "Corrected shutdown claim"
        );
        assert!(
            store
                .get_node(NodeId(Ulid::from(3)))
                .await
                .unwrap()
                .is_none()
        );
    }
}
