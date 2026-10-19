//! Best-effort demand metadata, not cached memory and not transfer authority.
//! Only a verified canonical payload read calls `record`; planning replays the
//! exact original request offline before an independently bounded worker acts.
use crate::{AnyErr, RequestAccess};
use mneme_config::{
    ActiveStore, CachePopulate, DiscoveryWitness, Owner, SelectionRequest, SelectionSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const DEMAND_SCHEMA: &str = "mneme.cache-demand.v1";
pub const MAX_DEMAND_BYTES: usize = 64 * 1024;
pub const MAX_DEMAND_AGE_SECS: u64 = 86_400;
const MAX_DEMANDS: usize = 128;
const COALESCE_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DemandRecord {
    pub schema: String,
    pub request: SelectionRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<DiscoveryWitness>,
    pub selection_digest: String,
    pub store: String,
    pub db_id: String,
    pub library: String,
    pub explicit_store: bool,
    pub observed_at_unix_secs: u64,
}

/// A checked, short-lived observation of current policy. It does not authorize
/// arbitrary commands, file paths, snapshot writes, or remote credentials.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DemandPlan {
    pub schema: String,
    pub demand_path: PathBuf,
    pub request: SelectionRequest,
    pub selection_digest: String,
    pub store: String,
    pub db_id: String,
    pub library: String,
    pub owner: Owner,
    pub cache: CachePopulate,
    pub catalog_path: PathBuf,
    pub binding: Option<DiscoveryWitness>,
    pub observed_at_unix_secs: u64,
    pub deadline_unix_secs: u64,
}

impl DemandRecord {
    pub(crate) fn from_selection(
        snapshot: &SelectionSnapshot,
        selected: &ActiveStore,
        now: u64,
    ) -> Result<Option<Self>, AnyErr> {
        let Some(cache) = &selected.store.cache else {
            return Ok(None);
        };
        let Some(policy) = &cache.populate else {
            return Ok(None);
        };
        policy.validate()?;
        if !selected.eligible.read || !cfg!(unix) {
            return Ok(None);
        }
        let Some(request) = &snapshot.request else {
            return Ok(None);
        };
        let Some(_) = demand_path(snapshot, selected)? else {
            return Ok(None);
        };
        let value = Self {
            schema: DEMAND_SCHEMA.into(),
            request: request.clone(),
            discovery: snapshot.discovery.clone(),
            selection_digest: snapshot.digest.clone(),
            store: selected.name.clone(),
            db_id: selected.store.id.clone(),
            library: cache.library.clone(),
            explicit_store: !snapshot.active.iter().any(|s| s.name == selected.name),
            observed_at_unix_secs: now,
        };
        value.validate(now)?;
        Ok(Some(value))
    }

    fn validate(&self, now: u64) -> Result<(), AnyErr> {
        if self.schema != DEMAND_SCHEMA
            || self.selection_digest.len() != 64
            || !self
                .selection_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.db_id.len() != 26
            || ulid::Ulid::from_string(&self.db_id)
                .ok()
                .is_none_or(|id| id.to_string() != self.db_id)
            || [&self.store, &self.library].iter().any(|name| {
                name.is_empty()
                    || name.len() > 128
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
            })
        {
            return Err(
                "invalid cache demand metadata; use the store again to record fresh demand".into(),
            );
        }
        if self.observed_at_unix_secs > now
            || now.saturating_sub(self.observed_at_unix_secs) >= MAX_DEMAND_AGE_SECS
            || self
                .observed_at_unix_secs
                .checked_add(MAX_DEMAND_AGE_SECS)
                .is_none()
        {
            return Err(
                "cache demand expired or clock moved backwards; use the store again".into(),
            );
        }
        if serde_json::to_vec(self)?.len() > MAX_DEMAND_BYTES {
            return Err("cache demand exceeds metadata byte bound".into());
        }
        Ok(())
    }
}

/// Closed actual-payload classification: metadata inspection and session
/// finalization remain inert. Unknown future tools cannot inherit demand.
pub(crate) fn payload_read(tool: &str, args: &Value, access: RequestAccess) -> bool {
    if access != RequestAccess::Read {
        return false;
    }
    match tool {
        "core" | "query" | "recall" | "recall_context" | "get" | "list" | "neighbors" => true,
        "graph" => args["action"] == "summaries",
        "walk" => matches!(
            args["action"].as_str(),
            Some("start" | "look" | "edges" | "body" | "go" | "back")
        ),
        "episode" => matches!(
            args["action"].as_str(),
            Some("list" | "search" | "get" | "history" | "references")
        ),
        _ => false,
    }
}

pub(crate) fn demand_path(
    snapshot: &SelectionSnapshot,
    selected: &ActiveStore,
) -> Result<Option<PathBuf>, AnyErr> {
    let Some(cache) = &selected.store.cache else {
        return Ok(None);
    };
    if cache.populate.is_none() {
        return Ok(None);
    }
    let library = snapshot
        .libraries
        .get(&cache.library)
        .ok_or("cache library is not admitted in this workspace/profile")?;
    // Effective discovery can reference a library, never create a local target.
    if snapshot.config.libraries.get(&cache.library) != Some(library) {
        return Err("cache demand requires a locally declared library target".into());
    }
    let parent = library
        .catalog
        .parent()
        .ok_or("cache catalog has no local parent")?;
    Ok(Some(
        parent
            .join("cache-demand")
            .join(format!("{}.json", selected.store.id)),
    ))
}

/// One tiny best-effort local metadata publication, with no lock waiting, sync,
/// network, capture, or owner call. Keeping this synchronous makes ordinary
/// short-lived CLI use reliable without a detached-thread/process-exit race.
/// Filesystem latency itself is not a hard deadline; errors never fail a read.
pub(crate) fn record(snapshot: &SelectionSnapshot, selected: &ActiveStore) {
    let Ok(now) = now_secs() else { return };
    let Ok(Some(record)) = DemandRecord::from_selection(snapshot, selected, now) else {
        return;
    };
    let Ok(Some(path)) = demand_path(snapshot, selected) else {
        return;
    };
    let _ = persist(&path, &record);
}

pub fn plan(path: &Path) -> Result<DemandPlan, AnyErr> {
    plan_at(path, now_secs()?)
}

pub(crate) fn plan_at(path: &Path, now: u64) -> Result<DemandPlan, AnyErr> {
    let record = read(path)?;
    record.validate(now)?;
    let snapshot = record
        .request
        .replay_offline(record.discovery.as_ref())?
        .ok_or("cache demand workspace is no longer configured; use the store again")?;
    if snapshot.digest != record.selection_digest {
        return Err("cache demand selection changed; use the store again before population".into());
    }
    let selected = if record.explicit_store {
        snapshot.explicit_store(&record.store)?
    } else {
        snapshot
            .active
            .iter()
            .find(|s| s.name == record.store)
            .cloned()
            .ok_or("cache demand store is no longer active; no owner was contacted")?
    };
    if !selected.eligible.read || selected.store.id != record.db_id {
        return Err("cache demand read authority or canonical identity changed".into());
    }
    let cache = selected
        .store
        .cache
        .as_ref()
        .ok_or("cache population is not configured")?;
    let policy = cache
        .populate
        .as_ref()
        .ok_or("cache population is disabled")?;
    policy.validate()?;
    if cache.library != record.library
        || demand_path(&snapshot, &selected)?.as_deref() != Some(path)
    {
        return Err("cache demand is not at the current locally declared library target".into());
    }
    Ok(DemandPlan {
        schema: "mneme.cache-plan.v1".into(),
        demand_path: path.into(),
        request: record.request,
        selection_digest: record.selection_digest,
        store: record.store,
        db_id: record.db_id,
        library: record.library,
        owner: selected.store.owner,
        cache: policy.clone(),
        catalog_path: snapshot.libraries[&cache.library].catalog.clone(),
        binding: record.discovery,
        observed_at_unix_secs: record.observed_at_unix_secs,
        deadline_unix_secs: record.observed_at_unix_secs + MAX_DEMAND_AGE_SECS,
    })
}

fn now_secs() -> Result<u64, AnyErr> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn private(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}
fn checked_directory(path: &Path, create: bool) -> Result<(), AnyErr> {
    if !path.is_absolute()
        || path.to_str().is_none_or(|p| p.len() > 4096)
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        || path.components().collect::<PathBuf>() != path
    {
        return Err("cache demand needs a bounded normalized absolute directory".into());
    }
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(m) if m.is_dir() && !m.is_symlink() => {}
            Err(e) if create && e.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = std::fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                match builder.create(ancestor) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err("cannot create private cache demand directory".into()),
                }
                let m = std::fs::symlink_metadata(ancestor)?;
                if !m.is_dir() || m.is_symlink() || !private(&m) {
                    return Err("unsafe cache demand directory".into());
                }
            }
            _ => return Err("cache demand ancestors must be non-symlink directories".into()),
        }
    }
    if !private(&std::fs::symlink_metadata(path)?) {
        return Err("cache demand directory must be private and user-owned (0700)".into());
    }
    Ok(())
}
fn checked_file(metadata: &std::fs::Metadata) -> Result<(), AnyErr> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("cache demand must not be hard-linked".into());
        }
    }
    if !metadata.is_file()
        || metadata.is_symlink()
        || !private(metadata)
        || metadata.len() > MAX_DEMAND_BYTES as u64
    {
        return Err("cache demand must be a bounded private user-owned regular file (0600)".into());
    }
    Ok(())
}
fn target(path: &Path) -> Result<bool, AnyErr> {
    match std::fs::symlink_metadata(path) {
        Ok(m) => {
            checked_file(&m)?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("cannot inspect cache demand file".into()),
    }
}
fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options
}
fn read(path: &Path) -> Result<DemandRecord, AnyErr> {
    checked_directory(path.parent().ok_or("cache demand has no parent")?, false)?;
    if !target(path)? {
        return Err("cache demand is missing; use the store first".into());
    }
    let mut file = options()
        .read(true)
        .open(path)
        .map_err(|_| "cannot open cache demand")?;
    checked_file(&file.metadata()?)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_DEMAND_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_DEMAND_BYTES {
        return Err("cache demand exceeds metadata byte bound".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid cache demand; inspect and remove only this demand file, then use the store again".into())
}

pub(crate) fn persist(path: &Path, record: &DemandRecord) -> Result<(), AnyErr> {
    record.validate(record.observed_at_unix_secs)?;
    let parent = path.parent().ok_or("cache demand has no parent")?;
    checked_directory(parent, true)?;
    // A persistent per-catalog nonblocking OS lock bounds simultaneous writers
    // without another wait queue or one lock inode per old store identity.
    let lock_path = parent.join(".lock");
    target(&lock_path)?;
    let lock = options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    checked_file(&lock.metadata()?)?;
    if lock.metadata()?.len() != 0 {
        return Err("cache demand lock must be empty".into());
    }
    lock.try_lock().map_err(|_| "cache demand writer busy")?;
    let exists = target(path)?;
    if !exists {
        let entries = std::fs::read_dir(parent)?
            .take(MAX_DEMANDS + 2)
            .collect::<Result<Vec<_>, _>>()?;
        // Include one sentinel beyond the complete allowance (.lock + demand
        // slots). A partial scan can never prove room, even if it could retire
        // some early expired entries while unseen entries remain.
        if entries.len() > MAX_DEMANDS + 1 {
            return Err(
                "cache demand inventory exceeds scan bound; inspect retained demand metadata"
                    .into(),
            );
        }
        let mut count = 0;
        for entry in entries {
            if entry.file_name() != ".lock" {
                // Only our bounded, private, identity-named demand hints may
                // expire. Unknown/malformed/unsafe entries remain untouched and
                // count toward admission instead of authorizing broad cleanup.
                if let Ok(old) = read(&entry.path())
                    && old.validate(old.observed_at_unix_secs).is_ok()
                    && entry.file_name() == format!("{}.json", old.db_id).as_str()
                    && record
                        .observed_at_unix_secs
                        .saturating_sub(old.observed_at_unix_secs)
                        >= MAX_DEMAND_AGE_SECS
                {
                    std::fs::remove_file(entry.path())?;
                    continue;
                }
                count += 1;
            }
        }
        if count >= MAX_DEMANDS {
            return Err("cache demand directory is full; inspect retained demand metadata".into());
        }
    }
    if exists {
        let mut previous = read(path)?;
        let observed = previous.observed_at_unix_secs;
        previous.observed_at_unix_secs = record.observed_at_unix_secs;
        if previous == *record
            && record.observed_at_unix_secs.saturating_sub(observed) < COALESCE_SECS
        {
            return Ok(());
        }
        if observed > record.observed_at_unix_secs {
            return Ok(());
        }
    }
    let bytes = serde_json::to_vec(record)?;
    if bytes.len() > MAX_DEMAND_BYTES {
        return Err("cache demand exceeds metadata byte bound".into());
    }
    let temp = parent.join(format!(".{}.tmp", ulid::Ulid::new()));
    struct Temporary(PathBuf);
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let cleanup = Temporary(temp.clone());
    let mut file = options().write(true).create_new(true).open(&temp)?;
    file.write_all(&bytes)?;
    drop(file);
    target(path)?;
    std::fs::rename(&temp, path)?;
    // Demand is an expendable hint, not a durability receipt. Only the worker's
    // verified snapshot publication syncs; a crash can lose this optional hint.
    drop(cleanup);
    Ok(())
}

#[cfg(test)]
#[path = "demand_tests.rs"]
mod tests;
