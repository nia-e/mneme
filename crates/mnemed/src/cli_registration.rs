//! Post-setup, add-only discovery publication. A failed hub never rolls back a
//! valid project owner, and ordinary reads/capture-init never enter this path.
use crate::{AnyErr, cli_init::SetupResult};
use clap::Args;
use mneme_app::discovery_registration::{
    RegistrationRequest, RegistrationResult as RegistrationReceipt, RegistrationStatus,
};
use mneme_config::{
    ConfigDeclarations, Owner, OwnerDeclaration, SetupDiscoveryHub, SetupDiscoveryMode,
    StoreDeclaration, WorkspaceKind,
};
use mneme_mcp_client::{ClientTimeouts, ConnectionOptions, ReceiveBudget, RemoteClient};
use serde::Serialize;
use serde_json::{Value, json};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::Instant;

const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(30);
const REGISTRATION_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 4096;
const RETRY: &str = "Project setup is intact. Resolve the reported hub/configuration issue, then rerun mnemed init --register with the same --root (and overrides); exact existing registrations are idempotent.";

#[derive(Args, Debug, Default)]
pub(crate) struct RegistrationArgs {
    /// Register this ready project at configured discovery hubs, including on retry.
    #[arg(long, conflicts_with_all = ["no_register", "interactive"])]
    register: bool,
    /// Skip discovery registration, overriding local setup defaults.
    #[arg(long, conflicts_with = "interactive")]
    no_register: bool,
    /// Ask for missing discovery settings and confirm publication (terminal only).
    #[arg(long)]
    interactive: bool,
    /// Local discovery-hub store label; repeat to replace the configured target list.
    #[arg(long, value_name = "NAME", value_parser = bounded_hub)]
    discovery_hub: Vec<String>,
    /// Reachable shared owner URL; {port} expands to this project's MCP port.
    #[arg(long, value_name = "ROUTE", value_parser = bounded_route)]
    advertise_url: Option<String>,
}
fn bounded_hub(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err("hub name must contain 1..=128 ASCII letters, digits, _, . or -".into());
    }
    Ok(value.into())
}
fn bounded_route(value: &str) -> Result<String, String> {
    mneme_config::render_setup_advertise_url(value, 1).map_err(|e| e.to_string())?;
    Ok(value.into())
}

#[derive(Debug)]
struct Preferences {
    mode: SetupDiscoveryMode,
    hubs: Vec<String>,
    advertise_url: Option<String>,
    device: Option<String>,
}
impl Preferences {
    fn resolve(local: &ConfigDeclarations, args: &RegistrationArgs) -> Self {
        let configured = local.setup.as_ref().and_then(|s| s.discovery.as_ref());
        Self {
            mode: if args.no_register {
                SetupDiscoveryMode::Off
            } else if args.register {
                SetupDiscoveryMode::Auto
            } else if args.interactive {
                SetupDiscoveryMode::Ask
            } else {
                configured.map(|s| s.mode()).unwrap_or_default()
            },
            hubs: if args.discovery_hub.is_empty() {
                configured.and_then(|s| s.hubs.clone()).unwrap_or_default()
            } else {
                args.discovery_hub.clone()
            },
            advertise_url: args
                .advertise_url
                .clone()
                .or_else(|| configured.and_then(|s| s.advertise_url.clone())),
            device: local.device.clone(),
        }
    }
}

#[derive(Debug)]
struct RegistrationPlan {
    label: String,
    store: StoreDeclaration,
    hubs: Vec<SetupDiscoveryHub>,
}

#[derive(Debug, Serialize)]
struct HubOutcome {
    hub: String,
    publisher_db_id: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt: Option<RegistrationReceipt>,
}
#[derive(Debug, Serialize)]
struct RegistrationReport {
    published: bool,
    schema: &'static str,
    status: &'static str,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<String>,
    hubs: Vec<HubOutcome>,
    bytes_received: usize,
}
impl RegistrationReport {
    fn deferred(summary: impl Into<String>) -> Self {
        Self {
            published: false,
            schema: "mneme.setup-registration.v1",
            status: "deferred",
            summary: summary.into(),
            recovery: Some(RETRY.into()),
            hubs: vec![],
            bytes_received: 0,
        }
    }
    fn off() -> Self {
        Self {
            published: false,
            schema: "mneme.setup-registration.v1",
            status: "off",
            summary: "Discovery registration is off; no hubs contacted.".into(),
            recovery: None,
            hubs: vec![],
            bytes_received: 0,
        }
    }
}

fn field<'a>(result: &'a SetupResult, key: &str) -> Result<&'a str, AnyErr> {
    result
        .extra
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!("native setup result omitted {key}; rerun init before registration").into()
        })
}

/// Strip host-local routing and scope from the shared base. Sparse non-location
/// policy remains sparse; the local root belongs only to its named device.
fn publication(
    source: &StoreDeclaration,
    owner: &Owner,
    project_root: &Path,
    device: &str,
    advertised: &str,
) -> Result<StoreDeclaration, AnyErr> {
    let endpoint = url::Url::parse(&owner.url)?;
    if endpoint.scheme() != "http"
        || !matches!(endpoint.host(), Some(url::Host::Ipv4(host)) if host.is_loopback())
        || endpoint.path() != "/"
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || owner.token_env.is_some()
        || owner.ssh_mcp_port.is_some()
    {
        return Err("ready project owner is not the expected unauthenticated loopback HTTP listener; inspect the native setup binding before registering".into());
    }
    let port = endpoint
        .port_or_known_default()
        .ok_or("ready project owner has no MCP port")?;
    let shared = mneme_config::setup_advertised_owner(advertised, &owner.db, port)?;
    let mut store = source.clone();
    store.owner = Some(OwnerDeclaration {
        url: Some(shared.url),
        db: Some(shared.db),
        ssh_mcp_port: shared.ssh_mcp_port,
        ..Default::default()
    });
    // Local library labels and arbitrary device routes are not a portable export.
    store.cache = None;
    store.devices.clear();
    let mut local_policy = source.devices.get(device).cloned().unwrap_or_default();
    local_policy.owner = Some(Owner {
        path: None,
        token_env: None,
        ..owner.clone()
    });
    local_policy.cache = None;
    let mut local_active = local_policy.active.take().unwrap_or_default();
    local_active.roots = Some(vec![project_root.to_owned()]);
    if let Some(active) = &store.active {
        if local_active.subtree_roots.is_none() {
            local_active.subtree_roots = active.subtree_roots.clone();
        }
        if local_active.excluded_roots.is_none() {
            local_active.excluded_roots = active.excluded_roots.clone();
        }
    }
    local_policy.active = Some(local_active);
    local_policy.applicability = merge_applicability(
        store.applicability.take(),
        local_policy.applicability.take(),
    );
    let active = store.active.get_or_insert_default();
    active.workspace = Some(WorkspaceKind::Project);
    active.roots = None;
    active.subtree_roots = None;
    active.excluded_roots = None;
    // Clears targeting moved fields belong to neither the new shared base nor
    // an unrelated local cache. Retain clears of portable non-location policy.
    let portable_clear = |field: &str| {
        !field.starts_with("active.roots")
            && !field.starts_with("active.subtree_roots")
            && !field.starts_with("active.excluded_roots")
            && !field.starts_with("applicability")
            && !field.starts_with("owner")
            && field != "cache"
    };
    store.clear.retain(|s| portable_clear(s));
    local_policy.clear.retain(|s| portable_clear(s));
    store.devices.insert(device.into(), local_policy);
    store.validate(true)?;
    Ok(store)
}

fn merge_scope(
    base: Option<mneme_config::OperationScopeDeclaration>,
    overlay: Option<mneme_config::OperationScopeDeclaration>,
) -> Option<mneme_config::OperationScopeDeclaration> {
    match (base, overlay) {
        (base, None) => base,
        (None, overlay) => overlay,
        (Some(base), Some(mut overlay)) => {
            overlay.roots = overlay.roots.or(base.roots);
            overlay.subtree_roots = overlay.subtree_roots.or(base.subtree_roots);
            overlay.excluded_roots = overlay.excluded_roots.or(base.excluded_roots);
            Some(overlay)
        }
    }
}
fn merge_applicability(
    base: Option<mneme_config::ApplicabilityDeclaration>,
    overlay: Option<mneme_config::ApplicabilityDeclaration>,
) -> Option<mneme_config::ApplicabilityDeclaration> {
    match (base, overlay) {
        (base, None) => base,
        (None, overlay) => overlay,
        (Some(base), Some(mut overlay)) => {
            overlay.core = merge_scope(base.core, overlay.core);
            overlay.context = merge_scope(base.context, overlay.context);
            overlay.recall = merge_scope(base.recall, overlay.recall);
            overlay.record = merge_scope(base.record, overlay.record);
            overlay.maintenance = merge_scope(base.maintenance, overlay.maintenance);
            Some(overlay)
        }
    }
}

fn make_plan(
    local: &ConfigDeclarations,
    snapshot: &mneme_config::SelectionSnapshot,
    db_id: &str,
    owner_url: &str,
    root: &Path,
    preferences: &Preferences,
) -> Result<RegistrationPlan, AnyErr> {
    if preferences.hubs.is_empty() {
        return Err(
            "choose discovery hubs in [setup.discovery].hubs or with --discovery-hub NAME".into(),
        );
    }
    let advertised = preferences.advertise_url.as_deref().ok_or("set [setup.discovery].advertise_url or --advertise-url to a reachable SSH/HTTP(S) owner route; the local loopback URL is never published as the shared endpoint")?;
    let device = preferences.device.as_deref().ok_or("set a top-level device name in memory.toml (or use init --interactive) so this project's local route and root are device-scoped")?;
    let rows: Vec<_> = snapshot
        .stores
        .iter()
        .filter(|row| row.store.id == db_id)
        .collect();
    if rows.len() != 1 {
        return Err("final setup identity has no unique selection row; rerun init after repairing local declarations".into());
    }
    let row = rows[0];
    if row.store.owner.url != owner_url {
        return Err("ready owner route changed after setup; rerun init before registration".into());
    }
    let sources: Vec<_> = local
        .stores
        .values()
        .filter(|store| store.id == db_id)
        .collect();
    if sources.len() != 1 {
        return Err("ready project has no unique local declaration; imported stores are not implicitly re-published".into());
    }
    let hubs = local.setup_discovery_hubs(&preferences.hubs)?;
    if hubs.iter().any(|hub| hub.id == db_id) {
        return Err(
            "a new project cannot register itself as its discovery hub; select an existing hub"
                .into(),
        );
    }
    let store = publication(sources[0], &row.store.owner, root, device, advertised)?;
    RegistrationRequest {
        label: row.name.clone(),
        store: store.clone(),
        expected_catalog_sha256: None,
    }
    .validate()?;
    Ok(RegistrationPlan {
        label: row.name.clone(),
        store,
        hubs,
    })
}

fn prompt(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    question: &str,
) -> Result<Option<String>, AnyErr> {
    write!(writer, "{question} ")?;
    writer.flush()?;
    let mut raw = Vec::new();
    // A terminal response has a finite byte budget, but deliberately no timer.
    let count = std::io::Read::take(&mut *reader, (MAX_PROMPT_BYTES + 1) as u64)
        .read_until(b'\n', &mut raw)?;
    if count > MAX_PROMPT_BYTES {
        return Err("interactive answer exceeds 4096 bytes; retry with bounded settings".into());
    }
    if count == 0 {
        return Ok(None);
    }
    let value = std::str::from_utf8(&raw)?.trim();
    Ok((!value.is_empty()).then(|| value.to_owned()))
}
fn complete_preferences(
    preferences: &mut Preferences,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<bool, AnyErr> {
    if preferences.hubs.is_empty() {
        let Some(value) = prompt(
            reader,
            writer,
            "Discovery hub local names (comma-separated; blank skips):",
        )?
        else {
            return Ok(false);
        };
        preferences.hubs = value
            .split(',')
            .map(str::trim)
            .map(bounded_hub)
            .collect::<Result<_, _>>()?;
    }
    if preferences.advertise_url.is_none() {
        let Some(value) = prompt(
            reader,
            writer,
            "Reachable owner URL (e.g. ssh://you@this-host; blank skips):",
        )?
        else {
            return Ok(false);
        };
        preferences.advertise_url = Some(bounded_route(&value)?);
    }
    if preferences.device.is_none() {
        let Some(value) = prompt(
            reader,
            writer,
            "This device's name for local root mapping (not saved; blank skips):",
        )?
        else {
            return Ok(false);
        };
        preferences.device = Some(bounded_hub(&value)?);
    }
    Ok(true)
}
fn can_prompt(json_output: bool, stdin_terminal: bool, stderr_terminal: bool) -> bool {
    !json_output && stdin_terminal && stderr_terminal
}

async fn call_hub(
    hub: SetupDiscoveryHub,
    label: String,
    store: StoreDeclaration,
    budget: ReceiveBudget,
    deadline: Instant,
) -> HubOutcome {
    let fail = |status, detail: String| HubOutcome {
        hub: hub.label.clone(),
        publisher_db_id: hub.id.clone(),
        status,
        detail: Some(detail),
        receipt: None,
    };
    let work_deadline = deadline - Duration::from_millis(250);
    let remaining = work_deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() || budget.remaining() == 0 {
        return fail(
            "pending",
            "Aggregate registration time/byte budget exhausted; hub not attempted.".into(),
        );
    }
    let options = ConnectionOptions {
        url: hub.owner.url.clone(),
        ssh_mcp_port: hub.owner.ssh_mcp_port.unwrap_or(0),
        token_env: hub.owner.token_env.clone(),
    };
    let mut client = match tokio::time::timeout_at(
        work_deadline,
        RemoteClient::connect_owner_with_budget(
            &options,
            ClientTimeouts {
                connect: remaining,
                request: remaining,
            },
            budget,
        ),
    )
    .await
    {
        Ok(Ok(client)) => client,
        Ok(Err(error)) => return fail("failed", bounded_error(&error.to_string())),
        Err(_) => {
            return fail(
                "pending",
                "Aggregate registration deadline reached while connecting; retry init.".into(),
            );
        }
    };
    let response = tokio::time::timeout_at(
        work_deadline,
        client.call_tool(
            "discovery_register",
            json!({"db":hub.owner.db,"expected_db_id":hub.id,"label":label,"store":store}),
        ),
    )
    .await;
    let _ = tokio::time::timeout_at(deadline, client.close()).await;
    match response {
        Ok(Ok(value)) => match serde_json::from_value::<RegistrationReceipt>(value) {
            Ok(receipt) if receipt.publisher_db_id == hub.id && match receipt.status {
RegistrationStatus::Added => receipt.label == label,
RegistrationStatus::AlreadyPresent => bounded_hub(&receipt.label).is_ok(),
}
                && valid_digest(&receipt.before_catalog_sha256) && valid_digest(&receipt.catalog_sha256) => {
                let status = if !receipt.durable { "pending" } else { match receipt.status { RegistrationStatus::Added => "added", RegistrationStatus::AlreadyPresent => "already_present" } };
                HubOutcome { hub: hub.label, publisher_db_id: hub.id, status, detail: None, receipt: Some(receipt) }
            },
            _ => fail("pending", "Hub returned an invalid/mismatched registration receipt; outcome uncertain, retry init safely.".into()),
        },
        Ok(Err(error)) => {
            if mneme_mcp_client::classify_error(error.as_ref()) == mneme_mcp_client::ClientErrorClass::Tool {
                fail("failed", bounded_error(&error.to_string()))
            } else {
                fail("pending", format!("{}; no confirmed receipt: the write may have completed, retry init safely.", bounded_error(&error.to_string())))
            }
        },
        Err(_) => fail("pending", "Aggregate registration deadline reached; write may have completed, retry init safely.".into()),
    }
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn bounded_error(value: &str) -> String {
    value.chars().take(2048).collect()
}

async fn execute(plan: RegistrationPlan) -> RegistrationReport {
    let receive = ReceiveBudget::new(REGISTRATION_BYTES);
    let deadline = Instant::now() + REGISTRATION_TIMEOUT;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
    let mut tasks = tokio::task::JoinSet::new();
    for (index, hub) in plan.hubs.into_iter().enumerate() {
        let (label, store, budget, permits) = (
            plan.label.clone(),
            plan.store.clone(),
            receive.clone(),
            semaphore.clone(),
        );
        tasks.spawn(async move {
            let _permit = permits
                .acquire_owned()
                .await
                .expect("registration semaphore remains open");
            (index, call_hub(hub, label, store, budget, deadline).await)
        });
    }
    let mut results = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(outcome) => results.push(outcome),
            Err(_) => results.push((
                usize::MAX,
                HubOutcome {
                    hub: "unknown".into(),
                    publisher_db_id: "unknown".into(),
                    status: "pending",
                    detail: Some("Registration task failed; retry init safely.".into()),
                    receipt: None,
                },
            )),
        }
    }
    results.sort_by_key(|(index, _)| *index);
    let hubs: Vec<_> = results.into_iter().map(|(_, result)| result).collect();
    let completed = hubs
        .iter()
        .filter(|h| matches!(h.status, "added" | "already_present"))
        .count();
    let all = completed == hubs.len();
    RegistrationReport {
        published: hubs.iter().any(|hub| hub.receipt.is_some()),
        schema: "mneme.setup-registration.v1",
        status: if all {
            "complete"
        } else if completed > 0 {
            "partial"
        } else {
            "pending"
        },
        summary: format!(
            "Discovery registration: {completed}/{} hubs confirmed; project setup remains ready.",
            hubs.len()
        ),
        recovery: (!all).then(|| RETRY.into()),
        hubs,
        bytes_received: receive.used(),
    }
}

async fn prepare_and_register(
    args: &RegistrationArgs,
    result: &SetupResult,
    json_output: bool,
) -> Result<RegistrationReport, AnyErr> {
    if args.no_register {
        return Ok(RegistrationReport::off());
    }
    let root = PathBuf::from(field(result, "project_root")?);
    let config = PathBuf::from(field(result, "memory_config")?);
    let local = mneme_config::prepare_workspace(Some(&config), &root, None)?
        .ok_or("native setup configuration disappeared")?
        .local;
    let mut preferences = Preferences::resolve(&local, args);
    if preferences.mode == SetupDiscoveryMode::Off {
        return Ok(RegistrationReport::off());
    }
    let interactive = preferences.mode == SetupDiscoveryMode::Ask;
    if interactive
        && !can_prompt(
            json_output,
            std::io::stdin().is_terminal(),
            std::io::stderr().is_terminal(),
        )
    {
        return Ok(RegistrationReport::deferred(
            "Discovery registration awaits confirmation; JSON/non-terminal init never prompts. Use init --interactive in a terminal, or --register with configured settings.",
        ));
    }
    if interactive
        && !complete_preferences(
            &mut preferences,
            &mut std::io::stdin().lock(),
            &mut std::io::stderr().lock(),
        )?
    {
        return Ok(RegistrationReport::deferred(
            "Discovery registration skipped at the prompt; no hubs contacted.",
        ));
    }
    // Replay the helper's EXPLICIT final request. A discover-mode request has a
    // different digest; a lifecycle word such as `adopted` is not a store alias.
    let snapshot = mneme_store_client::discovery::replay_workspace(
        Some(&config),
        &root,
        None,
        field(result, "selection_digest")?,
    )
    .map_err(|e| -> AnyErr { e })?
    .ok_or("final native setup selection is unavailable")?;
    let plan = make_plan(
        &local,
        &snapshot,
        field(result, "db_id")?,
        field(result, "owner_url")?,
        &root,
        &preferences,
    )?;
    if interactive {
        eprintln!(
            "Register {} ({}) at hubs [{}] using {}?",
            plan.label,
            plan.store.id,
            preferences.hubs.join(", "),
            plan.store
                .owner
                .as_ref()
                .and_then(|o| o.url.as_deref())
                .unwrap_or("unknown")
        );
        eprintln!(
            "Settings entered here are not saved; configure [setup.discovery] once to reuse them."
        );
        let answer = prompt(
            &mut std::io::stdin().lock(),
            &mut std::io::stderr().lock(),
            "Publish this discovery declaration? [y/N]",
        )?;
        if !answer.is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "y" | "yes")) {
            return Ok(RegistrationReport::deferred(
                "Discovery registration was not confirmed; no hubs contacted.",
            ));
        }
        // Human consideration can take arbitrarily long. Recheck the same
        // exact native selection before the first remote mutation.
        mneme_store_client::discovery::replay_workspace(
            Some(&config),
            &root,
            None,
            &snapshot.digest,
        )
        .map_err(|e| -> AnyErr { e })?
        .ok_or("native selection disappeared during confirmation")?;
    }
    Ok(execute(plan).await)
}

pub(crate) async fn after_init(
    args: &RegistrationArgs,
    result: &mut SetupResult,
    json_output: bool,
) {
    if result.extra.get("integration").and_then(Value::as_str) != Some("native") {
        if args.register || args.interactive {
            result.extra.insert("discovery_registration".into(), json!(RegistrationReport::deferred("Discovery registration requires native memory.toml setup; legacy setup remains unchanged.")));
        }
        return;
    }
    let report = prepare_and_register(args, result, json_output)
        .await
        .unwrap_or_else(|error| RegistrationReport::deferred(bounded_error(&error.to_string())));
    if report.hubs.iter().any(|hub| hub.receipt.is_some()) {
        result.extra.insert("sharing".into(), json!("Discovery declaration is visible at responding hubs; durability or other hubs may remain pending."));
    }
    result
        .extra
        .insert("discovery_registration".into(), json!(report));
}

#[cfg(test)]
#[path = "cli_registration_tests.rs"]
mod tests;
