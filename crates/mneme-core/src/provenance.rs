//! Where a memory came from and how an incoming capture proves exact replay.
//!
//! Source identity, codec domains and historical request proofs live together:
//! provenance describes an observation, not its current truth or usefulness.
//! These types validate representations without reading a store or choosing
//! capture policy.

use crate::{NodeId, Timestamp};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use ulid::Ulid;

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
    pub(crate) field: &'static str,
    pub(crate) reason: &'static str,
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

    /// Check that this source belongs to the semantic capture lane. A valid
    /// episode source is not interchangeable with semantic provenance.
    pub fn validate_semantic(&self) -> std::result::Result<(), CaptureSourceValidationError> {
        self.validate()?;
        if !matches!(
            self.request_codec,
            CaptureRequestCodec::CaptureV1
                | CaptureRequestCodec::CaptureV2
                | CaptureRequestCodec::TouchstoneV1
        ) {
            return Err(CaptureSourceValidationError {
                field: "request_codec",
                reason: "codec does not match enclosing memory kind",
            });
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

/// Incoming immutable request proof. Historical semantic digests are computed
/// from the original request, never from a subsequently edited node.
///
/// Constructors bind the source codec to its replay rules. Callers cannot bypass
/// that check by constructing a proof variant directly:
///
/// ```compile_fail
/// use mneme_core::{CaptureReplayProof, CaptureSource};
/// let source = CaptureSource::new("codex", "key", "ref", None, None, [0; 32]).unwrap();
/// let forged = CaptureReplayProof::Episode { source };
/// ```
#[derive(Clone, Debug)]
pub struct CaptureReplayProof(ReplayKind);

#[derive(Clone, Debug)]
enum ReplayKind {
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
        Ok(Self(ReplayKind::Semantic {
            source,
            legacy_candidate_digest,
            legacy_active_digest,
        }))
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
        Ok(Self(ReplayKind::Episode { source }))
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
        Ok(Self(ReplayKind::Touchstone { source }))
    }

    pub fn source(&self) -> &CaptureSource {
        match &self.0 {
            ReplayKind::Semantic { source, .. }
            | ReplayKind::Episode { source }
            | ReplayKind::Touchstone { source } => source,
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
        match &self.0 {
            ReplayKind::Semantic {
                source,
                legacy_candidate_digest,
                legacy_active_digest,
            } => match stored.request_codec {
                CaptureRequestCodec::CaptureV2 => stored.request_digest == source.request_digest,
                CaptureRequestCodec::CaptureV1 => {
                    stored.request_digest == *legacy_candidate_digest
                        || stored.request_digest == *legacy_active_digest
                }
                CaptureRequestCodec::EpisodeV1
                | CaptureRequestCodec::EpisodeV2
                | CaptureRequestCodec::TouchstoneV1 => false,
            },
            ReplayKind::Episode { source } | ReplayKind::Touchstone { source } => {
                stored.request_codec == source.request_codec
                    && stored.request_digest == source.request_digest
            }
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

pub(crate) fn validate_web_url(value: &str) -> std::result::Result<(), WebUrlValidationError> {
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

#[cfg(test)]
mod tests {
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
    }

    #[test]
    fn replay_constructors_bind_the_complete_codec_domain() {
        for codec in [
            CaptureRequestCodec::CaptureV1,
            CaptureRequestCodec::CaptureV2,
            CaptureRequestCodec::EpisodeV1,
            CaptureRequestCodec::EpisodeV2,
            CaptureRequestCodec::TouchstoneV1,
        ] {
            let source =
                CaptureSource::new_with_codec("codex", "key", "ref", None, None, [9; 32], codec)
                    .unwrap();
            assert_eq!(
                CaptureReplayProof::semantic(source.clone(), [1; 32], [2; 32]).is_ok(),
                codec == CaptureRequestCodec::CaptureV2,
            );
            assert_eq!(
                CaptureReplayProof::episode(source.clone()).is_ok(),
                matches!(
                    codec,
                    CaptureRequestCodec::EpisodeV1 | CaptureRequestCodec::EpisodeV2
                ),
            );
            assert_eq!(
                CaptureReplayProof::touchstone(source).is_ok(),
                codec == CaptureRequestCodec::TouchstoneV1,
            );
        }
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
}
