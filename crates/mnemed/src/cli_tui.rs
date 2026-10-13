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
}
impl From<TuiView> for View {
    fn from(view: TuiView) -> Self {
        match view {
            TuiView::Map => Self::Map,
            TuiView::Scenes => Self::Scenes,
            TuiView::Touchstones => Self::Touchstones,
        }
    }
}

#[derive(Args)]
pub(crate) struct TuiArgs {
    /// Open Map, Scenes or Touchstones; s switches Map/Scenes, t opens the cabinet.
    #[arg(long, value_enum, default_value = "map")]
    view: TuiView,
    /// Synthetic constellation; never contacts a memory service.
    #[arg(long, conflicts_with_all = ["config", "remote", "user", "db"])]
    demo: bool,
    /// Explicit library observatory; otherwise start on the selected project/user
    /// owner and Tab to its configured counterpart. Invalid optional counterpart
    /// config is shown as a warning; --remote selects a single source.
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
    if json {
        return Err(
            "tui is interactive, not JSON; use library/query/get for machine-readable results"
                .into(),
        );
    }
    if db.is_some() {
        return Err("tui never opens database files; use a configured CLI owner, --remote URL or tui --config PATH (unset MNEME_DB)".into());
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
        match crate::cli_config::discover(memory_config, profile)? {
            Some(snapshot) => declarative_targets(snapshot, store, user)?,
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
        println!("{}", mneme_tui::preview_view(120, 36, args.view.into())?);
        return Ok(());
    }
    mneme_tui::run(Options {
        sources: selection.sources,
        startup_warnings: selection.warnings,
        query: args.query.clone().unwrap_or_default(),
        demo: args.demo,
        view: args.view.into(),
    })
    .await
    .map_err(|e| -> AnyErr { e })
}

struct ResolvedTargets {
    sources: Vec<SourceChoice>,
    warnings: Vec<String>,
}

/// Same immutable metadata witness used by the CLI, MCP and native hook bridge.
fn declarative_targets(
    snapshot: mneme_config::SelectionSnapshot,
    named: Option<&str>,
    user: bool,
) -> Result<ResolvedTargets, AnyErr> {
    if user {
        return Err(
            "--user is a legacy selector; use --store NAME with declarative configuration".into(),
        );
    }
    let primary = named
        .or(snapshot.read_default.as_deref())
        .ok_or("no admitted TUI default; select --store NAME")?
        .to_owned();
    snapshot
        .explicit_store(&primary)?
        .require_operation(mneme_config::Operation::ExplicitRead)?;
    let snapshot = std::sync::Arc::new(snapshot);
    let mut sources = Vec::new();
    let mut rows: Vec<_> = snapshot.stores.iter().collect();
    rows.sort_by_key(|row| row.name != primary);
    for row in rows {
        if row.admitted && row.eligible.read && (row.active || row.name == primary) {
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
                row.reason
                    .as_deref()
                    .unwrap_or("read permission not granted"),
            );
            source.expected_db_id = Some(row.store.id.clone());
            sources.push(source);
        }
    }
    Ok(ResolvedTargets {
        sources,
        warnings: vec![],
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
        });
    }
    let selected = crate::cli_sources::default_selection(user)?;
    Ok(ResolvedTargets {
        sources: selected.choices,
        warnings: selected.warnings,
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

    #[test]
    fn declarative_picker_uses_the_cli_snapshot_and_does_not_activate_inventory() {
        let config = mneme_config::Config::parse(
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
"#,
        )
        .unwrap();
        let snapshot = mneme_config::select(
            &config,
            &mneme_config::SelectionContext::new(std::env::temp_dir().canonicalize().unwrap()),
        )
        .unwrap();
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
                .is_none()
        );
        assert!(
            selected
                .sources
                .iter()
                .find(|source| source.name == "blocked")
                .unwrap()
                .route()
                .is_none()
        );
        let explicit = declarative_targets(snapshot.clone(), Some("other"), false).unwrap();
        assert_eq!(explicit.sources[0].name, "other");
        assert!(explicit.sources[0].route().is_some());
        assert!(declarative_targets(snapshot.clone(), Some("blocked"), false).is_err());
        assert!(declarative_targets(snapshot, None, true).is_err());
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
