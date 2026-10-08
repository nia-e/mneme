//! Explicit remote selection. This module never resolves or opens a database.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use clap::Args;
use serde::Deserialize;

use crate::AnyErr;
use crate::remote_transport::ConnectionOptions;

const MAX_CONFIG_BYTES: usize = 64 * 1024;
const DEFAULT_SSH_MCP_PORT: u16 = 18766;

#[derive(Args, Default)]
#[command(next_help_heading = "Remote connection")]
pub(crate) struct RemoteOptions {
    /// Use an existing MCP server: http(s)://URL, ssh://[USER@]HOST[:SSH_PORT],
    /// or a name from ~/.config/mneme/remotes.json. Never opens a local database.
    #[arg(long, global = true, value_name = "URL_OR_NAME", conflicts_with = "db")]
    pub remote: Option<String>,
    /// Remote registry name (not a path). Defaults to project, or user with --user.
    #[arg(
        long,
        global = true,
        value_name = "NAME",
        requires = "remote",
        conflicts_with = "user"
    )]
    pub remote_db: Option<String>,
    /// Named-connection JSON; defaults to $XDG_CONFIG_HOME/mneme/remotes.json.
    #[arg(long, global = true, value_name = "PATH", requires = "remote")]
    pub remote_config: Option<PathBuf>,
    /// MCP port on the SSH host's loopback interface (default 18766).
    #[arg(long, global = true, value_name = "PORT", requires = "remote", value_parser = clap::value_parser!(u16).range(1..))]
    pub remote_mcp_port: Option<u16>,
    /// Read the server bearer token from this environment variable, never argv.
    #[arg(long, global = true, value_name = "VAR", requires = "remote")]
    pub remote_token_env: Option<String>,
}

pub(crate) struct ResolvedRemote {
    pub connection: ConnectionOptions,
    pub database: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteFile {
    version: u32,
    remotes: BTreeMap<String, SavedRemote>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedRemote {
    url: String,
    database: Option<String>,
    ssh_mcp_port: Option<u16>,
    token_env: Option<String>,
}

pub(crate) fn identifier(value: &str, kind: &str) -> Result<(), AnyErr> {
    if value.is_empty()
        || value.len() > 128
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(format!("{kind} must be a name of 1..=128 ASCII letters, digits, dots, underscores or hyphens, not a filesystem path").into());
    }
    Ok(())
}

fn token_variable(value: &str) -> Result<(), AnyErr> {
    let mut bytes = value.bytes();
    if value.len() > 128
        || !bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err("remote token environment variable must be an ASCII environment-variable name, not a token".into());
    }
    Ok(())
}

fn default_config() -> Result<PathBuf, AnyErr> {
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .ok_or("cannot locate remotes.json; set --remote-config PATH or use an explicit URL")?;
    Ok(root.join("mneme/remotes.json"))
}

fn saved_remote(path: &Path, name: &str) -> Result<SavedRemote, AnyErr> {
    let file = std::fs::File::open(path).map_err(|error| {
        format!("cannot read remote connection config {}: {error}; create a version-1 remotes.json or pass an explicit URL", path.display())
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err("remote connection config exceeds 64 KiB".into());
    }
    // serde error messages may contain offending values; do not echo a config
    // that could accidentally contain a credential instead of a variable name.
    let mut config: RemoteFile = serde_json::from_slice(&bytes)
        .map_err(|_| "invalid remote connection config; expected version and remotes with url, optional database, ssh_mcp_port, token_env")?;
    if config.version != 1 {
        return Err("unsupported remote connection config version; expected 1".into());
    }
    if config.remotes.len() > 64 {
        return Err("remote connection config exceeds 64 connections".into());
    }
    config.remotes.remove(name).ok_or_else(|| {
        format!(
            "unknown remote connection {name:?} in {}; add it or pass an explicit URL",
            path.display()
        )
        .into()
    })
}

impl RemoteOptions {
    pub(crate) fn resolve(
        &self,
        user: bool,
        local_db: Option<&Path>,
    ) -> Result<Option<ResolvedRemote>, AnyErr> {
        let Some(target) = &self.remote else {
            if self.remote_db.is_some()
                || self.remote_config.is_some()
                || self.remote_mcp_port.is_some()
                || self.remote_token_env.is_some()
            {
                return Err("remote connection options require --remote".into());
            }
            return Ok(None);
        };
        if local_db.is_some() {
            return Err("--remote cannot use --db or MNEME_DB filesystem paths; unset MNEME_DB and select --user or --remote-db NAME".into());
        }
        if user && self.remote_db.is_some() {
            return Err("select either --user or --remote-db NAME, not both".into());
        }
        let saved = if target.contains("://") {
            if self.remote_config.is_some() {
                return Err(
                    "--remote-config applies only to a saved connection name, not an explicit URL"
                        .into(),
                );
            }
            SavedRemote {
                url: target.clone(),
                database: None,
                ssh_mcp_port: None,
                token_env: None,
            }
        } else {
            identifier(target, "remote connection")?;
            let path = self
                .remote_config
                .clone()
                .map(Ok)
                .unwrap_or_else(default_config)?;
            saved_remote(&path, target)?
        };
        if saved.url.is_empty() || saved.url.len() > 2048 {
            return Err("remote URL must be 1..=2048 bytes".into());
        }
        if (self.remote_mcp_port.is_some() || saved.ssh_mcp_port.is_some())
            && !saved.url.starts_with("ssh://")
        {
            return Err(
                "--remote-mcp-port / ssh_mcp_port applies only to an ssh:// connection".into(),
            );
        }
        let database = if user {
            "user".to_owned()
        } else {
            self.remote_db
                .clone()
                .or(saved.database)
                .unwrap_or_else(|| "project".into())
        };
        identifier(&database, "remote database")?;
        let token_env = self.remote_token_env.clone().or(saved.token_env);
        if let Some(variable) = &token_env {
            token_variable(variable)?;
        }
        let ssh_mcp_port = self
            .remote_mcp_port
            .or(saved.ssh_mcp_port)
            .unwrap_or(DEFAULT_SSH_MCP_PORT);
        if ssh_mcp_port == 0 {
            return Err("remote MCP port must be between 1 and 65535".into());
        }
        Ok(Some(ResolvedRemote {
            connection: ConnectionOptions {
                url: saved.url,
                ssh_mcp_port,
                token_env,
            },
            database,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cli;
    use clap::Parser;

    fn options(url: &str) -> RemoteOptions {
        RemoteOptions {
            remote: Some(url.into()),
            ..Default::default()
        }
    }

    #[test]
    fn explicit_url_store_selection_never_uses_a_path() {
        let mut opts = options("http://127.0.0.1:18767");
        assert_eq!(
            opts.resolve(false, None).unwrap().unwrap().database,
            "project"
        );
        assert_eq!(opts.resolve(true, None).unwrap().unwrap().database, "user");
        opts.remote_db = Some("workshop".into());
        assert_eq!(
            opts.resolve(false, None).unwrap().unwrap().database,
            "workshop"
        );
        assert!(opts.resolve(true, None).is_err());
        assert!(opts.resolve(false, Some(Path::new("store.db"))).is_err());
        opts.remote_db = Some("../store.db".into());
        assert!(opts.resolve(false, None).is_err());
    }

    #[test]
    fn remote_flags_cannot_affect_local_mode() {
        assert!(
            RemoteOptions::default()
                .resolve(false, None)
                .unwrap()
                .is_none()
        );
        let opts = RemoteOptions {
            remote_db: Some("user".into()),
            ..Default::default()
        };
        assert!(opts.resolve(false, None).is_err());
        for args in [
            vec!["mnemed", "--remote-db", "user", "status"],
            vec!["mnemed", "--remote-mcp-port", "1234", "status"],
            vec!["mnemed", "--remote-token-env", "TOKEN", "status"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn remote_global_option_and_cross_db_subcommand_do_not_collide() {
        let cli = Cli::try_parse_from([
            "mnemed",
            "--remote",
            "ssh://user@remote-host",
            "--user",
            "remote",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        ])
        .unwrap();
        assert!(matches!(cli.command, crate::Command::Remote(_)));
        assert!(cli.connection.resolve(cli.user, None).unwrap().is_some());
    }

    #[test]
    fn token_variable_and_port_configuration_are_explicit() {
        let mut opts = options("http://localhost:1234");
        opts.remote_mcp_port = Some(1234);
        assert!(opts.resolve(false, None).is_err());
        opts.remote_mcp_port = None;
        opts.remote_token_env = Some("secret value!".into());
        assert!(opts.resolve(false, None).is_err());
        opts.remote_token_env = Some("MNEME_TEST_TOKEN".into());
        assert!(opts.resolve(false, None).is_ok());
    }

    #[test]
    fn alias_config_is_versioned_and_cli_store_selection_wins() {
        let root = std::env::temp_dir().join(format!("mneme-remotes-{}", ulid::Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("remotes.json");
        std::fs::write(&path, r#"{"version":1,"remotes":{"pi":{"url":"ssh://user@remote-host","database":"user","ssh_mcp_port":18766}}}"#).unwrap();
        let mut opts = options("pi");
        opts.remote_config = Some(path.clone());
        assert_eq!(opts.resolve(false, None).unwrap().unwrap().database, "user");
        opts.remote_db = Some("other".into());
        assert_eq!(
            opts.resolve(false, None).unwrap().unwrap().database,
            "other"
        );
        std::fs::write(&path, r#"{"version":2,"remotes":{}}"#).unwrap();
        assert!(opts.resolve(false, None).is_err());
        std::fs::write(
            &path,
            r#"{"version":1,"remotes":{"pi":{"url":"http://localhost","password":"secret"}}}"#,
        )
        .unwrap();
        assert!(opts.resolve(false, None).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
