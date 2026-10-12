//! Windows-local SQLite identity and custody boundary.
//!
//! The NTFS/ACL/no-reparse primitives belong to mneme-store-path. This module
//! composes those safe operations with SQLite's independent path-opened handle;
//! it does not pretend that the two handles are atomically bound or prevent
//! same-authority rename ABA. Windows evidence has its own domain and policy.

use super::*;
use mneme_store_path::windows::{
    Directory, FileRevision, file_identity, file_revision, open_database_sidecar_checked,
    open_regular_checked, require_private_file,
};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::FileExt;

pub(super) type FrozenMainFileIdentity = FileRevision;

pub(super) const MANAGED_CLOSED_SOURCE_IDENTITY_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-closed-source-identity.windows-ntfs.transcript.v1\0";
pub(super) const MANAGED_CLOSED_SOURCE_POLICY_DESCRIPTION_V1: &str = concat!(
    "mnestic.managed-sqlite-closed-source-policy.windows-ntfs.v1\n",
    "authority=owned historical evidence of one managed SQLite observation and pathname/handle/residual bracketing checks in one consuming operation; not an attestation or main-file seal\n",
    "identity=SHA256 of the exact Windows-NTFS v1 domain, entry marker 01, u64be frame-count 6, six u64be name/value length-framed fields in fixed order source-policy-fingerprint,managed-snapshot-policy,supplied-absolute-windows-path-utf16be,main-identity,wal-residual,shm-residual, then ff and u64be transcript-count 1\n",
    "snapshot-policy=explicit mnestic.managed-snapshot-policy identity bytes; never an enum discriminant or native representation\n",
    "path=caller-supplied absolute drive-path OsStr UTF-16 code units exactly in big-endian order; no canonicalization, lossy UTF-8, URI or native-endian encoding; UNC, device, alternate stream and reparse traversal refused\n",
    "construction=Windows local NTFS only; main requires current-owner ACL without foreign write grants; residuals require private DACL and current-user or builtin-Administrators owner for elevated SQLite default-owner creation; regular non-reparse single-link files, read-only held main handle; every named reopen repeats checks\n",
    "freeze=full 64-bit volume serial and 128-bit file ID, link count, length, creation/write/change times and attributes compared across held/named/held/named handles; SQLite's independently opened handle is not atomically bound\n",
    "preflight=bounded 108-byte SQLite header, single schema leaf page, at most 64 schema cells; residual WAL at most zero bytes and SHM at most 67108864 bytes, SHA256 through fixed 65536-byte buffer with before/after held/named revision checks; journal must be absent\n",
    "verification=residual equality also includes readonly and modified SystemTime projections; these redundant projections are omitted from public evidence and fingerprint bytes\n",
    "completion=tracked transaction state is compared with live sqlite3_get_autocommit; either possible-active view triggers rollback; live autocommit afterward; strict sqlite3_close success required; close_v2 fallback is failure\n",
    "bracketing=main-before,residual,and-main-after checks independently attempted and accumulated after close; operation, lifecycle, close or source failure withholds evidence\n",
    "main=volume u64be; file-id exact 16 bytes; link-count u32be; length u64be; creation/write/change times signed i64be Windows 100ns ticks since 1601; attributes u32be\n",
    "residual=absent 00; present 01 then exact main metadata encoding and exact SHA256; WAL and SHM separately named frames\n",
    "nonclaims=no authentication,freshness,anti-rollback,approval,mutation authority,managed ownership,same-authority rename-ABA protection,atomic SQLite-handle binding or main content digest; pathname may change immediately after return\n",
);

/// Historical full NTFS main identity. No Unix inode, UID or mode is fabricated.
pub struct ManagedSqliteClosedMainFileIdentityV1 {
    revision: FileRevision,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteClosedMainFileIdentityV1 {
    pub const fn volume_serial_number(&self) -> u64 {
        self.revision.identity.volume
    }
    pub const fn file_id(&self) -> &[u8; 16] {
        &self.revision.identity.id
    }
    pub const fn link_count(&self) -> u64 {
        self.revision.identity.links as u64
    }
    pub const fn length(&self) -> u64 {
        self.revision.identity.len
    }
    /// Windows FILE_BASIC_INFO ticks (100ns since 1601), not Unix time.
    pub const fn creation_time(&self) -> i64 {
        self.revision.creation_time
    }
    pub const fn write_time(&self) -> i64 {
        self.revision.write_time
    }
    pub const fn change_time(&self) -> i64 {
        self.revision.change_time
    }
    pub const fn attributes(&self) -> u32 {
        self.revision.attributes
    }
}

/// Historical NTFS metadata for one bounded, content-fingerprinted residual.
pub struct ManagedSqliteClosedResidualMetadataV1 {
    revision: FileRevision,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteClosedResidualMetadataV1 {
    pub const fn volume_serial_number(&self) -> u64 {
        self.revision.identity.volume
    }
    pub const fn file_id(&self) -> &[u8; 16] {
        &self.revision.identity.id
    }
    pub const fn link_count(&self) -> u64 {
        self.revision.identity.links as u64
    }
    pub const fn length(&self) -> u64 {
        self.revision.identity.len
    }
    /// Windows FILE_BASIC_INFO ticks (100ns since 1601), not Unix time.
    pub const fn creation_time(&self) -> i64 {
        self.revision.creation_time
    }
    pub const fn write_time(&self) -> i64 {
        self.revision.write_time
    }
    pub const fn change_time(&self) -> i64 {
        self.revision.change_time
    }
    pub const fn attributes(&self) -> u32 {
        self.revision.attributes
    }
}

fn revision(file: &File, path: &Path, role: &str) -> Result<FileRevision> {
    file_revision(file).map_err(|error| miette!("cannot inspect {role} handle {:?}: {error}", path))
}

fn named_revision(path: &Path, role: &str) -> Result<FileRevision> {
    let file = open_regular_checked(path, false)
        .map_err(|error| miette!("cannot open {role} no-follow {:?}: {error}", path))?;
    revision(&file, path, role)
}

pub(super) fn verify_frozen_file(
    file: &File,
    expected: FileRevision,
    path: &Path,
    role: &str,
) -> Result<()> {
    let before = revision(file, path, role)?;
    let named_before = named_revision(path, role)?;
    let after = revision(file, path, role)?;
    let named_after = named_revision(path, role)?;
    if [before, named_before, after, named_after]
        .iter()
        .any(|value| *value != expected)
    {
        bail!(
            "{role} NTFS identity/revision changed or its path names a different file: {:?}",
            path
        );
    }
    Ok(())
}

pub(super) fn open_source(path: &Path) -> Result<FrozenSourceMainFile> {
    let file = open_regular_checked(path, false).map_err(|error| {
        miette!(
            "cannot open snapshot source read-only and no-follow {:?}: {error}",
            path
        )
    })?;
    let identity = revision(&file, path, "snapshot source")?;
    let frozen = FrozenSourceMainFile { file, identity };
    frozen.verify(path, "snapshot source after binding its handle")?;
    frozen.validate_bounded_schema_root(path)?;
    frozen.verify(path, "snapshot source after bounded schema-root preflight")?;
    Ok(frozen)
}

pub(super) fn read_exact_at(
    file: &File,
    mut bytes: &mut [u8],
    mut offset: u64,
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match file.seek_read(bytes, offset) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => {
                offset = offset
                    .checked_add(count as u64)
                    .ok_or_else(|| std::io::Error::other("SQLite header read offset overflow"))?;
                bytes = &mut bytes[count..];
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn durable(file: &File, path: &Path, role: &str, maximum: u64) -> Result<DurableFileMetadata> {
    let revision = revision(file, path, role)?;
    if revision.identity.len > maximum {
        bail!(
            "{role} {:?} exceeds the {maximum}-byte snapshot residual limit",
            path
        );
    }
    let metadata = file.metadata().into_diagnostic()?;
    Ok(DurableFileMetadata {
        revision,
        len: revision.identity.len,
        readonly: metadata.permissions().readonly(),
        modified: metadata.modified().ok(),
    })
}

fn named_durable(path: &Path, role: &str, maximum: u64) -> Result<DurableFileMetadata> {
    let file = open_database_sidecar_checked(path)
        .map_err(|error| miette!("cannot open {role} no-follow {:?}: {error}", path))?;
    durable(&file, path, role, maximum)
}

impl FrozenResidualFile {
    pub(super) fn capture(path: &Path, maximum: u64, role: &str) -> Result<Self> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::Absent),
            Err(error) => bail!("cannot inspect {role} {:?}: {error}", path),
            Ok(_) => {}
        }
        let named_before = named_durable(path, role, maximum)?;
        let mut file = open_database_sidecar_checked(path).into_diagnostic()?;
        let opened = durable(&file, path, role, maximum)?;
        if named_before != opened {
            bail!("{role} changed while binding its handle");
        }
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let count = file.read(&mut buffer).into_diagnostic()?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .ok_or_else(|| miette!("residual byte count overflow"))?;
            if total > maximum {
                bail!("{role} exceeds the {maximum}-byte snapshot residual limit");
            }
            hash.update(&buffer[..count]);
        }
        let after = durable(&file, path, role, maximum)?;
        let named_after = named_durable(path, role, maximum)?;
        if total != opened.len || after != opened || named_after != opened {
            bail!("{role} changed while it was being fingerprinted");
        }
        Ok(Self::Present {
            metadata: opened,
            sha256: hash.finalize().into(),
        })
    }
}

pub(super) fn closed_main_identity_matches_frozen(
    expected: &ManagedSqliteClosedMainFileIdentityV1,
    observed: &FrozenSourceMainFile,
) -> bool {
    expected.revision == observed.identity
}

pub(super) fn closed_residual_matches_frozen(
    expected: &ManagedSqliteClosedResidualV1,
    observed: &FrozenResidualFile,
) -> bool {
    match (
        expected.present,
        expected.metadata.as_ref(),
        expected.sha256,
        observed,
    ) {
        (false, None, None, FrozenResidualFile::Absent) => true,
        (true, Some(expected), Some(hash), FrozenResidualFile::Present { metadata, sha256 }) => {
            expected.revision == metadata.revision && hash == *sha256
        }
        _ => false,
    }
}

pub(super) fn closed_source(
    supplied_path: PathBuf,
    managed_snapshot_policy: ManagedSnapshotPolicy,
    main_file: FrozenSourceMainFile,
    residuals: FrozenSourceResiduals,
) -> Result<ManagedSqliteClosedSourceEvidenceV1> {
    ensure_absolute_path(&supplied_path, "managed SQLite closed source evidence")?;
    let main_identity = ManagedSqliteClosedMainFileIdentityV1 {
        revision: main_file.identity,
        _thread_bound: PhantomData,
    };
    let wal_residual = closed_residual(residuals.wal);
    let shm_residual = closed_residual(residuals.shm);
    let identity_fingerprint = source_identity_fingerprint(
        &supplied_path,
        managed_snapshot_policy,
        &main_identity,
        &wal_residual,
        &shm_residual,
    )?;
    // The source descriptor is deliberately absent from the returned evidence.
    drop(main_file);
    Ok(ManagedSqliteClosedSourceEvidenceV1 {
        supplied_path,
        managed_snapshot_policy,
        main_identity,
        wal_residual,
        shm_residual,
        policy_fingerprint: *managed_snapshot_policy.source_fingerprint(),
        identity_fingerprint,
        _thread_bound: PhantomData,
    })
}

fn closed_residual(value: FrozenResidualFile) -> ManagedSqliteClosedResidualV1 {
    match value {
        FrozenResidualFile::Absent => ManagedSqliteClosedResidualV1 {
            present: false,
            metadata: None,
            sha256: None,
            _thread_bound: PhantomData,
        },
        FrozenResidualFile::Present { metadata, sha256 } => ManagedSqliteClosedResidualV1 {
            present: true,
            metadata: Some(ManagedSqliteClosedResidualMetadataV1 {
                revision: metadata.revision,
                _thread_bound: PhantomData,
            }),
            sha256: Some(sha256),
            _thread_bound: PhantomData,
        },
    }
}

fn revision_bytes(value: FileRevision) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(&value.identity.volume.to_be_bytes());
    bytes.extend_from_slice(&value.identity.id);
    bytes.extend_from_slice(&value.identity.links.to_be_bytes());
    bytes.extend_from_slice(&value.identity.len.to_be_bytes());
    bytes.extend_from_slice(&value.creation_time.to_be_bytes());
    bytes.extend_from_slice(&value.write_time.to_be_bytes());
    bytes.extend_from_slice(&value.change_time.to_be_bytes());
    bytes.extend_from_slice(&value.attributes.to_be_bytes());
    bytes
}

fn residual_bytes(value: &ManagedSqliteClosedResidualV1) -> Result<Vec<u8>> {
    match (value.present, value.metadata.as_ref(), value.sha256) {
        (false, None, None) => Ok(vec![0]),
        (true, Some(metadata), Some(hash)) => {
            let mut bytes = vec![1];
            bytes.extend_from_slice(&revision_bytes(metadata.revision));
            bytes.extend_from_slice(&hash);
            Ok(bytes)
        }
        _ => bail!("inconsistent closed residual evidence"),
    }
}

fn source_identity_fingerprint(
    path: &Path,
    policy: ManagedSnapshotPolicy,
    main: &ManagedSqliteClosedMainFileIdentityV1,
    wal: &ManagedSqliteClosedResidualV1,
    shm: &ManagedSqliteClosedResidualV1,
) -> Result<[u8; 32]> {
    let path_bytes: Vec<u8> = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_be_bytes)
        .collect();
    let main_bytes = revision_bytes(main.revision);
    let wal_bytes = residual_bytes(wal)?;
    let shm_bytes = residual_bytes(shm)?;
    let mut hash = Sha256::new();
    hash.update(MANAGED_CLOSED_SOURCE_IDENTITY_DOMAIN_V1);
    hash.update([1]);
    hash.update(6u64.to_be_bytes());
    for (name, value) in [
        (
            b"source-policy-fingerprint".as_slice(),
            policy.source_fingerprint().as_slice(),
        ),
        (
            b"managed-snapshot-policy".as_slice(),
            policy.identity_bytes(),
        ),
        (
            b"supplied-absolute-windows-path-utf16be".as_slice(),
            path_bytes.as_slice(),
        ),
        (b"main-identity".as_slice(), main_bytes.as_slice()),
        (b"wal-residual".as_slice(), wal_bytes.as_slice()),
        (b"shm-residual".as_slice(), shm_bytes.as_slice()),
    ] {
        managed_closed_source_named_frame_v1(&mut hash, name, value)?;
    }
    hash.update([0xff]);
    hash.update(1u64.to_be_bytes());
    Ok(hash.finalize().into())
}

pub(super) fn create_destination<F>(path: &Path, mut hook: F) -> Result<IncompleteDestination>
where
    F: FnMut(DestinationSetupPhase) -> Result<()>,
{
    ensure_absolute_path(path, "snapshot destination")?;
    let parent_path = path
        .parent()
        .ok_or_else(|| miette!("snapshot destination has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| miette!("snapshot destination has no leaf"))?;
    let parent = Directory::open_parent(parent_path).into_diagnostic()?;
    parent.require_private_children().into_diagnostic()?;
    // No DELETE right on the retained handle: SQLite's Windows VFS does not
    // share deletion. Cleanup obtains a separate checked handle after close.
    let file = parent.create_database_file(name).into_diagnostic()?;
    let mut destination = IncompleteDestination {
        parent,
        path: path.to_path_buf(),
        file: Some(file),
        identity: None,
        cleanup_armed: true,
    };
    let setup = (|| {
        hook(DestinationSetupPhase::BeforeIdentity)?;
        destination.identity =
            Some(file_identity(destination.file.as_ref().unwrap()).into_diagnostic()?);
        hook(DestinationSetupPhase::AfterIdentity)?;
        destination.verify_owned_path("snapshot destination after create")
    })();
    match setup {
        Ok(()) => Ok(destination),
        Err(error) => match destination.cleanup() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(miette!(
                "{error}; cleanup after snapshot destination setup failure also failed: {cleanup}"
            )),
        },
    }
}

pub(super) fn verify_destination(destination: &IncompleteDestination, role: &str) -> Result<()> {
    destination
        .parent
        .require_private_children()
        .into_diagnostic()?;
    let expected = destination.identity.ok_or_else(|| {
        miette!(
            "{role} identity was never bound; preserving {:?}",
            destination.path
        )
    })?;
    let held = destination
        .file
        .as_ref()
        .ok_or_else(|| miette!("{role} descriptor already closed"))?;
    let name = destination
        .path
        .file_name()
        .ok_or_else(|| miette!("{role} has no leaf"))?;
    for _ in 0..2 {
        require_private_file(held).into_diagnostic()?;
        let named = destination
            .parent
            .open_file(name, false)
            .into_diagnostic()?;
        if !file_identity(held).into_diagnostic()?.same_file(expected)
            || !file_identity(&named).into_diagnostic()?.same_file(expected)
        {
            bail!(
                "{role} no longer names the owned snapshot destination: {:?}",
                destination.path
            );
        }
    }
    destination
        .parent
        .require_private_children()
        .into_diagnostic()?;
    Ok(())
}

pub(super) fn duplicate_destination(
    destination: &IncompleteDestination,
) -> Result<RetainedBackupDestinationIdentity> {
    destination.verify_owned_path("snapshot destination before custody transfer")?;
    let original = destination
        .file
        .as_ref()
        .ok_or_else(|| miette!("destination descriptor closed"))?;
    let file = original.try_clone().into_diagnostic()?;
    let identity = revision(original, &destination.path, "original backup custody")?;
    verify_frozen_file(
        &file,
        identity,
        &destination.path,
        "duplicated backup custody",
    )?;
    verify_frozen_file(
        original,
        identity,
        &destination.path,
        "original backup custody",
    )?;
    destination.verify_owned_path("snapshot destination after custody transfer")?;
    Ok(RetainedBackupDestinationIdentity { file, identity })
}

pub(super) fn remove_destination(destination: &IncompleteDestination) -> std::io::Result<()> {
    let file = destination
        .file
        .as_ref()
        .ok_or_else(|| std::io::Error::other("destination handle closed"))?;
    let expected = file_identity(file)?;
    let bound = destination
        .identity
        .ok_or_else(|| std::io::Error::other("destination identity unbound"))?;
    if !bound.same_file(expected) {
        return Err(std::io::Error::other("destination identity changed"));
    }
    let name = destination
        .path
        .file_name()
        .ok_or_else(|| std::io::Error::other("destination has no leaf"))?;
    destination.parent.remove_file(name, expected)
}

/// Short-lived main-file custody around each path-based SQLite runtime open.
/// The service's store lease remains responsible for lifetime/exclusive ownership.
pub(super) struct RuntimeMainGuard {
    parent: Directory,
    path: PathBuf,
    file: File,
    identity: mneme_store_path::windows::FileIdentity,
}

impl RuntimeMainGuard {
    pub(super) fn open(path: &Path, create_if_missing: bool) -> Result<Self> {
        // SQLite's Rust path adapter consumes UTF-8. Refuse before a fresh
        // protected file is created, rather than leaving an unusable main.
        if path.to_str().is_none() {
            bail!("SQLite runtime path is not representable as UTF-8");
        }
        let path = std::path::absolute(path).into_diagnostic()?;
        let parent_path = path
            .parent()
            .ok_or_else(|| miette!("SQLite runtime path has no parent"))?;
        let name = path
            .file_name()
            .ok_or_else(|| miette!("SQLite runtime path has no leaf"))?;
        let parent = Directory::open_parent(parent_path).into_diagnostic()?;
        parent.require_private_children().into_diagnostic()?;
        let file = match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_if_missing => {
                parent.create_database_file(name).into_diagnostic()?
            }
            Err(error) => return Err(error).into_diagnostic(),
            Ok(_) => parent.open_database_file(name, true).into_diagnostic()?,
        };
        let identity = file_identity(&file).into_diagnostic()?;
        let guard = Self {
            parent,
            path,
            file,
            identity,
        };
        guard.verify()?;
        Ok(guard)
    }

    pub(super) fn verify(&self) -> Result<()> {
        self.parent.require_private_children().into_diagnostic()?;
        require_private_file(&self.file).into_diagnostic()?;
        let name = self
            .path
            .file_name()
            .ok_or_else(|| miette!("SQLite runtime path has no leaf"))?;
        let named = self
            .parent
            .open_database_file(name, false)
            .into_diagnostic()?;
        if !file_identity(&self.file)
            .into_diagnostic()?
            .same_file(self.identity)
            || !file_identity(&named)
                .into_diagnostic()?
                .same_file(self.identity)
        {
            bail!("SQLite runtime main path changed identity while opening");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::io::Write;

    fn root() -> (tempfile::TempDir, Directory) {
        let temp = tempfile::tempdir().unwrap();
        let root = Directory::create(&temp.path().join("private-sqlite")).unwrap();
        (temp, root)
    }

    fn managed(root: &Directory) -> PathBuf {
        let path = root.path().join("memory.db");
        crate::storage::sqlite::existing_snapshot_tests::create_populated_managed_cozo_source(
            &path,
        );
        path
    }

    #[test]
    fn canonical_drive_uri_preserves_unicode_and_escapes_query_characters() {
        let ordinary = immutable_source_uri(Path::new("C:\\owned\\雪 #%.db")).unwrap();
        let verbatim = immutable_source_uri(Path::new("\\\\?\\C:\\owned\\雪 #%.db")).unwrap();
        assert_eq!(ordinary, verbatim);
        assert_eq!(
            ordinary.to_str().unwrap(),
            "file:///C:/owned/%E9%9B%AA%20%23%25.db?mode=ro&immutable=1&cache=private"
        );
        for path in [
            "\\\\server\\share\\db",
            "\\\\?\\UNC\\server\\share\\db",
            "\\\\.\\C:\\db",
            "C:\\file:stream",
        ] {
            assert!(immutable_source_uri(Path::new(path)).is_err(), "{path}");
        }
    }

    #[test]
    fn closed_fence_preserves_full_ntfs_revision_and_permit_reopens() {
        let (_temp, root) = root();
        let path = managed(&root);
        let canonical = fs::canonicalize(&path).unwrap();
        let before = named_revision(&canonical, "fixture").unwrap();
        let bytes = fs::read(&canonical).unwrap();
        let fence = ExistingSqliteSnapshotSource::open(&canonical)
            .unwrap()
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |_| Ok(()))
            .unwrap();
        let identity = fence.source().main_identity();
        assert_eq!(identity.volume_serial_number(), before.identity.volume);
        assert_eq!(*identity.file_id(), before.identity.id);
        assert_eq!(identity.length(), before.identity.len);
        assert_eq!(identity.link_count(), 1);
        assert_eq!(identity.creation_time(), before.creation_time);
        assert_eq!(identity.write_time(), before.write_time);
        assert_eq!(identity.change_time(), before.change_time);
        assert_eq!(identity.attributes(), before.attributes);
        assert_eq!(
            fence.source_policy_fingerprint(),
            &MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1
        );
        let database =
            new_cozo_sqlite_existing(fence.into_existing_sqlite_open_permit_v1()).unwrap();
        database.db.prepare_for_file_move().unwrap();
        drop(database);
        assert_eq!(fs::read(&canonical).unwrap(), bytes);
    }

    #[test]
    fn absent_and_hardlinked_sources_are_refused_without_creation() {
        let (_temp, root) = root();
        let absent = root.path().join("absent.db");
        assert!(ExistingSqliteSnapshotSource::open(&absent).is_err());
        assert!(!absent.exists());
        use std::os::windows::ffi::OsStringExt;
        let unpaired = root
            .path()
            .join(std::ffi::OsString::from_wide(&[0xd800, 46, 100, 98]));
        assert!(RuntimeMainGuard::open(&unpaired, true).is_err());
        assert!(
            !unpaired.exists(),
            "unrepresentable runtime path must not create a file"
        );
        let source = managed(&root);
        let alias = root.path().join("alias.db");
        fs::hard_link(&source, &alias).unwrap();
        let before = fs::read(&source).unwrap();
        assert!(ExistingSqliteSnapshotSource::open(&source).is_err());
        assert_eq!(fs::read(&source).unwrap(), before);
        assert!(!sidecar_path(&source, "-journal").exists());
    }

    #[test]
    fn residuals_bind_identity_bytes_and_exact_resource_limits() {
        let (_temp, root) = root();
        let path = root.path().join("residual");
        let mut file = root.create_file(OsStr::new("residual")).unwrap();
        file.write_all(b"bounded residual").unwrap();
        file.sync_all().unwrap();
        let frozen = FrozenResidualFile::capture(&path, 16, "fixture").unwrap();
        assert!(FrozenResidualFile::capture(&path, 0, "WAL").is_err());
        let closed = closed_residual(frozen.clone());
        assert!(closed_residual_matches_frozen(&closed, &frozen));
        let before = file_revision(&file).unwrap();
        let metadata = closed.metadata().unwrap();
        assert_eq!(metadata.file_id(), &before.identity.id);
        assert_eq!(metadata.change_time(), before.change_time);
        file.write_all(b"!").unwrap();
        file.sync_all().unwrap();
        assert!(FrozenResidualFile::capture(&path, 16, "fixture").is_err());
        let changed = FrozenResidualFile::capture(&path, 17, "fixture").unwrap();
        assert!(!closed_residual_matches_frozen(&closed, &changed));
        fs::hard_link(&path, root.path().join("alias")).unwrap();
        assert!(FrozenResidualFile::capture(&path, 17, "fixture").is_err());
    }

    #[test]
    fn backup_handoff_retains_creation_custody_and_closed_fence() {
        let (_temp, root) = root();
        let source = managed(&root);
        let before = fs::read(&source).unwrap();
        let destination = root.path().join("backup.db");
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();
        let copied = snapshot
            .backup_to_new_snapshot_source_v1(&destination)
            .unwrap();
        let retained = copied.retained_backup_destination.as_ref().unwrap();
        assert_eq!(retained.identity, copied.main_file.identity);
        retained.verify(&destination, "test custody").unwrap();
        let fence = copied
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |_| Ok(()))
            .unwrap();
        assert_eq!(fence.source().supplied_path(), destination);
        assert_eq!(fs::read(&source).unwrap(), before);
        assert!(snapshot.backup_to_new(&destination).is_err());
    }

    #[test]
    fn destination_failure_cleanup_is_one_shot_and_preserves_replacements() {
        let (_temp, root) = root();
        let before_identity = root.path().join("unbound.db");
        assert!(IncompleteDestination::create_failing_before_identity(&before_identity).is_err());
        assert!(
            before_identity.exists(),
            "unbound identity must be preserved"
        );
        let after_identity = root.path().join("bound.db");
        assert!(IncompleteDestination::create_failing_after_identity(&after_identity).is_err());
        assert!(
            !after_identity.exists(),
            "known owned identity should be removed"
        );
        let path = root.path().join("replacement.db");
        let mut guard = IncompleteDestination::create(&path).unwrap();
        let moved = root.path().join("moved.db");
        fs::rename(&path, &moved).unwrap();
        let mut replacement = root.create_file(OsStr::new("replacement.db")).unwrap();
        replacement.write_all(b"foreign replacement").unwrap();
        assert!(guard.cleanup().is_err());
        drop(guard);
        assert_eq!(fs::read(&path).unwrap(), b"foreign replacement");
        assert!(moved.exists());
    }

    #[test]
    fn schema_preflight_refuses_large_tree_before_sqlite_open() {
        let (_temp, root) = root();
        let path = managed(&root);
        let file = root
            .open_database_file(OsStr::new("memory.db"), true)
            .unwrap();
        file.seek_write(&[0x05], 100).unwrap();
        file.sync_all().unwrap();
        let before = fs::read(&path).unwrap();
        assert!(ExistingSqliteSnapshotSource::open(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!sidecar_path(&path, "-journal").exists());
    }

    #[test]
    fn windows_source_fingerprint_has_a_hostile_utf16_known_answer() {
        use std::os::windows::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_wide(&[
            67, 58, 92, 0xd800, 46, 100, 98,
        ]));
        let revision = FileRevision {
            identity: mneme_store_path::windows::FileIdentity {
                volume: 0x0102030405060708,
                id: [0xa5; 16],
                links: 1,
                len: 4096,
            },
            creation_time: -2,
            write_time: 3,
            change_time: -4,
            attributes: 0x20,
        };
        let main = ManagedSqliteClosedMainFileIdentityV1 {
            revision,
            _thread_bound: PhantomData,
        };
        let wal = ManagedSqliteClosedResidualV1 {
            present: true,
            metadata: Some(ManagedSqliteClosedResidualMetadataV1 {
                revision,
                _thread_bound: PhantomData,
            }),
            sha256: Some(Sha256::digest([]).into()),
            _thread_bound: PhantomData,
        };
        let shm = closed_residual(FrozenResidualFile::Absent);
        let actual =
            source_identity_fingerprint(&path, ManagedSnapshotPolicy::V1, &main, &wal, &shm)
                .unwrap();
        // Independently generated fixed UTF-16BE path and all field framing;
        // an unpaired surrogate must not silently become replacement UTF-8.
        assert_eq!(
            actual,
            [
                207, 47, 225, 3, 151, 216, 28, 254, 199, 18, 240, 201, 167, 246, 162, 138, 165,
                213, 240, 35, 143, 129, 236, 79, 76, 132, 164, 225, 171, 126, 252, 12
            ]
        );
        let mut changed = ManagedSqliteClosedMainFileIdentityV1 {
            revision,
            _thread_bound: PhantomData,
        };
        changed.revision.identity.id[15] ^= 1;
        assert_ne!(
            actual,
            source_identity_fingerprint(&path, ManagedSnapshotPolicy::V1, &changed, &wal, &shm)
                .unwrap()
        );
        assert_ne!(
            actual,
            source_identity_fingerprint(&path, ManagedSnapshotPolicy::V1, &main, &shm, &wal)
                .unwrap()
        );
    }

    #[test]
    fn main_revision_encoding_preserves_full_id_signed_times_and_field_order() {
        let revision = FileRevision {
            identity: mneme_store_path::windows::FileIdentity {
                volume: 0x0102030405060708,
                id: [0xa5; 16],
                links: 1,
                len: 4096,
            },
            creation_time: -2,
            write_time: 3,
            change_time: -4,
            attributes: 0x20,
        };
        let bytes = revision_bytes(revision);
        assert_eq!(bytes.len(), 64);
        assert_eq!(&bytes[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&bytes[8..24], &[0xa5; 16]);
        assert_eq!(&bytes[24..28], &[0, 0, 0, 1]);
        assert_eq!(&bytes[28..36], &[0, 0, 0, 0, 0, 0, 16, 0]);
        assert_eq!(&bytes[36..44], &[255, 255, 255, 255, 255, 255, 255, 254]);
        assert_eq!(&bytes[44..52], &[0, 0, 0, 0, 0, 0, 0, 3]);
        assert_eq!(&bytes[52..60], &[255, 255, 255, 255, 255, 255, 255, 252]);
        assert_eq!(&bytes[60..64], &[0, 0, 0, 32]);
    }
}

// Independent policy-oracle tests in the parent module pin these literals.
pub(super) const MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    123, 79, 192, 20, 79, 29, 71, 134, 134, 52, 212, 14, 165, 8, 40, 171, 192, 106, 249, 95, 20,
    124, 80, 150, 241, 19, 215, 81, 233, 169, 223, 51,
];
pub(super) const MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    47, 34, 198, 167, 252, 85, 183, 55, 186, 202, 23, 158, 10, 85, 46, 69, 102, 151, 124, 97, 128,
    35, 65, 180, 184, 211, 69, 16, 174, 35, 245, 86,
];
pub(super) const MANAGED_EPISODE_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    60, 147, 55, 15, 42, 156, 69, 49, 25, 221, 227, 92, 240, 153, 243, 248, 69, 118, 127, 146, 201,
    174, 51, 245, 210, 36, 39, 71, 136, 172, 64, 214,
];
pub(super) const MANAGED_EPISODE_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    241, 32, 90, 231, 217, 205, 37, 161, 169, 44, 182, 73, 53, 132, 31, 226, 236, 116, 3, 100, 9,
    218, 172, 38, 196, 89, 186, 101, 130, 85, 66, 160,
];
pub(super) const MANAGED_SINGLE_GRAPH_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    59, 191, 13, 18, 59, 44, 204, 147, 229, 111, 113, 127, 135, 28, 215, 62, 43, 132, 54, 151, 216,
    192, 138, 135, 187, 91, 14, 51, 23, 51, 220, 188,
];
pub(super) const MANAGED_SINGLE_GRAPH_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    20, 40, 106, 135, 126, 37, 114, 80, 194, 164, 25, 196, 98, 0, 225, 41, 248, 1, 26, 249, 185,
    22, 15, 124, 99, 140, 73, 216, 34, 134, 194, 167,
];
pub(super) const MANAGED_CONCERN_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    112, 252, 189, 184, 127, 245, 197, 49, 146, 32, 81, 44, 110, 26, 86, 209, 246, 190, 41, 21,
    182, 32, 49, 162, 229, 209, 41, 9, 26, 82, 137, 163,
];
pub(super) const MANAGED_CONCERN_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    85, 84, 204, 249, 47, 62, 60, 27, 47, 142, 121, 11, 31, 105, 49, 13, 55, 164, 70, 223, 13, 240,
    60, 249, 231, 227, 16, 0, 135, 26, 85, 37,
];
pub(super) const MANAGED_TOUCHSTONES_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    208, 120, 206, 45, 29, 204, 61, 43, 57, 160, 116, 34, 247, 181, 217, 58, 166, 86, 212, 226,
    214, 101, 83, 125, 188, 145, 198, 45, 172, 37, 188, 222,
];
pub(super) const MANAGED_TOUCHSTONES_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    89, 207, 38, 149, 5, 138, 93, 189, 65, 189, 243, 207, 231, 115, 40, 123, 215, 169, 173, 235,
    88, 52, 49, 60, 135, 140, 241, 199, 210, 35, 7, 245,
];
