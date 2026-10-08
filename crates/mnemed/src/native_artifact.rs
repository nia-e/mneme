//! Shared fail-closed primitives for native, authenticated filesystem artifacts.
//!
//! Policy, state machines, and artifact schemas belong to their callers. This
//! module only centralizes the byte-preserving cryptographic and filesystem
//! mechanics those callers share.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use getrandom::fill as random_fill;
use serde::Serialize;
use sha2::{Digest, Sha256};
use ulid::Ulid;

use crate::AnyErr;

pub(super) const KEY_BYTES: usize = 32;

pub(super) fn canonical_git_invocation_root(
    requested_root: &Path,
    invocation_label: &str,
) -> Result<PathBuf, AnyErr> {
    let root = fs::canonicalize(requested_root)?;
    let cwd = fs::canonicalize(std::env::current_dir()?)?;
    if cwd != root {
        return Err(format!(
            "{invocation_label} must run with cwd exactly at repository root {}; current cwd is {}",
            root.display(),
            cwd.display()
        )
        .into());
    }
    if !root.join(".git").exists() {
        return Err(format!("{invocation_label} requires a Git repository root").into());
    }
    Ok(root)
}

pub(super) fn read_bounded_regular(
    path: &Path,
    max: usize,
    label: &str,
) -> Result<Vec<u8>, AnyErr> {
    let mut file = open_read_nofollow(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(format!("{label} {} is not a regular file", path.display()).into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(format!(
                "{label} {} has {} hard links; refusing ambiguous authority",
                path.display(),
                metadata.nlink()
            )
            .into());
        }
    }
    let declared = usize::try_from(metadata.len()).map_err(|_| format!("{label} is too large"))?;
    if declared > max {
        return Err(format!("{label} exceeds its hard {max}-byte maximum").into());
    }
    let mut bytes = Vec::with_capacity(declared.saturating_add(1));
    Read::by_ref(&mut file)
        .take(u64::try_from(max)?.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(format!("{label} exceeds its hard {max}-byte maximum").into());
    }
    verify_path_still_names_open_file(path, &metadata)?;
    Ok(bytes)
}

pub(super) fn open_read_nofollow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(not(unix))]
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing symlink",
        ));
    }
    options.open(path)
}

#[cfg(unix)]
pub(super) fn open_dir_nofollow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    options.open(path)
}

#[cfg(not(unix))]
pub(super) fn open_dir_nofollow(path: &Path) -> std::io::Result<File> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing symlink",
        ));
    }
    File::open(path)
}

pub(super) fn verify_path_still_names_open_file(
    path: &Path,
    opened: &fs::Metadata,
) -> Result<(), AnyErr> {
    verify_path_still_names_open_file_or_dir(path, opened, false)
}

pub(super) fn verify_path_still_names_open_file_or_dir(
    path: &Path,
    opened: &fs::Metadata,
    directory: bool,
) -> Result<(), AnyErr> {
    let current = fs::symlink_metadata(path)?;
    let right_type = if directory {
        current.file_type().is_dir()
    } else {
        current.file_type().is_file()
    };
    if !right_type || current.file_type().is_symlink() {
        return Err(format!("{} changed identity while it was open", path.display()).into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if current.dev() != opened.dev() || current.ino() != opened.ino() {
            return Err(format!("{} was replaced while it was open", path.display()).into());
        }
    }
    Ok(())
}

pub(super) struct ExclusiveLock {
    _file: File,
}

impl ExclusiveLock {
    pub(super) fn acquire(
        path: &Path,
        artifact_label: &str,
        contention_owner: &str,
    ) -> Result<Self, AnyErr> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let metadata = file.metadata()?;
            if !metadata.file_type().is_file()
                || metadata.nlink() != 1
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(format!(
                    "{artifact_label} must be a single-linked private regular file"
                )
                .into());
            }
            use std::os::fd::AsRawFd;
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                return Err(format!(
                    "{contention_owner} owns {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                )
                .into());
            }
            verify_path_still_names_open_file(path, &metadata)?;
        }
        Ok(Self { _file: file })
    }
}

pub(super) fn hash_serialized(value: &impl Serialize) -> Result<String, AnyErr> {
    Ok(sha256_bytes(&serde_json::to_vec(value)?))
}

pub(super) fn sha256_bytes(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes).as_slice())
}

pub(super) fn sha256_join(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part);
        hash.update([0]);
    }
    hex_digest(hash.finalize().as_slice())
}

pub(super) fn hmac_hex(key: &[u8], message: &[u8]) -> String {
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for index in 0..64 {
        inner_pad[index] ^= block[index];
        outer_pad[index] ^= block[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    hex_digest(outer.finalize().as_slice())
}

pub(super) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

pub(super) fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        value.push(HEX[(byte >> 4) as usize] as char);
        value.push(HEX[(byte & 0x0f) as usize] as char);
    }
    value
}

pub(super) fn load_or_create_key(path: &Path) -> Result<[u8; KEY_BYTES], AnyErr> {
    match fs::symlink_metadata(path) {
        Ok(_) => load_existing_key(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or("native key has no parent")?;
            ensure_private_dir(parent)?;
            let mut key = [0u8; KEY_BYTES];
            random_fill(&mut key).map_err(|error| format!("OS randomness unavailable: {error}"))?;
            publish_private_bytes_no_clobber(path, &key)?;
            load_existing_key(path)
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) fn load_existing_key(path: &Path) -> Result<[u8; KEY_BYTES], AnyErr> {
    let mut file = open_read_nofollow(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err("native bootstrap key is not a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.nlink() != 1 || metadata.permissions().mode() & 0o077 != 0 {
            return Err("native bootstrap key must be single-linked and private (0600)".into());
        }
    }
    let mut bytes = Vec::with_capacity(KEY_BYTES + 1);
    Read::by_ref(&mut file)
        .take((KEY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let key = bytes
        .try_into()
        .map_err(|_| "native bootstrap key must be exactly 32 bytes")?;
    verify_path_still_names_open_file(path, &metadata)?;
    Ok(key)
}

fn publish_private_bytes_no_clobber(path: &Path, bytes: &[u8]) -> Result<(), AnyErr> {
    let parent = path.parent().ok_or("publication path has no parent")?;
    let temp = parent.join(format!(".native-key-{}", Ulid::new()));
    write_private_file(&temp, bytes)?;
    match rename_no_replace(&temp, path) {
        Ok(()) => {
            sync_dir(parent)?;
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error.into())
        }
    }
}

pub(super) fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), AnyErr> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub(super) fn create_private_dir(path: &Path) -> Result<(), AnyErr> {
    fs::create_dir(path)?;
    set_private_dir(path)
}

pub(super) fn ensure_private_dir(path: &Path) -> Result<(), AnyErr> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            set_private_dir(path)
        }
        Ok(_) => Err(format!("{} is not a regular directory", path.display()).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_private_dir(path),
        Err(error) => Err(error.into()),
    }
}

fn set_private_dir(path: &Path) -> Result<(), AnyErr> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let directory = open_dir_nofollow(path)?;
        let opened = directory.metadata()?;
        if !opened.file_type().is_dir() {
            return Err(format!("{} is not a real directory", path.display()).into());
        }
        let rc = unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        verify_path_still_names_open_file_or_dir(path, &opened, true)?;
    }
    Ok(())
}

pub(super) fn sync_dir(path: &Path) -> Result<(), AnyErr> {
    #[cfg(unix)]
    {
        open_dir_nofollow(path)?.sync_all()?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(super) fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(
    unix,
    not(target_os = "linux"),
    not(target_os = "macos"),
    not(target_os = "ios")
))]
pub(super) fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let _ = (from, to);
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-clobber rename is unsupported on this Unix platform",
    ))
}

#[cfg(not(unix))]
pub(super) fn rename_no_replace(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-clobber bootstrap publication requires Unix",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_mac_detects_payload_and_tag_tampering() {
        let key = [7u8; KEY_BYTES];
        let original = hmac_hex(&key, b"payload");
        assert!(constant_time_eq(
            original.as_bytes(),
            hmac_hex(&key, b"payload").as_bytes()
        ));
        assert!(!constant_time_eq(
            original.as_bytes(),
            hmac_hex(&key, b"tampered").as_bytes()
        ));
        let mut tag = original.into_bytes();
        tag[0] ^= 1;
        assert!(!constant_time_eq(
            &tag,
            hmac_hex(&key, b"payload").as_bytes()
        ));
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231_case_one() {
        assert_eq!(
            hmac_hex(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_key_publication_is_atomic_single_link_and_no_clobber() {
        use std::os::unix::fs::MetadataExt;

        let requested_root =
            std::env::temp_dir().join(format!("mnemed-bootstrap-key-{}", Ulid::new()));
        fs::create_dir(&requested_root).unwrap();
        let root = fs::canonicalize(&requested_root).unwrap();
        let key = root.join("native-bootstrap.key");
        publish_private_bytes_no_clobber(&key, &[7; KEY_BYTES]).unwrap();
        assert_eq!(fs::symlink_metadata(&key).unwrap().nlink(), 1);
        assert_eq!(load_existing_key(&key).unwrap(), [7; KEY_BYTES]);

        assert!(publish_private_bytes_no_clobber(&key, &[9; KEY_BYTES]).is_err());
        assert_eq!(load_existing_key(&key).unwrap(), [7; KEY_BYTES]);
        assert_eq!(fs::symlink_metadata(&key).unwrap().nlink(), 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_lock_rejects_symlinks_and_hardlinks_without_touching_victim() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = std::env::temp_dir().join(format!("mnemed-bootstrap-lock-{}", Ulid::new()));
        fs::create_dir(&root).unwrap();
        let victim = root.join("victim");
        fs::write(&victim, b"do-not-touch").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
        let lock = root.join("lock");

        symlink(&victim, &lock).unwrap();
        assert!(
            ExclusiveLock::acquire(&lock, "bootstrap lock", "another bootstrap-create").is_err()
        );
        assert_eq!(fs::read(&victim).unwrap(), b"do-not-touch");
        fs::remove_file(&lock).unwrap();

        fs::hard_link(&victim, &lock).unwrap();
        assert!(
            ExclusiveLock::acquire(&lock, "bootstrap lock", "another bootstrap-create").is_err()
        );
        assert_eq!(fs::read(&victim).unwrap(), b"do-not-touch");

        fs::remove_dir_all(root).unwrap();
    }
}
