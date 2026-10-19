//! Read-only admission for declaratively managed TOML, not legacy privacy
//! markers or mutable store/state files. Resolve configuration symlinks before
//! opening a bounded regular target; callers retain the logical selection path.

use crate::{MAX_CONFIG_BYTES, Result, error};
use std::io::Read;
use std::path::{Path, PathBuf};

const READ_ERROR: &str = "cannot read configuration: use a readable regular file within its configured byte bound; repair broken or looping symlinks";

pub(crate) struct SourceFile {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

fn resolve(path: &Path) -> Result<Option<PathBuf>> {
    match path.canonicalize() {
        Ok(path) => Ok(Some(path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // `canonicalize` reports both absence and a dangling link as ENOENT.
            // Only a genuinely absent path beneath an existing directory may
            // mean no configuration. A broken leaf or ancestor link is present
            // invalid metadata, never permission to discover a fallback owner.
            for ancestor in path.ancestors() {
                match std::fs::symlink_metadata(ancestor) {
                    Ok(metadata) if ancestor != path && metadata.is_dir() => return Ok(None),
                    Ok(metadata) if ancestor != path && metadata.is_symlink() => {
                        return match std::fs::metadata(ancestor) {
                            Ok(target) if target.is_dir() => Ok(None),
                            _ => Err(error(READ_ERROR)),
                        };
                    }
                    Ok(_) => return Err(error(READ_ERROR)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err(error(READ_ERROR)),
                }
            }
            Ok(None)
        }
        Err(_) => Err(error(READ_ERROR)),
    }
}

pub(crate) fn present(path: &Path) -> Result<bool> {
    Ok(resolve(path)?.is_some())
}

pub(crate) fn read(path: &Path) -> Result<Option<SourceFile>> {
    read_bounded(path, MAX_CONFIG_BYTES)
}

pub(crate) fn read_bounded(path: &Path, max_bytes: usize) -> Result<Option<SourceFile>> {
    let Some(path) = resolve(path)? else {
        return Ok(None);
    };
    let valid =
        |metadata: &std::fs::Metadata| metadata.is_file() && metadata.len() <= max_bytes as u64;
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| error(READ_ERROR))?;
    if !valid(&metadata) {
        return Err(error(READ_ERROR));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // We already resolved intended symlinks. Do not follow a replacement
        // leaf, and never block if a FIFO races the regular-file preflight.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(&path).map_err(|_| error(READ_ERROR))?;
    if !valid(&file.metadata().map_err(|_| error(READ_ERROR))?) {
        return Err(error(READ_ERROR));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| error(READ_ERROR))?;
    if bytes.len() > max_bytes {
        return Err(error(READ_ERROR));
    }
    Ok(Some(SourceFile { path, bytes }))
}
