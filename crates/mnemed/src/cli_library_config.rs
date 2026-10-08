//! CLI-only selection; never enrolls, creates or opens a memory store.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::AnyErr;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    schema: String,
    mode: Mode,
    #[serde(default, deserialize_with = "library_path")]
    library_config: Option<PathBuf>,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Default,
    Private,
    Isolated,
}

fn library_path<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
    let value = String::deserialize(d)?;
    if value.len() > 4096 || value.contains('\0') || !Path::new(&value).is_absolute() {
        return Err(serde::de::Error::custom(
            "library_config must be a bounded absolute path",
        ));
    }
    Ok(Some(PathBuf::from(value)))
}

fn personal_config() -> Result<PathBuf, AnyErr> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local/share"))
        })
        .ok_or("library needs --config PATH when neither XDG_DATA_HOME nor HOME is set")?;
    if !data.is_absolute() {
        return Err("library default data directory must be absolute; use --config PATH".into());
    }
    Ok(data.join("mneme/libraries/personal/library.json"))
}

fn profile(cwd: &Path) -> Result<Option<Profile>, AnyErr> {
    for (index, parent) in cwd.ancestors().enumerate() {
        if index >= 64 {
            return Err(
                "library project profile search exceeds ancestor limit; use --config PATH".into(),
            );
        }
        let path = parent.join(".mneme/profile.json");
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(
                    format!("cannot inspect library profile {}: {error}", path.display()).into(),
                );
            }
        };
        if !metadata.is_file() {
            return Err(format!(
                "library profile {} must be a regular file, not a symlink",
                path.display()
            )
            .into());
        }
        let mut raw = Vec::new();
        std::fs::File::open(&path)?
            .take(8193)
            .read_to_end(&mut raw)?;
        if raw.len() > 8192 {
            return Err("library project profile exceeds 8192 bytes".into());
        }
        // Config values may accidentally contain credentials. Share the closed
        // contract in recovery copy, never echo serde's offending value.
        let selected: Profile = serde_json::from_slice(&raw).map_err(|_| {
            format!("invalid library profile {}; expected mneme.profile.v1 schema, mode default/private/isolated, and optional absolute library_config", path.display())
        })?;
        if selected.schema != "mneme.profile.v1" {
            return Err(format!("unsupported library profile schema in {}", path.display()).into());
        }
        return Ok(Some(selected));
    }
    Ok(None)
}

/// Any explicit native profile is configured state, not misc absence. A nested
/// Git boundary does not erase inherited configuration or privacy exclusions.
pub(crate) fn check_misc_fallback(cwd: &Path) -> Result<(), AnyErr> {
    if profile(cwd)?.is_some() {
        return Err("explicit project profile forbids shared misc fallback; configure the intended project CLI owner, or explicitly select --remote URL / --db PATH".into());
    }
    Ok(())
}

fn same_file(left: &Path, right: &Path) -> Result<bool, AnyErr> {
    let left = std::fs::canonicalize(left)?;
    let right = match std::fs::canonicalize(right) {
        Ok(right) => right,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if left == right {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let left = std::fs::metadata(left)?;
        let right = std::fs::metadata(right)?;
        return Ok(left.dev() == right.dev() && left.ino() == right.ino());
    }
    #[cfg(not(unix))]
    Ok(false)
}

pub(crate) fn resolve(explicit: Option<&Path>) -> Result<PathBuf, AnyErr> {
    // An explicit CLI selection remains an intentional override, not ambient policy.
    if let Some(path) = explicit {
        return Ok(path.to_owned());
    }
    if let Some(profile) = profile(&std::env::current_dir()?.canonicalize()?)? {
        if let Some(chosen) = profile.library_config {
            if profile.mode == Mode::Isolated && same_file(&chosen, &personal_config()?)? {
                return Err("isolated project selected the personal default library; configure an independent library_config".into());
            }
            return Ok(chosen);
        }
        if profile.mode == Mode::Isolated {
            return Err("isolated project needs an independent explicit library_config; personal fallback is disabled".into());
        }
    }
    let chosen = personal_config()?;
    if !chosen.try_exists()? {
        return Err(format!("no default library config at {}; use library --config PATH or set .mneme/profile.json library_config", chosen.display()).into());
    }
    Ok(chosen)
}
