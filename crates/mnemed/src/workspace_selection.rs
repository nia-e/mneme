//! Bounded metadata-only admission of genuinely unconfigured cwd to misc.
//! Owner lookup stops at a project boundary; configured-presence/privacy checks
//! deliberately do not. Nothing here opens a memory backend or contacts an owner.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::AnyErr;

const MAX_BYTES: u64 = 64 * 1024;
const MAX_ANCESTORS: usize = 64;

fn inspect(path: &Path) -> Result<Option<std::fs::Metadata>, AnyErr> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_symlink() => Err(format!(
            "workspace configuration {} must not be a symlink",
            path.display()
        )
        .into()),
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(format!("cannot inspect workspace configuration {}", path.display()).into()),
    }
}

fn read(path: &Path) -> Result<Option<Vec<u8>>, AnyErr> {
    let Some(metadata) = inspect(path)? else {
        return Ok(None);
    };
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(format!(
            "workspace configuration {} must be a regular file of at most 65536 bytes",
            path.display()
        )
        .into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err("workspace configuration must be a regular file".into());
    }
    let mut raw = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut raw)?;
    if raw.len() as u64 > MAX_BYTES {
        return Err("workspace configuration exceeds 65536 bytes".into());
    }
    Ok(Some(raw))
}

// Inspect routing fields only, not arbitrary developer instructions. Historical
// copied launchers need not retain a Mneme-named parent directory.
fn owned(value: &Value, depth: usize) -> Result<bool, AnyErr> {
    if depth > 32 {
        return Err("workspace configuration exceeds nesting limit".into());
    }
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if owned(&Value::String(key.clone()), depth + 1)? || owned(value, depth + 1)? {
                    return Ok(true);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                if owned(item, depth + 1)? {
                    return Ok(true);
                }
            }
        }
        Value::String(text) => {
            if text.to_ascii_lowercase().contains("mneme") {
                return Ok(true);
            }
            let words = shlex::split(text).unwrap_or_else(|| vec![text.clone()]);
            return Ok(words.iter().any(|word| {
                matches!(
                    Path::new(word).file_name().and_then(|name| name.to_str()),
                    Some("launcher.py" | "library_launcher.py" | "hooks.py" | "hook_launcher.py")
                )
            }));
        }
        _ => {}
    }
    Ok(false)
}

// The same structural routing cases as Codex's misc_binding reader. Prompt and
// status text are not enrollment; external managed handlers are explicit state.
fn hook_events_owned(events: &Value, allow_state: bool) -> Result<bool, AnyErr> {
    let events = events
        .as_object()
        .ok_or("unknown workspace hooks configuration")?;
    let mut configured = false;
    for (name, groups) in events {
        if allow_state && name == "state" {
            continue;
        }
        if allow_state && name == "enabled" {
            if !groups.is_boolean() {
                return Err("invalid workspace hooks enabled flag".into());
            }
            continue;
        }
        if allow_state && matches!(name.as_str(), "managed_dir" | "windows_managed_dir") {
            let path = groups
                .as_str()
                .filter(|path| path.len() <= 4096 && !path.contains('\0'))
                .ok_or("invalid workspace managed hook directory")?;
            configured |= !path.is_empty();
            continue;
        }
        for group in groups.as_array().ok_or("invalid workspace hook event")? {
            for handler in group
                .get("hooks")
                .and_then(Value::as_array)
                .ok_or("invalid workspace hook group")?
            {
                let field = |key: &str| {
                    handler
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                };
                match field("type").ok_or("invalid workspace hook handler")? {
                    "command" => {
                        configured |= owned(
                            &Value::String(
                                field("command")
                                    .ok_or("invalid workspace hook command")?
                                    .to_owned(),
                            ),
                            0,
                        )?
                    }
                    "mcp_tool" => {
                        let server = field("server").ok_or("invalid workspace MCP hook route")?;
                        let tool = field("tool").ok_or("invalid workspace MCP hook route")?;
                        configured |= server.to_ascii_lowercase().contains("mneme")
                            || tool.to_ascii_lowercase().contains("mneme");
                    }
                    "prompt" | "agent" => {
                        field("prompt").ok_or("invalid workspace prompt hook")?;
                    }
                    _ => return Err("unknown workspace hook handler type".into()),
                }
            }
        }
    }
    Ok(configured)
}

// serde_json::Value alone accepts duplicate fields. Configuration admission must
// not choose between contradictory routing records by their order in a file.
struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Unique;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON without duplicate fields")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Unique, M::Error> {
                let mut out = serde_json::Map::new();
                while let Some((key, Unique(value))) = map.next_entry::<String, Unique>()? {
                    if out.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate configuration field"));
                    }
                }
                Ok(Unique(Value::Object(out)))
            }
            fn visit_seq<S: serde::de::SeqAccess<'de>>(
                self,
                mut seq: S,
            ) -> Result<Unique, S::Error> {
                let mut out = Vec::new();
                while let Some(Unique(value)) = seq.next_element()? {
                    out.push(value);
                }
                Ok(Unique(Value::Array(out)))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Unique, E> {
                Ok(Unique(Value::String(value.to_owned())))
            }
            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Unique, E> {
                Ok(Unique(Value::Bool(value)))
            }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Unique, E> {
                Ok(Unique(value.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Unique, E> {
                Ok(Unique(value.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Unique, E> {
                Ok(Unique(
                    serde_json::Number::from_f64(value)
                        .map(Value::Number)
                        .ok_or_else(|| E::custom("invalid number"))?,
                ))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
        }
        d.deserialize_any(Visitor)
    }
}

/// Classify an explicit project binding without following service configuration
/// or applying the ambient device-config exemption. Bootstrap shares the same
/// bounded, fail-closed routing parser as ordinary workspace selection.
pub(crate) fn project_codex_configured(directory: &Path) -> Result<bool, AnyErr> {
    codex_configured(directory, None)
}

fn codex_configured(directory: &Path, user_config: Option<&Path>) -> Result<bool, AnyErr> {
    // Refuse a workspace symlink BEFORE the device exemption: a project alias
    // into global configuration is not permission to erase its boundary.
    if let Some(metadata) = inspect(directory)? {
        if !metadata.is_dir() {
            return Err("workspace .codex must be a directory".into());
        }
    }
    // The global device hook is not project enrollment and cannot self-suppress.
    if user_config.is_some_and(|user| {
        directory == user
            || directory
                .canonicalize()
                .ok()
                .zip(user.canonicalize().ok())
                .is_some_and(|(a, b)| a == b)
    }) {
        return Ok(false);
    }
    let mut configured = false;
    if let Some(raw) = read(&directory.join("config.toml"))? {
        // Never expose parser diagnostics: they can contain credentials.
        let data: toml::Value = std::str::from_utf8(&raw)
            .ok()
            .and_then(|s| toml::from_str(s).ok())
            .ok_or("invalid workspace Codex TOML configuration")?;
        let data = serde_json::to_value(data)?;
        if let Some(servers) = data.get("mcp_servers") {
            let servers = servers
                .as_object()
                .ok_or("invalid workspace MCP configuration")?;
            for (name, server) in servers {
                let server = server
                    .as_object()
                    .ok_or("invalid workspace MCP configuration")?;
                for field in ["command", "url"] {
                    if server.get(field).is_some_and(|value| !value.is_string()) {
                        return Err("invalid workspace MCP endpoint".into());
                    }
                }
                let has = |field| {
                    server
                        .get(field)
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.is_empty())
                };
                if has("command") == has("url") {
                    return Err("workspace MCP endpoint requires one command or URL".into());
                }
                if let Some(args) = server.get("args") {
                    if !args
                        .as_array()
                        .is_some_and(|args| args.iter().all(Value::is_string))
                    {
                        return Err("invalid workspace MCP arguments".into());
                    }
                }
                configured |= name.to_ascii_lowercase().contains("mneme");
                for field in ["command", "args"] {
                    if let Some(value) = server.get(field) {
                        configured |= owned(value, 0)?;
                    }
                }
            }
        }
        if let Some(value) = data.get("hooks") {
            configured |= hook_events_owned(value, true)?;
        }
        if let Some(value) = data.get("notify") {
            configured |= owned(value, 0)?;
        }
        configured |= raw
            .windows(b"# BEGIN mneme-codex".len())
            .any(|part| part == b"# BEGIN mneme-codex");
    }
    if let Some(raw) = read(&directory.join("hooks.json"))? {
        let Unique(data) = serde_json::from_slice(&raw)
            .map_err(|_| "invalid workspace hooks JSON configuration")?;
        let data = data
            .as_object()
            .ok_or("invalid workspace hooks configuration")?;
        if data
            .get("schema")
            .and_then(Value::as_str)
            .is_some_and(|schema| schema.starts_with("mneme."))
        {
            return Ok(true);
        }
        let events = data
            .get("hooks")
            .ok_or("unknown workspace hooks configuration")?;
        configured |= hook_events_owned(events, false)?;
    }
    Ok(configured)
}

fn workspace_origins(cwd: &Path) -> Result<Vec<PathBuf>, AnyErr> {
    let canonical = cwd.canonicalize()?;
    // Shell PWD can preserve a privacy-bearing lexical alias, but cannot select
    // another directory. Ignore stale/spoofed/relative/unbounded PWD entirely.
    let origin = std::env::var_os("PWD").map(PathBuf::from).filter(|path| {
        path.is_absolute()
            && path.as_os_str().len() <= 4096
            && path.canonicalize().ok().as_ref() == Some(&canonical)
    });
    Ok(origin
        .into_iter()
        .chain([cwd.to_owned(), canonical])
        .collect())
}

pub(crate) fn validate_excluded_roots(values: &[String]) -> Result<Vec<PathBuf>, AnyErr> {
    const ERROR: &str = "misc excluded_roots must be at most 64 unique normalized absolute paths (4096 bytes each, 16384 bytes total)";
    if values.len() > 64 || values.iter().map(String::len).sum::<usize>() > 16384 {
        return Err(ERROR.into());
    }
    let mut roots = Vec::new();
    for value in values {
        let path = PathBuf::from(value);
        // Path equality normalizes separators/dot components, so compare the
        // reconstructed UTF-8 spelling too. Normalization must be explicit.
        if value.len() > 4096
            || value.contains('\0')
            || !path.is_absolute()
            || path.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            || path.components().collect::<PathBuf>().to_str() != Some(value.as_str())
            || roots.contains(&path)
        {
            return Err(ERROR.into());
        }
        roots.push(path);
    }
    Ok(roots)
}

pub(crate) fn check_excluded_roots(cwd: &Path, roots: &[PathBuf]) -> Result<(), AnyErr> {
    for candidate in workspace_origins(cwd)? {
        for root in roots {
            let canonical = match root.canonicalize() {
                Ok(path) => Some(path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => return Err("cannot resolve configured misc excluded root; repair misc.json or select --remote URL explicitly".into()),
            };
            if candidate.starts_with(root)
                || canonical
                    .as_ref()
                    .is_some_and(|root| candidate.starts_with(root))
            {
                return Err("workspace is excluded from shared misc memory; use its intended project owner, --user for deliberate global selection, or --remote URL for deliberate remote selection (no fallback attempted)".into());
            }
        }
    }
    Ok(())
}

pub(crate) fn check_misc_fallback(cwd: &Path) -> Result<(), AnyErr> {
    let user_config = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")));
    let mut seen = std::collections::HashSet::new();
    for chain in workspace_origins(cwd)? {
        crate::cli_library_config::check_misc_fallback(&chain)?;
        for (index, parent) in chain.ancestors().enumerate() {
            if index >= MAX_ANCESTORS {
                return Err("misc workspace search exceeds 64 ancestors".into());
            }
            if !seen.insert(parent.to_owned()) {
                continue;
            }
            if inspect(&parent.join(".mneme"))?.is_some()
                || codex_configured(&parent.join(".codex"), user_config.as_deref())?
            {
                return Err(format!("project Mneme configuration exists at {}; shared misc fallback is disabled; enroll the intended CLI owner or explicitly select --remote URL / --db PATH", parent.display()).into());
            }
        }
    }
    Ok(())
}
