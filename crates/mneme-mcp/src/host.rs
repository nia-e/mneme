//! Host wiring: open a database path into a live [`Memory`] with the right
//! backend (cozo sqlite, or a JSON snapshot it falls back to), one process-shared
//! lazy inference runtime, and an `fs://` body store. Mirrors `mnemed`'s wiring — a future
//! cleanup could share one crate between the two binaries.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
#[cfg(feature = "fastembed")]
use std::time::Duration;

use mneme_body::FsStore;
use mneme_core::EmbeddingFingerprint;
use mneme_core::ports::{
    Clock, ColdPath, Embedder, EmbeddingMetadataStore, Error, GraphStore, LexicalIndex, Reranker,
    SystemClock, Traversal, VectorIndex,
};
use mneme_cozo::MemStore;
use mneme_embed::DEFAULT_DIM;
#[cfg(not(feature = "fastembed"))]
use mneme_embed::HashingEmbedder;
use mneme_engine::{Config, Memory};
use ulid::Ulid;

#[cfg(feature = "cozo")]
use mneme_cozo::CozoStore;

pub type AnyErr = Box<dyn std::error::Error + Send + Sync>;

/// One registry entry whose process lease can be handed to an offline
/// maintenance command without terminating the MCP server.
///
/// The opened handle and its lease live in the same `Arc`. A request checks out
/// that `Arc` for its complete operation, so `release` can only drop the lease
/// when the registry owns the sole strong reference. While released, ordinary
/// checkouts fail closed until an explicit `resume`; this prevents an MCP request
/// from racing a CLI command and silently reopening the database mid-maintenance.
pub struct DatabaseSlot {
    configured_path: PathBuf,
    runtime: Arc<InferenceRuntime>,
    state: Mutex<DatabaseSlotState>,
}

struct DatabaseSlotState {
    lifecycle: DatabaseLifecycle,
    /// Retained while released so catalog/status calls do not have to reopen the
    /// database merely to report its stable identity.
    known_db_id: Ulid,
    resolved_path: PathBuf,
}

enum DatabaseLifecycle {
    Open(Arc<LeasedDatabase>),
    Releasing,
    Maintenance,
    Resuming,
    Snapshotting,
    #[allow(dead_code)] // retained lease is the recovery capability, not read by normal dispatch
    SnapshotRecovery(Arc<mneme_store_path::StoreLease>),
}

struct LeasedDatabase {
    handle: DbHandle,
    /// Must be dropped after the handle: Rust drops fields in declaration order.
    /// Cozo/SQLite therefore closes before another process can acquire the lease.
    _lease: Arc<mneme_store_path::StoreLease>,
}

/// A request-scoped database checkout. Holding this value proves the process
/// lease cannot be handed off until the request releases every backend handle.
#[derive(Clone)]
pub struct DatabaseCheckout(Arc<LeasedDatabase>);

impl DatabaseCheckout {
    /// Positive current-generation admission under the retained process lease,
    /// before body publication or embedding on either write route.
    pub fn require_current_generation(&self) -> Result<(), AnyErr> {
        match &self.0.handle.saver {
            Saver::Snapshot(_) => Ok(()),
            #[cfg(feature = "cozo")]
            Saver::Cozo(_) => {
                CozoStore::require_existing_current(&self.0.handle.path, &self.0._lease)?;
                Ok(())
            }
        }
    }
}

impl Deref for DatabaseCheckout {
    type Target = DbHandle;

    fn deref(&self) -> &Self::Target {
        &self.0.handle
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseSlotStatus {
    pub state: &'static str,
    pub db_id: Ulid,
    pub configured_path: PathBuf,
    pub resolved_path: PathBuf,
    /// Request-scoped handle checkouts. Detached backend jobs are reported
    /// separately because cancellation can end the former before the latter.
    pub in_flight: usize,
    pub backend_jobs: usize,
}

/// Cancellation-safe blocking half of a maintenance release. Construction has
/// already fenced new checkouts; moving this value into `spawn_blocking` keeps
/// the live handle and OS lease owned until checkpointing really finishes.
pub struct DatabaseRelease {
    slot: Arc<DatabaseSlot>,
    opened: Option<Arc<LeasedDatabase>>,
    /// Held from the synchronous admission fence through backend close. This
    /// also covers cancellation-surviving engine tasks before they enter the
    /// adapter's own activity counter.
    _mutation_quiescence: mneme_engine::MutationQuiescenceGuard,
}

/// Cancellation-safe blocking half of a maintenance resume. Construction has
/// already installed a fail-closed transition state.
pub struct DatabaseResume {
    slot: Arc<DatabaseSlot>,
    feedback_epoch: String,
    expected_db_id: Option<Ulid>,
    armed: bool,
}

/// Synchronous, cancellation-safe snapshot work. The worker retains the exact
/// source lease even while SQLite's connection pool is closed for copying.
pub struct DatabaseSnapshot {
    slot: Arc<DatabaseSlot>,
    opened: Option<Arc<LeasedDatabase>>,
    lease: Arc<mneme_store_path::StoreLease>,
    feedback_epoch: String,
    prepared: bool,
    _mutation_quiescence: mneme_engine::MutationQuiescenceGuard,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DatabaseSnapshotResult {
    pub bundle: PathBuf,
    pub db_id: Ulid,
    pub generation: Ulid,
}

#[derive(Clone, Copy)]
enum SnapshotCut {
    None,
    #[cfg(test)]
    AfterClose,
    #[cfg(test)]
    AfterCopy,
    #[cfg(test)]
    AfterRename,
}

impl DatabaseSlot {
    /// Fence all new requests before handing this worker to `spawn_blocking`.
    /// Callers must fence active walks/receipts and rotate the feedback epoch
    /// under their session ledger lock before invoking this method.
    #[cfg(test)]
    pub fn begin_snapshot(
        self: &Arc<Self>,
        feedback_epoch: String,
    ) -> Result<DatabaseSnapshot, AnyErr> {
        self.begin_snapshot_guarded(feedback_epoch, None)
    }

    pub fn begin_snapshot_guarded(
        self: &Arc<Self>,
        feedback_epoch: String,
        expected_db_id: Option<Ulid>,
    ) -> Result<DatabaseSnapshot, AnyErr> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "database slot mutex poisoned")?;
        super::require_expected_db_id("selected slot", state.known_db_id, expected_db_id)?;
        let DatabaseLifecycle::Open(opened) = &state.lifecycle else {
            return Err("database is not open for snapshot".into());
        };
        if Arc::strong_count(opened) != 1 {
            return Err(
                "database has in-flight MCP operations; retry snapshot after they finish".into(),
            );
        }
        if opened.handle.backend_jobs_in_flight() != 0 {
            return Err(
                "database has detached backend jobs; retry snapshot after they finish".into(),
            );
        }
        let quiescence =
            opened.handle.mem.try_mutation_quiescence().ok_or(
                "database has an engine mutation in flight; retry snapshot after it finishes",
            )?;
        let lease = opened._lease.clone();
        let DatabaseLifecycle::Open(opened) =
            std::mem::replace(&mut state.lifecycle, DatabaseLifecycle::Snapshotting)
        else {
            unreachable!()
        };
        Ok(DatabaseSnapshot {
            slot: self.clone(),
            opened: Some(opened),
            lease,
            feedback_epoch,
            prepared: false,
            _mutation_quiescence: quiescence,
        })
    }

    /// Open a registry slot under the exact volatile feedback authority shared
    /// with the MCP session ledger.
    pub fn open(
        configured_path: PathBuf,
        runtime: Arc<InferenceRuntime>,
        feedback_epoch: String,
    ) -> Result<Self, AnyErr> {
        let (resolved_path, opened) =
            open_leased(&configured_path, &runtime, &feedback_epoch, None)?;
        let known_db_id = opened.handle.db_id;
        Ok(Self {
            configured_path,
            runtime,
            state: Mutex::new(DatabaseSlotState {
                lifecycle: DatabaseLifecycle::Open(Arc::new(opened)),
                known_db_id,
                resolved_path,
            }),
        })
    }

    /// Check out the live handle, or fail closed during a maintenance handoff.
    pub fn checkout(&self) -> Result<DatabaseCheckout, AnyErr> {
        let state = self
            .state
            .lock()
            .map_err(|_| "database slot mutex poisoned")?;
        match &state.lifecycle {
            DatabaseLifecycle::Open(opened) => Ok(DatabaseCheckout(opened.clone())),
            DatabaseLifecycle::Releasing => {
                Err("database is entering offline maintenance; retry after release completes".into())
            }
            DatabaseLifecycle::Maintenance => Err(
                "database is released for offline maintenance; finish the CLI operation, then call database_control action=resume"
                    .to_string()
                    .into(),
            ),
            DatabaseLifecycle::Resuming => {
                Err("database is resuming after offline maintenance; retry shortly".into())
            }
            DatabaseLifecycle::Snapshotting => Err("database snapshot is in progress; retry shortly".into()),
            DatabaseLifecycle::SnapshotRecovery(_) => Err("database snapshot source could not reopen; inspect the source, then restart mneme-mcp for recovery".into()),
        }
    }

    /// Atomically match a remote edge's stable target identity and pin the exact
    /// open handle that supplied it. A selector refresh cannot swap the slot to
    /// another database between identity lookup and checkout.
    ///
    /// `None` means this slot names another database. `Some(None)` means the id
    /// matches the retained catalog identity but the target is maintenance-fenced.
    pub fn checkout_if_db_id(
        &self,
        expected: Ulid,
    ) -> Result<Option<Option<DatabaseCheckout>>, AnyErr> {
        let state = self
            .state
            .lock()
            .map_err(|_| "database slot mutex poisoned")?;
        if state.known_db_id != expected {
            return Ok(None);
        }
        match &state.lifecycle {
            DatabaseLifecycle::Open(opened) => {
                debug_assert_eq!(opened.handle.db_id, expected);
                Ok(Some(Some(DatabaseCheckout(opened.clone()))))
            }
            DatabaseLifecycle::Releasing
            | DatabaseLifecycle::Maintenance
            | DatabaseLifecycle::Resuming
            | DatabaseLifecycle::Snapshotting
            | DatabaseLifecycle::SnapshotRecovery(_) => Ok(Some(None)),
        }
    }

    pub fn status(&self) -> Result<DatabaseSlotStatus, AnyErr> {
        let state = self
            .state
            .lock()
            .map_err(|_| "database slot mutex poisoned")?;
        let (name, in_flight, backend_jobs) = match &state.lifecycle {
            DatabaseLifecycle::Open(opened) => (
                "open",
                Arc::strong_count(opened).saturating_sub(1),
                opened.handle.backend_jobs_in_flight(),
            ),
            DatabaseLifecycle::Releasing => ("releasing", 0, 0),
            DatabaseLifecycle::Maintenance => ("maintenance", 0, 0),
            DatabaseLifecycle::Resuming => ("resuming", 0, 0),
            DatabaseLifecycle::Snapshotting => ("snapshotting", 0, 0),
            DatabaseLifecycle::SnapshotRecovery(_) => ("snapshot_recovery", 0, 0),
        };
        Ok(DatabaseSlotStatus {
            state: name,
            db_id: state.known_db_id,
            configured_path: self.configured_path.clone(),
            resolved_path: state.resolved_path.clone(),
            in_flight,
            backend_jobs,
        })
    }

    /// Atomically fence new checkouts and capture the only live handle for a
    /// blocking checkpoint. The caller must do this while session state is
    /// locked, then move the returned work item directly into `spawn_blocking`.
    #[cfg(any(test, feature = "http"))]
    pub fn begin_release(self: &Arc<Self>) -> Result<DatabaseRelease, AnyErr> {
        self.begin_release_guarded(None)
    }

    pub fn begin_release_guarded(
        self: &Arc<Self>,
        expected_db_id: Option<Ulid>,
    ) -> Result<DatabaseRelease, AnyErr> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "database slot mutex poisoned")?;
        super::require_expected_db_id("selected slot", state.known_db_id, expected_db_id)?;
        let DatabaseLifecycle::Open(opened) = &state.lifecycle else {
            return Err(match &state.lifecycle {
                DatabaseLifecycle::Releasing => "database release is already in progress",
                DatabaseLifecycle::Maintenance => {
                    "database is already released for offline maintenance"
                }
                DatabaseLifecycle::Resuming => "database resume is in progress",
                DatabaseLifecycle::Snapshotting => "database snapshot is in progress",
                DatabaseLifecycle::SnapshotRecovery(_) => {
                    "database snapshot requires operator recovery"
                }
                DatabaseLifecycle::Open(_) => unreachable!(),
            }
            .into());
        };
        let in_flight = Arc::strong_count(opened).saturating_sub(1);
        if in_flight != 0 {
            return Err(format!(
                "database has {in_flight} in-flight MCP operation(s); retry maintenance release after they finish"
            )
            .into());
        }
        let backend_jobs = opened.handle.backend_jobs_in_flight();
        if backend_jobs != 0 {
            return Err(format!(
                "database has {backend_jobs} detached backend job(s) still running after request cancellation; retry maintenance release after backend quiescence"
            )
            .into());
        }
        let mutation_quiescence = opened
            .handle
            .mem
            .try_mutation_quiescence()
            .ok_or(
                "database has an engine mutation or latent maintenance task still in flight; retry maintenance release after it finishes",
            )?;

        debug_assert_eq!(Arc::strong_count(opened), 1);
        let DatabaseLifecycle::Open(opened) =
            std::mem::replace(&mut state.lifecycle, DatabaseLifecycle::Releasing)
        else {
            unreachable!("validated open lifecycle under the same lock")
        };
        Ok(DatabaseRelease {
            slot: self.clone(),
            opened: Some(opened),
            _mutation_quiescence: mutation_quiescence,
        })
    }

    /// Install the fail-closed resume transition before blocking on path
    /// resolution, lease acquisition, schema checks, and backend open.
    #[cfg(test)]
    pub fn begin_resume(
        self: &Arc<Self>,
        feedback_epoch: String,
    ) -> Result<DatabaseResume, AnyErr> {
        self.begin_resume_guarded(feedback_epoch, None)
    }

    pub fn begin_resume_guarded(
        self: &Arc<Self>,
        feedback_epoch: String,
        expected_db_id: Option<Ulid>,
    ) -> Result<DatabaseResume, AnyErr> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "database slot mutex poisoned")?;
        super::require_expected_db_id("selected slot", state.known_db_id, expected_db_id)?;
        match &state.lifecycle {
            DatabaseLifecycle::Maintenance => {}
            DatabaseLifecycle::Open(_) => {
                return Err("database is already open in the MCP server".into());
            }
            DatabaseLifecycle::Releasing => {
                return Err("database release is still in progress".into());
            }
            DatabaseLifecycle::Resuming => {
                return Err("database resume is already in progress".into());
            }
            DatabaseLifecycle::Snapshotting => {
                return Err("database snapshot is in progress".into());
            }
            DatabaseLifecycle::SnapshotRecovery(_) => {
                return Err("database snapshot requires operator recovery".into());
            }
        }
        state.lifecycle = DatabaseLifecycle::Resuming;
        Ok(DatabaseResume {
            slot: self.clone(),
            feedback_epoch,
            expected_db_id,
            armed: true,
        })
    }
}

impl DatabaseRelease {
    /// Checkpoint, close the backend, drop its OS lease, then publish the
    /// maintenance state. If checkpointing fails, `Drop` restores the open slot.
    pub fn finish(mut self) -> Result<DatabaseSlotStatus, AnyErr> {
        self.opened
            .as_ref()
            .expect("release work owns its live database")
            .handle
            .prepare_for_lease_release()?;

        // Keep the transition fence installed while the handle and lease close.
        // Once they are gone an offline process may safely acquire ownership.
        drop(self.opened.take());
        let mut state = self
            .slot
            .state
            .lock()
            .map_err(|_| "database slot mutex poisoned")?;
        if !matches!(state.lifecycle, DatabaseLifecycle::Releasing) {
            return Err("database release transition was lost".into());
        }
        state.lifecycle = DatabaseLifecycle::Maintenance;
        Ok(DatabaseSlotStatus {
            state: "maintenance",
            db_id: state.known_db_id,
            configured_path: self.slot.configured_path.clone(),
            resolved_path: state.resolved_path.clone(),
            in_flight: 0,
            backend_jobs: 0,
        })
    }
}

impl Drop for DatabaseRelease {
    fn drop(&mut self) {
        let Some(opened) = self.opened.take() else {
            return;
        };
        if let Ok(mut state) = self.slot.state.lock()
            && matches!(state.lifecycle, DatabaseLifecycle::Releasing)
        {
            state.lifecycle = DatabaseLifecycle::Open(opened);
        }
    }
}

impl DatabaseResume {
    /// Re-resolve the configured selector and reacquire the exclusive lease.
    /// Failure leaves the maintenance fence in place. The complete operation is
    /// synchronous so callers can move it onto Tokio's blocking pool.
    pub fn finish(mut self) -> Result<DatabaseSlotStatus, AnyErr> {
        let opened = open_leased(
            &self.slot.configured_path,
            &self.slot.runtime,
            &self.feedback_epoch,
            self.expected_db_id,
        )?;
        let (resolved_path, opened) = opened;
        // A guarded resume may re-resolve a selector, but cannot publish a
        // replacement owner under a retained old identity. Drop keeps it fenced.
        super::require_expected_db_id("resumed slot", opened.handle.db_id, self.expected_db_id)?;
        let mut state = self
            .slot
            .state
            .lock()
            .map_err(|_| "database slot mutex poisoned")?;
        if !matches!(state.lifecycle, DatabaseLifecycle::Resuming) {
            return Err("database resume transition was lost".into());
        }
        state.known_db_id = opened.handle.db_id;
        state.resolved_path = resolved_path;
        state.lifecycle = DatabaseLifecycle::Open(Arc::new(opened));
        self.armed = false;
        Ok(DatabaseSlotStatus {
            state: "open",
            db_id: state.known_db_id,
            configured_path: self.slot.configured_path.clone(),
            resolved_path: state.resolved_path.clone(),
            in_flight: 0,
            backend_jobs: 0,
        })
    }
}

impl Drop for DatabaseResume {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut state) = self.slot.state.lock()
            && matches!(state.lifecycle, DatabaseLifecycle::Resuming)
        {
            state.lifecycle = DatabaseLifecycle::Maintenance;
        }
    }
}

impl DatabaseSnapshot {
    pub fn finish(self) -> Result<DatabaseSnapshotResult, AnyErr> {
        self.finish_inner(SnapshotCut::None)
    }

    #[cfg(test)]
    fn finish_with_cut(self, cut: SnapshotCut) -> Result<DatabaseSnapshotResult, AnyErr> {
        self.finish_inner(cut)
    }

    fn finish_inner(mut self, cut: SnapshotCut) -> Result<DatabaseSnapshotResult, AnyErr> {
        let opened = self.opened.as_ref().expect("snapshot owns open database");
        // A database created from a configured path can initially retain an
        // alias through a symlinked ancestor (notably macOS /var -> /private/var).
        // Existing-only SQLite admission requires the canonical source spelling.
        let path = std::fs::canonicalize(&opened.handle.path)?;
        // Managed generations have a separate selector/publication authority.
        // Do not pretend that copying their resolved SQLite path captures it.
        if path
            .components()
            .any(|part| part.as_os_str() == "generations")
        {
            return Err("snapshot of a managed generation is not supported".into());
        }
        if !path.is_file() {
            return Err("snapshot requires an existing regular source database".into());
        }
        self.lease.require_guards(&path)?;
        let db_id = opened.handle.db_id;
        let generation = Ulid::new();
        let fingerprint = runtime_fingerprint(
            self.slot
                .runtime
                .embedder(opened.handle.snapshot_dim())
                .as_ref(),
        )?;
        let bodies = opened.handle.snapshot_body_refs()?;
        let pending =
            super::snapshot::PendingSnapshot::new(&path, db_id, generation, fingerprint, bodies)?;

        // `prepare_for_file_move` can dismantle Cozo's connection pool even on
        // error. Mark the boundary *before* calling it: Drop must never restore
        // this potentially poisoned handle to normal service.
        self.prepared = true;
        let prepare = opened.handle.prepare_for_lease_release();
        drop(self.opened.take());
        super::snapshot::boundary(super::snapshot::SnapshotBoundary::AfterSourceClose);
        let copy = prepare.and_then(|_| {
            #[cfg(test)]
            if matches!(cut, SnapshotCut::AfterClose) {
                return Err("injected snapshot failure after source close".into());
            }
            let copied = pending.copy_source()?;
            super::snapshot::boundary(super::snapshot::SnapshotBoundary::AfterPayloadCopy);
            #[cfg(test)]
            if matches!(cut, SnapshotCut::AfterCopy) {
                return Err("injected snapshot failure after payload copy".into());
            }
            Ok(copied)
        });
        #[cfg(not(test))]
        let _ = cut;
        let reopened = open(
            &path,
            &self.slot.runtime,
            self.lease.clone(),
            &self.feedback_epoch,
        );
        match reopened {
            Ok(handle) => {
                let mut state = self
                    .slot
                    .state
                    .lock()
                    .map_err(|_| "database slot mutex poisoned")?;
                if !matches!(state.lifecycle, DatabaseLifecycle::Snapshotting) {
                    return Err("database snapshot transition was lost".into());
                }
                state.resolved_path = path.clone();
                state.lifecycle = DatabaseLifecycle::Open(Arc::new(LeasedDatabase {
                    handle,
                    _lease: self.lease.clone(),
                }));
                super::snapshot::boundary(super::snapshot::SnapshotBoundary::AfterSourceReopen);
            }
            Err(error) => {
                let mut state = self
                    .slot
                    .state
                    .lock()
                    .map_err(|_| "database slot mutex poisoned")?;
                state.lifecycle = DatabaseLifecycle::SnapshotRecovery(self.lease.clone());
                return Err(format!(
                    "snapshot source reopen failed; slot is fenced for recovery: {error}"
                )
                .into());
            }
        }
        let copied = copy?;
        #[cfg(test)]
        if matches!(cut, SnapshotCut::AfterRename) {
            return copied
                .verify_and_publish_with_post_rename(|| {
                    Err("injected snapshot failure after rename before final sync".into())
                })
                .map(|bundle| DatabaseSnapshotResult {
                    bundle,
                    db_id,
                    generation,
                });
        }
        let bundle = copied.verify_and_publish()?;
        Ok(DatabaseSnapshotResult {
            bundle,
            db_id,
            generation,
        })
    }
}

impl Drop for DatabaseSnapshot {
    fn drop(&mut self) {
        if self.prepared {
            if let Ok(mut state) = self.slot.state.lock()
                && matches!(state.lifecycle, DatabaseLifecycle::Snapshotting)
            {
                state.lifecycle = DatabaseLifecycle::SnapshotRecovery(self.lease.clone());
            }
            return;
        }
        // The dispatcher committed a new volatile epoch at admission. Merely
        // restoring the old handle would leave its backend epoch behind the
        // ledger. Even a preflight refusal therefore closes and reopens under
        // the retained lease; a failed close/reopen stays fenced.
        let Some(opened) = self.opened.take() else {
            return;
        };
        let old_path = opened.handle.path.clone();
        let _ = opened.handle.prepare_for_lease_release();
        drop(opened);
        let reopened = std::fs::canonicalize(&old_path)
            .map_err(|error| error.into())
            .and_then(|path| {
                open(
                    &path,
                    &self.slot.runtime,
                    self.lease.clone(),
                    &self.feedback_epoch,
                )
                .map(|handle| (path, handle))
            });
        if let Ok(mut state) = self.slot.state.lock()
            && matches!(state.lifecycle, DatabaseLifecycle::Snapshotting)
        {
            match reopened {
                Ok((path, handle)) => {
                    state.resolved_path = path;
                    state.lifecycle = DatabaseLifecycle::Open(Arc::new(LeasedDatabase {
                        handle,
                        _lease: self.lease.clone(),
                    }));
                }
                Err(_) => state.lifecycle = DatabaseLifecycle::SnapshotRecovery(self.lease.clone()),
            }
        }
    }
}

fn open_leased(
    configured_path: &Path,
    runtime: &InferenceRuntime,
    feedback_epoch: &str,
    expected_db_id: Option<Ulid>,
) -> Result<(PathBuf, LeasedDatabase), AnyErr> {
    let path = mneme_store_path::resolve_configured_store_path(configured_path).map_err(|error| {
        format!(
            "resolving configured db {configured_path:?} through the mneme store selector: {error}"
        )
    })?;
    if expected_db_id.is_some() && !path.is_file() {
        return Err("guarded resume requires an existing database; target is absent or changed, do not recreate or retry against a replacement".into());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let lease = Arc::new(mneme_store_path::StoreLease::acquire(&path)?);
    let handle = open_guarded(
        &path,
        runtime,
        lease.clone(),
        feedback_epoch,
        expected_db_id,
    )
    .map_err(|error| format!("opening database {path:?}: {error}"))?;
    Ok((
        path,
        LeasedDatabase {
            handle,
            _lease: lease,
        },
    ))
}

/// FastEmbed configures ONNX with its own CPU worker pool. One in-flight model
/// call per MCP process avoids multiplying that pool under concurrent HTTP
/// requests while still letting non-inference requests run independently.
#[cfg(feature = "fastembed")]
const MAX_CONCURRENT_INFERENCE: usize = 1;
/// Includes the executing request: one model call plus at most three bounded
/// waiters. A fifth inference request fails immediately instead of joining an
/// unbounded semaphore queue.
#[cfg(feature = "fastembed")]
const MAX_ADMITTED_INFERENCE: usize = 4;
#[cfg(feature = "fastembed")]
const INFERENCE_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);

/// Two-stage process-wide admission. `admission` bounds total retained request
/// state while `execution` protects the ONNX worker pool itself.
#[cfg(feature = "fastembed")]
struct InferenceGate {
    admission: Arc<tokio::sync::Semaphore>,
    execution: Arc<tokio::sync::Semaphore>,
    admitted_limit: usize,
    wait_timeout: Duration,
}

/// Cloneable so a blocking task keeps its request admitted even if the async
/// caller is cancelled while ONNX is still running.
#[cfg(feature = "fastembed")]
#[derive(Clone)]
struct InferenceAdmission {
    _permit: Arc<tokio::sync::OwnedSemaphorePermit>,
}

#[cfg(feature = "fastembed")]
struct InferenceExecution {
    _admission: InferenceAdmission,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[cfg(feature = "fastembed")]
impl InferenceGate {
    fn new(admitted_limit: usize, execution_limit: usize, wait_timeout: Duration) -> Self {
        assert!(
            admitted_limit > 0,
            "inference admission limit must be positive"
        );
        assert!(
            execution_limit > 0,
            "inference execution limit must be positive"
        );
        assert!(
            execution_limit <= admitted_limit,
            "inference execution limit cannot exceed admission limit"
        );
        Self {
            admission: Arc::new(tokio::sync::Semaphore::new(admitted_limit)),
            execution: Arc::new(tokio::sync::Semaphore::new(execution_limit)),
            admitted_limit,
            wait_timeout,
        }
    }

    fn admit(&self) -> mneme_core::ports::Result<InferenceAdmission> {
        match self.admission.clone().try_acquire_owned() {
            Ok(permit) => Ok(InferenceAdmission {
                _permit: Arc::new(permit),
            }),
            Err(tokio::sync::TryAcquireError::NoPermits) => Err(Error::CapacityExceeded {
                resource: "inference requests",
                limit: self.admitted_limit,
            }),
            Err(tokio::sync::TryAcquireError::Closed) => {
                Err(Error::Backend("inference admission gate closed".into()))
            }
        }
    }

    async fn execute(
        &self,
        admission: &InferenceAdmission,
    ) -> mneme_core::ports::Result<InferenceExecution> {
        let permit =
            tokio::time::timeout(self.wait_timeout, self.execution.clone().acquire_owned())
                .await
                .map_err(|_| {
                    Error::Backend(format!(
                        "inference execution queue timed out after {:?}",
                        self.wait_timeout
                    ))
                })?
                .map_err(|_| Error::Backend("inference execution gate closed".into()))?;
        Ok(InferenceExecution {
            _admission: admission.clone(),
            _permit: permit,
        })
    }
}

/// Process-wide model runtime shared by every database handle in the registry.
/// The expensive ONNX sessions remain lazy, but there is only one lazy cell for
/// the embedder and (when enabled) one for the reranker across the whole server.
pub struct InferenceRuntime {
    #[cfg(feature = "fastembed")]
    embedder: Arc<LazyEmbedder>,
    #[cfg(feature = "fastembed")]
    reranker: Option<Arc<LazyReranker>>,
}

impl InferenceRuntime {
    pub fn new() -> Self {
        #[cfg(feature = "fastembed")]
        {
            Self::new_fast(rerank_enabled())
        }

        #[cfg(not(feature = "fastembed"))]
        Self {}
    }

    #[cfg(feature = "fastembed")]
    fn new_fast(enable_reranker: bool) -> Self {
        let gate = Arc::new(InferenceGate::new(
            MAX_ADMITTED_INFERENCE,
            MAX_CONCURRENT_INFERENCE,
            INFERENCE_QUEUE_TIMEOUT,
        ));
        Self {
            embedder: Arc::new(LazyEmbedder {
                cell: tokio::sync::OnceCell::new(),
                gate: gate.clone(),
            }),
            reranker: enable_reranker.then(|| {
                Arc::new(LazyReranker {
                    cell: tokio::sync::OnceCell::new(),
                    gate,
                })
            }),
        }
    }

    fn embedder(&self, dim: usize) -> Arc<dyn Embedder> {
        #[cfg(feature = "fastembed")]
        {
            let _ = dim;
            let embedder: Arc<dyn Embedder> = self.embedder.clone();
            embedder
        }
        #[cfg(not(feature = "fastembed"))]
        {
            Arc::new(HashingEmbedder::new(dim))
        }
    }

    fn reranker(&self) -> Option<Arc<dyn Reranker>> {
        #[cfg(feature = "fastembed")]
        {
            self.reranker
                .as_ref()
                .map(|reranker| reranker.clone() as Arc<dyn Reranker>)
        }
        #[cfg(not(feature = "fastembed"))]
        {
            None
        }
    }
}

impl Default for InferenceRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// An opened database: the engine plus its stable id and a way to persist.
pub struct DbHandle {
    pub mem: Memory,
    pub db_id: Ulid,
    /// Where this db lives on disk — used to resolve the enclosing git working
    /// tree (if any) so new memories can be stamped with their origin commit.
    path: PathBuf,
    saver: Saver,
    /// Cozo moves synchronous queries to Tokio's blocking pool. A cancelled
    /// caller can drop its request checkout while that closure keeps running,
    /// so maintenance release must fence this independent lifetime too.
    #[cfg(feature = "cozo")]
    backend_activity: Option<mneme_cozo::BackendActivity>,
    #[cfg(all(test, feature = "fastembed"))]
    inference_embedder: Arc<dyn Embedder>,
}

impl DbHandle {
    /// Borrow the optional advisory lane under this already admitted checkout.
    /// This does not open a store, extend authority, or detach lease ownership.
    pub(crate) fn concerns(&self) -> Option<&dyn mneme_core::ports::ConcernStore> {
        match &self.saver {
            Saver::Snapshot(saver) => saver.store.concerns(),
            #[cfg(feature = "cozo")]
            Saver::Cozo(store) => store.concerns(),
        }
    }

    fn snapshot_dim(&self) -> usize {
        match &self.saver {
            Saver::Snapshot(saver) => saver.store.dim(),
            #[cfg(feature = "cozo")]
            Saver::Cozo(store) => store.dim(),
        }
    }

    fn snapshot_body_refs(&self) -> Result<Vec<String>, AnyErr> {
        let graph: &dyn GraphStore = match &self.saver {
            Saver::Snapshot(saver) => saver.store.as_ref(),
            #[cfg(feature = "cozo")]
            Saver::Cozo(store) => store.as_ref(),
        };
        // A whole-graph Vec can turn snapshot admission into unbounded RSS.
        // Page the canonical source while the mutation gate is held and cap
        // both node count and retained reference text.
        const MAX_SNAPSHOT_NODES: usize = 100_000;
        const MAX_REF_BYTES: usize = 8 * 1024 * 1024;
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        runtime.block_on(async {
            let cold = ColdPath::acquire();
            let Some(through) = graph.maintenance_node_upper_bound(cold).await? else {
                return Ok(Vec::new());
            };
            let mut refs = Vec::new();
            let mut bytes = 0usize;
            let mut after = None;
            loop {
                let page = graph
                    .maintenance_nodes_page(cold, after, through, 64)
                    .await?;
                for node in page.items {
                    if refs.len() >= MAX_SNAPSHOT_NODES {
                        return Err(
                            "snapshot canonical node inventory exceeds its 100000-node limit"
                                .into(),
                        );
                    }
                    let body = node.body().as_str();
                    bytes = bytes
                        .checked_add(body.len())
                        .ok_or("snapshot body-ref inventory overflow")?;
                    if bytes > MAX_REF_BYTES {
                        return Err("snapshot body-ref inventory exceeds its 8 MiB limit".into());
                    }
                    refs.push(body.to_owned());
                }
                match page.next {
                    Some(next) => after = Some(next),
                    None => return Ok(refs),
                }
            }
        })
    }
    /// Persist after a mutation (a no-op for the write-through cozo backend).
    pub fn save(&self) -> Result<(), AnyErr> {
        self.saver.save()
    }

    /// Close/checkpoint backend state before dropping the process lease.
    fn prepare_for_lease_release(&self) -> Result<(), AnyErr> {
        self.saver.prepare_for_lease_release()
    }

    /// The git HEAD of the working tree this db lives in, or `None` if it isn't in
    /// one (e.g. the global user store). Resolved fresh per call so a memory formed
    /// after a mid-session commit records the new HEAD. Stamped onto ingested nodes
    /// as their [temporal origin](mneme_core::Node::origin_commit).
    pub fn origin_commit(&self) -> Option<String> {
        self.path.parent().and_then(git_head)
    }

    fn backend_jobs_in_flight(&self) -> usize {
        #[cfg(feature = "cozo")]
        {
            self.backend_activity
                .as_ref()
                .map_or(0, mneme_cozo::BackendActivity::in_flight)
        }
        #[cfg(not(feature = "cozo"))]
        {
            0
        }
    }

    #[cfg(all(test, feature = "fastembed"))]
    fn shares_embedder_runtime(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inference_embedder, &other.inference_embedder)
    }
}

/// Resolve the git HEAD sha of the repo containing `dir`, or `None` if `dir` isn't
/// inside a git working tree (or git isn't installed). Shells out to `git` so
/// packed refs, linked worktrees, and detached HEAD all resolve correctly rather
/// than reimplementing `.git/HEAD` parsing.
fn git_head(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

struct SnapshotSaver {
    store: Arc<MemStore>,
    path: PathBuf,
    /// A checkpoint must be ordered from export through the final atomic rename.
    /// The engine mutation gate is released before the host persists, so without
    /// this lock an older export can otherwise rename over a newer completed one.
    checkpoint_gate: Mutex<()>,
}

impl SnapshotSaver {
    fn new(store: Arc<MemStore>, path: PathBuf) -> Self {
        Self {
            store,
            path,
            checkpoint_gate: Mutex::new(()),
        }
    }

    fn save(&self) -> Result<(), AnyErr> {
        self.save_with(|store, path| {
            store.save(path)?;
            Ok(())
        })
    }

    /// Keep the complete checkpoint operation inside one critical section. The
    /// injected operation makes the ordering guarantee directly testable without
    /// weakening `MemStore`'s production atomic-write implementation.
    fn save_with(
        &self,
        operation: impl FnOnce(&MemStore, &Path) -> Result<(), AnyErr>,
    ) -> Result<(), AnyErr> {
        let _checkpoint = self
            .checkpoint_gate
            .lock()
            .map_err(|_| "snapshot checkpoint mutex poisoned")?;
        operation(&self.store, &self.path)
    }
}

enum Saver {
    Snapshot(SnapshotSaver),
    #[cfg(feature = "cozo")]
    Cozo(Arc<CozoStore>),
}

impl Saver {
    fn save(&self) -> Result<(), AnyErr> {
        match self {
            Saver::Snapshot(saver) => saver.save(),
            #[cfg(feature = "cozo")]
            Saver::Cozo(_) => Ok(()),
        }
    }

    fn prepare_for_lease_release(&self) -> Result<(), AnyErr> {
        match self {
            Saver::Snapshot(saver) => saver.save(),
            #[cfg(feature = "cozo")]
            Saver::Cozo(store) => store.prepare_for_file_move().map_err(Into::into),
        }
    }
}

/// The unresolved XDG per-user database path used by the registry default.
/// Registry construction sends this through `mneme-store-path`, which handles
/// legacy snapshots and native generation activation consistently with the CLI.
pub fn user_db() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("mneme/memory.db")
}

fn open(
    db: &Path,
    runtime: &InferenceRuntime,
    lease: Arc<mneme_store_path::StoreLease>,
    feedback_epoch: &str,
) -> Result<DbHandle, AnyErr> {
    open_guarded(db, runtime, lease, feedback_epoch, None)
}

fn open_guarded(
    db: &Path,
    runtime: &InferenceRuntime,
    lease: Arc<mneme_store_path::StoreLease>,
    feedback_epoch: &str,
    expected_db_id: Option<Ulid>,
) -> Result<DbHandle, AnyErr> {
    if expected_db_id.is_some() && !db.is_file() {
        return Err(
            "guarded resume requires an existing database; target is absent or changed".into(),
        );
    }
    if let Some(parent) = db.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    open_inner(db, runtime, lease, feedback_epoch, expected_db_id)
}

#[cfg(not(feature = "cozo"))]
fn open_inner(
    db: &Path,
    runtime: &InferenceRuntime,
    _lease: Arc<mneme_store_path::StoreLease>,
    _feedback_epoch: &str,
    expected_db_id: Option<Ulid>,
) -> Result<DbHandle, AnyErr> {
    open_snapshot(db, runtime, expected_db_id)
}

#[cfg(feature = "cozo")]
fn open_inner(
    db: &Path,
    runtime: &InferenceRuntime,
    lease: Arc<mneme_store_path::StoreLease>,
    feedback_epoch: &str,
    expected_db_id: Option<Ulid>,
) -> Result<DbHandle, AnyErr> {
    // An existing JSON snapshot opens with the reference backend; otherwise use
    // (or create) the persistent cozo sqlite db.
    if db.exists() && is_snapshot(db)? {
        return open_snapshot(db, runtime, expected_db_id);
    }
    // Silent-amnesia guard: opening the cozo backend while a legacy JSON snapshot
    // sits beside it means that snapshot's memories are NOT loaded — a memory
    // system that recalls nothing looks identical to one with nothing to recall.
    // `migrate` folds the snapshot into the sqlite db; until then, say so loudly.
    if let Some(legacy) = legacy_snapshot_beside(db)? {
        eprintln!(
            "mneme-mcp: WARNING: opened cozo db {db:?} but a legacy snapshot {legacy:?} is present — \
             its memories are NOT loaded. Run `mnemed migrate` (pointed at {db:?}) to fold it in."
        );
    }
    let store = Arc::new(if expected_db_id.is_some() {
        CozoStore::open_existing_persistent(db, DEFAULT_DIM, lease)?
    } else {
        CozoStore::open_persistent(db, DEFAULT_DIM, lease)?
    });
    let db_id = store.db_id();
    // Classification and identity precede the first host-authority write,
    // embedding preparation, and body-store construction.
    super::require_expected_db_id("resumed store", db_id, expected_db_id)?;
    store.activate_feedback_epoch(feedback_epoch)?;
    let backend_activity = store.backend_activity();
    let dim = store.dim();
    let embedder = runtime.embedder(dim);
    #[cfg(all(test, feature = "fastembed"))]
    let inference_embedder = embedder.clone();
    prepare_embedding_store(store.as_ref(), embedder.as_ref(), db)?;
    let mem = assemble(
        (store.clone(), store.clone(), store.clone(), store.clone()),
        embedder,
        runtime.reranker(),
        &bodies_dir(db),
    )?;
    Ok(DbHandle {
        mem,
        db_id,
        path: db.to_path_buf(),
        saver: Saver::Cozo(store),
        backend_activity: Some(backend_activity),
        #[cfg(all(test, feature = "fastembed"))]
        inference_embedder,
    })
}

fn open_snapshot(
    db: &Path,
    runtime: &InferenceRuntime,
    expected_db_id: Option<Ulid>,
) -> Result<DbHandle, AnyErr> {
    let store = if db.exists() {
        Arc::new(MemStore::load(db)?)
    } else if expected_db_id.is_some() {
        return Err("guarded resume requires an existing snapshot; target disappeared".into());
    } else {
        Arc::new(MemStore::new(DEFAULT_DIM))
    };
    let db_id = store.db_id();
    super::require_expected_db_id("resumed snapshot", db_id, expected_db_id)?;
    let dim = store.dim();
    let embedder = runtime.embedder(dim);
    #[cfg(all(test, feature = "fastembed"))]
    let inference_embedder = embedder.clone();
    prepare_embedding_store(store.as_ref(), embedder.as_ref(), db)?;
    let mem = assemble(
        (store.clone(), store.clone(), store.clone(), store.clone()),
        embedder,
        runtime.reranker(),
        &bodies_dir(db),
    )?;
    Ok(DbHandle {
        mem,
        db_id,
        path: db.to_path_buf(),
        saver: Saver::Snapshot(SnapshotSaver::new(store, db.to_path_buf())),
        #[cfg(feature = "cozo")]
        backend_activity: None,
        #[cfg(all(test, feature = "fastembed"))]
        inference_embedder,
    })
}

type Backend = (
    Arc<dyn GraphStore>,
    Arc<dyn VectorIndex>,
    Arc<dyn Traversal>,
    Arc<dyn LexicalIndex>,
);

fn assemble(
    backend: Backend,
    embedder: Arc<dyn Embedder>,
    reranker: Option<Arc<dyn Reranker>>,
    bodies: &Path,
) -> Result<Memory, AnyErr> {
    let (graph, vectors, traversal, lexical) = backend;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let cfg = Config {
        default_body_scheme: "fs",
        ..Config::default()
    };
    let mem = Memory::new(graph, vectors, traversal, embedder, clock, cfg)
        .with_lexical_index(lexical)
        .with_body_store(Arc::new(FsStore::new(bodies)?));
    Ok(match reranker {
        Some(rr) => mem.with_reranker(rr),
        None => mem,
    })
}

fn runtime_fingerprint(embedder: &dyn Embedder) -> Result<EmbeddingFingerprint, AnyErr> {
    let fingerprint = embedder.fingerprint();
    fingerprint
        .validate()
        .map_err(|e| format!("embedder returned an invalid fingerprint: {e}"))?;
    if fingerprint.dimension != embedder.dim() {
        return Err(format!(
            "embedder fingerprint dim {} ≠ embedder dim {}",
            fingerprint.dimension,
            embedder.dim()
        )
        .into());
    }
    Ok(fingerprint)
}

fn prepare_embedding_store<S>(store: &S, embedder: &dyn Embedder, db: &Path) -> Result<(), AnyErr>
where
    S: EmbeddingMetadataStore + VectorIndex,
{
    let fingerprint = runtime_fingerprint(embedder)?;
    if store.dim() != embedder.dim() {
        return Err(format!(
            "vector index dim {} ≠ embedder dim {}; run `mnemed --db <PATH> reembed` \
             for {} before starting mneme-mcp",
            store.dim(),
            embedder.dim(),
            db.display()
        )
        .into());
    }
    store
        .ensure_embedding_fingerprint(&fingerprint)
        .map(|_| ())
        .map_err(|error| match error {
            Error::LegacyEmbeddingFingerprint
            | Error::EmbeddingFingerprintMismatch { .. }
            | Error::InvalidEmbeddingFingerprint(_) => format!(
                "{error}; run `mnemed --db <PATH> reembed` for {} before starting mneme-mcp",
                db.display()
            )
            .into(),
            other => Box::new(other) as AnyErr,
        })
}

/// Whether the optional cross-encoder reranker is enabled (env `MNEME_RERANK`
/// truthy). Off by default — it loads a second ONNX model.
#[cfg(feature = "fastembed")]
fn rerank_enabled() -> bool {
    std::env::var_os("MNEME_RERANK")
        .map(|v| v.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// Lazy cross-encoder reranker: loads bge-reranker on first `rerank`, not at open,
/// and shares both its session and inference gate across every database.
#[cfg(feature = "fastembed")]
struct LazyReranker {
    cell: tokio::sync::OnceCell<Arc<mneme_embed::FastReranker>>,
    gate: Arc<InferenceGate>,
}

#[cfg(feature = "fastembed")]
impl LazyReranker {
    async fn model(
        &self,
        admission: &InferenceAdmission,
    ) -> mneme_core::ports::Result<Arc<mneme_embed::FastReranker>> {
        let gate = self.gate.clone();
        let admission = admission.clone();
        self.cell
            .get_or_try_init(|| async move {
                let execution = gate.execute(&admission).await?;
                let model = tokio::task::spawn_blocking(move || {
                    let _execution = execution;
                    mneme_embed::FastReranker::new()
                })
                .await
                .map_err(|e| blocking_task_error("initialize reranker", e))??;
                Ok(Arc::new(model))
            })
            .await
            .cloned()
    }
}

#[cfg(feature = "fastembed")]
#[async_trait::async_trait]
impl Reranker for LazyReranker {
    fn semantic_id(&self) -> &'static str {
        "fastembed-bge-reranker-base-v1"
    }

    async fn rerank(&self, query: &str, docs: &[&str]) -> mneme_core::ports::Result<Vec<f32>> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }
        let admission = self.gate.admit()?;
        let model = self.model(&admission).await?;
        let execution = self.gate.execute(&admission).await?;
        let query = query.to_owned();
        let docs: Vec<String> = docs.iter().map(|doc| (*doc).to_owned()).collect();
        tokio::task::spawn_blocking(move || {
            let _execution = execution;
            let docs: Vec<&str> = docs.iter().map(String::as_str).collect();
            model.rerank_sync(&query, &docs)
        })
        .await
        .map_err(|e| blocking_task_error("rerank", e))?
    }
}

/// Loads the real embedder's ONNX model on first `embed`, not at open — so a
/// server can hold many dbs open while paying for exactly one session on demand.
#[cfg(feature = "fastembed")]
struct LazyEmbedder {
    cell: tokio::sync::OnceCell<Arc<mneme_embed::FastEmbedder>>,
    gate: Arc<InferenceGate>,
}

#[cfg(feature = "fastembed")]
impl LazyEmbedder {
    async fn model(
        &self,
        admission: &InferenceAdmission,
    ) -> mneme_core::ports::Result<Arc<mneme_embed::FastEmbedder>> {
        let gate = self.gate.clone();
        let admission = admission.clone();
        self.cell
            .get_or_try_init(|| async move {
                let execution = gate.execute(&admission).await?;
                let model = tokio::task::spawn_blocking(move || {
                    let _execution = execution;
                    mneme_embed::FastEmbedder::new()
                })
                .await
                .map_err(|e| blocking_task_error("initialize embedder", e))??;
                Ok(Arc::new(model))
            })
            .await
            .cloned()
    }
}

#[cfg(feature = "fastembed")]
#[async_trait::async_trait]
impl Embedder for LazyEmbedder {
    fn dim(&self) -> usize {
        DEFAULT_DIM
    }

    fn fingerprint(&self) -> EmbeddingFingerprint {
        mneme_embed::fastembed_fingerprint()
    }

    async fn embed(&self, texts: &[&str]) -> mneme_core::ports::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let admission = self.gate.admit()?;
        let model = self.model(&admission).await?;
        let execution = self.gate.execute(&admission).await?;
        let texts: Vec<String> = texts.iter().map(|text| (*text).to_owned()).collect();
        tokio::task::spawn_blocking(move || {
            let _execution = execution;
            let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
            model.embed_sync(&texts)
        })
        .await
        .map_err(|e| blocking_task_error("embed", e))?
    }

    async fn embed_query(&self, query: &str) -> mneme_core::ports::Result<Vec<f32>> {
        let admission = self.gate.admit()?;
        let model = self.model(&admission).await?;
        let execution = self.gate.execute(&admission).await?;
        let query = query.to_owned();
        tokio::task::spawn_blocking(move || {
            let _execution = execution;
            model.embed_query_sync(&query)
        })
        .await
        .map_err(|e| blocking_task_error("embed query", e))?
    }
}

#[cfg(feature = "fastembed")]
fn blocking_task_error(operation: &str, error: tokio::task::JoinError) -> Error {
    Error::Backend(format!("{operation} blocking task failed: {error}"))
}

fn bodies_dir(db: &Path) -> PathBuf {
    let dir = db.with_extension("bodies");
    if dir.is_absolute() {
        dir
    } else {
        std::env::current_dir().unwrap_or_default().join(dir)
    }
}

#[cfg(feature = "cozo")]
fn is_snapshot(db: &Path) -> Result<bool, AnyErr> {
    use std::io::Read;
    let mut buf = [0u8; 15];
    let n = std::fs::File::open(db)?.read(&mut buf)?;
    Ok(&buf[..n] != b"SQLite format 3")
}

#[cfg(feature = "cozo")]
fn legacy_snapshot_beside(db: &Path) -> Result<Option<PathBuf>, AnyErr> {
    let legacy = db.with_extension("json");
    if !legacy.exists() || !is_snapshot(&legacy)? {
        return Ok(None);
    }
    Ok(Some(legacy))
}

#[cfg(test)]
mod tests {
    #[cfg(all(feature = "cozo", unix))]
    #[tokio::test]
    async fn guarded_resume_checks_native_identity_before_feedback_or_body_initialization() {
        use mneme_core::ports::{
            FeedbackCommit, FeedbackCommitOutcome, FeedbackIdempotency, FeedbackRetryScope,
        };
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("mneme-native-resume-guard-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let path = root.join("memory.db");
        let replacement_path = root.join("replacement.db");
        CozoStore::materialize_fresh_current(&path, Ulid::new(), &MemStore::new(DEFAULT_DIM))
            .await
            .unwrap();
        CozoStore::materialize_fresh_current(
            &replacement_path,
            Ulid::new(),
            &MemStore::new(DEFAULT_DIM),
        )
        .await
        .unwrap();
        let slot = Arc::new(
            DatabaseSlot::open(
                path.clone(),
                Arc::new(InferenceRuntime::new()),
                "original-epoch".into(),
            )
            .unwrap(),
        );
        let original_id = slot.status().unwrap().db_id;
        slot.begin_release().unwrap().finish().unwrap();

        // Leave non-rebuildable receipt retry rows in the replacement. A late
        // identity guard would first activate another epoch and purge them.
        let lease = Arc::new(mneme_store_path::StoreLease::acquire(&replacement_path).unwrap());
        let replacement =
            CozoStore::open_existing_persistent(&replacement_path, DEFAULT_DIM, lease.clone())
                .unwrap();
        let replacement_id = replacement.db_id();
        replacement
            .activate_feedback_epoch("replacement-epoch")
            .unwrap();
        let retry = FeedbackCommit {
            idempotency: Some(
                FeedbackIdempotency::new(
                    "preserved-retry",
                    "a".repeat(64),
                    FeedbackRetryScope::new("replacement-epoch", 1, 1).unwrap(),
                )
                .unwrap(),
            ),
            applied_at: 1,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };
        assert_eq!(
            replacement.commit_feedback(&retry).await.unwrap(),
            FeedbackCommitOutcome::Applied
        );
        replacement.prepare_for_file_move().unwrap();
        drop(replacement);
        drop(lease);
        std::fs::copy(&replacement_path, &path).unwrap();
        std::fs::remove_dir_all(bodies_dir(&path)).unwrap();
        let before = std::fs::read(&path).unwrap();
        let error = slot
            .begin_resume_guarded("must-not-activate".into(), Some(original_id))
            .unwrap()
            .finish()
            .unwrap_err()
            .to_string();
        assert!(error.contains("expected_db_id mismatch"), "{error}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "replacement metadata and retry ledger must remain byte-identical"
        );
        assert!(
            !bodies_dir(&path).exists(),
            "wrong identity must not initialize body storage"
        );
        assert_eq!(slot.status().unwrap().db_id, original_id);
        assert_eq!(slot.status().unwrap().state, "maintenance");
        assert!(slot.checkout().is_err());

        // Guarded resume also cannot create a fresh store after disappearance.
        std::fs::remove_file(&path).unwrap();
        let error = slot
            .begin_resume_guarded("must-not-create".into(), Some(original_id))
            .unwrap()
            .finish()
            .unwrap_err()
            .to_string();
        assert!(error.contains("existing database"), "{error}");
        assert!(!path.exists());
        assert!(!bodies_dir(&path).exists());
        assert_eq!(slot.status().unwrap().state, "maintenance");
        assert_ne!(replacement_id, original_id);
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    use super::*;

    #[cfg(feature = "cozo")]
    #[test]
    fn snapshot_crash_child() {
        use mneme_core::{BodyRef, Node, NodeId, NodeStatus, Provenance};
        let Some(root) = std::env::var_os("MNEME_SNAPSHOT_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let path = root.join("memory.db");
        let slot = Arc::new(
            DatabaseSlot::open(
                path.clone(),
                Arc::new(InferenceRuntime::new()),
                Ulid::new().to_string(),
            )
            .unwrap(),
        );
        let checkout = slot.checkout().unwrap();
        let Saver::Cozo(store) = &checkout.saver else {
            panic!("crash fixture requires SQLite")
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for n in 0..2 {
            let node = Node::try_new(
                NodeId(Ulid::new()),
                format!("crash fixture {n}"),
                BodyRef::new(format!("fs://body{n}")).unwrap(),
                std::iter::empty::<&str>(),
                Provenance::derived_empty(),
                0.5,
                0.5,
                NodeStatus::Active,
                0,
            )
            .unwrap();
            runtime.block_on(store.put_node(&node)).unwrap();
            std::fs::write(
                path.with_extension("bodies").join(format!("body{n}")),
                format!("body {n}"),
            )
            .unwrap();
        }
        std::fs::write(root.join("expected-db-id"), checkout.db_id.to_string()).unwrap();
        drop(checkout);
        slot.begin_snapshot(Ulid::new().to_string())
            .unwrap()
            .finish()
            .unwrap();
        panic!("snapshot crash cut was not reached");
    }

    #[cfg(feature = "cozo")]
    #[test]
    fn snapshot_process_death_at_durable_cuts_reopens_source() {
        use super::super::snapshot::SnapshotBoundary as Cut;
        use sha2::{Digest, Sha256};
        let cuts = [
            Cut::AfterSourceClose,
            Cut::BeforePayloadWrite,
            Cut::AfterPayloadWrite,
            Cut::BeforePayloadSync,
            Cut::AfterPayloadSync,
            Cut::AfterPayloadCopy,
            Cut::AfterSourceReopen,
            Cut::BeforeVerify,
            Cut::AfterVerify,
            Cut::BeforeManifestWrite,
            Cut::AfterManifestWrite,
            Cut::BeforeManifestSync,
            Cut::AfterManifestSync,
            Cut::BeforeRename,
            Cut::AfterRename,
            Cut::AfterTargetSync,
            Cut::BeforeParentSync,
            Cut::AfterParentSync,
        ];
        // Every payload, DB and body alike, runs through the same copy_file
        // write/sync seam. Exercise its first and second invocation rather
        // than pretending each byte needs a distinct crash proof.
        for (cut, ordinal) in cuts
            .into_iter()
            .map(|cut| (cut, 1))
            .chain([(Cut::AfterPayloadSync, 2)])
        {
            let root = std::env::temp_dir().join(format!("mneme-mcp-crash-{}", Ulid::new()));
            std::fs::create_dir(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "host::tests::snapshot_crash_child",
                    "--nocapture",
                ])
                .env("MNEME_SNAPSHOT_TEST_ROOT", &root)
                .env(
                    "MNEME_SNAPSHOT_TEST_CRASH_CUT",
                    format!("{}@{ordinal}", cut.name()),
                )
                .output()
                .unwrap();
            assert_eq!(
                child.status.code(),
                Some(89),
                "cut {} missed: {}",
                cut.name(),
                String::from_utf8_lossy(&child.stderr)
            );
            let path = root.join("memory.db");
            let expected = std::fs::read_to_string(root.join("expected-db-id")).unwrap();
            let slot = DatabaseSlot::open(
                path.clone(),
                Arc::new(InferenceRuntime::new()),
                Ulid::new().to_string(),
            )
            .unwrap();
            assert_eq!(
                slot.checkout().unwrap().db_id.to_string(),
                expected,
                "source changed at {}",
                cut.name()
            );
            let snapshots = root.join("snapshots");
            let published: Vec<_> = std::fs::read_dir(&snapshots)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    !path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".stage-")
                })
                .collect();
            let crossed = matches!(
                cut,
                Cut::AfterRename
                    | Cut::AfterTargetSync
                    | Cut::BeforeParentSync
                    | Cut::AfterParentSync
            );
            assert_eq!(
                published.len(),
                usize::from(crossed),
                "wrong publication state at {}",
                cut.name()
            );
            if let Some(bundle) = published.first() {
                let manifest: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).unwrap())
                        .unwrap();
                assert_eq!(manifest["db_id"], expected);
                for file in manifest["files"].as_array().unwrap() {
                    let name = file["path"].as_str().unwrap();
                    let bytes = std::fs::read(bundle.join(name)).unwrap();
                    assert_eq!(bytes.len() as u64, file["size"].as_u64().unwrap());
                    assert_eq!(
                        format!("{:x}", Sha256::digest(&bytes)),
                        file["sha256"].as_str().unwrap()
                    );
                }
                assert_eq!(
                    std::fs::read(bundle.join("bodies/body0")).unwrap(),
                    b"body 0"
                );
                assert_eq!(
                    std::fs::read(bundle.join("bodies/body1")).unwrap(),
                    b"body 1"
                );
            }
            drop(slot);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn sqlite_snapshot_failures_at_close_and_copy_reopen_and_clean_stage() {
        for cut in [SnapshotCut::AfterClose, SnapshotCut::AfterCopy] {
            let root = std::env::temp_dir().join(format!("mneme-mcp-snapshot-cut-{}", Ulid::new()));
            std::fs::create_dir(&root).unwrap();
            let path = root.join("memory.db");
            let slot = Arc::new(
                DatabaseSlot::open(
                    path.clone(),
                    Arc::new(InferenceRuntime::new()),
                    Ulid::new().to_string(),
                )
                .unwrap(),
            );
            let db_id = slot.checkout().unwrap().db_id;
            let worker = slot.begin_snapshot(Ulid::new().to_string()).unwrap();
            let error = tokio::task::spawn_blocking(move || worker.finish_with_cut(cut))
                .await
                .unwrap()
                .unwrap_err()
                .to_string();
            assert!(error.contains("injected snapshot failure"), "{error}");
            assert_eq!(slot.status().unwrap().state, "open");
            assert_eq!(slot.checkout().unwrap().db_id, db_id);
            assert!(mneme_store_path::StoreLease::acquire(&path).is_err());
            let snapshots = path
                .parent()
                .unwrap()
                .canonicalize()
                .unwrap()
                .join("snapshots");
            assert!(std::fs::read_dir(snapshots).unwrap().next().is_none());
            drop(slot);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn sqlite_snapshot_post_rename_failure_keeps_complete_target_and_open_source() {
        let root = std::env::temp_dir().join(format!("mneme-mcp-snapshot-crossed-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("memory.db");
        let slot = Arc::new(
            DatabaseSlot::open(
                path.clone(),
                Arc::new(InferenceRuntime::new()),
                Ulid::new().to_string(),
            )
            .unwrap(),
        );
        let db_id = slot.checkout().unwrap().db_id;
        let worker = slot.begin_snapshot(Ulid::new().to_string()).unwrap();
        let error =
            tokio::task::spawn_blocking(move || worker.finish_with_cut(SnapshotCut::AfterRename))
                .await
                .unwrap()
                .unwrap_err()
                .to_string();
        assert!(error.contains("after rename"), "{error}");
        assert_eq!(slot.status().unwrap().state, "open");
        assert_eq!(slot.checkout().unwrap().db_id, db_id);
        assert!(mneme_store_path::StoreLease::acquire(&path).is_err());
        let snapshots = path
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join("snapshots");
        let entries: Vec<_> = std::fs::read_dir(&snapshots)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1);
        let bundle = &entries[0];
        assert!(
            bundle
                .file_name()
                .unwrap()
                .to_string_lossy()
                .parse::<Ulid>()
                .is_ok()
        );
        assert!(bundle.join("database.db").is_file());
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["schema"], "mneme.snapshot.v1");
        assert_eq!(manifest["db_id"], db_id.to_string());
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cancelled_snapshot_worker_reopens_before_unfencing_slot() {
        let root = std::env::temp_dir().join(format!("mneme-mcp-snapshot-cancel-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("memory.json");
        MemStore::new(DEFAULT_DIM).save(&path).unwrap();
        let slot = Arc::new(
            DatabaseSlot::open(
                path.clone(),
                Arc::new(InferenceRuntime::new()),
                Ulid::new().to_string(),
            )
            .unwrap(),
        );
        let worker = slot.begin_snapshot(Ulid::new().to_string()).unwrap();
        assert_eq!(slot.status().unwrap().state, "snapshotting");
        tokio::task::spawn_blocking(move || drop(worker))
            .await
            .unwrap();
        assert_eq!(slot.status().unwrap().state, "open");
        assert!(slot.checkout().is_ok());
        assert!(mneme_store_path::StoreLease::acquire(&path).is_err());
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn sqlite_snapshot_checkpoint_reopens_source_without_reacquiring_lease() {
        let root = std::env::temp_dir().join(format!("mneme-mcp-sqlite-snapshot-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("memory.db");
        let slot = Arc::new(
            DatabaseSlot::open(
                path.clone(),
                Arc::new(InferenceRuntime::new()),
                Ulid::new().to_string(),
            )
            .unwrap(),
        );
        let db_id = slot.checkout().unwrap().db_id;
        let worker = slot.begin_snapshot(Ulid::new().to_string()).unwrap();
        assert!(mneme_store_path::StoreLease::acquire(&path).is_err());
        let result = tokio::task::spawn_blocking(move || worker.finish())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.db_id, db_id);
        assert_eq!(slot.status().unwrap().state, "open");
        assert!(slot.checkout().is_ok());
        assert!(mneme_store_path::StoreLease::acquire(&path).is_err());
        assert!(
            std::fs::metadata(result.bundle.join("database.db"))
                .unwrap()
                .len()
                > 0
        );
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn snapshot_reopens_source_and_carries_referenced_body_under_same_lease() {
        use mneme_core::{BodyRef, Node, NodeId, NodeStatus, Provenance};
        let root = std::env::temp_dir().join(format!("mneme-mcp-snapshot-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("memory.json");
        MemStore::new(DEFAULT_DIM).save(&path).unwrap();
        let slot = Arc::new(
            DatabaseSlot::open(
                path.clone(),
                Arc::new(InferenceRuntime::new()),
                Ulid::new().to_string(),
            )
            .unwrap(),
        );
        let checkout = slot.checkout().unwrap();
        let node = Node::try_new(
            NodeId(Ulid::new()),
            "snapshot fixture",
            BodyRef::new("fs://fixture").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            0,
        )
        .unwrap();
        let Saver::Snapshot(saver) = &checkout.saver else {
            panic!("fixture must use JSON snapshot backend")
        };
        saver.store.put_node(&node).await.unwrap();
        checkout.save().unwrap();
        std::fs::write(
            path.with_extension("bodies").join("fixture"),
            b"body survived",
        )
        .unwrap();
        drop(checkout);
        let worker = slot.begin_snapshot(Ulid::new().to_string()).unwrap();
        assert_eq!(slot.status().unwrap().state, "snapshotting");
        assert!(slot.checkout().is_err());
        assert!(mneme_store_path::StoreLease::acquire(&path).is_err());
        let result = tokio::task::spawn_blocking(move || worker.finish())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(slot.status().unwrap().state, "open");
        assert!(mneme_store_path::StoreLease::acquire(&path).is_err());
        assert_eq!(
            std::fs::read(result.bundle.join("bodies/fixture")).unwrap(),
            b"body survived"
        );
        assert!(result.bundle.join("database.db").is_file());
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(result.bundle.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["schema"], "mneme.snapshot.v1");
        assert_eq!(manifest["files"].as_array().unwrap().len(), 2);
        assert_eq!(slot.checkout().unwrap().db_id, result.db_id);
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(not(feature = "fastembed"))]
    #[tokio::test]
    async fn snapshot_retains_every_episode_edition_body_and_identity() {
        use mneme_app::episode::PreparedEpisode;
        use serde_json::json;
        let root = std::env::temp_dir().join(format!("mneme-episode-snapshot-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("memory.json");
        MemStore::new(DEFAULT_DIM).save(&path).unwrap();
        let slot = Arc::new(
            DatabaseSlot::open(
                path.clone(),
                Arc::new(InferenceRuntime::new()),
                Ulid::new().to_string(),
            )
            .unwrap(),
        );
        let checkout = slot.checkout().unwrap();
        checkout.require_current_generation().unwrap();
        let first = PreparedEpisode::parse(&json!({
            "action":"append", "summary":"Original experience", "body":"original body",
            "source":{"namespace":"snapshot-test","key":"first","reference":"test://episode"},
        }))
        .unwrap()
        .run(&checkout.mem, checkout.db_id, None)
        .await
        .unwrap();
        let second = PreparedEpisode::parse(&json!({
            "action":"revise", "episode_id":first["episode_id"],
            "expected_edition_id":first["edition_id"], "reason":"Corrected the account",
            "summary":"Corrected experience", "body":"corrected body",
            "source":{"namespace":"snapshot-test","key":"second","reference":"test://episode"},
        }))
        .unwrap()
        .run(&checkout.mem, checkout.db_id, None)
        .await
        .unwrap();
        checkout.save().unwrap();
        let db_id = checkout.db_id;
        let first_id = mneme_core::NodeId(first["edition_id"].as_str().unwrap().parse().unwrap());
        let second_id = mneme_core::NodeId(second["edition_id"].as_str().unwrap().parse().unwrap());
        let old = checkout.mem.get_node(first_id).await.unwrap().unwrap();
        let new = checkout.mem.get_node(second_id).await.unwrap().unwrap();
        drop(checkout);
        let worker = slot.begin_snapshot(Ulid::new().to_string()).unwrap();
        let snapshot = tokio::task::spawn_blocking(move || worker.finish())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.db_id, db_id);
        assert_eq!(slot.status().unwrap().state, "open");
        assert!(mneme_store_path::StoreLease::acquire(&path).is_err());
        let exported = MemStore::load(snapshot.bundle.join("database.db")).unwrap();
        for (node, bytes) in [
            (old, b"original body".as_slice()),
            (new, b"corrected body".as_slice()),
        ] {
            let file = node
                .body()
                .as_str()
                .strip_prefix("fs://")
                .expect("fixture fs body");
            assert_eq!(
                std::fs::read(snapshot.bundle.join("bodies").join(file)).unwrap(),
                bytes
            );
            assert_eq!(
                serde_json::to_value(exported.get_node(node.id()).await.unwrap().unwrap()).unwrap(),
                serde_json::to_value(&node).unwrap(),
            );
        }
        let current = PreparedEpisode::parse(&json!({
            "action":"get", "episode_id":first["episode_id"],
        }))
        .unwrap()
        .run(&slot.checkout().unwrap().mem, db_id, None)
        .await
        .unwrap();
        assert_eq!(current["edition_id"], second["edition_id"]);
        assert_eq!(exported.export().nodes.len(), 2);
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn snapshot_rejects_unsupported_body_ref_without_tearing_down_source() {
        use mneme_core::{BodyRef, Node, NodeId, NodeStatus, Provenance};
        let root = std::env::temp_dir().join(format!("mneme-mcp-snapshot-ref-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("memory.json");
        MemStore::new(DEFAULT_DIM).save(&path).unwrap();
        let slot = Arc::new(
            DatabaseSlot::open(
                path.clone(),
                Arc::new(InferenceRuntime::new()),
                Ulid::new().to_string(),
            )
            .unwrap(),
        );
        let checkout = slot.checkout().unwrap();
        let node = Node::try_new(
            NodeId(Ulid::new()),
            "unsupported body",
            BodyRef::new("fs:///absolute").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            0,
        )
        .unwrap();
        let Saver::Snapshot(saver) = &checkout.saver else {
            panic!("fixture must use JSON backend")
        };
        saver.store.put_node(&node).await.unwrap();
        checkout.save().unwrap();
        drop(checkout);
        let worker = slot.begin_snapshot(Ulid::new().to_string()).unwrap();
        let error = tokio::task::spawn_blocking(move || worker.finish())
            .await
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(error.contains("relative fs body"), "{error}");
        assert_eq!(slot.status().unwrap().state, "open");
        assert!(slot.checkout().is_ok());
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn capture_checkout_requires_positive_current_generation() {
        let root =
            std::env::temp_dir().join(format!("mneme-mcp-capture-admission-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let capture = root.join("capture.db");
        CozoStore::materialize_fresh_current(&capture, Ulid::new(), &MemStore::new(DEFAULT_DIM))
            .await
            .unwrap();
        let runtime = Arc::new(InferenceRuntime::new());
        let capture_slot =
            DatabaseSlot::open(capture.clone(), runtime.clone(), Ulid::new().to_string()).unwrap();
        capture_slot
            .checkout()
            .unwrap()
            .require_current_generation()
            .unwrap();
        let normal = root.join("normal.json");
        MemStore::new(DEFAULT_DIM).save(&normal).unwrap();
        let normal_slot = DatabaseSlot::open(normal, runtime, Ulid::new().to_string()).unwrap();
        normal_slot
            .checkout()
            .unwrap()
            .require_current_generation()
            .unwrap();
        drop(capture_slot);
        drop(normal_slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "cozo")]
    #[test]
    fn legacy_guard_ignores_a_cozo_store_with_a_json_suffix() {
        let root = std::env::temp_dir().join(format!("mneme-mcp-legacy-guard-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let db = root.join("memory.db");
        let legacy = root.join("memory.json");

        std::fs::write(&legacy, b"SQLite format 3\0stale cozo store").unwrap();
        assert_eq!(legacy_snapshot_beside(&db).unwrap(), None);

        std::fs::write(&legacy, b"{\"dim\":384}").unwrap();
        assert_eq!(legacy_snapshot_beside(&db).unwrap(), Some(legacy));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_handoff_fences_concurrent_checkouts_and_split_brain() {
        use std::sync::mpsc;

        let root = std::env::temp_dir().join(format!("mneme-mcp-handoff-{}", Ulid::new()));
        let path = root.join("memory.db");
        let slot = Arc::new(
            DatabaseSlot::open(
                path.clone(),
                Arc::new(InferenceRuntime::new()),
                Ulid::new().to_string(),
            )
            .unwrap(),
        );

        assert!(
            mneme_store_path::StoreLease::acquire(&path).is_err(),
            "an open MCP slot must exclude an offline owner"
        );

        let checkout = slot.checkout().unwrap();
        let latent_mutation = checkout
            .mem
            .try_mutation_quiescence()
            .expect("fixture acquires the engine mutation domain");
        drop(checkout);
        let busy = slot.begin_release().err().unwrap().to_string();
        assert!(busy.contains("latent maintenance task"), "{busy}");
        assert_eq!(slot.status().unwrap().state, "open");
        drop(latent_mutation);

        let (checked_out_tx, checked_out_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_slot = slot.clone();
        let worker = std::thread::spawn(move || {
            let checkout = worker_slot.checkout().unwrap();
            checked_out_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(checkout);
        });
        checked_out_rx.recv().unwrap();
        let busy = slot.begin_release().err().unwrap().to_string();
        assert!(busy.contains("in-flight MCP operation"), "{busy}");
        assert_eq!(slot.status().unwrap().state, "open");
        assert!(mneme_store_path::StoreLease::acquire(&path).is_err());

        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(
            slot.begin_release().unwrap().finish().unwrap().state,
            "maintenance"
        );
        assert!(slot.checkout().is_err(), "maintenance must stay fenced");

        let offline = mneme_store_path::StoreLease::acquire(&path).unwrap();
        let resume_busy = slot
            .begin_resume(Ulid::new().to_string())
            .unwrap()
            .finish()
            .unwrap_err()
            .to_string();
        assert!(resume_busy.contains("already owned"), "{resume_busy}");
        assert_eq!(slot.status().unwrap().state, "maintenance");
        drop(offline);

        assert_eq!(
            slot.begin_resume(Ulid::new().to_string())
                .unwrap()
                .finish()
                .unwrap()
                .state,
            "open"
        );
        assert!(
            mneme_store_path::StoreLease::acquire(&path).is_err(),
            "successful resume must reacquire exclusion before serving"
        );

        slot.begin_release().unwrap().finish().unwrap();
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(all(unix, feature = "cozo"))]
    #[tokio::test]
    async fn maintenance_handoff_reopens_a_native_current_store() {
        use std::os::unix::fs::PermissionsExt;

        let parent = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = parent.join(format!("mneme-mcp-current-handoff-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("memory.db");

        CozoStore::materialize_fresh_current(&path, Ulid::new(), &MemStore::new(DEFAULT_DIM))
            .await
            .expect("materialize native current fixture");

        let slot = Arc::new(
            DatabaseSlot::open(
                path.clone(),
                Arc::new(InferenceRuntime::new()),
                "initial-current-epoch".to_owned(),
            )
            .expect("MCP opens native current store"),
        );
        assert_eq!(slot.status().unwrap().state, "open");
        let checkout = slot.checkout().unwrap();
        checkout.save().expect("write-through save remains a no-op");
        assert!(
            checkout
                .mem
                .get_node(mneme_core::NodeId(Ulid::new()))
                .await
                .expect("ordinary save must not detach the current backend")
                .is_none()
        );
        drop(checkout);
        assert_eq!(
            slot.begin_release().unwrap().finish().unwrap().state,
            "maintenance"
        );
        assert_eq!(
            slot.begin_resume("resumed-current-epoch".to_owned())
                .unwrap()
                .finish()
                .unwrap()
                .state,
            "open"
        );

        slot.begin_release().unwrap().finish().unwrap();
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn identity_checked_checkout_cannot_cross_a_resumed_generation() {
        let root = std::env::temp_dir().join(format!("mneme-mcp-target-swap-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("memory.json");
        let replacement = root.join("replacement.json");
        let runtime = Arc::new(InferenceRuntime::new());
        let slot =
            Arc::new(DatabaseSlot::open(path.clone(), runtime, Ulid::new().to_string()).unwrap());
        let old_id = slot.status().unwrap().db_id;

        let next = MemStore::new(DEFAULT_DIM);
        let next_id = next.db_id();
        assert_ne!(next_id, old_id);
        next.save(&replacement).unwrap();

        slot.begin_release().unwrap().finish().unwrap();
        std::fs::copy(&replacement, &path).unwrap();
        slot.begin_resume(Ulid::new().to_string())
            .unwrap()
            .finish()
            .unwrap();

        assert!(
            slot.checkout_if_db_id(old_id).unwrap().is_none(),
            "a stale remote db id must not check out the replacement generation"
        );
        let matching = slot
            .checkout_if_db_id(next_id)
            .unwrap()
            .expect("replacement id must match")
            .expect("replacement generation must be open");
        assert_eq!(matching.db_id, next_id);
        drop(matching);

        slot.begin_release().unwrap().finish().unwrap();
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(all(unix, feature = "cozo"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_waiter_cannot_release_lease_before_detached_backend_quiesces() {
        use std::sync::mpsc;

        let root = std::env::temp_dir().join(format!("mneme-mcp-cancelled-cozo-{}", Ulid::new()));
        let path = root.join("store.cozo");
        let slot = Arc::new(
            DatabaseSlot::open(
                path.clone(),
                Arc::new(InferenceRuntime::new()),
                Ulid::new().to_string(),
            )
            .unwrap(),
        );
        let checkout = slot.checkout().unwrap();
        let activity = checkout
            .backend_activity
            .clone()
            .expect("persistent Cozo handle exposes backend activity");
        drop(checkout);

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let guard = activity.enter();
        let waiter = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let _guard = guard;
                started_tx.send(()).ok();
                finish_rx.recv().unwrap();
            })
            .await
        });
        started_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(activity.in_flight(), 1);

        let busy = slot.begin_release().err().unwrap().to_string();
        assert!(busy.contains("detached backend job"), "{busy}");
        assert!(
            mneme_store_path::StoreLease::acquire(&path).is_err(),
            "a cancelled request must not expose the store while its detached backend job runs"
        );

        finish_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while activity.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached backend job did not quiesce");
        assert_eq!(
            slot.begin_release().unwrap().finish().unwrap().state,
            "maintenance"
        );
        let offline = mneme_store_path::StoreLease::acquire(&path).unwrap();
        drop(offline);
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn snapshot_checkpoint_orders_export_through_atomic_rename() {
        use std::sync::mpsc;
        use std::time::Duration;

        use mneme_core::{BodyRef, Node, NodeId, NodeStatus, Provenance};

        fn node(id: u128, summary: &str) -> Node {
            Node::try_new(
                NodeId(Ulid::from(id)),
                summary,
                BodyRef::new(format!("inline://{id}")).unwrap(),
                std::iter::empty::<&str>(),
                Provenance::derived_empty(),
                0.5,
                0.5,
                NodeStatus::Active,
                0,
            )
            .unwrap()
        }

        let root = std::env::temp_dir().join(format!("mneme-snapshot-save-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("memory.json");
        let store = Arc::new(MemStore::new(8));
        let first = node(1, "first generation");
        let second = node(2, "second generation");
        store.put_node(&first).await.unwrap();

        let saver = Arc::new(SnapshotSaver::new(store.clone(), path.clone()));
        let (old_captured_tx, old_captured_rx) = mpsc::channel();
        let (release_old_tx, release_old_rx) = mpsc::channel();
        let old_saver = saver.clone();
        let old = std::thread::spawn(move || {
            old_saver.save_with(|store, path| {
                // Capture the older generation, then pause immediately before its
                // atomic publication. This is the ordering that previously let a
                // stale writer land after a newer request had already succeeded.
                let bytes = serde_json::to_vec_pretty(&store.export())?;
                old_captured_tx.send(()).unwrap();
                release_old_rx.recv().unwrap();
                let temp = path.with_extension("old-checkpoint.tmp");
                std::fs::write(&temp, bytes)?;
                std::fs::rename(temp, path)?;
                Ok(())
            })
        });
        old_captured_rx.recv().unwrap();

        store.put_node(&second).await.unwrap();
        let (new_started_tx, new_started_rx) = mpsc::channel();
        let (new_entered_tx, new_entered_rx) = mpsc::channel();
        let new_saver = saver.clone();
        let newer = std::thread::spawn(move || {
            new_started_tx.send(()).unwrap();
            new_saver.save_with(|store, path| {
                new_entered_tx.send(()).unwrap();
                store.save(path)?;
                Ok(())
            })
        });
        new_started_rx.recv().unwrap();
        assert!(
            new_entered_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "a newer checkpoint entered while the older export-to-rename section was live"
        );

        release_old_tx.send(()).unwrap();
        old.join().unwrap().unwrap();
        newer.join().unwrap().unwrap();
        assert!(new_entered_rx.try_recv().is_ok());

        let reopened = MemStore::load(&path).unwrap();
        assert!(
            reopened.get_node(second.id()).await.unwrap().is_some(),
            "the older paused checkpoint landed after the newer completed checkpoint"
        );

        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(feature = "fastembed")]
    #[test]
    fn two_database_handles_share_one_inference_runtime() {
        let root = std::env::temp_dir().join(format!("mneme-mcp-runtime-{}", Ulid::new()));
        std::fs::create_dir_all(&root).unwrap();
        let runtime = InferenceRuntime::new();
        let first_path = root.join("first.db");
        let second_path = root.join("second.db");
        let first = open(
            &first_path,
            &runtime,
            Arc::new(mneme_store_path::StoreLease::acquire(&first_path).unwrap()),
            "first-epoch",
        )
        .unwrap();
        let second = open(
            &second_path,
            &runtime,
            Arc::new(mneme_store_path::StoreLease::acquire(&second_path).unwrap()),
            "second-epoch",
        )
        .unwrap();

        assert!(
            first.shares_embedder_runtime(&second),
            "every handle built by one registry must point at the same lazy model cell"
        );

        drop((first, second));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "fastembed")]
    #[test]
    fn enabled_reranker_is_shared_too() {
        let runtime = InferenceRuntime::new_fast(true);
        let first = runtime.reranker().unwrap();
        let second = runtime.reranker().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(
            &runtime.embedder.gate,
            &runtime.reranker.as_ref().unwrap().gate
        ));
    }

    #[cfg(feature = "fastembed")]
    #[test]
    fn inference_admission_is_bounded_and_released_on_drop() {
        let gate = InferenceGate::new(4, 1, Duration::from_secs(5));
        let admitted: Vec<_> = (0..4).map(|_| gate.admit().unwrap()).collect();

        assert!(matches!(
            gate.admit(),
            Err(Error::CapacityExceeded {
                resource: "inference requests",
                limit: 4
            })
        ));

        drop(admitted);
        assert_eq!(gate.admission.available_permits(), 4);
    }

    #[cfg(feature = "fastembed")]
    #[tokio::test]
    async fn inference_execution_is_serialized() {
        let gate = InferenceGate::new(2, 1, Duration::from_secs(1));
        let first_admission = gate.admit().unwrap();
        let first = gate.execute(&first_admission).await.unwrap();
        let second_admission = gate.admit().unwrap();
        let second = gate.execute(&second_admission);
        tokio::pin!(second);

        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut second)
                .await
                .is_err(),
            "a second inference execution started while the first held the gate"
        );

        drop(first);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut second)
                .await
                .unwrap()
                .is_ok()
        );
    }

    #[cfg(feature = "fastembed")]
    #[tokio::test]
    async fn inference_execution_wait_has_a_deadline() {
        let gate = InferenceGate::new(2, 1, Duration::from_millis(10));
        let first_admission = gate.admit().unwrap();
        let first = gate.execute(&first_admission).await.unwrap();
        let second_admission = gate.admit().unwrap();

        let result = gate.execute(&second_admission).await;

        assert!(matches!(
            result,
            Err(Error::Backend(message)) if message.contains("timed out")
        ));
        drop(second_admission);
        assert_eq!(gate.admission.available_permits(), 1);
        drop((first, first_admission));
        assert_eq!(gate.admission.available_permits(), 2);
    }

    #[cfg(feature = "fastembed")]
    #[tokio::test]
    async fn blocking_execution_keeps_its_request_admitted() {
        let gate = InferenceGate::new(1, 1, Duration::from_secs(1));
        let admission = gate.admit().unwrap();
        let execution = gate.execute(&admission).await.unwrap();

        drop(admission);
        assert!(matches!(
            gate.admit(),
            Err(Error::CapacityExceeded { limit: 1, .. })
        ));

        drop(execution);
        assert!(gate.admit().is_ok());
    }
}
