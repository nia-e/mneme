//! Exact, bounded presentation of typed mneme retrieval lanes.
//!
//! This crate is intentionally downstream of retrieval and upstream of every
//! transport. It selects whole cards before allocating summary text, renders
//! the final compact JSON once, and returns a manifest over exactly those
//! rendered cards. The transitional manifest is an audit artifact only: it has
//! node IDs and ranks, but no lifecycle binding or feedback authority.

use std::collections::HashSet;
use std::num::{NonZeroU16, NonZeroU32};

use mneme_core::episode::{
    EpisodeHeader, EpisodeIdentity, EpisodeRecordingSession, EpisodeThread, EpisodeTime,
    MAX_OCCURRENCE_CONTEXTS_JSON_BYTES, OccurrenceContextRef, OccurrenceContexts, OccurrenceSpan,
};
use mneme_core::tagged::{TaggedAnnWork, TaggedSeedCoverage};
use mneme_core::{BodySpan, Edge, EdgeKind, EmbeddingFingerprint, NodeId, NodeSummary};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

mod touchstone;
pub use touchstone::*;

const CONTEXT_SCHEMA: &str = "mneme.context.v7";
const QUERY_SCHEMA: &str = "mneme.query.v3";
const MAX_RETRIEVAL_STAMP_TEXT_BYTES: usize = 256;
const MAX_PROJECTION_WATERMARKS: usize = 8;
const MAX_QUERY_HITS_PER_LANE: usize = 256;
const TAG_MEMBERSHIP_PROJECTION: &str = "tag-membership";

/// Whether a retrieval used ordinary ANN seeds or the bounded tagged path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalMode {
    Untagged,
    Tagged,
}

/// The ordinary searchable retrieval lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalLane {
    Primary,
}

/// Stable ordering evidence for a query hit. Every rank is one-based and local
/// to its own retrieval leg; absent evidence means that leg did not retain the
/// node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RankEvidence {
    dense_rank: Option<NonZeroU16>,
    sparse_rank: Option<NonZeroU16>,
    graph_rank: Option<NonZeroU16>,
    rerank_rank: Option<NonZeroU16>,
}

impl RankEvidence {
    pub const fn new(
        dense_rank: Option<NonZeroU16>,
        sparse_rank: Option<NonZeroU16>,
        graph_rank: Option<NonZeroU16>,
        rerank_rank: Option<NonZeroU16>,
    ) -> Self {
        Self {
            dense_rank,
            sparse_rank,
            graph_rank,
            rerank_rank,
        }
    }

    pub const fn dense_rank(self) -> Option<NonZeroU16> {
        self.dense_rank
    }

    pub const fn sparse_rank(self) -> Option<NonZeroU16> {
        self.sparse_rank
    }

    pub const fn graph_rank(self) -> Option<NonZeroU16> {
        self.graph_rank
    }

    pub const fn rerank_rank(self) -> Option<NonZeroU16> {
        self.rerank_rank
    }
}

/// Lifecycle label exposed by diagnostic query output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryNodeStatus {
    Active,
    Archived,
}

/// Optional body state for a diagnostic query hit. Keeping the state typed
/// prevents `null` from ambiguously meaning "not requested" or "budget ran
/// out".
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum QueryBody {
    NotRequested,
    OmittedBudget,
    Included {
        content: String,
        source_start: u64,
        source_end: u64,
        next_offset: Option<u64>,
        truncated: bool,
    },
}

impl QueryBody {
    pub fn included(
        content: impl Into<String>,
        source_start: u64,
        source_end: u64,
        next_offset: Option<u64>,
    ) -> Result<Self, QueryInputError> {
        let body = Self::Included {
            content: content.into(),
            source_start,
            source_end,
            next_offset,
            truncated: next_offset.is_some(),
        };
        body.validate()?;
        Ok(body)
    }

    const fn is_partial(&self) -> bool {
        match self {
            Self::NotRequested => false,
            Self::OmittedBudget => true,
            Self::Included { truncated, .. } => *truncated,
        }
    }

    fn validate(&self) -> Result<(), QueryInputError> {
        match self {
            Self::NotRequested | Self::OmittedBudget => Ok(()),
            Self::Included {
                source_start,
                source_end,
                next_offset,
                truncated,
                ..
            } if source_end >= source_start
                && next_offset.is_none_or(|next| next == *source_end)
                && *truncated == next_offset.is_some() =>
            {
                Ok(())
            }
            Self::Included { .. } => Err(QueryInputError::InvalidBodyRange),
        }
    }
}

/// One typed diagnostic query hit. `lane_rank` is meaningful only inside the
/// containing lane; no score is exposed or compared across lanes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QueryHit {
    id: NodeId,
    lane_rank: NonZeroU16,
    evidence: RankEvidence,
    status: QueryNodeStatus,
    summary: String,
    summary_truncated: bool,
    body: QueryBody,
}

impl QueryHit {
    pub fn new(
        id: NodeId,
        lane_rank: NonZeroU16,
        evidence: RankEvidence,
        status: QueryNodeStatus,
        summary: impl Into<String>,
        summary_truncated: bool,
        body: QueryBody,
    ) -> Self {
        Self {
            id,
            lane_rank,
            evidence,
            status,
            summary: summary.into(),
            summary_truncated,
            body,
        }
    }

    pub const fn id(&self) -> NodeId {
        self.id
    }

    pub const fn lane_rank(&self) -> NonZeroU16 {
        self.lane_rank
    }

    pub const fn evidence(&self) -> RankEvidence {
        self.evidence
    }

    const fn presentation_partial(&self) -> bool {
        self.summary_truncated || self.body.is_partial()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct RetrievalPolicyIdentity {
    contract: String,
    fingerprint_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct RetrievalIndexSet {
    embedding: EmbeddingFingerprint,
    vector_semantics: String,
    lexical_semantics: Option<String>,
    reranker_semantics: Option<String>,
}

/// One backend-verified projection generation carried by the retrieval stamp.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectionWatermark {
    projection: String,
    watermark: String,
}

impl ProjectionWatermark {
    pub fn projection(&self) -> &str {
        &self.projection
    }

    pub fn watermark(&self) -> &str {
        &self.watermark
    }
}

/// Engine-independent wire identity for the policy and indexes that produced a
/// retrieval result. Every string is bounded at construction, and fingerprints
/// remain fixed-size SHA-256 values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RetrievalStamp {
    retrieval_policy: RetrievalPolicyIdentity,
    index_set: RetrievalIndexSet,
    projection_watermarks: Vec<ProjectionWatermark>,
}

impl RetrievalStamp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        policy_contract: impl Into<String>,
        policy_fingerprint: [u8; 32],
        embedding: EmbeddingFingerprint,
        vector_semantics: impl Into<String>,
        lexical_semantics: Option<String>,
        reranker_semantics: Option<String>,
        projection_watermarks: Vec<(String, String)>,
    ) -> Result<Self, RetrievalMetadataError> {
        let policy_contract = policy_contract.into();
        let vector_semantics = vector_semantics.into();
        validate_stamp_text("retrieval policy contract", &policy_contract)?;
        embedding
            .validate()
            .map_err(RetrievalMetadataError::InvalidEmbeddingFingerprint)?;
        for (field, value) in [
            ("embedding id", embedding.embedding_id.as_str()),
            ("embedding normalization", embedding.normalization.as_str()),
            ("embedding query mode", embedding.query_mode.as_str()),
            ("vector semantics", vector_semantics.as_str()),
        ] {
            validate_stamp_text(field, value)?;
        }
        for (field, value) in [
            ("lexical semantics", lexical_semantics.as_deref()),
            ("reranker semantics", reranker_semantics.as_deref()),
        ] {
            if let Some(value) = value {
                validate_stamp_text(field, value)?;
            }
        }
        if projection_watermarks.len() > MAX_PROJECTION_WATERMARKS {
            return Err(RetrievalMetadataError::TooManyProjectionWatermarks {
                provided: projection_watermarks.len(),
                maximum: MAX_PROJECTION_WATERMARKS,
            });
        }
        let mut seen = HashSet::with_capacity(projection_watermarks.len());
        let mut watermarks = Vec::with_capacity(projection_watermarks.len());
        for (projection, watermark) in projection_watermarks {
            validate_stamp_text("projection name", &projection)?;
            validate_stamp_text("projection watermark", &watermark)?;
            if !seen.insert(projection.clone()) {
                return Err(RetrievalMetadataError::DuplicateProjection { projection });
            }
            watermarks.push(ProjectionWatermark {
                projection,
                watermark,
            });
        }
        Ok(Self {
            retrieval_policy: RetrievalPolicyIdentity {
                contract: policy_contract,
                fingerprint_sha256: hex_digest(&policy_fingerprint),
            },
            index_set: RetrievalIndexSet {
                embedding,
                vector_semantics,
                lexical_semantics,
                reranker_semantics,
            },
            projection_watermarks: watermarks,
        })
    }

    pub fn projection_watermarks(&self) -> &[ProjectionWatermark] {
        &self.projection_watermarks
    }

    fn has_projection(&self, name: &str) -> bool {
        self.projection_watermarks
            .iter()
            .any(|watermark| watermark.projection == name)
    }
}

fn validate_stamp_text(field: &'static str, value: &str) -> Result<(), RetrievalMetadataError> {
    if value.is_empty()
        || value.trim() != value
        || value.chars().any(char::is_control)
        || value.len() > MAX_RETRIEVAL_STAMP_TEXT_BYTES
    {
        return Err(RetrievalMetadataError::InvalidStampText {
            field,
            bytes: value.len(),
            maximum: MAX_RETRIEVAL_STAMP_TEXT_BYTES,
        });
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct RetrievalLaneMetadata {
    seed_coverage: Option<TaggedSeedCoverage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct RetrievalLanesMetadata {
    primary: RetrievalLaneMetadata,
}

/// Bounded retrieval omissions and the policy/index stamp that produced them.
/// Seed coverage describes only tagged seed selection; graph expansion may add
/// untagged final hits without changing it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RetrievalMetadata {
    mode: RetrievalMode,
    lanes: RetrievalLanesMetadata,
    work: Option<TaggedAnnWork>,
    stamp: RetrievalStamp,
    partial: bool,
}

impl RetrievalMetadata {
    pub fn untagged(stamp: RetrievalStamp) -> Result<Self, RetrievalMetadataError> {
        if stamp.has_projection(TAG_MEMBERSHIP_PROJECTION) {
            return Err(RetrievalMetadataError::UnexpectedTagMembershipWatermark);
        }
        Ok(Self {
            mode: RetrievalMode::Untagged,
            lanes: RetrievalLanesMetadata {
                primary: RetrievalLaneMetadata {
                    seed_coverage: None,
                },
            },
            work: None,
            stamp,
            partial: false,
        })
    }

    pub fn tagged(
        stamp: RetrievalStamp,
        work: TaggedAnnWork,
        primary_seed_coverage: TaggedSeedCoverage,
    ) -> Result<Self, RetrievalMetadataError> {
        if !stamp.has_projection(TAG_MEMBERSHIP_PROJECTION) {
            return Err(RetrievalMetadataError::MissingTagMembershipWatermark);
        }
        work.validate()
            .map_err(|error| RetrievalMetadataError::InvalidTaggedWork(error.to_string()))?;
        primary_seed_coverage.validate().map_err(|error| {
            RetrievalMetadataError::InvalidSeedCoverage {
                lane: RetrievalLane::Primary,
                message: error.to_string(),
            }
        })?;
        let partial = primary_seed_coverage.is_partial();
        Ok(Self {
            mode: RetrievalMode::Tagged,
            lanes: RetrievalLanesMetadata {
                primary: RetrievalLaneMetadata {
                    seed_coverage: Some(primary_seed_coverage),
                },
            },
            work: Some(work),
            stamp,
            partial,
        })
    }

    pub const fn mode(&self) -> RetrievalMode {
        self.mode
    }

    pub const fn work(&self) -> Option<TaggedAnnWork> {
        self.work
    }

    pub const fn stamp(&self) -> &RetrievalStamp {
        &self.stamp
    }

    pub const fn partial(&self) -> bool {
        self.partial
    }

    pub fn seed_coverage(&self, lane: RetrievalLane) -> Option<&TaggedSeedCoverage> {
        match lane {
            RetrievalLane::Primary => self.lanes.primary.seed_coverage.as_ref(),
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RetrievalMetadataError {
    #[error(
        "invalid {field}: value is {bytes} bytes; maximum is {maximum}, and it must be nonempty, trimmed, and control-free"
    )]
    InvalidStampText {
        field: &'static str,
        bytes: usize,
        maximum: usize,
    },
    #[error("invalid embedding fingerprint: {0}")]
    InvalidEmbeddingFingerprint(String),
    #[error("retrieval stamp has {provided} projection watermarks; maximum is {maximum}")]
    TooManyProjectionWatermarks { provided: usize, maximum: usize },
    #[error("retrieval stamp repeats projection {projection:?}")]
    DuplicateProjection { projection: String },
    #[error("untagged retrieval must not carry a tag-membership watermark")]
    UnexpectedTagMembershipWatermark,
    #[error("tagged retrieval is missing its verified tag-membership watermark")]
    MissingTagMembershipWatermark,
    #[error("invalid tagged work counters: {0}")]
    InvalidTaggedWork(String),
    #[error("invalid {lane:?} seed coverage: {message}")]
    InvalidSeedCoverage {
        lane: RetrievalLane,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct QueryLane {
    seed_coverage: Option<TaggedSeedCoverage>,
    hits: Vec<QueryHit>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct QueryLanes {
    primary: QueryLane,
}

/// Versioned diagnostic query output shared by the CLI and MCP adapters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QueryEnvelope {
    schema: &'static str,
    mode: RetrievalMode,
    lanes: QueryLanes,
    work: Option<TaggedAnnWork>,
    stamp: RetrievalStamp,
    partial: bool,
}

impl QueryEnvelope {
    pub fn new(
        retrieval: RetrievalMetadata,
        primary: Vec<QueryHit>,
    ) -> Result<Self, QueryInputError> {
        validate_query_hits(RetrievalLane::Primary, &primary)?;
        validate_query_seed_coverage(&retrieval, RetrievalLane::Primary, &primary)?;
        let mut ids = HashSet::with_capacity(primary.len());
        for hit in &primary {
            if !ids.insert(hit.id) {
                return Err(QueryInputError::DuplicateNode { id: hit.id });
            }
        }
        let primary_seed_coverage = retrieval.lanes.primary.seed_coverage.clone();
        let presentation_partial = primary.iter().any(QueryHit::presentation_partial);
        Ok(Self {
            schema: QUERY_SCHEMA,
            mode: retrieval.mode,
            lanes: QueryLanes {
                primary: QueryLane {
                    seed_coverage: primary_seed_coverage,
                    hits: primary,
                },
            },
            work: retrieval.work,
            stamp: retrieval.stamp,
            partial: retrieval.partial || presentation_partial,
        })
    }

    pub const fn schema(&self) -> &str {
        self.schema
    }

    pub const fn partial(&self) -> bool {
        self.partial
    }
}

fn validate_query_hits(lane: RetrievalLane, hits: &[QueryHit]) -> Result<(), QueryInputError> {
    if hits.len() > MAX_QUERY_HITS_PER_LANE {
        return Err(QueryInputError::TooManyHits {
            lane,
            provided: hits.len(),
            maximum: MAX_QUERY_HITS_PER_LANE,
        });
    }
    for hit in hits {
        hit.body.validate()?;
    }
    for pair in hits.windows(2) {
        if pair[0].lane_rank >= pair[1].lane_rank {
            return Err(QueryInputError::RanksNotStrictlyIncreasing {
                lane,
                previous: pair[0].lane_rank,
                next: pair[1].lane_rank,
            });
        }
    }
    Ok(())
}

fn validate_query_seed_coverage(
    retrieval: &RetrievalMetadata,
    lane: RetrievalLane,
    hits: &[QueryHit],
) -> Result<(), QueryInputError> {
    if retrieval.mode() == RetrievalMode::Tagged
        && !hits.is_empty()
        && retrieval.seed_coverage(lane).is_none()
    {
        return Err(QueryInputError::MissingTaggedSeedCoverage { lane });
    }
    Ok(())
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum QueryInputError {
    #[error("query body range is internally inconsistent")]
    InvalidBodyRange,
    #[error("{lane:?} query lane has {provided} hits; maximum is {maximum}")]
    TooManyHits {
        lane: RetrievalLane,
        provided: usize,
        maximum: usize,
    },
    #[error("tagged {lane:?} query hits have no seed_coverage statement")]
    MissingTaggedSeedCoverage { lane: RetrievalLane },
    #[error(
        "{lane:?} query ranks must be strictly increasing, but {previous} was followed by {next}"
    )]
    RanksNotStrictlyIncreasing {
        lane: RetrievalLane,
        previous: NonZeroU16,
        next: NonZeroU16,
    },
    #[error("node {id:?} occurs more than once across query lanes")]
    DuplicateNode { id: NodeId },
}

/// A retrieval lane in the packed context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Core,
    Primary,
    Expansion,
    Episodic,
}

impl Lane {
    const ALL: [Self; 4] = [Self::Core, Self::Primary, Self::Expansion, Self::Episodic];

    // Episodes retain their bounded target, not a transferred probation slot.
    const TARGET_ORDER: [Self; 4] = [Self::Episodic, Self::Core, Self::Primary, Self::Expansion];

    const fn index(self) -> usize {
        match self {
            Self::Core => 0,
            Self::Primary => 1,
            Self::Expansion => 2,
            Self::Episodic => 3,
        }
    }
}

/// Validated per-lane item and encoded-card-payload limits.
///
/// `max_bytes` counts the exact compact JSON bytes of cards plus commas between
/// them. The enclosing `[` and `]` belong to the envelope rather than a lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneLimit {
    hard_min_items: u16,
    target_items: u16,
    max_items: u16,
    max_bytes: u32,
}

impl LaneLimit {
    pub fn new(
        hard_min_items: u16,
        target_items: u16,
        max_items: u16,
        max_bytes: u32,
    ) -> Result<Self, LaneLimitError> {
        if hard_min_items > target_items || target_items > max_items {
            return Err(LaneLimitError::InvalidItemOrder {
                hard_min_items,
                target_items,
                max_items,
            });
        }
        Ok(Self {
            hard_min_items,
            target_items,
            max_items,
            max_bytes,
        })
    }

    pub const fn hard_min_items(self) -> u16 {
        self.hard_min_items
    }

    pub const fn target_items(self) -> u16 {
        self.target_items
    }

    pub const fn max_items(self) -> u16 {
        self.max_items
    }

    pub const fn max_bytes(self) -> u32 {
        self.max_bytes
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum LaneLimitError {
    #[error(
        "lane item limits must satisfy hard_min_items <= target_items <= max_items (got {hard_min_items} <= {target_items} <= {max_items})"
    )]
    InvalidItemOrder {
        hard_min_items: u16,
        target_items: u16,
        max_items: u16,
    },
}

/// Four independently bounded context lanes. Episodic presentation is opt-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneBudgets {
    core: LaneLimit,
    primary: LaneLimit,
    expansion: LaneLimit,
    episodic: LaneLimit,
}

impl LaneBudgets {
    pub const fn new(core: LaneLimit, primary: LaneLimit, expansion: LaneLimit) -> Self {
        Self {
            core,
            primary,
            expansion,
            episodic: LaneLimit {
                hard_min_items: 0,
                target_items: 0,
                max_items: 0,
                max_bytes: 0,
            },
        }
    }

    pub const fn with_episodic(mut self, limit: LaneLimit) -> Self {
        self.episodic = limit;
        self
    }

    pub const fn episodic(self) -> LaneLimit {
        self.episodic
    }

    pub const fn get(self, lane: Lane) -> LaneLimit {
        match lane {
            Lane::Core => self.core,
            Lane::Primary => self.primary,
            Lane::Expansion => self.expansion,
            Lane::Episodic => self.episodic,
        }
    }

    pub const fn core(self) -> LaneLimit {
        self.core
    }

    pub const fn primary(self) -> LaneLimit {
        self.primary
    }

    pub const fn expansion(self) -> LaneLimit {
        self.expansion
    }
}

/// Body work is represented in the presentation budget, but this packer emits
/// metadata cards without fetching bodies. A non-disabled body budget does not
/// change the packer's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyBudget {
    max_fetches: u16,
    max_source_bytes_total: u32,
    max_rendered_bytes_total: u32,
    max_source_bytes_each: u32,
}

impl BodyBudget {
    pub const fn disabled() -> Self {
        Self {
            max_fetches: 0,
            max_source_bytes_total: 0,
            max_rendered_bytes_total: 0,
            max_source_bytes_each: 0,
        }
    }

    pub fn new(
        max_fetches: u16,
        max_source_bytes_total: u32,
        max_rendered_bytes_total: u32,
        max_source_bytes_each: u32,
    ) -> Result<Self, BodyBudgetError> {
        if max_fetches == 0 {
            if max_source_bytes_total != 0
                || max_rendered_bytes_total != 0
                || max_source_bytes_each != 0
            {
                return Err(BodyBudgetError::DisabledHasNonzeroBytes);
            }
        } else if max_source_bytes_each == 0
            || max_source_bytes_total == 0
            || max_rendered_bytes_total == 0
            || max_source_bytes_each > max_source_bytes_total
        {
            return Err(BodyBudgetError::InvalidEnabledBudget);
        }
        Ok(Self {
            max_fetches,
            max_source_bytes_total,
            max_rendered_bytes_total,
            max_source_bytes_each,
        })
    }

    pub const fn max_fetches(self) -> u16 {
        self.max_fetches
    }

    pub const fn max_source_bytes_total(self) -> u32 {
        self.max_source_bytes_total
    }

    pub const fn max_rendered_bytes_total(self) -> u32 {
        self.max_rendered_bytes_total
    }

    pub const fn max_source_bytes_each(self) -> u32 {
        self.max_source_bytes_each
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum BodyBudgetError {
    #[error("a disabled body budget must have zero byte allowances")]
    DisabledHasNonzeroBytes,
    #[error(
        "an enabled body budget needs nonzero totals and max_source_bytes_each <= max_source_bytes_total"
    )]
    InvalidEnabledBudget,
}

/// A construction-validated aggregate presentation budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentationBudget {
    max_content_bytes: NonZeroU32,
    control_reserve_bytes: NonZeroU32,
    max_items: NonZeroU16,
    max_summary_bytes_each: NonZeroU16,
    body: BodyBudget,
    lanes: LaneBudgets,
}

impl PresentationBudget {
    pub fn new(
        max_content_bytes: NonZeroU32,
        control_reserve_bytes: NonZeroU32,
        max_items: NonZeroU16,
        max_summary_bytes_each: NonZeroU16,
        body: BodyBudget,
        lanes: LaneBudgets,
    ) -> Result<Self, BudgetError> {
        let minimum_control = minimum_control_reserve_bytes();
        if control_reserve_bytes.get() < minimum_control {
            return Err(BudgetError::ControlReserveTooSmall {
                provided: control_reserve_bytes.get(),
                required: minimum_control,
            });
        }
        let Some(normal_limit) = max_content_bytes
            .get()
            .checked_sub(control_reserve_bytes.get())
        else {
            return Err(BudgetError::ControlReserveConsumesContent);
        };
        if normal_limit == 0 {
            return Err(BudgetError::ControlReserveConsumesContent);
        }

        let hard_items: u32 = Lane::ALL
            .into_iter()
            .map(|lane| u32::from(lanes.get(lane).hard_min_items))
            .sum();
        if hard_items > u32::from(max_items.get()) {
            return Err(BudgetError::HardMinItemsExceedGlobal {
                hard_items,
                max_items: max_items.get(),
            });
        }

        let worst = WireCard {
            id: NodeId(ulid::Ulid::from(u128::MAX)),
            rank: NonZeroU16::MAX,
            summary: SummaryText {
                text: String::new(),
                complete: false,
                source_bytes: u32::MAX,
            },
            content_trust: ContentTrust::Untrusted,
            episode: None,
            touchstone: None,
        };
        let mut payload_bytes = [0_u32; 4];
        let mut selected = [0_u32; 4];
        for lane in Lane::ALL {
            let count = u32::from(lanes.get(lane).hard_min_items);
            let mut lane_worst = worst.clone();
            if lane == Lane::Episodic {
                lane_worst.episode = Some(worst_episode_metadata());
            }
            let worst_len = compact_len(&lane_worst);
            let payload = repeated_payload_bytes(worst_len, count)
                .expect("u16 hard minima and bounded card skeleton fit u32");
            if payload > lanes.get(lane).max_bytes {
                return Err(BudgetError::LaneHardMinBytesTooSmall {
                    lane,
                    required: payload,
                    provided: lanes.get(lane).max_bytes,
                });
            }
            payload_bytes[lane.index()] = payload;
            selected[lane.index()] = count;
        }

        // Prove baseline hard-minimum reserves against the largest omission
        // counters and unknown tails, not merely the empty happy path. Origin
        // unions and retrieval/reference-work metadata are input-dependent;
        // `pack` must recheck their full footprint for every actual hard minimum.
        let omissions = std::array::from_fn(|_| LaneOmissions {
            bounded_window_budget: u32::MAX,
            further_tail_unknown: true,
        });
        let minimum_retrieval = minimum_retrieval_metadata();
        let required = exact_content_len(
            max_content_bytes.get(),
            control_reserve_bytes.get(),
            selected,
            payload_bytes,
            omissions,
            &minimum_retrieval,
            &EpisodicRetrieval::searched(false, false),
            &EpisodeReferenceRetrieval::not_searched(),
            &TouchstoneRetrieval::default(),
            false,
        );
        if required > normal_limit {
            return Err(BudgetError::MandatoryEnvelopeTooLarge {
                required,
                available: normal_limit,
            });
        }

        Ok(Self {
            max_content_bytes,
            control_reserve_bytes,
            max_items,
            max_summary_bytes_each,
            body,
            lanes,
        })
    }

    pub fn minimum_control_reserve_bytes() -> u32 {
        minimum_control_reserve_bytes()
    }

    pub const fn max_content_bytes(self) -> u32 {
        self.max_content_bytes.get()
    }

    pub const fn control_reserve_bytes(self) -> u32 {
        self.control_reserve_bytes.get()
    }

    pub const fn normal_content_limit(self) -> u32 {
        self.max_content_bytes.get() - self.control_reserve_bytes.get()
    }

    pub const fn max_items(self) -> u16 {
        self.max_items.get()
    }

    pub const fn max_summary_bytes_each(self) -> u16 {
        self.max_summary_bytes_each.get()
    }

    pub const fn body(self) -> BodyBudget {
        self.body
    }

    pub const fn lanes(self) -> LaneBudgets {
        self.lanes
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum BudgetError {
    #[error("control reserve is {provided} bytes but needs at least {required}")]
    ControlReserveTooSmall { provided: u32, required: u32 },
    #[error("control reserve leaves no room for a success envelope")]
    ControlReserveConsumesContent,
    #[error("hard minima require {hard_items} items but the global maximum is {max_items}")]
    HardMinItemsExceedGlobal { hard_items: u32, max_items: u16 },
    #[error(
        "{lane:?} hard-minimum card skeletons need {required} lane bytes but only {provided} are available"
    )]
    LaneHardMinBytesTooSmall {
        lane: Lane,
        required: u32,
        provided: u32,
    },
    #[error(
        "the fixed envelope and baseline hard-minimum reserves need {required} normal bytes but only {available} are available"
    )]
    MandatoryEnvelopeTooLarge { required: u32, available: u32 },
}

macro_rules! input_card {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct $name(SourceCard);

        impl $name {
            pub fn new(
                id: NodeId,
                rank: NonZeroU16,
                summary: impl Into<String>,
            ) -> Result<Self, InputError> {
                Ok(Self(SourceCard::new(id, rank, summary.into())?))
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
                self.0.touchstone = Some(view);
                self
            }

            pub fn with_rank(mut self, rank: NonZeroU16) -> Self {
                self.0.rank = rank;
                self
            }
        }
    };
}

input_card!(CoreInputCard);
input_card!(PrimaryInputCard);
input_card!(ExpansionInputCard);

/// Why an owner could not run an episodic search. Other backend errors are not
/// absence and must be propagated by the caller rather than encoded here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodicUnavailableReason {
    AdapterUnsupported,
    StoreNotUpgraded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodicRetrievalState {
    Searched,
    NotSearched,
    NotSearchedTagFilter,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EpisodicRetrievalMode {
    Lexical,
}

/// A checked statement about the independent episodic retrieval attempt.
/// Search coverage belongs to the lane window, not a synthetic semantic score.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EpisodicRetrieval {
    state: EpisodicRetrievalState,
    mode: EpisodicRetrievalMode,
    cue_normalized: bool,
    cue_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_reason: Option<EpisodicUnavailableReason>,
}

impl EpisodicRetrieval {
    pub const fn searched(cue_normalized: bool, cue_truncated: bool) -> Self {
        Self {
            state: EpisodicRetrievalState::Searched,
            mode: EpisodicRetrievalMode::Lexical,
            cue_normalized,
            cue_truncated,
            unavailable_reason: None,
        }
    }

    /// For manual callers which did not attempt episodic retrieval. The shared
    /// application path replaces this with a more specific outcome.
    pub const fn not_searched() -> Self {
        Self {
            state: EpisodicRetrievalState::NotSearched,
            ..Self::searched(false, false)
        }
    }

    pub const fn not_searched_tag_filter() -> Self {
        Self {
            state: EpisodicRetrievalState::NotSearchedTagFilter,
            ..Self::searched(false, false)
        }
    }

    pub const fn unavailable(reason: EpisodicUnavailableReason) -> Self {
        Self {
            state: EpisodicRetrievalState::Unavailable,
            unavailable_reason: Some(reason),
            ..Self::searched(false, false)
        }
    }

    pub const fn state(&self) -> EpisodicRetrievalState {
        self.state
    }

    pub const fn cue_normalized(&self) -> bool {
        self.cue_normalized
    }

    pub const fn cue_truncated(&self) -> bool {
        self.cue_truncated
    }

    pub const fn unavailable_reason(&self) -> Option<EpisodicUnavailableReason> {
        self.unavailable_reason
    }

    pub const fn partial(&self) -> bool {
        !matches!(self.state, EpisodicRetrievalState::Searched)
            || self.cue_normalized
            || self.cue_truncated
    }
}

/// Actual reference-discovery work, separate from lexical coverage and packing.
/// Endpoint reads count exact-header operations (bounded node/head overhead),
/// not literal disk I/O. Graph point-read counters come from the native adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeReferenceCounts {
    pub anchors_total: u32,
    pub anchors_examined: u32,
    pub raw_edges_scanned: u32,
    pub raw_edge_limit: u32,
    pub endpoint_reads: u32,
    pub endpoint_read_limit: u32,
    pub indexed_seeks: u32,
    pub edge_point_reads: u32,
    pub body_anchor_point_reads: u32,
    pub missing: u32,
    pub non_episode: u32,
    pub cache_hits: u32,
    pub unread_anchors: u32,
    pub further_tail_unknown: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeReferenceStopReason {
    Budget,
    Deadline,
    Unsupported,
    ReadError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeReferenceState {
    NotSearched,
    Searched,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "EpisodeReferenceRetrievalWire")]
pub struct EpisodeReferenceRetrieval {
    state: EpisodeReferenceState,
    #[serde(flatten)]
    counts: EpisodeReferenceCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<EpisodeReferenceStopReason>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EpisodeReferenceRetrievalWire {
    state: EpisodeReferenceState,
    anchors_total: u32,
    anchors_examined: u32,
    raw_edges_scanned: u32,
    raw_edge_limit: u32,
    endpoint_reads: u32,
    endpoint_read_limit: u32,
    indexed_seeks: u32,
    edge_point_reads: u32,
    body_anchor_point_reads: u32,
    missing: u32,
    non_episode: u32,
    cache_hits: u32,
    unread_anchors: u32,
    further_tail_unknown: bool,
    stop_reason: Option<EpisodeReferenceStopReason>,
}

impl TryFrom<EpisodeReferenceRetrievalWire> for EpisodeReferenceRetrieval {
    type Error = InputError;
    fn try_from(w: EpisodeReferenceRetrievalWire) -> Result<Self, Self::Error> {
        let value = Self {
            state: w.state,
            counts: EpisodeReferenceCounts {
                anchors_total: w.anchors_total,
                anchors_examined: w.anchors_examined,
                raw_edges_scanned: w.raw_edges_scanned,
                raw_edge_limit: w.raw_edge_limit,
                endpoint_reads: w.endpoint_reads,
                endpoint_read_limit: w.endpoint_read_limit,
                indexed_seeks: w.indexed_seeks,
                edge_point_reads: w.edge_point_reads,
                body_anchor_point_reads: w.body_anchor_point_reads,
                missing: w.missing,
                non_episode: w.non_episode,
                cache_hits: w.cache_hits,
                unread_anchors: w.unread_anchors,
                further_tail_unknown: w.further_tail_unknown,
            },
            stop_reason: w.stop_reason,
        };
        value.validate()?;
        Ok(value)
    }
}

impl EpisodeReferenceRetrieval {
    pub fn not_searched() -> Self {
        Self {
            state: EpisodeReferenceState::NotSearched,
            counts: EpisodeReferenceCounts::default(),
            stop_reason: None,
        }
    }
    pub fn searched(
        counts: EpisodeReferenceCounts,
        stop_reason: Option<EpisodeReferenceStopReason>,
    ) -> Result<Self, InputError> {
        let value = Self {
            state: EpisodeReferenceState::Searched,
            counts,
            stop_reason,
        };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<(), InputError> {
        let c = self.counts;
        if c.anchors_examined > c.anchors_total
            || c.unread_anchors != c.anchors_total - c.anchors_examined
            || c.raw_edge_limit > 512
            || c.endpoint_read_limit > 256
            || c.raw_edges_scanned > c.raw_edge_limit
            || c.endpoint_reads > c.endpoint_read_limit
            || u64::from(c.missing) + u64::from(c.non_episode) > u64::from(c.endpoint_reads)
            || (self.state == EpisodeReferenceState::NotSearched
                && (c != EpisodeReferenceCounts::default() || self.stop_reason.is_some()))
        {
            return Err(InputError::InvalidEpisodeReferenceCoverage);
        }
        Ok(())
    }
    pub const fn state(&self) -> EpisodeReferenceState {
        self.state
    }
    pub const fn counts(&self) -> &EpisodeReferenceCounts {
        &self.counts
    }
    pub const fn stop_reason(&self) -> Option<EpisodeReferenceStopReason> {
        self.stop_reason
    }
    pub const fn partial(&self) -> bool {
        matches!(self.state, EpisodeReferenceState::Searched)
            && (self.stop_reason.is_some()
                || self.counts.unread_anchors != 0
                || self.counts.further_tail_unknown)
    }
}

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
pub struct EpisodeInputCard(SourceCard);

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
        let mut source = SourceCard::new(
            header.identity.edition_id,
            rank,
            header.summary.as_str().to_owned(),
        )?;
        source.episode = Some(EpisodeCardMetadata {
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
        });
        Ok(Self(source))
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
        self.0
            .episode
            .as_ref()
            .expect("episode input metadata")
            .identity
    }
    pub fn origins(&self) -> &[EpisodeOrigin] {
        &self
            .0
            .episode
            .as_ref()
            .expect("episode input metadata")
            .origins
    }
    pub fn header(&self) -> EpisodeHeader {
        let m = self.0.episode.as_ref().expect("episode input metadata");
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
        self.0
            .episode
            .as_mut()
            .expect("episode input metadata")
            .merge_origins(other.0.episode.as_ref().expect("episode input metadata"))
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
fn worst_episode_metadata() -> EpisodeCardMetadata {
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
fn maximum_occurrence_contexts() -> OccurrenceContexts {
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct SourceCard {
    id: NodeId,
    rank: NonZeroU16,
    summary: String,
    source_bytes: u32,
    episode: Option<EpisodeCardMetadata>,
    touchstone: Option<TouchstoneView>,
}

impl SourceCard {
    fn new(id: NodeId, rank: NonZeroU16, summary: String) -> Result<Self, InputError> {
        let source_bytes = u32::try_from(summary.len()).map_err(|_| InputError::SummaryTooLarge)?;
        Ok(Self {
            id,
            rank,
            summary,
            source_bytes,
            episode: None,
            touchstone: None,
        })
    }

    fn skeleton(&self) -> WireCard {
        WireCard {
            id: self.id,
            rank: self.rank,
            summary: SummaryText {
                text: String::new(),
                complete: self.summary.is_empty(),
                source_bytes: self.source_bytes,
            },
            content_trust: ContentTrust::Untrusted,
            episode: self.episode.clone(),
            touchstone: self.touchstone.clone(),
        }
    }
}

/// A bounded input window. `further_tail_unknown` distinguishes known omissions
/// from results outside the caller's considered window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneWindow<T> {
    cards: Vec<T>,
    further_tail_unknown: bool,
}

impl<T> LaneWindow<T> {
    pub fn complete(cards: Vec<T>) -> Self {
        Self {
            cards,
            further_tail_unknown: false,
        }
    }

    pub fn bounded(cards: Vec<T>, further_tail_unknown: bool) -> Self {
        Self {
            cards,
            further_tail_unknown,
        }
    }

    pub fn cards(&self) -> &[T] {
        &self.cards
    }

    pub const fn further_tail_unknown(&self) -> bool {
        self.further_tail_unknown
    }
}

/// Typed, rank-validated input to one presentation attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackingInput {
    retrieval: RetrievalMetadata,
    episodic_retrieval: EpisodicRetrieval,
    episode_reference_retrieval: EpisodeReferenceRetrieval,
    touchstone_retrieval: TouchstoneRetrieval,
    lanes: [Vec<SourceCard>; 4],
    unknown_tail: [bool; 4],
}

impl PackingInput {
    pub fn new(
        retrieval: RetrievalMetadata,
        core: LaneWindow<CoreInputCard>,
        primary: LaneWindow<PrimaryInputCard>,
        expansion: LaneWindow<ExpansionInputCard>,
    ) -> Result<Self, InputError> {
        for (lane, has_cards) in [(RetrievalLane::Primary, !primary.cards.is_empty())] {
            if retrieval.mode() == RetrievalMode::Tagged
                && has_cards
                && retrieval.seed_coverage(lane).is_none()
            {
                return Err(InputError::MissingTaggedSeedCoverage { lane });
            }
        }
        let unknown_tail = [
            core.further_tail_unknown,
            primary.further_tail_unknown,
            expansion.further_tail_unknown,
            false,
        ];
        let lanes = [
            core.cards.into_iter().map(|card| card.0).collect(),
            primary.cards.into_iter().map(|card| card.0).collect(),
            expansion.cards.into_iter().map(|card| card.0).collect(),
            Vec::new(),
        ];
        validate_input(&lanes)?;
        Ok(Self {
            retrieval,
            episodic_retrieval: EpisodicRetrieval::not_searched(),
            episode_reference_retrieval: EpisodeReferenceRetrieval::not_searched(),
            touchstone_retrieval: TouchstoneRetrieval::default(),
            lanes,
            unknown_tail,
        })
    }

    pub fn from_complete(
        retrieval: RetrievalMetadata,
        core: Vec<CoreInputCard>,
        primary: Vec<PrimaryInputCard>,
        expansion: Vec<ExpansionInputCard>,
    ) -> Result<Self, InputError> {
        Self::new(
            retrieval,
            LaneWindow::complete(core),
            LaneWindow::complete(primary),
            LaneWindow::complete(expansion),
        )
    }

    pub const fn retrieval(&self) -> &RetrievalMetadata {
        &self.retrieval
    }

    pub fn with_episodic(
        mut self,
        retrieval: EpisodicRetrieval,
        episodes: LaneWindow<EpisodeInputCard>,
    ) -> Result<Self, InputError> {
        if retrieval.state != EpisodicRetrievalState::Searched
            && (episodes
                .cards
                .iter()
                .any(|card| card.origins().contains(&EpisodeOrigin::Lexical))
                || (episodes.cards.is_empty() && episodes.further_tail_unknown))
        {
            return Err(InputError::EpisodeCardsWithoutSearch);
        }
        self.unknown_tail[Lane::Episodic.index()] = episodes.further_tail_unknown;
        self.lanes[Lane::Episodic.index()] =
            episodes.cards.into_iter().map(|card| card.0).collect();
        self.episodic_retrieval = retrieval;
        validate_input(&self.lanes)?;
        Ok(self)
    }

    pub fn with_episode_reference_retrieval(
        mut self,
        retrieval: EpisodeReferenceRetrieval,
    ) -> Result<Self, InputError> {
        retrieval.validate()?;
        self.episode_reference_retrieval = retrieval;
        Ok(self)
    }

    pub const fn episodic_retrieval(&self) -> &EpisodicRetrieval {
        &self.episodic_retrieval
    }

    pub fn with_touchstone_retrieval(
        mut self,
        retrieval: TouchstoneRetrieval,
    ) -> Result<Self, InputError> {
        retrieval.validate()?;
        self.touchstone_retrieval = retrieval;
        Ok(self)
    }

    pub const fn touchstone_retrieval(&self) -> &TouchstoneRetrieval {
        &self.touchstone_retrieval
    }

    pub const fn episode_reference_retrieval(&self) -> &EpisodeReferenceRetrieval {
        &self.episode_reference_retrieval
    }
}

fn validate_input(lanes: &[Vec<SourceCard>; 4]) -> Result<(), InputError> {
    let mut ids = HashSet::new();
    for lane in Lane::ALL {
        let cards = &lanes[lane.index()];
        if cards.len() > u32::MAX as usize {
            return Err(InputError::ConsideredWindowTooLarge { lane });
        }
        for pair in cards.windows(2) {
            if pair[0].rank >= pair[1].rank {
                return Err(InputError::RanksNotStrictlyIncreasing {
                    lane,
                    previous: pair[0].rank,
                    next: pair[1].rank,
                });
            }
        }
        for card in cards {
            if !ids.insert(card.id) {
                return Err(InputError::DuplicateNode { id: card.id });
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum InputError {
    #[error("summary exceeds the u32 source-byte contract")]
    SummaryTooLarge,
    #[error("invalid touchstone view: {0}")]
    InvalidTouchstone(&'static str),
    #[error("invalid episodic card: {0}")]
    InvalidEpisode(&'static str),
    #[error(
        "an unsearched lexical lane cannot contain lexical origins or an unexplained result tail"
    )]
    EpisodeCardsWithoutSearch,
    #[error("invalid episode reference-work coverage")]
    InvalidEpisodeReferenceCoverage,
    #[error("{lane:?} considered window exceeds u32::MAX cards")]
    ConsideredWindowTooLarge { lane: Lane },
    #[error("{lane:?} ranks must be strictly increasing, but {previous} was followed by {next}")]
    RanksNotStrictlyIncreasing {
        lane: Lane,
        previous: NonZeroU16,
        next: NonZeroU16,
    },
    #[error("node {id:?} occurs more than once across presentation lanes")]
    DuplicateNode { id: NodeId },
    #[error("tagged {lane:?} presentation cards have no seed_coverage statement")]
    MissingTaggedSeedCoverage { lane: RetrievalLane },
}

/// UTF-8 summary text with an explicit source-length/completeness contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SummaryText {
    text: String,
    complete: bool,
    source_bytes: u32,
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
enum ContentTrust {
    Untrusted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct WireCard {
    id: NodeId,
    rank: NonZeroU16,
    summary: SummaryText,
    content_trust: ContentTrust,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    episode: Option<EpisodeCardMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    touchstone: Option<TouchstoneView>,
}

macro_rules! output_card {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
        #[serde(transparent)]
        pub struct $name(WireCard);

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct LaneOmissions {
    /// Cards inside the supplied bounded window that did not survive packing.
    bounded_window_budget: u32,
    /// There may be more results beyond the supplied bounded window.
    further_tail_unknown: bool,
}

impl LaneOmissions {
    pub const fn bounded_window_budget(self) -> u32 {
        self.bounded_window_budget
    }

    pub const fn further_tail_unknown(self) -> bool {
        self.further_tail_unknown
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct OmittedCounts {
    core: LaneOmissions,
    primary: LaneOmissions,
    expansion: LaneOmissions,
    episodic: LaneOmissions,
}

impl OmittedCounts {
    pub const fn get(self, lane: Lane) -> LaneOmissions {
        match lane {
            Lane::Core => self.core,
            Lane::Primary => self.primary,
            Lane::Expansion => self.expansion,
            Lane::Episodic => self.episodic,
        }
    }

    const fn any(self) -> bool {
        self.core.bounded_window_budget != 0
            || self.core.further_tail_unknown
            || self.primary.bounded_window_budget != 0
            || self.primary.further_tail_unknown
            || self.expansion.bounded_window_budget != 0
            || self.expansion.further_tail_unknown
            || self.episodic.bounded_window_budget != 0
            || self.episodic.further_tail_unknown
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LaneUsage {
    items: u16,
    /// Exact compact card bytes plus inter-card commas, excluding brackets.
    bytes: u32,
}

impl LaneUsage {
    pub const fn items(self) -> u16 {
        self.items
    }

    pub const fn bytes(self) -> u32 {
        self.bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ContextUsage {
    content_bytes: u32,
    normal_content_limit: u32,
    control_reserve_bytes: u32,
    items: u16,
    core: LaneUsage,
    primary: LaneUsage,
    expansion: LaneUsage,
    episodic: LaneUsage,
}

impl ContextUsage {
    pub const fn content_bytes(self) -> u32 {
        self.content_bytes
    }

    pub const fn normal_content_limit(self) -> u32 {
        self.normal_content_limit
    }

    pub const fn control_reserve_bytes(self) -> u32 {
        self.control_reserve_bytes
    }

    pub const fn items(self) -> u16 {
        self.items
    }

    pub const fn lane(self, lane: Lane) -> LaneUsage {
        match lane {
            Lane::Core => self.core,
            Lane::Primary => self.primary,
            Lane::Expansion => self.expansion,
            Lane::Episodic => self.episodic,
        }
    }
}

/// The success envelope cannot be constructed outside this crate. Callers can
/// inspect it, but only [`pack`] can create one and serialize its exact bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContextEnvelope {
    schema: &'static str,
    retrieval: RetrievalMetadata,
    core: Vec<NodeCard>,
    primary: Vec<HitCard>,
    expansions: Vec<ExpansionCard>,
    episodes: Vec<EpisodeCard>,
    episodic_retrieval: EpisodicRetrieval,
    episode_reference_retrieval: EpisodeReferenceRetrieval,
    touchstone_retrieval: TouchstoneRetrieval,
    receipt: Option<String>,
    partial: bool,
    omitted: OmittedCounts,
    usage: ContextUsage,
}

impl ContextEnvelope {
    pub const fn schema(&self) -> &str {
        self.schema
    }

    pub fn core(&self) -> &[NodeCard] {
        &self.core
    }

    pub const fn retrieval(&self) -> &RetrievalMetadata {
        &self.retrieval
    }

    pub fn primary(&self) -> &[HitCard] {
        &self.primary
    }

    pub fn expansions(&self) -> &[ExpansionCard] {
        &self.expansions
    }

    pub fn episodes(&self) -> &[EpisodeCard] {
        &self.episodes
    }

    pub const fn episodic_retrieval(&self) -> &EpisodicRetrieval {
        &self.episodic_retrieval
    }

    pub const fn episode_reference_retrieval(&self) -> &EpisodeReferenceRetrieval {
        &self.episode_reference_retrieval
    }

    pub const fn touchstone_retrieval(&self) -> &TouchstoneRetrieval {
        &self.touchstone_retrieval
    }

    /// Transitional plans cannot issue feedback authority.
    pub const fn receipt(&self) -> Option<&str> {
        None
    }

    /// True for incomplete or skipped retrieval, normalized/truncated episode
    /// cues, window omissions, or any emitted summary prefix shorter than its
    /// source (including the per-card summary cap).
    pub const fn partial(&self) -> bool {
        self.partial
    }

    pub const fn omitted(&self) -> OmittedCounts {
        self.omitted
    }

    pub const fn usage(&self) -> ContextUsage {
        self.usage
    }
}

/// One exact rendered-card identity in an emitted manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ManifestCard {
    node_id: NodeId,
    lane: Lane,
    rank: NonZeroU16,
    card_sha256: String,
}

impl ManifestCard {
    pub const fn node_id(&self) -> NodeId {
        self.node_id
    }

    pub const fn lane(&self) -> Lane {
        self.lane
    }

    pub const fn rank(&self) -> NonZeroU16 {
        self.rank
    }

    pub fn card_sha256(&self) -> &str {
        &self.card_sha256
    }
}

/// Audit manifest for the exact packed cards. It deliberately carries no
/// lifecycle binding, principal binding, receipt ID, or feedback operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EmittedManifest {
    schema: &'static str,
    cards: Vec<ManifestCard>,
}

impl EmittedManifest {
    pub const fn schema(&self) -> &str {
        self.schema
    }

    pub fn cards(&self) -> &[ManifestCard] {
        &self.cards
    }
}

/// A validated success envelope, its exact compact content bytes, and its
/// non-authorizing emitted-card manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackPlan {
    envelope: ContextEnvelope,
    rendered_content: String,
    manifest: EmittedManifest,
}

impl PackPlan {
    pub const fn envelope(&self) -> &ContextEnvelope {
        &self.envelope
    }

    pub fn rendered_content(&self) -> &str {
        &self.rendered_content
    }

    pub const fn manifest(&self) -> &EmittedManifest {
        &self.manifest
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlError {
    rendered_content: String,
    required_bytes: u32,
    available_bytes: u32,
}

impl ControlError {
    pub fn rendered_content(&self) -> &str {
        &self.rendered_content
    }

    pub const fn required_bytes(&self) -> u32 {
        self.required_bytes
    }

    pub const fn available_bytes(&self) -> u32 {
        self.available_bytes
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PackError {
    #[error("invalid presentation input: {0}")]
    InvalidInput(InputError),
    #[error("runtime mandatory cards do not fit the validated presentation budget")]
    BudgetTooSmall(ControlError),
}

/// Pack typed retrieval lanes into one exact compact JSON success envelope.
///
/// Admission pays for actual capped summary text, not just card skeletons.
/// Optional semantic cards fit their whole capped summary and retain rank prefixes.
/// Optional episodes retain their whole metadata/origins, but an oversized card
/// is omitted without hiding later feasible accounts. Source ranks stay intact;
/// hard minima and the episodic target first reserve a nonempty
/// UTF-8 scalar together, then share the remaining text budget.
/// A mandatory nonempty source that cannot emit even one scalar is a budget
/// error, not a successful content-free card. The envelope's `partial` bit also
/// covers emitted summaries whose `complete` flag is false.
pub fn pack(budget: &PresentationBudget, input: &PackingInput) -> Result<PackPlan, PackError> {
    // Builders may attach cards before discovery coverage (or vice versa).
    // Validate the final immutable window here, before either emission or
    // whole-card omission can conceal contradictory navigation provenance.
    input
        .episode_reference_retrieval
        .validate()
        .map_err(PackError::InvalidInput)?;
    input
        .touchstone_retrieval
        .validate()
        .map_err(PackError::InvalidInput)?;
    for lane in Lane::ALL {
        for source in &input.lanes[lane.index()] {
            if source.touchstone.is_some() {
                if lane == Lane::Episodic || !input.touchstone_retrieval.searched {
                    return Err(PackError::InvalidInput(InputError::InvalidTouchstone(
                        "unsearched or episodic touchstone owner",
                    )));
                }
                validate_touchstone_card_metadata(
                    &serde_json::to_value(source.skeleton()).expect("typed card encodes"),
                )
                .map_err(PackError::InvalidInput)?;
            }
        }
    }
    for source in &input.lanes[Lane::Episodic.index()] {
        let metadata = source
            .episode
            .as_ref()
            .expect("typed episodic input metadata");
        for origin in &metadata.origins {
            match origin {
                EpisodeOrigin::Lexical
                    if input.episodic_retrieval.state() != EpisodicRetrievalState::Searched
                        || metadata.identity.edition_id != metadata.current_edition_id =>
                {
                    return Err(PackError::InvalidInput(
                        InputError::EpisodeCardsWithoutSearch,
                    ));
                }
                EpisodeOrigin::Reference { .. }
                    if input.episode_reference_retrieval.state()
                        != EpisodeReferenceState::Searched =>
                {
                    return Err(PackError::InvalidInput(InputError::InvalidEpisode(
                        "reference origins require searched reference coverage",
                    )));
                }
                _ => {}
            }
        }
    }
    let mut state = PackingState::new(input);

    // Retrieval metadata is mandatory protocol data. Refuse the success path
    // before card selection if the exact v7 envelope cannot carry it; it is
    // never dropped or emitted through an out-of-budget diagnostic side
    // channel.
    if state.exact_content_len(*budget, &input.retrieval) > budget.normal_content_limit() {
        return Err(runtime_budget_error(*budget, &state, &input.retrieval));
    }

    // Reserve every hard minimum before *any* one card can consume the text
    // budget. Construction proves only fixed-header baseline reserves. Runtime
    // must carry the complete origin union, session and actual coverage, plus
    // at least one UTF-8 scalar for a nonempty summary.
    for lane in Lane::ALL {
        let wanted =
            usize::from(budget.lanes.get(lane).hard_min_items).min(input.lanes[lane.index()].len());
        while state.selected[lane.index()] < wanted {
            if !state.try_add_next(*budget, input, lane, true) {
                return Err(runtime_budget_error(*budget, &state, &input.retrieval));
            }
        }
    }

    // Reserve the episodic target alongside hard minima before
    // summary growth. Long primary text cannot starve a feasible episode.
    for lane in [Lane::Episodic] {
        let wanted =
            usize::from(budget.lanes.get(lane).target_items).min(input.lanes[lane.index()].len());
        while state.selected[lane.index()] < wanted
            && state.next_source[lane.index()] < input.lanes[lane.index()].len()
        {
            if !state.try_add_next(*budget, input, lane, true) {
                // Soft episodic reservation is not a prefix relevance claim.
                // Keep metadata atomic and give later smaller accounts a turn.
                state.next_source[lane.index()] += 1;
            }
        }
    }

    // Grow the jointly reserved cards before admitting optional cards. A
    // common byte cap avoids giving the first hard minimum all available text;
    // remaining space then goes in stable lane/rank order.
    state.grow_reserved(*budget, input);

    // Other soft targets need their entire capped summaries.
    for lane in Lane::TARGET_ORDER {
        if lane == Lane::Episodic {
            continue;
        }
        let wanted =
            usize::from(budget.lanes.get(lane).target_items).min(input.lanes[lane.index()].len());
        while state.selected[lane.index()] < wanted {
            if !state.try_add_next(*budget, input, lane, false) {
                break;
            }
        }
    }

    // Semantic lanes preserve prefixes. Episode lanes preserve source order
    // among emitted cards, but skip complete oversized accounts atomically.
    for lane in Lane::ALL {
        while state.next_source[lane.index()] < input.lanes[lane.index()].len() {
            if !state.try_add_next(*budget, input, lane, false) {
                if lane != Lane::Episodic {
                    break;
                }
                state.next_source[lane.index()] += 1;
            }
        }
    }

    let omitted = omitted_struct(state.omissions());
    let usage = state.usage(*budget);
    let mut envelope = ContextEnvelope {
        schema: CONTEXT_SCHEMA,
        retrieval: input.retrieval.clone(),
        core: state.cards[Lane::Core.index()]
            .iter()
            .cloned()
            .map(NodeCard)
            .collect(),
        primary: state.cards[Lane::Primary.index()]
            .iter()
            .cloned()
            .map(HitCard)
            .collect(),
        expansions: state.cards[Lane::Expansion.index()]
            .iter()
            .cloned()
            .map(ExpansionCard)
            .collect(),
        episodes: state.cards[Lane::Episodic.index()]
            .iter()
            .cloned()
            .map(EpisodeCard)
            .collect(),
        episodic_retrieval: input.episodic_retrieval.clone(),
        episode_reference_retrieval: input.episode_reference_retrieval.clone(),
        touchstone_retrieval: input.touchstone_retrieval.clone(),
        receipt: None,
        partial: omitted.any()
            || input.retrieval.partial()
            || input.episodic_retrieval.partial()
            || input.episode_reference_retrieval.partial()
            || input.touchstone_retrieval.partial()
            || state.summary_partial(),
        omitted,
        usage,
    };
    let rendered = encode_envelope_exact(&mut envelope);
    debug_assert_eq!(rendered.len(), envelope.usage.content_bytes as usize);
    debug_assert!(envelope.usage.content_bytes <= budget.normal_content_limit());
    debug_assert_eq!(
        rendered.len() as u32,
        state.exact_content_len(*budget, &input.retrieval)
    );

    let manifest = manifest_for(&envelope);
    Ok(PackPlan {
        envelope,
        rendered_content: String::from_utf8(rendered)
            .expect("serde_json always renders valid UTF-8"),
        manifest,
    })
}

#[derive(Clone, Debug)]
struct PackingState {
    cards: [Vec<WireCard>; 4],
    /// Actual input indices of admitted cards; episodic omission is non-prefix.
    source_indices: [Vec<usize>; 4],
    next_source: [usize; 4],
    selected: [usize; 4],
    payload_bytes: [u32; 4],
    input_len: [u32; 4],
    unknown_tail: [bool; 4],
    episodic_retrieval: EpisodicRetrieval,
    episode_reference_retrieval: EpisodeReferenceRetrieval,
    touchstone_retrieval: TouchstoneRetrieval,
}

impl PackingState {
    fn new(input: &PackingInput) -> Self {
        Self {
            cards: std::array::from_fn(|_| Vec::new()),
            source_indices: std::array::from_fn(|_| Vec::new()),
            next_source: [0; 4],
            selected: [0; 4],
            payload_bytes: [0; 4],
            input_len: std::array::from_fn(|index| {
                u32::try_from(input.lanes[index].len()).expect("input validated as u32")
            }),
            unknown_tail: input.unknown_tail,
            episodic_retrieval: input.episodic_retrieval.clone(),
            episode_reference_retrieval: input.episode_reference_retrieval.clone(),
            touchstone_retrieval: input.touchstone_retrieval.clone(),
        }
    }

    fn try_add_next(
        &mut self,
        budget: PresentationBudget,
        input: &PackingInput,
        lane: Lane,
        reserve_minimal: bool,
    ) -> bool {
        let index = lane.index();
        if self.next_source[index] >= input.lanes[index].len()
            || self.selected[index] >= usize::from(budget.lanes.get(lane).max_items)
            || self.total_selected() >= usize::from(budget.max_items())
        {
            return false;
        }
        let source = &input.lanes[index][self.next_source[index]];
        let boundaries = prefix_boundaries(
            &source.summary,
            usize::from(budget.max_summary_bytes_each()),
        );
        let prefix_len = if reserve_minimal && !source.summary.is_empty() {
            let Some(&first_scalar) = boundaries.get(1) else {
                return false;
            };
            first_scalar
        } else {
            *boundaries.last().expect("prefix boundaries include zero")
        };
        let chosen = self.candidate_with_prefix(budget, input, lane, prefix_len);
        let Some((card, payload_bytes)) = chosen else {
            return false;
        };
        self.cards[index].push(card);
        self.source_indices[index].push(self.next_source[index]);
        self.next_source[index] += 1;
        self.selected[index] += 1;
        self.payload_bytes[index] = payload_bytes;
        true
    }

    fn grow_reserved(&mut self, budget: PresentationBudget, input: &PackingInput) {
        if self.total_selected() == 0 {
            return;
        }
        // Find the largest common byte cap feasible for every reserved card.
        // The initial one-scalar representation is always retained if the
        // shortest common cap would lengthen an ASCII card too far to fit.
        let minimum_cap = Lane::ALL
            .iter()
            .flat_map(|lane| {
                self.source_indices[lane.index()]
                    .iter()
                    .filter_map(move |&source_index| {
                        input.lanes[lane.index()][source_index]
                            .summary
                            .chars()
                            .next()
                            .map(char::len_utf8)
                    })
            })
            .max()
            .unwrap_or(0);
        let mut low = minimum_cap;
        let mut high = usize::from(budget.max_summary_bytes_each()) + 1;
        let mut best = self.clone();
        while low < high {
            let middle = low + (high - low) / 2;
            match self.with_reserved_cap(budget, input, middle) {
                Some(candidate) => {
                    best = candidate;
                    low = middle + 1;
                }
                None => high = middle,
            }
        }
        *self = best;

        // A common cap can leave slack when a short note is complete or JSON
        // escaping differs. Spend that slack in stable lane/source-rank order.
        for lane in Lane::ALL {
            for card_index in 0..self.selected[lane.index()] {
                self.maximize_reserved(budget, input, lane, card_index);
            }
        }
    }

    fn with_reserved_cap(
        &self,
        budget: PresentationBudget,
        input: &PackingInput,
        cap: usize,
    ) -> Option<Self> {
        let mut trial = self.clone();
        for lane in Lane::ALL {
            for card_index in 0..self.selected[lane.index()] {
                let source =
                    &input.lanes[lane.index()][self.source_indices[lane.index()][card_index]];
                let boundaries = prefix_boundaries(&source.summary, cap);
                let prefix_len = *boundaries.last()?;
                if !source.summary.is_empty() && prefix_len == 0 {
                    return None;
                }
                trial.cards[lane.index()][card_index].summary = SummaryText {
                    text: source.summary[..prefix_len].to_owned(),
                    complete: prefix_len == source.summary.len(),
                    source_bytes: source.source_bytes,
                };
            }
            trial.recount_lane(budget, lane)?;
        }
        (trial.exact_content_len(budget, &input.retrieval) <= budget.normal_content_limit())
            .then_some(trial)
    }

    fn maximize_reserved(
        &mut self,
        budget: PresentationBudget,
        input: &PackingInput,
        lane: Lane,
        card_index: usize,
    ) {
        let source = &input.lanes[lane.index()][self.source_indices[lane.index()][card_index]];
        if source.summary.is_empty() {
            return;
        }
        let boundaries = prefix_boundaries(
            &source.summary,
            usize::from(budget.max_summary_bytes_each()),
        );
        let current = self.cards[lane.index()][card_index].summary.text.len();
        let mut low = boundaries.partition_point(|&n| n <= current);
        let mut high = boundaries.len();
        let mut best = None;
        while low < high {
            let middle = low + (high - low) / 2;
            let mut trial = self.clone();
            let prefix_len = boundaries[middle];
            trial.cards[lane.index()][card_index].summary = SummaryText {
                text: source.summary[..prefix_len].to_owned(),
                complete: prefix_len == source.summary.len(),
                source_bytes: source.source_bytes,
            };
            if trial.recount_lane(budget, lane).is_some()
                && trial.exact_content_len(budget, &input.retrieval)
                    <= budget.normal_content_limit()
            {
                best = Some(trial);
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if let Some(trial) = best {
            *self = trial;
        }
    }

    fn recount_lane(&mut self, budget: PresentationBudget, lane: Lane) -> Option<()> {
        let mut bytes = 0;
        for card in &self.cards[lane.index()] {
            bytes = append_payload_bytes(bytes, compact_len(card))?;
        }
        if bytes > budget.lanes.get(lane).max_bytes {
            return None;
        }
        self.payload_bytes[lane.index()] = bytes;
        Some(())
    }

    fn candidate_with_prefix(
        &self,
        budget: PresentationBudget,
        input: &PackingInput,
        lane: Lane,
        prefix_len: usize,
    ) -> Option<(WireCard, u32)> {
        let index = lane.index();
        let source = &input.lanes[index][self.next_source[index]];
        if !source.summary.is_empty() && prefix_len == 0 {
            return None;
        }
        let mut card = source.skeleton();
        card.summary = SummaryText {
            text: source.summary[..prefix_len].to_owned(),
            complete: prefix_len == source.summary.len(),
            source_bytes: source.source_bytes,
        };
        let payload_bytes = append_payload_bytes(self.payload_bytes[index], compact_len(&card))?;
        if payload_bytes > budget.lanes.get(lane).max_bytes {
            return None;
        }
        let mut projected_selected = self.selected_u32();
        projected_selected[index] += 1;
        let mut projected_payload = self.payload_bytes;
        projected_payload[index] = payload_bytes;
        let projected_omissions =
            omissions_from(self.input_len, projected_selected, self.unknown_tail);
        let projected = exact_content_len(
            budget.max_content_bytes(),
            budget.control_reserve_bytes(),
            projected_selected,
            projected_payload,
            projected_omissions,
            &input.retrieval,
            &input.episodic_retrieval,
            &input.episode_reference_retrieval,
            &input.touchstone_retrieval,
            self.summary_partial() || !card.summary.complete,
        );
        (projected <= budget.normal_content_limit()).then_some((card, payload_bytes))
    }

    fn summary_partial(&self) -> bool {
        self.cards.iter().flatten().any(|card| {
            !card.summary.complete
                || card
                    .touchstone
                    .as_ref()
                    .is_some_and(TouchstoneView::partial)
        })
    }

    fn total_selected(&self) -> usize {
        self.selected.iter().sum()
    }

    fn selected_u32(&self) -> [u32; 4] {
        std::array::from_fn(|index| self.selected[index] as u32)
    }

    fn omissions(&self) -> [LaneOmissions; 4] {
        omissions_from(self.input_len, self.selected_u32(), self.unknown_tail)
    }

    fn usage(&self, budget: PresentationBudget) -> ContextUsage {
        usage_from(
            budget.max_content_bytes(),
            budget.control_reserve_bytes(),
            self.selected_u32(),
            self.payload_bytes,
            0,
        )
    }

    fn exact_content_len(&self, budget: PresentationBudget, retrieval: &RetrievalMetadata) -> u32 {
        exact_content_len(
            budget.max_content_bytes(),
            budget.control_reserve_bytes(),
            self.selected_u32(),
            self.payload_bytes,
            self.omissions(),
            retrieval,
            &self.episodic_retrieval,
            &self.episode_reference_retrieval,
            &self.touchstone_retrieval,
            self.summary_partial(),
        )
    }
}

fn prefix_boundaries(source: &str, max_bytes: usize) -> Vec<usize> {
    let cap = source.len().min(max_bytes);
    let mut boundaries = vec![0];
    boundaries.extend(
        source
            .char_indices()
            .map(|(index, _)| index)
            .filter(|index| *index != 0 && *index <= cap),
    );
    if (cap == source.len() || source.is_char_boundary(cap))
        && boundaries.last().copied() != Some(cap)
    {
        boundaries.push(cap);
    }
    boundaries
}

fn runtime_budget_error(
    budget: PresentationBudget,
    state: &PackingState,
    retrieval: &RetrievalMetadata,
) -> PackError {
    let required = state
        .exact_content_len(budget, retrieval)
        .max(budget.normal_content_limit().saturating_add(1));
    let available = budget.normal_content_limit();
    let control = ControlEnvelope {
        schema: CONTEXT_SCHEMA,
        error: "budget_too_small",
        required_bytes: required,
        available_bytes: available,
    };
    let rendered = serde_json::to_string(&control).expect("fixed control envelope serializes");
    debug_assert!(rendered.len() as u32 <= budget.control_reserve_bytes());
    PackError::BudgetTooSmall(ControlError {
        rendered_content: rendered,
        required_bytes: required,
        available_bytes: available,
    })
}

fn manifest_for(envelope: &ContextEnvelope) -> EmittedManifest {
    let mut cards = Vec::with_capacity(usize::from(envelope.usage.items));
    for card in &envelope.core {
        cards.push(manifest_card(Lane::Core, &card.0));
    }
    for card in &envelope.primary {
        cards.push(manifest_card(Lane::Primary, &card.0));
    }
    for card in &envelope.expansions {
        cards.push(manifest_card(Lane::Expansion, &card.0));
    }
    for card in &envelope.episodes {
        cards.push(manifest_card(Lane::Episodic, &card.0));
    }
    EmittedManifest {
        schema: CONTEXT_SCHEMA,
        cards,
    }
}

fn manifest_card(lane: Lane, card: &WireCard) -> ManifestCard {
    let encoded = serde_json::to_vec(card).expect("fixed card serializes");
    let digest = Sha256::digest(encoded);
    ManifestCard {
        node_id: card.id,
        lane,
        rank: card.rank,
        card_sha256: hex_digest(&digest),
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

#[derive(Serialize)]
struct EnvelopeShell<'a> {
    schema: &'static str,
    retrieval: &'a RetrievalMetadata,
    core: [(); 0],
    primary: [(); 0],
    expansions: [(); 0],
    episodes: [(); 0],
    episodic_retrieval: &'a EpisodicRetrieval,
    episode_reference_retrieval: &'a EpisodeReferenceRetrieval,
    touchstone_retrieval: &'a TouchstoneRetrieval,
    receipt: Option<String>,
    partial: bool,
    omitted: OmittedCounts,
    usage: ContextUsage,
}

fn exact_content_len(
    max_content_bytes: u32,
    control_reserve_bytes: u32,
    selected: [u32; 4],
    payload_bytes: [u32; 4],
    omissions: [LaneOmissions; 4],
    retrieval: &RetrievalMetadata,
    episodic_retrieval: &EpisodicRetrieval,
    episode_reference_retrieval: &EpisodeReferenceRetrieval,
    touchstone_retrieval: &TouchstoneRetrieval,
    summary_partial: bool,
) -> u32 {
    let payload_total: u64 = payload_bytes.iter().copied().map(u64::from).sum();
    let mut content_bytes = 0;
    for _ in 0..16 {
        let usage = usage_from(
            max_content_bytes,
            control_reserve_bytes,
            selected,
            payload_bytes,
            content_bytes,
        );
        let omitted = omitted_struct(omissions);
        let shell = EnvelopeShell {
            schema: CONTEXT_SCHEMA,
            retrieval,
            core: [],
            primary: [],
            expansions: [],
            episodes: [],
            episodic_retrieval,
            episode_reference_retrieval,
            touchstone_retrieval,
            receipt: None,
            partial: omitted.any()
                || retrieval.partial()
                || episodic_retrieval.partial()
                || episode_reference_retrieval.partial()
                || touchstone_retrieval.partial()
                || summary_partial,
            omitted,
            usage,
        };
        let next =
            u32::try_from(u64::from(compact_len(&shell)) + payload_total).unwrap_or(u32::MAX);
        if next == content_bytes {
            return next;
        }
        content_bytes = next;
    }
    unreachable!("content byte fixed point converges after at most two digit changes")
}

fn usage_from(
    max_content_bytes: u32,
    control_reserve_bytes: u32,
    selected: [u32; 4],
    payload_bytes: [u32; 4],
    content_bytes: u32,
) -> ContextUsage {
    let lane_usage: [LaneUsage; 4] = std::array::from_fn(|index| LaneUsage {
        items: u16::try_from(selected[index]).expect("global u16 item budget enforced"),
        bytes: payload_bytes[index],
    });
    ContextUsage {
        content_bytes,
        normal_content_limit: max_content_bytes - control_reserve_bytes,
        control_reserve_bytes,
        items: u16::try_from(selected.iter().copied().sum::<u32>())
            .expect("global u16 item budget enforced"),
        core: lane_usage[0],
        primary: lane_usage[1],
        expansion: lane_usage[2],
        episodic: lane_usage[3],
    }
}

fn omissions_from(
    input_len: [u32; 4],
    selected: [u32; 4],
    unknown_tail: [bool; 4],
) -> [LaneOmissions; 4] {
    std::array::from_fn(|index| LaneOmissions {
        bounded_window_budget: input_len[index] - selected[index],
        further_tail_unknown: unknown_tail[index],
    })
}

const fn omitted_struct(lanes: [LaneOmissions; 4]) -> OmittedCounts {
    OmittedCounts {
        core: lanes[0],
        primary: lanes[1],
        expansion: lanes[2],
        episodic: lanes[3],
    }
}

fn encode_envelope_exact(envelope: &mut ContextEnvelope) -> Vec<u8> {
    for _ in 0..16 {
        let encoded = serde_json::to_vec(envelope).expect("fixed success envelope serializes");
        let length = u32::try_from(encoded.len()).expect("validated u32 content budget");
        if envelope.usage.content_bytes == length {
            return encoded;
        }
        envelope.usage.content_bytes = length;
    }
    unreachable!("content byte fixed point converges after at most two digit changes")
}

fn compact_len(value: &impl Serialize) -> u32 {
    u32::try_from(
        serde_json::to_vec(value)
            .expect("fixed presentation type serializes")
            .len(),
    )
    .expect("presentation component exceeds u32")
}

fn append_payload_bytes(current: u32, card_len: u32) -> Option<u32> {
    current
        .checked_add(u32::from(current != 0))?
        .checked_add(card_len)
}

fn repeated_payload_bytes(card_len: u32, count: u32) -> Option<u32> {
    if count == 0 {
        Some(0)
    } else {
        card_len
            .checked_mul(count)?
            .checked_add(count.saturating_sub(1))
    }
}

#[derive(Serialize)]
struct ControlEnvelope {
    schema: &'static str,
    error: &'static str,
    required_bytes: u32,
    available_bytes: u32,
}

fn minimum_control_reserve_bytes() -> u32 {
    compact_len(&ControlEnvelope {
        schema: CONTEXT_SCHEMA,
        error: "budget_too_small",
        required_bytes: u32::MAX,
        available_bytes: u32::MAX,
    })
}

fn minimum_retrieval_metadata() -> RetrievalMetadata {
    let stamp = RetrievalStamp::new(
        "x",
        [0; 32],
        EmbeddingFingerprint::new("x", 1, "x", "x"),
        "x",
        None,
        None,
        Vec::new(),
    )
    .expect("fixed minimum retrieval stamp is valid");
    RetrievalMetadata::untagged(stamp).expect("fixed minimum retrieval metadata is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u128) -> NodeId {
        NodeId(ulid::Ulid::from(value))
    }

    fn rank(value: u16) -> NonZeroU16 {
        NonZeroU16::new(value).unwrap()
    }

    fn episode_header(contexts: Option<OccurrenceContexts>) -> EpisodeHeader {
        use mneme_core::episode::{EpisodeId, EpisodeRevision};
        EpisodeHeader {
            identity: EpisodeIdentity {
                episode_id: EpisodeId::new(id(7)),
                edition_id: id(7),
                revision: EpisodeRevision::new(0),
            },
            summary: mneme_core::NodeSummary::new("violet\\\"🦋".repeat(200)).unwrap(),
            occurred: OccurrenceSpan::Unknown,
            recorded_at: EpisodeTime::new(42).unwrap(),
            edition_recorded_at: EpisodeTime::new(42).unwrap(),
            thread: Some(EpisodeThread::new("historical thread").unwrap()),
            occurrence_contexts: contexts,
            recording_session: None,
            current_edition_id: id(7),
        }
    }

    fn episodic_budget(lane_bytes: u32) -> PresentationBudget {
        let disabled = LaneLimit::new(0, 0, 0, 0).unwrap();
        PresentationBudget::new(
            NonZeroU32::new(32 * 1024).unwrap(),
            NonZeroU32::new(PresentationBudget::minimum_control_reserve_bytes()).unwrap(),
            NonZeroU16::new(1).unwrap(),
            NonZeroU16::new(64).unwrap(),
            BodyBudget::disabled(),
            LaneBudgets::new(disabled, disabled, disabled)
                .with_episodic(LaneLimit::new(0, 1, 1, lane_bytes).unwrap()),
        )
        .unwrap()
    }

    fn episode_input(header: EpisodeHeader) -> PackingInput {
        PackingInput::from_complete(minimum_retrieval_metadata(), vec![], vec![], vec![])
            .unwrap()
            .with_episodic(
                EpisodicRetrieval::searched(false, false),
                LaneWindow::complete(vec![EpisodeInputCard::new(header, rank(1)).unwrap()]),
            )
            .unwrap()
    }

    fn reference_card(header: EpisodeHeader, anchor_id: NodeId) -> EpisodeInputCard {
        let mut edge = Edge::new(
            header.identity.edition_id,
            anchor_id,
            0.3,
            EdgeKind::DerivedFrom,
            42,
        );
        edge.anchor = Some(BodySpan::new(2, 11));
        EpisodeInputCard::referenced(
            header,
            rank(1),
            EpisodeAnchor::Semantic { node_id: anchor_id },
            &edge,
        )
        .unwrap()
    }

    fn reference_coverage(card: &EpisodeInputCard) -> EpisodeReferenceRetrieval {
        let references = card
            .origins()
            .iter()
            .filter_map(|origin| match origin {
                EpisodeOrigin::Reference { anchor, .. } => Some(anchor.node_id()),
                EpisodeOrigin::Lexical => None,
            })
            .collect::<Vec<_>>();
        let anchors = references.iter().copied().collect::<HashSet<_>>().len() as u32;
        let rows = references.len() as u32;
        EpisodeReferenceRetrieval::searched(
            EpisodeReferenceCounts {
                anchors_total: anchors,
                anchors_examined: anchors,
                raw_edges_scanned: rows,
                raw_edge_limit: 512,
                endpoint_reads: 1,
                endpoint_read_limit: 256,
                indexed_seeks: anchors * 2,
                edge_point_reads: rows,
                body_anchor_point_reads: rows,
                cache_hits: rows.saturating_sub(1),
                ..EpisodeReferenceCounts::default()
            },
            None,
        )
        .unwrap()
    }

    fn reference_input(card: EpisodeInputCard) -> PackingInput {
        let coverage = reference_coverage(&card);
        PackingInput::from_complete(minimum_retrieval_metadata(), vec![], vec![], vec![])
            .unwrap()
            .with_episodic(
                EpisodicRetrieval::not_searched(),
                LaneWindow::complete(vec![card]),
            )
            .unwrap()
            .with_episode_reference_retrieval(coverage)
            .unwrap()
    }

    #[test]
    fn final_pack_rejects_reference_origin_without_reference_coverage_in_either_builder_order() {
        let card = reference_card(episode_header(None), id(1));
        let input =
            PackingInput::from_complete(minimum_retrieval_metadata(), vec![], vec![], vec![])
                .unwrap()
                .with_episodic(
                    EpisodicRetrieval::not_searched(),
                    LaneWindow::complete(vec![card.clone()]),
                )
                .unwrap();
        assert!(matches!(
            pack(&episodic_budget(4096), &input),
            Err(PackError::InvalidInput(_))
        ));
        let explicit_absence = input
            .clone()
            .with_episode_reference_retrieval(EpisodeReferenceRetrieval::not_searched())
            .unwrap();
        assert!(matches!(
            pack(&episodic_budget(4096), &explicit_absence),
            Err(PackError::InvalidInput(_))
        ));
        let searched = reference_coverage(&card);
        let cards_first = input
            .with_episode_reference_retrieval(searched.clone())
            .unwrap();
        let coverage_first =
            PackingInput::from_complete(minimum_retrieval_metadata(), vec![], vec![], vec![])
                .unwrap()
                .with_episode_reference_retrieval(searched)
                .unwrap()
                .with_episodic(
                    EpisodicRetrieval::not_searched(),
                    LaneWindow::complete(vec![card]),
                )
                .unwrap();
        assert_eq!(
            pack(&episodic_budget(4096), &cards_first).unwrap(),
            pack(&episodic_budget(4096), &coverage_first).unwrap()
        );
    }

    #[test]
    fn oversized_optional_episode_does_not_hide_a_later_small_scene_or_corrupt_its_rank() {
        use mneme_core::episode::EpisodeId;
        let header = episode_header(None);
        let mut oversized = reference_card(header.clone(), id(100));
        for anchor in 101..612 {
            oversized
                .merge_origins(&reference_card(header.clone(), id(anchor)))
                .unwrap();
        }
        let coverage = reference_coverage(&oversized);
        let mut small_header = episode_header(None);
        small_header.identity.episode_id = EpisodeId::new(id(9));
        small_header.identity.edition_id = id(9);
        small_header.current_edition_id = id(9);
        small_header.summary = NodeSummary::new("small later scene 🦋").unwrap();
        let small = reference_card(small_header, id(1)).with_rank(rank(2));
        let input =
            PackingInput::from_complete(minimum_retrieval_metadata(), vec![], vec![], vec![])
                .unwrap()
                .with_episodic(
                    EpisodicRetrieval::not_searched(),
                    LaneWindow::complete(vec![oversized, small.clone()]),
                )
                .unwrap()
                .with_episode_reference_retrieval(coverage)
                .unwrap();
        let budget = episodic_budget(4096);
        let plan = pack(&budget, &input).unwrap();
        assert_eq!(plan.envelope().episodes().len(), 1);
        assert_eq!(plan.envelope().episodes()[0].id(), small.id());
        assert_eq!(plan.envelope().episodes()[0].rank(), rank(2));
        assert_eq!(
            plan.envelope().episodes()[0].summary().text(),
            small.summary()
        );
        assert!(plan.envelope().episodes()[0].summary().complete());
        assert_eq!(plan.manifest().cards()[0].node_id(), small.id());
        assert_eq!(plan.manifest().cards()[0].rank(), rank(2));
        let value: serde_json::Value = serde_json::from_str(plan.rendered_content()).unwrap();
        assert_eq!(value["omitted"]["episodic"]["bounded_window_budget"], 1);
        assert_eq!(
            value["usage"]["content_bytes"],
            plan.rendered_content().len()
        );
        assert_eq!(
            value["usage"]["episodic"]["bytes"],
            serde_json::to_vec(&value["episodes"][0]).unwrap().len()
        );
        assert_eq!(
            value["episodes"][0]["origins"],
            serde_json::to_value(small.origins()).unwrap()
        );
        let disabled = LaneLimit::new(0, 0, 0, 0).unwrap();
        let hard = PresentationBudget::new(
            NonZeroU32::new(32 * 1024).unwrap(),
            NonZeroU32::new(PresentationBudget::minimum_control_reserve_bytes()).unwrap(),
            NonZeroU16::new(1).unwrap(),
            NonZeroU16::new(64).unwrap(),
            BodyBudget::disabled(),
            LaneBudgets::new(disabled, disabled, disabled)
                .with_episodic(LaneLimit::new(1, 1, 1, 4096).unwrap()),
        )
        .unwrap();
        assert!(matches!(
            pack(&hard, &input),
            Err(PackError::BudgetTooSmall(_))
        ));
    }

    #[test]
    fn exact_reference_admits_history_but_never_retargets_to_observed_head() {
        let mut header = episode_header(Some(maximum_occurrence_contexts()));
        header.current_edition_id = id(8);
        header.recording_session = Some(EpisodeRecordingSession::new("Pi\\\"🦋").unwrap());
        assert!(EpisodeInputCard::new(header.clone(), rank(1)).is_err());
        let wrong = Edge::new(id(1), id(8), 0.4, EdgeKind::Associative, 42);
        assert!(
            EpisodeInputCard::referenced(
                header.clone(),
                rank(1),
                EpisodeAnchor::Semantic { node_id: id(1) },
                &wrong
            )
            .is_err()
        );
        let card = reference_card(header.clone(), id(1));
        assert_eq!(card.header().identity, header.identity);
        assert_eq!(card.header().current_edition_id, id(8));
        let plan = pack(&episodic_budget(4096), &reference_input(card)).unwrap();
        let value: serde_json::Value = serde_json::from_str(plan.rendered_content()).unwrap();
        let episode = &value["episodes"][0];
        assert_eq!(episode["edition_id"], serde_json::to_value(id(7)).unwrap());
        assert_eq!(
            episode["current_edition_id"],
            serde_json::to_value(id(8)).unwrap()
        );
        assert_eq!(episode["recording_session"], "Pi\\\"🦋");
        assert_eq!(episode["origins"][0]["from"], episode["edition_id"]);
        assert_eq!(
            episode["origins"][0]["body_anchor"],
            serde_json::json!({"start":2,"end":11})
        );
        assert!(episode["origins"][0].get("weight").is_none());
        assert!(episode["origins"][0].get("trials").is_none());
        validate_episode_card_metadata(episode, false).unwrap();
        assert_eq!(value["episodic_retrieval"]["state"], "not_searched");
        assert_eq!(value["episode_reference_retrieval"]["state"], "searched");
        assert_eq!(
            value["usage"]["content_bytes"],
            plan.rendered_content().len()
        );
    }

    #[test]
    fn origins_union_canonical_without_changing_first_head_or_account() {
        let header = episode_header(None);
        let mut lexical = EpisodeInputCard::new(header.clone(), rank(1)).unwrap();
        let a = reference_card(header.clone(), id(1));
        let mut later = header.clone();
        later.current_edition_id = id(8);
        let b = reference_card(later, id(2));
        lexical.merge_origins(&b).unwrap();
        lexical.merge_origins(&a).unwrap();
        lexical.merge_origins(&a).unwrap();
        assert_eq!(lexical.origins().len(), 3);
        assert_eq!(lexical.origins()[0], EpisodeOrigin::Lexical);
        assert_eq!(
            lexical.header().current_edition_id,
            header.current_edition_id
        );
        let mut reordered = EpisodeInputCard::new(header.clone(), rank(1)).unwrap();
        reordered.merge_origins(&a).unwrap();
        reordered.merge_origins(&b).unwrap();
        assert_eq!(lexical.origins(), reordered.origins());
        assert_eq!(lexical.clone().with_rank(rank(3)).rank(), rank(3));
        let lexical_only = pack(&episodic_budget(4096), &episode_input(header)).unwrap();
        let reference_coverage = reference_coverage(&lexical);
        let mixed =
            PackingInput::from_complete(minimum_retrieval_metadata(), vec![], vec![], vec![])
                .unwrap()
                .with_episodic(
                    EpisodicRetrieval::searched(false, false),
                    LaneWindow::complete(vec![lexical]),
                )
                .unwrap()
                .with_episode_reference_retrieval(reference_coverage)
                .unwrap();
        let mixed_plan = pack(&episodic_budget(4096), &mixed).unwrap();
        assert_ne!(
            lexical_only.manifest().cards()[0].card_sha256(),
            mixed_plan.manifest().cards()[0].card_sha256()
        );
        assert_eq!(
            lexical_only.envelope().episodes()[0].episode().identity(),
            mixed_plan.envelope().episodes()[0].episode().identity()
        );
        assert!(
            PackingInput::from_complete(minimum_retrieval_metadata(), vec![], vec![], vec![])
                .unwrap()
                .with_episodic(
                    EpisodicRetrieval::not_searched_tag_filter(),
                    LaneWindow::complete(vec![reordered])
                )
                .is_err()
        );
        let mut changed_account = episode_header(None);
        changed_account.recording_session = Some(EpisodeRecordingSession::new("Mac").unwrap());
        let changed = reference_card(changed_account, id(1));
        let mut original = a;
        assert!(original.merge_origins(&changed).is_err());
    }

    #[test]
    fn reference_origin_and_head_changes_are_delivered_view_not_account_changes() {
        let header = episode_header(None);
        let first = reference_card(header.clone(), id(1));
        let mut later = header;
        later.current_edition_id = id(9);
        let second = reference_card(later, id(1));
        let first_plan = pack(&episodic_budget(4096), &reference_input(first)).unwrap();
        let second_plan = pack(&episodic_budget(4096), &reference_input(second)).unwrap();
        let a = first_plan.envelope().episodes()[0].episode();
        let b = second_plan.envelope().episodes()[0].episode();
        assert_eq!(a.identity(), b.identity());
        assert_eq!(a.occurred(), b.occurred());
        assert_eq!(a.recorded_at(), b.recorded_at());
        assert_eq!(a.recording_session(), b.recording_session());
        assert_ne!(a.current_edition_id(), b.current_edition_id());
        assert_ne!(
            first_plan.manifest().cards()[0].card_sha256(),
            second_plan.manifest().cards()[0].card_sha256()
        );
    }

    #[test]
    fn sparse_complete_and_unknown_reference_work_are_not_the_same_coverage() {
        let sparse = EpisodeReferenceCounts {
            anchors_total: 3,
            anchors_examined: 3,
            raw_edge_limit: 6,
            endpoint_read_limit: 3,
            indexed_seeks: 6,
            ..EpisodeReferenceCounts::default()
        };
        let complete = EpisodeReferenceRetrieval::searched(sparse, None).unwrap();
        assert!(!complete.partial());
        let unread = EpisodeReferenceCounts {
            anchors_examined: 1,
            unread_anchors: 2,
            further_tail_unknown: true,
            ..sparse
        };
        let partial =
            EpisodeReferenceRetrieval::searched(unread, Some(EpisodeReferenceStopReason::Budget))
                .unwrap();
        assert!(partial.partial());
        let json = serde_json::to_value(&partial).unwrap();
        assert_eq!(json["stop_reason"], "budget");
        assert_eq!(
            serde_json::from_value::<EpisodeReferenceRetrieval>(json).unwrap(),
            partial
        );
        assert!(
            EpisodeReferenceRetrieval::searched(
                EpisodeReferenceCounts {
                    endpoint_reads: 4,
                    ..sparse
                },
                None
            )
            .is_err()
        );
        assert!(
            EpisodeReferenceRetrieval::searched(
                EpisodeReferenceCounts {
                    unread_anchors: 1,
                    ..sparse
                },
                None
            )
            .is_err()
        );
    }

    #[test]
    fn large_origin_union_is_not_capped_or_shed_and_actual_hard_minimum_is_checked() {
        let mut header = episode_header(Some(maximum_occurrence_contexts()));
        header.recording_session = Some(EpisodeRecordingSession::new("\"".repeat(512)).unwrap());
        let mut card = reference_card(header.clone(), id(100));
        for anchor in 101..612 {
            card.merge_origins(&reference_card(header.clone(), id(anchor)))
                .unwrap();
        }
        assert_eq!(card.origins().len(), 512);
        let input = reference_input(card);
        let soft = pack(&episodic_budget(32 * 1024), &input).unwrap();
        assert!(soft.envelope().episodes().is_empty());
        assert_eq!(soft.envelope().usage().lane(Lane::Episodic).bytes(), 0);
        let disabled = LaneLimit::new(0, 0, 0, 0).unwrap();
        let hard = PresentationBudget::new(
            NonZeroU32::new(32 * 1024).unwrap(),
            NonZeroU32::new(PresentationBudget::minimum_control_reserve_bytes()).unwrap(),
            NonZeroU16::new(1).unwrap(),
            NonZeroU16::new(64).unwrap(),
            BodyBudget::disabled(),
            LaneBudgets::new(disabled, disabled, disabled)
                .with_episodic(LaneLimit::new(1, 1, 1, 32 * 1024).unwrap()),
        )
        .unwrap();
        assert!(matches!(
            pack(&hard, &input),
            Err(PackError::BudgetTooSmall(_))
        ));
        // A small genuine union retains every escaped metadata field whole.
        let small = reference_input(reference_card(header, id(1)));
        let packed = pack(&hard, &small).unwrap();
        let value: serde_json::Value = serde_json::from_str(packed.rendered_content()).unwrap();
        assert_eq!(
            value["episodes"][0]["recording_session"]
                .as_str()
                .unwrap()
                .len(),
            512
        );
        assert_eq!(
            serde_json::to_vec(&value["episodes"][0]["occurrence_contexts"])
                .unwrap()
                .len(),
            1024
        );
        assert_eq!(
            value["usage"]["content_bytes"],
            packed.rendered_content().len()
        );
        assert_eq!(
            value["usage"]["episodic"]["bytes"],
            serde_json::to_vec(&value["episodes"][0]).unwrap().len()
        );
    }

    #[test]
    fn tagged_semantic_anchor_can_present_an_indirect_scene_without_lexical_tag_claim() {
        let stamp = RetrievalStamp::new(
            "x",
            [0; 32],
            EmbeddingFingerprint::new("x", 1, "x", "x"),
            "x",
            None,
            None,
            vec![(TAG_MEMBERSHIP_PROJECTION.into(), "tag-view".into())],
        )
        .unwrap();
        let retrieval = RetrievalMetadata::tagged(
            stamp,
            TaggedAnnWork {
                query_dimension: 1,
                ..TaggedAnnWork::default()
            },
            TaggedSeedCoverage::ExactCosine,
        )
        .unwrap();
        let card = reference_card(episode_header(None), id(1));
        let reference_coverage = reference_coverage(&card);
        let input = PackingInput::from_complete(
            retrieval,
            vec![],
            vec![PrimaryInputCard::new(id(1), rank(1), "tagged lesson").unwrap()],
            vec![],
        )
        .unwrap()
        .with_episodic(
            EpisodicRetrieval::not_searched_tag_filter(),
            LaneWindow::complete(vec![card]),
        )
        .unwrap()
        .with_episode_reference_retrieval(reference_coverage)
        .unwrap();
        let plan = pack(&episodic_budget(4096), &input).unwrap();
        let value: serde_json::Value = serde_json::from_str(plan.rendered_content()).unwrap();
        assert_eq!(value["retrieval"]["mode"], "tagged");
        assert_eq!(
            value["episodic_retrieval"]["state"],
            "not_searched_tag_filter"
        );
        assert_eq!(
            value["episodes"][0]["origins"][0]["anchor"]["kind"],
            "semantic"
        );
        validate_episode_card_metadata(&value["episodes"][0], false).unwrap();
    }

    #[test]
    fn maximum_occurrence_context_metadata_is_reserved_exactly() {
        let contexts = maximum_occurrence_contexts();
        assert_eq!(
            serde_json::to_vec(&contexts).unwrap().len(),
            MAX_OCCURRENCE_CONTEXTS_JSON_BYTES
        );
        let with = worst_episode_metadata();
        let mut without = with.clone();
        without.occurrence_contexts = None;
        assert_eq!(
            compact_len(&with) - compact_len(&without),
            (b",\"occurrence_contexts\":".len() + MAX_OCCURRENCE_CONTEXTS_JSON_BYTES) as u32,
        );
        let skeleton = WireCard {
            id: NodeId(ulid::Ulid::from(u128::MAX)),
            rank: NonZeroU16::MAX,
            summary: SummaryText {
                text: String::new(),
                complete: false,
                source_bytes: u32::MAX,
            },
            content_trust: ContentTrust::Untrusted,
            episode: Some(with),
            touchstone: None,
        };
        let required = compact_len(&skeleton);
        let disabled = LaneLimit::new(0, 0, 0, 0).unwrap();
        let budget = PresentationBudget::new(
            NonZeroU32::new(32 * 1024).unwrap(),
            NonZeroU32::new(PresentationBudget::minimum_control_reserve_bytes()).unwrap(),
            NonZeroU16::new(1).unwrap(),
            NonZeroU16::new(64).unwrap(),
            BodyBudget::disabled(),
            LaneBudgets::new(disabled, disabled, disabled)
                .with_episodic(LaneLimit::new(1, 1, 1, required - 1).unwrap()),
        );
        assert_eq!(
            budget,
            Err(BudgetError::LaneHardMinBytesTooSmall {
                lane: Lane::Episodic,
                required,
                provided: required - 1,
            })
        );
    }

    #[test]
    fn episodic_packing_preserves_complete_contexts_and_exact_byte_counts() {
        let reference =
            OccurrenceContextRef::new("atelier\\\"🦋", "shared-scene", Some("x")).unwrap();
        let minimum = OccurrenceContexts::new(vec![reference]).unwrap();
        let label = "x".repeat(MAX_OCCURRENCE_CONTEXTS_JSON_BYTES - minimum.compact_json_len() + 1);
        let contexts = OccurrenceContexts::new(vec![
            OccurrenceContextRef::new("atelier\\\"🦋", "shared-scene", Some(&label)).unwrap(),
        ])
        .unwrap();
        assert_eq!(
            contexts.compact_json_len(),
            MAX_OCCURRENCE_CONTEXTS_JSON_BYTES
        );
        let input = episode_input(episode_header(Some(contexts.clone())));
        let plan = pack(&episodic_budget(4096), &input).unwrap();
        let envelope = plan.envelope();
        assert_eq!(envelope.episodes().len(), 1);
        assert_eq!(
            envelope.episodes()[0].episode().occurrence_contexts(),
            Some(&contexts)
        );
        let value: serde_json::Value = serde_json::from_str(plan.rendered_content()).unwrap();
        assert_eq!(
            value["episodes"][0]["occurrence_contexts"],
            serde_json::to_value(contexts).unwrap()
        );
        assert_eq!(
            value["usage"]["episodic"]["bytes"],
            serde_json::to_vec(&value["episodes"][0]).unwrap().len()
        );
        assert_eq!(
            value["usage"]["content_bytes"],
            plan.rendered_content().len()
        );
        assert_eq!(value["episodes"][0]["summary"]["complete"], false);
        assert!(value["episodes"][0].get("body").is_none());
    }

    #[test]
    fn emitted_episode_manifest_binds_occurrence_contexts() {
        let contexts = |key| {
            OccurrenceContexts::new(vec![
                OccurrenceContextRef::new("conversation", key, Some("shared event")).unwrap(),
            ])
            .unwrap()
        };
        let budget = episodic_budget(4096);
        let first = pack(
            &budget,
            &episode_input(episode_header(Some(contexts("first")))),
        )
        .unwrap();
        let second = pack(
            &budget,
            &episode_input(episode_header(Some(contexts("second")))),
        )
        .unwrap();
        assert_eq!(
            first.manifest().cards()[0].node_id(),
            second.manifest().cards()[0].node_id()
        );
        assert_ne!(
            first.manifest().cards()[0].card_sha256(),
            second.manifest().cards()[0].card_sha256()
        );
    }

    #[test]
    fn occurrence_contexts_are_never_shed_to_make_an_episode_fit() {
        let old_header = episode_header(None);
        let mut minimal = EpisodeInputCard::new(old_header.clone(), rank(1))
            .unwrap()
            .0
            .skeleton();
        minimal.summary.text = "v".into();
        let budget = episodic_budget(compact_len(&minimal));
        let old = pack(&budget, &episode_input(old_header)).unwrap();
        assert_eq!(old.envelope().episodes().len(), 1);
        let old_value: serde_json::Value = serde_json::from_str(old.rendered_content()).unwrap();
        assert!(
            old_value["episodes"][0]
                .get("occurrence_contexts")
                .is_none()
        );
        let contextual = pack(
            &budget,
            &episode_input(episode_header(Some(maximum_occurrence_contexts()))),
        )
        .unwrap();
        assert!(contextual.envelope().episodes().is_empty());
        let value: serde_json::Value = serde_json::from_str(contextual.rendered_content()).unwrap();
        assert_eq!(value["omitted"]["episodic"]["bounded_window_budget"], 1);
        assert_eq!(value["partial"], true);
        assert_eq!(
            value["usage"]["content_bytes"],
            contextual.rendered_content().len()
        );
    }

    #[test]
    fn query_v3_has_one_lane_and_no_compatibility_fields() {
        let hit = QueryHit::new(
            id(1),
            rank(1),
            RankEvidence::default(),
            QueryNodeStatus::Active,
            "ordinary",
            false,
            QueryBody::NotRequested,
        );
        let envelope = QueryEnvelope::new(minimum_retrieval_metadata(), vec![hit]).unwrap();
        let value = serde_json::to_value(envelope).unwrap();
        assert_eq!(value["schema"], "mneme.query.v3");
        assert_eq!(value["lanes"].as_object().unwrap().len(), 1);
        assert_eq!(
            value["lanes"]["primary"]["hits"].as_array().unwrap().len(),
            1
        );
        assert!(value["lanes"].get("probationary").is_none());
    }

    #[test]
    fn context_v7_retains_total_limits_without_probation_reservation() {
        let disabled = LaneLimit::new(0, 0, 0, 0).unwrap();
        let primary = LaneLimit::new(1, 12, 12, 30_000).unwrap();
        let budget = PresentationBudget::new(
            NonZeroU32::new(32 * 1024).unwrap(),
            NonZeroU32::new(2 * 1024).unwrap(),
            NonZeroU16::new(13).unwrap(),
            NonZeroU16::new(2048).unwrap(),
            BodyBudget::disabled(),
            LaneBudgets::new(disabled, primary, disabled),
        )
        .unwrap();
        let input = PackingInput::new(
            minimum_retrieval_metadata(),
            LaneWindow::complete(vec![]),
            LaneWindow::complete(vec![
                PrimaryInputCard::new(id(2), rank(1), "ordinary").unwrap(),
            ]),
            LaneWindow::complete(vec![]),
        )
        .unwrap();
        let plan = pack(&budget, &input).unwrap();
        let value: serde_json::Value = serde_json::from_str(plan.rendered_content()).unwrap();
        assert_eq!(value["schema"], "mneme.context.v7");
        for key in ["core", "primary", "expansions", "episodes"] {
            assert!(value[key].is_array(), "missing {key}");
            assert!(value["usage"].get(key).is_some() || key == "expansions" || key == "episodes");
        }
        assert!(value.get("probationary").is_none());
        assert!(value["retrieval"]["lanes"].get("probationary").is_none());
        assert!(value["omitted"].get("probationary").is_none());
        assert!(value["usage"].get("probationary").is_none());
        assert_eq!(
            value["usage"]["content_bytes"],
            plan.rendered_content().len()
        );
    }
}
