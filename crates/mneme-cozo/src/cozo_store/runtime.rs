use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
#[cfg(test)]
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use cozo::{DataValue, DbInstance, NamedRows, SQLITE_BUSY_TIMEOUT_MS, ScriptMutability};
use mneme_core::ports::{Error, Result};
#[cfg(test)]
use mneme_core::tagged::TaggedPhysicalStatus;

use super::backend;
use super::opening::PersistentStoreAuthority;

pub(super) const TAGGED_READ_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const MAX_CONCURRENT_TAGGED_READS: usize = 4;

/// Backoff for the cross-process "database is locked" retry (see `run`): 13
/// attempts, each with SQLite's 250 ms busy timeout, plus exponential sleeps.
/// The resulting contention-wait ceiling is exactly 4,765 ms; actual statement
/// work and scheduler delay are separate.
pub(super) const LOCK_RETRY_MAX: u32 = 12;
pub(super) const LOCK_RETRY_BASE_MS: u64 = 5;
pub(super) const LOCK_RETRY_CAP_MS: u64 = 200;

pub(super) const fn lock_retry_backoff_ceiling_ms() -> u64 {
    let mut total = 0;
    let mut delay = LOCK_RETRY_BASE_MS;
    let mut retry = 0;
    while retry < LOCK_RETRY_MAX {
        total += delay;
        delay = if delay.saturating_mul(2) < LOCK_RETRY_CAP_MS {
            delay * 2
        } else {
            LOCK_RETRY_CAP_MS
        };
        retry += 1;
    }
    total
}

/// Maximum time SQLite's busy handler plus our inter-attempt backoff can wait
/// before one script fails closed, excluding actual statement work/scheduling.
pub(super) const LOCK_RETRY_WAIT_CEILING_MS: u64 =
    SQLITE_BUSY_TIMEOUT_MS * (LOCK_RETRY_MAX as u64 + 1) + lock_retry_backoff_ceiling_ms();
const _: () = assert!(LOCK_RETRY_WAIT_CEILING_MS < 5_000);

/// Shared accounting for synchronous backend jobs detached onto Tokio's
/// blocking pool.
///
/// Dropping the async future that submitted a `spawn_blocking` job does not
/// cancel that job. The activity token is therefore created before submission
/// and moved into the blocking closure itself. Hosts that hand an exclusive
/// process lease to another process can query this handle and refuse the
/// handoff until every detached job has actually quiesced.
#[derive(Clone, Default)]
pub struct BackendActivity {
    in_flight: Arc<AtomicUsize>,
}

impl BackendActivity {
    /// Enter one backend job. The returned token must live inside the actual
    /// blocking work, rather than only in the caller awaiting its join handle.
    pub fn enter(&self) -> BackendActivityGuard {
        self.in_flight.fetch_add(1, AtomicOrdering::AcqRel);
        BackendActivityGuard {
            in_flight: self.in_flight.clone(),
        }
    }

    /// Number of submitted blocking jobs that have not yet returned or
    /// unwound. An acquire load pairs with the token's release on drop.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(AtomicOrdering::Acquire)
    }
}

/// RAII token proving one detached backend job is still live.
pub struct BackendActivityGuard {
    in_flight: Arc<AtomicUsize>,
}

impl Drop for BackendActivityGuard {
    fn drop(&mut self) {
        let previous = self.in_flight.fetch_sub(1, AtomicOrdering::AcqRel);
        debug_assert!(previous > 0, "backend activity counter underflow");
    }
}

/// One detached job's cloned backend, persistent authority, and activity token.
/// Struct fields drop in declaration order, so the `DbInstance` is fully closed
/// before the last authority can release its lease, and both precede the host
/// observing the activity count reach zero, including on unwind.
pub(super) struct ActiveBackendJob {
    pub(super) db: DbInstance,
    pub(super) authority: Option<Arc<PersistentStoreAuthority>>,
    pub(super) activity: BackendActivityGuard,
}

fn assert_active_backend_job_send_sync<T: Send + Sync>() {}
const _: fn() = assert_active_backend_job_send_sync::<ActiveBackendJob>;

impl ActiveBackendJob {
    /// Run one detached operation while owning the complete activity guard.
    ///
    /// The consuming receiver is a correctness boundary. Rust 2024 closures
    /// capture disjoint fields precisely, so referring to `job.db` directly in
    /// a `move` closure can leave `activity` in the cancelled outer future.
    /// Calling this method forces the complete job into the blocking closure;
    /// the guard cannot drop until the database operation returns or unwinds.
    pub(super) fn run<R>(self, operation: impl FnOnce(&DbInstance) -> R) -> R {
        let _authority = &self.authority;
        let _activity = &self.activity;
        operation(&self.db)
    }
}

#[derive(Clone, Default)]
pub(super) struct TaggedReadAdmission {
    live: Arc<AtomicUsize>,
}

impl TaggedReadAdmission {
    pub(super) fn try_acquire(&self) -> Result<TaggedReadPermit> {
        let mut observed = self.live.load(AtomicOrdering::Acquire);
        loop {
            if observed >= MAX_CONCURRENT_TAGGED_READS {
                return Err(Error::Backend(format!(
                    "tagged read snapshot capacity exhausted ({MAX_CONCURRENT_TAGGED_READS} live snapshots)"
                )));
            }
            match self.live.compare_exchange_weak(
                observed,
                observed + 1,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(TaggedReadPermit {
                        live: self.live.clone(),
                    });
                }
                Err(current) => observed = current,
            }
        }
    }

    #[cfg(test)]
    pub(super) fn live(&self) -> usize {
        self.live.load(AtomicOrdering::Acquire)
    }
}

pub(super) struct TaggedReadPermit {
    live: Arc<AtomicUsize>,
}

impl Drop for TaggedReadPermit {
    fn drop(&mut self) {
        let previous = self.live.fetch_sub(1, AtomicOrdering::AcqRel);
        debug_assert!(previous > 0, "tagged read permit underflow");
    }
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct TaggedReadTestHook {
    state: Mutex<TaggedReadTestState>,
    wake: Condvar,
}

#[cfg(test)]
#[derive(Default)]
struct TaggedReadTestState {
    held: bool,
    released: bool,
    entered: usize,
    record_scans: bool,
    scans: Vec<TaggedScanObservation>,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TaggedScanObservation {
    pub(super) tag: String,
    pub(super) status: TaggedPhysicalStatus,
    pub(super) continued: bool,
    pub(super) limit: usize,
    pub(super) returned: usize,
}

#[cfg(test)]
impl TaggedReadTestHook {
    pub(super) fn hold(self: &Arc<Self>) -> TaggedReadTestHold {
        let mut state = self.state.lock().expect("tagged test hook lock");
        assert!(!state.held, "tagged test hook is already held");
        *state = TaggedReadTestState {
            held: true,
            ..TaggedReadTestState::default()
        };
        TaggedReadTestHold(self.clone())
    }

    pub(super) fn block_if_held(&self) {
        let mut state = self.state.lock().expect("tagged test hook lock");
        if !state.held || state.released {
            return;
        }
        state.entered += 1;
        self.wake.notify_all();
        while !state.released {
            state = self.wake.wait(state).expect("tagged test hook wait");
        }
    }

    pub(super) fn wait_for_entered(&self, wanted: usize, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.state.lock().expect("tagged test hook lock");
        while state.entered < wanted {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, wait) = self
                .wake
                .wait_timeout(state, remaining)
                .expect("tagged test hook timed wait");
            state = next;
            if wait.timed_out() && state.entered < wanted {
                return false;
            }
        }
        true
    }

    pub(super) fn begin_scan_recording(&self) {
        let mut state = self.state.lock().expect("tagged test hook lock");
        assert!(
            !state.record_scans,
            "tagged scan recording is already active"
        );
        state.scans.clear();
        state.record_scans = true;
    }

    pub(super) fn record_scan(
        &self,
        tag: &str,
        status: TaggedPhysicalStatus,
        continued: bool,
        limit: usize,
        returned: usize,
    ) {
        let mut state = self.state.lock().expect("tagged test hook lock");
        if state.record_scans {
            state.scans.push(TaggedScanObservation {
                tag: tag.to_owned(),
                status,
                continued,
                limit,
                returned,
            });
        }
    }

    pub(super) fn finish_scan_recording(&self) -> Vec<TaggedScanObservation> {
        let mut state = self.state.lock().expect("tagged test hook lock");
        assert!(state.record_scans, "tagged scan recording is not active");
        state.record_scans = false;
        std::mem::take(&mut state.scans)
    }
}

#[cfg(test)]
pub(super) struct TaggedReadTestHold(Arc<TaggedReadTestHook>);

#[cfg(test)]
impl Drop for TaggedReadTestHold {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().expect("tagged test hook lock");
        state.released = true;
        state.held = false;
        self.0.wake.notify_all();
    }
}

/// Shared synchronous Cozo runner. `run` uses it for setup/cold-path calls;
/// `run_async` moves it onto Tokio's blocking pool for retrieval.
pub(super) fn run_db(
    db: &DbInstance,
    script: &str,
    params: BTreeMap<String, DataValue>,
    mutable: bool,
) -> Result<NamedRows> {
    let mutability = if mutable {
        ScriptMutability::Mutable
    } else {
        ScriptMutability::Immutable
    };

    // SQLite has a bounded busy timeout, but can still return or panic on BUSY
    // after it expires. Retry both forms; a panicked transaction rolls back in
    // SqliteTx::Drop before the next attempt.
    let mut delay = Duration::from_millis(LOCK_RETRY_BASE_MS);
    for attempt in 0..=LOCK_RETRY_MAX {
        let params = params.clone();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.run_script(script, params, mutability)
        }));
        let last = attempt == LOCK_RETRY_MAX;
        match outcome {
            Ok(Ok(rows)) => return Ok(rows),
            Ok(Err(error)) if is_locked(&error) => {
                if last {
                    return Err(Error::Backend(format!(
                        "database is locked: gave up after at most {LOCK_RETRY_WAIT_CEILING_MS} ms of contention wait"
                    )));
                }
            }
            Ok(Err(error)) => return Err(backend(error)),
            Err(panic) if panic_is_locked(panic.as_ref()) => {
                if last {
                    return Err(Error::Backend(format!(
                        "database is locked: gave up after at most {LOCK_RETRY_WAIT_CEILING_MS} ms of contention wait"
                    )));
                }
            }
            Err(panic) => std::panic::resume_unwind(panic),
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(LOCK_RETRY_CAP_MS));
    }
    unreachable!("the loop returns or re-raises on the final attempt")
}

/// Whether a cozo/sqlite error is lock contention worth retrying. cozo wraps the
/// sqlite `SQLITE_BUSY`/`SQLITE_LOCKED` ("… is locked") as a generic "when
/// executing against relation '…'", so the lock message is only in the cause
/// chain — which the `Debug` rendering of the miette report includes.
pub(super) fn is_locked(e: &impl std::fmt::Debug) -> bool {
    format!("{e:?}").contains("is locked")
}

/// Whether a caught panic payload is the lock-contention panic cozo raises by
/// `.unwrap()`-ing `SQLITE_BUSY` (payload is the unwrap message string).
pub(super) fn panic_is_locked(payload: &(dyn std::any::Any + Send)) -> bool {
    payload
        .downcast_ref::<&str>()
        .is_some_and(|s| s.contains("is locked"))
        || payload
            .downcast_ref::<String>()
            .is_some_and(|s| s.contains("is locked"))
}

/// Install (once per process) a panic hook that swallows the SQLITE_BUSY panics
/// `run` catches and retries, so they don't spam stderr; everything else falls
/// through to the default hook unchanged.
pub(super) fn install_lock_panic_filter() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if panic_is_locked(info.payload()) {
                return;
            }
            default(info);
        }));
    });
}
