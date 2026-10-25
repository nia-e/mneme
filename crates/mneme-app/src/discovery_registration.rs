//! Add-only registration in an operator's existing discovery catalog.
//!
//! This updates bounded TOML metadata, never opens a memory database, and never
//! activates a store. Cooperating writers share a persistent sibling lock. The
//! final byte comparison detects ordinary external edits, but is not a filesystem
//! compare-and-swap: an editor ignoring the lock can still race that last check.

use mneme_config::{
    DISCOVERY_CATALOG_SCHEMA, MAX_CATALOG_BYTES, PublishedCatalog, StoreDeclaration,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const MAX_REGISTRATION_BYTES: usize = MAX_CATALOG_BYTES;
const LOCK_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegistrationRequest {
    pub label: String,
    pub store: StoreDeclaration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_catalog_sha256: Option<String>,
}

impl RegistrationRequest {
    /// Pure request admission; does not inspect or create any filesystem entry.
    pub fn validate(&self) -> Result<(), RegistrationError> {
        self.validate_for(&self.store.id)
    }

    fn validate_for(&self, publisher_db_id: &str) -> Result<(), RegistrationError> {
        // Stop serializing as soon as the request exceeds its envelope, rather
        // than allocating an unbounded temporary encoding for a typed caller.
        serde_json::to_writer(BoundedCount(0), self).map_err(|_| {
            RegistrationError::InvalidRequest("registration request exceeds its byte bound".into())
        })?;
        if self.expected_catalog_sha256.as_ref().is_some_and(|hash| {
            hash.len() != 64
                || !hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Err(RegistrationError::InvalidRequest(
                "expected_catalog_sha256 must be 64 lowercase hexadecimal characters".into(),
            ));
        }
        #[derive(Serialize)]
        struct Document<'a> {
            schema: &'static str,
            stores: BTreeMap<&'a str, &'a StoreDeclaration>,
        }
        let text = toml_edit::ser::to_string(&Document {
            schema: DISCOVERY_CATALOG_SCHEMA,
            stores: BTreeMap::from([(self.label.as_str(), &self.store)]),
        })
        .map_err(|_| {
            RegistrationError::InvalidRequest("cannot encode declaration as TOML".into())
        })?;
        PublishedCatalog::parse_toml(&text, publisher_db_id)
            .map_err(|error| RegistrationError::InvalidRequest(error.to_string()))?;
        Ok(())
    }
}

struct BoundedCount(usize);
impl Write for BoundedCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_REGISTRATION_BYTES.saturating_sub(self.0) {
            return Err(io::Error::other("registration byte bound exceeded"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationStatus {
    Added,
    AlreadyPresent,
}

/// A successful result confirms the declaration is visible. `durable=false`
/// means publication succeeded but durability could not be confirmed; retrying
/// the identical declaration is safe, even with a stale expected revision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegistrationResult {
    pub status: RegistrationStatus,
    pub publisher_db_id: String,
    /// The existing label for an identity-idempotent retry, not necessarily the
    /// label requested by that retry.
    pub label: String,
    pub before_catalog_sha256: String,
    pub catalog_sha256: String,
    pub durable: bool,
    pub warning: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistrationError {
    #[error("invalid discovery registration request: {0}")]
    InvalidRequest(String),
    #[error("invalid existing discovery catalog: {0}; repair the catalog before registering")]
    InvalidCatalog(String),
    #[error(
        "discovery registration would produce an invalid catalog: {0}; the existing catalog was not changed"
    )]
    OutputRejected(String),
    #[error(
        "discovery registration conflicts with {0}; existing declarations are never overwritten"
    )]
    Conflict(&'static str),
    #[error(
        "discovery catalog revision changed (expected {expected}, found {actual}); reread and retry"
    )]
    RevisionMismatch { expected: String, actual: String },
    #[error("discovery catalog registration is busy; retry after the current writer finishes")]
    Busy,
    #[error(
        "discovery catalog target is read-only or Nix-managed; configure a writable publication file"
    )]
    ReadOnly,
    #[error(
        "discovery catalog changed during registration; no replacement was published; reread and retry"
    )]
    Changed,
    #[error("cannot {operation} for discovery registration: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
}

fn io_error(operation: &'static str, source: io::Error) -> RegistrationError {
    RegistrationError::Io { operation, source }
}

/// Register one declaration in an existing catalog, without opening any store.
///
/// This blocking operation waits at most two seconds for its cooperative lock.
/// Callers on an async executor should use their blocking-work adapter. Intended
/// configuration symlinks are followed; their resolved file is replaced, not the
/// symlink. The original file permissions and Unix group are retained; this does
/// not copy extended ACLs or retain a different Unix owning user. The target must be
/// writable even for an idempotent retry: registration is a writable publication
/// operation, not a substitute for the read-only catalog API.
pub fn register_catalog(
    path: &Path,
    publisher_db_id: &str,
    request: &RegistrationRequest,
) -> Result<RegistrationResult, RegistrationError> {
    request.validate_for(publisher_db_id)?;
    let target = path.canonicalize().map_err(|error| {
        io_error(
            "resolve existing catalog (repair absent or broken symlink targets)",
            error,
        )
    })?;
    check_writable_target(&target)?;
    let _lock = CatalogLock::acquire(&target, LOCK_WAIT)?;
    if path.canonicalize().ok().as_ref() != Some(&target) {
        return Err(RegistrationError::Changed);
    }
    let source = read_catalog(&target)?;
    let before = parse_catalog(&source.bytes, publisher_db_id)?;
    if let Some((label, existing)) = before
        .stores
        .iter()
        .find(|(_, store)| store.id == request.store.id)
    {
        if existing != &request.store {
            return Err(RegistrationError::Conflict(
                "a different declaration for the same database identity",
            ));
        }
        // Identity idempotence takes precedence over label/revision conflicts.
        // Sync the observed file and parent so a retry can confirm durability
        // after a previous response was lost or its directory sync failed.
        check_unchanged(path, &target, &source)?;
        let durability = sync_existing(&target).and_then(|()| sync_directory(parent(&target)));
        return Ok(result(
            RegistrationStatus::AlreadyPresent,
            &before,
            &before,
            label.clone(),
            durability,
        ));
    }
    if before.stores.contains_key(&request.label) {
        return Err(RegistrationError::Conflict(
            "an occupied label for another database identity",
        ));
    }
    if let Some(expected) = &request.expected_catalog_sha256 {
        if expected != &before.catalog_sha256 {
            return Err(RegistrationError::RevisionMismatch {
                expected: expected.clone(),
                actual: before.catalog_sha256,
            });
        }
    }

    let text = std::str::from_utf8(&source.bytes).expect("parse_catalog checked UTF-8");
    let mut document = text.parse::<toml_edit::DocumentMut>().map_err(|_| {
        RegistrationError::InvalidCatalog("cannot preserve existing TOML document".into())
    })?;
    if !document.contains_key("stores") {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        document.insert("stores", toml_edit::Item::Table(table));
    }
    let entry = toml_edit::ser::to_document(&request.store).map_err(|_| {
        RegistrationError::InvalidRequest("cannot encode declaration as TOML".into())
    })?;
    document["stores"]
        .as_table_like_mut()
        .ok_or_else(|| RegistrationError::InvalidCatalog("stores must be a TOML table".into()))?
        .insert(&request.label, toml_edit::Item::Table(entry.into_table()));
    let output = document.to_string();
    // Reparse the actual output, including its byte bound and semantic digest;
    // never serialize the runtime PublishedCatalog identity/hash fields to TOML.
    let after = PublishedCatalog::parse_toml(&output, publisher_db_id)
        .map_err(|error| RegistrationError::OutputRejected(error.to_string()))?;
    let durability = publish(path, &target, &source, output.as_bytes(), sync_directory)?;
    Ok(result(
        RegistrationStatus::Added,
        &before,
        &after,
        request.label.clone(),
        durability,
    ))
}

fn result(
    status: RegistrationStatus,
    before: &PublishedCatalog,
    after: &PublishedCatalog,
    label: String,
    durability: io::Result<()>,
) -> RegistrationResult {
    let warning = durability.err().map(|error| {
        if error.kind() == io::ErrorKind::Unsupported {
            "registration is visible but directory durability confirmation is unavailable on this platform".into()
        } else {
            "registration is visible but filesystem durability could not be confirmed; retry the identical declaration to confirm".into()
        }
    });
    RegistrationResult {
        status,
        publisher_db_id: after.publisher_db_id.clone(),
        label,
        before_catalog_sha256: before.catalog_sha256.clone(),
        catalog_sha256: after.catalog_sha256.clone(),
        durable: warning.is_none(),
        warning,
    }
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .expect("canonical regular target has a parent")
}

fn check_writable_target(path: &Path) -> Result<Metadata, RegistrationError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|e| io_error("inspect catalog target", e))?;
    if !metadata.is_file() || metadata.len() > MAX_CATALOG_BYTES as u64 {
        return Err(RegistrationError::InvalidCatalog(
            "target must be a bounded regular file".into(),
        ));
    }
    if metadata.permissions().readonly() || path.starts_with("/nix/store") {
        return Err(RegistrationError::ReadOnly);
    }
    Ok(metadata)
}

struct CatalogSource {
    bytes: Vec<u8>,
    permissions: Permissions,
    #[cfg(unix)]
    group: u32,
}

fn read_catalog(path: &Path) -> Result<CatalogSource, RegistrationError> {
    check_writable_target(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    nofollow_nonblock(&mut options);
    let file = options
        .open(path)
        .map_err(|e| io_error("read bounded catalog", e))?;
    let metadata = file
        .metadata()
        .map_err(|e| io_error("inspect open catalog", e))?;
    if !metadata.is_file() || metadata.len() > MAX_CATALOG_BYTES as u64 {
        return Err(RegistrationError::InvalidCatalog(
            "target must be a bounded regular file".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CATALOG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error("read bounded catalog", e))?;
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(RegistrationError::InvalidCatalog(
            "catalog exceeds byte bound".into(),
        ));
    }
    Ok(CatalogSource {
        bytes,
        permissions: metadata.permissions(),
        #[cfg(unix)]
        group: {
            use std::os::unix::fs::MetadataExt;
            metadata.gid()
        },
    })
}

fn parse_catalog(
    bytes: &[u8],
    publisher_db_id: &str,
) -> Result<PublishedCatalog, RegistrationError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| RegistrationError::InvalidCatalog("catalog must be UTF-8".into()))?;
    PublishedCatalog::parse_toml(text, publisher_db_id)
        .map_err(|e| RegistrationError::InvalidCatalog(e.to_string()))
}

fn nofollow_nonblock(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    let _ = options;
}

fn lock_path(target: &Path) -> PathBuf {
    let mut filename = target
        .file_name()
        .expect("regular target has a filename")
        .to_os_string();
    filename.push(".registration.lock");
    target.with_file_name(filename)
}

/// Kept on disk: unlinking this lock would allow two different locked inodes.
struct CatalogLock(File);
impl CatalogLock {
    fn acquire(target: &Path, timeout: Duration) -> Result<Self, RegistrationError> {
        let target_metadata = check_writable_target(target)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        nofollow_nonblock(&mut options);
        let lock_path = lock_path(target);
        let file = match options.open(&lock_path) {
            Ok(file) => {
                // Shared catalogs do not inherit a private-cache mode. Only the
                // creator sets access bits/group; an existing lock is untouched.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{MetadataExt, PermissionsExt};
                    retain_group(&lock_path, target_metadata.gid())
                        .map_err(|e| io_error("retain catalog registration lock group", e))?;
                    file.set_permissions(Permissions::from_mode(
                        target_metadata.permissions().mode() & 0o666,
                    ))
                    .map_err(|e| io_error("retain catalog registration lock access", e))?;
                }
                #[cfg(not(unix))]
                let _ = target_metadata;
                file
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&lock_path)
                    .map_err(|e| io_error("inspect catalog registration lock", e))?;
                if !metadata.is_file() {
                    return Err(RegistrationError::InvalidCatalog(
                        "registration lock must be an empty regular file".into(),
                    ));
                }
                options.create_new(false);
                options
                    .open(&lock_path)
                    .map_err(|e| io_error("open catalog registration lock", e))?
            }
            Err(error) => return Err(io_error("create catalog registration lock", error)),
        };
        let metadata = file
            .metadata()
            .map_err(|e| io_error("inspect catalog registration lock", e))?;
        if !metadata.is_file() || metadata.len() != 0 {
            return Err(RegistrationError::InvalidCatalog(
                "registration lock must be an empty regular file".into(),
            ));
        }
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self(file)),
                Err(std::fs::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(RegistrationError::Busy);
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(10)));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(io_error("lock catalog registration", error));
                }
            }
        }
    }
}
impl Drop for CatalogLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Outer error means no rename; the inner durability result follows a successful
/// rename and must never be converted into a claim that publication did not occur.
fn publish(
    logical_path: &Path,
    target: &Path,
    source: &CatalogSource,
    bytes: &[u8],
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<io::Result<()>, RegistrationError> {
    let temporary = parent(target).join(format!(".mneme-catalog-{}.tmp", ulid::Ulid::new()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|e| io_error("create catalog temporary file", e))?;
    let _cleanup = Temporary(temporary.clone());
    file.write_all(bytes)
        .map_err(|e| io_error("write catalog temporary file", e))?;
    #[cfg(unix)]
    retain_group(&temporary, source.group).map_err(|e| io_error("retain catalog file group", e))?;
    file.set_permissions(source.permissions.clone())
        .map_err(|e| io_error("retain catalog file permissions", e))?;
    file.sync_all()
        .map_err(|e| io_error("sync catalog temporary file", e))?;
    drop(file);
    check_unchanged(logical_path, target, source)?;
    std::fs::rename(&temporary, target)
        .map_err(|e| io_error("atomically replace catalog target", e))?;
    Ok(sync_parent(parent(target)))
}

fn check_unchanged(
    logical_path: &Path,
    target: &Path,
    source: &CatalogSource,
) -> Result<(), RegistrationError> {
    if logical_path.canonicalize().ok().as_deref() != Some(target) {
        return Err(RegistrationError::Changed);
    }
    let current = read_catalog(target)?;
    if current.bytes != source.bytes || current.permissions != source.permissions {
        return Err(RegistrationError::Changed);
    }
    #[cfg(unix)]
    if current.group != source.group {
        return Err(RegistrationError::Changed);
    }
    Ok(())
}

#[cfg(unix)]
fn retain_group(path: &Path, group: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if std::fs::metadata(path)?.gid() != group {
        std::os::unix::fs::chown(path, None, Some(group))?;
    }
    Ok(())
}

fn sync_existing(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    // Windows FlushFileBuffers requires a writable handle. Registration already
    // requires a writable target, even when this call is an identity retry.
    options.read(true).write(true);
    nofollow_nonblock(&mut options);
    options.open(path)?.sync_all()
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory durability confirmation is not supported on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    const PUBLISHER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const FIRST: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
    const SECOND: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
    const EMPTY: &str =
        "# authored header\nschema = 'mneme.discovery-catalog.v1' # schema comment\n";

    fn request(label: &str, id: &str) -> RegistrationRequest {
        RegistrationRequest {
            label: label.into(),
            store: StoreDeclaration {
                id: id.into(),
                ..StoreDeclaration::default()
            },
            expected_catalog_sha256: None,
        }
    }

    fn fixture(text: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("catalog.toml");
        std::fs::write(&path, text).unwrap();
        (directory, path.canonicalize().unwrap())
    }

    fn read(path: &Path) -> PublishedCatalog {
        parse_catalog(&std::fs::read(path).unwrap(), PUBLISHER).unwrap()
    }

    #[test]
    fn add_preserves_comments_entries_and_semantic_revision() {
        let text = format!(
            "{EMPTY}\n[stores.existing] # store comment\nid = '{FIRST}' # identity comment\ndiscovery = false\n"
        );
        let (_dir, path) = fixture(&text);
        let before = read(&path);
        let mut registration = request("new.store", SECOND);
        registration.expected_catalog_sha256 = Some(before.catalog_sha256.clone());
        let result = register_catalog(&path, PUBLISHER, &registration).unwrap();
        assert_eq!(result.status, RegistrationStatus::Added);
        assert_eq!(result.label, "new.store");
        assert_eq!(result.publisher_db_id, PUBLISHER);
        assert_eq!(result.before_catalog_sha256, before.catalog_sha256);
        assert_eq!(result.catalog_sha256, read(&path).catalog_sha256);
        assert_eq!(read(&path).stores["existing"], before.stores["existing"]);
        assert_eq!(read(&path).stores["new.store"], registration.store);
        let output = std::fs::read_to_string(&path).unwrap();
        for comment in [
            "# authored header",
            "# schema comment",
            "# store comment",
            "# identity comment",
        ] {
            assert!(output.contains(comment), "lost {comment}: {output}");
        }
        assert!(!output.contains("publisher_db_id"));
        assert!(!output.contains("catalog_sha256"));
        #[cfg(unix)]
        assert!(result.durable);
        assert!(lock_path(&path).exists());
    }

    #[test]
    fn inline_and_dotted_store_tables_preserve_existing_formatting() {
        for text in [
            format!(
                "{EMPTY}stores = {{ existing = {{ id = '{FIRST}', discovery = false }} }} # inline\n"
            ),
            format!("{EMPTY}stores.existing.id = '{FIRST}' # dotted\n"),
            format!("{EMPTY}[stores]\nexisting = {{ id = '{FIRST}' }} # mixed\n"),
        ] {
            let (_dir, path) = fixture(&text);
            let result = register_catalog(&path, PUBLISHER, &request("new.store", SECOND)).unwrap();
            assert_eq!(result.status, RegistrationStatus::Added);
            let output = std::fs::read_to_string(&path).unwrap();
            assert!(output.contains(&format!("id = '{FIRST}'")));
            assert!(output.contains(text.lines().last().unwrap().split('#').next_back().unwrap()));
            assert_eq!(read(&path).stores.len(), 2);
            assert!(read(&path).stores.contains_key("new.store"));
        }
        let (_dir, path) = fixture(&format!("{EMPTY}stores = {{}} # empty inline\n"));
        register_catalog(&path, PUBLISHER, &request("new.store", SECOND)).unwrap();
        assert_eq!(read(&path).stores["new.store"].id, SECOND);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# empty inline")
        );
    }

    #[test]
    fn exact_retry_ignores_stale_revision_and_requested_label_collision() {
        let (_dir, path) = fixture(EMPTY);
        let first = request("first", FIRST);
        let initial = register_catalog(&path, PUBLISHER, &first).unwrap();
        register_catalog(&path, PUBLISHER, &request("second", SECOND)).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let mut retry = first;
        retry.label = "second".into();
        retry.expected_catalog_sha256 = Some(initial.before_catalog_sha256);
        let result = register_catalog(&path, PUBLISHER, &retry).unwrap();
        assert_eq!(result.status, RegistrationStatus::AlreadyPresent);
        assert_eq!(result.label, "first");
        assert_eq!(result.before_catalog_sha256, result.catalog_sha256);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn conflicting_identity_label_and_revision_do_not_replace_bytes() {
        let (_dir, path) = fixture(EMPTY);
        register_catalog(&path, PUBLISHER, &request("first", FIRST)).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let mut identity_conflict = request("different-label", FIRST);
        identity_conflict.store.discovery = Some(false);
        assert!(matches!(
            register_catalog(&path, PUBLISHER, &identity_conflict),
            Err(RegistrationError::Conflict(_))
        ));
        assert!(matches!(
            register_catalog(&path, PUBLISHER, &request("first", SECOND)),
            Err(RegistrationError::Conflict(_))
        ));
        let mut revision_conflict = request("second", SECOND);
        revision_conflict.expected_catalog_sha256 = Some("0".repeat(64));
        assert!(matches!(
            register_catalog(&path, PUBLISHER, &revision_conflict),
            Err(RegistrationError::RevisionMismatch { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn typed_request_validation_happens_before_filesystem_work() {
        let (dir, path) = fixture(EMPTY);
        let mut invalids = vec![
            request("invalid label", FIRST),
            request("okay", "not-an-id"),
        ];
        let mut invalid_hash = request("okay", FIRST);
        invalid_hash.expected_catalog_sha256 = Some("wrong".into());
        invalids.push(invalid_hash);
        let mut huge = request("okay", FIRST);
        huge.store.clear = vec!["x".repeat(MAX_REGISTRATION_BYTES + 1)];
        invalids.push(huge);
        let mut secret = request("okay", FIRST);
        secret.store.owner = Some(mneme_config::OwnerDeclaration {
            url: Some("http://localhost:3000/mcp".into()),
            db: Some("store".into()),
            token_env: Some("TOKEN_ENV_NAME".into()),
            ..Default::default()
        });
        invalids.push(secret);
        for invalid in invalids {
            assert!(matches!(
                register_catalog(&path, PUBLISHER, &invalid),
                Err(RegistrationError::InvalidRequest(_))
            ));
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EMPTY);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let mut json = serde_json::to_value(request("okay", FIRST)).unwrap();
        json["unknown"] = true.into();
        assert!(serde_json::from_value::<RegistrationRequest>(json).is_err());
    }

    #[test]
    fn malformed_unknown_schema_and_oversized_catalogs_remain_untouched() {
        for text in [
            "schema = 'future-schema'".into(),
            "not toml".into(),
            format!("{EMPTY}publisher_db_id = '{PUBLISHER}'\n"),
            format!("{EMPTY}#{}", "x".repeat(MAX_CATALOG_BYTES)),
        ] {
            let (_dir, path) = fixture(&text);
            assert!(register_catalog(&path, PUBLISHER, &request("first", FIRST)).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
    }

    #[test]
    fn serialized_output_and_store_count_bounds_preserve_old_bytes() {
        let padded = format!(
            "{EMPTY}#{}\n",
            "x".repeat(MAX_CATALOG_BYTES - EMPTY.len() - 10)
        );
        let (_dir, path) = fixture(&padded);
        assert!(read(&path).stores.is_empty());
        assert!(register_catalog(&path, PUBLISHER, &request("first", FIRST)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), padded);

        let mut text = EMPTY.to_string();
        for index in 0..mneme_config::MAX_STORES {
            text.push_str(&format!(
                "\n[stores.s{index}]\nid = '{}'\n",
                ulid::Ulid::new()
            ));
        }
        let (_dir, path) = fixture(&text);
        assert_eq!(read(&path).stores.len(), mneme_config::MAX_STORES);
        assert!(register_catalog(&path, PUBLISHER, &request("first", FIRST)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn concurrent_distinct_additions_do_not_lose_entries() {
        let (_dir, path) = fixture(EMPTY);
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|index| {
                let barrier = barrier.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    let request = request(&format!("store{index}"), &ulid::Ulid::new().to_string());
                    barrier.wait();
                    register_catalog(&path, PUBLISHER, &request).unwrap()
                })
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap().status, RegistrationStatus::Added);
        }
        assert_eq!(read(&path).stores.len(), 8);
    }

    #[test]
    fn contention_is_bounded_and_lock_inode_is_retained() {
        let (_dir, path) = fixture(EMPTY);
        let held = CatalogLock::acquire(&path, LOCK_WAIT).unwrap();
        let start = Instant::now();
        assert!(matches!(
            CatalogLock::acquire(&path, Duration::from_millis(30)),
            Err(RegistrationError::Busy)
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(held);
        assert!(lock_path(&path).exists());
        CatalogLock::acquire(&path, Duration::ZERO).unwrap();
    }

    #[test]
    fn outside_edit_before_publish_is_not_overwritten_and_temp_is_cleaned() {
        let (dir, path) = fixture(EMPTY);
        let source = read_catalog(&path).unwrap();
        let replacement = format!("{EMPTY}# external editor\n");
        std::fs::write(&path, &replacement).unwrap();
        assert!(matches!(
            publish(
                &path,
                &path,
                &source,
                b"should not be published",
                sync_directory
            ),
            Err(RegistrationError::Changed)
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), replacement);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn post_rename_directory_sync_failure_reports_visible_not_durable() {
        let (dir, path) = fixture(EMPTY);
        let source = read_catalog(&path).unwrap();
        let before = read(&path);
        let output = format!("{EMPTY}\n[stores.first]\nid = '{FIRST}'\n");
        let after = parse_catalog(output.as_bytes(), PUBLISHER).unwrap();
        let durability = publish(&path, &path, &source, output.as_bytes(), |_| {
            Err(io::Error::other("injected directory sync failure"))
        })
        .unwrap();
        let result = result(
            RegistrationStatus::Added,
            &before,
            &after,
            "first".into(),
            durability,
        );
        assert_eq!(result.status, RegistrationStatus::Added);
        assert!(!result.durable);
        assert!(result.warning.unwrap().contains("visible"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), output);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let retry = register_catalog(&path, PUBLISHER, &request("first", FIRST)).unwrap();
        assert_eq!(retry.status, RegistrationStatus::AlreadyPresent);
        #[cfg(unix)]
        assert!(retry.durable);
    }

    #[test]
    fn unsupported_durability_does_not_recommend_an_impossible_retry() {
        let before = parse_catalog(EMPTY.as_bytes(), PUBLISHER).unwrap();
        let result = result(
            RegistrationStatus::AlreadyPresent,
            &before,
            &before,
            "first".into(),
            Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported")),
        );
        assert!(!result.durable);
        let warning = result.warning.unwrap();
        assert!(warning.contains("unavailable on this platform"));
        assert!(!warning.contains("retry"));
    }

    #[cfg(unix)]
    #[test]
    fn operator_symlink_and_shared_permissions_are_preserved() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let (dir, target) = fixture(EMPTY);
        std::fs::set_permissions(&target, Permissions::from_mode(0o664)).unwrap();
        let group = std::fs::metadata(&target).unwrap().gid();
        let link = dir.path().join("selected.toml");
        symlink("catalog.toml", &link).unwrap();
        register_catalog(&link, PUBLISHER, &request("first", FIRST)).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("catalog.toml")
        );
        assert_eq!(read(&link).stores["first"].id, FIRST);
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o664
        );
        assert!(lock_path(&target).exists());
        assert!(!lock_path(&link).exists());
        let metadata = std::fs::metadata(lock_path(&target)).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o664);
        assert_eq!(metadata.gid(), group);
        assert_eq!(std::fs::metadata(&target).unwrap().gid(), group);

        // Operator changes to an existing lock's access are not rewritten.
        std::fs::set_permissions(lock_path(&target), Permissions::from_mode(0o600)).unwrap();
        register_catalog(&link, PUBLISHER, &request("second", SECOND)).unwrap();
        assert_eq!(
            std::fs::metadata(lock_path(&target))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn lock_symlinks_and_nonempty_files_are_rejected_without_truncation() {
        use std::os::unix::fs::symlink;
        let (dir, target) = fixture(EMPTY);
        let other = dir.path().join("other");
        std::fs::write(&other, "other operator data").unwrap();
        symlink(&other, lock_path(&target)).unwrap();
        assert!(register_catalog(&target, PUBLISHER, &request("first", FIRST)).is_err());
        assert_eq!(
            std::fs::read_to_string(&other).unwrap(),
            "other operator data"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), EMPTY);
        std::fs::remove_file(lock_path(&target)).unwrap();
        std::fs::write(lock_path(&target), "not an empty lock").unwrap();
        assert!(register_catalog(&target, PUBLISHER, &request("first", FIRST)).is_err());
        assert_eq!(
            std::fs::read_to_string(lock_path(&target)).unwrap(),
            "not an empty lock"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_only_target_and_changed_symlink_are_not_replaced() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (dir, path) = fixture(EMPTY);
        std::fs::set_permissions(&path, Permissions::from_mode(0o444)).unwrap();
        assert!(matches!(
            register_catalog(&path, PUBLISHER, &request("first", FIRST)),
            Err(RegistrationError::ReadOnly)
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EMPTY);
        assert!(!lock_path(&path).exists());
        std::fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        let source = read_catalog(&path).unwrap();
        let link = dir.path().join("selected.toml");
        symlink("elsewhere.toml", &link).unwrap();
        std::fs::write(dir.path().join("elsewhere.toml"), EMPTY).unwrap();
        assert!(matches!(
            publish(&link, &path, &source, b"must not replace", sync_directory),
            Err(RegistrationError::Changed)
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EMPTY);
    }

    #[test]
    fn missing_target_is_not_created() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.toml");
        assert!(register_catalog(&path, PUBLISHER, &request("first", FIRST)).is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
