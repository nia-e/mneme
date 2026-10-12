//! Detached, immutable snapshot bundle construction. The caller owns the live
//! database lease and has quiesced mutations before any of these steps run.

use std::collections::BTreeSet;
#[cfg(any(not(windows), test))]
use std::fs;
use std::fs::File;
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use mneme_core::EmbeddingFingerprint;
use serde::Serialize;
use sha2::{Digest, Sha256};
use ulid::Ulid;

use crate::host::AnyErr;

/// Snapshot durability cuts. `boundary` is an optimized no-op outside tests;
/// the test binary alone can terminate a child at an exact cut.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SnapshotBoundary {
    AfterSourceClose,
    BeforePayloadWrite,
    AfterPayloadWrite,
    BeforePayloadSync,
    AfterPayloadSync,
    AfterPayloadCopy,
    AfterSourceReopen,
    BeforeVerify,
    AfterVerify,
    BeforeManifestWrite,
    AfterManifestWrite,
    BeforeManifestSync,
    AfterManifestSync,
    BeforeRename,
    AfterRename,
    AfterTargetSync,
    BeforeParentSync,
    AfterParentSync,
}

pub(crate) fn boundary(cut: SnapshotBoundary) {
    #[cfg(test)]
    {
        std::thread_local! {
            static MATCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        }
        if let Ok(selected) = std::env::var("MNEME_SNAPSHOT_TEST_CRASH_CUT") {
            let (name, ordinal) = selected.split_once('@').unwrap_or((&selected, "1"));
            if name == cut.name() {
                let wanted = ordinal.parse::<usize>().unwrap_or(1);
                MATCHES.with(|matches| {
                    let count = matches.get() + 1;
                    matches.set(count);
                    if count == wanted {
                        std::process::exit(89);
                    }
                });
            }
        }
    }
    #[cfg(not(test))]
    let _ = cut;
}

#[cfg(test)]
impl SnapshotBoundary {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::AfterSourceClose => "after_source_close",
            Self::BeforePayloadWrite => "before_payload_write",
            Self::AfterPayloadWrite => "after_payload_write",
            Self::BeforePayloadSync => "before_payload_sync",
            Self::AfterPayloadSync => "after_payload_sync",
            Self::AfterPayloadCopy => "after_payload_copy",
            Self::AfterSourceReopen => "after_source_reopen",
            Self::BeforeVerify => "before_verify",
            Self::AfterVerify => "after_verify",
            Self::BeforeManifestWrite => "before_manifest_write",
            Self::AfterManifestWrite => "after_manifest_write",
            Self::BeforeManifestSync => "before_manifest_sync",
            Self::AfterManifestSync => "after_manifest_sync",
            Self::BeforeRename => "before_rename",
            Self::AfterRename => "after_rename",
            Self::AfterTargetSync => "after_target_sync",
            Self::BeforeParentSync => "before_parent_sync",
            Self::AfterParentSync => "after_parent_sync",
        }
    }
}

#[derive(Serialize)]
struct FileRecord {
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Serialize)]
struct Manifest {
    schema: &'static str,
    db_id: Ulid,
    generation: Ulid,
    captured_at: u64,
    embedding_fingerprint: EmbeddingFingerprint,
    files: Vec<FileRecord>,
}

pub struct PendingSnapshot {
    source: PathBuf,
    body_names: BTreeSet<String>,
    root: PathBuf,
    #[cfg(not(windows))]
    stage: PathBuf,
    manifest: Manifest,
    total_size: u64,
    #[cfg(windows)]
    namespace: WindowsSnapshotNamespace,
}

pub struct CopiedSnapshot(PendingSnapshot);

impl Drop for PendingSnapshot {
    fn drop(&mut self) {
        // Only this operation's private stage, never a published generation.
        // Process death can still leave residue; it is never selected as a
        // snapshot and a later operator may inspect/remove it deliberately.
        #[cfg(not(windows))]
        {
            if fs::symlink_metadata(&self.stage).is_ok_and(|metadata| metadata.file_type().is_dir())
            {
                let _ = fs::remove_dir_all(&self.stage);
            }
        }
        #[cfg(windows)]
        self.namespace.cleanup();
    }
}

impl PendingSnapshot {
    pub fn new(
        source: &Path,
        db_id: Ulid,
        generation: Ulid,
        fingerprint: EmbeddingFingerprint,
        refs: Vec<String>,
    ) -> Result<Self, AnyErr> {
        let mut body_names = BTreeSet::new();
        for body in refs {
            let name = body
                .strip_prefix("fs://")
                .ok_or_else(|| format!("snapshot cannot carry non-fs body ref {body}"))?;
            let mut parts = Path::new(name).components();
            if !matches!(parts.next(), Some(Component::Normal(_)))
                || parts.next().is_some()
                || name == "."
                || name == ".."
            {
                return Err(
                    format!("snapshot requires one relative fs body filename, got {body}").into(),
                );
            }
            body_names.insert(name.to_owned());
        }
        if body_names.len() >= 100_000 {
            return Err("snapshot file inventory exceeds its 100000-file limit".into());
        }
        let root = source
            .parent()
            .ok_or("source database has no parent")?
            .join("snapshots");
        #[cfg(not(windows))]
        if root.exists() {
            let metadata = fs::symlink_metadata(&root)?;
            if !metadata.file_type().is_dir() {
                return Err(
                    format!("snapshot root {} is not a real directory", root.display()).into(),
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.permissions().mode() & 0o7777 != 0o700
                {
                    return Err("snapshot root must be current-owner mode 0700".into());
                }
            }
        } else {
            create_private_dir(&root)?;
            sync_parent(&root)?;
        }
        #[cfg(not(windows))]
        let stage = root.join(format!(".stage-{generation}"));
        #[cfg(not(windows))]
        create_private_dir(&stage)?;
        #[cfg(windows)]
        let namespace = WindowsSnapshotNamespace::new(source, generation)?;
        Ok(Self {
            source: source.to_path_buf(),
            body_names,
            root,
            #[cfg(not(windows))]
            stage,
            manifest: Manifest {
                schema: "mneme.snapshot.v1",
                db_id,
                generation,
                captured_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                embedding_fingerprint: fingerprint,
                files: Vec::new(),
            },
            total_size: 0,
            #[cfg(windows)]
            namespace,
        })
    }

    /// Called only after the source backend has been checkpointed and closed.
    pub fn copy_source(mut self) -> Result<CopiedSnapshot, AnyErr> {
        self.copy_file(&self.source.clone(), "database.db".to_owned())?;
        if !self.body_names.is_empty() {
            let body_dir = self.source.with_extension("bodies");
            #[cfg(not(windows))]
            {
                let metadata = fs::symlink_metadata(&body_dir)?;
                if !metadata.file_type().is_dir() {
                    return Err("source body directory is not a real directory".into());
                }
                let target = self.stage.join("bodies");
                create_private_dir(&target)?;
            }
            #[cfg(windows)]
            self.namespace.prepare_bodies()?;
            for name in self.body_names.clone() {
                self.copy_file(&body_dir.join(&name), format!("bodies/{name}"))?;
            }
            #[cfg(not(windows))]
            File::open(self.stage.join("bodies"))?.sync_all()?;
            #[cfg(windows)]
            self.namespace.sync_bodies()?;
        }
        #[cfg(not(windows))]
        File::open(&self.stage)?.sync_all()?;
        #[cfg(windows)]
        self.namespace.sync_stage()?;
        Ok(CopiedSnapshot(self))
    }

    fn copy_file(&mut self, source: &Path, relative: String) -> Result<(), AnyErr> {
        #[cfg(not(windows))]
        let mut input = read_nofollow(source)?;
        #[cfg(windows)]
        let mut input = self.namespace.open_source(&relative)?;
        #[cfg(windows)]
        let source_identity = mneme_store_path::windows::file_identity(&input)?;
        let metadata = input.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(format!("snapshot source {} is not regular", source.display()).into());
        }
        const MAX_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;
        if metadata.len() > MAX_BUNDLE_BYTES {
            return Err(format!(
                "snapshot source {} exceeds 512 MiB file limit",
                source.display()
            )
            .into());
        }
        let next_total = self
            .total_size
            .checked_add(metadata.len())
            .ok_or("snapshot bundle size overflow")?;
        if next_total > MAX_BUNDLE_BYTES {
            return Err("snapshot bundle exceeds its 512 MiB limit".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return Err("snapshot source has multiple hard links".into());
            }
        }
        #[cfg(not(windows))]
        let mut output = create_private_file(&self.stage.join(&relative))?;
        #[cfg(windows)]
        let mut output = self.namespace.create_output(&relative)?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        boundary(SnapshotBoundary::BeforePayloadWrite);
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            if n as u64 > metadata.len().saturating_sub(size) {
                return Err("snapshot source grew during copy".into());
            }
            output.write_all(&buffer[..n])?;
            hash.update(&buffer[..n]);
            size = size
                .checked_add(n as u64)
                .ok_or("snapshot file too large")?;
        }
        boundary(SnapshotBoundary::AfterPayloadWrite);
        boundary(SnapshotBoundary::BeforePayloadSync);
        output.sync_all()?;
        boundary(SnapshotBoundary::AfterPayloadSync);
        if input.metadata()?.len() != metadata.len() || size != metadata.len() {
            return Err("snapshot source changed during copy".into());
        }
        #[cfg(windows)]
        self.namespace.require_source(&relative, source_identity)?;
        self.manifest.files.push(FileRecord {
            path: relative,
            size,
            sha256: format!("{:x}", hash.finalize()),
        });
        self.total_size = next_total;
        Ok(())
    }
}

impl CopiedSnapshot {
    /// Verify copied bytes only after the normal source handle has reopened.
    pub fn verify_and_publish(self) -> Result<PathBuf, AnyErr> {
        self.verify_and_publish_with_post_rename(|| Ok(()))
    }

    pub(crate) fn verify_and_publish_with_post_rename(
        self,
        after_rename: impl FnOnce() -> Result<(), AnyErr>,
    ) -> Result<PathBuf, AnyErr> {
        #[allow(unused_mut)]
        let mut pending = self.0;
        boundary(SnapshotBoundary::BeforeVerify);
        for record in &pending.manifest.files {
            #[cfg(not(windows))]
            let mut file = read_nofollow(&pending.stage.join(&record.path))?;
            #[cfg(windows)]
            let mut file = pending.namespace.open_output(&record.path)?;
            let mut hash = Sha256::new();
            let mut size = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                if n as u64 > record.size.saturating_sub(size) {
                    return Err("snapshot staged file grew during verification".into());
                }
                hash.update(&buffer[..n]);
                size = size
                    .checked_add(n as u64)
                    .ok_or("snapshot verification size overflow")?;
            }
            if size != record.size || format!("{:x}", hash.finalize()) != record.sha256 {
                return Err(format!(
                    "snapshot staged file {} failed hash verification",
                    record.path
                )
                .into());
            }
        }
        boundary(SnapshotBoundary::AfterVerify);
        #[cfg(not(windows))]
        let mut manifest = create_private_file(&pending.stage.join("manifest.json"))?;
        #[cfg(windows)]
        let mut manifest = pending.namespace.create_output("manifest.json")?;
        boundary(SnapshotBoundary::BeforeManifestWrite);
        manifest.write_all(&serde_json::to_vec_pretty(&pending.manifest)?)?;
        boundary(SnapshotBoundary::AfterManifestWrite);
        boundary(SnapshotBoundary::BeforeManifestSync);
        manifest.sync_all()?;
        boundary(SnapshotBoundary::AfterManifestSync);
        #[cfg(not(windows))]
        File::open(&pending.stage)?.sync_all()?;
        #[cfg(windows)]
        pending.namespace.sync_stage()?;
        let published = pending.root.join(pending.manifest.generation.to_string());
        boundary(SnapshotBoundary::BeforeRename);
        #[cfg(not(windows))]
        rename_no_replace(&pending.stage, &published)?;
        #[cfg(windows)]
        pending.namespace.publish(pending.manifest.generation)?;
        boundary(SnapshotBoundary::AfterRename);
        // Once crossed, failure is ambiguous to the caller. The complete
        // target remains in place for inspection/retry; Drop sees no stage.
        after_rename()?;
        #[cfg(not(windows))]
        File::open(&published)?.sync_all()?;
        #[cfg(windows)]
        pending.namespace.sync_stage()?;
        boundary(SnapshotBoundary::AfterTargetSync);
        boundary(SnapshotBoundary::BeforeParentSync);
        #[cfg(not(windows))]
        File::open(&pending.root)?.sync_all()?;
        #[cfg(windows)]
        {
            pending.namespace.root.require_named()?;
            pending.namespace.root.sync_all()?;
        }
        boundary(SnapshotBoundary::AfterParentSync);
        Ok(published)
    }
}

#[cfg(not(windows))]
fn read_nofollow(path: &Path) -> Result<File, AnyErr> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    Ok(options.open(path)?)
}

#[cfg(not(windows))]
fn create_private_file(path: &Path) -> Result<File, AnyErr> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

#[cfg(not(windows))]
fn create_private_dir(path: &Path) -> Result<(), AnyErr> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    Ok(())
}

#[cfg(all(windows, test))]
fn create_private_dir(path: &Path) -> Result<(), AnyErr> {
    mneme_store_path::windows::Directory::create(path)?;
    Ok(())
}

#[cfg(not(windows))]
fn sync_parent(path: &Path) -> Result<(), AnyErr> {
    File::open(path.parent().ok_or("path has no parent")?)?.sync_all()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn rename_no_replace(from: &Path, to: &Path) -> Result<(), AnyErr> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let source = CString::new(from.as_os_str().as_bytes())?;
    let target = CString::new(to.as_os_str().as_bytes())?;
    let rc = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rename_no_replace(from: &Path, to: &Path) -> Result<(), AnyErr> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let source = CString::new(from.as_os_str().as_bytes())?;
    let target = CString::new(to.as_os_str().as_bytes())?;
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn rename_no_replace(_: &Path, _: &Path) -> Result<(), AnyErr> {
    Err("snapshot publication is unsupported on this platform".into())
}

/// Retain the exact Windows namespace throughout copying and publication. No
/// operation follows a reparse point or reopens an unchecked pathname. Cleanup
/// has a closed created-object inventory, rather than recursively deleting
/// whatever happens to occupy the staging pathname at drop time.
#[cfg(windows)]
struct WindowsSnapshotNamespace {
    source_parent: mneme_store_path::windows::Directory,
    source_name: std::ffi::OsString,
    source_body_name: std::ffi::OsString,
    source_bodies: Option<mneme_store_path::windows::Directory>,
    root: mneme_store_path::windows::Directory,
    stage: mneme_store_path::windows::Directory,
    bodies: Option<mneme_store_path::windows::Directory>,
    files: std::collections::BTreeMap<String, mneme_store_path::windows::FileIdentity>,
    published: Option<std::ffi::OsString>,
}

#[cfg(windows)]
impl WindowsSnapshotNamespace {
    fn new(source: &Path, generation: Ulid) -> Result<Self, AnyErr> {
        use mneme_store_path::windows::Directory;
        use std::ffi::OsStr;

        let source_parent =
            Directory::open_parent(source.parent().ok_or("source database has no parent")?)?;
        let source_name = source
            .file_name()
            .ok_or("source database has no name")?
            .to_owned();
        let source_body_name = source
            .with_extension("bodies")
            .file_name()
            .ok_or("source body directory has no name")?
            .to_owned();
        let root = match source_parent.open_directory(OsStr::new("snapshots")) {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                source_parent.create_directory(OsStr::new("snapshots"))?
            }
            Err(error) => return Err(error.into()),
        };
        root.require_named()?;
        let stage = root.create_directory(OsStr::new(&format!(".stage-{generation}")))?;
        Ok(Self {
            source_parent,
            source_name,
            source_body_name,
            source_bodies: None,
            root,
            stage,
            bodies: None,
            files: std::collections::BTreeMap::new(),
            published: None,
        })
    }

    fn prepare_bodies(&mut self) -> Result<(), AnyErr> {
        self.source_parent.require_named()?;
        self.source_bodies = Some(self.source_parent.open_directory(&self.source_body_name)?);
        self.bodies = Some(
            self.stage
                .create_directory(std::ffi::OsStr::new("bodies"))?,
        );
        Ok(())
    }

    fn open_source(&self, relative: &str) -> Result<File, AnyErr> {
        self.source_parent.require_named()?;
        if relative == "database.db" {
            return Ok(self.source_parent.open_file(&self.source_name, false)?);
        }
        let name = relative
            .strip_prefix("bodies/")
            .ok_or("unknown snapshot source")?;
        let directory = self
            .source_bodies
            .as_ref()
            .ok_or("source bodies are not open")?;
        directory.require_named()?;
        Ok(directory.open_file(std::ffi::OsStr::new(name), false)?)
    }

    fn require_source(
        &self,
        relative: &str,
        expected: mneme_store_path::windows::FileIdentity,
    ) -> Result<(), AnyErr> {
        let named = self.open_source(relative)?;
        if mneme_store_path::windows::file_identity(&named)? != expected {
            return Err("snapshot source changed identity during copy".into());
        }
        Ok(())
    }

    fn output_parent<'a>(
        &'a self,
        relative: &'a str,
    ) -> Result<
        (
            &'a mneme_store_path::windows::Directory,
            &'a std::ffi::OsStr,
        ),
        AnyErr,
    > {
        self.root.require_named()?;
        self.stage.require_named()?;
        if let Some(name) = relative.strip_prefix("bodies/") {
            let directory = self.bodies.as_ref().ok_or("staged bodies are not open")?;
            directory.require_named()?;
            Ok((directory, std::ffi::OsStr::new(name)))
        } else {
            Ok((&self.stage, std::ffi::OsStr::new(relative)))
        }
    }

    fn create_output(&mut self, relative: &str) -> Result<File, AnyErr> {
        let (parent, name) = self.output_parent(relative)?;
        let file = parent.create_file(name)?;
        self.files.insert(
            relative.to_owned(),
            mneme_store_path::windows::file_identity(&file)?,
        );
        Ok(file)
    }

    fn open_output(&self, relative: &str) -> Result<File, AnyErr> {
        let expected = self
            .files
            .get(relative)
            .ok_or("unrecognized staged snapshot file")?;
        let (parent, name) = self.output_parent(relative)?;
        let file = parent.open_file(name, false)?;
        if !mneme_store_path::windows::file_identity(&file)?.same_file(*expected) {
            return Err("snapshot staged file changed identity".into());
        }
        Ok(file)
    }

    fn sync_bodies(&self) -> Result<(), AnyErr> {
        let directory = self.bodies.as_ref().ok_or("staged bodies are not open")?;
        directory.require_named()?;
        directory.sync_all()?;
        Ok(())
    }

    fn sync_stage(&self) -> Result<(), AnyErr> {
        self.root.require_named()?;
        if let Some(name) = &self.published {
            let target = self.root.open_directory(name)?;
            if !target.identity().same_object(self.stage.identity()) {
                return Err("published snapshot changed identity before final sync".into());
            }
            target.require_named()?;
            target.sync_all()?;
        } else {
            self.stage.require_named()?;
            self.stage.sync_all()?;
        }
        Ok(())
    }

    fn publish(&mut self, generation: Ulid) -> Result<(), AnyErr> {
        let name = std::ffi::OsString::from(generation.to_string());
        mneme_store_path::windows::rename_directory_no_replace(&self.stage, &self.root, &name)?;
        // No fallible work between crossing and marking the target as preserved.
        // A later identity/flush failure is ambiguous, never permission to remove
        // the complete target or retry over it.
        self.published = Some(name);
        Ok(())
    }

    fn cleanup(&self) {
        if self.published.is_some() || self.stage.require_named().is_err() {
            return;
        }
        for (relative, expected) in &self.files {
            let Ok((parent, name)) = self.output_parent(relative) else {
                return;
            };
            let Ok(file) = parent.open_file(name, false) else {
                continue;
            };
            let Ok(identity) = mneme_store_path::windows::file_identity(&file) else {
                continue;
            };
            if identity.same_file(*expected) {
                let _ = parent.remove_file(name, identity);
            }
        }
        if let Some(bodies) = &self.bodies {
            let _ = self
                .stage
                .remove_directory(std::ffi::OsStr::new("bodies"), bodies.identity());
        }
        if let Some(name) = self.stage.path().file_name() {
            // The exact directory can only disappear when the recognized
            // inventory was all it contained; unknown residue stays for review.
            let _ = self.root.remove_directory(name, self.stage.identity());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(label: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("mneme-snapshot-{label}-{}", Ulid::new()));
        create_private_dir(&root).unwrap();
        let source = root.join("memory.db");
        fs::write(&source, b"fixture database bytes").unwrap();
        (root, source)
    }

    fn pending(source: &Path, generation: Ulid, refs: &[&str]) -> PendingSnapshot {
        PendingSnapshot::new(
            source,
            Ulid::new(),
            generation,
            EmbeddingFingerprint::new("test:model", 8, "l2-f32-v1", "symmetric-v1"),
            refs.iter().map(|value| (*value).to_owned()).collect(),
        )
        .unwrap()
    }

    #[test]
    fn missing_referenced_body_removes_only_this_unpublished_stage() {
        let (root, source) = fixture("stage");
        create_private_dir(&source.with_extension("bodies")).unwrap();
        let generation = Ulid::new();
        let pending = pending(&source, generation, &["fs://missing"]);
        let stage = root.join("snapshots").join(format!(".stage-{generation}"));
        assert!(stage.is_dir());
        assert!(pending.copy_source().is_err());
        assert!(!stage.exists());
        assert!(source.is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_never_replaces_an_existing_generation() {
        let (root, source) = fixture("collision");
        let generation = Ulid::new();
        let copied = pending(&source, generation, &[]).copy_source().unwrap();
        let target = root.join("snapshots").join(generation.to_string());
        create_private_dir(&target).unwrap();
        fs::write(target.join("sentinel"), b"do not replace").unwrap();
        assert!(copied.verify_and_publish().is_err());
        assert_eq!(
            fs::read(target.join("sentinel")).unwrap(),
            b"do not replace"
        );
        assert!(
            !root
                .join("snapshots")
                .join(format!(".stage-{generation}"))
                .exists()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn post_rename_failure_preserves_complete_ambiguous_target() {
        let (root, source) = fixture("crossed");
        let generation = Ulid::new();
        let copied = pending(&source, generation, &[]).copy_source().unwrap();
        let error = copied
            .verify_and_publish_with_post_rename(|| {
                Err("injected failure after rename before final sync".into())
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("after rename"), "{error}");
        let target = root.join("snapshots").join(generation.to_string());
        assert_eq!(
            fs::read(target.join("database.db")).unwrap(),
            b"fixture database bytes"
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(target.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["generation"], generation.to_string());
        assert!(
            !root
                .join("snapshots")
                .join(format!(".stage-{generation}"))
                .exists()
        );
        let original_manifest = fs::read(target.join("manifest.json")).unwrap();
        let retry = pending(&source, generation, &[]).copy_source().unwrap();
        assert!(retry.verify_and_publish().is_err());
        assert_eq!(
            fs::read(target.join("manifest.json")).unwrap(),
            original_manifest
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn successful_snapshot_contains_verified_database_and_referenced_bodies_only() {
        let (root, source) = fixture("success");
        let bodies = source.with_extension("bodies");
        create_private_dir(&bodies).unwrap();
        fs::write(bodies.join("wanted"), b"remember this").unwrap();
        fs::write(bodies.join("unreferenced"), b"not in the snapshot").unwrap();
        let db_id = Ulid::new();
        let generation = Ulid::new();
        let published = PendingSnapshot::new(
            &source,
            db_id,
            generation,
            EmbeddingFingerprint::new("test:model", 8, "l2-f32-v1", "symmetric-v1"),
            vec!["fs://wanted".to_owned(), "fs://wanted".to_owned()],
        )
        .unwrap()
        .copy_source()
        .unwrap()
        .verify_and_publish()
        .unwrap();
        assert_eq!(
            published,
            root.join("snapshots").join(generation.to_string())
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(published.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["schema"], "mneme.snapshot.v1");
        assert_eq!(manifest["db_id"], db_id.to_string());
        assert_eq!(manifest["generation"], generation.to_string());
        let records = manifest["files"].as_array().unwrap();
        assert_eq!(records.len(), 2);
        for record in records {
            let bytes = fs::read(published.join(record["path"].as_str().unwrap())).unwrap();
            assert_eq!(record["size"], bytes.len() as u64);
            assert_eq!(record["sha256"], format!("{:x}", Sha256::digest(&bytes)));
        }
        assert_eq!(
            fs::read(published.join("database.db")).unwrap(),
            b"fixture database bytes"
        );
        assert_eq!(
            fs::read(published.join("bodies/wanted")).unwrap(),
            b"remember this"
        );
        assert!(!published.join("bodies/unreferenced").exists());
        assert!(
            !root
                .join("snapshots")
                .join(format!(".stage-{generation}"))
                .exists()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_staged_bytes_refuse_publication() {
        let (root, source) = fixture("changed");
        let generation = Ulid::new();
        let copied = pending(&source, generation, &[]).copy_source().unwrap();
        let stage = root.join("snapshots").join(format!(".stage-{generation}"));
        fs::write(stage.join("database.db"), b"different").unwrap();
        assert!(copied.verify_and_publish().is_err());
        assert!(!root.join("snapshots").join(generation.to_string()).exists());
        assert!(!stage.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn multiply_linked_source_refuses_copy_without_touching_source() {
        let (root, source) = fixture("hardlink");
        fs::hard_link(&source, root.join("alias.db")).unwrap();
        let generation = Ulid::new();
        let pending = pending(&source, generation, &[]);
        assert!(pending.copy_source().is_err());
        assert_eq!(fs::read(&source).unwrap(), b"fixture database bytes");
        assert!(
            !root
                .join("snapshots")
                .join(format!(".stage-{generation}"))
                .exists()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn replacement_staged_file_is_not_published_or_cleaned_up() {
        let (root, source) = fixture("file-swap");
        let generation = Ulid::new();
        let copied = pending(&source, generation, &[]).copy_source().unwrap();
        let stage = root.join("snapshots").join(format!(".stage-{generation}"));
        // Keep the original object alive at a second name so NTFS cannot reuse
        // its identity for the replacement in this deterministic fixture.
        fs::rename(stage.join("database.db"), root.join("old-staged.db")).unwrap();
        fs::write(stage.join("database.db"), b"fixture database bytes").unwrap();
        assert!(copied.verify_and_publish().is_err());
        assert!(!root.join("snapshots").join(generation.to_string()).exists());
        assert_eq!(
            fs::read(stage.join("database.db")).unwrap(),
            b"fixture database bytes"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn cleanup_preserves_unknown_stage_entries() {
        let (root, source) = fixture("residue");
        let generation = Ulid::new();
        let copied = pending(&source, generation, &[]).copy_source().unwrap();
        let stage = root.join("snapshots").join(format!(".stage-{generation}"));
        fs::write(stage.join("unrecognized"), b"preserve me").unwrap();
        drop(copied);
        assert_eq!(
            fs::read(stage.join("unrecognized")).unwrap(),
            b"preserve me"
        );
        assert!(!stage.join("database.db").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn cleanup_preserves_replaced_stage_identity() {
        let (root, source) = fixture("replaced");
        let generation = Ulid::new();
        let copied = pending(&source, generation, &[]).copy_source().unwrap();
        let stage = root.join("snapshots").join(format!(".stage-{generation}"));
        let detached = root.join("detached");
        fs::rename(&stage, &detached).unwrap();
        create_private_dir(&stage).unwrap();
        fs::write(stage.join("unrecognized"), b"preserve me").unwrap();
        drop(copied);
        assert_eq!(
            fs::read(stage.join("unrecognized")).unwrap(),
            b"preserve me"
        );
        assert_eq!(
            fs::read(detached.join("database.db")).unwrap(),
            b"fixture database bytes"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
