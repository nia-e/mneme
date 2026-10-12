//! Windows filesystem bodies use the same checked namespace as store leases.
//!
//! A root retains its admitted directory handle. Child operations are relative
//! to that handle, never an absolute path reconstructed after admission. New
//! bodies become visible under their final names only after a complete write
//! and file flush; publication is no-clobber and followed by a directory flush.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use mneme_core::BodyRef;
use mneme_core::ports::{Error, Result};
use mneme_store_path::windows::{
    Directory, file_identity, rename_no_replace, validate_absolute_path, validate_leaf,
};
use ulid::Ulid;

pub(super) struct Root {
    directory: Directory,
}

fn body_error(operation: &str, path: &Path, error: io::Error) -> Error {
    Error::Body(format!("{operation} {path:?}: {error}"))
}

impl Root {
    pub(super) fn open_existing(path: &Path) -> Result<Self> {
        let directory = Directory::open(path)
            .map_err(|error| body_error("admit existing body root", path, error))?;
        Ok(Self { directory })
    }

    pub(super) fn create_or_open(path: &Path) -> Result<Self> {
        if path.as_os_str().is_empty() {
            return Err(Error::Body("body root must not be empty".into()));
        }
        // Let the shared parser anchor ordinary relative paths. Calling
        // std::path::absolute first would resolve drive-relative spellings via
        // hidden per-drive working directories before they could be refused.
        let directory = create_or_open_directory(path)
            .map_err(|error| body_error("create or admit body root", path, error))?;
        Ok(Self { directory })
    }

    pub(super) fn open_file(&self, body: &BodyRef) -> Result<File> {
        self.open_checked(body)
            .map_err(|error| body_error("open body", self.directory.path(), error))
    }

    fn open_checked(&self, body: &BodyRef) -> io::Result<File> {
        self.directory.require_named()?;
        let name = self.body_name(body)?;
        let file = self.directory.open_file(&name, false)?;
        self.directory.require_named()?;
        Ok(file)
    }

    /// Absolute legacy references must name an immediate child of the exact
    /// admitted root. New references contain only one opaque filename. Nested
    /// borrowed paths were never produced by this store and are not admitted on
    /// Windows; Unix retains its existing descendant-path compatibility.
    fn body_name(&self, body: &BodyRef) -> io::Result<OsString> {
        let raw = body.as_str().strip_prefix("fs://").ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "body reference is not fs://")
        })?;
        let raw_path = Path::new(raw);
        if !raw_path.is_absolute() {
            let name = OsStr::new(raw);
            // This is not a path join: a leaf cannot redirect the directory
            // handle via drive-relative/root-relative paths, ADS, or dot names.
            validate_leaf(name)?;
            return Ok(name.to_os_string());
        }

        // Validate the original spelling before parent()/file_name() can erase
        // dot segments or trailing separators. The shared admission grammar
        // excludes network and device namespaces as well as reparse traversal.
        validate_absolute_path(raw_path)?;
        let parent_path = raw_path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "body has no parent directory")
        })?;
        let name = raw_path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "body has no filename"))?;
        validate_leaf(name)?;
        let parent = Directory::open(parent_path)?;
        if !parent.identity().same_object(self.directory.identity()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fs body escapes its configured root or is not an immediate child",
            ));
        }
        Ok(name.to_os_string())
    }

    pub(super) fn put(&self, bytes: &[u8]) -> Result<BodyRef> {
        self.put_named(&Ulid::new().to_string(), bytes)
    }

    fn put_named(&self, name: &str, bytes: &[u8]) -> Result<BodyRef> {
        let body = BodyRef::new(format!("fs://{name}"))
            .map_err(|error| Error::Body(format!("mint fs body reference: {error}")))?;
        let name = OsStr::new(name);
        let temporary = OsString::from(format!(".mneme-body-{}.tmp", Ulid::new()));
        self.directory
            .require_named()
            .map_err(|error| body_error("check body root", self.directory.path(), error))?;
        let mut file = self
            .directory
            .create_file(&temporary)
            .map_err(|error| body_error("stage body", self.directory.path(), error))?;
        let written = (|| -> io::Result<()> {
            #[cfg(test)]
            tests::publication_cut(tests::PublicationCut::BeforeWrite)?;
            file.write_all(bytes)?;
            #[cfg(test)]
            tests::publication_cut(tests::PublicationCut::AfterWrite)?;
            file.sync_all()?;
            #[cfg(test)]
            tests::publication_cut(tests::PublicationCut::AfterFileSync)?;
            let expected = file_identity(&file)?;
            self.directory.require_named()?;
            #[cfg(test)]
            tests::publication_cut(tests::PublicationCut::BeforeRename)?;
            rename_no_replace(&file, &self.directory, name)?;
            #[cfg(test)]
            tests::publication_cut(tests::PublicationCut::AfterRename)?;
            self.directory.sync_all()?;
            #[cfg(test)]
            tests::publication_cut(tests::PublicationCut::AfterDirectorySync)?;
            let published = self.directory.open_file(name, false)?;
            if !file_identity(&published)?.same_file(expected)
                || published.metadata()?.len() != bytes.len() as u64
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "published body identity or length changed",
                ));
            }
            #[cfg(test)]
            tests::publication_cut(tests::PublicationCut::AfterReadback)?;
            self.directory.require_named()
        })();
        if let Err(error) = written {
            // Cleanup can remove only our still-named temporary inode. After
            // publication it is absent; an ambiguous final body is preserved,
            // never rolled back by deleting an unchecked final pathname.
            if let Ok(identity) = file_identity(&file) {
                let _ = self.directory.remove_file(&temporary, identity);
            }
            return Err(body_error("publish body", self.directory.path(), error));
        }
        Ok(body)
    }

    pub(super) fn delete(&self, body: &BodyRef) -> Result<()> {
        let removed = (|| -> io::Result<()> {
            self.directory.require_named()?;
            let name = self.body_name(body)?;
            let file = match self.directory.open_file(&name, false) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            let identity = file_identity(&file)?;
            self.directory.remove_file(&name, identity)?;
            drop(file);
            self.directory.sync_all()?;
            self.directory.require_named()
        })();
        removed.map_err(|error| body_error("delete body", self.directory.path(), error))
    }
}

/// Unlike existing-only admission, the creating constructor may make missing
/// ancestors. Every new directory receives the shared private ACL at creation;
/// an existing but inadmissible directory is not repaired or adopted silently.
fn create_or_open_directory(path: &Path) -> io::Result<Directory> {
    match Directory::open(path) {
        Ok(directory) => Ok(directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match Directory::create(path) {
                Ok(directory) => return Ok(directory),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Directory::open(path);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let parent_path = path.parent().ok_or(error)?;
            let parent = create_or_open_directory(parent_path)?;
            let name = path.file_name().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "body root has no directory name",
                )
            })?;
            let directory = match parent.create_directory(name) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    Directory::open(path)?
                }
                Err(error) => return Err(error),
            };
            directory.sync_all()?;
            parent.sync_all()?;
            parent.require_named()?;
            Ok(directory)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests;
