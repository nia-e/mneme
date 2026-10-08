//! Bounded advisory pair concerns. Pure transitions only: no storage, scheduling,
//! observation counts, or authority to supersede/merge a memory.
//!
//! A backend must read current meanings and the row and apply the returned
//! replacement atomically. These functions alone provide no persistence proof.

use crate::{Node, NodeId};
use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

pub const MAX_CONCERN_BYTES: usize = 512;
pub const MAX_CONCERN_MISSING_FACT_BYTES: usize = 256;
pub const MAX_CONCERN_SCOPE_BYTES: usize = 512;
pub const MAX_CONCERN_FINDING_BYTES: usize = 1_024;
pub const MAX_CONCERN_EVIDENCE_REF_BYTES: usize = 256;
/// Aggregate canonical evidence payload budget, not a citation-count policy.
/// Includes the framed item count and each framed UTF-8 reference and digest.
pub const MAX_CONCERN_EVIDENCE_BYTES: usize = 1_024;

/// Conservative encoded-row fuse shared by storage and public page admission.
/// Raw text budgets total at most 3328 bytes, with JSON escaping at most 6x;
/// evidence framing also bounds its count. This generous ceiling is not a
/// typical row size, degree limit, or semantic relevance policy. A tighter
/// proved wire bound may widen public pages without changing storage semantics.
pub const MAX_CONCERN_ROW_JSON_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConcernValidationError {
    #[error("concern endpoints must differ")]
    SameEndpoint,
    #[error("concern endpoints must be stored in canonical order")]
    NonCanonicalEndpoints,
    #[error("concern binding endpoints disagree with its key")]
    BindingMismatch,
    #[error("concern digest must be exactly 64 lowercase hexadecimal characters")]
    InvalidDigest,
    #[error("concern page limit must be 1..={}", crate::MAX_NODE_HYDRATION_BATCH)]
    PageLimit,
    #[error("concern cursor belongs to a different endpoint")]
    CursorEndpoint,
    #[error("concern text must be nonblank and at most {limit} UTF-8 bytes")]
    TextBounds { limit: usize },
    #[error(
        "scoped evidence must be nonempty and fit {MAX_CONCERN_EVIDENCE_BYTES} canonical bytes"
    )]
    EvidenceBounds,
}

/// Opaque content digest, not a claim that external content is immutable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConcernDigest([u8; 32]);

impl ConcernDigest {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_hex(text: &str) -> Result<Self, ConcernValidationError> {
        if text.len() != 64
            || !text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ConcernValidationError::InvalidDigest);
        }
        let mut bytes = [0; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                .map_err(|_| ConcernValidationError::InvalidDigest)?;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for ConcernDigest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let text: String = self.0.iter().map(|byte| format!("{byte:02x}")).collect();
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for ConcernDigest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_hex(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcernKind {
    Disagreement,
    Redundancy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ConcernKey {
    lo: NodeId,
    hi: NodeId,
    kind: ConcernKind,
}

impl ConcernKey {
    pub fn new(kind: ConcernKind, a: NodeId, b: NodeId) -> Result<Self, ConcernValidationError> {
        if a == b {
            return Err(ConcernValidationError::SameEndpoint);
        }
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        Ok(Self { kind, lo, hi })
    }

    pub fn kind(&self) -> ConcernKind {
        self.kind
    }

    pub fn endpoints(&self) -> [NodeId; 2] {
        [self.lo, self.hi]
    }

    pub fn other(&self, endpoint: NodeId) -> Option<NodeId> {
        if endpoint == self.lo {
            Some(self.hi)
        } else if endpoint == self.hi {
            Some(self.lo)
        } else {
            None
        }
    }
}

impl<'de> Deserialize<'de> for ConcernKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: ConcernKind,
            lo: NodeId,
            hi: NodeId,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.lo > wire.hi {
            return Err(serde::de::Error::custom(
                ConcernValidationError::NonCanonicalEndpoints,
            ));
        }
        Self::new(wire.kind, wire.lo, wire.hi).map_err(serde::de::Error::custom)
    }
}

/// Canonical native node projection. Excludes telemetry; binds the body reference,
/// not arbitrary external bytes. Historical inspection hashes belong in evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcernEndpoint {
    id: NodeId,
    meaning: ConcernDigest,
}

impl ConcernEndpoint {
    pub fn new(id: NodeId, meaning: ConcernDigest) -> Self {
        Self { id, meaning }
    }

    pub fn from_node(node: &Node) -> Self {
        let meaning = ConcernDigest::from_hex(&crate::ports::routing_content_fingerprint(node))
            .expect("native routing fingerprint is canonical SHA-256");
        Self::new(node.id(), meaning)
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn meaning(&self) -> ConcernDigest {
        self.meaning
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ConcernBinding {
    key: ConcernKey,
    endpoints: [ConcernEndpoint; 2],
}

impl ConcernBinding {
    pub fn new(
        kind: ConcernKind,
        a: ConcernEndpoint,
        b: ConcernEndpoint,
    ) -> Result<Self, ConcernValidationError> {
        let key = ConcernKey::new(kind, a.id, b.id)?;
        let endpoints = if a.id < b.id { [a, b] } else { [b, a] };
        Ok(Self { key, endpoints })
    }

    pub fn key(&self) -> ConcernKey {
        self.key
    }

    pub fn endpoints(&self) -> &[ConcernEndpoint; 2] {
        &self.endpoints
    }
}

impl<'de> Deserialize<'de> for ConcernBinding {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            key: ConcernKey,
            endpoints: [ConcernEndpoint; 2],
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.endpoints[0].id >= wire.endpoints[1].id {
            return Err(serde::de::Error::custom(
                ConcernValidationError::NonCanonicalEndpoints,
            ));
        }
        if [wire.endpoints[0].id, wire.endpoints[1].id] != wire.key.endpoints() {
            return Err(serde::de::Error::custom(
                ConcernValidationError::BindingMismatch,
            ));
        }
        Self::new(wire.key.kind(), wire.endpoints[0], wire.endpoints[1])
            .map_err(serde::de::Error::custom)
    }
}

/// Constructor-checked UTF-8 byte bound. No unchecked deserialization path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConcernText<const LIMIT: usize>(String);

impl<const LIMIT: usize> ConcernText<LIMIT> {
    pub fn new(text: impl Into<String>) -> Result<Self, ConcernValidationError> {
        let text = text.into();
        if text.trim().is_empty() || text.len() > LIMIT {
            return Err(ConcernValidationError::TextBounds { limit: LIMIT });
        }
        Ok(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<const LIMIT: usize> Serialize for ConcernText<LIMIT> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de, const LIMIT: usize> Deserialize<'de> for ConcernText<LIMIT> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A suspicion/question, not a verified inspection or the sole disagreement in
/// multi-claim notes. Fresh reader text need not repeat this first stored hint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcernNotice {
    binding: ConcernBinding,
    concern: ConcernText<MAX_CONCERN_BYTES>,
    missing_fact: ConcernText<MAX_CONCERN_MISSING_FACT_BYTES>,
}

impl ConcernNotice {
    pub fn new(
        binding: ConcernBinding,
        concern: impl Into<String>,
        missing_fact: impl Into<String>,
    ) -> Result<Self, ConcernValidationError> {
        Ok(Self {
            binding,
            concern: ConcernText::new(concern)?,
            missing_fact: ConcernText::new(missing_fact)?,
        })
    }

    pub fn binding(&self) -> ConcernBinding {
        self.binding
    }
    pub fn concern(&self) -> &str {
        self.concern.as_str()
    }
    pub fn missing_fact(&self) -> &str {
        self.missing_fact.as_str()
    }
}

/// Host-bound source reference and inspected evidence digest. Neither certifies
/// entailment, truth, or another agent's authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcernEvidence {
    source_ref: ConcernText<MAX_CONCERN_EVIDENCE_REF_BYTES>,
    digest: ConcernDigest,
}

impl ConcernEvidence {
    pub fn new(
        source_ref: impl Into<String>,
        digest: ConcernDigest,
    ) -> Result<Self, ConcernValidationError> {
        Ok(Self {
            source_ref: ConcernText::new(source_ref)?,
            digest,
        })
    }

    pub fn source_ref(&self) -> &str {
        self.source_ref.as_str()
    }
    pub fn digest(&self) -> ConcernDigest {
        self.digest
    }
}

/// The latest scoped observation is a cache, not an applicability history or
/// global resolution. Body-dependent findings remain observations of inspected
/// content even when an external body later changes without changing its URI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScopedConcernFinding {
    scope: ConcernText<MAX_CONCERN_SCOPE_BYTES>,
    observation: ConcernText<MAX_CONCERN_FINDING_BYTES>,
    evidence: Vec<ConcernEvidence>,
}

impl ScopedConcernFinding {
    pub fn new(
        scope: impl Into<String>,
        observation: impl Into<String>,
        evidence: Vec<ConcernEvidence>,
    ) -> Result<Self, ConcernValidationError> {
        if evidence.is_empty() {
            return Err(ConcernValidationError::EvidenceBounds);
        }
        // Match fingerprint framing: one length-prefixed u64 count, then a
        // length-prefixed reference and length-prefixed SHA-256 per item.
        // Check incrementally; excess input never becomes a retained finding.
        let mut evidence_bytes = 8 + 8;
        for item in &evidence {
            evidence_bytes += 8 + item.source_ref().len() + 8 + 32;
            if evidence_bytes > MAX_CONCERN_EVIDENCE_BYTES {
                return Err(ConcernValidationError::EvidenceBounds);
            }
        }
        Ok(Self {
            scope: ConcernText::new(scope)?,
            observation: ConcernText::new(observation)?,
            evidence,
        })
    }

    pub fn scope(&self) -> &str {
        self.scope.as_str()
    }
    pub fn observation(&self) -> &str {
        self.observation.as_str()
    }
    pub fn evidence(&self) -> &[ConcernEvidence] {
        &self.evidence
    }
}

fn deserialize_evidence<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ConcernEvidence>, D::Error> {
    struct EvidenceVisitor;
    impl<'de> serde::de::Visitor<'de> for EvidenceVisitor {
        type Value = Vec<ConcernEvidence>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("nonempty evidence within its canonical byte budget")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut evidence = Vec::new();
            let mut bytes = 16;
            while let Some(item) = seq.next_element::<ConcernEvidence>()? {
                bytes += 48 + item.source_ref().len();
                if bytes > MAX_CONCERN_EVIDENCE_BYTES {
                    return Err(serde::de::Error::custom(
                        ConcernValidationError::EvidenceBounds,
                    ));
                }
                evidence.push(item);
            }
            if evidence.is_empty() {
                return Err(serde::de::Error::custom(
                    ConcernValidationError::EvidenceBounds,
                ));
            }
            Ok(evidence)
        }
    }
    deserializer.deserialize_seq(EvidenceVisitor)
}

impl<'de> Deserialize<'de> for ScopedConcernFinding {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            scope: ConcernText<MAX_CONCERN_SCOPE_BYTES>,
            observation: ConcernText<MAX_CONCERN_FINDING_BYTES>,
            #[serde(deserialize_with = "deserialize_evidence")]
            evidence: Vec<ConcernEvidence>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.scope.0, wire.observation.0, wire.evidence).map_err(serde::de::Error::custom)
    }
}

/// Latest finding may answer a later reader-local question rather than the first
/// notice. No terminal pair resolution, question history, or revision counter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcernRow {
    notice: ConcernNotice,
    #[serde(deserialize_with = "deserialize_required_option")]
    finding: Option<ScopedConcernFinding>,
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

impl ConcernRow {
    pub fn from_notice(notice: ConcernNotice) -> Self {
        Self {
            notice,
            finding: None,
        }
    }

    pub fn binding(&self) -> ConcernBinding {
        self.notice.binding
    }
    pub fn notice(&self) -> &ConcernNotice {
        &self.notice
    }
    pub fn finding(&self) -> Option<&ScopedConcernFinding> {
        self.finding.as_ref()
    }

    /// Canonical backend CAS projection, derived from the sole authoritative
    /// expected snapshot. Never an independently supplied expectation field.
    pub fn fingerprint(&self) -> ConcernDigest {
        fn field(hash: &mut Sha256, bytes: &[u8]) {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        let mut hash = Sha256::new();
        field(&mut hash, b"mneme.advisory-concern.v1");
        field(
            &mut hash,
            &[match self.binding().key.kind {
                ConcernKind::Disagreement => 0,
                ConcernKind::Redundancy => 1,
            }],
        );
        for endpoint in self.binding().endpoints {
            field(&mut hash, &endpoint.id.0.to_bytes());
            field(&mut hash, endpoint.meaning.as_bytes());
        }
        field(&mut hash, self.notice.concern().as_bytes());
        field(&mut hash, self.notice.missing_fact().as_bytes());
        field(&mut hash, &[u8::from(self.finding.is_some())]);
        if let Some(finding) = &self.finding {
            field(&mut hash, finding.scope().as_bytes());
            field(&mut hash, finding.observation().as_bytes());
            field(&mut hash, &(finding.evidence.len() as u64).to_le_bytes());
            for evidence in &finding.evidence {
                field(&mut hash, evidence.source_ref().as_bytes());
                field(&mut hash, evidence.digest.as_bytes());
            }
        }
        ConcernDigest(hash.finalize().into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConcernUpdate {
    Notice(ConcernNotice),
    RecordScopedFinding {
        expected: ConcernRow,
        finding: ScopedConcernFinding,
    },
}

impl ConcernUpdate {
    pub fn binding(&self) -> ConcernBinding {
        match self {
            Self::Notice(notice) => notice.binding(),
            Self::RecordScopedFinding { expected, .. } => expected.binding(),
        }
    }

    pub fn key(&self) -> ConcernKey {
        self.binding().key()
    }
}

impl Serialize for ConcernUpdate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(tag = "action", rename_all = "snake_case")]
        enum Wire<'a> {
            Notice {
                notice: &'a ConcernNotice,
            },
            RecordFinding {
                expected: &'a ConcernRow,
                finding: &'a ScopedConcernFinding,
            },
        }
        match self {
            Self::Notice(notice) => Wire::Notice { notice },
            Self::RecordScopedFinding { expected, finding } => {
                Wire::RecordFinding { expected, finding }
            }
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ConcernUpdate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Notice {
                notice: ConcernNotice,
            },
            RecordFinding {
                expected: ConcernRow,
                finding: ScopedConcernFinding,
            },
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Notice { notice } => Self::Notice(notice),
            Wire::RecordFinding { expected, finding } => {
                Self::RecordScopedFinding { expected, finding }
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcernRefusal {
    WrongKey,
    StaleMeanings,
    MissingRow,
    StaleRow,
    MissingEndpoint,
    InactiveEndpoint,
}

/// Exact resulting advisory row, including on refusal when a row exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConcernCommitOutcome {
    Applied {
        row: ConcernRow,
    },
    Unchanged {
        row: ConcernRow,
    },
    Refused {
        reason: ConcernRefusal,
        row: Option<ConcernRow>,
    },
}

/// Live keyset position, not a snapshot token or concurrent-completeness proof.
/// Rows are ordered by other endpoint, then kind (disagreement before redundancy).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ConcernPageCursor {
    endpoint: NodeId,
    other: NodeId,
    kind: ConcernKind,
}

impl ConcernPageCursor {
    pub fn new(
        endpoint: NodeId,
        other: NodeId,
        kind: ConcernKind,
    ) -> Result<Self, ConcernValidationError> {
        ConcernKey::new(kind, endpoint, other)?;
        Ok(Self {
            endpoint,
            other,
            kind,
        })
    }
    pub fn endpoint(&self) -> NodeId {
        self.endpoint
    }
    pub fn other(&self) -> NodeId {
        self.other
    }
    pub fn kind(&self) -> ConcernKind {
        self.kind
    }
}

impl<'de> Deserialize<'de> for ConcernPageCursor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            endpoint: NodeId,
            other: NodeId,
            kind: ConcernKind,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.endpoint, wire.other, wire.kind).map_err(serde::de::Error::custom)
    }
}

/// One request's total row allowance across both endpoint indexes. The borrowed
/// hydration-batch ceiling bounds allocation, never lifetime concern degree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConcernPageRequest {
    endpoint: NodeId,
    limit: usize,
    after: Option<ConcernPageCursor>,
}

impl ConcernPageRequest {
    pub fn new(
        endpoint: NodeId,
        limit: usize,
        after: Option<ConcernPageCursor>,
    ) -> Result<Self, ConcernValidationError> {
        if limit == 0 || limit > crate::MAX_NODE_HYDRATION_BATCH {
            return Err(ConcernValidationError::PageLimit);
        }
        if after
            .as_ref()
            .is_some_and(|cursor| cursor.endpoint != endpoint)
        {
            return Err(ConcernValidationError::CursorEndpoint);
        }
        Ok(Self {
            endpoint,
            limit,
            after,
        })
    }
    pub fn endpoint(&self) -> NodeId {
        self.endpoint
    }
    pub fn limit(&self) -> usize {
        self.limit
    }
    pub fn after(&self) -> Option<&ConcernPageCursor> {
        self.after.as_ref()
    }
}

impl<'de> Deserialize<'de> for ConcernPageRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            endpoint: NodeId,
            limit: usize,
            after: Option<ConcernPageCursor>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.endpoint, wire.limit, wire.after).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcernPage {
    pub items: Vec<ConcernRow>,
    pub next: Option<ConcernPageCursor>,
}

/// Optional advisory lane, not authority to change memories or learn routes.
#[async_trait]
pub trait ConcernStore: Send + Sync {
    /// Exact keyed read, including historical rows with archived endpoints.
    async fn get_concern(&self, key: &ConcernKey) -> crate::ports::Result<Option<ConcernRow>>;
    /// Read both canonical nodes and the exact row and apply pure transition in
    /// one transaction. Missing/inactive endpoints refuse BEFORE
    /// desired-equal replay. Content identity permits A -> B -> A; no revision
    /// history fence or external-body immutability is claimed. Active semantic
    /// nodes and active episode editions qualify; an edition binding is not a
    /// claim about the episode's current head.
    async fn update_concern(
        &self,
        update: &ConcernUpdate,
    ) -> crate::ports::Result<ConcernCommitOutcome>;
    /// Indexed outgoing/incoming keyset merge; total items <= request.limit().
    /// No whole-store scan or per-endpoint concern-degree ceiling.
    async fn concerns_for_endpoint(
        &self,
        request: &ConcernPageRequest,
    ) -> crate::ports::Result<ConcernPage>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConcernTransition {
    Unchanged,
    Replace(ConcernRow),
    Refused(ConcernRefusal),
}

/// Pure content-CAS policy. The caller supplies transaction-consistent current
/// canonical node projections and the row at the requested key. This does not
/// inspect arbitrary external content; lifecycle eligibility belongs to adapter.
/// Exact desired state is unchanged; after an intervening update an old retry
/// may refuse as stale rather than reconstruct its original acknowledgement.
pub fn transition_concern(
    current_meanings: &ConcernBinding,
    current_row: Option<&ConcernRow>,
    update: &ConcernUpdate,
) -> ConcernTransition {
    let requested = match update {
        ConcernUpdate::Notice(notice) => notice.binding,
        ConcernUpdate::RecordScopedFinding { expected, .. } => expected.binding(),
    };
    if current_meanings.key != requested.key
        || current_row.is_some_and(|row| row.binding().key != requested.key)
    {
        return ConcernTransition::Refused(ConcernRefusal::WrongKey);
    }
    if *current_meanings != requested {
        return ConcernTransition::Refused(ConcernRefusal::StaleMeanings);
    }
    match update {
        ConcernUpdate::Notice(notice) => {
            if current_row.is_some_and(|row| row.binding() == requested) {
                ConcernTransition::Unchanged
            } else {
                ConcernTransition::Replace(ConcernRow::from_notice(notice.clone()))
            }
        }
        ConcernUpdate::RecordScopedFinding { expected, finding } => {
            let Some(current) = current_row else {
                return ConcernTransition::Refused(ConcernRefusal::MissingRow);
            };
            let mut desired = expected.clone();
            desired.finding = Some(finding.clone());
            if current == &desired {
                ConcernTransition::Unchanged
            } else if current != expected {
                ConcernTransition::Refused(ConcernRefusal::StaleRow)
            } else {
                ConcernTransition::Replace(desired)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BodyRef, Node, NodeStatus, Provenance, ports::routing_content_fingerprint};
    use ulid::Ulid;

    fn endpoint(id: u128, meaning: &str) -> ConcernEndpoint {
        ConcernEndpoint::new(
            NodeId(Ulid::from(id)),
            ConcernDigest::of_bytes(meaning.as_bytes()),
        )
    }
    fn binding() -> ConcernBinding {
        ConcernBinding::new(
            ConcernKind::Disagreement,
            endpoint(1, "old binary"),
            endpoint(2, "fixed binary"),
        )
        .unwrap()
    }
    fn notice(binding: ConcernBinding) -> ConcernNotice {
        ConcernNotice::new(
            binding,
            "Shutdown behaviour differs",
            "Which binary is installed?",
        )
        .unwrap()
    }
    fn finding(scope: &str) -> ScopedConcernFinding {
        ScopedConcernFinding::new(
            scope,
            "The installed binary includes the fix",
            vec![
                ConcernEvidence::new(
                    "tool://version-check",
                    ConcernDigest::of_bytes(b"v2 inspected"),
                )
                .unwrap(),
            ],
        )
        .unwrap()
    }
    fn apply(
        binding: &ConcernBinding,
        row: Option<&ConcernRow>,
        update: &ConcernUpdate,
    ) -> ConcernRow {
        match transition_concern(binding, row, update) {
            ConcernTransition::Replace(row) => row,
            other => panic!("expected replacement, got {other:?}"),
        }
    }

    #[test]
    fn reversed_pair_keeps_digests_with_endpoints_and_fingerprint() {
        let reversed = ConcernBinding::new(
            ConcernKind::Disagreement,
            endpoint(2, "fixed binary"),
            endpoint(1, "old binary"),
        )
        .unwrap();
        assert_eq!(binding(), reversed);
        assert_eq!(
            ConcernRow::from_notice(notice(binding())).fingerprint(),
            ConcernRow::from_notice(notice(reversed)).fingerprint()
        );
        assert!(
            ConcernKey::new(
                ConcernKind::Disagreement,
                endpoint(1, "x").id(),
                endpoint(1, "y").id()
            )
            .is_err()
        );
    }

    #[test]
    fn notice_inserts_and_same_meanings_preserve_finding_even_if_reworded() {
        let binding = binding();
        let row = apply(&binding, None, &ConcernUpdate::Notice(notice(binding)));
        let row = apply(
            &binding,
            Some(&row),
            &ConcernUpdate::RecordScopedFinding {
                expected: row.clone(),
                finding: finding("installed v2 only"),
            },
        );
        let reworded = ConcernNotice::new(binding, "A new phrasing", "Same missing fact").unwrap();
        assert_eq!(
            transition_concern(&binding, Some(&row), &ConcernUpdate::Notice(reworded)),
            ConcernTransition::Unchanged
        );
        assert_eq!(row.finding().unwrap().scope(), "installed v2 only");
    }

    #[test]
    fn lost_ack_replay_is_unchanged_but_intervening_finding_refuses() {
        let binding = binding();
        let initial = ConcernRow::from_notice(notice(binding));
        let first = ConcernUpdate::RecordScopedFinding {
            expected: initial.clone(),
            finding: finding("task A: v2"),
        };
        let accepted = apply(&binding, Some(&initial), &first);
        assert_eq!(
            transition_concern(&binding, Some(&accepted.clone()), &first),
            ConcernTransition::Unchanged
        );
        let later = apply(
            &binding,
            Some(&accepted),
            &ConcernUpdate::RecordScopedFinding {
                expected: accepted.clone(),
                finding: finding("task B: v3"),
            },
        );
        assert_eq!(
            transition_concern(&binding, Some(&later), &first),
            ConcernTransition::Refused(ConcernRefusal::StaleRow)
        );
        assert_eq!(later.finding().unwrap().scope(), "task B: v3");
    }

    #[test]
    fn changed_meaning_requires_fresh_notice_and_drops_cached_finding() {
        let old = binding();
        let initial = ConcernRow::from_notice(notice(old));
        let update = ConcernUpdate::RecordScopedFinding {
            expected: initial.clone(),
            finding: finding("v2 only"),
        };
        let row = apply(&old, Some(&initial), &update);
        let new = ConcernBinding::new(
            ConcernKind::Disagreement,
            endpoint(1, "new claim"),
            endpoint(2, "fixed binary"),
        )
        .unwrap();
        assert_eq!(
            transition_concern(&new, Some(&row), &update),
            ConcernTransition::Refused(ConcernRefusal::StaleMeanings)
        );
        assert_eq!(
            transition_concern(&new, Some(&row), &ConcernUpdate::Notice(notice(old))),
            ConcernTransition::Refused(ConcernRefusal::StaleMeanings)
        );
        let replaced = apply(&new, Some(&row), &ConcernUpdate::Notice(notice(new)));
        assert_eq!(replaced.binding(), new);
        assert!(replaced.finding().is_none());
    }

    #[test]
    fn external_body_race_preserves_historical_evidence_not_current_content_authority() {
        let current = binding();
        let initial = ConcernRow::from_notice(notice(current));
        let historical = ScopedConcernFinding::new(
            "inspection before external replacement",
            "At that inspection body A contained the claim",
            vec![
                ConcernEvidence::new("file:///external/body", ConcernDigest::of_bytes(b"body A"))
                    .unwrap(),
            ],
        )
        .unwrap();
        // External A -> B leaves canonical summary/body-ref/tags unchanged.
        // The pure/native CAS can only accept an explicitly historical finding.
        let update = ConcernUpdate::RecordScopedFinding {
            expected: initial.clone(),
            finding: historical,
        };
        let accepted = apply(&current, Some(&initial), &update);
        assert_eq!(accepted.binding(), initial.binding());
        assert_eq!(
            accepted.finding().unwrap().evidence()[0].digest(),
            ConcernDigest::of_bytes(b"body A")
        );
        assert_ne!(
            accepted.finding().unwrap().evidence()[0].digest(),
            ConcernDigest::of_bytes(b"body B")
        );
    }

    #[test]
    fn telemetry_is_absent_from_meaning_binding() {
        let original = Node::try_new(
            NodeId(Ulid::from(1)),
            "same claim",
            BodyRef::new("inline://same").unwrap(),
            ["scope"],
            Provenance::derived_empty(),
            1.0,
            1.0,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        let mut telemetry = original.clone();
        telemetry.record_grounded_use(1000);
        let original_endpoint = ConcernEndpoint::from_node(&original);
        let telemetry_endpoint = ConcernEndpoint::from_node(&telemetry);
        let serialized_digest = serde_json::to_value(original_endpoint.meaning()).unwrap();
        assert_eq!(
            serialized_digest,
            serde_json::json!(routing_content_fingerprint(&original))
        );
        assert_eq!(original_endpoint, telemetry_endpoint);
        let current = ConcernBinding::new(
            ConcernKind::Disagreement,
            telemetry_endpoint,
            endpoint(2, "other"),
        )
        .unwrap();
        let observed = ConcernBinding::new(
            ConcernKind::Disagreement,
            original_endpoint,
            endpoint(2, "other"),
        )
        .unwrap();
        let row = ConcernRow::from_notice(notice(observed));
        assert_eq!(
            transition_concern(
                &current,
                Some(&row),
                &ConcernUpdate::Notice(notice(observed))
            ),
            ConcernTransition::Unchanged
        );
    }

    #[test]
    fn partial_scope_is_only_an_observation_and_latest_finding_is_a_cache() {
        let binding = binding();
        let row = ConcernRow::from_notice(notice(binding));
        let current = apply(
            &binding,
            Some(&row),
            &ConcernUpdate::RecordScopedFinding {
                expected: row.clone(),
                finding: finding("shutdown claim only; other claims untouched"),
            },
        );
        assert_eq!(current.notice(), row.notice());
        assert_eq!(current.binding(), row.binding());
        assert_eq!(
            current.finding().unwrap().scope(),
            "shutdown claim only; other claims untouched"
        );
        assert_ne!(current.fingerprint(), row.fingerprint());
    }

    #[test]
    fn wrong_key_and_missing_row_refuse_instead_of_creating_finding() {
        let binding = binding();
        let row = ConcernRow::from_notice(notice(binding));
        let update = ConcernUpdate::RecordScopedFinding {
            expected: row.clone(),
            finding: finding("v2"),
        };
        assert_eq!(
            transition_concern(&binding, None, &update),
            ConcernTransition::Refused(ConcernRefusal::MissingRow)
        );
        let other = ConcernBinding::new(
            ConcernKind::Redundancy,
            endpoint(1, "old binary"),
            endpoint(2, "fixed binary"),
        )
        .unwrap();
        assert_eq!(
            transition_concern(&other, Some(&row), &update),
            ConcernTransition::Refused(ConcernRefusal::WrongKey)
        );
    }

    #[test]
    fn bounds_use_bytes_and_require_bounded_nonempty_evidence() {
        assert!(ConcernText::<4>::new("éé").is_ok());
        assert!(ConcernText::<4>::new("ééé").is_err());
        assert!(ConcernText::<4>::new(" \n ").is_err());
        assert!(ConcernNotice::new(binding(), "x".repeat(MAX_CONCERN_BYTES + 1), "fact").is_err());
        assert!(
            ConcernNotice::new(
                binding(),
                "concern",
                "x".repeat(MAX_CONCERN_MISSING_FACT_BYTES + 1)
            )
            .is_err()
        );
        assert!(
            ConcernEvidence::new(
                "x".repeat(MAX_CONCERN_EVIDENCE_REF_BYTES + 1),
                ConcernDigest::of_bytes(b"x")
            )
            .is_err()
        );
        assert!(ScopedConcernFinding::new("scope", "observation", vec![]).is_err());
        let evidence = finding("scope").evidence()[0].clone();
        assert!(
            ScopedConcernFinding::new(
                "x".repeat(MAX_CONCERN_SCOPE_BYTES + 1),
                "observation",
                vec![evidence.clone()]
            )
            .is_err()
        );
        assert!(
            ScopedConcernFinding::new(
                "scope",
                "x".repeat(MAX_CONCERN_FINDING_BYTES + 1),
                vec![evidence]
            )
            .is_err()
        );
    }

    #[test]
    fn five_short_evidence_refs_fit_without_a_cardinality_policy() {
        let evidence = (0..5)
            .map(|i| {
                ConcernEvidence::new(format!("source:{i}"), ConcernDigest::of_bytes(&[i])).unwrap()
            })
            .collect();
        let finding =
            ScopedConcernFinding::new("task scope", "five decisive sources", evidence).unwrap();
        assert_eq!(finding.evidence().len(), 5);
    }

    #[test]
    fn aggregate_evidence_budget_accepts_exact_boundary_and_refuses_one_extra_byte() {
        let reference = |bytes| {
            ConcernEvidence::new("x".repeat(bytes), ConcernDigest::of_bytes(b"source")).unwrap()
        };
        // 16-byte framed count + 4 * 48-byte framing/digests + references.
        let last_ref_bytes =
            MAX_CONCERN_EVIDENCE_BYTES - 16 - 4 * 48 - 3 * MAX_CONCERN_EVIDENCE_REF_BYTES;
        let at_limit = vec![
            reference(MAX_CONCERN_EVIDENCE_REF_BYTES),
            reference(MAX_CONCERN_EVIDENCE_REF_BYTES),
            reference(MAX_CONCERN_EVIDENCE_REF_BYTES),
            reference(last_ref_bytes),
        ];
        assert!(ScopedConcernFinding::new("scope", "observation", at_limit.clone()).is_ok());
        let mut over_limit = at_limit;
        over_limit[3] = reference(last_ref_bytes + 1);
        assert_eq!(
            ScopedConcernFinding::new("scope", "observation", over_limit),
            Err(ConcernValidationError::EvidenceBounds)
        );
    }

    #[test]
    fn serde_roundtrips_canonical_rows_updates_and_outcomes() {
        let current = binding();
        let row = ConcernRow::from_notice(notice(current));
        let update = ConcernUpdate::RecordScopedFinding {
            expected: row.clone(),
            finding: finding("inspection only"),
        };
        let accepted = apply(&current, Some(&row), &update);
        let json = serde_json::to_value(&accepted).unwrap();
        assert_eq!(
            serde_json::from_value::<ConcernRow>(json.clone()).unwrap(),
            accepted
        );
        for request in [ConcernUpdate::Notice(notice(current)), update] {
            let encoded = serde_json::to_value(&request).unwrap();
            assert!(matches!(
                encoded["action"].as_str(),
                Some("notice" | "record_finding")
            ));
            assert_eq!(
                serde_json::from_value::<ConcernUpdate>(encoded).unwrap(),
                request
            );
        }
        let outcome = ConcernCommitOutcome::Applied {
            row: accepted.clone(),
        };
        assert_eq!(
            serde_json::from_value::<ConcernCommitOutcome>(serde_json::to_value(&outcome).unwrap())
                .unwrap(),
            outcome
        );
        let restored = serde_json::from_value::<ConcernRow>(json).unwrap();
        assert_eq!(restored.fingerprint(), accepted.fingerprint());
    }

    #[test]
    fn serde_rejects_malformed_unknown_reversed_and_self_bindings() {
        let original = serde_json::to_value(ConcernRow::from_notice(notice(binding()))).unwrap();
        let mut mutations = Vec::new();
        let mut value = original.clone();
        value["extra"] = serde_json::json!(true);
        mutations.push(value);
        let mut value = original.clone();
        value.as_object_mut().unwrap().remove("finding");
        mutations.push(value);
        let mut value = original.clone();
        value["notice"]["binding"]["endpoints"][0]["meaning"] = serde_json::json!("AB".repeat(32));
        mutations.push(value);
        let mut value = original.clone();
        value["notice"]["binding"]["endpoints"][0]["meaning"] = serde_json::json!("0".repeat(63));
        mutations.push(value);
        let mut value = original.clone();
        value["notice"]["binding"]["endpoints"][0]["inspected_content"] =
            serde_json::json!("0".repeat(64));
        mutations.push(value);
        let mut value = original.clone();
        value["notice"]["binding"]["endpoints"]
            .as_array_mut()
            .unwrap()
            .swap(0, 1);
        mutations.push(value);
        let mut value = original.clone();
        let lo = value["notice"]["binding"]["key"]["lo"].clone();
        let hi = value["notice"]["binding"]["key"]["hi"].clone();
        value["notice"]["binding"]["key"]["lo"] = hi;
        value["notice"]["binding"]["key"]["hi"] = lo;
        mutations.push(value);
        let mut value = original.clone();
        value["notice"]["binding"]["key"]["hi"] = value["notice"]["binding"]["key"]["lo"].clone();
        mutations.push(value);
        let mut value = original.clone();
        value["notice"]["binding"]["endpoints"][0]["id"] = serde_json::json!(NodeId(Ulid::from(3)));
        mutations.push(value);
        let mut value = original.clone();
        value["notice"]["concern"] = serde_json::json!("x".repeat(MAX_CONCERN_BYTES + 1));
        mutations.push(value);
        for value in mutations {
            assert!(serde_json::from_value::<ConcernRow>(value).is_err());
        }
        let mut update = serde_json::to_value(ConcernUpdate::Notice(notice(binding()))).unwrap();
        update["expected"] = original;
        assert!(serde_json::from_value::<ConcernUpdate>(update).is_err());
    }

    #[test]
    fn serde_rejects_oversized_evidence_without_retaining_unbounded_vector() {
        let mut value = serde_json::to_value(finding("scope")).unwrap();
        let evidence = value["evidence"][0].clone();
        value["evidence"] = serde_json::json!(vec![evidence; MAX_CONCERN_EVIDENCE_BYTES]);
        assert!(serde_json::from_value::<ScopedConcernFinding>(value).is_err());
        let mut value = serde_json::to_value(finding("scope")).unwrap();
        value["evidence"] = serde_json::json!([]);
        assert!(serde_json::from_value::<ScopedConcernFinding>(value).is_err());
    }

    #[test]
    fn endpoint_pages_are_bounded_endpoint_bound_live_keysets() {
        let [a, b] = binding().key().endpoints();
        let cursor = ConcernPageCursor::new(a, b, ConcernKind::Disagreement).unwrap();
        let request =
            ConcernPageRequest::new(a, crate::MAX_NODE_HYDRATION_BATCH, Some(cursor)).unwrap();
        assert_eq!(
            serde_json::from_value::<ConcernPageRequest>(serde_json::to_value(&request).unwrap())
                .unwrap(),
            request
        );
        assert!(ConcernPageRequest::new(a, 0, None).is_err());
        assert!(ConcernPageRequest::new(a, crate::MAX_NODE_HYDRATION_BATCH + 1, None).is_err());
        assert!(ConcernPageRequest::new(b, 1, Some(cursor)).is_err());
        assert!(ConcernPageCursor::new(a, a, ConcernKind::Disagreement).is_err());
        let mut wire = serde_json::to_value(&request).unwrap();
        wire["endpoint"] = serde_json::json!(b);
        assert!(serde_json::from_value::<ConcernPageRequest>(wire).is_err());
    }

    #[test]
    fn multi_claim_question_b_does_not_resolve_question_a_and_content_aba_is_allowed() {
        let current = binding();
        let initial = ConcernRow::from_notice(notice(current));
        let question_b = ConcernNotice::new(
            current,
            "A different disagreement in the same pair",
            "Which filesystem was tested?",
        )
        .unwrap();
        assert_eq!(
            transition_concern(&current, Some(&initial), &ConcernUpdate::Notice(question_b)),
            ConcernTransition::Unchanged
        );
        let update = ConcernUpdate::RecordScopedFinding {
            expected: initial.clone(),
            finding: ScopedConcernFinding::new(
                "question B; filesystem test only",
                "B held in the test; shutdown question A remains unknown",
                finding("scope").evidence().to_vec(),
            )
            .unwrap(),
        };
        // Node content A -> B -> A is intentionally indistinguishable from A.
        let accepted = apply(&current, Some(&initial), &update);
        assert_eq!(accepted.notice(), initial.notice());
        assert!(
            accepted
                .finding()
                .unwrap()
                .observation()
                .contains("A remains unknown")
        );
        assert_eq!(
            transition_concern(&current, Some(&accepted), &update),
            ConcernTransition::Unchanged
        );
    }
}
