//! mneme-core — the domain. Types that describe *what* a memory graph is, plus
//! the [`ports`] (traits) describing *what can be done to it*.
//!
//! The load-bearing rule of the whole workspace lives here: this crate has no
//! I/O dependencies and never names a concrete backend. Storage, embeddings,
//! body resolution and traversal are all [`ports`]; adapter crates implement
//! them and the engine composes them. If anything in here ever learns about
//! cozo, fastembed, or a filesystem, the abstraction has leaked.
//!
//! Encapsulation here is deliberate: canonical node fields and counters that
//! carry invariants are private and mutated through checked methods, so neither
//! constructors, deserialization, nor maintenance callers can skip them. Policy
//! still lives in the engine; the domain owns only representational validity.

pub mod concern;
pub use concern::*;
pub mod episode;
pub use episode::*;
pub mod managed;
pub mod ports;
pub mod retag;
pub mod touchstone;
pub use touchstone::*;
pub mod tag_vocabulary;
pub mod tagged;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use ulid::Ulid;
use unordered_pair::UnorderedPair;

/// Hard storage invariant for the number of unique directed edge records that
/// may touch one node. A self-loop is one record, not two. Keeping this bound in
/// the domain crate makes every backend enforce the same worst-case adjacency
/// cost instead of relying on a retrieval-time `top_k` after an unbounded read.
pub const MAX_INCIDENT_EDGES: usize = 1_024;

/// Maximum number of node identities one hot-path hydration call may submit to
/// a graph adapter. Frontends currently admit at most 256 retrieved nodes; the
/// larger domain limit leaves room for graph expansion while still bounding
/// an adapter's inline query, parameter map, and returned JSON blobs.
/// Callers with a larger library-level result budget must page explicitly.
pub const MAX_NODE_HYDRATION_BATCH: usize = 1_024;

/// Maximum number of node identities one hot-path lifecycle-status lookup may
/// submit to a graph adapter. One incident set is the largest legitimate
/// caller: resolving a scoped adjacency first checks every bounded endpoint,
/// then hydrates only the much smaller caller-visible selection.
pub const MAX_NODE_STATUS_BATCH: usize = MAX_INCIDENT_EDGES;

/// Hard storage invariant for the number of unique cross-database edge records
/// owned by one local source node. Remote-edge identity is
/// `(from, target_db, target)`: updating that exact key consumes no new slot,
/// while arbitrarily many distinct local sources may point at the same remote
/// target. Keeping this source-owned bound in the domain makes every backend
/// enforce the same worst-case overlay cost.
pub const MAX_REMOTE_EDGES_PER_SOURCE: usize = 256;

/// Maximum number of remote edges returned by one hot-path read. This is
/// intentionally smaller than [`MAX_REMOTE_EDGES_PER_SOURCE`]: storage bounds
/// total source degree, while this bounds one request's allocation and the
/// number of remote databases a host might subsequently hydrate.
pub const MAX_REMOTE_EDGE_PAGE_SIZE: usize = 64;

/// Maximum number of unique local edge rows a full merge may inspect. Each
/// endpoint is independently bounded by [`MAX_INCIDENT_EDGES`], so their union
/// cannot exceed twice that value. The adapter must use its endpoint indexes;
/// this bound is not permission to scan the whole edge relation.
pub const MAX_FULL_MERGE_INCIDENT_EDGES: usize = MAX_INCIDENT_EDGES * 2;

/// Maximum number of source-owned remote edge rows a full merge may inspect.
/// Winner and loser each own at most [`MAX_REMOTE_EDGES_PER_SOURCE`].
pub const MAX_FULL_MERGE_REMOTE_EDGES: usize = MAX_REMOTE_EDGES_PER_SOURCE * 2;

/// Maximum number of path-edge observations one receipt-backed feedback
/// transaction may apply. The MCP surface currently admits at most eight walk
/// receipts of 63 reached edges each (504 total); 512 leaves a small protocol
/// margin without allowing an unbounded mutation script.
pub const MAX_FEEDBACK_BATCH_EVENTS: usize = 512;

/// Maximum size of the server-derived idempotency key attached to one feedback
/// transaction. Keys are capability identifiers (normally a canonical receipt
/// set), never caller prose.
pub const MAX_FEEDBACK_BATCH_KEY_BYTES: usize = 1_024;

/// A receipt-feedback payload is represented durably by one lowercase SHA-256
/// digest rather than by the full bounded event stream.
pub const MAX_FEEDBACK_FINGERPRINT_BYTES: usize = 64;

/// Bound the opaque server-generation identifier carried by retry metadata.
pub const MAX_FEEDBACK_EPOCH_BYTES: usize = 128;

/// Maximum reachable durable retry records per database. Unreachable prefixes
/// are reclaimed using the host's epoch/sequence watermark before this bound is
/// checked; a full reachable ledger rejects rather than discarding a proof.
pub const MAX_FEEDBACK_RETRY_RECORDS: usize = 1_024;

/// Maximum number of tags stored on one canonical node.
///
/// This is a storage invariant rather than an ingest-surface preference. Every
/// constructor, snapshot decoder, and adapter write/import boundary must reject
/// a larger set before doing work proportional to it.
pub const MAX_NODE_TAGS: usize = 64;

/// Maximum UTF-8 byte length of one canonical node tag.
pub const MAX_TAG_BYTES: usize = 256;

/// Why a canonical tag or tag set was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TagValidationError {
    #[error("tag must not be empty")]
    Empty,
    #[error("tag must not have leading or trailing whitespace")]
    Untrimmed,
    #[error("tag must not contain control characters")]
    ControlCharacter,
    #[error("tag is {actual} UTF-8 bytes; maximum is {MAX_TAG_BYTES}")]
    TooLong { actual: usize },
    #[error("duplicate tag {tag:?}")]
    Duplicate { tag: String },
    #[error("tag set has more than {MAX_NODE_TAGS} entries")]
    TooMany,
    #[error("tag set is not in canonical lexical order")]
    NonCanonicalOrder,
}

/// Validate the shared lexical invariant for a stored or queried tag.
///
/// Query tags deliberately use the same byte ceiling as canonical tags: a tag
/// accepted into storage must remain queryable. Callers that accept a complete
/// node tag collection should use [`BoundedTagSet::try_from_iter`] as well so
/// duplicate and cardinality validation cannot be skipped.
pub fn validate_tag(tag: &str) -> std::result::Result<(), TagValidationError> {
    if tag.is_empty() {
        return Err(TagValidationError::Empty);
    }
    if tag.len() > MAX_TAG_BYTES {
        return Err(TagValidationError::TooLong { actual: tag.len() });
    }
    if tag.trim() != tag {
        return Err(TagValidationError::Untrimmed);
    }
    if tag.chars().any(char::is_control) {
        return Err(TagValidationError::ControlCharacter);
    }
    Ok(())
}

/// An owned, deterministically ordered canonical node tag set.
///
/// The inner collection stays private so construction and deserialization
/// cannot bypass [`MAX_NODE_TAGS`] or [`validate_tag`]. Serialization remains a
/// plain JSON array, matching the historical interned-tag-set wire shape while
/// no longer retaining attacker-controlled snapshot or canonical data forever.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedTagSet(Arc<[String]>);

impl Default for BoundedTagSet {
    fn default() -> Self {
        Self(Arc::from(Vec::<String>::new()))
    }
}

impl BoundedTagSet {
    pub fn try_from_iter<I, T>(tags: I) -> std::result::Result<Self, TagValidationError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        let mut bounded = BTreeSet::new();
        for tag in tags {
            if bounded.len() == MAX_NODE_TAGS {
                return Err(TagValidationError::TooMany);
            }
            let tag = tag.as_ref();
            validate_tag(tag)?;
            if !bounded.insert(tag.to_owned()) {
                return Err(TagValidationError::Duplicate {
                    tag: tag.to_owned(),
                });
            }
        }
        Ok(Self(Arc::from(bounded.into_iter().collect::<Vec<_>>())))
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    pub fn contains(&self, tag: &str) -> bool {
        self.0
            .binary_search_by(|candidate| candidate.as_str().cmp(tag))
            .is_ok()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for BoundedTagSet {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.as_ref().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BoundedTagSet {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = BoundedTagSet;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "an array of at most {MAX_NODE_TAGS} unique canonical tags"
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut tags = BTreeSet::new();
                while let Some(tag) = sequence.next_element::<String>()? {
                    if tags.len() == MAX_NODE_TAGS {
                        return Err(serde::de::Error::custom(TagValidationError::TooMany));
                    }
                    validate_tag(&tag).map_err(serde::de::Error::custom)?;
                    if !tags.insert(tag.clone()) {
                        return Err(serde::de::Error::custom(TagValidationError::Duplicate {
                            tag,
                        }));
                    }
                }
                Ok(BoundedTagSet(Arc::from(
                    tags.into_iter().collect::<Vec<_>>(),
                )))
            }
        }

        deserializer.deserialize_seq(Visitor)
    }
}

/// Maximum UTF-8 byte length of a canonical node summary.
pub const MAX_NODE_SUMMARY_BYTES: usize = 16 * 1024;

/// Why a canonical node summary was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NodeSummaryValidationError {
    #[error("node summary is {actual} UTF-8 bytes; maximum is {MAX_NODE_SUMMARY_BYTES}")]
    TooLong { actual: usize },
    #[error("node summary must not be blank")]
    Blank,
}

/// A reclaimable, bounded, nonblank node summary.
///
/// Serialization remains a plain JSON string. Cloning shares the allocation;
/// independently constructed equal summaries do not enter a global interner.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeSummary(Arc<str>);

impl NodeSummary {
    pub fn new(value: impl AsRef<str>) -> std::result::Result<Self, NodeSummaryValidationError> {
        let value = value.as_ref();
        validate_node_summary(value)?;
        Ok(Self(Arc::from(value)))
    }

    fn from_owned(value: String) -> std::result::Result<Self, NodeSummaryValidationError> {
        validate_node_summary(&value)?;
        Ok(Self(Arc::from(value)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_node_summary(value: &str) -> std::result::Result<(), NodeSummaryValidationError> {
    if value.len() > MAX_NODE_SUMMARY_BYTES {
        return Err(NodeSummaryValidationError::TooLong {
            actual: value.len(),
        });
    }
    if value.trim().is_empty() {
        return Err(NodeSummaryValidationError::Blank);
    }
    Ok(())
}

impl TryFrom<&str> for NodeSummary {
    type Error = NodeSummaryValidationError;

    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl std::str::FromStr for NodeSummary {
    type Err = NodeSummaryValidationError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl AsRef<str> for NodeSummary {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for NodeSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for NodeSummary {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NodeSummary {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = NodeSummary;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "a nonblank node summary of at most {MAX_NODE_SUMMARY_BYTES} UTF-8 bytes"
                )
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                NodeSummary::new(value).map_err(E::custom)
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                NodeSummary::new(value).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                NodeSummary::from_owned(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

// Coordination-free IDs: no central counter, safe to generate on any node.
// Ulid is time-sortable (good index locality) and 128-bit.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct NodeId(pub Ulid);

/// Bounds for a durable external capture identity. These are storage limits;
/// frontends may impose tighter request budgets.
pub const MAX_CAPTURE_NAMESPACE_BYTES: usize = 64;
pub const MAX_CAPTURE_KEY_BYTES: usize = 512;
pub const MAX_CAPTURE_REFERENCE_BYTES: usize = 2_048;
pub const MAX_CAPTURE_SESSION_BYTES: usize = 512;
pub const MAX_CAPTURE_REVISION_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid capture source {field}: {reason}")]
pub struct CaptureSourceValidationError {
    field: &'static str,
    reason: &'static str,
}

/// Canonical encoding used to prove an immutable incoming capture request.
/// Historical codecs are admitted only for exact replay and named import.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureRequestCodec {
    CaptureV1,
    CaptureV2,
    EpisodeV1,
    EpisodeV2,
    TouchstoneV1,
}

/// Source identity and exact-request proof persisted with a captured node.
/// A key names one logical observation within a namespace; it is not a content
/// hash. Changing an observation requires a new key and an explicit correction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CaptureSource {
    namespace: String,
    key: String,
    reference: String,
    session: Option<String>,
    revision: Option<String>,
    request_digest: [u8; 32],
    request_codec: CaptureRequestCodec,
}

impl CaptureSource {
    pub fn new(
        namespace: &str,
        key: &str,
        reference: &str,
        session: Option<&str>,
        revision: Option<&str>,
        request_digest: [u8; 32],
    ) -> std::result::Result<Self, CaptureSourceValidationError> {
        Self::new_with_codec(
            namespace,
            key,
            reference,
            session,
            revision,
            request_digest,
            CaptureRequestCodec::CaptureV2,
        )
    }

    pub fn new_with_codec(
        namespace: &str,
        key: &str,
        reference: &str,
        session: Option<&str>,
        revision: Option<&str>,
        request_digest: [u8; 32],
        request_codec: CaptureRequestCodec,
    ) -> std::result::Result<Self, CaptureSourceValidationError> {
        let value = Self {
            namespace: namespace.to_owned(),
            key: key.to_owned(),
            reference: reference.to_owned(),
            session: session.map(str::to_owned),
            revision: revision.map(str::to_owned),
            request_digest,
            request_codec,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> std::result::Result<(), CaptureSourceValidationError> {
        fn text(
            field: &'static str,
            value: &str,
            max: usize,
        ) -> std::result::Result<(), CaptureSourceValidationError> {
            if value.is_empty() || value.len() > max {
                return Err(CaptureSourceValidationError {
                    field,
                    reason: "must be nonempty and within the byte limit",
                });
            }
            if value.trim() != value || value.chars().any(char::is_control) {
                return Err(CaptureSourceValidationError {
                    field,
                    reason: "must be trimmed and contain no controls",
                });
            }
            Ok(())
        }
        text("namespace", &self.namespace, MAX_CAPTURE_NAMESPACE_BYTES)?;
        if !self.namespace.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        }) {
            return Err(CaptureSourceValidationError {
                field: "namespace",
                reason: "must be a lowercase ASCII identifier",
            });
        }
        text("key", &self.key, MAX_CAPTURE_KEY_BYTES)?;
        text("reference", &self.reference, MAX_CAPTURE_REFERENCE_BYTES)?;
        if let Some(session) = &self.session {
            text("session", session, MAX_CAPTURE_SESSION_BYTES)?;
        }
        if let Some(revision) = &self.revision {
            text("revision", revision, MAX_CAPTURE_REVISION_BYTES)?;
        }
        Ok(())
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn reference(&self) -> &str {
        &self.reference
    }
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }
    pub fn revision(&self) -> Option<&str> {
        self.revision.as_deref()
    }
    pub fn request_digest(&self) -> [u8; 32] {
        self.request_digest
    }
    pub fn request_codec(&self) -> CaptureRequestCodec {
        self.request_codec
    }

    /// Stable, store-local node identity. A persisted marker is always checked
    /// before treating this hash as a replay, including improbable collisions.
    pub fn node_id(&self) -> NodeId {
        let mut hash = Sha256::new();
        hash.update(b"mneme.capture.node-id.v1\0");
        hash.update((self.namespace.len() as u64).to_be_bytes());
        hash.update(self.namespace.as_bytes());
        hash.update((self.key.len() as u64).to_be_bytes());
        hash.update(self.key.as_bytes());
        let bytes: [u8; 32] = hash.finalize().into();
        NodeId(Ulid::from(u128::from_be_bytes(
            bytes[..16].try_into().expect("digest prefix"),
        )))
    }
}

impl<'de> Deserialize<'de> for CaptureSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            namespace: String,
            key: String,
            reference: String,
            session: Option<String>,
            revision: Option<String>,
            request_digest: [u8; 32],
            request_codec: CaptureRequestCodec,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new_with_codec(
            &wire.namespace,
            &wire.key,
            &wire.reference,
            wire.session.as_deref(),
            wire.revision.as_deref(),
            wire.request_digest,
            wire.request_codec,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Incoming immutable request proof. The optional historical digests are
/// computed from the original request, never from a subsequently edited node.
#[derive(Clone, Debug)]
pub enum CaptureReplayProof {
    Semantic {
        source: CaptureSource,
        legacy_candidate_digest: [u8; 32],
        legacy_active_digest: [u8; 32],
    },
    Episode {
        source: CaptureSource,
    },
    Touchstone {
        source: CaptureSource,
    },
}

impl CaptureReplayProof {
    pub fn semantic(
        source: CaptureSource,
        legacy_candidate_digest: [u8; 32],
        legacy_active_digest: [u8; 32],
    ) -> std::result::Result<Self, CaptureSourceValidationError> {
        if source.request_codec() != CaptureRequestCodec::CaptureV2 {
            return Err(CaptureSourceValidationError {
                field: "request_codec",
                reason: "semantic replay requires capture_v2 incoming source",
            });
        }
        Ok(Self::Semantic {
            source,
            legacy_candidate_digest,
            legacy_active_digest,
        })
    }

    pub fn episode(
        source: CaptureSource,
    ) -> std::result::Result<Self, CaptureSourceValidationError> {
        if !matches!(
            source.request_codec(),
            CaptureRequestCodec::EpisodeV1 | CaptureRequestCodec::EpisodeV2
        ) {
            return Err(CaptureSourceValidationError {
                field: "request_codec",
                reason: "episode replay requires episode_v1 or episode_v2 incoming source",
            });
        }
        Ok(Self::Episode { source })
    }

    pub fn touchstone(
        source: CaptureSource,
    ) -> std::result::Result<Self, CaptureSourceValidationError> {
        if source.request_codec() != CaptureRequestCodec::TouchstoneV1 {
            return Err(CaptureSourceValidationError {
                field: "request_codec",
                reason: "touchstone replay requires touchstone_v1 incoming source",
            });
        }
        Ok(Self::Touchstone { source })
    }

    pub fn source(&self) -> &CaptureSource {
        match self {
            Self::Semantic { source, .. }
            | Self::Episode { source }
            | Self::Touchstone { source } => source,
        }
    }

    pub fn matches_source(&self, stored: &CaptureSource) -> bool {
        let source = self.source();
        if source.namespace != stored.namespace
            || source.key != stored.key
            || source.reference != stored.reference
            || source.session != stored.session
            || source.revision != stored.revision
        {
            return false;
        }
        match self {
            Self::Semantic {
                source,
                legacy_candidate_digest,
                legacy_active_digest,
            } if source.request_codec == CaptureRequestCodec::CaptureV2 => match stored
                .request_codec
            {
                CaptureRequestCodec::CaptureV2 => stored.request_digest == source.request_digest,
                CaptureRequestCodec::CaptureV1 => {
                    stored.request_digest == *legacy_candidate_digest
                        || stored.request_digest == *legacy_active_digest
                }
                CaptureRequestCodec::EpisodeV1
                | CaptureRequestCodec::EpisodeV2
                | CaptureRequestCodec::TouchstoneV1 => false,
            },
            Self::Episode { source }
                if matches!(
                    source.request_codec,
                    CaptureRequestCodec::EpisodeV1 | CaptureRequestCodec::EpisodeV2
                ) =>
            {
                stored.request_codec == source.request_codec
                    && stored.request_digest == source.request_digest
            }
            Self::Touchstone { source }
                if source.request_codec == CaptureRequestCodec::TouchstoneV1 =>
            {
                stored.request_codec == CaptureRequestCodec::TouchstoneV1
                    && stored.request_digest == source.request_digest
            }
            _ => false,
        }
    }
}

/// Maximum number of unique source nodes in one derived provenance record.
pub const MAX_DERIVED_SOURCES: usize = 64;

/// Why a canonical derived-source list was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DerivedSourcesValidationError {
    #[error("derived sources contains duplicate node {node:?}")]
    Duplicate { node: NodeId },
    #[error("derived sources has more than {MAX_DERIVED_SOURCES} entries")]
    TooMany,
}

/// An ordered, unique, bounded list of source node identities.
///
/// Input order is retained because it is part of the historical JSON wire
/// representation. Serialization remains a plain array; clones share one
/// reclaimable allocation rather than entering a process-global interner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DerivedSources(Arc<[NodeId]>);

impl DerivedSources {
    pub fn try_from_iter<I>(sources: I) -> std::result::Result<Self, DerivedSourcesValidationError>
    where
        I: IntoIterator<Item = NodeId>,
    {
        let mut ordered = Vec::new();
        let mut seen = BTreeSet::new();
        for source in sources {
            if ordered.len() == MAX_DERIVED_SOURCES {
                return Err(DerivedSourcesValidationError::TooMany);
            }
            if !seen.insert(source) {
                return Err(DerivedSourcesValidationError::Duplicate { node: source });
            }
            ordered.push(source);
        }
        Ok(Self(Arc::from(ordered)))
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, NodeId> {
        self.0.iter()
    }

    pub fn as_slice(&self) -> &[NodeId] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for DerivedSources {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.as_ref().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DerivedSources {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = DerivedSources;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "an array of at most {MAX_DERIVED_SOURCES} unique node ids"
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut ordered = Vec::new();
                let mut seen = BTreeSet::new();
                while let Some(source) = sequence.next_element::<NodeId>()? {
                    if ordered.len() == MAX_DERIVED_SOURCES {
                        return Err(serde::de::Error::custom(
                            DerivedSourcesValidationError::TooMany,
                        ));
                    }
                    if !seen.insert(source) {
                        return Err(serde::de::Error::custom(
                            DerivedSourcesValidationError::Duplicate { node: source },
                        ));
                    }
                    ordered.push(source);
                }
                Ok(DerivedSources(Arc::from(ordered)))
            }
        }

        deserializer.deserialize_seq(Visitor)
    }
}

/// A non-finite or out-of-range node stability value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("stability must be finite and between 0 and 1")]
pub struct StabilityValidationError;

/// Canonical stability on the closed unit interval.
///
/// Serialization remains a plain JSON number. The private scalar makes NaN,
/// infinities, and values outside `[0, 1]` unrepresentable through safe code.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stability(f32);

impl Stability {
    pub fn new(value: f32) -> std::result::Result<Self, StabilityValidationError> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(StabilityValidationError)
        }
    }

    pub const fn get(self) -> f32 {
        self.0
    }

    pub const fn to_bits(self) -> u32 {
        self.0.to_bits()
    }
}

impl TryFrom<f32> for Stability {
    type Error = StabilityValidationError;

    fn try_from(value: f32) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for Stability {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f32(self.0)
    }
}

impl<'de> Deserialize<'de> for Stability {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Stability;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a finite number between 0 and 1")
            }

            fn visit_f32<E>(self, value: f32) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Stability::new(value).map_err(E::custom)
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    return Err(E::custom(StabilityValidationError));
                }
                Stability::new(value as f32).map_err(E::custom)
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if !(0..=1).contains(&value) {
                    return Err(E::custom(StabilityValidationError));
                }
                Stability::new(value as f32).map_err(E::custom)
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value > 1 {
                    return Err(E::custom(StabilityValidationError));
                }
                Stability::new(value as f32).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// A non-finite or out-of-range node confidence value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("confidence must be finite and between 0 and 1")]
pub struct ConfidenceValidationError;

/// Authored retention/use value on the closed unit interval.
///
/// `Confidence` is the historical field name, not a measure of factual truth.
///
/// Serialization remains a plain JSON number. The private scalar makes NaN,
/// infinities, and values outside `[0, 1]` unrepresentable through safe code.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Confidence(f32);

impl Confidence {
    pub fn new(value: f32) -> std::result::Result<Self, ConfidenceValidationError> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ConfidenceValidationError)
        }
    }

    pub const fn get(self) -> f32 {
        self.0
    }

    pub const fn to_bits(self) -> u32 {
        self.0.to_bits()
    }
}

impl TryFrom<f32> for Confidence {
    type Error = ConfidenceValidationError;

    fn try_from(value: f32) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for Confidence {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f32(self.0)
    }
}

impl<'de> Deserialize<'de> for Confidence {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Confidence;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a finite number between 0 and 1")
            }

            fn visit_f32<E>(self, value: f32) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Confidence::new(value).map_err(E::custom)
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    return Err(E::custom(ConfidenceValidationError));
                }
                Confidence::new(value as f32).map_err(E::custom)
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if !(0..=1).contains(&value) {
                    return Err(E::custom(ConfidenceValidationError));
                }
                Confidence::new(value as f32).map_err(E::custom)
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value > 1 {
                    return Err(E::custom(ConfidenceValidationError));
                }
                Confidence::new(value as f32).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// Why an attempted confidence mutation was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfidenceMutationError {
    #[error("confidence mutation value must be finite and between 0 and 1")]
    InvalidValue,
}

/// Unix epoch milliseconds. Deliberately a dumb integer: the domain never reads
/// the wall clock itself — a [`ports::Clock`] hands it the current time, which
/// keeps decay and reinforcement testable with a fake clock.
pub type Timestamp = u128;

/// Versioned identity of the complete embedding contract behind a vector index.
///
/// Dimension alone is not an identity: two models (or two query adapters) can
/// produce equally-sized vectors that live in incompatible spaces. Hosts persist
/// this value beside the index and refuse to mix it with a different runtime
/// embedder. `embedding_id` is a stable implementation/model identifier;
/// `normalization` and `query_mode` are explicit because changing either alters
/// cosine-search semantics without necessarily changing the model or dimension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingFingerprint {
    pub format_version: u32,
    pub embedding_id: String,
    pub dimension: usize,
    pub normalization: String,
    pub query_mode: String,
}

impl EmbeddingFingerprint {
    /// Current serialized fingerprint format. Bump this when the identity fields
    /// or their comparison semantics change.
    pub const FORMAT_VERSION: u32 = 1;

    pub fn new(
        embedding_id: impl Into<String>,
        dimension: usize,
        normalization: impl Into<String>,
        query_mode: impl Into<String>,
    ) -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            embedding_id: embedding_id.into(),
            dimension,
            normalization: normalization.into(),
            query_mode: query_mode.into(),
        }
    }

    /// Reject malformed or future-format identities rather than accidentally
    /// treating partially-understood metadata as compatible.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.format_version != Self::FORMAT_VERSION {
            return Err(format!(
                "unsupported format version {} (runtime supports {})",
                self.format_version,
                Self::FORMAT_VERSION
            ));
        }
        if self.dimension == 0 {
            return Err("dimension must be positive".into());
        }
        for (name, value) in [
            ("embedding_id", self.embedding_id.as_str()),
            ("normalization", self.normalization.as_str()),
            ("query_mode", self.query_mode.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{name} must not be empty"));
            }
        }
        Ok(())
    }
}

impl std::fmt::Display for EmbeddingFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "v{}:{} (dim={}, norm={}, query={})",
            self.format_version,
            self.embedding_id,
            self.dimension,
            self.normalization,
            self.query_mode
        )
    }
}

/// Whether this database is responsible for erasing a node's external body.
///
/// `Managed` is a transferable responsibility token, not a claim that the body
/// has exactly one referring node: several nodes may share one [`BodyRef`], but
/// the body is erased only after the final local reference disappears. Explicit
/// pre-existing references are `Borrowed` and are never erased by `forget`.
/// Legacy node blobs default to `Borrowed`, which is the only migration choice
/// that cannot destroy a caller-owned resource.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyOwnership {
    #[default]
    Borrowed,
    Managed,
}

/// A full canonical Git object id attached to a memory as its source-code
/// anchor. Git repositories currently use either SHA-1 (40 lowercase hex
/// characters) or SHA-256 (64 lowercase hex characters); abbreviated or
/// otherwise non-canonical spellings are rejected so persisted anchors remain
/// unambiguous.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum OriginCommit {
    Sha1([u8; 20]),
    Sha256([u8; 32]),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OriginCommitError {
    #[error(
        "origin commit must be exactly 40 or 64 lowercase hexadecimal characters, got {actual} bytes"
    )]
    InvalidLength { actual: usize },
    #[error("origin commit contains a non-lowercase-hex byte at offset {index}")]
    InvalidHex { index: usize },
}

impl OriginCommit {
    pub fn parse(value: &str) -> std::result::Result<Self, OriginCommitError> {
        match value.len() {
            40 => {
                let mut digest = [0_u8; 20];
                decode_lower_hex(value.as_bytes(), &mut digest)?;
                Ok(Self::Sha1(digest))
            }
            64 => {
                let mut digest = [0_u8; 32];
                decode_lower_hex(value.as_bytes(), &mut digest)?;
                Ok(Self::Sha256(digest))
            }
            actual => Err(OriginCommitError::InvalidLength { actual }),
        }
    }

    pub const fn algorithm(self) -> &'static str {
        match self {
            Self::Sha1(_) => "sha1",
            Self::Sha256(_) => "sha256",
        }
    }

    fn write_hex(self, buffer: &mut [u8; 64]) -> &str {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let bytes: &[u8] = match &self {
            Self::Sha1(digest) => digest,
            Self::Sha256(digest) => digest,
        };
        for (index, byte) in bytes.iter().copied().enumerate() {
            buffer[index * 2] = HEX[usize::from(byte >> 4)];
            buffer[index * 2 + 1] = HEX[usize::from(byte & 0x0f)];
        }
        std::str::from_utf8(&buffer[..bytes.len() * 2])
            .expect("lowercase hexadecimal bytes are valid UTF-8")
    }
}

fn decode_lower_hex(
    encoded: &[u8],
    destination: &mut [u8],
) -> std::result::Result<(), OriginCommitError> {
    for (index, pair) in encoded.chunks_exact(2).enumerate() {
        let high =
            lower_hex_nibble(pair[0]).ok_or(OriginCommitError::InvalidHex { index: index * 2 })?;
        let low = lower_hex_nibble(pair[1]).ok_or(OriginCommitError::InvalidHex {
            index: index * 2 + 1,
        })?;
        destination[index] = (high << 4) | low;
    }
    Ok(())
}

const fn lower_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl fmt::Display for OriginCommit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buffer = [0_u8; 64];
        formatter.write_str(self.write_hex(&mut buffer))
    }
}

impl fmt::Debug for OriginCommit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("OriginCommit")
            .field(&format_args!("{self}"))
            .finish()
    }
}

impl std::str::FromStr for OriginCommit {
    type Err = OriginCommitError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for OriginCommit {
    type Error = OriginCommitError;

    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl Serialize for OriginCommit {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut buffer = [0_u8; 64];
        serializer.serialize_str(self.write_hex(&mut buffer))
    }
}

impl<'de> Deserialize<'de> for OriginCommit {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = OriginCommit;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a full lowercase SHA-1 or SHA-256 Git object id")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                OriginCommit::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

impl BodyOwnership {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Borrowed => "borrowed",
            Self::Managed => "managed",
        }
    }
}

/// Why raw canonical-node construction was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NodeValidationError {
    #[error(transparent)]
    Episode(#[from] episode::EpisodeValidationError),
    #[error(transparent)]
    Summary(#[from] NodeSummaryValidationError),
    #[error(transparent)]
    BodyRef(#[from] BodyRefValidationError),
    #[error(transparent)]
    Tags(#[from] TagValidationError),
    #[error(transparent)]
    WebUrl(#[from] WebUrlValidationError),
    #[error(transparent)]
    DerivedSources(#[from] DerivedSourcesValidationError),
    #[error(transparent)]
    CaptureSource(#[from] CaptureSourceValidationError),
    #[error(transparent)]
    Stability(#[from] StabilityValidationError),
    #[error(transparent)]
    Confidence(#[from] ConfidenceValidationError),
}

/// Already-validated inputs for infallible canonical-node construction.
///
/// The fields are public for ergonomic struct literals, but every field that
/// carries a lexical or scalar invariant is itself an unforgeable value object.
#[derive(Clone, Debug)]
pub struct NodeInit {
    pub id: NodeId,
    pub summary: NodeSummary,
    pub body: BodyRef,
    pub tags: BoundedTagSet,
    pub provenance: Provenance,
    pub stability: Stability,
    pub confidence: Confidence,
    pub status: NodeStatus,
    pub created: Timestamp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "NodeWire")]
pub struct Node {
    id: NodeId,
    #[serde(default, skip_serializing_if = "MemoryKind::is_semantic")]
    memory_kind: MemoryKind,
    /// Short, embedded, matched against during retrieval.
    summary: NodeSummary,
    /// Pointer, never inline. Resolved via [`ports::BodyStore`].
    body: BodyRef,
    /// Responsibility for deleting `body` when its final local reference is
    /// forgotten. Missing in legacy JSON blobs, which fail safe as borrowed.
    #[serde(default)]
    body_ownership: BodyOwnership,
    tags: BoundedTagSet,
    provenance: Provenance,
    /// The VCS commit the repo was at when this memory was formed, if it was
    /// formed inside a git working tree. A temporal/spatial anchor orthogonal to
    /// [`Provenance`]: *when in
    /// the project's history* the knowledge was learned, so a later reader can tell
    /// a fact captured against today's code from one captured ten commits ago. The
    /// domain never reads git itself — a host that has a working tree stamps it in
    /// via [`Node::with_origin_commit`]. `#[serde(default)]` so older snapshots
    /// without the field still load.
    #[serde(default)]
    origin_commit: Option<OriginCommit>,
    /// The knowledge<->analysis spectrum, NOT a binary.
    /// 0.0 = volatile (a webpage that can go stale)
    /// 1.0 = battle-tested invariant (only changes if assumptions change)
    /// Authored volatility metadata; current maintenance does not automatically
    /// decay or expire nodes from this value.
    stability: Stability,
    confidence: Confidence,
    created: Timestamp,
    /// Most recent time retrieval returned this node. Legacy blobs called this
    /// `last_activated`; that value is retained as exposure telemetry because
    /// the old counter mixed exposure with explicit use and cannot be safely
    /// reconstructed as grounded evidence.
    #[serde(default, alias = "last_activated")]
    last_exposed: Option<Timestamp>,
    /// Number of retrieval responses that returned this node. The legacy
    /// `activation_count` loads here, again conservatively treating ambiguous
    /// historical observations as exposure rather than proof of relevance.
    #[serde(default, alias = "activation_count")]
    exposure_count: u64,
    /// Most recent explicit relevant/not-new feedback for this node. Missing
    /// from legacy blobs, so old stores correctly migrate to "unknown/none"
    /// instead of fabricating grounded-use history.
    #[serde(default)]
    last_grounded_use: Option<Timestamp>,
    /// Count of explicit relevant/not-new feedback events.
    #[serde(default)]
    grounded_use_count: u64,
    /// Retained historical node-interference metadata. Successor feedback and
    /// maintenance never accumulate or spend this value. `#[serde(default)]`
    /// keeps older snapshots with no such field readable.
    #[serde(default)]
    interference: u32,
    status: NodeStatus,
}

// Serde has no post-deserialization validation hook. Keep the raw wire fields
// private and move them exhaustively into Node, then check cross-field episode
// invariants. NodeInit still constructs unchanged legacy semantic nodes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeWire {
    id: NodeId,
    #[serde(default)]
    memory_kind: MemoryKind,
    summary: NodeSummary,
    body: BodyRef,
    #[serde(default)]
    body_ownership: BodyOwnership,
    tags: BoundedTagSet,
    provenance: Provenance,
    #[serde(default)]
    origin_commit: Option<OriginCommit>,
    stability: Stability,
    confidence: Confidence,
    created: Timestamp,
    #[serde(default, alias = "last_activated")]
    last_exposed: Option<Timestamp>,
    #[serde(default, alias = "activation_count")]
    exposure_count: u64,
    #[serde(default)]
    last_grounded_use: Option<Timestamp>,
    #[serde(default)]
    grounded_use_count: u64,
    #[serde(default)]
    interference: u32,
    status: NodeStatus,
}

impl TryFrom<NodeWire> for Node {
    type Error = NodeValidationError;
    fn try_from(w: NodeWire) -> std::result::Result<Self, Self::Error> {
        let node = Self {
            id: w.id,
            memory_kind: w.memory_kind,
            summary: w.summary,
            body: w.body,
            body_ownership: w.body_ownership,
            tags: w.tags,
            provenance: w.provenance,
            origin_commit: w.origin_commit,
            stability: w.stability,
            confidence: w.confidence,
            created: w.created,
            last_exposed: w.last_exposed,
            exposure_count: w.exposure_count,
            last_grounded_use: w.last_grounded_use,
            grounded_use_count: w.grounded_use_count,
            interference: w.interference,
            status: w.status,
        };
        node.validate()?;
        Ok(node)
    }
}

impl Node {
    /// Build a canonical node from already-validated parts. Time remains an
    /// injected value so construction is deterministic under test.
    pub fn new(init: NodeInit) -> Self {
        Self {
            id: init.id,
            memory_kind: MemoryKind::Semantic,
            summary: init.summary,
            body: init.body,
            body_ownership: BodyOwnership::Borrowed,
            tags: init.tags,
            provenance: init.provenance,
            origin_commit: None,
            stability: init.stability,
            confidence: init.confidence,
            created: init.created,
            last_exposed: None,
            exposure_count: 0,
            last_grounded_use: None,
            grounded_use_count: 0,
            interference: 0,
            status: init.status,
        }
    }

    /// Ergonomic raw boundary for callers that do not already hold value
    /// objects. Every invariant is validated before a [`Node`] is returned;
    /// there is no unchecked raw constructor.
    #[expect(clippy::too_many_arguments)]
    pub fn try_new<I, T>(
        id: NodeId,
        summary: impl AsRef<str>,
        body: BodyRef,
        tags: I,
        provenance: Provenance,
        stability: f32,
        confidence: f32,
        status: NodeStatus,
        now: Timestamp,
    ) -> std::result::Result<Self, NodeValidationError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        Ok(Self::new(NodeInit {
            id,
            summary: NodeSummary::new(summary)?,
            body,
            tags: BoundedTagSet::try_from_iter(tags)?,
            provenance,
            stability: Stability::new(stability)?,
            confidence: Confidence::new(confidence)?,
            status,
            created: now,
        }))
    }

    pub fn memory_kind(&self) -> &MemoryKind {
        &self.memory_kind
    }
    pub fn is_semantic(&self) -> bool {
        self.memory_kind.is_semantic()
    }
    pub fn episode(&self) -> Option<&EpisodeFacet> {
        match &self.memory_kind {
            MemoryKind::Semantic => None,
            MemoryKind::Episode(facet) => Some(facet),
        }
    }
    pub fn with_episode(
        mut self,
        facet: EpisodeFacet,
    ) -> std::result::Result<Self, NodeValidationError> {
        if !self.is_semantic() {
            return Err(EpisodeValidationError("episode facet is immutable").into());
        }
        facet.validate_node(&self)?;
        self.memory_kind = MemoryKind::Episode(facet);
        self.validate()?;
        Ok(self)
    }
    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn summary(&self) -> &str {
        self.summary.as_str()
    }

    /// Replace only the checked semantic summary. Stores enforce edit authority,
    /// summary-snapshot comparison and atomic retrieval projection replacement.
    pub fn set_summary(&mut self, summary: NodeSummary) {
        self.summary = summary;
    }

    pub fn stability(&self) -> f32 {
        self.stability.get()
    }

    pub fn confidence(&self) -> f32 {
        self.confidence.get()
    }

    /// Recheck every bounded canonical-node invariant at an adapter boundary.
    ///
    /// Safe construction and deserialization already make these checks
    /// redundant in ordinary domain code. Keeping one bounded validator is
    /// nevertheless useful defense in depth before persistence and after a
    /// backend decode: it gives every adapter the same fail-closed contract
    /// without exposing the fields or relying on backend-specific assumptions.
    pub fn validate(&self) -> std::result::Result<(), NodeValidationError> {
        if let Some(facet) = self.episode() {
            facet.validate_node(self)?;
        }
        if let Provenance::External { source } = &self.provenance {
            let valid_codec = match self.memory_kind() {
                MemoryKind::Semantic => matches!(
                    source.request_codec(),
                    CaptureRequestCodec::CaptureV1
                        | CaptureRequestCodec::CaptureV2
                        | CaptureRequestCodec::TouchstoneV1
                ),
                MemoryKind::Episode(facet) => {
                    source.request_codec()
                        == if facet.occurrence_contexts().is_some() {
                            CaptureRequestCodec::EpisodeV2
                        } else {
                            CaptureRequestCodec::EpisodeV1
                        }
                }
            };
            if !valid_codec {
                return Err(CaptureSourceValidationError {
                    field: "request_codec",
                    reason: "codec does not match enclosing memory kind",
                }
                .into());
            }
        }
        validate_node_summary(self.summary.as_str())?;
        validate_body_ref(self.body.as_str())?;

        // Reconstruction rechecks lexical validity, cardinality, uniqueness,
        // and canonical set ordering while doing work bounded by MAX_NODE_TAGS.
        let tags = BoundedTagSet::try_from_iter(self.tags.iter())?;
        if tags != self.tags {
            return Err(TagValidationError::NonCanonicalOrder.into());
        }

        match &self.provenance {
            Provenance::Web { url, .. } => validate_web_url(url.as_str())?,
            Provenance::Conversation { .. } => {}
            Provenance::External { source } => source.validate()?,
            Provenance::Derived { from } => {
                // Preserve order while rechecking the bounded unique-source
                // invariant. This consumes at most MAX_DERIVED_SOURCES ids.
                DerivedSources::try_from_iter(from.iter().copied())?;
            }
        }

        Stability::new(self.stability.get())?;
        Confidence::new(self.confidence.get())?;
        Ok(())
    }

    pub fn body(&self) -> &BodyRef {
        &self.body
    }

    pub fn body_revision(&self) -> BodyRevision {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(b"mneme:body-pointer-revision:v1\0");
        hash.update(self.id.0.to_bytes());
        hash.update((self.body.as_str().len() as u64).to_be_bytes());
        hash.update(self.body.as_str().as_bytes());
        BodyRevision(format!("{:x}", hash.finalize()))
    }

    /// Replace only the checked body pointer and its deletion ownership.
    pub fn set_body_reference(&mut self, body: BodyRef, ownership: BodyOwnership) {
        self.body = body;
        self.body_ownership = ownership;
    }

    pub fn body_ownership(&self) -> BodyOwnership {
        self.body_ownership
    }

    /// Assign or transfer responsibility for eventually erasing this node's
    /// body. The engine uses this after it writes fresh bytes and when forgetting
    /// a managed node that still has local sharers.
    pub fn with_body_ownership(mut self, ownership: BodyOwnership) -> Self {
        self.body_ownership = ownership;
        self
    }

    pub fn set_body_ownership(&mut self, ownership: BodyOwnership) {
        self.body_ownership = ownership;
    }

    pub fn tags(&self) -> impl ExactSizeIterator<Item = &str> {
        self.tags.iter()
    }

    pub fn tag_set(&self) -> &BoundedTagSet {
        &self.tags
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.contains(tag)
    }

    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// Stamp the git commit this memory was formed against. Consuming builder used
    /// at construction (by the host that has the working tree) — the field is
    /// write-once immutable metadata, so there's no in-place setter. `None` leaves
    /// the node unanchored (formed outside a repo, e.g. a user-store memory).
    pub fn with_origin_commit(mut self, commit: Option<OriginCommit>) -> Self {
        self.origin_commit = commit;
        self
    }

    /// The git commit the repo was at when this memory was formed, if any.
    pub fn origin_commit(&self) -> Option<OriginCommit> {
        self.origin_commit
    }

    pub fn created(&self) -> Timestamp {
        self.created
    }

    pub fn last_exposed(&self) -> Option<Timestamp> {
        self.last_exposed
    }

    pub fn exposure_count(&self) -> u64 {
        self.exposure_count
    }

    pub fn last_grounded_use(&self) -> Option<Timestamp> {
        self.last_grounded_use
    }

    pub fn grounded_use_count(&self) -> u64 {
        self.grounded_use_count
    }

    pub fn interference(&self) -> u32 {
        self.interference
    }

    /// Receipt feedback may advance grounded-use telemetry only. Historical
    /// confidence and interference values are immutable in this path.
    pub fn same_feedback_static_fields(&self, other: &Self) -> bool {
        self.id == other.id
            && self.memory_kind == other.memory_kind
            && self.summary == other.summary
            && self.body == other.body
            && self.body_ownership == other.body_ownership
            && self.tags == other.tags
            && self.provenance == other.provenance
            && self.origin_commit == other.origin_commit
            && self.stability.to_bits() == other.stability.to_bits()
            && self.confidence.to_bits() == other.confidence.to_bits()
            && self.interference == other.interference
            && self.created == other.created
            && self.last_exposed == other.last_exposed
            && self.exposure_count == other.exposure_count
            && self.status == other.status
    }

    /// Replace confidence with an already-validated value.
    pub fn set_confidence(&mut self, confidence: Confidence) {
        self.confidence = confidence;
    }

    /// Validate a raw confidence value before mutating this node.
    pub fn try_set_confidence(
        &mut self,
        confidence: f32,
    ) -> std::result::Result<(), ConfidenceMutationError> {
        let confidence =
            Confidence::new(confidence).map_err(|_| ConfidenceMutationError::InvalidValue)?;
        self.set_confidence(confidence);
        Ok(())
    }

    pub fn status(&self) -> NodeStatus {
        self.status
    }

    pub fn is_active(&self) -> bool {
        matches!(self.status, NodeStatus::Active)
    }

    pub fn is_archived(&self) -> bool {
        matches!(self.status, NodeStatus::Archived)
    }

    /// Record that retrieval surfaced this node. This is observability, not a
    /// relevance judgement or an automatic lifecycle transition.
    pub fn record_exposure(&mut self, now: Timestamp) {
        self.last_exposed = Some(now);
        self.exposure_count = self.exposure_count.saturating_add(1);
    }

    /// Record an explicit relevant/not-new observation as telemetry only.
    /// Retrieval and feedback do not change lifecycle membership or confidence.
    pub fn record_grounded_use(&mut self, now: Timestamp) {
        self.last_grounded_use = Some(now);
        self.grounded_use_count = self.grounded_use_count.saturating_add(1);
    }

    /// Replace the complete authored tag set without changing other fields.
    pub fn replace_tags(&mut self, tags: BoundedTagSet) {
        self.tags = tags;
    }

    pub fn set_status(&mut self, status: NodeStatus) {
        self.status = status;
    }
}

/// Search membership is explicit: ordinary memories are Active at creation,
/// while Archived memories are excluded from ordinary recall until restored.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum NodeStatus {
    Active,
    Archived,
}

/// Maximum retained UTF-8 byte length of a web provenance URL.
pub const MAX_WEB_URL_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WebUrlValidationError {
    #[error("web URL is {actual} UTF-8 bytes; maximum is {MAX_WEB_URL_BYTES}")]
    TooLong { actual: usize },
    #[error("web URL must start with lowercase http:// or https://")]
    InvalidScheme,
    #[error("web URL must not contain control characters")]
    ControlCharacter,
    #[error("web URL must not contain whitespace")]
    Whitespace,
    #[error("web URL is not a syntactically valid absolute URL")]
    InvalidSyntax,
    #[error("web URL must have a nonempty host")]
    MissingHost,
}

/// A reclaimable, bounded HTTP(S) provenance URL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WebUrl(Arc<str>);

impl WebUrl {
    pub fn new(value: impl AsRef<str>) -> std::result::Result<Self, WebUrlValidationError> {
        let value = value.as_ref();
        validate_web_url(value)?;
        Ok(Self(Arc::from(value)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    fn shared_text(&self) -> &Arc<str> {
        &self.0
    }
}

fn validate_web_url(value: &str) -> std::result::Result<(), WebUrlValidationError> {
    if value.len() > MAX_WEB_URL_BYTES {
        return Err(WebUrlValidationError::TooLong {
            actual: value.len(),
        });
    }
    let remainder = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .ok_or(WebUrlValidationError::InvalidScheme)?;
    if value.chars().any(char::is_control) {
        return Err(WebUrlValidationError::ControlCharacter);
    }
    if value.chars().any(char::is_whitespace) {
        return Err(WebUrlValidationError::Whitespace);
    }
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    if authority_end == 0 {
        return Err(WebUrlValidationError::MissingHost);
    }
    let parsed = url::Url::parse(value).map_err(|_| WebUrlValidationError::InvalidSyntax)?;
    if !parsed.has_host() || parsed.host_str().is_none_or(str::is_empty) {
        return Err(WebUrlValidationError::MissingHost);
    }
    Ok(())
}

impl TryFrom<&str> for WebUrl {
    type Error = WebUrlValidationError;

    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for WebUrl {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for WebUrl {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = WebUrl;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "an HTTP(S) URL of at most {MAX_WEB_URL_BYTES} UTF-8 bytes"
                )
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                WebUrl::new(value).map_err(E::custom)
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                WebUrl::new(value).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                WebUrl::new(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Provenance {
    Web { url: WebUrl, fetched: Timestamp },
    Conversation { session: Ulid, turn: u32 },
    External { source: CaptureSource },
    Derived { from: DerivedSources }, // analysis built atop other nodes
}

#[cfg(test)]
mod capture_source_tests {
    use super::*;

    fn source(key: &str) -> CaptureSource {
        CaptureSource::new(
            "codex",
            key,
            "codex://sessions/550e8400-e29b-41d4-a716-446655440000#turn=7",
            Some("550e8400-e29b-41d4-a716-446655440000"),
            Some("turn-7"),
            [9; 32],
        )
        .unwrap()
    }

    #[test]
    fn external_source_preserves_opaque_session_and_identity() {
        let first = source("observation-1");
        let restored: CaptureSource =
            serde_json::from_value(serde_json::to_value(&first).unwrap()).unwrap();
        assert_eq!(restored, first);
        assert_eq!(
            restored.session(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(restored.node_id(), first.node_id());
        assert_ne!(source("observation-2").node_id(), first.node_id());
        let changed_digest = CaptureSource::new(
            first.namespace(),
            first.key(),
            first.reference(),
            first.session(),
            first.revision(),
            [10; 32],
        )
        .unwrap();
        assert_eq!(changed_digest.node_id(), first.node_id());
        assert_ne!(changed_digest, first);
    }

    #[test]
    fn replay_proof_matches_only_original_fields_and_codec_domain() {
        assert_eq!(
            serde_json::to_value(CaptureRequestCodec::CaptureV1).unwrap(),
            "capture_v1"
        );
        assert_eq!(
            serde_json::to_value(CaptureRequestCodec::CaptureV2).unwrap(),
            "capture_v2"
        );
        assert_eq!(
            serde_json::to_value(CaptureRequestCodec::EpisodeV1).unwrap(),
            "episode_v1"
        );
        let incoming = source("key");
        let proof = CaptureReplayProof::semantic(incoming.clone(), [1; 32], [2; 32]).unwrap();
        assert!(proof.matches_source(&incoming));
        for digest in [[1; 32], [2; 32]] {
            let old = CaptureSource::new_with_codec(
                incoming.namespace(),
                incoming.key(),
                incoming.reference(),
                incoming.session(),
                incoming.revision(),
                digest,
                CaptureRequestCodec::CaptureV1,
            )
            .unwrap();
            assert!(proof.matches_source(&old));
        }
        let changed = CaptureSource::new_with_codec(
            incoming.namespace(),
            incoming.key(),
            "changed-reference",
            incoming.session(),
            incoming.revision(),
            [1; 32],
            CaptureRequestCodec::CaptureV1,
        )
        .unwrap();
        assert!(!proof.matches_source(&changed));
        let episode = CaptureSource::new_with_codec(
            incoming.namespace(),
            incoming.key(),
            incoming.reference(),
            incoming.session(),
            incoming.revision(),
            [1; 32],
            CaptureRequestCodec::EpisodeV1,
        )
        .unwrap();
        assert!(!proof.matches_source(&episode));
        assert!(
            CaptureReplayProof::episode(episode.clone())
                .unwrap()
                .matches_source(&episode)
        );
        assert!(
            !CaptureReplayProof::episode(episode)
                .unwrap()
                .matches_source(&incoming)
        );
        let episode_v2 = CaptureSource::new_with_codec(
            incoming.namespace(),
            incoming.key(),
            incoming.reference(),
            incoming.session(),
            incoming.revision(),
            [1; 32],
            CaptureRequestCodec::EpisodeV2,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(CaptureRequestCodec::EpisodeV2).unwrap(),
            "episode_v2"
        );
        let episode_v2_proof = CaptureReplayProof::episode(episode_v2.clone()).unwrap();
        assert!(episode_v2_proof.matches_source(&episode_v2));
        let episode_v1 = CaptureSource::new_with_codec(
            incoming.namespace(),
            incoming.key(),
            incoming.reference(),
            incoming.session(),
            incoming.revision(),
            [1; 32],
            CaptureRequestCodec::EpisodeV1,
        )
        .unwrap();
        assert!(!episode_v2_proof.matches_source(&episode_v1));
        assert!(
            !CaptureReplayProof::episode(episode_v1)
                .unwrap()
                .matches_source(&episode_v2)
        );
        assert!(!proof.matches_source(&episode_v2));
        assert!(CaptureReplayProof::semantic(episode_v2, [1; 32], [2; 32]).is_err());
        let forged = CaptureReplayProof::Semantic {
            source: CaptureSource::new_with_codec(
                incoming.namespace(),
                incoming.key(),
                incoming.reference(),
                incoming.session(),
                incoming.revision(),
                incoming.request_digest(),
                CaptureRequestCodec::EpisodeV1,
            )
            .unwrap(),
            legacy_candidate_digest: [1; 32],
            legacy_active_digest: [2; 32],
        };
        assert!(!forged.matches_source(&incoming));
    }

    #[test]
    fn external_source_rejects_malformed_or_oversized_inputs_on_decode() {
        assert!(CaptureSource::new("Codex", "key", "ref", None, None, [0; 32]).is_err());
        assert!(CaptureSource::new("codex", " key", "ref", None, None, [0; 32]).is_err());
        assert!(
            CaptureSource::new(
                "codex",
                "key",
                &"x".repeat(MAX_CAPTURE_REFERENCE_BYTES + 1),
                None,
                None,
                [0; 32]
            )
            .is_err()
        );
        let mut encoded = serde_json::to_value(source("key")).unwrap();
        encoded["session"] = serde_json::json!("\n");
        assert!(serde_json::from_value::<CaptureSource>(encoded).is_err());
        let mut missing_codec = serde_json::to_value(source("key")).unwrap();
        missing_codec
            .as_object_mut()
            .unwrap()
            .remove("request_codec");
        assert!(serde_json::from_value::<CaptureSource>(missing_codec).is_err());
    }
}

impl Provenance {
    pub fn derived<I>(sources: I) -> std::result::Result<Self, DerivedSourcesValidationError>
    where
        I: IntoIterator<Item = NodeId>,
    {
        Ok(Self::Derived {
            from: DerivedSources::try_from_iter(sources)?,
        })
    }

    pub fn derived_empty() -> Self {
        Self::Derived {
            from: DerivedSources::empty(),
        }
    }
}

/// The shape of the edge-strength curve: how co-activation strengthens an edge
/// and how competitive interference erodes it. Pure domain math, parameterized
/// so the *formula* lives with the type it governs while the *values* are a
/// policy decision configured at the engine (its `Config` builds and threads one
/// of these in). `weight` is never set by hand — it is always the output of
/// [`Edge::reinforce`] / [`Edge::decay`] applied to these parameters.
///
/// Forgetting here is driven by *emitted signals*, not the wall clock: an edge
/// decays in proportion to how often it carried irrelevant activation (see
/// [`Edge::mark_interference`]) since the last sweep, so leaving a graph idle
/// never erodes it — only actively using the graph *for other things* does.
#[derive(Clone, Copy, Debug)]
pub struct StrengthParams {
    /// Diminishing-returns step toward 1.0 per hit (co-activation):
    /// `w += (1-w)*gain`. The first hits move an edge far more than later ones,
    /// so five accesses are worth well over a fifth of twenty-five.
    pub reinforce_gain: f32,
    /// Per-miss weight retention for a *fresh* edge (∈ (0, 1)): how much weight
    /// survives one banked interference event when the edge has little history.
    /// Lower ⇒ an unproven edge is cheap to forget.
    pub interference_retention: f32,
    /// Per-miss weight retention a *well-worn* edge approaches as its trial count
    /// grows. Strictly < 1, so even a deeply established edge stays killable —
    /// just slowly. The gap to [`interference_retention`] is the "stickiness"
    /// that makes a proven edge cost many misses to tear down.
    pub interference_resist: f32,
    /// Trials (hits + misses) at which per-miss retention sits halfway between
    /// [`interference_retention`] and [`interference_resist`] — i.e. how quickly
    /// an edge becomes "well-worn".
    pub resist_trials_half: f32,
}

impl Default for StrengthParams {
    fn default() -> Self {
        Self {
            reinforce_gain: 0.35,
            interference_retention: 0.6,
            interference_resist: 0.95,
            resist_trials_half: 20.0,
        }
    }
}

/// An association between two nodes. Identity is the endpoint pair `(from, to)`:
/// there is exactly one edge per directed pair, and an `Associative` edge is
/// undirected so its single row is surfaced from either end. The structural
/// fields are public plain data; the dynamic ones (`weight` and the raw signals
/// behind it) are private because they carry invariants — they move only through
/// [`reinforce`](Edge::reinforce), [`mark_interference`](Edge::mark_interference)
/// and [`decay`](Edge::decay), never by direct assignment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
    /// Optional chunk-level anchoring: "*this* span of `from`'s body is what's
    /// relevant to `to`." Byte range into the resolved body.
    pub anchor: Option<BodySpan>,
    /// Association strength in `[0, 1]` that attenuates spreading activation, and
    /// the edge's whole story: a hit raises it (diminishing returns), banked
    /// misses lower it. Recency-weighted by construction — it tracks how relevant
    /// the edge has *recently* been, not a lifetime tally. Whether it's stored
    /// (as here) or recomputed on read is an implementation detail behind
    /// [`weight`](Edge::weight); never assign it directly.
    weight: f32,
    /// Wall-clock of the last hit. Observability only — decay is driven by
    /// emitted interference, not the clock (see [`StrengthParams`]).
    last_reinforced: Timestamp,
    /// Trials seen (hits + misses). Only slows how fast misses can erode
    /// `weight`, via [`interference_resist`](StrengthParams::interference_resist),
    /// making a well-worn edge sticky — much harder to kill, but never immortal.
    trials: u32,
    /// Emitted-but-unprocessed interference: misses since the last
    /// [`decay`](Edge::decay). Consumed and reset each sweep, which is what makes
    /// decay idempotent.
    interference: u32,
}

impl Edge {
    /// Mint a new edge. `weight` is the initial prior — an ANN similarity score
    /// for a similarity-seeded link, or an agent-asserted strength — before any
    /// trial evidence exists.
    pub fn new(from: NodeId, to: NodeId, weight: f32, kind: EdgeKind, now: Timestamp) -> Self {
        // Keep newly minted state finite and canonical even when a library caller
        // bypasses a frontend validator. Infinities saturate; NaN carries no
        // ordering or strength information and becomes the weakest association.
        // Persisted state still goes through `from_stored` + `validate`, so corrupt
        // rows fail closed instead of being silently repaired on read.
        let weight = if weight.is_nan() {
            0.0
        } else {
            weight.clamp(0.0, 1.0)
        };
        let weight = if weight == 0.0 { 0.0 } else { weight };
        Self {
            from,
            to,
            kind,
            anchor: None,
            weight,
            last_reinforced: now,
            trials: 0,
            interference: 0,
        }
    }

    /// Rehydrate an edge a storage adapter previously persisted. The one way to
    /// reconstruct an edge with its accumulated state from outside this crate —
    /// new edges use [`Edge::new`].
    #[expect(clippy::too_many_arguments)]
    pub fn from_stored(
        from: NodeId,
        to: NodeId,
        kind: EdgeKind,
        anchor: Option<BodySpan>,
        weight: f32,
        last_reinforced: Timestamp,
        trials: u32,
        interference: u32,
    ) -> Self {
        Self {
            from,
            to,
            kind,
            anchor,
            weight,
            last_reinforced,
            trials,
            interference,
        }
    }

    pub fn weight(&self) -> f32 {
        self.weight
    }

    pub fn last_reinforced(&self) -> Timestamp {
        self.last_reinforced
    }

    pub fn trials(&self) -> u32 {
        self.trials
    }

    pub fn interference(&self) -> u32 {
        self.interference
    }

    /// Reject non-canonical persisted state before a compound mutation uses it
    /// as a compare-and-commit input.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.weight.is_finite()
            || !(0.0..=1.0).contains(&self.weight)
            || self.weight == 0.0 && self.weight.is_sign_negative()
        {
            return Err("edge weight must be finite, canonical, and in [0, 1]".into());
        }
        if self.anchor.is_some_and(|anchor| anchor.start > anchor.end) {
            return Err("edge anchor start must not exceed end".into());
        }
        if self.last_reinforced > i64::MAX as Timestamp {
            return Err("edge last_reinforced exceeds the storage integer range".into());
        }
        if self.interference > self.trials {
            return Err("edge interference must not exceed trials".into());
        }
        Ok(())
    }

    /// Whether `other` retains the identity and evidence fields that a decay
    /// pass is not allowed to rewrite. Only weight and pending interference may
    /// change when maintenance consumes misses.
    pub fn same_decay_static_fields(&self, other: &Self) -> bool {
        self.from == other.from
            && self.to == other.to
            && self.kind == other.kind
            && self.anchor == other.anchor
            && self.last_reinforced == other.last_reinforced
            && self.trials == other.trials
    }

    /// Rewrite a donor edge during a full merge without erasing its accumulated
    /// evidence. Any endpoint change invalidates the passage anchor. A rewrite
    /// that would create a self-loop is deliberately absent; callers preserve a
    /// pre-existing winner self-loop separately.
    pub(crate) fn rewrite_for_full_merge(&self, winner: NodeId, loser: NodeId) -> Option<Self> {
        let from = if self.from == loser {
            winner
        } else {
            self.from
        };
        let to = if self.to == loser { winner } else { self.to };
        if from == to {
            return None;
        }
        let mut rewritten = self.clone();
        if from != self.from || to != self.to {
            rewritten.anchor = None;
        }
        rewritten.from = from;
        rewritten.to = to;
        Some(rewritten)
    }

    /// Deterministically fold one donor into an already-existing destination
    /// pair. Static semantics and anchor stay owned by the pre-existing winner
    /// edge. Dynamic evidence is conserved without pretending a merge was a new
    /// successful traversal: weights take an observed maximum, timestamps take
    /// an observed maximum, and raw trial/interference counters saturating-add.
    pub(crate) fn absorb_full_merge_donor(&mut self, donor: &Self) {
        debug_assert_eq!((self.from, self.to), (donor.from, donor.to));
        if donor.weight > self.weight {
            self.weight = donor.weight;
        }
        self.last_reinforced = self.last_reinforced.max(donor.last_reinforced);
        self.trials = self.trials.saturating_add(donor.trials);
        self.interference = self.interference.saturating_add(donor.interference);
    }

    /// Hot-path hit: the edge was tried and led somewhere relevant. Strengthen
    /// `weight` with diminishing returns (`w += (1-w)*gain`). Deterministic and
    /// model-free; safe during a query.
    pub fn reinforce(&mut self, at: Timestamp, p: &StrengthParams) {
        self.weight = (self.weight + (1.0 - self.weight) * p.reinforce_gain).clamp(0.0, 1.0);
        self.trials = self.trials.saturating_add(1);
        self.last_reinforced = at;
    }

    /// Hot-path miss: the edge was tried but carried activation to a node that
    /// turned out irrelevant. Bank one pending interference event for the next
    /// decay pass to charge against `weight`; a cheap counter bump.
    pub fn mark_interference(&mut self) {
        self.trials = self.trials.saturating_add(1);
        self.interference = self.interference.saturating_add(1);
    }

    /// Per-miss weight retention at the current trial count: rises from
    /// [`interference_retention`] toward (but never reaching)
    /// [`interference_resist`] as the edge becomes well-worn. The closer to 1,
    /// the more misses it takes to tear the edge down — sticky, never immortal.
    ///
    /// [`interference_retention`]: StrengthParams::interference_retention
    /// [`interference_resist`]: StrengthParams::interference_resist
    fn retention(&self, p: &StrengthParams) -> f32 {
        let worn = self.trials as f32 / (self.trials as f32 + p.resist_trials_half);
        p.interference_retention + (p.interference_resist - p.interference_retention) * worn
    }

    /// Cold-path decay: erode `weight` by the interference emitted since the last
    /// sweep — `weight *= retention(trials)^interference` — then reset the
    /// counter. No floor: sustained, repeated irrelevance can drive a once-strong
    /// edge to zero; it just costs proportionally more misses the more worn the
    /// edge is. Consumes only emitted signal, so a second call with nothing
    /// emitted in between is a no-op — `decay_sweep` is idempotent.
    pub fn decay(&mut self, p: &StrengthParams) {
        if self.interference == 0 {
            return;
        }
        let retained = self.retention(p).powi(self.interference as i32);
        self.weight = (self.weight * retained).clamp(0.0, 1.0);
        self.interference = 0;
    }
}

/// Only the edges that participate in hot-path traversal live here.
/// Contradiction does NOT — it's a separate cold-path overlay (see below),
/// because spreading activation must never propagate across a conflict.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum EdgeKind {
    /// Logically undirected: the same relationship holds both ways. Stored once
    /// per node pair with a single weight; [`Dir::Both`](ports::Dir::Both)
    /// surfaces it from either endpoint. Co-activation strengthens it (Hebbian).
    Associative,
    /// A long-range association minted by consolidation across two *distant*
    /// clusters — the analogy/insight links. Undirected like `Associative` and
    /// traversed the same way, but tuned to live on a slower clock (reinforced
    /// gently, decayed gently) so a rarely-co-activated but genuine connection
    /// gets time to prove itself. When community detection later finds both
    /// endpoints in the *same* cluster, the bridge has done its job and is
    /// reclassified `Associative` (graduating to the faster intra-cluster
    /// dynamics).
    Bridge,
    /// A grounded navigation step learned from explicit `prior -> target`
    /// feedback. Unlike a conceptual association, this is traversed only in the
    /// observed forward direction. Reverse relevance must earn its own edge;
    /// otherwise a target retrieved for an unrelated query can walk backward
    /// through a learned star and manufacture false context.
    Transition,
    /// Structurally directional: newer analysis replaces older.
    Supersedes,
    /// Structurally directional: derived analysis -> its sources.
    DerivedFrom,
}

impl EdgeKind {
    /// Whether the relationship is logically undirected — surfaced from both
    /// endpoints with lazy reverse-edge materialization. `Associative` and
    /// `Bridge` are; the structural kinds (`Supersedes`, `DerivedFrom`) point one
    /// way only. Centralizes the rule the store adapters traverse by.
    pub fn is_undirected(self) -> bool {
        matches!(self, EdgeKind::Associative | EdgeKind::Bridge)
    }

    /// Whether hot-path traversal follows an edge from its stored `from` endpoint.
    pub fn traverses_outgoing(self) -> bool {
        self.is_undirected() || matches!(self, EdgeKind::Transition)
    }

    /// Whether hot-path traversal follows an edge backward from its stored `to`
    /// endpoint. Structural arrows intentionally expose their source from the
    /// target; learned transitions do not.
    pub fn traverses_incoming(self) -> bool {
        !matches!(self, EdgeKind::Transition)
    }
}

/// A cross-database association: a `from` node in *this* db points at a `target`
/// node in **another** db, named by that db's stamped [`Ulid`] db-id. Kept in a
/// separate overlay, out of the hot-path edge relation, because resolving it
/// means loading a different database — a host concern, done **opportunistically**:
/// if the target db isn't loaded the edge simply doesn't contribute (fallible).
/// Direction (user -> project) is enforced at the creation layer, so a project db
/// never accrues remote edges and stays self-contained and shippable.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RemoteEdge {
    pub from: NodeId,
    pub target_db: Ulid,
    pub target: NodeId,
    weight: f32,
}

impl RemoteEdge {
    pub fn new(from: NodeId, target_db: Ulid, target: NodeId, weight: f32) -> Self {
        // Keep the private field finite and canonical even for callers that
        // bypass a frontend validator. Infinities saturate at the appropriate
        // endpoint; NaN carries no useful ordering information and becomes the
        // weakest possible association. Hosts should still reject non-finite
        // user input rather than silently relying on this last line of defence.
        let weight = if weight.is_nan() {
            0.0
        } else {
            weight.clamp(0.0, 1.0)
        };
        let weight = if weight == 0.0 { 0.0 } else { weight };
        Self {
            from,
            target_db,
            target,
            weight,
        }
    }

    /// Association strength in `[0, 1]`; orders the see-also list at resolution.
    pub fn weight(&self) -> f32 {
        self.weight
    }

    /// Validate persisted/deserialized state before an adapter admits it.
    /// Ordinary values minted by [`RemoteEdge::new`] always satisfy this.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.weight.is_finite()
            || !(0.0..=1.0).contains(&self.weight)
            || self.weight == 0.0 && self.weight.is_sign_negative()
        {
            return Err("remote edge weight must be finite, canonical, and in [0, 1]".into());
        }
        Ok(())
    }

    /// Validate the edge in the context of the database that owns `from`.
    /// Cross-database edges must name a real, different target database; node
    /// identifiers remain opaque here and may therefore be nil.
    pub fn validate_for_source_database(&self, local_db: Ulid) -> std::result::Result<(), String> {
        self.validate()?;
        if self.target_db == Ulid::nil() {
            return Err("remote edge target database must not be nil".into());
        }
        if self.target_db == local_db {
            return Err("remote edge target database must differ from its source database".into());
        }
        Ok(())
    }
}

/// Compare remote edges in their one canonical presentation order: strongest
/// first, then database id and node id ascending. `f32::total_cmp` is safe here
/// because [`RemoteEdge`] keeps weights finite and canonicalizes signed zero.
pub fn remote_edge_order(a: &RemoteEdge, b: &RemoteEdge) -> std::cmp::Ordering {
    b.weight()
        .total_cmp(&a.weight())
        .then_with(|| a.target_db.cmp(&b.target_db))
        .then_with(|| a.target.cmp(&b.target))
}

/// Opaque keyset position for [`RemoteEdgePage`]. The source id is part of the
/// cursor so accidentally replaying it against another node fails closed. The
/// exact finite `f32` bits avoid decimal round-trips changing page boundaries.
///
/// Cursors are best-effort across concurrent mutation in v1: changing an
/// edge's weight moves it in the order and can therefore cause a duplicate or
/// omission across pages. Within an unchanged source adjacency, traversal is
/// exact and deterministic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEdgeCursor {
    from: NodeId,
    weight_bits: u32,
    target_db: Ulid,
    target: NodeId,
}

impl RemoteEdgeCursor {
    pub fn from_edge(edge: &RemoteEdge) -> Self {
        Self {
            from: edge.from,
            weight_bits: edge.weight().to_bits(),
            target_db: edge.target_db,
            target: edge.target,
        }
    }

    pub fn source(&self) -> NodeId {
        self.from
    }

    pub fn weight(&self) -> f32 {
        f32::from_bits(self.weight_bits)
    }

    pub fn weight_bits(&self) -> u32 {
        self.weight_bits
    }

    pub fn target_db(&self) -> Ulid {
        self.target_db
    }

    pub fn target(&self) -> NodeId {
        self.target
    }

    /// Check source binding and reject forged/corrupt non-finite positions.
    pub fn validate_for(&self, from: NodeId) -> std::result::Result<(), String> {
        if self.from != from {
            return Err("remote edge cursor belongs to a different source".into());
        }
        let weight = self.weight();
        if !weight.is_finite()
            || !(0.0..=1.0).contains(&weight)
            || weight == 0.0 && weight.is_sign_negative()
        {
            return Err("remote edge cursor contains a non-canonical weight".into());
        }
        Ok(())
    }
}

/// One bounded page of remote edges. `next` is present only when the adapter
/// observed at least one further row, and points at the final returned item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteEdgePage {
    pub items: Vec<RemoteEdge>,
    pub next: Option<RemoteEdgeCursor>,
}

/// Contradiction is symmetric and cold-path, so it's stored OUTSIDE the edge
/// relation, keyed by a canonical (unordered) node pair. We deliberately key
/// by node-pair and NOT cluster-pair: communities get recomputed, so any
/// cluster-keyed state goes stale the instant community detection re-runs.
/// The "which clusters are in conflict" view is DERIVED at triage time by
/// aggregating these counts through *current* membership — never stored.
///
/// The unordered pair is enforced by the type ([`UnorderedPair`]), not by a
/// hand-maintained `lo < hi` invariant: `(a,b)` and `(b,a)` hash and compare
/// equal, so they collapse to one row for free.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Contradiction {
    pub between: UnorderedPair<NodeId>,
    /// Increments each time a retrieval pass observes these two as conflicting.
    /// This is the cheap signal; reconciliation triages on the aggregate.
    pub observations: u32,
    pub first_seen: Timestamp,
    pub last_seen: Timestamp,
    /// `None` and `Some(Unresolved)` remain open for later adjudication.
    /// `Superseded` and `ContextDependent` are terminal decisions.
    pub resolution: Option<Resolution>,
}

impl Contradiction {
    pub fn new(a: NodeId, b: NodeId, now: Timestamp) -> Self {
        Self {
            between: UnorderedPair(a, b),
            observations: 1,
            first_seen: now,
            last_seen: now,
            resolution: None,
        }
    }

    /// Another retrieval pass saw this pair conflict.
    pub fn observe(&mut self, now: Timestamp) {
        self.observations = self.observations.saturating_add(1);
        self.last_seen = now;
    }

    pub fn resolve(&mut self, resolution: Resolution) {
        self.resolution = Some(resolution);
    }

    /// Still awaiting adjudication — what the reconciliation pass triages on.
    /// An explicit `Unresolved` verdict records that a reviewer deferred the
    /// decision; it does not make that deferral irreversible.
    pub fn is_open(&self) -> bool {
        matches!(self.resolution, None | Some(Resolution::Unresolved))
    }

    /// Reject persisted overlay state that cannot represent an observation
    /// history in the storage integer domain.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.between.0 == self.between.1 {
            return Err("contradiction endpoints must differ".into());
        }
        if self.observations == 0 {
            return Err("contradiction observations must be nonzero".into());
        }
        if self.first_seen > i64::MAX as Timestamp || self.last_seen > i64::MAX as Timestamp {
            return Err("contradiction timestamp exceeds the storage integer range".into());
        }
        if self.first_seen > self.last_seen {
            return Err("contradiction first_seen must not exceed last_seen".into());
        }
        Ok(())
    }
}

/// Feedback an agent emits about a node it just loaded, *relative to the node it
/// arrived from*. The strong, explicit form of the design's retrieval signals —
/// what actually drives edge weight. (Topology can't supply it: in spreading
/// activation a followed edge always lands on a node that surfaces, so "tried"
/// alone never distinguishes relevant from irrelevant; the agent has to say.)
/// `Contradicts` is deliberately absent — it needs inference and has its own
/// reconciliation path ([`Contradiction`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Signal {
    /// Relevant and new: strengthen the edge that led here, revive the node.
    RelevantNew,
    /// Relevant but redundant with what's already loaded: also strengthens the
    /// edge (they clearly belong together) but banks a [`MergeCandidate`].
    NotNew,
    /// Irrelevant: weaken the edge that led here, and the node.
    Irrelevant,
}

/// "These two nodes keep duplicating each other." Accrued from repeated
/// [`Signal::NotNew`] feedback along a shared edge and, exactly like
/// [`Contradiction`], stored as a cold-path overlay keyed by an unordered pair
/// and left for a later pass to adjudicate — never resolved inline. The persisted
/// verdict and its execution are separate; the legacy `Partial` verdict remains
/// readable even though no partial-merge writer ships.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeCandidate {
    pub between: UnorderedPair<NodeId>,
    /// Bumped each time the pair is flagged not-new; the merge pass triages on it.
    pub observations: u32,
    pub first_seen: Timestamp,
    pub last_seen: Timestamp,
    /// Set once the merge pass has decided, so we don't re-litigate.
    pub resolution: Option<MergeResolution>,
}

impl MergeCandidate {
    pub fn new(a: NodeId, b: NodeId, now: Timestamp) -> Self {
        Self {
            between: UnorderedPair(a, b),
            observations: 1,
            first_seen: now,
            last_seen: now,
            resolution: None,
        }
    }

    /// Another not-new observation for this pair.
    pub fn observe(&mut self, now: Timestamp) {
        self.observations = self.observations.saturating_add(1);
        self.last_seen = now;
    }

    pub fn resolve(&mut self, resolution: MergeResolution) {
        self.resolution = Some(resolution);
    }

    /// Still awaiting the merge pass — what triage lists.
    pub fn is_open(&self) -> bool {
        self.resolution.is_none()
    }

    /// Reject persisted overlay state that cannot represent an observation
    /// history in the storage integer domain.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.between.0 == self.between.1 {
            return Err("merge candidate endpoints must differ".into());
        }
        if self.observations == 0 {
            return Err("merge candidate observations must be nonzero".into());
        }
        if self.first_seen > i64::MAX as Timestamp || self.last_seen > i64::MAX as Timestamp {
            return Err("merge candidate timestamp exceeds the storage integer range".into());
        }
        if self.first_seen > self.last_seen {
            return Err("merge candidate first_seen must not exceed last_seen".into());
        }
        Ok(())
    }
}

/// Durable proof that one ordered full-collapse decision committed for a
/// canonical merge-candidate pair. The unordered pair is the idempotency key;
/// `winner`/`loser` are its payload, so retrying the opposite direction is a
/// conflict rather than a second merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullMergeRecord {
    pub between: UnorderedPair<NodeId>,
    pub winner: NodeId,
    pub loser: NodeId,
    pub applied_at: Timestamp,
}

impl FullMergeRecord {
    pub fn new(winner: NodeId, loser: NodeId, applied_at: Timestamp) -> Self {
        Self {
            between: UnorderedPair(winner, loser),
            winner,
            loser,
            applied_at,
        }
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.winner == self.loser {
            return Err("full merge winner and loser must differ".into());
        }
        if self.between != UnorderedPair(self.winner, self.loser) {
            return Err("full merge record pair does not match its payload".into());
        }
        if self.applied_at > i64::MAX as Timestamp {
            return Err("full merge timestamp exceeds the storage integer range".into());
        }
        Ok(())
    }
}

/// Durable proof that one ordered supersession decision committed for a
/// canonical contradiction pair. The unordered pair is the lifetime event
/// identity; `winner`/`loser` retain direction so retrying the inverse decision
/// is a conflict rather than a second confidence decay. This records historical
/// adjudication, not a standing graph invariant: later legal lifecycle or edge
/// mutations are not repaired by replaying the same pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupersedeRecord {
    pub between: UnorderedPair<NodeId>,
    pub winner: NodeId,
    pub loser: NodeId,
    pub applied_at: Timestamp,
}

impl SupersedeRecord {
    pub fn new(winner: NodeId, loser: NodeId, applied_at: Timestamp) -> Self {
        Self {
            between: UnorderedPair(winner, loser),
            winner,
            loser,
            applied_at,
        }
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.winner == self.loser {
            return Err("supersede winner and loser must differ".into());
        }
        if self.between != UnorderedPair(self.winner, self.loser) {
            return Err("supersede record pair does not match its payload".into());
        }
        if self.applied_at > i64::MAX as Timestamp {
            return Err("supersede timestamp exceeds the storage integer range".into());
        }
        Ok(())
    }
}

/// The merge pass's persisted verdict for a pair. Full collapse has a separate
/// acting primitive; recording a verdict here just closes the candidacy. `Partial`
/// remains representable for historical stores, but has no acting writer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum MergeResolution {
    /// Near-complete redundancy: collapse one node into the other.
    Full,
    /// Historical overlap verdict. A future typed operation may atomically create
    /// a shared child and both derivations; the legacy writer has been removed.
    Partial,
    /// Distinct enough after all — keep both, stop flagging the pair.
    Keep,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Resolution {
    /// Real conflict; one side superseded the other — emit a Supersedes edge
    /// and decay the loser.
    Superseded,
    /// Not actually a conflict: both true, different contexts. Suppress future
    /// flagging of this pair.
    ContextDependent,
    /// Explicit deferral. This remains open for a human / the main agent and may
    /// later transition to either terminal verdict.
    Unresolved,
}

/// Opaque pointer revision for optimistic body replacement. This is not a
/// content hash, authorization token, clock, or history/ABA guarantee.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BodyRevision(String);

impl BodyRevision {
    pub fn parse(value: &str) -> ports::Result<Self> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ports::Error::InvalidInput(
                "body revision must be 64 lowercase hex digits".into(),
            ));
        }
        Ok(Self(value.to_owned()))
    }
    pub fn to_hex(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for BodyRevision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::str::FromStr for BodyRevision {
    type Err = ports::Error;
    fn from_str(value: &str) -> ports::Result<Self> {
        Self::parse(value)
    }
}

/// A byte range into a node's resolved body — the anchor of a passage-level
/// edge ("*this span* of `from`'s body is what relates to `to`").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodySpan {
    pub start: u32,
    pub end: u32,
}

impl BodySpan {
    pub fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    /// The slice of `body` this span covers, clamped to the body's bounds (so a
    /// stale span can't panic).
    pub fn slice<'a>(&self, body: &'a [u8]) -> &'a [u8] {
        let start = (self.start as usize).min(body.len());
        let end = (self.end as usize).clamp(start, body.len());
        &body[start..end]
    }
}

/// Maximum UTF-8 byte length of a canonical body URI.
pub const MAX_BODY_REF_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BodyRefValidationError {
    #[error("body reference is {actual} UTF-8 bytes; maximum is {MAX_BODY_REF_BYTES}")]
    TooLong { actual: usize },
    #[error("body reference must have a valid ASCII URI scheme followed by ://")]
    InvalidScheme,
    #[error("body reference must have a nonempty remainder after ://")]
    EmptyRemainder,
    #[error("body reference must not contain control characters")]
    ControlCharacter,
}

/// A reclaimable, bounded URI whose scheme selects the [`ports::BodyStore`]
/// implementation: `fs://`, `https://`, `inline://`, ... . Equal references
/// compare and hash by text while clones share the same allocation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BodyRef(Arc<str>);

impl BodyRef {
    pub fn new(uri: impl AsRef<str>) -> std::result::Result<Self, BodyRefValidationError> {
        let uri = uri.as_ref();
        validate_body_ref(uri)?;
        Ok(Self(Arc::from(uri)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn scheme(&self) -> &str {
        self.0
            .split_once("://")
            .map(|(scheme, _)| scheme)
            .expect("BodyRef construction validates its URI scheme")
    }

    #[cfg(test)]
    fn shared_text(&self) -> &Arc<str> {
        &self.0
    }
}

fn validate_body_ref(uri: &str) -> std::result::Result<(), BodyRefValidationError> {
    if uri.len() > MAX_BODY_REF_BYTES {
        return Err(BodyRefValidationError::TooLong { actual: uri.len() });
    }
    if uri.chars().any(char::is_control) {
        return Err(BodyRefValidationError::ControlCharacter);
    }
    let (scheme, remainder) = uri
        .split_once("://")
        .ok_or(BodyRefValidationError::InvalidScheme)?;
    let mut scheme_bytes = scheme.bytes();
    if !scheme_bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic())
        || !scheme_bytes
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return Err(BodyRefValidationError::InvalidScheme);
    }
    if remainder.is_empty() {
        return Err(BodyRefValidationError::EmptyRemainder);
    }
    Ok(())
}

impl TryFrom<&str> for BodyRef {
    type Error = BodyRefValidationError;

    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for BodyRef {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BodyRef {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = BodyRef;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "a body URI of at most {MAX_BODY_REF_BYTES} UTF-8 bytes"
                )
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                BodyRef::new(value).map_err(E::custom)
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                BodyRef::new(value).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                BodyRef::new(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ulid::Ulid;

    #[test]
    fn embedding_fingerprint_is_versioned_and_round_trips() {
        let fingerprint =
            EmbeddingFingerprint::new("provider:model@revision", 384, "l2-f32-v1", "symmetric-v1");
        fingerprint.validate().unwrap();
        let json = serde_json::to_string(&fingerprint).unwrap();
        let loaded: EmbeddingFingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, fingerprint);

        let mut future = fingerprint;
        future.format_version += 1;
        assert!(
            future
                .validate()
                .unwrap_err()
                .contains("unsupported format")
        );
    }

    #[test]
    fn feedback_retry_identity_is_canonical_and_storage_bounded() {
        let retry = ports::FeedbackRetryScope::new("epoch", 2, 1).unwrap();
        ports::FeedbackIdempotency::new("key", "0".repeat(64), retry.clone()).unwrap();
        assert!(ports::FeedbackIdempotency::new("key", "A".repeat(64), retry).is_err());
        assert!(ports::FeedbackRetryScope::new("epoch", 0, 0).is_err());
        assert!(ports::FeedbackRetryScope::new("epoch", 1, 2).is_err());
        assert!(
            ports::FeedbackRetryScope::new("epoch", i64::MAX as u64, 1).is_err(),
            "one representable successor is reserved for checked allocation"
        );
    }

    fn a_node() -> Node {
        Node::try_new(
            NodeId(Ulid::new()),
            "s",
            BodyRef::new("inline://x").unwrap(),
            ["t"],
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap()
    }

    #[test]
    fn node_summaries_are_bounded_nonblank_and_reclaimable() {
        for blank in ["", " ", "\n\t", "\u{2003}\u{2009}"] {
            assert_eq!(
                NodeSummary::new(blank).unwrap_err(),
                NodeSummaryValidationError::Blank
            );
            assert!(serde_json::from_value::<NodeSummary>(serde_json::json!(blank)).is_err());
        }

        let exact = "a".repeat(MAX_NODE_SUMMARY_BYTES);
        assert_eq!(NodeSummary::new(&exact).unwrap().as_str(), exact);
        assert_eq!(
            NodeSummary::new(format!("{exact}a")).unwrap_err(),
            NodeSummaryValidationError::TooLong {
                actual: MAX_NODE_SUMMARY_BYTES + 1
            }
        );
        assert!(matches!(
            NodeSummary::new(" ".repeat(MAX_NODE_SUMMARY_BYTES + 1)),
            Err(NodeSummaryValidationError::TooLong { .. })
        ));

        let unicode_exact = "é".repeat(MAX_NODE_SUMMARY_BYTES / "é".len());
        assert_eq!(unicode_exact.len(), MAX_NODE_SUMMARY_BYTES);
        assert!(NodeSummary::new(&unicode_exact).is_ok());
        assert!(matches!(
            NodeSummary::new(format!("{unicode_exact}a")),
            Err(NodeSummaryValidationError::TooLong { .. })
        ));

        let original = NodeSummary::new("shared summary").unwrap();
        let weak = Arc::downgrade(&original.0);
        let cloned = original.clone();
        assert!(Arc::ptr_eq(&original.0, &cloned.0));
        assert_eq!(
            serde_json::to_string(&original).unwrap(),
            "\"shared summary\""
        );
        assert_eq!(
            serde_json::from_str::<NodeSummary>("\"shared summary\"")
                .unwrap()
                .as_str(),
            "shared summary"
        );
        let independently_allocated = NodeSummary::new("shared summary").unwrap();
        assert!(!Arc::ptr_eq(&original.0, &independently_allocated.0));
        drop(original);
        assert!(weak.upgrade().is_some());
        drop(cloned);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn derived_sources_are_ordered_unique_bounded_and_reclaimable() {
        let ids = [3_u128, 1, 2].map(|value| NodeId(Ulid::from(value)));
        let sources = DerivedSources::try_from_iter(ids).unwrap();
        assert_eq!(sources.as_slice(), &ids);
        assert_eq!(sources.iter().copied().collect::<Vec<_>>(), ids);
        assert_eq!(
            serde_json::to_value(&sources).unwrap(),
            serde_json::to_value(ids).unwrap()
        );
        let decoded: DerivedSources =
            serde_json::from_value(serde_json::to_value(ids).unwrap()).unwrap();
        assert_eq!(decoded.as_slice(), &ids);

        assert_eq!(
            DerivedSources::try_from_iter([ids[0], ids[0]]).unwrap_err(),
            DerivedSourcesValidationError::Duplicate { node: ids[0] }
        );
        assert!(
            serde_json::from_value::<DerivedSources>(serde_json::json!([ids[0], ids[0]])).is_err()
        );

        let exact = (0..MAX_DERIVED_SOURCES)
            .map(|index| NodeId(Ulid::from(index as u128 + 1)))
            .collect::<Vec<_>>();
        assert_eq!(
            DerivedSources::try_from_iter(exact.iter().copied())
                .unwrap()
                .len(),
            MAX_DERIVED_SOURCES
        );
        let too_many = (0..=MAX_DERIVED_SOURCES)
            .map(|index| NodeId(Ulid::from(index as u128 + 1)))
            .collect::<Vec<_>>();
        assert_eq!(
            DerivedSources::try_from_iter(too_many.iter().copied()).unwrap_err(),
            DerivedSourcesValidationError::TooMany
        );
        assert!(serde_json::from_value::<DerivedSources>(serde_json::json!(too_many)).is_err());

        struct HostileSizeHint {
            next: u128,
            calls: std::rc::Rc<std::cell::Cell<usize>>,
        }

        impl Iterator for HostileSizeHint {
            type Item = NodeId;

            fn next(&mut self) -> Option<Self::Item> {
                self.calls.set(self.calls.get() + 1);
                let next = self.next;
                self.next += 1;
                Some(NodeId(Ulid::from(next)))
            }

            fn size_hint(&self) -> (usize, Option<usize>) {
                (usize::MAX, None)
            }
        }

        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let hostile = HostileSizeHint {
            next: 1,
            calls: calls.clone(),
        };
        assert_eq!(
            DerivedSources::try_from_iter(hostile).unwrap_err(),
            DerivedSourcesValidationError::TooMany
        );
        assert_eq!(calls.get(), MAX_DERIVED_SOURCES + 1);

        let original = DerivedSources::try_from_iter(ids).unwrap();
        let weak = Arc::downgrade(&original.0);
        let cloned = original.clone();
        assert!(Arc::ptr_eq(&original.0, &cloned.0));
        drop(original);
        assert!(weak.upgrade().is_some());
        drop(cloned);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn node_scalars_are_finite_unit_intervals_with_number_wire_shape() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<Stability>();
        assert_copy::<Confidence>();

        for value in [0.0_f32, -0.0, 0.5, 1.0] {
            assert_eq!(Stability::new(value).unwrap().to_bits(), value.to_bits());
            assert_eq!(Confidence::new(value).unwrap().to_bits(), value.to_bits());
        }
        for invalid in [
            -f32::EPSILON,
            1.0 + f32::EPSILON,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            assert!(Stability::new(invalid).is_err());
            assert!(Confidence::new(invalid).is_err());
        }
        assert!(
            StabilityValidationError
                .to_string()
                .starts_with("stability")
        );
        assert!(
            ConfidenceValidationError
                .to_string()
                .starts_with("confidence")
        );

        let stability = Stability::new(0.25).unwrap();
        let confidence = Confidence::new(0.75).unwrap();
        assert_eq!(
            serde_json::to_value(stability).unwrap(),
            serde_json::json!(0.25)
        );
        assert_eq!(
            serde_json::to_value(confidence).unwrap(),
            serde_json::json!(0.75)
        );
        assert_eq!(
            serde_json::from_value::<Stability>(serde_json::json!(0.25))
                .unwrap()
                .get(),
            0.25
        );
        assert_eq!(
            serde_json::from_value::<Confidence>(serde_json::json!(0.75))
                .unwrap()
                .get(),
            0.75
        );
        assert!(serde_json::from_value::<Stability>(serde_json::json!(-0.1)).is_err());
        assert!(serde_json::from_value::<Confidence>(serde_json::json!(1.1)).is_err());
        for logically_outside_before_f32_rounding in ["-1e-50", "1.0000000000000002"] {
            assert!(
                serde_json::from_str::<Stability>(logically_outside_before_f32_rounding).is_err(),
                "stability accepted {logically_outside_before_f32_rounding}"
            );
            assert!(
                serde_json::from_str::<Confidence>(logically_outside_before_f32_rounding).is_err(),
                "confidence accepted {logically_outside_before_f32_rounding}"
            );
        }

        let negative_zero = Stability::new(-0.0).unwrap();
        let round_trip: Stability =
            serde_json::from_str(&serde_json::to_string(&negative_zero).unwrap()).unwrap();
        assert_eq!(round_trip.to_bits(), (-0.0_f32).to_bits());
    }

    #[test]
    fn body_refs_are_bounded_canonical_and_reclaimable() {
        let exact = format!("x://{}", "a".repeat(MAX_BODY_REF_BYTES - 4));
        assert_eq!(exact.len(), MAX_BODY_REF_BYTES);
        assert_eq!(BodyRef::new(&exact).unwrap().as_str(), exact);
        assert!(matches!(
            BodyRef::new(format!("{exact}a")),
            Err(BodyRefValidationError::TooLong { .. })
        ));

        let unicode_exact = format!(
            "x://{}",
            "é".repeat((MAX_BODY_REF_BYTES - "x://".len()) / "é".len())
        );
        assert_eq!(unicode_exact.len(), MAX_BODY_REF_BYTES);
        assert!(BodyRef::new(&unicode_exact).is_ok());
        assert!(matches!(
            BodyRef::new(format!("{unicode_exact}a")),
            Err(BodyRefValidationError::TooLong { .. })
        ));

        for invalid in [
            "://value",
            "1bad://value",
            "bad_scheme://value",
            "missing:/value",
            "empty://",
            "inline://bad\nvalue",
            "inline://bad\0value",
        ] {
            assert!(BodyRef::new(invalid).is_err(), "accepted {invalid:?}");
            assert!(serde_json::from_value::<BodyRef>(serde_json::json!(invalid)).is_err());
        }

        let original = BodyRef::new("inline://shared").unwrap();
        let weak = Arc::downgrade(original.shared_text());
        let cloned = original.clone();
        assert!(Arc::ptr_eq(original.shared_text(), cloned.shared_text()));
        drop(original);
        assert!(weak.upgrade().is_some());
        drop(cloned);
        assert!(weak.upgrade().is_none());

        let first = BodyRef::new("inline://same-text").unwrap();
        let independently_allocated = BodyRef::new("inline://same-text").unwrap();
        assert!(!Arc::ptr_eq(
            first.shared_text(),
            independently_allocated.shared_text()
        ));
        let mut map = std::collections::HashMap::new();
        map.insert(first, 7_u8);
        assert_eq!(map.get(&independently_allocated), Some(&7));
    }

    #[test]
    fn web_urls_are_bounded_http_urls_and_reclaimable() {
        let prefix = "https://example.test/";
        let exact = format!("{prefix}{}", "a".repeat(MAX_WEB_URL_BYTES - prefix.len()));
        assert_eq!(exact.len(), MAX_WEB_URL_BYTES);
        assert_eq!(WebUrl::new(&exact).unwrap().as_str(), exact);
        assert!(matches!(
            WebUrl::new(format!("{exact}a")),
            Err(WebUrlValidationError::TooLong { .. })
        ));
        assert!(serde_json::from_value::<WebUrl>(serde_json::json!(format!("{exact}a"))).is_err());
        let unicode_prefix = "https://example.test/a";
        let unicode_exact = format!(
            "{unicode_prefix}{}",
            "é".repeat((MAX_WEB_URL_BYTES - unicode_prefix.len()) / "é".len())
        );
        assert_eq!(unicode_exact.len(), MAX_WEB_URL_BYTES);
        assert!(WebUrl::new(&unicode_exact).is_ok());
        assert!(WebUrl::new(format!("{unicode_exact}a")).is_err());
        for invalid in [
            "",
            "https://",
            "http://",
            "https:///",
            "https:///path",
            "https://?query",
            "https://#fragment",
            "https:// ",
            "https://example.test/a b",
            "ftp://example.test",
            "HTTPS://example.test",
            "https://bad\nvalue",
            "https://bad\0value",
            "https://example.test:not-a-port/",
            "https://example.test:65536/",
            "https://[2001:db8::1/",
            "https://2001:db8::1/",
        ] {
            assert!(WebUrl::new(invalid).is_err(), "accepted {invalid:?}");
            assert!(serde_json::from_value::<WebUrl>(serde_json::json!(invalid)).is_err());
        }

        for valid in [
            "https://example.test",
            "https://example.test/path?query=value#fragment",
            "http://127.0.0.1:8080/path",
            "https://[2001:db8::1]:443/path",
            "https://例え.テスト/道?値=一#片",
        ] {
            assert_eq!(WebUrl::new(valid).unwrap().as_str(), valid);
        }

        let wire_text = "https://Example.COM/a%2Fb?x=%2f#Frag";
        let preserved = WebUrl::new(wire_text).unwrap();
        assert_eq!(preserved.as_str(), wire_text);
        let encoded = serde_json::to_string(&preserved).unwrap();
        assert_eq!(encoded, format!("\"{wire_text}\""));
        assert_eq!(
            serde_json::from_str::<WebUrl>(&encoded).unwrap().as_str(),
            wire_text
        );

        let original = WebUrl::new("https://example.test/path").unwrap();
        let weak = Arc::downgrade(original.shared_text());
        let cloned = original.clone();
        assert!(Arc::ptr_eq(original.shared_text(), cloned.shared_text()));
        drop(original);
        assert!(weak.upgrade().is_some());
        drop(cloned);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn origin_commits_are_full_lowercase_sha1_or_sha256() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<OriginCommit>();

        let sha1_text = "0123456789abcdef0123456789abcdef01234567";
        let sha256_text = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let sha1 = OriginCommit::parse(sha1_text).unwrap();
        let sha256 = OriginCommit::parse(sha256_text).unwrap();
        assert_eq!(sha1.algorithm(), "sha1");
        assert_eq!(sha256.algorithm(), "sha256");
        assert_eq!(sha1.to_string(), sha1_text);
        assert_eq!(sha256.to_string(), sha256_text);
        assert_eq!(
            serde_json::from_str::<OriginCommit>(&serde_json::to_string(&sha1).unwrap()).unwrap(),
            sha1
        );
        assert_eq!(
            serde_json::from_str::<OriginCommit>(&serde_json::to_string(&sha256).unwrap()).unwrap(),
            sha256
        );

        for invalid in [
            "deadbeef",
            "0123456789abcdef0123456789abcdef0123456G",
            "0123456789ABCDEF0123456789ABCDEF01234567",
            "0123456789abcdef0123456789abcdef012345678",
        ] {
            assert!(
                OriginCommit::parse(invalid).is_err(),
                "accepted {invalid:?}"
            );
            assert!(serde_json::from_value::<OriginCommit>(serde_json::json!(invalid)).is_err());
        }
    }

    #[test]
    fn node_text_metadata_keeps_its_string_wire_shape_and_rejects_oversized_snapshots() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let node = Node::new(NodeInit {
            id: NodeId(Ulid::from(1_u128)),
            summary: NodeSummary::new("summary").unwrap(),
            body: BodyRef::new("inline://body").unwrap(),
            tags: BoundedTagSet::try_from_iter(["tag"]).unwrap(),
            provenance: Provenance::Web {
                url: WebUrl::new("https://example.test/source").unwrap(),
                fetched: 7,
            },
            stability: Stability::new(0.5).unwrap(),
            confidence: Confidence::new(0.75).unwrap(),
            status: NodeStatus::Active,
            created: 3,
        })
        .with_origin_commit(Some(OriginCommit::parse(sha).unwrap()));
        node.validate().unwrap();

        let encoded = serde_json::to_string(&node).unwrap();
        assert_eq!(
            encoded,
            r#"{"id":"00000000000000000000000001","summary":"summary","body":"inline://body","body_ownership":"borrowed","tags":["tag"],"provenance":{"Web":{"url":"https://example.test/source","fetched":7}},"origin_commit":"0123456789abcdef0123456789abcdef01234567","stability":0.5,"confidence":0.75,"created":3,"last_exposed":null,"exposure_count":0,"last_grounded_use":null,"grounded_use_count":0,"interference":0,"status":"Active"}"#
        );
        let golden = serde_json::from_str::<serde_json::Value>(&encoded).unwrap();
        assert_eq!(golden["body"], serde_json::json!("inline://body"));
        assert_eq!(
            golden["provenance"],
            serde_json::json!({
                "Web": {
                    "url": "https://example.test/source",
                    "fetched": 7
                }
            })
        );
        assert_eq!(golden["origin_commit"], serde_json::json!(sha));
        let decoded: Node = serde_json::from_value(golden.clone()).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded.id(), node.id());
        assert_eq!(decoded.summary(), "summary");
        assert_eq!(decoded.stability().to_bits(), 0.5_f32.to_bits());
        assert_eq!(decoded.confidence().to_bits(), 0.75_f32.to_bits());
        assert_eq!(decoded.body().as_str(), "inline://body");
        assert_eq!(decoded.origin_commit(), node.origin_commit());

        let mut oversized = golden;
        oversized["body"] = serde_json::json!(format!(
            "x://{}",
            "a".repeat(MAX_BODY_REF_BYTES - "x://".len() + 1)
        ));
        assert!(serde_json::from_value::<Node>(oversized).is_err());
    }

    #[test]
    fn raw_node_construction_and_snapshot_decode_reject_invalid_fields() {
        let id = NodeId(Ulid::from(9_u128));
        let body = || BodyRef::new("inline://body").unwrap();

        assert!(matches!(
            Node::try_new(
                id,
                "  ",
                body(),
                ["tag"],
                Provenance::derived_empty(),
                0.5,
                0.5,
                NodeStatus::Active,
                1,
            ),
            Err(NodeValidationError::Summary(
                NodeSummaryValidationError::Blank
            ))
        ));
        assert!(matches!(
            Node::try_new(
                id,
                "s",
                body(),
                ["tag", "tag"],
                Provenance::derived_empty(),
                0.5,
                0.5,
                NodeStatus::Active,
                1,
            ),
            Err(NodeValidationError::Tags(
                TagValidationError::Duplicate { .. }
            ))
        ));
        assert!(matches!(
            Node::try_new(
                id,
                "s",
                body(),
                ["tag"],
                Provenance::derived_empty(),
                f32::NAN,
                0.5,
                NodeStatus::Active,
                1,
            ),
            Err(NodeValidationError::Stability(_))
        ));
        assert!(matches!(
            Node::try_new(
                id,
                "s",
                body(),
                ["tag"],
                Provenance::derived_empty(),
                0.5,
                f32::INFINITY,
                NodeStatus::Active,
                1,
            ),
            Err(NodeValidationError::Confidence(_))
        ));

        let source = NodeId(Ulid::from(10_u128));
        assert!(matches!(
            Provenance::derived([source, source]),
            Err(DerivedSourcesValidationError::Duplicate { node }) if node == source
        ));

        let valid = serde_json::to_value(a_node()).unwrap();
        let rejects_field = |field: &str, value: serde_json::Value| {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(
                serde_json::from_value::<Node>(invalid).is_err(),
                "snapshot accepted invalid {field}"
            );
        };
        rejects_field("summary", serde_json::json!("\n\t"));
        rejects_field(
            "summary",
            serde_json::json!("s".repeat(MAX_NODE_SUMMARY_BYTES + 1)),
        );
        rejects_field("body", serde_json::json!("not-a-body-ref"));
        rejects_field("tags", serde_json::json!(["duplicate", "duplicate"]));
        rejects_field("stability", serde_json::json!(-1e-50));
        rejects_field("confidence", serde_json::json!(1.0000000000000002));
        rejects_field(
            "provenance",
            serde_json::json!({
                "Web": { "url": "ftp://example.test", "fetched": 1 }
            }),
        );
        rejects_field(
            "provenance",
            serde_json::json!({ "Derived": { "from": [source, source] } }),
        );
        let too_many_sources = (0..=MAX_DERIVED_SOURCES)
            .map(|index| NodeId(Ulid::from(index as u128 + 100)))
            .collect::<Vec<_>>();
        rejects_field(
            "provenance",
            serde_json::json!({ "Derived": { "from": too_many_sources } }),
        );
    }

    #[test]
    fn node_serde_rejects_unknown_fields_at_every_struct_boundary() {
        fn assert_rejected(label: &str, value: serde_json::Value) {
            let error = serde_json::from_value::<Node>(value).unwrap_err();
            assert!(
                error.to_string().contains("unknown field"),
                "{label} failed for the wrong reason: {error}"
            );
        }

        let valid = serde_json::to_value(a_node()).unwrap();
        let mut unknown_node_field = valid.clone();
        unknown_node_field["summmary"] = serde_json::json!("typo");
        assert_rejected("Node", unknown_node_field);

        let provenance_variants = [
            (
                "Provenance::Web",
                serde_json::json!({
                    "Web": {
                        "url": "https://example.test/source",
                        "fetched": 1,
                        "future_field": true
                    }
                }),
            ),
            (
                "Provenance::Conversation",
                serde_json::json!({
                    "Conversation": {
                        "session": Ulid::from(11_u128),
                        "turn": 2,
                        "future_field": true
                    }
                }),
            ),
            (
                "Provenance::Derived",
                serde_json::json!({
                    "Derived": {
                        "from": [],
                        "future_field": true
                    }
                }),
            ),
        ];
        for (label, provenance) in provenance_variants {
            let mut unknown_variant_field = valid.clone();
            unknown_variant_field["provenance"] = provenance;
            assert_rejected(label, unknown_variant_field);
        }

        let mut unknown_candidate_field = valid;
        unknown_candidate_field["status"] = serde_json::json!({
            "Candidate": {
                "use_count": 0,
                "future_field": true
            }
        });
        assert!(serde_json::from_value::<Node>(unknown_candidate_field).is_err());
    }

    #[test]
    fn strict_node_serde_preserves_explicit_legacy_compatibility() {
        let mut legacy = serde_json::to_value(a_node()).unwrap();
        let object = legacy.as_object_mut().unwrap();
        for defaulted in [
            "body_ownership",
            "origin_commit",
            "last_exposed",
            "exposure_count",
            "last_grounded_use",
            "grounded_use_count",
            "interference",
        ] {
            object.remove(defaulted);
        }
        object.insert("last_activated".into(), serde_json::json!(55));
        object.insert("activation_count".into(), serde_json::json!(7));
        object.insert("tags".into(), serde_json::json!(["zeta", "alpha"]));

        let loaded: Node = serde_json::from_value(legacy).unwrap();
        loaded.validate().unwrap();
        assert_eq!(loaded.body_ownership(), BodyOwnership::Borrowed);
        assert_eq!(loaded.origin_commit(), None);
        assert_eq!(loaded.last_exposed(), Some(55));
        assert_eq!(loaded.exposure_count(), 7);
        assert_eq!(loaded.last_grounded_use(), None);
        assert_eq!(loaded.grounded_use_count(), 0);
        assert_eq!(loaded.interference(), 0);
        assert_eq!(loaded.tags().collect::<Vec<_>>(), ["alpha", "zeta"]);

        let canonical = serde_json::to_value(loaded).unwrap();
        let object = canonical.as_object().unwrap();
        assert!(!object.contains_key("last_activated"));
        assert!(!object.contains_key("activation_count"));
        assert_eq!(object["tags"], serde_json::json!(["alpha", "zeta"]));
    }

    #[test]
    fn confidence_scalar_mutations_are_checked_and_failure_is_atomic() {
        let mut node = a_node();
        let original = node.confidence().to_bits();
        assert_eq!(
            node.try_set_confidence(-0.1),
            Err(ConfidenceMutationError::InvalidValue)
        );
        assert_eq!(node.confidence().to_bits(), original);
        node.validate().unwrap();
    }

    #[test]
    fn bounded_tags_keep_the_json_array_shape_and_normalize_order() {
        let node = Node::try_new(
            NodeId(Ulid::from(1u128)),
            "s",
            BodyRef::new("inline://x").unwrap(),
            ["zeta", "alpha"],
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        let encoded = serde_json::to_value(&node).unwrap();
        assert_eq!(encoded["tags"], serde_json::json!(["alpha", "zeta"]));

        let mut old_shape = encoded;
        old_shape["tags"] = serde_json::json!(["zeta", "alpha"]);
        let decoded: Node = serde_json::from_value(old_shape).unwrap();
        assert_eq!(decoded.tags().collect::<Vec<_>>(), ["alpha", "zeta"]);
        assert!(decoded.has_tag("alpha"));
        assert!(!decoded.has_tag("never-intern-this-query-tag"));

        let original = BoundedTagSet::try_from_iter(["alpha", "zeta"]).unwrap();
        let weak = Arc::downgrade(&original.0);
        let cloned = original.clone();
        assert!(Arc::ptr_eq(&original.0, &cloned.0));
        drop(original);
        assert!(weak.upgrade().is_some());
        drop(cloned);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn bounded_tags_reject_invalid_construction_and_snapshot_data() {
        assert_eq!(
            BoundedTagSet::try_from_iter(["same", "same"]).unwrap_err(),
            TagValidationError::Duplicate { tag: "same".into() }
        );
        assert!(matches!(
            BoundedTagSet::try_from_iter(["x".repeat(MAX_TAG_BYTES + 1)]),
            Err(TagValidationError::TooLong { .. })
        ));
        assert!(BoundedTagSet::try_from_iter([" untrimmed"]).is_err());
        assert!(BoundedTagSet::try_from_iter(["control\n"]).is_err());

        let legal: Vec<String> = (0..MAX_NODE_TAGS)
            .map(|index| format!("t{index}"))
            .collect();
        assert_eq!(
            BoundedTagSet::try_from_iter(&legal).unwrap().len(),
            MAX_NODE_TAGS
        );
        let too_many: Vec<String> = (0..=MAX_NODE_TAGS)
            .map(|index| format!("t{index}"))
            .collect();
        assert!(matches!(
            BoundedTagSet::try_from_iter(&too_many),
            Err(TagValidationError::TooMany)
        ));
        assert!(
            serde_json::from_value::<BoundedTagSet>(serde_json::json!(["dup", "dup"])).is_err()
        );
        assert!(serde_json::from_value::<BoundedTagSet>(serde_json::json!(too_many)).is_err());

        assert!(
            Node::try_new(
                NodeId(Ulid::from(2u128)),
                "s",
                BodyRef::new("inline://x").unwrap(),
                ["bad\n"],
                Provenance::derived_empty(),
                0.5,
                0.5,
                NodeStatus::Active,
                1,
            )
            .is_err()
        );
    }

    #[test]
    fn origin_commit_round_trips_and_defaults() {
        // Stamped commit survives a serialize/deserialize cycle.
        let commit = OriginCommit::parse(&"a".repeat(40)).unwrap();
        let n = a_node().with_origin_commit(Some(commit));
        let json = serde_json::to_string(&n).unwrap();
        let back: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(back.origin_commit(), Some(commit));

        // Unstamped is None and serializes to JSON null (host renders it as such).
        let plain = a_node();
        assert_eq!(plain.origin_commit(), None);

        // Backward compat: a snapshot written before the field existed (no
        // `origin_commit` key) still loads, defaulting to None — the load-bearing
        // guarantee behind `#[serde(default)]`, since nodes are stored as JSON blobs.
        let mut legacy = serde_json::to_value(a_node()).unwrap();
        legacy.as_object_mut().unwrap().remove("origin_commit");
        let loaded: Node = serde_json::from_value(legacy).unwrap();
        assert_eq!(loaded.origin_commit(), None);
    }

    #[test]
    fn body_ownership_round_trips_and_legacy_defaults_to_borrowed() {
        let managed = a_node().with_body_ownership(BodyOwnership::Managed);
        let json = serde_json::to_string(&managed).unwrap();
        let loaded: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.body_ownership(), BodyOwnership::Managed);

        let mut legacy = serde_json::to_value(managed).unwrap();
        legacy.as_object_mut().unwrap().remove("body_ownership");
        let loaded: Node = serde_json::from_value(legacy).unwrap();
        assert_eq!(loaded.body_ownership(), BodyOwnership::Borrowed);
    }

    #[test]
    fn exposure_and_grounded_use_do_not_change_membership_or_confidence() {
        let mut node = Node::try_new(
            NodeId(Ulid::new()),
            "s",
            BodyRef::new("inline://x").unwrap(),
            ["t"],
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        let confidence = node.confidence().to_bits();
        node.record_exposure(2);
        node.record_grounded_use(3);
        assert_eq!(node.last_exposed(), Some(2));
        assert_eq!(node.exposure_count(), 1);
        assert_eq!(node.last_grounded_use(), Some(3));
        assert_eq!(node.grounded_use_count(), 1);
        assert_eq!(node.status(), NodeStatus::Active);
        assert_eq!(node.confidence().to_bits(), confidence);
        assert_eq!(node.interference(), 0);
    }

    #[test]
    fn legacy_activation_telemetry_migrates_to_exposure_only() {
        let mut legacy = serde_json::to_value(a_node()).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("last_exposed");
        object.remove("exposure_count");
        object.remove("last_grounded_use");
        object.remove("grounded_use_count");
        object.insert("last_activated".into(), serde_json::json!(55));
        object.insert("activation_count".into(), serde_json::json!(7));

        let loaded: Node = serde_json::from_value(legacy).unwrap();
        assert_eq!(loaded.last_exposed(), Some(55));
        assert_eq!(loaded.exposure_count(), 7);
        assert_eq!(loaded.last_grounded_use(), None);
        assert_eq!(loaded.grounded_use_count(), 0);

        let current = serde_json::to_value(loaded).unwrap();
        let object = current.as_object().unwrap();
        assert!(!object.contains_key("last_activated"));
        assert!(!object.contains_key("activation_count"));
        assert_eq!(object["last_exposed"], serde_json::json!(55));
        assert_eq!(object["exposure_count"], serde_json::json!(7));
        assert_eq!(object["last_grounded_use"], serde_json::Value::Null);
        assert_eq!(object["grounded_use_count"], serde_json::json!(0));
    }

    fn edge(weight: f32) -> Edge {
        Edge::new(
            NodeId(Ulid::new()),
            NodeId(Ulid::new()),
            weight,
            EdgeKind::Associative,
            0,
        )
    }

    #[test]
    fn reinforce_has_diminishing_returns() {
        let p = StrengthParams::default();
        let mut e = edge(0.0);
        e.reinforce(1, &p);
        let first = e.weight(); // step up from 0.0
        e.reinforce(2, &p);
        let second = e.weight() - first;
        assert!(
            first > second,
            "first hit ({first}) should move weight more than the next ({second})"
        );
        assert_eq!(e.trials(), 2);
        assert_eq!(e.last_reinforced(), 2);
    }

    #[test]
    fn idle_edge_never_decays() {
        let p = StrengthParams::default();
        let mut e = edge(0.7);
        e.decay(&p); // nothing was ever passed over along it
        assert_eq!(
            e.weight(),
            0.7,
            "forgetting needs emitted interference, not time"
        );
    }

    #[test]
    fn decay_consumes_interference_and_is_idempotent() {
        let p = StrengthParams::default();
        let mut e = edge(0.8);
        e.mark_interference();
        e.mark_interference();
        e.decay(&p);
        let after = e.weight();
        assert!(
            after < 0.8,
            "banked interference should erode weight, got {after}"
        );
        assert_eq!(
            e.interference(),
            0,
            "decay must consume the banked interference"
        );
        e.decay(&p); // nothing emitted since ⇒ no-op
        assert_eq!(e.weight(), after, "a repeat decay must be a no-op");
    }

    #[test]
    fn well_worn_edge_resists_but_is_not_immortal() {
        let p = StrengthParams::default();

        // One edge barely used, one deeply established (same starting weight).
        let mut fresh = edge(1.0);
        fresh.reinforce(1, &p);
        let mut worn = edge(1.0);
        for t in 1..=200 {
            worn.reinforce(t, &p);
        }

        // Identical interference load erodes the fresh edge far more.
        for _ in 0..5 {
            fresh.mark_interference();
            worn.mark_interference();
        }
        fresh.decay(&p);
        worn.decay(&p);
        assert!(
            worn.weight() > fresh.weight(),
            "the well-worn edge ({}) should resist far better than the fresh one ({})",
            worn.weight(),
            fresh.weight()
        );

        // But sustained, repeated irrelevance still kills it — no immortal edges.
        for _ in 0..400 {
            worn.mark_interference();
            worn.decay(&p);
        }
        assert!(
            worn.weight() < 0.05,
            "enough misses must eventually kill even a worn edge, got {}",
            worn.weight()
        );
    }

    #[test]
    fn remote_edge_weights_have_a_finite_canonical_order() {
        let from = NodeId(Ulid::from(1u128));
        let db = Ulid::from(2u128);
        let target = NodeId(Ulid::from(3u128));

        assert_eq!(RemoteEdge::new(from, db, target, f32::NAN).weight(), 0.0);
        assert_eq!(
            RemoteEdge::new(from, db, target, f32::INFINITY).weight(),
            1.0
        );
        assert_eq!(
            RemoteEdge::new(from, db, target, f32::NEG_INFINITY).weight(),
            0.0
        );
        assert_eq!(
            RemoteEdge::new(from, db, target, -0.0).weight().to_bits(),
            0
        );
    }

    #[test]
    fn local_edge_weights_have_a_finite_canonical_order() {
        let from = NodeId(Ulid::from(1u128));
        let to = NodeId(Ulid::from(2u128));
        let edge = |weight| Edge::new(from, to, weight, EdgeKind::Associative, 1);

        assert_eq!(edge(f32::NAN).weight(), 0.0);
        assert_eq!(edge(f32::INFINITY).weight(), 1.0);
        assert_eq!(edge(f32::NEG_INFINITY).weight(), 0.0);
        assert_eq!(edge(-0.0).weight().to_bits(), 0);
    }

    #[test]
    fn local_edge_validation_bounds_pending_interference_by_trials() {
        let from = NodeId(Ulid::from(1u128));
        let to = NodeId(Ulid::from(2u128));
        let stored = |trials, interference| {
            Edge::from_stored(
                from,
                to,
                EdgeKind::Associative,
                None,
                0.5,
                1,
                trials,
                interference,
            )
        };

        assert!(stored(0, 0).validate().is_ok());
        assert!(stored(3, 3).validate().is_ok());
        assert!(stored(3, 4).validate().is_err());
    }

    #[test]
    fn persisted_edge_validation_rejects_negative_zero() {
        let edge = Edge::from_stored(
            NodeId(Ulid::from(1u128)),
            NodeId(Ulid::from(2u128)),
            EdgeKind::Associative,
            None,
            -0.0,
            1,
            0,
            0,
        );

        assert!(edge.validate().is_err());
    }

    #[test]
    fn local_edge_validation_bounds_storage_timestamp() {
        let stored = |last_reinforced| {
            Edge::from_stored(
                NodeId(Ulid::from(1u128)),
                NodeId(Ulid::from(2u128)),
                EdgeKind::Associative,
                None,
                0.5,
                last_reinforced,
                0,
                0,
            )
        };

        assert!(stored(i64::MAX as Timestamp).validate().is_ok());
        assert!(stored(i64::MAX as Timestamp + 1).validate().is_err());
    }

    #[test]
    fn remote_edge_database_validation_rejects_nil_and_local_targets() {
        let local_db = Ulid::from(10u128);
        let remote_db = Ulid::from(11u128);
        let nil_node = NodeId(Ulid::nil());

        assert!(
            RemoteEdge::new(nil_node, remote_db, nil_node, 0.5)
                .validate_for_source_database(local_db)
                .is_ok(),
            "node ids remain opaque and may be nil"
        );
        assert!(
            RemoteEdge::new(nil_node, Ulid::nil(), nil_node, 0.5)
                .validate_for_source_database(local_db)
                .is_err()
        );
        assert!(
            RemoteEdge::new(nil_node, local_db, nil_node, 0.5)
                .validate_for_source_database(local_db)
                .is_err()
        );

        let invalid_weight = RemoteEdge {
            from: nil_node,
            target_db: remote_db,
            target: nil_node,
            weight: f32::NAN,
        };
        assert!(
            invalid_weight
                .validate_for_source_database(local_db)
                .is_err()
        );

        let negative_zero = RemoteEdge {
            from: nil_node,
            target_db: remote_db,
            target: nil_node,
            weight: -0.0,
        };
        assert!(
            negative_zero
                .validate_for_source_database(local_db)
                .is_err()
        );
    }

    #[test]
    fn contradiction_validation_bounds_observation_history() {
        let a = NodeId(Ulid::from(20u128));
        let b = NodeId(Ulid::from(21u128));
        let valid = Contradiction {
            between: UnorderedPair(a, b),
            observations: 1,
            first_seen: i64::MAX as Timestamp,
            last_seen: i64::MAX as Timestamp,
            resolution: None,
        };
        assert!(valid.validate().is_ok());

        let mut invalid = valid.clone();
        invalid.between = UnorderedPair(a, a);
        assert!(invalid.validate().is_err());

        let mut invalid = valid.clone();
        invalid.observations = 0;
        assert!(invalid.validate().is_err());

        let mut invalid = valid.clone();
        invalid.first_seen = i64::MAX as Timestamp + 1;
        assert!(invalid.validate().is_err());

        let mut invalid = valid.clone();
        invalid.last_seen = i64::MAX as Timestamp + 1;
        assert!(invalid.validate().is_err());

        let mut invalid = valid;
        invalid.first_seen = 2;
        invalid.last_seen = 1;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn merge_candidate_validation_bounds_observation_history() {
        let a = NodeId(Ulid::from(30u128));
        let b = NodeId(Ulid::from(31u128));
        let valid = MergeCandidate {
            between: UnorderedPair(a, b),
            observations: 1,
            first_seen: i64::MAX as Timestamp,
            last_seen: i64::MAX as Timestamp,
            resolution: None,
        };
        assert!(valid.validate().is_ok());

        let mut invalid = valid.clone();
        invalid.between = UnorderedPair(a, a);
        assert!(invalid.validate().is_err());

        let mut invalid = valid.clone();
        invalid.observations = 0;
        assert!(invalid.validate().is_err());

        let mut invalid = valid.clone();
        invalid.first_seen = i64::MAX as Timestamp + 1;
        assert!(invalid.validate().is_err());

        let mut invalid = valid.clone();
        invalid.last_seen = i64::MAX as Timestamp + 1;
        assert!(invalid.validate().is_err());

        let mut invalid = valid;
        invalid.first_seen = 2;
        invalid.last_seen = 1;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn remote_edge_cursor_is_exact_and_source_bound() {
        let from = NodeId(Ulid::from(10u128));
        let edge = RemoteEdge::new(from, Ulid::from(20u128), NodeId(Ulid::from(30u128)), 0.7);
        let cursor = RemoteEdgeCursor::from_edge(&edge);
        let round_trip: RemoteEdgeCursor =
            serde_json::from_str(&serde_json::to_string(&cursor).unwrap()).unwrap();

        assert_eq!(round_trip, cursor);
        assert_eq!(round_trip.weight().to_bits(), edge.weight().to_bits());
        assert!(round_trip.validate_for(from).is_ok());
        assert!(round_trip.validate_for(NodeId(Ulid::from(11u128))).is_err());
    }

    #[test]
    fn remote_edge_order_breaks_equal_weight_ties_by_identity() {
        let from = NodeId(Ulid::from(40u128));
        let low_db = RemoteEdge::new(from, Ulid::from(41u128), NodeId(Ulid::from(99u128)), 0.5);
        let low_target = RemoteEdge::new(from, Ulid::from(42u128), NodeId(Ulid::from(43u128)), 0.5);
        let high_target =
            RemoteEdge::new(from, Ulid::from(42u128), NodeId(Ulid::from(44u128)), 0.5);
        let stronger = RemoteEdge::new(from, Ulid::from(99u128), NodeId(Ulid::from(99u128)), 0.6);

        let mut edges = vec![
            high_target.clone(),
            low_target.clone(),
            low_db.clone(),
            stronger.clone(),
        ];
        edges.sort_by(remote_edge_order);
        assert_eq!(edges, vec![stronger, low_db, low_target, high_target]);
    }
}
