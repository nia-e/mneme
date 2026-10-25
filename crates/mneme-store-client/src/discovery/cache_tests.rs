use super::fixture::{DB, Fixture};
use super::*;
use std::{path::PathBuf, sync::atomic::Ordering, time::Duration};

struct Workspace {
    dir: PathBuf,
    config: PathBuf,
    receipt: PathBuf,
}
impl Workspace {
    fn new(owner: &Fixture) -> Self {
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mneme-discovery-client-{}", ulid::Ulid::new()));
        std::fs::create_dir(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config = dir.join("memory.toml");
        let receipt = dir.join("cache").join("receipt.json");
        std::fs::write(
            &config,
            format!(
                r#"schema="mneme.config.v1"
read_default="seed"
[stores.seed]
id="{DB}"
discovery=true
owner={{url="{}",db="project"}}
active={{workspace="any"}}
"#,
                owner.url
            ),
        )
        .unwrap();
        Self {
            dir,
            config,
            receipt,
        }
    }
    fn prepare(&self) -> WorkspaceDeclarations {
        prepare_workspace(Some(&self.config), &self.dir, None)
            .unwrap()
            .unwrap()
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}
fn budget() -> OperationBudget {
    OperationBudget::new(1024 * 1024, Duration::from_secs(3)).unwrap()
}
fn catalog(extra: &str) -> serde_json::Value {
    serde_json::to_value(
        PublishedCatalog::parse_toml(
            &format!("schema=\"mneme.discovery-catalog.v1\"\n{extra}"),
            DB,
        )
        .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn browse_reuses_discovery_without_changing_workspace_selection_or_acquisition() {
    const REMOTE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAT";
    let owner = Fixture::new(catalog(&format!(
        "[stores.remote]\nid='{REMOTE}'\nowner={{url='ssh://remote',db='project',ssh_mcp_port=35169}}\n\
         [stores.remote.devices.other.active]\nroots=['/another/device/project']",
    )));
    let workspace = Workspace::new(&owner);
    let automatic = resolve_prepared(
        workspace.prepare(),
        ResolveOptions::default(),
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap()
    .selection
    .unwrap();
    assert!(automatic.explicit_store("remote").is_err());
    let receipt = std::fs::read(&workspace.receipt).unwrap();
    let browse = resolve_prepared(
        workspace.prepare().for_browsing(),
        ResolveOptions::default(),
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap();
    assert_eq!(browse.cache_status, CacheStatus::Cached);
    assert!(
        browse.report.unwrap().stores[REMOTE].blocked,
        "automatic-policy evidence is retained"
    );
    let browse = browse.selection.unwrap();
    assert!(browse.explicit_store("remote").unwrap().eligible.read);
    assert!(!browse.explicit_store("remote").unwrap().eligible.write);
    browse.revalidate_exact().unwrap();
    let again = resolve_prepared(
        workspace.prepare(),
        ResolveOptions::default(),
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap()
    .selection
    .unwrap();
    assert_eq!(automatic, again);
    assert_eq!(std::fs::read(&workspace.receipt).unwrap(), receipt);
    assert_eq!(owner.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn receipt_readthrough_and_exact_replay_are_offline_and_stable() {
    let owner = Fixture::new(catalog(""));
    let workspace = Workspace::new(&owner);
    let first = resolve_prepared(
        workspace.prepare(),
        ResolveOptions::default(),
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap();
    assert_eq!(first.cache_status, CacheStatus::Acquired);
    let first = first.selection.unwrap();
    assert_eq!(owner.calls.load(Ordering::Relaxed), 1);
    let bytes = std::fs::read(&workspace.receipt).unwrap();
    owner.set_document(catalog(&format!(
        "[stores.seed]\nid=\"{DB}\"\npermissions={{write=false}}"
    )));
    let exhausted = OperationBudget::new(1, Duration::from_millis(1)).unwrap();
    tokio::time::sleep(Duration::from_millis(3)).await;
    let cached = resolve_prepared(
        workspace.prepare(),
        ResolveOptions::default(),
        &exhausted,
        &workspace.receipt,
    )
    .await
    .unwrap();
    assert_eq!(cached.cache_status, CacheStatus::Cached);
    assert_eq!(cached.selection.unwrap().digest, first.digest);
    assert_eq!(std::fs::read(&workspace.receipt).unwrap(), bytes);
    first.revalidate_exact().unwrap();
    assert_eq!(
        replay_prepared(workspace.prepare(), Some(&workspace.receipt), &first.digest)
            .unwrap()
            .digest,
        first.digest
    );
    assert!(
        replay_prepared(
            workspace.prepare(),
            Some(&workspace.receipt),
            &"0".repeat(64)
        )
        .unwrap_err()
        .to_string()
        .contains("selection digest changed")
    );
    assert_eq!(owner.calls.load(Ordering::Relaxed), 1);
    let fresh = resolve_prepared(
        workspace.prepare(),
        ResolveOptions {
            refresh: true,
            offline: false,
        },
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap();
    assert_eq!(fresh.cache_status, CacheStatus::Refreshed);
    assert_ne!(fresh.selection.unwrap().digest, first.digest);
    assert!(first.revalidate_exact().is_err());
    assert!(replay_prepared(workspace.prepare(), Some(&workspace.receipt), &first.digest).is_err());
    assert_eq!(owner.calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn failed_refresh_retains_receipt_and_offline_selection_with_attempt_diagnostics() {
    let owner = Fixture::new(catalog(""));
    let workspace = Workspace::new(&owner);
    let first = resolve_prepared(
        workspace.prepare(),
        ResolveOptions::default(),
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap()
    .selection
    .unwrap();
    let bytes = std::fs::read(&workspace.receipt).unwrap();
    owner.set_document(json!({"schema":"broken"}));
    let failed = resolve_prepared(
        workspace.prepare(),
        ResolveOptions {
            refresh: true,
            offline: false,
        },
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap();
    assert_eq!(failed.cache_status, CacheStatus::Stale);
    assert_eq!(failed.selection.unwrap().digest, first.digest);
    assert!(failed.attempt.unwrap().unavailable.contains_key(DB));
    assert!(failed.report.unwrap().unavailable.is_empty());
    assert_eq!(std::fs::read(&workspace.receipt).unwrap(), bytes);
    first.revalidate_exact().unwrap();
}

#[tokio::test]
async fn initial_failure_and_offline_absence_do_not_publish_an_empty_receipt() {
    let owner = Fixture::new(json!({"schema":"broken"}));
    let workspace = Workspace::new(&owner);
    let offline = resolve_prepared(
        workspace.prepare(),
        ResolveOptions {
            offline: true,
            refresh: false,
        },
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap();
    assert_eq!(offline.cache_status, CacheStatus::Unavailable);
    assert!(offline.selection.is_none());
    assert!(!workspace.receipt.parent().unwrap().exists());
    assert_eq!(owner.calls.load(Ordering::Relaxed), 0);
    let failed = resolve_prepared(
        workspace.prepare(),
        ResolveOptions::default(),
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap();
    assert!(failed.selection.is_none());
    assert_eq!(failed.cache_status, CacheStatus::Unavailable);
    assert!(!workspace.receipt.exists());
    assert_eq!(owner.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn concurrent_readthrough_acquires_once_and_reuses_winner() {
    let owner = Fixture::new(catalog(""));
    let workspace = Workspace::new(&owner);
    let a = budget();
    let b = budget();
    let (a, b) = tokio::join!(
        resolve_prepared(
            workspace.prepare(),
            ResolveOptions::default(),
            &a,
            &workspace.receipt
        ),
        resolve_prepared(
            workspace.prepare(),
            ResolveOptions::default(),
            &b,
            &workspace.receipt
        )
    );
    assert_eq!(
        a.unwrap().selection.unwrap().digest,
        b.unwrap().selection.unwrap().digest
    );
    assert_eq!(owner.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn publication_lock_wait_is_bounded_and_drop_releases() {
    let owner = Fixture::new(catalog(""));
    let workspace = Workspace::new(&owner);
    let held = cache::ReceiptLock::acquire(&workspace.receipt, &budget())
        .await
        .unwrap();
    let tiny = OperationBudget::new(1024, Duration::from_millis(20)).unwrap();
    assert!(
        cache::ReceiptLock::acquire(&workspace.receipt, &tiny)
            .await
            .is_err()
    );
    assert!(tiny.elapsed() < Duration::from_secs(1));
    drop(held);
    cache::ReceiptLock::acquire(&workspace.receipt, &budget())
        .await
        .unwrap();
    assert_eq!(owner.calls.load(Ordering::Relaxed), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn receipt_and_directory_symlinks_and_unsafe_modes_are_refused() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let owner = Fixture::new(catalog(""));
    let workspace = Workspace::new(&owner);
    std::fs::create_dir(workspace.receipt.parent().unwrap()).unwrap();
    let target = workspace.dir.join("target.json");
    std::fs::write(&target, b"{}").unwrap();
    symlink(&target, &workspace.receipt).unwrap();
    assert!(
        resolve_prepared(
            workspace.prepare(),
            ResolveOptions::default(),
            &budget(),
            &workspace.receipt
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"{}");
    std::fs::remove_file(&workspace.receipt).unwrap();
    std::fs::set_permissions(
        workspace.receipt.parent().unwrap(),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert!(
        cache::ReceiptLock::acquire(&workspace.receipt, &budget())
            .await
            .is_err()
    );
    std::fs::set_permissions(
        workspace.receipt.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let alias = workspace.dir.join("alias");
    symlink(workspace.receipt.parent().unwrap(), &alias).unwrap();
    assert!(
        cache::ReceiptLock::acquire(&alias.join("other.json"), &budget())
            .await
            .is_err()
    );
    assert_eq!(owner.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn no_discovery_preserves_digest_without_network_or_cache() {
    let owner = Fixture::new(catalog(""));
    let workspace = Workspace::new(&owner);
    let config = std::fs::read_to_string(&workspace.config)
        .unwrap()
        .replace("discovery=true\n", "");
    std::fs::write(&workspace.config, config).unwrap();
    let old = mneme_config::select_workspace(&workspace.config, &workspace.dir, None).unwrap();
    let tiny = OperationBudget::new(1, Duration::from_millis(1)).unwrap();
    tokio::time::sleep(Duration::from_millis(3)).await;
    let now = resolve_workspace(
        Some(&workspace.config),
        &workspace.dir,
        None,
        ResolveOptions::default(),
        &tiny,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(now.cache_status, CacheStatus::LocalOnly);
    assert_eq!(old.digest, now.selection.unwrap().digest);
    assert!(now.report.is_some());
    assert!(!workspace.receipt.parent().unwrap().exists());
    assert_eq!(owner.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn exhausted_refresh_budget_retains_previous_usable_receipt() {
    let owner = Fixture::new(catalog(""));
    let workspace = Workspace::new(&owner);
    let first = resolve_prepared(
        workspace.prepare(),
        ResolveOptions::default(),
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap()
    .selection
    .unwrap();
    let bytes = std::fs::read(&workspace.receipt).unwrap();
    let tiny = OperationBudget::new(1, Duration::from_millis(1)).unwrap();
    tokio::time::sleep(Duration::from_millis(3)).await;
    let stale = resolve_prepared(
        workspace.prepare(),
        ResolveOptions {
            refresh: true,
            offline: false,
        },
        &tiny,
        &workspace.receipt,
    )
    .await
    .unwrap();
    assert_eq!(stale.cache_status, CacheStatus::Stale);
    assert_eq!(stale.selection.unwrap().digest, first.digest);
    assert!(stale.attempt.unwrap().incomplete.is_some());
    assert_eq!(std::fs::read(&workspace.receipt).unwrap(), bytes);
    assert_eq!(owner.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn seedless_sparse_optin_is_diagnostic_not_an_empty_published_receipt() {
    let owner = Fixture::new(catalog(""));
    let workspace = Workspace::new(&owner);
    std::fs::write(
        &workspace.config,
        format!("schema=\"mneme.config.v1\"\n[stores.unresolved]\nid=\"{DB}\"\ndiscovery=true\n"),
    )
    .unwrap();
    let outcome = resolve_prepared(
        workspace.prepare(),
        ResolveOptions::default(),
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap();
    assert_eq!(outcome.cache_status, CacheStatus::Unavailable);
    assert!(outcome.selection.is_none());
    assert!(outcome.report.unwrap().incomplete.is_some());
    assert!(!workspace.receipt.exists());
    assert_eq!(owner.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn colliding_imported_allowlist_label_cannot_replace_previous_receipt() {
    use super::fixture::Options;
    const OTHER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA2";
    let child = Fixture::with_options(
        serde_json::to_value(
            PublishedCatalog::parse_toml("schema='mneme.discovery-catalog.v1'", OTHER).unwrap(),
        )
        .unwrap(),
        Options {
            database_id: OTHER,
            ..Options::default()
        },
    );
    let owner = Fixture::new(catalog(""));
    let workspace = Workspace::new(&owner);
    let old = std::fs::read_to_string(&workspace.config).unwrap();
    std::fs::write(
        &workspace.config,
        old.replace(
            "[stores.seed]",
            "[workspace]\nprofile='isolated'\nallow_stores=['seed']\n[stores.seed]",
        ),
    )
    .unwrap();
    let first = resolve_prepared(
        workspace.prepare(),
        ResolveOptions::default(),
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap()
    .selection
    .unwrap();
    let bytes = std::fs::read(&workspace.receipt).unwrap();
    owner.set_document(catalog(&format!(
        "[stores.seed]\nid='{OTHER}'\ndiscovery=true\nowner={{url='{}',db='project'}}",
        child.url
    )));
    let stale = resolve_prepared(
        workspace.prepare(),
        ResolveOptions {
            refresh: true,
            offline: false,
        },
        &budget(),
        &workspace.receipt,
    )
    .await
    .unwrap();
    assert_eq!(stale.cache_status, CacheStatus::Stale);
    assert_eq!(stale.selection.unwrap().digest, first.digest);
    assert!(stale.attempt.unwrap().unavailable.contains_key(OTHER));
    assert_eq!(child.calls.load(Ordering::Relaxed), 0);
    assert_eq!(child.database_calls.load(Ordering::Relaxed), 0);
    assert_eq!(std::fs::read(&workspace.receipt).unwrap(), bytes);
    first.revalidate_exact().unwrap();
}
