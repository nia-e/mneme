//! Native declarative selection and bounded bridge. Discovery may read explicitly
//! delegated owner catalogs; frozen replay never contacts a publisher. Neither
//! path opens storage, starts an owner, or interprets a remote path locally.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{AnyErr, Command, remote_commands};
use clap::{Args, Subcommand, ValueEnum};
use mneme_config::{Operation, SelectionSnapshot};
use mneme_store_client::discovery::{ResolutionOutcome, ResolveOptions};
use mneme_store_client::{ConfiguredClient, OperationBudget};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_REQUEST: usize = 128 * 1024;
const MAX_RESULT: usize = 512 * 1024;
// Explicit inspection can carry the separately bounded 1 MiB provenance report
// plus local-source and store-admission summaries. Ordinary bridge/selection
// output retains its smaller envelope and never embeds the report.
const MAX_INSPECTION_RESULT: usize = 2 * 1024 * 1024;
const DISCOVERY_TIMEOUT_MS: u64 = 15_000;
const DISCOVERY_NATIVE_BYTES: usize = 1024 * 1024;

#[derive(Args)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    action: ConfigAction,
}
impl ConfigArgs {
    pub(crate) fn is_call(&self) -> bool {
        matches!(self.action, ConfigAction::Call(_))
    }
}
#[derive(Subcommand)]
enum ConfigAction {
    /// Freeze store selection; opt-in discovery may fetch bounded owner catalogs.
    /// An expected digest requests offline replay instead of acquisition.
    Select(SelectArgs),
    /// Explain effective configuration, sources, conflicts and unavailable imports.
    /// Reports are useful even when no store can be activated.
    Inspect(InspectArgs),
    /// Make one checked native call under an exact selection digest. JSON stdin:
    /// {"tool":"get","arguments":{"id":"..."}}. Never retries uncertain writes.
    Call(CallArgs),
}
#[derive(Args)]
struct SelectionArgs {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    cwd: Option<PathBuf>,
}
#[derive(Args)]
struct DiscoveryArgs {
    /// Refresh delegated catalogs explicitly. Failed refresh retains the last
    /// usable receipt and reports stale coverage; it never deletes known stores.
    #[arg(long)]
    refresh: bool,
    /// Use only retained local metadata; never contact discovery publishers.
    #[arg(long, conflicts_with = "refresh")]
    offline: bool,
    /// Aggregate discovery deadline, including initialization and failed reads.
    #[arg(long, env = "MNEME_DISCOVERY_TIMEOUT_MS", default_value = "15000", value_parser = clap::value_parser!(u32).range(1..=30_000))]
    timeout_ms: u32,
    /// Aggregate decoded discovery bytes, including tool and identity catalogs.
    #[arg(long, default_value = "1048576", value_parser = clap::value_parser!(u32).range(1..=16_777_216))]
    max_native_bytes: u32,
}
#[derive(Args)]
struct SelectArgs {
    #[command(flatten)]
    selection: SelectionArgs,
    #[command(flatten)]
    discovery: DiscoveryArgs,
    /// Replay this exact selection offline. Never refreshes or adopts new grants.
    #[arg(long, conflicts_with = "refresh")]
    expected_selection_digest: Option<String>,
}
#[derive(Args)]
struct InspectArgs {
    #[command(flatten)]
    selection: SelectionArgs,
    #[command(flatten)]
    discovery: DiscoveryArgs,
}
#[derive(Clone, Copy, ValueEnum)]
enum BridgeOperation {
    Explicit,
    Core,
    Context,
    Recall,
    Record,
    Maintenance,
}
#[derive(Args)]
struct CallArgs {
    #[command(flatten)]
    selection: SelectionArgs,
    #[arg(long)]
    store: String,
    #[arg(long)]
    expected_selection_digest: String,
    #[arg(long, value_enum)]
    operation: BridgeOperation,
    #[arg(long, default_value = "30000", value_parser = clap::value_parser!(u32).range(1..=30_000))]
    timeout_ms: u32,
    /// Aggregate decoded native transport bytes, including catalogs and failures.
    #[arg(long, alias = "max-decoded-bytes", default_value = "524288", value_parser = clap::value_parser!(u32).range(1..=16_777_216))]
    max_native_bytes: u32,
    /// For callers unable to preserve immutable cache references. Record and
    /// maintenance always use the canonical owner regardless of this flag.
    #[arg(long)]
    canonical_only: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeRequest {
    tool: String,
    arguments: Value,
}

fn selection_paths(args: &SelectionArgs) -> Result<(Option<&Path>, PathBuf), AnyErr> {
    let cwd = args
        .cwd
        .clone()
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)?;
    // Omission must retain ordinary CLI/TUI discovery mode. Filling in the
    // default path here creates an explicit-selection boundary and refreshes
    // another receipt, leaving the ordinary command's stale catalog untouched.
    Ok((args.config.as_deref(), cwd))
}
fn replay(
    args: &SelectionArgs,
    profile: Option<&str>,
    digest: &str,
) -> Result<SelectionSnapshot, AnyErr> {
    let (config, cwd) = selection_paths(args)?;
    mneme_store_client::discovery::replay_workspace(config, &cwd, profile, digest)
        .map_err(|error| -> AnyErr { error })?
        .ok_or_else(|| {
            "configuration is unavailable; reselect and reassess before submitting".into()
        })
}
async fn resolve(
    args: &SelectionArgs,
    discovery: &DiscoveryArgs,
    profile: Option<&str>,
) -> Result<Option<ResolutionOutcome>, AnyErr> {
    let (config, cwd) = selection_paths(args)?;
    let budget = OperationBudget::new(
        discovery.max_native_bytes as usize,
        Duration::from_millis(discovery.timeout_ms.into()),
    )
    .map_err(|error| -> AnyErr { error })?;
    mneme_store_client::discovery::resolve_workspace(
        config,
        &cwd,
        profile,
        ResolveOptions {
            refresh: discovery.refresh,
            offline: discovery.offline,
        },
        &budget,
    )
    .await
    .map_err(|error| -> AnyErr { error })
}
pub(crate) async fn discover(
    path: Option<&Path>,
    profile: Option<&str>,
) -> Result<Option<SelectionSnapshot>, AnyErr> {
    let budget = OperationBudget::new(
        DISCOVERY_NATIVE_BYTES,
        Duration::from_millis(DISCOVERY_TIMEOUT_MS),
    )
    .map_err(|error| -> AnyErr { error })?;
    let outcome = mneme_store_client::discovery::resolve_workspace(
        path, &std::env::current_dir()?, profile, ResolveOptions::default(), &budget,
    ).await.map_err(|error| format!("shared configuration/profile selection refused: {error}; repair the workspace metadata or use an explicit configuration (no fallback attempted)"))?;
    let Some(outcome) = outcome else {
        return Ok(None);
    };
    outcome.selection.map(Some).ok_or_else(|| format!(
        "shared configuration selection unresolved: {}; use config inspect to review sources and conflicts (no fallback attempted)",
        outcome.error.as_deref().unwrap_or("discovery did not produce an admissible selection"),
    ).into())
}
pub(crate) fn independent(command: &Command) -> bool {
    matches!(
        command,
        Command::Activity(_)
            | Command::Config(_)
            | Command::Cache(_)
            | Command::Init(_)
            | Command::Client(_)
            | Command::Library(_)
            | Command::Tui(_)
            | Command::Stores(_)
            | Command::BootstrapInspect(_)
            | Command::BootstrapCreate(_)
            | Command::Demo { .. }
    )
}
pub(crate) fn render_selection(
    snapshot: &SelectionSnapshot,
    json_output: bool,
) -> Result<(), AnyErr> {
    if json_output {
        crate::print_json(&serde_json::to_value(snapshot)?);
    } else {
        println!("selection {}", snapshot.digest);
        for (name, store) in &snapshot.config.stores {
            let active = snapshot
                .active
                .iter()
                .any(|selected| selected.name == *name);
            println!(
                "{name}\t{}\t{}\tread={} write={}",
                store.id,
                if active { "active" } else { "inactive" },
                store.permissions.read,
                store.permissions.write
            );
        }
    }
    Ok(())
}

/// Inspection has a diagnostic success contract: a well-formed invocation can
/// describe an unavailable configuration without claiming selection succeeded.
/// In particular, an error is not an empty store list or permission to fall back.
fn inspection_value(outcome: Option<&ResolutionOutcome>, error: Option<&str>) -> Value {
    let selection = outcome.and_then(|outcome| outcome.selection.as_ref());
    let stores: Vec<_> = selection
        .into_iter()
        .flat_map(|selection| &selection.stores)
        .map(|row| {
            json!({
                "name": row.name,
                "id": row.store.id,
                "admitted": row.admitted,
                "active": row.active,
                "eligible": row.eligible,
                "reason": row.reason,
            })
        })
        .collect();
    json!({
        "schema": "mneme.config-inspection.v1",
        "configured": if error.is_some() { Value::Null } else { json!(outcome.is_some()) },
        "cache_status": outcome.map(|outcome| &outcome.cache_status),
        "selection_digest": selection.map(|selection| &selection.digest),
        "profile": selection.map(|selection| &selection.profile),
        "read_default": selection.and_then(|selection| selection.read_default.as_ref()),
        "sources": selection.map(|selection| &selection.sources),
        "stores": stores,
        "report": outcome.and_then(|outcome| outcome.report.as_ref()),
        "attempt": outcome.and_then(|outcome| outcome.attempt.as_ref()),
        "error": error.or_else(|| outcome.and_then(|outcome| outcome.error.as_deref())),
    })
}

fn report_value(value: Option<&Value>) -> String {
    value
        .map(ToString::to_string)
        .unwrap_or_else(|| "<not present in report>".into())
}

fn inspection_diagnostic_label(value: &Value) -> &'static str {
    if value["selection_digest"].is_string() {
        "warning"
    } else {
        "unavailable"
    }
}

fn render_inspection(value: &Value, json_output: bool) -> Result<(), AnyErr> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_INSPECTION_RESULT {
        return Err("configuration inspection exceeds its 2 MiB report envelope; reduce imported catalog/provenance size before retrying".into());
    }
    if json_output {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&bytes)?;
        stdout.write_all(b"\n")?;
    } else {
        println!("configuration inspection");
        if let Some(error) = value["error"].as_str() {
            println!("{}: {error}", inspection_diagnostic_label(value));
        }
        if value["configured"] == false {
            println!("No declarative configuration was selected.");
        }
        if let Some(cache) = value["cache_status"].as_str() {
            println!("catalogs: {cache}");
        }
        if let Some(digest) = value["selection_digest"].as_str() {
            println!("selection: {digest}");
        }
        if let Some(stores) = value["stores"].as_array() {
            for row in stores {
                println!(
                    "{}\t{}\tadmitted={} active={}{}",
                    row["name"].as_str().unwrap_or("?"),
                    row["id"].as_str().unwrap_or("?"),
                    row["admitted"],
                    row["active"],
                    row["reason"]
                        .as_str()
                        .map(|reason| format!(" — {reason}"))
                        .unwrap_or_default(),
                );
            }
        }
        if let Some(attempt) = value["attempt"].as_object() {
            if let Some(reason) = attempt.get("incomplete").and_then(Value::as_str) {
                println!("acquisition incomplete: {reason}");
            }
            if let Some(unavailable) = attempt.get("unavailable").and_then(Value::as_object) {
                for (publisher, reason) in unavailable {
                    println!(
                        "acquisition unavailable: {publisher}: {}",
                        reason.as_str().unwrap_or("unavailable")
                    );
                }
            }
        }
        if let Some(report) = value["report"].as_object() {
            println!("\nProvenance:");
            if let Some(stores) = report.get("stores").and_then(Value::as_object) {
                for (id, store) in stores {
                    println!(
                        "{} [{id}]{}",
                        store["label"].as_str().unwrap_or("unnamed"),
                        if store["blocked"] == true {
                            " — unresolved"
                        } else {
                            ""
                        }
                    );
                    if let Some(fields) = store["fields"].as_object() {
                        for (name, field) in fields {
                            let status = if field["local_override"] == true {
                                "local override"
                            } else if field["conflict"] == true {
                                "conflict"
                            } else {
                                "inherited/default"
                            };
                            println!(
                                "  {name} = {} ({status})",
                                report_value(field.get("effective"))
                            );
                            for (kind, key) in [("from", "contributors"), ("shadowed", "shadowed")]
                            {
                                if let Some(witnesses) = field[key].as_array() {
                                    for witness in witnesses {
                                        let branch = witness["witness"]
                                            .as_array()
                                            .into_iter()
                                            .flatten()
                                            .filter_map(Value::as_str)
                                            .collect::<Vec<_>>()
                                            .join(" -> ");
                                        let branch = if branch.is_empty() {
                                            "<branch not present in report>"
                                        } else {
                                            &branch
                                        };
                                        println!(
                                            "    {kind} {} via {branch}: {}",
                                            witness["publisher_db_id"].as_str().unwrap_or("?"),
                                            report_value(witness.get("value"))
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(conflicts) = report.get("conflicts").and_then(Value::as_array) {
                for conflict in conflicts {
                    println!(
                        "conflict: {} / {}: {}",
                        conflict["store_id"].as_str().unwrap_or("?"),
                        conflict["field"].as_str().unwrap_or("?"),
                        conflict["reason"].as_str().unwrap_or("unresolved")
                    );
                }
            }
            if let Some(unavailable) = report.get("unavailable").and_then(Value::as_object) {
                for (publisher, reason) in unavailable {
                    println!(
                        "unavailable: {publisher}: {}",
                        reason.as_str().unwrap_or("unavailable")
                    );
                }
            }
            if let Some(reason) = report.get("incomplete").and_then(Value::as_str) {
                println!("incomplete: {reason}");
            }
            if let Some(omitted) = report
                .get("omitted_details")
                .and_then(Value::as_u64)
                .filter(|count| *count > 0)
            {
                println!("{omitted} provenance details omitted by the report byte bound");
            }
        }
    }
    Ok(())
}
fn read_request(mut input: impl Read) -> Result<BridgeRequest, AnyErr> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take((MAX_REQUEST + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_REQUEST {
        return Err("config call input exceeds 128 KiB".into());
    }
    let request: BridgeRequest = serde_json::from_slice(&bytes)
        .map_err(|_| "config call expects one closed JSON request {tool,arguments}")?;
    if !request.arguments.is_object() || request.tool.is_empty() || request.tool.len() > 128 {
        return Err("config call requires a bounded tool name and object arguments".into());
    }
    Ok(request)
}
fn require_digest(snapshot: &SelectionSnapshot, expected: &str) -> Result<(), AnyErr> {
    if expected.len() != 64
        || !expected.bytes().all(|byte| byte.is_ascii_hexdigit())
        || snapshot.digest != expected
    {
        return Err("selection digest changed; select configuration again and reassess before submitting (no native operation sent)".into());
    }
    Ok(())
}
pub(crate) async fn run(
    args: &ConfigArgs,
    profile: Option<&str>,
    json_output: bool,
) -> Result<(), AnyErr> {
    match &args.action {
        ConfigAction::Select(args) => {
            let snapshot = if let Some(expected) = &args.expected_selection_digest {
                replay(&args.selection, profile, expected)?
            } else {
                let outcome = resolve(&args.selection, &args.discovery, profile)
                    .await?
                    .ok_or("configuration is unavailable; use config inspect for diagnostics")?;
                if args.discovery.refresh {
                    if let Some(error) = &outcome.error {
                        return Err(format!("{error}; use config inspect --offline to inspect the retained configuration").into());
                    }
                }
                outcome.selection.ok_or_else(|| {
                    outcome.error.unwrap_or_else(|| {
                        "configuration is unresolved; use config inspect to review conflicts".into()
                    })
                })?
            };
            render_selection(&snapshot, json_output)
        }
        ConfigAction::Inspect(args) => {
            let value = match resolve(&args.selection, &args.discovery, profile).await {
                Ok(outcome) => inspection_value(outcome.as_ref(), None),
                Err(error) => inspection_value(None, Some(&error.to_string())),
            };
            render_inspection(&value, json_output)
        }
        ConfigAction::Call(args) => {
            if !json_output {
                return Err("config call requires --json".into());
            }
            let result = match call(args, profile).await {
                Ok(result) => result,
                Err(error) => {
                    json!({"schema":"mneme.config-call.v1","selection_digest":args.expected_selection_digest,"store":args.store,"ok":false,"error":{"kind":"input","message":error.to_string()},"work":{"decoded_bytes":0,"native_seconds":0.0}})
                }
            };
            let bytes = serde_json::to_vec(&result)?;
            let bytes = if bytes.len() > MAX_RESULT {
                serde_json::to_vec(
                    &json!({"schema":"mneme.config-call.v1","selection_digest":args.expected_selection_digest,"store":args.store,"ok":false,"error":{"kind":"protocol","message":"result exceeds bridge output bound; submitted write outcome may be unknown, do not blindly retry"},"work":result["work"]}),
                )?
            } else {
                bytes
            };
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&bytes)?;
            stdout.write_all(b"\n")?;
            Ok(())
        }
    }
}

async fn call(args: &CallArgs, profile: Option<&str>) -> Result<Value, AnyErr> {
    let request = read_request(std::io::stdin().lock())?;
    let snapshot = replay(&args.selection, profile, &args.expected_selection_digest)?;
    require_digest(&snapshot, &args.expected_selection_digest)?;
    let access = match request.tool.as_str() {
        "save/verified" => {
            if request
                .arguments
                .as_object()
                .unwrap()
                .keys()
                .any(|key| !matches!(key.as_str(), "db" | "expected_db_id" | "payload"))
            {
                return Err(
                    "save/verified arguments allow only db, expected_db_id and payload".into(),
                );
            }
            let payload = request
                .arguments
                .get("payload")
                .ok_or("save/verified requires payload")?;
            mneme_app::save::PreparedSave::parse(payload, &ulid::Ulid::new().to_string())
                .map_err(|e| e.to_string())?;
            mneme_store_client::RequestAccess::Write
        }
        "owner_capabilities" => {
            if request
                .arguments
                .as_object()
                .is_none_or(|object| !object.is_empty())
            {
                return Err("owner_capabilities takes empty arguments".into());
            }
            mneme_store_client::RequestAccess::Read
        }
        _ => mneme_store_client::request_access(&request.tool, &request.arguments)
            .map_err(|e| e.to_string())?,
    };
    let write = access == mneme_store_client::RequestAccess::Write;
    if write {
        let supported = match args.operation {
            BridgeOperation::Explicit => true,
            BridgeOperation::Record => {
                matches!(request.tool.as_str(), "save/verified" | "save" | "capture")
                    || (request.tool == "episode" && request.arguments["action"] == "append")
            }
            BridgeOperation::Maintenance => request.tool == "retag",
            BridgeOperation::Core | BridgeOperation::Context | BridgeOperation::Recall => false,
        };
        if !supported {
            return Err("mutation is outside the declared background operation contract".into());
        }
    }
    let canonical_only = args.canonical_only
        || matches!(
            args.operation,
            BridgeOperation::Record | BridgeOperation::Maintenance
        );
    if canonical_only && request.arguments.get("_mneme_source").is_some() {
        return Err("cached references cannot be consumed by canonical-only/background write-dependent operations".into());
    }

    let operation = match (args.operation, write) {
        (BridgeOperation::Explicit, false) => Operation::ExplicitRead,
        (BridgeOperation::Explicit, true) => Operation::ExplicitWrite,
        (BridgeOperation::Core, false) => Operation::Core,
        (BridgeOperation::Context, false) => Operation::Context,
        (BridgeOperation::Recall, false) => Operation::Recall,
        (BridgeOperation::Record, _) => Operation::Record,
        (BridgeOperation::Maintenance, _) => Operation::Maintenance,
        _ => return Err("tool access does not match the declared bridge operation".into()),
    };
    let selected = snapshot.explicit_store(&args.store)?;
    let metadata = matches!(request.tool.as_str(), "databases" | "owner_capabilities");
    if metadata
        && matches!(args.operation, BridgeOperation::Explicit)
        && !selected.eligible.read
        && selected.eligible.write
    {
        selected.require_operation(Operation::ExplicitWrite)?;
    } else {
        selected.require_operation(operation)?;
    }
    if !write && !(metadata && selected.eligible.write) {
        selected.require_operation(Operation::ExplicitRead)?;
    }
    // Verified SAVE includes exact readback, so deny before any contact
    // when the configured store grants writes but not reads.
    if request.tool == "save/verified" {
        selected.require_operation(Operation::ExplicitRead)?;
    }
    if request
        .arguments
        .get("db")
        .is_some_and(|db| db.as_str() != Some(selected.store.owner.db.as_str()))
        || request
            .arguments
            .get("expected_db_id")
            .is_some_and(|id| id.as_str() != Some(selected.store.id.as_str()))
    {
        return Err("bridge request identity disagrees with selected store".into());
    }
    let mut arguments = request.arguments;
    if request.tool != "owner_capabilities" {
        arguments["db"] = json!(selected.store.owner.db);
        arguments["expected_db_id"] = json!(selected.store.id);
    }
    let digest = snapshot.digest.clone();
    let client = if matches!(args.operation, BridgeOperation::Explicit) {
        ConfiguredClient::for_explicit(snapshot, &args.store)
    } else {
        ConfiguredClient::new(snapshot)
    }
    .map_err(|e| e.to_string())?;
    let mut budget = OperationBudget::new(
        args.max_native_bytes as usize,
        Duration::from_millis(args.timeout_ms.into()),
    )
    .map_err(|e| e.to_string())?;
    let outcome = match request.tool.as_str() {
                "save/verified" => crate::cli_client::verified_configured_save(&arguments, &client, &args.store, &mut budget).await,
                "owner_capabilities" => {
                    client.catalog_owner_with_budget(&args.store, &mut budget).await
                        .map(|catalog| {
                            let features = mneme_mcp_client::CatalogView(&catalog);
                            json!({"retag_content_guards":features.supports_retag_content_guards(),"tag_vocabulary":features.supports_list_tags()})
                        }).map_err(|error| json!({"kind":"native","message":error.to_string()}))
                }
                _ => {
                    arguments["db"] = json!(args.store);
                    if canonical_only { client.call_owner_with_budget(&args.store, &request.tool, arguments, &mut budget).await }
                    else { client.call_with_budget(&args.store, &request.tool, arguments, &mut budget).await }
                }.map_err(|e| json!({"kind":"native","message":e.to_string()})),
            };
    let mut result = json!({"schema":"mneme.config-call.v1","selection_digest":digest,"store":args.store,"work":{"decoded_bytes":budget.bytes_used(),"native_seconds":budget.elapsed().as_secs_f64()}});
    match outcome {
        Ok(value) => {
            result["ok"] = json!(true);
            result["result"] = value;
        }
        Err(error) => {
            result["ok"] = json!(false);
            result["error"] = error;
        }
    }
    Ok(result)
}

pub(crate) async fn run_command(
    command: &Command,
    snapshot: SelectionSnapshot,
    named: Option<&str>,
    user: bool,
    json_output: bool,
) -> Result<(), AnyErr> {
    if user {
        return Err("--user is a legacy owner selector; declarative configuration has no name-based global role. Use --store NAME".into());
    }
    let name = named
        .or(snapshot.read_default.as_deref())
        .ok_or(
            "no admitted default store; inspect config select, or select an admitted --store NAME",
        )?
        .to_owned();
    if let Command::Repl(args) = command {
        return crate::remote_repl::run_configured(
            snapshot,
            &name,
            json_output,
            &args.start,
            args.budget,
            args.query.as_deref(),
        )
        .await;
    }
    let selected = snapshot.explicit_store(&name)?;
    let store = &snapshot.config.stores[&name];
    let database = store.owner.db.clone();
    let expected = store.id.parse::<ulid::Ulid>()?;
    let mut request = remote_commands::prepare(command, &database)?;
    selected.require_operation(match request.access() {
        remote_commands::RequestAccess::ReadOnly => Operation::ExplicitRead,
        remote_commands::RequestAccess::Mutation => Operation::ExplicitWrite,
    })?;
    if request
        .arguments()
        .get("expected_db_id")
        .is_some_and(|id| id.as_str() != Some(store.id.as_str()))
    {
        return Err("request database identity disagrees with selected store".into());
    }
    request.guard_database(expected);
    request.route_to(&name);
    let client = ConfiguredClient::for_explicit(snapshot, &name).map_err(|e| e.to_string())?;
    let tool = request.tool();
    let (arguments, response_check) = request.into_submission()?;
    let outcome = client.call(&name, tool, arguments).await;
    client.close().await;
    let result = outcome.map_err(|e| e.to_string())?;
    response_check.verify_configured(&result, &database, expected)?;
    remote_commands::render(command, &result, json_output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    fn snapshot() -> SelectionSnapshot {
        let config = mneme_config::Config::parse(
            r#"
schema = "mneme.config.v1"
read_default = "renamed"
[stores.renamed]
id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
owner = { url = "http://127.0.0.1:1", db = "native-alias" }
active = { workspace = "any" }
permissions = { read = true, write = false }
"#,
        )
        .unwrap();
        mneme_config::select(
            &config,
            &mneme_config::SelectionContext::new(std::env::temp_dir().canonicalize().unwrap()),
        )
        .unwrap()
    }
    #[test]
    fn inspection_distinguishes_missing_detail_clear_and_retained_warning() {
        assert_eq!(report_value(None), "<not present in report>");
        assert_eq!(report_value(Some(&Value::Null)), "null");
        assert_eq!(
            inspection_diagnostic_label(&json!({"selection_digest": "retained"})),
            "warning"
        );
        assert_eq!(
            inspection_diagnostic_label(&json!({"selection_digest": null})),
            "unavailable"
        );
    }
    #[test]
    fn stale_selection_refuses_before_native_client_creation() {
        let snapshot = snapshot();
        assert!(require_digest(&snapshot, &snapshot.digest).is_ok());
        assert!(require_digest(&snapshot, &"0".repeat(64)).is_err());
        assert!(require_digest(&snapshot, "").is_err());
    }
    #[tokio::test]
    async fn denied_cli_mutation_never_contacts_its_unreachable_owner() {
        let parsed = crate::Cli::try_parse_from(["mnemed", "save", "hello"]).unwrap();
        let error = run_command(&parsed.command, snapshot(), None, false, true)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("operation not granted"));
    }
    #[test]
    fn bounded_closed_bridge_input() {
        assert!(read_request(&b"{\"tool\":\"save\",\"arguments\":{},\"extra\":true}"[..]).is_err());
        assert!(read_request(&b"{\"tool\":\"get\",\"arguments\":[]}"[..]).is_err());
        assert!(read_request(&vec![b' '; MAX_REQUEST + 1][..]).is_err());
        assert!(read_request(&b"{\"tool\":\"get\",\"arguments\":{}}"[..]).is_ok());
    }
    #[test]
    fn selectors_cannot_mix_offline_and_declarative_authority() {
        for args in [
            vec!["mnemed", "--db", "x.db", "--store", "project", "status"],
            vec![
                "mnemed",
                "--remote",
                "http://127.0.0.1:1",
                "--memory-config",
                "x.toml",
                "status",
            ],
        ] {
            assert!(crate::Cli::try_parse_from(args).is_err());
        }
        assert!(
            crate::Cli::try_parse_from([
                "mnemed", "--json", "config", "select", "--config", "x.toml", "--cwd", "/tmp"
            ])
            .is_ok()
        );
    }
}
