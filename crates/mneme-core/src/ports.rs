//! Ports: the traits the domain speaks. Adapter crates implement these; the
//! engine composes them. The domain calls *these*, never raw Datalog, an HTTP
//! client, or the filesystem.
//!
//! Everything is `async` on purpose. Today's adapters are all in-process, so
//! the futures resolve immediately — but the moment a backend lives across a
//! network (a distributed graph store, a remote embedding API), the signatures
//! don't change. Going sync now would buy nothing and cost a rewrite later.
//!
//! The ports are split along the cost seam from the design — the *hot path*
//! (cheap, runs during a query: ANN, one-hop neighbors, deterministic edge
//! reinforcement) is kept separate from the *cold path* (background sweeps:
//! community detection, decay, contradiction triage). `GraphStore` stays a
//! dumb key/value-ish store; [`Traversal`] is its own port so a smarter
//! backend can own graph walks without bloating the store interface.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use unordered_pair::UnorderedPair;

pub use crate::concern::{
    ConcernCommitOutcome, ConcernPage, ConcernPageCursor, ConcernPageRequest, ConcernStore,
};
pub use crate::episode::EpisodeStore;
pub use crate::retag::{NodeContentGuard, RetagContentGuards};
pub use crate::tag_vocabulary::*;
pub use crate::tagged::*;
pub use crate::touchstone::TouchstoneStore;
use crate::{
    BodyRef, CaptureReplayProof, Contradiction, Edge, EdgeKind, EmbeddingFingerprint,
    FullMergeRecord, MAX_FEEDBACK_BATCH_EVENTS, MAX_FEEDBACK_BATCH_KEY_BYTES,
    MAX_FEEDBACK_EPOCH_BYTES, MAX_FEEDBACK_FINGERPRINT_BYTES, MAX_FULL_MERGE_INCIDENT_EDGES,
    MAX_FULL_MERGE_REMOTE_EDGES, MAX_INCIDENT_EDGES, MAX_REMOTE_EDGES_PER_SOURCE, MergeCandidate,
    MergeResolution, Node, NodeId, NodeStatus, RemoteEdge, RemoteEdgeCursor, RemoteEdgePage,
    Resolution, SupersedeRecord, TagValidationError, Timestamp,
};
use ulid::Ulid;

use crate::managed::{ManagedSnapshotPage, ManagedSnapshotRequest, ManagedStoreHead};

pub type Result<T> = std::result::Result<T, Error>;

/// Maximum number of caller-declared links in one atomic capture.
pub const MAX_CAPTURE_EDGES: usize = 8;

/// Maximum meaning-bound nominees inspected for one first-write capture.
/// This bounds discovery/admission work separately from the eight stored links.
pub const MAX_CAPTURE_PRIOR_CANDIDATES: usize = crate::MAX_NODE_HYDRATION_BATCH;

/// Per-arrival generated output allowance, not an endpoint lifetime degree.
/// A distinct type prevents older adapter implementations interpreting this as
/// the removed experimental association-degree ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapturePriorBudget(usize);
impl CapturePriorBudget {
    pub fn new(limit: usize) -> Self {
        Self(limit.min(MAX_CAPTURE_EDGES))
    }
    pub fn limit(self) -> usize {
        self.0
    }
}

pub fn validate_capture_prior_candidates(priors: &[CaptureSimilarityPrior]) -> Result<()> {
    if priors.len() > MAX_CAPTURE_PRIOR_CANDIDATES {
        return Err(Error::CapacityExceeded {
            resource: "capture prior candidates",
            limit: MAX_CAPTURE_PRIOR_CANDIDATES,
        });
    }
    Ok(())
}

/// A bounded, optional similarity prior, bound to the target meaning observed
/// during discovery. This is derived routing state, never source request identity.
#[derive(Clone, Debug)]
pub struct CaptureSimilarityPrior {
    edge: Edge,
    target_fingerprint: String,
}

impl CaptureSimilarityPrior {
    pub fn new(edge: Edge, target: &Node) -> Result<Self> {
        validate_capture_edges(edge.from, std::slice::from_ref(&edge))?;
        if edge.kind != EdgeKind::Associative || edge.to != target.id() || !target.is_semantic() {
            return Err(Error::InvalidInput(
                "invalid capture similarity prior".into(),
            ));
        }
        Ok(Self {
            edge,
            target_fingerprint: routing_content_fingerprint(target),
        })
    }
    pub fn edge(&self) -> &Edge {
        &self.edge
    }
    pub fn matches_target(&self, target: &Node) -> bool {
        target.id() == self.edge.to
            && target.is_semantic()
            && target.status() == NodeStatus::Active
            && routing_content_fingerprint(target) == self.target_fingerprint
    }
}

/// Validate the request shape independently of target existence or replay state.
pub fn validate_capture_edges(id: NodeId, edges: &[Edge]) -> Result<()> {
    if edges.len() > MAX_CAPTURE_EDGES {
        return Err(Error::CapacityExceeded {
            resource: "capture edges",
            limit: MAX_CAPTURE_EDGES,
        });
    }
    let mut targets = HashSet::new();
    for edge in edges {
        edge.validate().map_err(Error::InvalidInput)?;
        if edge.from != id || edge.to == id || !targets.insert(edge.to) {
            return Err(Error::InvalidInput(
                "capture edges require distinct, non-self targets and the captured source".into(),
            ));
        }
        if !matches!(
            edge.kind,
            EdgeKind::Associative | EdgeKind::Transition | EdgeKind::DerivedFrom
        ) {
            return Err(Error::InvalidInput(
                "capture edge kind is unsupported".into(),
            ));
        }
    }
    Ok(())
}

/// Read-only managed-storage identity and snapshot commitments.
///
/// This port is intentionally separate from [`GraphStore`]: exposing a generic
/// caller-controlled transaction there would bypass operation-specific epoch,
/// ownership, and retry invariants. Implementations of these methods must not
/// repair schemas, clear ledgers, checkpoint, or otherwise mutate durable state.
///
/// The current page payload is a bounded local page of record commitments because
/// `mneme-core` does not yet own a closed inventory for every adapter relation.
/// It proves pagination mechanics, not record completeness or apply authority.
/// C2 must add the typed canonical inventory, and the native snapshot publisher
/// must authenticate its ordered page hashes, before managed bind or apply can
/// rely on this port.
#[async_trait]
pub trait ManagedStore: Send + Sync {
    /// Observe the exact current store head without installing missing metadata.
    /// Managed equality includes the mutation epoch. An unmanaged result reports
    /// only logical database identity and is not a consistency token.
    async fn managed_head(&self) -> Result<ManagedStoreHead>;

    /// Return one validated page for the request's observed head.
    ///
    /// For a managed head, implementations must fail with [`Error::Conflict`]
    /// if the stored identity or epoch differs and must never silently restart
    /// from a newer head. For an unmanaged head, this port cannot detect graph
    /// drift: implementations or their native callers must page an immutable
    /// adapter snapshot or lease-owned backup, never a mutating conventional
    /// store.
    async fn managed_snapshot_page(
        &self,
        request: &ManagedSnapshotRequest,
    ) -> Result<ManagedSnapshotPage>;
}

/// A capability token proving the caller is on the **cold path** — a background
/// sweep or other off-turn maintenance, not servicing a live query. The
/// expensive operations (whole-graph scans, decay, community detection,
/// contradiction triage) demand one, so they can't be reached by accident from
/// the hot path: you'd have to mint a `ColdPath` right there, which is a loud
/// signal at the call site that you're about to do slow work.
///
/// It's a ZST, so it's passed *by value* (a `&ColdPath` would be pointer-sized
/// and might not inline away) and is `Copy` — the constructor is the assertion;
/// copying it afterward is free. The private field makes it unforgeable outside
/// this crate, so downstream code must go through [`ColdPath::acquire`].
#[derive(Clone, Copy, Debug)]
pub struct ColdPath(());

impl ColdPath {
    /// Mint a cold-path token. Call this only from a maintenance driver or the
    /// daemon's off-turn loop — never while servicing a retrieval.
    #[inline]
    pub fn acquire() -> Self {
        ColdPath(())
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("episode unavailable: {0}")]
    EpisodeUnavailable(crate::episode::EpisodeUnavailableReason),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("{resource} capacity exceeded (limit {limit})")]
    CapacityExceeded {
        resource: &'static str,
        limit: usize,
    },
    #[error("backend: {0}")]
    Backend(String),
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("body resolution: {0}")]
    Body(String),
    #[error("embedding dim mismatch: index={index}, provider={provider}")]
    DimMismatch { index: usize, provider: usize },
    #[error("legacy embedding index contains data but has no embedding fingerprint")]
    LegacyEmbeddingFingerprint,
    #[error("embedding fingerprint mismatch: stored={stored}; runtime={runtime}")]
    EmbeddingFingerprintMismatch {
        stored: Box<EmbeddingFingerprint>,
        runtime: Box<EmbeddingFingerprint>,
    },
    #[error("invalid embedding fingerprint: {0}")]
    InvalidEmbeddingFingerprint(String),
}

impl From<TagValidationError> for Error {
    fn from(error: TagValidationError) -> Self {
        Self::InvalidInput(error.to_string())
    }
}

/// One hop out of a node: the edge, the node on the far end, and whether it was
/// reached *against* the edge's stored arrow. Directionality is intrinsic to the
/// edge kind, not a caller-supplied axis, so there's no `Dir` parameter:
///
/// - an `Associative` edge surfaces from both ends — forward as an *outgoing*
///   hop, and in reverse (`incoming`) only when no matching outgoing edge exists,
///   so the reverse direction is navigable but materializes its own weight only
///   once it's actually walked (lazy bidirectionality, asymmetric weights);
/// - a learned `Transition` surfaces only from its `from` endpoint, following the
///   explicitly observed `prior -> target` direction;
/// - a directional kind (`Supersedes`, `DerivedFrom`) surfaces only on its
///   target, as an `incoming` hop (the superseded node sees its winner; the
///   winner isn't dragged back to the stale node).
#[derive(Clone, Debug)]
pub struct Neighbor {
    pub edge: Edge,
    /// The node on the other end of `edge`.
    pub node: NodeId,
    /// True if `edge` points *at* the queried node (reached in reverse).
    pub incoming: bool,
}

/// Caps on a spread. Both are safety rails, not the primary control: an agent
/// drives traversal itself (walk with `get`/`neighbors`, stop early once it's
/// sure the rest is irrelevant), so the default reach is generous rather than
/// the old aggressive 2–3 hops. `min_relevance` prunes branches once propagated
/// activation drops below the floor. (A future dynamic budget — extend while the
/// agent keeps emitting `RelevantNew` — belongs with the design's parallel
/// per-seed traversal, which isn't built yet.)
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub max_nodes: usize,
    pub max_depth: u8,
    pub min_relevance: f32,
    /// Probability of following an *under-weighted* edge that would otherwise be
    /// pruned — admitting it at the relevance floor. A small ε keeps spreading
    /// activation from always tracking the strongest (self-reinforcing) edges, so
    /// recall occasionally surfaces low-weight / long-range associations and
    /// escapes well-worn paths. `0.0` disables it (the default; tests rely on it).
    pub explore: f32,
    /// Adaptive cutoff: keep only results scoring within this fraction of the
    /// *top* hit (floor = `max(min_relevance, top · relevance_ratio)`). Makes the
    /// returned set adapt to the query — a sharp query yields a few strong hits, a
    /// broad one yields more — instead of a flat node count dragging in a long
    /// marginal tail. `0.0` disables it (the default; tests rely on it).
    pub relevance_ratio: f32,
    /// Near-duplicate cutoff: a result whose summary embedding is at least this
    /// cosine-similar to a higher-ranked kept result is dropped, so recall returns
    /// distinct memories instead of clusters of paraphrases. `≥ 1.0` disables it
    /// (the default; tests rely on it).
    pub dedup_similarity: f32,
    /// Query-conditioning strength γ ∈ [0, 1] for spreading activation: how much to
    /// damp the activation reaching a node by that node's semantic *distance* from
    /// the query, so the spread follows edges that lead *toward the query*, not just
    /// the strongest (self-reinforcing) ones. Each reached node's activation is
    /// scaled by `(1-γ) + γ·sim(query, node)`, so γ=1 fully gates on relevance and
    /// `0.0` disables it (the default; tests and direct spreads rely on it).
    /// Requires a query embedding be passed to [`Traversal::spread`].
    pub query_conditioning: f32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_nodes: 100,
            max_depth: 6,
            min_relevance: 0.05,
            explore: 0.0,
            relevance_ratio: 0.0,
            dedup_similarity: 1.0,
            query_conditioning: 0.0,
        }
    }
}

/// Which semantic node statuses an ANN search or traversal may return.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusFilter {
    pub active: bool,
    pub archived: bool,
}

impl StatusFilter {
    pub const ACTIVE: Self = Self {
        active: true,
        archived: false,
    };
    pub const ARCHIVED: Self = Self {
        active: false,
        archived: true,
    };
    pub const ALL: Self = Self {
        active: true,
        archived: true,
    };

    pub const fn allows(&self, status: NodeStatus) -> bool {
        match status {
            NodeStatus::Active => self.active,
            NodeStatus::Archived => self.archived,
        }
    }

    pub const fn is_empty(self) -> bool {
        !self.active && !self.archived
    }
    pub const fn overlaps(self, other: Self) -> bool {
        self.active && other.active || self.archived && other.archived
    }
}

impl Default for StatusFilter {
    fn default() -> Self {
        Self::ACTIVE
    }
}

/// The same membership boundary applies to roots and every graph hop.
#[derive(Clone, Copy, Debug)]
pub struct TraversalScope {
    pub status: StatusFilter,
}

impl TraversalScope {
    pub const fn new(status: StatusFilter) -> Self {
        Self { status }
    }
    pub const fn allows(&self, _id: NodeId, status: NodeStatus) -> bool {
        self.status.allows(status)
    }
}

/// Community label from the most recent detection pass. Ephemeral by design —
/// see the note on [`Contradiction`] about never persisting cluster-keyed state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClusterId(pub u32);

/// A node with a relevance score: an ANN hit, a seed, or a spread result.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scored {
    pub id: NodeId,
    pub score: f32,
}

/// Compare-and-commit replacement of one node inside a feedback transaction.
/// `expected` is a storage precondition, not rollback material: adapters must
/// reject the whole transaction if the canonical row no longer matches it.
#[derive(Clone, Debug)]
pub struct FeedbackNodeUpdate {
    pub expected: Node,
    pub replacement: Node,
}

/// Compare-and-commit replacement of one directed edge. `None` means the edge
/// must still be absent; this closes the create/create race at the transaction
/// boundary and lets the storage degree ceiling be checked against the final
/// batch rather than one optimistic engine read.
#[derive(Clone, Debug)]
pub struct FeedbackEdgeUpdate {
    pub expected: Option<Edge>,
    pub replacement: Edge,
}

/// One store-owned, atomic observation of a redundant semantic pair. Keeping
/// this as an operation rather than an engine-planned replacement lets the
/// adapter increment a concurrently existing candidate without a lost update.
/// The commit-wide `applied_at` timestamp is used for first/last seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedbackMergeObservation {
    pub between: UnorderedPair<NodeId>,
}

impl FeedbackMergeObservation {
    pub fn new(a: NodeId, b: NodeId) -> Result<Self> {
        let value = Self {
            between: UnorderedPair(a, b),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        if self.between.0 == self.between.1 {
            return Err(Error::InvalidInput(
                "feedback merge observation endpoints must differ".into(),
            ));
        }
        Ok(())
    }
}

/// Reachability metadata supplied by the trusted receipt host. Storage may
/// reclaim older proofs from this epoch only below `min_live_sequence`; another
/// epoch is unreachable because receipt capabilities are volatile and one host
/// exclusively leases a database. This contract ends when that host releases
/// its lease or restarts: a new epoch deliberately makes old proofs collectible,
/// so this is not durable exactly-once delivery across server generations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedbackRetryScope {
    /// Random identity of the volatile receipt-capability generation.
    pub epoch: String,
    /// Monotonic first-claim sequence within `(database, epoch)`.
    pub sequence: u64,
    /// Lowest sequence whose receipt batch is still reachable in this database.
    pub min_live_sequence: u64,
}

impl FeedbackRetryScope {
    pub fn new(epoch: impl Into<String>, sequence: u64, min_live_sequence: u64) -> Result<Self> {
        let value = Self {
            epoch: epoch.into(),
            sequence,
            min_live_sequence,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        if self.epoch.is_empty() || self.epoch.len() > MAX_FEEDBACK_EPOCH_BYTES {
            return Err(Error::InvalidInput(format!(
                "feedback epoch must contain 1..={MAX_FEEDBACK_EPOCH_BYTES} bytes"
            )));
        }
        if self.sequence == 0 || self.sequence >= i64::MAX as u64 {
            return Err(Error::InvalidInput(
                "feedback sequence must fit the positive storage integer range with one successor reserved".into(),
            ));
        }
        if self.min_live_sequence == 0 || self.min_live_sequence > self.sequence {
            return Err(Error::InvalidInput(
                "feedback live floor must be positive and no greater than the committing sequence"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// Optional durable at-most-once identity for a feedback commit. Replay identity
/// is the logical pair `(retry.epoch, key)`: a bare key may be reused by a new
/// host generation because that generation cannot possess an old receipt
/// capability. Within one epoch, the fingerprint binds the key to the exact
/// ordered path effects, so accidental or malicious key reuse with a different
/// payload fails closed instead of reporting a false replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedbackIdempotency {
    pub key: String,
    pub fingerprint: String,
    pub retry: FeedbackRetryScope,
}

impl FeedbackIdempotency {
    pub fn new(
        key: impl Into<String>,
        fingerprint: impl Into<String>,
        retry: FeedbackRetryScope,
    ) -> Result<Self> {
        let value = Self {
            key: key.into(),
            fingerprint: fingerprint.into(),
            retry,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        if self.key.is_empty() || self.key.len() > MAX_FEEDBACK_BATCH_KEY_BYTES {
            return Err(Error::InvalidInput(format!(
                "feedback idempotency key must contain 1..={MAX_FEEDBACK_BATCH_KEY_BYTES} bytes"
            )));
        }
        if self.fingerprint.len() != MAX_FEEDBACK_FINGERPRINT_BYTES
            || !self
                .fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::InvalidInput(format!(
                "feedback fingerprint must be a {MAX_FEEDBACK_FINGERPRINT_BYTES}-byte lowercase SHA-256 hex digest"
            )));
        }
        self.retry.validate()?;
        Ok(())
    }
}

/// Fully planned graph delta for one direct or receipt-feedback transaction.
/// Domain math stays in the engine; the adapter owns the all-or-nothing/CAS
/// boundary. The vectors and bodies are absent because feedback changes neither.
#[derive(Clone, Debug)]
pub struct FeedbackCommit {
    pub idempotency: Option<FeedbackIdempotency>,
    pub applied_at: Timestamp,
    pub nodes: Vec<FeedbackNodeUpdate>,
    pub edges: Vec<FeedbackEdgeUpdate>,
    pub merge_observations: Vec<FeedbackMergeObservation>,
}

impl FeedbackCommit {
    pub fn validate(&self) -> Result<()> {
        if self.applied_at > i64::MAX as Timestamp {
            return Err(Error::InvalidInput(
                "feedback timestamp exceeds the storage integer range".into(),
            ));
        }
        if self.nodes.len() > MAX_FEEDBACK_BATCH_EVENTS
            || self.edges.len() > MAX_FEEDBACK_BATCH_EVENTS
            || self.merge_observations.len() > MAX_FEEDBACK_BATCH_EVENTS
        {
            return Err(Error::CapacityExceeded {
                resource: "feedback batch updates",
                limit: MAX_FEEDBACK_BATCH_EVENTS,
            });
        }
        if let Some(idempotency) = &self.idempotency {
            idempotency.validate()?;
        }

        let mut node_ids = HashSet::with_capacity(self.nodes.len());
        for update in &self.nodes {
            if update.expected.id() != update.replacement.id() {
                return Err(Error::InvalidInput(
                    "feedback node replacement changes identity".into(),
                ));
            }
            if !update
                .expected
                .same_feedback_static_fields(&update.replacement)
            {
                return Err(Error::InvalidInput(
                    "feedback node replacement changes a non-feedback field".into(),
                ));
            }
            if !node_ids.insert(update.expected.id()) {
                return Err(Error::InvalidInput(
                    "feedback batch contains duplicate node replacements".into(),
                ));
            }
        }

        let mut edge_ids = HashSet::with_capacity(self.edges.len());
        for update in &self.edges {
            update.replacement.validate().map_err(|error| {
                Error::InvalidInput(format!("feedback edge replacement is invalid: {error}"))
            })?;
            if let Some(expected) = &update.expected {
                expected.validate().map_err(|error| {
                    Error::InvalidInput(format!("feedback edge precondition is invalid: {error}"))
                })?;
            }
            if update.replacement.from == update.replacement.to {
                return Err(Error::InvalidInput(
                    "feedback edge replacement cannot be a self-loop".into(),
                ));
            }
            if let Some(expected) = &update.expected
                && (expected.from != update.replacement.from
                    || expected.to != update.replacement.to)
            {
                return Err(Error::InvalidInput(
                    "feedback edge replacement changes identity".into(),
                ));
            }
            if let Some(expected) = &update.expected {
                if expected.kind != update.replacement.kind
                    || expected.anchor != update.replacement.anchor
                {
                    return Err(Error::InvalidInput(
                        "feedback edge replacement changes static edge fields".into(),
                    ));
                }
            } else if update.replacement.kind != crate::EdgeKind::Transition
                || update.replacement.anchor.is_some()
            {
                return Err(Error::InvalidInput(
                    "new feedback edges must be unanchored transitions".into(),
                ));
            }
            if !edge_ids.insert((update.replacement.from, update.replacement.to)) {
                return Err(Error::InvalidInput(
                    "feedback batch contains duplicate edge replacements".into(),
                ));
            }
        }

        let mut merge_pairs = HashSet::with_capacity(self.merge_observations.len());
        for observation in &self.merge_observations {
            observation.validate()?;
            if !merge_pairs.insert(observation.between) {
                return Err(Error::InvalidInput(
                    "feedback batch contains duplicate merge observations".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Storage result for a receipt-feedback commit. A replay is success: the exact
/// payload already took effect and callers may safely finish consuming receipts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackCommitOutcome {
    Applied,
    AlreadyApplied,
}

/// One bounded, store-owned full-collapse transaction. The canonical unordered
/// pair is the durable idempotency identity and the ordered winner/loser fields
/// are its payload. Adapters inspect only the indexed incident sets for these
/// two nodes and commit the resulting local/remote edge rewrite, loser archive,
/// candidate resolution, and retry proof together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FullMergeCommit {
    pub winner: NodeId,
    pub loser: NodeId,
    pub applied_at: Timestamp,
}

impl FullMergeCommit {
    pub fn new(winner: NodeId, loser: NodeId, applied_at: Timestamp) -> Result<Self> {
        let value = Self {
            winner,
            loser,
            applied_at,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn pair(&self) -> UnorderedPair<NodeId> {
        UnorderedPair(self.winner, self.loser)
    }

    pub fn record(&self) -> FullMergeRecord {
        FullMergeRecord::new(self.winner, self.loser, self.applied_at)
    }

    pub fn validate(&self) -> Result<()> {
        self.record().validate().map_err(Error::InvalidInput)
    }

    /// Normalize the complete indexed winner/loser incident union. Existing
    /// winner rows are inserted first so they retain kind/anchor on a duplicate;
    /// donor dynamic evidence is folded with [`Edge::absorb_full_merge_donor`]
    /// rather than manufacturing a reinforcement event.
    pub fn normalize_local_edges(&self, incident: &[Edge]) -> Result<Vec<Edge>> {
        self.validate()?;
        if incident.len() > MAX_FULL_MERGE_INCIDENT_EDGES {
            return Err(Error::CapacityExceeded {
                resource: "full merge incident edge read",
                limit: MAX_FULL_MERGE_INCIDENT_EDGES,
            });
        }

        let mut seen = HashSet::with_capacity(incident.len());
        let mut ordered = Vec::with_capacity(incident.len());
        for edge in incident {
            edge.validate().map_err(Error::InvalidInput)?;
            if edge.from != self.winner
                && edge.to != self.winner
                && edge.from != self.loser
                && edge.to != self.loser
            {
                return Err(Error::InvalidInput(
                    "full merge input contains a non-incident edge".into(),
                ));
            }
            if !seen.insert((edge.from, edge.to)) {
                return Err(Error::InvalidInput(
                    "full merge input contains a duplicate edge key".into(),
                ));
            }
            ordered.push(edge.clone());
        }
        // `false < true`: exact winner rows establish destination semantics
        // before any loser-owned donor can collide with them.
        ordered.sort_by_key(|edge| {
            (
                edge.from == self.loser || edge.to == self.loser,
                edge.from,
                edge.to,
            )
        });

        let mut normalized: BTreeMap<(NodeId, NodeId), Edge> = BTreeMap::new();
        for edge in ordered {
            let rewritten = if edge.from == self.loser || edge.to == self.loser {
                edge.rewrite_for_full_merge(self.winner, self.loser)
            } else {
                Some(edge)
            };
            let Some(rewritten) = rewritten else {
                // Loser self-loops and winner/loser cross-edges would become a
                // new winner self-loop. A pre-existing winner self-loop entered
                // through the non-donor branch above and remains untouched.
                continue;
            };
            let key = (rewritten.from, rewritten.to);
            match normalized.get_mut(&key) {
                Some(existing) => existing.absorb_full_merge_donor(&rewritten),
                None => {
                    normalized.insert(key, rewritten);
                }
            }
        }

        if normalized.len() > MAX_INCIDENT_EDGES {
            return Err(Error::CapacityExceeded {
                resource: "incident edge degree",
                limit: MAX_INCIDENT_EDGES,
            });
        }
        debug_assert!(
            normalized
                .values()
                .all(|edge| { edge.from == self.winner || edge.to == self.winner })
        );
        Ok(normalized.into_values().collect())
    }

    /// Normalize both bounded source-owned remote sets. Duplicate targets retain
    /// the strongest already-observed weight, with canonical target ordering as
    /// the deterministic tie-break; no synthetic reinforcement is emitted.
    pub fn normalize_remote_edges(
        &self,
        winner_edges: &[RemoteEdge],
        loser_edges: &[RemoteEdge],
    ) -> Result<Vec<RemoteEdge>> {
        self.validate()?;
        if winner_edges.len() > MAX_REMOTE_EDGES_PER_SOURCE
            || loser_edges.len() > MAX_REMOTE_EDGES_PER_SOURCE
            || winner_edges.len().saturating_add(loser_edges.len()) > MAX_FULL_MERGE_REMOTE_EDGES
        {
            return Err(Error::CapacityExceeded {
                resource: "full merge remote edge read",
                limit: MAX_FULL_MERGE_REMOTE_EDGES,
            });
        }

        let mut normalized: BTreeMap<(Ulid, NodeId), RemoteEdge> = BTreeMap::new();
        for (expected_source, edges) in [(self.winner, winner_edges), (self.loser, loser_edges)] {
            let mut seen = HashSet::with_capacity(edges.len());
            for edge in edges {
                edge.validate().map_err(Error::InvalidInput)?;
                if edge.from != expected_source {
                    return Err(Error::InvalidInput(
                        "full merge remote input belongs to another source".into(),
                    ));
                }
                let key = (edge.target_db, edge.target);
                if !seen.insert(key) {
                    return Err(Error::InvalidInput(
                        "full merge input contains a duplicate remote edge key".into(),
                    ));
                }
                let rewritten =
                    RemoteEdge::new(self.winner, edge.target_db, edge.target, edge.weight());
                match normalized.get_mut(&key) {
                    Some(existing) if rewritten.weight() > existing.weight() => {
                        *existing = rewritten;
                    }
                    Some(_) => {}
                    None => {
                        normalized.insert(key, rewritten);
                    }
                }
            }
        }
        if normalized.len() > MAX_REMOTE_EDGES_PER_SOURCE {
            return Err(Error::CapacityExceeded {
                resource: "remote edges per source",
                limit: MAX_REMOTE_EDGES_PER_SOURCE,
            });
        }
        Ok(normalized.into_values().collect())
    }
}

/// Adapter result for [`GraphStore::commit_full_merge`]. A replay is a success
/// and performs no graph, lifecycle, or overlay writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullMergeCommitOutcome {
    Applied,
    AlreadyApplied,
}

/// One atomic ordered supersession. The adapter owns the deterministic graph
/// delta because reading and replacing the loser must share the same snapshot:
/// install the canonical `winner -> loser` edge, halve loser confidence once,
/// archive it, observe and resolve the contradiction, and write the retry proof.
/// The canonical pair identifies this one historical adjudication for its whole
/// lifetime; `applied_at` is audit data, not a new-operation generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupersedeCommit {
    pub winner: NodeId,
    pub loser: NodeId,
    pub applied_at: Timestamp,
}

impl SupersedeCommit {
    pub fn new(winner: NodeId, loser: NodeId, applied_at: Timestamp) -> Result<Self> {
        let value = Self {
            winner,
            loser,
            applied_at,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn pair(&self) -> UnorderedPair<NodeId> {
        UnorderedPair(self.winner, self.loser)
    }

    pub fn record(&self) -> SupersedeRecord {
        SupersedeRecord::new(self.winner, self.loser, self.applied_at)
    }

    pub fn validate(&self) -> Result<()> {
        self.record().validate().map_err(Error::InvalidInput)
    }
}

/// Adapter result for [`GraphStore::commit_supersede`]. A same-direction replay
/// for the pair is a successful no-op and cannot decay the loser again, even if
/// later legal mutations have changed its lifecycle or edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupersedeCommitOutcome {
    Applied,
    AlreadyApplied,
}

/// Maximum canonical rows one online maintenance transaction may inspect for
/// compare-and-swap mutation. Whole-store passes keyset-page around this bound
/// and release the engine mutation gate between commits.
pub const MAX_MAINTENANCE_BATCH_ROWS: usize = 64;

/// Canonical key of one directed edge, used as the stable keyset position for
/// maintenance scans and as the identity in commit outcomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaintenanceEdgeKey {
    pub from: NodeId,
    pub to: NodeId,
}

impl MaintenanceEdgeKey {
    pub const fn new(from: NodeId, to: NodeId) -> Self {
        Self { from, to }
    }

    pub const fn from_edge(edge: &Edge) -> Self {
        Self::new(edge.from, edge.to)
    }
}

/// Maximum raw rows admitted by one linked-scene incident-edge operation.
/// This is a physical page quantum, not a cap on relevant scenes.
pub const MAX_INCIDENT_EDGE_SCAN_ROWS: usize = 64;
/// Shared native linked-read ceiling. Every operation receives the remaining
/// positive allowance, never a freshly reset full-stage timeout.
pub const MAX_LINKED_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn validate_linked_read_timeout(remaining: std::time::Duration) -> Result<()> {
    if remaining.is_zero() || remaining > MAX_LINKED_READ_TIMEOUT {
        return Err(Error::InvalidInput(
            "linked-read remaining timeout must be positive and at most five seconds".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IncidentEdgeLeg {
    Outgoing,
    Incoming,
}

/// Internal anchor-bound dual-index keyset. Not a snapshot or a serialized public
/// cursor: both positions and fair next-leg choice survive a bounded continuation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncidentEdgesCursor {
    anchor: NodeId,
    outgoing_after: Option<MaintenanceEdgeKey>,
    incoming_after: Option<MaintenanceEdgeKey>,
    outgoing_done: bool,
    incoming_done: bool,
    next_leg: IncidentEdgeLeg,
}
impl IncidentEdgesCursor {
    pub const fn new(anchor: NodeId) -> Self {
        Self {
            anchor,
            outgoing_after: None,
            incoming_after: None,
            outgoing_done: false,
            incoming_done: false,
            next_leg: IncidentEdgeLeg::Outgoing,
        }
    }
    pub fn resume(
        anchor: NodeId,
        outgoing_after: Option<MaintenanceEdgeKey>,
        incoming_after: Option<MaintenanceEdgeKey>,
        outgoing_done: bool,
        incoming_done: bool,
        next_leg: IncidentEdgeLeg,
    ) -> Result<Self> {
        let cursor = Self {
            anchor,
            outgoing_after,
            incoming_after,
            outgoing_done,
            incoming_done,
            next_leg,
        };
        cursor.validate(anchor)?;
        Ok(cursor)
    }
    pub fn validate(&self, anchor: NodeId) -> Result<()> {
        if self.anchor != anchor
            || self.outgoing_after.is_some_and(|key| key.from != anchor)
            || self.incoming_after.is_some_and(|key| key.to != anchor)
        {
            return Err(Error::InvalidInput(
                "incident-edge cursor belongs to another anchor or index leg".into(),
            ));
        }
        Ok(())
    }
    pub const fn anchor(&self) -> NodeId {
        self.anchor
    }
    pub const fn outgoing_after(&self) -> Option<MaintenanceEdgeKey> {
        self.outgoing_after
    }
    pub const fn incoming_after(&self) -> Option<MaintenanceEdgeKey> {
        self.incoming_after
    }
    pub const fn outgoing_done(&self) -> bool {
        self.outgoing_done
    }
    pub const fn incoming_done(&self) -> bool {
        self.incoming_done
    }
    pub const fn next_leg(&self) -> IncidentEdgeLeg {
        self.next_leg
    }
    pub const fn is_complete(&self) -> bool {
        self.outgoing_done && self.incoming_done
    }
}

#[derive(Clone, Debug)]
pub struct IncidentEdgesRequest {
    anchor: NodeId,
    scan_rows: usize,
    remaining: std::time::Duration,
    after: Option<IncidentEdgesCursor>,
}
impl IncidentEdgesRequest {
    pub fn new(
        anchor: NodeId,
        scan_rows: usize,
        remaining: std::time::Duration,
        after: Option<IncidentEdgesCursor>,
    ) -> Result<Self> {
        let request = Self {
            anchor,
            scan_rows,
            remaining,
            after,
        };
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> Result<()> {
        if self.scan_rows == 0 || self.scan_rows > MAX_INCIDENT_EDGE_SCAN_ROWS {
            return Err(Error::InvalidInput(
                "incident-edge scan rows must be 1..=64".into(),
            ));
        }
        validate_linked_read_timeout(self.remaining)?;
        if let Some(cursor) = &self.after {
            cursor.validate(self.anchor)?;
            if cursor.is_complete() {
                return Err(Error::InvalidInput(
                    "incident-edge cursor is already exhausted".into(),
                ));
            }
        }
        Ok(())
    }
    pub const fn anchor(&self) -> NodeId {
        self.anchor
    }
    pub const fn scan_rows(&self) -> usize {
        self.scan_rows
    }
    pub const fn remaining(&self) -> std::time::Duration {
        self.remaining
    }
    pub fn after(&self) -> Option<&IncidentEdgesCursor> {
        self.after.as_ref()
    }
}

/// Observed physical read work, including duplicate rows and indexed empty
/// seeks. Canonical edge and body-span anchor point reads are distinct; endpoint
/// classification/hydration is excluded because this port never performs it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IncidentEdgeReadWork {
    pub rows_scanned: usize,
    pub indexed_seeks: usize,
    pub edge_point_reads: usize,
    pub body_anchor_point_reads: usize,
}
#[derive(Clone, Debug)]
pub struct IncidentEdgesPage {
    pub items: Vec<Edge>,
    /// None proves both physical index legs exhausted. An unknown tail after
    /// budget exhaustion retains the last dual-index cursor, even on short pages.
    pub next: Option<IncidentEdgesCursor>,
    pub work: IncidentEdgeReadWork,
}

/// One bounded canonical-node keyset page. `next` is the last returned key only
/// when another row exists at or below the pass's fixed upper bound.
#[derive(Clone, Debug)]
pub struct MaintenanceNodePage {
    pub items: Vec<Node>,
    pub next: Option<NodeId>,
}

/// Edge analogue of [`MaintenanceNodePage`]. Anchors are part of each returned
/// canonical edge and therefore part of a later exact CAS precondition.
#[derive(Clone, Debug)]
pub struct MaintenanceEdgePage {
    pub items: Vec<Edge>,
    pub next: Option<MaintenanceEdgeKey>,
}

/// One independently conditional edge update or deletion.
#[derive(Clone, Debug)]
pub enum MaintenanceEdgeMutation {
    Decay { expected: Edge, replacement: Edge },
    DeleteWeak { expected: Edge },
}

impl MaintenanceEdgeMutation {
    pub fn expected(&self) -> &Edge {
        match self {
            Self::Decay { expected, .. } | Self::DeleteWeak { expected } => expected,
        }
    }

    pub fn replacement(&self) -> Option<&Edge> {
        match self {
            Self::Decay { replacement, .. } => Some(replacement),
            Self::DeleteWeak { .. } => None,
        }
    }

    pub fn key(&self) -> MaintenanceEdgeKey {
        MaintenanceEdgeKey::from_edge(self.expected())
    }

    fn validate(&self) -> Result<()> {
        let expected = self.expected();
        expected.validate().map_err(Error::InvalidInput)?;
        match self {
            Self::Decay { replacement, .. } => {
                replacement.validate().map_err(Error::InvalidInput)?;
                if !expected.same_decay_static_fields(replacement)
                    || expected.interference() == 0
                    || replacement.interference() != 0
                    || replacement.weight() > expected.weight()
                {
                    return Err(Error::InvalidInput(
                        "invalid maintenance edge decay transition".into(),
                    ));
                }
            }
            Self::DeleteWeak { .. } if expected.trials() != 0 => {
                return Err(Error::InvalidInput(
                    "maintenance weak-edge deletion requires zero trials".into(),
                ));
            }
            Self::DeleteWeak { .. } => {}
        }
        Ok(())
    }
}

/// One bounded online maintenance transaction. Rows are independent CAS
/// operations: all matching rows commit atomically, stale rows are reported and
/// left untouched, and an adapter error publishes no row from this chunk.
#[derive(Clone, Debug, Default)]
pub struct MaintenanceCommit {
    pub edges: Vec<MaintenanceEdgeMutation>,
}

impl MaintenanceCommit {
    pub fn validate(&self) -> Result<()> {
        let rows = self.edges.len();
        if rows == 0 {
            return Err(Error::InvalidInput(
                "maintenance commit must contain at least one row".into(),
            ));
        }
        if rows > MAX_MAINTENANCE_BATCH_ROWS {
            return Err(Error::CapacityExceeded {
                resource: "maintenance batch rows",
                limit: MAX_MAINTENANCE_BATCH_ROWS,
            });
        }
        let mut edges = HashSet::with_capacity(self.edges.len());
        for mutation in &self.edges {
            mutation.validate()?;
            if !edges.insert(mutation.key()) {
                return Err(Error::InvalidInput(
                    "maintenance commit contains duplicate edge keys".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Exact rows applied from a maintenance commit. Any planned key absent here
/// conflicted with a newer canonical row (or disappeared) and is deferred to a
/// later pass.
#[derive(Clone, Debug, Default)]
pub struct MaintenanceCommitOutcome {
    pub applied_edges: Vec<MaintenanceEdgeKey>,
}

/// Result of one current-adjacency degree-prune transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DensePruneChunkOutcome {
    pub pruned: usize,
    pub remaining_excess: usize,
}

/// Storage of the graph itself: nodes, the hot-path edge relation, and the
/// cold-path contradiction overlay. Deliberately does NOT expose traversal —
/// that's [`Traversal`], a separate port, so a dumb store and a smart graph
/// engine can be different backends.
#[async_trait]
pub trait GraphStore: Send + Sync {
    /// Indexed semantic-only observed vocabulary. Never fallback to node scans.
    async fn tag_vocabulary_page(
        &self,
        request: &TagVocabularyRequest,
    ) -> Result<TagVocabularyPage> {
        request.validate()?;
        Err(Error::InvalidInput(
            "indexed tag vocabulary is unsupported by this graph store".into(),
        ))
    }
    /// Native authored meaning records; a tag alone never supplies this capability.
    fn touchstones(&self) -> Option<&dyn TouchstoneStore> {
        None
    }

    /// Optional advisory concerns; absence never falls back to notes/contradictions.
    fn concerns(&self) -> Option<&dyn ConcernStore> {
        None
    }
    /// Separate, optional episodic-memory lane; never fallback to semantic capture.
    fn episodes(&self) -> Option<&dyn EpisodeStore> {
        None
    }
    /// Optional bounded raw incident-edge read over physical outgoing/incoming
    /// indexes. Include semantic, episode and dangling endpoints without node
    /// hydration or lifecycle filtering. A physical self-edge may appear in both
    /// legs and is charged twice. Return None only when the adapter lacks this
    /// read capability, never as a fallback to semantic neighbors. Apply the
    /// remaining timeout to the complete operation, including lock/index/point
    /// reads; do not reset it per native subquery or scan over-budget lookahead.
    async fn incident_edges_page(
        &self,
        request: &IncidentEdgesRequest,
    ) -> Result<Option<IncidentEdgesPage>> {
        request.validate()?;
        Ok(None)
    }
    /// Atomically replace only a semantic node's body and Managed ownership.
    /// Compare the pointer revision and reject touchstone owners and all outgoing
    /// body anchors in the same write snapshot. Preserve every other field and
    /// projection; never reclaim the previous blob here.
    async fn compare_replace_node_body(
        &self,
        _id: NodeId,
        _expected: &crate::BodyRevision,
        _replacement: &crate::BodyRef,
    ) -> Result<Node> {
        Err(Error::InvalidInput(
            "body editing is unsupported by this graph store".into(),
        ))
    }
    /// Atomically replace a semantic summary and its vector/lexical projections.
    /// Compare the database-bound summary snapshot, reject episodes, touchstone
    /// owners and unchanged text in the write snapshot, and preserve every other
    /// canonical field, edge and projection. Historical-reference targets and
    /// outgoing body anchors are not edit blockers. The digest compares content,
    /// not a monotonic revision; it intentionally excludes bodies/tags/lifecycle.
    async fn compare_replace_node_summary(
        &self,
        _id: NodeId,
        _expected: &crate::SummarySnapshotDigest,
        _summary: &crate::NodeSummary,
        _embedding: &[f32],
    ) -> Result<Node> {
        Err(Error::InvalidInput(
            "summary editing is unsupported by this graph store".into(),
        ))
    }
    async fn put_node(&self, node: &Node) -> Result<()>;
    /// Verified cheap replay probe. `Some` proves both the canonical capture
    /// marker and vector are present; a foreign row, changed request, or torn
    /// projection is a conflict, never a successful replay.
    async fn lookup_capture(&self, _proof: &CaptureReplayProof) -> Result<Option<NodeId>> {
        Err(Error::InvalidInput(
            "atomic capture is unsupported by this graph store".into(),
        ))
    }
    /// Commit a captured node and its vector/projections as one durable unit.
    /// An existing identical source+request returns `AlreadyApplied`; a reused
    /// identity with different content or a foreign row is a conflict. Adapters
    /// must not expose a canonical row before its vector is committed.
    async fn commit_capture(
        &self,
        _node: &Node,
        _embedding: &[f32],
        _proof: &CaptureReplayProof,
    ) -> Result<CaptureCommitOutcome> {
        Err(Error::InvalidInput(
            "atomic capture is unsupported by this graph store".into(),
        ))
    }
    /// Atomically commit a capture and its bounded caller-declared initial links.
    /// Replay does not rewrite links after learning or deletion.
    async fn commit_capture_with_edges(
        &self,
        node: &Node,
        embedding: &[f32],
        edges: &[Edge],
        proof: &CaptureReplayProof,
    ) -> Result<CaptureCommitOutcome> {
        validate_capture_edges(node.id(), edges)?;
        if !edges.is_empty() {
            return Err(Error::InvalidInput(
                "atomic capture with edges is unsupported by this graph store".into(),
            ));
        }
        self.commit_capture(node, embedding, proof).await
    }
    async fn get_node(&self, id: NodeId) -> Result<Option<Node>>;
    /// Hydrate an explicit bounded identity list in one adapter operation.
    /// The result is positional: it has exactly `ids.len()` entries, preserves
    /// input order and duplicates, and represents a missing row as `None`.
    /// Inputs above [`crate::MAX_NODE_HYDRATION_BATCH`] are rejected rather than
    /// silently split, so one call remains one storage snapshot/query.
    async fn get_nodes(&self, ids: &[NodeId]) -> Result<Vec<Option<Node>>>;
    /// Read the counter-free lifecycle projection for an explicit bounded
    /// identity list without hydrating complete canonical nodes. The result is
    /// positional: it has exactly `ids.len()` entries, preserves input order
    /// and duplicates, and represents a missing or episodic row as `None`. This
    /// is a semantic-admission projection, not a raw inventory. Inputs above
    /// [`crate::MAX_NODE_STATUS_BATCH`] are rejected before storage work.
    async fn get_node_statuses(&self, ids: &[NodeId]) -> Result<Vec<Option<NodeLifecycle>>>;
    /// Remove a node (and its tags) outright. In the same store transaction,
    /// discard unresolved contradiction and merge-candidate overlays involving
    /// the node: they can no longer be adjudicated once an endpoint is gone.
    /// Terminal overlays and durable full-merge proofs are retained as history.
    /// The engine's `forget` also drops the node's edges and vector; bodies are
    /// external and untouched.
    async fn delete_node(&self, id: NodeId) -> Result<()>;
    /// Whole-graph scan — gated on the [`ColdPath`] for community detection
    /// and explicit curation. Never on the hot path.
    async fn all_nodes(&self, cold: ColdPath) -> Result<Vec<Node>>;
    async fn set_status(&self, id: NodeId, status: NodeStatus) -> Result<()>;
    /// Atomically replace tags on a semantic node only when its exact current
    /// tag set matches `expected`. This is not a revision token: ABA is not
    /// detected, and callers must inspect stale state rather than replay blindly.
    /// Episode editions and immutable touchstone authored fields remain protected.
    async fn compare_replace_node_tags(
        &self,
        _id: NodeId,
        _expected: &crate::BoundedTagSet,
        _replacement: &crate::BoundedTagSet,
    ) -> Result<Node> {
        Err(Error::InvalidInput(
            "atomic tag replacement is unsupported by this graph store".into(),
        ))
    }

    /// Same tag edit with target and guide meaning checked in the write snapshot.
    /// This must not downgrade to the tag-only operation. Canonical content guards
    /// bind pointers, not mutable external bytes; content ABA is not detected.
    async fn compare_replace_node_tags_guarded(
        &self,
        _id: NodeId,
        _expected: &crate::BoundedTagSet,
        _replacement: &crate::BoundedTagSet,
        guards: &RetagContentGuards,
    ) -> Result<Node> {
        guards.validate()?;
        Err(Error::InvalidInput(
            "content-guarded tag replacement is unsupported by this graph store".into(),
        ))
    }

    /// Largest canonical node key observed at pass start. Keys above this mark
    /// wait for a later pass. A concurrent insert at or below the mark may still
    /// be observed by a later page; callers must not treat this as a snapshot.
    async fn maintenance_node_upper_bound(&self, cold: ColdPath) -> Result<Option<NodeId>>;
    /// Read at most `limit` canonical nodes after `after` and no later than
    /// `through`, in ascending key order. `limit` must be in
    /// `1..=MAX_MAINTENANCE_BATCH_ROWS`.
    async fn maintenance_nodes_page(
        &self,
        cold: ColdPath,
        after: Option<NodeId>,
        through: NodeId,
        limit: usize,
    ) -> Result<MaintenanceNodePage>;

    /// First-write capture with optional, meaning-bound routing priors. Adapters
    /// admit priors under the same lock/transaction as the canonical write, after
    /// replay and strict authored-link validation. Skipped priors never invalidate
    /// the claim. Inspect at most MAX_CAPTURE_PRIOR_CANDIDATES nominees and retain
    /// at most the generated budget and remaining eight authored/generated slots.
    /// The only endpoint degree ceiling is MAX_INCIDENT_EDGES. Replay precedes
    /// optional batch-bound validation. Unsupported adapters retain required links only.
    async fn commit_capture_with_priors(
        &self,
        node: &Node,
        embedding: &[f32],
        authored: &[Edge],
        _priors: &[CaptureSimilarityPrior],
        _generated_link_budget: CapturePriorBudget,
        proof: &CaptureReplayProof,
    ) -> Result<CaptureCommitOutcome> {
        self.commit_capture_with_edges(node, embedding, authored, proof)
            .await
    }

    /// Atomic owner/vector/immutable summary snapshots/index publication. Exact
    /// source replay MUST precede target reads. First apply checks expected target
    /// projections under this same transaction; weighted priors remain independent.
    /// None preserves ordinary capture behavior. Unsupported adapters never save a
    /// tagged approximation or silently discard authored meaning metadata.
    async fn commit_capture_with_touchstone(
        &self,
        node: &Node,
        embedding: &[f32],
        authored: &[Edge],
        priors: &[CaptureSimilarityPrior],
        generated_link_budget: CapturePriorBudget,
        proof: &CaptureReplayProof,
        touchstone: Option<&crate::touchstone::TouchstoneInput>,
    ) -> Result<CaptureCommitOutcome> {
        match touchstone {
            Some(input) => {
                input.validate()?;
                Err(Error::InvalidInput(
                    "native touchstone capture is unsupported by this graph store".into(),
                ))
            }
            None => {
                if proof.source().request_codec() == crate::CaptureRequestCodec::TouchstoneV1 {
                    return Err(Error::InvalidInput(
                        "touchstone_v1 capture requires authored touchstone metadata".into(),
                    ));
                }
                self.commit_capture_with_priors(
                    node,
                    embedding,
                    authored,
                    priors,
                    generated_link_budget,
                    proof,
                )
                .await
            }
        }
    }

    /// Insert or replace one directed edge. A new pair that would take either
    /// endpoint above [`crate::MAX_INCIDENT_EDGES`] fails with
    /// [`Error::CapacityExceeded`]; replacing an existing pair remains legal at
    /// the limit, and a self-loop consumes one incident slot.
    async fn put_edge(&self, edge: &Edge) -> Result<()>;
    /// Remove the directed `from -> to` edge if present (a no-op otherwise).
    /// Used by the merge primitives to repoint a loser's edges onto the winner.
    async fn delete_edge(&self, from: NodeId, to: NodeId) -> Result<()>;
    /// Directed lookup by exact endpoints. Edges are directed and single-weight;
    /// the *association* is bidirectional via two possible directed edges. To
    /// reinforce, read the edge, call [`Edge::reinforce`], and `put_edge` it back
    /// — the store no longer owns the strength formula.
    async fn get_edge(&self, from: NodeId, to: NodeId) -> Result<Option<Edge>>;
    /// Whole-edge scan — cold path only (decay sweep, merge repointing, the
    /// consolidation pass).
    async fn all_edges(&self, cold: ColdPath) -> Result<Vec<Edge>>;
    async fn maintenance_edge_upper_bound(
        &self,
        cold: ColdPath,
    ) -> Result<Option<MaintenanceEdgeKey>>;
    async fn maintenance_edges_page(
        &self,
        cold: ColdPath,
        after: Option<MaintenanceEdgeKey>,
        through: MaintenanceEdgeKey,
        limit: usize,
    ) -> Result<MaintenanceEdgePage>;
    /// Top-k [`Neighbor`]s of `id` in weight-desc order, oriented by edge kind
    /// (see [`Neighbor`]). [`Traversal`] composes these; keeping it one-hop keeps
    /// the store backend-agnostic.
    async fn neighbors(&self, id: NodeId, top_k: usize) -> Result<Vec<Neighbor>>;

    /// Atomically compare-and-commit every planned node/edge replacement,
    /// store-owned merge-candidate observation, and, when present, its durable
    /// idempotency record. No prefix may become visible on error. A matching
    /// `(epoch, key, fingerprint)` record returns `AlreadyApplied`; the same
    /// `(epoch, key)` with another fingerprint is invalid input. A record for
    /// the same bare key in another epoch is unreachable old-generation state:
    /// it is atomically replaced only if the new graph commit succeeds.
    /// Adapters must also enforce the incident-edge ceiling against the
    /// transaction's final state.
    async fn commit_feedback(&self, commit: &FeedbackCommit) -> Result<FeedbackCommitOutcome>;

    /// Atomically collapse one open merge-candidate pair. The adapter checks its
    /// durable pair ledger before reading the now-mutated graph, then uses only
    /// bounded endpoint/source indexes to normalize local and remote edges. It
    /// archives the loser, resolves the candidate, and records the outcome in
    /// the same transaction. Same-direction retry returns `AlreadyApplied`;
    /// opposite-direction reuse or re-merging an archived loser fails closed.
    async fn commit_full_merge(&self, commit: &FullMergeCommit) -> Result<FullMergeCommitOutcome>;

    /// Atomically apply one directional supersession and its durable proof.
    /// The proof is checked before reading post-operation state. Same-direction
    /// replay returns `AlreadyApplied`; opposite-direction reuse fails closed.
    /// This is at-most-once historical adjudication, not a standing invariant:
    /// replay never reasserts state changed by a later legal mutation.
    async fn commit_supersede(&self, commit: &SupersedeCommit) -> Result<SupersedeCommitOutcome>;

    /// Apply one bounded set of independent exact-row maintenance mutations.
    /// Matching rows commit together; stale/missing rows are omitted from the
    /// outcome rather than aborting unrelated work in the same batch.
    async fn commit_maintenance(
        &self,
        cold: ColdPath,
        commit: &MaintenanceCommit,
    ) -> Result<MaintenanceCommitOutcome>;

    /// Return the subset of one bounded node page whose current incident
    /// association degree exceeds `target_degree`. `candidates` must contain at
    /// most [`MAX_MAINTENANCE_BATCH_ROWS`] IDs. This is a hint that avoids a
    /// write transaction for every sparse node; the pruning transaction still
    /// rereads current adjacency and is authoritative under races.
    async fn maintenance_overfull_hubs(
        &self,
        cold: ColdPath,
        candidates: &[NodeId],
        target_degree: usize,
    ) -> Result<Vec<NodeId>>;

    /// Read one hub's current bounded incident association set and delete at
    /// most `max_deletes` deterministic weakest rows in the same transaction.
    /// This operation must never act from an engine-owned stale degree snapshot.
    async fn prune_incident_associations(
        &self,
        cold: ColdPath,
        hub: NodeId,
        target_degree: usize,
        max_deletes: usize,
    ) -> Result<DensePruneChunkOutcome>;

    /// Cold-path overlay, keyed by canonical unordered pair. Never traversed
    /// during retrieval; consumed by reconciliation. Upserts: first observation
    /// creates the row, later ones bump the count. (Detection is model-driven and
    /// agent-supplied — see [`Contradiction`]; this is just the mechanism.)
    async fn observe_contradiction(&self, a: NodeId, b: NodeId, at: Timestamp) -> Result<()>;
    async fn open_contradictions(&self, cold: ColdPath) -> Result<Vec<Contradiction>>;
    /// Set a reconciliation verdict. `Unresolved` is an explicit open deferral
    /// and may later transition to `Superseded` or `ContextDependent`. Repeating
    /// the current verdict is an idempotent no-op; replacing either terminal
    /// verdict conflicts.
    async fn resolve_contradiction(
        &self,
        pair: UnorderedPair<NodeId>,
        resolution: Resolution,
    ) -> Result<()>;

    /// Cold-path overlay for redundancy, the exact analogue of the contradiction
    /// overlay but fed by [`Signal::NotNew`](crate::Signal) feedback. Upserts:
    /// first not-new observation creates the row, later ones bump the count that
    /// the merge pass triages on. Never traversed during retrieval.
    async fn observe_merge_candidate(&self, a: NodeId, b: NodeId, at: Timestamp) -> Result<()>;
    async fn open_merge_candidates(&self, cold: ColdPath) -> Result<Vec<MergeCandidate>>;
    async fn resolve_merge_candidate(
        &self,
        pair: UnorderedPair<NodeId>,
        resolution: MergeResolution,
    ) -> Result<()>;

    /// Cross-db edges stored in *this* db: a `from` node here pointing at a node
    /// in another db (by its stamped db-id). A separate overlay, out of the
    /// hot-path edge relation; the host resolves them opportunistically. `put`
    /// upserts on `(from, target_db, target)`. A new key that would take its
    /// local `from` above [`crate::MAX_REMOTE_EDGES_PER_SOURCE`] fails with
    /// [`Error::CapacityExceeded`]; replacing an exact key remains legal at the
    /// ceiling. Remote target fan-in is deliberately not part of this
    /// source-owned invariant.
    async fn put_remote_edge(&self, edge: &RemoteEdge) -> Result<()>;
    /// Read one keyset page in the domain's canonical order (weight descending,
    /// then target database and node id ascending). `limit` must be in
    /// `1..=MAX_REMOTE_EDGE_PAGE_SIZE`; adapters reject larger values instead
    /// of silently allocating an unpaged adjacency. The cursor is exact while
    /// the source is unchanged and best-effort across concurrent mutation.
    async fn remote_edges_page(
        &self,
        from: NodeId,
        after: Option<RemoteEdgeCursor>,
        limit: usize,
    ) -> Result<RemoteEdgePage>;
    async fn delete_remote_edge(&self, from: NodeId, target_db: Ulid, target: NodeId)
    -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureCommitOutcome {
    Applied,
    AlreadyApplied,
}

/// ANN over a sparse projection of canonical node-summary embeddings.
///
/// Not every canonical node must have a vector (an interrupted ingest or an
/// explicit re-embedding repair can leave a temporary gap), but every stored
/// vector must belong to an existing canonical node. Dimension is fixed at
/// construction: changing embedding provider to a different dimension means a
/// full vector-index rebuild. Stored and query vectors must contain only finite
/// `f32` values and have a strictly positive, finite `f32` squared L2 norm so
/// cosine arithmetic cannot yield NaN or infinity.
#[async_trait]
pub trait VectorIndex: Send + Sync {
    /// Stable semantic generation of this retrieval implementation. Change it
    /// whenever already-persisted vectors can produce a different candidate
    /// set or ordering under the same query and parameters. This is separate
    /// from [`EmbeddingFingerprint`], which identifies the vector space rather
    /// than ANN/index behavior.
    fn semantic_id(&self) -> &'static str;
    fn dim(&self) -> usize;
    /// Insert or replace one canonical node's projection. Returns
    /// [`Error::NotFound`] when `id` has no canonical node, [`Error::DimMismatch`]
    /// for the wrong dimension, and [`Error::InvalidInput`] for an invalid cosine
    /// vector.
    async fn upsert(&self, id: NodeId, embedding: &[f32]) -> Result<()>;
    /// Drop a node's vector (a no-op if absent) — for `forget`.
    async fn remove(&self, id: NodeId) -> Result<()>;
    /// k nearest by cosine similarity, restricted to the statuses `status`
    /// admits. This primitive is deliberately untagged: bounded tagged work and
    /// its omission metadata live in [`Self::tagged_ann`].
    /// Scores are similarities in `[0, 1]` for non-negative embeddings. A query
    /// with the wrong dimension or invalid cosine norm fails with the same error
    /// classes as [`Self::upsert`]; adapters must not silently feed it to ANN.
    async fn ann(&self, query: &[f32], k: usize, status: StatusFilter) -> Result<Vec<Scored>>;

    /// Current verified tag-projection generation supported by this adapter.
    /// `None` means [`Self::tagged_ann`] is unsupported; callers must fail
    /// closed rather than interpreting that absence as empty/exact retrieval.
    fn tagged_projection_generation(&self) -> Option<&'static str> {
        None
    }

    /// Multi-lane bounded tagged seed retrieval.
    ///
    /// Adapters that support tagged retrieval **must override this method** and
    /// validate both the request before storage work and their batch with
    /// [`TaggedAnnBatch::validate_against`] before returning. The fail-closed
    /// default is only an incremental compatibility seam: lack of support is
    /// never represented as an empty or exact result.
    async fn tagged_ann(&self, request: TaggedAnnRequest<'_>) -> Result<TaggedAnnBatch> {
        request.validate()?;
        Err(Error::InvalidInput(
            "tagged ANN is not supported by this vector index (no verified tag projection generation is declared)"
                .into(),
        ))
    }
}

/// Sparse full-text retrieval over node summaries.
///
/// Unlike [`VectorIndex`], lexical scores have no cross-backend numeric
/// contract: BM25 implementations and native full-text engines use different
/// scales. Callers must consume the returned **ranking**, not blend the raw
/// values with cosine similarity. Results are restricted to `status` inside the
/// index so an ineligible lifecycle tier cannot starve eligible hits in a
/// post-filtered top-k window.
#[async_trait]
pub trait LexicalIndex: Send + Sync {
    /// Stable semantic generation of corpus selection, scoring, and ordering.
    /// Physical posting-format changes that preserve results need not change
    /// this value; scoring changes over unchanged postings do.
    fn semantic_id(&self) -> &'static str;
    async fn search(&self, query: &str, k: usize, status: StatusFilter) -> Result<Vec<Scored>>;
}

/// Model-agnostic. Local (a hashing/bag-of-words embedder, fastembed) or remote,
/// configured at the edge.
#[async_trait]
pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    /// Stable identity of the full vector-space contract. Hosts persist and
    /// compare this before the embedder is allowed to read or mutate an index.
    fn fingerprint(&self) -> EmbeddingFingerprint;
    /// Embed documents/passages — the *stored* side of the index. No instruction
    /// prefix; node summaries are embedded through here.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    /// Embed a search *query*. Asymmetric-retrieval models (BGE, GTE, E5) are
    /// trained with a query instruction the passage side omits, so an adapter that
    /// has one applies it here — the query and the documents it's compared against
    /// are then embedded under the conventions the model expects, which measurably
    /// lifts recall. The default treats a query like a document, which is correct
    /// for symmetric and lexical embedders.
    async fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        self.embed(&[query])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::Backend("embedder returned no query vector".into()))
    }
}

/// Metadata required to prove that a persisted vector index and a runtime
/// [`Embedder`] use the same vector space. This is deliberately separate from
/// [`VectorIndex`]: ANN backends need not know how hosts persist control-plane
/// metadata, while stores that do can implement both ports.
pub trait EmbeddingMetadataStore: Send + Sync {
    fn embedding_fingerprint(&self) -> Result<Option<EmbeddingFingerprint>>;

    /// Persist an identity only after a complete rebuild, or while the store is
    /// known to be empty. Implementations should overwrite atomically where their
    /// backend permits it.
    fn set_embedding_fingerprint(&self, fingerprint: &EmbeddingFingerprint) -> Result<()>;

    /// True when nodes or vectors exist. A missing fingerprint is safe to adopt
    /// only when this is false; otherwise the store is legacy/partially rebuilt.
    fn has_embedding_data(&self) -> Result<bool>;

    /// Validate the runtime contract, initializing metadata only for a genuinely
    /// empty store. Populated legacy stores and mismatches fail closed.
    fn ensure_embedding_fingerprint(
        &self,
        runtime: &EmbeddingFingerprint,
    ) -> Result<EmbeddingFingerprintInit> {
        runtime
            .validate()
            .map_err(Error::InvalidEmbeddingFingerprint)?;
        match self.embedding_fingerprint()? {
            Some(stored) => {
                stored
                    .validate()
                    .map_err(Error::InvalidEmbeddingFingerprint)?;
                if stored == *runtime {
                    Ok(EmbeddingFingerprintInit::Existing)
                } else {
                    Err(Error::EmbeddingFingerprintMismatch {
                        stored: Box::new(stored),
                        runtime: Box::new(runtime.clone()),
                    })
                }
            }
            None if self.has_embedding_data()? => Err(Error::LegacyEmbeddingFingerprint),
            None => {
                self.set_embedding_fingerprint(runtime)?;
                Ok(EmbeddingFingerprintInit::InitializedEmptyStore)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddingFingerprintInit {
    Existing,
    InitializedEmptyStore,
}

/// Optional cross-encoder reranker. Where an [`Embedder`] encodes query and
/// document *independently* (a bi-encoder), a reranker scores a `(query, document)`
/// pair **jointly**, catching relevance the embedding misses. It's model inference,
/// so it runs as an optional **post-spread** stage over the already-bounded
/// candidate set — never per-node during traversal — which is why the engine stays
/// model-free when none is wired.
#[async_trait]
pub trait Reranker: Send + Sync {
    /// Stable identity of the model plus preprocessing/scoring contract.
    fn semantic_id(&self) -> &'static str;
    /// A relevance score per document, **in input order** (higher = more relevant).
    async fn rerank(&self, query: &str, docs: &[&str]) -> Result<Vec<f32>>;
}

/// One bounded, byte-addressed window of a resolved body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyChunk {
    /// Raw source bytes. Ranges are byte-addressed and deliberately make no
    /// UTF-8 claim.
    pub bytes: Vec<u8>,
    /// Requested source offset, including for an empty range at/past EOF.
    pub source_start: u64,
    /// Exclusive source offset after the returned bytes. Always
    /// `source_start + bytes.len()` with checked arithmetic.
    pub source_end: u64,
    /// `Some(source_end)` iff at least one source byte follows this chunk.
    /// With `max_bytes == 0` before EOF this is `Some(source_start)`: callers
    /// must increase their limit to make progress.
    pub next_offset: Option<u64>,
}

/// Body resolution, dyn-dispatched by URI scheme. The node row holds a
/// [`BodyRef`]; resolution is pluggable so bodies can live inline, on disk, or
/// behind the network without the domain caring.
#[async_trait]
pub trait BodyStore: Send + Sync {
    fn scheme(&self) -> &'static str;
    async fn get(&self, body: &BodyRef) -> Result<Vec<u8>>;
    /// Read raw bytes from `[offset, offset + max_bytes)`, clamped at EOF.
    /// Implementations must bound source I/O and allocation directly; this is a
    /// required method and must never be emulated by loading the full body.
    /// Reading at most one extra byte to determine `next_offset` is permitted.
    /// Limits whose continuation probe or returned source offset cannot be
    /// represented must fail rather than wrap or silently narrow.
    async fn get_range(&self, body: &BodyRef, offset: u64, max_bytes: usize) -> Result<BodyChunk>;
    /// Compatibility wrapper for callers that need only offset zero. It
    /// delegates to the required bounded range primitive, never to full `get`.
    async fn get_prefix(&self, body: &BodyRef, max_bytes: usize) -> Result<(Vec<u8>, bool)> {
        let chunk = self.get_range(body, 0, max_bytes).await?;
        let truncated = chunk.next_offset.is_some();
        Ok((chunk.bytes, truncated))
    }
    async fn put(&self, bytes: &[u8]) -> Result<BodyRef>;
    /// Idempotently erase a body owned by this store. A memory system's explicit
    /// `forget` operation must not leave the most sensitive payload behind while
    /// deleting only its index entry.
    async fn delete(&self, body: &BodyRef) -> Result<()>;
}

/// Canonical compact JSON bytes for the complete existing wire hint array,
/// including each database ID. This bounds advisory validation work, not relevance.
pub const MAX_ROUTING_HINT_BYTES: usize = 16 * 1024;
/// The closed wire shape has fixed-size ULIDs and lowercase SHA256 fingerprints.
/// Boost is one byte shorter than weaken. Tests pin these costs to serde JSON.
pub const MIN_ROUTING_HINT_WIRE_BYTES: usize = 475;
pub const MAX_ROUTING_HINT_WIRE_BYTES: usize = 476;
/// Gross parser/work guard derived from the byte fuse and minimum legal wire row.
/// `n` rows cost at least `1 + n * (MIN_ROUTING_HINT_WIRE_BYTES + 1)` bytes.
/// This is not a separate hint or historical-witness quota.
pub const MAX_ROUTING_HINTS: usize =
    (MAX_ROUTING_HINT_BYTES - 1) / (MIN_ROUTING_HINT_WIRE_BYTES + 1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingRoute {
    pub previous: NodeId,
    pub target: NodeId,
    #[serde(rename = "from")]
    pub edge_from: NodeId,
    #[serde(rename = "to")]
    pub edge_to: NodeId,
}

impl RoutingRoute {
    pub fn from_neighbor(previous: NodeId, neighbor: &Neighbor) -> Self {
        Self {
            previous,
            target: neighbor.node,
            edge_from: neighbor.edge.from,
            edge_to: neighbor.edge.to,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignedRoutingBias {
    Boost,
    Weaken,
}
impl SignedRoutingBias {
    pub fn value(self) -> f32 {
        match self {
            Self::Boost => 0.25,
            Self::Weaken => -0.25,
        }
    }
}
pub type RoutingBiasMap = std::collections::HashMap<RoutingRoute, SignedRoutingBias>;

/// Native opaque semantic binding. Excludes use/lifecycle telemetry; body *reference*
/// is bound, not mutable externally owned body bytes or a unique edge incarnation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingBinding {
    pub previous: NodeId,
    pub target: NodeId,
    #[serde(rename = "from")]
    pub edge_from: NodeId,
    #[serde(rename = "to")]
    pub edge_to: NodeId,
    pub previous_fingerprint: String,
    pub target_fingerprint: String,
    pub edge_fingerprint: String,
}
impl RoutingBinding {
    /// Shared structural check only; current meaning/edge validation stays native.
    pub fn has_canonical_fingerprints(&self) -> bool {
        [
            &self.previous_fingerprint,
            &self.target_fingerprint,
            &self.edge_fingerprint,
        ]
        .into_iter()
        .all(|fp| {
            fp.len() == 64
                && fp
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    }

    pub fn route(&self) -> RoutingRoute {
        RoutingRoute {
            previous: self.previous,
            target: self.target,
            edge_from: self.edge_from,
            edge_to: self.edge_to,
        }
    }
    pub fn new(previous: &Node, target: &Node, edge: &Edge) -> Self {
        Self {
            previous: previous.id(),
            target: target.id(),
            edge_from: edge.from,
            edge_to: edge.to,
            previous_fingerprint: routing_content_fingerprint(previous),
            target_fingerprint: routing_content_fingerprint(target),
            edge_fingerprint: routing_edge_fingerprint(edge),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingHint {
    pub route: RoutingBinding,
    pub sign: SignedRoutingBias,
}

/// Measure the full canonical wire row, including its fixed-width database ID.
/// Typed callers cannot bypass the public byte fuse using malformed/escaped
/// fingerprint strings. No JSON dependency, allocation, graph GET or body read.
/// `None` means the row alone exceeds the complete array allowance.
pub fn routing_hint_wire_bytes(hint: &RoutingHint) -> Option<usize> {
    let mut bytes = MAX_ROUTING_HINT_WIRE_BYTES - 3 * 64;
    if hint.sign == SignedRoutingBias::Boost {
        bytes -= 1;
    }
    for fp in [
        &hint.route.previous_fingerprint,
        &hint.route.target_fingerprint,
        &hint.route.edge_fingerprint,
    ] {
        for byte in fp.bytes() {
            bytes += match byte {
                b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
                0..=31 => 6,
                _ => 1,
            };
            if bytes > MAX_ROUTING_HINT_BYTES {
                return None;
            }
        }
    }
    Some(bytes)
}

/// Whole-array admission before canonical node/edge validation. Never truncate
/// to a signed prefix. Includes malformed typed rows' actual JSON string costs;
/// structural rejection is checked separately by whole-batch admission.
pub fn routing_hints_within_wire_budget(hints: &[RoutingHint]) -> bool {
    if hints.len() > MAX_ROUTING_HINTS {
        return false;
    }
    let mut bytes = 2 + hints.len().saturating_sub(1);
    for hint in hints {
        let Some(row_bytes) = routing_hint_wire_bytes(hint) else {
            return false;
        };
        bytes += row_bytes;
        if bytes > MAX_ROUTING_HINT_BYTES {
            return false;
        }
    }
    true
}

/// Shared native typed admission. A malformed row neutralizes the whole batch
/// before meaning/edge GETs; dropping an opposing row could invent unanimity.
/// Staleness, duplicate route neutrality and database selection remain separate.
pub fn routing_hints_admissible(hints: &[RoutingHint]) -> bool {
    routing_hints_within_wire_budget(hints)
        && hints
            .iter()
            .all(|hint| hint.route.has_canonical_fingerprints())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoutingDiagnostics {
    pub validated: usize,
    pub ignored: usize,
}
#[derive(Clone, Debug, Default)]
pub struct RoutingOutcome {
    pub diagnostics: RoutingDiagnostics,
    /// Actual graph-arrival bindings; never conditional recommendations.
    pub bindings: BTreeMap<NodeId, RoutingBinding>,
    /// Exact current positive-route bindings for conditional-winning cards.
    /// These authorize recommendation provenance, not a manufactured walk.
    pub conditional_bindings: BTreeMap<NodeId, RoutingBinding>,
}

/// Stable domain identifier for the canonical meaning codec. Historical v1
/// advisory bindings are intentionally not rebound to this codec.
pub const ROUTING_CONTENT_FINGERPRINT_CODEC: &str = "mneme.routing-content.v2";
/// Stable domain identifier for the canonical structural edge codec.
pub const ROUTING_EDGE_FINGERPRINT_CODEC: &str = "mneme.routing-edge.v2";

/// Explicit byte contract, independent of Debug, serde, enum declaration order,
/// Unicode tables, host word width and endianness. Integer payloads are big-endian:
/// IDs/timestamps u128, collection/string lengths u64, revisions/turns/spans/f32
/// bits u32. Strings are length-framed raw UTF-8, without escaping/normalization.
/// Options use 0=None, 1=Some; enum tags below are fixed codec constants.
#[derive(Default)]
struct RoutingFingerprintBytes(Vec<u8>);
impl RoutingFingerprintBytes {
    fn tag(&mut self, tag: u8) {
        self.0.push(tag);
    }
    fn raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
    fn u32(&mut self, value: u32) {
        self.raw(&value.to_be_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.raw(&value.to_be_bytes());
    }
    fn u128(&mut self, value: u128) {
        self.raw(&value.to_be_bytes());
    }
    fn part(&mut self, bytes: &[u8]) {
        self.u64(bytes.len() as u64);
        self.raw(bytes);
    }
    fn text(&mut self, text: &str) {
        self.part(text.as_bytes());
    }
    fn optional_text(&mut self, value: Option<&str>) {
        match value {
            None => self.tag(0),
            Some(value) => {
                self.tag(1);
                self.text(value);
            }
        }
    }
    fn optional_id(&mut self, value: Option<NodeId>) {
        match value {
            None => self.tag(0),
            Some(value) => {
                self.tag(1);
                self.u128(value.0.0);
            }
        }
    }
}

fn routing_content_bytes(node: &Node) -> Vec<u8> {
    let mut bytes = RoutingFingerprintBytes::default();
    bytes.text(ROUTING_CONTENT_FINGERPRINT_CODEC);
    // Fixed outer field order: identity, kind/facet, summary, body, tags,
    // provenance, origin commit. Node status, ownership, clocks and telemetry
    // are excluded; episode authored occurrence/recording anchors remain meaning.
    bytes.u128(node.id().0.0);
    match node.memory_kind() {
        crate::MemoryKind::Semantic => bytes.tag(0),
        crate::MemoryKind::Episode(facet) => {
            bytes.tag(1);
            bytes.u128(facet.root().node_id().0.0);
            bytes.u32(facet.revision().get());
            bytes.optional_id(facet.revises());
            match facet.occurrence() {
                crate::OccurrenceSpan::Unknown => bytes.tag(0),
                crate::OccurrenceSpan::Point { at } => {
                    bytes.tag(1);
                    bytes.u128(at.get());
                }
                crate::OccurrenceSpan::Range { start, end } => {
                    bytes.tag(2);
                    bytes.u128(start.get());
                    bytes.u128(end.get());
                }
            }
            bytes.optional_text(facet.thread().map(crate::EpisodeThread::as_str));
            bytes.u128(facet.recorded_at().get());
            bytes.optional_text(
                facet
                    .edit_reason()
                    .map(crate::EpisodeRevisionReason::as_str),
            );
            match facet.occurrence_contexts() {
                None => bytes.tag(0),
                Some(contexts) => {
                    bytes.tag(1);
                    bytes.u64(contexts.as_slice().len() as u64);
                    // The private collection has canonical namespace/key order.
                    for context in contexts.iter() {
                        bytes.text(context.namespace());
                        bytes.text(context.key());
                        bytes.optional_text(context.label());
                    }
                }
            }
        }
    }
    bytes.text(node.summary());
    bytes.text(node.body().as_str());
    let mut tags: Vec<_> = node.tags().collect();
    tags.sort_unstable();
    bytes.u64(tags.len() as u64);
    for tag in tags {
        bytes.text(tag);
    }
    match node.provenance() {
        crate::Provenance::Web { url, fetched } => {
            bytes.tag(1);
            bytes.text(url.as_str());
            bytes.u128(*fetched);
        }
        crate::Provenance::Conversation { session, turn } => {
            bytes.tag(2);
            bytes.u128(session.0);
            bytes.u32(*turn);
        }
        crate::Provenance::External { source } => {
            bytes.tag(3);
            bytes.text(source.namespace());
            bytes.text(source.key());
            bytes.text(source.reference());
            bytes.optional_text(source.session());
            bytes.optional_text(source.revision());
            bytes.raw(&source.request_digest());
            bytes.tag(match source.request_codec() {
                crate::CaptureRequestCodec::CaptureV1 => 1,
                crate::CaptureRequestCodec::CaptureV2 => 2,
                crate::CaptureRequestCodec::EpisodeV1 => 3,
                crate::CaptureRequestCodec::EpisodeV2 => 4,
                crate::CaptureRequestCodec::TouchstoneV1 => 5,
            });
        }
        crate::Provenance::Derived { from } => {
            bytes.tag(4);
            bytes.u64(from.len() as u64);
            // Unlike tags/contexts, historical derived-source order is authored
            // data. Retain it exactly, not an inferred unordered set.
            for source in from.iter() {
                bytes.u128(source.0.0);
            }
        }
    }
    match node.origin_commit() {
        None => bytes.tag(0),
        Some(commit) => {
            bytes.tag(1);
            match commit {
                crate::OriginCommit::Sha1(digest) => {
                    bytes.tag(1);
                    bytes.raw(&digest);
                }
                crate::OriginCommit::Sha256(digest) => {
                    bytes.tag(2);
                    bytes.raw(&digest);
                }
            }
        }
    }
    bytes.0
}

pub fn routing_content_fingerprint(node: &Node) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(routing_content_bytes(node)))
}

fn routing_edge_bytes(edge: &Edge) -> Vec<u8> {
    let mut bytes = RoutingFingerprintBytes::default();
    bytes.text(ROUTING_EDGE_FINGERPRINT_CODEC);
    bytes.u128(edge.from.0.0);
    bytes.u128(edge.to.0.0);
    bytes.tag(match edge.kind {
        EdgeKind::Associative => 1,
        EdgeKind::Bridge => 2,
        EdgeKind::Transition => 3,
        EdgeKind::Supersedes => 4,
        EdgeKind::DerivedFrom => 5,
    });
    match edge.anchor {
        None => bytes.tag(0),
        Some(anchor) => {
            bytes.tag(1);
            bytes.u32(anchor.start);
            bytes.u32(anchor.end);
        }
    }
    // Exact IEEE-754 bits, not decimal formatting. Reinforcement timestamp,
    // trial count and interference remain excluded, as in the meaning contract.
    bytes.u32(edge.weight().to_bits());
    bytes.0
}

pub fn routing_edge_fingerprint(edge: &Edge) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(routing_edge_bytes(edge)))
}

/// One edge actually used while proposing a winning spreading activation.
/// `edge` preserves its stored arrow and static semantics even for incoming hops.
/// This is mechanical provenance, not evidence that the association was helpful.
#[derive(Clone, Debug)]
pub struct TraversalHop {
    pub previous: NodeId,
    pub target: NodeId,
    pub edge: Edge,
}

/// Transient final-hop presentation priority. This never becomes propagation
/// activation or authored weight. The real path may differ from the unchanged
/// original-score winning path when another arrival supplies better priority.
#[derive(Clone, Debug)]
pub struct TraversalOrdering {
    pub priority: f32,
    pub original_contribution: f32,
    pub path: Vec<TraversalHop>,
}

/// Read-only spread output with optional, query-local provenance. `None` means
/// the adapter does not trace. A supported trace contains winning non-root paths;
/// each path is captured during propagation, never reconstructed from later state.
#[derive(Clone, Debug)]
pub struct SpreadResult {
    pub hits: Vec<Scored>,
    pub paths: Option<BTreeMap<NodeId, Vec<TraversalHop>>>,
    pub ordering: Option<BTreeMap<NodeId, TraversalOrdering>>,
}

/// The semantic traversal port. Implemented app-side over the store's one-hop
/// `neighbors` (the reference adapter does this), or pushed down into a backend
/// with native PageRank / community detection / Datalog fixpoint. The domain
/// calls THIS, never the underlying query language.
#[async_trait]
pub trait Traversal: Send + Sync {
    /// Spreading activation from scored seeds. Seed `score` is the initial
    /// activation; it propagates along edges, attenuated by edge weight, until
    /// it drops below `budget.min_relevance` or the budget is spent. When `query`
    /// (the embedded query) is supplied and `budget.query_conditioning > 0`, the
    /// activation reaching each node is additionally damped by that node's semantic
    /// distance from the query, so the spread is pulled *toward* it. `scope` is
    /// enforced while traversing, not merely on the returned list: excluded nodes
    /// cannot surface, consume fan-out, or act as hidden routing intermediates.
    async fn spread(
        &self,
        seeds: &[Scored],
        budget: Budget,
        query: Option<&[f32]>,
        scope: TraversalScope,
    ) -> Result<Vec<Scored>>;

    /// Opt-in shadow provenance. Existing adapters remain valid but explicitly
    /// report that provenance is unavailable rather than fabricating routes.
    async fn spread_with_provenance(
        &self,
        seeds: &[Scored],
        budget: Budget,
        query: Option<&[f32]>,
        scope: TraversalScope,
    ) -> Result<SpreadResult> {
        Ok(SpreadResult {
            hits: self.spread(seeds, budget, query, scope).await?,
            paths: None,
            ordering: None,
        })
    }
    /// Explicit per-call ordering seam. Unsupported adapters preserve baseline.
    /// Biases never enter propagated scores or stored/provenance edge values.
    async fn spread_routed(
        &self,
        seeds: &[Scored],
        budget: Budget,
        query: Option<&[f32]>,
        scope: TraversalScope,
        _biases: &RoutingBiasMap,
        trace: bool,
    ) -> Result<SpreadResult> {
        if trace {
            self.spread_with_provenance(seeds, budget, query, scope)
                .await
        } else {
            Ok(SpreadResult {
                hits: self.spread(seeds, budget, query, scope).await?,
                paths: None,
                ordering: None,
            })
        }
    }

    /// Re-run community detection; returns a cluster label per node. Whole-graph
    /// work — cold path only.
    async fn detect_communities(&self, cold: ColdPath) -> Result<Vec<(NodeId, ClusterId)>>;
}

/// Injected, not called directly — makes decay testable and keeps the domain
/// free of wall-clock reads.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// The production clock: unix epoch millis from the system time. Tests use a
/// fake clock that returns a controlled value instead.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is before the unix epoch")
            .as_millis()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EdgeKind;

    fn edge(trials: u32, interference: u32) -> Edge {
        Edge::from_stored(
            NodeId(Ulid::from(1_u128)),
            NodeId(Ulid::from(2_u128)),
            EdgeKind::Transition,
            None,
            0.5,
            1,
            trials,
            interference,
        )
    }

    fn feedback_edge_commit(expected: Option<Edge>, replacement: Edge) -> FeedbackCommit {
        FeedbackCommit {
            idempotency: None,
            applied_at: 1,
            nodes: Vec::new(),
            edges: vec![FeedbackEdgeUpdate {
                expected,
                replacement,
            }],
            merge_observations: Vec::new(),
        }
    }

    #[test]
    fn linked_read_requests_validate_work_time_and_anchor_before_admission() {
        use std::time::Duration;
        let anchor = NodeId(Ulid(1));
        for rows in [0, MAX_INCIDENT_EDGE_SCAN_ROWS + 1, usize::MAX] {
            assert!(IncidentEdgesRequest::new(anchor, rows, Duration::from_secs(1), None).is_err());
        }
        for remaining in [
            Duration::ZERO,
            MAX_LINKED_READ_TIMEOUT + Duration::from_nanos(1),
        ] {
            assert!(IncidentEdgesRequest::new(anchor, 1, remaining, None).is_err());
            assert!(crate::EpisodeHeaderByEditionRequest::new(anchor, remaining).is_err());
        }
        for remaining in [Duration::from_nanos(1), MAX_LINKED_READ_TIMEOUT] {
            let request = IncidentEdgesRequest::new(anchor, 1, remaining, None).unwrap();
            assert_eq!(request.anchor(), anchor);
            assert_eq!(request.scan_rows(), 1);
            assert_eq!(request.remaining(), remaining);
            assert!(request.after().is_none());
            let header = crate::EpisodeHeaderByEditionRequest::new(anchor, remaining).unwrap();
            assert_eq!(header.edition_id(), anchor);
            assert_eq!(header.remaining(), remaining);
        }
        assert!(
            IncidentEdgesRequest::new(
                anchor,
                MAX_INCIDENT_EDGE_SCAN_ROWS,
                MAX_LINKED_READ_TIMEOUT,
                None
            )
            .is_ok()
        );
        let cursor = IncidentEdgesCursor::new(anchor);
        assert_eq!(cursor.next_leg(), IncidentEdgeLeg::Outgoing);
        assert!(!cursor.is_complete());
        assert!(
            IncidentEdgesRequest::new(NodeId(Ulid(2)), 1, Duration::from_secs(1), Some(cursor))
                .is_err()
        );
    }

    #[test]
    fn incident_cursor_retains_both_positions_and_fair_leg_without_restarting() {
        let anchor = NodeId(Ulid(1));
        let other = NodeId(Ulid(2));
        let outgoing = MaintenanceEdgeKey::new(anchor, other);
        let incoming = MaintenanceEdgeKey::new(other, anchor);
        for (outgoing_after, incoming_after) in [(Some(incoming), None), (None, Some(outgoing))] {
            assert!(
                IncidentEdgesCursor::resume(
                    anchor,
                    outgoing_after,
                    incoming_after,
                    false,
                    false,
                    IncidentEdgeLeg::Incoming
                )
                .is_err()
            );
        }
        let cursor = IncidentEdgesCursor::resume(
            anchor,
            Some(outgoing),
            Some(incoming),
            true,
            false,
            IncidentEdgeLeg::Incoming,
        )
        .unwrap();
        assert_eq!(cursor.anchor(), anchor);
        assert_eq!(cursor.outgoing_after(), Some(outgoing));
        assert_eq!(cursor.incoming_after(), Some(incoming));
        assert!(cursor.outgoing_done());
        assert!(!cursor.incoming_done());
        assert_eq!(cursor.next_leg(), IncidentEdgeLeg::Incoming);
        assert!(!cursor.is_complete());
        let request =
            IncidentEdgesRequest::new(anchor, 1, MAX_LINKED_READ_TIMEOUT, Some(cursor.clone()))
                .unwrap();
        assert_eq!(request.after(), Some(&cursor));
        let exhausted = IncidentEdgesCursor::resume(
            anchor,
            Some(outgoing),
            Some(incoming),
            true,
            true,
            IncidentEdgeLeg::Outgoing,
        )
        .unwrap();
        assert!(exhausted.is_complete());
        assert!(
            IncidentEdgesRequest::new(anchor, 1, MAX_LINKED_READ_TIMEOUT, Some(exhausted)).is_err()
        );
        let self_edge = MaintenanceEdgeKey::new(anchor, anchor);
        assert!(
            IncidentEdgesCursor::resume(
                anchor,
                Some(self_edge),
                Some(self_edge),
                false,
                false,
                IncidentEdgeLeg::Outgoing
            )
            .is_ok()
        );
    }

    #[test]
    fn feedback_commit_rejects_invalid_replacement_edge_counters() {
        let error = feedback_edge_commit(None, edge(1, 2))
            .validate()
            .unwrap_err();

        assert!(matches!(
            error,
            Error::InvalidInput(message)
                if message == "feedback edge replacement is invalid: edge interference must not exceed trials"
        ));
    }

    #[test]
    fn feedback_commit_rejects_invalid_expected_edge_counters() {
        let error = feedback_edge_commit(Some(edge(1, 2)), edge(2, 1))
            .validate()
            .unwrap_err();

        assert!(matches!(
            error,
            Error::InvalidInput(message)
                if message == "feedback edge precondition is invalid: edge interference must not exceed trials"
        ));
    }
}

#[cfg(test)]
mod routing_binding_tests {
    use super::*;
    fn node(id: u128, summary: &str, now: Timestamp) -> Node {
        Node::try_new(
            NodeId(Ulid::from(id)),
            summary,
            BodyRef::new("inline://same").unwrap(),
            ["scope"],
            crate::Provenance::derived_empty(),
            1.0,
            1.0,
            NodeStatus::Active,
            now,
        )
        .unwrap()
    }
    fn wire_hint(hint: &RoutingHint) -> serde_json::Value {
        serde_json::json!({"db_id":Ulid::from(7u128),"route":hint.route,"sign":hint.sign})
    }
    fn hint() -> RoutingHint {
        let previous = node(1, "first", 1);
        let target = node(2, "second", 1);
        let edge = Edge::new(previous.id(), target.id(), 0.6, EdgeKind::Transition, 1);
        RoutingHint {
            route: RoutingBinding::new(&previous, &target, &edge),
            sign: SignedRoutingBias::Boost,
        }
    }
    #[test]
    fn routing_wire_byte_contract_matches_closed_serializer_and_derived_guard() {
        let mut h = hint();
        assert!(h.route.has_canonical_fingerprints());
        assert_eq!(
            serde_json::to_vec(&wire_hint(&h)).unwrap().len(),
            MIN_ROUTING_HINT_WIRE_BYTES
        );
        assert_eq!(
            routing_hint_wire_bytes(&h),
            Some(MIN_ROUTING_HINT_WIRE_BYTES)
        );
        h.sign = SignedRoutingBias::Weaken;
        assert_eq!(
            serde_json::to_vec(&wire_hint(&h)).unwrap().len(),
            MAX_ROUTING_HINT_WIRE_BYTES
        );
        assert_eq!(
            routing_hint_wire_bytes(&h),
            Some(MAX_ROUTING_HINT_WIRE_BYTES)
        );
        assert!(MAX_ROUTING_HINTS > 8);
        let full = vec![h.clone(); MAX_ROUTING_HINTS];
        let wire: Vec<_> = full.iter().map(wire_hint).collect();
        assert!(serde_json::to_vec(&wire).unwrap().len() <= MAX_ROUTING_HINT_BYTES);
        assert!(routing_hints_within_wire_budget(&full));
        h.sign = SignedRoutingBias::Boost;
        let oversized = vec![h; MAX_ROUTING_HINTS + 1];
        let wire: Vec<_> = oversized.iter().map(wire_hint).collect();
        assert!(serde_json::to_vec(&wire).unwrap().len() > MAX_ROUTING_HINT_BYTES);
        assert!(!routing_hints_within_wire_budget(&oversized));
        assert!(routing_hints_within_wire_budget(&[]));
    }
    #[test]
    fn malformed_typed_hints_cannot_bypass_byte_admission_or_shape_validation() {
        let mut h = hint();
        h.route.previous_fingerprint = "\"\\\t\r\n\u{0008}\u{000c}\u{0001}mémöry".repeat(20);
        assert!(!h.route.has_canonical_fingerprints());
        assert_eq!(
            routing_hint_wire_bytes(&h),
            Some(serde_json::to_vec(&wire_hint(&h)).unwrap().len())
        );
        h.route.previous_fingerprint = "a".repeat(MAX_ROUTING_HINT_BYTES);
        assert!(routing_hint_wire_bytes(&h).is_none());
        assert!(!routing_hints_within_wire_budget(&[h]));
        let mut h = hint();
        h.route.previous_fingerprint = "A".repeat(64);
        assert!(!h.route.has_canonical_fingerprints());
        assert!(!routing_hints_admissible(&[hint(), h.clone()]));
        h.route.previous_fingerprint = "a".repeat(63);
        assert!(!h.route.has_canonical_fingerprints());
        assert!(!routing_hints_admissible(&[hint(), h.clone()]));
        h.route.previous_fingerprint = "a".repeat(MAX_ROUTING_HINT_BYTES / 2);
        assert!(routing_hint_wire_bytes(&h).is_some());
        assert!(!routing_hints_within_wire_budget(&[h.clone(), h]));
    }
    fn historical_episode(revision: bool, contexts: Option<crate::OccurrenceContexts>) -> Node {
        // Authored synthetic episode fixture. IDs, source proofs, clock anchors
        // and origin commit are test data, not a deployed artifact.
        let digest = crate::ConcernDigest::from_hex(if revision {
            "2c0e8e71ffa109793877c7154bfc1f02a7c507520a85a6257b3451e995bcb00b"
        } else {
            "26d311d05b189c76b1833697c93ef63c6dae8f88a8dd2e5603116622864420b1"
        })
        .unwrap();
        let source = crate::CaptureSource::new_with_codec(
            "scene-fixture",
            if revision { "event-revision" } else { "event" },
            "fixture:event",
            Some("synthetic-recorder"),
            None,
            *digest.as_bytes(),
            if contexts.is_some() {
                crate::CaptureRequestCodec::EpisodeV2
            } else {
                crate::CaptureRequestCodec::EpisodeV1
            },
        )
        .unwrap();
        let created = if revision {
            1700000001000
        } else {
            1700000000000
        };
        let node = Node::try_new(
            source.node_id(),
            "A remote experiment was recapped in a later session.",
            BodyRef::new(if revision {
                "fs://00000000000000000000000003"
            } else {
                "fs://00000000000000000000000002"
            })
            .unwrap(),
            Vec::<&str>::new(),
            crate::Provenance::External { source },
            0.5,
            0.5,
            NodeStatus::Active,
            created,
        )
        .unwrap()
        .with_origin_commit(Some(
            crate::OriginCommit::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
        ));
        let root = crate::CaptureSource::new_with_codec(
            "scene-fixture",
            "event",
            "fixture:event",
            None,
            None,
            [0; 32],
            crate::CaptureRequestCodec::EpisodeV1,
        )
        .unwrap()
        .node_id();
        let recorded = crate::EpisodeTime::new(1700000000000).unwrap();
        let thread = Some(crate::EpisodeThread::new("workshop").unwrap());
        let mut facet = if revision {
            crate::EpisodeFacet::revised(
                root.into(),
                root,
                crate::EpisodeRevision::new(1),
                crate::OccurrenceSpan::point(crate::EpisodeTime::new(15).unwrap()),
                thread,
                recorded,
                crate::EpisodeRevisionReason::new("Clarify event time from original notes")
                    .unwrap(),
            )
            .unwrap()
        } else {
            crate::EpisodeFacet::initial(
                root,
                crate::OccurrenceSpan::range(
                    crate::EpisodeTime::new(10).unwrap(),
                    crate::EpisodeTime::new(20).unwrap(),
                )
                .unwrap(),
                thread,
                recorded,
            )
            .unwrap()
        };
        if let Some(contexts) = contexts {
            facet = facet.with_occurrence_contexts(contexts);
        }
        node.with_episode(facet).unwrap()
    }

    fn byte_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn canonical_content_codec_has_explicit_byte_and_hash_goldens() {
        let simple = node(1, "old meaning", 1);
        assert_eq!(
            byte_hex(&routing_content_bytes(&simple)),
            "00000000000000186d6e656d652e726f7574696e672d636f6e74656e742e76320000000000000000000000000000000100000000000000000b6f6c64206d65616e696e67000000000000000d696e6c696e653a2f2f73616d650000000000000001000000000000000573636f706504000000000000000000"
        );
        assert_eq!(
            routing_content_fingerprint(&simple),
            "8b950e1041939d37775e87ab07cfb45a229fe7685f64093ed5aa8637e1d013e1"
        );
        let id = 0x0123456789abcdef0011223344556677_u128;
        let external = |codec, session, revision| crate::Provenance::External {
            source: crate::CaptureSource::new_with_codec(
                "fixture",
                "opaque",
                "r:\"\\é\u{2028}x",
                session,
                revision,
                [0x7f; 32],
                codec,
            )
            .unwrap(),
        };
        let cases = [
            (
                crate::Provenance::Web {
                    url: crate::WebUrl::new("https://example.test/path?x=%00").unwrap(),
                    fetched: u128::MAX,
                },
                None,
                "40fbf806200227b4180be585c069479d856ae10525da3046becbba3f56cdfadc",
            ),
            (
                crate::Provenance::Conversation {
                    session: Ulid(id),
                    turn: u32::MAX,
                },
                None,
                "494f572dc33f053a2be26bfa784ddd2a9ae43d05d31fd80662ed47ff36d788a4",
            ),
            (
                crate::Provenance::derived([NodeId(Ulid(3)), NodeId(Ulid(2))]).unwrap(),
                None,
                "a5a89c001a52ec7836d376b0f32c9dd0cc5009b3f79aa164491b28455a5e562c",
            ),
            (
                external(
                    crate::CaptureRequestCodec::CaptureV1,
                    Some("s\\\""),
                    Some("rev"),
                ),
                None,
                "a541518cc679d8f38430e633cfe803fb6993ffd1f5ca02f86fd2c2b9c11ee34a",
            ),
            (
                external(crate::CaptureRequestCodec::CaptureV2, None, None),
                Some(crate::OriginCommit::Sha1([0x33; 20])),
                "58368604161b33a561a2a778a6786cb543a56844a36147fcb5763cf9d146cf7c",
            ),
            (
                crate::Provenance::derived_empty(),
                Some(crate::OriginCommit::Sha256([0x44; 32])),
                "bb95ed22eeec971aad7a4b4f1523dec5e78b7b28198df0690ca9c4b4ca6a9709",
            ),
        ];
        for (provenance, origin, expected) in cases {
            let fixture = Node::try_new(
                NodeId(Ulid(id)),
                "hé\0e\u{301}\\\"\u{2028}\u{200d}",
                BodyRef::new("inline://bytes\\\"é").unwrap(),
                ["z", "a"],
                provenance,
                0.5,
                0.5,
                NodeStatus::Active,
                1,
            )
            .unwrap()
            .with_origin_commit(origin);
            assert_eq!(routing_content_fingerprint(&fixture), expected);
            let encoded = routing_content_bytes(&fixture);
            assert!(
                encoded
                    .windows(fixture.summary().len())
                    .any(|slice| slice == fixture.summary().as_bytes()),
                "NUL, combining text, backslash, quotes and Unicode are encoded as raw UTF-8"
            );
        }
        assert_eq!(
            routing_content_fingerprint(&historical_episode(false, None)),
            "c91008e0e08e12412d0de17596bef52cecca5a1ac3854f4b827940688b93da78"
        );
        assert_eq!(
            routing_content_fingerprint(&historical_episode(true, None)),
            "5ddcbc4e14640eefd40b32f1b5b6456875b1be7738c68b8551029fc4a68adc6c"
        );
        let mut unknown = serde_json::to_value(historical_episode(false, None)).unwrap();
        unknown["memory_kind"]["episode"]["occurred"] = serde_json::json!({"kind":"unknown"});
        unknown["memory_kind"]["episode"]["thread"] = serde_json::json!(null);
        let unknown: Node = serde_json::from_value(unknown).unwrap();
        assert_eq!(
            routing_content_fingerprint(&unknown),
            "d29cca61387db8b1b7f0c49c0c2a5f20cbedd1ba0d079f3afa51010c7ad0eb31"
        );
        let contexts = crate::OccurrenceContexts::new(vec![
            crate::OccurrenceContextRef::new("World", "gate", Some("Scene")).unwrap(),
            crate::OccurrenceContextRef::new("Room", "chat", None).unwrap(),
        ])
        .unwrap();
        assert_eq!(
            routing_content_fingerprint(&historical_episode(false, Some(contexts))),
            "d156bfecbf47cee69f5b04d5b4a2bf31aba8894dd21c5ab890ab18c3f1da834c"
        );
    }

    #[test]
    fn canonical_edge_codec_has_explicit_tags_numeric_widths_and_bit_goldens() {
        let cases = [
            (
                EdgeKind::Associative,
                "06d7295fa8737ecf47218ffbcb2a99a53062ad7a7b875689e0376b6da53c3492",
            ),
            (
                EdgeKind::Bridge,
                "b929b4daf93e616946473e395418a7c8bc07746cb30f8407438c496c4f1d6775",
            ),
            (
                EdgeKind::Transition,
                "6e226890abb7c3d816cfc3528e14a51547275f564ffcca98cab5807a87eafee4",
            ),
            (
                EdgeKind::Supersedes,
                "9a7f47798c714ec1404ebde15e2abebbf1254a9fd782a1c8444c0e1398a0958a",
            ),
            (
                EdgeKind::DerivedFrom,
                "1e74ae55d2874d695af74dff23ef0e8e1bc488fd4c53e838d6068d3b7472cf36",
            ),
        ];
        for (kind, expected) in cases {
            let edge = Edge::new(NodeId(Ulid(1)), NodeId(Ulid(2)), 0.5, kind, 1);
            assert_eq!(routing_edge_fingerprint(&edge), expected);
        }
        let simple = Edge::new(
            NodeId(Ulid(1)),
            NodeId(Ulid(2)),
            0.5,
            EdgeKind::Associative,
            1,
        );
        assert_eq!(
            byte_hex(&routing_edge_bytes(&simple)),
            "00000000000000156d6e656d652e726f7574696e672d656467652e7632000000000000000000000000000000010000000000000000000000000000000201003f000000"
        );
        let anchored = Edge::from_stored(
            NodeId(Ulid(1)),
            NodeId(Ulid(2)),
            EdgeKind::Transition,
            Some(crate::BodySpan::new(0, u32::MAX)),
            f32::from_bits(0x3f000001),
            1,
            u32::MAX,
            3,
        );
        assert_eq!(
            byte_hex(&routing_edge_bytes(&anchored)),
            "00000000000000156d6e656d652e726f7574696e672d656467652e76320000000000000000000000000000000100000000000000000000000000000002030100000000ffffffff3f000001"
        );
        assert_eq!(
            routing_edge_fingerprint(&anchored),
            "bf1d22be510d61ea0a02d8c56672c52ff8e36f018ea7089564b46d0b54930ed1"
        );
        let different_anchor = Edge::from_stored(
            anchored.from,
            anchored.to,
            anchored.kind,
            Some(crate::BodySpan::new(1, u32::MAX)),
            anchored.weight(),
            999,
            7,
            4,
        );
        assert_ne!(
            routing_edge_fingerprint(&anchored),
            routing_edge_fingerprint(&different_anchor)
        );
        let different_bits = Edge::from_stored(
            anchored.from,
            anchored.to,
            anchored.kind,
            anchored.anchor,
            f32::from_bits(0x3f000002),
            1,
            u32::MAX,
            3,
        );
        assert_ne!(
            routing_edge_fingerprint(&anchored),
            routing_edge_fingerprint(&different_bits)
        );
        let telemetry = Edge::from_stored(
            anchored.from,
            anchored.to,
            anchored.kind,
            anchored.anchor,
            anchored.weight(),
            999,
            7,
            4,
        );
        assert_eq!(
            routing_edge_fingerprint(&anchored),
            routing_edge_fingerprint(&telemetry)
        );
    }

    #[test]
    fn canonical_framing_options_unicode_and_collection_semantics_are_explicit() {
        let mut ab_c = RoutingFingerprintBytes::default();
        ab_c.text("ab");
        ab_c.text("c");
        let mut a_bc = RoutingFingerprintBytes::default();
        a_bc.text("a");
        a_bc.text("bc");
        assert_ne!(ab_c.0, a_bc.0);
        let mut absent = RoutingFingerprintBytes::default();
        absent.optional_text(None);
        let mut empty = RoutingFingerprintBytes::default();
        empty.optional_text(Some(""));
        assert_eq!(absent.0, [0]);
        assert_eq!(empty.0, [1, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_ne!(
            routing_content_fingerprint(&node(1, "é", 1)),
            routing_content_fingerprint(&node(1, "e\u{301}", 1)),
            "no Unicode normalization"
        );
        assert_ne!(
            routing_content_fingerprint(&node(1, "a\0b", 1)),
            routing_content_fingerprint(&node(1, "a\\0b", 1)),
            "raw NUL is not an escape spelling"
        );
        let fixture = |tags: &[&str], sources: &[NodeId]| {
            Node::try_new(
                NodeId(Ulid(1)),
                "same",
                BodyRef::new("inline://same").unwrap(),
                tags.iter().copied(),
                crate::Provenance::derived(sources.iter().copied()).unwrap(),
                0.5,
                0.5,
                NodeStatus::Active,
                1,
            )
            .unwrap()
        };
        let sources = [NodeId(Ulid(2)), NodeId(Ulid(3))];
        assert_eq!(
            routing_content_bytes(&fixture(&["z", "a"], &sources)),
            routing_content_bytes(&fixture(&["a", "z"], &sources)),
            "tags have canonical set order"
        );
        assert_ne!(
            routing_content_fingerprint(&fixture(&["a", "z"], &sources)),
            routing_content_fingerprint(&fixture(&["a", "z"], &[sources[1], sources[0]])),
            "derived-source authored order remains meaningful"
        );
        let mut high_ids = Edge::new(
            NodeId(Ulid(u128::MAX)),
            NodeId(Ulid(0)),
            1.0,
            EdgeKind::Bridge,
            1,
        );
        let encoded = routing_edge_bytes(&high_ids);
        // u64 domain length + 21 domain bytes, then the two exact 16-byte IDs.
        assert_eq!(&encoded[29..45], &[255; 16]);
        assert_eq!(&encoded[45..61], &[0; 16]);
        high_ids.to = NodeId(Ulid(1));
        assert_ne!(encoded, routing_edge_bytes(&high_ids));
    }

    #[test]
    fn historical_debug_bindings_are_not_silently_reissued() {
        assert_ne!(
            routing_content_fingerprint(&historical_episode(false, None)),
            "300f98796a696f29ea3d59379e1aa31d867ffb005854279e878d974e85456a7c"
        );
        assert_ne!(
            routing_content_fingerprint(&historical_episode(true, None)),
            "738e502212e48028eb5e83edeae4cc775ddb1d3a619978cfd6f408ce05862700"
        );
    }

    #[test]
    fn routing_episode_metadata_meaning_and_telemetry_contract_is_unchanged() {
        use serde_json::json;
        for first in [
            historical_episode(false, None),
            historical_episode(true, None),
        ] {
            let original = serde_json::to_value(&first).unwrap();
            for (field, value) in [
                ("summary", json!("A changed account")),
                ("body", json!("inline://changed")),
                ("tags", json!(["changed"])),
                ("origin_commit", json!(null)),
            ] {
                let mut changed = original.clone();
                changed[field] = value;
                let changed: Node = serde_json::from_value(changed).unwrap();
                assert_ne!(
                    routing_content_fingerprint(&first),
                    routing_content_fingerprint(&changed),
                    "{field} remains meaningful"
                );
            }
            let mut source_change = original.clone();
            source_change["provenance"]["External"]["source"]["reference"] = json!("fixture:other");
            let changed: Node = serde_json::from_value(source_change).unwrap();
            assert_ne!(
                routing_content_fingerprint(&first),
                routing_content_fingerprint(&changed)
            );
            let mut unknown = original.clone();
            unknown["memory_kind"]["episode"]["occurred"] = json!({"kind":"unknown"});
            unknown["memory_kind"]["episode"]["thread"] = json!(null);
            let unknown: Node = serde_json::from_value(unknown).unwrap();
            assert_ne!(
                routing_content_fingerprint(&first),
                routing_content_fingerprint(&unknown)
            );
            if first.episode().unwrap().revision().get() > 0 {
                let mut clock = original.clone();
                clock["created"] = json!(1790837999999_u128);
                let clock: Node = serde_json::from_value(clock).unwrap();
                assert_eq!(
                    routing_content_fingerprint(&first),
                    routing_content_fingerprint(&clock),
                    "edition host clock remains excluded"
                );
            }
            for (field, value) in [
                ("body_ownership", json!("managed")),
                ("confidence", json!(0.8)),
                ("stability", json!(0.8)),
                ("exposure_count", json!(99)),
                ("last_exposed", json!(1790837999999_u128)),
            ] {
                let mut telemetry = original.clone();
                telemetry[field] = value;
                let telemetry: Node = serde_json::from_value(telemetry).unwrap();
                assert_eq!(
                    routing_content_fingerprint(&first),
                    routing_content_fingerprint(&telemetry),
                    "{field} is still excluded"
                );
            }
        }
    }

    #[test]
    fn occurrence_routing_fingerprints_bind_canonical_context_not_input_order() {
        fn contexts(items: &[(&str, &str, Option<&str>)]) -> crate::OccurrenceContexts {
            crate::OccurrenceContexts::new(
                items
                    .iter()
                    .map(|(namespace, key, label)| {
                        crate::OccurrenceContextRef::new(namespace, key, *label).unwrap()
                    })
                    .collect(),
            )
            .unwrap()
        }
        let first = historical_episode(
            false,
            Some(contexts(&[
                ("World", "gate", Some("Scene")),
                ("Room", "chat", None),
            ])),
        );
        let reordered = historical_episode(
            false,
            Some(contexts(&[
                ("Room", "chat", None),
                ("World", "gate", Some("Scene")),
            ])),
        );
        assert_eq!(
            routing_content_fingerprint(&first),
            routing_content_fingerprint(&reordered)
        );
        assert_ne!(
            routing_content_fingerprint(&first),
            routing_content_fingerprint(&historical_episode(false, None))
        );
        for changed in [
            contexts(&[("World", "gate", None), ("Room", "chat", None)]),
            contexts(&[("World", "gate", Some("New scene")), ("Room", "chat", None)]),
            contexts(&[("world", "gate", Some("Scene")), ("Room", "chat", None)]),
            contexts(&[("World", "other", Some("Scene")), ("Room", "chat", None)]),
            contexts(&[("World", "gate", Some("Scene"))]),
        ] {
            assert_ne!(
                routing_content_fingerprint(&first),
                routing_content_fingerprint(&historical_episode(false, Some(changed)))
            );
        }
        let ab_c = historical_episode(false, Some(contexts(&[("ab", "c", None)])));
        let a_bc = historical_episode(false, Some(contexts(&[("a", "bc", None)])));
        assert_ne!(
            routing_content_fingerprint(&ab_c),
            routing_content_fingerprint(&a_bc),
            "context parts are length framed"
        );
        let mut telemetry = first.clone();
        telemetry.record_grounded_use(1790837999999);
        telemetry.set_status(NodeStatus::Archived);
        assert_eq!(
            routing_content_fingerprint(&first),
            routing_content_fingerprint(&telemetry)
        );
    }

    #[test]
    fn routing_fingerprints_bind_meaning_not_telemetry() {
        let first = node(1, "old meaning", 1);
        let mut telemetry = node(1, "old meaning", 999);
        telemetry.record_grounded_use(1000);
        telemetry.set_status(NodeStatus::Archived);
        assert_eq!(
            routing_content_fingerprint(&first),
            routing_content_fingerprint(&telemetry)
        );
        assert_ne!(
            routing_content_fingerprint(&first),
            routing_content_fingerprint(&node(1, "new meaning", 1))
        );
        let a = Edge::new(
            first.id(),
            NodeId(Ulid::from(2)),
            0.6,
            EdgeKind::Transition,
            1,
        );
        let b = Edge::new(a.from, a.to, 0.6, EdgeKind::Transition, 999);
        assert_eq!(routing_edge_fingerprint(&a), routing_edge_fingerprint(&b));
        assert_ne!(
            routing_edge_fingerprint(&a),
            routing_edge_fingerprint(&Edge::new(a.from, a.to, 0.7, a.kind, 1))
        );
        assert_ne!(
            routing_edge_fingerprint(&a),
            routing_edge_fingerprint(&Edge::new(a.to, a.from, 0.6, a.kind, 1))
        );
    }
}

#[cfg(test)]
mod capture_similarity_prior_tests {
    use super::*;
    #[test]
    fn generated_budget_is_a_distinct_bounded_output_allowance() {
        assert_eq!(CapturePriorBudget::new(0).limit(), 0);
        assert_eq!(CapturePriorBudget::new(2).limit(), 2);
        assert_eq!(
            CapturePriorBudget::new(usize::MAX).limit(),
            MAX_CAPTURE_EDGES
        );
        assert_eq!(
            MAX_CAPTURE_PRIOR_CANDIDATES,
            crate::MAX_NODE_HYDRATION_BATCH
        );
    }
    #[test]
    fn prior_binds_meaning_and_active_semantic_status_not_telemetry() {
        let id = NodeId(Ulid::new());
        let node = Node::try_new(
            id,
            "meaning",
            BodyRef::new("inline://meaning").unwrap(),
            std::iter::empty::<&str>(),
            crate::Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        let source = NodeId(Ulid::new());
        let prior = CaptureSimilarityPrior::new(
            Edge::new(source, id, 0.8, EdgeKind::Associative, 1),
            &node,
        )
        .unwrap();
        assert!(prior.matches_target(&node));
        let mut telemetry = node.clone();
        telemetry.set_confidence(crate::Confidence::new(0.7).unwrap());
        assert!(prior.matches_target(&telemetry));
        let mut archived = node.clone();
        archived.set_status(NodeStatus::Archived);
        assert!(!prior.matches_target(&archived));
        assert!(
            CaptureSimilarityPrior::new(Edge::new(source, id, 0.8, EdgeKind::Transition, 1), &node)
                .is_err()
        );
        assert!(
            CaptureSimilarityPrior::new(Edge::new(id, id, 0.8, EdgeKind::Associative, 1), &node)
                .is_err()
        );
        let changed = Node::try_new(
            id,
            "new meaning",
            BodyRef::new("inline://meaning").unwrap(),
            std::iter::empty::<&str>(),
            crate::Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        assert!(!prior.matches_target(&changed));
    }
}
