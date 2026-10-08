//! Selectively authored episodes: stable roots, immutable editions and bounded reads.
//!
//! These types do not put episodes into the semantic retrieval or lifecycle lane.
//! Cursor positions are filter/database-bound keysets, not snapshot tokens.

use crate::{CaptureSource, Edge, MAX_CAPTURE_SESSION_BYTES, Node, NodeId, NodeSummary, Timestamp};
use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::{fmt, str::FromStr};
use ulid::Ulid;

pub const MAX_EPISODE_PAGE_ITEMS: usize = 32;
pub const DEFAULT_EPISODE_PAGE_ITEMS: usize = 8;
pub const MAX_EPISODE_SUMMARY_BYTES: usize = 2048;
pub const MAX_EPISODE_BODY_BYTES: usize = 16 * 1024;
pub const MAX_EPISODE_RESPONSE_BYTES: usize = 32 * 1024;
pub const MAX_EPISODE_LINKS: usize = 8;
pub const MAX_EPISODE_THREAD_BYTES: usize = 128;
pub const MAX_EPISODE_CUE_BYTES: usize = 4096;
pub const MAX_EPISODE_REVISION_REASON_BYTES: usize = 1024;
pub const MAX_EPISODE_CURSOR_BYTES: usize = 1024;
pub const MAX_OCCURRENCE_CONTEXTS_JSON_BYTES: usize = 1024;

/// An expected absence of the episodic read capability, not a failed query.
/// General recall may retain semantic results for these two cases only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeUnavailableReason {
    #[error("this graph adapter does not support episode operations")]
    AdapterUnsupported,
    #[error("this store needs an explicit episode-generation upgrade")]
    StoreNotUpgraded,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid episode: {0}")]
pub struct EpisodeValidationError(pub &'static str);

impl From<EpisodeValidationError> for crate::ports::Error {
    fn from(value: EpisodeValidationError) -> Self {
        Self::InvalidInput(value.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EpisodeId(NodeId);
impl EpisodeId {
    pub const fn new(id: NodeId) -> Self {
        Self(id)
    }
    pub const fn node_id(self) -> NodeId {
        self.0
    }
    pub const fn get(self) -> NodeId {
        self.0
    }
}
impl From<NodeId> for EpisodeId {
    fn from(id: NodeId) -> Self {
        Self(id)
    }
}
impl From<EpisodeId> for NodeId {
    fn from(id: EpisodeId) -> Self {
        id.0
    }
}
impl fmt::Display for EpisodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.0.fmt(f)
    }
}

macro_rules! bounded_text {
    ($name:ident, $max:ident, $control_free:expr) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl AsRef<str>) -> Result<Self, EpisodeValidationError> {
                let value = value.as_ref();
                if value.is_empty() || value.trim().is_empty() {
                    return Err(EpisodeValidationError(concat!(
                        stringify!($name),
                        " is blank"
                    )));
                }
                if value.len() > $max {
                    return Err(EpisodeValidationError(concat!(
                        stringify!($name),
                        " exceeds byte limit"
                    )));
                }
                if $control_free && (value.trim() != value || value.chars().any(char::is_control)) {
                    return Err(EpisodeValidationError(concat!(
                        stringify!($name),
                        " must be trimmed and control-free"
                    )));
                }
                Ok(Self(value.to_owned()))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
            }
        }
    };
}
bounded_text!(EpisodeThread, MAX_EPISODE_THREAD_BYTES, true);
bounded_text!(EpisodeRecordingSession, MAX_CAPTURE_SESSION_BYTES, true);
bounded_text!(EpisodeCue, MAX_EPISODE_CUE_BYTES, true);
bounded_text!(
    EpisodeRevisionReason,
    MAX_EPISODE_REVISION_REASON_BYTES,
    true
);

// Missing new optional fields mean unknown; explicitly present values must
// satisfy the field type rather than quietly collapsing JSON null to absence.
fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Where an episode happened, independently of who recorded it. Identifiers
/// are exact opaque strings: no registry, normalization, or inferred aliases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "OccurrenceContextRefWire")]
pub struct OccurrenceContextRef {
    namespace: String,
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OccurrenceContextRefWire {
    namespace: String,
    key: String,
    #[serde(default, deserialize_with = "deserialize_present")]
    label: Option<String>,
}
impl TryFrom<OccurrenceContextRefWire> for OccurrenceContextRef {
    type Error = EpisodeValidationError;
    fn try_from(wire: OccurrenceContextRefWire) -> Result<Self, Self::Error> {
        Self::new(wire.namespace, wire.key, wire.label.as_deref())
    }
}
impl OccurrenceContextRef {
    pub fn new(
        namespace: impl AsRef<str>,
        key: impl AsRef<str>,
        label: Option<&str>,
    ) -> Result<Self, EpisodeValidationError> {
        fn text(value: &str) -> Result<(), EpisodeValidationError> {
            if value.is_empty() || value.trim().is_empty() {
                return Err(EpisodeValidationError("occurrence context text is blank"));
            }
            if value.trim() != value || value.chars().any(char::is_control) {
                return Err(EpisodeValidationError(
                    "occurrence context text must be trimmed and control-free",
                ));
            }
            Ok(())
        }
        text(namespace.as_ref())?;
        text(key.as_ref())?;
        if let Some(label) = label {
            text(label)?;
        }
        Ok(Self {
            namespace: namespace.as_ref().to_owned(),
            key: key.as_ref().to_owned(),
            label: label.map(str::to_owned),
        })
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
    fn compact_json_len(&self) -> usize {
        // Controls are forbidden, so serde_json escapes only quote/backslash.
        fn string_len(value: &str) -> usize {
            value.len().saturating_add(2).saturating_add(
                value
                    .bytes()
                    .filter(|byte| matches!(byte, b'"' | b'\\'))
                    .count(),
            )
        }
        let mut bytes = b"{\"namespace\":,\"key\":}"
            .len()
            .saturating_add(string_len(&self.namespace))
            .saturating_add(string_len(&self.key));
        if let Some(label) = &self.label {
            bytes = bytes
                .saturating_add(b",\"label\":".len())
                .saturating_add(string_len(label));
        }
        bytes
    }
}

/// A canonical nonempty collection bounded by its complete compact JSON size,
/// not an arbitrary item count. Duplicate namespace/key pairs are rejected even
/// when their labels differ. Reordering never creates new authored intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct OccurrenceContexts(Vec<OccurrenceContextRef>);
impl OccurrenceContexts {
    pub fn new(contexts: Vec<OccurrenceContextRef>) -> Result<Self, EpisodeValidationError> {
        if contexts.is_empty() {
            return Err(EpisodeValidationError(
                "occurrence contexts cannot be empty",
            ));
        }
        let mut value = Self(contexts);
        // Size is order-independent: refuse oversized metadata before sorting.
        if value.compact_json_len() > MAX_OCCURRENCE_CONTEXTS_JSON_BYTES {
            return Err(EpisodeValidationError(
                "occurrence contexts exceed compact JSON byte limit",
            ));
        }
        value
            .0
            .sort_unstable_by(|a, b| (&a.namespace, &a.key).cmp(&(&b.namespace, &b.key)));
        if value
            .0
            .windows(2)
            .any(|pair| pair[0].namespace == pair[1].namespace && pair[0].key == pair[1].key)
        {
            return Err(EpisodeValidationError(
                "duplicate occurrence context namespace/key",
            ));
        }
        Ok(value)
    }
    pub fn as_slice(&self) -> &[OccurrenceContextRef] {
        &self.0
    }
    pub fn iter(&self) -> std::slice::Iter<'_, OccurrenceContextRef> {
        self.0.iter()
    }
    pub fn compact_json_len(&self) -> usize {
        self.0
            .iter()
            .fold(2usize, |bytes, context| {
                bytes.saturating_add(context.compact_json_len())
            })
            .saturating_add(self.0.len().saturating_sub(1))
    }
}
impl<'de> Deserialize<'de> for OccurrenceContexts {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(Vec::<OccurrenceContextRef>::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct EpisodeTime(u64);
impl EpisodeTime {
    pub fn new(value: Timestamp) -> Result<Self, EpisodeValidationError> {
        if value > i64::MAX as u128 {
            return Err(EpisodeValidationError(
                "time exceeds signed epoch-millisecond range",
            ));
        }
        Ok(Self(value as u64))
    }
    pub const fn get(self) -> Timestamp {
        self.0 as u128
    }
}
impl<'de> Deserialize<'de> for EpisodeTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(u64::deserialize(d)? as u128).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EpisodeRevision(u32);
impl EpisodeRevision {
    pub const INITIAL: Self = Self(0);
    pub const fn new(value: u32) -> Self {
        Self(value)
    }
    pub const fn get(self) -> u32 {
        self.0
    }
    pub fn next(self) -> Result<Self, EpisodeValidationError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(EpisodeValidationError("revision overflow"))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    deny_unknown_fields,
    try_from = "OccurrenceWire"
)]
pub enum OccurrenceSpan {
    #[default]
    Unknown,
    Point {
        at: EpisodeTime,
    },
    Range {
        start: EpisodeTime,
        end: EpisodeTime,
    },
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum OccurrenceWire {
    Unknown,
    Point {
        at: EpisodeTime,
    },
    Range {
        start: EpisodeTime,
        end: EpisodeTime,
    },
}
impl TryFrom<OccurrenceWire> for OccurrenceSpan {
    type Error = EpisodeValidationError;
    fn try_from(wire: OccurrenceWire) -> Result<Self, Self::Error> {
        let result = match wire {
            OccurrenceWire::Unknown => Self::Unknown,
            OccurrenceWire::Point { at } => Self::Point { at },
            OccurrenceWire::Range { start, end } => Self::Range { start, end },
        };
        result.validate()?;
        Ok(result)
    }
}
impl OccurrenceSpan {
    pub fn point(at: EpisodeTime) -> Self {
        Self::Point { at }
    }
    pub fn range(start: EpisodeTime, end: EpisodeTime) -> Result<Self, EpisodeValidationError> {
        let value = Self::Range { start, end };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<(), EpisodeValidationError> {
        if matches!(self, Self::Range { start, end } if start >= end) {
            return Err(EpisodeValidationError(
                "occurrence range requires start < end; use point for equal endpoints",
            ));
        }
        Ok(())
    }
    pub fn start(&self) -> Option<EpisodeTime> {
        match self {
            Self::Unknown => None,
            Self::Point { at } => Some(*at),
            Self::Range { start, .. } => Some(*start),
        }
    }
    pub fn end(&self) -> Option<EpisodeTime> {
        match self {
            Self::Unknown => None,
            Self::Point { at } => Some(*at),
            Self::Range { end, .. } => Some(*end),
        }
    }
    pub fn overlaps(&self, window: &EpisodeTimeWindow) -> bool {
        match (self.start(), self.end()) {
            (Some(start), Some(end)) => {
                window.from.is_none_or(|from| end >= from)
                    && window.through.is_none_or(|through| start <= through)
            }
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeIdentity {
    pub episode_id: EpisodeId,
    pub edition_id: NodeId,
    pub revision: EpisodeRevision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "EpisodeFacetWire")]
pub struct EpisodeFacet {
    episode_id: EpisodeId,
    revision: EpisodeRevision,
    revises: Option<NodeId>,
    occurred: OccurrenceSpan,
    thread: Option<EpisodeThread>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    occurrence_contexts: Option<OccurrenceContexts>,
    recorded_at: EpisodeTime,
    edit_reason: Option<EpisodeRevisionReason>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EpisodeFacetWire {
    episode_id: EpisodeId,
    revision: EpisodeRevision,
    revises: Option<NodeId>,
    occurred: OccurrenceSpan,
    thread: Option<EpisodeThread>,
    #[serde(default, deserialize_with = "deserialize_present")]
    occurrence_contexts: Option<OccurrenceContexts>,
    recorded_at: EpisodeTime,
    edit_reason: Option<EpisodeRevisionReason>,
}
impl TryFrom<EpisodeFacetWire> for EpisodeFacet {
    type Error = EpisodeValidationError;
    fn try_from(w: EpisodeFacetWire) -> Result<Self, Self::Error> {
        let value = Self {
            episode_id: w.episode_id,
            revision: w.revision,
            revises: w.revises,
            occurred: w.occurred,
            thread: w.thread,
            occurrence_contexts: w.occurrence_contexts,
            recorded_at: w.recorded_at,
            edit_reason: w.edit_reason,
        };
        value.validate()?;
        Ok(value)
    }
}
impl EpisodeFacet {
    pub fn initial(
        id: NodeId,
        occurred: OccurrenceSpan,
        thread: Option<EpisodeThread>,
        recorded_at: EpisodeTime,
    ) -> Result<Self, EpisodeValidationError> {
        let value = Self {
            episode_id: EpisodeId::new(id),
            revision: EpisodeRevision::INITIAL,
            revises: None,
            occurred,
            thread,
            occurrence_contexts: None,
            recorded_at,
            edit_reason: None,
        };
        value.validate()?;
        Ok(value)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn revised(
        root: EpisodeId,
        predecessor: NodeId,
        revision: EpisodeRevision,
        occurred: OccurrenceSpan,
        thread: Option<EpisodeThread>,
        recorded_at: EpisodeTime,
        reason: EpisodeRevisionReason,
    ) -> Result<Self, EpisodeValidationError> {
        let value = Self {
            episode_id: root,
            revision,
            revises: Some(predecessor),
            occurred,
            thread,
            occurrence_contexts: None,
            recorded_at,
            edit_reason: Some(reason),
        };
        value.validate()?;
        Ok(value)
    }
    pub fn with_occurrence_contexts(mut self, contexts: OccurrenceContexts) -> Self {
        self.occurrence_contexts = Some(contexts);
        self
    }
    pub fn occurrence_contexts(&self) -> Option<&OccurrenceContexts> {
        self.occurrence_contexts.as_ref()
    }
    pub fn validate(&self) -> Result<(), EpisodeValidationError> {
        self.occurred.validate()?;
        if self.revision == EpisodeRevision::INITIAL {
            if self.revises.is_some() || self.edit_reason.is_some() {
                return Err(EpisodeValidationError(
                    "initial edition cannot revise or carry an editorial reason",
                ));
            }
        } else if self.revises.is_none() || self.edit_reason.is_none() {
            return Err(EpisodeValidationError(
                "revision requires predecessor and editorial reason",
            ));
        }
        Ok(())
    }
    pub fn validate_node(&self, node: &Node) -> Result<(), EpisodeValidationError> {
        self.validate()?;
        EpisodeTime::new(node.created())?;
        if node.summary().len() > MAX_EPISODE_SUMMARY_BYTES {
            return Err(EpisodeValidationError("summary exceeds episode byte limit"));
        }
        if node.has_tag("core") {
            return Err(EpisodeValidationError("episodes cannot be core memory"));
        }
        match node.provenance() {
            crate::Provenance::External { source } if source.node_id() == node.id() => {}
            crate::Provenance::External { .. } => {
                return Err(EpisodeValidationError(
                    "episode identity differs from capture source",
                ));
            }
            _ => {
                return Err(EpisodeValidationError(
                    "episode requires source-authored capture provenance",
                ));
            }
        }
        if !node.is_active() {
            return Err(EpisodeValidationError(
                "episode edition requires neutral active status",
            ));
        }
        if self.revision == EpisodeRevision::INITIAL {
            if self.episode_id.node_id() != node.id() || self.recorded_at.get() != node.created() {
                return Err(EpisodeValidationError(
                    "initial episode identity or recording time differs from edition",
                ));
            }
        } else if self.episode_id.node_id() == node.id() || self.revises == Some(node.id()) {
            return Err(EpisodeValidationError(
                "revision edition must differ from root and predecessor",
            ));
        }
        Ok(())
    }
    pub fn root(&self) -> EpisodeId {
        self.episode_id
    }
    pub fn revises(&self) -> Option<NodeId> {
        self.revises
    }
    pub fn revision(&self) -> EpisodeRevision {
        self.revision
    }
    pub fn occurrence(&self) -> &OccurrenceSpan {
        &self.occurred
    }
    pub fn thread(&self) -> Option<&EpisodeThread> {
        self.thread.as_ref()
    }
    pub fn recorded_at(&self) -> EpisodeTime {
        self.recorded_at
    }
    pub fn edit_reason(&self) -> Option<&EpisodeRevisionReason> {
        self.edit_reason.as_ref()
    }
    pub fn identity(&self, edition_id: NodeId) -> EpisodeIdentity {
        EpisodeIdentity {
            episode_id: self.episode_id,
            edition_id,
            revision: self.revision,
        }
    }
    pub fn validate_successor(&self, predecessor: &Node) -> Result<(), EpisodeValidationError> {
        let previous = predecessor
            .episode()
            .ok_or(EpisodeValidationError("predecessor is not an episode"))?;
        if self.root() != previous.root()
            || self.revises != Some(predecessor.id())
            || self.revision != previous.revision.next()?
            || self.recorded_at != previous.recorded_at
        {
            return Err(EpisodeValidationError(
                "revision does not extend predecessor lineage",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "episode",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MemoryKind {
    #[default]
    Semantic,
    Episode(EpisodeFacet),
}
impl MemoryKind {
    pub fn is_semantic(&self) -> bool {
        matches!(self, Self::Semantic)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpisodeWriteExpectation {
    NewRoot,
    CurrentEdition(NodeId),
}
pub struct EpisodeCommit<'a> {
    pub node: &'a Node,
    pub embedding: &'a [f32],
    pub links: &'a [Edge],
    pub expectation: EpisodeWriteExpectation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EpisodeCommitOutcome {
    Applied(EpisodeIdentity),
    AlreadyApplied(EpisodeIdentity),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EpisodeRecord {
    pub identity: EpisodeIdentity,
    pub node: Node,
    pub current_edition_id: NodeId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct EpisodePageLimit(u8);
impl EpisodePageLimit {
    pub fn new(value: usize) -> Result<Self, EpisodeValidationError> {
        if !(1..=MAX_EPISODE_PAGE_ITEMS).contains(&value) {
            return Err(EpisodeValidationError("page limit must be 1..=32"));
        }
        Ok(Self(value as u8))
    }
    pub fn get(self) -> usize {
        self.0 as usize
    }
}
impl Default for EpisodePageLimit {
    fn default() -> Self {
        Self(DEFAULT_EPISODE_PAGE_ITEMS as u8)
    }
}
impl<'de> Deserialize<'de> for EpisodePageLimit {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(usize::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EpisodeHeader {
    pub identity: EpisodeIdentity,
    pub summary: NodeSummary,
    pub occurred: OccurrenceSpan,
    pub recorded_at: EpisodeTime,
    pub edition_recorded_at: EpisodeTime,
    pub thread: Option<EpisodeThread>,
    /// Exact recorder session for this immutable edition; never an occurrence
    /// coordinate or an inferred/root-inherited source label.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub recording_session: Option<EpisodeRecordingSession>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub occurrence_contexts: Option<OccurrenceContexts>,
    pub current_edition_id: NodeId,
}
impl EpisodeHeader {
    pub fn from_node(
        node: &Node,
        current_edition_id: NodeId,
    ) -> Result<Self, EpisodeValidationError> {
        let facet = node
            .episode()
            .ok_or(EpisodeValidationError("node is not an episode"))?;
        facet.validate_node(node)?;
        Ok(Self {
            identity: facet.identity(node.id()),
            summary: NodeSummary::new(node.summary())
                .map_err(|_| EpisodeValidationError("invalid episode summary"))?,
            occurred: facet.occurrence().clone(),
            recorded_at: facet.recorded_at(),
            edition_recorded_at: EpisodeTime::new(node.created())?,
            thread: facet.thread().cloned(),
            recording_session: match node.provenance() {
                crate::Provenance::External { source } => source
                    .session()
                    .map(EpisodeRecordingSession::new)
                    .transpose()?,
                _ => None,
            },
            occurrence_contexts: facet.occurrence_contexts().cloned(),
            current_edition_id,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeGet {
    pub episode_id: EpisodeId,
    pub edition_id: Option<NodeId>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeTimelineAxis {
    #[default]
    Recorded,
    Occurred,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOrder {
    #[default]
    NewestFirst,
    OldestFirst,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "EpisodeTimeWindowWire")]
pub struct EpisodeTimeWindow {
    pub from: Option<EpisodeTime>,
    pub through: Option<EpisodeTime>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EpisodeTimeWindowWire {
    from: Option<EpisodeTime>,
    through: Option<EpisodeTime>,
}
impl TryFrom<EpisodeTimeWindowWire> for EpisodeTimeWindow {
    type Error = EpisodeValidationError;
    fn try_from(w: EpisodeTimeWindowWire) -> Result<Self, Self::Error> {
        Self::new(w.from, w.through)
    }
}
impl EpisodeTimeWindow {
    pub fn new(
        from: Option<EpisodeTime>,
        through: Option<EpisodeTime>,
    ) -> Result<Self, EpisodeValidationError> {
        let value = Self { from, through };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<(), EpisodeValidationError> {
        if self.from.is_none() && self.through.is_none() {
            return Err(EpisodeValidationError("time window requires a bound"));
        }
        if matches!((self.from,self.through),(Some(a),Some(b)) if a>b) {
            return Err(EpisodeValidationError("time window starts after its end"));
        }
        Ok(())
    }
    pub fn contains(&self, time: EpisodeTime) -> bool {
        self.from.is_none_or(|from| time >= from)
            && self.through.is_none_or(|through| time <= through)
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "window",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum EpisodeOccurrenceFilter {
    #[default]
    Any,
    Unknown,
    Overlaps(EpisodeTimeWindow),
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeFilter {
    pub thread: Option<EpisodeThread>,
    #[serde(default)]
    pub occurrence: EpisodeOccurrenceFilter,
}
impl EpisodeFilter {
    pub fn validate(&self) -> Result<(), EpisodeValidationError> {
        if let EpisodeOccurrenceFilter::Overlaps(window) = &self.occurrence {
            window.validate()?;
        }
        Ok(())
    }
    pub fn matches(&self, facet: &EpisodeFacet) -> bool {
        self.thread
            .as_ref()
            .is_none_or(|thread| facet.thread() == Some(thread))
            && match &self.occurrence {
                EpisodeOccurrenceFilter::Any => true,
                EpisodeOccurrenceFilter::Unknown => {
                    matches!(facet.occurrence(), OccurrenceSpan::Unknown)
                }
                EpisodeOccurrenceFilter::Overlaps(window) => facet.occurrence().overlaps(window),
            }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeTimelineRequest {
    #[serde(default)]
    pub axis: EpisodeTimelineAxis,
    #[serde(default)]
    pub order: EpisodeOrder,
    pub window: Option<EpisodeTimeWindow>,
    #[serde(default)]
    pub filter: EpisodeFilter,
    #[serde(default)]
    pub limit: EpisodePageLimit,
    pub after: Option<EpisodeTimelineCursor>,
}
impl EpisodeTimelineRequest {
    pub fn validate(&self) -> Result<(), EpisodeValidationError> {
        self.filter.validate()?;
        if let Some(window) = &self.window {
            window.validate()?;
        }
        if self.axis == EpisodeTimelineAxis::Occurred
            && self.filter.occurrence == EpisodeOccurrenceFilter::Unknown
        {
            return Err(EpisodeValidationError(
                "unknown occurrence cannot use occurred ordering",
            ));
        }
        Ok(())
    }
    pub fn time(&self, facet: &EpisodeFacet) -> Option<EpisodeTime> {
        match self.axis {
            EpisodeTimelineAxis::Recorded => Some(facet.recorded_at()),
            EpisodeTimelineAxis::Occurred => facet.occurrence().start(),
        }
    }
    pub fn matches(&self, facet: &EpisodeFacet) -> bool {
        self.filter.matches(facet)
            && self.time(facet).is_some()
            && self.window.as_ref().is_none_or(|window| match self.axis {
                EpisodeTimelineAxis::Recorded => window.contains(facet.recorded_at()),
                EpisodeTimelineAxis::Occurred => facet.occurrence().overlaps(window),
            })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeCueRequest {
    pub cue: EpisodeCue,
    #[serde(default)]
    pub filter: EpisodeFilter,
    #[serde(default)]
    pub limit: EpisodePageLimit,
}
impl EpisodeCueRequest {
    pub fn validate(&self) -> Result<(), EpisodeValidationError> {
        self.filter.validate()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeHistoryRequest {
    pub episode_id: EpisodeId,
    #[serde(default)]
    pub limit: EpisodePageLimit,
    pub after: Option<EpisodeHistoryCursor>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeReferencesRequest {
    pub anchor: NodeId,
    #[serde(default)]
    pub limit: EpisodePageLimit,
    pub after: Option<EpisodeReferencesCursor>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EpisodePage<C> {
    pub items: Vec<EpisodeHeader>,
    pub next: Option<C>,
    pub partial: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeCueMode {
    Lexical,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EpisodeCuePage {
    pub mode: EpisodeCueMode,
    pub items: Vec<EpisodeHeader>,
    pub has_more: bool,
    pub partial: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EpisodeReference {
    pub edge: Edge,
    pub from_episode: Option<EpisodeIdentity>,
    pub to_episode: Option<EpisodeIdentity>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EpisodeReferencesPage {
    pub items: Vec<EpisodeReference>,
    pub next: Option<EpisodeReferencesCursor>,
}

// Fingerprint only the selection, not page size or continuation. Each piece is
// length framed, making strings containing punctuation unambiguous.
fn hash_parts(parts: &[String]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}
fn window_key(window: &Option<EpisodeTimeWindow>) -> String {
    // Freeze the historical e1t selection spelling without relying on Debug.
    fn endpoint(value: Option<EpisodeTime>) -> String {
        value.map_or_else(|| "None".into(), |time| format!("Some({})", time.get()))
    }
    window.as_ref().map_or_else(
        || "none".into(),
        |w| format!("{}/{}", endpoint(w.from), endpoint(w.through)),
    )
}
fn filter_parts(filter: &EpisodeFilter) -> Vec<String> {
    vec![
        filter
            .thread
            .as_ref()
            .map_or_else(|| "none".into(), |t| format!("some:{}", t.as_str())),
        match &filter.occurrence {
            EpisodeOccurrenceFilter::Any => "any".into(),
            EpisodeOccurrenceFilter::Unknown => "unknown".into(),
            EpisodeOccurrenceFilter::Overlaps(w) => {
                format!("overlaps:{}", window_key(&Some(w.clone())))
            }
        },
    ]
}
fn timeline_binding(request: &EpisodeTimelineRequest) -> String {
    let mut parts = vec![
        match request.axis {
            EpisodeTimelineAxis::Recorded => "recorded",
            EpisodeTimelineAxis::Occurred => "occurred",
        }
        .into(),
        match request.order {
            EpisodeOrder::NewestFirst => "newest",
            EpisodeOrder::OldestFirst => "oldest",
        }
        .into(),
        window_key(&request.window),
    ];
    parts.extend(filter_parts(&request.filter));
    hash_parts(&parts)
}
fn parse_ulid(value: &str) -> Result<Ulid, EpisodeValidationError> {
    let id =
        Ulid::from_string(value).map_err(|_| EpisodeValidationError("invalid cursor identity"))?;
    if id.to_string() != value {
        return Err(EpisodeValidationError("noncanonical cursor identity"));
    }
    Ok(id)
}
fn cursor_parts<'a>(value: &'a str, kind: &str) -> Result<Vec<&'a str>, EpisodeValidationError> {
    if value.len() > MAX_EPISODE_CURSOR_BYTES {
        return Err(EpisodeValidationError("cursor exceeds byte limit"));
    }
    let p: Vec<_> = value.split(':').collect();
    if p.len() != 5 || p[0] != kind {
        return Err(EpisodeValidationError("wrong cursor operation or encoding"));
    }
    parse_ulid(p[1])?;
    if p[2].len() != 64
        || !p[2]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(EpisodeValidationError("invalid cursor binding"));
    }
    Ok(p)
}
fn parse_number<T: FromStr + fmt::Display>(value: &str) -> Result<T, EpisodeValidationError> {
    let number = value
        .parse::<T>()
        .map_err(|_| EpisodeValidationError("invalid cursor key"))?;
    if number.to_string() != value {
        return Err(EpisodeValidationError("noncanonical cursor key"));
    }
    Ok(number)
}

macro_rules! cursor_serde {
    ($ty:ident) => {
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.wire().fmt(f)
            }
        }
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.wire())
            }
        }
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                String::deserialize(d)?
                    .parse()
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodeTimelineCursor {
    db: Ulid,
    binding: String,
    time: EpisodeTime,
    root: EpisodeId,
}
impl EpisodeTimelineCursor {
    /// Database binding carried by this cursor; adapters still compare their own ID.
    pub fn database_id(&self) -> Ulid {
        self.db
    }
    pub fn new(
        db: Ulid,
        request: &EpisodeTimelineRequest,
        time: EpisodeTime,
        root: EpisodeId,
    ) -> Self {
        Self {
            db,
            binding: timeline_binding(request),
            time,
            root,
        }
    }
    pub fn validate(
        &self,
        db: Ulid,
        request: &EpisodeTimelineRequest,
    ) -> Result<(), EpisodeValidationError> {
        request.validate()?;
        if self.db != db || self.binding != timeline_binding(request) {
            return Err(EpisodeValidationError(
                "timeline cursor belongs to another database or selection",
            ));
        }
        Ok(())
    }
    pub fn key(&self) -> (EpisodeTime, EpisodeId) {
        (self.time, self.root)
    }
    fn wire(&self) -> String {
        format!(
            "e1t:{}:{}:{}:{}",
            self.db,
            self.binding,
            self.time.get(),
            self.root
        )
    }
}
impl FromStr for EpisodeTimelineCursor {
    type Err = EpisodeValidationError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let p = cursor_parts(value, "e1t")?;
        Ok(Self {
            db: parse_ulid(p[1])?,
            binding: p[2].into(),
            time: EpisodeTime::new(parse_number(p[3])?)?,
            root: EpisodeId::new(NodeId(parse_ulid(p[4])?)),
        })
    }
}
cursor_serde!(EpisodeTimelineCursor);
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodeHistoryCursor {
    db: Ulid,
    binding: String,
    revision: EpisodeRevision,
    edition: NodeId,
}
impl EpisodeHistoryCursor {
    /// Database binding carried by this cursor; adapters still compare their own ID.
    pub fn database_id(&self) -> Ulid {
        self.db
    }
    pub fn new(
        db: Ulid,
        request: &EpisodeHistoryRequest,
        revision: EpisodeRevision,
        edition: NodeId,
    ) -> Self {
        Self {
            db,
            binding: hash_parts(&[request.episode_id.to_string()]),
            revision,
            edition,
        }
    }
    pub fn validate(
        &self,
        db: Ulid,
        request: &EpisodeHistoryRequest,
    ) -> Result<(), EpisodeValidationError> {
        if self.db != db || self.binding != hash_parts(&[request.episode_id.to_string()]) {
            return Err(EpisodeValidationError(
                "history cursor belongs to another database or episode",
            ));
        }
        Ok(())
    }
    pub fn key(&self) -> (EpisodeRevision, NodeId) {
        (self.revision, self.edition)
    }
    fn wire(&self) -> String {
        format!(
            "e1h:{}:{}:{}:{}",
            self.db,
            self.binding,
            self.revision.get(),
            self.edition.0
        )
    }
}
impl FromStr for EpisodeHistoryCursor {
    type Err = EpisodeValidationError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let p = cursor_parts(value, "e1h")?;
        Ok(Self {
            db: parse_ulid(p[1])?,
            binding: p[2].into(),
            revision: EpisodeRevision::new(parse_number(p[3])?),
            edition: NodeId(parse_ulid(p[4])?),
        })
    }
}
cursor_serde!(EpisodeHistoryCursor);
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodeReferencesCursor {
    db: Ulid,
    binding: String,
    from: NodeId,
    to: NodeId,
}
impl EpisodeReferencesCursor {
    /// Database binding carried by this cursor; adapters still compare their own ID.
    pub fn database_id(&self) -> Ulid {
        self.db
    }
    pub fn new(db: Ulid, request: &EpisodeReferencesRequest, from: NodeId, to: NodeId) -> Self {
        Self {
            db,
            binding: hash_parts(&[request.anchor.0.to_string()]),
            from,
            to,
        }
    }
    pub fn validate(
        &self,
        db: Ulid,
        request: &EpisodeReferencesRequest,
    ) -> Result<(), EpisodeValidationError> {
        if self.db != db || self.binding != hash_parts(&[request.anchor.0.to_string()]) {
            return Err(EpisodeValidationError(
                "reference cursor belongs to another database or anchor",
            ));
        }
        if self.from != request.anchor && self.to != request.anchor {
            return Err(EpisodeValidationError(
                "reference cursor key is not incident to anchor",
            ));
        }
        Ok(())
    }
    pub fn key(&self) -> (NodeId, NodeId) {
        (self.from, self.to)
    }
    fn wire(&self) -> String {
        format!(
            "e1r:{}:{}:{}:{}",
            self.db, self.binding, self.from.0, self.to.0
        )
    }
}
impl FromStr for EpisodeReferencesCursor {
    type Err = EpisodeValidationError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let p = cursor_parts(value, "e1r")?;
        Ok(Self {
            db: parse_ulid(p[1])?,
            binding: p[2].into(),
            from: NodeId(parse_ulid(p[3])?),
            to: NodeId(parse_ulid(p[4])?),
        })
    }
}
cursor_serde!(EpisodeReferencesCursor);

/// Bounded exact-edition lookup; the adapter derives the root from that node
/// and observes its current head in the same coherent read, without body reads.
#[derive(Clone, Debug)]
pub struct EpisodeHeaderByEditionRequest {
    edition_id: NodeId,
    remaining: std::time::Duration,
}
impl EpisodeHeaderByEditionRequest {
    pub fn new(edition_id: NodeId, remaining: std::time::Duration) -> crate::ports::Result<Self> {
        let request = Self {
            edition_id,
            remaining,
        };
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> crate::ports::Result<()> {
        crate::ports::validate_linked_read_timeout(self.remaining)
    }
    pub const fn edition_id(&self) -> NodeId {
        self.edition_id
    }
    pub const fn remaining(&self) -> std::time::Duration {
        self.remaining
    }
}
#[derive(Clone, Debug)]
pub enum EpisodeHeaderByEdition {
    Missing,
    Semantic,
    Episode(EpisodeHeader),
}

/// Separate episode lane. Capability does not imply generation admission or
/// authorization. Exact source replay precedes current-head CAS in each adapter.
#[async_trait]
pub trait EpisodeStore: Send + Sync {
    /// Optional exact-edition header read. Missing and semantic endpoints are
    /// successful charged classifications, not unsupported capability. Preserve
    /// existing adapter/store unavailable reasons; never substitute current head
    /// for the cited edition. Bound lock + node + head reads by remaining time.
    async fn episode_header_by_edition(
        &self,
        request: &EpisodeHeaderByEditionRequest,
    ) -> crate::ports::Result<EpisodeHeaderByEdition> {
        request.validate()?;
        Err(crate::ports::Error::EpisodeUnavailable(
            EpisodeUnavailableReason::AdapterUnsupported,
        ))
    }
    async fn lookup_episode(
        &self,
        source: &CaptureSource,
    ) -> crate::ports::Result<Option<EpisodeRecord>>;
    async fn commit_episode(
        &self,
        request: EpisodeCommit<'_>,
    ) -> crate::ports::Result<EpisodeCommitOutcome>;
    async fn get_episode(
        &self,
        request: &EpisodeGet,
    ) -> crate::ports::Result<Option<EpisodeRecord>>;
    async fn episode_timeline(
        &self,
        request: &EpisodeTimelineRequest,
    ) -> crate::ports::Result<EpisodePage<EpisodeTimelineCursor>>;
    async fn episode_cue(
        &self,
        request: &EpisodeCueRequest,
    ) -> crate::ports::Result<EpisodeCuePage>;
    async fn episode_history(
        &self,
        request: &EpisodeHistoryRequest,
    ) -> crate::ports::Result<EpisodePage<EpisodeHistoryCursor>>;
    async fn episode_references(
        &self,
        request: &EpisodeReferencesRequest,
    ) -> crate::ports::Result<EpisodeReferencesPage>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BodyRef, CaptureRequestCodec, NodeStatus, Provenance};
    use serde_json::json;

    fn time(ms: u128) -> EpisodeTime {
        EpisodeTime::new(ms).unwrap()
    }
    fn source(key: &str) -> CaptureSource {
        CaptureSource::new("episode-test", key, "test://episode", None, None, [7; 32]).unwrap()
    }
    fn base(key: &str, now: u128) -> Node {
        let source = source(key);
        Node::try_new(
            source.node_id(),
            "A bounded account",
            BodyRef::new("inline://story").unwrap(),
            ["journal"],
            Provenance::External { source },
            0.5,
            1.0,
            NodeStatus::Active,
            now,
        )
        .unwrap()
    }
    fn episode_base(key: &str, now: u128) -> Node {
        let source = CaptureSource::new_with_codec(
            "episode-test",
            key,
            "test://episode",
            None,
            None,
            [7; 32],
            CaptureRequestCodec::EpisodeV1,
        )
        .unwrap();
        Node::try_new(
            source.node_id(),
            "A bounded account",
            BodyRef::new("inline://story").unwrap(),
            ["journal"],
            Provenance::External { source },
            0.5,
            1.0,
            NodeStatus::Active,
            now,
        )
        .unwrap()
    }
    fn initial(key: &str, now: u128) -> Node {
        let node = episode_base(key, now);
        let facet =
            EpisodeFacet::initial(node.id(), OccurrenceSpan::Unknown, None, time(now)).unwrap();
        node.with_episode(facet).unwrap()
    }

    #[test]
    fn episode_recording_session_is_bounded_exact_edition_provenance_not_context() {
        for invalid in ["", " ", " padded", "padded ", "line\nbreak", "a\0b"] {
            assert!(EpisodeRecordingSession::new(invalid).is_err());
            assert!(serde_json::from_value::<EpisodeRecordingSession>(json!(invalid)).is_err());
        }
        assert!(EpisodeRecordingSession::new("x".repeat(MAX_CAPTURE_SESSION_BYTES)).is_ok());
        assert!(EpisodeRecordingSession::new("x".repeat(MAX_CAPTURE_SESSION_BYTES + 1)).is_err());
        let initial = initial("recording-root", 3);
        let unknown = EpisodeHeader::from_node(&initial, initial.id()).unwrap();
        assert_eq!(unknown.recording_session, None);
        let mut wire = serde_json::to_value(&initial).unwrap();
        wire["provenance"]["External"]["source"]["session"] = json!("root-recorder");
        let initial: Node = serde_json::from_value(wire).unwrap();
        let old = initial.episode().unwrap();
        let facet = EpisodeFacet::revised(
            old.root(),
            initial.id(),
            old.revision().next().unwrap(),
            OccurrenceSpan::Unknown,
            Some(EpisodeThread::new("not-the-session").unwrap()),
            old.recorded_at(),
            EpisodeRevisionReason::new("A separate recorder made this correction").unwrap(),
        )
        .unwrap();
        let revision = episode_base("recording-revision", 5)
            .with_episode(facet)
            .unwrap();
        let mut wire = serde_json::to_value(&revision).unwrap();
        wire["provenance"]["External"]["source"]["session"] = json!("revision-recorder");
        let revision: Node = serde_json::from_value(wire).unwrap();
        let original_header = EpisodeHeader::from_node(&initial, revision.id()).unwrap();
        let revision_header = EpisodeHeader::from_node(&revision, revision.id()).unwrap();
        assert_eq!(
            original_header
                .recording_session
                .as_ref()
                .map(EpisodeRecordingSession::as_str),
            Some("root-recorder")
        );
        assert_eq!(
            revision_header
                .recording_session
                .as_ref()
                .map(EpisodeRecordingSession::as_str),
            Some("revision-recorder")
        );
        assert_eq!(revision_header.occurrence_contexts, None);
        let anchored = revision
            .clone()
            .with_origin_commit(Some(crate::OriginCommit::Sha1([3; 20])));
        assert_eq!(
            EpisodeHeader::from_node(&anchored, revision.id())
                .unwrap()
                .recording_session,
            revision_header.recording_session,
            "host VCS provenance does not infer the recording session"
        );
        let mut header_wire = serde_json::to_value(&revision_header).unwrap();
        assert_eq!(header_wire["recording_session"], "revision-recorder");
        assert!(header_wire.get("source").is_none());
        assert!(header_wire.get("request_digest").is_none());
        header_wire["recording_session"] = json!(null);
        assert!(serde_json::from_value::<EpisodeHeader>(header_wire).is_err());
        let mut omitted = serde_json::to_value(&revision).unwrap();
        omitted["provenance"]["External"]["source"]
            .as_object_mut()
            .unwrap()
            .remove("session");
        let omitted: Node = serde_json::from_value(omitted).unwrap();
        assert_eq!(
            EpisodeHeader::from_node(&omitted, omitted.id())
                .unwrap()
                .recording_session,
            None,
            "an edition with absent session never inherits its root's recorder"
        );
    }

    #[test]
    fn bounded_fields_and_serde_share_invariants() {
        for value in ["", " ", " padded", "padded ", "line\nbreak", "a\0b"] {
            assert!(EpisodeThread::new(value).is_err());
            assert!(EpisodeCue::new(value).is_err());
            assert!(EpisodeRevisionReason::new(value).is_err());
            assert!(serde_json::from_value::<EpisodeCue>(json!(value)).is_err());
        }
        assert!(EpisodeThread::new("é".repeat(64)).is_ok());
        assert!(EpisodeThread::new("é".repeat(65)).is_err());
        assert!(EpisodeRevisionReason::new("x".repeat(1024)).is_ok());
        assert!(EpisodeRevisionReason::new("x".repeat(1025)).is_err());
        for n in [0, 33, usize::MAX] {
            assert!(EpisodePageLimit::new(n).is_err());
        }
        assert_eq!(EpisodePageLimit::default().get(), 8);
        assert!(serde_json::from_value::<EpisodePageLimit>(json!(33)).is_err());
        assert_eq!(time(i64::MAX as u128).get(), i64::MAX as u128);
        assert!(EpisodeTime::new(i64::MAX as u128 + 1).is_err());
        assert!(serde_json::from_str::<EpisodeTime>("9223372036854775808").is_err());
        assert!(EpisodeRevision::new(u32::MAX).next().is_err());
    }

    #[test]
    fn occurrence_contexts_validate_canonicalize_and_measure_exact_json() {
        for invalid in [
            "",
            " ",
            " padded",
            "padded ",
            "line\nbreak",
            "a\0b",
            "a\u{0085}b",
        ] {
            assert!(OccurrenceContextRef::new(invalid, "key", None).is_err());
            assert!(OccurrenceContextRef::new("Namespace", invalid, None).is_err());
            assert!(OccurrenceContextRef::new("Namespace", "key", Some(invalid)).is_err());
            for wire in [
                json!({"namespace":invalid,"key":"key"}),
                json!({"namespace":"Namespace","key":invalid}),
                json!({"namespace":"Namespace","key":"key","label":invalid}),
            ] {
                assert!(serde_json::from_value::<OccurrenceContextRef>(wire).is_err());
            }
        }
        for wire in [
            json!([{ "namespace": "n", "key": "k", "label": null }]),
            json!([]),
            json!({"namespace":"n","key":"k"}),
            json!([{"namespace":"n","key":"k","surprise":true}]),
            json!([{"namespace":"n","key":"k"},{"namespace":"n","key":"k","label":"different"}]),
        ] {
            assert!(serde_json::from_value::<OccurrenceContexts>(wire).is_err());
        }
        let input = json!([
            {"namespace":"z","key":"a","label":"scene"},
            {"namespace":"World/opaque","key":"Case"},
            {"namespace":"World/opaque","key":"case"},
            {"namespace":"World/opaque","key":"A"},
        ]);
        let contexts: OccurrenceContexts = serde_json::from_value(input).unwrap();
        assert_eq!(
            contexts
                .iter()
                .map(|c| (c.namespace(), c.key()))
                .collect::<Vec<_>>(),
            vec![
                ("World/opaque", "A"),
                ("World/opaque", "Case"),
                ("World/opaque", "case"),
                ("z", "a")
            ]
        );
        let mut reordered = contexts.as_slice().to_vec();
        reordered.reverse();
        assert_eq!(OccurrenceContexts::new(reordered).unwrap(), contexts);
        for text in ["plain", "é", "🦆", "quote\"slash\\", "a/b", "a\u{2028}b"] {
            for label in [None, Some(text)] {
                let contexts = OccurrenceContexts::new(vec![
                    OccurrenceContextRef::new(text, text, label).unwrap(),
                    OccurrenceContextRef::new("second", "item", None).unwrap(),
                ])
                .unwrap();
                assert_eq!(
                    contexts.compact_json_len(),
                    serde_json::to_vec(&contexts).unwrap().len()
                );
            }
        }
        // The full JSON budget, including punctuation and escaping, is exact.
        let overhead =
            OccurrenceContexts::new(vec![OccurrenceContextRef::new("n", "x", None).unwrap()])
                .unwrap()
                .compact_json_len()
                - 1;
        let key = "x".repeat(MAX_OCCURRENCE_CONTEXTS_JSON_BYTES - overhead);
        let exact =
            OccurrenceContexts::new(vec![OccurrenceContextRef::new("n", &key, None).unwrap()])
                .unwrap();
        assert_eq!(exact.compact_json_len(), MAX_OCCURRENCE_CONTEXTS_JSON_BYTES);
        assert_eq!(
            serde_json::to_vec(&exact).unwrap().len(),
            MAX_OCCURRENCE_CONTEXTS_JSON_BYTES
        );
        let too_long = json!([{"namespace":"n","key":format!("{key}x")}]);
        assert!(serde_json::from_value::<OccurrenceContexts>(too_long).is_err());
        let escaped = "\\".repeat((MAX_OCCURRENCE_CONTEXTS_JSON_BYTES - overhead) / 2 + 1);
        assert!(
            OccurrenceContexts::new(vec![OccurrenceContextRef::new("n", escaped, None).unwrap()])
                .is_err()
        );
        let many = OccurrenceContexts::new(
            (0..20)
                .map(|n| OccurrenceContextRef::new("n", n.to_string(), None).unwrap())
                .collect(),
        )
        .unwrap();
        assert_eq!(many.as_slice().len(), 20, "no unrelated count cap");
        assert_eq!(
            many.compact_json_len(),
            serde_json::to_vec(&many).unwrap().len()
        );
    }

    #[test]
    fn context_presence_requires_matching_codec_and_preserves_old_facet_wire() {
        let node = initial("context-codec", 3);
        let facet = node.episode().unwrap();
        let expected = format!(
            "{{\"episode_id\":\"{}\",\"revision\":0,\"revises\":null,\"occurred\":{{\"kind\":\"unknown\"}},\"thread\":null,\"recorded_at\":3,\"edit_reason\":null}}",
            node.id().0
        );
        assert_eq!(serde_json::to_string(facet).unwrap(), expected);
        let mut null_facet = serde_json::to_value(facet).unwrap();
        null_facet["occurrence_contexts"] = json!(null);
        assert!(serde_json::from_value::<EpisodeFacet>(null_facet).is_err());
        let mut null_node = serde_json::to_value(&node).unwrap();
        null_node["memory_kind"]["episode"]["occurrence_contexts"] = json!(null);
        assert!(
            serde_json::from_value::<Node>(null_node).is_err(),
            "v1 null field is present, not unknown"
        );
        let header = EpisodeHeader::from_node(&node, node.id()).unwrap();
        let mut header_wire = serde_json::to_value(header).unwrap();
        assert!(header_wire.get("occurrence_contexts").is_none());
        assert!(serde_json::from_value::<EpisodeHeader>(header_wire.clone()).is_ok());
        header_wire["occurrence_contexts"] = json!(null);
        assert!(serde_json::from_value::<EpisodeHeader>(header_wire).is_err());
        let contexts = json!([{"namespace":"world","key":"canal","label":"Gate scene"}]);
        let mut wire = serde_json::to_value(&node).unwrap();
        wire["memory_kind"]["episode"]["occurrence_contexts"] = contexts.clone();
        assert!(
            serde_json::from_value::<Node>(wire.clone()).is_err(),
            "v1 cannot carry contexts"
        );
        wire["provenance"]["External"]["source"]["request_codec"] = json!("episode_v2");
        let decoded: Node = serde_json::from_value(wire.clone()).unwrap();
        let header = EpisodeHeader::from_node(&decoded, decoded.id()).unwrap();
        assert_eq!(
            serde_json::to_value(header.occurrence_contexts).unwrap(),
            contexts
        );
        wire["memory_kind"]["episode"]
            .as_object_mut()
            .unwrap()
            .remove("occurrence_contexts");
        assert!(
            serde_json::from_value::<Node>(wire).is_err(),
            "v2 requires contexts"
        );
        for codec in ["episode_v1", "episode_v2"] {
            let mut semantic = serde_json::to_value(base("semantic-codec", 3)).unwrap();
            semantic["provenance"]["External"]["source"]["request_codec"] = json!(codec);
            assert!(
                serde_json::from_value::<Node>(semantic).is_err(),
                "{codec} is not semantic"
            );
        }
    }

    #[test]
    fn occurrence_windows_use_inclusive_full_span_overlap() {
        let range = OccurrenceSpan::range(time(10), time(20)).unwrap();
        let window = EpisodeTimeWindow::new(Some(time(20)), Some(time(30))).unwrap();
        assert!(range.overlaps(&window));
        assert!(OccurrenceSpan::point(time(20)).overlaps(&window));
        assert!(!OccurrenceSpan::Unknown.overlaps(&window));
        assert!(!OccurrenceSpan::point(time(19)).overlaps(&window));
        assert!(OccurrenceSpan::range(time(20), time(20)).is_err());
        assert!(
            serde_json::from_value::<OccurrenceSpan>(json!({"kind":"range","start":20,"end":10}))
                .is_err()
        );
        assert!(EpisodeTimeWindow::new(None, None).is_err());
        assert!(EpisodeTimeWindow::new(Some(time(2)), Some(time(1))).is_err());
        assert!(
            serde_json::from_value::<EpisodeTimeWindow>(json!({"from":null,"through":null}))
                .is_err()
        );
        let facet = EpisodeFacet::initial(base("root", 1).id(), range, None, time(1)).unwrap();
        let request = EpisodeTimelineRequest {
            axis: EpisodeTimelineAxis::Occurred,
            window: Some(window),
            ..Default::default()
        };
        assert_eq!(request.time(&facet), Some(time(10)));
        assert!(
            request.matches(&facet),
            "an interval starting before the window still overlaps"
        );
        let invalid = EpisodeTimelineRequest {
            filter: EpisodeFilter {
                thread: None,
                occurrence: EpisodeOccurrenceFilter::Unknown,
            },
            ..request
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn legacy_semantic_wire_is_unchanged_and_episode_wire_is_typed() {
        let semantic = base("semantic", 1);
        let semantic_wire = serde_json::to_value(&semantic).unwrap();
        assert!(semantic_wire.get("memory_kind").is_none());
        let decoded: Node = serde_json::from_value(semantic_wire).unwrap();
        assert!(decoded.is_semantic());
        assert!(decoded.episode().is_none());
        let episode = initial("first", 3);
        let wire = serde_json::to_value(&episode).unwrap();
        assert_eq!(wire["memory_kind"]["kind"], "episode");
        let decoded: Node = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded.episode(), episode.episode());
        assert!(!decoded.is_semantic());
        let header = EpisodeHeader::from_node(&decoded, decoded.id()).unwrap();
        assert_eq!(header.identity.episode_id.node_id(), decoded.id());
        assert_eq!(header.recorded_at, time(3));
    }

    #[test]
    fn deserialization_enforces_enclosing_node_invariants() {
        let node = initial("first", 3);
        for field in ["created", "id"] {
            let mut wire = serde_json::to_value(&node).unwrap();
            wire[field] = if field == "created" {
                json!(4)
            } else {
                json!(Ulid::from(99).to_string())
            };
            assert!(serde_json::from_value::<Node>(wire).is_err(), "{field}");
        }
        let mut wire = serde_json::to_value(&node).unwrap();
        wire["tags"] = json!(["core"]);
        assert!(serde_json::from_value::<Node>(wire).is_err());
        let mut wire = serde_json::to_value(&node).unwrap();
        wire["summary"] = json!("a".repeat(2049));
        assert!(serde_json::from_value::<Node>(wire).is_err());
        let mut wire = serde_json::to_value(&node).unwrap();
        wire["memory_kind"]["episode"]["revision"] = json!(1);
        assert!(serde_json::from_value::<Node>(wire).is_err());
        // Match root/time locally but mismatch its authored source identity.
        let mut wire = serde_json::to_value(&node).unwrap();
        let foreign = Ulid::from(99).to_string();
        wire["id"] = json!(foreign);
        wire["memory_kind"]["episode"]["episode_id"] = json!(foreign);
        assert!(serde_json::from_value::<Node>(wire).is_err());
    }

    #[test]
    fn editions_keep_original_recording_time_even_after_clock_rollback() {
        let root = initial("root", 50);
        let old = root.episode().unwrap();
        let facet = EpisodeFacet::revised(
            old.root(),
            root.id(),
            old.revision().next().unwrap(),
            OccurrenceSpan::point(time(20)),
            Some(EpisodeThread::new("thread").unwrap()),
            old.recorded_at(),
            EpisodeRevisionReason::new("Correct a date").unwrap(),
        )
        .unwrap();
        facet.validate_successor(&root).unwrap();
        let revised = episode_base("revision", 40)
            .with_episode(facet.clone())
            .unwrap();
        let header = EpisodeHeader::from_node(&revised, revised.id()).unwrap();
        assert_eq!(header.recorded_at, time(50));
        assert_eq!(header.edition_recorded_at, time(40));
        assert_eq!(header.identity.revision.get(), 1);
        assert_eq!(root.episode().unwrap().edit_reason(), None);
        assert!(revised.clone().with_episode(facet).is_err());
        assert!(serde_json::from_value::<Node>(serde_json::to_value(&revised).unwrap()).is_ok());
        let unrelated = initial("other", 50);
        assert!(
            revised
                .episode()
                .unwrap()
                .validate_successor(&unrelated)
                .is_err()
        );
        let wrong = EpisodeFacet::revised(
            old.root(),
            root.id(),
            EpisodeRevision::new(3),
            OccurrenceSpan::Unknown,
            None,
            old.recorded_at(),
            EpisodeRevisionReason::new("No silent ordinal gap").unwrap(),
        )
        .unwrap();
        assert!(wrong.validate_successor(&root).is_err());
    }

    #[test]
    fn timeline_window_selection_spelling_is_frozen_without_debug() {
        assert_eq!(window_key(&None), "none");
        for (from, through, expected) in [
            (None, Some(time(0)), "None/Some(0)"),
            (Some(time(0)), None, "Some(0)/None"),
            (
                Some(time(0)),
                Some(time(i64::MAX as u128)),
                "Some(0)/Some(9223372036854775807)",
            ),
            (
                Some(time(i64::MAX as u128)),
                None,
                "Some(9223372036854775807)/None",
            ),
            (
                None,
                Some(time(i64::MAX as u128)),
                "None/Some(9223372036854775807)",
            ),
        ] {
            assert_eq!(
                window_key(&Some(EpisodeTimeWindow::new(from, through).unwrap())),
                expected
            );
        }
    }

    #[test]
    fn timeline_cursor_selection_keeps_pre_context_golden_bytes() {
        for (window, expected) in [
            (
                None,
                "e1t:00000000000000000000000001:b9e4e66ae5891dc904bad2c6b684ea844223f82254e5567627db82ccc160a45d:12:00000000000000000000000002",
            ),
            (
                Some(EpisodeTimeWindow::new(None, Some(time(0))).unwrap()),
                "e1t:00000000000000000000000001:8c91b567f423ee2dc699ee546cd7b0eb392220c39c26990cef9d3f9409f1564e:12:00000000000000000000000002",
            ),
            (
                Some(EpisodeTimeWindow::new(Some(time(0)), Some(time(i64::MAX as u128))).unwrap()),
                "e1t:00000000000000000000000001:d7412b55637c64b1e7ae45a8bad54209b4b361d2f5129415863be9a994d0726b:12:00000000000000000000000002",
            ),
        ] {
            let request = EpisodeTimelineRequest {
                window,
                ..Default::default()
            };
            let cursor = EpisodeTimelineCursor::new(
                Ulid(1),
                &request,
                time(12),
                EpisodeId::new(NodeId(Ulid(2))),
            );
            assert_eq!(cursor.to_string(), expected);
            assert!(
                expected
                    .parse::<EpisodeTimelineCursor>()
                    .unwrap()
                    .validate(Ulid(1), &request)
                    .is_ok()
            );
        }
    }

    #[test]
    fn timeline_cursor_roundtrips_binds_selection_not_page_size() {
        let db = Ulid::from(1);
        let request = EpisodeTimelineRequest::default();
        let cursor = EpisodeTimelineCursor::new(
            db,
            &request,
            time(12),
            EpisodeId::new(NodeId(Ulid::from(2))),
        );
        let parsed: EpisodeTimelineCursor = cursor.to_string().parse().unwrap();
        assert_eq!(parsed, cursor);
        assert!(parsed.validate(db, &request).is_ok());
        let mut changed = request.clone();
        changed.limit = EpisodePageLimit::new(32).unwrap();
        changed.after = Some(cursor.clone());
        assert!(parsed.validate(db, &changed).is_ok());
        assert!(parsed.validate(Ulid::from(3), &request).is_err());
        changed.order = EpisodeOrder::OldestFirst;
        assert!(parsed.validate(db, &changed).is_err());
        changed = request.clone();
        changed.axis = EpisodeTimelineAxis::Occurred;
        assert!(parsed.validate(db, &changed).is_err());
        changed = request.clone();
        changed.filter.thread = Some(EpisodeThread::new("topic").unwrap());
        assert!(parsed.validate(db, &changed).is_err());
        changed = request.clone();
        changed.filter.occurrence = EpisodeOccurrenceFilter::Unknown;
        assert!(parsed.validate(db, &changed).is_err());
        changed = request.clone();
        changed.window = Some(EpisodeTimeWindow::new(Some(time(1)), None).unwrap());
        assert!(parsed.validate(db, &changed).is_err());
        assert!(cursor.to_string().parse::<EpisodeHistoryCursor>().is_err());
        assert!(
            cursor
                .to_string()
                .replace(":12:", ":012:")
                .parse::<EpisodeTimelineCursor>()
                .is_err()
        );
        assert!("x".repeat(1025).parse::<EpisodeTimelineCursor>().is_err());
    }

    #[test]
    fn history_and_reference_cursors_bind_identity_and_operation() {
        let db = Ulid::from(1);
        let root = EpisodeId::new(NodeId(Ulid::from(2)));
        let edition = NodeId(Ulid::from(3));
        let request = EpisodeHistoryRequest {
            episode_id: root,
            limit: EpisodePageLimit::default(),
            after: None,
        };
        let cursor = EpisodeHistoryCursor::new(db, &request, EpisodeRevision::new(1), edition);
        let parsed: EpisodeHistoryCursor =
            serde_json::from_str(&serde_json::to_string(&cursor).unwrap()).unwrap();
        assert_eq!(parsed.key(), (EpisodeRevision::new(1), edition));
        parsed.validate(db, &request).unwrap();
        assert!(parsed.validate(Ulid::from(7), &request).is_err());
        let other = EpisodeHistoryRequest {
            episode_id: EpisodeId::new(edition),
            ..request
        };
        assert!(parsed.validate(db, &other).is_err());
        let request = EpisodeReferencesRequest {
            anchor: edition,
            limit: EpisodePageLimit::default(),
            after: None,
        };
        let cursor = EpisodeReferencesCursor::new(db, &request, root.node_id(), edition);
        let parsed: EpisodeReferencesCursor = cursor.to_string().parse().unwrap();
        assert_eq!(parsed.key(), (root.node_id(), edition));
        parsed.validate(db, &request).unwrap();
        assert!(parsed.validate(Ulid::from(7), &request).is_err());
        assert!(
            parsed
                .validate(
                    db,
                    &EpisodeReferencesRequest {
                        anchor: root.node_id(),
                        ..request.clone()
                    }
                )
                .is_err()
        );
        let nonincident =
            EpisodeReferencesCursor::new(db, &request, root.node_id(), root.node_id());
        assert!(nonincident.validate(db, &request).is_err());
    }
}
