//! `mneme-mcp` — an MCP server over the mneme memory graph, so an agent can use
//! memory through a connection instead of shelling out to a binary on PATH.
//!
//! Owner mode is told at startup which databases it may touch (`--db name=path`,
//! repeatable; **defaults to the user db only**) and addresses them by logical
//! name; each name resolves to a stamped db id. Explicit `--library-config`
//! selects a separate read-only coordinator mode with no owner registry.
//! Explicit `--router-config` selects scope-pinned forwarding to existing owners
//! and optional read-only library snapshots, also with no owner registry.
//! All modes use JSON-RPC over bounded stdio or Streamable HTTP transport.

mod activity;
mod capture;
mod concern;
mod context_observation;
mod edit_body;
mod edit_summary;
mod episode;
mod host;
#[cfg(feature = "http")]
mod http;
mod library_server;
mod receipts;
mod response;
mod retag;
mod router_catalog;
mod router_response;
mod router_server;
mod save;
mod snapshot;
mod touchstone;

use std::collections::HashSet;
use std::collections::{BTreeMap, HashMap};
use std::num::{NonZeroU16, NonZeroU32};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use host::{AnyErr, DatabaseCheckout, DatabaseSlot, DatabaseSlotStatus, DbHandle};
use mneme_app::episode::{EpisodeAction, EpisodeCapability};
#[cfg(test)]
use mneme_app::recall_context_routed;
use mneme_app::{prepare_context_window, presentation_retrieval_metadata};
use mneme_core::ports::{Budget, ColdPath, FeedbackCommitOutcome, StatusFilter};
use mneme_core::{
    BodySpan, EdgeKind, MAX_REMOTE_EDGE_PAGE_SIZE, MergeResolution, Node, NodeId, NodeStatus,
    Provenance, RemoteEdgeCursor, Resolution, Signal,
};
use mneme_engine::{Ingest, RetrievalEvidence, RetrievalHit};
use mneme_present::{
    BodyBudget, LaneBudgets, LaneLimit, PresentationBudget, QueryBody, QueryEnvelope, QueryHit,
    QueryNodeStatus, RankEvidence,
};
use mneme_walk::{WalkSession, kind_str, status_str};
use receipts::{McpSession, SessionState, claim_walk_receipts, complete_walk};
use response::BoundedResponse;
use serde_json::{Value, json};
#[cfg(test)]
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;

/// Drop a walk session left idle longer than this. Walks are short-lived; an
/// hour of silence means the client is gone.
const SESSION_TTL: Duration = Duration::from_secs(3600);
/// Hard ceiling on concurrent walk sessions. Expired sessions are purged first;
/// a full live pool rejects `start` instead of destroying a resumable walk.
const MAX_SESSIONS: usize = 64;
const MAX_WALK_ACTIONS: usize = 256;
const MAX_WALK_PATH_DEPTH: usize = 64;
const MAX_REFLECT_RECEIPTS: usize = 8;
const MAX_REFLECT_USED: usize = 64;

/// Current stable MCP revision plus the two Streamable-HTTP revisions we can
/// faithfully serve. During initialization we echo a supported client revision;
/// otherwise we offer the newest one and let the client decide whether to
/// continue, as MCP version negotiation requires.
const PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[PROTOCOL_VERSION, "2025-06-18", "2025-03-26"];
/// Top edges shown inline on a `get` (the full list is the `neighbors` tool).
const GET_EDGES: usize = 8;
const GET_REMOTE_EDGES: usize = 8;
/// Service-side ceilings: client-supplied budgets are hints inside these bounds,
/// never permission to turn one request into an unbounded graph/response walk.
const MAX_QUERY_K: usize = 64;
const MAX_QUERY_NODES: usize = 256;
const MAX_QUERY_DEPTH: u8 = 12;
const MAX_RECALL_EXPAND: usize = 16;
const MAX_NEIGHBORS_EACH: usize = 16;
const MAX_WALK_BUDGET: usize = 64;
const DEFAULT_BODY_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_CORE_NODES: usize = 32;
const MAX_CORE_BODY_BYTES: usize = 128 * 1024;
const MAX_QUERY_BYTES: usize = 8 * 1024;
const MAX_SUMMARY_BYTES: usize = 2 * 1024;
const MAX_INGEST_BODY_BYTES: usize = 256 * 1024;
const MAX_TAGS: usize = 32;
const MAX_TAG_BYTES: usize = mneme_core::MAX_TAG_BYTES;
const MAX_STDIO_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_CONTEXT_BYTES: u32 = 32 * 1024;
const CONTEXT_CONTROL_RESERVE_BYTES: u32 = 2 * 1024;

/// Expensive whole-store work is deliberately fail-fast and process-serialized.
/// Starting at most one such operation per interval prevents a fast public MCP
/// loop from repeatedly consuming the graph worker between latency-sensitive
/// recall calls. The permit is RAII so cancellation cannot strand the gate.
const COLD_WORK_MIN_INTERVAL: Duration = Duration::from_secs(1);

struct ColdWorkGate {
    inner: std::sync::Arc<ColdWorkGateInner>,
}

struct ColdWorkGateInner {
    state: std::sync::Mutex<ColdWorkState>,
    min_interval: Duration,
}

#[derive(Default)]
struct ColdWorkState {
    in_flight: Option<String>,
    next_admission: Option<Instant>,
}

struct ColdWorkAdmission {
    gate: std::sync::Arc<ColdWorkGateInner>,
}

impl ColdWorkGate {
    fn new(min_interval: Duration) -> Self {
        Self {
            inner: std::sync::Arc::new(ColdWorkGateInner {
                state: std::sync::Mutex::new(ColdWorkState::default()),
                min_interval,
            }),
        }
    }

    fn admit(&self, operation: &str) -> Result<ColdWorkAdmission, AnyErr> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active) = &state.in_flight {
            return Err(
                format!("cold work is busy running {active:?}; retry {operation:?} later").into(),
            );
        }

        let now = Instant::now();
        if let Some(next) = state.next_admission.filter(|next| *next > now) {
            let retry_ms = next.saturating_duration_since(now).as_millis().max(1);
            return Err(
                format!("cold work is rate limited; retry {operation:?} in {retry_ms} ms").into(),
            );
        }

        state.in_flight = Some(operation.to_owned());
        state.next_admission = now.checked_add(self.inner.min_interval);
        Ok(ColdWorkAdmission {
            gate: self.inner.clone(),
        })
    }
}

impl Default for ColdWorkGate {
    fn default() -> Self {
        Self::new(COLD_WORK_MIN_INTERVAL)
    }
}

impl Drop for ColdWorkAdmission {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight = None;
    }
}

/// Public calls whose implementation can scan a store or run whole-graph work.
/// Keyed curation mutations still mint `ColdPath` at their exact call sites, but
/// do not consume this scarce admission lane.
fn metered_cold_tool(name: &str) -> bool {
    matches!(
        name,
        "status" | "core" | "forget" | "contradictions" | "merges" | "decay" | "prune"
    )
}

/// Cross-db edges may only originate here — the private, never-shipped db — so a
/// project db stays self-contained. This is not an omitted-read selector.
const HOME_DB: &str = "user";

struct Registry {
    dbs: BTreeMap<String, std::sync::Arc<DatabaseSlot>>,
    activity: activity::ActivityRing,
}

impl Registry {
    fn slot(&self, name: &str) -> Result<&std::sync::Arc<DatabaseSlot>, AnyErr> {
        self.dbs.get(name).ok_or_else(|| {
            format!("unknown database {name:?} (not in the startup registry)").into()
        })
    }

    fn select_read_scope<'a>(&'a self, arguments: &'a Value) -> Result<&'a str, AnyErr> {
        if let Some(name) = optional_string(arguments, "db")? {
            return Ok(name);
        }
        if self.dbs.contains_key("project") {
            return Ok("project");
        }
        if self.dbs.len() == 1 {
            return Ok(self.dbs.keys().next().expect("sole registered owner"));
        }
        Err(
            "database scope is ambiguous or unavailable; pass explicit `db` from `databases`"
                .into(),
        )
    }

    fn checkout(&self, name: &str) -> Result<DatabaseCheckout, AnyErr> {
        self.slot(name)?
            .checkout()
            .map_err(|error| format!("database {name:?} is unavailable: {error}").into())
    }

    fn catalog(&self) -> Result<Vec<(String, DatabaseSlotStatus)>, AnyErr> {
        let mut entries = Vec::with_capacity(self.dbs.len());
        for (name, slot) in &self.dbs {
            entries.push((name.clone(), slot.status()?));
        }
        Ok(entries)
    }

    /// Match one stable remote target id and pin the matching open handle under
    /// the same slot lock. The nested option distinguishes an unknown id from a
    /// known but maintenance-fenced target.
    fn remote_target(
        &self,
        db_id: ulid::Ulid,
    ) -> Result<Option<(String, Option<DatabaseCheckout>)>, AnyErr> {
        for (name, slot) in &self.dbs {
            if let Some(checkout) = slot.checkout_if_db_id(db_id)? {
                return Ok(Some((name.clone(), checkout)));
            }
        }
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CapabilityProfile {
    ReadOnly,
    #[default]
    ReceiptGrounded,
    Curator,
    Operator,
}

impl CapabilityProfile {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ReceiptGrounded => "receipt-grounded",
            Self::Curator => "curator",
            Self::Operator => "operator",
        }
    }

    fn parse(value: &str) -> Result<Self, AnyErr> {
        match value {
            "read-only" => Ok(Self::ReadOnly),
            "receipt-grounded" => Ok(Self::ReceiptGrounded),
            "curator" => Ok(Self::Curator),
            "operator" => Ok(Self::Operator),
            other => Err(format!(
                "--capability-profile must be read-only|receipt-grounded|curator|operator, got {other:?}"
            )
            .into()),
        }
    }

    const fn permits(self, required: CapabilityClass) -> bool {
        match (self, required) {
            (Self::ReadOnly, CapabilityClass::ReadOnly) => true,
            (
                Self::ReceiptGrounded,
                CapabilityClass::ReadOnly | CapabilityClass::ReceiptGrounded,
            ) => true,
            (
                Self::Curator,
                CapabilityClass::ReadOnly
                | CapabilityClass::ReceiptGrounded
                | CapabilityClass::Curator,
            ) => true,
            (
                Self::Operator,
                CapabilityClass::ReadOnly
                | CapabilityClass::ReceiptGrounded
                | CapabilityClass::Curator
                | CapabilityClass::Operator,
            ) => true,
            (
                Self::ReadOnly,
                CapabilityClass::ReceiptGrounded
                | CapabilityClass::Curator
                | CapabilityClass::Operator,
            )
            | (Self::ReceiptGrounded, CapabilityClass::Curator | CapabilityClass::Operator)
            | (Self::Curator, CapabilityClass::Operator) => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CapabilityPolicy {
    profile: CapabilityProfile,
    allow_direct_feedback: bool,
}

impl CapabilityPolicy {
    const fn new(profile: CapabilityProfile, allow_direct_feedback: bool) -> Self {
        Self {
            profile,
            allow_direct_feedback,
        }
    }

    #[cfg(test)]
    const fn operator() -> Self {
        Self::new(CapabilityProfile::Operator, false)
    }

    #[cfg(test)]
    const fn operator_with_direct_feedback() -> Self {
        Self::new(CapabilityProfile::Operator, true)
    }

    fn authorize(self, request: ToolRequestKind) -> Result<(), AnyErr> {
        let required = request.capability_class();
        if !self.profile.permits(required) {
            return Err(format!(
                "capability profile {:?} does not authorize {} ({})",
                self.profile.as_str(),
                request.tool_name(),
                required.as_str(),
            )
            .into());
        }
        if request == ToolRequestKind::Feedback && !self.allow_direct_feedback {
            return Err(
                "direct feedback compatibility is disabled; operator deployments must also pass --allow-direct-feedback, or use a completed walk receipt with reflect"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Explicit non-owner modes: neither constructs a persistent-store registry.
enum Coordinator {
    Library(library_server::LibraryServer),
    Router(router_server::RouterServer),
}

impl Coordinator {
    async fn dispatch(&self, method: &str, params: Value) -> Result<Value, AnyErr> {
        match self {
            Self::Library(server) => server.dispatch(method, params).await,
            Self::Router(server) => server.dispatch(method, params).await,
        }
    }
    async fn close(&self) {
        if let Self::Router(server) = self {
            server.close().await;
        }
    }
}

/// Both transports drive the same owner or coordinator dispatch.
struct Server {
    registry: Registry,
    /// Present only in explicit library/router mode; owner registry is empty then.
    library: Option<Coordinator>,
    /// Keeping walk state separate means unrelated reads and writes can run
    /// concurrently instead of one slow embedding call serializing the service.
    sessions: std::sync::Arc<Mutex<SessionState>>,
    /// One server is one MCP process, shared by both possible transports and all
    /// registered databases. Hot retrieval never touches this cold-work gate.
    cold_work: ColdWorkGate,
    /// One process-wide ambient-authority profile shared by every transport and
    /// caller. This is not an identity, credential, or tenant boundary.
    capability: CapabilityPolicy,
}

struct Args {
    dbs: Vec<(String, PathBuf)>,
    library_config: Option<PathBuf>,
    router_config: Option<PathBuf>,
    http: Option<String>,
    capability_profile: CapabilityProfile,
    /// Explicitly expose the unreceipted direct-feedback compatibility tool.
    /// A bare legacy flag implies operator for backwards compatibility; an
    /// explicit weaker profile is rejected rather than silently escalated.
    allow_direct_feedback: bool,
    /// Whether the deprecated feedback spelling implicitly selected operator.
    direct_feedback_implied_operator: bool,
    /// Environment variable containing the HTTP bearer token. Taking the secret
    /// by variable name keeps it out of process listings and shell history.
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    http_token_env: Option<String>,
    /// Exact browser Origin allowed to use the HTTP endpoint. No CORS headers are
    /// emitted by default; non-browser MCP clients do not need them.
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    cors_origin: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), AnyErr> {
    let mut args = parse_args()?;
    if let Some(path) = args.router_config.as_deref() {
        let server = Server {
            registry: Registry {
                activity: crate::activity::ActivityRing::default(),
                dbs: BTreeMap::new(),
            },
            library: Some(Coordinator::Router(
                router_server::RouterServer::from_path(path).await?,
            )),
            sessions: std::sync::Arc::new(Mutex::new(SessionState::default())),
            cold_work: ColdWorkGate::default(),
            capability: CapabilityPolicy::new(CapabilityProfile::ReadOnly, false),
        };
        return serve_selected_transport(server, args, "configured owners and read-only replicas")
            .await;
    }
    if let Some(path) = args.library_config.as_deref() {
        // This branch intentionally never calls build_registry or constructs an
        // InferenceRuntime. Owner mode cannot be reached by this process.
        let server = Server {
            registry: Registry {
                activity: crate::activity::ActivityRing::default(),
                dbs: BTreeMap::new(),
            },
            library: Some(Coordinator::Library(
                library_server::LibraryServer::from_path(path)?,
            )),
            sessions: std::sync::Arc::new(Mutex::new(SessionState::default())),
            cold_work: ColdWorkGate::default(),
            capability: CapabilityPolicy::new(CapabilityProfile::ReadOnly, false),
        };
        return serve_selected_transport(server, args, "library snapshots").await;
    }
    let mut sessions = SessionState::default();
    let registry = build_registry(std::mem::take(&mut args.dbs), &mut sessions)?;
    let server = Server {
        registry,
        sessions: std::sync::Arc::new(Mutex::new(sessions)),
        cold_work: ColdWorkGate::default(),
        capability: CapabilityPolicy::new(args.capability_profile, args.allow_direct_feedback),
        library: None,
    };
    let names = server
        .registry
        .dbs
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "mneme-mcp: capability profile {} (ambient authority only; not caller authentication)",
        server.capability.profile.as_str()
    );
    if server.capability.allow_direct_feedback {
        if args.direct_feedback_implied_operator {
            eprintln!(
                "mneme-mcp: WARNING: --allow-direct-feedback is deprecated and selected --capability-profile operator for legacy compatibility"
            );
        } else {
            eprintln!(
                "mneme-mcp: WARNING: --allow-direct-feedback is a deprecated operator-only compatibility switch"
            );
        }
        eprintln!(
            "mneme-mcp: WARNING: exposing unreceipted direct feedback (--allow-direct-feedback)"
        );
    }
    serve_selected_transport(server, args, &format!("databases [{names}]")).await
}

async fn serve_selected_transport(server: Server, args: Args, subject: &str) -> Result<(), AnyErr> {
    match args.http {
        Some(addr) => {
            #[cfg(feature = "http")]
            {
                let token =
                    match args.http_token_env.as_deref() {
                        Some(name) => Some(std::env::var(name).map_err(|_| {
                            format!("--http-token-env names unset variable {name:?}")
                        })?),
                        None => None,
                    };
                eprintln!("mneme-mcp: serving {subject} over HTTP at {addr}");
                http::serve(server, &addr, token, args.cors_origin.as_deref()).await
            }
            #[cfg(not(feature = "http"))]
            {
                let _ = (addr, subject);
                Err("--http needs the `http` feature (rebuild with --features http)".into())
            }
        }
        None => {
            // Claim the real stdout for protocol output, then point fd 1 at stderr
            // so any library that writes to stdout (ONNX Runtime, fastembed's
            // progress bar) can't corrupt the JSON-RPC stream. Before any embedder.
            let out = claim_stdout();
            eprintln!("mneme-mcp: serving {subject} over stdio");
            run_stdio(server, out).await
        }
    }
}

/// `--db NAME=PATH` (repeatable) and optional hardened HTTP transport settings.
fn parse_args() -> Result<Args, AnyErr> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from<I, S>(args: I) -> Result<Args, AnyErr>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut dbs = Vec::new();
    let mut library_config = None;
    let mut router_config = None;
    let mut http = None;
    let mut http_token_env = None;
    let mut cors_origin = None;
    let mut capability_profile = None;
    let mut allow_direct_feedback = false;
    let mut it = args.into_iter().map(Into::into);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--db" => {
                let spec = it.next().ok_or("--db needs NAME=PATH")?;
                let (name, path) = spec
                    .split_once('=')
                    .ok_or_else(|| format!("--db expects NAME=PATH, got {spec:?}"))?;
                dbs.push((name.to_string(), PathBuf::from(path)));
            }
            "--router-config" => {
                if router_config.is_some() {
                    return Err("--router-config may be specified only once".into());
                }
                router_config = Some(PathBuf::from(
                    it.next().ok_or("--router-config needs PATH")?,
                ));
            }
            "--library-config" => {
                if library_config.is_some() {
                    return Err("--library-config may be specified only once".into());
                }
                library_config = Some(PathBuf::from(
                    it.next().ok_or("--library-config needs PATH")?,
                ));
            }
            "--http" => http = Some(it.next().ok_or("--http needs ADDR (e.g. 127.0.0.1:8080)")?),
            "--http-token-env" => {
                http_token_env = Some(
                    it.next()
                        .ok_or("--http-token-env needs an environment variable name")?,
                )
            }
            "--cors-origin" => {
                cors_origin = Some(
                    it.next()
                        .ok_or("--cors-origin needs one exact Origin URL")?,
                )
            }
            "--capability-profile" => {
                if capability_profile.is_some() {
                    return Err("--capability-profile may be specified only once".into());
                }
                capability_profile = Some(CapabilityProfile::parse(
                    &it.next()
                        .ok_or("--capability-profile needs a profile name")?,
                )?)
            }
            "--allow-direct-feedback" => allow_direct_feedback = true,
            "--help" | "-h" => {
                return Err(
                    "usage: mneme-mcp [--db NAME=PATH]... [--capability-profile read-only|receipt-grounded|curator|operator] [--allow-direct-feedback (deprecated; operator only)] [--http IP:PORT [--http-token-env VAR] [--cors-origin ORIGIN]] | mneme-mcp --library-config PATH [--http IP:PORT [--http-token-env VAR] [--cors-origin ORIGIN]] | mneme-mcp --router-config PATH [--http IP:PORT [--http-token-env VAR] [--cors-origin ORIGIN]]"
                        .into(),
                );
            }
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }
    if http.is_none() && (http_token_env.is_some() || cors_origin.is_some()) {
        return Err("--http-token-env and --cors-origin require --http".into());
    }
    if router_config.is_some()
        && (library_config.is_some()
            || !dbs.is_empty()
            || capability_profile.is_some()
            || allow_direct_feedback)
    {
        return Err("--router-config cannot be combined with --library-config, --db, --capability-profile, or --allow-direct-feedback".into());
    }
    if library_config.is_some()
        && (!dbs.is_empty() || capability_profile.is_some() || allow_direct_feedback)
    {
        return Err("--library-config cannot be combined with --db, --capability-profile, or --allow-direct-feedback".into());
    }
    let direct_feedback_implied_operator = allow_direct_feedback && capability_profile.is_none();
    let capability_profile = capability_profile.unwrap_or_else(|| {
        if allow_direct_feedback {
            CapabilityProfile::Operator
        } else {
            CapabilityProfile::default()
        }
    });
    if allow_direct_feedback && capability_profile != CapabilityProfile::Operator {
        return Err(
            "--allow-direct-feedback is only valid with --capability-profile operator".into(),
        );
    }
    Ok(Args {
        dbs,
        library_config,
        router_config,
        http,
        capability_profile,
        allow_direct_feedback,
        direct_feedback_implied_operator,
        http_token_env,
        cors_origin,
    })
}

/// Returns a writer to the *real* stdout and redirects fd 1 to stderr, so only
/// this writer can reach the client. On non-unix, falls back to plain stdout.
fn claim_stdout() -> Box<dyn std::io::Write + Send> {
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd;
        // SAFETY: run once at startup; fds 1 and 2 are open. `dup` gives us a
        // private copy of the original stdout before we overwrite fd 1.
        unsafe {
            let saved = libc::dup(1);
            if saved >= 0 && libc::dup2(2, 1) >= 0 {
                return Box::new(std::fs::File::from_raw_fd(saved));
            }
        }
    }
    Box::new(std::io::stdout())
}

/// Open each `NAME=PATH` into a releasable registry slot; with none given,
/// exposes just the user db. All slots share one lazy inference runtime.
fn build_registry(
    mut entries: Vec<(String, PathBuf)>,
    sessions: &mut SessionState,
) -> Result<Registry, AnyErr> {
    if entries.is_empty() {
        entries.push(("user".to_string(), host::user_db()));
    }
    let inference = std::sync::Arc::new(host::InferenceRuntime::new());
    let mut dbs = BTreeMap::new();
    for (name, configured_path) in entries {
        if dbs.contains_key(&name) {
            return Err(format!("duplicate --db name {name:?}").into());
        }
        let feedback_epoch = sessions.feedback_epoch(&name);
        let slot = std::sync::Arc::new(
            DatabaseSlot::open(configured_path.clone(), inference.clone(), feedback_epoch)
                .map_err(|error| format!("opening db {name:?} ({configured_path:?}): {error}"))?,
        );
        dbs.insert(name, slot);
    }
    Ok(Registry {
        dbs,
        activity: activity::ActivityRing::default(),
    })
}

async fn run_stdio(server: Server, mut out: Box<dyn std::io::Write + Send>) -> Result<(), AnyErr> {
    let outcome: Result<(), AnyErr> = async {
        let mut input = BufReader::new(tokio::io::stdin());
        let mut line = Vec::new();
        while let Some(oversized) = read_bounded_line(&mut input, &mut line).await? {
            if oversized {
                write(
                    &mut out,
                    &BoundedResponse::new(error(
                        Value::Null,
                        -32600,
                        &format!("stdio request exceeds {MAX_STDIO_REQUEST_BYTES} bytes"),
                    )),
                )?;
                continue;
            }
            if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                continue;
            }
            let msg: Value = match serde_json::from_slice(&line) {
                Ok(v) => v,
                Err(e) => {
                    write(
                        &mut out,
                        &BoundedResponse::new(error(
                            Value::Null,
                            -32700,
                            &format!("parse error: {e}"),
                        )),
                    )?;
                    continue;
                }
            };
            if let Some(resp) = server.handle(msg).await {
                write(&mut out, &resp)?;
            }
        }
        Ok(())
    }
    .await;
    if let Some(coordinator) = &server.library {
        coordinator.close().await;
    }
    outcome
}

/// Read one newline-delimited MCP message without ever accumulating more than
/// the service limit. `AsyncBufReadExt::lines` allocates until newline and lets a
/// local client exhaust memory before validation; this drains an oversized line
/// incrementally and lets the next request continue normally.
async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
) -> std::io::Result<Option<bool>> {
    out.clear();
    let mut oversized = false;
    let mut saw_bytes = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if saw_bytes {
                Ok(Some(oversized))
            } else {
                Ok(None)
            };
        }
        saw_bytes = true;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        if !oversized {
            if out.len().saturating_add(take) > MAX_STDIO_REQUEST_BYTES {
                oversized = true;
                out.clear();
            } else {
                out.extend_from_slice(&available[..take]);
            }
        }
        reader.consume(take);
        if newline.is_some() {
            return Ok(Some(oversized));
        }
    }
}

fn write(
    out: &mut Box<dyn std::io::Write + Send>,
    response: &BoundedResponse,
) -> Result<(), AnyErr> {
    response.write_stdio(out.as_mut())?;
    out.flush()?;
    Ok(())
}

fn result_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

impl Server {
    /// Process one JSON-RPC message, returning the response — or `None` for a
    /// notification (no `id`). Transport-agnostic.
    pub async fn handle(&self, msg: Value) -> Option<BoundedResponse> {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        if !msg.is_object() || msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(BoundedResponse::new(error(
                id,
                -32600,
                "invalid JSON-RPC 2.0 request",
            )));
        }
        // A client response to a server-initiated request has no method. Mneme
        // currently emits none, but accepting the envelope is required by the
        // bidirectional HTTP transport.
        if msg.get("method").is_none()
            && (msg.get("result").is_some() || msg.get("error").is_some())
        {
            return None;
        }
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            return Some(BoundedResponse::new(error(
                id,
                -32600,
                "request method must be a string",
            )));
        };
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let result = self.dispatch(method, params).await;
        let id = id?; // a notification expects no response
        Some(BoundedResponse::new(match result {
            Ok(r) => result_response(id, r),
            Err(e) => {
                let code = match method {
                    "initialize" => -32602,
                    "tools/call" | "tools/list" | "ping" => -32603,
                    _ => -32601,
                };
                error(id, code, &e.to_string())
            }
        }))
    }

    async fn dispatch(&self, method: &str, params: Value) -> Result<Value, AnyErr> {
        if let Some(library) = &self.library {
            return library.dispatch(method, params).await;
        }
        match method {
            "initialize" => {
                let profile = self.capability.profile.as_str();
                Ok(json!({
                    "protocolVersion": negotiated_protocol(&params)?,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "mneme-mcp",
                        "version": env!("CARGO_PKG_VERSION"),
                        "capabilityProfile": profile,
                    },
                    "capabilityProfile": profile,
                    // Tool hosts may prepend this to every tool description.
                    // Leave room for the operation in short discovery previews.
                    "instructions": format!("Mneme memory. Profile: {profile}. {}", mneme_app::episode::MEMORY_TIME_GUIDANCE),
                }))
            }
            "tools/list" => Ok(json!({ "tools": tool_schemas(self.capability) })),
            "tools/call" => {
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                // Tool-level failures are reported as a result with isError, not a
                // protocol error, per MCP.
                match call_tool_authorized(
                    &self.registry,
                    &self.sessions,
                    &self.cold_work,
                    self.capability,
                    name,
                    &args,
                )
                .await
                {
                    Ok(v) => Ok(response::tool_success(&v)),
                    Err(e) => Ok(response::tool_error(&e.to_string())),
                }
            }
            "ping" => Ok(json!({})),
            other => Err(format!("unknown method {other:?}").into()),
        }
    }
}

fn negotiated_protocol(params: &Value) -> Result<&'static str, AnyErr> {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or("initialize.params.protocolVersion is required")?;
    Ok(SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .copied()
        .find(|version| *version == requested)
        .unwrap_or(PROTOCOL_VERSION))
}

// ---- tools ------------------------------------------------------------------

async fn call_tool_authorized(
    reg: &Registry,
    sessions: &std::sync::Arc<Mutex<SessionState>>,
    cold_work: &ColdWorkGate,
    capability: CapabilityPolicy,
    name: &str,
    a: &Value,
) -> Result<Value, AnyErr> {
    // MCP clients are not required to validate tool schemas before dispatch.
    // Parse the complete public request before taking a cold-work permit or a
    // database checkout. This type-state boundary keeps malformed optionals,
    // lossy integer narrowing, and arguments from the wrong conditional variant
    // from reaching a lease, inference runtime, or durable mutation.
    let mut request = ValidatedToolArguments::parse(name, a)?;
    let prepared_save = request.prepared_save.take();
    let prepared_concern = request.prepared_concern.take();
    let prepared_retag = request.prepared_retag.take();
    let prepared_body_edit = request.prepared_body_edit.take();
    let prepared_summary_edit = request.prepared_summary_edit.take();
    let kind = request.kind();
    let name = kind.tool_name();
    let raw = request.arguments();

    // Catalog filtering is discovery UX, not enforcement. Authorize the fully
    // parsed action independently, before cold admission or database checkout.
    capability.authorize(kind)?;

    // Choose the owner once, after complete admission and authorization. Walk
    // continuations remain session-bound; process-wide tools have no owner.
    let scoped_arguments = if kind.has_request_database_scope() {
        let name = reg.select_read_scope(raw)?;
        let mut arguments = raw.clone();
        arguments["db"] = json!(name);
        Some(arguments)
    } else {
        None
    };
    let a = scoped_arguments.as_ref().unwrap_or(raw);

    // Only unbounded store/graph work enters this lane. This admission is held
    // across the awaited operation and released on completion or cancellation.
    let _cold_admission = metered_cold_tool(name)
        .then(|| cold_work.admit(name))
        .transpose()?;
    match kind {
        ToolRequestKind::Activity { after, limit } => {
            Ok(serde_json::to_value(reg.activity.page(after, limit))?)
        }
        ToolRequestKind::Databases => Ok(json!(
            reg.catalog()?
                .into_iter()
                .map(|(name, status)| database_status_json(&name, &status))
                .collect::<Vec<_>>()
        )),

        ToolRequestKind::DatabaseControl(action) => {
            database_control_tool(reg, sessions, cold_work, action, a).await
        }

        ToolRequestKind::SnapshotCreate => snapshot_create_tool(reg, sessions, cold_work, a).await,

        ToolRequestKind::Status => {
            let h = db(reg, a)?;
            let st = h.mem.status(ColdPath::acquire()).await?;
            Ok(json!({
                "nodes": st.nodes,
                "episodes": st.episodes,
                "episode_editions": st.episode_editions,
                "active": st.active,
                "archived": st.archived,
                "open_contradictions": st.open_contradictions,
                "open_merge_candidates": st.open_merge_candidates,
                "edge_decay_pending": st.edge_decay_pending,
            }))
        }

        ToolRequestKind::Query => {
            let request = PublicQueryInput::parse(a, true)?;
            let h = db(reg, a)?;
            let base = h.mem.config().budget;
            let budget = Budget {
                max_nodes: request
                    .max_nodes
                    .unwrap_or(base.max_nodes.clamp(1, MAX_QUERY_NODES)),
                max_depth: request.depth.unwrap_or(base.max_depth.min(MAX_QUERY_DEPTH)),
                min_relevance: request
                    .min_relevance
                    .unwrap_or(base.min_relevance.clamp(0.0, 1.0)),
                ..base
            };
            let k = request
                .k
                .unwrap_or(h.mem.config().ann_k.clamp(1, MAX_QUERY_K));
            let status = request.status_filter();
            let tag_refs: Vec<&str> = request.tags.iter().map(String::as_str).collect();
            let batch = h
                .mem
                .retrieve_batch_seeded(request.text, k, budget, status, &tag_refs)
                .await?;
            let retrieval = presentation_retrieval_metadata(&batch)?;
            let primary = batch
                .primary
                .into_iter()
                .map(mcp_query_hit)
                .collect::<Vec<_>>();
            let ids = primary
                .iter()
                .map(QueryHit::id)
                .take(activity::MAX_IDS + 1)
                .collect::<Vec<_>>();
            let response = serde_json::to_value(QueryEnvelope::new(retrieval, primary)?)?;
            reg.activity
                .returned("query", selected_db_name(a)?, h.db_id, ids);
            Ok(response)
        }

        ToolRequestKind::RecallContext => {
            let request = PublicQueryInput::parse(a, false)?;
            let observe = optional_bool(a, "observe")?;
            let routing_hints = context_observation::parse_hints(a);
            let h = db(reg, a)?;
            let base = h.mem.config().budget;
            let budget = Budget {
                max_nodes: request
                    .max_nodes
                    .unwrap_or(base.max_nodes.clamp(1, MAX_QUERY_NODES)),
                max_depth: request.depth.unwrap_or(base.max_depth.min(MAX_QUERY_DEPTH)),
                min_relevance: request
                    .min_relevance
                    .unwrap_or(base.min_relevance.clamp(0.0, 1.0)),
                ..base
            };
            let k = request
                .k
                .unwrap_or(h.mem.config().ann_k.clamp(1, MAX_QUERY_K));
            let tag_refs: Vec<&str> = request.tags.iter().map(String::as_str).collect();
            let presentation = context_presentation_budget(budget.max_nodes);
            let (hints, preignored) = routing_hints
                .map(|parsed| parsed.for_database(h.db_id))
                .map_or((None, 0), |(hints, ignored)| (Some(hints), ignored));
            let window = prepare_context_window(
                &h.mem,
                request.text,
                k,
                budget,
                &tag_refs,
                observe || hints.is_some(),
                hints.as_deref(),
            )
            .await?;
            let (context, rendered) = context_observation::pack_and_render(
                &window,
                &presentation,
                observe,
                hints.as_ref().map(|_| (h.db_id, preignored)),
                DEFAULT_CONTEXT_BYTES as usize,
            )?;
            let plan = context.plan;

            reg.activity.returned(
                "recall_context",
                selected_db_name(a)?,
                h.db_id,
                plan.manifest().cards().iter().map(|card| card.node_id()),
            );
            // A string value is significant here: `tool_success` recognizes it
            // as already-rendered compact content and wraps these exact bytes in
            // one MCP text block. `BoundedResponse` still applies the final
            // serialized JSON-RPC frame fuse after JSON escaping.
            Ok(Value::String(rendered))
        }

        ToolRequestKind::Get => {
            let h = db_with_expected_id(reg, a, request.expected_db_id)?;
            let id = parse_id(req_s(a, "id")?)?;
            let node = h.mem.get_node(id).await?.ok_or("node not found")?;
            let mut v = node_json(&node);
            let projection =
                mneme_app::touchstone::node_touchstone_projection(&h.mem, h.db_id, &node).await?;
            v.as_object_mut().expect("node JSON object").extend(
                projection
                    .as_object()
                    .expect("touchstone projection object")
                    .clone(),
            );
            v["concern_endpoint"] =
                serde_json::to_value(mneme_app::concern::endpoint_for_node(&node))?;
            if optional_bool(a, "body")? {
                let chunk = h
                    .mem
                    .resolve_body_range(&node, body_offset(a)?, body_limit(a)?)
                    .await?;
                v["body"] = json!(render_body_bytes(&chunk.bytes));
                v["body_range"] = json!({
                    "source_start": chunk.source_start,
                    "source_end": chunk.source_end,
                    "next_offset": chunk.next_offset,
                    "has_more": chunk.next_offset.is_some(),
                });
            }
            if optional_bool(a, "edges")? {
                let page = neighbors_json(&h, id, GET_EDGES).await?;
                v["edges"] = page["items"].clone();
                v["edges_returned"] = page["returned"].clone();
                v["edges_has_more"] = page["has_more"].clone();
                v["remote"] = remote_page_json(reg, &h, id, None, GET_REMOTE_EDGES, true).await?;
            }
            reg.activity
                .returned("get", selected_db_name(a)?, h.db_id, [node.id()]);
            if request.expected_db_id.is_some() {
                v["db"] = json!(selected_db_name(a)?);
                v["db_id"] = json!(h.db_id.to_string());
            }
            Ok(v)
        }

        ToolRequestKind::List => {
            let prepared = touchstone::prepare_list(a)?;
            let h = db_with_expected_id(reg, a, request.expected_db_id)?;
            let mut result = prepared.run(&h.mem, h.db_id).await?;
            result["db"] = json!(selected_db_name(a)?);
            Ok(result)
        }

        ToolRequestKind::Graph => {
            let prepared = prepare_graph(a)?;
            let h = db(reg, a)?;
            owner_response(
                selected_db_name(a)?,
                h.db_id,
                prepared.run(&h.mem, h.db_id).await?,
            )
        }

        ToolRequestKind::Neighbors => {
            let prepared = prepare_neighbors(a)?;
            let h = db(reg, a)?;
            owner_response(
                selected_db_name(a)?,
                h.db_id,
                prepared.run(&h.mem, h.db_id).await?,
            )
        }

        ToolRequestKind::RemoteEdges => {
            let h = db(reg, a)?;
            let id = parse_id(req_s(a, "id")?)?;
            remote_page_json(
                reg,
                &h,
                id,
                remote_cursor(a)?,
                remote_page_limit(a)?,
                optional_bool(a, "resolve")?,
            )
            .await
        }

        ToolRequestKind::Core => {
            let h = db(reg, a)?;
            let all = h.mem.core(ColdPath::acquire()).await?;
            let total = all.len();
            let visible = total.min(MAX_CORE_NODES);
            let per_body_limit = body_limit(a)?;
            let mut remaining_body_bytes = MAX_CORE_BODY_BYTES;
            let mut bodies_truncated = false;
            let mut out = Vec::new();
            let ids = all.iter().take(visible).map(Node::id).collect::<Vec<_>>();
            for (index, n) in all.into_iter().take(visible).enumerate() {
                let mut v = node_json(&n);
                // Share the aggregate allowance across the remaining nodes so one
                // huge first body cannot starve the entire core set. Prefix reads
                // also keep the service's memory/I/O bounded before serialization.
                let nodes_left = visible - index;
                let fair_share = remaining_body_bytes / nodes_left.max(1);
                let cap = per_body_limit.min(fair_share);
                if let Ok((bytes, truncated)) = h.mem.resolve_body_prefix(&n, cap).await {
                    remaining_body_bytes = remaining_body_bytes.saturating_sub(bytes.len());
                    bodies_truncated |= truncated;
                    let body = render_body_bytes(&bytes);
                    v["body"] = json!(body);
                    v["body_truncated"] = json!(truncated);
                } else {
                    v["body"] = Value::Null;
                }
                out.push(v);
            }
            reg.activity
                .returned("core", selected_db_name(a)?, h.db_id, ids);
            Ok(json!({
                "nodes": out,
                "total": total,
                "truncated": total > MAX_CORE_NODES || bodies_truncated,
                "nodes_truncated": total > MAX_CORE_NODES,
                "bodies_truncated": bodies_truncated,
                "body_bytes_limit": MAX_CORE_BODY_BYTES,
            }))
        }

        ToolRequestKind::Ingest(_) => {
            let h = db(reg, a)?;
            let summary = bounded_required(a, "summary", MAX_SUMMARY_BYTES)?;
            let body = optional_string(a, "body")?
                .unwrap_or(summary)
                .as_bytes()
                .to_vec();
            if body.len() > MAX_INGEST_BODY_BYTES {
                return Err(format!(
                    "body is {} bytes; maximum is {MAX_INGEST_BODY_BYTES}",
                    body.len()
                )
                .into());
            }
            let tags = strs(a, "tags")?;
            validate_tags(&tags)?;
            let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
            // Stamp the memory with the repo's current HEAD (if this db lives in a
            // working tree) — a temporal anchor into the project's history.
            let commit = h.origin_commit();
            let mut ing = Ingest::new(summary, &body, &tag_refs, Provenance::derived_empty())
                .with_origin_commit(commit.as_deref())?;
            if let Some(stability) = optional_unit_interval(a, "stability")? {
                ing = ing.with_stability(stability);
            }
            if let Some(confidence) = optional_unit_interval(a, "confidence")? {
                ing = ing.with_confidence(confidence);
            }
            let id = h.mem.ingest(ing).await?;
            h.save()?;
            mutation_response(a, &h, json!({ "id": id.0.to_string() }))
        }

        ToolRequestKind::EditBody => {
            let prepared = prepared_body_edit.expect("edit_body admission retains its request");
            let h = db_with_expected_id(reg, a, request.expected_db_id)?;
            let path = reg.slot(selected_db_name(a)?)?.status()?.resolved_path;
            if !std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                return Err(
                    "edit_body requires an existing database; initialize it explicitly".into(),
                );
            }
            h.require_current_generation()?;
            let result = prepared.inner.execute(&h.mem).await?;
            h.save()?;
            mutation_response(a, &h, result)
        }

        ToolRequestKind::EditSummary => {
            let prepared =
                prepared_summary_edit.expect("edit_summary admission retains its request");
            let h = db_with_expected_id(reg, a, request.expected_db_id)?;
            let path = reg.slot(selected_db_name(a)?)?.status()?.resolved_path;
            if !std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                return Err(
                    "edit_summary requires an existing database; initialize it explicitly".into(),
                );
            }
            h.require_current_generation()?;
            let result = prepared.inner.execute(&h.mem, h.db_id).await?;
            h.save()?;
            mutation_response(a, &h, result)
        }

        ToolRequestKind::Retag(_) => {
            let prepared = prepared_retag.expect("retag admission retains its request");
            let h = db_with_expected_id(reg, a, request.expected_db_id)?;
            let path = reg.slot(selected_db_name(a)?)?.status()?.resolved_path;
            if !std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                return Err("retag requires an existing database; initialize it explicitly".into());
            }
            h.require_current_generation()?;
            let result = prepared.inner.execute(&h.mem).await?;
            if result["changed"] == true {
                h.save()?;
            }
            mutation_response(a, &h, result)
        }
        ToolRequestKind::Concern(action) => {
            let prepared = prepared_concern.expect("concern admission retains its request");
            let h = db_with_expected_id(reg, a, request.expected_db_id)?;
            let is_mutation = matches!(
                action,
                concern::ConcernAction::Notice | concern::ConcernAction::RecordFinding
            );
            if is_mutation {
                let path = reg.slot(selected_db_name(a)?)?.status()?.resolved_path;
                if !std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                    return Err("concern mutation requires an existing current database; initialize it explicitly".into());
                }
                h.require_current_generation()?;
            }
            let store = h.concerns().ok_or(
                "selected database has no native concern support; use a matching current owner, not a SAVE fallback",
            )?;
            let mut result = prepared.execute(store).await?;
            if is_mutation {
                h.save()?;
            }
            result["db"] = json!(selected_db_name(a)?);
            result["db_id"] = json!(h.db_id.to_string());
            Ok(result)
        }

        ToolRequestKind::Save(_) => {
            // Admission generated a manual identity at most once, and this
            // retained request is the one whose authority was classified.
            let prepared = prepared_save.expect("SAVE admission retains its request");
            let h = db_with_expected_id(reg, a, request.expected_db_id)?;
            // Reference startup may register an in-memory empty snapshot for an
            // absent path. SAVE never turns that owner into an implicit create.
            let path = reg.slot(selected_db_name(a)?)?.status()?.resolved_path;
            if !std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                return Err(format!(
                    "save requires an existing database for {:?}; initialize it explicitly before saving",
                    selected_db_name(a)?,
                ).into());
            }
            h.require_current_generation()?;
            let commit = h.origin_commit();
            let result = prepared.run(&h.mem, h.db_id, commit.as_deref()).await?;
            h.save()?;
            mutation_response(a, &h, result)
        }

        ToolRequestKind::Capture(_) => {
            // Classification already parsed the complete envelope and rejected
            // absent authority before this checkout. Retain a typed value for
            // the execution path rather than probing raw JSON field-by-field.
            let prepared = capture::PreparedCapture::parse(a)?;
            let h = db_with_expected_id(reg, a, request.expected_db_id)?;
            h.require_current_generation()?;
            let commit = h.origin_commit();
            let outcome = prepared.run(&h.mem, commit.as_deref()).await?;
            // Snapshot stores require a checkpoint even on an exact replay.
            h.save()?;
            mutation_response(
                a,
                &h,
                json!({ "id": outcome.id.0.to_string(), "replayed": outcome.replayed }),
            )
        }

        ToolRequestKind::Episode(_) => {
            let prepared = episode::PreparedEpisode::parse(a)?;
            let is_mutation = prepared.is_mutation();
            let h = db_with_expected_id(reg, a, request.expected_db_id)?;
            if is_mutation {
                // Positive generation admission precedes body publication and
                // embedding. An episode operation never upgrades its store.
                h.require_current_generation()?;
            }
            let commit = is_mutation.then(|| h.origin_commit()).flatten();
            let result = prepared.run(&h.mem, h.db_id, commit.as_deref()).await?;
            if is_mutation {
                // JSON snapshots need the same durable checkpoint on an exact
                // source replay as on a newly committed edition.
                h.save()?;
            } else {
                reg.activity.returned(
                    "episode",
                    selected_db_name(a)?,
                    h.db_id,
                    episode::returned_ids(&result),
                );
            }
            // Both lanes identify their owner, rather than making historical
            // IDs ambiguous across configured databases.
            mutation_response(a, &h, result)
        }

        ToolRequestKind::Feedback => {
            let h = db(reg, a)?;
            let from = optional_string(a, "from")?.map(parse_id).transpose()?;
            let to = parse_id(req_s(a, "to")?)?;
            h.mem
                .apply_feedback(from, to, parse_signal(req_s(a, "signal")?)?)
                .await?;
            h.save()?;
            mutation_response(
                a,
                &h,
                json!({
                    "ok": true,
                    "scope": if from.is_some() { "node-and-route" } else { "node-only" },
                    "receipt_bound": false,
                }),
            )
        }

        ToolRequestKind::Forget => {
            let h = db(reg, a)?;
            let existed = h
                .mem
                .forget(ColdPath::acquire(), parse_id(req_s(a, "id")?)?)
                .await?;
            h.save()?;
            mutation_response(a, &h, json!({ "forgotten": existed }))
        }

        ToolRequestKind::Link(link_request) => {
            // Pin and guard the source before inspecting any remote target.
            let h = db(reg, a)?;
            let from = parse_id(req_s(a, "from")?)?;
            let to = parse_id(req_s(a, "to")?)?;
            // A `to_db` makes it a cross-db edge: `from` (here) -> `to` (there).
            if link_request == LinkRequest::Remote {
                let to_db = req_s(a, "to_db")?;
                let src_name = selected_db_name(a)?;
                if src_name != HOME_DB {
                    return Err(format!(
                        "cross-db edges may only originate from the {HOME_DB:?} db (keeps project dbs self-contained)"
                    )
                    .into());
                }
                if to_db == src_name {
                    return Err("to_db must be a different database than db".into());
                }
                let target = reg.checkout(to_db)?;
                if target.mem.get_node(to).await?.is_none() {
                    return Err(format!("target node {} not found in db {to_db:?}", to.0).into());
                }
                let target_db = target.db_id;
                let weight = optional_unit_interval(a, "weight")?.unwrap_or(0.5);
                let src = &h;
                src.mem.link_remote(from, target_db, to, weight).await?;
                src.save()?;
                return mutation_response(
                    a,
                    &src,
                    json!({
                        "ok": true,
                        "remote": true,
                        "to_db": to_db,
                        "target_db": target_db.to_string(),
                        "target_db_id": target_db.to_string(),
                    }),
                );
            }
            let kind = parse_kind(optional_string(a, "kind")?.unwrap_or("associative"))?;
            let weight = optional_unit_interval(a, "weight")?.unwrap_or(0.5);
            let anchor = optional_body_span(a)?;
            h.mem.link(from, to, kind, weight, anchor).await?;
            h.save()?;
            mutation_response(a, &h, json!({ "ok": true }))
        }

        ToolRequestKind::Supersede => {
            let h = db(reg, a)?;
            h.mem
                .supersede(
                    ColdPath::acquire(),
                    parse_id(req_s(a, "winner")?)?,
                    parse_id(req_s(a, "loser")?)?,
                )
                .await?;
            h.save()?;
            mutation_response(a, &h, json!({ "ok": true }))
        }

        ToolRequestKind::Contradict => {
            let h = db(reg, a)?;
            h.mem
                .observe_contradiction(
                    ColdPath::acquire(),
                    parse_id(req_s(a, "a")?)?,
                    parse_id(req_s(a, "b")?)?,
                )
                .await?;
            h.save()?;
            mutation_response(a, &h, json!({ "ok": true }))
        }

        ToolRequestKind::Contradictions => {
            let h = db(reg, a)?;
            let triage = h.mem.reconciliation_triage(ColdPath::acquire()).await?;
            Ok(json!({
                "contradictions": triage.contradictions.iter().map(|c| json!({
                    "a": c.between.0.0.to_string(),
                    "b": c.between.1.0.to_string(),
                    "observations": c.observations,
                    "clusters": [c.clusters.0.0, c.clusters.1.0],
                })).collect::<Vec<_>>(),
                "cluster_conflicts": triage.cluster_conflicts.iter().map(|cc| json!({
                    "clusters": [cc.clusters.0.0, cc.clusters.1.0],
                    "observations": cc.observations,
                    "pairs": cc.pairs,
                })).collect::<Vec<_>>(),
            }))
        }

        ToolRequestKind::Reconcile => {
            let h = db(reg, a)?;
            h.mem
                .reconcile(
                    ColdPath::acquire(),
                    parse_id(req_s(a, "a")?)?,
                    parse_id(req_s(a, "b")?)?,
                    parse_resolution(req_s(a, "resolution")?)?,
                )
                .await?;
            h.save()?;
            mutation_response(a, &h, json!({ "ok": true }))
        }

        ToolRequestKind::Merges => {
            let h = db(reg, a)?;
            let open = h.mem.open_merge_candidates(ColdPath::acquire()).await?;
            Ok(json!(
                open.iter()
                    .map(|m| json!({
                        "a": m.between.0.0.to_string(),
                        "b": m.between.1.0.to_string(),
                        "observations": m.observations,
                    }))
                    .collect::<Vec<_>>()
            ))
        }

        ToolRequestKind::Merge(merge_request) => match merge_request {
            MergeRequest::Full => {
                let h = db(reg, a)?;
                h.mem
                    .merge_full(
                        ColdPath::acquire(),
                        parse_id(req_s(a, "winner")?)?,
                        parse_id(req_s(a, "loser")?)?,
                    )
                    .await?;
                h.save()?;
                mutation_response(a, &h, json!({ "ok": true }))
            }
            MergeRequest::Keep => {
                let h = db(reg, a)?;
                h.mem
                    .resolve_merge(
                        ColdPath::acquire(),
                        parse_id(req_s(a, "a")?)?,
                        parse_id(req_s(a, "b")?)?,
                        MergeResolution::Keep,
                    )
                    .await?;
                h.save()?;
                mutation_response(a, &h, json!({ "ok": true }))
            }
        },

        ToolRequestKind::Recall => {
            let h = db(reg, a)?;
            let expand_top =
                optional_bounded_usize(a, "expand_top", 1, MAX_RECALL_EXPAND)?.unwrap_or(3);
            let neighbors_each =
                optional_bounded_usize(a, "neighbors_each", 1, MAX_NEIGHBORS_EACH)?.unwrap_or(5);
            let hits = h
                .mem
                .recall_expanded(
                    bounded_required(a, "text", MAX_QUERY_BYTES)?,
                    expand_top,
                    neighbors_each,
                )
                .await?;
            Ok(json!(
                hits.iter()
                    .map(|hit| {
                        let (summary, summary_truncated) =
                            bounded_summary(hit.node.summary(), MAX_SUMMARY_BYTES);
                        let neighbors = hit
                            .neighbors
                            .iter()
                            .map(|nb| {
                                let (summary, summary_truncated) =
                                    bounded_summary(nb.node.summary(), MAX_SUMMARY_BYTES);
                                json!({
                                    "id": nb.node.id().0.to_string(),
                                    "summary": summary,
                                    "summary_truncated": summary_truncated,
                                    "kind": kind_str(nb.kind),
                                    "incoming": nb.incoming,
                                    "weight": nb.weight,
                                })
                            })
                            .collect::<Vec<_>>();
                        json!({
                            "id": hit.node.id().0.to_string(),
                            "score": hit.score,
                            "status": status_str(hit.node.status()),
                            "summary": summary,
                            "summary_truncated": summary_truncated,
                            "neighbors": neighbors,
                        })
                    })
                    .collect::<Vec<_>>()
            ))
        }

        ToolRequestKind::Decay => {
            let h = db(reg, a)?;
            let r = h.mem.decay_sweep(ColdPath::acquire()).await?;
            h.save()?;
            mutation_response(
                a,
                &h,
                json!({
                    "edges_decayed": r.edges_decayed,
                    "edge_conflicts": r.edge_conflicts,
                    "edge_pages": r.edge_pages,
                }),
            )
        }

        ToolRequestKind::Prune => {
            let h = db(reg, a)?;
            let r = h.mem.prune_dense(ColdPath::acquire()).await?;
            h.save()?;
            mutation_response(
                a,
                &h,
                json!({
                    "pruned": r.pruned,
                    "capacity_pruned": r.capacity_pruned,
                    "weak_conflicts": r.weak_conflicts,
                    "contended_hubs": r.contended_hubs,
                    "edge_pages": r.edge_pages,
                    "hub_pages": r.hub_pages,
                    "chunks": r.chunks,
                }),
            )
        }

        ToolRequestKind::Walk(action) => {
            let mut sessions = sessions.lock().await;
            walk_tool(reg, &mut sessions, action, a).await
        }

        ToolRequestKind::Reflect => reflect_tool(reg, sessions.as_ref(), cold_work, a).await,
    }
}

/// Handler-level tests exercise storage semantics independently of an MCP
/// deployment profile. Public stdio and HTTP traffic never use this helper;
/// both enter through `Server::handle` and the authorized boundary above.
#[cfg(test)]
async fn call_tool(
    reg: &Registry,
    sessions: &std::sync::Arc<Mutex<SessionState>>,
    cold_work: &ColdWorkGate,
    name: &str,
    a: &Value,
) -> Result<Value, AnyErr> {
    call_tool_authorized(
        reg,
        sessions,
        cold_work,
        CapabilityPolicy::operator_with_direct_feedback(),
        name,
        a,
    )
    .await
}

fn database_status_json(name: &str, status: &DatabaseSlotStatus) -> Value {
    json!({
        "db": name,
        "name": name,
        "db_id": status.db_id.to_string(),
        "state": status.state,
        // JSON has no lossless representation for a non-UTF Unix path. Status
        // output is descriptive rather than an authority token, so render it
        // explicitly instead of letting `json!` panic during serialization.
        "configured_path": status.configured_path.to_string_lossy(),
        "resolved_path": status.resolved_path.to_string_lossy(),
        "in_flight": status.in_flight,
        "backend_jobs": status.backend_jobs,
    })
}

/// Explicit lease handoff for offline CLI maintenance. Holding session state
/// across the slot transition prevents a walk/receipt from appearing after the
/// quiescence check. Every ordinary operation holds a request-scoped checkout,
/// so the slot's strong-count fence covers concurrent reads and writes too.
async fn database_control_tool(
    reg: &Registry,
    sessions: &std::sync::Arc<Mutex<SessionState>>,
    cold_work: &ColdWorkGate,
    action: DatabaseControlRequest,
    a: &Value,
) -> Result<Value, AnyErr> {
    let name = selected_db_name(a)?;
    let slot = reg.slot(name)?;
    // This preliminary check precedes ledger cleanup; the guarded transition
    // rechecks under the slot lock before installing any lifecycle fence.
    require_expected_db_id(name, slot.status()?.db_id, optional_expected_db_id(a)?)?;
    let mut cold_admission = matches!(
        action,
        DatabaseControlRequest::Release | DatabaseControlRequest::Resume
    )
    .then(|| cold_work.admit(&format!("database_control {} {name}", action.as_str())))
    .transpose()?;
    let mut state = sessions.lock().await;
    purge_expired_sessions(&mut state.active);
    state.purge_expired_receipts();
    let session_status = state.database_status(name);

    match action {
        DatabaseControlRequest::Status => {
            let status = slot.status()?;
            require_expected_db_id(name, status.db_id, optional_expected_db_id(a)?)?;
            let mut value = database_status_json(name, &status);
            value["active_walks"] = json!(session_status.walks);
            value["live_receipts"] = json!(session_status.blocking_receipts);
            value["retry_tombstones"] = json!(session_status.retry_tombstones);
            Ok(value)
        }
        DatabaseControlRequest::Release => {
            if session_status.walks != 0 || session_status.blocking_receipts != 0 {
                return Err(format!(
                    "database {name:?} cannot enter maintenance: {} active walk(s), {} unconsumed or claimed receipt(s); finish/abort walks and reflect or let receipts expire",
                    session_status.walks, session_status.blocking_receipts,
                )
                .into());
            }
            // Install the checkout fence synchronously while session state is
            // locked, then move all blocking checkpoint/close work off Tokio.
            // The worker also owns the cold admission and post-release authority
            // rotation, so request cancellation cannot publish a half-handoff.
            let release = slot.begin_release_guarded(optional_expected_db_id(a)?)?;
            let worker_sessions = sessions.clone();
            let worker_db = name.to_owned();
            let worker_admission = cold_admission
                .take()
                .expect("release acquired cold-work admission");
            drop(state);
            let (status, purged_retry_tombstones) = tokio::task::spawn_blocking(move || {
                let _admission = worker_admission;
                let status = release.finish()?;

                // Old consumed receipt tokens are useful only for retries in
                // the old volatile authority epoch. Rotate that epoch first,
                // then discard only successful target-db tombstones.
                let mut state = worker_sessions.blocking_lock();
                state.rotate_feedback_epoch(&worker_db);
                let purged = state.purge_consumed_receipts(&worker_db);
                Ok::<_, AnyErr>((status, purged))
            })
            .await
            .map_err(|error| format!("join database release worker: {error}"))??;
            let mut value = database_status_json(name, &status);
            value["authority_rotated"] = json!(true);
            value["purged_retry_tombstones"] = json!(purged_retry_tombstones);
            Ok(value)
        }
        DatabaseControlRequest::Resume => {
            if session_status.walks != 0 || session_status.blocking_receipts != 0 {
                return Err(format!(
                    "database {name:?} has unexpected live session state while released: {} active walk(s), {} unconsumed or claimed receipt(s)",
                    session_status.walks, session_status.blocking_receipts,
                )
                .into());
            }
            let feedback_epoch = state.feedback_epoch(name);
            let resume = slot.begin_resume_guarded(feedback_epoch, optional_expected_db_id(a)?)?;
            let worker_admission = cold_admission
                .take()
                .expect("resume acquired cold-work admission");
            drop(state);
            let status = tokio::task::spawn_blocking(move || {
                let _admission = worker_admission;
                resume.finish()
            })
            .await
            .map_err(|error| format!("join database resume worker: {error}"))??;
            Ok(database_status_json(name, &status))
        }
    }
}

/// Snapshot admission shares the maintenance quiescence proof with release, but
/// the slot stays open after its cancellation-surviving blocking worker finishes.
async fn snapshot_create_tool(
    reg: &Registry,
    sessions: &std::sync::Arc<Mutex<SessionState>>,
    cold_work: &ColdWorkGate,
    a: &Value,
) -> Result<Value, AnyErr> {
    let name = selected_db_name(a)?;
    let slot = reg.slot(name)?;
    require_expected_db_id(name, slot.status()?.db_id, optional_expected_db_id(a)?)?;
    let admission = cold_work.admit(&format!("snapshot_create {name}"))?;
    let mut state = sessions.lock().await;
    purge_expired_sessions(&mut state.active);
    state.purge_expired_receipts();
    let status = state.database_status(name);
    if status.walks != 0 || status.blocking_receipts != 0 {
        return Err(format!(
            "database {name:?} cannot create snapshot: {} active walk(s), {} unconsumed or claimed receipt(s); finish/abort walks and reflect or let receipts expire",
            status.walks, status.blocking_receipts,
        ).into());
    }
    // Do not invalidate receipts if the slot rejects admission for an in-flight
    // operation, backend job, or maintenance state. The new epoch is committed
    // only after the slot installs its snapshot fence.
    let new_epoch = ulid::Ulid::new().to_string();
    let snapshot = slot.begin_snapshot_guarded(new_epoch.clone(), optional_expected_db_id(a)?)?;
    state.replace_feedback_epoch(name, new_epoch);
    let purged = state.purge_consumed_receipts(name);
    drop(state);
    let worker = tokio::task::spawn_blocking(move || {
        let _admission = admission;
        snapshot.finish()
    });
    let result = worker
        .await
        .map_err(|error| format!("join database snapshot worker: {error}"))??;
    Ok(json!({
        "db": name,
        "db_id": result.db_id.to_string(),
        "bundle": result.bundle.to_string_lossy(),
        "generation": result.generation.to_string(),
        "authority_rotated": true,
        "purged_retry_tombstones": purged,
    }))
}

/// The `repl` discipline over MCP: a constrained walk keyed by a session token,
/// over the shared [`mneme_walk::WalkSession`]. `start` mints the token and
/// returns the first view; every other action operates on the held session.
async fn walk_tool(
    reg: &Registry,
    state: &mut SessionState,
    action: WalkRequest,
    a: &Value,
) -> Result<Value, AnyErr> {
    if action == WalkRequest::Start {
        let h = db(reg, a)?;
        ensure_session_capacity(&mut state.active)?;
        let start = parse_id(req_s(a, "start")?)?;
        if h.mem.get_node(start).await?.is_none() {
            return Err("start node not found".into());
        }
        let budget = optional_bounded_usize(a, "budget", 1, MAX_WALK_BUDGET)?.unwrap_or(25);
        let dbname = selected_db_name(a)?.to_string();
        let mut session = WalkSession::new(start, budget);
        // Optional query: order this walk's edges by salience (relevance to the
        // query) instead of raw weight — the same signal the conditioned spread
        // uses, applied to a manual walk. Read-only; stored weights are untouched.
        if let Some(q) = optional_string(a, "query")? {
            validate_text_size("query", q, MAX_QUERY_BYTES)?;
            let rel = h.mem.query_relevance(q, 64).await?;
            session = session.with_query_relevance(rel);
        }
        let mut view = serde_json::to_value(session.view(&h.mem).await?)?;
        bound_summary_fields(&mut view);
        let token = fresh_state_token(&state.active);
        state.active.insert(
            token.clone(),
            McpSession {
                db: dbname,
                session,
                actions: 1,
                touched: Instant::now(),
            },
        );
        return Ok(
            json!({ "session": token, "view": view, "db": selected_db_name(a)?, "db_id": h.db_id.to_string() }),
        );
    }

    let token = req_s(a, "session")?.to_string();
    if !state.active.contains_key(&token) {
        return Err("unknown or finished session token".into());
    }

    // Guard the session's owner, never an omitted-read default. Even terminal
    // controls must reject a mismatched guard without consuming the walk.
    let session_db = state
        .active
        .get(&token)
        .expect("checked in same lock")
        .db
        .clone();
    let expected = optional_expected_db_id(a)?;
    let h = if expected.is_some() || !matches!(action, WalkRequest::Done | WalkRequest::Abort) {
        let h = reg.checkout(&session_db)?;
        require_expected_db_id(&session_db, h.db_id, expected)?;
        Some(h)
    } else {
        None
    };

    // Terminal controls bypass the action limit. `done` also checks and reserves
    // receipt capacity before removing the active walk, so a full receipt pool
    // leaves the session available for retry or abort.
    if action == WalkRequest::Done {
        let result = complete_walk(state, &token)?;
        return match h {
            Some(h) => owner_response(&session_db, h.db_id, result),
            None => Ok(result),
        };
    }
    if action == WalkRequest::Abort {
        let trail = serde_json::to_value(
            state
                .active
                .get(&token)
                .expect("validated in the same lock")
                .session
                .trail(),
        )?;
        state.active.remove(&token);
        let result = json!({ "trail": trail });
        return match h {
            Some(h) => owner_response(&session_db, h.db_id, result),
            None => Ok(result),
        };
    }

    let h = h.expect("nonterminal actions retain the session owner");
    let entry = state
        .active
        .get_mut(&token)
        .expect("validated in the same lock");
    admit_walk_action(entry)?;
    entry.touched = Instant::now();
    let s = &mut entry.session;
    let result = match action {
        WalkRequest::Look => {
            let mut value = serde_json::to_value(s.view(&h.mem).await?)?;
            bound_summary_fields(&mut value);
            Ok::<Value, AnyErr>(value)
        }
        WalkRequest::Edges => {
            let mut value = serde_json::to_value(s.edges(&h.mem).await?)?;
            bound_summary_fields(&mut value);
            Ok(json!({ "edges": value }))
        }
        WalkRequest::Body => {
            let offset = body_offset(a)?;
            let max_bytes = body_limit(a)?;
            let chunk = match h.mem.get_node(s.current()).await? {
                Some(node) => h.mem.resolve_body_range(&node, offset, max_bytes).await?,
                None => mneme_core::ports::BodyChunk {
                    bytes: Vec::new(),
                    source_start: offset,
                    source_end: offset,
                    next_offset: None,
                },
            };
            Ok(json!({
                "body": render_body_bytes(&chunk.bytes),
                "source_start": chunk.source_start,
                "source_end": chunk.source_end,
                "next_offset": chunk.next_offset,
                "has_more": chunk.next_offset.is_some(),
            }))
        }
        WalkRequest::Go => {
            ensure_walk_path_capacity(s)?;
            let mut value = serde_json::to_value(s.go(req_s(a, "to")?, &h.mem).await?)?;
            bound_summary_fields(&mut value);
            Ok(value)
        }
        WalkRequest::Back => {
            let mut value = serde_json::to_value(s.back(&h.mem).await?)?;
            bound_summary_fields(&mut value);
            Ok(value)
        }
        WalkRequest::Start | WalkRequest::Done | WalkRequest::Abort => {
            unreachable!("terminal and start actions return before session dispatch")
        }
    }?;
    owner_response(&session_db, h.db_id, result)
}

fn fresh_state_token<T>(map: &HashMap<String, T>) -> String {
    loop {
        let token = ulid::Ulid::new().to_string();
        if !map.contains_key(&token) {
            return token;
        }
    }
}

fn admit_walk_action(entry: &mut McpSession) -> Result<(), AnyErr> {
    if entry.actions >= MAX_WALK_ACTIONS {
        return Err(format!(
            "walk action budget exhausted ({MAX_WALK_ACTIONS} including start); done or abort remains available"
        )
        .into());
    }
    entry.actions += 1;
    Ok(())
}

fn ensure_walk_path_capacity(session: &WalkSession) -> Result<(), AnyErr> {
    if session.depth() >= MAX_WALK_PATH_DEPTH {
        return Err(format!(
            "walk path depth exhausted ({MAX_WALK_PATH_DEPTH}); use back, done, or abort"
        )
        .into());
    }
    Ok(())
}

/// Purge abandoned walks, then admit a new one only when a real slot exists.
/// Live sessions are never sacrificed to make a newer request succeed.
fn ensure_session_capacity(sessions: &mut HashMap<String, McpSession>) -> Result<(), AnyErr> {
    purge_expired_sessions(sessions);
    if sessions.len() >= MAX_SESSIONS {
        return Err(format!(
            "walk session capacity exceeded (limit {MAX_SESSIONS}); finish or abort an existing walk"
        )
        .into());
    }
    Ok(())
}

fn purge_expired_sessions(sessions: &mut HashMap<String, McpSession>) {
    let now = Instant::now();
    sessions.retain(|_, session| now.saturating_duration_since(session.touched) < SESSION_TTL);
}

fn reflection_judgments(a: &Value) -> Result<(Vec<NodeId>, HashSet<NodeId>), AnyErr> {
    let raw_used = strs(a, "used")?;
    let raw_unhelpful = strs(a, "unhelpful")?;
    if raw_used.len().saturating_add(raw_unhelpful.len()) > MAX_REFLECT_USED {
        return Err(
            format!("reflect accepts at most {MAX_REFLECT_USED} total judged nodes").into(),
        );
    }
    let mut used = Vec::new();
    for raw in raw_used {
        let id = parse_id(&raw)?;
        if !used.contains(&id) {
            used.push(id);
        }
    }
    let unhelpful = raw_unhelpful
        .iter()
        .map(|raw| parse_id(raw))
        .collect::<Result<HashSet<_>, _>>()?;
    if used.iter().any(|id| unhelpful.contains(id)) {
        return Err("reflect used and unhelpful nodes must be disjoint".into());
    }
    Ok((used, unhelpful))
}

/// Post-factum training from server-issued walk receipts. Receipts bind feedback
/// to paths the caller actually traversed; accepting arbitrary `{from,node}` pairs
/// here used to make `reflect` a disguised edge-poisoning endpoint.
async fn reflect_tool(
    reg: &Registry,
    state: &Mutex<SessionState>,
    cold_work: &ColdWorkGate,
    a: &Value,
) -> Result<Value, AnyErr> {
    if a.get("trail").is_some() {
        return Err("reflect no longer accepts caller-supplied trails; pass the receipt returned by walk done".into());
    }
    let receipt_ids = strs(a, "receipts")?;
    if receipt_ids.is_empty() || receipt_ids.len() > MAX_REFLECT_RECEIPTS {
        return Err(
            format!("reflect requires 1..={MAX_REFLECT_RECEIPTS} completed-walk receipts").into(),
        );
    }
    let (used_ids, unhelpful) = reflection_judgments(a)?;
    let used: HashSet<NodeId> = used_ids.iter().copied().collect();
    let db_name = selected_db_name(a)?;
    let h = db(reg, a)?;

    // Validate every receipt before claiming any of them, then claim as one
    // critical section. Storage binds the sorted receipt-set key to the exact
    // ordered path effects; the in-memory claim only prevents concurrent callers
    // from racing to choose different classifications for the same receipt.
    let claimed = {
        let mut state = state.lock().await;
        claim_walk_receipts(&mut state, &receipt_ids, db_name, &used, &unhelpful)?
    };
    let claim_guard = claimed.into_guard(state);

    let outcome: Result<Value, AnyErr> = async {
        let feedback = mneme_walk::reflect_observed_explicit_idempotent(
            &h.mem,
            claim_guard.idempotency_key(),
            claim_guard.retry_scope(),
            claim_guard.routes(),
            claim_guard.visited(),
            &used,
            &unhelpful,
        )
        .await?;
        let replayed = feedback.commit == FeedbackCommitOutcome::AlreadyApplied;

        // Consolidation/topology creation is deliberately outside the receipt
        // transaction. Its failure is reported but cannot make grounded feedback
        // replayable. A durable feedback replay does not run it again.
        let (bridges, consolidation_error) = if !replayed && used_ids.len() >= 2 {
            match consolidate_admitted(&h, cold_work, &used_ids).await {
                Ok(bridges) => (bridges, None),
                Err(error) => (Vec::new(), Some(error.to_string())),
            }
        } else {
            (Vec::new(), None)
        };
        // Snapshot hosts may have committed the in-memory graph+retry ledger but
        // failed their atomic checkpoint. Save even on `AlreadyApplied` so a retry
        // can finish that publication before the receipt is consumed.
        h.save()?;
        mutation_response(a, &h, json!({
            "feedback_commit": if replayed { "already_applied" } else { "applied" },
            "replayed": replayed,
            "reinforced": feedback.reinforced,
            "interfered": feedback.interfered,
            "bridged": bridges.len(),
            "bridges": bridges.iter().map(|(x, y)| json!([x.0.to_string(), y.0.to_string()])).collect::<Vec<_>>(),
            "consolidation_error": consolidation_error,
        }))
    }
    .await;

    let mut state = state.lock().await;
    claim_guard.finish(&mut state, outcome.is_ok());
    outcome
}

/// Consolidation is the conditional cold leg of `reflect`. Grounded feedback
/// stays outside this gate; only a call that can reach community detection is
/// admitted and mints the capability token.
async fn consolidate_admitted(
    h: &DbHandle,
    cold_work: &ColdWorkGate,
    used: &[NodeId],
) -> Result<Vec<(NodeId, NodeId)>, AnyErr> {
    // Installed defaults disable speculative bridges. Preserve that cheap no-op:
    // it neither mints a cold capability nor consumes another caller's lane.
    if used.len() < 2 || h.mem.config().bridge_probability <= 0.0 {
        return Ok(Vec::new());
    }
    let _admission = cold_work.admit("reflect consolidation")?;
    Ok(h.mem.consolidate(ColdPath::acquire(), used).await?)
}

/// One hard-bounded cross-database edge page. Resolution is opt-in because it
/// performs one bounded target lookup per returned edge; the default catalog
/// path returns exact edge records without an N+1 hydration bill.
async fn remote_page_json(
    reg: &Registry,
    h: &DbHandle,
    id: NodeId,
    after: Option<RemoteEdgeCursor>,
    limit: usize,
    resolve: bool,
) -> Result<Value, AnyErr> {
    let page = h.mem.remote_edges_page(id, after, limit).await?;
    let mut out = Vec::with_capacity(page.items.len());
    for edge in page.items {
        // Identity matching and checkout happen under one slot lock. A managed
        // selector refresh therefore cannot resolve an edge for old database A
        // against newly resumed database B under the same logical name.
        let target = reg.remote_target(edge.target_db)?;
        let (db_name, target) = match target {
            Some((name, checkout)) => (Some(name), checkout),
            None => (None, None),
        };
        let target_db_loaded = target.is_some();
        let summary = if resolve {
            match target {
                Some(handle) => handle.mem.get_node(edge.target).await?.map(|node| {
                    let (summary, truncated) = bounded_summary(node.summary(), MAX_SUMMARY_BYTES);
                    (summary.to_owned(), truncated)
                }),
                None => None,
            }
        } else {
            None
        };
        let summary_resolved = summary.is_some();
        let (summary, summary_truncated) = summary.unzip();
        out.push(json!({
            "target_db": edge.target_db.to_string(),
            "db": db_name,
            "target": edge.target.0.to_string(),
            "summary": summary,
            "summary_truncated": summary_truncated.unwrap_or(false),
            "weight": edge.weight(),
            "target_db_loaded": target_db_loaded,
            "summary_resolved": summary_resolved,
        }));
    }
    let returned = out.len();
    Ok(json!({
        "items": out,
        "next": page.next,
        "has_more": page.next.is_some(),
        "returned": returned,
        "summaries_requested": resolve,
    }))
}

async fn neighbors_json(h: &DbHandle, id: NodeId, limit: usize) -> Result<Value, AnyErr> {
    let read_limit = limit
        .checked_add(1)
        .ok_or("neighbor result limit overflow")?;
    let mut neighbors = h.mem.neighbors_hydrated(id, read_limit).await?;
    let has_more = neighbors.len() > limit;
    neighbors.truncate(limit);
    let mut out = Vec::with_capacity(neighbors.len());
    for hydrated in neighbors {
        let n = hydrated.neighbor;
        let summary = hydrated
            .node
            .as_ref()
            .map(|node| {
                let (summary, truncated) = bounded_summary(node.summary(), MAX_SUMMARY_BYTES);
                (summary.to_owned(), truncated)
            })
            .unwrap_or_default();
        out.push(json!({
            "neighbor": n.node.0.to_string(),
            "summary": summary.0,
            "summary_truncated": summary.1,
            "kind": kind_str(n.edge.kind),
            "incoming": n.incoming,
            "weight": n.edge.weight(),
            "anchor": n.edge.anchor.map(|s| json!([s.start, s.end])),
        }));
    }
    let returned = out.len();
    Ok(json!({
        "items": out,
        "returned": returned,
        "has_more": has_more,
    }))
}

fn context_presentation_budget(max_nodes: usize) -> PresentationBudget {
    // This is a per-request partial chunk, not a relevance/completeness quota.
    let primary_items = max_nodes.clamp(1, MAX_QUERY_NODES) as u16;
    let empty = LaneLimit::new(0, 0, 0, 0).expect("empty lane limit is valid");
    let primary = LaneLimit::new(1, primary_items, primary_items, DEFAULT_CONTEXT_BYTES)
        .expect("primary context lane limit is valid");
    PresentationBudget::new(
        NonZeroU32::new(DEFAULT_CONTEXT_BYTES).expect("context byte limit is nonzero"),
        NonZeroU32::new(CONTEXT_CONTROL_RESERVE_BYTES).expect("context control reserve is nonzero"),
        NonZeroU16::new(primary_items * 2).expect("context item limit is nonzero"),
        NonZeroU16::new(MAX_SUMMARY_BYTES as u16).expect("summary byte limit is nonzero"),
        BodyBudget::disabled(),
        LaneBudgets::new(empty, primary, empty).with_episodic(
            LaneLimit::new(
                0,
                2.min(primary_items),
                primary_items,
                DEFAULT_CONTEXT_BYTES,
            )
            .expect("episodic context lane limit is valid"),
        ),
    )
    .expect("the fixed MCP context budget is valid")
}

fn mcp_query_hit(hit: RetrievalHit) -> QueryHit {
    let (summary, summary_truncated) = bounded_summary(hit.node.summary(), MAX_SUMMARY_BYTES);
    let status = match hit.node.status() {
        NodeStatus::Active => QueryNodeStatus::Active,
        NodeStatus::Archived => QueryNodeStatus::Archived,
    };
    QueryHit::new(
        hit.node.id(),
        hit.lane_rank,
        mcp_rank_evidence(hit.evidence),
        status,
        summary,
        summary_truncated,
        QueryBody::NotRequested,
    )
}

fn mcp_rank_evidence(evidence: RetrievalEvidence) -> RankEvidence {
    RankEvidence::new(
        evidence.dense_rank,
        evidence.sparse_rank,
        evidence.graph_rank,
        evidence.rerank_rank,
    )
}

// ---- arg helpers ------------------------------------------------------------

/// Proof that one public tool call has passed the complete schema, domain, and
/// conditional-variant boundary. Keep this tiny wrapper even though handlers
/// mostly consume the original JSON: the exhaustive retained `kind` is the
/// one action-sensitive classification hook for dispatch and capability policy.
/// SAVE retains its admitted payload so a generated identity cannot drift;
/// concern keeps the exact checked action/expectation through atomic execution.
struct ValidatedToolArguments<'a> {
    raw: &'a Value,
    kind: ToolRequestKind,
    // Routing metadata stays outside the shared domain payload and its source
    // digest. Parsing it here also precedes capability/cold/checkout admission.
    expected_db_id: Option<ulid::Ulid>,
    prepared_save: Option<save::PreparedSave>,
    prepared_concern: Option<concern::PreparedConcern>,
    prepared_retag: Option<retag::PreparedRetag>,
    prepared_body_edit: Option<edit_body::PreparedBodyEdit>,
    prepared_summary_edit: Option<edit_summary::PreparedSummaryEdit>,
}

impl<'a> ValidatedToolArguments<'a> {
    fn parse(name: &str, raw: &'a Value) -> Result<Self, AnyErr> {
        validate_tool_arguments(name, raw)?;
        let prepared_save = (name == "save")
            .then(|| save::PreparedSave::parse(raw))
            .transpose()?;
        let prepared_concern = (name == "concern")
            .then(|| concern::PreparedConcern::parse(raw))
            .transpose()?;
        let prepared_retag = (name == "retag")
            .then(|| retag::PreparedRetag::parse(raw))
            .transpose()?;
        let prepared_body_edit = (name == "edit_body")
            .then(|| edit_body::PreparedBodyEdit::parse(raw))
            .transpose()?;
        let prepared_summary_edit = (name == "edit_summary")
            .then(|| edit_summary::PreparedSummaryEdit::parse(raw))
            .transpose()?;
        let kind = if prepared_summary_edit.is_some() {
            ToolRequestKind::EditSummary
        } else if prepared_body_edit.is_some() {
            ToolRequestKind::EditBody
        } else if let Some(prepared) = &prepared_retag {
            ToolRequestKind::Retag(if prepared.inner.requires_operator() {
                IngestRequest::Core
            } else {
                IngestRequest::Ordinary
            })
        } else {
            classify_tool_request(name, raw, prepared_save.as_ref(), prepared_concern.as_ref())?
        };
        if kind.requires_explicit_db() && raw.get("db").is_none() {
            return Err("missing required argument `db`".into());
        }
        let expected_db_id = optional_expected_db_id(raw)?;
        Ok(Self {
            raw,
            kind,
            expected_db_id,
            prepared_save,
            prepared_concern,
            prepared_retag,
            prepared_body_edit,
            prepared_summary_edit,
        })
    }

    const fn arguments(&self) -> &'a Value {
        self.raw
    }

    const fn kind(&self) -> ToolRequestKind {
        self.kind
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DatabaseControlRequest {
    Status,
    Release,
    Resume,
}

impl DatabaseControlRequest {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Release => "release",
            Self::Resume => "resume",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkRequest {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MergeRequest {
    Full,
    Keep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WalkRequest {
    Start,
    Look,
    Edges,
    Body,
    Go,
    Back,
    Done,
    Abort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IngestRequest {
    Ordinary,
    Core,
}

/// Minimum ambient authority required by one fully parsed public action.
///
/// Keep the mapping exhaustive: adding a tool or conditional action must make
/// this file fail to compile until its authority is deliberately classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CapabilityClass {
    ReadOnly,
    ReceiptGrounded,
    Curator,
    Operator,
}

impl CapabilityClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ReceiptGrounded => "receipt-grounded",
            Self::Curator => "curator",
            Self::Operator => "operator",
        }
    }
}

/// Exhaustive public request taxonomy. Mode-dependent tools retain the parsed
/// variant, so authorization never needs to infer authority from untrusted raw
/// JSON independently of the admission parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolRequestKind {
    Databases,
    Activity { after: u64, limit: usize },
    DatabaseControl(DatabaseControlRequest),
    SnapshotCreate,
    Status,
    Query,
    RecallContext,
    Recall,
    Get,
    List,
    Graph,
    Neighbors,
    RemoteEdges,
    Core,
    Ingest(IngestRequest),
    Capture(IngestRequest),
    Retag(IngestRequest),
    EditBody,
    EditSummary,
    Save(IngestRequest),
    Concern(concern::ConcernAction),
    Episode(EpisodeAction),
    Feedback,
    Forget,
    Link(LinkRequest),
    Supersede,
    Contradict,
    Contradictions,
    Reconcile,
    Merges,
    Merge(MergeRequest),
    Decay,
    Prune,
    Walk(WalkRequest),
    Reflect,
}

impl ToolRequestKind {
    const fn tool_name(self) -> &'static str {
        match self {
            Self::Databases => "databases",
            Self::Activity { .. } => "activity",
            Self::DatabaseControl(_) => "database_control",
            Self::SnapshotCreate => "snapshot_create",
            Self::Status => "status",
            Self::Query => "query",
            Self::RecallContext => "recall_context",
            Self::Recall => "recall",
            Self::Get => "get",
            Self::List => "list",
            Self::Graph => "graph",
            Self::Neighbors => "neighbors",
            Self::RemoteEdges => "remote_edges",
            Self::Core => "core",
            Self::Ingest(_) => "ingest",
            Self::Capture(_) => "capture",
            Self::Retag(_) => "retag",
            Self::EditBody => "edit_body",
            Self::EditSummary => "edit_summary",
            Self::Save(_) => "save",
            Self::Concern(_) => "concern",
            Self::Episode(_) => "episode",
            Self::Feedback => "feedback",
            Self::Forget => "forget",
            Self::Link(_) => "link",
            Self::Supersede => "supersede",
            Self::Contradict => "contradict",
            Self::Contradictions => "contradictions",
            Self::Reconcile => "reconcile",
            Self::Merges => "merges",
            Self::Merge(_) => "merge",
            Self::Decay => "decay",
            Self::Prune => "prune",
            Self::Walk(_) => "walk",
            Self::Reflect => "reflect",
        }
    }

    const fn capability_class(self) -> CapabilityClass {
        match self {
            Self::Databases
            | Self::Activity { .. }
            | Self::Status
            | Self::Query
            | Self::RecallContext
            | Self::Recall
            | Self::Get
            | Self::List
            | Self::Graph
            | Self::Neighbors
            | Self::RemoteEdges
            | Self::Core
            | Self::Contradictions
            | Self::Merges => CapabilityClass::ReadOnly,
            Self::DatabaseControl(action) => match action {
                DatabaseControlRequest::Status => CapabilityClass::ReadOnly,
                DatabaseControlRequest::Release | DatabaseControlRequest::Resume => {
                    CapabilityClass::Operator
                }
            },
            Self::SnapshotCreate | Self::EditBody | Self::EditSummary => CapabilityClass::Operator,
            Self::Concern(action) => match action {
                concern::ConcernAction::List => CapabilityClass::ReadOnly,
                concern::ConcernAction::Notice | concern::ConcernAction::RecordFinding => {
                    CapabilityClass::Curator
                }
            },
            Self::Walk(action) => match action {
                WalkRequest::Start
                | WalkRequest::Look
                | WalkRequest::Edges
                | WalkRequest::Body
                | WalkRequest::Go
                | WalkRequest::Back
                | WalkRequest::Done
                | WalkRequest::Abort => CapabilityClass::ReadOnly,
            },
            Self::Reflect => CapabilityClass::ReceiptGrounded,
            Self::Retag(action)
            | Self::Ingest(action)
            | Self::Capture(action)
            | Self::Save(action) => match action {
                IngestRequest::Ordinary => CapabilityClass::Curator,
                IngestRequest::Core => CapabilityClass::Operator,
            },
            Self::Episode(action) => match action.capability() {
                EpisodeCapability::ReadOnly => CapabilityClass::ReadOnly,
                EpisodeCapability::Curator => CapabilityClass::Curator,
                EpisodeCapability::Operator => CapabilityClass::Operator,
            },
            Self::Link(action) => match action {
                LinkRequest::Local | LinkRequest::Remote => CapabilityClass::Curator,
            },
            Self::Contradict => CapabilityClass::Curator,
            Self::Feedback
            | Self::Forget
            | Self::Supersede
            | Self::Reconcile
            | Self::Decay
            | Self::Prune => CapabilityClass::Operator,
            Self::Merge(action) => match action {
                MergeRequest::Full | MergeRequest::Keep => CapabilityClass::Operator,
            },
        }
    }

    const fn has_request_database_scope(self) -> bool {
        !matches!(
            self,
            Self::Databases
                | Self::Activity { .. }
                | Self::Walk(
                    WalkRequest::Look
                        | WalkRequest::Edges
                        | WalkRequest::Body
                        | WalkRequest::Go
                        | WalkRequest::Back
                        | WalkRequest::Done
                        | WalkRequest::Abort
                )
        )
    }

    const fn requires_explicit_db(self) -> bool {
        matches!(
            self,
            Self::DatabaseControl(DatabaseControlRequest::Release | DatabaseControlRequest::Resume)
                | Self::SnapshotCreate
                | Self::Ingest(_)
                | Self::Capture(_)
                | Self::Save(_)
                | Self::Retag(_)
                | Self::EditBody
                | Self::EditSummary
                | Self::Concern(
                    concern::ConcernAction::Notice | concern::ConcernAction::RecordFinding
                )
                | Self::Episode(EpisodeAction::Append | EpisodeAction::Revise)
                | Self::Feedback
                | Self::Forget
                | Self::Link(_)
                | Self::Supersede
                | Self::Contradict
                | Self::Reconcile
                | Self::Merge(_)
                | Self::Decay
                | Self::Prune
                | Self::Reflect
        )
    }
}

/// Fully validated hot-query admission. Construction happens before database
/// checkout so malformed optional values cannot acquire a lease, initialize an
/// embedder, or reach a backend. Caller-supplied budgets are rejected outside
/// the public range; only trusted configured defaults are clamped.
struct PublicQueryInput<'a> {
    text: &'a str,
    k: Option<usize>,
    depth: Option<u8>,
    max_nodes: Option<usize>,
    min_relevance: Option<f32>,
    tags: Vec<String>,
    archived: bool,
}

impl<'a> PublicQueryInput<'a> {
    fn parse(arguments: &'a Value, lifecycle_flags: bool) -> Result<Self, AnyErr> {
        let text = bounded_required(arguments, "text", MAX_QUERY_BYTES)?;
        let k = optional_bounded_usize(arguments, "k", 1, MAX_QUERY_K)?;
        let depth = optional_bounded_usize(arguments, "depth", 0, usize::from(MAX_QUERY_DEPTH))?
            .map(|depth| u8::try_from(depth).expect("validated query depth fits u8"));
        let max_nodes = optional_bounded_usize(arguments, "max_nodes", 1, MAX_QUERY_NODES)?;
        let min_relevance = optional_unit_interval(arguments, "min_relevance")?;
        let tags = strs(arguments, "tags")?;
        validate_tags(&tags)?;
        let archived = if lifecycle_flags {
            optional_bool(arguments, "archived")?
        } else {
            false
        };
        Ok(Self {
            text,
            k,
            depth,
            max_nodes,
            min_relevance,
            tags,
            archived,
        })
    }

    const fn status_filter(&self) -> StatusFilter {
        StatusFilter {
            active: true,
            archived: self.archived,
        }
    }
}

fn optional_bounded_usize(
    arguments: &Value,
    key: &str,
    minimum: usize,
    maximum: usize,
) -> Result<Option<usize>, AnyErr> {
    let Some(value) = arguments.get(key) else {
        return Ok(None);
    };
    let raw = value
        .as_u64()
        .ok_or_else(|| format!("argument `{key}` must be an integer"))?;
    let minimum = u64::try_from(minimum).expect("public query minimum fits u64");
    let maximum = u64::try_from(maximum).expect("public query maximum fits u64");
    if !(minimum..=maximum).contains(&raw) {
        return Err(format!("argument `{key}` must be in {minimum}..={maximum}").into());
    }
    Ok(Some(
        usize::try_from(raw).expect("validated public query integer fits usize"),
    ))
}

fn optional_unit_interval(arguments: &Value, key: &str) -> Result<Option<f32>, AnyErr> {
    let Some(value) = arguments.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_f64()
        .ok_or_else(|| format!("argument `{key}` must be a number"))?;
    Ok(Some(unit_interval(key, value)?))
}

fn optional_bool(arguments: &Value, key: &str) -> Result<bool, AnyErr> {
    match arguments.get(key) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| format!("argument `{key}` must be a boolean").into()),
    }
}

fn optional_string<'a>(arguments: &'a Value, key: &str) -> Result<Option<&'a str>, AnyErr> {
    match arguments.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| format!("argument `{key}` must be a string").into()),
    }
}

fn optional_body_span(arguments: &Value) -> Result<Option<BodySpan>, AnyErr> {
    let start = optional_u32(arguments, "anchor_start")?;
    let end = optional_u32(arguments, "anchor_end")?;
    match (start, end) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => {
            Err("anchor_start and anchor_end must be supplied together".into())
        }
        (Some(start), Some(end)) if start > end => {
            Err("anchor_start must not exceed anchor_end".into())
        }
        (Some(start), Some(end)) => Ok(Some(BodySpan::new(start, end))),
    }
}

fn optional_u32(arguments: &Value, key: &str) -> Result<Option<u32>, AnyErr> {
    let Some(value) = arguments.get(key) else {
        return Ok(None);
    };
    let raw = value
        .as_u64()
        .ok_or_else(|| format!("argument `{key}` must be a non-negative integer"))?;
    u32::try_from(raw)
        .map(Some)
        .map_err(|_| format!("argument `{key}` must be in 0..={}", u32::MAX).into())
}

fn db(reg: &Registry, a: &Value) -> Result<DatabaseCheckout, AnyErr> {
    db_with_expected_id(reg, a, optional_expected_db_id(a)?)
}

fn optional_expected_db_id(a: &Value) -> Result<Option<ulid::Ulid>, AnyErr> {
    let Some(raw) = a.get("expected_db_id") else {
        return Ok(None);
    };
    let raw = raw
        .as_str()
        .ok_or("argument `expected_db_id` must be a canonical uppercase ULID string")?;
    if raw.len() != 26 {
        return Err("argument `expected_db_id` must be a canonical 26-byte uppercase ULID".into());
    }
    let parsed = raw.parse::<ulid::Ulid>().ok();
    match parsed {
        Some(id) if id.to_string() == raw => Ok(Some(id)),
        _ => Err("argument `expected_db_id` must be a canonical 26-byte uppercase ULID".into()),
    }
}

fn db_with_expected_id(
    reg: &Registry,
    a: &Value,
    expected: Option<ulid::Ulid>,
) -> Result<DatabaseCheckout, AnyErr> {
    // Retain the named checkout before comparing. Its lease fences release and
    // resume through all subsequent generation checks, reads, and mutations.
    // Never search another registry alias to satisfy a stale identity binding.
    let h = reg.checkout(selected_db_name(a)?)?;
    require_expected_db_id(selected_db_name(a)?, h.db_id, expected)?;
    Ok(h)
}

fn require_expected_db_id(
    name: &str,
    actual: ulid::Ulid,
    expected: Option<ulid::Ulid>,
) -> Result<(), AnyErr> {
    if let Some(expected) = expected
        && actual != expected
    {
        return Err(format!(
            "expected_db_id mismatch for database {name:?}: expected {expected}, found {actual}; target changed, do not retry against a replacement"
        ).into());
    }
    Ok(())
}

fn selected_db_name(a: &Value) -> Result<&str, AnyErr> {
    match a.get("db") {
        None => Err("internal database scope was not resolved".into()),
        Some(Value::String(name)) => Ok(name),
        Some(_) => Err("argument `db` must be a string".into()),
    }
}

fn mutation_response(a: &Value, h: &DatabaseCheckout, payload: Value) -> Result<Value, AnyErr> {
    owner_response(req_s(a, "db")?, h.db_id, payload)
}

fn owner_response(db: &str, db_id: ulid::Ulid, mut payload: Value) -> Result<Value, AnyErr> {
    let object = payload
        .as_object_mut()
        .ok_or("internal mutation result must be a JSON object")?;
    object.insert("db".into(), json!(db));
    object.insert("db_id".into(), json!(db_id.to_string()));
    Ok(payload)
}

fn req_s<'a>(a: &'a Value, k: &str) -> Result<&'a str, AnyErr> {
    optional_string(a, k)?.ok_or_else(|| format!("missing required argument `{k}`").into())
}
fn strs(a: &Value, k: &str) -> Result<Vec<String>, AnyErr> {
    let Some(value) = a.get(k) else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| format!("{k} must be an array of strings"))?;
    array
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .map(String::from)
                .ok_or_else(|| format!("{k}[{index}] must be a string").into())
        })
        .collect()
}

fn bounded_required<'a>(a: &'a Value, key: &str, max_bytes: usize) -> Result<&'a str, AnyErr> {
    let value = req_s(a, key)?;
    validate_text_size(key, value, max_bytes)?;
    if value.trim().is_empty() {
        return Err(format!("{key} may not be blank").into());
    }
    Ok(value)
}

fn validate_text_size(label: &str, value: &str, max_bytes: usize) -> Result<(), AnyErr> {
    if value.len() > max_bytes {
        return Err(format!(
            "{label} is {} UTF-8 bytes; maximum is {max_bytes}",
            value.len()
        )
        .into());
    }
    Ok(())
}

fn validate_tags(tags: &[String]) -> Result<(), AnyErr> {
    if tags.len() > MAX_TAGS {
        return Err(format!("tags has {} entries; maximum is {MAX_TAGS}", tags.len()).into());
    }
    let mut unique = HashSet::new();
    for (index, tag) in tags.iter().enumerate() {
        if tag.is_empty() || tag.trim() != tag || tag.chars().any(char::is_control) {
            return Err(format!(
                "tags[{index}] must be nonblank, trimmed, and contain no controls"
            )
            .into());
        }
        if tag.len() > MAX_TAG_BYTES {
            return Err(format!(
                "tags[{index}] is {} UTF-8 bytes; maximum is {MAX_TAG_BYTES}",
                tag.len()
            )
            .into());
        }
        if !unique.insert(tag) {
            return Err(format!("duplicate tag {tag:?}").into());
        }
    }
    Ok(())
}

fn unit_interval(label: &str, value: f64) -> Result<f32, AnyErr> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(format!("{label} must be finite and between 0 and 1").into());
    }
    // These probabilities and weights are f32 throughout the durable domain.
    // JSON numbers arrive as f64, so ordinary decimal inputs (for example 0.1)
    // are deliberately rounded once to that domain representation. Unlike an
    // integer `as` cast, the range check above makes this finite and bounded.
    // Preserve the endpoint category as well: underflowing a positive value to
    // zero or rounding a below-one value to one can change threshold/lifecycle
    // behavior. Requiring a completely exact f64 round trip, however, would
    // reject nearly every useful JSON decimal without preserving any additional
    // storage precision.
    let quantized = value as f32;
    if (value > 0.0 && quantized == 0.0) || (value < 1.0 && quantized == 1.0) {
        return Err(format!(
            "{label} is too close to a unit-interval endpoint for finite f32 storage"
        )
        .into());
    }
    Ok(quantized)
}

fn body_limit(a: &Value) -> Result<usize, AnyErr> {
    let limit = match a.get("max_body_bytes") {
        None => DEFAULT_BODY_BYTES,
        Some(value) => value
            .as_u64()
            .and_then(|number| usize::try_from(number).ok())
            .ok_or("max_body_bytes must be a positive integer")?,
    };
    if !(1..=MAX_BODY_BYTES).contains(&limit) {
        return Err(format!("max_body_bytes must be in 1..={MAX_BODY_BYTES}").into());
    }
    Ok(limit)
}

fn body_offset(a: &Value) -> Result<u64, AnyErr> {
    match a.get("body_offset") {
        None => Ok(0),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "body_offset must be a non-negative integer".into()),
    }
}

fn remote_page_limit(a: &Value) -> Result<usize, AnyErr> {
    let limit = match a.get("limit") {
        None => 32,
        Some(value) => value
            .as_u64()
            .and_then(|number| usize::try_from(number).ok())
            .ok_or("remote edge limit must be a positive integer")?,
    };
    if !(1..=MAX_REMOTE_EDGE_PAGE_SIZE).contains(&limit) {
        return Err(format!("remote edge limit must be in 1..={MAX_REMOTE_EDGE_PAGE_SIZE}").into());
    }
    Ok(limit)
}

fn remote_cursor(a: &Value) -> Result<Option<RemoteEdgeCursor>, AnyErr> {
    match a.get("after") {
        None => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| format!("invalid remote edge cursor: {error}").into()),
    }
}

/// Render an already source-bounded body chunk without allowing malformed
/// UTF-8 to amplify the response beyond the bytes its cursor consumed.
///
/// Valid runs are copied byte-for-byte. Each malformed sequence is represented
/// by one ASCII `?`; an incomplete sequence at the end of the chunk is one
/// malformed suffix, regardless of how many of its bytes arrived. Thus the
/// rendered byte count never exceeds `bytes.len()`, while the body range's
/// `source_end` and `next_offset` continue to describe exact source progress.
fn render_body_bytes(bytes: &[u8]) -> String {
    let mut rendered = String::with_capacity(bytes.len());
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                rendered.push_str(valid);
                break;
            }
            Err(error) => {
                let valid_end = error.valid_up_to();
                let (valid, invalid_and_tail) = remaining.split_at(valid_end);
                rendered.push_str(
                    std::str::from_utf8(valid)
                        .expect("Utf8Error::valid_up_to identifies a valid UTF-8 prefix"),
                );
                rendered.push('?');

                let invalid_bytes = error.error_len().unwrap_or(invalid_and_tail.len());
                remaining = &invalid_and_tail[invalid_bytes..];
            }
        }
    }
    debug_assert!(rendered.len() <= bytes.len());
    rendered
}

/// Bound trusted UTF-8 summary text by encoded bytes without ever splitting a
/// scalar. The returned flag describes omission from the original summary, not
/// merely whether the chosen endpoint happened to be a character boundary.
fn bounded_summary(summary: &str, limit: usize) -> (&str, bool) {
    if summary.len() <= limit {
        return (summary, false);
    }

    let mut end = limit;
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    (&summary[..end], true)
}

fn bound_summary_fields(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                bound_summary_fields(value);
            }
        }
        Value::Object(fields) => {
            let bounded = fields
                .get("summary")
                .and_then(Value::as_str)
                .map(|summary| {
                    let (summary, truncated) = bounded_summary(summary, MAX_SUMMARY_BYTES);
                    (summary.to_owned(), truncated)
                });
            if let Some((summary, truncated)) = bounded {
                fields.insert("summary".into(), Value::String(summary));
                fields.insert("summary_truncated".into(), Value::Bool(truncated));
            }
            for value in fields.values_mut() {
                bound_summary_fields(value);
            }
        }
        _ => {}
    }
}

fn parse_id(s: &str) -> Result<NodeId, AnyErr> {
    ulid::Ulid::from_string(s)
        .map(NodeId)
        .map_err(|e| format!("invalid node id {s:?}: {e}").into())
}

fn parse_signal(s: &str) -> Result<Signal, AnyErr> {
    match s {
        "relevant" => Ok(Signal::RelevantNew),
        "not-new" => Ok(Signal::NotNew),
        "irrelevant" => Ok(Signal::Irrelevant),
        other => Err(format!("signal must be relevant|not-new|irrelevant, got {other:?}").into()),
    }
}

fn parse_resolution(s: &str) -> Result<Resolution, AnyErr> {
    match s {
        "context-dependent" => Ok(Resolution::ContextDependent),
        "unresolved" => Ok(Resolution::Unresolved),
        "superseded" => {
            Err("for a real supersession use the `supersede` tool (it emits the edge + archives the loser)".into())
        }
        other => Err(format!("resolution must be context-dependent|unresolved, got {other:?}").into()),
    }
}

fn parse_kind(s: &str) -> Result<EdgeKind, AnyErr> {
    match s {
        "associative" => Ok(EdgeKind::Associative),
        "transition" => Ok(EdgeKind::Transition),
        "supersedes" => Ok(EdgeKind::Supersedes),
        "derived_from" => Ok(EdgeKind::DerivedFrom),
        other => Err(format!(
            "kind must be associative|transition|supersedes|derived_from, got {other:?}"
        )
        .into()),
    }
}

fn node_json(n: &Node) -> Value {
    let (summary, summary_truncated) = bounded_summary(n.summary(), MAX_SUMMARY_BYTES);
    json!({
        "id": n.id().0.to_string(),
        "summary": summary,
        "summary_truncated": summary_truncated,
        "status": status_str(n.status()),
        "memory_kind": n.memory_kind(),
        "stability": n.stability(),
        "confidence": n.confidence(),
        "tags": n.tags().collect::<Vec<_>>(),
        "body_ownership": n.body_ownership().as_str(),
        "body_revision": n.body_revision().to_string(),
        // Temporal metadata keeps system-controlled exposure separate from
        // explicitly grounded use. Optional timestamps are unix millis and null
        // until the corresponding event first occurs.
        "created": n.created() as u64,
        "last_exposed": n.last_exposed().map(|timestamp| timestamp as u64),
        "exposure_count": n.exposure_count(),
        "last_grounded_use": n.last_grounded_use().map(|timestamp| timestamp as u64),
        "grounded_use_count": n.grounded_use_count(),
        "origin_commit": n.origin_commit(),
        "provenance": match n.provenance() {
            Provenance::Web { url, fetched } => json!({ "type": "web", "url": url.as_str(), "fetched": *fetched as u64 }),
            Provenance::Conversation { session, turn } => json!({ "type": "conversation", "session": session.to_string(), "turn": turn }),
            Provenance::External { source } => json!({
                "type": "external",
                "source": {
                    "namespace": source.namespace(),
                    "key": source.key(),
                    "reference": source.reference(),
                    "session": source.session(),
                    "revision": source.revision(),
                    "request_digest_sha256": source.request_digest().iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
                    "request_codec": source.request_codec(),
                },
            }),
            Provenance::Derived { from } => json!({ "type": "derived", "from": from.iter().map(|id| id.0.to_string()).collect::<Vec<_>>() }),
        },
    })
}

// ---- tool schemas -----------------------------------------------------------

fn tool(name: &str, desc: &str, mut props: Value, required: &[&str]) -> Value {
    if props.get("db").is_some() {
        props["expected_db_id"] = expected_db_id_prop();
    }
    let mut required = required.to_vec();
    if mutation_requires_explicit_db(name)
        && name != "database_control"
        && !required.contains(&"db")
    {
        required.push("db");
    }
    let mut schema = json!({
        "name": name,
        "description": desc,
        "inputSchema": {
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false,
        },
    });
    if name == "database_control" {
        schema["inputSchema"]["allOf"] = json!([{
            "if": {
                "properties": { "action": { "enum": ["release", "resume"] } },
                "required": ["action"],
            },
            "then": { "required": ["db"] },
        }]);
    }
    schema
}

fn forbid_arguments(names: &[&str]) -> Value {
    json!({
        "not": {
            "anyOf": names
                .iter()
                .map(|name| json!({ "required": [name] }))
                .collect::<Vec<_>>()
        }
    })
}

fn discriminated_variant(
    discriminator: &str,
    value: Value,
    required: &[&str],
    forbidden: &[&str],
) -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert(discriminator.to_owned(), json!({ "const": value }));
    let mut required_fields = Vec::with_capacity(required.len() + 1);
    required_fields.push(discriminator);
    required_fields.extend_from_slice(required);
    let mut variant = json!({
        "properties": Value::Object(properties),
        "required": required_fields,
    });
    if !forbidden.is_empty() {
        variant["allOf"] = json!([forbid_arguments(forbidden)]);
    }
    variant
}

fn set_tool_schema_keyword(tools: &mut [Value], name: &str, keyword: &str, value: Value) {
    let schema = tools
        .iter_mut()
        .find(|schema| schema["name"] == name)
        .unwrap_or_else(|| panic!("missing internal tool schema {name:?}"));
    schema["inputSchema"][keyword] = value;
}

/// Publish the same conditional language enforced by `classify_tool_request`.
/// Closed top-level properties are insufficient here: without these variants,
/// clients would be told that ignored/mode-inapplicable arguments are valid.
fn attach_conditional_tool_schemas(tools: &mut [Value]) {
    let body_true = discriminated_variant("body", json!(true), &[], &[]);
    let body_not_true = json!({
        "allOf": [
            { "not": {
                "properties": { "body": { "const": true } },
                "required": ["body"]
            }},
            forbid_arguments(&["body_offset", "max_body_bytes"]),
        ]
    });
    set_tool_schema_keyword(
        tools,
        "get",
        "allOf",
        json!([{ "oneOf": [body_true, body_not_true] }]),
    );

    let local_link = json!({
        "allOf": [
            forbid_arguments(&["to_db"]),
            { "oneOf": [
                { "required": ["anchor_start", "anchor_end"] },
                forbid_arguments(&["anchor_start", "anchor_end"]),
            ]},
        ]
    });
    let remote_link = json!({
        "required": ["to_db"],
        "allOf": [forbid_arguments(&["kind", "anchor_start", "anchor_end"])]
    });
    set_tool_schema_keyword(tools, "link", "oneOf", json!([local_link, remote_link]));

    set_tool_schema_keyword(
        tools,
        "merge",
        "oneOf",
        json!([
            discriminated_variant("mode", json!("full"), &["winner", "loser"], &["a", "b"],),
            discriminated_variant("mode", json!("keep"), &["a", "b"], &["winner", "loser"],),
        ]),
    );

    let terminal_forbidden = [
        "db",
        "start",
        "budget",
        "query",
        "to",
        "body_offset",
        "max_body_bytes",
    ];
    set_tool_schema_keyword(
        tools,
        "walk",
        "oneOf",
        json!([
            discriminated_variant(
                "action",
                json!("start"),
                &["start"],
                &["session", "to", "body_offset", "max_body_bytes"],
            ),
            discriminated_variant("action", json!("look"), &["session"], &terminal_forbidden),
            discriminated_variant("action", json!("edges"), &["session"], &terminal_forbidden),
            discriminated_variant(
                "action",
                json!("body"),
                &["session"],
                &["db", "start", "budget", "query", "to"],
            ),
            discriminated_variant(
                "action",
                json!("go"),
                &["session", "to"],
                &[
                    "db",
                    "start",
                    "budget",
                    "query",
                    "body_offset",
                    "max_body_bytes",
                ],
            ),
            discriminated_variant("action", json!("back"), &["session"], &terminal_forbidden),
            discriminated_variant("action", json!("done"), &["session"], &terminal_forbidden),
            discriminated_variant("action", json!("abort"), &["session"], &terminal_forbidden),
        ]),
    );
}

/// Tools whose flat schema requires owner selection for mutation or lease work.
/// Grouped `concern` and `episode` keep write-only requirements in their union
/// schemas and typed action classification, rather than forbidding defaulted reads.
///
/// `walk` remains absent: it is a read-only capability session and `done` only
/// returns a receipt. The actual training boundary is `reflect`.
fn mutation_requires_explicit_db(name: &str) -> bool {
    matches!(
        name,
        "database_control"
            | "snapshot_create"
            | "decay"
            | "prune"
            | "ingest"
            | "capture"
            | "retag"
            | "edit_body"
            | "edit_summary"
            | "save"
            | "feedback"
            | "forget"
            | "link"
            | "supersede"
            | "contradict"
            | "reconcile"
            | "merge"
            | "reflect"
    )
}

/// Apply the advertised closed shape and scalar/collection types even when a
/// caller skips JSON Schema validation. Semantic and conditional validation is
/// deliberately separate below, but both run before any admission or checkout.
fn validate_tool_arguments(name: &str, arguments: &Value) -> Result<(), AnyErr> {
    if name == "graph" {
        prepare_graph(arguments)?;
        return Ok(());
    }
    if name == "list" {
        touchstone::prepare_list(arguments)?;
        return Ok(());
    }
    if matches!(name, "retag" | "edit_body" | "edit_summary") {
        return Ok(());
    }
    if name == "concern" {
        // Complete shared union admission is retained below, before authority.
        return Ok(());
    }
    if name == "save" {
        // The union is admitted once into the retained PreparedSave below.
        // Parsing here as well would generate a second manual operation ID.
        return Ok(());
    }
    if name == "episode" {
        // The shared app parser owns episode unions and conditional fields
        // (`body` is write text or a read boolean). Do not reinterpret that
        // grammar with the legacy flat scalar-schema validator below.
        episode::PreparedEpisode::parse(arguments)?;
        return Ok(());
    }
    static SCHEMAS: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();
    let schemas = SCHEMAS.get_or_init(unfiltered_tool_schemas);
    let schema = schemas
        .iter()
        .find(|schema| schema["name"] == name)
        .ok_or_else(|| format!("unknown tool {name:?}"))?;
    let arguments = arguments
        .as_object()
        .ok_or_else(|| format!("arguments for tool {name:?} must be an object"))?;
    let properties = schema["inputSchema"]["properties"]
        .as_object()
        .expect("tool properties are objects");
    if let Some(key) = arguments.keys().find(|key| !properties.contains_key(*key)) {
        return Err(format!("unknown argument `{key}` for tool {name:?}").into());
    }
    for required in schema["inputSchema"]["required"]
        .as_array()
        .expect("tool required is an array")
    {
        let required = required.as_str().expect("required names are strings");
        if !arguments.contains_key(required) {
            return Err(format!("missing required argument `{required}`").into());
        }
    }
    for (key, value) in arguments {
        if name == "recall_context" && key == "routing_hints" {
            // Canonical catalog shape is a bounded array. This optional advisory
            // field alone has an explicit neutral-on-invalid contract, parsed
            // into a bounded ignored disposition before checkout below.
            continue;
        }
        validate_advertised_value(
            key,
            value,
            properties
                .get(key)
                .expect("unknown arguments were rejected above"),
        )?;
    }
    Ok(())
}

fn validate_advertised_value(key: &str, value: &Value, schema: &Value) -> Result<(), AnyErr> {
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => {
            if !value.is_string() {
                return Err(format!("argument `{key}` must be a string").into());
            }
        }
        Some("boolean") => {
            if !value.is_boolean() {
                return Err(format!("argument `{key}` must be a boolean").into());
            }
        }
        Some("integer") => {
            value
                .as_u64()
                .ok_or_else(|| format!("argument `{key}` must be a non-negative integer"))?;
        }
        Some("number") => {
            value
                .as_f64()
                .filter(|number| number.is_finite())
                .ok_or_else(|| format!("argument `{key}` must be a finite number"))?;
        }
        Some("array") => {
            let values = value
                .as_array()
                .ok_or_else(|| format!("argument `{key}` must be an array"))?;
            if let Some(maximum) = schema.get("maxItems").and_then(Value::as_u64)
                && u64::try_from(values.len()).expect("array length fits u64") > maximum
            {
                return Err(format!("argument `{key}` accepts at most {maximum} items").into());
            }
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in values.iter().enumerate() {
                    validate_advertised_value(&format!("{key}[{index}]"), item, item_schema)?;
                }
            }
        }
        Some("object") => {
            if !value.is_object() {
                return Err(format!("argument `{key}` must be an object").into());
            }
        }
        Some(other) => return Err(format!("internal unsupported schema type {other:?}").into()),
        None => return Err(format!("internal schema for argument `{key}` has no type").into()),
    }
    Ok(())
}

/// Validate every domain value and every action/mode-specific shape while the
/// request has no database access. Existing routes duplicate some cheap parses
/// in their handlers; SAVE instead retains its admitted request across this
/// boundary because reconstruction could generate a different manual identity.
fn classify_tool_request(
    name: &str,
    arguments: &Value,
    prepared_save: Option<&save::PreparedSave>,
    prepared_concern: Option<&concern::PreparedConcern>,
) -> Result<ToolRequestKind, AnyErr> {
    match name {
        "databases" => Ok(ToolRequestKind::Databases),
        "activity" => Ok(ToolRequestKind::Activity {
            after: arguments.get("after").map_or(Ok(0), |value| {
                value
                    .as_u64()
                    .ok_or("argument `after` must be a non-negative integer")
            })?,
            limit: optional_bounded_usize(arguments, "limit", 1, activity::MAX_PAGE)?
                .unwrap_or(activity::MAX_PAGE),
        }),
        "snapshot_create" => Ok(ToolRequestKind::SnapshotCreate),
        "status" => Ok(ToolRequestKind::Status),
        "decay" => Ok(ToolRequestKind::Decay),
        "prune" => Ok(ToolRequestKind::Prune),
        "contradictions" => Ok(ToolRequestKind::Contradictions),
        "merges" => Ok(ToolRequestKind::Merges),
        "database_control" => match req_s(arguments, "action")? {
            "status" => Ok(ToolRequestKind::DatabaseControl(
                DatabaseControlRequest::Status,
            )),
            "release" => Ok(ToolRequestKind::DatabaseControl(
                DatabaseControlRequest::Release,
            )),
            "resume" => Ok(ToolRequestKind::DatabaseControl(
                DatabaseControlRequest::Resume,
            )),
            other => Err(format!(
                "database_control action must be status|release|resume, got {other:?}"
            )
            .into()),
        },
        "query" => PublicQueryInput::parse(arguments, true).map(|_| ToolRequestKind::Query),
        "recall_context" => {
            let _ = context_observation::parse_hints(arguments);
            optional_bool(arguments, "observe")?;
            PublicQueryInput::parse(arguments, false).map(|_| ToolRequestKind::RecallContext)
        }
        "recall" => {
            bounded_required(arguments, "text", MAX_QUERY_BYTES)?;
            optional_bounded_usize(arguments, "expand_top", 1, MAX_RECALL_EXPAND)?;
            optional_bounded_usize(arguments, "neighbors_each", 1, MAX_NEIGHBORS_EACH)?;
            Ok(ToolRequestKind::Recall)
        }
        "get" => {
            parse_id(req_s(arguments, "id")?)?;
            let body = optional_bool(arguments, "body")?;
            optional_bool(arguments, "edges")?;
            if body {
                body_offset(arguments)?;
                body_limit(arguments)?;
            } else {
                reject_irrelevant(
                    arguments,
                    &["body_offset", "max_body_bytes"],
                    "get without body:true",
                )?;
            }
            Ok(ToolRequestKind::Get)
        }
        "list" => {
            touchstone::prepare_list(arguments)?;
            Ok(ToolRequestKind::List)
        }
        "graph" => {
            prepare_graph(arguments)?;
            Ok(ToolRequestKind::Graph)
        }
        "neighbors" => {
            prepare_neighbors(arguments)?;
            Ok(ToolRequestKind::Neighbors)
        }
        "remote_edges" => {
            let source = parse_id(req_s(arguments, "id")?)?;
            remote_page_limit(arguments)?;
            if let Some(cursor) = remote_cursor(arguments)? {
                cursor
                    .validate_for(source)
                    .map_err(|error| format!("invalid remote edge cursor: {error}"))?;
            }
            optional_bool(arguments, "resolve")?;
            Ok(ToolRequestKind::RemoteEdges)
        }
        "core" => {
            body_limit(arguments)?;
            Ok(ToolRequestKind::Core)
        }
        "ingest" => {
            let summary = bounded_required(arguments, "summary", MAX_SUMMARY_BYTES)?;
            if let Some(body) = optional_string(arguments, "body")? {
                validate_text_size("body", body, MAX_INGEST_BODY_BYTES)?;
            } else {
                debug_assert!(summary.len() <= MAX_INGEST_BODY_BYTES);
            }
            let tags = strs(arguments, "tags")?;
            validate_tags(&tags)?;
            optional_unit_interval(arguments, "stability")?;
            optional_unit_interval(arguments, "confidence")?;
            let action = if tags.iter().any(|tag| tag == "core") {
                IngestRequest::Core
            } else {
                IngestRequest::Ordinary
            };
            Ok(ToolRequestKind::Ingest(action))
        }
        "concern" => Ok(ToolRequestKind::Concern(
            prepared_concern
                .expect("concern retained before classification")
                .action(),
        )),
        "save" => Ok(ToolRequestKind::Save(
            prepared_save
                .expect("SAVE is retained before classification")
                .authority(),
        )),
        "capture" => Ok(ToolRequestKind::Capture(
            capture::PreparedCapture::parse(arguments)?.authority(),
        )),
        "episode" => Ok(ToolRequestKind::Episode(
            episode::PreparedEpisode::parse(arguments)?.action(),
        )),
        "feedback" => {
            optional_string(arguments, "from")?
                .map(parse_id)
                .transpose()?;
            parse_id(req_s(arguments, "to")?)?;
            parse_signal(req_s(arguments, "signal")?)?;
            Ok(ToolRequestKind::Feedback)
        }
        "forget" => {
            parse_id(req_s(arguments, "id")?)?;
            Ok(ToolRequestKind::Forget)
        }
        "link" => validate_link_arguments(arguments).map(ToolRequestKind::Link),
        "supersede" => {
            parse_id(req_s(arguments, "winner")?)?;
            parse_id(req_s(arguments, "loser")?)?;
            Ok(ToolRequestKind::Supersede)
        }
        "contradict" => {
            parse_id(req_s(arguments, "a")?)?;
            parse_id(req_s(arguments, "b")?)?;
            Ok(ToolRequestKind::Contradict)
        }
        "reconcile" => {
            parse_id(req_s(arguments, "a")?)?;
            parse_id(req_s(arguments, "b")?)?;
            parse_resolution(req_s(arguments, "resolution")?)?;
            Ok(ToolRequestKind::Reconcile)
        }
        "merge" => validate_merge_arguments(arguments).map(ToolRequestKind::Merge),
        "walk" => validate_walk_arguments(arguments).map(ToolRequestKind::Walk),
        "reflect" => {
            let receipts = strs(arguments, "receipts")?;
            if receipts.is_empty() || receipts.len() > MAX_REFLECT_RECEIPTS {
                return Err(format!(
                    "reflect requires 1..={MAX_REFLECT_RECEIPTS} completed-walk receipts"
                )
                .into());
            }
            let mut unique_receipts = HashSet::with_capacity(receipts.len());
            for receipt in &receipts {
                if !unique_receipts.insert(receipt) {
                    return Err(format!("duplicate walk receipt {receipt:?}").into());
                }
            }
            reflection_judgments(arguments)?;
            Ok(ToolRequestKind::Reflect)
        }
        other => Err(format!("unknown tool {other:?}").into()),
    }
}

fn validate_link_arguments(arguments: &Value) -> Result<LinkRequest, AnyErr> {
    parse_id(req_s(arguments, "from")?)?;
    parse_id(req_s(arguments, "to")?)?;
    optional_unit_interval(arguments, "weight")?;

    if let Some(to_db) = optional_string(arguments, "to_db")? {
        reject_irrelevant(
            arguments,
            &["kind", "anchor_start", "anchor_end"],
            "a cross-database link",
        )?;
        let source = selected_db_name(arguments)?;
        if source != HOME_DB {
            return Err(format!(
                "cross-db edges may only originate from the {HOME_DB:?} db (keeps project dbs self-contained)"
            )
            .into());
        }
        if to_db == source {
            return Err("to_db must be a different database than db".into());
        }
        return Ok(LinkRequest::Remote);
    }

    parse_kind(optional_string(arguments, "kind")?.unwrap_or("associative"))?;
    optional_body_span(arguments)?;
    Ok(LinkRequest::Local)
}

fn validate_merge_arguments(arguments: &Value) -> Result<MergeRequest, AnyErr> {
    match req_s(arguments, "mode")? {
        "full" => {
            reject_irrelevant(arguments, &["a", "b"], "merge mode full")?;
            parse_id(req_s(arguments, "winner")?)?;
            parse_id(req_s(arguments, "loser")?)?;
            Ok(MergeRequest::Full)
        }
        "keep" => {
            reject_irrelevant(arguments, &["winner", "loser"], "merge mode keep")?;
            parse_id(req_s(arguments, "a")?)?;
            parse_id(req_s(arguments, "b")?)?;
            Ok(MergeRequest::Keep)
        }
        "partial" => Err(
            "partial merge is not exposed: the unsafe legacy writer was removed; use full or keep"
                .into(),
        ),
        other => Err(format!("merge mode must be full|keep, got {other:?}").into()),
    }
}

fn validate_walk_arguments(arguments: &Value) -> Result<WalkRequest, AnyErr> {
    let action = req_s(arguments, "action")?;
    match action {
        "start" => {
            reject_irrelevant(
                arguments,
                &["session", "to", "body_offset", "max_body_bytes"],
                "walk action start",
            )?;
            parse_id(req_s(arguments, "start")?)?;
            optional_bounded_usize(arguments, "budget", 1, MAX_WALK_BUDGET)?;
            if let Some(query) = optional_string(arguments, "query")? {
                validate_text_size("query", query, MAX_QUERY_BYTES)?;
            }
            Ok(WalkRequest::Start)
        }
        "body" => {
            reject_irrelevant(
                arguments,
                &["db", "start", "budget", "query", "to"],
                "walk action body",
            )?;
            req_s(arguments, "session")?;
            body_offset(arguments)?;
            body_limit(arguments)?;
            Ok(WalkRequest::Body)
        }
        "go" => {
            reject_irrelevant(
                arguments,
                &[
                    "db",
                    "start",
                    "budget",
                    "query",
                    "body_offset",
                    "max_body_bytes",
                ],
                "walk action go",
            )?;
            req_s(arguments, "session")?;
            let target = req_s(arguments, "to")?;
            if target.parse::<usize>().is_err() && parse_id(target).is_err() {
                return Err("argument `to` must be a neighbor index or node id".into());
            }
            Ok(WalkRequest::Go)
        }
        "look" | "edges" | "back" | "done" | "abort" => {
            reject_irrelevant(
                arguments,
                &[
                    "db",
                    "start",
                    "budget",
                    "query",
                    "to",
                    "body_offset",
                    "max_body_bytes",
                ],
                &format!("walk action {action}"),
            )?;
            req_s(arguments, "session")?;
            Ok(match action {
                "look" => WalkRequest::Look,
                "edges" => WalkRequest::Edges,
                "back" => WalkRequest::Back,
                "done" => WalkRequest::Done,
                "abort" => WalkRequest::Abort,
                _ => unreachable!("match arm restricts the action"),
            })
        }
        other => Err(format!(
            "walk action must be start|look|edges|body|go|back|done|abort, got {other:?}"
        )
        .into()),
    }
}

fn reject_irrelevant(arguments: &Value, keys: &[&str], variant: &str) -> Result<(), AnyErr> {
    if let Some(key) = keys.iter().find(|key| arguments.get(**key).is_some()) {
        return Err(format!("argument `{key}` is not valid for {variant}").into());
    }
    Ok(())
}

/// `db` selector shared by most tools.
fn db_prop() -> Value {
    json!({ "type": "string", "description": "logical database name from the startup registry; omitted reads select registered 'project', otherwise the sole owner; ambiguous registries require db. Mutators always require an explicit selector" })
}

fn expected_db_id_prop() -> Value {
    json!({
        "type": "string", "minLength": 26, "maxLength": 26,
        "pattern": "^[0-7][0-9A-HJKMNP-TV-Z]{25}$",
        "description": "Optional native database identity precondition. Refuses a changed target before reading or writing; never follows the identity to another registry name.",
    })
}

fn prepare_neighbors(arguments: &Value) -> Result<mneme_app::neighbors::PreparedNeighbors, AnyErr> {
    let mut domain = arguments
        .as_object()
        .ok_or("neighbors arguments must be an object")?
        .clone();
    optional_string(arguments, "db")?;
    optional_expected_db_id(arguments)?;
    domain.remove("db");
    domain.remove("expected_db_id");
    mneme_app::neighbors::PreparedNeighbors::parse(&Value::Object(domain))
}

fn neighbors_tool_schema() -> Value {
    let mut input = mneme_app::neighbors::neighbors_input_schema();
    input["properties"]["db"] = db_prop();
    input["properties"]["expected_db_id"] = expected_db_id_prop();
    json!({
        "name": "neighbors",
        "description": "Page raw local incident edges, including episodes and unavailable neighbors. Bound page size and summary hydration; return next_cursor for continuation. Cursors bind this database and node, and are best-effort under concurrent graph mutation. Does not reinforce or learn.",
        "inputSchema": input,
    })
}

fn prepare_graph(arguments: &Value) -> Result<mneme_app::graph_view::PreparedGraph, AnyErr> {
    let mut domain = arguments
        .as_object()
        .ok_or("graph arguments must be an object")?
        .clone();
    optional_string(arguments, "db")?;
    optional_expected_db_id(arguments)?;
    domain.remove("db");
    domain.remove("expected_db_id");
    mneme_app::graph_view::PreparedGraph::parse(&Value::Object(domain))
}

fn graph_tool_schema() -> Value {
    let mut input = mneme_app::graph_view::graph_input_schema();
    input["properties"]["db"] = db_prop();
    input["properties"]["expected_db_id"] = expected_db_id_prop();
    // The closed action branches must expose the same routing envelope.
    if let Some(branches) = input["oneOf"].as_array_mut() {
        for branch in branches {
            branch["properties"]["db"] = db_prop();
            branch["properties"]["expected_db_id"] = expected_db_id_prop();
        }
    }
    json!({
        "name": "graph",
        "description": "Read a lightweight graph skeleton in bounded topology pages (IDs and edges, no summaries). Fetch summaries only for up to 64 requested visible nodes. Follow next_cursor; pages are not a transaction snapshot. Does not reinforce or learn.",
        "inputSchema": input,
    })
}

fn unfiltered_tool_schemas() -> Vec<Value> {
    let mut tools = vec![
        tool(
            "databases",
            "List registered databases with stable id, resolved path, lease state, and in-flight MCP checkout count.",
            json!({}),
            &[],
        ),
        tool(
            "activity",
            "Best-effort in-memory owner activity for visualizers: successful prepared read results, not graph traversal or proof of client delivery. Returns bounded node IDs only, never query/body text. Process-wide read-only visibility like databases; not caller authentication. Polling emits no event and takes no database checkout. Retains at most 256 events; reset after to zero when instance changes. Busy polls preserve the cursor. This disposable stream may miss events and resets on restart.",
            json!({
                "after": { "type": "integer", "minimum": 0, "description": "exclusive sequence cursor; default 0" },
                "limit": { "type": "integer", "minimum": 1, "maximum": activity::MAX_PAGE, "description": "maximum events; default 64" },
            }),
            &[],
        ),
        tool(
            "database_control",
            "Explicitly hand one database lease to offline CLI maintenance. status is read-only. release succeeds only with no request checkouts, detached backend jobs, walks, or unconsumed/claimed receipts; it checkpoints snapshots, rotates that database's volatile feedback authority, purges now-invalid consumed retry tombstones, and fences normal MCP access. resume re-resolves the configured selector and fails closed while another process owns the lease.",
            json!({
                "db": db_prop(),
                "action": { "type": "string", "enum": ["status", "release", "resume"] },
            }),
            &["action"],
        ),
        tool(
            "snapshot_create",
            "Create an immutable native snapshot of one registered database in its trusted adjacent snapshots root. Requires quiescent walks, receipts, requests, and backend work; rotates volatile feedback authority. The destination is server-selected, not caller-supplied.",
            json!({ "db": db_prop() }),
            &["db"],
        ),
        tool(
            "status",
            "Inspect Active/Archived counts and edge_decay_pending. Omitted db selects project or the sole owner; pass db explicitly for user. Includes episode editions, open contradictions and merge candidates.",
            json!({ "db": db_prop() }),
            &[],
        ),
        tool(
            "decay",
            "Bounded cold-path edge-decay sweep with conditional updates and conflict counts. It never archives or demotes nodes.",
            json!({ "db": db_prop() }),
            &[],
        ),
        tool(
            "prune",
            "Bounded cold-path density GC: conditionally delete stale weak guesses in bounded pages, then trim each hub from its current indexed adjacency inside bounded store transactions. On write-through Cozo this is online-safe; snapshot stores still pay one final O(N) checkpoint. Structural edges are exempt; conflicts/contention are reported.",
            json!({ "db": db_prop() }),
            &[],
        ),
        tool(
            "query",
            "Semantic recall as mneme.query.v3: one ordinary primary lane with rank evidence and a retrieval stamp. Tagged queries include primary seed_coverage and bounded work; seed coverage is not final graph-hit coverage. Read-only: retrieval records no exposure and trains nothing.",
            json!({
                "db": db_prop(),
                "text": { "type": "string", "maxLength": MAX_QUERY_BYTES, "description": "what you're working on / looking for (8192 UTF-8 bytes max)" },
                "k": { "type": "integer", "minimum": 1, "maximum": MAX_QUERY_K },
                "depth": { "type": "integer", "minimum": 0, "maximum": MAX_QUERY_DEPTH, "description": "graph traversal depth; 0 is the exact sparse+dense graph-off baseline" },
                "max_nodes": { "type": "integer", "minimum": 1, "maximum": MAX_QUERY_NODES },
                "min_relevance": { "type": "number", "minimum": 0, "maximum": 1 },
                "archived": { "type": "boolean", "description": "also search forgotten nodes" },
                "tags": { "type": "array", "maxItems": MAX_TAGS, "items": { "type": "string", "maxLength": MAX_TAG_BYTES } },
            }),
            &["text"],
        ),
        tool(
            "recall_context",
            "Recall task-relevant context with db and text (not query). Omitted db selects project or the sole registered owner; use db explicitly otherwise. Read-only mneme.context.v7: semantic memories plus current lexical and exact linked historical scenes, packed together into bounded JSON. Both lane capacities follow effective max_nodes and share the 32 KiB envelope (including a 2 KiB control reserve). Tag filters skip lexical episode search; links may supply indirect scenes, not episode tag matches. Frozen initial hits supply one-hop reference anchors; explicit reference coverage reports bounded work and unknown tails. These are partial request windows, not complete discovery. Retrieval metadata and overall partial report omissions or unavailable lanes. Optional observe makes non-authorizing exact card digests and contributing graph paths mandatory within the same total byte limit. Byte pressure triggers bounded same-window repacking without another retrieval; fewer cards may be emitted, with explicit omissions. Optional routing_hints (whole canonical compact array at most 16 KiB, fixed boost/weaken) affect ordering within the existing traversal shortlists; oversized or malformed batches are wholly neutral, never a signed prefix. Stale/wrong-db hints fall back to baseline ordering. Presence including [] requests validated/ignored diagnostics and, with observe, native semantic route bindings across the bounded returned window, without a separate eight-binding cutoff. Requested observation provenance is never silently dropped. Bindings exclude use timestamps and mutable external body bytes. No bodies, feedback receipt, exposure, or learning.",
            json!({
                "db": db_prop(),
                "routing_hints": context_observation::hints_schema(),
                "observe": { "type": "boolean", "description": "opt-in shadow provenance only; never creates feedback authority or records delivery" },
                "text": { "type": "string", "maxLength": MAX_QUERY_BYTES, "description": "what you're working on / looking for (8192 UTF-8 bytes max)" },
                "k": { "type": "integer", "minimum": 1, "maximum": MAX_QUERY_K },
                "depth": { "type": "integer", "minimum": 0, "maximum": MAX_QUERY_DEPTH, "description": "graph traversal depth; 0 is the exact sparse+dense graph-off baseline" },
                "max_nodes": { "type": "integer", "minimum": 1, "maximum": MAX_QUERY_NODES },
                "min_relevance": { "type": "number", "minimum": 0, "maximum": 1 },
                "tags": { "type": "array", "maxItems": MAX_TAGS, "items": { "type": "string", "maxLength": MAX_TAG_BYTES } },
            }),
            &["text"],
        ),
        tool(
            "recall",
            "Recall with a shallow one-hop expansion — the middle tier between `query` and a full `walk`. Returns bounded associative presentation candidates in one read-only call; it records no exposure and trains nothing. Use feedback/reflect only for grounded learning.",
            json!({
                "db": db_prop(),
                "text": { "type": "string", "maxLength": MAX_QUERY_BYTES, "description": "what you're working on / looking for (8192 UTF-8 bytes max)" },
                "expand_top": { "type": "integer", "minimum": 1, "maximum": MAX_RECALL_EXPAND, "description": "how many top hits to expand (default 3)" },
                "neighbors_each": { "type": "integer", "minimum": 1, "maximum": MAX_NEIGHBORS_EACH, "description": "neighbours per expanded hit (default 5)" },
            }),
            &["text"],
        ),
        episode::tool_schema(CapabilityProfile::Operator),
        concern::tool_schema(CapabilityProfile::Operator),
        retag::tool_schema(CapabilityProfile::Operator),
        edit_body::tool_schema(),
        edit_summary::tool_schema(),
        tool(
            "get",
            "Read one memory by id in explicit db, including summary-only snapshot hash and immutable touchstone metadata with separate current-resolution caveats. Historical episode editions stay exact. Optional body and edges are bounded; edge metadata reports returned/has_more, never a fake total.",
            json!({
                "db": db_prop(), "id": { "type": "string" },
                "expected_db_id": expected_db_id_prop(),
                "body": { "type": "boolean" }, "edges": { "type": "boolean" },
                "body_offset": { "type": "integer", "minimum": 0, "description": "source byte offset for the body range; default 0" },
                "max_body_bytes": { "type": "integer", "minimum": 1, "maximum": MAX_BODY_BYTES, "description": "body range output cap; default 65536" },
            }),
            &["id"],
        ),
        touchstone::list_tool_schema(),
        neighbors_tool_schema(),
        graph_tool_schema(),
        tool(
            "remote_edges",
            "Page cross-database edges from one local node in exact weight-descending order. Pass the opaque `next` object back as `after`. Target-summary hydration is opt-in and bounded by the page size; cursors are exact on unchanged adjacency and best-effort across concurrent mutation.",
            json!({
                "db": db_prop(),
                "id": { "type": "string", "description": "local source node id" },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_REMOTE_EDGE_PAGE_SIZE, "description": "page size; default 32" },
                "after": { "type": "object", "description": "opaque next cursor returned by the previous page" },
                "resolve": { "type": "boolean", "description": "hydrate target summaries from loaded databases (default false; at most one lookup per item)" },
            }),
            &["id"],
        ),
        tool(
            "core",
            "The always-loaded core set with bounded bodies. Returns truncation metadata, emits at most 32 nodes, and shares a 128 KiB body allowance fairly across them.",
            json!({
                "db": db_prop(),
                "max_body_bytes": { "type": "integer", "minimum": 1, "maximum": MAX_BODY_BYTES, "description": "per-body output cap; default 65536" },
            }),
            &[],
        ),
        tool(
            "ingest",
            "Experimental raw ingest without replay identity. Use save for ordinary memory. Tag core requires operator authority.",
            json!({
                "db": db_prop(),
                "summary": { "type": "string", "maxLength": MAX_SUMMARY_BYTES, "description": "short, embeddable one-liner (2048 UTF-8 bytes max)" },
                "body": { "type": "string", "maxLength": MAX_INGEST_BODY_BYTES, "description": "full content (defaults to the summary; 256 KiB UTF-8 max)" },
                "tags": { "type": "array", "maxItems": MAX_TAGS, "items": { "type": "string", "maxLength": MAX_TAG_BYTES } },
                "stability": { "type": "number", "minimum": 0, "maximum": 1, "description": "0..1, how durable" },
                "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
            }),
            &["summary"],
        ),
        save::tool_schema(CapabilityProfile::Operator),
        tool(
            "capture",
            "Save a sourced memory with explicit db, structured source, and summary. Compatibility route for installed callers; prefer save. A namespace/key names one logical claim in this database: the exact same request returns its original ID with replayed:true; changed content or declared links under that key conflict. Optional links atomically connect to existing same-database nodes (no inferred similarity links). Optional touchstone authors an immutable note-only subject and summary snapshots; use GET hashes and same-db references. Exact replay precedes target resolution. Ordinary captures are searchable immediately under curator authority; the core tag requires operator authority. Source is visible in get.",
            capture::properties(),
            &["db", "source", "summary"],
        ),
        tool(
            "feedback",
            "Report direct node evidence, optionally attributed to an observed `from -> to` route. Omit `from` for node-only evidence. relevant records grounded use; not-new records grounded redundancy (and only flags a merge pair when `from` is present); irrelevant weakens it. This direct tool is not receipt-bound or idempotent: do not replay an ambiguous call.",
            json!({ "db": db_prop(), "from": { "type": "string", "description": "optional route source; omit for node-only evidence" }, "to": { "type": "string" }, "signal": { "type": "string", "enum": ["relevant", "not-new", "irrelevant"] } }),
            &["to", "signal"],
        ),
        tool(
            "forget",
            "Drop a node, its edges, and its vector — for a wrong/unwanted memory.",
            json!({ "db": db_prop(), "id": { "type": "string" } }),
            &["id"],
        ),
        tool(
            "link",
            "Assert an edge `from -> to`. Same-db by default; pass `to_db` (a registered db name) to make it a cross-db see-also edge — `to` then refers to a node there, and only the `user` db may originate one. Optional anchor (byte range into from's body) for a passage-level association.",
            json!({
                "db": db_prop(), "from": { "type": "string" }, "to": { "type": "string" },
                "to_db": { "type": "string", "description": "target db for a cross-db edge (must differ from db; source must be 'user')" },
                "kind": { "type": "string", "enum": ["associative", "transition", "supersedes", "derived_from"], "description": "local links only" },
                "weight": { "type": "number", "minimum": 0, "maximum": 1 },
                "anchor_start": { "type": "integer", "minimum": 0, "maximum": u32::MAX, "description": "local links only; must be paired with anchor_end" },
                "anchor_end": { "type": "integer", "minimum": 0, "maximum": u32::MAX, "description": "local links only; must be paired with anchor_start and be >= anchor_start" },
            }),
            &["from", "to"],
        ),
        tool(
            "supersede",
            "Resolve a contradiction with explicit db, winner, and loser. Winner replaces loser: emits a Supersedes edge and archives the loser while preserving readable history.",
            json!({ "db": db_prop(), "winner": { "type": "string" }, "loser": { "type": "string" } }),
            &["winner", "loser"],
        ),
        tool(
            "contradict",
            "Flag two nodes as conflicting, for a later reconciliation pass.",
            json!({ "db": db_prop(), "a": { "type": "string" }, "b": { "type": "string" } }),
            &["a", "b"],
        ),
        tool(
            "contradictions",
            "The reconciliation triage: open contradictions (observation-count desc), each tagged with its nodes' current communities, plus the derived community-level conflict aggregate. Decide each: supersede (real, newer wins), or reconcile context-dependent / unresolved.",
            json!({ "db": db_prop() }),
            &[],
        ),
        tool(
            "reconcile",
            "Record a reconciliation verdict for a flagged pair: context-dependent (both true in different contexts — stop flagging) or unresolved (leave standing). For a real supersession use `supersede`.",
            json!({ "db": db_prop(), "a": { "type": "string" }, "b": { "type": "string" }, "resolution": { "type": "string", "enum": ["context-dependent", "unresolved"] } }),
            &["a", "b", "resolution"],
        ),
        tool(
            "merges",
            "Open merge candidates (pairs flagged redundant) awaiting a decision.",
            json!({ "db": db_prop() }),
            &[],
        ),
        tool(
            "merge",
            "Adjudicate a merge candidate. mode=full collapses loser into winner; mode=keep declines. The unsafe legacy partial-merge writer was removed; a future replacement requires one atomic child-plus-derivations commit.",
            json!({
                "db": db_prop(), "mode": { "type": "string", "enum": ["full", "keep"] },
                "winner": { "type": "string" }, "loser": { "type": "string" },
                "a": { "type": "string" }, "b": { "type": "string" },
            }),
            &["mode"],
        ),
        tool(
            "walk",
            "Constrained, READ-ONLY traversal keyed by a session token. action=start returns {session, view}; inspect/move with look|edges|body|go|back. Budget caps distinct nodes; the server also caps total actions and current path depth. done and abort remain available after action exhaustion. done returns the visible trail plus an opaque, single-use receipt; abort returns only the trail. To train afterward, call `reflect` with one or more receipts and the nodes that actually fed your answer.",
            json!({
                "action": { "type": "string", "enum": ["start", "look", "edges", "body", "go", "back", "done", "abort"] },
                "db": db_prop(),
                "start": { "type": "string", "description": "start node id (action=start)" },
                "budget": { "type": "integer", "minimum": 1, "maximum": MAX_WALK_BUDGET, "description": "max distinct nodes (action=start; default 25)" },
                "query": { "type": "string", "maxLength": MAX_QUERY_BYTES, "description": "optional (action=start): order this walk's edges by relevance to this query, not raw weight" },
                "session": { "type": "string", "description": "the token from start (every action but start)" },
                "to": { "type": "string", "description": "neighbor index or id (action=go)" },
                "body_offset": { "type": "integer", "minimum": 0, "description": "action=body source byte offset; default 0" },
                "max_body_bytes": { "type": "integer", "minimum": 1, "maximum": MAX_BODY_BYTES, "description": "action=body range output cap; default 65536" },
            }),
            &["action"],
        ),
        tool(
            "reflect",
            "Post-factum learning from completed-walk receipts. List nodes that helped in used and explicitly unhelpful nodes in unhelpful; these sets must be disjoint and observed. Omitted nodes are unknown and get no learning effects. One call atomically applies exact observed-route and node-only judgments. Repeated observations count once; conflicting judgments on a shared stored arrow leave that arrow unchanged. Exact retries are idempotent only while the issuing host generation and receipt remain live, not across restart. Optional configured consolidation over genuinely used nodes runs afterward. No receiptless consolidation or caller-supplied trails. A receipt records the database and observation, not truth or caller authorization.",
            json!({
                "db": db_prop(),
                "receipts": { "type": "array", "minItems": 1, "maxItems": MAX_REFLECT_RECEIPTS, "items": { "type": "string" }, "description": "opaque receipt tokens from completed walks; at least one required" },
                "used": { "type": "array", "maxItems": MAX_REFLECT_USED, "items": { "type": "string" }, "description": "observed node ids that helped; at most 64 judged nodes total" },
                "unhelpful": { "type": "array", "maxItems": MAX_REFLECT_USED, "items": { "type": "string" }, "description": "explicitly unhelpful observed node ids, disjoint from used; omitted nodes are unknown" },
            }),
            &["receipts", "used"],
        ),
    ];
    attach_conditional_tool_schemas(&mut tools);
    tools
}

/// Lowest profile at which a tool has at least one legal action. Tools with
/// mixed authority are narrowed further by [`apply_profile_schema_restrictions`].
/// Returning `None` is deliberately fail-closed; the closed-world catalog test
/// makes an accidentally unclassified schema a test failure.
fn catalog_minimum_capability(name: &str) -> Option<CapabilityClass> {
    match name {
        "databases" | "activity" | "database_control" | "status" | "query" | "recall_context"
        | "recall" | "get" | "list" | "graph" | "neighbors" | "remote_edges" | "core"
        | "contradictions" | "merges" | "walk" | "episode" | "concern" => {
            Some(CapabilityClass::ReadOnly)
        }
        "reflect" => Some(CapabilityClass::ReceiptGrounded),
        "snapshot_create" | "edit_body" | "edit_summary" => Some(CapabilityClass::Operator),
        "retag" | "ingest" | "capture" | "save" | "link" | "contradict" => {
            Some(CapabilityClass::Curator)
        }
        "decay" | "prune" | "feedback" | "forget" | "supersede" | "reconcile" | "merge" => {
            Some(CapabilityClass::Operator)
        }
        _ => None,
    }
}

fn profile_tool_schema_mut<'a>(tools: &'a mut [Value], name: &str) -> &'a mut Value {
    tools
        .iter_mut()
        .find(|schema| schema["name"] == name)
        .unwrap_or_else(|| panic!("missing retained tool schema {name:?}"))
}

fn apply_profile_schema_restrictions(tools: &mut [Value], profile: CapabilityProfile) {
    *profile_tool_schema_mut(tools, "concern") = concern::tool_schema(profile);
    if profile.permits(CapabilityClass::Curator) {
        *profile_tool_schema_mut(tools, "retag") = retag::tool_schema(profile);
    }
    let mut episode = episode::tool_schema(profile);
    let description = episode["description"]
        .as_str()
        .expect("episode description")
        .to_owned();
    episode["description"] = json!(format!(
        "Append remains a compatibility route for installed callers; prefer save for new episodes. {description}"
    ));
    *profile_tool_schema_mut(tools, "episode") = episode;
    if profile.permits(CapabilityClass::Curator) {
        *profile_tool_schema_mut(tools, "save") = save::tool_schema(profile);
    }
    if profile != CapabilityProfile::Operator {
        let database_control = profile_tool_schema_mut(tools, "database_control");
        database_control["description"] = json!(
            "Read the lease, path, in-flight checkout, walk, and receipt status of one registered database. Lease release and resume require the operator profile."
        );
        database_control["inputSchema"]["properties"]["action"]["enum"] = json!(["status"]);
        database_control["inputSchema"]
            .as_object_mut()
            .expect("database_control input schema is an object")
            .remove("allOf");
    }

    if profile == CapabilityProfile::Curator {
        let ingest = profile_tool_schema_mut(tools, "ingest");
        ingest["description"] = json!(
            "Experimental raw ingest without replay identity; use save for ordinary memory. The core tag is forbidden; Core requires the operator profile."
        );
        ingest["inputSchema"]["properties"]["tags"]["items"]["not"] = json!({ "const": "core" });
        let capture = profile_tool_schema_mut(tools, "capture");
        capture["description"] = json!(
            "Save a sourced memory with explicit db, structured source, and summary. Compatibility route for installed callers; prefer save. The core tag is forbidden; Core requires the operator profile."
        );
        capture["inputSchema"]["properties"]["tags"]["items"]["not"] = json!({ "const": "core" });
    }
}

fn tool_schemas(capability: CapabilityPolicy) -> Vec<Value> {
    let mut tools = unfiltered_tool_schemas();
    tools.retain(|schema| {
        let Some(name) = schema["name"].as_str() else {
            return false;
        };
        let Some(required) = catalog_minimum_capability(name) else {
            return false;
        };
        capability.profile.permits(required)
            && (name != "feedback" || capability.allow_direct_feedback)
    });
    apply_profile_schema_restrictions(&mut tools, capability.profile);
    tools
}

#[cfg(test)]
mod tests;
