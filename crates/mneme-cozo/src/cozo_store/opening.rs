//! Capability-bound open for an existing current conventional store.
//!
//! This boundary joins the raw closed-source admission fence to Mnestic's
//! consuming existing-only constructor, retains either a caller's lease or the
//! complete fresh-publication capability for the database lifetime, and
//! performs only bounded, read-only post-constructor checks. The shared-lease
//! bootstrap/recovery seam is public; schema creation and fresh-publication
//! composition remain private or separately capability-owning.
//!
//! The lease coordinates cooperating Mneme processes; it is not authentication
//! against another same-UID process. Mnestic rechecks the fenced main file while
//! consuming the permit, but the runtime still is not an fd-backed VFS, so a
//! same-authority pathname ABA or later in-place mutation remains outside this
//! local trust boundary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use cozo::{DataValue, DbInstance, ExistingSqliteSnapshotSource, NamedRows};
use mneme_core::managed::DatabaseId;
use mneme_core::ports::{EmbeddingMetadataStore, Error, Result};
use mneme_core::{MAX_INCIDENT_EDGES, MAX_REMOTE_EDGES_PER_SOURCE};
use mneme_store_path::{PublishedFreshStoreV1, StoreLease};
use ulid::Ulid;

use crate::storage_contract::conventional_unmanaged::admission::{
    CurrentOpenPermitV1, ExistingOpenAdmissionReceiptV1, OpenAdmissionV1,
    recognize_concern_open_source_v1, recognize_episode_context_open_source_v2,
    recognize_single_graph_open_source_v1, recognize_touchstones_open_source_v1,
};
use crate::storage_contract::conventional_unmanaged::spec::{
    CONCERN_V1_CATALOG_GENERATION_MARKER, CatalogContract,
    EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER, EPISODE_V1_CATALOG_GENERATION_MARKER,
    PERMANENT_VECTOR_GUARD_VALUE, SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER,
    TOUCHSTONES_V1_CATALOG_GENERATION_MARKER,
};
use crate::storage_contract::conventional_unmanaged::visible_catalog::{
    CatalogResults, validate_catalog_visible_manifest, validate_concern_catalog_visible_manifest,
    validate_episode_catalog_visible_manifest, validate_episode_context_catalog_visible_manifest,
    validate_single_graph_catalog_visible_manifest, validate_touchstones_catalog_visible_manifest,
};

use super::{
    BackendActivity, INCIDENT_EDGE_CAP_META_KEY, LEXICAL_PROJECTION_META_KEY,
    REMOTE_EDGE_SOURCE_CAP_META_KEY, TaggedReadAdmission, VECTOR_PROJECTION_META_KEY, backend,
    install_lock_panic_filter,
};

/// The complete filesystem capability retained behind one persistent store.
///
/// In particular, publication authority is not weakened back into a bare
/// lease: its retained main-file identity remains live for the complete
/// database lifetime, while its pre-open sidecar-absence proof is enforced
/// only until the runtime constructor may legitimately create sidecars.
enum PersistentStoreGuards {
    Existing(Arc<StoreLease>),
    Published(Arc<PublishedFreshStoreV1>),
}

impl PersistentStoreGuards {
    fn require_preopen_guards(&self, requested_path: &Path) -> std::io::Result<()> {
        match self {
            Self::Existing(lease) => lease.require_guards(requested_path),
            Self::Published(published) => {
                require_published_path(published, requested_path)?;
                published.require_guards()
            }
        }
    }

    fn require_runtime_guards(&self, requested_path: &Path) -> std::io::Result<()> {
        match self {
            Self::Existing(lease) => lease.require_guards(requested_path),
            Self::Published(published) => {
                require_published_path(published, requested_path)?;
                published.require_runtime_guards()
            }
        }
    }
}

fn require_published_path(
    published: &PublishedFreshStoreV1,
    requested_path: &Path,
) -> std::io::Result<()> {
    if published.path().as_os_str() != requested_path.as_os_str() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "published fresh-store path no longer exactly matches the retained authority path",
        ));
    }
    Ok(())
}

/// Exact path, filesystem guards, and classifier receipt retained behind every
/// persistent database handle and detached backend job.
pub(super) struct PersistentStoreAuthority {
    path: PathBuf,
    guards: PersistentStoreGuards,
    admission: PersistentStoreAdmission,
    feedback_epoch: Mutex<Option<String>>,
}

enum PersistentStoreAdmission {
    /// Transitional compatibility for the exact old conventional generation.
    /// The raw classifier positively recognized it before the legacy
    /// constructor ran; unknown/torn sources never reach this state.
    FreshCreated,
    Current(ExistingOpenAdmissionReceiptV1),
}

impl PersistentStoreAuthority {
    fn require_preopen_guards(&self, requested_path: &Path, stage: &'static str) -> Result<()> {
        if self.path.as_os_str() != requested_path.as_os_str() {
            return Err(Error::InvalidInput(format!(
                "store_source_changed: admitted database path no longer exactly matches at {stage}"
            )));
        }
        self.guards
            .require_preopen_guards(requested_path)
            .map_err(|error| authority_failure(stage, error))
    }

    fn require_runtime_guards(&self, requested_path: &Path, stage: &'static str) -> Result<()> {
        if self.path.as_os_str() != requested_path.as_os_str() {
            return Err(Error::InvalidInput(format!(
                "store_source_changed: admitted database path no longer exactly matches at {stage}"
            )));
        }
        self.guards
            .require_runtime_guards(requested_path)
            .map_err(|error| authority_failure(stage, error))
    }

    fn has_current_admission_receipt(&self) -> bool {
        matches!(
            &self.admission,
            PersistentStoreAdmission::Current(receipt)
                if receipt.uses_current_classifier_policy()
        )
    }

    pub(super) fn require_live_guards(&self, stage: &'static str) -> Result<()> {
        self.require_runtime_guards(&self.path, stage)
    }

    pub(super) fn lock_feedback_epoch(&self) -> Result<MutexGuard<'_, Option<String>>> {
        self.feedback_epoch
            .lock()
            .map_err(|_| Error::Backend("feedback epoch state lock was poisoned".into()))
    }

    pub(super) fn require_active_feedback_epoch(&self, requested: Option<&str>) -> Result<()> {
        let active = self.lock_feedback_epoch()?;
        let current = active.as_deref().ok_or_else(|| {
            Error::Conflict(
                "feedback epoch is not activated for this persistent store handle".into(),
            )
        })?;
        if requested.is_some_and(|requested| requested != current) {
            return Err(Error::Conflict(
                "feedback commit epoch does not match the active store-handle epoch".into(),
            ));
        }
        Ok(())
    }
}

fn assert_send_sync<T: Send + Sync>() {}
const _: fn() = assert_send_sync::<PersistentStoreAuthority>;

impl super::CozoStore {
    /// Read-only positive capture-generation preflight before any body read or
    /// embedder construction. An acquired matching lease is required; this
    /// never creates or upgrades a database.
    pub fn require_existing_current(path: &Path, lease: &StoreLease) -> Result<()> {
        lease
            .require_guards(path)
            .map_err(|e| authority_failure("before single-graph preflight", e))?;
        let source = ExistingSqliteSnapshotSource::open(path).map_err(|_| {
            current_open_error("touchstones-v1 generation required; use single-graph-upgrade --target-generation touchstones-v1 --backend sqlite --output <ABSENT_PATH>")
        })?;
        let result = recognize_touchstones_open_source_v1(source).map_err(|_| {
            current_open_error("touchstones-v1 generation required; use single-graph-upgrade --target-generation touchstones-v1 --backend sqlite --output <ABSENT_PATH>")
        })?;
        lease
            .require_guards(path)
            .map_err(|e| authority_failure("after single-graph preflight", e))?;
        match result {
            OpenAdmissionV1::Current(_) => Ok(()),
            _ => Err(current_open_error("touchstones-v1 generation required")),
        }
    }
    /// Open or create the persistent conventional store used by a normal
    /// frontend while retaining its exclusive process lease.
    ///
    /// Existing SQLite sources require exact TouchstonesV1 admission before
    /// any runtime constructor. Named predecessors, managed, torn, dirty, and
    /// unknown stores fail closed; use the detached single-graph-upgrade route
    /// through explicitly named predecessor targets. An absent path creates only the
    /// successor schema while retaining the supplied lease.
    ///
    /// JSON snapshot selection remains the frontend's responsibility.
    pub fn open_persistent(
        path: &Path,
        create_dimension: usize,
        lease: Arc<StoreLease>,
    ) -> Result<Self> {
        Self::open_persistent_with_intent(path, create_dimension, lease, true)
    }

    /// Open an existing persistent conventional store without ever creating a
    /// replacement if its pathname disappears before admission.
    ///
    /// This is the recovery/verification counterpart to [`Self::open_persistent`].
    /// It retains the same exact current-versus-recognized-legacy routing but
    /// treats absence as terminal.
    pub fn open_existing_persistent(
        path: &Path,
        create_dimension: usize,
        lease: Arc<StoreLease>,
    ) -> Result<Self> {
        Self::open_persistent_with_intent(path, create_dimension, lease, false)
    }

    fn open_persistent_with_intent(
        path: &Path,
        create_dimension: usize,
        lease: Arc<StoreLease>,
        allow_create: bool,
    ) -> Result<Self> {
        let guards = PersistentStoreGuards::Existing(lease);
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_create => {
                Self::create_fresh_single_graph(path, create_dimension, guards)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(current_open_error(
                "existing_store_admission_failed: existing persistent database is absent",
            )),
            Err(error) => Err(Error::InvalidInput(format!(
                "unsupported_store_path: cannot inspect persistent database {}: {error}",
                path.display()
            ))),
            Ok(metadata) if !metadata.file_type().is_file() => Err(Error::InvalidInput(format!(
                "unsupported_store_path: persistent database {} is not a regular file",
                path.display()
            ))),
            Ok(_) => match classify_existing(path, &guards)? {
                OpenAdmissionV1::Current(current) => {
                    Self::open_admitted_current(path.to_path_buf(), guards, current)
                }
                OpenAdmissionV1::CatalogCodecUpgradeRequired(evidence) => {
                    // Keep this branch structurally tied to positive owning
                    // legacy evidence. No classifier error can fall through to
                    // the compatibility constructor.
                    drop(evidence);
                    Err(current_open_error(
                        "single-graph-upgrade required for predecessor stores",
                    ))
                }
            },
        }
    }

    /// Open one exact current conventional SQLite store from consuming raw
    /// admission evidence while retaining its already-acquired lease.
    ///
    /// This bare-lease entry point remains available for staged existing-store
    /// callers; fresh publication uses the capability-preserving entry point
    /// below.
    pub(crate) fn open_existing_current(path: &Path, lease: StoreLease) -> Result<Self> {
        Self::open_leased_current(path, Arc::new(lease))
    }

    /// Open one exact current conventional SQLite store while retaining a
    /// shared exclusive lease for the complete database lifetime.
    ///
    /// Bootstrap/recovery code which independently established an existing
    /// current-conventional artifact uses this entry point so strictly
    /// sequential verification opens can share the same filesystem authority
    /// without dropping or recursively reacquiring its non-reentrant lock.
    /// The current-generation raw admission fence is consumed before the
    /// runtime constructor; this is not a marker-only or bare-path open. It
    /// neither creates a store, resolves a selector, falls back to a legacy
    /// generation, nor serializes sibling handles sharing the same `Arc`.
    pub fn open_leased_current(path: &Path, lease: Arc<StoreLease>) -> Result<Self> {
        Self::open_current_with_guards(path.to_path_buf(), PersistentStoreGuards::Existing(lease))
    }

    /// Open a just-published current store without weakening its publication
    /// capability into a bare lease.
    ///
    /// The exact target path is derived from the capability. Callers retain an
    /// outer `Arc` across failure; on success the complete capability remains
    /// behind the returned store handle and detached jobs.
    pub(super) fn open_published_current(published: Arc<PublishedFreshStoreV1>) -> Result<Self> {
        let path = published.path().to_path_buf();
        Self::open_current_with_guards(path, PersistentStoreGuards::Published(published))
    }

    // Only detached historical materialization may reopen the frozen predecessor.
    pub(super) fn open_published_single_graph(
        published: Arc<PublishedFreshStoreV1>,
    ) -> Result<Self> {
        let path = published.path().to_path_buf();
        let guards = PersistentStoreGuards::Published(published);
        let current = match classify_existing_for(&path, &guards, CatalogContract::SingleGraphV1)? {
            OpenAdmissionV1::Current(current) => current,
            _ => {
                return Err(current_open_error(
                    "exact single-graph-v1 publication required",
                ));
            }
        };
        Self::open_admitted_current(path, guards, current)
    }

    pub(super) fn open_published_concern(published: Arc<PublishedFreshStoreV1>) -> Result<Self> {
        let path = published.path().to_path_buf();
        let guards = PersistentStoreGuards::Published(published);
        let current = match classify_existing_for(&path, &guards, CatalogContract::ConcernV1)? {
            OpenAdmissionV1::Current(current) => current,
            _ => {
                return Err(current_open_error("exact concern-v1 publication required"));
            }
        };
        Self::open_admitted_current(path, guards, current)
    }

    pub(super) fn open_published_episode_context(
        published: Arc<PublishedFreshStoreV1>,
    ) -> Result<Self> {
        let path = published.path().to_path_buf();
        let guards = PersistentStoreGuards::Published(published);
        let OpenAdmissionV1::Current(current) =
            classify_existing_for(&path, &guards, CatalogContract::EpisodeContextV2)?
        else {
            return Err(current_open_error(
                "exact episode-context-v2 publication required",
            ));
        };
        Self::open_admitted_current(path, guards, current)
    }

    // Test-only explicit historical handle; normal admission never falls back.
    #[cfg(test)]
    pub(super) fn open_leased_concern(path: &Path, lease: Arc<StoreLease>) -> Result<Self> {
        let guards = PersistentStoreGuards::Existing(lease);
        let OpenAdmissionV1::Current(current) =
            classify_existing_for(path, &guards, CatalogContract::ConcernV1)?
        else {
            return Err(current_open_error("exact concern-v1 fixture required"));
        };
        Self::open_admitted_current(path.to_path_buf(), guards, current)
    }

    fn open_current_with_guards(path: PathBuf, guards: PersistentStoreGuards) -> Result<Self> {
        if !path.is_absolute() || path.as_os_str().is_empty() {
            return Err(Error::InvalidInput(
                "unsupported_store_path: existing database path must be nonempty and absolute"
                    .into(),
            ));
        }

        let current = match classify_existing(&path, &guards)? {
            OpenAdmissionV1::Current(current) => current,
            OpenAdmissionV1::CatalogCodecUpgradeRequired(evidence) => {
                drop(evidence);
                return Err(current_open_error(
                    "catalog_codec_upgrade_required: run mnemed --db <PATH> --json upgrade-catalog-codec --operation-id <ULID>",
                ));
            }
        };
        Self::open_admitted_current(path, guards, current)
    }

    fn open_admitted_current(
        path: PathBuf,
        guards: PersistentStoreGuards,
        current: CurrentOpenPermitV1,
    ) -> Result<Self> {
        let admission = current
            .into_existing_open_admission_v1(&path)
            .map_err(|_| {
                current_open_error(
                    "store_source_changed: admitted source path did not exactly match the requested database path",
                )
            })?;
        let (supplied_path, dim, admission_receipt, runtime_permit) =
            admission.into_runtime_parts();
        if supplied_path.as_os_str() != path.as_os_str() {
            return Err(current_open_error(
                "store_source_changed: consumed admission path did not exactly match the requested database path",
            ));
        }

        let authority = Arc::new(PersistentStoreAuthority {
            path: supplied_path,
            guards,
            admission: PersistentStoreAdmission::Current(admission_receipt),
            feedback_epoch: Mutex::new(None),
        });
        if !authority.has_current_admission_receipt() {
            return Err(current_open_error(
                "catalog_generation_torn: current admission classifier receipt did not match runtime policy",
            ));
        }

        install_lock_panic_filter();
        authority.require_preopen_guards(&path, "immediately before runtime constructor")?;
        let constructor = DbInstance::open_existing_sqlite(runtime_permit).map_err(backend);
        let db = combine_with_authority(
            constructor,
            authority.require_runtime_guards(&path, "immediately after runtime constructor"),
        )?;

        let mut store = Self {
            db,
            persistent_authority: Some(authority.clone()),
            dim,
            // Replaced only after the exact persisted value passes validation.
            db_id: Ulid::nil(),
            backend_activity: BackendActivity::default(),
            tagged_read_admission: TaggedReadAdmission::default(),
            #[cfg(test)]
            tagged_read_test_hook: Arc::new(super::TaggedReadTestHook::default()),
            #[cfg(test)]
            query_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            last_maintenance_statements: std::sync::atomic::AtomicUsize::new(0),
        };

        let validation = validate_current_runtime(&store, authority.as_ref());
        let db_id = combine_with_authority(
            validation,
            authority.require_runtime_guards(&path, "after post-constructor validation"),
        )?;
        store.db_id = db_id;
        Ok(store)
    }

    fn create_fresh_single_graph(
        path: &Path,
        create_dimension: usize,
        guards: PersistentStoreGuards,
    ) -> Result<Self> {
        if path.as_os_str().is_empty() {
            return Err(Error::InvalidInput(
                "unsupported_store_path: persistent database path must be nonempty".into(),
            ));
        }
        require_preopen_guards(&guards, path, "before fresh single-graph constructor")?;
        let path_text = path.to_str().ok_or_else(|| {
            Error::InvalidInput("unsupported_store_path: database path is not UTF-8".into())
        })?;
        let opened = super::CozoStore::open(path_text, create_dimension);
        let mut store = combine_with_authority(
            opened,
            require_preopen_guards(&guards, path, "after fresh single-graph constructor"),
        )?;
        store.persistent_authority = Some(Arc::new(PersistentStoreAuthority {
            path: path.to_path_buf(),
            guards,
            admission: PersistentStoreAdmission::FreshCreated,
            feedback_epoch: Mutex::new(None),
        }));
        Ok(store)
    }
}

// Keep every capability entry point type-checked.
const _: fn(&Path, StoreLease) -> Result<super::CozoStore> =
    super::CozoStore::open_existing_current;
const _: fn(&Path, Arc<StoreLease>) -> Result<super::CozoStore> =
    super::CozoStore::open_leased_current;
const _: fn(&Path, usize, Arc<StoreLease>) -> Result<super::CozoStore> =
    super::CozoStore::open_persistent;
const _: fn(&Path, usize, Arc<StoreLease>) -> Result<super::CozoStore> =
    super::CozoStore::open_existing_persistent;
const _: fn(Arc<PublishedFreshStoreV1>) -> Result<super::CozoStore> =
    super::CozoStore::open_published_current;

fn require_preopen_guards(
    guards: &PersistentStoreGuards,
    path: &Path,
    stage: &'static str,
) -> Result<()> {
    guards
        .require_preopen_guards(path)
        .map_err(|error| authority_failure(stage, error))
}

fn classify_existing(path: &Path, guards: &PersistentStoreGuards) -> Result<OpenAdmissionV1> {
    classify_existing_for(path, guards, CatalogContract::TouchstonesV1)
}

fn classify_existing_for(
    path: &Path,
    guards: &PersistentStoreGuards,
    generation: CatalogContract,
) -> Result<OpenAdmissionV1> {
    require_preopen_guards(guards, path, "before raw admission fence")?;
    let raw_admission = ExistingSqliteSnapshotSource::open(path)
        .map_err(|_| {
            current_open_error(
                "existing_store_admission_failed: clean existing SQLite source was not established",
            )
        })
        .and_then(|source| {
            (match generation {
                CatalogContract::SingleGraphV1 => recognize_single_graph_open_source_v1(source),
                CatalogContract::ConcernV1 => recognize_concern_open_source_v1(source),
                CatalogContract::EpisodeContextV2 => recognize_episode_context_open_source_v2(source),
                CatalogContract::TouchstonesV1 => recognize_touchstones_open_source_v1(source),
                _ => return Err(current_open_error("unsupported publication generation")),
            }).map_err(|_| current_open_error(
                "episode_context_upgrade_required_or_invalid: exact current admission failed; ConcernV1 predecessors require single-graph-upgrade --target-generation episode-context-v2 --backend sqlite --output <ABSENT_PATH>; earlier predecessors require the named historical targets first",
            ))
        });
    combine_with_authority(
        raw_admission,
        require_preopen_guards(guards, path, "after raw admission fence"),
    )
}

fn authority_failure(stage: &'static str, error: std::io::Error) -> Error {
    Error::InvalidInput(format!(
        "store_source_changed: filesystem authority guard failed {stage}: {error}"
    ))
}

/// Join an operation to its filesystem-authority postcondition without
/// allowing an operation error to conceal simultaneous loss of path authority.
fn combine_with_authority<T>(operation: Result<T>, authority: Result<()>) -> Result<T> {
    match (operation, authority) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(authority_error)) => Err(authority_error),
        (Err(operation_error), Ok(())) => Err(operation_error),
        (Err(operation_error), Err(authority_error)) => Err(Error::Backend(format!(
            "{operation_error}; additionally, filesystem authority postcondition failed: {authority_error}"
        ))),
    }
}

fn current_open_error(reason: &'static str) -> Error {
    Error::Backend(reason.into())
}

fn validate_current_runtime(
    store: &super::CozoStore,
    authority: &PersistentStoreAuthority,
) -> Result<Ulid> {
    if !authority.has_current_admission_receipt() {
        return Err(validation_error("classifier receipt"));
    }

    validate_visible_catalog(store)?;
    require_meta(store, "dim", &store.dim.to_string(), "vector dimension")?;
    require_meta(
        store,
        INCIDENT_EDGE_CAP_META_KEY,
        &MAX_INCIDENT_EDGES.to_string(),
        "incident edge cap",
    )?;
    require_meta(
        store,
        REMOTE_EDGE_SOURCE_CAP_META_KEY,
        &MAX_REMOTE_EDGES_PER_SOURCE.to_string(),
        "remote edge cap",
    )?;
    require_meta(
        store,
        LEXICAL_PROJECTION_META_KEY,
        "complete",
        "lexical projection marker",
    )?;
    let generation = store.read_meta(VECTOR_PROJECTION_META_KEY)?;
    if !matches!(
        generation.as_deref(),
        Some(SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER)
            | Some(CONCERN_V1_CATALOG_GENERATION_MARKER)
            | Some(EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER)
            | Some(TOUCHSTONES_V1_CATALOG_GENERATION_MARKER)
    ) {
        return Err(validation_error("vector generation marker"));
    }
    let (canonical_key, canonical_value) = if matches!(
        generation.as_deref(),
        Some(EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER)
            | Some(TOUCHSTONES_V1_CATALOG_GENERATION_MARKER)
    ) {
        (
            crate::canonical_node_contract::EPISODE_CONTEXT_META_KEY,
            crate::canonical_node_contract::EPISODE_CONTEXT_META_VALUE,
        )
    } else {
        (
            crate::canonical_node_contract::SINGLE_GRAPH_META_KEY,
            crate::canonical_node_contract::SINGLE_GRAPH_META_VALUE,
        )
    };
    require_meta(
        store,
        canonical_key,
        canonical_value,
        "canonical node marker",
    )?;
    require_meta(
        store,
        crate::tag_projection::META_KEY,
        crate::tag_projection::META_VALUE,
        "tag projection marker",
    )?;

    validate_permanent_guard(store)?;
    validate_retired_legacy_guard(store)?;
    validate_optional_embedding_fingerprint(store)?;

    let raw_db_id = store
        .read_meta("db_id")?
        .ok_or_else(|| validation_error("database id"))?;
    let db_id = Ulid::from_string(&raw_db_id).map_err(|_| validation_error("database id"))?;
    if db_id.to_string() != raw_db_id {
        return Err(validation_error("database id"));
    }
    DatabaseId::new(db_id)
        .map(DatabaseId::get)
        .map_err(|_| validation_error("database id"))
}

fn validate_visible_catalog(store: &super::CozoStore) -> Result<()> {
    let contract = if store.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref()
        == Some(TOUCHSTONES_V1_CATALOG_GENERATION_MARKER)
    {
        CatalogContract::TouchstonesV1
    } else if store.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref()
        == Some(EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER)
    {
        CatalogContract::EpisodeContextV2
    } else if store.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref()
        == Some(CONCERN_V1_CATALOG_GENERATION_MARKER)
    {
        CatalogContract::ConcernV1
    } else if store.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref()
        == Some(SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER)
    {
        CatalogContract::SingleGraphV1
    } else if store.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref()
        == Some(EPISODE_V1_CATALOG_GENERATION_MARKER)
    {
        CatalogContract::EpisodeV1
    } else {
        CatalogContract::Predecessor
    };
    let relations = store.run("::relations", BTreeMap::new(), false)?;
    let mut columns = BTreeMap::new();
    let mut indices = BTreeMap::new();
    for relation in contract.relations() {
        columns.insert(
            relation.name.to_owned(),
            store.run(
                &format!("::columns {}", relation.name),
                BTreeMap::new(),
                false,
            )?,
        );
        indices.insert(
            relation.name.to_owned(),
            store.run(
                &format!("::indices {}", relation.name),
                BTreeMap::new(),
                false,
            )?,
        );
    }
    let mut trigger_catalogs = BTreeMap::new();
    for base in contract.bases() {
        trigger_catalogs.insert(
            base.relation.name.to_owned(),
            store.run(
                &format!("::show_triggers {}", base.relation.name),
                BTreeMap::new(),
                false,
            )?,
        );
    }
    let validate = match contract {
        CatalogContract::Predecessor => validate_catalog_visible_manifest,
        CatalogContract::EpisodeV1 => validate_episode_catalog_visible_manifest,
        CatalogContract::SingleGraphV1 => validate_single_graph_catalog_visible_manifest,
        CatalogContract::ConcernV1 => validate_concern_catalog_visible_manifest,
        CatalogContract::EpisodeContextV2 => validate_episode_context_catalog_visible_manifest,
        CatalogContract::TouchstonesV1 => validate_touchstones_catalog_visible_manifest,
    };
    let manifest = validate(CatalogResults {
        relations: &relations,
        columns: &columns,
        indices: &indices,
        trigger_catalogs: &trigger_catalogs,
    })
    .map_err(|_| validation_error("catalog-visible relation, index, or access shape"))?;
    let catalog_dim = usize::try_from(manifest.vector_dimension)
        .map_err(|_| validation_error("catalog vector dimension"))?;
    if catalog_dim != store.dim {
        return Err(validation_error("catalog vector dimension"));
    }
    Ok(())
}

fn require_meta(
    store: &super::CozoStore,
    key: &str,
    expected: &str,
    label: &'static str,
) -> Result<()> {
    if store.read_meta(key)?.as_deref() != Some(expected) {
        return Err(validation_error(label));
    }
    Ok(())
}

fn validate_permanent_guard(store: &super::CozoStore) -> Result<()> {
    let rows = store.run(
        "?[fence, generation] := *mneme_reembed_shadow_node_vec{fence, generation} :limit 2",
        BTreeMap::new(),
        false,
    )?;
    if rows.next.is_some()
        || rows.rows.len() != 1
        || !exact_string_pair(
            &rows,
            crate::vector_projection::GUARD_KEY,
            PERMANENT_VECTOR_GUARD_VALUE,
        )
    {
        return Err(validation_error("permanent vector guard"));
    }
    Ok(())
}

fn exact_string_pair(rows: &NamedRows, expected_left: &str, expected_right: &str) -> bool {
    matches!(
        rows.rows.as_slice(),
        [row]
            if matches!(row.as_slice(), [DataValue::Str(left), DataValue::Str(right)]
                if left.as_str() == expected_left && right.as_str() == expected_right)
    )
}

fn validate_retired_legacy_guard(store: &super::CozoStore) -> Result<()> {
    let rows = store.run("?[id] := *node_tag{id} :limit 1", BTreeMap::new(), false)?;
    if rows.next.is_some() || !rows.rows.is_empty() {
        return Err(validation_error("retired legacy tag guard"));
    }
    Ok(())
}

fn validate_optional_embedding_fingerprint(store: &super::CozoStore) -> Result<()> {
    let fingerprint = store
        .embedding_fingerprint()
        .map_err(|_| validation_error("optional embedding fingerprint JSON"))?;
    let Some(fingerprint) = fingerprint else {
        return Ok(());
    };
    fingerprint
        .validate()
        .map_err(|_| validation_error("optional embedding fingerprint value"))?;
    if fingerprint.dimension != store.dim {
        return Err(validation_error("optional embedding fingerprint dimension"));
    }
    Ok(())
}

fn validation_error(label: &'static str) -> Error {
    Error::Backend(format!(
        "catalog_generation_torn: post-constructor current validation rejected {label}"
    ))
}

#[cfg(test)]
mod tests;
