//! End-to-end exercise of the engine against the reference adapters. This is
//! the loop the design says to close first: ingest → retrieve, then the cold
//! path on top. Adapters are wired here (they're dev-dependencies); the engine
//! itself never names them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use mneme_body::InlineStore;
use mneme_core::ports::{
    BodyChunk, BodyStore, Budget, Clock, ColdPath, DensePruneChunkOutcome, Embedder, Error,
    FeedbackCommitOutcome, FeedbackRetryScope, GraphStore, LexicalIndex, MaintenanceCommit,
    MaintenanceCommitOutcome, MaintenanceEdgeKey, MaintenanceEdgePage, MaintenanceNodePage,
    Reranker, Result as PortResult, Scored, StatusFilter, SystemClock, TaggedPhysicalStatus,
    Traversal, VectorIndex,
};
use mneme_core::tagged::{
    MAX_TAGGED_RAW_MEMBERSHIPS, TaggedAnnBatch, TaggedAnnRequest, TaggedProjectionGeneration,
    TaggedSeedCoverage,
};
use mneme_core::{
    BodyOwnership, BodyRef, BodySpan, Edge, EdgeKind, MAX_DERIVED_SOURCES, MAX_INCIDENT_EDGES,
    MAX_REMOTE_EDGE_PAGE_SIZE, MAX_REMOTE_EDGES_PER_SOURCE, MergeResolution, Node, NodeId,
    NodeStatus, Provenance, Resolution, Signal, Timestamp,
};
#[cfg(feature = "cozo")]
use mneme_cozo::CozoStore;
use mneme_cozo::MemStore;
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::{Capture, Config, Ingest, MAX_RESOLVED_NEIGHBORS, Memory, ReceiptFeedback};
use tokio::sync::Notify;
use ulid::Ulid;

#[path = "end_to_end/routing_entry_boundary.rs"]
mod routing_entry_boundary;

#[path = "end_to_end/conditional_entry.rs"]
mod conditional_entry;

/// Wire one in-process `MemStore` in as all three graph-side ports, plus the
/// hashing embedder and an inline body store.
fn build(clock: Arc<dyn Clock>) -> Memory {
    // Disable the similarity-link floor by default so these tests see only
    // threshold-based linking (the lexical test embedder scores disjoint
    // summaries near zero); the floor is exercised in its own test. Exploration
    // is off too, so spread stays deterministic (its own test enables it).
    build_with(
        clock,
        Config {
            min_similarity_links: 0,
            budget: Budget {
                explore: 0.0,
                ..Budget::default()
            },
            ..Config::default()
        },
    )
}

fn build_with_similarity_prior(clock: Arc<dyn Clock>) -> Memory {
    build_with(
        clock,
        Config {
            similarity_link_cap: 5,
            min_similarity_links: 0,
            budget: Budget {
                explore: 0.0,
                ..Budget::default()
            },
            ..Config::default()
        },
    )
}

fn build_with(clock: Arc<dyn Clock>, cfg: Config) -> Memory {
    build_with_store(clock, cfg).0
}

fn build_with_store(clock: Arc<dyn Clock>, cfg: Config) -> (Memory, Arc<MemStore>) {
    let dim = DEFAULT_DIM;
    let store = Arc::new(MemStore::new(dim));
    let graph: Arc<dyn GraphStore> = store.clone();
    let vectors: Arc<dyn VectorIndex> = store.clone();
    let traversal: Arc<dyn Traversal> = store.clone();
    let embedder = Arc::new(HashingEmbedder::new(dim));

    let memory = Memory::new(graph, vectors, traversal, embedder, clock, cfg)
        .with_body_store(Arc::new(InlineStore::new()));
    (memory, store)
}

struct RecordingRetrievalIndex {
    inner: Arc<MemStore>,
    ann_limits: Mutex<Vec<usize>>,
    lexical_limits: Mutex<Vec<usize>>,
}

struct CountingGraphStore {
    inner: Arc<MemStore>,
    node_writes: AtomicU64,
    point_reads: AtomicU64,
    edge_point_reads: AtomicU64,
    batch_reads: AtomicU64,
    batch_sizes: Mutex<Vec<usize>>,
    status_batch_reads: AtomicU64,
    status_batch_sizes: Mutex<Vec<usize>>,
    neighbor_reads: AtomicU64,
    neighbor_limits: Mutex<Vec<usize>>,
    overreturn_neighbors: AtomicBool,
    archive_after_status_batch: Mutex<Option<NodeId>>,
    pause_status_batch: AtomicBool,
    status_batch_started: Notify,
    release_status_batch: Notify,
    edge_writes: AtomicU64,
    fail_edge_write_at: AtomicU64,
    fail_feedback_commit: AtomicBool,
    feedback_commits: AtomicU64,
    fail_supersede_after_commit: AtomicBool,
    fail_edge_deletes: AtomicBool,
    maintenance_batch_sizes: Mutex<Vec<usize>>,
    maintenance_commits: AtomicU64,
    dense_prune_commits: AtomicU64,
    pause_maintenance: AtomicBool,
    maintenance_started: Notify,
    release_maintenance: Notify,
    event_clock: AtomicU64,
    record_next_put: AtomicBool,
    pause_next_put: AtomicBool,
    put_started: Notify,
    release_put: Notify,
    hot_put_order: AtomicU64,
    second_chunk_order: AtomicU64,
    whole_node_scans: AtomicU64,
    whole_edge_scans: AtomicU64,
}

struct AmbiguousVectorIndex {
    inner: Arc<MemStore>,
    fail_after_upsert: AtomicBool,
}

struct CountingVectorIndex {
    inner: Arc<MemStore>,
    upserts: AtomicU64,
}

#[async_trait]
impl VectorIndex for CountingVectorIndex {
    fn semantic_id(&self) -> &'static str {
        VectorIndex::semantic_id(self.inner.as_ref())
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn upsert(&self, id: NodeId, embedding: &[f32]) -> PortResult<()> {
        self.upserts.fetch_add(1, Ordering::SeqCst);
        self.inner.upsert(id, embedding).await
    }

    async fn remove(&self, id: NodeId) -> PortResult<()> {
        self.inner.remove(id).await
    }

    async fn ann(&self, query: &[f32], k: usize, status: StatusFilter) -> PortResult<Vec<Scored>> {
        self.inner.ann(query, k, status).await
    }
}

struct CountingEmbedder {
    inner: HashingEmbedder,
    calls: AtomicU64,
}

#[async_trait]
impl Embedder for CountingEmbedder {
    fn dim(&self) -> usize {
        self.inner.dim()
    }

    fn fingerprint(&self) -> mneme_core::EmbeddingFingerprint {
        self.inner.fingerprint()
    }

    async fn embed(&self, texts: &[&str]) -> PortResult<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.embed(texts).await
    }
}

#[async_trait]
impl VectorIndex for AmbiguousVectorIndex {
    fn semantic_id(&self) -> &'static str {
        VectorIndex::semantic_id(self.inner.as_ref())
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn upsert(&self, id: NodeId, embedding: &[f32]) -> PortResult<()> {
        self.inner.upsert(id, embedding).await?;
        if self.fail_after_upsert.swap(false, Ordering::Relaxed) {
            return Err(Error::Backend(
                "injected ambiguous vector write failure after commit".into(),
            ));
        }
        Ok(())
    }

    async fn remove(&self, id: NodeId) -> PortResult<()> {
        self.inner.remove(id).await
    }

    async fn ann(&self, query: &[f32], k: usize, status: StatusFilter) -> PortResult<Vec<Scored>> {
        self.inner.ann(query, k, status).await
    }
}

struct AnchorFailingEmbedder {
    inner: HashingEmbedder,
    fail_multi_document: AtomicBool,
}

#[async_trait]
impl Embedder for AnchorFailingEmbedder {
    fn dim(&self) -> usize {
        self.inner.dim()
    }

    fn fingerprint(&self) -> mneme_core::EmbeddingFingerprint {
        self.inner.fingerprint()
    }

    async fn embed(&self, texts: &[&str]) -> PortResult<Vec<Vec<f32>>> {
        if texts.len() > 1 && self.fail_multi_document.swap(false, Ordering::Relaxed) {
            return Err(Error::Backend("injected anchor planning failure".into()));
        }
        self.inner.embed(texts).await
    }
}

impl CountingGraphStore {
    fn new(inner: Arc<MemStore>) -> Self {
        Self {
            inner,
            node_writes: AtomicU64::new(0),
            point_reads: AtomicU64::new(0),
            edge_point_reads: AtomicU64::new(0),
            batch_reads: AtomicU64::new(0),
            batch_sizes: Mutex::new(Vec::new()),
            status_batch_reads: AtomicU64::new(0),
            status_batch_sizes: Mutex::new(Vec::new()),
            neighbor_reads: AtomicU64::new(0),
            neighbor_limits: Mutex::new(Vec::new()),
            overreturn_neighbors: AtomicBool::new(false),
            archive_after_status_batch: Mutex::new(None),
            pause_status_batch: AtomicBool::new(false),
            status_batch_started: Notify::new(),
            release_status_batch: Notify::new(),
            edge_writes: AtomicU64::new(0),
            fail_edge_write_at: AtomicU64::new(0),
            fail_feedback_commit: AtomicBool::new(false),
            feedback_commits: AtomicU64::new(0),
            fail_supersede_after_commit: AtomicBool::new(false),
            fail_edge_deletes: AtomicBool::new(false),
            maintenance_batch_sizes: Mutex::new(Vec::new()),
            maintenance_commits: AtomicU64::new(0),
            dense_prune_commits: AtomicU64::new(0),
            pause_maintenance: AtomicBool::new(false),
            maintenance_started: Notify::new(),
            release_maintenance: Notify::new(),
            event_clock: AtomicU64::new(0),
            record_next_put: AtomicBool::new(false),
            pause_next_put: AtomicBool::new(false),
            put_started: Notify::new(),
            release_put: Notify::new(),
            hot_put_order: AtomicU64::new(0),
            second_chunk_order: AtomicU64::new(0),
            whole_node_scans: AtomicU64::new(0),
            whole_edge_scans: AtomicU64::new(0),
        }
    }

    fn reset_reads(&self) {
        self.point_reads.store(0, Ordering::Relaxed);
        self.edge_point_reads.store(0, Ordering::Relaxed);
        self.batch_reads.store(0, Ordering::Relaxed);
        self.batch_sizes.lock().unwrap().clear();
        self.status_batch_reads.store(0, Ordering::Relaxed);
        self.status_batch_sizes.lock().unwrap().clear();
        self.neighbor_reads.store(0, Ordering::Relaxed);
        self.neighbor_limits.lock().unwrap().clear();
    }

    fn inject_ambiguous_edge_failure(&self, write_number: u64) {
        self.edge_writes.store(0, Ordering::Relaxed);
        self.fail_edge_write_at
            .store(write_number, Ordering::Relaxed);
        self.whole_edge_scans.store(0, Ordering::Relaxed);
    }
}

#[async_trait]
impl GraphStore for CountingGraphStore {
    async fn put_node(&self, node: &Node) -> PortResult<()> {
        self.node_writes.fetch_add(1, Ordering::SeqCst);
        if self.pause_next_put.swap(false, Ordering::SeqCst) {
            self.put_started.notify_one();
            self.release_put.notified().await;
        }
        if self.record_next_put.swap(false, Ordering::SeqCst) {
            let order = self.event_clock.fetch_add(1, Ordering::SeqCst) + 1;
            self.hot_put_order.store(order, Ordering::SeqCst);
        }
        self.inner.put_node(node).await
    }

    async fn get_node(&self, id: NodeId) -> PortResult<Option<Node>> {
        self.point_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get_node(id).await
    }

    async fn get_nodes(&self, ids: &[NodeId]) -> PortResult<Vec<Option<Node>>> {
        self.batch_reads.fetch_add(1, Ordering::Relaxed);
        self.batch_sizes.lock().unwrap().push(ids.len());
        self.inner.get_nodes(ids).await
    }

    async fn get_node_statuses(
        &self,
        ids: &[NodeId],
    ) -> PortResult<Vec<Option<TaggedPhysicalStatus>>> {
        self.status_batch_reads.fetch_add(1, Ordering::Relaxed);
        self.status_batch_sizes.lock().unwrap().push(ids.len());
        let statuses = self.inner.get_node_statuses(ids).await?;
        if self.pause_status_batch.swap(false, Ordering::SeqCst) {
            self.status_batch_started.notify_one();
            self.release_status_batch.notified().await;
        }
        let archive = self.archive_after_status_batch.lock().unwrap().take();
        if let Some(id) = archive {
            self.inner.set_status(id, NodeStatus::Archived).await?;
        }
        Ok(statuses)
    }

    async fn delete_node(&self, id: NodeId) -> PortResult<()> {
        self.inner.delete_node(id).await
    }

    async fn all_nodes(&self, cold: ColdPath) -> PortResult<Vec<Node>> {
        self.whole_node_scans.fetch_add(1, Ordering::Relaxed);
        self.inner.all_nodes(cold).await
    }

    async fn set_status(&self, id: NodeId, status: NodeStatus) -> PortResult<()> {
        self.inner.set_status(id, status).await
    }

    async fn maintenance_node_upper_bound(&self, cold: ColdPath) -> PortResult<Option<NodeId>> {
        self.inner.maintenance_node_upper_bound(cold).await
    }

    async fn maintenance_nodes_page(
        &self,
        cold: ColdPath,
        after: Option<NodeId>,
        through: NodeId,
        limit: usize,
    ) -> PortResult<MaintenanceNodePage> {
        self.inner
            .maintenance_nodes_page(cold, after, through, limit)
            .await
    }

    async fn put_edge(&self, edge: &Edge) -> PortResult<()> {
        let write_number = self.edge_writes.fetch_add(1, Ordering::Relaxed) + 1;
        self.inner.put_edge(edge).await?;
        if self.fail_edge_write_at.load(Ordering::Relaxed) == write_number {
            return Err(Error::Backend(
                "injected ambiguous edge write failure after commit".into(),
            ));
        }
        Ok(())
    }

    async fn delete_edge(&self, from: NodeId, to: NodeId) -> PortResult<()> {
        if self.fail_edge_deletes.load(Ordering::Relaxed) {
            return Err(Error::Backend("injected edge rollback failure".into()));
        }
        self.inner.delete_edge(from, to).await
    }

    async fn get_edge(&self, from: NodeId, to: NodeId) -> PortResult<Option<Edge>> {
        self.edge_point_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get_edge(from, to).await
    }

    async fn all_edges(&self, cold: ColdPath) -> PortResult<Vec<Edge>> {
        self.whole_edge_scans.fetch_add(1, Ordering::Relaxed);
        self.inner.all_edges(cold).await
    }

    async fn maintenance_edge_upper_bound(
        &self,
        cold: ColdPath,
    ) -> PortResult<Option<MaintenanceEdgeKey>> {
        self.inner.maintenance_edge_upper_bound(cold).await
    }

    async fn maintenance_edges_page(
        &self,
        cold: ColdPath,
        after: Option<MaintenanceEdgeKey>,
        through: MaintenanceEdgeKey,
        limit: usize,
    ) -> PortResult<MaintenanceEdgePage> {
        self.inner
            .maintenance_edges_page(cold, after, through, limit)
            .await
    }

    async fn neighbors(
        &self,
        id: NodeId,
        top_k: usize,
    ) -> PortResult<Vec<mneme_core::ports::Neighbor>> {
        self.neighbor_reads.fetch_add(1, Ordering::Relaxed);
        self.neighbor_limits.lock().unwrap().push(top_k);
        let mut neighbors = self.inner.neighbors(id, top_k).await?;
        if self.overreturn_neighbors.swap(false, Ordering::Relaxed)
            && let Some(first) = neighbors.first().cloned()
        {
            neighbors.resize(top_k.min(MAX_INCIDENT_EDGES) + 1, first);
        }
        Ok(neighbors)
    }

    async fn commit_feedback(
        &self,
        commit: &mneme_core::ports::FeedbackCommit,
    ) -> PortResult<mneme_core::ports::FeedbackCommitOutcome> {
        self.feedback_commits.fetch_add(1, Ordering::Relaxed);
        if self.fail_feedback_commit.swap(false, Ordering::Relaxed) {
            return Err(Error::Backend(
                "injected feedback transaction failure before commit".into(),
            ));
        }
        self.inner.commit_feedback(commit).await
    }

    async fn commit_full_merge(
        &self,
        commit: &mneme_core::ports::FullMergeCommit,
    ) -> PortResult<mneme_core::ports::FullMergeCommitOutcome> {
        self.inner.commit_full_merge(commit).await
    }

    async fn commit_supersede(
        &self,
        commit: &mneme_core::ports::SupersedeCommit,
    ) -> PortResult<mneme_core::ports::SupersedeCommitOutcome> {
        let outcome = self.inner.commit_supersede(commit).await?;
        if self
            .fail_supersede_after_commit
            .swap(false, Ordering::Relaxed)
        {
            return Err(Error::Backend(
                "injected lost supersede acknowledgement".into(),
            ));
        }
        Ok(outcome)
    }

    async fn commit_maintenance(
        &self,
        cold: ColdPath,
        commit: &MaintenanceCommit,
    ) -> PortResult<MaintenanceCommitOutcome> {
        let commit_number = self.maintenance_commits.fetch_add(1, Ordering::SeqCst) + 1;
        if commit_number == 2 {
            let order = self.event_clock.fetch_add(1, Ordering::SeqCst) + 1;
            self.second_chunk_order.store(order, Ordering::SeqCst);
        }
        self.maintenance_batch_sizes
            .lock()
            .unwrap()
            .push(commit.edges.len());
        if self.pause_maintenance.swap(false, Ordering::Relaxed) {
            self.maintenance_started.notify_one();
            self.release_maintenance.notified().await;
        }
        self.inner.commit_maintenance(cold, commit).await
    }

    async fn maintenance_overfull_hubs(
        &self,
        cold: ColdPath,
        candidates: &[NodeId],
        target_degree: usize,
    ) -> PortResult<Vec<NodeId>> {
        self.inner
            .maintenance_overfull_hubs(cold, candidates, target_degree)
            .await
    }

    async fn prune_incident_associations(
        &self,
        cold: ColdPath,
        hub: NodeId,
        target_degree: usize,
        max_deletes: usize,
    ) -> PortResult<DensePruneChunkOutcome> {
        self.dense_prune_commits.fetch_add(1, Ordering::Relaxed);
        self.inner
            .prune_incident_associations(cold, hub, target_degree, max_deletes)
            .await
    }

    async fn observe_contradiction(&self, a: NodeId, b: NodeId, at: Timestamp) -> PortResult<()> {
        self.inner.observe_contradiction(a, b, at).await
    }

    async fn open_contradictions(
        &self,
        cold: ColdPath,
    ) -> PortResult<Vec<mneme_core::Contradiction>> {
        self.inner.open_contradictions(cold).await
    }

    async fn resolve_contradiction(
        &self,
        pair: unordered_pair::UnorderedPair<NodeId>,
        resolution: mneme_core::Resolution,
    ) -> PortResult<()> {
        self.inner.resolve_contradiction(pair, resolution).await
    }

    async fn observe_merge_candidate(&self, a: NodeId, b: NodeId, at: Timestamp) -> PortResult<()> {
        self.inner.observe_merge_candidate(a, b, at).await
    }

    async fn open_merge_candidates(
        &self,
        cold: ColdPath,
    ) -> PortResult<Vec<mneme_core::MergeCandidate>> {
        self.inner.open_merge_candidates(cold).await
    }

    async fn resolve_merge_candidate(
        &self,
        pair: unordered_pair::UnorderedPair<NodeId>,
        resolution: mneme_core::MergeResolution,
    ) -> PortResult<()> {
        self.inner.resolve_merge_candidate(pair, resolution).await
    }

    async fn put_remote_edge(&self, edge: &mneme_core::RemoteEdge) -> PortResult<()> {
        self.inner.put_remote_edge(edge).await
    }

    async fn remote_edges_page(
        &self,
        from: NodeId,
        after: Option<mneme_core::RemoteEdgeCursor>,
        limit: usize,
    ) -> PortResult<mneme_core::RemoteEdgePage> {
        self.inner.remote_edges_page(from, after, limit).await
    }

    async fn delete_remote_edge(
        &self,
        from: NodeId,
        target_db: Ulid,
        target: NodeId,
    ) -> PortResult<()> {
        self.inner.delete_remote_edge(from, target_db, target).await
    }
}

impl RecordingRetrievalIndex {
    fn new(inner: Arc<MemStore>) -> Self {
        Self {
            inner,
            ann_limits: Mutex::new(Vec::new()),
            lexical_limits: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl VectorIndex for RecordingRetrievalIndex {
    fn semantic_id(&self) -> &'static str {
        VectorIndex::semantic_id(self.inner.as_ref())
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn upsert(&self, id: NodeId, embedding: &[f32]) -> PortResult<()> {
        self.inner.upsert(id, embedding).await
    }

    async fn remove(&self, id: NodeId) -> PortResult<()> {
        self.inner.remove(id).await
    }

    async fn ann(&self, query: &[f32], k: usize, status: StatusFilter) -> PortResult<Vec<Scored>> {
        self.ann_limits
            .lock()
            .expect("recording ANN mutex poisoned")
            .push(k);
        self.inner.ann(query, k, status).await
    }
}

#[async_trait]
impl LexicalIndex for RecordingRetrievalIndex {
    fn semantic_id(&self) -> &'static str {
        LexicalIndex::semantic_id(self.inner.as_ref())
    }

    async fn search(&self, query: &str, k: usize, status: StatusFilter) -> PortResult<Vec<Scored>> {
        self.lexical_limits
            .lock()
            .expect("recording lexical mutex poisoned")
            .push(k);
        self.inner.search(query, k, status).await
    }
}

/// Deliberately violates the tagged port contract to prove the engine treats
/// adapter output as untrusted even when the adapter declares a generation.
struct MalformedTaggedVectorIndex {
    inner: Arc<MemStore>,
}

struct WrongTagVectorIndex {
    inner: Arc<MemStore>,
    substitute: Mutex<Option<NodeId>>,
}

struct BadTaggedGenerationVectorIndex {
    inner: Arc<MemStore>,
}

#[async_trait]
impl VectorIndex for MalformedTaggedVectorIndex {
    fn semantic_id(&self) -> &'static str {
        "test-malformed-tagged-vector-v1"
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn upsert(&self, id: NodeId, embedding: &[f32]) -> PortResult<()> {
        self.inner.upsert(id, embedding).await
    }

    async fn remove(&self, id: NodeId) -> PortResult<()> {
        self.inner.remove(id).await
    }

    async fn ann(&self, query: &[f32], k: usize, status: StatusFilter) -> PortResult<Vec<Scored>> {
        self.inner.ann(query, k, status).await
    }

    fn tagged_projection_generation(&self) -> Option<&'static str> {
        Some("test-malformed-tag-projection-v1")
    }

    async fn tagged_ann(&self, _request: TaggedAnnRequest<'_>) -> PortResult<TaggedAnnBatch> {
        Ok(TaggedAnnBatch {
            lanes: Vec::new(),
            work: mneme_core::tagged::TaggedAnnWork {
                query_dimension: DEFAULT_DIM,
                ..Default::default()
            },
            projection_generation: TaggedProjectionGeneration::new(
                "test-malformed-tag-projection-v1",
            )
            .unwrap(),
        })
    }
}

#[async_trait]
impl VectorIndex for WrongTagVectorIndex {
    fn semantic_id(&self) -> &'static str {
        VectorIndex::semantic_id(self.inner.as_ref())
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn upsert(&self, id: NodeId, embedding: &[f32]) -> PortResult<()> {
        self.inner.upsert(id, embedding).await
    }

    async fn remove(&self, id: NodeId) -> PortResult<()> {
        self.inner.remove(id).await
    }

    async fn ann(&self, query: &[f32], k: usize, status: StatusFilter) -> PortResult<Vec<Scored>> {
        self.inner.ann(query, k, status).await
    }

    fn tagged_projection_generation(&self) -> Option<&'static str> {
        self.inner.tagged_projection_generation()
    }

    async fn tagged_ann(&self, request: TaggedAnnRequest<'_>) -> PortResult<TaggedAnnBatch> {
        let mut batch = self.inner.tagged_ann(request).await?;
        if let Some(substitute) = *self
            .substitute
            .lock()
            .expect("wrong-tag substitute mutex poisoned")
            && let Some(hit) = batch.lanes[0].hits.first_mut()
        {
            hit.id = substitute;
        }
        Ok(batch)
    }
}

#[async_trait]
impl VectorIndex for BadTaggedGenerationVectorIndex {
    fn semantic_id(&self) -> &'static str {
        VectorIndex::semantic_id(self.inner.as_ref())
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn upsert(&self, id: NodeId, embedding: &[f32]) -> PortResult<()> {
        self.inner.upsert(id, embedding).await
    }

    async fn remove(&self, id: NodeId) -> PortResult<()> {
        self.inner.remove(id).await
    }

    async fn ann(&self, query: &[f32], k: usize, status: StatusFilter) -> PortResult<Vec<Scored>> {
        self.inner.ann(query, k, status).await
    }

    fn tagged_projection_generation(&self) -> Option<&'static str> {
        Some("invalid\ngeneration")
    }

    async fn tagged_ann(&self, _request: TaggedAnnRequest<'_>) -> PortResult<TaggedAnnBatch> {
        panic!("malformed declared generation must fail before backend work")
    }
}

#[tokio::test]
async fn retrieval_caps_primary_work_without_extra_lane() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let index = Arc::new(RecordingRetrievalIndex::new(store.clone()));
    let memory = Memory::new(
        store.clone(),
        index.clone(),
        store,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            lexical_k: usize::MAX,
            graph_seed_cap: 0,
            ..Config::default()
        },
    )
    .with_lexical_index(index.clone());
    let budget = Budget {
        max_nodes: 3,
        max_depth: 0,
        ..Budget::default()
    };

    assert!(
        memory
            .retrieve_seeded("bounded", usize::MAX, budget, StatusFilter::ACTIVE, &[])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        *index
            .ann_limits
            .lock()
            .expect("recording ANN mutex poisoned"),
        vec![3],
        "the only semantic lane uses the visible bound"
    );
    assert_eq!(
        *index
            .lexical_limits
            .lock()
            .expect("recording lexical mutex poisoned"),
        vec![3],
        "the sparse leg uses the same bounded primary window"
    );

    let no_work = Budget {
        max_nodes: 0,
        ..budget
    };
    assert!(
        memory
            .retrieve_seeded("no work", usize::MAX, no_work, StatusFilter::ACTIVE, &[],)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        index
            .ann_limits
            .lock()
            .expect("recording ANN mutex poisoned")
            .len(),
        1,
        "zero node budget performs no adapter query"
    );
}

#[cfg(feature = "cozo")]
fn build_with_cozo_store(clock: Arc<dyn Clock>, cfg: Config) -> (Memory, Arc<CozoStore>) {
    let dim = DEFAULT_DIM;
    let store = Arc::new(CozoStore::new(dim).unwrap());
    let graph: Arc<dyn GraphStore> = store.clone();
    let vectors: Arc<dyn VectorIndex> = store.clone();
    let traversal: Arc<dyn Traversal> = store.clone();
    let embedder = Arc::new(HashingEmbedder::new(dim));

    let memory = Memory::new(graph, vectors, traversal, embedder, clock, cfg)
        .with_body_store(Arc::new(InlineStore::new()));
    (memory, store)
}

/// A body adapter whose delete can fail or pause, for checking `forget`'s
/// cross-adapter ordering and its serialization against other public mutators.
#[derive(Default)]
struct ControlledBodyStore {
    blobs: Mutex<HashMap<BodyRef, Vec<u8>>>,
    next: AtomicU64,
    put_calls: AtomicU64,
    get_calls: AtomicU64,
    range_calls: AtomicU64,
    delete_calls: AtomicU64,
    fail_put: AtomicBool,
    pause_put: AtomicBool,
    put_started: Notify,
    release_put: Notify,
    fail_delete: AtomicBool,
    pause_delete: AtomicBool,
    delete_started: Notify,
    release_delete: Notify,
}

#[async_trait]
impl BodyStore for ControlledBodyStore {
    fn scheme(&self) -> &'static str {
        "controlled"
    }

    async fn get(&self, body: &BodyRef) -> PortResult<Vec<u8>> {
        self.get_calls.fetch_add(1, Ordering::SeqCst);
        self.blobs
            .lock()
            .expect("controlled body mutex poisoned")
            .get(body)
            .cloned()
            .ok_or_else(|| Error::Body(format!("no controlled body for {}", body.as_str())))
    }

    async fn get_range(
        &self,
        body: &BodyRef,
        offset: u64,
        max_bytes: usize,
    ) -> PortResult<BodyChunk> {
        self.range_calls.fetch_add(1, Ordering::SeqCst);
        max_bytes.checked_add(1).ok_or_else(|| {
            Error::InvalidInput("body range max_bytes overflows its continuation probe".into())
        })?;
        let blobs = self.blobs.lock().expect("controlled body mutex poisoned");
        let bytes = blobs
            .get(body)
            .ok_or_else(|| Error::Body(format!("no controlled body for {}", body.as_str())))?;
        let body_len = u64::try_from(bytes.len())
            .map_err(|_| Error::InvalidInput("controlled body length does not fit u64".into()))?;
        if offset >= body_len {
            return Ok(BodyChunk {
                bytes: Vec::new(),
                source_start: offset,
                source_end: offset,
                next_offset: None,
            });
        }
        let start = usize::try_from(offset)
            .map_err(|_| Error::InvalidInput("body range offset does not fit usize".into()))?;
        let end = start.saturating_add(max_bytes).min(bytes.len());
        let source_end = offset
            .checked_add(u64::try_from(end - start).map_err(|_| {
                Error::InvalidInput("controlled body chunk length does not fit u64".into())
            })?)
            .ok_or_else(|| Error::InvalidInput("body range end overflow".into()))?;
        Ok(BodyChunk {
            bytes: bytes[start..end].to_vec(),
            source_start: offset,
            source_end,
            next_offset: (end < bytes.len()).then_some(source_end),
        })
    }

    async fn put(&self, bytes: &[u8]) -> PortResult<BodyRef> {
        self.put_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_put.load(Ordering::SeqCst) {
            return Err(Error::Body("injected put failure".into()));
        }
        if self.pause_put.swap(false, Ordering::SeqCst) {
            self.put_started.notify_one();
            self.release_put.notified().await;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let body = BodyRef::new(format!("controlled://{id}"))
            .map_err(|error| Error::Body(format!("mint controlled body reference: {error}")))?;
        self.blobs
            .lock()
            .expect("controlled body mutex poisoned")
            .insert(body.clone(), bytes.to_vec());
        Ok(body)
    }

    async fn delete(&self, body: &BodyRef) -> PortResult<()> {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(Error::Body("injected delete failure".into()));
        }
        if self.pause_delete.load(Ordering::SeqCst) {
            self.delete_started.notify_one();
            self.release_delete.notified().await;
        }
        self.blobs
            .lock()
            .expect("controlled body mutex poisoned")
            .remove(body);
        Ok(())
    }
}

fn build_with_controlled_body(store: Arc<ControlledBodyStore>) -> Memory {
    let dim = DEFAULT_DIM;
    let backend = Arc::new(MemStore::new(dim));
    let cfg = Config {
        default_body_scheme: "controlled",
        min_similarity_links: 0,
        budget: Budget {
            explore: 0.0,
            ..Budget::default()
        },
        ..Config::default()
    };
    Memory::new(
        backend.clone(),
        backend.clone(),
        backend,
        Arc::new(HashingEmbedder::new(dim)),
        Arc::new(SystemClock),
        cfg,
    )
    .with_body_store(store)
}

fn prov() -> Provenance {
    Provenance::derived_empty()
}

fn raw_active_node(id: NodeId) -> Node {
    Node::try_new(
        id,
        format!("node {}", id.0),
        BodyRef::new(format!("inline://{}", id.0)).unwrap(),
        std::iter::empty::<&str>(),
        prov(),
        1.0,
        1.0,
        NodeStatus::Active,
        1,
    )
    .unwrap()
}

#[test]
fn ingest_origin_commit_is_validated_before_the_request_can_run() {
    assert!(
        Ingest::new("summary", b"body", &[], prov())
            .with_origin_commit(Some("deadbeef"))
            .is_err()
    );
    let sha1 = "0123456789abcdef0123456789abcdef01234567";
    assert!(
        Ingest::new("summary", b"body", &[], prov())
            .with_origin_commit(Some(sha1))
            .is_ok()
    );
}

#[tokio::test]
async fn sparse_capacity_prune_pages_without_per_node_write_transactions() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let mem = Memory::new(
        counted.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config::default(),
    );
    for raw in 1_500u128..1_630 {
        store
            .put_node(&raw_active_node(NodeId(Ulid::from(raw))))
            .await
            .unwrap();
    }

    let report = mem.prune_dense(ColdPath::acquire()).await.unwrap();
    assert_eq!(report.pruned, 0);
    assert_eq!(report.hub_pages, 3);
    assert_eq!(report.chunks, 0);
    assert_eq!(counted.dense_prune_commits.load(Ordering::Relaxed), 0);
    assert_eq!(counted.whole_node_scans.load(Ordering::Relaxed), 0);
    assert_eq!(counted.whole_edge_scans.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn capacity_prune_reuses_a_shared_edge_deletion_across_hubs() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            dense_degree_threshold: 1,
            prune_weight_floor: 0.0,
            ..Config::default()
        },
    );
    let a = NodeId(Ulid::from(1_700u128));
    let b = NodeId(Ulid::from(1_701u128));
    let x = NodeId(Ulid::from(1_702u128));
    let y = NodeId(Ulid::from(1_703u128));
    for id in [a, b, x, y] {
        store.put_node(&raw_active_node(id)).await.unwrap();
    }
    // The old global sort deleted B-Y (0.1) and then A-B (0.2). Current
    // per-hub pruning visits A first, deletes its weakest A-B, and that one
    // shared deletion makes B no longer overfull, retaining both private edges.
    for edge in [
        Edge::new(a, b, 0.2, EdgeKind::Associative, 1),
        Edge::new(a, x, 0.9, EdgeKind::Associative, 1),
        Edge::new(b, y, 0.1, EdgeKind::Associative, 1),
    ] {
        store.put_edge(&edge).await.unwrap();
    }

    let report = mem.prune_dense(ColdPath::acquire()).await.unwrap();
    assert_eq!(report.capacity_pruned, 1);
    assert!(store.get_edge(a, b).await.unwrap().is_none());
    assert!(store.get_edge(a, x).await.unwrap().is_some());
    assert!(store.get_edge(b, y).await.unwrap().is_some());
}

#[tokio::test]
async fn ingest_prepares_all_domain_values_before_gate_or_adapter_side_effects() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let graph = Arc::new(CountingGraphStore::new(store.clone()));
    let vectors = Arc::new(CountingVectorIndex {
        inner: store.clone(),
        upserts: AtomicU64::new(0),
    });
    let embedder = Arc::new(CountingEmbedder {
        inner: HashingEmbedder::new(DEFAULT_DIM),
        calls: AtomicU64::new(0),
    });
    let bodies = Arc::new(ControlledBodyStore::default());
    let memory = Arc::new(
        Memory::new(
            graph.clone(),
            vectors.clone(),
            store,
            embedder.clone(),
            Arc::new(SystemClock),
            Config {
                default_body_scheme: "controlled",
                min_similarity_links: 0,
                ..Config::default()
            },
        )
        .with_body_store(bodies.clone()),
    );

    graph.pause_next_put.store(true, Ordering::SeqCst);
    let valid = {
        let memory = memory.clone();
        tokio::spawn(async move {
            memory
                .ingest(Ingest::new(
                    "valid request holding the mutation gate",
                    b"body",
                    &[],
                    prov(),
                ))
                .await
        })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        graph.put_started.notified(),
    )
    .await
    .expect("valid ingest should hold the mutation gate at its graph write");

    let observed_effects = || {
        (
            embedder.calls.load(Ordering::SeqCst),
            bodies.put_calls.load(Ordering::SeqCst),
            graph.node_writes.load(Ordering::SeqCst),
            vectors.upserts.load(Ordering::SeqCst),
        )
    };
    let baseline = observed_effects();
    assert_eq!(baseline, (1, 1, 1, 0));

    let duplicate_tags = ["duplicate", "duplicate"];
    for req in [
        Ingest::new("   ", b"body", &[], prov()),
        Ingest::new("finite", b"body", &[], prov()).with_stability(f32::NAN),
        Ingest::new("range", b"body", &[], prov()).with_confidence(f32::INFINITY),
        Ingest::new("tag", b"body", &[" bad"], prov()),
        Ingest::new("duplicate tags", b"body", &duplicate_tags, prov()),
    ] {
        let error = tokio::time::timeout(std::time::Duration::from_millis(250), memory.ingest(req))
            .await
            .expect("invalid ingest must not wait for the held mutation gate")
            .expect_err("invalid ingest should fail");
        assert!(matches!(error, Error::InvalidInput(_)));
        assert_eq!(observed_effects(), baseline);
    }

    let duplicate = NodeId(Ulid::from(90_000_u128));
    assert!(Provenance::derived([duplicate, duplicate]).is_err());
    let too_many_sources = (0..=MAX_DERIVED_SOURCES)
        .map(|offset| NodeId(Ulid::from(100_000_u128 + u128::try_from(offset).unwrap())));
    assert!(Provenance::derived(too_many_sources).is_err());
    assert_eq!(observed_effects(), baseline);

    graph.release_put.notify_one();
    valid.await.unwrap().unwrap();
}

#[tokio::test]
async fn retrieval_hydrates_and_revalidates_in_two_batches_without_point_reads() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let cfg = Config {
        ann_k: 3,
        lexical_k: 0,
        graph_seed_cap: 0,
        min_similarity_links: 0,
        budget: Budget {
            max_nodes: 3,
            max_depth: 0,
            min_relevance: 0.0,
            relevance_ratio: 0.0,
            dedup_similarity: 1.0,
            ..Budget::default()
        },
        ..Config::default()
    };
    let mem = Memory::new(
        counted.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        cfg,
    )
    .with_body_store(Arc::new(InlineStore::new()));
    for suffix in ["alpha", "beta", "gamma"] {
        mem.ingest(Ingest::new(
            &format!("shared batch token {suffix}"),
            b"body",
            &[],
            prov(),
        ))
        .await
        .unwrap();
    }

    counted.reset_reads();
    let hits = mem.retrieve("shared batch token").await.unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(counted.point_reads.load(Ordering::Relaxed), 0);
    assert_eq!(counted.batch_reads.load(Ordering::Relaxed), 2);
    assert_eq!(*counted.batch_sizes.lock().unwrap(), vec![3, 3]);
}

#[tokio::test]
async fn resolved_neighbors_batch_full_status_scope_then_one_bounded_hydration() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let mem = Memory::new(
        counted.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            min_similarity_links: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let hub = mem
        .ingest(Ingest::new("hub", b"", &[], prov()))
        .await
        .unwrap();

    for offset in 0..70 {
        let node = mem
            .ingest(Ingest::new(&format!("archived {offset}"), b"", &[], prov()))
            .await
            .unwrap();
        mem.link(hub, node, EdgeKind::Associative, 0.9, None)
            .await
            .unwrap();
        store.set_status(node, NodeStatus::Archived).await.unwrap();
    }
    for offset in 0..70 {
        let node = mem
            .ingest(Ingest::new(&format!("active {offset}"), b"", &[], prov()))
            .await
            .unwrap();
        mem.link(hub, node, EdgeKind::Associative, 0.1, None)
            .await
            .unwrap();
    }
    let missing = mem
        .ingest(Ingest::new("missing", b"", &[], prov()))
        .await
        .unwrap();
    mem.link(hub, missing, EdgeKind::Associative, 1.0, None)
        .await
        .unwrap();
    // The low-level adapter deliberately leaves the separate edge relation in
    // place; this simulates an interrupted/corrupt endpoint cleanup.
    store.delete_node(missing).await.unwrap();

    counted.reset_reads();
    let raw = mem.neighbors_hydrated(hub, 1).await.unwrap();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].neighbor.node, missing);
    assert!(
        raw[0].node.is_none(),
        "raw diagnostics retain dangling rows"
    );
    assert_eq!(*counted.neighbor_limits.lock().unwrap(), vec![1]);
    assert_eq!(counted.status_batch_reads.load(Ordering::Relaxed), 0);
    assert_eq!(*counted.batch_sizes.lock().unwrap(), vec![1]);

    counted.reset_reads();
    let all = mem
        .resolved_neighbors_scoped(hub, MAX_RESOLVED_NEIGHBORS, StatusFilter::ALL)
        .await
        .unwrap();
    assert_eq!(
        all.len(),
        MAX_RESOLVED_NEIGHBORS,
        "even ALL excludes missing/episodic rows before its shortlist"
    );
    assert_eq!(
        *counted.neighbor_limits.lock().unwrap(),
        vec![MAX_INCIDENT_EDGES]
    );
    assert_eq!(counted.status_batch_reads.load(Ordering::Relaxed), 1);
    assert_eq!(
        *counted.batch_sizes.lock().unwrap(),
        vec![MAX_RESOLVED_NEIGHBORS]
    );

    counted.reset_reads();
    let resolved = mem
        .resolved_neighbors_scoped(hub, MAX_RESOLVED_NEIGHBORS, StatusFilter::ACTIVE)
        .await
        .unwrap();
    assert_eq!(resolved.len(), MAX_RESOLVED_NEIGHBORS);
    assert!(
        resolved
            .iter()
            .all(|item| item.node.status() == NodeStatus::Active)
    );
    assert!(
        resolved
            .iter()
            .all(|item| item.node.summary().starts_with("active "))
    );
    assert_eq!(counted.point_reads.load(Ordering::Relaxed), 0);
    assert_eq!(counted.neighbor_reads.load(Ordering::Relaxed), 1);
    assert_eq!(
        *counted.neighbor_limits.lock().unwrap(),
        vec![MAX_INCIDENT_EDGES]
    );
    assert_eq!(counted.status_batch_reads.load(Ordering::Relaxed), 1);
    assert_eq!(*counted.status_batch_sizes.lock().unwrap(), vec![142]);
    assert_eq!(counted.batch_reads.load(Ordering::Relaxed), 1);
    assert_eq!(
        *counted.batch_sizes.lock().unwrap(),
        vec![MAX_RESOLVED_NEIGHBORS]
    );
}

#[tokio::test]
async fn resolved_neighbors_revalidate_lifecycle_and_reject_oversize_before_adapter_work() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let mem = Memory::new(
        counted.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            min_similarity_links: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let hub = mem
        .ingest(Ingest::new("hub", b"", &[], prov()))
        .await
        .unwrap();
    let target = mem
        .ingest(Ingest::new("target", b"", &[], prov()))
        .await
        .unwrap();
    mem.link(hub, target, EdgeKind::Associative, 0.5, None)
        .await
        .unwrap();

    counted.reset_reads();
    *counted.archive_after_status_batch.lock().unwrap() = Some(target);
    assert!(
        mem.resolved_neighbors_scoped(hub, 1, StatusFilter::ACTIVE)
            .await
            .unwrap()
            .is_empty(),
        "a node archived after status selection must fail hydration revalidation"
    );
    assert_eq!(counted.point_reads.load(Ordering::Relaxed), 0);
    assert_eq!(counted.neighbor_reads.load(Ordering::Relaxed), 1);
    assert_eq!(counted.status_batch_reads.load(Ordering::Relaxed), 1);
    assert_eq!(counted.batch_reads.load(Ordering::Relaxed), 1);

    counted.reset_reads();
    counted.overreturn_neighbors.store(true, Ordering::Relaxed);
    assert!(matches!(
        mem.resolved_neighbors_scoped(hub, 1, StatusFilter::ALL)
            .await,
        Err(Error::Backend(message)) if message.contains("bounded request of 1024")
    ));
    assert_eq!(
        *counted.neighbor_limits.lock().unwrap(),
        vec![MAX_INCIDENT_EDGES]
    );
    assert_eq!(counted.status_batch_reads.load(Ordering::Relaxed), 0);
    assert_eq!(counted.batch_reads.load(Ordering::Relaxed), 0);

    counted.reset_reads();
    counted.overreturn_neighbors.store(true, Ordering::Relaxed);
    assert!(matches!(
        mem.neighbors_hydrated(hub, 1).await,
        Err(Error::Backend(message)) if message.contains("bounded request of 1")
    ));
    assert_eq!(counted.batch_reads.load(Ordering::Relaxed), 0);

    counted.reset_reads();
    assert!(matches!(
        mem.resolved_neighbors_scoped(hub, MAX_RESOLVED_NEIGHBORS + 1, StatusFilter::ALL,)
            .await,
        Err(Error::CapacityExceeded {
            resource: "resolved neighbor fanout",
            limit: MAX_RESOLVED_NEIGHBORS,
        })
    ));
    assert_eq!(counted.neighbor_reads.load(Ordering::Relaxed), 0);
    assert_eq!(counted.status_batch_reads.load(Ordering::Relaxed), 0);
    assert_eq!(counted.batch_reads.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn resolved_neighbors_serialize_public_forget_across_the_whole_composite() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let mem = Arc::new(
        Memory::new(
            counted.clone(),
            store.clone(),
            store,
            Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
            Arc::new(SystemClock),
            Config {
                min_similarity_links: 0,
                ..Config::default()
            },
        )
        .with_body_store(Arc::new(InlineStore::new())),
    );
    let hub = mem
        .ingest(Ingest::new("hub", b"", &[], prov()))
        .await
        .unwrap();
    let target = mem
        .ingest(Ingest::new("target", b"", &[], prov()))
        .await
        .unwrap();
    mem.link(hub, target, EdgeKind::Associative, 0.5, None)
        .await
        .unwrap();

    counted.pause_status_batch.store(true, Ordering::SeqCst);
    let reading = {
        let mem = mem.clone();
        tokio::spawn(async move {
            mem.resolved_neighbors_scoped(hub, 1, StatusFilter::ACTIVE)
                .await
        })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        counted.status_batch_started.notified(),
    )
    .await
    .expect("resolved read reached its paused status batch");

    let mut forgetting = {
        let mem = mem.clone();
        tokio::spawn(async move { mem.forget(ColdPath::acquire(), target).await })
    };
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut forgetting)
            .await
            .is_err(),
        "forget must wait until adjacency, status, and hydration finish"
    );

    counted.release_status_batch.notify_one();
    let snapshot = reading.await.unwrap().unwrap();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].neighbor.node, target);
    assert_eq!(snapshot[0].node.id(), target);
    assert!(forgetting.await.unwrap().unwrap());
    assert!(mem.get_node(target).await.unwrap().is_none());

    // The snapshot is coherent at the call boundary, not a standing lease on
    // returned values: a later mutation may make it stale, and callers then
    // issue another read.
}

#[tokio::test]
async fn hybrid_rrf_rescues_an_exact_token_the_dense_top_misses() {
    let dim = DEFAULT_DIM;
    let store = Arc::new(MemStore::new(dim));
    let embedder = Arc::new(HashingEmbedder::new(dim));
    let cfg = Config {
        ann_k: 1,
        lexical_k: 1,
        min_similarity_links: 0,
        budget: Budget {
            max_nodes: 1,
            max_depth: 0,
            min_relevance: 0.0,
            relevance_ratio: 0.0,
            explore: 0.0,
            dedup_similarity: 1.0,
            query_conditioning: 0.0,
        },
        ..Config::default()
    };
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        embedder.clone(),
        Arc::new(SystemClock),
        cfg,
    )
    .with_lexical_index(store.clone())
    .with_body_store(Arc::new(InlineStore::new()));
    let exact = mem
        .ingest(Ingest::new(
            "deployment incident ZXQ771",
            b"exact",
            &[],
            prov(),
        ))
        .await
        .unwrap();
    let distractor = mem
        .ingest(Ingest::new(
            "unrelated semantic distractor",
            b"dense",
            &[],
            prov(),
        ))
        .await
        .unwrap();

    // Force the dense leg to be maximally wrong. The public path must still
    // surface the identifier match from the independent summary-BM25 leg.
    let query_vector = embedder.embed_query("ZXQ771").await.unwrap();
    let query_bucket = query_vector.iter().position(|value| *value != 0.0).unwrap();
    let mut orthogonal = vec![0.0; dim];
    orthogonal[(query_bucket + 1) % dim] = 1.0;
    store.upsert(exact, &orthogonal).await.unwrap();
    store.upsert(distractor, &query_vector).await.unwrap();
    let hits = mem.retrieve("ZXQ771").await.unwrap();
    assert_eq!(hits[0].node.id(), exact);
}

#[tokio::test]
async fn graph_fusion_preserves_full_direct_leg_and_admits_an_expansion() {
    let dim = DEFAULT_DIM;
    let store = Arc::new(MemStore::new(dim));
    let embedder = Arc::new(HashingEmbedder::new(dim));
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        embedder.clone(),
        Arc::new(SystemClock),
        Config {
            lexical_k: 0,
            graph_seed_cap: 1,
            similarity_link_threshold: 1.0,
            min_similarity_links: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));

    let direct_a = mem
        .ingest(Ingest::new("alpha beta query", b"", &[], prov()))
        .await
        .unwrap();
    let direct_b = mem
        .ingest(Ingest::new("second direct", b"", &[], prov()))
        .await
        .unwrap();
    let expansion = mem
        .ingest(Ingest::new("graph-only expansion", b"", &[], prov()))
        .await
        .unwrap();

    let query = "alpha beta query";
    let query_vector = embedder.embed_query(query).await.unwrap();
    let empty_axis = query_vector
        .iter()
        .position(|value| value.abs() < f32::EPSILON)
        .expect("hashing query leaves unused vector dimensions");
    let mut second_vector = query_vector.clone();
    second_vector[empty_axis] = 0.5;
    let mut expansion_vector = vec![0.0; dim];
    expansion_vector[empty_axis] = 1.0;
    store.upsert(direct_a, &query_vector).await.unwrap();
    store.upsert(direct_b, &second_vector).await.unwrap();
    store.upsert(expansion, &expansion_vector).await.unwrap();
    // With default query conditioning, this orthogonal expansion propagates at
    // 0.7 * (1 - 0.4) = 0.42 of its root: useful, but below the old erroneous
    // root-relative `relevance_ratio` floor of 0.5.
    mem.link(direct_a, expansion, EdgeKind::Associative, 0.7, None)
        .await
        .unwrap();

    let direct_budget = Budget {
        max_nodes: 3,
        max_depth: 0,
        min_relevance: 0.0,
        relevance_ratio: 0.0,
        explore: 0.0,
        dedup_similarity: 1.0,
        query_conditioning: 0.0,
    };
    let direct = mem
        .retrieve_seeded(query, 2, direct_budget, StatusFilter::ACTIVE, &[])
        .await
        .unwrap();
    assert_eq!(
        direct.iter().map(|hit| hit.node.id()).collect::<Vec<_>>(),
        vec![direct_a, direct_b],
        "depth zero remains the unmodified full direct ordering"
    );

    let fused = mem
        .retrieve_seeded(
            query,
            2,
            Budget {
                max_depth: 1,
                ..direct_budget
            },
            StatusFilter::ACTIVE,
            &[],
        )
        .await
        .unwrap();
    let fused_ids: HashSet<NodeId> = fused.iter().map(|hit| hit.node.id()).collect();
    assert!(fused_ids.contains(&direct_a));
    assert!(
        fused_ids.contains(&direct_b),
        "root cap must not truncate direct hits"
    );
    assert!(
        fused_ids.contains(&expansion),
        "a non-root graph result enters through the independent expansion leg"
    );

    let shipped = mem
        .retrieve_seeded(query, 2, mem.config().budget, StatusFilter::ACTIVE, &[])
        .await
        .unwrap();
    assert!(
        shipped.iter().any(|hit| hit.node.id() == expansion),
        "the shipped relevance ratio must be relative to expansion-top, not direct-root top"
    );
}

/// A fixed clock, for tests that need a deterministic `now` (e.g. a stable rng
/// seed in the consolidation pass).
struct FakeClock(Timestamp);
impl FakeClock {
    fn new(t: Timestamp) -> Self {
        Self(t)
    }
}
impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

/// Current weight of the edge surfaced between two nodes (either direction).
async fn edge_weight(mem: &Memory, from: NodeId, to: NodeId) -> f32 {
    mem.neighbors(from, 64)
        .await
        .unwrap()
        .into_iter()
        .find(|n| n.node == to)
        .map(|n| n.edge.weight())
        .expect("edge present")
}

/// Kind of the edge surfaced between two nodes (either direction), if any.
async fn edge_kind(mem: &Memory, from: NodeId, to: NodeId) -> Option<EdgeKind> {
    mem.neighbors(from, 64)
        .await
        .unwrap()
        .into_iter()
        .find(|n| n.node == to)
        .map(|n| n.edge.kind)
}

#[tokio::test]
async fn ingest_retrieve_then_grounded_feedback_reinforces() {
    let mem = build_with_similarity_prior(Arc::new(SystemClock));

    let tokio_id = mem
        .ingest(Ingest::new(
            "Tokio is an async runtime for Rust",
            b"<tokio body>",
            &["rust", "async"],
            prov(),
        ))
        .await
        .unwrap();
    let rust_id = mem
        .ingest(Ingest::new(
            "Rust async await desugars into state machines",
            b"...",
            &["rust", "async"],
            prov(),
        ))
        .await
        .unwrap();
    mem.ingest(Ingest::new(
        "Sourdough bread needs a long cold ferment",
        b"...",
        &["baking"],
        prov(),
    ))
    .await
    .unwrap();

    // Ranking: the tokio node should win for a tokio-shaped query.
    let results = mem.retrieve("rust async runtime tokio").await.unwrap();
    assert!(!results.is_empty());
    assert!(
        results[0].node.summary().to_lowercase().contains("tokio"),
        "expected tokio node first, got {:?}",
        results[0].node.summary()
    );

    // Bodies resolve on demand, not during retrieval.
    let body = mem.resolve_body(&results[0].node).await.unwrap();
    assert_eq!(body, b"<tokio body>");

    // The similarity prior linked the two rust/async nodes.
    let nbrs = mem.neighbors(tokio_id, 10).await.unwrap();
    assert!(
        nbrs.iter().any(|n| n.edge.kind == EdgeKind::Associative),
        "expected an associative neighbor from the similarity prior"
    );
    let trials = |nbrs: &[mneme_core::ports::Neighbor]| -> u32 {
        nbrs.iter().map(|n| n.edge.trials()).sum()
    };
    let before = trials(&nbrs);

    // Retrieval only plans context: repeating it does not train topology.
    let _ = mem.retrieve("rust async runtime tokio").await.unwrap();
    let after_plan = trials(&mem.neighbors(tokio_id, 10).await.unwrap());
    assert_eq!(after_plan, before);

    // Grounded agent feedback does reinforce the traversed/relevant direction.
    mem.apply_feedback(Some(tokio_id), rust_id, Signal::RelevantNew)
        .await
        .unwrap();
    let after = trials(&mem.neighbors(tokio_id, 10).await.unwrap());
    assert!(
        after > before,
        "grounded use should grow trials: {after} !> {before}"
    );
}

#[tokio::test]
async fn expanded_recall_does_not_reintroduce_hidden_lifecycle_nodes() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let graph: Arc<dyn GraphStore> = store.clone();
    let vectors: Arc<dyn VectorIndex> = store.clone();
    let traversal: Arc<dyn Traversal> = store.clone();
    let mem = Memory::new(
        graph,
        vectors,
        traversal,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            min_similarity_links: 0,
            budget: Budget {
                explore: 0.0,
                relevance_ratio: 0.0,
                dedup_similarity: 1.0,
                query_conditioning: 0.0,
                ..Budget::default()
            },
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));

    let seed = mem
        .ingest(Ingest::new("alpha beta gamma", b"", &[], prov()))
        .await
        .unwrap();
    let candidate = mem
        .ingest(Ingest::new("quartz candidate", b"", &[], prov()))
        .await
        .unwrap();
    let archived = mem
        .ingest(Ingest::new("obsolete archive", b"", &[], prov()))
        .await
        .unwrap();
    store
        .set_status(archived, NodeStatus::Archived)
        .await
        .unwrap();
    mem.link(seed, candidate, EdgeKind::Associative, 1.0, None)
        .await
        .unwrap();
    mem.link(seed, archived, EdgeKind::Associative, 0.99, None)
        .await
        .unwrap();

    let recall = mem.recall_expanded("alpha beta gamma", 1, 8).await.unwrap();
    assert!(!recall.is_empty());
    assert!(recall.iter().all(|hit| hit.node.is_active()));
    assert!(
        recall
            .iter()
            .flat_map(|hit| &hit.neighbors)
            .all(|neighbor| {
                neighbor.node.id() != candidate
                    && neighbor.node.id() != archived
                    && neighbor.node.is_active()
            })
    );
}

#[tokio::test]
async fn feedback_drives_edges_and_merge_candidates() {
    let mem = build(Arc::new(SystemClock));
    let cold = ColdPath::acquire();

    let a = mem
        .ingest(Ingest::new("graph datalog cozo", b"a", &["db"], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("knitting patterns", b"b", &["craft"], prov()))
        .await
        .unwrap();

    // RelevantNew from a→b creates+strengthens the edge (the agent navigated there).
    mem.apply_feedback(Some(a), b, Signal::RelevantNew)
        .await
        .unwrap();
    assert_eq!(
        edge_kind(&mem, a, b).await,
        Some(EdgeKind::Transition),
        "grounded navigation creates a forward-only learned transition"
    );
    assert!(
        mem.neighbors(b, 16)
            .await
            .unwrap()
            .iter()
            .all(|neighbor| neighbor.node != a),
        "the learned transition must not leak backward before reverse feedback"
    );
    let w1 = edge_weight(&mem, a, b).await;
    mem.apply_feedback(Some(a), b, Signal::RelevantNew)
        .await
        .unwrap();
    assert!(
        edge_weight(&mem, a, b).await > w1,
        "relevant feedback strengthens the edge"
    );

    // NotNew banks a merge candidate for the pair.
    assert!(mem.open_merge_candidates(cold).await.unwrap().is_empty());
    mem.apply_feedback(Some(a), b, Signal::NotNew)
        .await
        .unwrap();
    assert_eq!(mem.open_merge_candidates(cold).await.unwrap().len(), 1);

    // Irrelevance is edge-scoped; the node keeps its authored retention metadata.
    mem.apply_feedback(Some(a), b, Signal::Irrelevant)
        .await
        .unwrap();
    assert_eq!(mem.get_node(b).await.unwrap().unwrap().interference(), 0);
    assert!(
        mem.neighbors(a, 10)
            .await
            .unwrap()
            .iter()
            .any(|n| n.node == b && n.edge.interference() > 0)
    );
}

#[tokio::test]
async fn direct_not_new_feedback_has_no_visible_prefix_on_commit_failure_and_retries_cleanly() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let mem = Memory::new(
        counted.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            min_similarity_links: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let prior = mem
        .ingest(Ingest::new("feedback prior", b"", &[], prov()))
        .await
        .unwrap();
    let target = mem
        .ingest(Ingest::new("feedback target", b"", &[], prov()))
        .await
        .unwrap();
    let before = mem.get_node(target).await.unwrap().unwrap();

    counted.fail_feedback_commit.store(true, Ordering::Relaxed);
    assert!(matches!(
        mem.apply_feedback(Some(prior), target, Signal::NotNew)
            .await,
        Err(Error::Backend(_))
    ));
    let after_failure = mem.get_node(target).await.unwrap().unwrap();
    assert_eq!(
        (
            after_failure.grounded_use_count(),
            after_failure.last_grounded_use(),
            after_failure.status(),
            after_failure.interference(),
        ),
        (
            before.grounded_use_count(),
            before.last_grounded_use(),
            before.status(),
            before.interference(),
        ),
        "a rejected graph commit cannot leak the node prefix"
    );
    assert!(store.get_edge(prior, target).await.unwrap().is_none());
    assert!(
        store
            .open_merge_candidates(ColdPath::acquire())
            .await
            .unwrap()
            .is_empty(),
        "candidate observation must share the node/edge commit point"
    );

    mem.apply_feedback(Some(prior), target, Signal::NotNew)
        .await
        .unwrap();
    assert_eq!(counted.feedback_commits.load(Ordering::Relaxed), 2);
    assert_eq!(
        mem.get_node(target)
            .await
            .unwrap()
            .unwrap()
            .grounded_use_count(),
        before.grounded_use_count() + 1
    );
    assert!(store.get_edge(prior, target).await.unwrap().is_some());
    let merges = store
        .open_merge_candidates(ColdPath::acquire())
        .await
        .unwrap();
    assert_eq!(merges.len(), 1);
    assert_eq!(
        merges[0].between,
        unordered_pair::UnorderedPair(prior, target)
    );
    assert_eq!(merges[0].observations, 1);
}

#[cfg(feature = "cozo")]
#[tokio::test]
async fn direct_not_new_feedback_commits_node_edge_and_observation_in_real_cozo() {
    let (mem, store) = build_with_cozo_store(
        Arc::new(SystemClock),
        Config {
            min_similarity_links: 0,
            ..Config::default()
        },
    );
    let prior = mem
        .ingest(Ingest::new("cozo feedback prior", b"", &[], prov()))
        .await
        .unwrap();
    let target = mem
        .ingest(Ingest::new("cozo feedback target", b"", &[], prov()))
        .await
        .unwrap();

    mem.apply_feedback(Some(prior), target, Signal::NotNew)
        .await
        .unwrap();

    assert_eq!(
        store
            .get_node(target)
            .await
            .unwrap()
            .unwrap()
            .grounded_use_count(),
        1
    );
    assert!(store.get_edge(prior, target).await.unwrap().is_some());
    let merges = store
        .open_merge_candidates(ColdPath::acquire())
        .await
        .unwrap();
    assert_eq!(merges.len(), 1);
    assert_eq!(
        merges[0].between,
        unordered_pair::UnorderedPair(prior, target)
    );
    assert_eq!(merges[0].observations, 1);
}

#[tokio::test]
async fn receipt_feedback_is_atomic_idempotent_and_edge_scoped() {
    let (mem, store) = build_with_store(Arc::new(SystemClock), Config::default());
    let a = mem
        .ingest(Ingest::new("receipt source", b"", &[], prov()))
        .await
        .unwrap();
    let candidate = mem
        .ingest(Ingest::new("receipt candidate", b"", &[], prov()))
        .await
        .unwrap();
    let unused = mem
        .ingest(Ingest::new("receipt unused", b"", &[], prov()))
        .await
        .unwrap();
    mem.link(a, candidate, EdgeKind::Associative, 0.2, None)
        .await
        .unwrap();
    mem.link(candidate, unused, EdgeKind::Associative, 0.4, None)
        .await
        .unwrap();

    let events = [
        ReceiptFeedback::routed(
            mneme_engine::ObservedRoute::new(a, candidate, a, candidate).unwrap(),
            true,
        ),
        ReceiptFeedback::routed(
            mneme_engine::ObservedRoute::new(candidate, unused, candidate, unused).unwrap(),
            false,
        ),
    ];
    let retry = FeedbackRetryScope::new("test-epoch", 1, 1).unwrap();
    let first = mem
        .apply_receipt_feedback_idempotent("receipt-set-v1:test", &retry, &events)
        .await
        .unwrap();
    assert_eq!(first.commit, FeedbackCommitOutcome::Applied);
    assert_eq!((first.reinforced, first.interfered), (1, 1));

    let candidate_after = mem.get_node(candidate).await.unwrap().unwrap();
    assert_eq!(candidate_after.grounded_use_count(), 1);
    assert_eq!(candidate_after.status(), NodeStatus::Active);
    let unused_after = mem.get_node(unused).await.unwrap().unwrap();
    assert_eq!(unused_after.interference(), 0);
    let relevant_edge = store.get_edge(a, candidate).await.unwrap().unwrap();
    let unused_edge = store.get_edge(candidate, unused).await.unwrap().unwrap();

    let replay = mem
        .apply_receipt_feedback_idempotent("receipt-set-v1:test", &retry, &events)
        .await
        .unwrap();
    assert_eq!(replay.commit, FeedbackCommitOutcome::AlreadyApplied);
    assert_eq!(
        mem.get_node(candidate)
            .await
            .unwrap()
            .unwrap()
            .grounded_use_count(),
        1,
        "a replay cannot double-credit grounded use"
    );
    assert_eq!(
        store
            .get_edge(a, candidate)
            .await
            .unwrap()
            .unwrap()
            .trials(),
        relevant_edge.trials()
    );
    assert_eq!(
        store
            .get_edge(candidate, unused)
            .await
            .unwrap()
            .unwrap()
            .interference(),
        unused_edge.interference()
    );

    let mut different = events;
    different[1].relevant = true;
    assert!(matches!(
        mem.apply_receipt_feedback_idempotent("receipt-set-v1:test", &retry, &different)
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(
        mem.get_node(unused).await.unwrap().unwrap().interference(),
        unused_after.interference(),
        "same-key/different-payload rejection mutates nothing"
    );

    assert!(mem.forget(ColdPath::acquire(), unused).await.unwrap());
    let replay_after_forget = mem
        .apply_receipt_feedback_idempotent("receipt-set-v1:test", &retry, &events)
        .await
        .unwrap();
    assert_eq!(
        replay_after_forget.commit,
        FeedbackCommitOutcome::AlreadyApplied,
        "an exact receipt replay reaches the ledger after a used target is forgotten"
    );
    assert_eq!(
        mem.get_node(candidate)
            .await
            .unwrap()
            .unwrap()
            .grounded_use_count(),
        1
    );
}

#[tokio::test]
async fn public_link_rejects_a_missing_endpoint_without_creating_an_orphan() {
    let mem = build(Arc::new(SystemClock));
    let from = mem
        .ingest(Ingest::new("surviving source", b"a", &[], prov()))
        .await
        .unwrap();
    let gone = mem
        .ingest(Ingest::new("forgotten target", b"b", &[], prov()))
        .await
        .unwrap();
    assert!(mem.forget(ColdPath::acquire(), gone).await.unwrap());

    assert!(matches!(
        mem.link(from, gone, EdgeKind::Associative, 0.8, None).await,
        Err(Error::NotFound)
    ));
    assert!(mem.neighbors(from, 16).await.unwrap().is_empty());
}

#[tokio::test]
async fn stale_feedback_cannot_recreate_an_edge_or_merge_candidate() {
    let mem = build(Arc::new(SystemClock));
    let prior = mem
        .ingest(Ingest::new("walk prior", b"a", &[], prov()))
        .await
        .unwrap();
    let gone = mem
        .ingest(Ingest::new("stale receipt target", b"b", &[], prov()))
        .await
        .unwrap();
    assert!(mem.forget(ColdPath::acquire(), gone).await.unwrap());

    mem.apply_feedback(Some(prior), gone, Signal::NotNew)
        .await
        .unwrap();

    assert!(mem.neighbors(prior, 16).await.unwrap().is_empty());
    assert!(
        mem.open_merge_candidates(ColdPath::acquire())
            .await
            .unwrap()
            .is_empty()
    );
}

/// Explicit negative feedback accrues edge interference, then a bounded sweep
/// consumes it exactly once without changing node confidence or membership.
#[tokio::test]
async fn decay_sweep_is_idempotent() {
    let mem = build(Arc::new(SystemClock));
    let cold = ColdPath::acquire();

    let a = mem
        .ingest(Ingest::new(
            "graph datalog cozo query",
            b"a",
            &["db"],
            prov(),
        ))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new(
            "unrelated knitting pattern",
            b"b",
            &["craft"],
            prov(),
        ))
        .await
        .unwrap();
    mem.link(a, b, EdgeKind::Associative, 0.5, None)
        .await
        .unwrap();
    for _ in 0..5 {
        mem.apply_feedback(Some(a), b, Signal::Irrelevant)
            .await
            .unwrap();
    }

    // First sweep consumes the banked interference and does real work.
    let first = mem.decay_sweep(cold).await.unwrap();
    assert!(first.edges_decayed > 0, "first sweep should act");
    let conf_after = mem.get_node(b).await.unwrap().unwrap().confidence();
    let weight_after = edge_weight(&mem, a, b).await;

    // Second sweep, nothing emitted in between ⇒ a no-op: counters were reset.
    let second = mem.decay_sweep(cold).await.unwrap();
    assert_eq!(second.edges_decayed, 0);
    assert_eq!(
        mem.get_node(b).await.unwrap().unwrap().confidence(),
        conf_after
    );
    assert_eq!(edge_weight(&mem, a, b).await, weight_after);
}

#[tokio::test]
async fn supersede_resolves_contradiction_and_archives_loser() {
    let mem = build(Arc::new(SystemClock));

    let updated = mem
        .ingest(Ingest::new(
            "the answer is 42 updated",
            b"x",
            &["fact"],
            prov(),
        ))
        .await
        .unwrap();
    let stale = mem
        .ingest(Ingest::new(
            "the answer is 7 stale",
            b"y",
            &["fact", "core"],
            prov(),
        ))
        .await
        .unwrap();

    let cold = ColdPath::acquire();
    mem.observe_contradiction(cold, updated, stale)
        .await
        .unwrap();
    assert_eq!(mem.open_contradictions(cold).await.unwrap().len(), 1);

    mem.supersede(cold, updated, stale).await.unwrap();
    assert!(
        mem.open_contradictions(cold).await.unwrap().is_empty(),
        "supersession should close the contradiction"
    );

    // A directional Supersedes edge surfaces only on its target (the loser), as
    // an incoming hop to the winner — the winner isn't dragged back to it.
    let from_loser = mem.neighbors(stale, 10).await.unwrap();
    assert!(
        from_loser
            .iter()
            .any(|n| n.edge.kind == EdgeKind::Supersedes && n.incoming && n.node == updated),
        "loser should see its winner via an incoming supersedes edge"
    );
    assert!(
        mem.neighbors(updated, 10)
            .await
            .unwrap()
            .iter()
            .all(|n| n.edge.kind != EdgeKind::Supersedes),
        "winner should not surface the supersedes edge"
    );
    let archived = mem.get_node(stale).await.unwrap().unwrap();
    assert!(archived.is_archived(), "loser should be archived");
    assert_eq!(mem.resolve_body(&archived).await.unwrap(), b"y");
    assert!(
        mem.core(cold)
            .await
            .unwrap()
            .iter()
            .all(|node| node.id() != stale)
    );
    assert!(
        mem.retrieve("the answer is 7 stale")
            .await
            .unwrap()
            .iter()
            .all(|hit| hit.node.id() != stale)
    );
    let recalled = mem
        .recall_expanded("the answer is 7 stale", 3, 10)
        .await
        .unwrap();
    assert!(recalled.iter().all(|hit| {
        hit.node.id() != stale
            && hit
                .neighbors
                .iter()
                .all(|neighbor| neighbor.node.id() != stale)
    }));
}

#[tokio::test]
async fn supersede_lost_acknowledgement_retries_without_changing_authored_confidence() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let mem = Memory::new(
        counted.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let winner = mem
        .ingest(Ingest::new("replacement", b"", &[], prov()))
        .await
        .unwrap();
    let loser = mem
        .ingest(Ingest::new("stale", b"", &[], prov()))
        .await
        .unwrap();
    let before = mem.get_node(loser).await.unwrap().unwrap().confidence();

    counted
        .fail_supersede_after_commit
        .store(true, Ordering::Relaxed);
    assert!(matches!(
        mem.supersede(ColdPath::acquire(), winner, loser).await,
        Err(Error::Backend(message)) if message.contains("lost supersede acknowledgement")
    ));
    let committed = mem.get_node(loser).await.unwrap().unwrap();
    assert_eq!(committed.confidence(), before);
    assert!(committed.is_archived());

    mem.supersede(ColdPath::acquire(), winner, loser)
        .await
        .unwrap();
    assert_eq!(
        mem.get_node(loser).await.unwrap().unwrap().confidence(),
        before,
        "retry after an ambiguous success preserves authored confidence"
    );
}

#[tokio::test]
async fn superseded_capture_replay_preserves_archive() {
    let mem = build(Arc::new(SystemClock));
    let capture = || {
        Capture::new(
            "codex",
            "session/turn/claim",
            "codex://session/turn#claim",
            Some("session"),
            Some("turn"),
            "stale captured advice",
            b"original evidence",
            &["core"],
        )
    };
    let stale = mem.capture(capture()).await.unwrap();
    let winner = mem
        .ingest(Ingest::new("corrected advice", b"correction", &[], prov()))
        .await
        .unwrap();
    mem.supersede(ColdPath::acquire(), winner, stale.id)
        .await
        .unwrap();
    let retry = mem.capture(capture()).await.unwrap();
    assert_eq!(retry.id, stale.id);
    assert!(retry.replayed);
    assert!(mem.get_node(stale.id).await.unwrap().unwrap().is_archived());
    assert!(mem.core(ColdPath::acquire()).await.unwrap().is_empty());
}

#[tokio::test]
async fn consolidate_follows_archived_superseded_endpoint_to_winner() {
    let cfg = Config {
        bridge_probability: 1.0,
        similarity_link_threshold: 0.99,
        min_similarity_links: 0,
        ..Config::default()
    };
    let mem = build_with(Arc::new(FakeClock::new(1_700_000_000_000)), cfg);
    let winner = mem
        .ingest(Ingest::new("replacement", b"", &[], prov()))
        .await
        .unwrap();
    let loser = mem
        .ingest(Ingest::new("obsolete", b"", &[], prov()))
        .await
        .unwrap();
    let other = mem
        .ingest(Ingest::new("unrelated", b"", &[], prov()))
        .await
        .unwrap();
    mem.supersede(ColdPath::acquire(), winner, loser)
        .await
        .unwrap();
    assert!(mem.get_node(loser).await.unwrap().unwrap().is_archived());
    let bridges = mem
        .consolidate(ColdPath::acquire(), &[loser, other])
        .await
        .unwrap();
    assert_eq!(bridges.len(), 1);
    assert!(
        matches!(bridges[0], (a, b) if (a == winner && b == other) || (a == other && b == winner))
    );
}

#[tokio::test]
async fn semantic_pair_apis_reject_self_pairs_without_mutation() {
    let (mem, store) = build_with_store(Arc::new(SystemClock), Config::default());
    let cold = ColdPath::acquire();
    let id = mem
        .ingest(Ingest::new("one node", b"body", &[], prov()))
        .await
        .unwrap();

    assert!(matches!(
        mem.link(id, id, EdgeKind::Associative, 0.5, None).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        mem.observe_contradiction(cold, id, id).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        mem.reconcile(cold, id, id, Resolution::ContextDependent)
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        mem.resolve_merge(cold, id, id, MergeResolution::Keep).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        mem.supersede(cold, id, id).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        mem.merge_full(cold, id, id).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(
        store.get_edge(id, id).await.unwrap().is_none(),
        "the rejected link and supersede must not create a self-loop"
    );
    assert!(mem.open_contradictions(cold).await.unwrap().is_empty());
    assert!(mem.open_merge_candidates(cold).await.unwrap().is_empty());
    let nodes = mem.all_nodes(cold).await.unwrap();
    assert_eq!(
        nodes.len(),
        1,
        "rejected pair operations must not add nodes"
    );
    assert!(
        nodes[0].is_active(),
        "supersede must reject before archiving"
    );
}

#[tokio::test]
async fn public_merge_resolution_rejects_new_partial_verdicts() {
    let (mem, store) = build_with_store(Arc::new(SystemClock), Config::default());
    let cold = ColdPath::acquire();
    let a = mem
        .ingest(Ingest::new("first parent", b"a", &[], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("second parent", b"b", &[], prov()))
        .await
        .unwrap();
    store.observe_merge_candidate(a, b, 1).await.unwrap();

    assert!(matches!(
        mem.resolve_merge(cold, a, b, MergeResolution::Partial)
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(
        mem.open_merge_candidates(cold).await.unwrap().len(),
        1,
        "rejecting the unsupported verdict must leave the candidate open"
    );
    mem.resolve_merge(cold, a, b, MergeResolution::Keep)
        .await
        .unwrap();
    assert!(mem.open_merge_candidates(cold).await.unwrap().is_empty());
}

#[tokio::test]
async fn merge_full_repoints_and_archives_loser() {
    let (mem, store) = build_with_store(Arc::new(SystemClock), Config::default());
    let cold = ColdPath::acquire();

    let winner = mem
        .ingest(Ingest::new("primary", b"x", &[], prov()))
        .await
        .unwrap();
    let loser = mem
        .ingest(Ingest::new("duplicate", b"y", &[], prov()))
        .await
        .unwrap();
    let other = mem
        .ingest(Ingest::new("unrelated", b"z", &[], prov()))
        .await
        .unwrap();
    mem.link(loser, other, EdgeKind::Associative, 0.6, None)
        .await
        .unwrap();
    store
        .observe_merge_candidate(winner, loser, 1)
        .await
        .unwrap();

    mem.merge_full(cold, winner, loser).await.unwrap();

    assert!(mem.get_node(loser).await.unwrap().unwrap().is_archived());
    assert!(
        mem.neighbors(winner, 16)
            .await
            .unwrap()
            .iter()
            .any(|n| n.node == other),
        "winner inherited the loser's edge to `other`"
    );
    assert!(
        mem.neighbors(loser, 16).await.unwrap().is_empty(),
        "the loser's edges were dropped"
    );
}

#[tokio::test]
async fn merge_full_normalizes_both_endpoints_and_converges_duplicate_pairs() {
    let (mem, store) = build_with_store(
        Arc::new(SystemClock),
        Config {
            min_similarity_links: 0,
            budget: Budget {
                explore: 0.0,
                ..Budget::default()
            },
            ..Config::default()
        },
    );
    assert_merge_full_normalization(mem, store).await;
}

#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_merge_full_normalizes_both_endpoints_and_converges_duplicate_pairs() {
    let (mem, store) = build_with_cozo_store(
        Arc::new(SystemClock),
        Config {
            min_similarity_links: 0,
            budget: Budget {
                explore: 0.0,
                ..Budget::default()
            },
            ..Config::default()
        },
    );
    assert_merge_full_normalization(mem, store).await;
}

async fn assert_merge_full_normalization(mem: Memory, store: Arc<dyn GraphStore>) {
    let cold = ColdPath::acquire();

    let winner = mem
        .ingest(Ingest::new("winner", b"w", &[], prov()))
        .await
        .unwrap();
    let loser = mem
        .ingest(Ingest::new("loser", b"l", &[], prov()))
        .await
        .unwrap();
    let outgoing = mem
        .ingest(Ingest::new("outgoing", b"o", &[], prov()))
        .await
        .unwrap();
    let incoming = mem
        .ingest(Ingest::new("incoming", b"i", &[], prov()))
        .await
        .unwrap();
    let novel = mem
        .ingest(Ingest::new("novel", b"n", &[], prov()))
        .await
        .unwrap();

    // Every *incident* edge that collapses onto `(winner, winner)` must
    // disappear, including a loser self-loop and both winner/loser directions.
    // A self-loop the winner already owned is unrelated state and must survive
    // without being reinforced by those collapsed rows.
    // Self-loops remain valid low-level graph records, but the public semantic
    // assertion API rejects them. Seed these storage-normalization fixtures
    // through the adapter boundary deliberately.
    let mut winner_self = Edge::new(winner, winner, 0.33, EdgeKind::Bridge, 0);
    winner_self.anchor = Some(BodySpan::new(0, 1));
    store.put_edge(&winner_self).await.unwrap();
    store
        .put_edge(&Edge::new(loser, loser, 0.9, EdgeKind::Associative, 0))
        .await
        .unwrap();
    mem.link(loser, winner, EdgeKind::Transition, 0.8, None)
        .await
        .unwrap();
    mem.link(winner, loser, EdgeKind::Supersedes, 0.7, None)
        .await
        .unwrap();

    // Donor edges in both directions converge on already-existing canonical
    // pairs. The destination edge owns kind/anchor while both rows' counters
    // remain evidence. A merge is not a traversal, so it must not manufacture a
    // reinforcement event. Keep each row's pending interference <= its trials.
    let outgoing_anchor = BodySpan::new(1, 2);
    store
        .put_edge(&Edge::from_stored(
            winner,
            outgoing,
            EdgeKind::Associative,
            Some(outgoing_anchor),
            0.6,
            10,
            3,
            2,
        ))
        .await
        .unwrap();
    store
        .put_edge(&Edge::from_stored(
            loser,
            outgoing,
            EdgeKind::Bridge,
            Some(BodySpan::new(0, 1)),
            0.95,
            20,
            7,
            7,
        ))
        .await
        .unwrap();
    let incoming_anchor = BodySpan::new(0, 1);
    store
        .put_edge(&Edge::from_stored(
            incoming,
            winner,
            EdgeKind::Transition,
            Some(incoming_anchor),
            0.55,
            11,
            4,
            1,
        ))
        .await
        .unwrap();
    store
        .put_edge(&Edge::from_stored(
            incoming,
            loser,
            EdgeKind::DerivedFrom,
            Some(BodySpan::new(1, 2)),
            0.99,
            9,
            6,
            2,
        ))
        .await
        .unwrap();
    mem.link(
        loser,
        novel,
        EdgeKind::Transition,
        0.42,
        Some(BodySpan::new(0, 1)),
    )
    .await
    .unwrap();
    store
        .observe_merge_candidate(winner, loser, 1)
        .await
        .unwrap();

    mem.merge_full(cold, winner, loser).await.unwrap();

    let edges = store.all_edges(cold).await.unwrap();
    assert!(
        edges
            .iter()
            .all(|edge| edge.from != loser && edge.to != loser),
        "no canonical edge may retain the archived loser as either endpoint"
    );
    let winner_self: Vec<_> = edges
        .iter()
        .filter(|edge| edge.from == winner && edge.to == winner)
        .collect();
    assert_eq!(winner_self.len(), 1);
    assert_eq!(winner_self[0].kind, EdgeKind::Bridge);
    assert_eq!(winner_self[0].weight(), 0.33);
    let anchor = winner_self[0].anchor.unwrap();
    assert_eq!((anchor.start, anchor.end), (0, 1));
    assert_eq!(
        edges
            .iter()
            .filter(|edge| edge.from == winner && edge.to == outgoing)
            .count(),
        1,
        "duplicate outgoing pairs must converge to one canonical row"
    );
    assert_eq!(
        edges
            .iter()
            .filter(|edge| edge.from == incoming && edge.to == winner)
            .count(),
        1,
        "duplicate incoming pairs must converge to one canonical row"
    );

    let outgoing_edge = store.get_edge(winner, outgoing).await.unwrap().unwrap();
    assert_eq!(outgoing_edge.kind, EdgeKind::Associative);
    assert_eq!(outgoing_edge.weight(), 0.95);
    assert_eq!(outgoing_edge.last_reinforced(), 20);
    assert_eq!(outgoing_edge.trials(), 10);
    assert_eq!(outgoing_edge.interference(), 9);
    let anchor = outgoing_edge.anchor.unwrap();
    assert_eq!((anchor.start, anchor.end), (1, 2));

    let incoming_edge = store.get_edge(incoming, winner).await.unwrap().unwrap();
    assert_eq!(incoming_edge.kind, EdgeKind::Transition);
    assert_eq!(incoming_edge.weight(), 0.99);
    assert_eq!(incoming_edge.last_reinforced(), 11);
    assert_eq!(incoming_edge.trials(), 10);
    assert_eq!(incoming_edge.interference(), 3);
    let anchor = incoming_edge.anchor.unwrap();
    assert_eq!((anchor.start, anchor.end), (0, 1));

    let novel_edge = store.get_edge(winner, novel).await.unwrap().unwrap();
    assert_eq!(novel_edge.kind, EdgeKind::Transition);
    assert_eq!(novel_edge.weight(), 0.42);
    assert_eq!(novel_edge.trials(), 0);
    assert!(
        novel_edge.anchor.is_none(),
        "a donor anchor must not be transplanted onto a changed semantic pair"
    );
}

#[tokio::test]
async fn merge_full_retry_is_a_noop_and_conflicting_reuse_fails_closed() {
    let (mem, store) = build_with_store(Arc::new(FakeClock::new(77)), Config::default());
    assert_merge_full_retry_contract(mem, store).await;
}

#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_merge_full_retry_is_a_noop_and_conflicting_reuse_fails_closed() {
    let (mem, store) = build_with_cozo_store(Arc::new(FakeClock::new(77)), Config::default());
    assert_merge_full_retry_contract(mem, store).await;
}

async fn assert_merge_full_retry_contract(mem: Memory, store: Arc<dyn GraphStore>) {
    let cold = ColdPath::acquire();
    let winner = mem
        .ingest(Ingest::new("winner", b"w", &[], prov()))
        .await
        .unwrap();
    let loser = mem
        .ingest(Ingest::new("loser", b"l", &[], prov()))
        .await
        .unwrap();
    let other_winner = mem
        .ingest(Ingest::new("other winner", b"ow", &[], prov()))
        .await
        .unwrap();
    let neighbor = mem
        .ingest(Ingest::new("neighbor", b"n", &[], prov()))
        .await
        .unwrap();
    let target_db = Ulid::from(119_000u128);
    let remote_target = NodeId(Ulid::from(119_001u128));

    store
        .put_edge(&Edge::from_stored(
            loser,
            neighbor,
            EdgeKind::Transition,
            None,
            0.7,
            12,
            4,
            3,
        ))
        .await
        .unwrap();
    mem.link_remote(loser, target_db, remote_target, 0.8)
        .await
        .unwrap();
    store
        .observe_merge_candidate(winner, loser, 10)
        .await
        .unwrap();
    // Keep a second open pair involving the soon-to-be archived loser. It
    // proves a different-winner request cannot treat the archived node as a
    // fresh donor merely because that pair has no retry record.
    store
        .observe_merge_candidate(other_winner, loser, 11)
        .await
        .unwrap();

    mem.merge_full(cold, winner, loser).await.unwrap();
    let inherited = store.get_edge(winner, neighbor).await.unwrap().unwrap();
    let inherited_state = (
        inherited.kind,
        inherited.weight().to_bits(),
        inherited.last_reinforced(),
        inherited.trials(),
        inherited.interference(),
    );
    let remote_state = collect_remote_for_test(&mem, winner).await;
    assert!(mem.get_node(loser).await.unwrap().unwrap().is_archived());
    assert_eq!(store.open_merge_candidates(cold).await.unwrap().len(), 1);

    // The ledger must be consulted before attempting to re-read the archived
    // loser and its now-empty adjacency.
    mem.merge_full(cold, winner, loser).await.unwrap();
    let replayed = store.get_edge(winner, neighbor).await.unwrap().unwrap();
    assert_eq!(
        (
            replayed.kind,
            replayed.weight().to_bits(),
            replayed.last_reinforced(),
            replayed.trials(),
            replayed.interference(),
        ),
        inherited_state,
        "an exact retry must not synthesize another evidence event"
    );
    let replayed_remote = collect_remote_for_test(&mem, winner).await;
    assert_eq!(replayed_remote.len(), remote_state.len());
    assert_eq!(replayed_remote[0].target, remote_state[0].target);
    assert_eq!(
        replayed_remote[0].weight().to_bits(),
        remote_state[0].weight().to_bits()
    );

    assert!(matches!(
        mem.merge_full(cold, loser, winner).await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        mem.merge_full(cold, other_winner, loser).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(store.open_merge_candidates(cold).await.unwrap().len(), 1);
    assert!(store.get_edge(winner, neighbor).await.unwrap().is_some());
    assert!(
        store
            .get_edge(other_winner, neighbor)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn merge_full_handles_the_maximal_duplicate_local_and_remote_unions() {
    let (mem, store) = build_with_store(Arc::new(FakeClock::new(88)), Config::default());
    assert_merge_full_maximal_duplicate_union(mem, store).await;
}

#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_merge_full_handles_the_maximal_duplicate_local_and_remote_unions() {
    let (mem, store) = build_with_cozo_store(Arc::new(FakeClock::new(88)), Config::default());
    assert_merge_full_maximal_duplicate_union(mem, store).await;
}

async fn assert_merge_full_maximal_duplicate_union(mem: Memory, store: Arc<dyn GraphStore>) {
    let cold = ColdPath::acquire();
    let winner = NodeId(Ulid::from(115_000u128));
    let loser = NodeId(Ulid::from(115_001u128));
    store.put_node(&raw_active_node(winner)).await.unwrap();
    store.put_node(&raw_active_node(loser)).await.unwrap();

    // Both donor rows are saturated with valid pending interference; the merge
    // must preserve their additive counters while collapsing every duplicate.
    for offset in 0..MAX_INCIDENT_EDGES {
        let target = NodeId(Ulid::from(116_000u128 + offset as u128));
        store.put_node(&raw_active_node(target)).await.unwrap();
        store
            .put_edge(&Edge::from_stored(
                winner,
                target,
                EdgeKind::Associative,
                None,
                0.2,
                3,
                2,
                2,
            ))
            .await
            .unwrap();
        store
            .put_edge(&Edge::from_stored(
                loser,
                target,
                EdgeKind::Bridge,
                None,
                0.8,
                7,
                4,
                4,
            ))
            .await
            .unwrap();
    }

    let target_db = Ulid::from(117_000u128);
    for offset in 0..MAX_REMOTE_EDGES_PER_SOURCE {
        let target = NodeId(Ulid::from(118_000u128 + offset as u128));
        mem.link_remote(winner, target_db, target, 0.3)
            .await
            .unwrap();
        mem.link_remote(loser, target_db, target, 0.9)
            .await
            .unwrap();
    }
    store
        .observe_merge_candidate(winner, loser, 20)
        .await
        .unwrap();

    mem.merge_full(cold, winner, loser).await.unwrap();

    let final_edges = store.all_edges(cold).await.unwrap();
    assert_eq!(final_edges.len(), MAX_INCIDENT_EDGES);
    assert!(
        final_edges
            .iter()
            .all(|edge| edge.from == winner && edge.to != loser)
    );
    let sample = &final_edges[0];
    assert_eq!(sample.kind, EdgeKind::Associative);
    assert_eq!(sample.weight(), 0.8);
    assert_eq!(sample.last_reinforced(), 7);
    assert_eq!(sample.trials(), 6);
    assert_eq!(sample.interference(), 6);

    let remotes = collect_remote_for_test(&mem, winner).await;
    assert_eq!(remotes.len(), MAX_REMOTE_EDGES_PER_SOURCE);
    assert!(remotes.iter().all(|edge| edge.weight() == 0.9));
    assert!(collect_remote_for_test(&mem, loser).await.is_empty());
    assert!(mem.get_node(loser).await.unwrap().unwrap().is_archived());

    // The max-sized retry still exits at the ledger and performs no 2,048-row
    // incident or 512-row remote read.
    mem.merge_full(cold, winner, loser).await.unwrap();
    assert_eq!(
        store.all_edges(cold).await.unwrap().len(),
        MAX_INCIDENT_EDGES
    );
}

#[tokio::test]
async fn merge_full_rewrite_succeeds_when_unchanged_endpoint_is_at_capacity() {
    let (mem, store) = build_with_store(
        Arc::new(SystemClock),
        Config {
            min_similarity_links: 0,
            ..Config::default()
        },
    );
    let cold = ColdPath::acquire();
    let winner = NodeId(Ulid::from(120_000u128));
    let loser = NodeId(Ulid::from(120_001u128));
    let hub = NodeId(Ulid::from(120_002u128));
    for id in [winner, loser, hub] {
        store.put_node(&raw_active_node(id)).await.unwrap();
    }
    store
        .put_edge(&Edge::new(loser, hub, 0.8, EdgeKind::Transition, 1))
        .await
        .unwrap();
    for offset in 0..MAX_INCIDENT_EDGES - 1 {
        let filler = NodeId(Ulid::from(121_000u128 + offset as u128));
        store.put_node(&raw_active_node(filler)).await.unwrap();
        store
            .put_edge(&Edge::new(hub, filler, 0.2, EdgeKind::Transition, 1))
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .all_edges(cold)
            .await
            .unwrap()
            .iter()
            .filter(|edge| edge.from == hub || edge.to == hub)
            .count(),
        MAX_INCIDENT_EDGES
    );
    store
        .observe_merge_candidate(winner, loser, 1)
        .await
        .unwrap();

    mem.merge_full(cold, winner, loser).await.unwrap();

    assert!(store.get_edge(loser, hub).await.unwrap().is_none());
    assert!(store.get_edge(winner, hub).await.unwrap().is_some());
    assert_eq!(
        store
            .all_edges(cold)
            .await
            .unwrap()
            .iter()
            .filter(|edge| edge.from == hub || edge.to == hub)
            .count(),
        MAX_INCIDENT_EDGES,
        "rewriting an incident row must consume the donor's released slot"
    );
}

#[tokio::test]
async fn merge_full_rejects_an_over_capacity_union_before_mutating_edges() {
    let (mem, store) = build_with_store(
        Arc::new(SystemClock),
        Config {
            min_similarity_links: 0,
            ..Config::default()
        },
    );
    let cold = ColdPath::acquire();
    let winner = NodeId(Ulid::from(130_000u128));
    let loser = NodeId(Ulid::from(130_001u128));
    for id in [winner, loser] {
        store.put_node(&raw_active_node(id)).await.unwrap();
    }

    let each = MAX_INCIDENT_EDGES / 2 + 1;
    for offset in 0..each {
        let winner_target = NodeId(Ulid::from(131_000u128 + offset as u128));
        let loser_target = NodeId(Ulid::from(132_000u128 + offset as u128));
        for id in [winner_target, loser_target] {
            store.put_node(&raw_active_node(id)).await.unwrap();
        }
        store
            .put_edge(&Edge::new(
                winner,
                winner_target,
                0.3,
                EdgeKind::Transition,
                1,
            ))
            .await
            .unwrap();
        store
            .put_edge(&Edge::new(
                loser,
                loser_target,
                0.4,
                EdgeKind::Transition,
                1,
            ))
            .await
            .unwrap();
    }
    let before = store.all_edges(cold).await.unwrap().len();
    store
        .observe_merge_candidate(winner, loser, 1)
        .await
        .unwrap();

    assert!(matches!(
        mem.merge_full(cold, winner, loser).await,
        Err(Error::CapacityExceeded {
            resource: "incident edge degree",
            limit: MAX_INCIDENT_EDGES,
        })
    ));
    assert_eq!(store.all_edges(cold).await.unwrap().len(), before);
    assert_eq!(
        store.neighbors(loser, usize::MAX).await.unwrap().len(),
        each,
        "capacity rejection must leave every donor row available for retry"
    );
    assert!(store.get_node(loser).await.unwrap().unwrap().is_active());
}

#[tokio::test]
async fn merge_full_reparents_remote_edges_and_keeps_the_strongest_duplicate() {
    let (mem, store) = build_with_store(Arc::new(SystemClock), Config::default());
    assert_merge_full_remote_reparenting(mem, store).await;
}

#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_merge_full_reparents_remote_edges_and_keeps_the_strongest_duplicate() {
    let (mem, store) = build_with_cozo_store(
        Arc::new(SystemClock),
        Config {
            min_similarity_links: 0,
            budget: Budget {
                explore: 0.0,
                ..Budget::default()
            },
            ..Config::default()
        },
    );
    assert_merge_full_remote_reparenting(mem, store).await;
}

async fn assert_merge_full_remote_reparenting(mem: Memory, store: Arc<dyn GraphStore>) {
    let cold = ColdPath::acquire();

    let winner = mem
        .ingest(Ingest::new("winner", b"w", &[], prov()))
        .await
        .unwrap();
    let loser = mem
        .ingest(Ingest::new("loser", b"l", &[], prov()))
        .await
        .unwrap();
    let target_db = Ulid::from(91_000u128);
    let shared_target = NodeId(Ulid::from(91_001u128));
    let donor_only_target = NodeId(Ulid::from(91_002u128));
    let winner_stronger_target = NodeId(Ulid::from(91_003u128));

    mem.link_remote(winner, target_db, shared_target, 0.4)
        .await
        .unwrap();
    mem.link_remote(loser, target_db, shared_target, 0.9)
        .await
        .unwrap();
    mem.link_remote(loser, target_db, donor_only_target, 0.7)
        .await
        .unwrap();
    mem.link_remote(winner, target_db, winner_stronger_target, 0.95)
        .await
        .unwrap();
    mem.link_remote(loser, target_db, winner_stronger_target, 0.2)
        .await
        .unwrap();
    store
        .observe_merge_candidate(winner, loser, 1)
        .await
        .unwrap();

    mem.merge_full(cold, winner, loser).await.unwrap();

    assert!(
        mem.remote_edges_page(loser, None, 8)
            .await
            .unwrap()
            .items
            .is_empty(),
        "the archived loser must not retain remote-edge ownership"
    );
    let winner_remote = mem.remote_edges_page(winner, None, 8).await.unwrap().items;
    assert_eq!(winner_remote.len(), 3);
    assert_eq!(
        winner_remote
            .iter()
            .filter(|edge| edge.target_db == target_db && edge.target == shared_target)
            .count(),
        1,
        "duplicate remote keys must converge to one row"
    );
    assert_eq!(
        winner_remote
            .iter()
            .find(|edge| edge.target == shared_target)
            .unwrap()
            .weight(),
        0.9,
        "a weaker winner-side duplicate must not erase stronger donor evidence"
    );
    assert!(
        winner_remote
            .iter()
            .any(|edge| edge.target == donor_only_target && edge.weight() == 0.7)
    );
    assert_eq!(
        winner_remote
            .iter()
            .find(|edge| edge.target == winner_stronger_target)
            .unwrap()
            .weight(),
        0.95,
        "a weaker donor duplicate must not overwrite the winner's stronger edge"
    );
}

#[tokio::test]
async fn merge_full_rejects_an_over_capacity_remote_union_before_mutation() {
    let (mem, store) = build_with_store(Arc::new(SystemClock), Config::default());
    let cold = ColdPath::acquire();
    let winner = mem
        .ingest(Ingest::new("winner", b"w", &[], prov()))
        .await
        .unwrap();
    let loser = mem
        .ingest(Ingest::new("loser", b"l", &[], prov()))
        .await
        .unwrap();
    let target_db = Ulid::from(140_000u128);
    let each = MAX_REMOTE_EDGES_PER_SOURCE / 2 + 1;
    for offset in 0..each {
        mem.link_remote(
            winner,
            target_db,
            NodeId(Ulid::from(141_000u128 + offset as u128)),
            0.3,
        )
        .await
        .unwrap();
        mem.link_remote(
            loser,
            target_db,
            NodeId(Ulid::from(142_000u128 + offset as u128)),
            0.4,
        )
        .await
        .unwrap();
    }
    store
        .observe_merge_candidate(winner, loser, 1)
        .await
        .unwrap();

    assert!(matches!(
        mem.merge_full(cold, winner, loser).await,
        Err(Error::CapacityExceeded {
            resource: "remote edges per source",
            limit: MAX_REMOTE_EDGES_PER_SOURCE,
        })
    ));
    assert_eq!(collect_remote_for_test(&mem, winner).await.len(), each);
    assert_eq!(collect_remote_for_test(&mem, loser).await.len(), each);
    assert!(mem.get_node(loser).await.unwrap().unwrap().is_active());
}

async fn collect_remote_for_test(mem: &Memory, from: NodeId) -> Vec<mneme_core::RemoteEdge> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = mem
            .remote_edges_page(from, after, MAX_REMOTE_EDGE_PAGE_SIZE)
            .await
            .unwrap();
        out.extend(page.items);
        let Some(next) = page.next else {
            break;
        };
        after = Some(next);
    }
    out
}

#[tokio::test]
async fn default_ingest_does_not_duplicate_ann_as_graph_edges() {
    let mem = build(Arc::new(SystemClock));
    let first = mem
        .ingest(Ingest::new("rust async tokio runtime", b"", &[], prov()))
        .await
        .unwrap();
    mem.ingest(Ingest::new("rust async tokio executor", b"", &[], prov()))
        .await
        .unwrap();

    assert!(mem.neighbors(first, 8).await.unwrap().is_empty());
}

#[tokio::test]
async fn failed_ingest_rolls_back_the_bounded_edge_plan_without_a_graph_scan() {
    let dim = DEFAULT_DIM;
    let store = Arc::new(MemStore::new(dim));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let bodies = Arc::new(ControlledBodyStore::default());
    let memory = Memory::new(
        counted.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(dim)),
        Arc::new(SystemClock),
        Config {
            default_body_scheme: "controlled",
            similarity_link_cap: 3,
            min_similarity_links: 3,
            similarity_link_threshold: 1.0,
            ..Config::default()
        },
    )
    .with_body_store(bodies.clone());

    for summary in ["alpha", "beta", "gamma", "delta"] {
        memory
            .ingest(Ingest::new(summary, summary.as_bytes(), &[], prov()))
            .await
            .unwrap();
    }
    let before = store.export();
    let before_nodes: HashSet<_> = before.nodes.iter().map(Node::id).collect();
    let before_vectors: HashSet<_> = before.vectors.iter().map(|(id, _)| *id).collect();
    let before_edges: HashMap<_, _> = before
        .edges
        .iter()
        .map(|edge| ((edge.from, edge.to), format!("{edge:?}")))
        .collect();
    let before_bodies = bodies.blobs.lock().unwrap().len();

    counted.inject_ambiguous_edge_failure(2);
    let error = memory
        .ingest(Ingest::new(
            "epsilon",
            b"body that must be compensated",
            &[],
            prov(),
        ))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("ambiguous edge write failure"),
        "{error}"
    );
    assert_eq!(
        counted.whole_edge_scans.load(Ordering::Relaxed),
        0,
        "ingest compensation must stay proportional to the planned link cap"
    );

    let after = store.export();
    assert_eq!(
        after.nodes.iter().map(Node::id).collect::<HashSet<_>>(),
        before_nodes
    );
    assert_eq!(
        after
            .vectors
            .iter()
            .map(|(id, _)| *id)
            .collect::<HashSet<_>>(),
        before_vectors
    );
    assert_eq!(
        after
            .edges
            .iter()
            .map(|edge| ((edge.from, edge.to), format!("{edge:?}")))
            .collect::<HashMap<_, _>>(),
        before_edges,
        "bounded compensation must not touch unrelated topology"
    );
    assert_eq!(bodies.blobs.lock().unwrap().len(), before_bodies);
    assert_eq!(bodies.delete_calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn ambiguous_vector_publication_is_compensated_without_scanning_edges() {
    let dim = DEFAULT_DIM;
    let store = Arc::new(MemStore::new(dim));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let vectors = Arc::new(AmbiguousVectorIndex {
        inner: store.clone(),
        fail_after_upsert: AtomicBool::new(true),
    });
    let bodies = Arc::new(ControlledBodyStore::default());
    let memory = Memory::new(
        counted.clone(),
        vectors,
        store.clone(),
        Arc::new(HashingEmbedder::new(dim)),
        Arc::new(SystemClock),
        Config {
            default_body_scheme: "controlled",
            ..Config::default()
        },
    )
    .with_body_store(bodies.clone());

    let error = memory
        .ingest(Ingest::new("vector failure", b"body", &[], prov()))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("ambiguous vector write failure"), "{error}");
    let after = store.export();
    assert!(after.nodes.is_empty());
    assert!(after.vectors.is_empty());
    assert!(after.edges.is_empty());
    assert!(bodies.blobs.lock().unwrap().is_empty());
    assert_eq!(bodies.delete_calls.load(Ordering::Relaxed), 1);
    assert_eq!(counted.whole_edge_scans.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn anchor_planning_failure_rolls_back_before_any_edge_write() {
    let dim = DEFAULT_DIM;
    let store = Arc::new(MemStore::new(dim));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let embedder = Arc::new(AnchorFailingEmbedder {
        inner: HashingEmbedder::new(dim),
        fail_multi_document: AtomicBool::new(false),
    });
    let bodies = Arc::new(ControlledBodyStore::default());
    let memory = Memory::new(
        counted.clone(),
        store.clone(),
        store.clone(),
        embedder.clone(),
        Arc::new(SystemClock),
        Config {
            default_body_scheme: "controlled",
            similarity_link_cap: 1,
            min_similarity_links: 1,
            ..Config::default()
        },
    )
    .with_body_store(bodies.clone());
    memory
        .ingest(Ingest::new("seed memory", b"seed body", &[], prov()))
        .await
        .unwrap();
    let before = store.export();
    let before_bodies = bodies.blobs.lock().unwrap().len();

    embedder.fail_multi_document.store(true, Ordering::Relaxed);
    let error = memory
        .ingest(Ingest::new(
            "related memory",
            b"first paragraph\n\nsecond paragraph",
            &[],
            prov(),
        ))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("anchor planning failure"), "{error}");
    let after = store.export();
    assert_eq!(after.nodes.len(), before.nodes.len());
    assert_eq!(after.vectors.len(), before.vectors.len());
    assert_eq!(after.edges.len(), before.edges.len());
    assert_eq!(bodies.blobs.lock().unwrap().len(), before_bodies);
    assert_eq!(bodies.delete_calls.load(Ordering::Relaxed), 1);
    assert_eq!(counted.edge_writes.load(Ordering::Relaxed), 0);
    assert_eq!(counted.whole_edge_scans.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn failed_ingest_reports_an_incomplete_bounded_rollback() {
    let dim = DEFAULT_DIM;
    let store = Arc::new(MemStore::new(dim));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let bodies = Arc::new(ControlledBodyStore::default());
    let memory = Memory::new(
        counted.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(dim)),
        Arc::new(SystemClock),
        Config {
            default_body_scheme: "controlled",
            similarity_link_cap: 1,
            min_similarity_links: 1,
            ..Config::default()
        },
    )
    .with_body_store(bodies.clone());
    memory
        .ingest(Ingest::new("seed", b"seed", &[], prov()))
        .await
        .unwrap();

    counted.inject_ambiguous_edge_failure(1);
    counted.fail_edge_deletes.store(true, Ordering::Relaxed);
    let error = memory
        .ingest(Ingest::new("failure", b"failure", &[], prov()))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("rollback incomplete"), "{error}");
    assert!(error.contains("injected edge rollback failure"), "{error}");
    assert_eq!(counted.whole_edge_scans.load(Ordering::Relaxed), 0);
    let residual = store.export();
    assert_eq!(
        residual.nodes.len(),
        2,
        "a possibly referenced node must survive incomplete edge cleanup"
    );
    assert_eq!(residual.vectors.len(), 2);
    assert_eq!(residual.edges.len(), 1);
    assert_eq!(bodies.blobs.lock().unwrap().len(), 2);
    assert_eq!(
        bodies.delete_calls.load(Ordering::Relaxed),
        0,
        "a body must not be erased while its canonical node may remain"
    );
}

#[tokio::test]
async fn similarity_floor_links_top_n_even_below_threshold() {
    // Floor of 2, but a threshold so high nothing clears it ⇒ links come purely
    // from the top-N floor, so a new node is never orphaned.
    let cfg = Config {
        similarity_link_cap: 2,
        min_similarity_links: 2,
        similarity_link_threshold: 0.99,
        ..Config::default()
    };
    let mem = build_with(Arc::new(SystemClock), cfg);
    mem.ingest(Ingest::new("alpha", b"", &[], prov()))
        .await
        .unwrap();
    mem.ingest(Ingest::new("beta", b"", &[], prov()))
        .await
        .unwrap();
    let last = mem
        .ingest(Ingest::new("gamma", b"", &[], prov()))
        .await
        .unwrap();
    // gamma links to its top-2 nearest despite nothing clearing 0.99.
    let n = mem.neighbors(last, 16).await.unwrap();
    assert_eq!(
        n.len(),
        2,
        "the floor wires the two closest even below threshold"
    );
}

#[tokio::test]
async fn legacy_flat_tagged_retrieval_fails_closed_without_erasing_coverage() {
    let mem = build(Arc::new(SystemClock));
    // Disjoint topics (no shared tokens ⇒ no similarity edge ⇒ no spread across).
    let _pit = mem
        .ingest(Ingest::new("deadlock hazard", b"", &["pitfall"], prov()))
        .await
        .unwrap();
    let _food = mem
        .ingest(Ingest::new("banana bread recipe", b"", &["food"], prov()))
        .await
        .unwrap();

    // Seeding the search from `pitfall` tags only — the adversarial mode.
    let error = mem
        .retrieve_seeded(
            "deadlock",
            5,
            mem.config().budget,
            StatusFilter::ACTIVE,
            &["pitfall"],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        Error::InvalidInput(message) if message.contains("flat compatibility API")
    ));
}

#[tokio::test]
async fn malformed_tagged_scope_is_rejected_before_embedding_work() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let embedder = Arc::new(CountingEmbedder {
        inner: HashingEmbedder::new(DEFAULT_DIM),
        calls: AtomicU64::new(0),
    });
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store,
        embedder.clone(),
        Arc::new(SystemClock),
        Config::default(),
    );

    let error = mem
        .retrieve_batch_seeded(
            "never embedded",
            5,
            mem.config().budget,
            StatusFilter::ACTIVE,
            &["duplicate", "duplicate"],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        Error::InvalidInput(message) if message.contains("must be unique")
    ));
    assert_eq!(embedder.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_tagged_generation_is_rejected_before_embedding_or_backend_work() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let vectors = Arc::new(BadTaggedGenerationVectorIndex {
        inner: store.clone(),
    });
    let embedder = Arc::new(CountingEmbedder {
        inner: HashingEmbedder::new(DEFAULT_DIM),
        calls: AtomicU64::new(0),
    });
    let mem = Memory::new(
        store.clone(),
        vectors,
        store,
        embedder.clone(),
        Arc::new(SystemClock),
        Config::default(),
    );

    let error = mem
        .retrieve_batch_seeded(
            "never embedded",
            5,
            mem.config().budget,
            StatusFilter::ACTIVE,
            &["valid-tag"],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        Error::InvalidInput(message) if message.contains("control")
    ));
    assert_eq!(embedder.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn typed_tagged_retrieval_preserves_lane_coverage_work_and_watermark() {
    let mem = build(Arc::new(SystemClock));
    let pitfall = mem
        .ingest(Ingest::new("deadlock hazard", b"", &["pitfall"], prov()))
        .await
        .unwrap();
    mem.ingest(Ingest::new("banana bread recipe", b"", &["food"], prov()))
        .await
        .unwrap();

    let batch = mem
        .retrieve_batch_seeded(
            "deadlock",
            5,
            mem.config().budget,
            StatusFilter::ACTIVE,
            &["pitfall"],
        )
        .await
        .unwrap();
    assert_eq!(batch.primary[0].node.id(), pitfall);

    let tagged = batch.tagged.as_ref().expect("typed tagged metadata");
    assert_eq!(
        tagged.primary_seed_coverage,
        TaggedSeedCoverage::ExactCosine
    );
    assert!(!tagged.is_partial());
    assert_eq!(tagged.work.raw_memberships, 1);
    assert_eq!(tagged.work.exact_hydrated_ids, 1);
    assert_eq!(batch.stamp.projection_watermarks.len(), 1);
    assert_eq!(
        batch.stamp.projection_watermarks.as_slice()[0].projection,
        "tag-membership"
    );
    assert_eq!(
        batch.stamp.projection_watermarks.as_slice()[0].watermark,
        "mneme-mem-semantic-tag-membership-v3"
    );
}

#[tokio::test]
async fn tagged_policy_fingerprint_uses_normalized_tag_order() {
    let mem = build(Arc::new(SystemClock));
    mem.ingest(Ingest::new("ordered tags", b"", &["a", "b"], prov()))
        .await
        .unwrap();
    let left = mem
        .retrieve_batch_seeded(
            "ordered tags",
            2,
            mem.config().budget,
            StatusFilter::ACTIVE,
            &["b", "a"],
        )
        .await
        .unwrap();
    let right = mem
        .retrieve_batch_seeded(
            "ordered tags",
            2,
            mem.config().budget,
            StatusFilter::ACTIVE,
            &["a", "b"],
        )
        .await
        .unwrap();
    assert_eq!(
        left.stamp.retrieval_policy.fingerprint,
        right.stamp.retrieval_policy.fingerprint
    );
}

#[tokio::test]
async fn tagged_seed_coverage_survives_untagged_graph_expansion() {
    let cfg = Config {
        ann_k: 1,
        lexical_k: 0,
        graph_seed_cap: 1,
        graph_slot_cap: 1,
        similarity_link_cap: 0,
        min_similarity_links: 0,
        budget: Budget {
            max_nodes: 2,
            max_depth: 1,
            min_relevance: 0.0,
            relevance_ratio: 0.0,
            explore: 0.0,
            dedup_similarity: 1.0,
            query_conditioning: 0.0,
        },
        ..Config::default()
    };
    let mem = build_with(Arc::new(SystemClock), cfg);
    let seed = mem
        .ingest(Ingest::new("tagged anchor", b"", &["scope"], prov()))
        .await
        .unwrap();
    let expansion = mem
        .ingest(Ingest::new("untagged graph neighbor", b"", &[], prov()))
        .await
        .unwrap();
    mem.link(seed, expansion, EdgeKind::Associative, 1.0, None)
        .await
        .unwrap();

    let batch = mem
        .retrieve_batch_seeded(
            "tagged anchor",
            1,
            cfg.budget,
            StatusFilter::ACTIVE,
            &["scope"],
        )
        .await
        .unwrap();
    assert_eq!(batch.primary[0].node.id(), seed);
    assert!(
        batch.primary.iter().any(|hit| hit.node.id() == expansion),
        "tagged mode constrains seeds, not graph-related final hits"
    );
    assert!(batch.primary[1].node.tags().all(|tag| tag != "scope"));
    let tagged = batch.tagged.expect("tagged coverage survives graph work");
    assert_eq!(
        tagged.primary_seed_coverage,
        TaggedSeedCoverage::ExactCosine
    );
    assert!(!tagged.is_partial());
}

#[tokio::test]
async fn tagged_k_one_admits_fresh_low_stability_node_without_probation() {
    let cfg = Config {
        graph_seed_cap: 0,
        budget: Budget {
            max_nodes: 1,
            max_depth: 0,
            ..Budget::default()
        },
        ..Config::default()
    };
    let mem = build_with(Arc::new(SystemClock), cfg);
    let fresh = mem
        .ingest(Ingest::new("fresh scoped fix", b"fix", &["scope"], prov()).with_stability(0.01))
        .await
        .unwrap();
    let batch = mem
        .retrieve_batch_seeded(
            "fresh scoped fix",
            1,
            cfg.budget,
            StatusFilter::ACTIVE,
            &["scope"],
        )
        .await
        .unwrap();
    assert_eq!(batch.primary[0].node.id(), fresh);
    assert_eq!(batch.primary[0].node.status(), NodeStatus::Active);
}

#[tokio::test]
async fn tagged_k_zero_and_256_are_valid_257_requires_a_budget_clamp() {
    let cfg = Config {
        graph_seed_cap: 0,
        ..Config::default()
    };
    let mem = build_with(Arc::new(SystemClock), cfg);
    let wide = Budget {
        max_nodes: 300,
        max_depth: 0,
        ..cfg.budget
    };

    let zero = mem
        .retrieve_batch_seeded("empty", 0, wide, StatusFilter::ACTIVE, &["scope"])
        .await
        .unwrap();
    assert!(zero.primary.is_empty());
    assert_eq!(
        zero.tagged.unwrap().primary_seed_coverage,
        TaggedSeedCoverage::ExactCosine
    );

    mem.retrieve_batch_seeded("empty", 256, wide, StatusFilter::ACTIVE, &["scope"])
        .await
        .unwrap();
    let over = mem
        .retrieve_batch_seeded("empty", 257, wide, StatusFilter::ACTIVE, &["scope"])
        .await
        .unwrap_err();
    assert!(matches!(
        over,
        Error::InvalidInput(message) if message.contains("maximum is 256")
    ));

    let clamped = Budget {
        max_nodes: 256,
        ..wide
    };
    mem.retrieve_batch_seeded("empty", 257, clamped, StatusFilter::ACTIVE, &["scope"])
        .await
        .unwrap();
}

#[tokio::test]
async fn popular_tag_partial_seed_coverage_survives_the_engine() {
    let cfg = Config {
        ann_k: 2,
        lexical_k: 0,
        graph_seed_cap: 0,
        graph_slot_cap: 0,
        similarity_link_cap: 0,
        min_similarity_links: 0,
        budget: Budget {
            max_nodes: 2,
            max_depth: 0,
            min_relevance: 0.0,
            relevance_ratio: 0.0,
            explore: 0.0,
            dedup_similarity: 1.0,
            query_conditioning: 0.0,
        },
        ..Config::default()
    };
    let mem = build_with(Arc::new(SystemClock), cfg);
    for index in 0..MAX_TAGGED_RAW_MEMBERSHIPS {
        let summary = format!("popular tagged node {index}");
        mem.ingest(Ingest::new(&summary, b"", &["popular"], prov()))
            .await
            .unwrap();
    }
    let _fresh = mem
        .ingest(Ingest::new(
            "popular tagged fresh target",
            b"",
            &["popular"],
            prov(),
        ))
        .await
        .unwrap();

    let batch = mem
        .retrieve_batch_seeded(
            "popular tagged fresh target",
            2,
            cfg.budget,
            StatusFilter::ACTIVE,
            &["popular"],
        )
        .await
        .unwrap();
    assert!(!batch.primary.is_empty());
    let tagged = batch.tagged.expect("tagged work metadata");
    assert_eq!(tagged.work.raw_memberships, MAX_TAGGED_RAW_MEMBERSHIPS + 1);
    assert!(tagged.primary_seed_coverage.is_partial());
    let fallback_quota = |coverage: &TaggedSeedCoverage| match coverage {
        TaggedSeedCoverage::ExactCosine => 0,
        TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter { physical, .. }
        | TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter { physical, .. } => physical
            .iter()
            .map(|item| item.total_quota().unwrap())
            .sum(),
    };
    assert_eq!(fallback_quota(&tagged.primary_seed_coverage), 192);
    assert!(tagged.is_partial());
}

#[tokio::test]
async fn engine_rejects_malformed_tagged_adapter_output_before_graph_expansion() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let vectors = Arc::new(MalformedTaggedVectorIndex {
        inner: store.clone(),
    });
    let mem = Memory::new(
        store.clone(),
        vectors,
        store,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(Arc::new(InlineStore::new()));
    mem.ingest(Ingest::new(
        "corrupt adapter target",
        b"",
        &["scope"],
        prov(),
    ))
    .await
    .unwrap();

    let error = mem
        .retrieve_batch_seeded(
            "target",
            5,
            mem.config().budget,
            StatusFilter::ACTIVE,
            &["scope"],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        Error::InvalidInput(message) if message.contains("must contain exactly one primary lane")
    ));
}

#[tokio::test]
async fn engine_rejects_structurally_valid_wrong_tag_seed_before_graph_expansion() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let vectors = Arc::new(WrongTagVectorIndex {
        inner: store.clone(),
        substitute: Mutex::new(None),
    });
    let mem = Memory::new(
        store.clone(),
        vectors.clone(),
        store,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    mem.ingest(Ingest::new("real tagged seed", b"", &["scope"], prov()))
        .await
        .unwrap();
    let wrong = mem
        .ingest(Ingest::new("wrong untagged root", b"", &[], prov()))
        .await
        .unwrap();
    *vectors
        .substitute
        .lock()
        .expect("wrong-tag substitute mutex poisoned") = Some(wrong);

    let batch = mem
        .retrieve_batch_seeded(
            "real tagged seed",
            1,
            mem.config().budget,
            StatusFilter::ACTIVE,
            &["scope"],
        )
        .await
        .unwrap();
    assert!(
        batch.primary.is_empty(),
        "a stale or lying wrong-tag seed is removed before it can root graph work"
    );
}

#[tokio::test]
async fn anchored_edge_resolves_the_passage() {
    let store = Arc::new(ControlledBodyStore::default());
    let mem = build_with_controlled_body(store.clone());
    let a = mem
        .ingest(Ingest::new("doc a", b"the quick brown fox", &[], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("doc b", b"y", &[], prov()))
        .await
        .unwrap();
    mem.link(a, b, EdgeKind::Associative, 0.5, Some(BodySpan::new(4, 9)))
        .await
        .unwrap();

    let edge = mem
        .neighbors(a, 16)
        .await
        .unwrap()
        .into_iter()
        .find(|n| n.node == b)
        .unwrap()
        .edge;
    assert_eq!(edge.anchor.map(|s| (s.start, s.end)), Some((4, 9)));
    let source = mem.get_node(a).await.unwrap().unwrap();
    let chunk = mem.resolve_body_range(&source, 10, 5).await.unwrap();
    assert_eq!(chunk.bytes, b"brown");
    assert_eq!((chunk.source_start, chunk.source_end), (10, 15));
    assert_eq!(chunk.next_offset, Some(15));
    // The span slices `a`'s body, not the edge's — bytes [4..9] = "quick".
    assert_eq!(mem.resolve_anchor(&edge).await.unwrap().unwrap(), b"quick");
    assert_eq!(
        mem.resolve_anchor_from(&edge, &source)
            .await
            .unwrap()
            .unwrap(),
        b"quick"
    );
    let wrong_source = mem.get_node(b).await.unwrap().unwrap();
    assert!(matches!(
        mem.resolve_anchor_from(&edge, &wrong_source).await,
        Err(Error::InvalidInput(message)) if message.contains("does not match edge source")
    ));
    assert_eq!(
        store.get_calls.load(Ordering::SeqCst),
        0,
        "range and anchor resolution must never fall back to full-body get"
    );
    assert_eq!(store.range_calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn core_is_explicit_and_negative_feedback_does_not_archive_any_node() {
    let mem = build(Arc::new(SystemClock));
    let cold = ColdPath::acquire();
    // Core remains explicit metadata, not a stability-derived reward.
    let core = mem
        .ingest(Ingest::new("user identity", b"x", &["core"], prov()).with_stability(0.0))
        .await
        .unwrap();
    let other = mem
        .ingest(Ingest::new("transient note", b"y", &["misc"], prov()))
        .await
        .unwrap();

    let set = mem.core(cold).await.unwrap();
    assert_eq!(set.len(), 1, "core() returns only core-tagged nodes");
    assert_eq!(set[0].id(), core);

    // Repeated irrelevance decays the edge, not either endpoint.
    mem.link(other, core, EdgeKind::Associative, 0.5, None)
        .await
        .unwrap();
    for _ in 0..30 {
        mem.apply_feedback(Some(other), core, Signal::Irrelevant)
            .await
            .unwrap();
    }
    mem.decay_sweep(cold).await.unwrap();
    mem.decay_sweep(cold).await.unwrap();
    assert!(
        mem.get_node(core).await.unwrap().unwrap().is_active(),
        "negative edge feedback must not archive core"
    );
    assert!(mem.get_node(other).await.unwrap().unwrap().is_active());
}

#[tokio::test]
async fn forget_drops_node_edges_and_vector() {
    let mem = build(Arc::new(SystemClock));
    let cold = ColdPath::acquire();
    let a = mem
        .ingest(Ingest::new("alpha node", b"x", &[], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("beta node", b"y", &[], prov()))
        .await
        .unwrap();
    mem.link(a, b, EdgeKind::Associative, 0.5, None)
        .await
        .unwrap();
    let forgotten_body = mem.get_node(a).await.unwrap().unwrap();
    assert_eq!(mem.resolve_body(&forgotten_body).await.unwrap(), b"x");

    assert!(
        mem.forget(cold, a).await.unwrap(),
        "forget reports it existed"
    );
    assert!(mem.get_node(a).await.unwrap().is_none(), "node is gone");
    assert!(
        mem.resolve_body(&forgotten_body).await.is_err(),
        "explicit forgetting erases the external payload too"
    );
    assert!(
        mem.neighbors(b, 16).await.unwrap().is_empty(),
        "the incident edge is gone"
    );
    assert!(
        mem.retrieve("alpha node")
            .await
            .unwrap()
            .iter()
            .all(|r| r.node.id() != a),
        "the vector is gone, so it's not retrievable"
    );
    assert!(
        !mem.forget(cold, a).await.unwrap(),
        "forgetting it again is a no-op"
    );
}

#[tokio::test]
async fn forget_keeps_the_body_reference_when_body_deletion_fails() {
    let store = Arc::new(ControlledBodyStore::default());
    let mem = build_with_controlled_body(store.clone());
    let id = mem
        .ingest(Ingest::new("private fact", b"secret", &[], prov()))
        .await
        .unwrap();
    let before = mem.get_node(id).await.unwrap().unwrap();

    store.fail_delete.store(true, Ordering::SeqCst);
    assert!(mem.forget(ColdPath::acquire(), id).await.is_err());

    let retained = mem
        .get_node(id)
        .await
        .unwrap()
        .expect("failed body deletion must retain the graph reference for retry");
    assert_eq!(retained.body(), before.body());
    assert_eq!(mem.resolve_body(&retained).await.unwrap(), b"secret");

    store.fail_delete.store(false, Ordering::SeqCst);
    assert!(mem.forget(ColdPath::acquire(), id).await.unwrap());
    assert!(mem.get_node(id).await.unwrap().is_none());
}

#[tokio::test]
async fn forget_never_deletes_an_explicit_borrowed_body() {
    let store = Arc::new(ControlledBodyStore::default());
    let external = store.put(b"caller owned").await.unwrap();
    let mem = build_with_controlled_body(store.clone());
    let id = mem
        .ingest(Ingest::new("borrowed", b"ignored", &[], prov()).with_body_ref(external.clone()))
        .await
        .unwrap();
    assert_eq!(
        mem.get_node(id).await.unwrap().unwrap().body_ownership(),
        BodyOwnership::Borrowed
    );

    assert!(mem.forget(ColdPath::acquire(), id).await.unwrap());
    assert_eq!(store.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.get(&external).await.unwrap(), b"caller owned");
}

#[tokio::test]
async fn managed_body_responsibility_transfers_until_the_last_local_reference() {
    let store = Arc::new(ControlledBodyStore::default());
    let mem = build_with_controlled_body(store.clone());
    let owner = mem
        .ingest(Ingest::new("owner", b"shared secret", &[], prov()))
        .await
        .unwrap();
    let owner_node = mem.get_node(owner).await.unwrap().unwrap();
    assert_eq!(owner_node.body_ownership(), BodyOwnership::Managed);
    let body = owner_node.body().clone();
    let sharer = mem
        .ingest(Ingest::new("sharer", b"ignored", &[], prov()).with_body_ref(body.clone()))
        .await
        .unwrap();

    // A shared managed body must not call delete, even when deletion would fail.
    store.fail_delete.store(true, Ordering::SeqCst);
    assert!(mem.forget(ColdPath::acquire(), owner).await.unwrap());
    assert_eq!(store.delete_calls.load(Ordering::SeqCst), 0);
    let successor = mem.get_node(sharer).await.unwrap().unwrap();
    assert_eq!(successor.body_ownership(), BodyOwnership::Managed);
    assert_eq!(
        mem.resolve_body(&successor).await.unwrap(),
        b"shared secret"
    );

    // The deterministic successor is now the final responsibility token.
    assert!(mem.forget(ColdPath::acquire(), sharer).await.is_err());
    assert!(mem.get_node(sharer).await.unwrap().is_some());
    store.fail_delete.store(false, Ordering::SeqCst);
    assert!(mem.forget(ColdPath::acquire(), sharer).await.unwrap());
    assert!(store.get(&body).await.is_err());
}

#[tokio::test]
async fn forgetting_a_borrowed_sharer_first_leaves_the_managed_owner_intact() {
    let store = Arc::new(ControlledBodyStore::default());
    let mem = build_with_controlled_body(store.clone());
    let owner = mem
        .ingest(Ingest::new("owner", b"shared secret", &[], prov()))
        .await
        .unwrap();
    let body = mem.get_node(owner).await.unwrap().unwrap().body().clone();
    let borrower = mem
        .ingest(Ingest::new("borrower", b"ignored", &[], prov()).with_body_ref(body.clone()))
        .await
        .unwrap();

    assert!(mem.forget(ColdPath::acquire(), borrower).await.unwrap());
    assert_eq!(store.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.get(&body).await.unwrap(), b"shared secret");
    assert!(mem.forget(ColdPath::acquire(), owner).await.unwrap());
    assert_eq!(store.delete_calls.load(Ordering::SeqCst), 1);
    assert!(store.get(&body).await.is_err());
}

#[tokio::test]
async fn forget_serializes_against_feedback_while_body_deletion_is_in_flight() {
    let store = Arc::new(ControlledBodyStore::default());
    let mem = Arc::new(build_with_controlled_body(store.clone()));
    let id = mem
        .ingest(Ingest::new("doomed", b"secret", &[], prov()))
        .await
        .unwrap();
    store.pause_delete.store(true, Ordering::SeqCst);

    let forgetting = {
        let mem = mem.clone();
        tokio::spawn(async move { mem.forget(ColdPath::acquire(), id).await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        store.delete_started.notified(),
    )
    .await
    .expect("forget reached the paused body deletion");

    let mut feedback = {
        let mem = mem.clone();
        tokio::spawn(async move { mem.apply_feedback(None, id, Signal::RelevantNew).await })
    };
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut feedback)
            .await
            .is_err(),
        "feedback must wait behind the in-flight forget mutation"
    );

    store.release_delete.notify_one();
    assert!(forgetting.await.unwrap().unwrap());
    feedback.await.unwrap().unwrap();
    assert!(mem.get_node(id).await.unwrap().is_none());
}

#[tokio::test]
async fn consolidate_bridges_across_clusters_only() {
    // High link threshold ⇒ ingest mints no similarity edges, so we control the
    // graph exactly; bridge_probability 1.0 makes the stochastic pass deterministic.
    let cfg = Config {
        bridge_probability: 1.0,
        similarity_link_threshold: 0.99,
        min_similarity_links: 0,
        ..Config::default()
    };
    // A fixed clock fixes the rng seed, so the representative pick is stable
    // across the two consolidate calls below (the second is then a true no-op).
    let mem = build_with(Arc::new(FakeClock::new(1_700_000_000_000)), cfg);

    // Two clusters, wired explicitly, with nothing across.
    let a = mem
        .ingest(Ingest::new("aaa", b"", &["g1"], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("bbb", b"", &["g1"], prov()))
        .await
        .unwrap();
    let c = mem
        .ingest(Ingest::new("ccc", b"", &["g2"], prov()))
        .await
        .unwrap();
    let d = mem
        .ingest(Ingest::new("ddd", b"", &["g2"], prov()))
        .await
        .unwrap();
    mem.link(a, b, EdgeKind::Associative, 0.9, None)
        .await
        .unwrap();
    mem.link(c, d, EdgeKind::Associative, 0.9, None)
        .await
        .unwrap();
    let cold = ColdPath::acquire();

    // No cross-cluster edge exists yet.
    assert!(
        mem.neighbors(a, 16)
            .await
            .unwrap()
            .iter()
            .all(|n| n.node != c && n.node != d),
        "g1 and g2 start disconnected"
    );

    let bridges = mem.consolidate(cold, &[a, b, c, d]).await.unwrap();
    assert_eq!(bridges.len(), 1, "one bridge per cross-cluster pair");
    let (x, y) = bridges[0];
    let (g1, g2) = ([a, b], [c, d]);
    assert!(
        (g1.contains(&x) && g2.contains(&y)) || (g2.contains(&x) && g1.contains(&y)),
        "the bridge spans the two clusters, never within one"
    );

    // Idempotent-ish: the bridge now exists, so a second pass won't duplicate it.
    let again = mem.consolidate(cold, &[a, b, c, d]).await.unwrap();
    assert!(again.is_empty(), "an existing bridge isn't re-minted");
}

#[tokio::test]
async fn communities_separate_disjoint_topics() {
    let mem = build_with_similarity_prior(Arc::new(SystemClock));

    // Two groups with strong intra-group token overlap and zero across.
    let a = mem
        .ingest(Ingest::new("alpha beta gamma delta", b"", &["g1"], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new(
            "alpha beta gamma epsilon",
            b"",
            &["g1"],
            prov(),
        ))
        .await
        .unwrap();
    let c = mem
        .ingest(Ingest::new("kappa lambda mu nu", b"", &["g2"], prov()))
        .await
        .unwrap();
    let d = mem
        .ingest(Ingest::new("kappa lambda mu xi", b"", &["g2"], prov()))
        .await
        .unwrap();

    let labels: HashMap<_, _> = mem
        .detect_communities(ColdPath::acquire())
        .await
        .unwrap()
        .into_iter()
        .collect();

    assert_eq!(labels[&a], labels[&b], "group 1 should share a cluster");
    assert_eq!(labels[&c], labels[&d], "group 2 should share a cluster");
    assert_ne!(labels[&a], labels[&c], "the two groups should differ");
}

/// Cross-db edges: link to a node in another db (by stamped db-id), read them
/// back weight-ordered, reject a dangling source, and clear them on forget.
#[tokio::test]
async fn remote_edges_link_read_and_forget() {
    use ulid::Ulid;
    let mem = build(Arc::new(SystemClock));
    let a = mem
        .ingest(Ingest::new("source node", b"s", &[], prov()))
        .await
        .unwrap();

    let project_db = Ulid::new();
    let (lo, hi) = (NodeId(Ulid::new()), NodeId(Ulid::new()));
    mem.link_remote(a, project_db, hi, 0.8).await.unwrap();
    mem.link_remote(a, project_db, lo, 0.3).await.unwrap();
    mem.link_remote(a, project_db, a, 0.2).await.unwrap();

    let edges = mem.remote_edges_page(a, None, 64).await.unwrap().items;
    assert_eq!(edges.len(), 3);
    assert_eq!(edges[0].target, hi, "weight-desc: 0.8 first");
    assert_eq!(edges[0].target_db, project_db);
    assert!((edges[0].weight() - 0.8).abs() < 1e-6);
    assert!(
        edges
            .iter()
            .any(|edge| edge.target_db == project_db && edge.target == a),
        "equal node ids remain valid when they name nodes in different databases"
    );

    // Re-linking upserts (replaces the weight, doesn't duplicate).
    mem.link_remote(a, project_db, hi, 0.5).await.unwrap();
    assert_eq!(
        mem.remote_edges_page(a, None, 64)
            .await
            .unwrap()
            .items
            .len(),
        3
    );

    // A dangling source is rejected.
    assert!(
        mem.link_remote(NodeId(Ulid::new()), project_db, hi, 0.5)
            .await
            .is_err()
    );
    assert!(matches!(
        mem.link_remote(a, project_db, hi, f32::NAN).await,
        Err(Error::InvalidInput(_))
    ));

    // forget drops the node's remote edges too.
    mem.forget(ColdPath::acquire(), a).await.unwrap();
    assert!(
        mem.remote_edges_page(a, None, 64)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn forget_deletes_every_remote_edge_page() {
    use ulid::Ulid;
    let mem = build(Arc::new(SystemClock));
    let source = mem
        .ingest(Ingest::new("many remote edges", b"s", &[], prov()))
        .await
        .unwrap();
    let target_db = Ulid::new();
    for offset in 0..130u128 {
        mem.link_remote(
            source,
            target_db,
            NodeId(Ulid::from(500_000u128 + offset)),
            (offset % 10) as f32 / 10.0,
        )
        .await
        .unwrap();
    }

    assert!(mem.forget(ColdPath::acquire(), source).await.unwrap());
    assert!(
        mem.remote_edges_page(source, None, 64)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

/// Retrieval is read-only even when co-retrieval edge learning is configured.
#[tokio::test]
async fn query_and_recall_are_pure_even_when_coretrieval_is_configured() {
    let cfg = Config {
        ann_k: 2,
        lexical_k: 0,
        similarity_link_cap: 0,
        min_similarity_links: 0,
        similarity_link_threshold: 0.0,
        // This legacy experiment used to mutate topology inside retrieval. A
        // nonzero value now proves the query path fails closed until a host can
        // commit the exact cards it actually emitted.
        coretrieval_link_cap: 2,
        graph_seed_cap: 0,
        budget: Budget {
            max_nodes: 2,
            max_depth: 0,
            min_relevance: 0.0,
            relevance_ratio: 0.0,
            dedup_similarity: 1.0,
            ..Budget::default()
        },
        ..Config::default()
    };
    let (mem, store) = build_with_store(Arc::new(SystemClock), cfg);
    let a = mem
        .ingest(Ingest::new("shared alpha", b"a", &[], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("shared beta", b"b", &[], prov()))
        .await
        .unwrap();

    let before_edges = store.all_edges(ColdPath::acquire()).await.unwrap();
    assert!(before_edges.is_empty(), "fixture starts without topology");

    let query = mem.retrieve("shared alpha beta").await.unwrap();
    assert!(query.iter().any(|hit| hit.node.id() == a));
    assert!(query.iter().any(|hit| hit.node.id() == b));
    let recall = mem
        .recall_expanded("shared alpha beta", 2, 2)
        .await
        .unwrap();
    assert!(recall.iter().any(|hit| hit.node.id() == a));
    assert!(recall.iter().any(|hit| hit.node.id() == b));

    assert!(
        store
            .all_edges(ColdPath::acquire())
            .await
            .unwrap()
            .is_empty(),
        "retrieval planning must not learn co-retrieval topology"
    );
    for id in [a, b] {
        let node = mem.get_node(id).await.unwrap().unwrap();
        assert_eq!(node.exposure_count(), 0);
        assert_eq!(node.last_exposed(), None);
        assert_eq!(node.grounded_use_count(), 0);
    }
}

/// Ingest anchors each similarity edge to the body chunk that drove the match:
/// the paragraph sharing vocabulary with the neighbor, not the unrelated one.
#[tokio::test]
async fn ingest_anchors_similarity_edge_to_the_matching_chunk() {
    // Floor at 1 so the new node links to its top neighbor regardless of threshold.
    let mem = build_with(
        Arc::new(SystemClock),
        Config {
            similarity_link_cap: 1,
            min_similarity_links: 1,
            ..Config::default()
        },
    );
    let b = mem
        .ingest(Ingest::new("rust async tokio runtime", b"", &[], prov()))
        .await
        .unwrap();
    let body: &[u8] =
        b"completely unrelated text about gardening tomatoes\n\nrust async tokio runtime reactor executor scheduler";
    let a = mem
        .ingest(Ingest::new("rust async tokio executor", body, &[], prov()))
        .await
        .unwrap();

    let nbr = mem
        .neighbors(a, 8)
        .await
        .unwrap()
        .into_iter()
        .find(|n| n.node == b)
        .expect("a -> b similarity edge");
    let span = nbr.edge.anchor.expect("edge is anchored to a body chunk");
    let anchored = std::str::from_utf8(span.slice(body)).unwrap();
    assert!(
        anchored.contains("reactor"),
        "anchored on the matching chunk; got {anchored:?}"
    );
    assert!(!anchored.contains("gardening"), "not the unrelated chunk");
}

/// ε-exploration: an under-weighted edge that the spread would prune is followed
/// occasionally, so recall surfaces a low-weight/long-range association it would
/// otherwise never reach.
#[tokio::test]
async fn exploration_surfaces_underweighted_edges() {
    let mem = build_with(
        Arc::new(SystemClock),
        Config {
            min_similarity_links: 0,
            ..Config::default()
        },
    );
    let a = mem
        .ingest(Ingest::new("alpha unique zzz", b"", &[], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("beta", b"", &[], prov()))
        .await
        .unwrap();
    let c = mem
        .ingest(Ingest::new("gamma", b"", &[], prov()))
        .await
        .unwrap();
    mem.link(a, b, EdgeKind::Associative, 0.9, None)
        .await
        .unwrap(); // strong
    mem.link(a, c, EdgeKind::Associative, 0.01, None)
        .await
        .unwrap(); // weak/long-range

    let q = "alpha unique zzz";
    let off = Budget {
        explore: 0.0,
        ..Budget::default()
    };
    let on = Budget {
        explore: 1.0,
        ..Budget::default()
    };

    let no = mem
        .retrieve_seeded(q, 1, off, StatusFilter::ACTIVE, &[])
        .await
        .unwrap();
    assert!(
        no.iter().any(|h| h.node.id() == b),
        "the strong edge always surfaces"
    );
    assert!(
        !no.iter().any(|h| h.node.id() == c),
        "the weak edge is pruned without exploration"
    );

    let yes = mem
        .retrieve_seeded(q, 1, on, StatusFilter::ACTIVE, &[])
        .await
        .unwrap();
    assert!(
        yes.iter().any(|h| h.node.id() == c),
        "exploration surfaces the weak-edge node"
    );
}

/// Query-conditioning pulls the spread toward the query: two neighbours reached by
/// equally strong edges tie without it, but the off-query one is damped below the
/// on-query one once conditioning is on — a per-query attention mask over the hub.
#[tokio::test]
async fn query_conditioning_damps_off_query_neighbors() {
    let mem = build(Arc::new(SystemClock));
    // Seed matches the query; both neighbours hang off an equally strong edge, but
    // only one shares the query's vocabulary (the lexical embedder scores the other
    // near zero, so it lands outside the query's relevance neighbourhood).
    let seed = mem
        .ingest(Ingest::new("alpha beta gamma", b"", &[], prov()))
        .await
        .unwrap();
    let on_query = mem
        .ingest(Ingest::new("alpha beta delta", b"", &[], prov()))
        .await
        .unwrap();
    let off_query = mem
        .ingest(Ingest::new("xylophone quartz widget", b"", &[], prov()))
        .await
        .unwrap();
    mem.link(seed, on_query, EdgeKind::Associative, 0.9, None)
        .await
        .unwrap();
    mem.link(seed, off_query, EdgeKind::Associative, 0.9, None)
        .await
        .unwrap();

    let q = "alpha beta gamma";
    let off = Budget {
        query_conditioning: 0.0,
        ..Budget::default()
    };
    let on = Budget {
        query_conditioning: 0.8,
        ..Budget::default()
    };

    // Unconditioned: both neighbours surface (equal edges ⇒ equal activation).
    let plain = mem
        .retrieve_seeded(q, 1, off, StatusFilter::ACTIVE, &[])
        .await
        .unwrap();
    let plain_on = plain
        .iter()
        .find(|h| h.node.id() == on_query)
        .map(|h| h.score);
    let plain_off = plain
        .iter()
        .find(|h| h.node.id() == off_query)
        .map(|h| h.score);
    assert!(
        plain_on.is_some() && plain_off.is_some(),
        "both neighbours surface without conditioning"
    );

    // Conditioned: the off-query neighbour is damped below the on-query one (or
    // dropped under the floor entirely — even stronger evidence).
    let cond = mem
        .retrieve_seeded(q, 1, on, StatusFilter::ACTIVE, &[])
        .await
        .unwrap();
    let cond_on = cond
        .iter()
        .find(|h| h.node.id() == on_query)
        .map(|h| h.score)
        .expect("on-query neighbour still surfaces under conditioning");
    // Some ⇒ damped below the on-query node; None ⇒ damped under the floor entirely.
    if let Some(off_s) = cond
        .iter()
        .find(|h| h.node.id() == off_query)
        .map(|h| h.score)
    {
        assert!(
            cond_on > off_s,
            "on-query ({cond_on}) should outrank off-query ({off_s}) under conditioning"
        );
    }
}

/// A trivial reranker for the test: lifts any document containing "xenon".
struct KeywordReranker;
#[async_trait::async_trait]
impl mneme_core::ports::Reranker for KeywordReranker {
    fn semantic_id(&self) -> &'static str {
        "test-keyword-reranker-v1"
    }

    async fn rerank(&self, _query: &str, docs: &[&str]) -> mneme_core::ports::Result<Vec<f32>> {
        Ok(docs
            .iter()
            .map(|d| if d.contains("xenon") { 5.0 } else { 0.0 })
            .collect())
    }
}

struct NonFiniteReranker;

#[async_trait::async_trait]
impl Reranker for NonFiniteReranker {
    fn semantic_id(&self) -> &'static str {
        "test-non-finite-reranker-v1"
    }

    async fn rerank(&self, _query: &str, docs: &[&str]) -> PortResult<Vec<f32>> {
        Ok(vec![f32::NAN; docs.len()])
    }
}

#[derive(Default)]
struct PausingReranker {
    started: Notify,
    release: Notify,
}

#[async_trait]
impl Reranker for PausingReranker {
    fn semantic_id(&self) -> &'static str {
        "test-pausing-reranker-v1"
    }

    async fn rerank(&self, _query: &str, docs: &[&str]) -> PortResult<Vec<f32>> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(vec![0.0; docs.len()])
    }
}

#[tokio::test]
async fn active_retrieval_revalidates_lifecycle_after_reranking() {
    let dim = DEFAULT_DIM;
    let store = Arc::new(MemStore::new(dim));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let reranker = Arc::new(PausingReranker::default());
    let cfg = Config {
        min_similarity_links: 0,
        graph_seed_cap: 0,
        budget: Budget {
            max_depth: 0,
            min_relevance: 0.0,
            relevance_ratio: 0.0,
            dedup_similarity: 1.0,
            ..Budget::default()
        },
        ..Config::default()
    };
    let mem = Arc::new(
        Memory::new(
            counted.clone(),
            store.clone(),
            store,
            Arc::new(HashingEmbedder::new(dim)),
            Arc::new(SystemClock),
            cfg,
        )
        .with_body_store(Arc::new(InlineStore::new()))
        .with_reranker(reranker.clone()),
    );
    let doomed = mem
        .ingest(Ingest::new("doomed exact token", b"", &[], prov()))
        .await
        .unwrap();
    let replacement = mem
        .ingest(Ingest::new("replacement", b"", &[], prov()))
        .await
        .unwrap();
    counted.reset_reads();

    let retrieving = {
        let mem = mem.clone();
        tokio::spawn(async move { mem.retrieve("doomed exact token").await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        reranker.started.notified(),
    )
    .await
    .expect("retrieval reached the paused reranker");
    mem.supersede(ColdPath::acquire(), replacement, doomed)
        .await
        .unwrap();
    reranker.release.notify_one();

    let hits = retrieving.await.unwrap().unwrap();
    assert!(hits.iter().all(|hit| hit.node.id() != doomed));
    assert_eq!(
        counted.batch_reads.load(Ordering::Relaxed),
        2,
        "the pre-rank hydration and post-rank lifecycle snapshot are each one batch"
    );
    let current = mem.get_node(doomed).await.unwrap().unwrap();
    assert!(current.is_archived());
    assert_eq!(current.exposure_count(), 0);
    assert_eq!(current.grounded_use_count(), 0);
}

#[tokio::test]
async fn co_retrieval_never_recreates_an_edge_to_a_concurrently_forgotten_node() {
    let dim = DEFAULT_DIM;
    let store = Arc::new(MemStore::new(dim));
    let reranker = Arc::new(PausingReranker::default());
    let cfg = Config {
        min_similarity_links: 0,
        similarity_link_threshold: 0.0,
        coretrieval_link_cap: 2,
        graph_seed_cap: 0,
        budget: Budget {
            max_depth: 0,
            min_relevance: 0.0,
            relevance_ratio: 0.0,
            dedup_similarity: 1.0,
            ..Budget::default()
        },
        ..Config::default()
    };
    let mem = Arc::new(
        Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(HashingEmbedder::new(dim)),
            Arc::new(SystemClock),
            cfg,
        )
        .with_body_store(Arc::new(InlineStore::new()))
        .with_reranker(reranker.clone()),
    );
    let a = mem
        .ingest(Ingest::new("shared alpha", b"a", &[], prov()))
        .await
        .unwrap();
    let forgotten = mem
        .ingest(Ingest::new("shared beta", b"b", &[], prov()))
        .await
        .unwrap();

    let retrieving = {
        let mem = mem.clone();
        tokio::spawn(async move { mem.retrieve("shared alpha beta").await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        reranker.started.notified(),
    )
    .await
    .expect("retrieval reached the paused reranker");
    assert!(mem.forget(ColdPath::acquire(), forgotten).await.unwrap());
    reranker.release.notify_one();

    let hits = retrieving.await.unwrap().unwrap();
    assert!(hits.iter().all(|hit| hit.node.id() != forgotten));
    assert!(hits.iter().any(|hit| hit.node.id() == a));
    for edge in store.all_edges(ColdPath::acquire()).await.unwrap() {
        assert_ne!(edge.from, forgotten);
        assert_ne!(edge.to, forgotten);
        assert!(store.get_node(edge.from).await.unwrap().is_some());
        assert!(store.get_node(edge.to).await.unwrap().is_some());
    }
}

/// The rerank stage re-scores the bounded candidate set jointly against the query,
/// lifting a lower-similarity node the bi-encoder ranked below a closer one.
#[tokio::test]
async fn reranker_reorders_the_candidate_set() {
    let dim = DEFAULT_DIM;
    let store = Arc::new(MemStore::new(dim));
    let g: Arc<dyn GraphStore> = store.clone();
    let v: Arc<dyn VectorIndex> = store.clone();
    let t: Arc<dyn Traversal> = store.clone();
    let e = Arc::new(HashingEmbedder::new(dim));
    let clk: Arc<dyn Clock> = Arc::new(SystemClock);
    // No adaptive cutoff / dedup / conditioning, so both candidates reach the rerank.
    let cfg = Config {
        min_similarity_links: 0,
        budget: Budget {
            explore: 0.0,
            ..Budget::default()
        },
        ..Config::default()
    };
    let plain = Memory::new(g.clone(), v.clone(), t.clone(), e.clone(), clk.clone(), cfg)
        .with_body_store(Arc::new(InlineStore::new()));

    // A: higher lexical overlap with the query; B: lower overlap but the rerank target.
    let _a = plain
        .ingest(Ingest::new("apple banana cherry", b"", &[], prov()))
        .await
        .unwrap();
    let b = plain
        .ingest(Ingest::new("apple xenon", b"", &[], prov()))
        .await
        .unwrap();

    let q = "apple banana";
    // Bi-encoder alone ranks the closer A first.
    let no = plain.retrieve(q).await.unwrap();
    assert!(
        no[0].node.summary().contains("banana"),
        "embedding ranks A first, got {:?}",
        no[0].node.summary()
    );

    // The reranker lifts B (the target) to the top, with a sigmoid'd score.
    let rr = Memory::new(g, v, t, e, clk, cfg)
        .with_body_store(Arc::new(InlineStore::new()))
        .with_reranker(Arc::new(KeywordReranker));
    let yes = rr.retrieve(q).await.unwrap();
    assert_eq!(yes[0].node.id(), b, "reranker lifts the target to the top");
    assert!(
        yes[0].score > 0.9,
        "top score is the sigmoid of the high rerank logit"
    );
}

#[tokio::test]
async fn non_finite_reranker_output_fails_closed() {
    let dim = DEFAULT_DIM;
    let store = Arc::new(MemStore::new(dim));
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(dim)),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(Arc::new(InlineStore::new()))
    .with_reranker(Arc::new(NonFiniteReranker));
    mem.ingest(Ingest::new("finite query", b"", &[], prov()))
        .await
        .unwrap();

    let error = mem.retrieve_batch("finite query").await.unwrap_err();
    assert!(
        error.to_string().contains("non-finite relevance score"),
        "unexpected error: {error}"
    );
}

/// Density GC deletes never-validated sub-floor edges and physically caps an
/// over-connected node by dropping its weakest associations.
#[tokio::test]
async fn prune_dense_drops_speculative_edges_and_caps_hubs() {
    let cfg = Config {
        min_similarity_links: 0,
        dense_degree_threshold: 2,
        prune_weight_floor: 0.2,
        ..Config::default()
    };
    let mem = build_with(Arc::new(SystemClock), cfg);
    let cold = ColdPath::acquire();

    let id = |sum: &str| {
        let mem = &mem;
        let sum = sum.to_string();
        async move {
            mem.ingest(Ingest::new(&sum, b"", &[], prov()))
                .await
                .unwrap()
        }
    };
    let h = id("hub").await;
    let a = id("alpha").await;
    let b = id("bravo").await;
    let c = id("charlie").await;
    let d = id("delta").await;
    let s = id("sierra").await;
    let t = id("tango").await;

    // Hub with four out-edges (all above the prune floor, so all kept) — over the
    // degree threshold of 2.
    for (to, w) in [(a, 0.9), (b, 0.8), (c, 0.3), (d, 0.25)] {
        mem.link(h, to, EdgeKind::Associative, w, None)
            .await
            .unwrap();
    }
    // A never-validated speculative edge: zero trials, below the floor.
    mem.link(s, t, EdgeKind::Associative, 0.1, None)
        .await
        .unwrap();

    let report = mem.prune_dense(cold).await.unwrap();
    assert_eq!(report.pruned, 3, "speculative + two excess edges deleted");
    assert_eq!(
        report.capacity_pruned, 2,
        "the hub's two weakest edges are physically collected"
    );

    assert!(
        !mem.neighbors(s, 64)
            .await
            .unwrap()
            .iter()
            .any(|n| n.node == t),
        "the speculative S→T edge was pruned"
    );
    // Hub: only the strongest two remain.
    let nbrs = mem.neighbors(h, 64).await.unwrap();
    let targets: HashSet<NodeId> = nbrs.iter().map(|n| n.node).collect();
    assert_eq!(targets, HashSet::from([a, b]));
}

/// Reconciliation triage: open contradictions are tagged with their nodes'
/// current communities, and cross-pair conflicts aggregate to the community level.
#[tokio::test]
async fn reconciliation_triage_aggregates_conflicts_by_community() {
    let mem = build_with_similarity_prior(Arc::new(SystemClock));
    let cold = ColdPath::acquire();
    // Two clusters by shared vocabulary (the similarity prior links within each).
    let a = mem
        .ingest(Ingest::new("alpha beta gamma", b"", &["g1"], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("alpha beta delta", b"", &["g1"], prov()))
        .await
        .unwrap();
    let c = mem
        .ingest(Ingest::new("kappa lambda mu", b"", &["g2"], prov()))
        .await
        .unwrap();
    let d = mem
        .ingest(Ingest::new("kappa lambda nu", b"", &["g2"], prov()))
        .await
        .unwrap();

    mem.observe_contradiction(cold, a, c).await.unwrap();
    mem.observe_contradiction(cold, a, c).await.unwrap(); // pair (a,c): 2 observations
    mem.observe_contradiction(cold, b, d).await.unwrap(); // pair (b,d): 1

    let triage = mem.reconciliation_triage(cold).await.unwrap();
    assert_eq!(triage.contradictions.len(), 2);
    assert_eq!(
        triage.contradictions[0].observations, 2,
        "observation-count desc"
    );
    assert_ne!(
        triage.contradictions[0].clusters.0, triage.contradictions[0].clusters.1,
        "a and c are in different communities"
    );
    // (a,c) and (b,d) span the SAME community pair, so they collapse to one
    // cluster-level conflict with the summed count.
    assert_eq!(triage.cluster_conflicts.len(), 1);
    assert_eq!(triage.cluster_conflicts[0].observations, 3);
    assert_eq!(triage.cluster_conflicts[0].pairs, 2);
}

/// The adaptive relevance floor trims the marginal tail: a weak hit that clears
/// the absolute floor is dropped when it falls below `top · relevance_ratio`.
#[tokio::test]
async fn relevance_ratio_trims_the_marginal_tail() {
    let mem = build(Arc::new(SystemClock));
    let a = mem
        .ingest(Ingest::new("alpha beta gamma", b"", &[], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("alpha distinct unrelated", b"", &[], prov()))
        .await
        .unwrap();
    let q = "alpha beta gamma"; // exact match to A (score ~1.0); B shares only "alpha" (~0.33)

    let loose = Budget {
        max_depth: 0,
        relevance_ratio: 0.0,
        ..Budget::default()
    };
    let tight = Budget {
        max_depth: 0,
        relevance_ratio: 0.5,
        ..Budget::default()
    };
    let loose_hits = mem
        .retrieve_seeded(q, 5, loose, StatusFilter::ACTIVE, &[])
        .await
        .unwrap();
    let tight_hits = mem
        .retrieve_seeded(q, 5, tight, StatusFilter::ACTIVE, &[])
        .await
        .unwrap();

    assert!(
        loose_hits.iter().any(|h| h.node.id() == b),
        "marginal hit kept with ratio 0"
    );
    assert!(
        tight_hits.iter().any(|h| h.node.id() == a),
        "the strong hit always stays"
    );
    assert!(
        !tight_hits.iter().any(|h| h.node.id() == b),
        "marginal hit trimmed at ratio 0.5"
    );
}

/// Near-duplicate filtering: two identical-summary nodes both surface when off,
/// but collapse to one when dedup is on.
#[tokio::test]
async fn near_duplicates_are_filtered() {
    let mem = build(Arc::new(SystemClock));
    let a1 = mem
        .ingest(Ingest::new("alpha beta gamma delta", b"", &[], prov()))
        .await
        .unwrap();
    let a2 = mem
        .ingest(Ingest::new("alpha beta gamma delta", b"", &[], prov()))
        .await
        .unwrap();
    let q = "alpha beta gamma delta";

    let off = Budget {
        dedup_similarity: 1.0,
        ..Budget::default()
    }; // disabled
    let on = Budget {
        dedup_similarity: 0.9,
        ..Budget::default()
    };
    let off_hits = mem
        .retrieve_seeded(q, 10, off, StatusFilter::ACTIVE, &[])
        .await
        .unwrap();
    let on_hits = mem
        .retrieve_seeded(q, 10, on, StatusFilter::ACTIVE, &[])
        .await
        .unwrap();

    let copies = |hits: &[mneme_engine::Retrieved]| {
        hits.iter()
            .filter(|h| h.node.id() == a1 || h.node.id() == a2)
            .count()
    };
    assert_eq!(
        copies(&off_hits),
        2,
        "both paraphrases kept when dedup is off"
    );
    assert_eq!(
        copies(&on_hits),
        1,
        "near-duplicate collapsed when dedup is on"
    );
}

/// status() surfaces what maintenance is due: node counts, open contradictions,
/// and banked interference a decay sweep would act on.
#[tokio::test]
async fn status_reports_pending_maintenance() {
    let mem = build(Arc::new(SystemClock));
    let cold = ColdPath::acquire();
    let a = mem
        .ingest(Ingest::new("alpha", b"", &[], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("beta", b"", &[], prov()))
        .await
        .unwrap();
    let _c = mem
        .ingest(Ingest::new("gamma", b"", &[], prov()))
        .await
        .unwrap(); // fresh searchable node
    mem.link(a, b, EdgeKind::Associative, 0.5, None)
        .await
        .unwrap();

    let s0 = mem.status(cold).await.unwrap();
    assert_eq!((s0.nodes, s0.active), (3, 3));
    assert_eq!(s0.open_contradictions, 0);
    assert_eq!(
        s0.edge_decay_pending, 0,
        "nothing banked yet ⇒ a decay sweep would no-op"
    );

    mem.observe_contradiction(cold, a, b).await.unwrap();
    mem.apply_feedback(Some(a), b, Signal::Irrelevant)
        .await
        .unwrap(); // banks interference only on the edge
    let s1 = mem.status(cold).await.unwrap();
    assert_eq!(
        s1.open_contradictions, 1,
        "a contradiction now needs reconciling"
    );
    assert!(
        s1.edge_decay_pending >= 1,
        "irrelevance banked interference for the next sweep"
    );
}

#[tokio::test]
async fn consolidation_mints_bridge_kind_not_associative() {
    // Same controlled two-cluster setup as `consolidate_bridges_across_clusters_only`,
    // asserting the *kind* of the minted edge: long-range links are `Bridge`.
    let cfg = Config {
        bridge_probability: 1.0,
        similarity_link_threshold: 0.99,
        min_similarity_links: 0,
        ..Config::default()
    };
    let mem = build_with(Arc::new(FakeClock::new(1_700_000_000_000)), cfg);

    let a = mem
        .ingest(Ingest::new("aaa", b"", &["g1"], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("bbb", b"", &["g1"], prov()))
        .await
        .unwrap();
    let c = mem
        .ingest(Ingest::new("ccc", b"", &["g2"], prov()))
        .await
        .unwrap();
    let d = mem
        .ingest(Ingest::new("ddd", b"", &["g2"], prov()))
        .await
        .unwrap();
    mem.link(a, b, EdgeKind::Associative, 0.9, None)
        .await
        .unwrap();
    mem.link(c, d, EdgeKind::Associative, 0.9, None)
        .await
        .unwrap();

    let bridges = mem
        .consolidate(ColdPath::acquire(), &[a, b, c, d])
        .await
        .unwrap();
    assert_eq!(bridges.len(), 1);
    let (x, y) = bridges[0];
    assert_eq!(
        edge_kind(&mem, x, y).await,
        Some(EdgeKind::Bridge),
        "a consolidation bridge is minted as EdgeKind::Bridge"
    );
}

#[tokio::test]
async fn graduate_bridges_promotes_only_intra_cluster_bridges() {
    // Control the graph exactly (no similarity links). g1 = {a, b, e} via strong
    // associative edges; g2 = {c, d}. Then two bridges: b-e *within* g1, and a-c
    // *across* g1/g2. Graduation should reclassify the intra-cluster one only.
    let cfg = Config {
        similarity_link_threshold: 0.99,
        min_similarity_links: 0,
        ..Config::default()
    };
    let mem = build_with(Arc::new(SystemClock), cfg);

    let a = mem
        .ingest(Ingest::new("aaa", b"", &["g1"], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("bbb", b"", &["g1"], prov()))
        .await
        .unwrap();
    let e = mem
        .ingest(Ingest::new("eee", b"", &["g1"], prov()))
        .await
        .unwrap();
    let c = mem
        .ingest(Ingest::new("ccc", b"", &["g2"], prov()))
        .await
        .unwrap();
    let d = mem
        .ingest(Ingest::new("ddd", b"", &["g2"], prov()))
        .await
        .unwrap();
    // Strong intra-cluster structure (a is the g1 hub).
    mem.link(a, b, EdgeKind::Associative, 0.9, None)
        .await
        .unwrap();
    mem.link(a, e, EdgeKind::Associative, 0.9, None)
        .await
        .unwrap();
    mem.link(c, d, EdgeKind::Associative, 0.9, None)
        .await
        .unwrap();
    // Two weak bridges: one already inside g1, one genuinely spanning g1↔g2.
    mem.link(b, e, EdgeKind::Bridge, 0.15, None).await.unwrap();
    mem.link(a, c, EdgeKind::Bridge, 0.15, None).await.unwrap();

    let converted = mem.graduate_bridges(ColdPath::acquire()).await.unwrap();
    assert_eq!(converted, 1, "only the intra-cluster bridge graduates");
    assert_eq!(
        edge_kind(&mem, b, e).await,
        Some(EdgeKind::Associative),
        "an intra-cluster bridge becomes an ordinary association"
    );
    assert_eq!(
        edge_kind(&mem, a, c).await,
        Some(EdgeKind::Bridge),
        "a still-cross-cluster bridge is left alone"
    );
}

#[tokio::test]
async fn bridge_edges_reinforce_slower_than_associative() {
    // Two edges minted at the same weight but different kinds; one RelevantNew
    // hit each. The bridge climbs less (gentler reinforce_gain), confirming the
    // params are selected by edge kind.
    let mem = build(Arc::new(SystemClock));
    let a = mem
        .ingest(Ingest::new("aaa", b"", &[], prov()))
        .await
        .unwrap();
    let b = mem
        .ingest(Ingest::new("bbb", b"", &[], prov()))
        .await
        .unwrap();
    let c = mem
        .ingest(Ingest::new("ccc", b"", &[], prov()))
        .await
        .unwrap();
    let d = mem
        .ingest(Ingest::new("ddd", b"", &[], prov()))
        .await
        .unwrap();

    mem.link(a, b, EdgeKind::Bridge, 0.15, None).await.unwrap();
    mem.link(c, d, EdgeKind::Associative, 0.15, None)
        .await
        .unwrap();
    mem.apply_feedback(Some(a), b, Signal::RelevantNew)
        .await
        .unwrap();
    mem.apply_feedback(Some(c), d, Signal::RelevantNew)
        .await
        .unwrap();

    let bridge_w = edge_weight(&mem, a, b).await;
    let assoc_w = edge_weight(&mem, c, d).await;
    assert!(
        bridge_w < assoc_w,
        "bridge reinforces slower: bridge={bridge_w} should be < associative={assoc_w}"
    );
}

#[tokio::test]
async fn scoped_neighbors_full_incident_ceiling_batches_source_admission_without_overflow() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let mem = Memory::new(
        counted.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config::default(),
    );
    let hub = NodeId(Ulid::from(1u128));
    store.put_node(&raw_active_node(hub)).await.unwrap();
    for offset in 0..MAX_INCIDENT_EDGES {
        let target = NodeId(Ulid::from(offset as u128 + 2));
        store.put_node(&raw_active_node(target)).await.unwrap();
        store
            .put_edge(&Edge::new(hub, target, 0.5, EdgeKind::Associative, 1))
            .await
            .unwrap();
    }
    counted.reset_reads();
    let rows = mem
        .resolved_neighbors_scoped(hub, 1, StatusFilter::ALL)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        *counted.neighbor_limits.lock().unwrap(),
        vec![MAX_INCIDENT_EDGES]
    );
    assert_eq!(
        *counted.status_batch_sizes.lock().unwrap(),
        vec![MAX_INCIDENT_EDGES, 1]
    );
    assert_eq!(*counted.batch_sizes.lock().unwrap(), vec![1]);
    assert_eq!(counted.point_reads.load(Ordering::Relaxed), 0);
}

struct RoutingPausedReranker {
    started: Notify,
    release: Notify,
}
#[async_trait]
impl Reranker for RoutingPausedReranker {
    fn semantic_id(&self) -> &'static str {
        "routing-paused-fixture"
    }
    async fn rerank(&self, _query: &str, docs: &[&str]) -> PortResult<Vec<f32>> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(vec![1.0; docs.len()])
    }
}

struct RoutingFixtureEmbedder;
#[async_trait]
impl Embedder for RoutingFixtureEmbedder {
    fn dim(&self) -> usize {
        4
    }
    fn fingerprint(&self) -> mneme_core::EmbeddingFingerprint {
        mneme_core::EmbeddingFingerprint::new("routing-fixture", 4, "l2-f32-v1", "symmetric-v1")
    }
    async fn embed(&self, texts: &[&str]) -> PortResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
    }
}

async fn assert_v1_routing_bindings_are_stale_without_meaning_edits<S>(store: Arc<S>)
where
    S: GraphStore + VectorIndex + Traversal + 'static,
{
    use mneme_core::ports::{RoutingBinding, RoutingHint, SignedRoutingBias};

    let root = raw_active_node(NodeId(Ulid::from(900_001u128)));
    let target = raw_active_node(NodeId(Ulid::from(900_002u128)));
    // Keep the V1 vector's authored inputs frozen if shared helpers change.
    for (node, id) in [
        (&root, "0000000000000000000000VEX1"),
        (&target, "0000000000000000000000VEX2"),
    ] {
        assert_eq!(node.id().0.to_string(), id);
        assert_eq!(node.summary(), format!("node {id}"));
        assert_eq!(node.body().as_str(), format!("inline://{id}"));
        assert!(node.episode().is_none());
        assert_eq!(node.tags().count(), 0);
        assert_eq!(node.provenance(), &Provenance::derived_empty());
        assert!(node.origin_commit().is_none());
    }
    store.put_node(&root).await.unwrap();
    store.put_node(&target).await.unwrap();
    store
        .upsert(root.id(), &[1.0, 0.0, 0.0, 0.0])
        .await
        .unwrap();
    store
        .upsert(target.id(), &[0.5, 0.75_f32.sqrt(), 0.0, 0.0])
        .await
        .unwrap();
    let edge = Edge::from_stored(
        root.id(),
        target.id(),
        EdgeKind::Transition,
        None,
        0.6,
        1,
        7,
        2,
    );
    store.put_edge(&edge).await.unwrap();

    // Pinned pre-change V1 codec vector, not a genuine old executable artifact.
    // Derived offline from the frozen before/crates/mneme-core/src/ports.rs:
    // each content field is UTF-8 prefixed by its u64 little-endian byte length:
    // ["mneme.routing-content.v1", ID, "Semantic", "node " + ID,
    //  "inline://" + ID, "[]", "Derived { from: DerivedSources([]) }", "None"].
    // ID is 0000000000000000000000VEX1 / 0000000000000000000000VEX2.
    // Edge bytes are the literal UTF-8 string
    // "mneme.routing-edge.v1|0000000000000000000000VEX1|0000000000000000000000VEX2|Transition|None|3f19999a".
    // SHA-256 is pinned here; never derive old expectations from today's codec.
    let old = RoutingBinding {
        previous: root.id(),
        target: target.id(),
        edge_from: root.id(),
        edge_to: target.id(),
        previous_fingerprint: "9d15ccb75d9b4514442e4769888cea5af58a9082311a6d87312152aac01a9b9f"
            .into(),
        target_fingerprint: "3cc4aae52bd49ee2fe1efbee50e7559b20d40d88b18e3232c2294ca621e5cde2"
            .into(),
        edge_fingerprint: "b678093ae443a0b773012d9f42d569c6a4dad78102a23a3fcfc860eb5ebdea9e".into(),
    };
    assert!(
        old.has_canonical_fingerprints(),
        "V1 is structurally valid, not malformed input"
    );
    let fresh = RoutingBinding::new(&root, &target, &edge);
    assert_ne!(fresh.previous_fingerprint, old.previous_fingerprint);
    assert_ne!(fresh.target_fingerprint, old.target_fingerprint);
    assert_ne!(fresh.edge_fingerprint, old.edge_fingerprint);

    let before_nodes = [
        serde_json::to_value(store.get_node(root.id()).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(store.get_node(target.id()).await.unwrap().unwrap()).unwrap(),
    ];
    let before_edge = serde_json::to_value(
        store
            .get_edge(root.id(), target.id())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let budget = Budget {
        max_nodes: 16,
        max_depth: 1,
        min_relevance: 0.0,
        relevance_ratio: 0.0,
        explore: 0.0,
        query_conditioning: 1.0,
        dedup_similarity: 1.0,
        ..Budget::default()
    };
    let signature = |batch: &mneme_engine::RetrievalBatch| {
        batch
            .primary
            .iter()
            .map(|hit| {
                let path = hit.graph_path.as_ref().map(|hops| {
                    hops.iter()
                        .map(|hop| {
                            serde_json::json!({
                                "previous": hop.previous,
                                "target": hop.target,
                                "edge": hop.edge,
                            })
                        })
                        .collect::<Vec<_>>()
                });
                (hit.node.id(), hit.lane_rank, hit.evidence, path)
            })
            .collect::<Vec<_>>()
    };
    for graph_seed_cap in [0, 1] {
        let memory = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(RoutingFixtureEmbedder),
            Arc::new(SystemClock),
            Config {
                graph_seed_cap,
                graph_slot_cap: 8,
                lexical_k: 0,
                ..Config::default()
            },
        );
        let baseline = memory
            .retrieve_batch_seeded_observed("query", 1, budget, StatusFilter::ACTIVE, &[])
            .await
            .unwrap();
        let target_hit = baseline
            .primary
            .iter()
            .find(|hit| hit.node.id() == target.id());
        if graph_seed_cap == 0 {
            assert!(target_hit.is_none());
        } else {
            // Make stale negative-route neutrality non-vacuous: the target is
            // actually present via this edge, not already missing from recall.
            assert!(target_hit.unwrap().graph_path.as_ref().is_some_and(|path| {
                path.iter()
                    .any(|hop| hop.edge.from == root.id() && hop.edge.to == target.id())
            }));
        }
        for sign in [SignedRoutingBias::Boost, SignedRoutingBias::Weaken] {
            let result = memory
                .retrieve_batch_seeded_routed(
                    "query",
                    1,
                    budget,
                    StatusFilter::ACTIVE,
                    &[],
                    &[RoutingHint {
                        route: old.clone(),
                        sign,
                    }],
                )
                .await
                .unwrap();
            assert_eq!(signature(&result), signature(&baseline));
            let outcome = result.routing.as_ref().unwrap();
            assert_eq!(outcome.diagnostics.validated, 0);
            assert_eq!(outcome.diagnostics.ignored, 1);
            assert!(outcome.conditional_bindings.is_empty());
        }
        if graph_seed_cap == 0 {
            let result = memory
                .retrieve_batch_seeded_routed(
                    "query",
                    1,
                    budget,
                    StatusFilter::ACTIVE,
                    &[],
                    &[RoutingHint {
                        route: fresh.clone(),
                        sign: SignedRoutingBias::Boost,
                    }],
                )
                .await
                .unwrap();
            let outcome = result.routing.as_ref().unwrap();
            assert_eq!(outcome.diagnostics.validated, 1);
            assert_eq!(outcome.conditional_bindings[&target.id()], fresh);
            let hit = result
                .primary
                .iter()
                .find(|hit| hit.node.id() == target.id())
                .unwrap();
            assert!(hit.graph_path.is_none());
            assert!(hit.evidence.graph_rank.is_none());
        }
    }
    // Read-only routing is not feedback: whole canonical snapshots include
    // confidence/use telemetry and edge weight, trials and interference. Neither
    // stale sign (nor fresh conditional recall) rewrites the persisted evidence.
    assert_eq!(
        before_nodes,
        [
            serde_json::to_value(store.get_node(root.id()).await.unwrap().unwrap()).unwrap(),
            serde_json::to_value(store.get_node(target.id()).await.unwrap().unwrap()).unwrap(),
        ]
    );
    assert_eq!(
        before_edge,
        serde_json::to_value(
            store
                .get_edge(root.id(), target.id())
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap()
    );
}

#[tokio::test]
async fn reference_engine_ignores_v1_bindings_for_unchanged_semantic_nodes() {
    assert_v1_routing_bindings_are_stale_without_meaning_edits(Arc::new(MemStore::new(4))).await;
}

#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_engine_ignores_v1_bindings_for_unchanged_semantic_nodes() {
    assert_v1_routing_bindings_are_stale_without_meaning_edits(Arc::new(
        CozoStore::new(4).unwrap(),
    ))
    .await;
}

async fn assert_engine_routing_hints<S>(store: Arc<S>)
where
    S: GraphStore + VectorIndex + Traversal + 'static,
{
    use mneme_core::ports::{RoutingBinding, RoutingHint, SignedRoutingBias};
    let root = raw_active_node(NodeId(Ulid::from(900_001u128)));
    let target = raw_active_node(NodeId(Ulid::from(900_002u128)));
    store.put_node(&root).await.unwrap();
    store.put_node(&target).await.unwrap();
    store
        .upsert(root.id(), &[1.0, 0.0, 0.0, 0.0])
        .await
        .unwrap();
    store
        .upsert(target.id(), &[0.5, 0.75_f32.sqrt(), 0.0, 0.0])
        .await
        .unwrap();
    let edge = Edge::new(root.id(), target.id(), 0.6, EdgeKind::Transition, 1);
    store.put_edge(&edge).await.unwrap();
    for i in 0..32u128 {
        let node = raw_active_node(NodeId(Ulid::from(900_100 + i)));
        store.put_node(&node).await.unwrap();
        store
            .upsert(node.id(), &[0.0, 1.0, 0.0, 0.0])
            .await
            .unwrap();
        store
            .put_edge(&Edge::new(
                root.id(),
                node.id(),
                0.7,
                EdgeKind::Transition,
                1,
            ))
            .await
            .unwrap();
    }
    let memory = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(RoutingFixtureEmbedder),
        Arc::new(SystemClock),
        Config {
            graph_seed_cap: 1,
            graph_slot_cap: 8,
            ..Config::default()
        },
    );
    let budget = Budget {
        max_nodes: 128,
        max_depth: 1,
        min_relevance: 0.0,
        relevance_ratio: 0.0,
        explore: 0.0,
        query_conditioning: 1.0,
        dedup_similarity: 1.0,
        ..Budget::default()
    };
    let baseline = memory
        .retrieve_batch_seeded_observed("query", 1, budget, StatusFilter::ACTIVE, &[])
        .await
        .unwrap();
    assert!(!baseline.primary.iter().any(|h| h.node.id() == target.id()));
    assert!(baseline.routing.is_none());
    let hint = RoutingHint {
        route: RoutingBinding::new(&root, &target, &edge),
        sign: SignedRoutingBias::Boost,
    };
    let routed = memory
        .retrieve_batch_seeded_routed(
            "query",
            1,
            budget,
            StatusFilter::ACTIVE,
            &[],
            &[hint.clone()],
        )
        .await
        .unwrap();
    assert!(routed.primary.iter().any(|h| h.node.id() == target.id()));
    let outcome = routed.routing.unwrap();
    assert_eq!(outcome.diagnostics.validated, 1);
    assert_eq!(outcome.conditional_bindings[&target.id()], hint.route);
    assert!(!outcome.bindings.contains_key(&target.id()));
    assert!(
        routed
            .primary
            .iter()
            .find(|h| h.node.id() == target.id())
            .unwrap()
            .graph_path
            .is_none()
    );
    assert_eq!(
        store
            .get_edge(root.id(), target.id())
            .await
            .unwrap()
            .unwrap()
            .weight(),
        0.6
    );
    let duplicate = memory
        .retrieve_batch_seeded_routed(
            "query",
            1,
            budget,
            StatusFilter::ACTIVE,
            &[],
            &[hint.clone(), hint.clone()],
        )
        .await
        .unwrap();
    assert_eq!(duplicate.routing.unwrap().diagnostics.ignored, 2);
    assert!(!duplicate.primary.iter().any(|h| h.node.id() == target.id()));
    // Both endpoint meaning edits make a formerly valid hint stale.
    for id in [root.id(), target.id()] {
        let changed = Node::try_new(
            id,
            "rewritten meaning",
            BodyRef::new(format!("inline://{}", id.0)).unwrap(),
            std::iter::empty::<&str>(),
            prov(),
            1.0,
            1.0,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        store.put_node(&changed).await.unwrap();
        let stale = memory
            .retrieve_batch_seeded_routed(
                "query",
                1,
                budget,
                StatusFilter::ACTIVE,
                &[],
                &[hint.clone()],
            )
            .await
            .unwrap();
        assert_eq!(stale.routing.unwrap().diagnostics.ignored, 1);
        assert!(!stale.primary.iter().any(|h| h.node.id() == target.id()));
        store
            .put_node(if id == root.id() { &root } else { &target })
            .await
            .unwrap();
    }
    store
        .put_edge(&Edge::new(
            root.id(),
            target.id(),
            0.5,
            EdgeKind::Transition,
            2,
        ))
        .await
        .unwrap();
    let stale = memory
        .retrieve_batch_seeded_routed("query", 1, budget, StatusFilter::ACTIVE, &[], &[hint])
        .await
        .unwrap();
    assert_eq!(stale.routing.unwrap().diagnostics.ignored, 1);
    store.put_edge(&edge).await.unwrap();
    let reranker = Arc::new(RoutingPausedReranker {
        started: Notify::new(),
        release: Notify::new(),
    });
    let memory = Arc::new(
        Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(RoutingFixtureEmbedder),
            Arc::new(SystemClock),
            Config {
                graph_seed_cap: 1,
                graph_slot_cap: 8,
                ..Config::default()
            },
        )
        .with_reranker(reranker.clone()),
    );
    let reader = memory.clone();
    let live_hint = RoutingHint {
        route: RoutingBinding::new(&root, &target, &edge),
        sign: SignedRoutingBias::Boost,
    };
    let task = tokio::spawn(async move {
        reader
            .retrieve_batch_seeded_routed(
                "query",
                1,
                budget,
                StatusFilter::ACTIVE,
                &[],
                &[live_hint],
            )
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reranker.started.notified(),
    )
    .await
    .unwrap();
    // Ordinary engine mutation is possible while provider/reranker work is
    // outside the gate. Later observations must not bind old hops to new edges.
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        memory.link(root.id(), target.id(), EdgeKind::Transition, 0.5, None),
    )
    .await
    .unwrap()
    .unwrap();
    reranker.release.notify_one();
    let edited = task.await.unwrap();
    assert!(!edited.primary.iter().any(|h| h.node.id() == target.id()));
    let outcome = edited.routing.unwrap();
    assert!(!outcome.bindings.contains_key(&target.id()));
    assert!(!outcome.conditional_bindings.contains_key(&target.id()));
}

#[tokio::test]
async fn reference_engine_routing_hints_validate_before_raw_cutoff() {
    assert_engine_routing_hints(Arc::new(MemStore::new(4))).await;
}
#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_engine_routing_hints_validate_before_raw_cutoff() {
    assert_engine_routing_hints(Arc::new(CozoStore::new(4).unwrap())).await;
}

async fn assert_engine_routing_window<S>(store: Arc<S>)
where
    S: GraphStore + VectorIndex + Traversal + 'static,
{
    use mneme_core::ports::{RoutingBinding, RoutingHint, SignedRoutingBias};
    let root = raw_active_node(NodeId(Ulid::from(910_000u128)));
    store.put_node(&root).await.unwrap();
    store
        .upsert(root.id(), &[1.0, 0.0, 0.0, 0.0])
        .await
        .unwrap();
    let mut hints = Vec::new();
    for index in 0..8u128 {
        let child = raw_active_node(NodeId(Ulid::from(910_001 + index)));
        let grandchild = raw_active_node(NodeId(Ulid::from(910_101 + index)));
        for node in [&child, &grandchild] {
            store.put_node(node).await.unwrap();
            store
                .upsert(node.id(), &[0.0, 1.0, 0.0, 0.0])
                .await
                .unwrap();
        }
        for (previous, target) in [(&root, &child), (&child, &grandchild)] {
            let edge = Edge::new(previous.id(), target.id(), 0.8, EdgeKind::Transition, 1);
            store.put_edge(&edge).await.unwrap();
            hints.push(RoutingHint {
                route: RoutingBinding::new(previous, target, &edge),
                sign: if index % 2 == 0 {
                    SignedRoutingBias::Boost
                } else {
                    SignedRoutingBias::Weaken
                },
            });
        }
    }
    let config = Config {
        graph_seed_cap: 1,
        graph_slot_cap: 64,
        lexical_k: 0,
        ..Config::default()
    };
    let budget = Budget {
        max_nodes: 64,
        max_depth: 2,
        min_relevance: 0.0,
        relevance_ratio: 0.0,
        explore: 0.0,
        query_conditioning: 0.0,
        dedup_similarity: 1.0,
        ..Budget::default()
    };
    let memory = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(RoutingFixtureEmbedder),
        Arc::new(SystemClock),
        config,
    );
    let routed = memory
        .retrieve_batch_seeded_routed("query", 1, budget, StatusFilter::ACTIVE, &[], &hints)
        .await
        .unwrap();
    let outcome = routed.routing.as_ref().unwrap();
    assert_eq!(outcome.diagnostics.validated, 16);
    assert_eq!(outcome.diagnostics.ignored, 0);
    assert_eq!(
        outcome.bindings.len() + outcome.conditional_bindings.len(),
        16,
        "no independent eight-binding output cutoff"
    );
    assert_eq!(
        routed.primary.len(),
        17,
        "fixed root/fanout/depth reach the same bounded topology"
    );
    for hint in &hints {
        assert_eq!(
            outcome
                .bindings
                .get(&hint.route.target)
                .or_else(|| outcome.conditional_bindings.get(&hint.route.target)),
            Some(&hint.route)
        );
        let hit = routed
            .primary
            .iter()
            .find(|hit| hit.node.id() == hint.route.target)
            .unwrap();
        assert_eq!(
            hit.graph_path.is_none(),
            outcome
                .conditional_bindings
                .contains_key(&hint.route.target)
        );
        assert_eq!(
            store
                .get_edge(hint.route.edge_from, hint.route.edge_to)
                .await
                .unwrap()
                .unwrap()
                .weight(),
            0.8
        );
    }
    let baseline = memory
        .retrieve_batch_seeded_routed("query", 1, budget, StatusFilter::ACTIVE, &[], &[])
        .await
        .unwrap();
    assert_eq!(baseline.routing.unwrap().bindings.len(), 16);
    assert_eq!(
        baseline
            .primary
            .iter()
            .map(|h| h.node.id())
            .collect::<HashSet<_>>(),
        routed
            .primary
            .iter()
            .map(|h| h.node.id())
            .collect::<HashSet<_>>()
    );
    let mut opposing = hints[..8].to_vec();
    let mut conflict = hints[0].clone();
    conflict.sign = SignedRoutingBias::Weaken;
    opposing.push(conflict);
    let duplicated = memory
        .retrieve_batch_seeded_routed("query", 1, budget, StatusFilter::ACTIVE, &[], &opposing)
        .await
        .unwrap();
    assert_eq!(
        duplicated.routing.as_ref().unwrap().diagnostics.validated,
        7
    );
    assert_eq!(duplicated.routing.as_ref().unwrap().diagnostics.ignored, 2);

    // Equal reranker scores order by ID: eight children precede this ninth
    // route-bearing target. Editing it while reranking must invalidate that
    // exact binding, not bind an old traversal to its new canonical meaning.
    let ninth = NodeId(Ulid::from(910_101u128));
    let reranker = Arc::new(RoutingPausedReranker {
        started: Notify::new(),
        release: Notify::new(),
    });
    let memory = Arc::new(
        Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(RoutingFixtureEmbedder),
            Arc::new(SystemClock),
            config,
        )
        .with_reranker(reranker.clone()),
    );
    let reader = memory.clone();
    let task = tokio::spawn(async move {
        reader
            .retrieve_batch_seeded_routed("query", 1, budget, StatusFilter::ACTIVE, &[], &[])
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reranker.started.notified(),
    )
    .await
    .unwrap();
    let changed = Node::try_new(
        ninth,
        "changed ninth endpoint meaning",
        BodyRef::new(format!("inline://{}", ninth.0)).unwrap(),
        std::iter::empty::<&str>(),
        prov(),
        1.0,
        1.0,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    store.put_node(&changed).await.unwrap();
    reranker.release.notify_one();
    let edited = task.await.unwrap();
    assert!(
        edited
            .primary
            .iter()
            .any(|h| h.node.id() == ninth && h.graph_path.is_some())
    );
    assert_eq!(edited.routing.as_ref().unwrap().bindings.len(), 15);
    assert!(
        !edited
            .routing
            .as_ref()
            .unwrap()
            .bindings
            .contains_key(&ninth)
    );
}

#[tokio::test]
async fn reference_engine_routing_window_accepts_more_than_eight_and_revalidates_ninth() {
    assert_engine_routing_window(Arc::new(MemStore::new(4))).await;
}

#[cfg(feature = "cozo")]
#[tokio::test]
async fn cozo_engine_routing_window_accepts_more_than_eight_and_revalidates_ninth() {
    assert_engine_routing_window(Arc::new(CozoStore::new(4).unwrap())).await;
}

#[tokio::test]
async fn engine_whole_routing_overflow_is_neutral_before_meaning_or_edge_gets() {
    use mneme_core::ports::{
        MAX_ROUTING_HINT_BYTES, MAX_ROUTING_HINTS, RoutingBinding, RoutingHint, SignedRoutingBias,
    };
    let store = Arc::new(MemStore::new(4));
    let counted = Arc::new(CountingGraphStore::new(store.clone()));
    let root = raw_active_node(NodeId(Ulid::from(920_000u128)));
    store.put_node(&root).await.unwrap();
    store
        .upsert(root.id(), &[1.0, 0.0, 0.0, 0.0])
        .await
        .unwrap();
    let mut hints = Vec::new();
    for index in 0..=MAX_ROUTING_HINTS {
        // Absent distinct targets prevent duplicate-route neutrality from
        // masking whether any signed prefix reached canonical validation.
        let absent = raw_active_node(NodeId(Ulid::from(920_100 + index as u128)));
        let edge = Edge::new(root.id(), absent.id(), 0.8, EdgeKind::Transition, 1);
        hints.push(RoutingHint {
            route: RoutingBinding::new(&root, &absent, &edge),
            sign: SignedRoutingBias::Boost,
        });
    }
    let memory = Memory::new(
        counted.clone(),
        store.clone(),
        store,
        Arc::new(RoutingFixtureEmbedder),
        Arc::new(SystemClock),
        Config {
            graph_seed_cap: 1,
            lexical_k: 0,
            ..Config::default()
        },
    );
    let budget = Budget {
        max_nodes: 64,
        max_depth: 1,
        min_relevance: 0.0,
        relevance_ratio: 0.0,
        query_conditioning: 0.0,
        dedup_similarity: 1.0,
        explore: 0.0,
        ..Budget::default()
    };
    let mut byte_overflow = hints[..2].to_vec();
    byte_overflow[1].route.edge_fingerprint = "\u{0001}".repeat(MAX_ROUTING_HINT_BYTES / 6);
    let mut malformed_opposite = vec![hints[0].clone(), hints[0].clone()];
    malformed_opposite[1].sign = SignedRoutingBias::Weaken;
    malformed_opposite[1].route.edge_fingerprint = "A".repeat(64);
    let mut malformed_unrelated = hints[..2].to_vec();
    malformed_unrelated[1].route.target_fingerprint = "a".repeat(63);
    for batch in [
        &hints[..],
        &byte_overflow[..],
        &malformed_opposite[..],
        &malformed_unrelated[..],
    ] {
        counted.reset_reads();
        let result = memory
            .retrieve_batch_seeded_routed("query", 1, budget, StatusFilter::ACTIVE, &[], batch)
            .await
            .unwrap();
        let diagnostics = result.routing.unwrap().diagnostics;
        assert_eq!(diagnostics.validated, 0);
        assert_eq!(diagnostics.ignored, batch.len());
        assert_eq!(counted.point_reads.load(Ordering::Relaxed), 0);
        assert_eq!(counted.edge_point_reads.load(Ordering::Relaxed), 0);
        assert_eq!(
            result.primary.len(),
            1,
            "ordinary baseline recall survives optional overflow"
        );
    }
    counted.reset_reads();
    let within = memory
        .retrieve_batch_seeded_routed("query", 1, budget, StatusFilter::ACTIVE, &[], &hints[..1])
        .await
        .unwrap();
    assert_eq!(within.routing.unwrap().diagnostics.ignored, 1);
    assert_eq!(
        counted.point_reads.load(Ordering::Relaxed),
        2,
        "control proves in-budget stale binding actually performs canonical validation"
    );
}

async fn body_edit_contract<S>(store: Arc<S>)
where
    S: GraphStore + VectorIndex + LexicalIndex + Traversal + 'static,
{
    let bodies = Arc::new(ControlledBodyStore::default());
    let embedder = Arc::new(CountingEmbedder {
        inner: HashingEmbedder::new(DEFAULT_DIM),
        calls: AtomicU64::new(0),
    });
    let memory = Arc::new(
        Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            embedder.clone(),
            Arc::new(SystemClock),
            Config {
                default_body_scheme: "controlled",
                min_similarity_links: 0,
                ..Config::default()
            },
        )
        .with_body_store(bodies.clone()),
    );
    let capture = || {
        Capture::new(
            "codex",
            "body-edit-contract",
            "codex://body-edit-contract",
            Some("session"),
            Some("turn"),
            "unchanged retrieval summary",
            b"original evidence",
            &["test"],
        )
    };
    let id = memory.capture(capture()).await.unwrap().id;
    let before = store.get_node(id).await.unwrap().unwrap();
    let revision = before.body_revision();
    assert_eq!(
        mneme_core::BodyRevision::parse(revision.to_hex()).unwrap(),
        revision
    );
    assert!(mneme_core::BodyRevision::parse(&revision.to_hex().to_uppercase()).is_err());
    let query = embedder
        .inner
        .embed(&[before.summary()])
        .await
        .unwrap()
        .remove(0);
    let vector_before = store.ann(&query, 10, StatusFilter::ALL).await.unwrap();
    let lexical_before = store
        .search(before.summary(), 10, StatusFilter::ALL)
        .await
        .unwrap();
    let calls = embedder.calls.load(Ordering::SeqCst);
    bodies.fail_put.store(true, Ordering::SeqCst);
    assert!(matches!(
        memory.edit_body(id, &revision, b"put failure").await,
        Err(Error::Body(_))
    ));
    assert_eq!(
        store.get_node(id).await.unwrap().unwrap().body_revision(),
        revision
    );
    bodies.fail_put.store(false, Ordering::SeqCst);
    let changed = memory
        .edit_body(id, &revision, b"edited evidence")
        .await
        .unwrap();
    assert_ne!(revision, changed.body_revision());
    let mut expected = before.clone();
    expected.set_body_reference(changed.body().clone(), BodyOwnership::Managed);
    assert_eq!(
        serde_json::to_value(&changed).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(
        memory.resolve_body(&changed).await.unwrap(),
        b"edited evidence"
    );
    assert_eq!(
        memory.resolve_body(&before).await.unwrap(),
        b"original evidence"
    );
    assert_eq!(embedder.calls.load(Ordering::SeqCst), calls);
    let scores = |rows: Vec<Scored>| {
        rows.into_iter()
            .map(|row| (row.id, row.score.to_bits()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        scores(store.ann(&query, 10, StatusFilter::ALL).await.unwrap()),
        scores(vector_before)
    );
    assert_eq!(
        scores(
            store
                .search(before.summary(), 10, StatusFilter::ALL)
                .await
                .unwrap()
        ),
        scores(lexical_before)
    );
    assert_eq!(bodies.delete_calls.load(Ordering::SeqCst), 0);
    let puts = bodies.put_calls.load(Ordering::SeqCst);
    assert!(matches!(
        memory.edit_body(id, &revision, b"stale").await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        memory
            .edit_body(NodeId(Ulid::new()), &revision, b"missing")
            .await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        memory
            .edit_body(
                id,
                &changed.body_revision(),
                &vec![0; mneme_engine::MAX_CAPTURE_BODY_BYTES + 1]
            )
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(bodies.put_calls.load(Ordering::SeqCst), puts);
    assert!(memory.capture(capture()).await.unwrap().replayed);
    assert_eq!(
        store.get_node(id).await.unwrap().unwrap().body(),
        changed.body()
    );
    assert!(matches!(
        memory
            .capture(Capture::new(
                "codex",
                "body-edit-contract",
                "codex://body-edit-contract",
                Some("session"),
                Some("turn"),
                "changed source request",
                b"original evidence",
                &["test"]
            ))
            .await,
        Err(Error::Conflict(_))
    ));
    // Independent backend clients compete against the same pointer revision.
    let current = store.get_node(id).await.unwrap().unwrap();
    let revision = current.body_revision();
    let a = BodyRef::new("controlled://cas-a").unwrap();
    let b = BodyRef::new("controlled://cas-b").unwrap();
    let (left, right) = tokio::join!(
        store.compare_replace_node_body(id, &revision, &a),
        store.compare_replace_node_body(id, &revision, &b)
    );
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    let current = store.get_node(id).await.unwrap().unwrap();
    let changed = memory
        .edit_body(
            id,
            &current.body_revision(),
            b"restore readable after direct CAS probe",
        )
        .await
        .unwrap();
    let mut shared = Node::try_new(
        NodeId(Ulid::new()),
        "borrowed shared body",
        before.body().clone(),
        ["shared"],
        prov(),
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    store.put_node(&shared).await.unwrap();
    let edited_shared = memory
        .edit_body(
            shared.id(),
            &shared.body_revision(),
            b"now independently managed",
        )
        .await
        .unwrap();
    assert_eq!(edited_shared.body_ownership(), BodyOwnership::Managed);
    assert_eq!(
        memory.resolve_body(&shared).await.unwrap(),
        b"original evidence"
    );
    shared.set_body_reference(before.body().clone(), BodyOwnership::Managed);
    store.put_node(&shared).await.unwrap();
    let edited_shared = memory
        .edit_body(
            shared.id(),
            &shared.body_revision(),
            b"managed old blob still not reclaimed",
        )
        .await
        .unwrap();
    assert_ne!(edited_shared.body(), before.body());
    assert_eq!(
        memory.resolve_body(&before).await.unwrap(),
        b"original evidence"
    );
    assert_eq!(bodies.delete_calls.load(Ordering::SeqCst), 0);
    let other = memory
        .ingest(Ingest::new("other node", b"other body", &[], prov()))
        .await
        .unwrap();
    let mut incoming = Edge::new(other, id, 0.5, EdgeKind::Associative, 0);
    incoming.anchor = Some(BodySpan { start: 0, end: 1 });
    store.put_edge(&incoming).await.unwrap();
    let changed = memory
        .edit_body(
            id,
            &changed.body_revision(),
            b"incoming anchor does not belong to me",
        )
        .await
        .unwrap();
    let mut outgoing = Edge::new(id, other, 0.5, EdgeKind::Associative, 0);
    outgoing.anchor = Some(BodySpan { start: 0, end: 1 });
    bodies.pause_put.store(true, Ordering::SeqCst);
    let task = {
        let memory = memory.clone();
        let revision = changed.body_revision();
        tokio::spawn(async move {
            memory
                .edit_body(id, &revision, b"anchor raced my put")
                .await
        })
    };
    bodies.put_started.notified().await;
    store.put_edge(&outgoing).await.unwrap();
    bodies.release_put.notify_one();
    assert!(matches!(task.await.unwrap(), Err(Error::Conflict(_))));
    assert!(matches!(
        store
            .compare_replace_node_body(id, &changed.body_revision(), before.body())
            .await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        store.get_node(id).await.unwrap().unwrap().body(),
        changed.body()
    );
}

#[tokio::test]
async fn body_edit_memstore_contract() {
    body_edit_contract(Arc::new(MemStore::new(DEFAULT_DIM))).await;
}
#[cfg(feature = "cozo")]
#[tokio::test]
async fn body_edit_cozo_contract() {
    body_edit_contract(Arc::new(CozoStore::new(DEFAULT_DIM).unwrap())).await;
}
