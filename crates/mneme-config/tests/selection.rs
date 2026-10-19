use mneme_config::*;
use std::path::{Path, PathBuf};

const FIRST: &str = "01M3CV5EZP4HWAK56YJCMZQG2Y";
const SECOND: &str = "01M3XW4C1X5E4YW0Q7Q6V5Y4XH";
const THIRD: &str = "01M3CPEVDTM9PP4HJTMMBZ8HR9";
fn entry(name: &str, id: &str, activation: &str) -> String {
    format!(
        "\n[stores.{name}]\nid='{id}'\nowner={{url='http://127.0.0.1:1',db='project'}}\nactive={{{activation}}}\n"
    )
}
fn config(extra: &str) -> Config {
    Config::parse(&format!("schema='mneme.config.v1'\n{extra}")).unwrap()
}
fn context(path: &Path) -> SelectionContext {
    SelectionContext::new(path.to_owned())
}
fn store_names(s: &SelectionSnapshot) -> Vec<&str> {
    s.active.iter().map(|s| s.name.as_str()).collect()
}
fn recipe(root: &Path) -> Config {
    config(&format!(
        "{}{}{}",
        entry("personal", FIRST, "workspace='any'"),
        entry("ordinary", SECOND, "workspace='no-project'"),
        entry(
            "work",
            THIRD,
            &format!("workspace='project',roots=['{}']", root.display())
        )
    ))
}
#[test]
fn ordinary_recipe_uses_activation_not_names() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let cfg = recipe(&project);
    let no_project = select(&cfg, &context(dir.path())).unwrap();
    assert_eq!(store_names(&no_project), vec!["ordinary", "personal"]);
    assert_eq!(no_project.read_default.as_deref(), Some("ordinary"));
    let configured = select(&cfg, &context(&project)).unwrap();
    assert_eq!(store_names(&configured), vec!["personal", "work"]);
    assert_eq!(configured.read_default.as_deref(), Some("work"));
    for store in &configured.active {
        assert!(
            store.eligible.read
                && store.eligible.write
                && store.eligible.core
                && store.eligible.context
                && store.eligible.recall
                && store.eligible.record
                && store.eligible.maintenance
        );
    }
}
#[test]
fn background_off_keeps_foreground_core_context_and_explicit_grants() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = recipe(&dir.path().join("project"));
    cfg.background.enabled = false;
    for s in select(&cfg, &context(dir.path())).unwrap().active {
        assert!(s.eligible.core && s.eligible.context && s.eligible.read && s.eligible.write);
        assert!(!s.eligible.recall && !s.eligible.record && !s.eligible.maintenance);
    }
}
#[test]
fn permission_delivery_background_matrix_is_exhaustive() {
    let dir = tempfile::tempdir().unwrap();
    for read in [false, true] {
        for write in [false, true] {
            for background in [false, true] {
                for delivery in [false, true] {
                    let mut cfg = config(&entry("not_global", FIRST, "workspace='any'"));
                    cfg.background.enabled = background;
                    let store = cfg.stores.get_mut("not_global").unwrap();
                    store.permissions = Permissions { read, write };
                    store.delivery = Delivery {
                        core: delivery,
                        context: delivery,
                    };
                    let selected = select(&cfg, &context(dir.path())).unwrap();
                    let s = &selected.active[0];
                    assert_eq!(s.eligible.read, read);
                    assert_eq!(s.eligible.write, write);
                    assert_eq!(s.eligible.core, read && delivery);
                    assert_eq!(s.eligible.context, read && delivery);
                    assert_eq!(s.eligible.recall, read && delivery && background);
                    assert_eq!(s.eligible.record, write && background);
                    assert_eq!(s.eligible.maintenance, read && write && background);
                    for (op, allowed) in [
                        (Operation::ExplicitRead, read),
                        (Operation::ExplicitWrite, write),
                        (Operation::Core, read && delivery),
                        (Operation::Context, read && delivery),
                        (Operation::Recall, read && delivery && background),
                        (Operation::Record, write && background),
                        (Operation::Maintenance, read && write && background),
                    ] {
                        assert_eq!(s.require_operation(op).is_ok(), allowed);
                    }
                }
            }
        }
    }
}
#[test]
fn each_background_lane_is_independent() {
    let dir = tempfile::tempdir().unwrap();
    for recall in [false, true] {
        for record in [false, true] {
            for maintenance in [false, true] {
                let mut cfg = config(&entry("memory", FIRST, "workspace='any'"));
                cfg.stores.get_mut("memory").unwrap().background = StoreBackground {
                    recall,
                    record,
                    maintenance,
                    ..StoreBackground::default()
                };
                let s = select(&cfg, &context(dir.path())).unwrap().active.remove(0);
                assert!(s.eligible.core && s.eligible.context);
                assert_eq!(s.eligible.recall, recall);
                assert_eq!(s.eligible.record, record);
                assert_eq!(s.eligible.maintenance, maintenance);
            }
        }
    }
}
#[test]
fn disabled_project_is_configured_and_never_misc() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = recipe(dir.path());
    cfg.stores.get_mut("work").unwrap().active.enabled = false;
    let s = select(&cfg, &context(dir.path())).unwrap();
    assert!(s.project.configured);
    assert_eq!(store_names(&s), vec!["personal"]);
    assert_eq!(s.read_default, None);
}
#[test]
fn independent_lane_scope_preserves_core_everywhere_but_local_recording() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let mut cfg = config(&entry("anything", FIRST, "workspace='any'"));
    let store = cfg.stores.get_mut("anything").unwrap();
    store.applicability.record.roots = vec![work.clone()];
    store.applicability.context.roots = vec![work.clone()];
    let outside = select(&cfg, &context(dir.path())).unwrap().active.remove(0);
    assert!(outside.eligible.core);
    assert!(!outside.eligible.context && !outside.eligible.recall && !outside.eligible.record);
    let inside = select(&cfg, &context(&work)).unwrap().active.remove(0);
    assert!(
        inside.eligible.core
            && inside.eligible.context
            && inside.eligible.recall
            && inside.eligible.record
    );
}
#[test]
fn default_private_isolated_and_empty_allowlist_admission() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&entry("personal", FIRST, "workspace='any'"));
    let mut ctx = context(dir.path());
    ctx.profile = "private".into();
    assert_eq!(store_names(&select(&cfg, &ctx).unwrap()), vec!["personal"]);
    ctx.profile = "isolated".into();
    let s = select(&cfg, &ctx).unwrap();
    assert!(s.active.is_empty());
    assert!(s.explicit_store("personal").is_err());
    cfg.workspace.allow_stores = Some(vec!["personal".into()]);
    assert_eq!(store_names(&select(&cfg, &ctx).unwrap()), vec!["personal"]);
    cfg.workspace.allow_stores = Some(vec![]);
    ctx.profile = "default".into();
    assert!(select(&cfg, &ctx).unwrap().active.is_empty());
}
#[test]
fn exclusions_win_over_explicit_name_and_canonical_alias() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&entry("personal", FIRST, "workspace='any'"));
    cfg.stores
        .get_mut("personal")
        .unwrap()
        .active
        .excluded_roots = vec![dir.path().to_owned()];
    let s = select(&cfg, &context(dir.path())).unwrap();
    assert!(s.active.is_empty());
    assert!(s.explicit_store("personal").is_err());
}
#[test]
fn nonactive_explicit_access_does_not_reactivate_background() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = recipe(dir.path());
    cfg.stores.get_mut("work").unwrap().active.enabled = false;
    let s = select(&cfg, &context(dir.path())).unwrap();
    let ordinary = s.explicit_store("ordinary").unwrap();
    assert!(ordinary.eligible.read && ordinary.eligible.write);
    assert!(!ordinary.eligible.core && !ordinary.eligible.record && !ordinary.eligible.recall);
    assert!(s.explicit_store("work").is_err());
}
#[test]
fn snapshot_digest_covers_every_resolved_policy_and_selection_field() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&entry("memory", FIRST, "workspace='any'"));
    let s = select(&cfg, &context(dir.path())).unwrap();
    s.verify_digest().unwrap();
    let json = serde_json::to_vec(&s).unwrap();
    let round: SelectionSnapshot = serde_json::from_slice(&json).unwrap();
    round.verify_digest().unwrap();
    assert_eq!(s, round);
    let mut changes = vec![];
    let mut changed = s.clone();
    changed.config.stores.get_mut("memory").unwrap().owner.db = "other".into();
    changes.push(changed);
    let mut changed = s.clone();
    changed.active[0].eligible.write = false;
    changes.push(changed);
    let mut changed = s.clone();
    changed.origins.push(PathBuf::from("/other"));
    changes.push(changed);
    let mut changed = s.clone();
    changed.profile = "private".into();
    changes.push(changed);
    for changed in changes {
        assert!(changed.verify_digest().is_err());
    }
}
#[test]
fn strict_closed_duplicate_types_bounds_and_no_diagnostic_secrets() {
    for bad in [
        "schema='mneme.config.v1'\nunknown=true",
        "schema='mneme.config.v1'\nschema='mneme.config.v1'",
        "schema='mneme.config.v2'",
        "schema='mneme.config.v1'\n[background]\nenabled='secret-token'",
        "schema='mneme.config.v1'\n[workspace]\nprofile='secret-token'",
    ] {
        let e = Config::parse(bad).unwrap_err();
        assert!(!e.to_string().contains("secret-token"));
    }
    assert!(Config::parse(&"x".repeat(MAX_CONFIG_BYTES + 1)).is_err());
    for suffix in [
        "permissions={read='yes'}",
        "guidance={tag_guide={id='01M3CV5EZP4HWAK56YJCMZQG2Y',content='body'}}",
        "retrieval={tags=['a','a']}",
        "capture={max_summary_bytes=513}",
        "capture={required_tags=['core']}",
        "capture={kinds=['unknown']}",
        "capture={max_body_bytes=-1}",
    ] {
        assert!(
            Config::parse(&format!(
                "schema='mneme.config.v1'\n{}{}",
                entry("memory", FIRST, "workspace='any'"),
                suffix
            ))
            .is_err(),
            "{suffix}"
        );
    }
}

fn path_list_toml(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| serde_json::to_string(path.to_str().unwrap()).unwrap())
        .collect::<Vec<_>>()
        .join(",")
}

fn path_list_config(location: &str, paths: &[PathBuf]) -> String {
    let paths = path_list_toml(paths);
    let store = if let Some(field) = location.strip_prefix("active.") {
        entry(
            "memory",
            FIRST,
            &format!("workspace='any',{field}=[{paths}]"),
        )
    } else {
        format!(
            "{}{location}=[{paths}]\n",
            entry("memory", FIRST, "workspace='any'")
        )
    };
    format!("schema='mneme.config.v1'\n{store}")
}

fn path_list_locations() -> Vec<String> {
    [
        "active",
        "applicability.core",
        "applicability.context",
        "applicability.recall",
        "applicability.record",
        "applicability.maintenance",
    ]
    .into_iter()
    .flat_map(|scope| {
        [
            format!("{scope}.roots"),
            format!("{scope}.subtree_roots"),
            format!("{scope}.excluded_roots"),
        ]
    })
    .collect()
}

#[test]
fn every_root_list_accepts_explicit_worktree_capacity_and_refuses_one_more() {
    assert_eq!(MAX_PATHS_PER_LIST, 256);
    let paths = (0..=MAX_PATHS_PER_LIST)
        .map(|i| PathBuf::from(format!("/worktrees/w{i:03}")))
        .collect::<Vec<_>>();
    for location in path_list_locations() {
        // Retain the old range, and exercise the reported fleet and exact new cap.
        for count in [0, 64, 65, 150, MAX_PATHS_PER_LIST] {
            let text = path_list_config(&location, &paths[..count]);
            Config::parse(&text).unwrap();
            Config::parse_overlay(&text).unwrap();
        }
        let text = path_list_config(&location, &paths);
        for parse in [Config::parse, Config::parse_overlay] {
            let error = parse(&text).unwrap_err().to_string();
            assert!(error.contains("256 entries"), "{location}: {error}");
            assert!(error.contains("reduce roots/excluded_roots"));
            assert!(!error.contains("/worktrees"));
        }
    }
    assert_eq!(MAX_STORES, 64, "store identity capacity is independent");
}

#[test]
fn every_root_list_retains_aggregate_utf8_byte_and_individual_path_bounds() {
    assert_eq!(MAX_PATH_LIST_BYTES, 16_384);
    // Eight distinct 2,048-byte paths: count characters incorrectly and the +1
    // case would pass. These are parser-only paths, not filesystem components.
    let paths = (0..8)
        .map(|i| PathBuf::from(format!("/{}{i}", "é".repeat(1023))))
        .collect::<Vec<_>>();
    assert_eq!(
        paths.iter().map(|p| p.as_os_str().len()).sum::<usize>(),
        MAX_PATH_LIST_BYTES
    );
    for location in path_list_locations() {
        let text = path_list_config(&location, &paths);
        Config::parse(&text).unwrap();
        Config::parse_overlay(&text).unwrap();
        let mut over = paths.clone();
        over[0].as_mut_os_string().push("x");
        for parse in [Config::parse, Config::parse_overlay] {
            let error = parse(&path_list_config(&location, &over))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("16384 total path bytes"),
                "{location}: {error}"
            );
            assert!(error.contains("shorten paths or reduce"));
            assert!(!error.contains('é'));
        }
        let maximum_path = PathBuf::from(format!("/{}", "a".repeat(4095)));
        Config::parse(&path_list_config(
            &location,
            std::slice::from_ref(&maximum_path),
        ))
        .unwrap();
        let mut oversized_path = maximum_path;
        oversized_path.as_mut_os_string().push("a");
        assert!(Config::parse(&path_list_config(&location, &[oversized_path])).is_err());
        assert!(
            Config::parse(&path_list_config(
                &location,
                &[PathBuf::from("/same"), PathBuf::from("/same")]
            ))
            .is_err()
        );
        assert!(
            Config::parse(&path_list_config(
                &location,
                &[PathBuf::from("/work/../other")]
            ))
            .is_err()
        );
    }
}

#[test]
fn explicit_150_worktrees_select_without_parent_membership_or_nested_project_leaks() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().canonicalize().unwrap();
    let mut roots = (0..150)
        .map(|i| parent.join(format!("w{i:03}")))
        .collect::<Vec<_>>();
    for root in &roots {
        std::fs::create_dir(root).unwrap();
        // Git worktrees use a .git file; selection inspects presence, not Git data.
        std::fs::write(root.join(".git"), "gitdir: /unused-metadata-only-fixture").unwrap();
    }
    let user = parent.join("user.toml");
    let write_config = |roots: &[PathBuf], exclusions: &[PathBuf]| {
        let roots = path_list_toml(roots);
        std::fs::write(
            &user,
            format!(
                "schema='mneme.config.v1'\n{}applicability.record={{roots=[{roots}]}}\n",
                entry(
                    "work",
                    FIRST,
                    &format!(
                        "workspace='project',roots=[{roots}],excluded_roots=[{}]",
                        path_list_toml(exclusions)
                    )
                )
            ),
        )
        .unwrap();
    };
    write_config(&roots, &[]);
    for root in &roots {
        let selected = select_workspace(&user, root, None).unwrap();
        assert_eq!(store_names(&selected), vec!["work"]);
        assert!(selected.active[0].eligible.record);
        selected.verify_digest().unwrap();
    }
    let unrelated = parent.join("unlisted");
    std::fs::create_dir(&unrelated).unwrap();
    assert!(
        select_workspace(&user, &unrelated, None)
            .unwrap()
            .active
            .is_empty()
    );
    // A parent is not a shorthand for membership: it crosses neither the Git
    // worktree marker nor a nested .git/.mneme boundary.
    write_config(std::slice::from_ref(&parent), &[]);
    assert!(
        select_workspace(&user, roots.last().unwrap(), None)
            .unwrap()
            .active
            .is_empty()
    );
    write_config(&roots, &[]);
    for marker in [".git", ".mneme"] {
        let child = roots[149].join(marker.trim_start_matches('.'));
        std::fs::create_dir_all(child.join(marker)).unwrap();
        assert!(
            select_workspace(&user, &child, None)
                .unwrap()
                .active
                .is_empty()
        );
        roots.push(child.clone());
        write_config(&roots, &[]);
        assert!(
            select_workspace(&user, &child, None).unwrap().active[0]
                .eligible
                .record
        );
    }
    // Exclusions still override a listed root, even at the end of a large list.
    let excluded = roots[149].clone();
    let mut exclusions = roots[..150].to_vec();
    write_config(&roots, &exclusions);
    let selected = select_workspace(&user, &excluded, None).unwrap();
    assert!(selected.active.is_empty());
    assert!(selected.explicit_store("work").is_err());
    #[cfg(unix)]
    {
        let lexical_parent = parent.join("private-aliases");
        std::fs::create_dir(&lexical_parent).unwrap();
        let lexical = lexical_parent.join("alias");
        std::os::unix::fs::symlink(&excluded, &lexical).unwrap();
        // An alias in the exclusion blocks the canonical root too.
        exclusions[149] = lexical.clone();
        write_config(&roots, &exclusions);
        assert!(
            select_workspace(&user, &excluded, None)
                .unwrap()
                .active
                .is_empty()
        );
        // This parent is not an ancestor of the canonical cwd. Only the lexical
        // origin proves the exclusion, and it must survive canonicalization.
        exclusions[149] = lexical_parent;
        write_config(&roots, &exclusions);
        assert!(
            select_workspace(&user, &lexical, None)
                .unwrap()
                .active
                .is_empty()
        );
        assert!(
            select_workspace(&user, &excluded, None).unwrap().active[0]
                .eligible
                .record
        );
    }
}

fn positive_path_lists_config(scope: &str, roots: &[PathBuf], subtrees: &[PathBuf]) -> String {
    let fields = format!(
        "roots=[{}],subtree_roots=[{}]",
        path_list_toml(roots),
        path_list_toml(subtrees)
    );
    let store = if scope == "active" {
        entry("memory", FIRST, &format!("workspace='any',{fields}"))
    } else {
        format!(
            "{}{scope}={{{fields}}}\n",
            entry("memory", FIRST, "workspace='any'")
        )
    };
    format!("schema='mneme.config.v1'\n{store}")
}

#[test]
fn ordinary_and_subtree_roots_share_count_bytes_and_uniqueness_bounds() {
    let short = (0..=MAX_PATHS_PER_LIST)
        .map(|i| PathBuf::from(format!("/w/{i}")))
        .collect::<Vec<_>>();
    let long = (0..8)
        .map(|i| PathBuf::from(format!("/{}{i}", "é".repeat(1023))))
        .collect::<Vec<_>>();
    for scope in [
        "active",
        "applicability.core",
        "applicability.context",
        "applicability.recall",
        "applicability.record",
        "applicability.maintenance",
    ] {
        for parse in [Config::parse, Config::parse_overlay] {
            parse(&positive_path_lists_config(
                scope,
                &short[..128],
                &short[128..256],
            ))
            .unwrap();
            let error = parse(&positive_path_lists_config(
                scope,
                &short[..128],
                &short[128..],
            ))
            .unwrap_err()
            .to_string();
            assert!(error.contains("256 entries") && error.contains("share one bound"));
            parse(&positive_path_lists_config(scope, &long[..4], &long[4..])).unwrap();
            let mut over = long.clone();
            over[7].as_mut_os_string().push("x");
            let error = parse(&positive_path_lists_config(scope, &over[..4], &over[4..]))
                .unwrap_err()
                .to_string();
            assert!(error.contains("16384 total path bytes") && error.contains("share one bound"));
            assert!(
                parse(&positive_path_lists_config(scope, &short[..1], &short[..1]))
                    .unwrap_err()
                    .to_string()
                    .contains("duplicate path")
            );
        }
    }
}

fn subtree_project_config(root: &Path, activation_extra: &str) -> String {
    format!(
        "schema='mneme.config.v1'\n{}{}{}",
        entry("personal", FIRST, "workspace='any'"),
        entry("ordinary", SECOND, "workspace='no-project'"),
        entry(
            "work",
            THIRD,
            &format!(
                "workspace='project',subtree_roots=[{}]{activation_extra}",
                path_list_toml(&[root.to_owned()])
            )
        )
    )
}

#[test]
fn subtree_activation_includes_future_unrelated_projects_without_enumeration() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("worktrees");
    std::fs::create_dir(&parent).unwrap();
    let user = dir.path().join("user.toml");
    std::fs::write(&user, subtree_project_config(&parent, "")).unwrap();
    let retained_parent = select_workspace(&user, &parent, None).unwrap();
    assert_eq!(store_names(&retained_parent), vec!["personal", "work"]);
    assert_eq!(retained_parent.read_default.as_deref(), Some("work"));
    for marker in [
        "git-directory",
        "git-worktree-file",
        "mneme",
        "codex-routing",
    ] {
        let child = parent.join(marker);
        std::fs::create_dir(&child).unwrap();
        let retained_child = select_workspace(&user, &child, None).unwrap();
        match marker {
            "git-directory" => std::fs::create_dir(child.join(".git")).unwrap(),
            "git-worktree-file" => {
                std::fs::write(child.join(".git"), "gitdir: unrelated-fixture").unwrap()
            }
            "mneme" => std::fs::create_dir(child.join(".mneme")).unwrap(),
            "codex-routing" => {
                std::fs::create_dir(child.join(".codex")).unwrap();
                std::fs::write(
                    child.join(".codex/config.toml"),
                    "[mcp_servers.mneme]\nurl='http://127.0.0.1:1'",
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            retained_child.revalidate_exact().is_err(),
            "new ancestry metadata is still frozen"
        );
        let selected = select_workspace(&user, &child, None).unwrap();
        assert_eq!(store_names(&selected), vec!["personal", "work"]);
        assert!(selected.active[1].eligible.core && selected.active[1].eligible.record);
        selected.revalidate_exact().unwrap();
        retained_parent.revalidate_exact().unwrap();
    }
    let ordinary_roots = subtree_project_config(&parent, "").replace("subtree_roots", "roots");
    std::fs::write(&user, &ordinary_roots).unwrap();
    assert_eq!(
        store_names(&select_workspace(&user, &parent.join("git-directory"), None).unwrap()),
        vec!["personal"]
    );
    std::fs::write(
        &user,
        ordinary_roots.replace("workspace='project'", "workspace='any'"),
    )
    .unwrap();
    assert!(
        store_names(&select_workspace(&user, &parent.join("git-directory"), None).unwrap())
            .contains(&"work"),
        "existing any-workspace root-prefix behavior is unchanged"
    );
    std::fs::write(&user, subtree_project_config(&parent, "")).unwrap();
    let outside = dir.path().join("worktrees-other");
    std::fs::create_dir(&outside).unwrap();
    let selected = select_workspace(&user, &outside, None).unwrap();
    assert_eq!(store_names(&selected), vec!["ordinary", "personal"]);
    assert_eq!(selected.read_default.as_deref(), Some("ordinary"));
    // Disabled project scope still proves project presence, never misc fallback.
    std::fs::write(&user, subtree_project_config(&parent, ",enabled=false")).unwrap();
    let selected = select_workspace(&user, &parent, None).unwrap();
    assert!(selected.project.configured);
    assert_eq!(store_names(&selected), vec!["personal"]);
}

#[test]
fn every_operation_subtree_is_independent_and_exclusions_still_win() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("work");
    let child = parent.join("new-project");
    let excluded = child.join("private");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(child.join(".git")).unwrap();
    std::fs::create_dir(&excluded).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let user = dir.path().join("user.toml");
    for scope in ["core", "context", "recall", "record", "maintenance"] {
        std::fs::write(&user, format!(
            "schema='mneme.config.v1'\n{}applicability.{scope}={{subtree_roots=[{}],excluded_roots=[{}]}}\n",
            entry("personal", FIRST, "workspace='any'"),
            path_list_toml(std::slice::from_ref(&parent)), path_list_toml(std::slice::from_ref(&excluded))
        )).unwrap();
        for (cwd, scoped) in [(&child, true), (&outside, false), (&excluded, false)] {
            let selected = select_workspace(&user, cwd, None).unwrap();
            let store = &selected.active[0];
            assert!(store.eligible.read && store.eligible.write);
            for (operation, name, eligible) in [
                (Operation::Core, "core", store.eligible.core),
                (Operation::Context, "context", store.eligible.context),
                (Operation::Recall, "recall", store.eligible.recall),
                (Operation::Record, "record", store.eligible.record),
                (
                    Operation::Maintenance,
                    "maintenance",
                    store.eligible.maintenance,
                ),
            ] {
                let expected =
                    scoped || (scope != name && !(scope == "context" && name == "recall"));
                assert_eq!(
                    eligible, expected,
                    "scope={scope} operation={name} cwd={cwd:?}"
                );
                assert_eq!(store.require_operation(operation).is_ok(), expected);
            }
        }
    }
}

#[test]
fn subtree_scope_does_not_bypass_profiles_allowlists_permissions_or_nested_overlays() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("work");
    let child = parent.join("child");
    std::fs::create_dir_all(child.join(".mneme")).unwrap();
    let user = dir.path().join("user.toml");
    std::fs::write(&user, subtree_project_config(&parent, "")).unwrap();
    let profile = child.join(".mneme/profile.json");
    for (mode, names, allowed) in [
        ("default", "[]", false),
        ("private", "['work']", true),
        ("isolated", "[]", false),
        ("isolated", "['work']", true),
    ] {
        let names = names.replace('\'', "\"");
        std::fs::write(
            &profile,
            format!(r#"{{"schema":"mneme.profile.v2","mode":"{mode}","stores":{names}}}"#),
        )
        .unwrap();
        let selected = select_workspace(&user, &child, None).unwrap();
        assert_eq!(
            store_names(&selected),
            if allowed { vec!["work"] } else { vec![] }
        );
        assert_eq!(selected.explicit_store("work").is_ok(), allowed);
    }
    std::fs::remove_file(&profile).unwrap();
    std::fs::write(
        &user,
        format!(
            "{}[workspace]\nallow_stores=[]\n",
            subtree_project_config(&parent, "")
        ),
    )
    .unwrap();
    assert!(
        select_workspace(&user, &child, None)
            .unwrap()
            .active
            .is_empty()
    );
    std::fs::write(
        &user,
        subtree_project_config(&parent, ",profiles=['default']"),
    )
    .unwrap();
    assert_eq!(
        store_names(&select_workspace(&user, &child, Some("private")).unwrap()),
        vec!["personal"]
    );
    let policy = format!(
        "{}permissions={{read=false,write=false}}\n",
        subtree_project_config(&parent, "")
    );
    std::fs::write(&user, policy).unwrap();
    let selected = select_workspace(&user, &child, None).unwrap();
    let work = &selected.active[1];
    assert!(
        !work.eligible.read && !work.eligible.write && !work.eligible.core && !work.eligible.record
    );
    std::fs::write(&user, subtree_project_config(&parent, "")).unwrap();
    std::fs::write(
        child.join(".mneme/memory.toml"),
        "schema='mneme.config.v1'\n[background]\nenabled=false\n",
    )
    .unwrap();
    let selected = select_workspace(&user, &child, None).unwrap();
    assert!(selected.active[1].eligible.core && !selected.active[1].eligible.record);
    std::fs::write(child.join(".mneme/memory.toml"), "invalid").unwrap();
    assert!(select_workspace(&user, &child, None).is_err());
}

#[test]
fn subtree_selectors_suppress_both_overlay_default_root_inference_paths() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(project.join(".mneme")).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    let text = format!(
        "schema='mneme.config.v1'\n{}",
        entry(
            "work",
            THIRD,
            &format!(
                "workspace='project',subtree_roots=[{}]",
                path_list_toml(std::slice::from_ref(&elsewhere))
            )
        )
    );
    let merged = config("")
        .merge_project(Config::parse_overlay(&text).unwrap(), &project)
        .unwrap();
    assert!(merged.stores["work"].active.roots.is_empty());
    assert_eq!(merged.stores["work"].active.subtree_roots, vec![elsewhere]);
    let overlay = project.join(".mneme/memory.toml");
    std::fs::write(&overlay, &text).unwrap();
    let user = dir.path().join("user.toml");
    std::fs::write(&user, "schema='mneme.config.v1'").unwrap();
    for source in [&user, &overlay] {
        let selected = select_workspace(source, &project, None).unwrap();
        assert!(selected.config.stores["work"].active.roots.is_empty());
        assert!(
            selected.active.is_empty(),
            "an explicit elsewhere scope must not acquire its overlay's root"
        );
    }
}

#[cfg(unix)]
#[test]
fn subtree_scope_preserves_lexical_canonical_exclusions_and_all_origin_matching() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().canonicalize().unwrap().join("work");
    let child = parent.join("child");
    let private = parent.join("private");
    let outside = dir.path().canonicalize().unwrap().join("outside");
    std::fs::create_dir_all(child.join(".git")).unwrap();
    std::fs::create_dir(&private).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let user = dir.path().join("user.toml");
    let link = private.join("alias");
    symlink(&child, &link).unwrap();
    std::fs::write(
        &user,
        subtree_project_config(
            &parent,
            &format!(",excluded_roots=[{}]", path_list_toml(&[private])),
        ),
    )
    .unwrap();
    assert!(
        select_workspace(&user, &link, None)
            .unwrap()
            .explicit_store("work")
            .is_err()
    );
    assert!(
        select_workspace(&user, &child, None)
            .unwrap()
            .explicit_store("work")
            .is_ok()
    );
    std::fs::write(
        &user,
        subtree_project_config(
            &parent,
            &format!(",excluded_roots=[{}]", path_list_toml(&[link])),
        ),
    )
    .unwrap();
    assert!(
        select_workspace(&user, &child, None)
            .unwrap()
            .explicit_store("work")
            .is_err()
    );
    std::fs::write(&user, subtree_project_config(&parent, "")).unwrap();
    let escape = parent.join("escape");
    symlink(&outside, &escape).unwrap();
    assert!(!store_names(&select_workspace(&user, &escape, None).unwrap()).contains(&"work"));
    let outside_alias = outside.join("alias");
    symlink(&child, &outside_alias).unwrap();
    assert!(
        !store_names(&select_workspace(&user, &outside_alias, None).unwrap()).contains(&"work")
    );
    // Each origin can use a different explicit grant, without requiring an
    // implicit escape: lexical outside path has its own ordinary-root grant.
    let ordinary = parent.join("ordinary");
    std::fs::create_dir(&ordinary).unwrap();
    let ordinary_alias = outside.join("ordinary-alias");
    symlink(&ordinary, &ordinary_alias).unwrap();
    std::fs::write(
        &user,
        subtree_project_config(&parent, &format!(",roots=[{}]", path_list_toml(&[outside]))),
    )
    .unwrap();
    assert!(
        store_names(&select_workspace(&user, &ordinary_alias, None).unwrap()).contains(&"work")
    );
}

#[test]
fn empty_subtree_selectors_preserve_old_snapshot_shape_and_new_grants_are_frozen() {
    let dir = tempfile::tempdir().unwrap();
    let base = config(&entry("personal", FIRST, "workspace='any'"));
    let before = select(&base, &context(dir.path())).unwrap();
    let text = format!(
        "schema='mneme.config.v1'\n{}applicability.record={{subtree_roots=[]}}\n",
        entry("personal", FIRST, "workspace='any',subtree_roots=[]")
    );
    let after = select(&Config::parse(&text).unwrap(), &context(dir.path())).unwrap();
    assert_eq!(before, after);
    assert!(
        !serde_json::to_string(&after)
            .unwrap()
            .contains("subtree_roots")
    );
    let user = dir.path().join("user.toml");
    std::fs::write(
        &user,
        format!(
            "schema='mneme.config.v1'\n{}",
            entry(
                "work",
                THIRD,
                &format!(
                    "workspace='project',roots=[{}]",
                    path_list_toml(&[dir.path().to_owned()])
                )
            )
        ),
    )
    .unwrap();
    let retained = select_workspace(&user, dir.path(), None).unwrap();
    std::fs::write(
        &user,
        format!(
            "schema='mneme.config.v1'\n{}",
            entry(
                "work",
                THIRD,
                &format!(
                    "workspace='project',subtree_roots=[{}]",
                    path_list_toml(&[dir.path().to_owned()])
                )
            )
        ),
    )
    .unwrap();
    assert!(retained.revalidate_exact().is_err());
    let selected = select_workspace(&user, dir.path(), None).unwrap();
    assert_eq!(store_names(&retained), store_names(&selected));
    assert_eq!(retained.active[0].eligible, selected.active[0].eligible);
    assert_ne!(retained.digest, selected.digest);
    selected.revalidate_exact().unwrap();
}

const PREFERENCE_CAPTURE: &str = "intent='collaboration-preferences',kinds=['lesson'],evidence_kinds=['user_statement'],required_tags=['collaboration-preference'],allow_associations=false,max_summary_bytes=384,max_body_bytes=1024";

fn capture_config(policy: &str) -> String {
    format!(
        "schema='mneme.config.v1'\n{}capture={{{policy}}}",
        entry("not_personal", FIRST, "workspace='any'")
    )
}

#[test]
fn capture_intent_defaults_to_ordinary_without_name_or_tag_inference() {
    let cfg = config(&entry("personal", FIRST, "workspace='any'"));
    assert_eq!(cfg.stores["personal"].capture, CapturePolicy::default());
    assert_eq!(CapturePolicy::default().intent, CaptureIntent::Ordinary);
    let encoded = serde_json::to_value(&cfg.stores["personal"].capture).unwrap();
    assert!(encoded.get("intent").is_none());
    assert_eq!(
        serde_json::from_value::<CapturePolicy>(encoded).unwrap(),
        CapturePolicy::default()
    );

    // The historical tag and narrow structure alone do not confer an intent.
    for policy in [
        PREFERENCE_CAPTURE.replace("intent='collaboration-preferences',", ""),
        PREFERENCE_CAPTURE.replace("collaboration-preferences", "ordinary"),
    ] {
        let cfg = Config::parse(&capture_config(&policy)).unwrap();
        assert_eq!(
            cfg.stores["not_personal"].capture.intent,
            CaptureIntent::Ordinary
        );
        assert!(
            serde_json::to_value(&cfg.stores["not_personal"].capture)
                .unwrap()
                .get("intent")
                .is_none()
        );
    }
}

#[test]
fn collaboration_preference_intent_roundtrips_with_extra_ordinary_tags_and_narrower_bounds() {
    let policy = PREFERENCE_CAPTURE
        .replace(
            "['collaboration-preference']",
            "['collaboration-preference','writing']",
        )
        .replace("=384", "=128")
        .replace("=1024", "=512");
    let cfg = Config::parse(&capture_config(&policy)).unwrap();
    let capture = &cfg.stores["not_personal"].capture;
    assert_eq!(capture.intent, CaptureIntent::CollaborationPreferences);
    let json = serde_json::to_value(capture).unwrap();
    assert_eq!(json["intent"], "collaboration-preferences");
    assert_eq!(
        serde_json::from_value::<CapturePolicy>(json).unwrap(),
        *capture
    );
    let round: Config = serde_json::from_slice(&serde_json::to_vec(&cfg).unwrap()).unwrap();
    round.validate().unwrap();
    assert_eq!(round, cfg);
}

#[test]
fn cross_workspace_intent_roundtrips_with_ordinary_structural_defaults_and_narrowing() {
    for policy in [
        "intent='cross-workspace'",
        "intent='cross-workspace',kinds=['episode','possibility'],evidence_kinds=['tool_result'],required_tags=['rust'],allow_associations=false,max_summary_bytes=400,max_body_bytes=1500",
    ] {
        let cfg = Config::parse(&capture_config(policy)).unwrap();
        let capture = &cfg.stores["not_personal"].capture;
        assert_eq!(capture.intent, CaptureIntent::CrossWorkspace);
        let ordinary = Config::parse(&capture_config(
            &policy.replace("cross-workspace", "ordinary"),
        ))
        .unwrap();
        let mut ordinary_capture = ordinary.stores["not_personal"].capture.clone();
        ordinary_capture.intent = CaptureIntent::CrossWorkspace;
        assert_eq!(*capture, ordinary_capture);
        let json = serde_json::to_value(capture).unwrap();
        assert_eq!(json["intent"], "cross-workspace");
        assert_eq!(
            serde_json::from_value::<CapturePolicy>(json).unwrap(),
            *capture
        );
        let round: Config = serde_json::from_slice(&serde_json::to_vec(&cfg).unwrap()).unwrap();
        round.validate().unwrap();
        assert_eq!(round, cfg);
        assert_eq!(Config::parse_overlay(&capture_config(policy)).unwrap(), cfg);
    }

    for invalid in [
        "kinds=[]",
        "evidence_kinds=[]",
        "kinds=['lesson','lesson']",
        "required_tags=['core']",
        "max_summary_bytes=513",
        "max_body_bytes=2049",
        "allow_associations='yes'",
    ] {
        for intent in ["ordinary", "cross-workspace"] {
            let text = capture_config(&format!("intent='{intent}',{invalid}"));
            assert!(Config::parse(&text).is_err(), "{intent}: {invalid}");
            assert!(Config::parse_overlay(&text).is_err(), "{intent}: {invalid}");
        }
    }
}

#[test]
fn capture_intent_rejects_unknown_values_wrong_types_and_incompatible_policies() {
    for intent in [
        serde_json::json!("secret-token"),
        serde_json::json!(null),
        serde_json::json!(true),
        serde_json::json!(7),
        serde_json::json!([]),
        serde_json::json!({}),
    ] {
        assert!(
            serde_json::from_value::<CapturePolicy>(serde_json::json!({
                "intent": intent,
            }))
            .is_err()
        );
    }
    for bad_intent in ["'secret-token'", "true", "7", "[]", "{}"] {
        let text =
            capture_config(&PREFERENCE_CAPTURE.replace("'collaboration-preferences'", bad_intent));
        for result in [Config::parse(&text), Config::parse_overlay(&text)] {
            let error = result.unwrap_err();
            assert!(!error.to_string().contains("secret-token"));
        }
    }
    for (from, to) in [
        ("kinds=['lesson']", "kinds=['episode']"),
        ("kinds=['lesson']", "kinds=['lesson','episode']"),
        (
            "evidence_kinds=['user_statement']",
            "evidence_kinds=['assistant_assertion']",
        ),
        (
            "evidence_kinds=['user_statement']",
            "evidence_kinds=['user_statement','tool_result']",
        ),
        ("allow_associations=false", "allow_associations=true"),
        ("max_summary_bytes=384", "max_summary_bytes=385"),
        ("max_body_bytes=1024", "max_body_bytes=1025"),
        (
            "required_tags=['collaboration-preference']",
            "required_tags=[]",
        ),
        (
            "required_tags=['collaboration-preference']",
            "required_tags=['writing']",
        ),
    ] {
        let text = capture_config(&PREFERENCE_CAPTURE.replace(from, to));
        assert!(Config::parse(&text).is_err(), "{from} -> {to}");
        assert!(Config::parse_overlay(&text).is_err(), "{from} -> {to}");
    }
    // Selecting the intent does not silently choose its required structural policy.
    assert!(Config::parse(&capture_config("intent='collaboration-preferences'")).is_err());

    // Programmatic construction cannot bypass admission, even for an inactive owner.
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::parse(&capture_config(PREFERENCE_CAPTURE)).unwrap();
    let store = cfg.stores.get_mut("not_personal").unwrap();
    store.active.enabled = false;
    store.capture.allow_associations = true;
    assert!(select(&cfg, &context(dir.path())).is_err());
}

#[test]
fn capture_intent_is_selected_and_fenced_without_changing_authority() {
    for (policy, intent) in [
        (PREFERENCE_CAPTURE, CaptureIntent::CollaborationPreferences),
        ("intent='cross-workspace'", CaptureIntent::CrossWorkspace),
    ] {
        assert_capture_intent_selected_and_fenced(policy, intent);
    }
}

fn assert_capture_intent_selected_and_fenced(policy: &str, intent: CaptureIntent) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::parse(&capture_config(policy)).unwrap();
    let selected = select(&cfg, &context(dir.path())).unwrap();
    assert_eq!(selected.active[0].store.capture.intent, intent);
    assert_eq!(
        selected.stores[0].store.capture,
        cfg.stores["not_personal"].capture
    );
    assert!(selected.active[0].eligible.record);
    let round: SelectionSnapshot =
        serde_json::from_slice(&serde_json::to_vec(&selected).unwrap()).unwrap();
    round.verify_digest().unwrap();
    assert_eq!(selected, round);

    let mut ordinary = cfg.clone();
    ordinary
        .stores
        .get_mut("not_personal")
        .unwrap()
        .capture
        .intent = CaptureIntent::Ordinary;
    let ordinary = select(&ordinary, &context(dir.path())).unwrap();
    assert_eq!(selected.active[0].eligible, ordinary.active[0].eligible);
    assert_ne!(selected.digest, ordinary.digest);

    // Each duplicated policy in the frozen snapshot is independently fenced.
    for field in ["config", "stores", "active"] {
        let mut changed = selected.clone();
        let capture = match field {
            "config" => {
                &mut changed
                    .config
                    .stores
                    .get_mut("not_personal")
                    .unwrap()
                    .capture
            }
            "stores" => &mut changed.stores[0].store.capture,
            "active" => &mut changed.active[0].store.capture,
            _ => unreachable!(),
        };
        capture.intent = CaptureIntent::Ordinary;
        assert!(changed.verify_digest().is_err(), "{field}");
    }

    let mut no_write = cfg.clone();
    no_write
        .stores
        .get_mut("not_personal")
        .unwrap()
        .permissions
        .write = false;
    let selected = select(&no_write, &context(dir.path())).unwrap();
    assert!(!selected.active[0].eligible.record);
    assert!(
        selected.active[0]
            .require_operation(Operation::Record)
            .is_err()
    );
    let mut isolated = context(dir.path());
    isolated.profile = "isolated".into();
    assert!(select(&cfg, &isolated).unwrap().active.is_empty());
    let mut scoped = cfg;
    scoped
        .stores
        .get_mut("not_personal")
        .unwrap()
        .applicability
        .record
        .roots = vec![dir.path().join("elsewhere")];
    assert!(
        !select(&scoped, &context(dir.path())).unwrap().active[0]
            .eligible
            .record
    );
}

#[test]
fn changing_capture_intent_invalidates_filesystem_selection() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("memory.toml");
    for original in ["ordinary", "collaboration-preferences", "cross-workspace"] {
        for changed in ["ordinary", "collaboration-preferences", "cross-workspace"] {
            if original == changed {
                continue;
            }
            // Keep every structural field identical and valid for all intents.
            std::fs::write(
                &user,
                capture_config(&PREFERENCE_CAPTURE.replace("collaboration-preferences", original)),
            )
            .unwrap();
            let selected = select_workspace(&user, dir.path(), None).unwrap();
            selected.revalidate_exact().unwrap();
            std::fs::write(
                &user,
                capture_config(&PREFERENCE_CAPTURE.replace("collaboration-preferences", changed)),
            )
            .unwrap();
            assert!(
                selected.revalidate_exact().is_err(),
                "{original} -> {changed}"
            );
            let fresh = select_workspace(&user, dir.path(), None).unwrap();
            fresh.revalidate_exact().unwrap();
            assert_ne!(selected.digest, fresh.digest);
        }
    }
}

#[test]
fn identity_owner_and_tag_guide_are_validated_without_contact() {
    let base = format!(
        "schema='mneme.config.v1'\n{}",
        entry("memory", FIRST, "workspace='any'")
    );
    for (from, to) in [
        (FIRST, "01m3cv5ezp4hwak56yjcmzqg2y"),
        ("http://127.0.0.1:1", "https://secret-token@example.com"),
        (
            "http://127.0.0.1:1",
            "https://example.com/?token=secret-token",
        ),
        ("http://127.0.0.1:1", "file:///tmp/db"),
    ] {
        assert!(Config::parse(&base.replace(from, to)).is_err());
    }
    assert!(
        Config::parse(&format!(
            "{base}{}",
            entry("alias", FIRST, "workspace='any'")
        ))
        .is_err()
    );
    let remote=base.replace("owner={url='http://127.0.0.1:1',db='project'}","owner={url='ssh://mica@example.com:2222',ssh_mcp_port=18765,db='project',path='/home/mica/db',token_env='MNEME_TOKEN'}");
    assert!(Config::parse(&remote).is_ok());
    assert!(Config::parse(&remote.replace("/home/mica/db", r"C:\Users\mica\db")).is_ok());
}
#[test]
fn overlay_same_schema_adds_project_roots_and_inherited_library_refs() {
    let dir = tempfile::tempdir().unwrap();
    let user = config(&format!(
        "{}\n[libraries.reference]\ncatalog='{}'",
        entry("personal", FIRST, "workspace='any'"),
        dir.path().join("catalog.json").display()
    ));
    let overlay=Config::parse_overlay(&format!("schema='mneme.config.v1'\n[workspace]\nallow_stores=['personal','work']\n{}cache={{library='reference'}}",entry("work",SECOND,"workspace='project'"))).unwrap();
    let merged = user.clone().merge_project(overlay, dir.path()).unwrap();
    assert_eq!(
        merged.stores["work"].active.roots,
        vec![dir.path().to_owned()]
    );
    assert!(
        select(&merged, &context(dir.path()))
            .unwrap()
            .project
            .configured
    );
    assert!(
        user.clone()
            .merge_project(
                config(&entry("personal", SECOND, "workspace='project'")),
                dir.path()
            )
            .is_err()
    );
    assert!(
        user.merge_project(
            config(&entry("alias", FIRST, "workspace='project'")),
            dir.path()
        )
        .is_err()
    );
}
fn write_user(root: &Path, _cfg: &Config) -> PathBuf {
    let p = root.join("user.toml");
    std::fs::write(
        &p,
        format!(
            "schema='mneme.config.v1'\n{}{}",
            entry("personal", FIRST, "workspace='any'"),
            entry("ordinary", SECOND, "workspace='no-project'")
        ),
    )
    .unwrap();
    p
}
#[test]
fn filesystem_selection_is_metadata_only_owner_unreachable_does_not_change_activation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    std::fs::create_dir_all(root.join(".mneme")).unwrap();
    let user = write_user(dir.path(), &config(""));
    std::fs::write(
        root.join(".mneme/memory.toml"),
        format!(
            "schema='mneme.config.v1'\n{}",
            entry("work", THIRD, "workspace='project'")
        ),
    )
    .unwrap();
    let s = select_workspace(&user, &root, None).unwrap();
    assert_eq!(store_names(&s), vec!["personal", "work"]);
    assert_eq!(s.sources.len(), 2);
    assert!(s.active[1].eligible.record);
    s.verify_digest().unwrap();
}
#[test]
fn malformed_or_legacy_project_presence_never_means_misc() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    std::fs::create_dir_all(root.join(".mneme")).unwrap();
    let user = write_user(dir.path(), &config(""));
    let s = select_workspace(&user, &root, None).unwrap();
    assert_eq!(store_names(&s), vec!["personal"]);
    assert!(s.project.configured);
    std::fs::write(root.join(".mneme/memory.toml"), "schema='wrong'").unwrap();
    assert!(select_workspace(&user, &root, None).is_err());
}
#[test]
fn nested_git_or_mneme_boundary_fences_inherited_project_not_global_core() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    let nested = root.join("nested");
    std::fs::create_dir_all(root.join(".mneme")).unwrap();
    std::fs::create_dir(&nested).unwrap();
    let user = write_user(dir.path(), &config(""));
    std::fs::write(
        root.join(".mneme/memory.toml"),
        format!(
            "schema='mneme.config.v1'\n{}",
            entry("work", THIRD, "workspace='project'")
        ),
    )
    .unwrap();
    assert_eq!(
        store_names(&select_workspace(&user, &nested, None).unwrap()),
        vec!["personal", "work"]
    );
    std::fs::create_dir(nested.join(".git")).unwrap();
    let s = select_workspace(&user, &nested, None).unwrap();
    assert_eq!(store_names(&s), vec!["personal"]);
    assert!(s.project.configured);
    assert!(s.active[0].eligible.core);
    std::fs::remove_dir(nested.join(".git")).unwrap();
    std::fs::create_dir(nested.join(".mneme")).unwrap();
    assert_eq!(
        store_names(&select_workspace(&user, &nested, None).unwrap()),
        vec!["personal"]
    );
}
#[test]
fn retained_v1_v2_privacy_and_library_boundaries_remain_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    std::fs::create_dir_all(root.join(".mneme")).unwrap();
    let user = write_user(dir.path(), &config(""));
    let profile = root.join(".mneme/profile.json");
    std::fs::write(
        &profile,
        r#"{"schema":"mneme.profile.v2","mode":"default","stores":[]}"#,
    )
    .unwrap();
    assert!(
        select_workspace(&user, &root, None)
            .unwrap()
            .active
            .is_empty()
    );
    std::fs::write(
        &profile,
        r#"{"schema":"mneme.profile.v2","mode":"isolated","stores":["personal"]}"#,
    )
    .unwrap();
    assert_eq!(
        store_names(&select_workspace(&user, &root, None).unwrap()),
        vec!["personal"]
    );
    std::fs::write(
        &profile,
        format!(
            r#"{{"schema":"mneme.profile.v1","mode":"private","library_config":"{}"}}"#,
            dir.path().join("library.json").display()
        ),
    )
    .unwrap();
    let s = select_workspace(&user, &root, None).unwrap();
    assert!(s.active.is_empty());
    assert!(s.project.legacy_library.is_some());
    std::fs::write(
        &profile,
        r#"{"schema":"mneme.profile.v1","mode":"default","mode":"private"}"#,
    )
    .unwrap();
    assert!(select_workspace(&user, &root, None).is_err());
}
#[test]
fn codex_structural_routing_presence_is_frozen_but_prose_is_not_enrollment() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    std::fs::create_dir_all(root.join(".codex")).unwrap();
    let user = write_user(dir.path(), &config(""));
    let codex = root.join(".codex/config.toml");
    std::fs::write(
        &codex,
        "developer_instructions='mneme is mentioned in prose'",
    )
    .unwrap();
    let s = select_workspace(&user, &root, None).unwrap();
    assert!(!s.project.configured);
    assert_eq!(store_names(&s), vec!["ordinary", "personal"]);
    std::fs::write(&codex, "[mcp_servers.mneme]\nurl='http://127.0.0.1:1'").unwrap();
    let t = select_workspace(&user, &root, None).unwrap();
    assert!(t.project.configured);
    assert_eq!(store_names(&t), vec!["personal"]);
    assert_ne!(s.digest, t.digest);
    std::fs::write(&codex, "garbage=!").unwrap();
    assert!(select_workspace(&user, &root, None).is_err());
}
#[test]
fn file_admission_refuses_oversize_and_directory() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Config::load(dir.path()).is_err());
    let file = dir.path().join("big");
    std::fs::write(&file, vec![b'x'; MAX_CONFIG_BYTES + 1]).unwrap();
    assert!(Config::load(&file).is_err());
}
#[cfg(unix)]
#[test]
fn config_symlink_is_allowed_but_workspace_symlink_and_alias_exclusion_stay_fenced() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let user = write_user(dir.path(), &config(""));
    let alias = dir.path().join("alias.toml");
    symlink(&user, &alias).unwrap();
    assert_eq!(Config::load(&alias).unwrap(), Config::load(&user).unwrap());
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    symlink(dir.path(), work.join(".mneme")).unwrap();
    assert!(select_workspace(&user, &work, None).is_err());
    let lexical = dir.path().join("private");
    let actual = dir.path().join("actual");
    std::fs::create_dir(&lexical).unwrap();
    std::fs::create_dir(&actual).unwrap();
    let link = lexical.join("alias");
    symlink(&actual, &link).unwrap();
    let mut cfg = config(&entry("personal", FIRST, "workspace='any'"));
    cfg.stores
        .get_mut("personal")
        .unwrap()
        .active
        .excluded_roots = vec![lexical.clone()];
    let mut ctx = context(&actual);
    ctx.origins = vec![link, actual.clone()];
    let s = select(&cfg, &ctx).unwrap();
    assert!(s.active.is_empty());
    assert_eq!(s.origins.len(), 2);
}

#[cfg(unix)]
#[test]
fn relative_chained_declarative_symlinks_keep_logical_project_scope() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let work = root.join("work");
    std::fs::create_dir_all(work.join(".mneme")).unwrap();
    std::fs::create_dir(root.join("generated")).unwrap();
    let user = root.join("user.toml");
    std::fs::write(
        &user,
        format!(
            "schema='mneme.config.v1'{}",
            entry("ordinary", FIRST, "workspace='no-project'")
        ),
    )
    .unwrap();
    symlink(user.file_name().unwrap(), root.join("user-link")).unwrap();
    symlink("user-link", root.join("memory.toml")).unwrap();
    let overlay = root.join("generated/project.toml");
    std::fs::write(
        &overlay,
        format!(
            "schema='mneme.config.v1'{}",
            entry("work", SECOND, "workspace='project'")
        ),
    )
    .unwrap();
    symlink("project.toml", root.join("generated/link")).unwrap();
    symlink("../../generated/link", work.join(".mneme/memory.toml")).unwrap();

    let snapshot = select_workspace(&root.join("memory.toml"), &work, None).unwrap();
    assert_eq!(snapshot.project.root.as_ref(), Some(&work));
    assert_eq!(snapshot.project.overlay.as_ref(), Some(&overlay));
    assert_eq!(
        snapshot.config.stores["work"].active.roots,
        vec![work.clone()]
    );
    assert_eq!(store_names(&snapshot), vec!["work"]);
    assert_eq!(
        snapshot.request.as_ref().unwrap().user_path.as_ref(),
        Some(&root.join("memory.toml"))
    );
    snapshot.revalidate_exact().unwrap();

    // Explicitly choosing the overlay alias must still avoid merging the same
    // owner twice, and infer scope from its .mneme location, not its target.
    let direct = select_workspace(&work.join(".mneme/memory.toml"), &work, None).unwrap();
    assert_eq!(direct.config.stores["work"].active.roots, vec![work]);
    assert_eq!(store_names(&direct), vec!["work"]);
    direct.revalidate_exact().unwrap();
}

#[cfg(unix)]
#[test]
fn declarative_symlink_retarget_or_edit_invalidates_frozen_admission() {
    use std::os::unix::fs::symlink;
    for same_bytes in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.toml");
        let new = dir.path().join("new.toml");
        let link = dir.path().join("memory.toml");
        std::fs::write(&old, "schema='mneme.config.v1'").unwrap();
        std::fs::write(
            &new,
            if same_bytes {
                "schema='mneme.config.v1'"
            } else {
                "schema='mneme.config.v1'\n[background]\nenabled=false"
            },
        )
        .unwrap();
        symlink(&old, &link).unwrap();
        let snapshot = select_workspace(&link, dir.path(), None).unwrap();
        snapshot.revalidate_exact().unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&new, &link).unwrap();
        assert!(
            snapshot.revalidate_exact().is_err(),
            "retarget invalidates even identical bytes: {same_bytes}"
        );
        let fresh = select_workspace(&link, dir.path(), None).unwrap();
        fresh.revalidate_exact().unwrap();
        std::fs::write(
            &new,
            "schema='mneme.config.v1'\n[workspace]\nprofile='isolated'",
        )
        .unwrap();
        assert!(fresh.revalidate_exact().is_err());
    }
}

#[cfg(unix)]
#[test]
fn invalid_declarative_symlinks_fail_closed_and_keep_diagnostics_sanitized() {
    use std::os::unix::fs::symlink;
    for kind in [
        "dangling",
        "loop",
        "directory",
        "fifo",
        "oversized",
        "invalid",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("private-target-secret");
        let user = dir.path().join("memory.toml");
        match kind {
            "dangling" => {}
            "loop" => symlink(&user, &target).unwrap(),
            "directory" => std::fs::create_dir(&target).unwrap(),
            "fifo" => {
                use std::os::unix::ffi::OsStrExt;
                let path = std::ffi::CString::new(target.as_os_str().as_bytes()).unwrap();
                // SAFETY: this live CString supplies a readable NUL-terminated
                // path for the whole call; mkfifo neither writes nor retains it.
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
            "oversized" => std::fs::write(&target, vec![b'x'; MAX_CONFIG_BYTES + 1]).unwrap(),
            "invalid" => std::fs::write(&target, "secret='private-contents-secret'").unwrap(),
            _ => unreachable!(),
        }
        symlink(&target, &user).unwrap();
        for result in [
            Config::load(&user).map(|_| ()),
            select_workspace(&user, dir.path(), None).map(|_| ()),
        ] {
            let message = result.expect_err(kind).to_string();
            assert!(!message.contains("private-target-secret"), "{message}");
            assert!(!message.contains("private-contents-secret"), "{message}");
        }
        // A present invalid overlay cannot disappear into misc fallback either.
        let work = dir.path().join("work");
        std::fs::create_dir_all(work.join(".mneme")).unwrap();
        symlink(&target, work.join(".mneme/memory.toml")).unwrap();
        assert!(discover_workspace(None, &work, None).is_err(), "{kind}");
    }
}

#[cfg(unix)]
#[test]
fn declarative_symlinks_do_not_relax_legacy_privacy_markers() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".mneme")).unwrap();
    let user = write_user(
        dir.path(),
        &config(&entry("personal", FIRST, "workspace='any'")),
    );
    symlink(&user, dir.path().join("memory.toml")).unwrap();
    let privacy = dir.path().join("private.json");
    std::fs::write(
        &privacy,
        r#"{"schema":"mneme.profile.v2","mode":"isolated","stores":[]}"#,
    )
    .unwrap();
    symlink(&privacy, work.join(".mneme/profile.json")).unwrap();
    assert!(select_workspace(&dir.path().join("memory.toml"), &work, None).is_err());
    std::fs::remove_file(work.join(".mneme/profile.json")).unwrap();
    std::fs::copy(&privacy, work.join(".mneme/profile.json")).unwrap();
    let snapshot = select_workspace(&dir.path().join("memory.toml"), &work, None).unwrap();
    assert_eq!(snapshot.profile, "isolated");
    assert!(snapshot.active.is_empty());
    assert!(snapshot.explicit_store("personal").is_err());
}
#[test]
fn preference_capture_recipe_is_executable_policy_not_prose() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&format!(
        "{}capture={{{PREFERENCE_CAPTURE}}}\nretrieval={{tags=['collaboration-preference']}}",
        entry("not_global", FIRST, "workspace='any'")
    ));
    let s = select(&cfg, &context(dir.path())).unwrap();
    let policy = &s.active[0].store.capture;
    assert_eq!(policy.intent, CaptureIntent::CollaborationPreferences);
    assert_eq!(policy.kinds, vec![CaptureKind::Lesson]);
    assert_eq!(policy.evidence_kinds, vec![EvidenceKind::UserStatement]);
    assert!(!policy.allow_associations);
    assert_eq!(policy.max_body_bytes, 1024);
    assert_eq!(
        s.active[0].store.retrieval.as_ref().unwrap().tags,
        vec!["collaboration-preference"]
    );
}

#[test]
fn overlay_narrows_background_and_workspace_never_widens_parent() {
    let dir = tempfile::tempdir().unwrap();
    let mut user = config(&entry("personal", FIRST, "workspace='any'"));
    user.workspace.profile = Some("private".into());
    user.workspace.allow_stores = Some(vec!["personal".into()]);
    let overlay=Config::parse_overlay(&format!("schema='mneme.config.v1'\n[background]\nenabled=false\n[workspace]\nprofile='default'\nallow_stores=['personal','work']\n{}",entry("work",SECOND,"workspace='project'"))).unwrap();
    let merged = user.merge_project(overlay, dir.path()).unwrap();
    assert!(!merged.background.enabled);
    assert_eq!(merged.workspace.profile.as_deref(), Some("private"));
    assert_eq!(merged.workspace.allow_stores, Some(vec!["personal".into()]));
    let s = select(&merged, &context(dir.path())).unwrap();
    assert_eq!(store_names(&s), vec!["personal"]);
    assert!(s.active[0].eligible.core);
    assert!(!s.active[0].eligible.record);
    let mut off = merged.clone();
    off.background.enabled = false;
    let overlay =
        Config::parse_overlay("schema='mneme.config.v1'\n[background]\nenabled=true").unwrap();
    assert!(
        !off.merge_project(overlay, dir.path())
            .unwrap()
            .background
            .enabled
    );
}
#[test]
fn duplicate_and_null_legacy_selectors_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    std::fs::create_dir_all(root.join(".mneme")).unwrap();
    let user = write_user(dir.path(), &config(""));
    for invalid in [
        r#"{"schema":"mneme.profile.v2","mode":"default","stores":null}"#,
        r#"{"schema":"mneme.profile.v1","mode":"default","library_config":null}"#,
    ] {
        std::fs::write(root.join(".mneme/profile.json"), invalid).unwrap();
        assert!(select_workspace(&user, &root, None).is_err());
    }
}

#[test]
fn aggregate_store_and_replica_bounds_are_pre_admission() {
    let mut text = String::from("schema='mneme.config.v1'\n");
    for n in 1..=65 {
        text += &entry(
            &format!("s{n}"),
            &ulid::Ulid::from(n as u128).to_string(),
            "workspace='any'",
        );
    }
    assert!(Config::parse(&text).is_err());
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&format!(
        "{}\n[libraries.reference]\ncatalog='{}'",
        entry("memory", FIRST, "workspace='any'"),
        dir.path().join("catalog.json").display()
    ));
    for n in 0..17 {
        cfg.libraries.get_mut("reference").unwrap().replicas.insert(
            format!("r{n}"),
            Replica {
                url: "http://127.0.0.1:1".into(),
                ssh_mcp_port: None,
                token_env: None,
            },
        );
    }
    assert!(select(&cfg, &context(dir.path())).is_err());
}
#[test]
fn library_admission_is_inventory_not_automatic_store_activation() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&format!(
        "{}\n[libraries.reference]\ncatalog='{}'",
        entry("memory", FIRST, "workspace='any'"),
        dir.path().join("catalog.json").display()
    ));
    let mut ctx = context(dir.path());
    assert_eq!(select(&cfg, &ctx).unwrap().libraries.len(), 1);
    ctx.profile = "isolated".into();
    assert!(select(&cfg, &ctx).unwrap().libraries.is_empty());
    cfg.workspace.allow_libraries = Some(vec!["reference".into()]);
    let s = select(&cfg, &ctx).unwrap();
    assert_eq!(s.libraries.len(), 1);
    assert!(s.active.is_empty());
    ctx.profile = "default".into();
    cfg.workspace.allow_libraries = Some(vec![]);
    assert!(select(&cfg, &ctx).unwrap().libraries.is_empty());
}
#[test]
fn ambiguous_default_requires_deliberate_owner_not_first_match() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&format!(
        "{}{}",
        entry("one", FIRST, "workspace='no-project'"),
        entry("two", SECOND, "workspace='no-project'")
    ));
    assert!(select(&cfg, &context(dir.path())).is_err());
    cfg.read_default = "two".into();
    assert_eq!(
        select(&cfg, &context(dir.path()))
            .unwrap()
            .read_default
            .as_deref(),
        Some("two")
    );
}

#[test]
fn explicit_project_config_is_not_merged_with_itself() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".mneme")).unwrap();
    let file = dir.path().join(".mneme/memory.toml");
    std::fs::write(
        &file,
        format!(
            "schema='mneme.config.v1'\n[background]\nenabled=false\n{}",
            entry("work", FIRST, "workspace='project'")
        ),
    )
    .unwrap();
    let s = select_workspace(&file, dir.path(), None).unwrap();
    assert_eq!(store_names(&s), vec!["work"]);
    assert!(!s.active[0].eligible.record);
    assert!(s.active[0].eligible.core);
}

#[test]
fn frozen_product_request_revalidation_refuses_permission_edits() {
    let dir = tempfile::tempdir().unwrap();
    let user = write_user(dir.path(), &config(""));
    let snapshot = select_workspace(&user, dir.path(), None).unwrap();
    assert!(snapshot.request.is_some());
    snapshot.revalidate_exact().unwrap();
    let cloned: SelectionSnapshot =
        serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
    cloned.revalidate_exact().unwrap();
    let text = std::fs::read_to_string(&user).unwrap().replace(
        "active={workspace='any'}",
        "active={workspace='any'}\npermissions={read=true,write=false}",
    );
    std::fs::write(&user, text).unwrap();
    assert!(snapshot.revalidate_exact().is_err());
    assert!(cloned.revalidate_exact().is_err());
}
#[test]
fn frozen_request_refuses_new_nested_profile_and_new_native_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let nested = work.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let user = dir.path().join("user.toml");
    std::fs::write(
        &user,
        format!(
            "schema='mneme.config.v1'\n{}{}",
            entry("personal", FIRST, "workspace='any'"),
            entry(
                "work",
                SECOND,
                &format!("workspace='project',roots=['{}']", work.display())
            )
        ),
    )
    .unwrap();
    let snapshot = select_workspace(&user, &nested, None).unwrap();
    assert_eq!(store_names(&snapshot), vec!["personal", "work"]);
    snapshot.revalidate_exact().unwrap();
    std::fs::create_dir(nested.join(".mneme")).unwrap();
    std::fs::write(
        nested.join(".mneme/profile.json"),
        r#"{"schema":"mneme.profile.v2","mode":"isolated","stores":[]}"#,
    )
    .unwrap();
    assert!(snapshot.revalidate_exact().is_err());
    std::fs::remove_file(nested.join(".mneme/profile.json")).unwrap();
    std::fs::write(
        nested.join(".mneme/memory.toml"),
        "schema='mneme.config.v1'\n[background]\nenabled=false",
    )
    .unwrap();
    assert!(snapshot.revalidate_exact().is_err());
}
#[test]
fn pure_select_has_explicit_immutable_not_filesystem_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let snapshot = select(
        &config(&entry("memory", FIRST, "workspace='any'")),
        &context(dir.path()),
    )
    .unwrap();
    assert!(snapshot.request.is_none());
    std::fs::create_dir(dir.path().join(".mneme")).unwrap();
    snapshot.revalidate_exact().unwrap();
}

#[test]
fn tag_guide_remains_a_canonical_summary_node_and_hippocampus_is_not_a_node_selector() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&format!(
        "{}guidance={{tag_guide={{id='{}',content='summary'}}}}",
        entry("memory", FIRST, "workspace='any'"),
        SECOND
    ));
    let selected = select(&cfg, &context(dir.path())).unwrap();
    assert_eq!(
        selected.active[0]
            .store
            .guidance
            .tag_guide
            .as_ref()
            .unwrap()
            .id,
        SECOND
    );
    assert!(
        Config::parse(&format!(
            "schema='mneme.config.v1'\n{}guidance={{hippocampus={{id='{}',content='body'}}}}",
            entry("memory", FIRST, "workspace='any'"),
            SECOND
        ))
        .is_err()
    );
}

fn scoped_any_config(roots: &[&Path]) -> String {
    let roots = roots
        .iter()
        .map(|p| format!("'{}'", p.display()))
        .collect::<Vec<_>>()
        .join(",");
    let mut text = format!(
        "schema='mneme.config.v1'\n{}",
        entry("personal", FIRST, "workspace='any'")
    );
    for operation in ["context", "recall", "record", "maintenance"] {
        text += &format!("applicability.{operation}={{roots=[{roots}]}}\n");
    }
    text
}
fn assert_scoped_background(s: &SelectionSnapshot, allowed: bool) {
    assert_eq!(store_names(s), vec!["personal"]);
    let store = &s.active[0];
    assert!(
        store.eligible.core && store.eligible.read && store.eligible.write,
        "broad core and manual grants stay available"
    );
    for (operation, eligible) in [
        (Operation::Context, store.eligible.context),
        (Operation::Recall, store.eligible.recall),
        (Operation::Record, store.eligible.record),
        (Operation::Maintenance, store.eligible.maintenance),
    ] {
        assert_eq!(eligible, allowed, "{operation:?}");
        assert_eq!(
            store.require_operation(operation).is_ok(),
            allowed,
            "{operation:?}"
        );
    }
}
#[test]
fn positive_operation_roots_fence_nested_git_and_mneme_but_explicit_children_opt_in() {
    for marker in [".git", ".mneme"] {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("workshop");
        let ordinary = parent.join("ordinary");
        let nested = parent.join("projects/child");
        std::fs::create_dir_all(&ordinary).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        let user = dir.path().join("user.toml");
        std::fs::write(&user, scoped_any_config(&[&parent])).unwrap();
        assert_scoped_background(&select_workspace(&user, &parent, None).unwrap(), true);
        assert_scoped_background(&select_workspace(&user, &ordinary, None).unwrap(), true);
        std::fs::create_dir(nested.join(marker)).unwrap();
        if marker == ".mneme" {
            std::fs::write(
                nested.join(".mneme/memory.toml"),
                "schema='mneme.config.v1'",
            )
            .unwrap();
        }
        let denied = select_workspace(&user, &nested, None).unwrap();
        assert_scoped_background(&denied, false);
        std::fs::write(&user, scoped_any_config(&[&parent, &nested])).unwrap();
        assert_scoped_background(&select_workspace(&user, &nested, None).unwrap(), true);
        assert!(
            denied.revalidate_exact().is_err(),
            "new child grant must not hot-swap retained selection"
        );
    }
}
#[test]
fn new_nested_marker_invalidates_retained_any_owner_operation_admission() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("workshop");
    let nested = parent.join("projects/child");
    std::fs::create_dir_all(&nested).unwrap();
    let user = dir.path().join("user.toml");
    std::fs::write(&user, scoped_any_config(&[&parent])).unwrap();
    let retained = select_workspace(&user, &nested, None).unwrap();
    assert_scoped_background(&retained, true);
    retained.revalidate_exact().unwrap();
    std::fs::write(nested.join(".git"), "gitdir: unrelated-checkout").unwrap();
    assert!(retained.revalidate_exact().is_err());
    assert_scoped_background(&select_workspace(&user, &nested, None).unwrap(), false);
}
#[test]
fn explicit_child_candidate_is_not_rejected_by_an_intermediate_ancestor_marker() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("workshop");
    let intermediate = parent.join("projects");
    let child = intermediate.join("child");
    std::fs::create_dir_all(child.join(".git")).unwrap();
    std::fs::create_dir(intermediate.join(".git")).unwrap();
    let user = dir.path().join("user.toml");
    std::fs::write(&user, scoped_any_config(&[&parent, &child])).unwrap();
    assert_scoped_background(&select_workspace(&user, &child, None).unwrap(), true);
}
#[cfg(unix)]
#[test]
fn nested_symlink_and_loopback_alias_cannot_escape_positive_operation_scope() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("workshop");
    let child = parent.join("projects/child");
    std::fs::create_dir_all(child.join(".git")).unwrap();
    let user = dir.path().join("user.toml");
    std::fs::write(&user, scoped_any_config(&[&parent])).unwrap();
    let link = parent.join("alias-to-child");
    symlink(&child, &link).unwrap();
    assert_scoped_background(&select_workspace(&user, &link, None).unwrap(), false);
    // Canonical cwd is exactly the allowed parent; its own marker is encountered
    // via the alias before the enclosing lexical child's marker. Keep both.
    std::fs::create_dir(parent.join(".git")).unwrap();
    let loopback = child.join("back-to-parent");
    symlink(&parent, &loopback).unwrap();
    let denied = select_workspace(&user, &loopback, None).unwrap();
    assert_eq!(denied.cwd, parent.canonicalize().unwrap());
    assert_scoped_background(&denied, false);
    assert!(
        denied
            .project
            .boundaries
            .iter()
            .any(|p| p.ends_with("projects/child"))
    );
}

#[test]
fn capture_required_tags_normalize_before_reserved_membership_validation() {
    for tag in [
        "Core",
        " CORE ",
        "Routing_Judgment",
        "Possibility",
        "Pursuing",
        "Closed",
    ] {
        let text = format!(
            "schema='mneme.config.v1'\n{}capture={{required_tags=['{tag}']}}",
            entry("memory", FIRST, "workspace='any'")
        );
        assert!(
            Config::parse(&text).is_err(),
            "reserved normalized tag: {tag}"
        );
    }
    let text = format!(
        "schema='mneme.config.v1'\n{}capture={{required_tags=[' Rust:Libs ', 'rust-libs']}}",
        entry("memory", FIRST, "workspace='any'")
    );
    let config = Config::parse(&text).unwrap();
    assert_eq!(config.stores["memory"].capture.required_tags, ["rust-libs"]);
}
