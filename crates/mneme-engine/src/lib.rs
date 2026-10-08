//! mneme-engine — the services that turn ports into a working memory.
//!
//! Everything here is generic over the [`ports`](mneme_core::ports): the engine
//! holds `Arc<dyn Trait>` for each capability and never names a concrete
//! backend. That's the discipline from the design — the dependency arrow points
//! only at `mneme-core`. The integration tests (and the `mnemed` daemon) are
//! where adapters actually get wired in.
//!
//! The methods are split along the hot/cold cost seam:
//!
//! - **Hot path** (runs during a query, must stay cheap — no per-node model
//!   inference): [`Memory::ingest`] and [`Memory::retrieve`]. Embedding is one
//!   local call; everything after is bounded dense+sparse retrieval, an optional
//!   graph leg, and pure result planning. Grounded feedback trains later.
//! - **Cold path** (background / off-turn, where model-sized work belongs):
//!   [`Memory::decay_sweep`] and the
//!   contradiction machinery ([`Memory::observe_contradiction`],
//!   [`Memory::reconcile`], [`Memory::supersede`]). These provide the
//!   *mechanism*; the *decisions* (what's a real contradiction, which side
//!   wins) belong to the specialist agents that call them.

pub mod conditional_preference;
pub mod episode;
mod feedback;
pub mod graph_view;
pub mod neighbor_inspect;
pub mod touchstone;
pub use episode::{EpisodeWrite, EpisodeWriteResult};
pub use feedback::{ObservedRoute, ReceiptFeedback, ReceiptFeedbackResult};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroU16;
use std::sync::Arc;

use mneme_core::ports::{
    BodyChunk, BodyStore, Budget, CaptureCommitOutcome, CaptureSimilarityPrior, Clock, ClusterId,
    ColdPath, Embedder, Error, FullMergeCommit, FullMergeCommitOutcome, GraphStore, LexicalIndex,
    MAX_CAPTURE_EDGES, MAX_MAINTENANCE_BATCH_ROWS, MaintenanceCommit, MaintenanceCommitOutcome,
    MaintenanceEdgeMutation, Neighbor, NodeLifecycle, Reranker, Result, RoutingBiasMap,
    RoutingBinding, RoutingHint, RoutingOutcome, Scored, SignedRoutingBias, StatusFilter,
    SupersedeCommit, SupersedeCommitOutcome, Traversal, TraversalHop, TraversalScope, VectorIndex,
    routing_content_fingerprint, routing_edge_fingerprint, routing_hints_admissible,
};
use mneme_core::tagged::{
    MAX_TAGGED_QUERY_TAGS, MAX_TAGGED_VECTOR_DIMENSION, RetrievalLifecycleLane,
    TAGGED_ACTIVE_FALLBACK_CANDIDATES, TaggedAnnLane, TaggedAnnLaneRequest, TaggedAnnRequest,
    TaggedAnnWork, TaggedAnnWorkLimits, TaggedProjectionGeneration, TaggedSeedCoverage,
};
use mneme_core::{
    BodyOwnership, BodyRef, BodySpan, BoundedTagSet, CaptureReplayProof, CaptureRequestCodec,
    CaptureSource, Confidence, Contradiction, Edge, EdgeKind, EmbeddingFingerprint,
    MAX_INCIDENT_EDGES, MAX_NODE_HYDRATION_BATCH, MAX_NODE_STATUS_BATCH, MAX_REMOTE_EDGE_PAGE_SIZE,
    MergeCandidate, MergeResolution, Node, NodeId, NodeInit, NodeStatus, NodeSummary, OriginCommit,
    Provenance, RemoteEdge, RemoteEdgeCursor, RemoteEdgePage, Resolution, Stability,
    StrengthParams, Timestamp, validate_tag,
};
use sha2::{Digest, Sha256};
use ulid::Ulid;
use unordered_pair::UnorderedPair;

/// Native capture body ceiling shared by the CLI and MCP frontends.
pub const MAX_CAPTURE_BODY_BYTES: usize = 256 * 1024;

/// Canonical domain ceiling for summaries accepted by ingest. Frontends may
/// impose a smaller presentation or transport policy bound.
pub use mneme_core::MAX_NODE_SUMMARY_BYTES;

/// Maximum number of fully hydrated one-hop neighbors returned by one engine
/// call. Storage may inspect the complete bounded incident set first, but
/// presentation-oriented callers never hydrate more than this shortlist.
pub const MAX_RESOLVED_NEIGHBORS: usize = 64;

/// The blessed tag marking always-loaded "core" memory: user identity, the
/// active project, working assumptions. Core nodes are returned by
/// [`Memory::core`]. The tag is explicit authority, not a retention score.
pub const CORE_TAG: &str = "core";
const MAX_LEXICAL_K: usize = 64;
const MAX_RETRIEVAL_RANK: usize = u16::MAX as usize;
pub const MAX_PROJECTION_WATERMARKS: usize = 8;
const TAGGED_RETRIEVAL_POLICY_GENERATION: &str = "tagged-seed-v3";
const MAX_DENSE_PRUNE_CHUNKS: usize = MAX_INCIDENT_EDGES.div_ceil(MAX_MAINTENANCE_BATCH_ROWS);

/// Retrieval and edge-learning policy knobs, not correctness invariants.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Seeds pulled from ANN before the spread.
    pub ann_k: usize,
    /// Maximum summary-BM25 seeds fused with dense seeds. The effective count is
    /// also capped by the caller's dense `k` and [`MAX_LEXICAL_K`]. `0` disables
    /// sparse retrieval even when a [`LexicalIndex`] is wired.
    pub lexical_k: usize,
    /// Reciprocal-rank-fusion denominator. Larger values make rank differences
    /// within each retrieval leg gentler; only ordering is fused, never raw BM25
    /// and cosine magnitudes.
    pub rrf_constant: f32,
    /// Dense leg weight in reciprocal-rank fusion.
    pub dense_weight: f32,
    /// Sparse leg weight. A small exact-token preference helps identifiers,
    /// symbols, and proper nouns that semantic embedding models often blur.
    pub lexical_weight: f32,
    /// Maximum direct-retrieval hits used as roots for graph traversal. Roots
    /// are selected across fused, dense, and sparse evidence lanes, then start
    /// with equal activation so overlap-heavy fused hits cannot starve a strong
    /// single-lane anchor. The complete dense+sparse ranking is still retained
    /// as the direct result leg; this cap bounds graph work. `0` disables graph
    /// traversal.
    pub graph_seed_cap: usize,
    /// Reciprocal-rank-fusion weight for non-root graph expansions when they are
    /// merged back into the complete direct retrieval leg. Path activation
    /// orders this leg, but its attenuated magnitude is deliberately not compared
    /// directly with dense/RRF scores. Non-finite or non-positive values disable
    /// the graph leg; finite values at or above one are clamped just below one so
    /// a graph contribution cannot outrank the direct winner at fusion. Optional
    /// reranking, deduplication and lifecycle changes can alter the final result.
    pub graph_weight: f32,
    /// Maximum graph interventions admitted into the final fused ranking. An
    /// intervention is either a novel graph-only candidate or graph evidence
    /// strong enough to promote a non-root direct-tail hit. This is an explicit
    /// base-result quota: traversal may discover more, but it cannot alter more
    /// than this many ranks. `0` disables graph expansion while leaving direct
    /// retrieval unchanged. The default representable ceiling lets the requested
    /// `Budget::max_nodes` govern admission; positive overrides remain hard caps.
    pub graph_slot_cap: usize,
    /// Maximum embedding-similarity routes for one first-write capture or
    /// experimental ingest. Nomination uses the separate bounded `ann_k`. These
    /// links duplicate evidence already available from ANN, add ingest-time ANN
    /// and anchoring work, and can drown learned paths in a dense graph, so they
    /// are disabled by default. A nonzero value enables the experimental prior
    /// and is also a hard per-ingest cap.
    pub similarity_link_cap: usize,
    /// On ingest, create an associative edge to an existing node when their
    /// cosine similarity clears this. Used only when `similarity_link_cap > 0`.
    pub similarity_link_threshold: f32,
    /// Experimental legacy floor (default zero): always wire top-N neighbours on
    /// ingest even if they're below the threshold. The actual set is bounded by
    /// `similarity_link_cap`; orphan nodes are valid and expected when the prior
    /// is disabled. Weak floor links start at their similarity and decay away if
    /// never used.
    pub min_similarity_links: usize,
    /// Reserved legacy cap for experimental co-retrieval learning. Retrieval is
    /// now a pure planning operation, so this knob is deliberately not consumed on
    /// the query path: a ranked node is not known to have reached model context
    /// until a presentation layer has packed and emitted it. A future explicit
    /// presentation commit may use this cap for controlled ablations; grounded
    /// [`Memory::apply_feedback`] remains the production topology-training path.
    pub coretrieval_link_cap: usize,
    /// Shape of the edge-strength curve: diminishing-returns reinforcement plus
    /// floor-capped, interference-aware decay. See [`StrengthParams`].
    pub strength: StrengthParams,
    /// Strength curve for [`EdgeKind::Bridge`] edges — a slower clock than
    /// `strength`: reinforced gently and decayed gently, so a long-range link
    /// that only fires occasionally still survives long enough to prove itself.
    /// On graduation to `Associative` (intra-cluster) the edge reverts to
    /// `strength`'s snappier dynamics.
    pub bridge_strength: StrengthParams,
    /// Default spread budget for [`Memory::retrieve`].
    pub budget: Budget,
    /// Scheme used to store ingested bodies (must have a registered store).
    pub default_body_scheme: &'static str,
    /// Probability the experimental consolidation pass mints a long-range bridge
    /// for a cross-cluster pair. Disabled by default: automatic random bridges
    /// have not beaten study-only feedback in the local ablation, and should not
    /// impose whole-graph work or speculative topology without evidence.
    pub bridge_probability: f32,
    /// Initial (weak) weight of a minted long-range bridge: it has to earn its
    /// keep through use or it fades.
    pub bridge_weight: f32,
    /// Density GC ([`Memory::prune_dense`]): an edge with **zero trials** and weight
    /// below this is a never-validated similarity prior — deleted outright. Above
    /// it, or once it has any trial history, an edge is kept.
    pub prune_weight_floor: f32,
    /// Target cap on stored associative/bridge edges incident to one node.
    /// [`Memory::prune_dense`] deletes the weakest excess links even when
    /// previously trialed, but this is an eventual cold-path bound rather than
    /// write-time admission. Structural edges are exempt; arrival admission does
    /// not consult this maintenance policy.
    pub dense_degree_threshold: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ann_k: 5,
            lexical_k: 8,
            rrf_constant: 60.0,
            dense_weight: 1.0,
            lexical_weight: 1.15,
            // One root per fused/dense/sparse evidence lane preserves anchor
            // diversity without letting a larger root set manufacture competing
            // learned paths. Graph admission uses the requested candidate capacity; explicit
            // graph_slot_cap overrides can impose a smaller intervention quota.
            graph_seed_cap: 3,
            graph_weight: 1.0,
            graph_slot_cap: MAX_RETRIEVAL_RANK,
            similarity_link_cap: 0,
            similarity_link_threshold: 0.25,
            min_similarity_links: 0,
            coretrieval_link_cap: 0,
            strength: StrengthParams::default(),
            // Patient by design: ~half the reinforce step of an associative edge
            // (climbs slowly), and high per-miss retention (≈0.8 fresh → 0.97
            // worn vs 0.6 → 0.95) so early misses barely erode a young bridge —
            // it gets time to be co-used before decay can prune it.
            bridge_strength: StrengthParams {
                reinforce_gain: 0.2,
                interference_retention: 0.8,
                interference_resist: 0.97,
                resist_trials_half: 20.0,
            },
            // Exploration stays opt-in: the epsilon path admits weak edges at the
            // raw relevance floor, but later adaptive cutoffs generally discard
            // those results while their routing can still perturb descendants.
            // Keep the shipped path deterministic until exploration has its own
            // result lane and measurable contract. Rank-relative relevance
            // pruning is also opt-in: it erased grounded/direct-tail hits for a
            // negligible context saving in the scale/noise ablations. Semantic
            // result dedup stays opt-in because it adds a second embedding pass
            // and can erase distinct facts that happen to use templated language.
            budget: Budget {
                explore: 0.0,
                relevance_ratio: 0.0,
                dedup_similarity: 1.0,
                max_depth: 4,
                // Pull the spread toward the query, not just the strongest edges —
                // a per-query attention mask over each node's neighbourhood, which
                // is also the main lever against hub/density noise at retrieval.
                query_conditioning: 0.4,
                ..Budget::default()
            },
            default_body_scheme: "inline",
            bridge_probability: 0.0,
            bridge_weight: 0.15,
            prune_weight_floor: 0.2,
            dense_degree_threshold: 24,
        }
    }
}

/// Supplies identities for newly ingested nodes. The engine's default source
/// uses coordination-free random ULIDs; deterministic tests and benchmarks can
/// inject a reproducible source without weakening production uniqueness.
///
/// Implementations must be safe to call from multiple threads. `Memory` also
/// serializes ingest mutations, but the stronger contract keeps the source
/// independently reusable and avoids hiding concurrency assumptions in it.
pub trait NodeIdSource: Send + Sync {
    fn next_id(&self) -> NodeId;
}

/// Production identity source: a fresh random ULID per node.
#[derive(Default)]
pub struct RandomNodeIdSource;

impl NodeIdSource for RandomNodeIdSource {
    fn next_id(&self) -> NodeId {
        NodeId(Ulid::new())
    }
}

/// Reproducible, thread-safe identity source for tests and evaluation. The seed
/// occupies the high half of the ULID; the low half is a seeded permutation of
/// the one-based atomic ordinal. Equal seeds and ingest order therefore yield
/// equal IDs, but equal-score tie order is not merely corpus ingest order.
///
/// [`seeded_ordinal_permutation`] is a bijection over all `u64` values: xor,
/// xor-shift, and multiplication by an odd integer are each invertible modulo
/// 2^64. Distinct ordinals therefore remain distinct without a collision table.
pub struct DeterministicNodeIdSource {
    seed: u64,
    next: std::sync::atomic::AtomicU64,
}

impl DeterministicNodeIdSource {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            next: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl NodeIdSource for DeterministicNodeIdSource {
    fn next_id(&self) -> NodeId {
        let ordinal = self
            .next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .checked_add(1)
            .expect("deterministic node-id source exhausted its u64 ordinal space");
        NodeId(Ulid::from(
            ((self.seed as u128) << 64) | seeded_ordinal_permutation(ordinal, self.seed) as u128,
        ))
    }
}

/// Stable SplitMix64 finalizer used as a keyed permutation, not as a PRNG. Every
/// operation is bijective on `u64`, while the avalanche breaks the accidental
/// equivalence between ingest order and equal-score ID tie order.
fn seeded_ordinal_permutation(ordinal: u64, seed: u64) -> u64 {
    let mut value = ordinal ^ seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

/// A request to materialize a node. Borrows its inputs; nothing is stored until
/// [`Memory::ingest`] commits it.
pub struct Ingest<'a> {
    pub summary: &'a str,
    pub body: &'a [u8],
    /// An explicit pre-existing body reference (e.g. `fs://…` pointing at a file
    /// on disk, or `https://…`). When set, the body is **referenced, not copied**
    /// — `body` is ignored. When `None`, `body`'s bytes are stored via the body
    /// store under the default scheme.
    pub body_ref: Option<BodyRef>,
    pub tags: &'a [&'a str],
    pub provenance: Provenance,
    pub stability: f32,
    pub confidence: f32,
    /// The git commit the host's working tree was at when this memory was formed,
    /// if any. Set by the front-end (which is allowed to read git); the engine just
    /// stamps it onto the node. `None` for memories formed outside a repo.
    pub origin_commit: Option<OriginCommit>,
}

impl<'a> Ingest<'a> {
    /// Create an immediately searchable semantic memory.
    pub fn new(
        summary: &'a str,
        body: &'a [u8],
        tags: &'a [&'a str],
        provenance: Provenance,
    ) -> Self {
        Self {
            summary,
            body,
            body_ref: None,
            tags,
            provenance,
            stability: 0.5,
            confidence: 0.5,
            origin_commit: None,
        }
    }

    /// Stamp the git commit this memory is being formed against (the front-end
    /// resolves it from the working tree; validation happens here before the
    /// request can reach embedding or body-store side effects).
    pub fn with_origin_commit(mut self, commit: Option<&str>) -> Result<Self> {
        self.origin_commit = commit
            .map(OriginCommit::parse)
            .transpose()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        Ok(self)
    }

    /// Reference an existing body (`fs://…`, `https://…`) instead of storing
    /// `body`'s bytes — the node points at the file/URL in place.
    pub fn with_body_ref(mut self, body_ref: BodyRef) -> Self {
        self.body_ref = Some(body_ref);
        self
    }

    pub fn with_stability(mut self, stability: f32) -> Self {
        self.stability = stability;
        self
    }

    pub fn with_confidence(mut self, confidence: f32) -> Self {
        self.confidence = confidence;
        self
    }
}

/// A source-keyed, retry-safe capture request. Unlike ordinary `Ingest`, this
/// operation commits its canonical node, vector, source-authored links, and
/// admitted optional similarity priors atomically. Priors are disabled by default
/// and are derived state, not part of the source-authored request identity.
/// An exact retry returns the original id
/// while that node still exists; explicit `forget` removes the retry proof.
/// `revision` is source-authored identity and is bound into that proof. By
/// contrast, `origin_commit` is the host's working-tree anchor at first capture:
/// a retry after checkout changes preserves the original anchor rather than
/// turning the same source observation into a conflict.
/// A source-authored edge created atomically with a capture. This is not an
/// embedding-similarity guess or a feedback observation.
#[derive(Clone, Copy, Debug)]
pub struct CaptureLink {
    pub to: NodeId,
    pub kind: EdgeKind,
    pub weight: f32,
}

impl CaptureLink {
    pub fn new(to: NodeId, kind: EdgeKind, weight: f32) -> Result<Self> {
        let link = Self { to, kind, weight };
        link.validate()?;
        Ok(link)
    }

    fn validate(&self) -> Result<()> {
        if !matches!(
            self.kind,
            EdgeKind::Associative | EdgeKind::Transition | EdgeKind::DerivedFrom
        ) {
            return Err(Error::InvalidInput(
                "capture link kind must be associative, transition, or derived-from".into(),
            ));
        }
        if !self.weight.is_finite() || !(0.0..=1.0).contains(&self.weight) {
            return Err(Error::InvalidInput(
                "capture link weight must be finite and within [0, 1]".into(),
            ));
        }
        Ok(())
    }
}

pub struct Capture<'a> {
    pub namespace: &'a str,
    pub key: &'a str,
    pub reference: &'a str,
    pub session: Option<&'a str>,
    pub revision: Option<&'a str>,
    pub summary: &'a str,
    pub body: &'a [u8],
    pub tags: &'a [&'a str],
    pub links: &'a [CaptureLink],
    /// A typed historical summary annotation owned by this semantic note.
    /// The first native commit copies its references atomically; retries never
    /// refresh historical evidence from the current target rows.
    pub touchstone: Option<mneme_core::touchstone::TouchstoneInput>,
    pub stability: f32,
    pub confidence: f32,
    /// Host-supplied anchor recorded on first application, not part of the
    /// source request digest. A replay does not update the persisted anchor.
    pub origin_commit: Option<OriginCommit>,
}

impl<'a> Capture<'a> {
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        namespace: &'a str,
        key: &'a str,
        reference: &'a str,
        session: Option<&'a str>,
        revision: Option<&'a str>,
        summary: &'a str,
        body: &'a [u8],
        tags: &'a [&'a str],
    ) -> Self {
        Self {
            namespace,
            key,
            reference,
            session,
            revision,
            summary,
            body,
            tags,
            links: &[],
            touchstone: None,
            stability: 0.5,
            confidence: 0.5,
            origin_commit: None,
        }
    }

    pub fn with_origin_commit(mut self, commit: Option<&str>) -> Result<Self> {
        self.origin_commit = commit
            .map(OriginCommit::parse)
            .transpose()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        Ok(self)
    }

    pub fn with_links(mut self, links: &'a [CaptureLink]) -> Self {
        self.links = links;
        self
    }

    pub fn with_touchstone(mut self, touchstone: mneme_core::touchstone::TouchstoneInput) -> Self {
        self.touchstone = Some(touchstone);
        self
    }

    pub fn with_stability(mut self, stability: f32) -> Self {
        self.stability = stability;
        self
    }

    pub fn with_confidence(mut self, confidence: f32) -> Self {
        self.confidence = confidence;
        self
    }

    /// Validate a capture and derive its exact source identity and request digest
    /// without touching inference or storage. Uses the same preparation as capture.
    pub fn validated_source(self) -> Result<CaptureSource> {
        Ok(PreparedCapture::try_from(self)?.source)
    }

    /// Validate the incoming fields and produce its retry proof. Ordinary
    /// captures retain the bounded legacy alternatives; touchstones are exact
    /// successor-codec requests and cannot replay untyped predecessors.
    pub fn validated_replay_proof(self) -> Result<CaptureReplayProof> {
        PreparedCapture::try_from(self)?.replay_proof()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureResult {
    pub id: NodeId,
    pub replayed: bool,
}

struct PreparedCapture<'a> {
    source: CaptureSource,
    legacy_candidate_digest: [u8; 32],
    legacy_active_digest: [u8; 32],
    summary: NodeSummary,
    body: &'a [u8],
    tags: BoundedTagSet,
    links: Vec<CaptureLink>,
    touchstone: Option<mneme_core::touchstone::TouchstoneInput>,
    stability: Stability,
    confidence: Confidence,
    origin_commit: Option<OriginCommit>,
}

impl<'a> TryFrom<Capture<'a>> for PreparedCapture<'a> {
    type Error = Error;

    fn try_from(request: Capture<'a>) -> Result<Self> {
        let codec = if request.touchstone.is_some() {
            CaptureRequestCodec::TouchstoneV1
        } else {
            CaptureRequestCodec::CaptureV2
        };
        Self::with_codec(request, codec)
    }
}

impl<'a> PreparedCapture<'a> {
    fn with_codec(request: Capture<'a>, codec: CaptureRequestCodec) -> Result<Self> {
        // Reconstruct through the typed constructor here as well: the pure
        // preparation boundary settles bounds and canonical reference order
        // before replay lookup, inference, clock or body-store work.
        let touchstone = request
            .touchstone
            .as_ref()
            .map(|input| {
                mneme_core::touchstone::TouchstoneInput::new(
                    input.subject().clone(),
                    input.references().to_vec(),
                )
            })
            .transpose()?;
        if touchstone.is_some() != (codec == CaptureRequestCodec::TouchstoneV1) {
            return Err(Error::InvalidInput(
                "typed touchstone input requires the touchstone_v1 semantic codec".into(),
            ));
        }
        if request.body.len() > MAX_CAPTURE_BODY_BYTES {
            return Err(Error::InvalidInput(format!(
                "capture body exceeds {MAX_CAPTURE_BODY_BYTES} bytes"
            )));
        }
        let summary = NodeSummary::new(request.summary)
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let tags = BoundedTagSet::try_from_iter(request.tags.iter().copied())
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let stability = Stability::new(request.stability)
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let confidence = Confidence::new(request.confidence)
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        // A temporary validated source settles all external text bounds before
        // inference/body I/O; the final source carries the exact request digest.
        let preliminary = CaptureSource::new(
            request.namespace,
            request.key,
            request.reference,
            request.session,
            request.revision,
            [0; 32],
        )
        .map_err(|error| Error::InvalidInput(error.to_string()))?;
        if touchstone.as_ref().is_some_and(|input| {
            input
                .references()
                .iter()
                .any(|reference| reference.id() == preliminary.node_id())
        }) {
            return Err(Error::InvalidInput(
                "a touchstone cannot reference its own owner".into(),
            ));
        }
        if request.links.len() > MAX_CAPTURE_EDGES {
            return Err(Error::InvalidInput(format!(
                "capture links exceed {MAX_CAPTURE_EDGES} edges"
            )));
        }
        let mut links = request.links.to_vec();
        for link in &mut links {
            link.validate()?;
            if link.to == preliminary.node_id() {
                return Err(Error::InvalidInput(
                    "capture link cannot target itself".into(),
                ));
            }
            // A signed zero is not meaningful association strength. Normalize
            // it before identity binding and persistence.
            if link.weight == 0.0 {
                link.weight = 0.0;
            }
        }
        links.sort_unstable_by_key(|link| link.to);
        if links.windows(2).any(|pair| pair[0].to == pair[1].to) {
            return Err(Error::InvalidInput("duplicate capture link target".into()));
        }
        let legacy_candidate_digest = capture_request_digest(
            &preliminary,
            &summary,
            request.body,
            &tags,
            stability,
            confidence,
            &links,
            CaptureRequestCodec::CaptureV1,
            Some(false),
        );
        let legacy_active_digest = capture_request_digest(
            &preliminary,
            &summary,
            request.body,
            &tags,
            stability,
            confidence,
            &links,
            CaptureRequestCodec::CaptureV1,
            Some(true),
        );
        let request_digest = match codec {
            CaptureRequestCodec::TouchstoneV1 => {
                let base = capture_request_digest(
                    &preliminary,
                    &summary,
                    request.body,
                    &tags,
                    stability,
                    confidence,
                    &links,
                    CaptureRequestCodec::CaptureV2,
                    None,
                );
                touchstone::request_digest(
                    base,
                    touchstone
                        .as_ref()
                        .expect("touchstone codec has typed input"),
                )
            }
            CaptureRequestCodec::CaptureV2 => capture_request_digest(
                &preliminary,
                &summary,
                request.body,
                &tags,
                stability,
                confidence,
                &links,
                CaptureRequestCodec::CaptureV2,
                None,
            ),
            CaptureRequestCodec::EpisodeV1 | CaptureRequestCodec::EpisodeV2 => legacy_active_digest,
            CaptureRequestCodec::CaptureV1 => unreachable!("new semantic requests use v2"),
        };
        let source = CaptureSource::new_with_codec(
            request.namespace,
            request.key,
            request.reference,
            request.session,
            request.revision,
            request_digest,
            codec,
        )
        .expect("preliminary capture source was validated");
        Ok(Self {
            source,
            legacy_candidate_digest,
            legacy_active_digest,
            summary,
            body: request.body,
            tags,
            links,
            touchstone,
            stability,
            confidence,
            origin_commit: request.origin_commit,
        })
    }

    fn replay_proof(&self) -> Result<CaptureReplayProof> {
        if self.source.request_codec() == CaptureRequestCodec::TouchstoneV1 {
            return CaptureReplayProof::touchstone(self.source.clone())
                .map_err(|error| Error::InvalidInput(error.to_string()));
        }
        CaptureReplayProof::semantic(
            self.source.clone(),
            self.legacy_candidate_digest,
            self.legacy_active_digest,
        )
        .map_err(|error| Error::InvalidInput(error.to_string()))
    }
}

fn capture_request_digest(
    source: &CaptureSource,
    summary: &NodeSummary,
    body: &[u8],
    tags: &BoundedTagSet,
    stability: Stability,
    confidence: Confidence,
    links: &[CaptureLink],
    codec: CaptureRequestCodec,
    legacy_active: Option<bool>,
) -> [u8; 32] {
    fn part(hash: &mut Sha256, bytes: &[u8]) {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    fn optional(hash: &mut Sha256, value: Option<&str>) {
        match value {
            Some(value) => {
                hash.update([1]);
                part(hash, value.as_bytes());
            }
            None => hash.update([0]),
        }
    }
    let mut hash = Sha256::new();
    hash.update(match codec {
        CaptureRequestCodec::CaptureV2 => b"mneme.capture.request.v2\0".as_slice(),
        CaptureRequestCodec::CaptureV1
        | CaptureRequestCodec::EpisodeV1
        | CaptureRequestCodec::EpisodeV2 => b"mneme.capture.request.v1\0".as_slice(),
        CaptureRequestCodec::TouchstoneV1 => {
            unreachable!("touchstone requests use an explicit separate codec")
        }
    });
    part(&mut hash, source.namespace().as_bytes());
    part(&mut hash, source.key().as_bytes());
    part(&mut hash, source.reference().as_bytes());
    optional(&mut hash, source.session());
    optional(&mut hash, source.revision());
    part(&mut hash, summary.as_str().as_bytes());
    part(&mut hash, body);
    hash.update((tags.len() as u64).to_be_bytes());
    for tag in tags.iter() {
        part(&mut hash, tag.as_bytes());
    }
    hash.update(stability.get().to_bits().to_be_bytes());
    hash.update(confidence.get().to_bits().to_be_bytes());
    if let Some(active) = legacy_active {
        hash.update([u8::from(active)]);
    }
    if !links.is_empty() {
        // Preserve the exact v1 digest for all historical no-link requests.
        // A nonempty request gets an unambiguous, domain-separated extension.
        hash.update(b"mneme.capture.links.v1\0");
        hash.update((links.len() as u64).to_be_bytes());
        for link in links {
            hash.update(link.to.0.to_bytes());
            hash.update([match link.kind {
                EdgeKind::Associative => 1,
                EdgeKind::Transition => 2,
                EdgeKind::DerivedFrom => 3,
                _ => unreachable!("capture link kind validated"),
            }]);
            hash.update(link.weight.to_bits().to_be_bytes());
        }
    }
    hash.finalize().into()
}

/// Canonical ingest values prepared before entering the mutation domain.
///
/// This boundary is deliberately separate from [`NodeInit`]: body bytes still
/// need to be embedded and possibly materialized into a [`BodyRef`], while all
/// caller-controlled node invariants must already be settled before we wait on
/// the mutation gate or invoke any adapter.
struct PreparedIngest<'a> {
    summary: NodeSummary,
    body: &'a [u8],
    body_ref: Option<BodyRef>,
    tags: BoundedTagSet,
    provenance: Provenance,
    stability: Stability,
    confidence: Confidence,
    origin_commit: Option<OriginCommit>,
}

impl<'a> TryFrom<Ingest<'a>> for PreparedIngest<'a> {
    type Error = Error;

    fn try_from(request: Ingest<'a>) -> Result<Self> {
        let summary = NodeSummary::new(request.summary)
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let tags = BoundedTagSet::try_from_iter(request.tags.iter().copied())
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let stability = Stability::new(request.stability)
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let confidence = Confidence::new(request.confidence)
            .map_err(|error| Error::InvalidInput(error.to_string()))?;

        // Provenance and explicit body references are sealed value objects:
        // accepting them here is the validation step. Invalid derived-source
        // sets and malformed references cannot be constructed through safe
        // public code or deserialized into these types.
        Ok(Self {
            summary,
            body: request.body,
            body_ref: request.body_ref,
            tags,
            provenance: request.provenance,
            stability,
            confidence,
            origin_commit: request.origin_commit,
        })
    }
}

/// Stable ordering evidence for one typed retrieval hit. Ranks are one-based;
/// absent evidence means that retrieval leg did not retain this node. Raw ANN,
/// BM25, graph-activation, and reranker scores intentionally stay private: they
/// are not calibrated against one another or across result lanes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetrievalEvidence {
    pub dense_rank: Option<NonZeroU16>,
    pub sparse_rank: Option<NonZeroU16>,
    pub graph_rank: Option<NonZeroU16>,
    pub rerank_rank: Option<NonZeroU16>,
}

/// One node in a typed retrieval lane. Containment in
/// [`RetrievalBatch::primary`] is the sole lane.
#[derive(Clone, Debug)]
pub struct RetrievalHit {
    pub node: Node,
    pub lane_rank: NonZeroU16,
    pub evidence: RetrievalEvidence,
    /// Actual winning traversal path, only when graph evidence admitted or
    /// promoted this hit and the caller opted into read-only provenance.
    /// Reaching a node is not by itself a graph contribution to the final result.
    pub graph_path: Option<Vec<TraversalHop>>,
}

/// Version of the rank-only retrieval policy. The string is deliberately
/// opaque to consumers: compare it for equality and treat a change as a new
/// ordering/admission contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetrievalPolicyVersion {
    /// Human-readable contract generation.
    pub contract: &'static str,
    /// SHA-256 over every runtime knob and enabled leg that changes ranking or
    /// result admission under this contract.
    pub fingerprint: [u8; 32],
}

impl RetrievalPolicyVersion {
    pub const fn as_str(&self) -> &'static str {
        self.contract
    }
}

/// Runtime identity of the retrieval indexes visible to this engine. This is a
/// structured transitional fingerprint: the embedding contract is exact, and
/// enabled retrieval legs expose stable semantic generations rather than
/// booleans that would equate behavior-changing backend upgrades. Physical
/// projection generations remain separate below.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexSetFingerprint {
    pub embedding: EmbeddingFingerprint,
    pub vector_semantics: &'static str,
    pub lexical_semantics: Option<&'static str>,
    pub reranker_semantics: Option<&'static str>,
}

/// One backend-supplied projection watermark. Tagged retrieval supplies the
/// verified tag-membership generation; paths without a trustworthy generation
/// leave the bounded list empty rather than manufacturing one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionWatermark {
    pub projection: String,
    pub watermark: String,
}

/// Construction-validated projection watermarks. The private storage makes the
/// eight-item protocol limit an invariant instead of a comment on a public
/// `Vec` that any caller could violate after construction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectionWatermarks(Vec<ProjectionWatermark>);

impl ProjectionWatermarks {
    pub fn new(values: Vec<ProjectionWatermark>) -> std::result::Result<Self, WatermarkLimitError> {
        if values.len() > MAX_PROJECTION_WATERMARKS {
            return Err(WatermarkLimitError {
                provided: values.len(),
            });
        }
        Ok(Self(values))
    }

    pub const fn as_slice(&self) -> &[ProjectionWatermark] {
        self.0.as_slice()
    }

    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub const fn len(&self) -> usize {
        self.0.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatermarkLimitError {
    pub provided: usize,
}

impl std::fmt::Display for WatermarkLimitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "retrieval policy stamp has {} projection watermarks; maximum is {MAX_PROJECTION_WATERMARKS}",
            self.provided
        )
    }
}

impl std::error::Error for WatermarkLimitError {}

/// Policy/index identity attached to one typed batch. At most eight projection
/// watermarks may be carried. Untagged retrieval currently carries none; tagged
/// retrieval carries the adapter's verified `tag-membership` generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetrievalPolicyStamp {
    pub retrieval_policy: RetrievalPolicyVersion,
    pub index_set: IndexSetFingerprint,
    pub projection_watermarks: ProjectionWatermarks,
}

/// Fixed-size identity of the backend behavior that can affect retrieval. A
/// configured but disabled optional leg is deliberately absent: it cannot have
/// influenced this batch, while enabling it produces a different component.
fn retrieval_index_semantics_fingerprint(
    vector: &str,
    lexical: Option<&str>,
    lexical_enabled: bool,
    reranker: Option<&str>,
    reranker_enabled: bool,
) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    debug_assert!(!lexical_enabled || lexical.is_some());
    debug_assert!(!reranker_enabled || reranker.is_some());
    let mut hash = Sha256::new();
    hash.update(b"mneme:retrieval-index-semantics-v1\0");
    for semantic_id in [
        Some(vector),
        lexical_enabled.then_some(lexical).flatten(),
        reranker_enabled.then_some(reranker).flatten(),
    ] {
        match semantic_id {
            Some(value) => {
                hash.update([1]);
                hash.update((value.len() as u64).to_le_bytes());
                hash.update(value.as_bytes());
            }
            None => hash.update([0]),
        }
    }
    hash.finalize().into()
}

/// Validate and canonicalize query tags before invoking the embedder. The core
/// request repeats this check at the adapter boundary; doing it here prevents a
/// malformed 33-tag or duplicate-tag request from buying model work first.
fn normalize_tagged_query_tags<'a>(tags: &[&'a str]) -> Result<Vec<&'a str>> {
    if tags.is_empty() || tags.len() > MAX_TAGGED_QUERY_TAGS {
        return Err(Error::InvalidInput(format!(
            "tagged retrieval requires 1..={MAX_TAGGED_QUERY_TAGS} query tags; got {}",
            tags.len()
        )));
    }
    let mut normalized = Vec::with_capacity(tags.len());
    for tag in tags {
        validate_tag(tag).map_err(|error| {
            Error::InvalidInput(format!(
                "invalid tagged retrieval query tag {tag:?}: {error}"
            ))
        })?;
        normalized.push(*tag);
    }
    normalized.sort_unstable();
    if let Some(duplicate) = normalized
        .windows(2)
        .find_map(|pair| (pair[0] == pair[1]).then_some(pair[0]))
    {
        return Err(Error::InvalidInput(format!(
            "tagged retrieval query tags must be unique; duplicate tag {duplicate:?}"
        )));
    }
    Ok(normalized)
}

/// Fingerprint only the declared tagged strategy and quota topology. Observed
/// work (inspected/matching counts and pivots) belongs in result metadata, not
/// policy identity.
fn tagged_coverage_shape_fingerprint(lanes: &[TaggedAnnLane]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let mut hash = Sha256::new();
    hash.update(b"mneme:tagged-coverage-shape:v1\0");
    hash.update((lanes.len() as u64).to_le_bytes());
    for lane in lanes {
        let strategy = lane.seed_coverage.strategy_id();
        hash.update((strategy.len() as u64).to_le_bytes());
        hash.update(strategy.as_bytes());
        let physical = match &lane.seed_coverage {
            TaggedSeedCoverage::ExactCosine => None,
            TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter { physical, .. }
            | TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter { physical, .. } => {
                Some(physical.as_slice())
            }
        };
        match physical {
            None => hash.update([0]),
            Some(physical) => {
                hash.update([1]);
                hash.update((physical.len() as u64).to_le_bytes());
                for item in physical {
                    let status = item.status.as_str();
                    hash.update((status.len() as u64).to_le_bytes());
                    hash.update(status.as_bytes());
                    hash.update((item.hnsw_quota as u64).to_le_bytes());
                    hash.update((item.tag_samples.len() as u64).to_le_bytes());
                    for sample in &item.tag_samples {
                        hash.update([sample.query_tag_index]);
                        hash.update((sample.quota as u64).to_le_bytes());
                    }
                }
            }
        }
    }
    hash.finalize().into()
}

/// Typed retrieval output for one semantic search population.
#[derive(Clone, Debug)]
pub struct RetrievalBatch {
    pub primary: Vec<RetrievalHit>,
    pub stamp: RetrievalPolicyStamp,
    /// Present only for tagged retrieval. Coverage describes the bounded seed
    /// search, not the final graph-expanded hit set: graph traversal may add
    /// untagged nodes but cannot upgrade a partial seed search to exact.
    pub tagged: Option<TaggedRetrievalMetadata>,
    /// Present only for explicit query-local routing opt-in; not learning authority.
    pub routing: Option<RoutingOutcome>,
}

/// Honest bounded-work metadata retained beside a tagged retrieval result.
/// Every requested lane has coverage even when it returned no hits.
#[derive(Clone, Debug, PartialEq)]
pub struct TaggedRetrievalMetadata {
    pub work: TaggedAnnWork,
    pub primary_seed_coverage: TaggedSeedCoverage,
}

impl TaggedRetrievalMetadata {
    pub fn is_partial(&self) -> bool {
        self.primary_seed_coverage.is_partial()
    }
}

/// Lane label retained by the temporary flat compatibility API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetrievalLane {
    Primary,
}

/// A node surfaced by the legacy flat retrieval adapter. `score` is private
/// evidence from the node's own lane and must not be compared across `lane`.
/// New callers should use [`RetrievalBatch`] and its rank evidence instead.
#[derive(Clone, Debug)]
pub struct Retrieved {
    pub node: Node,
    pub score: f32,
    pub lane: RetrievalLane,
}

#[derive(Clone, Debug)]
struct RankedRetrievalHit {
    node: Node,
    score: f32,
    evidence: RetrievalEvidence,
    graph_path: Option<Vec<TraversalHop>>,
}

#[derive(Clone, Debug)]
struct RankedRetrievalBatch {
    primary: Vec<RankedRetrievalHit>,
    stamp: RetrievalPolicyStamp,
    tagged: Option<TaggedRetrievalMetadata>,
    routing: Option<RoutingOutcome>,
}

#[derive(Clone, Copy)]
struct RetrievalPolicyInput<'a> {
    effective_k: usize,
    budget: Budget,
    status: StatusFilter,
    tags: &'a [&'a str],
    sparse_enabled: bool,
    reranker_enabled: bool,
    tagged_request: Option<&'a TaggedAnnRequest<'a>>,
    tagged_lanes: Option<&'a [TaggedAnnLane]>,
    tagged_projection_generation: Option<&'a str>,
}

impl RankedRetrievalBatch {
    fn into_public(self) -> RetrievalBatch {
        RetrievalBatch {
            primary: into_typed_lane(self.primary),
            stamp: self.stamp,
            tagged: self.tagged,
            routing: self.routing,
        }
    }

    /// Temporary flat adapter; the one primary lane has a single score order.
    fn into_compatibility_flat(self, total_cap: usize) -> Vec<Retrieved> {
        self.primary
            .into_iter()
            .take(total_cap)
            .map(|hit| Retrieved {
                node: hit.node,
                score: hit.score,
                lane: RetrievalLane::Primary,
            })
            .collect()
    }
}

fn into_typed_lane(hits: Vec<RankedRetrievalHit>) -> Vec<RetrievalHit> {
    hits.into_iter()
        .enumerate()
        .map(|(index, hit)| RetrievalHit {
            node: hit.node,
            lane_rank: rank_from_index(index),
            evidence: hit.evidence,
            graph_path: hit.graph_path,
        })
        .collect()
}

/// A recall hit plus a shallow one-hop expansion — see [`Memory::recall_expanded`].
/// Adds immediate associations to ranked retrieval hits. `retrieve` already
/// includes graph spreading; this view hydrates one-hop neighbors without a walk.
#[derive(Clone, Debug)]
pub struct RecallHit {
    pub node: Node,
    pub score: f32,
    /// The hit's strongest neighbours (deduped against the hit set and each other),
    /// empty for hits below the expansion rank.
    pub neighbors: Vec<RecalledNeighbor>,
}

/// One neighbour surfaced by [`Memory::recall_expanded`]: the node plus the edge
/// that reached it (mirrors [`ports::Neighbor`] but resolves the node).
#[derive(Clone, Debug)]
pub struct RecalledNeighbor {
    pub node: Node,
    pub kind: EdgeKind,
    pub incoming: bool,
    pub weight: f32,
}

/// Input to the reconciliation pass — see [`Memory::reconciliation_triage`].
#[derive(Clone, Debug)]
pub struct ReconciliationTriage {
    /// Open contradictions, observation-count desc, each tagged with its nodes'
    /// current communities.
    pub contradictions: Vec<TriagedContradiction>,
    /// The derived "community X conflicts with community Y" aggregate, count desc.
    pub cluster_conflicts: Vec<ClusterConflict>,
}

#[derive(Clone, Debug)]
pub struct TriagedContradiction {
    pub between: UnorderedPair<NodeId>,
    pub observations: u32,
    /// Current cluster of each node in `between`, same order.
    pub clusters: (ClusterId, ClusterId),
}

/// Conflict aggregated to the community level through current membership.
/// `clusters.0 == clusters.1` is an intra-community conflict.
#[derive(Clone, Debug)]
pub struct ClusterConflict {
    pub clusters: UnorderedPair<ClusterId>,
    /// Summed observations across all node-pairs spanning these communities.
    pub observations: u32,
    /// How many distinct node-pairs contribute.
    pub pairs: u32,
}

/// A maintenance snapshot — see [`Memory::status`]. Lets a caller see mechanically
/// whether anything's due before running passes blindly.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub nodes: usize,
    /// Immutable episode editions, excluded from semantic lifecycle counters.
    pub episode_editions: usize,
    /// Stable episode roots (initial editions).
    pub episodes: usize,
    pub active: usize,
    pub archived: usize,
    /// Contradictions flagged but not yet reconciled (drain via the triage pass).
    pub open_contradictions: usize,
    /// Pairs flagged redundant, awaiting a `merge` decision.
    pub open_merge_candidates: usize,
    /// Edges carrying banked interference for the next decay sweep.
    pub edge_decay_pending: usize,
}

/// What a [`Memory::decay_sweep`] did, for logging/observability.
#[derive(Default, Clone, Copy, Debug)]
pub struct DecayReport {
    pub edges_decayed: usize,
    pub edge_conflicts: usize,
    pub edge_pages: usize,
}

/// What a [`Memory::prune_dense`] pass did, for logging/observability.
#[derive(Default, Clone, Copy, Debug)]
pub struct PruneReport {
    /// Total edges physically deleted.
    pub pruned: usize,
    /// Subset deleted specifically to enforce the incident-degree cap.
    pub capacity_pruned: usize,
    /// Weak-edge deletion plans skipped after a concurrent reinforcement/update.
    pub weak_conflicts: usize,
    /// Hubs that remained over target after the bounded retry budget.
    pub contended_hubs: usize,
    pub edge_pages: usize,
    pub hub_pages: usize,
    pub chunks: usize,
}

/// Resolves a [`BodyRef`] to a [`BodyStore`] by URI scheme.
#[derive(Default, Clone)]
struct BodyRegistry {
    stores: HashMap<&'static str, Arc<dyn BodyStore>>,
}

impl BodyRegistry {
    fn register(&mut self, store: Arc<dyn BodyStore>) {
        self.stores.insert(store.scheme(), store);
    }

    fn store_for(&self, scheme: &str) -> Result<&Arc<dyn BodyStore>> {
        self.stores
            .get(scheme)
            .ok_or_else(|| Error::Body(format!("no body store for scheme {scheme:?}")))
    }

    async fn resolve(&self, body: &BodyRef) -> Result<Vec<u8>> {
        self.store_for(body.scheme())?.get(body).await
    }

    async fn resolve_range(
        &self,
        body: &BodyRef,
        offset: u64,
        max_bytes: usize,
    ) -> Result<BodyChunk> {
        self.store_for(body.scheme())?
            .get_range(body, offset, max_bytes)
            .await
    }

    async fn resolve_prefix(&self, body: &BodyRef, max_bytes: usize) -> Result<(Vec<u8>, bool)> {
        let chunk = self.resolve_range(body, 0, max_bytes).await?;
        let truncated = chunk.next_offset.is_some();
        Ok((chunk.bytes, truncated))
    }

    async fn put(&self, scheme: &str, bytes: &[u8]) -> Result<BodyRef> {
        self.store_for(scheme)?.put(bytes).await
    }

    async fn delete(&self, body: &BodyRef) -> Result<()> {
        self.store_for(body.scheme())?.delete(body).await
    }
}

/// The memory itself: the composition root for the ports.
pub struct Memory {
    graph: Arc<dyn GraphStore>,
    vectors: Arc<dyn VectorIndex>,
    /// Optional sparse summary index. Production hosts wire the same backend as
    /// graph/vector/lexical; leaving it absent preserves a dense-only adapter.
    lexical: Option<Arc<dyn LexicalIndex>>,
    traversal: Arc<dyn Traversal>,
    embedder: Arc<dyn Embedder>,
    clock: Arc<dyn Clock>,
    node_ids: Arc<dyn NodeIdSource>,
    /// Optional cross-encoder reranker (see [`Memory::with_reranker`]); `None`
    /// keeps retrieval free of per-result model inference.
    reranker: Option<Arc<dyn Reranker>>,
    bodies: BodyRegistry,
    cfg: Config,
    /// Serializes public mutations inside one long-lived engine. Retrieval keeps
    /// its expensive read-only ranking outside this gate and acquires it briefly
    /// to revalidate lifecycle state without racing an in-process mutation.
    /// Backends still own durable/cross-process transaction semantics.
    mutation_gate: Arc<tokio::sync::Mutex<()>>,
}

/// Exclusive proof that no engine mutation or detached maintenance task owns
/// the mutation domain.
///
/// A host may retain this while it checkpoints and closes a backend for lease
/// handoff, but only after independently fencing new request admission. The
/// guard does not account for read-only adapter work; hosts must combine it
/// with the backend's activity fence.
pub struct MutationQuiescenceGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

/// Owns a cancellation-surviving task's store capability and mutation lock in
/// an explicit drop order. The graph must disappear before the lock becomes
/// available to a host preparing the backing file for handoff, including when
/// the task panics or its runtime future is torn down.
struct DetachedMutationAuthority {
    graph: Option<Arc<dyn GraphStore>>,
    mutation: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl DetachedMutationAuthority {
    fn new(graph: Arc<dyn GraphStore>, mutation: tokio::sync::OwnedMutexGuard<()>) -> Self {
        Self {
            graph: Some(graph),
            mutation: Some(mutation),
        }
    }

    fn graph(&self) -> &dyn GraphStore {
        self.graph
            .as_deref()
            .expect("detached mutation graph authority remains live")
    }
}

impl Drop for DetachedMutationAuthority {
    fn drop(&mut self) {
        drop(self.graph.take());
        drop(self.mutation.take());
    }
}

fn assert_send<T: Send>() {}
const _: fn() = assert_send::<MutationQuiescenceGuard>;
const _: fn() = assert_send::<DetachedMutationAuthority>;

/// One stored edge together with the current canonical node at its far end.
/// The node has been lifecycle-revalidated after shortlist selection, so a
/// caller can render its summary without issuing a point read per edge.
#[derive(Clone, Debug)]
pub struct ResolvedNeighbor {
    pub neighbor: Neighbor,
    pub node: Node,
}

/// One raw stored edge together with its positional canonical endpoint, when
/// that endpoint still exists. Diagnostic callers retain the edge even when
/// `node` is `None`; user-facing traversal can instead use
/// [`Memory::resolved_neighbors_scoped`] to omit missing or ineligible rows.
#[derive(Clone, Debug)]
pub struct HydratedNeighbor {
    pub neighbor: Neighbor,
    pub node: Option<Node>,
}

impl Memory {
    /// Wire the ports together. Panics if the embedder and vector index disagree
    /// on dimension — that's a wiring bug, not a runtime condition.
    pub fn new(
        graph: Arc<dyn GraphStore>,
        vectors: Arc<dyn VectorIndex>,
        traversal: Arc<dyn Traversal>,
        embedder: Arc<dyn Embedder>,
        clock: Arc<dyn Clock>,
        cfg: Config,
    ) -> Self {
        let fingerprint = embedder.fingerprint();
        assert_eq!(
            fingerprint.dimension,
            embedder.dim(),
            "embedder fingerprint/dimension mismatch"
        );
        assert_eq!(
            embedder.dim(),
            vectors.dim(),
            "embedder/vector-index dimension mismatch"
        );
        Self {
            graph,
            vectors,
            lexical: None,
            traversal,
            embedder,
            clock,
            node_ids: Arc::new(RandomNodeIdSource),
            reranker: None,
            bodies: BodyRegistry::default(),
            cfg,
            mutation_gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Try to acquire exclusive mutation quiescence without waiting.
    ///
    /// `None` means a public mutation or cancellation-surviving maintenance
    /// task already owns or is queued inside the mutation domain. This is an
    /// instantaneous engine proof, not a request-admission fence: an owning
    /// host must prevent new callers before relying on the returned guard.
    pub fn try_mutation_quiescence(&self) -> Option<MutationQuiescenceGuard> {
        Arc::clone(&self.mutation_gate)
            .try_lock_owned()
            .ok()
            .map(|guard| MutationQuiescenceGuard { _guard: guard })
    }

    /// Register a body store, keyed by its [`BodyStore::scheme`]. Returns `self`
    /// for chaining at construction.
    pub fn with_body_store(mut self, store: Arc<dyn BodyStore>) -> Self {
        self.bodies.register(store);
        self
    }

    /// Add sparse summary retrieval. Dense and lexical scores are not
    /// commensurate, so retrieval combines only their ranks via weighted RRF.
    pub fn with_lexical_index(mut self, lexical: Arc<dyn LexicalIndex>) -> Self {
        self.lexical = Some(lexical);
        self
    }

    /// Wire an optional cross-encoder [`Reranker`]. With one set, retrieval
    /// re-scores its bounded candidate set jointly against the query before the
    /// near-duplicate filter — a precision stage on top of the spread's recall.
    /// Without one, retrieval does no per-result model inference.
    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = Some(reranker);
        self
    }

    /// Override the production random-ULID source. Intended for reproducible
    /// evaluation and deterministic tests; hosts should normally keep the
    /// coordination-free default.
    pub fn with_node_id_source(mut self, source: Arc<dyn NodeIdSource>) -> Self {
        self.node_ids = source;
        self
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    // ---- hot path -----------------------------------------------------------

    /// Guarded in-place semantic curation. Inference follows eligibility and
    /// snapshot admission; the backend compares again and publishes the summary
    /// and retrieval projections together. Capture proof and body stay intact.
    pub async fn edit_summary(
        &self,
        id: NodeId,
        expected: &mneme_core::SummarySnapshotDigest,
        summary: NodeSummary,
    ) -> Result<Node> {
        let _mutation = self.mutation_gate.lock().await;
        let node = self.graph.get_node(id).await?.ok_or(Error::NotFound)?;
        Self::require_semantic(&node)?;
        let store = self.touchstone_store()?;
        if mneme_core::SummarySnapshot::from_node(store.database_id()?, &node)?.digest()
            != *expected
        {
            return Err(Error::Conflict(
                "summary snapshot changed; GET the node again".into(),
            ));
        }
        if store.get_touchstone(id).await?.is_some() {
            return Err(Error::Conflict(
                "summary editing cannot replace a touchstone owner".into(),
            ));
        }
        if node.summary() == summary.as_str() {
            return Err(Error::InvalidInput("summary is unchanged".into()));
        }
        let embedding = self.embed_one(summary.as_str()).await?;
        self.graph
            .compare_replace_node_summary(id, expected, &summary, &embedding)
            .await
    }

    /// Edit a semantic body without re-embedding or changing its capture proof.
    /// Publish the fresh blob durably before CAS. Cancellation, a conflict, or an
    /// ambiguous acknowledgement may leave an orphan, as with capture; neither
    /// old nor unpublished blobs are reclaimed by this operation.
    pub async fn edit_body(
        &self,
        id: NodeId,
        expected: &mneme_core::BodyRevision,
        body: &[u8],
    ) -> Result<Node> {
        if body.len() > MAX_CAPTURE_BODY_BYTES {
            return Err(Error::InvalidInput(format!(
                "body exceeds {MAX_CAPTURE_BODY_BYTES} bytes"
            )));
        }
        let _mutation = self.mutation_gate.lock().await;
        let node = self.graph.get_node(id).await?.ok_or(Error::NotFound)?;
        Self::require_semantic(&node)?;
        if node.body_revision() != *expected {
            return Err(Error::Conflict("body revision changed".into()));
        }
        if let Some(store) = self.graph.touchstones() {
            if store.get_touchstone(id).await?.is_some() {
                return Err(Error::Conflict(
                    "body editing cannot replace a touchstone owner".into(),
                ));
            }
        }
        let replacement = self.bodies.put(self.cfg.default_body_scheme, body).await?;
        self.graph
            .compare_replace_node_body(id, expected, &replacement)
            .await
    }

    /// Capture one source-keyed memory with a backend-atomic canonical/vector
    /// commit. Exact replay returns the existing id; key reuse with changed
    /// content conflicts. The proof lasts only while the captured node exists.
    pub async fn capture(&self, request: Capture<'_>) -> Result<CaptureResult> {
        let request = PreparedCapture::try_from(request)?;
        let proof = request.replay_proof()?;
        let _mutation = self.mutation_gate.lock().await;
        if let Some(id) = self.graph.lookup_capture(&proof).await? {
            // The persisted source binds the *original* request. Explicit
            // curation may since have edited this canonical node, so matching
            // mutable summary/body against the original would reject an honest
            // replay after migration or supersession.
            let node = self.graph.get_node(id).await?.ok_or_else(|| {
                Error::Conflict(format!("capture node {} vanished during replay", id.0))
            })?;
            Self::require_semantic(&node)?;
            self.bodies
                .resolve_range(node.body(), 0, 0)
                .await
                .map_err(|error| {
                    Error::Conflict(format!("capture body unavailable on replay: {error}"))
                })?;
            return Ok(CaptureResult { id, replayed: true });
        }

        if let Some(touchstone) = request.touchstone.as_ref() {
            let db_id = self.touchstone_store()?.database_id()?;
            if touchstone
                .references()
                .iter()
                .any(|reference| reference.db_id() != db_id)
            {
                return Err(Error::InvalidInput(
                    "touchstone references must belong to this logical database".into(),
                ));
            }
        }

        let embedding = self.embed_one(request.summary.as_str()).await?;
        // BodyStore::put completes durable body publication before the backend
        // can point a canonical row at it. A cancelled/precommit call can leave
        // an orphan body, but cannot leave a false successful capture proof.
        let body_ref = self
            .bodies
            .put(self.cfg.default_body_scheme, request.body)
            .await?;
        let id = request.source.node_id();
        let node = Node::new(NodeInit {
            id,
            summary: request.summary,
            body: body_ref.clone(),
            tags: request.tags,
            provenance: Provenance::External {
                source: request.source,
            },
            stability: request.stability,
            confidence: request.confidence,
            status: NodeStatus::Active,
            created: self.clock.now(),
        })
        .with_body_ownership(BodyOwnership::Managed)
        .with_origin_commit(request.origin_commit);
        let edges: Vec<_> = request
            .links
            .iter()
            .map(|link| Edge::new(id, link.to, link.weight, link.kind, node.created()))
            .collect();
        // Optional discovery cannot erase a valid sourced claim. Mandatory
        // embedding/body/commit failures still propagate. Replay exited above.
        let priors = self
            .capture_similarity_priors(&node, &embedding, &edges)
            .await;
        let prior_budget = mneme_core::ports::CapturePriorBudget::new(self.cfg.similarity_link_cap);
        let outcome = if let Some(touchstone) = request.touchstone.as_ref() {
            self.graph
                .commit_capture_with_touchstone(
                    &node,
                    &embedding,
                    &edges,
                    &priors,
                    prior_budget,
                    &proof,
                    Some(touchstone),
                )
                .await?
        } else {
            // The predecessor path remains exact, including optional authored
            // links and discovery priors. Typed metadata is never inferred
            // from a bare tag.
            self.graph
                .commit_capture_with_priors(
                    &node,
                    &embedding,
                    &edges,
                    &priors,
                    prior_budget,
                    &proof,
                )
                .await?
        };
        match outcome {
            CaptureCommitOutcome::Applied => Ok(CaptureResult {
                id,
                replayed: false,
            }),
            CaptureCommitOutcome::AlreadyApplied => {
                // Another Memory instance won the backend CAS after our probe.
                // This body was not published and is safe to discard.
                self.bodies.delete(&body_ref).await?;
                Ok(CaptureResult { id, replayed: true })
            }
        }
    }

    /// Materialize a node: store its body, embed its summary, persist it, index
    /// the vector, and seed associative edges from embedding similarity (the
    /// prior used before any co-activation data exists). Returns the new id.
    pub async fn ingest(&self, req: Ingest<'_>) -> Result<NodeId> {
        let req = PreparedIngest::try_from(req)?;
        let _mutation = self.mutation_gate.lock().await;
        self.ingest_with_gate_held(req).await
    }

    /// Ingest while the caller owns `mutation_gate`.
    async fn ingest_with_gate_held(&self, req: PreparedIngest<'_>) -> Result<NodeId> {
        let PreparedIngest {
            summary,
            body,
            body_ref,
            tags,
            provenance,
            stability,
            confidence,
            origin_commit,
        } = req;
        let now = self.clock.now();
        // Inference comes first: if it fails, no external body has been written.
        let embedding = self.embed_one(summary.as_str()).await?;
        let owns_body = body_ref.is_none();
        let body_ref = match body_ref {
            Some(r) => r,
            None => self.bodies.put(self.cfg.default_body_scheme, body).await?,
        };
        let id = self.node_ids.next_id();
        let node = Node::new(NodeInit {
            id,
            summary,
            body: body_ref,
            tags,
            provenance,
            stability,
            confidence,
            status: NodeStatus::Active,
            created: now,
        })
        .with_body_ownership(if owns_body {
            BodyOwnership::Managed
        } else {
            BodyOwnership::Borrowed
        })
        .with_origin_commit(origin_commit);
        // Plan every similarity edge before publishing the first one. Besides
        // keeping passage-anchor inference out of the edge-write prefix, this
        // gives compensation an exact bounded delete set even when an adapter
        // returns an ambiguous error after committing an edge write.
        let mut planned_edges = Vec::new();
        let mut attempted_edges = Vec::new();
        let result: Result<()> = async {
            self.graph.put_node(&node).await?;
            self.vectors.upsert(id, &embedding).await?;
            planned_edges = self
                .plan_similarity_edges(id, &embedding, body, now)
                .await?;
            for edge in &planned_edges {
                // Record before calling the adapter: an error may mean the write
                // committed but its acknowledgement was lost. Never include the
                // unattempted suffix, which another store client could populate
                // independently after planning.
                attempted_edges.push((edge.from, edge.to));
                self.graph.put_edge(edge).await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            // Cross-adapter atomicity is implemented as a compensating saga: the
            // graph/vector backend and filesystem cannot share one transaction,
            // but a failed ingest must not leave a visible half-memory behind.
            // Delete the complete attempted prefix, not merely writes whose
            // acknowledgement arrived: the failing adapter call may have
            // committed and lost its response. This remains O(link cap), never
            // O(E), without touching a key no ingest write ever attempted.
            let mut edge_rollback_failures = Vec::new();
            for (from, to) in &attempted_edges {
                if let Err(rollback) = self.graph.delete_edge(*from, *to).await {
                    edge_rollback_failures.push(format!("edge {} -> {}: {rollback}", from.0, to.0));
                }
            }
            // Cleanup is dependency ordered. If an edge might remain, retain its
            // node, vector, and body; if vector removal is ambiguous, retain the
            // node/body; if node removal is ambiguous, retain the body. Leaking a
            // retryable object is safer than manufacturing a dangling reference.
            if !edge_rollback_failures.is_empty() {
                return Err(Error::Backend(format!(
                    "ingest failed ({error}); rollback incomplete: {}",
                    edge_rollback_failures.join("; ")
                )));
            }
            if let Err(rollback) = self.vectors.remove(id).await {
                return Err(Error::Backend(format!(
                    "ingest failed ({error}); rollback incomplete: vector {}: {rollback}",
                    id.0
                )));
            }
            if let Err(rollback) = self.graph.delete_node(id).await {
                return Err(Error::Backend(format!(
                    "ingest failed ({error}); rollback incomplete: node {}: {rollback}",
                    id.0
                )));
            }
            if owns_body && let Err(rollback) = self.bodies.delete(node.body()).await {
                return Err(Error::Backend(format!(
                    "ingest failed ({error}); rollback incomplete: body {}: {rollback}",
                    node.body().as_str()
                )));
            }
            return Err(error);
        }
        Ok(id)
    }

    /// Retrieve with the default [`Config::budget`].
    pub async fn retrieve(&self, query: &str) -> Result<Vec<Retrieved>> {
        self.retrieve_with(query, self.cfg.budget).await
    }

    /// [`Memory::retrieve`] with an explicit spread budget. Searches active nodes
    /// only; use [`Memory::retrieve_seeded`] to widen the status set.
    pub async fn retrieve_with(&self, query: &str, budget: Budget) -> Result<Vec<Retrieved>> {
        self.retrieve_seeded(query, self.cfg.ann_k, budget, StatusFilter::default(), &[])
            .await
    }

    /// Retrieve a typed primary lane with the default policy.
    pub async fn retrieve_batch(&self, query: &str) -> Result<RetrievalBatch> {
        self.retrieve_batch_with(query, self.cfg.budget).await
    }

    /// [`Memory::retrieve_batch`] with an explicit spread budget.
    pub async fn retrieve_batch_with(&self, query: &str, budget: Budget) -> Result<RetrievalBatch> {
        self.retrieve_batch_seeded(query, self.cfg.ann_k, budget, StatusFilter::default(), &[])
            .await
    }

    /// Embed the query → dense+sparse RRF seeds → spreading activation →
    /// ranked results.
    /// `status` selects active or explicitly requested archived nodes. Retrieval
    /// is pure: returned nodes do not record exposure or train topology. The host
    /// must pack and emit context; grounded feedback arrives explicitly through
    /// [`Memory::apply_feedback`]. `k` is the ANN seed count.
    /// The flat compatibility result cannot represent tagged seed omissions, so
    /// this method rejects nonempty `tags`; use [`Memory::retrieve_batch_seeded`]
    /// for tagged retrieval.
    pub async fn retrieve_seeded(
        &self,
        query: &str,
        k: usize,
        budget: Budget,
        status: StatusFilter,
        tags: &[&str],
    ) -> Result<Vec<Retrieved>> {
        if !tags.is_empty() {
            return Err(Error::InvalidInput(
                "tagged retrieval requires retrieve_batch_seeded because the flat compatibility API cannot represent seed coverage"
                    .into(),
            ));
        }
        let ranked = self
            .retrieve_ranked_seeded(query, k, budget, status, tags, false, None)
            .await?;
        Ok(ranked.into_compatibility_flat(budget.max_nodes.min(MAX_RETRIEVAL_RANK)))
    }

    /// Typed seeded retrieval over the requested lifecycle scope.
    /// Nonempty `tags` restrict dense seeds to nodes carrying at least one tag
    /// while preserving exact/partial seed coverage and aggregate work. Tagged
    /// mode is dense-only because post-filtering sparse top-k would violate its
    /// completeness contract; graph expansion may still add untagged nodes.
    pub async fn retrieve_batch_seeded(
        &self,
        query: &str,
        k: usize,
        budget: Budget,
        status: StatusFilter,
        tags: &[&str],
    ) -> Result<RetrievalBatch> {
        Ok(self
            .retrieve_ranked_seeded(query, k, budget, status, tags, false, None)
            .await?
            .into_public())
    }

    /// The same pure retrieval policy, with opt-in actual graph-path provenance.
    /// This records no exposure, creates no receipt, and changes no learning state.
    pub async fn retrieve_batch_seeded_observed(
        &self,
        query: &str,
        k: usize,
        budget: Budget,
        status: StatusFilter,
        tags: &[&str],
    ) -> Result<RetrievalBatch> {
        Ok(self
            .retrieve_ranked_seeded(query, k, budget, status, tags, true, None)
            .await?
            .into_public())
    }

    /// Bounded per-call signed route ordering and positive conditional target entry.
    /// Never persisted or interpreted as feedback. Bindings are checked under the
    /// mutation gate before admission and again after reranking; conditional entry
    /// carries recommendation provenance, not a fabricated graph path.
    #[expect(clippy::too_many_arguments)]
    pub async fn retrieve_batch_seeded_routed(
        &self,
        query: &str,
        k: usize,
        budget: Budget,
        status: StatusFilter,
        tags: &[&str],
        hints: &[RoutingHint],
    ) -> Result<RetrievalBatch> {
        // Match the public whole-array byte contract before any binding GETs.
        // Empty opt-in still requests provenance, but no signed prefix survives.
        let admitted = routing_hints_admissible(hints);
        let preignored = if admitted { 0 } else { hints.len() };
        let hints = if admitted { hints } else { &[] };
        let mut batch = self
            .retrieve_ranked_seeded(query, k, budget, status, tags, true, Some(hints))
            .await?;
        if let Some(outcome) = batch.routing.as_mut() {
            outcome.diagnostics.ignored += preignored;
        }
        Ok(batch.into_public())
    }

    async fn retrieve_ranked_seeded(
        &self,
        query: &str,
        k: usize,
        budget: Budget,
        status: StatusFilter,
        tags: &[&str],
        trace: bool,
        routing_hints: Option<&[RoutingHint]>,
    ) -> Result<RankedRetrievalBatch> {
        let effective_k = k.min(budget.max_nodes).min(MAX_RETRIEVAL_RANK);
        let budget = Budget {
            max_nodes: budget.max_nodes.min(MAX_RETRIEVAL_RANK),
            ..budget
        };
        if !tags.is_empty() {
            return self
                .retrieve_tagged_ranked_seeded(
                    query,
                    effective_k,
                    budget,
                    status,
                    tags,
                    trace,
                    routing_hints,
                )
                .await;
        }
        let lexical_k = self.cfg.lexical_k.min(effective_k).min(MAX_LEXICAL_K);
        let sparse_enabled = lexical_k > 0 && self.lexical.is_some();
        let reranker_enabled = effective_k > 0 && self.reranker.is_some();
        let stamp = self.retrieval_policy_stamp(RetrievalPolicyInput {
            effective_k,
            budget,
            status,
            tags,
            sparse_enabled,
            reranker_enabled,
            tagged_request: None,
            tagged_lanes: None,
            tagged_projection_generation: None,
        });
        if effective_k == 0 {
            return Ok(RankedRetrievalBatch {
                primary: Vec::new(),
                stamp,
                tagged: None,
                routing: routing_hints.map(|hints| {
                    let mut out = RoutingOutcome::default();
                    out.diagnostics.ignored = hints.len();
                    out
                }),
            });
        }
        // The query side gets the embedder's query convention (e.g. BGE's
        // instruction prefix); node summaries were embedded as passages on ingest.
        let embedding = self.embedder.embed_query(query).await?;
        let mut dense_seeds = self.vectors.ann(&embedding, effective_k, status).await?;
        // Index adapters need not define an equal-score order. Rank fusion and
        // graph-root selection do, so stabilize each evidence lane before either
        // consumes its ranks.
        dense_seeds.sort_by(scored_order);
        dense_seeds.truncate(effective_k);
        let mut direct = dense_seeds.clone();
        let mut sparse_seeds = Vec::new();
        if sparse_enabled && let Some(lexical) = &self.lexical {
            sparse_seeds = lexical.search(query, lexical_k, status).await?;
            sparse_seeds.sort_by(scored_order);
            sparse_seeds.truncate(lexical_k);
            direct = reciprocal_rank_fuse(
                &dense_seeds,
                &sparse_seeds,
                self.cfg.rrf_constant,
                self.cfg.dense_weight,
                self.cfg.lexical_weight,
            );
        }
        let primary_evidence = EvidenceRanks {
            dense: ranked_ids(&dense_seeds),
            sparse: ranked_ids(&sparse_seeds),
            ..EvidenceRanks::default()
        };
        // Traversal and final ranking use a stable tie-break even when the
        // backing ANN leaves equal-distance ordering unspecified.
        direct.sort_by(scored_order);
        self.retrieve_pass(
            &embedding,
            query,
            direct,
            dense_seeds,
            sparse_seeds,
            primary_evidence,
            budget,
            status,
            stamp,
            None,
            trace,
            routing_hints,
            tags,
            effective_k > 0,
        )
        .await
    }

    async fn retrieve_tagged_ranked_seeded(
        &self,
        query: &str,
        effective_k: usize,
        budget: Budget,
        status: StatusFilter,
        tags: &[&str],
        trace: bool,
        routing_hints: Option<&[RoutingHint]>,
    ) -> Result<RankedRetrievalBatch> {
        let normalized_tags = normalize_tagged_query_tags(tags)?;
        if self.embedder.dim() == 0 || self.embedder.dim() > MAX_TAGGED_VECTOR_DIMENSION {
            return Err(Error::InvalidInput(format!(
                "tagged vector dimension must be in 1..={MAX_TAGGED_VECTOR_DIMENSION}; got {}",
                self.embedder.dim()
            )));
        }
        let limits = TaggedAnnWorkLimits::default();
        let primary_fallback = if effective_k == 0 {
            0
        } else if status == StatusFilter::ACTIVE {
            TAGGED_ACTIVE_FALLBACK_CANDIDATES
        } else {
            limits.max_fallback_candidates()
        };
        let lanes = vec![TaggedAnnLaneRequest::new(
            RetrievalLifecycleLane::Primary,
            status,
            effective_k,
            primary_fallback,
        )?];

        let projection_generation = self.vectors.tagged_projection_generation().ok_or_else(|| {
            Error::InvalidInput(
                "tagged retrieval is unsupported by this vector index because it declares no verified tag projection generation"
                    .into(),
            )
        })?;
        TaggedProjectionGeneration::new(projection_generation)?;
        let retrieval_semantic_generation = self.vectors.semantic_id();
        TaggedProjectionGeneration::new(retrieval_semantic_generation).map_err(|error| {
            Error::InvalidInput(format!(
                "invalid tagged retrieval semantic generation: {error}"
            ))
        })?;
        let embedding = self.embedder.embed_query(query).await?;
        let request =
            TaggedAnnRequest::new(&embedding, normalized_tags.iter().copied(), &lanes, limits)?;
        let mut batch = self.vectors.tagged_ann(request.clone()).await?;
        // Storage adapters are trust boundaries. Validate again in the engine so
        // a custom implementation cannot smuggle malformed work, lanes, scores,
        // or a stale projection generation into graph expansion.
        batch.validate_against(
            &request,
            retrieval_semantic_generation,
            projection_generation,
        )?;
        self.revalidate_tagged_seed_membership(&request, &mut batch)
            .await?;

        let stamp = self.retrieval_policy_stamp(RetrievalPolicyInput {
            effective_k,
            budget,
            status,
            tags: request.tags(),
            sparse_enabled: false,
            reranker_enabled: effective_k > 0 && self.reranker.is_some(),
            tagged_request: Some(&request),
            tagged_lanes: Some(&batch.lanes),
            tagged_projection_generation: Some(projection_generation),
        });
        let mut returned = batch.lanes.into_iter();
        let primary_lane = returned
            .next()
            .expect("validated tagged batch contains the requested primary lane");
        debug_assert!(returned.next().is_none());
        let dense_seeds = primary_lane.hits;
        let direct = dense_seeds.clone();
        let primary_evidence = EvidenceRanks {
            dense: ranked_ids(&dense_seeds),
            ..EvidenceRanks::default()
        };
        let tagged = TaggedRetrievalMetadata {
            work: batch.work,
            primary_seed_coverage: primary_lane.seed_coverage,
        };

        self.retrieve_pass(
            &embedding,
            query,
            direct,
            dense_seeds,
            Vec::new(),
            primary_evidence,
            budget,
            status,
            stamp,
            Some(tagged),
            trace,
            routing_hints,
            request.tags(),
            effective_k > 0,
        )
        .await
    }

    /// Structural batch validation cannot prove that a returned ID still
    /// carries a requested tag. Hydrate every bounded seed before it can become
    /// a graph root, then remove stale/malformed canonical memberships. A valid
    /// adapter snapshot can race a concurrent lifecycle or tag mutation, so a
    /// mismatch is an omission rather than a query-wide adapter error.
    async fn revalidate_tagged_seed_membership(
        &self,
        request: &TaggedAnnRequest<'_>,
        batch: &mut mneme_core::tagged::TaggedAnnBatch,
    ) -> Result<()> {
        let ids: Vec<_> = batch
            .lanes
            .iter()
            .flat_map(|lane| lane.hits.iter().map(|hit| hit.id))
            .collect();
        let mut hydrated = self.hydrate_nodes(&ids).await?.into_iter();
        for (requested, returned) in request.lanes.iter().zip(&mut batch.lanes) {
            returned.hits.retain(|_| {
                hydrated.next().is_some_and(|node| {
                    node.is_some_and(|node| {
                        node.is_semantic()
                            && requested.status.allows(node.status())
                            && node
                                .tags()
                                .any(|tag| request.tags().binary_search(&tag).is_ok())
                    })
                })
            });
        }
        debug_assert!(hydrated.next().is_none());
        Ok(())
    }

    fn retrieval_policy_stamp(&self, input: RetrievalPolicyInput<'_>) -> RetrievalPolicyStamp {
        let projection_watermarks = input.tagged_projection_generation.map_or_else(
            ProjectionWatermarks::default,
            |generation| {
                ProjectionWatermarks::new(vec![ProjectionWatermark {
                    projection: "tag-membership".into(),
                    watermark: generation.into(),
                }])
                .expect("one tagged projection watermark is within the domain limit")
            },
        );
        RetrievalPolicyStamp {
            retrieval_policy: self.retrieval_policy_version(input),
            index_set: IndexSetFingerprint {
                embedding: self.embedder.fingerprint(),
                vector_semantics: self.vectors.semantic_id(),
                lexical_semantics: input.sparse_enabled.then(|| {
                    self.lexical
                        .as_ref()
                        .expect("enabled lexical index")
                        .semantic_id()
                }),
                reranker_semantics: input.reranker_enabled.then(|| {
                    self.reranker
                        .as_ref()
                        .expect("enabled reranker")
                        .semantic_id()
                }),
            },
            projection_watermarks,
        }
    }

    fn retrieval_policy_version(&self, input: RetrievalPolicyInput<'_>) -> RetrievalPolicyVersion {
        use sha2::{Digest, Sha256};

        let RetrievalPolicyInput {
            effective_k,
            budget,
            status,
            tags,
            sparse_enabled,
            reranker_enabled,
            tagged_request,
            tagged_lanes,
            ..
        } = input;

        let mut hash = Sha256::new();
        hash.update(
            b"mneme:rank-only-v6\0semantic-corpus-only\0precise-graph-fusion-direct-winner-ties\0conditional-target-fair-entry-v1\0",
        );
        hash.update(retrieval_index_semantics_fingerprint(
            self.vectors.semantic_id(),
            self.lexical.as_ref().map(|index| index.semantic_id()),
            sparse_enabled,
            self.reranker
                .as_ref()
                .map(|reranker| reranker.semantic_id()),
            reranker_enabled,
        ));
        for value in [
            self.cfg.lexical_k,
            self.cfg.graph_seed_cap,
            self.cfg.graph_slot_cap,
            MAX_LEXICAL_K,
            MAX_RETRIEVAL_RANK,
            effective_k,
            budget.max_nodes,
        ] {
            hash.update((value as u64).to_le_bytes());
        }
        for value in [
            self.cfg.rrf_constant,
            self.cfg.dense_weight,
            self.cfg.lexical_weight,
            self.cfg.graph_weight,
            budget.min_relevance,
            budget.explore,
            budget.relevance_ratio,
            budget.dedup_similarity,
            budget.query_conditioning,
        ] {
            hash.update(value.to_bits().to_le_bytes());
        }
        hash.update([budget.max_depth]);
        hash.update([
            u8::from(status.active),
            u8::from(status.archived),
            u8::from(sparse_enabled),
            u8::from(reranker_enabled),
        ]);
        hash.update((tags.len() as u64).to_le_bytes());
        for tag in tags {
            hash.update((tag.len() as u64).to_le_bytes());
            hash.update(tag.as_bytes());
        }
        match (tagged_request, tagged_lanes) {
            (Some(request), Some(returned_lanes)) => {
                hash.update([1]);
                hash.update(TAGGED_RETRIEVAL_POLICY_GENERATION.as_bytes());
                for value in [
                    request.limits.max_raw_memberships(),
                    request.limits.max_unique_exact_ids(),
                    request.limits.max_exact_vector_components(),
                    request.limits.exact_hydration_page_ids(),
                    request.limits.max_fallback_candidates(),
                    request.limits.max_fallback_hydration_ids(),
                    request.limits.max_fallback_vector_components(),
                ] {
                    hash.update((value as u64).to_le_bytes());
                }
                hash.update((request.lanes.len() as u64).to_le_bytes());
                for lane in request.lanes {
                    hash.update([match lane.lane {
                        RetrievalLifecycleLane::Primary => 0,
                    }]);
                    hash.update([u8::from(lane.status.active), u8::from(lane.status.archived)]);
                    hash.update((lane.k as u64).to_le_bytes());
                    hash.update((lane.fallback_candidates as u64).to_le_bytes());
                }
                hash.update(tagged_coverage_shape_fingerprint(returned_lanes));
            }
            (None, None) => hash.update([0]),
            _ => unreachable!("tagged policy request and validated lane coverage travel together"),
        }

        RetrievalPolicyVersion {
            contract: "rank-only-v6",
            fingerprint: hash.finalize().into(),
        }
    }

    /// Page an operation-bounded identity list through the store's positional
    /// batch primitive. Normal MCP/CLI retrieval fits in one page; direct
    /// library callers with a larger budget retain their historical semantics
    /// without turning one backend query into an unbounded parameter payload.
    async fn hydrate_nodes(&self, ids: &[NodeId]) -> Result<Vec<Option<Node>>> {
        let mut hydrated = Vec::with_capacity(ids.len());
        for page in ids.chunks(MAX_NODE_HYDRATION_BATCH) {
            let nodes = self.graph.get_nodes(page).await?;
            if nodes.len() != page.len() {
                return Err(Error::Backend(format!(
                    "graph store returned {} node hydration slots for {} ids",
                    nodes.len(),
                    page.len()
                )));
            }
            for (expected, node) in page.iter().zip(&nodes) {
                if let Some(node) = node
                    && node.id() != *expected
                {
                    return Err(Error::Backend(format!(
                        "graph store returned node {:?} in hydration slot for {expected:?}",
                        node.id()
                    )));
                }
            }
            hydrated.extend(nodes);
        }
        Ok(hydrated)
    }

    /// Page an operation-bounded identity list through the semantic admission
    /// projection, checking every positional response before decisions consume it.
    async fn node_statuses(&self, ids: &[NodeId]) -> Result<Vec<Option<NodeLifecycle>>> {
        let mut statuses = Vec::with_capacity(ids.len());
        for page in ids.chunks(MAX_NODE_STATUS_BATCH) {
            let result = self.graph.get_node_statuses(page).await?;
            if result.len() != page.len() {
                return Err(Error::Backend(format!(
                    "graph store returned {} node status slots for {} ids",
                    result.len(),
                    page.len()
                )));
            }
            statuses.extend(result);
        }
        Ok(statuses)
    }

    /// The middle tier of recall: a normal [`retrieve`](Self::retrieve), then a
    /// read-only one-hop expansion of the top `expand_top` hits (their strongest
    /// `neighbors_each` associations each), all in one call. It exists so an agent
    /// that won't pay for a full walk still gets the graph's associative context
    /// instead of degrading to plain ANN — the spread already ran; this just
    /// surfaces the immediate neighbourhood around the winners. Like `retrieve`,
    /// this only plans context and mutates nothing. Neighbours are deduped against
    /// the hit set and one another, so the expansion is all *new* context.
    pub async fn recall_expanded(
        &self,
        query: &str,
        expand_top: usize,
        neighbors_each: usize,
    ) -> Result<Vec<RecallHit>> {
        let hits = self.retrieve(query).await?;
        // Retrieval's expensive ranking has already committed, but a cold-path
        // lifecycle mutation can win the gate before this second-stage expansion.
        // Revalidate every source and hold the gate across the bounded neighbor
        // reads so one `recall` response is a coherent current view.
        let _mutation = self.mutation_gate.lock().await;
        let hit_ids: Vec<NodeId> = hits.iter().map(|hit| hit.node.id()).collect();
        let revalidated_hits = self.hydrate_nodes(&hit_ids).await?;
        let mut current_hits = Vec::with_capacity(hits.len());
        for (Retrieved { node, score, .. }, current) in hits.into_iter().zip(revalidated_hits) {
            if let Some(current) = current
                && current.is_semantic()
                && same_status_lane(node.status(), current.status())
            {
                current_hits.push((current, score));
            }
        }
        let mut seen: HashSet<NodeId> = current_hits.iter().map(|(node, _)| node.id()).collect();
        let mut neighbor_plans: Vec<Vec<Neighbor>> =
            (0..current_hits.len()).map(|_| Vec::new()).collect();
        for (rank, (node, _)) in current_hits.iter().enumerate() {
            if rank < expand_top && node.is_active() {
                for nb in self
                    .scoped_neighbor_candidates(node.id(), neighbors_each, StatusFilter::ACTIVE)
                    .await?
                {
                    // Skip a neighbour that's itself a hit or already shown — keep
                    // the expansion additive.
                    if !seen.insert(nb.node) {
                        continue;
                    }
                    neighbor_plans[rank].push(nb);
                }
            }
        }

        let target_ids: Vec<NodeId> = neighbor_plans
            .iter()
            .flat_map(|neighbors| neighbors.iter().map(|neighbor| neighbor.node))
            .collect();
        let mut targets = self.hydrate_nodes(&target_ids).await?.into_iter();
        let mut out = Vec::with_capacity(current_hits.len());
        for ((node, score), planned) in current_hits.into_iter().zip(neighbor_plans) {
            let mut neighbors = Vec::with_capacity(planned.len());
            for nb in planned {
                let target = targets.next().ok_or_else(|| {
                    Error::Backend("graph store truncated recall target hydration".into())
                })?;
                let Some(target) = target else { continue };
                // Do not reintroduce archived nodes after active traversal.
                if !target.is_semantic() || !target.is_active() {
                    continue;
                }
                neighbors.push(RecalledNeighbor {
                    node: target,
                    kind: nb.edge.kind,
                    incoming: nb.incoming,
                    weight: nb.edge.weight(),
                });
            }
            out.push(RecallHit {
                node,
                score,
                neighbors,
            });
        }
        debug_assert!(targets.next().is_none());
        Ok(out)
    }

    /// One pass of retrieval at a fixed [`StatusFilter`]: preserve the complete
    /// direct hybrid ranking, spread from a separately bounded root prefix, fuse
    /// non-root expansions back as an independent rank leg, then hydrate/dedup
    /// and return a pure presentation plan.
    #[expect(clippy::too_many_arguments)]
    async fn retrieve_pass(
        &self,
        embedding: &[f32],
        query: &str,
        direct: Vec<Scored>,
        dense: Vec<Scored>,
        sparse: Vec<Scored>,
        mut primary_evidence: EvidenceRanks,
        budget: Budget,
        status: StatusFilter,
        stamp: RetrievalPolicyStamp,
        tagged: Option<TaggedRetrievalMetadata>,
        trace: bool,
        routing_hints: Option<&[RoutingHint]>,
        seed_tags: &[&str],
        routing_work_enabled: bool,
    ) -> Result<RankedRetrievalBatch> {
        // Direct retrieval and graph activation are different evidence scales.
        // Keep every direct hit, select a bounded and evidence-diverse root set,
        // then convert the non-root spread into its own ranked leg. RRF combines
        // ranks rather than pretending an attenuated path product is calibrated
        // like cosine/RRF.
        // A zero-depth budget remains the exact direct ordering used historically.
        let mut graph_paths = BTreeMap::new();
        let mut routing = routing_hints.map(|hints| {
            let mut outcome = RoutingOutcome::default();
            outcome.diagnostics.ignored = hints.len();
            outcome
        });
        // Conditional target entry is independent of physical spread controls.
        // Keep validation and any ordinary routed spread in one semantic snapshot;
        // release it before hydration/reranker/embedder work below.
        let route_snapshot = if routing_work_enabled
            && routing_hints.is_some()
            && budget.max_depth > 0
            && budget.max_nodes > 0
        {
            Some(self.mutation_gate.lock().await)
        } else {
            None
        };
        let (biases, conditional) = if let Some(hints) =
            routing_hints.filter(|_| route_snapshot.is_some())
        {
            let (biases, conditional) = self.validate_routing_hints(hints, status, seed_tags).await;
            let outcome = routing.as_mut().expect("routing opt-in");
            outcome.diagnostics.validated = biases.len();
            outcome.diagnostics.ignored = hints.len() - biases.len();
            (biases, conditional)
        } else {
            (RoutingBiasMap::new(), BTreeMap::new())
        };
        let (ranked, graph_ranks) = if budget.max_depth == 0
            || budget.max_nodes == 0
            || self.cfg.graph_seed_cap == 0
            || self.cfg.graph_slot_cap == 0
            || !self.cfg.graph_weight.is_finite()
            || self.cfg.graph_weight <= 0.0
            || direct.is_empty()
        {
            (direct, HashMap::new())
        } else {
            let direct_floor = {
                let top = direct.first().map_or(0.0, |scored| scored.score);
                budget.min_relevance.max(top * budget.relevance_ratio)
            };
            let direct_leg: Vec<Scored> = direct
                .iter()
                .copied()
                .filter(|hit| hit.score >= direct_floor)
                .collect();
            // Exact/approximate indexes may pad a short corpus with below-floor
            // hits. Preserve the complete ranking until the normal relevance
            // policy is applied, but do not let its rejected tail consume roots
            // or hide a useful incoming graph path merely by being called seeds.
            // A fused top-k is a good final ranking but a bad graph-entry policy:
            // exact lexical anchors and semantic anchors can be buried by items
            // supported by both lanes. Select roots round-robin from fused,
            // dense, and sparse evidence, then normalize admitted roots to unit
            // activation. The total cap is unchanged, so broader evidence
            // coverage does not buy recall with unbounded traversal work.
            let roots = stratified_graph_roots(
                &direct_leg,
                &sparse,
                &dense,
                self.cfg.graph_seed_cap.min(budget.max_nodes),
            );
            if roots.is_empty() {
                (direct, HashMap::new())
            } else {
                let root_ids: HashSet<NodeId> = roots.iter().map(|hit| hit.id).collect();
                let scope = TraversalScope::new(status);
                let (spread, observed_paths, ordering) = if routing_hints.is_some() {
                    let outcome = routing.as_mut().expect("routing opt-in");
                    let observed = self
                        .traversal
                        .spread_routed(&roots, budget, Some(embedding), scope, &biases, true)
                        .await?;
                    if let Some(paths) = &observed.paths {
                        for (&target, base_path) in paths {
                            let path = observed
                                .ordering
                                .as_ref()
                                .and_then(|ordering| ordering.get(&target))
                                .map_or(base_path, |winner| &winner.path);
                            if let Some(hop) = path.last()
                                && let Some(binding) = self.bind_routing_hop(hop).await
                            {
                                outcome.bindings.insert(target, binding);
                            }
                        }
                    }
                    (observed.hits, observed.paths, observed.ordering)
                } else if trace {
                    let observed = self
                        .traversal
                        .spread_with_provenance(&roots, budget, Some(embedding), scope)
                        .await?;
                    (observed.hits, observed.paths, observed.ordering)
                } else {
                    (
                        self.traversal
                            .spread(&roots, budget, Some(embedding), scope)
                            .await?,
                        None,
                        None,
                    )
                };
                let mut expansions: Vec<Scored> = spread
                    .into_iter()
                    .filter(|scored| !root_ids.contains(&scored.id) && scored.score.is_finite())
                    .collect();
                // `Traversal` does not promise output order. Sort the independent
                // non-root leg before deriving its adaptive floor or applying its
                // cap. In particular, direct roots must not set this floor: path
                // attenuation puts roots and useful expansions on different raw
                // scales, which is why the legs are rank-fused below.
                expansions.sort_by(scored_order);
                let expansion_floor = {
                    let top = expansions.first().map_or(0.0, |scored| scored.score);
                    budget.min_relevance.max(top * budget.relevance_ratio)
                };
                expansions.retain(|scored| scored.score >= expansion_floor);
                // Eligibility and original scores stay baseline. Only ordering
                // of actual eligible route arrivals survives into graph slots.
                if let Some(ordering) = &ordering {
                    let key = |hit: &Scored| {
                        ordering
                            .get(&hit.id)
                            .filter(|winner| {
                                winner.original_contribution.is_finite()
                                    && winner.original_contribution > 0.0
                                    && winner.priority.is_finite()
                            })
                            .map_or(hit.score, |winner| winner.priority)
                    };
                    expansions
                        .sort_by(|a, b| key(b).total_cmp(&key(a)).then_with(|| scored_order(a, b)));
                }
                expansions.truncate(budget.max_nodes);
                let graph_ranks = ranked_ids(&expansions);
                let (fused, admitted) = reciprocal_rank_max_fuse_observed(
                    &direct_leg,
                    &expansions,
                    self.cfg.rrf_constant,
                    1.0,
                    self.cfg.graph_weight,
                    self.cfg.graph_slot_cap.min(budget.max_nodes),
                );
                if let Some(mut paths) = observed_paths {
                    for id in admitted {
                        if let Some(base_path) = paths.remove(&id) {
                            let path = ordering
                                .as_ref()
                                .and_then(|ordering| ordering.get(&id))
                                .map_or(base_path, |winner| winner.path.clone());
                            validate_retrieval_path(&path, id, &root_ids, budget.max_depth)?;
                            graph_paths.insert(id, path);
                        }
                    }
                }
                (fused, graph_ranks)
            }
        };
        drop(route_snapshot);
        primary_evidence.graph = graph_ranks;

        // Only plan nodes that clear the relevance floor. Direct and expansion
        // legs have already applied the ratio within
        // their own evidence scales; this final pass applies it once more only
        // after weighted RRF has calibrated the legs onto one output scale. Thus
        // `graph_weight` deliberately participates in the final policy, while an
        // attenuated root score cannot accidentally erase its entire expansion leg.
        let floor = {
            let top = ranked.first().map_or(0.0, |s| s.score);
            budget.min_relevance.max(top * budget.relevance_ratio)
        };
        // Gather candidates above the floor, status-filtered (spread can reach any
        // status through edges; hold the surfaced set to the seed filter), capped.
        let ranked: Vec<Scored> = ranked
            .into_iter()
            .filter(|scored| scored.score >= floor)
            .collect();
        let hydration_ids: Vec<_> = ranked.iter().map(|scored| scored.id).collect();
        let hydrated = self.hydrate_nodes(&hydration_ids).await?;

        let mut cands: Vec<(Node, f32)> = Vec::new();
        for (scored, node) in ranked.into_iter().zip(hydrated) {
            if cands.len() >= budget.max_nodes {
                break;
            }
            if let Some(node) = node
                && node.is_semantic()
                && status.allows(node.status())
            {
                cands.push((node, scored.score));
            }
        }

        let (admitted, conditional_bindings) = interleave_conditional_candidates(
            cands,
            conditional,
            budget.max_nodes,
            self.cfg.rrf_constant,
        );
        cands = admitted;
        if let Some(outcome) = &mut routing {
            for id in conditional_bindings.keys() {
                graph_paths.remove(id);
                outcome.bindings.remove(id);
            }
            outcome.conditional_bindings = conditional_bindings;
        }

        // Optional rerank: a cross-encoder re-scores (query, summary) *jointly* over
        // the bounded candidate set, catching relevance the bi-encoder seeds missed.
        // Replaces the spread score with a sigmoid of the rerank score and re-sorts,
        // so the dedup and return below act on the sharper ordering. Off — and
        // model-free — when no reranker is wired.
        if let Some(reranker) = &self.reranker
            && !cands.is_empty()
        {
            let summaries: Vec<&str> = cands.iter().map(|(n, _)| n.summary()).collect();
            let scores = reranker.rerank(query, &summaries).await?;
            if scores.len() != cands.len() {
                return Err(Error::Backend(format!(
                    "reranker returned {} scores for {} documents",
                    scores.len(),
                    cands.len()
                )));
            }
            if scores.iter().any(|score| !score.is_finite()) {
                return Err(Error::Backend(
                    "reranker returned a non-finite relevance score".into(),
                ));
            }
            for ((_, s), raw) in cands.iter_mut().zip(scores) {
                *s = 1.0 / (1.0 + (-raw).exp()); // sigmoid → comparable [0, 1]
            }
            cands.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.id().cmp(&b.0.id())));
            primary_evidence.rerank = cands
                .iter()
                .enumerate()
                .map(|(index, (node, _))| (node.id(), rank_from_index(index)))
                .collect();
        }

        // Near-duplicate filter: drop a result whose summary embedding is too close
        // to a higher-ranked kept one, so recall returns distinct memories rather
        // than clusters of paraphrases. Active only when configured (re-embeds the
        // surfaced summaries — the same vectors the index stores).
        let mut keep = vec![true; cands.len()];
        if budget.dedup_similarity < 1.0 && cands.len() > 1 {
            let summaries: Vec<&str> = cands.iter().map(|(n, _)| n.summary()).collect();
            let embs = self.embedder.embed(&summaries).await?;
            let mut kept: Vec<usize> = Vec::new();
            for i in 0..cands.len() {
                if kept
                    .iter()
                    .any(|&j| cosine(&embs[i], &embs[j]) >= budget.dedup_similarity)
                {
                    keep[i] = false;
                } else {
                    kept.push(i);
                }
            }
        }

        // Revalidate under the engine mutation gate. Ranking stays concurrent, but
        // lifecycle may change while it runs; only current nodes in the exact
        // requested lane may enter this read-only presentation plan.
        let _lifecycle_snapshot = self.mutation_gate.lock().await;
        let retained: Vec<(Node, f32)> = keep
            .into_iter()
            .zip(cands)
            .filter_map(|(keep, candidate)| keep.then_some(candidate))
            .collect();
        let revalidation_ids: Vec<_> = retained.iter().map(|(node, _)| node.id()).collect();
        let revalidated = self.hydrate_nodes(&revalidation_ids).await?;

        let mut current = Vec::new();
        for ((_, score), node) in retained.into_iter().zip(revalidated) {
            if let Some(node) = node
                && node.is_semantic()
                && status.allows(node.status())
            {
                if let Some(binding) = routing
                    .as_ref()
                    .and_then(|outcome| outcome.conditional_bindings.get(&node.id()))
                    && (!self.routing_binding_current(binding, status).await
                        || routing_content_fingerprint(&node) != binding.target_fingerprint)
                {
                    // A conditional admission cannot fall back to an ordinary tail
                    // that never won its place in the shared candidate budget.
                    continue;
                }
                current.push((node, score));
            }
        }

        if let Some(outcome) = &mut routing {
            // The reranker ran outside the lock. Never join old traversal to new
            // semantic endpoints/edge unnoticed; unavailable evidence is omitted.
            let mut bindings = BTreeMap::new();
            for (node, _) in &current {
                if let Some(binding) = outcome.bindings.get(&node.id())
                    && self.routing_binding_current(binding, status).await
                    && routing_content_fingerprint(node) == binding.target_fingerprint
                {
                    bindings.insert(node.id(), binding.clone());
                }
            }
            outcome.bindings = bindings;
            let current_ids: HashSet<_> = current.iter().map(|(node, _)| node.id()).collect();
            outcome
                .conditional_bindings
                .retain(|id, _| current_ids.contains(id));
        }

        // Build a pure typed plan without recording exposure. Scores stay private
        // to the temporary compatibility adapter; rank evidence is the public
        // contract and is never rewritten to fake cross-lane monotonicity.
        let primary = current
            .into_iter()
            .map(|(node, score)| RankedRetrievalHit {
                evidence: primary_evidence.for_node(node.id()),
                graph_path: graph_paths.remove(&node.id()),
                node,
                score,
            })
            .collect();
        Ok(RankedRetrievalBatch {
            primary,
            stamp,
            tagged,
            routing,
        })
    }

    async fn routing_binding_current(
        &self,
        binding: &RoutingBinding,
        status: StatusFilter,
    ) -> bool {
        self.current_routing_target(binding, status).await.is_some()
    }

    /// Fetch once per validated route; retain the target for optional admission.
    async fn current_routing_target(
        &self,
        binding: &RoutingBinding,
        status: StatusFilter,
    ) -> Option<(Node, Edge)> {
        let previous = self.graph.get_node(binding.previous).await.ok()??;
        let target = self.graph.get_node(binding.target).await.ok()??;
        let edge = self
            .graph
            .get_edge(binding.edge_from, binding.edge_to)
            .await
            .ok()??;
        let current = previous.is_semantic()
            && target.is_semantic()
            && status.allows(previous.status())
            && status.allows(target.status())
            && routing_content_fingerprint(&previous) == binding.previous_fingerprint
            && routing_content_fingerprint(&target) == binding.target_fingerprint
            && routing_edge_fingerprint(&edge) == binding.edge_fingerprint
            && ((edge.from == binding.previous
                && edge.to == binding.target
                && edge.kind.traverses_outgoing())
                || (edge.to == binding.previous
                    && edge.from == binding.target
                    && edge.kind.traverses_incoming()));
        current.then_some((target, edge))
    }

    async fn validate_routing_hints(
        &self,
        hints: &[RoutingHint],
        status: StatusFilter,
        seed_tags: &[&str],
    ) -> (RoutingBiasMap, BTreeMap<NodeId, ConditionalNomination>) {
        let mut counts = HashMap::new();
        for hint in hints {
            *counts.entry(hint.route.route()).or_insert(0usize) += 1;
        }
        let mut biases = RoutingBiasMap::new();
        let mut conditional: BTreeMap<NodeId, ConditionalNomination> = BTreeMap::new();
        for hint in hints {
            if counts[&hint.route.route()] == 1
                && hint.route.has_canonical_fingerprints()
                && let Some((target, edge)) = self.current_routing_target(&hint.route, status).await
            {
                biases.insert(hint.route.route(), hint.sign);
                if hint.sign == SignedRoutingBias::Boost
                    && hint.route.previous != hint.route.target
                    && edge.weight().is_finite()
                    && edge.weight() > 0.0
                    && (seed_tags.is_empty() || target.tags().any(|tag| seed_tags.contains(&tag)))
                {
                    // Canonical representative; neither supporter count nor request
                    // order is ranking evidence. Different routes may share a target.
                    let key = |binding: &RoutingBinding| {
                        (
                            binding.previous,
                            binding.target,
                            binding.edge_from,
                            binding.edge_to,
                        )
                    };
                    if conditional
                        .get(&target.id())
                        .is_none_or(|old| key(&hint.route) < key(&old.binding))
                    {
                        conditional.insert(
                            target.id(),
                            ConditionalNomination {
                                node: target,
                                binding: hint.route.clone(),
                            },
                        );
                    }
                }
            }
        }
        (biases, conditional)
    }

    async fn bind_routing_hop(&self, hop: &TraversalHop) -> Option<RoutingBinding> {
        let previous = self.graph.get_node(hop.previous).await.ok()??;
        let target = self.graph.get_node(hop.target).await.ok()??;
        let edge = self
            .graph
            .get_edge(hop.edge.from, hop.edge.to)
            .await
            .ok()??;
        if !previous.is_semantic()
            || !target.is_semantic()
            || routing_edge_fingerprint(&edge) != routing_edge_fingerprint(&hop.edge)
        {
            return None;
        }
        Some(RoutingBinding::new(&previous, &target, &hop.edge))
    }

    /// Load a node's body. Deferred from retrieval on purpose: only pay the
    /// resolution cost once a result clears your relevance bar.
    pub async fn resolve_body(&self, node: &Node) -> Result<Vec<u8>> {
        self.bodies.resolve(node.body()).await
    }

    /// Load one byte-addressed body window. The storage adapter, rather than a
    /// caller-side truncation, owns the I/O and allocation bound.
    pub async fn resolve_body_range(
        &self,
        node: &Node,
        offset: u64,
        max_bytes: usize,
    ) -> Result<BodyChunk> {
        self.bodies
            .resolve_range(node.body(), offset, max_bytes)
            .await
    }

    /// Load a bounded body prefix. Unlike truncating [`Self::resolve_body`]
    /// afterward, capable stores bound their underlying I/O and allocation too.
    pub async fn resolve_body_prefix(
        &self,
        node: &Node,
        max_bytes: usize,
    ) -> Result<(Vec<u8>, bool)> {
        self.bodies.resolve_prefix(node.body(), max_bytes).await
    }

    // ---- cold path ----------------------------------------------------------

    /// Run one bounded maintenance transaction under an owned gate guard. The
    /// spawned task deliberately outlives cancellation of its caller: an
    /// adapter may already be executing an uncancellable blocking transaction,
    /// so releasing the gate before that transaction ends would let a later
    /// in-process mutation race it.
    async fn commit_maintenance_chunk(
        &self,
        cold: ColdPath,
        commit: MaintenanceCommit,
    ) -> Result<MaintenanceCommitOutcome> {
        let graph = Arc::clone(&self.graph);
        // Cancellation while waiting must enqueue no latent write. Once this
        // await returns there is no suspension point before spawn, so the task
        // either never exists or owns the guard through adapter completion.
        let guard = Arc::clone(&self.mutation_gate).lock_owned().await;
        tokio::spawn(async move {
            let authority = DetachedMutationAuthority::new(graph, guard);
            let result = authority.graph().commit_maintenance(cold, &commit).await;
            drop(authority);
            result
        })
        .await
        .map_err(|error| Error::Backend(format!("maintenance task failed: {error}")))?
    }

    async fn prune_incident_chunk(
        &self,
        cold: ColdPath,
        hub: NodeId,
        target_degree: usize,
    ) -> Result<mneme_core::ports::DensePruneChunkOutcome> {
        let graph = Arc::clone(&self.graph);
        let guard = Arc::clone(&self.mutation_gate).lock_owned().await;
        tokio::spawn(async move {
            let authority = DetachedMutationAuthority::new(graph, guard);
            let result = authority
                .graph()
                .prune_incident_associations(cold, hub, target_degree, MAX_MAINTENANCE_BATCH_ROWS)
                .await;
            drop(authority);
            result
        })
        .await
        .map_err(|error| Error::Backend(format!("dense-prune task failed: {error}")))?
    }

    /// Apply bounded, compare-and-swap decay to edges with explicit negative
    /// feedback. Node retention is never changed by this sweep.
    pub async fn decay_sweep(&self, cold: ColdPath) -> Result<DecayReport> {
        let mut report = DecayReport::default();

        if let Some(through) = self.graph.maintenance_edge_upper_bound(cold).await? {
            let mut after = None;
            loop {
                let page = self
                    .graph
                    .maintenance_edges_page(cold, after, through, MAX_MAINTENANCE_BATCH_ROWS)
                    .await?;
                report.edge_pages += 1;
                let next = page.next;
                let mut mutations = Vec::new();
                for expected in self.semantic_edges(page.items).await? {
                    if expected.interference() == 0 {
                        continue;
                    }
                    let mut replacement = expected.clone();
                    replacement.decay(self.strength_for(replacement.kind));
                    mutations.push(MaintenanceEdgeMutation::Decay {
                        expected,
                        replacement,
                    });
                }
                if !mutations.is_empty() {
                    let planned = mutations.len();
                    let outcome = self
                        .commit_maintenance_chunk(cold, MaintenanceCommit { edges: mutations })
                        .await?;
                    report.edges_decayed += outcome.applied_edges.len();
                    report.edge_conflicts += planned.saturating_sub(outcome.applied_edges.len());
                }
                let Some(cursor) = next else { break };
                after = Some(cursor);
                tokio::task::yield_now().await;
            }
        }

        Ok(report)
    }

    /// Density GC (cold path) — the subtractive counterweight to additive edge
    /// creation (similarity priors, grounded feedback, bridge minting). Two
    /// rules, both signal-driven (no wall clock):
    ///
    /// 1. **Prune never-validated edges** — an edge with zero trials and weight
    ///    below [`Config::prune_weight_floor`] is a similarity guess that never
    ///    earned a co-activation; delete it.
    /// 2. **Physically cap incident association degree** — weakest links are
    ///    deleted until every node is at or below
    ///    [`Config::dense_degree_threshold`]. Prior trials are evidence for rank,
    ///    not an exemption from garbage collection.
    pub async fn prune_dense(&self, cold: ColdPath) -> Result<PruneReport> {
        let mut report = PruneReport::default();

        // Weak-edge GC is exact-row CAS: a reinforcement racing the page read
        // wins, and only that stale deletion is skipped.
        if let Some(through) = self.graph.maintenance_edge_upper_bound(cold).await? {
            let mut after = None;
            loop {
                let page = self
                    .graph
                    .maintenance_edges_page(cold, after, through, MAX_MAINTENANCE_BATCH_ROWS)
                    .await?;
                report.edge_pages += 1;
                let next = page.next;
                let edges: Vec<_> = self
                    .semantic_edges(page.items)
                    .await?
                    .into_iter()
                    .filter(|edge| {
                        edge.trials() == 0 && edge.weight() < self.cfg.prune_weight_floor
                    })
                    .map(|expected| MaintenanceEdgeMutation::DeleteWeak { expected })
                    .collect();
                if !edges.is_empty() {
                    let planned = edges.len();
                    let outcome = self
                        .commit_maintenance_chunk(cold, MaintenanceCommit { edges })
                        .await?;
                    report.pruned += outcome.applied_edges.len();
                    report.weak_conflicts += planned.saturating_sub(outcome.applied_edges.len());
                }
                let Some(cursor) = next else { break };
                after = Some(cursor);
                tokio::task::yield_now().await;
            }
        }

        // Each overfull hub is read, ranked, and pruned inside the adapter
        // transaction, so concurrent weak-GC or feedback cannot make an
        // engine-owned degree snapshot delete the wrong edge. Hubs are visited
        // in key order and each transaction sees deletions made for earlier
        // hubs. This intentionally replaces the old one-shot global edge sort:
        // a shared deletion may satisfy two hubs, so the exact retained set can
        // differ while the per-hub weakest/current-degree rule stays explicit.
        if let Some(through) = self.graph.maintenance_node_upper_bound(cold).await? {
            let mut after = None;
            loop {
                let page = self
                    .graph
                    .maintenance_nodes_page(cold, after, through, MAX_MAINTENANCE_BATCH_ROWS)
                    .await?;
                report.hub_pages += 1;
                let next = page.next;
                let candidate_hubs: Vec<_> = page
                    .items
                    .into_iter()
                    .filter(Node::is_semantic)
                    .map(|node| node.id())
                    .collect();
                let overfull = self
                    .graph
                    .maintenance_overfull_hubs(
                        cold,
                        &candidate_hubs,
                        self.cfg.dense_degree_threshold,
                    )
                    .await?;
                for hub in overfull {
                    let mut remaining = 1;
                    let mut attempts = 0;
                    while remaining > 0 && attempts < MAX_DENSE_PRUNE_CHUNKS {
                        let outcome = self
                            .prune_incident_chunk(cold, hub, self.cfg.dense_degree_threshold)
                            .await?;
                        attempts += 1;
                        report.chunks += 1;
                        report.pruned += outcome.pruned;
                        report.capacity_pruned += outcome.pruned;
                        remaining = outcome.remaining_excess;
                        if outcome.pruned == 0 {
                            break;
                        }
                        if remaining > 0 {
                            tokio::task::yield_now().await;
                        }
                    }
                    if remaining > 0 {
                        report.contended_hubs += 1;
                    }
                }
                let Some(cursor) = next else { break };
                after = Some(cursor);
                tokio::task::yield_now().await;
            }
        }
        Ok(report)
    }

    /// Forget a node outright. Explicit body references are borrowed and never
    /// deleted. For a managed body, responsibility transfers deterministically to
    /// a surviving local sharer; only the final reference erases the payload.
    ///
    /// The transfer/body-delete step happens before metadata cleanup, and the node
    /// row is deleted last. A failed transfer or body deletion therefore retains
    /// the original retry handle; a later cleanup failure may leave duplicate
    /// managed markers (safe) or a bodyless final node (retryable because body
    /// deletion is idempotent). This explicit cold path is O(nodes + edges).
    pub async fn forget(&self, cold: ColdPath, id: NodeId) -> Result<bool> {
        let _mutation = self.mutation_gate.lock().await;
        let Some(node) = self.graph.get_node(id).await? else {
            return Ok(false);
        };

        Self::require_semantic(&node)?;
        // Forgetting a lesson must not silently sever historical evidence. Do
        // this before body ownership transfer, body deletion or any edge write.
        let incident: Vec<_> = self
            .graph
            .all_edges(cold)
            .await?
            .into_iter()
            .filter(|edge| edge.from == id || edge.to == id)
            .collect();
        let endpoints: Vec<_> = incident
            .iter()
            .map(|edge| if edge.from == id { edge.to } else { edge.from })
            .collect();
        if self
            .hydrate_nodes(&endpoints)
            .await?
            .into_iter()
            .flatten()
            .any(|endpoint| !endpoint.is_semantic())
        {
            return Err(Error::InvalidInput(
                "cannot forget a semantic memory referenced by an episode".into(),
            ));
        }

        let mut sharers: Vec<Node> = self
            .graph
            .all_nodes(cold)
            .await?
            .into_iter()
            .filter(|other| other.id() != id && other.body() == node.body())
            .collect();
        sharers.sort_by_key(Node::id);
        if node.body_ownership() == BodyOwnership::Managed {
            if sharers.is_empty() {
                // Privacy-first ordering: if erasure fails, retain the canonical
                // node/ref needed to retry. BodyStore::delete is idempotent.
                self.bodies.delete(node.body()).await?;
            } else if !sharers
                .iter()
                .any(|other| other.body_ownership() == BodyOwnership::Managed)
            {
                // Persist a successor before removing the current responsibility
                // token. Lowest id makes retries and tests deterministic.
                let successor = sharers
                    .first_mut()
                    .expect("non-empty sharer set checked above");
                successor.set_body_ownership(BodyOwnership::Managed);
                self.graph.put_node(successor).await?;
            }
        }

        for edge in incident {
            self.graph.delete_edge(edge.from, edge.to).await?;
        }
        let mut remote_after = None;
        loop {
            let page = self
                .graph
                .remote_edges_page(id, remote_after, MAX_REMOTE_EDGE_PAGE_SIZE)
                .await?;
            for edge in &page.items {
                self.graph
                    .delete_remote_edge(edge.from, edge.target_db, edge.target)
                    .await?;
            }
            let Some(next) = page.next else {
                break;
            };
            remote_after = Some(next);
        }
        self.vectors.remove(id).await?;
        self.graph.delete_node(id).await?;
        Ok(true)
    }

    /// Record that a retrieval pass observed two nodes as conflicting. Cheap and
    /// idempotent-ish: first call creates the overlay row, later calls bump the
    /// observation count that reconciliation triages on.
    pub async fn observe_contradiction(&self, _cold: ColdPath, a: NodeId, b: NodeId) -> Result<()> {
        Self::require_distinct_pair(a, b)?;
        let _mutation = self.mutation_gate.lock().await;
        self.require_semantic_pair(a, b).await?;
        self.graph
            .observe_contradiction(a, b, self.clock.now())
            .await
    }

    pub async fn open_contradictions(&self, cold: ColdPath) -> Result<Vec<Contradiction>> {
        self.graph.open_contradictions(cold).await
    }

    /// Open merge candidates awaiting the merge pass (the redundancy analogue of
    /// [`Memory::open_contradictions`]).
    pub async fn open_merge_candidates(&self, cold: ColdPath) -> Result<Vec<MergeCandidate>> {
        self.graph.open_merge_candidates(cold).await
    }

    /// Record the merge pass's persisted verdict for a pair. Closes the candidacy
    /// so it is not re-litigated. Full collapse has a separate acting primitive.
    /// Legacy `Partial` verdicts remain readable, but new ones are rejected because
    /// no atomic child-plus-derivations writer exists.
    pub async fn resolve_merge(
        &self,
        _cold: ColdPath,
        a: NodeId,
        b: NodeId,
        resolution: MergeResolution,
    ) -> Result<()> {
        Self::require_distinct_pair(a, b)?;
        if resolution == MergeResolution::Partial {
            return Err(Error::InvalidInput(
                "partial merge is unavailable until child, derivations, and resolution commit atomically"
                    .into(),
            ));
        }
        let _mutation = self.mutation_gate.lock().await;
        self.require_semantic_pair(a, b).await?;
        self.graph
            .resolve_merge_candidate(UnorderedPair(a, b), resolution)
            .await
    }

    /// Record a reconciliation verdict. The *decision* — real conflict vs.
    /// context-dependent — is the strong reconciliation agent's job; this is the
    /// deterministic machinery that files it. For a real supersession, prefer
    /// [`Memory::supersede`], which also emits the edge and decays the loser.
    pub async fn reconcile(
        &self,
        _cold: ColdPath,
        a: NodeId,
        b: NodeId,
        resolution: Resolution,
    ) -> Result<()> {
        Self::require_distinct_pair(a, b)?;
        let _mutation = self.mutation_gate.lock().await;
        self.require_semantic_pair(a, b).await?;
        self.graph
            .resolve_contradiction(UnorderedPair(a, b), resolution)
            .await
    }

    /// The reconciliation pass's input: every open contradiction tagged with the
    /// **current** community of each of its nodes, plus the derived cluster-level
    /// "these two communities conflict" aggregate. Communities are recomputed
    /// here and the cluster view is never stored — it's always fresh against the
    /// latest membership (see [`Contradiction`]). The *classification* of each
    /// pair (real supersession vs. context-dependent vs. left unresolved) is the
    /// agent's; this just hands it the evidence, ranked by observation count.
    pub async fn reconciliation_triage(&self, cold: ColdPath) -> Result<ReconciliationTriage> {
        let open = self.graph.open_contradictions(cold).await?;
        let labels: HashMap<NodeId, ClusterId> = self
            .traversal
            .detect_communities(cold)
            .await?
            .into_iter()
            .collect();
        // A node with no community label (e.g. just deleted) gets a sentinel.
        let cluster_of = |id: NodeId| labels.get(&id).copied().unwrap_or(ClusterId(u32::MAX));

        let mut agg: HashMap<UnorderedPair<ClusterId>, (u32, u32)> = HashMap::new();
        let mut contradictions: Vec<TriagedContradiction> = open
            .iter()
            .map(|c| {
                let clusters = (cluster_of(c.between.0), cluster_of(c.between.1));
                let e = agg
                    .entry(UnorderedPair(clusters.0, clusters.1))
                    .or_insert((0, 0));
                e.0 += c.observations;
                e.1 += 1;
                TriagedContradiction {
                    between: c.between,
                    observations: c.observations,
                    clusters,
                }
            })
            .collect();
        contradictions.sort_by_key(|c| std::cmp::Reverse(c.observations));

        let mut cluster_conflicts: Vec<ClusterConflict> = agg
            .into_iter()
            .map(|(clusters, (observations, pairs))| ClusterConflict {
                clusters,
                observations,
                pairs,
            })
            .collect();
        cluster_conflicts.sort_by_key(|c| std::cmp::Reverse(c.observations));

        Ok(ReconciliationTriage {
            contradictions,
            cluster_conflicts,
        })
    }

    /// A whole-graph maintenance snapshot: semantic and episode counts,
    /// unresolved overlays, and edge interference awaiting decay. Cold-path.
    pub async fn status(&self, cold: ColdPath) -> Result<Status> {
        let mut s = Status::default();
        let mut semantic_ids = HashSet::new();
        for node in self.graph.all_nodes(cold).await? {
            s.nodes += 1;
            if let Some(episode) = node.episode() {
                s.episode_editions += 1;
                s.episodes += usize::from(episode.revises().is_none());
                continue;
            }
            semantic_ids.insert(node.id());
            match node.status() {
                NodeStatus::Active => s.active += 1,
                NodeStatus::Archived => s.archived += 1,
            }
        }
        for edge in self.graph.all_edges(cold).await? {
            if semantic_ids.contains(&edge.from)
                && semantic_ids.contains(&edge.to)
                && edge.interference() > 0
            {
                s.edge_decay_pending += 1;
            }
        }
        s.open_contradictions = self.graph.open_contradictions(cold).await?.len();
        s.open_merge_candidates = self.graph.open_merge_candidates(cold).await?.len();
        Ok(s)
    }

    /// Adjudicate a contradiction as a real supersession: `winner` replaces
    /// `loser`. Emits a directional `Supersedes` edge, archives the loser, and
    /// marks the pair resolved. Historical content remains inspectable. The
    /// pair is a one-time historical event identity: later legal graph/lifecycle
    /// changes are not repaired by calling `supersede` on the pair again.
    pub async fn supersede(&self, _cold: ColdPath, winner: NodeId, loser: NodeId) -> Result<()> {
        Self::require_distinct_pair(winner, loser)?;
        let _mutation = self.mutation_gate.lock().await;
        self.require_semantic_pair(winner, loser).await?;
        let commit = SupersedeCommit::new(winner, loser, self.clock.now())?;
        match self.graph.commit_supersede(&commit).await? {
            SupersedeCommitOutcome::Applied | SupersedeCommitOutcome::AlreadyApplied => Ok(()),
        }
    }

    /// Adjudicate an open merge candidate as **full collapse**. The adapter owns
    /// the bounded indexed read, final-state capacity check, edge/evidence
    /// normalization, loser archive, candidate resolution, and durable retry
    /// proof in one transaction. No whole-edge scan or compensating row saga is
    /// permitted here.
    pub async fn merge_full(&self, _cold: ColdPath, winner: NodeId, loser: NodeId) -> Result<()> {
        Self::require_distinct_pair(winner, loser)?;
        let _mutation = self.mutation_gate.lock().await;
        self.require_semantic_pair(winner, loser).await?;
        if let Some(touchstones) = self.graph.touchstones() {
            for owner in [winner, loser] {
                if touchstones.get_touchstone(owner).await?.is_some() {
                    return Err(Error::InvalidInput(
                        "touchstone owners are immutable; use a new capture and explicit supersession"
                            .into(),
                    ));
                }
            }
        }
        let commit = FullMergeCommit::new(winner, loser, self.clock.now())?;
        match self.graph.commit_full_merge(&commit).await? {
            FullMergeCommitOutcome::Applied | FullMergeCommitOutcome::AlreadyApplied => Ok(()),
        }
    }

    /// Re-run community detection over the current graph.
    pub async fn detect_communities(&self, cold: ColdPath) -> Result<Vec<(NodeId, ClusterId)>> {
        self.traversal.detect_communities(cold).await
    }

    /// Long-range consolidation. Over a set of nodes that were relevant together
    /// this session (e.g. a retrieval walk's trail), mint sparse **cross-cluster**
    /// bridges — the associations that connect distant regions of the graph and
    /// enable analogy/insight. Intra-cluster pairs are skipped: that density is
    /// what makes them a cluster, or the agent linked them directly. It's a
    /// *generator*, not a curator — bridges start weak and the decay sweep prunes
    /// the ones that never prove useful (biological replay: rest proposes
    /// associations, waking experience keeps or forgets them). Returns the
    /// bridges it created.
    pub async fn consolidate(
        &self,
        cold: ColdPath,
        relevant: &[NodeId],
    ) -> Result<Vec<(NodeId, NodeId)>> {
        let _mutation = self.mutation_gate.lock().await;
        if relevant.len() < 2 || self.cfg.bridge_probability <= 0.0 {
            return Ok(Vec::new());
        }
        let now = self.clock.now();
        let labels: HashMap<NodeId, u32> = self
            .traversal
            .detect_communities(cold)
            .await?
            .into_iter()
            .map(|(id, c)| (id, c.0))
            .collect();

        // First, graduate bridges that community detection now finds *within* a
        // single cluster — they've pulled their regions together, so they become
        // ordinary associations (reusing the labels we just computed). Then mint
        // new cross-cluster bridges below.
        self.graduate_with_labels(cold, &labels).await?;

        // Bucket the session-relevant nodes by current cluster, sanitizing each
        // (skip archived; follow a supersede to the winner), deduped per cluster.
        let mut by_cluster: BTreeMap<u32, Vec<NodeId>> = BTreeMap::new();
        for &id in relevant {
            let Some(rep) = self.bridge_representative(id).await? else {
                continue;
            };
            if let Some(&cluster) = labels.get(&rep) {
                let bucket = by_cluster.entry(cluster).or_default();
                if !bucket.contains(&rep) {
                    bucket.push(rep);
                }
            }
        }

        let clusters: Vec<(u32, Vec<NodeId>)> = by_cluster.into_iter().collect();
        let mut created = Vec::new();
        for i in 0..clusters.len() {
            for j in (i + 1)..clusters.len() {
                // One bridge per cross-cluster pair, minted with small probability
                // between a (pseudo-random) representative of each cluster.
                let seed = now as u64 ^ ((clusters[i].0 as u64) << 32) ^ clusters[j].0 as u64;
                if rand_unit(seed) >= self.cfg.bridge_probability {
                    continue;
                }
                let a = clusters[i].1[(seed as usize) % clusters[i].1.len()];
                let b = clusters[j].1[((seed >> 17) as usize) % clusters[j].1.len()];
                if a == b || self.connecting_edge(a, b).await?.is_some() {
                    continue;
                }
                self.graph
                    .put_edge(&Edge::new(
                        a,
                        b,
                        self.cfg.bridge_weight,
                        EdgeKind::Bridge,
                        now,
                    ))
                    .await?;
                created.push((a, b));
            }
        }
        Ok(created)
    }

    /// Reclassify [`EdgeKind::Bridge`] edges whose endpoints now share a community
    /// as `Associative` — a bridge that's been absorbed into a cluster is no
    /// longer long-range and should run on the faster intra-cluster clock. Runs
    /// its own community detection; [`consolidate`](Self::consolidate) calls the
    /// label-sharing variant to avoid detecting twice. Returns the count converted.
    pub async fn graduate_bridges(&self, cold: ColdPath) -> Result<usize> {
        let _mutation = self.mutation_gate.lock().await;
        let labels: HashMap<NodeId, u32> = self
            .traversal
            .detect_communities(cold)
            .await?
            .into_iter()
            .map(|(id, c)| (id, c.0))
            .collect();
        self.graduate_with_labels(cold, &labels).await
    }

    /// [`graduate_bridges`](Self::graduate_bridges) given precomputed community
    /// labels. A bridge with both endpoints in the same labelled cluster graduates;
    /// one whose endpoints are unlabelled or still cross-cluster is left alone.
    async fn graduate_with_labels(
        &self,
        cold: ColdPath,
        labels: &HashMap<NodeId, u32>,
    ) -> Result<usize> {
        let mut converted = 0;
        for mut edge in self
            .semantic_edges(self.graph.all_edges(cold).await?)
            .await?
        {
            if edge.kind != EdgeKind::Bridge {
                continue;
            }
            if let (Some(ca), Some(cb)) = (labels.get(&edge.from), labels.get(&edge.to))
                && ca == cb
            {
                edge.kind = EdgeKind::Associative;
                self.graph.put_edge(&edge).await?;
                converted += 1;
            }
        }
        Ok(converted)
    }

    /// Sanitize a bridge endpoint: drop archived nodes, and if a node has been
    /// superseded, bridge to its winner instead (follow the supersede first).
    async fn bridge_representative(&self, id: NodeId) -> Result<Option<NodeId>> {
        let Some(node) = self.graph.get_node(id).await? else {
            return Ok(None);
        };
        if !node.is_semantic() {
            return Ok(None);
        }
        for n in self
            .scoped_neighbor_candidates(id, 16, StatusFilter::ALL)
            .await?
        {
            if n.edge.kind == EdgeKind::Supersedes && n.incoming {
                // A later supersession may have archived this winner too. Do
                // not mint a bridge to an archived intermediate node.
                return Ok(self
                    .graph
                    .get_node(n.node)
                    .await?
                    .filter(|winner| winner.is_semantic() && !winner.is_archived())
                    .map(|winner| winner.id()));
            }
        }
        Ok((!node.is_archived()).then_some(id))
    }

    /// Create (or overwrite) an edge between two nodes. For an agent asserting a
    /// relationship the graph didn't infer on its own. Identity is the endpoint
    /// pair, so this replaces any existing edge for `(from, to)`. `anchor` pins
    /// the edge to a span of `from`'s body (passage-level association).
    pub async fn link(
        &self,
        from: NodeId,
        to: NodeId,
        kind: EdgeKind,
        weight: f32,
        anchor: Option<BodySpan>,
    ) -> Result<()> {
        Self::require_distinct_pair(from, to)?;
        let _mutation = self.mutation_gate.lock().await;
        self.require_pair(from, to).await?;
        let mut edge = Edge::new(from, to, weight, kind, self.clock.now());
        edge.anchor = anchor;
        self.graph.put_edge(&edge).await
    }

    /// Assert a **cross-db** edge: a `from` node in *this* db points at `target`
    /// in another db, named by that db's stamped id `target_db`. The target lives
    /// in a database the engine doesn't hold, so its existence isn't checked here
    /// — the host validates it at creation and resolves it (or skips it) at read
    /// time. `from` must exist locally. Re-linking upserts (replaces the weight).
    pub async fn link_remote(
        &self,
        from: NodeId,
        target_db: Ulid,
        target: NodeId,
        weight: f32,
    ) -> Result<()> {
        let _mutation = self.mutation_gate.lock().await;
        if !weight.is_finite() {
            return Err(Error::InvalidInput(
                "remote edge weight must be finite".into(),
            ));
        }
        if self.graph.get_node(from).await?.is_none() {
            return Err(Error::NotFound);
        }
        self.graph
            .put_remote_edge(&RemoteEdge::new(from, target_db, target, weight))
            .await
    }

    /// One bounded keyset page of cross-db edges out of `from`, as stored and
    /// unresolved. The host may hydrate targets from registered databases, but
    /// one call can never fan out beyond the domain page ceiling.
    pub async fn remote_edges_page(
        &self,
        from: NodeId,
        after: Option<RemoteEdgeCursor>,
        limit: usize,
    ) -> Result<RemoteEdgePage> {
        self.graph.remote_edges_page(from, after, limit).await
    }

    /// The body passage an edge is anchored to (a span of its `from` node's
    /// body), or `None` if it isn't anchored or the source is gone.
    pub async fn resolve_anchor(&self, edge: &Edge) -> Result<Option<Vec<u8>>> {
        if edge.anchor.is_none() {
            return Ok(None);
        }
        let Some(node) = self.graph.get_node(edge.from).await? else {
            return Ok(None);
        };
        self.resolve_anchor_from(edge, &node).await
    }

    /// Resolve an anchor when the caller already hydrated its source node.
    /// This is the batch-friendly form used by neighbor renderers: outgoing
    /// anchors share the current node, while an incoming anchor's source is the
    /// already-resolved far endpoint.
    pub async fn resolve_anchor_from(&self, edge: &Edge, source: &Node) -> Result<Option<Vec<u8>>> {
        let Some(span) = edge.anchor else {
            return Ok(None);
        };
        if source.id() != edge.from {
            return Err(Error::InvalidInput(format!(
                "anchor source {} does not match edge source {}",
                source.id().0,
                edge.from.0
            )));
        }
        let max_bytes = usize::try_from(span.end.saturating_sub(span.start)).map_err(|_| {
            Error::InvalidInput("anchor byte length does not fit this platform".into())
        })?;
        let chunk = self
            .bodies
            .resolve_range(source.body(), u64::from(span.start), max_bytes)
            .await?;
        Ok(Some(chunk.bytes))
    }

    // ---- read-through helpers (handy for callers / the daemon) --------------

    /// Compare the exact current tag set and atomically replace tags only.
    pub async fn compare_replace_node_tags(
        &self,
        id: NodeId,
        expected: &mneme_core::BoundedTagSet,
        replacement: &mneme_core::BoundedTagSet,
    ) -> Result<Node> {
        self.graph
            .compare_replace_node_tags(id, expected, replacement)
            .await
    }

    pub async fn get_node(&self, id: NodeId) -> Result<Option<Node>> {
        self.graph.get_node(id).await
    }

    pub async fn all_nodes(&self, cold: ColdPath) -> Result<Vec<Node>> {
        self.graph.all_nodes(cold).await
    }

    /// Fixed upper key for bounded, read-only canonical inventory browsing.
    /// This watermark is not a transactional snapshot.
    pub async fn inventory_node_upper_bound(&self) -> Result<Option<NodeId>> {
        self.graph
            .maintenance_node_upper_bound(ColdPath::acquire())
            .await
    }

    /// Read one indexed canonical inventory page, including historical episode
    /// editions. Unlike semantic retrieval this does not apply admission policy.
    pub async fn inventory_nodes_page(
        &self,
        after: Option<NodeId>,
        through: NodeId,
        limit: usize,
    ) -> Result<mneme_core::ports::MaintenanceNodePage> {
        self.graph
            .maintenance_nodes_page(ColdPath::acquire(), after, through, limit)
            .await
    }

    /// The always-loaded core set — nodes blessed with the [`CORE_TAG`]. Not
    /// retrieved by similarity; meant to be loaded wholesale into context (resolve
    /// their bodies via [`Memory::resolve_body`]). The `core` tag protects from
    /// decay, but an explicit supersession can archive a tagged node; archived
    /// nodes are not always loaded.
    pub async fn core(&self, cold: ColdPath) -> Result<Vec<Node>> {
        Ok(self
            .all_nodes(cold)
            .await?
            .into_iter()
            .filter(|n| n.is_semantic() && n.has_tag(CORE_TAG) && !n.is_archived())
            .collect())
    }

    pub async fn neighbors(&self, id: NodeId, top_k: usize) -> Result<Vec<Neighbor>> {
        self.graph.neighbors(id, top_k).await
    }

    /// Lifecycle-scoped one-hop neighbors, preserving the store's edge ordering.
    ///
    /// Filtering happens before the caller-visible `top_k` fanout: an archived or
    /// otherwise excluded high-weight endpoint cannot consume every slot and hide
    /// a weaker eligible edge. Incident degree is already bounded by
    /// [`MAX_INCIDENT_EDGES`], so this remains operation-bounded without minting a
    /// cold whole-graph capability.
    pub async fn neighbors_scoped(
        &self,
        id: NodeId,
        top_k: usize,
        status: StatusFilter,
    ) -> Result<Vec<Neighbor>> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let _lifecycle_snapshot = self.mutation_gate.lock().await;
        self.scoped_neighbor_candidates(id, top_k, status).await
    }

    /// Read a raw bounded adjacency and hydrate its endpoint slots in one
    /// in-process mutation snapshot. Ordering, duplicates, and missing endpoint
    /// rows are preserved exactly; a missing endpoint is `node: None` rather
    /// than an excuse to hide a stored edge from diagnostic output.
    pub async fn neighbors_hydrated(
        &self,
        id: NodeId,
        top_k: usize,
    ) -> Result<Vec<HydratedNeighbor>> {
        if top_k > MAX_RESOLVED_NEIGHBORS {
            return Err(Error::CapacityExceeded {
                resource: "hydrated neighbor fanout",
                limit: MAX_RESOLVED_NEIGHBORS,
            });
        }
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let _snapshot = self.mutation_gate.lock().await;
        let neighbors = self.bounded_neighbors(id, top_k).await?;
        self.hydrate_neighbor_rows(neighbors).await
    }

    /// Resolve a lifecycle-scoped one-hop shortlist without per-neighbor point
    /// reads. For a selective lifecycle scope the operation is deliberately
    /// staged:
    ///
    /// 1. read at most the complete 1,024-edge incident set;
    /// 2. fetch semantic admission for source/endpoints in bounded positional
    ///    batches (one normally, two only at the full incident ceiling);
    /// 3. select at most 64 eligible neighbors in stored edge order;
    /// 4. hydrate that shortlist in one positional batch; and
    /// 5. revalidate lifecycle from the hydrated canonical rows.
    ///
    /// Even `StatusFilter::ALL` excludes episode editions before shortlist
    /// selection. The in-process mutation gate is held from the
    /// adjacency read through final revalidation, so a public mutation cannot
    /// splice edges and nodes from two engine states. A row missing at either
    /// read is omitted. As with every returned value, mutation after this call
    /// returns can make the snapshot stale; callers needing newer state must
    /// call again.
    pub async fn resolved_neighbors_scoped(
        &self,
        id: NodeId,
        top_k: usize,
        status: StatusFilter,
    ) -> Result<Vec<ResolvedNeighbor>> {
        if top_k > MAX_RESOLVED_NEIGHBORS {
            return Err(Error::CapacityExceeded {
                resource: "resolved neighbor fanout",
                limit: MAX_RESOLVED_NEIGHBORS,
            });
        }
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let _lifecycle_snapshot = self.mutation_gate.lock().await;
        let selected = self.scoped_neighbor_candidates(id, top_k, status).await?;
        if selected.is_empty() {
            return Ok(Vec::new());
        }

        Ok(self
            .hydrate_neighbor_rows(selected)
            .await?
            .into_iter()
            .filter_map(|hydrated| {
                let node = hydrated.node?;
                (node.is_semantic() && status.allows(node.status())).then_some(ResolvedNeighbor {
                    neighbor: hydrated.neighbor,
                    node,
                })
            })
            .collect())
    }

    /// Build a query → relevance map: embed the query and ANN for the nearest `k`
    /// nodes (over all statuses), returning `node → similarity`. The same signal
    /// the conditioned spread uses (see [`Budget::query_conditioning`]), exposed so
    /// a constrained walk can present query-relevant *salience* next to a node's
    /// stored edge weights without itself running a spread. Read-only.
    pub async fn query_relevance(&self, query: &str, k: usize) -> Result<HashMap<NodeId, f32>> {
        let emb = self.embedder.embed_query(query).await?;
        Ok(self
            .vectors
            .ann(&emb, k, StatusFilter::ALL)
            .await?
            .into_iter()
            .map(|s| (s.id, s.score))
            .collect())
    }

    // ---- internals ----------------------------------------------------------

    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        self.embedder
            .embed(&[text])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::Backend("embedder returned no vectors".into()))
    }

    async fn scoped_neighbor_candidates(
        &self,
        id: NodeId,
        top_k: usize,
        status: StatusFilter,
    ) -> Result<Vec<Neighbor>> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let neighbors = self.bounded_neighbors(id, MAX_INCIDENT_EDGES).await?;
        let mut ids = neighbors
            .iter()
            .map(|neighbor| neighbor.node)
            .collect::<Vec<_>>();
        ids.push(id);
        let mut statuses = self.node_statuses(&ids).await?;
        // The source must belong to the same lifecycle scope as its endpoints.
        // A direct Archived ID cannot become an ordinary Active walk root.
        if !statuses
            .pop()
            .flatten()
            .is_some_and(|source| source.is_admitted_by(status))
        {
            return Ok(Vec::new());
        }
        Ok(neighbors
            .into_iter()
            .zip(statuses)
            .filter_map(|(neighbor, node_status)| {
                node_status
                    .filter(|node_status| node_status.is_admitted_by(status))
                    .map(|_| neighbor)
            })
            .take(top_k)
            .collect())
    }

    async fn bounded_neighbors(&self, id: NodeId, top_k: usize) -> Result<Vec<Neighbor>> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let neighbors = self.graph.neighbors(id, top_k).await?;
        let expected_max = top_k.min(MAX_INCIDENT_EDGES);
        if neighbors.len() > expected_max {
            return Err(Error::Backend(format!(
                "graph store returned {} neighbors for a bounded request of {expected_max}",
                neighbors.len()
            )));
        }
        Ok(neighbors)
    }

    async fn hydrate_neighbor_rows(
        &self,
        neighbors: Vec<Neighbor>,
    ) -> Result<Vec<HydratedNeighbor>> {
        let ids = neighbors
            .iter()
            .map(|neighbor| neighbor.node)
            .collect::<Vec<_>>();
        let nodes = self.hydrate_nodes(&ids).await?;
        Ok(neighbors
            .into_iter()
            .zip(nodes)
            .map(|(neighbor, node)| HydratedNeighbor { neighbor, node })
            .collect())
    }

    /// The directed edge that makes `b` a neighbor of `a`: the forward `a -> b`
    /// if it exists, else the reverse `b -> a` (which surfaces as `a`'s incoming
    /// neighbor). Bridge learning uses this to check for an existing connection.
    async fn connecting_edge(&self, a: NodeId, b: NodeId) -> Result<Option<Edge>> {
        if let Some(edge) = self.graph.get_edge(a, b).await? {
            return Ok(Some(edge));
        }
        self.graph.get_edge(b, a).await
    }

    /// Automatic maintenance acts only on semantic relationships. Episode
    /// evidence survives regardless of its weight, trials or interference.
    async fn semantic_edges(&self, edges: Vec<Edge>) -> Result<Vec<Edge>> {
        let ids: Vec<_> = edges.iter().flat_map(|edge| [edge.from, edge.to]).collect();
        let admitted = self.node_statuses(&ids).await?;
        Ok(edges
            .into_iter()
            .zip(admitted.chunks_exact(2))
            .filter_map(|(edge, endpoints)| endpoints.iter().all(Option::is_some).then_some(edge))
            .collect())
    }

    fn require_semantic(node: &Node) -> Result<()> {
        if node.is_semantic() {
            Ok(())
        } else {
            Err(Error::InvalidInput(
                "episodes are outside semantic learning and maintenance; use episode operations"
                    .into(),
            ))
        }
    }

    async fn require_semantic_pair(&self, a: NodeId, b: NodeId) -> Result<()> {
        // Missing endpoints may still have an authoritative terminal/retry
        // proof. Let each adapter operation check that ledger before absence;
        // this guard only prevents present episodes entering semantic writes.
        let ids = [a, b];
        for (id, status) in ids.into_iter().zip(self.node_statuses(&ids).await?) {
            // A present semantic lifecycle proves admission without hydrating
            // summaries/bodies. Only absence from that projection needs a
            // canonical lookup to distinguish a missing row from an episode.
            if status.is_none()
                && let Some(node) = self.graph.get_node(id).await?
            {
                Self::require_semantic(&node)?;
            }
        }
        Ok(())
    }

    async fn pair_exists(&self, a: NodeId, b: NodeId) -> Result<bool> {
        if a == b {
            return Ok(false);
        }
        if self.graph.get_node(a).await?.is_none() {
            return Ok(false);
        }
        Ok(self.graph.get_node(b).await?.is_some())
    }

    fn require_distinct_pair(a: NodeId, b: NodeId) -> Result<()> {
        if a == b {
            Err(Error::InvalidInput(
                "semantic relation endpoints must be distinct".into(),
            ))
        } else {
            Ok(())
        }
    }

    async fn require_pair(&self, a: NodeId, b: NodeId) -> Result<()> {
        Self::require_distinct_pair(a, b)?;
        if self.pair_exists(a, b).await? {
            Ok(())
        } else {
            Err(Error::NotFound)
        }
    }

    /// The strength curve to use for an edge of this kind: bridges live on the
    /// slower [`Config::bridge_strength`] clock, everything else on `strength`.
    fn strength_for(&self, kind: EdgeKind) -> &StrengthParams {
        match kind {
            EdgeKind::Bridge => &self.cfg.bridge_strength,
            _ => &self.cfg.strength,
        }
    }

    async fn capture_similarity_priors(
        &self,
        node: &Node,
        embedding: &[f32],
        authored: &[Edge],
    ) -> Vec<CaptureSimilarityPrior> {
        if authored.len() == MAX_CAPTURE_EDGES || self.cfg.similarity_link_cap == 0 {
            return Vec::new();
        }
        let Ok(planned) = self
            .qualify_similarity_candidates(node.id(), embedding, authored)
            .await
        else {
            return Vec::new();
        };
        planned
            .into_iter()
            .filter_map(|(target, _, score)| {
                let edge = Edge::new(
                    node.id(),
                    target.id(),
                    score,
                    EdgeKind::Associative,
                    node.created(),
                );
                CaptureSimilarityPrior::new(edge, &target).ok()
            })
            .collect()
    }

    async fn plan_similarity_edges(
        &self,
        id: NodeId,
        embedding: &[f32],
        body: &[u8],
        now: Timestamp,
    ) -> Result<Vec<Edge>> {
        let kept: Vec<_> = self
            .qualify_similarity_candidates(id, embedding, &[])
            .await?
            .into_iter()
            .take(self.cfg.similarity_link_cap)
            .collect();
        if kept.is_empty() {
            return Ok(Vec::new());
        }
        let body_text = String::from_utf8_lossy(body);
        let chunks = chunk_spans(&body_text);
        let anchors = if chunks.len() >= 2 {
            let summaries = kept
                .iter()
                .map(|(_, embedding, _)| embedding.as_slice())
                .collect::<Vec<_>>();
            self.best_anchors(&chunks, &summaries).await?
        } else {
            vec![None; kept.len()]
        };
        Ok(kept
            .into_iter()
            .zip(anchors)
            .map(|((target, _, score), anchor)| {
                let mut edge = Edge::new(id, target.id(), score, EdgeKind::Associative, now);
                edge.anchor = anchor;
                edge
            })
            .collect())
    }

    async fn qualify_similarity_candidates(
        &self,
        id: NodeId,
        embedding: &[f32],
        authored: &[Edge],
    ) -> Result<Vec<(Node, Vec<f32>, f32)>> {
        let nomination_cap = self
            .cfg
            .ann_k
            .min(mneme_core::ports::MAX_CAPTURE_PRIOR_CANDIDATES);
        let link_cap = self.cfg.similarity_link_cap.min(nomination_cap);
        if link_cap == 0 {
            return Ok(Vec::new());
        }
        // ANN nominates only: an independent writer can change a node after its
        // indexed score was read. Qualification must use the hydrated snapshot.
        // At most C+1 raw hits allow ingest's already-indexed self. At most C+1
        // canonical reads retain/rescore C=min(ann_k,1024) valid distinct targets.
        // Capture passes all qualified nominees to atomic output-budget admission.
        let hits = self
            .vectors
            .ann(embedding, nomination_cap + 1, StatusFilter::ACTIVE)
            .await?;
        if hits.len() > nomination_cap + 1 {
            return Err(Error::Backend(
                "similarity nomination exceeded its bound".into(),
            ));
        }
        let mut targets = Vec::new();
        let mut seen = HashSet::new();
        for hit in hits {
            if targets.len() == nomination_cap {
                break;
            }
            if hit.id == id || !seen.insert(hit.id) || authored.iter().any(|edge| edge.to == hit.id)
            {
                continue;
            }
            if let Some(target) = self.graph.get_node(hit.id).await?
                && target.is_semantic()
                && target.status() == NodeStatus::Active
                && self.graph.get_edge(id, hit.id).await?.is_none()
            {
                targets.push(target);
            }
        }
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let texts = targets
            .iter()
            .map(|target| target.summary())
            .collect::<Vec<_>>();
        let embs = self.embedder.embed(&texts).await?;
        self.validate_similarity_embeddings(&embs, targets.len(), embedding.len())?;
        let mut ranked = targets
            .into_iter()
            .zip(embs)
            .map(|(target, vector)| {
                let score = cosine(embedding, &vector);
                (target, vector, score)
            })
            .collect::<Vec<_>>();
        if ranked.iter().any(|(_, _, score)| !score.is_finite()) {
            return Err(Error::Backend("nonfinite similarity rescore".into()));
        }
        ranked.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.id().cmp(&b.0.id())));
        let kept = ranked
            .into_iter()
            .enumerate()
            .filter_map(|(rank, candidate)| {
                (rank < self.cfg.min_similarity_links.min(link_cap)
                    || candidate.2 >= self.cfg.similarity_link_threshold)
                    .then_some(candidate)
            })
            .collect::<Vec<_>>();
        Ok(kept)
    }

    fn validate_similarity_embeddings(
        &self,
        embs: &[Vec<f32>],
        count: usize,
        dimension: usize,
    ) -> Result<()> {
        if embs.len() != count
            || embs.iter().any(|v| {
                v.len() != dimension
                    || v.iter().any(|x| !x.is_finite())
                    || !v
                        .iter()
                        .map(|x| (*x as f64).powi(2))
                        .sum::<f64>()
                        .is_normal()
            })
        {
            return Err(Error::Backend("invalid similarity embedding batch".into()));
        }
        Ok(())
    }

    /// Anchors reuse the same hydrated-summary embeddings used to qualify the
    /// candidate. Only body chunks need another batch, and capture opts out.
    async fn best_anchors(
        &self,
        chunks: &[(BodySpan, &str)],
        summary_embs: &[&[f32]],
    ) -> Result<Vec<Option<BodySpan>>> {
        let texts = chunks.iter().map(|(_, text)| *text).collect::<Vec<_>>();
        let chunk_embs = self.embedder.embed(&texts).await?;
        self.validate_similarity_embeddings(&chunk_embs, chunks.len(), self.embedder.dim())?;
        Ok(summary_embs
            .iter()
            .map(|se| {
                chunk_embs
                    .iter()
                    .enumerate()
                    .max_by(|(_, a), (_, b)| cosine(a, se).total_cmp(&cosine(b, se)))
                    .map(|(i, _)| chunks[i].0)
            })
            .collect())
    }
}

struct ConditionalNomination {
    node: Node,
    binding: RoutingBinding,
}

/// Fair admission between an already eligible ordinary queue and independently
/// eligible conditional recommendations. Origin is first admission, not votes,
/// similarity confidence, graph traversal, reranker causality, or useful delivery.
fn interleave_conditional_candidates(
    ordinary: Vec<(Node, f32)>,
    conditional: BTreeMap<NodeId, ConditionalNomination>,
    limit: usize,
    constant: f32,
) -> (Vec<(Node, f32)>, BTreeMap<NodeId, RoutingBinding>) {
    if conditional.is_empty() {
        return (ordinary, BTreeMap::new());
    }
    let target_top = ordinary
        .iter()
        .map(|(_, score)| *score)
        .filter(|score| score.is_finite() && *score > 0.0)
        .max_by(f32::total_cmp)
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
    let mut ordinary = ordinary.into_iter();
    let mut conditional = conditional.into_values();
    let mut candidates = Vec::new();
    let mut bindings = BTreeMap::new();
    let mut seen = HashSet::new();
    let mut ordinary_turn = true;
    while candidates.len() < limit {
        // Duplicate IDs do not spend a turn. If a queue is exhausted, drain the
        // other; the ordinary winner therefore remains first when it exists.
        let next = if ordinary_turn {
            ordinary
                .find(|(node, _)| !seen.contains(&node.id()))
                .map(|(node, score)| (node, score, None))
                .or_else(|| {
                    conditional
                        .find(|entry| !seen.contains(&entry.node.id()))
                        .map(|entry| (entry.node, 0.0, Some(entry.binding)))
                })
        } else {
            conditional
                .find(|entry| !seen.contains(&entry.node.id()))
                .map(|entry| (entry.node, 0.0, Some(entry.binding)))
                .or_else(|| {
                    ordinary
                        .find(|(node, _)| !seen.contains(&node.id()))
                        .map(|(node, score)| (node, score, None))
                })
        };
        let Some((node, score, binding)) = next else {
            break;
        };
        seen.insert(node.id());
        if let Some(binding) = binding {
            bindings.insert(node.id(), binding);
        }
        candidates.push((node, score));
        ordinary_turn = !ordinary_turn;
    }
    if !bindings.is_empty() {
        // All compatibility values now express this queue's rank, NOT query
        // cosine. Eligibility was settled per lane before admission, so these
        // values must never be passed through ordinary relevance floors.
        let constant = if constant.is_finite() {
            constant.max(1.0)
        } else {
            60.0
        };
        for (index, (_, score)) in candidates.iter_mut().enumerate() {
            *score = (f64::from(target_top) * (f64::from(constant) + 1.0)
                / (f64::from(constant) + index as f64 + 1.0)) as f32;
        }
    }
    (candidates, bindings)
}

#[derive(Default)]
struct EvidenceRanks {
    dense: HashMap<NodeId, NonZeroU16>,
    sparse: HashMap<NodeId, NonZeroU16>,
    graph: HashMap<NodeId, NonZeroU16>,
    rerank: HashMap<NodeId, NonZeroU16>,
}

impl EvidenceRanks {
    fn for_node(&self, id: NodeId) -> RetrievalEvidence {
        RetrievalEvidence {
            dense_rank: self.dense.get(&id).copied(),
            sparse_rank: self.sparse.get(&id).copied(),
            graph_rank: self.graph.get(&id).copied(),
            rerank_rank: self.rerank.get(&id).copied(),
        }
    }
}

fn rank_from_index(index: usize) -> NonZeroU16 {
    let rank = u16::try_from(index + 1)
        .expect("retrieval ranks are capped to the public NonZeroU16 contract");
    NonZeroU16::new(rank).expect("one-based retrieval rank is never zero")
}

fn ranked_ids(hits: &[Scored]) -> HashMap<NodeId, NonZeroU16> {
    let mut ranks = HashMap::with_capacity(hits.len().min(MAX_RETRIEVAL_RANK));
    for (index, hit) in hits.iter().take(MAX_RETRIEVAL_RANK).enumerate() {
        ranks
            .entry(hit.id)
            .or_insert_with(|| rank_from_index(index));
    }
    ranks
}

/// Weighted reciprocal-rank fusion with score normalization. Raw values are
/// intentionally ignored except that the first leg's best positive score
/// supplies the fused list's output scale. Dense+sparse fusion therefore keeps
/// the cosine scale, while direct+graph fusion keeps the already-normalized
/// direct scale instead of comparing it with attenuated path products.
fn reciprocal_rank_fuse(
    primary: &[Scored],
    secondary: &[Scored],
    constant: f32,
    primary_weight: f32,
    secondary_weight: f32,
) -> Vec<Scored> {
    if secondary.is_empty() {
        return primary.to_vec();
    }
    let primary_weight = primary_weight.max(0.0);
    let secondary_weight = secondary_weight.max(0.0);
    if primary_weight == 0.0 && secondary_weight == 0.0 {
        return primary.to_vec();
    }
    let constant = if constant.is_finite() {
        constant.max(1.0)
    } else {
        60.0
    };
    let mut fused: HashMap<NodeId, f32> = HashMap::new();
    for (rank, hit) in primary.iter().enumerate() {
        *fused.entry(hit.id).or_default() += primary_weight / (constant + rank as f32 + 1.0);
    }
    for (rank, hit) in secondary.iter().enumerate() {
        *fused.entry(hit.id).or_default() += secondary_weight / (constant + rank as f32 + 1.0);
    }
    let raw_top = fused
        .values()
        .copied()
        .max_by(f32::total_cmp)
        .unwrap_or(1.0);
    let target_top = primary
        .iter()
        .map(|hit| hit.score)
        .filter(|score| score.is_finite() && *score > 0.0)
        .max_by(f32::total_cmp)
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
    let scale = if raw_top > 0.0 {
        target_top / raw_top
    } else {
        1.0
    };
    let mut out: Vec<Scored> = fused
        .into_iter()
        .map(|(id, score)| Scored {
            id,
            score: (score * scale).clamp(0.0, 1.0),
        })
        .collect();
    out.sort_by(scored_order);
    out
}

/// Pick a bounded set of graph entry points without letting fused-rank overlap
/// hide a strong single-lane anchor. `eligible` is the already-policy-filtered
/// direct leg and defines which nodes are policy-eligible; the dense and sparse
/// lanes contribute ordering only. Every admitted root starts at unit activation
/// so a single-lane anchor is not penalized twice (first in fused rank, then
/// again while its descendants compete with paths from overlap-heavy roots).
///
/// Round-robin selection is deliberately simple and deterministic. When a lane
/// is absent or duplicates another, its cursor advances to the next unseen
/// eligible id, so the function naturally degrades to the direct prefix without
/// wasting capacity.
fn stratified_graph_roots(
    eligible: &[Scored],
    sparse: &[Scored],
    dense: &[Scored],
    limit: usize,
) -> Vec<Scored> {
    if limit == 0 || eligible.is_empty() {
        return Vec::new();
    }

    let eligible_ids: HashSet<NodeId> = eligible
        .iter()
        .filter(|hit| hit.score.is_finite() && hit.score > 0.0)
        .map(|hit| hit.id)
        .collect();
    if eligible_ids.is_empty() {
        return Vec::new();
    }

    let lanes = [eligible, dense, sparse];
    let mut cursors = [0usize; 3];
    let mut seen = HashSet::with_capacity(limit.min(eligible_ids.len()));
    let mut roots = Vec::with_capacity(limit.min(eligible_ids.len()));

    while roots.len() < limit && roots.len() < eligible_ids.len() {
        let mut progressed = false;
        for (lane_index, lane) in lanes.iter().enumerate() {
            while let Some(hit) = lane.get(cursors[lane_index]) {
                cursors[lane_index] += 1;
                if !eligible_ids.contains(&hit.id) {
                    continue;
                }
                if seen.insert(hit.id) {
                    roots.push(Scored {
                        id: hit.id,
                        score: 1.0,
                    });
                    progressed = true;
                    break;
                }
            }
            if roots.len() >= limit {
                break;
            }
        }
        if !progressed {
            break;
        }
    }

    roots.sort_by_key(|root| root.id);
    roots
}

/// Reciprocal-rank merge for a secondary leg derived from the primary one.
/// Unlike ordinary RRF, overlap takes the stronger contribution instead of
/// summing both: graph expansion begins at direct roots, so treating agreement
/// as independent evidence would double-count a correlated path. A non-root
/// direct hit may still be promoted when its graph rank is stronger than its
/// direct rank—that is the useful multi-hop signal. The explicit cap counts both
/// promotions and novel candidates, bounding how many base ranks graph evidence
/// can alter. The clamped secondary weight keeps the exact direct winner fixed.
#[cfg(test)]
fn reciprocal_rank_max_fuse(
    primary: &[Scored],
    secondary: &[Scored],
    constant: f32,
    primary_weight: f32,
    secondary_weight: f32,
    secondary_slot_cap: usize,
) -> Vec<Scored> {
    reciprocal_rank_max_fuse_observed(
        primary,
        secondary,
        constant,
        primary_weight,
        secondary_weight,
        secondary_slot_cap,
    )
    .0
}

fn reciprocal_rank_max_fuse_observed(
    primary: &[Scored],
    secondary: &[Scored],
    constant: f32,
    primary_weight: f32,
    secondary_weight: f32,
    secondary_slot_cap: usize,
) -> (Vec<Scored>, HashSet<NodeId>) {
    let mut contributions = HashSet::new();
    if secondary.is_empty()
        || secondary_slot_cap == 0
        || !secondary_weight.is_finite()
        || secondary_weight <= 0.0
    {
        return (primary.to_vec(), contributions);
    }
    let primary_weight = primary_weight.max(0.0);
    // Preserve the direct winner at fusion for the engine caller (primary weight
    // one). This is not a graph-majority limit; later reranking may reorder.
    let secondary_weight = secondary_weight.min(1.0 - f32::EPSILON);
    if primary_weight == 0.0 && secondary_weight == 0.0 {
        return (primary.to_vec(), contributions);
    }
    let constant = if constant.is_finite() {
        constant.max(1.0)
    } else {
        60.0
    };
    let mut fused: HashMap<NodeId, f64> = HashMap::new();
    for (rank, hit) in primary.iter().enumerate() {
        let contribution = f64::from(primary_weight) / (f64::from(constant) + rank as f64 + 1.0);
        fused
            .entry(hit.id)
            .and_modify(|score| *score = score.max(contribution))
            .or_insert(contribution);
    }
    let mut admitted = 0usize;
    for (rank, hit) in secondary.iter().enumerate() {
        if admitted >= secondary_slot_cap {
            break;
        }
        let contribution = f64::from(secondary_weight) / (f64::from(constant) + rank as f64 + 1.0);
        match fused.entry(hit.id) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if contribution > *entry.get() {
                    entry.insert(contribution);
                    admitted += 1;
                    contributions.insert(hit.id);
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(contribution);
                admitted += 1;
                contributions.insert(hit.id);
            }
        }
    }
    // Keep precise evidence ordering before quantizing compatibility scores.
    // Huge finite constants and subnormal target scores otherwise erase the
    // clamped graph/direct distinction or overflow f32 normalization.
    let direct_winner = primary.first().map(|hit| hit.id);
    let mut precise: Vec<_> = fused.into_iter().collect();
    precise.sort_by(|(a_id, a_score), (b_id, b_score)| {
        b_score
            .total_cmp(a_score)
            .then_with(|| (Some(*b_id) == direct_winner).cmp(&(Some(*a_id) == direct_winner)))
            .then_with(|| a_id.cmp(b_id))
    });
    let raw_top = precise.first().map_or(1.0, |(_, score)| *score);
    let target_top = primary
        .iter()
        .map(|hit| hit.score)
        .filter(|score| score.is_finite() && *score > 0.0)
        .max_by(f32::total_cmp)
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
    let scale = if raw_top > 0.0 {
        f64::from(target_top) / raw_top
    } else {
        1.0
    };
    let out = precise
        .into_iter()
        .map(|(id, score)| Scored {
            id,
            score: (score * scale).clamp(0.0, 1.0) as f32,
        })
        .collect();
    (out, contributions)
}

fn validate_retrieval_path(
    path: &[TraversalHop],
    target: NodeId,
    roots: &HashSet<NodeId>,
    max_depth: u8,
) -> Result<()> {
    if path.is_empty()
        || path.len() > usize::from(max_depth)
        || !roots.contains(&path[0].previous)
        || path.last().is_none_or(|hop| hop.target != target)
    {
        return Err(Error::Backend(
            "invalid bounded retrieval provenance path".into(),
        ));
    }
    for (index, hop) in path.iter().enumerate() {
        let outgoing = hop.previous == hop.edge.from
            && hop.target == hop.edge.to
            && hop.edge.kind.traverses_outgoing();
        let incoming = hop.previous == hop.edge.to
            && hop.target == hop.edge.from
            && hop.edge.kind.traverses_incoming();
        if hop.previous == hop.target
            || !(outgoing || incoming)
            || index > 0 && path[index - 1].target != hop.previous
        {
            return Err(Error::Backend("invalid retrieval provenance edge".into()));
        }
    }
    Ok(())
}

fn scored_order(a: &Scored, b: &Scored) -> std::cmp::Ordering {
    b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id))
}

fn same_status_lane(a: NodeStatus, b: NodeStatus) -> bool {
    matches!(
        (a, b),
        (NodeStatus::Active, NodeStatus::Active) | (NodeStatus::Archived, NodeStatus::Archived)
    )
}

/// A unit float in `[0, 1)` from a seed — a tiny xorshift64*, so the
/// consolidation pass needs no RNG dependency and stays reproducible per seed.
fn rand_unit(seed: u64) -> f32 {
    let mut x = seed | 1;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    ((v >> 40) as f32) / ((1u64 << 24) as f32)
}

/// Split a body into coarse chunks on blank lines (paragraphs), each with its
/// byte span. A single-paragraph body yields one chunk (so the caller leaves the
/// edge whole-node). Spans land on ASCII boundaries (whitespace / `\n`).
fn chunk_spans(body: &str) -> Vec<(BodySpan, &str)> {
    let bytes = body.as_bytes();
    let mut chunks = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < bytes.len() {
            if bytes[i] == b'\n' && bytes.get(i + 1) == Some(&b'\n') {
                break;
            }
            i += 1;
        }
        if i > start {
            chunks.push((BodySpan::new(start as u32, i as u32), &body[start..i]));
        }
    }
    chunks
}

/// Cosine similarity; `0.0` if either vector is degenerate.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(test)]
mod retrieval_tests {
    use super::*;

    #[test]
    fn conditional_interleave_preserves_empty_policy_and_derives_rank_only_scores() {
        let a = ordering_node(97_001, false);
        let b = ordering_node(97_002, false);
        let c = ordering_node(97_003, false);
        let nomination = |target: &Node| ConditionalNomination {
            node: target.clone(),
            binding: RoutingBinding::new(
                &a,
                target,
                &Edge::new(a.id(), target.id(), 0.2, EdgeKind::Transition, 1),
            ),
        };
        let ordinary = || vec![(a.clone(), 0.9), (b.clone(), 0.6)];
        let (unchanged, bindings) =
            interleave_conditional_candidates(ordinary(), BTreeMap::new(), 2, 60.0);
        assert_eq!(
            unchanged
                .iter()
                .map(|(node, score)| (node.id(), *score))
                .collect::<Vec<_>>(),
            vec![(a.id(), 0.9), (b.id(), 0.6)]
        );
        assert!(bindings.is_empty());
        let (duplicate_winner, bindings) = interleave_conditional_candidates(
            ordinary(),
            BTreeMap::from([(a.id(), nomination(&a))]),
            2,
            60.0,
        );
        assert_eq!(
            duplicate_winner
                .iter()
                .map(|(node, score)| (node.id(), *score))
                .collect::<Vec<_>>(),
            vec![(a.id(), 0.9), (b.id(), 0.6)]
        );
        assert!(bindings.is_empty());
        let (merged, bindings) = interleave_conditional_candidates(
            ordinary(),
            BTreeMap::from([
                (a.id(), nomination(&a)),
                (b.id(), nomination(&b)),
                (c.id(), nomination(&c)),
            ]),
            3,
            60.0,
        );
        assert_eq!(
            merged.iter().map(|(node, _)| node.id()).collect::<Vec<_>>(),
            vec![a.id(), b.id(), c.id()]
        );
        assert_eq!(
            bindings.keys().copied().collect::<Vec<_>>(),
            vec![b.id(), c.id()]
        );
        assert_eq!(merged[0].1, 0.9);
        assert!((merged[1].1 - 0.9 * 61.0 / 62.0).abs() < 0.000001);
        assert!(merged.windows(2).all(|window| window[0].1 > window[1].1));
        let (empty_ordinary, bindings) = interleave_conditional_candidates(
            Vec::new(),
            BTreeMap::from([(c.id(), nomination(&c))]),
            1,
            60.0,
        );
        assert_eq!(empty_ordinary[0].0.id(), c.id());
        assert_eq!(empty_ordinary[0].1, 1.0);
        assert_eq!(bindings.len(), 1);
    }

    #[test]
    fn retrieval_backend_semantics_are_fingerprinted_only_when_enabled() {
        let fingerprint = |vector, lexical, lexical_enabled, reranker, reranker_enabled| {
            retrieval_index_semantics_fingerprint(
                vector,
                Some(lexical),
                lexical_enabled,
                Some(reranker),
                reranker_enabled,
            )
        };
        let baseline = fingerprint("vector-v1", "lexical-v1", true, "reranker-v1", true);
        assert_ne!(
            baseline,
            fingerprint("vector-v2", "lexical-v1", true, "reranker-v1", true)
        );
        assert_ne!(
            baseline,
            fingerprint("vector-v1", "lexical-v2", true, "reranker-v1", true)
        );
        assert_ne!(
            baseline,
            fingerprint("vector-v1", "lexical-v1", true, "reranker-v2", true)
        );

        let disabled = fingerprint("vector-v1", "lexical-v1", false, "reranker-v1", false);
        assert_eq!(
            disabled,
            fingerprint("vector-v1", "lexical-v2", false, "reranker-v2", false),
            "configured but unused legs must not claim to influence the batch"
        );
    }

    #[test]
    fn tagged_policy_fingerprint_tracks_strategy_and_quotas_not_observed_work() {
        use mneme_core::tagged::{
            MAX_TAGGED_RAW_MEMBERSHIPS, TaggedExactWorkLimit, TaggedPhysicalSeedCoverage,
            TaggedPhysicalStatus, TaggedQueryTagSeedCoverage,
        };

        let lane = |coverage| TaggedAnnLane {
            lane: RetrievalLifecycleLane::Primary,
            hits: Vec::new(),
            seed_coverage: coverage,
        };
        let physical = |hnsw_quota, sample_quota, sample_inspected, pivot| {
            TaggedPhysicalSeedCoverage::new(
                TaggedPhysicalStatus::Active,
                hnsw_quota,
                0,
                vec![TaggedQueryTagSeedCoverage::new(0, sample_quota, sample_inspected).unwrap()],
                pivot,
            )
            .unwrap()
        };
        let partial = |hybrid, physical, checked, matching| {
            if hybrid {
                TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter {
                    exceeded_limit: TaggedExactWorkLimit::RawMemberships,
                    raw_memberships: MAX_TAGGED_RAW_MEMBERSHIPS + 1,
                    physical: vec![physical],
                    canonical_candidates_checked: checked,
                    matching_candidates: matching,
                }
            } else {
                TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter {
                    exceeded_limit: TaggedExactWorkLimit::RawMemberships,
                    raw_memberships: MAX_TAGGED_RAW_MEMBERSHIPS + 1,
                    physical: vec![physical],
                    canonical_candidates_checked: checked,
                    matching_candidates: matching,
                }
            }
        };

        let exact = [lane(TaggedSeedCoverage::ExactCosine)];
        let deterministic = [lane(partial(false, physical(0, 192, 0, 0), 0, 0))];
        let deterministic_observed = [lane(partial(false, physical(0, 192, 1, 99), 1, 1))];
        let hybrid = [lane(partial(true, physical(96, 96, 0, 0), 0, 0))];
        let changed_quota = [lane(partial(true, physical(64, 128, 0, 0), 0, 0))];
        for lanes in [
            exact.as_slice(),
            deterministic.as_slice(),
            deterministic_observed.as_slice(),
            hybrid.as_slice(),
            changed_quota.as_slice(),
        ] {
            lanes[0].seed_coverage.validate().unwrap();
        }

        let digest = tagged_coverage_shape_fingerprint;
        assert_ne!(digest(&exact), digest(&deterministic));
        assert_ne!(digest(&deterministic), digest(&hybrid));
        assert_ne!(digest(&hybrid), digest(&changed_quota));
        assert_eq!(
            digest(&deterministic),
            digest(&deterministic_observed),
            "observed counters and pivots do not redefine retrieval policy"
        );
    }

    #[test]
    fn projection_watermark_count_is_construction_bounded() {
        let watermark = || ProjectionWatermark {
            projection: "dense".into(),
            watermark: "generation-1".into(),
        };
        let bounded = ProjectionWatermarks::new(
            (0..MAX_PROJECTION_WATERMARKS)
                .map(|_| watermark())
                .collect(),
        )
        .unwrap();
        assert_eq!(bounded.len(), MAX_PROJECTION_WATERMARKS);
        assert!(matches!(
            ProjectionWatermarks::new(
                (0..=MAX_PROJECTION_WATERMARKS)
                    .map(|_| watermark())
                    .collect()
            ),
            Err(WatermarkLimitError { provided }) if provided == MAX_PROJECTION_WATERMARKS + 1
        ));
    }

    struct OrderingFixtureEmbedder;
    #[async_trait::async_trait]
    impl Embedder for OrderingFixtureEmbedder {
        fn dim(&self) -> usize {
            4
        }
        fn fingerprint(&self) -> EmbeddingFingerprint {
            EmbeddingFingerprint::new("ordering-fixture", 4, "l2-f32-v1", "symmetric-v1")
        }
        async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
        }
    }

    fn ordering_node(raw: u128, seed: bool) -> Node {
        Node::try_new(
            NodeId(Ulid::from(raw)),
            "routing fixture quartz",
            BodyRef::new("inline://fixture").unwrap(),
            if seed {
                vec!["ordering-seed"]
            } else {
                Vec::new()
            },
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap()
    }

    async fn assert_routing_graph_slots<S: GraphStore + VectorIndex + Traversal + 'static>(
        store: Arc<S>,
    ) {
        use mneme_core::ports::{SignedRoutingBias, SystemClock};
        let root = ordering_node(94_001, true);
        let target = ordering_node(94_002, false);
        for node in [&root, &target] {
            store.put_node(node).await.unwrap();
            store
                .upsert(node.id(), &[1.0, 0.0, 0.0, 0.0])
                .await
                .unwrap();
        }
        let edge = Edge::new(root.id(), target.id(), 0.6, EdgeKind::Transition, 1);
        store.put_edge(&edge).await.unwrap();
        let memory = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(OrderingFixtureEmbedder),
            Arc::new(SystemClock),
            Config {
                graph_seed_cap: 1,
                graph_slot_cap: 2,
                ..Config::default()
            },
        );
        let budget = Budget {
            max_nodes: 8,
            max_depth: 1,
            min_relevance: 0.0,
            relevance_ratio: 0.0,
            query_conditioning: 1.0,
            dedup_similarity: 1.0,
            explore: 0.0,
            ..Budget::default()
        };
        let hint = RoutingHint {
            route: RoutingBinding::new(&root, &target, &edge),
            sign: SignedRoutingBias::Boost,
        };
        for count in 0..32u128 {
            let node = ordering_node(94_100 + count, false);
            store.put_node(&node).await.unwrap();
            store
                .upsert(node.id(), &[1.0, 0.0, 0.0, 0.0])
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
            if count == 7 || count == 31 {
                let base = memory
                    .retrieve_batch_seeded_observed(
                        "query",
                        1,
                        budget,
                        StatusFilter::ACTIVE,
                        &["ordering-seed"],
                    )
                    .await
                    .unwrap();
                let empty = memory
                    .retrieve_batch_seeded_routed(
                        "query",
                        1,
                        budget,
                        StatusFilter::ACTIVE,
                        &["ordering-seed"],
                        &[],
                    )
                    .await
                    .unwrap();
                let projection = |batch: &RetrievalBatch| {
                    batch
                        .primary
                        .iter()
                        .map(|hit| {
                            (
                                hit.node.id(),
                                hit.lane_rank,
                                format!("{:?}", hit.graph_path),
                            )
                        })
                        .collect::<Vec<_>>()
                };
                assert_eq!(projection(&base), projection(&empty));
                assert!(!base.primary.iter().any(|hit| hit.node.id() == target.id()));
                let boost = memory
                    .retrieve_batch_seeded_routed(
                        "query",
                        1,
                        budget,
                        StatusFilter::ACTIVE,
                        &["ordering-seed"],
                        &[hint.clone()],
                    )
                    .await
                    .unwrap();
                let actual = boost
                    .primary
                    .iter()
                    .find(|hit| hit.node.id() == target.id())
                    .expect("priority survives native max8 and engine final2");
                assert_eq!(actual.graph_path.as_ref().unwrap()[0].edge.weight(), 0.6);
                assert_eq!(
                    boost.routing.as_ref().unwrap().bindings[&target.id()],
                    hint.route
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
                let conflict = RoutingHint {
                    route: hint.route.clone(),
                    sign: SignedRoutingBias::Weaken,
                };
                let neutral = memory
                    .retrieve_batch_seeded_routed(
                        "query",
                        1,
                        budget,
                        StatusFilter::ACTIVE,
                        &["ordering-seed"],
                        &[hint.clone(), conflict],
                    )
                    .await
                    .unwrap();
                assert_eq!(projection(&base), projection(&neutral));
                assert_eq!(neutral.routing.unwrap().diagnostics.ignored, 2);
                let incumbent = base
                    .primary
                    .iter()
                    .find(|hit| hit.graph_path.is_some())
                    .unwrap();
                let binding =
                    empty.routing.as_ref().unwrap().bindings[&incumbent.node.id()].clone();
                let weak = memory
                    .retrieve_batch_seeded_routed(
                        "query",
                        1,
                        budget,
                        StatusFilter::ACTIVE,
                        &["ordering-seed"],
                        &[RoutingHint {
                            route: binding,
                            sign: SignedRoutingBias::Weaken,
                        }],
                    )
                    .await
                    .unwrap();
                assert!(
                    !weak
                        .primary
                        .iter()
                        .any(|hit| hit.node.id() == incumbent.node.id())
                );
                let floor_budget = Budget {
                    min_relevance: 0.65,
                    ..budget
                };
                let threshold = memory
                    .retrieve_batch_seeded_routed(
                        "query",
                        1,
                        floor_budget,
                        StatusFilter::ACTIVE,
                        &["ordering-seed"],
                        &[hint.clone()],
                    )
                    .await
                    .unwrap();
                assert!(
                    !threshold
                        .primary
                        .iter()
                        .any(|hit| hit.node.id() == target.id()),
                    "priority cannot invent threshold eligibility"
                );
            }
        }
        // A different actual arrival must survive when the original best route is weakened.
        let a = ordering_node(95_001, true);
        let b = ordering_node(95_002, true);
        let z = ordering_node(95_003, false);
        for node in [&a, &b, &z] {
            store.put_node(node).await.unwrap();
            store
                .upsert(node.id(), &[1.0, 0.0, 0.0, 0.0])
                .await
                .unwrap();
        }
        let az = Edge::new(a.id(), z.id(), 0.7, EdgeKind::Transition, 1);
        let bz = Edge::new(b.id(), z.id(), 0.6, EdgeKind::Transition, 1);
        store.put_edge(&az).await.unwrap();
        store.put_edge(&bz).await.unwrap();
        let memory = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(OrderingFixtureEmbedder),
            Arc::new(SystemClock),
            Config {
                graph_seed_cap: 2,
                graph_slot_cap: 2,
                ..Config::default()
            },
        );
        // Use unique root tags to avoid the first independent pressure fixture's root.
        // Tagged fixture construction is immutable here, so query enough seeds and
        // give distinct roots a strictly stronger query score than the old root.
        store
            .upsert(root.id(), &[0.0, 1.0, 0.0, 0.0])
            .await
            .unwrap();
        let weak = RoutingHint {
            route: RoutingBinding::new(&a, &z, &az),
            sign: SignedRoutingBias::Weaken,
        };
        let result = memory
            .retrieve_batch_seeded_routed(
                "query",
                2,
                budget,
                StatusFilter::ACTIVE,
                &["ordering-seed"],
                &[weak],
            )
            .await
            .unwrap();
        let reached = result
            .primary
            .iter()
            .find(|hit| hit.node.id() == z.id())
            .unwrap();
        assert_eq!(reached.graph_path.as_ref().unwrap()[0].previous, b.id());
        assert_eq!(
            result.routing.unwrap().bindings[&z.id()],
            RoutingBinding::new(&b, &z, &bz)
        );
    }

    #[tokio::test]
    async fn routing_graph_slots_reference_small_budget() {
        assert_routing_graph_slots(Arc::new(mneme_cozo::MemStore::new(4))).await;
    }

    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn routing_graph_slots_cozo_small_budget() {
        assert_routing_graph_slots(Arc::new(mneme_cozo::CozoStore::new(4).unwrap())).await;
    }

    async fn assert_default_capacity_graph_admission<
        S: GraphStore + VectorIndex + Traversal + 'static,
    >(
        store: Arc<S>,
    ) {
        use mneme_core::ports::SystemClock;
        let root = ordering_node(96_000, true);
        store.put_node(&root).await.unwrap();
        store
            .upsert(root.id(), &[1.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        let targets: Vec<_> = (1..=4).map(|i| ordering_node(96_000 + i, false)).collect();
        for target in &targets {
            store.put_node(target).await.unwrap();
            store
                .upsert(target.id(), &[1.0, 0.0, 0.0, 0.0])
                .await
                .unwrap();
            store
                .put_edge(&Edge::new(
                    root.id(),
                    target.id(),
                    0.8,
                    EdgeKind::Transition,
                    1,
                ))
                .await
                .unwrap();
        }
        for cap in [None, Some(0), Some(1), Some(2)] {
            let mut cfg = Config::default();
            if let Some(cap) = cap {
                cfg.graph_slot_cap = cap;
            }
            let memory = Memory::new(
                store.clone(),
                store.clone(),
                store.clone(),
                Arc::new(OrderingFixtureEmbedder),
                Arc::new(SystemClock),
                cfg,
            );
            for capacity in [1, 2, 3, 5, 8] {
                let budget = Budget {
                    max_nodes: capacity,
                    max_depth: 1,
                    min_relevance: 0.0,
                    ..Budget::default()
                };
                let plain = memory
                    .retrieve_batch_seeded(
                        "query",
                        1,
                        budget,
                        StatusFilter::ACTIVE,
                        &["ordering-seed"],
                    )
                    .await
                    .unwrap();
                let observed = memory
                    .retrieve_batch_seeded_observed(
                        "query",
                        1,
                        budget,
                        StatusFilter::ACTIVE,
                        &["ordering-seed"],
                    )
                    .await
                    .unwrap();
                let routed = memory
                    .retrieve_batch_seeded_routed(
                        "query",
                        1,
                        budget,
                        StatusFilter::ACTIVE,
                        &["ordering-seed"],
                        &[],
                    )
                    .await
                    .unwrap();
                let ids = |batch: &RetrievalBatch| {
                    batch
                        .primary
                        .iter()
                        .map(|hit| hit.node.id())
                        .collect::<Vec<_>>()
                };
                assert_eq!(ids(&plain), ids(&observed));
                assert_eq!(ids(&plain), ids(&routed));
                assert_eq!(
                    observed.primary.len(),
                    1 + 4usize
                        .min(capacity - 1)
                        .min(cap.unwrap_or(MAX_RETRIEVAL_RANK))
                );
                assert_eq!(observed.primary[0].node.id(), root.id());
                assert!(observed.primary.len() <= capacity);
                for hit in observed.primary.iter().skip(1) {
                    let path = hit.graph_path.as_ref().unwrap();
                    assert_eq!(path.len(), 1);
                    assert_eq!(path[0].previous, root.id());
                    assert_eq!(path[0].target, hit.node.id());
                    assert_eq!(
                        routed.routing.as_ref().unwrap().bindings[&hit.node.id()],
                        RoutingBinding::new(&root, &hit.node, &path[0].edge)
                    );
                }
            }
        }
        assert_eq!(store.neighbors(root.id(), 8).await.unwrap().len(), 4);
        // No-edge and isolated-note cases require no quota filling.
        for target in &targets {
            store.delete_edge(root.id(), target.id()).await.unwrap();
        }
        let memory = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(OrderingFixtureEmbedder),
            Arc::new(SystemClock),
            Config::default(),
        );
        let batch = memory
            .retrieve_batch_seeded_observed(
                "query",
                1,
                Budget {
                    max_nodes: 8,
                    ..Budget::default()
                },
                StatusFilter::ACTIVE,
                &["ordering-seed"],
            )
            .await
            .unwrap();
        assert_eq!(batch.primary.len(), 1);
        assert!(batch.primary[0].graph_path.is_none());
    }

    #[tokio::test]
    async fn default_capacity_graph_admission_mem() {
        assert_default_capacity_graph_admission(Arc::new(mneme_cozo::MemStore::new(4))).await;
    }

    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn default_capacity_graph_admission_cozo() {
        assert_default_capacity_graph_admission(Arc::new(mneme_cozo::CozoStore::new(4).unwrap()))
            .await;
    }

    #[test]
    fn shipped_graph_policy_is_bounded_and_exploration_is_opt_in() {
        let config = Config::default();
        assert_eq!(config.graph_seed_cap, 3);
        assert_eq!(config.graph_slot_cap, MAX_RETRIEVAL_RANK);
        assert!(config.graph_weight.is_finite());
        assert_eq!(config.graph_weight, 1.0);
        assert_eq!(config.similarity_link_cap, 0);
        assert_eq!(config.min_similarity_links, 0);
        assert_eq!(config.budget.explore, 0.0);
        assert_eq!(config.budget.relevance_ratio, 0.0);
        assert_eq!(config.budget.dedup_similarity, 1.0);
    }

    #[test]
    fn graph_roots_reserve_cross_lane_anchors_without_growing_the_cap() {
        let eligible: Vec<Scored> = (1u128..=9)
            .map(|id| Scored {
                id: NodeId(Ulid::from(id)),
                score: 1.0 - id as f32 * 0.05,
            })
            .collect();
        let sparse = [eligible[8], eligible[7]];
        let dense = [eligible[6], eligible[5]];

        let roots = stratified_graph_roots(&eligible, &sparse, &dense, 6);
        let ids: HashSet<NodeId> = roots.iter().map(|root| root.id).collect();

        assert_eq!(roots.len(), 6);
        assert!(ids.contains(&eligible[0].id), "retain the fused winner");
        assert!(
            ids.contains(&eligible[8].id),
            "reserve a buried exact lexical anchor"
        );
        assert!(
            ids.contains(&eligible[6].id),
            "reserve a buried semantic anchor"
        );
        assert!(
            roots
                .windows(2)
                .all(|pair| scored_order(&pair[0], &pair[1]).is_le())
        );
    }

    #[test]
    fn graph_root_stratification_degrades_to_the_direct_prefix() {
        let eligible: Vec<Scored> = (1u128..=6)
            .map(|id| Scored {
                id: NodeId(Ulid::from(id)),
                score: 1.0 - id as f32 * 0.05,
            })
            .collect();
        let roots = stratified_graph_roots(&eligible, &eligible, &eligible, 4);
        assert_eq!(
            roots.iter().map(|root| root.id).collect::<Vec<_>>(),
            eligible[..4].iter().map(|root| root.id).collect::<Vec<_>>()
        );
        assert!(stratified_graph_roots(&eligible, &eligible, &eligible, 0).is_empty());
    }

    #[test]
    fn deterministic_node_id_sources_repeat_without_sharing_state() {
        let first = DeterministicNodeIdSource::new(42);
        let second = DeterministicNodeIdSource::new(42);
        let other = DeterministicNodeIdSource::new(43);
        let a: Vec<NodeId> = (0..4_096).map(|_| first.next_id()).collect();
        let b: Vec<NodeId> = (0..4_096).map(|_| second.next_id()).collect();
        let c: Vec<NodeId> = (0..4_096).map(|_| other.next_id()).collect();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.iter().copied().collect::<HashSet<_>>().len(), a.len());
        assert!(
            a.windows(2).any(|pair| pair[0] > pair[1]),
            "seeded tie order must not collapse back to corpus ingest order"
        );
        assert!(
            a.windows(2).any(|pair| pair[0] < pair[1]),
            "the permutation should not merely reverse corpus ingest order either"
        );
    }

    #[test]
    fn seeded_ordinal_permutation_is_repeatable_and_seeded() {
        let first: Vec<u64> = (1..=1_024)
            .map(|ordinal| seeded_ordinal_permutation(ordinal, 42))
            .collect();
        let repeated: Vec<u64> = (1..=1_024)
            .map(|ordinal| seeded_ordinal_permutation(ordinal, 42))
            .collect();
        let alternate: Vec<u64> = (1..=1_024)
            .map(|ordinal| seeded_ordinal_permutation(ordinal, 43))
            .collect();
        assert_eq!(first, repeated);
        assert_ne!(first, alternate);
        assert_eq!(
            first.iter().copied().collect::<HashSet<_>>().len(),
            first.len(),
            "a bijection cannot collide over distinct ordinals"
        );
    }

    #[test]
    fn rrf_rewards_cross_leg_agreement_and_keeps_spread_scale() {
        let dense_only = NodeId(Ulid::new());
        let overlap = NodeId(Ulid::new());
        let sparse_only = NodeId(Ulid::new());
        let dense = [
            Scored {
                id: dense_only,
                score: 0.8,
            },
            Scored {
                id: overlap,
                score: 0.7,
            },
        ];
        let sparse = [
            Scored {
                id: overlap,
                score: 12.0,
            },
            Scored {
                id: sparse_only,
                score: 3.0,
            },
        ];
        let fused = reciprocal_rank_fuse(&dense, &sparse, 60.0, 1.0, 1.15);
        assert_eq!(
            fused[0].id, overlap,
            "agreement should beat either solo leg"
        );
        assert!(fused.iter().any(|hit| hit.id == sparse_only));
        assert_eq!(
            fused[0].score, 0.8,
            "top score keeps the dense spread scale"
        );
        assert!(fused.iter().all(|hit| (0.0..=1.0).contains(&hit.score)));
    }

    #[test]
    fn derived_graph_overlap_cannot_leapfrog_the_direct_winner() {
        let winner = NodeId(Ulid::from(1u128));
        let overlap = NodeId(Ulid::from(2u128));
        let primary = [
            Scored {
                id: winner,
                score: 0.9,
            },
            Scored {
                id: overlap,
                score: 0.8,
            },
        ];
        let graph = [Scored {
            id: overlap,
            score: 1.0,
        }];
        let fused = reciprocal_rank_max_fuse(&primary, &graph, 60.0, 1.0, 0.9, 3);
        assert_eq!(fused[0].id, winner);
        assert_eq!(fused[1].id, overlap);
    }

    #[test]
    fn graph_evidence_can_promote_a_direct_tail_without_beating_the_winner() {
        let primary: Vec<Scored> = (0..12)
            .map(|rank| Scored {
                id: NodeId(Ulid::from(100 + rank as u128)),
                score: 1.0 - rank as f32 * 0.05,
            })
            .collect();
        let tail = primary[10].id;
        let graph = [Scored {
            id: tail,
            score: 1.0,
        }];

        let fused = reciprocal_rank_max_fuse(&primary, &graph, 60.0, 1.0, 0.9, 1);
        let promoted_rank = fused.iter().position(|hit| hit.id == tail).unwrap();
        assert_eq!(fused[0].id, primary[0].id);
        assert!(
            promoted_rank < 10,
            "a strong learned path should rescue a direct-tail hit"
        );
        assert_eq!(fused.len(), primary.len(), "promotion is not a novel slot");
    }

    #[test]
    fn graph_fusion_enforces_a_total_intervention_quota() {
        let primary: Vec<Scored> = (0..10)
            .map(|rank| Scored {
                id: NodeId(Ulid::from(100 + rank as u128)),
                score: 1.0 - rank as f32 * 0.05,
            })
            .collect();
        let mut graph = vec![Scored {
            id: primary[4].id,
            score: 1.0,
        }];
        graph.extend((0..10).map(|rank| Scored {
            id: NodeId(Ulid::from(1_000 + rank as u128)),
            score: 1.0 - rank as f32 * 0.05,
        }));

        // A hostile oversized weight still cannot beat the direct winner. One
        // overlap promotion consumes one of the three intervention slots, so
        // only two novel graph candidates may enter.
        let fused = reciprocal_rank_max_fuse(&primary, &graph, 60.0, 1.0, 99.0, 3);
        assert_eq!(fused[0].id, primary[0].id);
        let direct_ids: HashSet<NodeId> = primary.iter().map(|hit| hit.id).collect();
        assert_eq!(fused[1].id, primary[4].id);
        let untouched_direct_order: Vec<NodeId> = fused
            .iter()
            .filter(|hit| direct_ids.contains(&hit.id) && hit.id != primary[4].id)
            .map(|hit| hit.id)
            .collect();
        assert_eq!(
            untouched_direct_order,
            primary
                .iter()
                .filter(|hit| hit.id != primary[4].id)
                .map(|hit| hit.id)
                .collect::<Vec<_>>()
        );
        let graph_only_in_top_ten = fused
            .iter()
            .take(10)
            .filter(|hit| !direct_ids.contains(&hit.id))
            .count();
        assert_eq!(graph_only_in_top_ten, 2);
        assert_eq!(fused.len(), primary.len() + 2);
    }

    #[test]
    fn graph_fusion_preserves_precise_order_and_finite_scores() {
        let direct = NodeId(Ulid::from(900u128));
        let graph = NodeId(Ulid::from(1u128)); // Adverse ID tie-break.
        let tail = NodeId(Ulid::from(2u128));
        for constant in [1.0, 60.0, 1e20, f32::MAX] {
            for top in [1.0, 0.05, f32::MIN_POSITIVE, f32::from_bits(1)] {
                let primary = [
                    Scored {
                        id: direct,
                        score: top,
                    },
                    Scored {
                        id: tail,
                        score: top,
                    },
                ];
                let secondary = [Scored {
                    id: graph,
                    score: 1.0,
                }];
                let (ranked, contributed) = reciprocal_rank_max_fuse_observed(
                    &primary,
                    &secondary,
                    constant,
                    1.0,
                    1.0,
                    MAX_RETRIEVAL_RANK,
                );
                assert_eq!(ranked[0].id, direct, "constant={constant}, top={top}");
                assert!(
                    ranked
                        .iter()
                        .all(|hit| hit.score.is_finite() && hit.score > 0.0)
                );
                assert_eq!(contributed, HashSet::from([graph]));
            }
        }
    }

    #[test]
    fn capacity_graph_fusion_interleaves_without_accumulating_votes() {
        let primary: Vec<_> = (0..8)
            .map(|rank| Scored {
                id: NodeId(Ulid::from(100 + rank)),
                score: 1.0,
            })
            .collect();
        let graph: Vec<_> = (0..4)
            .map(|rank| Scored {
                id: NodeId(Ulid::from(1 + rank)),
                score: 1.0,
            })
            .collect();
        let (ranked, admitted) =
            reciprocal_rank_max_fuse_observed(&primary, &graph, 60.0, 1.0, 1.0, MAX_RETRIEVAL_RANK);
        assert_eq!(admitted.len(), 4);
        assert_eq!(
            ranked.iter().take(8).map(|hit| hit.id).collect::<Vec<_>>(),
            (0..4)
                .flat_map(|i| [primary[i].id, graph[i].id])
                .collect::<Vec<_>>()
        );
        // Four graph-only false neighbours can displace four direct-tail hits:
        // capacity-scaled admission is bounded ranking, not utility certification.
        assert_eq!(
            ranked
                .iter()
                .take(8)
                .filter(|hit| graph.iter().any(|g| g.id == hit.id))
                .count(),
            4
        );
        let (repeated, unchanged) = reciprocal_rank_max_fuse_observed(
            &primary,
            &vec![primary[0]; 32],
            60.0,
            1.0,
            1.0,
            MAX_RETRIEVAL_RANK,
        );
        assert!(unchanged.is_empty());
        assert_eq!(
            repeated.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            primary.iter().map(|hit| hit.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn graph_fusion_provenance_is_admission_not_incidental_reach() {
        let ids = [1u128, 2, 3, 4].map(|id| NodeId(Ulid::from(id)));
        let primary = [
            Scored {
                id: ids[0],
                score: 1.0,
            },
            Scored {
                id: ids[1],
                score: 0.8,
            },
            Scored {
                id: ids[2],
                score: 0.7,
            },
        ];
        let secondary = [
            Scored {
                id: ids[1],
                score: 0.9,
            },
            Scored {
                id: ids[3],
                score: 0.8,
            },
            Scored {
                id: ids[2],
                score: 0.7,
            },
        ];
        let (ranked, admitted) =
            reciprocal_rank_max_fuse_observed(&primary, &secondary, 60.0, 1.0, 1.0, 2);
        assert_eq!(
            ranked,
            reciprocal_rank_max_fuse(&primary, &secondary, 60.0, 1.0, 1.0, 2)
        );
        assert_eq!(admitted, HashSet::from([ids[1], ids[3]]));
        let (_, capped) =
            reciprocal_rank_max_fuse_observed(&primary, &secondary, 60.0, 1.0, 1.0, 1);
        assert_eq!(capped, HashSet::from([ids[1]]));
        let (_, weak) = reciprocal_rank_max_fuse_observed(&primary, &secondary, 60.0, 1.0, 0.1, 2);
        assert_eq!(
            weak,
            HashSet::from([ids[3]]),
            "direct hits reached by weak graph evidence did not use graph ranks"
        );
    }

    #[test]
    fn retrieval_provenance_rejects_fabricated_or_unbounded_hops() {
        let [a, b, c] = [1u128, 2, 3].map(|id| NodeId(Ulid::from(id)));
        let roots = HashSet::from([a]);
        let incoming = TraversalHop {
            previous: a,
            target: b,
            edge: Edge::new(b, a, 0.5, EdgeKind::Associative, 1),
        };
        assert!(validate_retrieval_path(&[incoming.clone()], b, &roots, 1).is_ok());
        assert!(validate_retrieval_path(&[incoming.clone()], b, &roots, 0).is_err());
        assert!(validate_retrieval_path(&[incoming.clone()], c, &roots, 1).is_err());
        let mut wrong_direction = incoming.clone();
        wrong_direction.edge = Edge::new(b, a, 0.5, EdgeKind::Transition, 1);
        assert!(validate_retrieval_path(&[wrong_direction], b, &roots, 1).is_err());
        let broken = TraversalHop {
            previous: a,
            target: c,
            edge: Edge::new(a, c, 0.5, EdgeKind::Transition, 1),
        };
        assert!(validate_retrieval_path(&[incoming, broken], c, &roots, 2).is_err());
        assert!(validate_retrieval_path(&[], a, &roots, 1).is_err());
    }

    #[test]
    fn non_finite_graph_weight_disables_the_secondary_leg() {
        let primary = [Scored {
            id: NodeId(Ulid::from(1u128)),
            score: 0.8,
        }];
        let graph = [Scored {
            id: NodeId(Ulid::from(2u128)),
            score: 1.0,
        }];
        for weight in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let fused = reciprocal_rank_max_fuse(&primary, &graph, 60.0, 1.0, weight, 3);
            assert_eq!(fused.len(), 1);
            assert_eq!(fused[0].id, primary[0].id);
            assert_eq!(fused[0].score, primary[0].score);
        }
    }
}
