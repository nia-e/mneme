//! Shared bounded canonical inventory. This is not ranked semantic retrieval:
//! exact historical episode editions are included, with no head substitution.
//! Cursors bind the database, selection and upper key, not a transaction snapshot.
use crate::touchstone::PreparedTouchstoneList;
use mneme_core::{Node, NodeId, NodeStatus};
use mneme_engine::Memory;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ulid::Ulid;

pub type ListError = Box<dyn std::error::Error + Send + Sync>;
pub const MAX_LIST_LIMIT: usize = 64;
pub const DEFAULT_LIST_LIMIT: usize = 50;
pub const MAX_LIST_SCANNED_NODES: usize = 256;
pub const MAX_LIST_SUMMARY_BYTES: usize = 1024;
pub const MAX_LIST_PAGE_BYTES: usize = 128 * 1024;
const MAX_CURSOR_BYTES: usize = 1024;
const MAX_INPUT_BYTES: usize = 8192;
const PAGE_METADATA_RESERVE: usize = 2048;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Active,
    Archived,
    #[default]
    All,
}

impl Status {
    fn allows(self, node: &Node) -> bool {
        match self {
            Self::Active => node.is_active(),
            Self::Archived => node.is_archived(),
            Self::All => true,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedNodeList {
    #[serde(default = "nodes_kind")]
    kind: String,
    #[serde(default)]
    status: Status,
    tag: Option<String>,
    after: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn nodes_kind() -> String {
    "nodes".into()
}
fn default_limit() -> usize {
    DEFAULT_LIST_LIMIT
}

/// One admitted list request shared by local CLI and MCP owner forwarding.
pub enum PreparedList {
    Nodes(PreparedNodeList),
    Touchstones(PreparedTouchstoneList),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    db_id: Ulid,
    status: Status,
    tag: Option<String>,
    through: NodeId,
    after: NodeId,
}

impl Cursor {
    fn parse(value: &str) -> Result<Self, ListError> {
        let encoded = value
            .strip_prefix("nodes-v1:")
            .ok_or("invalid nodes list cursor version")?;
        let cursor: Self = serde_json::from_str(encoded)?;
        if cursor.after > cursor.through {
            return Err("nodes list cursor is beyond its upper key".into());
        }
        Ok(cursor)
    }
    fn encode(&self) -> Result<String, ListError> {
        Ok(format!("nodes-v1:{}", serde_json::to_string(self)?))
    }
}

impl PreparedList {
    pub fn parse(raw: &Value) -> Result<Self, ListError> {
        let object = raw.as_object().ok_or("list arguments must be an object")?;
        if serde_json::to_vec(raw)?.len() > MAX_INPUT_BYTES {
            return Err("list input exceeds 8192 UTF-8 bytes".into());
        }
        if object.get("kind").and_then(Value::as_str) == Some("touchstones") {
            return Ok(Self::Touchstones(PreparedTouchstoneList::parse(raw)?));
        }
        for name in ["kind", "status", "tag", "after", "limit"] {
            if object.get(name).is_some_and(Value::is_null) {
                return Err(format!("list {name} must not be null").into());
            }
        }
        let input: PreparedNodeList = serde_json::from_value(raw.clone())?;
        if input.kind != "nodes" {
            return Err("list kind must be nodes or touchstones".into());
        }
        if !(1..=MAX_LIST_LIMIT).contains(&input.limit) {
            return Err(format!("nodes list limit must be 1..={MAX_LIST_LIMIT}").into());
        }
        if let Some(tag) = &input.tag {
            mneme_core::validate_tag(tag)?;
        }
        if let Some(after) = &input.after {
            if after.is_empty()
                || after.len() > MAX_CURSOR_BYTES
                || after.chars().any(char::is_control)
            {
                return Err("nodes list after must be a nonempty bounded native cursor".into());
            }
            let cursor = Cursor::parse(after)?;
            if cursor.status != input.status || cursor.tag != input.tag {
                return Err("nodes list cursor selection mismatch; keep status/tag unchanged or start a new list".into());
            }
        }
        Ok(Self::Nodes(input))
    }

    pub fn into_json(self) -> Value {
        match self {
            Self::Touchstones(input) => input.into_json(),
            Self::Nodes(input) => {
                let mut value = json!({"kind":"nodes","status":input.status,"limit":input.limit});
                if let Some(tag) = input.tag {
                    value["tag"] = json!(tag);
                }
                if let Some(after) = input.after {
                    value["after"] = json!(after);
                }
                value
            }
        }
    }

    pub async fn run(self, mem: &Memory, db_id: Ulid) -> Result<Value, ListError> {
        match self {
            Self::Touchstones(input) => input.run(mem, db_id).await,
            Self::Nodes(input) => input.run(mem, db_id).await,
        }
    }
}

fn node_card(node: &Node) -> Value {
    let summary = node.summary();
    let mut end = summary.len().min(MAX_LIST_SUMMARY_BYTES);
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    json!({
        "id":node.id(), "status":match node.status() { NodeStatus::Active => "active", NodeStatus::Archived => "archived" },
        "summary":&summary[..end], "summary_truncated":end < summary.len(),
        "tags":node.tags().collect::<Vec<_>>(),
        "kind":if node.is_semantic() { "note" } else { "episode" },
        "created":node.created(),"stability":node.stability(),"confidence":node.confidence()
    })
}

impl PreparedNodeList {
    async fn run(self, mem: &Memory, db_id: Ulid) -> Result<Value, ListError> {
        let (through, mut after) = if let Some(encoded) = &self.after {
            let cursor = Cursor::parse(encoded)?;
            if cursor.db_id != db_id {
                return Err("nodes list cursor database identity mismatch; start a new list in this database".into());
            }
            (Some(cursor.through), Some(cursor.after))
        } else {
            (mem.inventory_node_upper_bound().await?, None)
        };
        let mut items = Vec::new();
        let mut scanned = 0;
        let mut hydrated = 0;
        let mut requested = 0;
        let mut storage_pages = 0;
        let mut item_bytes = 0;
        let mut has_more = false;
        let mut stopped = "exhausted";
        if let Some(through) = through {
            'pages: while requested < MAX_LIST_SCANNED_NODES {
                let request_limit = mneme_core::ports::MAX_MAINTENANCE_BATCH_ROWS
                    .min(MAX_LIST_SCANNED_NODES - requested);
                requested += request_limit;
                storage_pages += 1;
                let page = mem
                    .inventory_nodes_page(after, through, request_limit)
                    .await?;
                if page.items.len() > request_limit {
                    return Err("inventory backend exceeded requested page bound".into());
                }
                hydrated += page.items.len();
                for (index, node) in page.items.iter().enumerate() {
                    if after.is_some_and(|last| node.id() <= last) || node.id() > through {
                        return Err(
                            "inventory backend returned an out-of-order or out-of-range node"
                                .into(),
                        );
                    }
                    if self.status.allows(node)
                        && self.tag.as_deref().is_none_or(|tag| node.has_tag(tag))
                    {
                        let card = node_card(node);
                        let bytes = serde_json::to_vec(&card)?.len() + 1;
                        if item_bytes + bytes > MAX_LIST_PAGE_BYTES - PAGE_METADATA_RESERVE {
                            if items.is_empty() {
                                return Err("inventory card exceeds page byte budget".into());
                            }
                            has_more = true;
                            stopped = "output_bytes";
                            break 'pages;
                        }
                        item_bytes += bytes;
                        items.push(card);
                    }
                    scanned += 1;
                    after = Some(node.id());
                    has_more = index + 1 < page.items.len() || page.next.is_some();
                    if items.len() == self.limit {
                        stopped = if has_more { "item_limit" } else { "exhausted" };
                        break 'pages;
                    }
                }
                if page.next.is_none() {
                    has_more = false;
                    break;
                }
                if page.items.is_empty() || page.next != after {
                    return Err("inventory backend returned a non-progressing continuation".into());
                }
                if requested == MAX_LIST_SCANNED_NODES {
                    has_more = true;
                    stopped = "scan_budget";
                    break;
                }
            }
        }
        let next_cursor = if has_more {
            Some(
                Cursor {
                    db_id,
                    status: self.status,
                    tag: self.tag.clone(),
                    through: through.ok_or("missing inventory upper key")?,
                    after: after.ok_or("missing inventory progress key")?,
                }
                .encode()?,
            )
        } else {
            None
        };
        let result = json!({"kind":"nodes","db_id":db_id.to_string(),"items":items,
            "next_cursor":next_cursor,"has_more":has_more,"partial":has_more,
            "coverage":{"order":"canonical_id_ascending","snapshot":false,"includes_historical_episode_editions":true,
                "scanned_nodes":scanned,"hydrated_nodes":hydrated,"requested_nodes":requested,
                "storage_pages":storage_pages,"stopped":stopped,
                "summary_max_bytes":MAX_LIST_SUMMARY_BYTES,"max_scanned_nodes":MAX_LIST_SCANNED_NODES,
                "upper_key":through}});
        if serde_json::to_vec(&result)?.len() > MAX_LIST_PAGE_BYTES {
            return Err("inventory response exceeds page byte budget".into());
        }
        Ok(result)
    }
}

pub fn list_input_schema() -> Value {
    let touchstones = crate::touchstone::list_input_schema();
    let mut schema = json!({"type":"object","additionalProperties":false,"required":[],"oneOf":[
        {"type":"object","additionalProperties":false,
        "description":"Bounded canonical inventory, including exact historical episode editions. Default status is all. Ascending canonical ID order, not newest-first or ranked search. Follow next_cursor until null, even on empty filtered pages; each call scans at most 256 canonical nodes (plus at most four indexed lookahead rows), returns at most 128 KiB, and truncates summaries at 1024 UTF-8 bytes. Cursors bind database identity, status/tag and upper key, not a snapshot. No bodies, inference, reinforcement or topology writes.",
        "properties":{
            "kind":{"type":"string","const":"nodes","default":"nodes"},
            "status":{"type":"string","enum":["active","archived","all"],"default":"all"},
            "tag":{"type":"string","minLength":1,"maxLength":mneme_core::MAX_TAG_BYTES},
            "after":{"type":"string","minLength":1,"maxLength":MAX_CURSOR_BYTES},
            "limit":{"type":"integer","minimum":1,"maximum":MAX_LIST_LIMIT,"default":DEFAULT_LIST_LIMIT}
        }}, touchstones
    ]});
    // Keep the closed root property catalog for clients which discover fields
    // without traversing oneOf; branch closure still rejects node-only filters
    // on touchstone requests.
    schema["properties"] = schema["oneOf"][0]["properties"].clone();
    schema["properties"]["kind"] =
        json!({"type":"string","enum":["nodes","touchstones"],"default":"nodes"});
    schema
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
