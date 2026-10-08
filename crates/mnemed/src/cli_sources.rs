//! One metadata-only projection for native `stores` and the observatory picker.
//! Known/configured does not mean reachable. Discovery never opens a store or MCP client.
use crate::{
    AnyErr,
    cli_owner::{OwnerOrigin, SelectedRemote},
    remote_config::RemoteOptions,
};
use clap::Args;
use mneme_tui::{SourceChoice, Target};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

pub(crate) struct Sources {
    pub(crate) choices: Vec<SourceChoice>,
    pub(crate) warnings: Vec<String>,
}
#[derive(Args)]
pub(crate) struct StoresArgs {
    /// List only this existing library's descriptors; no ambient owners.
    #[arg(long)]
    pub(crate) config: Option<PathBuf>,
}
pub(crate) fn selected_target(selected: SelectedRemote) -> Target {
    let name = if selected.origin == OwnerOrigin::User {
        "Global".into()
    } else {
        format!("{} ({})", selected.origin.label(), selected.remote.database)
    };
    Target {
        name,
        database: selected.remote.database,
        url: selected.remote.connection.url,
        ssh_mcp_port: selected.remote.connection.ssh_mcp_port,
        token_env: selected.remote.connection.token_env,
        expected_db_id: selected.owner.map(|owner| owner.db_id().to_string()),
        expected_path: None,
    }
}
// Name preference is discovery provenance, not a parsed display-string convention.
struct DiscoveredSource {
    choice: SourceChoice,
    authored_name: bool,
}
fn add(choices: &mut Vec<DiscoveredSource>, source: SourceChoice) {
    choices.push(DiscoveredSource {
        choice: source,
        authored_name: false,
    });
}
fn same_endpoint(left: &Target, right: &Target) -> bool {
    // URL parsing normalizes HTTP's empty root path/default port/host case.
    // In particular, never trim a significant /mcp versus /mcp/ path.
    let url = |value: &str| {
        url::Url::parse(value)
            .map(|url| url.to_string())
            .unwrap_or_else(|_| value.to_owned())
    };
    left.database == right.database
        && url(&left.url) == url(&right.url)
        && left.ssh_mcp_port == right.ssh_mcp_port
        && left.token_env == right.token_env
}
fn compatible_pin(left: &Option<String>, right: &Option<String>) -> bool {
    left.is_none() || right.is_none() || left == right
}
fn compatible_guards(left: &Target, right: &Target) -> bool {
    compatible_pin(&left.expected_db_id, &right.expected_db_id)
        && compatible_pin(&left.expected_path, &right.expected_path)
}
fn merge_named(records: Vec<(usize, DiscoveredSource)>) -> (usize, DiscoveredSource) {
    let position = records.iter().map(|(position, _)| *position).min().unwrap();
    let winner = records
        .iter()
        .find(|(_, source)| source.authored_name && !source.choice.name.trim().is_empty())
        .unwrap_or(&records[0]);
    let name = winner.1.choice.name.clone();
    let authored_name = winner.1.authored_name;
    let mut route = records[0].1.choice.route().unwrap().clone();
    for (_, source) in &records {
        let other = source.choice.route().unwrap();
        if route.expected_db_id.is_none() {
            route.expected_db_id = other.expected_db_id.clone();
        }
        if route.expected_path.is_none() {
            route.expected_path = other.expected_path.clone();
        }
    }
    route.name = name;
    (
        position,
        DiscoveredSource {
            choice: SourceChoice::routed(route),
            authored_name,
        },
    )
}
fn coalesce(records: Vec<DiscoveredSource>) -> Vec<SourceChoice> {
    let mut groups: Vec<Vec<(usize, DiscoveredSource)>> = Vec::new();
    let mut unrouted = Vec::new();
    for (position, source) in records.into_iter().enumerate() {
        let Some(route) = source.choice.route() else {
            unrouted.push((position, source));
            continue;
        };
        if let Some(group) = groups
            .iter_mut()
            .find(|group| same_endpoint(group[0].1.choice.route().unwrap(), route))
        {
            group.push((position, source));
        } else {
            groups.push(vec![(position, source)]);
        }
    }
    let mut merged = Vec::new();
    for group in groups {
        // Compatibility is NOT transitive: an unpinned descriptor must not
        // bridge two conflicting identities or paths, regardless of arrival order.
        let conflicting = group.iter().enumerate().any(|(index, (_, left))| {
            group[index + 1..].iter().any(|(_, right)| {
                !compatible_guards(left.choice.route().unwrap(), right.choice.route().unwrap())
            })
        });
        if conflicting {
            let mut exact: Vec<Vec<(usize, DiscoveredSource)>> = Vec::new();
            for item in group {
                let route = item.1.choice.route().unwrap();
                if let Some(group) = exact.iter_mut().find(|group| {
                    let other = group[0].1.choice.route().unwrap();
                    other.expected_db_id == route.expected_db_id
                        && other.expected_path == route.expected_path
                }) {
                    group.push(item);
                } else {
                    exact.push(vec![item]);
                }
            }
            let mut choices: Vec<_> = exact.into_iter().map(merge_named).collect();
            let names: Vec<_> = choices
                .iter()
                .map(|(_, source)| source.choice.name.clone())
                .collect();
            for (_, source) in &mut choices {
                if names
                    .iter()
                    .filter(|name| *name == &source.choice.name)
                    .count()
                    > 1
                {
                    let mut route = source.choice.route().unwrap().clone();
                    let id = route.expected_db_id.as_deref().unwrap_or("unbound");
                    route.name = format!(
                        "{} · ID {}{}",
                        source.choice.name,
                        id,
                        route
                            .expected_path
                            .as_deref()
                            .map(|path| format!(" · {path}"))
                            .unwrap_or_default()
                    );
                    source.choice = SourceChoice::routed(route);
                }
            }
            merged.extend(choices);
        } else {
            merged.push(merge_named(group));
        }
    }
    for (position, source) in unrouted {
        let candidates: Vec<_> = source
            .choice
            .expected_db_id
            .as_ref()
            .map(|id| {
                merged
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, other))| {
                        other.choice.route().is_some()
                            && other.choice.expected_db_id.as_ref() == Some(id)
                    })
                    .map(|(index, _)| index)
                    .collect()
            })
            .unwrap_or_default();
        if candidates.len() == 1 {
            let (existing_position, existing) = &mut merged[candidates[0]];
            *existing_position = (*existing_position).min(position);
            if source.authored_name
                && !existing.authored_name
                && !source.choice.name.trim().is_empty()
            {
                let mut route = existing.choice.route().unwrap().clone();
                route.name = source.choice.name;
                existing.choice = SourceChoice::routed(route);
                existing.authored_name = true;
            }
        } else {
            merged.push((position, source));
        }
    }
    merged.sort_by_key(|(position, _)| *position);
    merged
        .into_iter()
        .map(|(_, source)| source.choice)
        .collect()
}
fn prioritize(primary: Target, mut choices: Vec<SourceChoice>) -> Vec<SourceChoice> {
    let matches: Vec<_> = choices
        .iter()
        .enumerate()
        .filter(|(_, source)| {
            source.route().is_some_and(|route| {
                same_endpoint(&primary, route) && compatible_guards(&primary, route)
            })
        })
        .map(|(index, _)| index)
        .collect();
    let promote = if matches.len() == 1 {
        Some(matches[0])
    } else {
        matches.iter().copied().find(|index| {
            let route = choices[*index].route().unwrap();
            route.expected_db_id == primary.expected_db_id
                && route.expected_path == primary.expected_path
        })
    };
    if let Some(index) = promote {
        let source = choices.remove(index);
        let mut route = source.route().unwrap().clone();
        // Discovery can observe changed config between the required-primary
        // read and the known-source projection. Never weaken that first pin.
        if route.expected_db_id.is_none() {
            route.expected_db_id = primary.expected_db_id;
        }
        if route.expected_path.is_none() {
            route.expected_path = primary.expected_path;
        }
        route.name = source.name;
        choices.insert(0, SourceChoice::routed(route));
    } else {
        choices.insert(0, SourceChoice::routed(primary));
    }
    choices
}

fn optional_record(
    choices: &mut Vec<DiscoveredSource>,
    path: Result<PathBuf, AnyErr>,
    origin: OwnerOrigin,
) {
    let path = match path {
        Ok(path) => path,
        Err(error) => {
            add(
                choices,
                SourceChoice::unavailable(origin.label(), error.to_string()),
            );
            return;
        }
    };
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => add(
            choices,
            SourceChoice::unavailable(
                origin.label(),
                format!("Cannot inspect owner config: {error}"),
            ),
        ),
        Ok(_) => match crate::cli_owner::read_owner_record(&path, origin) {
            Ok(selected) => add(choices, SourceChoice::routed(selected_target(selected))),
            Err(error) => add(
                choices,
                SourceChoice::unavailable(origin.label(), error.to_string()),
            ),
        },
    }
}
/// List configured metadata independently of whether the default primary exists.
pub(crate) fn known() -> Sources {
    let library_config = match crate::cli_library_config::resolve(None) {
        Ok(path) => Ok(Some(path)),
        Err(error)
            if error.to_string().starts_with("no default library config")
                || error
                    .to_string()
                    .starts_with("library needs --config PATH when neither") =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    };
    known_at(
        std::env::current_dir().map_err(Into::into),
        crate::cli_owner::owner_config_path(OwnerOrigin::User),
        crate::cli_owner::owner_config_path(OwnerOrigin::Misc),
        library_config,
    )
}
fn known_at(
    cwd: Result<PathBuf, AnyErr>,
    user_config: Result<PathBuf, AnyErr>,
    misc_config: Result<PathBuf, AnyErr>,
    library_config: Result<Option<PathBuf>, AnyErr>,
) -> Sources {
    let mut choices = Vec::new();
    match cwd.and_then(|cwd| crate::cli_owner::discover_project(&cwd)) {
        Ok(Some(selected)) => add(
            &mut choices,
            SourceChoice::routed(selected_target(selected)),
        ),
        Ok(None) => {}
        Err(error) => add(
            &mut choices,
            SourceChoice::unavailable("project", error.to_string()),
        ),
    }
    optional_record(&mut choices, user_config, OwnerOrigin::User);
    optional_record(&mut choices, misc_config, OwnerOrigin::Misc);
    // The native library selector is the only ambient library source; never
    // enumerate directories or enroll arbitrary .db files.
    match library_config {
        Ok(Some(config)) => match library_entries(Some(&config)) {
            Ok(entries) => choices.extend(entries),
            Err(error) => add(
                &mut choices,
                SourceChoice::unavailable("selected library", error.to_string()),
            ),
        },
        Ok(None) => {}
        Err(error) => add(
            &mut choices,
            SourceChoice::unavailable("selected library", error.to_string()),
        ),
    }
    Sources {
        choices: coalesce(choices),
        warnings: Vec::new(),
    }
}
pub(crate) fn default_selection(user: bool) -> Result<Sources, AnyErr> {
    let selected = crate::cli_owner::resolve_store(&RemoteOptions::default(), user, None)?
        .ok_or("no configured owner")?;
    let warnings = selected.notice().map(str::to_owned).into_iter().collect();
    let choices = prioritize(selected_target(selected), known().choices);
    Ok(Sources { choices, warnings })
}

pub(crate) fn library(config: Option<&Path>) -> Result<Vec<SourceChoice>, AnyErr> {
    Ok(coalesce(library_entries(config)?))
}
fn library_entries(config: Option<&Path>) -> Result<Vec<DiscoveredSource>, AnyErr> {
    let config = crate::cli_library_config::resolve(config)?;
    let library = mneme_library::LibraryRuntime::from_path(&config)?;
    let mut choices = Vec::new();
    for (entry, route) in library.known_sources()? {
        if let Some(endpoint) = route {
            choices.push(DiscoveredSource {
                choice: SourceChoice::routed(Target {
                    name: entry.display_name,
                    database: entry.database,
                    url: endpoint.url,
                    ssh_mcp_port: endpoint.ssh_mcp_port,
                    token_env: endpoint.token_env,
                    expected_db_id: Some(entry.db_id),
                    expected_path: None,
                }),
                authored_name: true,
            });
        } else {
            let mut source = SourceChoice::unavailable(
                entry.display_name,
                "No configured live owner route; descriptor retained, not an empty database",
            );
            source.expected_db_id = Some(entry.db_id);
            choices.push(DiscoveredSource {
                choice: source,
                authored_name: true,
            });
        }
    }
    if let Some(core) = library.core_paths() {
        let result: Result<Target, AnyErr> = (|| {
            let mut raw = Vec::new();
            std::fs::File::open(&core.global_service)?
                .take(65537)
                .read_to_end(&mut raw)?;
            if raw.len() > 65536 {
                return Err("global service config exceeds 64 KiB".into());
            }
            let service: serde_json::Value = serde_json::from_slice(&raw)?;
            if service["mode"] != "connect" {
                return Err("global core is not configured in connect mode".into());
            }
            if service["database_path"].as_str().map(Path::new)
                != Some(core.global_database.as_path())
            {
                return Err(
                    "library core global_database and selected global service disagree".into(),
                );
            }
            Ok(Target {
                name: "Global".into(),
                database: service["database_name"]
                    .as_str()
                    .ok_or("global core lacks a database alias")?
                    .into(),
                url: service["url"]
                    .as_str()
                    .ok_or("global core lacks an owner URL")?
                    .into(),
                ssh_mcp_port: 18766,
                token_env: service["token_env"].as_str().map(str::to_owned),
                expected_db_id: None,
                expected_path: Some(core.global_database.to_string_lossy().into_owned()),
            })
        })();
        add(
            &mut choices,
            match result {
                Ok(target) => SourceChoice::routed(target),
                Err(error) => SourceChoice::unavailable("Global", error.to_string()),
            },
        );
    }
    Ok(choices)
}
pub(crate) fn run(
    args: &StoresArgs,
    remote: &RemoteOptions,
    user: bool,
    db: Option<&Path>,
    json: bool,
) -> Result<(), AnyErr> {
    if db.is_some() {
        return Err(
            "stores lists configured metadata, not --db/MNEME_DB files; unset MNEME_DB".into(),
        );
    }
    let sources = if let Some(config) = &args.config {
        if user
            || (remote.remote.is_some()
                || remote.remote_db.is_some()
                || remote.remote_config.is_some()
                || remote.remote_mcp_port.is_some()
                || remote.remote_token_env.is_some())
        {
            return Err(
                "stores --config cannot be combined with --user or --remote selectors".into(),
            );
        }
        Sources {
            choices: library(Some(config))?,
            warnings: Vec::new(),
        }
    } else if let Some(remote) = remote.resolve(user, None)? {
        Sources {
            choices: vec![SourceChoice::routed(selected_target(SelectedRemote {
                remote,
                owner: None,
                origin: OwnerOrigin::ExplicitRemote,
            }))],
            warnings: Vec::new(),
        }
    } else if user {
        let mut choices = Vec::new();
        optional_record(
            &mut choices,
            crate::cli_owner::owner_config_path(OwnerOrigin::User),
            OwnerOrigin::User,
        );
        Sources {
            choices: coalesce(choices),
            warnings: Vec::new(),
        }
    } else {
        known()
    };
    let value = serde_json::json!({"schema":"mneme.stores.v1","sources":sources.choices.iter().map(|source|serde_json::json!({"name":source.name,"db_id":source.expected_db_id,"route":if source.route().is_some(){"configured"}else{"unavailable"},"reason":source.unavailable_reason()})).collect::<Vec<_>>()});
    if json {
        println!("{}", serde_json::to_string(&value)?);
    } else if sources.choices.is_empty() {
        println!("No known configured sources.");
    } else {
        for source in sources.choices {
            println!(
                "{} · {}",
                source.name,
                source
                    .unavailable_reason()
                    .unwrap_or("configured route (not probed)")
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("mneme-source-projection-{}", ulid::Ulid::new()));
            std::fs::create_dir_all(path.join("project/.git")).unwrap();
            Self(path)
        }
        fn owner(&self, path: &str, alias: &str, url: &str) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path,serde_json::to_vec(&serde_json::json!({"schema":"mneme.cli.owner.v1","url":url,"database":alias,"db_id":ID})).unwrap()).unwrap();
        }
        fn known(&self, library: Result<Option<PathBuf>, AnyErr>) -> Sources {
            known_at(
                Ok(self.0.join("project")),
                Ok(self.0.join("global.json")),
                Ok(self.0.join("misc.json")),
                library,
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn projection_deduplicates_only_equivalent_routes_without_composite_names() {
        let fixture = Fixture::new();
        fixture.owner(
            "project/.mneme/cli.json",
            "user-alias",
            "http://127.0.0.1:1",
        );
        fixture.owner("global.json", "user-alias", "http://127.0.0.1:1");
        let sources = fixture.known(Ok(None)).choices;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "project (user-alias)");
        assert_eq!(
            sources[0].route().unwrap().expected_db_id.as_deref(),
            Some(ID)
        );
        fixture.owner("global.json", "user-alias", "http://127.0.0.1:2");
        assert_eq!(fixture.known(Ok(None)).choices.len(), 2);
    }
    #[test]
    fn projection_preserves_failed_known_metadata_but_not_absent_records() {
        let fixture = Fixture::new();
        fixture.owner(
            "project/.mneme/cli.json",
            "work-alias",
            "http://127.0.0.1:1",
        );
        assert_eq!(fixture.known(Ok(None)).choices.len(), 1);
        std::fs::write(fixture.0.join("global.json"), "malformed").unwrap();
        let choices = fixture
            .known(Err(
                "isolated profile needs an explicit library_config".into()
            ))
            .choices;
        assert_eq!(choices.len(), 3);
        assert!(choices[0].route().is_some());
        assert_eq!(choices[1].name, "global");
        assert!(
            choices[1]
                .unavailable_reason()
                .unwrap()
                .contains("owner config")
        );
        assert_eq!(choices[2].name, "selected library");
        assert!(
            choices[2]
                .unavailable_reason()
                .unwrap()
                .contains("isolated")
        );
    }
    fn target(name: &str, url: &str, id: Option<&str>, path: Option<&str>) -> Target {
        Target {
            name: name.into(),
            database: "user".into(),
            url: url.into(),
            ssh_mcp_port: 18766,
            token_env: None,
            expected_db_id: id.map(str::to_owned),
            expected_path: path.map(str::to_owned),
        }
    }
    fn discovered(route: Target, authored_name: bool) -> DiscoveredSource {
        DiscoveredSource {
            choice: SourceChoice::routed(route),
            authored_name,
        }
    }
    #[test]
    fn real_owner_core_complements_merge_with_both_guards_and_primary_name() {
        let owner = target("Global", "http://127.0.0.1:18767", Some(ID), None);
        let core = target(
            "global core",
            "http://127.0.0.1:18767/",
            None,
            Some("/home/user/.local/share/mneme/memory.db"),
        );
        for records in [
            vec![
                discovered(owner.clone(), false),
                discovered(core.clone(), false),
            ],
            vec![
                discovered(core.clone(), false),
                discovered(owner.clone(), false),
            ],
        ] {
            let sources = coalesce(records);
            assert_eq!(sources.len(), 1);
            let route = sources[0].route().unwrap();
            assert_eq!(route.expected_db_id.as_deref(), Some(ID));
            assert_eq!(route.expected_path, core.expected_path);
            assert_eq!(sources[0].expected_db_id, route.expected_db_id);
            assert_eq!(sources[0].name, route.name);
        }
    }
    #[test]
    fn default_selection_promotes_existing_authored_name_without_reconcatenation() {
        let project = target(
            "project (project)",
            "http://127.0.0.1:18765",
            Some(ID),
            None,
        );
        let library = target("mneme", "http://127.0.0.1:18765/", Some(ID), None);
        let global = target(
            "Global",
            "http://127.0.0.1:18767",
            Some("other-identity"),
            None,
        );
        let known = coalesce(vec![
            discovered(project.clone(), false),
            discovered(global.clone(), false),
            discovered(library, true),
        ]);
        let choices = prioritize(project, known);
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].name, "mneme");
        assert_eq!(choices[0].route().unwrap().name, "mneme");
        let choices = prioritize(global, choices);
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].name, "Global");
        assert_eq!(choices[1].name, "mneme");
    }
    #[test]
    fn conflicting_guard_bridges_never_merge_in_any_arrival_order() {
        for targets in [
            [
                target("same", "http://localhost", Some("identity-a"), None),
                target("same", "http://localhost/", Some("identity-b"), None),
                target("same", "http://localhost:80", None, Some("/path")),
            ],
            [
                target("same", "http://localhost", None, Some("/a")),
                target("same", "http://localhost/", None, Some("/b")),
                target("same", "http://localhost:80", Some(ID), None),
            ],
        ] {
            let expected: std::collections::BTreeSet<_> = targets
                .iter()
                .map(|route| (route.expected_db_id.clone(), route.expected_path.clone()))
                .collect();
            for order in [
                [0, 1, 2],
                [0, 2, 1],
                [1, 0, 2],
                [1, 2, 0],
                [2, 0, 1],
                [2, 1, 0],
            ] {
                let sources = coalesce(
                    order
                        .into_iter()
                        .map(|index| discovered(targets[index].clone(), false))
                        .collect(),
                );
                assert_eq!(sources.len(), 3);
                assert_eq!(
                    sources
                        .iter()
                        .map(|source| {
                            let route = source.route().unwrap();
                            assert_eq!(source.expected_db_id, route.expected_db_id);
                            assert_eq!(source.name, route.name);
                            (route.expected_db_id.clone(), route.expected_path.clone())
                        })
                        .collect::<std::collections::BTreeSet<_>>(),
                    expected
                );
                assert_eq!(
                    sources
                        .iter()
                        .map(|source| source.name.clone())
                        .collect::<std::collections::HashSet<_>>()
                        .len(),
                    3
                );
            }
        }
    }
    #[test]
    fn route_discriminators_remain_distinct_without_network_identity_discovery() {
        let base = target("source", "http://example.com/mcp", Some(ID), None);
        let mut variants = vec![base.clone()];
        let mut path = base.clone();
        path.url = "http://example.com/mcp/".into();
        variants.push(path);
        let mut endpoint = base.clone();
        endpoint.url = "http://other.example/mcp".into();
        variants.push(endpoint);
        let mut alias = base.clone();
        alias.database = "another".into();
        variants.push(alias);
        let mut port = base.clone();
        port.ssh_mcp_port = 1;
        variants.push(port);
        let mut auth = base.clone();
        auth.token_env = Some("OTHER_TOKEN".into());
        variants.push(auth);
        assert_eq!(
            coalesce(
                variants
                    .into_iter()
                    .map(|route| discovered(route, false))
                    .collect()
            )
            .len(),
            6
        );
    }
    #[test]
    fn unrouted_descriptor_folds_only_into_one_unambiguous_known_identity() {
        let routed = target(
            "project (project)",
            "http://localhost",
            Some(ID),
            Some("/known/path"),
        );
        let mut unavailable = SourceChoice::unavailable("mneme", "No configured live route");
        unavailable.expected_db_id = Some(ID.into());
        let record = || DiscoveredSource {
            choice: unavailable.clone(),
            authored_name: true,
        };
        let sources = coalesce(vec![discovered(routed.clone(), false), record()]);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "mneme");
        assert_eq!(
            sources[0].route().unwrap().expected_path.as_deref(),
            Some("/known/path")
        );
        let mut alternate = routed.clone();
        alternate.url = "http://other.example".into();
        let sources = coalesce(vec![
            discovered(routed, false),
            discovered(alternate, false),
            record(),
        ]);
        assert_eq!(sources.len(), 3);
        assert!(sources[2].route().is_none());
    }
    #[test]
    fn primary_promotion_unions_required_pins_after_changed_metadata() {
        let id_only = target("required", "http://localhost", Some(ID), None);
        let path_only = target(
            "authored known name",
            "http://localhost/",
            None,
            Some("/required/path"),
        );
        for (primary, known) in [
            (id_only.clone(), path_only.clone()),
            (path_only.clone(), id_only.clone()),
        ] {
            let name = known.name.clone();
            let choices = prioritize(primary, vec![SourceChoice::routed(known)]);
            assert_eq!(choices.len(), 1);
            let route = choices[0].route().unwrap();
            assert_eq!(route.expected_db_id.as_deref(), Some(ID));
            assert_eq!(route.expected_path.as_deref(), Some("/required/path"));
            assert_eq!(choices[0].name, name);
            assert_eq!(route.name, name);
            assert_eq!(choices[0].expected_db_id, route.expected_db_id);
        }
        let mut unpinned = path_only.clone();
        unpinned.expected_path = None;
        let choices = prioritize(id_only, vec![SourceChoice::routed(unpinned)]);
        assert_eq!(
            choices[0].route().unwrap().expected_db_id.as_deref(),
            Some(ID)
        );
    }
    #[test]
    fn primary_prefers_existing_exact_guard_row_among_ambiguous_matches() {
        let primary = target("required", "http://localhost", Some(ID), None);
        let exact = target("authored primary", "http://localhost/", Some(ID), None);
        let partial = target("partial", "http://localhost:80", None, Some("/path"));
        let choices = prioritize(
            primary,
            vec![SourceChoice::routed(partial), SourceChoice::routed(exact)],
        );
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].name, "authored primary");
        assert_eq!(
            choices[0].route().unwrap().expected_db_id.as_deref(),
            Some(ID)
        );
        assert!(choices[0].route().unwrap().expected_path.is_none());
    }
    #[test]
    fn conflicting_id_labels_do_not_collide_when_ulids_share_a_suffix() {
        let ids = ["01ARZ3NDEKTSV4RRFFQ69G5FAV", "01BRZ3NDEKTSV4RRFFQ69G5FAV"];
        let choices = coalesce(
            ids.into_iter()
                .map(|id| discovered(target("same", "http://localhost", Some(id), None), false))
                .collect(),
        );
        assert_eq!(choices.len(), 2);
        assert_ne!(choices[0].name, choices[1].name);
        for (choice, id) in choices.iter().zip(ids) {
            assert!(choice.name.contains(id));
            assert_eq!(choice.name, choice.route().unwrap().name);
        }
    }
}
