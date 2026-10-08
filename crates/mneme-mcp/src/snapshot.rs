//! Detached, immutable snapshot bundle construction. The caller owns the live
//! database lease and has quiesced mutations before any of these steps run.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
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
    stage: PathBuf,
    manifest: Manifest,
    total_size: u64,
}

pub struct CopiedSnapshot(PendingSnapshot);

impl Drop for PendingSnapshot {
    fn drop(&mut self) {
        // Only this operation's private stage, never a published generation.
        // Process death can still leave residue; it is never selected as a
        // snapshot and a later operator may inspect/remove it deliberately.
        if fs::symlink_metadata(&self.stage).is_ok_and(|metadata| metadata.file_type().is_dir()) {
            let _ = fs::remove_dir_all(&self.stage);
        }
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
        let stage = root.join(format!(".stage-{generation}"));
        create_private_dir(&stage)?;
        Ok(Self {
            source: source.to_path_buf(),
            body_names,
            root,
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
        })
    }

    /// Called only after the source backend has been checkpointed and closed.
    pub fn copy_source(mut self) -> Result<CopiedSnapshot, AnyErr> {
        self.copy_file(&self.source.clone(), "database.db".to_owned())?;
        if !self.body_names.is_empty() {
            let body_dir = self.source.with_extension("bodies");
            let metadata = fs::symlink_metadata(&body_dir)?;
            if !metadata.file_type().is_dir() {
                return Err("source body directory is not a real directory".into());
            }
            let target = self.stage.join("bodies");
            create_private_dir(&target)?;
            for name in self.body_names.clone() {
                self.copy_file(&body_dir.join(&name), format!("bodies/{name}"))?;
            }
            File::open(target)?.sync_all()?;
        }
        File::open(&self.stage)?.sync_all()?;
        Ok(CopiedSnapshot(self))
    }

    fn copy_file(&mut self, source: &Path, relative: String) -> Result<(), AnyErr> {
        let mut input = read_nofollow(source)?;
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
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(self.stage.join(&relative))?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        boundary(SnapshotBoundary::BeforePayloadWrite);
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
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
        let pending = self.0;
        boundary(SnapshotBoundary::BeforeVerify);
        for record in &pending.manifest.files {
            let mut file = read_nofollow(&pending.stage.join(&record.path))?;
            let mut hash = Sha256::new();
            let mut size = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
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
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut manifest = options.open(pending.stage.join("manifest.json"))?;
        boundary(SnapshotBoundary::BeforeManifestWrite);
        manifest.write_all(&serde_json::to_vec_pretty(&pending.manifest)?)?;
        boundary(SnapshotBoundary::AfterManifestWrite);
        boundary(SnapshotBoundary::BeforeManifestSync);
        manifest.sync_all()?;
        boundary(SnapshotBoundary::AfterManifestSync);
        File::open(&pending.stage)?.sync_all()?;
        let published = pending.root.join(pending.manifest.generation.to_string());
        boundary(SnapshotBoundary::BeforeRename);
        rename_no_replace(&pending.stage, &published)?;
        boundary(SnapshotBoundary::AfterRename);
        // Once crossed, failure is ambiguous to the caller. The complete
        // target remains in place for inspection/retry; Drop sees no stage.
        after_rename()?;
        File::open(&published)?.sync_all()?;
        boundary(SnapshotBoundary::AfterTargetSync);
        boundary(SnapshotBoundary::BeforeParentSync);
        File::open(&pending.root)?.sync_all()?;
        boundary(SnapshotBoundary::AfterParentSync);
        Ok(published)
    }
}

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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_no_replace(_: &Path, _: &Path) -> Result<(), AnyErr> {
    Err("snapshot publication is unsupported on this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_referenced_body_removes_only_this_unpublished_stage() {
        let root = std::env::temp_dir().join(format!("mneme-snapshot-stage-{}", Ulid::new()));
        fs::create_dir(&root).unwrap();
        let source = root.join("memory.db");
        fs::write(&source, b"fixture database bytes").unwrap();
        create_private_dir(&source.with_extension("bodies")).unwrap();
        let generation = Ulid::new();
        let pending = PendingSnapshot::new(
            &source,
            Ulid::new(),
            generation,
            EmbeddingFingerprint::new("test:model", 8, "l2-f32-v1", "symmetric-v1"),
            vec!["fs://missing".to_owned()],
        )
        .unwrap();
        let stage = root.join("snapshots").join(format!(".stage-{generation}"));
        assert!(stage.is_dir());
        assert!(pending.copy_source().is_err());
        assert!(!stage.exists());
        assert!(source.is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_never_replaces_an_existing_generation() {
        let root = std::env::temp_dir().join(format!("mneme-snapshot-collision-{}", Ulid::new()));
        fs::create_dir(&root).unwrap();
        let source = root.join("memory.db");
        fs::write(&source, b"fixture database bytes").unwrap();
        let generation = Ulid::new();
        let copied = PendingSnapshot::new(
            &source,
            Ulid::new(),
            generation,
            EmbeddingFingerprint::new("test:model", 8, "l2-f32-v1", "symmetric-v1"),
            vec![],
        )
        .unwrap()
        .copy_source()
        .unwrap();
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
        let root = std::env::temp_dir().join(format!("mneme-snapshot-crossed-{}", Ulid::new()));
        fs::create_dir(&root).unwrap();
        let source = root.join("memory.db");
        fs::write(&source, b"fixture database bytes").unwrap();
        let generation = Ulid::new();
        let copied = PendingSnapshot::new(
            &source,
            Ulid::new(),
            generation,
            EmbeddingFingerprint::new("test:model", 8, "l2-f32-v1", "symmetric-v1"),
            vec![],
        )
        .unwrap()
        .copy_source()
        .unwrap();
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
        fs::remove_dir_all(root).unwrap();
    }
}
