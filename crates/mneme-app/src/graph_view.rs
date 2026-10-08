//! Lightweight whole-graph topology and separately batched viewport cards.
//! A scan binds database identity and upper keys, not a transaction snapshot.
use mneme_core::ports::{MAX_LINKED_READ_TIMEOUT, MAX_MAINTENANCE_BATCH_ROWS, MaintenanceEdgeKey};
use mneme_core::{EdgeKind, Node, NodeId, NodeStatus};
use mneme_engine::Memory;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ulid::Ulid;

pub type GraphError = Box<dyn std::error::Error + Send + Sync>;
pub const MAX_TOPOLOGY_LIMIT: usize = 256;
pub const MAX_SUMMARY_IDS: usize = 64;
pub const MAX_GRAPH_PAGE_BYTES: usize = 128 * 1024;
pub const MAX_GRAPH_SUMMARY_BYTES: usize = 1024;
const MAX_CURSOR_BYTES: usize = 2048;
const MAX_INPUT_BYTES: usize = 8192;
const MAX_SUMMARY_JSON_BYTES: usize = 1024;
const MAX_TAGS_JSON_BYTES: usize = 512;
// Reserve half each topology record's page allowance for identity fields and
// the bounded cursor/coverage envelope. Summary batches contain only 64 cards.
const MAX_TOPOLOGY_TAGS_JSON_BYTES: usize = MAX_GRAPH_PAGE_BYTES / MAX_TOPOLOGY_LIMIT / 2;

pub struct PreparedGraph(GraphRequest);

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum GraphRequest {
    Topology {
        after: Option<String>,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    Summaries {
        ids: Vec<NodeId>,
    },
}
fn default_limit() -> usize {
    MAX_TOPOLOGY_LIMIT
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    db_id: Ulid,
    node_through: Option<NodeId>,
    edge_through: Option<[NodeId; 2]>,
    node_after: Option<NodeId>,
    edge_after: Option<[NodeId; 2]>,
    nodes_done: bool,
    edges_done: bool,
    nodes_seen: usize,
    edges_seen: usize,
}
impl Cursor {
    fn parse(raw: &str) -> Result<Self, GraphError> {
        let encoded = raw
            .strip_prefix("graph-v1:")
            .ok_or("invalid graph cursor version")?;
        let c: Self = serde_json::from_str(encoded)?;
        if c.node_after
            .is_some_and(|a| c.node_through.is_none_or(|t| a > t))
            || c.edge_after
                .is_some_and(|a| c.edge_through.is_none_or(|t| a > t))
            || (c.node_through.is_none() && !c.nodes_done)
            || (c.edge_through.is_none() && !c.edges_done)
            || (c.nodes_done && c.edges_done)
            || (!c.nodes_done && c.edge_after.is_some())
        {
            return Err("invalid graph cursor progress or upper keys".into());
        }
        Ok(c)
    }
    fn encode(&self) -> Result<String, GraphError> {
        Ok(format!("graph-v1:{}", serde_json::to_string(self)?))
    }
}

impl PreparedGraph {
    pub fn parse(raw: &Value) -> Result<Self, GraphError> {
        let object = raw.as_object().ok_or("graph arguments must be an object")?;
        if serde_json::to_vec(raw)?.len() > MAX_INPUT_BYTES {
            return Err("graph input exceeds 8192 UTF-8 bytes".into());
        }
        for field in ["action", "after", "limit", "ids"] {
            if object.get(field).is_some_and(Value::is_null) {
                return Err(format!("graph {field} must not be null").into());
            }
        }
        let request: GraphRequest = serde_json::from_value(raw.clone())?;
        match &request {
            GraphRequest::Topology { after, limit } => {
                if !(1..=MAX_TOPOLOGY_LIMIT).contains(limit) {
                    return Err("graph topology limit must be 1..=256".into());
                }
                if let Some(after) = after {
                    if after.is_empty()
                        || after.len() > MAX_CURSOR_BYTES
                        || after.chars().any(char::is_control)
                    {
                        return Err("graph after must be a bounded native cursor".into());
                    }
                    Cursor::parse(after)?;
                }
            }
            GraphRequest::Summaries { ids } => {
                if ids.is_empty() || ids.len() > MAX_SUMMARY_IDS {
                    return Err("graph summaries ids must contain 1..=64 identities".into());
                }
                let unique = ids.iter().collect::<std::collections::HashSet<_>>();
                if unique.len() != ids.len() {
                    return Err("graph summaries ids must be unique".into());
                }
            }
        }
        Ok(Self(request))
    }
    pub fn into_json(self) -> Value {
        match self.0 {
            GraphRequest::Topology { after, limit } => {
                let mut value = json!({"action":"topology","limit":limit});
                if let Some(after) = after {
                    value["after"] = json!(after);
                }
                value
            }
            GraphRequest::Summaries { ids } => json!({"action":"summaries","ids":ids}),
        }
    }
    pub async fn run(self, mem: &Memory, db_id: Ulid) -> Result<Value, GraphError> {
        let result = tokio::time::timeout(MAX_LINKED_READ_TIMEOUT, self.run_inner(mem, db_id))
            .await
            .map_err(|_| "graph read exceeded its five-second deadline; retry this page")??;
        if serde_json::to_vec(&result)?.len() > MAX_GRAPH_PAGE_BYTES {
            return Err("graph response exceeds 128 KiB byte budget".into());
        }
        Ok(result)
    }
    async fn run_inner(self, mem: &Memory, db_id: Ulid) -> Result<Value, GraphError> {
        match self.0 {
            GraphRequest::Summaries { ids } => {
                let nodes = mem.graph_summary_nodes(&ids).await?;
                let items = nodes
                    .iter()
                    .zip(&ids)
                    .map(|(node, id)| match node {
                        Some(node) => summary_card(node),
                        None => json!({"id":id,"missing":true}),
                    })
                    .collect::<Vec<_>>();
                Ok(json!({"action":"summaries","db_id":db_id,"items":items,
                    "coverage":{"snapshot":false,"exact_requested_identities":true,"body_reads":0,"summary_max_bytes":MAX_GRAPH_SUMMARY_BYTES,"tags_json_max_bytes":MAX_TAGS_JSON_BYTES}}))
            }
            GraphRequest::Topology { after, limit } => {
                let mut cursor = if let Some(after) = after {
                    let c = Cursor::parse(&after)?;
                    if c.db_id != db_id {
                        return Err(
                            "graph cursor database identity mismatch; start a new topology scan"
                                .into(),
                        );
                    }
                    c
                } else {
                    let node_through = mem.inventory_node_upper_bound().await?;
                    let edge_through = mem
                        .graph_edge_upper_bound()
                        .await?
                        .map(|key| [key.from, key.to]);
                    Cursor {
                        db_id,
                        node_through,
                        edge_through,
                        node_after: None,
                        edge_after: None,
                        nodes_done: node_through.is_none(),
                        edges_done: edge_through.is_none(),
                        nodes_seen: 0,
                        edges_seen: 0,
                    }
                };
                let mut nodes = Vec::new();
                let mut edges = Vec::new();
                let mut scans = 0;
                while nodes.len() + edges.len() < limit && !(cursor.nodes_done && cursor.edges_done)
                {
                    let batch = (limit - nodes.len() - edges.len()).min(MAX_MAINTENANCE_BATCH_ROWS);
                    scans += 1;
                    if scans > limit.div_ceil(MAX_MAINTENANCE_BATCH_ROWS) + 1 {
                        return Err("graph backend exceeded indexed-page work budget".into());
                    }
                    if !cursor.nodes_done {
                        let through = cursor
                            .node_through
                            .ok_or("graph cursor missing node upper key")?;
                        let before = cursor.node_after;
                        let page = mem.inventory_nodes_page(before, through, batch).await?;
                        if page.items.len() > batch {
                            return Err("graph backend exceeded node page bound".into());
                        }
                        for node in page.items {
                            if cursor.node_after.is_some_and(|last| node.id() <= last)
                                || node.id() > through
                            {
                                return Err("graph backend returned unordered node keys".into());
                            }
                            cursor.node_after = Some(node.id());
                            nodes.push(topology_card(&node));
                            cursor.nodes_seen = cursor
                                .nodes_seen
                                .checked_add(1)
                                .ok_or("graph node count overflow")?;
                        }
                        if page.next.is_some()
                            && (cursor.node_after == before || page.next != cursor.node_after)
                        {
                            return Err("graph backend returned non-progressing node cursor".into());
                        }
                        cursor.nodes_done = page.next.is_none();
                    } else {
                        let through = cursor
                            .edge_through
                            .map(|[from, to]| MaintenanceEdgeKey::new(from, to))
                            .ok_or("graph cursor missing edge upper key")?;
                        let before = cursor.edge_after;
                        let page = mem
                            .graph_edges_page(
                                before.map(|[from, to]| MaintenanceEdgeKey::new(from, to)),
                                through,
                                batch,
                            )
                            .await?;
                        if page.items.len() > batch {
                            return Err("graph backend exceeded edge page bound".into());
                        }
                        for edge in page.items {
                            let key = [edge.from, edge.to];
                            if cursor.edge_after.is_some_and(|last| key <= last)
                                || key > [through.from, through.to]
                            {
                                return Err("graph backend returned unordered edge keys".into());
                            }
                            cursor.edge_after = Some(key);
                            let kind = match edge.kind {
                                EdgeKind::Associative => "associative",
                                EdgeKind::Bridge => "bridge",
                                EdgeKind::Transition => "transition",
                                EdgeKind::Supersedes => "supersedes",
                                EdgeKind::DerivedFrom => "derived_from",
                            };
                            edges.push(json!({"from":edge.from,"to":edge.to,"kind":kind,"weight":edge.weight()}));
                            cursor.edges_seen = cursor
                                .edges_seen
                                .checked_add(1)
                                .ok_or("graph edge count overflow")?;
                        }
                        if page.next.is_some()
                            && (cursor.edge_after == before
                                || page.next.map(|key| [key.from, key.to]) != cursor.edge_after)
                        {
                            return Err("graph backend returned non-progressing edge cursor".into());
                        }
                        cursor.edges_done = page.next.is_none();
                    }
                }
                let has_more = !(cursor.nodes_done && cursor.edges_done);
                let next = has_more.then(|| cursor.encode()).transpose()?;
                Ok(
                    json!({"action":"topology","db_id":db_id,"nodes":nodes,"edges":edges,"has_more":has_more,"next_cursor":next,
                    "coverage":{"snapshot":false,"complete":!has_more,"nodes_done":cursor.nodes_done,"edges_done":cursor.edges_done,
                        "nodes_seen":cursor.nodes_seen,"edges_seen":cursor.edges_seen,"page_records":nodes.len()+edges.len(),
                        "indexed_pages":scans,"max_page_records":limit,"indexed_lookahead_rows_max":scans,"body_reads":0,
                        "node_upper_key":cursor.node_through,"edge_upper_key":cursor.edge_through,
                        "tags_json_max_bytes":MAX_TOPOLOGY_TAGS_JSON_BYTES,
                        "includes_isolated_and_historical_editions":true,"counts_are_observed_not_snapshot_totals":true}}),
                )
            }
        }
    }
}

fn identity_card(node: &Node) -> Value {
    json!({"id":node.id(),"status":match node.status() {NodeStatus::Active=>"active",NodeStatus::Archived=>"archived"},"kind":if node.is_semantic(){"note"}else{"episode"}})
}
fn topology_card(node: &Node) -> Value {
    let mut card = identity_card(node);
    let (tags, truncated) = bounded_tag_prefix(node, MAX_TOPOLOGY_TAGS_JSON_BYTES);
    card["tags"] = json!(tags);
    card["tags_truncated"] = json!(truncated);
    card
}
/// Preserve exact authored tag identities in canonical order. An omitted tail
/// is unknown metadata, not evidence that those tags are absent.
fn bounded_tag_prefix(node: &Node, max_json_bytes: usize) -> (Vec<&str>, bool) {
    let mut tags = Vec::new();
    let mut tags_bytes = 2;
    let mut truncated = false;
    for tag in node.tags() {
        let bytes =
            serde_json::to_vec(tag).expect("tags serialize").len() + usize::from(!tags.is_empty());
        if tags_bytes + bytes > max_json_bytes {
            truncated = true;
            break;
        }
        tags_bytes += bytes;
        tags.push(tag);
    }
    (tags, truncated)
}
fn summary_card(node: &Node) -> Value {
    let mut card = identity_card(node);
    let mut end = 0;
    let mut json_bytes = 2;
    for (index, ch) in node.summary().char_indices() {
        let width = serde_json::to_vec(&ch.to_string())
            .expect("characters serialize")
            .len()
            - 2;
        if index + ch.len_utf8() > MAX_GRAPH_SUMMARY_BYTES
            || json_bytes + width > MAX_SUMMARY_JSON_BYTES
        {
            break;
        }
        end = index + ch.len_utf8();
        json_bytes += width;
    }
    let (tags, truncated) = bounded_tag_prefix(node, MAX_TAGS_JSON_BYTES);
    card["missing"] = json!(false);
    card["summary"] = json!(&node.summary()[..end]);
    card["summary_truncated"] = json!(end < node.summary().len());
    card["tags"] = json!(tags);
    card["tags_truncated"] = json!(truncated);
    card
}

pub fn graph_input_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["action"],
        "properties":{"action":{"type":"string","enum":["topology","summaries"]},"after":{"type":"string","minLength":1,"maxLength":MAX_CURSOR_BYTES},"limit":{"type":"integer","minimum":1,"maximum":MAX_TOPOLOGY_LIMIT},"ids":{"type":"array","minItems":1,"maxItems":MAX_SUMMARY_IDS,"uniqueItems":true,"items":{"type":"string","minLength":26,"maxLength":26}}},"oneOf":[
        {"type":"object","additionalProperties":false,"required":["action"],
         "description":"Read lightweight canonical topology in global indexed keyset order: all nodes first (including isolated and exact historical editions), then actual edges. Exact authored tag prefixes (at most 256 JSON bytes) include tags_truncated; no summaries, bodies, inference or writes. Follow next_cursor until null; at most 256 records and 128 KiB per five-second call. Cursor binds database and fixed upper keys, not a snapshot: concurrent changes can be omitted or observed. Terminal observed counts are scan counts, not a transactional total.",
         "properties":{"action":{"const":"topology","type":"string"},"limit":{"type":"integer","minimum":1,"maximum":MAX_TOPOLOGY_LIMIT,"default":MAX_TOPOLOGY_LIMIT},"after":{"type":"string","minLength":1,"maxLength":MAX_CURSOR_BYTES}}},
        {"type":"object","additionalProperties":false,"required":["action","ids"],
         "description":"Read exact requested identity slots in one bounded batch, missing explicit. Summary and tag prefixes are JSON-byte bounded with truncation flags; no bodies, episode head substitution, inference or learning writes.",
         "properties":{"action":{"const":"summaries","type":"string"},"ids":{"type":"array","minItems":1,"maxItems":MAX_SUMMARY_IDS,"uniqueItems":true,"items":{"type":"string","minLength":26,"maxLength":26}}}}
    ]})
}

#[cfg(test)]
#[path = "graph_view_tests.rs"]
mod tests;
