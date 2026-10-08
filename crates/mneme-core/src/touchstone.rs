//! Authored personal meaning, independent of learned relevance. Immutable
//! summary-only citations are canonical records, not weighted discovery edges.

use crate::ports::{Error, Result};
use crate::{
    CaptureRequestCodec, MemoryKind, Node, NodeId, NodeStatus, NodeSummary, Provenance, Timestamp,
};
use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};
use ulid::Ulid;

pub const MAX_TOUCHSTONE_SUBJECT_BYTES: usize = 256;
pub const MAX_TOUCHSTONE_INPUT_BYTES: usize = 8 * 1024;
pub const MAX_TOUCHSTONE_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_TOUCHSTONE_PAGE_ITEMS: usize = 32;
pub const MAX_TOUCHSTONE_CURSOR_BYTES: usize = 1024;
pub const SUMMARY_SNAPSHOT_CODEC: &str = "mneme.touchstone.summary-snapshot.v1";

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidInput(message.into())
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TouchstoneSubject(String);
impl TouchstoneSubject {
    pub fn new(value: impl AsRef<str>) -> Result<Self> {
        let value = value.as_ref();
        if value.is_empty()
            || value.trim().is_empty()
            || value.len() > MAX_TOUCHSTONE_SUBJECT_BYTES
            || value.trim() != value
            || value.chars().any(char::is_control)
        {
            return Err(invalid(
                "touchstone subject must be nonblank, trimmed, control-free and at most 256 UTF-8 bytes",
            ));
        }
        Ok(Self(value.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for TouchstoneSubject {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SummarySnapshotDigest([u8; 32]);
impl SummarySnapshotDigest {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    pub fn from_hex(value: &str) -> Result<Self> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid(
                "summary snapshot digest must be 64 lowercase hexadecimal characters",
            ));
        }
        let mut bytes = [0; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            fn nibble(byte: u8) -> u8 {
                if byte.is_ascii_digit() {
                    byte - b'0'
                } else {
                    byte - b'a' + 10
                }
            }
            bytes[index] = (nibble(pair[0]) << 4) | nibble(pair[1]);
        }
        Ok(Self(bytes))
    }
    pub fn to_hex(self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
impl fmt::Display for SummarySnapshotDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}
impl Serialize for SummarySnapshotDigest {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}
impl<'de> Deserialize<'de> for SummarySnapshotDigest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::from_hex(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TouchstoneReference {
    db_id: Ulid,
    id: NodeId,
    expected_snapshot_sha256: SummarySnapshotDigest,
}
impl TouchstoneReference {
    pub const fn new(
        db_id: Ulid,
        id: NodeId,
        expected_snapshot_sha256: SummarySnapshotDigest,
    ) -> Self {
        Self {
            db_id,
            id,
            expected_snapshot_sha256,
        }
    }
    pub const fn db_id(&self) -> Ulid {
        self.db_id
    }
    pub const fn id(&self) -> NodeId {
        self.id
    }
    pub const fn expected_snapshot_sha256(&self) -> SummarySnapshotDigest {
        self.expected_snapshot_sha256
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "TouchstoneInputWire")]
pub struct TouchstoneInput {
    subject: TouchstoneSubject,
    references: Vec<TouchstoneReference>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TouchstoneInputWire {
    subject: TouchstoneSubject,
    references: Vec<TouchstoneReference>,
}
impl TryFrom<TouchstoneInputWire> for TouchstoneInput {
    type Error = Error;
    fn try_from(w: TouchstoneInputWire) -> Result<Self> {
        Self::new(w.subject, w.references)
    }
}
impl TouchstoneInput {
    pub fn new(subject: TouchstoneSubject, references: Vec<TouchstoneReference>) -> Result<Self> {
        if references.is_empty() {
            return Err(invalid("touchstone references cannot be empty"));
        }
        let mut input = Self {
            subject,
            references,
        };
        if input.compact_json_len() > MAX_TOUCHSTONE_INPUT_BYTES {
            return Err(invalid(
                "touchstone input exceeds 8 KiB compact JSON byte limit",
            ));
        }
        input
            .references
            .sort_unstable_by_key(|reference| (reference.db_id, reference.id));
        if input
            .references
            .windows(2)
            .any(|p| (p[0].db_id, p[0].id) == (p[1].db_id, p[1].id))
        {
            return Err(invalid(
                "duplicate touchstone reference database/node identity",
            ));
        }
        Ok(input)
    }
    pub fn validate(&self) -> Result<()> {
        if Self::new(self.subject.clone(), self.references.clone())? != *self {
            return Err(invalid("touchstone references are not canonically ordered"));
        }
        Ok(())
    }
    pub fn subject(&self) -> &TouchstoneSubject {
        &self.subject
    }
    pub fn references(&self) -> &[TouchstoneReference] {
        &self.references
    }
    pub fn compact_json_len(&self) -> usize {
        serde_json::to_vec(self)
            .expect("validated touchstone input has an infallible JSON wire")
            .len()
    }
    /// Subject frame, u64-BE count, then each canonical db/id u128-BE and raw
    /// 32-byte expected digest. The caller supplies its distinct request domain
    /// and nested ordinary-capture proof; existing capture codecs are untouched.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = CanonicalBytes::default();
        bytes.text(self.subject.as_str());
        bytes.u64(self.references.len() as u64);
        for reference in &self.references {
            bytes.u128(reference.db_id.0);
            bytes.u128(reference.id.0.0);
            bytes.raw(reference.expected_snapshot_sha256.as_bytes());
        }
        bytes.0
    }
    pub fn validate_database_id(&self, db_id: Ulid) -> Result<()> {
        self.validate()?;
        if self
            .references
            .iter()
            .any(|reference| reference.db_id != db_id)
        {
            return Err(invalid(
                "touchstone v1 references must name this logical database",
            ));
        }
        Ok(())
    }
}

/// Complete historical summary projection, NOT a body archive or a claim of
/// full-content equivalence. Current resolution/head/status are separate reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SummarySnapshotWire")]
pub struct SummarySnapshot {
    db_id: Ulid,
    id: NodeId,
    summary: NodeSummary,
    provenance: Provenance,
    created: Timestamp,
    memory_kind: MemoryKind,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SummarySnapshotWire {
    db_id: Ulid,
    id: NodeId,
    summary: NodeSummary,
    provenance: Provenance,
    created: Timestamp,
    memory_kind: MemoryKind,
}
impl TryFrom<SummarySnapshotWire> for SummarySnapshot {
    type Error = Error;
    fn try_from(w: SummarySnapshotWire) -> Result<Self> {
        let value = Self {
            db_id: w.db_id,
            id: w.id,
            summary: w.summary,
            provenance: w.provenance,
            created: w.created,
            memory_kind: w.memory_kind,
        };
        value.validate()?;
        Ok(value)
    }
}
impl SummarySnapshot {
    pub fn from_node(db_id: Ulid, node: &Node) -> Result<Self> {
        node.validate().map_err(|e| invalid(e.to_string()))?;
        Ok(Self {
            db_id,
            id: node.id(),
            summary: NodeSummary::new(node.summary()).map_err(|e| invalid(e.to_string()))?,
            provenance: node.provenance().clone(),
            created: node.created(),
            memory_kind: node.memory_kind().clone(),
        })
    }
    pub fn validate(&self) -> Result<()> {
        // Reuse enclosing-node invariants without including this dummy body,
        // empty tags, neutral ranking or status in the snapshot or digest.
        let node = Node::try_new(
            self.id,
            self.summary.as_str(),
            crate::BodyRef::new("inline://summary-snapshot-validation").expect("constant body ref"),
            Vec::<&str>::new(),
            self.provenance.clone(),
            0.5,
            0.5,
            NodeStatus::Active,
            self.created,
        )
        .map_err(|e| invalid(e.to_string()))?;
        let node = match &self.memory_kind {
            MemoryKind::Semantic => node,
            MemoryKind::Episode(facet) => node
                .with_episode(facet.clone())
                .map_err(|e| invalid(e.to_string()))?,
        };
        node.validate().map_err(|e| invalid(e.to_string()))
    }
    pub const fn db_id(&self) -> Ulid {
        self.db_id
    }
    pub const fn id(&self) -> NodeId {
        self.id
    }
    pub fn summary(&self) -> &NodeSummary {
        &self.summary
    }
    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }
    pub const fn created(&self) -> Timestamp {
        self.created
    }
    pub fn memory_kind(&self) -> &MemoryKind {
        &self.memory_kind
    }
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = CanonicalBytes::default();
        bytes.text(SUMMARY_SNAPSHOT_CODEC);
        bytes.u128(self.db_id.0);
        bytes.u128(self.id.0.0);
        bytes.text(self.summary.as_str());
        bytes.provenance(&self.provenance);
        bytes.u128(self.created);
        bytes.memory_kind(&self.memory_kind);
        bytes.0
    }
    pub fn digest(&self) -> SummarySnapshotDigest {
        SummarySnapshotDigest(Sha256::digest(self.canonical_bytes()).into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "TouchstoneRecordWire")]
pub struct TouchstoneRecord {
    owner: NodeId,
    subject: TouchstoneSubject,
    references: Vec<SummarySnapshot>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TouchstoneRecordWire {
    owner: NodeId,
    subject: TouchstoneSubject,
    references: Vec<SummarySnapshot>,
}
impl TryFrom<TouchstoneRecordWire> for TouchstoneRecord {
    type Error = Error;
    fn try_from(w: TouchstoneRecordWire) -> Result<Self> {
        let value = Self {
            owner: w.owner,
            subject: w.subject,
            references: w.references,
        };
        value.validate()?;
        Ok(value)
    }
}
impl TouchstoneRecord {
    pub fn new(
        owner: NodeId,
        input: &TouchstoneInput,
        mut snapshots: Vec<SummarySnapshot>,
    ) -> Result<Self> {
        input.validate()?;
        snapshots.sort_unstable_by_key(|snapshot| (snapshot.db_id, snapshot.id));
        if snapshots.len() != input.references.len() {
            return Err(invalid(
                "touchstone snapshot count differs from authored references",
            ));
        }
        for (snapshot, reference) in snapshots.iter().zip(&input.references) {
            snapshot.validate()?;
            if snapshot.db_id != reference.db_id
                || snapshot.id != reference.id
                || snapshot.digest() != reference.expected_snapshot_sha256
            {
                return Err(Error::Conflict(
                    "touchstone referenced summary no longer matches its expected snapshot".into(),
                ));
            }
        }
        let value = Self {
            owner,
            subject: input.subject.clone(),
            references: snapshots,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn owner(&self) -> NodeId {
        self.owner
    }
    pub fn subject(&self) -> &TouchstoneSubject {
        &self.subject
    }
    pub fn references(&self) -> &[SummarySnapshot] {
        &self.references
    }
    pub fn validate(&self) -> Result<()> {
        if self.references.is_empty() {
            return Err(invalid("touchstone record cannot have empty references"));
        }
        if self
            .references
            .windows(2)
            .any(|p| (p[0].db_id, p[0].id) >= (p[1].db_id, p[1].id))
        {
            return Err(invalid(
                "touchstone snapshots must have canonical unique database/node order",
            ));
        }
        if self
            .references
            .iter()
            .any(|snapshot| snapshot.id == self.owner)
        {
            return Err(invalid("a touchstone cannot cite its own owner"));
        }
        for snapshot in &self.references {
            snapshot.validate()?;
        }
        if compact_json_bytes(self)? > MAX_TOUCHSTONE_RECORD_BYTES {
            return Err(invalid(
                "touchstone record exceeds 64 KiB compact JSON byte limit",
            ));
        }
        Ok(())
    }
    pub fn compact_json_len(&self) -> Result<usize> {
        compact_json_bytes(self)
    }
    pub fn validate_owner(&self, db_id: Ulid, node: &Node) -> Result<()> {
        self.validate()?;
        node.validate().map_err(|e| invalid(e.to_string()))?;
        if node.id() != self.owner
            || !node.is_semantic()
            || self.references.iter().any(|s| s.db_id != db_id)
        {
            return Err(invalid(
                "touchstone owner/database does not match canonical semantic record",
            ));
        }
        match node.provenance() {
            Provenance::External { source }
                if source.node_id() == node.id()
                    && source.request_codec() == CaptureRequestCodec::TouchstoneV1 =>
            {
                Ok(())
            }
            _ => Err(invalid(
                "touchstone owner requires matching touchstone_v1 capture provenance",
            )),
        }
    }
}

/// Use only on first apply, inside the owner's native atomic transaction. Probe
/// exact replay before calling: targets may have changed/deleted after commit.
pub fn capture_touchstone_snapshots(
    db_id: Ulid,
    owner: NodeId,
    input: &TouchstoneInput,
    targets: &[Node],
) -> Result<TouchstoneRecord> {
    input.validate_database_id(db_id)?;
    let mut nodes = BTreeMap::new();
    for node in targets {
        if nodes.insert(node.id(), node).is_some() {
            return Err(invalid(
                "duplicate target node in touchstone snapshot capture",
            ));
        }
    }
    let mut snapshots = Vec::with_capacity(input.references.len());
    for reference in &input.references {
        if reference.id == owner {
            return Err(invalid("a touchstone cannot cite its own owner"));
        }
        let node = nodes
            .get(&reference.id)
            .ok_or_else(|| Error::Conflict("touchstone referenced summary is missing".into()))?;
        snapshots.push(SummarySnapshot::from_node(db_id, node)?);
    }
    TouchstoneRecord::new(owner, input, snapshots)
}

/// Typed relation presence, not a discovery tag, activates this overwrite guard.
/// Lifecycle and ranking/exposure/use telemetry may change; authored content,
/// provenance, resource ownership, identity and original clock anchors may not.
pub fn validate_touchstone_owner_replacement(before: &Node, after: &Node) -> Result<()> {
    if before.id() != after.id()
        || before.memory_kind() != after.memory_kind()
        || before.summary() != after.summary()
        || before.body() != after.body()
        || before.body_ownership() != after.body_ownership()
        || before.tags().ne(after.tags())
        || before.provenance() != after.provenance()
        || before.origin_commit() != after.origin_commit()
        || before.created() != after.created()
    {
        return Err(Error::Conflict("touchstone authored owner content is immutable; save a successor with a new source key".into()));
    }
    Ok(())
}

/// Closed catalog join for import/export admission. Deleted targets are legal:
/// snapshots carry their historical evidence. Missing owners and orphan codecs
/// are not. No migration recomputes source proofs or substitutes target heads.
pub fn validate_touchstone_catalog(
    db_id: Ulid,
    nodes: &[Node],
    records: &[TouchstoneRecord],
) -> Result<()> {
    let mut owners = BTreeSet::new();
    let mut node_map = BTreeMap::new();
    for node in nodes {
        if node_map.insert(node.id(), node).is_some() {
            return Err(invalid(
                "duplicate node identity in touchstone catalog validation",
            ));
        }
    }
    for record in records {
        if !owners.insert(record.owner) {
            return Err(invalid("duplicate touchstone owner record"));
        }
        let node = node_map
            .get(&record.owner)
            .ok_or_else(|| invalid("touchstone record has no canonical owner"))?;
        record.validate_owner(db_id, node)?;
    }
    for node in nodes {
        if matches!(node.provenance(), Provenance::External { source } if source.request_codec() == CaptureRequestCodec::TouchstoneV1)
            && !owners.contains(&node.id())
        {
            return Err(invalid(
                "touchstone_v1 owner has no canonical touchstone record",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TouchstoneHeader {
    pub id: NodeId,
    pub summary: NodeSummary,
    pub subject: TouchstoneSubject,
    pub reference_count: usize,
    pub status: NodeStatus,
}
impl TouchstoneHeader {
    pub fn from_node(node: &Node, record: &TouchstoneRecord) -> Result<Self> {
        if node.id() != record.owner || !node.is_semantic() {
            return Err(invalid("touchstone header owner differs from record"));
        }
        Ok(Self {
            id: node.id(),
            summary: NodeSummary::new(node.summary()).map_err(|e| invalid(e.to_string()))?,
            subject: record.subject.clone(),
            reference_count: record.references.len(),
            status: node.status(),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum TouchstoneSelection {
    Owners(Option<TouchstoneSubject>),
    Referrers(NodeId),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TouchstoneCursor {
    db_id: Ulid,
    after: NodeId,
    selection: TouchstoneSelection,
}
impl TouchstoneCursor {
    pub fn owners(db_id: Ulid, subject: Option<TouchstoneSubject>, after: NodeId) -> Self {
        Self {
            db_id,
            after,
            selection: TouchstoneSelection::Owners(subject),
        }
    }
    pub const fn referrers(db_id: Ulid, target: NodeId, after: NodeId) -> Self {
        Self {
            db_id,
            after,
            selection: TouchstoneSelection::Referrers(target),
        }
    }
    pub const fn db_id(&self) -> Ulid {
        self.db_id
    }
    pub const fn after(&self) -> NodeId {
        self.after
    }
    pub fn validate_owners(&self, db_id: Ulid, subject: Option<&TouchstoneSubject>) -> Result<()> {
        if self.db_id != db_id
            || !matches!(&self.selection, TouchstoneSelection::Owners(s) if s.as_ref() == subject)
        {
            return Err(invalid(
                "touchstone cursor belongs to another database or subject shelf",
            ));
        }
        Ok(())
    }
    pub fn validate_referrers(&self, db_id: Ulid, target: NodeId) -> Result<()> {
        if self.db_id != db_id || self.selection != TouchstoneSelection::Referrers(target) {
            return Err(invalid(
                "touchstone cursor belongs to another database or reference target",
            ));
        }
        Ok(())
    }
}
impl fmt::Display for TouchstoneCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.selection {
            TouchstoneSelection::Owners(subject) => {
                let subject = subject.as_ref().map_or_else(
                    || "-".to_owned(),
                    |s| s.0.as_bytes().iter().map(|b| format!("{b:02x}")).collect(),
                );
                write!(f, "t1o:{}:{}:{}", self.db_id, self.after.0, subject)
            }
            TouchstoneSelection::Referrers(target) => {
                write!(f, "t1r:{}:{}:{}", self.db_id, self.after.0, target.0)
            }
        }
    }
}
impl FromStr for TouchstoneCursor {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        if value.len() > MAX_TOUCHSTONE_CURSOR_BYTES {
            return Err(invalid("touchstone cursor exceeds byte limit"));
        }
        let p: Vec<_> = value.split(':').collect();
        if p.len() != 4 {
            return Err(invalid("invalid touchstone cursor"));
        }
        fn ulid(value: &str) -> Result<Ulid> {
            let id = Ulid::from_string(value)
                .map_err(|_| invalid("invalid touchstone cursor identity"))?;
            if id.to_string() != value {
                return Err(invalid("noncanonical touchstone cursor identity"));
            }
            Ok(id)
        }
        let db_id = ulid(p[1])?;
        let after = NodeId(ulid(p[2])?);
        let cursor = match p[0] {
            "t1r" => Self::referrers(db_id, NodeId(ulid(p[3])?), after),
            "t1o" => {
                let subject = if p[3] == "-" {
                    None
                } else {
                    if p[3].is_empty()
                        || p[3].len() % 2 != 0
                        || p[3].len() > MAX_TOUCHSTONE_SUBJECT_BYTES * 2
                        || !p[3]
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    {
                        return Err(invalid("invalid touchstone cursor subject encoding"));
                    }
                    let bytes = p[3]
                        .as_bytes()
                        .chunks_exact(2)
                        .map(|pair| {
                            fn nibble(b: u8) -> u8 {
                                if b.is_ascii_digit() {
                                    b - b'0'
                                } else {
                                    b - b'a' + 10
                                }
                            }
                            (nibble(pair[0]) << 4) | nibble(pair[1])
                        })
                        .collect::<Vec<_>>();
                    Some(TouchstoneSubject::new(String::from_utf8(bytes).map_err(
                        |_| invalid("touchstone cursor subject is not UTF-8"),
                    )?)?)
                };
                Self::owners(db_id, subject, after)
            }
            _ => return Err(invalid("unknown touchstone cursor version or selection")),
        };
        if cursor.to_string() != value {
            return Err(invalid("noncanonical touchstone cursor"));
        }
        Ok(cursor)
    }
}
impl Serialize for TouchstoneCursor {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for TouchstoneCursor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug)]
pub struct TouchstonePageRequest {
    subject: Option<TouchstoneSubject>,
    after: Option<TouchstoneCursor>,
    limit: usize,
}
impl TouchstonePageRequest {
    pub fn new(
        subject: Option<TouchstoneSubject>,
        after: Option<TouchstoneCursor>,
        limit: usize,
    ) -> Result<Self> {
        page_limit(limit)?;
        if let Some(cursor) = &after {
            cursor.validate_owners(cursor.db_id, subject.as_ref())?;
        }
        Ok(Self {
            subject,
            after,
            limit,
        })
    }
    pub fn subject(&self) -> Option<&TouchstoneSubject> {
        self.subject.as_ref()
    }
    pub fn after(&self) -> Option<&TouchstoneCursor> {
        self.after.as_ref()
    }
    pub const fn limit(&self) -> usize {
        self.limit
    }
    pub fn validate(&self, db_id: Ulid) -> Result<()> {
        page_limit(self.limit)?;
        if let Some(cursor) = &self.after {
            cursor.validate_owners(db_id, self.subject.as_ref())?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct TouchstoneReferrersRequest {
    target: NodeId,
    after: Option<TouchstoneCursor>,
    limit: usize,
}
impl TouchstoneReferrersRequest {
    pub fn new(target: NodeId, after: Option<TouchstoneCursor>, limit: usize) -> Result<Self> {
        page_limit(limit)?;
        if let Some(cursor) = &after {
            cursor.validate_referrers(cursor.db_id, target)?;
        }
        Ok(Self {
            target,
            after,
            limit,
        })
    }
    pub const fn target(&self) -> NodeId {
        self.target
    }
    pub fn after(&self) -> Option<&TouchstoneCursor> {
        self.after.as_ref()
    }
    pub const fn limit(&self) -> usize {
        self.limit
    }
    pub fn validate(&self, db_id: Ulid) -> Result<()> {
        page_limit(self.limit)?;
        if let Some(cursor) = &self.after {
            cursor.validate_referrers(db_id, self.target)?;
        }
        Ok(())
    }
}
fn page_limit(limit: usize) -> Result<()> {
    if limit == 0 || limit > MAX_TOUCHSTONE_PAGE_ITEMS {
        return Err(invalid("touchstone page limit must be 1..=32"));
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A subject-filtered page can be empty while `next` is present: the adapter
/// limits the physical owner keyset BEFORE filtering and advances after the last
/// examined owner. Empty items are not proof that the shelf/inventory is empty.
pub struct TouchstonePage {
    pub items: Vec<TouchstoneHeader>,
    pub next: Option<TouchstoneCursor>,
}

#[async_trait]
pub trait TouchstoneStore: Send + Sync {
    fn database_id(&self) -> Result<Ulid>;
    async fn summary_snapshot(&self, id: NodeId) -> Result<Option<SummarySnapshot>>;
    async fn get_touchstone(&self, owner: NodeId) -> Result<Option<TouchstoneRecord>>;
    async fn touchstones_page(&self, request: &TouchstonePageRequest) -> Result<TouchstonePage>;
    async fn touchstone_referrers_page(
        &self,
        request: &TouchstoneReferrersRequest,
    ) -> Result<TouchstonePage>;
}

#[derive(Default)]
struct CanonicalBytes(Vec<u8>);
impl CanonicalBytes {
    fn raw(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }
    fn tag(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.raw(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.raw(&v.to_be_bytes());
    }
    fn u128(&mut self, v: u128) {
        self.raw(&v.to_be_bytes());
    }
    fn text(&mut self, v: &str) {
        self.u64(v.len() as u64);
        self.raw(v.as_bytes());
    }
    fn optional_text(&mut self, v: Option<&str>) {
        match v {
            None => self.tag(0),
            Some(v) => {
                self.tag(1);
                self.text(v)
            }
        }
    }
    fn provenance(&mut self, p: &Provenance) {
        match p {
            Provenance::Web { url, fetched } => {
                self.tag(1);
                self.text(url.as_str());
                self.u128(*fetched)
            }
            Provenance::Conversation { session, turn } => {
                self.tag(2);
                self.u128(session.0);
                self.u32(*turn)
            }
            Provenance::External { source } => {
                self.tag(3);
                self.text(source.namespace());
                self.text(source.key());
                self.text(source.reference());
                self.optional_text(source.session());
                self.optional_text(source.revision());
                self.raw(&source.request_digest());
                self.tag(match source.request_codec() {
                    CaptureRequestCodec::CaptureV1 => 1,
                    CaptureRequestCodec::CaptureV2 => 2,
                    CaptureRequestCodec::EpisodeV1 => 3,
                    CaptureRequestCodec::EpisodeV2 => 4,
                    CaptureRequestCodec::TouchstoneV1 => 5,
                });
            }
            Provenance::Derived { from } => {
                self.tag(4);
                self.u64(from.len() as u64);
                for id in from.iter() {
                    self.u128(id.0.0)
                }
            }
        }
    }
    fn memory_kind(&mut self, k: &MemoryKind) {
        match k {
            MemoryKind::Semantic => self.tag(0),
            MemoryKind::Episode(f) => {
                self.tag(1);
                self.u128(f.root().node_id().0.0);
                self.u32(f.revision().get());
                match f.revises() {
                    None => self.tag(0),
                    Some(id) => {
                        self.tag(1);
                        self.u128(id.0.0)
                    }
                }
                match f.occurrence() {
                    crate::OccurrenceSpan::Unknown => self.tag(0),
                    crate::OccurrenceSpan::Point { at } => {
                        self.tag(1);
                        self.u128(at.get())
                    }
                    crate::OccurrenceSpan::Range { start, end } => {
                        self.tag(2);
                        self.u128(start.get());
                        self.u128(end.get())
                    }
                }
                self.optional_text(f.thread().map(crate::EpisodeThread::as_str));
                self.u128(f.recorded_at().get());
                self.optional_text(f.edit_reason().map(crate::EpisodeRevisionReason::as_str));
                match f.occurrence_contexts() {
                    None => self.tag(0),
                    Some(c) => {
                        self.tag(1);
                        self.u64(c.as_slice().len() as u64);
                        for c in c.iter() {
                            self.text(c.namespace());
                            self.text(c.key());
                            self.optional_text(c.label())
                        }
                    }
                }
            }
        }
    }
}

// Resource limits use the actual compact JSON wire. Identity codecs above are
// independent of JSON field order and string escaping.
fn compact_json_bytes(value: &impl Serialize) -> Result<usize> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|e| invalid(format!("cannot serialize touchstone record: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BodyRef, CaptureSource, EpisodeFacet, EpisodeTime, OccurrenceSpan};
    use serde_json::json;

    fn subject() -> TouchstoneSubject {
        TouchstoneSubject::new("project:mneme").unwrap()
    }
    fn node(id: u128, summary: &str, provenance: Provenance) -> Node {
        Node::try_new(
            NodeId(Ulid(id)),
            summary,
            BodyRef::new("inline://same").unwrap(),
            ["scope"],
            provenance,
            0.5,
            0.5,
            NodeStatus::Active,
            7,
        )
        .unwrap()
    }
    fn target(id: u128) -> Node {
        node(id, "material", Provenance::derived_empty())
    }
    fn input(snapshots: &[SummarySnapshot]) -> TouchstoneInput {
        TouchstoneInput::new(
            subject(),
            snapshots
                .iter()
                .map(|s| TouchstoneReference::new(s.db_id(), s.id(), s.digest()))
                .collect(),
        )
        .unwrap()
    }
    fn owner() -> Node {
        let source = CaptureSource::new_with_codec(
            "fixture",
            "owner",
            "fixture:touchstone",
            None,
            None,
            [9; 32],
            CaptureRequestCodec::TouchstoneV1,
        )
        .unwrap();
        node(
            source.node_id().0.0,
            "Why this disagreement mattered",
            Provenance::External { source },
        )
    }
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // Pinned independently with Python hashlib + struct.pack, not emitted by
    // the production encoder. These are byte-codec fixtures; enclosing-node
    // provenance/episode consistency is tested separately by from_node/validate.
    #[test]
    fn snapshot_codec_has_explicit_golden_provenance_and_episode_tags() {
        let mut snapshot = SummarySnapshot {
            db_id: Ulid(1),
            id: NodeId(Ulid(2)),
            summary: NodeSummary::new("é\0e\u{301}\"\\🦆").unwrap(),
            provenance: Provenance::derived([NodeId(Ulid(9)), NodeId(Ulid(8))]).unwrap(),
            created: 7,
            memory_kind: MemoryKind::Semantic,
        };
        assert_eq!(
            hex(&snapshot.canonical_bytes()),
            concat!(
                "00000000000000246d6e656d652e746f75636873746f6e652e73756d6d6172792d736e617073686f742e7631",
                "0000000000000000000000000000000100000000000000000000000000000002",
                "000000000000000cc3a90065cc81225cf09fa686040000000000000002",
                "0000000000000000000000000000000900000000000000000000000000000008",
                "0000000000000000000000000000000700"
            )
        );
        assert_eq!(
            snapshot.digest().to_hex(),
            "a347bcf55161c738989070b3ceaae8cd02c0f6ab315051508095e982f0b4537f"
        );
        let derived = snapshot.provenance.clone();
        snapshot.provenance = Provenance::Web {
            url: crate::WebUrl::new("https://example.test/a?x=1&y=2").unwrap(),
            fetched: u128::MAX,
        };
        assert_eq!(
            snapshot.digest().to_hex(),
            "7505dfaca1b885bb0271012eae85fc20ce4c3226523c3ac64126af7643eec2ce"
        );
        snapshot.provenance = Provenance::Conversation {
            session: Ulid(u128::MAX),
            turn: u32::MAX,
        };
        assert_eq!(
            snapshot.digest().to_hex(),
            "c23e1c566ca0a01447ee364a0169c81310ee52b950afa26b6249e8ffead07dd8"
        );
        for (codec, expected) in [
            (
                CaptureRequestCodec::CaptureV1,
                "51f5abb4c2b6be29a0f695e68a5cc186ba744299cd1c5240a19dadfe11acd207",
            ),
            (
                CaptureRequestCodec::CaptureV2,
                "6c18bc9b0c4f53fdc94aec5119620b8e434ce4baab066e5e2ffca4a05b1b2859",
            ),
            (
                CaptureRequestCodec::EpisodeV1,
                "2f5af64a5ff200846bd51c1fcd32b961f778b3f2902fa641e89a36611fe84739",
            ),
            (
                CaptureRequestCodec::EpisodeV2,
                "86d7e360c706bbcafad04b7a81e52d0e1ca9531c1451babd2339d2e104eefdd0",
            ),
            (
                CaptureRequestCodec::TouchstoneV1,
                "78e1355c3e75e2cf049433336f74b34766b18cb787a0a5dacbb5133887b101a8",
            ),
        ] {
            snapshot.provenance = Provenance::External {
                source: CaptureSource::new_with_codec(
                    "fixture",
                    "key",
                    "r:\"\\é\u{2028}x",
                    Some("opaque:🦆"),
                    Some("rev:1"),
                    [0x77; 32],
                    codec,
                )
                .unwrap(),
            };
            assert_eq!(snapshot.digest().to_hex(), expected, "{codec:?}");
        }
        snapshot.provenance = derived;
        let base = |occurrence| {
            EpisodeFacet::initial(
                NodeId(Ulid(2)),
                occurrence,
                None,
                EpisodeTime::new(i64::MAX as u128).unwrap(),
            )
            .unwrap()
        };
        for (occurrence, expected) in [
            (
                OccurrenceSpan::Unknown,
                "0fe689ddcae095c87cd38604a5c95827701e00d1b487676e005639cbf2e3c9c5",
            ),
            (
                OccurrenceSpan::point(EpisodeTime::new(0).unwrap()),
                "642de0462d42f14d769c3fa1d8708a052c831369fcfc529939f5171217a92231",
            ),
        ] {
            snapshot.memory_kind = MemoryKind::Episode(base(occurrence));
            assert_eq!(snapshot.digest().to_hex(), expected);
        }
        let contexts = vec![
            crate::OccurrenceContextRef::new("room", "🦆", Some("quote\"\\")).unwrap(),
            crate::OccurrenceContextRef::new("project", "mneme", None::<&str>).unwrap(),
        ];
        let facet = base(
            OccurrenceSpan::range(
                EpisodeTime::new(1).unwrap(),
                EpisodeTime::new(i64::MAX as u128).unwrap(),
            )
            .unwrap(),
        )
        .with_occurrence_contexts(crate::OccurrenceContexts::new(contexts.clone()).unwrap());
        snapshot.memory_kind = MemoryKind::Episode(facet.clone());
        assert_eq!(
            snapshot.digest().to_hex(),
            "e8d0a5b46293c71d3f91d8eae4ef086b0a1ac977e99ddb689a27ef2b96ee86f5"
        );
        let mut reversed = contexts;
        reversed.reverse();
        snapshot.memory_kind = MemoryKind::Episode(
            facet.with_occurrence_contexts(crate::OccurrenceContexts::new(reversed).unwrap()),
        );
        assert_eq!(
            snapshot.digest().to_hex(),
            "e8d0a5b46293c71d3f91d8eae4ef086b0a1ac977e99ddb689a27ef2b96ee86f5"
        );
        snapshot.memory_kind = MemoryKind::Episode(
            EpisodeFacet::revised(
                crate::EpisodeId::new(NodeId(Ulid(2))),
                NodeId(Ulid(3)),
                crate::EpisodeRevision::new(u32::MAX),
                OccurrenceSpan::Unknown,
                Some(crate::EpisodeThread::new("thread:🦆").unwrap()),
                EpisodeTime::new(i64::MAX as u128).unwrap(),
                crate::EpisodeRevisionReason::new("correct \"\\").unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(
            snapshot.digest().to_hex(),
            "dcc391be47bdc9c8a90af65c1be35345d1af2e46998f4f0dab19b3711d3329ef"
        );
    }

    #[test]
    fn input_has_strict_types_canonical_identity_and_exact_json_budget() {
        for invalid in ["", " ", " padded", "padded ", "a\0b", "a\nb"] {
            assert!(TouchstoneSubject::new(invalid).is_err());
            assert!(serde_json::from_value::<TouchstoneSubject>(json!(invalid)).is_err());
        }
        assert!(TouchstoneSubject::new("é".repeat(128)).is_ok());
        assert!(TouchstoneSubject::new("é".repeat(129)).is_err());
        for invalid in [
            "",
            &"0".repeat(63),
            &"0".repeat(65),
            &"A".repeat(64),
            &"g".repeat(64),
        ] {
            assert!(SummarySnapshotDigest::from_hex(invalid).is_err());
            assert!(serde_json::from_value::<SummarySnapshotDigest>(json!(invalid)).is_err());
        }
        let a = TouchstoneReference::new(
            Ulid(0),
            NodeId(Ulid(1)),
            SummarySnapshotDigest::new([0x11; 32]),
        );
        let b = TouchstoneReference::new(
            Ulid(0),
            NodeId(Ulid(2)),
            SummarySnapshotDigest::new([0x22; 32]),
        );
        let first = TouchstoneInput::new(subject(), vec![a.clone(), b.clone()]).unwrap();
        let reordered = TouchstoneInput::new(subject(), vec![b, a.clone()]).unwrap();
        assert_eq!(first, reordered);
        assert_eq!(first.canonical_bytes(), reordered.canonical_bytes());
        assert_eq!(
            hex(&first.canonical_bytes()),
            concat!(
                "000000000000000d70726f6a6563743a6d6e656d650000000000000002",
                "0000000000000000000000000000000000000000000000000000000000000001",
                "1111111111111111111111111111111111111111111111111111111111111111",
                "0000000000000000000000000000000000000000000000000000000000000002",
                "2222222222222222222222222222222222222222222222222222222222222222"
            )
        );
        assert_eq!(
            hex(&Sha256::digest(first.canonical_bytes())),
            "3c086ce26ae501688a5dbbb8d24b2ed5e43354091209534da9f8acc4a3506bf3"
        );
        // 48 fixed-width references plus 147 ASCII subject bytes is exactly
        // 8192 compact JSON bytes. One more subject byte must fail, not truncate.
        let boundary_refs = (1..=48)
            .map(|id| {
                TouchstoneReference::new(
                    Ulid(0),
                    NodeId(Ulid(id)),
                    SummarySnapshotDigest::new([0; 32]),
                )
            })
            .collect::<Vec<_>>();
        let boundary = TouchstoneInput::new(
            TouchstoneSubject::new("x".repeat(147)).unwrap(),
            boundary_refs.clone(),
        )
        .unwrap();
        assert_eq!(boundary.compact_json_len(), MAX_TOUCHSTONE_INPUT_BYTES);
        assert!(
            TouchstoneInput::new(
                TouchstoneSubject::new("x".repeat(148)).unwrap(),
                boundary_refs
            )
            .is_err()
        );

        assert_eq!(
            first.compact_json_len(),
            serde_json::to_vec(&first).unwrap().len()
        );
        let contradictory =
            TouchstoneReference::new(a.db_id(), a.id(), SummarySnapshotDigest::new([0x55; 32]));
        assert!(TouchstoneInput::new(subject(), vec![a, contradictory]).is_err());
        assert!(TouchstoneInput::new(subject(), vec![]).is_err());
        assert!(first.validate_database_id(Ulid(1)).is_err());
        for bad in [
            json!({"subject":"subject","references":[]}),
            json!({"subject":null,"references":[]}),
            json!({"subject":"subject","references":null}),
            json!({"subject":"subject","references":[],"importance":1}),
        ] {
            assert!(serde_json::from_value::<TouchstoneInput>(bad).is_err());
        }
        let refs = (0..200)
            .map(|n| {
                TouchstoneReference::new(
                    Ulid(0),
                    NodeId(Ulid(n)),
                    SummarySnapshotDigest::new([0; 32]),
                )
            })
            .collect();
        assert!(TouchstoneInput::new(subject(), refs).is_err());
        for text in ["quote\"backslash\\", "é", "🦆", "a\u{2028}b"] {
            let value = TouchstoneInput::new(
                TouchstoneSubject::new(text).unwrap(),
                first.references.to_vec(),
            )
            .unwrap();
            assert_eq!(
                value.compact_json_len(),
                serde_json::to_vec(&value).unwrap().len()
            );
        }
    }

    #[test]
    fn record_checks_expectations_bytes_and_retains_deleted_target_evidence() {
        let db = Ulid(10);
        let targets = [target(1), target(2)];
        let snapshots = targets
            .iter()
            .map(|t| SummarySnapshot::from_node(db, t).unwrap())
            .collect::<Vec<_>>();
        let request = input(&snapshots);
        let authored_owner = owner();
        let record =
            capture_touchstone_snapshots(db, authored_owner.id(), &request, &targets).unwrap();
        assert_eq!(record.references(), snapshots);
        assert_eq!(
            record.compact_json_len().unwrap(),
            serde_json::to_vec(&record).unwrap().len()
        );
        assert_eq!(
            serde_json::from_value::<TouchstoneRecord>(serde_json::to_value(&record).unwrap())
                .unwrap(),
            record
        );
        assert!(
            validate_touchstone_catalog(db, &[authored_owner.clone()], &[record.clone()]).is_ok(),
            "deleted target is historical evidence, not an orphan owner"
        );
        assert!(
            validate_touchstone_catalog(db, &[authored_owner.clone()], &[]).is_err(),
            "codec requires record"
        );
        assert!(
            validate_touchstone_catalog(db, &[], &[record.clone()]).is_err(),
            "record requires owner"
        );
        assert!(
            validate_touchstone_catalog(
                db,
                &[authored_owner.clone()],
                &[record.clone(), record.clone()]
            )
            .is_err()
        );
        assert!(
            validate_touchstone_catalog(Ulid(11), &[authored_owner], &[record.clone()]).is_err()
        );
        assert!(matches!(
            capture_touchstone_snapshots(db, record.owner(), &request, &targets[..1]),
            Err(Error::Conflict(_))
        ));
        let changed = [
            node(1, "different meaning", Provenance::derived_empty()),
            target(2),
        ];
        assert!(matches!(
            capture_touchstone_snapshots(db, record.owner(), &request, &changed),
            Err(Error::Conflict(_))
        ));
        let mut reordered = serde_json::to_value(&record).unwrap();
        reordered["references"].as_array_mut().unwrap().reverse();
        assert!(
            serde_json::from_value::<TouchstoneRecord>(reordered).is_err(),
            "stored order is canonical, unlike unordered authored input"
        );
        let self_snapshot = SummarySnapshot::from_node(db, &owner()).unwrap();
        assert!(
            TouchstoneRecord::new(
                owner().id(),
                &input(&[self_snapshot.clone()]),
                vec![self_snapshot]
            )
            .is_err()
        );
        // JSON control escaping counts toward the full stored-record budget.
        let text = format!("{}x", "\0".repeat(crate::MAX_NODE_SUMMARY_BYTES - 1));
        let large_targets = (1..=32)
            .map(|id| node(id, &text, Provenance::derived_empty()))
            .collect::<Vec<_>>();
        let large = large_targets
            .iter()
            .map(|t| SummarySnapshot::from_node(db, t).unwrap())
            .collect::<Vec<_>>();
        assert!(TouchstoneRecord::new(owner().id(), &input(&large), large).is_err());
    }

    #[test]
    fn owner_guard_freezes_authorship_not_lifecycle_or_relevance() {
        let original = owner();
        let mut telemetry = original.clone();
        telemetry.record_exposure(9);
        telemetry.record_grounded_use(10);
        telemetry.set_status(NodeStatus::Archived);
        telemetry.set_confidence(crate::Confidence::new(0.8).unwrap());
        assert!(validate_touchstone_owner_replacement(&original, &telemetry).is_ok());
        let wire = serde_json::to_value(&original).unwrap();
        for (field, value) in [
            ("summary", json!("new meaning")),
            ("body", json!("inline://changed")),
            ("tags", json!(["new"])),
            ("created", json!(8)),
            (
                "origin_commit",
                json!("3333333333333333333333333333333333333333"),
            ),
        ] {
            let mut changed = wire.clone();
            changed[field] = value;
            let changed: Node = serde_json::from_value(changed).unwrap();
            assert!(
                matches!(
                    validate_touchstone_owner_replacement(&original, &changed),
                    Err(Error::Conflict(_))
                ),
                "{field}"
            );
        }
        let source = match original.provenance() {
            Provenance::External { source } => source.clone(),
            _ => unreachable!(),
        };
        let proof = crate::CaptureReplayProof::touchstone(source.clone()).unwrap();
        assert!(proof.matches_source(&source));
        let ordinary = CaptureSource::new_with_codec(
            source.namespace(),
            source.key(),
            source.reference(),
            source.session(),
            source.revision(),
            source.request_digest(),
            CaptureRequestCodec::CaptureV2,
        )
        .unwrap();
        assert!(!proof.matches_source(&ordinary));
        assert!(crate::CaptureReplayProof::semantic(source, [0; 32], [0; 32]).is_err());
        assert!(crate::CaptureReplayProof::touchstone(ordinary).is_err());
    }

    #[test]
    fn summary_digest_excludes_body_tags_status_ranking_and_host_anchor() {
        let db = Ulid(1);
        let original = target(2);
        let snapshot = SummarySnapshot::from_node(db, &original).unwrap();
        let wire = serde_json::to_value(&original).unwrap();
        for (field, value) in [
            ("body", json!("inline://different-unavailable-body")),
            ("tags", json!(["changed"])),
            ("status", json!("Archived")),
            ("confidence", json!(0.9)),
            ("stability", json!(0.8)),
            (
                "origin_commit",
                json!("3333333333333333333333333333333333333333"),
            ),
            ("exposure_count", json!(9)),
        ] {
            let mut changed = wire.clone();
            changed[field] = value;
            let changed: Node = serde_json::from_value(changed).unwrap();
            assert_eq!(
                snapshot,
                SummarySnapshot::from_node(db, &changed).unwrap(),
                "{field} excluded from summary-only equivalence"
            );
        }
        for (field, value) in [
            ("summary", json!("changed")),
            ("created", json!(8)),
            (
                "provenance",
                json!({"Conversation":{"session":Ulid(3),"turn":1}}),
            ),
        ] {
            let mut changed = wire.clone();
            changed[field] = value;
            let changed: Node = serde_json::from_value(changed).unwrap();
            assert_ne!(
                snapshot.digest(),
                SummarySnapshot::from_node(db, &changed).unwrap().digest(),
                "{field} meaningful"
            );
        }
        assert_ne!(
            snapshot.digest(),
            SummarySnapshot::from_node(Ulid(2), &original)
                .unwrap()
                .digest()
        );
        let projection = serde_json::to_value(&snapshot).unwrap();
        for excluded in [
            "body",
            "body_ref",
            "tags",
            "status",
            "confidence",
            "origin_commit",
            "current_edition_id",
        ] {
            assert!(projection.get(excluded).is_none(), "{excluded}");
        }
    }

    #[test]
    fn cursors_bind_database_selection_and_continue_beyond_one_resource_page() {
        let db = Ulid(1);
        let after = NodeId(Ulid(1000));
        for cursor in [
            TouchstoneCursor::owners(db, None, after),
            TouchstoneCursor::owners(
                db,
                Some(TouchstoneSubject::new("mémöry:🦆").unwrap()),
                after,
            ),
            TouchstoneCursor::referrers(db, NodeId(Ulid(2)), after),
        ] {
            let encoded = cursor.to_string();
            assert_eq!(encoded.parse::<TouchstoneCursor>().unwrap(), cursor);
            assert_eq!(
                serde_json::from_value::<TouchstoneCursor>(serde_json::to_value(&cursor).unwrap())
                    .unwrap(),
                cursor
            );
            assert_eq!(cursor.after(), after);
        }
        let cursor = TouchstoneCursor::owners(db, None, after);
        assert!(
            TouchstonePageRequest::new(None, Some(cursor.clone()), 32)
                .unwrap()
                .validate(db)
                .is_ok()
        );
        assert!(
            TouchstonePageRequest::new(None, Some(cursor.clone()), 32)
                .unwrap()
                .validate(Ulid(2))
                .is_err()
        );
        assert!(TouchstonePageRequest::new(Some(subject()), Some(cursor.clone()), 32).is_err());
        assert!(TouchstoneReferrersRequest::new(NodeId(Ulid(2)), Some(cursor), 32).is_err());
        for limit in [0, 33, usize::MAX] {
            assert!(TouchstonePageRequest::new(None, None, limit).is_err());
            assert!(TouchstoneReferrersRequest::new(NodeId(Ulid(2)), None, limit).is_err());
        }
        for bad in [
            "t2o:00000000000000000000000001:00000000000000000000000002:-",
            "t1o:00000000000000000000000001:00000000000000000000000002:00",
            "t1o:00000000000000000000000001:00000000000000000000000002:AA",
        ] {
            assert!(bad.parse::<TouchstoneCursor>().is_err());
        }
    }
}
