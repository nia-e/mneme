//! One canonical resolver for mneme's stable database path.
//!
//! A native bootstrap publishes an immutable generation below
//! `.mneme/generations/<operation-id>/` and selects it through `.mneme/current`.
//! Every frontend must resolve that indirection identically. Otherwise one
//! process can open the activated graph while another silently creates the old
//! `.mneme/memory.db` path and the project appears to forget everything.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::fs::{File, OpenOptions};

use ulid::Ulid;

mod fresh;

pub use fresh::{
    FreshStoreNameObservationV1, FreshStorePublicationNeedsRecoveryV1,
    FreshStorePublicationRecoveryPhaseV1, FreshStorePublishErrorV1,
    FreshStorePublishedGuardNeedsRecoveryV1, FreshStorePublishedResiduePhaseV1,
    FreshStorePublishedWithResidueV1, FreshStoreStageSpecV1, FreshStoreStageV1,
    PreparedFreshStoreStageV1, PublishedFreshStoreV1,
};

const DATABASE_FILE: &str = "memory.db";
const LEGACY_FILE: &str = "memory.json";

/// Exclusive process ownership of one resolved database path.
///
/// The lock inode deliberately remains beside the database after drop. Removing
/// it would let two contenders lock different inodes under the same pathname.
/// Publication code must acquire this lease too, before its last absence check,
/// so a normal frontend cannot create the conventional path during activation.
#[derive(Debug)]
pub struct StoreLease {
    #[cfg(unix)]
    file: File,
    #[cfg(unix)]
    parent: File,
    lock_path: PathBuf,
    #[cfg(unix)]
    parent_path: PathBuf,
    #[cfg(unix)]
    parent_identity: UnixIdentity,
    #[cfg(unix)]
    lock_identity: UnixIdentity,
}

impl StoreLease {
    pub fn acquire(database: &Path) -> io::Result<Self> {
        reject_multiply_linked_database(database)?;
        let path = store_lock_path(database)?;

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let parent_path = path
                .parent()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("mneme store lease {} has no parent", path.display()),
                    )
                })?
                .to_path_buf();
            let (parent, parent_identity) = open_safe_parent(&parent_path)?;
            let file = open_or_create_lease(&parent, &path)?;
            let lock_identity = validate_private_lease(&file.metadata()?, &path)?;

            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let source = io::Error::last_os_error();
                if source.raw_os_error() == Some(libc::EWOULDBLOCK)
                    || source.raw_os_error() == Some(libc::EAGAIN)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "database {} is already owned by another mneme process (lease {}); stop or release that process and retry ({source})",
                            database.display(),
                            path.display()
                        ),
                    ));
                }
                return Err(io::Error::new(
                    source.kind(),
                    format!(
                        "cannot acquire mneme store lease {} for database {}: {source}",
                        path.display(),
                        database.display()
                    ),
                ));
            }

            require_parent_identity(&parent_path, &parent, parent_identity)?;
            require_named_lease_identity(&path, &file, lock_identity)?;

            Ok(Self {
                file,
                parent,
                lock_path: path,
                parent_path,
                parent_identity,
                lock_identity,
            })
        }

        #[cfg(not(unix))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "exclusive mneme store leases are currently supported only on Unix",
            ))
        }
    }

    /// Prove this unforgeable lease guards `database`'s canonical adjacent lock.
    /// Mutation APIs use this as a capability check rather than trusting a
    /// comment that callers happened to acquire some lease.
    pub fn require_guards(&self, database: &Path) -> io::Result<()> {
        reject_multiply_linked_database(database)?;
        let expected = store_lock_path(database)?;
        if expected != self.lock_path {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "store lease {} does not guard database {} (expected lease {})",
                    self.lock_path.display(),
                    database.display(),
                    expected.display()
                ),
            ));
        }

        #[cfg(unix)]
        {
            require_parent_identity(&self.parent_path, &self.parent, self.parent_identity)?;
            require_named_lease_identity(&self.lock_path, &self.file, self.lock_identity)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UnixIdentity {
    device: u64,
    inode: u64,
    owner: u32,
    mode: u32,
    links: u64,
}

#[cfg(unix)]
impl UnixIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            owner: metadata.uid(),
            mode: metadata.mode(),
            links: metadata.nlink(),
        }
    }

    fn same_parent(self, other: Self) -> bool {
        self.device == other.device
            && self.inode == other.inode
            && self.owner == other.owner
            && self.mode == other.mode
    }
}

#[cfg(unix)]
fn open_safe_parent(path: &Path) -> io::Result<(File, UnixIdentity)> {
    use std::os::unix::fs::OpenOptionsExt;

    let lexical = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot inspect mneme store lease parent {}: {error}",
                path.display()
            ),
        )
    })?;
    let identity = validate_safe_parent(&lexical, path)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY);
    let parent = options.open(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot open mneme store lease parent {}: {error}",
                path.display()
            ),
        )
    })?;
    let opened = validate_safe_parent(&parent.metadata()?, path)?;
    if !opened.same_parent(identity) {
        return Err(namespace_changed(format!(
            "mneme store lease parent {} changed while it was opened",
            path.display()
        )));
    }
    Ok((parent, identity))
}

#[cfg(unix)]
fn validate_safe_parent(metadata: &fs::Metadata, path: &Path) -> io::Result<UnixIdentity> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let identity = UnixIdentity::from_metadata(metadata);
    let expected_owner = unsafe { libc::geteuid() };
    let mode = metadata.permissions().mode() & 0o7777;
    let private_current_owner = metadata.uid() == expected_owner && mode & 0o022 == 0;
    let root_owned_sticky_temp = metadata.uid() == 0 && mode == 0o1777;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || (!private_current_owner && !root_owned_sticky_temp)
    {
        return Err(invalid_data(format!(
            "mneme store lease parent {} must be either a current-owner directory not writable by group or other, or a root-owned sticky temporary directory with exact mode 1777 (owner {}, mode {:04o})",
            path.display(),
            metadata.uid(),
            mode
        )));
    }
    Ok(identity)
}

#[cfg(unix)]
fn validate_private_lease(metadata: &fs::Metadata, path: &Path) -> io::Result<UnixIdentity> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let identity = UnixIdentity::from_metadata(metadata);
    let expected_owner = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != expected_owner
        || metadata.permissions().mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(invalid_data(format!(
            "mneme store lease {} must be a current-owner regular file with exact mode 0600 and one link (owner {}, mode {:04o}, links {})",
            path.display(),
            metadata.uid(),
            metadata.permissions().mode() & 0o7777,
            metadata.nlink()
        )));
    }
    Ok(identity)
}

#[cfg(unix)]
fn open_or_create_lease(parent: &File, path: &Path) -> io::Result<File> {
    let filename = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mneme store lease {} has no filename", path.display()),
        )
    })?;

    match openat_file(
        parent,
        filename,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        0o600,
    ) {
        Ok(file) => {
            set_created_lease_mode(&file, path)?;
            validate_private_lease(&file.metadata()?, path)?;
            Ok(file)
        }
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
            open_existing_lease(parent, path, filename)
        }
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "cannot create mneme store lease {}: {error}",
                path.display()
            ),
        )),
    }
}

#[cfg(unix)]
fn sync_retained_lease(parent: &File, file: &File, path: &Path) -> io::Result<()> {
    fail_lease_sync_for_test()?;
    file.sync_all().map_err(|source| {
        io::Error::new(
            source.kind(),
            format!(
                "cannot sync retained mneme store lease {} before fresh intent binding: {source}",
                path.display()
            ),
        )
    })?;
    parent.sync_all().map_err(|source| {
        io::Error::new(
            source.kind(),
            format!(
                "cannot sync parent of retained mneme store lease {} before fresh intent binding: {source}",
                path.display()
            ),
        )
    })
}

#[cfg(all(unix, test))]
std::thread_local! {
    static FAIL_LEASE_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(all(unix, test))]
fn arm_lease_sync_failure_for_test() {
    FAIL_LEASE_SYNC.with(|armed| armed.set(true));
}

#[cfg(all(unix, test))]
fn fail_lease_sync_for_test() -> io::Result<()> {
    FAIL_LEASE_SYNC.with(|armed| {
        if armed.replace(false) {
            Err(io::Error::other(
                "injected retained lease durability failure",
            ))
        } else {
            Ok(())
        }
    })
}

#[cfg(all(unix, not(test)))]
fn fail_lease_sync_for_test() -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn open_existing_lease(parent: &File, path: &Path, filename: &std::ffi::OsStr) -> io::Result<File> {
    let lexical = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            namespace_changed(format!(
                "cannot inspect existing mneme store lease {} after create-new refusal: {error}",
                path.display()
            ))
        } else {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot inspect existing mneme store lease {} after create-new refusal: {error}",
                    path.display()
                ),
            )
        }
    })?;
    let expected = validate_private_lease(&lexical, path)?;
    let file = openat_file(
        parent,
        filename,
        libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        0,
    )
    .map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            namespace_changed(format!(
                "cannot open existing mneme store lease {}: {error}",
                path.display()
            ))
        } else {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot open existing mneme store lease {}: {error}",
                    path.display()
                ),
            )
        }
    })?;
    let opened = validate_private_lease(&file.metadata()?, path)?;
    if opened != expected {
        return Err(namespace_changed(format!(
            "mneme store lease {} changed while it was opened",
            path.display()
        )));
    }
    Ok(file)
}

#[cfg(unix)]
fn set_created_lease_mode(file: &File, path: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let rc = unsafe { libc::fchmod(file.as_raw_fd(), 0o600) };
    if rc == 0 {
        Ok(())
    } else {
        let source = io::Error::last_os_error();
        Err(io::Error::new(
            source.kind(),
            format!(
                "cannot set newly created mneme store lease {} to mode 0600: {source}",
                path.display()
            ),
        ))
    }
}

#[cfg(unix)]
fn openat_file(
    parent: &File,
    filename: &std::ffi::OsStr,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let filename = std::ffi::CString::new(filename.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "mneme store lease filename contains an interior NUL",
        )
    })?;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), filename.as_ptr(), flags, mode) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn require_parent_identity(path: &Path, parent: &File, expected: UnixIdentity) -> io::Result<()> {
    let opened = validate_safe_parent(&parent.metadata()?, path)?;
    let named = fs::symlink_metadata(path)
        .and_then(|metadata| validate_safe_parent(&metadata, path))
        .map_err(|error| {
            namespace_changed(format!(
                "mneme store lease parent {} is no longer its admitted directory: {error}",
                path.display()
            ))
        })?;
    if !opened.same_parent(expected) || !named.same_parent(expected) {
        return Err(namespace_changed(format!(
            "mneme store lease parent {} was replaced after admission (expected {expected:?}, opened {opened:?}, named {named:?})",
            path.display(),
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn require_named_lease_identity(
    path: &Path,
    file: &File,
    expected: UnixIdentity,
) -> io::Result<()> {
    let opened = validate_private_lease(&file.metadata()?, path)?;
    let named = fs::symlink_metadata(path)
        .and_then(|metadata| validate_private_lease(&metadata, path))
        .map_err(|error| {
            namespace_changed(format!(
                "mneme store lease {} is no longer its admitted inode: {error}",
                path.display()
            ))
        })?;
    if opened != expected || named != expected {
        return Err(namespace_changed(format!(
            "mneme store lease {} was replaced after admission",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn namespace_changed(message: impl Into<String>) -> io::Error {
    invalid_data(message)
}

/// Stable adjacent lock identity. Existing database aliases canonicalize to the
/// same inode path; a missing database inherits the canonical parent identity.
pub fn store_lock_path(database: &Path) -> io::Result<PathBuf> {
    let identity = match fs::canonicalize(database) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = database
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let parent = fs::canonicalize(parent)?;
            let filename = database.file_name().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("database path {} has no filename", database.display()),
                )
            })?;
            parent.join(filename)
        }
        Err(error) => return Err(error),
    };
    let filename = identity.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("database path {} has no filename", database.display()),
        )
    })?;
    let mut lock_filename = filename.to_os_string();
    lock_filename.push(".mneme.lock");
    Ok(identity.with_file_name(lock_filename))
}

#[cfg(unix)]
fn reject_multiply_linked_database(database: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = match fs::metadata(database) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.nlink() > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing multiply-linked database {} (link count {}); copy it to a distinct file instead",
                database.display(),
                metadata.nlink()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_multiply_linked_database(_database: &Path) -> io::Result<()> {
    Ok(())
}

/// Resolve the conventional database inside `directory`.
pub fn default_store_path(directory: &Path) -> io::Result<PathBuf> {
    resolve_configured_store_path(&directory.join(DATABASE_FILE))
}

/// Resolve a configured database path without ever creating a fallback beside
/// an activated generation.
///
/// A configured input may not nominate any path below `.mneme/generations`.
/// Native bootstrap owns those paths; ordinary frontends must configure the
/// conventional `.mneme/memory.db` and let `current` select one immutable
/// generation. This restriction applies to the input provenance only: a valid
/// conventional input may still resolve to a canonical generation path.
///
/// Only the conventional `memory.db` name participates in activation and
/// legacy fallback. Custom filenames remain literal. When `current` exists it
/// must be exactly a relative `generations/<ULID>` symlink selecting a real,
/// non-symlink generation and regular database file. The returned activated
/// path is canonicalized to the immutable generation, so a later activation
/// switch cannot retarget an already-running process.
pub fn resolve_configured_store_path(configured: &Path) -> io::Result<PathBuf> {
    reject_generation_local_configured_path(configured)?;
    if configured.file_name() != Some(DATABASE_FILE.as_ref()) {
        return match metadata_if_present(configured)? {
            Some(metadata) => canonical_existing_store(configured, &metadata, "custom database"),
            None => Ok(configured.to_path_buf()),
        };
    }
    let directory = configured
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // The conventional name is also the implicit project-discovery boundary.
    // Refuse a repository-controlled `.mneme` symlink (or other non-directory)
    // before inspecting children; otherwise an innocent default invocation can
    // create/open a database and body siblings outside the repository. Explicit
    // custom filenames remain literal above for operators who intentionally own
    // another path.
    require_real_directory_if_present(directory, "store directory")?;
    let legacy = directory.join(LEGACY_FILE);
    let current = directory.join("current");

    let configured_metadata = metadata_if_present(configured)?;
    let legacy_metadata = metadata_if_present(&legacy)?;
    let current_metadata = metadata_if_present(&current)?;

    if let Some(metadata) = configured_metadata {
        if current_metadata.is_some() {
            return Err(invalid_data(format!(
                "ambiguous mneme store: both {} and activation {} exist",
                configured.display(),
                current.display()
            )));
        }
        reject_unactivated_generation_catalog(directory)?;
        return canonical_existing_store(configured, &metadata, "database");
    }
    if let Some(metadata) = legacy_metadata {
        if current_metadata.is_some() {
            return Err(invalid_data(format!(
                "ambiguous mneme store: both legacy {} and activation {} exist",
                legacy.display(),
                current.display()
            )));
        }
        reject_unactivated_generation_catalog(directory)?;
        return canonical_existing_store(&legacy, &metadata, "legacy database");
    }

    let Some(current_metadata) = current_metadata else {
        // A native bootstrap publishes the generation before it publishes the
        // selector. If the process dies in that narrow window, silently opening
        // (and potentially creating) the conventional database would turn a
        // recoverable activation into two competing stores. Ordinary frontends
        // therefore fail closed; only `bootstrap-create` owns recovery of this
        // state while holding the conventional store lease.
        reject_unactivated_generation_catalog(directory)?;
        return Ok(configured.to_path_buf());
    };
    if !current_metadata.file_type().is_symlink() {
        return Err(invalid_data(format!(
            "mneme activation {} is not a symlink",
            current.display()
        )));
    }

    let target = fs::read_link(&current)?;
    let operation_id = activation_operation_id(&target).ok_or_else(|| {
        invalid_data(format!(
            "mneme activation {} must target relative generations/<ULID>, got {}",
            current.display(),
            target.display()
        ))
    })?;
    // Parsing is an authority check, not just cosmetic naming: it excludes
    // separators, dot components, and arbitrary repository-controlled paths.
    Ulid::from_string(operation_id).map_err(|_| {
        invalid_data(format!(
            "mneme activation {} has an invalid generation id {operation_id:?}",
            current.display()
        ))
    })?;

    let generations = directory.join("generations");
    require_real_directory(&generations, "generation catalog")?;
    let generation = generations.join(operation_id);
    require_real_directory(&generation, "activated generation")?;
    let database = generation.join(DATABASE_FILE);
    let database_metadata = fs::symlink_metadata(&database).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot inspect activated mneme database {}: {error}",
                database.display()
            ),
        )
    })?;
    if database_metadata.file_type().is_symlink() || !database_metadata.is_file() {
        return Err(invalid_data(format!(
            "activated mneme database {} is not a real regular file",
            database.display()
        )));
    }

    let canonical_generations = fs::canonicalize(&generations)?;
    let canonical_generation = fs::canonicalize(&generation)?;
    if canonical_generation.parent() != Some(canonical_generations.as_path()) {
        return Err(invalid_data(format!(
            "activated mneme generation {} escapes catalog {}",
            canonical_generation.display(),
            canonical_generations.display()
        )));
    }
    let canonical_database = fs::canonicalize(&database)?;
    if canonical_database.parent() != Some(canonical_generation.as_path())
        || canonical_database.file_name() != Some(DATABASE_FILE.as_ref())
    {
        return Err(invalid_data(format!(
            "activated mneme database {} escapes generation {}",
            canonical_database.display(),
            canonical_generation.display()
        )));
    }

    // Detect a concurrent selector swap across validation. Returning the
    // canonical generation makes later swaps harmless, but accepting a path
    // assembled from two different selector observations would be misleading.
    if fs::read_link(&current)? != target {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "mneme activation {} changed while resolving",
                current.display()
            ),
        ));
    }
    Ok(canonical_database)
}

fn reject_generation_local_configured_path(configured: &Path) -> io::Result<()> {
    let lexical = lexical_absolute_path(configured)?;
    if path_names_generation_catalog(&lexical) {
        return Err(generation_local_config_error(configured));
    }

    // Lexical inspection catches absent inner stages and explicit `..`
    // spellings. Canonicalizing the deepest existing ancestor also catches an
    // otherwise innocent-looking symlink alias into a real generation catalog.
    let physical = physical_path_from_existing_ancestor(&lexical)?;
    if path_names_generation_catalog(&physical) {
        return Err(generation_local_config_error(configured));
    }
    Ok(())
}

fn lexical_absolute_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                // `absolute` has a root. A parent at the root is the root, not
                // an unresolved component which could hide later provenance.
                let _ = normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn physical_path_from_existing_ancestor(path: &Path) -> io::Result<PathBuf> {
    let mut cursor = path;
    let mut missing_suffix = Vec::new();
    loop {
        match fs::symlink_metadata(cursor) {
            Ok(_) => {
                let mut physical = fs::canonicalize(cursor).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "cannot establish configured mneme store provenance at {}: {error}",
                            cursor.display()
                        ),
                    )
                })?;
                for component in missing_suffix.iter().rev() {
                    physical.push(component);
                }
                return Ok(physical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(name) = cursor.file_name() else {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "cannot find an existing ancestor for configured mneme store {}",
                            path.display()
                        ),
                    ));
                };
                missing_suffix.push(name.to_os_string());
                cursor = cursor.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "cannot find an existing ancestor for configured mneme store {}",
                            path.display()
                        ),
                    )
                })?;
            }
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "cannot inspect configured mneme store provenance at {}: {error}",
                        cursor.display()
                    ),
                ));
            }
        }
    }
}

fn path_names_generation_catalog(path: &Path) -> bool {
    let mut previous_was_mneme = false;
    for component in path.components() {
        let Component::Normal(part) = component else {
            previous_was_mneme = false;
            continue;
        };
        if previous_was_mneme && part == "generations" {
            return true;
        }
        previous_was_mneme = part == ".mneme";
    }
    false
}

fn generation_local_config_error(configured: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "configured mneme store {} is generation-local and bootstrap-owned; configure the conventional .mneme/memory.db (or omit --db) so current selects the generation",
            configured.display()
        ),
    )
}

fn activation_operation_id(target: &Path) -> Option<&str> {
    let mut components = target.components();
    match (components.next(), components.next(), components.next()) {
        (Some(Component::Normal(first)), Some(Component::Normal(second)), None)
            if first == "generations" =>
        {
            second.to_str()
        }
        _ => None,
    }
}

fn metadata_if_present(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn require_real_directory(path: &Path, label: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot inspect mneme {label} {}: {error}", path.display()),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_data(format!(
            "mneme {label} {} is not a real directory",
            path.display()
        )));
    }
    Ok(())
}

fn require_real_directory_if_present(path: &Path, label: &str) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => Ok(()),
        Ok(_) => Err(invalid_data(format!(
            "mneme {label} {} is not a real directory",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("cannot inspect mneme {label} {}: {error}", path.display()),
        )),
    }
}

fn reject_unactivated_generation_catalog(directory: &Path) -> io::Result<()> {
    let generations = directory.join("generations");
    match fs::symlink_metadata(&generations) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(invalid_data(format!(
                    "mneme generation catalog {} exists without an activation and is not a real directory",
                    generations.display()
                )));
            }
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "mneme generation catalog {} exists without an activation; run the exact bootstrap-create recovery instead of opening a fallback store",
                    generations.display()
                ),
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn canonical_existing_store(
    path: &Path,
    lexical_metadata: &fs::Metadata,
    label: &str,
) -> io::Result<PathBuf> {
    if !lexical_metadata.is_file() && !lexical_metadata.file_type().is_symlink() {
        return Err(invalid_data(format!(
            "mneme {label} {} is not a regular file",
            path.display()
        )));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot resolve mneme {label} {}: {error}", path.display()),
        )
    })?;
    if !fs::metadata(&canonical)?.is_file() {
        return Err(invalid_data(format!(
            "resolved mneme {label} {} is not a regular file",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("mneme-store-path-{}", Ulid::new()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn absent_store_uses_conventional_path() {
        let scratch = Scratch::new();
        assert_eq!(
            default_store_path(&scratch.0).unwrap(),
            scratch.0.join(DATABASE_FILE)
        );
    }

    #[test]
    fn literal_custom_filename_is_not_reinterpreted() {
        let scratch = Scratch::new();
        let custom = scratch.0.join("custom.sqlite");
        assert_eq!(resolve_configured_store_path(&custom).unwrap(), custom);
    }

    #[test]
    fn existing_custom_filename_is_canonicalized_without_activation() {
        let scratch = Scratch::new();
        let custom = scratch.0.join("custom.sqlite");
        fs::write(&custom, b"fixture").unwrap();
        assert_eq!(
            resolve_configured_store_path(&custom).unwrap(),
            fs::canonicalize(&custom).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn conventional_store_refuses_a_symlinked_parent_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let scratch = Scratch::new();
        let project = scratch.0.join("project");
        let outside = scratch.0.join("outside");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, project.join(".mneme")).unwrap();

        let configured = project.join(".mneme").join(DATABASE_FILE);
        let error = resolve_configured_store_path(&configured).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!outside.join(DATABASE_FILE).exists());
        assert!(!outside.join(LEGACY_FILE).exists());
    }

    #[test]
    fn legacy_snapshot_is_reused_only_without_activation() {
        let scratch = Scratch::new();
        let legacy = scratch.0.join(LEGACY_FILE);
        fs::write(&legacy, b"{}").unwrap();
        assert_eq!(
            default_store_path(&scratch.0).unwrap(),
            fs::canonicalize(legacy).unwrap()
        );
    }

    #[cfg(unix)]
    fn activated(scratch: &Scratch) -> (PathBuf, PathBuf) {
        activated_at(&scratch.0)
    }

    #[cfg(unix)]
    fn activated_at(directory: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::symlink;

        let operation = Ulid::new().to_string();
        let generation = directory.join("generations").join(&operation);
        fs::create_dir_all(&generation).unwrap();
        let database = generation.join(DATABASE_FILE);
        fs::write(&database, b"db").unwrap();
        symlink(
            Path::new("generations").join(operation),
            directory.join("current"),
        )
        .unwrap();
        (generation, database)
    }

    fn assert_generation_local_config_rejected(configured: &Path) {
        let mut lock_name = configured.file_name().unwrap().to_os_string();
        lock_name.push(".mneme.lock");
        let lock = configured.with_file_name(lock_name);
        assert!(!lock.exists());
        let error = resolve_configured_store_path(configured).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("generation-local"));
        assert!(error.to_string().contains("current selects"));
        assert!(!lock.exists(), "provenance refusal created a store lease");
    }

    #[cfg(unix)]
    #[test]
    fn activation_resolves_to_canonical_immutable_generation() {
        let scratch = Scratch::new();
        let (_, database) = activated(&scratch);
        assert_eq!(
            default_store_path(&scratch.0).unwrap(),
            fs::canonicalize(database).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn conventional_named_catalog_may_resolve_to_but_not_configure_a_generation() {
        let scratch = Scratch::new();
        let catalog = scratch.0.join("project").join(".mneme");
        fs::create_dir_all(&catalog).unwrap();
        let (generation, database) = activated_at(&catalog);

        assert_eq!(
            resolve_configured_store_path(&catalog.join(DATABASE_FILE)).unwrap(),
            fs::canonicalize(&database).unwrap()
        );
        assert_generation_local_config_rejected(&database);
        assert_generation_local_config_rejected(&generation.join("other.sqlite"));
    }

    #[test]
    fn absent_private_generation_and_dotdot_spellings_are_rejected_without_residue() {
        let scratch = Scratch::new();
        let catalog = scratch.0.join("project").join(".mneme");
        let stage = catalog
            .join("generations")
            .join(format!(".bootstrap-{}-private", Ulid::new()));
        fs::create_dir_all(&stage).unwrap();
        let database = stage.join(DATABASE_FILE);
        assert_generation_local_config_rejected(&database);
        assert!(!database.exists());

        let disguised = catalog
            .join("unrelated")
            .join("..")
            .join("generations")
            .join(Ulid::new().to_string())
            .join(DATABASE_FILE);
        assert_generation_local_config_rejected(&disguised);
        assert!(!catalog.join("unrelated").exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_and_directory_aliases_into_generation_catalog_are_rejected() {
        use std::os::unix::fs::symlink;

        let scratch = Scratch::new();
        let catalog = scratch.0.join("project").join(".mneme");
        fs::create_dir_all(&catalog).unwrap();
        let (generation, database) = activated_at(&catalog);
        let file_alias = scratch.0.join("innocent-looking-db");
        symlink(&database, &file_alias).unwrap();
        assert_generation_local_config_rejected(&file_alias);

        let directory_alias = scratch.0.join("innocent-looking-directory");
        symlink(&generation, &directory_alias).unwrap();
        assert_generation_local_config_rejected(&directory_alias.join("new.sqlite"));
    }

    #[cfg(unix)]
    #[test]
    fn old_and_activated_paths_cannot_split_brain() {
        let scratch = Scratch::new();
        activated(&scratch);
        fs::write(scratch.0.join(DATABASE_FILE), b"old").unwrap();
        let error = default_store_path(&scratch.0).unwrap_err().to_string();
        assert!(error.contains("ambiguous mneme store"));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_and_activated_paths_cannot_split_brain() {
        let scratch = Scratch::new();
        activated(&scratch);
        fs::write(scratch.0.join(LEGACY_FILE), b"old").unwrap();
        let error = default_store_path(&scratch.0).unwrap_err().to_string();
        assert!(error.contains("ambiguous mneme store"));
    }

    #[cfg(unix)]
    #[test]
    fn malformed_or_dangling_activation_fails_closed() {
        use std::os::unix::fs::symlink;

        let malformed = Scratch::new();
        symlink("../elsewhere", malformed.0.join("current")).unwrap();
        assert!(default_store_path(&malformed.0).is_err());

        let dangling = Scratch::new();
        symlink(
            Path::new("generations").join(Ulid::new().to_string()),
            dangling.0.join("current"),
        )
        .unwrap();
        assert!(default_store_path(&dangling.0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_conventional_database_symlink_fails_closed() {
        use std::os::unix::fs::symlink;

        let scratch = Scratch::new();
        symlink("missing-target", scratch.0.join(DATABASE_FILE)).unwrap();
        assert!(default_store_path(&scratch.0).is_err());
        assert!(!scratch.0.join("missing-target").exists());
    }

    #[test]
    fn non_symlink_activation_fails_closed() {
        let scratch = Scratch::new();
        fs::create_dir(scratch.0.join("current")).unwrap();
        assert!(default_store_path(&scratch.0).is_err());
    }

    #[test]
    fn unactivated_generation_catalog_fails_closed() {
        let scratch = Scratch::new();
        fs::create_dir(scratch.0.join("generations")).unwrap();

        let error = default_store_path(&scratch.0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("without an activation"));
        assert!(!scratch.0.join(DATABASE_FILE).exists());
    }

    #[test]
    fn conventional_store_cannot_mask_an_unactivated_generation_catalog() {
        let scratch = Scratch::new();
        fs::write(scratch.0.join(DATABASE_FILE), b"competing store").unwrap();
        fs::create_dir(scratch.0.join("generations")).unwrap();

        let error = default_store_path(&scratch.0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("without an activation"));
    }

    #[cfg(unix)]
    #[test]
    fn lease_is_exclusive_across_symlink_aliases_and_released_on_drop() {
        use std::os::unix::fs::symlink;

        let scratch = Scratch::new();
        let database = scratch.0.join(DATABASE_FILE);
        let alias = scratch.0.join("alias.db");
        fs::write(&database, b"database identity").unwrap();
        symlink(&database, &alias).unwrap();
        assert_eq!(
            store_lock_path(&database).unwrap(),
            store_lock_path(&alias).unwrap()
        );

        let first = StoreLease::acquire(&database).unwrap();
        first.require_guards(&database).unwrap();
        first.require_guards(&alias).unwrap();
        assert_eq!(
            first
                .require_guards(&scratch.0.join("other.db"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let error = StoreLease::acquire(&alias).unwrap_err().to_string();
        assert!(error.contains("already owned"));
        drop(first);
        StoreLease::acquire(&alias).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lease_rejects_hard_link_database_aliases() {
        let scratch = Scratch::new();
        let database = scratch.0.join(DATABASE_FILE);
        let alias = scratch.0.join("alias.db");
        fs::write(&database, b"database identity").unwrap();
        fs::hard_link(&database, &alias).unwrap();

        let error = StoreLease::acquire(&alias).unwrap_err().to_string();
        assert!(error.contains("multiply-linked") && error.contains("link count 2"));
    }

    #[cfg(unix)]
    #[test]
    fn lease_rejects_a_symlink_lock_without_touching_its_victim() {
        use std::os::unix::fs::symlink;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let scratch = Scratch::new();
        let database = scratch.0.join(DATABASE_FILE);
        let victim = scratch.0.join("victim");
        fs::write(&victim, b"do not chmod or lock me").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        let victim_before = fs::metadata(&victim).unwrap();
        let lock = store_lock_path(&database).unwrap();
        symlink(&victim, &lock).unwrap();

        assert!(StoreLease::acquire(&database).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"do not chmod or lock me");
        let victim_after = fs::metadata(&victim).unwrap();
        assert_eq!(victim_after.ino(), victim_before.ino());
        assert_eq!(victim_after.permissions().mode() & 0o7777, 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn lease_rejects_a_public_existing_lock_without_repairing_it() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let scratch = Scratch::new();
        let database = scratch.0.join(DATABASE_FILE);
        let lock = store_lock_path(&database).unwrap();
        fs::write(&lock, b"operator-owned evidence").unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
        let before = fs::metadata(&lock).unwrap();

        let error = StoreLease::acquire(&database).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exact mode 0600"));
        let after = fs::metadata(&lock).unwrap();
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.permissions().mode() & 0o7777, 0o644);
        assert_eq!(fs::read(&lock).unwrap(), b"operator-owned evidence");
    }

    #[cfg(unix)]
    #[test]
    fn lease_rejects_a_multiply_linked_existing_lock() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let scratch = Scratch::new();
        let database = scratch.0.join(DATABASE_FILE);
        let lock = store_lock_path(&database).unwrap();
        let alias = scratch.0.join("lock-alias");
        fs::write(&lock, b"").unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&lock, &alias).unwrap();

        let error = StoreLease::acquire(&database).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("one link"));
        assert_eq!(fs::metadata(&lock).unwrap().nlink(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn lease_rejects_a_non_file_existing_lock() {
        let scratch = Scratch::new();
        let database = scratch.0.join(DATABASE_FILE);
        let lock = store_lock_path(&database).unwrap();
        fs::create_dir(&lock).unwrap();

        let error = StoreLease::acquire(&database).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("regular file"));
        assert!(lock.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn lease_rejects_a_fifo_existing_lock() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::FileTypeExt;

        let scratch = Scratch::new();
        let database = scratch.0.join(DATABASE_FILE);
        let lock = store_lock_path(&database).unwrap();
        let lock_bytes = std::ffi::CString::new(lock.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(lock_bytes.as_ptr(), 0o600) }, 0);

        let error = StoreLease::acquire(&database).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("regular file"));
        assert!(fs::symlink_metadata(&lock).unwrap().file_type().is_fifo());
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_lease_has_exact_private_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let scratch = Scratch::new();
        let database = scratch.0.join(DATABASE_FILE);
        let lock = store_lock_path(&database).unwrap();
        let lease = StoreLease::acquire(&database).unwrap();
        let metadata = fs::symlink_metadata(&lock).unwrap();

        assert!(metadata.is_file());
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        assert_eq!(metadata.nlink(), 1);
        lease.require_guards(&database).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restrictive_umask_cannot_poison_a_new_lease() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        const CHILD_ENV: &str = "MNEME_STORE_PATH_RESTRICTIVE_UMASK_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            let scratch = Scratch::new();
            let database = scratch.0.join(DATABASE_FILE);
            let lock = store_lock_path(&database).unwrap();
            let old_umask = unsafe { libc::umask(0o777) };

            let lease = StoreLease::acquire(&database).unwrap();
            assert_eq!(
                fs::metadata(&lock).unwrap().permissions().mode() & 0o7777,
                0o600
            );
            drop(lease);
            StoreLease::acquire(&database).unwrap();
            unsafe { libc::umask(old_umask) };
            return;
        }

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::restrictive_umask_cannot_poison_a_new_lease",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "restrictive-umask child failed\nstdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("1 passed"),
            "child test did not execute: {stdout}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_owned_sticky_system_temp_is_an_admitted_parent() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = fs::canonicalize("/private/tmp")
            .or_else(|_| fs::canonicalize("/tmp"))
            .unwrap();
        let metadata = fs::metadata(&temp).unwrap();
        if metadata.uid() != 0 || metadata.permissions().mode() & 0o7777 != 0o1777 {
            return;
        }

        let database = temp.join(format!("mneme-store-path-{}.db", Ulid::new()));
        let lock = store_lock_path(&database).unwrap();
        let lease = StoreLease::acquire(&database).unwrap();
        lease.require_guards(&database).unwrap();
        drop(lease);
        fs::remove_file(lock).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn contention_is_actionable_and_reported_as_would_block() {
        let scratch = Scratch::new();
        let database = scratch.0.join(DATABASE_FILE);
        let _first = StoreLease::acquire(&database).unwrap();

        let error = StoreLease::acquire(&database).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("already owned"));
        assert!(error.to_string().contains("stop or release"));
    }

    #[cfg(unix)]
    #[test]
    fn require_guards_rejects_a_replaced_lease_inode() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new();
        let database = scratch.0.join(DATABASE_FILE);
        let lock = store_lock_path(&database).unwrap();
        let displaced = scratch.0.join("displaced-lock");
        let lease = StoreLease::acquire(&database).unwrap();
        fs::rename(&lock, &displaced).unwrap();
        fs::write(&lock, b"").unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();

        let error = lease.require_guards(&database).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("replaced after admission"));
    }

    #[cfg(unix)]
    #[test]
    fn lease_refuses_an_unsafe_parent_before_creating_a_lock() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new();
        let unsafe_parent = scratch.0.join("unsafe");
        fs::create_dir(&unsafe_parent).unwrap();
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777)).unwrap();
        let database = unsafe_parent.join(DATABASE_FILE);
        let lock = store_lock_path(&database).unwrap();

        let error = StoreLease::acquire(&database).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("current-owner directory"));
        assert!(!lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn require_guards_rejects_a_replaced_parent_directory() {
        let scratch = Scratch::new();
        let parent = scratch.0.join("store");
        let displaced = scratch.0.join("displaced-store");
        fs::create_dir(&parent).unwrap();
        let database = parent.join(DATABASE_FILE);
        let lease = StoreLease::acquire(&database).unwrap();
        fs::rename(&parent, &displaced).unwrap();
        fs::create_dir(&parent).unwrap();

        let error = lease.require_guards(&database).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("parent"));
    }

    #[cfg(unix)]
    #[test]
    fn lease_continues_to_exclude_after_its_parent_is_renamed() {
        let scratch = Scratch::new();
        let old_parent = scratch.0.join("staged");
        let new_parent = scratch.0.join("published");
        fs::create_dir(&old_parent).unwrap();
        let old_database = old_parent.join(DATABASE_FILE);
        let new_database = new_parent.join(DATABASE_FILE);

        let old_lease = StoreLease::acquire(&old_database).unwrap();
        fs::rename(&old_parent, &new_parent).unwrap();

        let error = StoreLease::acquire(&new_database).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("already owned"));

        assert!(old_lease.require_guards(&old_database).is_err());

        drop(old_lease);
        StoreLease::acquire(&new_database).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lease_identity_preserves_non_utf8_path_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let scratch = Scratch::new();
        let filename = std::ffi::OsString::from_vec(b"memory-\xff.db".to_vec());
        let database = scratch.0.join(filename);
        let lock = store_lock_path(&database).unwrap();

        assert!(lock.as_os_str().as_bytes().ends_with(b"\xff.db.mneme.lock"));
    }

    #[cfg(unix)]
    #[test]
    fn distinct_database_extensions_have_distinct_leases() {
        let scratch = Scratch::new();
        let database = scratch.0.join(DATABASE_FILE);
        let sqlite = scratch.0.join("memory.sqlite");
        assert_ne!(
            store_lock_path(&database).unwrap(),
            store_lock_path(&sqlite).unwrap()
        );
        let first = StoreLease::acquire(&database).unwrap();
        let second = StoreLease::acquire(&sqlite).unwrap();
        drop((first, second));
    }
}
