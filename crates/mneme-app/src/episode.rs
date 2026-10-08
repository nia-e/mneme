//! Shared episode admission, execution and bounded presentation.
//!
//! Database selection and authority belong to the transport. This module validates
//! the complete request before either frontend opens a store or contacts an owner.

use mneme_core::{CaptureSource, EdgeKind, MAX_TAG_BYTES, NodeId, Provenance, episode::*};
use mneme_engine::{CaptureLink, Memory, episode::EpisodeWrite};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashSet, error::Error};
use ulid::Ulid;

/// Interpretation guidance only; no type or schema behavior changes.
pub const MEMORY_TIME_GUIDANCE: &str = "Episodes and dated reports describe state at the time, not necessarily now. An earlier account can be accurate as of then while later records describe a change. Distinguish historical progression from factual correction, and latest recorded state from verified current state. A current episode edition is the latest account, not proof of current reality; occurrence time and recording time differ.";

pub type EpisodeError = Box<dyn Error + Send + Sync>;
const MAX_INPUT_BYTES: usize = 128 * 1024;
const MAX_TAGS: usize = 32;
/// Reserve half of each bounded details response for actual body progress.
pub const MAX_EPISODE_METADATA_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpisodeAction {
    Append,
    List,
    Search,
    Get,
    Revise,
    History,
    References,
}
impl EpisodeAction {
    pub const ALL: [Self; 7] = [
        Self::Append,
        Self::List,
        Self::Search,
        Self::Get,
        Self::Revise,
        Self::History,
        Self::References,
    ];
    pub const READ_ONLY: [Self; 5] = [
        Self::List,
        Self::Search,
        Self::Get,
        Self::History,
        Self::References,
    ];
    pub const CURATOR: [Self; 6] = [
        Self::Append,
        Self::List,
        Self::Search,
        Self::Get,
        Self::History,
        Self::References,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::List => "list",
            Self::Search => "search",
            Self::Get => "get",
            Self::Revise => "revise",
            Self::History => "history",
            Self::References => "references",
        }
    }
    pub const fn capability(self) -> EpisodeCapability {
        match self {
            Self::Append => EpisodeCapability::Curator,
            Self::Revise => EpisodeCapability::Operator,
            _ => EpisodeCapability::ReadOnly,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpisodeCapability {
    ReadOnly,
    Curator,
    Operator,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceInput {
    namespace: String,
    key: String,
    reference: String,
    session: Option<String>,
    revision: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkInput {
    to: String,
    kind: Option<String>,
    weight: Option<f32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    source: SourceInput,
    summary: String,
    body: Option<String>,
    tags: Option<Vec<String>>,
    occurred: Option<Value>,
    thread: Option<String>,
    occurrence_contexts: Option<Value>,
    links: Option<Vec<LinkInput>>,
}
struct PreparedWrite {
    input: WriteInput,
    occurrence: OccurrenceSpan,
    thread: Option<EpisodeThread>,
    occurrence_contexts: Option<OccurrenceContexts>,
    links: Vec<CaptureLink>,
}

enum Request {
    Append(PreparedWrite),
    Revise {
        root: EpisodeId,
        expected: NodeId,
        reason: EpisodeRevisionReason,
        write: PreparedWrite,
    },
    List(EpisodeTimelineRequest),
    Search(EpisodeCueRequest),
    Get {
        request: EpisodeGet,
        body: bool,
        offset: u64,
        max_bytes: usize,
    },
    History(EpisodeHistoryRequest),
    References(EpisodeReferencesRequest),
}

pub struct PreparedEpisode {
    raw: Value,
    request: Request,
}

fn invalid(message: impl Into<String>) -> EpisodeError {
    message.into().into()
}
fn object(value: &Value) -> Result<&serde_json::Map<String, Value>, EpisodeError> {
    value
        .as_object()
        .ok_or_else(|| invalid("episode arguments must be an object"))
}
fn fields(value: &Value, allowed: &[&str]) -> Result<(), EpisodeError> {
    for (name, value) in object(value)? {
        if !allowed.contains(&name.as_str()) {
            return Err(invalid(format!("unknown episode field `{name}`")));
        }
        if value.is_null() {
            return Err(invalid(format!("episode field `{name}` must not be null")));
        }
    }
    Ok(())
}
fn text<'a>(value: &'a Value, name: &str) -> Result<&'a str, EpisodeError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("episode field `{name}` must be a string")))
}
fn bounded_text(name: &str, value: &str, max: usize) -> Result<(), EpisodeError> {
    if value.is_empty()
        || value.len() > max
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(invalid(format!(
            "episode `{name}` must be trimmed, control-free text of 1..={max} UTF-8 bytes"
        )));
    }
    Ok(())
}
fn node_id(value: &Value, name: &str) -> Result<NodeId, EpisodeError> {
    parse_node_id(text(value, name)?)
}
fn parse_node_id(text: &str) -> Result<NodeId, EpisodeError> {
    let id = Ulid::from_string(text)?;
    if text.len() != 26 || id.to_string() != text.to_ascii_uppercase() {
        return Err(invalid("episode node ID must be a canonical ULID"));
    }
    Ok(NodeId(id))
}
fn root_id(value: &Value, name: &str) -> Result<EpisodeId, EpisodeError> {
    Ok(EpisodeId::new(node_id(value, name)?))
}
fn number(value: &Value, name: &str) -> Result<u64, EpisodeError> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid(format!("episode `{name}` must be a nonnegative integer")))
}
fn time(value: &Value, name: &str) -> Result<EpisodeTime, EpisodeError> {
    Ok(EpisodeTime::new(number(value, name)?.into())?)
}
fn window(value: &Value) -> Result<Option<EpisodeTimeWindow>, EpisodeError> {
    let from = value.get("from").map(|_| time(value, "from")).transpose()?;
    let through = value
        .get("through")
        .map(|_| time(value, "through"))
        .transpose()?;
    if from.is_none() && through.is_none() {
        return Ok(None);
    }
    Ok(Some(EpisodeTimeWindow::new(from, through)?))
}
fn occurrence(value: Option<&Value>) -> Result<OccurrenceSpan, EpisodeError> {
    let Some(value) = value else {
        return Ok(OccurrenceSpan::Unknown);
    };
    match text(value, "kind")? {
        "unknown" => {
            fields(value, &["kind"])?;
            Ok(OccurrenceSpan::Unknown)
        }
        "point" => {
            fields(value, &["kind", "at"])?;
            Ok(OccurrenceSpan::Point {
                at: time(value, "at")?,
            })
        }
        "range" => {
            fields(value, &["kind", "start", "end"])?;
            let start = time(value, "start")?;
            let end = time(value, "end")?;
            if start >= end {
                return Err(invalid("episode occurred range requires start < end"));
            }
            Ok(OccurrenceSpan::Range { start, end })
        }
        _ => Err(invalid("episode occurred.kind must be unknown|point|range")),
    }
}
fn occurrence_contexts(value: Option<&Value>) -> Result<Option<OccurrenceContexts>, EpisodeError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or("episode occurrence_contexts must be a nonempty array")?;
    let contexts = values
        .iter()
        .map(|value| -> Result<_, EpisodeError> {
            fields(value, &["namespace", "key", "label"])?;
            Ok(OccurrenceContextRef::new(
                text(value, "namespace")?,
                text(value, "key")?,
                value
                    .get("label")
                    .map(|_| text(value, "label"))
                    .transpose()?,
            )?)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(OccurrenceContexts::new(contexts)?))
}
fn filter(value: &Value) -> Result<EpisodeFilter, EpisodeError> {
    let thread = value
        .get("thread")
        .map(|_| -> Result<_, EpisodeError> { Ok(EpisodeThread::new(text(value, "thread")?)?) })
        .transpose()?;
    let occurrence = match value.get("occurrence") {
        None => EpisodeOccurrenceFilter::Any,
        Some(v) => match text(v, "kind")? {
            "any" => {
                fields(v, &["kind"])?;
                EpisodeOccurrenceFilter::Any
            }
            "unknown" => {
                fields(v, &["kind"])?;
                EpisodeOccurrenceFilter::Unknown
            }
            "overlaps" => {
                fields(v, &["kind", "from", "through"])?;
                EpisodeOccurrenceFilter::Overlaps(
                    window(v)?.ok_or("episode overlap requires from or through")?,
                )
            }
            _ => {
                return Err(invalid(
                    "episode occurrence.kind must be any|unknown|overlaps",
                ));
            }
        },
    };
    Ok(EpisodeFilter { thread, occurrence })
}
fn limit(value: &Value) -> Result<EpisodePageLimit, EpisodeError> {
    Ok(EpisodePageLimit::new(
        value
            .get("limit")
            .map(|_| number(value, "limit"))
            .transpose()?
            .unwrap_or(8)
            .try_into()
            .map_err(|_| invalid("episode limit must be 1..32"))?,
    )?)
}
fn parse_write(mut value: Value) -> Result<PreparedWrite, EpisodeError> {
    let obj = value
        .as_object_mut()
        .ok_or("episode write must be an object")?;
    for name in ["action", "episode_id", "expected_edition_id", "reason"] {
        obj.remove(name);
    }
    let input: WriteInput = serde_json::from_value(value)?;
    let source = CaptureSource::new(
        &input.source.namespace,
        &input.source.key,
        &input.source.reference,
        input.source.session.as_deref(),
        input.source.revision.as_deref(),
        [0; 32],
    )?;
    bounded_text("summary", &input.summary, MAX_EPISODE_SUMMARY_BYTES)?;
    if input
        .body
        .as_ref()
        .is_some_and(|v| v.len() > MAX_EPISODE_BODY_BYTES)
    {
        return Err(invalid("episode body exceeds 16384 UTF-8 bytes"));
    }
    let tags = input.tags.as_deref().unwrap_or(&[]);
    if tags.len() > MAX_TAGS {
        return Err(invalid("episode tags exceed 32 entries"));
    }
    for tag in tags {
        bounded_text("tags[]", tag, MAX_TAG_BYTES)?;
        if tag == "core" {
            return Err(invalid("episodes cannot have the core tag"));
        }
    }
    let occurrence = occurrence(input.occurred.as_ref())?;
    let thread = input.thread.as_ref().map(EpisodeThread::new).transpose()?;
    let occurrence_contexts = occurrence_contexts(input.occurrence_contexts.as_ref())?;
    let mut targets = HashSet::new();
    let mut links = Vec::new();
    for link in input.links.as_deref().unwrap_or(&[]) {
        if links.len() == MAX_EPISODE_LINKS {
            return Err(invalid("episode links exceed 8 entries"));
        }
        let to = parse_node_id(&link.to)?;
        if to == source.node_id() || !targets.insert(to) {
            return Err(invalid("episode links require distinct, non-self targets"));
        }
        let kind = match link.kind.as_deref().unwrap_or("associative") {
            "associative" => EdgeKind::Associative,
            "transition" => EdgeKind::Transition,
            "derived_from" => EdgeKind::DerivedFrom,
            _ => {
                return Err(invalid(
                    "episode link kind must be associative|transition|derived_from",
                ));
            }
        };
        links.push(CaptureLink::new(to, kind, link.weight.unwrap_or(0.5))?);
    }
    Ok(PreparedWrite {
        input,
        occurrence,
        thread,
        occurrence_contexts,
        links,
    })
}

impl PreparedEpisode {
    pub fn parse(raw: &Value) -> Result<Self, EpisodeError> {
        if serde_json::to_vec(raw)?.len() > MAX_INPUT_BYTES {
            return Err(invalid("episode input exceeds 128 KiB"));
        }
        object(raw)?;
        // Refuse nulls at every optional object boundary rather than letting
        // serde's Option turn explicit invalid values into omission.
        for (name, value) in object(raw)? {
            if value.is_null() {
                return Err(invalid(format!("episode `{name}` must not be null")));
            }
        }
        if let Some(source) = raw.get("source") {
            fields(
                source,
                &["namespace", "key", "reference", "session", "revision"],
            )?;
        }
        if let Some(links) = raw.get("links") {
            for link in links.as_array().ok_or("episode links must be an array")? {
                fields(link, &["to", "kind", "weight"])?;
            }
        }
        let request = match text(raw, "action")? {
            "append" => {
                fields(raw, action_fields(EpisodeAction::Append).0)?;
                Request::Append(parse_write(raw.clone())?)
            }
            "revise" => {
                fields(raw, action_fields(EpisodeAction::Revise).0)?;
                Request::Revise {
                    root: root_id(raw, "episode_id")?,
                    expected: node_id(raw, "expected_edition_id")?,
                    reason: EpisodeRevisionReason::new(text(raw, "reason")?)?,
                    write: parse_write(raw.clone())?,
                }
            }
            "list" => {
                fields(raw, action_fields(EpisodeAction::List).0)?;
                let axis = match raw
                    .get("axis")
                    .map(|_| text(raw, "axis"))
                    .transpose()?
                    .unwrap_or("recorded")
                {
                    "recorded" => EpisodeTimelineAxis::Recorded,
                    "occurred" => EpisodeTimelineAxis::Occurred,
                    _ => return Err(invalid("episode axis must be recorded|occurred")),
                };
                let order = match raw
                    .get("order")
                    .map(|_| text(raw, "order"))
                    .transpose()?
                    .unwrap_or("newest_first")
                {
                    "newest_first" => EpisodeOrder::NewestFirst,
                    "oldest_first" => EpisodeOrder::OldestFirst,
                    _ => return Err(invalid("episode order must be newest_first|oldest_first")),
                };
                let filter = filter(raw)?;
                if matches!(axis, EpisodeTimelineAxis::Occurred)
                    && matches!(filter.occurrence, EpisodeOccurrenceFilter::Unknown)
                {
                    return Err(invalid(
                        "occurred timeline cannot select unknown occurrence",
                    ));
                }
                Request::List(EpisodeTimelineRequest {
                    axis,
                    order,
                    window: window(raw)?,
                    filter,
                    limit: limit(raw)?,
                    after: raw
                        .get("after")
                        .map(|_| -> Result<_, EpisodeError> { Ok(text(raw, "after")?.parse()?) })
                        .transpose()?,
                })
            }
            "search" => {
                fields(raw, action_fields(EpisodeAction::Search).0)?;
                Request::Search(EpisodeCueRequest {
                    cue: EpisodeCue::new(text(raw, "cue")?)?,
                    filter: filter(raw)?,
                    limit: limit(raw)?,
                })
            }
            "get" => {
                fields(raw, action_fields(EpisodeAction::Get).0)?;
                let body = raw
                    .get("body")
                    .map(|v| v.as_bool().ok_or("episode body must be boolean"))
                    .transpose()?
                    .unwrap_or(false);
                if !body && (raw.get("offset").is_some() || raw.get("max_bytes").is_some()) {
                    return Err(invalid("episode offset/max_bytes require body:true"));
                }
                let offset = raw
                    .get("offset")
                    .map(|_| number(raw, "offset"))
                    .transpose()?
                    .unwrap_or(0);
                let max_bytes = raw
                    .get("max_bytes")
                    .map(|_| number(raw, "max_bytes"))
                    .transpose()?
                    .unwrap_or(MAX_EPISODE_BODY_BYTES as u64);
                if max_bytes == 0
                    || max_bytes > MAX_EPISODE_BODY_BYTES as u64
                    || offset > MAX_EPISODE_BODY_BYTES as u64
                {
                    return Err(invalid(
                        "episode body offset/limit exceeds 16 KiB (limit must be positive)",
                    ));
                }
                Request::Get {
                    request: EpisodeGet {
                        episode_id: root_id(raw, "episode_id")?,
                        edition_id: raw
                            .get("edition_id")
                            .map(|_| node_id(raw, "edition_id"))
                            .transpose()?,
                    },
                    body,
                    offset,
                    max_bytes: max_bytes as usize,
                }
            }
            "history" => {
                fields(raw, action_fields(EpisodeAction::History).0)?;
                Request::History(EpisodeHistoryRequest {
                    episode_id: root_id(raw, "episode_id")?,
                    limit: limit(raw)?,
                    after: raw
                        .get("after")
                        .map(|_| -> Result<_, EpisodeError> { Ok(text(raw, "after")?.parse()?) })
                        .transpose()?,
                })
            }
            "references" => {
                fields(raw, action_fields(EpisodeAction::References).0)?;
                Request::References(EpisodeReferencesRequest {
                    anchor: node_id(raw, "anchor")?,
                    limit: limit(raw)?,
                    after: raw
                        .get("after")
                        .map(|_| -> Result<_, EpisodeError> { Ok(text(raw, "after")?.parse()?) })
                        .transpose()?,
                })
            }
            _ => return Err(invalid("unknown episode action")),
        };
        let mut canonical = raw.clone();
        if let Request::Append(write) | Request::Revise { write, .. } = &request
            && let Some(contexts) = &write.occurrence_contexts
        {
            canonical["occurrence_contexts"] = json!(contexts);
        }
        let prepared = Self {
            raw: canonical,
            request,
        };
        prepared.validate_preflight()?;
        Ok(prepared)
    }
    fn validate_preflight(&self) -> Result<(), EpisodeError> {
        match &self.request {
            Request::Append(write) => {
                let tags = write
                    .input
                    .tags
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                let source = write.request(&tags, None)?.validated_source()?;
                write.validate_metadata(&source, EpisodeId::new(source.node_id()), None, None)?;
            }
            Request::Revise {
                root,
                expected,
                reason,
                write,
            } => {
                let tags = write
                    .input
                    .tags
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                // Any positive ordinal suffices for request-only validation.
                // The engine derives the real ordinal from the named predecessor.
                let source = write.request(&tags, None)?.validated_revision_source(
                    *root,
                    *expected,
                    EpisodeRevision::new(1),
                    reason,
                )?;
                write.validate_metadata(&source, *root, Some(*expected), Some(reason))?;
            }
            Request::List(request) => {
                request.validate()?;
                if let Some(cursor) = &request.after {
                    cursor.validate(cursor.database_id(), request)?;
                }
            }
            Request::Search(request) => request.validate()?,
            Request::History(request) => {
                if let Some(cursor) = &request.after {
                    cursor.validate(cursor.database_id(), request)?;
                }
            }
            Request::References(request) => {
                if let Some(cursor) = &request.after {
                    cursor.validate(cursor.database_id(), request)?;
                }
            }
            Request::Get { .. } => {}
        }
        Ok(())
    }
    pub fn action(&self) -> EpisodeAction {
        match self.request {
            Request::Append(_) => EpisodeAction::Append,
            Request::Revise { .. } => EpisodeAction::Revise,
            Request::List(_) => EpisodeAction::List,
            Request::Search(_) => EpisodeAction::Search,
            Request::Get { .. } => EpisodeAction::Get,
            Request::History(_) => EpisodeAction::History,
            Request::References(_) => EpisodeAction::References,
        }
    }
    pub fn capability(&self) -> EpisodeCapability {
        self.action().capability()
    }
    pub fn is_mutation(&self) -> bool {
        self.capability() != EpisodeCapability::ReadOnly
    }
    pub fn into_json(self) -> Value {
        self.raw
    }
}

fn action_fields(action: EpisodeAction) -> (&'static [&'static str], &'static [&'static str]) {
    match action {
        EpisodeAction::Append => (
            &[
                "action",
                "source",
                "summary",
                "body",
                "tags",
                "occurred",
                "thread",
                "occurrence_contexts",
                "links",
            ],
            &["action", "source", "summary"],
        ),
        EpisodeAction::Revise => (
            &[
                "action",
                "episode_id",
                "expected_edition_id",
                "reason",
                "source",
                "summary",
                "body",
                "tags",
                "occurred",
                "thread",
                "occurrence_contexts",
                "links",
            ],
            &[
                "action",
                "episode_id",
                "expected_edition_id",
                "reason",
                "source",
                "summary",
            ],
        ),
        EpisodeAction::List => (
            &[
                "action",
                "axis",
                "order",
                "from",
                "through",
                "thread",
                "occurrence",
                "limit",
                "after",
            ],
            &["action"],
        ),
        EpisodeAction::Search => (
            &["action", "cue", "thread", "occurrence", "limit"],
            &["action", "cue"],
        ),
        EpisodeAction::Get => (
            &[
                "action",
                "episode_id",
                "edition_id",
                "body",
                "offset",
                "max_bytes",
            ],
            &["action", "episode_id"],
        ),
        EpisodeAction::History => (
            &["action", "episode_id", "limit", "after"],
            &["action", "episode_id"],
        ),
        EpisodeAction::References => (
            &["action", "anchor", "limit", "after"],
            &["action", "anchor"],
        ),
    }
}

/// Shared schema with complete, closed action branches. Transports that add an
/// explicit database envelope must add it at the root and in each union branch.
pub fn input_schema(allowed_actions: &[EpisodeAction]) -> Value {
    let mut capture = crate::capture::properties();
    capture["tags"]["items"]["not"] = json!({"const":"core"});
    let time = json!({"type":"integer","minimum":0,"maximum":i64::MAX});
    let occurrence = json!({"oneOf":[
        {"type":"object","additionalProperties":false,"required":["kind"],"properties":{"kind":{"const":"unknown"}}},
        {"type":"object","additionalProperties":false,"required":["kind","at"],"properties":{"kind":{"const":"point"},"at":time}},
        {"type":"object","additionalProperties":false,"required":["kind","start","end"],"properties":{"kind":{"const":"range"},"start":time,"end":time}}
    ]});
    let filter = json!({"oneOf":[
        {"type":"object","additionalProperties":false,"required":["kind"],"properties":{"kind":{"const":"any"}}},
        {"type":"object","additionalProperties":false,"required":["kind"],"properties":{"kind":{"const":"unknown"}}},
        {"type":"object","additionalProperties":false,"required":["kind"],"anyOf":[{"required":["from"]},{"required":["through"]}],"properties":{"kind":{"const":"overlaps"},"from":time,"through":time}}
    ]});
    let id = json!({"type":"string","minLength":26,"maxLength":26,"description":"ULID","pattern":"^[0-7][0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{25}$"});
    let properties = json!({
        "action":{"type":"string","enum":allowed_actions.iter().map(|a|a.as_str()).collect::<Vec<_>>()},
        "source":capture["source"],"summary":{"type":"string","minLength":1,"maxLength":MAX_EPISODE_SUMMARY_BYTES,"description":"trimmed, control-free; maximum UTF-8 bytes"},
        "body":{"oneOf":[{"type":"string","maxLength":MAX_EPISODE_BODY_BYTES},{"type":"boolean"}]},
        "tags":capture["tags"],"links":capture["links"],"occurred":occurrence,
        "thread":{"type":"string","minLength":1,"maxLength":MAX_EPISODE_THREAD_BYTES},
        "occurrence_contexts":{"type":"array","minItems":1,
            "description":"Where this episode happened, independently of its recorder. Omission means unknown, including full replacement revisions. Exact case-sensitive opaque namespace/key pairs must be distinct; canonical order is namespace then key. The complete canonical compact JSON collection must fit 1024 UTF-8 bytes; no inferred contexts or truncated identifiers.",
            "items":{"type":"object","additionalProperties":false,"required":["namespace","key"],
                "properties":{
                    "namespace":{"type":"string","minLength":1,"maxLength":MAX_OCCURRENCE_CONTEXTS_JSON_BYTES,"description":"Nonblank, trimmed, control-free opaque text; case-sensitive, no registry or hierarchy."},
                    "key":{"type":"string","minLength":1,"maxLength":MAX_OCCURRENCE_CONTEXTS_JSON_BYTES,"description":"Nonblank, trimmed, control-free opaque text; case-sensitive."},
                    "label":{"type":"string","minLength":1,"maxLength":MAX_OCCURRENCE_CONTEXTS_JSON_BYTES,"description":"Optional nonblank, trimmed, control-free display label; not identity."}
                }}},
        "episode_id":id,"edition_id":id,"expected_edition_id":id,"anchor":id,
        "reason":{"type":"string","minLength":1,"maxLength":1024},
        "axis":{"type":"string","enum":["recorded","occurred"]},"order":{"type":"string","enum":["newest_first","oldest_first"]},
        "from":time,"through":time,"occurrence":filter,
        "limit":{"type":"integer","minimum":1,"maximum":MAX_EPISODE_PAGE_ITEMS,"default":DEFAULT_EPISODE_PAGE_ITEMS},
        "after":{"type":"string","minLength":1,"maxLength":1024,"description":"keyset continuation for this database and exact request; not a snapshot"},
        "cue":{"type":"string","minLength":1,"maxLength":MAX_EPISODE_CUE_BYTES,"description":"plain text lexical cue over current summaries; no implicit graph expansion"},
        "offset":{"type":"integer","minimum":0,"maximum":MAX_EPISODE_BODY_BYTES},
        "max_bytes":{"type":"integer","minimum":1,"maximum":MAX_EPISODE_BODY_BYTES}
    });
    // Each union arm is also a complete discovery surface. Sparse refinements
    // are valid JSON Schema, but tool hosts that render oneOf arms independently
    // otherwise lose source/summary (and render conditional-only arms unknown).
    let variants = allowed_actions.iter().map(|action| {
        let (fields, required) = action_fields(*action);
        let fields = fields.iter().map(|name| ((*name).to_owned(), properties[*name].clone()))
            .collect::<serde_json::Map<_, _>>();
        let mut variant = json!({"type":"object", "additionalProperties":false,
            "required":required, "properties":fields});
        variant["properties"]["action"] = json!({"type":"string", "const":action.as_str()});
        if matches!(action, EpisodeAction::Append | EpisodeAction::Revise) {
            variant["properties"]["body"] = json!({"type":"string", "maxLength":MAX_EPISODE_BODY_BYTES});
        }
        if *action == EpisodeAction::List {
            variant["not"] = json!({"required":["axis","occurrence"],"properties":{
                "axis":{"const":"occurred"},"occurrence":{"required":["kind"],"properties":{"kind":{"const":"unknown"}}}}});
        }
        if *action == EpisodeAction::Get {
            variant["properties"]["body"] = json!({"type":"boolean"});
            variant["if"] = json!({"anyOf":[{"required":["offset"]},{"required":["max_bytes"]}]});
            variant["then"] = json!({"required":["body"],"properties":{"body":{"const":true}}});
        }
        variant
    }).collect::<Vec<_>>();
    json!({"type":"object","additionalProperties":false,"description":"Selective immutable episode editions. Authoring metadata must fit 16 KiB encoded JSON in aggregate (individual bounds also apply), reserving body progress inside each 32 KiB response. Cursors are keysets, not snapshots.","required":["action"],"properties":properties,"oneOf":variants})
}

fn checked_response(value: Value) -> Result<Value, EpisodeError> {
    if serde_json::to_vec(&value)?.len() > MAX_EPISODE_RESPONSE_BYTES {
        return Err(invalid("episode response exceeds encoded 32 KiB budget"));
    }
    Ok(value)
}
fn occurrence_json(span: &OccurrenceSpan) -> Value {
    match span {
        OccurrenceSpan::Unknown => json!({"kind":"unknown"}),
        OccurrenceSpan::Point { at } => json!({"kind":"point","at":at}),
        OccurrenceSpan::Range { start, end } => json!({"kind":"range","start":start,"end":end}),
    }
}
/// A compact header never contains body or duplicated provenance.
pub fn header_json(header: &EpisodeHeader) -> Value {
    let mut value = json!({"episode_id":header.identity.episode_id,"edition_id":header.identity.edition_id,"revision":header.identity.revision,"summary":header.summary,"occurred":occurrence_json(&header.occurred),"recorded_at":header.recorded_at,"edition_recorded_at":header.edition_recorded_at,"thread":header.thread,"current_edition_id":header.current_edition_id});
    if let Some(contexts) = &header.occurrence_contexts {
        value["occurrence_contexts"] = json!(contexts);
    }
    value
}
fn source_json(source: &CaptureSource) -> Value {
    let digest: String = source
        .request_digest()
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect();
    json!({"namespace":source.namespace(),"key":source.key(),"reference":source.reference(),"session":source.session(),"revision":source.revision(),"request_digest_sha256":digest,"request_codec":source.request_codec()})
}
fn record_json(record: &EpisodeRecord) -> Result<Value, EpisodeError> {
    let facet = record
        .node
        .episode()
        .ok_or("episode store returned semantic node")?;
    let header = EpisodeHeader::from_node(&record.node, record.current_edition_id)?;
    let source = match record.node.provenance() {
        Provenance::External { source } => source,
        _ => return Err(invalid("episode record lacks external provenance")),
    };
    Ok(details_json(
        &header,
        record.node.tags().collect(),
        source,
        facet.edit_reason(),
        facet.revises(),
        record.node.id() == record.current_edition_id,
    ))
}

fn details_json(
    header: &EpisodeHeader,
    tags: Vec<&str>,
    source: &CaptureSource,
    reason: Option<&EpisodeRevisionReason>,
    revises: Option<NodeId>,
    is_current: bool,
) -> Value {
    let mut value = header_json(header);
    value["action"] = json!("get");
    value["tags"] = json!(tags);
    value["edit_reason"] = json!(reason);
    value["revises"] = json!(revises);
    value["is_current"] = json!(is_current);
    value["source"] = source_json(source);
    value
}

fn timeline_key(
    header: &EpisodeHeader,
    axis: EpisodeTimelineAxis,
) -> Result<EpisodeTime, EpisodeError> {
    match axis {
        EpisodeTimelineAxis::Recorded => Ok(header.recorded_at),
        EpisodeTimelineAxis::Occurred => match header.occurred {
            OccurrenceSpan::Point { at } => Ok(at),
            OccurrenceSpan::Range { start, .. } => Ok(start),
            OccurrenceSpan::Unknown => {
                Err(invalid("occurred timeline returned unknown occurrence"))
            }
        },
    }
}

impl PreparedWrite {
    fn validate_metadata(
        &self,
        source: &CaptureSource,
        root: EpisodeId,
        revises: Option<NodeId>,
        reason: Option<&EpisodeRevisionReason>,
    ) -> Result<(), EpisodeError> {
        // Size the real encoded presentation, with maximum-width generated
        // values. Byte sums miss quote escaping and can leave a legal body
        // requiring dozens of tiny reads after an already-committed write.
        let maximum_time = EpisodeTime::new(i64::MAX as u128)?;
        let header = EpisodeHeader {
            identity: EpisodeIdentity {
                episode_id: root,
                edition_id: source.node_id(),
                revision: EpisodeRevision::new(u32::MAX),
            },
            summary: mneme_core::NodeSummary::new(&self.input.summary)?,
            occurred: self.occurrence.clone(),
            recorded_at: maximum_time,
            edition_recorded_at: maximum_time,
            thread: self.thread.clone(),
            occurrence_contexts: self.occurrence_contexts.clone(),
            recording_session: source
                .session()
                .map(EpisodeRecordingSession::new)
                .transpose()?,
            current_edition_id: source.node_id(),
        };
        let tags = self
            .input
            .tags
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(String::as_str)
            .collect();
        let mut value = details_json(&header, tags, source, reason, revises, false);
        value["body"] = json!("");
        value["body_range"] = json!({"source_start":MAX_EPISODE_BODY_BYTES,"source_end":MAX_EPISODE_BODY_BYTES,"next_offset":MAX_EPISODE_BODY_BYTES,"has_more":false});
        if serde_json::to_vec(&value)?.len() > MAX_EPISODE_METADATA_BYTES {
            return Err(invalid(
                "episode combined encoded metadata exceeds 16 KiB; shorten summary, tags, source or editorial reason to reserve bounded body readback",
            ));
        }
        Ok(())
    }
    fn body(&self) -> &[u8] {
        self.input
            .body
            .as_deref()
            .unwrap_or(&self.input.summary)
            .as_bytes()
    }
    fn request<'a>(
        &'a self,
        tags: &'a [&'a str],
        origin_commit: Option<&str>,
    ) -> Result<EpisodeWrite<'a>, EpisodeError> {
        let mut request = EpisodeWrite::new(
            &self.input.source.namespace,
            &self.input.source.key,
            &self.input.source.reference,
            self.input.source.session.as_deref(),
            self.input.source.revision.as_deref(),
            &self.input.summary,
            self.body(),
            tags,
            self.occurrence.clone(),
            self.thread.clone(),
        )
        .with_links(&self.links)
        .with_origin_commit(origin_commit)?;
        if let Some(contexts) = &self.occurrence_contexts {
            request = request.with_occurrence_contexts(contexts.clone());
        }
        Ok(request)
    }
}

impl PreparedEpisode {
    pub async fn run(
        self,
        mem: &Memory,
        db_id: Ulid,
        origin_commit: Option<&str>,
    ) -> Result<Value, EpisodeError> {
        let action = self.action().as_str();
        let value = match self.request {
            Request::Append(write) => {
                let tags = write
                    .input
                    .tags
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                let result = mem
                    .append_episode(write.request(&tags, origin_commit)?)
                    .await?;
                json!({"action":action,"episode_id":result.identity.episode_id,"edition_id":result.identity.edition_id,"revision":result.identity.revision,"replayed":result.replayed})
            }
            Request::Revise {
                root,
                expected,
                reason,
                write,
            } => {
                let tags = write
                    .input
                    .tags
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                let result = mem
                    .revise_episode(root, expected, reason, write.request(&tags, origin_commit)?)
                    .await?;
                json!({"action":action,"episode_id":result.identity.episode_id,"edition_id":result.identity.edition_id,"revision":result.identity.revision,"replayed":result.replayed})
            }
            Request::List(request) => {
                let page = mem.episode_timeline(&request).await?;
                pack_page(
                    action,
                    &page.items,
                    page.next.as_ref().map(ToString::to_string),
                    page.partial,
                    header_json,
                    |last| {
                        Ok(EpisodeTimelineCursor::new(
                            db_id,
                            &request,
                            timeline_key(last, request.axis)?,
                            last.identity.episode_id,
                        )
                        .to_string())
                    },
                )?
            }
            Request::Search(request) => {
                let page = mem.episode_cue(&request).await?;
                let mut items = page.items.iter().map(header_json).collect::<Vec<_>>();
                let mut has_more = page.has_more;
                loop {
                    let value = json!({"action":action,"mode":"lexical","items":items,"has_more":has_more,"partial":page.partial});
                    if serde_json::to_vec(&value)?.len() <= MAX_EPISODE_RESPONSE_BYTES {
                        break value;
                    }
                    if items.len() <= 1 {
                        return Err(invalid("episode search header exceeds response budget"));
                    }
                    items.pop();
                    has_more = true;
                }
            }
            Request::History(request) => {
                let page = mem.episode_history(&request).await?;
                pack_page(
                    action,
                    &page.items,
                    page.next.as_ref().map(ToString::to_string),
                    page.partial,
                    header_json,
                    |last| {
                        Ok(EpisodeHistoryCursor::new(
                            db_id,
                            &request,
                            last.identity.revision,
                            last.identity.edition_id,
                        )
                        .to_string())
                    },
                )?
            }
            Request::References(request) => {
                let page = mem.episode_references(&request).await?;
                pack_page(
                    action,
                    &page.items,
                    page.next.as_ref().map(ToString::to_string),
                    false,
                    |item| json!({"edge":item.edge,"from_episode":item.from_episode,"to_episode":item.to_episode}),
                    |last| {
                        Ok(EpisodeReferencesCursor::new(
                            db_id,
                            &request,
                            last.edge.from,
                            last.edge.to,
                        )
                        .to_string())
                    },
                )?
            }
            Request::Get {
                request,
                body,
                offset,
                max_bytes,
            } => {
                let record = mem
                    .get_episode(&request)
                    .await?
                    .ok_or("episode not found")?;
                let mut value = record_json(&record)?;
                let snapshot =
                    crate::touchstone::node_touchstone_projection(mem, db_id, &record.node).await?;
                value
                    .as_object_mut()
                    .expect("episode get details are an object")
                    .extend(
                        snapshot
                            .as_object()
                            .expect("touchstone projection is an object")
                            .clone(),
                    );
                value["action"] = json!(action);
                if body {
                    let chunk = mem
                        .resolve_body_range(&record.node, offset, max_bytes)
                        .await?;
                    pack_body(&mut value, &chunk)?;
                }
                value
            }
        };
        checked_response(value)
    }
}

fn pack_page<T>(
    action: &str,
    items: &[T],
    next: Option<String>,
    partial: bool,
    render: impl Fn(&T) -> Value,
    continuation: impl Fn(&T) -> Result<String, EpisodeError>,
) -> Result<Value, EpisodeError> {
    let mut rendered = items.iter().map(render).collect::<Vec<_>>();
    let mut next = next;
    loop {
        let value = json!({"action":action,"items":rendered,"next":next,"partial":partial});
        if serde_json::to_vec(&value)?.len() <= MAX_EPISODE_RESPONSE_BYTES {
            return Ok(value);
        }
        if rendered.len() <= 1 {
            return Err(invalid("episode page item exceeds encoded response budget"));
        }
        rendered.pop();
        // Keyset starts after exactly the last delivered row, not backend lookahead.
        next = Some(continuation(&items[rendered.len() - 1])?);
    }
}

fn pack_body(value: &mut Value, chunk: &mneme_core::ports::BodyChunk) -> Result<(), EpisodeError> {
    // Episode authors supply UTF-8. A caller can still choose a byte offset in
    // the middle of a codepoint; refuse that offset, never silently corrupt it.
    let bytes = &chunk.bytes;
    let valid = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()])?
        }
        Err(_) => return Err(invalid("episode body offset must be a UTF-8 boundary")),
    };
    let boundaries = valid
        .char_indices()
        .map(|(at, _)| at)
        .chain(std::iter::once(valid.len()))
        .collect::<Vec<_>>();
    let mut low = 0;
    let mut high = boundaries.len();
    while low < high {
        let mid = (low + high) / 2;
        set_body(
            value,
            &valid[..boundaries[mid]],
            chunk.source_start,
            bytes.len(),
            chunk.next_offset,
        );
        if serde_json::to_vec(value)?.len() <= MAX_EPISODE_RESPONSE_BYTES {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    if low == 0 {
        return Err(invalid(
            "episode details exceed response budget before body",
        ));
    }
    let end = boundaries[low - 1];
    if end == 0 && !bytes.is_empty() {
        return Err(invalid(
            "episode body max_bytes cannot fit the next UTF-8 character",
        ));
    }
    set_body(
        value,
        &valid[..end],
        chunk.source_start,
        bytes.len(),
        chunk.next_offset,
    );
    Ok(())
}
fn set_body(value: &mut Value, text: &str, start: u64, fetched: usize, backend_next: Option<u64>) {
    let end = start + text.len() as u64;
    let has_more = text.len() < fetched || backend_next.is_some();
    value["body"] = json!(text);
    value["body_range"] = json!({"source_start":start,"source_end":end,"next_offset":if has_more{Some(end)}else{None},"has_more":has_more});
}

pub fn render_human(value: &Value) -> String {
    if let Some(items) = value.get("items").and_then(Value::as_array) {
        let mut lines = items
            .iter()
            .map(|item| {
                let mut line = if let Some(edge) = item.get("edge") {
                    format!(
                        "{} → {}  {}",
                        edge["from"].as_str().unwrap_or("?"),
                        edge["to"].as_str().unwrap_or("?"),
                        edge["kind"]
                    )
                } else if value["action"] == "history" {
                    format!(
                        "r{}  {}  {}",
                        item["revision"],
                        item["edition_id"].as_str().unwrap_or("?"),
                        item["summary"].as_str().unwrap_or("")
                    )
                } else {
                    format!(
                        "{}  recorded:{}  {}",
                        item["episode_id"].as_str().unwrap_or("?"),
                        item["recorded_at"],
                        item["summary"].as_str().unwrap_or("")
                    )
                };
                if let Some(contexts) = item.get("occurrence_contexts") {
                    line.push_str(&format!("  occurrence_contexts: {contexts}"));
                }
                line
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            lines.push("No episodes.".into());
        }
        if value["mode"] == "lexical" {
            lines.insert(0, "Lexical matches".into());
        }
        if let Some(next) = value.get("next").and_then(Value::as_str) {
            lines.push(format!("next: {next}"));
        }
        if value.get("has_more") == Some(&json!(true)) {
            lines.push("More lexical matches; narrow the cue.".into());
        }
        if value.get("partial") == Some(&json!(true)) {
            lines.push("Partial page (bounded read).".into());
        }
        lines.join("\n")
    } else if let Some(summary) = value.get("summary").and_then(Value::as_str) {
        let mut result = format!(
            "{}  edition {} (revision {})\n{summary}",
            value["episode_id"].as_str().unwrap_or("?"),
            value["edition_id"].as_str().unwrap_or("?"),
            value["revision"]
        );
        if let Some(contexts) = value.get("occurrence_contexts") {
            result.push_str(&format!("\noccurrence_contexts: {contexts}"));
        }
        if let Some(body) = value.get("body").and_then(Value::as_str) {
            result.push_str("\n\n");
            result.push_str(body);
        }
        if let Some(next) = value
            .pointer("/body_range/next_offset")
            .and_then(Value::as_u64)
        {
            result.push_str(&format!("\n[body continues at byte {next}]"));
        }
        result
    } else {
        format!(
            "{}  edition {} (revision {}){}",
            value["episode_id"].as_str().unwrap_or("?"),
            value["edition_id"].as_str().unwrap_or("?"),
            value["revision"],
            if value["replayed"] == true {
                " (replayed)"
            } else {
                ""
            }
        )
    }
}

impl PreparedEpisode {
    /// Stable source-key identity is available without reading a predecessor.
    pub fn expected_edition_id(&self) -> Result<Option<NodeId>, EpisodeError> {
        let write = match &self.request {
            Request::Append(write) | Request::Revise { write, .. } => write,
            _ => return Ok(None),
        };
        Ok(Some(
            CaptureSource::new(
                &write.input.source.namespace,
                &write.input.source.key,
                &write.input.source.reference,
                write.input.source.session.as_deref(),
                write.input.source.revision.as_deref(),
                [0; 32],
            )?
            .node_id(),
        ))
    }
    pub fn expected_body(&self) -> Option<&[u8]> {
        match &self.request {
            Request::Append(write) | Request::Revise { write, .. } => Some(write.body()),
            _ => None,
        }
    }
    /// Check receipt fields before a native client uses them for exact readback.
    pub fn verify_write_receipt_json(&self, receipt: &Value) -> Result<(), EpisodeError> {
        let edition = self
            .expected_edition_id()?
            .ok_or("episode read has no write receipt")?;
        let revision = number(receipt, "revision")?;
        if revision > u32::MAX as u64
            || receipt["action"] != self.action().as_str()
            || receipt["edition_id"] != json!(edition)
            || !receipt["replayed"].is_boolean()
        {
            return Err(invalid(
                "episode write receipt identity/action/revision mismatch",
            ));
        }
        let root = match &self.request {
            Request::Append(_) => {
                if revision != 0 {
                    return Err(invalid("episode append receipt revision must be zero"));
                }
                EpisodeId::new(edition)
            }
            Request::Revise { root, .. } => {
                if revision == 0 {
                    return Err(invalid(
                        "episode editorial receipt revision must be positive",
                    ));
                }
                *root
            }
            _ => unreachable!(),
        };
        if receipt["episode_id"] != json!(root) {
            return Err(invalid("episode write receipt root mismatch"));
        }
        Ok(())
    }
    /// The native client may assemble several bounded `get` body chunks first.
    /// Its synthesized full range must then cover exactly this authored body.
    pub fn verify_readback_json(
        &self,
        receipt: &Value,
        readback: &Value,
    ) -> Result<(), EpisodeError> {
        self.verify_write_receipt_json(receipt)?;
        let revision = EpisodeRevision::new(number(receipt, "revision")? as u32);
        let (write, expected_source, revises, reason) = match &self.request {
            Request::Append(write) => {
                let tags = write
                    .input
                    .tags
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                (
                    write,
                    write.request(&tags, None)?.validated_source()?,
                    None,
                    None,
                )
            }
            Request::Revise {
                root,
                expected,
                reason,
                write,
            } => {
                let tags = write
                    .input
                    .tags
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                (
                    write,
                    write
                        .request(&tags, None)?
                        .validated_revision_source(*root, *expected, revision, reason)?,
                    Some(*expected),
                    Some(reason),
                )
            }
            _ => return Err(invalid("episode read has no write readback proof")),
        };
        let mut expected_tags = write
            .input
            .tags
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        expected_tags.sort_unstable();
        expected_tags.dedup();
        let mut actual_tags = readback["tags"]
            .as_array()
            .ok_or("episode readback lacks tags")?
            .iter()
            .map(|v| v.as_str().ok_or("episode readback invalid tag"))
            .collect::<Result<Vec<_>, _>>()?;
        actual_tags.sort_unstable();
        let valid = readback["action"] == "get"
            && readback["episode_id"] == receipt["episode_id"]
            && readback["edition_id"] == receipt["edition_id"]
            && readback["revision"] == receipt["revision"]
            && readback["summary"] == write.input.summary
            && readback["occurred"] == occurrence_json(&write.occurrence)
            && readback["thread"] == json!(write.thread)
            && readback.get("occurrence_contexts")
                == write
                    .occurrence_contexts
                    .as_ref()
                    .map(|contexts| json!(contexts))
                    .as_ref()
            && readback["revises"] == json!(revises)
            && readback["edit_reason"] == json!(reason)
            && readback["source"] == source_json(&expected_source)
            && actual_tags == expected_tags;
        if !valid {
            return Err(invalid(
                "episode readback authored fields, identity or source proof mismatch",
            ));
        }
        let recorded = time(readback, "recorded_at")?;
        let edited = time(readback, "edition_recorded_at")?;
        let current = node_id(readback, "current_edition_id")?;
        if readback["is_current"] != json!(current == expected_source.node_id())
            || revision == EpisodeRevision::INITIAL && recorded != edited
        {
            return Err(invalid("episode readback metadata mismatch"));
        }
        let body = write.body();
        let range = &readback["body_range"];
        if readback["body"] != std::str::from_utf8(body)?
            || range["source_start"] != 0
            || range["source_end"] != body.len()
            || range.get("next_offset") != Some(&Value::Null)
            || range["has_more"] != false
        {
            return Err(invalid(
                "episode readback body is missing, truncated or changed",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "episode_tests.rs"]
mod tests;
