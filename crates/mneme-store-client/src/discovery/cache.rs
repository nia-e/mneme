//! Client configuration receipts only: never memory database files or generations.
use crate::{AnyErr, OperationBudget};
use mneme_config::{DiscoveryBinding, DiscoveryReceipt, MAX_DISCOVERY_RECEIPT_BYTES, sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// A process-scoped OS lock. The empty lock file persists so another process
/// cannot lock a replacement inode while a previous holder is still publishing.
pub(super) struct ReceiptLock {
    _file: File,
}

fn owned(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Local metadata may grant authority; never accept another user's cache
        // or a group/world-writable publication directory/file.
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

fn checked_directory(path: &Path) -> Result<(), AnyErr> {
    if !path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err("discovery cache path must be normalized absolute metadata path".into());
    }
    // Existing ancestors may be system-owned (e.g. /Users), but must not be
    // symlinks. The publication directory itself must belong to this user.
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() && !metadata.is_symlink() => {}
            Ok(_) => return Err("discovery cache ancestor is not a non-symlink directory".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = std::fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                match builder.create(ancestor) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => {
                        return Err("cannot create user-owned discovery cache directory".into());
                    }
                }
                let metadata = std::fs::symlink_metadata(ancestor)?;
                if !metadata.is_dir() || metadata.is_symlink() || !owned(&metadata) {
                    return Err(
                        "discovery cache directory is not private user-owned metadata".into(),
                    );
                }
            }
            Err(_) => return Err("cannot inspect discovery cache directory".into()),
        }
    }
    if !owned(&std::fs::symlink_metadata(path)?) {
        return Err(
            "discovery cache directory must be user-owned and not group/world writable".into(),
        );
    }
    Ok(())
}

fn checked_target(path: &Path) -> Result<(), AnyErr> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.is_file()
                && !metadata.is_symlink()
                && owned(&metadata)
                && metadata.len() <= MAX_DISCOVERY_RECEIPT_BYTES as u64 =>
        {
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(
            "discovery cache target must be a bounded user-owned regular file, not a symlink"
                .into(),
        ),
    }
}

impl ReceiptLock {
    pub(super) async fn acquire(path: &Path, budget: &OperationBudget) -> Result<Self, AnyErr> {
        budget.timeouts()?;
        checked_directory(path.parent().ok_or("discovery receipt has no parent")?)?;
        let lock_path = path.with_extension("lock");
        checked_target(&lock_path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options
            .open(lock_path)
            .map_err(|_| "cannot open discovery publication lock")?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || !owned(&metadata) || metadata.len() != 0 {
            return Err(
                "discovery publication lock must be an empty user-owned regular file".into(),
            );
        }
        loop {
            budget.timeouts()?;
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::time::timeout_at(
                        budget.deadline,
                        tokio::time::sleep(std::time::Duration::from_millis(10)),
                    )
                    .await
                    .map_err(|_| "discovery publication is busy; shared deadline exhausted")?;
                }
                Err(_) => return Err("cannot lock discovery receipt publication".into()),
            }
        }
    }

    pub(super) fn publish(
        &self,
        path: &Path,
        receipt: &DiscoveryReceipt,
    ) -> Result<DiscoveryBinding, AnyErr> {
        checked_target(path)?;
        let bytes = serde_json::to_vec(receipt)?;
        if bytes.len() > MAX_DISCOVERY_RECEIPT_BYTES {
            return Err("discovery receipt exceeds aggregate byte limit".into());
        }
        // Check the actual serialized contract before touching the old usable file.
        DiscoveryReceipt::parse(&bytes)?;
        let temporary = path.with_extension(format!("{}.tmp", ulid::Ulid::new()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|_| "cannot create discovery receipt temporary file")?;
        let cleanup = Temporary(temporary.clone());
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        checked_target(path)?;
        std::fs::rename(&temporary, path)
            .map_err(|_| "cannot atomically publish discovery receipt")?;
        // A directory sync failure is an explicit failure, not a successful
        // durability claim. The new complete file may already be visible.
        File::open(path.parent().unwrap())?.sync_all()?;
        drop(cleanup);
        Ok(DiscoveryBinding {
            path: path.to_owned(),
            sha256: sha256(&bytes),
        })
    }
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
