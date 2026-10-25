//! Explicitly invoked, non-owning memory observatory.
use crate::{AnyErr, remote_config::RemoteOptions};
use clap::{Args, ValueEnum};
#[cfg(test)]
use mneme_tui::Target;
use mneme_tui::{Options, SourceChoice, View};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Clone, Copy, Default, ValueEnum)]
enum TuiView {
    #[default]
    Map,
    Scenes,
    Touchstones,
    Config,
}
impl From<TuiView> for View {
    fn from(view: TuiView) -> Self {
        match view {
            TuiView::Map => Self::Map,
            TuiView::Scenes => Self::Scenes,
            TuiView::Touchstones => Self::Touchstones,
            // Configuration is a metadata-only modal, never a memory view.
            TuiView::Config => Self::Map,
        }
    }
}

#[derive(Args)]
pub(crate) struct TuiArgs {
    /// Open Map, Scenes, Touchstones or the metadata-only Config inspector.
    /// Config may acquire delegated catalogs, but never reads memories at startup.
    #[arg(long, value_enum, default_value = "map")]
    view: TuiView,
    /// Synthetic constellation; never contacts a memory service.
    #[arg(long, conflicts_with_all = ["config", "remote", "user", "db"])]
    demo: bool,
    /// Explicit library observatory; otherwise start on the configured default
    /// owner and Tab to another readable source, ignoring workspace applicability.
    /// --remote selects one route.
    #[arg(long, conflicts_with_all = ["remote", "user", "db"])]
    config: Option<PathBuf>,
    /// Open a semantic search lens instead of the default graph inventory.
    #[arg(long)]
    query: Option<String>,
    /// Render one 120x36 synthetic frame and exit; useful without a terminal.
    #[arg(long, requires = "demo")]
    snapshot: bool,
}

pub(crate) async fn run(
    args: &TuiArgs,
    remote: &RemoteOptions,
    user: bool,
    db: Option<&std::path::Path>,
    memory_config: Option<&std::path::Path>,
    store: Option<&str>,
    profile: Option<&str>,
    json: bool,
) -> Result<(), AnyErr> {
    let inspect_config = matches!(args.view, TuiView::Config);
    if json {
        return Err(
            "tui is interactive, not JSON; use library/query/get for machine-readable results"
                .into(),
        );
    }
    if db.is_some() {
        return Err("tui never opens database files; use a configured CLI owner, --remote URL or tui --config PATH (unset MNEME_DB)".into());
    }
    if inspect_config && args.query.is_some() {
        return Err("tui --view config inspects metadata; omit --query, then explicitly open a source to search memories".into());
    }
    if args
        .query
        .as_ref()
        .is_some_and(|query| query.trim().is_empty() || query.len() > 4096)
    {
        return Err("tui query must be nonblank and at most 4096 UTF-8 bytes".into());
    }
    // Refuse noninteractive invocation before network, profile or config access.
    if !args.snapshot && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal()) {
        return Err(
            "tui needs an interactive terminal; try tui --demo --snapshot for a preview".into(),
        );
    }
    let selection = if !args.demo && args.config.is_none() && remote.remote.is_none() {
        match discover_targets(memory_config, profile, store, user, inspect_config).await? {
            Some(selection) => selection,
            None if inspect_config => ResolvedTargets::empty(
                "No declarative configuration found; no ambient owner fallback was attempted",
            ),
            None if store.is_some() || profile.is_some() || memory_config.is_some() => {
                return Err(
                    "declarative TUI selection requires memory.toml; use --memory-config PATH"
                        .into(),
                );
            }
            None => resolve_targets(args, remote, user)?,
        }
    } else {
        if memory_config.is_some() || store.is_some() || profile.is_some() {
            return Err("do not mix declarative selectors with demo, explicit library or remote TUI selection".into());
        }
        resolve_targets(args, remote, user)?
    };
    if args.snapshot {
        println!(
            "{}",
            if inspect_config {
                mneme_tui::preview_config(120, 36)?
            } else {
                mneme_tui::preview_view(120, 36, args.view.into())?
            }
        );
        return Ok(());
    }
    mneme_tui::run(Options {
        sources: selection.sources,
        startup_warnings: selection.warnings,
        query: args.query.clone().unwrap_or_default(),
        demo: args.demo,
        view: args.view.into(),
        config_report: selection.report,
        config_selection: selection.selection,
        inspect_config,
    })
    .await
    .map_err(|e| -> AnyErr { e })
}

struct ResolvedTargets {
    sources: Vec<SourceChoice>,
    warnings: Vec<String>,
    report: Option<std::sync::Arc<mneme_config::DiscoveryReport>>,
    selection: Option<std::sync::Arc<mneme_config::SelectionSnapshot>>,
}

impl ResolvedTargets {
    fn empty(warning: impl Into<String>) -> Self {
        Self {
            sources: vec![],
            warnings: vec![warning.into()],
            report: None,
            selection: None,
        }
    }

    fn attach_report(
        &mut self,
        report: Option<mneme_config::DiscoveryReport>,
        inspected_name: Option<&str>,
    ) -> Result<(), AnyErr> {
        if let Some(report) = &report {
            for (id, row) in &report.stores {
                if !self
                    .sources
                    .iter()
                    .any(|source| source.expected_db_id.as_deref() == Some(id.as_str()))
                {
                    let mut source = SourceChoice::unavailable(
                        &row.label,
                        "No resolved admitted owner route; inspect configuration for details",
                    );
                    source.expected_db_id = Some(id.clone());
                    self.sources.push(source);
                }
            }
        }
        self.report = report.map(std::sync::Arc::new);
        if let Some(name) = inspected_name {
            if !self.sources.iter().any(|source| source.name == name) {
                return Err(format!("no configured store named {name}; omit --store to inspect all declarations (no fallback attempted)").into());
            }
            // A denied/unresolved known row is inspectable, not an authority
            // error. A typo is different: never silently show another source.
            self.sources.sort_by_key(|source| source.name != name);
        }
        Ok(())
    }
}

/// The shared acquisition workflow reads catalog metadata, not owner memories.
/// Configured-but-unresolved results remain inspectable and never fall back.
async fn discover_targets(
    memory_config: Option<&std::path::Path>,
    profile: Option<&str>,
    named: Option<&str>,
    user: bool,
    inspect: bool,
) -> Result<Option<ResolvedTargets>, AnyErr> {
    let cwd = std::env::current_dir()?;
    let budget =
        mneme_store_client::OperationBudget::new(1024 * 1024, std::time::Duration::from_secs(15))
            .map_err(|error| -> AnyErr { error })?;
    let outcome = mneme_store_client::discovery::resolve_workspace_for_browsing(
        memory_config,
        &cwd,
        profile,
        mneme_store_client::discovery::ResolveOptions::default(),
        &budget,
    )
    .await;
    let outcome = match outcome {
        Ok(Some(outcome)) => outcome,
        Ok(None) => return Ok(None),
        Err(error) if inspect => {
            return Ok(Some(ResolvedTargets::empty(format!(
                "Configuration unresolved: {error}; no fallback attempted",
            ))));
        }
        Err(error) => return Err(error),
    };
    let mut selected = match outcome.selection {
        Some(snapshot) => declarative_targets_for_view(snapshot, named, user, inspect)?,
        None if inspect => ResolvedTargets::empty("No resolved selection; admission and activation remain unknown"),
        None => return Err(format!("configuration selection unresolved: {}; use tui --view config to inspect it (no fallback attempted)", outcome.error.as_deref().unwrap_or("unresolved discovery")).into()),
    };
    selected.attach_report(outcome.report, inspect.then_some(named).flatten())?;
    let cache_status = serde_json::to_value(outcome.cache_status)?;
    selected.warnings.push(format!(
        "Configuration metadata: {}",
        cache_status.as_str().unwrap_or("unknown")
    ));
    if let Some(error) = outcome.error {
        selected.warnings.push(error);
    }
    if let Some(attempt) = outcome.attempt {
        for (id, reason) in attempt.unavailable {
            selected.warnings.push(format!(
                "Acquisition attempt (not selected evidence): {id}: {reason}"
            ));
        }
        if let Some(reason) = attempt.incomplete {
            selected.warnings.push(format!(
                "Incomplete acquisition attempt (not selected evidence): {reason}"
            ));
        }
    }
    Ok(Some(selected))
}

/// Same immutable metadata witness used by the CLI, MCP and native hook bridge.
#[cfg(test)]
fn declarative_targets(
    snapshot: mneme_config::SelectionSnapshot,
    named: Option<&str>,
    user: bool,
) -> Result<ResolvedTargets, AnyErr> {
    declarative_targets_for_view(snapshot, named, user, false)
}

fn declarative_targets_for_view(
    snapshot: mneme_config::SelectionSnapshot,
    named: Option<&str>,
    user: bool,
    inspect: bool,
) -> Result<ResolvedTargets, AnyErr> {
    if snapshot.purpose != mneme_config::SelectionPurpose::Browse {
        return Err("TUI requires an explicit read-only browsing selection".into());
    }
    if user {
        return Err(
            "--user is a legacy selector; use --store NAME with declarative configuration".into(),
        );
    }
    let primary = named
        .or(snapshot.read_default.as_deref())
        .or_else(|| {
            snapshot
                .stores
                .iter()
                .find(|row| row.eligible.read)
                .map(|row| row.name.as_str())
        })
        .map(str::to_owned);
    if !inspect {
        snapshot
            .explicit_store(
                primary.as_deref().ok_or(
                    "no admitted TUI default; select --store NAME or use tui --view config",
                )?,
            )?
            .require_operation(mneme_config::Operation::ExplicitRead)?;
    }
    let snapshot = std::sync::Arc::new(snapshot);
    let mut sources = Vec::new();
    let mut rows: Vec<_> = snapshot.stores.iter().collect();
    rows.sort_by_key(|row| Some(row.name.as_str()) != primary.as_deref());
    for row in rows {
        // The shared browse snapshot ignores workspace applicability and grants
        // only explicit reads. Real permission/profile/identity boundaries stay.
        if row.admitted && row.eligible.read {
            let owner = &row.store.owner;
            sources.push(SourceChoice::routed(mneme_tui::Target {
                name: row.name.clone(),
                database: owner.db.clone(),
                url: owner.url.clone(),
                ssh_mcp_port: owner.ssh_mcp_port.unwrap_or(18766),
                token_env: owner.token_env.clone(),
                expected_db_id: Some(row.store.id.clone()),
                expected_path: owner
                    .path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                configured: Some(mneme_tui::ConfiguredTarget {
                    selection: snapshot.clone(),
                    store: row.name.clone(),
                }),
            }));
        } else {
            let mut source = SourceChoice::unavailable(
                &row.name,
                if row.admitted {
                    "read permission not granted"
                } else {
                    row.reason.as_deref().unwrap_or("store is not admitted")
                },
            );
            source.expected_db_id = Some(row.store.id.clone());
            sources.push(source);
        }
    }
    Ok(ResolvedTargets {
        sources,
        warnings: vec![],
        report: None,
        selection: Some(snapshot),
    })
}

/// Explicit selectors never inspect or append ambient sources.
fn resolve_targets(
    args: &TuiArgs,
    remote: &RemoteOptions,
    user: bool,
) -> Result<ResolvedTargets, AnyErr> {
    if args.demo {
        return Ok(ResolvedTargets {
            sources: vec![SourceChoice::unavailable(
                "Demo",
                "Synthetic memories · no live source",
            )],
            warnings: Vec::new(),
            report: matches!(args.view, TuiView::Config)
                .then(|| std::sync::Arc::new(mneme_tui::demo_config_report())),
            selection: None,
        });
    }
    if let Some(config) = args.config.as_deref() {
        if user
            || (remote.remote.is_some()
                || remote.remote_db.is_some()
                || remote.remote_config.is_some()
                || remote.remote_mcp_port.is_some()
                || remote.remote_token_env.is_some())
        {
            return Err("tui --config is an explicit library view; do not combine it with --user or --remote selectors".into());
        }
        return Ok(ResolvedTargets {
            sources: crate::cli_sources::library(Some(config))?,
            warnings: Vec::new(),
            report: None,
            selection: None,
        });
    }
    if let Some(remote) = remote.resolve(user, None)? {
        return Ok(ResolvedTargets {
            sources: vec![SourceChoice::routed(crate::cli_sources::selected_target(
                crate::cli_owner::SelectedRemote {
                    remote,
                    owner: None,
                    origin: crate::cli_owner::OwnerOrigin::ExplicitRemote,
                },
            ))],
            warnings: Vec::new(),
            report: None,
            selection: None,
        });
    }
    let selected = crate::cli_sources::default_selection(user)?;
    Ok(ResolvedTargets {
        sources: selected.choices,
        warnings: selected.warnings,
        report: None,
        selection: None,
    })
}

#[cfg(test)]
impl ResolvedTargets {
    fn targets(self) -> Vec<Target> {
        self.sources
            .into_iter()
            .filter_map(|source| source.route().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browse_context(cwd: PathBuf) -> mneme_config::SelectionContext {
        let mut context = mneme_config::SelectionContext::new(cwd);
        context.purpose = mneme_config::SelectionPurpose::Browse;
        context
    }

    fn declarative_config(cwd: &std::path::Path) -> mneme_config::Config {
        let mut config = mneme_config::Config::parse(
            r#"
schema = "mneme.config.v1"
read_default = "renamed-project"
[stores.renamed-project]
id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
owner = { url = "http://127.0.0.1:1", db = "project-alias" }
active = { workspace = "any" }
[stores.other]
id = "01BRZ3NDEKTSV4RRFFQ69G5FAV"
owner = { url = "http://127.0.0.1:2", db = "other-alias" }
active = { workspace = "project", roots = ["/not-this-workspace"] }
[stores.blocked]
id = "01CRZ3NDEKTSV4RRFFQ69G5FAV"
owner = { url = "http://127.0.0.1:3", db = "blocked-alias" }
active = { workspace = "any", profiles = ["isolated"] }
[stores.unreadable]
id = "01DRZ3NDEKTSV4RRFFQ69G5FAV"
owner = { url = "http://127.0.0.1:4", db = "unreadable-alias" }
active = { workspace = "any" }
permissions = { read = false, write = true }
[stores.disabled]
id = "01ERZ3NDEKTSV4RRFFQ69G5FAV"
owner = { url = "http://127.0.0.1:5", db = "disabled-alias" }
active = { enabled = false, workspace = "any" }
[stores.excluded]
id = "01FRZ3NDEKTSV4RRFFQ69G5FAV"
owner = { url = "http://127.0.0.1:6", db = "excluded-alias" }
active = { workspace = "any" }
"#,
        )
        .unwrap();
        config
            .stores
            .get_mut("excluded")
            .unwrap()
            .active
            .excluded_roots = vec![cwd.to_owned()];
        config
    }

    #[test]
    fn declarative_picker_uses_the_cli_snapshot_and_does_not_activate_inventory() {
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        let config = declarative_config(&cwd);
        let snapshot = mneme_config::select(&config, &browse_context(cwd)).unwrap();
        let digest = snapshot.digest.clone();
        let selected = declarative_targets(snapshot.clone(), None, false).unwrap();
        assert_eq!(selected.sources[0].name, "renamed-project");
        assert_eq!(
            selected.sources[0].route().unwrap().database,
            "project-alias"
        );
        let binding = selected.sources[0]
            .route()
            .unwrap()
            .configured
            .as_ref()
            .unwrap();
        assert_eq!(binding.selection.digest, digest);
        assert_eq!(*binding.selection, snapshot);
        assert!(
            selected
                .sources
                .iter()
                .find(|source| source.name == "other")
                .unwrap()
                .route()
                .is_some()
        );
        let inactive = snapshot
            .stores
            .iter()
            .find(|row| row.name == "other")
            .unwrap();
        assert!(!inactive.active);
        assert!(inactive.admitted && inactive.eligible.read);
        assert!(!inactive.eligible.core && !inactive.eligible.context && !inactive.eligible.recall);
        assert!(!snapshot.active.iter().any(|row| row.name == "other"));
        for source in &selected.sources {
            if let Some(route) = source.route() {
                assert_eq!(*route.configured.as_ref().unwrap().selection, snapshot);
            }
        }
        let explicit = declarative_targets(snapshot.clone(), Some("other"), false).unwrap();
        assert_eq!(explicit.sources[0].name, "other");
        assert!(explicit.sources[0].route().is_some());
        assert!(declarative_targets(snapshot.clone(), Some("blocked"), false).is_err());
        assert!(declarative_targets(snapshot, None, true).is_err());
    }

    #[test]
    fn declarative_picker_preserves_read_and_admission_denials() {
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        let mut config = declarative_config(&cwd);
        for profile in ["default", "private", "isolated"] {
            let mut context = browse_context(cwd.clone());
            context.profile = profile.into();
            let snapshot = mneme_config::select(&config, &context).unwrap();
            if profile == "isolated" {
                // Naming a store cannot bypass isolation without an allowlist.
                for row in &snapshot.stores {
                    assert!(!row.admitted);
                    assert!(declarative_targets(snapshot.clone(), Some(&row.name), false).is_err());
                }
                config.workspace.allow_stores =
                    Some(vec!["renamed-project".into(), "other".into()]);
            }
            let snapshot = mneme_config::select(&config, &context).unwrap();
            let selected =
                declarative_targets(snapshot.clone(), Some("renamed-project"), false).unwrap();
            for source in &selected.sources {
                let expected = matches!(source.name.as_str(), "renamed-project" | "other")
                    || (profile != "isolated"
                        && matches!(source.name.as_str(), "disabled" | "excluded"));
                assert_eq!(
                    source.route().is_some(),
                    expected,
                    "{profile}: {}",
                    source.name
                );
                let row = snapshot
                    .stores
                    .iter()
                    .find(|row| row.name == source.name)
                    .unwrap();
                assert_eq!(
                    source.expected_db_id.as_deref(),
                    Some(row.store.id.as_str())
                );
                assert_eq!(
                    declarative_targets(snapshot.clone(), Some(&source.name), false).is_ok(),
                    expected
                );
                if source.name == "unreadable" && profile != "isolated" {
                    assert_eq!(
                        source.unavailable_reason(),
                        Some("read permission not granted")
                    );
                }
                if let Some(route) = source.route() {
                    assert_eq!(*route.configured.as_ref().unwrap().selection, snapshot);
                    assert!(!row.eligible.write && !row.active);
                    assert!(!row.eligible.core && !row.eligible.context && !row.eligible.recall);
                    assert!(!row.eligible.record && !row.eligible.maintenance);
                }
            }
        }
    }

    #[test]
    fn configuration_inspection_keeps_zero_defaultless_and_denied_stores() {
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        let empty = mneme_config::Config::parse("schema = 'mneme.config.v1'").unwrap();
        let snapshot = mneme_config::select(&empty, &browse_context(cwd.clone())).unwrap();
        assert!(snapshot.stores.is_empty());
        assert!(snapshot.read_default.is_none());
        let selected = declarative_targets_for_view(snapshot, None, false, true).unwrap();
        assert!(selected.sources.is_empty());
        assert!(
            selected.selection.is_some(),
            "empty metadata is not an absent selection"
        );

        let mut config = declarative_config(&cwd);
        config.read_default = "project-or-misc".into();
        let snapshot = mneme_config::select(&config, &browse_context(cwd.clone())).unwrap();
        assert!(snapshot.read_default.is_none());
        assert!(declarative_targets(snapshot.clone(), None, false).is_ok());
        let selected = declarative_targets_for_view(snapshot.clone(), None, false, true).unwrap();
        assert!(
            selected
                .sources
                .iter()
                .any(|source| source.route().is_some())
        );
        assert_eq!(*selected.selection.unwrap(), snapshot);

        for store in config.stores.values_mut() {
            store.active.enabled = false;
        }
        let snapshot = mneme_config::select(&config, &browse_context(cwd)).unwrap();
        let selected =
            declarative_targets_for_view(snapshot.clone(), Some("disabled"), false, true).unwrap();
        assert_eq!(selected.sources[0].name, "disabled");
        assert_eq!(selected.sources.len(), snapshot.stores.len());
        assert!(
            selected.sources[0].route().is_some(),
            "activation disable does not disable browsing"
        );
        assert_eq!(*selected.selection.unwrap(), snapshot);
    }

    #[test]
    fn report_only_rows_are_inspectable_but_unknown_names_do_not_select_another_source() {
        let report = mneme_tui::demo_config_report();
        let name = report.stores.values().next().unwrap().label.clone();
        let id = report.stores.keys().next().unwrap().clone();
        let mut selected = ResolvedTargets::empty("unresolved selection");
        selected
            .attach_report(Some(report.clone()), Some(&name))
            .unwrap();
        assert_eq!(selected.sources.len(), 1);
        assert_eq!(selected.sources[0].name, name);
        assert_eq!(
            selected.sources[0].expected_db_id.as_deref(),
            Some(id.as_str())
        );
        assert!(selected.sources[0].route().is_none());
        assert!(selected.selection.is_none());
        assert_eq!(*selected.report.as_ref().unwrap().as_ref(), report);
        assert!(
            selected
                .attach_report(None, Some("typo"))
                .unwrap_err()
                .to_string()
                .contains("no configured store named typo")
        );
    }

    #[test]
    fn explicit_remote_and_demo_do_not_append_ambient_owners() {
        let args = TuiArgs {
            view: TuiView::Map,
            demo: false,
            config: None,
            query: None,
            snapshot: false,
        };
        let options = RemoteOptions {
            remote: Some("http://127.0.0.1:1".into()),
            ..Default::default()
        };
        let selected = resolve_targets(&args, &options, false).unwrap();
        assert!(selected.warnings.is_empty());
        assert!(selected.report.is_none() && selected.selection.is_none());
        let targets = selected.targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].url, "http://127.0.0.1:1");
        assert!(targets[0].expected_db_id.is_none());
        let demo = TuiArgs { demo: true, ..args };
        assert!(
            resolve_targets(&demo, &RemoteOptions::default(), false)
                .unwrap()
                .targets()
                .is_empty()
        );
    }

    #[test]
    fn library_source_discovery_preserves_identity_and_global_binding() {
        let dir = std::env::temp_dir().join(format!("mneme-tui-routing-{}", ulid::Ulid::new()));
        std::fs::create_dir(&dir).unwrap();
        let service = dir.join("service.json");
        std::fs::write(&service,serde_json::to_vec(&serde_json::json!({"mode":"connect","url":"http://127.0.0.1:1","database_name":"user","database_path":"/global.db","token_env":"TEST_TUI_TOKEN"})).unwrap()).unwrap();
        let config = dir.join("library.json");
        std::fs::write(&config,serde_json::to_vec(&serde_json::json!({"schema":"mneme.library.config.v1","library_id":"test","device_id":"mac","catalog_path":"catalog.json","owner_routes":{"mac":{"database-1":{"url":"http://127.0.0.1:2"}}},"core":{"global_service":service,"global_database":"/global.db"}})).unwrap()).unwrap();
        std::fs::write(dir.join("catalog.json"),r#"{"schema":"mneme.library.catalog.v1","library_id":"test","revision":1,"entries":[{"project_id":"p1","db_id":"database-1","owner_device_id":"mac","display_name":"Mneme","database":"project","revision":1,"replicas":[]},{"project_id":"p2","db_id":"database-2","owner_device_id":"offline","display_name":"Offline","database":"project","revision":1,"replicas":[]}]}"#).unwrap();
        let args = TuiArgs {
            view: TuiView::Map,
            demo: false,
            config: Some(config.clone()),
            query: None,
            snapshot: false,
        };
        let selected = resolve_targets(&args, &RemoteOptions::default(), false).unwrap();
        assert!(selected.warnings.is_empty());
        assert!(selected.report.is_none() && selected.selection.is_none());
        assert_eq!(selected.sources.len(), 3);
        assert_eq!(
            selected.sources[1].expected_db_id.as_deref(),
            Some("database-2")
        );
        assert!(selected.sources[1].route().is_none());
        let sources = selected.targets();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].name, "Mneme");
        assert_eq!(sources[0].expected_db_id.as_deref(), Some("database-1"));
        assert_eq!(sources[1].expected_path.as_deref(), Some("/global.db"));
        assert_eq!(sources[1].token_env.as_deref(), Some("TEST_TUI_TOKEN"));
        std::fs::write(&service,r#"{"mode":"connect","url":"http://127.0.0.1:1","database_name":"user","database_path":"/wrong.db"}"#).unwrap();
        assert!(
            crate::cli_sources::library(Some(&config))
                .unwrap()
                .iter()
                .any(|s| s
                    .unavailable_reason()
                    .is_some_and(|r| r.contains("disagree")))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
