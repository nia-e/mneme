//! Bounded presentation of native episodes, not semantic hits or inferred scenes.
//! Episode roots and displayed immutable editions are deliberately separate.
use crate::model::clean;
use serde_json::Value;
use std::collections::HashSet;

type Error = Box<dyn std::error::Error + Send + Sync>;
pub(crate) const PAGE_ITEMS: usize = 32;
pub(crate) const BODY_BYTES: usize = 8192;
const RESPONSE_BYTES: usize = 34 * 1024; // Native 32 KiB payload plus owner envelope.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SceneAxis {
    #[default]
    Recorded,
    Occurred,
}
impl SceneAxis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Occurred => "occurred",
        }
    }
    pub fn other(self) -> Self {
        match self {
            Self::Recorded => Self::Occurred,
            Self::Occurred => Self::Recorded,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SceneOccurrence {
    #[default]
    Unknown,
    Point {
        at: u64,
    },
    Range {
        start: u64,
        end: u64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SceneContext {
    pub namespace: String,
    pub key: String,
    pub label: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub episode_id: String,
    pub edition_id: String,
    pub revision: u64,
    pub summary: String,
    pub occurred: SceneOccurrence,
    pub recorded_at: u64,
    pub edition_recorded_at: u64,
    pub thread: Option<String>,
    pub current_edition_id: String,
    pub occurrence_contexts: Vec<SceneContext>,
    pub body: String,
    pub body_partial: bool,
    pub source: String,
}
#[derive(Clone, Debug, Default)]
pub struct ScenePage {
    pub items: Vec<Scene>,
    pub axis: SceneAxis,
    pub partial: bool,
    /// Informational continuation: this UI keeps one bounded page, not a crawl.
    pub next: Option<String>,
    pub cue: Option<String>,
}

fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str, Error> {
    v[key]
        .as_str()
        .ok_or_else(|| format!("episode response omitted {key}").into())
}
fn id(v: &Value, key: &str) -> Result<String, Error> {
    let value = string(v, key)?;
    if value.len() != 26
        || !matches!(value.as_bytes()[0], b'0'..=b'7')
        || !value
            .bytes()
            .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b))
    {
        return Err(format!("episode {key} must be a canonical ULID").into());
    }
    Ok(value.to_owned())
}
fn time(v: &Value, key: &str) -> Result<u64, Error> {
    v[key]
        .as_u64()
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or_else(|| format!("episode response has invalid {key}").into())
}
fn bounded(value: &str, max: usize) -> String {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    clean(&value[..end])
}
fn header(v: &Value) -> Result<Scene, Error> {
    let revision = v["revision"]
        .as_u64()
        .filter(|n| *n <= u32::MAX as u64)
        .ok_or("episode revision is invalid")?;
    let summary = string(v, "summary")?;
    if summary.len() > 2048 {
        return Err("episode summary exceeds native bound".into());
    }
    let occurrence = &v["occurred"];
    let occurred = match string(occurrence, "kind")? {
        "unknown" => SceneOccurrence::Unknown,
        "point" => SceneOccurrence::Point {
            at: time(occurrence, "at")?,
        },
        "range" => {
            let start = time(occurrence, "start")?;
            let end = time(occurrence, "end")?;
            if end <= start {
                return Err("episode occurrence range is reversed".into());
            }
            SceneOccurrence::Range { start, end }
        }
        _ => return Err("episode occurrence kind is unsupported".into()),
    };
    let thread = match v.get("thread") {
        Some(Value::Null) | None => None,
        Some(Value::String(value)) if value.len() <= 128 => Some(clean(value)),
        _ => return Err("episode thread is invalid".into()),
    };
    let mut occurrence_contexts = Vec::new();
    if let Some(contexts) = v.get("occurrence_contexts") {
        if serde_json::to_vec(contexts)?.len() > 1024 {
            return Err("episode contexts exceed native bound".into());
        }
        let rows = contexts
            .as_array()
            .ok_or("episode contexts are not an array")?;
        if rows.is_empty() {
            return Err("episode contexts must be nonempty when supplied".into());
        }
        let mut seen = HashSet::new();
        for row in rows {
            let namespace = string(row, "namespace")?;
            let key = string(row, "key")?;
            let label = match row.get("label") {
                None => None,
                Some(Value::String(value)) => Some(value.as_str()),
                _ => return Err("episode context label is invalid".into()),
            };
            for text in [Some(namespace), Some(key), label].into_iter().flatten() {
                if text.is_empty() || text.trim() != text || text.chars().any(char::is_control) {
                    return Err("episode context identifier is invalid".into());
                }
            }
            if !seen.insert((namespace.to_owned(), key.to_owned())) {
                return Err("episode contexts contain duplicate identities".into());
            }
            occurrence_contexts.push(SceneContext {
                namespace: namespace.into(),
                key: key.into(),
                label: label.map(str::to_owned),
            });
        }
    }
    Ok(Scene {
        episode_id: id(v, "episode_id")?,
        edition_id: id(v, "edition_id")?,
        revision,
        summary: clean(summary),
        occurred,
        recorded_at: time(v, "recorded_at")?,
        edition_recorded_at: time(v, "edition_recorded_at")?,
        thread,
        current_edition_id: id(v, "current_edition_id")?,
        occurrence_contexts,
        ..Scene::default()
    })
}
fn owner(v: &Value, identity: &str) -> Result<(), Error> {
    if serde_json::to_vec(v)?.len() > RESPONSE_BYTES {
        return Err("episode response exceeds native aggregate bound".into());
    }
    if v["db_id"].as_str() != Some(identity) {
        return Err("episode response database identity mismatch".into());
    }
    Ok(())
}
pub(crate) fn parse_page(
    v: &Value,
    identity: &str,
    axis: SceneAxis,
    cue: Option<String>,
) -> Result<ScenePage, Error> {
    owner(v, identity)?;
    let action = if cue.is_some() { "search" } else { "list" };
    if v["action"] != action {
        return Err("episode response action mismatch".into());
    }
    let rows = v["items"].as_array().ok_or("episode page omitted items")?;
    if rows.len() > PAGE_ITEMS {
        return Err("episode page exceeds native item bound".into());
    }
    let mut items = Vec::with_capacity(rows.len());
    let mut roots = HashSet::new();
    for row in rows {
        let scene = header(row)?;
        if scene.edition_id != scene.current_edition_id {
            return Err("episode page returned a noncurrent edition".into());
        }
        if !roots.insert(scene.episode_id.clone()) {
            return Err("episode page contains duplicate roots".into());
        }
        items.push(scene);
    }
    let mut partial = v["partial"]
        .as_bool()
        .ok_or("episode page omitted partial marker")?;
    let next = if action == "list" {
        match v.get("next") {
            Some(Value::Null) => None,
            Some(Value::String(cursor))
                if !cursor.is_empty()
                    && cursor.len() <= 1024
                    && !cursor.chars().any(char::is_control) =>
            {
                Some(cursor.clone())
            }
            _ => return Err("episode page has invalid continuation".into()),
        }
    } else {
        if v["mode"] != "lexical" {
            return Err("episode search mode is unsupported".into());
        }
        partial |= v["has_more"]
            .as_bool()
            .ok_or("episode search omitted has_more")?;
        None
    };
    partial |= next.is_some();
    if cue.is_some() {
        // Lexical search supplies a bounded matching window, not a timeline.
        // Sort only delivered cards; this cannot claim newest matching globally.
        sort_scenes(&mut items, axis);
    }
    Ok(ScenePage {
        items,
        axis,
        partial,
        next,
        cue,
    })
}
pub(crate) fn sort_scenes(items: &mut [Scene], axis: SceneAxis) {
    items.sort_by(|a, b| {
        let key = |scene: &Scene| match axis {
            SceneAxis::Recorded => Some(scene.recorded_at),
            SceneAxis::Occurred => match scene.occurred {
                SceneOccurrence::Unknown => None,
                SceneOccurrence::Point { at } => Some(at),
                SceneOccurrence::Range { start, .. } => Some(start),
            },
        };
        key(b)
            .cmp(&key(a))
            .then_with(|| a.episode_id.cmp(&b.episode_id))
    });
}
pub(crate) fn parse_detail(
    v: &Value,
    identity: &str,
    root: &str,
    edition: &str,
) -> Result<Scene, Error> {
    owner(v, identity)?;
    if v["action"] != "get" {
        return Err("episode response action mismatch".into());
    }
    let mut scene = header(v)?;
    if scene.episode_id != root || scene.edition_id != edition {
        return Err("owner returned a different episode edition than requested".into());
    }
    if v["is_current"].as_bool() != Some(scene.edition_id == scene.current_edition_id) {
        return Err("episode current-head marker disagrees with edition identity".into());
    }
    let body = string(v, "body")?;
    scene.body = bounded(body, BODY_BYTES);
    scene.body_partial = v["body_range"]["has_more"]
        .as_bool()
        .ok_or("episode detail omitted body coverage")?
        || body.len() > BODY_BYTES;
    if !v["source"].is_object() {
        return Err("episode detail omitted source provenance".into());
    }
    // Metadata fits the native details envelope; body paging does not rewrite it.
    scene.source = clean(&v["source"].to_string());
    Ok(scene)
}

/// Authored-looking fixtures are explicitly synthetic; no service or model call.
pub fn demo_scenes(axis: SceneAxis) -> ScenePage {
    let summaries = [
        "Rain delayed the bridge repair; the next shift changed the plan",
        "A borrowed memory turned out to belong to the wrong project",
        "We stopped calling retrieval relevance a truth score",
        "A delivery arrived before its tracking status caught up",
        "An empty page was a continuation, not an empty history",
        "A failed validation changed our migration plan",
        "A scene can matter without becoming a general lesson",
        "The owner restarted; the activity ring was not the past coming alive",
        "We repaired the old greenhouse instead of rebuilding it",
        "The exact edition mattered more than the newest summary",
        "A touchstone held the reason we wanted to return",
        "The observatory learned to watch without becoming another writer",
    ];
    let base = 1_790_820_000_000;
    let mut items: Vec<_> = summaries.iter().enumerate().map(|(i, summary)| {
        let episode_id = format!("01ARZ3NDEKTSV4RRFFQ69G5F{:02}", i);
        let revised = i == 9;
        let edition_id = if revised { "01ARZ3NDEKTSV4RRFFQ69G5G09".into() } else { episode_id.clone() };
        let at = base + i as u64 * 3_600_000;
        Scene {
            episode_id, edition_id: edition_id.clone(), revision: if revised { 1 } else { 0 },
            summary: (*summary).into(),
            occurred: match i % 4 { 0 => SceneOccurrence::Unknown, 1 => SceneOccurrence::Range { start: at - 600_000, end: at }, _ => SceneOccurrence::Point { at } },
            recorded_at: at + 60_000, edition_recorded_at: at + if revised { 600_000 } else { 60_000 },
            thread: Some(if i % 3 == 0 { "example-handoff" } else { "workshop" }.into()),
            current_edition_id: edition_id,
            occurrence_contexts: if i % 4 == 0 { vec![] } else { vec![SceneContext { namespace: "project".into(), key: if i % 3 == 0 { "example-project" } else { "workshop" }.into(), label: Some(if i % 3 == 0 { "Example project handoff" } else { "The workshop" }.into()) }] },
            body: format!("DEMO · synthetic scene\n\n{summary}.\n\nThis is an account of a particular moment, not automatic advice for today. The interesting part was the revision: what we believed before the outcome, and what changed afterward.\n\nThe original evidence stayed attached to the exact edition. We chose not to turn uncertainty into a confident little slogan."),
            source: "DEMO · synthetic workshop account; not a stored record".into(), body_partial: false,
        }
    }).collect();
    if axis == SceneAxis::Occurred {
        items.retain(|scene| scene.occurred != SceneOccurrence::Unknown);
    }
    items.reverse();
    ScenePage {
        items,
        axis,
        partial: false,
        next: None,
        cue: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const DB: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const ROOT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
    const NEW: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
    pub(crate) fn header_fixture() -> Value {
        json!({"episode_id":ROOT,"edition_id":ROOT,"current_edition_id":ROOT,
            "revision":0,"summary":"A particular moment","occurred":{"kind":"unknown"},
            "recorded_at":42,"edition_recorded_at":42,"thread":null})
    }
    fn page() -> Value {
        json!({"db_id":DB,"action":"list","items":[header_fixture()],"next":null,"partial":false})
    }
    fn detail() -> Value {
        let mut value = header_fixture();
        value.as_object_mut().unwrap().extend(
            json!({"db_id":DB,"action":"get","is_current":true,
            "source":{"namespace":"workshop","key":"one","reference":"session"},
            "body":"an account\u{001b}[31m","body_range":{"has_more":false}})
            .as_object()
            .unwrap()
            .clone(),
        );
        value
    }
    #[test]
    fn initial_revision_zero_and_unknown_occurrence_are_native_scenes() {
        let page = parse_page(&page(), DB, SceneAxis::Recorded, None).unwrap();
        assert_eq!(page.items[0].revision, 0);
        assert_eq!(page.items[0].occurred, SceneOccurrence::Unknown);
        assert!(page.items[0].occurrence_contexts.is_empty());
        assert!(!page.partial);
    }
    #[test]
    fn exact_detail_keeps_historical_edition_and_reports_new_head() {
        let mut value = detail();
        value["current_edition_id"] = json!(NEW);
        value["is_current"] = json!(false);
        let scene = parse_detail(&value, DB, ROOT, ROOT).unwrap();
        assert_eq!(scene.edition_id, ROOT);
        assert_eq!(scene.current_edition_id, NEW);
        assert!(!scene.body.contains('\u{001b}'));
        value["edition_id"] = json!(NEW);
        assert!(parse_detail(&value, DB, ROOT, ROOT).is_err());
    }
    #[test]
    fn detail_body_is_utf8_bounded_and_coverage_is_explicit() {
        let mut value = detail();
        value["body"] = json!("🦋".repeat(2500));
        let scene = parse_detail(&value, DB, ROOT, ROOT).unwrap();
        assert_eq!(scene.body.len(), BODY_BYTES);
        assert!(scene.body_partial);
        value["body"] = json!("bounded excerpt");
        value["body_range"]["has_more"] = json!(true);
        assert!(parse_detail(&value, DB, ROOT, ROOT).unwrap().body_partial);
    }
    #[test]
    fn owner_identity_current_page_and_continuation_are_checked() {
        let mut value = page();
        assert!(parse_page(&value, NEW, SceneAxis::Recorded, None).is_err());
        value["next"] = json!("opaque-cursor");
        assert!(
            parse_page(&value, DB, SceneAxis::Recorded, None)
                .unwrap()
                .partial
        );
        value["items"][0]["current_edition_id"] = json!(NEW);
        assert!(parse_page(&value, DB, SceneAxis::Recorded, None).is_err());
    }
    #[test]
    fn malformed_times_contexts_and_duplicate_roots_are_refused() {
        let mut value = page();
        value["items"][0]["occurred"] = json!({"kind":"range","start":4,"end":4});
        assert!(parse_page(&value, DB, SceneAxis::Recorded, None).is_err());
        let mut value = page();
        value["items"][0]["occurrence_contexts"] =
            json!([{"namespace":"project","key":"one"},{"namespace":"project","key":"one"}]);
        assert!(parse_page(&value, DB, SceneAxis::Recorded, None).is_err());
        let mut value = page();
        value["items"] = json!([header_fixture(), header_fixture()]);
        assert!(parse_page(&value, DB, SceneAxis::Recorded, None).is_err());
        value["items"] = json!((0..33).map(|_| header_fixture()).collect::<Vec<_>>());
        assert!(parse_page(&value, DB, SceneAxis::Recorded, None).is_err());
    }
    #[test]
    fn lexical_window_sorts_by_selected_axis_with_unknown_last() {
        let mut later = header_fixture();
        later["episode_id"] = json!(NEW);
        later["edition_id"] = json!(NEW);
        later["current_edition_id"] = json!(NEW);
        later["occurred"] = json!({"kind":"point","at":100});
        let value = json!({"db_id":DB,"action":"search","mode":"lexical","items":[header_fixture(),later],"has_more":true,"partial":false});
        let page = parse_page(&value, DB, SceneAxis::Occurred, Some("moment".into())).unwrap();
        assert_eq!(page.items[0].episode_id, NEW);
        assert_eq!(page.items[1].occurred, SceneOccurrence::Unknown);
        assert!(page.partial);
        assert_eq!(page.cue.as_deref(), Some("moment"));
    }
    #[test]
    fn demo_is_bounded_and_axis_does_not_invent_unknown_occurrences() {
        let recorded = demo_scenes(SceneAxis::Recorded);
        assert_eq!(recorded.items.len(), 12);
        assert!(recorded.items.iter().any(|scene| scene.revision == 1));
        assert!(
            recorded
                .items
                .iter()
                .any(|scene| scene.occurred == SceneOccurrence::Unknown)
        );
        let occurred = demo_scenes(SceneAxis::Occurred);
        assert!(occurred.items.len() < recorded.items.len());
        assert!(
            occurred
                .items
                .iter()
                .all(|scene| scene.occurred != SceneOccurrence::Unknown)
        );
    }
}
