//! mneme-cozo — the graph backend adapter.
//!
//! Implements four ports — [`GraphStore`], [`VectorIndex`], [`LexicalIndex`],
//! and [`Traversal`] —
//! over a single in-process store ([`MemStore`]). One struct backs all three so
//! the vector half can read node tags for filtered ANN and the traversal half
//! can walk the edge relation directly; the daemon hands the *same* `Arc` to the
//! engine under three different trait objects.
//!
//! The crate is named for its production target: cozo (a transactional
//! relational-graph-vector DB with Datalog, native HNSW, PageRank and
//! community-detection algos). [`SCHEMA`] is an illustrative CozoScript sketch,
//! not durable schema authority; persistent conventional-unmanaged validation
//! uses the closed typed specification in `storage_contract`. Spreading
//! activation becomes a recursive Datalog
//! rule (and community detection a native algo call) once cozo's `graph-algo`
//! feature builds again; until then both run in Rust — a weighted BFS and weighted
//! Louvain ([`weighted_louvain`]). [`MemStore`] is the reference backend: zero
//! native deps, so the whole system builds, tests and runs without a RocksDB/C++
//! toolchain. Both speak only the ports, so swapping one for the other is a
//! one-line change in the daemon.

#[cfg(feature = "cozo")]
mod canonical_node_contract;
#[cfg(feature = "cozo")]
mod cozo_store;
#[cfg(feature = "cozo")]
mod storage_contract;
#[cfg(feature = "cozo")]
mod tag_projection;
#[cfg(feature = "cozo")]
mod vector_projection;
#[cfg(feature = "cozo")]
pub use canonical_node_contract::MAX_CANONICAL_NODE_JSON_BYTES;
#[cfg(feature = "cozo")]
pub use cozo_store::{
    BackendActivity, BackendActivityGuard, CozoStore, FreshCurrentMaterializationErrorV1,
    FreshCurrentMaterializationFailurePhaseV1, FreshCurrentMaterializationResultV1,
    FreshCurrentTargetPublicationStateV1,
};

mod mem_episodes;
mod mem_touchstones;
mod routing_probe;
#[cfg(test)]
mod touchstone_tests;

pub(crate) use mem_episodes::validate_episode_import;

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use mneme_core::concern::*;
use mneme_core::managed::{
    DatabaseId, ManagedSchemaVersion, ManagedStoreHead, MutationEpoch, StorageIdentity,
    WriterSchemaVersion,
};
use mneme_core::ports::{
    Budget, CaptureCommitOutcome, ClusterId, ColdPath, DensePruneChunkOutcome,
    EmbeddingFingerprintInit, EmbeddingMetadataStore, Error, FeedbackCommit, FeedbackCommitOutcome,
    FullMergeCommit, FullMergeCommitOutcome, GraphStore, IncidentEdgeLeg, IncidentEdgeReadWork,
    IncidentEdgesCursor, IncidentEdgesPage, IncidentEdgesRequest, LexicalIndex,
    MAX_MAINTENANCE_BATCH_ROWS, MaintenanceCommit, MaintenanceCommitOutcome, MaintenanceEdgeKey,
    MaintenanceEdgeMutation, MaintenanceEdgePage, MaintenanceNodePage, Neighbor, Result, Scored,
    StatusFilter, SupersedeCommit, SupersedeCommitOutcome, TaggedAnnBatch, TaggedAnnLane,
    TaggedAnnRequest, TaggedAnnWork, TaggedExactWorkLimit, TaggedFallbackStrategy,
    TaggedPhysicalSeedCoverage, TaggedPhysicalStatus, TaggedProjectionGeneration,
    TaggedQueryTagSeedCoverage, TaggedSeedCoverage, Traversal, TraversalScope, VectorIndex,
    stable_tag_sample_hash, tagged_exact_work_overflow, tagged_fallback_leg_quotas,
    tagged_physical_status_quotas, tagged_query_tag_quotas, tagged_sample_pivot,
};
use mneme_core::touchstone::*;
use mneme_core::{
    CaptureReplayProof, CaptureSource, Contradiction, Edge, EmbeddingFingerprint, FullMergeRecord,
    MAX_FEEDBACK_RETRY_RECORDS, MAX_INCIDENT_EDGES, MAX_NODE_HYDRATION_BATCH,
    MAX_NODE_STATUS_BATCH, MAX_REMOTE_EDGE_PAGE_SIZE, MAX_REMOTE_EDGES_PER_SOURCE, MemoryKind,
    MergeCandidate, MergeResolution, Node, NodeId, NodeStatus, Provenance, RemoteEdge,
    RemoteEdgeCursor, RemoteEdgePage, Resolution, SupersedeRecord, Timestamp,
};
use serde::{Deserialize, Serialize};
use ulid::Ulid;
use unordered_pair::UnorderedPair;

pub(crate) fn linked_read_remaining(
    started: std::time::Instant,
    allowance: std::time::Duration,
) -> Result<std::time::Duration> {
    allowance
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| Error::Backend("linked read deadline exhausted".into()))
}

/// Both adapters use the same fair physical-leg scheduler. A full admitted
/// batch leaves its tail unknown; only a short/empty bounded seek proves done.
/// No over-budget lookahead, union materialization, or endpoint classification.
pub(crate) fn collect_incident_edges_page(
    request: &IncidentEdgesRequest,
    mut read: impl FnMut(
        IncidentEdgeLeg,
        Option<MaintenanceEdgeKey>,
        usize,
        &mut IncidentEdgeReadWork,
    ) -> Result<Vec<Edge>>,
) -> Result<IncidentEdgesPage> {
    request.validate()?;
    let cursor = request
        .after()
        .cloned()
        .unwrap_or_else(|| IncidentEdgesCursor::new(request.anchor()));
    let mut after = [cursor.outgoing_after(), cursor.incoming_after()];
    let mut done = [cursor.outgoing_done(), cursor.incoming_done()];
    let mut next = if cursor.next_leg() == IncidentEdgeLeg::Outgoing {
        0
    } else {
        1
    };
    if done[next] {
        next = 1 - next;
    }
    let first = next;
    let both = !done[0] && !done[1];
    let first_quota = if both {
        request.scan_rows().div_ceil(2)
    } else {
        request.scan_rows()
    };
    let mut items = Vec::with_capacity(request.scan_rows());
    let mut work = IncidentEdgeReadWork::default();
    for step in 0..3 {
        let leg_index = if step == 1 { 1 - first } else { first };
        let remaining = request.scan_rows() - work.rows_scanned;
        if remaining == 0 || done[leg_index] {
            continue;
        }
        let quota = if step == 0 {
            first_quota
        } else if step == 1 && !done[first] {
            request.scan_rows() / 2
        } else if step == 2 && !done[1 - first] {
            0
        } else {
            remaining
        };
        if quota == 0 {
            continue;
        }
        let leg = if leg_index == 0 {
            IncidentEdgeLeg::Outgoing
        } else {
            IncidentEdgeLeg::Incoming
        };
        let rows = read(leg, after[leg_index], quota, &mut work)?;
        if rows.len() > quota {
            return Err(Error::Backend(
                "incident seek exceeded admitted row quota".into(),
            ));
        }
        for edge in &rows {
            if (leg_index == 0 && edge.from != request.anchor())
                || (leg_index == 1 && edge.to != request.anchor())
            {
                return Err(Error::Backend("incident seek escaped its anchor".into()));
            }
        }
        work.rows_scanned += rows.len();
        if let Some(last) = rows.last() {
            after[leg_index] = Some(MaintenanceEdgeKey::from_edge(last));
        }
        done[leg_index] = rows.len() < quota;
        items.extend(rows);
        // Rotate the leading leg between pages (including the odd-row extra).
        next = 1 - first;
    }
    let next = if done[0] && done[1] {
        None
    } else {
        Some(IncidentEdgesCursor::resume(
            request.anchor(),
            after[0],
            after[1],
            done[0],
            done[1],
            if next == 0 {
                IncidentEdgeLeg::Outgoing
            } else {
                IncidentEdgeLeg::Incoming
            },
        )?)
    };
    Ok(IncidentEdgesPage { items, next, work })
}

/// One-hop fan-out per node during a spread. The budget's depth/node caps are
/// the real limits; this just keeps a hub node from dominating one frontier.
const SPREAD_FANOUT: usize = 8;
/// Raw-edge shortlist considered before query-conditioned fanout selection.
/// This lets a semantically relevant neighbor survive a saturated hub without
/// turning conditioning into unbounded vector work.
const SPREAD_CONDITION_CANDIDATES: usize = SPREAD_FANOUT * 4;
/// Max local-moving passes in [`weighted_louvain`] before stopping (it converges
/// in a handful; this only bounds a pathological graph).
const LOUVAIN_MAX_PASSES: usize = 32;
/// In-memory tag membership is rebuilt from canonical nodes on every load.
const MEM_TAGGED_PROJECTION_GENERATION: &str = "mneme-mem-semantic-tag-membership-v3";

/// Explicit predecessor-JSON admission limits; normal successor snapshots use
/// their own read path. The byte cap is checked both before and during reading
/// so a growing file cannot bypass the metadata check.
pub const MAX_LEGACY_JSON_UPGRADE_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_LEGACY_JSON_UPGRADE_NODES: usize = 100_000;

type TagSampleKey = (i64, NodeId);
type TagStatusProjection = BTreeMap<String, BTreeSet<TagSampleKey>>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TagProjection {
    active: TagStatusProjection,
    archived: TagStatusProjection,
}

impl TagProjection {
    fn status(&self, status: TaggedPhysicalStatus) -> &TagStatusProjection {
        match status {
            TaggedPhysicalStatus::Active => &self.active,
            TaggedPhysicalStatus::Archived => &self.archived,
        }
    }

    fn status_mut(&mut self, status: TaggedPhysicalStatus) -> &mut TagStatusProjection {
        match status {
            TaggedPhysicalStatus::Active => &mut self.active,
            TaggedPhysicalStatus::Archived => &mut self.archived,
        }
    }

    fn index_node(&mut self, node: &Node, sample: TagSampleKey) {
        if !node.is_semantic() {
            return;
        }
        let projection = self.status_mut(node.status().into());
        for tag in node.tags() {
            projection.entry(tag.to_owned()).or_default().insert(sample);
        }
    }

    fn unindex_node(&mut self, node: &Node, sample: TagSampleKey) {
        let projection = self.status_mut(node.status().into());
        for tag in node.tags() {
            let remove_bucket = projection.get_mut(tag).is_some_and(|members| {
                members.remove(&sample);
                members.is_empty()
            });
            if remove_bucket {
                projection.remove(tag);
            }
        }
    }

    fn indexes_node_exactly(&self, node: &Node, sample: TagSampleKey) -> bool {
        if !node.is_semantic() {
            return !self.contains_sample(sample);
        }
        let expected_status = TaggedPhysicalStatus::from(node.status());
        for status in TaggedPhysicalStatus::ALL {
            for (tag, members) in self.status(status) {
                let expected = status == expected_status && node.has_tag(tag);
                if members.contains(&sample) != expected {
                    return false;
                }
            }
        }
        node.tags().all(|tag| {
            self.status(expected_status)
                .get(tag)
                .is_some_and(|members| members.contains(&sample))
        })
    }

    fn contains_sample(&self, sample: TagSampleKey) -> bool {
        TaggedPhysicalStatus::ALL.into_iter().any(|status| {
            self.status(status)
                .values()
                .any(|members| members.contains(&sample))
        })
    }

    fn remove_sample_everywhere(&mut self, sample: TagSampleKey) {
        for status in TaggedPhysicalStatus::ALL {
            self.status_mut(status).retain(|_, members| {
                members.remove(&sample);
                !members.is_empty()
            });
        }
    }
}
pub(crate) fn incident_edge_capacity_error() -> Error {
    Error::CapacityExceeded {
        resource: "incident edge degree",
        limit: MAX_INCIDENT_EDGES,
    }
}

pub(crate) fn remote_edge_source_capacity_error() -> Error {
    Error::CapacityExceeded {
        resource: "remote edges per source",
        limit: MAX_REMOTE_EDGES_PER_SOURCE,
    }
}

pub(crate) fn feedback_retry_capacity_error() -> Error {
    Error::CapacityExceeded {
        resource: "feedback retry records",
        limit: MAX_FEEDBACK_RETRY_RECORDS,
    }
}

pub(crate) fn validate_cosine_vector(values: &[f32], role: &str) -> Result<()> {
    if !values.iter().all(|value| value.is_finite()) {
        return Err(Error::InvalidInput(format!(
            "{role} contains a non-finite value; rebuild or replace it with finite f32 values"
        )));
    }

    // Keep this accumulation in f32 deliberately: mnestic's cosine/HNSW path
    // accumulates f32 squared norms, so a mathematically finite f64 norm is not
    // sufficient. Accepting a vector whose f32 norm overflows would still feed
    // `dot / sqrt(inf)` (or NaN in nearby arithmetic) into the index.
    let squared_norm = values
        .iter()
        .fold(0.0_f32, |norm, value| norm + value * value);
    if !(squared_norm.is_finite() && squared_norm > 0.0) {
        return Err(Error::InvalidInput(format!(
            "{role} has a non-positive or non-finite f32 squared L2 norm; cosine similarity requires a non-zero vector whose f32 norm does not overflow"
        )));
    }
    Ok(())
}

const LEGACY_FEEDBACK_EPOCH: &str = "legacy-wall-clock-v1";

fn legacy_feedback_epoch() -> String {
    LEGACY_FEEDBACK_EPOCH.into()
}

/// Validate the semantic postcondition represented by durable full-merge
/// proofs before an import mutates its destination. Proofs deliberately do not
/// require either endpoint to remain present: later forgetting is legitimate.
/// If the loser is still present, however, the collapse must have archived it.
pub(crate) fn validate_full_merge_import(export: &StoreExport) -> Result<()> {
    let mut candidates = HashMap::with_capacity(export.merges.len());
    for candidate in &export.merges {
        if candidates
            .insert(candidate.between, candidate.resolution)
            .is_some()
        {
            return Err(Error::InvalidInput(
                "import contains duplicate merge candidates".into(),
            ));
        }
    }

    let mut proof_pairs = HashSet::with_capacity(export.full_merge_commits.len());
    let mut losers = HashSet::with_capacity(export.full_merge_commits.len());
    for record in &export.full_merge_commits {
        record.validate().map_err(Error::InvalidInput)?;
        if !proof_pairs.insert(record.between) {
            return Err(Error::InvalidInput(
                "import contains duplicate full merge records".into(),
            ));
        }
        if candidates.get(&record.between) != Some(&Some(MergeResolution::Full)) {
            return Err(Error::InvalidInput(
                "full merge proof requires one matching candidate resolved Full".into(),
            ));
        }
        losers.insert(record.loser);
    }
    if export
        .nodes
        .iter()
        .any(|node| losers.contains(&node.id()) && !node.is_archived())
    {
        return Err(Error::InvalidInput(
            "full merge proof requires its retained loser to be archived".into(),
        ));
    }
    let episode_ids: HashSet<_> = export
        .nodes
        .iter()
        .filter(|node| !node.is_semantic())
        .map(Node::id)
        .collect();
    if export.edges.iter().any(|edge| {
        (losers.contains(&edge.from) || losers.contains(&edge.to))
            && !episode_ids.contains(&edge.from)
            && !episode_ids.contains(&edge.to)
    }) {
        return Err(Error::InvalidInput(
            "full merge proof requires no semantic local edge incident to its loser".into(),
        ));
    }
    if export
        .remote_edges
        .iter()
        .any(|edge| losers.contains(&edge.from))
    {
        return Err(Error::InvalidInput(
            "full merge proof requires no remote edge sourced by its loser".into(),
        ));
    }
    Ok(())
}

/// Validate durable supersession proofs before import. A proof without the
/// matching terminal contradiction could turn a crafted snapshot into a false
/// `AlreadyApplied` result and suppress the actual graph mutation. This checks
/// semantic coherence, not authenticity: snapshots are caller-controlled, so a
/// deployment that accepts untrusted imports must authenticate the whole file.
pub(crate) fn validate_supersede_import(export: &StoreExport) -> Result<()> {
    let mut contradictions = HashMap::with_capacity(export.contradictions.len());
    for contradiction in &export.contradictions {
        if contradictions
            .insert(contradiction.between, contradiction.resolution)
            .is_some()
        {
            return Err(Error::InvalidInput(
                "import contains duplicate contradictions".into(),
            ));
        }
    }

    let mut proof_pairs = HashSet::with_capacity(export.supersede_commits.len());
    for record in &export.supersede_commits {
        record.validate().map_err(Error::InvalidInput)?;
        if !proof_pairs.insert(record.between) {
            return Err(Error::InvalidInput(
                "import contains duplicate supersede records".into(),
            ));
        }
        if contradictions.get(&record.between) != Some(&Some(Resolution::Superseded)) {
            return Err(Error::InvalidInput(
                "supersede proof requires one matching contradiction resolved Superseded".into(),
            ));
        }
    }
    Ok(())
}

fn validate_graph_import(export: &StoreExport, local_db: Ulid) -> Result<()> {
    let nodes: HashSet<_> = export.nodes.iter().map(Node::id).collect();

    for edge in &export.edges {
        edge.validate().map_err(Error::InvalidInput)?;
    }
    for contradiction in &export.contradictions {
        contradiction.validate().map_err(Error::InvalidInput)?;
        if contradiction.is_open() {
            validate_open_overlay_endpoints(contradiction.between, "contradiction", |id| {
                nodes.contains(&id)
            })?;
        }
    }
    for candidate in &export.merges {
        candidate.validate().map_err(Error::InvalidInput)?;
        if candidate.is_open() {
            validate_open_overlay_endpoints(candidate.between, "merge candidate", |id| {
                nodes.contains(&id)
            })?;
        }
    }
    for edge in &export.remote_edges {
        edge.validate_for_source_database(local_db)
            .map_err(Error::InvalidInput)?;
    }
    Ok(())
}

// Raw text budgets total 3328 bytes; worst-case JSON escaping is 6x. Evidence
// framing bounds its count (<32); 64-byte hex digests + keys remain far below
// this generous byte ceiling. It is not a concern-degree or semantic limit.
pub(crate) use mneme_core::MAX_CONCERN_ROW_JSON_BYTES;
pub(crate) fn encode_concern_row(row: &ConcernRow) -> Result<String> {
    let data = serde_json::to_string(row)
        .map_err(|e| Error::InvalidInput(format!("invalid concern: {e}")))?;
    if data.len() > MAX_CONCERN_ROW_JSON_BYTES {
        return Err(Error::InvalidInput(
            "concern row exceeds serialized byte ceiling".into(),
        ));
    }
    Ok(data)
}

pub(crate) fn validate_concern_import(export: &StoreExport) -> Result<()> {
    let ids: HashSet<_> = export.nodes.iter().map(Node::id).collect();
    let mut keys = BTreeSet::new();
    for row in &export.concerns {
        // Roundtrip exercises every closed constructor-checked nested DTO.
        let checked: ConcernRow = serde_json::from_str(&encode_concern_row(row)?)
            .map_err(|e| Error::InvalidInput(format!("invalid concern: {e}")))?;
        let key = checked.binding().key();
        if !key.endpoints().iter().all(|id| ids.contains(id)) || !keys.insert(key) {
            return Err(Error::InvalidInput(
                "concern has missing endpoints or duplicate key".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_vector_import(export: &StoreExport) -> Result<()> {
    let mut nodes = HashSet::with_capacity(export.nodes.len());
    for node in &export.nodes {
        node.validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        if !nodes.insert(node.id()) {
            return Err(Error::InvalidInput(format!(
                "import contains duplicate node {}",
                node.id().0
            )));
        }
    }

    let mut vectors = HashSet::with_capacity(export.vectors.len());
    for (id, vector) in &export.vectors {
        if !nodes.contains(id) {
            return Err(Error::InvalidInput(format!(
                "import contains a vector without node {}",
                id.0
            )));
        }
        if !vectors.insert(*id) {
            return Err(Error::InvalidInput(format!(
                "import contains duplicate vector {}",
                id.0
            )));
        }
        if vector.len() != export.dim {
            return Err(Error::DimMismatch {
                index: export.dim,
                provider: vector.len(),
            });
        }
        validate_cosine_vector(vector, &format!("vector for {}", id.0))?;
    }
    Ok(())
}

fn same_serialized<T: Serialize>(left: &T, right: &T) -> Result<bool> {
    let left = serde_json::to_vec(left)
        .map_err(|error| Error::Backend(format!("serialize feedback precondition: {error}")))?;
    let right = serde_json::to_vec(right)
        .map_err(|error| Error::Backend(format!("serialize feedback precondition: {error}")))?;
    Ok(left == right)
}

fn same_vector_bits(left: &[f32], right: &[f32]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.to_bits() == right.to_bits())
}

/// The CozoScript the relations here map onto. Run once at init against a real
/// `cozo::Db`. Illustrative — tune types/fields as the schema settles.
pub const SCHEMA: &str = r#"
# nodes
:create node {
    id: String
    =>
    summary: String,
    body_ref: String,
    stability: Float,
    confidence: Float,
    status: String,          # "active" | "archived"
    created: Int,
    last_exposed: Int?,
    exposure_count: Int,
    last_grounded_use: Int?,
    grounded_use_count: Int,
}

# tags: many-per-node, queried for filtered ANN + adversarial-agent entry
:create node_tag { id: String, tag: String }
::index create node_tag:by_tag { tag }

# sparse retrieval projection. Separate lifecycle indexes keep top-k exact;
# by_status is also required by the current mnestic FTS update path to retire stale postings.
:create node_search { id: String => summary: String, status: String }
::index create node_search:by_status { status }
::fts create node_search:active_fts {
    extractor: summary, extract_filter: status == 'active',
    tokenizer: Simple, filters: [Lowercase]
}
::fts create node_search:archived_fts {
    extractor: summary, extract_filter: status == 'archived',
    tokenizer: Simple, filters: [Lowercase]
}

# edges (HOT PATH only — no contradiction here). Identity is the (from, to)
# pair; weight is the derived strength, trials + interference its raw signals.
:create edge {
    from: String,
    to: String,
    =>
    weight: Float,
    kind: String,            # associative | bridge | supersedes | derived_from
    anchor_start: Int?,
    anchor_end: Int?,
    last_reinforced: Int,
    trials: Int,
    interference: Int,
}

# contradiction overlay (COLD PATH). canonical unordered pair: lo < hi.
# never traversed during retrieval; consumed by reconciliation.
:create contradiction {
    lo: String,
    hi: String,
    =>
    observations: Int,
    first_seen: Int,
    last_seen: Int,
    resolution: String?,     # null | superseded | context_dependent | unresolved
}

# merge-candidate overlay (COLD PATH). same shape as contradiction, fed by
# not-new feedback; consumed by the merge pass.
:create merge_candidate {
    lo: String,
    hi: String,
    =>
    observations: Int,
    first_seen: Int,
    last_seen: Int,
    resolution: String?,     # null | full | partial | keep
}

# durable ordered outcome for atomic full collapse; canonical pair is the key.
:create full_merge_commit {
    lo: String,
    hi: String,
    =>
    winner: String,
    loser: String,
    applied_at: Int,
}

# durable ordered outcome for atomic supersession; canonical pair is the key.
:create supersede_commit {
    lo: String,
    hi: String,
    =>
    winner: String,
    loser: String,
    applied_at: Int,
}

# bounded operational inbox for at-most-once receipt-feedback retries.
:create feedback_retry {
    key: String,
    =>
    fingerprint: String,
    applied_at: Int,         # legacy column; v2 stores sequence, never wall time
}

:create feedback_retry_order {
    epoch: String,
    sequence: Int,
    key: String,
    =>
    marker: Bool,
}

# vectors: one canonical projection, partitioned into exact lifecycle lanes at
# index time. Dimension is pinned; changing provider width requires a rebuild.
:create node_vec { id: String => e: <F32; 768>, status: String }

::hnsw create node_vec:active_idx {
    dim: 768, m: 16, ef_construction: 200, fields: [e], distance: Cosine,
    filter: status == 'active',
}
::hnsw create node_vec:archived_idx {
    dim: 768, m: 16, ef_construction: 200, fields: [e], distance: Cosine,
    filter: status == 'archived',
}
"#;

/// In-process reference backend. Interior-mutable behind a single mutex; every
/// port method is sync work wrapped in an `async fn`, so the lock is never held
/// across an `.await`.
pub struct MemStore {
    dim: usize,
    /// Stable identity of this database, stamped on creation and persisted —
    /// what a logical name resolves to, and what a cross-db edge references.
    db_id: Ulid,
    inner: Mutex<Inner>,
}

type RemoteEdgeKey = (NodeId, Ulid, NodeId);
type RemoteEdgeOrderKey = (Reverse<u32>, Ulid, NodeId);

/// Reference-only managed-generation metadata. It deliberately has no serde
/// shape: the legacy JSON export is semantic migration data, not an authenticated
/// managed artifact or exact-generation restore format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ManagedReferenceMetadata {
    storage: StorageIdentity,
    managed_schema_version: ManagedSchemaVersion,
    minimum_writer_schema: WriterSchemaVersion,
    mutation_epoch: MutationEpoch,
}

fn remote_edge_order_key(edge: &RemoteEdge) -> RemoteEdgeOrderKey {
    (
        Reverse(edge.weight().to_bits()),
        edge.target_db,
        edge.target,
    )
}

fn remote_edge_cursor_key(cursor: RemoteEdgeCursor) -> RemoteEdgeOrderKey {
    (
        Reverse(cursor.weight_bits()),
        cursor.target_db(),
        cursor.target(),
    )
}

fn validate_remote_edge_page_request(
    from: NodeId,
    after: Option<RemoteEdgeCursor>,
    limit: usize,
) -> Result<()> {
    if !(1..=MAX_REMOTE_EDGE_PAGE_SIZE).contains(&limit) {
        return Err(Error::InvalidInput(format!(
            "remote edge page limit must be in 1..={MAX_REMOTE_EDGE_PAGE_SIZE}"
        )));
    }
    if let Some(cursor) = after {
        cursor.validate_for(from).map_err(Error::InvalidInput)?;
    }
    Ok(())
}

fn finish_remote_edge_page(mut items: Vec<RemoteEdge>, limit: usize) -> RemoteEdgePage {
    let has_more = items.len() > limit;
    items.truncate(limit);
    let next = if has_more {
        items.last().map(RemoteEdgeCursor::from_edge)
    } else {
        None
    };
    RemoteEdgePage { items, next }
}

fn validate_maintenance_page_limit(limit: usize) -> Result<()> {
    if !(1..=MAX_MAINTENANCE_BATCH_ROWS).contains(&limit) {
        return Err(Error::InvalidInput(format!(
            "maintenance page limit must be in 1..={MAX_MAINTENANCE_BATCH_ROWS}"
        )));
    }
    Ok(())
}

fn finish_maintenance_node_page(mut items: Vec<Node>, limit: usize) -> MaintenanceNodePage {
    let has_more = items.len() > limit;
    items.truncate(limit);
    let next = has_more.then(|| items.last().expect("non-empty bounded node page").id());
    MaintenanceNodePage { items, next }
}

fn finish_maintenance_edge_page(mut items: Vec<Edge>, limit: usize) -> MaintenanceEdgePage {
    let has_more = items.len() > limit;
    items.truncate(limit);
    let next = has_more
        .then(|| MaintenanceEdgeKey::from_edge(items.last().expect("non-empty bounded edge page")));
    MaintenanceEdgePage { items, next }
}

fn pair_contains(pair: UnorderedPair<NodeId>, id: NodeId) -> bool {
    pair.0 == id || pair.1 == id
}

fn validate_open_overlay_endpoints(
    between: UnorderedPair<NodeId>,
    overlay: &str,
    contains_node: impl Fn(NodeId) -> bool,
) -> Result<()> {
    if !contains_node(between.0) || !contains_node(between.1) {
        return Err(Error::InvalidInput(format!(
            "open {overlay} {} <-> {} has a missing node endpoint",
            between.0.0, between.1.0
        )));
    }
    Ok(())
}

#[derive(Default)]
struct Inner {
    /// Present only for the explicit ephemeral managed-reference constructor.
    /// Keeping the epoch under this same mutex makes each reference mutation and
    /// its one successor indivisible to concurrent callers.
    managed_reference: Option<ManagedReferenceMetadata>,
    nodes: BTreeMap<NodeId, Node>,
    touchstones: BTreeMap<NodeId, TouchstoneRecord>,
    touchstone_targets: BTreeSet<(NodeId, NodeId)>,
    /// Canonical-node-derived tag membership, sharded by physical lifecycle.
    /// Hash-ordered members support bounded wraparound sampling without an
    /// `O(N)` skip. This projection is rebuilt on import and never serialized.
    tag_projection: TagProjection,
    /// Rebuilt canonical episode projections; never snapshot authority.
    episode_projection: mem_episodes::EpisodeProjection,
    /// Hot-path edges, keyed by directed endpoints. `Associative` edges are
    /// stored once; [`Dir::Both`] surfaces them from either endpoint.
    edges: BTreeMap<(NodeId, NodeId), Edge>,
    /// Derived endpoint projection over `edges`. Each directed edge key is
    /// indexed under both endpoints (once for a self-loop), so hot-path
    /// adjacency reads scale with incident degree rather than total edge count.
    /// This is rebuilt from `edges` on import and deliberately never serialized.
    edge_keys_by_endpoint: HashMap<NodeId, HashSet<(NodeId, NodeId)>>,
    /// Disposable incoming keyset, exactly mirroring native edge:by_to.
    /// Rebuilt from canonical edges on load/import; never serialized.
    edge_keys_by_to: BTreeSet<(NodeId, NodeId)>,
    vectors: HashMap<NodeId, Vec<f32>>,
    /// Identity of the model + adapters that produced `vectors`. `None` means a
    /// legacy store or an interrupted rebuild, never "compatible by default".
    embedding_fingerprint: Option<EmbeddingFingerprint>,
    /// Cold-path overlay. `UnorderedPair` keying collapses (a,b) and (b,a).
    contradictions: HashMap<UnorderedPair<NodeId>, Contradiction>,
    /// Redundancy overlay, same shape — fed by not-new feedback.
    merges: HashMap<UnorderedPair<NodeId>, MergeCandidate>,
    /// Durable direction/idempotency proof for committed full collapses. Keyed
    /// by the same canonical pair as `merges` so the opposite direction cannot
    /// later masquerade as an unrelated operation.
    full_merge_commits: HashMap<UnorderedPair<NodeId>, FullMergeRecord>,
    /// Durable direction/idempotency proof for committed supersessions.
    supersede_commits: HashMap<UnorderedPair<NodeId>, SupersedeRecord>,
    /// Cross-db edges, keyed by `(from, target_db, target)` so a re-link upserts.
    remote_edges: HashMap<RemoteEdgeKey, RemoteEdge>,
    /// Exact source-owned projection over `remote_edges`. This is the storage
    /// admission index as well as the bounded read path; it is rebuilt from
    /// canonical rows on import and never serialized.
    remote_edge_keys_by_source: HashMap<NodeId, BTreeSet<RemoteEdgeOrderKey>>,
    /// Bounded in-process retry proofs for receipt-feedback transactions. The
    /// physical key remains the bare receipt-set key, but replay identity is
    /// logically `(epoch, key)`: admitting a new epoch atomically replaces any
    /// old-generation row with the same physical key.
    /// Exports expose them for diagnostics, but load/import deliberately starts
    /// a fresh authority generation and never rehydrates these records. While a
    /// managed reference store is live, insert/GC of these proofs is durable
    /// reference state and advances its mutation epoch like any other relation.
    feedback_retries: HashMap<String, FeedbackRetryRecord>,
    concerns: BTreeMap<(NodeId, NodeId, ConcernKind), ConcernRow>,
    concern_incoming: BTreeSet<(NodeId, NodeId, ConcernKind)>,
}

impl Inner {
    /// Compute the successor before publishing any durable reference mutation.
    /// Conventional stores and semantic no-ops have no managed epoch to update.
    fn preflight_epoch_advance(&self, changed: bool) -> Result<Option<MutationEpoch>> {
        if !changed {
            return Ok(None);
        }
        self.managed_reference
            .map(|metadata| {
                metadata
                    .mutation_epoch
                    .checked_successor()
                    .map_err(|error| {
                        Error::Conflict(format!(
                            "managed reference mutation epoch cannot advance: {error}"
                        ))
                    })
            })
            .transpose()
    }

    /// Publish a successor only after the operation's complete, infallible
    /// durable delta. `next` can be `Some` only for a managed reference store.
    fn finish_epoch_advance(&mut self, next: Option<MutationEpoch>) {
        if let Some(next) = next {
            self.managed_reference
                .as_mut()
                .expect("managed epoch successor requires managed metadata")
                .mutation_epoch = next;
        }
    }

    fn replace_node(&mut self, node: Node) {
        let node_id = node.id();
        let sample = (stable_tag_sample_hash(node_id), node_id);
        if let Some(previous) = self.nodes.remove(&node_id) {
            self.tag_projection.unindex_node(&previous, sample);
        }
        self.tag_projection.index_node(&node, sample);
        self.nodes.insert(node_id, node);
    }

    fn replace_node_repairing_projection(&mut self, node: Node) {
        let node_id = node.id();
        let sample = (stable_tag_sample_hash(node_id), node_id);
        self.nodes.remove(&node_id);
        self.tag_projection.remove_sample_everywhere(sample);
        self.tag_projection.index_node(&node, sample);
        self.nodes.insert(node_id, node);
    }

    fn remove_node(&mut self, id: NodeId) -> Option<Node> {
        let removed = self.nodes.remove(&id)?;
        let sample = (stable_tag_sample_hash(id), id);
        self.tag_projection.unindex_node(&removed, sample);
        Some(removed)
    }

    fn update_existing_node(&mut self, id: NodeId, update: impl FnOnce(&mut Node)) {
        let mut node = self
            .nodes
            .remove(&id)
            .expect("node update was prevalidated");
        let sample = (stable_tag_sample_hash(id), id);
        self.tag_projection.unindex_node(&node, sample);
        update(&mut node);
        self.tag_projection.index_node(&node, sample);
        self.nodes.insert(id, node);
    }

    fn set_existing_node_status(&mut self, id: NodeId, status: NodeStatus) {
        self.update_existing_node(id, |node| node.set_status(status));
    }

    fn rebuild_tag_projection(&mut self) {
        let mut rebuilt = TagProjection::default();
        for node in self.nodes.values() {
            let sample = (stable_tag_sample_hash(node.id()), node.id());
            rebuilt.index_node(node, sample);
        }
        self.tag_projection = rebuilt;
    }

    fn index_edge_key(&mut self, key: (NodeId, NodeId)) {
        self.edge_keys_by_to.insert((key.1, key.0));
        self.edge_keys_by_endpoint
            .entry(key.0)
            .or_default()
            .insert(key);
        if key.1 != key.0 {
            self.edge_keys_by_endpoint
                .entry(key.1)
                .or_default()
                .insert(key);
        }
    }

    fn unindex_endpoint(&mut self, endpoint: NodeId, key: (NodeId, NodeId)) {
        let remove_bucket = self
            .edge_keys_by_endpoint
            .get_mut(&endpoint)
            .is_some_and(|keys| {
                keys.remove(&key);
                keys.is_empty()
            });
        if remove_bucket {
            self.edge_keys_by_endpoint.remove(&endpoint);
        }
    }

    fn unindex_edge_key(&mut self, key: (NodeId, NodeId)) {
        self.edge_keys_by_to.remove(&(key.1, key.0));
        self.unindex_endpoint(key.0, key);
        if key.1 != key.0 {
            self.unindex_endpoint(key.1, key);
        }
    }

    fn validate_edge_upsert_capacity(&self, key: (NodeId, NodeId)) -> Result<()> {
        if !self.edges.contains_key(&key) {
            for endpoint in [Some(key.0), (key.1 != key.0).then_some(key.1)]
                .into_iter()
                .flatten()
            {
                if self
                    .edge_keys_by_endpoint
                    .get(&endpoint)
                    .is_some_and(|keys| keys.len() >= MAX_INCIDENT_EDGES)
                {
                    return Err(incident_edge_capacity_error());
                }
            }
        }
        Ok(())
    }

    fn put_edge_prevalidated(&mut self, edge: Edge) {
        let key = (edge.from, edge.to);
        self.edges.insert(key, edge);
        // An update has the same canonical endpoints, but inserting into the set
        // again is cheap and makes this helper repair a missing derived entry.
        self.index_edge_key(key);
    }

    fn delete_edge(&mut self, key: (NodeId, NodeId)) {
        self.edges.remove(&key);
        // Also clean a stale projection if a damaged/imported store somehow has
        // the key indexed without the canonical edge.
        self.unindex_edge_key(key);
    }

    fn rebuild_edge_index(&mut self) -> Result<()> {
        let mut rebuilt: HashMap<NodeId, HashSet<(NodeId, NodeId)>> = HashMap::new();
        for key in self.edges.keys().copied() {
            for endpoint in [Some(key.0), (key.1 != key.0).then_some(key.1)]
                .into_iter()
                .flatten()
            {
                let keys = rebuilt.entry(endpoint).or_default();
                if keys.len() >= MAX_INCIDENT_EDGES {
                    return Err(incident_edge_capacity_error());
                }
                keys.insert(key);
            }
        }
        self.edge_keys_by_to = self.edges.keys().map(|&(from, to)| (to, from)).collect();
        self.edge_keys_by_endpoint = rebuilt;
        Ok(())
    }

    fn incident_edges(&self, id: NodeId) -> Vec<Edge> {
        self.edge_keys_by_endpoint
            .get(&id)
            .into_iter()
            .flatten()
            .filter_map(|key| self.edges.get(key).cloned())
            .collect()
    }

    fn full_merge_incident_edges(&self, winner: NodeId, loser: NodeId) -> Result<Vec<Edge>> {
        let mut keys = HashSet::new();
        for endpoint in [winner, loser] {
            if let Some(indexed) = self.edge_keys_by_endpoint.get(&endpoint) {
                if indexed.len() > MAX_INCIDENT_EDGES {
                    return Err(incident_edge_capacity_error());
                }
                keys.extend(indexed.iter().copied());
            }
        }
        keys.into_iter()
            .map(|key| {
                self.edges
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| Error::Backend("incident edge index is inconsistent".into()))
            })
            .collect()
    }

    fn remote_edges_for_source(&self, from: NodeId) -> Result<Vec<RemoteEdge>> {
        let Some(index) = self.remote_edge_keys_by_source.get(&from) else {
            return Ok(Vec::new());
        };
        if index.len() > MAX_REMOTE_EDGES_PER_SOURCE {
            return Err(remote_edge_source_capacity_error());
        }
        index
            .iter()
            .map(|(_, target_db, target)| {
                self.remote_edges
                    .get(&(from, *target_db, *target))
                    .cloned()
                    .ok_or_else(|| {
                        Error::Backend("remote edge source index is inconsistent".into())
                    })
            })
            .collect()
    }

    fn validate_remote_edge_upsert_capacity(&self, edge: &RemoteEdge) -> Result<()> {
        let source_index = self.remote_edge_keys_by_source.get(&edge.from);
        let matching_identity_rows = source_index.map_or(0, |keys| {
            keys.iter()
                .filter(|(_, target_db, target)| {
                    *target_db == edge.target_db && *target == edge.target
                })
                .count()
        });
        let final_index_len = source_index
            .map_or(0, BTreeSet::len)
            .saturating_sub(matching_identity_rows)
            .saturating_add(1);
        if final_index_len > MAX_REMOTE_EDGES_PER_SOURCE {
            return Err(remote_edge_source_capacity_error());
        }
        Ok(())
    }

    fn remote_edge_index_matches_exactly(&self, edge: &RemoteEdge) -> bool {
        let expected = remote_edge_order_key(edge);
        let Some(keys) = self.remote_edge_keys_by_source.get(&edge.from) else {
            return false;
        };
        let mut matching = keys.iter().filter(|(_, target_db, target)| {
            *target_db == edge.target_db && *target == edge.target
        });
        matching.next().is_some_and(|key| *key == expected) && matching.next().is_none()
    }

    fn put_remote_edge_prevalidated(&mut self, edge: RemoteEdge) {
        let key = (edge.from, edge.target_db, edge.target);
        self.remote_edges.insert(key, edge.clone());
        let source_index = self.remote_edge_keys_by_source.entry(key.0).or_default();
        // Weight bits order rows but do not identify an edge. Purge every stale
        // duplicate for this canonical identity before publishing its one exact
        // ordering row.
        source_index.retain(|(_, target_db, target)| !(*target_db == key.1 && *target == key.2));
        source_index.insert(remote_edge_order_key(&edge));
    }

    fn delete_remote_edge(&mut self, key: RemoteEdgeKey) {
        let removed = self.remote_edges.remove(&key);
        let remove_bucket = self
            .remote_edge_keys_by_source
            .get_mut(&key.0)
            .is_some_and(|keys| {
                if let Some(edge) = removed.as_ref() {
                    keys.remove(&remote_edge_order_key(edge));
                }
                // Also repair an orphan/stale ordering row for this canonical
                // identity. The weight bits are ordering data, not identity.
                keys.retain(|(_, target_db, target)| !(*target_db == key.1 && *target == key.2));
                keys.is_empty()
            });
        if remove_bucket {
            self.remote_edge_keys_by_source.remove(&key.0);
        }
    }

    fn rebuild_remote_edge_index(&mut self, local_db: Ulid) -> Result<()> {
        let mut rebuilt: HashMap<NodeId, BTreeSet<RemoteEdgeOrderKey>> = HashMap::new();
        for edge in self.remote_edges.values() {
            edge.validate_for_source_database(local_db)
                .map_err(Error::InvalidInput)?;
            let keys = rebuilt.entry(edge.from).or_default();
            if keys.len() >= MAX_REMOTE_EDGES_PER_SOURCE {
                return Err(remote_edge_source_capacity_error());
            }
            keys.insert(remote_edge_order_key(edge));
        }
        self.remote_edge_keys_by_source = rebuilt;
        Ok(())
    }

    fn remote_edges_page(
        &self,
        from: NodeId,
        after: Option<RemoteEdgeCursor>,
        limit: usize,
    ) -> Result<RemoteEdgePage> {
        validate_remote_edge_page_request(from, after, limit)?;
        let Some(index) = self.remote_edge_keys_by_source.get(&from) else {
            return Ok(RemoteEdgePage {
                items: Vec::new(),
                next: None,
            });
        };
        if index.len() > MAX_REMOTE_EDGES_PER_SOURCE {
            return Err(remote_edge_source_capacity_error());
        }

        let mut items = Vec::with_capacity(limit.saturating_add(1));
        let start = after.map(remote_edge_cursor_key);
        let ordered = match start.as_ref() {
            Some(start) => {
                index.range((std::ops::Bound::Excluded(start), std::ops::Bound::Unbounded))
            }
            None => index.range::<RemoteEdgeOrderKey, _>(..),
        };
        for (_, target_db, target) in ordered.take(limit + 1) {
            let key = (from, *target_db, *target);
            let edge = self
                .remote_edges
                .get(&key)
                .ok_or_else(|| Error::Backend("remote edge order index is inconsistent".into()))?;
            items.push(edge.clone());
        }
        Ok(finish_remote_edge_page(items, limit))
    }
}

impl MemStore {
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            db_id: Ulid::new(),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Construct an empty, ephemeral managed **reference** store with an exact
    /// caller-supplied generation identity and initial epoch.
    ///
    /// This exists to falsify managed mutation semantics without pretending the
    /// in-memory adapter is a durable managed deployment. [`MemStore::save`]
    /// refuses this mode, and [`MemStore::export`] intentionally omits storage
    /// identity and epoch. Exact managed restore requires a future authenticated
    /// native artifact; `StoreExport -> MemStore::from_export` instead creates a
    /// conventional semantic migration/replacement that retains only the logical
    /// [`DatabaseId`].
    pub fn new_ephemeral_managed_reference(
        dim: usize,
        database_id: DatabaseId,
        storage: StorageIdentity,
        managed_schema_version: ManagedSchemaVersion,
        minimum_writer_schema: WriterSchemaVersion,
        initial_epoch: MutationEpoch,
    ) -> Self {
        Self {
            dim,
            db_id: database_id.get(),
            inner: Mutex::new(Inner {
                managed_reference: Some(ManagedReferenceMetadata {
                    storage,
                    managed_schema_version,
                    minimum_writer_schema,
                    mutation_epoch: initial_epoch,
                }),
                ..Inner::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("mem store mutex poisoned")
    }

    fn linked_read_lock(
        &self,
        started: std::time::Instant,
        allowance: std::time::Duration,
    ) -> Result<std::sync::MutexGuard<'_, Inner>> {
        loop {
            linked_read_remaining(started, allowance)?;
            match self.inner.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(Error::Backend("mem store mutex poisoned".into()));
                }
                Err(std::sync::TryLockError::WouldBlock) => std::thread::yield_now(),
            }
        }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// This database's stable id.
    pub fn db_id(&self) -> Ulid {
        self.db_id
    }

    /// Observe the conventional or managed reference head under the same mutex
    /// that serializes every mutation and managed epoch successor.
    pub fn observed_managed_head(&self) -> ManagedStoreHead {
        let database_id = DatabaseId::new(self.db_id)
            .expect("MemStore constructors and import reject the nil database id");
        let g = self.lock();
        match g.managed_reference {
            Some(metadata) => ManagedStoreHead::managed(
                database_id,
                metadata.storage,
                metadata.managed_schema_version,
                metadata.minimum_writer_schema,
                metadata.mutation_epoch,
            ),
            None => ManagedStoreHead::unmanaged(database_id),
        }
    }

    /// Clone out a lossless, self-describing export for migration or verification.
    /// Unlike the old tuple return value, identity and every relation are explicit,
    /// so adding a semantic store feature cannot silently make migrations lossy.
    /// This deliberately retains the logical database id but omits managed storage
    /// identity, schema/fence metadata, and mutation epoch. Importing it is a
    /// conventional semantic migration/replacement, never exact managed restore.
    pub fn export(&self) -> StoreExport {
        let g = self.lock();
        self.export_locked(&g)
    }

    fn export_locked(&self, g: &Inner) -> StoreExport {
        StoreExport {
            db_id: self.db_id,
            dim: self.dim,
            embedding_fingerprint: g.embedding_fingerprint.clone(),
            nodes: g.nodes.values().cloned().collect(),
            edges: g.edges.values().cloned().collect(),
            vectors: g.vectors.iter().map(|(id, v)| (*id, v.clone())).collect(),
            contradictions: g.contradictions.values().cloned().collect(),
            merges: g.merges.values().cloned().collect(),
            full_merge_commits: g.full_merge_commits.values().copied().collect(),
            supersede_commits: g.supersede_commits.values().copied().collect(),
            remote_edges: g.remote_edges.values().cloned().collect(),
            feedback_retries: g.feedback_retries.values().cloned().collect(),
            concerns: g.concerns.values().cloned().collect(),
            touchstones: g.touchstones.values().cloned().collect(),
        }
    }

    /// Snapshot the whole graph to a JSON file. This is what makes the reference
    /// store usable from a one-shot CLI: load on startup, save after a mutation,
    /// and the graph survives between invocations. (Tuple/pair map keys don't
    /// round-trip as JSON objects, so the snapshot flattens the maps to lists.)
    /// Managed reference stores refuse this conventional semantic envelope because
    /// it cannot authenticate or exactly restore storage identity and epoch.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let snap = {
            let g = self.lock();
            if g.managed_reference.is_some() {
                return Err(Error::Conflict(
                    "ephemeral managed reference stores cannot be saved through conventional JSON"
                        .into(),
                ));
            }
            self.export_locked(&g)
        };
        let bytes = serde_json::to_vec_pretty(&StoreExportEnvelopeV5::new(snap))
            .map_err(|e| Error::Backend(format!("serialize snapshot: {e}")))?;
        atomic_replace(path.as_ref(), &bytes)
    }

    /// Rebuild a store from a snapshot written by [`MemStore::save`].
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let bytes =
            std::fs::read(path).map_err(|e| Error::Backend(format!("read snapshot: {e}")))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| Error::InvalidInput(format!("invalid snapshot: {e}")))?;
        match value.get("schema").and_then(|schema| schema.as_str()) {
            Some(STORE_EXPORT_SCHEMA_V5) => {},
            Some(STORE_EXPORT_SCHEMA_V4) => return Err(Error::InvalidInput(
                "EpisodeContextV2 JSON predecessor requires explicit single-graph-upgrade --target-generation touchstones-v1".into())),
            Some(STORE_EXPORT_SCHEMA_V3) => return Err(Error::InvalidInput(
                "ConcernV1 JSON predecessor requires explicit single-graph-upgrade --target-generation episode-context-v2".into())),
            Some(STORE_EXPORT_SCHEMA_V2) => return Err(Error::InvalidInput(
                "SingleGraphV1 JSON predecessor requires explicit single-graph-upgrade --target-generation concern-v1".into())),
            _ => return Err(Error::InvalidInput("unsupported store-export schema".into())),
        }
        if value.get("store").and_then(|v| v.get("concerns")).is_none() {
            return Err(Error::InvalidInput(
                "v5 snapshot requires concerns field".into(),
            ));
        }
        if value
            .get("store")
            .and_then(|v| v.get("touchstones"))
            .is_none()
        {
            return Err(Error::InvalidInput(
                "v5 snapshot requires touchstones field".into(),
            ));
        }
        let envelope: StoreExportEnvelopeV5 = serde_json::from_value(value)
            .map_err(|e| Error::Backend(format!("parse snapshot: {e}")))?;
        Self::from_export_unchecked(envelope.into_store()?)
    }

    /// Frozen EpisodeContextV2 predecessor output, never drops touchstone rows.
    pub fn save_episode_context_v4(&self, path: impl AsRef<Path>) -> Result<()> {
        let g = self.lock();
        if g.managed_reference.is_some() {
            return Err(Error::Conflict(
                "ephemeral managed reference stores cannot be saved through conventional JSON"
                    .into(),
            ));
        }
        let bytes = serde_json::to_vec_pretty(&StoreExportEnvelopeV4::new(self.export_locked(&g)))
            .map_err(|e| Error::InvalidInput(format!("serialize predecessor: {e}")))?;
        atomic_replace(path.as_ref(), &bytes)
    }
    /// Named read-only predecessor decode; ordinary load refuses v4.
    pub fn load_episode_context_v4(path: impl AsRef<Path>) -> Result<Self> {
        let bytes =
            std::fs::read(path).map_err(|e| Error::Backend(format!("load predecessor: {e}")))?;
        let envelope: StoreExportEnvelopeV4 = serde_json::from_slice(&bytes)
            .map_err(|e| Error::InvalidInput(format!("invalid predecessor snapshot: {e}")))?;
        Self::from_export_unchecked(envelope.into_store()?)
    }

    /// Frozen ConcernV1 JSON output. New episode metadata cannot be written
    /// under an old marker, even when there are no concern rows.
    pub fn save_concern_v3(&self, path: impl AsRef<Path>) -> Result<()> {
        let snap = {
            let g = self.lock();
            if g.managed_reference.is_some() {
                return Err(Error::Conflict(
                    "ephemeral managed reference stores cannot be saved through conventional JSON"
                        .into(),
                ));
            }
            self.export_locked(&g)
        };
        validate_pre_context_export(&snap)?;
        let bytes = serde_json::to_vec_pretty(&StoreExportEnvelopeV3::new(snap))
            .map_err(|e| Error::Backend(format!("serialize predecessor: {e}")))?;
        atomic_replace(path.as_ref(), &bytes)
    }

    /// Exact, read-only ConcernV1 predecessor admission. Never accepts v4 or
    /// silently normalizes a successor field carried under a v3 marker.
    pub fn load_concern_v3(path: impl AsRef<Path>) -> Result<Self> {
        let bytes =
            std::fs::read(path).map_err(|e| Error::Backend(format!("load predecessor: {e}")))?;
        let envelope: StoreExportEnvelopeV3 = serde_json::from_slice(&bytes)
            .map_err(|e| Error::InvalidInput(format!("invalid predecessor snapshot: {e}")))?;
        Self::from_export_unchecked(envelope.into_store()?)
    }

    /// Explicit historical output; never silently discards successor rows or
    /// managed identity.
    pub fn save_single_graph_v2(&self, path: impl AsRef<Path>) -> Result<()> {
        let snap = {
            let g = self.lock();
            if g.managed_reference.is_some() {
                return Err(Error::Conflict(
                    "ephemeral managed reference stores cannot be saved through conventional JSON"
                        .into(),
                ));
            }
            self.export_locked(&g)
        };
        if !snap.concerns.is_empty() {
            return Err(Error::InvalidInput(
                "cannot export concerns into v2 predecessor".into(),
            ));
        }
        validate_pre_context_export(&snap)?;
        let mut value = serde_json::to_value(StoreExportEnvelopeV2::new(snap))
            .map_err(|e| Error::Backend(format!("serialize predecessor: {e}")))?;
        value["store"]
            .as_object_mut()
            .expect("serialized export object")
            .remove("concerns");
        atomic_replace(
            path.as_ref(),
            &serde_json::to_vec_pretty(&value)
                .map_err(|e| Error::Backend(format!("serialize predecessor: {e}")))?,
        )
    }

    /// Explicit, read-only predecessor decode. Ordinary `load` never accepts v2.
    pub fn load_single_graph_v2(path: impl AsRef<Path>) -> Result<Self> {
        let bytes =
            std::fs::read(path).map_err(|e| Error::Backend(format!("load predecessor: {e}")))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| Error::InvalidInput(format!("invalid predecessor snapshot: {e}")))?;
        if value
            .get("store")
            .and_then(|v| v.as_object())
            .is_some_and(|v| v.contains_key("concerns"))
        {
            return Err(Error::InvalidInput(
                "v2 predecessor cannot contain concerns".into(),
            ));
        }
        let envelope: StoreExportEnvelopeV2 = serde_json::from_value(value)
            .map_err(|e| Error::InvalidInput(format!("invalid predecessor snapshot: {e}")))?;
        Self::from_export(envelope.into_store()?)
    }

    pub fn load_legacy_v1(path: impl AsRef<Path>) -> Result<Self> {
        let file = std::fs::File::open(path)
            .map_err(|e| Error::Backend(format!("open legacy snapshot: {e}")))?;
        let metadata = file
            .metadata()
            .map_err(|e| Error::Backend(format!("stat legacy snapshot: {e}")))?;
        if !metadata.is_file() {
            return Err(Error::InvalidInput(
                "legacy snapshot must be a regular file".into(),
            ));
        }
        if metadata.len() > MAX_LEGACY_JSON_UPGRADE_SOURCE_BYTES {
            return Err(Error::InvalidInput(
                "legacy snapshot exceeds byte limit".into(),
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_LEGACY_JSON_UPGRADE_SOURCE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Error::Backend(format!("read legacy snapshot: {e}")))?;
        if bytes.len() as u64 > MAX_LEGACY_JSON_UPGRADE_SOURCE_BYTES {
            return Err(Error::InvalidInput(
                "legacy snapshot exceeds byte limit".into(),
            ));
        }
        let legacy: LegacyStoreExportV1 = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Backend(format!("parse legacy snapshot: {e}")))?;
        Self::from_export(legacy.into_current()?)
    }

    /// Construct a detached store from a validated export. Re-embedding uses
    /// this to assemble a complete replacement without mutating the live
    /// snapshot. [`MemStore::load`] remains permissive about malformed
    /// vector payloads so they can be repaired explicitly. The named
    /// predecessor-JSON loader applies its own byte and node admission caps
    /// before calling this method.
    pub fn from_export(snap: StoreExport) -> Result<Self> {
        if let Some(fingerprint) = &snap.embedding_fingerprint {
            fingerprint
                .validate()
                .map_err(Error::InvalidEmbeddingFingerprint)?;
            if fingerprint.dimension != snap.dim {
                return Err(Error::DimMismatch {
                    index: snap.dim,
                    provider: fingerprint.dimension,
                });
            }
        }
        validate_vector_import(&snap)?;
        Self::from_export_unchecked(snap)
    }

    fn from_export_unchecked(snap: StoreExport) -> Result<Self> {
        mem_touchstones::validate_touchstone_import(&snap)?;
        validate_episode_import(&snap)?;
        validate_full_merge_import(&snap)?;
        validate_supersede_import(&snap)?;
        let database_id = DatabaseId::new(snap.db_id)
            .map_err(|error| Error::InvalidInput(format!("invalid database id: {error}")))?;
        validate_graph_import(&snap, database_id.get())?;
        validate_concern_import(&snap)?;
        let store = Self {
            dim: snap.dim,
            db_id: database_id.get(),
            inner: Mutex::new(Inner::default()),
        };
        {
            let mut g = store.lock();
            g.embedding_fingerprint = snap.embedding_fingerprint;
            for node in snap.nodes {
                g.nodes.insert(node.id(), node);
            }
            // Legacy snapshot loading is deliberately last-write-wins for
            // duplicate node IDs. Rebuild only after the final canonical map
            // exists so stale earlier duplicates cannot leak memberships.
            g.rebuild_tag_projection();
            g.episode_projection = mem_episodes::EpisodeProjection::rebuild(&g.nodes)?;
            for edge in snap.edges {
                g.edges.insert((edge.from, edge.to), edge);
            }
            g.rebuild_edge_index()?;
            for (id, v) in snap.vectors {
                g.vectors.insert(id, v);
            }
            for c in snap.contradictions {
                g.contradictions.insert(c.between, c);
            }
            for m in snap.merges {
                g.merges.insert(m.between, m);
            }
            for record in snap.full_merge_commits {
                record.validate().map_err(Error::InvalidInput)?;
                if g.full_merge_commits
                    .insert(record.between, record)
                    .is_some()
                {
                    return Err(Error::InvalidInput(
                        "snapshot contains duplicate full merge records".into(),
                    ));
                }
            }
            for record in snap.supersede_commits {
                record.validate().map_err(Error::InvalidInput)?;
                if g.supersede_commits.insert(record.between, record).is_some() {
                    return Err(Error::InvalidInput(
                        "snapshot contains duplicate supersede records".into(),
                    ));
                }
            }
            for r in snap.remote_edges {
                let r = RemoteEdge::new(r.from, r.target_db, r.target, r.weight());
                g.remote_edges.insert((r.from, r.target_db, r.target), r);
            }
            g.rebuild_remote_edge_index(store.db_id)?;
            // Receipt capabilities and their authority epoch are process-local.
            // Snapshot bytes may expose live records for diagnostics, but a load
            // starts a new authority generation and therefore imports no retry
            // proof. This prevents crafted snapshots from manufacturing an
            // `AlreadyApplied` result for a capability that cannot exist.
            for row in snap.concerns {
                let key = row.binding().key();
                let [lo, hi] = key.endpoints();
                g.concern_incoming.insert((hi, lo, key.kind()));
                g.concerns.insert((lo, hi, key.kind()), row);
            }
            for record in snap.touchstones {
                g.insert_touchstone(record);
            }
            drop(snap.feedback_retries);
        }
        Ok(store)
    }
}

/// Write bytes to a unique same-directory file, durably finish that inode, then
/// atomically replace the destination. Every fallible operation before `rename`
/// leaves the old destination untouched; the temporary is best-effort cleaned.
fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_replace_with(path, bytes, |_| Ok(()))
}

fn atomic_replace_with(
    path: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| Error::Backend(format!("snapshot path has no file name: {path:?}")))?
        .to_string_lossy();

    let mut reserved = None;
    for _ in 0..16 {
        let candidate = parent.join(format!(".{name}.tmp-{}", Ulid::new()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                reserved = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(Error::Backend(format!(
                    "create snapshot temp beside {path:?}: {error}"
                )));
            }
        }
    }
    let (temp, mut file) = reserved.ok_or_else(|| {
        Error::Backend(format!(
            "could not reserve a unique snapshot temp beside {path:?}"
        ))
    })?;
    let mut cleanup = TempCleanup(Some(temp.clone()));

    let write_result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        before_rename(&temp)?;
        std::fs::rename(&temp, path)?;
        cleanup.0 = None;
        sync_parent_best_effort(parent);
        Ok(())
    })();
    write_result
        .map_err(|error| Error::Backend(format!("atomically write snapshot {path:?}: {error}")))
}

struct TempCleanup(Option<PathBuf>);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn sync_parent_best_effort(_parent: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(_parent) {
        let _ = dir.sync_all();
    }
}

/// A deterministic pseudo-random draw in `[0, 1)` for an edge `a -> b`, used by
/// both spread backends for ε-exploration (see [`Budget::explore`]). Keyed by the
/// pair so it's stable per edge and reproducible, but varies across edges. A tiny
/// xorshift64*, no rng dependency.
pub(crate) fn explore_draw(a: NodeId, b: NodeId) -> f32 {
    let mut x = ((a.0.0 as u64) ^ (b.0.0 as u64).rotate_left(32)) | 1;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    ((v >> 40) as f32) / ((1u64 << 24) as f32)
}

/// Map an ANN similarity (∈ [0,1], or the floor 0 for a node outside the query's
/// neighbourhood) to a gentle spreading-activation multiplier in `[1-γ, 1]` — the
/// query-conditioning attenuation. γ=0 yields 1.0 (no conditioning).
pub(crate) fn conditioning_factor(gamma: f32, sim: f32) -> f32 {
    (1.0 - gamma) + gamma * sim.clamp(0.0, 1.0)
}

/// Apply the graph's directional semantics to the incident edges of one node.
/// Lifecycle filtering deliberately happens after this step and before sorting
/// or fan-out, so a forbidden neighbour cannot steal a traversal slot.
pub(crate) fn oriented_neighbors(id: NodeId, edges: &[Edge]) -> Vec<Neighbor> {
    let outgoing: HashSet<NodeId> = edges
        .iter()
        .filter(|edge| edge.from == id && edge.kind.is_undirected())
        .map(|edge| edge.to)
        .collect();
    let mut out = Vec::new();
    for edge in edges {
        if edge.from == id && edge.kind.traverses_outgoing() {
            out.push(Neighbor {
                edge: edge.clone(),
                node: edge.to,
                incoming: false,
            });
        } else if edge.to == id
            && edge.kind.traverses_incoming()
            && (!edge.kind.is_undirected() || !outgoing.contains(&edge.from))
        {
            out.push(Neighbor {
                edge: edge.clone(),
                node: edge.from,
                incoming: true,
            });
        }
    }
    out
}

pub(crate) fn neighbor_order(a: &Neighbor, b: &Neighbor) -> Ordering {
    b.edge
        .weight()
        .total_cmp(&a.edge.weight())
        .then_with(|| a.node.cmp(&b.node))
        .then_with(|| a.incoming.cmp(&b.incoming))
        .then_with(|| a.edge.from.cmp(&b.edge.from))
        .then_with(|| a.edge.to.cmp(&b.edge.to))
}

pub(crate) fn scored_order(a: &Scored, b: &Scored) -> Ordering {
    b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id))
}

/// Reverse-rank wrapper for a max-heap whose root is the worst retained hit.
/// `scored_order` itself is the protocol order (best first), so its greatest
/// element is exactly the deterministic eviction candidate.
#[derive(Clone, Copy, Debug)]
struct WorstFirstScored(Scored);

impl PartialEq for WorstFirstScored {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id && self.0.score.to_bits() == other.0.score.to_bits()
    }
}

impl Eq for WorstFirstScored {}

impl PartialOrd for WorstFirstScored {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WorstFirstScored {
    fn cmp(&self, other: &Self) -> Ordering {
        scored_order(&self.0, &other.0)
    }
}

fn retain_tagged_top_k(heap: &mut BinaryHeap<WorstFirstScored>, hit: Scored, k: usize) {
    if k == 0 {
        return;
    }
    let ranked = WorstFirstScored(hit);
    if heap.len() < k {
        heap.push(ranked);
    } else if heap.peek().is_some_and(|worst| ranked < *worst) {
        *heap.peek_mut().expect("nonzero full top-k heap") = ranked;
    }
    debug_assert!(heap.len() <= k);
}

/// Retain the strongest score proposed for one node. Traversal layers collect
/// into a map before admitting anything so hash iteration order cannot decide
/// which child consumes a bounded slot.
pub(crate) fn propose_score(scores: &mut HashMap<NodeId, f32>, id: NodeId, score: f32) {
    scores
        .entry(id)
        .and_modify(|current| {
            if score > *current {
                *current = score;
            }
        })
        .or_insert(score);
}

/// Rank unique scores and retain at most `limit`, with node id as the stable
/// tie-break. Used for the seed layer before it consumes the traversal cap.
pub(crate) fn bounded_ranked(scores: HashMap<NodeId, f32>, limit: usize) -> Vec<Scored> {
    let mut ranked: Vec<Scored> = scores
        .into_iter()
        .map(|(id, score)| Scored { id, score })
        .collect();
    ranked.sort_by(scored_order);
    ranked.truncate(limit);
    ranked
}

/// Apply one complete layer's proposals without ever admitting more than
/// `max_nodes` unique nodes. Improvements to already-admitted nodes do not spend
/// another slot; novel nodes compete globally by score then id for the remaining
/// capacity. The returned map is the next frontier.
pub(crate) fn admit_layer(
    best: &mut HashMap<NodeId, f32>,
    proposals: HashMap<NodeId, f32>,
    max_nodes: usize,
) -> HashMap<NodeId, f32> {
    let ranked = bounded_ranked(proposals, usize::MAX);
    let mut next = HashMap::new();
    let mut novel = Vec::new();

    for proposal in ranked {
        if let Some(current) = best.get_mut(&proposal.id) {
            if proposal.score > *current {
                *current = proposal.score;
                next.insert(proposal.id, proposal.score);
            }
        } else {
            novel.push(proposal);
        }
    }

    let remaining = max_nodes.saturating_sub(best.len());
    for proposal in novel.into_iter().take(remaining) {
        best.insert(proposal.id, proposal.score);
        next.insert(proposal.id, proposal.score);
    }
    next
}

/// Compare only transient candidate priority; retain original numeric scores.
pub(crate) fn routed_scored_order(
    a: &Scored,
    b: &Scored,
    ordering: Option<&BTreeMap<NodeId, mneme_core::ports::TraversalOrdering>>,
) -> std::cmp::Ordering {
    let key = |hit: &Scored| {
        ordering
            .and_then(|map| map.get(&hit.id))
            .map_or(hit.score, |candidate| candidate.priority)
    };
    key(b).total_cmp(&key(a)).then_with(|| scored_order(a, b))
}

/// Parallel best *encountered* route, before original-score max loses arrivals.
/// No votes, inherited bias, zero contribution, or threshold-only exploration.
pub(crate) fn record_routing_order(
    ordering: &mut Option<BTreeMap<NodeId, mneme_core::ports::TraversalOrdering>>,
    biases: Option<&routing_probe::RoutingBiasMap>,
    source: Scored,
    neighbor: &Neighbor,
    original: f32,
    min_relevance: f32,
    paths: &Option<BTreeMap<NodeId, Vec<mneme_core::ports::TraversalHop>>>,
) {
    let Some(ordering) = ordering else {
        return;
    };
    if !original.is_finite() || original <= 0.0 || original < min_relevance {
        return;
    }
    let sign = biases
        .and_then(|map| {
            map.get(&mneme_core::ports::RoutingRoute::from_neighbor(
                source.id, neighbor,
            ))
        })
        .map_or(0.0, |sign| sign.value());
    let priority = original + source.score * sign;
    if !priority.is_finite() {
        return;
    }
    if ordering.get(&neighbor.node).is_some_and(|old| {
        priority < old.priority
            || (priority == old.priority && original <= old.original_contribution)
    }) {
        return;
    }
    let mut path = paths
        .as_ref()
        .and_then(|paths| paths.get(&source.id))
        .cloned()
        .unwrap_or_default();
    path.push(mneme_core::ports::TraversalHop {
        previous: source.id,
        target: neighbor.node,
        edge: neighbor.edge.clone(),
    });
    ordering.insert(
        neighbor.node,
        mneme_core::ports::TraversalOrdering {
            priority,
            original_contribution: original,
            path,
        },
    );
}

pub(crate) fn admit_layer_routed(
    best: &mut HashMap<NodeId, f32>,
    proposals: HashMap<NodeId, f32>,
    max_nodes: usize,
    ordering: Option<&BTreeMap<NodeId, mneme_core::ports::TraversalOrdering>>,
) -> HashMap<NodeId, f32> {
    if ordering.is_none() {
        return admit_layer(best, proposals, max_nodes);
    }
    let mut ranked: Vec<_> = proposals
        .into_iter()
        .map(|(id, score)| Scored { id, score })
        .collect();
    ranked.sort_by(|a, b| routed_scored_order(a, b, ordering));
    let mut next = HashMap::new();
    let mut remaining = max_nodes.saturating_sub(best.len());
    for proposal in ranked {
        if let Some(current) = best.get_mut(&proposal.id) {
            if proposal.score > *current {
                *current = proposal.score;
                next.insert(proposal.id, proposal.score);
            }
        } else if remaining > 0 {
            best.insert(proposal.id, proposal.score);
            next.insert(proposal.id, proposal.score);
            remaining -= 1;
        }
    }
    next
}

/// One level of **weighted modularity optimization** — the local-moving phase of
/// Louvain, iterated to convergence. Every node starts in its own community and
/// repeatedly moves to whichever neighbouring community most raises modularity
/// (`ΔQ = k_{i→c} − Σ_tot(c)·k_i / 2m`), until a full pass moves nobody. The win
/// over label propagation: a node joins by *modularity gain*, not by a plurality
/// of neighbour labels, and the `Σ_tot·k_i` penalty resists joining an already-
/// heavy community — so a hub wired to many *low-weight* edges is not swept into
/// one giant cluster (LPA's dense-graph failure mode). `adj` must be symmetric:
/// `adj[i]` lists `(neighbour_index, weight)`. Returns a raw community index per
/// node (densify with [`densify`]). Deterministic — ties go to the lower index,
/// independent of map iteration order.
pub(crate) fn weighted_louvain(n: usize, adj: &[Vec<(usize, f32)>]) -> Vec<usize> {
    let mut degree = vec![0f64; n];
    for (i, nbrs) in adj.iter().enumerate() {
        for &(_, w) in nbrs {
            degree[i] += w as f64;
        }
    }
    let m2: f64 = degree.iter().sum(); // 2m — the sum of weighted degrees
    let mut comm: Vec<usize> = (0..n).collect();
    if m2 == 0.0 {
        return comm; // edgeless: every node is its own community
    }
    let mut sigma_tot = degree.clone(); // total degree per community

    for _ in 0..LOUVAIN_MAX_PASSES {
        let mut moved = false;
        for i in 0..n {
            let ci = comm[i];
            let ki = degree[i];
            sigma_tot[ci] -= ki; // tentatively pull i out of its community
            // Weight from i into each neighbouring community.
            let mut k_in: HashMap<usize, f64> = HashMap::new();
            for &(j, w) in &adj[i] {
                if j != i {
                    *k_in.entry(comm[j]).or_insert(0.0) += w as f64;
                }
            }
            // Best modularity gain over candidate communities (staying included).
            let mut best_c = ci;
            let mut best_gain = k_in.get(&ci).copied().unwrap_or(0.0) - sigma_tot[ci] * ki / m2;
            for (&c, &kic) in &k_in {
                let gain = kic - sigma_tot[c] * ki / m2;
                if gain > best_gain || (gain == best_gain && c < best_c) {
                    best_gain = gain;
                    best_c = c;
                }
            }
            sigma_tot[best_c] += ki;
            comm[i] = best_c;
            moved |= best_c != ci;
        }
        if !moved {
            break;
        }
    }
    comm
}

/// Densify arbitrary community indices into contiguous [`ClusterId`]s, numbered in
/// the order nodes appear in `ids` (callers pass them sorted, so it's stable).
pub(crate) fn densify(ids: &[NodeId], comm: &[usize]) -> Vec<(NodeId, ClusterId)> {
    let mut remap: HashMap<usize, u32> = HashMap::new();
    ids.iter()
        .zip(comm)
        .map(|(id, &c)| {
            let next = remap.len() as u32;
            (*id, ClusterId(*remap.entry(c).or_insert(next)))
        })
        .collect()
}

/// Portable semantic migration form of a store. Maps are flattened to lists
/// because JSON object keys must be strings, and our keys are node ids / unordered
/// pairs. Canonical graph state is lossless; volatile feedback retry diagnostics
/// are exported but deliberately not rehydrated by load/import.
///
/// `db_id` retains logical database identity across a migration/replacement. This
/// format deliberately contains no managed storage identity, schema/fence state,
/// or mutation epoch and therefore cannot represent an exact managed restore.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreExport {
    pub db_id: Ulid,
    pub dim: usize,
    /// Missing in v1 snapshots. Populated legacy stores are rejected by normal
    /// opens until `mnemed reembed` rebuilds and stamps their vectors.
    #[serde(default)]
    pub embedding_fingerprint: Option<EmbeddingFingerprint>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub vectors: Vec<(NodeId, Vec<f32>)>,
    pub contradictions: Vec<Contradiction>,
    #[serde(default)]
    pub merges: Vec<MergeCandidate>,
    #[serde(default)]
    pub full_merge_commits: Vec<FullMergeRecord>,
    #[serde(default)]
    pub supersede_commits: Vec<SupersedeRecord>,
    #[serde(default)]
    pub remote_edges: Vec<RemoteEdge>,
    #[serde(default)]
    pub feedback_retries: Vec<FeedbackRetryRecord>,
    #[serde(default)]
    pub concerns: Vec<ConcernRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touchstones: Vec<TouchstoneRecord>,
}

impl StoreExport {
    /// Exact logical migration comparison. Flattened keyed collections and
    /// unordered tags are canonicalized; no field/value is omitted or rounded.
    pub fn canonical_value(mut self) -> Result<serde_json::Value> {
        let export = &mut self;
        export.nodes.sort_by_key(Node::id);
        export.touchstones.sort_by_key(TouchstoneRecord::owner);
        export.concerns.sort_by_key(|row| {
            let key = row.binding().key();
            let [lo, hi] = key.endpoints();
            (lo, hi, key.kind())
        });
        export.edges.sort_by_key(|edge| (edge.from, edge.to));
        export.vectors.sort_by_key(|(id, _)| *id);
        for contradiction in &mut export.contradictions {
            let (lo, hi) = if contradiction.between.0 <= contradiction.between.1 {
                (contradiction.between.0, contradiction.between.1)
            } else {
                (contradiction.between.1, contradiction.between.0)
            };
            contradiction.between = UnorderedPair(lo, hi);
        }
        export
            .contradictions
            .sort_by_key(|item| (item.between.0, item.between.1));
        for merge in &mut export.merges {
            let (lo, hi) = if merge.between.0 <= merge.between.1 {
                (merge.between.0, merge.between.1)
            } else {
                (merge.between.1, merge.between.0)
            };
            merge.between = UnorderedPair(lo, hi);
        }
        export
            .merges
            .sort_by_key(|item| (item.between.0, item.between.1));
        for record in &mut export.full_merge_commits {
            let (lo, hi) = if record.between.0 <= record.between.1 {
                (record.between.0, record.between.1)
            } else {
                (record.between.1, record.between.0)
            };
            record.between = UnorderedPair(lo, hi);
        }
        export
            .full_merge_commits
            .sort_by_key(|item| (item.between.0, item.between.1));
        for record in &mut export.supersede_commits {
            let (lo, hi) = if record.between.0 <= record.between.1 {
                (record.between.0, record.between.1)
            } else {
                (record.between.1, record.between.0)
            };
            record.between = UnorderedPair(lo, hi);
        }
        export
            .supersede_commits
            .sort_by_key(|item| (item.between.0, item.between.1));
        export
            .remote_edges
            .sort_by_key(|edge| (edge.from, edge.target_db, edge.target));
        export.feedback_retries.sort_by(|a, b| a.key.cmp(&b.key));

        let mut value = serde_json::to_value(self).map_err(|error| {
            Error::Backend(format!("serialize migration verification: {error}"))
        })?;
        normalize_export_tag_arrays(&mut value);
        Ok(value)
    }
}

fn normalize_export_tag_arrays(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if key == "tags" {
                    if let serde_json::Value::Array(tags) = value {
                        tags.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
                    }
                } else {
                    normalize_export_tag_arrays(value);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_export_tag_arrays(item);
            }
        }
        _ => {}
    }
}

pub const STORE_EXPORT_SCHEMA_V5: &str = "mneme.store-export.v5";

#[derive(Clone, Debug)]
pub struct StoreExportEnvelopeV5 {
    pub schema: String,
    pub store: StoreExport,
}
impl Serialize for StoreExportEnvelopeV5 {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut store = serde_json::to_value(&self.store).map_err(serde::ser::Error::custom)?;
        store
            .as_object_mut()
            .ok_or_else(|| serde::ser::Error::custom("invalid export object"))?
            .insert(
                "touchstones".into(),
                serde_json::to_value(&self.store.touchstones).map_err(serde::ser::Error::custom)?,
            );
        #[derive(Serialize)]
        struct Wire<'a> {
            schema: &'a str,
            store: serde_json::Value,
        }
        Wire {
            schema: &self.schema,
            store,
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for StoreExportEnvelopeV5 {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: String,
            store: serde_json::Value,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.schema != STORE_EXPORT_SCHEMA_V5
            || wire.store.get("concerns").is_none()
            || wire.store.get("touchstones").is_none()
        {
            return Err(serde::de::Error::custom(
                "v5 snapshot requires exact schema, concerns and touchstones fields",
            ));
        }
        let store = serde_json::from_value(wire.store).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema: wire.schema,
            store,
        })
    }
}
impl StoreExportEnvelopeV5 {
    pub fn new(store: StoreExport) -> Self {
        Self {
            schema: STORE_EXPORT_SCHEMA_V5.into(),
            store,
        }
    }
    pub fn into_store(self) -> Result<StoreExport> {
        if self.schema != STORE_EXPORT_SCHEMA_V5 {
            return Err(Error::InvalidInput(
                "unsupported store-export schema".into(),
            ));
        }
        mem_touchstones::validate_touchstone_import(&self.store)?;
        Ok(self.store)
    }
}

pub const STORE_EXPORT_SCHEMA_V4: &str = "mneme.store-export.v4";

#[derive(Clone, Debug)]
pub struct StoreExportEnvelopeV4 {
    pub schema: String,
    pub store: StoreExport,
}

impl Serialize for StoreExportEnvelopeV4 {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        mem_touchstones::validate_pre_touchstone_export(&self.store)
            .map_err(serde::ser::Error::custom)?;
        #[derive(Serialize)]
        struct Wire<'a> {
            schema: &'a str,
            store: &'a StoreExport,
        }
        Wire {
            schema: &self.schema,
            store: &self.store,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StoreExportEnvelopeV4 {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: String,
            store: serde_json::Value,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.schema != STORE_EXPORT_SCHEMA_V4 {
            return Err(serde::de::Error::custom(
                "unsupported store-export schema; predecessor requires explicit upgrade",
            ));
        }
        if wire.store.get("concerns").is_none() {
            return Err(serde::de::Error::custom(
                "v4 snapshot requires concerns field",
            ));
        }
        if wire.store.get("touchstones").is_some() {
            return Err(serde::de::Error::custom(
                "v4 predecessor cannot contain touchstones",
            ));
        }
        let store = serde_json::from_value(wire.store).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema: wire.schema,
            store,
        })
    }
}

impl StoreExportEnvelopeV4 {
    pub fn new(store: StoreExport) -> Self {
        Self {
            schema: STORE_EXPORT_SCHEMA_V4.into(),
            store,
        }
    }
    pub fn into_store(self) -> Result<StoreExport> {
        if self.schema != STORE_EXPORT_SCHEMA_V4 {
            return Err(Error::InvalidInput(
                "unsupported store-export schema".into(),
            ));
        }
        mem_touchstones::validate_pre_touchstone_export(&self.store)?;
        Ok(self.store)
    }
}

pub const STORE_EXPORT_SCHEMA_V3: &str = "mneme.store-export.v3";

#[derive(Clone, Debug)]
pub struct StoreExportEnvelopeV3 {
    pub schema: String,
    pub store: StoreExport,
}

impl Serialize for StoreExportEnvelopeV3 {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        validate_pre_context_export(&self.store).map_err(serde::ser::Error::custom)?;
        #[derive(Serialize)]
        struct Wire<'a> {
            schema: &'a str,
            store: &'a StoreExport,
        }
        Wire {
            schema: &self.schema,
            store: &self.store,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StoreExportEnvelopeV3 {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: String,
            store: serde_json::Value,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.schema != STORE_EXPORT_SCHEMA_V3 {
            return Err(serde::de::Error::custom(
                "unsupported store-export schema; predecessor requires explicit upgrade",
            ));
        }
        if wire.store.get("concerns").is_none() {
            return Err(serde::de::Error::custom(
                "v3 snapshot requires concerns field",
            ));
        }
        validate_pre_context_store_value(&wire.store).map_err(serde::de::Error::custom)?;
        let store = serde_json::from_value(wire.store).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema: wire.schema,
            store,
        })
    }
}

impl StoreExportEnvelopeV3 {
    pub fn new(store: StoreExport) -> Self {
        Self {
            schema: STORE_EXPORT_SCHEMA_V3.into(),
            store,
        }
    }
    pub fn into_store(self) -> Result<StoreExport> {
        if self.schema != STORE_EXPORT_SCHEMA_V3 {
            return Err(Error::InvalidInput(
                "unsupported store-export schema".into(),
            ));
        }
        validate_pre_context_export(&self.store)?;
        Ok(self.store)
    }
}

pub const STORE_EXPORT_SCHEMA_V2: &str = "mneme.store-export.v2";

/// Incompatible successor JSON envelope. The former flat parser requires a
/// top-level `dim`, so it cannot silently admit this generation as writable.
#[derive(Clone, Debug)]
pub struct StoreExportEnvelopeV2 {
    pub schema: String,
    pub store: StoreExport,
}

impl Serialize for StoreExportEnvelopeV2 {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        validate_pre_context_export(&self.store).map_err(serde::ser::Error::custom)?;
        if !self.store.concerns.is_empty() {
            return Err(serde::ser::Error::custom("v2 cannot contain concerns"));
        }
        let mut store = serde_json::to_value(&self.store).map_err(serde::ser::Error::custom)?;
        store
            .as_object_mut()
            .expect("export object")
            .remove("concerns");
        #[derive(Serialize)]
        struct Wire<'a> {
            schema: &'a str,
            store: serde_json::Value,
        }
        Wire {
            schema: &self.schema,
            store,
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for StoreExportEnvelopeV2 {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: String,
            store: serde_json::Value,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.schema != STORE_EXPORT_SCHEMA_V2 {
            return Err(serde::de::Error::custom("unsupported store-export schema"));
        }
        if wire.store.get("concerns").is_some() {
            return Err(serde::de::Error::custom("v2 cannot contain concerns field"));
        }
        validate_pre_context_store_value(&wire.store).map_err(serde::de::Error::custom)?;
        let store = serde_json::from_value(wire.store).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema: wire.schema,
            store,
        })
    }
}

impl StoreExportEnvelopeV2 {
    pub fn new(store: StoreExport) -> Self {
        Self {
            schema: STORE_EXPORT_SCHEMA_V2.into(),
            store,
        }
    }
    pub fn into_store(self) -> Result<StoreExport> {
        if self.schema != STORE_EXPORT_SCHEMA_V2 {
            return Err(Error::InvalidInput(
                "unsupported store-export schema".into(),
            ));
        }
        validate_pre_context_export(&self.store)?;
        Ok(self.store)
    }
}

/// Named predecessor DTO. Only this explicit import path accepts pre-codec
/// canonical nodes; it retains every old field before normalizing membership.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyStoreExportV1 {
    pub db_id: Ulid,
    pub dim: usize,
    #[serde(default)]
    pub embedding_fingerprint: Option<EmbeddingFingerprint>,
    #[serde(serialize_with = "serialize_pre_context_nodes")]
    pub nodes: Vec<serde_json::Value>,
    pub edges: Vec<Edge>,
    pub vectors: Vec<(NodeId, Vec<f32>)>,
    pub contradictions: Vec<Contradiction>,
    #[serde(default)]
    pub merges: Vec<MergeCandidate>,
    #[serde(default)]
    pub full_merge_commits: Vec<FullMergeRecord>,
    #[serde(default)]
    pub supersede_commits: Vec<SupersedeRecord>,
    #[serde(default)]
    pub remote_edges: Vec<RemoteEdge>,
    #[serde(default)]
    pub feedback_retries: Vec<FeedbackRetryRecord>,
}

fn serialize_pre_context_nodes<S: serde::Serializer>(
    nodes: &[serde_json::Value],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    for node in nodes {
        validate_pre_context_node_value(node).map_err(serde::ser::Error::custom)?;
    }
    nodes.serialize(serializer)
}

/// Raw predecessor fence, shared by named JSON and SQLite migrations. Presence
/// is checked before typed decoding can turn explicit null into unknown.
pub(crate) fn validate_pre_context_node_value(value: &serde_json::Value) -> Result<()> {
    if value
        .get("memory_kind")
        .and_then(|kind| kind.get("episode"))
        .and_then(serde_json::Value::as_object)
        .is_some_and(|facet| facet.contains_key("occurrence_contexts"))
    {
        return Err(Error::InvalidInput(
            "predecessor episode cannot contain occurrence_contexts field".into(),
        ));
    }
    if value
        .get("provenance")
        .and_then(|provenance| provenance.get("External"))
        .and_then(|external| external.get("source"))
        .and_then(|source| source.get("request_codec"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|codec| matches!(codec, "episode_v2" | "touchstone_v1"))
    {
        return Err(Error::InvalidInput(
            "predecessor node cannot carry episode_v2 request codec".into(),
        ));
    }
    Ok(())
}

fn validate_pre_context_store_value(value: &serde_json::Value) -> Result<()> {
    if value.get("touchstones").is_some() {
        return Err(Error::InvalidInput(
            "predecessor cannot contain touchstones field".into(),
        ));
    }
    if let Some(nodes) = value.get("nodes").and_then(serde_json::Value::as_array) {
        for node in nodes {
            validate_pre_context_node_value(node)?;
        }
    }
    Ok(())
}

fn validate_pre_context_export(export: &StoreExport) -> Result<()> {
    mem_touchstones::validate_pre_touchstone_export(export)?;
    for node in &export.nodes {
        if node
            .episode()
            .is_some_and(|facet| facet.occurrence_contexts().is_some())
            || matches!(node.provenance(), Provenance::External { source }
                if source.request_codec() == mneme_core::CaptureRequestCodec::EpisodeV2)
        {
            return Err(Error::InvalidInput(
                "predecessor export cannot contain occurrence contexts or episode_v2 codec".into(),
            ));
        }
    }
    Ok(())
}

/// Decode one predecessor canonical row at the named migration boundary.
/// The status result is its validated *original* physical-key status, not the
/// normalized destination membership. Source commitments use raw predecessor
/// bytes; destination commitments must be computed independently.
pub(crate) fn decode_legacy_node_v1(data: &str) -> Result<(Node, &'static str)> {
    let value: serde_json::Value = serde_json::from_str(data)
        .map_err(|e| Error::InvalidInput(format!("invalid legacy node JSON: {e}")))?;
    decode_legacy_node_v1_value(value)
}

fn decode_legacy_node_v1_value(mut value: serde_json::Value) -> Result<(Node, &'static str)> {
    validate_pre_context_node_value(&value)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| Error::InvalidInput("legacy node is not an object".into()))?;
    let status = object
        .get_mut("status")
        .ok_or_else(|| Error::InvalidInput("legacy node lacks status".into()))?;
    let original_status = match status {
        serde_json::Value::String(s) if s == "Active" => "active",
        serde_json::Value::String(s) if s == "Archived" => "archived",
        serde_json::Value::Object(map) if map.len() == 1 && map.contains_key("Candidate") => {
            let candidate = map.get("Candidate").unwrap();
            let count = candidate
                .get("use_count")
                .and_then(serde_json::Value::as_u64);
            if count.is_none_or(|count| count > u32::MAX as u64)
                || candidate.as_object().is_none_or(|fields| fields.len() != 1)
            {
                return Err(Error::InvalidInput(
                    "invalid legacy Candidate status".into(),
                ));
            }
            *status = serde_json::Value::String("Active".into());
            "candidate"
        }
        _ => return Err(Error::InvalidInput("unknown legacy node status".into())),
    };
    let episode = match object.get("memory_kind") {
        None => false,
        Some(kind) => matches!(
            serde_json::from_value::<MemoryKind>(kind.clone())
                .map_err(|e| Error::InvalidInput(format!("invalid legacy memory kind: {e}")))?,
            MemoryKind::Episode(_)
        ),
    };
    if let Some(provenance) = object.get_mut("provenance") {
        if let Some(source) = provenance
            .get_mut("External")
            .and_then(|v| v.get_mut("source"))
        {
            let source = source.as_object_mut().ok_or_else(|| {
                Error::InvalidInput("legacy capture source is not an object".into())
            })?;
            if source.contains_key("request_codec") {
                return Err(Error::InvalidInput(
                    "legacy capture source carries successor codec".into(),
                ));
            }
            source.insert(
                "request_codec".into(),
                serde_json::Value::String(if episode { "episode_v1" } else { "capture_v1" }.into()),
            );
        }
    }
    let node: Node = serde_json::from_value(value)
        .map_err(|e| Error::InvalidInput(format!("invalid legacy node: {e}")))?;
    node.validate()
        .map_err(|e| Error::InvalidInput(e.to_string()))?;
    Ok((node, original_status))
}

impl LegacyStoreExportV1 {
    pub fn into_current(self) -> Result<StoreExport> {
        if self.nodes.len() > MAX_LEGACY_JSON_UPGRADE_NODES {
            return Err(Error::InvalidInput(
                "legacy snapshot exceeds node limit".into(),
            ));
        }
        let mut nodes = Vec::with_capacity(self.nodes.len());
        for value in self.nodes {
            nodes.push(decode_legacy_node_v1_value(value)?.0);
        }
        Ok(StoreExport {
            db_id: self.db_id,
            dim: self.dim,
            embedding_fingerprint: self.embedding_fingerprint,
            nodes,
            edges: self.edges,
            vectors: self.vectors,
            contradictions: self.contradictions,
            merges: self.merges,
            touchstones: Vec::new(),
            full_merge_commits: self.full_merge_commits,
            supersede_commits: self.supersede_commits,
            remote_edges: self.remote_edges,
            feedback_retries: self.feedback_retries,
            concerns: Vec::new(),
        })
    }
}

#[async_trait]
impl ConcernStore for MemStore {
    async fn get_concern(&self, key: &ConcernKey) -> Result<Option<ConcernRow>> {
        let [lo, hi] = key.endpoints();
        Ok(self.lock().concerns.get(&(lo, hi, key.kind())).cloned())
    }

    async fn update_concern(&self, update: &ConcernUpdate) -> Result<ConcernCommitOutcome> {
        let mut g = self.lock();
        let key = update.key();
        let [lo, hi] = key.endpoints();
        let physical = (lo, hi, key.kind());
        let row = g.concerns.get(&physical).cloned();
        let Some(a) = g.nodes.get(&lo) else {
            return Ok(ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::MissingEndpoint,
                row,
            });
        };
        let Some(b) = g.nodes.get(&hi) else {
            return Ok(ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::MissingEndpoint,
                row,
            });
        };
        if a.status() != NodeStatus::Active || b.status() != NodeStatus::Active {
            return Ok(ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::InactiveEndpoint,
                row,
            });
        }
        let current = ConcernBinding::new(
            key.kind(),
            ConcernEndpoint::from_node(a),
            ConcernEndpoint::from_node(b),
        )
        .map_err(|e| Error::InvalidInput(e.to_string()))?;
        match transition_concern(&current, row.as_ref(), update) {
            ConcernTransition::Unchanged => Ok(ConcernCommitOutcome::Unchanged {
                row: row.expect("unchanged existing concern"),
            }),
            ConcernTransition::Refused(reason) => Ok(ConcernCommitOutcome::Refused { reason, row }),
            ConcernTransition::Replace(row) => {
                encode_concern_row(&row)?;
                let next = g.preflight_epoch_advance(true)?;
                g.concern_incoming.insert((hi, lo, key.kind()));
                g.concerns.insert(physical, row.clone());
                g.finish_epoch_advance(next);
                Ok(ConcernCommitOutcome::Applied { row })
            }
        }
    }

    async fn concerns_for_endpoint(&self, request: &ConcernPageRequest) -> Result<ConcernPage> {
        use std::ops::Bound::{Excluded, Included, Unbounded};
        let g = self.lock();
        let endpoint = request.endpoint();
        let start = request.after().map_or_else(
            || {
                Included((
                    endpoint,
                    NodeId(Ulid::from(0u128)),
                    ConcernKind::Disagreement,
                ))
            },
            |after| Excluded((endpoint, after.other(), after.kind())),
        );
        let mut keys: Vec<_> = g
            .concerns
            .range((start.clone(), Unbounded))
            .take_while(|((lo, _, _), _)| *lo == endpoint)
            .take(request.limit() + 1)
            .map(|((lo, hi, kind), _)| (*hi, *kind, (*lo, *hi, *kind)))
            .collect();
        keys.extend(
            g.concern_incoming
                .range((start, Unbounded))
                .take_while(|(hi, _, _)| *hi == endpoint)
                .take(request.limit() + 1)
                .map(|(hi, lo, kind)| (*lo, *kind, (*lo, *hi, *kind))),
        );
        keys.sort_by_key(|(other, kind, _)| (*other, *kind));
        let more = keys.len() > request.limit();
        keys.truncate(request.limit());
        let next = if more {
            keys.last().map(|(other, kind, _)| {
                ConcernPageCursor::new(endpoint, *other, *kind).expect("distinct endpoint")
            })
        } else {
            None
        };
        let items = keys
            .iter()
            .map(|(_, _, key)| g.concerns.get(key).expect("indexed concern exists").clone())
            .collect();
        Ok(ConcernPage { items, next })
    }
}

/// Volatile adapter record proving an exact receipt-feedback payload has already
/// committed in the current host authority generation. Public only because
/// [`StoreExport`] exposes operational diagnostics; load/import discards it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackRetryRecord {
    pub key: String,
    pub fingerprint: String,
    #[serde(default = "legacy_feedback_epoch")]
    pub epoch: String,
    #[serde(default)]
    pub sequence: u64,
}

impl EmbeddingMetadataStore for MemStore {
    fn embedding_fingerprint(&self) -> Result<Option<EmbeddingFingerprint>> {
        Ok(self.lock().embedding_fingerprint.clone())
    }

    fn set_embedding_fingerprint(&self, fingerprint: &EmbeddingFingerprint) -> Result<()> {
        fingerprint
            .validate()
            .map_err(Error::InvalidEmbeddingFingerprint)?;
        if fingerprint.dimension != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: fingerprint.dimension,
            });
        }
        let replacement = fingerprint.clone();
        let mut g = self.lock();
        let changed = g.embedding_fingerprint.as_ref() != Some(&replacement);
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            g.embedding_fingerprint = Some(replacement);
            g.finish_epoch_advance(next_epoch);
        }
        Ok(())
    }

    fn has_embedding_data(&self) -> Result<bool> {
        let g = self.lock();
        Ok(!g.nodes.is_empty() || !g.vectors.is_empty())
    }

    fn ensure_embedding_fingerprint(
        &self,
        runtime: &EmbeddingFingerprint,
    ) -> Result<EmbeddingFingerprintInit> {
        runtime
            .validate()
            .map_err(Error::InvalidEmbeddingFingerprint)?;
        let replacement = runtime.clone();
        let mut g = self.lock();
        match &g.embedding_fingerprint {
            Some(stored) => {
                stored
                    .validate()
                    .map_err(Error::InvalidEmbeddingFingerprint)?;
                if stored == runtime {
                    Ok(EmbeddingFingerprintInit::Existing)
                } else {
                    Err(Error::EmbeddingFingerprintMismatch {
                        stored: Box::new(stored.clone()),
                        runtime: Box::new(replacement),
                    })
                }
            }
            None if !g.nodes.is_empty() || !g.vectors.is_empty() => {
                Err(Error::LegacyEmbeddingFingerprint)
            }
            None => {
                if runtime.dimension != self.dim {
                    return Err(Error::DimMismatch {
                        index: self.dim,
                        provider: runtime.dimension,
                    });
                }
                let next_epoch = g.preflight_epoch_advance(true)?;
                g.embedding_fingerprint = Some(replacement);
                g.finish_epoch_advance(next_epoch);
                Ok(EmbeddingFingerprintInit::InitializedEmptyStore)
            }
        }
    }
}

#[async_trait]
impl GraphStore for MemStore {
    fn touchstones(&self) -> Option<&dyn TouchstoneStore> {
        Some(self)
    }
    fn concerns(&self) -> Option<&dyn ConcernStore> {
        Some(self)
    }
    fn episodes(&self) -> Option<&dyn mneme_core::ports::EpisodeStore> {
        Some(self)
    }

    async fn compare_replace_node_summary(
        &self,
        id: NodeId,
        expected: &mneme_core::SummarySnapshotDigest,
        summary: &mneme_core::NodeSummary,
        embedding: &[f32],
    ) -> Result<Node> {
        if embedding.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: embedding.len(),
            });
        }
        validate_cosine_vector(embedding, "summary edit embedding")?;
        let mut g = self.lock();
        let mut node = g.nodes.get(&id).cloned().ok_or(Error::NotFound)?;
        mem_episodes::require_semantic_node(&node)?;
        if mneme_core::SummarySnapshot::from_node(self.db_id, &node)?.digest() != *expected {
            return Err(Error::Conflict(
                "summary snapshot changed; GET the node again".into(),
            ));
        }
        if g.touchstones.contains_key(&id) {
            return Err(Error::Conflict(
                "summary editing cannot replace a touchstone owner".into(),
            ));
        }
        if node.summary() == summary.as_str() {
            return Err(Error::InvalidInput("summary is unchanged".into()));
        }
        node.set_summary(summary.clone());
        node.validate()
            .map_err(|e| Error::InvalidInput(e.to_string()))?;
        let next = g.preflight_epoch_advance(true)?;
        // Lexical search reads canonical summaries directly. Tags/status and all
        // indexes other than the vector are unchanged, so do not rebuild them.
        g.nodes.insert(id, node.clone());
        g.vectors.insert(id, embedding.to_vec());
        g.finish_epoch_advance(next);
        Ok(node)
    }

    async fn compare_replace_node_body(
        &self,
        id: NodeId,
        expected: &mneme_core::BodyRevision,
        replacement: &mneme_core::BodyRef,
    ) -> Result<Node> {
        let mut g = self.lock();
        let mut node = g.nodes.get(&id).cloned().ok_or(Error::NotFound)?;
        mem_episodes::require_semantic_node(&node)?;
        if node.body_revision() != *expected {
            return Err(Error::Conflict("body revision changed".into()));
        }
        if g.touchstones.contains_key(&id) {
            return Err(Error::Conflict(
                "body editing cannot replace a touchstone owner".into(),
            ));
        }
        if g.edges
            .range((id, NodeId(Ulid::from(0)))..=(id, NodeId(Ulid::from(u128::MAX))))
            .any(|(_, edge)| edge.anchor.is_some())
        {
            return Err(Error::Conflict(
                "body editing cannot invalidate outgoing body anchors".into(),
            ));
        }
        node.set_body_reference(replacement.clone(), mneme_core::BodyOwnership::Managed);
        let changed = g.nodes.get(&id).is_none_or(|before| {
            before.body() != node.body() || before.body_ownership() != node.body_ownership()
        });
        let next = g.preflight_epoch_advance(changed)?;
        g.nodes.insert(id, node.clone());
        g.finish_epoch_advance(next);
        Ok(node)
    }

    async fn lookup_capture(&self, proof: &CaptureReplayProof) -> Result<Option<NodeId>> {
        let source = proof.source();
        source
            .validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let id = source.node_id();
        let g = self.lock();
        match g.nodes.get(&id) {
            Some(node) => match node.provenance() {
                Provenance::External { source: stored }
                    if proof.matches_source(stored) && node.is_semantic() =>
                {
                    if !g.vectors.contains_key(&id) {
                        return Err(Error::Conflict(format!(
                            "capture node {} has no vector",
                            id.0
                        )));
                    }
                    g.verify_touchstone_replay(self.db_id, self.dim, node, None)?;
                    Ok(Some(id))
                }
                _ => Err(Error::Conflict(format!(
                    "capture identity {} is already occupied by different content",
                    id.0
                ))),
            },
            None if g.vectors.contains_key(&id) => Err(Error::Conflict(format!(
                "capture identity {} has an orphan vector",
                id.0
            ))),
            None => Ok(None),
        }
    }

    async fn commit_capture(
        &self,
        node: &Node,
        embedding: &[f32],
        proof: &CaptureReplayProof,
    ) -> Result<CaptureCommitOutcome> {
        self.commit_capture_with_edges(node, embedding, &[], proof)
            .await
    }

    async fn commit_capture_with_edges(
        &self,
        node: &Node,
        embedding: &[f32],
        edges: &[Edge],
        proof: &CaptureReplayProof,
    ) -> Result<CaptureCommitOutcome> {
        self.commit_capture_with_priors(
            node,
            embedding,
            edges,
            &[],
            mneme_core::ports::CapturePriorBudget::new(0),
            proof,
        )
        .await
    }

    async fn commit_capture_with_priors(
        &self,
        node: &Node,
        embedding: &[f32],
        edges: &[Edge],
        priors: &[mneme_core::ports::CaptureSimilarityPrior],
        generated_link_budget: mneme_core::ports::CapturePriorBudget,
        proof: &CaptureReplayProof,
    ) -> Result<CaptureCommitOutcome> {
        self.commit_capture_with_touchstone(
            node,
            embedding,
            edges,
            priors,
            generated_link_budget,
            proof,
            None,
        )
        .await
    }

    async fn commit_capture_with_touchstone(
        &self,
        node: &Node,
        embedding: &[f32],
        edges: &[Edge],
        priors: &[mneme_core::ports::CaptureSimilarityPrior],
        generated_link_budget: mneme_core::ports::CapturePriorBudget,
        proof: &CaptureReplayProof,
        touchstone: Option<&TouchstoneInput>,
    ) -> Result<CaptureCommitOutcome> {
        mem_episodes::require_semantic_node(node)?;
        mneme_core::ports::validate_capture_edges(node.id(), edges)?;
        node.validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let Provenance::External { source } = node.provenance() else {
            return Err(Error::InvalidInput(
                "capture node requires external source provenance".into(),
            ));
        };
        if source.node_id() != node.id() || source != proof.source() {
            return Err(Error::InvalidInput(
                "capture node identity or source does not match incoming proof".into(),
            ));
        }
        if embedding.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: embedding.len(),
            });
        }
        validate_cosine_vector(embedding, "capture embedding")?;
        let id = node.id();
        let mut g = self.lock();
        if let Some(existing) = g.nodes.get(&id) {
            return match existing.provenance() {
                Provenance::External { source: stored }
                    if proof.matches_source(stored)
                        && existing.is_semantic()
                        && g.vectors.contains_key(&id) =>
                {
                    g.verify_touchstone_replay(self.db_id, self.dim, existing, touchstone)?;
                    Ok(CaptureCommitOutcome::AlreadyApplied)
                }
                _ => Err(Error::Conflict(format!(
                    "capture identity {} is already occupied by different or incomplete content",
                    id.0
                ))),
            };
        }
        let sample = (stable_tag_sample_hash(id), id);
        if g.vectors.contains_key(&id)
            || g.touchstones.contains_key(&id)
            || g.tag_projection.contains_sample(sample)
        {
            return Err(Error::Conflict(format!(
                "capture identity {} has an orphan projection",
                id.0
            )));
        }
        // Preflight the entire batch under the same lock. Never let a late
        // target/capacity failure leave a partial capture behind.
        if g.edge_keys_by_endpoint
            .get(&id)
            .is_some_and(|keys| keys.len().saturating_add(edges.len()) > MAX_INCIDENT_EDGES)
            || edges.len() > MAX_INCIDENT_EDGES
        {
            return Err(incident_edge_capacity_error());
        }
        for edge in edges {
            if !g.nodes.contains_key(&edge.to) {
                return Err(Error::InvalidInput(format!(
                    "capture edge target {} does not exist",
                    edge.to.0
                )));
            }
            if g.edges.contains_key(&(edge.from, edge.to)) {
                return Err(Error::Conflict(format!(
                    "capture edge {} -> {} already exists",
                    edge.from.0, edge.to.0
                )));
            }
            if g.edge_keys_by_endpoint
                .get(&edge.to)
                .is_some_and(|keys| keys.len() >= MAX_INCIDENT_EDGES)
            {
                return Err(incident_edge_capacity_error());
            }
        }
        mneme_core::ports::validate_capture_prior_candidates(priors)?;
        let mut admitted = edges.to_vec();
        let limit = generated_link_budget
            .limit()
            .min(mneme_core::ports::MAX_CAPTURE_EDGES.saturating_sub(edges.len()));
        for prior in priors {
            if admitted.len() - edges.len() >= limit {
                break;
            }
            let edge = prior.edge();
            if edge.from != id
                || admitted.iter().any(|e| e.to == edge.to)
                || g.edges.contains_key(&(id, edge.to))
                || !g
                    .nodes
                    .get(&edge.to)
                    .is_some_and(|target| prior.matches_target(target))
                || g.edge_keys_by_endpoint
                    .get(&edge.to)
                    .is_some_and(|keys| keys.len() >= MAX_INCIDENT_EDGES)
                || g.edge_keys_by_endpoint
                    .get(&id)
                    .map_or(0, |keys| keys.len())
                    .saturating_add(admitted.len())
                    >= MAX_INCIDENT_EDGES
            {
                continue;
            }
            admitted.push(edge.clone());
        }
        let record = mem_touchstones::prepare_touchstone(self.db_id, node, touchstone, |id| {
            Ok(g.nodes.get(&id).cloned())
        })?;
        let next_epoch = g.preflight_epoch_advance(true)?;
        g.replace_node(node.clone());
        g.vectors.insert(id, embedding.to_vec());
        if let Some(record) = record {
            g.insert_touchstone(record);
        }
        for edge in &admitted {
            g.put_edge_prevalidated(edge.clone());
        }
        g.finish_epoch_advance(next_epoch);
        Ok(CaptureCommitOutcome::Applied)
    }

    async fn put_node(&self, node: &Node) -> Result<()> {
        mem_episodes::require_semantic_node(node)?;
        node.validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let replacement = node.clone();
        let mut g = self.lock();
        g.require_semantic_ids(&[replacement.id()])?;
        if g.touchstones.contains_key(&replacement.id()) {
            validate_touchstone_owner_replacement(
                g.nodes.get(&replacement.id()).ok_or(Error::NotFound)?,
                &replacement,
            )?;
        }
        if matches!(replacement.provenance(), Provenance::External {source}
            if source.request_codec() == mneme_core::CaptureRequestCodec::TouchstoneV1)
            && !g.touchstones.contains_key(&replacement.id())
        {
            return Err(Error::InvalidInput(
                "touchstone owner must be written through atomic capture".into(),
            ));
        }
        let sample = (stable_tag_sample_hash(replacement.id()), replacement.id());
        let (canonical_changed, projection_needs_repair) = match g.nodes.get(&replacement.id()) {
            Some(current) => (
                !same_serialized(current, &replacement)?,
                !g.tag_projection.indexes_node_exactly(current, sample),
            ),
            None => (true, g.tag_projection.contains_sample(sample)),
        };
        let changed = canonical_changed || projection_needs_repair;
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            if projection_needs_repair {
                g.replace_node_repairing_projection(replacement);
            } else {
                g.replace_node(replacement);
            }
            g.finish_epoch_advance(next_epoch);
        }
        Ok(())
    }

    async fn get_node(&self, id: NodeId) -> Result<Option<Node>> {
        Ok(self.lock().nodes.get(&id).cloned())
    }

    async fn get_nodes(&self, ids: &[NodeId]) -> Result<Vec<Option<Node>>> {
        if ids.len() > MAX_NODE_HYDRATION_BATCH {
            return Err(Error::CapacityExceeded {
                resource: "node hydration batch",
                limit: MAX_NODE_HYDRATION_BATCH,
            });
        }
        let g = self.lock();
        Ok(ids.iter().map(|id| g.nodes.get(id).cloned()).collect())
    }

    async fn get_node_statuses(&self, ids: &[NodeId]) -> Result<Vec<Option<TaggedPhysicalStatus>>> {
        if ids.len() > MAX_NODE_STATUS_BATCH {
            return Err(Error::CapacityExceeded {
                resource: "node status batch",
                limit: MAX_NODE_STATUS_BATCH,
            });
        }
        let g = self.lock();
        Ok(ids
            .iter()
            .map(|id| {
                g.nodes
                    .get(id)
                    .filter(|node| node.is_semantic())
                    .map(|node| TaggedPhysicalStatus::from_node_status(node.status()))
            })
            .collect())
    }

    async fn delete_node(&self, id: NodeId) -> Result<()> {
        // Edges are a separate relation: the engine's forget path deletes them
        // first, matching the persistent adapter. Keep the derived projection
        // aligned with the still-canonical edge rows if this low-level method is
        // called on its own. Open reconciliation work is no longer actionable
        // after an endpoint disappears, but terminal decisions and full-merge
        // retry proofs remain durable history.
        let mut g = self.lock();
        g.require_semantic_ids(&[id])?;
        g.require_no_episode_evidence(id)?;
        let changed = g.nodes.contains_key(&id)
            || g.vectors.contains_key(&id)
            || g.contradictions
                .iter()
                .any(|(pair, contradiction)| pair_contains(*pair, id) && contradiction.is_open())
            || g.merges
                .iter()
                .any(|(pair, candidate)| pair_contains(*pair, id) && candidate.is_open());
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if !changed {
            return Ok(());
        }
        let first = (id, NodeId(Ulid::from(0u128)), ConcernKind::Disagreement);
        let mut incident: Vec<_> = g
            .concerns
            .range(first..)
            .take_while(|((lo, _, _), _)| *lo == id)
            .map(|(key, _)| *key)
            .collect();
        incident.extend(
            g.concern_incoming
                .range(first..)
                .take_while(|(hi, _, _)| *hi == id)
                .map(|(hi, lo, kind)| (*lo, *hi, *kind)),
        );
        for (lo, hi, kind) in incident {
            g.concerns.remove(&(lo, hi, kind));
            g.concern_incoming.remove(&(hi, lo, kind));
        }
        g.remove_touchstone(id);
        g.remove_node(id);
        g.vectors.remove(&id);
        g.contradictions
            .retain(|pair, contradiction| !pair_contains(*pair, id) || !contradiction.is_open());
        g.merges
            .retain(|pair, candidate| !pair_contains(*pair, id) || !candidate.is_open());
        g.finish_epoch_advance(next_epoch);
        Ok(())
    }

    async fn all_nodes(&self, _: ColdPath) -> Result<Vec<Node>> {
        Ok(self.lock().nodes.values().cloned().collect())
    }

    async fn compare_replace_node_tags(
        &self,
        id: NodeId,
        expected: &mneme_core::BoundedTagSet,
        tags: &mneme_core::BoundedTagSet,
    ) -> Result<Node> {
        let mut g = self.lock();
        g.require_semantic_ids(&[id])?;
        let current = g.nodes.get(&id).ok_or(Error::NotFound)?;
        current
            .validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        if current.tag_set() != expected {
            return Err(Error::Conflict(
                "node tags changed; inspect current tags before retrying".into(),
            ));
        }
        let mut replacement = current.clone();
        replacement.replace_tags(tags.clone());
        if g.touchstones.contains_key(&id) {
            validate_touchstone_owner_replacement(current, &replacement)?;
        }
        let sample = (stable_tag_sample_hash(id), id);
        let projection_needs_repair = !g.tag_projection.indexes_node_exactly(current, sample);
        let changed = current.tag_set() != tags || projection_needs_repair;
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            if projection_needs_repair {
                g.replace_node_repairing_projection(replacement.clone());
            } else {
                g.replace_node(replacement.clone());
            }
            g.finish_epoch_advance(next_epoch);
        }
        Ok(replacement)
    }

    async fn set_status(&self, id: NodeId, status: NodeStatus) -> Result<()> {
        let mut g = self.lock();
        g.require_semantic_ids(&[id])?;
        let current = g.nodes.get(&id).ok_or(Error::NotFound)?;
        current
            .validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let mut replacement = current.clone();
        replacement.set_status(status);
        let sample = (stable_tag_sample_hash(id), id);
        let canonical_changed = !same_serialized(current, &replacement)?;
        let projection_needs_repair = !g.tag_projection.indexes_node_exactly(current, sample);
        let changed = canonical_changed || projection_needs_repair;
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            if projection_needs_repair {
                g.replace_node_repairing_projection(replacement);
            } else {
                g.replace_node(replacement);
            }
            g.finish_epoch_advance(next_epoch);
        }
        Ok(())
    }

    async fn maintenance_node_upper_bound(&self, _: ColdPath) -> Result<Option<NodeId>> {
        Ok(self.lock().nodes.keys().next_back().copied())
    }

    async fn maintenance_nodes_page(
        &self,
        _: ColdPath,
        after: Option<NodeId>,
        through: NodeId,
        limit: usize,
    ) -> Result<MaintenanceNodePage> {
        validate_maintenance_page_limit(limit)?;
        if after.is_some_and(|after| after >= through) {
            return Ok(MaintenanceNodePage {
                items: Vec::new(),
                next: None,
            });
        }
        use std::ops::Bound::{Excluded, Included, Unbounded};
        let g = self.lock();
        let lower = after.map_or(Unbounded, Excluded);
        let items = g
            .nodes
            .range((lower, Included(through)))
            .take(limit + 1)
            .map(|(_, node)| node.clone())
            .collect();
        Ok(finish_maintenance_node_page(items, limit))
    }

    async fn put_edge(&self, edge: &Edge) -> Result<()> {
        edge.validate().map_err(Error::InvalidInput)?;
        let replacement = edge.clone();
        let key = (replacement.from, replacement.to);
        let mut g = self.lock();
        let canonical_changed = match g.edges.get(&key) {
            Some(current) => !same_serialized(current, &replacement)?,
            None => true,
        };
        let indexed = g
            .edge_keys_by_endpoint
            .get(&key.0)
            .is_some_and(|keys| keys.contains(&key))
            && (key.0 == key.1
                || g.edge_keys_by_endpoint
                    .get(&key.1)
                    .is_some_and(|keys| keys.contains(&key)));
        let changed = canonical_changed || !indexed;
        if !changed {
            return Ok(());
        }
        g.validate_edge_upsert_capacity(key)?;
        let next_epoch = g.preflight_epoch_advance(true)?;
        g.put_edge_prevalidated(replacement);
        g.finish_epoch_advance(next_epoch);
        Ok(())
    }

    async fn delete_edge(&self, from: NodeId, to: NodeId) -> Result<()> {
        let key = (from, to);
        let mut g = self.lock();
        let changed = g.edges.contains_key(&key)
            || g.edge_keys_by_endpoint
                .get(&from)
                .is_some_and(|keys| keys.contains(&key))
            || (to != from
                && g.edge_keys_by_endpoint
                    .get(&to)
                    .is_some_and(|keys| keys.contains(&key)));
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            g.delete_edge(key);
            g.finish_epoch_advance(next_epoch);
        }
        Ok(())
    }

    async fn get_edge(&self, from: NodeId, to: NodeId) -> Result<Option<Edge>> {
        Ok(self.lock().edges.get(&(from, to)).cloned())
    }

    async fn all_edges(&self, _: ColdPath) -> Result<Vec<Edge>> {
        Ok(self.lock().edges.values().cloned().collect())
    }

    async fn maintenance_edge_upper_bound(
        &self,
        _: ColdPath,
    ) -> Result<Option<MaintenanceEdgeKey>> {
        Ok(self
            .lock()
            .edges
            .keys()
            .next_back()
            .map(|(from, to)| MaintenanceEdgeKey::new(*from, *to)))
    }

    async fn maintenance_edges_page(
        &self,
        _: ColdPath,
        after: Option<MaintenanceEdgeKey>,
        through: MaintenanceEdgeKey,
        limit: usize,
    ) -> Result<MaintenanceEdgePage> {
        validate_maintenance_page_limit(limit)?;
        if after.is_some_and(|after| after >= through) {
            return Ok(MaintenanceEdgePage {
                items: Vec::new(),
                next: None,
            });
        }
        use std::ops::Bound::{Excluded, Included, Unbounded};
        let g = self.lock();
        let lower = after.map_or(Unbounded, |key| Excluded((key.from, key.to)));
        let items = g
            .edges
            .range((lower, Included((through.from, through.to))))
            .take(limit + 1)
            .map(|(_, edge)| edge.clone())
            .collect();
        Ok(finish_maintenance_edge_page(items, limit))
    }

    async fn incident_edges_page(
        &self,
        request: &IncidentEdgesRequest,
    ) -> Result<Option<IncidentEdgesPage>> {
        request.validate()?;
        let started = std::time::Instant::now();
        let inner = self.linked_read_lock(started, request.remaining())?;
        let page = collect_incident_edges_page(request, |leg, after, quota, work| {
            linked_read_remaining(started, request.remaining())?;
            work.indexed_seeks += 1;
            let anchor = request.anchor();
            let min = NodeId(Ulid::from(0u128));
            let max = NodeId(Ulid::from(u128::MAX));
            let mut items = Vec::with_capacity(quota);
            match leg {
                IncidentEdgeLeg::Outgoing => {
                    let lower = after.map_or(std::ops::Bound::Included((anchor, min)), |key| {
                        std::ops::Bound::Excluded((anchor, key.to))
                    });
                    for (_, edge) in inner
                        .edges
                        .range((lower, std::ops::Bound::Included((anchor, max))))
                        .take(quota)
                    {
                        linked_read_remaining(started, request.remaining())?;
                        items.push(edge.clone());
                    }
                }
                IncidentEdgeLeg::Incoming => {
                    let lower = after.map_or(std::ops::Bound::Included((anchor, min)), |key| {
                        std::ops::Bound::Excluded((anchor, key.from))
                    });
                    for &(to, from) in inner
                        .edge_keys_by_to
                        .range((lower, std::ops::Bound::Included((anchor, max))))
                        .take(quota)
                    {
                        linked_read_remaining(started, request.remaining())?;
                        work.edge_point_reads += 1;
                        let edge = inner.edges.get(&(from, to)).ok_or_else(|| {
                            Error::Backend("incoming edge projection has no canonical edge".into())
                        })?;
                        items.push(edge.clone());
                    }
                }
            }
            // Mem canonical edges already contain body-span anchors: no separate
            // anchor point reads, and no endpoint/node hydration in either leg.
            Ok(items)
        })?;
        linked_read_remaining(started, request.remaining())?;
        Ok(Some(page))
    }

    async fn neighbors(&self, id: NodeId, top_k: usize) -> Result<Vec<Neighbor>> {
        let g = self.lock();
        let incident = g.incident_edges(id);
        let mut out = oriented_neighbors(id, &incident);
        out.sort_by(neighbor_order);
        out.truncate(top_k);
        Ok(out)
    }

    async fn commit_feedback(&self, commit: &FeedbackCommit) -> Result<FeedbackCommitOutcome> {
        commit.validate()?;
        let mut g = self.lock();
        for update in &commit.nodes {
            mem_episodes::require_semantic_node(&update.expected)?;
            mem_episodes::require_semantic_node(&update.replacement)?;
            g.require_semantic_ids(&[update.expected.id()])?;
        }
        for update in &commit.edges {
            g.require_semantic_ids(&[update.replacement.from, update.replacement.to])?;
        }
        for observation in &commit.merge_observations {
            g.require_semantic_ids(&[observation.between.0, observation.between.1])?;
        }
        // Stage retry-ledger GC alongside the graph delta. Even reachability cleanup
        // must not leak out of a later CAS/capacity failure.
        let mut unreachable_retry_keys = Vec::new();
        let mut changed = commit.idempotency.is_some();

        if let Some(idempotency) = &commit.idempotency {
            // An exact proof in this authority generation always wins before
            // watermark cleanup. A bare-key collision from another epoch is
            // unreachable old-generation state, not a replay or payload
            // conflict; it joins the atomically staged GC below.
            if let Some(applied) = g.feedback_retries.get(&idempotency.key)
                && applied.epoch == idempotency.retry.epoch
            {
                return if applied.fingerprint == idempotency.fingerprint {
                    Ok(FeedbackCommitOutcome::AlreadyApplied)
                } else {
                    Err(Error::InvalidInput(
                        "feedback idempotency key was reused with a different payload in the same epoch"
                            .into(),
                    ))
                };
            }
            unreachable_retry_keys.extend(
                g.feedback_retries
                    .iter()
                    .filter(|(_, retry)| {
                        retry.epoch != idempotency.retry.epoch
                            || retry.sequence < idempotency.retry.min_live_sequence
                    })
                    .map(|(key, _)| key.clone()),
            );
            let live = g
                .feedback_retries
                .len()
                .saturating_sub(unreachable_retry_keys.len());
            if live >= MAX_FEEDBACK_RETRY_RECORDS {
                return Err(feedback_retry_capacity_error());
            }
            g.feedback_retries.try_reserve(1).map_err(|error| {
                Error::Backend(format!("reserve feedback retry record: {error}"))
            })?;
        }

        // Validate every compare-and-swap precondition before touching a row.
        // Serialization is the canonical snapshot representation and includes
        // private counters that the adapter cannot otherwise compare directly.
        for update in &commit.nodes {
            update
                .expected
                .validate()
                .map_err(|error| Error::InvalidInput(error.to_string()))?;
            update
                .replacement
                .validate()
                .map_err(|error| Error::InvalidInput(error.to_string()))?;
            let current = g.nodes.get(&update.expected.id()).ok_or_else(|| {
                Error::Conflict(format!(
                    "feedback target {} disappeared before commit",
                    update.expected.id().0
                ))
            })?;
            if g.touchstones.contains_key(&current.id()) {
                validate_touchstone_owner_replacement(current, &update.replacement)?;
            }
            if !same_serialized(current, &update.expected)? {
                return Err(Error::Conflict(format!(
                    "feedback target {} changed before commit",
                    update.expected.id().0
                )));
            }
            changed |= !same_serialized(current, &update.replacement)?;
        }
        for update in &commit.edges {
            let key = (update.replacement.from, update.replacement.to);
            let current = g.edges.get(&key);
            let matches = match (&update.expected, current) {
                (None, None) => true,
                (Some(expected), Some(current)) => same_serialized(current, expected)?,
                _ => false,
            };
            if !matches {
                return Err(Error::Conflict(format!(
                    "feedback edge {} -> {} changed before commit",
                    key.0.0, key.1.0
                )));
            }
            if !g.nodes.contains_key(&key.0) || !g.nodes.contains_key(&key.1) {
                return Err(Error::Conflict(format!(
                    "feedback edge {} -> {} has a missing endpoint",
                    key.0.0, key.1.0
                )));
            }
            changed |= match current {
                Some(current) => !same_serialized(current, &update.replacement)?,
                None => true,
            };
        }

        let mut new_merge_candidates = 0usize;
        for observation in &commit.merge_observations {
            let pair = observation.between;
            if !g.nodes.contains_key(&pair.0) || !g.nodes.contains_key(&pair.1) {
                return Err(Error::Conflict(format!(
                    "feedback merge pair {} <-> {} has a missing endpoint",
                    pair.0.0, pair.1.0
                )));
            }
            new_merge_candidates += usize::from(!g.merges.contains_key(&pair));
            changed |= match g.merges.get(&pair) {
                Some(current) => {
                    let mut replacement = current.clone();
                    replacement.observe(commit.applied_at);
                    !same_serialized(current, &replacement)?
                }
                None => true,
            };
        }
        g.merges
            .try_reserve(new_merge_candidates)
            .map_err(|error| {
                Error::Backend(format!("reserve feedback merge candidates: {error}"))
            })?;

        // Check every new edge against the *final batch* degree before writes.
        // Sequential put_edge calls would otherwise make a late capacity error
        // leave an already-written prefix in this in-memory transaction.
        let mut additions: HashMap<NodeId, usize> = HashMap::new();
        for update in &commit.edges {
            if update.expected.is_some() {
                continue;
            }
            *additions.entry(update.replacement.from).or_default() += 1;
            if update.replacement.to != update.replacement.from {
                *additions.entry(update.replacement.to).or_default() += 1;
            }
        }
        for (endpoint, added) in additions {
            let current = g
                .edge_keys_by_endpoint
                .get(&endpoint)
                .map_or(0, HashSet::len);
            if current > MAX_INCIDENT_EDGES
                || current
                    .checked_add(added)
                    .is_none_or(|degree| degree > MAX_INCIDENT_EDGES)
            {
                return Err(incident_edge_capacity_error());
            }
        }

        let next_epoch = g.preflight_epoch_advance(changed)?;
        if !changed {
            return Ok(FeedbackCommitOutcome::Applied);
        }

        // Everything after this point is infallible under the validated bounds.
        for update in &commit.nodes {
            g.nodes
                .insert(update.replacement.id(), update.replacement.clone());
        }
        for update in &commit.edges {
            let key = (update.replacement.from, update.replacement.to);
            g.edges.insert(key, update.replacement.clone());
            g.index_edge_key(key);
        }
        for observation in &commit.merge_observations {
            g.merges
                .entry(observation.between)
                .and_modify(|candidate| candidate.observe(commit.applied_at))
                .or_insert_with(|| {
                    MergeCandidate::new(
                        observation.between.0,
                        observation.between.1,
                        commit.applied_at,
                    )
                });
        }
        if let Some(idempotency) = &commit.idempotency {
            for key in unreachable_retry_keys {
                g.feedback_retries.remove(&key);
            }
            g.feedback_retries.insert(
                idempotency.key.clone(),
                FeedbackRetryRecord {
                    key: idempotency.key.clone(),
                    fingerprint: idempotency.fingerprint.clone(),
                    epoch: idempotency.retry.epoch.clone(),
                    sequence: idempotency.retry.sequence,
                },
            );
        }
        g.finish_epoch_advance(next_epoch);
        Ok(FeedbackCommitOutcome::Applied)
    }

    async fn commit_full_merge(&self, commit: &FullMergeCommit) -> Result<FullMergeCommitOutcome> {
        commit.validate()?;
        let pair = commit.pair();
        let mut g = self.lock();
        g.require_semantic_ids(&[commit.winner, commit.loser])?;

        // Retry proof is checked before reading the post-merge graph: after a
        // successful collapse the loser is archived and has no adjacency left,
        // so replanning first could never recognize the exact lost response.
        if let Some(applied) = g.full_merge_commits.get(&pair) {
            return if applied.winner == commit.winner && applied.loser == commit.loser {
                Ok(FullMergeCommitOutcome::AlreadyApplied)
            } else {
                Err(Error::Conflict(
                    "full merge pair already committed in the opposite direction".into(),
                ))
            };
        }

        if g.touchstones.contains_key(&commit.winner) || g.touchstones.contains_key(&commit.loser) {
            return Err(Error::Conflict("full merge cannot consume a touchstone owner; author a new interpretation and explicit supersession".into()));
        }
        let winner = g.nodes.get(&commit.winner).ok_or(Error::NotFound)?;
        let loser = g.nodes.get(&commit.loser).ok_or(Error::NotFound)?;
        winner
            .validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        loser
            .validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        if winner.is_archived() || loser.is_archived() {
            return Err(Error::Conflict(
                "full merge requires live winner and loser nodes".into(),
            ));
        }
        let candidate = g.merges.get(&pair).ok_or(Error::NotFound)?;
        if !candidate.is_open() {
            return Err(Error::Conflict(
                "full merge candidate was already resolved".into(),
            ));
        }

        // History keeps its original lesson anchor even when that lesson is
        // collapsed into another semantic node. Never rewire episode evidence.
        let incident: Vec<_> = g
            .full_merge_incident_edges(commit.winner, commit.loser)?
            .into_iter()
            .filter(|edge| !g.is_episode_edge(edge))
            .collect();
        let final_edges = commit.normalize_local_edges(&incident)?;
        let retained_evidence = g
            .incident_edges(commit.winner)
            .iter()
            .filter(|edge| g.is_episode_edge(edge))
            .count();
        if final_edges.len().saturating_add(retained_evidence) > MAX_INCIDENT_EDGES {
            return Err(incident_edge_capacity_error());
        }
        let winner_remote = g.remote_edges_for_source(commit.winner)?;
        let loser_remote = g.remote_edges_for_source(commit.loser)?;
        let final_remote = commit.normalize_remote_edges(&winner_remote, &loser_remote)?;
        let record = commit.record();
        g.full_merge_commits
            .try_reserve(1)
            .map_err(|error| Error::Backend(format!("reserve full merge record: {error}")))?;

        let next_epoch = g.preflight_epoch_advance(true)?;

        // All semantic validation and capacity checks are complete. Replace the
        // two bounded incident/source sets under the one mutex; no unrelated row
        // or whole-edge scan participates.
        let incident_keys: Vec<_> = incident.iter().map(|edge| (edge.from, edge.to)).collect();
        for key in incident_keys {
            g.delete_edge(key);
        }
        for edge in final_edges {
            let key = (edge.from, edge.to);
            g.edges.insert(key, edge);
            g.index_edge_key(key);
        }

        for edge in winner_remote.iter().chain(&loser_remote) {
            g.delete_remote_edge((edge.from, edge.target_db, edge.target));
        }
        for edge in final_remote {
            let key = (edge.from, edge.target_db, edge.target);
            g.remote_edges.insert(key, edge.clone());
            g.remote_edge_keys_by_source
                .entry(commit.winner)
                .or_default()
                .insert(remote_edge_order_key(&edge));
        }

        g.set_existing_node_status(commit.loser, NodeStatus::Archived);
        g.merges
            .get_mut(&pair)
            .expect("full merge candidate was validated")
            .resolve(MergeResolution::Full);
        g.full_merge_commits.insert(pair, record);
        g.finish_epoch_advance(next_epoch);
        Ok(FullMergeCommitOutcome::Applied)
    }

    async fn commit_supersede(&self, commit: &SupersedeCommit) -> Result<SupersedeCommitOutcome> {
        commit.validate()?;
        let pair = commit.pair();
        let mut g = self.lock();
        g.require_semantic_ids(&[commit.winner, commit.loser])?;

        // Consult the durable direction proof before post-operation state. A
        // successful first call may have changed every row below already.
        if let Some(applied) = g.supersede_commits.get(&pair) {
            return if applied.winner == commit.winner && applied.loser == commit.loser {
                Ok(SupersedeCommitOutcome::AlreadyApplied)
            } else {
                Err(Error::Conflict(
                    "supersede pair already committed in the opposite direction".into(),
                ))
            };
        }
        if g.contradictions
            .get(&pair)
            .is_some_and(|contradiction| !contradiction.is_open())
        {
            return Err(Error::Conflict(
                "supersede contradiction already has a terminal resolution".into(),
            ));
        }

        g.nodes
            .get(&commit.winner)
            .ok_or(Error::NotFound)?
            .validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let loser = g.nodes.get(&commit.loser).ok_or(Error::NotFound)?;
        loser
            .validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;

        let edge_key = (commit.winner, commit.loser);
        if !g.edges.contains_key(&edge_key) {
            for endpoint in [commit.winner, commit.loser] {
                if g.edge_keys_by_endpoint
                    .get(&endpoint)
                    .is_some_and(|keys| keys.len() >= MAX_INCIDENT_EDGES)
                {
                    return Err(incident_edge_capacity_error());
                }
            }
        }

        // Finish every fallible allocation before publishing a prefix under the
        // reference adapter's one lock.
        g.supersede_commits
            .try_reserve(1)
            .map_err(|error| Error::Backend(format!("reserve supersede record: {error}")))?;
        if !g.contradictions.contains_key(&pair) {
            g.contradictions.try_reserve(1).map_err(|error| {
                Error::Backend(format!("reserve supersede contradiction: {error}"))
            })?;
        }

        let mut contradiction =
            g.contradictions.get(&pair).cloned().unwrap_or_else(|| {
                Contradiction::new(commit.winner, commit.loser, commit.applied_at)
            });
        if g.contradictions.contains_key(&pair) {
            contradiction.observe(commit.applied_at);
        }
        contradiction.resolve(Resolution::Superseded);

        let edge = Edge::new(
            commit.winner,
            commit.loser,
            1.0,
            mneme_core::EdgeKind::Supersedes,
            commit.applied_at,
        );
        let next_epoch = g.preflight_epoch_advance(true)?;
        g.edges.insert(edge_key, edge);
        g.index_edge_key(edge_key);
        g.update_existing_node(commit.loser, |loser| {
            loser.set_status(NodeStatus::Archived);
        });
        g.contradictions.insert(pair, contradiction);
        g.supersede_commits.insert(pair, commit.record());
        g.finish_epoch_advance(next_epoch);
        Ok(SupersedeCommitOutcome::Applied)
    }

    async fn commit_maintenance(
        &self,
        _: ColdPath,
        commit: &MaintenanceCommit,
    ) -> Result<MaintenanceCommitOutcome> {
        commit.validate()?;
        let mut g = self.lock();
        for mutation in &commit.edges {
            let key = mutation.key();
            g.require_semantic_ids(&[key.from, key.to])?;
        }
        let mut applied_edges = Vec::with_capacity(commit.edges.len());
        let mut changed = false;

        // Validate matching edge CAS rows before publishing any mutation.
        for mutation in &commit.edges {
            let key = mutation.key();
            if let Some(current) = g.edges.get(&(key.from, key.to))
                && same_serialized(current, mutation.expected())?
            {
                applied_edges.push(key);
                changed |= match mutation.replacement() {
                    Some(replacement) => !same_serialized(current, replacement)?,
                    None => true,
                };
            }
        }

        // Validation normally guarantees that a matching maintenance CAS changes
        // its row, but decide from the final canonical values so that an exact
        // replacement can never mint an epoch if the domain contract evolves.
        let next_epoch = g.preflight_epoch_advance(changed)?;

        let applied_edge_set: HashSet<_> = applied_edges.iter().copied().collect();
        for mutation in &commit.edges {
            let key = mutation.key();
            if !applied_edge_set.contains(&key) {
                continue;
            }
            match mutation {
                MaintenanceEdgeMutation::Decay { replacement, .. } => {
                    // The identity already exists, so this cannot increase
                    // endpoint degree or trip the admission ceiling.
                    g.edges.insert((key.from, key.to), replacement.clone());
                    g.index_edge_key((key.from, key.to));
                }
                MaintenanceEdgeMutation::DeleteWeak { .. } => {
                    g.delete_edge((key.from, key.to));
                }
            }
        }

        g.finish_epoch_advance(next_epoch);

        Ok(MaintenanceCommitOutcome { applied_edges })
    }

    async fn maintenance_overfull_hubs(
        &self,
        _: ColdPath,
        candidates: &[NodeId],
        target_degree: usize,
    ) -> Result<Vec<NodeId>> {
        if candidates.len() > MAX_MAINTENANCE_BATCH_ROWS {
            return Err(Error::CapacityExceeded {
                resource: "maintenance hub preflight",
                limit: MAX_MAINTENANCE_BATCH_ROWS,
            });
        }
        let g = self.lock();
        Ok(candidates
            .iter()
            .copied()
            .filter(|id| {
                g.edge_keys_by_endpoint.get(id).is_some_and(|keys| {
                    keys.iter()
                        .filter(|key| {
                            g.edges.get(key).is_some_and(|edge| {
                                edge.kind.is_undirected() && !g.is_episode_edge(edge)
                            })
                        })
                        .count()
                        > target_degree
                })
            })
            .collect())
    }

    async fn prune_incident_associations(
        &self,
        _: ColdPath,
        hub: NodeId,
        target_degree: usize,
        max_deletes: usize,
    ) -> Result<DensePruneChunkOutcome> {
        if !(1..=MAX_MAINTENANCE_BATCH_ROWS).contains(&max_deletes) {
            return Err(Error::InvalidInput(format!(
                "maintenance prune limit must be in 1..={MAX_MAINTENANCE_BATCH_ROWS}"
            )));
        }
        let mut g = self.lock();
        let mut associations: Vec<_> = g
            .full_merge_incident_edges(hub, hub)?
            .into_iter()
            .filter(|edge| edge.kind.is_undirected() && !g.is_episode_edge(edge))
            .collect();
        associations.sort_by(|left, right| {
            left.weight()
                .total_cmp(&right.weight())
                .then_with(|| left.trials().cmp(&right.trials()))
                .then_with(|| left.from.cmp(&right.from))
                .then_with(|| left.to.cmp(&right.to))
        });
        let excess = associations.len().saturating_sub(target_degree);
        let pruned = excess.min(max_deletes);
        let next_epoch = g.preflight_epoch_advance(pruned > 0)?;
        for edge in associations.into_iter().take(pruned) {
            g.delete_edge((edge.from, edge.to));
        }
        g.finish_epoch_advance(next_epoch);
        Ok(DensePruneChunkOutcome {
            pruned,
            remaining_excess: excess.saturating_sub(pruned),
        })
    }

    async fn observe_contradiction(&self, a: NodeId, b: NodeId, at: Timestamp) -> Result<()> {
        let pair = UnorderedPair(a, b);
        let mut g = self.lock();
        g.require_semantic_ids(&[a, b])?;
        let is_new = !g.contradictions.contains_key(&pair);
        let replacement = match g.contradictions.get(&pair) {
            Some(current) => {
                let mut replacement = current.clone();
                replacement.observe(at);
                replacement
            }
            None => Contradiction::new(a, b, at),
        };
        replacement.validate().map_err(Error::InvalidInput)?;
        if replacement.is_open() {
            validate_open_overlay_endpoints(pair, "contradiction", |id| g.nodes.contains_key(&id))?;
        }
        if is_new {
            g.contradictions.try_reserve(1).map_err(|error| {
                Error::Backend(format!("reserve contradiction observation: {error}"))
            })?;
        }
        let changed = match g.contradictions.get(&pair) {
            Some(current) => !same_serialized(current, &replacement)?,
            None => true,
        };
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            g.contradictions.insert(pair, replacement);
            g.finish_epoch_advance(next_epoch);
        }
        Ok(())
    }

    async fn open_contradictions(&self, _: ColdPath) -> Result<Vec<Contradiction>> {
        Ok(self
            .lock()
            .contradictions
            .values()
            .filter(|c| c.is_open())
            .cloned()
            .collect())
    }

    async fn resolve_contradiction(
        &self,
        pair: UnorderedPair<NodeId>,
        resolution: Resolution,
    ) -> Result<()> {
        let mut g = self.lock();
        g.require_semantic_ids(&[pair.0, pair.1])?;
        let current = g.contradictions.get(&pair).ok_or(Error::NotFound)?;
        if let Some(existing) = current.resolution
            && existing != Resolution::Unresolved
        {
            return if existing == resolution {
                Ok(())
            } else {
                Err(Error::Conflict(
                    "contradiction already has a different terminal resolution".into(),
                ))
            };
        }
        let mut replacement = current.clone();
        replacement.resolve(resolution);
        let changed = !same_serialized(current, &replacement)?;
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            g.contradictions.insert(pair, replacement);
            g.finish_epoch_advance(next_epoch);
        }
        Ok(())
    }

    async fn observe_merge_candidate(&self, a: NodeId, b: NodeId, at: Timestamp) -> Result<()> {
        let pair = UnorderedPair(a, b);
        let mut g = self.lock();
        g.require_semantic_ids(&[a, b])?;
        let is_new = !g.merges.contains_key(&pair);
        let replacement = match g.merges.get(&pair) {
            Some(current) => {
                let mut replacement = current.clone();
                replacement.observe(at);
                replacement
            }
            None => MergeCandidate::new(a, b, at),
        };
        replacement.validate().map_err(Error::InvalidInput)?;
        if replacement.is_open() {
            validate_open_overlay_endpoints(pair, "merge candidate", |id| {
                g.nodes.contains_key(&id)
            })?;
        }
        if is_new {
            g.merges
                .try_reserve(1)
                .map_err(|error| Error::Backend(format!("reserve merge observation: {error}")))?;
        }
        let changed = match g.merges.get(&pair) {
            Some(current) => !same_serialized(current, &replacement)?,
            None => true,
        };
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            g.merges.insert(pair, replacement);
            g.finish_epoch_advance(next_epoch);
        }
        Ok(())
    }

    async fn open_merge_candidates(&self, _: ColdPath) -> Result<Vec<MergeCandidate>> {
        Ok(self
            .lock()
            .merges
            .values()
            .filter(|m| m.is_open())
            .cloned()
            .collect())
    }

    async fn resolve_merge_candidate(
        &self,
        pair: UnorderedPair<NodeId>,
        resolution: MergeResolution,
    ) -> Result<()> {
        let mut g = self.lock();
        g.require_semantic_ids(&[pair.0, pair.1])?;
        let current = g.merges.get(&pair).ok_or(Error::NotFound)?;
        if let Some(existing) = current.resolution {
            return if existing == resolution {
                Ok(())
            } else {
                Err(Error::Conflict(
                    "merge candidate already has a different terminal resolution".into(),
                ))
            };
        }
        let mut replacement = current.clone();
        replacement.resolve(resolution);
        let changed = !same_serialized(current, &replacement)?;
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            g.merges.insert(pair, replacement);
            g.finish_epoch_advance(next_epoch);
        }
        Ok(())
    }

    async fn put_remote_edge(&self, edge: &RemoteEdge) -> Result<()> {
        edge.validate_for_source_database(self.db_id)
            .map_err(Error::InvalidInput)?;
        let replacement = RemoteEdge::new(edge.from, edge.target_db, edge.target, edge.weight());
        let key = (replacement.from, replacement.target_db, replacement.target);
        let mut g = self.lock();
        let indexed = g.remote_edge_index_matches_exactly(&replacement);
        let changed = g.remote_edges.get(&key) != Some(&replacement) || !indexed;
        if !changed {
            return Ok(());
        }
        g.validate_remote_edge_upsert_capacity(&replacement)?;
        let next_epoch = g.preflight_epoch_advance(true)?;
        g.put_remote_edge_prevalidated(replacement);
        g.finish_epoch_advance(next_epoch);
        Ok(())
    }

    async fn remote_edges_page(
        &self,
        from: NodeId,
        after: Option<RemoteEdgeCursor>,
        limit: usize,
    ) -> Result<RemoteEdgePage> {
        self.lock().remote_edges_page(from, after, limit)
    }

    async fn delete_remote_edge(
        &self,
        from: NodeId,
        target_db: Ulid,
        target: NodeId,
    ) -> Result<()> {
        let key = (from, target_db, target);
        let mut g = self.lock();
        let changed = g.remote_edges.contains_key(&key)
            || g.remote_edge_keys_by_source.get(&from).is_some_and(|keys| {
                keys.iter().any(|(_, indexed_db, indexed_target)| {
                    *indexed_db == target_db && *indexed_target == target
                })
            });
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            g.delete_remote_edge(key);
            g.finish_epoch_advance(next_epoch);
        }
        Ok(())
    }
}

fn request_admits_physical_status(
    request: &TaggedAnnRequest<'_>,
    status: TaggedPhysicalStatus,
) -> bool {
    request
        .lanes
        .iter()
        .any(|lane| status.is_admitted_by(lane.status))
}

fn mem_tagged_exact_prefix(
    inner: &Inner,
    request: &TaggedAnnRequest<'_>,
) -> Result<(usize, BTreeSet<NodeId>, Option<TaggedExactWorkLimit>)> {
    let mut raw_memberships = 0usize;
    let mut unique_ids = BTreeSet::new();
    let unique_canary = request.limits.max_unique_exact_ids() + 1;

    'scan: for tag in request.tags() {
        for status in TaggedPhysicalStatus::ALL {
            if !request_admits_physical_status(request, status) {
                continue;
            }
            let Some(members) = inner.tag_projection.status(status).get(*tag) else {
                continue;
            };
            for &(_, id) in members {
                // Raw membership is charged before cross-tag ID deduplication.
                raw_memberships += 1;
                if unique_ids.len() < unique_canary {
                    unique_ids.insert(id);
                }
                if raw_memberships > request.limits.max_raw_memberships() {
                    break 'scan;
                }
            }
        }
    }

    let overflow = tagged_exact_work_overflow(
        raw_memberships,
        unique_ids.len(),
        request.query.len(),
        request.limits,
    )?;
    Ok((raw_memberships, unique_ids, overflow))
}

fn mem_exact_tagged_batch(
    inner: &Inner,
    request: &TaggedAnnRequest<'_>,
    raw_memberships: usize,
    exact_ids: BTreeSet<NodeId>,
    generation: TaggedProjectionGeneration,
) -> Result<TaggedAnnBatch> {
    let unique_exact_ids = exact_ids.len();
    let mut lane_scores: Vec<_> = request
        .lanes
        .iter()
        .map(|lane| BinaryHeap::with_capacity(lane.k))
        .collect();
    let mut exact_hydrated_ids = 0usize;

    for id in exact_ids {
        let Some(node) = inner.nodes.get(&id) else {
            continue;
        };
        let Some((lane_index, _)) = request
            .lanes
            .iter()
            .enumerate()
            .find(|(_, lane)| lane.status.allows(node.status()))
        else {
            continue;
        };
        if !request.tags().iter().any(|tag| node.has_tag(tag)) {
            continue;
        }
        let Some(vector) = inner.vectors.get(&id) else {
            continue;
        };
        if vector.len() != request.query.len() {
            return Err(Error::DimMismatch {
                index: request.query.len(),
                provider: vector.len(),
            });
        }
        validate_cosine_vector(vector, "stored tagged vector embedding")?;
        exact_hydrated_ids += 1;
        retain_tagged_top_k(
            &mut lane_scores[lane_index],
            Scored {
                id,
                score: cosine(request.query, vector).clamp(-1.0, 1.0),
            },
            request.lanes[lane_index].k,
        );
    }

    let exact_vector_components = request
        .limits
        .checked_exact_components(exact_hydrated_ids, request.query.len())?;
    let lanes = request
        .lanes
        .iter()
        .zip(lane_scores)
        .map(|(requested, heap)| {
            let mut hits: Vec<_> = heap.into_iter().map(|ranked| ranked.0).collect();
            hits.sort_by(scored_order);
            TaggedAnnLane {
                lane: requested.lane,
                hits,
                seed_coverage: TaggedSeedCoverage::ExactCosine,
            }
        })
        .collect();
    TaggedAnnBatch::new(
        lanes,
        TaggedAnnWork {
            query_dimension: request.query.len(),
            raw_memberships,
            unique_exact_ids,
            exact_hydrated_ids,
            exact_vector_components,
            fallback_hnsw_inspected: 0,
            fallback_sample_inspected: 0,
            fallback_unique_ids: 0,
            fallback_canonical_candidates_checked: 0,
            fallback_matching_candidates: 0,
            fallback_hydrated_ids: 0,
            fallback_vector_components: 0,
        },
        generation,
    )
}

fn mem_sample_tag_members(
    members: Option<&BTreeSet<TagSampleKey>>,
    pivot: i64,
    quota: usize,
    status: TaggedPhysicalStatus,
    lane_candidates: &mut BTreeMap<NodeId, BTreeSet<TaggedPhysicalStatus>>,
    all_candidates: &mut BTreeSet<NodeId>,
) -> usize {
    let Some(members) = members else {
        return 0;
    };
    let start = (pivot, NodeId(Ulid::from(0u128)));
    let mut inspected = 0usize;
    for &(_, id) in members
        .range(start..)
        .chain(members.range(..start))
        .take(quota)
    {
        inspected += 1;
        // Membership work is already charged above; only now may IDs dedup.
        lane_candidates.entry(id).or_default().insert(status);
        all_candidates.insert(id);
    }
    inspected
}

fn mem_fallback_tagged_batch(
    inner: &Inner,
    request: &TaggedAnnRequest<'_>,
    raw_memberships: usize,
    unique_exact_ids: usize,
    exceeded_limit: TaggedExactWorkLimit,
    retrieval_generation: &str,
    projection_generation: TaggedProjectionGeneration,
) -> Result<TaggedAnnBatch> {
    let mut lanes = Vec::with_capacity(request.lanes.len());
    let mut all_candidates = BTreeSet::new();
    let mut fallback_sample_inspected = 0usize;
    let mut fallback_canonical_candidates_checked = 0usize;
    let mut fallback_matching_candidates = 0usize;
    let mut fallback_hydrated_ids = 0usize;

    for requested in request.lanes {
        let mut lane_candidates: BTreeMap<NodeId, BTreeSet<TaggedPhysicalStatus>> = BTreeMap::new();
        let mut physical = Vec::new();
        for physical_quota in
            tagged_physical_status_quotas(request, requested.lane, retrieval_generation)?
        {
            let legs = tagged_fallback_leg_quotas(
                physical_quota.candidates,
                TaggedFallbackStrategy::DeterministicHashedTagSample,
            )?;
            debug_assert_eq!(legs.hnsw, 0);
            let pivot = tagged_sample_pivot(
                request,
                physical_quota.status,
                requested.lane,
                retrieval_generation,
            )?;
            let mut tag_samples = Vec::with_capacity(request.tags().len());
            for (index, tag_quota) in tagged_query_tag_quotas(
                request,
                physical_quota.status,
                requested.lane,
                retrieval_generation,
                legs.sample,
            )?
            .into_iter()
            .enumerate()
            {
                let inspected = mem_sample_tag_members(
                    inner
                        .tag_projection
                        .status(physical_quota.status)
                        .get(tag_quota.tag),
                    pivot,
                    tag_quota.candidates,
                    physical_quota.status,
                    &mut lane_candidates,
                    &mut all_candidates,
                );
                fallback_sample_inspected += inspected;
                tag_samples.push(TaggedQueryTagSeedCoverage::new(
                    u8::try_from(index).expect("tag request cap fits u8"),
                    tag_quota.candidates,
                    inspected,
                )?);
            }
            physical.push(TaggedPhysicalSeedCoverage::new(
                physical_quota.status,
                0,
                0,
                tag_samples,
                pivot,
            )?);
        }

        let mut canonical_candidates_checked = 0usize;
        let mut matching_candidates = 0usize;
        let mut hits = Vec::new();
        for (id, sampled_statuses) in lane_candidates {
            canonical_candidates_checked += 1;
            let Some(node) = inner.nodes.get(&id) else {
                continue;
            };
            let canonical_status = TaggedPhysicalStatus::from(node.status());
            if !sampled_statuses.contains(&canonical_status)
                || !requested.status.allows(node.status())
                || !request.tags().iter().any(|tag| node.has_tag(tag))
            {
                continue;
            }
            matching_candidates += 1;
            let Some(vector) = inner.vectors.get(&id) else {
                continue;
            };
            if vector.len() != request.query.len() {
                return Err(Error::DimMismatch {
                    index: request.query.len(),
                    provider: vector.len(),
                });
            }
            validate_cosine_vector(vector, "stored tagged fallback vector embedding")?;
            fallback_hydrated_ids += 1;
            hits.push(Scored {
                id,
                score: cosine(request.query, vector).clamp(-1.0, 1.0),
            });
        }
        fallback_canonical_candidates_checked += canonical_candidates_checked;
        fallback_matching_candidates += matching_candidates;
        hits.sort_by(scored_order);
        hits.truncate(requested.k);
        lanes.push(TaggedAnnLane {
            lane: requested.lane,
            hits,
            seed_coverage: TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter {
                exceeded_limit,
                raw_memberships,
                physical,
                canonical_candidates_checked,
                matching_candidates,
            },
        });
    }

    let fallback_vector_components = request
        .limits
        .checked_fallback_components(fallback_hydrated_ids, request.query.len())?;
    TaggedAnnBatch::new(
        lanes,
        TaggedAnnWork {
            query_dimension: request.query.len(),
            raw_memberships,
            unique_exact_ids,
            exact_hydrated_ids: 0,
            exact_vector_components: 0,
            fallback_hnsw_inspected: 0,
            fallback_sample_inspected,
            fallback_unique_ids: all_candidates.len(),
            fallback_canonical_candidates_checked,
            fallback_matching_candidates,
            fallback_hydrated_ids,
            fallback_vector_components,
        },
        projection_generation,
    )
}

#[async_trait]
impl VectorIndex for MemStore {
    fn semantic_id(&self) -> &'static str {
        "mneme-exact-cosine-semantic-status-v3"
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn tagged_projection_generation(&self) -> Option<&'static str> {
        Some(MEM_TAGGED_PROJECTION_GENERATION)
    }

    async fn upsert(&self, id: NodeId, embedding: &[f32]) -> Result<()> {
        if embedding.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: embedding.len(),
            });
        }
        validate_cosine_vector(embedding, "vector embedding")?;
        let replacement = embedding.to_vec();
        let mut g = self.lock();
        g.require_semantic_ids(&[id])?;
        if !g.nodes.contains_key(&id) {
            return Err(Error::NotFound);
        }
        let changed = g
            .vectors
            .get(&id)
            .is_none_or(|current| !same_vector_bits(current, &replacement));
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            g.vectors.insert(id, replacement);
            g.finish_epoch_advance(next_epoch);
        }
        Ok(())
    }

    async fn remove(&self, id: NodeId) -> Result<()> {
        let mut g = self.lock();
        g.require_semantic_ids(&[id])?;
        let changed = g.vectors.contains_key(&id);
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            g.vectors.remove(&id);
            g.finish_epoch_advance(next_epoch);
        }
        Ok(())
    }

    async fn ann(&self, query: &[f32], k: usize, status: StatusFilter) -> Result<Vec<Scored>> {
        if query.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: query.len(),
            });
        }
        validate_cosine_vector(query, "ANN query vector")?;
        let g = self.lock();

        let mut scored: Vec<Scored> = Vec::new();
        for (id, v) in g.vectors.iter() {
            if v.len() != self.dim {
                return Err(Error::DimMismatch {
                    index: self.dim,
                    provider: v.len(),
                });
            }
            validate_cosine_vector(v, "stored vector embedding")?;
            // Restrict to the admitted statuses inside the index so an
            // ineligible lifecycle tier cannot starve the requested top-k.
            let keep = match g.nodes.get(id) {
                Some(n) if !n.is_semantic() || !status.allows(n.status()) => false,
                Some(_) => true,
                None => false,
            };
            if keep {
                scored.push(Scored {
                    id: *id,
                    score: cosine(query, v),
                });
            }
        }
        scored.sort_by(scored_order);
        scored.truncate(k);
        Ok(scored)
    }

    async fn tagged_ann(&self, request: TaggedAnnRequest<'_>) -> Result<TaggedAnnBatch> {
        request.validate()?;
        if request.query.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: request.query.len(),
            });
        }
        validate_cosine_vector(request.query, "tagged ANN query vector")?;
        let projection_generation =
            TaggedProjectionGeneration::new(MEM_TAGGED_PROJECTION_GENERATION)?;
        let retrieval_generation = <Self as VectorIndex>::semantic_id(self);

        // One lock owns the complete canonical/projection/vector snapshot.
        let inner = self.lock();
        let (raw_memberships, exact_ids, overflow) = mem_tagged_exact_prefix(&inner, &request)?;
        let batch = match overflow {
            None => mem_exact_tagged_batch(
                &inner,
                &request,
                raw_memberships,
                exact_ids,
                projection_generation,
            )?,
            Some(exceeded_limit) => {
                let unique_exact_ids = exact_ids.len();
                // The biased exact prefix is never retained or scored.
                drop(exact_ids);
                mem_fallback_tagged_batch(
                    &inner,
                    &request,
                    raw_memberships,
                    unique_exact_ids,
                    exceeded_limit,
                    retrieval_generation,
                    projection_generation,
                )?
            }
        };
        batch.validate_against(
            &request,
            retrieval_generation,
            MEM_TAGGED_PROJECTION_GENERATION,
        )?;
        Ok(batch)
    }
}

#[async_trait]
impl LexicalIndex for MemStore {
    fn semantic_id(&self) -> &'static str {
        "mneme-semantic-eligible-corpus-bm25-v2"
    }

    async fn search(&self, query: &str, k: usize, status: StatusFilter) -> Result<Vec<Scored>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let query_terms: HashSet<String> = lexical_terms(query).into_iter().collect();
        if query_terms.is_empty() {
            return Ok(Vec::new());
        }

        // This is deliberately a transparent reference implementation, not the
        // production scaling path. Compute BM25 over the exact eligible corpus so
        // tests and dependency-free builds preserve lifecycle semantics.
        let g = self.lock();
        let mut documents = Vec::new();
        let mut document_frequency: HashMap<String, usize> = HashMap::new();
        let mut total_terms = 0usize;
        for node in g
            .nodes
            .values()
            .filter(|node| node.is_semantic() && status.allows(node.status()))
        {
            let terms = lexical_terms(node.summary());
            let document_len = terms.len();
            total_terms += document_len;
            let mut frequencies: HashMap<String, usize> = HashMap::new();
            for term in terms {
                if query_terms.contains(&term) {
                    *frequencies.entry(term).or_default() += 1;
                }
            }
            for term in frequencies.keys() {
                *document_frequency.entry(term.clone()).or_default() += 1;
            }
            documents.push((node.id(), frequencies, document_len));
        }
        if documents.is_empty() {
            return Ok(Vec::new());
        }

        const K1: f32 = 1.2;
        const B: f32 = 0.75;
        let corpus_len = documents.len() as f32;
        let average_len = (total_terms as f32 / corpus_len).max(1.0);
        let mut scored = Vec::new();
        for (id, frequencies, len) in documents {
            let mut score = 0.0f32;
            for term in &query_terms {
                let Some(&frequency) = frequencies.get(term) else {
                    continue;
                };
                let df = *document_frequency.get(term.as_str()).unwrap_or(&0) as f32;
                let idf = (1.0 + (corpus_len - df + 0.5) / (df + 0.5)).ln();
                let tf = frequency as f32;
                let length_norm = 1.0 - B + B * len as f32 / average_len;
                score += idf * (tf * (K1 + 1.0)) / (tf + K1 * length_norm);
            }
            if score > 0.0 {
                scored.push(Scored { id, score });
            }
        }
        scored.sort_by(scored_order);
        scored.truncate(k);
        Ok(scored)
    }
}

impl MemStore {
    async fn spread_inner(
        &self,
        seeds: &[Scored],
        budget: Budget,
        query: Option<&[f32]>,
        scope: TraversalScope,
        trace: bool,
        routing_biases: Option<&routing_probe::RoutingBiasMap>,
    ) -> Result<mneme_core::ports::SpreadResult> {
        let active_ordering = routing_biases.is_some_and(|biases| !biases.is_empty());
        let mut ordering = active_ordering.then(BTreeMap::new);
        let mut paths = (trace || active_ordering).then(BTreeMap::new);
        if budget.max_nodes == 0 {
            return Ok(mneme_core::ports::SpreadResult {
                hits: Vec::new(),
                paths: trace.then_some(paths).flatten(),
                ordering,
            });
        }
        let gamma = budget.query_conditioning;
        if gamma > 0.0
            && let Some(query) = query
            && query.len() != self.dim
        {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: query.len(),
            });
        }

        // Layered weighted BFS. Filtering is part of traversal, not a final
        // presentation pass: forbidden lifecycle tiers never enter `best`, a
        // frontier, or the per-source fan-out competition.
        let g = self.lock();
        let mut seed_scores: HashMap<NodeId, f32> = HashMap::new();
        for s in seeds {
            let Some(node) = g.nodes.get(&s.id) else {
                continue;
            };
            if !node.is_semantic() || !scope.allows(s.id, node.status()) {
                continue;
            }
            let a = s.score.max(0.0);
            propose_score(&mut seed_scores, s.id, a);
        }
        let admitted_seeds = bounded_ranked(seed_scores, budget.max_nodes);
        let mut best: HashMap<NodeId, f32> = admitted_seeds
            .iter()
            .map(|seed| (seed.id, seed.score))
            .collect();
        let mut frontier = best.clone();

        let mut expanded_sources = 0usize;
        for _depth in 0..budget.max_depth {
            if frontier.is_empty() || expanded_sources >= budget.max_nodes {
                break;
            }

            let mut sources: Vec<Scored> = frontier
                .drain()
                .map(|(id, score)| Scored { id, score })
                .collect();
            sources.sort_by(|a, b| routed_scored_order(a, b, ordering.as_ref()));
            sources.truncate(budget.max_nodes - expanded_sources);
            expanded_sources += sources.len();

            let mut proposals: HashMap<NodeId, f32> = HashMap::new();
            let mut proposed_paths = BTreeMap::new();
            for source in sources {
                // The derived endpoint projection makes one frontier layer
                // O(sum of source degrees), rather than O(total edges).
                let incident = g.incident_edges(source.id);
                let mut neighbors = oriented_neighbors(source.id, &incident);
                neighbors.retain(|neighbor| {
                    g.nodes.get(&neighbor.node).is_some_and(|node| {
                        node.is_semantic() && scope.allows(neighbor.node, node.status())
                    })
                });
                if let Some(biases) = routing_biases.filter(|biases| !biases.is_empty()) {
                    neighbors.sort_by(|a, b| {
                        routing_probe::biased_neighbor_order(source.id, a, 1.0, b, 1.0, biases)
                    });
                } else {
                    neighbors.sort_by(neighbor_order);
                }
                let conditioned = gamma > 0.0 && query.is_some();
                neighbors.truncate(if conditioned {
                    SPREAD_CONDITION_CANDIDATES
                } else {
                    SPREAD_FANOUT
                });
                let mut neighbors: Vec<(Neighbor, f32)> = neighbors
                    .into_iter()
                    .map(|neighbor| {
                        let factor = if let Some(query) = query.filter(|_| gamma > 0.0) {
                            let sim = g
                                .vectors
                                .get(&neighbor.node)
                                .map_or(0.0, |vector| cosine(query, vector));
                            conditioning_factor(gamma, sim)
                        } else {
                            1.0
                        };
                        (neighbor, factor)
                    })
                    .collect();
                if let Some(biases) = routing_biases.filter(|biases| !biases.is_empty()) {
                    neighbors.sort_by(|(a, af), (b, bf)| {
                        routing_probe::biased_neighbor_order(source.id, a, *af, b, *bf, biases)
                    });
                } else {
                    neighbors.sort_by(|(a, af), (b, bf)| {
                        let a_score = a.edge.weight() * af;
                        let b_score = b.edge.weight() * bf;
                        b_score
                            .total_cmp(&a_score)
                            .then_with(|| neighbor_order(a, b))
                    });
                }
                neighbors.truncate(SPREAD_FANOUT);

                for (neighbor, factor) in neighbors {
                    let original = source.score * neighbor.edge.weight() * factor;
                    record_routing_order(
                        &mut ordering,
                        routing_biases,
                        source,
                        &neighbor,
                        original,
                        budget.min_relevance,
                        &paths,
                    );
                    let mut propagated = original;
                    if propagated < budget.min_relevance {
                        // ε-exploration: occasionally follow an under-weighted
                        // edge, admitting it at the relevance floor.
                        if budget.explore > 0.0
                            && explore_draw(source.id, neighbor.node) < budget.explore
                        {
                            propagated = budget.min_relevance;
                        } else {
                            continue;
                        }
                    }
                    if propagated > best.get(&neighbor.node).copied().unwrap_or(0.0)
                        && propagated > proposals.get(&neighbor.node).copied().unwrap_or(0.0)
                    {
                        propose_score(&mut proposals, neighbor.node, propagated);
                        if let Some(paths) = &paths {
                            // Copy now: a predecessor can later acquire another
                            // winning route without having propagated it here.
                            let mut path = paths.get(&source.id).cloned().unwrap_or_default();
                            path.push(mneme_core::ports::TraversalHop {
                                previous: source.id,
                                target: neighbor.node,
                                edge: neighbor.edge.clone(),
                            });
                            proposed_paths.insert(neighbor.node, path);
                        }
                    }
                }
            }
            frontier =
                admit_layer_routed(&mut best, proposals, budget.max_nodes, ordering.as_ref());
            if let Some(ordering) = &mut ordering {
                ordering.retain(|id, _| best.contains_key(id));
            }
            if let Some(paths) = &mut paths {
                for id in frontier.keys() {
                    if let Some(path) = proposed_paths.remove(id) {
                        paths.insert(*id, path);
                    }
                }
            }
        }

        let mut out: Vec<Scored> = best
            .into_iter()
            .map(|(id, score)| Scored { id, score })
            .collect();
        out.sort_by(scored_order);
        Ok(mneme_core::ports::SpreadResult {
            hits: out,
            paths: trace.then_some(paths).flatten(),
            ordering,
        })
    }
}

#[async_trait]
impl Traversal for MemStore {
    async fn spread(
        &self,
        seeds: &[Scored],
        budget: Budget,
        query: Option<&[f32]>,
        scope: TraversalScope,
    ) -> Result<Vec<Scored>> {
        Ok(self
            .spread_inner(seeds, budget, query, scope, false, None)
            .await?
            .hits)
    }

    async fn spread_with_provenance(
        &self,
        seeds: &[Scored],
        budget: Budget,
        query: Option<&[f32]>,
        scope: TraversalScope,
    ) -> Result<mneme_core::ports::SpreadResult> {
        self.spread_inner(seeds, budget, query, scope, true, None)
            .await
    }

    async fn spread_routed(
        &self,
        seeds: &[Scored],
        budget: Budget,
        query: Option<&[f32]>,
        scope: TraversalScope,
        biases: &mneme_core::ports::RoutingBiasMap,
        trace: bool,
    ) -> Result<mneme_core::ports::SpreadResult> {
        self.spread_inner(seeds, budget, query, scope, trace, Some(biases))
            .await
    }

    async fn detect_communities(&self, _: ColdPath) -> Result<Vec<(NodeId, ClusterId)>> {
        // Weighted Louvain over a symmetric view of the edge map (see
        // `weighted_louvain`); the cozo backend builds the same adjacency, so the
        // two agree on what a community is.
        let g = self.lock();
        let mut ids: Vec<NodeId> = g
            .nodes
            .values()
            .filter(|node| node.is_semantic())
            .map(Node::id)
            .collect();
        ids.sort();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let index: HashMap<NodeId, usize> =
            ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();

        let mut adj_w: Vec<HashMap<usize, f32>> = vec![HashMap::new(); ids.len()];
        for ((from, to), edge) in g.edges.iter() {
            if let (Some(&a), Some(&b)) = (index.get(from), index.get(to)) {
                *adj_w[a].entry(b).or_insert(0.0) += edge.weight();
                *adj_w[b].entry(a).or_insert(0.0) += edge.weight();
            }
        }
        drop(g);
        let adj: Vec<Vec<(usize, f32)>> =
            adj_w.into_iter().map(|m| m.into_iter().collect()).collect();

        let comm = weighted_louvain(ids.len(), &adj);
        Ok(densify(&ids, &comm))
    }
}

pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len().min(b.len()) {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Shared minimal analyzer for the dependency-free BM25 path and for turning an
/// arbitrary user string into a safe native-FTS query. Keeping only Unicode
/// alphanumeric runs avoids treating user punctuation as FTS query syntax.
pub(crate) fn lexical_terms(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing_probe::{RoutingBiasMap, RoutingRoute, SignedRoutingBias};
    use mneme_core::ports::{
        Budget, FeedbackCommit, FeedbackCommitOutcome, FeedbackEdgeUpdate, FeedbackIdempotency,
        FeedbackMergeObservation, FeedbackNodeUpdate, FeedbackRetryScope,
        MAX_TAGGED_RAW_MEMBERSHIPS, RetrievalLifecycleLane, Scored, StatusFilter,
        TaggedAnnLaneRequest, TaggedAnnWorkLimits, TraversalScope,
    };
    use mneme_core::{BodyRef, EdgeKind, MAX_FEEDBACK_BATCH_EVENTS, NodeStatus, Provenance};
    use ulid::Ulid;

    #[tokio::test]
    async fn canonical_export_comparison_is_order_independent_and_value_exact() {
        use crate::concern_tests::{finding, node, notice, resulting};
        let store = MemStore::new(4);
        store
            .set_embedding_fingerprint(&EmbeddingFingerprint::new(
                "test:canonical-export-v1",
                4,
                "l2-f32-v1",
                "symmetric-v1",
            ))
            .unwrap();
        let nodes: Vec<_> = (1..=4).map(|id| node(id, "canonical comparison")).collect();
        for node in &nodes {
            store.put_node(node).await.unwrap();
            store
                .upsert(node.id(), &[1.0, 0.0, 0.0, 0.0])
                .await
                .unwrap();
        }
        for pair in nodes.chunks_exact(2) {
            store
                .put_edge(&Edge::new(
                    pair[0].id(),
                    pair[1].id(),
                    0.5,
                    EdgeKind::Transition,
                    1,
                ))
                .await
                .unwrap();
            let row = resulting(
                store
                    .update_concern(&notice(&pair[0], &pair[1], ConcernKind::Disagreement))
                    .await
                    .unwrap(),
            );
            store
                .update_concern(&finding(row, "canonical export fixture"))
                .await
                .unwrap();
        }
        let mut original = store.export();
        for pair in nodes.chunks_exact(2) {
            original.contradictions.push(Contradiction {
                between: UnorderedPair(pair[0].id(), pair[1].id()),
                observations: 1,
                first_seen: 1,
                last_seen: 1,
                resolution: None,
            });
            original.merges.push(MergeCandidate {
                between: UnorderedPair(pair[0].id(), pair[1].id()),
                observations: 1,
                first_seen: 1,
                last_seen: 1,
                resolution: None,
            });
            original.remote_edges.push(RemoteEdge::new(
                pair[0].id(),
                Ulid::from(100u128),
                pair[1].id(),
                0.5,
            ));
        }
        let expected = original.clone().canonical_value().unwrap();
        let mut reordered = original.clone();
        reordered.nodes.reverse();
        reordered.edges.reverse();
        reordered.vectors.reverse();
        reordered.contradictions.reverse();
        reordered.merges.reverse();
        reordered.remote_edges.reverse();
        reordered.concerns.reverse();
        assert_eq!(reordered.clone().canonical_value().unwrap(), expected);
        let reloaded = MemStore::from_export(reordered).unwrap();
        assert_eq!(reloaded.export().canonical_value().unwrap(), expected);
        let root = std::env::temp_dir().join(format!("mneme-canonical-export-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("graph.json");
        reloaded.save(&path).unwrap();
        assert_eq!(
            MemStore::load(&path)
                .unwrap()
                .export()
                .canonical_value()
                .unwrap(),
            expected
        );
        std::fs::remove_dir_all(root).unwrap();

        let mut changed = original.clone();
        changed.vectors[0].1[0] = 0.5;
        assert_ne!(changed.canonical_value().unwrap(), expected);
        let mut changed = original.clone();
        changed.contradictions[0].observations += 1;
        assert_ne!(changed.canonical_value().unwrap(), expected);
        let mut changed = original;
        changed.concerns.pop();
        assert_ne!(changed.canonical_value().unwrap(), expected);
    }

    /// Test-only access to the actual adapter path. The public Traversal port
    /// intentionally has no routing-bias argument and always uses the baseline.
    #[async_trait]
    pub(crate) trait RoutingProbeStore: GraphStore + VectorIndex + Traversal {
        async fn probe_spread(
            &self,
            seeds: &[Scored],
            budget: Budget,
            query: Option<&[f32]>,
            scope: TraversalScope,
            biases: Option<&RoutingBiasMap>,
        ) -> Result<mneme_core::ports::SpreadResult>;
    }

    #[async_trait]
    impl RoutingProbeStore for MemStore {
        async fn probe_spread(
            &self,
            seeds: &[Scored],
            budget: Budget,
            query: Option<&[f32]>,
            scope: TraversalScope,
            biases: Option<&RoutingBiasMap>,
        ) -> Result<mneme_core::ports::SpreadResult> {
            self.spread_inner(seeds, budget, query, scope, true, biases)
                .await
        }
    }

    type NeighborSignature = (NodeId, NodeId, EdgeKind, u32, NodeId, bool);

    #[tokio::test]
    async fn legacy_candidate_replay_survives_normalization_and_later_node_edits() {
        use mneme_core::{CaptureReplayProof, CaptureRequestCodec};
        let old = CaptureSource::new_with_codec(
            "legacy",
            "observation",
            "legacy://source",
            None,
            None,
            [7; 32],
            CaptureRequestCodec::CaptureV1,
        )
        .unwrap();
        let node = Node::try_new(
            old.node_id(),
            "original",
            BodyRef::new("inline://body").unwrap(),
            ["core"],
            Provenance::External {
                source: old.clone(),
            },
            0.2,
            0.8,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        let mut wire = serde_json::to_value(&node).unwrap();
        wire["status"] = serde_json::json!({"Candidate": {"use_count": 9}});
        wire["summary"] = serde_json::json!("edited before migration");
        wire["provenance"]["External"]["source"]
            .as_object_mut()
            .unwrap()
            .remove("request_codec");
        let mut malformed = wire.clone();
        malformed["status"] = serde_json::json!({"Candidate": {"use_count": 9, "unknown": 1}});
        assert!(decode_legacy_node_v1(&malformed.to_string()).is_err());
        assert!(
            serde_json::from_value::<Node>(wire.clone()).is_err(),
            "ordinary Node decoder must not accept historical Candidate"
        );
        let (normalized, old_status) = decode_legacy_node_v1(&wire.to_string()).unwrap();
        let mut explicit_semantic = wire.clone();
        explicit_semantic["memory_kind"] = serde_json::json!({"kind": "semantic"});
        assert_eq!(
            decode_legacy_node_v1(&explicit_semantic.to_string())
                .unwrap()
                .0
                .provenance(),
            node.provenance(),
            "an explicit old semantic kind must not be misclassified as episode_v1"
        );
        assert_eq!(old_status, "candidate");
        assert_eq!(normalized.status(), NodeStatus::Active);
        assert_eq!(normalized.summary(), "edited before migration");
        assert_eq!(normalized.confidence().to_bits(), 0.8f32.to_bits());
        assert_eq!(normalized.stability().to_bits(), 0.2f32.to_bits());
        assert_eq!(normalized.provenance(), node.provenance());
        let mut flat = serde_json::to_value(MemStore::new(2).export()).unwrap();
        flat.as_object_mut().unwrap().remove("concerns");
        flat["nodes"] = serde_json::json!([wire]);
        flat["vectors"] = serde_json::to_value(vec![(node.id(), vec![1.0f32, 0.0])]).unwrap();
        let legacy_path =
            std::env::temp_dir().join(format!("mneme-legacy-candidate-{}.json", Ulid::new()));
        std::fs::write(&legacy_path, serde_json::to_vec(&flat).unwrap()).unwrap();
        assert!(MemStore::load(&legacy_path).is_err());
        let from_named_import = MemStore::load_legacy_v1(&legacy_path).unwrap();
        assert_eq!(
            from_named_import
                .get_node(node.id())
                .await
                .unwrap()
                .unwrap()
                .status(),
            NodeStatus::Active
        );
        assert_eq!(
            from_named_import.db_id(),
            serde_json::from_value::<LegacyStoreExportV1>(flat)
                .unwrap()
                .db_id
        );
        let _ = std::fs::remove_file(legacy_path);
        let mut export = MemStore::new(2).export();
        export.nodes.push(normalized.clone());
        export.vectors.push((normalized.id(), vec![1.0, 0.0]));
        let store = MemStore::from_export(export).unwrap();
        let incoming = CaptureSource::new(
            old.namespace(),
            old.key(),
            old.reference(),
            old.session(),
            old.revision(),
            [8; 32],
        )
        .unwrap();
        let proof = CaptureReplayProof::semantic(incoming.clone(), [7; 32], [6; 32]).unwrap();
        assert_eq!(store.lookup_capture(&proof).await.unwrap(), Some(node.id()));
        let old_active = CaptureSource::new_with_codec(
            "legacy",
            "observation",
            "legacy://source",
            None,
            None,
            [6; 32],
            CaptureRequestCodec::CaptureV1,
        )
        .unwrap();
        let active_node = Node::try_new(
            old_active.node_id(),
            "active predecessor",
            BodyRef::new("inline://body").unwrap(),
            ["core"],
            Provenance::External { source: old_active },
            0.2,
            0.8,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        let mut active_export = MemStore::new(2).export();
        active_export.nodes.push(active_node);
        active_export.vectors.push((node.id(), vec![1.0, 0.0]));
        let active_store = MemStore::from_export(active_export).unwrap();
        assert_eq!(
            active_store.lookup_capture(&proof).await.unwrap(),
            Some(node.id())
        );
        let edited = Node::try_new(
            old.node_id(),
            "edited after capture",
            BodyRef::new("inline://body").unwrap(),
            ["core"],
            Provenance::External { source: old },
            0.2,
            0.8,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        store.put_node(&edited).await.unwrap();
        let fresh = Node::try_new(
            incoming.node_id(),
            "original",
            BodyRef::new("inline://body").unwrap(),
            ["core"],
            Provenance::External {
                source: incoming.clone(),
            },
            0.2,
            0.8,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        assert_eq!(
            store
                .commit_capture(&fresh, &[0.0, 1.0], &proof)
                .await
                .unwrap(),
            CaptureCommitOutcome::AlreadyApplied
        );
        assert_eq!(
            store.get_node(node.id()).await.unwrap().unwrap().summary(),
            "edited after capture"
        );
        let changed = CaptureSource::new(
            incoming.namespace(),
            incoming.key(),
            "changed-reference",
            None,
            None,
            [8; 32],
        )
        .unwrap();
        let changed_proof = CaptureReplayProof::semantic(changed, [7; 32], [6; 32]).unwrap();
        assert!(matches!(
            store.lookup_capture(&changed_proof).await,
            Err(Error::Conflict(_))
        ));
    }

    #[test]
    fn named_legacy_json_import_rejects_oversized_source_and_node_inventory() {
        let path = std::env::temp_dir().join(format!("mneme-legacy-limit-{}.json", Ulid::new()));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_LEGACY_JSON_UPGRADE_SOURCE_BYTES + 1)
            .unwrap();
        assert!(matches!(
            MemStore::load_legacy_v1(&path),
            Err(Error::InvalidInput(message)) if message.contains("byte limit")
        ));
        drop(file);
        std::fs::remove_file(&path).unwrap();

        let mut predecessor = serde_json::to_value(MemStore::new(2).export()).unwrap();
        predecessor.as_object_mut().unwrap().remove("concerns");
        let mut legacy: LegacyStoreExportV1 = serde_json::from_value(predecessor).unwrap();
        legacy.nodes = vec![serde_json::Value::Null; MAX_LEGACY_JSON_UPGRADE_NODES + 1];
        assert!(matches!(
            legacy.into_current(),
            Err(Error::InvalidInput(message)) if message.contains("node limit")
        ));
    }

    #[test]
    fn json_v5_envelope_fences_flat_reader_and_requires_identity() {
        let store = MemStore::new(2);
        let path = std::env::temp_dir().join(format!("mneme-v2-envelope-{}.json", Ulid::new()));
        store.save(&path).unwrap();
        let encoded: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(encoded["schema"], STORE_EXPORT_SCHEMA_V5);
        assert!(encoded.get("dim").is_none());
        assert!(encoded["store"].get("db_id").is_some());
        assert!(serde_json::from_value::<LegacyStoreExportV1>(encoded.clone()).is_err());
        assert_eq!(MemStore::load(&path).unwrap().db_id(), store.db_id());
        let mut predecessor = serde_json::to_value(store.export()).unwrap();
        predecessor.as_object_mut().unwrap().remove("concerns");
        let flat = serde_json::to_vec(&predecessor).unwrap();
        std::fs::write(&path, flat).unwrap();
        assert!(MemStore::load(&path).is_err());
        assert_eq!(
            MemStore::load_legacy_v1(&path).unwrap().db_id(),
            store.db_id()
        );
        let mut invalid = encoded;
        invalid["store"].as_object_mut().unwrap().remove("db_id");
        std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(MemStore::load(&path).is_err());
        invalid["store"]["db_id"] = serde_json::to_value(store.db_id()).unwrap();
        invalid["schema"] = serde_json::json!("mneme.store-export.v999");
        std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(MemStore::load(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn node_hydration_is_positional_and_bounded() {
        let store = MemStore::new(4);
        let first = Node::try_new(
            NodeId(Ulid::from(1u128)),
            "first",
            BodyRef::new("inline://first").unwrap(),
            ["shared-tag"],
            Provenance::derived([NodeId(Ulid::from(99u128)), NodeId(Ulid::from(98u128))]).unwrap(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        let second = Node::try_new(
            NodeId(Ulid::from(2u128)),
            "second",
            BodyRef::new("inline://second").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Archived,
            1,
        )
        .unwrap();
        store.put_node(&first).await.unwrap();
        store.put_node(&second).await.unwrap();
        let missing = NodeId(Ulid::from(3u128));

        let hydrated = store
            .get_nodes(&[second.id(), missing, first.id(), second.id()])
            .await
            .unwrap();
        assert_eq!(hydrated.len(), 4);
        assert_eq!(hydrated[0].as_ref().map(Node::id), Some(second.id()));
        assert!(hydrated[1].is_none());
        assert_eq!(hydrated[2].as_ref().map(Node::id), Some(first.id()));
        assert_eq!(hydrated[3].as_ref().map(Node::id), Some(second.id()));

        let repeated = vec![first.id(); MAX_NODE_HYDRATION_BATCH];
        let repeated = store.get_nodes(&repeated).await.unwrap();
        let canonical = repeated[0].as_ref().unwrap();
        match canonical.provenance() {
            Provenance::Derived { from } => assert_eq!(
                from.as_slice(),
                &[NodeId(Ulid::from(99u128)), NodeId(Ulid::from(98u128))]
            ),
            _ => panic!("pointer-sharing fixture must use Derived provenance"),
        }
        for cloned in repeated.iter().map(|node| node.as_ref().unwrap()).skip(1) {
            assert_eq!(cloned.summary().as_ptr(), canonical.summary().as_ptr());
            assert_eq!(
                cloned.tags().next().unwrap().as_ptr(),
                canonical.tags().next().unwrap().as_ptr()
            );
            match (cloned.provenance(), canonical.provenance()) {
                (Provenance::Derived { from: cloned }, Provenance::Derived { from: canonical }) => {
                    assert_eq!(cloned.as_slice().as_ptr(), canonical.as_slice().as_ptr())
                }
                _ => panic!("pointer-sharing fixture must use Derived provenance"),
            }
        }

        let oversized = vec![first.id(); MAX_NODE_HYDRATION_BATCH + 1];
        assert!(matches!(
            store.get_nodes(&oversized).await,
            Err(Error::CapacityExceeded {
                resource: "node hydration batch",
                limit: MAX_NODE_HYDRATION_BATCH,
            })
        ));
    }

    #[tokio::test]
    async fn node_status_lookup_is_positional_and_bounded() {
        let store = MemStore::new(4);
        let active = Node::try_new(
            NodeId(Ulid::from(11u128)),
            "active",
            BodyRef::new("inline://active").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        let candidate = Node::try_new(
            NodeId(Ulid::from(12u128)),
            "candidate",
            BodyRef::new("inline://candidate").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Archived,
            1,
        )
        .unwrap();
        store.put_node(&active).await.unwrap();
        store.put_node(&candidate).await.unwrap();
        let missing = NodeId(Ulid::from(13u128));

        assert_eq!(
            store
                .get_node_statuses(&[candidate.id(), missing, active.id(), candidate.id()])
                .await
                .unwrap(),
            vec![
                Some(TaggedPhysicalStatus::Archived),
                None,
                Some(TaggedPhysicalStatus::Active),
                Some(TaggedPhysicalStatus::Archived),
            ]
        );
        assert!(store.get_node_statuses(&[]).await.unwrap().is_empty());

        let repeated = vec![active.id(); MAX_NODE_STATUS_BATCH];
        assert_eq!(
            store.get_node_statuses(&repeated).await.unwrap(),
            vec![Some(TaggedPhysicalStatus::Active); MAX_NODE_STATUS_BATCH]
        );
        let oversized = vec![active.id(); MAX_NODE_STATUS_BATCH + 1];
        assert!(matches!(
            store.get_node_statuses(&oversized).await,
            Err(Error::CapacityExceeded {
                resource: "node status batch",
                limit: MAX_NODE_STATUS_BATCH,
            })
        ));
    }

    fn feedback_idempotency(
        key: &str,
        payload: &str,
        epoch: &str,
        sequence: u64,
        floor: u64,
    ) -> FeedbackIdempotency {
        let digest = payload
            .bytes()
            .fold(0u64, |hash, byte| hash.wrapping_mul(257) ^ u64::from(byte));
        FeedbackIdempotency::new(
            key,
            format!("{digest:064x}"),
            FeedbackRetryScope::new(epoch, sequence, floor).unwrap(),
        )
        .unwrap()
    }

    fn managed_reference_store(dim: usize, epoch: u64) -> MemStore {
        MemStore::new_ephemeral_managed_reference(
            dim,
            DatabaseId::new(Ulid::from(70_001u128)).unwrap(),
            StorageIdentity::new(
                mneme_core::managed::StorageId::new(Ulid::from(70_002u128)).unwrap(),
                mneme_core::managed::StorageGeneration::new(3).unwrap(),
            ),
            ManagedSchemaVersion::new(4).unwrap(),
            WriterSchemaVersion::new(20).unwrap(),
            MutationEpoch::new(epoch).unwrap(),
        )
    }

    fn managed_epoch(store: &MemStore) -> u64 {
        store
            .observed_managed_head()
            .mutation_epoch()
            .expect("managed reference fixture")
            .get()
    }

    #[test]
    fn managed_reference_head_export_and_save_boundaries_are_explicit() {
        let store = managed_reference_store(4, 7);
        let expected = ManagedStoreHead::managed(
            DatabaseId::new(Ulid::from(70_001u128)).unwrap(),
            StorageIdentity::new(
                mneme_core::managed::StorageId::new(Ulid::from(70_002u128)).unwrap(),
                mneme_core::managed::StorageGeneration::new(3).unwrap(),
            ),
            ManagedSchemaVersion::new(4).unwrap(),
            WriterSchemaVersion::new(20).unwrap(),
            MutationEpoch::new(7).unwrap(),
        );
        assert_eq!(store.observed_managed_head(), expected);

        let export = store.export();
        assert_eq!(managed_epoch(&store), 7, "export is read-only");
        let wire = serde_json::to_value(&export).unwrap();
        for forbidden in [
            "storage",
            "storage_id",
            "storage_generation",
            "managed_schema_version",
            "minimum_writer_schema",
            "mutation_epoch",
        ] {
            assert!(
                wire.get(forbidden).is_none(),
                "legacy export leaked {forbidden}"
            );
        }

        let replacement = MemStore::from_export(export).unwrap();
        assert_eq!(replacement.db_id(), store.db_id());
        assert_eq!(
            replacement.observed_managed_head(),
            ManagedStoreHead::unmanaged(DatabaseId::new(store.db_id()).unwrap()),
            "semantic migration retains logical identity but is not exact managed restore"
        );

        let path = std::env::temp_dir().join(format!("mneme-managed-ref-{}.json", Ulid::new()));
        assert!(matches!(store.save(&path), Err(Error::Conflict(_))));
        assert!(matches!(
            store.save_single_graph_v2(&path),
            Err(Error::Conflict(_))
        ));
        assert!(!path.exists());
        assert_eq!(store.observed_managed_head(), expected);

        let conventional = MemStore::new(4);
        assert_eq!(
            conventional.observed_managed_head(),
            ManagedStoreHead::unmanaged(DatabaseId::new(conventional.db_id()).unwrap())
        );
    }

    #[tokio::test]
    async fn managed_reference_simple_writer_matrix_counts_only_final_changes() {
        let store = managed_reference_store(4, 0);
        let fingerprint = EmbeddingFingerprint::new("managed:test", 4, "l2-f32-v1", "symmetric-v1");
        assert_eq!(
            store.ensure_embedding_fingerprint(&fingerprint).unwrap(),
            EmbeddingFingerprintInit::InitializedEmptyStore
        );
        assert_eq!(managed_epoch(&store), 1);
        assert_eq!(
            store.ensure_embedding_fingerprint(&fingerprint).unwrap(),
            EmbeddingFingerprintInit::Existing
        );
        store.set_embedding_fingerprint(&fingerprint).unwrap();
        assert!(matches!(
            store.set_embedding_fingerprint(&EmbeddingFingerprint::new(
                "wrong-dim",
                3,
                "l2-f32-v1",
                "symmetric-v1"
            )),
            Err(Error::DimMismatch { .. })
        ));
        assert_eq!(managed_epoch(&store), 1);

        let a = NodeId(Ulid::from(71_001u128));
        let b = NodeId(Ulid::from(71_002u128));
        let node_a = tagged_test_node(a, &["managed"], NodeStatus::Active);
        let node_b = tagged_test_node(b, &["managed"], NodeStatus::Active);
        store.put_node(&node_a).await.unwrap();
        assert_eq!(managed_epoch(&store), 2);
        store.put_node(&node_a).await.unwrap();
        store.set_status(a, NodeStatus::Active).await.unwrap();
        assert_eq!(
            managed_epoch(&store),
            2,
            "exact node/status writes are no-ops"
        );
        store.set_status(a, NodeStatus::Archived).await.unwrap();
        assert_eq!(managed_epoch(&store), 3);
        store.set_status(a, NodeStatus::Archived).await.unwrap();
        store.put_node(&node_b).await.unwrap();
        assert_eq!(managed_epoch(&store), 4);

        let edge = Edge::new(a, b, 0.4, EdgeKind::Associative, 1);
        store.put_edge(&edge).await.unwrap();
        assert_eq!(managed_epoch(&store), 5);
        store.put_edge(&edge).await.unwrap();
        assert_eq!(managed_epoch(&store), 5);
        let replacement_edge = Edge::new(a, b, 0.6, EdgeKind::Associative, 2);
        store.put_edge(&replacement_edge).await.unwrap();
        assert_eq!(managed_epoch(&store), 6);
        store.delete_edge(a, b).await.unwrap();
        assert_eq!(managed_epoch(&store), 7);
        store.delete_edge(a, b).await.unwrap();
        assert_eq!(managed_epoch(&store), 7);

        store.upsert(a, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        assert_eq!(managed_epoch(&store), 8);
        store.upsert(a, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        assert_eq!(managed_epoch(&store), 8);
        store.upsert(a, &[0.0, 1.0, 0.0, 0.0]).await.unwrap();
        assert_eq!(managed_epoch(&store), 9);
        assert!(matches!(
            store.upsert(a, &[0.0, 0.0, 0.0, 0.0]).await,
            Err(Error::InvalidInput(_))
        ));
        store.remove(a).await.unwrap();
        assert_eq!(managed_epoch(&store), 10);
        store.remove(a).await.unwrap();
        assert_eq!(managed_epoch(&store), 10);

        let target_db = Ulid::from(71_100u128);
        let remote = RemoteEdge::new(a, target_db, b, 0.25);
        store.put_remote_edge(&remote).await.unwrap();
        assert_eq!(managed_epoch(&store), 11);
        store.put_remote_edge(&remote).await.unwrap();
        assert_eq!(managed_epoch(&store), 11);
        let replacement_remote = RemoteEdge::new(a, target_db, b, 0.75);
        store.put_remote_edge(&replacement_remote).await.unwrap();
        assert_eq!(managed_epoch(&store), 12);
        store.delete_remote_edge(a, target_db, b).await.unwrap();
        assert_eq!(managed_epoch(&store), 13);
        store.delete_remote_edge(a, target_db, b).await.unwrap();
        assert_eq!(managed_epoch(&store), 13);

        let pair = UnorderedPair(a, b);
        store.observe_contradiction(a, b, 10).await.unwrap();
        store.observe_contradiction(a, b, 10).await.unwrap();
        assert_eq!(managed_epoch(&store), 15);
        store
            .resolve_contradiction(pair, Resolution::Unresolved)
            .await
            .unwrap();
        assert_eq!(managed_epoch(&store), 16);
        store
            .resolve_contradiction(pair, Resolution::Unresolved)
            .await
            .unwrap();
        assert_eq!(managed_epoch(&store), 16);
        store
            .resolve_contradiction(pair, Resolution::Superseded)
            .await
            .unwrap();
        assert_eq!(managed_epoch(&store), 17);
        store
            .resolve_contradiction(pair, Resolution::Superseded)
            .await
            .unwrap();
        assert!(matches!(
            store
                .resolve_contradiction(pair, Resolution::ContextDependent)
                .await,
            Err(Error::Conflict(_))
        ));
        assert_eq!(managed_epoch(&store), 17);

        store.observe_merge_candidate(a, b, 20).await.unwrap();
        store.observe_merge_candidate(a, b, 20).await.unwrap();
        assert_eq!(managed_epoch(&store), 19);
        store
            .resolve_merge_candidate(pair, MergeResolution::Keep)
            .await
            .unwrap();
        assert_eq!(managed_epoch(&store), 20);
        store
            .resolve_merge_candidate(pair, MergeResolution::Keep)
            .await
            .unwrap();
        assert!(matches!(
            store
                .resolve_merge_candidate(pair, MergeResolution::Full)
                .await,
            Err(Error::Conflict(_))
        ));
        assert_eq!(managed_epoch(&store), 20);

        // Test-only corruption fixture: deleting a missing canonical node still
        // changes durable reference state when orphan vector/open overlays exist.
        let orphan = NodeId(Ulid::from(71_003u128));
        {
            let mut g = store.lock();
            g.vectors.insert(orphan, vec![1.0, 0.0, 0.0, 0.0]);
            g.contradictions
                .insert(UnorderedPair(orphan, b), Contradiction::new(orphan, b, 30));
            g.merges
                .insert(UnorderedPair(orphan, b), MergeCandidate::new(orphan, b, 30));
        }
        store.delete_node(orphan).await.unwrap();
        assert_eq!(managed_epoch(&store), 21);
        store.delete_node(orphan).await.unwrap();
        store.delete_node(a).await.unwrap();
        assert_eq!(managed_epoch(&store), 22);
        store.delete_node(a).await.unwrap();
        assert_eq!(managed_epoch(&store), 22);
    }

    #[tokio::test]
    async fn managed_general_writers_count_and_repair_derived_index_changes() {
        let store = managed_reference_store(2, 0);
        let a = NodeId(Ulid::from(73_010u128));
        let b = NodeId(Ulid::from(73_011u128));
        let node_a = tagged_test_node(a, &["repair"], NodeStatus::Active);
        let node_b = tagged_test_node(b, &["repair"], NodeStatus::Active);
        store.put_node(&node_a).await.unwrap();
        store.put_node(&node_b).await.unwrap();

        {
            let mut g = store.lock();
            g.tag_projection
                .active
                .get_mut("repair")
                .unwrap()
                .remove(&(stable_tag_sample_hash(a), a));
        }
        let before_node_repair = managed_epoch(&store);
        store.put_node(&node_a).await.unwrap();
        assert_eq!(managed_epoch(&store), before_node_repair + 1);
        assert!(projection_contains(
            &store.lock().tag_projection,
            TaggedPhysicalStatus::Active,
            "repair",
            a
        ));

        let edge = Edge::new(a, b, 0.5, EdgeKind::Associative, 1);
        store.put_edge(&edge).await.unwrap();
        {
            store
                .lock()
                .edge_keys_by_endpoint
                .get_mut(&b)
                .unwrap()
                .remove(&(a, b));
        }
        let before_edge_repair = managed_epoch(&store);
        store.put_edge(&edge).await.unwrap();
        assert_eq!(managed_epoch(&store), before_edge_repair + 1);
        assert!(
            store
                .lock()
                .edge_keys_by_endpoint
                .get(&b)
                .unwrap()
                .contains(&(a, b))
        );

        let target_db = Ulid::from(73_100u128);
        let remote = RemoteEdge::new(a, target_db, b, 0.5);
        store.put_remote_edge(&remote).await.unwrap();
        store
            .lock()
            .remote_edge_keys_by_source
            .get_mut(&a)
            .unwrap()
            .clear();
        let before_remote_repair = managed_epoch(&store);
        store.put_remote_edge(&remote).await.unwrap();
        assert_eq!(managed_epoch(&store), before_remote_repair + 1);

        // The expected ordering row alone is not sufficient: a stale row with
        // different weight bits but the same canonical identity must be counted
        // as durable corruption, repaired, and charged once.
        store
            .lock()
            .remote_edge_keys_by_source
            .get_mut(&a)
            .unwrap()
            .insert((Reverse(0.25_f32.to_bits()), target_db, b));
        let before_duplicate_repair = managed_epoch(&store);
        store.put_remote_edge(&remote).await.unwrap();
        assert_eq!(managed_epoch(&store), before_duplicate_repair + 1);
        {
            let g = store.lock();
            assert!(g.remote_edge_index_matches_exactly(&remote));
            assert_eq!(
                g.remote_edge_keys_by_source
                    .get(&a)
                    .unwrap()
                    .iter()
                    .filter(|(_, indexed_db, indexed_target)| {
                        *indexed_db == target_db && *indexed_target == b
                    })
                    .count(),
                1
            );
        }
        store.put_remote_edge(&remote).await.unwrap();
        assert_eq!(managed_epoch(&store), before_duplicate_repair + 1);

        // Orphan the ordering row without changing its source bucket, then prove
        // the public delete counts and repairs that derived durable state.
        store.lock().remote_edges.remove(&(a, target_db, b));
        let before_orphan_delete = managed_epoch(&store);
        store.delete_remote_edge(a, target_db, b).await.unwrap();
        assert_eq!(managed_epoch(&store), before_orphan_delete + 1);
        assert!(
            store
                .lock()
                .remote_edge_keys_by_source
                .get(&a)
                .is_none_or(BTreeSet::is_empty)
        );

        // Capacity rejection is decided before either canonical/index state or
        // the managed epoch changes.
        let missing_index = RemoteEdge::new(a, target_db, NodeId(Ulid::from(73_900u128)), 0.5);
        {
            let mut g = store.lock();
            for ordinal in 0..MAX_REMOTE_EDGES_PER_SOURCE as u128 {
                g.put_remote_edge_prevalidated(RemoteEdge::new(
                    a,
                    target_db,
                    NodeId(Ulid::from(73_200u128 + ordinal)),
                    0.5,
                ));
            }
            g.remote_edges.insert(
                (
                    missing_index.from,
                    missing_index.target_db,
                    missing_index.target,
                ),
                missing_index.clone(),
            );
        }
        let before_capacity_head = store.observed_managed_head();
        let before_capacity_export = serde_json::to_value(store.export()).unwrap();
        assert!(matches!(
            store.put_remote_edge(&missing_index).await,
            Err(Error::CapacityExceeded { .. })
        ));
        let overflow = RemoteEdge::new(a, target_db, NodeId(Ulid::from(74_000u128)), 0.5);
        assert!(matches!(
            store.put_remote_edge(&overflow).await,
            Err(Error::CapacityExceeded { .. })
        ));
        assert_eq!(store.observed_managed_head(), before_capacity_head);
        assert_eq!(
            serde_json::to_value(store.export()).unwrap(),
            before_capacity_export
        );
    }

    #[tokio::test]
    async fn managed_epoch_overflow_refuses_compound_without_any_visible_prefix() {
        let store = managed_reference_store(4, mneme_core::managed::MAX_MANAGED_STORAGE_INTEGER);
        let id = NodeId(Ulid::from(74_001u128));
        let expected = cap_test_node(id, NodeStatus::Active);
        {
            // Detached test fixture: seed the pre-existing max-epoch generation
            // without exercising a public writer that must correctly refuse it.
            store.lock().replace_node(expected.clone());
        }
        let mut replacement = expected.clone();
        replacement.record_grounded_use(2);
        let commit = FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                "overflow-proof",
                "overflow-payload",
                "overflow-authority",
                1,
                1,
            )),
            applied_at: 2,
            nodes: vec![FeedbackNodeUpdate {
                expected: expected.clone(),
                replacement,
            }],
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };
        let before_head = store.observed_managed_head();
        let before_export = serde_json::to_value(store.export()).unwrap();
        assert!(matches!(
            store.commit_feedback(&commit).await,
            Err(Error::Conflict(message)) if message.contains("epoch")
        ));
        assert_eq!(store.observed_managed_head(), before_head);
        assert_eq!(serde_json::to_value(store.export()).unwrap(), before_export);

        // Semantic no-ops do not need a successor and therefore remain legal at
        // the maximum representable epoch.
        store.put_node(&expected).await.unwrap();
        let empty = FeedbackCommit {
            idempotency: None,
            applied_at: 3,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };
        assert_eq!(
            store.commit_feedback(&empty).await.unwrap(),
            FeedbackCommitOutcome::Applied
        );
        assert_eq!(store.observed_managed_head(), before_head);
        assert_eq!(serde_json::to_value(store.export()).unwrap(), before_export);
    }

    #[tokio::test]
    async fn managed_saturated_overlay_observations_use_final_value_changedness() {
        let store = managed_reference_store(2, 0);
        let a = NodeId(Ulid::from(74_010u128));
        let b = NodeId(Ulid::from(74_011u128));
        let pair = UnorderedPair(a, b);
        {
            let mut contradiction = Contradiction::new(a, b, 5);
            contradiction.observations = u32::MAX;
            let mut merge = MergeCandidate::new(a, b, 5);
            merge.observations = u32::MAX;
            let mut g = store.lock();
            g.nodes.insert(a, cap_test_node(a, NodeStatus::Active));
            g.nodes.insert(b, cap_test_node(b, NodeStatus::Active));
            g.rebuild_tag_projection();
            g.contradictions.insert(pair, contradiction);
            g.merges.insert(pair, merge);
        }

        store.observe_contradiction(a, b, 5).await.unwrap();
        store.observe_merge_candidate(a, b, 5).await.unwrap();
        assert_eq!(
            managed_epoch(&store),
            0,
            "saturated same-time values are exact no-ops"
        );

        store.observe_contradiction(a, b, 6).await.unwrap();
        store.observe_merge_candidate(a, b, 6).await.unwrap();
        assert_eq!(
            managed_epoch(&store),
            2,
            "last_seen changes still advance each operation"
        );
    }

    #[tokio::test]
    async fn managed_reference_rejects_invalid_overlay_and_remote_writes_without_advancing() {
        let store = managed_reference_store(2, 0);
        let a = NodeId(Ulid::from(74_020u128));
        let b = NodeId(Ulid::from(74_021u128));
        let missing = NodeId(Ulid::from(74_022u128));
        store
            .put_node(&cap_test_node(a, NodeStatus::Active))
            .await
            .unwrap();
        store
            .put_node(&cap_test_node(b, NodeStatus::Active))
            .await
            .unwrap();
        let before = managed_epoch(&store);

        for result in [
            store.observe_contradiction(a, a, 1).await,
            store.observe_contradiction(a, missing, 1).await,
            store
                .observe_contradiction(a, b, i64::MAX as Timestamp + 1)
                .await,
            store.observe_merge_candidate(a, a, 1).await,
            store.observe_merge_candidate(a, missing, 1).await,
            store
                .observe_merge_candidate(a, b, i64::MAX as Timestamp + 1)
                .await,
            store
                .put_remote_edge(&RemoteEdge::new(a, Ulid::nil(), b, 0.5))
                .await,
            store
                .put_remote_edge(&RemoteEdge::new(a, store.db_id(), b, 0.5))
                .await,
        ] {
            assert!(matches!(result, Err(Error::InvalidInput(_))));
            assert_eq!(managed_epoch(&store), before);
        }
        assert!(store.export().contradictions.is_empty());
        assert!(store.export().merges.is_empty());
        assert!(store.export().remote_edges.is_empty());
    }

    #[tokio::test]
    async fn open_overlays_accept_nil_node_ids_but_terminal_history_may_dangle() {
        let store = managed_reference_store(2, 0);
        let nil = NodeId(Ulid::nil());
        let peer = NodeId(Ulid::from(74_030u128));
        store
            .put_node(&cap_test_node(nil, NodeStatus::Active))
            .await
            .unwrap();
        store
            .put_node(&cap_test_node(peer, NodeStatus::Active))
            .await
            .unwrap();

        let pair = UnorderedPair(nil, peer);
        store.observe_contradiction(nil, peer, 1).await.unwrap();
        store.observe_merge_candidate(nil, peer, 1).await.unwrap();
        store
            .resolve_contradiction(pair, Resolution::ContextDependent)
            .await
            .unwrap();
        store
            .resolve_merge_candidate(pair, MergeResolution::Keep)
            .await
            .unwrap();
        store.delete_node(peer).await.unwrap();
        let before = managed_epoch(&store);

        store.observe_contradiction(nil, peer, 2).await.unwrap();
        store.observe_merge_candidate(nil, peer, 2).await.unwrap();
        assert_eq!(managed_epoch(&store), before + 2);
        let export = store.export();
        assert_eq!(export.contradictions[0].last_seen, 2);
        assert_eq!(export.merges[0].last_seen, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn managed_epoch_serializes_concurrent_changes_and_exact_upserts() {
        let store = std::sync::Arc::new(managed_reference_store(2, 0));
        let mut tasks = Vec::new();
        for ordinal in 1..=32u128 {
            let store = std::sync::Arc::clone(&store);
            tasks.push(tokio::spawn(async move {
                let id = NodeId(Ulid::from(75_000u128 + ordinal));
                store.put_node(&cap_test_node(id, NodeStatus::Active)).await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(managed_epoch(&store), 32);
        assert_eq!(store.export().nodes.len(), 32);

        let exact = cap_test_node(NodeId(Ulid::from(75_001u128)), NodeStatus::Active);
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let store = std::sync::Arc::clone(&store);
            let exact = exact.clone();
            tasks.push(tokio::spawn(async move { store.put_node(&exact).await }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(
            managed_epoch(&store),
            32,
            "racing exact upserts bump zero times"
        );
        assert_eq!(store.export().nodes.len(), 32);
    }

    fn neighbor_signatures(neighbors: Vec<Neighbor>) -> Vec<NeighborSignature> {
        neighbors
            .into_iter()
            .map(|neighbor| {
                (
                    neighbor.edge.from,
                    neighbor.edge.to,
                    neighbor.edge.kind,
                    neighbor.edge.weight().to_bits(),
                    neighbor.node,
                    neighbor.incoming,
                )
            })
            .collect()
    }

    async fn collect_remote_edges(
        store: &(impl GraphStore + ?Sized),
        from: NodeId,
    ) -> Vec<RemoteEdge> {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = store
                .remote_edges_page(from, after, MAX_REMOTE_EDGE_PAGE_SIZE)
                .await
                .unwrap();
            out.extend(page.items);
            let Some(next) = page.next else {
                return out;
            };
            after = Some(next);
        }
    }

    async fn assert_incident_index_matches_scan(store: &MemStore, endpoints: &[NodeId]) {
        let expected_neighbors = {
            let g = store.lock();
            let mut expected_index: HashMap<NodeId, HashSet<(NodeId, NodeId)>> = HashMap::new();
            for key in g.edges.keys().copied() {
                expected_index.entry(key.0).or_default().insert(key);
                if key.1 != key.0 {
                    expected_index.entry(key.1).or_default().insert(key);
                }
            }
            assert_eq!(
                g.edge_keys_by_endpoint, expected_index,
                "derived endpoint projection must exactly match canonical edges"
            );

            endpoints
                .iter()
                .copied()
                .map(|id| {
                    let incident: Vec<Edge> = g
                        .edges
                        .values()
                        .filter(|edge| edge.from == id || edge.to == id)
                        .cloned()
                        .collect();
                    let mut expected = oriented_neighbors(id, &incident);
                    expected.sort_by(neighbor_order);
                    (id, neighbor_signatures(expected))
                })
                .collect::<HashMap<_, _>>()
        };

        for &id in endpoints {
            let actual = store.neighbors(id, usize::MAX).await.unwrap();
            assert_eq!(
                neighbor_signatures(actual),
                expected_neighbors[&id],
                "indexed neighbor read diverged from whole-edge scan for {id:?}"
            );
        }
    }

    fn next_random(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn cap_test_node(id: NodeId, status: NodeStatus) -> Node {
        Node::try_new(
            id,
            "node",
            BodyRef::new("inline://x").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            status,
            1,
        )
        .unwrap()
    }

    fn tagged_test_node(id: NodeId, tags: &[&str], status: NodeStatus) -> Node {
        Node::try_new(
            id,
            format!("tagged-{}", id.0),
            BodyRef::new(format!("inline://tagged/{}", id.0)).unwrap(),
            tags.iter().copied(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            status,
            1,
        )
        .unwrap()
    }

    fn tagged_lane(
        lane: RetrievalLifecycleLane,
        status: StatusFilter,
        k: usize,
        fallback_candidates: usize,
    ) -> TaggedAnnLaneRequest {
        TaggedAnnLaneRequest::new(lane, status, k, fallback_candidates).unwrap()
    }

    fn tagged_limits(
        max_raw_memberships: usize,
        max_unique_exact_ids: usize,
        max_exact_vector_components: usize,
        max_fallback_candidates: usize,
        max_fallback_hydration_ids: usize,
        max_fallback_vector_components: usize,
    ) -> TaggedAnnWorkLimits {
        TaggedAnnWorkLimits::new(
            max_raw_memberships,
            max_unique_exact_ids,
            max_exact_vector_components,
            max_unique_exact_ids.min(256),
            max_fallback_candidates,
            max_fallback_hydration_ids,
            max_fallback_vector_components,
        )
        .unwrap()
    }

    fn projection_contains(
        projection: &TagProjection,
        status: TaggedPhysicalStatus,
        tag: &str,
        id: NodeId,
    ) -> bool {
        projection
            .status(status)
            .get(tag)
            .is_some_and(|members| members.contains(&(stable_tag_sample_hash(id), id)))
    }

    #[tokio::test]
    async fn mem_tag_projection_tracks_replacement_repair_status_and_delete() {
        let store = MemStore::new(2);
        let id = NodeId(Ulid::from(40_001u128));
        let first = tagged_test_node(id, &["a", "b"], NodeStatus::Active);
        store.put_node(&first).await.unwrap();
        {
            let projection = &store.lock().tag_projection;
            assert!(projection_contains(
                projection,
                TaggedPhysicalStatus::Active,
                "a",
                id
            ));
            assert!(projection_contains(
                projection,
                TaggedPhysicalStatus::Active,
                "b",
                id
            ));
        }

        // A same-ID write is also a local repair: remove one expected row and
        // prove that replacement reconstructs it without duplicating another.
        {
            let mut inner = store.lock();
            inner
                .tag_projection
                .active
                .get_mut("a")
                .unwrap()
                .remove(&(stable_tag_sample_hash(id), id));
        }
        store.put_node(&first).await.unwrap();
        assert!(projection_contains(
            &store.lock().tag_projection,
            TaggedPhysicalStatus::Active,
            "a",
            id
        ));

        let replacement = tagged_test_node(id, &["b", "c"], NodeStatus::Active);
        store.put_node(&replacement).await.unwrap();
        {
            let projection = &store.lock().tag_projection;
            assert!(!projection_contains(
                projection,
                TaggedPhysicalStatus::Active,
                "a",
                id
            ));
            assert!(!projection.active.contains_key("a"), "empty bucket remains");
            assert!(projection_contains(
                projection,
                TaggedPhysicalStatus::Active,
                "b",
                id
            ));
            assert!(projection_contains(
                projection,
                TaggedPhysicalStatus::Active,
                "c",
                id
            ));
        }

        store
            .put_node(&tagged_test_node(id, &[], NodeStatus::Active))
            .await
            .unwrap();
        assert!(store.lock().tag_projection.active.is_empty());

        store
            .put_node(&tagged_test_node(id, &["x"], NodeStatus::Active))
            .await
            .unwrap();
        store.set_status(id, NodeStatus::Archived).await.unwrap();
        {
            let projection = &store.lock().tag_projection;
            assert!(!projection_contains(
                projection,
                TaggedPhysicalStatus::Active,
                "x",
                id
            ));
            assert!(projection_contains(
                projection,
                TaggedPhysicalStatus::Archived,
                "x",
                id
            ));
        }
        store.delete_node(id).await.unwrap();
        let projection = &store.lock().tag_projection;
        assert!(projection.active.is_empty());
        assert!(projection.archived.is_empty());
    }

    #[test]
    fn mem_load_rebuilds_final_last_write_wins_tag_projection_once() {
        let id = NodeId(Ulid::from(40_030u128));
        let mut export = MemStore::new(2).export();
        export
            .nodes
            .push(tagged_test_node(id, &["stale"], NodeStatus::Active));
        export
            .nodes
            .push(tagged_test_node(id, &["final"], NodeStatus::Archived));
        let encoded = serde_json::to_value(&export).unwrap();
        assert!(encoded.get("tag_projection").is_none());

        let path = std::env::temp_dir().join(format!("mneme-tag-load-{}.json", Ulid::new()));
        std::fs::write(
            &path,
            serde_json::to_vec(&StoreExportEnvelopeV5::new(export)).unwrap(),
        )
        .unwrap();
        let loaded = MemStore::load(&path).unwrap();
        let _ = std::fs::remove_file(path);
        let inner = loaded.lock();
        assert_eq!(inner.nodes.len(), 1);
        assert!(!projection_contains(
            &inner.tag_projection,
            TaggedPhysicalStatus::Active,
            "stale",
            id
        ));
        assert!(!inner.tag_projection.active.contains_key("stale"));
        assert!(projection_contains(
            &inner.tag_projection,
            TaggedPhysicalStatus::Archived,
            "final",
            id
        ));
    }

    #[tokio::test]
    async fn mem_tagged_exact_top_k_matches_full_sort_with_ties_and_zero_k() {
        let store = MemStore::new(2);
        let query = [1.0, 0.0];
        let mut all_scores = Vec::new();
        for index in 0..12 {
            let id = NodeId(Ulid::from(41_100u128 + index as u128));
            let node = tagged_test_node(id, &["top-k"], NodeStatus::Active);
            let vector = if index < 6 {
                // Six exact ties force deterministic ID-ascending eviction.
                [1.0, 0.0]
            } else {
                [1.0, index as f32]
            };
            store.put_node(&node).await.unwrap();
            store.upsert(id, &vector).await.unwrap();
            all_scores.push(Scored {
                id,
                score: cosine(&query, &vector).clamp(-1.0, 1.0),
            });
        }
        all_scores.sort_by(scored_order);
        let expected = all_scores[..3].to_vec();
        let lanes = [tagged_lane(
            RetrievalLifecycleLane::Primary,
            StatusFilter::ACTIVE,
            3,
            3,
        )];
        let batch = store
            .tagged_ann(
                TaggedAnnRequest::new(&query, ["top-k"], &lanes, TaggedAnnWorkLimits::default())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(batch.lanes[0].hits, expected);
        assert_eq!(
            batch.lanes[0]
                .hits
                .iter()
                .map(|hit| hit.id)
                .collect::<Vec<_>>(),
            (0..3)
                .map(|offset| NodeId(Ulid::from(41_100u128 + offset)))
                .collect::<Vec<_>>()
        );
        assert_eq!(batch.work.exact_hydrated_ids, 12);

        let zero_lane = [tagged_lane(
            RetrievalLifecycleLane::Primary,
            StatusFilter::ACTIVE,
            0,
            0,
        )];
        let zero = store
            .tagged_ann(
                TaggedAnnRequest::new(
                    &query,
                    ["top-k"],
                    &zero_lane,
                    TaggedAnnWorkLimits::default(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(zero.lanes[0].hits.is_empty());
        assert_eq!(zero.work.exact_hydrated_ids, 12);
        assert_eq!(zero.work.exact_vector_components, 24);
    }

    #[tokio::test]
    async fn mem_tagged_exact_fuses_are_staged_raw_then_unique_then_components() {
        let store = MemStore::new(4);
        let first = NodeId(Ulid::from(41_010u128));
        let second = NodeId(Ulid::from(41_011u128));
        for id in [first, second] {
            let node = tagged_test_node(id, &["a", "b"], NodeStatus::Active);
            store.put_node(&node).await.unwrap();
            store.upsert(id, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        }
        let lanes = [tagged_lane(
            RetrievalLifecycleLane::Primary,
            StatusFilter::ACTIVE,
            1,
            0,
        )];
        let query = [1.0, 0.0, 0.0, 0.0];

        let raw = store
            .tagged_ann(
                TaggedAnnRequest::new(&query, ["a", "b"], &lanes, tagged_limits(3, 1, 16, 1, 1, 4))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(raw.work.raw_memberships, 4);
        assert_eq!(raw.work.unique_exact_ids, 2);
        assert_eq!(raw.work.exact_hydrated_ids, 0);
        assert!(matches!(
            raw.lanes[0].seed_coverage,
            TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter {
                exceeded_limit: TaggedExactWorkLimit::RawMemberships,
                ..
            }
        ));

        let unique = store
            .tagged_ann(
                TaggedAnnRequest::new(
                    &query,
                    ["a", "b"],
                    &lanes,
                    tagged_limits(10, 1, 16, 1, 1, 4),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unique.work.raw_memberships, 4);
        assert_eq!(unique.work.unique_exact_ids, 2);
        assert!(matches!(
            unique.lanes[0].seed_coverage,
            TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter {
                exceeded_limit: TaggedExactWorkLimit::UniqueExactIds,
                ..
            }
        ));

        let components = store
            .tagged_ann(
                TaggedAnnRequest::new(
                    &query,
                    ["a"],
                    &[tagged_lane(
                        RetrievalLifecycleLane::Primary,
                        StatusFilter::ACTIVE,
                        1,
                        2,
                    )],
                    tagged_limits(10, 10, 7, 2, 2, 8),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(components.work.raw_memberships, 2);
        assert_eq!(components.work.unique_exact_ids, 2);
        assert_eq!(components.work.exact_hydrated_ids, 0);
        assert_eq!(components.work.exact_vector_components, 0);
        assert!(matches!(
            components.lanes[0].seed_coverage,
            TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter {
                exceeded_limit: TaggedExactWorkLimit::ExactVectorComponents,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn mem_tagged_default_boundary_is_exact_at_4096_and_hashed_at_4097() {
        let store = MemStore::new(2);
        let mut ids = Vec::with_capacity(MAX_TAGGED_RAW_MEMBERSHIPS + 1);
        {
            let mut inner = store.lock();
            for offset in 0..MAX_TAGGED_RAW_MEMBERSHIPS {
                let id = NodeId(Ulid::from(50_000u128 + offset as u128));
                ids.push(id);
                inner.replace_node(tagged_test_node(id, &["popular"], NodeStatus::Active));
                inner.vectors.insert(id, vec![1.0, offset as f32 + 1.0]);
            }
        }
        let lanes = [tagged_lane(
            RetrievalLifecycleLane::Primary,
            StatusFilter::ACTIVE,
            1,
            1,
        )];
        let query = [1.0, 0.0];
        let exact = store
            .tagged_ann(
                TaggedAnnRequest::new(&query, ["popular"], &lanes, TaggedAnnWorkLimits::default())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(exact.work.raw_memberships, MAX_TAGGED_RAW_MEMBERSHIPS);
        assert_eq!(exact.work.unique_exact_ids, MAX_TAGGED_RAW_MEMBERSHIPS);
        assert_eq!(exact.work.exact_hydrated_ids, MAX_TAGGED_RAW_MEMBERSHIPS);
        assert_eq!(
            exact.lanes[0].seed_coverage,
            TaggedSeedCoverage::ExactCosine
        );

        let final_id = NodeId(Ulid::from(50_000u128 + MAX_TAGGED_RAW_MEMBERSHIPS as u128));
        ids.push(final_id);
        store
            .put_node(&tagged_test_node(
                final_id,
                &["popular"],
                NodeStatus::Active,
            ))
            .await
            .unwrap();
        store.upsert(final_id, &[1.0, 1.0]).await.unwrap();

        let request =
            TaggedAnnRequest::new(&query, ["popular"], &lanes, TaggedAnnWorkLimits::default())
                .unwrap();
        let pivot = tagged_sample_pivot(
            &request,
            TaggedPhysicalStatus::Active,
            RetrievalLifecycleLane::Primary,
            <MemStore as VectorIndex>::semantic_id(&store),
        )
        .unwrap();
        let members: Vec<TagSampleKey> = {
            let inner = store.lock();
            inner.tag_projection.active["popular"]
                .iter()
                .copied()
                .collect()
        };
        let sample_at = |pivot| {
            let start = (pivot, NodeId(Ulid::from(0u128)));
            members
                .get(members.partition_point(|member| *member < start))
                .or_else(|| members.first())
                .unwrap()
                .1
        };
        let expected_sample = sample_at(pivot);

        let fallback = store.tagged_ann(request.clone()).await.unwrap();
        let repeated = store.tagged_ann(request).await.unwrap();
        assert_eq!(
            fallback, repeated,
            "same snapshot/request must sample identically"
        );
        assert_eq!(
            fallback.work.raw_memberships,
            MAX_TAGGED_RAW_MEMBERSHIPS + 1
        );
        assert_eq!(
            fallback.work.unique_exact_ids,
            MAX_TAGGED_RAW_MEMBERSHIPS + 1
        );
        assert_eq!(fallback.work.exact_hydrated_ids, 0);
        assert_eq!(fallback.work.fallback_sample_inspected, 1);
        assert_eq!(fallback.lanes[0].hits[0].id, expected_sample);
        assert!(matches!(
            fallback.lanes[0].seed_coverage,
            TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter {
                exceeded_limit: TaggedExactWorkLimit::RawMemberships,
                ..
            }
        ));

        // Exercise many independently digested requests against monotonically
        // increasing ULIDs. An implementation that samples by ID age, or wraps
        // most pivots to the oldest row, collapses this cohort into the first
        // quartile instead of covering the hash-ordered population.
        let mut cohort_samples = BTreeSet::new();
        let mut age_quartiles = [0usize; 4];
        for nonce in 0..128 {
            let cohort_query = [1.0, (nonce as f32 - 63.5) / 16.0];
            let cohort_request = TaggedAnnRequest::new(
                &cohort_query,
                ["popular"],
                &lanes,
                TaggedAnnWorkLimits::default(),
            )
            .unwrap();
            let cohort_pivot = tagged_sample_pivot(
                &cohort_request,
                TaggedPhysicalStatus::Active,
                RetrievalLifecycleLane::Primary,
                <MemStore as VectorIndex>::semantic_id(&store),
            )
            .unwrap();
            let expected = sample_at(cohort_pivot);
            let cohort = store.tagged_ann(cohort_request).await.unwrap();
            assert_eq!(cohort.lanes[0].hits[0].id, expected);
            cohort_samples.insert(expected);
            let age_ordinal = ids.binary_search(&expected).unwrap();
            age_quartiles[(age_ordinal * 4 / ids.len()).min(3)] += 1;
        }
        assert!(
            cohort_samples.len() >= 64,
            "hash sampling collapsed to only {} of 128 cohort selections",
            cohort_samples.len()
        );
        assert!(
            age_quartiles.iter().all(|count| *count >= 16),
            "hash samples are systematically age-skewed: {age_quartiles:?}"
        );
    }

    #[tokio::test]
    async fn mem_tagged_vectorless_and_unknown_tags_are_exact_and_read_only() {
        let store = MemStore::new(2);
        let vectorless = NodeId(Ulid::from(61_000u128));
        store
            .put_node(&tagged_test_node(
                vectorless,
                &["known"],
                NodeStatus::Active,
            ))
            .await
            .unwrap();
        let lanes = [tagged_lane(
            RetrievalLifecycleLane::Primary,
            StatusFilter::ACTIVE,
            4,
            4,
        )];
        let query = [1.0, 0.0];
        let vectorless_batch = store
            .tagged_ann(
                TaggedAnnRequest::new(&query, ["known"], &lanes, TaggedAnnWorkLimits::default())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(vectorless_batch.work.raw_memberships, 1);
        assert_eq!(vectorless_batch.work.unique_exact_ids, 1);
        assert_eq!(vectorless_batch.work.exact_hydrated_ids, 0);
        assert!(vectorless_batch.lanes[0].hits.is_empty());
        assert_eq!(
            vectorless_batch.lanes[0].seed_coverage,
            TaggedSeedCoverage::ExactCosine
        );

        let before = store.lock().tag_projection.clone();
        let unknown = store
            .tagged_ann(
                TaggedAnnRequest::new(
                    &query,
                    ["does-not-exist"],
                    &lanes,
                    TaggedAnnWorkLimits::default(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.work.raw_memberships, 0);
        assert_eq!(unknown.work.unique_exact_ids, 0);
        assert!(unknown.lanes[0].hits.is_empty());
        assert_eq!(store.lock().tag_projection, before);
    }

    #[tokio::test]
    async fn mem_tagged_overflow_never_hydrates_or_scores_the_exact_prefix() {
        let store = MemStore::new(2);
        for offset in 0..2 {
            let id = NodeId(Ulid::from(61_010u128 + offset));
            store
                .put_node(&tagged_test_node(id, &["overflow"], NodeStatus::Active))
                .await
                .unwrap();
            // This payload would fail immediately if the biased exact prefix
            // were validated, hydrated, or scored before fallback selection.
            store.lock().vectors.insert(id, vec![f32::NAN, 0.0]);
        }
        let lanes = [tagged_lane(
            RetrievalLifecycleLane::Primary,
            StatusFilter::ACTIVE,
            1,
            0,
        )];
        let query = [1.0, 0.0];
        let batch = store
            .tagged_ann(
                TaggedAnnRequest::new(
                    &query,
                    ["overflow"],
                    &lanes,
                    tagged_limits(1, 4, 8, 1, 1, 2),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(batch.work.raw_memberships, 2);
        assert_eq!(batch.work.exact_hydrated_ids, 0);
        assert_eq!(batch.work.fallback_sample_inspected, 0);
        assert_eq!(batch.work.fallback_hydrated_ids, 0);
        assert!(batch.lanes[0].hits.is_empty());
    }

    #[tokio::test]
    async fn mem_tagged_rejects_malformed_requests_and_stored_vectors() {
        let lanes = [tagged_lane(
            RetrievalLifecycleLane::Primary,
            StatusFilter::ACTIVE,
            1,
            1,
        )];
        assert!(matches!(
            TaggedAnnRequest::new(&[0.0, 0.0], ["tag"], &lanes, TaggedAnnWorkLimits::default()),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            TaggedAnnRequest::new(
                &[f32::NAN, 0.0],
                ["tag"],
                &lanes,
                TaggedAnnWorkLimits::default()
            ),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            TaggedAnnRequest::new(&[1.0, 0.0], [""], &lanes, TaggedAnnWorkLimits::default()),
            Err(Error::InvalidInput(_))
        ));

        let store = MemStore::new(2);
        let valid_short_query = [1.0];
        let dimension_mismatch = TaggedAnnRequest::new(
            &valid_short_query,
            ["tag"],
            &lanes,
            TaggedAnnWorkLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            store.tagged_ann(dimension_mismatch).await,
            Err(Error::DimMismatch {
                index: 2,
                provider: 1
            })
        ));

        let id = NodeId(Ulid::from(61_020u128));
        store
            .put_node(&tagged_test_node(id, &["tag"], NodeStatus::Active))
            .await
            .unwrap();
        let query = [1.0, 0.0];
        store.lock().vectors.insert(id, vec![1.0]);
        let request = || {
            TaggedAnnRequest::new(&query, ["tag"], &lanes, TaggedAnnWorkLimits::default()).unwrap()
        };
        assert!(matches!(
            store.tagged_ann(request()).await,
            Err(Error::DimMismatch {
                index: 2,
                provider: 1
            })
        ));
        store.lock().vectors.insert(id, vec![f32::INFINITY, 0.0]);
        assert!(matches!(
            store.tagged_ann(request()).await,
            Err(Error::InvalidInput(_))
        ));
        store.lock().vectors.insert(id, vec![0.0, 0.0]);
        assert!(matches!(
            store.tagged_ann(request()).await,
            Err(Error::InvalidInput(_))
        ));
    }

    /// A child retains the route that actually propagated its activation, even
    /// when its predecessor acquires a better route in the same traversal layer.
    pub(crate) async fn assert_spread_provenance<S>(store: &S)
    where
        S: GraphStore + Traversal,
    {
        let [a, b, c, d, e] = [201u128, 202, 203, 204, 205].map(|id| NodeId(Ulid::from(id)));
        for id in [a, b, c, d, e] {
            store
                .put_node(&cap_test_node(id, NodeStatus::Active))
                .await
                .unwrap();
        }
        for edge in [
            Edge::new(b, a, 0.4, EdgeKind::Associative, 1),
            Edge::new(a, c, 0.9, EdgeKind::Transition, 1),
            Edge::new(c, b, 0.9, EdgeKind::Transition, 1),
            Edge::new(b, d, 0.7, EdgeKind::Transition, 1),
            Edge::new(d, e, 0.5, EdgeKind::Transition, 1),
        ] {
            store.put_edge(&edge).await.unwrap();
        }
        let seeds = [Scored { id: a, score: 1.0 }];
        let scope = TraversalScope::new(StatusFilter::ACTIVE);
        let budget = Budget {
            max_nodes: 10,
            max_depth: 3,
            min_relevance: 0.0,
            ..Budget::default()
        };
        let plain = store.spread(&seeds, budget, None, scope).await.unwrap();
        let observed = store
            .spread_with_provenance(&seeds, budget, None, scope)
            .await
            .unwrap();
        assert_eq!(
            plain, observed.hits,
            "tracing must not change the score/order"
        );
        let paths = observed.paths.expect("adapter supports provenance");
        let nodes = |id| {
            paths[&id]
                .iter()
                .map(|hop| (hop.previous, hop.target))
                .collect::<Vec<_>>()
        };
        assert_eq!(nodes(b), vec![(a, c), (c, b)]);
        assert_eq!(nodes(d), vec![(a, c), (c, b), (b, d)]);
        assert_eq!(
            nodes(e),
            vec![(a, b), (b, d), (d, e)],
            "do not splice in a later predecessor route"
        );
        let first = &paths[&e][0];
        assert_eq!(
            (first.edge.from, first.edge.to),
            (b, a),
            "incoming hop preserves stored arrow"
        );
        assert!(!paths.contains_key(&a));
        assert!(paths.len() < budget.max_nodes);
        assert!(
            paths
                .values()
                .all(|path| path.len() <= usize::from(budget.max_depth))
        );

        // Stored snapshots are not reread/reconstructed when a later edit lands.
        store
            .put_edge(&Edge::new(b, a, 0.1, EdgeKind::Transition, 2))
            .await
            .unwrap();
        assert_eq!(first.edge.kind, EdgeKind::Associative);
        assert_eq!(first.edge.weight(), 0.4);

        // Equal alternatives retain the first deterministic source-order winner.
        store
            .put_edge(&Edge::new(b, a, 0.9, EdgeKind::Associative, 3))
            .await
            .unwrap();
        store
            .put_edge(&Edge::new(b, d, 0.5, EdgeKind::Transition, 3))
            .await
            .unwrap();
        store
            .put_edge(&Edge::new(c, d, 0.5, EdgeKind::Transition, 3))
            .await
            .unwrap();
        let tied = store
            .spread_with_provenance(&seeds, budget, None, scope)
            .await
            .unwrap();
        let tied_path = &tied.paths.as_ref().unwrap()[&d];
        assert_eq!(
            tied_path
                .iter()
                .map(|hop| (hop.previous, hop.target))
                .collect::<Vec<_>>(),
            vec![(a, b), (b, d)]
        );
        let again = store
            .spread_with_provenance(&seeds, budget, None, scope)
            .await
            .unwrap();
        assert_eq!(tied.hits, again.hits);
        assert_eq!(again.paths.unwrap()[&d][0].target, b);

        let empty = store
            .spread_with_provenance(
                &seeds,
                Budget {
                    max_nodes: 0,
                    ..budget
                },
                None,
                scope,
            )
            .await
            .unwrap();
        assert!(empty.hits.is_empty());
        assert!(empty.paths.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reference_spread_provenance_preserves_actual_winning_paths() {
        assert_spread_provenance(&MemStore::new(4)).await;
    }

    /// Shared conformance suite for the reference and Cozo traversal adapters.
    /// A single helper keeps their cap semantics from drifting apart again.
    pub(crate) async fn assert_spread_node_cap<S>(store: &S)
    where
        S: GraphStore + Traversal,
    {
        let seed_a = NodeId(Ulid::from(101u128));
        let seed_b = NodeId(Ulid::from(102u128));
        let seed_c = NodeId(Ulid::from(103u128));
        let archived_seed = NodeId(Ulid::from(104u128));
        for (id, status) in [
            (seed_a, NodeStatus::Active),
            (seed_b, NodeStatus::Active),
            (seed_c, NodeStatus::Active),
            (archived_seed, NodeStatus::Archived),
        ] {
            store.put_node(&cap_test_node(id, status)).await.unwrap();
        }

        let seed_cap = store
            .spread(
                &[
                    Scored {
                        id: seed_c,
                        score: 0.8,
                    },
                    Scored {
                        id: archived_seed,
                        score: 1.0,
                    },
                    Scored {
                        id: seed_b,
                        score: 0.9,
                    },
                    Scored {
                        id: seed_a,
                        score: 0.9,
                    },
                    Scored {
                        id: seed_c,
                        score: 0.7,
                    },
                ],
                Budget {
                    max_nodes: 2,
                    max_depth: 0,
                    min_relevance: 0.0,
                    explore: 0.0,
                    ..Budget::default()
                },
                None,
                TraversalScope::new(StatusFilter::ACTIVE),
            )
            .await
            .unwrap();
        assert_eq!(
            seed_cap.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            vec![seed_a, seed_b],
            "eligible seeds consume the cap by score then id"
        );

        let zero = store
            .spread(
                &[Scored {
                    id: seed_a,
                    score: 1.0,
                }],
                Budget {
                    max_nodes: 0,
                    max_depth: u8::MAX,
                    ..Budget::default()
                },
                None,
                TraversalScope::new(StatusFilter::ACTIVE),
            )
            .await
            .unwrap();
        assert!(zero.is_empty(), "a zero node budget is a true no-op");

        let hub = NodeId(Ulid::from(200u128));
        store
            .put_node(&cap_test_node(hub, NodeStatus::Active))
            .await
            .unwrap();
        for raw in 201u128..=205 {
            let child = NodeId(Ulid::from(raw));
            store
                .put_node(&cap_test_node(child, NodeStatus::Active))
                .await
                .unwrap();
            store
                .put_edge(&Edge::new(hub, child, 1.0, EdgeKind::Associative, 1))
                .await
                .unwrap();
        }
        let one = store
            .spread(
                &[Scored {
                    id: hub,
                    score: 1.0,
                }],
                Budget {
                    max_nodes: 1,
                    max_depth: 2,
                    min_relevance: 0.0,
                    explore: 0.0,
                    ..Budget::default()
                },
                None,
                TraversalScope::new(StatusFilter::ACTIVE),
            )
            .await
            .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, hub, "the seed itself spends the only slot");

        let source_a = NodeId(Ulid::from(300u128));
        let source_b = NodeId(Ulid::from(301u128));
        let archived_child = NodeId(Ulid::from(399u128));
        let strongest = NodeId(Ulid::from(400u128));
        let tied_low_id = NodeId(Ulid::from(401u128));
        let tied_high_id = NodeId(Ulid::from(402u128));
        let weaker = NodeId(Ulid::from(403u128));
        for (id, status) in [
            (source_a, NodeStatus::Active),
            (source_b, NodeStatus::Active),
            (archived_child, NodeStatus::Archived),
            (strongest, NodeStatus::Active),
            (tied_low_id, NodeStatus::Active),
            (tied_high_id, NodeStatus::Active),
            (weaker, NodeStatus::Active),
        ] {
            store.put_node(&cap_test_node(id, status)).await.unwrap();
        }
        for (from, to, weight) in [
            (source_a, archived_child, 1.0),
            (source_a, strongest, 0.95),
            (source_a, tied_high_id, 0.9),
            (source_b, tied_low_id, 0.9),
            (source_b, weaker, 0.8),
        ] {
            store
                .put_edge(&Edge::new(from, to, weight, EdgeKind::Associative, 1))
                .await
                .unwrap();
        }
        let layered = store
            .spread(
                &[
                    Scored {
                        id: source_b,
                        score: 1.0,
                    },
                    Scored {
                        id: source_a,
                        score: 1.0,
                    },
                ],
                Budget {
                    max_nodes: 4,
                    max_depth: 1,
                    min_relevance: 0.0,
                    explore: 0.0,
                    ..Budget::default()
                },
                None,
                TraversalScope::new(StatusFilter::ACTIVE),
            )
            .await
            .unwrap();
        assert_eq!(layered.len(), 4);
        assert_eq!(
            layered.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            vec![source_a, source_b, strongest, tied_low_id],
            "children from the complete layer compete by score then id"
        );
        assert!(!layered.iter().any(|hit| hit.id == archived_child));
    }

    #[tokio::test]
    async fn reference_spread_enforces_a_true_unique_node_cap() {
        assert_spread_node_cap(&MemStore::new(4)).await;
    }

    #[tokio::test]
    async fn reference_bm25_is_exact_and_lifecycle_scoped() {
        let store = MemStore::new(4);
        let active = NodeId(Ulid::new());
        let archived = NodeId(Ulid::new());
        for (id, summary, status) in [
            (active, "release marker ZXQ771", NodeStatus::Active),
            (archived, "old marker ZXQ771", NodeStatus::Archived),
        ] {
            store
                .put_node(
                    &Node::try_new(
                        id,
                        summary,
                        BodyRef::new("inline://x").unwrap(),
                        std::iter::empty::<&str>(),
                        Provenance::derived_empty(),
                        0.5,
                        0.5,
                        status,
                        1,
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        let active_hits = store
            .search("zxq771", 4, StatusFilter::ACTIVE)
            .await
            .unwrap();
        assert_eq!(
            active_hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            vec![active]
        );
        let archived_hits = store
            .search("ZXQ771", 4, StatusFilter::ARCHIVED)
            .await
            .unwrap();
        assert_eq!(
            archived_hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            vec![archived]
        );
    }

    #[tokio::test]
    async fn snapshot_round_trips() {
        let store = MemStore::new(4);
        let fingerprint =
            EmbeddingFingerprint::new("test:embedder-v1", 4, "l2-f32-v1", "symmetric-v1");
        store.set_embedding_fingerprint(&fingerprint).unwrap();
        let a = NodeId(Ulid::new());
        let b = NodeId(Ulid::new());
        for id in [a, b] {
            let node = Node::try_new(
                id,
                "n",
                BodyRef::new("inline://x").unwrap(),
                ["t"],
                Provenance::derived_empty(),
                0.5,
                0.5,
                NodeStatus::Active,
                1,
            )
            .unwrap();
            store.put_node(&node).await.unwrap();
        }
        store.upsert(a, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        store
            .put_edge(&Edge::new(a, b, 0.4, EdgeKind::Associative, 1))
            .await
            .unwrap();
        store.observe_contradiction(a, b, 1).await.unwrap();
        store.observe_merge_candidate(a, b, 2).await.unwrap();
        let target_db = Ulid::new();
        let remote = RemoteEdge::new(a, target_db, b, 0.7);
        store.put_remote_edge(&remote).await.unwrap();
        let feedback = FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                "snapshot-retry",
                "snapshot-payload",
                "snapshot-epoch",
                1,
                1,
            )),
            applied_at: 3,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };
        store.commit_feedback(&feedback).await.unwrap();

        let path = std::env::temp_dir().join(format!("mneme-snap-{}.json", Ulid::new()));
        store.save(&path).unwrap();
        assert_eq!(
            store.commit_feedback(&feedback).await.unwrap(),
            FeedbackCommitOutcome::AlreadyApplied,
            "saving must not clear the live in-process retry proof"
        );
        let loaded = MemStore::load(&path).unwrap();

        let cold = ColdPath::acquire();
        assert_eq!(loaded.dim(), 4);
        assert_eq!(loaded.db_id(), store.db_id(), "db id survives save/load");
        assert_eq!(
            loaded.embedding_fingerprint().unwrap(),
            Some(fingerprint),
            "embedding identity survives save/load"
        );
        assert_eq!(loaded.all_nodes(cold).await.unwrap().len(), 2);
        assert!(loaded.get_edge(a, b).await.unwrap().is_some());
        assert_eq!(
            loaded.neighbors(a, 8).await.unwrap()[0].node,
            b,
            "snapshot load rebuilds the derived endpoint projection"
        );
        assert_eq!(loaded.open_contradictions(cold).await.unwrap().len(), 1);
        assert_eq!(loaded.open_merge_candidates(cold).await.unwrap().len(), 1);
        assert_eq!(collect_remote_edges(&loaded, a).await, vec![remote]);
        assert!(loaded.export().feedback_retries.is_empty());
        assert_eq!(
            loaded
                .export()
                .vectors
                .into_iter()
                .find(|(id, _)| *id == a)
                .map(|(_, vector)| vector),
            Some(vec![1.0, 0.0, 0.0, 0.0]),
            "vector payload survives exactly"
        );
        let active = mneme_core::ports::StatusFilter::ACTIVE;
        assert_eq!(
            loaded.ann(&[1.0, 0.0, 0.0, 0.0], 1, active).await.unwrap()[0].id,
            a
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600,
                "snapshots contain user data and must not be group/world readable"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn snapshot_decode_rejects_an_oversized_body_reference() {
        let store = MemStore::new(4);
        let mut export = serde_json::to_value(store.export()).unwrap();
        let node = Node::try_new(
            NodeId(Ulid::from(1_u128)),
            "oversized body-ref fixture",
            BodyRef::new("inline://valid").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        export["nodes"] = serde_json::json!([node]);
        export["nodes"][0]["body"] = serde_json::json!(format!(
            "x://{}",
            "a".repeat(mneme_core::MAX_BODY_REF_BYTES - "x://".len() + 1)
        ));
        export["touchstones"] = serde_json::json!([]);
        let path = std::env::temp_dir().join(format!("mneme-oversized-ref-{}.json", Ulid::new()));
        std::fs::write(
            &path,
            serde_json::to_vec(
                &serde_json::json!({"schema": STORE_EXPORT_SCHEMA_V5, "store": export}),
            )
            .unwrap(),
        )
        .unwrap();
        let error = match MemStore::load(&path) {
            Ok(_) => panic!("oversized body reference unexpectedly loaded"),
            Err(error) => error.to_string(),
        };
        let _ = std::fs::remove_file(path);
        assert!(error.contains("body reference is"), "{error}");
        assert!(error.contains("maximum is"), "{error}");
    }

    #[tokio::test]
    async fn validated_import_rejects_orphan_vectors_and_legacy_ann_hides_them() {
        let orphan = NodeId(Ulid::new());
        let mut export = MemStore::new(4).export();
        export.vectors.push((orphan, vec![1.0, 0.0, 0.0, 0.0]));

        assert!(matches!(
            MemStore::from_export(export.clone()),
            Err(Error::InvalidInput(message)) if message.contains("vector without node")
        ));

        // Ordinary legacy snapshot load intentionally remains permissive so a
        // malformed file can be repaired, but retrieval must never surface its
        // orphan as a real memory.
        let loaded = MemStore::from_export_unchecked(export).unwrap();
        assert!(
            loaded
                .ann(&[1.0, 0.0, 0.0, 0.0], 1, StatusFilter::ALL)
                .await
                .unwrap()
                .is_empty()
        );

        #[cfg(feature = "cozo")]
        {
            let mut destination = CozoStore::new(4).unwrap();
            assert!(matches!(
                destination.import_mem(&loaded).await,
                Err(Error::InvalidInput(message)) if message.contains("vector without node")
            ));
            assert!(
                destination
                    .all_nodes(ColdPath::acquire())
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn graph_import_rejects_invalid_edges_overlays_and_remote_databases() {
        fn assert_rejected(export: StoreExport, expected: &str) {
            for result in [
                MemStore::from_export(export.clone()),
                MemStore::from_export_unchecked(export.clone()),
            ] {
                match result {
                    Ok(_) => panic!("invalid graph export unexpectedly loaded"),
                    Err(Error::InvalidInput(message)) => {
                        assert!(message.contains(expected), "{message:?}")
                    }
                    Err(error) => panic!("unexpected import error: {error}"),
                }
            }
        }

        let local_db = Ulid::from(90_000u128);
        let a = NodeId(Ulid::from(90_001u128));
        let b = NodeId(Ulid::from(90_002u128));
        let mut base = MemStore::new(2).export();
        base.db_id = local_db;
        base.nodes.push(cap_test_node(a, NodeStatus::Active));
        base.nodes.push(cap_test_node(b, NodeStatus::Active));

        let mut bad_edge = base.clone();
        bad_edge.edges.push(Edge::from_stored(
            a,
            b,
            EdgeKind::Associative,
            None,
            0.5,
            1,
            0,
            1,
        ));
        assert_rejected(bad_edge, "interference must not exceed trials");

        let mut bad_contradiction = base.clone();
        bad_contradiction.contradictions.push(Contradiction {
            between: UnorderedPair(a, b),
            observations: 0,
            first_seen: 1,
            last_seen: 1,
            resolution: None,
        });
        assert_rejected(bad_contradiction, "observations must be nonzero");

        let mut bad_merge = base.clone();
        bad_merge.merges.push(MergeCandidate {
            between: UnorderedPair(a, b),
            observations: 1,
            first_seen: 2,
            last_seen: 1,
            resolution: None,
        });
        assert_rejected(bad_merge, "first_seen must not exceed last_seen");

        let mut nil_remote_db = base.clone();
        nil_remote_db
            .remote_edges
            .push(RemoteEdge::new(a, Ulid::nil(), b, 0.5));
        assert_rejected(nil_remote_db, "target database must not be nil");

        let mut local_remote_db = base;
        local_remote_db
            .remote_edges
            .push(RemoteEdge::new(a, local_db, b, 0.5));
        assert_rejected(local_remote_db, "must differ from its source database");
    }

    #[test]
    fn graph_import_requires_only_open_overlay_endpoints_and_allows_nil_node_ids() {
        let local_db = Ulid::from(90_100u128);
        let nil = NodeId(Ulid::nil());
        let peer = NodeId(Ulid::from(90_101u128));
        let missing = NodeId(Ulid::from(90_102u128));
        let mut base = MemStore::new(2).export();
        base.db_id = local_db;
        base.nodes.push(cap_test_node(nil, NodeStatus::Active));
        base.nodes.push(cap_test_node(peer, NodeStatus::Active));
        base.contradictions.push(Contradiction::new(nil, peer, 1));
        base.merges.push(MergeCandidate::new(nil, peer, 1));
        assert!(MemStore::from_export(base.clone()).is_ok());
        assert!(MemStore::from_export_unchecked(base.clone()).is_ok());

        let mut dangling = base.clone();
        dangling.contradictions.clear();
        dangling.merges.clear();
        dangling
            .contradictions
            .push(Contradiction::new(peer, missing, 1));
        assert!(matches!(
            MemStore::from_export(dangling.clone()),
            Err(Error::InvalidInput(message)) if message.contains("missing node endpoint")
        ));
        assert!(matches!(
            MemStore::from_export_unchecked(dangling),
            Err(Error::InvalidInput(message)) if message.contains("missing node endpoint")
        ));

        let mut terminal = base;
        terminal.contradictions.clear();
        terminal.merges.clear();
        let mut contradiction = Contradiction::new(peer, missing, 1);
        contradiction.resolve(Resolution::ContextDependent);
        terminal.contradictions.push(contradiction);
        let mut merge = MergeCandidate::new(peer, missing, 1);
        merge.resolve(MergeResolution::Keep);
        terminal.merges.push(merge);
        assert!(MemStore::from_export(terminal.clone()).is_ok());
        assert!(MemStore::from_export_unchecked(terminal).is_ok());
    }

    #[tokio::test]
    async fn legacy_ann_fails_closed_on_malformed_stored_vectors() {
        let id = NodeId(Ulid::new());
        let mut base = MemStore::new(4).export();
        base.nodes.push(
            Node::try_new(
                id,
                "legacy corrupt vector",
                BodyRef::new("inline://legacy-corrupt").unwrap(),
                std::iter::empty::<&str>(),
                Provenance::derived_empty(),
                0.5,
                0.5,
                NodeStatus::Active,
                1,
            )
            .unwrap(),
        );

        let mut wrong_dim = base.clone();
        wrong_dim.vectors.push((id, vec![1.0, 0.0, 0.0]));
        let loaded = MemStore::from_export_unchecked(wrong_dim).unwrap();
        assert!(matches!(
            loaded
                .ann(&[1.0, 0.0, 0.0, 0.0], 1, StatusFilter::ALL)
                .await,
            Err(Error::DimMismatch {
                index: 4,
                provider: 3
            })
        ));

        for vector in [
            vec![0.0; 4],
            vec![f32::MAX; 4],
            vec![f32::NAN, 0.0, 0.0, 0.0],
        ] {
            let mut malformed = base.clone();
            malformed.vectors.push((id, vector));
            let loaded = MemStore::from_export_unchecked(malformed).unwrap();
            assert!(matches!(
                loaded
                    .ann(&[1.0, 0.0, 0.0, 0.0], 1, StatusFilter::ALL)
                    .await,
                Err(Error::InvalidInput(_))
            ));
        }
    }

    #[tokio::test]
    async fn snapshot_preserves_full_merge_retry_proof() {
        let store = MemStore::new(4);
        let winner = NodeId(Ulid::from(9_100u128));
        let loser = NodeId(Ulid::from(9_101u128));
        for id in [winner, loser] {
            let node = Node::try_new(
                id,
                "merge node",
                BodyRef::new("inline://merge").unwrap(),
                std::iter::empty::<&str>(),
                Provenance::derived_empty(),
                0.5,
                0.5,
                NodeStatus::Active,
                1,
            )
            .unwrap();
            store.put_node(&node).await.unwrap();
        }
        store
            .observe_merge_candidate(winner, loser, 2)
            .await
            .unwrap();
        let commit = FullMergeCommit::new(winner, loser, 3).unwrap();
        assert_eq!(
            store.commit_full_merge(&commit).await.unwrap(),
            FullMergeCommitOutcome::Applied
        );

        let path = std::env::temp_dir().join(format!("mneme-merge-snap-{}.json", Ulid::new()));
        store.save(&path).unwrap();
        let loaded = MemStore::load(&path).unwrap();
        assert_eq!(loaded.export().full_merge_commits.len(), 1);
        loaded
            .resolve_merge_candidate(commit.pair(), MergeResolution::Full)
            .await
            .unwrap();
        assert!(matches!(
            loaded
                .resolve_merge_candidate(commit.pair(), MergeResolution::Keep)
                .await,
            Err(Error::Conflict(_))
        ));
        assert_eq!(
            loaded.commit_full_merge(&commit).await.unwrap(),
            FullMergeCommitOutcome::AlreadyApplied,
            "snapshot restore must check the retry proof before the archived loser"
        );
        assert!(matches!(
            loaded
                .commit_full_merge(&FullMergeCommit::new(loser, winner, 4).unwrap())
                .await,
            Err(Error::Conflict(_))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn reference_supersede_is_atomic_directional_and_snapshot_retry_safe() {
        let store = MemStore::new(4);
        let winner = NodeId(Ulid::from(9_150u128));
        let loser = NodeId(Ulid::from(9_151u128));
        store
            .put_node(&cap_test_node(winner, NodeStatus::Active))
            .await
            .unwrap();
        let mut loser_node = cap_test_node(loser, NodeStatus::Active);
        loser_node.try_set_confidence(0.8).unwrap();
        store.put_node(&loser_node).await.unwrap();

        let commit = SupersedeCommit::new(winner, loser, 3).unwrap();
        store.observe_contradiction(winner, loser, 1).await.unwrap();
        store
            .resolve_contradiction(commit.pair(), Resolution::Unresolved)
            .await
            .unwrap();
        let deferred = store
            .open_contradictions(ColdPath::acquire())
            .await
            .unwrap();
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].resolution, Some(Resolution::Unresolved));
        assert_eq!(
            store.commit_supersede(&commit).await.unwrap(),
            SupersedeCommitOutcome::Applied
        );
        let after = store.get_node(loser).await.unwrap().unwrap();
        assert_eq!(after.confidence(), 0.8);
        assert_eq!(after.status(), NodeStatus::Archived);
        assert_eq!(
            store.get_edge(winner, loser).await.unwrap().unwrap().kind,
            EdgeKind::Supersedes
        );
        let export = store.export();
        assert_eq!(export.supersede_commits, vec![commit.record()]);
        let contradiction = export
            .contradictions
            .iter()
            .find(|item| item.between == commit.pair())
            .unwrap();
        assert_eq!(contradiction.observations, 2);
        assert_eq!(contradiction.resolution, Some(Resolution::Superseded));
        assert_eq!(
            store.commit_supersede(&commit).await.unwrap(),
            SupersedeCommitOutcome::AlreadyApplied
        );
        assert_eq!(
            store.get_node(loser).await.unwrap().unwrap().confidence(),
            0.8,
            "same-direction retry preserves authored confidence"
        );
        assert!(matches!(
            store
                .commit_supersede(&SupersedeCommit::new(loser, winner, 4).unwrap())
                .await,
            Err(Error::Conflict(_))
        ));

        // The proof is one historical adjudication event, not a standing
        // invariant. A later legal status/edge change survives a same-direction
        // call with a fresh timestamp.
        store.set_status(loser, NodeStatus::Active).await.unwrap();
        let mut later_edge = Edge::new(winner, loser, 0.25, EdgeKind::Associative, 5);
        later_edge.anchor = Some(mneme_core::BodySpan::new(1, 2));
        store.put_edge(&later_edge).await.unwrap();
        let later = SupersedeCommit::new(winner, loser, 6).unwrap();
        assert_eq!(
            store.commit_supersede(&later).await.unwrap(),
            SupersedeCommitOutcome::AlreadyApplied
        );
        let evolved_loser = store.get_node(loser).await.unwrap().unwrap();
        assert_eq!(evolved_loser.status(), NodeStatus::Active);
        assert_eq!(evolved_loser.confidence(), 0.8);
        let evolved_edge = store.get_edge(winner, loser).await.unwrap().unwrap();
        assert_eq!(evolved_edge.kind, later_edge.kind);
        assert_eq!(evolved_edge.anchor, later_edge.anchor);
        assert_eq!(evolved_edge.weight(), later_edge.weight());
        store
            .resolve_contradiction(commit.pair(), Resolution::Superseded)
            .await
            .unwrap();
        assert!(matches!(
            store
                .resolve_contradiction(commit.pair(), Resolution::ContextDependent)
                .await,
            Err(Error::Conflict(_))
        ));

        // Forgetting removes current graph state but retains terminal history
        // and its direction proof, so a delayed retry remains a no-op.
        store.delete_edge(winner, loser).await.unwrap();
        store.delete_node(loser).await.unwrap();
        let path = std::env::temp_dir().join(format!("mneme-supersede-{}.json", Ulid::new()));
        store.save(&path).unwrap();
        let loaded = MemStore::load(&path).unwrap();
        assert_eq!(loaded.export().supersede_commits, vec![commit.record()]);
        assert_eq!(
            loaded.commit_supersede(&commit).await.unwrap(),
            SupersedeCommitOutcome::AlreadyApplied
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn reference_sealed_node_rejects_nan_and_import_rejects_forged_proof() {
        let winner = NodeId(Ulid::from(9_160u128));
        let loser = NodeId(Ulid::from(9_161u128));
        let mut loser_node = cap_test_node(loser, NodeStatus::Active);
        assert!(loser_node.try_set_confidence(f32::NAN).is_err());
        assert_eq!(loser_node.confidence(), 0.5);
        let commit = SupersedeCommit::new(winner, loser, 3).unwrap();

        let mut forged = MemStore::new(4).export();
        forged.supersede_commits.push(commit.record());
        assert!(matches!(
            MemStore::from_export(forged),
            Err(Error::InvalidInput(message)) if message.contains("matching contradiction")
        ));

        // Old snapshots predate the additive proof field and remain readable.
        let mut legacy = serde_json::to_value(MemStore::new(4).export()).unwrap();
        legacy.as_object_mut().unwrap().remove("supersede_commits");
        let legacy: StoreExport = serde_json::from_value(legacy).unwrap();
        assert!(legacy.supersede_commits.is_empty());
        MemStore::from_export(legacy).unwrap();
    }

    #[tokio::test]
    async fn reference_supersede_capacity_rejection_publishes_no_prefix() {
        let store = MemStore::new(4);
        let winner = NodeId(Ulid::from(9_170u128));
        let loser = NodeId(Ulid::from(9_171u128));
        store
            .put_node(&cap_test_node(winner, NodeStatus::Active))
            .await
            .unwrap();
        let mut loser_node = cap_test_node(loser, NodeStatus::Active);
        loser_node.try_set_confidence(0.8).unwrap();
        store.put_node(&loser_node).await.unwrap();
        for offset in 0..MAX_INCIDENT_EDGES {
            let peer = NodeId(Ulid::from(9_200u128 + offset as u128));
            store
                .put_edge(&Edge::new(winner, peer, 0.5, EdgeKind::Associative, 1))
                .await
                .unwrap();
        }
        store.observe_contradiction(winner, loser, 2).await.unwrap();

        let commit = SupersedeCommit::new(winner, loser, 3).unwrap();
        assert!(matches!(
            store.commit_supersede(&commit).await,
            Err(Error::CapacityExceeded {
                resource: "incident edge degree",
                limit: MAX_INCIDENT_EDGES,
            })
        ));
        assert!(store.get_edge(winner, loser).await.unwrap().is_none());
        let after = store.get_node(loser).await.unwrap().unwrap();
        assert_eq!(after.confidence(), 0.8);
        assert_eq!(after.status(), NodeStatus::Active);
        let export = store.export();
        let contradiction = export
            .contradictions
            .iter()
            .find(|item| item.between == commit.pair())
            .unwrap();
        assert_eq!(contradiction.observations, 1);
        assert!(contradiction.resolution.is_none());
        assert!(export.supersede_commits.is_empty());
    }

    #[tokio::test]
    async fn reference_delete_node_discards_only_open_overlays() {
        let store = MemStore::new(4);
        let forgotten = NodeId(Ulid::from(9_200u128));
        let winner = NodeId(Ulid::from(9_201u128));
        let open_peer = NodeId(Ulid::from(9_202u128));
        let resolved_peer = NodeId(Ulid::from(9_203u128));
        let unrelated_a = NodeId(Ulid::from(9_204u128));
        let unrelated_b = NodeId(Ulid::from(9_205u128));
        for id in [
            forgotten,
            winner,
            open_peer,
            resolved_peer,
            unrelated_a,
            unrelated_b,
        ] {
            store
                .put_node(&cap_test_node(id, NodeStatus::Active))
                .await
                .unwrap();
        }

        let full_merge = FullMergeCommit::new(winner, forgotten, 3).unwrap();
        store
            .observe_merge_candidate(winner, forgotten, 2)
            .await
            .unwrap();
        assert_eq!(
            store.commit_full_merge(&full_merge).await.unwrap(),
            FullMergeCommitOutcome::Applied
        );

        let open_pair = UnorderedPair(forgotten, open_peer);
        store
            .observe_contradiction(forgotten, open_peer, 4)
            .await
            .unwrap();
        store
            .resolve_contradiction(open_pair, Resolution::Unresolved)
            .await
            .unwrap();
        store
            .observe_merge_candidate(forgotten, open_peer, 5)
            .await
            .unwrap();

        let resolved_pair = UnorderedPair(forgotten, resolved_peer);
        store
            .observe_contradiction(forgotten, resolved_peer, 6)
            .await
            .unwrap();
        store
            .resolve_contradiction(resolved_pair, Resolution::ContextDependent)
            .await
            .unwrap();
        store
            .observe_merge_candidate(forgotten, resolved_peer, 7)
            .await
            .unwrap();
        store
            .resolve_merge_candidate(resolved_pair, MergeResolution::Keep)
            .await
            .unwrap();

        let unrelated_pair = UnorderedPair(unrelated_a, unrelated_b);
        store
            .observe_contradiction(unrelated_a, unrelated_b, 8)
            .await
            .unwrap();
        store
            .observe_merge_candidate(unrelated_a, unrelated_b, 9)
            .await
            .unwrap();

        store.delete_node(forgotten).await.unwrap();

        let export = store.export();
        assert!(!export.nodes.iter().any(|node| node.id() == forgotten));
        assert!(
            !export
                .contradictions
                .iter()
                .any(|overlay| overlay.between == open_pair)
        );
        assert!(export.contradictions.iter().any(|overlay| {
            overlay.between == resolved_pair
                && overlay.resolution == Some(Resolution::ContextDependent)
        }));
        assert!(
            export.contradictions.iter().any(|overlay| {
                overlay.between == unrelated_pair && overlay.resolution.is_none()
            })
        );
        assert!(
            !export
                .merges
                .iter()
                .any(|overlay| overlay.between == open_pair)
        );
        assert!(export.merges.iter().any(|overlay| {
            overlay.between == resolved_pair && overlay.resolution == Some(MergeResolution::Keep)
        }));
        assert!(export.merges.iter().any(|overlay| {
            overlay.between == full_merge.pair()
                && overlay.resolution == Some(MergeResolution::Full)
        }));
        assert!(
            export.merges.iter().any(|overlay| {
                overlay.between == unrelated_pair && overlay.resolution.is_none()
            })
        );
        assert_eq!(export.full_merge_commits, vec![full_merge.record()]);

        let cold = ColdPath::acquire();
        assert_eq!(
            store
                .open_contradictions(cold)
                .await
                .unwrap()
                .into_iter()
                .map(|overlay| overlay.between)
                .collect::<HashSet<_>>(),
            HashSet::from([unrelated_pair])
        );
        assert_eq!(
            store
                .open_merge_candidates(cold)
                .await
                .unwrap()
                .into_iter()
                .map(|overlay| overlay.between)
                .collect::<HashSet<_>>(),
            HashSet::from([unrelated_pair])
        );
    }

    #[test]
    fn snapshot_load_drops_even_crafted_feedback_retry_proofs() {
        let mut export = MemStore::new(4).export();
        export.feedback_retries.push(FeedbackRetryRecord {
            key: "forged".into(),
            fingerprint: "not-a-digest".into(),
            epoch: "dead-authority".into(),
            sequence: u64::MAX,
        });

        let loaded = MemStore::from_export(export).unwrap();
        assert!(loaded.export().feedback_retries.is_empty());
    }

    #[test]
    fn full_merge_import_requires_full_candidate_and_archived_retained_loser() {
        let winner = NodeId(Ulid::from(9_200u128));
        let loser = NodeId(Ulid::from(9_201u128));
        let mut export = MemStore::new(4).export();
        export
            .full_merge_commits
            .push(FullMergeRecord::new(winner, loser, 3));
        assert!(matches!(
            MemStore::from_export(export.clone()),
            Err(Error::InvalidInput(_))
        ));

        let mut candidate = MergeCandidate::new(winner, loser, 1);
        candidate.resolve(MergeResolution::Partial);
        export.merges.push(candidate.clone());
        assert!(matches!(
            MemStore::from_export(export.clone()),
            Err(Error::InvalidInput(_))
        ));

        export.merges[0].resolve(MergeResolution::Full);
        export.nodes.push(cap_test_node(loser, NodeStatus::Active));
        assert!(matches!(
            MemStore::from_export(export.clone()),
            Err(Error::InvalidInput(_))
        ));

        export.nodes[0] = cap_test_node(loser, NodeStatus::Archived);
        let mut duplicate = export.clone();
        duplicate.merges.push(export.merges[0].clone());
        assert!(matches!(
            MemStore::from_export(duplicate),
            Err(Error::InvalidInput(_))
        ));
        let mut forgotten_loser = export.clone();
        forgotten_loser.nodes.clear();
        assert!(MemStore::from_export(forgotten_loser).is_ok());

        let mut lingering_local_edge = export.clone();
        lingering_local_edge.nodes.clear();
        lingering_local_edge
            .edges
            .push(Edge::new(loser, winner, 0.5, EdgeKind::Associative, 4));
        assert!(matches!(
            MemStore::from_export(lingering_local_edge),
            Err(Error::InvalidInput(_))
        ));

        let mut lingering_remote_edge = export.clone();
        lingering_remote_edge.nodes.clear();
        lingering_remote_edge.remote_edges.push(RemoteEdge::new(
            loser,
            Ulid::from(9_202u128),
            winner,
            0.5,
        ));
        assert!(matches!(
            MemStore::from_export(lingering_remote_edge),
            Err(Error::InvalidInput(_))
        ));
        let loaded = MemStore::from_export(export).unwrap();
        assert_eq!(loaded.export().full_merge_commits.len(), 1);
    }

    #[tokio::test]
    async fn mem_many_full_merge_proofs_preserve_exact_retry_and_allow_new_pair() {
        const HISTORICAL_PROOFS: usize = 256;
        let store = MemStore::new(4);
        let exact = FullMergeCommit::new(
            NodeId(Ulid::from(9_300u128)),
            NodeId(Ulid::from(9_301u128)),
            1,
        )
        .unwrap();
        {
            let mut inner = store.lock();
            inner
                .full_merge_commits
                .insert(exact.pair(), exact.record());
            for index in 1..HISTORICAL_PROOFS {
                let winner = NodeId(Ulid::from(10_000u128 + index as u128 * 2));
                let loser = NodeId(Ulid::from(10_001u128 + index as u128 * 2));
                let record = FullMergeRecord::new(winner, loser, index as Timestamp);
                inner.full_merge_commits.insert(record.between, record);
            }
            assert_eq!(inner.full_merge_commits.len(), HISTORICAL_PROOFS);
        }
        assert_eq!(
            store.commit_full_merge(&exact).await.unwrap(),
            FullMergeCommitOutcome::AlreadyApplied
        );

        let winner = NodeId(Ulid::from(99_000u128));
        let loser = NodeId(Ulid::from(99_001u128));
        store
            .put_node(&cap_test_node(winner, NodeStatus::Active))
            .await
            .unwrap();
        store
            .put_node(&cap_test_node(loser, NodeStatus::Active))
            .await
            .unwrap();
        store
            .observe_merge_candidate(winner, loser, 2)
            .await
            .unwrap();
        let new_commit = FullMergeCommit::new(winner, loser, 3).unwrap();
        assert_eq!(
            store.commit_full_merge(&new_commit).await.unwrap(),
            FullMergeCommitOutcome::Applied
        );
        assert!(store.get_node(loser).await.unwrap().unwrap().is_archived());
        assert_eq!(store.lock().full_merge_commits.len(), HISTORICAL_PROOFS + 1);
    }

    #[test]
    fn failed_atomic_snapshot_keeps_previous_file_readable() {
        let path = std::env::temp_dir().join(format!("mneme-atomic-{}.json", Ulid::new()));
        let original = MemStore::new(4);
        original.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();

        let replacement = serde_json::to_vec_pretty(&MemStore::new(8).export()).unwrap();
        let error = atomic_replace_with(&path, &replacement, |_| {
            Err(std::io::Error::other("injected pre-rename failure"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("injected pre-rename failure"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(MemStore::load(&path).unwrap().dim(), 4);

        let prefix = format!(".{}.tmp-", path.file_name().unwrap().to_string_lossy());
        assert!(
            std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(std::result::Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(&prefix)),
            "failed writes clean their uniquely named temp file"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn remote_edge_source_cap_is_exact_and_survives_import_rebuild() {
        let store = MemStore::new(4);
        let source = NodeId(Ulid::from(20_000u128));
        let other_source = NodeId(Ulid::from(20_001u128));
        let target_db = Ulid::from(21_000u128);
        let first_target = NodeId(Ulid::from(30_000u128));

        // The composite key counts the same remote node in distinct databases
        // as distinct source-owned edges.
        for offset in 0..MAX_REMOTE_EDGES_PER_SOURCE {
            store
                .put_remote_edge(&RemoteEdge::new(
                    source,
                    Ulid::from(21_000u128 + offset as u128),
                    first_target,
                    0.5,
                ))
                .await
                .unwrap();
        }
        assert_eq!(
            collect_remote_edges(&store, source).await.len(),
            MAX_REMOTE_EDGES_PER_SOURCE
        );

        // An exact-key update consumes no new source-owned slot at the ceiling.
        store
            .put_remote_edge(&RemoteEdge::new(source, target_db, first_target, 0.9))
            .await
            .unwrap();
        assert_eq!(
            collect_remote_edges(&store, source).await.len(),
            MAX_REMOTE_EDGES_PER_SOURCE
        );
        assert_eq!(
            collect_remote_edges(&store, source)
                .await
                .first()
                .unwrap()
                .weight(),
            0.9
        );

        let overflow = RemoteEdge::new(source, Ulid::from(25_000u128), first_target, 0.5);
        assert!(matches!(
            store.put_remote_edge(&overflow).await,
            Err(Error::CapacityExceeded {
                resource: "remote edges per source",
                limit: MAX_REMOTE_EDGES_PER_SOURCE,
            })
        ));

        // Capacity belongs only to the local `from`; neither another source nor
        // unbounded target fan-in consumes this source's slots.
        for offset in 0..MAX_REMOTE_EDGES_PER_SOURCE {
            store
                .put_remote_edge(&RemoteEdge::new(
                    other_source,
                    target_db,
                    NodeId(Ulid::from(50_000u128 + offset as u128)),
                    0.4,
                ))
                .await
                .unwrap();
        }
        let shared_target = NodeId(Ulid::from(60_000u128));
        for offset in 0..=MAX_REMOTE_EDGES_PER_SOURCE {
            store
                .put_remote_edge(&RemoteEdge::new(
                    NodeId(Ulid::from(70_000u128 + offset as u128)),
                    target_db,
                    shared_target,
                    0.3,
                ))
                .await
                .unwrap();
        }

        // Deletion releases exactly one source-owned slot.
        store
            .delete_remote_edge(source, target_db, first_target)
            .await
            .unwrap();
        store.put_remote_edge(&overflow).await.unwrap();

        let rebuilt = MemStore::from_export(store.export()).unwrap();
        assert_eq!(
            collect_remote_edges(&rebuilt, source).await.len(),
            MAX_REMOTE_EDGES_PER_SOURCE
        );

        // Canonical import and ordinary snapshot load both reject legacy data
        // that already violates the source-owned invariant.
        let mut malformed = store.export();
        malformed.remote_edges.push(RemoteEdge::new(
            other_source,
            target_db,
            NodeId(Ulid::from(80_000u128)),
            0.2,
        ));
        assert!(matches!(
            MemStore::from_export(malformed.clone()),
            Err(Error::CapacityExceeded {
                resource: "remote edges per source",
                limit: MAX_REMOTE_EDGES_PER_SOURCE,
            })
        ));
        let path = std::env::temp_dir().join(format!("mneme-remote-overfull-{}.json", Ulid::new()));
        std::fs::write(
            &path,
            serde_json::to_vec(&StoreExportEnvelopeV5::new(malformed)).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            MemStore::load(&path),
            Err(Error::CapacityExceeded {
                resource: "remote edges per source",
                limit: MAX_REMOTE_EDGES_PER_SOURCE,
            })
        ));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn remote_edge_pages_are_keyset_ordered_and_hard_bounded() {
        let store = MemStore::new(4);
        let source = NodeId(Ulid::from(90_000u128));
        let mut expected = Vec::new();
        for offset in 0..130u128 {
            let edge = RemoteEdge::new(
                source,
                Ulid::from(91_000u128 + offset % 3),
                NodeId(Ulid::from(92_000u128 + offset)),
                match offset % 4 {
                    0 => 0.9,
                    1 => 0.5,
                    2 => 0.5,
                    _ => 0.1,
                },
            );
            store.put_remote_edge(&edge).await.unwrap();
            expected.push(edge);
        }
        expected.sort_by(mneme_core::remote_edge_order);

        let mut actual = Vec::new();
        let mut after = None;
        loop {
            let page = store.remote_edges_page(source, after, 7).await.unwrap();
            assert!(page.items.len() <= 7);
            actual.extend(page.items);
            let Some(next) = page.next else {
                break;
            };
            assert_eq!(next.source(), source);
            after = Some(next);
        }
        assert_eq!(actual, expected);

        assert!(matches!(
            store.remote_edges_page(source, None, 0).await,
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            store
                .remote_edges_page(source, None, MAX_REMOTE_EDGE_PAGE_SIZE + 1)
                .await,
            Err(Error::InvalidInput(_))
        ));
        let foreign_cursor = RemoteEdgeCursor::from_edge(&expected[0]);
        assert!(matches!(
            store
                .remote_edges_page(NodeId(Ulid::from(99_999u128)), Some(foreign_cursor), 7)
                .await,
            Err(Error::InvalidInput(_))
        ));

        // Updating an exact identity must remove its old ordered-index entry.
        let moved = RemoteEdge::new(
            source,
            expected.last().unwrap().target_db,
            expected.last().unwrap().target,
            1.0,
        );
        store.put_remote_edge(&moved).await.unwrap();
        let updated = collect_remote_edges(&store, source).await;
        assert_eq!(updated.len(), expected.len());
        assert_eq!(updated[0], moved);

        let corrupt = MemStore::new(4);
        {
            let mut inner = corrupt.lock();
            for offset in 0..=MAX_REMOTE_EDGES_PER_SOURCE as u128 {
                let edge = RemoteEdge::new(
                    source,
                    Ulid::from(93_000u128),
                    NodeId(Ulid::from(94_000u128 + offset)),
                    0.2,
                );
                inner
                    .remote_edges
                    .insert((edge.from, edge.target_db, edge.target), edge.clone());
                inner
                    .remote_edge_keys_by_source
                    .entry(source)
                    .or_default()
                    .insert(remote_edge_order_key(&edge));
            }
        }
        assert!(matches!(
            corrupt.remote_edges_page(source, None, 7).await,
            Err(Error::CapacityExceeded {
                resource: "remote edges per source",
                limit: MAX_REMOTE_EDGES_PER_SOURCE,
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_mem_remote_edge_inserts_cannot_oversubscribe_a_source() {
        let store = std::sync::Arc::new(MemStore::new(4));
        let source = NodeId(Ulid::from(90_000u128));
        let target_db = Ulid::from(90_001u128);
        let spare = 8;
        for offset in 0..(MAX_REMOTE_EDGES_PER_SOURCE - spare) {
            store
                .put_remote_edge(&RemoteEdge::new(
                    source,
                    target_db,
                    NodeId(Ulid::from(91_000u128 + offset as u128)),
                    0.5,
                ))
                .await
                .unwrap();
        }

        let mut tasks = Vec::new();
        for offset in 0..(spare * 2) {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                store
                    .put_remote_edge(&RemoteEdge::new(
                        source,
                        target_db,
                        NodeId(Ulid::from(92_000u128 + offset as u128)),
                        0.5,
                    ))
                    .await
            }));
        }
        let mut inserted = 0;
        let mut rejected = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(()) => inserted += 1,
                Err(Error::CapacityExceeded {
                    resource: "remote edges per source",
                    limit: MAX_REMOTE_EDGES_PER_SOURCE,
                }) => rejected += 1,
                Err(error) => panic!("unexpected remote-edge insertion error: {error}"),
            }
        }
        assert_eq!(inserted, spare);
        assert_eq!(rejected, spare);
        assert_eq!(
            collect_remote_edges(store.as_ref(), source).await.len(),
            MAX_REMOTE_EDGES_PER_SOURCE
        );
    }

    #[tokio::test]
    async fn incident_index_survives_updates_deletes_and_import_rebuilds() {
        let ids: Vec<NodeId> = (1u128..=12).map(|raw| NodeId(Ulid::from(raw))).collect();
        let mut store = MemStore::new(4);
        for &id in &ids {
            store
                .put_node(&cap_test_node(id, NodeStatus::Active))
                .await
                .unwrap();
        }

        // Pin the awkward cases before the mutation fuzz: a self-loop must be
        // indexed once, and opposite associative rows retain lazy reverse-pair
        // suppression exactly as the scan implementation did.
        store
            .put_edge(&Edge::new(ids[0], ids[0], 0.9, EdgeKind::Associative, 1))
            .await
            .unwrap();
        store
            .put_edge(&Edge::new(ids[0], ids[1], 0.8, EdgeKind::Associative, 1))
            .await
            .unwrap();
        store
            .put_edge(&Edge::new(ids[1], ids[0], 0.7, EdgeKind::Associative, 1))
            .await
            .unwrap();
        assert_incident_index_matches_scan(&store, &ids).await;
        assert_eq!(
            store.neighbors(ids[0], usize::MAX).await.unwrap().len(),
            2,
            "self-loop appears once and the reverse associative row is suppressed"
        );
        store.delete_node(ids[0]).await.unwrap();
        assert_incident_index_matches_scan(&store, &ids).await;
        assert_eq!(
            store.neighbors(ids[0], usize::MAX).await.unwrap().len(),
            2,
            "low-level node deletion leaves the independent edge relation indexed"
        );
        store
            .put_node(&cap_test_node(ids[0], NodeStatus::Active))
            .await
            .unwrap();

        let mut random = 0x6d6e_656d_652d_6564u64;
        for step in 0..384u64 {
            let draw = next_random(&mut random);
            let from = ids[(draw as usize) % ids.len()];
            let to = ids[((draw >> 8) as usize) % ids.len()];
            match (draw >> 16) % 8 {
                0..=3 => {
                    let kind = match (draw >> 24) % 5 {
                        0 => EdgeKind::Associative,
                        1 => EdgeKind::Bridge,
                        2 => EdgeKind::Transition,
                        3 => EdgeKind::Supersedes,
                        _ => EdgeKind::DerivedFrom,
                    };
                    let weight = ((draw >> 32) as u32) as f32 / u32::MAX as f32;
                    store
                        .put_edge(&Edge::new(from, to, weight, kind, (step + 2) as Timestamp))
                        .await
                        .unwrap();
                }
                4 => store.delete_edge(from, to).await.unwrap(),
                5 => {
                    // `delete_node` intentionally leaves the independent edge
                    // relation alone. This exercises the engine's ordering:
                    // edge deletes clean the projection, then node delete is inert.
                    let incident: Vec<_> = store
                        .all_edges(ColdPath::acquire())
                        .await
                        .unwrap()
                        .into_iter()
                        .filter(|edge| edge.from == from || edge.to == from)
                        .map(|edge| (edge.from, edge.to))
                        .collect();
                    for (edge_from, edge_to) in incident {
                        store.delete_edge(edge_from, edge_to).await.unwrap();
                    }
                    store.delete_node(from).await.unwrap();
                    store
                        .put_node(&cap_test_node(from, NodeStatus::Active))
                        .await
                        .unwrap();
                }
                6 => {
                    // Export contains only canonical relations; construction
                    // must derive a fresh endpoint projection from them.
                    store = MemStore::from_export(store.export()).unwrap();
                }
                _ => {
                    // Updating a canonical key must not duplicate it in either
                    // endpoint bucket, including self-loops.
                    store
                        .put_edge(&Edge::new(
                            from,
                            to,
                            0.25,
                            EdgeKind::Transition,
                            (step + 2) as Timestamp,
                        ))
                        .await
                        .unwrap();
                    store
                        .put_edge(&Edge::new(
                            from,
                            to,
                            0.75,
                            EdgeKind::Associative,
                            (step + 3) as Timestamp,
                        ))
                        .await
                        .unwrap();
                }
            }
            assert_incident_index_matches_scan(&store, &ids).await;
        }
    }

    #[tokio::test]
    async fn incident_degree_cap_counts_self_loops_once_and_allows_pair_updates() {
        let store = MemStore::new(4);
        let hub = NodeId(Ulid::from(10_000u128));
        let self_loop = Edge::new(hub, hub, 0.2, EdgeKind::Associative, 1);
        store.put_edge(&self_loop).await.unwrap();

        // One self-loop plus MAX-1 distinct children reaches the boundary.
        for offset in 1..MAX_INCIDENT_EDGES {
            let child = NodeId(Ulid::from(10_000u128 + offset as u128));
            store
                .put_edge(&Edge::new(hub, child, 0.5, EdgeKind::Associative, 1))
                .await
                .unwrap();
        }
        assert_eq!(
            store.neighbors(hub, usize::MAX).await.unwrap().len(),
            MAX_INCIDENT_EDGES
        );

        let overflow = Edge::new(
            hub,
            NodeId(Ulid::from(20_000u128)),
            0.5,
            EdgeKind::Associative,
            1,
        );
        assert!(matches!(
            store.put_edge(&overflow).await,
            Err(Error::CapacityExceeded {
                resource: "incident edge degree",
                limit: MAX_INCIDENT_EDGES,
            })
        ));
        let incoming_overflow = Edge::new(
            NodeId(Ulid::from(20_001u128)),
            hub,
            0.5,
            EdgeKind::Associative,
            1,
        );
        assert!(matches!(
            store.put_edge(&incoming_overflow).await,
            Err(Error::CapacityExceeded {
                resource: "incident edge degree",
                limit: MAX_INCIDENT_EDGES,
            })
        ));

        // Updating an existing directed pair consumes no new capacity, even at
        // the ceiling. The derived endpoint projection must remain unchanged.
        store
            .put_edge(&Edge::new(hub, hub, 0.9, EdgeKind::Transition, 2))
            .await
            .unwrap();
        assert_eq!(
            store.neighbors(hub, usize::MAX).await.unwrap().len(),
            MAX_INCIDENT_EDGES
        );

        let last_child = NodeId(Ulid::from(10_000u128 + (MAX_INCIDENT_EDGES - 1) as u128));
        store.delete_edge(hub, last_child).await.unwrap();
        store.put_edge(&overflow).await.unwrap();
        assert_eq!(
            store.neighbors(hub, usize::MAX).await.unwrap().len(),
            MAX_INCIDENT_EDGES
        );

        // Canonical exports rebuild the exact same bounded projection. Legacy
        // snapshots that already exceed the invariant fail closed on both
        // validated construction and ordinary load.
        let mut malformed = store.export();
        malformed.edges.push(incoming_overflow);
        assert!(matches!(
            MemStore::from_export(malformed.clone()),
            Err(Error::CapacityExceeded {
                resource: "incident edge degree",
                limit: MAX_INCIDENT_EDGES,
            })
        ));
        let path = std::env::temp_dir().join(format!("mneme-overfull-{}.json", Ulid::new()));
        std::fs::write(
            &path,
            serde_json::to_vec(&StoreExportEnvelopeV5::new(malformed)).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            MemStore::load(&path),
            Err(Error::CapacityExceeded {
                resource: "incident edge degree",
                limit: MAX_INCIDENT_EDGES,
            })
        ));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_mem_edge_inserts_cannot_oversubscribe_a_hub() {
        let store = std::sync::Arc::new(MemStore::new(4));
        let hub = NodeId(Ulid::from(30_000u128));
        let spare = 16;
        for offset in 0..(MAX_INCIDENT_EDGES - spare) {
            let child = NodeId(Ulid::from(40_000u128 + offset as u128));
            store
                .put_edge(&Edge::new(hub, child, 0.5, EdgeKind::Associative, 1))
                .await
                .unwrap();
        }

        let mut tasks = Vec::new();
        for offset in 0..(spare * 2) {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                let child = NodeId(Ulid::from(50_000u128 + offset as u128));
                store
                    .put_edge(&Edge::new(hub, child, 0.5, EdgeKind::Associative, 1))
                    .await
            }));
        }
        let mut inserted = 0;
        let mut rejected = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(()) => inserted += 1,
                Err(Error::CapacityExceeded {
                    resource: "incident edge degree",
                    limit: MAX_INCIDENT_EDGES,
                }) => rejected += 1,
                Err(error) => panic!("unexpected edge insertion error: {error}"),
            }
        }
        assert_eq!(inserted, spare);
        assert_eq!(rejected, spare);
        assert_eq!(
            store.neighbors(hub, usize::MAX).await.unwrap().len(),
            MAX_INCIDENT_EDGES
        );
        assert_incident_index_matches_scan(&store, &[hub]).await;
    }

    #[test]
    fn empty_store_initializes_identity() {
        let store = MemStore::new(4);
        let fingerprint =
            EmbeddingFingerprint::new("test:embedder-v1", 4, "l2-f32-v1", "symmetric-v1");
        assert_eq!(
            store.ensure_embedding_fingerprint(&fingerprint).unwrap(),
            mneme_core::ports::EmbeddingFingerprintInit::InitializedEmptyStore
        );
        assert_eq!(store.embedding_fingerprint().unwrap(), Some(fingerprint));
    }

    #[tokio::test]
    async fn populated_legacy_snapshot_is_detected_not_adopted() {
        let store = MemStore::new(4);
        let id = NodeId(Ulid::new());
        store
            .put_node(&cap_test_node(id, NodeStatus::Active))
            .await
            .unwrap();
        store.upsert(id, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        let path = std::env::temp_dir().join(format!("mneme-legacy-{}.json", Ulid::new()));
        store.save(&path).unwrap();

        // Accurately emulate a pre-fingerprint snapshot: the field is absent,
        // rather than merely serialized as null.
        let bytes = std::fs::read(&path).unwrap();
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        json.as_object_mut()
            .unwrap()
            .remove("embedding_fingerprint");
        std::fs::write(&path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();

        let loaded = MemStore::load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let runtime = EmbeddingFingerprint::new("test:embedder-v1", 4, "l2-f32-v1", "symmetric-v1");
        assert!(matches!(
            loaded.ensure_embedding_fingerprint(&runtime),
            Err(Error::LegacyEmbeddingFingerprint)
        ));
        assert_eq!(loaded.embedding_fingerprint().unwrap(), None);
    }

    /// All bounded admission cuts must carry priority without changing activation.
    pub(crate) async fn assert_routing_ordering_winners<S: RoutingProbeStore>(store: &S) {
        let root = NodeId(Ulid::from(91_001u128));
        let target = NodeId(Ulid::from(91_002u128));
        let query = [1.0, 0.0, 0.0, 0.0];
        for id in [root, target] {
            store
                .put_node(&cap_test_node(id, NodeStatus::Active))
                .await
                .unwrap();
            store.upsert(id, &query).await.unwrap();
        }
        let edge = Edge::new(root, target, 0.6, EdgeKind::Transition, 1);
        store.put_edge(&edge).await.unwrap();
        let mut signs = RoutingBiasMap::new();
        signs.insert(
            RoutingRoute {
                previous: root,
                target,
                edge_from: root,
                edge_to: target,
            },
            SignedRoutingBias::Boost,
        );
        let seeds = [Scored {
            id: root,
            score: 1.0,
        }];
        let budget = Budget {
            max_nodes: 8,
            max_depth: 1,
            min_relevance: 0.0,
            query_conditioning: 1.0,
            explore: 0.0,
            ..Budget::default()
        };
        let scope = TraversalScope::new(StatusFilter::ACTIVE);
        for count in 0..32u128 {
            let id = NodeId(Ulid::from(91_100 + count));
            store
                .put_node(&cap_test_node(id, NodeStatus::Active))
                .await
                .unwrap();
            store.upsert(id, &query).await.unwrap();
            store
                .put_edge(&Edge::new(root, id, 0.7, EdgeKind::Transition, 1))
                .await
                .unwrap();
            if count == 7 || count == 31 {
                let base = store
                    .probe_spread(&seeds, budget, Some(&query), scope, None)
                    .await
                    .unwrap();
                let empty = store
                    .probe_spread(
                        &seeds,
                        budget,
                        Some(&query),
                        scope,
                        Some(&RoutingBiasMap::new()),
                    )
                    .await
                    .unwrap();
                assert_eq!(base.hits, empty.hits);
                assert_eq!(format!("{:?}", base.paths), format!("{:?}", empty.paths));
                assert!(empty.ordering.is_none());
                assert!(!base.hits.iter().any(|hit| hit.id == target));
                let boosted = store
                    .probe_spread(&seeds, budget, Some(&query), scope, Some(&signs))
                    .await
                    .unwrap();
                assert_eq!(boosted.hits.len(), 8);
                assert_eq!(
                    boosted
                        .hits
                        .iter()
                        .find(|hit| hit.id == target)
                        .unwrap()
                        .score,
                    0.6
                );
                let winner = &boosted.ordering.as_ref().unwrap()[&target];
                assert_eq!(winner.priority, 0.6 + 0.25);
                assert_eq!(winner.original_contribution, 0.6);
                assert_eq!(winner.path[0].edge.weight(), 0.6);
                assert_eq!(
                    store
                        .get_edge(root, target)
                        .await
                        .unwrap()
                        .unwrap()
                        .weight(),
                    0.6
                );
            }
        }
        let zero_root = NodeId(Ulid::from(93_001u128));
        let zero_target = NodeId(Ulid::from(93_002u128));
        for id in [zero_root, zero_target] {
            store
                .put_node(&cap_test_node(id, NodeStatus::Active))
                .await
                .unwrap();
        }
        store.upsert(zero_root, &query).await.unwrap();
        store
            .upsert(zero_target, &[0.0, 1.0, 0.0, 0.0])
            .await
            .unwrap();
        store
            .put_edge(&Edge::new(
                zero_root,
                zero_target,
                0.9,
                EdgeKind::Transition,
                1,
            ))
            .await
            .unwrap();
        for n in 0..8u128 {
            let id = NodeId(Ulid::from(93_100 + n));
            store
                .put_node(&cap_test_node(id, NodeStatus::Active))
                .await
                .unwrap();
            store.upsert(id, &query).await.unwrap();
            store
                .put_edge(&Edge::new(zero_root, id, 0.1, EdgeKind::Transition, 1))
                .await
                .unwrap();
        }
        let zeros = RoutingBiasMap::from([(
            RoutingRoute {
                previous: zero_root,
                target: zero_target,
                edge_from: zero_root,
                edge_to: zero_target,
            },
            SignedRoutingBias::Boost,
        )]);
        let full = Budget {
            max_nodes: 16,
            ..budget
        };
        let seeded = [Scored {
            id: zero_root,
            score: 1.0,
        }];
        let zero_base = store
            .probe_spread(&seeded, full, Some(&query), scope, None)
            .await
            .unwrap();
        let zero_hint = store
            .probe_spread(&seeded, full, Some(&query), scope, Some(&zeros))
            .await
            .unwrap();
        assert_eq!(
            zero_base.hits, zero_hint.hits,
            "zero conditioning must not steal a real final-eight slot"
        );
        assert_eq!(zero_hint.hits.len(), 9);
        assert!(
            !zero_hint
                .ordering
                .as_ref()
                .unwrap()
                .contains_key(&zero_target)
        );
        // Priority ties at the final-eight cutoff preserve the greater original contribution.
        let [tie_root, tie_a, tie_b] =
            [96_001u128, 96_002, 96_003].map(|id| NodeId(Ulid::from(id)));
        for id in [tie_root, tie_a, tie_b] {
            store
                .put_node(&cap_test_node(id, NodeStatus::Active))
                .await
                .unwrap();
            let vector = if id == tie_a {
                [0.5, 0.75_f32.sqrt(), 0.0, 0.0]
            } else {
                query
            };
            store.upsert(id, &vector).await.unwrap();
        }
        store
            .put_edge(&Edge::new(tie_root, tie_a, 0.7, EdgeKind::Transition, 1))
            .await
            .unwrap();
        store
            .put_edge(&Edge::new(tie_root, tie_b, 0.6, EdgeKind::Transition, 1))
            .await
            .unwrap();
        for n in 0..7u128 {
            let id = NodeId(Ulid::from(96_100 + n));
            store
                .put_node(&cap_test_node(id, NodeStatus::Active))
                .await
                .unwrap();
            store.upsert(id, &query).await.unwrap();
            store
                .put_edge(&Edge::new(tie_root, id, 0.9, EdgeKind::Transition, 1))
                .await
                .unwrap();
        }
        let tie_signs = RoutingBiasMap::from([(
            RoutingRoute {
                previous: tie_root,
                target: tie_a,
                edge_from: tie_root,
                edge_to: tie_a,
            },
            SignedRoutingBias::Boost,
        )]);
        let tied = store
            .probe_spread(
                &[Scored {
                    id: tie_root,
                    score: 1.0,
                }],
                full,
                Some(&query),
                scope,
                Some(&tie_signs),
            )
            .await
            .unwrap();
        assert!(tied.hits.iter().any(|hit| hit.id == tie_b));
        assert!(!tied.hits.iter().any(|hit| hit.id == tie_a));
        // Final-hop priority is max across real arrivals, not a target penalty.
        let [a, b, z, child, archived, zero, below, reverse, zero_weight] = [
            92_001u128, 92_002, 92_003, 92_004, 92_005, 92_006, 92_007, 92_008, 92_009,
        ]
        .map(|id| NodeId(Ulid::from(id)));
        for id in [a, b, z, child, zero, below, reverse, zero_weight] {
            store
                .put_node(&cap_test_node(id, NodeStatus::Active))
                .await
                .unwrap();
            store
                .upsert(
                    id,
                    if id == zero {
                        &[0.0, 1.0, 0.0, 0.0]
                    } else {
                        &query
                    },
                )
                .await
                .unwrap();
        }
        store
            .put_node(&cap_test_node(archived, NodeStatus::Archived))
            .await
            .unwrap();
        store.upsert(archived, &query).await.unwrap();
        let az = Edge::new(a, z, 0.7, EdgeKind::Transition, 1);
        let bz = Edge::new(b, z, 0.6, EdgeKind::Transition, 1);
        for edge in [
            az.clone(),
            bz.clone(),
            Edge::new(z, child, 0.5, EdgeKind::Transition, 1),
            Edge::new(a, archived, 0.9, EdgeKind::Transition, 1),
            Edge::new(a, zero, 0.9, EdgeKind::Transition, 1),
            Edge::new(a, below, 0.05, EdgeKind::Transition, 1),
            Edge::new(a, zero_weight, 0.0, EdgeKind::Transition, 1),
            Edge::new(reverse, a, 0.4, EdgeKind::Associative, 1),
        ] {
            store.put_edge(&edge).await.unwrap();
        }
        let seeds = [Scored { id: a, score: 1.0 }, Scored { id: b, score: 1.0 }];
        let budget = Budget {
            max_nodes: 8,
            max_depth: 2,
            min_relevance: 0.1,
            query_conditioning: 1.0,
            explore: 0.0,
            ..Budget::default()
        };
        let mut signs = RoutingBiasMap::new();
        signs.insert(
            RoutingRoute {
                previous: a,
                target: z,
                edge_from: a,
                edge_to: z,
            },
            SignedRoutingBias::Weaken,
        );
        for target in [archived, zero, below, zero_weight] {
            signs.insert(
                RoutingRoute {
                    previous: a,
                    target,
                    edge_from: a,
                    edge_to: target,
                },
                SignedRoutingBias::Boost,
            );
        }
        // Stored arrow stays reverse while traversal is incoming.
        signs.insert(
            RoutingRoute {
                previous: a,
                target: reverse,
                edge_from: reverse,
                edge_to: a,
            },
            SignedRoutingBias::Boost,
        );
        let observed = store
            .probe_spread(&seeds, budget, Some(&query), scope, Some(&signs))
            .await
            .unwrap();
        assert_eq!(
            observed.hits.iter().find(|hit| hit.id == z).unwrap().score,
            0.7
        );
        assert_eq!(observed.paths.as_ref().unwrap()[&z][0].previous, a);
        let ordering = observed.ordering.as_ref().unwrap();
        assert_eq!(ordering[&z].priority, 0.6);
        assert_eq!(ordering[&z].path[0].previous, b);
        assert_eq!(ordering[&child].original_contribution, 0.7 * 0.5);
        assert_eq!(
            ordering[&child].priority,
            0.7 * 0.5,
            "ancestor weakening is not propagated"
        );
        assert_eq!(ordering[&child].path[0].previous, a);
        assert_eq!(ordering[&reverse].path[0].edge.from, reverse);
        assert_eq!(ordering[&reverse].priority, 0.4 + 0.25);
        for id in [archived, zero, below, zero_weight] {
            assert!(!observed.hits.iter().any(|hit| hit.id == id));
            assert!(!ordering.contains_key(&id));
        }
        signs.insert(
            RoutingRoute {
                previous: b,
                target: z,
                edge_from: b,
                edge_to: z,
            },
            SignedRoutingBias::Boost,
        );
        let opposing = store
            .probe_spread(&seeds, budget, Some(&query), scope, Some(&signs))
            .await
            .unwrap();
        assert_eq!(opposing.ordering.as_ref().unwrap()[&z].priority, 0.6 + 0.25);
        assert_eq!(opposing.ordering.as_ref().unwrap()[&z].path[0].previous, b);
        let unequal = [Scored { id: a, score: 0.5 }, Scored { id: b, score: 1.0 }];
        let different_roots = store
            .probe_spread(&unequal, budget, Some(&query), scope, Some(&signs))
            .await
            .unwrap();
        assert_eq!(
            different_roots.ordering.as_ref().unwrap()[&z].priority,
            0.6 + 0.25
        );
        assert_eq!(
            different_roots
                .hits
                .iter()
                .find(|hit| hit.id == z)
                .unwrap()
                .score,
            0.6
        );
        // Unhinted equal alternatives keep the ordinary deterministic source winner.
        store
            .put_edge(&Edge::new(a, z, 0.6, EdgeKind::Transition, 1))
            .await
            .unwrap();
        let off_route = RoutingBiasMap::from([(
            RoutingRoute {
                previous: a,
                target: archived,
                edge_from: a,
                edge_to: archived,
            },
            SignedRoutingBias::Boost,
        )]);
        let tie = store
            .probe_spread(&seeds, budget, Some(&query), scope, Some(&off_route))
            .await
            .unwrap();
        assert_eq!(tie.ordering.as_ref().unwrap()[&z].path[0].previous, a);
        assert_eq!(tie.paths.as_ref().unwrap()[&z][0].previous, a);
        store.put_edge(&az).await.unwrap();
        // With only an ancestor boost, descendants still keep exact original activation.
        signs.insert(
            RoutingRoute {
                previous: a,
                target: z,
                edge_from: a,
                edge_to: z,
            },
            SignedRoutingBias::Boost,
        );
        signs.remove(&RoutingRoute {
            previous: b,
            target: z,
            edge_from: b,
            edge_to: z,
        });
        let positive = store
            .probe_spread(&seeds, budget, Some(&query), scope, Some(&signs))
            .await
            .unwrap();
        assert_eq!(
            positive.ordering.as_ref().unwrap()[&child].priority,
            0.7 * 0.5
        );
    }

    #[tokio::test]
    async fn mem_routing_ordering_winners_survive_small_caps() {
        assert_routing_ordering_winners(&MemStore::new(4)).await;
    }

    /// Run the same actual traversal experiment against both store adapters.
    /// A fixed query-local sign must reach both cutoff sites, but the admitted
    /// child keeps its authored edge weight/propagation score. Finite slots do
    /// displace another route; this says nothing yet about task utility.
    pub(crate) async fn assert_signed_routing_probe<S: RoutingProbeStore>(store: &S) {
        let [root, other_root, rescued] =
            [80_001u128, 80_002, 80_003].map(|id| NodeId(Ulid::from(id)));
        let distractors: Vec<NodeId> = (80_100u128..80_132)
            .map(|id| NodeId(Ulid::from(id)))
            .collect();
        let query = [1.0, 0.0, 0.0, 0.0];
        let off_topic = [0.0, 1.0, 0.0, 0.0];
        for id in [root, other_root, rescued]
            .into_iter()
            .chain(distractors.iter().copied())
        {
            store
                .put_node(&cap_test_node(id, NodeStatus::Active))
                .await
                .unwrap();
        }
        store.upsert(rescued, &query).await.unwrap();
        for id in &distractors {
            store.upsert(*id, &off_topic).await.unwrap();
            for source in [root, other_root] {
                store
                    .put_edge(&Edge::new(source, *id, 0.7, EdgeKind::Transition, 1))
                    .await
                    .unwrap();
            }
        }
        for source in [root, other_root] {
            store
                .put_edge(&Edge::new(source, rescued, 0.6, EdgeKind::Transition, 1))
                .await
                .unwrap();
        }
        let scope = TraversalScope::new(StatusFilter::ACTIVE);
        let budget = Budget {
            max_nodes: 128,
            max_depth: 1,
            min_relevance: 0.0,
            explore: 0.0,
            query_conditioning: 1.0,
            ..Budget::default()
        };
        let seeds = [Scored {
            id: root,
            score: 1.0,
        }];
        let baseline = store
            .spread_with_provenance(&seeds, budget, Some(&query), scope)
            .await
            .unwrap();
        let no_map = store
            .probe_spread(&seeds, budget, Some(&query), scope, None)
            .await
            .unwrap();
        let empty = RoutingBiasMap::new();
        let empty_map = store
            .probe_spread(&seeds, budget, Some(&query), scope, Some(&empty))
            .await
            .unwrap();
        for actual in [no_map, empty_map] {
            assert_eq!(actual.hits, baseline.hits);
            assert_eq!(
                format!("{:?}", actual.paths),
                format!("{:?}", baseline.paths)
            );
        }
        assert!(!baseline.hits.iter().any(|hit| hit.id == rescued));

        let absent = NodeId(Ulid::from(80_999u128));
        let mut unknown = RoutingBiasMap::new();
        unknown.insert(
            RoutingRoute {
                previous: root,
                target: absent,
                edge_from: root,
                edge_to: absent,
            },
            SignedRoutingBias::Boost,
        );
        let unknown_result = store
            .probe_spread(&seeds, budget, Some(&query), scope, Some(&unknown))
            .await
            .unwrap();
        assert_eq!(unknown_result.hits, baseline.hits);
        assert_eq!(
            format!("{:?}", unknown_result.paths),
            format!("{:?}", baseline.paths)
        );

        let route = RoutingRoute {
            previous: root,
            target: rescued,
            edge_from: root,
            edge_to: rescued,
        };
        let before = serde_json::to_value(store.get_edge(root, rescued).await.unwrap()).unwrap();
        let mut boost = RoutingBiasMap::new();
        boost.insert(route, SignedRoutingBias::Boost);
        let rescued_spread = store
            .probe_spread(&seeds, budget, Some(&query), scope, Some(&boost))
            .await
            .unwrap();
        assert!(rescued_spread.hits.iter().any(|hit| hit.id == rescued));
        assert_eq!(
            rescued_spread
                .hits
                .iter()
                .find(|hit| hit.id == rescued)
                .unwrap()
                .score,
            0.6,
            "ranking bias must not propagate as edge weight"
        );
        let hop = &rescued_spread.paths.as_ref().unwrap()[&rescued][0];
        assert_eq!(
            (hop.previous, hop.target, hop.edge.weight()),
            (root, rescued, 0.6)
        );
        assert_eq!(
            serde_json::to_value(store.get_edge(root, rescued).await.unwrap()).unwrap(),
            before,
            "a routing probe must not mutate the stored edge"
        );
        assert_eq!(
            store
                .probe_spread(&seeds, budget, Some(&query), scope, Some(&boost))
                .await
                .unwrap()
                .hits,
            rescued_spread.hits,
            "the same signed ordering must replay deterministically"
        );

        // A map for one directed route does not become a target-global penalty
        // or ambient context. Both stores see the same target from another root.
        let other_seeds = [Scored {
            id: other_root,
            score: 1.0,
        }];
        assert!(
            !store
                .probe_spread(&other_seeds, budget, Some(&query), scope, Some(&boost))
                .await
                .unwrap()
                .hits
                .iter()
                .any(|hit| hit.id == rescued)
        );
        let mut unrelated = RoutingBiasMap::new();
        unrelated.insert(
            RoutingRoute {
                previous: other_root,
                target: rescued,
                edge_from: other_root,
                edge_to: rescued,
            },
            SignedRoutingBias::Boost,
        );
        assert_eq!(
            store
                .probe_spread(&seeds, budget, Some(&query), scope, Some(&unrelated))
                .await
                .unwrap()
                .hits,
            baseline.hits
        );
        assert_eq!(
            store
                .spread(&seeds, budget, Some(&query), scope)
                .await
                .unwrap(),
            baseline.hits,
            "public traversal remains the unmodified baseline after the probe"
        );

        // A negative sign can reverse reach at the same raw-32 boundary.
        store
            .put_edge(&Edge::new(root, rescued, 0.8, EdgeKind::Transition, 2))
            .await
            .unwrap();
        assert!(
            store
                .spread(&seeds, budget, Some(&query), scope)
                .await
                .unwrap()
                .iter()
                .any(|hit| hit.id == rescued)
        );
        let before_negative =
            serde_json::to_value(store.get_edge(root, rescued).await.unwrap()).unwrap();
        let mut weaken = RoutingBiasMap::new();
        weaken.insert(route, SignedRoutingBias::Weaken);
        assert!(
            !store
                .probe_spread(&seeds, budget, Some(&query), scope, Some(&weaken))
                .await
                .unwrap()
                .hits
                .iter()
                .any(|hit| hit.id == rescued)
        );
        assert_eq!(
            serde_json::to_value(store.get_edge(root, rescued).await.unwrap()).unwrap(),
            before_negative
        );
        store
            .set_status(rescued, NodeStatus::Archived)
            .await
            .unwrap();
        assert!(
            !store
                .probe_spread(&seeds, budget, Some(&query), scope, Some(&boost))
                .await
                .unwrap()
                .hits
                .iter()
                .any(|hit| hit.id == rescued),
            "bias cannot restore a lifecycle-ineligible endpoint"
        );

        // Nine eligible routes all fit the raw 32, so this second fixture
        // isolates the *final eight* boundary rather than rescuing raw rank 33.
        // A half-similar target also distinguishes weight * factor + bias
        // (0.55, admitted) from (weight + bias) * factor (0.425, excluded).
        let [fanout_root, fanout_target] = [80_300u128, 80_301].map(|id| NodeId(Ulid::from(id)));
        let fanout_others: Vec<NodeId> = (80_310u128..80_318)
            .map(|id| NodeId(Ulid::from(id)))
            .collect();
        for id in [fanout_root, fanout_target]
            .into_iter()
            .chain(fanout_others.iter().copied())
        {
            store
                .put_node(&cap_test_node(id, NodeStatus::Active))
                .await
                .unwrap();
            store.upsert(id, &query).await.unwrap();
        }
        let half_similar = [0.5, 0.75_f32.sqrt(), 0.0, 0.0];
        store.upsert(fanout_target, &half_similar).await.unwrap();
        for id in &fanout_others {
            store
                .put_edge(&Edge::new(fanout_root, *id, 0.5, EdgeKind::Transition, 1))
                .await
                .unwrap();
        }
        store
            .put_edge(&Edge::new(
                fanout_root,
                fanout_target,
                0.6,
                EdgeKind::Transition,
                1,
            ))
            .await
            .unwrap();
        let fanout_seeds = [Scored {
            id: fanout_root,
            score: 1.0,
        }];
        let ordinary = store
            .spread(&fanout_seeds, budget, Some(&query), scope)
            .await
            .unwrap();
        assert_eq!(ordinary.len(), 9);
        assert!(!ordinary.iter().any(|hit| hit.id == fanout_target));
        let fanout_route = RoutingRoute {
            previous: fanout_root,
            target: fanout_target,
            edge_from: fanout_root,
            edge_to: fanout_target,
        };
        let mut wrong_edge = RoutingBiasMap::new();
        wrong_edge.insert(
            RoutingRoute {
                edge_from: fanout_target,
                edge_to: fanout_root,
                ..fanout_route
            },
            SignedRoutingBias::Boost,
        );
        let wrong_edge_result = store
            .probe_spread(
                &fanout_seeds,
                budget,
                Some(&query),
                scope,
                Some(&wrong_edge),
            )
            .await
            .unwrap();
        assert_eq!(
            wrong_edge_result.hits, ordinary,
            "matching traversal endpoints alone must not inherit a stored-edge bias"
        );
        let mut fanout_boost = RoutingBiasMap::new();
        fanout_boost.insert(fanout_route, SignedRoutingBias::Boost);
        let gained = store
            .probe_spread(
                &fanout_seeds,
                budget,
                Some(&query),
                scope,
                Some(&fanout_boost),
            )
            .await
            .unwrap();
        assert_eq!(
            gained.hits.len(),
            ordinary.len(),
            "one route displaces another"
        );
        assert!(gained.hits.iter().any(|hit| hit.id == fanout_target));
        let propagated = gained
            .hits
            .iter()
            .find(|hit| hit.id == fanout_target)
            .unwrap()
            .score;
        assert!(
            (propagated - 0.3).abs() < 1e-6,
            "routing bias must not enter conditioned propagation: {propagated}"
        );
        // Keep the separate negative-sign check fully query-similar.
        // Its 0.7 baseline beats 0.5 competitors; weakening to 0.45 loses.
        store.upsert(fanout_target, &query).await.unwrap();
        store
            .put_edge(&Edge::new(
                fanout_root,
                fanout_target,
                0.7,
                EdgeKind::Transition,
                2,
            ))
            .await
            .unwrap();
        assert!(
            store
                .spread(&fanout_seeds, budget, Some(&query), scope)
                .await
                .unwrap()
                .iter()
                .any(|hit| hit.id == fanout_target)
        );
        let mut fanout_weaken = RoutingBiasMap::new();
        fanout_weaken.insert(fanout_route, SignedRoutingBias::Weaken);
        assert!(
            !store
                .probe_spread(
                    &fanout_seeds,
                    budget,
                    Some(&query),
                    scope,
                    Some(&fanout_weaken),
                )
                .await
                .unwrap()
                .hits
                .iter()
                .any(|hit| hit.id == fanout_target)
        );
    }

    #[tokio::test]
    async fn reference_signed_routing_probe_changes_both_native_cutoffs() {
        assert_signed_routing_probe(&MemStore::new(4)).await;
    }

    #[tokio::test]
    async fn reference_spread_conditions_neighbors_before_final_fanout() {
        let store = MemStore::new(4);
        let root = NodeId(Ulid::from(1u128));
        let relevant = NodeId(Ulid::from(2u128));
        let distractors: Vec<NodeId> = (3u128..11).map(|id| NodeId(Ulid::from(id))).collect();
        for id in std::iter::once(root)
            .chain(std::iter::once(relevant))
            .chain(distractors.iter().copied())
        {
            store
                .put_node(
                    &Node::try_new(
                        id,
                        "node",
                        BodyRef::new("inline://x").unwrap(),
                        std::iter::empty::<&str>(),
                        Provenance::derived_empty(),
                        0.5,
                        0.5,
                        NodeStatus::Active,
                        1,
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        store.upsert(relevant, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        store
            .put_edge(&Edge::new(root, relevant, 0.5, EdgeKind::Associative, 1))
            .await
            .unwrap();
        for id in distractors {
            store.upsert(id, &[0.0, 1.0, 0.0, 0.0]).await.unwrap();
            store
                .put_edge(&Edge::new(root, id, 0.9, EdgeKind::Associative, 1))
                .await
                .unwrap();
        }

        let spread = store
            .spread(
                &[Scored {
                    id: root,
                    score: 1.0,
                }],
                Budget {
                    max_nodes: 32,
                    max_depth: 1,
                    min_relevance: 0.0,
                    explore: 0.0,
                    query_conditioning: 1.0,
                    ..Budget::default()
                },
                Some(&[1.0, 0.0, 0.0, 0.0]),
                TraversalScope::new(StatusFilter::ACTIVE),
            )
            .await
            .unwrap();
        assert!(
            spread.iter().any(|hit| hit.id == relevant),
            "the query-relevant ninth raw edge must survive the saturated fanout"
        );
    }

    #[tokio::test]
    async fn mem_feedback_commit_replays_exactly_and_rejects_key_reuse_or_stale_cas() {
        let store = MemStore::new(4);
        let a = NodeId(Ulid::from(1u128));
        let b = NodeId(Ulid::from(2u128));
        for id in [a, b] {
            store
                .put_node(
                    &Node::try_new(
                        id,
                        "feedback node",
                        BodyRef::new("inline://x").unwrap(),
                        std::iter::empty::<&str>(),
                        Provenance::derived_empty(),
                        0.5,
                        0.5,
                        NodeStatus::Active,
                        1,
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        let mut edge = Edge::new(a, b, 0.2, EdgeKind::Associative, 1);
        edge.anchor = Some(mneme_core::BodySpan::new(3, 7));
        store.put_edge(&edge).await.unwrap();

        let original_a = store.get_node(a).await.unwrap().unwrap();
        let mut replacement_a = original_a.clone();
        replacement_a.record_grounded_use(10);
        let mut replacement_edge = edge.clone();
        replacement_edge.reinforce(10, &mneme_core::StrengthParams::default());
        let commit = FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                "receipts:a",
                "payload:a",
                "epoch-a",
                1,
                1,
            )),
            applied_at: 10,
            nodes: vec![FeedbackNodeUpdate {
                expected: original_a.clone(),
                replacement: replacement_a,
            }],
            edges: vec![FeedbackEdgeUpdate {
                expected: Some(edge.clone()),
                replacement: replacement_edge,
            }],
            merge_observations: vec![FeedbackMergeObservation::new(a, b).unwrap()],
        };
        assert_eq!(
            store.commit_feedback(&commit).await.unwrap(),
            FeedbackCommitOutcome::Applied
        );
        let after_first = store.get_node(a).await.unwrap().unwrap();
        let edge_after_first = store.get_edge(a, b).await.unwrap().unwrap();
        assert_eq!(
            store.commit_feedback(&commit).await.unwrap(),
            FeedbackCommitOutcome::AlreadyApplied
        );
        assert_eq!(store.lock().merges[&UnorderedPair(a, b)].observations, 1);
        let unrelated = FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                "receipts:b",
                "payload:b",
                "epoch-a",
                2,
                1,
            )),
            applied_at: i64::MAX as Timestamp,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };
        assert_eq!(
            store.commit_feedback(&unrelated).await.unwrap(),
            FeedbackCommitOutcome::Applied
        );
        let mut after_clock_jump = commit.clone();
        after_clock_jump.applied_at = 0;
        assert_eq!(
            store.commit_feedback(&after_clock_jump).await.unwrap(),
            FeedbackCommitOutcome::AlreadyApplied,
            "unrelated forward/back wall-clock values cannot collect a reachable proof"
        );
        assert!(same_serialized(&after_first, &store.get_node(a).await.unwrap().unwrap()).unwrap());
        assert!(
            same_serialized(
                &edge_after_first,
                &store.get_edge(a, b).await.unwrap().unwrap()
            )
            .unwrap()
        );

        let mut wrong_payload = commit.clone();
        wrong_payload.idempotency.as_mut().unwrap().fingerprint =
            feedback_idempotency("unused", "wrong", "epoch-a", 1, 1).fingerprint;
        assert!(matches!(
            store.commit_feedback(&wrong_payload).await,
            Err(Error::InvalidInput(_))
        ));

        let original_b = store.get_node(b).await.unwrap().unwrap();
        let mut replacement_b = original_b.clone();
        replacement_b.record_grounded_use(11);
        let stale = FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                "receipts:stale",
                "payload:stale",
                "epoch-a",
                2,
                1,
            )),
            applied_at: 11,
            nodes: vec![
                FeedbackNodeUpdate {
                    expected: original_b.clone(),
                    replacement: replacement_b,
                },
                FeedbackNodeUpdate {
                    expected: original_a,
                    replacement: after_first.clone(),
                },
            ],
            edges: Vec::new(),
            merge_observations: vec![FeedbackMergeObservation::new(a, b).unwrap()],
        };
        assert!(matches!(
            store.commit_feedback(&stale).await,
            Err(Error::Conflict(_))
        ));
        assert!(same_serialized(&original_b, &store.get_node(b).await.unwrap().unwrap()).unwrap());
        assert_eq!(
            store.lock().merges[&UnorderedPair(a, b)].observations,
            1,
            "a stale node CAS cannot publish its merge observation"
        );
        assert!(
            !store.lock().feedback_retries.contains_key("receipts:stale"),
            "a failed CAS cannot publish its retry proof"
        );

        store
            .resolve_merge_candidate(UnorderedPair(a, b), MergeResolution::Keep)
            .await
            .unwrap();
        let terminal_observation = FeedbackCommit {
            idempotency: None,
            applied_at: 12,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: vec![FeedbackMergeObservation::new(a, b).unwrap()],
        };
        store.commit_feedback(&terminal_observation).await.unwrap();
        let inner = store.lock();
        let candidate = &inner.merges[&UnorderedPair(a, b)];
        assert_eq!(candidate.observations, 2);
        assert_eq!(candidate.resolution, Some(MergeResolution::Keep));
    }

    #[tokio::test]
    async fn mem_feedback_replay_identity_is_epoch_and_key() {
        let store = MemStore::new(4);
        let commit = |payload, epoch| FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                "receipts:epoch-collision",
                payload,
                epoch,
                1,
                1,
            )),
            applied_at: 1,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };
        let commit_a = commit("same-path-effects", "epoch-a");
        assert_eq!(
            store.commit_feedback(&commit_a).await.unwrap(),
            FeedbackCommitOutcome::Applied
        );

        let commit_b = commit("same-path-effects", "epoch-b");
        assert_eq!(
            store.commit_feedback(&commit_b).await.unwrap(),
            FeedbackCommitOutcome::Applied,
            "epoch A's matching bare key and fingerprint is not epoch B's replay"
        );
        assert_eq!(
            store.commit_feedback(&commit_b).await.unwrap(),
            FeedbackCommitOutcome::AlreadyApplied
        );

        let wrong_payload_b = commit("different-path-effects", "epoch-b");
        assert!(matches!(
            store.commit_feedback(&wrong_payload_b).await,
            Err(Error::InvalidInput(_))
        ));

        let commit_c = commit("different-path-effects", "epoch-c");
        assert_eq!(
            store.commit_feedback(&commit_c).await.unwrap(),
            FeedbackCommitOutcome::Applied,
            "bare-key payload reuse is legal in a different epoch"
        );
        let proof = store.lock().feedback_retries["receipts:epoch-collision"].clone();
        assert_eq!(proof.epoch, "epoch-c");
        assert_eq!(
            proof.fingerprint,
            commit_c.idempotency.as_ref().unwrap().fingerprint
        );
    }

    #[tokio::test]
    async fn mem_cross_epoch_replacement_rolls_back_with_a_failed_graph_preflight() {
        let store = MemStore::new(4);
        let id = NodeId(Ulid::from(74_002u128));
        store
            .put_node(&cap_test_node(id, NodeStatus::Active))
            .await
            .unwrap();

        let commit_a = FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                "receipts:atomic-epoch-replace",
                "payload-a",
                "epoch-a",
                1,
                1,
            )),
            applied_at: 1,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };
        store.commit_feedback(&commit_a).await.unwrap();

        let stale_expected = store.get_node(id).await.unwrap().unwrap();
        let mut current = stale_expected.clone();
        current.record_exposure(2);
        store.put_node(&current).await.unwrap();
        let failed_b = FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                "receipts:atomic-epoch-replace",
                "payload-b",
                "epoch-b",
                1,
                1,
            )),
            applied_at: 2,
            nodes: vec![FeedbackNodeUpdate {
                expected: stale_expected.clone(),
                replacement: stale_expected,
            }],
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };
        assert!(matches!(
            store.commit_feedback(&failed_b).await,
            Err(Error::Conflict(_))
        ));

        let proof = store.lock().feedback_retries["receipts:atomic-epoch-replace"].clone();
        assert_eq!(proof.epoch, "epoch-a");
        assert_eq!(
            proof.fingerprint,
            commit_a.idempotency.as_ref().unwrap().fingerprint
        );
        assert_eq!(
            store.commit_feedback(&commit_a).await.unwrap(),
            FeedbackCommitOutcome::AlreadyApplied,
            "failed epoch-B preflight must retain epoch A's acknowledgement"
        );
        assert_eq!(
            store.get_node(id).await.unwrap().unwrap().exposure_count(),
            1
        );
    }

    #[tokio::test]
    async fn mem_feedback_capacity_rejection_has_no_written_prefix() {
        let store = MemStore::new(4);
        let hub = NodeId(Ulid::from(1u128));
        let node = |id| {
            Node::try_new(
                id,
                "capacity node",
                BodyRef::new("inline://x").unwrap(),
                std::iter::empty::<&str>(),
                Provenance::derived_empty(),
                0.5,
                0.5,
                NodeStatus::Active,
                1,
            )
            .unwrap()
        };
        store.put_node(&node(hub)).await.unwrap();
        for ordinal in 2..=(MAX_INCIDENT_EDGES as u128) {
            let leaf = NodeId(Ulid::from(ordinal));
            store.put_node(&node(leaf)).await.unwrap();
            store
                .put_edge(&Edge::new(hub, leaf, 0.1, EdgeKind::Transition, 1))
                .await
                .unwrap();
        }
        let first = NodeId(Ulid::from(MAX_INCIDENT_EDGES as u128 + 1));
        let second = NodeId(Ulid::from(MAX_INCIDENT_EDGES as u128 + 2));
        store.put_node(&node(first)).await.unwrap();
        store.put_node(&node(second)).await.unwrap();
        let expected_hub = store.get_node(hub).await.unwrap().unwrap();
        let mut replacement_hub = expected_hub.clone();
        replacement_hub.record_grounded_use(10);
        let commit = FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                "receipts:capacity",
                "payload:capacity",
                "capacity-epoch",
                1,
                1,
            )),
            applied_at: 10,
            nodes: vec![FeedbackNodeUpdate {
                expected: expected_hub.clone(),
                replacement: replacement_hub,
            }],
            edges: vec![
                FeedbackEdgeUpdate {
                    expected: None,
                    replacement: Edge::new(hub, first, 0.1, EdgeKind::Transition, 10),
                },
                FeedbackEdgeUpdate {
                    expected: None,
                    replacement: Edge::new(hub, second, 0.1, EdgeKind::Transition, 10),
                },
            ],
            merge_observations: vec![FeedbackMergeObservation::new(hub, first).unwrap()],
        };
        assert!(matches!(
            store.commit_feedback(&commit).await,
            Err(Error::CapacityExceeded { .. })
        ));
        assert!(store.get_edge(hub, first).await.unwrap().is_none());
        assert!(store.get_edge(hub, second).await.unwrap().is_none());
        assert!(!store.lock().merges.contains_key(&UnorderedPair(hub, first)));
        assert!(
            same_serialized(&expected_hub, &store.get_node(hub).await.unwrap().unwrap()).unwrap()
        );
        assert!(
            !store
                .lock()
                .feedback_retries
                .contains_key("receipts:capacity")
        );
    }

    #[tokio::test]
    async fn mem_feedback_ledger_never_evicts_a_live_retry_proof() {
        let store = MemStore::new(4);
        {
            let mut inner = store.lock();
            for index in 0..MAX_FEEDBACK_RETRY_RECORDS {
                let key = format!("live-{index}");
                inner.feedback_retries.insert(
                    key.clone(),
                    FeedbackRetryRecord {
                        key,
                        fingerprint: format!("{index:064x}"),
                        epoch: "epoch".into(),
                        sequence: index as u64 + 1,
                    },
                );
            }
        }
        let full = FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                "new",
                "payload",
                "epoch",
                MAX_FEEDBACK_RETRY_RECORDS as u64 + 1,
                1,
            )),
            applied_at: 100,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };
        assert!(matches!(
            store.commit_feedback(&full).await,
            Err(Error::CapacityExceeded { .. })
        ));
        assert_eq!(
            store.lock().feedback_retries.len(),
            MAX_FEEDBACK_RETRY_RECORDS
        );

        let mut reclaimed = full;
        reclaimed
            .idempotency
            .as_mut()
            .unwrap()
            .retry
            .min_live_sequence = MAX_FEEDBACK_RETRY_RECORDS as u64 + 1;
        assert_eq!(
            store.commit_feedback(&reclaimed).await.unwrap(),
            FeedbackCommitOutcome::Applied
        );
        {
            let inner = store.lock();
            assert_eq!(inner.feedback_retries.len(), 1);
            assert!(inner.feedback_retries.contains_key("new"));
        }

        // The physical map is still keyed by the bare receipt key. Even at the
        // ceiling, a new epoch atomically reclaims old-generation rows before
        // replacing a colliding key; it must not report false capacity.
        {
            let mut inner = store.lock();
            inner.feedback_retries.clear();
            for index in 0..MAX_FEEDBACK_RETRY_RECORDS {
                let key = format!("colliding-{index}");
                inner.feedback_retries.insert(
                    key.clone(),
                    FeedbackRetryRecord {
                        key,
                        fingerprint: format!("{index:064x}"),
                        epoch: "old-epoch".into(),
                        sequence: index as u64 + 1,
                    },
                );
            }
        }
        let cross_epoch_collision = FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                "colliding-0",
                "new-payload",
                "new-epoch",
                1,
                1,
            )),
            applied_at: 101,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };
        assert_eq!(
            store.commit_feedback(&cross_epoch_collision).await.unwrap(),
            FeedbackCommitOutcome::Applied
        );
        let inner = store.lock();
        assert_eq!(inner.feedback_retries.len(), 1);
        assert_eq!(inner.feedback_retries["colliding-0"].epoch, "new-epoch");
    }

    #[tokio::test]
    async fn mem_feedback_reachability_survives_holes_but_not_authority_restart() {
        let store = MemStore::new(4);
        let commit = |key, epoch, sequence, floor, applied_at| FeedbackCommit {
            idempotency: Some(feedback_idempotency(key, key, epoch, sequence, floor)),
            applied_at,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };

        let oldest = commit("oldest", "epoch-a", 1, 1, i64::MAX as Timestamp);
        store.commit_feedback(&oldest).await.unwrap();
        // Sequence holes need no rows. A much later operation may commit before
        // a previously claimed lower sequence, but both carry the pinned floor.
        store
            .commit_feedback(&commit("late", "epoch-a", 100, 1, 0))
            .await
            .unwrap();
        store
            .commit_feedback(&commit("middle", "epoch-a", 2, 1, 42))
            .await
            .unwrap();
        assert_eq!(
            store.commit_feedback(&oldest).await.unwrap(),
            FeedbackCommitOutcome::AlreadyApplied
        );

        let restored = MemStore::from_export(store.export()).unwrap();
        assert_eq!(
            restored.commit_feedback(&oldest).await.unwrap(),
            FeedbackCommitOutcome::Applied,
            "snapshot restore starts a new authority generation"
        );

        // A new server epoch has no surviving receipt capabilities. Its first
        // unique operation may reclaim every prior-epoch proof under the
        // exclusive database lease.
        restored
            .commit_feedback(&commit("restart", "epoch-b", 1, 1, 7))
            .await
            .unwrap();
        let inner = restored.lock();
        assert_eq!(inner.feedback_retries.len(), 1);
        assert!(inner.feedback_retries.contains_key("restart"));
    }

    #[test]
    fn feedback_commit_rejects_static_anchor_self_loop_and_non_finite_changes() {
        let a = NodeId(Ulid::from(1u128));
        let b = NodeId(Ulid::from(2u128));
        let mut expected = Edge::new(a, b, 0.2, EdgeKind::Associative, 1);
        expected.anchor = Some(mneme_core::BodySpan::new(1, 2));
        let mut changed_anchor = expected.clone();
        changed_anchor.anchor = Some(mneme_core::BodySpan::new(2, 3));
        let anchor_commit = FeedbackCommit {
            idempotency: None,
            applied_at: 1,
            nodes: Vec::new(),
            edges: vec![FeedbackEdgeUpdate {
                expected: Some(expected),
                replacement: changed_anchor,
            }],
            merge_observations: Vec::new(),
        };
        assert!(matches!(
            anchor_commit.validate(),
            Err(Error::InvalidInput(_))
        ));

        let self_loop = FeedbackCommit {
            idempotency: None,
            applied_at: 1,
            nodes: Vec::new(),
            edges: vec![FeedbackEdgeUpdate {
                expected: None,
                replacement: Edge::new(a, a, 0.1, EdgeKind::Transition, 1),
            }],
            merge_observations: Vec::new(),
        };
        assert!(matches!(self_loop.validate(), Err(Error::InvalidInput(_))));

        let non_finite = FeedbackCommit {
            idempotency: None,
            applied_at: 1,
            nodes: Vec::new(),
            edges: vec![FeedbackEdgeUpdate {
                expected: None,
                replacement: Edge::from_stored(a, b, EdgeKind::Transition, None, f32::NAN, 1, 1, 0),
            }],
            merge_observations: Vec::new(),
        };
        assert!(matches!(non_finite.validate(), Err(Error::InvalidInput(_))));

        let self_merge = FeedbackCommit {
            idempotency: None,
            applied_at: 1,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: vec![FeedbackMergeObservation {
                between: UnorderedPair(a, a),
            }],
        };
        assert!(matches!(self_merge.validate(), Err(Error::InvalidInput(_))));

        let oversized_merges = FeedbackCommit {
            idempotency: None,
            applied_at: 1,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: vec![
                FeedbackMergeObservation::new(a, b).unwrap();
                MAX_FEEDBACK_BATCH_EVENTS + 1
            ],
        };
        assert!(matches!(
            oversized_merges.validate(),
            Err(Error::CapacityExceeded { .. })
        ));
    }
}

#[cfg(test)]
mod concern_tests {
    use super::*;
    use mneme_core::BodyRef;
    pub(crate) fn node(id: u128, summary: &str) -> Node {
        Node::try_new(
            NodeId(Ulid::from(id)),
            summary,
            BodyRef::new("inline://concern").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap()
    }
    pub(crate) fn notice(a: &Node, b: &Node, kind: ConcernKind) -> ConcernUpdate {
        ConcernUpdate::Notice(
            ConcernNotice::new(
                ConcernBinding::new(
                    kind,
                    ConcernEndpoint::from_node(a),
                    ConcernEndpoint::from_node(b),
                )
                .unwrap(),
                "assertions differ",
                "which deployment?",
            )
            .unwrap(),
        )
    }
    pub(crate) fn finding(expected: ConcernRow, scope: &str) -> ConcernUpdate {
        ConcernUpdate::RecordScopedFinding {
            expected,
            finding: ScopedConcernFinding::new(
                scope,
                "observed here",
                vec![
                    ConcernEvidence::new(
                        "tool://probe",
                        ConcernDigest::of_bytes(b"historical output"),
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        }
    }
    pub(crate) fn resulting(outcome: ConcernCommitOutcome) -> ConcernRow {
        match outcome {
            ConcernCommitOutcome::Applied { row } | ConcernCommitOutcome::Unchanged { row } => row,
            other => panic!("unexpected {other:?}"),
        }
    }
    pub(crate) async fn cas_lifecycle<S: GraphStore + ConcernStore>(store: &S) {
        let a = node(1, "old deployment");
        let b = node(2, "new deployment");
        store.put_node(&a).await.unwrap();
        store.put_node(&b).await.unwrap();
        let update = notice(&a, &b, ConcernKind::Disagreement);
        let key = update.key();
        let initial = resulting(store.update_concern(&update).await.unwrap());
        let f = finding(initial.clone(), "local binary");
        let resolved = resulting(store.update_concern(&f).await.unwrap());
        assert_eq!(
            store.update_concern(&f).await.unwrap(),
            ConcernCommitOutcome::Unchanged {
                row: resolved.clone()
            }
        );
        assert_eq!(
            store.update_concern(&update).await.unwrap(),
            ConcernCommitOutcome::Unchanged {
                row: resolved.clone()
            }
        );
        let conflicting = finding(initial.clone(), "other deployment");
        assert!(matches!(
            store.update_concern(&conflicting).await.unwrap(),
            ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::StaleRow,
                ..
            }
        ));
        let changed = node(2, "actually different canonical meaning");
        store.put_node(&changed).await.unwrap();
        assert_eq!(
            store.get_concern(&key).await.unwrap(),
            Some(resolved.clone())
        );
        assert!(matches!(
            store.update_concern(&f).await.unwrap(),
            ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::StaleMeanings,
                ..
            }
        ));
        let reset = resulting(
            store
                .update_concern(&notice(&a, &changed, ConcernKind::Disagreement))
                .await
                .unwrap(),
        );
        assert!(reset.finding().is_none());
        store
            .set_status(a.id(), NodeStatus::Archived)
            .await
            .unwrap();
        assert!(matches!(
            store
                .update_concern(&notice(&a, &changed, ConcernKind::Disagreement))
                .await
                .unwrap(),
            ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::InactiveEndpoint,
                ..
            }
        ));
        assert_eq!(store.get_concern(&key).await.unwrap(), Some(reset));
        store.delete_node(a.id()).await.unwrap();
        assert!(store.get_concern(&key).await.unwrap().is_none());
        assert!(matches!(
            store.update_concern(&update).await.unwrap(),
            ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::MissingEndpoint,
                ..
            }
        ));
    }
    pub(crate) async fn paging<S: GraphStore + ConcernStore>(store: &S) {
        let center = node(50, "center");
        store.put_node(&center).await.unwrap();
        for id in [10, 20, 60, 70] {
            let other = node(id, "other");
            store.put_node(&other).await.unwrap();
            for kind in [ConcernKind::Disagreement, ConcernKind::Redundancy] {
                store
                    .update_concern(&notice(&center, &other, kind))
                    .await
                    .unwrap();
            }
        }
        let mut cursor = None;
        let mut all = Vec::new();
        loop {
            let page = store
                .concerns_for_endpoint(&ConcernPageRequest::new(center.id(), 3, cursor).unwrap())
                .await
                .unwrap();
            assert!(page.items.len() <= 3);
            for row in page.items {
                let key = row.binding().key();
                let other = key
                    .endpoints()
                    .into_iter()
                    .find(|id| *id != center.id())
                    .unwrap();
                all.push((other, key.kind()));
            }
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(all.len(), 8);
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(all, sorted);
        store.delete_node(center.id()).await.unwrap();
        assert!(
            store
                .concerns_for_endpoint(&ConcernPageRequest::new(center.id(), 3, None).unwrap())
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }
    pub(crate) async fn unbounded_degree<S: GraphStore + ConcernStore>(store: &S) {
        let center = node(1, "hub");
        store.put_node(&center).await.unwrap();
        for id in 2..=1026 {
            let other = node(id, "distinct endpoint");
            store.put_node(&other).await.unwrap();
            assert!(matches!(
                store
                    .update_concern(&notice(&center, &other, ConcernKind::Disagreement))
                    .await
                    .unwrap(),
                ConcernCommitOutcome::Applied { .. }
            ));
        }
        let first = store
            .concerns_for_endpoint(&ConcernPageRequest::new(center.id(), 1024, None).unwrap())
            .await
            .unwrap();
        assert_eq!(first.items.len(), 1024);
        let cursor = first
            .next
            .expect("one more than graph structural degree ceiling");
        let last = store
            .concerns_for_endpoint(
                &ConcernPageRequest::new(center.id(), 1024, Some(cursor)).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(last.items.len(), 1);
        assert!(last.next.is_none());
        store.delete_node(center.id()).await.unwrap();
        assert!(
            store
                .concerns_for_endpoint(&ConcernPageRequest::new(center.id(), 1024, None).unwrap())
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }
    pub(crate) async fn body_ref_and_escaped_bounds<S: GraphStore + ConcernStore>(store: &S) {
        let a = node(1, "a");
        let b = node(2, "b");
        store.put_node(&a).await.unwrap();
        store.put_node(&b).await.unwrap();
        let binding = ConcernBinding::new(
            ConcernKind::Disagreement,
            ConcernEndpoint::from_node(&a),
            ConcernEndpoint::from_node(&b),
        )
        .unwrap();
        // NUL is constructor-valid text and JSON encodes every byte as six bytes.
        let update = ConcernUpdate::Notice(
            ConcernNotice::new(
                binding,
                "\0".repeat(MAX_CONCERN_BYTES),
                "\0".repeat(MAX_CONCERN_MISSING_FACT_BYTES),
            )
            .unwrap(),
        );
        let initial = resulting(store.update_concern(&update).await.unwrap());
        let escaped = ConcernUpdate::RecordScopedFinding {
            expected: initial,
            finding: ScopedConcernFinding::new(
                "\0".repeat(MAX_CONCERN_SCOPE_BYTES),
                "\0".repeat(MAX_CONCERN_FINDING_BYTES),
                vec![
                    ConcernEvidence::new(
                        "\0".repeat(MAX_CONCERN_EVIDENCE_REF_BYTES),
                        ConcernDigest::of_bytes(b"historical body bytes"),
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        };
        let row = resulting(store.update_concern(&escaped).await.unwrap());
        assert!(encode_concern_row(&row).unwrap().len() < MAX_CONCERN_ROW_JSON_BYTES);
        assert_eq!(
            store.get_concern(&binding.key()).await.unwrap(),
            Some(row.clone())
        );
        assert_eq!(
            store.update_concern(&escaped).await.unwrap(),
            ConcernCommitOutcome::Unchanged { row: row.clone() }
        );
        let changed = Node::try_new(
            b.id(),
            b.summary(),
            BodyRef::new("inline://different-body-reference").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        store.put_node(&changed).await.unwrap();
        assert_eq!(store.get_concern(&binding.key()).await.unwrap(), Some(row));
        assert!(matches!(
            store.update_concern(&escaped).await.unwrap(),
            ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::StaleMeanings,
                ..
            }
        ));
    }
    #[tokio::test]
    async fn mem_concern_degree_exceeds_graph_ceiling() {
        unbounded_degree(&MemStore::new(1)).await;
    }
    #[tokio::test]
    async fn mem_concern_body_reference_and_escape_bounds() {
        body_ref_and_escaped_bounds(&MemStore::new(1)).await;
    }
    #[tokio::test]
    async fn mem_concern_cas_lifecycle() {
        cas_lifecycle(&MemStore::new(1)).await;
    }
    #[tokio::test]
    async fn mem_concern_indexed_paging() {
        paging(&MemStore::new(1)).await;
    }
    #[test]
    fn predecessor_json_is_named_and_current_refusal_is_actionable() {
        let store = MemStore::new(1);
        let path =
            std::env::temp_dir().join(format!("mneme-concern-predecessor-{}.json", Ulid::new()));
        store.save_single_graph_v2(&path).unwrap();
        let error = match MemStore::load(&path) {
            Ok(_) => panic!("predecessor admitted"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("single-graph-upgrade --target-generation concern-v1"),
            "{error}"
        );
        assert_eq!(
            MemStore::load_single_graph_v2(&path).unwrap().db_id(),
            store.db_id()
        );
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn mem_concern_import_and_envelopes() {
        let store = MemStore::new(1);
        let a = node(1, "a");
        let b = node(2, "b");
        store.put_node(&a).await.unwrap();
        store.put_node(&b).await.unwrap();
        store
            .update_concern(&notice(&a, &b, ConcernKind::Disagreement))
            .await
            .unwrap();
        store.put_node(&node(2, "changed")).await.unwrap();
        let export = store.export();
        let reopened = MemStore::from_export(export.clone()).unwrap();
        assert_eq!(reopened.export().concerns, export.concerns);
        let mut duplicate = export.clone();
        duplicate.concerns.push(duplicate.concerns[0].clone());
        assert!(MemStore::from_export(duplicate).is_err());
        let mut missing = export.clone();
        missing.nodes.pop();
        assert!(MemStore::from_export(missing).is_err());
        assert!(serde_json::to_value(StoreExportEnvelopeV2::new(export.clone())).is_err());
        let mut malformed = serde_json::to_value(StoreExportEnvelopeV5::new(export)).unwrap();
        malformed["store"]["concerns"][0]["notice"]["binding"]["key"]["extra"] =
            serde_json::json!(1);
        assert!(serde_json::from_value::<StoreExportEnvelopeV5>(malformed).is_err());
        let path = std::env::temp_dir().join(format!("mneme-concern-{}.json", Ulid::new()));
        store.save(&path).unwrap();
        assert_eq!(
            MemStore::load(&path).unwrap().export().concerns,
            store.export().concerns
        );
        assert!(MemStore::load_single_graph_v2(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod tag_replacement_tests;

#[cfg(test)]
mod summary_edit_atomic_tests {
    use super::*;
    use mneme_core::managed::{
        DatabaseId, ManagedSchemaVersion, MutationEpoch, StorageGeneration, StorageId,
        StorageIdentity, WriterSchemaVersion,
    };
    use mneme_core::{BodyRef, NodeSummary, SummarySnapshot};

    #[tokio::test]
    async fn summary_edit_epoch_overflow_and_invalid_vectors_are_atomic() {
        let store = MemStore::new_ephemeral_managed_reference(
            4,
            DatabaseId::new(Ulid::from(75001u128)).unwrap(),
            StorageIdentity::new(
                StorageId::new(Ulid::from(75002u128)).unwrap(),
                StorageGeneration::new(1).unwrap(),
            ),
            ManagedSchemaVersion::new(4).unwrap(),
            WriterSchemaVersion::new(20).unwrap(),
            MutationEpoch::new(mneme_core::managed::MAX_MANAGED_STORAGE_INTEGER - 2).unwrap(),
        );
        let node = Node::try_new(
            NodeId(Ulid::new()),
            "oldsummary",
            BodyRef::new("inline://body").unwrap(),
            ["fixture"],
            Provenance::derived_empty(),
            0.3,
            0.7,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        store.put_node(&node).await.unwrap();
        store
            .upsert(node.id(), &[1.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        assert_eq!(
            store
                .observed_managed_head()
                .mutation_epoch()
                .unwrap()
                .get(),
            mneme_core::managed::MAX_MANAGED_STORAGE_INTEGER
        );
        let digest = SummarySnapshot::from_node(store.db_id(), &node)
            .unwrap()
            .digest();
        let original = serde_json::to_value(store.export()).unwrap();
        let summary = NodeSummary::new("newsummary").unwrap();
        for vector in [
            vec![1.0],
            vec![0.0; 4],
            vec![f32::NAN; 4],
            vec![f32::MAX; 4],
            vec![0.0, 1.0, 0.0, 0.0],
        ] {
            assert!(
                store
                    .compare_replace_node_summary(node.id(), &digest, &summary, &vector)
                    .await
                    .is_err()
            );
            assert_eq!(serde_json::to_value(store.export()).unwrap(), original);
            assert_eq!(
                store
                    .observed_managed_head()
                    .mutation_epoch()
                    .unwrap()
                    .get(),
                mneme_core::managed::MAX_MANAGED_STORAGE_INTEGER
            );
        }
    }
}
