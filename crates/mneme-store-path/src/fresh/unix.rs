use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::Path;

use super::record::MAX_RECORD_BYTES;
use super::{
    FreshFileIdentityV1, FreshStoreStageV1, INTENT_RECORD, MAX_STAGE_ENTRIES, STAGE_DATABASE,
    invalid_data, invalid_input, unsupported,
};
use crate::{
    StoreLease, UnixIdentity, openat_file, require_named_lease_identity, require_parent_identity,
};

pub(super) fn require_fresh_target_parent(lease: &StoreLease) -> io::Result<()> {
    require_parent_identity(&lease.parent_path, &lease.parent, lease.parent_identity)?;
    let metadata = lease.parent.metadata()?;
    let owner = unsafe { libc::geteuid() };
    let mode = metadata.permissions().mode() & 0o7777;
    if metadata.uid() != owner || mode & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "fresh-store target parent {} must be current-user owned and not writable by group or other; shared or root-owned sticky parents are not publication authorities (owner {}, mode {:04o})",
                lease.parent_path.display(),
                metadata.uid(),
                mode
            ),
        ));
    }
    Ok(())
}

pub(super) fn ascii_casefold_equal(left: &OsStr, right: &OsStr) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

pub(super) fn mkdir_private_at(parent: &File, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    // Unlike a newly created file, a mode-000 directory cannot be opened to
    // repair its mode through the resulting descriptor.  Repair it relative
    // to the already validated parent before attempting that open.  The
    // no-follow flag prevents a pathname replacement from redirecting chmod.
    let rc = unsafe {
        libc::fchmodat(
            parent.as_raw_fd(),
            name.as_ptr(),
            0o700,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn open_stage_directory(lease: &StoreLease, name: &OsStr) -> io::Result<File> {
    openat_file(
        &lease.parent,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
}

pub(super) fn validate_stage_directory(
    metadata: &fs::Metadata,
    path: &Path,
    parent_device: u64,
) -> io::Result<UnixIdentity> {
    let mode = metadata.permissions().mode() & 0o7777;
    let owner = unsafe { libc::geteuid() };
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != owner
        || mode != 0o700
        || metadata.dev() != parent_device
    {
        return Err(invalid_data(format!(
            "fresh stage {} must be a current-owner directory on the target filesystem with exact mode 0700",
            path.display()
        )));
    }
    Ok(UnixIdentity::from_metadata(metadata))
}

pub(super) fn set_exact_mode(file: &File, mode: libc::mode_t, label: &str) -> io::Result<()> {
    let rc = unsafe { libc::fchmod(file.as_raw_fd(), mode) };
    if rc == 0 {
        Ok(())
    } else {
        let source = io::Error::last_os_error();
        Err(io::Error::new(
            source.kind(),
            format!("cannot set {label} to mode {mode:04o}: {source}"),
        ))
    }
}

pub(super) fn create_record(
    stage_dir: &File,
    name: &str,
    bytes: &[u8],
) -> io::Result<(File, FreshFileIdentityV1)> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(invalid_input("fresh-store record exceeds its fixed bound"));
    }
    let mut file = openat_file(
        stage_dir,
        OsStr::new(name),
        libc::O_RDWR
            | libc::O_CREAT
            | libc::O_EXCL
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK,
        0o600,
    )?;
    set_exact_mode(&file, 0o600, "fresh-store record")?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let identity = validate_private_file(&file.metadata()?, Path::new(name))?;
    Ok((file, identity))
}

pub(super) fn create_empty_main(
    stage_dir: &File,
    display_path: &Path,
) -> io::Result<(File, FreshFileIdentityV1)> {
    let file = openat_file(
        stage_dir,
        OsStr::new(STAGE_DATABASE),
        libc::O_RDWR
            | libc::O_CREAT
            | libc::O_EXCL
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK,
        0o600,
    )?;
    set_exact_mode(&file, 0o600, "fresh-store main")?;
    file.sync_all()?;
    let identity = validate_private_file(&file.metadata()?, display_path)?;
    if identity.len != 0 {
        return Err(invalid_data("new fresh-store main was not empty"));
    }
    Ok((file, identity))
}

pub(super) fn open_private_file_at(parent: &File, name: &OsStr, label: &str) -> io::Result<File> {
    openat_file(
        parent,
        name,
        libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        0,
    )
    .map_err(|source| io::Error::new(source.kind(), format!("cannot open {label}: {source}")))
}

pub(super) fn validate_private_file(
    metadata: &fs::Metadata,
    path: &Path,
) -> io::Result<FreshFileIdentityV1> {
    let mode = metadata.permissions().mode() & 0o7777;
    let owner = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != owner
        || mode != 0o600
        || metadata.nlink() != 1
    {
        return Err(invalid_data(format!(
            "fresh-store file {} must be a current-owner regular file with exact mode 0600 and one link",
            path.display()
        )));
    }
    Ok(FreshFileIdentityV1 {
        device: metadata.dev(),
        inode: metadata.ino(),
        owner: metadata.uid(),
        mode,
        links: metadata.nlink(),
        len: metadata.len(),
    })
}

pub(super) fn validate_private_main(
    metadata: &fs::Metadata,
    path: &Path,
) -> io::Result<FreshFileIdentityV1> {
    let identity = validate_private_file(metadata, path)?;
    if identity.len == 0 {
        return Err(invalid_data(format!(
            "fresh-store main {} is empty",
            path.display()
        )));
    }
    Ok(identity)
}

pub(super) fn verify_record(
    parent: &File,
    name: &str,
    retained: &File,
    identity: FreshFileIdentityV1,
    expected: &[u8],
) -> io::Result<()> {
    let opened = validate_private_file(&retained.metadata()?, Path::new(name))?;
    if opened != identity {
        return Err(invalid_data(format!(
            "fresh-store record {name} descriptor changed identity"
        )));
    }
    require_named_file_identity(
        parent,
        OsStr::new(name),
        identity,
        false,
        "fresh-store record",
    )?;
    let bytes = read_bounded_at(retained, MAX_RECORD_BYTES)?;
    if bytes != expected {
        return Err(invalid_data(format!(
            "fresh-store record {name} does not match this operation"
        )));
    }
    Ok(())
}

fn read_bounded_at(file: &File, max: usize) -> io::Result<Vec<u8>> {
    let declared = usize::try_from(file.metadata()?.len())
        .map_err(|_| invalid_data("fresh-store record length does not fit memory"))?;
    if declared > max {
        return Err(invalid_data("fresh-store record exceeds its fixed bound"));
    }
    let mut bytes = vec![0; declared];
    let mut read = 0;
    while read < declared {
        let count = file.read_at(&mut bytes[read..], u64::try_from(read).unwrap())?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "fresh-store record was truncated while reading",
            ));
        }
        read += count;
    }
    Ok(bytes)
}

pub(super) fn require_named_file_identity(
    parent: &File,
    name: &OsStr,
    expected: FreshFileIdentityV1,
    require_nonempty: bool,
    label: &str,
) -> io::Result<()> {
    let file = open_private_file_at(parent, name, label)?;
    let observed = if require_nonempty {
        validate_private_main(&file.metadata()?, Path::new(name))?
    } else {
        validate_private_file(&file.metadata()?, Path::new(name))?
    };
    if observed != expected {
        return Err(invalid_data(format!("{label} changed identity")));
    }
    Ok(())
}

pub(super) fn require_named_file_stable_identity(
    parent: &File,
    name: &OsStr,
    expected: FreshFileIdentityV1,
    label: &str,
) -> io::Result<()> {
    let file = open_private_file_at(parent, name, label)?;
    let observed = validate_private_file(&file.metadata()?, Path::new(name))?;
    if !expected.same_stable_file(observed) {
        return Err(invalid_data(format!("{label} changed stable identity")));
    }
    Ok(())
}

pub(super) fn require_named_stage_directory(
    lease: &StoreLease,
    name: &OsStr,
    expected: UnixIdentity,
) -> io::Result<()> {
    require_parent_identity(&lease.parent_path, &lease.parent, lease.parent_identity)?;
    require_named_lease_identity(&lease.lock_path, &lease.file, lease.lock_identity)?;
    let dir = open_stage_directory(lease, name)?;
    let observed = validate_stage_directory(
        &dir.metadata()?,
        &lease.parent_path.join(name),
        lease.parent_identity.device,
    )?;
    // Directory link counts are not a stable identity component while stage
    // entries are created and removed (and APFS reports these changes).
    if !observed.same_parent(expected) {
        return Err(invalid_data(
            "fresh-store operation directory changed identity",
        ));
    }
    Ok(())
}

pub(super) fn require_name_absent(parent: &File, name: &OsStr, label: &str) -> io::Result<()> {
    match openat_file(
        parent,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        0,
    ) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{label} already exists"),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{label} is not safely absent: {error}"),
        )),
    }
}

pub(super) fn require_target_family_absent(
    lease: &StoreLease,
    target_name: &OsStr,
) -> io::Result<()> {
    lease.require_guards(&lease.parent_path.join(target_name))?;
    require_name_absent(&lease.parent, target_name, "fresh-store target")?;
    require_target_sidecars_absent(lease, target_name)
}

pub(super) fn require_target_sidecars_absent(
    lease: &StoreLease,
    target_name: &OsStr,
) -> io::Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let name = sqlite_sidecar_os_name(target_name, suffix);
        require_name_absent(&lease.parent, &name, "fresh-store target sidecar")?;
    }
    Ok(())
}

pub(super) fn sqlite_sidecar_name(name: &str, suffix: &str) -> OsString {
    sqlite_sidecar_os_name(OsStr::new(name), suffix)
}

fn sqlite_sidecar_os_name(name: &OsStr, suffix: &str) -> OsString {
    let mut result = name.to_os_string();
    result.push(suffix);
    result
}

pub(super) fn stage_inventory(path: &Path, max: usize) -> io::Result<BTreeSet<OsString>> {
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(path)?.take(max + 1) {
        let entry = entry?;
        if names.len() == max {
            return Err(invalid_data(
                "fresh-store stage has too many directory entries",
            ));
        }
        names.insert(entry.file_name());
    }
    Ok(names)
}

pub(super) fn require_exact_inventory(
    stage_path: &Path,
    lease: &StoreLease,
    stage_identity: UnixIdentity,
    expected: &[&str],
) -> io::Result<()> {
    let stage_name = stage_path
        .file_name()
        .ok_or_else(|| invalid_input("fresh stage has no filename"))?;
    require_named_stage_directory(lease, stage_name, stage_identity)?;
    let inventory = stage_inventory(stage_path, MAX_STAGE_ENTRIES)?;
    require_inventory_set(&inventory, &expected.iter().map(OsString::from).collect())?;
    require_named_stage_directory(lease, stage_name, stage_identity)
}

fn require_inventory_set(
    observed: &BTreeSet<OsString>,
    expected: &BTreeSet<OsString>,
) -> io::Result<()> {
    if observed != expected {
        return Err(invalid_data(format!(
            "fresh-store stage inventory is not exact (expected {} entries, observed {})",
            expected.len(),
            observed.len()
        )));
    }
    Ok(())
}

pub(super) fn require_building_inventory(stage: &FreshStoreStageV1) -> io::Result<()> {
    let inventory = stage_inventory(&stage.stage_path, MAX_STAGE_ENTRIES)?;
    let allowed = BTreeSet::from([
        OsString::from(INTENT_RECORD),
        OsString::from(STAGE_DATABASE),
        sqlite_sidecar_name(STAGE_DATABASE, "-wal"),
        sqlite_sidecar_name(STAGE_DATABASE, "-shm"),
        sqlite_sidecar_name(STAGE_DATABASE, "-journal"),
    ]);
    if !inventory.contains(OsStr::new(INTENT_RECORD))
        || inventory.iter().any(|name| !allowed.contains(name))
    {
        return Err(invalid_data(
            "fresh-store build stage contains an unrecognized entry",
        ));
    }
    Ok(())
}

pub(super) fn remove_stage_directory(
    lease: &StoreLease,
    stage_path: &Path,
    expected: UnixIdentity,
) -> io::Result<()> {
    let name = stage_path
        .file_name()
        .ok_or_else(|| invalid_input("fresh stage has no filename"))?;
    require_named_stage_directory(lease, name, expected)?;
    let name = c_name(name)?;
    let rc = unsafe { libc::unlinkat(lease.parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(super) fn unlink_file_at(parent: &File, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
pub(super) fn rename_no_replace_at(
    from_parent: &File,
    from: &OsStr,
    to_parent: &File,
    to: &OsStr,
) -> io::Result<()> {
    let from = c_name(from)?;
    let to = c_name(to)?;
    let rc = unsafe {
        libc::renameat2(
            from_parent.as_raw_fd(),
            from.as_ptr(),
            to_parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        let source = io::Error::last_os_error();
        if matches!(
            source.raw_os_error(),
            Some(code)
                if code == libc::ENOSYS || code == libc::EINVAL || code == libc::ENOTSUP
        ) {
            Err(unsupported())
        } else {
            Err(source)
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(super) fn rename_no_replace_at(
    from_parent: &File,
    from: &OsStr,
    to_parent: &File,
    to: &OsStr,
) -> io::Result<()> {
    let from = c_name(from)?;
    let to = c_name(to)?;
    let rc = unsafe {
        libc::renameatx_np(
            from_parent.as_raw_fd(),
            from.as_ptr(),
            to_parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        let source = io::Error::last_os_error();
        if matches!(
            source.raw_os_error(),
            Some(code) if code == libc::ENOTSUP || code == libc::EINVAL
        ) {
            Err(unsupported())
        } else {
            Err(source)
        }
    }
}

#[cfg(all(
    unix,
    not(target_os = "linux"),
    not(target_os = "macos"),
    not(target_os = "ios")
))]
pub(super) fn rename_no_replace_at(
    _from_parent: &File,
    _from: &OsStr,
    _to_parent: &File,
    _to: &OsStr,
) -> io::Result<()> {
    Err(unsupported())
}

fn c_name(name: &OsStr) -> io::Result<std::ffi::CString> {
    if name.as_bytes().contains(&b'/') {
        return Err(invalid_input("fresh-store entry name contains a separator"));
    }
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| invalid_input("fresh-store entry name contains an interior NUL"))
}
