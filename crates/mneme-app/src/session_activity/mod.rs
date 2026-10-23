//! Body-free, prospective session observations. This module never opens memory
//! owners: a report is a bounded read of the separately configured local sidecar.

pub mod local;
pub use local::PreparedActivity;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MANIFEST_SCHEMA: &str = "mneme.session-activity.config.v1";
pub const RECORD_SCHEMA: &str = "mneme.session-activity.v1";
pub const REPORT_SCHEMA: &str = "mneme.session-activity.report.v1";
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_READ_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_DIRECTORY_ENTRIES: usize = 4096;
pub const MAX_OUTPUT_BYTES: usize = 48 * 1024;
pub const MAX_SESSION_ID_BYTES: usize = 256;
pub const MAX_SESSION_SELECTOR_BYTES: usize = 289;
pub const MAX_JSON_DEPTH: usize = 12;

#[derive(Clone, Debug, Default)]
pub struct ActivityRequest {
    pub session: Option<String>,
    pub details: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityStatus {
    Ready,
    NotConfigured,
    InvalidManifest,
    Unavailable,
    WorkspaceUnbound,
    WorkspaceBoundary,
    NoObservations,
    SessionNotFound,
    SessionUnavailable,
    AmbiguousSession,
    FullIdRequired,
    Cancelled,
}

/// Stable messages deliberately exclude paths, JSON input and OS error text.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct ActivityError {
    code: &'static str,
    message: &'static str,
}

impl ActivityError {
    pub fn code(&self) -> &'static str {
        self.code
    }
    fn invalid_selector() -> Self {
        Self {
            code: "invalid_session_selector",
            message: "Session must be a bounded full ID or prefix, optionally source-qualified.",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: String,
    pub root: PathBuf,
    pub workspaces: Vec<WorkspaceBinding>,
    pub device: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceBinding {
    pub path: PathBuf,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecord {
    pub schema: String,
    pub source: String,
    pub session_id: String,
    pub workspace: String,
    pub cwd: String,
    #[serde(deserialize_with = "required_option")]
    pub device: Option<String>,
    pub first_observed: u64,
    pub last_observed: u64,
    pub events: Vec<ActivityEvent>,
    pub evicted_events: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Lifecycle,
    Core,
    RecallAdmission,
    RecallSelection,
    DeliveryConsumed,
    HookOutput,
    HostAdmission,
    Recording,
    Checkpoint,
}
impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lifecycle => "lifecycle",
            Self::Core => "core",
            Self::RecallAdmission => "recall_admission",
            Self::RecallSelection => "recall_selection",
            Self::DeliveryConsumed => "delivery_consumed",
            Self::HookOutput => "hook_output",
            Self::HostAdmission => "host_admission",
            Self::Recording => "recording",
            Self::Checkpoint => "checkpoint",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityEvent {
    pub key: String,
    pub at: u64,
    pub kind: EventKind,
    pub outcome: String,
    #[serde(deserialize_with = "required_option")]
    pub reason: Option<String>,
    pub origin: EventOrigin,
    #[serde(deserialize_with = "required_option")]
    pub owner: Option<EventOwner>,
    pub nodes: Vec<String>,
    #[serde(deserialize_with = "required_option")]
    pub count: Option<u64>,
    pub omitted: u64,
    #[serde(deserialize_with = "required_option")]
    pub selection_digest: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Generation {
    Number(u64),
    Id(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventOrigin {
    #[serde(deserialize_with = "required_option")]
    pub turn: Option<String>,
    #[serde(deserialize_with = "required_option")]
    pub generation: Option<Generation>,
    #[serde(deserialize_with = "required_option")]
    pub context_generation: Option<Generation>,
    #[serde(deserialize_with = "required_option")]
    pub attempt: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventOwner {
    pub name: String,
    pub id: String,
    pub db: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActivityScope {
    pub workspace: String,
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActivityReport {
    pub schema: &'static str,
    pub status: ActivityStatus,
    pub scope: Option<ActivityScope>,
    pub sessions: Vec<SessionActivity>,
    pub coverage: Coverage,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionActivity {
    pub source: String,
    pub session_id: String,
    /// Always the full source-qualified selector, including incomplete catalogs.
    pub selector: String,
    pub display_id: String,
    pub workspace: String,
    pub cwd: String,
    pub device: Option<String>,
    pub first_observed: u64,
    pub last_observed: u64,
    pub retained_events: u64,
    pub evicted_events: u64,
    pub omitted_nodes: u64,
    pub summary: Vec<LaneSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<ActivityEvent>>,
    pub details_omitted: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LaneSummary {
    pub kind: EventKind,
    pub observed_events: u64,
    pub outcomes: BTreeMap<String, u64>,
    /// Conflicting summary fields at `last_at` leave all latest-value fields
    /// null. Outcome counts still include every retained observation.
    pub latest_status: LatestObservationStatus,
    pub last_owner: Option<EventOwner>,
    pub last_outcome: Option<String>,
    pub last_reason: Option<String>,
    pub last_at: u64,
    pub last_count: Option<u64>,
}

/// Whole-second occurrence times cannot order conflicting observations in the
/// same second. Arrival order is deliberately not used as a tie breaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LatestObservationStatus {
    Observed,
    AmbiguousTimestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanKind {
    NotStarted,
    Direct,
    Catalog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageIssue {
    Missing,
    Unreadable,
    Malformed,
    Unsupported,
    Oversize,
    UnsafePath,
    ScopeMismatch,
    IdentityMismatch,
    UnknownEntry,
    DirectoryLimit,
    ReadLimit,
    OutputLimit,
    Cancelled,
    PlatformUnsupported,
}

#[derive(Clone, Debug, Serialize)]
pub struct Coverage {
    pub scan: ScanKind,
    pub complete_catalog: bool,
    pub partial: bool,
    pub inspected_entries: usize,
    pub read_bytes: usize,
    /// Bytes charged, including partial failed reads; manifest allowance is reserved.
    pub read_budget_used: usize,
    pub verified_records: usize,
    pub issues: BTreeMap<CoverageIssue, usize>,
    pub omitted_sessions: usize,
    /// These are not inferred to be zero from absent prospective observations.
    pub not_tracked: [&'static str; 4],
    pub limitations: [&'static str; 4],
}
impl Default for Coverage {
    fn default() -> Self {
        Self {
            scan: ScanKind::NotStarted,
            complete_catalog: false,
            partial: false,
            inspected_entries: 0,
            read_bytes: 0,
            read_budget_used: 0,
            verified_records: 0,
            issues: BTreeMap::new(),
            omitted_sessions: 0,
            not_tracked: [
                "explicit_cli_mcp_operations",
                "claude_capture",
                "other_hosts",
                "remote_devices",
            ],
            limitations: [
                "prospective_best_effort_observations_only",
                "no_observation_is_not_no_activity",
                "no_model_attention_or_usefulness_proof",
                "atomic_records_not_cross_session_snapshot",
            ],
        }
    }
}
impl Coverage {
    fn issue(&mut self, issue: CoverageIssue) {
        *self.issues.entry(issue).or_default() += 1;
        self.partial = true;
    }
}

pub fn default_manifest_path() -> Result<PathBuf, ActivityError> {
    let config = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty());
    #[cfg(windows)]
    let config = config.or_else(|| std::env::var_os("APPDATA").filter(|s| !s.is_empty()));
    let base = config
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                #[cfg(windows)]
                {
                    PathBuf::from(home).join("AppData/Roaming").into_os_string()
                }
                #[cfg(not(windows))]
                {
                    PathBuf::from(home).join(".config").into_os_string()
                }
            })
        })
        .ok_or(ActivityError {
            code: "config_directory_unavailable",
            message: "No absolute platform configuration directory is available.",
        })?;
    let base = PathBuf::from(base);
    if !base.is_absolute() {
        return Err(ActivityError {
            code: "config_directory_invalid",
            message: "The platform configuration directory must be absolute.",
        });
    }
    Ok(base.join("mneme/activity.json"))
}

/// No owner discovery, graph access, subprocess, locking or writes occur here.
pub fn report(
    manifest: &Path,
    cwd: &Path,
    request: ActivityRequest,
) -> Result<ActivityReport, ActivityError> {
    report_with_cancel(manifest, cwd, request, &AtomicBool::new(false))
}

/// Cancellation is checked between bounded filesystem/decoding/projection steps.
pub fn report_with_cancel(
    manifest: &Path,
    cwd: &Path,
    request: ActivityRequest,
    cancelled: &AtomicBool,
) -> Result<ActivityReport, ActivityError> {
    local::report(manifest, cwd, request, cancelled)
}

fn cancelled(flag: &AtomicBool) -> bool {
    flag.load(Ordering::Relaxed)
}

pub fn record_filename(source: &str, session_id: &str) -> String {
    format!("{source}-{:x}.json", Sha256::digest(session_id.as_bytes()))
}

// Optional wire values are explicit nulls, not optional schema fields.
fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn bounded_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn machine_code(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}
fn identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-.:".contains(&c))
}
fn generation_valid(value: &Option<Generation>) -> bool {
    match value {
        None | Some(Generation::Number(_)) => true,
        Some(Generation::Id(s)) => identifier(s, 128),
    }
}
fn absolute_path(value: &str) -> bool {
    bounded_text(value, 4096)
        && Path::new(value).is_absolute()
        && !Path::new(value)
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
}

/// Decodes only after the input byte/depth envelope is admitted. Unknown fields
/// fail closed, so a body/query/provider receipt cannot ride along in a report.
pub fn decode_record(bytes: &[u8]) -> Result<SessionRecord, CoverageIssue> {
    decode_json(bytes, MAX_RECORD_BYTES, RECORD_SCHEMA)?;
    // Decode the original bytes so duplicate fields are not erased by Value.
    let record: SessionRecord =
        serde_json::from_slice(bytes).map_err(|_| CoverageIssue::Malformed)?;
    if !machine_code(&record.source, 32)
        || !bounded_text(&record.session_id, MAX_SESSION_ID_BYTES)
        || !absolute_path(&record.workspace)
        || !absolute_path(&record.cwd)
        || !record
            .device
            .as_deref()
            .is_none_or(|s| bounded_text(s, 128))
        || record.first_observed > record.last_observed
        || record.events.len() > MAX_RECORD_BYTES / 128
    {
        return Err(CoverageIssue::Malformed);
    }
    let mut keys = std::collections::BTreeSet::new();
    for event in &record.events {
        if !bounded_text(&event.key, 256)
            || !keys.insert(&event.key)
            || event.at < record.first_observed
            || event.at > record.last_observed
            || !machine_code(&event.outcome, 64)
            || !event.reason.as_deref().is_none_or(|s| machine_code(s, 64))
            || !event
                .origin
                .turn
                .as_deref()
                .is_none_or(|s| bounded_text(s, 256))
            || !event
                .origin
                .attempt
                .as_deref()
                .is_none_or(|s| bounded_text(s, 256))
            || !generation_valid(&event.origin.generation)
            || !generation_valid(&event.origin.context_generation)
            || event.nodes.len() > 256
            || event.nodes.iter().any(|id| !identifier(id, 128))
            || (!event.nodes.is_empty() && event.owner.is_none())
            || !event.selection_digest.as_deref().is_none_or(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            })
            || !event.owner.as_ref().is_none_or(|o| {
                identifier(&o.name, 128) && identifier(&o.id, 128) && identifier(&o.db, 128)
            })
        {
            return Err(CoverageIssue::Malformed);
        }
    }
    Ok(record)
}

fn decode_json(bytes: &[u8], max: usize, schema: &str) -> Result<serde_json::Value, CoverageIssue> {
    if bytes.len() > max {
        return Err(CoverageIssue::Oversize);
    }
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for &byte in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > MAX_JSON_DEPTH {
                        return Err(CoverageIssue::Malformed);
                    }
                }
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| CoverageIssue::Malformed)?;
    match value.get("schema").and_then(|v| v.as_str()) {
        Some(got) if got == schema => Ok(value),
        Some(_) => Err(CoverageIssue::Unsupported),
        None => Err(CoverageIssue::Malformed),
    }
}

fn project(record: SessionRecord, details: bool) -> SessionActivity {
    let mut lanes: BTreeMap<EventKind, LaneSummary> = BTreeMap::new();
    for event in &record.events {
        let lane = lanes.entry(event.kind).or_insert_with(|| LaneSummary {
            kind: event.kind,
            observed_events: 0,
            outcomes: BTreeMap::new(),
            latest_status: LatestObservationStatus::Observed,
            last_owner: event.owner.clone(),
            last_outcome: Some(event.outcome.clone()),
            last_reason: event.reason.clone(),
            last_at: event.at,
            last_count: event.count,
        });
        lane.observed_events += 1;
        *lane.outcomes.entry(event.outcome.clone()).or_default() += 1;
        if event.at > lane.last_at {
            lane.latest_status = LatestObservationStatus::Observed;
            lane.last_outcome = Some(event.outcome.clone());
            lane.last_reason.clone_from(&event.reason);
            lane.last_owner.clone_from(&event.owner);
            lane.last_at = event.at;
            lane.last_count = event.count;
        } else if event.at == lane.last_at
            && lane.latest_status == LatestObservationStatus::Observed
            && (lane.last_outcome.as_deref() != Some(event.outcome.as_str())
                || lane.last_reason != event.reason
                || lane.last_owner != event.owner
                || lane.last_count != event.count)
        {
            // Record order is publication order, not operation order. Once
            // conflicting latest-second summaries exist, only a strictly later
            // occurrence can establish an unambiguous latest summary again.
            lane.latest_status = LatestObservationStatus::AmbiguousTimestamp;
            lane.last_outcome = None;
            lane.last_reason = None;
            lane.last_owner = None;
            lane.last_count = None;
        }
    }
    let retained_events = record.events.len() as u64;
    let omitted_nodes = record
        .events
        .iter()
        .fold(0u64, |sum, e| sum.saturating_add(e.omitted));
    SessionActivity {
        selector: format!("{}:{}", record.source, record.session_id),
        display_id: record.session_id.clone(),
        source: record.source,
        session_id: record.session_id,
        workspace: record.workspace,
        cwd: record.cwd,
        device: record.device,
        first_observed: record.first_observed,
        last_observed: record.last_observed,
        retained_events,
        evicted_events: record.evicted_events,
        omitted_nodes,
        summary: lanes.into_values().collect(),
        details: details.then_some(record.events),
        details_omitted: 0,
    }
}

fn bound_output(report: &mut ActivityReport, cancel: &AtomicBool) {
    // Bound both representations from the same projection. Geometric detail
    // removal avoids one full reserialization per observation (quadratic work).
    loop {
        if cancelled(cancel) {
            report.status = ActivityStatus::Cancelled;
            if !report
                .coverage
                .issues
                .contains_key(&CoverageIssue::Cancelled)
            {
                report.coverage.issue(CoverageIssue::Cancelled);
            }
            // Cancellation does not bypass the output bound. Retain at most one
            // compact session and explicitly account for discarded observations.
            for session in &mut report.sessions {
                if let Some(details) = session.details.take() {
                    session.details_omitted += details.len() as u64;
                }
            }
            report.coverage.omitted_sessions += report.sessions.len().saturating_sub(1);
            report.sessions.truncate(1);
        }
        let json_size = serde_json::to_vec(&report).map_or(usize::MAX, |b| b.len());
        let human_size = render_human_unbounded(report).len();
        if json_size < MAX_OUTPUT_BYTES && human_size < MAX_OUTPUT_BYTES {
            break;
        }
        if !report
            .coverage
            .issues
            .contains_key(&CoverageIssue::OutputLimit)
        {
            report.coverage.issue(CoverageIssue::OutputLimit);
        }
        let mut removed_details = false;
        for session in report.sessions.iter_mut().rev() {
            if let Some(details) = &mut session.details {
                if !details.is_empty() {
                    let remove = details.len().div_ceil(2);
                    details.drain(..remove);
                    session.details_omitted += remove as u64;
                    removed_details = true;
                }
            }
        }
        if !removed_details {
            if report.sessions.is_empty() {
                break;
            }
            let remove = report.sessions.len().div_ceil(2);
            report.sessions.truncate(report.sessions.len() - remove);
            report.coverage.omitted_sessions += remove;
        }
    }
}

/// Human output has the same observations and coverage limits, never stronger
/// claims about delivery, attention or absent activity than the JSON report.
pub fn render_human(report: &ActivityReport) -> String {
    render_human_unbounded(report)
}

fn human_time(seconds: u64) -> String {
    mneme_present::format_timestamp_millis(u128::from(seconds) * 1000)
}

fn lane_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Lifecycle => "Lifecycle",
        EventKind::Core => "Startup core",
        EventKind::RecallAdmission => "Recall",
        EventKind::RecallSelection => "Recall selection",
        EventKind::DeliveryConsumed => "Delivery",
        EventKind::HookOutput => "Hook output",
        EventKind::HostAdmission => "Host admission",
        EventKind::Recording => "Recording",
        EventKind::Checkpoint => "Checkpoint",
    }
}

// These phrases describe one recorded operation, not a lifetime memory total.
// In particular a SAVE count can include episode/edition IDs, so it is never
// presented as a number of notes saved.
fn outcome_words(kind: EventKind, outcome: &str) -> String {
    match (kind, outcome) {
        (EventKind::Core, "ok") => "prepared".into(),
        (EventKind::Core, "no_selected_owners") => "no owners selected".into(),
        (EventKind::DeliveryConsumed, "consumed" | "consumed_for_emission") => {
            "consumed for emission".into()
        }
        (EventKind::Recording, "verified") => "job verified (not a save confirmation)".into(),
        (EventKind::Recording, "mode_automatic") => "automatic recording enabled".into(),
        (EventKind::Recording, "mode_disabled") => "disabled".into(),
        (EventKind::Checkpoint, "actor_reported_capture") => "capture reported by actor".into(),
        (EventKind::HostAdmission, "admitted_retained") => {
            "admission observed; evidence retained".into()
        }
        (EventKind::HostAdmission, "admitted_omitted") => {
            "admission observed; evidence omitted from recording context".into()
        }
        (EventKind::HostAdmission, "not_admitted") => {
            "admission not observed in the transcript slice".into()
        }
        (_, "mode_disabled") => "disabled mode".into(),
        (_, "mode_automatic") => "automatic mode".into(),
        _ => outcome.replace('_', " "),
    }
}

fn counted_notes(count: Option<u64>, action: &str) -> String {
    match count {
        Some(count) => format!("{count} note{} {action}", if count == 1 { "" } else { "s" }),
        None => format!("{action} (note count unknown)"),
    }
}

fn latest_description(lane: &LaneSummary) -> String {
    let Some(outcome) = lane.last_outcome.as_deref() else {
        return "unknown".into();
    };
    let mut description = match (lane.kind, outcome) {
        (EventKind::Core, "ok" | "prepared") => counted_notes(lane.last_count, "prepared"),
        (EventKind::Core, "partial") => format!(
            "{} (partial coverage)",
            counted_notes(lane.last_count, "prepared")
        ),
        (EventKind::Core, "empty") => "no notes returned".into(),
        (EventKind::RecallSelection, "selected") => counted_notes(lane.last_count, "selected"),
        (EventKind::RecallSelection, "empty") => "no notes selected".into(),
        (EventKind::DeliveryConsumed, "consumed" | "consumed_for_emission") => {
            counted_notes(lane.last_count, "consumed for emission")
        }
        (EventKind::HookOutput, "prepared") => {
            counted_notes(lane.last_count, "prepared for the host")
        }
        _ => outcome_words(lane.kind, outcome),
    };
    if let Some(owner) = &lane.last_owner {
        description.push_str(" for ");
        description.push_str(&owner.name);
    }
    if matches!(lane.kind, EventKind::Recording | EventKind::HostAdmission) {
        if let Some(count) = lane.last_count.filter(|count| *count > 0) {
            description.push_str(&format!(
                " ({count} referenced ID{})",
                if count == 1 { "" } else { "s" }
            ));
        }
    }
    if lane.kind == EventKind::Checkpoint && outcome == "actor_reported_capture" {
        // An actor claim never becomes independent save confirmation, even if
        // its optional reason is absent or names some other disposition.
        description.push_str(" — unverified");
        if let Some(reason) = lane
            .last_reason
            .as_deref()
            .filter(|reason| *reason != "unverified")
        {
            description.push_str("; ");
            description.push_str(&reason.replace('_', " "));
        }
    } else if let Some(reason) = &lane.last_reason {
        description.push_str(" — ");
        description.push_str(&reason.replace('_', " "));
    }
    description
}

fn outcome_counts(lane: &LaneSummary) -> String {
    // Some producer codes have the same human meaning (core ok/prepared).
    // Combine those labels, not their memory counts or any separate lanes.
    let mut counts = BTreeMap::new();
    for (outcome, count) in &lane.outcomes {
        *counts
            .entry(outcome_words(lane.kind, outcome))
            .or_insert(0u64) += count;
    }
    counts
        .into_iter()
        .map(|(outcome, count)| format!("{count} {outcome}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn issue_name(issue: CoverageIssue) -> &'static str {
    match issue {
        CoverageIssue::Missing => "missing",
        CoverageIssue::Unreadable => "unreadable",
        CoverageIssue::Malformed => "malformed",
        CoverageIssue::Unsupported => "unsupported schema",
        CoverageIssue::Oversize => "oversize",
        CoverageIssue::UnsafePath => "unsafe path refused",
        CoverageIssue::ScopeMismatch => "outside workspace/source scope",
        CoverageIssue::IdentityMismatch => "identity mismatch",
        CoverageIssue::UnknownEntry => "unrecognized directory entry",
        CoverageIssue::DirectoryLimit => "directory scan limit",
        CoverageIssue::ReadLimit => "read budget limit",
        CoverageIssue::OutputLimit => "report size limit",
        CoverageIssue::Cancelled => "cancelled",
        CoverageIssue::PlatformUnsupported => "platform unsupported",
    }
}

fn render_human_unbounded(report: &ActivityReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Session activity: {}",
        match report.status {
            ActivityStatus::Ready => "retained observations",
            ActivityStatus::NotConfigured => "not configured",
            ActivityStatus::InvalidManifest => "invalid activity manifest",
            ActivityStatus::Unavailable => "unavailable",
            ActivityStatus::WorkspaceUnbound => "workspace not admitted",
            ActivityStatus::WorkspaceBoundary =>
                "nested project boundary; explicit binding required",
            ActivityStatus::NoObservations => "no retained observation",
            ActivityStatus::SessionNotFound => "no retained observation for that session",
            ActivityStatus::SessionUnavailable => "session observation unavailable",
            ActivityStatus::AmbiguousSession => "ambiguous prefix; choose a session below",
            ActivityStatus::FullIdRequired => "catalog incomplete; use a full session ID",
            ActivityStatus::Cancelled => "cancelled; partial observations only",
        }
    );
    if let Some(scope) = &report.scope {
        let _ = writeln!(out, "Workspace: {} ({})", scope.workspace, scope.source);
    }
    if report.coverage.scan == ScanKind::Catalog && !report.coverage.complete_catalog {
        out.push_str("Recent among scanned sessions only; catalog coverage is incomplete.\n");
    }
    for session in &report.sessions {
        let selector = if report.coverage.partial
            || !report.coverage.complete_catalog
            || session.details.is_some()
        {
            session.selector.clone()
        } else {
            format!("{}:{}", session.source, session.display_id)
        };
        let _ = writeln!(
            out,
            "\n{selector} · {}",
            session.device.as_deref().unwrap_or("device unknown")
        );
        let _ = writeln!(
            out,
            "  Observed: {} → {} (UTC)",
            human_time(session.first_observed),
            human_time(session.last_observed)
        );
        for lane in &session.summary {
            let _ = write!(out, "  {}: ", lane_name(lane.kind));
            if lane.latest_status == LatestObservationStatus::AmbiguousTimestamp {
                let _ = write!(
                    out,
                    "{} observations — {}; latest observation ambiguous at {} (conflicting observations share this second)",
                    lane.observed_events,
                    outcome_counts(lane),
                    human_time(lane.last_at)
                );
            } else {
                out.push_str(&latest_description(lane));
                if lane.observed_events > 1 {
                    let _ = write!(
                        out,
                        " (latest of {} observations; outcomes: {})",
                        lane.observed_events,
                        outcome_counts(lane)
                    );
                }
            }
            out.push('\n');
        }
        let missing = [
            EventKind::Core,
            EventKind::RecallAdmission,
            EventKind::RecallSelection,
            EventKind::DeliveryConsumed,
            EventKind::HookOutput,
            EventKind::HostAdmission,
            EventKind::Recording,
        ]
        .into_iter()
        .filter(|kind| !session.summary.iter().any(|s| s.kind == *kind))
        .map(|kind| lane_name(kind).to_lowercase())
        .collect::<Vec<_>>();
        if !missing.is_empty() {
            let _ = writeln!(out, "  No retained observation: {}", missing.join(", "));
        }
        if let Some(details) = &session.details {
            for event in details {
                let _ = write!(
                    out,
                    "  @{} {} {}",
                    human_time(event.at),
                    event.kind.as_str(),
                    event.outcome
                );
                if let Some(reason) = &event.reason {
                    let _ = write!(out, " reason={reason}");
                }
                if let Some(owner) = &event.owner {
                    let _ = write!(out, " owner={}/{} db={}", owner.name, owner.id, owner.db);
                }
                if !event.nodes.is_empty() {
                    let _ = write!(out, " nodes={}", event.nodes.join(","));
                }
                if let Some(digest) = &event.selection_digest {
                    let _ = write!(out, " selection_digest={digest}");
                }
                let _ = writeln!(
                    out,
                    " origin={} count={} omitted={} key={}",
                    serde_json::to_string(&event.origin).unwrap_or_default(),
                    event
                        .count
                        .map_or_else(|| "unknown".into(), |n| n.to_string()),
                    event.omitted,
                    event.key
                );
            }
        }
        let mut limits = Vec::new();
        if session.evicted_events > 0 {
            limits.push(format!("{} observations evicted", session.evicted_events));
        }
        if session.omitted_nodes > 0 {
            limits.push(format!(
                "{} node ID{} omitted",
                session.omitted_nodes,
                if session.omitted_nodes == 1 { "" } else { "s" }
            ));
        }
        if session.details_omitted > 0 {
            limits.push(format!(
                "{} detail observations omitted",
                session.details_omitted
            ));
        }
        if !limits.is_empty() {
            let _ = writeln!(out, "  Limits: {}", limits.join("; "));
        }
    }
    let scope = match report.coverage.scan {
        ScanKind::NotStarted => "catalog not scanned",
        ScanKind::Direct => "direct full-ID lookup; other sessions not scanned",
        ScanKind::Catalog if report.coverage.complete_catalog => "catalog scanned completely",
        ScanKind::Catalog => "catalog only partly scanned",
    };
    let _ = write!(out, "\nCoverage: {scope}");
    if !report.coverage.issues.is_empty() {
        let issues = report
            .coverage
            .issues
            .iter()
            .map(|(issue, count)| format!("{count} {}", issue_name(*issue)))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(out, "; {issues}");
    }
    if report.coverage.omitted_sessions > 0 {
        let _ = write!(
            out,
            "; {} sessions omitted from report",
            report.coverage.omitted_sessions
        );
    }
    out.push_str(".\nNot tracked: explicit CLI/MCP operations, Claude capture, other hosts or remote devices.\nProspective, best-effort history: gaps do not mean inactivity. Prepared or selected context does not prove host admission, attention or usefulness.\n");
    out
}

#[cfg(test)]
mod tests;
