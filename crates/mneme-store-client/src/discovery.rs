//! Shared bounded acquisition of native owner-authored configuration metadata.
//!
//! Discovery is not activation: this module contacts only the native catalog
//! endpoint, never a memory read, store opener, launcher or background worker.
//! Frozen selection replay is a separate, strictly offline path.
mod cache;
#[cfg(test)]
mod cache_tests;
#[cfg(test)]
mod fixture;
#[cfg(test)]
mod tests;

use crate::{AnyErr, OperationBudget, native_profile};
use mneme_config::{
    DiscoveryGraph, DiscoveryReceipt, DiscoveryReport, DiscoverySeed, PublishedCatalog,
    SelectionPurpose, SelectionSnapshot, WorkspaceDeclarations, finish_workspace,
    prepare_workspace, read_discovery_receipt,
};
use mneme_mcp_client::{ConnectionOptions, RemoteClient};
use serde::Serialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct ResolveOptions {
    /// Deliberate acquisition; only a complete run replaces a usable receipt.
    pub refresh: bool,
    /// Inspect/replay only retained metadata. Never acquire even if it is absent.
    pub offline: bool,
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CacheStatus {
    LocalOnly,
    Cached,
    Acquired,
    Refreshed,
    Stale,
    Unavailable,
}

#[derive(Debug, Serialize)]
pub struct AttemptCoverage {
    pub unavailable: BTreeMap<String, String>,
    pub incomplete: Option<String>,
}
impl From<&DiscoveryGraph> for AttemptCoverage {
    fn from(graph: &DiscoveryGraph) -> Self {
        Self {
            unavailable: graph.unavailable.clone(),
            incomplete: graph.incomplete.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ResolutionOutcome {
    pub selection: Option<SelectionSnapshot>,
    pub report: Option<DiscoveryReport>,
    pub cache_status: CacheStatus,
    /// Current attempted coverage, separate from the retained frozen selection.
    /// In particular a failed refresh cannot rewrite the usable receipt's facts.
    pub acquisition: Option<DiscoveryGraph>,
    pub attempt: Option<AttemptCoverage>,
    pub error: Option<String>,
}

/// The single discovery-aware workspace entry point. `None` means genuinely
/// unconfigured; a configured but unavailable/unresolved workspace is `Some`
/// without a selection, so callers cannot accidentally fall back to another owner.
pub async fn resolve_workspace(
    user_path: Option<&Path>,
    cwd: &Path,
    profile: Option<&str>,
    options: ResolveOptions,
    budget: &OperationBudget,
) -> Result<Option<ResolutionOutcome>, AnyErr> {
    resolve_workspace_for_purpose(
        user_path,
        cwd,
        profile,
        options,
        budget,
        SelectionPurpose::Workspace,
    )
    .await
}

/// Explicit observatory selection. Catalog acquisition is unchanged; only the
/// resulting read-only owner inventory ignores workspace applicability.
pub async fn resolve_workspace_for_browsing(
    user_path: Option<&Path>,
    cwd: &Path,
    profile: Option<&str>,
    options: ResolveOptions,
    budget: &OperationBudget,
) -> Result<Option<ResolutionOutcome>, AnyErr> {
    resolve_workspace_for_purpose(
        user_path,
        cwd,
        profile,
        options,
        budget,
        SelectionPurpose::Browse,
    )
    .await
}

async fn resolve_workspace_for_purpose(
    user_path: Option<&Path>,
    cwd: &Path,
    profile: Option<&str>,
    options: ResolveOptions,
    budget: &OperationBudget,
    purpose: SelectionPurpose,
) -> Result<Option<ResolutionOutcome>, AnyErr> {
    if options.offline && options.refresh {
        return Err("offline inspection and discovery refresh are mutually exclusive".into());
    }
    let Some(prepared) = prepare_workspace(user_path, cwd, profile)? else {
        return Ok(None);
    };
    let prepared = if purpose == SelectionPurpose::Browse {
        prepared.for_browsing()
    } else {
        prepared
    };
    if !prepared.has_discovery() {
        let report = prepared.resolve_graph(&DiscoveryGraph::default())?.report;
        return Ok(Some(outcome_from_selection(
            finish_workspace(prepared, None),
            Some(report),
            CacheStatus::LocalOnly,
            None,
            None,
        )));
    }
    let path = prepared.receipt_path()?;
    resolve_prepared(prepared, options, budget, &path)
        .await
        .map(Some)
}

async fn resolve_prepared(
    prepared: WorkspaceDeclarations,
    options: ResolveOptions,
    budget: &OperationBudget,
    path: &Path,
) -> Result<ResolutionOutcome, AnyErr> {
    let prior = read_discovery_receipt(&path)?;
    if let Some((receipt, binding)) = prior.as_ref() {
        if receipt.boundary_sha256 != prepared.boundary_sha256()? {
            return Err(
                "discovery receipt boundary changed; explicitly refresh configuration".into(),
            );
        }
        if !options.refresh {
            let report = Some(prepared.resolve_graph(&receipt.graph)?.report);
            let selected = finish_workspace(prepared, Some((receipt, binding.clone())));
            return Ok(outcome_from_selection(
                selected,
                report,
                CacheStatus::Cached,
                None,
                None,
            ));
        }
    }
    if options.offline {
        let mut graph = DiscoveryGraph::default();
        for seed in prepared.discovery_seeds()? {
            graph.seeds.push(seed.id.clone());
            graph.unavailable.insert(seed.id, "no frozen discovery receipt; refresh explicitly or allow initial metadata acquisition".into());
        }
        graph.incomplete = Some("offline inspection has no frozen discovery receipt".into());
        let report = prepared.resolve_graph(&graph)?.report;
        return Ok(ResolutionOutcome {
            selection: None,
            report: Some(report),
            cache_status: CacheStatus::Unavailable,
            attempt: Some(AttemptCoverage::from(&graph)),
            acquisition: Some(graph),
            error: Some(
                "no frozen discovery receipt; offline inspection did not contact an owner".into(),
            ),
        });
    }
    // Serialize acquisition too: losing initial selectors reuse the winner and
    // do not produce a second publication or invalidate its admitted work.
    let lock = match cache::ReceiptLock::acquire(path, budget).await {
        Ok(lock) => lock,
        Err(error) => {
            let graph = unattempted(
                &prepared,
                "discovery acquisition could not begin within the local cache or shared operation envelope",
            )?;
            if let Some((receipt, binding)) = prior.as_ref() {
                let report = Some(prepared.resolve_graph(&receipt.graph)?.report);
                let selected = finish_workspace(prepared, Some((receipt, binding.clone())));
                return Ok(outcome_from_selection(
                    selected,
                    report,
                    CacheStatus::Stale,
                    Some(graph),
                    Some(format!(
                        "discovery refresh was not published; retained frozen configuration: {error}"
                    )),
                ));
            }
            return Err(error);
        }
    };
    let current = read_discovery_receipt(path)?;
    if !options.refresh {
        if let Some((receipt, binding)) = current.as_ref() {
            let report = Some(prepared.resolve_graph(&receipt.graph)?.report);
            let selected = finish_workspace(prepared, Some((receipt, binding.clone())));
            return Ok(outcome_from_selection(
                selected,
                report,
                CacheStatus::Cached,
                None,
                None,
            ));
        }
    } else if prior.as_ref().map(|(_, binding)| &binding.sha256)
        != current.as_ref().map(|(_, binding)| &binding.sha256)
    {
        return Err("discovery receipt changed while waiting to refresh; inspect current configuration and refresh deliberately".into());
    }
    let graph = match acquire(&prepared, budget).await {
        Ok(graph) => graph,
        Err(_) => unattempted(
            &prepared,
            "discovery acquisition could not resolve complete bounded metadata; no empty catalog was inferred",
        )?,
    };
    let report = prepared.resolve_graph(&graph)?.report;
    if !graph.unavailable.is_empty() || graph.incomplete.is_some() {
        if let Some((receipt, binding)) = current.as_ref() {
            let retained_report = Some(prepared.resolve_graph(&receipt.graph)?.report);
            let selected = finish_workspace(prepared, Some((receipt, binding.clone())));
            return Ok(outcome_from_selection(selected,retained_report,CacheStatus::Stale,Some(graph),Some("discovery refresh incomplete; retained the previous frozen configuration without new grants".into())));
        }
        return Ok(ResolutionOutcome {selection:None,report:Some(report),cache_status:CacheStatus::Unavailable,attempt:Some(AttemptCoverage::from(&graph)),acquisition:Some(graph),error:Some("discovery is incomplete; no configuration receipt was published and no memory operation was activated".into())});
    }
    prepared.validate_frozen_graph(&graph)?;
    let receipt = DiscoveryReceipt::new(prepared.boundary_sha256()?, graph.clone())?;
    lock.publish(&path, &receipt)?;
    let (receipt, binding) =
        read_discovery_receipt(&path)?.ok_or("published discovery receipt disappeared")?;
    let selected = finish_workspace(prepared, Some((&receipt, binding)));
    Ok(outcome_from_selection(
        selected,
        Some(report),
        if current.is_some() {
            CacheStatus::Refreshed
        } else {
            CacheStatus::Acquired
        },
        Some(graph),
        None,
    ))
}

fn unattempted(prepared: &WorkspaceDeclarations, reason: &str) -> Result<DiscoveryGraph, AnyErr> {
    let mut graph = DiscoveryGraph {
        incomplete: Some(reason.into()),
        ..DiscoveryGraph::default()
    };
    for seed in prepared.discovery_seeds()? {
        graph.seeds.push(seed.id.clone());
        graph.unavailable.insert(seed.id, reason.into());
    }
    Ok(graph)
}

fn outcome_from_selection(
    selected: mneme_config::Result<SelectionSnapshot>,
    report: Option<DiscoveryReport>,
    cache_status: CacheStatus,
    graph: Option<DiscoveryGraph>,
    warning: Option<String>,
) -> ResolutionOutcome {
    let (selection, error) = match selected {
        Ok(selection) => (Some(selection), warning),
        Err(error) => (None, Some(error.to_string())),
    };
    ResolutionOutcome {
        selection,
        report,
        cache_status,
        attempt: graph.as_ref().map(AttemptCoverage::from),
        acquisition: graph,
        error,
    }
}

pub async fn select_workspace(
    path: &Path,
    cwd: &Path,
    profile: Option<&str>,
    options: ResolveOptions,
    budget: &OperationBudget,
) -> Result<SelectionSnapshot, AnyErr> {
    let outcome = resolve_workspace(Some(path), cwd, profile, options, budget)
        .await?
        .ok_or("explicit configuration is unavailable")?;
    if options.refresh {
        if let Some(error) = outcome.error.as_ref() {
            return Err(format!("{error}; previous receipt retained when usable; inspect --offline or normal select can reuse it").into());
        }
    }
    outcome.selection.ok_or_else(|| {
        outcome
            .error
            .unwrap_or_else(|| "configured workspace has no resolved selection".into())
            .into()
    })
}

/// Exact offline replay. Never upgrade an old digest to today's remote policy,
/// acquire a missing receipt, or mint a replacement binding on a failed match.
pub fn replay_workspace(
    user_path: Option<&Path>,
    cwd: &Path,
    profile: Option<&str>,
    expected_digest: &str,
) -> Result<Option<SelectionSnapshot>, AnyErr> {
    let Some(prepared) = prepare_workspace(user_path, cwd, profile)? else {
        return Ok(None);
    };
    let path = if prepared.has_discovery() {
        Some(prepared.receipt_path()?)
    } else {
        None
    };
    replay_prepared(prepared, path.as_deref(), expected_digest).map(Some)
}

fn replay_prepared(
    prepared: WorkspaceDeclarations,
    path: Option<&Path>,
    expected_digest: &str,
) -> Result<SelectionSnapshot, AnyErr> {
    let snapshot = if let Some(path) = path {
        let (receipt, binding) = read_discovery_receipt(path)?.ok_or("selection digest changed or its frozen discovery receipt is missing; reselect explicitly, no operation was sent")?;
        finish_workspace(prepared, Some((&receipt, binding)))?
    } else {
        finish_workspace(prepared, None)?
    };
    if snapshot.digest != expected_digest {
        return Err("selection digest changed; reselect and reassess the original operation, no owner was contacted".into());
    }
    snapshot.revalidate_exact()?;
    Ok(snapshot)
}

/// Monotone, deterministic rounds. Every explicitly opted-in reachable edge is
/// retained as evidence, even when final merged activation/discovery is false.
/// A local denial still gates contact, and each canonical publisher is attempted
/// once. No URL-order precedence or recursive per-call budget exists here.
async fn acquire(
    prepared: &WorkspaceDeclarations,
    budget: &OperationBudget,
) -> Result<DiscoveryGraph, AnyErr> {
    let mut graph = DiscoveryGraph::default();
    let mut candidates: BTreeMap<String, DiscoverySeed> = prepared
        .discovery_seeds()?
        .into_iter()
        .map(|seed| (seed.id.clone(), seed))
        .collect();
    graph.seeds = candidates.keys().cloned().collect();
    if candidates.is_empty()
        && prepared.local.stores.iter().any(|(label, store)| {
            store.selected_discovery(prepared.local.device.as_deref()) == Some(true)
                && prepared
                    .catalog_contact_allowed(&store.id, label)
                    .unwrap_or(false)
        })
    {
        graph.incomplete = Some(
            "discovery was requested but no admitted seed has a complete local owner route".into(),
        );
        return Ok(graph);
    }
    let mut attempted = BTreeSet::new();
    'rounds: loop {
        if candidates.is_empty() {
            break;
        }
        for (id, seed) in std::mem::take(&mut candidates) {
            if attempted.len() >= mneme_config::MAX_DISCOVERY_PUBLISHERS {
                graph.incomplete = Some(
                    "publisher acquisition exceeds the bounded configuration graph envelope".into(),
                );
                break 'rounds;
            }
            if !attempted.insert(id.clone()) {
                continue;
            }
            if !prepared.catalog_contact_allowed(&id, &seed.label)? {
                continue;
            }
            match fetch_catalog(&seed, budget).await {
                Ok(catalog) => {
                    graph.catalogs.insert(id.clone(), catalog);
                    // Validate prospective aggregate work before the next owner
                    // contact. Reject the whole oversized source, not a silently
                    // truncated publication that could erase a denial.
                    if prepared.resolve_graph(&graph).is_err() {
                        graph.catalogs.remove(&id);
                        graph.unavailable.insert(id,"publisher snapshot exceeds the aggregate configuration envelope; whole document omitted".into());
                    }
                }
                Err(error) => {
                    graph.unavailable.insert(id, failure_reason(error.as_ref()));
                }
            }
        }
        let resolved = prepared.resolve_graph(&graph)?;
        let mut requested = BTreeMap::<String, BTreeSet<String>>::new();
        let mut reachable: BTreeSet<String> = graph
            .seeds
            .iter()
            .chain(graph.edges.values().flatten())
            .cloned()
            .collect();
        for (publisher, catalog) in &graph.catalogs {
            let edges = graph.edges.entry(publisher.clone()).or_default();
            for (label, declaration) in &catalog.stores {
                if !prepared.catalog_referral_allowed(label, declaration)? {
                    continue;
                }
                if !edges.contains(&declaration.id) {
                    if !reachable.contains(&declaration.id)
                        && reachable.len() >= mneme_config::MAX_DISCOVERY_PUBLISHERS
                    {
                        graph.incomplete = Some(
                            "publisher referrals exceed the bounded configuration graph envelope"
                                .into(),
                        );
                        continue;
                    }
                    reachable.insert(declaration.id.clone());
                    edges.push(declaration.id.clone());
                }
                requested
                    .entry(declaration.id.clone())
                    .or_default()
                    .insert(label.clone());
            }
            edges.sort();
        }
        // Route resolution must see this round's admitted graph edges. It is
        // separate from opt-in evidence: a final false is not a reason to erase
        // an independently authorized catalog-only branch.
        let resolved = if requested.is_empty() {
            resolved
        } else {
            prepared.resolve_graph(&graph)?
        };
        for (id, labels) in requested {
            if attempted.contains(&id) {
                continue;
            }
            if graph.catalogs.len() + graph.unavailable.len() + candidates.len()
                >= mneme_config::MAX_DISCOVERY_PUBLISHERS
                && !graph.unavailable.contains_key(&id)
            {
                graph.incomplete = Some(
                    "publisher acquisition exceeds the bounded configuration graph envelope".into(),
                );
                continue;
            }
            if let Some(owner) = resolved.routes.get(&id) {
                let label = resolved
                    .report
                    .stores
                    .get(&id)
                    .map(|store| store.label.clone())
                    .unwrap_or_else(|| labels.iter().next().cloned().unwrap_or_else(|| id.clone()));
                if prepared.catalog_contact_allowed(&id, &label)? {
                    candidates.insert(
                        id.clone(),
                        DiscoverySeed {
                            label,
                            id,
                            owner: owner.clone(),
                        },
                    );
                } else {
                    graph.unavailable.insert(id,"resolved publisher binding is outside the local privacy boundary; no owner was contacted".into());
                }
            } else {
                // This is unresolved route evidence, not a successful empty
                // document. Reconsider next round if new declarations fill gaps.
                graph.unavailable.insert(
                    id.clone(),
                    format!(
                        "catalog route unresolved or conflicting for {}",
                        labels.into_iter().next().unwrap_or(id)
                    ),
                );
            }
        }
        // A previously unresolved route can become resolvable in a later round;
        // only attempted transport failures remain unavailable permanently.
        for id in candidates.keys() {
            graph.unavailable.remove(id);
        }
    }
    Ok(graph)
}

#[derive(Debug)]
struct CatalogFailure(&'static str);
impl std::fmt::Display for CatalogFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for CatalogFailure {}
fn catalog_failure(reason: &'static str) -> AnyErr {
    Box::new(CatalogFailure(reason))
}
fn transport_failure(error: AnyErr, budget: &OperationBudget) -> AnyErr {
    if budget.bytes_remaining() == 0 || tokio::time::Instant::now() >= budget.deadline {
        catalog_failure(
            "shared discovery byte or deadline budget exhausted; publisher snapshot not obtained",
        )
    } else if mneme_mcp_client::is_availability(error.as_ref()) {
        catalog_failure("publisher unavailable; no empty catalog was inferred")
    } else {
        catalog_failure(
            "publisher rejected the native metadata protocol; verify owner build, route and catalog configuration",
        )
    }
}
async fn fetch_catalog(
    seed: &DiscoverySeed,
    budget: &OperationBudget,
) -> Result<PublishedCatalog, AnyErr> {
    let options = ConnectionOptions {
        url: seed.owner.url.clone(),
        token_env: seed.owner.token_env.clone(),
        ssh_mcp_port: seed.owner.ssh_mcp_port.unwrap_or(18766),
    };
    let timeouts = budget
        .timeouts()
        .map_err(|error| transport_failure(error, budget))?;
    let mut client = tokio::time::timeout_at(
        budget.deadline,
        RemoteClient::connect_native_reads_with_budget(&options, timeouts, budget.receive.clone()),
    )
    .await
    .map_err(|_| {
        catalog_failure("shared discovery deadline exhausted; publisher snapshot not obtained")
    })?
    .map_err(|error| transport_failure(error, budget))?;
    let result=match tokio::time::timeout_at(budget.deadline,async{
        native_profile(&client,false).map_err(|_|catalog_failure("publisher is not a native owner with a supported capability profile; routers cannot publish"))?;
        let databases=client.call_native_read("databases",json!({})).await.map_err(|error|transport_failure(error,budget))?;
        verify_catalog_owner(&databases,seed).map_err(|_|catalog_failure("publisher native identity, alias, path or retained state differs from the configured owner"))?;
        client.ensure_catalog().await.map_err(|error|transport_failure(error,budget))?;
        if !client.supports_discovery_catalog(){return Err(catalog_failure("native owner does not advertise guarded discovery_catalog; update owner or disable discovery explicitly"));}
        let value=client.call_tool("discovery_catalog",json!({"db":seed.owner.db,"expected_db_id":seed.id})).await.map_err(|error|transport_failure(error,budget))?;
        if value.get("stores").and_then(serde_json::Value::as_object).is_some_and(|stores|stores.values().any(|store|store.get("owner").is_some_and(|owner|owner.get("token_env").is_some()))){
            return Err(catalog_failure("published declarations cannot choose local secret references"));
        }
        let catalog=PublishedCatalog::parse_json(&serde_json::to_vec(&value)?).map_err(|_|catalog_failure("publisher returned malformed, oversized, unsupported or revision-mismatched catalog metadata"))?;
        if catalog.publisher_db_id!=seed.id{return Err(catalog_failure("catalog publisher identity differs from the pinned native owner"));}
        Ok(catalog)
    }).await {Ok(result)=>result,Err(_)=>Err(catalog_failure("shared discovery deadline exhausted; publisher snapshot not obtained"))};
    // Shutdown cannot spend a fresh per-publisher timeout after the shared
    // deadline; dropping also terminates any owned SSH forward.
    let _ = tokio::time::timeout_at(budget.deadline, client.close()).await;
    result
}

/// Metadata remains readable across retained native lifecycle states. This is
/// deliberately NOT the ordinary memory-read verifier, which requires `open`.
fn verify_catalog_owner(rows: &serde_json::Value, seed: &DiscoverySeed) -> Result<(), AnyErr> {
    let rows = rows.as_array().ok_or("invalid native database catalog")?;
    let mut matching = rows.iter().filter(|row| row["db"] == seed.owner.db);
    let row = matching
        .next()
        .ok_or("discovery owner alias is not advertised")?;
    if matching.next().is_some()
        || row["db_id"] != seed.id
        || !matches!(
            row["state"].as_str(),
            Some(
                "open"
                    | "releasing"
                    | "maintenance"
                    | "resuming"
                    | "snapshotting"
                    | "snapshot_recovery"
            )
        )
    {
        return Err(
            "discovery owner canonical identity or retained lifecycle state mismatch".into(),
        );
    }
    if let Some(path) = seed.owner.path.as_deref() {
        if row["resolved_path"].as_str().map(Path::new) != Some(path) {
            return Err("discovery owner path witness mismatch".into());
        }
    }
    Ok(())
}

fn failure_reason(error: &(dyn std::error::Error + 'static)) -> String {
    // Retain only our own stable, actionable diagnostics, never arbitrary peer
    // error text, endpoint internals, authentication values or secret names.
    error
        .downcast_ref::<CatalogFailure>()
        .map(|error| error.0)
        .unwrap_or("publisher acquisition failed; no empty catalog was inferred")
        .into()
}
