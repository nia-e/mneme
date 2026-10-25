//! Frozen metadata contracts: these tests never open an owner or contact a service.
use mneme_config::*;
use std::path::{Path, PathBuf};

fn config() -> Config {
    Config::parse(
        "schema='mneme.config.v1'\n[stores.memory]\nid='01M3CV5EZP4HWAK56YJCMZQG2Y'\nowner={url='http://127.0.0.1:1',db='project'}\nactive={workspace='any'}",
    )
    .unwrap()
}

#[cfg(unix)]
fn frozen_snapshot() -> SelectionSnapshot {
    let mut config = config();
    config.workspace.profile = Some("private".into());
    config.background.enabled = false;
    let mut dormant = config.stores["memory"].clone();
    dormant.id = "01M3XW4C1X5E4YW0Q7Q6V5Y4XH".into();
    dormant.active.workspace = WorkspaceKind::NoProject;
    config.stores.insert("dormant".into(), dormant);
    let mut disabled = config.stores["memory"].clone();
    disabled.id = "01M3CPEVDTM9PP4HJTMMBZ8HR9".into();
    disabled.active.enabled = false;
    config.stores.insert("disabled".into(), disabled);
    let mut context = SelectionContext::new(PathBuf::from("/mneme-config-contract/work"));
    // Configured presence does not imply a known root.
    context.project.configured = true;
    context.sources = vec![ConfigSource {
        role: "user".into(),
        path: PathBuf::from("/mneme-config-contract/memory.toml"),
        sha256: sha256(b"frozen raw source witness\n"),
        bytes: 26,
    }];
    select(&config, &context).unwrap()
}

#[cfg(unix)]
#[test]
fn snapshot_json_and_digest_match_the_pre_refactor_contract() {
    let snapshot = frozen_snapshot();
    snapshot.verify_digest().unwrap();
    assert!(snapshot.project.configured);
    assert!(snapshot.project.root.is_none());
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        include_str!("fixtures/selection_contract.json").trim_end()
    );
    let decoded: SelectionSnapshot =
        serde_json::from_str(include_str!("fixtures/selection_contract.json")).unwrap();
    assert_eq!(decoded, snapshot);
}

#[test]
fn denial_precedence_and_explicit_vs_automatic_grants_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    let mut context = SelectionContext::new(dir.path().to_owned());
    context.profile = "private".into();
    context.project.configured = true;
    context.project.legacy_library = Some(dir.path().join("legacy.json"));
    let mut config = config();
    config.workspace.allow_stores = Some(vec![]);
    let store = config.stores.get_mut("memory").unwrap();
    store.active.enabled = false;
    store.active.excluded_roots = vec![dir.path().to_owned()];
    store.active.profiles.clear();
    store.active.workspace = WorkspaceKind::NoProject;
    store.active.roots = vec![dir.path().join("elsewhere")];

    let check = |config: &Config, context: &SelectionContext, reason, admitted, active| {
        let snapshot = select(config, context).unwrap();
        let selected = &snapshot.stores[0];
        assert_eq!(selected.reason.as_deref(), reason);
        assert_eq!((selected.admitted, selected.active), (admitted, active));
        assert_eq!(snapshot.explicit_store("memory").is_ok(), admitted);
        assert_eq!(snapshot.active.len(), usize::from(active));
        assert_eq!(
            selected.eligible,
            Eligibility {
                read: admitted,
                write: admitted,
                core: active,
                context: active,
                recall: active,
                record: active,
                maintenance: active,
            }
        );
    };
    check(&config, &context, Some("disabled"), false, false);
    config.stores.get_mut("memory").unwrap().active.enabled = true;
    context.profile = "isolated".into();
    check(&config, &context, Some("excluded-root"), false, false);
    config
        .stores
        .get_mut("memory")
        .unwrap()
        .active
        .excluded_roots
        .clear();
    check(
        &config,
        &context,
        Some("isolated-not-allowlisted"),
        false,
        false,
    );
    context.profile = "private".into();
    check(&config, &context, Some("workspace-allowlist"), false, false);
    config.workspace.allow_stores = None;
    check(
        &config,
        &context,
        Some("explicit-legacy-library"),
        false,
        false,
    );
    context.project.legacy_library = None;
    check(
        &config,
        &context,
        Some("profile-not-admitted"),
        false,
        false,
    );
    config
        .stores
        .get_mut("memory")
        .unwrap()
        .active
        .profiles
        .push("private".into());
    check(
        &config,
        &context,
        Some("workspace-not-applicable"),
        true,
        false,
    );
    config.stores.get_mut("memory").unwrap().active.workspace = WorkspaceKind::Any;
    check(&config, &context, Some("root-not-applicable"), true, false);
    config
        .stores
        .get_mut("memory")
        .unwrap()
        .active
        .roots
        .clear();
    check(&config, &context, None, true, true);
}

#[test]
fn profile_narrowing_preserves_raw_merge_validation_and_error_precedence() {
    let dir = tempfile::tempdir().unwrap();
    let empty = || Config::parse("schema='mneme.config.v1'").unwrap();
    for (old, new, expected) in [
        (Some("default"), Some("private"), Some("private")),
        (Some("private"), Some("isolated"), Some("isolated")),
        (Some("isolated"), Some("default"), Some("isolated")),
        (Some("private"), Some("not-a-profile"), Some("private")),
        (Some("not-a-profile"), Some("private"), Some("private")),
        (Some("default"), Some("not-a-profile"), Some("default")),
        (None, None, None),
    ] {
        let mut user = empty();
        user.workspace.profile = old.map(str::to_owned);
        let mut overlay = empty();
        overlay.workspace.profile = new.map(str::to_owned);
        assert_eq!(
            user.merge_project(overlay, dir.path())
                .unwrap()
                .workspace
                .profile
                .as_deref(),
            expected
        );
    }
    for (old, new) in [(None, Some("invalid")), (Some("invalid"), Some("default"))] {
        let mut user = empty();
        user.workspace.profile = old.map(str::to_owned);
        let mut overlay = empty();
        overlay.workspace.profile = new.map(str::to_owned);
        assert_eq!(
            user.merge_project(overlay, dir.path())
                .unwrap_err()
                .to_string(),
            "profile must be default, private or isolated"
        );
    }
    let mut invalid = config();
    invalid.workspace.profile = Some("invalid".into());
    // Neither profile parsing nor duplicate checks may jump ahead of root checks.
    assert_eq!(
        invalid
            .clone()
            .merge_project(invalid.clone(), Path::new("relative"))
            .unwrap_err()
            .to_string(),
        "paths must be bounded normalized absolute paths"
    );
    assert_eq!(
        invalid
            .clone()
            .merge_project(invalid, dir.path())
            .unwrap_err()
            .to_string(),
        "project overlay duplicates a user store name"
    );
}

#[cfg(unix)]
#[test]
fn denied_stores_still_check_activation_paths_but_skip_operation_paths() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let loop_path = dir.path().join("loop");
    symlink("loop", &loop_path).unwrap();
    let context = SelectionContext::new(dir.path().to_owned());
    let mut config = config();
    let store = config.stores.get_mut("memory").unwrap();
    store.active.enabled = false;
    store.active.roots = vec![loop_path.clone()];
    assert_eq!(
        select(&config, &context).unwrap_err().to_string(),
        "cannot resolve configured workspace root"
    );
    let store = config.stores.get_mut("memory").unwrap();
    store.active.roots.clear();
    store.active.excluded_roots = vec![loop_path.clone()];
    assert_eq!(
        select(&config, &context).unwrap_err().to_string(),
        "cannot resolve configured workspace root"
    );
    let store = config.stores.get_mut("memory").unwrap();
    store.active.excluded_roots.clear();
    store.applicability.core.roots = vec![loop_path.clone()];
    store.applicability.context.roots = vec![loop_path.clone()];
    store.applicability.recall.roots = vec![loop_path.clone()];
    store.applicability.record.roots = vec![loop_path.clone()];
    store.applicability.maintenance.roots = vec![loop_path];
    assert_eq!(
        select(&config, &context).unwrap().stores[0]
            .reason
            .as_deref(),
        Some("disabled")
    );
}

#[test]
fn all_valid_profile_pairs_narrow_in_merge_and_selection() {
    let dir = tempfile::tempdir().unwrap();
    let profiles = ["default", "private", "isolated"];
    for (a, parent) in profiles.iter().enumerate() {
        for (b, overlay) in profiles.iter().enumerate() {
            let mut config = config();
            config.workspace.profile = Some((*parent).into());
            let mut project = Config::parse("schema='mneme.config.v1'").unwrap();
            project.workspace.profile = Some((*overlay).into());
            assert_eq!(
                config
                    .clone()
                    .merge_project(project, dir.path())
                    .unwrap()
                    .workspace
                    .profile
                    .as_deref(),
                Some(profiles[a.max(b)])
            );
            let mut context = SelectionContext::new(dir.path().to_owned());
            context.profile = (*overlay).into();
            assert_eq!(
                select(&config, &context).unwrap().profile,
                profiles[a.max(b)]
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn frozen_request_serialization_keeps_fields_and_spellings() {
    for (mode, spelling) in [
        (SelectionRequestMode::Explicit, "explicit"),
        (SelectionRequestMode::Discover, "discover"),
    ] {
        let request = SelectionRequest {
            purpose: Default::default(),
            mode,
            user_path: Some("/config/absent.toml".into()),
            cwd: "/work/project".into(),
            requested_profile: Some("private".into()),
            lexical_pwd: Some("/alias/project".into()),
            user_codex_dir: Some("/device/codex".into()),
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            format!(
                r#"{{"mode":"{spelling}","user_path":"/config/absent.toml","cwd":"/work/project","requested_profile":"private","lexical_pwd":"/alias/project","user_codex_dir":"/device/codex"}}"#
            )
        );
    }
}

#[test]
fn nearest_legacy_selectors_and_strictest_ancestor_privacy_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let child = parent.join("child");
    std::fs::create_dir_all(parent.join(".mneme")).unwrap();
    std::fs::create_dir_all(child.join(".mneme")).unwrap();
    let user = dir.path().join("memory.toml");
    std::fs::write(&user, "schema='mneme.config.v1'\n[stores.memory]\nid='01M3CV5EZP4HWAK56YJCMZQG2Y'\nowner={url='http://127.0.0.1:1',db='project'}\nactive={workspace='any'}").unwrap();
    std::fs::write(
        parent.join(".mneme/profile.json"),
        r#"{"schema":"mneme.profile.v2","mode":"isolated","stores":[]}"#,
    )
    .unwrap();
    std::fs::write(
        child.join(".mneme/profile.json"),
        r#"{"schema":"mneme.profile.v2","mode":"default","stores":["memory"]}"#,
    )
    .unwrap();
    let selected = select_workspace(&user, &child, None).unwrap();
    assert_eq!(selected.profile, "isolated");
    assert_eq!(
        selected.project.legacy_allow_stores,
        Some(vec!["memory".into()])
    );
    assert_eq!(selected.active.len(), 1);
    // Nearest selector absence is meaningful: it is not permission to inherit.
    std::fs::write(
        child.join(".mneme/profile.json"),
        r#"{"schema":"mneme.profile.v2","mode":"default"}"#,
    )
    .unwrap();
    let selected = select_workspace(&user, &child, None).unwrap();
    assert_eq!(selected.profile, "isolated");
    assert_eq!(selected.project.legacy_allow_stores, None);
    assert_eq!(
        selected.stores[0].reason.as_deref(),
        Some("isolated-not-allowlisted")
    );
}
