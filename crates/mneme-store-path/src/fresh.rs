//! Lease-owned filesystem staging for a fresh persistent store.
//!
//! This module deliberately proves only filesystem facts.  In particular, a
//! [`PreparedFreshStoreStageV1`] is not evidence that `memory.db` is SQLite, a
//! Mneme store, or a current catalog generation.  Exposing the pathname cannot
//! prove that a caller closed every handle or performed semantic validation.
//! The storage crate must establish those facts before it asks this layer to
//! seal and publish the file, and must validate the published path again
//! afterwards.  These capabilities must not be routed into production until a
//! consuming storage integration enforces that protocol.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::fs::File;
#[cfg(all(unix, test))]
use std::fs::OpenOptions;
#[cfg(all(unix, test))]
use std::io::Write;
#[cfg(all(unix, test))]
use std::os::unix::fs::OpenOptionsExt;

use ulid::Ulid;

use super::StoreLease;
#[cfg(unix)]
use super::UnixIdentity;

#[cfg(unix)]
mod record;
#[cfg(unix)]
mod unix;

#[cfg(unix)]
use record::{encode_intent, encode_prepared, require_target_binding_len};
#[cfg(unix)]
use unix::*;

const STAGE_PREFIX: &str = ".mneme-fresh-store-v1-";
const STAGE_DATABASE: &str = "memory.db";
const INTENT_RECORD: &str = "intent.v1";
const PREPARED_RECORD: &str = "prepared.v1";
const MAX_STAGE_ENTRIES: usize = 8;

/// Caller-owned identity for one fresh-store filesystem operation.
///
/// `policy_binding` is opaque to this crate.  It lets the storage layer bind
/// its dimension, database id, and generation policy into durable recovery
/// evidence without teaching this filesystem crate those semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FreshStoreStageSpecV1 {
    pub operation_id: Ulid,
    pub policy_binding: [u8; 32],
}

/// A durable private operation directory in which a caller may build a store.
///
/// This capability owns the lease for the final target.  Dropping it closes
/// descriptors and releases the lease, but intentionally removes nothing.
#[derive(Debug)]
#[must_use = "a fresh stage must be explicitly sealed, discarded, or left for recovery"]
pub struct FreshStoreStageV1 {
    target: PathBuf,
    stage_path: PathBuf,
    database_path: PathBuf,
    spec: FreshStoreStageSpecV1,
    lease: StoreLease,
    #[cfg(unix)]
    stage_dir: File,
    #[cfg(unix)]
    stage_identity: UnixIdentity,
    #[cfg(unix)]
    intent: File,
    #[cfg(unix)]
    intent_identity: FreshFileIdentityV1,
    #[cfg(unix)]
    main: File,
    #[cfg(unix)]
    initial_main_identity: FreshFileIdentityV1,
}

/// A closed, private, sidecar-free staged file whose inode is durably recorded.
///
/// This is filesystem readiness only, not a semantic store permit.
#[derive(Debug)]
#[must_use = "a prepared fresh stage must be explicitly published or discarded"]
pub struct PreparedFreshStoreStageV1 {
    target: PathBuf,
    stage_path: PathBuf,
    database_path: PathBuf,
    spec: FreshStoreStageSpecV1,
    lease: StoreLease,
    #[cfg(unix)]
    stage_dir: File,
    #[cfg(unix)]
    stage_identity: UnixIdentity,
    #[cfg(unix)]
    intent: File,
    #[cfg(unix)]
    intent_identity: FreshFileIdentityV1,
    #[cfg(unix)]
    prepared: File,
    #[cfg(unix)]
    prepared_identity: FreshFileIdentityV1,
    #[cfg(unix)]
    main: File,
    #[cfg(unix)]
    initial_main_identity: FreshFileIdentityV1,
    #[cfg(unix)]
    main_identity: FreshFileIdentityV1,
}

/// One atomically published target retaining the original target lease and the
/// open inode that crossed the rename.
#[derive(Debug)]
#[must_use = "consume the published capability into the persistent open path"]
pub struct PublishedFreshStoreV1 {
    target: PathBuf,
    lease: StoreLease,
    #[cfg(unix)]
    main: File,
    #[cfg(unix)]
    main_identity: FreshFileIdentityV1,
}

/// What the two publication pathnames named after a rename syscall reported an
/// error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FreshStoreNameObservationV1 {
    Absent,
    ExactRetainedMain,
    OtherOrUninspectable,
}

/// The last publication truth boundary crossed before recovery became
/// necessary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FreshStorePublicationRecoveryPhaseV1 {
    /// The rename syscall reported an error, but identity inspection proved
    /// exact target plus absent source.
    RenameReportedErrorAfterCrossing,
    /// The rename syscall reported an error and identity inspection could not
    /// prove either the retryable or crossed state.
    RenameOutcomeAmbiguous,
    /// Rename was known to cross, but the complete inode/stage/parent
    /// durability barrier did not complete.
    DurabilityBarrierFailed,
    /// The durability barrier completed, but the initial published-path guard
    /// did not. Cleanup has not started.
    DurablePublicationInitialGuardFailed,
}

/// Exact cleanup operation that failed after durable publication and its
/// initial pathname guard were established.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FreshStorePublishedResiduePhaseV1 {
    VerifyIntentRecord,
    RemoveIntentRecord,
    SyncIntentRemoval,
    VerifyPreparedRecord,
    RemovePreparedRecord,
    SyncPreparedRemoval,
    RemoveStageDirectory,
    SyncStageRemoval,
}

/// Publication failed at one of four materially different truth boundaries.
///
/// Only [`Self::NotPublished`] retains a prepared-stage capability; whether a
/// preflight error is actually retryable depends on that error and the next
/// full validation. Rename ambiguity/crossing, durable publication with
/// cleanup residue, and a final published-path guard failure retain distinct
/// capabilities so callers cannot accidentally treat one as another.
#[derive(Debug)]
#[must_use = "publication failure retains a capability that must be handled or deliberately left for recovery"]
pub enum FreshStorePublishErrorV1 {
    NotPublished {
        source: io::Error,
        prepared: Box<PreparedFreshStoreStageV1>,
    },
    PublicationNeedsRecovery {
        source: io::Error,
        recovery: Box<FreshStorePublicationNeedsRecoveryV1>,
    },
    PublishedWithResidue {
        source: io::Error,
        recovery: Box<FreshStorePublishedWithResidueV1>,
    },
    PublishedGuardNeedsRecovery {
        source: io::Error,
        recovery: Box<FreshStorePublishedGuardNeedsRecoveryV1>,
    },
}

/// Rename may have crossed, but the durable-and-initially-guarded publication
/// cut was not completed.
///
/// This retains the target lease and exact main descriptor. `stage_path()` is
/// exposed only when the operation directory was still proved to be named at
/// error construction; ambiguous rename outcomes instead expose the observed
/// source and target identities. Dropping it performs no cleanup.
#[derive(Debug)]
#[must_use = "publication crossed the rename boundary and requires explicit future recovery"]
pub struct FreshStorePublicationNeedsRecoveryV1 {
    state: PreparedFreshStoreStageV1,
    phase: FreshStorePublicationRecoveryPhaseV1,
    source_observation: FreshStoreNameObservationV1,
    target_observation: FreshStoreNameObservationV1,
    named_stage_path: Option<PathBuf>,
}

/// A durably published and initially guarded target whose stage cleanup did
/// not finish.
#[derive(Debug)]
#[must_use = "durable publication has operation residue requiring explicit cleanup recovery"]
pub struct FreshStorePublishedWithResidueV1 {
    state: PreparedFreshStoreStageV1,
    phase: FreshStorePublishedResiduePhaseV1,
    named_stage_path: Option<PathBuf>,
}

/// Stage cleanup completed, but the final published-path guard failed.
///
/// Only the published target lease and retained main descriptor survive this
/// boundary; this capability intentionally claims no remaining stage evidence.
#[derive(Debug)]
#[must_use = "durable publication requires a final pathname guard retry"]
pub struct FreshStorePublishedGuardNeedsRecoveryV1 {
    state: PublishedFreshStoreV1,
}

impl FreshStorePublishErrorV1 {
    pub fn kind(&self) -> io::ErrorKind {
        self.io_error().kind()
    }

    pub fn io_error(&self) -> &io::Error {
        match self {
            Self::NotPublished { source, .. }
            | Self::PublicationNeedsRecovery { source, .. }
            | Self::PublishedWithResidue { source, .. }
            | Self::PublishedGuardNeedsRecovery { source, .. } => source,
        }
    }
}

impl std::fmt::Display for FreshStorePublishErrorV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPublished { source, .. } => {
                write!(formatter, "fresh store was not published: {source}")
            }
            Self::PublicationNeedsRecovery { source, .. } => write!(
                formatter,
                "fresh-store publication did not reach the durable-and-guarded cut and requires recovery: {source}"
            ),
            Self::PublishedWithResidue {
                source, recovery, ..
            } => write!(
                formatter,
                "fresh store is durably published with cleanup residue at {:?}: {source}",
                recovery.phase
            ),
            Self::PublishedGuardNeedsRecovery { source, .. } => write!(
                formatter,
                "fresh store is durably published and cleaned, but its final pathname guard failed: {source}"
            ),
        }
    }
}

impl std::error::Error for FreshStorePublishErrorV1 {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.io_error())
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FreshFileIdentityV1 {
    device: u64,
    inode: u64,
    owner: u32,
    mode: u32,
    links: u64,
    len: u64,
}

#[cfg(unix)]
#[derive(Debug)]
struct ValidatedDiscardMemberV1 {
    name: OsString,
    file: File,
    identity: FreshFileIdentityV1,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenameNamespaceObservationV1 {
    source: FreshStoreNameObservationV1,
    target: FreshStoreNameObservationV1,
}

#[cfg(unix)]
#[derive(Debug)]
struct PublishedCleanupErrorV1 {
    source: io::Error,
    phase: FreshStorePublishedResiduePhaseV1,
}

#[cfg(unix)]
impl FreshFileIdentityV1 {
    fn same_stable_file(self, other: Self) -> bool {
        self.device == other.device
            && self.inode == other.inode
            && self.owner == other.owner
            && self.mode == other.mode
            && self.links == other.links
    }
}

impl StoreLease {
    /// Reserve a deterministic, private sibling operation directory while
    /// consuming and retaining this target lease.
    pub fn begin_fresh_store_v1(
        self,
        target: &Path,
        spec: FreshStoreStageSpecV1,
    ) -> io::Result<FreshStoreStageV1> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
        {
            begin_fresh_store_v1(self, target, spec)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
        {
            let _ = (self, target, spec);
            Err(unsupported())
        }
    }
}

impl FreshStoreStageV1 {
    /// Path of the private, pre-created mode-0600 file which the storage layer
    /// may open and populate as its database.
    ///
    /// This filesystem capability cannot observe whether the caller later
    /// closes every handle or validates the resulting store semantics.
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Bind a closed, nonempty, private, sidecar-free main inode and durably
    /// record it before publication.
    ///
    /// The caller must already have performed its semantic validation.  This
    /// method intentionally does not inspect SQLite or Mneme bytes.
    pub fn seal_closed_file(self) -> io::Result<PreparedFreshStoreStageV1> {
        #[cfg(unix)]
        {
            seal_closed_file(self)
        }
        #[cfg(not(unix))]
        {
            let _ = self;
            Err(unsupported())
        }
    }

    /// Explicitly discard an unpublished operation-owned stage.
    ///
    /// The caller must have closed every handle it opened.  Cleanup accepts only the
    /// fixed record, main, and three SQLite sidecar names with exact private
    /// identities.  Drop never calls this method.
    pub fn discard_closed(self) -> io::Result<StoreLease> {
        #[cfg(unix)]
        {
            discard_building(self)
        }
        #[cfg(not(unix))]
        {
            let _ = self;
            Err(unsupported())
        }
    }
}

impl PreparedFreshStoreStageV1 {
    /// Path retained for diagnostics only.
    ///
    /// Its exposure is not evidence that caller-owned handles are closed or
    /// that semantic store validation happened before sealing.
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Atomically rename the recorded inode into the absent target without
    /// releasing the target lease.  A pre-rename error returns the prepared
    /// capability; a post-rename error returns a distinct recovery capability.
    pub fn publish(self) -> Result<PublishedFreshStoreV1, FreshStorePublishErrorV1> {
        #[cfg(unix)]
        {
            publish_prepared(self)
        }
        #[cfg(not(unix))]
        {
            Err(FreshStorePublishErrorV1::NotPublished {
                source: unsupported(),
                prepared: Box::new(self),
            })
        }
    }

    /// Explicitly discard a prepared but unpublished stage.
    pub fn discard_closed(self) -> io::Result<StoreLease> {
        #[cfg(unix)]
        {
            discard_prepared(self)
        }
        #[cfg(not(unix))]
        {
            let _ = self;
            Err(unsupported())
        }
    }
}

impl PublishedFreshStoreV1 {
    pub fn path(&self) -> &Path {
        &self.target
    }

    /// Revalidate the unchanged target lease and prove the published pathname
    /// still names the inode retained across publication.
    pub fn require_guards(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            require_published_guards(self)
        }
        #[cfg(not(unix))]
        {
            Err(unsupported())
        }
    }

    /// Revalidate the retained lease and published main-file identity after a
    /// database runtime has been constructed.
    ///
    /// Unlike [`Self::require_guards`], this deliberately permits SQLite's
    /// runtime-owned WAL, SHM, or rollback-journal sidecars.  A publisher must
    /// use the stronger sidecar-free guard through the final instant before
    /// constructing SQLite, then retain this capability and use this runtime
    /// guard for the lifetime of the opened database.
    pub fn require_runtime_guards(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            require_published_main_guards(self)
        }
        #[cfg(not(unix))]
        {
            Err(unsupported())
        }
    }
}

impl FreshStorePublicationNeedsRecoveryV1 {
    pub fn path(&self) -> &Path {
        &self.state.target
    }

    pub fn phase(&self) -> FreshStorePublicationRecoveryPhaseV1 {
        self.phase
    }

    pub fn source_observation(&self) -> FreshStoreNameObservationV1 {
        self.source_observation
    }

    pub fn target_observation(&self) -> FreshStoreNameObservationV1 {
        self.target_observation
    }

    /// A still-named operation directory proved at error construction.
    /// Ambiguous namespace outcomes return `None` rather than presenting a
    /// diagnostic pathname as a recovery recipe.
    pub fn stage_path(&self) -> Option<&Path> {
        self.named_stage_path.as_deref()
    }

    /// Revalidate the target as the retained main inode. This can fail for an
    /// ambiguous outcome and does not recover, clean up, or semantically admit
    /// the store.
    pub fn require_guards(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            require_post_rename_guards(&self.state)
        }
        #[cfg(not(unix))]
        {
            Err(unsupported())
        }
    }
}

impl FreshStorePublishedWithResidueV1 {
    pub fn path(&self) -> &Path {
        &self.state.target
    }

    pub fn phase(&self) -> FreshStorePublishedResiduePhaseV1 {
        self.phase
    }

    /// Present only when the operation directory was still proved named at
    /// error construction. Later cleanup phases may already have removed it.
    pub fn stage_path(&self) -> Option<&Path> {
        self.named_stage_path.as_deref()
    }

    pub fn require_guards(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            require_post_rename_guards(&self.state)
        }
        #[cfg(not(unix))]
        {
            Err(unsupported())
        }
    }
}

impl FreshStorePublishedGuardNeedsRecoveryV1 {
    pub fn path(&self) -> &Path {
        self.state.path()
    }

    pub fn require_guards(&self) -> io::Result<()> {
        self.state.require_guards()
    }
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "fresh-store atomic no-clobber publication is supported only on Linux and Darwin",
    )
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
fn begin_fresh_store_v1(
    lease: StoreLease,
    requested_target: &Path,
    spec: FreshStoreStageSpecV1,
) -> io::Result<FreshStoreStageV1> {
    if spec.operation_id == Ulid::nil() {
        return Err(invalid_input("fresh-store operation id must not be nil"));
    }
    if spec.policy_binding.iter().all(|byte| *byte == 0) {
        return Err(invalid_input(
            "fresh-store policy binding must not be all zero",
        ));
    }
    let (target, target_name) = exact_target(&lease, requested_target)?;
    require_fresh_target_parent(&lease)?;
    require_target_binding_len(&target)?;
    require_target_family_absent(&lease, &target_name)?;

    // The durable intent below binds this exact retained lease inode and its
    // parent. Do the same durability cut whether acquisition created the lock
    // or opened an older one, without taxing ordinary lease acquisitions that
    // never publish recovery evidence.
    super::sync_retained_lease(&lease.parent, &lease.file, &lease.lock_path)?;

    let stage_name = stage_name(spec.operation_id);
    if ascii_casefold_equal(&target_name, &stage_name) {
        return Err(invalid_input(
            "fresh-store target filename collides with its operation directory under ASCII case-folding",
        ));
    }
    mkdir_private_at(&lease.parent, &stage_name)?;
    let stage_dir = open_stage_directory(&lease, &stage_name)?;
    set_exact_mode(&stage_dir, 0o700, "fresh stage directory")?;
    let stage_identity = validate_stage_directory(
        &stage_dir.metadata()?,
        &lease.parent_path.join(&stage_name),
        lease.parent_identity.device,
    )?;

    let stage_path = lease.parent_path.join(&stage_name);
    let database_path = stage_path.join(STAGE_DATABASE);
    let intent_bytes = encode_intent(&lease, &target, stage_identity, spec)?;
    let (intent, intent_identity) = create_record(&stage_dir, INTENT_RECORD, &intent_bytes)?;
    let (main, initial_main_identity) = create_empty_main(&stage_dir, &database_path)?;
    stage_dir.sync_all()?;
    lease.parent.sync_all()?;
    lease.require_guards(&target)?;
    require_named_stage_directory(&lease, &stage_name, stage_identity)?;
    require_target_family_absent(&lease, &target_name)?;
    verify_record(
        &stage_dir,
        INTENT_RECORD,
        &intent,
        intent_identity,
        &intent_bytes,
    )?;
    require_named_file_stable_identity(
        &stage_dir,
        OsStr::new(STAGE_DATABASE),
        initial_main_identity,
        "fresh stage main",
    )?;
    require_exact_inventory(
        &stage_path,
        &lease,
        stage_identity,
        &[INTENT_RECORD, STAGE_DATABASE],
    )?;

    Ok(FreshStoreStageV1 {
        target,
        stage_path,
        database_path,
        spec,
        lease,
        stage_dir,
        stage_identity,
        intent,
        intent_identity,
        main,
        initial_main_identity,
    })
}

#[cfg(unix)]
fn seal_closed_file(stage: FreshStoreStageV1) -> io::Result<PreparedFreshStoreStageV1> {
    validate_building_stage(&stage, true)?;
    stage.main.sync_all()?;
    let main_identity = validate_private_main(&stage.main.metadata()?, &stage.database_path)?;
    if !stage.initial_main_identity.same_stable_file(main_identity) {
        return Err(invalid_data(
            "fresh-store main descriptor changed stable identity during build",
        ));
    }

    let intent_bytes = encode_intent(
        &stage.lease,
        &stage.target,
        stage.stage_identity,
        stage.spec,
    )?;
    let prepared_bytes = encode_prepared(&intent_bytes, main_identity)?;
    let (prepared, prepared_identity) =
        create_record(&stage.stage_dir, PREPARED_RECORD, &prepared_bytes)?;
    stage.stage_dir.sync_all()?;

    verify_record(
        &stage.stage_dir,
        PREPARED_RECORD,
        &prepared,
        prepared_identity,
        &prepared_bytes,
    )?;
    require_named_file_identity(
        &stage.stage_dir,
        OsStr::new(STAGE_DATABASE),
        main_identity,
        true,
        "stage main",
    )?;
    require_exact_inventory(
        &stage.stage_path,
        &stage.lease,
        stage.stage_identity,
        &[INTENT_RECORD, PREPARED_RECORD, STAGE_DATABASE],
    )?;

    let FreshStoreStageV1 {
        target,
        stage_path,
        database_path,
        spec,
        lease,
        stage_dir,
        stage_identity,
        intent,
        intent_identity,
        main,
        initial_main_identity,
    } = stage;
    Ok(PreparedFreshStoreStageV1 {
        target,
        stage_path,
        database_path,
        spec,
        lease,
        stage_dir,
        stage_identity,
        intent,
        intent_identity,
        prepared,
        prepared_identity,
        main,
        initial_main_identity,
        main_identity,
    })
}

#[cfg(unix)]
fn publish_prepared(
    stage: PreparedFreshStoreStageV1,
) -> Result<PublishedFreshStoreV1, FreshStorePublishErrorV1> {
    let preflight = (|| -> io::Result<OsString> {
        validate_prepared_stage(&stage)?;
        let target_name = stage
            .target
            .file_name()
            .ok_or_else(|| invalid_input("fresh-store target has no filename"))?
            .to_os_string();
        require_target_family_absent(&stage.lease, &target_name)?;
        Ok(target_name)
    })();
    let target_name = match preflight {
        Ok(target_name) => target_name,
        Err(source) => {
            return Err(FreshStorePublishErrorV1::NotPublished {
                source,
                prepared: Box::new(stage),
            });
        }
    };

    if let Err(source) = rename_for_publication(&stage, &target_name) {
        let observation = observe_rename_namespace(&stage, &target_name);
        if observation.source == FreshStoreNameObservationV1::ExactRetainedMain
            && observation.target == FreshStoreNameObservationV1::Absent
        {
            return Err(FreshStorePublishErrorV1::NotPublished {
                source,
                prepared: Box::new(stage),
            });
        }

        let phase = if observation.source == FreshStoreNameObservationV1::Absent
            && observation.target == FreshStoreNameObservationV1::ExactRetainedMain
        {
            FreshStorePublicationRecoveryPhaseV1::RenameReportedErrorAfterCrossing
        } else {
            FreshStorePublicationRecoveryPhaseV1::RenameOutcomeAmbiguous
        };
        return Err(publication_recovery_error(
            stage,
            source,
            phase,
            observation,
        ));
    }

    // Crossing is known, but publication is not durable until the retained
    // inode plus both namespace directories have synced. Every error before
    // that complete barrier keeps the pre-durable recovery capability.
    let durability = (|| -> io::Result<()> {
        fail_publish_point_for_test(FreshPublishFailPointV1::DurabilityBarrier)?;
        require_name_absent(&stage.stage_dir, OsStr::new(STAGE_DATABASE), "staged main")?;
        require_named_file_identity(
            &stage.lease.parent,
            &target_name,
            stage.main_identity,
            true,
            "published main",
        )?;
        stage.main.sync_all()?;
        stage.stage_dir.sync_all()?;
        stage.lease.parent.sync_all()
    })();
    if let Err(source) = durability {
        let observation = observe_rename_namespace(&stage, &target_name);
        return Err(publication_recovery_error(
            stage,
            source,
            FreshStorePublicationRecoveryPhaseV1::DurabilityBarrierFailed,
            observation,
        ));
    }

    let initial_guard = fail_publish_point_for_test(FreshPublishFailPointV1::InitialGuard)
        .and_then(|()| require_post_rename_guards(&stage));
    if let Err(source) = initial_guard {
        let observation = observe_rename_namespace(&stage, &target_name);
        return Err(publication_recovery_error(
            stage,
            source,
            FreshStorePublicationRecoveryPhaseV1::DurablePublicationInitialGuardFailed,
            observation,
        ));
    }

    // Durable publication and its first pathname guard are now facts. Cleanup
    // failure must never be reported as a prepared or pre-durable operation.
    if let Err(cleanup) = cleanup_published_records(&stage) {
        let named_stage_path = named_stage_path_if_proved(&stage);
        return Err(FreshStorePublishErrorV1::PublishedWithResidue {
            source: cleanup.source,
            recovery: Box::new(FreshStorePublishedWithResidueV1 {
                state: stage,
                phase: cleanup.phase,
                named_stage_path,
            }),
        });
    }

    // Cleanup consumed all named stage evidence. Convert before the final
    // guard so its error capability cannot accidentally claim that evidence.
    let published = into_published(stage);
    let final_guard = fail_publish_point_for_test(FreshPublishFailPointV1::FinalGuard)
        .and_then(|()| require_published_guards(&published));
    if let Err(source) = final_guard {
        return Err(FreshStorePublishErrorV1::PublishedGuardNeedsRecovery {
            source,
            recovery: Box::new(FreshStorePublishedGuardNeedsRecoveryV1 { state: published }),
        });
    }
    Ok(published)
}

#[cfg(unix)]
fn publication_recovery_error(
    stage: PreparedFreshStoreStageV1,
    source: io::Error,
    phase: FreshStorePublicationRecoveryPhaseV1,
    observation: RenameNamespaceObservationV1,
) -> FreshStorePublishErrorV1 {
    let named_stage_path = if phase == FreshStorePublicationRecoveryPhaseV1::RenameOutcomeAmbiguous
    {
        None
    } else {
        named_stage_path_if_proved(&stage)
    };
    FreshStorePublishErrorV1::PublicationNeedsRecovery {
        source,
        recovery: Box::new(FreshStorePublicationNeedsRecoveryV1 {
            state: stage,
            phase,
            source_observation: observation.source,
            target_observation: observation.target,
            named_stage_path,
        }),
    }
}

#[cfg(unix)]
fn into_published(stage: PreparedFreshStoreStageV1) -> PublishedFreshStoreV1 {
    let PreparedFreshStoreStageV1 {
        target,
        lease,
        main,
        main_identity,
        ..
    } = stage;
    PublishedFreshStoreV1 {
        target,
        lease,
        main,
        main_identity,
    }
}

#[cfg(unix)]
fn observe_rename_namespace(
    stage: &PreparedFreshStoreStageV1,
    target_name: &OsStr,
) -> RenameNamespaceObservationV1 {
    RenameNamespaceObservationV1 {
        source: observe_main_name(
            &stage.stage_dir,
            OsStr::new(STAGE_DATABASE),
            stage.main_identity,
        ),
        target: observe_main_name(&stage.lease.parent, target_name, stage.main_identity),
    }
}

#[cfg(unix)]
fn observe_main_name(
    parent: &File,
    name: &OsStr,
    expected: FreshFileIdentityV1,
) -> FreshStoreNameObservationV1 {
    match open_private_file_at(parent, name, "fresh-store rename outcome") {
        Ok(file) => match file
            .metadata()
            .and_then(|metadata| validate_private_main(&metadata, Path::new(name)))
        {
            Ok(observed) if observed == expected => FreshStoreNameObservationV1::ExactRetainedMain,
            Ok(_) | Err(_) => FreshStoreNameObservationV1::OtherOrUninspectable,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            FreshStoreNameObservationV1::Absent
        }
        Err(_) => FreshStoreNameObservationV1::OtherOrUninspectable,
    }
}

#[cfg(unix)]
fn named_stage_path_if_proved(stage: &PreparedFreshStoreStageV1) -> Option<PathBuf> {
    let stage_name = stage.stage_path.file_name()?;
    require_named_stage_directory(&stage.lease, stage_name, stage.stage_identity)
        .ok()
        .map(|()| stage.stage_path.clone())
}

#[cfg(unix)]
fn rename_for_publication(
    stage: &PreparedFreshStoreStageV1,
    target_name: &OsStr,
) -> io::Result<()> {
    if take_publish_failpoint_for_test(FreshPublishFailPointV1::RenameErrorBeforeSyscall) {
        return Err(injected_publish_error("before no-clobber rename"));
    }
    if take_publish_failpoint_for_test(FreshPublishFailPointV1::RenameErrorAmbiguous) {
        create_competing_target_for_test(stage, target_name)?;
    }

    rename_no_replace_at(
        &stage.stage_dir,
        OsStr::new(STAGE_DATABASE),
        &stage.lease.parent,
        target_name,
    )?;
    if take_publish_failpoint_for_test(FreshPublishFailPointV1::RenameErrorAfterSyscall) {
        return Err(injected_publish_error("after no-clobber rename"));
    }
    Ok(())
}

#[cfg(all(unix, test))]
fn create_competing_target_for_test(
    stage: &PreparedFreshStoreStageV1,
    target_name: &OsStr,
) -> io::Result<()> {
    let path = stage.lease.parent_path.join(target_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    set_exact_mode(&file, 0o600, "injected competing target")?;
    file.write_all(b"injected competing target")?;
    file.sync_all()
}

#[cfg(all(unix, not(test)))]
fn create_competing_target_for_test(
    _stage: &PreparedFreshStoreStageV1,
    _target_name: &OsStr,
) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn validate_building_stage(stage: &FreshStoreStageV1, require_main_only: bool) -> io::Result<()> {
    stage.lease.require_guards(&stage.target)?;
    let target_name = stage
        .target
        .file_name()
        .ok_or_else(|| invalid_input("fresh-store target has no filename"))?;
    require_target_family_absent(&stage.lease, target_name)?;
    require_named_stage_directory(
        &stage.lease,
        stage
            .stage_path
            .file_name()
            .ok_or_else(|| invalid_input("fresh stage has no filename"))?,
        stage.stage_identity,
    )?;
    let intent_bytes = encode_intent(
        &stage.lease,
        &stage.target,
        stage.stage_identity,
        stage.spec,
    )?;
    verify_record(
        &stage.stage_dir,
        INTENT_RECORD,
        &stage.intent,
        stage.intent_identity,
        &intent_bytes,
    )?;
    if require_main_only {
        require_exact_inventory(
            &stage.stage_path,
            &stage.lease,
            stage.stage_identity,
            &[INTENT_RECORD, STAGE_DATABASE],
        )?;
    } else {
        require_building_inventory(stage)?;
    }
    let observed_main = validate_private_file(&stage.main.metadata()?, &stage.database_path)?;
    if !stage.initial_main_identity.same_stable_file(observed_main) {
        return Err(invalid_data(
            "fresh-store main descriptor changed stable identity during build",
        ));
    }
    require_named_file_stable_identity(
        &stage.stage_dir,
        OsStr::new(STAGE_DATABASE),
        stage.initial_main_identity,
        "fresh stage main",
    )?;
    Ok(())
}

#[cfg(unix)]
fn validate_prepared_stage(stage: &PreparedFreshStoreStageV1) -> io::Result<()> {
    stage.lease.require_guards(&stage.target)?;
    let target_name = stage
        .target
        .file_name()
        .ok_or_else(|| invalid_input("fresh-store target has no filename"))?;
    require_target_family_absent(&stage.lease, target_name)?;
    let stage_name = stage
        .stage_path
        .file_name()
        .ok_or_else(|| invalid_input("fresh stage has no filename"))?;
    require_named_stage_directory(&stage.lease, stage_name, stage.stage_identity)?;
    let intent_bytes = encode_intent(
        &stage.lease,
        &stage.target,
        stage.stage_identity,
        stage.spec,
    )?;
    verify_record(
        &stage.stage_dir,
        INTENT_RECORD,
        &stage.intent,
        stage.intent_identity,
        &intent_bytes,
    )?;
    let prepared_bytes = encode_prepared(&intent_bytes, stage.main_identity)?;
    verify_record(
        &stage.stage_dir,
        PREPARED_RECORD,
        &stage.prepared,
        stage.prepared_identity,
        &prepared_bytes,
    )?;
    require_named_file_identity(
        &stage.stage_dir,
        OsStr::new(STAGE_DATABASE),
        stage.main_identity,
        true,
        "stage main",
    )?;
    require_exact_inventory(
        &stage.stage_path,
        &stage.lease,
        stage.stage_identity,
        &[INTENT_RECORD, PREPARED_RECORD, STAGE_DATABASE],
    )
}

#[cfg(unix)]
fn discard_building(stage: FreshStoreStageV1) -> io::Result<StoreLease> {
    validate_building_stage(&stage, false)?;
    let inventory = stage_inventory(&stage.stage_path, MAX_STAGE_ENTRIES)?;
    let mut members = Vec::new();
    for name in [
        sqlite_sidecar_name(STAGE_DATABASE, "-journal"),
        sqlite_sidecar_name(STAGE_DATABASE, "-shm"),
        sqlite_sidecar_name(STAGE_DATABASE, "-wal"),
        OsString::from(STAGE_DATABASE),
    ] {
        if inventory.contains(&name) {
            let file = open_private_file_at(&stage.stage_dir, &name, "discarded stage member")?;
            let identity = validate_private_file(&file.metadata()?, &stage.stage_path.join(&name))?;
            members.push(ValidatedDiscardMemberV1 {
                name,
                file,
                identity,
            });
        }
    }

    // The complete deletion set is opened and identity-checked before the
    // first unlink.  A malformed later sidecar therefore cannot turn discard
    // into a misleading partial cleanup.
    for member in &members {
        require_named_file_identity(
            &stage.stage_dir,
            &member.name,
            member.identity,
            false,
            "discarded stage member",
        )?;
    }
    for member in members {
        unlink_file_at(&stage.stage_dir, &member.name)?;
        require_name_absent(&stage.stage_dir, &member.name, "discarded stage member")?;
        drop(member.file);
    }
    stage.stage_dir.sync_all()?;
    verify_record(
        &stage.stage_dir,
        INTENT_RECORD,
        &stage.intent,
        stage.intent_identity,
        &encode_intent(
            &stage.lease,
            &stage.target,
            stage.stage_identity,
            stage.spec,
        )?,
    )?;
    unlink_file_at(&stage.stage_dir, OsStr::new(INTENT_RECORD))?;
    stage.stage_dir.sync_all()?;
    remove_stage_directory(&stage.lease, &stage.stage_path, stage.stage_identity)?;
    stage.lease.parent.sync_all()?;
    Ok(stage.lease)
}

#[cfg(unix)]
fn discard_prepared(stage: PreparedFreshStoreStageV1) -> io::Result<StoreLease> {
    validate_prepared_stage(&stage)?;
    unlink_file_at(&stage.stage_dir, OsStr::new(PREPARED_RECORD))?;
    stage.stage_dir.sync_all()?;
    let PreparedFreshStoreStageV1 {
        target,
        stage_path,
        database_path,
        spec,
        lease,
        stage_dir,
        stage_identity,
        intent,
        intent_identity,
        prepared,
        main,
        initial_main_identity,
        main_identity: _,
        prepared_identity: _,
    } = stage;
    drop(prepared);
    discard_building(FreshStoreStageV1 {
        target,
        stage_path,
        database_path,
        spec,
        lease,
        stage_dir,
        stage_identity,
        intent,
        intent_identity,
        main,
        initial_main_identity,
    })
}

#[cfg(unix)]
fn cleanup_published_records(
    stage: &PreparedFreshStoreStageV1,
) -> Result<(), PublishedCleanupErrorV1> {
    fn at(
        phase: FreshStorePublishedResiduePhaseV1,
    ) -> impl FnOnce(io::Error) -> PublishedCleanupErrorV1 {
        move |source| PublishedCleanupErrorV1 { source, phase }
    }

    // Remove intent first.  If cleanup is interrupted, prepared.v1 remains a
    // complete independent binding for exact post-rename recovery.
    fail_cleanup_phase_for_test(FreshStorePublishedResiduePhaseV1::VerifyIntentRecord)
        .map_err(at(FreshStorePublishedResiduePhaseV1::VerifyIntentRecord))?;
    verify_record(
        &stage.stage_dir,
        INTENT_RECORD,
        &stage.intent,
        stage.intent_identity,
        &encode_intent(
            &stage.lease,
            &stage.target,
            stage.stage_identity,
            stage.spec,
        )
        .map_err(at(FreshStorePublishedResiduePhaseV1::VerifyIntentRecord))?,
    )
    .map_err(at(FreshStorePublishedResiduePhaseV1::VerifyIntentRecord))?;
    fail_cleanup_phase_for_test(FreshStorePublishedResiduePhaseV1::RemoveIntentRecord)
        .map_err(at(FreshStorePublishedResiduePhaseV1::RemoveIntentRecord))?;
    unlink_file_at(&stage.stage_dir, OsStr::new(INTENT_RECORD))
        .map_err(at(FreshStorePublishedResiduePhaseV1::RemoveIntentRecord))?;
    fail_cleanup_phase_for_test(FreshStorePublishedResiduePhaseV1::SyncIntentRemoval)
        .map_err(at(FreshStorePublishedResiduePhaseV1::SyncIntentRemoval))?;
    stage
        .stage_dir
        .sync_all()
        .map_err(at(FreshStorePublishedResiduePhaseV1::SyncIntentRemoval))?;

    fail_cleanup_phase_for_test(FreshStorePublishedResiduePhaseV1::VerifyPreparedRecord)
        .map_err(at(FreshStorePublishedResiduePhaseV1::VerifyPreparedRecord))?;
    require_named_file_identity(
        &stage.stage_dir,
        OsStr::new(PREPARED_RECORD),
        stage.prepared_identity,
        false,
        "prepared record",
    )
    .map_err(at(FreshStorePublishedResiduePhaseV1::VerifyPreparedRecord))?;
    fail_cleanup_phase_for_test(FreshStorePublishedResiduePhaseV1::RemovePreparedRecord)
        .map_err(at(FreshStorePublishedResiduePhaseV1::RemovePreparedRecord))?;
    unlink_file_at(&stage.stage_dir, OsStr::new(PREPARED_RECORD))
        .map_err(at(FreshStorePublishedResiduePhaseV1::RemovePreparedRecord))?;
    fail_cleanup_phase_for_test(FreshStorePublishedResiduePhaseV1::SyncPreparedRemoval)
        .map_err(at(FreshStorePublishedResiduePhaseV1::SyncPreparedRemoval))?;
    stage
        .stage_dir
        .sync_all()
        .map_err(at(FreshStorePublishedResiduePhaseV1::SyncPreparedRemoval))?;
    fail_cleanup_phase_for_test(FreshStorePublishedResiduePhaseV1::RemoveStageDirectory)
        .map_err(at(FreshStorePublishedResiduePhaseV1::RemoveStageDirectory))?;
    remove_stage_directory(&stage.lease, &stage.stage_path, stage.stage_identity)
        .map_err(at(FreshStorePublishedResiduePhaseV1::RemoveStageDirectory))?;
    fail_cleanup_phase_for_test(FreshStorePublishedResiduePhaseV1::SyncStageRemoval)
        .map_err(at(FreshStorePublishedResiduePhaseV1::SyncStageRemoval))?;
    stage
        .lease
        .parent
        .sync_all()
        .map_err(at(FreshStorePublishedResiduePhaseV1::SyncStageRemoval))?;
    Ok(())
}

#[cfg(unix)]
fn require_post_rename_guards(stage: &PreparedFreshStoreStageV1) -> io::Result<()> {
    stage.lease.require_guards(&stage.target)?;
    let target_name = stage
        .target
        .file_name()
        .ok_or_else(|| invalid_input("published fresh-store target has no filename"))?;
    require_named_file_identity(
        &stage.lease.parent,
        target_name,
        stage.main_identity,
        true,
        "published main",
    )?;
    let opened = validate_private_main(&stage.main.metadata()?, &stage.target)?;
    if opened != stage.main_identity {
        return Err(invalid_data(
            "published fresh-store main descriptor changed identity",
        ));
    }
    require_target_sidecars_absent(&stage.lease, target_name)
}

#[cfg(unix)]
fn require_published_guards(published: &PublishedFreshStoreV1) -> io::Result<()> {
    require_published_main_guards(published)?;
    let target_name = published
        .target
        .file_name()
        .ok_or_else(|| invalid_input("published fresh-store target has no filename"))?;
    require_target_sidecars_absent(&published.lease, target_name)
}

#[cfg(unix)]
fn require_published_main_guards(published: &PublishedFreshStoreV1) -> io::Result<()> {
    published.lease.require_guards(&published.target)?;
    let target_name = published
        .target
        .file_name()
        .ok_or_else(|| invalid_input("published fresh-store target has no filename"))?;
    require_named_file_stable_identity(
        &published.lease.parent,
        target_name,
        published.main_identity,
        "published main",
    )?;
    let opened = validate_private_main(&published.main.metadata()?, &published.target)?;
    if !opened.same_stable_file(published.main_identity) {
        return Err(invalid_data(
            "published fresh-store main descriptor changed stable identity",
        ));
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FreshPublishFailPointV1 {
    RenameErrorBeforeSyscall,
    RenameErrorAfterSyscall,
    RenameErrorAmbiguous,
    DurabilityBarrier,
    InitialGuard,
    Cleanup(FreshStorePublishedResiduePhaseV1),
    FinalGuard,
}

#[cfg(all(unix, test))]
std::thread_local! {
    static PUBLISH_FAILPOINT: std::cell::Cell<Option<FreshPublishFailPointV1>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(all(unix, test))]
fn arm_publish_failpoint_for_test(point: FreshPublishFailPointV1) {
    PUBLISH_FAILPOINT.with(|armed| armed.set(Some(point)));
}

#[cfg(all(unix, test))]
fn take_publish_failpoint_for_test(point: FreshPublishFailPointV1) -> bool {
    PUBLISH_FAILPOINT.with(|armed| {
        if armed.get() == Some(point) {
            armed.set(None);
            true
        } else {
            false
        }
    })
}

#[cfg(all(unix, not(test)))]
fn take_publish_failpoint_for_test(_point: FreshPublishFailPointV1) -> bool {
    false
}

#[cfg(unix)]
fn fail_publish_point_for_test(point: FreshPublishFailPointV1) -> io::Result<()> {
    if take_publish_failpoint_for_test(point) {
        Err(injected_publish_error("at a publication phase boundary"))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn fail_cleanup_phase_for_test(phase: FreshStorePublishedResiduePhaseV1) -> io::Result<()> {
    fail_publish_point_for_test(FreshPublishFailPointV1::Cleanup(phase))
}

#[cfg(unix)]
fn injected_publish_error(at: &str) -> io::Error {
    io::Error::other(format!("injected fresh-store failure {at}"))
}

#[cfg(unix)]
fn exact_target(lease: &StoreLease, requested: &Path) -> io::Result<(PathBuf, OsString)> {
    if requested.as_os_str().is_empty() || !requested.is_absolute() {
        return Err(invalid_input(
            "fresh-store target must be a nonempty absolute path",
        ));
    }
    lease.require_guards(requested)?;
    let name = requested
        .file_name()
        .ok_or_else(|| invalid_input("fresh-store target has no filename"))?
        .to_os_string();
    let canonical = lease.parent_path.join(&name);
    if canonical.as_os_str() != requested.as_os_str() {
        return Err(invalid_input(format!(
            "fresh-store target {} is not the exact canonical leased path {}",
            requested.display(),
            canonical.display()
        )));
    }
    Ok((canonical, name))
}

#[cfg(unix)]
fn stage_name(operation_id: Ulid) -> OsString {
    OsString::from(format!("{STAGE_PREFIX}{operation_id}"))
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(all(test, unix))]
mod tests;
