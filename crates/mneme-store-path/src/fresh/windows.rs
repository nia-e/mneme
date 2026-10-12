//! Windows filesystem operations for the shared fresh-publication state machine.
//!
//! Child mutations are relative to retained directory handles; every removal
//! validates the full expected identity and publication renames the retained
//! main handle without replacement. Path-based enumeration is bounded and
//! guarded, and never itself authorizes deletion.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::FileExt;
use std::path::Path;

use super::record::MAX_RECORD_BYTES;
use super::{
    FreshFileIdentityV1, FreshStoreStageV1, INTENT_RECORD, MAX_STAGE_ENTRIES, STAGE_DATABASE,
    StageDirectory, StageIdentity, invalid_data, invalid_input,
};
use crate::StoreLease;
use crate::windows::{file_identity, require_private_file};

pub(super) fn require_fresh_target_parent(lease: &StoreLease) -> io::Result<()> {
    lease.parent.require_named()?;
    if !lease.parent.identity().same_object(lease.parent_identity) {
        return Err(invalid_data("fresh-store target parent changed identity"));
    }
    Ok(())
}

pub(super) fn ascii_casefold_equal(left: &OsStr, right: &OsStr) -> bool {
    let mut left = left.encode_wide();
    let mut right = right.encode_wide();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(left), Some(right)) if fold_ascii(left) == fold_ascii(right) => {}
            _ => return false,
        }
    }
}

fn fold_ascii(value: u16) -> u16 {
    if (u16::from(b'A')..=u16::from(b'Z')).contains(&value) {
        value + u16::from(b'a' - b'A')
    } else {
        value
    }
}

pub(super) fn create_stage_directory(
    lease: &StoreLease,
    name: &OsStr,
) -> io::Result<(StageDirectory, StageIdentity)> {
    let directory = lease.parent.create_directory(name)?;
    directory.require_private()?;
    let identity = directory.identity();
    if identity.volume != lease.parent_identity.volume {
        return Err(invalid_data("fresh stage is not on the target filesystem"));
    }
    Ok((directory, identity))
}

pub(super) fn create_record(
    stage_dir: &StageDirectory,
    name: &str,
    bytes: &[u8],
) -> io::Result<(File, FreshFileIdentityV1)> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(invalid_input("fresh-store record exceeds its fixed bound"));
    }
    let mut file = stage_dir.create_file(OsStr::new(name))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let identity = validate_file(&file, Path::new(name))?;
    Ok((file, identity))
}

pub(super) fn create_empty_main(
    stage_dir: &StageDirectory,
    display_path: &Path,
) -> io::Result<(File, FreshFileIdentityV1)> {
    // SQLite does not grant FILE_SHARE_DELETE when it opens its database.
    // The continuously retained main must therefore not request DELETE access.
    let file = stage_dir.create_database_file(OsStr::new(STAGE_DATABASE))?;
    file.sync_all()?;
    let identity = validate_file(&file, display_path)?;
    if identity.len != 0 {
        return Err(invalid_data("new fresh-store main was not empty"));
    }
    Ok((file, identity))
}

pub(super) fn open_private_file_at(
    parent: &StageDirectory,
    name: &OsStr,
    label: &str,
) -> io::Result<File> {
    let file = parent.open_file(name, false).map_err(|source| {
        io::Error::new(source.kind(), format!("cannot open {label}: {source}"))
    })?;
    require_private_file(&file)?;
    Ok(file)
}

pub(super) fn validate_file(file: &File, _path: &Path) -> io::Result<FreshFileIdentityV1> {
    require_private_file(file)?;
    file_identity(file)
}

pub(super) fn validate_main(file: &File, path: &Path) -> io::Result<FreshFileIdentityV1> {
    let identity = validate_file(file, path)?;
    if identity.len == 0 {
        return Err(invalid_data(format!(
            "fresh-store main {} is empty",
            path.display()
        )));
    }
    Ok(identity)
}

pub(super) fn verify_record(
    parent: &StageDirectory,
    name: &str,
    retained: &File,
    identity: FreshFileIdentityV1,
    expected: &[u8],
) -> io::Result<()> {
    if validate_file(retained, Path::new(name))? != identity {
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
    if read_bounded_at(retained, MAX_RECORD_BYTES)? != expected {
        return Err(invalid_data(format!(
            "fresh-store record {name} does not match this operation"
        )));
    }
    Ok(())
}

fn read_bounded_at(file: &File, max: usize) -> io::Result<Vec<u8>> {
    let declared = usize::try_from(file_identity(file)?.len)
        .map_err(|_| invalid_data("fresh-store record length does not fit memory"))?;
    if declared > max {
        return Err(invalid_data("fresh-store record exceeds its fixed bound"));
    }
    let mut bytes = vec![0; declared];
    let mut read = 0;
    while read < declared {
        let count = file.seek_read(&mut bytes[read..], u64::try_from(read).unwrap())?;
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
    parent: &StageDirectory,
    name: &OsStr,
    expected: FreshFileIdentityV1,
    require_nonempty: bool,
    label: &str,
) -> io::Result<()> {
    let file = open_private_file_at(parent, name, label)?;
    let observed = if require_nonempty {
        validate_main(&file, Path::new(name))?
    } else {
        validate_file(&file, Path::new(name))?
    };
    if observed != expected {
        return Err(invalid_data(format!("{label} changed identity")));
    }
    Ok(())
}

pub(super) fn require_named_file_stable_identity(
    parent: &StageDirectory,
    name: &OsStr,
    expected: FreshFileIdentityV1,
    label: &str,
) -> io::Result<()> {
    let file = open_private_file_at(parent, name, label)?;
    let observed = validate_file(&file, Path::new(name))?;
    if !expected.same_file(observed) {
        return Err(invalid_data(format!("{label} changed stable identity")));
    }
    Ok(())
}

pub(super) fn require_named_stage_directory(
    lease: &StoreLease,
    name: &OsStr,
    expected: StageIdentity,
) -> io::Result<()> {
    // Recheck the exact retained parent and lease, not merely a stage pathname.
    lease.parent.require_named()?;
    let lock_name = lease
        .lock_path
        .file_name()
        .ok_or_else(|| invalid_input("fresh-store lease has no filename"))?;
    let named_lock = lease.parent.open_file(lock_name, true)?;
    if !file_identity(&named_lock)?.same_file(lease.lock_identity)
        || !file_identity(&lease.file)?.same_file(lease.lock_identity)
        || !lease.parent.identity().same_object(lease.parent_identity)
    {
        return Err(invalid_data("fresh-store lease changed identity"));
    }
    let directory = lease.parent.open_directory(name)?;
    directory.require_private()?;
    if !directory.identity().same_object(expected) {
        return Err(invalid_data(
            "fresh-store operation directory changed identity",
        ));
    }
    Ok(())
}

pub(super) fn require_name_absent(
    parent: &StageDirectory,
    name: &OsStr,
    label: &str,
) -> io::Result<()> {
    match parent.open_file(name, false) {
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
        let mut name = target_name.to_os_string();
        name.push(suffix);
        require_name_absent(&lease.parent, &name, "fresh-store target sidecar")?;
    }
    Ok(())
}

pub(super) fn sqlite_sidecar_name(name: &str, suffix: &str) -> OsString {
    let mut name = OsString::from(name);
    name.push(suffix);
    name
}

pub(super) fn stage_inventory(path: &Path, max: usize) -> io::Result<BTreeSet<OsString>> {
    let directory = StageDirectory::open(path)?;
    directory.require_named()?;
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
    directory.require_named()?;
    Ok(names)
}

pub(super) fn require_exact_inventory(
    stage_path: &Path,
    lease: &StoreLease,
    stage_identity: StageIdentity,
    expected: &[&str],
) -> io::Result<()> {
    let name = stage_path
        .file_name()
        .ok_or_else(|| invalid_input("fresh stage has no filename"))?;
    require_named_stage_directory(lease, name, stage_identity)?;
    let observed = stage_inventory(stage_path, MAX_STAGE_ENTRIES)?;
    let expected: BTreeSet<_> = expected.iter().map(OsString::from).collect();
    if observed != expected {
        return Err(invalid_data(format!(
            "fresh-store stage inventory is not exact (expected {} entries, observed {})",
            expected.len(),
            observed.len()
        )));
    }
    require_named_stage_directory(lease, name, stage_identity)
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
    let name = stage
        .stage_path
        .file_name()
        .ok_or_else(|| invalid_input("fresh stage has no filename"))?;
    require_named_stage_directory(&stage.lease, name, stage.stage_identity)
}

pub(super) fn remove_stage_directory(
    lease: &StoreLease,
    stage_path: &Path,
    expected: StageIdentity,
) -> io::Result<()> {
    let name = stage_path
        .file_name()
        .ok_or_else(|| invalid_input("fresh stage has no filename"))?;
    require_named_stage_directory(lease, name, expected)?;
    lease.parent.remove_directory(name, expected)
}

pub(super) fn unlink_validated_file(
    parent: &StageDirectory,
    name: &OsStr,
    expected: FreshFileIdentityV1,
) -> io::Result<()> {
    parent.remove_file(name, expected)
}
