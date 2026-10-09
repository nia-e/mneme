//! `CozoStore` — the production graph backend, behind the `cozo` feature.
//!
//! Maps the same three ports onto an embedded [cozo] database (the `mem` engine
//! here; swapping to `sqlite`/`rocksdb` is a one-line change in [`CozoStore::new`]
//! once those features are enabled). What cozo actually buys us:
//!
//! - **transactional Datalog storage** for nodes, edges and the contradiction
//!   overlay — every method below is a `run_script` call;
//! - **native HNSW** vector search for [`VectorIndex::ann`], instead of the
//!   reference store's brute-force scan.
//!
//! [`Traversal::spread`] and [`Traversal::detect_communities`] run in Rust over
//! the edge relation: a weighted, lifecycle-scoped BFS fetched one frontier per
//! indexed query, and **weighted Louvain** (modularity optimization —
//! [`crate::weighted_louvain`]). Cozo *has* native PageRank /
//! Louvain behind its `graph-algo` feature, but enabling it still pulls
//! `graph_builder 0.4.1`, which doesn't compile against current `rayon` (an
//! `E0308` in its `edges()`; re-verified 2026-06 — the mnestic fork re-exposes
//! the feature but didn't fix that transitive dep), so we own the algorithm.
//! The recursive-Datalog spread the scaffold describes remains a possible future
//! home; the current layer batching already avoids the old per-visited-node N+1.
//!
//! Nodes are stored as a JSON blob (`data`) plus the columns queries actually
//! need (`status` for the archived filter; tags and vectors in their own
//! relations). The blob keeps rehydration lossless without widening
//! `mneme_core::Node`'s encapsulated API.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
#[cfg(test)]
use std::path::Path;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use async_trait::async_trait;
use cozo::{
    BoundedReadTransaction, DataValue, DbInstance, MultiTransaction, NamedRows, Num,
    PrimaryKeyScan, PrimaryKeyScanBound, PrimaryKeyScanDirection, PrimaryKeyScanPage, Vector,
};
use mneme_core::managed::DatabaseId;
#[cfg(test)]
use mneme_core::managed::{
    ManagedSchemaVersion, ManagedStoreHead, MutationEpoch, StorageGeneration, StorageId,
    StorageIdentity, WriterSchemaVersion,
};
use mneme_core::ports::{
    Budget, ClusterId, ColdPath, DensePruneChunkOutcome, EmbeddingMetadataStore, Error,
    FeedbackCommit, FeedbackCommitOutcome, FullMergeCommit, FullMergeCommitOutcome, GraphStore,
    LexicalIndex, MAX_MAINTENANCE_BATCH_ROWS, MaintenanceCommit, MaintenanceCommitOutcome,
    MaintenanceEdgeKey, MaintenanceEdgeMutation, MaintenanceEdgePage, MaintenanceNodePage,
    Neighbor, Result, Scored, StatusFilter, SupersedeCommit, SupersedeCommitOutcome,
    TaggedAnnBatch, TaggedAnnLane, TaggedAnnLaneRequest, TaggedAnnRequest, TaggedAnnWork,
    TaggedExactWorkLimit, TaggedFallbackStrategy, TaggedPhysicalSeedCoverage,
    TaggedProjectionGeneration, TaggedQueryTagSeedCoverage, TaggedSeedCoverage, Traversal,
    TraversalScope, VectorIndex, tagged_exact_work_overflow, tagged_fallback_leg_quotas,
    tagged_physical_status_quotas, tagged_query_tag_quotas, tagged_sample_pivot,
};
use mneme_core::tagged::{TaggedPhysicalStatus, stable_tag_sample_hash};
use mneme_core::touchstone::*;
use mneme_core::{
    BodySpan, Contradiction, Edge, EdgeKind, EmbeddingFingerprint, FullMergeRecord,
    MAX_FEEDBACK_RETRY_RECORDS, MAX_FULL_MERGE_INCIDENT_EDGES, MAX_INCIDENT_EDGES,
    MAX_NODE_HYDRATION_BATCH, MAX_NODE_STATUS_BATCH, MAX_REMOTE_EDGES_PER_SOURCE, MergeCandidate,
    MergeResolution, Node, NodeId, NodeStatus, Provenance, RemoteEdge, RemoteEdgeCursor,
    RemoteEdgePage, Resolution, SupersedeRecord, Timestamp,
};
use ulid::Ulid;
use unordered_pair::UnorderedPair;

use crate::storage_contract::conventional_unmanaged::spec::{
    CAPTURE_V1_CATALOG_GENERATION_MARKER, CONCERN_V1_CATALOG_GENERATION_MARKER,
    EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER, LEGACY_CATALOG_GENERATION_MARKER,
    MANAGED_WRITER_GENERATION_MARKER, SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER,
    TOUCHSTONES_V1_CATALOG_GENERATION_MARKER,
};

#[cfg(test)]
use crate::storage_contract::conventional_unmanaged::spec::PERMANENT_VECTOR_GUARD_VALUE;

use self::commits::{
    stage_feedback_transaction, stage_full_merge_transaction, stage_supersede_transaction,
};
pub use self::fresh_current::{
    FreshCurrentMaterializationErrorV1, FreshCurrentMaterializationFailurePhaseV1,
    FreshCurrentMaterializationResultV1, FreshCurrentTargetPublicationStateV1,
};
use self::graph_records::row_to_edge;
#[cfg(test)]
use self::maintenance::tx_sync_tag_projection;
use self::maintenance::{
    MaintenanceStageOutcome, run_maintenance_transaction, stage_dense_prune_transaction,
    validate_primary_key_scan_page,
};
use self::opening::PersistentStoreAuthority;
use self::runtime::{
    ActiveBackendJob, LOCK_RETRY_BASE_MS, LOCK_RETRY_CAP_MS, LOCK_RETRY_MAX, TAGGED_READ_TIMEOUT,
    TaggedReadAdmission, install_lock_panic_filter, is_locked, panic_is_locked, run_db,
};
pub use self::runtime::{BackendActivity, BackendActivityGuard};
#[cfg(test)]
use self::runtime::{
    LOCK_RETRY_WAIT_CEILING_MS, MAX_CONCURRENT_TAGGED_READS, TaggedReadTestHook,
    TaggedScanObservation, lock_retry_backoff_ceiling_ms,
};

const SPREAD_FANOUT: usize = 8;
/// Bounded raw-edge shortlist re-ranked by query-conditioned edge score before
/// the final fanout is chosen.
const SPREAD_CONDITION_CANDIDATES: usize = SPREAD_FANOUT * 4;
/// HNSW search breadth; higher trades latency for recall.
const ANN_EF: i64 = 64;
const EMBEDDING_FINGERPRINT_META_KEY: &str = "embedding_fingerprint_v1";
const VECTOR_PROJECTION_META_KEY: &str = crate::vector_projection::META_KEY;
#[cfg(test)]
const CONVENTIONAL_VECTOR_PROJECTION_META_VALUE: &str = LEGACY_CATALOG_GENERATION_MARKER;
/// First managed Cozo storage contract. This deliberately rotates the existing
/// vector-generation sentinel so supported conventional-era binaries encounter an
/// unknown generation before their normal writers. An additive relation on its
/// own would be ignored and is not a fence. The historical-binary harness must
/// still be extended to managed v1 before this becomes an executable compatibility
/// claim.
#[cfg(test)]
const MANAGED_V1_VECTOR_PROJECTION_META_VALUE: &str = MANAGED_WRITER_GENERATION_MARKER;
#[cfg(test)]
const CANONICAL_NODE_V1_VECTOR_PROJECTION_META_VALUE: &str = crate::vector_projection::META_VALUE;
const CANONICAL_NODE_META_KEY: &str = crate::canonical_node_contract::META_KEY;
const CANONICAL_NODE_META_VALUE: &str = crate::canonical_node_contract::META_VALUE;
const LEXICAL_PROJECTION_META_KEY: &str = "lexical_projection_v1";
const INCIDENT_EDGE_CAP_META_KEY: &str = "max_incident_edges_v1";
const REMOTE_EDGE_SOURCE_CAP_META_KEY: &str = "max_remote_edges_per_source_v1";
#[cfg(test)]
const REEMBED_SHADOW_NODE_VEC: &str = crate::vector_projection::V3_SHADOW_NODE_VEC;
#[cfg(test)]
const REEMBED_SHADOW_META: &str = crate::vector_projection::V3_SHADOW_META;
#[cfg(test)]
const REEMBED_OLD_NODE_VEC: &str = crate::vector_projection::V3_OLD_NODE_VEC;
#[cfg(test)]
const REEMBED_OLD_META: &str = crate::vector_projection::V3_OLD_META;
const ACTIVE_VECTOR_INDEX: &str = "active_idx";
#[cfg(test)]
const CANDIDATE_VECTOR_INDEX: &str = "candidate_idx";
const ARCHIVED_VECTOR_INDEX: &str = "archived_idx";
const TAGGED_SCAN_PAGE: usize = 256;
#[cfg(test)]
const MANAGED_STORE_META_RELATION: &str = "store_meta";
#[cfg(test)]
const MANAGED_V1_SCHEMA_VERSION: u32 = 1;
#[cfg(test)]
const MANAGED_V1_MINIMUM_WRITER_SCHEMA: u32 = 1;
#[cfg(test)]
const MANAGED_V1_WRITER_FENCE: &str = "mneme-managed-writer-v1";
#[cfg(test)]
const CORRUPT_METADATA_PREVIEW_CHARS: usize = 64;
#[cfg(test)]
type RelationShapePredicate = fn(&NamedRows) -> bool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(test)]
enum StorageContractGeneration {
    /// Combined vector-v3, sealed canonical Node, and tag-v2 conventional unmanaged contract.
    ConventionalUnmanaged,
    /// Parsed managed-v1 metadata beside the exact managed v1 physical fence.
    ///
    /// This defensive slice recognizes the generation for read-only identity.
    /// No production path publishes managed v1, and normal persistent reopen refuses it. This is not yet a per-write
    /// fence for an already-open handle manually retagged through raw storage.
    ManagedV1(ManagedStoreHead),
    /// Combined vector-v3 plus sealed canonical Node contract before the conventional unmanaged
    /// contract.
    CanonicalNodeV1,
    /// Committed vector-v3 before the cumulative Node contract was fenced.
    VectorV3,
    /// Known older vector layout, or a store with no vector generation marker.
    LegacyVector,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DatabaseIdValidationError {
    Malformed,
    NonCanonical,
    Nil,
}

impl DatabaseIdValidationError {
    const fn detail(self) -> &'static str {
        match self {
            Self::Malformed => "is not a valid ULID",
            Self::NonCanonical => "is not canonical uppercase ULID text",
            Self::Nil => "must be non-nil",
        }
    }
}

fn parse_canonical_database_id(
    value: &str,
) -> std::result::Result<Ulid, DatabaseIdValidationError> {
    let parsed = Ulid::from_string(value).map_err(|_| DatabaseIdValidationError::Malformed)?;
    if parsed.to_string() != value {
        return Err(DatabaseIdValidationError::NonCanonical);
    }
    DatabaseId::new(parsed)
        .map(DatabaseId::get)
        .map_err(|_| DatabaseIdValidationError::Nil)
}

#[derive(Debug)]
#[cfg(test)]
struct ConventionalUpgradeAudit {
    generation: StorageContractGeneration,
    nodes: usize,
    vectors: usize,
    memberships: usize,
    canonical_normalization_required: bool,
    vector_shadow_required: bool,
    source_vector_has_status: bool,
    vector_guard: PredecessorGuardState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(test)]
enum PredecessorGuardGeneration {
    VectorV3,
    CanonicalNodeV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(test)]
enum PredecessorGuardState {
    /// The permanent read-only compatibility relation already exists.
    Ready(PredecessorGuardGeneration),
    /// A genuine old store has not yet installed the permanent fence.
    Missing,
    /// The fixed name still contains an exact recognized predecessor vector
    /// scratch relation. It must be replaced atomically with the fence.
    ReplaceRecognizedVectorArtifact,
}

#[derive(Clone, Debug)]
#[cfg(test)]
struct CanonicalAuditRow {
    id: String,
    node: Node,
    normalized: Option<String>,
}

pub struct CozoStore {
    db: DbInstance,
    persistent_authority: Option<Arc<PersistentStoreAuthority>>,
    dim: usize,
    db_id: Ulid,
    backend_activity: BackendActivity,
    tagged_read_admission: TaggedReadAdmission,
    #[cfg(test)]
    tagged_read_test_hook: Arc<TaggedReadTestHook>,
    #[cfg(test)]
    query_count: AtomicUsize,
    #[cfg(test)]
    last_maintenance_statements: AtomicUsize,
}

fn assert_cozo_store_send_sync<T: Send + Sync>() {}
const _: fn() = assert_cozo_store_send_sync::<CozoStore>;

impl CozoStore {
    /// Activity handle used by an owning host to prove the embedded backend is
    /// quiescent before it drops a cross-process store lease.
    pub fn backend_activity(&self) -> BackendActivity {
        self.backend_activity.clone()
    }

    /// Checkpoint and detach SQLite sidecars before an exclusive offline move.
    /// No database work may be started after this succeeds.
    pub fn prepare_for_file_move(&self) -> Result<()> {
        if self.backend_activity.in_flight() != 0 {
            return Err(Error::Backend(
                "cannot prepare sqlite file move while backend work is active".into(),
            ));
        }
        self.db.prepare_sqlite_for_file_move().map_err(backend)
    }

    /// Open an **ephemeral** in-memory cozo database and install the schema.
    pub fn new(dim: usize) -> Result<Self> {
        let db = DbInstance::new("mem", "", "").map_err(backend)?;
        let store = Self {
            db,
            persistent_authority: None,
            dim,
            db_id: Ulid::new(),
            backend_activity: BackendActivity::default(),
            tagged_read_admission: TaggedReadAdmission::default(),
            #[cfg(test)]
            tagged_read_test_hook: Arc::new(TaggedReadTestHook::default()),
            #[cfg(test)]
            query_count: AtomicUsize::new(0),
            #[cfg(test)]
            last_maintenance_statements: AtomicUsize::new(0),
        };
        store.run(
            &creation::touchstones_memory_schema(dim),
            BTreeMap::new(),
            true,
        )?;
        store.put_meta("db_id", &store.db_id.to_string())?;
        store.put_meta("dim", &dim.to_string())?;
        store.put_meta(
            VECTOR_PROJECTION_META_KEY,
            TOUCHSTONES_V1_CATALOG_GENERATION_MARKER,
        )?;
        store.put_meta(
            crate::canonical_node_contract::EPISODE_CONTEXT_META_KEY,
            crate::canonical_node_contract::EPISODE_CONTEXT_META_VALUE,
        )?;
        store.put_meta(
            crate::tag_projection::META_KEY,
            crate::tag_projection::META_VALUE,
        )?;
        store.put_meta(LEXICAL_PROJECTION_META_KEY, "complete")?;

        store.put_meta(INCIDENT_EDGE_CAP_META_KEY, &MAX_INCIDENT_EDGES.to_string())?;
        store.put_meta(
            REMOTE_EDGE_SOURCE_CAP_META_KEY,
            &MAX_REMOTE_EDGES_PER_SOURCE.to_string(),
        )?;
        Ok(store)
    }

    /// Create a fresh persistent EpisodeContextV2 database at an absent path.
    /// Existing stores require the leased current-generation opener; this entry
    /// point never repairs or adopts a predecessor. Writes persist directly.
    pub fn open(path: &str, dim: usize) -> Result<Self> {
        let path = std::path::Path::new(path);
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Err(Error::InvalidInput("existing stores require the leased current-generation opener; predecessors require single-graph-upgrade".into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(backend_str(format!("inspect fresh store path: {error}"))),
        }
        install_lock_panic_filter();
        let db = DbInstance::new("sqlite", path, "").map_err(backend)?;
        restrict_persistent_permissions(
            path.to_str()
                .ok_or_else(|| Error::InvalidInput("database path must be UTF-8".into()))?,
        )?;
        let id = Ulid::new();
        creation::stage_touchstones_schema(
            &db,
            dim,
            DatabaseId::new(id).map_err(|e| Error::InvalidInput(e.to_string()))?,
        )?;
        Ok(fresh_current::store_over_staged_database(db, dim, id))
    }

    /// Frozen predecessor fixture only: never a production admission fallback.
    #[cfg(test)]
    pub(crate) fn open_legacy_fixture(path: &str, dim: usize) -> Result<Self> {
        let db = DbInstance::new("sqlite", path, "").map_err(backend)?;
        let id = Ulid::new();
        creation::stage_current_schema(
            &db,
            dim,
            DatabaseId::new(id).map_err(|e| Error::InvalidInput(e.to_string()))?,
        )?;
        let store = fresh_current::store_over_staged_database(db, dim, id);
        store.put_meta(
            VECTOR_PROJECTION_META_KEY,
            crate::storage_contract::conventional_unmanaged::spec::LEGACY_CATALOG_GENERATION_MARKER,
        )?;
        Ok(store)
    }

    // Historical detectors below remain executable archaeology for raw fixtures.
    // Supported persistent admission uses opening/raw catalog contracts instead.
    #[cfg(test)]
    fn validate_vector_upgrade_source(&self, source_has_status: bool) -> Result<()> {
        let probes = [
            (
                "?[id] := *node_vec{id}, not *node{id} :limit 1",
                "vector without a canonical node",
            ),
            (
                "?[id] := *node{id, status}, status != 'active', status != 'candidate', status != 'archived' :limit 1",
                "node with an invalid lifecycle",
            ),
        ];
        for (probe, message) in probes {
            if !self.run(probe, BTreeMap::new(), false)?.rows.is_empty() {
                return Err(Error::Backend(format!(
                    "cannot upgrade vector projection: {message}"
                )));
            }
        }
        if source_has_status
            && !self
                .run(
                    "?[id] := *node_vec{id, status: projected}, *node{id, status}, projected != status :limit 1",
                    BTreeMap::new(),
                    false,
                )?
                .rows
                .is_empty()
        {
            return Err(Error::Backend(
                "cannot upgrade vector projection: node/vector lifecycle mismatch".into(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn validate_vector_values(&self) -> Result<usize> {
        let page_rows =
            crate::vector_projection::upgrade_vector_copy_page_rows(self.dim).ok_or_else(|| {
                backend_str(format!(
                    "cannot audit conventional vectors with dimension {} inside the {}-component page budget",
                    self.dim,
                    crate::vector_projection::UPGRADE_VECTOR_COPY_COMPONENT_BUDGET
                ))
            })?;
        let mut after: Option<String> = None;
        let mut count = 0usize;
        loop {
            let page = self.scan_primary_key(
                "node_vec",
                PrimaryKeyScan {
                    prefix: Vec::new(),
                    lower: after.as_ref().map_or(PrimaryKeyScanBound::Unbounded, |id| {
                        PrimaryKeyScanBound::Excluded(vec![dv_str(id)])
                    }),
                    upper: PrimaryKeyScanBound::Unbounded,
                    direction: PrimaryKeyScanDirection::Ascending,
                    limit: page_rows,
                },
            )?;
            if page.rows.rows.is_empty() {
                return Ok(count);
            }
            for row in &page.rows.rows {
                let id = want_str(&row[0])?;
                let vector = want_vector(&row[1])?;
                crate::validate_cosine_vector(&vector, &format!("legacy vector {id}"))?;
                after = Some(id.to_string());
            }
            count = count.saturating_add(page.rows.rows.len());
        }
    }

    #[cfg(test)]
    fn verify_shadow_hnsw(&self, relation: &str, index: &str, status: &str) -> Result<()> {
        let first = self.run(
            &format!("?[e] := *{relation}{{id, e, status: '{status}'}} :limit 1"),
            BTreeMap::new(),
            false,
        )?;
        let Some(row) = first.rows.first() else {
            return Ok(());
        };
        let query = want_vector(&row[0])?;
        crate::validate_cosine_vector(&query, "shadow verification vector")?;
        let mut params = BTreeMap::new();
        params.insert("q".into(), dv_float_list(&query));
        let hits = self.run(
            &format!(
                "?[id] := ~{relation}:{index}{{id | query: v, k: 1, ef: 16}}, \
                 v = vec($q) :limit 1"
            ),
            params,
            false,
        )?;
        if hits.rows.is_empty() {
            return Err(Error::Backend(format!(
                "shadow HNSW verification returned no hit for {relation}"
            )));
        }
        Ok(())
    }

    #[cfg(test)]
    fn canonical_vector_dim(&self) -> Result<usize> {
        let rows = self.run("::columns node_vec", BTreeMap::new(), false)?;
        vector_dim_from_columns(&rows)
    }

    /// This database's stable id.
    pub fn db_id(&self) -> Ulid {
        self.db_id
    }

    fn read_canonical_database_id(&self) -> Result<Option<Ulid>> {
        self.read_meta("db_id")?
            .map(|value| {
                parse_canonical_database_id(&value)
                    .map_err(|error| backend_str(format!("bad db_id: {}", error.detail())))
            })
            .transpose()
    }

    #[cfg(test)]
    fn require_matching_stored_dim(&self, operation: &str) -> Result<()> {
        let Some(stored) = self.read_meta("dim")? else {
            return Ok(());
        };
        let parsed = stored
            .parse::<usize>()
            .map_err(|error| backend_str(format!("bad stored dim {stored:?}: {error}")))?;
        if parsed != self.dim {
            return Err(Error::Backend(format!(
                "cannot {operation}: meta dimension {parsed} does not match node_vec dimension {}",
                self.dim
            )));
        }
        Ok(())
    }

    fn put_meta(&self, k: &str, v: &str) -> Result<()> {
        self.put_meta_in("meta", k, v)
    }

    fn put_meta_in(&self, relation: &str, k: &str, v: &str) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("k".into(), dv_str(k));
        p.insert("v".into(), dv_str(v));
        self.run(
            &format!("?[k, v] <- [[$k, $v]] :put {relation} {{k => v}}"),
            p,
            true,
        )?;
        Ok(())
    }

    fn read_meta(&self, k: &str) -> Result<Option<String>> {
        let mut p = BTreeMap::new();
        p.insert("k".into(), dv_str(k));
        let rows = self.run("?[v] := *meta{k: $k, v}", p, false)?;
        match rows.rows.into_iter().next() {
            None => Ok(None),
            Some(row) => Ok(Some(want_str(&row[0])?.to_string())),
        }
    }

    /// Classify only explicitly supported generations. A lone or unexpected
    /// canonical Node marker is metadata corruption, not license to rewrite
    /// the store. Managed v1 is valid only when its exact `store_meta` row and
    /// physical marker were published together. A side relation beside the
    /// conventional generation is not a writer fence; every torn or foreign
    /// pairing fails closed.
    #[cfg(test)]
    fn storage_contract_generation(&self) -> Result<StorageContractGeneration> {
        use crate::vector_projection::{LEGACY_V2_META_VALUE, PRIOR_V3_META_VALUE};

        let has_store_meta = self.relation_exists(MANAGED_STORE_META_RELATION)?;
        if !self.relation_exists("meta")? {
            if has_store_meta {
                return Err(managed_contract_corrupt(
                    "store_meta exists without the canonical meta relation",
                ));
            }
            return Ok(StorageContractGeneration::LegacyVector);
        }
        let vector = self.read_meta(VECTOR_PROJECTION_META_KEY)?;
        let node = self.read_meta(CANONICAL_NODE_META_KEY)?;
        let tag = self.read_meta(crate::tag_projection::META_KEY)?;

        if has_store_meta && vector.as_deref() != Some(MANAGED_V1_VECTOR_PROJECTION_META_VALUE) {
            return Err(managed_contract_corrupt(format!(
                "store_meta exists beside non-managed v1 vector sentinel {}; additive metadata alone is not a writer fence",
                bounded_optional_metadata(vector.as_deref())
            )));
        }

        match (vector.as_deref(), node.as_deref(), tag.as_deref()) {
            (
                Some(CONVENTIONAL_VECTOR_PROJECTION_META_VALUE),
                Some(CANONICAL_NODE_META_VALUE),
                Some(crate::tag_projection::META_VALUE),
            ) => Ok(StorageContractGeneration::ConventionalUnmanaged),
            (
                Some(MANAGED_V1_VECTOR_PROJECTION_META_VALUE),
                Some(CANONICAL_NODE_META_VALUE),
                Some(crate::tag_projection::META_VALUE),
            ) if has_store_meta => Ok(StorageContractGeneration::ManagedV1(
                self.read_exact_managed_v1_head()?,
            )),
            (
                Some(MANAGED_V1_VECTOR_PROJECTION_META_VALUE),
                Some(CANONICAL_NODE_META_VALUE),
                Some(crate::tag_projection::META_VALUE),
            ) => Err(managed_contract_corrupt(
                "managed v1 vector sentinel exists without store_meta; publication is torn",
            )),
            (
                Some(CANONICAL_NODE_V1_VECTOR_PROJECTION_META_VALUE),
                Some(CANONICAL_NODE_META_VALUE),
                None,
            ) => Ok(StorageContractGeneration::CanonicalNodeV1),
            (Some(PRIOR_V3_META_VALUE), None, None) => Ok(StorageContractGeneration::VectorV3),
            (Some(LEGACY_V2_META_VALUE) | None, None, None) => {
                Ok(StorageContractGeneration::LegacyVector)
            }
            (Some(CONVENTIONAL_VECTOR_PROJECTION_META_VALUE), node, tag) => {
                Err(Error::Backend(format!(
                    "current conventional vector sentinel disagrees with companion markers: expected canonical {CANONICAL_NODE_META_VALUE:?} and tag {:?}, found canonical {}, tag {}; database metadata is corrupt",
                    crate::tag_projection::META_VALUE,
                    bounded_optional_metadata(node),
                    bounded_optional_metadata(tag),
                )))
            }
            (Some(CAPTURE_V1_CATALOG_GENERATION_MARKER), node, tag) => {
                Err(Error::Backend(format!(
                    "capture v1 vector sentinel disagrees with companion markers: expected canonical {CANONICAL_NODE_META_VALUE:?} and tag {:?}, found canonical {}, tag {}; database metadata is corrupt",
                    crate::tag_projection::META_VALUE,
                    bounded_optional_metadata(node),
                    bounded_optional_metadata(tag),
                )))
            }
            (Some(MANAGED_V1_VECTOR_PROJECTION_META_VALUE), node, tag) => {
                Err(managed_contract_corrupt(format!(
                    "managed v1 vector sentinel disagrees with companion markers: expected canonical {CANONICAL_NODE_META_VALUE:?} and tag {:?}, found canonical {}, tag {}",
                    crate::tag_projection::META_VALUE,
                    bounded_optional_metadata(node),
                    bounded_optional_metadata(tag),
                )))
            }
            (
                Some(
                    CANONICAL_NODE_V1_VECTOR_PROJECTION_META_VALUE
                    | PRIOR_V3_META_VALUE
                    | LEGACY_V2_META_VALUE,
                )
                | None,
                node,
                tag,
            ) if node.is_some() || tag.is_some() => Err(Error::Backend(format!(
                "legacy vector sentinel disagrees with companion markers canonical {}, tag {}; database metadata is corrupt",
                bounded_optional_metadata(node),
                bounded_optional_metadata(tag),
            ))),
            (Some(other), node, tag) => Err(Error::Backend(format!(
                "unrecognized vector projection sentinel {} with canonical marker {} and tag marker {}; database metadata is corrupt",
                bounded_metadata(other),
                bounded_optional_metadata(node),
                bounded_optional_metadata(tag),
            ))),
            (None, node, tag) => Err(Error::Backend(format!(
                "missing vector projection sentinel with canonical marker {} and tag marker {}; database metadata is corrupt",
                bounded_optional_metadata(node),
                bounded_optional_metadata(tag),
            ))),
        }
    }

    /// Parse the one exact managed-v1 metadata row without repairing, stamping,
    /// or accepting future relation shapes under today's managed v1 sentinel.
    #[cfg(test)]
    fn read_exact_managed_v1_head(&self) -> Result<ManagedStoreHead> {
        self.require_exact_managed_store_meta_catalog()?;

        let rows = self.run(
            "?[storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch] := *store_meta{storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch} :limit 2",
            BTreeMap::new(),
            false,
        )?;
        let canonical_db_id = self
            .read_meta("db_id")?
            .ok_or_else(|| managed_contract_corrupt("managed v1 requires meta.db_id"))?;
        decode_exact_managed_v1_head(&rows, &canonical_db_id)
    }

    #[cfg(test)]
    fn require_exact_managed_store_meta_catalog(&self) -> Result<()> {
        let relation = MANAGED_STORE_META_RELATION;
        if self.relation_access_level(relation)?.as_deref() == Some("normal")
            && managed_store_meta_columns_are_exact(&self.relation_columns(relation)?)
            && self.normal_secondary_indices_are_exact(relation, &[])?
            && self.relation_has_no_triggers(relation)?
        {
            return Ok(());
        }
        Err(managed_contract_corrupt(
            "store_meta has an unexpected schema, access level, child index, trigger, or default",
        ))
    }

    /// Complete mutation-free conventional canonical audit. Besides the sealed Node
    /// contract it records the exact amount of derived membership work and
    /// whether the only admitted historical rewrite (raw tag-array ordering)
    /// needs a detached canonical shadow.
    #[cfg(test)]
    fn audit_canonical_nodes_for_conventional_upgrade(&self) -> Result<(usize, usize, bool)> {
        if self.relation_access_level("node")?.as_deref() != Some("normal")
            || !crate::tag_projection::canonical_node_columns_are_exact(
                &self.relation_columns("node")?,
            )
            || !self.normal_secondary_indices_are_exact("node", &[])?
            || !self.relation_has_no_triggers("node")?
        {
            return Err(Error::Backend(
                "cannot perform conventional upgrade: canonical node relation has an unexpected shape, access level, or child index"
                    .into(),
            ));
        }
        let mut after: Option<String> = None;
        let mut nodes = 0usize;
        let mut memberships = 0usize;
        let mut normalization = false;
        loop {
            let page = self.scan_primary_key(
                "node",
                PrimaryKeyScan {
                    prefix: Vec::new(),
                    lower: after.as_ref().map_or(PrimaryKeyScanBound::Unbounded, |id| {
                        PrimaryKeyScanBound::Excluded(vec![dv_str(id)])
                    }),
                    upper: PrimaryKeyScanBound::Unbounded,
                    direction: PrimaryKeyScanDirection::Ascending,
                    // Historical rows have no authenticated length projection.
                    // Decode one complete raw blob at a time before allocating
                    // any typed substructure, matching the sealed-node audit.
                    limit: crate::canonical_node_contract::AUDIT_PAGE_SIZE,
                },
            )?;
            if page.rows.rows.is_empty() {
                return Ok((nodes, memberships, normalization));
            }
            for row in &page.rows.rows {
                let audited = decode_upgrade_canonical_row(row).map_err(|error| {
                    let detail = match error {
                        Error::Backend(detail) => detail,
                        other => other.to_string(),
                    };
                    Error::Backend(format!(
                        "{detail}; the conventional upgrade does not repair malformed canonical semantics; repair with an offline export/edit/rebuild and retry"
                    ))
                })?;
                memberships = memberships
                    .checked_add(audited.node.tags().len())
                    .ok_or_else(|| {
                        backend_str("conventional tag membership count overflowed".into())
                    })?;
                normalization |= audited.normalized.is_some();
                after = Some(audited.id);
            }
            nodes = nodes.checked_add(page.rows.rows.len()).ok_or_else(|| {
                backend_str("conventional canonical node count overflowed".into())
            })?;
        }
    }

    #[cfg(test)]
    fn delete_meta(&self, k: &str) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("k".into(), dv_str(k));
        self.run("?[k] := *meta{k}, k == $k :rm meta {k}", p, true)?;
        Ok(())
    }

    #[cfg(test)]
    fn relation_exists(&self, relation: &str) -> Result<bool> {
        let rows = self.run("::relations", BTreeMap::new(), false)?;
        Ok(rows.rows.iter().any(
            |row| matches!(row.first(), Some(DataValue::Str(name)) if name.as_str() == relation),
        ))
    }

    #[cfg(test)]
    fn relation_access_level(&self, relation: &str) -> Result<Option<String>> {
        let rows = self.run("::relations", BTreeMap::new(), false)?;
        rows.rows
            .iter()
            .find(|row| {
                matches!(row.first(), Some(DataValue::Str(name)) if name.as_str() == relation)
            })
            .map(|row| {
                row.get(2)
                    .ok_or_else(|| {
                        Error::Backend(format!(
                            "relation catalog row for {relation:?} has no access level"
                        ))
                    })
                    .and_then(want_str)
                    .map(str::to_owned)
            })
            .transpose()
    }

    #[cfg(test)]
    fn relation_columns(&self, relation: &str) -> Result<NamedRows> {
        self.run(&format!("::columns {relation}"), BTreeMap::new(), false)
    }

    #[cfg(test)]
    fn relation_has_no_triggers(&self, relation: &str) -> Result<bool> {
        Ok(self
            .run(
                &format!("::show_triggers {relation}"),
                BTreeMap::new(),
                false,
            )?
            .rows
            .is_empty())
    }

    #[cfg(test)]
    fn relation_count(&self, relation: &str, key: &str) -> Result<usize> {
        let rows = self.run(
            &format!("?[count({key})] := *{relation}{{{key}}}"),
            BTreeMap::new(),
            false,
        )?;
        rows.rows
            .first()
            .map(|row| want_i64(&row[0]).map(|count| count.max(0) as usize))
            .transpose()
            .map(|count| count.unwrap_or(0))
    }

    #[cfg(test)]
    fn normal_secondary_indices_are_exact(
        &self,
        relation: &str,
        expected: &[(&str, &[usize])],
    ) -> Result<bool> {
        use crate::storage_contract::conventional_unmanaged::visible_catalog::{
            NormalIndexExpectation, validate_catalog_visible_indices,
        };

        let catalog = self.run(&format!("::indices {relation}"), BTreeMap::new(), false)?;
        let child_relations = expected
            .iter()
            .map(|(name, _)| format!("{relation}:{name}"))
            .collect::<Vec<_>>();
        let expected = expected
            .iter()
            .zip(&child_relations)
            .map(|((name, columns), child)| {
                NormalIndexExpectation::new(name, child.as_str(), columns)
            })
            .collect::<Vec<_>>();
        Ok(validate_catalog_visible_indices(&catalog, &expected, &[]).is_ok())
    }

    #[cfg(test)]
    fn hnsw_indices_are_exact(&self, relation: &str, expected: &[&str]) -> Result<bool> {
        use crate::storage_contract::conventional_unmanaged::visible_catalog::{
            HnswIndexExpectation, validate_catalog_visible_indices,
        };

        let catalog = self.run(&format!("::indices {relation}"), BTreeMap::new(), false)?;
        let dimension = vector_dim_from_columns(&self.relation_columns(relation)?)?;
        let child_relations = expected
            .iter()
            .map(|name| format!("{relation}:{name}"))
            .collect::<Vec<_>>();
        let expected = expected
            .iter()
            .zip(&child_relations)
            .map(|(name, child)| HnswIndexExpectation::new(name, child.as_str(), dimension))
            .collect::<Vec<_>>();
        Ok(validate_catalog_visible_indices(&catalog, &[], &expected).is_ok())
    }

    #[cfg(test)]
    fn vector_child_set_matches_known_exact_manifest(&self, relation: &str) -> Result<bool> {
        for expected in [
            &[][..],
            &["idx"][..],
            &[
                ACTIVE_VECTOR_INDEX,
                CANDIDATE_VECTOR_INDEX,
                ARCHIVED_VECTOR_INDEX,
            ][..],
        ] {
            if self.hnsw_indices_are_exact(relation, expected)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[cfg(test)]
    fn audit_legacy_tag_source(&self) -> Result<()> {
        if self.relation_access_level("node_tag")?.as_deref() != Some("normal")
            || !crate::tag_projection::legacy_writable_columns_are_exact(
                &self.relation_columns("node_tag")?,
            )
            || !self.normal_secondary_indices_are_exact("node_tag", &[("by_tag", &[1, 0])])?
            || !self.relation_has_no_triggers("node_tag")?
        {
            return Err(Error::Backend(
                "cannot perform conventional upgrade: legacy node_tag must have the exact writable {id, tag} shape and by_tag index"
                    .into(),
            ));
        }
        if self.relation_exists("node_tag_v2")? {
            return Err(Error::Backend(
                "cannot perform conventional upgrade: an unmarked node_tag_v2 relation is mixed-generation state"
                    .into(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn audit_conventional_owned_scratch_state(
        &self,
        generation: StorageContractGeneration,
    ) -> Result<()> {
        use crate::tag_projection as tag;

        let shadow_specs: [(&str, RelationShapePredicate); 4] = [
            (tag::SHADOW_NODE_TAG, tag::membership_columns_are_exact),
            (
                tag::SHADOW_LEGACY_GUARD,
                tag::legacy_writable_columns_are_exact,
            ),
            (tag::SHADOW_NODE, tag::canonical_node_columns_are_exact),
            (
                tag::SHADOW_META,
                crate::vector_projection::meta_columns_are_exact,
            ),
        ];
        for (relation, shape) in shadow_specs {
            if !self.relation_exists(relation)? {
                continue;
            }
            if matches!(
                generation,
                StorageContractGeneration::ConventionalUnmanaged
                    | StorageContractGeneration::ManagedV1(_)
            ) || self.relation_access_level(relation)?.as_deref() != Some("normal")
                || !shape(&self.relation_columns(relation)?)
                || !self.relation_has_no_triggers(relation)?
            {
                return Err(Error::Backend(format!(
                    "cannot perform conventional upgrade: scratch relation {relation:?} is mixed or unrecognized"
                )));
            }
            if relation == tag::SHADOW_NODE_TAG
                && !self
                    .normal_secondary_indices_are_exact(relation, &[("by_id", &[3, 0, 1, 2])])?
            {
                return Err(Error::Backend(format!(
                    "cannot perform conventional upgrade: membership scratch {relation:?} has unexpected indexes"
                )));
            }
            if relation == tag::SHADOW_LEGACY_GUARD
                && (!self.normal_secondary_indices_are_exact(relation, &[("by_tag", &[1, 0])])?
                    || self.relation_count(relation, "id")? != 0)
            {
                return Err(Error::Backend(format!(
                    "cannot perform conventional upgrade: legacy-guard scratch {relation:?} is not exact and empty"
                )));
            }
            if matches!(relation, tag::SHADOW_NODE | tag::SHADOW_META)
                && !self.normal_secondary_indices_are_exact(relation, &[])?
            {
                return Err(Error::Backend(format!(
                    "cannot perform conventional upgrade: scratch relation {relation:?} has unexpected indexes"
                )));
            }
        }
        if self.relation_exists(tag::SHADOW_NODE_VEC)? {
            let relation = tag::SHADOW_NODE_VEC;
            let columns = self.relation_columns(relation)?;
            if matches!(
                generation,
                StorageContractGeneration::ConventionalUnmanaged
                    | StorageContractGeneration::ManagedV1(_)
            ) || self.relation_access_level(relation)?.as_deref() != Some("normal")
                || !crate::vector_projection::legacy_vector_columns_are_recognized(&columns)
                || !self.vector_relation_has_status(relation)?
                || vector_dim_from_columns(&columns)? != self.dim
                || !self.relation_has_no_triggers(relation)?
            {
                return Err(Error::Backend(format!(
                    "cannot perform conventional upgrade: vector scratch {relation:?} has an unexpected conventional schema, dimension, or access level"
                )));
            }
            if !(self.hnsw_indices_are_exact(relation, &[])?
                || self.hnsw_indices_are_exact(
                    relation,
                    &[
                        ACTIVE_VECTOR_INDEX,
                        CANDIDATE_VECTOR_INDEX,
                        ARCHIVED_VECTOR_INDEX,
                    ],
                )?)
            {
                return Err(Error::Backend(format!(
                    "cannot perform conventional upgrade: vector scratch {relation:?} has a partial or unexpected child-index set"
                )));
            }
        }
        if self.relation_exists(tag::SHADOW_VECTOR_GUARD)? {
            let relation = tag::SHADOW_VECTOR_GUARD;
            if matches!(
                generation,
                StorageContractGeneration::ConventionalUnmanaged
                    | StorageContractGeneration::ManagedV1(_)
            ) || self.relation_access_level(relation)?.as_deref() != Some("normal")
                || !crate::vector_projection::guard_columns_are_exact(
                    &self.relation_columns(relation)?,
                )
                || !self.relation_has_no_triggers(relation)?
            {
                return Err(Error::Backend(format!(
                    "cannot perform conventional upgrade: vector-guard scratch {relation:?} is unrecognized"
                )));
            }
            if !self.normal_secondary_indices_are_exact(relation, &[])? {
                return Err(Error::Backend(format!(
                    "cannot perform conventional upgrade: vector-guard scratch {relation:?} has unexpected indexes"
                )));
            }
            let rows = self.run(
                &format!("?[fence, generation] := *{relation}{{fence, generation}} :limit 2"),
                BTreeMap::new(),
                false,
            )?;
            if rows.rows.as_slice()
                != [vec![
                    dv_str(crate::vector_projection::GUARD_KEY),
                    dv_str(PERMANENT_VECTOR_GUARD_VALUE),
                ]]
            {
                return Err(Error::Backend(format!(
                    "cannot perform conventional upgrade: vector-guard scratch {relation:?} has the wrong row"
                )));
            }
        }

        for relation in [
            tag::OLD_LEGACY_NODE_TAG,
            tag::OLD_NODE,
            tag::OLD_NODE_VEC,
            tag::OLD_META,
        ] {
            if self.relation_exists(relation)?
                && !matches!(
                    generation,
                    StorageContractGeneration::ConventionalUnmanaged
                        | StorageContractGeneration::ManagedV1(_)
                )
            {
                return Err(Error::Backend(format!(
                    "cannot perform conventional upgrade: rollback relation {relation:?} beside an old marker is mixed-generation state"
                )));
            }
        }

        Ok(())
    }

    #[cfg(test)]
    fn audit_legacy_cleanup_state(&self) -> Result<()> {
        for relation in [
            REEMBED_SHADOW_NODE_VEC,
            REEMBED_OLD_NODE_VEC,
            crate::vector_projection::LEGACY_OLD_NODE_VEC,
            "candidate_vec",
        ] {
            if !self.relation_exists(relation)? {
                continue;
            }
            if self.relation_access_level(relation)?.as_deref() != Some("normal")
                || !crate::vector_projection::legacy_vector_columns_are_recognized(
                    &self.relation_columns(relation)?,
                )
                || !self.vector_child_set_matches_known_exact_manifest(relation)?
                || !self.relation_has_no_triggers(relation)?
            {
                return Err(Error::Backend(format!(
                    "cannot perform conventional upgrade: legacy vector cleanup artifact {relation:?} has an unrecognized shape or child-index set"
                )));
            }
        }
        for relation in [
            REEMBED_SHADOW_META,
            REEMBED_OLD_META,
            crate::vector_projection::LEGACY_SHADOW_META,
            crate::vector_projection::LEGACY_OLD_META,
        ] {
            if !self.relation_exists(relation)? {
                continue;
            }
            if self.relation_access_level(relation)?.as_deref() != Some("normal")
                || !crate::vector_projection::meta_columns_are_exact(
                    &self.relation_columns(relation)?,
                )
                || !self.normal_secondary_indices_are_exact(relation, &[])?
                || !self.relation_has_no_triggers(relation)?
            {
                return Err(Error::Backend(format!(
                    "cannot perform conventional upgrade: legacy meta cleanup artifact {relation:?} has an unrecognized shape or child-index set"
                )));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn audit_conventional_upgrade_source(
        &self,
        generation: StorageContractGeneration,
    ) -> Result<ConventionalUpgradeAudit> {
        if self.relation_access_level("meta")?.as_deref() != Some("normal")
            || !crate::vector_projection::meta_columns_are_exact(&self.relation_columns("meta")?)
            || !self.normal_secondary_indices_are_exact("meta", &[])?
            || !self.relation_has_no_triggers("meta")?
        {
            return Err(Error::Backend(
                "cannot perform conventional upgrade: meta relation has an unexpected shape, access level, or child index"
                    .into(),
            ));
        }
        if let Some(db_id) = self.read_meta("db_id")? {
            parse_canonical_database_id(&db_id).map_err(|error| {
                Error::Backend(format!(
                    "cannot perform conventional upgrade: source db_id {}",
                    error.detail()
                ))
            })?;
        }
        self.audit_conventional_owned_scratch_state(generation)?;
        self.audit_legacy_cleanup_state()?;
        self.audit_legacy_tag_source()?;
        let (nodes, memberships, canonical_normalization_required) =
            self.audit_canonical_nodes_for_conventional_upgrade()?;

        let vector_columns = self.relation_columns("node_vec")?;
        if self.relation_access_level("node_vec")?.as_deref() != Some("normal")
            || !crate::vector_projection::legacy_vector_columns_are_recognized(&vector_columns)
            || !self.relation_has_no_triggers("node_vec")?
        {
            return Err(Error::Backend(
                "cannot perform conventional upgrade: node_vec has an unexpected shape or access level".into(),
            ));
        }
        let source_vector_has_status = self.vector_relation_has_status("node_vec")?;
        self.validate_vector_upgrade_source(source_vector_has_status)?;
        let vectors = self.validate_vector_values()?;

        let current_hnsws = self.hnsw_indices_are_exact(
            "node_vec",
            &[
                ACTIVE_VECTOR_INDEX,
                CANDIDATE_VECTOR_INDEX,
                ARCHIVED_VECTOR_INDEX,
            ],
        )?;
        let vector_shadow_required = match generation {
            StorageContractGeneration::LegacyVector => {
                if !self.vector_child_set_matches_known_exact_manifest("node_vec")? {
                    return Err(Error::Backend(
                        "cannot perform conventional upgrade: pre-v3 node_vec has an unrecognized child-index set"
                            .into(),
                    ));
                }
                // `::indices` exposes names and kinds, but not the HNSW filter
                // predicates. A pre-v3 marker authenticates no lifecycle
                // manifest, so even three plausibly named lanes must be rebuilt.
                true
            }
            StorageContractGeneration::CanonicalNodeV1 | StorageContractGeneration::VectorV3 => {
                if !source_vector_has_status || !current_hnsws {
                    return Err(Error::Backend(
                        "cannot perform conventional upgrade: committed vector-v3 source is missing its exact lifecycle HNSWs"
                            .into(),
                    ));
                }
                false
            }
            StorageContractGeneration::ConventionalUnmanaged
            | StorageContractGeneration::ManagedV1(_) => {
                unreachable!("published generations are audited separately")
            }
        };
        if !vector_shadow_required {
            self.require_matching_stored_dim("perform conventional upgrade")?;
            for (index, status) in vector_lanes() {
                self.verify_shadow_hnsw("node_vec", index, status)?;
            }
        }

        let vector_guard = crate::vector_projection::LEGACY_SHADOW_GUARD;
        let vector_guard = match generation {
            StorageContractGeneration::CanonicalNodeV1 => {
                self.ensure_predecessor_guard_ready()?;
                PredecessorGuardState::Ready(PredecessorGuardGeneration::CanonicalNodeV1)
            }
            StorageContractGeneration::VectorV3 => {
                self.ensure_prior_legacy_shadow_guard_ready()?;
                PredecessorGuardState::Ready(PredecessorGuardGeneration::VectorV3)
            }
            StorageContractGeneration::LegacyVector => {
                if !self.relation_exists(vector_guard)? {
                    PredecessorGuardState::Missing
                } else if self.ensure_predecessor_guard_ready().is_ok() {
                    PredecessorGuardState::Ready(PredecessorGuardGeneration::CanonicalNodeV1)
                } else if self.ensure_prior_legacy_shadow_guard_ready().is_ok() {
                    // A predecessor migration may have committed its early
                    // fence and then crashed before publishing its markers.
                    PredecessorGuardState::Ready(PredecessorGuardGeneration::VectorV3)
                } else {
                    let columns = self.relation_columns(vector_guard)?;
                    if self.relation_access_level(vector_guard)?.as_deref() == Some("normal")
                        && crate::vector_projection::legacy_vector_columns_are_recognized(&columns)
                        && self.vector_child_set_matches_known_exact_manifest(vector_guard)?
                        && self.relation_has_no_triggers(vector_guard)?
                    {
                        PredecessorGuardState::ReplaceRecognizedVectorArtifact
                    } else {
                        return Err(Error::Backend(
                            "cannot perform conventional upgrade: pre-v3 vector guard/scratch has an unrecognized shape"
                                .into(),
                        ));
                    }
                }
            }
            StorageContractGeneration::ConventionalUnmanaged
            | StorageContractGeneration::ManagedV1(_) => {
                unreachable!("published generations are audited separately")
            }
        };

        Ok(ConventionalUpgradeAudit {
            generation,
            nodes,
            vectors,
            memberships,
            canonical_normalization_required,
            vector_shadow_required,
            source_vector_has_status,
            vector_guard,
        })
    }

    #[cfg(test)]
    fn verify_membership_upgrade_projection(
        &self,
        relation: &str,
        expected: usize,
        require_canonical_encoding: bool,
    ) -> Result<()> {
        use crate::tag_projection as tag;

        if self.relation_count(relation, "tag")? != expected {
            return Err(Error::Backend(format!(
                "conventional membership projection {relation:?} aggregate count does not equal {expected}"
            )));
        }
        let mut after: Option<String> = None;
        let mut verified = 0usize;
        loop {
            let page = self.scan_primary_key(
                "node",
                PrimaryKeyScan {
                    prefix: Vec::new(),
                    lower: after.as_ref().map_or(PrimaryKeyScanBound::Unbounded, |id| {
                        PrimaryKeyScanBound::Excluded(vec![dv_str(id)])
                    }),
                    upper: PrimaryKeyScanBound::Unbounded,
                    direction: PrimaryKeyScanDirection::Ascending,
                    limit: tag::NODE_AUDIT_PAGE,
                },
            )?;
            if page.rows.rows.is_empty() {
                break;
            }
            let mut expected_by_id = BTreeMap::new();
            let mut ids = Vec::with_capacity(page.rows.rows.len());
            for row in &page.rows.rows {
                let audited = decode_upgrade_canonical_row(row)?;
                if require_canonical_encoding && audited.normalized.is_some() {
                    return Err(Error::Backend(format!(
                        "current conventional canonical node {} still needs normalization",
                        audited.id
                    )));
                }
                let id = audited.node.id();
                let status = TaggedPhysicalStatus::from(audited.node.status());
                let sample_hash = stable_tag_sample_hash(id);
                let expected_rows = audited
                    .node
                    .tags()
                    .map(|tag| (tag.to_owned(), status.as_str().to_owned(), sample_hash))
                    .collect::<Vec<_>>();
                ids.push(id);
                if expected_by_id
                    .insert(audited.id.clone(), expected_rows)
                    .is_some()
                {
                    return Err(Error::Backend(format!(
                        "conventional trusted node page repeated canonical id {}",
                        audited.id
                    )));
                }
                after = Some(audited.id);
            }

            // One indexed query covers the complete <=64-node page. The +1
            // canary keeps hostile scratch from returning more than the
            // authenticated 64 tags per node before Rust groups any rows.
            let (input, params) = id_input("wanted", &ids);
            let rows = self.run(
                &tag::membership_verification_query(&input, relation),
                params,
                false,
            )?;
            if rows.rows.len() >= tag::MEMBERSHIP_VERIFY_RESULT_CAP {
                return Err(Error::Backend(format!(
                    "conventional membership projection {relation:?} exceeded the {}-row verification cap for one trusted node page",
                    tag::MEMBERSHIP_VERIFY_RESULT_CAP - 1
                )));
            }
            let mut actual_by_id = expected_by_id
                .keys()
                .map(|id| (id.clone(), Vec::new()))
                .collect::<BTreeMap<_, Vec<(String, String, i64)>>>();
            for row in &rows.rows {
                let id = want_str(&row[0])?;
                let actual = actual_by_id.get_mut(id).ok_or_else(|| {
                    backend_str(format!(
                        "conventional membership page returned unrequested canonical id {id}"
                    ))
                })?;
                actual.push((
                    want_str(&row[1])?.to_owned(),
                    want_str(&row[2])?.to_owned(),
                    want_i64(&row[3])?,
                ));
            }
            for (id, mut expected_rows) in expected_by_id {
                let mut actual = actual_by_id.remove(&id).ok_or_else(|| {
                    backend_str(format!(
                        "conventional membership verifier lost canonical id {id}"
                    ))
                })?;
                expected_rows.sort();
                actual.sort();
                if actual != expected_rows {
                    return Err(Error::Backend(format!(
                        "conventional membership projection {relation:?} differs from canonical node {id}"
                    )));
                }
                verified = verified.checked_add(actual.len()).ok_or_else(|| {
                    backend_str("conventional verified membership count overflowed".into())
                })?;
            }
        }
        if verified != expected {
            return Err(Error::Backend(format!(
                "conventional membership projection {relation:?} contains orphan rows: canonical memberships {verified}, aggregate {expected}"
            )));
        }
        Ok(())
    }

    /// Re-authenticate the complete active conventional generation after rename/reopen.
    /// This is intentionally stronger than ordinary startup: it authorizes
    /// deletion of rollback evidence and therefore checks every catalog-visible
    /// index field, canonical membership parity, vector values/lifecycle, and
    /// the non-empty HNSW lanes before returning counts.
    #[cfg(test)]
    fn verify_current_conventional_generation(
        &self,
        require_db_id: bool,
    ) -> Result<(usize, usize, usize)> {
        if self.storage_contract_generation()? != StorageContractGeneration::ConventionalUnmanaged {
            return Err(Error::Backend(
                "cannot verify conventional cleanup authority beside a non-current marker".into(),
            ));
        }
        self.ensure_vector_projection_ready()?;
        if self.relation_access_level("meta")?.as_deref() != Some("normal")
            || !crate::vector_projection::meta_columns_are_exact(&self.relation_columns("meta")?)
            || !self.normal_secondary_indices_are_exact("meta", &[])?
            || !self.relation_has_no_triggers("meta")?
        {
            return Err(Error::Backend(
                "current conventional metadata relation is not exact".into(),
            ));
        }
        if self.relation_access_level("node")?.as_deref() != Some("normal")
            || !crate::tag_projection::canonical_node_columns_are_exact(
                &self.relation_columns("node")?,
            )
            || !self.normal_secondary_indices_are_exact("node", &[])?
            || !self.relation_has_no_triggers("node")?
        {
            return Err(Error::Backend(
                "current conventional canonical node relation is not exact".into(),
            ));
        }
        let vector_columns = self.relation_columns("node_vec")?;
        if self.relation_access_level("node_vec")?.as_deref() != Some("normal")
            || !crate::vector_projection::legacy_vector_columns_are_recognized(&vector_columns)
            || !self.vector_relation_has_status("node_vec")?
            || vector_dim_from_columns(&vector_columns)? != self.dim
            || !self.relation_has_no_triggers("node_vec")?
            || !self.hnsw_indices_are_exact(
                "node_vec",
                &[
                    ACTIVE_VECTOR_INDEX,
                    CANDIDATE_VECTOR_INDEX,
                    ARCHIVED_VECTOR_INDEX,
                ],
            )?
        {
            return Err(Error::Backend(
                "current conventional vector relation or HNSW catalog is not exact".into(),
            ));
        }
        self.require_matching_stored_dim("verify current conventional generation")?;

        match self.read_meta("db_id")? {
            Some(db_id) => {
                parse_canonical_database_id(&db_id).map_err(|error| {
                    backend_str(format!(
                        "current conventional metadata db_id {}",
                        error.detail()
                    ))
                })?;
            }
            None if !require_db_id => {}
            None => {
                return Err(Error::Backend(
                    "current conventional metadata is missing db_id".into(),
                ));
            }
        }

        let nodes = self.relation_count("node", "id")?;
        let memberships = self.relation_count("node_tag_v2", "tag")?;
        self.verify_membership_upgrade_projection("node_tag_v2", memberships, true)?;
        self.validate_vector_upgrade_source(true)?;
        let vectors = self.validate_vector_values()?;
        for (index, status) in vector_lanes() {
            self.verify_shadow_hnsw("node_vec", index, status)?;
        }
        Ok((nodes, vectors, memberships))
    }

    /// Validate older databases once, then stamp the invariant version. A full
    /// edge scan on every one-shot CLI open would turn startup into O(E), while
    /// all supported writes after this marker are transactionally guarded.
    #[cfg(test)]
    fn ensure_incident_degree_bound(&self) -> Result<()> {
        let expected = MAX_INCIDENT_EDGES.to_string();
        if self.read_meta(INCIDENT_EDGE_CAP_META_KEY)?.as_deref() == Some(expected.as_str()) {
            return Ok(());
        }
        let mut params = BTreeMap::new();
        params.insert("cap".into(), dv_int(MAX_INCIDENT_EDGES as i64));
        let rows = self.run(
            "incident[node, edge_from, edge_to] := \
               *edge{from: edge_from, to: edge_to}, node = edge_from\n\
             incident[node, edge_from, edge_to] := \
               *edge{from: edge_from, to: edge_to}, node = edge_to, edge_from != edge_to\n\
             degree[node, count(edge_from)] := incident[node, edge_from, edge_to]\n\
             ?[node, n] := degree[node, n], n > $cap :limit 1",
            params,
            false,
        )?;
        if !rows.rows.is_empty() {
            return Err(crate::incident_edge_capacity_error());
        }
        self.put_meta(INCIDENT_EDGE_CAP_META_KEY, &expected)
    }

    /// Validate pre-invariant remote overlays once, then stamp the source-owned
    /// cap. Supported writes are guarded transactionally after this marker, so
    /// ordinary reopen remains O(1); legacy databases pay one grouped scan.
    #[cfg(test)]
    fn ensure_remote_edge_source_bound(&self) -> Result<()> {
        let expected = MAX_REMOTE_EDGES_PER_SOURCE.to_string();
        if self.read_meta(REMOTE_EDGE_SOURCE_CAP_META_KEY)?.as_deref() == Some(expected.as_str()) {
            return Ok(());
        }
        let mut params = BTreeMap::new();
        params.insert("cap".into(), dv_int(MAX_REMOTE_EDGES_PER_SOURCE as i64));
        let rows = self.run(
            "source_degree[from, count(target)] := \
               *remote_edge{from, target_db, target}\n\
             ?[from, n] := source_degree[from, n], n > $cap :limit 1",
            params,
            false,
        )?;
        if !rows.rows.is_empty() {
            return Err(crate::remote_edge_source_capacity_error());
        }
        self.put_meta(REMOTE_EDGE_SOURCE_CAP_META_KEY, &expected)
    }

    #[cfg(test)]
    fn ensure_legacy_shadow_guard_ready(&self) -> Result<()> {
        self.ensure_legacy_shadow_guard_generation(PERMANENT_VECTOR_GUARD_VALUE)
    }

    #[cfg(test)]
    fn ensure_prior_legacy_shadow_guard_ready(&self) -> Result<()> {
        self.ensure_legacy_shadow_guard_generation(crate::vector_projection::PRIOR_GUARD_VALUE)
    }

    #[cfg(test)]
    fn ensure_predecessor_guard_ready(&self) -> Result<()> {
        self.ensure_legacy_shadow_guard_generation(crate::vector_projection::GUARD_VALUE)
    }

    #[cfg(test)]
    fn ensure_legacy_shadow_guard_generation(&self, expected: &str) -> Result<()> {
        use crate::vector_projection::{GUARD_KEY, LEGACY_SHADOW_GUARD, guard_columns_are_exact};

        let access = self.relation_access_level(LEGACY_SHADOW_GUARD)?;
        if access.as_deref() != Some("read_only") {
            return Err(Error::Backend(format!(
                "vector projection v3 old-writer fence {LEGACY_SHADOW_GUARD:?} is missing or not read_only"
            )));
        }
        let columns = self.relation_columns(LEGACY_SHADOW_GUARD)?;
        if !guard_columns_are_exact(&columns) {
            return Err(Error::Backend(format!(
                "vector projection v3 old-writer fence {LEGACY_SHADOW_GUARD:?} has an unexpected schema"
            )));
        }
        if !self.normal_secondary_indices_are_exact(LEGACY_SHADOW_GUARD, &[])? {
            return Err(Error::Backend(format!(
                "vector projection v3 old-writer fence {LEGACY_SHADOW_GUARD:?} has unexpected child indexes"
            )));
        }
        if !self.relation_has_no_triggers(LEGACY_SHADOW_GUARD)? {
            return Err(Error::Backend(format!(
                "vector projection v3 old-writer fence {LEGACY_SHADOW_GUARD:?} has unexpected triggers"
            )));
        }
        let rows = self.run(
            &format!(
                "?[fence, generation] := *{LEGACY_SHADOW_GUARD}{{fence, generation}} :limit 2"
            ),
            BTreeMap::new(),
            false,
        )?;
        let valid = rows.rows.as_slice() == [vec![dv_str(GUARD_KEY), dv_str(expected)]];
        if !valid {
            return Err(Error::Backend(format!(
                "vector projection v3 old-writer fence {LEGACY_SHADOW_GUARD:?} does not carry expected generation {expected:?}"
            )));
        }
        Ok(())
    }

    #[cfg(test)]
    fn drop_hnsw_indices(&self, relation: &str) -> Result<()> {
        let rows = self.run(&format!("::indices {relation}"), BTreeMap::new(), false)?;
        for row in rows.rows {
            if row.len() < 2 || want_str(&row[1])? != "hnsw" {
                continue;
            }
            let local = want_str(&row[0])?;
            let mut names = Vec::new();
            if let Some(DataValue::List(backing)) = row.get(2) {
                for value in backing {
                    if let Ok(name) = want_str(value) {
                        names.push(name.to_string());
                    }
                }
            }
            names.push(format!("{relation}:{local}"));
            names.sort();
            names.dedup();

            let mut dropped = false;
            let mut last_error = None;
            for name in names {
                match self.run(&format!("::hnsw drop {name}"), BTreeMap::new(), true) {
                    Ok(_) => {
                        dropped = true;
                        break;
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            if !dropped {
                return Err(last_error.unwrap_or_else(|| {
                    Error::Backend(format!(
                        "cannot drop HNSW index {local:?} from relation {relation:?}"
                    ))
                }));
            }
        }
        Ok(())
    }

    /// Refuse legacy vector layouts and pre-contract node blobs during ordinary open.
    /// These older generations are outside the successor migration horizon; this
    /// historical detector performs bounded probes without changing their rows.
    #[cfg(test)]
    fn ensure_vector_projection_ready(&self) -> Result<()> {
        match self.storage_contract_generation()? {
            StorageContractGeneration::ConventionalUnmanaged => {}
            StorageContractGeneration::ManagedV1(head) => {
                return Err(managed_v1_entry_refused("open", head));
            }
            StorageContractGeneration::CanonicalNodeV1 => {
                self.ensure_predecessor_guard_ready()?;
                return Err(storage_contract_upgrade_required());
            }
            StorageContractGeneration::VectorV3 => {
                // A real prior-v3 store carries the prior permanent guard. A
                // current guard paired with old metadata is a torn/foreign
                // generation, not an upgradeable legacy state.
                self.ensure_prior_legacy_shadow_guard_ready()?;
                return Err(storage_contract_upgrade_required());
            }
            StorageContractGeneration::LegacyVector => {
                return Err(storage_contract_upgrade_required());
            }
        }
        self.ensure_vector_projection_physical_ready(
            "current conventional combined generation",
            PERMANENT_VECTOR_GUARD_VALUE,
        )?;
        self.ensure_tag_projection_ready()
    }

    #[cfg(test)]
    fn ensure_tag_projection_ready(&self) -> Result<()> {
        use crate::tag_projection::{
            legacy_writable_columns_are_exact, membership_columns_are_exact,
        };

        if !self.relation_exists("node_tag_v2")?
            || self.relation_access_level("node_tag_v2")?.as_deref() != Some("normal")
            || !membership_columns_are_exact(&self.relation_columns("node_tag_v2")?)
            || !self
                .normal_secondary_indices_are_exact("node_tag_v2", &[("by_id", &[3, 0, 1, 2])])?
            || !self.relation_has_no_triggers("node_tag_v2")?
        {
            return Err(Error::Backend(
                "current conventional marker exists but node_tag_v2 has an unexpected schema"
                    .into(),
            ));
        }
        if self.relation_access_level("node_tag")?.as_deref() != Some("read_only")
            || !legacy_writable_columns_are_exact(&self.relation_columns("node_tag")?)
            || !self.normal_secondary_indices_are_exact("node_tag", &[("by_tag", &[1, 0])])?
            || !self.relation_has_no_triggers("node_tag")?
        {
            return Err(Error::Backend(
                "current conventional legacy node_tag old-writer fence is missing or malformed"
                    .into(),
            ));
        }
        if self.relation_count("node_tag", "id")? != 0 {
            return Err(Error::Backend(
                "current conventional legacy node_tag old-writer fence is not empty with its by_tag child"
                    .into(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn ensure_vector_projection_physical_ready(
        &self,
        generation: &str,
        guard_generation: &str,
    ) -> Result<()> {
        self.ensure_legacy_shadow_guard_generation(guard_generation)?;
        if !self.vector_relation_has_status("node_vec")? {
            return Err(Error::Backend(format!(
                "{generation} vector marker exists but node_vec has no lifecycle column"
            )));
        }
        if !self.vector_lane_indices_ready("node_vec")? {
            return Err(Error::Backend(format!(
                "{generation} vector marker exists but one or more lifecycle HNSW indexes are missing"
            )));
        }
        let physical = self.canonical_vector_dim()?;
        if let Some(stored) = self.read_meta("dim")? {
            let stored = stored
                .parse::<usize>()
                .map_err(|error| backend_str(format!("bad stored dim {stored:?}: {error}")))?;
            if stored != physical {
                return Err(Error::Backend(format!(
                    "{generation} vector metadata dimension {stored} does not match physical node_vec dimension {physical}"
                )));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn vector_relation_has_status(&self, relation: &str) -> Result<bool> {
        Ok(self
            .run(
                &format!("?[id, status] := *{relation}{{id, status}} :limit 0"),
                BTreeMap::new(),
                false,
            )
            .is_ok())
    }

    #[cfg(test)]
    fn vector_lane_indices_ready(&self, relation: &str) -> Result<bool> {
        let rows = match self.run(&format!("::indices {relation}"), BTreeMap::new(), false) {
            Ok(rows) => rows,
            Err(_) => return Ok(false),
        };
        let mut found = HashSet::new();
        for row in &rows.rows {
            if row.len() >= 2 && want_str(&row[1])? == "hnsw" {
                found.insert(want_str(&row[0])?.to_string());
            }
        }
        Ok([
            ACTIVE_VECTOR_INDEX,
            CANDIDATE_VECTOR_INDEX,
            ARCHIVED_VECTOR_INDEX,
        ]
        .into_iter()
        .all(|name| found.contains(name)))
    }

    /// The anchor span recorded for a directed edge, if any.
    async fn get_anchor(&self, from: NodeId, to: NodeId) -> Result<Option<BodySpan>> {
        let mut p = BTreeMap::new();
        p.insert("from".into(), dv_str(&from.0.to_string()));
        p.insert("to".into(), dv_str(&to.0.to_string()));
        let rows = self
            .run_async(
                "?[start, end] := *edge_anchor{from: $from, to: $to, start, end}".into(),
                p,
                false,
            )
            .await?;
        match rows.rows.into_iter().next() {
            Some(r) => Ok(Some(graph_records::decode_body_span(&r[0], &r[1])?)),
            None => Ok(None),
        }
    }

    /// All anchors keyed by directed pair — one query, for attaching to a batch
    /// of edges (neighbors / all_edges) without a lookup each.
    fn anchor_map(&self) -> Result<HashMap<(NodeId, NodeId), BodySpan>> {
        let rows = self.run(
            "?[from, to, start, end] := *edge_anchor{from, to, start, end}",
            BTreeMap::new(),
            false,
        )?;
        let mut m = HashMap::new();
        for r in &rows.rows {
            m.insert(
                (node_id(want_str(&r[0])?)?, node_id(want_str(&r[1])?)?),
                graph_records::decode_body_span(&r[2], &r[3])?,
            );
        }
        Ok(m)
    }

    /// Anchors for a bounded set of directed edge pairs. Unlike [`Self::anchor_map`],
    /// this is safe on the retrieval path: the keyed join cannot grow with the
    /// number of unrelated anchors in the store.
    async fn anchors_for(
        &self,
        pairs: &[(NodeId, NodeId)],
    ) -> Result<HashMap<(NodeId, NodeId), BodySpan>> {
        if pairs.is_empty() {
            return Ok(HashMap::new());
        }

        let mut pairs = pairs.to_vec();
        pairs.sort_unstable();
        pairs.dedup();
        let (input, params) = edge_pair_input("wanted", &pairs);
        let script = format!(
            "{input}\n?[from, to, start, end] := wanted[from, to], \
             *edge_anchor{{from, to, start, end}}"
        );
        let rows = self.run_async(script, params, false).await?;
        let mut anchors = HashMap::with_capacity(rows.rows.len());
        for row in &rows.rows {
            anchors.insert(
                (node_id(want_str(&row[0])?)?, node_id(want_str(&row[1])?)?),
                graph_records::decode_body_span(&row[2], &row[3])?,
            );
        }
        Ok(anchors)
    }

    /// Copy a snapshot-backed [`MemStore`](crate::MemStore)'s complete export into
    /// this store. The destination must be a fresh, empty database: importing over
    /// live data is ambiguous and used to silently merge unrelated identities.
    pub async fn import_mem(&mut self, mem: &crate::MemStore) -> Result<usize> {
        self.ensure_empty_import_target()?;
        let export = mem.export();
        if !matches!(
            self.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref(),
            Some(
                EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                    | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
            )
        ) {
            for node in &export.nodes {
                let raw = serde_json::to_value(node)
                    .map_err(|error| Error::InvalidInput(error.to_string()))?;
                crate::validate_pre_context_node_value(&raw)?;
            }
        }
        if export
            .nodes
            .iter()
            .any(|node| matches!(node.provenance(), Provenance::External { .. }))
            && !matches!(
                self.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref(),
                Some(
                    SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER
                        | CONCERN_V1_CATALOG_GENERATION_MARKER
                        | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                        | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
                )
            )
        {
            return Err(Error::InvalidInput(
                "external capture provenance requires a capture-enabled store; refusing ordinary-generation import".into(),
            ));
        }
        if export.dim != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: export.dim,
            });
        }
        if let Some(fingerprint) = &export.embedding_fingerprint {
            fingerprint
                .validate()
                .map_err(Error::InvalidEmbeddingFingerprint)?;
            if fingerprint.dimension != export.dim {
                return Err(Error::DimMismatch {
                    index: export.dim,
                    provider: fingerprint.dimension,
                });
            }
        }
        crate::validate_graph_import(&export, export.db_id)?;
        crate::validate_concern_import(&export)?;
        crate::mem_touchstones::validate_touchstone_import(&export)?;
        if !export.touchstones.is_empty() && !self.touchstones_generation()? {
            return Err(Error::InvalidInput(
                "touchstones require touchstones-v1 generation".into(),
            ));
        }
        if !export.concerns.is_empty()
            && !matches!(
                self.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref(),
                Some(
                    CONCERN_V1_CATALOG_GENERATION_MARKER
                        | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                        | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
                )
            )
        {
            return Err(Error::InvalidInput(
                "concerns require concern generation".into(),
            ));
        }
        crate::validate_episode_import(&export)?;
        if export.nodes.iter().any(|node| !node.is_semantic())
            && !matches!(
                self.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref(),
                Some(
                    SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER
                        | CONCERN_V1_CATALOG_GENERATION_MARKER
                        | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                        | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
                )
            )
        {
            return Err(Error::InvalidInput(
                "episode import requires episode generation".into(),
            ));
        }
        // Validate the complete source-owned remote-key sets before the first
        // destination mutation. Import is intentionally row-oriented today;
        // without this preflight a late over-cap source would leave a partially
        // populated destination that could not be safely resumed.
        validate_remote_edge_source_bound(&export.remote_edges)?;
        crate::validate_full_merge_import(&export)?;
        crate::validate_supersede_import(&export)?;
        crate::validate_vector_import(&export)?;

        for node in export.nodes.iter().filter(|node| node.is_semantic()) {
            self.put_node_for_import(node, true).await?;
        }
        let semantic_ids: HashSet<_> = export
            .nodes
            .iter()
            .filter(|node| node.is_semantic())
            .map(Node::id)
            .collect();
        for (id, v) in &export.vectors {
            if semantic_ids.contains(id) {
                self.upsert(*id, v).await?;
            }
        }
        self.import_episode_editions(&export)?;
        for edge in &export.edges {
            self.put_edge(edge).await?;
        }
        for c in &export.contradictions {
            self.upsert_contradiction(c)?;
        }
        for m in &export.merges {
            self.upsert_merge(m)?;
        }
        for record in &export.full_merge_commits {
            self.upsert_full_merge_record(record)?;
        }
        for record in &export.supersede_commits {
            self.upsert_supersede_record(record)?;
        }
        for edge in &export.remote_edges {
            self.upsert_remote_edge_for_source_database(edge, export.db_id)?;
        }
        for row in &export.concerns {
            self.import_concern(row)?;
        }
        // Feedback retry proofs are intentionally not migrated. Their receipt
        // capabilities cannot survive the source host's authority epoch.

        // Identity metadata is the commit marker: stamp it only after every row,
        // especially every vector, has succeeded. An interrupted import therefore
        // cannot masquerade as a complete compatible index.
        self.import_touchstones(&export)?;
        self.put_meta("db_id", &export.db_id.to_string())?;
        if let Some(fingerprint) = &export.embedding_fingerprint {
            self.set_embedding_fingerprint(fingerprint)?;
        }
        self.db_id = export.db_id;
        Ok(export.nodes.len())
    }

    /// Re-export every canonical relation and compare it to the source snapshot.
    /// Migration invokes this after closing and reopening sqlite, so it proves the
    /// data made it to disk rather than merely surviving in a connection cache.
    pub async fn verify_import(&self, expected: &crate::StoreExport) -> Result<()> {
        self.verify_episode_import(expected)?;
        let actual = self.export().await?;
        let mut expected = expected.clone();
        expected.feedback_retries.clear();
        let expected_value = canonical_export_value(expected.clone())?;
        let actual_value = canonical_export_value(actual)?;
        if actual_value != expected_value {
            return Err(Error::Backend(format!(
                "migration verification failed: expected {} nodes/{} edges/{} vectors/{} contradictions/{} merges/{} full merges/{} supersedes/{} remote edges/{} feedback retries",
                expected.nodes.len(),
                expected.edges.len(),
                expected.vectors.len(),
                expected.contradictions.len(),
                expected.merges.len(),
                expected.full_merge_commits.len(),
                expected.supersede_commits.len(),
                expected.remote_edges.len(),
                expected.feedback_retries.len(),
            )));
        }
        Ok(())
    }

    fn ensure_empty_import_target(&self) -> Result<()> {
        let probes = [
            "?[id] := *node{id} :limit 1",
            "?[id] := *node_tag_v2:by_id{id} :limit 1",
            "?[id] := *node_vec{id} :limit 1",
            "?[id] := *node_search{id} :limit 1",
            "?[from] := *edge{from} :limit 1",
            "?[from] := *edge_anchor{from} :limit 1",
            "?[lo] := *contradiction{lo} :limit 1",
            "?[lo] := *merge_candidate{lo} :limit 1",
            "?[lo] := *full_merge_commit{lo} :limit 1",
            "?[lo] := *supersede_commit{lo} :limit 1",
            "?[from] := *remote_edge{from} :limit 1",
            "?[key] := *feedback_retry{key} :limit 1",
            "?[key] := *feedback_retry_order{key} :limit 1",
        ];
        let has_rows = probes.iter().try_fold(false, |found, probe| {
            Ok::<_, Error>(found || !self.run(probe, BTreeMap::new(), false)?.rows.is_empty())
        })?;
        let concern_rows = matches!(
            self.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref(),
            Some(
                CONCERN_V1_CATALOG_GENERATION_MARKER
                    | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                    | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
            )
        ) && !self
            .run("?[lo] := *concern{lo} :limit 1", BTreeMap::new(), false)?
            .rows
            .is_empty();
        let touchstone_rows = self.touchstones_generation()?
            && (!self
                .run(
                    "?[owner] := *touchstone{owner} :limit 1",
                    BTreeMap::new(),
                    false,
                )?
                .rows
                .is_empty()
                || !self
                    .run(
                        "?[target] := *touchstone_target{target} :limit 1",
                        BTreeMap::new(),
                        false,
                    )?
                    .rows
                    .is_empty());
        if has_rows || concern_rows || touchstone_rows || self.embedding_fingerprint()?.is_some() {
            return Err(Error::Backend(
                "refusing to import into a non-empty destination".into(),
            ));
        }
        Ok(())
    }

    /// Losslessly export canonical graph data and the live vector projection.
    /// Derived lexical rows are rebuilt from nodes by import and deliberately do
    /// not become a second source of truth.
    pub async fn export(&self) -> Result<crate::StoreExport> {
        self.export_with_nodes(self.all_nodes(ColdPath::acquire()).await?)
            .await
    }

    async fn export_with_nodes(&self, nodes: Vec<Node>) -> Result<crate::StoreExport> {
        let vectors = self
            .run("?[id, e] := *node_vec{id, e}", BTreeMap::new(), false)?
            .rows
            .iter()
            .map(|row| Ok((node_id(want_str(&row[0])?)?, want_vector(&row[1])?)))
            .collect::<Result<Vec<_>>>()?;

        let contradictions = self
            .run(
                "?[lo, hi, observations, first_seen, last_seen, resolution] := \
                 *contradiction{lo, hi, observations, first_seen, last_seen, resolution}",
                BTreeMap::new(),
                false,
            )?
            .rows
            .iter()
            .map(|row| {
                let between = overlay::decode_stored_pair(&row[0], &row[1], "contradiction")?;
                let values = row.get(2..6).ok_or_else(|| {
                    backend_str("stored contradiction export row is truncated".into())
                })?;
                overlay::decode_contradiction_values(between, values)
            })
            .collect::<Result<Vec<_>>>()?;

        let merges = self
            .run(
                "?[lo, hi, observations, first_seen, last_seen, resolution] := \
                 *merge_candidate{lo, hi, observations, first_seen, last_seen, resolution}",
                BTreeMap::new(),
                false,
            )?
            .rows
            .iter()
            .map(|row| {
                let between = overlay::decode_stored_pair(&row[0], &row[1], "merge candidate")?;
                let values = row.get(2..6).ok_or_else(|| {
                    backend_str("stored merge candidate export row is truncated".into())
                })?;
                overlay::decode_merge_candidate_values(between, values)
            })
            .collect::<Result<Vec<_>>>()?;

        let full_merge_commits = self
            .run(
                "?[lo, hi, winner, loser, applied_at] := \
                 *full_merge_commit{lo, hi, winner, loser, applied_at}",
                BTreeMap::new(),
                false,
            )?
            .rows
            .iter()
            .map(|row| {
                let record = FullMergeRecord {
                    between: UnorderedPair(
                        node_id(want_str(&row[0])?)?,
                        node_id(want_str(&row[1])?)?,
                    ),
                    winner: node_id(want_str(&row[2])?)?,
                    loser: node_id(want_str(&row[3])?)?,
                    applied_at: want_i64(&row[4])? as Timestamp,
                };
                record.validate().map_err(Error::InvalidInput)?;
                Ok(record)
            })
            .collect::<Result<Vec<_>>>()?;

        let supersede_commits = self
            .run(
                "?[lo, hi, winner, loser, applied_at] := \
                 *supersede_commit{lo, hi, winner, loser, applied_at}",
                BTreeMap::new(),
                false,
            )?
            .rows
            .iter()
            .map(|row| {
                let record = SupersedeRecord {
                    between: UnorderedPair(
                        node_id(want_str(&row[0])?)?,
                        node_id(want_str(&row[1])?)?,
                    ),
                    winner: node_id(want_str(&row[2])?)?,
                    loser: node_id(want_str(&row[3])?)?,
                    applied_at: want_i64(&row[4])? as Timestamp,
                };
                record.validate().map_err(Error::InvalidInput)?;
                Ok(record)
            })
            .collect::<Result<Vec<_>>>()?;

        let remote_edges = self
            .run(
                "?[from, target_db, target, weight] := \
                 *remote_edge{from, target_db, target, weight}",
                BTreeMap::new(),
                false,
            )?
            .rows
            .iter()
            .map(|row| {
                graph_records::decode_remote_edge(
                    self.db_id,
                    graph_records::decode_canonical_node_id(&row[0], "remote edge source node")?,
                    &row[1],
                    &row[2],
                    &row[3],
                )
            })
            .collect::<Result<Vec<_>>>()?;

        let feedback_retries = self
            .run(
                "?[key, fingerprint, epoch, sequence] := \
                 *feedback_retry{key, fingerprint}, \
                 *feedback_retry_order{epoch, sequence, key}",
                BTreeMap::new(),
                false,
            )?
            .rows
            .iter()
            .map(|row| {
                Ok(crate::FeedbackRetryRecord {
                    key: want_str(&row[0])?.to_string(),
                    fingerprint: want_str(&row[1])?.to_string(),
                    epoch: want_str(&row[2])?.to_string(),
                    sequence: want_i64(&row[3])? as u64,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let export = crate::StoreExport {
            db_id: self.db_id,
            dim: self.dim,
            embedding_fingerprint: self.embedding_fingerprint()?,
            nodes,
            edges: self.all_edges(ColdPath::acquire()).await?,
            vectors,
            contradictions,
            merges,
            full_merge_commits,
            supersede_commits,
            remote_edges,
            feedback_retries,
            touchstones: self.export_touchstones()?,
            concerns: if matches!(
                self.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref(),
                Some(
                    CONCERN_V1_CATALOG_GENERATION_MARKER
                        | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                        | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
                )
            ) {
                self.export_concerns()?
            } else {
                Vec::new()
            },
        };
        // Inherited export reads several snapshots, not one global snapshot.
        // A concurrent owner capture/delete may therefore require retry. Never
        // acknowledge a torn typed owner/record/vector join as a valid export.
        crate::mem_touchstones::validate_touchstone_import(&export)?;
        Ok(export)
    }

    /// Whether the canonical relation already exists in a nonempty Mneme
    /// catalog. `false` means the catalog is genuinely empty and is therefore
    /// the only state ordinary open may initialize.
    ///
    /// Treating "node is absent" as equivalent to "fresh" is unsafe: an
    /// interrupted, managed-only, or foreign catalog can contain durable rows
    /// that the conventional schema does not name. Installing around them would mutate
    /// evidence before the later generation checks finally reject the mix.
    #[cfg(test)]
    fn has_schema(&self) -> Result<bool> {
        let rows = self.run("::relations", BTreeMap::new(), false)?;
        if rows.rows.is_empty() {
            return Ok(false);
        }
        if rows
            .rows
            .iter()
            .any(|r| matches!(r.first(), Some(DataValue::Str(s)) if s.as_str() == "node"))
        {
            return Ok(true);
        }
        Err(Error::InvalidInput(
            "refusing to initialize a nonempty Cozo catalog without the canonical node relation; the database is partial, foreign, or managed and must be inspected or repaired explicitly"
                .into(),
        ))
    }

    fn run(
        &self,
        script: &str,
        params: BTreeMap<String, DataValue>,
        mutable: bool,
    ) -> Result<NamedRows> {
        #[cfg(test)]
        self.query_count.fetch_add(1, AtomicOrdering::Relaxed);
        run_db(&self.db, script, params, mutable)
    }

    /// Execute synchronous Cozo work away from the async runtime. Retrieval is
    /// latency-sensitive and may share the runtime with unrelated MCP requests;
    /// a SQLite read or lock backoff must not park a Tokio worker thread.
    async fn run_async(
        &self,
        script: String,
        params: BTreeMap<String, DataValue>,
        mutable: bool,
    ) -> Result<NamedRows> {
        #[cfg(test)]
        self.query_count.fetch_add(1, AtomicOrdering::Relaxed);
        // The outer future can be cancelled after `spawn_blocking` has detached
        // its closure. Keep this guard *inside* that closure so a lease-owning
        // host still observes the backend as live until Cozo really returns.
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        tokio::task::spawn_blocking(move || job.run(|db| run_db(db, &script, params, mutable)))
            .await
            .map_err(|error| backend_str(format!("join cozo query worker: {error}")))?
    }

    /// Synchronous counterpart used only by lease-held offline migrations.
    /// The direct range scan avoids routing each cursor page through a global
    /// CozoScript `:order`, which would make a nominally paged migration O(N^2).
    fn scan_primary_key(
        &self,
        relation: &'static str,
        scan: PrimaryKeyScan,
    ) -> Result<PrimaryKeyScanPage> {
        #[cfg(test)]
        self.query_count.fetch_add(1, AtomicOrdering::Relaxed);
        let limit = scan.limit;
        let page = self
            .db
            .scan_relation_by_primary_key(relation, &scan)
            .map_err(backend)?;
        validate_primary_key_scan_page(relation, limit, page)
    }

    /// Run one storage-native bounded primary-key page away from the async
    /// runtime. Unlike an ordered CozoScript query, this seeks the physical key
    /// range directly and cannot materialize a whole-relation sorter first.
    async fn scan_primary_key_async(
        &self,
        relation: &'static str,
        scan: PrimaryKeyScan,
    ) -> Result<PrimaryKeyScanPage> {
        #[cfg(test)]
        self.query_count.fetch_add(1, AtomicOrdering::Relaxed);
        let limit = scan.limit;
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        let page = tokio::task::spawn_blocking(move || {
            job.run(|db| {
                db.scan_relation_by_primary_key(relation, &scan)
                    .map_err(backend)
            })
        })
        .await
        .map_err(|error| backend_str(format!("join primary-key scan worker: {error}")))??;
        validate_primary_key_scan_page(relation, limit, page)
    }

    #[cfg(test)]
    fn reset_query_count(&self) {
        self.query_count.store(0, AtomicOrdering::Relaxed);
    }

    #[cfg(test)]
    fn query_count(&self) -> usize {
        self.query_count.load(AtomicOrdering::Relaxed)
    }

    #[cfg(test)]
    fn last_maintenance_statements(&self) -> usize {
        self.last_maintenance_statements
            .load(AtomicOrdering::Relaxed)
    }

    /// Fetch lifecycle status for an explicit bounded id set. The inline input
    /// relation makes Cozo perform key lookups; `is_in(id, $ids)` would apply a
    /// predicate after scanning the base relation.
    async fn statuses_for(&self, ids: &[NodeId]) -> Result<HashMap<NodeId, NodeStatus>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let (input, params) = id_input("wanted", ids);
        // node_search is the transactionally maintained semantic-membership
        // projection. Join it before returning canonical lifecycle flags; raw
        // episode editions remain accessible through get_nodes, not this lane.
        let script = format!(
            "{input}\n?[id, status] := wanted[id], *node{{id, status}}, *node_search{{id}}"
        );
        let rows = self.run_async(script, params, false).await?;
        rows.rows
            .iter()
            .map(|row| {
                Ok((
                    node_id(want_str(&row[0])?)?,
                    node_status_from(want_str(&row[1])?)?,
                ))
            })
            .collect()
    }

    /// Fetch vectors for an explicit bounded id set, again as indexed point
    /// lookups. Missing vectors are intentionally absent and receive semantic
    /// similarity zero in the caller.
    async fn vectors_for(&self, ids: &[NodeId]) -> Result<HashMap<NodeId, Vec<f32>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let (input, params) = id_input("wanted", ids);
        let script = format!("{input}\n?[id, e] := wanted[id], *node_vec{{id, e}}");
        let rows = self.run_async(script, params, false).await?;
        rows.rows
            .iter()
            .map(|row| Ok((node_id(want_str(&row[0])?)?, want_vector(&row[1])?)))
            .collect()
    }

    /// Fetch every incident edge for a whole traversal frontier in one Cozo
    /// query. Outgoing lookups use the base `(from,to)` key; incoming lookups use
    /// the `edge:by_to` secondary index, so neither rule scans the graph.
    async fn incident_for(&self, frontier: &[NodeId]) -> Result<HashMap<NodeId, Vec<Edge>>> {
        if frontier.is_empty() {
            return Ok(HashMap::new());
        }
        let (input, mut params) = id_input("frontier", frontier);
        // Supported stores can return at most `frontier.len() * cap` rows. One
        // extra slot per source is a bounded corruption canary: if the query
        // fills this limit, pigeonhole guarantees some source crosses the cap;
        // if it does not, the whole result was bounded anyway.
        let read_cap = frontier
            .len()
            .saturating_mul(MAX_INCIDENT_EDGES + 1)
            .min(i64::MAX as usize) as i64;
        params.insert("read_cap".into(), dv_int(read_cap));
        let script = format!(
            "{input}\n\
             incident[src, src, edge_to, weight, kind, lr, tr, intf] := \
               frontier[src], \
               *edge{{from: src, to: edge_to, weight, kind, last_reinforced: lr, trials: tr, interference: intf}}\n\
             incident[src, edge_from, src, weight, kind, lr, tr, intf] := \
               frontier[src], \
               *edge{{to: src, from: edge_from, weight, kind, last_reinforced: lr, trials: tr, interference: intf}}\n\
             ?[src, from, to, weight, kind, lr, tr, intf] := \
               incident[src, from, to, weight, kind, lr, tr, intf] :limit $read_cap"
        );
        let rows = self.run_async(script, params, false).await?;
        let mut out: HashMap<NodeId, Vec<Edge>> = HashMap::new();
        for row in &rows.rows {
            let source = node_id(want_str(&row[0])?)?;
            let from = node_id(want_str(&row[1])?)?;
            let to = node_id(want_str(&row[2])?)?;
            let incident = out.entry(source).or_default();
            incident.push(graph_records::row_to_edge(from, to, &row[3..])?);
            if incident.len() > MAX_INCIDENT_EDGES {
                return Err(crate::incident_edge_capacity_error());
            }
        }
        Ok(out)
    }

    /// Orient, lifecycle-filter, sort and cap a whole frontier's neighbours.
    /// Status filtering precedes `SPREAD_FANOUT`, preventing forbidden nodes from
    /// affecting even the ranking of allowed paths.
    async fn scoped_layer(
        &self,
        frontier: &[NodeId],
        scope: TraversalScope,
        candidate_cap: usize,
        routing_biases: Option<&crate::routing_probe::RoutingBiasMap>,
    ) -> Result<HashMap<NodeId, Vec<Neighbor>>> {
        let incident = self.incident_for(frontier).await?;
        let mut oriented: HashMap<NodeId, Vec<Neighbor>> = frontier
            .iter()
            .map(|id| {
                (
                    *id,
                    crate::oriented_neighbors(*id, incident.get(id).map_or(&[], Vec::as_slice)),
                )
            })
            .collect();

        let mut target_ids: Vec<NodeId> = oriented
            .values()
            .flatten()
            .map(|neighbor| neighbor.node)
            .collect();
        target_ids.sort();
        target_ids.dedup();
        let statuses = self.statuses_for(&target_ids).await?;

        for (source, neighbors) in &mut oriented {
            neighbors.retain(|neighbor| {
                statuses
                    .get(&neighbor.node)
                    .is_some_and(|status| scope.allows(neighbor.node, *status))
            });
            if let Some(biases) = routing_biases.filter(|biases| !biases.is_empty()) {
                neighbors.sort_by(|a, b| {
                    crate::routing_probe::biased_neighbor_order(*source, a, 1.0, b, 1.0, biases)
                });
            } else {
                neighbors.sort_by(crate::neighbor_order);
            }
            neighbors.truncate(candidate_cap);
        }
        Ok(oriented)
    }

    /// Read-modify-write a node through its encapsulated API.
    async fn mutate_node(&self, id: NodeId, f: impl FnOnce(&mut Node)) -> Result<()> {
        let mut node = self.get_node(id).await?.ok_or(Error::NotFound)?;
        f(&mut node);
        self.put_node(&node).await
    }

    fn upsert_remote_edge_for_source_database(
        &self,
        edge: &RemoteEdge,
        source_db: Ulid,
    ) -> Result<()> {
        edge.validate_for_source_database(source_db)
            .map_err(Error::InvalidInput)?;
        let weight =
            RemoteEdge::new(edge.from, edge.target_db, edge.target, edge.weight()).weight();
        let mut params = BTreeMap::new();
        params.insert("from".into(), dv_str(&edge.from.0.to_string()));
        params.insert("target_db".into(), dv_str(&edge.target_db.to_string()));
        params.insert("target".into(), dv_str(&edge.target.0.to_string()));
        params.insert("weight".into(), dv_float(weight as f64));
        params.insert("cap".into(), dv_int(MAX_REMOTE_EDGES_PER_SOURCE as i64));
        // Count and write share one Cozo transaction. Exact-key replacement is
        // legal at the ceiling; only a new `(target_db, target)` needs a source
        // slot. SQLite conflict retries re-run the complete guard and put.
        let result = self.run(
            "{source_key[target_db, target] := \
                *remote_edge{from: $from, target_db, target}\n\
              source_degree[count(target)] := source_key[target_db, target]\n\
              existing[target] := source_key[target_db, target], \
                target_db == $target_db, target == $target\n\
              admissible[one] := existing[target], one = 1\n\
              admissible[n] := source_degree[n], n < $cap\n\
              ?[ok] := admissible[ok] :assert some}\n\
             {?[from, target_db, target, weight] <- \
                [[$from, $target_db, $target, $weight]] \
              :put remote_edge {from, target_db, target => weight}}",
            params,
            true,
        );
        result.map_err(|error| match error {
            Error::Backend(message) if message.contains("asserted to return some") => {
                crate::remote_edge_source_capacity_error()
            }
            other => other,
        })?;
        Ok(())
    }

    fn upsert_full_merge_record(&self, record: &FullMergeRecord) -> Result<()> {
        record.validate().map_err(Error::InvalidInput)?;
        let (lo, hi) = canonical(record.between);
        let mut params = BTreeMap::new();
        params.insert("lo".into(), dv_str(&lo));
        params.insert("hi".into(), dv_str(&hi));
        params.insert("winner".into(), dv_str(&record.winner.0.to_string()));
        params.insert("loser".into(), dv_str(&record.loser.0.to_string()));
        params.insert(
            "applied_at".into(),
            dv_int(i64::try_from(record.applied_at).map_err(|_| {
                Error::InvalidInput("full merge timestamp exceeds the storage integer range".into())
            })?),
        );
        self.run(
            "?[lo, hi, winner, loser, applied_at] <- \
             [[$lo, $hi, $winner, $loser, $applied_at]] \
             :put full_merge_commit {lo, hi => winner, loser, applied_at}",
            params,
            true,
        )?;
        Ok(())
    }

    fn upsert_supersede_record(&self, record: &SupersedeRecord) -> Result<()> {
        record.validate().map_err(Error::InvalidInput)?;
        let (lo, hi) = canonical(record.between);
        let mut params = BTreeMap::new();
        params.insert("lo".into(), dv_str(&lo));
        params.insert("hi".into(), dv_str(&hi));
        params.insert("winner".into(), dv_str(&record.winner.0.to_string()));
        params.insert("loser".into(), dv_str(&record.loser.0.to_string()));
        params.insert(
            "applied_at".into(),
            dv_int(i64::try_from(record.applied_at).map_err(|_| {
                Error::InvalidInput("supersede timestamp exceeds the storage integer range".into())
            })?),
        );
        self.run(
            "?[lo, hi, winner, loser, applied_at] <- \
             [[$lo, $hi, $winner, $loser, $applied_at]] \
             :put supersede_commit {lo, hi => winner, loser, applied_at}",
            params,
            true,
        )?;
        Ok(())
    }
}

impl EmbeddingMetadataStore for CozoStore {
    fn embedding_fingerprint(&self) -> Result<Option<EmbeddingFingerprint>> {
        self.read_meta(EMBEDDING_FINGERPRINT_META_KEY)?
            .map(|json| {
                serde_json::from_str(&json).map_err(|e| {
                    Error::InvalidEmbeddingFingerprint(format!(
                        "stored {EMBEDDING_FINGERPRINT_META_KEY} is not valid JSON: {e}"
                    ))
                })
            })
            .transpose()
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
        let json = serde_json::to_string(fingerprint)
            .map_err(|e| Error::Backend(format!("serialize embedding fingerprint: {e}")))?;
        self.put_meta(EMBEDDING_FINGERPRINT_META_KEY, &json)
    }

    fn has_embedding_data(&self) -> Result<bool> {
        let nodes = self.run("?[id] := *node{id} :limit 1", BTreeMap::new(), false)?;
        if !nodes.rows.is_empty() {
            return Ok(true);
        }
        let vectors = self.run("?[id] := *node_vec{id} :limit 1", BTreeMap::new(), false)?;
        Ok(!vectors.rows.is_empty())
    }
}

// ---- pure helpers -----------------------------------------------------------

fn tx_run(
    tx: &MultiTransaction,
    script: &str,
    params: BTreeMap<String, DataValue>,
) -> Result<NamedRows> {
    tx.run_script(script, params).map_err(backend)
}

/// Narrow transaction-kernel probe for the first managed-writer cut.
///
/// This stays test-only until production open can retain an authenticated
/// storage identity and every public writer is routed through the same epoch
/// contract. In particular, it is not a generic mutable-script escape hatch.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagedTestFailure {
    None,
    AfterMutationStatement,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ManagedTestMutationOutcome {
    changed: bool,
    epoch: MutationEpoch,
}

/// Delete one local edge plus its durable anchor through the private managed
/// transaction kernel. Public [`GraphStore::delete_edge`] deliberately remains
/// on the conventional-only path.
#[cfg(test)]
fn managed_test_delete_edge(
    store: &CozoStore,
    expected_storage: StorageIdentity,
    from: NodeId,
    to: NodeId,
    failure: ManagedTestFailure,
) -> Result<ManagedTestMutationOutcome> {
    // Mnestic's `MultiTransaction` accepts Datalog programs but not `::columns`,
    // `::indices`, `::show_triggers`, or `::relations`. Check the exact catalog
    // before opening the transaction, then re-read every mutable managed v1 datum from
    // the write snapshot below. This remains test-only because production
    // admission still needs a transaction-scoped catalog API or a stronger DDL
    // exclusion proof to close that narrow check/use gap.
    store.require_exact_managed_store_meta_catalog()?;

    let tx = store.db.multi_transaction(true);
    let staged = (|| {
        let expected_database = DatabaseId::new(store.db_id)
            .map_err(|error| managed_contract_corrupt(format!("invalid store db_id: {error}")))?;
        let head = tx_read_exact_managed_v1_head(&tx)?;
        if head.database_id() != expected_database {
            return Err(managed_contract_corrupt(format!(
                "transaction store_meta db_id {} disagrees with the open database id {expected_database}",
                head.database_id()
            )));
        }
        if head.storage() != Some(expected_storage) {
            let found = head.storage().map_or_else(
                || "missing".to_owned(),
                |storage| format!("{} generation {}", storage.id(), storage.generation().get()),
            );
            return Err(managed_contract_corrupt(format!(
                "transaction storage identity must be {} generation {}, found {found}",
                expected_storage.id(),
                expected_storage.generation().get()
            )));
        }

        let changed = tx_plan_managed_test_edge_delete(&tx, from, to)?;
        let epoch = head.mutation_epoch().ok_or_else(|| {
            managed_contract_corrupt("transaction head unexpectedly lacks a mutation epoch")
        })?;
        if !changed {
            return Ok(ManagedTestMutationOutcome {
                changed: false,
                epoch,
            });
        }

        // Overflow is decided before the first mutation statement. A valid
        // max-epoch store can still service semantic no-ops indefinitely.
        let next_epoch = epoch.checked_successor().map_err(|error| {
            Error::InvalidInput(format!(
                "managed v1 mutation cannot advance its epoch: {error}"
            ))
        })?;
        tx_apply_managed_test_edge_delete(&tx, from, to)?;
        if failure == ManagedTestFailure::AfterMutationStatement {
            tx_run(
                &tx,
                "?[must_be_empty] <- [[1]] :assert none",
                BTreeMap::new(),
            )?;
        }

        // This must remain the final durable statement before commit. It
        // rewrites the exact row observed above and changes only the epoch.
        tx_write_managed_epoch(&tx, head, next_epoch)?;
        Ok(ManagedTestMutationOutcome {
            changed: true,
            epoch: next_epoch,
        })
    })();

    match staged {
        Ok(outcome) => match tx.commit() {
            Ok(()) => Ok(outcome),
            Err(error) => {
                // A failed Mnestic commit already drops the storage
                // transaction. Still exercise the abort channel best-effort;
                // preserve the original backend error if teardown is closed.
                let _ = tx.abort();
                Err(backend(error))
            }
        },
        Err(error) => {
            let _ = tx.abort();
            Err(error)
        }
    }
}

#[cfg(test)]
fn tx_read_exact_managed_v1_head(tx: &MultiTransaction) -> Result<ManagedStoreHead> {
    let rows = tx_run(
        tx,
        "?[storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch] := *store_meta{storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch} :limit 2",
        BTreeMap::new(),
    )?;

    let mut params = BTreeMap::new();
    params.insert("vector_key".into(), dv_str(VECTOR_PROJECTION_META_KEY));
    params.insert("canonical_key".into(), dv_str(CANONICAL_NODE_META_KEY));
    params.insert("tag_key".into(), dv_str(crate::tag_projection::META_KEY));
    params.insert("db_key".into(), dv_str("db_id"));
    let markers = tx_run(
        tx,
        "wanted[k] <- [[$vector_key], [$canonical_key], [$tag_key], [$db_key]]\n\
         ?[k, v] := wanted[k], *meta{k, v} :order k",
        params,
    )?;
    if markers.rows.len() != 4 {
        return Err(managed_contract_corrupt(format!(
            "managed v1 requires exactly four companion metadata rows, found {}",
            markers.rows.len()
        )));
    }

    let mut vector = None;
    let mut canonical = None;
    let mut tag = None;
    let mut database = None;
    for (index, row) in markers.rows.iter().enumerate() {
        if row.len() != 2 {
            return Err(managed_contract_corrupt(format!(
                "companion metadata row {index} returned {} fields, expected 2",
                row.len()
            )));
        }
        let (DataValue::Str(key), DataValue::Str(value)) = (&row[0], &row[1]) else {
            return Err(managed_contract_corrupt(format!(
                "companion metadata row {index} must contain two String fields"
            )));
        };
        let slot = match key.as_str() {
            VECTOR_PROJECTION_META_KEY => &mut vector,
            CANONICAL_NODE_META_KEY => &mut canonical,
            crate::tag_projection::META_KEY => &mut tag,
            "db_id" => &mut database,
            other => {
                return Err(managed_contract_corrupt(format!(
                    "transaction returned an unexpected companion metadata key {}",
                    bounded_metadata(other)
                )));
            }
        };
        if slot.replace(value.as_str()).is_some() {
            return Err(managed_contract_corrupt(format!(
                "transaction returned a duplicate companion metadata key {}",
                bounded_metadata(key.as_str())
            )));
        }
    }

    if vector != Some(MANAGED_V1_VECTOR_PROJECTION_META_VALUE) {
        return Err(managed_contract_corrupt(format!(
            "managed v1 requires vector sentinel {MANAGED_V1_VECTOR_PROJECTION_META_VALUE:?}, found {}",
            bounded_optional_metadata(vector)
        )));
    }
    if canonical != Some(CANONICAL_NODE_META_VALUE) {
        return Err(managed_contract_corrupt(format!(
            "managed v1 requires canonical marker {CANONICAL_NODE_META_VALUE:?}, found {}",
            bounded_optional_metadata(canonical)
        )));
    }
    if tag != Some(crate::tag_projection::META_VALUE) {
        return Err(managed_contract_corrupt(format!(
            "managed v1 requires tag marker {:?}, found {}",
            crate::tag_projection::META_VALUE,
            bounded_optional_metadata(tag)
        )));
    }
    let database =
        database.ok_or_else(|| managed_contract_corrupt("managed v1 requires meta.db_id"))?;
    decode_exact_managed_v1_head(&rows, database)
}

#[cfg(test)]
fn tx_plan_managed_test_edge_delete(
    tx: &MultiTransaction,
    from: NodeId,
    to: NodeId,
) -> Result<bool> {
    let mut params = BTreeMap::new();
    params.insert("from".into(), dv_str(&from.0.to_string()));
    params.insert("to".into(), dv_str(&to.0.to_string()));
    let rows = tx_run(
        tx,
        "present[kind] := *edge{from: $from, to: $to}, kind = 'edge'\n\
         present[kind] := *edge_anchor{from: $from, to: $to}, kind = 'edge_anchor'\n\
         ?[kind] := present[kind] :order kind",
        params,
    )?;
    if rows.rows.len() > 2
        || rows
            .rows
            .iter()
            .any(|row| !matches!(row.as_slice(), [DataValue::Str(_)]))
    {
        return Err(managed_contract_corrupt(
            "managed edge-delete plan returned an impossible presence shape",
        ));
    }
    Ok(!rows.rows.is_empty())
}

#[cfg(test)]
fn tx_apply_managed_test_edge_delete(
    tx: &MultiTransaction,
    from: NodeId,
    to: NodeId,
) -> Result<()> {
    let mut params = BTreeMap::new();
    params.insert("from".into(), dv_str(&from.0.to_string()));
    params.insert("to".into(), dv_str(&to.0.to_string()));
    tx_run(
        tx,
        "?[from, to] := *edge{from, to}, from == $from, to == $to :rm edge {from, to}",
        params.clone(),
    )?;
    tx_run(
        tx,
        "?[from, to] := *edge_anchor{from, to}, from == $from, to == $to :rm edge_anchor {from, to}",
        params,
    )?;
    Ok(())
}

#[cfg(test)]
fn tx_write_managed_epoch(
    tx: &MultiTransaction,
    head: ManagedStoreHead,
    mutation_epoch: MutationEpoch,
) -> Result<()> {
    let storage = head.storage().ok_or_else(|| {
        managed_contract_corrupt("transaction head unexpectedly lacks a storage identity")
    })?;
    let managed_schema_version = head.managed_schema_version().ok_or_else(|| {
        managed_contract_corrupt("transaction head unexpectedly lacks a managed schema version")
    })?;
    let minimum_writer_schema = head.minimum_writer_schema().ok_or_else(|| {
        managed_contract_corrupt("transaction head unexpectedly lacks a writer schema version")
    })?;
    let mut params = BTreeMap::new();
    params.insert("storage_id".into(), dv_str(&storage.id().to_string()));
    params.insert("db_id".into(), dv_str(&head.database_id().to_string()));
    params.insert(
        "storage_generation".into(),
        dv_int(storage.generation().get() as i64),
    );
    params.insert(
        "managed_schema_version".into(),
        dv_int(i64::from(managed_schema_version.get())),
    );
    params.insert(
        "minimum_writer_schema".into(),
        dv_int(i64::from(minimum_writer_schema.get())),
    );
    params.insert("writer_fence".into(), dv_str(MANAGED_V1_WRITER_FENCE));
    params.insert("mutation_epoch".into(), dv_int(mutation_epoch.get() as i64));
    tx_run(
        tx,
        "?[storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch] <- [[$storage_id, $db_id, $storage_generation, $managed_schema_version, $minimum_writer_schema, $writer_fence, $mutation_epoch]] :put store_meta {storage_id => db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch}",
        params,
    )?;
    Ok(())
}

fn feedback_conflict(error: Error) -> Error {
    match error {
        Error::Backend(message) if message.to_ascii_lowercase().contains("assert") => {
            Error::Conflict("feedback precondition changed before commit".into())
        }
        other => other,
    }
}

fn feedback_capacity_error(error: Error) -> Error {
    match error {
        Error::Backend(message) if message.to_ascii_lowercase().contains("assert") => {
            crate::incident_edge_capacity_error()
        }
        other => other,
    }
}

fn feedback_node_input(
    tx: &MultiTransaction,
    commit: &FeedbackCommit,
) -> Result<(String, BTreeMap<String, DataValue>)> {
    let ids = commit
        .nodes
        .iter()
        .map(|update| update.expected.id())
        .collect::<Vec<_>>();
    let (wanted, wanted_params) = id_input("wanted_feedback_nodes", &ids);
    let current = tx_run(
        tx,
        &format!(
            "{wanted}\n?[id, data, status] := wanted_feedback_nodes[id], *node{{id, data, status}}"
        ),
        wanted_params,
    )?;
    let mut canonical = HashMap::with_capacity(current.rows.len());
    for row in &current.rows {
        let id = node_id(want_str(&row[0])?)?;
        decode_canonical_node_row(row)?;
        let raw = want_str(&row[1])?.to_owned();
        let status = want_str(&row[2])?.to_owned();
        if canonical.insert(id, (raw, status)).is_some() {
            return Err(backend_str(format!(
                "duplicate canonical node row returned for {} during feedback",
                id.0
            )));
        }
    }

    let mut params = BTreeMap::new();
    let mut rows = Vec::with_capacity(commit.nodes.len());
    for (index, update) in commit.nodes.iter().enumerate() {
        let expected_id = update.expected.id();
        let node_id = expected_id.0.to_string();
        let (stored, projected_status) = canonical.get(&expected_id).ok_or_else(|| {
            Error::Conflict(format!("feedback node {node_id} disappeared before commit"))
        })?;
        let hydrated = decode_canonical_node(expected_id, stored, projected_status)?;
        let mut hydrated = serde_json::to_value(hydrated).map_err(|error| {
            backend_str(format!(
                "normalize feedback node {node_id} precondition: {error}"
            ))
        })?;
        let mut expected = serde_json::to_value(&update.expected).map_err(|error| {
            backend_str(format!(
                "normalize expected feedback node {node_id}: {error}"
            ))
        })?;
        normalize_tag_arrays(&mut hydrated);
        normalize_tag_arrays(&mut expected);
        if hydrated != expected {
            return Err(Error::Conflict(format!(
                "feedback node {node_id} changed before commit"
            )));
        }

        let id = format!("feedback_node_id_{index}");
        let expected = format!("feedback_node_expected_{index}");
        let replacement = format!("feedback_node_replacement_{index}");
        let status = format!("feedback_node_status_{index}");
        params.insert(id.clone(), dv_str(&node_id));
        // Compare the hydrated representation above so rows written by an older
        // schema (and therefore missing serde-default fields) remain writable.
        // The transaction still CASes against the exact raw bytes that it read,
        // preserving protection against a concurrent writer.
        params.insert(expected.clone(), dv_str(stored));
        params.insert(
            replacement.clone(),
            dv_str(&encode_canonical_node(&update.replacement)?),
        );
        params.insert(
            status.clone(),
            dv_str(status_str(update.replacement.status())),
        );
        rows.push(format!("[${id}, ${expected}, ${replacement}, ${status}]"));
    }
    Ok((
        format!(
            "feedback_nodes[id, expected, replacement, status] <- [{}]",
            rows.join(", ")
        ),
        params,
    ))
}

fn feedback_edge_input(commit: &FeedbackCommit) -> (String, BTreeMap<String, DataValue>) {
    let mut params = BTreeMap::new();
    let mut rows = Vec::with_capacity(commit.edges.len());
    for (index, update) in commit.edges.iter().enumerate() {
        let expected = update.expected.as_ref();
        let replacement = &update.replacement;
        let anchor = expected.and_then(|edge| edge.anchor);
        let values = [
            dv_str(&replacement.from.0.to_string()),
            dv_str(&replacement.to.0.to_string()),
            dv_int(i64::from(expected.is_some())),
            dv_float(expected.map_or(0.0, Edge::weight) as f64),
            dv_str(expected.map_or("transition", |edge| edge_kind_str(edge.kind))),
            dv_int(expected.map_or(0, Edge::last_reinforced) as i64),
            dv_int(expected.map_or(0, Edge::trials) as i64),
            dv_int(expected.map_or(0, Edge::interference) as i64),
            dv_int(i64::from(anchor.is_some())),
            dv_int(anchor.map_or(0, |anchor| anchor.start) as i64),
            dv_int(anchor.map_or(0, |anchor| anchor.end) as i64),
            dv_float(replacement.weight() as f64),
            dv_str(edge_kind_str(replacement.kind)),
            dv_int(replacement.last_reinforced() as i64),
            dv_int(replacement.trials() as i64),
            dv_int(replacement.interference() as i64),
        ];
        let names = (0..values.len())
            .map(|column| format!("feedback_edge_{index}_{column}"))
            .collect::<Vec<_>>();
        for (name, value) in names.iter().cloned().zip(values) {
            params.insert(name, value);
        }
        rows.push(format!(
            "[{}]",
            names
                .iter()
                .map(|name| format!("${name}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    (
        format!(
            "feedback_edges[from, to, present, ew, ek, elr, et, ei, ap, ast, aen, nw, nk, nlr, nt, ni] <- [{}]",
            rows.join(", ")
        ),
        params,
    )
}

fn feedback_merge_pair_input(commit: &FeedbackCommit) -> (String, BTreeMap<String, DataValue>) {
    let mut params = BTreeMap::new();
    let mut rows = Vec::with_capacity(commit.merge_observations.len());
    for (index, observation) in commit.merge_observations.iter().enumerate() {
        let (lo, hi) = canonical(observation.between);
        let lo_name = format!("feedback_merge_lo_{index}");
        let hi_name = format!("feedback_merge_hi_{index}");
        params.insert(lo_name.clone(), dv_str(&lo));
        params.insert(hi_name.clone(), dv_str(&hi));
        rows.push(format!("[${lo_name}, ${hi_name}]"));
    }
    (
        format!("feedback_merge_pairs[lo, hi] <- [{}]", rows.join(", ")),
        params,
    )
}

fn feedback_merge_replacement_input(
    tx: &MultiTransaction,
    commit: &FeedbackCommit,
) -> Result<(String, BTreeMap<String, DataValue>)> {
    let (input, params) = feedback_merge_pair_input(commit);
    let reversed = tx_run(
        tx,
        &format!(
            "{input}\n\
             ?[wanted_lo, wanted_hi] := \
               feedback_merge_pairs[wanted_lo, wanted_hi], \
               *merge_candidate{{lo: wanted_hi, hi: wanted_lo}} :limit 1"
        ),
        params.clone(),
    )?;
    if !reversed.rows.is_empty() {
        return Err(backend_str(
            "stored feedback merge candidate endpoints are in reversed physical-key order".into(),
        ));
    }
    let current = tx_run(
        tx,
        &format!(
            "{input}\n\
             ?[lo, hi, observations, first_seen, last_seen, resolution] := \
               feedback_merge_pairs[lo, hi], \
               *merge_candidate{{lo, hi, observations, first_seen, last_seen, resolution}}"
        ),
        params,
    )?;
    let requested = commit
        .merge_observations
        .iter()
        .map(|observation| observation.between)
        .collect::<HashSet<_>>();
    let mut existing = HashMap::with_capacity(current.rows.len());
    for row in current.rows {
        let between = overlay::decode_stored_pair(&row[0], &row[1], "merge candidate")?;
        if !requested.contains(&between) {
            return Err(backend_str(
                "feedback merge read returned an unrequested candidate".into(),
            ));
        }
        let values = row.get(2..6).ok_or_else(|| {
            backend_str("stored feedback merge candidate row is truncated".into())
        })?;
        let candidate = overlay::decode_merge_candidate_values(between, values)?;
        if existing.insert(between, candidate).is_some() {
            return Err(backend_str(
                "feedback merge read returned a duplicate candidate".into(),
            ));
        }
    }

    let mut out_params = BTreeMap::new();
    let mut rows = Vec::with_capacity(commit.merge_observations.len());
    for (index, observation) in commit.merge_observations.iter().enumerate() {
        let (lo, hi) = canonical(observation.between);
        let (mut candidate, existed) = match existing.remove(&observation.between) {
            Some(candidate) => (candidate, true),
            None => (
                MergeCandidate::new(
                    observation.between.0,
                    observation.between.1,
                    commit.applied_at,
                ),
                false,
            ),
        };
        if existed {
            candidate.observe(commit.applied_at);
        }
        candidate.validate().map_err(|error| {
            Error::InvalidInput(format!("feedback merge replacement is invalid: {error}"))
        })?;

        let values = [
            dv_str(&lo),
            dv_str(&hi),
            dv_int(i64::from(candidate.observations)),
            dv_int(graph_records::encode_storage_timestamp(
                candidate.first_seen,
                "feedback merge first_seen",
            )?),
            dv_int(graph_records::encode_storage_timestamp(
                candidate.last_seen,
                "feedback merge last_seen",
            )?),
            candidate
                .resolution
                .map(|resolution| dv_str(merge_resolution_str(resolution)))
                .unwrap_or(DataValue::Null),
        ];
        let names = (0..values.len())
            .map(|column| format!("feedback_merge_{index}_{column}"))
            .collect::<Vec<_>>();
        for (name, value) in names.iter().cloned().zip(values) {
            out_params.insert(name, value);
        }
        rows.push(format!(
            "[{}]",
            names
                .iter()
                .map(|name| format!("${name}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok((
        format!(
            "feedback_merge_replacements[lo, hi, observations, first_seen, last_seen, resolution] <- [{}]",
            rows.join(", ")
        ),
        out_params,
    ))
}

/// Build a small inline relation backed entirely by bound parameters. Callers
/// sort/deduplicate ids first when ordering matters.
fn id_input(name: &str, ids: &[NodeId]) -> (String, BTreeMap<String, DataValue>) {
    let mut params = BTreeMap::new();
    let rows = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let key = format!("id_{index}");
            params.insert(key.clone(), dv_str(&id.0.to_string()));
            format!("[${key}]")
        })
        .collect::<Vec<_>>()
        .join(", ");
    (format!("{name}[id] <- [{rows}]"), params)
}

fn maintenance_overfull_hubs_query(input: &str) -> String {
    format!(
        "{input}\n\
         outgoing[hub, to, kind] := candidate_hub[hub], *edge{{from: hub, to, kind}}, *node_search{{id: hub}}, *node_search{{id: to}}\n\
         incoming[hub, from, kind] := candidate_hub[hub], \
           *edge{{to: hub, from, kind}}, *node_search{{id: hub}}, *node_search{{id: from}}, from != hub\n\
         association[hub, hub, to] := outgoing[hub, to, kind], kind == 'associative'\n\
         association[hub, hub, to] := outgoing[hub, to, kind], kind == 'bridge'\n\
         association[hub, from, hub] := incoming[hub, from, kind], kind == 'associative'\n\
         association[hub, from, hub] := incoming[hub, from, kind], kind == 'bridge'\n\
         degree[hub, count(from)] := association[hub, from, to]\n\
         ?[hub] := degree[hub, n], n > $target :limit $fetch"
    )
}

fn maintenance_edge_read_query(input: &str) -> String {
    format!(
        "{input}\n\
         ?[from, to, weight, kind, lr, trials, interference] := \
           maintenance_edge_key[from, to], \
           *edge{{from, to, weight, kind, last_reinforced: lr, trials, interference}}"
    )
}

fn maintenance_anchor_read_query(input: &str) -> String {
    format!(
        "{input}\n\
         ?[from, to, start, end] := maintenance_anchor_key[from, to], \
           *edge_anchor{{from, to, start, end}}"
    )
}

/// Build a two-column inline relation of directed edge identities.
fn edge_pair_input(
    name: &str,
    pairs: &[(NodeId, NodeId)],
) -> (String, BTreeMap<String, DataValue>) {
    let mut params = BTreeMap::new();
    let rows = pairs
        .iter()
        .enumerate()
        .map(|(index, (from, to))| {
            let from_key = format!("from_{index}");
            let to_key = format!("to_{index}");
            params.insert(from_key.clone(), dv_str(&from.0.to_string()));
            params.insert(to_key.clone(), dv_str(&to.0.to_string()));
            format!("[${from_key}, ${to_key}]")
        })
        .collect::<Vec<_>>()
        .join(", ");
    (format!("{name}[from, to] <- [{rows}]"), params)
}

fn canonical_export_value(export: crate::StoreExport) -> Result<serde_json::Value> {
    export.canonical_value()
}

fn normalize_tag_arrays(value: &mut serde_json::Value) {
    crate::normalize_export_tag_arrays(value);
}

#[cfg(unix)]
fn restrict_persistent_permissions(path: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        backend_str(format!(
            "set persistent db permissions on {path:?}: {error}"
        ))
    })
}

#[cfg(not(unix))]
fn restrict_persistent_permissions(_path: &str) -> Result<()> {
    Ok(())
}

fn scored_rows(rows: NamedRows, k: usize) -> Result<Vec<Scored>> {
    let mut scored = Vec::with_capacity(rows.rows.len().min(k));
    for row in &rows.rows {
        let id = node_id(want_str(&row[0])?)?;
        // Cosine distance is in [0, 2]; similarity = 1 - distance.
        let score = (1.0 - want_f64(&row[1])? as f32).clamp(0.0, 1.0);
        scored.push(Scored { id, score });
        if scored.len() >= k {
            break;
        }
    }
    Ok(scored)
}

#[cfg(test)]
fn create_vector_relation_script(relation: &str, dim: usize) -> String {
    format!(":create {relation} {{id: String => e: <F32; {dim}>, status: String}}")
}

fn vector_lanes() -> [(&'static str, &'static str); 2] {
    [
        (ACTIVE_VECTOR_INDEX, "active"),
        (ARCHIVED_VECTOR_INDEX, "archived"),
    ]
}

fn selected_vector_lanes(status: StatusFilter) -> Vec<(&'static str, &'static str)> {
    vector_lanes()
        .into_iter()
        .filter(|(_, lifecycle)| match *lifecycle {
            "active" => status.active,
            "archived" => status.archived,
            _ => unreachable!(),
        })
        .collect()
}

#[cfg(test)]
fn vector_dim_from_columns(rows: &NamedRows) -> Result<usize> {
    let ty = rows
        .rows
        .iter()
        .find(|row| row.first().and_then(|value| want_str(value).ok()) == Some("e"))
        .and_then(|row| row.get(3))
        .map(want_str)
        .transpose()?
        .ok_or_else(|| Error::Backend("vector relation has no e column".into()))?;
    let (_, suffix) = ty
        .split_once(';')
        .ok_or_else(|| Error::Backend(format!("cannot read vector dimension from {ty:?}")))?;
    suffix
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>()
        .parse::<usize>()
        .map_err(|error| {
            Error::Backend(format!("cannot read vector dimension from {ty:?}: {error}"))
        })
}

#[cfg(test)]
fn storage_contract_upgrade_required() -> Error {
    Error::Backend(
        "older vector/canonical generation is outside the supported single-graph-upgrade horizon; preserve the source and use its matching historical recovery tooling"
            .into(),
    )
}

#[cfg(test)]
fn schema(dim: usize) -> String {
    schema_with_after_contract_markers(dim, "")
}

#[cfg(test)]
fn schema_with_after_contract_markers(dim: usize, after_contract_markers: &str) -> String {
    let canonical = creation::marker_free_schema_script(dim);
    format!(
        "{canonical}\n\
         {{contract[k, v] <- [\
           ['{VECTOR_PROJECTION_META_KEY}', '{CONVENTIONAL_VECTOR_PROJECTION_META_VALUE}'], \
           ['{CANONICAL_NODE_META_KEY}', '{CANONICAL_NODE_META_VALUE}'], \
           ['{}', '{}']]\n\
           ?[k, v] := contract[k, v] :put meta {{k => v}}}}\n\
         {after_contract_markers}",
        crate::tag_projection::META_KEY,
        crate::tag_projection::META_VALUE,
    )
}

#[cfg(test)]
fn schema_with_marker_failure(dim: usize) -> String {
    schema_with_after_contract_markers(dim, "{?[must_be_empty] <- [[1]] :assert none}")
}

fn validate_remote_edge_source_bound(edges: &[RemoteEdge]) -> Result<()> {
    let mut keys_by_source: HashMap<NodeId, HashSet<(Ulid, NodeId)>> = HashMap::new();
    for edge in edges {
        let keys = keys_by_source.entry(edge.from).or_default();
        if keys.insert((edge.target_db, edge.target)) && keys.len() > MAX_REMOTE_EDGES_PER_SOURCE {
            return Err(crate::remote_edge_source_capacity_error());
        }
    }
    Ok(())
}

/// Canonical (lo, hi) string keys for a node pair, lo < hi by ULID.
fn canonical(pair: UnorderedPair<NodeId>) -> (String, String) {
    let (a, b) = (pair.0, pair.1);
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    (lo.0.to_string(), hi.0.to_string())
}

#[cfg(test)]
const MANAGED_STORE_META_COLUMN_HEADERS: [&str; 6] = [
    "column",
    "is_key",
    "index",
    "type",
    "has_default",
    "default_expr",
];

#[cfg(test)]
fn managed_store_meta_columns_are_exact(columns: &NamedRows) -> bool {
    const EXPECTED: [(&str, bool, &str); 7] = [
        ("storage_id", true, "String"),
        ("db_id", false, "String"),
        ("storage_generation", false, "Int"),
        ("managed_schema_version", false, "Int"),
        ("minimum_writer_schema", false, "Int"),
        ("writer_fence", false, "String"),
        ("mutation_epoch", false, "Int"),
    ];

    columns.next.is_none()
        && columns
            .headers
            .iter()
            .map(String::as_str)
            .eq(MANAGED_STORE_META_COLUMN_HEADERS)
        && columns.rows.len() == EXPECTED.len()
        && columns
            .rows
            .iter()
            .zip(EXPECTED)
            .enumerate()
            .all(|(index, (row, (name, is_key, ty)))| {
                row.len() == MANAGED_STORE_META_COLUMN_HEADERS.len()
                    && matches!(row.first(), Some(DataValue::Str(value)) if value.as_str() == name)
                    && matches!(row.get(1), Some(DataValue::Bool(value)) if *value == is_key)
                    && matches!(row.get(2), Some(DataValue::Num(Num::Int(value))) if *value == index as i64)
                    && matches!(row.get(3), Some(DataValue::Str(value)) if value.as_str() == ty)
                    && matches!(row.get(4), Some(DataValue::Bool(false)))
                    && matches!(row.get(5), Some(DataValue::Null))
            })
}

#[cfg(test)]
fn bounded_metadata(value: &str) -> String {
    let mut chars = value.chars();
    let preview = chars
        .by_ref()
        .take(CORRUPT_METADATA_PREVIEW_CHARS)
        .flat_map(char::escape_debug)
        .collect::<String>();
    let suffix = if chars.next().is_some() {
        " (truncated)"
    } else {
        ""
    };
    format!("{} bytes, preview {preview:?}{suffix}", value.len())
}

#[cfg(test)]
fn bounded_optional_metadata(value: Option<&str>) -> String {
    value.map_or_else(|| "missing".into(), bounded_metadata)
}

#[cfg(test)]
fn managed_contract_corrupt(detail: impl Into<String>) -> Error {
    Error::Backend(format!(
        "managed v1 storage contract is corrupt: {}",
        detail.into()
    ))
}

#[cfg(test)]
fn managed_v1_entry_refused(operation: &str, head: ManagedStoreHead) -> Error {
    let database_id = head.database_id();
    let storage = head
        .storage()
        .map(|storage| format!("{} generation {}", storage.id(), storage.generation().get()))
        .unwrap_or_else(|| "missing managed storage identity".into());
    Error::InvalidInput(format!(
        "cannot {operation}: database {database_id} storage {storage} uses managed v1; this production entry refuses managed stores until every durable writer implements the fence and mutation-epoch contract"
    ))
}

#[cfg(test)]
fn decode_exact_managed_v1_head(
    rows: &NamedRows,
    canonical_db_id: &str,
) -> Result<ManagedStoreHead> {
    if rows.rows.len() != 1 {
        return Err(managed_contract_corrupt(format!(
            "store_meta must contain exactly one row, found {}",
            rows.rows.len()
        )));
    }
    let row = &rows.rows[0];
    if row.len() != 7 {
        return Err(managed_contract_corrupt(format!(
            "store_meta query returned {} fields, expected 7",
            row.len()
        )));
    }

    let storage_id = StorageId::new(managed_ulid(row, 0, "storage_id")?)
        .map_err(|error| managed_contract_corrupt(format!("invalid storage_id: {error}")))?;
    let database_id = DatabaseId::new(managed_ulid(row, 1, "db_id")?)
        .map_err(|error| managed_contract_corrupt(format!("invalid db_id: {error}")))?;
    let storage_generation = StorageGeneration::new(managed_u64(row, 2, "storage_generation")?)
        .map_err(|error| {
            managed_contract_corrupt(format!("invalid storage_generation: {error}"))
        })?;
    let managed_schema_version =
        ManagedSchemaVersion::new(managed_u32(row, 3, "managed_schema_version")?).map_err(
            |error| managed_contract_corrupt(format!("invalid managed_schema_version: {error}")),
        )?;
    if managed_schema_version.get() != MANAGED_V1_SCHEMA_VERSION {
        return Err(managed_contract_corrupt(format!(
            "managed v1 requires managed_schema_version {MANAGED_V1_SCHEMA_VERSION}, found {}",
            managed_schema_version.get()
        )));
    }
    let minimum_writer_schema =
        WriterSchemaVersion::new(managed_u32(row, 4, "minimum_writer_schema")?).map_err(
            |error| managed_contract_corrupt(format!("invalid minimum_writer_schema: {error}")),
        )?;
    if minimum_writer_schema.get() != MANAGED_V1_MINIMUM_WRITER_SCHEMA {
        return Err(managed_contract_corrupt(format!(
            "managed v1 requires minimum_writer_schema {MANAGED_V1_MINIMUM_WRITER_SCHEMA}, found {}",
            minimum_writer_schema.get()
        )));
    }
    let writer_fence = managed_string(row, 5, "writer_fence")?;
    if writer_fence != MANAGED_V1_WRITER_FENCE {
        return Err(managed_contract_corrupt(format!(
            "managed v1 requires writer_fence {MANAGED_V1_WRITER_FENCE:?}, found {}",
            bounded_metadata(writer_fence)
        )));
    }
    let mutation_epoch = MutationEpoch::new(managed_u64(row, 6, "mutation_epoch")?)
        .map_err(|error| managed_contract_corrupt(format!("invalid mutation_epoch: {error}")))?;

    let canonical_db_id = parse_managed_ulid("meta.db_id", canonical_db_id)?;
    if canonical_db_id != database_id.get() {
        return Err(managed_contract_corrupt(format!(
            "store_meta db_id {database_id} disagrees with meta.db_id {canonical_db_id}"
        )));
    }

    Ok(ManagedStoreHead::managed(
        database_id,
        StorageIdentity::new(storage_id, storage_generation),
        managed_schema_version,
        minimum_writer_schema,
        mutation_epoch,
    ))
}

#[cfg(test)]
fn managed_value<'a>(row: &'a [DataValue], index: usize, field: &str) -> Result<&'a DataValue> {
    row.get(index).ok_or_else(|| {
        managed_contract_corrupt(format!(
            "store_meta row is missing field {field:?} at index {index}"
        ))
    })
}

#[cfg(test)]
fn managed_string<'a>(row: &'a [DataValue], index: usize, field: &str) -> Result<&'a str> {
    match managed_value(row, index, field)? {
        DataValue::Str(value) => Ok(value.as_str()),
        _ => Err(managed_contract_corrupt(format!(
            "store_meta field {field:?} must be String, found a non-String value"
        ))),
    }
}

#[cfg(test)]
fn managed_i64(row: &[DataValue], index: usize, field: &str) -> Result<i64> {
    match managed_value(row, index, field)? {
        DataValue::Num(Num::Int(value)) => Ok(*value),
        _ => Err(managed_contract_corrupt(format!(
            "store_meta field {field:?} must be an exact Int, found a non-Int value"
        ))),
    }
}

#[cfg(test)]
fn managed_u64(row: &[DataValue], index: usize, field: &str) -> Result<u64> {
    u64::try_from(managed_i64(row, index, field)?).map_err(|_| {
        managed_contract_corrupt(format!(
            "store_meta field {field:?} must be a non-negative storage integer"
        ))
    })
}

#[cfg(test)]
fn managed_u32(row: &[DataValue], index: usize, field: &str) -> Result<u32> {
    u32::try_from(managed_i64(row, index, field)?).map_err(|_| {
        managed_contract_corrupt(format!(
            "store_meta field {field:?} is outside the supported u32 version range"
        ))
    })
}

#[cfg(test)]
fn managed_ulid(row: &[DataValue], index: usize, field: &str) -> Result<Ulid> {
    parse_managed_ulid(field, managed_string(row, index, field)?)
}

#[cfg(test)]
fn parse_managed_ulid(field: &str, value: &str) -> Result<Ulid> {
    let parsed = Ulid::from_string(value).map_err(|_| {
        managed_contract_corrupt(format!(
            "managed metadata field {field:?} has a malformed ULID: {}",
            bounded_metadata(value)
        ))
    })?;
    if parsed.to_string() != value {
        return Err(managed_contract_corrupt(format!(
            "managed metadata field {field:?} must use canonical ULID text, found {}",
            bounded_metadata(value)
        )));
    }
    Ok(parsed)
}

fn dv_str(s: &str) -> DataValue {
    DataValue::Str(s.into())
}
fn dv_int(i: i64) -> DataValue {
    DataValue::Num(Num::Int(i))
}
fn dv_float(f: f64) -> DataValue {
    DataValue::Num(Num::Float(f))
}
fn dv_float_list(v: &[f32]) -> DataValue {
    DataValue::List(
        v.iter()
            .map(|x| DataValue::Num(Num::Float(*x as f64)))
            .collect(),
    )
}

fn want_vector(dv: &DataValue) -> Result<Vec<f32>> {
    match dv {
        DataValue::Vec(Vector::F32(values)) => Ok(values.to_vec()),
        DataValue::Vec(Vector::F64(values)) => {
            Ok(values.iter().map(|value| *value as f32).collect())
        }
        DataValue::List(values) => values
            .iter()
            .map(|value| want_f64(value).map(|value| value as f32))
            .collect(),
        other => Err(backend_str(format!("expected vector, got {other:?}"))),
    }
}

fn want_str(dv: &DataValue) -> Result<&str> {
    match dv {
        DataValue::Str(s) => Ok(s.as_str()),
        other => Err(backend_str(format!("expected string, got {other:?}"))),
    }
}
fn want_i64(dv: &DataValue) -> Result<i64> {
    match dv {
        DataValue::Num(Num::Int(i)) => Ok(*i),
        DataValue::Num(Num::Float(f)) => Ok(*f as i64),
        other => Err(backend_str(format!("expected int, got {other:?}"))),
    }
}
fn want_f64(dv: &DataValue) -> Result<f64> {
    match dv {
        DataValue::Num(Num::Float(f)) => Ok(*f),
        DataValue::Num(Num::Int(i)) => Ok(*i as f64),
        other => Err(backend_str(format!("expected float, got {other:?}"))),
    }
}

fn node_id(s: &str) -> Result<NodeId> {
    Ulid::from_string(s)
        .map(NodeId)
        .map_err(|e| backend_str(format!("bad node ulid {s:?}: {e}")))
}

/// Decode one physical canonical-node row through the sole trusted hydration
/// boundary. Cozo has already materialized the raw string by the time this
/// function runs; the byte check deliberately happens before `serde_json`
/// performs any further allocation or parsing.
fn decode_canonical_node_row(row: &[DataValue]) -> Result<Node> {
    let raw_id = row
        .first()
        .ok_or_else(|| backend_str("canonical node row is missing its id column".into()))
        .and_then(want_str)?;
    let id = node_id(raw_id)?;
    let raw = row
        .get(1)
        .ok_or_else(|| {
            backend_str(format!(
                "canonical node {} row is missing its data column",
                id.0
            ))
        })
        .and_then(want_str)
        .map_err(|error| {
            backend_str(format!(
                "canonical node {} violates sealed Node contract: {error}",
                id.0
            ))
        })?;
    let projected_status = row
        .get(2)
        .ok_or_else(|| {
            backend_str(format!(
                "canonical node {} row is missing its projected status column",
                id.0
            ))
        })
        .and_then(want_str)
        .map_err(|error| {
            backend_str(format!(
                "canonical node {} violates sealed Node contract: {error}",
                id.0
            ))
        })?;
    decode_canonical_node(id, raw, projected_status)
}

#[cfg(test)]
fn decode_upgrade_canonical_row(row: &[DataValue]) -> Result<CanonicalAuditRow> {
    let id_text = row
        .first()
        .ok_or_else(|| backend_str("canonical node row is missing its id column".into()))
        .and_then(want_str)?;
    let id = node_id(id_text)?;
    let raw = row
        .get(1)
        .ok_or_else(|| backend_str(format!("canonical node {} has no data column", id.0)))
        .and_then(want_str)?;
    let status = row
        .get(2)
        .ok_or_else(|| backend_str(format!("canonical node {} has no status column", id.0)))
        .and_then(want_str)?;
    let node = decode_canonical_node(id, raw, status)?;

    // Serde admission deliberately sorts valid historical tag arrays. Inspect
    // the raw array as well so duplicates/oversize/malformed entries have
    // already failed, while a pure ordering difference can be normalized.
    let raw_value: serde_json::Value = serde_json::from_str(raw).map_err(|error| {
        backend_str(format!(
            "canonical node {} violates sealed Node contract: {error}",
            id.0
        ))
    })?;
    let raw_tags = raw_value
        .get("tags")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            backend_str(format!(
                "canonical node {} violates sealed Node contract: tags is not an array",
                id.0
            ))
        })?;
    let mut sorted_tags = raw_tags
        .iter()
        .map(|tag| {
            tag.as_str().map(str::to_owned).ok_or_else(|| {
                backend_str(format!(
                    "canonical node {} violates sealed Node contract: tag is not a string",
                    id.0
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let original_tags = sorted_tags.clone();
    sorted_tags.sort();

    let normalized = if original_tags == sorted_tags {
        None
    } else {
        let mut only_tags_sorted = raw_value.clone();
        only_tags_sorted["tags"] = serde_json::Value::Array(
            sorted_tags
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        );
        // Preserve every historical/default-omitted field exactly as JSON
        // values and change only the raw tag-array order. Comparing against a
        // freshly serialized Node would spuriously reject rows that lawfully
        // omit serde-defaulted fields under the sealed predecessor contract.
        let normalized_raw = serde_json::to_string(&only_tags_sorted).map_err(|error| {
            backend_str(format!("normalize canonical node {} tags: {error}", id.0))
        })?;
        let normalized_node = decode_canonical_node(id, &normalized_raw, status)?;
        let original_semantics = serde_json::to_value(&node).map_err(|error| {
            backend_str(format!(
                "compare canonical node {} semantics: {error}",
                id.0
            ))
        })?;
        let normalized_semantics = serde_json::to_value(&normalized_node).map_err(|error| {
            backend_str(format!(
                "compare normalized node {} semantics: {error}",
                id.0
            ))
        })?;
        if normalized_semantics != original_semantics {
            return Err(backend_str(format!(
                "canonical node {} tag-order normalization changed decoded semantics",
                id.0
            )));
        }
        Some(normalized_raw)
    };

    Ok(CanonicalAuditRow {
        id: id_text.to_owned(),
        node,
        normalized,
    })
}

fn decode_canonical_node(id: NodeId, raw: &str, projected_status: &str) -> Result<Node> {
    let bytes = raw.len();
    if bytes > crate::MAX_CANONICAL_NODE_JSON_BYTES {
        return Err(backend_str(format!(
            "canonical node {} violates sealed Node contract: serialized blob is {bytes} bytes; maximum is {}",
            id.0,
            crate::MAX_CANONICAL_NODE_JSON_BYTES
        )));
    }
    let node: Node = serde_json::from_str(raw).map_err(|error| {
        backend_str(format!(
            "canonical node {} violates sealed Node contract: {error}",
            id.0
        ))
    })?;
    node.validate().map_err(|error| {
        backend_str(format!(
            "canonical node {} violates sealed Node contract: {error}",
            id.0
        ))
    })?;
    if node.id() != id {
        return Err(backend_str(format!(
            "canonical node {} violates sealed Node contract: blob id {} does not match row key",
            id.0,
            node.id().0
        )));
    }
    node_status_from(projected_status).map_err(|_| {
        backend_str(format!(
            "canonical node {} violates sealed Node contract: projected status {projected_status:?} is invalid",
            id.0
        ))
    })?;
    let blob_status = status_str(node.status());
    if blob_status != projected_status {
        return Err(backend_str(format!(
            "canonical node {} violates sealed Node contract: blob status {blob_status:?} does not match projected status {projected_status:?}",
            id.0
        )));
    }
    Ok(node)
}

/// Validate and serialize a canonical node before any storage mutation sees
/// it. The backend cap is deliberately checked on the exact encoded bytes that
/// will be written, not an estimate of the in-memory representation.
fn encode_canonical_node(node: &Node) -> Result<String> {
    node.validate().map_err(|error| {
        Error::InvalidInput(format!(
            "canonical node {} violates sealed Node contract: {error}",
            node.id().0
        ))
    })?;
    let raw = serde_json::to_string(node).map_err(|error| {
        backend_str(format!("serialize canonical node {}: {error}", node.id().0))
    })?;
    let bytes = raw.len();
    if bytes > crate::MAX_CANONICAL_NODE_JSON_BYTES {
        return Err(Error::InvalidInput(format!(
            "canonical node {} serialized blob is {bytes} bytes; maximum is {}",
            node.id().0,
            crate::MAX_CANONICAL_NODE_JSON_BYTES
        )));
    }
    Ok(raw)
}

fn status_str(s: NodeStatus) -> &'static str {
    match s {
        NodeStatus::Active => "active",
        NodeStatus::Archived => "archived",
    }
}

fn node_status_from(status: &str) -> Result<NodeStatus> {
    match status {
        "active" => Ok(NodeStatus::Active),
        "archived" => Ok(NodeStatus::Archived),
        other => Err(backend_str(format!("unknown node status {other:?}"))),
    }
}

fn edge_kind_str(k: EdgeKind) -> &'static str {
    match k {
        EdgeKind::Associative => "associative",
        EdgeKind::Transition => "transition",
        EdgeKind::Bridge => "bridge",
        EdgeKind::Supersedes => "supersedes",
        EdgeKind::DerivedFrom => "derived_from",
    }
}
fn edge_kind_from(s: &str) -> Result<EdgeKind> {
    match s {
        "associative" => Ok(EdgeKind::Associative),
        "transition" => Ok(EdgeKind::Transition),
        "bridge" => Ok(EdgeKind::Bridge),
        "supersedes" => Ok(EdgeKind::Supersedes),
        "derived_from" => Ok(EdgeKind::DerivedFrom),
        other => Err(backend_str(format!("unknown edge kind {other:?}"))),
    }
}

fn resolution_str(r: Resolution) -> &'static str {
    match r {
        Resolution::Superseded => "superseded",
        Resolution::ContextDependent => "context_dependent",
        Resolution::Unresolved => "unresolved",
    }
}
fn resolution_from(dv: &DataValue) -> Option<Resolution> {
    match dv {
        DataValue::Str(s) => match s.as_str() {
            "superseded" => Some(Resolution::Superseded),
            "context_dependent" => Some(Resolution::ContextDependent),
            "unresolved" => Some(Resolution::Unresolved),
            _ => None,
        },
        _ => None,
    }
}

fn merge_resolution_str(r: MergeResolution) -> &'static str {
    match r {
        MergeResolution::Full => "full",
        MergeResolution::Partial => "partial",
        MergeResolution::Keep => "keep",
    }
}

fn backend(e: impl std::fmt::Display) -> Error {
    Error::Backend(e.to_string())
}

fn backend_str(s: String) -> Error {
    Error::Backend(s)
}

mod capture;
mod commits;
mod concerns;
mod creation;
mod episodes;
mod feedback;
mod fresh_current;
mod graph;
mod graph_records;
mod lexical;
mod maintenance;
mod opening;
mod overlay;
mod runtime;
mod single_graph;
mod tag_vocabulary;
mod touchstones;
mod traversal;
mod vector;

#[cfg(test)]
use vector::tagged_membership_hydration_query;

#[cfg(test)]
mod tests;
