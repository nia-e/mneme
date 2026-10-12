//! Windows activation selectors are protected regular files, not symlinks.
//!
//! This is an additive Windows-only format. Unix keeps its existing relative
//! symlink selector. Reads are bounded and retain both content and file identity
//! while the resolver validates the selected generation.

use std::fs::{self, File};
use std::io;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};

use ulid::Ulid;

use super::{
    Directory, FILE_OPEN, FILE_READ_ATTRIBUTES, FileIdentity, READ_CONTROL, SYNCHRONIZE,
    absolute_disk_path, directory_identity, file_identity, open_absolute_directory, open_child,
};

const DOMAIN: &str = "mneme.windows-selector.v1\n";
const MAX_SELECTOR_BYTES: usize = 256;

/// Encode one Windows selector. Publication must use a protected, no-clobber
/// regular file and durably sync it and its parent before claiming activation.
pub fn encode_activation_selector(operation_id: Ulid) -> Vec<u8> {
    format!("{DOMAIN}generations/{operation_id}\n").into_bytes()
}

/// Read the bounded Windows-only selector without following reparse points.
/// Returns the validated relative `generations/<ULID>` target, not a database.
pub fn read_activation_selector(path: &Path) -> io::Result<PathBuf> {
    let selector = ActivationSelector::open(path)?;
    Ok(selector.target.clone())
}

/// A retained, parsed Windows selector bound to its exact file identity and bytes.
///
/// Keep this snapshot while validating the selected generation. Before cleanup,
/// call [`Self::require_unchanged`] and pass [`Self::identity`] to identity-bound
/// deletion; do not capture a fresh, unvalidated file identity after parsing.
#[derive(Debug)]
pub struct ActivationSelector {
    parent: Directory,
    file: File,
    identity: FileIdentity,
    path: PathBuf,
    bytes: Vec<u8>,
    target: PathBuf,
}

impl ActivationSelector {
    /// Open, parse and bind a protected selector without following reparse points.
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = absolute_disk_path(path)?;
        let parent = Directory::open_parent(
            path.parent()
                .ok_or_else(|| invalid("selector has no parent"))?,
        )?;
        let name = path
            .file_name()
            .ok_or_else(|| invalid("selector has no filename"))?;
        let file = parent.open_file(name, false)?;
        let identity = file_identity(&file)?;
        let bytes = read_bounded(&file)?;
        let target = parse_target(&bytes)?;
        let selector = Self {
            parent,
            file,
            identity,
            path,
            bytes,
            target,
        };
        selector.require_unchanged()?;
        Ok(selector)
    }

    /// The validated relative `generations/<ULID>` target from this snapshot.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Original file identity whose bytes were parsed, not a later pathname read.
    pub fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Revalidate the retained and named file, original bytes and parent binding.
    pub fn require_unchanged(&self) -> io::Result<()> {
        self.parent.require_named()?;
        let name = self
            .path
            .file_name()
            .ok_or_else(|| invalid("selector has no filename"))?;
        let named = self.parent.open_file(name, false)?;
        if file_identity(&self.file)? != self.identity
            || file_identity(&named)? != self.identity
            || read_bounded(&self.file)? != self.bytes
            || read_bounded(&named)? != self.bytes
            || file_identity(&self.file)? != self.identity
            || file_identity(&named)? != self.identity
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "mneme activation {} changed while resolving",
                    self.path.display()
                ),
            ));
        }
        // The named handle retained above could itself have been replaced
        // during content validation. Reopen once at the final identity cut.
        let current = self.parent.open_file(name, false)?;
        if file_identity(&current)? != self.identity {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "mneme activation changed identity while resolving",
            ));
        }
        self.parent.require_named()
    }
}

fn read_bounded(file: &File) -> io::Result<Vec<u8>> {
    let len = file_identity(file)?.len;
    if len == 0 || len > MAX_SELECTOR_BYTES as u64 {
        return Err(invalid(
            "Windows activation selector must contain 1..=256 bytes",
        ));
    }
    // Read one extra byte so growth during the read is not silently truncated.
    let mut bytes = vec![0; MAX_SELECTOR_BYTES + 1];
    let mut count = 0;
    while count < bytes.len() {
        let read = file.seek_read(&mut bytes[count..], count as u64)?;
        if read == 0 {
            break;
        }
        count += read;
    }
    if count != len as usize || count > MAX_SELECTOR_BYTES {
        return Err(invalid(
            "Windows activation selector changed length while reading",
        ));
    }
    bytes.truncate(count);
    Ok(bytes)
}

fn parse_target(bytes: &[u8]) -> io::Result<PathBuf> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid("Windows activation selector is not UTF-8"))?;
    let id = text.strip_prefix(DOMAIN)
        .and_then(|rest| rest.strip_prefix("generations/"))
        .and_then(|rest| rest.strip_suffix('\n'))
        .filter(|id| id.len() == 26)
        .ok_or_else(|| invalid("Windows activation must be mneme.windows-selector.v1 followed by generations/<ULID> and one final newline"))?;
    Ulid::from_string(id)
        .map_err(|_| invalid("Windows activation selector has an invalid generation ULID"))?;
    // The path is built only after strict byte-format and ULID validation: no
    // separator normalization, CRLF conversion, ADS or dot-path interpretation.
    Ok(Path::new("generations").join(id))
}

/// Verify every existing component before canonicalization can hide a reparse
/// alias. Directory ancestors need not be user-owned (e.g. C:\\Users), but every
/// component must be a plain local NTFS object. This is provenance inspection;
/// admission still performs the stronger owner/ACL checks separately.
pub(crate) fn require_plain_existing_path(path: &Path) -> io::Result<()> {
    let path = absolute_disk_path(path)?;
    if fs::symlink_metadata(&path)?.is_dir() {
        let directory = open_absolute_directory(&path, false)?;
        directory_identity(&directory)?;
    } else {
        let parent = open_absolute_directory(
            path.parent().ok_or_else(|| invalid("path has no parent"))?,
            false,
        )?;
        let file = open_child(
            &parent,
            path.file_name()
                .ok_or_else(|| invalid("path has no filename"))?,
            FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE,
            FILE_OPEN,
            false,
            None,
        )?;
        file_identity(&file)?;
    }
    Ok(())
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("mneme-windows-selector-{}", Ulid::new()));
            drop(Directory::create(&path).unwrap());
            Self(fs::canonicalize(path).unwrap())
        }
        fn write(&self, bytes: &[u8]) -> PathBuf {
            let directory = Directory::open(&self.0).unwrap();
            let mut file = directory
                .create_file(std::ffi::OsStr::new("current"))
                .unwrap();
            file.write_all(bytes).unwrap();
            file.sync_all().unwrap();
            self.0.join("current")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn exact_format_round_trips_and_rejects_other_platform_or_ambiguous_bytes() {
        let id = Ulid::new();
        let valid = encode_activation_selector(id);
        assert_eq!(
            valid,
            format!("mneme.windows-selector.v1\ngenerations/{id}\n").as_bytes()
        );
        assert_eq!(
            parse_target(&valid).unwrap(),
            Path::new("generations").join(id.to_string())
        );
        for invalid_bytes in [
            b"".to_vec(),
            format!("generations/{id}").into_bytes(),
            format!("mneme.windows-selector.v2\ngenerations/{id}\n").into_bytes(),
            format!("{DOMAIN}generations\\{id}\n").into_bytes(),
            format!("{DOMAIN}generations/{id}\r\n").into_bytes(),
            format!("{DOMAIN}generations/{id}\n\n").into_bytes(),
            format!("{DOMAIN}../generations/{id}\n").into_bytes(),
            format!("{DOMAIN}generations/{id}:stream\n").into_bytes(),
            vec![255],
            vec![b'x'; 257],
        ] {
            assert!(parse_target(&invalid_bytes).is_err());
        }
        // Match Unix's accepted ULID domain; nil is a valid identifier here.
        assert!(parse_target(&encode_activation_selector(Ulid::nil())).is_ok());
    }

    #[test]
    fn reader_is_private_bounded_and_rejects_hardlinks() {
        let scratch = Scratch::new();
        let id = Ulid::new();
        let path = scratch.write(&encode_activation_selector(id));
        assert_eq!(
            read_activation_selector(&path).unwrap(),
            Path::new("generations").join(id.to_string())
        );
        fs::hard_link(&path, scratch.0.join("alias")).unwrap();
        assert!(read_activation_selector(&path).is_err());
        let other = Scratch::new();
        assert!(read_activation_selector(&other.write(&vec![b'x'; 257])).is_err());
    }

    #[test]
    fn snapshot_rejects_same_bytes_on_a_different_file_identity() {
        let scratch = Scratch::new();
        let bytes = encode_activation_selector(Ulid::new());
        let path = scratch.write(&bytes);
        let snapshot = ActivationSelector::open(&path).unwrap();
        fs::rename(&path, scratch.0.join("old-current")).unwrap();
        scratch.write(&bytes);
        assert_eq!(
            snapshot.require_unchanged().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn snapshot_rejects_same_inode_same_length_content_rewrite() {
        let scratch = Scratch::new();
        let path = scratch.write(&encode_activation_selector(Ulid::from(1_u128)));
        let snapshot = ActivationSelector::open(&path).unwrap();
        let original_identity = snapshot.identity;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.write_all(&encode_activation_selector(Ulid::from(2_u128)))
            .unwrap();
        file.sync_all().unwrap();
        assert_eq!(file_identity(&file).unwrap(), original_identity);
        assert_eq!(
            snapshot.require_unchanged().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn mixed_case_generation_local_configuration_is_still_bootstrap_owned() {
        let path = Path::new(r"C:\example\.MNEME\GeNeRaTiOnS\00000000000000000000000001\memory.db");
        let error = crate::resolve_configured_store_path(path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("generation-local"));
    }

    #[test]
    fn selector_resolves_only_complete_generations_without_conventional_fallback() {
        let scratch = Scratch::new();
        let id = Ulid::new();
        let root = Directory::open(&scratch.0).unwrap();
        let generations = root
            .create_directory(std::ffi::OsStr::new("generations"))
            .unwrap();
        let generation = generations
            .create_directory(std::ffi::OsStr::new(&id.to_string()))
            .unwrap();
        let mut main = generation
            .create_database_file(std::ffi::OsStr::new("memory.db"))
            .unwrap();
        main.write_all(b"filesystem-only fixture").unwrap();
        let conventional = scratch.0.join("memory.db");
        assert!(crate::resolve_configured_store_path(&conventional).is_err());
        scratch.write(&encode_activation_selector(id));
        let expected = fs::canonicalize(generation.path().join("memory.db")).unwrap();
        assert_eq!(
            crate::resolve_configured_store_path(&conventional).unwrap(),
            expected
        );
        assert_eq!(
            crate::resolve_configured_store_path(&scratch.0.join("MEMORY.DB")).unwrap(),
            expected
        );
        let mut competitor = root
            .create_database_file(std::ffi::OsStr::new("memory.db"))
            .unwrap();
        competitor.write_all(b"ambiguous").unwrap();
        assert!(crate::resolve_configured_store_path(&conventional).is_err());
    }
}
