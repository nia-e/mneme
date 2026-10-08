//! Authored meaning and immutable summary citations, never inferred identity.
use crate::model::{Node, clean};
use serde_json::Value;
use std::collections::HashSet;
type Error = Box<dyn std::error::Error + Send + Sync>;
pub(crate) const PAGE_ITEMS: usize = 8;
#[derive(Clone, Debug, Default)]
pub struct TouchstoneCard {
    pub id: String,
    pub summary: String,
    pub subject: String,
    pub reference_count: usize,
    pub status: String,
}
#[derive(Clone, Debug, Default)]
pub struct TouchstonePage {
    pub items: Vec<TouchstoneCard>,
    pub next: Option<String>,
    pub partial: bool,
}
#[derive(Clone, Debug)]
pub struct TouchstoneDetail {
    pub node: Node,
    pub owner: String,
    pub subject: String,
    pub references: Vec<TouchstoneReference>,
    pub body_partial: bool,
    pub summary_partial: bool,
}
#[derive(Clone, Debug)]
pub struct TouchstoneReference {
    pub db_id: String,
    pub id: String,
    pub summary: String,
    pub provenance: String,
    pub created: u64,
    pub memory_kind: String,
    pub resolution: ReferenceResolution,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceResolution {
    Unchanged,
    SnapshotChanged,
    Absent,
    Unavailable,
}
#[derive(Clone, Debug)]
pub struct ExactTarget {
    pub db_id: String,
    pub node: Option<Node>,
    pub body_partial: bool,
    pub summary_partial: bool,
}
fn string<'a>(v: &'a Value, k: &str) -> Result<&'a str, Error> {
    v[k].as_str()
        .ok_or_else(|| format!("touchstone response omitted {k}").into())
}
pub(crate) fn canonical_id(s: &str) -> Result<(), Error> {
    if s.len() != 26
        || !matches!(s.as_bytes()[0], b'0'..=b'7')
        || !s
            .bytes()
            .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b))
    {
        return Err("touchstone identity is not a canonical ULID".into());
    }
    Ok(())
}
fn id(v: &Value, k: &str) -> Result<String, Error> {
    let s = string(v, k)?;
    canonical_id(s)?;
    Ok(s.into())
}
fn subject(v: &Value) -> Result<String, Error> {
    let s = string(v, "subject")?;
    if s.is_empty() || s.len() > 256 || s.trim() != s || s.chars().any(char::is_control) {
        return Err("invalid touchstone subject".into());
    }
    Ok(s.into())
}
fn bound(v: &Value, max: usize) -> Result<(), Error> {
    if serde_json::to_vec(v)?.len() > max {
        return Err("touchstone response exceeds aggregate byte bound".into());
    }
    Ok(())
}
pub(crate) fn parse_page(v: &Value, db: &str) -> Result<TouchstonePage, Error> {
    bound(v, 256 * 1024)?;
    if v["kind"] != "touchstones" || id(v, "db_id")? != db {
        return Err("touchstone page has wrong kind or owner identity".into());
    }
    let rows = v["items"]
        .as_array()
        .ok_or("touchstone page omitted items")?;
    if rows.len() > PAGE_ITEMS {
        return Err("touchstone page exceeds requested bound".into());
    }
    let next = match &v["next_cursor"] {
        Value::Null => None,
        Value::String(s) if !s.is_empty() && s.len() <= 1024 => Some(s.clone()),
        _ => return Err("invalid touchstone cursor".into()),
    };
    if v["has_more"].as_bool() != Some(next.is_some()) {
        return Err("touchstone continuation markers disagree".into());
    }
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for row in rows {
        let id = id(row, "id")?;
        if !seen.insert(id.clone()) {
            return Err("duplicate touchstone owner".into());
        }
        let summary = string(row, "summary")?;
        if summary.len() > 16 * 1024 {
            return Err("touchstone summary exceeds bound".into());
        }
        let status = string(row, "status")?;
        if !matches!(status, "Active" | "Archived") {
            return Err("invalid touchstone owner status".into());
        }
        let count = row["reference_count"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or("invalid touchstone reference count")?;
        items.push(TouchstoneCard {
            id,
            summary: clean(summary),
            subject: subject(row)?,
            reference_count: usize::try_from(count)
                .map_err(|_| "touchstone reference count exceeds platform capacity")?,
            status: status.into(),
        });
    }
    Ok(TouchstonePage {
        items,
        partial: next.is_some(),
        next,
    })
}
pub(crate) fn parse_detail(
    v: &Value,
    db: &str,
    requested: &str,
) -> Result<TouchstoneDetail, Error> {
    bound(v, 256 * 1024)?;
    canonical_id(db)?;
    canonical_id(requested)?;
    let node = crate::data::touchstone_node(v)?;
    let record = &v["touchstone"];
    bound(record, 64 * 1024)?;
    let owner = id(record, "owner")?;
    if owner != requested || node.id != requested {
        return Err("touchstone owner does not match requested annotation".into());
    }
    if v["summary_snapshot"]["coverage"] != "summary_only"
        || id(&v["summary_snapshot"], "db_id")? != db
        || id(&v["summary_snapshot"], "id")? != requested
    {
        return Err("touchstone annotation has wrong database binding".into());
    }
    if v["touchstone_current"]["coverage"] != "summary_only" {
        return Err("touchstone current resolution must be summary only".into());
    }
    let rows = record["references"]
        .as_array()
        .ok_or("touchstone snapshots missing")?;
    let current = v["touchstone_current"]["references"]
        .as_array()
        .ok_or("touchstone resolutions missing")?;
    if rows.is_empty() || current.len() != rows.len() {
        return Err("touchstone snapshot/resolution count invalid".into());
    }
    let mut seen = HashSet::new();
    let mut previous: Option<(String, String)> = None;
    let mut references = Vec::new();
    for (row, resolution) in rows.iter().zip(current) {
        let db_id = id(row, "db_id")?;
        let reference_id = id(row, "id")?;
        if db_id != db
            || reference_id == owner
            || !seen.insert((db_id.clone(), reference_id.clone()))
            || id(resolution, "db_id")? != db_id
            || id(resolution, "id")? != reference_id
        {
            return Err("touchstone reference identity or correspondence invalid".into());
        }
        let key = (db_id.clone(), reference_id.clone());
        if previous.as_ref().is_some_and(|old| old >= &key) {
            return Err("touchstone references are not in canonical order".into());
        }
        previous = Some(key);
        let status = match string(resolution, "status")? {
            "unchanged" => ReferenceResolution::Unchanged,
            "snapshot_changed" => ReferenceResolution::SnapshotChanged,
            "absent" => ReferenceResolution::Absent,
            "unavailable" => ReferenceResolution::Unavailable,
            _ => return Err("unknown touchstone resolution".into()),
        };
        let summary = string(row, "summary")?;
        if summary.len() > 16 * 1024 {
            return Err("touchstone snapshot summary exceeds bound".into());
        }
        let created = row["created"]
            .as_u64()
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or("invalid snapshot creation time")?;
        let memory_kind = string(&row["memory_kind"], "kind")?;
        if !matches!(memory_kind, "semantic" | "episode") {
            return Err("unknown snapshot memory kind".into());
        }
        references.push(TouchstoneReference {
            db_id,
            id: reference_id,
            summary: clean(summary),
            provenance: clean(&row["provenance"].to_string()),
            created,
            memory_kind: memory_kind.into(),
            resolution: status,
        });
    }
    Ok(TouchstoneDetail {
        node,
        owner,
        subject: subject(record)?,
        references,
        summary_partial: v["summary_truncated"] == true,
        body_partial: v["body_range"]["has_more"] == true
            || v["body"].as_str().is_some_and(|body| body.len() > 8192),
    })
}
pub fn demo_touchstones() -> TouchstonePage {
    let cards = [
        (
            "00000000000000000000000001",
            "The moment memory stopped being a leaderboard",
            "Continuity without a truth score",
            "Active",
        ),
        (
            "00000000000000000000000003",
            "Leave room for the strange idea",
            "Play as a working practice",
            "Active",
        ),
        (
            "00000000000000000000000005",
            "A missing account can still have mattered",
            "Keep the meaning, name the gap",
            "Archived",
        ),
    ];
    TouchstonePage {
        items: cards
            .into_iter()
            .map(|(id, summary, subject, status)| TouchstoneCard {
                id: id.into(),
                summary: summary.into(),
                subject: subject.into(),
                reference_count: 1,
                status: status.into(),
            })
            .collect(),
        ..Default::default()
    }
}
pub fn demo_detail(card: &TouchstoneCard) -> TouchstoneDetail {
    let (id, summary, resolution) = match card.id.as_str() {
        "00000000000000000000000003" => (
            "00000000000000000000000004",
            "Make room for play, even while the machinery changes",
            ReferenceResolution::Unchanged,
        ),
        "00000000000000000000000005" => (
            "00000000000000000000000006",
            "A captured account whose exact target is now absent",
            ReferenceResolution::Absent,
        ),
        _ => (
            "00000000000000000000000002",
            "An exact historical scene, not its successor",
            ReferenceResolution::SnapshotChanged,
        ),
    };
    TouchstoneDetail {
        node: Node {
            id: card.id.clone(),
            summary: card.summary.clone(),
            body: "A synthetic authored annotation. Keep the meaning; let the machinery change."
                .into(),
            status: card.status.clone(),
            ..Default::default()
        },
        owner: card.id.clone(),
        subject: card.subject.clone(),
        references: vec![TouchstoneReference {
            db_id: "00000000000000000000000000".into(),
            id: id.into(),
            summary: summary.into(),
            provenance: "Synthetic workshop example · never saved".into(),
            created: 42,
            memory_kind: "episode".into(),
            resolution,
        }],
        body_partial: false,
        summary_partial: false,
    }
}
