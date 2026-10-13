//! Native declarative selection and bounded bridge. No metadata operation opens
//! storage, starts an owner, or interprets a configured remote path locally.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{AnyErr, Command, remote_commands};
use clap::{Args, Subcommand, ValueEnum};
use mneme_config::{Operation, SelectionSnapshot};
use mneme_store_client::{ConfiguredClient, OperationBudget};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_REQUEST: usize = 128 * 1024;
const MAX_RESULT: usize = 512 * 1024;

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
    /// Freeze metadata-only store selection; never contacts or starts an owner.
    Select(SelectionArgs),
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

fn select(args: &SelectionArgs, profile: Option<&str>) -> Result<SelectionSnapshot, AnyErr> {
    let cwd = args
        .cwd
        .clone()
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)?;
    let config = args
        .config
        .clone()
        .map(Ok)
        .unwrap_or_else(mneme_config::default_user_path)?;
    Ok(mneme_config::select_workspace(&config, &cwd, profile)?)
}
pub(crate) fn discover(
    path: Option<&Path>,
    profile: Option<&str>,
) -> Result<Option<SelectionSnapshot>, AnyErr> {
    mneme_config::discover_workspace(path, &std::env::current_dir()?, profile)
        .map_err(|error| format!("shared configuration/profile selection refused: {error}; repair the workspace metadata or use an explicit configuration (no fallback attempted)").into())
}
pub(crate) fn independent(command: &Command) -> bool {
    matches!(
        command,
        Command::Config(_)
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
        ConfigAction::Select(args) => render_selection(&select(args, profile)?, json_output),
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
    let snapshot = select(&args.selection, profile)?;
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
    selected.require_operation(match request.access {
        remote_commands::RequestAccess::ReadOnly => Operation::ExplicitRead,
        remote_commands::RequestAccess::Mutation => Operation::ExplicitWrite,
    })?;
    if request
        .arguments
        .get("expected_db_id")
        .is_some_and(|id| id.as_str() != Some(store.id.as_str()))
    {
        return Err("request database identity disagrees with selected store".into());
    }
    request.arguments["expected_db_id"] = json!(store.id);
    request.arguments["db"] = json!(name);
    let client = ConfiguredClient::for_explicit(snapshot, &name).map_err(|e| e.to_string())?;
    let outcome = client.call(&name, request.tool, request.arguments).await;
    client.close().await;
    let result = outcome.map_err(|e| e.to_string())?;
    if let Some(save) = &request.save {
        save.verify_write_receipt_json(&result)
            .map_err(|e| e.to_string())?;
    }
    if let Some(prepared) = &request.concern {
        prepared
            .request
            .validate_routed_response_json(&result, &database, Some(expected))
            .map_err(|e| e.to_string())?;
    }
    if let Some(prepared) = &request.retag {
        prepared
            .request
            .validate_routed_response_json(&result, &database, Some(expected))
            .map_err(|e| e.to_string())?;
    }
    if let Some(prepared) = &request.edit_body {
        prepared
            .request
            .validate_routed_response_json(&result, &database, Some(expected))
            .map_err(|e| e.to_string())?;
    }
    if let Some(prepared) = &request.edit_summary {
        prepared
            .request
            .validate_routed_response_json(&result, &database, Some(expected))
            .map_err(|e| e.to_string())?;
    }
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
