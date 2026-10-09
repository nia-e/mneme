//! mnemed — the memory daemon as a one-shot CLI.
//!
//! Designed to be driven by an agent across many invocations: the graph
//! persists to `--db` between calls, so an agent can `query` for entry points
//! and then walk the graph with `get`/`neighbors`, pulling exactly what it needs
//! (a node's edges, its full body) in a single call rather than flooding its
//! context. Every command speaks both ways: human-readable text by default,
//! `--json` for machine consumption.
//!
//!   mnemed ingest --summary "..." --tag rust --body "..."
//!   mnemed query "spreading activation" --json
//!   mnemed get <id> --edges --body
//!   mnemed neighbors <id> --json
//!
//! The default build uses the persistent Cozo/SQLite backend with native HNSW and
//! fastembed. A `--no-default-features` build uses the JSON-snapshot reference
//! store and deterministic lexical embedder; `migrate` converts a current v5
//! JSON snapshot to TouchstonesV1 SQLite.

use std::collections::HashSet;
use std::io::Write;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

use mneme_app::{presentation_retrieval_metadata, recall_context};
use mneme_body::{FsStore, InlineStore};
#[cfg(test)]
use mneme_core::BodyRef;
use mneme_core::ports::{
    Budget, Clock, ColdPath, Embedder, EmbeddingMetadataStore, Error, GraphStore, LexicalIndex,
    Reranker, StatusFilter, SystemClock, Traversal, VectorIndex,
};
use mneme_core::{
    BodySpan, EdgeKind, EmbeddingFingerprint, MAX_REMOTE_EDGE_PAGE_SIZE, MergeResolution, Node,
    NodeId, NodeStatus, Provenance, RemoteEdgeCursor, RemoteEdgePage, Resolution, Signal,
};
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::{
    Config, HydratedNeighbor, Ingest, Memory, RetrievalBatch, RetrievalEvidence, RetrievalHit,
};
use mneme_present::{
    BodyBudget, LaneBudgets, LaneLimit, PackPlan, PresentationBudget, QueryBody, QueryEnvelope,
    QueryHit, QueryNodeStatus, RankEvidence, RetrievalLane, RetrievalMetadata,
};

// MemStore (the JSON snapshot backend) is always available, so even a cozo
// build can read — and migrate — a legacy snapshot store.
#[cfg(feature = "cozo")]
use mneme_cozo::CozoStore;
use mneme_cozo::MemStore;

use ulid::Ulid;

mod bootstrap;
mod cli_capture;
mod cli_client;
mod cli_concern;
mod cli_edit_body;
mod cli_edit_summary;
mod cli_episode;
#[cfg(test)]
mod cli_help;
mod cli_ingest;
mod cli_init;
mod cli_library;
mod cli_library_config;
mod cli_list;
mod cli_owner;
mod cli_retag;
mod cli_save;
mod cli_single_graph_upgrade;
mod cli_sources;
mod cli_tui;
mod native_artifact;
mod remote_commands;
mod remote_config;
mod remote_repl;
mod remote_touchstone;
mod remote_transport;
mod repl;
mod workspace_selection;

use cli_list::ListArgs;

pub(crate) type AnyErr = Box<dyn std::error::Error>;
const DEFAULT_CLI_BODY_BYTES: usize = 64 * 1024;
const MAX_CLI_BODY_BYTES: usize = 1024 * 1024;
const MAX_CLI_QUERY_BYTES: usize = 8 * 1024;
const MAX_CLI_QUERY_K: usize = 64;
const MAX_CLI_QUERY_NODES: usize = 256;
const MAX_CLI_QUERY_DEPTH: u8 = 12;
const MAX_CLI_QUERY_TAGS: usize = 32;
const MAX_CLI_QUERY_TAG_BYTES: usize = mneme_core::MAX_TAG_BYTES;
const MAX_CLI_QUERY_BODY_BYTES_TOTAL: usize = 1024 * 1024;
const DEFAULT_CLI_CONTEXT_BYTES: u32 = 32 * 1024;
const MIN_CLI_CONTEXT_BYTES: u32 = 4 * 1024;
const MAX_CLI_CONTEXT_BYTES: u32 = 32 * 1024;
const MAX_CLI_CONTEXT_SUMMARY_BYTES: u16 = 4 * 1024;
type Backend = (
    Arc<dyn GraphStore>,
    Arc<dyn VectorIndex>,
    Arc<dyn Traversal>,
    Arc<dyn LexicalIndex>,
);

#[derive(Parser)]
#[command(
    name = "mnemed",
    version,
    about = "A traversable memory graph for agents",
    after_help = "Ordinary commands use the configured MCP owner: project .mneme/cli.json by default, or $XDG_CONFIG_HOME/mneme/cli.json (~/.config/mneme/cli.json) with --user. Missing or unavailable owners never fall back to opening a database. --db/MNEME_DB explicitly selects offline storage; --remote overrides enrollment. Bootstrap, creation, migration and index replacement retain explicit offline authority. See docs/remote-cli.md for connection configuration and deliberate surface differences.",
    before_help = "START HERE\n  Read:      recall-context TEXT for bounded model context; episode list for the timeline; library catalog for enrolled projects.\n  Inspect:   tui for the memory observatory; get ID for node details; body ID for a bounded body range.\n  Write:     save TEXT for a note; save --kind episode TEXT for an experience; init sets up project memory and Codex hippocampus (use --no-recording to disable automatic recording); capture init with --db PATH explicitly initializes absent storage. Compatibility: capture add --input PATH and episode append --input PATH.\n  Curate:    edit-summary ID for guarded summary replacement; edit-body ID for guarded body replacement; retag ID for guarded complete tag replacement; concern --input PATH for bounded advisory maintenance; status, then reconcile or merge reviewed conflicts.\n  Maintain:  decay and prune are explicit edge sweeps.\n\nUse 'mnemed COMMAND --help' for arguments and recovery details."
)]
struct Cli {
    /// Explicit offline database path; bypasses the configured owner.
    /// Otherwise use the project owner, or the global owner with --user.
    #[arg(long, global = true, env = "MNEME_DB")]
    db: Option<PathBuf>,
    /// Use the configured global owner, never project enrollment. With --remote,
    /// select registry entry user. Explicit --db remains an offline override.
    #[arg(long, global = true)]
    user: bool,
    /// Emit JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,
    #[command(flatten)]
    connection: remote_config::RemoteOptions,
    #[command(subcommand)]
    command: Command,
}

/// Resolve which database to operate on: an explicit `--db`/`$MNEME_DB` wins,
/// else the per-user db when `--user` is set, else the per-project db. The
/// shared resolver follows a native bootstrap generation and rejects an old
/// path coexisting with one, so CLI and MCP cannot silently split the project.
fn resolve_db(db: Option<PathBuf>, user: bool) -> std::io::Result<PathBuf> {
    if let Some(p) = db {
        return mneme_store_path::resolve_configured_store_path(&p);
    }
    if user {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."));
        return mneme_store_path::default_store_path(&base.join("mneme"));
    }
    project_db()
}

/// The current git HEAD in the working directory, or `None` if it isn't a git
/// working tree (or git isn't installed). Stamped onto project memories as their
/// [temporal origin](mneme_core::Node::origin_commit); shells out so packed refs
/// and detached HEAD resolve correctly.
fn git_head_cwd() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// The per-project db in `.mneme/` at the project root — the nearest ancestor
/// (including the cwd) holding a `.mneme/` or a `.git/`, so it's stable from any
/// subdirectory. Falls back to `.mneme/` in the cwd.
fn project_db() -> std::io::Result<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = cwd.as_path();
    loop {
        if dir.join(".mneme").is_dir() || dir.join(".git").exists() {
            return mneme_store_path::default_store_path(&dir.join(".mneme"));
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return mneme_store_path::default_store_path(&cwd.join(".mneme")),
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Internal bounded local MCP client process.
    #[command(hide = true)]
    Client(cli_client::ClientArgs),
    /// Read an immutable memory-library generation without opening a local database.
    Library(cli_library::LibraryArgs),
    /// Open the read-only memory observatory (or use --demo).
    Tui(cli_tui::TuiArgs),
    /// List known configured sources without connecting or opening databases.
    Stores(cli_sources::StoresArgs),
    /// Ask the existing remote owner to create a library snapshot.
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },
    /// Inspect repository bootstrap and store state without mutation.
    ///
    /// Classify bootstrap/store state without opening or creating a database,
    /// loading an embedder, or creating a lease inode.
    BootstrapInspect(bootstrap::BootstrapInspectArgs),
    /// Create an approved greenfield repository memory generation.
    ///
    /// Validate an approved greenfield repo-sync plan, construct it in a private
    /// generation, and atomically activate it. Managed refresh remains refused.
    BootstrapCreate(bootstrap::BootstrapCreateArgs),
    /// Create or adopt a local project store, its owner, and Codex hippocampus. Never publishes.
    Init(cli_init::InitArgs),
    /// Experimental raw node ingestion; ordinary manual saving uses `save`.
    Ingest(cli_ingest::IngestArgs),
    /// Save a note or experience to an existing current database; never creates or upgrades.
    Save(cli_save::SaveArgs),
    /// List bounded concern pages or record guarded advisory findings (no reconciliation).
    Concern(cli_concern::ConcernArgs),
    /// Compare and replace the complete inspected tag set of an existing node.
    Retag(cli_retag::RetagArgs),
    /// Replace only a note body with an inspected revision guard. No replay/history promise.
    EditBody(cli_edit_body::EditBodyArgs),
    /// Guarded semantic summary replacement with atomic retrieval embedding refresh.
    EditSummary(cli_edit_summary::EditSummaryArgs),
    /// Initialize or inspect explicit storage, or use the compatibility sourced-capture path.
    Capture {
        #[command(subcommand)]
        action: cli_capture::CaptureAction,
    },
    /// Read/edit experiences; `episode append` is a compatibility save path.
    /// Ordinary context recall includes typed episodes; semantic ranking and decay do not.
    Episode {
        #[command(subcommand)]
        action: cli_episode::EpisodeAction,
    },
    /// Build a detached named successor; defaults to single-graph-v1 from
    /// capture/episode predecessors. Use --target-generation concern-v1 for
    /// single-graph-v1, episode-context-v2 for concern-v1, and touchstones-v1
    /// for episode-context-v2 predecessors.
    /// The source is retained, not activated.
    SingleGraphUpgrade(cli_single_graph_upgrade::SingleGraphUpgradeArgs),
    /// Show a node — and, on request, a bounded body range and/or edge prefix.
    Get(GetArgs),
    /// Read a bounded body range (use `get --body` alongside node details).
    ///
    /// `body --raw` is an explicit unbounded escape hatch, incompatible with
    /// `--json` and range options.
    Body(BodyArgs),
    /// List a node's neighbors, for walking the graph.
    Neighbors(NeighborsArgs),
    /// Page through cross-database edges from one local node.
    Remote(RemoteArgs),
    /// Diagnostic raw retrieval; use `recall-context` for bounded model context.
    ///
    /// Searches ANN seeds and spreads activation through the graph.
    Query(QueryArgs),
    /// Bounded model-context JSON; use `query` to inspect raw retrieval.
    ///
    /// Retrieves semantic memories and lexical or linked scenes into bounded JSON.
    /// Both lane capacities follow --max-nodes and share the byte budget.
    /// Links preserve exact historical accounts. Partial windows and
    /// omissions are explicit. This command is always JSON; `--json` is unnecessary.
    RecallContext(RecallContextArgs),
    /// Browse a bounded page of stored nodes, or the native touchstone collection.
    List(ListArgs),
    /// Create (or overwrite) an edge between two nodes.
    Link(LinkArgs),
    /// Resolve a contradiction: winner supersedes loser.
    Supersede {
        #[arg(long)]
        winner: String,
        #[arg(long)]
        loser: String,
    },
    /// Record that two nodes contradict each other.
    Contradict {
        #[arg(long)]
        a: String,
        #[arg(long)]
        b: String,
    },
    /// List open contradictions or record a reviewed verdict.
    ///
    /// Reconciliation triage. With no `--a/--b`, list open contradictions tagged
    /// with their current communities + the community-level conflict aggregate.
    /// With `--a A --b B --as <context-dependent|unresolved>`, record a verdict
    /// (for a real supersession use `supersede`).
    Reconcile {
        #[arg(long)]
        a: Option<String>,
        #[arg(long)]
        b: Option<String>,
        #[arg(long = "as", value_name = "RESOLUTION")]
        resolution: Option<String>,
    },
    /// Report observed node or edge relevance.
    ///
    /// Report node-only or `from -> to` feedback: strengthen, flag-redundant, or
    /// weaken it. Direct CLI feedback is neither receipt-bound nor idempotent;
    /// callers must not replay an ambiguous invocation.
    Feedback(FeedbackArgs),
    /// List open merge candidates awaiting the merge pass.
    Merges,
    /// Adjudicate a merge candidate (the acting half of the merge overlay).
    Merge {
        #[command(subcommand)]
        action: MergeAction,
    },
    /// Walk the graph from a node in a bounded traversal shell.
    ///
    /// Drop into a bounded traversal shell from a start node — the sandbox a
    /// retrieval sub-agent is given instead of this full binary. Movement is
    /// read-only and free within the node budget; `done <used-id...>` explicitly
    /// reflects over the completed trail, while abort/EOF trains nothing.
    Repl(ReplArgs),
    /// Print the always-loaded core set.
    ///
    /// Print the always-loaded core set — every node tagged `core`, with its body
    /// — for injecting into context at session start. Core nodes never decay.
    Core,
    /// Delete a node and its edges and vector.
    ///
    /// Forget a node outright — drop it, its edges, and its vector. Edge decay
    /// never archives or forgets a node.
    Forget { id: String },
    /// Migrate a JSON snapshot to the persistent backend.
    ///
    /// Migrate a current v5 JSON snapshot (at `--db`) to a TouchstonesV1
    /// SQLite store at the same path. The old JSON is retained as
    /// `<db>.snapshot.bak`; older JSON needs explicit `single-graph-upgrade`
    /// targets first (single-graph-v1, concern-v1, then episode-context-v2, then touchstones-v1).
    Migrate,
    /// Re-embed summaries and replace the vector index.
    ///
    /// Re-embed every canonical node, including Archived and historical episode
    /// editions, in bounded batches. JSON replaces a validated detached export;
    /// SQLite publishes a verified TouchstonesV1 copy under the original path
    /// and retains the old database as `<db>.reembed.bak`.
    #[command(visible_alias = "reindex")]
    Reembed,
    /// Run a bounded edge-decay sweep (cold path).
    Decay,
    /// Density GC (cold path): delete never-validated similarity edges and
    /// weaken over-connected nodes' weakest edges.
    Prune,
    /// Show counts and pending curation and maintenance work.
    ///
    /// Maintenance snapshot: counts + what's due (contradictions, merges,
    /// and edge decay).
    Status,
    /// Seed sample data and print a walkthrough (ephemeral; ignores --db).
    Demo {
        #[arg(long)]
        query: Option<String>,
    },
}

#[derive(Subcommand)]
enum SnapshotAction {
    /// Create a snapshot on the selected MCP server; requires --remote.
    Create,
}

#[cfg(test)]
mod reembed_cli_tests {
    use super::{Cli, Command};
    use clap::Parser;

    #[test]
    fn upgrade_target_is_explicit_and_default_remains_historical() {
        use crate::cli_single_graph_upgrade::TargetGeneration;
        for (target, expected) in [
            (None, TargetGeneration::SingleGraphV1),
            (Some("concern-v1"), TargetGeneration::ConcernV1),
            (
                Some("episode-context-v2"),
                TargetGeneration::EpisodeContextV2,
            ),
            (Some("touchstones-v1"), TargetGeneration::TouchstonesV1),
        ] {
            let mut args = vec![
                "mnemed",
                "--db",
                "/tmp/source.db",
                "single-graph-upgrade",
                "--backend",
                "sqlite",
                "--output",
                "/tmp/absent.db",
            ];
            if let Some(target) = target {
                args.extend(["--target-generation", target]);
            }
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::SingleGraphUpgrade(args) = cli.command else {
                panic!("upgrade route")
            };
            assert_eq!(args.target_generation, expected);
        }
        assert!(
            Cli::try_parse_from([
                "mnemed",
                "single-graph-upgrade",
                "--backend",
                "json",
                "--output",
                "/tmp/new.json",
                "--target-generation",
                "guess"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from(["mnemed", "concern"]).is_err(),
            "public concern dispatch is deliberately absent"
        );
    }

    #[test]
    fn legacy_reindex_name_routes_to_offline_reembed() {
        let canonical = Cli::try_parse_from(["mnemed", "reembed"]).unwrap();
        assert!(matches!(canonical.command, Command::Reembed));

        let legacy = Cli::try_parse_from(["mnemed", "reindex"]).unwrap();
        assert!(
            matches!(legacy.command, Command::Reembed),
            "the compatibility name must not reopen the retired engine path"
        );
    }
}

#[derive(Args)]
struct BodyArgs {
    id: String,
    /// Source byte offset. The default is zero.
    #[arg(long, conflicts_with = "raw")]
    offset: Option<u64>,
    /// Maximum source bytes to emit. The default is 65536; the hard maximum is
    /// 1048576. Continue from the offset reported on stderr or in JSON.
    #[arg(long = "max-bytes", conflicts_with = "raw")]
    max_bytes: Option<usize>,
    /// Explicitly read and emit the complete body with no allocation/output
    /// bound. Incompatible with --json and range options.
    #[arg(long, conflicts_with_all = ["offset", "max_bytes"])]
    raw: bool,
}

#[cfg(test)]
mod cli_body_tests {
    use super::*;

    const NODE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[test]
    fn bounded_body_defaults_and_limits_are_exact() {
        assert_eq!(cli_body_limit(None).unwrap(), DEFAULT_CLI_BODY_BYTES);
        assert_eq!(cli_body_limit(Some(1)).unwrap(), 1);
        assert_eq!(
            cli_body_limit(Some(MAX_CLI_BODY_BYTES)).unwrap(),
            MAX_CLI_BODY_BYTES
        );
        assert!(cli_body_limit(Some(0)).is_err());
        assert!(cli_body_limit(Some(MAX_CLI_BODY_BYTES + 1)).is_err());
    }

    #[test]
    fn body_range_arguments_parse() {
        let cli = Cli::try_parse_from([
            "mnemed",
            "body",
            NODE,
            "--offset",
            "42",
            "--max-bytes",
            "99",
        ])
        .unwrap();
        let Command::Body(args) = cli.command else {
            panic!("expected body command");
        };
        assert_eq!(args.offset, Some(42));
        assert_eq!(args.max_bytes, Some(99));
        assert!(!args.raw);
    }

    #[test]
    fn raw_body_is_an_explicit_incompatible_escape_hatch() {
        assert!(Cli::try_parse_from(["mnemed", "body", NODE, "--raw", "--offset", "1"]).is_err());
        assert!(
            Cli::try_parse_from(["mnemed", "body", NODE, "--raw", "--max-bytes", "1"]).is_err()
        );
        let cli = Cli::try_parse_from(["mnemed", "--json", "body", NODE, "--raw"]).unwrap();
        let Command::Body(args) = cli.command else {
            panic!("expected body command");
        };
        assert!(validate_cli_body_mode(true, &args).is_err());
    }
}

#[derive(Subcommand)]
enum MergeAction {
    /// Collapse `loser` into `winner` (near-complete redundancy): repoint the
    /// loser's edges onto the winner and archive it.
    Full {
        #[arg(long)]
        winner: String,
        #[arg(long)]
        loser: String,
    },
    /// Decide a flagged pair shouldn't merge — close the candidacy.
    Keep {
        #[arg(long)]
        a: String,
        #[arg(long)]
        b: String,
    },
}

#[cfg(test)]
mod merge_cli_tests {
    use super::{Cli, Command, MergeAction};
    use clap::Parser;

    #[test]
    fn partial_merge_is_not_a_public_cli_operation() {
        assert!(
            Cli::try_parse_from([
                "mnemed",
                "merge",
                "partial",
                "--a",
                "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "--b",
                "01ARZ3NDEKTSV4RRFFQ69G5FAW",
                "--summary",
                "shared child",
            ])
            .is_err(),
            "an unkeyed cross-store saga must not remain reachable by accident"
        );

        let parsed = Cli::try_parse_from([
            "mnemed",
            "merge",
            "full",
            "--winner",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--loser",
            "01ARZ3NDEKTSV4RRFFQ69G5FAW",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Command::Merge {
                action: MergeAction::Full { .. }
            }
        ));
    }
}

#[derive(Args)]
struct ReplArgs {
    /// Node id to start the walk from (e.g. an entry point found via `query`).
    start: String,
    /// Max distinct nodes the walk may visit (backtracking and revisits are free).
    #[arg(long, default_value_t = 25)]
    budget: usize,
    /// Optional: order each node's edges by relevance to this query, not raw weight.
    #[arg(long)]
    query: Option<String>,
}

#[derive(Args)]
struct FeedbackArgs {
    /// Relevance of `to`, optionally reported along the edge `from -> to`.
    #[arg(value_enum)]
    signal: SignalArg,
    /// The node you arrived from (the edge's source). Omit for node-only
    /// feedback, such as a result surfaced without a primary root.
    #[arg(long)]
    from: Option<String>,
    /// The node you're giving feedback on (the edge's target).
    #[arg(long)]
    to: String,
}

#[derive(Clone, Copy, ValueEnum)]
enum SignalArg {
    /// Relevant and new — strengthen the edge you walked, revive the node.
    Relevant,
    /// Relevant but redundant — strengthen, and flag the pair as a merge candidate.
    NotNew,
    /// Irrelevant — weaken the edge you walked, and the node.
    Irrelevant,
}
impl From<SignalArg> for Signal {
    fn from(s: SignalArg) -> Self {
        match s {
            SignalArg::Relevant => Signal::RelevantNew,
            SignalArg::NotNew => Signal::NotNew,
            SignalArg::Irrelevant => Signal::Irrelevant,
        }
    }
}
impl SignalArg {
    fn label(self) -> &'static str {
        match self {
            SignalArg::Relevant => "relevant",
            SignalArg::NotNew => "not-new",
            SignalArg::Irrelevant => "irrelevant",
        }
    }
}

#[cfg(test)]
mod cli_feedback_tests {
    use super::*;

    #[test]
    fn feedback_accepts_node_only_evidence() {
        let cli = Cli::try_parse_from([
            "mnemed",
            "feedback",
            "relevant",
            "--to",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        ])
        .unwrap();
        let Command::Feedback(args) = cli.command else {
            panic!("expected feedback command");
        };
        assert!(args.from.is_none());
    }

    #[test]
    fn feedback_still_accepts_an_observed_transition() {
        let cli = Cli::try_parse_from([
            "mnemed",
            "feedback",
            "not-new",
            "--from",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--to",
            "01ARZ3NDEKTSV4RRFFQ69G5FAW",
        ])
        .unwrap();
        let Command::Feedback(args) = cli.command else {
            panic!("expected feedback command");
        };
        assert_eq!(args.from.as_deref(), Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
    }
}

#[derive(Args)]
struct GetArgs {
    id: String,
    /// Include a bounded resolved body range.
    #[arg(long)]
    body: bool,
    /// Body source byte offset (requires --body).
    #[arg(long = "body-offset", requires = "body")]
    body_offset: Option<u64>,
    /// Maximum body bytes, up to 1048576 (requires --body; default 65536).
    #[arg(long = "max-body-bytes", requires = "body")]
    max_body_bytes: Option<usize>,
    /// Include the node's edges/neighbors.
    #[arg(long)]
    edges: bool,
}

#[derive(Args)]
struct NeighborsArgs {
    id: String,
    /// Number of neighbors to return. Hard-capped at 64.
    #[arg(long, default_value_t = 16)]
    limit: usize,
    /// Opaque database- and node-bound continuation from a previous page.
    #[arg(long)]
    after: Option<String>,
}

impl NeighborsArgs {
    fn prepare(&self) -> Result<mneme_app::neighbors::PreparedNeighbors, AnyErr> {
        let mut input = json!({"id": self.id, "limit": self.limit});
        if let Some(after) = &self.after {
            input["after"] = json!(after);
        }
        mneme_app::neighbors::PreparedNeighbors::parse(&input).map_err(|error| -> AnyErr { error })
    }
}

#[derive(Args)]
struct RemoteArgs {
    /// Local source node id.
    id: String,
    /// Page size. Hard-capped at 64.
    #[arg(long, default_value_t = 32)]
    limit: usize,
    /// Opaque JSON cursor returned by the previous page.
    #[arg(long)]
    after: Option<String>,
}

#[cfg(test)]
mod cli_remote_tests {
    use super::*;

    #[test]
    fn remote_page_limit_is_hard_bounded() {
        assert_eq!(cli_remote_limit(1).unwrap(), 1);
        assert_eq!(
            cli_remote_limit(MAX_REMOTE_EDGE_PAGE_SIZE).unwrap(),
            MAX_REMOTE_EDGE_PAGE_SIZE
        );
        assert!(cli_remote_limit(0).is_err());
        assert!(cli_remote_limit(MAX_REMOTE_EDGE_PAGE_SIZE + 1).is_err());
    }

    #[test]
    fn remote_page_arguments_parse_without_interpreting_the_opaque_cursor() {
        let cli = Cli::try_parse_from([
            "mnemed",
            "remote",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--limit",
            "17",
            "--after",
            "{\"opaque\":true}",
        ])
        .unwrap();
        let Command::Remote(args) = cli.command else {
            panic!("expected remote command");
        };
        assert_eq!(args.limit, 17);
        assert_eq!(args.after.as_deref(), Some("{\"opaque\":true}"));
    }
}

#[derive(Args)]
struct QueryArgs {
    text: String,
    /// How many ANN hits seed the spread (1..=64).
    #[arg(long)]
    k: Option<usize>,
    /// Graph traversal depth (0..=12).
    #[arg(long)]
    depth: Option<u8>,
    /// Maximum hydrated graph results (1..=256).
    #[arg(long = "max-nodes")]
    max_nodes: Option<usize>,
    /// Relevance floor in [0, 1].
    #[arg(long = "min-relevance")]
    min_relevance: Option<f32>,
    /// Also search archived (forgotten) nodes.
    #[arg(long)]
    archived: bool,
    /// Restrict the seeds to nodes carrying at least one of these tags — e.g.
    /// `--tag pitfall` for an adversarial search. Repeatable.
    #[arg(long = "tag")]
    tags: Vec<String>,
    /// Include a bounded prefix of each hit's body.
    #[arg(long)]
    bodies: bool,
    /// Maximum source bytes per included body (default 65536, max 1048576).
    #[arg(long = "max-body-bytes", requires = "bodies")]
    max_body_bytes: Option<usize>,
}

#[derive(Args)]
struct RecallContextArgs {
    text: String,
    /// How many ANN hits seed the spread (1..=64).
    #[arg(long)]
    k: Option<usize>,
    /// Graph traversal depth (0..=12).
    #[arg(long)]
    depth: Option<u8>,
    /// Discovery and per-lane presentation capacity (1..=256).
    #[arg(long = "max-nodes")]
    max_nodes: Option<usize>,
    /// Relevance floor in [0, 1].
    #[arg(long = "min-relevance")]
    min_relevance: Option<f32>,
    /// Restrict retrieval seeds to nodes carrying at least one of these tags.
    /// Repeatable; each tag is bounded and must be canonical. Tagged recall skips
    /// lexical episode search; links can supply indirect scenes, not tag matches.
    #[arg(long = "tag")]
    tags: Vec<String>,
    /// Exact total context budget in bytes (4096..=32768; default 32768).
    #[arg(long = "max-content-bytes")]
    max_content_bytes: Option<u32>,
}

#[cfg(test)]
mod cli_query_tests;

#[cfg(test)]
mod cli_recall_context_tests;

#[derive(Args)]
struct LinkArgs {
    #[arg(long)]
    from: String,
    #[arg(long)]
    to: String,
    #[arg(long, value_enum, default_value_t = KindArg::Associative)]
    kind: KindArg,
    #[arg(long, default_value_t = 0.5)]
    weight: f32,
    /// Anchor the edge to a byte span of `from`'s body, as `START:END`
    /// (passage-level association).
    #[arg(long)]
    anchor: Option<String>,
    /// Make it a **cross-db** see-also edge: a path to another db, in which `to`
    /// is the target node. Must be run with `--user` (only the user db may
    /// originate one, so project dbs stay self-contained).
    #[arg(long, value_name = "PATH", conflicts_with = "to_remote_db")]
    to_db: Option<PathBuf>,
    /// Cross-database link to an MCP registry name, never a filesystem path.
    /// Requires an owner connection; only user memory may originate the link.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["to_db", "anchor"])]
    to_remote_db: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum StatusArg {
    Active,
    Archived,
}

#[derive(Clone, Copy, ValueEnum)]
enum KindArg {
    Associative,
    Transition,
    Supersedes,
    DerivedFrom,
}
impl From<KindArg> for EdgeKind {
    fn from(k: KindArg) -> Self {
        match k {
            KindArg::Associative => EdgeKind::Associative,
            KindArg::Transition => EdgeKind::Transition,
            KindArg::Supersedes => EdgeKind::Supersedes,
            KindArg::DerivedFrom => EdgeKind::DerivedFrom,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), AnyErr> {
    let cli = Cli::parse();
    let json = cli.json;

    if let Command::Init(args) = &cli.command {
        if cli.user || cli.db.is_some() || cli.connection.remote.is_some() {
            return cli_init::refuse(
                "init forbids --db/MNEME_DB, --user and --remote; use init --root PATH for local project setup",
                json,
            );
        }
        return cli_init::run(args, json).await;
    }

    if let Command::Client(args) = cli.command {
        if cli.user || cli.db.is_some() || cli.connection.remote.is_some() || cli.json {
            return Err("client forbids database, remote, user, and JSON global options".into());
        }
        return cli_client::run(args).await;
    }

    if let Command::Tui(args) = &cli.command {
        return cli_tui::run(args, &cli.connection, cli.user, cli.db.as_deref(), json).await;
    }

    if let Command::Stores(args) = &cli.command {
        return cli_sources::run(args, &cli.connection, cli.user, cli.db.as_deref(), json);
    }

    // Library refs are a separate, immutable product. Never interpret global
    // database or remote selectors as fallbacks for their config path.
    if let Command::Library(args) = &cli.command {
        if cli.db.is_some() || cli.user || cli.connection.remote.is_some() {
            return Err(
                "library forbids --db/MNEME_DB, --user and --remote; use library --config PATH"
                    .into(),
            );
        }
        return cli_library::run(args, json).await;
    }

    // Remote mode is a client of the existing owner, never an alternative store
    // opener. Resolve the connection and validate the entire command before
    // contacting the server, and before any local path/lease/model admission.
    if let Some(selected) =
        cli_owner::resolve(&cli.command, &cli.connection, cli.user, cli.db.as_deref())?
    {
        if let Command::Repl(args) = &cli.command {
            return remote_repl::run(
                selected,
                json,
                &args.start,
                args.budget,
                args.query.as_deref(),
            )
            .await;
        }
        let remote = selected.remote;
        let mut request = remote_commands::prepare(&cli.command, &remote.database)?;
        if let Some(owner) = &selected.owner {
            owner.check_request(&request)?;
        }
        let connect = remote_transport::RemoteClient::connect(&remote.connection);
        let mut client = tokio::select! {
            result = connect => result.map_err(|error| error.to_string())?,
            _ = tokio::signal::ctrl_c() => return Err("remote connection interrupted".into()),
        };
        if let Some(owner) = &selected.owner {
            let admitted = tokio::select! {
                result = owner.verify_and_guard(&mut client, &mut request, &remote.database) => result,
                _ = tokio::signal::ctrl_c() => Err("configured owner identity check interrupted; no operation sent".into()),
            };
            if let Err(error) = admitted {
                client.close().await;
                return Err(error);
            }
        }
        if let Some(save) = &request.save {
            let kind = save.kind().as_str();
            let has_links = request
                .arguments
                .get("links")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|links| !links.is_empty());
            if !client.supports_save_kind(kind) || (has_links && !client.supports_save_links(kind))
            {
                client.close().await;
                return Err("remote save is unavailable: the server does not advertise the requested canonical SAVE contract; use a SAVE-capable server (no capture/ingest fallback was attempted)".into());
            }
        }
        if request.edit_body.is_some() && !client.supports_edit_body() {
            client.close().await;
            return Err("remote edit-body requires a server advertising the checked edit_body contract; no fallback attempted".into());
        }
        if request.edit_summary.is_some() && !client.supports_edit_summary() {
            client.close().await;
            return Err("remote edit-summary requires a server advertising the checked edit_summary contract; no fallback attempted".into());
        }
        if let Some(retag) = &request.retag {
            if !client.supports_retag() {
                client.close().await;
                return Err("remote retag requires a server advertising the complete checked retag contract; no fallback attempted".into());
            }
            if retag.request.requires_content_guards() && !client.supports_retag_content_guards() {
                client.close().await;
                return Err("remote retag content guards are unavailable on this owner; update the owner build. No unguarded fallback was attempted".into());
            }
        }
        if let Some(concern) = &request.concern {
            if !client.supports_concern_action(concern.request.action()) {
                client.close().await;
                return Err("remote concern requires a server advertising the checked concern contract; no fallback attempted".into());
            }
        }
        if request.requires_capture_links && !client.supports_capture_links() {
            client.close().await;
            return Err("remote capture links are unavailable: the server does not advertise a compatible `links` schema; use a server that supports atomic capture links, or omit links".into());
        }
        if (request.arguments.get("touchstone").is_some()
            || (request.tool == "list" && request.arguments["kind"] == "touchstones"))
            && !remote_touchstone::advertised(client.tool_catalog(), request.tool)
        {
            client.close().await;
            return Err("remote touchstones are unavailable: the server does not advertise the native touchstone schema; no fallback or write attempted".into());
        }
        if request.tool == "list"
            && request.arguments["kind"] == "nodes"
            && !remote_touchstone::inventory_advertised(client.tool_catalog())
        {
            client.close().await;
            return Err("this MCP owner does not support node inventory yet; update the owner build, not just mnemed. No local database fallback was attempted".into());
        }
        if request.tool == "list"
            && request.arguments["kind"] == "tags"
            && !client.supports_list_tags()
        {
            client.close().await;
            return Err("this MCP owner does not support tag vocabulary; update the owner build. No local database fallback was attempted".into());
        }
        let guarded_edit_expected_db_id = request
            .arguments
            .get("expected_db_id")
            .and_then(serde_json::Value::as_str)
            .map(str::parse::<Ulid>)
            .transpose()?;
        let result = tokio::select! {
            result = client.call_tool(request.tool, request.arguments) => result,
            _ = tokio::signal::ctrl_c() => Err("remote operation interrupted; a submitted write may have completed; do not blindly retry".into()),
        };
        client.close().await;
        let result = result.map_err(|error| error.to_string())?;
        if let Some(owner) = &selected.owner {
            owner.check_result(request.access, &result, &remote.database)?;
        }
        if let Some(save) = &request.save {
            save.verify_write_receipt_json(&result).map_err(|error| -> AnyErr {
                format!("remote SAVE receipt could not be verified; submitted write outcome is unknown: {error}; do not blindly retry").into()
            })?;
            if result["db"].as_str() != Some(remote.database.as_str())
                || result["db_id"]
                    .as_str()
                    .is_none_or(|id| id.parse::<Ulid>().is_err())
            {
                return Err("remote SAVE receipt database mismatch; the submitted write outcome is unknown; do not blindly retry".into());
            }
        }
        if let Some(concern) = &request.concern {
            concern.request.validate_routed_response_json(&result, &remote.database, concern.expected_db_id)
                .map_err(|error| -> AnyErr { format!("remote concern acknowledgement invalid: {error}; a submitted write may have completed; do not blindly retry").into() })?;
        }
        if let Some(retag) = &request.retag {
            retag.request.validate_routed_response_json(&result, &remote.database, guarded_edit_expected_db_id)
                .map_err(|error| -> AnyErr { format!("remote retag acknowledgement invalid: {error}; submitted write outcome is unknown; do not blindly retry").into() })?;
        }
        if let Some(edit) = &request.edit_body {
            edit.request.validate_routed_response_json(&result, &remote.database, guarded_edit_expected_db_id)
                .map_err(|error| -> AnyErr { format!("remote edit-body acknowledgement invalid: {error}; submitted outcome is unknown; inspect before a new intent").into() })?;
        }
        if let Some(edit) = &request.edit_summary {
            edit.request.validate_routed_response_json(&result, &remote.database, guarded_edit_expected_db_id)
                .map_err(|error| -> AnyErr { format!("remote edit-summary acknowledgement invalid: {error}; submitted outcome is unknown; inspect before a new intent").into() })?;
        }
        return remote_commands::render(&cli.command, &result, json);
    }

    match cli.command {
        Command::Init(_) | Command::Client(_) | Command::Library(_) | Command::Tui(_) | Command::Stores(_) => {
            unreachable!("library handled before connection/store admission")
        }
        Command::Snapshot { .. } => Err("snapshot create requires an existing MCP owner; omit --db and configure the owner, or select --remote URL".into()),
        Command::Demo { query } => run_demo(query).await,
        Command::BootstrapInspect(args) => {
            if cli.user || cli.db.is_some() {
                return Err(
                    "bootstrap-inspect examines the project root selected by --root; --db and --user are forbidden"
                        .into(),
                );
            }
            bootstrap::run_inspect(args, json)
        }
        Command::BootstrapCreate(args) => {
            if cli.user || cli.db.is_some() {
                return Err(
                    "bootstrap-create targets the project root selected by --root; --db and --user are forbidden"
                        .into(),
                );
            }
            bootstrap::run(args, json).await
        }
        Command::Ingest(args) => {
            let prepared = cli_ingest::PreparedIngest::read(&args)?;
            // Freeze the optional project anchor before opening persistent state.
            let commit = (!cli.user).then(git_head_cwd).flatten();
            let prepared = prepared.with_origin_commit(commit.as_deref())?;
            let db = resolve_db(cli.db, cli.user)?;
            if let Some(parent) = db.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            // Retain the lease through both ingestion and the snapshot checkpoint.
            let db_lock = Arc::new(mneme_store_path::StoreLease::acquire(&db)?);
            let (mem, saver, _) = open(&db, db_lock.clone(), None)?;
            let id = prepared.run(&mem).await?;
            cli_ingest::render(id, json);
            saver.save()?;
            Ok(())
        }
        Command::EditBody(args) => {
            let prepared = cli_edit_body::Prepared::read(&args)?;
            let db = cli_capture::canonical_target(&resolve_db(cli.db, cli.user)?)?;
            let lease = Arc::new(mneme_store_path::StoreLease::acquire(&db)?);
            cli_save::check_target(&db, lease.as_ref())?;
            let (mem, saver, db_id) = open(&db, lease.clone(), None)?;
            if prepared.expected_db_id.is_some_and(|expected| expected != db_id) {
                return Err("edit-body expected_db_id mismatch; no mutation performed".into());
            }
            let mut result = prepared.request.execute(&mem).await.map_err(|error| -> AnyErr { error })?;
            saver.save()?;
            result["db"] = serde_json::json!(db);
            result["db_id"] = serde_json::json!(db_id.to_string());
            cli_edit_body::render(&result, json);
            Ok(())
        }
        Command::EditSummary(args) => {
            let prepared = cli_edit_summary::Prepared::read(&args)?;
            let db = cli_capture::canonical_target(&resolve_db(cli.db, cli.user)?)?;
            let lease = Arc::new(mneme_store_path::StoreLease::acquire(&db)?);
            cli_save::check_target(&db, lease.as_ref())?;
            let (mem, saver, db_id) = open(&db, lease.clone(), None)?;
            if prepared.expected_db_id.is_some_and(|expected| expected != db_id) {
                return Err("edit-summary expected_db_id mismatch; no mutation performed".into());
            }
            let mut result = prepared.request.execute(&mem, db_id).await.map_err(|error| -> AnyErr { error })?;
            saver.save()?;
            result["db"] = serde_json::json!(db);
            result["db_id"] = serde_json::json!(db_id.to_string());
            cli_edit_summary::render(&result, json);
            Ok(())
        }
        Command::Retag(args) => {
            let prepared = cli_retag::Prepared::read(&args)?;
            let db = cli_capture::canonical_target(&resolve_db(cli.db, cli.user)?)?;
            let lease = Arc::new(mneme_store_path::StoreLease::acquire(&db)?);
            cli_save::check_target(&db, lease.as_ref())?;
            let (mem, saver, db_id) = open(&db, lease.clone(), None)?;
            if prepared.expected_db_id.is_some_and(|expected| expected != db_id) {
                return Err("retag expected_db_id mismatch; no mutation performed".into());
            }
            let mut result = prepared.request.execute(&mem).await.map_err(|error| -> AnyErr { error })?;
            if result["changed"] == true { saver.save()?; }
            result["db"] = serde_json::json!(db);
            result["db_id"] = serde_json::json!(db_id.to_string());
            cli_retag::render(&result, json);
            Ok(())
        }
        Command::Concern(args) => {
            let prepared = cli_concern::PreparedConcern::read(&args)?;
            let db = resolve_db(cli.db, cli.user)?;
            cli_concern::run(prepared, &db, json).await
        }
        Command::Save(args) => {
            let prepared = cli_save::PreparedSave::read(&args)?;
            let db = cli_capture::canonical_target(&resolve_db(cli.db, cli.user)?)?;
            let db_lock = Arc::new(mneme_store_path::StoreLease::acquire(&db)?);
            // SAVE is existing-current-only for both kinds. Never call episode's
            // compatibility absent-store append constructor or generic fallback.
            cli_save::check_target(&db, db_lock.as_ref())?;
            let (mem, saver, db_id) = open(&db, db_lock.clone(), None)?;
            let commit = (!cli.user).then(git_head_cwd).flatten();
            let mut receipt = prepared
                .prepared
                .run(&mem, db_id, commit.as_deref())
                .await
                .map_err(|e| -> AnyErr { e })?;
            saver.save()?;
            receipt["db"] = serde_json::json!(db);
            receipt["db_id"] = serde_json::json!(db_id.to_string());
            cli_save::render(&receipt, json);
            Ok(())
        }
        Command::Capture { action } => match action {
            cli_capture::CaptureAction::Init => {
                if cli.user || cli.db.is_none() {
                    return Err(
                        "capture init requires an explicit --db target and forbids --user".into(),
                    );
                }
                let db = cli_capture::canonical_target(&resolve_db(cli.db, false)?)?;
                cli_capture::init(&db, json).await
            }
            cli_capture::CaptureAction::Inspect => {
                if cli.user || cli.db.is_none() {
                    return Err("capture inspect requires an explicit --db target and forbids --user".into());
                }
                let db = cli_capture::canonical_target(&resolve_db(cli.db, false)?)?;
                cli_capture::inspect(&db, json)
            }
            cli_capture::CaptureAction::Add(args) => {
                // The whole JSON envelope is read and validated before path
                // resolution, parent creation, lease, open, or inference.
                let prepared = cli_capture::PreparedCapture::read(&args)?;
                let db = cli_capture::canonical_target(&resolve_db(cli.db, cli.user)?)?;
                let db_lock = Arc::new(mneme_store_path::StoreLease::acquire(&db)?);
                cli_capture::check_add_target(&db, db_lock.as_ref())?;
                let (mem, saver, _db_id) = open(&db, db_lock, None)?;
                prepared.run(&mem, json, cli.user).await?;
                // A replay must still checkpoint the reference snapshot backend.
                saver.save()?;
                Ok(())
            }
        },
        Command::Episode { action } => {
            let prepared = action.prepare()?;
            let db = cli_capture::canonical_target(&resolve_db(cli.db, cli.user)?)?;
            cli_episode::admit_target(&db, prepared.action()).await?;
            let db_lock = Arc::new(mneme_store_path::StoreLease::acquire(&db)?);
            cli_episode::check_target(&db, db_lock.as_ref())?;
            // Keep the frontend guard through the snapshot checkpoint too;
            // the reference backend does not retain a lease in its handle.
            let (mem, saver, db_id) = open(&db, db_lock.clone(), None)?;
            let is_mutation = prepared.is_mutation();
            let commit = (!cli.user && is_mutation).then(git_head_cwd).flatten();
            let result = prepared
                .run(&mem, db_id, commit.as_deref())
                .await
                .map_err(|error| -> AnyErr { error })?;
            if is_mutation {
                saver.save()?;
            }
            cli_episode::render(&result, json);
            Ok(())
        }
        Command::SingleGraphUpgrade(args) => {
            if cli.user || cli.db.is_none() {
                return Err(
                    "single-graph-upgrade requires an explicit --db source and forbids --user"
                        .into(),
                );
            }
            let db = resolve_db(cli.db, false)?;
            cli_single_graph_upgrade::run_single_graph(&db, &args, json).await
        }
        cmd => {
            if let Command::List(args) = &cmd {
                args.prepare()?; // strict admission before path resolution
            }
            if let Command::Neighbors(args) = &cmd {
                args.prepare()?;
            }
            if let Command::Link(args) = &cmd {
                if args.to_remote_db.is_some() {
                    return Err("--to-remote-db requires an MCP owner; do not combine it with --db/MNEME_DB".into());
                }
            }
            let db = resolve_db(cli.db, cli.user)?;
            // The per-user db lives under a directory that may not exist yet.
            if let Some(parent) = db.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            // One-shot CLI processes otherwise interleave engine-level
            // read-modify-write sequences and silently lose learning counters.
            // Hold an OS lock for the complete command. The long-lived MCP host
            // performs its own in-process coordination.
            let db_lock = Arc::new(mneme_store_path::StoreLease::acquire(&db)?);
            match cmd {
                // Migration needs raw access to both backends, so it runs outside
                // the normal open/dispatch flow.
                Command::Migrate => run_migrate(&db, db_lock.clone()).await,
                Command::Reembed => run_reembed(&db, db_lock.clone()).await,
                cmd => {
                    let feedback_epoch = matches!(&cmd, Command::Feedback(_) | Command::Repl(_))
                        .then(|| Ulid::new().to_string());
                    let (mem, saver, db_id) =
                        open(&db, db_lock.clone(), feedback_epoch.as_deref())?;
                    if dispatch(&mem, json, cli.user, &db, db_id, cmd).await? {
                        saver.save()?;
                    }
                    Ok(())
                }
            }
        }
    }
}

#[cfg(feature = "cozo")]
async fn run_migrate(db: &Path, lease: Arc<mneme_store_path::StoreLease>) -> Result<(), AnyErr> {
    if !db.exists() || !is_snapshot(db)? {
        return Err(
            "--db is not a JSON snapshot — nothing to migrate (already cozo, or missing)".into(),
        );
    }
    let backup = db.with_extension("snapshot.bak");
    match std::fs::symlink_metadata(&backup) {
        Ok(_) => {
            return Err(format!(
                "refusing to overwrite existing migration backup {}",
                backup.display()
            )
            .into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let mem = MemStore::load(db).map_err(|error| format!(
        "migrate requires current v5 JSON: {error}; v4 JSON needs `mnemed --db <SOURCE> single-graph-upgrade --backend json --target-generation touchstones-v1 --output <ABSENT_ABSOLUTE_PATH>` first; v3 needs episode-context-v2 before that, v2 needs concern-v1, and flat v1 starts with single-graph-v1"
    ))?;
    let expected = mem.export();
    let legacy_embeddings = mem.embedding_fingerprint()?.is_none() && mem.has_embedding_data()?;
    let seal = CurrentRebuildSeal::capture(db, &expected, &lease)?;
    publish_detached_current(db, &backup, &mem, &expected, &lease, &seal, "migrate").await?;
    let n = expected.nodes.len();
    println!(
        "migrated {n} nodes to cozo (sqlite) at {}; snapshot backed up to {}",
        db.display(),
        backup.display()
    );
    if legacy_embeddings {
        println!(
            "legacy vectors had no verifiable embedding identity; run \
             `mnemed --db <PATH> reembed` for {} before normal use",
            db.display()
        );
    }
    Ok(())
}

#[cfg(feature = "cozo")]
#[derive(Clone, PartialEq, Eq)]
struct CurrentRebuildSeal {
    source_sha256: [u8; 32],
    bodies: Vec<(String, u64, [u8; 32])>,
}

#[cfg(feature = "cozo")]
impl CurrentRebuildSeal {
    fn capture(
        db: &Path,
        export: &mneme_cozo::StoreExport,
        lease: &mneme_store_path::StoreLease,
    ) -> Result<Self, AnyErr> {
        lease.require_guards(db)?;
        let source_sha256 = bounded_regular_sha256(db)?;
        let mut names = std::collections::BTreeSet::new();
        for node in &export.nodes {
            let reference = node.body().as_str();
            // Only fs:// names refer to local sidecar assets. Other valid
            // BodyRefs remain in the exact export, but their content is not
            // fetched or vouched for by this rebuild.
            let Some(name) = reference.strip_prefix("fs://") else {
                continue;
            };
            let mut parts = Path::new(name).components();
            if !matches!(parts.next(), Some(std::path::Component::Normal(_)))
                || parts.next().is_some()
                || name.contains('/')
            {
                return Err(
                    "detached current rebuild requires single relative fs:// body names".into(),
                );
            }
            names.insert(name.to_owned());
        }
        let mut total = 0_u64;
        let mut bodies = Vec::new();
        if !names.is_empty() {
            let _ = FsStore::open_existing(db.with_extension("bodies"))?;
        }
        for name in names {
            let path = db.with_extension("bodies").join(&name);
            let metadata = std::fs::symlink_metadata(&path)?;
            total = total
                .checked_add(metadata.len())
                .ok_or("body inventory overflow")?;
            if total > 512 * 1024 * 1024 {
                return Err("detached rebuild bodies exceed 512 MiB".into());
            }
            bodies.push((name, metadata.len(), bounded_regular_sha256(&path)?));
        }
        lease.require_guards(db)?;
        Ok(Self {
            source_sha256,
            bodies,
        })
    }

    fn verify(
        &self,
        db: &Path,
        export: &mneme_cozo::StoreExport,
        lease: &mneme_store_path::StoreLease,
    ) -> Result<(), AnyErr> {
        let current = Self::capture(db, export, lease)?;
        if &current != self {
            return Err(
                "detached rebuild source database or body bytes changed before publication".into(),
            );
        }
        Ok(())
    }
}

#[cfg(feature = "cozo")]
fn bounded_regular_sha256(path: &Path) -> Result<[u8; 32], AnyErr> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > 512 * 1024 * 1024 {
        return Err("detached rebuild source must be a regular file of at most 512 MiB".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err("detached rebuild source has multiple hard links".into());
        }
    }
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        if size > 512 * 1024 * 1024 {
            return Err("detached rebuild source grew past 512 MiB".into());
        }
        hasher.update(&buffer[..n]);
    }
    if file.metadata()?.len() != size {
        return Err("detached rebuild source changed during hash".into());
    }
    Ok(hasher.finalize().into())
}

#[cfg(feature = "cozo")]
async fn publish_detached_current(
    db: &Path,
    backup: &Path,
    snapshot: &MemStore,
    expected: &mneme_cozo::StoreExport,
    lease: &Arc<mneme_store_path::StoreLease>,
    seal: &CurrentRebuildSeal,
    purpose: &str,
) -> Result<(), AnyErr> {
    match std::fs::symlink_metadata(backup) {
        Ok(_) => {
            return Err(format!(
                "refusing to overwrite existing migration backup {}",
                backup.display()
            )
            .into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut stage = MigrationTemp::reserve_private_dir_beside(db, purpose)?;
    if let Err(error) =
        CozoStore::materialize_fresh_current(stage.path(), Ulid::new(), snapshot).await
    {
        let retained = stage.disarm();
        return Err(Box::new(DetachedCurrentMaterializationFailure {
            stage: retained,
            source: error,
        }));
    }
    let stage_lease = Arc::new(mneme_store_path::StoreLease::acquire(stage.path())?);
    let verified =
        CozoStore::open_existing_persistent(stage.path(), expected.dim, stage_lease.clone())?;
    verified.verify_import(expected).await?;
    verified.prepare_for_file_move()?;
    drop(verified);
    drop(stage_lease);
    reject_sqlite_sidecars(stage.path())?;
    std::fs::File::open(stage.path())?.sync_all()?;
    seal.verify(db, expected, lease)?;
    install_verified_migration(
        db,
        backup,
        &mut stage,
        seal.source_sha256,
        || seal.verify(db, expected, lease),
        |from, to| std::fs::rename(from, to),
    )?;
    // The final rename is visible. A postflight error is now recovery-needed,
    // never a claim that the source was rolled back.
    #[cfg(test)]
    migration_install_checkpoint("before_postflight_open").map_err(|error| {
        installed_postflight_failure(db, backup, format!("before postflight open: {error}"))
    })?;
    let final_store = CozoStore::open_existing_persistent(db, expected.dim, lease.clone())
        .map_err(|error| {
            installed_postflight_failure(db, backup, format!("postflight open failed: {error}"))
        })?;
    #[cfg(test)]
    migration_install_checkpoint("after_postflight_open").map_err(|error| {
        installed_postflight_failure(db, backup, format!("after postflight open: {error}"))
    })?;
    #[cfg(test)]
    migration_install_checkpoint("before_postflight_verify").map_err(|error| {
        installed_postflight_failure(db, backup, format!("before postflight verify: {error}"))
    })?;
    final_store.verify_import(expected).await.map_err(|error| {
        installed_postflight_failure(
            db,
            backup,
            format!("postflight verification failed: {error}"),
        )
    })?;
    #[cfg(test)]
    migration_install_checkpoint("after_postflight_verify").map_err(|error| {
        installed_postflight_failure(db, backup, format!("after postflight verify: {error}"))
    })?;
    #[cfg(test)]
    migration_install_checkpoint("before_postflight_checkpoint").map_err(|error| {
        installed_postflight_failure(db, backup, format!("before postflight checkpoint: {error}"))
    })?;
    final_store.prepare_for_file_move().map_err(|error| {
        installed_postflight_failure(db, backup, format!("postflight checkpoint failed: {error}"))
    })?;
    #[cfg(test)]
    migration_install_checkpoint("after_postflight_checkpoint").map_err(|error| {
        installed_postflight_failure(db, backup, format!("after postflight checkpoint: {error}"))
    })?;
    drop(final_store);
    Ok(())
}

#[cfg(feature = "cozo")]
struct DetachedCurrentMaterializationFailure {
    stage: PathBuf,
    source: mneme_cozo::FreshCurrentMaterializationErrorV1,
}

#[cfg(feature = "cozo")]
impl std::fmt::Display for DetachedCurrentMaterializationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "detached current materialization failed; preserve stage {} (publication {:?}, retained authority {}): {}",
            self.stage.display(),
            self.source.target_publication_state(),
            self.source.retains_authority(),
            self.source
        )
    }
}
#[cfg(feature = "cozo")]
impl std::fmt::Debug for DetachedCurrentMaterializationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
#[cfg(feature = "cozo")]
impl std::error::Error for DetachedCurrentMaterializationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(feature = "cozo")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MigrationInstallState {
    NotPublished,
    InstalledDurabilityUnknown,
    InstalledPostflightFailure,
    Ambiguous,
}

#[cfg(feature = "cozo")]
#[derive(Debug)]
struct MigrationInstallFailure {
    state: MigrationInstallState,
    db: PathBuf,
    backup: PathBuf,
    stage: Option<PathBuf>,
    detail: String,
}

#[cfg(feature = "cozo")]
impl MigrationInstallFailure {
    fn new(
        state: MigrationInstallState,
        db: &Path,
        backup: &Path,
        stage: &Path,
        detail: String,
    ) -> Self {
        Self {
            state,
            db: db.to_owned(),
            backup: backup.to_owned(),
            stage: Some(stage.to_owned()),
            detail,
        }
    }

    fn postflight(db: &Path, backup: &Path, detail: String) -> Self {
        Self {
            state: MigrationInstallState::InstalledPostflightFailure,
            db: db.to_owned(),
            backup: backup.to_owned(),
            stage: None,
            detail,
        }
    }
}

#[cfg(feature = "cozo")]
impl std::fmt::Display for MigrationInstallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "migration install {:?} at {}; backup {} retained",
            self.state,
            self.db.display(),
            self.backup.display()
        )?;
        if let Some(stage) = &self.stage {
            write!(f, "; stage {}", stage.display())?;
        }
        write!(f, ": {}", self.detail)
    }
}

#[cfg(feature = "cozo")]
impl std::error::Error for MigrationInstallFailure {}

#[cfg(feature = "cozo")]
fn installed_postflight_failure(db: &Path, backup: &Path, detail: String) -> AnyErr {
    Box::new(MigrationInstallFailure::postflight(db, backup, detail))
}

#[cfg(feature = "cozo")]
fn same_file_identity(left: Option<&std::fs::Metadata>, right: Option<&std::fs::Metadata>) -> bool {
    let (Some(left), Some(right)) = (left, right) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        false
    }
}

#[cfg(feature = "cozo")]
fn install_verified_migration(
    db: &Path,
    backup: &Path,
    temp: &mut MigrationTemp,
    expected_source_sha256: [u8; 32],
    source_guard: impl FnOnce() -> Result<(), AnyErr>,
    install: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), AnyErr> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Seek, SeekFrom, Write};

    // A separate inode leaves the selected source singly linked and normally
    // admissible even if the process dies partway through copying the backup.
    // create_new also turns any partial backup into a deliberate retry fence.
    #[cfg(test)]
    migration_install_checkpoint("before_backup_create").map_err(|error| {
        Box::new(MigrationInstallFailure::new(
            MigrationInstallState::NotPublished,
            db,
            backup,
            temp.path(),
            format!("backup creation did not start: {error}"),
        )) as AnyErr
    })?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut backup_file = options.open(backup).map_err(|error| {
        Box::new(MigrationInstallFailure::new(
            MigrationInstallState::NotPublished,
            db,
            backup,
            temp.path(),
            format!(
                "backup create_new refused without replacing source or existing backup: {error}"
            ),
        )) as AnyErr
    })?;
    let backup_durability = (|| {
        #[cfg(test)]
        migration_install_checkpoint("after_backup_create")?;
        let mut source_options = std::fs::OpenOptions::new();
        source_options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            source_options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut source_file = source_options.open(db)?;
        let source_meta = source_file.metadata()?;
        if !source_meta.is_file() {
            return Err(std::io::Error::other(
                "migration source is not a regular file",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if source_meta.nlink() != 1 {
                return Err(std::io::Error::other("migration source is multiply linked"));
            }
        }
        let source_len = source_meta.len();
        let mut remaining = source_len;
        let mut buffer = [0_u8; 64 * 1024];
        while remaining != 0 {
            let wanted = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
            let read = source_file.read(&mut buffer[..wanted])?;
            if read == 0 {
                return Err(std::io::Error::other(
                    "migration source shrank during backup copy",
                ));
            }
            backup_file.write_all(&buffer[..read])?;
            remaining -= read as u64;
            #[cfg(test)]
            migration_install_checkpoint("after_backup_copy_chunk")?;
        }
        if source_file.read(&mut buffer[..1])? != 0 || source_file.metadata()?.len() != source_len {
            return Err(std::io::Error::other(
                "migration source grew during backup copy",
            ));
        }
        #[cfg(test)]
        migration_install_checkpoint("before_backup_file_sync")?;
        backup_file.sync_all()?;
        #[cfg(test)]
        migration_install_checkpoint("after_backup_file_sync")?;
        backup_file.seek(SeekFrom::Start(0))?;
        let mut hasher = Sha256::new();
        let mut backed_up = 0_u64;
        loop {
            let read = backup_file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            backed_up = backed_up
                .checked_add(read as u64)
                .ok_or_else(|| std::io::Error::other("backup length overflow"))?;
            if backed_up > source_len {
                return Err(std::io::Error::other(
                    "migration backup grew during readback",
                ));
            }
            hasher.update(&buffer[..read]);
        }
        if backed_up != source_len
            || backup_file.metadata()?.len() != source_len
            || hasher.finalize().as_slice() != expected_source_sha256
        {
            return Err(std::io::Error::other(
                "migration backup does not match sealed source",
            ));
        }
        #[cfg(test)]
        migration_install_checkpoint("after_backup_readback")?;
        #[cfg(test)]
        migration_install_checkpoint("before_backup_parent_sync")?;
        sync_parent_required(backup)?;
        #[cfg(test)]
        migration_install_checkpoint("after_backup_parent_sync")?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(error) = backup_durability {
        return Err(clean_up_owned_backup_copy(
            db,
            backup,
            &backup_file,
            temp,
            &format!("backup durability failed: {error}"),
        ));
    }
    if let Err(error) = source_guard() {
        return Err(clean_up_owned_backup_copy(
            db,
            backup,
            &backup_file,
            temp,
            &format!("source changed before publication: {error}"),
        ));
    }
    let old_identity = match std::fs::metadata(db) {
        Ok(identity) => identity,
        Err(error) => {
            return Err(clean_up_owned_backup_copy(
                db,
                backup,
                &backup_file,
                temp,
                &format!("source identity unavailable before publication: {error}"),
            ));
        }
    };
    let stage_identity = match std::fs::metadata(temp.path()) {
        Ok(identity) => identity,
        Err(error) => {
            return Err(clean_up_owned_backup_copy(
                db,
                backup,
                &backup_file,
                temp,
                &format!("stage identity unavailable before publication: {error}"),
            ));
        }
    };
    let installed = (|| {
        #[cfg(test)]
        migration_install_checkpoint("before_final_rename")?;
        install(temp.path(), db)
    })();
    if let Err(install_error) = installed {
        let db_now = std::fs::metadata(db).ok();
        let stage_now = std::fs::metadata(temp.path()).ok();
        if same_file_identity(db_now.as_ref(), Some(&old_identity))
            && same_file_identity(stage_now.as_ref(), Some(&stage_identity))
        {
            return Err(clean_up_owned_backup_copy(
                db,
                backup,
                &backup_file,
                temp,
                &format!("failed to install verified sqlite database: {install_error}"),
            ));
        }
        if same_file_identity(db_now.as_ref(), Some(&stage_identity)) && stage_now.is_none() {
            let installed_stage = temp.disarm();
            return Err(Box::new(MigrationInstallFailure::new(
                MigrationInstallState::InstalledDurabilityUnknown,
                db,
                backup,
                &installed_stage,
                format!(
                    "rename reported error after installing verified database: {install_error}"
                ),
            )));
        }
        let retained = temp.disarm();
        return Err(Box::new(MigrationInstallFailure::new(
            MigrationInstallState::Ambiguous,
            db,
            backup,
            &retained,
            format!("rename outcome cannot be classified: {install_error}"),
        )));
    }
    let _ = temp.disarm();
    let final_durability = (|| {
        #[cfg(test)]
        migration_install_checkpoint("after_final_rename")?;
        #[cfg(test)]
        migration_install_checkpoint("before_final_parent_sync")?;
        sync_parent_required(db)?;
        #[cfg(test)]
        migration_install_checkpoint("after_final_parent_sync")?;
        Ok::<(), std::io::Error>(())
    })();
    final_durability.map_err(|error| {
        Box::new(MigrationInstallFailure::new(
            MigrationInstallState::InstalledDurabilityUnknown,
            db,
            backup,
            db,
            format!("directory durability is uncertain: {error}"),
        )) as AnyErr
    })?;
    Ok(())
}

#[cfg(feature = "cozo")]
fn clean_up_owned_backup_copy(
    db: &Path,
    backup: &Path,
    backup_file: &std::fs::File,
    temp: &mut MigrationTemp,
    cause: &str,
) -> AnyErr {
    let exact_owned = std::fs::symlink_metadata(backup)
        .ok()
        .zip(backup_file.metadata().ok())
        .is_some_and(|(named, opened)| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                named.file_type().is_file()
                    && named.dev() == opened.dev()
                    && named.ino() == opened.ino()
            }
            #[cfg(not(unix))]
            {
                let _ = (named, opened);
                false
            }
        });
    #[cfg(test)]
    let cleanup_allowed = migration_install_checkpoint("before_owned_backup_cleanup").is_ok();
    #[cfg(not(test))]
    let cleanup_allowed = true;
    if exact_owned && cleanup_allowed && std::fs::remove_file(backup).is_ok() {
        let cleanup_durability = (|| {
            #[cfg(test)]
            migration_install_checkpoint("before_backup_cleanup_parent_sync")?;
            sync_parent_required(db)
        })();
        if let Err(error) = cleanup_durability {
            let staged = temp.disarm();
            return Box::new(MigrationInstallFailure::new(
                MigrationInstallState::NotPublished,
                db,
                backup,
                &staged,
                format!(
                    "{cause}; selected source was not replaced, but backup cleanup durability is uncertain ({error}); preserve stage"
                ),
            ));
        }
        return Box::new(MigrationInstallFailure::new(
            MigrationInstallState::NotPublished,
            db,
            backup,
            temp.path(),
            format!("{cause}; selected source was not replaced"),
        ));
    }
    let staged = temp.disarm();
    Box::new(MigrationInstallFailure::new(
        MigrationInstallState::NotPublished,
        db,
        backup,
        &staged,
        format!(
            "{cause}; backup must be inspected (owned copy could not be safely removed); preserve stage"
        ),
    ))
}

#[cfg(all(test, feature = "cozo"))]
std::thread_local! {
    static MIGRATION_INSTALL_FAILURE: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

#[cfg(all(test, feature = "cozo"))]
fn migration_install_checkpoint(at: &'static str) -> std::io::Result<()> {
    if std::env::var("MNEME_TEST_MIGRATION_KILL_AT").as_deref() == Ok(at) {
        // Only the explicitly spawned unit-test subprocess sets this variable.
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        unreachable!("SIGKILL did not terminate migration test child");
    }
    if MIGRATION_INSTALL_FAILURE.with(|slot| {
        if slot.get() == Some(at) {
            slot.set(None);
            true
        } else {
            false
        }
    }) {
        Err(std::io::Error::other(format!(
            "injected migration install failure at {at}"
        )))
    } else {
        Ok(())
    }
}

#[cfg(feature = "cozo")]
fn sync_parent_required(path: &Path) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(feature = "cozo")]
struct MigrationTemp {
    path: Option<PathBuf>,
    private_dir: Option<PathBuf>,
}

#[cfg(feature = "cozo")]
impl MigrationTemp {
    #[cfg(test)]
    fn reserve_beside(db: &Path) -> Result<Self, AnyErr> {
        let parent = db
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = parent.canonicalize()?;
        let name = db
            .file_name()
            .ok_or("--db path has no file name")?
            .to_string_lossy();
        for _ in 0..16 {
            let path = parent.join(format!(".{name}.migrate-{}.sqlite", Ulid::new()));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    file.sync_all()?;
                    return Ok(Self {
                        path: Some(path),
                        private_dir: None,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err("could not reserve a unique migration temp path".into())
    }

    fn reserve_private_dir_beside(db: &Path, purpose: &str) -> Result<Self, AnyErr> {
        let parent = db
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = parent.canonicalize()?;
        let name = db
            .file_name()
            .ok_or("--db path has no file name")?
            .to_string_lossy();
        for _ in 0..16 {
            let dir = parent.join(format!(".{name}.{purpose}-{}", Ulid::new()));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&dir) {
                Ok(()) => {
                    return Ok(Self {
                        path: Some(dir.join("database.db")),
                        private_dir: Some(dir),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err("could not reserve a private rebuild stage".into())
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("migration temp is armed")
    }

    fn disarm(&mut self) -> PathBuf {
        let path = self.path.take().expect("migration temp is armed");
        if !path.exists() {
            if let Some(dir) = self.private_dir.take() {
                let _ = std::fs::remove_dir_all(dir);
            }
        } else {
            // An ambiguous install keeps its private stage and recovery path.
            self.private_dir.take();
        }
        path
    }
}

#[cfg(feature = "cozo")]
impl Drop for MigrationTemp {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(&path);
            for suffix in ["-wal", "-shm", "-journal"] {
                let _ = std::fs::remove_file(path_with_suffix(&path, suffix));
            }
        }
        if let Some(dir) = self.private_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[cfg(feature = "cozo")]
fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(feature = "cozo")]
fn reject_sqlite_sidecars(path: &Path) -> Result<(), AnyErr> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = path_with_suffix(path, suffix);
        if sidecar.exists() {
            return Err(format!(
                "sqlite left sidecar {} after close; refusing a potentially lossy migration",
                sidecar.display()
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(not(feature = "cozo"))]
async fn run_migrate(_db: &Path, _lease: Arc<mneme_store_path::StoreLease>) -> Result<(), AnyErr> {
    Err("migrate requires a cozo-enabled build (cargo install --features cozo)".into())
}

const REEMBED_BATCH_SIZE: usize = 64;

async fn embed_rebuild_batch(
    embedder: &dyn Embedder,
    nodes: &[Node],
) -> Result<Vec<(NodeId, Vec<f32>)>, AnyErr> {
    let summaries: Vec<&str> = nodes.iter().map(|node| node.summary()).collect();
    let embeddings = embedder.embed(&summaries).await?;
    if embeddings.len() != nodes.len() {
        return Err(format!(
            "embedder returned {} vectors for {} nodes in a rebuild batch",
            embeddings.len(),
            nodes.len()
        )
        .into());
    }
    let target = embedder.dim();
    nodes
        .iter()
        .zip(embeddings)
        .map(|(node, vector)| {
            if vector.len() != target {
                return Err(format!(
                    "embedder returned dimension {} for node {}; expected {target}",
                    vector.len(),
                    node.id().0
                )
                .into());
            }
            if !vector.iter().all(|value| value.is_finite()) {
                return Err(format!(
                    "embedder returned a non-finite vector for node {}",
                    node.id().0
                )
                .into());
            }
            Ok((node.id(), vector))
        })
        .collect()
}

/// Reembed a JSON-snapshot (reference) store in place, recreating its vector map at
/// the embedder's dim if it changed. Inference calls are bounded, but the snapshot
/// format requires one complete detached O(N) export/replacement in memory.
async fn reembed_snapshot(db: &Path) -> Result<(), AnyErr> {
    if !db.exists() {
        return Err("--db does not exist — nothing to reembed".into());
    }
    let store = MemStore::load(db)?;
    let embedder = make_embedder(store.dim())?;
    let (n, fingerprint) =
        reembed_snapshot_with(db, &store, embedder.as_ref(), REEMBED_BATCH_SIZE).await?;
    println!("re-embedded {n} node(s) with {fingerprint}");
    Ok(())
}

async fn reembed_snapshot_with(
    db: &Path,
    source: &MemStore,
    embedder: &dyn Embedder,
    batch_size: usize,
) -> Result<(usize, EmbeddingFingerprint), AnyErr> {
    let (replacement, fingerprint) = rebuild_all_node_vectors(source, embedder, batch_size).await?;
    replacement.save(db)?;
    Ok((replacement.export().nodes.len(), fingerprint))
}

async fn rebuild_all_node_vectors(
    source: &MemStore,
    embedder: &dyn Embedder,
    batch_size: usize,
) -> Result<(MemStore, EmbeddingFingerprint), AnyErr> {
    let fingerprint = runtime_fingerprint(embedder)?;
    let mut replacement = source.export();
    replacement.nodes.sort_by_key(|node| node.id());
    let mut vectors = Vec::with_capacity(replacement.nodes.len());
    for nodes in replacement.nodes.chunks(batch_size.clamp(1, 1_024)) {
        vectors.extend(embed_rebuild_batch(embedder, nodes).await?);
    }
    if vectors.len() != replacement.nodes.len() {
        return Err("reembed did not produce exactly one vector per node".into());
    }
    replacement.dim = embedder.dim();
    replacement.embedding_fingerprint = Some(fingerprint.clone());
    replacement.vectors = vectors;

    // All physical nodes, including Archived semantic nodes and every episode
    // edition, participate. Only vector state and its identity change.
    let replacement = MemStore::from_export(replacement)?;
    Ok((replacement, fingerprint))
}

#[cfg(feature = "cozo")]
async fn reembed_cozo_with(
    db: &Path,
    lease: &Arc<mneme_store_path::StoreLease>,
    embedder: &dyn Embedder,
    batch_size: usize,
) -> Result<(usize, EmbeddingFingerprint), AnyErr> {
    let backup = db.with_extension("reembed.bak");
    match std::fs::symlink_metadata(&backup) {
        Ok(_) => {
            return Err(format!(
                "refusing to overwrite existing migration backup {}",
                backup.display()
            )
            .into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let store = CozoStore::open_existing_persistent(db, DEFAULT_DIM, lease.clone())?;
    let source_export = store.export().await?;
    store.prepare_for_file_move()?;
    drop(store);
    let source = MemStore::from_export(source_export.clone())?;
    let seal = CurrentRebuildSeal::capture(db, &source_export, lease)?;
    let (replacement, fingerprint) =
        rebuild_all_node_vectors(&source, embedder, batch_size).await?;
    let expected = replacement.export();
    publish_detached_current(
        db,
        &backup,
        &replacement,
        &expected,
        lease,
        &seal,
        "reembed",
    )
    .await?;
    Ok((expected.nodes.len(), fingerprint))
}

#[cfg(feature = "cozo")]
async fn run_reembed(db: &Path, lease: Arc<mneme_store_path::StoreLease>) -> Result<(), AnyErr> {
    // Current JSON snapshots reembed through the reference store. Persistent
    // stores use a detached verified TouchstonesV1 replacement, not the retired
    // in-place shadow-index publisher.
    if db.exists() && is_snapshot(db)? {
        return reembed_snapshot(db).await;
    }
    let store = CozoStore::open_existing_persistent(db, DEFAULT_DIM, lease.clone())?;
    let embedder = make_embedder(store.dim())?;
    drop(store);
    let (n, fingerprint) =
        reembed_cozo_with(db, &lease, embedder.as_ref(), REEMBED_BATCH_SIZE).await?;
    println!("re-embedded {n} node(s) with {fingerprint}");
    Ok(())
}

#[cfg(not(feature = "cozo"))]
async fn run_reembed(db: &Path, _lease: Arc<mneme_store_path::StoreLease>) -> Result<(), AnyErr> {
    reembed_snapshot(db).await
}

/// Run a command. Returns whether it mutated the graph (so we know to persist).
async fn dispatch(
    mem: &Memory,
    json: bool,
    user: bool,
    source_db: &Path,
    db_id: Ulid,
    cmd: Command,
) -> Result<bool, AnyErr> {
    match cmd {
        Command::Library(_) | Command::Tui(_) | Command::Stores(_) | Command::Snapshot { .. } => {
            unreachable!("handled before local store admission")
        }
        Command::Init(_) | Command::Client(_) => {
            unreachable!("init/client handled before remote/store admission")
        }
        Command::BootstrapInspect(_) => unreachable!("bootstrap-inspect handled in main"),
        Command::BootstrapCreate(_) => unreachable!("bootstrap-create handled in main"),
        Command::Ingest(_) => unreachable!("ingest preflight and checkpoint handled in main"),
        Command::Capture { .. }
        | Command::Save(_)
        | Command::Concern(_)
        | Command::Retag(_)
        | Command::EditBody(_)
        | Command::EditSummary(_) => {
            unreachable!("save/capture preflight and checkpoint handled in main")
        }
        Command::Episode { .. } | Command::SingleGraphUpgrade(_) => {
            unreachable!("episode admission and checkpoint handled in main")
        }
        Command::Get(a) => cmd_get(mem, db_id, json, a).await,
        Command::Body(a) => cmd_body(mem, json, a).await,
        Command::Neighbors(a) => cmd_neighbors(mem, db_id, json, a).await,
        Command::Remote(a) => cmd_remote(mem, json, a).await,
        Command::Query(a) => cmd_query(mem, json, a).await,
        Command::RecallContext(a) => cmd_recall_context(mem, a).await,
        Command::List(a) => cmd_list(mem, db_id, json, a).await,
        Command::Link(a) => cmd_link(mem, json, user, source_db, a).await,
        Command::Feedback(a) => cmd_feedback(mem, json, a).await,
        Command::Merges => cmd_merges(mem, json).await,
        Command::Merge { action } => cmd_merge(mem, json, action).await,
        Command::Repl(a) => repl::run(mem, json, &a.start, a.budget, a.query.as_deref()).await,
        Command::Supersede { winner, loser } => {
            mem.supersede(ColdPath::acquire(), parse_id(&winner)?, parse_id(&loser)?)
                .await?;
            emit(json, "ok", || println!("superseded {loser} with {winner}"));
            Ok(true)
        }
        Command::Contradict { a, b } => {
            mem.observe_contradiction(ColdPath::acquire(), parse_id(&a)?, parse_id(&b)?)
                .await?;
            emit(json, "ok", || {
                println!("recorded contradiction {a} <-> {b}")
            });
            Ok(true)
        }
        Command::Reconcile { a, b, resolution } => cmd_reconcile(mem, json, a, b, resolution).await,
        Command::Decay => {
            let r = mem.decay_sweep(ColdPath::acquire()).await?;
            if json {
                print_json(&json!({
                    "edges_decayed": r.edges_decayed,
                    "edge_conflicts": r.edge_conflicts,
                    "edge_pages": r.edge_pages,
                }));
            } else {
                println!(
                    "decayed {} edge(s); conflicts={}; pages={}",
                    r.edges_decayed, r.edge_conflicts, r.edge_pages
                );
            }
            Ok(true)
        }
        Command::Prune => {
            let r = mem.prune_dense(ColdPath::acquire()).await?;
            if json {
                print_json(&json!({
                    "pruned": r.pruned, "capacity_pruned": r.capacity_pruned,
                    "weak_conflicts": r.weak_conflicts,
                    "contended_hubs": r.contended_hubs,
                    "edge_pages": r.edge_pages, "hub_pages": r.hub_pages,
                    "chunks": r.chunks,
                }));
            } else {
                println!(
                    "pruned {} edge(s), {} for capacity; weak conflicts={}; contended hubs={}; pages edge={} hub={}, chunks={}",
                    r.pruned,
                    r.capacity_pruned,
                    r.weak_conflicts,
                    r.contended_hubs,
                    r.edge_pages,
                    r.hub_pages,
                    r.chunks,
                );
            }
            Ok(true)
        }
        Command::Status => cmd_status(mem, json).await,
        Command::Core => cmd_core(mem, json).await,
        Command::Forget { id } => {
            let existed = mem.forget(ColdPath::acquire(), parse_id(&id)?).await?;
            if existed {
                emit(json, "ok", || println!("forgot {id}"));
            } else {
                emit(json, "not-found", || println!("no such node {id}"));
            }
            Ok(existed)
        }
        Command::Demo { .. } => unreachable!("demo handled in main"),
        Command::Migrate => unreachable!("migrate handled in main"),
        Command::Reembed => unreachable!("reembed handled in main"),
    }
}

/// Top edges shown inline on a `get` (the full list is `mnemed neighbors <id>`).
const GET_EDGES: usize = 8;
const GET_REMOTE_EDGES: usize = 8;

async fn cmd_get(mem: &Memory, db_id: Ulid, json: bool, a: GetArgs) -> Result<bool, AnyErr> {
    let id = parse_id(&a.id)?;
    let node = mem.get_node(id).await?.ok_or("node not found")?;
    let body = if a.body {
        Some(
            mem.resolve_body_range(
                &node,
                a.body_offset.unwrap_or(0),
                cli_body_limit(a.max_body_bytes)?,
            )
            .await?,
        )
    } else {
        None
    };
    // Fetch one sentinel beyond the inline prefix. This reports truncation
    // honestly without pretending a capped read produced a total edge count.
    let edges = if a.edges {
        Some(inline_edges(mem, id, GET_EDGES).await?)
    } else {
        None
    };
    // Cross-db edges, listed as raw records: this one-shot CLI holds a single db,
    // so it can't resolve the target summary (the MCP server, which holds a
    // registry, does). Empty for most nodes.
    let remote = if a.edges {
        Some(mem.remote_edges_page(id, None, GET_REMOTE_EDGES).await?)
    } else {
        None
    };

    let projection = mneme_app::touchstone::node_touchstone_projection(mem, db_id, &node)
        .await
        .map_err(|error| -> AnyErr { error })?;
    if json {
        let mut v = node_json(&node);
        let obj = v.as_object_mut().unwrap();
        obj.extend(
            projection
                .as_object()
                .expect("touchstone projection object")
                .clone(),
        );
        obj.insert(
            "concern_endpoint".into(),
            serde_json::to_value(mneme_app::concern::endpoint_for_node(&node))?,
        );
        if let Some(chunk) = &body {
            obj.insert(
                "body".into(),
                json!({
                    "content": String::from_utf8_lossy(&chunk.bytes),
                    "source_start": chunk.source_start,
                    "source_end": chunk.source_end,
                    "next_offset": chunk.next_offset,
                    "has_more": chunk.next_offset.is_some(),
                }),
            );
        }
        if let Some((es, has_more)) = edges {
            let returned = es.len();
            obj.insert("edges".into(), edges_json(mem, Some(&node), es).await?);
            obj.insert("edges_returned".into(), json!(returned));
            obj.insert("edges_has_more".into(), json!(has_more));
        }
        if let Some(page) = &remote
            && (!page.items.is_empty() || page.next.is_some())
        {
            obj.insert("remote".into(), remote_page_json(page));
        }
        print_json(&v);
    } else {
        print_node_text(&node);
        println!(
            "\nsummary snapshot (summary_only): {}",
            projection["summary_snapshot"]
        );
        if let Some(record) = projection.get("touchstone") {
            println!(
                "\ntouchstone (immutable):\n{}",
                serde_json::to_string_pretty(record)?
            );
            println!(
                "\ncurrent resolution (not body equality):\n{}",
                serde_json::to_string_pretty(&projection["touchstone_current"])?
            );
        }
        if let Some(chunk) = &body {
            println!("\nbody:\n{}", String::from_utf8_lossy(&chunk.bytes));
            if let Some(next) = chunk.next_offset {
                println!("  … body continues at source byte {next}");
            }
        }
        if let Some((es, has_more)) = edges {
            println!("\nedges (returned {}):", es.len());
            print_edges_text(mem, Some(&node), es).await?;
            if has_more {
                println!("  … more; use `neighbors {}` (up to 64)", a.id);
            }
        }
        if let Some(page) = &remote
            && !page.items.is_empty()
        {
            println!("\nremote (returned {}):", page.items.len());
            for r in &page.items {
                println!("  -> {}@{}  w={:.2}", r.target.0, r.target_db, r.weight());
            }
            if let Some(next) = &page.next {
                println!(
                    "  … more; use `remote {} --after '{}'`",
                    a.id,
                    serde_json::to_string(next)?
                );
            }
        }
    }
    Ok(false)
}

/// Cross-db edge records as JSON (unresolved — the target lives in another db).
fn remote_json(edges: &[mneme_core::RemoteEdge]) -> Value {
    json!(
        edges
            .iter()
            .map(|r| json!({
                "target_db": r.target_db.to_string(),
                "target": r.target.0.to_string(),
                "weight": r.weight(),
            }))
            .collect::<Vec<_>>()
    )
}

fn remote_page_json(page: &RemoteEdgePage) -> Value {
    json!({
        "items": remote_json(&page.items),
        "next": page.next,
        "has_more": page.next.is_some(),
        "returned": page.items.len(),
    })
}

async fn cmd_body(mem: &Memory, json: bool, a: BodyArgs) -> Result<bool, AnyErr> {
    validate_cli_body_mode(json, &a)?;
    let id = parse_id(&a.id)?;
    let node = mem.get_node(id).await?.ok_or("node not found")?;
    if a.raw {
        let body = mem.resolve_body(&node).await?;
        std::io::stdout().lock().write_all(&body)?;
        return Ok(false);
    }

    let max_bytes = cli_body_limit(a.max_bytes)?;
    let chunk = mem
        .resolve_body_range(&node, a.offset.unwrap_or(0), max_bytes)
        .await?;
    if json {
        print_json(&json!({
            "body": String::from_utf8_lossy(&chunk.bytes),
            "source_start": chunk.source_start,
            "source_end": chunk.source_end,
            "next_offset": chunk.next_offset,
            "has_more": chunk.next_offset.is_some(),
        }));
    } else {
        std::io::stdout().lock().write_all(&chunk.bytes)?;
        if let Some(next) = chunk.next_offset {
            eprintln!(
                "\n[mnemed: body truncated at source byte {}; continue with --offset {next}]",
                chunk.source_end
            );
        }
    }
    Ok(false)
}

fn validate_cli_body_mode(json: bool, args: &BodyArgs) -> Result<(), AnyErr> {
    if json && args.raw {
        return Err(
            "--raw cannot be combined with --json; raw is an explicit stdout passthrough".into(),
        );
    }
    Ok(())
}

fn cli_body_limit(requested: Option<usize>) -> Result<usize, AnyErr> {
    let max_bytes = requested.unwrap_or(DEFAULT_CLI_BODY_BYTES);
    if !(1..=MAX_CLI_BODY_BYTES).contains(&max_bytes) {
        return Err(format!("body byte limit must be between 1 and {MAX_CLI_BODY_BYTES}").into());
    }
    Ok(max_bytes)
}

async fn inline_edges(
    mem: &Memory,
    id: NodeId,
    limit: usize,
) -> Result<(Vec<HydratedNeighbor>, bool), AnyErr> {
    let read_limit = limit.checked_add(1).ok_or("inline edge limit overflow")?;
    let mut edges = mem.neighbors_hydrated(id, read_limit).await?;
    let has_more = edges.len() > limit;
    edges.truncate(limit);
    Ok((edges, has_more))
}

async fn cmd_neighbors(
    mem: &Memory,
    db_id: Ulid,
    json: bool,
    args: NeighborsArgs,
) -> Result<bool, AnyErr> {
    let page = args
        .prepare()?
        .run(mem, db_id)
        .await
        .map_err(|error| -> AnyErr { error })?;
    remote_commands::render(&Command::Neighbors(args), &page, json)?;
    Ok(false)
}

async fn cmd_remote(mem: &Memory, json: bool, a: RemoteArgs) -> Result<bool, AnyErr> {
    let limit = cli_remote_limit(a.limit)?;
    let id = parse_id(&a.id)?;
    let after = a
        .after
        .as_deref()
        .map(serde_json::from_str::<RemoteEdgeCursor>)
        .transpose()
        .map_err(|error| format!("invalid --after remote-edge cursor: {error}"))?;
    let page = mem.remote_edges_page(id, after, limit).await?;
    if json {
        print_json(&remote_page_json(&page));
    } else if page.items.is_empty() {
        println!("(no remote edges)");
    } else {
        for edge in &page.items {
            println!(
                "{} -> {}@{}  w={:.2}",
                edge.from.0,
                edge.target.0,
                edge.target_db,
                edge.weight()
            );
        }
        if let Some(next) = &page.next {
            println!(
                "more: mnemed remote {} --limit {} --after '{}'",
                a.id,
                a.limit,
                serde_json::to_string(next)?
            );
        }
    }
    Ok(false)
}

fn cli_remote_limit(limit: usize) -> Result<usize, AnyErr> {
    if !(1..=MAX_REMOTE_EDGE_PAGE_SIZE).contains(&limit) {
        return Err(format!("--limit must be between 1 and {MAX_REMOTE_EDGE_PAGE_SIZE}").into());
    }
    Ok(limit)
}

async fn cmd_query(mem: &Memory, json: bool, a: QueryArgs) -> Result<bool, AnyErr> {
    let (batch, body_limit) = retrieve_cli_query(mem, &a).await?;

    if json {
        let envelope = cli_query_envelope(mem, batch, a.bodies, body_limit).await?;
        print_json(&serde_json::to_value(envelope)?);
        // Retrieval only plans context. It records no exposure or topology
        // change, so snapshot backends must not checkpoint an O(store) no-op.
        return Ok(false);
    }

    let retrieval = presentation_retrieval_metadata(&batch)?;
    if batch.primary.is_empty() {
        print_partial_seed_warnings(&retrieval);
        println!("(no results above relevance threshold)");
    } else {
        print_partial_seed_warnings(&retrieval);
        let mut body_remaining = MAX_CLI_QUERY_BODY_BYTES_TOTAL;
        let mut ordinal = 1;
        for hit in &batch.primary {
            print_human_query_hit(
                mem,
                RetrievalLane::Primary,
                ordinal,
                hit,
                a.bodies,
                body_limit,
                &mut body_remaining,
            )
            .await?;
            ordinal += 1;
        }
    }
    // Retrieval only plans context. It records no exposure or topology change,
    // so snapshot backends must not checkpoint an O(store) no-op.
    Ok(false)
}

async fn retrieve_cli_query(
    mem: &Memory,
    a: &QueryArgs,
) -> Result<(RetrievalBatch, usize), AnyErr> {
    let base = mem.config().budget;
    let (budget, k, body_limit) = validate_cli_query(a, base, mem.config().ann_k)?;
    let status = StatusFilter {
        active: true,
        archived: a.archived,
    };
    let tags: Vec<&str> = a.tags.iter().map(String::as_str).collect();
    let batch = mem
        .retrieve_batch_seeded(&a.text, k, budget, status, &tags)
        .await?;
    Ok((batch, body_limit))
}

async fn cli_query_envelope(
    mem: &Memory,
    batch: RetrievalBatch,
    bodies: bool,
    body_limit: usize,
) -> Result<QueryEnvelope, AnyErr> {
    let retrieval = presentation_retrieval_metadata(&batch)?;
    let mut body_remaining = MAX_CLI_QUERY_BODY_BYTES_TOTAL;
    let mut primary = Vec::with_capacity(batch.primary.len());
    for hit in batch.primary {
        primary.push(cli_query_hit(mem, hit, bodies, body_limit, &mut body_remaining).await?);
    }
    Ok(QueryEnvelope::new(retrieval, primary)?)
}

fn query_rank_evidence(evidence: RetrievalEvidence) -> RankEvidence {
    RankEvidence::new(
        evidence.dense_rank,
        evidence.sparse_rank,
        evidence.graph_rank,
        evidence.rerank_rank,
    )
}

fn query_node_status(status: NodeStatus) -> QueryNodeStatus {
    match status {
        NodeStatus::Active => QueryNodeStatus::Active,
        NodeStatus::Archived => QueryNodeStatus::Archived,
    }
}

async fn cli_query_hit(
    mem: &Memory,
    hit: RetrievalHit,
    bodies: bool,
    body_limit: usize,
    body_remaining: &mut usize,
) -> Result<QueryHit, AnyErr> {
    let body = if !bodies {
        QueryBody::NotRequested
    } else if *body_remaining == 0 {
        QueryBody::OmittedBudget
    } else {
        let body = mem
            .resolve_body_range(&hit.node, 0, body_limit.min(*body_remaining))
            .await?;
        *body_remaining = body_remaining.saturating_sub(body.bytes.len());
        QueryBody::included(
            String::from_utf8_lossy(&body.bytes),
            body.source_start,
            body.source_end,
            body.next_offset,
        )?
    };
    Ok(QueryHit::new(
        hit.node.id(),
        hit.lane_rank,
        query_rank_evidence(hit.evidence),
        query_node_status(hit.node.status()),
        hit.node.summary(),
        false,
        body,
    ))
}

fn partial_seed_warning(
    lane: RetrievalLane,
    coverage: &mneme_core::tagged::TaggedSeedCoverage,
) -> Option<String> {
    coverage.is_partial().then(|| {
        format!(
            "warning: {} seed_coverage is partial ({}) and describes tagged seed selection, not final graph-hit coverage",
            match lane {
                RetrievalLane::Primary => "primary",
            },
            coverage.strategy_id()
        )
    })
}

fn print_partial_seed_warnings(retrieval: &RetrievalMetadata) {
    for lane in [RetrievalLane::Primary] {
        if let Some(warning) = retrieval
            .seed_coverage(lane)
            .and_then(|coverage| partial_seed_warning(lane, coverage))
        {
            eprintln!("{warning}");
        }
    }
}

fn human_rank_evidence(evidence: RetrievalEvidence) -> String {
    let mut ranks = Vec::with_capacity(4);
    for (name, rank) in [
        ("dense", evidence.dense_rank),
        ("sparse", evidence.sparse_rank),
        ("graph", evidence.graph_rank),
        ("rerank", evidence.rerank_rank),
    ] {
        if let Some(rank) = rank {
            ranks.push(format!("{name}#{rank}"));
        }
    }
    ranks.join(",")
}

#[allow(clippy::too_many_arguments)]
async fn print_human_query_hit(
    mem: &Memory,
    lane: RetrievalLane,
    ordinal: usize,
    hit: &RetrievalHit,
    bodies: bool,
    body_limit: usize,
    body_remaining: &mut usize,
) -> Result<(), AnyErr> {
    let lane = match lane {
        RetrievalLane::Primary => "primary",
    };
    println!(
        "{:>2}. [{lane}#{}; {}] {} ({}) {}",
        ordinal,
        hit.lane_rank,
        human_rank_evidence(hit.evidence),
        hit.node.id().0,
        status_str(hit.node.status()),
        hit.node.summary()
    );
    if bodies {
        if *body_remaining == 0 {
            println!("    [body omitted: aggregate query body budget exhausted]");
        } else {
            let body = mem
                .resolve_body_range(&hit.node, 0, body_limit.min(*body_remaining))
                .await?;
            *body_remaining = body_remaining.saturating_sub(body.bytes.len());
            println!("    {}", String::from_utf8_lossy(&body.bytes));
            if let Some(next) = body.next_offset {
                println!(
                    "    [body truncated at source byte {}; continue with `mnemed body {} --offset {next}`]",
                    body.source_end,
                    hit.node.id().0
                );
            }
        }
    }
    Ok(())
}

async fn cmd_recall_context(mem: &Memory, a: RecallContextArgs) -> Result<bool, AnyErr> {
    let plan = recall_context_plan(mem, &a).await?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(plan.rendered_content().as_bytes())?;
    stdout.flush()?;
    // Retrieval and packing are pure. In particular, this transitional surface
    // emits no feedback receipt and must not checkpoint a snapshot backend.
    Ok(false)
}

async fn recall_context_plan(mem: &Memory, a: &RecallContextArgs) -> Result<PackPlan, AnyErr> {
    let (retrieval_budget, k, presentation_budget) =
        validate_cli_recall_context(a, mem.config().budget, mem.config().ann_k)?;
    let tags: Vec<&str> = a.tags.iter().map(String::as_str).collect();
    Ok(recall_context(
        mem,
        &a.text,
        k,
        retrieval_budget,
        &tags,
        &presentation_budget,
    )
    .await?)
}

fn validate_cli_recall_context(
    args: &RecallContextArgs,
    base: Budget,
    default_k: usize,
) -> Result<(Budget, usize, PresentationBudget), AnyErr> {
    let (budget, k) = validate_cli_query_work(
        &args.text,
        args.k,
        args.depth,
        args.max_nodes,
        args.min_relevance,
        &args.tags,
        base,
        default_k,
    )?;
    let presentation = cli_presentation_budget(args.max_content_bytes, budget.max_nodes)?;
    Ok((budget, k, presentation))
}

fn cli_presentation_budget(
    max_content_bytes: Option<u32>,
    max_nodes: usize,
) -> Result<PresentationBudget, AnyErr> {
    let primary_items = max_nodes.clamp(1, MAX_CLI_QUERY_NODES) as u16;
    let max_content_bytes = max_content_bytes.unwrap_or(DEFAULT_CLI_CONTEXT_BYTES);
    if !(MIN_CLI_CONTEXT_BYTES..=MAX_CLI_CONTEXT_BYTES).contains(&max_content_bytes) {
        return Err(format!(
            "--max-content-bytes must be between {MIN_CLI_CONTEXT_BYTES} and {MAX_CLI_CONTEXT_BYTES}"
        )
        .into());
    }
    let control_reserve = PresentationBudget::minimum_control_reserve_bytes();
    let lane_bytes = max_content_bytes
        .checked_sub(control_reserve)
        .expect("the validated CLI minimum exceeds the control reserve");
    let disabled = LaneLimit::new(0, 0, 0, 0)?;
    let lanes = LaneBudgets::new(
        disabled,
        LaneLimit::new(1, primary_items, primary_items, lane_bytes)?,
        disabled,
    )
    .with_episodic(LaneLimit::new(
        0,
        2.min(primary_items),
        primary_items,
        lane_bytes,
    )?);
    Ok(PresentationBudget::new(
        NonZeroU32::new(max_content_bytes).expect("validated nonzero content budget"),
        NonZeroU32::new(control_reserve).expect("control envelope is nonempty"),
        NonZeroU16::new(primary_items * 2).expect("request item budget is nonzero"),
        NonZeroU16::new(MAX_CLI_CONTEXT_SUMMARY_BYTES)
            .expect("fixed per-summary budget is nonzero"),
        BodyBudget::disabled(),
        lanes,
    )?)
}

fn validate_cli_query(
    args: &QueryArgs,
    base: Budget,
    default_k: usize,
) -> Result<(Budget, usize, usize), AnyErr> {
    let (budget, k) = validate_cli_query_work(
        &args.text,
        args.k,
        args.depth,
        args.max_nodes,
        args.min_relevance,
        &args.tags,
        base,
        default_k,
    )?;
    let body_limit = if args.bodies {
        cli_body_limit(args.max_body_bytes)?
    } else {
        0
    };
    Ok((budget, k, body_limit))
}

#[allow(clippy::too_many_arguments)]
fn validate_cli_query_work(
    text: &str,
    k: Option<usize>,
    depth: Option<u8>,
    max_nodes: Option<usize>,
    min_relevance: Option<f32>,
    tags: &[String],
    base: Budget,
    default_k: usize,
) -> Result<(Budget, usize), AnyErr> {
    if text.len() > MAX_CLI_QUERY_BYTES {
        return Err(format!(
            "query text is {} UTF-8 bytes; maximum is {MAX_CLI_QUERY_BYTES}",
            text.len()
        )
        .into());
    }
    let k = k.unwrap_or(default_k);
    if !(1..=MAX_CLI_QUERY_K).contains(&k) {
        return Err(format!("--k must be between 1 and {MAX_CLI_QUERY_K}").into());
    }
    let max_nodes = max_nodes.unwrap_or(base.max_nodes);
    if !(1..=MAX_CLI_QUERY_NODES).contains(&max_nodes) {
        return Err(format!("--max-nodes must be between 1 and {MAX_CLI_QUERY_NODES}").into());
    }
    let max_depth = depth.unwrap_or(base.max_depth);
    if max_depth > MAX_CLI_QUERY_DEPTH {
        return Err(format!("--depth must be at most {MAX_CLI_QUERY_DEPTH}").into());
    }
    let min_relevance = min_relevance.unwrap_or(base.min_relevance);
    if !min_relevance.is_finite() || !(0.0..=1.0).contains(&min_relevance) {
        return Err("--min-relevance must be finite and between 0 and 1".into());
    }
    if tags.len() > MAX_CLI_QUERY_TAGS {
        return Err(format!(
            "--tag has {} entries; maximum is {MAX_CLI_QUERY_TAGS}",
            tags.len()
        )
        .into());
    }
    let mut unique = HashSet::with_capacity(tags.len());
    for (index, tag) in tags.iter().enumerate() {
        if tag.is_empty() || tag.trim() != tag || tag.chars().any(char::is_control) {
            return Err(format!(
                "--tag value {index} must be nonblank, trimmed, and contain no controls"
            )
            .into());
        }
        if tag.len() > MAX_CLI_QUERY_TAG_BYTES {
            return Err(format!(
                "--tag value {index} is {} UTF-8 bytes; maximum is {MAX_CLI_QUERY_TAG_BYTES}",
                tag.len()
            )
            .into());
        }
        if !unique.insert(tag) {
            return Err(format!("duplicate --tag value {tag:?}").into());
        }
    }
    Ok((
        Budget {
            max_nodes,
            max_depth,
            min_relevance,
            ..base
        },
        k,
    ))
}

#[cfg(test)]
mod query_purity_tests {
    use super::*;

    #[tokio::test]
    async fn query_reports_no_mutation_to_the_snapshot_dispatcher() {
        let store = Arc::new(MemStore::new(DEFAULT_DIM));
        let mem = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
            Arc::new(SystemClock),
            Config {
                graph_seed_cap: 0,
                ..Config::default()
            },
        )
        .with_body_store(Arc::new(InlineStore::new()));
        let id = mem
            .ingest(Ingest::new(
                "pure query",
                b"",
                &[],
                Provenance::derived_empty(),
            ))
            .await
            .unwrap();

        let batch = mem
            .retrieve_batch_seeded(
                "pure query",
                1,
                Budget {
                    max_nodes: 1,
                    max_depth: 0,
                    min_relevance: 0.0,
                    ..Budget::default()
                },
                StatusFilter::default(),
                &[],
            )
            .await
            .unwrap();
        let retrieval = presentation_retrieval_metadata(&batch).unwrap();
        let mut primary = Vec::new();
        let mut body_remaining = MAX_CLI_QUERY_BODY_BYTES_TOTAL;
        for hit in batch.primary {
            primary.push(
                cli_query_hit(&mem, hit, false, 1, &mut body_remaining)
                    .await
                    .unwrap(),
            );
        }
        let envelope = QueryEnvelope::new(retrieval, primary).unwrap();
        let value = serde_json::to_value(envelope).unwrap();
        assert_eq!(value["schema"], "mneme.query.v3");
        assert_eq!(value["mode"], "untagged");
        assert!(value["lanes"]["primary"]["hits"].is_array());
        assert!(value["lanes"]["primary"]["hits"][0].get("score").is_none());

        let mutated = cmd_query(
            &mem,
            true,
            QueryArgs {
                text: "pure query".into(),
                k: Some(1),
                depth: Some(0),
                max_nodes: Some(1),
                min_relevance: Some(0.0),
                archived: false,
                tags: Vec::new(),
                bodies: false,
                max_body_bytes: None,
            },
        )
        .await
        .unwrap();

        assert!(!mutated, "a pure query must not request a snapshot save");
        let node = mem.get_node(id).await.unwrap().unwrap();
        assert_eq!(node.exposure_count(), 0);
        assert_eq!(node.last_exposed(), None);
        assert!(mem.neighbors(id, 1).await.unwrap().is_empty());
    }
}

async fn cmd_list(mem: &Memory, db_id: Ulid, json: bool, args: ListArgs) -> Result<bool, AnyErr> {
    let page = args
        .prepare()?
        .run(mem, db_id)
        .await
        .map_err(|error| -> AnyErr { error })?;
    cli_list::render(&page, json)?;
    Ok(false)
}

async fn cmd_link(
    mem: &Memory,
    json: bool,
    user: bool,
    source_db: &Path,
    a: LinkArgs,
) -> Result<bool, AnyErr> {
    let (from, to) = (parse_id(&a.from)?, parse_id(&a.to)?);
    if let Some(to_db) = &a.to_db {
        if !user {
            return Err("cross-db edges must originate from the user db — pass --user".into());
        }
        // Resolve and independently lease the target before reading its stable
        // identity. A self-target would recursively acquire the source's
        // non-reentrant lease, so reject it explicitly instead of surfacing a
        // misleading contention error.
        let target_path = mneme_store_path::resolve_configured_store_path(to_db)?;
        let source_path = std::fs::canonicalize(source_db)?;
        if target_path == source_path {
            return Err("cross-db link target resolves to the source database".into());
        }
        let target_metadata = std::fs::symlink_metadata(&target_path).map_err(|error| {
            format!(
                "cross-db link target {} must already exist as a resolved database: {error}",
                target_path.display()
            )
        })?;
        if !target_metadata.is_file() || target_metadata.file_type().is_symlink() {
            return Err(format!(
                "cross-db link target {} is not a real database file",
                target_path.display()
            )
            .into());
        }
        let target_lease = Arc::new(mneme_store_path::StoreLease::acquire(&target_path)?);
        let (target_mem, _saver, target_db) = open(&target_path, target_lease.clone(), None)?;
        if target_mem.get_node(to).await?.is_none() {
            return Err(format!(
                "target node {} not found in {}",
                a.to,
                target_path.display()
            )
            .into());
        }
        mem.link_remote(from, target_db, to, a.weight).await?;
        emit(json, "ok", || {
            println!("linked {} -> {}@{target_db} (cross-db)", a.from, a.to)
        });
        return Ok(true);
    }
    let anchor = a.anchor.as_deref().map(parse_span).transpose()?;
    mem.link(from, to, a.kind.into(), a.weight, anchor).await?;
    emit(json, "ok", || println!("linked {} -> {}", a.from, a.to));
    Ok(true)
}

/// Parse a `START:END` byte span.
fn parse_span(s: &str) -> Result<BodySpan, AnyErr> {
    let (a, b) = s.split_once(':').ok_or("anchor must be START:END")?;
    Ok(BodySpan::new(a.trim().parse()?, b.trim().parse()?))
}

async fn cmd_reconcile(
    mem: &Memory,
    json: bool,
    a: Option<String>,
    b: Option<String>,
    resolution: Option<String>,
) -> Result<bool, AnyErr> {
    let cold = ColdPath::acquire();
    if let (Some(a), Some(b), Some(res)) = (&a, &b, &resolution) {
        mem.reconcile(cold, parse_id(a)?, parse_id(b)?, parse_resolution(res)?)
            .await?;
        emit(json, "ok", || println!("reconciled {a} <-> {b} as {res}"));
        return Ok(true);
    }
    let t = mem.reconciliation_triage(cold).await?;
    if json {
        print_json(&json!({
            "contradictions": t.contradictions.iter().map(|c| json!({
                "a": c.between.0.0.to_string(), "b": c.between.1.0.to_string(),
                "observations": c.observations, "clusters": [c.clusters.0.0, c.clusters.1.0],
            })).collect::<Vec<_>>(),
            "cluster_conflicts": t.cluster_conflicts.iter().map(|cc| json!({
                "clusters": [cc.clusters.0.0, cc.clusters.1.0],
                "observations": cc.observations, "pairs": cc.pairs,
            })).collect::<Vec<_>>(),
        }));
    } else if t.contradictions.is_empty() {
        println!("(no open contradictions)");
    } else {
        println!("open contradictions (obs desc):");
        for c in &t.contradictions {
            println!(
                "  {} <-> {}  obs={}  clusters {}/{}",
                c.between.0.0, c.between.1.0, c.observations, c.clusters.0.0, c.clusters.1.0
            );
        }
        println!("community conflicts:");
        for cc in &t.cluster_conflicts {
            println!(
                "  {} <-> {}  obs={} pairs={}",
                cc.clusters.0.0, cc.clusters.1.0, cc.observations, cc.pairs
            );
        }
    }
    Ok(false)
}

fn parse_resolution(s: &str) -> Result<Resolution, AnyErr> {
    match s {
        "context-dependent" | "context" => Ok(Resolution::ContextDependent),
        "unresolved" => Ok(Resolution::Unresolved),
        "superseded" => Err("for a real supersession use `supersede`".into()),
        other => Err(format!("--as must be context-dependent|unresolved, got {other:?}").into()),
    }
}

async fn cmd_status(mem: &Memory, json: bool) -> Result<bool, AnyErr> {
    let s = mem.status(ColdPath::acquire()).await?;
    if json {
        print_json(&json!({
            "nodes": s.nodes, "active": s.active, "archived": s.archived,
            "episodes": s.episodes, "episode_editions": s.episode_editions,
            "open_contradictions": s.open_contradictions,
            "open_merge_candidates": s.open_merge_candidates,
            "edge_decay_pending": s.edge_decay_pending,
        }));
    } else {
        println!(
            "nodes {} (active {}, archived {})",
            s.nodes, s.active, s.archived
        );
        println!(
            "episodes {} ({} immutable editions)",
            s.episodes, s.episode_editions
        );
        println!("due:");
        println!("  contradictions to reconcile : {}", s.open_contradictions);
        println!(
            "  merge candidates            : {}",
            s.open_merge_candidates
        );
        println!("  edge decay pending          : {}", s.edge_decay_pending);
    }
    Ok(false)
}

async fn cmd_feedback(mem: &Memory, json: bool, a: FeedbackArgs) -> Result<bool, AnyErr> {
    let from = a.from.as_deref().map(parse_id).transpose()?;
    let to = parse_id(&a.to)?;
    mem.apply_feedback(from, to, a.signal.into()).await?;
    emit(json, "ok", || match &a.from {
        Some(from) => println!("feedback {}: {from} -> {}", a.signal.label(), a.to),
        None => println!("feedback {}: node {}", a.signal.label(), a.to),
    });
    Ok(true)
}

async fn cmd_core(mem: &Memory, json: bool) -> Result<bool, AnyErr> {
    let nodes = mem.core(ColdPath::acquire()).await?;
    if json {
        let mut arr = Vec::with_capacity(nodes.len());
        for n in &nodes {
            let body = mem
                .resolve_body(n)
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned());
            arr.push(json!({
                "id": n.id().0.to_string(),
                "summary": n.summary(),
                "tags": n.tags().collect::<Vec<_>>(),
                "body": body,
            }));
        }
        print_json(&json!(arr));
    } else if nodes.is_empty() {
        println!("(no core memory — tag a node `core` to bless it as always-loaded)");
    } else {
        for n in &nodes {
            println!("# {}", n.summary());
            if let Ok(body) = mem.resolve_body(n).await {
                println!("{}", String::from_utf8_lossy(&body));
            }
            println!();
        }
    }
    Ok(false)
}

async fn cmd_merge(mem: &Memory, json: bool, action: MergeAction) -> Result<bool, AnyErr> {
    let cold = ColdPath::acquire();
    match action {
        MergeAction::Full { winner, loser } => {
            let (w, l) = (parse_id(&winner)?, parse_id(&loser)?);
            mem.merge_full(cold, w, l).await?;
            emit(json, "ok", || {
                println!("merged {loser} into {winner} (full)")
            });
        }
        MergeAction::Keep { a, b } => {
            mem.resolve_merge(cold, parse_id(&a)?, parse_id(&b)?, MergeResolution::Keep)
                .await?;
            emit(json, "ok", || println!("kept {a} and {b} separate"));
        }
    }
    Ok(true)
}

async fn cmd_merges(mem: &Memory, json: bool) -> Result<bool, AnyErr> {
    let mut open = mem.open_merge_candidates(ColdPath::acquire()).await?;
    open.sort_by_key(|m| std::cmp::Reverse(m.observations));
    if json {
        let arr: Vec<Value> = open
            .iter()
            .map(|m| {
                json!({
                    "a": m.between.0.0.to_string(),
                    "b": m.between.1.0.to_string(),
                    "observations": m.observations,
                })
            })
            .collect();
        print_json(&json!(arr));
    } else if open.is_empty() {
        println!("(no open merge candidates)");
    } else {
        for m in &open {
            println!(
                "{}  <~>  {}   (x{})",
                m.between.0.0, m.between.1.0, m.observations
            );
        }
    }
    Ok(false)
}

// ---- rendering helpers ------------------------------------------------------

pub(crate) fn status_str(s: NodeStatus) -> &'static str {
    match s {
        NodeStatus::Active => "active",
        NodeStatus::Archived => "archived",
    }
}

/// Hand-built JSON so the wire format is stable and decoupled from the internal
/// field layout (and avoids u128 timestamp serialization quirks).
fn node_json(n: &Node) -> Value {
    let state = match n.status() {
        NodeStatus::Active => "active",
        NodeStatus::Archived => "archived",
    };
    json!({
        "id": n.id().0.to_string(),
        "content_fingerprint": mneme_core::ports::routing_content_fingerprint(n),
        "content_fingerprint_codec": mneme_core::ports::ROUTING_CONTENT_FINGERPRINT_CODEC,
        "summary": n.summary(),
        "status": state,
        "stability": n.stability(),
        "confidence": n.confidence(),
        "tags": n.tags().collect::<Vec<_>>(),
        "body_ref": n.body().as_str(),
        "body_revision": n.body_revision().to_string(),
        "body_ownership": n.body_ownership().as_str(),
        "created": n.created() as u64,
        "last_exposed": n.last_exposed().map(|timestamp| timestamp as u64),
        "exposure_count": n.exposure_count(),
        "last_grounded_use": n.last_grounded_use().map(|timestamp| timestamp as u64),
        "grounded_use_count": n.grounded_use_count(),
        "origin_commit": n.origin_commit(),
        "provenance": provenance_json(n.provenance()),
        "memory_kind": n.memory_kind(),
    })
}

fn provenance_json(p: &Provenance) -> Value {
    match p {
        Provenance::Web { url, fetched } => {
            json!({ "type": "web", "url": url.as_str(), "fetched": *fetched as u64 })
        }
        Provenance::Conversation { session, turn } => {
            json!({ "type": "conversation", "session": session.to_string(), "turn": turn })
        }
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
        Provenance::Derived { from } => json!({
            "type": "derived",
            "from": from.iter().map(|n| n.0.to_string()).collect::<Vec<_>>(),
        }),
    }
}

fn print_node_text(n: &Node) {
    println!("id          {}", n.id().0);
    println!("summary     {}", n.summary());
    if !n.is_semantic() {
        println!(
            "memory-kind {}",
            serde_json::to_string(n.memory_kind()).expect("memory kind serializes")
        );
    }
    match n.status() {
        NodeStatus::Active => println!("status      active"),
        NodeStatus::Archived => println!("status      archived"),
    }
    println!(
        "stability   {:.2}    confidence {:.2}",
        n.stability(),
        n.confidence()
    );
    println!("tags        {}", n.tags().collect::<Vec<_>>().join(", "));
    println!("body-ref    {}", n.body().as_str());
    println!("body-owner  {}", n.body_ownership().as_str());
    if let Provenance::External { source } = n.provenance() {
        println!("source      {}:{}", source.namespace(), source.key());
        println!("source-ref  {}", source.reference());
        if let Some(session) = source.session() {
            println!("source-session  {session}");
        }
        if let Some(revision) = source.revision() {
            println!("source-revision {revision}");
        }
    }
    println!(
        "origin-commit {}",
        n.origin_commit()
            .map_or_else(|| "(none)".to_owned(), |commit| commit.to_string())
    );
    let last_exposed = n
        .last_exposed()
        .map_or_else(|| "never".to_string(), |timestamp| timestamp.to_string());
    let last_grounded_use = n
        .last_grounded_use()
        .map_or_else(|| "never".to_string(), |timestamp| timestamp.to_string());
    println!("exposures   {} (last {last_exposed})", n.exposure_count());
    println!(
        "grounded    {} (last {last_grounded_use})",
        n.grounded_use_count()
    );
}

#[cfg(test)]
mod telemetry_render_tests {
    use super::*;

    fn node() -> Node {
        Node::try_new(
            NodeId(Ulid::new()),
            "telemetry",
            BodyRef::new("inline://telemetry").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap()
    }

    #[test]
    fn node_json_names_exposure_and_grounded_use_explicitly() {
        let mut node = node();
        let initial = node_json(&node);
        assert_eq!(
            initial["content_fingerprint"],
            mneme_core::ports::routing_content_fingerprint(&node)
        );
        assert_eq!(
            initial["content_fingerprint_codec"],
            mneme_core::ports::ROUTING_CONTENT_FINGERPRINT_CODEC
        );
        assert_eq!(initial["last_exposed"], Value::Null);
        assert_eq!(initial["exposure_count"], 0);
        assert_eq!(initial["last_grounded_use"], Value::Null);
        assert_eq!(initial["grounded_use_count"], 0);
        assert!(initial.get("candidate_use_count").is_none());
        assert!(initial.get("use_count").is_none());
        assert!(initial.get("last_activated").is_none());
        assert!(initial.get("activation_count").is_none());

        node.record_exposure(2);
        node.record_grounded_use(3);
        let current = node_json(&node);
        assert_eq!(
            current["content_fingerprint"],
            initial["content_fingerprint"]
        );
        assert_eq!(current["last_exposed"], 2);
        assert_eq!(current["exposure_count"], 1);
        assert_eq!(current["last_grounded_use"], 3);
        assert_eq!(current["grounded_use_count"], 1);
        assert!(current.get("candidate_use_count").is_none());

        node.set_status(NodeStatus::Active);
        let active = node_json(&node);
        assert!(active.get("candidate_use_count").is_none());
        assert_eq!(active["grounded_use_count"], 1);
    }

    #[test]
    fn node_json_preserves_full_origin_commit_or_explicit_null() {
        for commit in [
            None,
            Some("0123456789abcdef0123456789abcdef01234567"),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        ] {
            let node = node().with_origin_commit(
                commit.map(|value| mneme_core::OriginCommit::parse(value).unwrap()),
            );
            let rendered = node_json(&node);
            assert_eq!(rendered.get("origin_commit"), Some(&json!(commit)));
        }
    }
}

async fn edges_json(
    mem: &Memory,
    current: Option<&Node>,
    nbrs: Vec<HydratedNeighbor>,
) -> Result<Value, AnyErr> {
    let mut arr = Vec::with_capacity(nbrs.len());
    for hydrated in nbrs {
        let n = hydrated.neighbor;
        let mut obj = json!({
            "neighbor": n.node.0.to_string(),
            "summary": hydrated.node.as_ref().map(Node::summary).unwrap_or_default(),
            "kind": kind_str(n.edge.kind),
            "incoming": n.incoming,
            "weight": n.edge.weight(),
            "trials": n.edge.trials(),
            "interference": n.edge.interference(),
            "last_reinforced": n.edge.last_reinforced() as u64,
        });
        if let Some(span) = n.edge.anchor {
            obj["anchor"] = json!({ "start": span.start, "end": span.end });
            let source = if n.edge.from == n.node {
                hydrated.node.as_ref()
            } else {
                current.filter(|node| node.id() == n.edge.from)
            };
            let text = match source {
                Some(source) => mem.resolve_anchor_from(&n.edge, source).await?,
                None => None,
            };
            if let Some(text) = text {
                obj["anchor_text"] = json!(String::from_utf8_lossy(&text));
            }
        }
        arr.push(obj);
    }
    Ok(json!(arr))
}

async fn print_edges_text(
    mem: &Memory,
    current: Option<&Node>,
    nbrs: Vec<HydratedNeighbor>,
) -> Result<(), AnyErr> {
    for hydrated in nbrs {
        let n = hydrated.neighbor;
        let arrow = if n.incoming { "<--" } else { "-->" };
        println!(
            "  {arrow}[{} w={:.2} t{}] {}  {}",
            kind_str(n.edge.kind),
            n.edge.weight(),
            n.edge.trials(),
            n.node.0,
            hydrated
                .node
                .as_ref()
                .map(Node::summary)
                .unwrap_or_default()
        );
        if let Some(span) = n.edge.anchor {
            let source = if n.edge.from == n.node {
                hydrated.node.as_ref()
            } else {
                current.filter(|node| node.id() == n.edge.from)
            };
            let text = match source {
                Some(source) => mem.resolve_anchor_from(&n.edge, source).await?,
                None => None,
            }
            .unwrap_or_default();
            println!(
                "      @[{}..{}] {:?}",
                span.start,
                span.end,
                String::from_utf8_lossy(&text)
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod neighbor_render_tests {
    use super::*;

    #[tokio::test]
    async fn dangling_edges_remain_visible_and_count_toward_the_raw_sentinel() {
        let store = Arc::new(MemStore::new(DEFAULT_DIM));
        let mem = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
            Arc::new(SystemClock),
            Config {
                similarity_link_cap: 0,
                min_similarity_links: 0,
                ..Config::default()
            },
        )
        .with_body_store(Arc::new(InlineStore::new()));
        let hub = mem
            .ingest(Ingest::new(
                "diagnostic hub",
                b"",
                &[],
                Provenance::derived_empty(),
            ))
            .await
            .unwrap();
        let missing = mem
            .ingest(Ingest::new(
                "missing endpoint",
                b"",
                &[],
                Provenance::derived_empty(),
            ))
            .await
            .unwrap();
        mem.link(hub, missing, EdgeKind::Associative, 1.0, None)
            .await
            .unwrap();
        for offset in 0..GET_EDGES {
            let endpoint = mem
                .ingest(Ingest::new(
                    &format!("visible endpoint {offset}"),
                    b"",
                    &[],
                    Provenance::derived_empty(),
                ))
                .await
                .unwrap();
            mem.link(
                hub,
                endpoint,
                EdgeKind::Associative,
                0.9 - (offset as f32 * 0.01),
                None,
            )
            .await
            .unwrap();
        }
        store.delete_node(missing).await.unwrap();

        let hub_node = mem.get_node(hub).await.unwrap().unwrap();
        let (edges, has_more) = inline_edges(&mem, hub, GET_EDGES).await.unwrap();
        assert!(
            has_more,
            "the ninth raw row is the sentinel even when one row dangles"
        );
        assert_eq!(edges.len(), GET_EDGES);
        assert!(
            edges
                .iter()
                .any(|edge| edge.neighbor.node == missing && edge.node.is_none()),
            "the stronger dangling row remains diagnostic evidence"
        );

        let rendered = edges_json(&mem, Some(&hub_node), edges).await.unwrap();
        let missing_row = rendered
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["neighbor"] == missing.0.to_string())
            .unwrap();
        assert_eq!(missing_row["summary"], "");
    }
}

pub(crate) fn kind_str(k: EdgeKind) -> &'static str {
    match k {
        EdgeKind::Associative => "associative",
        EdgeKind::Transition => "transition",
        EdgeKind::Bridge => "bridge",
        EdgeKind::Supersedes => "supersedes",
        EdgeKind::DerivedFrom => "derived_from",
    }
}

fn print_json(v: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).expect("serialize json")
    );
}

/// Emit a uniform `{"status": "..."}` in JSON mode, or run the text closure.
fn emit(json: bool, status: &str, text: impl FnOnce()) {
    if json {
        print_json(&json!({ "status": status }));
    } else {
        text();
    }
}

pub(crate) fn parse_id(s: &str) -> Result<NodeId, AnyErr> {
    Ulid::from_string(s)
        .map(NodeId)
        .map_err(|e| format!("invalid node id {s:?}: {e}").into())
}

// ---- wiring (the only backend-aware code) -----------------------------------

enum Saver {
    Snapshot {
        store: Arc<MemStore>,
        path: PathBuf,
    },
    #[cfg(feature = "cozo")]
    Cozo, // cozo writes through to its sqlite file; nothing to flush
}

impl Saver {
    fn save(&self) -> Result<(), AnyErr> {
        match self {
            Saver::Snapshot { store, path } => Ok(store.save(path)?),
            #[cfg(feature = "cozo")]
            Saver::Cozo => Ok(()),
        }
    }
}

/// Bodies live in a directory next to the db (e.g. `mneme.bodies/`), as absolute
/// paths so `fs://` refs resolve regardless of the caller's working directory.
fn bodies_dir(db: &Path) -> PathBuf {
    let dir = db.with_extension("bodies");
    if dir.is_absolute() {
        dir
    } else {
        std::env::current_dir().unwrap_or_default().join(dir)
    }
}

fn assemble(
    backend: Backend,
    embedder: Arc<dyn Embedder>,
    bodies: &Path,
) -> Result<Memory, AnyErr> {
    let (graph, vectors, traversal, lexical) = backend;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let cfg = Config {
        default_body_scheme: "fs",
        ..Config::default()
    };
    let mem = Memory::new(graph, vectors, traversal, embedder, clock, cfg)
        .with_lexical_index(lexical)
        .with_body_store(Arc::new(FsStore::new(bodies)?));
    Ok(match make_reranker() {
        Some(rr) => mem.with_reranker(rr),
        None => mem,
    })
}

fn runtime_fingerprint(embedder: &dyn Embedder) -> Result<EmbeddingFingerprint, AnyErr> {
    let fingerprint = embedder.fingerprint();
    fingerprint
        .validate()
        .map_err(|e| format!("embedder returned an invalid fingerprint: {e}"))?;
    if fingerprint.dimension != embedder.dim() {
        return Err(format!(
            "embedder fingerprint dim {} ≠ embedder dim {}",
            fingerprint.dimension,
            embedder.dim()
        )
        .into());
    }
    Ok(fingerprint)
}

/// Prove the store and runtime embedder share a vector space before assembling
/// `Memory`. Missing metadata is initialized only for an empty store.
fn prepare_embedding_store<S>(store: &S, embedder: &dyn Embedder, db: &Path) -> Result<(), AnyErr>
where
    S: EmbeddingMetadataStore + VectorIndex,
{
    let fingerprint = runtime_fingerprint(embedder)?;
    if store.dim() != embedder.dim() {
        return Err(format!(
            "vector index dim {} ≠ embedder dim {}; run `mnemed --db <PATH> reembed` \
             for {} to rebuild every vector",
            store.dim(),
            embedder.dim(),
            db.display()
        )
        .into());
    }
    store
        .ensure_embedding_fingerprint(&fingerprint)
        .map(|_| ())
        .map_err(|error| match error {
            Error::LegacyEmbeddingFingerprint
            | Error::EmbeddingFingerprintMismatch { .. }
            | Error::InvalidEmbeddingFingerprint(_) => format!(
                "{error}; run `mnemed --db <PATH> reembed` for {} to rebuild every vector \
                     and adopt the runtime fingerprint",
                db.display()
            )
            .into(),
            other => Box::new(other) as AnyErr,
        })
}

/// Whether the optional cross-encoder reranker is enabled (env `MNEME_RERANK`
/// truthy). Off by default — it loads a second ONNX model.
#[cfg(feature = "fastembed")]
fn rerank_enabled() -> bool {
    std::env::var_os("MNEME_RERANK")
        .map(|v| v.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

#[cfg(not(feature = "fastembed"))]
fn make_reranker() -> Option<Arc<dyn Reranker>> {
    None
}

#[cfg(feature = "fastembed")]
fn make_reranker() -> Option<Arc<dyn Reranker>> {
    rerank_enabled().then(|| {
        Arc::new(LazyReranker {
            cell: tokio::sync::OnceCell::new(),
        }) as Arc<dyn Reranker>
    })
}

/// Lazy cross-encoder reranker: loads bge-reranker on first `rerank`, not at open,
/// so the model cost is paid only by reranked queries (and only when enabled).
#[cfg(feature = "fastembed")]
struct LazyReranker {
    cell: tokio::sync::OnceCell<mneme_embed::FastReranker>,
}

#[cfg(feature = "fastembed")]
#[async_trait::async_trait]
impl Reranker for LazyReranker {
    fn semantic_id(&self) -> &'static str {
        "fastembed-bge-reranker-base-v1"
    }

    async fn rerank(&self, query: &str, docs: &[&str]) -> mneme_core::ports::Result<Vec<f32>> {
        let model = self
            .cell
            .get_or_try_init(|| async { mneme_embed::FastReranker::new() })
            .await?;
        model.rerank(query, docs).await
    }
}

#[cfg(not(feature = "fastembed"))]
fn make_embedder(dim: usize) -> Result<Arc<dyn Embedder>, AnyErr> {
    Ok(Arc::new(HashingEmbedder::new(dim)))
}

#[cfg(feature = "fastembed")]
fn make_embedder(_dim: usize) -> Result<Arc<dyn Embedder>, AnyErr> {
    // Lazy: the ONNX model loads only when something actually embeds, so
    // read-only commands (list/get/neighbors/forget) don't pay to load it.
    Ok(Arc::new(LazyEmbedder {
        cell: tokio::sync::OnceCell::new(),
    }))
}

/// Wraps the real (BGE-base, 768-dim) embedder so the model is downloaded/loaded
/// on first `embed`, not at construction. `dim` is known up front, so the wiring
/// dimension check doesn't force a load.
#[cfg(feature = "fastembed")]
struct LazyEmbedder {
    cell: tokio::sync::OnceCell<mneme_embed::FastEmbedder>,
}

#[cfg(feature = "fastembed")]
#[async_trait::async_trait]
impl Embedder for LazyEmbedder {
    fn dim(&self) -> usize {
        DEFAULT_DIM
    }

    fn fingerprint(&self) -> EmbeddingFingerprint {
        mneme_embed::fastembed_fingerprint()
    }

    async fn embed(&self, texts: &[&str]) -> mneme_core::ports::Result<Vec<Vec<f32>>> {
        let model = self
            .cell
            .get_or_try_init(|| async { mneme_embed::FastEmbedder::new() })
            .await?;
        model.embed(texts).await
    }

    async fn embed_query(&self, query: &str) -> mneme_core::ports::Result<Vec<f32>> {
        // Delegate so the real model's query-prefix override is reached, not the
        // trait default (which would skip the prefix).
        let model = self
            .cell
            .get_or_try_init(|| async { mneme_embed::FastEmbedder::new() })
            .await?;
        model.embed_query(query).await
    }
}

/// Open the JSON snapshot (reference) backend at `db`. Available in every build,
/// so a cozo binary can still read a legacy snapshot store.
fn open_snapshot(db: &Path) -> Result<(Memory, Saver, Ulid), AnyErr> {
    let store = if db.exists() {
        Arc::new(MemStore::load(db)?)
    } else {
        Arc::new(MemStore::new(DEFAULT_DIM))
    };
    let dim = store.dim();
    let db_id = store.db_id();
    let embedder = make_embedder(dim)?;
    prepare_embedding_store(store.as_ref(), embedder.as_ref(), db)?;
    let mem = assemble(
        (store.clone(), store.clone(), store.clone(), store.clone()),
        embedder,
        &bodies_dir(db),
    )?;
    Ok((
        mem,
        Saver::Snapshot {
            store,
            path: db.to_path_buf(),
        },
        db_id,
    ))
}

#[cfg(not(feature = "cozo"))]
fn open(
    db: &Path,
    _lease: Arc<mneme_store_path::StoreLease>,
    _feedback_epoch: Option<&str>,
) -> Result<(Memory, Saver, Ulid), AnyErr> {
    open_snapshot(db)
}

#[cfg(feature = "cozo")]
fn open(
    db: &Path,
    lease: Arc<mneme_store_path::StoreLease>,
    feedback_epoch: Option<&str>,
) -> Result<(Memory, Saver, Ulid), AnyErr> {
    // Fall back to the snapshot backend for an existing JSON store (so a cozo
    // build reads legacy stores and can `migrate` them); otherwise use — or
    // create — the persistent sqlite-backed cozo db, which writes through.
    if db.exists() && is_snapshot(db)? {
        return open_snapshot(db);
    }
    let store = Arc::new(CozoStore::open_persistent(db, DEFAULT_DIM, lease)?);
    if let Some(epoch) = feedback_epoch {
        store.activate_feedback_epoch(epoch)?;
    }
    let db_id = store.db_id();
    let dim = store.dim();
    let embedder = make_embedder(dim)?;
    prepare_embedding_store(store.as_ref(), embedder.as_ref(), db)?;
    let mem = assemble(
        (store.clone(), store.clone(), store.clone(), store.clone()),
        embedder,
        &bodies_dir(db),
    )?;
    Ok((mem, Saver::Cozo, db_id))
}

/// Sniff whether `db` is a JSON snapshot rather than a cozo sqlite file: the
/// sqlite file opens with the well-known magic, our snapshot starts with `{`.
#[cfg(feature = "cozo")]
fn is_snapshot(db: &Path) -> Result<bool, AnyErr> {
    use std::io::Read;
    let mut buf = [0u8; 15];
    let n = std::fs::File::open(db)?.read(&mut buf)?;
    Ok(&buf[..n] != b"SQLite format 3")
}

// ---- demo (ephemeral; ignores --db) -----------------------------------------

#[cfg(not(feature = "cozo"))]
fn fresh_backend(dim: usize) -> Result<Backend, AnyErr> {
    let store = Arc::new(MemStore::new(dim));
    Ok((store.clone(), store.clone(), store.clone(), store))
}

#[cfg(feature = "cozo")]
fn fresh_backend(dim: usize) -> Result<Backend, AnyErr> {
    let store = Arc::new(CozoStore::new(dim)?);
    Ok((store.clone(), store.clone(), store.clone(), store))
}

async fn run_demo(query: Option<String>) -> Result<(), AnyErr> {
    let dim = DEFAULT_DIM;
    let (graph, vectors, traversal, lexical) = fresh_backend(dim)?;
    let embedder = Arc::new(HashingEmbedder::new(dim));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mem = Memory::new(
        graph,
        vectors,
        traversal,
        embedder,
        clock,
        Config::default(),
    )
    .with_lexical_index(lexical)
    .with_body_store(Arc::new(InlineStore::new()));

    // Summaries deliberately share vocabulary within a topic: the reference
    // embedder is lexical (bag-of-words), so token overlap is what links nodes.
    let seeds: &[(&str, &[&str])] = &[
        (
            "Rust async runtime Tokio polls futures to completion",
            &["rust", "async"],
        ),
        (
            "Rust async await desugars futures into state machines",
            &["rust", "async"],
        ),
        (
            "An async runtime drives futures by polling them",
            &["rust", "async"],
        ),
        (
            "Spreading activation retrieves nodes from the memory graph",
            &["graph", "memory"],
        ),
        (
            "Memory graph retrieval ranks nodes by spreading activation",
            &["graph", "memory"],
        ),
        (
            "Community detection clusters the memory graph into related nodes",
            &["graph", "memory"],
        ),
        (
            "Sourdough bread rises with a slow cold ferment",
            &["baking"],
        ),
    ];
    for (summary, tags) in seeds {
        let body = format!("full body for: {summary}");
        mem.ingest(Ingest {
            summary,
            body: body.as_bytes(),
            body_ref: None,
            tags,
            provenance: Provenance::derived_empty(),
            stability: 0.6,
            confidence: 0.6,
            origin_commit: None,
        })
        .await?;
    }

    let query =
        query.unwrap_or_else(|| "how does spreading activation over a memory graph work".into());
    println!("seeded {} nodes\nquery: {query:?}\n", seeds.len());
    for (i, r) in mem.retrieve(&query).await?.iter().enumerate() {
        println!("{:>2}. [{:.3}] {}", i + 1, r.score, r.node.summary());
    }
    println!("\n(this is a scratch demo; use `ingest`/`query`/`get` against --db for real use)");
    Ok(())
}

#[cfg(test)]
mod embedding_identity_tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn all_node_rebuild_includes_archived_semantics_and_historical_episode_editions() {
        use mneme_core::ports::EpisodeStore;
        use mneme_core::{
            CaptureRequestCodec, CaptureSource, EpisodeCommit, EpisodeFacet, EpisodeRevisionReason,
            EpisodeTime, EpisodeWriteExpectation, OccurrenceSpan,
        };
        let source = MemStore::new(8);
        let mut archived = Node::try_new(
            NodeId(Ulid::new()),
            "archived semantic",
            BodyRef::new("fs://archived").unwrap(),
            ["history"],
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        archived.set_status(NodeStatus::Archived);
        source.put_node(&archived).await.unwrap();
        let episode = |key: &str, summary: &str, now: u128| {
            let proof = CaptureSource::new_with_codec(
                "reembed-test",
                key,
                "test://episode",
                None::<&str>,
                None::<&str>,
                [now as u8; 32],
                CaptureRequestCodec::EpisodeV1,
            )
            .unwrap();
            Node::try_new(
                proof.node_id(),
                summary,
                BodyRef::new(format!("fs://{key}")).unwrap(),
                ["history"],
                Provenance::External { source: proof },
                0.5,
                0.5,
                NodeStatus::Active,
                now,
            )
            .unwrap()
        };
        let initial = episode("initial", "first episode edition", 2);
        let initial_id = initial.id();
        let initial = initial
            .with_episode(
                EpisodeFacet::initial(
                    initial_id,
                    OccurrenceSpan::Unknown,
                    None,
                    EpisodeTime::new(2).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        source
            .commit_episode(EpisodeCommit {
                node: &initial,
                embedding: &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                links: &[],
                expectation: EpisodeWriteExpectation::NewRoot,
            })
            .await
            .unwrap();
        let revised = episode("revised", "corrected episode edition", 3);
        let revised = revised
            .with_episode(
                EpisodeFacet::revised(
                    initial_id.into(),
                    initial_id,
                    initial.episode().unwrap().revision().next().unwrap(),
                    OccurrenceSpan::Unknown,
                    None,
                    initial.episode().unwrap().recorded_at(),
                    EpisodeRevisionReason::new("Correction after a second look").unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        source
            .commit_episode(EpisodeCommit {
                node: &revised,
                embedding: &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                links: &[],
                expectation: EpisodeWriteExpectation::CurrentEdition(initial_id),
            })
            .await
            .unwrap();
        source
            .upsert(archived.id(), &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        let embedder = TestReembedder::new(4, "test:episode-inclusive-v1", None);
        let (rebuilt, fingerprint) = rebuild_all_node_vectors(&source, &embedder, 2)
            .await
            .unwrap();
        let export = rebuilt.export();
        assert_eq!(export.nodes.len(), 3);
        assert_eq!(export.vectors.len(), 3);
        assert!(export.vectors.iter().all(|(_, vector)| vector.len() == 4));
        assert_eq!(rebuilt.embedding_fingerprint().unwrap(), Some(fingerprint));
        assert!(
            export
                .nodes
                .iter()
                .any(|node| node.id() == archived.id() && node.status() == NodeStatus::Archived)
        );
        assert!(
            export
                .nodes
                .iter()
                .any(|node| node.id() == initial.id() && node.episode().is_some())
        );
        assert!(
            export
                .nodes
                .iter()
                .any(|node| node.id() == revised.id() && node.episode().is_some())
        );
    }

    struct TestReembedder {
        dim: usize,
        identity: &'static str,
        fail_on_call: Option<usize>,
        calls: AtomicUsize,
        max_batch: AtomicUsize,
    }

    impl TestReembedder {
        fn new(dim: usize, identity: &'static str, fail_on_call: Option<usize>) -> Self {
            Self {
                dim,
                identity,
                fail_on_call,
                calls: AtomicUsize::new(0),
                max_batch: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Embedder for TestReembedder {
        fn dim(&self) -> usize {
            self.dim
        }

        fn fingerprint(&self) -> EmbeddingFingerprint {
            EmbeddingFingerprint::new(self.identity, self.dim, "l2-f32-v1", "symmetric-test-v1")
        }

        async fn embed(&self, texts: &[&str]) -> mneme_core::ports::Result<Vec<Vec<f32>>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_batch.fetch_max(texts.len(), Ordering::SeqCst);
            if self.fail_on_call == Some(call) {
                return Err(Error::Backend(format!(
                    "injected embedder failure on call {call}"
                )));
            }
            Ok(texts
                .iter()
                .map(|text| {
                    let mut vector = vec![0.0; self.dim];
                    let bucket = text
                        .as_bytes()
                        .iter()
                        .fold(0usize, |sum, byte| sum.wrapping_add(*byte as usize))
                        % self.dim;
                    vector[bucket] = 1.0;
                    vector
                })
                .collect())
        }
    }

    async fn seeded_reembed_store(dim: usize, count: usize) -> MemStore {
        let store = MemStore::new(dim);
        store
            .set_embedding_fingerprint(&EmbeddingFingerprint::new(
                "test:old-reembed-space-v1",
                dim,
                "l2-f32-v1",
                "symmetric-test-v1",
            ))
            .unwrap();
        for index in 0..count {
            let mut node = Node::try_new(
                NodeId(Ulid::new()),
                format!("bounded reembed node {index}"),
                BodyRef::new(format!("fs://reembed-{index}")).unwrap(),
                ["reembed"],
                Provenance::derived_empty(),
                0.5,
                0.5,
                NodeStatus::Active,
                index as u128,
            )
            .unwrap();
            if index + 1 == count {
                node.set_status(NodeStatus::Archived);
            }
            store.put_node(&node).await.unwrap();
            let mut vector = vec![0.0; dim];
            vector[index % dim] = 1.0;
            store.upsert(node.id(), &vector).await.unwrap();
        }
        store
    }

    #[test]
    fn normal_open_contract_initializes_only_an_empty_store() {
        let store = MemStore::new(8);
        let embedder = HashingEmbedder::new(8);
        prepare_embedding_store(&store, &embedder, Path::new("memory.json")).unwrap();
        assert_eq!(
            store.embedding_fingerprint().unwrap(),
            Some(embedder.fingerprint())
        );
    }

    #[tokio::test]
    async fn normal_open_contract_rejects_legacy_and_same_dim_mismatch_with_recovery() {
        let runtime = HashingEmbedder::new(8);

        let legacy = MemStore::new(8);
        let legacy_id = NodeId(Ulid::new());
        legacy
            .put_node(
                &Node::try_new(
                    legacy_id,
                    "legacy fingerprint node",
                    BodyRef::new("inline://legacy").unwrap(),
                    std::iter::empty::<&str>(),
                    Provenance::derived_empty(),
                    0.5,
                    0.5,
                    NodeStatus::Active,
                    1,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        legacy
            .upsert(legacy_id, &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        let error = prepare_embedding_store(&legacy, &runtime, Path::new("legacy.json"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("no embedding fingerprint"));
        assert!(error.contains("mnemed --db <PATH> reembed"));

        let mismatch = MemStore::new(8);
        mismatch
            .set_embedding_fingerprint(&EmbeddingFingerprint::new(
                "test:other-embedder-v1",
                8,
                "l2-f32-v1",
                "symmetric-document-v1",
            ))
            .unwrap();
        let error = prepare_embedding_store(&mismatch, &runtime, Path::new("mismatch.json"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("fingerprint mismatch"));
        assert!(error.contains("mnemed --db <PATH> reembed"));
    }

    fn with_advisory_concern(source: MemStore) -> MemStore {
        use mneme_core::{ConcernBinding, ConcernEndpoint, ConcernKind, ConcernNotice, ConcernRow};
        let mut export = source.export();
        let nodes: Vec<_> = export
            .nodes
            .iter()
            .filter(|node| node.is_active())
            .take(2)
            .collect();
        let binding = ConcernBinding::new(
            ConcernKind::Disagreement,
            ConcernEndpoint::from_node(nodes[0]),
            ConcernEndpoint::from_node(nodes[1]),
        )
        .unwrap();
        export.concerns.push(ConcernRow::from_notice(
            ConcernNotice::new(
                binding,
                "fixture advisory tension",
                "which context applies?",
            )
            .unwrap(),
        ));
        MemStore::from_export(export).unwrap()
    }

    #[tokio::test]
    async fn snapshot_reembed_is_bounded_and_failure_leaves_source_untouched() {
        let path = std::env::temp_dir().join(format!("mnemed-reembed-{}.json", Ulid::new()));
        let source = with_advisory_concern(seeded_reembed_store(8, 7).await);
        let db_id = source.db_id();
        let old_fingerprint = source.embedding_fingerprint().unwrap();
        let expected_concerns = source.export().concerns;
        source.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();

        let failing = TestReembedder::new(4, "test:new-reembed-space-v1", Some(2));
        let error = reembed_snapshot_with(&path, &source, &failing, 2)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("injected embedder failure"), "{error}");
        assert_eq!(failing.max_batch.load(Ordering::SeqCst), 2);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "no failed inference batch may rewrite the live snapshot"
        );
        let unchanged = MemStore::load(&path).unwrap();
        assert_eq!(unchanged.dim(), 8);
        assert_eq!(unchanged.embedding_fingerprint().unwrap(), old_fingerprint);

        let successful = TestReembedder::new(4, "test:new-reembed-space-v1", None);
        let (count, fingerprint) = reembed_snapshot_with(&path, &source, &successful, 2)
            .await
            .unwrap();
        assert_eq!(count, 7);
        assert_eq!(successful.calls.load(Ordering::SeqCst), 4);
        assert_eq!(successful.max_batch.load(Ordering::SeqCst), 2);
        let rebuilt = MemStore::load(&path).unwrap();
        let export = rebuilt.export();
        assert_eq!(rebuilt.db_id(), db_id);
        assert_eq!(rebuilt.dim(), 4);
        assert_eq!(rebuilt.embedding_fingerprint().unwrap(), Some(fingerprint));
        assert_eq!(export.concerns, expected_concerns);
        assert_eq!(export.nodes.len(), 7);
        assert_eq!(export.vectors.len(), 7);
        assert!(export.vectors.iter().all(|(_, vector)| vector.len() == 4));
        assert_eq!(
            export.nodes.iter().filter(|node| node.is_active()).count(),
            6
        );
        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn migration_refuses_v2_predecessor_without_publication() {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mnemed-migrate-v2-{}.json", Ulid::new()));
        MemStore::new(8).save_single_graph_v2(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let lease = Arc::new(mneme_store_path::StoreLease::acquire(&path).unwrap());
        assert!(run_migrate(&path, lease.clone()).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!path.with_extension("snapshot.bak").exists());
        drop(lease);
        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn migration_keeps_source_backup_and_verifies_every_store_relation() {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mnemed-migrate-{}.json", Ulid::new()));
        let backup = path.with_extension("snapshot.bak");
        let body_root = path.with_extension("bodies");
        std::fs::create_dir(&body_root).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&body_root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(body_root.join("a"), b"migration body a").unwrap();
        std::fs::write(body_root.join("b"), b"migration body b").unwrap();
        #[cfg(unix)]
        for key in ["a", "b"] {
            std::fs::set_permissions(body_root.join(key), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let source = MemStore::new(8);
        let fingerprint =
            EmbeddingFingerprint::new("test:migration-v1", 8, "l2-f32-v1", "symmetric-v1");
        source.set_embedding_fingerprint(&fingerprint).unwrap();
        let a = Node::try_new(
            NodeId(Ulid::new()),
            "migration a",
            BodyRef::new("fs://a").unwrap(),
            ["migration"],
            Provenance::derived_empty(),
            0.6,
            0.7,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        let b = Node::try_new(
            NodeId(Ulid::new()),
            "migration b",
            BodyRef::new("fs://b").unwrap(),
            ["migration"],
            Provenance::derived([a.id()]).unwrap(),
            0.6,
            0.7,
            NodeStatus::Active,
            2,
        )
        .unwrap();
        let inline = Node::try_new(
            NodeId(Ulid::new()),
            "migration inline reference",
            BodyRef::new("inline://migration-inline").unwrap(),
            ["migration"],
            Provenance::derived_empty(),
            0.6,
            0.7,
            NodeStatus::Active,
            3,
        )
        .unwrap();
        source.put_node(&a).await.unwrap();
        source.put_node(&b).await.unwrap();
        source.put_node(&inline).await.unwrap();
        source
            .upsert(a.id(), &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        source
            .upsert(b.id(), &[0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        source
            .upsert(inline.id(), &[0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        source
            .put_edge(&mneme_core::Edge::new(
                a.id(),
                b.id(),
                0.8,
                EdgeKind::DerivedFrom,
                3,
            ))
            .await
            .unwrap();
        source
            .observe_contradiction(a.id(), b.id(), 4)
            .await
            .unwrap();
        source
            .observe_merge_candidate(a.id(), b.id(), 5)
            .await
            .unwrap();
        let remote = mneme_core::RemoteEdge::new(a.id(), Ulid::new(), NodeId(Ulid::new()), 0.9);
        source.put_remote_edge(&remote).await.unwrap();
        let source = with_advisory_concern(source);
        let expected = source.export();
        source.save(&path).unwrap();

        let lease = Arc::new(mneme_store_path::StoreLease::acquire(&path).unwrap());
        run_migrate(&path, lease.clone()).await.unwrap();
        drop(lease);
        assert!(
            backup.exists(),
            "the JSON is retained as an explicit backup"
        );
        assert!(is_snapshot(&backup).unwrap());
        assert!(!is_snapshot(&path).unwrap());
        let backed_up = MemStore::load(&backup).unwrap();
        assert_eq!(backed_up.db_id(), source.db_id());

        let readback_lease = Arc::new(mneme_store_path::StoreLease::acquire(&path).unwrap());
        let migrated = CozoStore::open_existing_persistent(&path, 8, readback_lease).unwrap();
        assert_eq!(migrated.db_id(), source.db_id());
        migrated.verify_import(&expected).await.unwrap();
        assert_eq!(
            migrated
                .remote_edges_page(a.id(), None, MAX_REMOTE_EDGE_PAGE_SIZE)
                .await
                .unwrap()
                .items,
            vec![remote]
        );
        drop(migrated);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::remove_dir_all(&body_root);
    }

    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn cozo_reembed_failure_keeps_source_and_detached_retry_rebuilds_all_vectors() {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mnemed-cozo-reembed-{}.db", Ulid::new()));
        let body_root = path.with_extension("bodies");
        std::fs::create_dir(&body_root).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&body_root, std::fs::Permissions::from_mode(0o700)).unwrap();
        for index in 0..7 {
            let file = body_root.join(format!("reembed-{index}"));
            std::fs::write(&file, format!("body {index}")).unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let source = with_advisory_concern(seeded_reembed_store(8, 7).await);
        let missing_before_reembed = source.export().nodes[0].id();
        source.remove(missing_before_reembed).await.unwrap();
        let old_fingerprint = source.embedding_fingerprint().unwrap();
        let old_export = source.export();
        let candidate = old_export
            .nodes
            .iter()
            .find(|node| matches!(node.status(), NodeStatus::Active))
            .unwrap()
            .id();
        let mut store = CozoStore::open(path.to_str().unwrap(), 8).unwrap();
        store.import_mem(&source).await.unwrap();
        store.prepare_for_file_move().unwrap();
        drop(store);
        let lease = Arc::new(mneme_store_path::StoreLease::acquire(&path).unwrap());
        let before_bytes = std::fs::read(&path).unwrap();
        let body_before = std::fs::read(body_root.join("reembed-0")).unwrap();

        let failing = TestReembedder::new(4, "test:new-cozo-space-v1", Some(2));
        let error = reembed_cozo_with(&path, &lease, &failing, 2)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("injected embedder failure"), "{error}");
        assert_eq!(failing.max_batch.load(Ordering::SeqCst), 2);
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
        assert_eq!(
            std::fs::read(body_root.join("reembed-0")).unwrap(),
            body_before
        );
        assert!(!path.with_extension("reembed.bak").exists());
        let store = CozoStore::open_existing_persistent(&path, 8, lease.clone()).unwrap();
        assert_eq!(store.dim(), 8);
        assert_eq!(store.embedding_fingerprint().unwrap(), old_fingerprint);
        let after_failure = store.export().await.unwrap();
        let mut old_vectors = old_export.vectors.clone();
        old_vectors.sort_by_key(|(id, _)| *id);
        let mut live_vectors = after_failure.vectors;
        live_vectors.sort_by_key(|(id, _)| *id);
        assert_eq!(live_vectors, old_vectors, "live vectors remain untouched");
        drop(store);

        let successful = TestReembedder::new(4, "test:new-cozo-space-v1", None);
        let (count, fingerprint) = reembed_cozo_with(&path, &lease, &successful, 2)
            .await
            .unwrap();
        assert_eq!(count, 7);
        assert_eq!(successful.calls.load(Ordering::SeqCst), 4);
        assert_eq!(successful.max_batch.load(Ordering::SeqCst), 2);
        let store = CozoStore::open_existing_persistent(&path, 4, lease.clone()).unwrap();
        assert_eq!(store.dim(), 4);
        assert_eq!(
            store.embedding_fingerprint().unwrap(),
            Some(fingerprint.clone())
        );
        let rebuilt = store.export().await.unwrap();
        assert_eq!(rebuilt.concerns, old_export.concerns);
        assert_eq!(rebuilt.nodes.len(), 7);
        assert_eq!(rebuilt.vectors.len(), 7);
        assert!(
            rebuilt
                .vectors
                .iter()
                .any(|(id, _)| *id == missing_before_reembed)
        );
        assert!(rebuilt.vectors.iter().all(|(_, vector)| vector.len() == 4));
        assert!(
            store
                .search("bounded reembed node", 16, StatusFilter::ACTIVE)
                .await
                .unwrap()
                .iter()
                .any(|hit| hit.id == candidate)
        );
        drop(store);

        let reopened = CozoStore::open_existing_persistent(&path, 4, lease.clone()).unwrap();
        assert_eq!(reopened.dim(), 4);
        assert_eq!(reopened.embedding_fingerprint().unwrap(), Some(fingerprint));
        assert_eq!(reopened.export().await.unwrap().vectors.len(), 7);
        drop(reopened);
        assert!(path.with_extension("reembed.bak").exists());
        let published = std::fs::read(&path).unwrap();
        let retained = std::fs::read(path.with_extension("reembed.bak")).unwrap();
        let retry = TestReembedder::new(4, "test:retry-space-v1", None);
        let error = reembed_cozo_with(&path, &lease, &retry, 2)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("refusing to overwrite existing migration backup"));
        assert_eq!(retry.calls.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(&path).unwrap(), published);
        assert_eq!(
            std::fs::read(path.with_extension("reembed.bak")).unwrap(),
            retained
        );
        drop(lease);
        let _ = std::fs::remove_file(path.with_extension("reembed.bak"));
        let _ = std::fs::remove_dir_all(body_root);
        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn migration_refuses_to_overwrite_an_existing_backup() {
        let path = std::env::temp_dir().join(format!("mnemed-migrate-{}.json", Ulid::new()));
        let backup = path.with_extension("snapshot.bak");
        MemStore::new(8).save(&path).unwrap();
        std::fs::write(&backup, b"precious prior backup").unwrap();
        let source_before = std::fs::read(&path).unwrap();

        let lease = Arc::new(mneme_store_path::StoreLease::acquire(&path).unwrap());
        let error = run_migrate(&path, lease).await.unwrap_err().to_string();
        assert!(error.contains("refusing to overwrite existing migration backup"));
        assert_eq!(std::fs::read(&path).unwrap(), source_before);
        assert_eq!(std::fs::read(&backup).unwrap(), b"precious prior backup");

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(backup);
    }

    #[cfg(feature = "cozo")]
    #[test]
    fn failed_final_migration_rename_retains_the_original_source() {
        let path = std::env::temp_dir().join(format!("mnemed-install-{}.json", Ulid::new()));
        let backup = path.with_extension("snapshot.bak");
        std::fs::write(&path, b"original snapshot bytes").unwrap();
        let mut temp = MigrationTemp::reserve_beside(&path).unwrap();
        let temp_path = temp.path().to_path_buf();

        let error = install_verified_migration(
            &path,
            &backup,
            &mut temp,
            bounded_regular_sha256(&path).unwrap(),
            || Ok(()),
            |_, _| Err(std::io::Error::other("injected final rename failure")),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("selected source was not replaced"));
        assert_eq!(std::fs::read(&path).unwrap(), b"original snapshot bytes");
        #[cfg(unix)]
        assert_eq!(std::fs::metadata(&path).unwrap().nlink(), 1);
        assert!(!backup.exists());
        assert!(
            temp_path.exists(),
            "verified temp remains until guard cleanup"
        );
        drop(temp);
        assert!(!temp_path.exists());

        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "cozo")]
    #[test]
    fn migration_install_checkpoints_preserve_prepublication_source_and_report_postrename_state() {
        for point in [
            "before_backup_create",
            "after_backup_create",
            "after_backup_copy_chunk",
            "before_backup_file_sync",
            "after_backup_file_sync",
            "after_backup_readback",
            "before_backup_parent_sync",
            "after_backup_parent_sync",
            "before_final_rename",
            "after_final_rename",
            "before_final_parent_sync",
            "after_final_parent_sync",
        ] {
            let db = std::env::temp_dir().join(format!("mnemed-install-cut-{}.db", Ulid::new()));
            let backup = db.with_extension("snapshot.bak");
            std::fs::write(&db, b"original source bytes").unwrap();
            let mut temp = MigrationTemp::reserve_beside(&db).unwrap();
            let stage = temp.path().to_path_buf();
            std::fs::write(&stage, b"verified replacement bytes").unwrap();
            MIGRATION_INSTALL_FAILURE.with(|slot| slot.set(Some(point)));
            let error = install_verified_migration(
                &db,
                &backup,
                &mut temp,
                bounded_regular_sha256(&db).unwrap(),
                || Ok(()),
                |from, to| std::fs::rename(from, to),
            )
            .unwrap_err()
            .to_string();
            MIGRATION_INSTALL_FAILURE.with(|slot| slot.set(None));
            assert!(error.contains(point), "{point}: {error}");
            let installed = matches!(
                point,
                "after_final_rename" | "before_final_parent_sync" | "after_final_parent_sync"
            );
            if installed {
                assert_eq!(std::fs::read(&db).unwrap(), b"verified replacement bytes");
                assert_eq!(std::fs::read(&backup).unwrap(), b"original source bytes");
                assert!(error.contains("InstalledDurabilityUnknown"));
                assert!(!stage.exists());
            } else {
                assert_eq!(std::fs::read(&db).unwrap(), b"original source bytes");
                assert!(!backup.exists(), "{point}: owned copy should be removed");
                assert!(stage.exists(), "{point}: stage remains until guard drop");
            }
            #[cfg(unix)]
            assert_eq!(
                std::fs::metadata(&db).unwrap().nlink(),
                1,
                "{point}: selected source must remain singly linked"
            );
            drop(temp);
            assert!(!stage.exists());
            let _ = std::fs::remove_file(&backup);
            std::fs::remove_file(&db).unwrap();
        }
    }

    #[cfg(feature = "cozo")]
    #[test]
    fn migration_backup_cleanup_refusal_retains_exact_copy_and_stage_for_recovery() {
        for point in [
            "before_owned_backup_cleanup",
            "before_backup_cleanup_parent_sync",
        ] {
            let db = std::env::temp_dir().join(format!("mnemed-cleanup-cut-{}.db", Ulid::new()));
            let backup = db.with_extension("snapshot.bak");
            std::fs::write(&db, b"original source bytes").unwrap();
            let mut temp = MigrationTemp::reserve_beside(&db).unwrap();
            let stage = temp.path().to_path_buf();
            std::fs::write(&stage, b"verified replacement bytes").unwrap();
            MIGRATION_INSTALL_FAILURE.with(|slot| slot.set(Some(point)));
            let error = install_verified_migration(
                &db,
                &backup,
                &mut temp,
                bounded_regular_sha256(&db).unwrap(),
                || Ok(()),
                |_, _| Err(std::io::Error::other("injected final rename refusal")),
            )
            .unwrap_err()
            .to_string();
            MIGRATION_INSTALL_FAILURE.with(|slot| slot.set(None));
            assert_eq!(std::fs::read(&db).unwrap(), b"original source bytes");
            assert!(stage.exists(), "{point}: recovery stage must be retained");
            #[cfg(unix)]
            assert_eq!(std::fs::metadata(&db).unwrap().nlink(), 1);
            if point == "before_owned_backup_cleanup" {
                assert_eq!(std::fs::read(&backup).unwrap(), b"original source bytes");
                assert!(error.contains("must be inspected"));
            } else {
                assert!(!backup.exists());
                assert!(error.contains("cleanup durability is uncertain"));
            }
            drop(temp);
            assert!(stage.exists(), "{point}: disarmed stage must survive drop");
            let _ = std::fs::remove_file(&backup);
            std::fs::remove_file(&stage).unwrap();
            std::fs::remove_file(&db).unwrap();
        }
    }

    #[cfg(feature = "cozo")]
    #[test]
    fn migration_rename_error_classifies_crossed_and_unknown_outcomes() {
        for crossed in [true, false] {
            let db = std::env::temp_dir().join(format!("mnemed-rename-class-{}.json", Ulid::new()));
            let backup = db.with_extension("snapshot.bak");
            std::fs::write(&db, b"old bytes").unwrap();
            let mut temp = MigrationTemp::reserve_beside(&db).unwrap();
            std::fs::write(temp.path(), b"new bytes").unwrap();
            let error = install_verified_migration(
                &db,
                &backup,
                &mut temp,
                bounded_regular_sha256(&db).unwrap(),
                || Ok(()),
                |from, to| {
                    if crossed {
                        std::fs::rename(from, to)?;
                    } else {
                        std::fs::remove_file(from)?;
                    }
                    Err(std::io::Error::other(
                        "injected lost rename acknowledgement",
                    ))
                },
            )
            .unwrap_err();
            let typed = error.downcast_ref::<MigrationInstallFailure>().unwrap();
            assert_eq!(
                typed.state,
                if crossed {
                    MigrationInstallState::InstalledDurabilityUnknown
                } else {
                    MigrationInstallState::Ambiguous
                }
            );
            assert_eq!(
                std::fs::read(&db).unwrap(),
                if crossed {
                    b"new bytes".as_slice()
                } else {
                    b"old bytes".as_slice()
                }
            );
            assert_eq!(std::fs::read(&backup).unwrap(), b"old bytes");
            #[cfg(unix)]
            assert_eq!(std::fs::metadata(&db).unwrap().nlink(), 1);
            drop(temp);
            std::fs::remove_file(&backup).unwrap();
            std::fs::remove_file(&db).unwrap();
        }
    }

    #[cfg(feature = "cozo")]
    #[test]
    fn migration_source_guard_refusal_keeps_selected_source_admissible() {
        let db = std::env::temp_dir().join(format!("mnemed-guard-refusal-{}.json", Ulid::new()));
        let backup = db.with_extension("snapshot.bak");
        MemStore::new(8).save(&db).unwrap();
        let original = std::fs::read(&db).unwrap();
        let mut temp = MigrationTemp::reserve_beside(&db).unwrap();
        std::fs::write(temp.path(), b"new bytes").unwrap();
        let error = install_verified_migration(
            &db,
            &backup,
            &mut temp,
            bounded_regular_sha256(&db).unwrap(),
            || Err("injected source/body seal drift".into()),
            |from, to| std::fs::rename(from, to),
        )
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<MigrationInstallFailure>()
                .unwrap()
                .state,
            MigrationInstallState::NotPublished
        );
        assert_eq!(std::fs::read(&db).unwrap(), original);
        assert!(!backup.exists());
        let lease = mneme_store_path::StoreLease::acquire(&db).unwrap();
        assert_eq!(MemStore::load(&db).unwrap().dim(), 8);
        drop(lease);
        drop(temp);
        std::fs::remove_file(&db).unwrap();
    }

    #[cfg(feature = "cozo")]
    #[test]
    fn migration_copy_readback_refusal_and_existing_backup_do_not_replace_source() {
        let db = std::env::temp_dir().join(format!("mnemed-copy-refusal-{}.json", Ulid::new()));
        let backup = db.with_extension("snapshot.bak");
        MemStore::new(8).save(&db).unwrap();
        let original = std::fs::read(&db).unwrap();
        let mut temp = MigrationTemp::reserve_beside(&db).unwrap();
        std::fs::write(temp.path(), b"new bytes").unwrap();
        let error = install_verified_migration(
            &db,
            &backup,
            &mut temp,
            [0; 32],
            || Ok(()),
            |from, to| std::fs::rename(from, to),
        )
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<MigrationInstallFailure>()
                .unwrap()
                .state,
            MigrationInstallState::NotPublished
        );
        assert!(
            error
                .to_string()
                .contains("backup does not match sealed source")
        );
        assert_eq!(std::fs::read(&db).unwrap(), original);
        assert!(!backup.exists());

        std::fs::write(&backup, b"preexisting backup").unwrap();
        let error = install_verified_migration(
            &db,
            &backup,
            &mut temp,
            bounded_regular_sha256(&db).unwrap(),
            || Ok(()),
            |from, to| std::fs::rename(from, to),
        )
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<MigrationInstallFailure>()
                .unwrap()
                .state,
            MigrationInstallState::NotPublished
        );
        assert!(error.to_string().contains("create_new refused"));
        assert_eq!(std::fs::read(&db).unwrap(), original);
        assert_eq!(std::fs::read(&backup).unwrap(), b"preexisting backup");
        #[cfg(unix)]
        assert_eq!(std::fs::metadata(&db).unwrap().nlink(), 1);
        let lease = mneme_store_path::StoreLease::acquire(&db).unwrap();
        assert_eq!(MemStore::load(&db).unwrap().dim(), 8);
        drop(lease);
        drop(temp);
        std::fs::remove_file(&backup).unwrap();
        std::fs::remove_file(&db).unwrap();
    }

    #[cfg(all(feature = "cozo", unix))]
    #[test]
    fn migration_sigkill_child() {
        let Ok(db) = std::env::var("MNEME_TEST_MIGRATION_CHILD_DB") else {
            return;
        };
        let db = PathBuf::from(db);
        let stage = PathBuf::from(std::env::var("MNEME_TEST_MIGRATION_CHILD_STAGE").unwrap());
        let backup = db.with_extension("snapshot.bak");
        let mut temp = MigrationTemp {
            path: Some(stage),
            private_dir: None,
        };
        let result = install_verified_migration(
            &db,
            &backup,
            &mut temp,
            bounded_regular_sha256(&db).unwrap(),
            || Ok(()),
            |from, to| std::fs::rename(from, to),
        );
        panic!("SIGKILL checkpoint failed to terminate child: {result:?}");
    }

    #[cfg(all(feature = "cozo", unix))]
    #[tokio::test]
    async fn migration_sigkill_cuts_keep_normal_source_admission() {
        use std::os::unix::process::ExitStatusExt;
        for point in [
            "after_backup_create",
            "after_backup_parent_sync",
            "after_final_rename",
        ] {
            let db = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("mnemed-kill-cut-{}.json", Ulid::new()));
            let backup = db.with_extension("snapshot.bak");
            let snapshot = MemStore::new(8);
            let expected = snapshot.export();
            snapshot.save(&db).unwrap();
            let original = std::fs::read(&db).unwrap();
            let stage = MigrationTemp::reserve_private_dir_beside(&db, "kill-cut").unwrap();
            let stage_path = stage.path().to_owned();
            CozoStore::materialize_fresh_current(&stage_path, Ulid::new(), &snapshot)
                .await
                .unwrap();
            let stage_lease = Arc::new(mneme_store_path::StoreLease::acquire(&stage_path).unwrap());
            let staged =
                CozoStore::open_existing_persistent(&stage_path, 8, stage_lease.clone()).unwrap();
            staged.verify_import(&expected).await.unwrap();
            staged.prepare_for_file_move().unwrap();
            drop(staged);
            drop(stage_lease);
            reject_sqlite_sidecars(&stage_path).unwrap();
            std::fs::File::open(&stage_path)
                .unwrap()
                .sync_all()
                .unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("embedding_identity_tests::migration_sigkill_child")
                .env("MNEME_TEST_MIGRATION_CHILD_DB", &db)
                .env("MNEME_TEST_MIGRATION_CHILD_STAGE", &stage_path)
                .env("MNEME_TEST_MIGRATION_KILL_AT", point)
                .status()
                .unwrap();
            assert_eq!(
                status.signal(),
                Some(libc::SIGKILL),
                "{point}: child did not die at cut"
            );
            assert!(backup.exists());
            let lease = Arc::new(mneme_store_path::StoreLease::acquire(&db).unwrap());
            assert_eq!(
                std::fs::metadata(&db).unwrap().nlink(),
                1,
                "{point}: selected source wedged"
            );
            if point == "after_final_rename" {
                assert_eq!(std::fs::read(&backup).unwrap(), original);
                let current = CozoStore::open_existing_persistent(&db, 8, lease.clone()).unwrap();
                current.verify_import(&expected).await.unwrap();
                current.prepare_for_file_move().unwrap();
                drop(current);
                assert!(!stage_path.exists());
            } else {
                assert_eq!(std::fs::read(&db).unwrap(), original);
                assert_eq!(MemStore::load(&db).unwrap().db_id(), snapshot.db_id());
                assert!(stage_path.exists());
                if point == "after_backup_create" {
                    assert_eq!(std::fs::metadata(&backup).unwrap().len(), 0);
                } else {
                    assert_eq!(std::fs::read(&backup).unwrap(), original);
                }
                let retry = run_migrate(&db, lease.clone())
                    .await
                    .unwrap_err()
                    .to_string();
                assert!(retry.contains("refusing to overwrite existing migration backup"));
            }
            drop(lease);
            drop(stage);
            std::fs::remove_file(&backup).unwrap();
            std::fs::remove_file(&db).unwrap();
        }
    }

    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn migration_postflight_failures_remain_typed_and_reopen_complete_source() {
        for point in [
            "before_postflight_open",
            "after_postflight_open",
            "before_postflight_verify",
            "after_postflight_verify",
            "before_postflight_checkpoint",
            "after_postflight_checkpoint",
        ] {
            let db = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("mnemed-postflight-{}.json", Ulid::new()));
            let backup = db.with_extension("snapshot.bak");
            let snapshot = MemStore::new(8);
            let expected = snapshot.export();
            snapshot.save(&db).unwrap();
            let old_bytes = std::fs::read(&db).unwrap();
            let lease = Arc::new(mneme_store_path::StoreLease::acquire(&db).unwrap());
            let seal = CurrentRebuildSeal::capture(&db, &expected, &lease).unwrap();
            MIGRATION_INSTALL_FAILURE.with(|slot| slot.set(Some(point)));
            let error = publish_detached_current(
                &db, &backup, &snapshot, &expected, &lease, &seal, "migrate",
            )
            .await
            .unwrap_err();
            MIGRATION_INSTALL_FAILURE.with(|slot| slot.set(None));
            let typed = error.downcast_ref::<MigrationInstallFailure>().unwrap();
            assert_eq!(
                typed.state,
                MigrationInstallState::InstalledPostflightFailure,
                "{point}: {error}"
            );
            assert!(error.to_string().contains(point));
            assert_eq!(std::fs::read(&backup).unwrap(), old_bytes);
            assert_eq!(MemStore::load(&backup).unwrap().db_id(), snapshot.db_id());
            #[cfg(unix)]
            assert_eq!(std::fs::metadata(&db).unwrap().nlink(), 1);
            drop(lease);
            let next_lease = Arc::new(mneme_store_path::StoreLease::acquire(&db).unwrap());
            let current = CozoStore::open_existing_persistent(&db, 8, next_lease.clone()).unwrap();
            current.verify_import(&expected).await.unwrap();
            current.prepare_for_file_move().unwrap();
            drop(current);
            let installed_bytes = std::fs::read(&db).unwrap();
            let retry = publish_detached_current(
                &db,
                &backup,
                &snapshot,
                &expected,
                &next_lease,
                &seal,
                "migrate",
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(retry.contains("refusing to overwrite existing migration backup"));
            assert_eq!(std::fs::read(&db).unwrap(), installed_bytes);
            assert_eq!(std::fs::read(&backup).unwrap(), old_bytes);
            drop(next_lease);
            std::fs::remove_file(&backup).unwrap();
            std::fs::remove_file(&db).unwrap();
        }
    }
}
