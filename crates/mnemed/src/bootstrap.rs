//! Native, fail-closed creation of one approved repo-sync greenfield generation.
//!
//! The ordinary CLI mutation surface is intentionally not reused here. A plan is
//! revalidated, materialized into a private directory with deterministic physical
//! identities and no automatic topology writers, verified as a complete export,
//! and only then published and activated with no-clobber filesystem operations.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Args;
use mneme_core::ports::EmbeddingMetadataStore;
use mneme_core::{
    BodyOwnership, BodyRef, BoundedTagSet, Edge, EdgeKind, Node, NodeId, NodeStatus, OriginCommit,
    Provenance,
};
#[cfg(feature = "cozo")]
use mneme_cozo::CozoStore;
use mneme_cozo::{MemStore, StoreExport};
use mneme_embed::DEFAULT_DIM;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ulid::Ulid;

use crate::AnyErr;
use crate::native_artifact::{
    ExclusiveLock, KEY_BYTES, canonical_git_invocation_root, constant_time_eq, create_private_dir,
    ensure_private_dir, hash_serialized, hex_digest, hmac_hex, load_existing_key,
    load_or_create_key, open_dir_nofollow, open_read_nofollow, read_bounded_regular,
    rename_no_replace, sha256_bytes, sha256_join, sync_dir, verify_path_still_names_open_file,
    verify_path_still_names_open_file_or_dir, write_private_file,
};

const PLAN_MAX_BYTES: usize = 2 * 1024 * 1024;
const APPROVAL_MAX_BYTES: usize = 16 * 1024;
const RECEIPT_MAX_BYTES: usize = 256 * 1024;
const CHILD_OUTPUT_MAX_BYTES: usize = 64 * 1024;
const VALIDATOR_TIMEOUT: Duration = Duration::from_secs(300);
const NAMESPACE: &str = "repo-sync-v1";
const POLICY_VERSION: &str = "repo-sync-v1-default";
const REPO_SYNC_MAX_SUMMARY_BYTES: usize = 1_024;
const RECEIPT_NAMESPACE: &str = "repo-sync-v1-native-bootstrap-receipt";
const TRUSTED_CONTRACT_MODULES: &[(&str, &[u8])] = &[
    (
        "validate_plan.py",
        include_bytes!("../../../.agents/skills/mneme-bootstrap/scripts/validate_plan.py"),
    ),
    (
        "inventory_repo.py",
        include_bytes!("../../../.agents/skills/mneme-bootstrap/scripts/inventory_repo.py"),
    ),
    (
        "manifest_contract.py",
        include_bytes!("../../../.agents/skills/mneme-bootstrap/scripts/manifest_contract.py"),
    ),
    (
        "plan_contract.py",
        include_bytes!("../../../.agents/skills/mneme-bootstrap/scripts/plan_contract.py"),
    ),
    (
        "review_contract.py",
        include_bytes!("../../../.agents/skills/mneme-bootstrap/scripts/review_contract.py"),
    ),
    (
        "secret_scan.py",
        include_bytes!("../../../.agents/skills/mneme-bootstrap/scripts/secret_scan.py"),
    ),
];

#[derive(Args, Debug)]
pub struct BootstrapCreateArgs {
    /// Repository root. The process cwd must be this exact directory.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Builder-owned greenfield plan below ROOT/.mneme/bootstrap.
    #[arg(long)]
    plan: PathBuf,
    /// Exact human approval artifact below ROOT/.mneme/bootstrap.
    #[arg(long)]
    approval: PathBuf,
    /// Stable idempotency key. Reusing it with different approved bytes fails.
    #[arg(long = "operation-id")]
    operation_id: String,
    /// Validate the complete source cut, approval, bounds, and target precondition
    /// without creating a key, generation, database, body, receipt, or symlink.
    #[arg(long)]
    dry_run: bool,
}

/// Read-only classification of a repository's bootstrap/store layout.
///
/// This command deliberately does not use the normal store resolver or open
/// path: both are allowed to return a creatable conventional pathname, and a
/// normal open may install schema, metadata, projections, and an embedder. The
/// inspector only reads bounded filesystem artifacts and, when one already
/// exists, briefly probes the adjacent lease inode without creating it.
#[derive(Args, Debug)]
pub struct BootstrapInspectArgs {
    /// Repository root to inspect. No directory or sidecar is created.
    #[arg(long, default_value = ".")]
    root: PathBuf,
}

const INSPECTION_NAMESPACE: &str = "repo-sync-v1-bootstrap-inspection";
const SNAPSHOT_INSPECTION_MAX_BYTES: usize = 2 * 1024 * 1024;
const GENERATION_INSPECTION_MAX_ENTRIES: usize = 1024;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum InspectionState {
    GreenfieldAbsent,
    ActiveManaged,
    ConventionalUnmanaged,
    InterruptedActivation,
    BlockedMalformed,
    BlockedOrphan,
    BlockedSymlink,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum LeaseObservation {
    NotApplicable,
    Absent,
    Free,
    Held,
    Unknown,
}

#[derive(Debug, Serialize)]
struct InspectionBlocker {
    kind: String,
    message: String,
    action: String,
}

#[derive(Debug, Serialize)]
struct BootstrapInspection {
    schema_version: u64,
    namespace: &'static str,
    state: InspectionState,
    root: String,
    layout: Option<String>,
    database: Option<String>,
    database_format: Option<String>,
    manifest: Option<String>,
    existing_db_id: Option<String>,
    existing_generation: Option<u64>,
    existing_operation_id: Option<String>,
    identity_basis: Option<String>,
    suggested_target_db_id: Option<String>,
    suggested_operation_id: Option<String>,
    recommended_mode: Option<String>,
    apply_capability: String,
    lease: LeaseObservation,
    authoritative: bool,
    retry_safe: bool,
    blocker: Option<InspectionBlocker>,
}

#[derive(Clone, Copy, Debug)]
enum InspectionProblemClass {
    Malformed,
    Orphan,
    Symlink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotsPathKind {
    Absent,
    RealDirectory,
    Symlink,
    Other,
}

#[derive(Debug)]
struct InspectionProblem {
    class: InspectionProblemClass,
    kind: &'static str,
    message: String,
    action: String,
}

impl InspectionProblem {
    fn malformed(
        kind: &'static str,
        message: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            class: InspectionProblemClass::Malformed,
            kind,
            message: message.into(),
            action: action.into(),
        }
    }

    fn orphan(kind: &'static str, message: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            class: InspectionProblemClass::Orphan,
            kind,
            message: message.into(),
            action: action.into(),
        }
    }

    fn symlink(label: &str, path: &Path) -> Self {
        Self {
            class: InspectionProblemClass::Symlink,
            kind: "symlink_layout",
            message: format!("{label} {} is a symlink", path.display()),
            action: "replace the redirected bootstrap/store path with an operator-reviewed real in-repository path before retrying inspection".to_string(),
        }
    }
}

#[derive(Debug)]
struct DatabaseInspection {
    path: PathBuf,
    format: &'static str,
    db_id: Option<String>,
}

#[derive(Debug)]
struct ManifestInspection {
    db_id: String,
    generation: u64,
}

#[derive(Debug)]
struct NativeGenerationInspection {
    database: PathBuf,
    database_format: &'static str,
    manifest: PathBuf,
    db_id: String,
    generation: u64,
    operation_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RepoCut {
    head: String,
    tree: String,
    object_format: String,
    dirty_digest: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    db_id: String,
    expected_empty: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Base {
    manifest_path: String,
    manifest_generation: u64,
    manifest_hash: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryBinding {
    path: String,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    nodes: usize,
    edges: usize,
    bytes_per_body: usize,
    max_out_degree: usize,
    max_in_degree: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedSource {
    path: String,
    blob: String,
    sha256: String,
    bytes: usize,
    class: String,
    decision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct Evidence {
    path: String,
    blob: String,
    sha256: String,
    span: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanNode {
    key: String,
    content_hash: String,
    materialization_hash: String,
    summary: String,
    body: String,
    tags: Vec<String>,
    status: String,
    stability: f64,
    confidence: f64,
    sources: Vec<Evidence>,
    action: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanEdge {
    key: String,
    from_key: String,
    to_key: String,
    kind: String,
    weight: f64,
    assertion_hash: String,
    sources: Vec<Evidence>,
    action: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Finding {
    kind: String,
    status: String,
    detail: String,
    disposition: String,
    sources: Vec<Evidence>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Exclusions {
    count: usize,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeltaAction {
    key: String,
    action: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostManifestDelta {
    publish: bool,
    next_generation: Option<u64>,
    node_actions: Vec<DeltaAction>,
    edge_actions: Vec<DeltaAction>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GreenfieldPlan {
    schema_version: u64,
    namespace: String,
    policy_version: String,
    mode: String,
    target: Target,
    repo: RepoCut,
    base: Base,
    inventory: InventoryBinding,
    limits: Limits,
    sources: Vec<SelectedSource>,
    nodes: Vec<PlanNode>,
    edges: Vec<PlanEdge>,
    brownfield_dispositions: Vec<Value>,
    adversarial_findings: Vec<Finding>,
    exclusions: Exclusions,
    post_manifest_delta: PostManifestDelta,
    plan_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    schema_version: u64,
    namespace: String,
    plan_hash: String,
    rendered_review_sha256: String,
    reviewer_id: String,
    decision: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidatorReport {
    valid: bool,
    apply_allowed: bool,
    apply_blocked_reason: String,
    adversarial_review_complete: bool,
    blocking_findings: usize,
    blocking_reasons: Vec<String>,
    edges: usize,
    head: String,
    live_precondition_required: String,
    mode: String,
    native_bootstrap_create_required: bool,
    nodes: usize,
    plan_hash: String,
    sources: usize,
    structurally_valid: bool,
    tree: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct EffectivePolicy {
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ReceiptEdge {
    from_node_id: String,
    to_node_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ReceiptPayload {
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
    effective_policy: EffectivePolicy,
    policy_fingerprint: String,
    projection_digest: String,
    node_ids: BTreeMap<String, String>,
    edge_ids: BTreeMap<String, ReceiptEdge>,
    manifest_path: String,
    manifest_hash: String,
    activation_mode: String,
    activation_target: String,
    publication_state: String,
    key_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NativeReceipt {
    payload: ReceiptPayload,
    mac_hmac_sha256: String,
}

#[derive(Debug, Serialize)]
struct Outcome<'a> {
    status: &'a str,
    dry_run: bool,
    mode: &'a str,
    plan_hash: &'a str,
    approval_hash: &'a str,
    reviewer_id: &'a str,
    db_id: &'a str,
    operation_id: &'a str,
    nodes: usize,
    edges: usize,
    generation: String,
    database: String,
    manifest: String,
    receipt: String,
    limitation: &'a str,
}

struct ValidatedInput {
    root: PathBuf,
    plan: GreenfieldPlan,
    approval: Approval,
    approval_hash: String,
    operation_id: Ulid,
    source_timestamp_ms: u64,
    prepared_nodes: PreparedNodes,
}

/// Canonical node records prepared while invocation sidecars are still the
/// only project state being inspected. Keeping the deterministic key mapping
/// beside the records prevents materialization from reconstructing either one
/// after it has created generation-catalog or key material.
struct PreparedNodes {
    by_key: BTreeMap<String, NodeId>,
    canonical: Vec<Node>,
}

/// Inspect a repository without crossing any mutation-capable store boundary.
pub fn run_inspect(args: BootstrapInspectArgs, json_output: bool) -> Result<(), AnyErr> {
    let inspection = inspect_repository(&args.root);
    if json_output {
        println!("{}", serde_json::to_string_pretty(&inspection)?);
    } else {
        println!(
            "bootstrap state: {:?}; mode: {}; apply: {}",
            inspection.state,
            inspection.recommended_mode.as_deref().unwrap_or("none"),
            inspection.apply_capability
        );
        if let Some(blocker) = &inspection.blocker {
            println!("blocked: {}\naction: {}", blocker.message, blocker.action);
        }
    }
    Ok(())
}

fn inspect_repository(requested_root: &Path) -> BootstrapInspection {
    let display_root = requested_root.display().to_string();
    let root = match fs::canonicalize(requested_root) {
        Ok(root) => root,
        Err(error) => {
            return blocked_inspection(
                display_root,
                InspectionProblem::malformed(
                    "unreadable_root",
                    format!(
                        "cannot resolve repository root {}: {error}",
                        requested_root.display()
                    ),
                    "pass an existing readable Git repository root",
                ),
            );
        }
    };
    let root_text = root.display().to_string();
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return blocked_inspection(
                root_text,
                InspectionProblem::malformed(
                    "root_not_directory",
                    "bootstrap inspection root is not a real directory",
                    "pass a real Git repository directory",
                ),
            );
        }
        Err(error) => {
            return blocked_inspection(
                root_text,
                InspectionProblem::malformed(
                    "unreadable_root",
                    format!("cannot inspect repository root: {error}"),
                    "restore read access to the repository root and retry",
                ),
            );
        }
    }
    if !root.join(".git").exists() {
        return blocked_inspection(
            root_text,
            InspectionProblem::malformed(
                "not_git_repository",
                "bootstrap inspection requires a Git repository root",
                "run from, or pass --root for, the committed repository root",
            ),
        );
    }

    match inspect_store_layout(&root) {
        Ok(report) => report,
        Err(problem) => blocked_inspection(root.display().to_string(), problem),
    }
}

fn inspect_store_layout(root: &Path) -> Result<BootstrapInspection, InspectionProblem> {
    reject_ordinary_project_owner(root)?;
    let mneme = root.join(".mneme");
    let Some(mneme_metadata) = inspection_metadata(&mneme)? else {
        return Ok(greenfield_inspection(
            root,
            LeaseObservation::NotApplicable,
            None,
        ));
    };
    if mneme_metadata.file_type().is_symlink() {
        return Err(InspectionProblem::symlink(
            "project store directory",
            &mneme,
        ));
    }
    if !mneme_metadata.is_dir() {
        return Err(InspectionProblem::malformed(
            "store_not_directory",
            format!("project store path {} is not a directory", mneme.display()),
            "move the conflicting path aside and create/recover a real .mneme directory through the native workflow",
        ));
    }

    let snapshots = mneme.join("snapshots");
    let snapshots_kind = classify_snapshots_path(&snapshots).map_err(|error| {
        InspectionProblem::malformed(
            "snapshot_path_inspection_failed",
            format!(
                "cannot inspect snapshot residue {}: {error}",
                snapshots.display()
            ),
            "restore filesystem access and retry the read-only inspection",
        )
    })?;
    match snapshots_kind {
        SnapshotsPathKind::Symlink => {
            return Err(InspectionProblem::symlink(
                "snapshot residue directory",
                &snapshots,
            ));
        }
        SnapshotsPathKind::Other => {
            return Err(InspectionProblem::malformed(
                "snapshots_not_directory",
                format!(
                    "snapshot residue path {} is not a directory",
                    snapshots.display()
                ),
                "move the conflicting path aside after operator review or restore a real snapshot directory",
            ));
        }
        SnapshotsPathKind::Absent | SnapshotsPathKind::RealDirectory => {}
    }

    let bootstrap_dir = mneme.join("bootstrap");
    if let Some(metadata) = inspection_metadata(&bootstrap_dir)? {
        if metadata.file_type().is_symlink() {
            return Err(InspectionProblem::symlink(
                "bootstrap sidecar directory",
                &bootstrap_dir,
            ));
        }
        if !metadata.is_dir() {
            return Err(InspectionProblem::malformed(
                "bootstrap_not_directory",
                format!(
                    "bootstrap sidecar path {} is not a directory",
                    bootstrap_dir.display()
                ),
                "replace the conflicting path with a real bootstrap sidecar directory",
            ));
        }
    }

    let conventional_db = inspect_optional_database(&mneme.join("memory.db"))?;
    let legacy_db = inspect_optional_database(&mneme.join("memory.json"))?;
    if conventional_db.is_none() {
        reject_orphan_database_sidecars(&mneme.join("memory.db"))?;
    }
    if legacy_db.is_none() {
        reject_orphan_database_sidecars(&mneme.join("memory.json"))?;
    }
    if conventional_db.is_some() && legacy_db.is_some() {
        return Err(InspectionProblem::malformed(
            "ambiguous_conventional_store",
            "both .mneme/memory.db and .mneme/memory.json exist",
            "choose and migrate one store explicitly; do not let bootstrap guess which graph owns the repository",
        ));
    }
    let existing_conventional = conventional_db.or(legacy_db);

    let bodies = mneme.join("memory.bodies");
    let bodies_present = match inspection_metadata(&bodies)? {
        Some(metadata) if metadata.file_type().is_symlink() => {
            return Err(InspectionProblem::symlink("body directory", &bodies));
        }
        Some(metadata) if metadata.is_dir() => true,
        Some(_) => {
            return Err(InspectionProblem::malformed(
                "bodies_not_directory",
                format!("body path {} is not a directory", bodies.display()),
                "restore the body directory from a known-good store or move the malformed path aside after review",
            ));
        }
        None => false,
    };

    let current = mneme.join("current");
    let current_metadata = inspection_metadata(&current)?;
    let generations = mneme.join("generations");
    let generations_metadata = inspection_metadata(&generations)?;

    if let Some(current_metadata) = current_metadata {
        if existing_conventional.is_some() || bodies_present {
            return Err(InspectionProblem::malformed(
                "activated_conventional_ambiguity",
                "an activation selector coexists with a conventional database or body tree",
                "do not open either graph; identify the intended owner and recover the layout explicitly",
            ));
        }
        return inspect_active_layout(
            root,
            &mneme,
            &current,
            current_metadata,
            generations_metadata,
        );
    }

    if let Some(generations_metadata) = generations_metadata {
        return inspect_interrupted_layout(
            root,
            &mneme,
            &generations,
            generations_metadata,
            existing_conventional.as_ref(),
        );
    }

    if bodies_present && existing_conventional.is_none() {
        return Err(InspectionProblem::orphan(
            "orphan_body_tree",
            "a conventional body tree exists without a database",
            "restore its matching database or move the orphan aside after operator review; bootstrap-create must not adopt it",
        ));
    }

    let root_manifest = bootstrap_dir.join("manifest.json");
    let manifest_present = match inspection_metadata(&root_manifest)? {
        Some(metadata) if metadata.file_type().is_symlink() => {
            return Err(InspectionProblem::symlink(
                "managed manifest",
                &root_manifest,
            ));
        }
        Some(metadata) if metadata.is_file() => true,
        Some(_) => {
            return Err(InspectionProblem::malformed(
                "manifest_not_file",
                format!(
                    "managed manifest {} is not a regular file",
                    root_manifest.display()
                ),
                "restore a regular, bounded repo-sync-v1 manifest",
            ));
        }
        None => false,
    };
    if let Some(database) = existing_conventional {
        let lease = inspect_lease(&database.path)?;
        if manifest_present {
            let manifest = inspect_manifest(&root_manifest)?;
            if let Some(db_id) = &database.db_id
                && db_id != &manifest.db_id
            {
                return Err(InspectionProblem::malformed(
                    "manifest_database_identity_mismatch",
                    format!(
                        "managed manifest db_id {} does not match safely inspected database db_id {}",
                        manifest.db_id, db_id
                    ),
                    "restore a manifest/database pair from the same graph incarnation",
                ));
            }
            return Ok(BootstrapInspection {
                schema_version: 1,
                namespace: INSPECTION_NAMESPACE,
                state: InspectionState::ActiveManaged,
                root: root.display().to_string(),
                layout: Some("conventional".to_string()),
                database: Some(database.path.display().to_string()),
                database_format: Some(database.format.to_string()),
                manifest: Some(root_manifest.display().to_string()),
                existing_db_id: Some(manifest.db_id),
                existing_generation: Some(manifest.generation),
                existing_operation_id: None,
                identity_basis: Some("managed_manifest".to_string()),
                suggested_target_db_id: None,
                suggested_operation_id: None,
                recommended_mode: Some("managed_refresh_proposal".to_string()),
                apply_capability: "proposal_only".to_string(),
                lease,
                authoritative: false,
                retry_safe: true,
                blocker: None,
            });
        }
        let identity_basis = database
            .db_id
            .as_ref()
            .map(|_| "database_snapshot".to_string());
        return Ok(BootstrapInspection {
            schema_version: 1,
            namespace: INSPECTION_NAMESPACE,
            state: InspectionState::ConventionalUnmanaged,
            root: root.display().to_string(),
            layout: Some("conventional".to_string()),
            database: Some(database.path.display().to_string()),
            database_format: Some(database.format.to_string()),
            manifest: None,
            existing_db_id: database.db_id,
            existing_generation: None,
            existing_operation_id: None,
            identity_basis,
            suggested_target_db_id: None,
            suggested_operation_id: None,
            recommended_mode: Some("brownfield_proposal".to_string()),
            apply_capability: "proposal_only".to_string(),
            lease,
            authoritative: false,
            retry_safe: true,
            blocker: None,
        });
    }

    if manifest_present {
        return Err(InspectionProblem::orphan(
            "orphan_manifest",
            "a managed manifest exists without a conventional or activated database",
            "restore its exact database or move the orphan aside after review; do not treat this as greenfield",
        ));
    }
    let native_key = bootstrap_dir.join("native-bootstrap.key");
    if let Some(metadata) = inspection_metadata(&native_key)? {
        if metadata.file_type().is_symlink() {
            return Err(InspectionProblem::symlink(
                "native authentication key",
                &native_key,
            ));
        }
        return Err(InspectionProblem::orphan(
            "orphan_native_key",
            "a native bootstrap authentication key exists without a generation catalog",
            "recover the matching generation from backup or move the orphaned native artifacts aside after review",
        ));
    }

    if snapshots_kind == SnapshotsPathKind::RealDirectory {
        return Err(InspectionProblem::orphan(
            "orphan_snapshot_directory",
            "a .mneme/snapshots directory exists without a conventional or managed database",
            "restore the database that owns these snapshots or move the orphan aside after operator review; bootstrap-create must not adopt it",
        ));
    }

    let conventional_target = mneme.join("memory.db");
    let lease = inspect_lease(&conventional_target)?;
    let blocker = (matches!(lease, LeaseObservation::Held)).then(|| InspectionBlocker {
        kind: "lease_held".to_string(),
        message: "the absent conventional target is currently leased by another mneme process"
            .to_string(),
        action: "release or stop that process before native bootstrap-create rechecks absence"
            .to_string(),
    });
    Ok(greenfield_inspection(root, lease, blocker))
}

/// Bootstrap owns its generation layout, not an ordinary configured project's
/// graph. Presence is enough to refuse: an empty, unavailable or partially
/// installed owner is not an absent owner. Never follow its service config or
/// open its database merely to decide whether creating another graph is safe.
fn reject_ordinary_project_owner(root: &Path) -> Result<(), InspectionProblem> {
    let action = "use the existing project owner to seed or inspect its graph; repair incomplete configuration explicitly; bootstrap-create cannot adopt an ordinary project store";
    let boundary = |path: &Path| {
        InspectionProblem::malformed(
            "ordinary_project_owner_boundary",
            format!(
                "ordinary project memory boundary at {}; bootstrap must not create a parallel graph",
                path.display()
            ),
            action,
        )
    };
    let mneme = root.join(".mneme");
    if let Some(metadata) = inspection_metadata(&mneme)? {
        ensure_real_inspection_directory(&mneme, &metadata, "project store directory")?;
        for name in [
            "codex-memory.db",
            "codex-memory.db-wal",
            "codex-memory.db-shm",
            "codex-memory.db-journal",
            "codex-memory.db.mneme.lock",
            "codex-memory.bodies",
        ] {
            let path = mneme.join(name);
            if inspection_metadata(&path)?.is_some() {
                return Err(boundary(&path));
            }
        }
    }
    let mut binding = None;
    for name in ["cli.json", "config.json", "service.json", "profile.json"] {
        let path = mneme.join(name);
        if inspection_metadata(&path)?.is_some() {
            binding = Some(path);
            break;
        }
    }
    let codex = root.join(".codex");
    if let Some(metadata) = inspection_metadata(&codex)? {
        ensure_real_inspection_directory(&codex, &metadata, "project Codex directory")?;
        let configured = crate::workspace_selection::project_codex_configured(&codex)
            .map_err(|_| InspectionProblem::malformed(
                "unreadable_project_binding",
                format!("project binding {} cannot be safely classified", codex.display()),
                "repair the bounded project configuration explicitly before retrying bootstrap inspection; do not treat a broken binding as absence",
            ))?;
        if configured && binding.is_none() {
            binding = Some(codex);
        }
    }
    // No actual binding means no exception to establish. In particular,
    // unrelated Codex settings must not add another native integrity gate.
    let Some(binding) = binding else {
        return Ok(());
    };

    // A native generation may subsequently be served by a configured owner.
    // Only a positively authenticated published graph permits this: an empty
    // catalog or private-only build residue is still creatable, not proof that
    // a binding belongs to a native graph. A codex-memory store above remains
    // a distinct conflicting graph even in this layout.
    let generations = mneme.join("generations");
    let generations_metadata = inspection_metadata(&generations)?;
    let current = mneme.join("current");
    let native = if let Some(current_metadata) = inspection_metadata(&current)? {
        Some(inspect_active_layout(
            root,
            &mneme,
            &current,
            current_metadata,
            generations_metadata,
        )?)
    } else if let Some(generations_metadata) = generations_metadata {
        Some(inspect_interrupted_layout(
            root,
            &mneme,
            &generations,
            generations_metadata,
            None,
        )?)
    } else {
        None
    };
    if native.is_some_and(|report| {
        report.identity_basis.as_deref() == Some("authenticated_native_manifest")
    }) {
        return Ok(());
    }
    Err(boundary(&binding))
}

fn greenfield_inspection(
    root: &Path,
    lease: LeaseObservation,
    blocker: Option<InspectionBlocker>,
) -> BootstrapInspection {
    BootstrapInspection {
        schema_version: 1,
        namespace: INSPECTION_NAMESPACE,
        state: InspectionState::GreenfieldAbsent,
        root: root.display().to_string(),
        layout: Some("absent".to_string()),
        database: None,
        database_format: None,
        manifest: None,
        existing_db_id: None,
        existing_generation: None,
        existing_operation_id: None,
        identity_basis: None,
        suggested_target_db_id: Some(Ulid::new().to_string()),
        suggested_operation_id: Some(Ulid::new().to_string()),
        recommended_mode: Some("greenfield".to_string()),
        apply_capability: if blocker.is_some() {
            "blocked_until_lease_release".to_string()
        } else {
            "native_bootstrap_create_after_exact_review".to_string()
        },
        lease,
        authoritative: false,
        retry_safe: true,
        blocker,
    }
}

fn blocked_inspection(root: String, problem: InspectionProblem) -> BootstrapInspection {
    let state = match problem.class {
        InspectionProblemClass::Malformed => InspectionState::BlockedMalformed,
        InspectionProblemClass::Orphan => InspectionState::BlockedOrphan,
        InspectionProblemClass::Symlink => InspectionState::BlockedSymlink,
    };
    BootstrapInspection {
        schema_version: 1,
        namespace: INSPECTION_NAMESPACE,
        state,
        root,
        layout: None,
        database: None,
        database_format: None,
        manifest: None,
        existing_db_id: None,
        existing_generation: None,
        existing_operation_id: None,
        identity_basis: None,
        suggested_target_db_id: None,
        suggested_operation_id: None,
        recommended_mode: None,
        apply_capability: "none".to_string(),
        lease: LeaseObservation::Unknown,
        authoritative: false,
        retry_safe: true,
        blocker: Some(InspectionBlocker {
            kind: problem.kind.to_string(),
            message: problem.message,
            action: problem.action,
        }),
    }
}

fn inspect_active_layout(
    root: &Path,
    mneme: &Path,
    current: &Path,
    current_metadata: fs::Metadata,
    generations_metadata: Option<fs::Metadata>,
) -> Result<BootstrapInspection, InspectionProblem> {
    if !current_metadata.file_type().is_symlink() {
        return Err(InspectionProblem::malformed(
            "selector_not_symlink",
            format!("activation selector {} is not a symlink", current.display()),
            "restore the exact relative generations/<operation-id> activation symlink",
        ));
    }
    let target = fs::read_link(current).map_err(|error| {
        InspectionProblem::malformed(
            "unreadable_selector",
            format!(
                "cannot read activation selector {}: {error}",
                current.display()
            ),
            "restore the selector from the authenticated native bootstrap receipt",
        )
    })?;
    let operation_id = inspection_activation_operation_id(&target).ok_or_else(|| {
        InspectionProblem::malformed(
            "malformed_selector",
            format!(
                "activation selector {} must target relative generations/<ULID>, got {}",
                current.display(),
                target.display()
            ),
            "restore the exact relative activation target recorded by native bootstrap",
        )
    })?;
    Ulid::from_string(&operation_id).map_err(|error| {
        InspectionProblem::malformed(
            "malformed_selector",
            format!("activation selector generation id is invalid: {error}"),
            "restore a selector naming the original native bootstrap operation ULID",
        )
    })?;

    let generations = mneme.join("generations");
    let Some(generations_metadata) = generations_metadata else {
        return Err(InspectionProblem::orphan(
            "orphan_selector",
            "activation selector exists without a generation catalog",
            "restore the selected generation catalog from backup; do not create a conventional fallback",
        ));
    };
    ensure_real_inspection_directory(&generations, &generations_metadata, "generation catalog")?;
    let entries = inspect_generation_entries(&generations)?;
    let selected = generations.join(&operation_id);
    if !entries.iter().any(|entry| entry == &operation_id) {
        return Err(InspectionProblem::orphan(
            "orphan_selector",
            format!(
                "activation selector names missing generation {}",
                selected.display()
            ),
            "restore that exact authenticated generation or recover the selector and generation together from backup",
        ));
    }
    let unexpected: Vec<_> = entries
        .iter()
        .filter(|entry| {
            entry.as_str() != operation_id
                && private_generation_operation(entry).as_deref() != Some(operation_id.as_str())
        })
        .collect();
    if !unexpected.is_empty() {
        return Err(InspectionProblem::orphan(
            "foreign_generation_artifacts",
            format!(
                "active generation catalog contains {} unselected artifact(s)",
                unexpected.len()
            ),
            "identify the owning operation for every catalog entry before removing or selecting anything",
        ));
    }
    let native = inspect_native_generation(mneme, &selected, &operation_id)?;
    if fs::read_link(current).map_err(|error| {
        InspectionProblem::malformed(
            "selector_changed",
            format!("activation selector changed during inspection: {error}"),
            "retry inspection after the publisher is quiescent",
        )
    })? != target
    {
        return Err(InspectionProblem::malformed(
            "selector_changed",
            "activation selector changed during inspection",
            "retry inspection after the publisher is quiescent",
        ));
    }
    let lease = inspect_lease(&native.database)?;
    Ok(BootstrapInspection {
        schema_version: 1,
        namespace: INSPECTION_NAMESPACE,
        state: InspectionState::ActiveManaged,
        root: root.display().to_string(),
        layout: Some("native_generation".to_string()),
        database: Some(native.database.display().to_string()),
        database_format: Some(native.database_format.to_string()),
        manifest: Some(native.manifest.display().to_string()),
        existing_db_id: Some(native.db_id),
        existing_generation: Some(native.generation),
        existing_operation_id: Some(native.operation_id),
        identity_basis: Some("authenticated_native_manifest".to_string()),
        suggested_target_db_id: None,
        suggested_operation_id: None,
        recommended_mode: Some("managed_refresh_proposal".to_string()),
        apply_capability: "proposal_only".to_string(),
        lease,
        authoritative: false,
        retry_safe: true,
        blocker: None,
    })
}

fn inspect_interrupted_layout(
    root: &Path,
    mneme: &Path,
    generations: &Path,
    generations_metadata: fs::Metadata,
    conventional: Option<&DatabaseInspection>,
) -> Result<BootstrapInspection, InspectionProblem> {
    ensure_real_inspection_directory(generations, &generations_metadata, "generation catalog")?;
    let entries = inspect_generation_entries(generations)?;
    let mut published = Vec::new();
    let mut private_operations = BTreeSet::new();
    for entry in &entries {
        if Ulid::from_string(entry).is_ok() {
            published.push(entry.clone());
        } else if let Some(operation_id) = private_generation_operation(entry) {
            private_operations.insert(operation_id);
        } else {
            return Err(InspectionProblem::orphan(
                "unknown_generation_artifact",
                format!("generation catalog contains unrecognized entry {entry:?}"),
                "identify the artifact's owning operation before removing or selecting it",
            ));
        }
    }
    if published.len() > 1 || private_operations.len() > 1 {
        return Err(InspectionProblem::orphan(
            "ambiguous_interrupted_activation",
            "generation catalog contains artifacts for more than one possible bootstrap operation",
            "recover using the exact reviewed plan/operation pair; do not choose a generation by recency",
        ));
    }
    if let (Some(published), Some(private)) = (published.first(), private_operations.first())
        && published != private
    {
        return Err(InspectionProblem::orphan(
            "foreign_generation_artifacts",
            "published and private generation artifacts belong to different operations",
            "recover each operation's provenance before changing the catalog",
        ));
    }

    let native = if let Some(operation_id) = published.first() {
        Some(inspect_native_generation(
            mneme,
            &generations.join(operation_id),
            operation_id,
        )?)
    } else {
        None
    };
    let operation_id = native
        .as_ref()
        .map(|native| native.operation_id.clone())
        .or_else(|| private_operations.first().cloned());
    let conventional_target = mneme.join("memory.db");
    // Bootstrap recovery deliberately guards the conventional identity until a
    // selector exists, even when a complete generation has already published.
    let lease = inspect_lease(&conventional_target)?;
    let (database, database_format, manifest, db_id, generation) = match native {
        Some(native) => (
            Some(native.database.display().to_string()),
            Some(native.database_format.to_string()),
            Some(native.manifest.display().to_string()),
            Some(native.db_id),
            Some(native.generation),
        ),
        None => (
            conventional.map(|database| database.path.display().to_string()),
            conventional.map(|database| database.format.to_string()),
            None,
            conventional.and_then(|database| database.db_id.clone()),
            None,
        ),
    };
    let lease_suffix = if matches!(lease, LeaseObservation::Held) {
        " The conventional recovery lease is currently held and must be released first."
    } else {
        ""
    };
    let identity_basis = manifest
        .as_ref()
        .map(|_| "authenticated_native_manifest".to_string())
        .or_else(|| {
            conventional
                .and_then(|database| database.db_id.as_ref())
                .map(|_| "database_snapshot".to_string())
        });
    Ok(BootstrapInspection {
        schema_version: 1,
        namespace: INSPECTION_NAMESPACE,
        state: InspectionState::InterruptedActivation,
        root: root.display().to_string(),
        layout: Some("native_generation_unselected".to_string()),
        database,
        database_format,
        manifest,
        existing_db_id: db_id,
        existing_generation: generation,
        existing_operation_id: operation_id,
        identity_basis,
        suggested_target_db_id: None,
        suggested_operation_id: None,
        recommended_mode: Some("greenfield".to_string()),
        apply_capability: "exact_bootstrap_retry_only".to_string(),
        lease,
        authoritative: false,
        retry_safe: true,
        blocker: Some(InspectionBlocker {
            kind: "activation_incomplete".to_string(),
            message: format!(
                "a generation catalog exists without .mneme/current; ordinary open must remain disabled.{lease_suffix}"
            ),
            action: "rerun native bootstrap-create with the exact reviewed plan, approval, and original operation id; do not select or delete a generation manually".to_string(),
        }),
    })
}

fn inspect_native_generation(
    mneme: &Path,
    generation: &Path,
    operation_id: &str,
) -> Result<NativeGenerationInspection, InspectionProblem> {
    let metadata = inspection_metadata(generation)?.ok_or_else(|| {
        InspectionProblem::orphan(
            "missing_generation",
            format!("native generation {} is missing", generation.display()),
            "restore the exact generation from backup or retry the original native bootstrap operation",
        )
    })?;
    ensure_real_inspection_directory(generation, &metadata, "native generation")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(InspectionProblem::malformed(
                "generation_not_private",
                format!(
                    "native generation {} is not private (0700)",
                    generation.display()
                ),
                "restore private generation permissions before retrying native recovery",
            ));
        }
    }

    let database = inspect_optional_database(&generation.join("memory.db"))?.ok_or_else(|| {
        InspectionProblem::orphan(
            "missing_generation_database",
            "native generation is missing memory.db",
            "restore the exact generation or rerun the original bootstrap operation for recovery",
        )
    })?;
    let bodies = generation.join("memory.bodies");
    let bodies_metadata = inspection_metadata(&bodies)?.ok_or_else(|| {
        InspectionProblem::orphan(
            "missing_generation_bodies",
            "native generation is missing memory.bodies",
            "restore the exact generation body tree before activation/recovery",
        )
    })?;
    ensure_real_inspection_directory(&bodies, &bodies_metadata, "native body directory")?;

    let generation_bootstrap = generation.join("bootstrap");
    let generation_bootstrap_metadata =
        inspection_metadata(&generation_bootstrap)?.ok_or_else(|| {
            InspectionProblem::orphan(
                "missing_generation_bootstrap",
                "native generation is missing its bootstrap authority directory",
                "restore the complete authenticated generation before activation/recovery",
            )
        })?;
    ensure_real_inspection_directory(
        &generation_bootstrap,
        &generation_bootstrap_metadata,
        "generation bootstrap directory",
    )?;

    let manifest_path = generation_bootstrap.join("manifest.json");
    let receipt_path = generation_bootstrap.join("receipt.json");
    let key_path = mneme.join("bootstrap/native-bootstrap.key");
    for (label, path) in [
        ("native manifest", &manifest_path),
        ("native receipt", &receipt_path),
        ("native authentication key", &key_path),
    ] {
        let metadata = inspection_metadata(path)?.ok_or_else(|| {
            InspectionProblem::orphan(
                "missing_native_artifact",
                format!("{label} {} is missing", path.display()),
                "restore the complete authenticated native generation; do not synthesize missing authority sidecars",
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(InspectionProblem::symlink(label, path));
        }
        if !metadata.is_file() {
            return Err(InspectionProblem::malformed(
                "native_artifact_not_file",
                format!("{label} {} is not a regular file", path.display()),
                "restore the exact regular artifact from the native bootstrap result",
            ));
        }
    }

    let key = load_existing_key(&key_path).map_err(|error| {
        InspectionProblem::malformed(
            "invalid_native_key",
            format!("cannot validate native bootstrap key: {error}"),
            "restore the exact private single-linked native key associated with this generation",
        )
    })?;
    let receipt_raw = read_bounded_regular(&receipt_path, RECEIPT_MAX_BYTES, "native receipt")
        .map_err(|error| {
            InspectionProblem::malformed(
                "invalid_native_receipt",
                format!("cannot read native bootstrap receipt: {error}"),
                "restore the exact authenticated receipt for this generation",
            )
        })?;
    let receipt: NativeReceipt = serde_json::from_slice(&receipt_raw).map_err(|error| {
        InspectionProblem::malformed(
            "invalid_native_receipt",
            format!("native bootstrap receipt is malformed: {error}"),
            "restore the exact authenticated receipt for this generation",
        )
    })?;
    let expected_mac = hmac_hex(
        &key,
        &serde_json::to_vec(&receipt.payload).map_err(|error| {
            InspectionProblem::malformed(
                "invalid_native_receipt",
                format!("cannot canonicalize native receipt payload: {error}"),
                "restore the exact authenticated receipt for this generation",
            )
        })?,
    );
    if !constant_time_eq(receipt.mac_hmac_sha256.as_bytes(), expected_mac.as_bytes()) {
        return Err(InspectionProblem::malformed(
            "native_receipt_authentication_failed",
            "native bootstrap receipt authentication failed",
            "restore the receipt and key as one exact pair; do not trust this generation for activation",
        ));
    }
    if receipt.payload.schema_version != 1
        || receipt.payload.namespace != RECEIPT_NAMESPACE
        || receipt.payload.operation_id != operation_id
        || receipt.payload.activation_target != format!("generations/{operation_id}")
        || receipt.payload.activation_mode != "atomic-no-clobber-relative-symlink-v1"
        || receipt.payload.publication_state != "activation-bound"
        || receipt.payload.manifest_path != "bootstrap/manifest.json"
        || receipt.payload.key_id != sha256_bytes(&key)
        || !receipt.payload.absent_target_precondition
    {
        return Err(InspectionProblem::malformed(
            "native_receipt_binding_mismatch",
            "native bootstrap receipt does not bind the selected operation/layout",
            "recover the selector, key, receipt, and generation from the same native bootstrap operation",
        ));
    }
    let policy_fingerprint =
        hash_serialized(&receipt.payload.effective_policy).map_err(|error| {
            InspectionProblem::malformed(
                "native_receipt_policy_malformed",
                format!("cannot validate native policy fingerprint: {error}"),
                "restore the exact authenticated native receipt",
            )
        })?;
    if receipt.payload.policy_fingerprint != policy_fingerprint {
        return Err(InspectionProblem::malformed(
            "native_receipt_policy_mismatch",
            "native bootstrap receipt policy fingerprint is inconsistent",
            "restore the exact authenticated native receipt",
        ));
    }

    let manifest_hash = canonical_json_hash(&manifest_path).map_err(|error| {
        InspectionProblem::malformed(
            "invalid_native_manifest",
            format!("cannot hash native bootstrap manifest: {error}"),
            "restore the exact native manifest authenticated by the receipt",
        )
    })?;
    if manifest_hash != receipt.payload.manifest_hash {
        return Err(InspectionProblem::malformed(
            "native_manifest_hash_mismatch",
            "native bootstrap manifest hash does not match the authenticated receipt",
            "restore the exact manifest/receipt pair; do not activate or plan against this generation",
        ));
    }
    let manifest = inspect_manifest(&manifest_path)?;
    if manifest.db_id != receipt.payload.db_id
        || manifest.generation != receipt.payload.storage_generation
    {
        return Err(InspectionProblem::malformed(
            "native_identity_mismatch",
            "native manifest identity does not match the authenticated receipt",
            "restore all native generation artifacts from the same bootstrap operation",
        ));
    }
    if let Some(database_db_id) = &database.db_id
        && database_db_id != &manifest.db_id
    {
        return Err(InspectionProblem::malformed(
            "native_database_identity_mismatch",
            "safely inspected database db_id does not match its authenticated manifest",
            "restore the database and native authority artifacts from the same generation",
        ));
    }

    Ok(NativeGenerationInspection {
        database: database.path,
        database_format: database.format,
        manifest: manifest_path,
        db_id: manifest.db_id,
        generation: manifest.generation,
        operation_id: operation_id.to_string(),
    })
}

fn inspect_manifest(path: &Path) -> Result<ManifestInspection, InspectionProblem> {
    let metadata = inspection_metadata(path)?.ok_or_else(|| {
        InspectionProblem::orphan(
            "missing_manifest",
            format!("managed manifest {} is missing", path.display()),
            "restore the exact managed manifest or treat the graph as brownfield only after explicit review",
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(InspectionProblem::symlink("managed manifest", path));
    }
    if !metadata.is_file() {
        return Err(InspectionProblem::malformed(
            "manifest_not_file",
            format!("managed manifest {} is not a regular file", path.display()),
            "restore a regular, bounded repo-sync-v1 manifest",
        ));
    }
    let raw = read_bounded_regular(path, PLAN_MAX_BYTES, "managed manifest").map_err(|error| {
        InspectionProblem::malformed(
            "invalid_manifest",
            format!("cannot read managed manifest: {error}"),
            "restore the exact bounded repo-sync-v1 manifest",
        )
    })?;
    let value: Value = serde_json::from_slice(&raw).map_err(|error| {
        InspectionProblem::malformed(
            "invalid_manifest",
            format!("managed manifest is malformed JSON: {error}"),
            "restore the exact repo-sync-v1 manifest; do not hand-repair authority JSON",
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        InspectionProblem::malformed(
            "invalid_manifest",
            "managed manifest root is not an object",
            "restore the exact repo-sync-v1 manifest",
        )
    })?;
    if object.get("schema_version").and_then(Value::as_u64) != Some(1)
        || object.get("namespace").and_then(Value::as_str) != Some(NAMESPACE)
    {
        return Err(InspectionProblem::malformed(
            "unsupported_manifest",
            "managed manifest is not repo-sync-v1 schema version 1",
            "use a compatible mnemed binary or migrate through an explicitly reviewed native contract",
        ));
    }
    let db_id = object
        .get("db_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            InspectionProblem::malformed(
                "invalid_manifest_identity",
                "managed manifest does not contain a string db_id",
                "restore the exact manifest for this graph incarnation",
            )
        })?
        .to_string();
    Ulid::from_string(&db_id).map_err(|error| {
        InspectionProblem::malformed(
            "invalid_manifest_identity",
            format!("managed manifest db_id is not a ULID: {error}"),
            "restore the exact manifest for this graph incarnation",
        )
    })?;
    let generation = object
        .get("generation")
        .and_then(Value::as_u64)
        .filter(|generation| *generation > 0)
        .ok_or_else(|| {
            InspectionProblem::malformed(
                "invalid_manifest_generation",
                "managed manifest generation is missing or zero",
                "restore a published managed manifest with a positive generation",
            )
        })?;
    Ok(ManifestInspection { db_id, generation })
}

fn inspect_optional_database(path: &Path) -> Result<Option<DatabaseInspection>, InspectionProblem> {
    let Some(metadata) = inspection_metadata(path)? else {
        return Ok(None);
    };
    if metadata.file_type().is_symlink() {
        return Err(InspectionProblem::symlink("database", path));
    }
    if !metadata.is_file() {
        return Err(InspectionProblem::malformed(
            "database_not_file",
            format!("database {} is not a regular file", path.display()),
            "restore a real single-file database or snapshot",
        ));
    }
    inspect_present_database_sidecars(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(InspectionProblem::malformed(
                "multiply_linked_database",
                format!(
                    "database {} has {} hard links",
                    path.display(),
                    metadata.nlink()
                ),
                "copy the database to one distinct inode before using Mneme",
            ));
        }
    }
    let mut file = open_read_nofollow(path).map_err(|error| {
        InspectionProblem::malformed(
            "unreadable_database",
            format!("cannot open database {} read-only: {error}", path.display()),
            "restore read access without opening it through a mutation-capable Mneme frontend",
        )
    })?;
    let mut prefix = [0u8; 64];
    let read = file.read(&mut prefix).map_err(|error| {
        InspectionProblem::malformed(
            "unreadable_database",
            format!("cannot read database {}: {error}", path.display()),
            "restore a readable database from a known-good copy",
        )
    })?;
    verify_path_still_names_open_file(path, &metadata).map_err(|error| {
        InspectionProblem::malformed(
            "database_changed",
            format!("database changed during inspection: {error}"),
            "retry inspection after every database writer is quiescent",
        )
    })?;
    let bytes = &prefix[..read];
    if bytes.starts_with(b"SQLite format 3\0") {
        if path.file_name().and_then(|name| name.to_str()) == Some("memory.json") {
            return Err(InspectionProblem::malformed(
                "legacy_filename_format_mismatch",
                "memory.json contains a SQLite database",
                "rename/migrate the store through the explicit migration workflow; do not let bootstrap infer intent",
            ));
        }
        return Ok(Some(DatabaseInspection {
            path: path.to_path_buf(),
            format: "cozo_sqlite",
            db_id: None,
        }));
    }
    if bytes
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'{')
    {
        let db_id = if metadata.len() <= SNAPSHOT_INSPECTION_MAX_BYTES as u64 {
            let raw = read_bounded_regular(
                path,
                SNAPSHOT_INSPECTION_MAX_BYTES,
                "JSON snapshot database",
            )
            .map_err(|error| {
                InspectionProblem::malformed(
                    "invalid_snapshot",
                    format!("cannot read JSON snapshot: {error}"),
                    "restore a valid bounded snapshot or use the ordinary explicit migration diagnostics",
                )
            })?;
            let value: Value = serde_json::from_slice(&raw).map_err(|error| {
                InspectionProblem::malformed(
                    "invalid_snapshot",
                    format!("JSON snapshot is malformed: {error}"),
                    "restore a valid Mneme snapshot from backup",
                )
            })?;
            let object = value.as_object().ok_or_else(|| {
                InspectionProblem::malformed(
                    "invalid_snapshot",
                    "JSON snapshot root is not an object",
                    "restore a valid Mneme snapshot from backup",
                )
            })?;
            match object.get("db_id") {
                Some(Value::String(db_id)) => {
                    Ulid::from_string(db_id).map_err(|error| {
                        InspectionProblem::malformed(
                            "invalid_snapshot_identity",
                            format!("JSON snapshot db_id is not a ULID: {error}"),
                            "restore the snapshot from the graph incarnation that owns it",
                        )
                    })?;
                    Some(db_id.clone())
                }
                Some(_) => {
                    return Err(InspectionProblem::malformed(
                        "invalid_snapshot_identity",
                        "JSON snapshot db_id is not a string",
                        "restore a valid Mneme snapshot",
                    ));
                }
                None => None,
            }
        } else {
            None
        };
        return Ok(Some(DatabaseInspection {
            path: path.to_path_buf(),
            format: "json_snapshot",
            db_id,
        }));
    }
    Err(InspectionProblem::malformed(
        "unknown_database_format",
        format!(
            "database {} is neither SQLite nor a JSON snapshot",
            path.display()
        ),
        "restore a supported database or snapshot; do not run normal open against these bytes",
    ))
}

fn inspect_present_database_sidecars(database: &Path) -> Result<(), InspectionProblem> {
    for sidecar in database_sidecars(database) {
        let Some(metadata) = inspection_metadata(&sidecar)? else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            return Err(InspectionProblem::symlink("SQLite sidecar", &sidecar));
        }
        if !metadata.is_file() {
            return Err(InspectionProblem::malformed(
                "sqlite_sidecar_not_file",
                format!("SQLite sidecar {} is not a regular file", sidecar.display()),
                "quiesce the store and restore its sidecars as regular files before any open",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return Err(InspectionProblem::malformed(
                    "multiply_linked_sqlite_sidecar",
                    format!(
                        "SQLite sidecar {} has {} hard links",
                        sidecar.display(),
                        metadata.nlink()
                    ),
                    "quiesce the store and restore a single-linked sidecar set",
                ));
            }
        }
    }
    Ok(())
}

fn reject_orphan_database_sidecars(database: &Path) -> Result<(), InspectionProblem> {
    for sidecar in database_sidecars(database) {
        let Some(metadata) = inspection_metadata(&sidecar)? else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            return Err(InspectionProblem::symlink(
                "orphan SQLite sidecar",
                &sidecar,
            ));
        }
        return Err(InspectionProblem::orphan(
            "orphan_sqlite_sidecar",
            format!(
                "SQLite sidecar {} exists without its database",
                sidecar.display()
            ),
            "quiesce every possible owner, then restore the matching database or move the orphaned sidecar set aside after review",
        ));
    }
    Ok(())
}

fn database_sidecars(database: &Path) -> Vec<PathBuf> {
    ["-wal", "-shm", "-journal"]
        .into_iter()
        .map(|suffix| {
            let mut filename = database
                .file_name()
                .expect("conventional/native database has a filename")
                .to_os_string();
            filename.push(suffix);
            database.with_file_name(filename)
        })
        .collect()
}

fn inspection_metadata(path: &Path) -> Result<Option<fs::Metadata>, InspectionProblem> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(InspectionProblem::malformed(
            "metadata_unavailable",
            format!("cannot inspect {}: {error}", path.display()),
            "restore filesystem access and retry the read-only inspection",
        )),
    }
}

fn ensure_real_inspection_directory(
    path: &Path,
    metadata: &fs::Metadata,
    label: &str,
) -> Result<(), InspectionProblem> {
    if metadata.file_type().is_symlink() {
        return Err(InspectionProblem::symlink(label, path));
    }
    if !metadata.is_dir() {
        return Err(InspectionProblem::malformed(
            "catalog_entry_not_directory",
            format!("{label} {} is not a directory", path.display()),
            "restore the exact real directory required by the native layout",
        ));
    }
    Ok(())
}

fn inspect_generation_entries(catalog: &Path) -> Result<Vec<String>, InspectionProblem> {
    let mut entries = Vec::new();
    let directory = fs::read_dir(catalog).map_err(|error| {
        InspectionProblem::malformed(
            "unreadable_generation_catalog",
            format!(
                "cannot read generation catalog {}: {error}",
                catalog.display()
            ),
            "restore read access and retry after publishers are quiescent",
        )
    })?;
    for (index, entry) in directory.enumerate() {
        if index >= GENERATION_INSPECTION_MAX_ENTRIES {
            return Err(InspectionProblem::malformed(
                "generation_catalog_too_large",
                format!(
                    "generation catalog exceeds the {}-entry inspection bound",
                    GENERATION_INSPECTION_MAX_ENTRIES
                ),
                "audit and compact the catalog with an explicit future generation-management operation",
            ));
        }
        let entry = entry.map_err(|error| {
            InspectionProblem::malformed(
                "unreadable_generation_catalog",
                format!("cannot inspect generation entry: {error}"),
                "restore read access and retry",
            )
        })?;
        let name = entry.file_name().into_string().map_err(|_| {
            InspectionProblem::malformed(
                "non_utf8_generation_name",
                "generation catalog contains a non-UTF-8 entry name",
                "identify and remove the foreign artifact only after operator review",
            )
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            InspectionProblem::malformed(
                "unreadable_generation_entry",
                format!("cannot inspect generation entry {name:?}: {error}"),
                "retry after filesystem activity is quiescent",
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(InspectionProblem::symlink(
                "generation catalog entry",
                &entry.path(),
            ));
        }
        if !metadata.is_dir() {
            return Err(InspectionProblem::malformed(
                "generation_entry_not_directory",
                format!("generation catalog entry {name:?} is not a directory"),
                "identify the foreign artifact before changing the catalog",
            ));
        }
        entries.push(name);
    }
    entries.sort();
    Ok(entries)
}

fn inspection_activation_operation_id(target: &Path) -> Option<String> {
    use std::path::Component;
    let mut components = target.components();
    match (components.next(), components.next(), components.next()) {
        (Some(Component::Normal(first)), Some(Component::Normal(second)), None)
            if first == "generations" =>
        {
            second.to_str().map(str::to_owned)
        }
        _ => None,
    }
}

fn private_generation_operation(name: &str) -> Option<String> {
    private_operation_scoped_name(name, ".bootstrap-")
}

fn private_activation_operation(name: &str) -> Option<String> {
    private_operation_scoped_name(name, ".current-")
}

fn private_operation_scoped_name(name: &str, prefix: &str) -> Option<String> {
    let suffix = name.strip_prefix(prefix)?;
    let operation = suffix.get(..26)?;
    let build_id = suffix.get(27..)?;
    if suffix.as_bytes().get(26) != Some(&b'-') {
        return None;
    }
    let operation_id = canonical_non_nil_ulid(operation)?;
    canonical_non_nil_ulid(build_id)?;
    Some(operation_id.to_string())
}

fn canonical_non_nil_ulid(value: &str) -> Option<Ulid> {
    let parsed = Ulid::from_string(value).ok()?;
    (parsed != Ulid::nil() && parsed.to_string() == value).then_some(parsed)
}

fn parse_operation_id(value: &str) -> Result<Ulid, AnyErr> {
    canonical_non_nil_ulid(value)
        .ok_or_else(|| "operation-id must be a non-nil ULID in canonical uppercase encoding".into())
}

fn inspect_lease(database: &Path) -> Result<LeaseObservation, InspectionProblem> {
    let lock = if database.exists() {
        mneme_store_path::store_lock_path(database).map_err(|error| {
            InspectionProblem::malformed(
                "invalid_lease_identity",
                format!("cannot derive store lease identity: {error}"),
                "restore a canonical single-inode database path",
            )
        })?
    } else {
        let filename = database.file_name().ok_or_else(|| {
            InspectionProblem::malformed(
                "invalid_database_path",
                "conventional database path has no filename",
                "pass a valid repository root",
            )
        })?;
        let mut lock_name = filename.to_os_string();
        lock_name.push(".mneme.lock");
        database.with_file_name(lock_name)
    };
    let Some(metadata) = inspection_metadata(&lock)? else {
        return Ok(LeaseObservation::Absent);
    };
    if metadata.file_type().is_symlink() {
        return Err(InspectionProblem::symlink("store lease", &lock));
    }
    if !metadata.is_file() {
        return Err(InspectionProblem::malformed(
            "lease_not_file",
            format!("store lease {} is not a regular file", lock.display()),
            "restore the stable private lease inode before opening this store",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        if metadata.nlink() != 1 || metadata.permissions().mode() & 0o077 != 0 {
            return Err(InspectionProblem::malformed(
                "invalid_lease_inode",
                format!(
                    "store lease {} is not private and single-linked",
                    lock.display()
                ),
                "restore the stable 0600 single-linked lease inode",
            ));
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = options.open(&lock).map_err(|error| {
            InspectionProblem::malformed(
                "unreadable_lease",
                format!(
                    "cannot open existing lease {} read-only: {error}",
                    lock.display()
                ),
                "restore read access without replacing the lease inode",
            )
        })?;
        let opened = file.metadata().map_err(|error| {
            InspectionProblem::malformed(
                "unreadable_lease",
                format!("cannot inspect opened lease: {error}"),
                "retry after filesystem activity is quiescent",
            )
        })?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(InspectionProblem::malformed(
                "lease_changed",
                "store lease inode changed during inspection",
                "retry after every store owner is quiescent",
            ));
        }
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
        if rc == 0 {
            let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return Ok(LeaseObservation::Free);
        }
        let error = std::io::Error::last_os_error();
        let raw_error = error.raw_os_error();
        if raw_error == Some(libc::EWOULDBLOCK) || raw_error == Some(libc::EAGAIN) {
            return Ok(LeaseObservation::Held);
        }
        Err(InspectionProblem::malformed(
            "lease_probe_failed",
            format!("cannot probe existing lease {}: {error}", lock.display()),
            "retry on a filesystem that supports advisory flock, or inspect ownership out of band",
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(LeaseObservation::Unknown)
    }
}

pub async fn run(args: BootstrapCreateArgs, json_output: bool) -> Result<(), AnyErr> {
    // A completed operation remains observable after the repository advances.
    // This fast path authenticates only an already-active, exact operation; it
    // cannot authorize a new generation or recover a not-yet-active one. Those
    // paths still go through the live Git/source-cut validator below.
    if !args.dry_run && retry_exact_active(&args, json_output).await? {
        return Ok(());
    }
    let validated = validate_inputs(&args)?;
    let paths = GenerationPaths::new(&validated.root, validated.operation_id);
    let target_state = inspect_target(&paths)?;

    if target_state == TargetState::OrphanSnapshots {
        return Err(
            "bootstrap target contains an orphan .mneme/snapshots directory; restore its owning database or move the residue aside after operator review"
                .into(),
        );
    }

    if args.dry_run {
        if target_state != TargetState::Absent {
            return Err(format!(
                "bootstrap target is not absent ({target_state:?}); dry-run refuses a create/update ambiguity"
            )
            .into());
        }
        emit_outcome(json_output, &validated, &paths, "validated", true);
        return Ok(());
    }

    let _lock = ExclusiveLock::acquire(&paths.lock, "bootstrap lock", "another bootstrap-create")?;
    // Resolve and acquire the exact same store lease as ordinary CLI/MCP
    // frontends before the final absence check. Before activation this is the
    // conventional `.mneme/memory.db` lease; on an exact active retry it is the
    // immutable generation database lease. Holding it through activation closes
    // the split-brain race where a normal opener could otherwise create the old
    // path during materialization.
    let configured_database = paths.mneme.join("memory.db");
    // A final generation without a selector is an interrupted native
    // activation, not a conventional greenfield path.  Lease that immutable
    // database itself through recovery and activation; otherwise a recovery
    // could verify one inode while an ordinary owner mutates another.
    let leased_database = match inspect_target(&paths)? {
        TargetState::GenerationExists => paths.final_generation.join("memory.db"),
        _ => match fs::symlink_metadata(&paths.current) {
            // The native command is the sole recovery authority for a generation
            // catalog published before `current`. It must lock the conventional
            // identity in that state; ordinary resolvers intentionally reject it.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                configured_database.clone()
            }
            Ok(_) => mneme_store_path::resolve_configured_store_path(&configured_database)?,
            Err(error) => return Err(error.into()),
        },
    };
    let store_lease = Arc::new(mneme_store_path::StoreLease::acquire(&leased_database)?);
    let locked_state = inspect_target(&paths)?;
    if matches!(
        locked_state,
        TargetState::Absent | TargetState::GenerationExists
    ) {
        cleanup_abandoned_generation_temps(&paths)?;
    }
    match inspect_target(&paths)? {
        TargetState::Absent => create_and_activate(&validated, &paths).await?,
        TargetState::GenerationExists => {
            recover_exact_retry(&validated, &paths, &store_lease).await?
        }
        state => {
            return Err(format!(
                "bootstrap target is not absent ({state:?}); stale/brownfield mutation requires the future native managed-apply contract"
            )
            .into());
        }
    }
    emit_outcome(json_output, &validated, &paths, "activated", false);
    Ok(())
}

async fn retry_exact_active(args: &BootstrapCreateArgs, json_output: bool) -> Result<bool, AnyErr> {
    let operation_id = parse_operation_id(&args.operation_id)?;
    let root = canonical_invocation_root(args)?;
    let paths = GenerationPaths::new(&root, operation_id);
    let expected = PathBuf::from("generations").join(operation_id.to_string());
    if read_activation_target(&paths.current).as_deref() != Some(expected.as_path()) {
        return Ok(false);
    }
    if inspect_target(&paths)? != TargetState::GenerationExists {
        return Err("active bootstrap operation changed before retry validation".into());
    }

    // Parse and fully reconstruct every approved canonical node before taking
    // a lock (which creates a lockfile) or collecting abandoned build dirs.
    // The selector is deliberately checked again under both locks below.
    let validated = validate_active_retry_inputs(args, root, operation_id)?;

    // Recheck the selector after both locks. Bootstrap serialization prevents a
    // second publisher, and the resolved generation lease excludes ordinary
    // CLI/MCP mutation while the stored result is authenticated.
    let _lock = ExclusiveLock::acquire(&paths.lock, "bootstrap lock", "another bootstrap-create")?;
    let configured_database = paths.mneme.join("memory.db");
    let leased_database = mneme_store_path::resolve_configured_store_path(&configured_database)?;
    let store_lease = Arc::new(mneme_store_path::StoreLease::acquire(&leased_database)?);
    if read_activation_target(&paths.current).as_deref() != Some(expected.as_path())
        || inspect_target(&paths)? != TargetState::GenerationExists
    {
        return Err("active bootstrap operation changed while acquiring its leases".into());
    }
    cleanup_abandoned_generation_temps(&paths)?;
    if inspect_target(&paths)? != TargetState::GenerationExists {
        return Err("active bootstrap operation changed while cleaning abandoned work".into());
    }

    recover_exact_retry(&validated, &paths, &store_lease).await?;
    emit_outcome(json_output, &validated, &paths, "activated", false);
    Ok(true)
}

fn emit_outcome(
    json_output: bool,
    validated: &ValidatedInput,
    paths: &GenerationPaths,
    status: &'static str,
    dry_run: bool,
) {
    let outcome = Outcome {
        status,
        dry_run,
        mode: "greenfield",
        plan_hash: &validated.plan.plan_hash,
        approval_hash: &validated.approval_hash,
        reviewer_id: &validated.approval.reviewer_id,
        db_id: &validated.plan.target.db_id,
        operation_id: &validated.operation_id.to_string(),
        nodes: validated.plan.nodes.len(),
        edges: validated.plan.edges.len(),
        generation: paths.final_generation.display().to_string(),
        database: paths
            .final_generation
            .join("memory.db")
            .display()
            .to_string(),
        manifest: paths
            .final_generation
            .join("bootstrap/manifest.json")
            .display()
            .to_string(),
        receipt: paths
            .final_generation
            .join("bootstrap/receipt.json")
            .display()
            .to_string(),
        limitation: "greenfield creation only; managed refresh and brownfield apply remain refused",
    };
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome).expect("serialize outcome")
        );
    } else if dry_run {
        println!(
            "validated greenfield plan {} for {} node(s), {} edge(s); no files changed",
            validated.plan.plan_hash,
            validated.plan.nodes.len(),
            validated.plan.edges.len()
        );
    } else {
        println!(
            "activated bootstrap generation {} at {}",
            validated.operation_id,
            paths.final_generation.display()
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetState {
    Absent,
    GenerationExists,
    ActiveDatabaseExists,
    ActivationExists,
    LegacyBodiesExist,
    ForeignGenerationExists,
    OrphanSnapshots,
}

struct GenerationPaths {
    mneme: PathBuf,
    generations: PathBuf,
    final_generation: PathBuf,
    current: PathBuf,
    lock: PathBuf,
    key: PathBuf,
}

impl GenerationPaths {
    fn new(root: &Path, operation_id: Ulid) -> Self {
        let mneme = root.join(".mneme");
        let generations = mneme.join("generations");
        Self {
            final_generation: generations.join(operation_id.to_string()),
            current: mneme.join("current"),
            lock: mneme.join("bootstrap-create.lock"),
            key: mneme.join("bootstrap/native-bootstrap.key"),
            mneme,
            generations,
        }
    }
}

fn inspect_target(paths: &GenerationPaths) -> Result<TargetState, AnyErr> {
    reject_ordinary_project_owner(paths.mneme.parent().expect("project .mneme has a parent"))
        .map_err(|problem| format!("{}; {}", problem.message, problem.action))?;
    let snapshots = paths.mneme.join("snapshots");
    let snapshots_kind = classify_snapshots_path(&snapshots)?;
    match snapshots_kind {
        SnapshotsPathKind::Symlink => {
            return Err(format!(
                "snapshot residue path {} must not be a symlink",
                snapshots.display()
            )
            .into());
        }
        SnapshotsPathKind::Other => {
            return Err(format!(
                "snapshot residue path {} must be a real directory",
                snapshots.display()
            )
            .into());
        }
        SnapshotsPathKind::Absent | SnapshotsPathKind::RealDirectory => {}
    }
    for path in [
        paths.mneme.join("memory.db"),
        paths.mneme.join("memory.json"),
    ] {
        if path_is_present(&path)? {
            return Ok(TargetState::ActiveDatabaseExists);
        }
    }
    if path_is_present(&paths.mneme.join("memory.bodies"))? {
        return Ok(TargetState::LegacyBodiesExist);
    }
    let activation_exact = match fs::symlink_metadata(&paths.current) {
        Ok(_) => {
            let expected = PathBuf::from("generations").join(
                paths
                    .final_generation
                    .file_name()
                    .expect("generation has filename"),
            );
            if read_activation_target(&paths.current).as_deref() != Some(expected.as_path()) {
                return Ok(TargetState::ActivationExists);
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let mut final_exists = false;
    match fs::symlink_metadata(&paths.generations) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                return Err(".mneme/generations must be a real directory".into());
            }
            let generations_guard = open_dir_nofollow(&paths.generations)?;
            let generations_opened = generations_guard.metadata()?;
            if fs::canonicalize(&paths.generations)? != paths.generations {
                return Err(".mneme/generations resolves outside its lexical project path".into());
            }
            let final_name = paths
                .final_generation
                .file_name()
                .ok_or("generation target has no filename")?;
            let final_name_text = final_name
                .to_str()
                .ok_or("generation filename is not UTF-8")?;
            for (index, entry) in fs::read_dir(&paths.generations)?.enumerate() {
                if index >= 1024 {
                    return Err("generation catalog exceeds the native inspection bound".into());
                }
                let entry = entry?;
                let name = entry.file_name();
                let entry_metadata = fs::symlink_metadata(entry.path())?;
                if name == final_name {
                    if !entry_metadata.file_type().is_dir()
                        || entry_metadata.file_type().is_symlink()
                    {
                        return Err("target bootstrap generation is not a real directory".into());
                    }
                    final_exists = true;
                    continue;
                }
                let name = name
                    .to_str()
                    .ok_or("generation catalog contains a non-UTF-8 entry")?;
                match private_generation_operation(name) {
                    Some(operation) if operation == final_name_text => {
                        if entry_metadata.file_type().is_dir()
                            && !entry_metadata.file_type().is_symlink()
                        {
                            continue;
                        }
                        return Err(format!(
                            "private bootstrap work {} is not a real directory",
                            entry.path().display()
                        )
                        .into());
                    }
                    // An entry that merely resembles this operation's private
                    // namespace is never garbage: preserve it and refuse the
                    // ambiguous catalog rather than trusting a prefix match.
                    None if name.strip_prefix(".bootstrap-").is_some() => {
                        return Err(format!(
                            "malformed private bootstrap generation entry {name:?}; operator review is required"
                        )
                        .into());
                    }
                    _ => {}
                }
                return Ok(TargetState::ForeignGenerationExists);
            }
            verify_path_still_names_open_file_or_dir(
                &paths.generations,
                &generations_opened,
                true,
            )?;
            drop(generations_guard);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if activation_exact && !final_exists {
        return Err("activation points at a missing target generation".into());
    }
    if final_exists {
        return Ok(TargetState::GenerationExists);
    }
    if snapshots_kind == SnapshotsPathKind::RealDirectory {
        return Ok(TargetState::OrphanSnapshots);
    }
    Ok(TargetState::Absent)
}

fn classify_snapshots_path(path: &Path) -> std::io::Result<SnapshotsPathKind> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(SnapshotsPathKind::Symlink),
        Ok(metadata) if metadata.is_dir() => Ok(SnapshotsPathKind::RealDirectory),
        Ok(_) => Ok(SnapshotsPathKind::Other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SnapshotsPathKind::Absent),
        Err(error) => Err(error),
    }
}

fn path_is_present(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Remove private build directories left when this exact operation was killed
/// before publication. Every nested database lease is acquired before any
/// deletion, so one blocked candidate leaves the entire set intact. Exact
/// operation parsing prevents one retry from collecting another operation's
/// work or malformed lookalikes. Top-level symlinks are rejected and
/// `remove_dir_all` unlinks, rather than follows, symlinks below the real temp
/// directory.
fn cleanup_abandoned_generation_temps(paths: &GenerationPaths) -> Result<usize, AnyErr> {
    let metadata = match fs::symlink_metadata(&paths.generations) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(".mneme/generations must be a real directory".into());
    }
    let generations_guard = open_dir_nofollow(&paths.generations)?;
    let generations_opened = generations_guard.metadata()?;
    if fs::canonicalize(&paths.generations)? != paths.generations {
        return Err(".mneme/generations resolves outside its lexical project path".into());
    }

    let operation = paths
        .final_generation
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("generation filename is not UTF-8")?;
    let mut stale = Vec::new();
    for (index, entry) in fs::read_dir(&paths.generations)?.enumerate() {
        if index >= 1024 {
            return Err("generation catalog exceeds the native inspection bound".into());
        }
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let parsed = private_generation_operation(&name);
        if parsed.as_deref() != Some(operation) {
            if parsed.is_none() && name.strip_prefix(".bootstrap-").is_some() {
                return Err(format!(
                    "malformed private bootstrap generation entry {name:?}; refusing cleanup"
                )
                .into());
            }
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "abandoned bootstrap work {} is not a real directory",
                path.display()
            )
            .into());
        }
        if fs::canonicalize(&path)?.parent() != Some(paths.generations.as_path()) {
            return Err(format!(
                "abandoned bootstrap work {} escapes the generation catalog",
                path.display()
            )
            .into());
        }
        stale.push(path);
    }
    verify_path_still_names_open_file_or_dir(&paths.generations, &generations_opened, true)?;

    // Do not delete *any* candidate until all of their nested database leases
    // are held. StoreLease also validates the private adjacent lock inode.
    let mut nested_leases = Vec::with_capacity(stale.len());
    for path in &stale {
        nested_leases.push(mneme_store_path::StoreLease::acquire(
            &path.join("memory.db"),
        )?);
    }

    for path in &stale {
        fs::remove_dir_all(path)?;
    }
    if !stale.is_empty() {
        generations_guard.sync_all()?;
    }
    drop(nested_leases);
    drop(generations_guard);
    Ok(stale.len())
}

fn validate_inputs(args: &BootstrapCreateArgs) -> Result<ValidatedInput, AnyErr> {
    let root = canonical_invocation_root(args)?;
    let (plan_raw, approval_raw) = read_invocation_sidecars(args, &root)?;

    let preliminary: Value = serde_json::from_slice(&plan_raw)?;
    let mode = preliminary
        .get("mode")
        .and_then(Value::as_str)
        .ok_or("plan mode is missing")?;
    if mode != "greenfield" {
        return Err(format!(
            "plan mode {mode:?} is proposal-only; bootstrap-create cannot update a stale/brownfield graph without native managed snapshot, epoch, ownership, and CAS apply"
        )
        .into());
    }

    let trusted_contract = TrustedContractRuntime::create()?;
    let report = run_structural_validator(&root, trusted_contract.path(), &plan_raw)?;
    let plan: GreenfieldPlan = serde_json::from_slice(&plan_raw)?;
    validate_report_and_projection_shape(&plan, &report)?;
    let approval: Approval = serde_json::from_slice(&approval_raw)?;
    let approval_hash =
        validate_approval(&root, trusted_contract.path(), &plan_raw, &approval_raw)?;
    validate_approval_shape(&plan, &approval)?;
    let operation_id = parse_operation_id(&args.operation_id)?;
    verify_git_cut(&root, &plan.repo)?;
    let source_timestamp_ms = git_commit_timestamp_ms(&root, &plan.repo.head)?;
    let prepared_nodes = prepare_canonical_nodes(
        &plan.plan_hash,
        &plan.repo.head,
        source_timestamp_ms,
        &plan.nodes,
    )?;

    Ok(ValidatedInput {
        root,
        plan,
        approval,
        approval_hash,
        operation_id,
        source_timestamp_ms,
        prepared_nodes,
    })
}

fn canonical_invocation_root(args: &BootstrapCreateArgs) -> Result<PathBuf, AnyErr> {
    let root = canonical_git_invocation_root(&args.root, "bootstrap-create")?;
    reject_ordinary_project_owner(&root)
        .map_err(|problem| format!("{}; {}", problem.message, problem.action))?;
    let mneme_dir = root.join(".mneme");
    let bootstrap_path = mneme_dir.join("bootstrap");
    for (label, path) in [(".mneme", &mneme_dir), ("bootstrap", &bootstrap_path)] {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(
                format!("{label} must be a real directory below the repository root").into(),
            );
        }
    }
    let bootstrap_dir = fs::canonicalize(&bootstrap_path)?;
    if bootstrap_dir != bootstrap_path {
        return Err("bootstrap sidecar directory resolves outside its lexical project path".into());
    }
    Ok(root)
}

fn read_invocation_sidecars(
    args: &BootstrapCreateArgs,
    root: &Path,
) -> Result<(Vec<u8>, Vec<u8>), AnyErr> {
    let bootstrap_path = root.join(".mneme/bootstrap");
    let bootstrap_dir = fs::canonicalize(&bootstrap_path)?;
    let plan_path = canonical_sidecar(&args.plan, &bootstrap_dir, "plan")?;
    let approval_path = canonical_sidecar(&args.approval, &bootstrap_dir, "approval")?;
    let plan_raw = read_bounded_regular(&plan_path, PLAN_MAX_BYTES, "plan")?;
    let approval_raw = read_bounded_regular(&approval_path, APPROVAL_MAX_BYTES, "approval")?;
    Ok((plan_raw, approval_raw))
}

fn validate_active_retry_inputs(
    args: &BootstrapCreateArgs,
    root: PathBuf,
    operation_id: Ulid,
) -> Result<ValidatedInput, AnyErr> {
    let (plan_raw, approval_raw) = read_invocation_sidecars(args, &root)?;
    let plan: GreenfieldPlan = serde_json::from_slice(&plan_raw)?;
    // This local shape gate is not permission to apply: the exact active
    // selector, locks, authenticated receipt, and generation manifest are all
    // checked by the caller. It only makes parsing/reconstruction total and
    // retains the fixed native projection contract without consulting HEAD.
    validate_projection_shape(&plan)?;
    let trusted_contract = TrustedContractRuntime::create()?;
    let approval: Approval = serde_json::from_slice(&approval_raw)?;
    let approval_hash =
        validate_approval(&root, trusted_contract.path(), &plan_raw, &approval_raw)?;
    validate_approval_shape(&plan, &approval)?;
    let source_timestamp_ms = git_commit_timestamp_ms(&root, &plan.repo.head)?;
    let prepared_nodes = prepare_canonical_nodes(
        &plan.plan_hash,
        &plan.repo.head,
        source_timestamp_ms,
        &plan.nodes,
    )?;

    Ok(ValidatedInput {
        root,
        plan,
        approval,
        approval_hash,
        operation_id,
        source_timestamp_ms,
        prepared_nodes,
    })
}

fn canonical_sidecar(path: &Path, bootstrap_dir: &Path, label: &str) -> Result<PathBuf, AnyErr> {
    let original = fs::symlink_metadata(path)?;
    if original.file_type().is_symlink() {
        return Err(format!("{label} path must not be a symlink").into());
    }
    let path = fs::canonicalize(path)?;
    if !path.starts_with(bootstrap_dir) || path.parent() != Some(bootstrap_dir) {
        return Err(format!(
            "{label} must be a direct regular file below {}",
            bootstrap_dir.display()
        )
        .into());
    }
    Ok(path)
}

fn validate_report_and_projection_shape(
    plan: &GreenfieldPlan,
    report: &ValidatorReport,
) -> Result<(), AnyErr> {
    if !report.valid
        || !report.structurally_valid
        || report.apply_allowed
        || !report.adversarial_review_complete
        || report.blocking_findings != 0
        || !report.blocking_reasons.is_empty()
        || report.apply_blocked_reason.is_empty()
        || report.live_precondition_required.is_empty()
        || !report.native_bootstrap_create_required
        || report.mode != "greenfield"
        || report.plan_hash != plan.plan_hash
        || report.head != plan.repo.head
        || report.tree != plan.repo.tree
        || report.nodes != plan.nodes.len()
        || report.edges != plan.edges.len()
        || report.sources != plan.sources.len()
    {
        return Err(
            "structural validator report does not bind this exact non-mutating greenfield plan"
                .into(),
        );
    }
    validate_projection_shape(plan)
}

fn validate_node_materialization_shape(node: &PlanNode) -> Result<(), AnyErr> {
    BoundedTagSet::try_from_iter(&node.tags)
        .map_err(|error| format!("node {:?} has invalid canonical tags: {error}", node.key))?;
    if node.status != "active" {
        return Err(format!(
            "node {:?} has unsupported bootstrap status {:?}",
            node.key, node.status
        )
        .into());
    }
    Ok(())
}

/// Reconstruct the exact canonical records that will be persisted, without
/// touching the generation catalog, key, body store, embedder, or database.
/// This is intentionally separate from the structural plan validator: an
/// approval can bind only plan syntax, while this gate binds the current Rust
/// domain invariants as well.
fn prepare_canonical_nodes(
    plan_hash: &str,
    origin_commit: &str,
    source_timestamp_ms: u64,
    nodes: &[PlanNode],
) -> Result<PreparedNodes, AnyErr> {
    let origin_commit = OriginCommit::parse(origin_commit)?;
    let timestamp = u128::from(source_timestamp_ms);
    let mut by_key = BTreeMap::new();
    let mut canonical = Vec::with_capacity(nodes.len());

    for node in nodes {
        if node.summary.len() > REPO_SYNC_MAX_SUMMARY_BYTES {
            return Err(format!(
                "node {:?} summary is {} UTF-8 bytes; repo-sync-v1 hard maximum is {REPO_SYNC_MAX_SUMMARY_BYTES}",
                node.key,
                node.summary.len()
            )
            .into());
        }
        validate_node_materialization_shape(node)?;
        let status = match node.status.as_str() {
            "active" => NodeStatus::Active,
            other => return Err(format!("unsupported bootstrap node status {other:?}").into()),
        };
        let id = deterministic_node_id(plan_hash, &node.key);
        if by_key.insert(node.key.clone(), id).is_some()
            || by_key
                .values()
                .filter(|&&candidate| candidate == id)
                .count()
                != 1
        {
            return Err("deterministic node-id collision".into());
        }
        let body_ref = BodyRef::new(format!("fs://{}", id.0))?;
        let materialized = Node::try_new(
            id,
            &node.summary,
            body_ref,
            &node.tags,
            Provenance::derived_empty(),
            node.stability as f32,
            node.confidence as f32,
            status,
            timestamp,
        )
        .map_err(|error| format!("node {:?} is not canonical: {error}", node.key))?
        .with_body_ownership(BodyOwnership::Managed)
        .with_origin_commit(Some(origin_commit));
        materialized
            .validate()
            .map_err(|error| format!("node {:?} is not canonical: {error}", node.key))?;
        canonical.push(materialized);
    }

    Ok(PreparedNodes { by_key, canonical })
}

fn validate_edge_materialization_shape(edge: &PlanEdge) -> Result<(), AnyErr> {
    if !matches!(
        edge.kind.as_str(),
        "associative" | "supersedes" | "derived-from"
    ) {
        return Err(format!(
            "edge {:?} has unsupported bootstrap kind {:?}",
            edge.key, edge.kind
        )
        .into());
    }
    if !edge.weight.is_finite() || !(0.0..=1.0).contains(&edge.weight) {
        return Err(format!("edge {:?} weight must be finite and in [0, 1]", edge.key).into());
    }
    Ok(())
}

fn validate_projection_shape(plan: &GreenfieldPlan) -> Result<(), AnyErr> {
    if plan.schema_version != 1
        || plan.namespace != NAMESPACE
        || plan.policy_version != POLICY_VERSION
        || plan.mode != "greenfield"
        || !plan.target.expected_empty
        || plan.base.manifest_generation != 0
        || plan.base.manifest_hash.is_some()
        || !plan.post_manifest_delta.publish
        || plan.post_manifest_delta.next_generation != Some(1)
        || !plan.brownfield_dispositions.is_empty()
        || plan.base.manifest_path != ".mneme/bootstrap/manifest.json"
        || !plan.inventory.path.starts_with(".mneme/bootstrap/")
        || !is_hex_digest(&plan.inventory.sha256)
        || !is_hex_digest(&plan.exclusions.sha256)
        || plan.exclusions.count > 100_000
    {
        return Err("greenfield plan preconditions or publication delta are not executable".into());
    }
    if plan.limits.nodes > 20
        || plan.limits.edges > 40
        || plan.limits.bytes_per_body > 16_384
        || plan.limits.max_out_degree > 8
        || plan.limits.max_in_degree > 12
        || plan.nodes.len() > plan.limits.nodes
        || plan.edges.len() > plan.limits.edges
    {
        return Err("plan attempts to raise or exceed repo-sync-v1 hard bounds".into());
    }
    if plan.nodes.iter().any(|node| node.action != "ingest")
        || plan.edges.iter().any(|edge| edge.action != "link")
    {
        return Err("greenfield native creation accepts only ingest and link actions".into());
    }
    let source_paths: Vec<&str> = plan
        .sources
        .iter()
        .map(|source| source.path.as_str())
        .collect();
    if source_paths.windows(2).any(|pair| pair[0] >= pair[1])
        || plan.sources.iter().any(|source| {
            source.path.is_empty()
                || source.blob.is_empty()
                || !is_hex_digest(&source.sha256)
                || source.bytes > 1024 * 1024
                || source.class.is_empty()
                || !matches!(
                    source.decision.as_str(),
                    "inspect" | "evidence" | "deferred"
                )
        })
    {
        return Err("selected source inventory is not bounded and canonical".into());
    }
    let mut keys = BTreeSet::new();
    for node in &plan.nodes {
        if !keys.insert(node.key.as_str()) {
            return Err(format!("duplicate node key {:?}", node.key).into());
        }
        validate_node_materialization_shape(node)?;
        if node.body.len() > plan.limits.bytes_per_body
            || node.tags.len() > 16
            || !node.stability.is_finite()
            || !node.confidence.is_finite()
            || !(0.0..=1.0).contains(&node.stability)
            || !(0.0..=1.0).contains(&node.confidence)
        {
            return Err(
                format!("node {:?} exceeds native materialization bounds", node.key).into(),
            );
        }
    }
    for edge in &plan.edges {
        validate_edge_materialization_shape(edge)?;
        if !keys.contains(edge.from_key.as_str()) || !keys.contains(edge.to_key.as_str()) {
            return Err("edge endpoint is not an ingest in the same plan".into());
        }
    }
    if plan.adversarial_findings.iter().any(|finding| {
        finding.status == "unresolved"
            && matches!(
                finding.kind.as_str(),
                "conflict" | "ambiguity" | "injection" | "ownership" | "sensitive"
            )
    }) {
        return Err("unresolved adversarial finding blocks native creation".into());
    }
    if plan.adversarial_findings.is_empty()
        || plan.adversarial_findings.iter().any(|finding| {
            finding.detail.is_empty()
                || finding.disposition.is_empty()
                || finding.sources.len() > 16
        })
    {
        return Err("adversarial review record is missing or unbounded".into());
    }
    let expected_node_actions: Vec<_> = plan
        .nodes
        .iter()
        .map(|node| (node.key.as_str(), node.action.as_str()))
        .collect();
    let actual_node_actions: Vec<_> = plan
        .post_manifest_delta
        .node_actions
        .iter()
        .map(|action| (action.key.as_str(), action.action.as_str()))
        .collect();
    let expected_edge_actions: Vec<_> = plan
        .edges
        .iter()
        .map(|edge| (edge.key.as_str(), edge.action.as_str()))
        .collect();
    let actual_edge_actions: Vec<_> = plan
        .post_manifest_delta
        .edge_actions
        .iter()
        .map(|action| (action.key.as_str(), action.action.as_str()))
        .collect();
    if actual_node_actions != expected_node_actions || actual_edge_actions != expected_edge_actions
    {
        return Err("post-manifest delta does not exactly cover executable actions".into());
    }
    Ulid::from_string(&plan.target.db_id)
        .map_err(|error| format!("invalid target db_id: {error}"))?;
    Ok(())
}

fn validate_approval_shape(plan: &GreenfieldPlan, approval: &Approval) -> Result<(), AnyErr> {
    if approval.schema_version != 1
        || approval.namespace != NAMESPACE
        || approval.plan_hash != plan.plan_hash
        || approval.decision != "approved"
        || approval.reviewer_id.trim() != approval.reviewer_id
        || approval.reviewer_id.is_empty()
        || approval.reviewer_id.len() > 256
        || approval.reviewer_id.chars().any(char::is_control)
        || approval.rendered_review_sha256.len() != 64
    {
        return Err("approval does not satisfy the exact approved v1 review gate".into());
    }
    Ok(())
}

fn run_structural_validator(
    root: &Path,
    contract_scripts: &Path,
    plan_raw: &[u8],
) -> Result<ValidatorReport, AnyErr> {
    const RUN_PINNED: &str = r#"import runpy,sys
sys.path.append(sys.argv[1])
script=sys.argv[2]
sys.argv=[script,*sys.argv[3:]]
runpy.run_path(script,run_name='__main__')
"#;
    let validator = contract_scripts.join("validate_plan.py");
    let metadata = fs::symlink_metadata(&validator)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "bootstrap validator {} is not a regular file",
            validator.display()
        )
        .into());
    }
    let mut command = Command::new("python3");
    command
        .arg("-I")
        .arg("-B")
        .arg("-c")
        .arg(RUN_PINNED)
        .arg(contract_scripts)
        .arg(&validator)
        .arg("--root")
        .arg(root)
        .arg("--native-greenfield-postcondition")
        .arg("-")
        .current_dir(root)
        .env("PYTHONDONTWRITEBYTECODE", "1");
    sanitize_git_environment(&mut command);
    let output = run_bounded_command(command, plan_raw, VALIDATOR_TIMEOUT)?;
    if !output.status.success() {
        return Err(format!(
            "bootstrap plan structural validation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn validate_approval(
    root: &Path,
    contract_scripts: &Path,
    plan_raw: &[u8],
    approval_raw: &[u8],
) -> Result<String, AnyErr> {
    const CODE: &str = r#"import hashlib,json,sys
sys.path.append(sys.argv[1])
from review_contract import validate_approval
raw=sys.stdin.buffer.read()
plan_raw,approval_raw=raw.split(b'\0',1)
plan=json.loads(plan_raw.decode('utf-8'))
approval=json.loads(approval_raw.decode('utf-8'))
approved=validate_approval(plan,approval,raw_size=len(approval_raw))
canonical=json.dumps(approved,ensure_ascii=True,allow_nan=False,sort_keys=True,separators=(',',':')).encode('utf-8')
print(hashlib.sha256(canonical).hexdigest())
"#;
    let mut input = Vec::with_capacity(plan_raw.len() + approval_raw.len() + 1);
    input.extend_from_slice(plan_raw);
    input.push(0);
    input.extend_from_slice(approval_raw);
    let mut command = Command::new("python3");
    command
        .arg("-I")
        .arg("-B")
        .arg("-c")
        .arg(CODE)
        .arg(contract_scripts)
        .current_dir(root)
        .env("PYTHONDONTWRITEBYTECODE", "1");
    let output = run_bounded_command(command, &input, VALIDATOR_TIMEOUT)?;
    if !output.status.success() {
        return Err(format!(
            "bootstrap approval validation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    parse_hex_line(&output.stdout, "approval hash")
}

struct ChildOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_bounded_command(
    mut command: Command,
    input: &[u8],
    timeout: Duration,
) -> Result<ChildOutput, AnyErr> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().ok_or("child stdin unavailable")?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let stdout = child.stdout.take().ok_or("child stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("child stderr unavailable")?;
    let stdout_reader = std::thread::spawn(move || read_capped_to_eof(stdout));
    let stderr_reader = std::thread::spawn(move || read_capped_to_eof(stderr));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(
                format!("bootstrap validator exceeded {} seconds", timeout.as_secs()).into(),
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    writer.join().map_err(|_| "child stdin writer panicked")??;
    let stdout = stdout_reader
        .join()
        .map_err(|_| "child stdout reader panicked")??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "child stderr reader panicked")??;
    Ok(ChildOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_capped_to_eof(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if captured.len() <= CHILD_OUTPUT_MAX_BYTES {
            let remaining = CHILD_OUTPUT_MAX_BYTES
                .saturating_add(1)
                .saturating_sub(captured.len());
            captured.extend_from_slice(&chunk[..n.min(remaining)]);
        }
    }
    if captured.len() > CHILD_OUTPUT_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "child output exceeded hard bound",
        ));
    }
    Ok(captured)
}

fn parse_hex_line(bytes: &[u8], label: &str) -> Result<String, AnyErr> {
    let value = std::str::from_utf8(bytes)?.trim();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("{label} is not a SHA-256 digest").into());
    }
    Ok(value.to_ascii_lowercase())
}

struct TrustedContractRuntime {
    path: PathBuf,
}

impl TrustedContractRuntime {
    fn create() -> Result<Self, AnyErr> {
        let parent = fs::canonicalize(std::env::temp_dir())?;
        for _ in 0..16 {
            let path = parent.join(format!(".mnemed-bootstrap-contract-{}", Ulid::new()));
            match create_private_dir(&path) {
                Ok(()) => {
                    let runtime = Self { path };
                    for (name, bytes) in TRUSTED_CONTRACT_MODULES {
                        write_private_file(&runtime.path.join(name), bytes)?;
                    }
                    sync_dir(&runtime.path)?;
                    return Ok(runtime);
                }
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err("could not reserve a private trusted contract runtime".into())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TrustedContractRuntime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn is_hex_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn verify_git_cut(root: &Path, cut: &RepoCut) -> Result<(), AnyErr> {
    let head = git_value(root, "HEAD^{commit}")?;
    let tree = git_value(root, "HEAD^{tree}")?;
    if head != cut.head || tree != cut.tree || cut.dirty_digest.is_some() {
        return Err(format!(
            "repository cut moved or is not committed-only: expected {}/{}, observed {head}/{tree}",
            cut.head, cut.tree
        )
        .into());
    }
    Ok(())
}

fn git_value(root: &Path, revision: &str) -> Result<String, AnyErr> {
    let mut command = Command::new("git");
    command
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg(revision);
    sanitize_git_environment(&mut command);
    let output = command.output()?;
    if !output.status.success() || output.stdout.len() > 128 || output.stderr.len() > 4096 {
        return Err(format!(
            "cannot resolve frozen Git cut: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn git_commit_timestamp_ms(root: &Path, commit: &str) -> Result<u64, AnyErr> {
    let mut command = Command::new("git");
    command
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(root)
        .arg("show")
        .arg("-s")
        .arg("--format=%ct")
        .arg(commit);
    sanitize_git_environment(&mut command);
    let output = command.output()?;
    if !output.status.success() || output.stdout.len() > 64 || output.stderr.len() > 4096 {
        return Err("cannot read deterministic source commit timestamp".into());
    }
    let seconds: u64 = String::from_utf8(output.stdout)?.trim().parse()?;
    let millis = seconds
        .checked_mul(1000)
        .ok_or("source commit timestamp overflows milliseconds")?;
    if millis > i64::MAX as u64 {
        return Err("source commit timestamp does not fit the persistent backend".into());
    }
    Ok(millis)
}

fn sanitize_git_environment(command: &mut Command) {
    for name in [
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
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_PREFIX",
    ] {
        command.env_remove(name);
    }
    command
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "0");
}

async fn create_and_activate(
    validated: &ValidatedInput,
    paths: &GenerationPaths,
) -> Result<(), AnyErr> {
    ensure_private_dir(&paths.generations)?;
    sync_dir(&paths.mneme)?;
    let key = load_or_create_key(&paths.key)?;
    let mut temp = GenerationTemp::create(&paths.generations, validated.operation_id)?;
    let materialized = materialize_generation(validated, temp.path()).await?;
    verify_git_cut(&validated.root, &validated.plan.repo)?;

    let manifest = build_manifest(&validated.plan, &materialized.node_ids)?;
    let bootstrap_dir = temp.path().join("bootstrap");
    create_private_dir(&bootstrap_dir)?;
    let manifest_path = bootstrap_dir.join("manifest.json");
    write_private_file(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)?;
    let manifest_hash = canonical_json_hash(&manifest_path)?;
    let receipt = build_receipt(validated, &materialized, &manifest_hash, &key)?;
    let receipt_path = bootstrap_dir.join("receipt.json");
    write_private_file(&receipt_path, &serde_json::to_vec_pretty(&receipt)?)?;
    // Retain the nested database lease across verification, generation rename,
    // and selector activation. The lease inode moves with the private
    // generation, so even though its original pathname no longer validates
    // after rename, its flock continues to exclude a final-path opener until
    // activation is durable.
    let generation_lease = Arc::new(mneme_store_path::StoreLease::acquire(
        &temp.path().join("memory.db"),
    )?);
    verify_private_generation(validated, temp.path(), &key, true, &generation_lease).await?;
    verify_git_cut(&validated.root, &validated.plan.repo)?;

    // Publication ordering is intentionally explicit: after all SQLite opens
    // are closed and sidecars rejected, sync the database, then the generation
    // tree, then the generation catalog. Only after that durable rename may a
    // selector be written and `.mneme` synced by activate_no_replace.
    sync_closed_generation_for_publication(temp.path())?;
    publish_generation_no_replace(temp.path(), &paths.final_generation)?;
    temp.disarm();
    activate_no_replace(paths, validated.operation_id)?;
    Ok(())
}

struct Materialized {
    node_ids: BTreeMap<String, NodeId>,
    projection_digest: String,
    effective_policy: EffectivePolicy,
    policy_fingerprint: String,
}

#[expect(clippy::too_many_lines)]
async fn materialize_generation(
    validated: &ValidatedInput,
    generation: &Path,
) -> Result<Materialized, AnyErr> {
    let plan = &validated.plan;
    let timestamp = u128::from(validated.source_timestamp_ms);
    let body_dir = generation.join("memory.bodies");
    create_private_dir(&body_dir)?;
    let embedder = crate::make_embedder(DEFAULT_DIM)?;
    let fingerprint = crate::runtime_fingerprint(embedder.as_ref())?;
    let summaries: Vec<&str> = validated
        .prepared_nodes
        .canonical
        .iter()
        .map(Node::summary)
        .collect();
    debug_assert_eq!(summaries.len(), plan.nodes.len());
    let vectors = embedder.embed(&summaries).await?;
    if vectors.len() != plan.nodes.len() {
        return Err("embedder did not return exactly one vector per bootstrap node".into());
    }

    let mut nodes = Vec::with_capacity(plan.nodes.len());
    let mut stored_vectors = Vec::with_capacity(plan.nodes.len());
    for ((node, vector), materialized) in plan
        .nodes
        .iter()
        .zip(vectors)
        .zip(&validated.prepared_nodes.canonical)
    {
        if vector.len() != DEFAULT_DIM || !vector.iter().all(|value| value.is_finite()) {
            return Err(format!("invalid embedding for bootstrap node {:?}", node.key).into());
        }
        let id = materialized.id();
        let body_path = body_dir.join(id.0.to_string());
        write_private_file(&body_path, node.body.as_bytes())?;
        nodes.push(materialized.clone());
        stored_vectors.push((id, vector));
    }

    let mut edges = Vec::with_capacity(plan.edges.len());
    for edge in &plan.edges {
        validate_edge_materialization_shape(edge)?;
        let from = validated.prepared_nodes.by_key[&edge.from_key];
        let to = validated.prepared_nodes.by_key[&edge.to_key];
        let kind = match edge.kind.as_str() {
            "associative" => EdgeKind::Associative,
            "supersedes" => EdgeKind::Supersedes,
            "derived-from" => EdgeKind::DerivedFrom,
            other => return Err(format!("unsupported bootstrap edge kind {other:?}").into()),
        };
        edges.push(Edge::new(from, to, edge.weight as f32, kind, timestamp));
    }
    let db_id = Ulid::from_string(&plan.target.db_id)?;
    let export = StoreExport {
        db_id,
        dim: DEFAULT_DIM,
        embedding_fingerprint: Some(fingerprint.clone()),
        nodes,
        edges,
        vectors: stored_vectors,
        contradictions: Vec::new(),
        merges: Vec::new(),
        full_merge_commits: Vec::new(),
        supersede_commits: Vec::new(),
        remote_edges: Vec::new(),
        feedback_retries: Vec::new(),
        concerns: Vec::new(),
        touchstones: Vec::new(),
    };
    let store = MemStore::from_export(export.clone())?;
    let database = generation.join("memory.db");
    #[cfg(feature = "cozo")]
    {
        if !database.is_absolute() {
            return Err("native bootstrap generation database path is not absolute".into());
        }
        if let Err(error) =
            CozoStore::materialize_fresh_current(&database, validated.operation_id, &store).await
        {
            // The typed error may retain the inner target's SQLite handle or
            // fresh-store lease. Never box/propagate it with `?`: the enclosing
            // GenerationTemp would then try to remove its directory first.
            // This generation is still private and outer-unpublished, so record
            // diagnostics, relinquish inner authority, and only then unwind to
            // the outer temp's discard.
            let message = error.to_string();
            let phase = error.failure_phase();
            let retained_authority = error.retains_authority();
            let target_publication = error.relinquish_for_unpublished_outer_discard();
            return Err(format!(
                "fresh-current bootstrap materialization failed at {phase:?} with inner target state {target_publication:?} (retained authority before release: {retained_authority}); the enclosing generation remains unpublished and will be discarded: {message}"
            )
            .into());
        }
    }
    #[cfg(not(feature = "cozo"))]
    store.save(&database)?;
    File::open(&database)?.sync_all()?;

    let backend_format = if cfg!(feature = "cozo") {
        "cozo-sqlite-v1"
    } else {
        "mneme-json-snapshot-v1"
    };
    let effective_policy = EffectivePolicy {
        policy_version: POLICY_VERSION.to_string(),
        automatic_similarity_links: false,
        automatic_coretrieval_links: false,
        automatic_bridges: false,
        node_id_scheme: "sha256(plan_hash,key)-128-v1".to_string(),
        timestamp_scheme: "git-source-commit-time-ms-v1".to_string(),
        source_timestamp_ms: validated.source_timestamp_ms,
        body_store: "generation-local-fs-relative-v1".to_string(),
        backend_format: backend_format.to_string(),
        embedding_fingerprint: serde_json::to_value(fingerprint)?,
    };
    let policy_fingerprint = hash_serialized(&effective_policy)?;
    let projection_digest = projection_digest(&export, &body_dir, plan.limits.bytes_per_body)?;
    Ok(Materialized {
        node_ids: validated.prepared_nodes.by_key.clone(),
        projection_digest,
        effective_policy,
        policy_fingerprint,
    })
}

fn deterministic_node_id(plan_hash: &str, key: &str) -> NodeId {
    let mut hash = Sha256::new();
    hash.update(b"mneme-bootstrap-node-v1\0");
    hash.update(plan_hash.as_bytes());
    hash.update(b"\0");
    hash.update(key.as_bytes());
    let digest = hash.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    NodeId(Ulid::from(u128::from_be_bytes(bytes)))
}

fn require_fresh_concern_state(export: &StoreExport) -> Result<(), AnyErr> {
    if !export.concerns.is_empty() {
        return Err("approved fresh bootstrap projection cannot contain advisory concerns".into());
    }
    Ok(())
}

fn projection_digest(
    export: &StoreExport,
    body_dir: &Path,
    max_body_bytes: usize,
) -> Result<String, AnyErr> {
    require_fresh_concern_state(export)?;
    let body_dir_guard = open_dir_nofollow(body_dir)?;
    let body_dir_metadata = body_dir_guard.metadata()?;
    if !body_dir_metadata.file_type().is_dir() {
        return Err("bootstrap body store is not a real directory".into());
    }
    let mut normalized = export.clone();
    normalized.nodes.sort_by_key(|node| node.id());
    normalized
        .edges
        .sort_by_key(|edge| (edge.from, edge.to, edge.kind as u8));
    normalized.vectors.sort_by_key(|(id, _)| *id);
    normalized
        .contradictions
        .sort_by_key(|item| (item.between.0, item.between.1));
    normalized
        .merges
        .sort_by_key(|item| (item.between.0, item.between.1));
    normalized
        .full_merge_commits
        .sort_by_key(|item| (item.between.0, item.between.1));
    normalized
        .remote_edges
        .sort_by_key(|edge| (edge.from, edge.target_db, edge.target));
    normalized
        .feedback_retries
        .sort_by(|left, right| left.key.cmp(&right.key));
    let mut node_values = Vec::with_capacity(normalized.nodes.len());
    for node in &normalized.nodes {
        let mut value = serde_json::to_value(node)?;
        if let Some(tags) = value.get_mut("tags").and_then(Value::as_array_mut) {
            tags.sort_by(|left, right| {
                left.as_str()
                    .unwrap_or_default()
                    .cmp(right.as_str().unwrap_or_default())
            });
        }
        node_values.push(value);
    }
    let vector_digests: Vec<Value> = normalized
        .vectors
        .iter()
        .map(|(id, vector)| {
            let mut bytes = Vec::with_capacity(vector.len() * 4);
            for value in vector {
                bytes.extend_from_slice(&value.to_bits().to_le_bytes());
            }
            json!({"node_id": id.0.to_string(), "f32le_sha256": sha256_bytes(&bytes)})
        })
        .collect();
    let canonical = json!({
        "db_id": normalized.db_id.to_string(),
        "dim": normalized.dim,
        "embedding_fingerprint": normalized.embedding_fingerprint,
        "nodes": node_values,
        "edges": normalized.edges,
        "vectors": vector_digests,
        "contradictions": normalized.contradictions,
        "merges": normalized.merges,
        "full_merge_commits": normalized.full_merge_commits,
        "remote_edges": normalized.remote_edges,
        "feedback_retries": normalized.feedback_retries,
    });
    let mut hash = Sha256::new();
    hash.update(b"mneme-bootstrap-projection-v2\0");
    hash.update(serde_json::to_vec(&canonical)?);
    for node in &normalized.nodes {
        let body = read_bounded_regular(
            &body_dir.join(node.id().0.to_string()),
            max_body_bytes,
            "bootstrap projection body",
        )?;
        hash.update(node.id().0.to_string().as_bytes());
        hash.update([0]);
        hash.update(Sha256::digest(body));
    }
    verify_path_still_names_open_file_or_dir(body_dir, &body_dir_metadata, true)?;
    drop(body_dir_guard);
    Ok(hex_digest(hash.finalize().as_slice()))
}

fn build_manifest(
    plan: &GreenfieldPlan,
    node_ids: &BTreeMap<String, NodeId>,
) -> Result<Value, AnyErr> {
    let mut files: BTreeMap<String, Value> = BTreeMap::new();
    let mut nodes = BTreeMap::new();
    for node in &plan.nodes {
        for source in &node.sources {
            insert_manifest_file(&mut files, source)?;
        }
        let claim = recovery_claim(&node.body)?;
        nodes.insert(
            node.key.clone(),
            json!({
                "current": {
                    "node_id": node_ids[&node.key].0.to_string(),
                    "content_hash": node.content_hash,
                    "materialization_hash": node.materialization_hash,
                    "summary": node.summary,
                    "claim": claim,
                    "tags": node.tags,
                    "status": node.status,
                    "stability": node.stability,
                    "confidence": node.confidence,
                    "body_source_commit": plan.repo.head,
                    "evidence_commit": plan.repo.head,
                    "evidence": node.sources,
                    "state": "current"
                },
                "history": []
            }),
        );
    }
    let mut edges = BTreeMap::new();
    for edge in &plan.edges {
        for source in &edge.sources {
            insert_manifest_file(&mut files, source)?;
        }
        edges.insert(
            edge.key.clone(),
            json!({
                "current": {
                    "from_key": edge.from_key,
                    "to_key": edge.to_key,
                    "from_node_id": node_ids[&edge.from_key].0.to_string(),
                    "to_node_id": node_ids[&edge.to_key].0.to_string(),
                    "kind": edge.kind,
                    "weight": edge.weight,
                    "assertion_hash": edge.assertion_hash,
                    "evidence_commit": plan.repo.head,
                    "evidence": edge.sources,
                    "state": "current"
                },
                "history": []
            }),
        );
    }
    Ok(json!({
        "schema_version": 1,
        "namespace": NAMESPACE,
        "db_id": plan.target.db_id,
        "generation": 1,
        "repo": plan.repo,
        "files": files,
        "nodes": nodes,
        "edges": edges,
        "applied_plan_hash": plan.plan_hash
    }))
}

fn insert_manifest_file(
    files: &mut BTreeMap<String, Value>,
    evidence: &Evidence,
) -> Result<(), AnyErr> {
    let value = json!({"blob": evidence.blob, "sha256": evidence.sha256});
    if let Some(existing) = files.insert(evidence.path.clone(), value.clone())
        && existing != value
    {
        return Err(format!(
            "evidence path {:?} has conflicting identities",
            evidence.path
        )
        .into());
    }
    Ok(())
}

fn recovery_claim(body: &str) -> Result<String, AnyErr> {
    let claim = body
        .splitn(10, '\n')
        .nth(9)
        .and_then(|value| value.strip_suffix('\n'))
        .ok_or("bootstrap body does not have the exact recovery header/trailing LF")?;
    if claim.is_empty() {
        return Err("bootstrap recovery claim is blank".into());
    }
    Ok(claim.to_string())
}

fn canonical_json_hash(path: &Path) -> Result<String, AnyErr> {
    const CODE: &str = r#"import hashlib,json,sys
raw=sys.stdin.buffer.read()
value=json.loads(raw.decode('utf-8'))
canonical=json.dumps(value,ensure_ascii=True,allow_nan=False,sort_keys=True,separators=(',',':')).encode('utf-8')
print(hashlib.sha256(canonical).hexdigest())
"#;
    let raw = read_bounded_regular(path, PLAN_MAX_BYTES, "native JSON artifact")?;
    let mut command = Command::new("python3");
    command
        .arg("-I")
        .arg("-B")
        .arg("-c")
        .arg(CODE)
        .env("PYTHONDONTWRITEBYTECODE", "1");
    let output = run_bounded_command(command, &raw, VALIDATOR_TIMEOUT)?;
    if !output.status.success() {
        return Err(format!(
            "cannot canonicalize native JSON artifact: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    parse_hex_line(&output.stdout, "canonical JSON hash")
}

fn build_receipt(
    validated: &ValidatedInput,
    materialized: &Materialized,
    manifest_hash: &str,
    key: &[u8; KEY_BYTES],
) -> Result<NativeReceipt, AnyErr> {
    let node_ids = materialized
        .node_ids
        .iter()
        .map(|(key, id)| (key.clone(), id.0.to_string()))
        .collect();
    let edge_ids = validated
        .plan
        .edges
        .iter()
        .map(|edge| {
            (
                edge.key.clone(),
                ReceiptEdge {
                    from_node_id: materialized.node_ids[&edge.from_key].0.to_string(),
                    to_node_id: materialized.node_ids[&edge.to_key].0.to_string(),
                },
            )
        })
        .collect();
    let storage_incarnation = sha256_join(&[
        b"mneme-bootstrap-storage-v1",
        validated.plan.plan_hash.as_bytes(),
        validated.operation_id.to_string().as_bytes(),
    ]);
    let key_id = sha256_bytes(key);
    let payload = ReceiptPayload {
        schema_version: 1,
        namespace: RECEIPT_NAMESPACE.to_string(),
        plan_hash: validated.plan.plan_hash.clone(),
        approval_hash: validated.approval_hash.clone(),
        reviewer_id: validated.approval.reviewer_id.clone(),
        db_id: validated.plan.target.db_id.clone(),
        storage_incarnation,
        storage_generation: 1,
        operation_id: validated.operation_id.to_string(),
        absent_target_precondition: true,
        effective_policy: materialized.effective_policy.clone(),
        policy_fingerprint: materialized.policy_fingerprint.clone(),
        projection_digest: materialized.projection_digest.clone(),
        node_ids,
        edge_ids,
        manifest_path: "bootstrap/manifest.json".to_string(),
        manifest_hash: manifest_hash.to_string(),
        activation_mode: "atomic-no-clobber-relative-symlink-v1".to_string(),
        activation_target: format!("generations/{}", validated.operation_id),
        publication_state: "activation-bound".to_string(),
        key_id,
    };
    let mac_hmac_sha256 = hmac_hex(key, &serde_json::to_vec(&payload)?);
    Ok(NativeReceipt {
        payload,
        mac_hmac_sha256,
    })
}

async fn recover_exact_retry(
    validated: &ValidatedInput,
    paths: &GenerationPaths,
    store_lease: &Arc<mneme_store_path::StoreLease>,
) -> Result<(), AnyErr> {
    let key = load_existing_key(&paths.key)?;
    let expected = PathBuf::from("generations").join(validated.operation_id.to_string());
    let already_active =
        read_activation_target(&paths.current).as_deref() == Some(expected.as_path());
    // Once activated, the graph is intentionally mutable. An exact retry returns
    // the authenticated creation result without pretending later legitimate graph
    // changes still equal the original projection. Before activation, however,
    // every byte is re-proved before the symlink can become visible.
    // A previously active generation can carry a normal SQLite WAL across a
    // synthetic/interrupted selector loss. Authenticate its receipt, manifest,
    // database identity, and policy before the identity verifier checkpoints
    // that WAL. The exact projection/inventory proof then runs over the closed
    // main file; malformed unauthenticated residue is never opened for repair.
    if generation_has_sqlite_sidecars(&paths.final_generation)? {
        verify_private_generation(validated, &paths.final_generation, &key, false, store_lease)
            .await?;
        #[cfg(feature = "cozo")]
        crate::reject_sqlite_sidecars(&paths.final_generation.join("memory.db"))?;
    }
    verify_private_generation(
        validated,
        &paths.final_generation,
        &key,
        !already_active,
        store_lease,
    )
    .await?;
    // A crash can occur after the no-clobber generation rename but before its
    // parent directory's fsync. Re-prove the closed generation, then repair the
    // durable catalog boundary before making (or confirming) the selector.
    sync_closed_generation_for_publication(&paths.final_generation)?;
    sync_dir(&paths.generations)?;
    activate_no_replace(paths, validated.operation_id)?;
    Ok(())
}

async fn verify_private_generation(
    validated: &ValidatedInput,
    generation: &Path,
    key: &[u8; KEY_BYTES],
    verify_projection: bool,
    store_lease: &Arc<mneme_store_path::StoreLease>,
) -> Result<(), AnyErr> {
    let generation_guard = open_dir_nofollow(generation)?;
    let generation_metadata = generation_guard.metadata()?;
    if !generation_metadata.file_type().is_dir() {
        return Err("native bootstrap generation is not a real directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if generation_metadata.permissions().mode() & 0o077 != 0 {
            return Err("native bootstrap generation must remain private (0700)".into());
        }
    }
    let receipt_path = generation.join("bootstrap/receipt.json");
    let raw = read_bounded_regular(&receipt_path, RECEIPT_MAX_BYTES, "native receipt")?;
    let receipt: NativeReceipt = serde_json::from_slice(&raw)?;
    let expected_mac = hmac_hex(key, &serde_json::to_vec(&receipt.payload)?);
    if !constant_time_eq(receipt.mac_hmac_sha256.as_bytes(), expected_mac.as_bytes()) {
        return Err("native bootstrap receipt authentication failed".into());
    }
    if receipt.payload.schema_version != 1
        || receipt.payload.namespace != RECEIPT_NAMESPACE
        || receipt.payload.plan_hash != validated.plan.plan_hash
        || receipt.payload.approval_hash != validated.approval_hash
        || receipt.payload.reviewer_id != validated.approval.reviewer_id
        || receipt.payload.db_id != validated.plan.target.db_id
        || receipt.payload.operation_id != validated.operation_id.to_string()
        || !receipt.payload.absent_target_precondition
        || receipt.payload.storage_generation != 1
        || receipt.payload.storage_incarnation
            != sha256_join(&[
                b"mneme-bootstrap-storage-v1",
                validated.plan.plan_hash.as_bytes(),
                validated.operation_id.to_string().as_bytes(),
            ])
        || receipt.payload.activation_mode != "atomic-no-clobber-relative-symlink-v1"
        || receipt.payload.activation_target != format!("generations/{}", validated.operation_id)
        || receipt.payload.publication_state != "activation-bound"
        || receipt.payload.manifest_path != "bootstrap/manifest.json"
        || receipt.payload.key_id != sha256_bytes(key)
        || receipt.payload.policy_fingerprint != hash_serialized(&receipt.payload.effective_policy)?
    {
        return Err("native bootstrap receipt does not bind this exact operation".into());
    }
    let expected_ids: BTreeMap<String, NodeId> = validated
        .plan
        .nodes
        .iter()
        .map(|node| {
            (
                node.key.clone(),
                deterministic_node_id(&validated.plan.plan_hash, &node.key),
            )
        })
        .collect();
    let expected_receipt_ids: BTreeMap<String, String> = expected_ids
        .iter()
        .map(|(key, id)| (key.clone(), id.0.to_string()))
        .collect();
    let expected_edge_ids: BTreeMap<String, ReceiptEdge> = validated
        .plan
        .edges
        .iter()
        .map(|edge| {
            (
                edge.key.clone(),
                ReceiptEdge {
                    from_node_id: expected_ids[&edge.from_key].0.to_string(),
                    to_node_id: expected_ids[&edge.to_key].0.to_string(),
                },
            )
        })
        .collect();
    if receipt.payload.node_ids != expected_receipt_ids
        || receipt.payload.edge_ids != expected_edge_ids
        || receipt.payload.effective_policy.policy_version != POLICY_VERSION
        || receipt.payload.effective_policy.automatic_similarity_links
        || receipt.payload.effective_policy.automatic_coretrieval_links
        || receipt.payload.effective_policy.automatic_bridges
        || receipt.payload.effective_policy.node_id_scheme != "sha256(plan_hash,key)-128-v1"
        || receipt.payload.effective_policy.timestamp_scheme != "git-source-commit-time-ms-v1"
        || receipt.payload.effective_policy.source_timestamp_ms != validated.source_timestamp_ms
        || receipt.payload.effective_policy.body_store != "generation-local-fs-relative-v1"
    {
        return Err(
            "native bootstrap receipt identity/policy mapping is not the fixed v1 projection"
                .into(),
        );
    }
    // Before an unselected generation may become visible, prove its complete
    // filesystem census. This rejects interrupted nested fresh-stage work,
    // SQLite sidecars, extra authority files, and unreferenced bodies rather
    // than letting a receipt authenticate only a convenient subset.
    if verify_projection {
        verify_generation_inventory(generation, expected_ids.values())?;
    }
    let manifest = generation.join(&receipt.payload.manifest_path);
    if canonical_json_hash(&manifest)? != receipt.payload.manifest_hash {
        return Err("native bootstrap manifest hash mismatch".into());
    }
    let manifest_raw = read_bounded_regular(&manifest, PLAN_MAX_BYTES, "native manifest")?;
    let manifest_value: Value = serde_json::from_slice(&manifest_raw)?;
    if manifest_value != build_manifest(&validated.plan, &expected_ids)? {
        return Err("native bootstrap manifest does not exactly describe the approved plan".into());
    }
    verify_generation_identity(generation, &receipt.payload, store_lease).await?;
    if !verify_projection {
        verify_path_still_names_open_file_or_dir(generation, &generation_metadata, true)?;
        drop(generation_guard);
        return Ok(());
    }
    let export = load_generation_export(generation, store_lease).await?;
    if export.db_id.to_string() != receipt.payload.db_id {
        return Err("native bootstrap database id mismatch".into());
    }
    let digest = projection_digest(
        &export,
        &generation.join("memory.bodies"),
        validated.plan.limits.bytes_per_body,
    )?;
    if digest != receipt.payload.projection_digest {
        return Err(format!(
            "native bootstrap projection digest mismatch: receipt {}, materialized {digest}",
            receipt.payload.projection_digest
        )
        .into());
    }
    verify_export_against_plan(
        &validated.plan,
        generation,
        &export,
        &expected_ids,
        &receipt.payload.effective_policy,
        validated.source_timestamp_ms,
    )?;
    if export.nodes.len() != validated.plan.nodes.len()
        || export.edges.len() != validated.plan.edges.len()
        || export.vectors.len() != validated.plan.nodes.len()
        || !export.contradictions.is_empty()
        || !export.merges.is_empty()
        || !export.full_merge_commits.is_empty()
        || !export.remote_edges.is_empty()
        || !export.feedback_retries.is_empty()
        || !export.concerns.is_empty()
    {
        return Err("native bootstrap projection cardinality mismatch".into());
    }
    verify_path_still_names_open_file_or_dir(generation, &generation_metadata, true)?;
    drop(generation_guard);
    Ok(())
}

fn verify_generation_inventory<'a>(
    generation: &Path,
    expected_ids: impl IntoIterator<Item = &'a NodeId>,
) -> Result<(), AnyErr> {
    let database = generation.join("memory.db");
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = database.as_os_str().to_os_string();
        sidecar.push(suffix);
        match fs::symlink_metadata(Path::new(&sidecar)) {
            Ok(_) => {
                return Err(format!(
                    "native bootstrap generation retains SQLite sidecar {}",
                    Path::new(&sidecar).display()
                )
                .into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    let lease_name = mneme_store_path::store_lock_path(&database)?
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("native bootstrap lease filename is not UTF-8")?
        .to_owned();
    let expected_top = BTreeSet::from([
        "memory.db".to_owned(),
        "memory.bodies".to_owned(),
        "bootstrap".to_owned(),
        lease_name.clone(),
    ]);
    verify_inventory_directory(
        generation,
        &expected_top,
        &["memory.db", "memory.bodies", "bootstrap"],
        "native bootstrap generation",
    )?;

    let bootstrap = generation.join("bootstrap");
    let expected_bootstrap =
        BTreeSet::from(["manifest.json".to_owned(), "receipt.json".to_owned()]);
    verify_inventory_directory(
        &bootstrap,
        &expected_bootstrap,
        &["manifest.json", "receipt.json"],
        "native bootstrap authority",
    )?;

    let expected_bodies: BTreeSet<String> = expected_ids
        .into_iter()
        .map(|id| id.0.to_string())
        .collect();
    let required_bodies: Vec<&str> = expected_bodies.iter().map(String::as_str).collect();
    let body_dir = generation.join("memory.bodies");
    verify_inventory_directory(
        &body_dir,
        &expected_bodies,
        &required_bodies,
        "native bootstrap body store",
    )?;

    for path in [
        generation.join("memory.db"),
        bootstrap.join("manifest.json"),
        bootstrap.join("receipt.json"),
    ] {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "native bootstrap inventory entry {} is not a real regular file",
                path.display()
            )
            .into());
        }
    }
    let lease = generation.join(&lease_name);
    match fs::symlink_metadata(&lease) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => {}
        Ok(_) => {
            return Err(format!(
                "native bootstrap lease {} is not a real regular file",
                lease.display()
            )
            .into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    for body in &expected_bodies {
        let path = body_dir.join(body);
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "native bootstrap body {} is not a real regular file",
                path.display()
            )
            .into());
        }
    }
    Ok(())
}

fn verify_inventory_directory(
    path: &Path,
    allowed: &BTreeSet<String>,
    required: &[&str],
    label: &str,
) -> Result<(), AnyErr> {
    let guard = open_dir_nofollow(path)?;
    let opened = guard.metadata()?;
    if !opened.file_type().is_dir() {
        return Err(format!("{label} {} is not a real directory", path.display()).into());
    }
    let mut seen = BTreeSet::new();
    for (index, entry) in fs::read_dir(path)?.enumerate() {
        if index >= 1024 {
            return Err(format!("{label} exceeds the native inspection bound").into());
        }
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("{label} contains a non-UTF-8 entry"))?;
        if !allowed.contains(&name) {
            return Err(format!("{label} contains unexpected entry {name:?}").into());
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(format!("{label} entry {name:?} is a symlink").into());
        }
        seen.insert(name);
    }
    for name in required {
        if !seen.contains(*name) {
            return Err(format!("{label} is missing required entry {name:?}").into());
        }
    }
    verify_path_still_names_open_file_or_dir(path, &opened, true)?;
    Ok(())
}

async fn verify_generation_identity(
    generation: &Path,
    receipt: &ReceiptPayload,
    _store_lease: &Arc<mneme_store_path::StoreLease>,
) -> Result<(), AnyErr> {
    // Cozo accepts a pathname rather than an already-open descriptor. The
    // generation parent is a verified private 0700 directory and remains held
    // open by the caller; this function anchors the database inode before and
    // after the backend open. A hostile process running as the same OS user is
    // outside the local trust boundary, but any observed replacement fails.
    let database = generation.join("memory.db");
    let database_guard = open_read_nofollow(&database)?;
    let metadata = database_guard.metadata()?;
    if !metadata.file_type().is_file() {
        return Err("native bootstrap database is not a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("native bootstrap database must be single-linked".into());
        }
    }
    let (db_id, fingerprint) = match receipt.effective_policy.backend_format.as_str() {
        "mneme-json-snapshot-v1" => {
            #[cfg(feature = "cozo")]
            if !crate::is_snapshot(&database)? {
                return Err("receipt says snapshot but generation database is not JSON".into());
            }
            verify_path_still_names_open_file(&database, &metadata)?;
            let store = MemStore::load(&database)?;
            (store.db_id(), store.embedding_fingerprint()?)
        }
        "cozo-sqlite-v1" => {
            #[cfg(feature = "cozo")]
            {
                if crate::is_snapshot(&database)? {
                    return Err("receipt says cozo but generation database is JSON".into());
                }
                verify_path_still_names_open_file(&database, &metadata)?;
                let store = CozoStore::open_existing_persistent(
                    &database,
                    DEFAULT_DIM,
                    _store_lease.clone(),
                )?;
                let identity = (store.db_id(), store.embedding_fingerprint()?);
                // Verification itself must leave the closed generation
                // publishable; Cozo reads can otherwise retain a WAL after
                // proving the bytes we are about to activate.
                store.prepare_for_file_move()?;
                drop(store);
                crate::reject_sqlite_sidecars(&database)?;
                identity
            }
            #[cfg(not(feature = "cozo"))]
            return Err("this mnemed build cannot verify a cozo bootstrap generation".into());
        }
        other => return Err(format!("unsupported bootstrap backend format {other:?}").into()),
    };
    if db_id.to_string() != receipt.db_id
        || serde_json::to_value(fingerprint)?
            != serde_json::to_value(Some(receipt.effective_policy.embedding_fingerprint.clone()))?
    {
        return Err("active bootstrap database identity/fingerprint mismatch".into());
    }
    verify_path_still_names_open_file(&database, &metadata)?;
    drop(database_guard);
    Ok(())
}

fn verify_export_against_plan(
    plan: &GreenfieldPlan,
    generation: &Path,
    export: &StoreExport,
    expected_ids: &BTreeMap<String, NodeId>,
    policy: &EffectivePolicy,
    source_timestamp_ms: u64,
) -> Result<(), AnyErr> {
    require_fresh_concern_state(export)?;
    let expected_origin_commit = OriginCommit::parse(&plan.repo.head)?;
    let fingerprint = serde_json::to_value(&export.embedding_fingerprint)?;
    let expected_fingerprint = Some(policy.embedding_fingerprint.clone());
    if fingerprint != serde_json::to_value(expected_fingerprint)?
        || export.dim != DEFAULT_DIM
        || policy.node_id_scheme != "sha256(plan_hash,key)-128-v1"
        || policy.timestamp_scheme != "git-source-commit-time-ms-v1"
        || policy.source_timestamp_ms != source_timestamp_ms
        || policy.body_store != "generation-local-fs-relative-v1"
        || policy.backend_format
            != if cfg!(feature = "cozo") {
                "cozo-sqlite-v1"
            } else {
                "mneme-json-snapshot-v1"
            }
    {
        return Err("native bootstrap effective storage/embedding policy mismatch".into());
    }
    let by_id: BTreeMap<NodeId, &Node> =
        export.nodes.iter().map(|node| (node.id(), node)).collect();
    let vector_ids: BTreeSet<NodeId> = export.vectors.iter().map(|(id, _)| *id).collect();
    if by_id.len() != plan.nodes.len() || vector_ids.len() != plan.nodes.len() {
        return Err("native bootstrap contains duplicate or missing node/vector ids".into());
    }
    let timestamp = u128::from(source_timestamp_ms);
    for expected in &plan.nodes {
        let id = expected_ids[&expected.key];
        let node = by_id
            .get(&id)
            .ok_or_else(|| format!("bootstrap node {:?} is missing", expected.key))?;
        let expected_status = NodeStatus::Active;
        let mut actual_tags: Vec<&str> = node.tags().collect();
        actual_tags.sort_unstable();
        let expected_tags: Vec<&str> = expected.tags.iter().map(String::as_str).collect();
        if node.summary() != expected.summary
            || node.body().as_str() != format!("fs://{}", id.0)
            || node.body_ownership() != BodyOwnership::Managed
            || actual_tags != expected_tags
            || node.status() != expected_status
            || node.stability().to_bits() != (expected.stability as f32).to_bits()
            || node.confidence().to_bits() != (expected.confidence as f32).to_bits()
            || node.created() != timestamp
            || node.origin_commit() != Some(expected_origin_commit)
            || !vector_ids.contains(&id)
        {
            return Err(format!(
                "bootstrap node {:?} does not exactly match the approved materialization",
                expected.key
            )
            .into());
        }
        let body = read_bounded_regular(
            &generation.join("memory.bodies").join(id.0.to_string()),
            plan.limits.bytes_per_body,
            "bootstrap body",
        )?;
        if body != expected.body.as_bytes() {
            return Err(format!("bootstrap body {:?} differs from the plan", expected.key).into());
        }
    }
    let by_pair: BTreeMap<(NodeId, NodeId), &Edge> = export
        .edges
        .iter()
        .map(|edge| ((edge.from, edge.to), edge))
        .collect();
    if by_pair.len() != plan.edges.len() {
        return Err("native bootstrap contains duplicate or missing edge pairs".into());
    }
    for expected in &plan.edges {
        let pair = (
            expected_ids[&expected.from_key],
            expected_ids[&expected.to_key],
        );
        let edge = by_pair
            .get(&pair)
            .ok_or_else(|| format!("bootstrap edge {:?} is missing", expected.key))?;
        let kind = match expected.kind.as_str() {
            "associative" => EdgeKind::Associative,
            "supersedes" => EdgeKind::Supersedes,
            "derived-from" => EdgeKind::DerivedFrom,
            _ => return Err("validated bootstrap edge kind changed unexpectedly".into()),
        };
        if edge.kind != kind
            || edge.weight().to_bits() != (expected.weight as f32).to_bits()
            || edge.anchor.is_some()
            || edge.trials() != 0
            || edge.interference() != 0
            || edge.last_reinforced() != timestamp
        {
            return Err(format!(
                "bootstrap edge {:?} does not exactly match the approved assertion",
                expected.key
            )
            .into());
        }
    }
    Ok(())
}

async fn load_generation_export(
    generation: &Path,
    _store_lease: &Arc<mneme_store_path::StoreLease>,
) -> Result<StoreExport, AnyErr> {
    let database = generation.join("memory.db");
    let guard = open_read_nofollow(&database)?;
    let opened = guard.metadata()?;
    if !opened.file_type().is_file() {
        return Err("generation database is not a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.nlink() != 1 {
            return Err("generation database must be single-linked".into());
        }
    }
    #[cfg(feature = "cozo")]
    {
        if crate::is_snapshot(&database)? {
            verify_path_still_names_open_file(&database, &opened)?;
            let export = MemStore::load(&database)?.export();
            verify_path_still_names_open_file(&database, &opened)?;
            drop(guard);
            return Ok(export);
        }
        verify_path_still_names_open_file(&database, &opened)?;
        let store =
            CozoStore::open_existing_persistent(&database, DEFAULT_DIM, _store_lease.clone())?;
        let export = store.export().await?;
        store.prepare_for_file_move()?;
        drop(store);
        crate::reject_sqlite_sidecars(&database)?;
        verify_path_still_names_open_file(&database, &opened)?;
        drop(guard);
        Ok(export)
    }
    #[cfg(not(feature = "cozo"))]
    {
        verify_path_still_names_open_file(&database, &opened)?;
        let export = MemStore::load(&database)?.export();
        verify_path_still_names_open_file(&database, &opened)?;
        drop(guard);
        Ok(export)
    }
}

fn sync_closed_generation_for_publication(generation: &Path) -> Result<(), AnyErr> {
    let database = generation.join("memory.db");
    #[cfg(feature = "cozo")]
    crate::reject_sqlite_sidecars(&database)?;
    File::open(&database)?.sync_all()?;
    sync_dir(&generation.join("memory.bodies"))?;
    sync_dir(&generation.join("bootstrap"))?;
    sync_dir(generation)?;
    Ok(())
}

fn generation_has_sqlite_sidecars(generation: &Path) -> Result<bool, AnyErr> {
    #[cfg(feature = "cozo")]
    {
        let database = generation.join("memory.db");
        let mut has_sidecar = false;
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut candidate = database.as_os_str().to_os_string();
            candidate.push(suffix);
            match fs::symlink_metadata(Path::new(&candidate)) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    return Err(format!(
                        "generation SQLite sidecar {} is not a real regular file",
                        Path::new(&candidate).display()
                    )
                    .into());
                }
                Ok(_) => has_sidecar = true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(has_sidecar)
    }
    #[cfg(not(feature = "cozo"))]
    {
        let _ = generation;
        Ok(false)
    }
}

struct GenerationTemp {
    path: Option<PathBuf>,
}

impl GenerationTemp {
    fn create(parent: &Path, operation_id: Ulid) -> Result<Self, AnyErr> {
        for _ in 0..16 {
            let path = parent.join(format!(".bootstrap-{operation_id}-{}", Ulid::new()));
            match create_private_dir(&path) {
                Ok(()) => return Ok(Self { path: Some(path) }),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err("could not reserve a private bootstrap generation".into())
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("generation temp is armed")
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for GenerationTemp {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn publish_generation_no_replace(from: &Path, to: &Path) -> Result<(), AnyErr> {
    rename_no_replace(from, to)?;
    sync_dir(to.parent().ok_or("generation target has no parent")?)?;
    Ok(())
}

fn activate_no_replace(paths: &GenerationPaths, operation_id: Ulid) -> Result<(), AnyErr> {
    let expected = PathBuf::from("generations").join(operation_id.to_string());
    cleanup_abandoned_activation_temps(paths, operation_id, &expected)?;
    match fs::symlink_metadata(&paths.current) {
        Ok(_) => {
            if read_activation_target(&paths.current).as_deref() == Some(expected.as_path()) {
                // An already-crossed selector rename still needs a parent fsync
                // before this exact retry may report a durable activation.
                sync_dir(&paths.mneme)?;
                return Ok(());
            }
            return Err("an existing activation points at a different generation; managed update is refused".into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let temp = paths
            .mneme
            .join(format!(".current-{operation_id}-{}", Ulid::new()));
        symlink(&expected, &temp)?;
        match rename_no_replace(&temp, &paths.current) {
            Ok(()) => {
                sync_dir(&paths.mneme)?;
                Ok(())
            }
            Err(error) => {
                let _ = fs::remove_file(&temp);
                if read_activation_target(&paths.current).as_deref() == Some(expected.as_path()) {
                    sync_dir(&paths.mneme)?;
                    Ok(())
                } else {
                    Err(error.into())
                }
            }
        }
    }
    #[cfg(not(unix))]
    Err("atomic bootstrap activation requires Unix symlinks".into())
}

/// Remove only selector temps that parse as this exact operation and point at
/// its exact relative target. Anything malformed or differently targeted is
/// preserved and blocks activation for operator review.
fn cleanup_abandoned_activation_temps(
    paths: &GenerationPaths,
    operation_id: Ulid,
    expected: &Path,
) -> Result<(), AnyErr> {
    let metadata = match fs::symlink_metadata(&paths.mneme) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(".mneme must be a real directory".into());
    }
    let mneme_guard = open_dir_nofollow(&paths.mneme)?;
    let mneme_opened = mneme_guard.metadata()?;
    let operation = operation_id.to_string();
    let mut stale = Vec::new();
    for (index, entry) in fs::read_dir(&paths.mneme)?.enumerate() {
        if index >= 1024 {
            return Err(".mneme exceeds the native inspection bound".into());
        }
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ".mneme contains a non-UTF-8 entry")?;
        let parsed = private_activation_operation(&name);
        if parsed.as_deref() != Some(operation.as_str()) {
            if parsed.is_none() && name.strip_prefix(".current-").is_some() {
                return Err(format!(
                    "malformed private activation entry {name:?}; refusing cleanup"
                )
                .into());
            }
            continue;
        }
        let path = entry.path();
        let entry_metadata = fs::symlink_metadata(&path)?;
        if !entry_metadata.file_type().is_symlink() {
            return Err(format!(
                "private activation entry {} is not a symlink",
                path.display()
            )
            .into());
        }
        if read_activation_target(&path).as_deref() != Some(expected) {
            return Err(format!(
                "private activation entry {} targets a different generation",
                path.display()
            )
            .into());
        }
        stale.push(path);
    }
    verify_path_still_names_open_file_or_dir(&paths.mneme, &mneme_opened, true)?;
    for path in &stale {
        fs::remove_file(path)?;
    }
    if !stale.is_empty() {
        mneme_guard.sync_all()?;
    }
    Ok(())
}

fn read_activation_target(path: &Path) -> Option<PathBuf> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_symlink() {
        return None;
    }
    fs::read_link(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_projection_freezes_empty_digest_and_refuses_advisory_rows() {
        use mneme_core::{
            ConcernBinding, ConcernDigest, ConcernEndpoint, ConcernKind, ConcernNotice, ConcernRow,
        };
        let root = std::env::temp_dir().join(format!("mneme-bootstrap-concern-{}", Ulid::new()));
        fs::create_dir(&root).unwrap();
        let mut export = MemStore::new(4).export();
        export.db_id = Ulid::from(1u128);
        // Frozen pre-concern v2 digest; no new empty relation changes its bytes.
        assert_eq!(
            projection_digest(&export, &root, 1024).unwrap(),
            "2bf2691983d0f6f781bf17ba201e830dde85f1bbd59f2b792a29cd932da4f07f"
        );
        let binding = ConcernBinding::new(
            ConcernKind::Disagreement,
            ConcernEndpoint::new(NodeId(Ulid::from(1u128)), ConcernDigest::of_bytes(b"a")),
            ConcernEndpoint::new(NodeId(Ulid::from(2u128)), ConcernDigest::of_bytes(b"b")),
        )
        .unwrap();
        export.concerns.push(ConcernRow::from_notice(
            ConcernNotice::new(
                binding,
                "advisory mutation",
                "fresh projection has no advisory mandate",
            )
            .unwrap(),
        ));
        assert!(
            require_fresh_concern_state(&export)
                .unwrap_err()
                .to_string()
                .contains("advisory concerns")
        );
        let error = projection_digest(&export, &root.join("absent"), 1024).unwrap_err();
        assert!(
            error.to_string().contains("advisory concerns"),
            "refuse before body IO: {error}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn materializable_plan_node() -> PlanNode {
        PlanNode {
            key: "component:memory".into(),
            content_hash: "content".into(),
            materialization_hash: "materialized".into(),
            summary: "summary".into(),
            body: "body".into(),
            tags: vec!["component".into()],
            status: "active".into(),
            stability: 0.5,
            confidence: 0.5,
            sources: Vec::new(),
            action: "ingest".into(),
        }
    }

    fn materializable_plan_edge() -> PlanEdge {
        PlanEdge {
            key: "edge:memory-store".into(),
            from_key: "component:memory".into(),
            to_key: "component:store".into(),
            kind: "associative".into(),
            weight: 0.5,
            assertion_hash: "assertion".into(),
            sources: Vec::new(),
            action: "link".into(),
        }
    }

    #[test]
    fn pre_inference_shape_rejects_non_materializable_nodes_and_edges() {
        let node = materializable_plan_node();
        validate_node_materialization_shape(&node).unwrap();

        let mut invalid_tag = materializable_plan_node();
        invalid_tag.tags = vec![" untrimmed".into()];
        assert!(validate_node_materialization_shape(&invalid_tag).is_err());
        let mut duplicate_tag = materializable_plan_node();
        duplicate_tag.tags = vec!["same".into(), "same".into()];
        assert!(validate_node_materialization_shape(&duplicate_tag).is_err());
        let mut invalid_status = materializable_plan_node();
        invalid_status.status = "archived".into();
        assert!(validate_node_materialization_shape(&invalid_status).is_err());

        let edge = materializable_plan_edge();
        validate_edge_materialization_shape(&edge).unwrap();
        let mut invalid_kind = materializable_plan_edge();
        invalid_kind.kind = "transition".into();
        assert!(validate_edge_materialization_shape(&invalid_kind).is_err());
        let mut invalid_weight = materializable_plan_edge();
        invalid_weight.weight = f64::NAN;
        assert!(validate_edge_materialization_shape(&invalid_weight).is_err());
        invalid_weight.weight = 1.01;
        assert!(validate_edge_materialization_shape(&invalid_weight).is_err());
    }

    #[test]
    fn canonical_node_preparation_enforces_summary_policy_and_domain_invariants() {
        let plan_hash = "b".repeat(64);
        let origin_commit = "a".repeat(40);

        let valid = prepare_canonical_nodes(
            &plan_hash,
            &origin_commit,
            42,
            &[materializable_plan_node()],
        )
        .unwrap();
        assert_eq!(valid.canonical.len(), 1);
        assert_eq!(valid.canonical[0].summary(), "summary");
        assert_eq!(valid.canonical[0].created(), 42);
        assert_eq!(valid.by_key["component:memory"], valid.canonical[0].id());

        let mut blank = materializable_plan_node();
        blank.summary = " \n\t".into();
        let blank_error = prepare_canonical_nodes(&plan_hash, &origin_commit, 42, &[blank])
            .err()
            .expect("blank summary must be rejected");
        assert!(blank_error.to_string().contains("must not be blank"));

        let mut exact_policy_max = materializable_plan_node();
        exact_policy_max.summary = "x".repeat(REPO_SYNC_MAX_SUMMARY_BYTES);
        prepare_canonical_nodes(&plan_hash, &origin_commit, 42, &[exact_policy_max])
            .expect("exact repo-sync-v1 summary maximum must remain accepted");

        let mut over_policy_max = materializable_plan_node();
        over_policy_max.summary = "x".repeat(REPO_SYNC_MAX_SUMMARY_BYTES + 1);
        let oversized_error =
            prepare_canonical_nodes(&plan_hash, &origin_commit, 42, &[over_policy_max])
                .err()
                .expect("repo-sync-v1-oversized summary must be rejected");
        assert!(
            oversized_error
                .to_string()
                .contains("repo-sync-v1 hard maximum")
        );
    }

    #[test]
    fn deterministic_node_ids_bind_plan_and_key() {
        let a = deterministic_node_id(&"a".repeat(64), "project:overview");
        assert_eq!(
            a,
            deterministic_node_id(&"a".repeat(64), "project:overview")
        );
        assert_ne!(
            a,
            deterministic_node_id(&"b".repeat(64), "project:overview")
        );
        assert_ne!(a, deterministic_node_id(&"a".repeat(64), "component:other"));
    }

    #[test]
    fn target_inspection_refuses_foreign_published_generation() {
        let requested_root =
            std::env::temp_dir().join(format!("mnemed-bootstrap-catalog-{}", Ulid::new()));
        fs::create_dir(&requested_root).unwrap();
        let root = fs::canonicalize(&requested_root).unwrap();
        let operation_id = Ulid::new();
        let paths = GenerationPaths::new(&root, operation_id);
        fs::create_dir_all(&paths.generations).unwrap();
        fs::create_dir(paths.generations.join(Ulid::new().to_string())).unwrap();

        assert_eq!(
            inspect_target(&paths).unwrap(),
            TargetState::ForeignGenerationExists
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn target_inspection_refuses_orphan_snapshots_but_preserves_valid_classification() {
        let requested_root =
            std::env::temp_dir().join(format!("mnemed-bootstrap-snapshots-{}", Ulid::new()));
        fs::create_dir(&requested_root).unwrap();
        let root = fs::canonicalize(&requested_root).unwrap();
        let operation_id = Ulid::new();
        let paths = GenerationPaths::new(&root, operation_id);
        fs::create_dir_all(paths.mneme.join("snapshots")).unwrap();

        assert_eq!(
            inspect_target(&paths).unwrap(),
            TargetState::OrphanSnapshots
        );
        assert!(paths.mneme.join("snapshots").is_dir());

        fs::write(paths.mneme.join("memory.db"), b"existing database").unwrap();
        assert_eq!(
            inspect_target(&paths).unwrap(),
            TargetState::ActiveDatabaseExists
        );
        fs::remove_file(paths.mneme.join("memory.db")).unwrap();

        fs::create_dir_all(&paths.final_generation).unwrap();
        assert_eq!(
            inspect_target(&paths).unwrap(),
            TargetState::GenerationExists
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn target_inspection_refuses_malformed_snapshot_paths() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "mnemed-bootstrap-malformed-snapshots-{}",
            Ulid::new()
        ));
        let paths = GenerationPaths::new(&root, Ulid::new());
        fs::create_dir_all(&paths.mneme).unwrap();

        fs::write(paths.mneme.join("snapshots"), b"not a directory").unwrap();
        assert!(inspect_target(&paths).is_err());
        fs::remove_file(paths.mneme.join("snapshots")).unwrap();

        symlink(&root, paths.mneme.join("snapshots")).unwrap();
        assert!(inspect_target(&paths).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn abandoned_temp_cleanup_is_operation_scoped_and_refuses_symlinks() {
        use std::os::unix::fs::symlink;

        let requested_root =
            std::env::temp_dir().join(format!("mnemed-bootstrap-temp-gc-{}", Ulid::new()));
        fs::create_dir(&requested_root).unwrap();
        let root = fs::canonicalize(&requested_root).unwrap();
        let operation_id = Ulid::new();
        let paths = GenerationPaths::new(&root, operation_id);
        fs::create_dir_all(&paths.generations).unwrap();

        let first = paths
            .generations
            .join(format!(".bootstrap-{operation_id}-{}", Ulid::new()));
        let second = paths
            .generations
            .join(format!(".bootstrap-{operation_id}-{}", Ulid::new()));
        let foreign_operation = Ulid::new();
        let foreign = paths
            .generations
            .join(format!(".bootstrap-{foreign_operation}-{}", Ulid::new()));
        fs::create_dir(&first).unwrap();
        fs::create_dir(first.join("partial-bodies")).unwrap();
        fs::create_dir(&second).unwrap();
        fs::create_dir(&foreign).unwrap();

        assert_eq!(cleanup_abandoned_generation_temps(&paths).unwrap(), 2);
        assert!(!first.exists());
        assert!(!second.exists());
        assert!(foreign.is_dir());

        let victim = root.join("victim");
        fs::create_dir(&victim).unwrap();
        let linked = paths
            .generations
            .join(format!(".bootstrap-{operation_id}-{}", Ulid::new()));
        symlink(&victim, &linked).unwrap();
        assert!(cleanup_abandoned_generation_temps(&paths).is_err());
        assert!(victim.is_dir());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn private_temp_parser_requires_two_exact_ulids() {
        let operation = Ulid::new();
        let build = Ulid::new();
        assert_eq!(
            private_generation_operation(&format!(".bootstrap-{operation}-{build}")),
            Some(operation.to_string())
        );
        assert_eq!(
            private_activation_operation(&format!(".current-{operation}-{build}")),
            Some(operation.to_string())
        );
        for malformed in [
            format!(".bootstrap-{operation}-not-a-ulid"),
            format!(".bootstrap-{operation}"),
            format!(".current-{operation}-not-a-ulid"),
            format!(".current-{operation}"),
            format!(
                ".bootstrap-{operation}-{}",
                build.to_string().to_lowercase()
            ),
            format!(".bootstrap-{operation}-{}", Ulid::nil()),
            format!(".current-{}-{build}", operation.to_string().to_lowercase()),
        ] {
            assert!(
                private_generation_operation(&malformed).is_none()
                    && private_activation_operation(&malformed).is_none()
            );
        }
        assert!(parse_operation_id(&operation.to_string()).is_ok());
        assert!(parse_operation_id(&operation.to_string().to_lowercase()).is_err());
        assert!(parse_operation_id(&Ulid::nil().to_string()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn abandoned_temp_cleanup_preserves_malformed_entries_and_is_all_or_nothing_on_leases() {
        let requested_root =
            std::env::temp_dir().join(format!("mnemed-bootstrap-temp-preflight-{}", Ulid::new()));
        fs::create_dir(&requested_root).unwrap();
        let root = fs::canonicalize(&requested_root).unwrap();
        let operation_id = Ulid::new();
        let paths = GenerationPaths::new(&root, operation_id);
        fs::create_dir_all(&paths.generations).unwrap();

        let first = paths
            .generations
            .join(format!(".bootstrap-{operation_id}-{}", Ulid::new()));
        let second = paths
            .generations
            .join(format!(".bootstrap-{operation_id}-{}", Ulid::new()));
        let malformed = paths
            .generations
            .join(format!(".bootstrap-{operation_id}-not-a-ulid"));
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        fs::create_dir(&malformed).unwrap();

        assert!(cleanup_abandoned_generation_temps(&paths).is_err());
        assert!(first.is_dir());
        assert!(second.is_dir());
        assert!(malformed.is_dir());

        fs::remove_dir(&malformed).unwrap();
        let held = mneme_store_path::StoreLease::acquire(&second.join("memory.db")).unwrap();
        assert!(cleanup_abandoned_generation_temps(&paths).is_err());
        assert!(first.is_dir());
        assert!(second.is_dir());
        drop(held);

        assert_eq!(cleanup_abandoned_generation_temps(&paths).unwrap(), 2);
        assert!(!first.exists());
        assert!(!second.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn activation_temp_cleanup_is_exact_target_scoped_and_fail_closed() {
        use std::os::unix::fs::symlink;

        let requested_root =
            std::env::temp_dir().join(format!("mnemed-bootstrap-selector-gc-{}", Ulid::new()));
        fs::create_dir(&requested_root).unwrap();
        let root = fs::canonicalize(&requested_root).unwrap();
        let operation_id = Ulid::new();
        let paths = GenerationPaths::new(&root, operation_id);
        fs::create_dir_all(&paths.mneme).unwrap();
        let expected = PathBuf::from("generations").join(operation_id.to_string());
        let valid = paths
            .mneme
            .join(format!(".current-{operation_id}-{}", Ulid::new()));
        symlink(&expected, &valid).unwrap();
        cleanup_abandoned_activation_temps(&paths, operation_id, &expected).unwrap();
        assert!(fs::symlink_metadata(&valid).is_err());

        let malformed = paths
            .mneme
            .join(format!(".current-{operation_id}-not-a-ulid"));
        symlink(&expected, &malformed).unwrap();
        assert!(cleanup_abandoned_activation_temps(&paths, operation_id, &expected).is_err());
        assert!(fs::symlink_metadata(&malformed).is_ok());
        fs::remove_file(&malformed).unwrap();

        let wrong_target = paths
            .mneme
            .join(format!(".current-{operation_id}-{}", Ulid::new()));
        symlink(
            PathBuf::from("generations").join(Ulid::new().to_string()),
            &wrong_target,
        )
        .unwrap();
        assert!(cleanup_abandoned_activation_temps(&paths, operation_id, &expected).is_err());
        assert!(fs::symlink_metadata(&wrong_target).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generation_inventory_rejects_fresh_residue_sidecars_and_extra_bodies() {
        let requested_root =
            std::env::temp_dir().join(format!("mnemed-bootstrap-inventory-{}", Ulid::new()));
        fs::create_dir(&requested_root).unwrap();
        let root = fs::canonicalize(&requested_root).unwrap();
        let generation = root.join("generation");
        let body_id = NodeId(Ulid::new());
        fs::create_dir(&generation).unwrap();
        fs::create_dir(generation.join("memory.bodies")).unwrap();
        fs::create_dir(generation.join("bootstrap")).unwrap();
        fs::write(generation.join("memory.db"), b"database").unwrap();
        fs::write(
            generation.join("memory.bodies").join(body_id.0.to_string()),
            b"body",
        )
        .unwrap();
        fs::write(generation.join("bootstrap/manifest.json"), b"{}").unwrap();
        fs::write(generation.join("bootstrap/receipt.json"), b"{}").unwrap();
        verify_generation_inventory(&generation, [&body_id]).unwrap();

        fs::create_dir(generation.join(".fresh-stage-residue")).unwrap();
        assert!(verify_generation_inventory(&generation, [&body_id]).is_err());
        fs::remove_dir(generation.join(".fresh-stage-residue")).unwrap();

        fs::write(generation.join("memory.db-wal"), b"sidecar").unwrap();
        assert!(verify_generation_inventory(&generation, [&body_id]).is_err());
        fs::remove_file(generation.join("memory.db-wal")).unwrap();

        fs::write(generation.join("memory.bodies/unexpected"), b"body").unwrap();
        assert!(verify_generation_inventory(&generation, [&body_id]).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn target_presence_propagates_metadata_errors_instead_of_treating_them_as_absent() {
        let root = std::env::temp_dir().join(format!("mnemed-bootstrap-metadata-{}", Ulid::new()));
        fs::create_dir(&root).unwrap();
        let not_a_directory = root.join("not-a-directory");
        fs::write(&not_a_directory, b"file").unwrap();

        assert!(path_is_present(&root.join("missing")).is_ok_and(|present| !present));
        assert!(path_is_present(&not_a_directory).is_ok_and(|present| present));
        assert!(path_is_present(&not_a_directory.join("child")).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn target_inspection_and_private_dir_refuse_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("mnemed-bootstrap-dirlink-{}", Ulid::new()));
        let paths = GenerationPaths::new(&root, Ulid::new());
        fs::create_dir_all(&paths.mneme).unwrap();
        let outside = root.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, &paths.generations).unwrap();

        assert!(inspect_target(&paths).is_err());
        assert!(ensure_private_dir(&paths.generations).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn generation_and_activation_publication_never_clobber() {
        let root = std::env::temp_dir().join(format!("mnemed-bootstrap-publish-{}", Ulid::new()));
        fs::create_dir(&root).unwrap();
        let generations = root.join("generations");
        fs::create_dir(&generations).unwrap();
        let first = generations.join("first-temp");
        let final_path = generations.join("generation");
        fs::create_dir(&first).unwrap();
        fs::write(first.join("marker"), b"first").unwrap();
        publish_generation_no_replace(&first, &final_path).unwrap();

        let second = generations.join("second-temp");
        fs::create_dir(&second).unwrap();
        fs::write(second.join("marker"), b"second").unwrap();
        assert!(publish_generation_no_replace(&second, &final_path).is_err());
        assert_eq!(fs::read(final_path.join("marker")).unwrap(), b"first");

        let paths = GenerationPaths {
            mneme: root.clone(),
            generations,
            final_generation: final_path,
            current: root.join("current"),
            lock: root.join("lock"),
            key: root.join("key"),
        };
        let first_id = Ulid::new();
        activate_no_replace(&paths, first_id).unwrap();
        activate_no_replace(&paths, first_id).unwrap();
        assert!(activate_no_replace(&paths, Ulid::new()).is_err());
        assert_eq!(
            read_activation_target(&paths.current).unwrap(),
            PathBuf::from("generations").join(first_id.to_string())
        );

        fs::remove_dir_all(root).unwrap();
    }
}
