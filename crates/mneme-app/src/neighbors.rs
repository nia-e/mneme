//! Shared read-only indexed neighbor inspection. Cursors bind the logical
//! database and anchor; they are continuations, not transactional snapshots.
use mneme_core::ports::{
    IncidentEdgeLeg, IncidentEdgesCursor, IncidentEdgesRequest, MAX_LINKED_READ_TIMEOUT,
    MaintenanceEdgeKey,
};
use mneme_core::{EdgeKind, NodeId};
use mneme_engine::Memory;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ulid::Ulid;

pub type NeighborError = Box<dyn std::error::Error + Send + Sync>;
pub const DEFAULT_NEIGHBOR_LIMIT: usize = 32;
pub const MAX_NEIGHBOR_LIMIT: usize = 64;
pub const MAX_NEIGHBOR_SUMMARY_BYTES: usize = 1024;
pub const MAX_NEIGHBOR_PAGE_BYTES: usize = 128 * 1024;
const MAX_CURSOR_BYTES: usize = 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedNeighbors {
    id: NodeId,
    #[serde(default = "default_limit")]
    limit: usize,
    after: Option<String>,
}
fn default_limit() -> usize {
    DEFAULT_NEIGHBOR_LIMIT
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    db_id: Ulid,
    anchor: NodeId,
    outgoing_after: Option<[NodeId; 2]>,
    incoming_after: Option<[NodeId; 2]>,
    outgoing_done: bool,
    incoming_done: bool,
    next_incoming: bool,
}
impl Cursor {
    fn parse(value: &str) -> Result<Self, NeighborError> {
        let encoded = value
            .strip_prefix("neighbors-v1:")
            .ok_or("invalid neighbor cursor version")?;
        Ok(serde_json::from_str(encoded)?)
    }
    fn native(&self) -> Result<IncidentEdgesCursor, NeighborError> {
        let cursor = IncidentEdgesCursor::resume(
            self.anchor,
            self.outgoing_after
                .map(|[from, to]| MaintenanceEdgeKey::new(from, to)),
            self.incoming_after
                .map(|[from, to]| MaintenanceEdgeKey::new(from, to)),
            self.outgoing_done,
            self.incoming_done,
            if self.next_incoming {
                IncidentEdgeLeg::Incoming
            } else {
                IncidentEdgeLeg::Outgoing
            },
        )?;
        if cursor.is_complete() {
            return Err("neighbor cursor is exhausted; start a new inspection".into());
        }
        Ok(cursor)
    }
    fn encode(db_id: Ulid, cursor: IncidentEdgesCursor) -> Result<String, NeighborError> {
        let value = Self {
            db_id,
            anchor: cursor.anchor(),
            outgoing_after: cursor.outgoing_after().map(|key| [key.from, key.to]),
            incoming_after: cursor.incoming_after().map(|key| [key.from, key.to]),
            outgoing_done: cursor.outgoing_done(),
            incoming_done: cursor.incoming_done(),
            next_incoming: cursor.next_leg() == IncidentEdgeLeg::Incoming,
        };
        Ok(format!("neighbors-v1:{}", serde_json::to_string(&value)?))
    }
}

// Bound both UTF-8 content and its JSON-escaped representation. A control-heavy
// valid summary must not turn a full 64-row page into a response-size refusal.
fn bounded_summary(text: &str) -> (&str, bool) {
    let mut end = 0;
    let mut encoded_bytes = 0;
    for (index, character) in text.char_indices() {
        let bytes = match character {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
            character if character < '\u{20}' => 6,
            character => character.len_utf8(),
        };
        if index + character.len_utf8() > MAX_NEIGHBOR_SUMMARY_BYTES
            || encoded_bytes + bytes > MAX_NEIGHBOR_SUMMARY_BYTES
        {
            break;
        }
        encoded_bytes += bytes;
        end = index + character.len_utf8();
    }
    (&text[..end], end < text.len())
}

impl PreparedNeighbors {
    /// Frontends remove their database/identity envelope before admission.
    pub fn parse(raw: &Value) -> Result<Self, NeighborError> {
        let object = raw
            .as_object()
            .ok_or("neighbors arguments must be an object")?;
        if serde_json::to_vec(raw)?.len() > 8192 {
            return Err("neighbors input exceeds 8192 UTF-8 bytes".into());
        }
        for name in ["id", "limit", "after"] {
            if object.get(name).is_some_and(Value::is_null) {
                return Err(format!("neighbors {name} must not be null").into());
            }
        }
        let input: Self = serde_json::from_value(raw.clone())?;
        if !(1..=MAX_NEIGHBOR_LIMIT).contains(&input.limit) {
            return Err(format!("neighbors limit must be 1..={MAX_NEIGHBOR_LIMIT}").into());
        }
        if let Some(after) = &input.after {
            if after.is_empty()
                || after.len() > MAX_CURSOR_BYTES
                || after.chars().any(char::is_control)
            {
                return Err("neighbors after must be a nonempty bounded native cursor".into());
            }
            let cursor = Cursor::parse(after)?;
            cursor.native()?.validate(input.id)?;
        }
        Ok(input)
    }
    pub fn into_json(self) -> Value {
        let mut value = json!({"id":self.id,"limit":self.limit});
        if let Some(after) = self.after {
            value["after"] = json!(after);
        }
        value
    }
    pub async fn run(self, mem: &Memory, db_id: Ulid) -> Result<Value, NeighborError> {
        let after = if let Some(encoded) = &self.after {
            let cursor = Cursor::parse(encoded)?;
            if cursor.db_id != db_id {
                return Err("neighbor cursor database identity mismatch; start a new inspection in this database".into());
            }
            Some(cursor.native()?)
        } else {
            None
        };
        let request =
            IncidentEdgesRequest::new(self.id, self.limit, MAX_LINKED_READ_TIMEOUT, after)?;
        let page = tokio::time::timeout(
            MAX_LINKED_READ_TIMEOUT,
            mem.inspect_neighbors_page(&request),
        )
        .await
        .map_err(|_| "neighbor inspection exceeded its five-second deadline; retry this page")??;
        let mut items = Vec::with_capacity(page.items.len());
        for hydrated in page.items {
            let neighbor = hydrated.neighbor;
            let (summary, truncated) = hydrated
                .node
                .as_ref()
                .map(|node| {
                    let (text, truncated) = bounded_summary(node.summary());
                    (text.to_owned(), truncated)
                })
                .unwrap_or_default();
            let kind = match neighbor.edge.kind {
                EdgeKind::Associative => "associative",
                EdgeKind::Bridge => "bridge",
                EdgeKind::Transition => "transition",
                EdgeKind::Supersedes => "supersedes",
                EdgeKind::DerivedFrom => "derived_from",
            };
            items.push(
                json!({"neighbor":neighbor.node,"summary":summary,"summary_truncated":truncated,
                "kind":kind,"incoming":neighbor.incoming,"weight":neighbor.edge.weight(),
                "anchor":neighbor.edge.anchor.map(|span| [span.start,span.end])}),
            );
        }
        let next_cursor = page
            .next
            .map(|cursor| Cursor::encode(db_id, cursor))
            .transpose()?;
        let result = json!({"db_id":db_id.to_string(),"returned":items.len(),"items":items,
            "has_more":next_cursor.is_some(),"next_cursor":next_cursor,
            "coverage":{"order":"incident_dual_index_keyset","snapshot":false,
                "includes_episode_and_missing_endpoints":true,"rows_scanned":page.work.rows_scanned,
                "indexed_seeks":page.work.indexed_seeks,"edge_point_reads":page.work.edge_point_reads,
                "body_anchor_point_reads":page.work.body_anchor_point_reads,
                "max_rows_scanned":self.limit,"summary_max_bytes":MAX_NEIGHBOR_SUMMARY_BYTES,"summary_json_max_bytes":MAX_NEIGHBOR_SUMMARY_BYTES + 2}});
        if serde_json::to_vec(&result)?.len() > MAX_NEIGHBOR_PAGE_BYTES {
            return Err("neighbor response exceeds page byte budget".into());
        }
        Ok(result)
    }
}

pub fn neighbors_input_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["id"],
        "description":"Bounded raw adjacency inspection, including episode and missing endpoints. Indexed outgoing/incoming keyset order, not weight ranking. Follow next_cursor until null, including empty pages; a non-null tail may require one final empty read. Cursors bind logical database and anchor, not a snapshot. At most 64 physical rows/hydrated endpoints per call, summaries truncated at 1024 UTF-8 and JSON-escaped content bytes, response at most 128 KiB, no bodies or reinforcement. Self-edges can appear once per physical index leg.",
        "properties":{"id":{"type":"string","minLength":26,"maxLength":26},
            "limit":{"type":"integer","minimum":1,"maximum":MAX_NEIGHBOR_LIMIT,"default":DEFAULT_NEIGHBOR_LIMIT},
            "after":{"type":"string","minLength":1,"maxLength":MAX_CURSOR_BYTES}}})
}
