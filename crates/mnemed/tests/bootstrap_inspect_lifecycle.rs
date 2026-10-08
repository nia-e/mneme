#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ulid::Ulid;

const DB_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OPERATION_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("mneme-bootstrap-inspect-{label}-{}", Ulid::new()));
        fs::create_dir(&path).expect("create inspection fixture root");
        fs::create_dir(path.join(".git")).expect("mark fixture as a Git repository root");
        Self(path)
    }

    fn root(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TreeEntry {
    path: String,
    kind: &'static str,
    mode: u32,
    len: u64,
    mtime_seconds: i64,
    mtime_nanoseconds: i64,
    content_sha256: Option<String>,
    symlink_target: Option<String>,
}

#[test]
fn absent_conventional_active_and_interrupted_states_are_mutation_free() {
    let absent = Scratch::new("absent");
    let absent_report = inspect_without_mutation(absent.root());
    assert_eq!(absent_report["state"], "greenfield_absent");
    assert_eq!(absent_report["recommended_mode"], "greenfield");
    assert_eq!(absent_report["authoritative"], false);
    assert_eq!(absent_report["retry_safe"], true);
    assert!(
        Ulid::from_string(
            absent_report["suggested_target_db_id"]
                .as_str()
                .expect("greenfield target id")
        )
        .is_ok()
    );
    assert!(
        Ulid::from_string(
            absent_report["suggested_operation_id"]
                .as_str()
                .expect("greenfield operation id")
        )
        .is_ok()
    );
    assert!(!absent.root().join(".mneme").exists());

    let planned_absent = Scratch::new("planned-absent");
    fs::create_dir_all(planned_absent.root().join(".mneme/bootstrap")).unwrap();
    fs::write(
        planned_absent.root().join(".mneme/bootstrap/draft.json"),
        b"{}",
    )
    .unwrap();
    let planned_absent_report = inspect_without_mutation(planned_absent.root());
    assert_eq!(planned_absent_report["state"], "greenfield_absent");
    assert_eq!(planned_absent_report["lease"], "absent");
    assert!(
        !planned_absent
            .root()
            .join(".mneme/memory.db.mneme.lock")
            .exists(),
        "inspection must not create the conventional lease inode"
    );

    let conventional = Scratch::new("conventional");
    let mneme = conventional.root().join(".mneme");
    fs::create_dir(&mneme).unwrap();
    fs::create_dir(mneme.join("snapshots")).unwrap();
    fs::write(
        mneme.join("memory.db"),
        b"SQLite format 3\0read-only inspection fixture",
    )
    .unwrap();
    fs::write(mneme.join("memory.db-wal"), b"wal must survive").unwrap();
    fs::write(mneme.join("memory.db-shm"), b"shm must survive").unwrap();
    let conventional_report = inspect_without_mutation(conventional.root());
    assert_eq!(conventional_report["state"], "conventional_unmanaged");
    assert_eq!(conventional_report["database_format"], "cozo_sqlite");
    assert_eq!(
        conventional_report["recommended_mode"],
        "brownfield_proposal"
    );
    assert_eq!(conventional_report["existing_db_id"], Value::Null);

    let active = Scratch::new("active");
    create_native_active_fixture(active.root());
    fs::create_dir(active.root().join(".mneme/snapshots")).unwrap();
    let active_report = inspect_without_mutation(active.root());
    assert_eq!(active_report["state"], "active_managed");
    assert_eq!(active_report["layout"], "native_generation");
    assert_eq!(active_report["existing_db_id"], DB_ID);
    assert_eq!(active_report["existing_generation"], 1);
    assert_eq!(active_report["existing_operation_id"], OPERATION_ID);
    assert_eq!(
        active_report["recommended_mode"],
        "managed_refresh_proposal"
    );
    assert_eq!(active_report["apply_capability"], "proposal_only");

    let interrupted = Scratch::new("interrupted");
    fs::create_dir_all(interrupted.root().join(".mneme/generations")).unwrap();
    let interrupted_report = inspect_without_mutation(interrupted.root());
    assert_eq!(interrupted_report["state"], "interrupted_activation");
    assert_eq!(
        interrupted_report["apply_capability"],
        "exact_bootstrap_retry_only"
    );
    assert_eq!(
        interrupted_report["blocker"]["kind"],
        "activation_incomplete"
    );
}

#[test]
fn ordinary_project_owners_and_partial_bindings_cannot_become_greenfield() {
    let initialized = Scratch::new("init-empty-store");
    fs::create_dir(initialized.root().join(".mneme")).unwrap();
    let database = initialized.root().join(".mneme/codex-memory.db");
    #[cfg(feature = "cozo")]
    {
        // The ordinary init owner's empty Cozo store, not a bootstrap generation.
        let store = mneme_cozo::CozoStore::open(database.to_str().unwrap(), 384).unwrap();
        store.prepare_for_file_move().unwrap();
        drop(store);
    }
    #[cfg(not(feature = "cozo"))]
    fs::write(
        &database,
        br#"{"db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","nodes":[],"edges":[]}"#,
    )
    .unwrap();
    assert_ordinary_owner_refused_without_mutation(initialized.root());

    for (label, relative, bytes) in [
        (
            "remote-cli-owner",
            ".mneme/cli.json",
            r#"{"schema":"mneme.cli.owner.v1","url":"https://offline.invalid/mcp","database":"project","db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}"#,
        ),
        (
            "broken-cli-owner",
            ".mneme/cli.json",
            "unfinished CLI owner must not mean absence",
        ),
        (
            "partial-service",
            ".mneme/service.json",
            "unfinished service binding must not mean absence",
        ),
        (
            "remote",
            ".codex/config.toml",
            "[mcp_servers.mneme_project]\nurl = 'https://offline.invalid/mcp'\n",
        ),
        (
            "alternate",
            ".codex/config.toml",
            "[mcp_servers.mneme_project]\ncommand = 'python3'\nargs = ['/elsewhere/launcher.py', '--service-config', '/elsewhere/service.json']\n",
        ),
        (
            "disabled",
            ".codex/config.toml",
            "[mcp_servers.mneme_project]\nenabled = false\nurl = 'https://offline.invalid/mcp'\n",
        ),
        (
            "partial-profile",
            ".mneme/profile.json",
            "broken profile must not mean absence",
        ),
        (
            "partial-hook",
            ".codex/hooks.json",
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"python3 /copied/hooks.py --config /missing/hooks.json"}]}]}}"#,
        ),
        (
            "orphan-wal",
            ".mneme/codex-memory.db-wal",
            "orphan owner sidecar",
        ),
        (
            "orphan-lease",
            ".mneme/codex-memory.db.mneme.lock",
            "partial owner lease",
        ),
    ] {
        let fixture = Scratch::new(label);
        let marker = fixture.root().join(relative);
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(marker, bytes).unwrap();
        assert_ordinary_owner_refused_without_mutation(fixture.root());
        if matches!(label, "remote-cli-owner" | "remote") {
            fs::create_dir_all(fixture.root().join(".mneme/generations")).unwrap();
            assert_ordinary_owner_refused_without_mutation(fixture.root());
            fs::create_dir(
                fixture
                    .root()
                    .join(".mneme/generations")
                    .join(format!(".bootstrap-{OPERATION_ID}-{}", Ulid::new())),
            )
            .unwrap();
            assert_ordinary_owner_refused_without_mutation(fixture.root());
        }
    }

    let configured_native = Scratch::new("native-with-owner-binding");
    create_native_active_fixture(configured_native.root());
    fs::write(configured_native.root().join(".mneme/cli.json"), b"{}").unwrap();
    assert_eq!(
        inspect_without_mutation(configured_native.root())["state"],
        "active_managed"
    );
    fs::write(
        configured_native.root().join(".mneme/codex-memory.db"),
        b"second graph",
    )
    .unwrap();
    assert_ordinary_owner_refused_without_mutation(configured_native.root());

    let broken = Scratch::new("malformed-config");
    fs::create_dir(broken.root().join(".codex")).unwrap();
    fs::write(broken.root().join(".codex/config.toml"), b"[mcp_servers.").unwrap();
    let report = inspect_without_mutation(broken.root());
    assert_eq!(report["state"], "blocked_malformed");
    assert_eq!(report["blocker"]["kind"], "unreadable_project_binding");
    assert_create_refused_without_mutation(broken.root(), "cannot be safely classified");
    assert!(!broken.root().join(".mneme").exists());

    // Existing Codex settings and authored text are not project enrollment.
    let unrelated = Scratch::new("unrelated-codex");
    fs::create_dir(unrelated.root().join(".codex")).unwrap();
    fs::write(unrelated.root().join(".codex/config.toml"),
        "developer_instructions = 'Mneme is neat'\n[mcp_servers.other]\nurl = 'https://offline.invalid/mcp'\n").unwrap();
    fs::write(
        unrelated.root().join(".codex/hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"prompt","prompt":"Mneme is neat"}]}]}}"#,
    )
    .unwrap();
    assert_eq!(
        inspect_without_mutation(unrelated.root())["state"],
        "greenfield_absent"
    );
    assert!(!unrelated.root().join(".mneme").exists());
}

fn assert_ordinary_owner_refused_without_mutation(root: &Path) {
    let report = inspect_without_mutation(root);
    assert_eq!(report["state"], "blocked_malformed");
    assert_eq!(report["blocker"]["kind"], "ordinary_project_owner_boundary");
    assert_eq!(report["recommended_mode"], Value::Null);
    assert_create_refused_without_mutation(root, "must not create a parallel graph");
}

fn assert_create_refused_without_mutation(root: &Path, error: &str) {
    // Missing plan/approval is intentional: owner admission must refuse before
    // sidecar validation, private validator extraction, mkdir or lease creation.
    for dry_run in [false, true] {
        for json in [false, true] {
            let before = snapshot_tree(root);
            let mut command = Command::new(env!("CARGO_BIN_EXE_mnemed"));
            command.current_dir(root).env_remove("MNEME_DB");
            if json {
                command.arg("--json");
            }
            command
                .args(["bootstrap-create", "--root"])
                .arg(root)
                .arg("--plan")
                .arg(root.join(".mneme/bootstrap/missing-plan.json"))
                .arg("--approval")
                .arg(root.join(".mneme/bootstrap/missing-approval.json"))
                .args(["--operation-id", OPERATION_ID]);
            if dry_run {
                command.arg("--dry-run");
            }
            let output = command.output().unwrap();
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(error),
                "unexpected refusal: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                snapshot_tree(root),
                before,
                "refused bootstrap-create mutated state"
            );
        }
    }
}

#[test]
fn malformed_orphan_and_symlink_layouts_fail_closed_without_mutation() {
    let malformed = Scratch::new("malformed");
    fs::create_dir(malformed.root().join(".mneme")).unwrap();
    fs::write(malformed.root().join(".mneme/current"), b"not a selector").unwrap();
    let report = inspect_without_mutation(malformed.root());
    assert_eq!(report["state"], "blocked_malformed");
    assert_eq!(report["blocker"]["kind"], "selector_not_symlink");
    assert!(report["blocker"]["action"].as_str().unwrap().len() > 20);

    let orphan = Scratch::new("orphan");
    fs::create_dir(orphan.root().join(".mneme")).unwrap();
    symlink(
        Path::new("generations").join(OPERATION_ID),
        orphan.root().join(".mneme/current"),
    )
    .unwrap();
    let report = inspect_without_mutation(orphan.root());
    assert_eq!(report["state"], "blocked_orphan");
    assert_eq!(report["blocker"]["kind"], "orphan_selector");

    let orphan_sidecar = Scratch::new("orphan-sidecar");
    fs::create_dir(orphan_sidecar.root().join(".mneme")).unwrap();
    fs::write(
        orphan_sidecar.root().join(".mneme/memory.db-wal"),
        b"orphan wal",
    )
    .unwrap();
    let report = inspect_without_mutation(orphan_sidecar.root());
    assert_eq!(report["state"], "blocked_orphan");
    assert_eq!(report["blocker"]["kind"], "orphan_sqlite_sidecar");

    let orphan_snapshots = Scratch::new("orphan-snapshots");
    fs::create_dir_all(orphan_snapshots.root().join(".mneme/snapshots")).unwrap();
    let report = inspect_without_mutation(orphan_snapshots.root());
    assert_eq!(report["state"], "blocked_orphan");
    assert_eq!(report["blocker"]["kind"], "orphan_snapshot_directory");
    assert!(orphan_snapshots.root().join(".mneme/snapshots").is_dir());

    let snapshots_file = Scratch::new("snapshots-file");
    fs::create_dir(snapshots_file.root().join(".mneme")).unwrap();
    fs::write(
        snapshots_file.root().join(".mneme/snapshots"),
        b"not a directory",
    )
    .unwrap();
    let report = inspect_without_mutation(snapshots_file.root());
    assert_eq!(report["state"], "blocked_malformed");
    assert_eq!(report["blocker"]["kind"], "snapshots_not_directory");

    let snapshots_symlink = Scratch::new("snapshots-symlink");
    fs::create_dir(snapshots_symlink.root().join(".mneme")).unwrap();
    symlink(
        snapshots_symlink.root(),
        snapshots_symlink.root().join(".mneme/snapshots"),
    )
    .unwrap();
    let report = inspect_without_mutation(snapshots_symlink.root());
    assert_eq!(report["state"], "blocked_symlink");
    assert_eq!(report["blocker"]["kind"], "symlink_layout");

    let tampered = Scratch::new("tampered-native");
    create_native_active_fixture(tampered.root());
    let receipt = tampered
        .root()
        .join(".mneme/generations")
        .join(OPERATION_ID)
        .join("bootstrap/receipt.json");
    let mut bytes = fs::read(&receipt).unwrap();
    let position = bytes
        .windows(b"fixture-reviewer".len())
        .position(|window| window == b"fixture-reviewer")
        .expect("fixture receipt contains reviewer identity");
    bytes[position] = b'F';
    fs::write(&receipt, bytes).unwrap();
    let report = inspect_without_mutation(tampered.root());
    assert_eq!(report["state"], "blocked_malformed");
    assert_eq!(
        report["blocker"]["kind"],
        "native_receipt_authentication_failed"
    );

    let redirected = Scratch::new("symlink");
    let outside = std::env::temp_dir().join(format!("mneme-inspect-outside-{}", Ulid::new()));
    fs::create_dir(&outside).unwrap();
    let marker = outside.join("marker");
    fs::write(&marker, b"must remain untouched").unwrap();
    symlink(&outside, redirected.root().join(".mneme")).unwrap();
    let marker_before = fs::read(&marker).unwrap();
    let report = inspect_without_mutation(redirected.root());
    assert_eq!(report["state"], "blocked_symlink");
    assert_eq!(report["blocker"]["kind"], "symlink_layout");
    assert_eq!(fs::read(&marker).unwrap(), marker_before);
    fs::remove_dir_all(outside).unwrap();
}

#[test]
fn existing_held_lease_is_observed_without_creating_or_mutating_it() {
    let fixture = Scratch::new("leased");
    let mneme = fixture.root().join(".mneme");
    fs::create_dir(&mneme).unwrap();
    let database = mneme.join("memory.db");
    fs::write(
        &database,
        b"SQLite format 3\0leased read-only inspection fixture",
    )
    .unwrap();
    fs::write(mneme.join("memory.db-wal"), b"live wal bytes").unwrap();
    fs::write(mneme.join("memory.db-shm"), b"live shm bytes").unwrap();
    let _lease = mneme_store_path::StoreLease::acquire(&database).unwrap();

    let report = inspect_without_mutation(fixture.root());
    assert_eq!(report["state"], "conventional_unmanaged");
    assert_eq!(report["lease"], "held");
    assert_eq!(report["apply_capability"], "proposal_only");

    let absent = Scratch::new("leased-absent");
    fs::create_dir(absent.root().join(".mneme")).unwrap();
    let absent_database = absent.root().join(".mneme/memory.db");
    let _absent_lease = mneme_store_path::StoreLease::acquire(&absent_database).unwrap();
    let report = inspect_without_mutation(absent.root());
    assert_eq!(report["state"], "greenfield_absent");
    assert_eq!(report["lease"], "held");
    assert_eq!(report["apply_capability"], "blocked_until_lease_release");
    assert_eq!(report["blocker"]["kind"], "lease_held");
}

fn inspect_without_mutation(root: &Path) -> Value {
    let before = snapshot_tree(root);
    let output = Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .current_dir(root)
        .env_remove("MNEME_DB")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .args(["--json", "bootstrap-inspect", "--root"])
        .arg(root)
        .output()
        .expect("run bootstrap-inspect");
    assert_success(&output);
    let after = snapshot_tree(root);
    assert_eq!(after, before, "bootstrap-inspect mutated repository state");
    serde_json::from_slice(&output.stdout).expect("parse bootstrap inspection JSON")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "bootstrap-inspect failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn snapshot_tree(root: &Path) -> Vec<TreeEntry> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<TreeEntry>) {
        let mut children: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        children.sort();
        for child in children {
            let metadata = fs::symlink_metadata(&child).unwrap();
            let relative = child
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .to_string();
            let (kind, content_sha256, symlink_target) = if metadata.file_type().is_symlink() {
                (
                    "symlink",
                    None,
                    Some(fs::read_link(&child).unwrap().to_string_lossy().to_string()),
                )
            } else if metadata.is_dir() {
                ("directory", None, None)
            } else {
                (
                    "file",
                    Some(hex(Sha256::digest(fs::read(&child).unwrap()).as_slice())),
                    None,
                )
            };
            entries.push(TreeEntry {
                path: relative,
                kind,
                mode: metadata.permissions().mode(),
                len: metadata.len(),
                mtime_seconds: metadata.mtime(),
                mtime_nanoseconds: metadata.mtime_nsec(),
                content_sha256,
                symlink_target,
            });
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                visit(root, &child, entries);
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries
}

#[derive(Serialize)]
struct TestEffectivePolicy {
    policy_version: String,
    automatic_similarity_links: bool,
    automatic_coretrieval_links: bool,
    automatic_bridges: bool,
    node_id_scheme: String,
    timestamp_scheme: String,
    source_timestamp_ms: u64,
    body_store: String,
    backend_format: String,
    embedding_fingerprint: Value,
}

#[derive(Serialize)]
struct TestReceiptEdge {
    from_node_id: String,
    to_node_id: String,
}

#[derive(Serialize)]
struct TestReceiptPayload {
    schema_version: u64,
    namespace: String,
    plan_hash: String,
    approval_hash: String,
    reviewer_id: String,
    db_id: String,
    storage_incarnation: String,
    storage_generation: u64,
    operation_id: String,
    absent_target_precondition: bool,
    effective_policy: TestEffectivePolicy,
    policy_fingerprint: String,
    projection_digest: String,
    node_ids: BTreeMap<String, String>,
    edge_ids: BTreeMap<String, TestReceiptEdge>,
    manifest_path: String,
    manifest_hash: String,
    activation_mode: String,
    activation_target: String,
    publication_state: String,
    key_id: String,
}

#[derive(Serialize)]
struct TestReceipt {
    payload: TestReceiptPayload,
    mac_hmac_sha256: String,
}

const FIXED_KEY_RECEIPT_MAC: &str =
    "7c9daa5ee0916bccfd2e06453ae3fa249da525b751418d7a0e209788c7fb6b79";
const FIXED_KEY_RECEIPT_BYTES: &[u8] = br#"{
  "payload": {
    "schema_version": 1,
    "namespace": "repo-sync-v1-native-bootstrap-receipt",
    "plan_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "approval_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "reviewer_id": "fixture-reviewer",
    "db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "storage_incarnation": "fixture-incarnation",
    "storage_generation": 1,
    "operation_id": "01ARZ3NDEKTSV4RRFFQ69G5FAW",
    "absent_target_precondition": true,
    "effective_policy": {
      "policy_version": "repo-sync-v1-default",
      "automatic_similarity_links": false,
      "automatic_coretrieval_links": false,
      "automatic_bridges": false,
      "node_id_scheme": "sha256(plan_hash,key)-128-v1",
      "timestamp_scheme": "git-source-commit-time-ms-v1",
      "source_timestamp_ms": 0,
      "body_store": "generation-local-fs-relative-v1",
      "backend_format": "mneme-json-snapshot-v1",
      "embedding_fingerprint": null
    },
    "policy_fingerprint": "7be9ccb42b35b8c23bc7cfd0d994fad9e75c9fbfa7e69f2eba6517d02da2c808",
    "projection_digest": "fixture-projection",
    "node_ids": {},
    "edge_ids": {},
    "manifest_path": "bootstrap/manifest.json",
    "manifest_hash": "1fc4aad6d855c2ccdda2dbe8756cf8265b64b6730a6118b0510c12a421131f1b",
    "activation_mode": "atomic-no-clobber-relative-symlink-v1",
    "activation_target": "generations/01ARZ3NDEKTSV4RRFFQ69G5FAW",
    "publication_state": "activation-bound",
    "key_id": "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0"
  },
  "mac_hmac_sha256": "7c9daa5ee0916bccfd2e06453ae3fa249da525b751418d7a0e209788c7fb6b79"
}"#;

fn create_native_active_fixture(root: &Path) {
    let mneme = root.join(".mneme");
    let generation = mneme.join("generations").join(OPERATION_ID);
    fs::create_dir_all(generation.join("bootstrap")).unwrap();
    fs::create_dir(generation.join("memory.bodies")).unwrap();
    fs::create_dir_all(mneme.join("bootstrap")).unwrap();
    fs::set_permissions(&generation, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        generation.join("memory.db"),
        format!("{{\"db_id\":\"{DB_ID}\"}}"),
    )
    .unwrap();

    let manifest = json!({
        "schema_version": 1,
        "namespace": "repo-sync-v1",
        "db_id": DB_ID,
        "generation": 1,
        "repo": {
            "head": "0000000000000000000000000000000000000000",
            "tree": "0000000000000000000000000000000000000000",
            "object_format": "sha1",
            "dirty_digest": null
        },
        "files": {},
        "nodes": {},
        "edges": {},
        "applied_plan_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    });
    fs::write(
        generation.join("bootstrap/manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let manifest_hash = hex(Sha256::digest(serde_json::to_vec(&manifest).unwrap()).as_slice());

    let key = [7u8; 32];
    let key_path = mneme.join("bootstrap/native-bootstrap.key");
    fs::write(&key_path, key).unwrap();
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
    let policy = TestEffectivePolicy {
        policy_version: "repo-sync-v1-default".to_string(),
        automatic_similarity_links: false,
        automatic_coretrieval_links: false,
        automatic_bridges: false,
        node_id_scheme: "sha256(plan_hash,key)-128-v1".to_string(),
        timestamp_scheme: "git-source-commit-time-ms-v1".to_string(),
        source_timestamp_ms: 0,
        body_store: "generation-local-fs-relative-v1".to_string(),
        backend_format: "mneme-json-snapshot-v1".to_string(),
        embedding_fingerprint: json!(null),
    };
    let policy_fingerprint = hex(Sha256::digest(serde_json::to_vec(&policy).unwrap()).as_slice());
    let payload = TestReceiptPayload {
        schema_version: 1,
        namespace: "repo-sync-v1-native-bootstrap-receipt".to_string(),
        plan_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        approval_hash: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .to_string(),
        reviewer_id: "fixture-reviewer".to_string(),
        db_id: DB_ID.to_string(),
        storage_incarnation: "fixture-incarnation".to_string(),
        storage_generation: 1,
        operation_id: OPERATION_ID.to_string(),
        absent_target_precondition: true,
        effective_policy: policy,
        policy_fingerprint,
        projection_digest: "fixture-projection".to_string(),
        node_ids: BTreeMap::new(),
        edge_ids: BTreeMap::new(),
        manifest_path: "bootstrap/manifest.json".to_string(),
        manifest_hash,
        activation_mode: "atomic-no-clobber-relative-symlink-v1".to_string(),
        activation_target: format!("generations/{OPERATION_ID}"),
        publication_state: "activation-bound".to_string(),
        key_id: hex(Sha256::digest(key).as_slice()),
    };
    let compact_payload = serde_json::to_vec(&payload).unwrap();
    let mac = hmac_hex(&key, &compact_payload);
    assert_eq!(mac, FIXED_KEY_RECEIPT_MAC);
    assert_ne!(
        hmac_hex(&key, &serde_json::to_vec_pretty(&payload).unwrap()),
        FIXED_KEY_RECEIPT_MAC,
        "the native receipt MAC input must remain compact JSON"
    );
    let receipt = TestReceipt {
        payload,
        mac_hmac_sha256: mac,
    };
    let receipt_bytes = serde_json::to_vec_pretty(&receipt).unwrap();
    assert_eq!(receipt_bytes, FIXED_KEY_RECEIPT_BYTES);
    assert!(
        receipt
            .mac_hmac_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "native receipt HMAC must remain lowercase hexadecimal"
    );
    assert!(
        !receipt_bytes.ends_with(b"\n"),
        "native receipt must not gain a trailing line feed"
    );
    fs::write(generation.join("bootstrap/receipt.json"), receipt_bytes).unwrap();
    symlink(
        Path::new("generations").join(OPERATION_ID),
        mneme.join("current"),
    )
    .unwrap();
}

fn hmac_hex(key: &[u8], message: &[u8]) -> String {
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for index in 0..64 {
        inner_pad[index] ^= block[index];
        outer_pad[index] ^= block[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    hex(outer.finalize().as_slice())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
