//! Capability-owning materialization of one fresh current conventional store.
//!
//! Native bootstrap, fresh capture initialization, and detached episodic-generation
//! construction are the production routes through this boundary. It joins the private schema kernel,
//! fresh-file publication authority, raw closed-source recognition, and the
//! consuming published-current open. No intermediate pathname, filesystem
//! capability, schema constructor, or live `CozoStore` escapes this module.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use cozo::{DbInstance, ExistingSqliteSnapshotSource, ManagedSnapshotPolicy};
use mneme_core::managed::DatabaseId;
use mneme_core::ports::{Error, Result};
use mneme_store_path::{
    FreshStorePublishErrorV1, FreshStoreStageSpecV1, PublishedFreshStoreV1, StoreLease,
};
use sha2::{Digest, Sha256};
use ulid::Ulid;

use crate::storage_contract::conventional_unmanaged::admission::{
    CatalogCodecClassification, CatalogSealGenerationV1, close_concern_source_bound_seal_v1,
    close_episode_context_source_bound_seal_v2, close_single_graph_source_bound_seal_v1,
    close_touchstones_source_bound_seal_v1,
};

use super::{BackendActivity, CozoStore, TaggedReadAdmission};

const FRESH_CURRENT_POLICY_BINDING_DOMAIN_V1: &[u8] =
    b"mneme.cozo.single-graph-materialization.policy-binding.v1\0";
const CONCERN_POLICY_BINDING_DOMAIN_V1: &[u8] =
    b"mneme.cozo.concern-materialization.policy-binding.v1\0";
use crate::storage_contract::conventional_unmanaged::spec::{
    CONCERN_V1_CATALOG_GENERATION_MARKER, CatalogContract,
    EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER, SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER,
    TOUCHSTONES_V1_CATALOG_GENERATION_MARKER,
};

/// Result of a fresh-current materialization attempt.
pub type FreshCurrentMaterializationResultV1<T> =
    std::result::Result<T, FreshCurrentMaterializationErrorV1>;

/// Publication truth for the fresh target file after a materialization failure.
///
/// This describes only the materializer's target (for example, an inner
/// `memory.db` inside a private bootstrap generation). It says nothing about
/// publication of any enclosing directory or generation. In particular,
/// `PublicationUncertain` is not evidence that the target file is absent, while
/// `Published` proves only that this target crossed its inner publication
/// boundary even if its open/postflight failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FreshCurrentTargetPublicationStateV1 {
    /// The materializer failed before the fresh-store publication boundary.
    NotPublished,
    /// The filesystem publication layer could not prove whether its rename
    /// crossed, so recovery must inspect its retained evidence.
    PublicationUncertain,
    /// The filesystem layer proved durable publication, or the database was
    /// opened after publication and then failed later.
    Published,
}

/// The materialization phase whose failure produced an error.
///
/// This is deliberately separate from [`FreshCurrentTargetPublicationStateV1`]:
/// a filesystem publication error can be definitely unpublished, uncertain,
/// or definitely published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FreshCurrentMaterializationFailurePhaseV1 {
    /// Schema construction, import, verification, or fresh-stage sealing
    /// failed before publication.
    PrePublication,
    /// The fresh-store filesystem publication protocol itself failed.
    Publication,
    /// Publication completed, but opening the resulting current store failed.
    PublishedOpen,
    /// Publication and current-store open completed, but final verification or
    /// quiescing failed.
    PublishedPostflight,
}

/// Exact truth boundary retained when fresh-current materialization does not
/// return a completely closed, sidecar-free store.
///
/// Publication and post-publication variants deliberately retain the owning
/// capability.  A caller cannot mistake an error after the rename for proof
/// that the target remained absent, nor release the lease before deciding how
/// its enclosing operation will recover.
#[must_use = "a failed fresh-current materialization may retain authority that must be deliberately relinquished before its enclosing staging directory is discarded"]
pub struct FreshCurrentMaterializationErrorV1 {
    inner: FreshCurrentMaterializationErrorInnerV1,
}

enum FreshCurrentMaterializationErrorInnerV1 {
    NotPublished {
        source: Error,
    },
    Publication {
        source: FreshStorePublishErrorV1,
    },
    PublishedOpen {
        source: Error,
        published: Arc<PublishedFreshStoreV1>,
    },
    PublishedPostflight {
        source: Error,
        current: Box<CozoStore>,
    },
}

impl FreshCurrentMaterializationErrorV1 {
    fn not_published(source: Error) -> Self {
        Self {
            inner: FreshCurrentMaterializationErrorInnerV1::NotPublished { source },
        }
    }

    fn publication(source: FreshStorePublishErrorV1) -> Self {
        Self {
            inner: FreshCurrentMaterializationErrorInnerV1::Publication { source },
        }
    }

    fn published_open(source: Error, published: Arc<PublishedFreshStoreV1>) -> Self {
        Self {
            inner: FreshCurrentMaterializationErrorInnerV1::PublishedOpen { source, published },
        }
    }

    fn published_postflight(source: Error, current: CozoStore) -> Self {
        Self {
            inner: FreshCurrentMaterializationErrorInnerV1::PublishedPostflight {
                source,
                current: Box::new(current),
            },
        }
    }

    /// Exact fresh-target publication truth for this failure.
    ///
    /// This does not describe publication of any enclosing directory.
    pub fn target_publication_state(&self) -> FreshCurrentTargetPublicationStateV1 {
        match &self.inner {
            FreshCurrentMaterializationErrorInnerV1::NotPublished { .. } => {
                FreshCurrentTargetPublicationStateV1::NotPublished
            }
            FreshCurrentMaterializationErrorInnerV1::Publication { source } => match source {
                FreshStorePublishErrorV1::NotPublished { .. } => {
                    FreshCurrentTargetPublicationStateV1::NotPublished
                }
                FreshStorePublishErrorV1::PublicationNeedsRecovery { .. } => {
                    FreshCurrentTargetPublicationStateV1::PublicationUncertain
                }
                FreshStorePublishErrorV1::PublishedWithResidue { .. }
                | FreshStorePublishErrorV1::PublishedGuardNeedsRecovery { .. } => {
                    FreshCurrentTargetPublicationStateV1::Published
                }
            },
            FreshCurrentMaterializationErrorInnerV1::PublishedOpen { .. }
            | FreshCurrentMaterializationErrorInnerV1::PublishedPostflight { .. } => {
                FreshCurrentTargetPublicationStateV1::Published
            }
        }
    }

    /// Phase which reported the failure, for diagnostics and recovery logs.
    pub fn failure_phase(&self) -> FreshCurrentMaterializationFailurePhaseV1 {
        match &self.inner {
            FreshCurrentMaterializationErrorInnerV1::NotPublished { .. } => {
                FreshCurrentMaterializationFailurePhaseV1::PrePublication
            }
            FreshCurrentMaterializationErrorInnerV1::Publication { .. } => {
                FreshCurrentMaterializationFailurePhaseV1::Publication
            }
            FreshCurrentMaterializationErrorInnerV1::PublishedOpen { .. } => {
                FreshCurrentMaterializationFailurePhaseV1::PublishedOpen
            }
            FreshCurrentMaterializationErrorInnerV1::PublishedPostflight { .. } => {
                FreshCurrentMaterializationFailurePhaseV1::PublishedPostflight
            }
        }
    }

    /// Whether this error owns a live filesystem or database capability.
    ///
    /// Publication-protocol failures retain the filesystem capability even
    /// when the target is definitely absent; the capability is the evidence
    /// needed for a normal retry or recovery. `false` means only that this
    /// returned error retains no live authority; an earlier stage may still
    /// have acquired and released one and left operation-scoped residue for its
    /// enclosing recovery policy.
    pub fn retains_authority(&self) -> bool {
        !matches!(
            self.inner,
            FreshCurrentMaterializationErrorInnerV1::NotPublished { .. }
        )
    }

    /// Close and relinquish every retained inner capability before immediately
    /// discarding the caller's unpublished, private enclosing directory.
    ///
    /// This consumes the error, drops any SQLite handle and fresh-store lease,
    /// and returns the target-file publication truth the caller may preserve in
    /// diagnostics. It intentionally does not remove paths.
    ///
    /// # Preconditions
    ///
    /// Call this only after independently proving that the enclosing directory
    /// is private and has never crossed its own publication boundary, and only
    /// when that directory will be discarded immediately. Otherwise retain the
    /// error and its capability for explicit target recovery.
    pub fn relinquish_for_unpublished_outer_discard(self) -> FreshCurrentTargetPublicationStateV1 {
        let state = self.target_publication_state();
        drop(self);
        state
    }
}

impl fmt::Debug for FreshCurrentMaterializationErrorV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            FreshCurrentMaterializationErrorInnerV1::NotPublished { source } => formatter
                .debug_struct("NotPublished")
                .field("source", source)
                .finish(),
            FreshCurrentMaterializationErrorInnerV1::Publication { source } => formatter
                .debug_struct("Publication")
                .field("source", source)
                .finish(),
            FreshCurrentMaterializationErrorInnerV1::PublishedOpen { source, published } => {
                formatter
                    .debug_struct("PublishedOpen")
                    .field("source", source)
                    .field("path", &published.path())
                    .finish_non_exhaustive()
            }
            FreshCurrentMaterializationErrorInnerV1::PublishedPostflight { source, current } => {
                formatter
                    .debug_struct("PublishedPostflight")
                    .field("source", source)
                    .field("database_id", &current.db_id())
                    .finish_non_exhaustive()
            }
        }
    }
}

impl fmt::Display for FreshCurrentMaterializationErrorV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            FreshCurrentMaterializationErrorInnerV1::NotPublished { source } => {
                write!(formatter, "fresh current store was not published: {source}")
            }
            FreshCurrentMaterializationErrorInnerV1::Publication { source } => {
                write!(formatter, "{source}")
            }
            FreshCurrentMaterializationErrorInnerV1::PublishedOpen {
                source, published, ..
            } => write!(
                formatter,
                "fresh current store was published at {} but current-only open failed: {source}",
                published.path().display()
            ),
            FreshCurrentMaterializationErrorInnerV1::PublishedPostflight { source, .. } => write!(
                formatter,
                "fresh current store was published and opened but postflight failed: {source}"
            ),
        }
    }
}

impl std::error::Error for FreshCurrentMaterializationErrorV1 {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.inner {
            FreshCurrentMaterializationErrorInnerV1::NotPublished { source }
            | FreshCurrentMaterializationErrorInnerV1::PublishedOpen { source, .. }
            | FreshCurrentMaterializationErrorInnerV1::PublishedPostflight { source, .. } => {
                Some(source)
            }
            FreshCurrentMaterializationErrorInnerV1::Publication { source } => Some(source),
        }
    }
}

fn not_published(
    phase: &'static str,
    error: impl fmt::Display,
) -> FreshCurrentMaterializationErrorV1 {
    FreshCurrentMaterializationErrorV1::not_published(contextual(phase, error))
}

fn published_postflight(
    phase: &'static str,
    error: impl fmt::Display,
    current: CozoStore,
) -> FreshCurrentMaterializationErrorV1 {
    FreshCurrentMaterializationErrorV1::published_postflight(contextual(phase, error), current)
}

impl CozoStore {
    pub async fn materialize_single_graph(
        target: &Path,
        operation_id: Ulid,
        source: &crate::MemStore,
    ) -> FreshCurrentMaterializationResultV1<()> {
        materialize_fresh_generation_with_observers(
            target,
            operation_id,
            source,
            CatalogContract::SingleGraphV1,
            |_| {},
            |_, _| Ok(()),
        )
        .await
    }

    pub async fn materialize_concern(
        target: &Path,
        operation_id: Ulid,
        source: &crate::MemStore,
    ) -> FreshCurrentMaterializationResultV1<()> {
        materialize_fresh_generation_with_observers(
            target,
            operation_id,
            source,
            CatalogContract::ConcernV1,
            |_| {},
            |_, _| Ok(()),
        )
        .await
    }

    /// Explicit frozen historical target for the episode-context upgrade route.
    pub async fn materialize_episode_context(
        target: &Path,
        operation_id: Ulid,
        source: &crate::MemStore,
    ) -> FreshCurrentMaterializationResultV1<()> {
        materialize_fresh_generation_with_observers(
            target,
            operation_id,
            source,
            CatalogContract::EpisodeContextV2,
            |_| {},
            |_, _| Ok(()),
        )
        .await
    }

    /// Materialize one exact migratable in-memory snapshot as a fresh current
    /// persistent store, closing every database handle before returning.
    /// Volatile feedback retry proofs follow the existing import contract and
    /// deliberately do not cross the host-authority boundary.
    ///
    /// `target` must be an absent, nonempty absolute path whose parent satisfies
    /// the fresh-store publication policy. Native bootstrap routes this only
    /// while the target is nested below its own unpublished private generation.
    /// Errors before publication are explicit, while every publication or
    /// postflight error retains the owning capability needed by an enclosing
    /// recovery operation.
    pub async fn materialize_fresh_current(
        target: &Path,
        operation_id: Ulid,
        source: &crate::MemStore,
    ) -> FreshCurrentMaterializationResultV1<()> {
        materialize_fresh_current_with_observers(
            target,
            operation_id,
            source,
            |_| {},
            |_, _| Ok(()),
        )
        .await
    }
}

// Keep the public high-level boundary compile-reachable.
const _: () = {
    let _ = CozoStore::materialize_fresh_current;
    let _ = CozoStore::materialize_single_graph;
};

async fn materialize_fresh_current_with_observers<BeforePublish, WhileCurrent>(
    target: &Path,
    operation_id: Ulid,
    source: &crate::MemStore,
    before_publish: BeforePublish,
    while_current: WhileCurrent,
) -> FreshCurrentMaterializationResultV1<()>
where
    BeforePublish: FnOnce(&Path),
    WhileCurrent: FnOnce(&Path, &CozoStore) -> Result<()>,
{
    materialize_fresh_generation_with_observers(
        target,
        operation_id,
        source,
        CatalogContract::TouchstonesV1,
        before_publish,
        while_current,
    )
    .await
}

// Local, one-shot qualification hooks; absent from production artifacts.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConcernStageCut {
    AfterImportBeforeVerification,
    TamperCatalogAfterVerification,
}
#[cfg(test)]
std::thread_local! {
    static CONCERN_STAGE_CUT: std::cell::Cell<Option<ConcernStageCut>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
fn take_concern_stage_cut(cut: ConcernStageCut) -> bool {
    CONCERN_STAGE_CUT.with(|slot| {
        if slot.get() == Some(cut) {
            slot.set(None);
            true
        } else {
            false
        }
    })
}

async fn materialize_fresh_generation_with_observers<BeforePublish, WhileCurrent>(
    target: &Path,
    operation_id: Ulid,
    source: &crate::MemStore,
    generation: CatalogContract,
    before_publish: BeforePublish,
    while_current: WhileCurrent,
) -> FreshCurrentMaterializationResultV1<()>
where
    BeforePublish: FnOnce(&Path),
    WhileCurrent: FnOnce(&Path, &CozoStore) -> Result<()>,
{
    if target.as_os_str().is_empty() || !target.is_absolute() {
        return Err(not_published(
            "fresh_current_preflight",
            Error::InvalidInput("target must be a nonempty absolute path".into()),
        ));
    }
    if operation_id == Ulid::nil() {
        return Err(not_published(
            "fresh_current_preflight",
            Error::InvalidInput("operation id must not be nil".into()),
        ));
    }

    // Freeze the source exactly once.  Importing from a detached reconstruction
    // prevents a concurrently mutated MemStore from making import and
    // verification observe different snapshots.
    let expected = source.export();
    if generation == CatalogContract::SingleGraphV1 && !expected.concerns.is_empty() {
        return Err(not_published(
            "single_graph_preflight",
            "cannot discard concerns into historical generation",
        ));
    }
    if !matches!(
        generation,
        CatalogContract::EpisodeContextV2 | CatalogContract::TouchstonesV1
    ) {
        for node in &expected.nodes {
            let raw = serde_json::to_value(node)
                .map_err(|error| not_published("historical_generation_preflight", error))?;
            crate::validate_pre_context_node_value(&raw)
                .map_err(|error| not_published("historical_generation_preflight", error))?;
        }
    }
    let database_id = DatabaseId::new(expected.db_id).map_err(|error| {
        not_published(
            "fresh_current_preflight",
            Error::InvalidInput(format!("source database id is invalid: {error}")),
        )
    })?;
    let stable_source = crate::MemStore::from_export(expected.clone())
        .map_err(|error| not_published("fresh_current_preflight_exact_export", error))?;
    let policy_binding = fresh_generation_policy_binding(expected.dim, expected.db_id, generation);

    let lease = StoreLease::acquire(target)
        .map_err(|error| not_published("fresh_current_lease_acquire", error))?;
    let stage = lease
        .begin_fresh_store_v1(
            target,
            FreshStoreStageSpecV1 {
                operation_id,
                policy_binding,
            },
        )
        .map_err(|error| not_published("fresh_current_stage_begin_not_published", error))?;

    // The filesystem layer pre-created this exact private inode.  The stage
    // capability retains its descriptor and later refuses sealing if the name
    // no longer identifies it.
    super::install_lock_panic_filter();
    let database = DbInstance::new("sqlite", stage.database_path(), "")
        .map_err(|error| not_published("fresh_current_stage_open_not_published", error))?;
    (match generation {
        CatalogContract::SingleGraphV1 => {
            super::creation::stage_single_graph_schema(&database, expected.dim, database_id)
        }
        CatalogContract::ConcernV1 => {
            super::creation::stage_concern_schema(&database, expected.dim, database_id)
        }
        CatalogContract::TouchstonesV1 => {
            super::creation::stage_touchstones_schema(&database, expected.dim, database_id)
        }
        CatalogContract::EpisodeContextV2 => {
            super::creation::stage_episode_context_schema(&database, expected.dim, database_id)
        }
        _ => Err(Error::InvalidInput("unsupported fresh generation".into())),
    })
    .map_err(|error| not_published("fresh_current_schema_not_published", error))?;

    let mut staged_store = store_over_staged_database(database, expected.dim, expected.db_id);
    staged_store
        .import_mem(&stable_source)
        .await
        .map_err(|error| not_published("fresh_current_import_not_published", error))?;
    #[cfg(test)]
    if take_concern_stage_cut(ConcernStageCut::AfterImportBeforeVerification) {
        let imported = staged_store
            .export()
            .await
            .expect("test imported concern rows");
        assert_eq!(imported.concerns.len(), 1);
        assert!(imported.concerns[0].finding().is_some());
        return Err(not_published(
            "concern_after_import_before_verification",
            "injected concern stage failure",
        ));
    }
    staged_store
        .verify_import(&expected)
        .await
        .map_err(|error| not_published("fresh_current_verify_not_published", error))?;
    #[cfg(test)]
    if take_concern_stage_cut(ConcernStageCut::TamperCatalogAfterVerification) {
        staged_store
            .run(
                "{:create unexpected_concern_catalog {id: String}}",
                std::collections::BTreeMap::new(),
                true,
            )
            .expect("test alter staged catalog after complete content verification");
    }
    staged_store
        .prepare_for_file_move()
        .map_err(|error| not_published("fresh_current_checkpoint_not_published", error))?;
    drop(staged_store);

    require_raw_closed_generation_for(stage.database_path(), expected.dim, generation)
        .map_err(|error| not_published("fresh_current_raw_closed_not_published", error))?;
    before_publish(target);

    let prepared = stage
        .seal_closed_file()
        .map_err(|error| not_published("fresh_current_seal_not_published", error))?;
    let published = prepared
        .publish()
        .map_err(FreshCurrentMaterializationErrorV1::publication)?;
    let published = Arc::new(published);
    let current = match match generation {
        CatalogContract::SingleGraphV1 => {
            CozoStore::open_published_single_graph(Arc::clone(&published))
        }
        CatalogContract::ConcernV1 => CozoStore::open_published_concern(Arc::clone(&published)),
        CatalogContract::EpisodeContextV2 => {
            CozoStore::open_published_episode_context(Arc::clone(&published))
        }
        CatalogContract::TouchstonesV1 => CozoStore::open_published_current(Arc::clone(&published)),
        _ => Err(Error::InvalidInput("unsupported fresh generation".into())),
    } {
        Ok(current) => current,
        Err(source) => {
            return Err(FreshCurrentMaterializationErrorV1::published_open(
                contextual("fresh_current_post_publish_open", source),
                published,
            ));
        }
    };
    drop(published);
    {
        let marker = match generation {
            CatalogContract::SingleGraphV1 => SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER,
            CatalogContract::ConcernV1 => CONCERN_V1_CATALOG_GENERATION_MARKER,
            CatalogContract::EpisodeContextV2 => EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER,
            CatalogContract::TouchstonesV1 => TOUCHSTONES_V1_CATALOG_GENERATION_MARKER,
            _ => unreachable!("fresh generation checked before staging"),
        };
        if current
            .read_meta(super::VECTOR_PROJECTION_META_KEY)
            .ok()
            .flatten()
            .as_deref()
            != Some(marker)
        {
            return Err(published_postflight(
                "fresh_generation_post_publish_generation",
                Error::Backend("published target generation marker changed".into()),
                current,
            ));
        }
    }
    if let Err(error) = current.verify_import(&expected).await {
        return Err(published_postflight(
            "fresh_current_post_publish_verify",
            error,
            current,
        ));
    }

    if let Err(error) = while_current(target, &current) {
        return Err(published_postflight(
            "fresh_current_post_publish_observer",
            error,
            current,
        ));
    }

    if let Err(error) = current.prepare_for_file_move() {
        return Err(published_postflight(
            "fresh_current_post_publish_checkpoint",
            error,
            current,
        ));
    }
    // `prepare_for_file_move` synchronously checkpoints and detaches SQLite and
    // prevents new backend work.  The still-live publication capability keeps
    // the target lease and main descriptor guarded while this first absence
    // proof runs; retaining that plain main fd cannot recreate a sidecar.
    if let Err(error) = require_no_sqlite_sidecars(target, "fresh_current_post_publish_checkpoint")
    {
        return Err(published_postflight(
            "fresh_current_post_publish_sidecars",
            error,
            current,
        ));
    }
    drop(current);
    Ok(())
}

pub(super) fn store_over_staged_database(
    database: DbInstance,
    dimension: usize,
    database_id: Ulid,
) -> CozoStore {
    CozoStore {
        db: database,
        persistent_authority: None,
        dim: dimension,
        db_id: database_id,
        backend_activity: BackendActivity::default(),
        tagged_read_admission: TaggedReadAdmission::default(),
        #[cfg(test)]
        tagged_read_test_hook: std::sync::Arc::new(super::TaggedReadTestHook::default()),
        #[cfg(test)]
        query_count: std::sync::atomic::AtomicUsize::new(0),
        #[cfg(test)]
        last_maintenance_statements: std::sync::atomic::AtomicUsize::new(0),
    }
}

#[cfg(test)]
fn fresh_current_policy_binding(dimension: usize, database_id: Ulid) -> [u8; 32] {
    fresh_generation_policy_binding(dimension, database_id, CatalogContract::TouchstonesV1)
}

fn fresh_generation_policy_binding(
    dimension: usize,
    database_id: Ulid,
    generation: CatalogContract,
) -> [u8; 32] {
    let mut hash = Sha256::new();
    if generation == CatalogContract::TouchstonesV1 {
        hash.update(b"mneme.cozo.touchstones-materialization.policy-binding.v1\0");
        hash.update(TOUCHSTONES_V1_CATALOG_GENERATION_MARKER.as_bytes());
    } else if generation == CatalogContract::EpisodeContextV2 {
        hash.update(b"mneme.cozo.episode-context-materialization.policy-binding.v2\0");
        hash.update(EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER.as_bytes());
        hash.update(crate::canonical_node_contract::EPISODE_CONTEXT_META_KEY.as_bytes());
        hash.update(crate::canonical_node_contract::EPISODE_CONTEXT_META_VALUE.as_bytes());
    } else if generation == CatalogContract::ConcernV1 {
        hash.update(CONCERN_POLICY_BINDING_DOMAIN_V1);
        hash.update(CONCERN_V1_CATALOG_GENERATION_MARKER.as_bytes());
    } else {
        // Frozen predecessor recovery ABI: do not retrofit a new discriminator.
        hash.update(FRESH_CURRENT_POLICY_BINDING_DOMAIN_V1);
    }
    hash.update((dimension as u64).to_be_bytes());
    hash.update(database_id.to_bytes());
    let mut binding: [u8; 32] = hash.finalize().into();

    // SHA-256's all-zero output is fantastically unlikely but the filesystem
    // contract requires a logically impossible value, not a probabilistic one.
    if binding.iter().all(|byte| *byte == 0) {
        binding[31] = 1;
    }
    binding
}

#[cfg(test)]
fn require_raw_closed_generation(path: &Path, expected_dimension: usize) -> Result<()> {
    require_raw_closed_generation_for(path, expected_dimension, CatalogContract::TouchstonesV1)
}

fn require_raw_closed_generation_for(
    path: &Path,
    expected_dimension: usize,
    generation: CatalogContract,
) -> Result<()> {
    let reader_policy = if generation == CatalogContract::TouchstonesV1 {
        cozo::ManagedSnapshotPolicy::TouchstonesV1
    } else if generation == CatalogContract::SingleGraphV1 {
        ManagedSnapshotPolicy::SingleGraphV1
    } else {
        ManagedSnapshotPolicy::ConcernV1
    };
    let reader = ExistingSqliteSnapshotSource::open(path)
        .and_then(|source| source.into_managed_reader(reader_policy))
        .map_err(|error| contextual("fresh_current_raw_classification_not_published", error))?;
    let seal = (match generation {
        CatalogContract::SingleGraphV1 => close_single_graph_source_bound_seal_v1(reader),
        CatalogContract::ConcernV1 => close_concern_source_bound_seal_v1(reader),
        CatalogContract::EpisodeContextV2 => close_episode_context_source_bound_seal_v2(reader),
        CatalogContract::TouchstonesV1 => close_touchstones_source_bound_seal_v1(reader),
        _ => return Err(Error::InvalidInput("unsupported fresh generation".into())),
    })
    .map_err(|error| contextual("fresh_current_raw_classification_not_published", error))?;

    let dimension_matches =
        u64::try_from(expected_dimension).is_ok_and(|expected| seal.vector_dimension() == expected);
    if seal.generation()
        != match generation {
            CatalogContract::SingleGraphV1 => CatalogSealGenerationV1::SingleGraphV1,
            CatalogContract::ConcernV1 => CatalogSealGenerationV1::ConcernV1,
            CatalogContract::EpisodeContextV2 => CatalogSealGenerationV1::EpisodeContextV2,
            CatalogContract::TouchstonesV1 => CatalogSealGenerationV1::TouchstonesV1,
            _ => return Err(Error::InvalidInput("unsupported fresh generation".into())),
        }
        || seal.codec_classification() != CatalogCodecClassification::AllStructMapV1
        || !dimension_matches
    {
        return Err(Error::Backend(format!(
            "fresh_current_raw_classification_not_published: closed stage was not exact requested struct-map generation at dimension {expected_dimension}"
        )));
    }
    drop(seal);
    Ok(())
}

fn require_no_sqlite_sidecars(target: &Path, phase: &'static str) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = target.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = std::path::PathBuf::from(sidecar);
        match std::fs::symlink_metadata(&sidecar) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(Error::Backend(format!(
                    "{phase}: SQLite sidecar remained after checkpoint and close: {}",
                    sidecar.display()
                )));
            }
            Err(error) => {
                return Err(Error::Backend(format!(
                    "{phase}: could not prove SQLite sidecar absence at {}: {error}",
                    sidecar.display()
                )));
            }
        }
    }
    Ok(())
}

fn contextual(phase: &'static str, error: impl std::fmt::Display) -> Error {
    Error::Backend(format!("{phase}: {error}"))
}

#[cfg(test)]
mod tests {
    include!("fresh_current/episode_tests.rs");

    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use mneme_core::ports::{
        EmbeddingMetadataStore, FeedbackCommit, FeedbackCommitOutcome, FeedbackIdempotency,
        FeedbackRetryScope, FullMergeCommit, FullMergeCommitOutcome, GraphStore, SupersedeCommit,
        SupersedeCommitOutcome, VectorIndex,
    };
    use mneme_core::{
        BodyRef, BodySpan, CaptureSource, Edge, EdgeKind, EmbeddingFingerprint, Node, NodeId,
        NodeStatus, Provenance, RemoteEdge,
    };

    use super::*;

    struct Fixture {
        root: std::path::PathBuf,
        target: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let parent = fs::canonicalize(std::env::temp_dir())
                .expect("canonicalize isolated publication parent");
            let root = parent.join(format!("mneme-fresh-current-{}", Ulid::new()));
            fs::create_dir(&root).expect("create isolated publication parent");
            #[cfg(unix)]
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                .expect("make publication parent private");
            let target = root.join("memory.db");
            Self { root, target }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn node(id: u128, summary: &str) -> Node {
        Node::try_new(
            NodeId(Ulid::from(id)),
            summary,
            BodyRef::new(format!("inline://{summary}")).expect("valid body reference"),
            ["alpha", "round-trip"],
            Provenance::derived_empty(),
            0.75,
            0.5,
            NodeStatus::Active,
            1,
        )
        .expect("valid fixture node")
    }

    async fn external_source() -> (crate::MemStore, NodeId) {
        let source = crate::MemStore::new(4);
        let capture = CaptureSource::new(
            "codex",
            "import-test",
            "codex://import-test",
            None,
            None,
            [7; 32],
        )
        .expect("valid capture source");
        let id = capture.node_id();
        let node = Node::try_new(
            id,
            "captured fact",
            BodyRef::new("inline://captured-fact").expect("valid body reference"),
            ["capture"],
            Provenance::External { source: capture },
            0.75,
            0.5,
            NodeStatus::Active,
            1,
        )
        .expect("valid external node");
        source.put_node(&node).await.expect("store external source");
        (source, id)
    }

    async fn exact_component_source() -> crate::MemStore {
        let source = crate::MemStore::new(4);
        source
            .set_embedding_fingerprint(&EmbeddingFingerprint::new(
                "test:fresh-current-v1",
                4,
                "l2-f32-v1",
                "symmetric-v1",
            ))
            .expect("set embedding fingerprint");

        let a = NodeId(Ulid::from(1_u128));
        let b = NodeId(Ulid::from(2_u128));
        for value in [
            node(1, "alpha"),
            node(2, "beta"),
            node(3, "open-merge-a"),
            node(4, "open-merge-b"),
            node(5, "full-merge-winner"),
            node(6, "full-merge-loser"),
            node(7, "supersede-winner"),
            node(8, "supersede-loser"),
        ] {
            source.put_node(&value).await.expect("put fixture node");
        }
        source
            .upsert(a, &[1.0, 0.0, 0.0, 0.0])
            .await
            .expect("put fixture vector");
        let mut edge = Edge::new(a, b, 0.4, EdgeKind::Associative, 2);
        edge.anchor = Some(BodySpan::new(3, 9));
        source.put_edge(&edge).await.expect("put fixture edge");
        source
            .observe_contradiction(a, b, 3)
            .await
            .expect("put open contradiction");
        source
            .observe_merge_candidate(NodeId(Ulid::from(3_u128)), NodeId(Ulid::from(4_u128)), 4)
            .await
            .expect("put open merge candidate");

        let full = FullMergeCommit::new(NodeId(Ulid::from(5_u128)), NodeId(Ulid::from(6_u128)), 6)
            .expect("valid full merge commit");
        source
            .observe_merge_candidate(full.winner, full.loser, 5)
            .await
            .expect("put full merge candidate");
        assert_eq!(
            source
                .commit_full_merge(&full)
                .await
                .expect("commit full merge"),
            FullMergeCommitOutcome::Applied
        );

        let supersede =
            SupersedeCommit::new(NodeId(Ulid::from(7_u128)), NodeId(Ulid::from(8_u128)), 8)
                .expect("valid supersede commit");
        source
            .observe_contradiction(supersede.winner, supersede.loser, 7)
            .await
            .expect("put supersede contradiction");
        assert_eq!(
            source
                .commit_supersede(&supersede)
                .await
                .expect("commit supersede"),
            SupersedeCommitOutcome::Applied
        );

        source
            .put_remote_edge(&RemoteEdge::new(a, Ulid::from(99_u128), b, 0.6))
            .await
            .expect("put remote edge");
        let feedback = FeedbackCommit {
            idempotency: Some(
                FeedbackIdempotency::new(
                    "fresh-current-retry",
                    "a".repeat(64),
                    FeedbackRetryScope::new("fresh-current-epoch", 1, 1)
                        .expect("valid retry scope"),
                )
                .expect("valid idempotency proof"),
            ),
            applied_at: 9,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };
        assert_eq!(
            source
                .commit_feedback(&feedback)
                .await
                .expect("commit retry proof"),
            FeedbackCommitOutcome::Applied
        );
        source
    }

    #[test]
    fn policy_binding_is_deterministic_domain_separated_and_nonzero() {
        let database_id = Ulid::from(0x1234_u128);
        let binding = fresh_current_policy_binding(4, database_id);
        let binding_hex = binding
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            binding_hex, "c0f877a2875cb22324fd75a835495d743c4d8dfb4d77436e558587867b378a36",
            "fresh-current recovery binding bytes are a versioned ABI"
        );
        let historical_context =
            fresh_generation_policy_binding(4, database_id, CatalogContract::EpisodeContextV2);
        let historical_context_hex = historical_context
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            historical_context_hex,
            "a99bb58a4cd77e3bf89a0a1a4289340cd3b55fdaa4e809f451546858db944999",
            "frozen episode-context recovery ABI"
        );
        assert_ne!(binding, historical_context);
        let predecessor =
            fresh_generation_policy_binding(4, database_id, CatalogContract::SingleGraphV1);
        let predecessor_hex = predecessor
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            predecessor_hex,
            "bbe07d7f94195a6787a5e87ce1140f22f36f5175e56450492a2d6eb97218d4eb"
        );
        assert_ne!(
            binding, predecessor,
            "successor must not reuse predecessor recovery authority"
        );
        assert_eq!(binding, fresh_current_policy_binding(4, database_id));
        assert_ne!(binding, [0; 32]);
        assert_ne!(binding, fresh_current_policy_binding(5, database_id));
        assert_ne!(
            binding,
            fresh_current_policy_binding(4, Ulid::from(0x1235_u128))
        );
    }

    #[tokio::test]
    async fn invalid_operation_is_rejected_before_any_lease_artifact() {
        let fixture = Fixture::new();
        let lock = mneme_store_path::store_lock_path(&fixture.target)
            .expect("derive adjacent fixture lease");
        let source = crate::MemStore::new(4);

        let error = CozoStore::materialize_fresh_current(&fixture.target, Ulid::nil(), &source)
            .await
            .expect_err("nil operation id must fail preflight");
        assert!(
            std::error::Error::source(&error)
                .expect("typed error retains a source")
                .to_string()
                .contains("operation id"),
            "nil operation returned the wrong truth boundary"
        );
        assert_eq!(
            error.target_publication_state(),
            FreshCurrentTargetPublicationStateV1::NotPublished
        );
        assert_eq!(
            error.failure_phase(),
            FreshCurrentMaterializationFailurePhaseV1::PrePublication
        );
        assert!(!error.retains_authority());
        assert!(!fixture.target.exists());
        assert!(!lock.exists(), "preflight failure created a lease artifact");
    }

    #[tokio::test]
    async fn post_publish_failure_returns_the_live_authority_until_recovery() {
        let fixture = Fixture::new();
        let source = exact_component_source().await;

        let error = materialize_fresh_current_with_observers(
            &fixture.target,
            Ulid::new(),
            &source,
            |_| {},
            |target, current| {
                assert!(target.is_file(), "postflight ran before publication");
                current
                    .persistent_authority
                    .as_ref()
                    .expect("published current authority")
                    .require_live_guards("injected post-publish failure")?;
                assert_eq!(
                    StoreLease::acquire(target).unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                Err(Error::Backend("injected post-publish failure".into()))
            },
        )
        .await
        .expect_err("injected postflight must return published recovery authority");

        assert!(error.to_string().contains("injected post-publish failure"));
        assert_eq!(
            error.target_publication_state(),
            FreshCurrentTargetPublicationStateV1::Published
        );
        assert_eq!(
            error.failure_phase(),
            FreshCurrentMaterializationFailurePhaseV1::PublishedPostflight
        );
        assert!(error.retains_authority());
        assert_eq!(
            StoreLease::acquire(&fixture.target).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "returned postflight state released its publication lease"
        );

        let state = error.relinquish_for_unpublished_outer_discard();
        assert_eq!(
            state,
            FreshCurrentTargetPublicationStateV1::Published,
            "outer cleanup must retain the truth it needs after authority closes"
        );
        drop(
            StoreLease::acquire(&fixture.target)
                .expect("explicit relinquish releases the postflight authority lease"),
        );
    }

    #[tokio::test]
    async fn exact_components_publish_only_after_closed_verification_and_release_cleanly() {
        let fixture = Fixture::new();
        let source = exact_component_source().await;
        let expected = source.export();
        assert_eq!(expected.nodes.len(), 8);
        assert_eq!(expected.vectors.len(), 1);
        assert_eq!(expected.contradictions.len(), 2);
        assert_eq!(expected.merges.len(), 2);
        assert_eq!(expected.full_merge_commits.len(), 1);
        assert_eq!(expected.supersede_commits.len(), 1);
        assert_eq!(expected.remote_edges.len(), 1);
        assert_eq!(expected.feedback_retries.len(), 1);

        materialize_fresh_current_with_observers(
            &fixture.target,
            Ulid::new(),
            &source,
            |target| {
                assert!(!target.exists(), "target became visible before publication");
                require_no_sqlite_sidecars(target, "pre-publish test")
                    .expect("target sidecars remain absent before publication");
            },
            |target, _current| {
                let error = StoreLease::acquire(target)
                    .expect_err("published current handle must retain target lease");
                assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                Ok(())
            },
        )
        .await
        .expect("materialize fresh current store");

        assert!(fixture.target.is_file());
        require_no_sqlite_sidecars(&fixture.target, "post-return test")
            .expect("materializer returned sidecar-free");

        let lease = StoreLease::acquire(&fixture.target)
            .expect("materializer dropped the current handle and released its lease");
        let reopened = CozoStore::open_existing_current(&fixture.target, lease)
            .expect("reopen materialized current store");
        reopened
            .verify_import(&expected)
            .await
            .expect("every migratable export component round-tripped exactly");
        let actual = reopened.export().await.expect("export reopened store");
        assert!(actual.feedback_retries.is_empty());
        assert_eq!(actual.nodes.len(), expected.nodes.len());
        assert_eq!(actual.edges.len(), expected.edges.len());
        assert_eq!(actual.vectors.len(), expected.vectors.len());
        assert_eq!(actual.contradictions.len(), expected.contradictions.len());
        assert_eq!(actual.merges.len(), expected.merges.len());
        assert_eq!(
            actual.full_merge_commits.len(),
            expected.full_merge_commits.len()
        );
        assert_eq!(
            actual.supersede_commits.len(),
            expected.supersede_commits.len()
        );
        assert_eq!(actual.remote_edges.len(), expected.remote_edges.len());
        reopened
            .prepare_for_file_move()
            .expect("checkpoint reopened fixture");
        drop(reopened);
        require_no_sqlite_sidecars(&fixture.target, "reopened fixture close")
            .expect("reopened fixture closed sidecar-free");
    }

    #[tokio::test]
    async fn fresh_capture_generation_is_absent_only_and_reopens_through_normal_admission() {
        let fixture = Fixture::new();
        let source = crate::MemStore::new(4);
        CozoStore::materialize_fresh_current(&fixture.target, Ulid::new(), &source)
            .await
            .expect("publish capture generation");
        assert!(fixture.target.is_file());
        require_no_sqlite_sidecars(&fixture.target, "capture post-return")
            .expect("capture materializer returned sidecar-free");

        let lease = StoreLease::acquire(&fixture.target).expect("capture target lease");
        CozoStore::require_existing_current(&fixture.target, &lease)
            .expect("positive capture preflight");
        let reopened = CozoStore::open_persistent(&fixture.target, 4, Arc::new(lease))
            .expect("normal open recognizes capture generation");
        assert_eq!(
            reopened.read_meta(super::super::VECTOR_PROJECTION_META_KEY).unwrap().as_deref(),
            Some(crate::storage_contract::conventional_unmanaged::spec::TOUCHSTONES_V1_CATALOG_GENERATION_MARKER),
        );
        reopened
            .prepare_for_file_move()
            .expect("checkpoint capture store");
        drop(reopened);

        let _ = CozoStore::materialize_fresh_current(&fixture.target, Ulid::new(), &source)
            .await
            .expect_err("fresh capture may not overwrite an existing target");
    }

    #[tokio::test]
    async fn ordinary_fresh_store_is_capture_and_episode_enabled() {
        let fixture = Fixture::new();
        CozoStore::materialize_fresh_current(
            &fixture.target,
            Ulid::new(),
            &crate::MemStore::new(4),
        )
        .await
        .unwrap();
        let lease = StoreLease::acquire(&fixture.target).unwrap();
        CozoStore::require_existing_current(&fixture.target, &lease).unwrap();
        let store = CozoStore::open_persistent(&fixture.target, 4, Arc::new(lease)).unwrap();
        assert!(store.relation_exists("episode_head").unwrap());
        assert!(!store.relation_exists("node_vec:candidate_idx").unwrap());
        assert!(!store.relation_exists("node_search:candidate_fts").unwrap());
    }

    #[tokio::test]
    async fn single_graph_import_accepts_external_nonempty_source() {
        let (source, id) = external_source().await;
        let fixture = Fixture::new();
        CozoStore::materialize_fresh_current(&fixture.target, Ulid::new(), &source)
            .await
            .unwrap();
        let lease = StoreLease::acquire(&fixture.target).unwrap();
        let reopened = CozoStore::open_persistent(&fixture.target, 4, Arc::new(lease)).unwrap();
        assert!(reopened.get_node(id).await.unwrap().is_some());
        let mut memory = CozoStore::new(4).unwrap();
        memory.import_mem(&source).await.unwrap();
        assert!(memory.get_node(id).await.unwrap().is_some());
    }

    #[test]
    fn bare_create_is_successor_and_existing_requires_lease() {
        let fixture = Fixture::new();
        let fresh = CozoStore::open(fixture.target.to_str().unwrap(), 4).unwrap();
        fresh.prepare_for_file_move().unwrap();
        drop(fresh);
        assert!(CozoStore::open(fixture.target.to_str().unwrap(), 4).is_err());
        let lease = StoreLease::acquire(&fixture.target).unwrap();
        CozoStore::require_existing_current(&fixture.target, &lease).unwrap();
        CozoStore::open_persistent(&fixture.target, 4, Arc::new(lease)).unwrap();
    }
    #[tokio::test]
    async fn historical_publication_has_no_concern_port_and_empty_advisory_export() {
        let fixture = Fixture::new();
        let source = crate::MemStore::new(4);
        materialize_fresh_generation_with_observers(
            &fixture.target,
            Ulid::new(),
            &source,
            CatalogContract::SingleGraphV1,
            |_| {},
            |_, current| {
                assert!(mneme_core::ports::GraphStore::concerns(current).is_none());
                assert!(!current.relation_exists("concern")?);
                Ok(())
            },
        )
        .await
        .unwrap();
        let lease = StoreLease::acquire(&fixture.target).unwrap();
        assert!(CozoStore::require_existing_current(&fixture.target, &lease).is_err());
        let export = CozoStore::export_concern_predecessor(&fixture.target, &lease)
            .await
            .unwrap();
        assert!(export.concerns.is_empty());
        assert_eq!(
            super::super::canonical_export_value(export).unwrap(),
            super::super::canonical_export_value(source.export()).unwrap()
        );
    }
    async fn populated_concern_finding_source() -> crate::MemStore {
        use crate::concern_tests::{finding, node, notice, resulting};
        use mneme_core::concern::{ConcernKind, ConcernStore};
        let source = crate::MemStore::new(4);
        let a = node(101, "bounded left claim");
        let b = node(102, "bounded right claim");
        source.put_node(&a).await.unwrap();
        source.put_node(&b).await.unwrap();
        let row = resulting(
            source
                .update_concern(&notice(&a, &b, ConcernKind::Disagreement))
                .await
                .unwrap(),
        );
        source
            .update_concern(&finding(row, "this fixture only"))
            .await
            .unwrap();
        let export = source.export();
        assert_eq!(export.concerns.len(), 1);
        assert!(export.concerns[0].finding().is_some());
        source
    }

    #[tokio::test]
    async fn concern_successor_stage_cuts_do_not_publish_or_change_source() {
        let source = populated_concern_finding_source().await;
        let expected = super::super::canonical_export_value(source.export()).unwrap();
        for cut in [
            ConcernStageCut::AfterImportBeforeVerification,
            ConcernStageCut::TamperCatalogAfterVerification,
        ] {
            let fixture = Fixture::new();
            CONCERN_STAGE_CUT.with(|slot| slot.set(Some(cut)));
            let error = CozoStore::materialize_fresh_current(&fixture.target, Ulid::new(), &source)
                .await
                .unwrap_err();
            assert!(
                CONCERN_STAGE_CUT.with(|slot| slot.get()).is_none(),
                "one-shot cut must be consumed"
            );
            assert_eq!(
                error.target_publication_state(),
                FreshCurrentTargetPublicationStateV1::NotPublished
            );
            assert_eq!(
                error.failure_phase(),
                FreshCurrentMaterializationFailurePhaseV1::PrePublication
            );
            assert!(!fixture.target.exists());
            assert_eq!(
                super::super::canonical_export_value(source.export()).unwrap(),
                expected
            );
            assert!(error.to_string().contains(
                if cut == ConcernStageCut::AfterImportBeforeVerification {
                    "concern_after_import_before_verification"
                } else {
                    "fresh_current_raw_closed_not_published"
                }
            ));
            drop(error);
            drop(
                StoreLease::acquire(&fixture.target)
                    .expect("unpublished failure releases target lease"),
            );
        }
    }

    #[tokio::test]
    async fn concern_finding_postpublication_failure_retains_authority_and_reopens_exactly() {
        let fixture = Fixture::new();
        let source = populated_concern_finding_source().await;
        let expected = source.export();
        let error = materialize_fresh_current_with_observers(
            &fixture.target,
            Ulid::new(),
            &source,
            |_| {},
            |_, _| {
                Err(Error::Backend(
                    "injected populated concern postflight".into(),
                ))
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.target_publication_state(),
            FreshCurrentTargetPublicationStateV1::Published
        );
        assert_eq!(
            error.failure_phase(),
            FreshCurrentMaterializationFailurePhaseV1::PublishedPostflight
        );
        assert!(error.retains_authority());
        assert_eq!(
            StoreLease::acquire(&fixture.target).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(
            error.relinquish_for_unpublished_outer_discard(),
            FreshCurrentTargetPublicationStateV1::Published
        );
        let lease = StoreLease::acquire(&fixture.target).unwrap();
        let current = CozoStore::open_leased_current(&fixture.target, Arc::new(lease)).unwrap();
        current.verify_import(&expected).await.unwrap();
        let actual = current.export().await.unwrap();
        assert_eq!(actual.concerns, expected.concerns);
        assert!(actual.concerns[0].finding().is_some());
        assert_eq!(
            super::super::canonical_export_value(source.export()).unwrap(),
            super::super::canonical_export_value(expected).unwrap()
        );
        current.prepare_for_file_move().unwrap();
        drop(current);
        require_no_sqlite_sidecars(&fixture.target, "populated concern recovered close").unwrap();
    }
}
