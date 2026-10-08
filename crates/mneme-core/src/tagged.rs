//! Bounded tagged-retrieval domain contract.
//!
//! This module owns request normalization, work ceilings, stable sampling keys,
//! strategy/coverage metadata, and adapter-output validation. Storage adapters
//! implement the mechanics through [`crate::ports::VectorIndex`]; no backend
//! representation belongs here.

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use crate::ports::{Error, Result, Scored, StatusFilter};
use crate::{NodeId, NodeStatus, validate_tag};

/// Maximum number of raw tag strings in one tagged ANN request.
pub const MAX_TAGGED_QUERY_TAGS: usize = 32;
/// A query tag uses the canonical tag ceiling so every stored tag is queryable.
pub const MAX_TAGGED_QUERY_TAG_BYTES: usize = crate::MAX_TAG_BYTES;
/// Maximum charged tag-membership rows before exact work falls back.
pub const MAX_TAGGED_RAW_MEMBERSHIPS: usize = 4_096;
/// One additional row may be observed solely to prove exact-work overflow.
pub const MAX_TAGGED_RAW_MEMBERSHIP_READS: usize = MAX_TAGGED_RAW_MEMBERSHIPS + 1;
/// Maximum unique canonical IDs admitted to exact cosine scoring.
pub const MAX_TAGGED_EXACT_IDS: usize = 4_096;
/// Maximum vector components hydrated by the exact tagged path.
pub const MAX_TAGGED_EXACT_VECTOR_COMPONENTS: usize = 4_194_304;
/// Maximum IDs in one exact hydration page.
pub const MAX_TAGGED_EXACT_HYDRATION_PAGE_IDS: usize = 256;
/// Tagged retrieval cannot support a larger vector space within its fallback envelope.
pub const MAX_TAGGED_VECTOR_DIMENSION: usize = 4_096;
/// Maximum visible hits requested from one logical lifecycle lane.
pub const MAX_TAGGED_HITS_PER_LANE: usize = 256;
/// Aggregate HNSW plus hashed-membership fallback candidates across every lane.
pub const MAX_TAGGED_FALLBACK_CANDIDATES: usize = 256;
/// Ordinary retrieval's bounded fallback reserve; retired probationary work
/// is not silently transferred here.
pub const TAGGED_ACTIVE_FALLBACK_CANDIDATES: usize = 192;
/// Maximum unique fallback IDs hydrated after candidate deduplication.
pub const MAX_TAGGED_FALLBACK_HYDRATION_IDS: usize = 256;
/// Maximum vector components hydrated by a tagged fallback.
pub const MAX_TAGGED_FALLBACK_VECTOR_COMPONENTS: usize = 1_048_576;
/// Maximum serialized tag-projection generation identifier.
pub const MAX_TAGGED_PROJECTION_GENERATION_BYTES: usize = 128;

/// Stable logical output-lane identity. The string ID is public protocol data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalLifecycleLane {
    Primary,
}

impl RetrievalLifecycleLane {
    pub const fn as_str(self) -> &'static str {
        "primary"
    }
    const fn digest_byte(self) -> u8 {
        0
    }
}

/// Physical lifecycle projection used by vector and tag indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaggedPhysicalStatus {
    Active,
    Archived,
}

/// Counter-free lifecycle classification projected by hot-path indexes.
pub type NodeLifecycle = TaggedPhysicalStatus;

impl TaggedPhysicalStatus {
    pub const ALL: [Self; 2] = [Self::Active, Self::Archived];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }
    pub const fn from_node_status(status: NodeStatus) -> Self {
        match status {
            NodeStatus::Active => Self::Active,
            NodeStatus::Archived => Self::Archived,
        }
    }
    pub const fn is_admitted_by(self, filter: StatusFilter) -> bool {
        match self {
            Self::Active => filter.active,
            Self::Archived => filter.archived,
        }
    }
    const fn digest_byte(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::Archived => 1,
        }
    }
}

impl From<NodeStatus> for TaggedPhysicalStatus {
    fn from(status: NodeStatus) -> Self {
        Self::from_node_status(status)
    }
}

/// Verified derived tag-projection generation returned with every tagged batch.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TaggedProjectionGeneration(String);

impl TaggedProjectionGeneration {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(Error::InvalidInput(
                "tag projection generation must be nonempty".into(),
            ));
        }
        if value.len() > MAX_TAGGED_PROJECTION_GENERATION_BYTES {
            return Err(Error::InvalidInput(format!(
                "tag projection generation is {} UTF-8 bytes; maximum is {MAX_TAGGED_PROJECTION_GENERATION_BYTES}",
                value.len()
            )));
        }
        if value.trim() != value || value.chars().any(char::is_control) {
            return Err(Error::InvalidInput(
                "tag projection generation must be trimmed and contain no controls".into(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TaggedProjectionGeneration {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Request-tunable work ceilings. Adapters may accept lower values but never
/// enlarge these domain maxima, and must call [`Self::validate`] before work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TaggedAnnWorkLimits {
    max_raw_memberships: usize,
    max_unique_exact_ids: usize,
    max_exact_vector_components: usize,
    exact_hydration_page_ids: usize,
    max_fallback_candidates: usize,
    max_fallback_hydration_ids: usize,
    max_fallback_vector_components: usize,
}

impl TaggedAnnWorkLimits {
    pub fn new(
        max_raw_memberships: usize,
        max_unique_exact_ids: usize,
        max_exact_vector_components: usize,
        exact_hydration_page_ids: usize,
        max_fallback_candidates: usize,
        max_fallback_hydration_ids: usize,
        max_fallback_vector_components: usize,
    ) -> Result<Self> {
        let limits = Self {
            max_raw_memberships,
            max_unique_exact_ids,
            max_exact_vector_components,
            exact_hydration_page_ids,
            max_fallback_candidates,
            max_fallback_hydration_ids,
            max_fallback_vector_components,
        };
        limits.validate()?;
        Ok(limits)
    }

    pub fn validate(self) -> Result<()> {
        for (name, value, hard_max) in [
            (
                "raw tagged memberships",
                self.max_raw_memberships,
                MAX_TAGGED_RAW_MEMBERSHIPS,
            ),
            (
                "unique exact tagged IDs",
                self.max_unique_exact_ids,
                MAX_TAGGED_EXACT_IDS,
            ),
            (
                "exact tagged vector components",
                self.max_exact_vector_components,
                MAX_TAGGED_EXACT_VECTOR_COMPONENTS,
            ),
            (
                "exact tagged hydration page",
                self.exact_hydration_page_ids,
                MAX_TAGGED_EXACT_HYDRATION_PAGE_IDS,
            ),
            (
                "tagged fallback candidates",
                self.max_fallback_candidates,
                MAX_TAGGED_FALLBACK_CANDIDATES,
            ),
            (
                "tagged fallback hydration IDs",
                self.max_fallback_hydration_ids,
                MAX_TAGGED_FALLBACK_HYDRATION_IDS,
            ),
            (
                "tagged fallback vector components",
                self.max_fallback_vector_components,
                MAX_TAGGED_FALLBACK_VECTOR_COMPONENTS,
            ),
        ] {
            if value == 0 || value > hard_max {
                return Err(Error::InvalidInput(format!(
                    "{name} limit must be in 1..={hard_max}; got {value}"
                )));
            }
        }
        if self.exact_hydration_page_ids > self.max_unique_exact_ids {
            return Err(Error::InvalidInput(
                "exact tagged hydration page exceeds the unique-ID limit".into(),
            ));
        }
        if self.max_fallback_hydration_ids > self.max_fallback_candidates {
            return Err(Error::InvalidInput(
                "tagged fallback hydration limit exceeds the candidate limit".into(),
            ));
        }
        Ok(())
    }

    pub const fn max_raw_memberships(self) -> usize {
        self.max_raw_memberships
    }
    pub const fn max_unique_exact_ids(self) -> usize {
        self.max_unique_exact_ids
    }
    pub const fn max_exact_vector_components(self) -> usize {
        self.max_exact_vector_components
    }
    pub const fn exact_hydration_page_ids(self) -> usize {
        self.exact_hydration_page_ids
    }
    pub const fn max_fallback_candidates(self) -> usize {
        self.max_fallback_candidates
    }
    pub const fn max_fallback_hydration_ids(self) -> usize {
        self.max_fallback_hydration_ids
    }
    pub const fn max_fallback_vector_components(self) -> usize {
        self.max_fallback_vector_components
    }

    pub fn checked_exact_components(self, ids: usize, dimension: usize) -> Result<usize> {
        checked_component_work(
            "exact tagged vector components",
            ids,
            dimension,
            self.max_unique_exact_ids,
            self.max_exact_vector_components,
        )
    }

    pub fn checked_fallback_components(self, ids: usize, dimension: usize) -> Result<usize> {
        checked_component_work(
            "tagged fallback vector components",
            ids,
            dimension,
            self.max_fallback_hydration_ids,
            self.max_fallback_vector_components,
        )
    }
}

impl Default for TaggedAnnWorkLimits {
    fn default() -> Self {
        Self {
            max_raw_memberships: MAX_TAGGED_RAW_MEMBERSHIPS,
            max_unique_exact_ids: MAX_TAGGED_EXACT_IDS,
            max_exact_vector_components: MAX_TAGGED_EXACT_VECTOR_COMPONENTS,
            exact_hydration_page_ids: MAX_TAGGED_EXACT_HYDRATION_PAGE_IDS,
            max_fallback_candidates: MAX_TAGGED_FALLBACK_CANDIDATES,
            max_fallback_hydration_ids: MAX_TAGGED_FALLBACK_HYDRATION_IDS,
            max_fallback_vector_components: MAX_TAGGED_FALLBACK_VECTOR_COMPONENTS,
        }
    }
}

#[derive(Deserialize)]
struct TaggedAnnWorkLimitsWire {
    max_raw_memberships: usize,
    max_unique_exact_ids: usize,
    max_exact_vector_components: usize,
    exact_hydration_page_ids: usize,
    max_fallback_candidates: usize,
    max_fallback_hydration_ids: usize,
    max_fallback_vector_components: usize,
}

impl<'de> Deserialize<'de> for TaggedAnnWorkLimits {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = TaggedAnnWorkLimitsWire::deserialize(deserializer)?;
        Self::new(
            wire.max_raw_memberships,
            wire.max_unique_exact_ids,
            wire.max_exact_vector_components,
            wire.exact_hydration_page_ids,
            wire.max_fallback_candidates,
            wire.max_fallback_hydration_ids,
            wire.max_fallback_vector_components,
        )
        .map_err(serde::de::Error::custom)
    }
}

fn checked_component_work(
    resource: &'static str,
    ids: usize,
    dimension: usize,
    max_ids: usize,
    max_components: usize,
) -> Result<usize> {
    if ids > max_ids {
        return Err(Error::CapacityExceeded {
            resource,
            limit: max_ids,
        });
    }
    if dimension == 0 || dimension > MAX_TAGGED_VECTOR_DIMENSION {
        return Err(Error::InvalidInput(format!(
            "tagged vector dimension must be in 1..={MAX_TAGGED_VECTOR_DIMENSION}; got {dimension}"
        )));
    }
    let components = ids
        .checked_mul(dimension)
        .ok_or_else(|| Error::InvalidInput(format!("{resource} multiplication overflowed")))?;
    if components > max_components {
        return Err(Error::CapacityExceeded {
            resource,
            limit: max_components,
        });
    }
    Ok(components)
}

/// One independently budgeted logical result lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaggedAnnLaneRequest {
    pub lane: RetrievalLifecycleLane,
    pub status: StatusFilter,
    pub k: usize,
    pub fallback_candidates: usize,
}

impl TaggedAnnLaneRequest {
    pub fn new(
        lane: RetrievalLifecycleLane,
        status: StatusFilter,
        k: usize,
        fallback_candidates: usize,
    ) -> Result<Self> {
        let request = Self {
            lane,
            status,
            k,
            fallback_candidates,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(self) -> Result<()> {
        if self.status.is_empty() {
            return Err(Error::InvalidInput(format!(
                "tagged ANN {} lane has an empty status filter",
                self.lane.as_str()
            )));
        }
        if self.k > MAX_TAGGED_HITS_PER_LANE {
            return Err(Error::InvalidInput(format!(
                "tagged ANN {} lane requests {} hits; maximum is {MAX_TAGGED_HITS_PER_LANE}",
                self.lane.as_str(),
                self.k
            )));
        }
        if self.fallback_candidates > MAX_TAGGED_FALLBACK_CANDIDATES {
            return Err(Error::InvalidInput(format!(
                "tagged ANN {} lane requests {} fallback candidates; maximum is {MAX_TAGGED_FALLBACK_CANDIDATES}",
                self.lane.as_str(),
                self.fallback_candidates
            )));
        }
        if self.k == 0 && self.fallback_candidates != 0 {
            return Err(Error::InvalidInput(format!(
                "tagged ANN {} lane reserves fallback work but requests zero hits",
                self.lane.as_str()
            )));
        }
        Ok(())
    }
}

/// Validated, normalized tagged ANN request. Tags remain borrowed ordinary
/// strings; construction sorts them and rejects duplicates without interning.
#[derive(Clone, Debug, Serialize)]
pub struct TaggedAnnRequest<'a> {
    pub query: &'a [f32],
    tags: Vec<&'a str>,
    pub lanes: &'a [TaggedAnnLaneRequest],
    pub limits: TaggedAnnWorkLimits,
}

impl<'a> TaggedAnnRequest<'a> {
    pub fn new(
        query: &'a [f32],
        tags: impl IntoIterator<Item = &'a str>,
        lanes: &'a [TaggedAnnLaneRequest],
        limits: TaggedAnnWorkLimits,
    ) -> Result<Self> {
        let mut normalized = Vec::with_capacity(MAX_TAGGED_QUERY_TAGS);
        for tag in tags {
            if normalized.len() == MAX_TAGGED_QUERY_TAGS {
                return Err(Error::InvalidInput(format!(
                    "tagged ANN has more than {MAX_TAGGED_QUERY_TAGS} query tags"
                )));
            }
            validate_tag(tag).map_err(|error| {
                Error::InvalidInput(format!("invalid tagged ANN query tag {tag:?}: {error}"))
            })?;
            normalized.push(tag);
        }
        let mut tags = normalized;
        tags.sort_unstable();
        let request = Self {
            query,
            tags,
            lanes,
            limits,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn tags(&self) -> &[&'a str] {
        &self.tags
    }

    pub fn validate(&self) -> Result<()> {
        self.limits.validate()?;
        if self.query.is_empty() || self.query.len() > MAX_TAGGED_VECTOR_DIMENSION {
            return Err(Error::InvalidInput(format!(
                "tagged ANN query dimension must be in 1..={MAX_TAGGED_VECTOR_DIMENSION}; got {}",
                self.query.len()
            )));
        }
        let mut norm = 0.0_f32;
        for value in self.query {
            if !value.is_finite() {
                return Err(Error::InvalidInput(
                    "tagged ANN query contains a non-finite component".into(),
                ));
            }
            norm += value * value;
        }
        if !norm.is_finite() || norm <= 0.0 {
            return Err(Error::InvalidInput(
                "tagged ANN query must have a positive finite squared L2 norm".into(),
            ));
        }
        if self.tags.is_empty() || self.tags.len() > MAX_TAGGED_QUERY_TAGS {
            return Err(Error::InvalidInput(format!(
                "tagged ANN requires 1..={MAX_TAGGED_QUERY_TAGS} query tags; got {}",
                self.tags.len()
            )));
        }
        let mut previous = None;
        for tag in &self.tags {
            validate_tag(tag).map_err(|error| {
                Error::InvalidInput(format!("invalid tagged ANN query tag {tag:?}: {error}"))
            })?;
            if previous.is_some_and(|value| value >= *tag) {
                return Err(Error::InvalidInput(format!(
                    "tagged ANN query tags must be normalized and unique; duplicate or unsorted tag {tag:?}"
                )));
            }
            previous = Some(*tag);
        }
        if self.lanes.len() != 1 {
            return Err(Error::InvalidInput(format!(
                "tagged ANN requires exactly one primary lane; got {}",
                self.lanes.len()
            )));
        }
        let lane = &self.lanes[0];
        lane.validate()?;
        let fallback = lane.fallback_candidates;
        if fallback > self.limits.max_fallback_candidates {
            return Err(Error::InvalidInput(format!(
                "tagged ANN primary lane requests {fallback} fallback candidates; limit is {}",
                self.limits.max_fallback_candidates
            )));
        }
        self.limits
            .checked_fallback_components(fallback, self.query.len())?;
        Ok(())
    }
}

/// Domain-separated, stable signed hash used as the physical membership sample key.
/// The signed interpretation is part of the persistence contract.
pub fn stable_tag_sample_hash(id: NodeId) -> i64 {
    let mut hash = Sha256::new();
    hash.update(b"mneme:tag-member-sample-hash:v1\0");
    hash.update(id.0.to_bytes());
    let digest = hash.finalize();
    i64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("SHA-256 prefix is eight bytes"),
    )
}

/// Domain-separated request digest shared by persistent and reference adapters.
/// Framing and enum bytes are explicit so fallback pivots and quota rotations
/// cannot drift between implementations.
pub fn tagged_sample_request_digest(
    request: &TaggedAnnRequest<'_>,
    physical_status: TaggedPhysicalStatus,
    lane: RetrievalLifecycleLane,
    retrieval_semantic_generation: &str,
) -> Result<[u8; 32]> {
    request.validate()?;
    validate_retrieval_semantic_generation(retrieval_semantic_generation)?;
    let requested_lane = request
        .lanes
        .iter()
        .find(|candidate| candidate.lane == lane)
        .ok_or_else(|| Error::InvalidInput("tagged sample lane is absent from request".into()))?;
    if !physical_status.is_admitted_by(requested_lane.status) {
        return Err(Error::InvalidInput(format!(
            "{} physical status is not admitted by the tagged {} lane",
            physical_status.as_str(),
            lane.as_str()
        )));
    }

    let mut hash = Sha256::new();
    hash.update(b"mneme:tagged-sample-request:v1\0");
    hash.update((request.query.len() as u64).to_be_bytes());
    for value in request.query {
        hash.update(value.to_bits().to_be_bytes());
    }
    hash.update((request.tags.len() as u64).to_be_bytes());
    for tag in &request.tags {
        hash.update((tag.len() as u64).to_be_bytes());
        hash.update(tag.as_bytes());
    }
    hash.update([physical_status.digest_byte()]);
    hash.update([lane.digest_byte()]);
    hash.update((retrieval_semantic_generation.len() as u64).to_be_bytes());
    hash.update(retrieval_semantic_generation.as_bytes());
    Ok(hash.finalize().into())
}

fn validate_retrieval_semantic_generation(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_TAGGED_PROJECTION_GENERATION_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput(format!(
            "retrieval semantic generation must be nonempty, trimmed, control-free, and at most {MAX_TAGGED_PROJECTION_GENERATION_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

/// Signed physical-key pivot selected by [`tagged_sample_request_digest`].
pub fn tagged_sample_pivot(
    request: &TaggedAnnRequest<'_>,
    physical_status: TaggedPhysicalStatus,
    lane: RetrievalLifecycleLane,
    retrieval_semantic_generation: &str,
) -> Result<i64> {
    let digest = tagged_sample_request_digest(
        request,
        physical_status,
        lane,
        retrieval_semantic_generation,
    )?;
    Ok(i64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("digest prefix is eight bytes"),
    ))
}

/// One deterministic share of a logical lane's fallback allowance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaggedPhysicalQuota {
    pub status: TaggedPhysicalStatus,
    pub candidates: usize,
}

/// Split a logical fallback allowance across its admitted physical statuses.
/// Remainder ownership rotates from a domain-separated request digest, while
/// returned entries remain in stable active/archived order.
pub fn tagged_physical_status_quotas(
    request: &TaggedAnnRequest<'_>,
    lane: RetrievalLifecycleLane,
    retrieval_semantic_generation: &str,
) -> Result<Vec<TaggedPhysicalQuota>> {
    request.validate()?;
    validate_retrieval_semantic_generation(retrieval_semantic_generation)?;
    let lane = request
        .lanes
        .iter()
        .find(|candidate| candidate.lane == lane)
        .ok_or_else(|| Error::InvalidInput("tagged quota lane is absent from request".into()))?;
    let statuses: Vec<_> = TaggedPhysicalStatus::ALL
        .into_iter()
        .filter(|status| status.is_admitted_by(lane.status))
        .collect();
    debug_assert!(!statuses.is_empty());

    let mut hash = Sha256::new();
    hash.update(b"mneme:tagged-physical-quota:v1\0");
    hash.update((request.query.len() as u64).to_be_bytes());
    for value in request.query {
        hash.update(value.to_bits().to_be_bytes());
    }
    hash.update((request.tags.len() as u64).to_be_bytes());
    for tag in &request.tags {
        hash.update((tag.len() as u64).to_be_bytes());
        hash.update(tag.as_bytes());
    }
    hash.update([lane.lane.digest_byte()]);
    hash.update([u8::from(lane.status.active), u8::from(lane.status.archived)]);
    hash.update((retrieval_semantic_generation.len() as u64).to_be_bytes());
    hash.update(retrieval_semantic_generation.as_bytes());
    let digest = hash.finalize();
    let rotation = (u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("digest prefix is eight bytes"),
    ) % statuses.len() as u64) as usize;
    let base = lane.fallback_candidates / statuses.len();
    let remainder = lane.fallback_candidates % statuses.len();
    let mut candidates = vec![base; statuses.len()];
    for offset in 0..remainder {
        candidates[(rotation + offset) % statuses.len()] += 1;
    }
    Ok(statuses
        .into_iter()
        .zip(candidates)
        .map(|(status, candidates)| TaggedPhysicalQuota { status, candidates })
        .collect())
}

/// One deterministic share of a physical sample quota assigned to a query tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TaggedQueryTagQuota<'a> {
    pub tag: &'a str,
    pub candidates: usize,
}

/// Split one physical lifecycle's hashed-membership quota across normalized
/// query tags. Unused shares are not borrowed; adapters must inspect each tag's
/// own prefix no more than the returned allowance.
pub fn tagged_query_tag_quotas<'tags>(
    request: &TaggedAnnRequest<'tags>,
    physical_status: TaggedPhysicalStatus,
    lane: RetrievalLifecycleLane,
    retrieval_semantic_generation: &str,
    sample_quota: usize,
) -> Result<Vec<TaggedQueryTagQuota<'tags>>> {
    request.validate()?;
    if sample_quota > MAX_TAGGED_FALLBACK_CANDIDATES {
        return Err(Error::InvalidInput(format!(
            "tagged sample quota is {sample_quota}; maximum is {MAX_TAGGED_FALLBACK_CANDIDATES}"
        )));
    }
    let digest = tagged_sample_request_digest(
        request,
        physical_status,
        lane,
        retrieval_semantic_generation,
    )?;
    let rotation = (u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("digest prefix is eight bytes"),
    ) % request.tags.len() as u64) as usize;
    let base = sample_quota / request.tags.len();
    let remainder = sample_quota % request.tags.len();
    let mut candidates = vec![base; request.tags.len()];
    for offset in 0..remainder {
        candidates[(rotation + offset) % request.tags.len()] += 1;
    }
    Ok(request
        .tags
        .iter()
        .copied()
        .zip(candidates)
        .map(|(tag, candidates)| TaggedQueryTagQuota { tag, candidates })
        .collect())
}

/// Supported bounded fallback construction strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaggedFallbackStrategy {
    LifecycleHnswAndHashedTagSample,
    DeterministicHashedTagSample,
}

/// Deterministic split of one physical allowance between fallback legs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaggedFallbackLegQuota {
    pub hnsw: usize,
    pub sample: usize,
}

pub fn tagged_fallback_leg_quotas(
    candidates: usize,
    strategy: TaggedFallbackStrategy,
) -> Result<TaggedFallbackLegQuota> {
    if candidates > MAX_TAGGED_FALLBACK_CANDIDATES {
        return Err(Error::InvalidInput(format!(
            "tagged physical fallback quota is {candidates}; maximum is {MAX_TAGGED_FALLBACK_CANDIDATES}"
        )));
    }
    let sample = match strategy {
        TaggedFallbackStrategy::LifecycleHnswAndHashedTagSample => candidates.div_ceil(2),
        TaggedFallbackStrategy::DeterministicHashedTagSample => candidates,
    };
    Ok(TaggedFallbackLegQuota {
        hnsw: candidates - sample,
        sample,
    })
}

/// Exact-work ceiling that selected bounded partial fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaggedExactWorkLimit {
    RawMemberships,
    UniqueExactIds,
    ExactVectorComponents,
}

/// Select the first exact-work fuse in the staged tagged-scan precedence.
///
/// "First" is not chronological row-by-row crossing. Adapters charge the raw
/// scan through its canary while saturating unique-ID storage at the unique-ID
/// limit plus one; they then select Raw > Unique > Components from the final bounded
/// counters. This order is part of the adapter contract. Counter and dimension
/// relationships are validated before a result is selected so direct adapter
/// callers cannot manufacture coverage from an impossible tuple.
pub fn tagged_exact_work_overflow(
    raw_memberships: usize,
    unique_exact_ids: usize,
    query_dimension: usize,
    limits: TaggedAnnWorkLimits,
) -> Result<Option<TaggedExactWorkLimit>> {
    limits.validate()?;
    if query_dimension == 0 || query_dimension > MAX_TAGGED_VECTOR_DIMENSION {
        return Err(Error::InvalidInput(format!(
            "tagged exact-work query dimension must be in 1..={MAX_TAGGED_VECTOR_DIMENSION}; got {query_dimension}"
        )));
    }
    if raw_memberships > limits.max_raw_memberships() + 1 {
        return Err(Error::InvalidInput(format!(
            "tagged exact-work raw memberships exceed the request canary ceiling of {}",
            limits.max_raw_memberships() + 1
        )));
    }
    if unique_exact_ids > raw_memberships {
        return Err(Error::InvalidInput(
            "tagged exact-work unique IDs exceed charged raw memberships".into(),
        ));
    }
    if unique_exact_ids > limits.max_unique_exact_ids() + 1 {
        return Err(Error::InvalidInput(format!(
            "tagged exact-work unique IDs exceed the request canary ceiling of {}",
            limits.max_unique_exact_ids() + 1
        )));
    }
    let components = unique_exact_ids
        .checked_mul(query_dimension)
        .ok_or_else(|| {
            Error::InvalidInput("tagged exact-work component count overflowed".into())
        })?;

    Ok(if raw_memberships > limits.max_raw_memberships() {
        Some(TaggedExactWorkLimit::RawMemberships)
    } else if unique_exact_ids > limits.max_unique_exact_ids() {
        Some(TaggedExactWorkLimit::UniqueExactIds)
    } else if components > limits.max_exact_vector_components() {
        Some(TaggedExactWorkLimit::ExactVectorComponents)
    } else {
        None
    })
}

/// One query tag's positional sample allowance and observed work.
///
/// The index addresses the request's normalized tag array without echoing an
/// attacker-controlled tag string into every lane/status coverage record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaggedQueryTagSeedCoverage {
    pub query_tag_index: u8,
    pub quota: usize,
    pub inspected: usize,
}

impl TaggedQueryTagSeedCoverage {
    pub fn new(query_tag_index: u8, quota: usize, inspected: usize) -> Result<Self> {
        let coverage = Self {
            query_tag_index,
            quota,
            inspected,
        };
        coverage.validate()?;
        Ok(coverage)
    }

    pub fn validate(self) -> Result<()> {
        if usize::from(self.query_tag_index) >= MAX_TAGGED_QUERY_TAGS {
            return Err(Error::InvalidInput(format!(
                "tagged sample query-tag index {} is outside 0..{MAX_TAGGED_QUERY_TAGS}",
                self.query_tag_index
            )));
        }
        if self.quota > MAX_TAGGED_FALLBACK_CANDIDATES {
            return Err(Error::InvalidInput(format!(
                "tagged query-tag sample quota is {}; maximum is {MAX_TAGGED_FALLBACK_CANDIDATES}",
                self.quota
            )));
        }
        if self.inspected > self.quota {
            return Err(Error::InvalidInput(format!(
                "tagged query-tag sample {} inspected count exceeds its own quota",
                self.query_tag_index
            )));
        }
        Ok(())
    }
}

/// Per-physical-lifecycle fallback quota and observed work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaggedPhysicalSeedCoverage {
    pub status: TaggedPhysicalStatus,
    pub hnsw_quota: usize,
    pub hnsw_inspected: usize,
    pub tag_samples: Vec<TaggedQueryTagSeedCoverage>,
    pub sample_pivot: i64,
}

impl TaggedPhysicalSeedCoverage {
    pub fn new(
        status: TaggedPhysicalStatus,
        hnsw_quota: usize,
        hnsw_inspected: usize,
        tag_samples: Vec<TaggedQueryTagSeedCoverage>,
        sample_pivot: i64,
    ) -> Result<Self> {
        let coverage = Self {
            status,
            hnsw_quota,
            hnsw_inspected,
            tag_samples,
            sample_pivot,
        };
        coverage.validate()?;
        Ok(coverage)
    }

    pub fn validate(&self) -> Result<()> {
        if self.hnsw_inspected > self.hnsw_quota {
            return Err(Error::InvalidInput(format!(
                "{} tagged HNSW inspected count exceeds its quota",
                self.status.as_str()
            )));
        }
        if self.tag_samples.is_empty() || self.tag_samples.len() > MAX_TAGGED_QUERY_TAGS {
            return Err(Error::InvalidInput(format!(
                "{} tagged sample coverage must contain 1..={MAX_TAGGED_QUERY_TAGS} positional query-tag entries",
                self.status.as_str()
            )));
        }
        for (expected_index, sample) in self.tag_samples.iter().enumerate() {
            sample.validate()?;
            if usize::from(sample.query_tag_index) != expected_index {
                return Err(Error::InvalidInput(format!(
                    "{} tagged sample coverage query-tag indexes must be exactly 0..{} in order",
                    self.status.as_str(),
                    self.tag_samples.len()
                )));
            }
        }
        let quota = self.total_quota()?;
        if quota > MAX_TAGGED_FALLBACK_CANDIDATES {
            return Err(Error::InvalidInput(format!(
                "{} tagged physical fallback quota is {quota}; maximum is {MAX_TAGGED_FALLBACK_CANDIDATES}",
                self.status.as_str()
            )));
        }
        self.total_inspected()?;
        Ok(())
    }

    pub fn sample_quota(&self) -> Result<usize> {
        self.tag_samples.iter().try_fold(0usize, |total, sample| {
            total.checked_add(sample.quota).ok_or_else(|| {
                Error::InvalidInput("tagged query-tag sample quota sum overflowed".into())
            })
        })
    }

    pub fn sample_inspected(&self) -> Result<usize> {
        self.tag_samples.iter().try_fold(0usize, |total, sample| {
            total.checked_add(sample.inspected).ok_or_else(|| {
                Error::InvalidInput("tagged query-tag inspected sum overflowed".into())
            })
        })
    }

    pub fn total_quota(&self) -> Result<usize> {
        self.hnsw_quota
            .checked_add(self.sample_quota()?)
            .ok_or_else(|| Error::InvalidInput("tagged physical fallback quota overflowed".into()))
    }

    pub fn total_inspected(&self) -> Result<usize> {
        self.hnsw_inspected
            .checked_add(self.sample_inspected()?)
            .ok_or_else(|| Error::InvalidInput("tagged physical inspected count overflowed".into()))
    }
}

/// Completeness/strategy statement for one logical tagged seed lane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum TaggedSeedCoverage {
    ExactCosine,
    LifecycleHnswAndHashedTagSamplePostfilter {
        exceeded_limit: TaggedExactWorkLimit,
        raw_memberships: usize,
        physical: Vec<TaggedPhysicalSeedCoverage>,
        canonical_candidates_checked: usize,
        matching_candidates: usize,
    },
    DeterministicHashedTagSamplePostfilter {
        exceeded_limit: TaggedExactWorkLimit,
        raw_memberships: usize,
        physical: Vec<TaggedPhysicalSeedCoverage>,
        canonical_candidates_checked: usize,
        matching_candidates: usize,
    },
}

impl TaggedSeedCoverage {
    pub const fn strategy_id(&self) -> &'static str {
        match self {
            Self::ExactCosine => "exact_cosine",
            Self::LifecycleHnswAndHashedTagSamplePostfilter { .. } => {
                "lifecycle_hnsw_and_hashed_tag_sample_postfilter"
            }
            Self::DeterministicHashedTagSamplePostfilter { .. } => {
                "deterministic_hashed_tag_sample_postfilter"
            }
        }
    }

    pub const fn is_partial(&self) -> bool {
        !matches!(self, Self::ExactCosine)
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::ExactCosine => Ok(()),
            Self::LifecycleHnswAndHashedTagSamplePostfilter {
                raw_memberships,
                physical,
                canonical_candidates_checked,
                matching_candidates,
                ..
            } => validate_partial_coverage(
                *raw_memberships,
                physical,
                *canonical_candidates_checked,
                *matching_candidates,
                false,
            ),
            Self::DeterministicHashedTagSamplePostfilter {
                raw_memberships,
                physical,
                canonical_candidates_checked,
                matching_candidates,
                ..
            } => validate_partial_coverage(
                *raw_memberships,
                physical,
                *canonical_candidates_checked,
                *matching_candidates,
                true,
            ),
        }
    }

    fn partial_fields(
        &self,
    ) -> Option<(
        TaggedExactWorkLimit,
        usize,
        &[TaggedPhysicalSeedCoverage],
        usize,
        usize,
    )> {
        match self {
            Self::LifecycleHnswAndHashedTagSamplePostfilter {
                exceeded_limit,
                raw_memberships,
                physical,
                canonical_candidates_checked,
                matching_candidates,
                ..
            }
            | Self::DeterministicHashedTagSamplePostfilter {
                exceeded_limit,
                raw_memberships,
                physical,
                canonical_candidates_checked,
                matching_candidates,
                ..
            } => Some((
                *exceeded_limit,
                *raw_memberships,
                physical,
                *canonical_candidates_checked,
                *matching_candidates,
            )),
            Self::ExactCosine => None,
        }
    }
}

fn validate_partial_coverage(
    raw_memberships: usize,
    physical: &[TaggedPhysicalSeedCoverage],
    canonical_candidates_checked: usize,
    matching_candidates: usize,
    require_zero_hnsw: bool,
) -> Result<()> {
    if raw_memberships > MAX_TAGGED_RAW_MEMBERSHIP_READS {
        return Err(Error::InvalidInput(format!(
            "tagged raw membership counter is {raw_memberships}; hard maximum including canary is {MAX_TAGGED_RAW_MEMBERSHIP_READS}"
        )));
    }
    if physical.is_empty() || physical.len() > TaggedPhysicalStatus::ALL.len() {
        return Err(Error::InvalidInput(
            "partial tagged seed coverage requires one to three physical statuses".into(),
        ));
    }
    let mut previous = None;
    let mut quota = 0usize;
    let mut inspected = 0usize;
    for item in physical {
        item.validate()?;
        if previous.is_some_and(|status| status >= item.status) {
            return Err(Error::InvalidInput(
                "tagged physical coverage statuses must be unique and lifecycle-ordered".into(),
            ));
        }
        if require_zero_hnsw && (item.hnsw_quota != 0 || item.hnsw_inspected != 0) {
            return Err(Error::InvalidInput(
                "deterministic tagged sample coverage must have zero HNSW work".into(),
            ));
        }
        quota = quota
            .checked_add(item.total_quota()?)
            .ok_or_else(|| Error::InvalidInput("tagged physical quota sum overflowed".into()))?;
        inspected = inspected
            .checked_add(item.total_inspected()?)
            .ok_or_else(|| Error::InvalidInput("tagged inspected sum overflowed".into()))?;
        previous = Some(item.status);
    }
    if quota > MAX_TAGGED_FALLBACK_CANDIDATES || inspected > MAX_TAGGED_FALLBACK_CANDIDATES {
        return Err(Error::InvalidInput(
            "tagged physical coverage exceeds the aggregate fallback ceiling".into(),
        ));
    }
    if canonical_candidates_checked > inspected {
        return Err(Error::InvalidInput(
            "tagged canonical checked count exceeds inspected fallback candidates".into(),
        ));
    }
    if matching_candidates > canonical_candidates_checked {
        return Err(Error::InvalidInput(
            "tagged matching count exceeds canonical checked candidates".into(),
        ));
    }
    Ok(())
}

/// Aggregate adapter work for one multi-lane tagged request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaggedAnnWork {
    pub query_dimension: usize,
    pub raw_memberships: usize,
    pub unique_exact_ids: usize,
    pub exact_hydrated_ids: usize,
    pub exact_vector_components: usize,
    pub fallback_hnsw_inspected: usize,
    pub fallback_sample_inspected: usize,
    pub fallback_unique_ids: usize,
    pub fallback_canonical_candidates_checked: usize,
    pub fallback_matching_candidates: usize,
    pub fallback_hydrated_ids: usize,
    pub fallback_vector_components: usize,
}

impl TaggedAnnWork {
    pub fn validate(&self) -> Result<()> {
        if self.query_dimension == 0 || self.query_dimension > MAX_TAGGED_VECTOR_DIMENSION {
            return Err(Error::InvalidInput(format!(
                "tagged work query dimension must be in 1..={MAX_TAGGED_VECTOR_DIMENSION}; got {}",
                self.query_dimension
            )));
        }
        if self.raw_memberships > MAX_TAGGED_RAW_MEMBERSHIP_READS {
            return Err(Error::InvalidInput(
                "tagged work exceeds the raw-membership canary ceiling".into(),
            ));
        }
        if self.unique_exact_ids > MAX_TAGGED_EXACT_IDS + 1 {
            return Err(Error::InvalidInput(
                "tagged work exceeds the unique exact-ID canary ceiling".into(),
            ));
        }
        if self.unique_exact_ids > self.raw_memberships {
            return Err(Error::InvalidInput(
                "tagged unique exact-ID count exceeds charged memberships".into(),
            ));
        }
        if self.exact_hydrated_ids > self.unique_exact_ids
            || self.exact_hydrated_ids > MAX_TAGGED_EXACT_IDS
        {
            return Err(Error::InvalidInput(
                "tagged exact hydration count exceeds admitted exact IDs".into(),
            ));
        }
        let exact_components = self
            .exact_hydrated_ids
            .checked_mul(self.query_dimension)
            .ok_or_else(|| Error::InvalidInput("tagged exact component count overflowed".into()))?;
        if exact_components != self.exact_vector_components
            || exact_components > MAX_TAGGED_EXACT_VECTOR_COMPONENTS
        {
            return Err(Error::InvalidInput(
                "tagged exact vector-component counter is inconsistent or over limit".into(),
            ));
        }
        let inspected = self
            .fallback_hnsw_inspected
            .checked_add(self.fallback_sample_inspected)
            .ok_or_else(|| {
                Error::InvalidInput("tagged fallback inspected sum overflowed".into())
            })?;
        if inspected > MAX_TAGGED_FALLBACK_CANDIDATES
            || self.fallback_unique_ids > inspected
            || self.fallback_canonical_candidates_checked > self.fallback_unique_ids
            || self.fallback_matching_candidates > self.fallback_canonical_candidates_checked
            || self.fallback_hydrated_ids > self.fallback_canonical_candidates_checked
            || self.fallback_hydrated_ids > MAX_TAGGED_FALLBACK_HYDRATION_IDS
        {
            return Err(Error::InvalidInput(
                "tagged fallback counters are inconsistent or over limit".into(),
            ));
        }
        let fallback_components = self
            .fallback_hydrated_ids
            .checked_mul(self.query_dimension)
            .ok_or_else(|| {
                Error::InvalidInput("tagged fallback component count overflowed".into())
            })?;
        if fallback_components != self.fallback_vector_components
            || fallback_components > MAX_TAGGED_FALLBACK_VECTOR_COMPONENTS
        {
            return Err(Error::InvalidInput(
                "tagged fallback vector-component counter is inconsistent or over limit".into(),
            ));
        }
        Ok(())
    }

    fn validate_against(&self, request: &TaggedAnnRequest<'_>) -> Result<()> {
        self.validate()?;
        if self.query_dimension != request.query.len()
            || self.raw_memberships > request.limits.max_raw_memberships + 1
            || self.unique_exact_ids > request.limits.max_unique_exact_ids + 1
            || self.exact_hydrated_ids > request.limits.max_unique_exact_ids
            || self.exact_vector_components > request.limits.max_exact_vector_components
            || self.fallback_hnsw_inspected + self.fallback_sample_inspected
                > request.limits.max_fallback_candidates
            || self.fallback_hydrated_ids > request.limits.max_fallback_hydration_ids
            || self.fallback_vector_components > request.limits.max_fallback_vector_components
        {
            return Err(Error::InvalidInput(
                "tagged adapter work exceeds the validated request".into(),
            ));
        }
        Ok(())
    }
}

/// One logical tagged result lane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaggedAnnLane {
    pub lane: RetrievalLifecycleLane,
    pub hits: Vec<Scored>,
    pub seed_coverage: TaggedSeedCoverage,
}

/// Complete multi-lane tagged seed result plus its verified projection watermark.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaggedAnnBatch {
    pub lanes: Vec<TaggedAnnLane>,
    pub work: TaggedAnnWork,
    pub projection_generation: TaggedProjectionGeneration,
}

impl TaggedAnnBatch {
    pub fn new(
        lanes: Vec<TaggedAnnLane>,
        work: TaggedAnnWork,
        projection_generation: TaggedProjectionGeneration,
    ) -> Result<Self> {
        let batch = Self {
            lanes,
            work,
            projection_generation,
        };
        batch.validate()?;
        Ok(batch)
    }

    pub fn validate(&self) -> Result<()> {
        self.work.validate()?;
        if self.lanes.len() != 1 {
            return Err(Error::InvalidInput(
                "tagged ANN batch must contain exactly one primary lane".into(),
            ));
        }
        let mut hit_ids = HashSet::new();
        for lane in &self.lanes {
            if lane.hits.len() > MAX_TAGGED_HITS_PER_LANE {
                return Err(Error::InvalidInput(
                    "tagged ANN batch contains too many lane hits".into(),
                ));
            }
            lane.seed_coverage.validate()?;
            let mut previous_hit: Option<&Scored> = None;
            for hit in &lane.hits {
                if !hit.score.is_finite() || !(-1.0..=1.0).contains(&hit.score) {
                    return Err(Error::InvalidInput(
                        "tagged ANN batch contains an invalid score".into(),
                    ));
                }
                if !hit_ids.insert(hit.id) {
                    return Err(Error::InvalidInput(
                        "tagged ANN batch contains duplicate hit IDs".into(),
                    ));
                }
                if previous_hit.is_some_and(|previous| {
                    previous.score.total_cmp(&hit.score).is_lt()
                        || previous.score.total_cmp(&hit.score).is_eq() && previous.id > hit.id
                }) {
                    return Err(Error::InvalidInput(
                        "tagged ANN lane hits must be ordered by score descending then ID ascending"
                            .into(),
                    ));
                }
                previous_hit = Some(hit);
            }
        }
        Ok(())
    }

    /// Reject malformed or over-budget adapter output before graph expansion.
    pub fn validate_against(
        &self,
        request: &TaggedAnnRequest<'_>,
        retrieval_semantic_generation: &str,
        expected_projection_generation: &str,
    ) -> Result<()> {
        request.validate()?;
        validate_retrieval_semantic_generation(retrieval_semantic_generation)?;
        let expected_projection_generation =
            TaggedProjectionGeneration::new(expected_projection_generation)?;
        self.validate()?;
        if self.projection_generation != expected_projection_generation {
            return Err(Error::InvalidInput(format!(
                "tagged ANN adapter returned projection generation {:?}; expected {:?}",
                self.projection_generation.as_str(),
                expected_projection_generation.as_str()
            )));
        }
        self.work.validate_against(request)?;
        if self.lanes.len() != request.lanes.len() {
            return Err(Error::InvalidInput(
                "tagged ANN adapter omitted or added a logical lane".into(),
            ));
        }

        let mut hnsw_inspected = 0usize;
        let mut sample_inspected = 0usize;
        let mut canonical_checked = 0usize;
        let mut matching = 0usize;
        let mut returned_hits = 0usize;
        let mut shared_overflow = None;
        let expected_overflow = tagged_exact_work_overflow(
            self.work.raw_memberships,
            self.work.unique_exact_ids,
            self.work.query_dimension,
            request.limits,
        )?;
        for (index, requested) in request.lanes.iter().enumerate() {
            let returned = &self.lanes[index];
            if returned.lane != requested.lane {
                return Err(Error::InvalidInput(format!(
                    "tagged ANN adapter returned lanes out of request order; expected {} at index {index}",
                    requested.lane.as_str(),
                )));
            }
            if returned.hits.len() > requested.k {
                return Err(Error::InvalidInput(format!(
                    "tagged ANN {} lane returned more than its requested hit limit",
                    requested.lane.as_str()
                )));
            }
            returned_hits = returned_hits
                .checked_add(returned.hits.len())
                .ok_or_else(|| Error::InvalidInput("tagged hit count overflowed".into()))?;
            if let Some((exceeded, raw, physical, checked, matches)) =
                returned.seed_coverage.partial_fields()
            {
                if returned.hits.len() > matches {
                    return Err(Error::InvalidInput(format!(
                        "tagged ANN {} lane returns more hits than its matching fallback candidates",
                        requested.lane.as_str()
                    )));
                }
                if raw != self.work.raw_memberships {
                    return Err(Error::InvalidInput(
                        "tagged lane raw-work coverage disagrees with aggregate work".into(),
                    ));
                }
                let expected: Vec<_> = TaggedPhysicalStatus::ALL
                    .into_iter()
                    .filter(|status| status.is_admitted_by(requested.status))
                    .collect();
                let actual: Vec<_> = physical.iter().map(|item| item.status).collect();
                if actual != expected {
                    return Err(Error::InvalidInput(format!(
                        "tagged ANN {} lane physical coverage disagrees with its status filter",
                        requested.lane.as_str()
                    )));
                }
                let expected_quotas = tagged_physical_status_quotas(
                    request,
                    requested.lane,
                    retrieval_semantic_generation,
                )?;
                let strategy = if matches!(
                    returned.seed_coverage,
                    TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter { .. }
                ) {
                    TaggedFallbackStrategy::LifecycleHnswAndHashedTagSample
                } else {
                    TaggedFallbackStrategy::DeterministicHashedTagSample
                };
                for (item, expected) in physical.iter().zip(expected_quotas) {
                    if item.status != expected.status || item.total_quota()? != expected.candidates
                    {
                        return Err(Error::InvalidInput(format!(
                            "tagged ANN {} lane physical quota disagrees with the deterministic split",
                            requested.lane.as_str()
                        )));
                    }
                    let expected_legs = tagged_fallback_leg_quotas(expected.candidates, strategy)?;
                    if item.sample_quota()? != expected_legs.sample
                        || item.hnsw_quota != expected_legs.hnsw
                    {
                        return Err(Error::InvalidInput(format!(
                            "tagged ANN {} lane fallback-strategy split is invalid",
                            requested.lane.as_str()
                        )));
                    }
                    let expected_tag_quotas = tagged_query_tag_quotas(
                        request,
                        item.status,
                        requested.lane,
                        retrieval_semantic_generation,
                        expected_legs.sample,
                    )?;
                    if item.tag_samples.len() != expected_tag_quotas.len()
                        || item
                            .tag_samples
                            .iter()
                            .zip(expected_tag_quotas)
                            .enumerate()
                            .any(|(index, (actual, expected))| {
                                usize::from(actual.query_tag_index) != index
                                    || actual.quota != expected.candidates
                            })
                    {
                        return Err(Error::InvalidInput(format!(
                            "tagged ANN {} lane per-query-tag sample quotas are invalid",
                            requested.lane.as_str()
                        )));
                    }
                    let expected_pivot = tagged_sample_pivot(
                        request,
                        item.status,
                        requested.lane,
                        retrieval_semantic_generation,
                    )?;
                    if item.sample_pivot != expected_pivot {
                        return Err(Error::InvalidInput(format!(
                            "tagged ANN {} lane sample pivot is invalid",
                            requested.lane.as_str()
                        )));
                    }
                    hnsw_inspected =
                        hnsw_inspected
                            .checked_add(item.hnsw_inspected)
                            .ok_or_else(|| {
                                Error::InvalidInput("tagged HNSW inspected sum overflowed".into())
                            })?;
                    sample_inspected = sample_inspected
                        .checked_add(item.sample_inspected()?)
                        .ok_or_else(|| {
                            Error::InvalidInput("tagged sample inspected sum overflowed".into())
                        })?;
                }
                canonical_checked = canonical_checked.checked_add(checked).ok_or_else(|| {
                    Error::InvalidInput("tagged canonical checked sum overflowed".into())
                })?;
                matching = matching
                    .checked_add(matches)
                    .ok_or_else(|| Error::InvalidInput("tagged matching sum overflowed".into()))?;
                if expected_overflow != Some(exceeded) {
                    return Err(Error::InvalidInput(
                        "tagged partial coverage names the wrong exact-work overflow reason".into(),
                    ));
                }
                if shared_overflow.is_some_and(|prior| prior != exceeded) {
                    return Err(Error::InvalidInput(
                        "tagged ANN lanes disagree on the shared exact-work overflow reason".into(),
                    ));
                }
                shared_overflow = Some(exceeded);
            }
        }
        let all_exact = self
            .lanes
            .iter()
            .all(|lane| matches!(lane.seed_coverage, TaggedSeedCoverage::ExactCosine));
        if all_exact {
            if expected_overflow.is_some()
                || self.work.fallback_hnsw_inspected != 0
                || self.work.fallback_sample_inspected != 0
                || self.work.fallback_unique_ids != 0
            {
                return Err(Error::InvalidInput(
                    "exact tagged coverage contradicts aggregate work".into(),
                ));
            }
            if returned_hits > self.work.exact_hydrated_ids {
                return Err(Error::InvalidInput(
                    "exact tagged batch returns more hits than hydrated exact vectors".into(),
                ));
            }
        } else {
            let uses_bounded_fallback = self
                .lanes
                .iter()
                .all(|lane| lane.seed_coverage.partial_fields().is_some());
            if uses_bounded_fallback
                && (self.work.exact_hydrated_ids != 0 || self.work.exact_vector_components != 0)
            {
                return Err(Error::InvalidInput(
                    "tagged fallback retained exact-prefix hydration work".into(),
                ));
            }
            if returned_hits > self.work.fallback_matching_candidates
                || returned_hits > self.work.fallback_hydrated_ids
            {
                return Err(Error::InvalidInput(
                    "partial tagged batch returns more hits than matching hydrated candidates"
                        .into(),
                ));
            }
            if hnsw_inspected != self.work.fallback_hnsw_inspected
                || sample_inspected != self.work.fallback_sample_inspected
                || canonical_checked != self.work.fallback_canonical_candidates_checked
                || matching != self.work.fallback_matching_candidates
            {
                return Err(Error::InvalidInput(
                    "tagged per-lane fallback counters disagree with aggregate work".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use ulid::Ulid;

    fn active_lane(k: usize, fallback_candidates: usize) -> TaggedAnnLaneRequest {
        TaggedAnnLaneRequest::new(
            RetrievalLifecycleLane::Primary,
            StatusFilter::ACTIVE,
            k,
            fallback_candidates,
        )
        .unwrap()
    }

    fn exact_work(query_dimension: usize, hydrated: usize) -> TaggedAnnWork {
        TaggedAnnWork {
            query_dimension,
            raw_memberships: hydrated,
            unique_exact_ids: hydrated,
            exact_hydrated_ids: hydrated,
            exact_vector_components: hydrated * query_dimension,
            fallback_hnsw_inspected: 0,
            fallback_sample_inspected: 0,
            fallback_unique_ids: 0,
            fallback_canonical_candidates_checked: 0,
            fallback_matching_candidates: 0,
            fallback_hydrated_ids: 0,
            fallback_vector_components: 0,
        }
    }

    #[test]
    fn request_normalizes_tags_and_rejects_invalid_caps() {
        let query = [1.0, 0.0];
        let lanes = [active_lane(1, 1)];
        let request = TaggedAnnRequest::new(
            &query,
            ["zeta", "alpha"],
            &lanes,
            TaggedAnnWorkLimits::default(),
        )
        .unwrap();
        assert_eq!(request.tags(), ["alpha", "zeta"]);
        assert!(
            TaggedAnnRequest::new(
                &query,
                ["same", "same"],
                &lanes,
                TaggedAnnWorkLimits::default(),
            )
            .is_err()
        );

        let max_tag = "x".repeat(MAX_TAGGED_QUERY_TAG_BYTES);
        assert!(
            TaggedAnnRequest::new(
                &query,
                [max_tag.as_str()],
                &lanes,
                TaggedAnnWorkLimits::default(),
            )
            .is_ok()
        );
        let oversized = "x".repeat(MAX_TAGGED_QUERY_TAG_BYTES + 1);
        assert!(
            TaggedAnnRequest::new(
                &query,
                [oversized.as_str()],
                &lanes,
                TaggedAnnWorkLimits::default(),
            )
            .is_err()
        );

        assert!(
            TaggedAnnLaneRequest::new(
                RetrievalLifecycleLane::Primary,
                StatusFilter::ACTIVE,
                MAX_TAGGED_HITS_PER_LANE + 1,
                0,
            )
            .is_err()
        );
        assert!(
            TaggedAnnLaneRequest::new(RetrievalLifecycleLane::Primary, StatusFilter::ACTIVE, 0, 1,)
                .is_err()
        );
        assert!(
            TaggedAnnWorkLimits::new(
                MAX_TAGGED_RAW_MEMBERSHIPS + 1,
                MAX_TAGGED_EXACT_IDS,
                MAX_TAGGED_EXACT_VECTOR_COMPONENTS,
                MAX_TAGGED_EXACT_HYDRATION_PAGE_IDS,
                MAX_TAGGED_FALLBACK_CANDIDATES,
                MAX_TAGGED_FALLBACK_HYDRATION_IDS,
                MAX_TAGGED_FALLBACK_VECTOR_COMPONENTS,
            )
            .is_err()
        );
        assert_eq!(
            TaggedAnnWorkLimits::default()
                .checked_fallback_components(256, 4_096)
                .unwrap(),
            MAX_TAGGED_FALLBACK_VECTOR_COMPONENTS
        );
        assert!(
            TaggedAnnWorkLimits::default()
                .checked_fallback_components(256, 4_097)
                .is_err()
        );
    }

    #[test]
    fn request_stream_caps_raw_tags_before_collecting() {
        const TAGS: [&str; 33] = [
            "t00", "t01", "t02", "t03", "t04", "t05", "t06", "t07", "t08", "t09", "t10", "t11",
            "t12", "t13", "t14", "t15", "t16", "t17", "t18", "t19", "t20", "t21", "t22", "t23",
            "t24", "t25", "t26", "t27", "t28", "t29", "t30", "t31", "t32",
        ];
        let visited = Cell::new(0usize);
        let tags = TAGS.into_iter().inspect(|_| visited.set(visited.get() + 1));
        let query = [1.0, 0.0];
        let lanes = [active_lane(1, 1)];
        assert!(
            TaggedAnnRequest::new(&query, tags, &lanes, TaggedAnnWorkLimits::default(),).is_err()
        );
        assert_eq!(visited.get(), MAX_TAGGED_QUERY_TAGS + 1);
    }

    #[test]
    fn generation_and_serde_reject_malformed_values() {
        let generation = TaggedProjectionGeneration::new("tag-v2").unwrap();
        let json = serde_json::to_string(&generation).unwrap();
        assert_eq!(json, "\"tag-v2\"");
        assert_eq!(
            serde_json::from_str::<TaggedProjectionGeneration>(&json)
                .unwrap()
                .as_str(),
            "tag-v2"
        );
        assert!(TaggedProjectionGeneration::new(" bad").is_err());
        assert!(TaggedProjectionGeneration::new("x".repeat(129)).is_err());
        assert!(serde_json::from_str::<TaggedProjectionGeneration>("\"bad\\n\"").is_err());
    }

    #[test]
    fn exact_work_overflow_precedence_is_pinned() {
        let limits = TaggedAnnWorkLimits::new(12, 10, 20, 5, 1, 1, 2).unwrap();

        assert_eq!(tagged_exact_work_overflow(10, 10, 2, limits).unwrap(), None);
        assert_eq!(
            tagged_exact_work_overflow(13, 11, 2, limits).unwrap(),
            Some(TaggedExactWorkLimit::RawMemberships)
        );
        assert_eq!(
            tagged_exact_work_overflow(11, 11, 1, limits).unwrap(),
            Some(TaggedExactWorkLimit::UniqueExactIds)
        );
        assert_eq!(
            tagged_exact_work_overflow(10, 10, 3, limits).unwrap(),
            Some(TaggedExactWorkLimit::ExactVectorComponents)
        );
        assert!(tagged_exact_work_overflow(0, 2, 2, limits).is_err());
        assert!(tagged_exact_work_overflow(10, 10, 0, limits).is_err());
    }

    #[test]
    fn negative_cosine_hit_is_valid_exact_output() {
        let query = [1.0, 0.0];
        let lanes = [active_lane(1, 0)];
        let request =
            TaggedAnnRequest::new(&query, ["tag"], &lanes, TaggedAnnWorkLimits::default()).unwrap();
        let batch = TaggedAnnBatch::new(
            vec![TaggedAnnLane {
                lane: RetrievalLifecycleLane::Primary,
                hits: vec![Scored {
                    id: NodeId(Ulid::from(1u128)),
                    score: -0.25,
                }],
                seed_coverage: TaggedSeedCoverage::ExactCosine,
            }],
            exact_work(query.len(), 1),
            TaggedProjectionGeneration::new("tag-v2").unwrap(),
        )
        .unwrap();
        assert!(
            batch
                .validate_against(&request, "vector-v3", "stale-tag-v1")
                .is_err()
        );
        batch
            .validate_against(&request, "vector-v3", "tag-v2")
            .unwrap();

        let unordered = TaggedAnnBatch::new(
            vec![TaggedAnnLane {
                lane: RetrievalLifecycleLane::Primary,
                hits: vec![
                    Scored {
                        id: NodeId(Ulid::from(2u128)),
                        score: -0.5,
                    },
                    Scored {
                        id: NodeId(Ulid::from(3u128)),
                        score: 0.5,
                    },
                ],
                seed_coverage: TaggedSeedCoverage::ExactCosine,
            }],
            exact_work(query.len(), 2),
            TaggedProjectionGeneration::new("tag-v2").unwrap(),
        );
        assert!(unordered.is_err());
    }

    #[test]
    fn exact_coverage_rejects_potential_component_overflow_before_hydration() {
        let query = [1.0, 0.0];
        let lanes = [active_lane(1, 0)];
        let limits = TaggedAnnWorkLimits::new(10, 10, 10, 5, 1, 1, 2).unwrap();
        let request = TaggedAnnRequest::new(&query, ["tag"], &lanes, limits).unwrap();
        let mut work = exact_work(query.len(), 1);
        work.raw_memberships = 6;
        work.unique_exact_ids = 6;
        let batch = TaggedAnnBatch::new(
            vec![TaggedAnnLane {
                lane: RetrievalLifecycleLane::Primary,
                hits: vec![Scored {
                    id: NodeId(Ulid::from(1u128)),
                    score: 0.5,
                }],
                seed_coverage: TaggedSeedCoverage::ExactCosine,
            }],
            work,
            TaggedProjectionGeneration::new("tag-v2").unwrap(),
        )
        .unwrap();
        assert!(
            batch
                .validate_against(&request, "vector-v3", "tag-v2")
                .is_err()
        );
    }

    fn partial_fixture() -> (TaggedAnnRequest<'static>, TaggedAnnBatch) {
        static QUERY: [f32; 2] = [1.0, 0.0];
        static LANES: std::sync::LazyLock<[TaggedAnnLaneRequest; 1]> =
            std::sync::LazyLock::new(|| {
                [TaggedAnnLaneRequest::new(
                    RetrievalLifecycleLane::Primary,
                    StatusFilter::ACTIVE,
                    1,
                    3,
                )
                .unwrap()]
            });
        let limits = TaggedAnnWorkLimits::new(10, 10, 20, 10, 3, 3, 6).unwrap();
        let request = TaggedAnnRequest::new(&QUERY, ["zeta", "alpha"], &*LANES, limits).unwrap();
        let pivot = tagged_sample_pivot(
            &request,
            TaggedPhysicalStatus::Active,
            RetrievalLifecycleLane::Primary,
            "vector-v3",
        )
        .unwrap();
        let work = TaggedAnnWork {
            query_dimension: 2,
            raw_memberships: 11,
            unique_exact_ids: 11,
            exact_hydrated_ids: 0,
            exact_vector_components: 0,
            fallback_hnsw_inspected: 1,
            fallback_sample_inspected: 2,
            fallback_unique_ids: 3,
            fallback_canonical_candidates_checked: 3,
            fallback_matching_candidates: 3,
            fallback_hydrated_ids: 3,
            fallback_vector_components: 6,
        };
        let batch = TaggedAnnBatch::new(
            vec![TaggedAnnLane {
                lane: RetrievalLifecycleLane::Primary,
                hits: vec![Scored {
                    id: NodeId(Ulid::from(2u128)),
                    score: 0.5,
                }],
                seed_coverage: TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter {
                    exceeded_limit: TaggedExactWorkLimit::RawMemberships,
                    raw_memberships: 11,
                    physical: vec![
                        TaggedPhysicalSeedCoverage::new(
                            TaggedPhysicalStatus::Active,
                            1,
                            1,
                            vec![
                                TaggedQueryTagSeedCoverage::new(0, 1, 1).unwrap(),
                                TaggedQueryTagSeedCoverage::new(1, 1, 1).unwrap(),
                            ],
                            pivot,
                        )
                        .unwrap(),
                    ],
                    canonical_candidates_checked: 3,
                    matching_candidates: 3,
                },
            }],
            work,
            TaggedProjectionGeneration::new("tag-v2").unwrap(),
        )
        .unwrap();
        (request, batch)
    }

    #[test]
    fn partial_output_validates_canaries_quota_split_and_prefix_discard() {
        let (request, batch) = partial_fixture();
        batch
            .validate_against(&request, "vector-v3", "tag-v2")
            .unwrap();

        let mut too_far = batch.clone();
        too_far.work.raw_memberships = 12;
        assert!(
            too_far
                .validate_against(&request, "vector-v3", "tag-v2")
                .is_err()
        );

        let mut wrong_pivot = batch.clone();
        let TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter { physical, .. } =
            &mut wrong_pivot.lanes[0].seed_coverage
        else {
            unreachable!()
        };
        physical[0].sample_pivot ^= 1;
        assert!(
            wrong_pivot
                .validate_against(&request, "vector-v3", "tag-v2")
                .is_err()
        );

        let mut wrong_overflow = batch.clone();
        let TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter {
            exceeded_limit, ..
        } = &mut wrong_overflow.lanes[0].seed_coverage
        else {
            unreachable!()
        };
        *exceeded_limit = TaggedExactWorkLimit::UniqueExactIds;
        assert!(
            wrong_overflow
                .validate_against(&request, "vector-v3", "tag-v2")
                .is_err()
        );

        let mut retained_prefix = batch.clone();
        retained_prefix.work.exact_hydrated_ids = 1;
        retained_prefix.work.exact_vector_components = 2;
        assert!(
            retained_prefix
                .validate_against(&request, "vector-v3", "tag-v2")
                .is_err()
        );

        let mut wrong_split = batch;
        let TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter { physical, .. } =
            &mut wrong_split.lanes[0].seed_coverage
        else {
            unreachable!()
        };
        physical[0].hnsw_quota = 2;
        assert!(
            wrong_split
                .validate_against(&request, "vector-v3", "tag-v2")
                .is_err()
        );
    }

    #[test]
    fn partial_query_tags_cannot_borrow_each_others_sample_quota() {
        let (request, mut batch) = partial_fixture();
        let TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter { physical, .. } =
            &mut batch.lanes[0].seed_coverage
        else {
            unreachable!()
        };
        physical[0].tag_samples[0].inspected = 2;
        physical[0].tag_samples[1].inspected = 0;

        assert_eq!(physical[0].sample_inspected().unwrap(), 2);
        assert!(matches!(
            batch.validate_against(&request, "vector-v3", "tag-v2"),
            Err(Error::InvalidInput(message)) if message.contains("exceeds its own quota")
        ));
    }

    #[test]
    fn partial_query_tag_coverage_is_positional_and_quota_exact() {
        let (request, batch) = partial_fixture();

        let mut shifted_quota = batch.clone();
        let TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter { physical, .. } =
            &mut shifted_quota.lanes[0].seed_coverage
        else {
            unreachable!()
        };
        physical[0].tag_samples[0].quota = 0;
        physical[0].tag_samples[0].inspected = 0;
        physical[0].tag_samples[1].quota = 2;
        physical[0].tag_samples[1].inspected = 2;
        assert!(shifted_quota.validate().is_ok());
        assert!(matches!(
            shifted_quota.validate_against(&request, "vector-v3", "tag-v2"),
            Err(Error::InvalidInput(message)) if message.contains("per-query-tag sample quotas")
        ));

        let mut wrong_index = batch;
        let TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter { physical, .. } =
            &mut wrong_index.lanes[0].seed_coverage
        else {
            unreachable!()
        };
        physical[0].tag_samples[1].query_tag_index = 0;
        assert!(matches!(
            wrong_index.validate_against(&request, "vector-v3", "tag-v2"),
            Err(Error::InvalidInput(message)) if message.contains("indexes must be exactly")
        ));
    }
    #[test]
    fn successor_status_and_lane_shapes_are_closed() {
        assert!(serde_json::from_str::<RetrievalLifecycleLane>("\"probationary\"").is_err());
        assert!(serde_json::from_str::<TaggedPhysicalStatus>("\"candidate\"").is_err());
        assert!(
            serde_json::from_str::<StatusFilter>(
                r#"{"active":true,"archived":false,"candidates":true}"#
            )
            .is_err()
        );
        assert_eq!(
            TaggedPhysicalStatus::ALL,
            [TaggedPhysicalStatus::Active, TaggedPhysicalStatus::Archived]
        );
        let query = [1.0, 0.0];
        let lanes = [active_lane(1, 1), active_lane(1, 1)];
        assert!(
            TaggedAnnRequest::new(&query, ["tag"], &lanes, TaggedAnnWorkLimits::default()).is_err()
        );
        let single = [active_lane(1, 1)];
        let request =
            TaggedAnnRequest::new(&query, ["tag"], &single, TaggedAnnWorkLimits::default())
                .unwrap();
        let quotas = tagged_physical_status_quotas(
            &request,
            RetrievalLifecycleLane::Primary,
            "rank-only-v4",
        )
        .unwrap();
        assert_eq!(quotas.len(), 1);
        assert_eq!(quotas[0].status, TaggedPhysicalStatus::Active);
        assert_eq!(quotas[0].candidates, 1);
    }
}
