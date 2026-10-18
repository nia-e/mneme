//! Domain admission before any owner connection. Shared app parsers remain the
//! source of truth for unions; the small scalar read surface is closed here.
use crate::{AnyErr, RequestAccess};
use mneme_app::{
    edit_body::PreparedBodyEdit, edit_summary::PreparedSummaryEdit, retag::PreparedRetag,
};
use serde_json::{Value, json};
pub(crate) enum Checked {
    Ordinary,
    Retag(PreparedRetag),
    Body(PreparedBodyEdit),
    Summary(PreparedSummaryEdit),
}
impl Checked {
    pub fn operator(&self) -> bool {
        matches!(self,Self::Retag(request) if request.requires_operator())
    }
    pub fn verify(&self, result: &Value, db: &str, id: &str) -> Result<(), AnyErr> {
        match self {
            Self::Retag(request) => {
                request.validate_routed_response_json(result, db, Some(id.parse()?))
            }
            Self::Body(request) => {
                request.validate_routed_response_json(result, db, Some(id.parse()?))
            }
            Self::Summary(request) => {
                request.validate_routed_response_json(result, db, Some(id.parse()?))
            }
            Self::Ordinary => Ok(()),
        }
    }
}
pub(crate) fn prepare(tool: &str, args: &Value, _access: RequestAccess) -> Result<Checked, AnyErr> {
    let mut domain = args
        .as_object()
        .ok_or("arguments must be an object")?
        .clone();
    domain.remove("db");
    domain.remove("expected_db_id");
    domain.remove("_mneme_source");
    let raw = Value::Object(domain);
    match tool {
        "retag" => return Ok(Checked::Retag(PreparedRetag::parse(&raw)?)),
        "edit_body" => return Ok(Checked::Body(PreparedBodyEdit::parse(&raw)?)),
        "edit_summary" => return Ok(Checked::Summary(PreparedSummaryEdit::parse(&raw)?)),
        "save" => {
            mneme_app::save::PreparedSave::parse(&raw, "configured-manual-admission")?;
        }
        "capture" => {
            mneme_app::capture::PreparedCapture::parse(&raw)?;
        }
        "episode" => {
            mneme_app::episode::PreparedEpisode::parse(&raw)?;
        }
        "concern" => {
            mneme_app::concern::PreparedConcernRequest::parse(&raw)?;
        }
        "list" => {
            mneme_app::list::PreparedList::parse(&raw)?;
        }
        "graph" => {
            mneme_app::graph_view::PreparedGraph::parse(&raw)?;
        }
        "neighbors" => {
            mneme_app::neighbors::PreparedNeighbors::parse(&raw)?;
        }
        "get" => {
            fields(
                &raw,
                &["id", "body", "edges", "body_offset", "max_body_bytes"],
            )?;
            id(&raw, "id")?;
            boolean(&raw, "body")?;
            boolean(&raw, "edges")?;
            integer(&raw, "body_offset", 0, u64::MAX)?;
            integer(&raw, "max_body_bytes", 1, 1024 * 1024)?;
            if raw.get("body_offset").is_some() && raw["body"] != true {
                return Err("body_offset requires body:true".into());
            }
        }
        "core" => {
            fields(&raw, &["max_body_bytes"])?;
            integer(&raw, "max_body_bytes", 1, 1024 * 1024)?;
        }
        "databases" | "status" | "contradictions" | "merges" | "decay" | "prune"
        | "snapshot_create" => fields(&raw, &[])?,
        "activity" => {
            fields(&raw, &["after", "limit"])?;
            integer(&raw, "after", 0, u64::MAX)?;
            integer(&raw, "limit", 1, 64)?;
        }
        "query" | "recall_context" => {
            let allowed = if tool == "query" {
                &[
                    "text",
                    "k",
                    "depth",
                    "max_nodes",
                    "min_relevance",
                    "archived",
                    "summary_max_bytes",
                    "tags",
                ][..]
            } else {
                &[
                    "text",
                    "k",
                    "depth",
                    "max_nodes",
                    "min_relevance",
                    "tags",
                    "observe",
                    "routing_hints",
                ][..]
            };
            fields(&raw, allowed)?;
            text(&raw, "text", 8192)?;
            integer(&raw, "k", 1, 64)?;
            integer(&raw, "depth", 0, 12)?;
            integer(&raw, "max_nodes", 1, 256)?;
            number(&raw, "min_relevance")?;
            if let Some(value) = raw.get("summary_max_bytes") {
                serde_json::from_value::<mneme_app::query::SummaryMaxBytes>(value.clone())?;
            }
            boolean(&raw, "archived")?;
            boolean(&raw, "observe")?;
            if let Some(tags) = raw.get("tags") {
                let tags = tags.as_array().ok_or("tags must be an array")?;
                if tags.len() > 32 {
                    return Err("too many tags".into());
                }
                for tag in tags {
                    mneme_core::normalize_tag(tag.as_str().ok_or("tags must be strings")?)?;
                }
            }
        }
        "recall" => {
            fields(&raw, &["text", "expand_top", "neighbors_each"])?;
            text(&raw, "text", 8192)?;
            integer(&raw, "expand_top", 1, 16)?;
            integer(&raw, "neighbors_each", 1, 16)?;
        }
        "walk" => {
            let action = raw["action"].as_str().ok_or("walk action required")?;
            let allowed = match action {
                "start" => vec!["action", "start", "budget", "query"],
                "body" => vec!["action", "session", "body_offset", "max_body_bytes"],
                "go" => vec!["action", "session", "to"],
                "look" | "edges" | "back" | "done" | "abort" => vec!["action", "session"],
                _ => return Err("unsupported walk action".into()),
            };
            fields(&raw, &allowed)?;
            if action == "start" {
                id(&raw, "start")?;
                integer(&raw, "budget", 1, 64)?;
                if raw.get("query").is_some() {
                    text(&raw, "query", 8192)?;
                }
            } else {
                text(&raw, "session", 256)?;
                if action == "go" {
                    text(&raw, "to", 256)?;
                }
                if action == "body" {
                    integer(&raw, "body_offset", 0, u64::MAX)?;
                    integer(&raw, "max_body_bytes", 1, 1024 * 1024)?;
                }
            }
        }
        "reflect" => {
            fields(&raw, &["receipts", "used", "unhelpful"])?;
            for key in ["receipts", "used"] {
                if !raw[key].is_array() {
                    return Err("reflect requires receipts and used arrays".into());
                }
            }
            for key in ["receipts", "used", "unhelpful"] {
                if let Some(items) = raw.get(key) {
                    let items = items.as_array().ok_or("reflect arrays must be arrays")?;
                    if items.len() > if key == "receipts" { 8 } else { 64 }
                        || (key == "receipts" && items.is_empty())
                    {
                        return Err("reflect array allowance exceeded".into());
                    }
                    for item in items {
                        let text = item
                            .as_str()
                            .ok_or("reflect array members must be strings")?;
                        if text.is_empty() || text.len() > 256 {
                            return Err("reflect array member exceeds allowance".into());
                        }
                        if key != "receipts" {
                            let _: ulid::Ulid = text.parse()?;
                        }
                    }
                }
            }
            if raw["used"].as_array().unwrap().len()
                + raw["unhelpful"].as_array().map_or(0, Vec::len)
                > 64
            {
                return Err("reflect accepts at most 64 total judgments".into());
            }
            if raw["used"].as_array().unwrap().iter().any(|id| {
                raw["unhelpful"]
                    .as_array()
                    .is_some_and(|ids| ids.contains(id))
            }) {
                return Err("reflect judgments must be disjoint".into());
            }
        }
        "remote_edges" => {
            fields(&raw, &["id", "limit", "after", "resolve"])?;
            id(&raw, "id")?;
            integer(
                &raw,
                "limit",
                1,
                mneme_core::MAX_REMOTE_EDGE_PAGE_SIZE as u64,
            )?;
            boolean(&raw, "resolve")?;
            if let Some(after) = raw.get("after") {
                let cursor: mneme_core::RemoteEdgeCursor = serde_json::from_value(after.clone())?;
                cursor.validate_for(mneme_core::NodeId(raw["id"].as_str().unwrap().parse()?))?;
            }
        }
        "ingest" => {
            fields(
                &raw,
                &["summary", "body", "tags", "stability", "confidence"],
            )?;
            text(&raw, "summary", 2048)?;
            mneme_core::NodeSummary::new(raw["summary"].as_str().unwrap())?;
            if raw.get("body").is_some() {
                text(&raw, "body", 256 * 1024)?;
            }
            number(&raw, "stability")?;
            number(&raw, "confidence")?;
            if let Some(tags) = raw.get("tags") {
                let tags = tags.as_array().ok_or("tags must be array")?;
                if tags.len() > 32 {
                    return Err("too many tags".into());
                }
                for tag in tags {
                    mneme_core::normalize_tag(tag.as_str().ok_or("tags must be strings")?)?;
                }
            }
        }
        "contradict" => {
            fields(&raw, &["a", "b"])?;
            id(&raw, "a")?;
            id(&raw, "b")?;
        }
        "reconcile" => {
            fields(&raw, &["a", "b", "resolution"])?;
            id(&raw, "a")?;
            id(&raw, "b")?;
            if ![json!("context-dependent"), json!("unresolved")].contains(&raw["resolution"]) {
                return Err("reconcile resolution must be context-dependent|unresolved".into());
            }
        }
        "feedback" => {
            fields(&raw, &["from", "to", "signal"])?;
            id(&raw, "to")?;
            if raw.get("from").is_some() {
                id(&raw, "from")?;
            }
            if ![json!("relevant"), json!("not-new"), json!("irrelevant")].contains(&raw["signal"])
            {
                return Err("feedback signal unsupported".into());
            }
        }
        "merge" => match raw["mode"].as_str() {
            Some("full") => {
                fields(&raw, &["mode", "winner", "loser"])?;
                id(&raw, "winner")?;
                id(&raw, "loser")?;
            }
            Some("keep") => {
                fields(&raw, &["mode", "a", "b"])?;
                id(&raw, "a")?;
                id(&raw, "b")?;
            }
            _ => return Err("merge mode must be full|keep".into()),
        },
        "forget" => {
            fields(&raw, &["id"])?;
            id(&raw, "id")?;
        }
        "supersede" => {
            fields(&raw, &["winner", "loser"])?;
            id(&raw, "winner")?;
            id(&raw, "loser")?;
        }
        "link" => {
            fields(
                &raw,
                &["from", "to", "kind", "weight", "anchor_start", "anchor_end"],
            )?;
            id(&raw, "from")?;
            id(&raw, "to")?;
            number(&raw, "weight")?;
            if let Some(kind) = raw.get("kind") {
                if ![
                    json!("associative"),
                    json!("transition"),
                    json!("supersedes"),
                    json!("derived_from"),
                ]
                .contains(kind)
                {
                    return Err("invalid link kind".into());
                }
            }
            integer(&raw, "anchor_start", 0, u32::MAX as u64)?;
            integer(&raw, "anchor_end", 0, u32::MAX as u64)?;
            if raw.get("anchor_start").is_some() != raw.get("anchor_end").is_some()
                || raw["anchor_start"]
                    .as_u64()
                    .zip(raw["anchor_end"].as_u64())
                    .is_some_and(|(start, end)| start > end)
            {
                return Err("link anchors must be paired and ordered".into());
            }
        }
        _ => return Err("unknown configured operation".into()),
    }
    Ok(Checked::Ordinary)
}
fn fields(raw: &Value, allowed: &[&str]) -> Result<(), AnyErr> {
    for (key, value) in raw.as_object().unwrap() {
        if !allowed.contains(&key.as_str()) || value.is_null() {
            return Err(format!("unknown or null configured argument {key:?}").into());
        }
    }
    Ok(())
}
fn text(raw: &Value, key: &str, max: usize) -> Result<(), AnyErr> {
    let value = raw[key]
        .as_str()
        .ok_or_else(|| format!("{key} must be a string"))?;
    if value.len() > max {
        return Err(format!("{key} exceeds UTF-8 allowance").into());
    }
    Ok(())
}
fn id(raw: &Value, key: &str) -> Result<(), AnyErr> {
    let value = raw[key]
        .as_str()
        .ok_or("node identity must be a canonical ULID")?;
    let id: ulid::Ulid = value.parse()?;
    if id.to_string() != value {
        return Err("node identity must be a canonical ULID".into());
    }
    Ok(())
}
fn boolean(raw: &Value, key: &str) -> Result<(), AnyErr> {
    if raw.get(key).is_some_and(|v| !v.is_boolean()) {
        return Err(format!("{key} must be boolean").into());
    }
    Ok(())
}
fn integer(raw: &Value, key: &str, min: u64, max: u64) -> Result<(), AnyErr> {
    if let Some(value) = raw.get(key) {
        if !value.as_u64().is_some_and(|v| v >= min && v <= max) {
            return Err(format!("{key} must be a bounded unsigned integer").into());
        }
    }
    Ok(())
}
fn number(raw: &Value, key: &str) -> Result<(), AnyErr> {
    if let Some(value) = raw.get(key) {
        if !value.as_f64().is_some_and(|v| (0.0..=1.0).contains(&v)) {
            return Err(format!("{key} must be a number in 0..=1").into());
        }
    }
    Ok(())
}
