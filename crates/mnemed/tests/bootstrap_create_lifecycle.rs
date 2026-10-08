#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
#[cfg(not(any(feature = "cozo", feature = "fastembed")))]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(not(any(feature = "cozo", feature = "fastembed")))]
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use ulid::Ulid;

const DB_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OPERATION_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const SECOND_OPERATION_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
const BROWNFIELD_OPERATION_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAY";
const MODEL_REPO: &str = "models--Xenova--bge-base-en-v1.5";
#[cfg(not(any(feature = "cozo", feature = "fastembed")))]
const FIXED_NATIVE_KEY: [u8; 32] = [7u8; 32];

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("mneme-bootstrap-e2e-{}", Ulid::new()));
        fs::create_dir(&path).expect("create bootstrap integration-test root");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    scratch: Scratch,
    plan: PathBuf,
    approval: PathBuf,
    model_cache: Option<PathBuf>,
}

impl Fixture {
    fn root(&self) -> &Path {
        &self.scratch.0
    }

    fn bootstrap_command(&self, operation_id: &str, plan: &Path) -> Command {
        self.bootstrap_command_with_approval(operation_id, plan, &self.approval)
    }

    fn bootstrap_command_with_approval(
        &self,
        operation_id: &str,
        plan: &Path,
        approval: &Path,
    ) -> Command {
        let mut command = self.mnemed();
        command
            .arg("--json")
            .arg("bootstrap-create")
            .arg("--root")
            .arg(self.root())
            .arg("--plan")
            .arg(plan)
            .arg("--approval")
            .arg(approval)
            .arg("--operation-id")
            .arg(operation_id);
        command
    }

    fn mnemed(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mnemed"));
        command.current_dir(self.root());
        hermetic_environment(&mut command);
        if let Some(cache) = &self.model_cache {
            command.env("FASTEMBED_CACHE_DIR", cache);
        }
        command
    }
}

#[test]
#[cfg(not(feature = "fastembed"))]
fn unrelated_codex_settings_preserve_empty_and_private_only_greenfield_admission() {
    for private_only in [false, true] {
        let fixture = build_fixture(None);
        let root = fixture.root();
        fs::create_dir(root.join(".codex")).unwrap();
        fs::write(root.join(".codex/config.toml"),
            "developer_instructions = 'Mneme is neat'\n[mcp_servers.other]\nurl = 'https://offline.invalid/mcp'\n").unwrap();
        fs::write(root.join(".codex/hooks.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"prompt","prompt":"Mneme is neat"}]}]}}"#).unwrap();
        let generations = root.join(".mneme/generations");
        fs::create_dir(&generations).unwrap();
        let residue = generations.join(format!(".bootstrap-{OPERATION_ID}-{}", Ulid::new()));
        if private_only {
            fs::create_dir(&residue).unwrap();
        }
        let before = tree_fingerprint(root);
        let mut dry_run = fixture.bootstrap_command(OPERATION_ID, &fixture.plan);
        dry_run.arg("--dry-run");
        assert_eq!(json_stdout(&checked(&mut dry_run))["status"], "validated");
        assert_eq!(
            tree_fingerprint(root),
            before,
            "successful dry-run changed project state"
        );
        let created = checked(&mut fixture.bootstrap_command(OPERATION_ID, &fixture.plan));
        assert_eq!(json_stdout(&created)["status"], "activated");
        assert!(!residue.exists());
    }
}

#[test]
fn ordinary_owner_cannot_be_bypassed_by_empty_or_private_only_generation_catalog() {
    for (relative, binding) in [
        (
            ".mneme/cli.json",
            r#"{"schema":"mneme.cli.owner.v1","url":"https://offline.invalid/mcp","database":"project","db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}"#,
        ),
        (
            ".codex/config.toml",
            "[mcp_servers.mneme_project]\nurl = 'https://offline.invalid/mcp'\n",
        ),
    ] {
        let fixture = build_fixture(None);
        let root = fixture.root();
        let marker = root.join(relative);
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, binding).unwrap();
        let generations = root.join(".mneme/generations");
        fs::create_dir(&generations).unwrap();
        for private_only in [false, true] {
            if private_only {
                fs::create_dir(
                    generations.join(format!(".bootstrap-{OPERATION_ID}-{}", Ulid::new())),
                )
                .unwrap();
            }
            let before = tree_fingerprint(root);
            for dry_run in [false, true] {
                let mut command = fixture.bootstrap_command(OPERATION_ID, &fixture.plan);
                if dry_run {
                    command.arg("--dry-run");
                }
                let output = failed(&mut command);
                assert_stderr_contains(&output, "must not create a parallel graph");
                assert_eq!(
                    tree_fingerprint(root),
                    before,
                    "ordinary-owner refusal changed state with an approved valid plan"
                );
            }
        }
    }
}

fn tree_fingerprint(root: &Path) -> BTreeMap<PathBuf, String> {
    fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, String>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let content = if metadata.is_dir() {
                String::new()
            } else {
                format!("{:x}", Sha256::digest(fs::read(&path).unwrap()))
            };
            result.insert(
                path.strip_prefix(root).unwrap().to_path_buf(),
                format!(
                    "{:?}:{}:{content}",
                    metadata.modified().unwrap(),
                    metadata.len()
                ),
            );
            if metadata.is_dir() {
                visit(root, &path, result);
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

#[test]
fn native_greenfield_bootstrap_lifecycle_is_fail_closed_and_idempotent() {
    let model_cache = if cfg!(feature = "fastembed") {
        let Some(cache) = populated_model_cache() else {
            eprintln!(
                "skipping fastembed bootstrap lifecycle: no complete local BGE cache; network downloads are forbidden in this test"
            );
            return;
        };
        Some(cache)
    } else {
        None
    };
    exercise_lifecycle(model_cache);
}

#[test]
fn native_bootstrap_commands_reject_global_store_routing_without_residue() {
    for routed_by_user in [false, true] {
        for command_name in ["bootstrap-inspect", "bootstrap-create"] {
            let scratch = Scratch::new();
            let root = &scratch.0;
            let mut command = Command::new(env!("CARGO_BIN_EXE_mnemed"));
            command.current_dir(root);
            hermetic_environment(&mut command);
            if routed_by_user {
                command.arg("--user");
            } else {
                command.arg("--db").arg(root.join("routed-memory.db"));
            }
            command
                .arg("--json")
                .arg(command_name)
                .arg("--root")
                .arg(root);
            if command_name == "bootstrap-create" {
                command
                    .arg("--plan")
                    .arg(root.join("missing-plan.json"))
                    .arg("--approval")
                    .arg(root.join("missing-approval.json"))
                    .arg("--operation-id")
                    .arg(OPERATION_ID);
            }

            let rejected = failed(&mut command);
            assert_stderr_contains(&rejected, "--db and --user are forbidden");
            assert!(
                fs::read_dir(root).unwrap().next().is_none(),
                "rejected {command_name} routing left repository state"
            );
        }
    }
}

fn exercise_lifecycle(model_cache: Option<PathBuf>) {
    let fixture = build_fixture(model_cache);
    let root = fixture.root();
    let bootstrap = root.join(".mneme/bootstrap");
    let invalid_sidecars: Vec<_> = [
        (
            "blank",
            " \n\t".to_owned(),
            "summary is blank or noncanonical",
            "node summary must not be blank",
        ),
        (
            "policy-oversized",
            "x".repeat(1_025),
            "summary exceeds hard maximum",
            "repo-sync-v1 hard maximum is 1024",
        ),
    ]
    .into_iter()
    .map(|(label, summary, validator_error, canonical_error)| {
        let (plan, approval) = reapprove_plan_with_summary(&fixture, label, &summary);
        (label, plan, approval, validator_error, canonical_error)
    })
    .collect();

    // Even an approval that exactly binds malformed plan bytes must fail while
    // the target still consists solely of caller-supplied sidecars.
    for (label, plan, approval, validator_error, _) in &invalid_sidecars {
        let rejected =
            failed(&mut fixture.bootstrap_command_with_approval(OPERATION_ID, plan, approval));
        assert_stderr_contains(&rejected, validator_error);
        assert!(
            !root.join(".mneme/generations").exists()
                && !root.join(".mneme/current").exists()
                && !root.join(".mneme/bootstrap-create.lock").exists()
                && !bootstrap.join("native-bootstrap.key").exists(),
            "rejected {label} summary left native bootstrap residue"
        );
    }

    let orphan_snapshots = root.join(".mneme/snapshots");
    fs::create_dir(&orphan_snapshots).unwrap();
    let rejected = failed(&mut fixture.bootstrap_command(OPERATION_ID, &fixture.plan));
    assert_stderr_contains(&rejected, "orphan .mneme/snapshots");
    assert!(
        orphan_snapshots.is_dir()
            && !root.join(".mneme/generations").exists()
            && !root.join(".mneme/current").exists()
            && !root.join(".mneme/bootstrap-create.lock").exists()
            && !bootstrap.join("native-bootstrap.key").exists(),
        "snapshot-only create refusal must preserve residue and create no native state"
    );
    fs::remove_dir(&orphan_snapshots).unwrap();

    let mut dry_run = fixture.bootstrap_command(OPERATION_ID, &fixture.plan);
    dry_run.arg("--dry-run");
    let dry_run = checked(&mut dry_run);
    assert_eq!(json_stdout(&dry_run)["status"], "validated");
    assert!(!root.join(".mneme/generations").exists());
    assert!(!root.join(".mneme/current").exists());
    assert!(!bootstrap.join("native-bootstrap.key").exists());

    let configured_database = root.join(".mneme/memory.db");
    let competing_lease = mneme_store_path::StoreLease::acquire(&configured_database)
        .expect("hold the ordinary frontend lease");
    let blocked = failed(&mut fixture.bootstrap_command(OPERATION_ID, &fixture.plan));
    assert_stderr_contains(&blocked, "already owned");
    assert!(!root.join(".mneme/generations").exists());
    assert!(!root.join(".mneme/current").exists());
    drop(competing_lease);

    #[cfg(not(any(feature = "cozo", feature = "fastembed")))]
    let native_key = {
        let native_key = bootstrap.join("native-bootstrap.key");
        fs::write(&native_key, FIXED_NATIVE_KEY).expect("preinstall deterministic native key");
        fs::set_permissions(&native_key, fs::Permissions::from_mode(0o600))
            .expect("make deterministic native key private");
        native_key
    };

    let abandoned_temp = root
        .join(".mneme/generations")
        .join(format!(".bootstrap-{OPERATION_ID}-{}", Ulid::new()));
    fs::create_dir_all(abandoned_temp.join("partial-bodies"))
        .expect("create an abandoned operation-scoped generation");

    let created = checked(&mut fixture.bootstrap_command(OPERATION_ID, &fixture.plan));
    assert_eq!(json_stdout(&created)["status"], "activated");
    // Attaching an owner to this native graph is not a second graph, and must
    // not break its authenticated exact retry/recovery later in this test.
    fs::write(
        root.join(".mneme/cli.json"),
        serde_json::to_vec(&json!({
            "schema": "mneme.cli.owner.v1", "url": "https://offline.invalid/mcp",
            "database": "project", "db_id": DB_ID,
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(
        !abandoned_temp.exists(),
        "bootstrap should clean its own abandoned generation temp"
    );
    let generation = root.join(".mneme/generations").join(OPERATION_ID);
    let database = generation.join("memory.db");
    let manifest = generation.join("bootstrap/manifest.json");
    let receipt = generation.join("bootstrap/receipt.json");
    assert_eq!(
        fs::read_link(root.join(".mneme/current")).expect("read activation"),
        Path::new("generations").join(OPERATION_ID)
    );
    assert!(database.is_file());
    assert!(manifest.is_file());
    assert!(receipt.is_file());
    let mut generation_inventory: Vec<String> = fs::read_dir(&generation)
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .file_name()
                .into_string()
                .expect("generation entry is UTF-8")
        })
        .collect();
    generation_inventory.sort();
    let mut expected_inventory = vec![
        "bootstrap".to_owned(),
        "memory.bodies".to_owned(),
        "memory.db".to_owned(),
        "memory.db.mneme.lock".to_owned(),
    ];
    expected_inventory.sort();
    assert_eq!(
        generation_inventory, expected_inventory,
        "activated generation retains inner fresh-stage or SQLite residue"
    );

    let resolved = mneme_store_path::default_store_path(&root.join(".mneme"))
        .expect("shared resolver accepts native activation");
    assert_eq!(
        resolved,
        fs::canonicalize(&database).expect("canonical created database")
    );
    let mut status = fixture.mnemed();
    status.args(["--db", ".mneme/memory.db", "--json", "status"]);
    assert_eq!(json_stdout(&checked(&mut status))["nodes"], 1);

    let mut direct_generation = fixture.mnemed();
    direct_generation
        .arg("--db")
        .arg(&database)
        .args(["--json", "status"]);
    let direct_generation = failed(&mut direct_generation);
    assert_stderr_contains(&direct_generation, "generation-local and bootstrap-owned");

    for (label, invalid_plan, invalid_approval, _, canonical_error) in &invalid_sidecars {
        let residue = root
            .join(".mneme/generations")
            .join(format!(".bootstrap-{OPERATION_ID}-{}", Ulid::new()));
        fs::create_dir_all(residue.join("partial-bodies"))
            .expect("create retry residue that invalid input must not collect");

        let invalid_retry = failed(&mut fixture.bootstrap_command_with_approval(
            OPERATION_ID,
            invalid_plan,
            invalid_approval,
        ));
        assert_stderr_contains(&invalid_retry, canonical_error);
        assert!(
            residue.is_dir(),
            "canonical {label} summary validation must precede retry cleanup"
        );
        fs::remove_dir_all(residue).unwrap();
    }

    let inventory_script =
        workspace_root().join(".agents/skills/mneme-bootstrap/scripts/inventory_repo.py");
    let mut activated_inventory = python_command();
    activated_inventory
        .arg(inventory_script)
        .arg("--root")
        .arg(root)
        .current_dir(root);
    let activated_inventory = json_stdout(&checked(&mut activated_inventory));
    assert_eq!(
        activated_inventory["manifest"]["path"],
        ".mneme/bootstrap/manifest.json"
    );
    assert_eq!(activated_inventory["manifest"]["status"], "valid");
    assert_eq!(activated_inventory["manifest"]["db_id"], DB_ID);

    let receipt_before = fs::read(&receipt).expect("read creation receipt");
    let manifest_before = fs::read(&manifest).expect("read creation manifest");
    // The dependency-free reference backend is the deterministic S4 byte gate;
    // feature backends intentionally bind different policy/projection bytes.
    #[cfg(not(any(feature = "cozo", feature = "fastembed")))]
    {
        assert_eq!(fs::read(&native_key).unwrap(), FIXED_NATIVE_KEY);
        assert_native_receipt_byte_identity(&receipt_before);
    }

    // Reconstruct the durable crash boundary: generation publication succeeded,
    // but the no-clobber activation symlink was not installed yet.
    fs::remove_file(root.join(".mneme/current")).expect("remove activation");
    let nested_fresh_residue = generation.join(format!(".mneme-fresh-store-v1-{OPERATION_ID}"));
    fs::create_dir(&nested_fresh_residue).expect("create nested fresh-stage residue");
    let residue_recovery = failed(&mut fixture.bootstrap_command(OPERATION_ID, &fixture.plan));
    assert_stderr_contains(&residue_recovery, "unexpected entry");
    assert!(
        !root.join(".mneme/current").exists(),
        "a generation with nested fresh-stage residue must not activate"
    );
    assert!(
        nested_fresh_residue.is_dir(),
        "failed recovery must preserve unrecognized inner residue for review"
    );
    fs::remove_dir(&nested_fresh_residue).unwrap();
    let recovery_database = root
        .join(".mneme/generations")
        .join(OPERATION_ID)
        .join("memory.db");
    let recovery_lease = mneme_store_path::StoreLease::acquire(&recovery_database)
        .expect("hold the published generation lease during interrupted activation");
    let blocked_recovery = failed(&mut fixture.bootstrap_command(OPERATION_ID, &fixture.plan));
    assert_stderr_contains(&blocked_recovery, "already owned");
    assert!(!root.join(".mneme/current").exists());
    drop(recovery_lease);
    let recovered = checked(&mut fixture.bootstrap_command(OPERATION_ID, &fixture.plan));
    assert_eq!(json_stdout(&recovered)["status"], "activated");
    assert_eq!(fs::read(&receipt).unwrap(), receipt_before);

    let replay = checked(&mut fixture.bootstrap_command(OPERATION_ID, &fixture.plan));
    assert_eq!(json_stdout(&replay)["status"], "activated");
    assert_eq!(fs::read(&receipt).unwrap(), receipt_before);

    let mut tampered_manifest: Value =
        serde_json::from_slice(&manifest_before).expect("parse native manifest");
    tampered_manifest["generation"] = json!(2);
    fs::write(
        &manifest,
        serde_json::to_vec_pretty(&tampered_manifest).unwrap(),
    )
    .unwrap();
    let rejected = failed(&mut fixture.bootstrap_command(OPERATION_ID, &fixture.plan));
    assert_stderr_contains(&rejected, "manifest");
    fs::write(&manifest, &manifest_before).unwrap();

    let mut tampered_receipt: Value =
        serde_json::from_slice(&receipt_before).expect("parse native receipt");
    let mac = tampered_receipt["mac_hmac_sha256"]
        .as_str()
        .expect("receipt MAC")
        .to_owned();
    let replacement = if mac.starts_with('0') { '1' } else { '0' };
    tampered_receipt["mac_hmac_sha256"] = json!(format!("{replacement}{}", &mac[1..]));
    fs::write(
        &receipt,
        serde_json::to_vec_pretty(&tampered_receipt).unwrap(),
    )
    .unwrap();
    let rejected = failed(&mut fixture.bootstrap_command(OPERATION_ID, &fixture.plan));
    assert_stderr_contains(&rejected, "authentication");
    fs::write(&receipt, &receipt_before).unwrap();

    let mut ingest = fixture.mnemed();
    ingest.args(["--db", ".mneme/memory.db"]);
    ingest.args([
        "--json",
        "ingest",
        "--summary",
        "Legitimate post-bootstrap memory",
        "--body",
        "An exact bootstrap retry must preserve legitimate later graph mutation.",
    ]);
    checked(&mut ingest);
    let replay_after_mutation =
        checked(&mut fixture.bootstrap_command(OPERATION_ID, &fixture.plan));
    assert_eq!(json_stdout(&replay_after_mutation)["status"], "activated");
    assert_eq!(fs::read(&receipt).unwrap(), receipt_before);
    let mut status = fixture.mnemed();
    status.args(["--db", ".mneme/memory.db", "--json", "status"]);
    assert_eq!(json_stdout(&checked(&mut status))["nodes"], 2);

    let manifest_value: Value = serde_json::from_slice(&manifest_before).unwrap();
    assert_eq!(manifest_value["db_id"], DB_ID);
    assert_eq!(manifest_value["generation"], 1);
    let receipt_value: Value = serde_json::from_slice(&receipt_before).unwrap();
    let expected_backend = if cfg!(feature = "cozo") {
        "cozo-sqlite-v1"
    } else {
        "mneme-json-snapshot-v1"
    };
    assert_eq!(
        receipt_value["payload"]["effective_policy"]["backend_format"],
        expected_backend
    );

    let mut tampered_plan: Value =
        serde_json::from_slice(&fs::read(&fixture.plan).unwrap()).expect("parse approved plan");
    tampered_plan["target"]["db_id"] = json!(SECOND_OPERATION_ID);
    let tampered_plan_path = bootstrap.join("tampered-retry-plan.json");
    fs::write(
        &tampered_plan_path,
        serde_json::to_vec_pretty(&tampered_plan).unwrap(),
    )
    .unwrap();
    let tampered_invocation =
        failed(&mut fixture.bootstrap_command(OPERATION_ID, &tampered_plan_path));
    assert_stderr_contains(&tampered_invocation, "approval");

    let mut wrong_root = fixture.mnemed();
    wrong_root
        .arg("--json")
        .arg("bootstrap-create")
        .arg("--root")
        .arg(root.parent().expect("fixture root has parent"))
        .arg("--plan")
        .arg(&fixture.plan)
        .arg("--approval")
        .arg(&fixture.approval)
        .arg("--operation-id")
        .arg(OPERATION_ID);
    assert_stderr_contains(&failed(&mut wrong_root), "cwd exactly");

    let second = failed(&mut fixture.bootstrap_command(SECOND_OPERATION_ID, &fixture.plan));
    assert!(
        stderr(&second).contains("not absent") || stderr(&second).contains("managed"),
        "unexpected second-operation refusal: {}",
        stderr(&second)
    );

    for mode in ["brownfield_proposal", "managed_refresh_proposal"] {
        let mut proposal_plan: Value =
            serde_json::from_slice(&fs::read(&fixture.plan).unwrap()).expect("parse approved plan");
        proposal_plan["mode"] = json!(mode);
        let proposal_path = bootstrap.join(format!("{mode}-plan.json"));
        fs::write(
            &proposal_path,
            serde_json::to_vec_pretty(&proposal_plan).unwrap(),
        )
        .unwrap();
        let rejected =
            failed(&mut fixture.bootstrap_command(BROWNFIELD_OPERATION_ID, &proposal_path));
        assert_stderr_contains(&rejected, "proposal-only");
    }

    // Idempotency observes the authenticated completed operation, not the
    // repository's mutable HEAD. Repo drift still blocks every new/recovery
    // activation path because only the exact active selector takes this path.
    fs::write(root.join("AFTER_BOOTSTRAP.md"), "later repository work\n").unwrap();
    git(root, &["add", "AFTER_BOOTSTRAP.md"]);
    git(
        root,
        &["commit", "-q", "--no-gpg-sign", "-m", "advance source cut"],
    );
    let replay_after_repo_advance =
        checked(&mut fixture.bootstrap_command(OPERATION_ID, &fixture.plan));
    assert_eq!(
        json_stdout(&replay_after_repo_advance)["status"],
        "activated"
    );
}

fn build_fixture(model_cache: Option<PathBuf>) -> Fixture {
    let scratch = Scratch::new();
    let root = &scratch.0;
    let readme = b"# Fixture\n\nThis project exists to exercise native bootstrap creation.\n";
    fs::write(root.join(".gitignore"), ".mneme/\n").unwrap();
    fs::write(root.join("README.md"), readme).unwrap();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Bootstrap Integration Test"]);
    git(root, &["config", "user.email", "bootstrap@example.invalid"]);
    git(root, &["add", "-A"]);
    let mut commit = git_command(root, &["commit", "-q", "--no-gpg-sign", "-m", "fixture"]);
    commit
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z");
    checked(&mut commit);

    let workspace = workspace_root();
    let scripts = workspace.join(".agents/skills/mneme-bootstrap/scripts");
    let bootstrap = root.join(".mneme/bootstrap");
    fs::create_dir_all(&bootstrap).unwrap();
    let inventory_path = bootstrap.join("inventory.json");
    let mut inventory = python_command();
    inventory
        .arg(scripts.join("inventory_repo.py"))
        .arg("--root")
        .arg(root)
        .arg("--manifest")
        .arg(bootstrap.join("manifest.json"))
        .arg("--output")
        .arg(&inventory_path)
        .current_dir(root);
    checked(&mut inventory);
    let inventory_value: Value =
        serde_json::from_slice(&fs::read(&inventory_path).unwrap()).unwrap();
    let files = inventory_value["files"]
        .as_array()
        .expect("inventory files");
    assert!(files.iter().any(|item| item["path"] == "README.md"));
    let mut source_decisions = Map::new();
    for item in files {
        let path = item["path"].as_str().expect("inventory path");
        source_decisions.insert(
            path.to_owned(),
            json!(if path == "README.md" {
                "evidence"
            } else {
                "deferred"
            }),
        );
    }
    let blob = stdout_trimmed(&git(root, &["rev-parse", "HEAD:README.md"]));
    let draft = json!({
        "mode": "greenfield",
        "target": {"db_id": DB_ID, "expected_empty": true},
        "repo": {
            "head": inventory_value["repo"]["head"],
            "tree": inventory_value["repo"]["tree"],
            "object_format": inventory_value["repo"]["object_format"],
            "dirty_digest": null
        },
        "base": {
            "manifest_path": ".mneme/bootstrap/manifest.json",
            "manifest_generation": 0,
            "manifest_hash": null
        },
        "inventory": {
            "path": ".mneme/bootstrap/inventory.json",
            "sha256": inventory_value["inventory_hash"]
        },
        "limits": {
            "nodes": 20,
            "edges": 40,
            "bytes_per_body": 16_384,
            "max_out_degree": 8,
            "max_in_degree": 12
        },
        "source_decisions": source_decisions,
        "nodes": [{
            "key": "project:overview",
            "summary": "Native bootstrap fixture",
            "claim": "This project exercises native bootstrap creation.",
            "tags": ["core", "project", "repo-sync-v1"],
            "status": "active",
            "stability": 0.9,
            "confidence": 0.95,
            "sources": [{
                "path": "README.md",
                "blob": blob,
                "sha256": hex_digest(Sha256::digest(readme).as_slice()),
                "span": "1:3"
            }],
            "action": "ingest"
        }],
        "edges": [],
        "brownfield_dispositions": [],
        "adversarial_findings": [{
            "kind": "other",
            "status": "resolved",
            "detail": "Adversarial review completed.",
            "disposition": "No blocking finding was identified.",
            "sources": []
        }]
    });
    let draft_path = bootstrap.join("draft.json");
    fs::write(&draft_path, serde_json::to_vec_pretty(&draft).unwrap()).unwrap();
    let plan = bootstrap.join("plan.json");
    let mut build_plan = python_command();
    build_plan
        .arg(scripts.join("build_plan.py"))
        .arg("--root")
        .arg(root)
        .arg("--output")
        .arg(&plan)
        .arg(&draft_path)
        .current_dir(root);
    checked(&mut build_plan);
    let plan_value: Value = serde_json::from_slice(&fs::read(&plan).unwrap()).unwrap();

    const REVIEW_HASH: &str = r#"import json,sys
sys.path.append(sys.argv[1])
from review_contract import rendered_sha256
with open(sys.argv[2], 'rb') as source:
    plan=json.load(source)
print(rendered_sha256(plan))
"#;
    let mut review_hash = python_command();
    review_hash
        .args(["-I", "-B", "-c", REVIEW_HASH])
        .arg(&scripts)
        .arg(&plan)
        .current_dir(root);
    let rendered_review_sha256 = stdout_trimmed(&checked(&mut review_hash));
    let approval = json!({
        "schema_version": 1,
        "namespace": "repo-sync-v1",
        "plan_hash": plan_value["plan_hash"],
        "rendered_review_sha256": rendered_review_sha256,
        "reviewer_id": "bootstrap-integration@example.invalid",
        "decision": "approved"
    });
    let approval_path = bootstrap.join("approval.json");
    fs::write(
        &approval_path,
        serde_json::to_vec_pretty(&approval).unwrap(),
    )
    .unwrap();

    Fixture {
        scratch,
        plan,
        approval: approval_path,
        model_cache,
    }
}

fn reapprove_plan_with_summary(
    fixture: &Fixture,
    label: &str,
    summary: &str,
) -> (PathBuf, PathBuf) {
    let scripts = workspace_root().join(".agents/skills/mneme-bootstrap/scripts");
    let bootstrap = fixture.root().join(".mneme/bootstrap");
    let plan_path = bootstrap.join(format!("invalid-{label}-plan.json"));
    let approval_path = bootstrap.join(format!("invalid-{label}-approval.json"));
    let mut plan: Value =
        serde_json::from_slice(&fs::read(&fixture.plan).unwrap()).expect("parse fixture plan");
    plan["nodes"][0]["summary"] = json!(summary);
    fs::write(&plan_path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();

    const REHASH_PLAN: &str = r#"import json,sys
sys.path.append(sys.argv[1])
from plan_contract import plan_hash
path=sys.argv[2]
with open(path, 'rb') as source:
    plan=json.load(source)
plan['plan_hash']=plan_hash(plan)
with open(path, 'w', encoding='utf-8') as output:
    json.dump(plan,output,ensure_ascii=True,allow_nan=False,sort_keys=True,indent=2)
    output.write('\n')
"#;
    let mut rehash = python_command();
    rehash
        .args(["-I", "-B", "-c", REHASH_PLAN])
        .arg(&scripts)
        .arg(&plan_path)
        .current_dir(fixture.root());
    checked(&mut rehash);

    let plan: Value = serde_json::from_slice(&fs::read(&plan_path).unwrap()).unwrap();
    const REVIEW_HASH: &str = r#"import json,sys
sys.path.append(sys.argv[1])
from review_contract import rendered_sha256
with open(sys.argv[2], 'rb') as source:
    plan=json.load(source)
print(rendered_sha256(plan))
"#;
    let mut review_hash = python_command();
    review_hash
        .args(["-I", "-B", "-c", REVIEW_HASH])
        .arg(&scripts)
        .arg(&plan_path)
        .current_dir(fixture.root());
    let rendered_review_sha256 = stdout_trimmed(&checked(&mut review_hash));
    let approval = json!({
        "schema_version": 1,
        "namespace": "repo-sync-v1",
        "plan_hash": plan["plan_hash"],
        "rendered_review_sha256": rendered_review_sha256,
        "reviewer_id": "bootstrap-integration@example.invalid",
        "decision": "approved"
    });
    fs::write(
        &approval_path,
        serde_json::to_vec_pretty(&approval).unwrap(),
    )
    .unwrap();
    (plan_path, approval_path)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonical workspace root")
}

fn populated_model_cache() -> Option<PathBuf> {
    let explicit = std::env::var_os("MNEME_TEST_FASTEMBED_CACHE_DIR")
        .or_else(|| std::env::var_os("FASTEMBED_CACHE_DIR"))
        .map(PathBuf::from);
    let candidates: Vec<PathBuf> = explicit
        .into_iter()
        .chain(std::iter::once(workspace_root().join(".fastembed_cache")))
        .collect();
    candidates.into_iter().find(|cache| {
        let repo = cache.join(MODEL_REPO);
        let Ok(revision) = fs::read_to_string(repo.join("refs/main")) else {
            return false;
        };
        repo.join("snapshots")
            .join(revision.trim())
            .join("onnx/model.onnx")
            .is_file()
    })
}

fn git(root: &Path, args: &[&str]) -> Output {
    checked(&mut git_command(root, args))
}

fn git_command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(root)
        .args(args);
    hermetic_environment(&mut command);
    command
}

fn python_command() -> Command {
    let mut command = Command::new("python3");
    hermetic_environment(&mut command);
    command
}

fn hermetic_environment(command: &mut Command) {
    for name in [
        "MNEME_DB",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_INDEX_FILE",
        "GIT_GRAFT_FILE",
        "GIT_REPLACE_REF_BASE",
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
    ] {
        command.env_remove(name);
    }
    command
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "0")
        .env("HF_HUB_OFFLINE", "1")
        .env("HF_HUB_DISABLE_TELEMETRY", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1");
}

fn checked(command: &mut Command) -> Output {
    let invocation = format!("{command:?}");
    let output = command.output().expect("spawn integration-test subprocess");
    assert!(
        output.status.success(),
        "command failed: {invocation}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn failed(command: &mut Command) -> Output {
    let invocation = format!("{command:?}");
    let output = command.output().expect("spawn integration-test subprocess");
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded: {invocation}\nstdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    output
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON output ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn stdout_trimmed(output: &Output) -> String {
    String::from_utf8(output.stdout.clone())
        .expect("UTF-8 subprocess output")
        .trim()
        .to_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_stderr_contains(output: &Output, expected: &str) {
    assert!(
        stderr(output).contains(expected),
        "stderr did not contain {expected:?}: {}",
        stderr(output)
    );
}

#[cfg(not(any(feature = "cozo", feature = "fastembed")))]
#[derive(Deserialize, Serialize)]
struct GoldenEffectivePolicy {
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

#[cfg(not(any(feature = "cozo", feature = "fastembed")))]
#[derive(Deserialize, Serialize)]
struct GoldenReceiptEdge {
    from_node_id: String,
    to_node_id: String,
}

#[cfg(not(any(feature = "cozo", feature = "fastembed")))]
#[derive(Deserialize, Serialize)]
struct GoldenReceiptPayload {
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
    effective_policy: GoldenEffectivePolicy,
    policy_fingerprint: String,
    projection_digest: String,
    node_ids: BTreeMap<String, String>,
    edge_ids: BTreeMap<String, GoldenReceiptEdge>,
    manifest_path: String,
    manifest_hash: String,
    activation_mode: String,
    activation_target: String,
    publication_state: String,
    key_id: String,
}

#[cfg(not(any(feature = "cozo", feature = "fastembed")))]
#[derive(Deserialize, Serialize)]
struct GoldenReceipt {
    payload: GoldenReceiptPayload,
    mac_hmac_sha256: String,
}

#[cfg(not(any(feature = "cozo", feature = "fastembed")))]
const NATIVE_RECEIPT_MAC: &str = "64acbe0b3be84c13933637808a906eae87eee341b9e8c69eb2195f1548570ac3";

#[cfg(not(any(feature = "cozo", feature = "fastembed")))]
const NATIVE_RECEIPT_BYTES: &[u8] = br#"{
  "payload": {
    "schema_version": 1,
    "namespace": "repo-sync-v1-native-bootstrap-receipt",
    "plan_hash": "9dc8864e294599c3bd69a9d592a653e95fadc87da1234a8ba35b5870a927d53b",
    "approval_hash": "23881a6a8a96f2fcfa8ca8c4313c14b65e2eb4517443bb61dc14501d4a37eaf8",
    "reviewer_id": "bootstrap-integration@example.invalid",
    "db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "storage_incarnation": "e72005e396bf8279f977bee27a50984acbee04d1df2974ed369a39a1fcdd6cf0",
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
      "source_timestamp_ms": 946684800000,
      "body_store": "generation-local-fs-relative-v1",
      "backend_format": "mneme-json-snapshot-v1",
      "embedding_fingerprint": {
        "dimension": 768,
        "embedding_id": "mneme:hashing-fnv1a64-token-count-lower-alnum-v1",
        "format_version": 1,
        "normalization": "l2-f32-v1",
        "query_mode": "symmetric-document-v1"
      }
    },
    "policy_fingerprint": "ecfd541b31293b4b6b2c31192a244f81a4e80346ce346229c402892fedbcc467",
    "projection_digest": "5654835e4116fe66ab50a30a816ead74af5451140c05f706e75719b6b4811a09",
    "node_ids": {
      "project:overview": "3CX5K2RH7GC1NZHQD0Y5QS4N3N"
    },
    "edge_ids": {},
    "manifest_path": "bootstrap/manifest.json",
    "manifest_hash": "26860641ea55f9ffb2bcd7d5a577cef037adbdbb82a92940c3eba8b893f72ac9",
    "activation_mode": "atomic-no-clobber-relative-symlink-v1",
    "activation_target": "generations/01ARZ3NDEKTSV4RRFFQ69G5FAW",
    "publication_state": "activation-bound",
    "key_id": "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0"
  },
  "mac_hmac_sha256": "64acbe0b3be84c13933637808a906eae87eee341b9e8c69eb2195f1548570ac3"
}"#;

#[cfg(not(any(feature = "cozo", feature = "fastembed")))]
fn assert_native_receipt_byte_identity(receipt_bytes: &[u8]) {
    assert_eq!(receipt_bytes, NATIVE_RECEIPT_BYTES);
    assert!(
        !receipt_bytes.ends_with(b"\n"),
        "native receipt must not gain a trailing line feed"
    );

    let receipt: GoldenReceipt =
        serde_json::from_slice(receipt_bytes).expect("parse golden native receipt");
    assert_eq!(receipt.mac_hmac_sha256, NATIVE_RECEIPT_MAC);
    assert!(
        receipt
            .mac_hmac_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "native receipt HMAC must remain lowercase hexadecimal"
    );
    assert_eq!(
        hmac_hex(
            &FIXED_NATIVE_KEY,
            &serde_json::to_vec(&receipt.payload).unwrap()
        ),
        NATIVE_RECEIPT_MAC,
        "native receipt MAC input must remain compact JSON"
    );
    assert_ne!(
        hmac_hex(
            &FIXED_NATIVE_KEY,
            &serde_json::to_vec_pretty(&receipt.payload).unwrap()
        ),
        NATIVE_RECEIPT_MAC,
        "pretty payload JSON must not be accepted as the native receipt MAC input"
    );
    assert_eq!(
        serde_json::to_vec_pretty(&receipt).unwrap(),
        receipt_bytes,
        "native receipt bytes must remain serde pretty JSON"
    );
}

#[cfg(not(any(feature = "cozo", feature = "fastembed")))]
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
    hex_digest(outer.finalize().as_slice())
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        value.push(HEX[(byte >> 4) as usize] as char);
        value.push(HEX[(byte & 0x0f) as usize] as char);
    }
    value
}
