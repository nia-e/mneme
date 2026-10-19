//! Checked input cards and their exact wire projection. This module owns
//! semantic versus episode provenance; budget allocation stays in the packer.

use std::num::NonZeroU16;

use mneme_core::episode::{
    EpisodeHeader, EpisodeIdentity, EpisodeRecordingSession, EpisodeThread, EpisodeTime,
    MAX_OCCURRENCE_CONTEXTS_JSON_BYTES, OccurrenceContextRef, OccurrenceContexts, OccurrenceSpan,
};
use mneme_core::{BodySpan, Edge, EdgeKind, NodeId, NodeSummary, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{EpisodeTimePresentation, InputError, TouchstoneView, format_timestamp_millis};

macro_rules! input_card {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct $name(CardSource<SemanticMetadata>);

        impl $name {
            pub fn new(
                id: NodeId,
                created: Timestamp,
                rank: NonZeroU16,
                summary: impl Into<String>,
            ) -> Result<Self, InputError> {
                Ok(Self(CardSource::new(
                    id,
                    rank,
                    summary.into(),
                    SemanticMetadata {
                        created,
                        touchstone: None,
                    },
                )?))
            }

            /// When the semantic node was recorded; not its last verification.
            pub fn created(&self) -> Timestamp {
                self.0.metadata.created
            }

            pub const fn id(&self) -> NodeId {
                self.0.id
            }

            pub const fn rank(&self) -> NonZeroU16 {
                self.0.rank
            }

            pub fn summary(&self) -> &str {
                &self.0.summary
            }

            pub fn with_touchstone(mut self, view: TouchstoneView) -> Self {
                self.0.metadata.touchstone = Some(view);
                self
            }

            pub fn with_rank(mut self, rank: NonZeroU16) -> Self {
                self.0.rank = rank;
                self
            }
        }

        impl From<$name> for SourceCard {
            fn from(card: $name) -> Self {
                card.0.map_metadata(CardMetadata::Semantic)
            }
        }
    };
}

input_card!(CoreInputCard);
input_card!(PrimaryInputCard);
input_card!(ExpansionInputCard);

/// The exact frozen anchor which exposed a one-hop reference. Episode anchors
/// name an immutable edition, not a mutable head or newly discovered scene.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EpisodeAnchor {
    Semantic { node_id: NodeId },
    Episode { identity: EpisodeIdentity },
}
impl EpisodeAnchor {
    pub const fn node_id(&self) -> NodeId {
        match self {
            Self::Semantic { node_id } => *node_id,
            Self::Episode { identity } => identity.edition_id,
        }
    }
}

/// Query-local navigation evidence. Body anchors always belong to stored
/// `from`, even when discovery arrived along an incoming edge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EpisodeOrigin {
    Lexical,
    Reference {
        anchor: EpisodeAnchor,
        from: NodeId,
        to: NodeId,
        edge_kind: EdgeKind,
        body_anchor: Option<BodySpan>,
    },
}

/// Stable public wire ordering, independent of Rust Debug or process hashes.
pub fn canonical_episode_origins(mut origins: Vec<EpisodeOrigin>) -> Vec<EpisodeOrigin> {
    origins.sort_by_cached_key(|origin| {
        serde_json::to_vec(origin).expect("typed episode origin serializes")
    });
    origins.dedup();
    origins
}

pub fn canonicalize_episode_origins(
    value: &serde_json::Value,
) -> Result<serde_json::Value, InputError> {
    let origins: Vec<EpisodeOrigin> = serde_json::from_value(value.clone())
        .map_err(|_| InputError::InvalidEpisode("invalid typed origins"))?;
    if origins.is_empty() {
        return Err(InputError::InvalidEpisode(
            "episode origins cannot be empty",
        ));
    }
    Ok(serde_json::to_value(canonical_episode_origins(origins)).expect("typed origins serialize"))
}

fn valid_episode_identity(identity: EpisodeIdentity) -> bool {
    (identity.revision.get() == 0) == (identity.episode_id.node_id() == identity.edition_id)
}

fn validate_episode_origin(origin: &EpisodeOrigin, edition_id: NodeId) -> Result<(), InputError> {
    if let EpisodeOrigin::Reference {
        anchor, from, to, ..
    } = origin
    {
        let anchor_id = anchor.node_id();
        if matches!(anchor, EpisodeAnchor::Episode { identity } if !valid_episode_identity(*identity))
            || from == to
            || !((*from == anchor_id && *to == edition_id)
                || (*to == anchor_id && *from == edition_id))
        {
            return Err(InputError::InvalidEpisode(
                "reference does not name the exact opposite episode endpoint",
            ));
        }
    }
    Ok(())
}

/// A checked immutable account with query-local origins and observed head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodeInputCard(CardSource<EpisodeCardMetadata>);

impl EpisodeInputCard {
    pub fn new(header: EpisodeHeader, rank: NonZeroU16) -> Result<Self, InputError> {
        if header.identity.edition_id != header.current_edition_id {
            return Err(InputError::InvalidEpisode(
                "only current editions can be presented",
            ));
        }
        Self::from_header(header, rank, EpisodeOrigin::Lexical)
    }

    pub fn referenced(
        header: EpisodeHeader,
        rank: NonZeroU16,
        anchor: EpisodeAnchor,
        edge: &Edge,
    ) -> Result<Self, InputError> {
        let origin = EpisodeOrigin::Reference {
            anchor,
            from: edge.from,
            to: edge.to,
            edge_kind: edge.kind,
            body_anchor: edge.anchor,
        };
        validate_episode_origin(&origin, header.identity.edition_id)?;
        Self::from_header(header, rank, origin)
    }

    fn from_header(
        header: EpisodeHeader,
        rank: NonZeroU16,
        origin: EpisodeOrigin,
    ) -> Result<Self, InputError> {
        let is_initial = header.identity.revision.get() == 0;
        if is_initial != (header.identity.episode_id.node_id() == header.identity.edition_id) {
            return Err(InputError::InvalidEpisode(
                "root identity and revision disagree",
            ));
        }
        // Edition ordinals, not wall-clock monotonicity, establish history
        // order. A later editorial pass may be recorded after clock rollback.
        if is_initial && header.recorded_at != header.edition_recorded_at {
            return Err(InputError::InvalidEpisode("recording timestamps disagree"));
        }
        header
            .occurred
            .validate()
            .map_err(|error| InputError::InvalidEpisode(error.0))?;
        let metadata = EpisodeCardMetadata {
            kind: EpisodeCardKind::Episode,
            identity: header.identity,
            current_edition_id: header.current_edition_id,
            occurred: header.occurred,
            recorded_at: header.recorded_at,
            edition_recorded_at: header.edition_recorded_at,
            thread: header.thread,
            occurrence_contexts: header.occurrence_contexts,
            recording_session: header.recording_session,
            origins: vec![origin],
        };
        Ok(Self(CardSource::new(
            header.identity.edition_id,
            rank,
            header.summary.as_str().to_owned(),
            metadata,
        )?))
    }

    pub const fn id(&self) -> NodeId {
        self.0.id
    }

    pub const fn rank(&self) -> NonZeroU16 {
        self.0.rank
    }

    pub fn summary(&self) -> &str {
        &self.0.summary
    }

    pub fn identity(&self) -> EpisodeIdentity {
        self.0.metadata.identity
    }
    pub fn origins(&self) -> &[EpisodeOrigin] {
        &self.0.metadata.origins
    }
    pub fn header(&self) -> EpisodeHeader {
        let m = &self.0.metadata;
        EpisodeHeader {
            identity: m.identity,
            summary: NodeSummary::new(&self.0.summary).expect("validated episode summary"),
            occurred: m.occurred.clone(),
            recorded_at: m.recorded_at,
            edition_recorded_at: m.edition_recorded_at,
            thread: m.thread.clone(),
            occurrence_contexts: m.occurrence_contexts.clone(),
            recording_session: m.recording_session.clone(),
            current_edition_id: m.current_edition_id,
        }
    }
    pub fn with_rank(mut self, rank: NonZeroU16) -> Self {
        self.0.rank = rank;
        self
    }
    pub fn merge_origins(&mut self, other: &Self) -> Result<(), InputError> {
        if self.0.summary != other.0.summary {
            return Err(InputError::InvalidEpisode(
                "cannot merge different immutable accounts",
            ));
        }
        self.0.metadata.merge_origins(&other.0.metadata)
    }
}

impl From<EpisodeInputCard> for SourceCard {
    fn from(card: EpisodeInputCard) -> Self {
        card.0.map_metadata(CardMetadata::Episode)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EpisodeCardKind {
    Episode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpisodeCardMetadata {
    kind: EpisodeCardKind,
    #[serde(flatten)]
    identity: EpisodeIdentity,
    current_edition_id: NodeId,
    occurred: OccurrenceSpan,
    recorded_at: EpisodeTime,
    edition_recorded_at: EpisodeTime,
    thread: Option<EpisodeThread>,
    #[serde(skip_serializing_if = "Option::is_none")]
    occurrence_contexts: Option<OccurrenceContexts>,
    recording_session: Option<EpisodeRecordingSession>,
    origins: Vec<EpisodeOrigin>,
}

impl EpisodeCardMetadata {
    pub fn time_presentation(&self) -> EpisodeTimePresentation {
        EpisodeTimePresentation::new(&self.occurred, self.recorded_at, self.edition_recorded_at)
    }

    pub const fn identity(&self) -> EpisodeIdentity {
        self.identity
    }

    pub const fn current_edition_id(&self) -> NodeId {
        self.current_edition_id
    }

    pub const fn occurred(&self) -> &OccurrenceSpan {
        &self.occurred
    }

    pub const fn recorded_at(&self) -> EpisodeTime {
        self.recorded_at
    }

    pub const fn edition_recorded_at(&self) -> EpisodeTime {
        self.edition_recorded_at
    }

    pub const fn thread(&self) -> Option<&EpisodeThread> {
        self.thread.as_ref()
    }

    pub const fn occurrence_contexts(&self) -> Option<&OccurrenceContexts> {
        self.occurrence_contexts.as_ref()
    }
    pub fn recording_session(&self) -> Option<&EpisodeRecordingSession> {
        self.recording_session.as_ref()
    }
    pub fn origins(&self) -> &[EpisodeOrigin] {
        &self.origins
    }
    pub fn merge_origins(&mut self, other: &Self) -> Result<(), InputError> {
        let mut account = other.clone();
        account.current_edition_id = self.current_edition_id;
        account.origins = self.origins.clone();
        if *self != account {
            return Err(InputError::InvalidEpisode(
                "cannot merge different immutable accounts",
            ));
        }
        let origins =
            canonical_episode_origins(self.origins.iter().chain(&other.origins).cloned().collect());
        if origins.contains(&EpisodeOrigin::Lexical)
            && self.current_edition_id != self.identity.edition_id
        {
            return Err(InputError::InvalidEpisode(
                "lexical origin requires the observed current edition",
            ));
        }
        self.origins = origins;
        Ok(())
    }
}

/// Shared native owner-card validation; library adapters retain their own
/// summary/trust/ID checks but do not reinterpret historical episode metadata.
pub fn validate_episode_card_metadata(
    card: &serde_json::Value,
    lexical_searched: bool,
) -> Result<EpisodeCardMetadata, InputError> {
    if card.get("recording_session").is_none() {
        return Err(InputError::InvalidEpisode(
            "recording_session must be explicit",
        ));
    }
    let m: EpisodeCardMetadata = serde_json::from_value(card.clone())
        .map_err(|_| InputError::InvalidEpisode("invalid typed episode metadata"))?;
    if !valid_episode_identity(m.identity)
        || (m.identity.revision.get() == 0 && m.recorded_at != m.edition_recorded_at)
        || m.origins.is_empty()
        || m.origins != canonical_episode_origins(m.origins.clone())
        || (m.origins.contains(&EpisodeOrigin::Lexical)
            && (!lexical_searched || m.identity.edition_id != m.current_edition_id))
    {
        return Err(InputError::InvalidEpisode(
            "invalid episode identity or origins",
        ));
    }
    m.occurred
        .validate()
        .map_err(|e| InputError::InvalidEpisode(e.0))?;
    for origin in &m.origins {
        validate_episode_origin(origin, m.identity.edition_id)?;
    }
    let display = serde_json::to_value(m.time_presentation()).expect("typed dates serialize");
    for key in ["occurred_utc", "recorded_at_utc", "edition_recorded_at_utc"] {
        if card.get(key).is_some_and(|value| value != &display[key]) {
            return Err(InputError::InvalidEpisode(
                "calendar presentation disagrees with canonical time",
            ));
        }
    }
    let canonical = serde_json::to_value(&m).expect("typed metadata serializes");
    for key in ["origins", "occurrence_contexts", "recording_session"] {
        if card.get(key) != canonical.get(key) {
            return Err(InputError::InvalidEpisode("noncanonical episode metadata"));
        }
    }
    Ok(m)
}

/// Conservative fixed-header reserve plus minimal legal origins. Origin unions
/// are input-dependent; only runtime packing proves the whole mandatory card
/// footprint. Never pretend the reference-work ceiling is a small scene quota.
pub(super) fn worst_episode_metadata() -> EpisodeCardMetadata {
    use mneme_core::episode::{EpisodeId, EpisodeRevision, MAX_EPISODE_THREAD_BYTES};

    let last = EpisodeTime::new(i64::MAX as u128).expect("maximum episode timestamp");
    let before = EpisodeTime::new(i64::MAX as u128 - 1).expect("episode timestamp");
    let id = NodeId(ulid::Ulid::from(u128::MAX));
    EpisodeCardMetadata {
        kind: EpisodeCardKind::Episode,
        identity: EpisodeIdentity {
            episode_id: EpisodeId::new(NodeId(ulid::Ulid::from(u128::MAX - 1))),
            edition_id: id,
            revision: EpisodeRevision::new(u32::MAX),
        },
        current_edition_id: id,
        occurred: OccurrenceSpan::range(before, last).expect("ordered time range"),
        recorded_at: last,
        edition_recorded_at: last,
        thread: Some(
            EpisodeThread::new("\"".repeat(MAX_EPISODE_THREAD_BYTES))
                .expect("bounded thread with worst-case JSON escaping"),
        ),
        occurrence_contexts: Some(maximum_occurrence_contexts()),
        recording_session: Some(
            EpisodeRecordingSession::new("\"".repeat(mneme_core::MAX_CAPTURE_SESSION_BYTES))
                .expect("maximum session"),
        ),
        origins: vec![EpisodeOrigin::Lexical],
    }
}

/// Fill the complete context collection's byte allowance, not an individual
/// identifier allowance. Runtime cards retain the real collection unchanged.
pub(super) fn maximum_occurrence_contexts() -> OccurrenceContexts {
    let minimum = OccurrenceContexts::new(vec![
        OccurrenceContextRef::new("x", "x", None).expect("fixed valid context"),
    ])
    .expect("fixed valid context collection");
    let key = "x".repeat(MAX_OCCURRENCE_CONTEXTS_JSON_BYTES - minimum.compact_json_len() + 1);
    let contexts = OccurrenceContexts::new(vec![
        OccurrenceContextRef::new("x", key, None).expect("bounded context identifier"),
    ])
    .expect("maximum context collection fits its byte allowance");
    debug_assert_eq!(
        contexts.compact_json_len(),
        MAX_OCCURRENCE_CONTEXTS_JSON_BYTES
    );
    contexts
}

/// Semantic creation and authored meaning are never episode provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SemanticMetadata {
    created: Timestamp,
    touchstone: Option<TouchstoneView>,
}

/// The homogeneous packer erases the input wrapper's payload type here, not
/// while constructing a card. A semantic card always has a creation time;
/// an episode always has its immutable account and cannot carry a touchstone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CardMetadata {
    Semantic(SemanticMetadata),
    Episode(EpisodeCardMetadata),
}

/// Common text/rank accounting; the private payload parameter preserves each
/// public input wrapper's provenance without a public typestate API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CardSource<P> {
    pub(super) id: NodeId,
    pub(super) rank: NonZeroU16,
    pub(super) summary: String,
    pub(super) source_bytes: u32,
    metadata: P,
}

pub(super) type SourceCard = CardSource<CardMetadata>;

impl<P> CardSource<P> {
    fn new(id: NodeId, rank: NonZeroU16, summary: String, metadata: P) -> Result<Self, InputError> {
        let source_bytes = u32::try_from(summary.len()).map_err(|_| InputError::SummaryTooLarge)?;
        Ok(Self {
            id,
            rank,
            summary,
            source_bytes,
            metadata,
        })
    }

    fn map_metadata<Q>(self, map: impl FnOnce(P) -> Q) -> CardSource<Q> {
        CardSource {
            id: self.id,
            rank: self.rank,
            summary: self.summary,
            source_bytes: self.source_bytes,
            metadata: map(self.metadata),
        }
    }
}

impl SourceCard {
    pub(super) fn touchstone(&self) -> Option<&TouchstoneView> {
        match &self.metadata {
            CardMetadata::Semantic(metadata) => metadata.touchstone.as_ref(),
            CardMetadata::Episode(_) => None,
        }
    }

    pub(super) fn episode(&self) -> Option<&EpisodeCardMetadata> {
        match &self.metadata {
            CardMetadata::Semantic(_) => None,
            CardMetadata::Episode(metadata) => Some(metadata),
        }
    }

    pub(super) fn skeleton(&self) -> WireCard {
        let (created, episode, touchstone) = match &self.metadata {
            CardMetadata::Semantic(metadata) => {
                (Some(metadata.created), None, metadata.touchstone.clone())
            }
            CardMetadata::Episode(metadata) => (None, Some(metadata.clone()), None),
        };
        WireCard {
            id: self.id,
            created,
            created_utc: created.map(format_timestamp_millis),
            episode_times_utc: episode.as_ref().map(EpisodeCardMetadata::time_presentation),
            rank: self.rank,
            summary: SummaryText {
                text: String::new(),
                complete: self.summary.is_empty(),
                source_bytes: self.source_bytes,
            },
            content_trust: ContentTrust::Untrusted,
            episode,
            touchstone,
        }
    }
}

/// UTF-8 summary text with an explicit source-length/completeness contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SummaryText {
    pub(super) text: String,
    pub(super) complete: bool,
    pub(super) source_bytes: u32,
}

impl SummaryText {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn complete(&self) -> bool {
        self.complete
    }

    pub const fn source_bytes(&self) -> u32 {
        self.source_bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ContentTrust {
    Untrusted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
// This serialization-only DTO deliberately retains its established field
// order and flattening: the compact bytes are also the manifest's digest input.
pub(super) struct WireCard {
    pub(super) id: NodeId,
    /// Semantic node creation, not a claim about present-day validity. Episodes
    /// retain their separate occurrence and account/edition recording times.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) created: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) created_utc: Option<String>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub(super) episode_times_utc: Option<EpisodeTimePresentation>,
    pub(super) rank: NonZeroU16,
    pub(super) summary: SummaryText,
    pub(super) content_trust: ContentTrust,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub(super) episode: Option<EpisodeCardMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) touchstone: Option<TouchstoneView>,
}

macro_rules! output_card {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
        #[serde(transparent)]
        pub struct $name(pub(super) WireCard);

        impl $name {
            pub const fn id(&self) -> NodeId {
                self.0.id
            }

            pub const fn rank(&self) -> NonZeroU16 {
                self.0.rank
            }

            pub const fn summary(&self) -> &SummaryText {
                &self.0.summary
            }

            pub fn touchstone(&self) -> Option<&TouchstoneView> {
                self.0.touchstone.as_ref()
            }

            pub const fn content_is_untrusted(&self) -> bool {
                true
            }
        }
    };
}

output_card!(NodeCard);
output_card!(HitCard);
output_card!(ExpansionCard);
output_card!(EpisodeCard);

impl EpisodeCard {
    pub fn episode(&self) -> &EpisodeCardMetadata {
        self.0
            .episode
            .as_ref()
            .expect("episode output is constructed only from typed episode input")
    }
}
