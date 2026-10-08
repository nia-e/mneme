//! mneme-body — [`BodyStore`] adapters, dispatched by URI scheme.
//!
//! A node holds only a [`BodyRef`] (a `scheme://...` URI); the full body lives
//! wherever that scheme resolves. The engine keeps a registry of stores keyed by
//! [`BodyStore::scheme`] and routes resolution accordingly. That indirection is
//! the point: a summary stays small and embeddable, the body is fetched only
//! once a node clears the relevance threshold, and *where* it lives (memory,
//! disk, the network) is swappable without touching the graph.
//!
//! Two stores ship here, both std-only:
//! - [`InlineStore`] (`inline://`) — bytes held in-process. The default for
//!   ingested-on-the-fly content; nothing to clean up, nothing external.
//! - [`FsStore`] (`fs://`) — bytes on the local filesystem under a base dir.
//!
//! An `https://` store (cached web fetches) is the natural third adapter; it
//! needs an HTTP client, so it's left for when a network dep is acceptable.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use mneme_core::BodyRef;
use mneme_core::ports::{BodyChunk, BodyStore, Error, Result};
use ulid::Ulid;

fn checked_probe_limit(max_bytes: usize) -> Result<u64> {
    let with_probe = max_bytes.checked_add(1).ok_or_else(|| {
        Error::InvalidInput("body range max_bytes overflows its continuation probe".into())
    })?;
    u64::try_from(with_probe)
        .map_err(|_| Error::InvalidInput("body range read limit does not fit u64".into()))
}

fn finish_chunk(bytes: Vec<u8>, source_start: u64, has_more: bool) -> Result<BodyChunk> {
    let byte_len = u64::try_from(bytes.len())
        .map_err(|_| Error::InvalidInput("body chunk length does not fit u64".into()))?;
    let source_end = source_start
        .checked_add(byte_len)
        .ok_or_else(|| Error::InvalidInput("body range end overflows the source offset".into()))?;
    Ok(BodyChunk {
        bytes,
        source_start,
        source_end,
        next_offset: has_more.then_some(source_end),
    })
}

fn read_seek_range<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    max_bytes: usize,
) -> Result<BodyChunk> {
    let read_limit = checked_probe_limit(max_bytes)?;
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|error| Error::Body(format!("seek bounded body range: {error}")))?;
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024).saturating_add(1));
    reader
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::Body(format!("read bounded body range: {error}")))?;
    let has_more = bytes.len() > max_bytes;
    bytes.truncate(max_bytes);
    finish_chunk(bytes, offset, has_more)
}

/// `inline://` — bodies kept in memory, keyed by a minted ULID. Resolution is a
/// map lookup; nothing touches the disk or network.
#[derive(Default)]
pub struct InlineStore {
    blobs: Mutex<HashMap<BodyRef, Vec<u8>>>,
}

impl InlineStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl BodyStore for InlineStore {
    fn scheme(&self) -> &'static str {
        "inline"
    }

    async fn get(&self, body: &BodyRef) -> Result<Vec<u8>> {
        self.blobs
            .lock()
            .expect("inline store mutex poisoned")
            .get(body)
            .cloned()
            .ok_or_else(|| Error::Body(format!("no inline body for {}", body.as_str())))
    }

    async fn get_range(&self, body: &BodyRef, offset: u64, max_bytes: usize) -> Result<BodyChunk> {
        // Keep the same representability contract as streaming stores even
        // though this adapter can inspect its in-memory length directly.
        checked_probe_limit(max_bytes)?;
        let blobs = self.blobs.lock().expect("inline store mutex poisoned");
        let bytes = blobs
            .get(body)
            .ok_or_else(|| Error::Body(format!("no inline body for {}", body.as_str())))?;
        let body_len = u64::try_from(bytes.len())
            .map_err(|_| Error::InvalidInput("inline body length does not fit u64".into()))?;
        if offset >= body_len {
            return finish_chunk(Vec::new(), offset, false);
        }
        let start = usize::try_from(offset)
            .map_err(|_| Error::InvalidInput("body range offset does not fit usize".into()))?;
        let end = start.saturating_add(max_bytes).min(bytes.len());
        finish_chunk(bytes[start..end].to_vec(), offset, end < bytes.len())
    }

    async fn put(&self, bytes: &[u8]) -> Result<BodyRef> {
        let body = BodyRef::new(format!("inline://{}", Ulid::new()))
            .map_err(|error| Error::Body(format!("mint inline body reference: {error}")))?;
        self.blobs
            .lock()
            .expect("inline store mutex poisoned")
            .insert(body.clone(), bytes.to_vec());
        Ok(body)
    }

    async fn delete(&self, body: &BodyRef) -> Result<()> {
        self.blobs
            .lock()
            .expect("inline store mutex poisoned")
            .remove(body);
        Ok(())
    }
}

/// `fs://` — bodies as files under `base`. A `put` mints `fs://<base>/<ulid>`;
/// `get` reads the path back out. The scheme prefix is stripped to recover the
/// path, so the URI is self-describing.
pub struct FsStore {
    base: PathBuf,
}

impl FsStore {
    /// Create the store, ensuring `base` exists.
    pub fn new(base: impl Into<PathBuf>) -> Result<Self> {
        let base = base.into();
        std::fs::create_dir_all(&base).map_err(|e| Error::Body(format!("create {base:?}: {e}")))?;
        set_private_dir(&base)?;
        let base = std::fs::canonicalize(&base)
            .map_err(|e| Error::Body(format!("canonicalize {base:?}: {e}")))?;
        Ok(Self { base })
    }

    /// Open an already-admitted body directory without creating, repairing, or
    /// changing it.
    ///
    /// This is the constructor for a body root selected from a managed store
    /// generation. `base` must be absolute. On Unix, the final named object
    /// must be a current-owner directory with exact mode `0700`; symlinks are
    /// rejected. Its identity is checked before and after canonicalization so a
    /// simple replacement race fails closed. Other platforms fail as
    /// unsupported until they have an equivalent identity-and-permission
    /// admission policy.
    ///
    /// The returned `PathBuf` is deliberately not advertised as an fd-backed
    /// namespace authority. A same-UID process can still replace a path after
    /// this function returns, and `std` cannot keep every ancestor pinned while
    /// canonicalizing it. The managed generation/root guard must remain held
    /// for the lifetime of a store opened through this constructor.
    pub fn open_existing(base: impl Into<PathBuf>) -> Result<Self> {
        let base = base.into();
        if !base.is_absolute() {
            return Err(Error::Body(format!(
                "existing fs body root must be absolute: {base:?}"
            )));
        }

        let base = admit_existing_fs_root(&base)?;
        Ok(Self { base })
    }

    /// Resolve a ref under this store's base. New refs contain only an opaque
    /// filename; legacy absolute refs remain readable iff their canonical target
    /// is still inside the configured base.
    fn resolve(&self, body: &BodyRef) -> Result<PathBuf> {
        let raw = body
            .as_str()
            .strip_prefix("fs://")
            .ok_or_else(|| Error::Body(format!("not an fs:// ref: {}", body.as_str())))?;
        if raw.is_empty() {
            return Err(Error::Body("empty fs:// ref".into()));
        }
        let raw = Path::new(raw);
        let path = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.base.join(raw)
        };
        let canonical = std::fs::canonicalize(&path)
            .map_err(|e| Error::Body(format!("resolve {path:?}: {e}")))?;
        if !canonical.starts_with(&self.base) {
            return Err(Error::Body(format!(
                "fs body escapes configured base {:?}",
                self.base
            )));
        }
        Ok(canonical)
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UnixDirectoryIdentity {
    device: u64,
    inode: u64,
    owner: u32,
    mode: u32,
}

#[cfg(unix)]
impl UnixDirectoryIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            owner: metadata.uid(),
            mode: metadata.permissions().mode() & 0o7777,
        }
    }
}

#[cfg(unix)]
fn current_effective_uid() -> u32 {
    // `std` exposes a file's owner but not the process's effective UID. Keep
    // this tiny FFI shim local rather than making body storage depend on a
    // general Unix abstraction crate.
    unsafe extern "C" {
        fn geteuid() -> u32;
    }

    // SAFETY: `geteuid` has no preconditions and only reads process identity.
    unsafe { geteuid() }
}

#[cfg(unix)]
fn validate_existing_fs_root(path: &Path, expected_owner: u32) -> Result<UnixDirectoryIdentity> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| Error::Body(format!("inspect existing body root {path:?}: {error}")))?;
    let mode = metadata.permissions().mode() & 0o7777;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != expected_owner
        || mode != 0o700
    {
        return Err(Error::Body(format!(
            "existing body root {path:?} must be a current-owner directory with exact mode 0700 (owner {}, mode {mode:04o})",
            metadata.uid()
        )));
    }
    Ok(UnixDirectoryIdentity::from_metadata(&metadata))
}

#[cfg(unix)]
fn admit_existing_fs_root(path: &Path) -> Result<PathBuf> {
    admit_existing_fs_root_for_owner(path, current_effective_uid())
}

#[cfg(unix)]
fn admit_existing_fs_root_for_owner(path: &Path, expected_owner: u32) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    // A trailing separator makes `symlink_metadata("link/")` follow the final
    // symlink on Unix. Require one unambiguous lexical spelling so the first
    // check really does inspect the exact final named object. Dot segments and
    // repeated separators are rejected for the same reason.
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > 1
        && (bytes.ends_with(b"/")
            || bytes[1..]
                .split(|byte| *byte == b'/')
                .any(|component| component.is_empty() || component == b"." || component == b".."))
    {
        return Err(Error::Body(format!(
            "existing body root must have a normalized absolute spelling: {path:?}"
        )));
    }

    let original = validate_existing_fs_root(path, expected_owner)?;
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        Error::Body(format!("canonicalize existing body root {path:?}: {error}"))
    })?;
    let canonical_identity = validate_existing_fs_root(&canonical, expected_owner)?;
    let named_again = validate_existing_fs_root(path, expected_owner)?;
    if canonical_identity != original || named_again != original {
        return Err(Error::Body(format!(
            "existing body root {path:?} changed while it was admitted"
        )));
    }
    Ok(canonical)
}

#[cfg(not(unix))]
fn admit_existing_fs_root(_path: &Path) -> Result<PathBuf> {
    Err(Error::Body(
        "unsupported: existing-only fs body-root admission is currently supported only on Unix"
            .into(),
    ))
}

#[async_trait]
impl BodyStore for FsStore {
    fn scheme(&self) -> &'static str {
        "fs"
    }

    async fn get(&self, body: &BodyRef) -> Result<Vec<u8>> {
        let path = self.resolve(body)?;
        std::fs::read(&path).map_err(|e| Error::Body(format!("read {path:?}: {e}")))
    }

    async fn get_range(&self, body: &BodyRef, offset: u64, max_bytes: usize) -> Result<BodyChunk> {
        let path = self.resolve(body)?;
        let mut file =
            std::fs::File::open(&path).map_err(|e| Error::Body(format!("open {path:?}: {e}")))?;
        // Some platforms expose signed filesystem offsets even though Rust's
        // API accepts u64. Preserve the range contract for every u64 by avoiding
        // a seek when the requested offset is already at/past known EOF.
        checked_probe_limit(max_bytes)?;
        let source_len = file
            .metadata()
            .map_err(|e| Error::Body(format!("stat {path:?}: {e}")))?
            .len();
        if offset >= source_len {
            return finish_chunk(Vec::new(), offset, false);
        }
        read_seek_range(&mut file, offset, max_bytes)
    }

    async fn put(&self, bytes: &[u8]) -> Result<BodyRef> {
        use std::io::Write;

        let name = Ulid::new().to_string();
        let body = BodyRef::new(format!("fs://{name}"))
            .map_err(|error| Error::Body(format!("mint fs body reference: {error}")))?;
        let path = self.base.join(&name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|e| Error::Body(format!("create {path:?}: {e}")))?;
        file.write_all(bytes)
            .map_err(|e| Error::Body(format!("write {path:?}: {e}")))?;
        file.sync_all()
            .map_err(|e| Error::Body(format!("sync {path:?}: {e}")))?;
        std::fs::File::open(&self.base)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| Error::Body(format!("sync body directory {:?}: {e}", self.base)))?;
        // Relative refs are portable with the database/body directory and do not
        // disclose an absolute host path to every client that reads a node.
        Ok(body)
    }

    async fn delete(&self, body: &BodyRef) -> Result<()> {
        let raw = body
            .as_str()
            .strip_prefix("fs://")
            .ok_or_else(|| Error::Body(format!("not an fs:// ref: {}", body.as_str())))?;
        let raw = Path::new(raw);
        let untrusted = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.base.join(raw)
        };
        if !untrusted.exists() {
            return Ok(());
        }
        let path = self.resolve(body)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Body(format!("delete {path:?}: {e}"))),
        }
    }
}

#[cfg(unix)]
fn set_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| Error::Body(format!("chmod 0700 {path:?}: {e}")))
}

#[cfg(not(unix))]
fn set_private_dir(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("mneme-body-test-{}", Ulid::new()));
            std::fs::create_dir_all(&path).unwrap();
            set_private_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn expect_existing_root_refusal(path: &Path) -> Error {
        match FsStore::open_existing(path) {
            Ok(_) => panic!("unexpectedly admitted existing body root {path:?}"),
            Err(error) => error,
        }
    }

    #[cfg(unix)]
    #[derive(Debug, Eq, PartialEq)]
    struct MetadataSnapshot {
        device: u64,
        inode: u64,
        owner: u32,
        group: u32,
        mode: u32,
        links: u64,
        len: u64,
        modified_seconds: i64,
        modified_nanos: i64,
        changed_seconds: i64,
        changed_nanos: i64,
    }

    #[cfg(unix)]
    fn metadata_snapshot(path: &Path) -> MetadataSnapshot {
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::symlink_metadata(path).unwrap();
        MetadataSnapshot {
            device: metadata.dev(),
            inode: metadata.ino(),
            owner: metadata.uid(),
            group: metadata.gid(),
            mode: metadata.mode(),
            links: metadata.nlink(),
            len: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanos: metadata.ctime_nsec(),
        }
    }

    #[cfg(unix)]
    fn directory_entries(path: &Path) -> Vec<std::ffi::OsString> {
        let mut entries = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    #[cfg(unix)]
    fn make_private_directory(path: &Path) {
        std::fs::create_dir(path).unwrap();
        set_private_dir(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fs_open_existing_absence_is_non_mutating() {
        let scratch = Scratch::new();
        let missing = scratch.0.join("absent");
        let parent_metadata = metadata_snapshot(&scratch.0);
        let parent_entries = directory_entries(&scratch.0);

        let error = expect_existing_root_refusal(&missing);

        assert!(error.to_string().contains("inspect existing body root"));
        assert!(!missing.exists());
        assert_eq!(metadata_snapshot(&scratch.0), parent_metadata);
        assert_eq!(directory_entries(&scratch.0), parent_entries);
    }

    #[cfg(unix)]
    #[test]
    fn fs_open_existing_symlink_refusal_preserves_namespace_and_bytes() {
        use std::os::unix::fs::symlink;

        let scratch = Scratch::new();
        let target = scratch.0.join("target");
        let alias = scratch.0.join("alias");
        make_private_directory(&target);
        let sentinel = target.join("sentinel");
        std::fs::write(&sentinel, b"do not touch").unwrap();
        symlink(&target, &alias).unwrap();

        let parent_entries = directory_entries(&scratch.0);
        let target_entries = directory_entries(&target);
        let target_metadata = metadata_snapshot(&target);
        let link_metadata = metadata_snapshot(&alias);
        let sentinel_metadata = metadata_snapshot(&sentinel);
        let sentinel_bytes = std::fs::read(&sentinel).unwrap();

        let error = expect_existing_root_refusal(&alias);

        assert!(error.to_string().contains("exact mode 0700"));
        assert_eq!(directory_entries(&scratch.0), parent_entries);
        assert_eq!(directory_entries(&target), target_entries);
        assert_eq!(metadata_snapshot(&target), target_metadata);
        assert_eq!(metadata_snapshot(&alias), link_metadata);
        assert_eq!(metadata_snapshot(&sentinel), sentinel_metadata);
        assert_eq!(std::fs::read(&sentinel).unwrap(), sentinel_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn fs_open_existing_rejects_terminal_symlink_with_trailing_separator() {
        use std::os::unix::fs::symlink;

        let scratch = Scratch::new();
        let target = scratch.0.join("target");
        let alias = scratch.0.join("alias");
        make_private_directory(&target);
        let sentinel = target.join("sentinel");
        std::fs::write(&sentinel, b"trailing slash must not follow me").unwrap();
        symlink(&target, &alias).unwrap();
        let mut trailing_spelling = alias.clone().into_os_string();
        trailing_spelling.push("/");
        let trailing_spelling = PathBuf::from(trailing_spelling);

        let parent_entries = directory_entries(&scratch.0);
        let target_entries = directory_entries(&target);
        let target_metadata = metadata_snapshot(&target);
        let link_metadata = metadata_snapshot(&alias);
        let sentinel_metadata = metadata_snapshot(&sentinel);
        let sentinel_bytes = std::fs::read(&sentinel).unwrap();

        let error = expect_existing_root_refusal(&trailing_spelling);

        assert!(error.to_string().contains("normalized absolute spelling"));
        assert_eq!(directory_entries(&scratch.0), parent_entries);
        assert_eq!(directory_entries(&target), target_entries);
        assert_eq!(metadata_snapshot(&target), target_metadata);
        assert_eq!(metadata_snapshot(&alias), link_metadata);
        assert_eq!(metadata_snapshot(&sentinel), sentinel_metadata);
        assert_eq!(std::fs::read(&sentinel).unwrap(), sentinel_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn fs_open_existing_rejects_dot_segment_spellings_without_mutation() {
        let scratch = Scratch::new();
        let base = scratch.0.join("bodies");
        make_private_directory(&base);
        let sentinel = base.join("sentinel");
        std::fs::write(&sentinel, b"dot segments stay lexical").unwrap();
        let spellings = [base.join("."), base.join("..").join("bodies")];

        let parent_entries = directory_entries(&scratch.0);
        let base_entries = directory_entries(&base);
        let base_metadata = metadata_snapshot(&base);
        let sentinel_metadata = metadata_snapshot(&sentinel);
        let sentinel_bytes = std::fs::read(&sentinel).unwrap();

        for spelling in &spellings {
            let error = expect_existing_root_refusal(spelling);
            assert!(error.to_string().contains("normalized absolute spelling"));
        }

        assert_eq!(directory_entries(&scratch.0), parent_entries);
        assert_eq!(directory_entries(&base), base_entries);
        assert_eq!(metadata_snapshot(&base), base_metadata);
        assert_eq!(metadata_snapshot(&sentinel), sentinel_metadata);
        assert_eq!(std::fs::read(&sentinel).unwrap(), sentinel_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn fs_open_existing_wrong_mode_refusal_preserves_namespace_and_bytes() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new();
        let base = scratch.0.join("bodies");
        make_private_directory(&base);
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o750)).unwrap();
        let sentinel = base.join("sentinel");
        std::fs::write(&sentinel, b"still here").unwrap();

        let parent_entries = directory_entries(&scratch.0);
        let base_entries = directory_entries(&base);
        let base_metadata = metadata_snapshot(&base);
        let sentinel_metadata = metadata_snapshot(&sentinel);
        let sentinel_bytes = std::fs::read(&sentinel).unwrap();

        let error = expect_existing_root_refusal(&base);

        assert!(error.to_string().contains("mode 0750"));
        assert_eq!(directory_entries(&scratch.0), parent_entries);
        assert_eq!(directory_entries(&base), base_entries);
        assert_eq!(metadata_snapshot(&base), base_metadata);
        assert_eq!(metadata_snapshot(&sentinel), sentinel_metadata);
        assert_eq!(std::fs::read(&sentinel).unwrap(), sentinel_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn fs_open_existing_wrong_owner_policy_is_non_mutating() {
        let scratch = Scratch::new();
        let base = scratch.0.join("bodies");
        make_private_directory(&base);
        let sentinel = base.join("sentinel");
        std::fs::write(&sentinel, b"owner policy").unwrap();

        let parent_entries = directory_entries(&scratch.0);
        let base_entries = directory_entries(&base);
        let base_metadata = metadata_snapshot(&base);
        let sentinel_metadata = metadata_snapshot(&sentinel);
        let sentinel_bytes = std::fs::read(&sentinel).unwrap();

        // An unprivileged test cannot create a directory owned by a different
        // UID. Injecting a different expected owner exercises the exact helper
        // used by the public constructor without mutating fixture ownership.
        let wrong_owner = current_effective_uid().wrapping_add(1);
        let error = admit_existing_fs_root_for_owner(&base, wrong_owner).unwrap_err();

        assert!(error.to_string().contains("current-owner directory"));
        assert_eq!(directory_entries(&scratch.0), parent_entries);
        assert_eq!(directory_entries(&base), base_entries);
        assert_eq!(metadata_snapshot(&base), base_metadata);
        assert_eq!(metadata_snapshot(&sentinel), sentinel_metadata);
        assert_eq!(std::fs::read(&sentinel).unwrap(), sentinel_bytes);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fs_open_existing_valid_root_supports_reads_and_writes() {
        let scratch = Scratch::new();
        let base = scratch.0.join("bodies");
        std::fs::create_dir(&base).unwrap();
        set_private_dir(&base).unwrap();
        std::fs::write(base.join("existing"), b"already there").unwrap();

        let store = FsStore::open_existing(&base).unwrap();
        let existing = BodyRef::new("fs://existing").unwrap();
        assert_eq!(store.get(&existing).await.unwrap(), b"already there");

        let written = store.put(b"new body").await.unwrap();
        assert_eq!(store.get(&written).await.unwrap(), b"new body");
    }

    #[cfg(unix)]
    #[test]
    fn fs_open_existing_ignores_restrictive_umask() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        const CHILD_ENV: &str = "MNEME_BODY_EXISTING_UMASK_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            let scratch = Scratch::new();
            let base = scratch.0.join("bodies");
            make_private_directory(&base);
            let before = metadata_snapshot(&base);

            let store = FsStore::open_existing(&base).unwrap();

            assert_eq!(store.base, std::fs::canonicalize(&base).unwrap());
            assert_eq!(metadata_snapshot(&base), before);
            assert_eq!(
                std::fs::metadata(&base).unwrap().permissions().mode() & 0o7777,
                0o700
            );
            return;
        }

        let output = Command::new("/bin/sh")
            .args([
                "-c",
                "umask 0777; exec \"$1\" --exact tests::fs_open_existing_ignores_restrictive_umask",
                "mneme-body-umask",
            ])
            .arg(std::env::current_exe().unwrap())
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "restrictive-umask child failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child test did not execute: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn fs_open_existing_is_explicitly_unsupported_off_unix() {
        let absent = std::env::temp_dir().join(format!(
            "mneme-body-unsupported-existing-root-{}",
            Ulid::new()
        ));
        assert!(absent.is_absolute());

        let error = expect_existing_root_refusal(&absent);

        assert!(error.to_string().contains("unsupported"));
        assert!(error.to_string().contains("supported only on Unix"));
        assert!(!absent.exists());
    }

    async fn assert_range_contract(store: &dyn BodyStore, body: &BodyRef) {
        assert_eq!(
            store.get_range(body, 0, 0).await.unwrap(),
            BodyChunk {
                bytes: vec![],
                source_start: 0,
                source_end: 0,
                next_offset: Some(0),
            }
        );
        assert_eq!(
            store.get_range(body, 0, 3).await.unwrap(),
            BodyChunk {
                bytes: b"012".to_vec(),
                source_start: 0,
                source_end: 3,
                next_offset: Some(3),
            }
        );
        assert_eq!(
            store.get_range(body, 2, 3).await.unwrap(),
            BodyChunk {
                bytes: b"234".to_vec(),
                source_start: 2,
                source_end: 5,
                next_offset: Some(5),
            }
        );
        assert_eq!(
            store.get_range(body, 3, 3).await.unwrap(),
            BodyChunk {
                bytes: b"345".to_vec(),
                source_start: 3,
                source_end: 6,
                next_offset: None,
            }
        );
        assert_eq!(
            store.get_range(body, 6, 3).await.unwrap(),
            BodyChunk {
                bytes: vec![],
                source_start: 6,
                source_end: 6,
                next_offset: None,
            }
        );
        assert_eq!(
            store.get_range(body, 9, 3).await.unwrap(),
            BodyChunk {
                bytes: vec![],
                source_start: 9,
                source_end: 9,
                next_offset: None,
            }
        );
        assert_eq!(
            store.get_range(body, u64::MAX, 1).await.unwrap(),
            BodyChunk {
                bytes: vec![],
                source_start: u64::MAX,
                source_end: u64::MAX,
                next_offset: None,
            }
        );

        let first = store.get_range(body, 0, 2).await.unwrap();
        let second = store
            .get_range(body, first.next_offset.unwrap(), 2)
            .await
            .unwrap();
        let third = store
            .get_range(body, second.next_offset.unwrap(), 2)
            .await
            .unwrap();
        assert_eq!(first.bytes, b"01");
        assert_eq!(second.bytes, b"23");
        assert_eq!(third.bytes, b"45");
        assert_eq!(third.next_offset, None);

        assert_eq!(
            store.get_prefix(body, 0).await.unwrap(),
            (vec![], true),
            "the compatibility prefix delegates to the zero-offset range"
        );
        assert_eq!(
            store.get_prefix(body, 6).await.unwrap(),
            (b"012345".to_vec(), false)
        );
        assert!(matches!(
            store.get_range(body, 0, usize::MAX).await,
            Err(Error::InvalidInput(_))
        ));
    }

    struct CountingReader<R> {
        inner: R,
        bytes_read: usize,
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let read = self.inner.read(buf)?;
            self.bytes_read += read;
            Ok(read)
        }
    }

    impl<R: Seek> Seek for CountingReader<R> {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[tokio::test]
    async fn inline_round_trips() {
        let store = InlineStore::new();
        let body = store.put(b"hello world").await.unwrap();
        assert_eq!(body.scheme(), "inline");
        assert_eq!(store.get(&body).await.unwrap(), b"hello world");
    }

    #[tokio::test]
    async fn inline_missing_is_error() {
        let store = InlineStore::new();
        let missing = BodyRef::new("inline://nope").unwrap();
        assert!(store.get(&missing).await.is_err());
    }

    #[tokio::test]
    async fn inline_delete_is_idempotent() {
        let store = InlineStore::new();
        let body = store.put(b"erase me").await.unwrap();
        store.delete(&body).await.unwrap();
        store.delete(&body).await.unwrap();
        assert!(store.get(&body).await.is_err());
    }

    #[tokio::test]
    async fn inline_ranges_are_exact_and_prefix_delegates() {
        let store = InlineStore::new();
        let body = store.put(b"012345").await.unwrap();
        assert_range_contract(&store, &body).await;

        let binary = store.put(&[0xff, 0x00, 0x80, 0xfe]).await.unwrap();
        assert_eq!(
            store.get_range(&binary, 1, 2).await.unwrap().bytes,
            vec![0x00, 0x80]
        );
    }

    #[tokio::test]
    async fn fs_round_trips() {
        let dir = std::env::temp_dir().join(format!("mneme-body-test-{}", Ulid::new()));
        let store = FsStore::new(&dir).unwrap();
        let body = store.put(b"on disk").await.unwrap();
        assert_eq!(body.scheme(), "fs");
        assert_eq!(
            body.as_str().strip_prefix("fs://").unwrap().len(),
            26,
            "new refs expose only an opaque ULID, not an absolute host path"
        );
        assert_eq!(store.get(&body).await.unwrap(), b"on disk");
        store.delete(&body).await.unwrap();
        store.delete(&body).await.unwrap();
        assert!(store.get(&body).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fs_rejects_paths_outside_its_base() {
        let root = std::env::temp_dir().join(format!("mneme-body-test-{}", Ulid::new()));
        let base = root.join("bodies");
        std::fs::create_dir_all(&base).unwrap();
        let outside = root.join("secret");
        std::fs::write(&outside, b"nope").unwrap();
        let store = FsStore::new(&base).unwrap();
        let escaped = BodyRef::new(format!("fs://{}", outside.display())).unwrap();
        assert!(store.get(&escaped).await.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn fs_ranges_are_exact_and_prefix_delegates() {
        let dir = std::env::temp_dir().join(format!("mneme-body-test-{}", Ulid::new()));
        let store = FsStore::new(&dir).unwrap();
        let body = store.put(b"012345").await.unwrap();
        assert_range_contract(&store, &body).await;

        let binary = store.put(&[0xff, 0x00, 0x80, 0xfe]).await.unwrap();
        assert_eq!(
            store.get_range(&binary, 1, 2).await.unwrap().bytes,
            vec![0x00, 0x80]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn filesystem_range_reads_at_most_limit_plus_continuation_probe() {
        let dir = std::env::temp_dir().join(format!("mneme-body-test-{}", Ulid::new()));
        let store = FsStore::new(&dir).unwrap();
        let source: Vec<u8> = (0..4_096).map(|index| (index % 251) as u8).collect();
        let body = store.put(&source).await.unwrap();
        let file = std::fs::File::open(store.resolve(&body).unwrap()).unwrap();
        let mut counted = CountingReader {
            inner: file,
            bytes_read: 0,
        };

        let chunk = read_seek_range(&mut counted, 1_000, 7).unwrap();
        assert_eq!(chunk.bytes, source[1_000..1_007]);
        assert_eq!(chunk.next_offset, Some(1_007));
        assert_eq!(
            counted.bytes_read, 8,
            "Take must prevent the source from serving more than max_bytes + 1"
        );

        let file = std::fs::File::open(store.resolve(&body).unwrap()).unwrap();
        let mut zero = CountingReader {
            inner: file,
            bytes_read: 0,
        };
        let chunk = read_seek_range(&mut zero, 0, 0).unwrap();
        assert!(chunk.bytes.is_empty());
        assert_eq!(chunk.next_offset, Some(0));
        assert_eq!(zero.bytes_read, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn body_range_arithmetic_overflow_fails_closed() {
        assert!(matches!(
            checked_probe_limit(usize::MAX),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            finish_chunk(vec![0xff], u64::MAX, false),
            Err(Error::InvalidInput(_))
        ));
    }
}
