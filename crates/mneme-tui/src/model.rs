use crate::scenes::{Scene, SceneAxis, ScenePage};
use crate::touchstones::{ExactTarget, TouchstoneDetail, TouchstonePage};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct Target {
    pub name: String,
    pub database: String,
    pub url: String,
    pub ssh_mcp_port: u16,
    pub token_env: Option<String>,
    pub expected_db_id: Option<String>,
    pub expected_path: Option<String>,
}

/// A known source is metadata, not an already opened connection.
#[derive(Clone, Debug)]
pub struct SourceChoice {
    pub name: String,
    pub(crate) route: Option<Target>,
    pub(crate) unavailable_reason: Option<String>,
    pub expected_db_id: Option<String>,
}
impl SourceChoice {
    pub fn routed(target: Target) -> Self {
        Self {
            name: target.name.clone(),
            expected_db_id: target.expected_db_id.clone(),
            route: Some(target),
            unavailable_reason: None,
        }
    }
    pub fn route(&self) -> Option<&Target> {
        self.route.as_ref()
    }
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable_reason.as_deref()
    }
    pub fn unavailable(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            route: None,
            unavailable_reason: Some(reason.into()),
            expected_db_id: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Node {
    pub id: String,
    pub summary: String,
    pub body: String,
    pub tags: Vec<String>,
    /// Complete, identity-preserving tag metadata; false for placeholders or excerpts.
    /// Display labels must not interpret unknown tags as authored absence.
    pub tags_complete: bool,
    pub status: String,
    pub confidence: Option<f64>,
    pub provenance: String,
}
#[derive(Clone, Debug)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub weight: f64,
}
#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub partial: bool,
    pub note: String,
    pub focus: Option<String>,
    pub inventory: Option<InventoryState>,
}
/// Coverage is not a store total or an atomic snapshot. Reads never imply absence.
#[derive(Clone, Debug, Default)]
pub struct InventoryState {
    pub db_id: String,
    pub next: Option<String>,
    pub bytes: usize,
    pub complete: bool,
    pub topology_edges_complete: bool,
    pub hydrated: std::collections::HashSet<String>,
    pub summary_attempted: std::collections::HashSet<String>,
    pub summaries_loaded: usize,
    pub background_paused: bool,
    pub crawling: bool,
}
#[derive(Clone, Debug)]
pub struct InventoryPage {
    pub graph: Graph,
    pub bytes: usize,
}
#[derive(Clone, Debug, Default)]
pub struct InventorySummaries {
    pub nodes: Vec<Node>,
    pub missing: Vec<String>,
    pub bytes: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Activity {
    pub state: String,
    pub in_flight: u64,
    pub backend_jobs: u64,
    pub returns: u64,
    pub node_ids: Vec<String>,
    pub feed: bool,
    pub missed: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Map,
    Scenes,
    Touchstones,
}

pub enum Request {
    Inventory {
        after: Option<String>,
    },
    Summaries {
        ids: Vec<String>,
    },
    Query(String),
    Focus(String),
    Lens(String),
    Scenes {
        axis: SceneAxis,
        cue: Option<String>,
    },
    Scene {
        episode_id: String,
        edition_id: String,
    },
    Touchstones {
        after: Option<String>,
    },
    Touchstone {
        id: String,
    },
    TouchstoneTarget {
        db_id: String,
        id: String,
    },
    Poll,
    Cancel,
    Shutdown,
}
pub enum Response {
    Inventory(InventoryPage),
    Summaries(InventorySummaries),
    Loaded(Graph),
    Scenes(ScenePage),
    Scene(Scene),
    Touchstones(TouchstonePage),
    Touchstone(TouchstoneDetail),
    TouchstoneTarget(ExactTarget),
    Activity(Activity),
    Canceled,
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LensState {
    Loading,
    Ready,
    Unavailable,
}

/// One temporary, bounded neighborhood, never a replacement for the base field.
/// The anchor and the base field's presentation stay fixed until this lens closes.
pub struct EdgeLens {
    pub anchor: String,
    pub graph: Graph,
    pub state: LensState,
    saved_scroll: usize,
    saved_details: bool,
}

impl EdgeLens {
    pub fn replace_graph(&mut self, mut graph: Graph) {
        // The reader already caps these, but keep presentation storage bounded too.
        graph.partial |= graph.nodes.len() > 33 || graph.edges.len() > 32;
        graph.nodes.truncate(33);
        graph
            .edges
            .retain(|edge| edge.from == self.anchor || edge.to == self.anchor);
        graph.edges.truncate(32);
        self.graph = graph;
        self.state = LensState::Ready;
    }

    /// A neighbors read is traversal-oriented, not an exhaustive incident-edge
    /// read. Keep field edges that the selected endpoint cannot traverse; a
    /// missing row is not evidence that the stored relationship disappeared.
    pub fn merge_graph(&mut self, mut fresh: Graph) {
        let known = &self.graph;
        let same_edge = |a: &Edge, b: &Edge| a.from == b.from && a.to == b.to && a.kind == b.kind;
        let card = |id: &str| {
            fresh
                .nodes
                .iter()
                .find(|node| node.id == id)
                .or_else(|| known.nodes.iter().find(|node| node.id == id))
        };
        let mut partial = known.partial || fresh.partial;
        let mut nodes = Vec::with_capacity(33);
        if let Some(anchor) = card(&self.anchor) {
            nodes.push(anchor.clone());
        } else {
            partial = true;
        }
        let mut edges: Vec<Edge> = Vec::with_capacity(32);
        // Known edges come first so an otherwise full fresh neighborhood cannot
        // erase a relationship already visible in the base scene. Fresh values
        // win for matching keys without changing that priority or adding copies.
        for candidate in known
            .edges
            .iter()
            .chain(&fresh.edges)
            .filter(|edge| edge.from == self.anchor || edge.to == self.anchor)
        {
            let edge = fresh
                .edges
                .iter()
                .find(|edge| same_edge(candidate, edge))
                .unwrap_or(candidate);
            if edges.iter().any(|kept| same_edge(kept, edge)) {
                continue;
            }
            if edges.len() == 32 {
                partial = true;
                continue;
            }
            let (Some(from), Some(to)) = (card(&edge.from), card(&edge.to)) else {
                // Do not publish a dangling endpoint if a bounded card set omitted
                // it. This is normally prevented by the reader's own admission.
                partial = true;
                continue;
            };
            for node in [from, to] {
                if !nodes.iter().any(|kept| kept.id == node.id) {
                    nodes.push(node.clone());
                }
            }
            edges.push(edge.clone());
        }
        // Every admitted edge is incident to the anchor: 32 edges require at
        // most the anchor plus 32 other cards, even across relation kinds.
        debug_assert!(nodes.len() <= 33);
        fresh.nodes = nodes;
        fresh.edges = edges;
        fresh.partial = partial;
        fresh.focus = Some(self.anchor.clone());
        self.graph = fresh;
        self.state = LensState::Ready;
    }

    pub fn includes(&self, id: &str) -> bool {
        id == self.anchor
            || self
                .graph
                .edges
                .iter()
                .any(|edge| edge.from == id || edge.to == id)
    }
}

#[derive(Clone)]
pub struct Visit {
    graph: Graph,
    layout: crate::layout::LayoutCache,
    selected: usize,
    query: String,
    scroll: usize,
    show_details: bool,
    last_refresh: Option<Instant>,
}

/// One bounded shelf page, one annotation, and one explicitly read exact target.
#[derive(Default)]
pub struct TouchstoneCabinet {
    pub page: TouchstonePage,
    pub selected: usize,
    pub detail: Option<TouchstoneDetail>,
    pub reference: usize,
    pub current: Option<ExactTarget>,
    pub loaded: bool,
    pub(crate) generation: u64,
}
impl TouchstoneCabinet {
    pub(crate) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

pub struct App {
    pub cabinet: TouchstoneCabinet,
    pub(crate) deferred_view_read: bool,
    pub view: View,
    pub scenes: ScenePage,
    pub scene_selected: usize,
    pub scene_query: String,
    pub scene_axis: SceneAxis,
    pub scene_detail: Option<Scene>,
    pub scenes_loaded: bool,
    pub scene_refreshed: Option<Instant>,
    pub sources: Vec<SourceChoice>,
    pub source_menu: Option<usize>,
    pub(crate) source_menu_hits: RefCell<Vec<(ratatui::layout::Rect, usize)>>,
    pub targets: Vec<Target>,
    pub target: usize,
    pub graph: Graph,
    pub(crate) layout: RefCell<crate::layout::LayoutCache>,
    pub selected: usize,
    pub hide_archived: bool,
    pub query: String,
    pub input: String,
    pub editing: bool,
    pub busy: bool,
    pub error: Option<String>,
    pub events: VecDeque<String>,
    pub activity: Option<Activity>,
    pub activity_samples: VecDeque<Activity>,
    pub pulses: std::collections::HashMap<String, Instant>,
    pub last_refresh: Option<Instant>,
    pub tick: u64,
    pub show_help: bool,
    pub show_details: bool,
    pub edge_lens: Option<EdgeLens>,
    pub history: Vec<Visit>,
    pub demo: bool,
    pub scroll: usize,
    pub(crate) detail_viewport: RefCell<Option<crate::detail_scroll::Viewport>>,
}
impl App {
    pub fn new(targets: Vec<Target>, query: String, demo: bool) -> Self {
        Self {
            cabinet: TouchstoneCabinet::default(),
            deferred_view_read: false,
            view: View::Map,
            scenes: ScenePage::default(),
            scene_selected: 0,
            scene_query: String::new(),
            scene_axis: SceneAxis::Recorded,
            scene_detail: None,
            scenes_loaded: false,
            scene_refreshed: None,
            sources: targets.iter().cloned().map(SourceChoice::routed).collect(),
            source_menu: None,
            source_menu_hits: RefCell::default(),
            targets,
            target: 0,
            graph: Graph::default(),
            layout: RefCell::default(),
            selected: 0,
            hide_archived: false,
            query,
            input: String::new(),
            editing: false,
            busy: false,
            error: None,
            events: VecDeque::new(),
            activity: None,
            activity_samples: VecDeque::new(),
            pulses: std::collections::HashMap::new(),
            last_refresh: None,
            tick: 0,
            show_help: false,
            show_details: false,
            edge_lens: None,
            history: Vec::new(),
            demo,
            scroll: 0,
            detail_viewport: RefCell::default(),
        }
    }
    pub fn selected_scene(&self) -> Option<&Scene> {
        self.scenes.items.get(self.scene_selected)
    }
    pub(crate) fn node_selectable(&self, node: &Node) -> bool {
        !self.hide_archived || node.status != "archived"
    }
    pub(crate) fn hidden_node_ids(&self) -> std::collections::HashSet<&str> {
        if !self.hide_archived {
            return Default::default();
        }
        self.scene_nodes()
            .into_iter()
            .filter(|node| !self.node_selectable(node))
            .map(|node| node.id.as_str())
            .collect()
    }
    pub fn selected_node(&self) -> Option<&Node> {
        let node = if self.selected < self.graph.nodes.len() {
            self.graph.nodes.get(self.selected)
        } else {
            self.scene_nodes().get(self.selected).copied()
        };
        node.filter(|node| self.node_selectable(node))
    }
    pub(crate) fn select_node(&mut self, index: usize) {
        if self
            .scene_nodes()
            .get(index)
            .is_some_and(|node| self.node_selectable(node))
        {
            self.selected = index;
            self.scroll = 0;
            self.layout.borrow_mut().invalidate_hits();
        }
    }
    pub(crate) fn cycle_nodes(&mut self, forward: bool) {
        let nodes = self.scene_nodes();
        let count = nodes.len();
        if count == 0 {
            return;
        }
        let next = (1..=count)
            .map(|step| {
                if forward {
                    (self.selected % count + step) % count
                } else {
                    (self.selected % count + count - step) % count
                }
            })
            .find(|&index| self.node_selectable(nodes[index]));
        if let Some(index) = next {
            self.select_node(index);
        }
    }
    /// Keep raw indices and the world intact. All-hidden has no selection, not
    /// a fabricated empty inventory or an out-of-range sentinel.
    pub(crate) fn normalize_map_selection(&mut self) {
        if self.hide_archived
            && self.edge_lens.as_ref().is_some_and(|lens| {
                self.scene_nodes()
                    .iter()
                    .any(|node| node.id == lens.anchor && node.status == "archived")
            })
        {
            self.close_edge_lens();
        }
        if self.selected_node().is_none() {
            self.cycle_nodes(true);
        }
    }
    pub(crate) fn toggle_archived(&mut self) {
        self.hide_archived = !self.hide_archived;
        self.normalize_map_selection();
        let mut layout = self.layout.borrow_mut();
        layout.invalidate_hits();
        layout.cancel_drag();
    }
    pub fn open_edge_lens(&mut self, anchor: String, graph: Graph, state: LensState) {
        self.close_edge_lens();
        let mut lens = EdgeLens {
            anchor,
            graph: Graph::default(),
            state,
            saved_scroll: self.scroll,
            saved_details: self.show_details,
        };
        lens.replace_graph(graph);
        lens.state = state;
        self.edge_lens = Some(lens);
        self.scroll = 0;
        self.show_details = false;
    }
    pub fn close_edge_lens(&mut self) {
        if let Some(lens) = self.edge_lens.take() {
            if self.selected >= self.graph.nodes.len() {
                self.selected = self
                    .graph
                    .nodes
                    .iter()
                    .position(|node| node.id == lens.anchor)
                    .unwrap_or(0);
            }
            self.layout.borrow_mut().invalidate_hits();
            self.scroll = lens.saved_scroll;
            self.show_details = lens.saved_details;
        }
    }
    pub fn scene_nodes(&self) -> Vec<&Node> {
        let mut nodes: Vec<_> = self.graph.nodes.iter().collect();
        if let Some(lens) = &self.edge_lens {
            let mut extra: Vec<_> = lens
                .graph
                .nodes
                .iter()
                .filter(|node| !self.graph.nodes.iter().any(|base| base.id == node.id))
                .collect();
            extra.sort_by(|a, b| a.id.cmp(&b.id));
            extra.dedup_by(|a, b| a.id == b.id);
            nodes.extend(extra.into_iter().take(33));
        }
        nodes
    }
    pub fn scene_edges(&self) -> &[Edge] {
        self.edge_lens
            .as_ref()
            .map_or(&self.graph.edges, |lens| &lens.graph.edges)
    }
    pub fn node_visible(&self, id: &str) -> bool {
        self.scene_nodes()
            .iter()
            .find(|node| node.id == id)
            .is_some_and(|node| self.node_visible_node(node))
    }
    pub(crate) fn node_visible_node(&self, node: &Node) -> bool {
        self.node_selectable(node)
            && (self
                .edge_lens
                .as_ref()
                .is_none_or(|lens| lens.includes(&node.id))
                || self
                    .graph
                    .nodes
                    .get(self.selected)
                    .is_some_and(|selected| selected.id == node.id))
    }
    pub fn event(&mut self, text: impl Into<String>) {
        self.events.push_front(text.into());
        self.events.truncate(3);
    }
    pub fn visit(&self) -> Visit {
        Visit {
            graph: self.graph.clone(),
            layout: self.layout.borrow().clone(),
            selected: if self.selected < self.graph.nodes.len() {
                self.selected
            } else {
                self.edge_lens
                    .as_ref()
                    .and_then(|lens| {
                        self.graph
                            .nodes
                            .iter()
                            .position(|node| node.id == lens.anchor)
                    })
                    .unwrap_or(0)
            },
            query: self.query.clone(),
            scroll: self
                .edge_lens
                .as_ref()
                .map_or(self.scroll, |lens| lens.saved_scroll),
            show_details: self
                .edge_lens
                .as_ref()
                .map_or(self.show_details, |lens| lens.saved_details),
            last_refresh: self.last_refresh,
        }
    }
    pub fn remember(&mut self, visit: Visit) {
        self.history.push(visit);
        if self.history.len() > 16 {
            self.history.remove(0);
        }
    }
    pub fn can_go_back(&self) -> bool {
        !self.history.is_empty()
    }
    pub fn go_back(&mut self) {
        if let Some(visit) = self.history.pop() {
            self.close_edge_lens();
            self.graph = visit.graph;
            *self.layout.borrow_mut() = visit.layout;
            self.selected = visit.selected;
            self.query = visit.query;
            self.scroll = visit.scroll;
            self.show_details = visit.show_details;
            self.last_refresh = visit.last_refresh;
            self.error = None;
        }
    }
}

pub fn neighborhood(graph: &Graph, id: &str) -> Graph {
    let mut graph = graph.clone();
    graph.edges.retain(|edge| edge.from == id || edge.to == id);
    graph.nodes.retain(|node| {
        node.id == id
            || graph
                .edges
                .iter()
                .any(|edge| edge.from == node.id || edge.to == node.id)
    });
    graph
}

/// Demo navigation follows the same bounded, one-hop presentation as live reads.
pub fn demo_neighborhood(id: &str) -> Graph {
    let mut graph = demo_graph();
    graph.edges.retain(|edge| edge.from == id || edge.to == id);
    graph.nodes.retain(|node| {
        node.id == id
            || graph
                .edges
                .iter()
                .any(|e| e.from == node.id || e.to == node.id)
    });
    graph.nodes.sort_by_key(|node| node.id != id);
    graph.focus = Some(id.to_owned());
    graph
}

/// Terminal content is text, never control sequences. Preserve useful body newlines.
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

pub fn demo_graph() -> Graph {
    let summaries = [
        "A memory is a promise to return",
        "One writer, many windows",
        "Keep the strange idea",
        "Revision is not betrayal",
        "A name can be a place",
        "The small useful lesson",
        "Leave a thread for tomorrow",
        "A map is not the territory",
        "Evidence has a date",
        "An unfinished question",
        "Make room for play",
        "Notice what changed",
    ];
    let nodes = summaries.iter().enumerate().map(|(i,s)| Node { id:format!("demo-{i:02}"),summary:(*s).into(),body:"A synthetic memory in the observatory's demonstration constellation.\n\nThe display is a lens, not the whole sky. Open a live source to inspect your actual memories.\n\nPositions are for legibility; only the drawn edges assert a relationship.".into(),tags:vec!["demo".into(),match i { 0 => "core", 1 => "ownership", 2..=4 => "identity", 5..=7 => "practice", _ => "inquiry" }.into()],tags_complete:true,status:"active".into(),confidence:Some(0.8),provenance:"Synthetic example · never saved".into() }).collect::<Vec<_>>();
    let mut edges: Vec<_> = (1..nodes.len())
        .map(|i| Edge {
            from: nodes[0].id.clone(),
            to: nodes[i].id.clone(),
            kind: ["associative", "transition", "derived_from", "supersedes"][i % 4].into(),
            weight: match i {
                1 => 0.6,
                2..=4 => 0.9,
                5..=7 => 0.45,
                _ => 0.2,
            },
        })
        .collect();
    for (from, to) in [(2, 3), (3, 4), (5, 6), (6, 7), (8, 9), (9, 10), (10, 11)] {
        edges.push(Edge {
            from: nodes[from].id.clone(),
            to: nodes[to].id.clone(),
            kind: "associative".into(),
            weight: 0.8,
        });
    }
    Graph {
        nodes,
        edges,
        focus: Some("demo-00".into()),
        note: "Synthetic demonstration · no memory service contacted".into(),
        partial: false,
        inventory: None,
    }
}
