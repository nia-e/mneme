use super::*;
use mneme_config::{SelectionRequest, SelectionRequestMode};
use std::path::PathBuf;

const DB: &str = "01M3CPEVDTM9PP4HJTMMBZ8HR9";
const NOW: u64 = 1_800_000_000;

/// Metadata-only fixture: no live store, listener, environment mutation, or
/// configuration beneath the caller's actual home directory is involved.
struct Workspace {
    dir: PathBuf,
    config: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mneme-cache-demand-{}", ulid::Ulid::new()));
        std::fs::create_dir(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            config: dir.join("memory.toml"),
            dir,
        }
    }

    fn text(&self) -> String {
        format!(
            r#"schema="mneme.config.v1"
read_default="alpha"
[stores.alpha]
id="{DB}"
owner={{url="http://127.0.0.1:1/mcp",db="project"}}
active={{workspace="any"}}
cache={{library="reference",populate={{refresh_after_secs=900,max_snapshot_bytes=134217728}}}}
[libraries.reference]
catalog="{}"
"#,
            self.dir.join("catalog.json").display()
        )
    }

    fn write_config(&self) {
        std::fs::write(&self.config, self.text()).unwrap();
    }

    fn write_overlay(&self) {
        let overlay_dir = self.dir.join(".mneme");
        std::fs::create_dir(&overlay_dir).unwrap();
        std::fs::write(
            overlay_dir.join("memory.toml"),
            self.text().replace("read_default=\"alpha\"\n", ""),
        )
        .unwrap();
    }

    fn request(&self, mode: SelectionRequestMode) -> SelectionRequest {
        SelectionRequest {
            mode,
            user_path: Some(self.config.clone()),
            cwd: self.dir.clone(),
            requested_profile: Some("private".into()),
            lexical_pwd: Some(self.dir.clone()),
            user_codex_dir: Some(self.dir.join("private-codex-home")),
        }
    }

    fn selection(&self) -> SelectionSnapshot {
        self.request(SelectionRequestMode::Explicit)
            .replay_offline(None)
            .unwrap()
            .unwrap()
    }

    fn demand(&self) -> (SelectionSnapshot, DemandRecord, PathBuf) {
        self.write_config();
        let selection = self.selection();
        let active = &selection.active[0];
        let record = DemandRecord::from_selection(&selection, active, NOW)
            .unwrap()
            .unwrap();
        let path = demand_path(&selection, active).unwrap().unwrap();
        (selection, record, path)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}

#[test]
fn exact_request_replay_preserves_explicit_and_discovered_selection_modes() {
    let workspace = Workspace::new();
    workspace.write_config();
    let mut digests = Vec::new();
    for mode in [
        SelectionRequestMode::Explicit,
        SelectionRequestMode::Discover,
    ] {
        let request = workspace.request(mode);
        let selected = request.replay_offline(None).unwrap().unwrap();
        assert_eq!(selected.request.as_ref(), Some(&request));
        assert_eq!(selected.profile, "private");
        assert_eq!(selected.active[0].store.id, DB);
        selected.revalidate_exact().unwrap();
        let replay = request.replay_offline(None).unwrap().unwrap();
        assert_eq!(replay.digest, selected.digest);
        digests.push(selected.digest);
    }
    assert_ne!(digests[0], digests[1], "request mode is part of authority");
    assert!(!workspace.dir.join("catalog.json").exists());
}

#[test]
fn missing_default_is_preserved_and_appearance_invalidates_discovered_authority() {
    let workspace = Workspace::new();
    workspace.write_overlay();
    let request = workspace.request(SelectionRequestMode::Discover);
    let selected = request.replay_offline(None).unwrap().unwrap();
    assert_eq!(selected.request.as_ref(), Some(&request));
    assert!(!workspace.config.exists());
    assert_eq!(selected.sources.len(), 1);
    assert_eq!(selected.sources[0].role, "project");
    selected.revalidate_exact().unwrap();

    // A default file appearing is a new authority input even if it contributes
    // no store grants. Replaying only the old source list would miss this.
    std::fs::write(&workspace.config, "schema='mneme.config.v1'").unwrap();
    assert!(selected.revalidate_exact().is_err());
    let new = request.replay_offline(None).unwrap().unwrap();
    assert_ne!(new.digest, selected.digest);
}

#[test]
fn missing_explicit_config_is_not_reinterpreted_as_discovery() {
    let workspace = Workspace::new();
    workspace.write_overlay();
    assert!(
        workspace
            .request(SelectionRequestMode::Explicit)
            .replay_offline(None)
            .is_err()
    );
    assert!(
        workspace
            .request(SelectionRequestMode::Discover)
            .replay_offline(None)
            .unwrap()
            .is_some()
    );
}

#[test]
fn absent_discovered_configuration_stays_unconfigured() {
    let workspace = Workspace::new();
    assert!(
        workspace
            .request(SelectionRequestMode::Discover)
            .replay_offline(None)
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read_dir(&workspace.dir).unwrap().count(), 0);
}

#[test]
fn demand_is_bounded_local_metadata_and_plans_replay_exact_authority() {
    let workspace = Workspace::new();
    let (selected, record, path) = workspace.demand();
    assert_eq!(
        path,
        workspace
            .dir
            .join("cache-demand")
            .join(format!("{DB}.json"))
    );
    assert_eq!(record.request, *selected.request.as_ref().unwrap());
    assert_eq!(record.selection_digest, selected.digest);
    assert_eq!(record.store, "alpha");
    assert_eq!(record.db_id, DB);
    assert_eq!(record.library, "reference");
    assert!(!record.explicit_store);

    persist(&path, &record).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() <= MAX_DEMAND_BYTES);
    let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for forbidden in ["config", "query", "body", "summary", "payload", "owner"] {
        assert!(document.get(forbidden).is_none(), "retained {forbidden}");
    }
    let plan = plan_at(&path, NOW).unwrap();
    assert_eq!(plan.request, record.request);
    assert_eq!(plan.selection_digest, selected.digest);
    assert_eq!(plan.store, "alpha");
    assert_eq!(plan.db_id, DB);
    assert_eq!(plan.library, "reference");
    assert_eq!(plan.owner, selected.active[0].store.owner);
    assert_eq!(plan.cache.refresh_after_secs, 900);
    assert_eq!(plan.cache.max_snapshot_bytes, 134_217_728);
    assert_eq!(plan.catalog_path, workspace.dir.join("catalog.json"));
    assert_eq!(plan.observed_at_unix_secs, NOW);
    assert!(!workspace.dir.join("catalog.json").exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
fn successful_read_cannot_mint_demand_without_opt_in_or_replayable_request() {
    let workspace = Workspace::new();
    std::fs::write(
        &workspace.config,
        workspace.text().replace(
            ",populate={refresh_after_secs=900,max_snapshot_bytes=134217728}",
            "",
        ),
    )
    .unwrap();
    let selected = workspace.selection();
    assert!(
        DemandRecord::from_selection(&selected, &selected.active[0], NOW)
            .unwrap()
            .is_none()
    );

    // Pure select() is useful in callers' unit tests, but lacks a replayable
    // filesystem request and must not authorize later background population.
    let config = mneme_config::Config::parse(&workspace.text()).unwrap();
    let selected = mneme_config::select(
        &config,
        &mneme_config::SelectionContext::new(workspace.dir.clone()),
    )
    .unwrap();
    assert!(selected.request.is_none());
    assert!(
        DemandRecord::from_selection(&selected, &selected.active[0], NOW)
            .unwrap()
            .is_none()
    );
    assert!(!workspace.dir.join("cache-demand").exists());
}

#[test]
fn stale_demand_rejects_replaced_configuration_and_does_not_contact_an_owner() {
    let workspace = Workspace::new();
    let (_, record, path) = workspace.demand();
    persist(&path, &record).unwrap();
    std::fs::write(
        &workspace.config,
        workspace.text().replace("db=\"project\"", "db=\"changed\""),
    )
    .unwrap();
    assert!(plan_at(&path, NOW).is_err());
    assert!(!workspace.dir.join("catalog.json").exists());
}

#[test]
fn explicit_inactive_store_demand_replays_without_activating_background_selection() {
    let workspace = Workspace::new();
    std::fs::write(
        &workspace.config,
        workspace
            .text()
            .replace("workspace=\"any\"", "workspace=\"project\""),
    )
    .unwrap();
    let selected = workspace.selection();
    assert!(selected.active.is_empty());
    let explicit = selected.explicit_store("alpha").unwrap();
    let record = DemandRecord::from_selection(&selected, &explicit, NOW)
        .unwrap()
        .unwrap();
    assert!(record.explicit_store);
    let path = demand_path(&selected, &explicit).unwrap().unwrap();
    persist(&path, &record).unwrap();
    let plan = plan_at(&path, NOW).unwrap();
    assert_eq!(plan.db_id, DB);
    let replay = plan
        .request
        .replay_offline(plan.binding.as_ref())
        .unwrap()
        .unwrap();
    assert!(replay.active.is_empty());
    assert_eq!(replay.digest, selected.digest);
}

#[test]
fn denied_reads_cannot_mint_population_demand() {
    let workspace = Workspace::new();
    std::fs::write(
        &workspace.config,
        workspace.text().replace(
            "active={workspace=\"any\"}",
            "active={workspace=\"any\"}\npermissions={read=false,write=false}",
        ),
    )
    .unwrap();
    let selected = workspace.selection();
    let explicit = selected.explicit_store("alpha").unwrap();
    assert!(!explicit.eligible.read);
    assert!(!matches!(
        DemandRecord::from_selection(&selected, &explicit, NOW),
        Ok(Some(_))
    ));
    assert!(!workspace.dir.join("cache-demand").exists());
}

#[test]
fn recorded_absent_default_cannot_silently_become_present_during_planning() {
    let workspace = Workspace::new();
    workspace.write_overlay();
    let selection = workspace
        .request(SelectionRequestMode::Discover)
        .replay_offline(None)
        .unwrap()
        .unwrap();
    let active = &selection.active[0];
    let record = DemandRecord::from_selection(&selection, active, NOW)
        .unwrap()
        .unwrap();
    let path = demand_path(&selection, active).unwrap().unwrap();
    persist(&path, &record).unwrap();
    plan_at(&path, NOW).unwrap();
    std::fs::write(&workspace.config, "schema='mneme.config.v1'").unwrap();
    assert!(plan_at(&path, NOW).is_err());
}

#[test]
fn demand_expiry_future_timestamps_and_changed_identity_are_refused() {
    let workspace = Workspace::new();
    let (_, mut record, path) = workspace.demand();
    persist(&path, &record).unwrap();
    assert!(plan_at(&path, NOW + MAX_DEMAND_AGE_SECS + 1).is_err());
    assert!(plan_at(&path, NOW - MAX_DEMAND_AGE_SECS - 1).is_err());
    record.db_id = "01M3CV5EZP4HWAK56YJCMZQG2Y".into();
    // Either persistence rejects a malformed path/identity pair or planning
    // rejects it; neither stage may admit a different database.
    if persist(&path, &record).is_ok() {
        assert!(plan_at(&path, NOW).is_err());
    }
}

#[test]
fn oversized_and_unknown_field_demand_files_are_refused() {
    let workspace = Workspace::new();
    let (_, record, path) = workspace.demand();
    persist(&path, &record).unwrap();
    std::fs::write(&path, vec![b' '; MAX_DEMAND_BYTES + 1]).unwrap();
    assert!(plan_at(&path, NOW).is_err());
    let mut document = serde_json::to_value(&record).unwrap();
    document["query"] = serde_json::json!("must not be retained");
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    assert!(plan_at(&path, NOW).is_err());
}

#[test]
fn population_policy_rejects_unbounded_and_subminimum_work() {
    for refresh_after_secs in [0, 59, 86_401, u64::MAX] {
        assert!(
            mneme_config::CachePopulate {
                refresh_after_secs,
                max_snapshot_bytes: 1024 * 1024,
            }
            .validate()
            .is_err()
        );
    }
    for max_snapshot_bytes in [0, 1024 * 1024 - 1, 512 * 1024 * 1024 + 1, u64::MAX] {
        assert!(
            mneme_config::CachePopulate {
                refresh_after_secs: 60,
                max_snapshot_bytes,
            }
            .validate()
            .is_err()
        );
    }
    for (refresh_after_secs, max_snapshot_bytes) in [(60, 1024 * 1024), (86_400, 512 * 1024 * 1024)]
    {
        mneme_config::CachePopulate {
            refresh_after_secs,
            max_snapshot_bytes,
        }
        .validate()
        .unwrap();
    }
}

#[cfg(unix)]
#[test]
fn demand_files_refuse_public_permissions_symlinks_and_hardlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let workspace = Workspace::new();
    let (_, record, path) = workspace.demand();
    persist(&path, &record).unwrap();
    let private = std::fs::read(&path).unwrap();

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(plan_at(&path, NOW).is_err());
    assert!(persist(&path, &record).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let hardlink = workspace.dir.join("hardlink.json");
    std::fs::hard_link(&path, &hardlink).unwrap();
    assert!(plan_at(&path, NOW).is_err());
    assert!(persist(&path, &record).is_err());
    std::fs::remove_file(&hardlink).unwrap();

    let original = workspace.dir.join("original.json");
    std::fs::rename(&path, &original).unwrap();
    symlink(&original, &path).unwrap();
    assert!(plan_at(&path, NOW).is_err());
    assert!(persist(&path, &record).is_err());
    assert_eq!(std::fs::read(&original).unwrap(), private);
}

#[cfg(unix)]
#[test]
fn demand_directory_must_remain_private_and_non_symlinked() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let workspace = Workspace::new();
    let (_, record, path) = workspace.demand();
    persist(&path, &record).unwrap();
    let directory = path.parent().unwrap();
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(plan_at(&path, NOW).is_err());
    assert!(persist(&path, &record).is_err());
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();

    let original = workspace.dir.join("original-demand");
    std::fs::rename(directory, &original).unwrap();
    symlink(&original, directory).unwrap();
    assert!(plan_at(&path, NOW).is_err());
    assert!(persist(&path, &record).is_err());
}

#[test]
fn only_substantive_read_actions_classify_as_population_demand() {
    use crate::{ORDINARY_TOOLS, RequestAccess, request_access};
    use serde_json::json;
    use std::collections::BTreeSet;

    let direct_payload = [
        "core",
        "query",
        "recall",
        "recall_context",
        "get",
        "list",
        "neighbors",
    ];
    let mut covered = BTreeSet::new();
    for tool in ORDINARY_TOOLS {
        let actions: &[(&str, bool)] = match *tool {
            "graph" => &[("topology", false), ("summaries", true)],
            "walk" => &[
                ("start", true),
                ("look", true),
                ("edges", true),
                ("body", true),
                ("go", true),
                ("back", true),
                ("done", false),
                ("abort", false),
            ],
            "episode" => &[
                ("list", true),
                ("search", true),
                ("get", true),
                ("history", true),
                ("references", true),
                ("append", false),
                ("revise", false),
            ],
            "concern" => &[
                ("list", false),
                ("notice", false),
                ("record_finding", false),
            ],
            _ => &[],
        };
        covered.insert(*tool);
        if actions.is_empty() {
            let args = json!({});
            let access = request_access(tool, &args).unwrap();
            assert_eq!(
                payload_read(tool, &args, access),
                direct_payload.contains(tool),
                "{tool}"
            );
        } else {
            for (action, expected) in actions {
                let args = json!({"action":action});
                let access = request_access(tool, &args).unwrap();
                assert_eq!(
                    payload_read(tool, &args, access),
                    *expected,
                    "{tool}/{action}"
                );
                assert!(
                    !payload_read(tool, &args, RequestAccess::Write),
                    "{tool}/{action} write"
                );
            }
            assert!(!payload_read(
                tool,
                &json!({"action":"future-action"}),
                RequestAccess::Read
            ));
        }
        assert!(!payload_read(tool, &json!({}), RequestAccess::Write));
    }
    assert_eq!(covered.len(), ORDINARY_TOOLS.len());
    for metadata in ["catalog", "configuration_catalog", "future-tool"] {
        assert!(!payload_read(metadata, &json!({}), RequestAccess::Read));
    }
    assert!(!payload_read("graph", &json!({}), RequestAccess::Read));
}

#[test]
fn repeated_demand_coalesces_but_changed_workspace_request_replaces_same_identity() {
    let workspace = Workspace::new();
    let (_, mut record, path) = workspace.demand();
    persist(&path, &record).unwrap();
    record.observed_at_unix_secs = NOW + COALESCE_SECS - 1;
    persist(&path, &record).unwrap();
    assert_eq!(read(&path).unwrap().observed_at_unix_secs, NOW);
    record.observed_at_unix_secs = NOW + COALESCE_SECS;
    persist(&path, &record).unwrap();
    assert_eq!(
        read(&path).unwrap().observed_at_unix_secs,
        NOW + COALESCE_SECS
    );

    let another_cwd = workspace.dir.join("another-cwd");
    std::fs::create_dir(&another_cwd).unwrap();
    let mut another_request = workspace.request(SelectionRequestMode::Explicit);
    another_request.cwd = another_cwd.clone();
    another_request.lexical_pwd = Some(another_cwd);
    let selected = another_request.replay_offline(None).unwrap().unwrap();
    let replacement =
        DemandRecord::from_selection(&selected, &selected.active[0], NOW + COALESCE_SECS + 1)
            .unwrap()
            .unwrap();
    assert_eq!(
        demand_path(&selected, &selected.active[0])
            .unwrap()
            .unwrap(),
        path
    );
    assert_ne!(replacement.selection_digest, record.selection_digest);
    persist(&path, &replacement).unwrap();
    assert_eq!(read(&path).unwrap(), replacement);
    // A delayed earlier job cannot overwrite newer observed authority.
    persist(&path, &record).unwrap();
    assert_eq!(read(&path).unwrap(), replacement);
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json"))
            .count(),
        1
    );
}

#[cfg(unix)]
#[test]
fn demand_fifo_is_rejected_without_opening_or_blocking() {
    use std::os::unix::ffi::OsStrExt;
    let workspace = Workspace::new();
    let (_, record, path) = workspace.demand();
    checked_directory(path.parent().unwrap(), true).unwrap();
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a NUL-terminated valid pathname for this call's duration.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(plan_at(&path, NOW).is_err());
    assert!(persist(&path, &record).is_err());
}

#[test]
fn bounded_demand_directory_refuses_new_identities_but_refreshes_existing_ones() {
    let workspace = Workspace::new();
    let (_, mut record, path) = workspace.demand();
    persist(&path, &record).unwrap();
    for index in 1..MAX_DEMANDS {
        std::fs::write(
            path.parent()
                .unwrap()
                .join(format!("reserved-{index}.json")),
            b"{}",
        )
        .unwrap();
    }
    let rejected = path.parent().unwrap().join("new-identity.json");
    assert!(persist(&rejected, &record).is_err());
    assert!(!rejected.exists());
    record.observed_at_unix_secs = NOW + COALESCE_SECS;
    persist(&path, &record).unwrap();
    assert_eq!(read(&path).unwrap(), record);
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        MAX_DEMANDS + 1
    );
}

#[cfg(unix)]
#[test]
fn retirement_reclaims_only_expired_private_identity_named_demand_at_its_deadline() {
    use std::os::unix::fs::PermissionsExt;

    const NEXT_DB: &str = "01M3CV5EZP4HWAK56YJCMZQG2Y";
    const PUBLIC_DB: &str = "01M3CV5EZP4HWAK56YJCMZQG2Z";
    let workspace = Workspace::new();
    let (_, old, old_path) = workspace.demand();
    persist(&old_path, &old).unwrap();
    let parent = old_path.parent().unwrap();
    let deadline = NOW + MAX_DEMAND_AGE_SECS;
    assert_eq!(
        plan_at(&old_path, deadline - 1).unwrap().deadline_unix_secs,
        deadline
    );
    assert!(plan_at(&old_path, deadline).is_err());

    // Each lookalike still occupies capacity, but none grants cleanup authority.
    let mut public = old.clone();
    public.db_id = PUBLIC_DB.into();
    let retained = [
        ("malformed.json".into(), b"malformed".to_vec(), 0o600),
        (
            "wrong-name.json".into(),
            serde_json::to_vec(&old).unwrap(),
            0o600,
        ),
        (
            format!("{PUBLIC_DB}.json"),
            serde_json::to_vec(&public).unwrap(),
            0o644,
        ),
        ("unknown.txt".into(), b"not a demand record".to_vec(), 0o600),
    ];
    for (name, bytes, mode) in &retained {
        let path = parent.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(*mode)).unwrap();
    }
    for index in 1 + retained.len()..MAX_DEMANDS {
        std::fs::write(parent.join(format!("reserved-{index}.json")), b"{}").unwrap();
    }

    // The replacement is another genuinely selected identity, not a fabricated
    // record whose later authority check would fail for an unrelated reason.
    std::fs::write(&workspace.config, workspace.text().replace(DB, NEXT_DB)).unwrap();
    let selected = workspace.selection();
    let active = &selected.active[0];
    let mut replacement = DemandRecord::from_selection(&selected, active, deadline - 1)
        .unwrap()
        .unwrap();
    let new_path = demand_path(&selected, active).unwrap().unwrap();
    assert!(persist(&new_path, &replacement).is_err());
    assert!(old_path.exists());
    assert!(!new_path.exists());

    replacement.observed_at_unix_secs = deadline;
    persist(&new_path, &replacement).unwrap();
    assert!(!old_path.exists());
    assert_eq!(read(&new_path).unwrap(), replacement);
    assert_eq!(plan_at(&new_path, deadline).unwrap().db_id, NEXT_DB);
    for (name, bytes, _) in retained {
        assert_eq!(std::fs::read(parent.join(name)).unwrap(), bytes);
    }
    assert_eq!(std::fs::read_dir(parent).unwrap().count(), MAX_DEMANDS + 1);
}

#[test]
fn overfilled_expired_inventory_cannot_retire_a_partial_scan_to_admit_new_demand() {
    use std::collections::BTreeMap;

    const NEXT_DB: &str = "01M3CV5EZP4HWAK56YJCMZQG2Y";
    let workspace = Workspace::new();
    let (_, old, old_path) = workspace.demand();
    persist(&old_path, &old).unwrap();
    let parent = old_path.parent().unwrap();
    // Every demand entry would be eligible for retirement if the inventory
    // were complete. More than the scan allowance must fail before deleting
    // any of them, regardless of the filesystem's iteration order.
    for index in 0..MAX_DEMANDS + 1 {
        let mut expired = old.clone();
        expired.db_id = ulid::Ulid::from(index as u128 + 1).to_string();
        expired.validate(expired.observed_at_unix_secs).unwrap();
        let mut file = options()
            .write(true)
            .create_new(true)
            .open(parent.join(format!("{}.json", expired.db_id)))
            .unwrap();
        file.write_all(&serde_json::to_vec(&expired).unwrap())
            .unwrap();
    }
    let inventory = || -> BTreeMap<_, _> {
        std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), std::fs::read(entry.path()).unwrap())
            })
            .collect()
    };
    let before = inventory();
    assert!(before.len() > MAX_DEMANDS + 2);

    std::fs::write(&workspace.config, workspace.text().replace(DB, NEXT_DB)).unwrap();
    let selected = workspace.selection();
    let active = &selected.active[0];
    let replacement = DemandRecord::from_selection(&selected, active, NOW + MAX_DEMAND_AGE_SECS)
        .unwrap()
        .unwrap();
    let new_path = demand_path(&selected, active).unwrap().unwrap();
    assert!(persist(&new_path, &replacement).is_err());
    assert!(!new_path.exists());
    assert_eq!(inventory(), before);
}

#[test]
fn contended_catalog_lock_refuses_demand_without_waiting() {
    let workspace = Workspace::new();
    let (_, mut record, path) = workspace.demand();
    persist(&path, &record).unwrap();
    let held = options()
        .read(true)
        .write(true)
        .open(path.parent().unwrap().join(".lock"))
        .unwrap();
    held.try_lock().unwrap();
    record.observed_at_unix_secs += COALESCE_SECS;
    let (sender, receiver) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        sender
            .send(persist(&path, &record).map_err(|error| error.to_string()))
            .unwrap();
    });
    let result = receiver.recv_timeout(std::time::Duration::from_secs(1));
    // Release even if the regression waits, so a failing test can still join.
    drop(held);
    writer.join().unwrap();
    assert!(
        result
            .expect("demand writer waited on the catalog lock")
            .unwrap_err()
            .contains("busy")
    );
}

#[test]
fn malformed_existing_demand_does_not_get_silently_replaced() {
    let workspace = Workspace::new();
    let (_, record, path) = workspace.demand();
    persist(&path, &record).unwrap();
    std::fs::write(&path, b"malformed").unwrap();
    assert!(plan_at(&path, NOW).is_err());
    assert!(persist(&path, &record).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"malformed");
    assert!(
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .all(|entry| {
                entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "tmp")
            })
    );
}
