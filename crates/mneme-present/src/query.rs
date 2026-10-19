//! Retrieval evidence and diagnostic query output, independent of context
//! packing. Keep the bounded provenance contract shared by both frontends.

use std::collections::HashSet;
use std::num::NonZeroU16;

use mneme_core::tagged::{TaggedAnnWork, TaggedSeedCoverage};
use mneme_core::{EmbeddingFingerprint, NodeId, Timestamp};
use serde::Serialize;
use thiserror::Error;

use crate::{format_timestamp_millis, hex_digest};

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
    /// Node creation in Unix epoch milliseconds, not last verification or update.
    created: Timestamp,
    created_utc: String,
    lane_rank: NonZeroU16,
    evidence: RankEvidence,
    status: QueryNodeStatus,
    summary: String,
    summary_truncated: bool,
    body: QueryBody,
}

impl QueryHit {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: NodeId,
        created: Timestamp,
        lane_rank: NonZeroU16,
        evidence: RankEvidence,
        status: QueryNodeStatus,
        summary: impl Into<String>,
        summary_truncated: bool,
        body: QueryBody,
    ) -> Self {
        Self {
            id,
            created,
            created_utc: format_timestamp_millis(created),
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

pub(super) fn minimum_retrieval_metadata() -> RetrievalMetadata {
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
