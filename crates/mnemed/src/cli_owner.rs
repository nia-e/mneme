//! Owner-first project/user selection for ordinary CLI commands. This is a native CLI
//! contract. Ambient integration/profile metadata only fences misc fallback. A
//! configured owner is never an invitation to open its database ourselves.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use ulid::Ulid;

use crate::remote_commands::{Request, RequestAccess};
use crate::remote_config::{RemoteOptions, ResolvedRemote};
use crate::remote_transport::RemoteClient;
use crate::{AnyErr, Command, cli_capture::CaptureAction};

const SCHEMA: &str = "mneme.cli.owner.v1";
const MAX_CONFIG_BYTES: usize = 8 * 1024;
const MAX_ANCESTORS: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerRecord {
    schema: String,
    url: String,
    database: String,
    db_id: String,
    #[serde(default, deserialize_with = "present_optional")]
    ssh_mcp_port: Option<u16>,
    #[serde(default, deserialize_with = "present_optional")]
    token_env: Option<String>,
    #[serde(default, deserialize_with = "present_optional")]
    excluded_roots: Option<Vec<String>>,
}

// Omitted optional fields are defaults; explicit null is not a value.
fn present_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnerOrigin {
    Project,
    User,
    Misc,
    ExplicitRemote,
}

impl OwnerOrigin {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "global",
            Self::Misc => "misc",
            Self::ExplicitRemote => "remote",
        }
    }
}

pub(crate) struct SelectedRemote {
    pub(crate) remote: ResolvedRemote,
    pub(crate) owner: Option<OwnerBinding>,
    pub(crate) origin: OwnerOrigin,
}

impl SelectedRemote {
    /// Notices describe scope, not remote URLs or credentials. CLI callers send
    /// this to stderr; the TUI can surface it after entering its alternate screen.
    pub(crate) fn notice(&self) -> Option<&'static str> {
        (self.origin == OwnerOrigin::Misc)
            .then_some("no project owner configured; using the configured shared misc memory owner")
    }
}

/// Only a validated record can construct this binding. The configured identity
/// is never rebound from a discovered alias. Every enrolled database operation
/// requires a native identity guard in its owning checkout. The separate
/// registry preflight is diagnostic, not proof against replacement between calls.
pub(crate) struct OwnerBinding {
    db_id: Ulid,
    source: PathBuf,
    excluded_roots: Vec<PathBuf>,
}

fn parse(bytes: &[u8], path: &Path, origin: OwnerOrigin) -> Result<SelectedRemote, AnyErr> {
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err("CLI owner config exceeds 8192 bytes".into());
    }
    // Do not echo offending values: a mistaken URL/field may contain a secret.
    let record: OwnerRecord = serde_json::from_slice(bytes).map_err(|_| {
        format!("invalid CLI owner config {}; expected schema, url, database, db_id and optional ssh_mcp_port/token_env", path.display())
    })?;
    if record.excluded_roots.is_some() && origin != OwnerOrigin::Misc {
        return Err("excluded_roots is supported only in shared misc.json, not project/global owner records".into());
    }
    let excluded_roots = crate::workspace_selection::validate_excluded_roots(
        record.excluded_roots.as_deref().unwrap_or_default(),
    )?;
    if record.schema != SCHEMA {
        return Err(format!("unsupported CLI owner config schema; expected {SCHEMA}").into());
    }
    let id: Ulid = record
        .db_id
        .parse()
        .map_err(|_| "CLI owner db_id must be a canonical uppercase ULID")?;
    if record.db_id.len() != 26 || id.to_string() != record.db_id {
        return Err("CLI owner db_id must be a canonical uppercase ULID".into());
    }
    let url = url::Url::parse(&record.url)
        .map_err(|_| "CLI owner url must be an explicit HTTP(S) or SSH URL")?;
    if !matches!(url.scheme(), "http" | "https" | "ssh") || url.host_str().is_none() {
        return Err("CLI owner url must be an explicit HTTP(S) or SSH URL".into());
    }
    if url.password().is_some() || (url.scheme() != "ssh" && !url.username().is_empty()) {
        return Err(
            "CLI owner URL must not contain credentials; use token_env or SSH authentication"
                .into(),
        );
    }
    let remote = RemoteOptions {
        remote: Some(record.url),
        remote_db: Some(record.database),
        remote_mcp_port: record.ssh_mcp_port,
        remote_token_env: record.token_env,
        ..Default::default()
    }
    .resolve(false, None)?
    .expect("explicit URL always selects a remote");
    Ok(SelectedRemote {
        remote,
        owner: Some(OwnerBinding {
            db_id: id,
            source: path.to_path_buf(),
            excluded_roots,
        }),
        origin,
    })
}

fn read_record(path: &Path, origin: OwnerOrigin) -> Result<SelectedRemote, AnyErr> {
    if let Some(parent) = path.parent() {
        if !std::fs::symlink_metadata(parent)?.is_dir() {
            return Err("CLI owner directory must be a directory, not a symlink or file".into());
        }
    }
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err("CLI owner config must be a regular file, not a symlink or directory".into());
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
        return Err("CLI owner config must be a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    parse(&bytes, path, origin)
}

fn entry_exists(path: &Path) -> Result<bool, AnyErr> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn discover_project(cwd: &Path) -> Result<Option<SelectedRemote>, AnyErr> {
    for (index, ancestor) in cwd.ancestors().enumerate() {
        if index >= MAX_ANCESTORS {
            return Err(
                "CLI owner search exceeds 64 ancestors; select --remote or --db explicitly".into(),
            );
        }
        let directory = ancestor.join(".mneme");
        let directory_boundary = match std::fs::symlink_metadata(&directory) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(format!(
                    "CLI owner directory {} must be a directory, not a symlink or file",
                    directory.display()
                )
                .into());
            }
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let path = directory.join("cli.json");
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    return Err(format!(
                        "CLI owner config {} must be a regular file, not a symlink or directory",
                        path.display()
                    )
                    .into());
                }
                return read_owner_record(&path, OwnerOrigin::Project).map(Some);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot inspect CLI owner config {}: {error}",
                    path.display()
                )
                .into());
            }
        }
        // Match local project selection: either .git or any .mneme directory
        // bounds the project. An inner conventional store or isolated profile
        // must not accidentally inherit its parent's owner. Foreign profile
        // schemas are never parsed or reinterpreted as CLI configuration.
        if directory_boundary {
            return Err(format!("project Mneme configuration exists at {} without a CLI owner; configure .mneme/cli.json or explicitly select --remote URL / --db PATH; shared misc fallback is disabled", directory.display()).into());
        }
        if entry_exists(&ancestor.join(".git"))? {
            return Ok(None);
        }
    }
    Ok(None)
}

/// Resolve the ordinary store selector without opening a local database. None
/// means an explicitly selected offline database, never absent enrollment.
/// TUI and REPL use this same contract as one-shot commands.
pub(crate) fn resolve_store(
    options: &RemoteOptions,
    user: bool,
    db: Option<&Path>,
) -> Result<Option<SelectedRemote>, AnyErr> {
    if let Some(remote) = options.resolve(user, db)? {
        return Ok(Some(SelectedRemote {
            remote,
            owner: None,
            origin: OwnerOrigin::ExplicitRemote,
        }));
    }
    if db.is_some() {
        return Ok(None);
    }
    if user {
        let path = user_config_path()?;
        return read_owner_record(&path, OwnerOrigin::User).map(Some).map_err(|error| {
            format!("cannot select user CLI owner from {}: {error}; configure this owner record or select --remote URL / --db PATH explicitly (no local fallback)", path.display()).into()
        });
    }
    let cwd = std::env::current_dir()?;
    let selected = resolve_project_owner_at(&cwd, || owner_config_path(OwnerOrigin::Misc))?;
    if let Some(notice) = selected.notice() {
        eprintln!("{notice}");
    }
    Ok(Some(selected))
}

pub(crate) fn owner_config_path(origin: OwnerOrigin) -> Result<PathBuf, AnyErr> {
    let filename = match origin {
        OwnerOrigin::User => "cli.json",
        OwnerOrigin::Misc => "misc.json",
        _ => return Err("this owner origin has no personal connection configuration path".into()),
    };
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .ok_or("cannot locate CLI owner configuration; set XDG_CONFIG_HOME or HOME, or select --remote URL explicitly")?;
    Ok(root.join("mneme").join(filename))
}

fn user_config_path() -> Result<PathBuf, AnyErr> {
    owner_config_path(OwnerOrigin::User)
}

pub(crate) fn read_owner_record(
    path: &Path,
    origin: OwnerOrigin,
) -> Result<SelectedRemote, AnyErr> {
    if origin == OwnerOrigin::ExplicitRemote {
        return Err("explicit remote selectors do not use enrolled owner records".into());
    }
    let selected = read_record(path, origin)?;
    if origin == OwnerOrigin::Misc && selected.remote.database != "project" {
        return Err(format!("shared misc owner {} must select the ordinary 'project' registry database, not private user memory; repair misc.json or explicitly select --remote URL / --db PATH", path.display()).into());
    }
    Ok(selected)
}

fn read_misc_owner(path: &Path) -> Result<SelectedRemote, AnyErr> {
    let selected = read_owner_record(path, OwnerOrigin::Misc).map_err(|error| {
        format!("no project CLI owner configured and cannot select shared misc owner from {}: {error}; configure this mneme.cli.owner.v1 record, enroll a project owner, or explicitly select --remote URL / --db PATH (no local fallback)", path.display())
    })?;
    Ok(selected)
}

/// Shared native routing. Resolve optional misc prerequisites lazily so a valid
/// required project owner does not need HOME/XDG. Configuration/privacy fences
/// are inspected across all ancestors, independent of Git boundaries.
pub(crate) fn resolve_project_owner_at(
    cwd: &Path,
    misc_config: impl FnOnce() -> Result<PathBuf, AnyErr>,
) -> Result<SelectedRemote, AnyErr> {
    if let Some(selected) = discover_project(cwd)? {
        return Ok(selected);
    }
    crate::workspace_selection::check_misc_fallback(cwd)?;
    let selected = read_misc_owner(&misc_config()?)?;
    crate::workspace_selection::check_excluded_roots(
        cwd,
        &selected
            .owner
            .as_ref()
            .expect("enrolled misc owner")
            .excluded_roots,
    )?;
    Ok(selected)
}

/// Independent products and repository bootstrap retain their own explicit
/// selection contracts. Maintenance always requires explicit offline authority.
pub(crate) fn resolve(
    command: &Command,
    options: &RemoteOptions,
    user: bool,
    db: Option<&Path>,
) -> Result<Option<SelectedRemote>, AnyErr> {
    // Validate explicit remote/offline combinations even for independent modes.
    if let Some(remote) = options.resolve(user, db)? {
        return Ok(Some(SelectedRemote {
            remote,
            owner: None,
            origin: OwnerOrigin::ExplicitRemote,
        }));
    }
    if db.is_none()
        && matches!(
            command,
            Command::Migrate
                | Command::Reembed
                | Command::SingleGraphUpgrade(_)
                | Command::Capture {
                    action: CaptureAction::Init | CaptureAction::Inspect
                }
        )
    {
        return Err("this offline maintenance command requires an explicit --db PATH (or MNEME_DB); it never opens an implicit project or user store".into());
    }
    if matches!(
        command,
        Command::Client(_)
            | Command::Library(_)
            | Command::Tui(_)
            | Command::BootstrapInspect(_)
            | Command::BootstrapCreate(_)
            | Command::Demo { .. }
    ) {
        return Ok(None);
    }
    resolve_store(options, user, db)
}

impl OwnerBinding {
    pub(crate) fn db_id(&self) -> String {
        self.db_id.to_string()
    }

    /// A caller-provided identity must agree; enrollment never rewrites a
    /// concern packet or another explicitly pinned request to a different DB.
    pub(crate) fn check_request(&self, request: &Request) -> Result<(), AnyErr> {
        if request
            .arguments
            .get("expected_db_id")
            .is_some_and(|value| value.as_str() != Some(self.db_id.to_string().as_str()))
        {
            return Err(
                "request expected_db_id disagrees with the configured CLI owner; no operation sent"
                    .into(),
            );
        }
        Ok(())
    }

    fn check_registry(&self, database: &str, value: &Value) -> Result<(), AnyErr> {
        let rows = value
            .as_array()
            .ok_or("configured CLI owner databases response must be an array")?;
        let mut matches = rows
            .iter()
            .filter(|row| row["db"] == database || row["name"] == database);
        let row = matches.next().ok_or("configured CLI owner does not advertise the selected database; check .mneme/cli.json, no local fallback")?;
        if matches.next().is_some()
            || row["db"].as_str() != Some(database)
            || row
                .get("name")
                .is_some_and(|name| name.as_str() != Some(database))
        {
            return Err(
                "configured CLI owner advertised an ambiguous database alias; no operation sent"
                    .into(),
            );
        }
        if row["db_id"].as_str() != Some(self.db_id.to_string().as_str()) {
            return Err(format!("configured CLI owner database identity mismatch for {database:?}; inspect {}, do not retarget or retry against a replacement", self.source.display()).into());
        }
        if row["state"].as_str() != Some("open") {
            return Err("configured CLI owner database is not open; resume the intended owner explicitly, no local fallback".into());
        }
        Ok(())
    }

    pub(crate) async fn verify_and_guard(
        &self,
        client: &mut RemoteClient,
        request: &mut Request,
        database: &str,
    ) -> Result<(), AnyErr> {
        if client.server_name() != "mneme-mcp" {
            return Err(
                "configured CLI owner must be an mneme-mcp server; no operation sent".into(),
            );
        }
        let action = match request.tool {
            "save" => Some(request.arguments["kind"].as_str().unwrap_or("note")),
            "episode" | "concern" | "walk" | "database_control" | "graph" => {
                request.arguments["action"].as_str()
            }
            _ => None,
        };
        let guarded = client.supports_expected_db_id(request.tool, action);
        if !guarded {
            return Err(format!("configured CLI owner cannot guard {} with canonical expected_db_id; no operation sent. Update the owner's mneme-mcp runtime (updating only this CLI is insufficient), or explicitly select --remote URL for an unbound connection / --db PATH for offline work; no fallback attempted", request.tool).into());
        }
        request.arguments["expected_db_id"] = json!(self.db_id.to_string());
        let databases = client
            .call_tool("databases", json!({}))
            .await
            .map_err(|error| error.to_string())?;
        self.check_registry(database, &databases)
    }

    pub(crate) fn check_result(
        &self,
        request_access: RequestAccess,
        value: &Value,
        database: &str,
    ) -> Result<(), AnyErr> {
        // Reads are pinned by the atomic request guard; their native result
        // need not invent db/db_id receipt fields. Mutations additionally report
        // their selected owner. An unverified receipt is an unknown outcome,
        // never permission to retry the write.
        if request_access == RequestAccess::Mutation
            && (value["db"].as_str() != Some(database)
                || value["db_id"].as_str() != Some(self.db_id.to_string().as_str()))
        {
            return Err("configured CLI owner write receipt identity mismatch; submitted outcome is unknown, do not blindly retry".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    fn record() -> Value {
        json!({"schema":SCHEMA,"url":"http://127.0.0.1:18767/mcp","database":"project","db_id":ID})
    }
    fn parsed(raw: &Value) -> Result<SelectedRemote, AnyErr> {
        parse(
            &serde_json::to_vec(raw).unwrap(),
            Path::new("fixture/.mneme/cli.json"),
            OwnerOrigin::Project,
        )
    }

    #[test]
    fn config_has_one_closed_bounded_contract() {
        assert!(parsed(&record()).is_ok());
        for (key, value) in [
            ("schema", json!("other")),
            ("db_id", json!(ID.to_lowercase())),
            ("database", json!("../project.db")),
            ("url", json!("saved-name")),
            ("url", json!("file:///tmp/project.db")),
            ("url", json!("http://user:secret@localhost")),
            ("token_env", Value::Null),
            ("ssh_mcp_port", Value::Null),
            ("token_env", json!("secret value!")),
            ("ssh_mcp_port", json!(0)),
            ("unknown", json!(true)),
        ] {
            let mut raw = record();
            raw[key] = value;
            assert!(parsed(&raw).is_err(), "accepted invalid {key}");
        }
        assert!(
            parse(
                &vec![b' '; MAX_CONFIG_BYTES + 1],
                Path::new("test"),
                OwnerOrigin::Project
            )
            .is_err()
        );
        let mut exact = serde_json::to_vec(&record()).unwrap();
        exact.resize(MAX_CONFIG_BYTES, b' ');
        assert!(parse(&exact, Path::new("test"), OwnerOrigin::Project).is_ok());
    }

    #[test]
    fn tui_selected_owner_carries_the_same_pinned_identity() {
        let target = crate::cli_sources::selected_target(parsed(&record()).unwrap());
        assert_eq!(target.database, "project");
        assert_eq!(target.url, "http://127.0.0.1:18767/mcp");
        assert_eq!(target.expected_db_id.as_deref(), Some(ID));
        assert!(target.expected_path.is_none());
        let selected = resolve_store(
            &RemoteOptions {
                remote: Some("http://127.0.0.1:1".into()),
                ..Default::default()
            },
            true,
            None,
        )
        .unwrap()
        .unwrap();
        let target = crate::cli_sources::selected_target(selected);
        assert_eq!(target.database, "user");
        assert!(target.expected_db_id.is_none());
    }

    #[test]
    fn origin_is_explicit_metadata_and_misc_never_selects_private_registry() {
        let selected = parsed(&record()).unwrap();
        assert_eq!(selected.origin, OwnerOrigin::Project);
        assert!(selected.notice().is_none());
        let dir = std::env::temp_dir().join(format!("mneme-owner-origin-{}", Ulid::new()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("owner.json");
        std::fs::write(&path, record().to_string()).unwrap();
        for origin in [OwnerOrigin::Project, OwnerOrigin::User, OwnerOrigin::Misc] {
            let selected = read_owner_record(&path, origin).unwrap();
            assert_eq!(selected.origin, origin);
            assert_eq!(selected.notice().is_some(), origin == OwnerOrigin::Misc);
        }
        let mut raw = record();
        raw["database"] = json!("user");
        std::fs::write(&path, raw.to_string()).unwrap();
        assert!(read_owner_record(&path, OwnerOrigin::Misc).is_err());
        assert!(read_owner_record(&path, OwnerOrigin::User).is_ok());
        let selected = resolve_store(
            &RemoteOptions {
                remote: Some("http://127.0.0.1:1".into()),
                ..Default::default()
            },
            false,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected.origin, OwnerOrigin::ExplicitRemote);
        assert!(selected.notice().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn registry_must_unambiguously_bind_open_alias_to_pinned_identity() {
        let binding = parsed(&record()).unwrap().owner.unwrap();
        let valid = json!([{"db":"project","name":"project","db_id":ID,"state":"open"}]);
        assert!(binding.check_registry("project", &valid).is_ok());
        for raw in [
            json!({}),
            json!([]),
            json!([{"db":"user","db_id":ID,"state":"open"}]),
            json!([{"db":"project","name":"user","db_id":ID,"state":"open"}]),
            json!([{"db":"project","db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAW","state":"open"}]),
            json!([{"db":"project","db_id":ID,"state":"released"}]),
            json!([valid[0].clone(), valid[0].clone()]),
        ] {
            assert!(binding.check_registry("project", &raw).is_err());
        }
    }
}
