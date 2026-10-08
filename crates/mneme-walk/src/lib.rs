//! A constrained, stateful traversal **session** — the `repl` discipline as a
//! reusable state machine, independent of how it's driven or rendered.
//!
//! The walk itself is **read-only**: from a seed you read the current node, see its
//! edges, and step along one (within a node budget), gathering a **trail**. It
//! trains nothing, so a browse — by however weak a sub-agent — can't skew the graph.
//! Walks admit semantic memories only. Episodes have their own bounded timeline,
//! history and reference reads; a directly supplied episode ID is not a shortcut
//! into the semantic walk, even under an explicit lifecycle scope.
//!
//! Training is **post-factum** and deliberate: once you know what actually fed the
//! answer, [`reflect`](WalkSession::reflect) (or [`reflect_observed`]) reinforces the
//! edges that reached the **used** nodes and banks interference only for explicitly
//! **unhelpful** nodes. Omitted nodes are unknown and receive no learning signal.
//! Observed usefulness is judged after the fact, separately from retrieval
//! relevance — not guessed at each step under pressure to keep moving.
//!
//! Methods return **typed, `Serialize`-able views** ([`View`], [`EdgeView`],
//! [`TrailStep`]); a caller serializes them to JSON (the MCP walk tool) or renders
//! them as text (the stdin repl). Rule violations come back as
//! [`WalkError::Rejected`] (report and keep going) versus [`WalkError::Backend`]
//! (an underlying engine failure).

use std::collections::{HashMap, HashSet};

use mneme_core::ports::{FeedbackRetryScope, Neighbor, StatusFilter};
use mneme_core::{EdgeKind, Node, NodeId, NodeStatus, Signal};
use mneme_engine::{
    Memory, ObservedRoute, ReceiptFeedback, ReceiptFeedbackResult, ResolvedNeighbor,
};
use serde::Serialize;

/// How many neighbors a node's edge list surfaces (weight-desc, from the engine).
const EDGE_FANOUT: usize = 64;
/// How many edges the per-step `view` shows (top by weight). The full set is a
/// step away via the `edges` action — the view stays scannable rather than
/// dumping every neighbor each move.
const VIEW_EDGES: usize = 8;
/// Blend λ for query salience: `salience = weight · ((1-λ) + λ·rel(target))`. A
/// gentle nudge — an on-query edge keeps its weight, an off-query one is halved —
/// so the ordering leans toward the query without overriding learned strength.
const SALIENCE_LAMBDA: f32 = 0.5;

pub type Result<T> = std::result::Result<T, WalkError>;

/// Either a rule the caller broke (report it; the walk continues) or an
/// underlying engine failure (usually fatal to the walk).
#[derive(Debug)]
pub enum WalkError {
    Rejected(String),
    Backend(mneme_core::ports::Error),
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::Rejected(m) => f.write_str(m),
            WalkError::Backend(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WalkError {}

impl From<mneme_core::ports::Error> for WalkError {
    fn from(e: mneme_core::ports::Error) -> Self {
        WalkError::Backend(e)
    }
}

fn reject(msg: impl Into<String>) -> WalkError {
    WalkError::Rejected(msg.into())
}

/// The shared admission boundary for every node-reading walk action. Preserve
/// the existing missing-node behavior, but never treat an immutable episode as
/// a semantic memory merely because the caller supplied its ID directly.
async fn semantic_node(mem: &Memory, id: NodeId, status: StatusFilter) -> Result<Option<Node>> {
    let node = mem.get_node(id).await?;
    if node.as_ref().is_some_and(|node| !node.is_semantic()) {
        return Err(reject(
            "episodes are not part of semantic walks; use episode reads instead",
        ));
    }
    if node
        .as_ref()
        .is_some_and(|node| !status.allows(node.status()))
    {
        return Err(reject("node is outside this walk's lifecycle scope"));
    }
    Ok(node)
}

/// One edge of the current node, as a move option.
#[derive(Serialize, Debug)]
pub struct EdgeView {
    pub i: usize,
    pub id: String,
    pub kind: &'static str,
    pub incoming: bool,
    /// The stored, query-independent edge strength.
    pub weight: f32,
    /// Query-relevance-adjusted salience, present only on a query-seeded walk: the
    /// weight nudged by how relevant the *target* is to the walk's query. The edge
    /// list (and `go` indices) order by this when present — the stored `weight` is
    /// unchanged, so this is presentation only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub salience: Option<f32>,
    pub summary: String,
}

/// The current node plus the walk's state.
#[derive(Serialize, Debug)]
pub struct View {
    pub at: String,
    pub summary: Option<String>,
    pub status: Option<&'static str>,
    pub visited: usize,
    pub budget: usize,
    pub depth: usize,
    /// The current node's top edges by weight (capped at `VIEW_EDGES`).
    pub edges: Vec<EdgeView>,
    /// Total edge count, so a truncated `edges` list is obvious (use the `edges`
    /// action for the full set).
    pub edge_count: usize,
}

/// One step of the exit trail: a node and the exact stored edge it was first
/// reached by. The start has no route fields.
#[derive(Serialize, Debug)]
pub struct TrailStep {
    pub node: String,
    pub from: Option<String>,
    pub edge_from: Option<String>,
    pub edge_to: Option<String>,
    pub incoming: Option<bool>,
}

/// What a [`reflect`](WalkSession::reflect) pass did to the graph.
#[derive(Serialize, Debug, Default)]
pub struct ReflectStats {
    pub reinforced: usize,
    pub interfered: usize,
}

pub struct WalkSession {
    /// The walk's stack; `path.last()` is the current node.
    path: Vec<NodeId>,
    /// Distinct nodes seen — what the budget caps.
    visited: HashSet<NodeId>,
    /// First-visit order, for the exit trail.
    order: Vec<NodeId>,
    /// Exact stored edge each node was first reached by (the start has none).
    routes: HashMap<NodeId, ObservedRoute>,
    budget: usize,
    /// Optional `node → similarity-to-query` map. When set, edge views carry a
    /// `salience` and order by it; purely presentational (trains nothing).
    query_rel: Option<HashMap<NodeId, f32>>,
    /// Lifecycle scope for move options. Ordinary walks are active-only; a
    /// library caller must opt in explicitly for recovery/inspection scopes.
    status: StatusFilter,
}

impl WalkSession {
    /// A **read-only** browse: free movement, gathers a trail, trains nothing.
    /// Training is a deliberate, post-factum [`reflect`](WalkSession::reflect) over
    /// the trail — so a browse, by however weak a sub-agent, can't skew the graph.
    pub fn new(start: NodeId, budget: usize) -> Self {
        let mut s = WalkSession {
            path: Vec::new(),
            visited: HashSet::new(),
            order: Vec::new(),
            routes: HashMap::new(),
            budget: budget.max(1),
            query_rel: None,
            status: StatusFilter::ACTIVE,
        };
        s.arrive(start, None);
        s
    }

    /// Attach a `node → similarity-to-query` map (e.g. from
    /// [`mneme_engine::Memory::query_relevance`]). Each edge view then carries a
    /// `salience` — the stored weight nudged by how relevant the *target* is to the
    /// query — and the edge list and `go` indices order by it, so the walk leans
    /// toward the query. No stored weight changes; the walk stays read-only.
    pub fn with_query_relevance(mut self, rel: HashMap<NodeId, f32>) -> Self {
        self.query_rel = Some(rel);
        self
    }

    /// Explicitly widen or replace the default active-only lifecycle scope.
    /// Intended for diagnostic/recovery walks; receipt reflection still refuses
    /// to reinforce an edge through any archived endpoint.
    pub fn with_status_filter(mut self, status: StatusFilter) -> Self {
        self.status = status;
        self
    }

    pub fn current(&self) -> NodeId {
        *self.path.last().expect("path is never empty")
    }

    pub fn visited_count(&self) -> usize {
        self.visited.len()
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    pub fn depth(&self) -> usize {
        self.path.len() - 1
    }

    fn arrive(&mut self, node: NodeId, via: Option<ObservedRoute>) {
        self.path.push(node);
        if self.visited.insert(node) {
            self.order.push(node);
            if let Some(route) = via {
                debug_assert_eq!(route.target, node);
                self.routes.insert(node, route);
            }
        }
    }

    /// Step to a neighbor (`arg` is an index into the weight-ordered edges or a
    /// node id). Movement is free; only the node budget gates it.
    pub async fn go(&mut self, arg: &str, mem: &Memory) -> Result<View> {
        let cur = self.current();
        semantic_node(mem, cur, self.status).await?;
        let ordered = ordered_neighbors(mem, cur, self.query_rel.as_ref(), self.status).await?;
        let neighbor = resolve_neighbor(arg, &ordered)
            .ok_or_else(|| reject(format!("{arg:?} is not an edge of the current node")))?;
        let target = neighbor.node;
        if !self.visited.contains(&target) && self.visited.len() >= self.budget {
            return Err(reject(format!(
                "node budget exhausted ({} visited)",
                self.budget
            )));
        }
        let route = ObservedRoute::new(cur, target, neighbor.edge.from, neighbor.edge.to)?;
        // Admission and hydration can fail. Build the destination view before
        // changing any path, budget, trail or receipt state.
        let mut view = self.view_at(target, mem).await?;
        self.arrive(target, Some(route));
        view.visited = self.visited.len();
        view.depth = self.depth();
        Ok(view)
    }

    pub async fn back(&mut self, mem: &Memory) -> Result<View> {
        semantic_node(mem, self.current(), self.status).await?;
        if self.path.len() <= 1 {
            return Err(reject("already at the start node"));
        }
        let previous = self.path[self.path.len() - 2];
        let mut view = self.view_at(previous, mem).await?;
        self.path.pop();
        view.depth = self.depth();
        Ok(view)
    }

    pub async fn view(&self, mem: &Memory) -> Result<View> {
        self.view_at(self.current(), mem).await
    }

    async fn view_at(&self, cur: NodeId, mem: &Memory) -> Result<View> {
        let node = semantic_node(mem, cur, self.status).await?;
        // Show only the top edges per step; the full set is the `edges` action.
        let mut edges = edge_views(mem, cur, self.query_rel.as_ref(), self.status).await?;
        let edge_count = edges.len();
        edges.truncate(VIEW_EDGES);
        Ok(View {
            at: cur.0.to_string(),
            summary: node.as_ref().map(|n| n.summary().to_string()),
            status: node.as_ref().map(|n| status_str(n.status())),
            visited: self.visited.len(),
            budget: self.budget,
            depth: self.depth(),
            edges,
            edge_count,
        })
    }

    pub async fn edges(&self, mem: &Memory) -> Result<Vec<EdgeView>> {
        semantic_node(mem, self.current(), self.status).await?;
        edge_views(mem, self.current(), self.query_rel.as_ref(), self.status).await
    }

    pub async fn body(&self, mem: &Memory) -> Result<String> {
        let body = match semantic_node(mem, self.current(), self.status).await? {
            Some(n) => mem.resolve_body(&n).await?,
            None => Vec::new(),
        };
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// Bounded variant for network-facing hosts. The boolean reports whether
    /// bytes remain beyond the returned prefix.
    pub async fn body_prefix(&self, mem: &Memory, max_bytes: usize) -> Result<(String, bool)> {
        let (body, truncated) = match semantic_node(mem, self.current(), self.status).await? {
            Some(n) => mem.resolve_body_prefix(&n, max_bytes).await?,
            None => (Vec::new(), false),
        };
        Ok((String::from_utf8_lossy(&body).into_owned(), truncated))
    }

    /// Positive-only reflection. Omitted nodes are unknown, not unhelpful.
    pub async fn reflect(&self, used: &[NodeId], mem: &Memory) -> Result<ReflectStats> {
        self.reflect_explicit(used, &[], mem).await
    }

    /// Learn from this walk's observed nodes after synthesis. `used` and
    /// `unhelpful` must be disjoint; omitted nodes receive no learning signal.
    pub async fn reflect_explicit(
        &self,
        used: &[NodeId],
        unhelpful: &[NodeId],
        mem: &Memory,
    ) -> Result<ReflectStats> {
        let used = used.iter().copied().collect();
        let unhelpful = unhelpful.iter().copied().collect();
        reflect_observed_explicit(
            mem,
            &self.observed_routes(),
            &self.visited,
            &used,
            &unhelpful,
        )
        .await
    }

    /// Exact first-visit routes in trail order. Network hosts bind these trusted
    /// observations into opaque receipts instead of reparsing rendered JSON.
    pub fn observed_routes(&self) -> Vec<ObservedRoute> {
        self.order
            .iter()
            .filter_map(|node| self.routes.get(node).copied())
            .collect()
    }

    pub fn visited_nodes(&self) -> HashSet<NodeId> {
        self.visited.clone()
    }

    pub fn trail(&self) -> Vec<TrailStep> {
        self.order
            .iter()
            .map(|id| TrailStep {
                node: id.0.to_string(),
                from: self
                    .routes
                    .get(id)
                    .map(|route| route.previous.0.to_string()),
                edge_from: self
                    .routes
                    .get(id)
                    .map(|route| route.edge_from.0.to_string()),
                edge_to: self.routes.get(id).map(|route| route.edge_to.0.to_string()),
                incoming: self.routes.get(id).map(ObservedRoute::incoming),
            })
            .collect()
    }
}

/// Positive-only reflection over exact stored routes a walk observed.
/// Omitted nodes receive no learning signal.
pub async fn reflect_observed(
    mem: &Memory,
    routes: &[ObservedRoute],
    visited: &HashSet<NodeId>,
    used: &HashSet<NodeId>,
) -> Result<ReflectStats> {
    reflect_observed_explicit(mem, routes, visited, used, &HashSet::new()).await
}

/// Post-factum reflection with explicit, disjoint positive and negative judgments.
/// Starts have no route and receive node-only effects. Unknown nodes are omitted.
pub async fn reflect_observed_explicit(
    mem: &Memory,
    routes: &[ObservedRoute],
    visited: &HashSet<NodeId>,
    used: &HashSet<NodeId>,
    unhelpful: &HashSet<NodeId>,
) -> Result<ReflectStats> {
    let events = receipt_feedback_events(routes, visited, used, unhelpful)?;
    let result = mem.apply_receipt_feedback(&events).await?;
    Ok(ReflectStats {
        reinforced: result.reinforced,
        interfered: result.interfered,
    })
}

/// Positive-only receipt reflection. Exact retries are idempotent while the
/// issuing host generation and its receipt remain live.
pub async fn reflect_observed_idempotent(
    mem: &Memory,
    key: &str,
    retry: &FeedbackRetryScope,
    routes: &[ObservedRoute],
    visited: &HashSet<NodeId>,
    used: &HashSet<NodeId>,
) -> Result<ReceiptFeedbackResult> {
    reflect_observed_explicit_idempotent(mem, key, retry, routes, visited, used, &HashSet::new())
        .await
}

/// Receipt-bound explicit reflection. `key` must remain the same server-derived
/// receipt-set identity on retry. Unknown nodes produce no learning effects;
/// an all-unknown batch may still commit its operational retry proof.
pub async fn reflect_observed_explicit_idempotent(
    mem: &Memory,
    key: &str,
    retry: &FeedbackRetryScope,
    routes: &[ObservedRoute],
    visited: &HashSet<NodeId>,
    used: &HashSet<NodeId>,
    unhelpful: &HashSet<NodeId>,
) -> Result<ReceiptFeedbackResult> {
    let events = receipt_feedback_events(routes, visited, used, unhelpful)?;
    Ok(mem
        .apply_receipt_feedback_idempotent(key, retry, &events)
        .await?)
}

fn receipt_feedback_events(
    routes: &[ObservedRoute],
    visited: &HashSet<NodeId>,
    used: &HashSet<NodeId>,
    unhelpful: &HashSet<NodeId>,
) -> Result<Vec<ReceiptFeedback>> {
    if let Some(overlap) = used.intersection(unhelpful).next() {
        return Err(reject(format!(
            "node {} cannot be both used and unhelpful",
            overlap.0,
        )));
    }
    for (label, ids) in [("used", used), ("unhelpful", unhelpful)] {
        if let Some(unvisited) = ids.iter().find(|id| !visited.contains(id)) {
            return Err(reject(format!(
                "{label} node {} was not visited by this walk receipt set",
                unvisited.0,
            )));
        }
    }
    // Route payloads come from the host, but keep this reusable entry point
    // honest too: an unobserved endpoint is not a receipt-rooted route.
    if routes
        .iter()
        .any(|route| !visited.contains(&route.previous) || !visited.contains(&route.target))
    {
        return Err(reject("receipt route endpoint was not visited"));
    }
    let routed_targets = routes
        .iter()
        .map(|route| route.target)
        .collect::<HashSet<_>>();
    let mut events = routes
        .iter()
        .filter_map(|route| {
            if used.contains(&route.target) {
                Some(ReceiptFeedback::routed(*route, true))
            } else if unhelpful.contains(&route.target) {
                Some(ReceiptFeedback::routed(*route, false))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let mut node_only = used
        .union(unhelpful)
        .filter(|id| !routed_targets.contains(id))
        .copied()
        .collect::<Vec<_>>();
    node_only.sort_unstable();
    events.extend(node_only.into_iter().map(|id| {
        if used.contains(&id) {
            ReceiptFeedback::node_used(id)
        } else {
            ReceiptFeedback::node_unhelpful(id)
        }
    }));
    Ok(events)
}

/// A node's neighbours as ordered move options, each with an optional query
/// salience. With a relevance map, `salience = weight · ((1-λ) + λ·rel(target))`
/// and the list sorts by it (desc); without one, the engine's weight-desc order is
/// kept. `go` resolves indices against this same list, so display and movement agree.
async fn ordered_neighbors(
    mem: &Memory,
    id: NodeId,
    query_rel: Option<&HashMap<NodeId, f32>>,
    status: StatusFilter,
) -> Result<Vec<(ResolvedNeighbor, Option<f32>)>> {
    let mut out: Vec<(ResolvedNeighbor, Option<f32>)> = mem
        .resolved_neighbors_scoped(id, EDGE_FANOUT, status)
        .await?
        .into_iter()
        .map(|resolved| {
            let sal = query_rel.map(|m| {
                let rel = m
                    .get(&resolved.neighbor.node)
                    .copied()
                    .unwrap_or(0.0)
                    .clamp(0.0, 1.0);
                resolved.neighbor.edge.weight() * ((1.0 - SALIENCE_LAMBDA) + SALIENCE_LAMBDA * rel)
            });
            (resolved, sal)
        })
        .collect();
    if query_rel.is_some() {
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    }
    Ok(out)
}

async fn edge_views(
    mem: &Memory,
    id: NodeId,
    query_rel: Option<&HashMap<NodeId, f32>>,
    status: StatusFilter,
) -> Result<Vec<EdgeView>> {
    let mut out = Vec::new();
    for (i, (resolved, salience)) in ordered_neighbors(mem, id, query_rel, status)
        .await?
        .into_iter()
        .enumerate()
    {
        let nb = resolved.neighbor;
        out.push(EdgeView {
            i,
            id: nb.node.0.to_string(),
            kind: kind_str(nb.edge.kind),
            incoming: nb.incoming,
            weight: nb.edge.weight(),
            salience,
            summary: resolved.node.summary().to_string(),
        });
    }
    Ok(out)
}

/// Resolve a `go` argument (an index into the ordered edge list, or a neighbor id)
/// to a neighbor node id. The index is into the *same* ordering the view shows.
fn resolve_neighbor<'a>(
    arg: &str,
    ordered: &'a [(ResolvedNeighbor, Option<f32>)],
) -> Option<&'a Neighbor> {
    if let Ok(i) = arg.parse::<usize>()
        && let Some((resolved, _)) = ordered.get(i)
    {
        return Some(&resolved.neighbor);
    }
    ordered
        .iter()
        .find(|(resolved, _)| resolved.neighbor.node.0.to_string() == arg)
        .map(|(resolved, _)| &resolved.neighbor)
}

pub fn kind_str(k: EdgeKind) -> &'static str {
    match k {
        EdgeKind::Associative => "associative",
        EdgeKind::Transition => "transition",
        EdgeKind::Bridge => "bridge",
        EdgeKind::Supersedes => "supersedes",
        EdgeKind::DerivedFrom => "derived_from",
    }
}

pub fn status_str(s: NodeStatus) -> &'static str {
    match s {
        NodeStatus::Active => "active",
        NodeStatus::Archived => "archived",
    }
}

pub fn signal_str(s: Signal) -> &'static str {
    match s {
        Signal::RelevantNew => "relevant",
        Signal::NotNew => "not-new",
        Signal::Irrelevant => "irrelevant",
    }
}

/// Parse a signal, accepting the repl's short aliases and the MCP spellings.
pub fn parse_signal(s: &str) -> Option<Signal> {
    match s.to_ascii_lowercase().as_str() {
        "relevant" | "relevant-new" | "rel" | "r" => Some(Signal::RelevantNew),
        "not-new" | "notnew" | "nn" | "n" => Some(Signal::NotNew),
        "irrelevant" | "irr" | "i" => Some(Signal::Irrelevant),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use mneme_core::episode::OccurrenceSpan;
    use mneme_core::ports::{
        Budget, Clock, ColdPath, DensePruneChunkOutcome, FeedbackCommit, FeedbackCommitOutcome,
        FeedbackRetryScope, FullMergeCommit, FullMergeCommitOutcome, GraphStore, MaintenanceCommit,
        MaintenanceCommitOutcome, MaintenanceEdgeKey, MaintenanceEdgePage, MaintenanceNodePage,
        Result as PortResult, SupersedeCommit, SupersedeCommitOutcome, SystemClock,
        TaggedPhysicalStatus, Traversal, VectorIndex,
    };
    use mneme_core::{
        Contradiction, Edge, MergeCandidate, MergeResolution, Provenance, RemoteEdge,
        RemoteEdgeCursor, RemoteEdgePage, Resolution, Timestamp,
    };
    use mneme_engine::{Capture, CaptureLink, Config, EpisodeWrite, Ingest};
    use ulid::Ulid;
    use unordered_pair::UnorderedPair;

    struct CountingGraphStore {
        inner: Arc<mneme_cozo::MemStore>,
        point_reads: AtomicUsize,
        batch_reads: AtomicUsize,
        status_reads: AtomicUsize,
        neighbor_reads: AtomicUsize,
    }

    impl CountingGraphStore {
        fn new(inner: Arc<mneme_cozo::MemStore>) -> Self {
            Self {
                inner,
                point_reads: AtomicUsize::new(0),
                batch_reads: AtomicUsize::new(0),
                status_reads: AtomicUsize::new(0),
                neighbor_reads: AtomicUsize::new(0),
            }
        }

        fn reset_reads(&self) {
            self.point_reads.store(0, Ordering::Relaxed);
            self.batch_reads.store(0, Ordering::Relaxed);
            self.status_reads.store(0, Ordering::Relaxed);
            self.neighbor_reads.store(0, Ordering::Relaxed);
        }
    }

    #[async_trait::async_trait]
    impl GraphStore for CountingGraphStore {
        async fn put_node(&self, node: &mneme_core::Node) -> PortResult<()> {
            self.inner.put_node(node).await
        }

        async fn get_node(&self, id: NodeId) -> PortResult<Option<mneme_core::Node>> {
            self.point_reads.fetch_add(1, Ordering::Relaxed);
            self.inner.get_node(id).await
        }

        async fn get_nodes(&self, ids: &[NodeId]) -> PortResult<Vec<Option<mneme_core::Node>>> {
            self.batch_reads.fetch_add(1, Ordering::Relaxed);
            self.inner.get_nodes(ids).await
        }

        async fn get_node_statuses(
            &self,
            ids: &[NodeId],
        ) -> PortResult<Vec<Option<TaggedPhysicalStatus>>> {
            self.status_reads.fetch_add(1, Ordering::Relaxed);
            self.inner.get_node_statuses(ids).await
        }

        async fn delete_node(&self, id: NodeId) -> PortResult<()> {
            self.inner.delete_node(id).await
        }

        async fn all_nodes(&self, cold: ColdPath) -> PortResult<Vec<mneme_core::Node>> {
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
            self.inner.put_edge(edge).await
        }

        async fn delete_edge(&self, from: NodeId, to: NodeId) -> PortResult<()> {
            self.inner.delete_edge(from, to).await
        }

        async fn get_edge(&self, from: NodeId, to: NodeId) -> PortResult<Option<Edge>> {
            self.inner.get_edge(from, to).await
        }

        async fn all_edges(&self, cold: ColdPath) -> PortResult<Vec<Edge>> {
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

        async fn neighbors(&self, id: NodeId, top_k: usize) -> PortResult<Vec<Neighbor>> {
            self.neighbor_reads.fetch_add(1, Ordering::Relaxed);
            self.inner.neighbors(id, top_k).await
        }

        async fn commit_feedback(
            &self,
            commit: &FeedbackCommit,
        ) -> PortResult<FeedbackCommitOutcome> {
            self.inner.commit_feedback(commit).await
        }

        async fn commit_full_merge(
            &self,
            commit: &FullMergeCommit,
        ) -> PortResult<FullMergeCommitOutcome> {
            self.inner.commit_full_merge(commit).await
        }

        async fn commit_supersede(
            &self,
            commit: &SupersedeCommit,
        ) -> PortResult<SupersedeCommitOutcome> {
            self.inner.commit_supersede(commit).await
        }

        async fn commit_maintenance(
            &self,
            cold: ColdPath,
            commit: &MaintenanceCommit,
        ) -> PortResult<MaintenanceCommitOutcome> {
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
            self.inner
                .prune_incident_associations(cold, hub, target_degree, max_deletes)
                .await
        }

        async fn observe_contradiction(
            &self,
            a: NodeId,
            b: NodeId,
            at: Timestamp,
        ) -> PortResult<()> {
            self.inner.observe_contradiction(a, b, at).await
        }

        async fn open_contradictions(&self, cold: ColdPath) -> PortResult<Vec<Contradiction>> {
            self.inner.open_contradictions(cold).await
        }

        async fn resolve_contradiction(
            &self,
            pair: UnorderedPair<NodeId>,
            resolution: Resolution,
        ) -> PortResult<()> {
            self.inner.resolve_contradiction(pair, resolution).await
        }

        async fn observe_merge_candidate(
            &self,
            a: NodeId,
            b: NodeId,
            at: Timestamp,
        ) -> PortResult<()> {
            self.inner.observe_merge_candidate(a, b, at).await
        }

        async fn open_merge_candidates(&self, cold: ColdPath) -> PortResult<Vec<MergeCandidate>> {
            self.inner.open_merge_candidates(cold).await
        }

        async fn resolve_merge_candidate(
            &self,
            pair: UnorderedPair<NodeId>,
            resolution: MergeResolution,
        ) -> PortResult<()> {
            self.inner.resolve_merge_candidate(pair, resolution).await
        }

        async fn put_remote_edge(&self, edge: &RemoteEdge) -> PortResult<()> {
            self.inner.put_remote_edge(edge).await
        }

        async fn remote_edges_page(
            &self,
            from: NodeId,
            after: Option<RemoteEdgeCursor>,
            limit: usize,
        ) -> PortResult<RemoteEdgePage> {
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

    fn empty_memory() -> (Memory, Arc<mneme_cozo::MemStore>) {
        let dim = mneme_embed::DEFAULT_DIM;
        let store = Arc::new(mneme_cozo::MemStore::new(dim));
        let graph: Arc<dyn GraphStore> = store.clone();
        let vectors: Arc<dyn VectorIndex> = store.clone();
        let traversal: Arc<dyn Traversal> = store.clone();
        let embedder = Arc::new(mneme_embed::HashingEmbedder::new(dim));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        // Floor off: the lexical embedder scores disjoint summaries near zero, so
        // the only edges are the explicit ones tests add.
        let cfg = Config {
            min_similarity_links: 0,
            ..Config::default()
        };
        let mem = Memory::new(graph, vectors, traversal, embedder, clock, cfg)
            .with_body_store(Arc::new(mneme_body::InlineStore::new()));
        (mem, store)
    }

    fn counting_memory() -> (Memory, Arc<CountingGraphStore>, Arc<mneme_cozo::MemStore>) {
        let dim = mneme_embed::DEFAULT_DIM;
        let store = Arc::new(mneme_cozo::MemStore::new(dim));
        let graph = Arc::new(CountingGraphStore::new(store.clone()));
        let vectors: Arc<dyn VectorIndex> = store.clone();
        let traversal: Arc<dyn Traversal> = store.clone();
        let embedder = Arc::new(mneme_embed::HashingEmbedder::new(dim));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let cfg = Config {
            min_similarity_links: 0,
            ..Config::default()
        };
        let mem = Memory::new(graph.clone(), vectors, traversal, embedder, clock, cfg)
            .with_body_store(Arc::new(mneme_body::InlineStore::new()));
        (mem, graph, store)
    }

    /// A two-node graph A→B over the reference adapters; returns (mem, a, b).
    async fn fixture() -> (Memory, Arc<mneme_cozo::MemStore>, NodeId, NodeId) {
        let (mem, store) = empty_memory();

        let prov = Provenance::derived_empty;
        let a = mem
            .ingest(Ingest::new("alpha", b"alpha", &[], prov()))
            .await
            .unwrap();
        let b = mem
            .ingest(Ingest::new("beta", b"beta", &[], prov()))
            .await
            .unwrap();
        mem.link(a, b, EdgeKind::Associative, 0.5, None)
            .await
            .unwrap();
        (mem, store, a, b)
    }

    async fn edge_weight(mem: &Memory, from: NodeId, to: NodeId) -> f32 {
        mem.neighbors(from, 64)
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.node == to)
            .map(|n| n.edge.weight())
            .expect("edge present")
    }

    async fn append_episode(mem: &Memory, key: &str, links: &[CaptureLink]) -> NodeId {
        mem.append_episode(
            EpisodeWrite::new(
                "walk-test",
                key,
                "walk episode admission fixture",
                None,
                None,
                "A scene, not a semantic walk node",
                b"A preserved historical account.",
                &[],
                OccurrenceSpan::Unknown,
                None,
            )
            .with_links(links),
        )
        .await
        .unwrap()
        .identity
        .edition_id
    }

    fn assert_episode_rejection<T: std::fmt::Debug>(result: Result<T>) {
        match result {
            Err(WalkError::Rejected(message)) => {
                assert!(message.contains("episode reads"), "got: {message}");
            }
            other => panic!("expected an actionable episode rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn direct_episode_start_is_rejected_by_every_node_read_without_state_changes() {
        let (mem, _store) = empty_memory();
        let episode = append_episode(&mem, "direct-start", &[]).await;

        // Lifecycle scopes never widen the memory-kind boundary. Explicit
        // inspection can still read the exact edition outside a semantic walk.
        assert!(
            mem.get_node(episode)
                .await
                .unwrap()
                .unwrap()
                .episode()
                .is_some()
        );
        for status in [StatusFilter::ACTIVE, StatusFilter::ALL] {
            let mut walk = WalkSession::new(episode, 4).with_status_filter(status);
            assert_episode_rejection(walk.view(&mem).await);
            assert_episode_rejection(walk.edges(&mem).await);
            assert_episode_rejection(walk.body(&mem).await);
            assert_episode_rejection(walk.body_prefix(&mem, 12).await);
            assert_episode_rejection(walk.go("0", &mem).await);
            assert_episode_rejection(walk.back(&mem).await);
            assert_eq!(walk.path, vec![episode]);
            assert_eq!(walk.visited, HashSet::from([episode]));
            assert_eq!(walk.order, vec![episode]);
            assert!(walk.routes.is_empty());
        }
    }

    #[tokio::test]
    async fn back_admits_destination_before_changing_the_path() {
        let (mem, _store, semantic, _) = fixture().await;
        let episode = append_episode(&mem, "back-target", &[]).await;

        // A normal walk cannot build this mixed path. Model a restored stale
        // session explicitly to exercise the destination-admission boundary.
        let mut walk = WalkSession::new(episode, 4);
        walk.arrive(semantic, None);
        let before_path = walk.path.clone();
        let before_visited = walk.visited.clone();
        let before_order = walk.order.clone();
        assert_episode_rejection(walk.back(&mem).await);
        assert_eq!(walk.path, before_path);
        assert_eq!(walk.visited, before_visited);
        assert_eq!(walk.order, before_order);
        assert!(walk.routes.is_empty());
    }

    #[tokio::test]
    async fn episode_neighbors_cannot_spend_semantic_walk_fanout_or_bypass_it_by_id() {
        let (mem, _store, hub, semantic) = fixture().await;
        let mut episodes = Vec::new();
        let links = [CaptureLink::new(hub, EdgeKind::Associative, 0.9).unwrap()];
        for index in 0..EDGE_FANOUT {
            episodes.push(append_episode(&mem, &format!("neighbor-{index}"), &links).await);
        }

        for status in [StatusFilter::ACTIVE, StatusFilter::ALL] {
            let mut walk = WalkSession::new(hub, 4).with_status_filter(status);
            let edges = walk.edges(&mem).await.unwrap();
            assert_eq!(edges.len(), 1);
            assert_eq!(edges[0].id, semantic.0.to_string());
            assert!(matches!(
                walk.go(&episodes[0].0.to_string(), &mem).await,
                Err(WalkError::Rejected(_))
            ));
            assert_eq!(walk.current(), hub);
            assert_eq!(walk.visited_count(), 1);
            assert!(walk.observed_routes().is_empty());
            assert_eq!(walk.go("0", &mem).await.unwrap().at, semantic.0.to_string());
        }
    }

    #[tokio::test]
    async fn back_view_describes_the_restored_position_and_preserves_the_trail() {
        let (mem, _store, a, b) = fixture().await;
        let mut walk = WalkSession::new(a, 3);
        let forward = walk.go("0", &mem).await.unwrap();
        assert_eq!((forward.visited, forward.depth), (2, 1));
        assert_eq!(forward.at, b.0.to_string());
        let back = walk.back(&mem).await.unwrap();
        assert_eq!((back.visited, back.depth), (2, 0));
        assert_eq!(back.at, a.0.to_string());
        assert_eq!(walk.trail().len(), 2);
    }

    #[tokio::test]
    async fn walk_gathers_a_trail_without_training() {
        let (mem, _store, a, b) = fixture().await;
        let w0 = edge_weight(&mem, a, b).await;

        // Free movement, no signalling; the walk alone mutates nothing.
        let mut s = WalkSession::new(a, 5);
        let view = s.go("0", &mem).await.unwrap();
        assert_eq!(view.at, b.0.to_string(), "stepped to B");
        assert_eq!(s.trail().len(), 2, "trail records A then B");
        assert_eq!(
            edge_weight(&mem, a, b).await,
            w0,
            "browsing didn't change the edge"
        );
    }

    #[tokio::test]
    async fn default_walk_filters_archived_nodes_before_fanout() {
        let (mem, store) = empty_memory();
        let prov = Provenance::derived_empty;
        let hub = mem
            .ingest(Ingest::new("hub", b"", &[], prov()))
            .await
            .unwrap();

        // Fill the raw top-64 window with stronger ineligible edges. A
        // post-truncation filter would return no move and hide the weaker active
        // neighbor; lifecycle filtering before fanout must still surface it.
        let mut archived = Vec::new();
        for i in 0..(EDGE_FANOUT + 1) {
            let summary = format!("archived {i}");
            let id = mem
                .ingest(Ingest::new(&summary, b"", &[], prov()))
                .await
                .unwrap();
            store.set_status(id, NodeStatus::Archived).await.unwrap();
            mem.link(hub, id, EdgeKind::Associative, 0.9, None)
                .await
                .unwrap();
            archived.push(id);
        }
        let active = mem
            .ingest(Ingest::new("active", b"", &[], prov()))
            .await
            .unwrap();
        mem.link(hub, active, EdgeKind::Associative, 0.1, None)
            .await
            .unwrap();

        let mut walk = WalkSession::new(hub, 3);
        let edges = walk.edges(&mem).await.unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].id, active.0.to_string());
        assert!(
            walk.go(&archived[0].0.to_string(), &mem).await.is_err(),
            "an archived id must not bypass the filtered move list"
        );
        assert_eq!(walk.go("0", &mem).await.unwrap().at, active.0.to_string());

        // Recovery inspection is possible only through an explicit lifecycle
        // scope; it never changes the ordinary active-only default.
        let archived_edges = WalkSession::new(hub, 3)
            .with_status_filter(StatusFilter::ALL)
            .edges(&mem)
            .await
            .unwrap();
        assert_eq!(archived_edges.len(), EDGE_FANOUT);
    }

    #[tokio::test]
    async fn archived_roots_require_an_explicit_walk_scope_even_after_session_creation() {
        let (mem, store) = empty_memory();
        let prov = Provenance::derived_empty;
        let root = mem
            .ingest(Ingest::new("root", b"root body", &[], prov()))
            .await
            .unwrap();
        let target = mem
            .ingest(Ingest::new("target", b"", &[], prov()))
            .await
            .unwrap();
        mem.link(root, target, EdgeKind::Associative, 0.8, None)
            .await
            .unwrap();

        let mut started_while_active = WalkSession::new(root, 3);
        assert_eq!(started_while_active.edges(&mem).await.unwrap().len(), 1);
        store.set_status(root, NodeStatus::Archived).await.unwrap();

        assert!(
            mem.neighbors_scoped(root, 1, StatusFilter::ACTIVE)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            mem.resolved_neighbors_scoped(root, 1, StatusFilter::ACTIVE)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(started_while_active.view(&mem).await.is_err());
        assert!(started_while_active.edges(&mem).await.is_err());
        assert!(started_while_active.body(&mem).await.is_err());
        assert!(started_while_active.go("0", &mem).await.is_err());
        assert_eq!(started_while_active.current(), root);
        assert_eq!(started_while_active.visited_count(), 1);
        assert!(started_while_active.observed_routes().is_empty());

        let mut explicit_all = WalkSession::new(root, 3).with_status_filter(StatusFilter::ALL);
        assert_eq!(
            explicit_all.view(&mem).await.unwrap().at,
            root.0.to_string()
        );
        assert_eq!(
            explicit_all.go("0", &mem).await.unwrap().at,
            target.0.to_string()
        );

        store
            .set_status(target, NodeStatus::Archived)
            .await
            .unwrap();
        let mut explicit_archived =
            WalkSession::new(root, 3).with_status_filter(StatusFilter::ARCHIVED);
        assert_eq!(
            explicit_archived.go("0", &mem).await.unwrap().at,
            target.0.to_string()
        );
    }

    #[tokio::test]
    async fn fresh_low_stability_capture_is_a_search_hit_and_walk_endpoint_until_archived() {
        let (mem, store) = empty_memory();
        let hub = mem
            .ingest(Ingest::new(
                "deployment hub",
                b"",
                &[],
                Provenance::derived_empty(),
            ))
            .await
            .unwrap();
        let links = [CaptureLink::new(hub, EdgeKind::Associative, 0.8).unwrap()];
        let fresh = mem
            .capture(
                Capture::new(
                    "walk-test",
                    "fresh-fix",
                    "test://fresh-fix",
                    None,
                    None,
                    "deployment fix: clear the stale cache",
                    b"the fix",
                    &[],
                )
                .with_stability(0.01)
                .with_links(&links),
            )
            .await
            .unwrap()
            .id;
        let hits = mem
            .retrieve_batch_seeded(
                "deployment fix: clear the stale cache",
                5,
                Budget {
                    max_depth: 0,
                    ..Budget::default()
                },
                StatusFilter::ACTIVE,
                &[],
            )
            .await
            .unwrap();
        assert!(hits.primary.iter().any(|hit| hit.node.id() == fresh));
        let edges = WalkSession::new(hub, 3).edges(&mem).await.unwrap();
        assert!(edges.iter().any(|edge| edge.id == fresh.0.to_string()));

        store.set_status(fresh, NodeStatus::Archived).await.unwrap();
        let edges = WalkSession::new(hub, 3).edges(&mem).await.unwrap();
        assert!(edges.iter().all(|edge| edge.id != fresh.0.to_string()));
    }

    #[tokio::test]
    async fn walk_edge_views_use_one_admission_read_and_batched_neighbor_hydration() {
        let (mem, counted, store) = counting_memory();
        let prov = Provenance::derived_empty;
        let hub = mem
            .ingest(Ingest::new("hub", b"", &[], prov()))
            .await
            .unwrap();
        for offset in 0..4 {
            let node = mem
                .ingest(Ingest::new(&format!("archived {offset}"), b"", &[], prov()))
                .await
                .unwrap();
            mem.link(hub, node, EdgeKind::Associative, 0.9, None)
                .await
                .unwrap();
            store.set_status(node, NodeStatus::Archived).await.unwrap();
        }
        for offset in 0..8 {
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
        store.delete_node(missing).await.unwrap();

        counted.reset_reads();
        let edges = WalkSession::new(hub, 16).edges(&mem).await.unwrap();
        assert_eq!(edges.len(), 8);
        assert!(edges.iter().all(|edge| edge.summary.starts_with("active ")));
        assert_eq!(
            counted.point_reads.load(Ordering::Relaxed),
            1,
            "one source-admission read, never a point read per neighbor"
        );
        assert_eq!(counted.neighbor_reads.load(Ordering::Relaxed), 1);
        assert_eq!(counted.status_reads.load(Ordering::Relaxed), 1);
        assert_eq!(counted.batch_reads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn reflect_reinforces_used_and_interferes_only_explicit_unhelpful() {
        // Used node → edge reinforced (immediate weight bump).
        let (mem, _store, a, b) = fixture().await;
        let w0 = edge_weight(&mem, a, b).await;
        let mut s = WalkSession::new(a, 5);
        s.go("0", &mem).await.unwrap(); // A -> B
        let stats = s.reflect(&[b], &mem).await.unwrap();
        assert_eq!((stats.reinforced, stats.interfered), (1, 0));
        assert!(
            edge_weight(&mem, a, b).await > w0,
            "reflect(used) reinforced A→B"
        );

        // Explicit unhelpful route → edge interference, never node decay debt.
        let (mem, store, a, b) = fixture().await;
        let mut s = WalkSession::new(a, 5);
        s.go("0", &mem).await.unwrap();
        let stats = s.reflect_explicit(&[], &[b], &mem).await.unwrap();
        assert_eq!((stats.reinforced, stats.interfered), (0, 1));
        assert_eq!(mem.get_node(b).await.unwrap().unwrap().interference(), 0);
        assert!(store.get_edge(a, b).await.unwrap().unwrap().interference() > 0);
    }

    fn assert_node_unchanged(before: &mneme_core::Node, after: &mneme_core::Node) {
        assert!(before.same_feedback_static_fields(after));
        assert_eq!(before.confidence(), after.confidence());
        assert_eq!(before.grounded_use_count(), after.grounded_use_count());
        assert_eq!(before.last_grounded_use(), after.last_grounded_use());
        assert_eq!(before.interference(), after.interference());
        assert_eq!(before.status(), after.status());
    }

    #[tokio::test]
    async fn omitted_nodes_and_routes_receive_no_learning_effects() {
        let (mem, store, a, b) = fixture().await;
        let c = mem
            .ingest(Ingest::new("gamma", b"", &[], Provenance::derived_empty()))
            .await
            .unwrap();
        mem.link(b, c, EdgeKind::Associative, 0.3, None)
            .await
            .unwrap();
        let mut walk = WalkSession::new(a, 3);
        walk.go(&b.0.to_string(), &mem).await.unwrap();
        walk.go(&c.0.to_string(), &mem).await.unwrap();
        let a_before = mem.get_node(a).await.unwrap().unwrap();
        let b_before = mem.get_node(b).await.unwrap().unwrap();
        let c_before = mem.get_node(c).await.unwrap().unwrap();
        let ab_before = store.get_edge(a, b).await.unwrap().unwrap();
        let bc_before = store.get_edge(b, c).await.unwrap().unwrap();

        let none = walk.reflect(&[], &mem).await.unwrap();
        assert_eq!((none.reinforced, none.interfered), (0, 0));
        for (id, before) in [(a, &a_before), (b, &b_before), (c, &c_before)] {
            assert_node_unchanged(before, &mem.get_node(id).await.unwrap().unwrap());
        }
        assert_eq!(
            store.get_edge(a, b).await.unwrap().unwrap().trials(),
            ab_before.trials()
        );
        assert_eq!(
            store.get_edge(b, c).await.unwrap().unwrap().trials(),
            bc_before.trials()
        );

        let some = walk.reflect(&[b], &mem).await.unwrap();
        assert_eq!((some.reinforced, some.interfered), (1, 0));
        assert_node_unchanged(&a_before, &mem.get_node(a).await.unwrap().unwrap());
        assert_node_unchanged(&c_before, &mem.get_node(c).await.unwrap().unwrap());
        let bc_after = store.get_edge(b, c).await.unwrap().unwrap();
        assert_eq!(bc_after.weight(), bc_before.weight());
        assert_eq!(bc_after.trials(), bc_before.trials());
        assert_eq!(bc_after.interference(), bc_before.interference());
    }

    #[tokio::test]
    async fn explicit_judgments_must_be_disjoint_and_observed() {
        let (mem, store, a, b) = fixture().await;
        let mut walk = WalkSession::new(a, 2);
        walk.go(&b.0.to_string(), &mem).await.unwrap();
        let before = mem.get_node(b).await.unwrap().unwrap();
        let edge_before = store.get_edge(a, b).await.unwrap().unwrap();
        assert!(walk.reflect_explicit(&[b], &[b], &mem).await.is_err());
        let unseen = NodeId(ulid::Ulid::new());
        assert!(walk.reflect_explicit(&[], &[unseen], &mem).await.is_err());
        assert_node_unchanged(&before, &mem.get_node(b).await.unwrap().unwrap());
        assert_eq!(
            store.get_edge(a, b).await.unwrap().unwrap().trials(),
            edge_before.trials()
        );
    }

    #[tokio::test]
    async fn explicit_start_negative_is_node_only_and_retry_safe() {
        let (mem, store, a, b) = fixture().await;
        let edge_before = store.get_edge(a, b).await.unwrap().unwrap();
        let visited = HashSet::from([a]);
        let unhelpful = HashSet::from([a]);
        let retry = FeedbackRetryScope::new("explicit-negative-epoch", 1, 1).unwrap();
        let first = reflect_observed_explicit_idempotent(
            &mem,
            "negative-start",
            &retry,
            &[],
            &visited,
            &HashSet::new(),
            &unhelpful,
        )
        .await
        .unwrap();
        assert_eq!(first.commit, FeedbackCommitOutcome::Applied);
        assert_eq!((first.reinforced, first.interfered), (0, 1));
        let after = mem.get_node(a).await.unwrap().unwrap();
        assert_eq!(after.interference(), 0);
        assert_eq!(after.grounded_use_count(), 0);
        let replay = reflect_observed_explicit_idempotent(
            &mem,
            "negative-start",
            &retry,
            &[],
            &visited,
            &HashSet::new(),
            &unhelpful,
        )
        .await
        .unwrap();
        assert_eq!(replay.commit, FeedbackCommitOutcome::AlreadyApplied);
        assert_node_unchanged(&after, &mem.get_node(a).await.unwrap().unwrap());
        assert_eq!(
            store.get_edge(a, b).await.unwrap().unwrap().trials(),
            edge_before.trials()
        );
        assert!(
            reflect_observed_explicit_idempotent(
                &mem,
                "negative-start",
                &retry,
                &[],
                &visited,
                &HashSet::from([a]),
                &HashSet::new(),
            )
            .await
            .is_err(),
            "a changed judgment cannot reuse the receipt"
        );
    }

    #[tokio::test]
    async fn repeated_routes_credit_node_and_edge_once_per_batch() {
        let (mem, store, a, b) = fixture().await;
        let route = ObservedRoute::new(a, b, a, b).unwrap();
        let retry = FeedbackRetryScope::new("dedup-epoch", 1, 1).unwrap();
        let visited = HashSet::from([a, b]);
        let used = HashSet::from([b]);
        let first = reflect_observed_explicit_idempotent(
            &mem,
            "dedup",
            &retry,
            &[route, route],
            &visited,
            &used,
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!((first.reinforced, first.interfered), (1, 0));
        assert_eq!(
            mem.get_node(b).await.unwrap().unwrap().grounded_use_count(),
            1
        );
        assert_eq!(store.get_edge(a, b).await.unwrap().unwrap().trials(), 1);
        let replay = reflect_observed_explicit_idempotent(
            &mem,
            "dedup",
            &retry,
            &[route],
            &visited,
            &used,
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(replay.commit, FeedbackCommitOutcome::AlreadyApplied);
    }

    #[tokio::test]
    async fn shared_stored_arrow_is_counted_once_or_left_unknown_on_conflict() {
        for conflicting in [false, true] {
            let (mem, store, a, b) = fixture().await;
            let before = store.get_edge(a, b).await.unwrap().unwrap();
            // Two independent walks observed opposite directions of one
            // logically undirected association, not two different edges.
            let routes = [
                ObservedRoute::new(a, b, a, b).unwrap(),
                ObservedRoute::new(b, a, a, b).unwrap(),
            ];
            let used = if conflicting {
                HashSet::from([b])
            } else {
                HashSet::from([a, b])
            };
            let unhelpful = if conflicting {
                HashSet::from([a])
            } else {
                HashSet::new()
            };
            reflect_observed_explicit(&mem, &routes, &HashSet::from([a, b]), &used, &unhelpful)
                .await
                .unwrap();
            assert_eq!(
                mem.get_node(b).await.unwrap().unwrap().grounded_use_count(),
                1
            );
            let after = store.get_edge(a, b).await.unwrap().unwrap();
            if conflicting {
                assert_eq!(mem.get_node(a).await.unwrap().unwrap().interference(), 0);
                assert_eq!(after.weight(), before.weight());
                assert_eq!(after.trials(), before.trials());
                assert_eq!(after.interference(), before.interference());
            } else {
                assert_eq!(
                    mem.get_node(a).await.unwrap().unwrap().grounded_use_count(),
                    1
                );
                assert_eq!(after.trials(), before.trials() + 1);
            }
        }
    }

    #[tokio::test]
    async fn unknown_receipt_consumption_changes_only_retry_bookkeeping() {
        let (mem, store, a, b) = fixture().await;
        let before = mem.get_node(b).await.unwrap().unwrap();
        let edge_before = store.get_edge(a, b).await.unwrap().unwrap();
        let route = ObservedRoute::new(a, b, a, b).unwrap();
        let retry = FeedbackRetryScope::new("unknown-epoch", 1, 1).unwrap();
        let visited = HashSet::from([a, b]);
        for expected in [
            FeedbackCommitOutcome::Applied,
            FeedbackCommitOutcome::AlreadyApplied,
        ] {
            let result = reflect_observed_explicit_idempotent(
                &mem,
                "unknown",
                &retry,
                &[route],
                &visited,
                &HashSet::new(),
                &HashSet::new(),
            )
            .await
            .unwrap();
            assert_eq!(result.commit, expected);
            assert_eq!((result.reinforced, result.interfered), (0, 0));
            assert_node_unchanged(&before, &mem.get_node(b).await.unwrap().unwrap());
            let edge = store.get_edge(a, b).await.unwrap().unwrap();
            assert_eq!(edge.weight(), edge_before.weight());
            assert_eq!(edge.trials(), edge_before.trials());
            assert_eq!(edge.interference(), edge_before.interference());
        }
    }

    #[tokio::test]
    async fn incoming_hop_trains_only_the_observed_stored_arrow() {
        let (mem, store, a, b) = fixture().await;
        store.delete_edge(a, b).await.unwrap();
        mem.link(b, a, EdgeKind::Supersedes, 0.25, None)
            .await
            .unwrap();
        let before = store.get_edge(b, a).await.unwrap().unwrap();
        assert!(store.get_edge(a, b).await.unwrap().is_none());

        let mut session = WalkSession::new(a, 2);
        session.go(&b.0.to_string(), &mem).await.unwrap();
        let trail = session.trail();
        assert_eq!(trail[1].incoming, Some(true));
        assert_eq!(
            trail[1].edge_from.as_deref(),
            Some(b.0.to_string().as_str())
        );
        assert_eq!(trail[1].edge_to.as_deref(), Some(a.0.to_string().as_str()));

        session.reflect(&[b], &mem).await.unwrap();
        let after = store.get_edge(b, a).await.unwrap().unwrap();
        assert_eq!(after.trials(), before.trials() + 1);
        assert!(after.weight() > before.weight());
        assert!(
            store.get_edge(a, b).await.unwrap().is_none(),
            "incoming traversal must not synthesize the opposite arrow"
        );
    }

    #[tokio::test]
    async fn disappeared_observed_route_is_not_recreated() {
        let (mem, store, a, b) = fixture().await;
        let mut session = WalkSession::new(a, 2);
        session.go(&b.0.to_string(), &mem).await.unwrap();
        store.delete_edge(a, b).await.unwrap();

        let stats = session.reflect(&[b], &mem).await.unwrap();
        assert_eq!((stats.reinforced, stats.interfered), (1, 0));
        assert!(
            store.get_edge(a, b).await.unwrap().is_none(),
            "reflection may credit the used node but never recreate a vanished route"
        );
        assert_eq!(
            mem.get_node(b).await.unwrap().unwrap().grounded_use_count(),
            1
        );
    }

    #[tokio::test]
    async fn archived_route_endpoints_keep_node_credit_but_never_edge_reinforcement() {
        // The target can be archived after traversal but before reflection.
        let (mem, store, a, b) = fixture().await;
        let mut session = WalkSession::new(a, 2);
        session.go(&b.0.to_string(), &mem).await.unwrap();
        let before = store.get_edge(a, b).await.unwrap().unwrap();
        store.set_status(b, NodeStatus::Archived).await.unwrap();

        let stats = session.reflect(&[b], &mem).await.unwrap();
        assert_eq!((stats.reinforced, stats.interfered), (1, 0));
        let after = store.get_edge(a, b).await.unwrap().unwrap();
        assert_eq!(after.trials(), before.trials());
        assert_eq!(after.weight(), before.weight());
        let target = mem.get_node(b).await.unwrap().unwrap();
        assert!(target.is_archived());
        assert_eq!(target.grounded_use_count(), 1, "used target keeps credit");

        // The previous endpoint is equally capable of turning an old receipt
        // into zombie reinforcement; block that direction too.
        let (mem, store, a, b) = fixture().await;
        let mut session = WalkSession::new(a, 2);
        session.go(&b.0.to_string(), &mem).await.unwrap();
        let before = store.get_edge(a, b).await.unwrap().unwrap();
        store.set_status(a, NodeStatus::Archived).await.unwrap();

        let stats = session.reflect(&[b], &mem).await.unwrap();
        assert_eq!((stats.reinforced, stats.interfered), (1, 0));
        let after = store.get_edge(a, b).await.unwrap().unwrap();
        assert_eq!(after.trials(), before.trials());
        assert_eq!(after.weight(), before.weight());
        assert_eq!(
            mem.get_node(b).await.unwrap().unwrap().grounded_use_count(),
            1,
            "used target keeps credit when the previous endpoint was archived"
        );
    }

    #[tokio::test]
    async fn stale_route_endpoint_or_direction_never_trains_the_edge() {
        // A dangling edge must not be trained after its previous endpoint is gone.
        let (mem, store, a, b) = fixture().await;
        let mut session = WalkSession::new(a, 2);
        session.go(&b.0.to_string(), &mem).await.unwrap();
        let before = store.get_edge(a, b).await.unwrap().unwrap();
        store.delete_node(a).await.unwrap();
        session.reflect(&[b], &mem).await.unwrap();
        let after = store.get_edge(a, b).await.unwrap().unwrap();
        assert_eq!(after.trials(), before.trials());
        assert_eq!(after.weight(), before.weight());

        // The same identity can no longer be trained if its kind changes to one
        // that traversal would not expose in the observed direction.
        let (mem, store, a, b) = fixture().await;
        let mut session = WalkSession::new(a, 2);
        session.go(&b.0.to_string(), &mem).await.unwrap();
        let incompatible = mneme_core::Edge::new(a, b, 0.25, EdgeKind::Supersedes, 1);
        store.put_edge(&incompatible).await.unwrap();
        session.reflect(&[b], &mem).await.unwrap();
        let after = store.get_edge(a, b).await.unwrap().unwrap();
        assert_eq!(after.trials(), 0);
        assert_eq!(after.weight(), incompatible.weight());
    }

    #[tokio::test]
    async fn start_only_grounded_use_is_idempotent_and_payload_bound() {
        let (mem, _store, _a, _b) = fixture().await;
        let node_id = mem
            .ingest(Ingest::new(
                "start node",
                b"",
                &[],
                Provenance::derived_empty(),
            ))
            .await
            .unwrap();
        let visited = HashSet::from([node_id]);
        let used = HashSet::from([node_id]);
        let retry = FeedbackRetryScope::new("walk-test-epoch", 1, 1).unwrap();

        let first =
            reflect_observed_idempotent(&mem, "start-only-receipt", &retry, &[], &visited, &used)
                .await
                .unwrap();
        assert_eq!(first.commit, FeedbackCommitOutcome::Applied);
        assert_eq!((first.reinforced, first.interfered), (1, 0));

        let replay =
            reflect_observed_idempotent(&mem, "start-only-receipt", &retry, &[], &visited, &used)
                .await
                .unwrap();
        assert_eq!(replay.commit, FeedbackCommitOutcome::AlreadyApplied);
        let node = mem.get_node(node_id).await.unwrap().unwrap();
        assert_eq!(node.grounded_use_count(), 1);
        assert_eq!(node.status(), NodeStatus::Active);

        assert!(
            reflect_observed_idempotent(
                &mem,
                "start-only-receipt",
                &retry,
                &[],
                &visited,
                &HashSet::new(),
            )
            .await
            .is_err(),
            "same key with an altered used set must conflict"
        );
        assert!(
            reflect_observed(&mem, &[], &HashSet::new(), &used)
                .await
                .is_err(),
            "used ids omitted from the receipt's visited set are rejected"
        );
    }

    #[tokio::test]
    async fn budget_caps_distinct_nodes() {
        let (mem, _store, a, _b) = fixture().await;
        let mut s = WalkSession::new(a, 1); // only the start fits
        match s.go("0", &mem).await {
            Err(WalkError::Rejected(m)) => assert!(m.contains("budget"), "got: {m}"),
            other => panic!("expected a budget rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn query_salience_reorders_edges() {
        // start S → ON (query-relevant) and OFF, with OFF on the *stronger* edge so
        // weight-order would rank it first.
        let dim = mneme_embed::DEFAULT_DIM;
        let store = Arc::new(mneme_cozo::MemStore::new(dim));
        let graph: Arc<dyn GraphStore> = store.clone();
        let vectors: Arc<dyn VectorIndex> = store.clone();
        let traversal: Arc<dyn Traversal> = store.clone();
        let embedder = Arc::new(mneme_embed::HashingEmbedder::new(dim));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let cfg = Config {
            min_similarity_links: 0,
            ..Config::default()
        };
        let mem = Memory::new(graph, vectors, traversal, embedder, clock, cfg)
            .with_body_store(Arc::new(mneme_body::InlineStore::new()));
        let prov = Provenance::derived_empty;
        let s = mem
            .ingest(Ingest::new("start", b"", &[], prov()))
            .await
            .unwrap();
        let on = mem
            .ingest(Ingest::new("on", b"", &[], prov()))
            .await
            .unwrap();
        let off = mem
            .ingest(Ingest::new("off", b"", &[], prov()))
            .await
            .unwrap();
        mem.link(s, on, EdgeKind::Associative, 0.6, None)
            .await
            .unwrap();
        mem.link(s, off, EdgeKind::Associative, 0.7, None)
            .await
            .unwrap();

        // No query: weight-order puts the stronger OFF edge first, no salience shown.
        let plain = WalkSession::new(s, 5).edges(&mem).await.unwrap();
        assert_eq!(
            plain[0].id,
            off.0.to_string(),
            "weight-order: stronger first"
        );
        assert!(plain[0].salience.is_none());

        // A relevance map favouring ON pulls it ahead of the (stronger) OFF edge.
        let rel = HashMap::from([(on, 0.95f32), (off, 0.0f32)]);
        let salient = WalkSession::new(s, 5)
            .with_query_relevance(rel)
            .edges(&mem)
            .await
            .unwrap();
        assert_eq!(
            salient[0].id,
            on.0.to_string(),
            "salience pulls the on-query edge to the front despite its lower weight"
        );
        assert!(salient[0].salience.unwrap() > salient[1].salience.unwrap());
    }
}
