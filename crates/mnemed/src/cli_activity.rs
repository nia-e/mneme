//! Local session diagnostics, before owner configuration or graph admission.

use crate::{AnyErr, Cli};
use clap::Args;
use mneme_app::session_activity::{self, ActivityRequest, MAX_OUTPUT_BYTES};
use std::path::PathBuf;

#[derive(Args)]
pub(crate) struct ActivityArgs {
    /// Full host session ID, uniquely matching prefix, or source-qualified ID.
    /// Without a selector, list retained sessions; never assume the latest is current.
    #[arg(long, value_name = "ID")]
    session: Option<String>,
    /// Include bounded event times, reasons, origins and owner-qualified node IDs.
    /// Never includes memory bodies, queries or transcripts.
    #[arg(long)]
    details: bool,
    /// Local activity manifest (default: platform Mneme config/activity.json).
    /// This is separate from memory.toml; no owner or database is opened.
    #[arg(long, value_name = "PATH")]
    activity_config: Option<PathBuf>,
}

fn require_local_scope(cli: &Cli) -> Result<(), &'static str> {
    if cli.db.is_some()
        || cli.user
        || cli.memory_config.is_some()
        || cli.store.is_some()
        || cli.profile.is_some()
        || cli.connection.remote.is_some()
        || cli.connection.remote_db.is_some()
        || cli.connection.remote_config.is_some()
        || cli.connection.remote_mcp_port.is_some()
        || cli.connection.remote_token_env.is_some()
    {
        return Err(
            "activity reports only the current workspace; remove --db/MNEME_DB, --user, --memory-config, --store, --profile and remote options; use --activity-config PATH for its local manifest",
        );
    }
    Ok(())
}

fn refuse(code: &str, message: &str, json: bool) -> Result<(), AnyErr> {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "schema": "mneme.session-activity.error.v1",
                "code": code,
                "error": message,
            })
        );
    }
    Err(format!("{code}: {message}").into())
}

pub(crate) fn run(args: &ActivityArgs, cli: &Cli) -> Result<(), AnyErr> {
    if let Err(message) = require_local_scope(cli) {
        return refuse("unsupported_selectors", message, cli.json);
    }
    // Capture the actual process cwd, not ambient PWD or a graph discovery result.
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(_) => {
            return refuse(
                "cwd_unavailable",
                "cannot resolve the current workspace directory",
                cli.json,
            );
        }
    };
    let manifest = match args
        .activity_config
        .clone()
        .map(Ok)
        .unwrap_or_else(session_activity::default_manifest_path)
    {
        Ok(path) => path,
        Err(error) => return refuse(error.code(), &error.to_string(), cli.json),
    };
    let request = ActivityRequest {
        session: args.session.clone(),
        details: args.details,
    };
    let report = match session_activity::report(&manifest, &cwd, request) {
        Ok(report) => report,
        Err(error) => return refuse(error.code(), &error.to_string(), cli.json),
    };
    // The shared workflow bounds projection before rendering. This final fuse
    // also covers frontend framing, without silently dropping report semantics.
    let mut output = if cli.json {
        serde_json::to_string(&report)?
    } else {
        session_activity::render_human(&report)
    };
    if !output.ends_with('\n') {
        output.push('\n');
    }
    if output.len() > MAX_OUTPUT_BYTES {
        return refuse(
            "output_limit",
            "activity report exceeds the output bound",
            cli.json,
        );
    }
    print!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Command;
    use clap::Parser;

    #[test]
    fn activity_parser_keeps_chooser_and_explicit_session_distinct() {
        for selector in [None, Some("a17c9e"), Some("codex:full-host-session-id")] {
            let mut argv = vec!["mnemed", "--json", "activity", "--details"];
            if let Some(selector) = selector {
                argv.extend(["--session", selector]);
            }
            let cli = Cli::try_parse_from(argv).unwrap();
            let Command::Activity(args) = &cli.command else {
                panic!("expected activity command");
            };
            assert!(cli.json);
            assert!(args.details);
            assert_eq!(args.session.as_deref(), selector);
        }
    }

    #[test]
    fn activity_rejects_global_scope_and_graph_arguments() {
        for option in ["--global", "--db-id", "--body", "--query", "--cwd"] {
            assert!(Cli::try_parse_from(["mnemed", "activity", option]).is_err());
        }
    }

    #[test]
    fn activity_is_not_a_graph_or_remote_operation_even_for_internal_callers() {
        let cli = Cli::try_parse_from(["mnemed", "activity"]).unwrap();
        assert!(crate::cli_config::independent(&cli.command));
        let connection = crate::remote_config::RemoteOptions {
            remote: Some("not-configured".into()),
            remote_config: Some("absent-remote-config.json".into()),
            ..Default::default()
        };
        let error = crate::cli_owner::resolve(&cli.command, &connection, false, None)
            .err()
            .expect("activity cannot resolve an owner");
        assert_eq!(
            error.to_string(),
            "activity is a local diagnostic and cannot resolve an owner"
        );
        assert!(crate::remote_commands::prepare(&cli.command, "project").is_err());
        for json in [false, true] {
            assert!(
                crate::remote_commands::render(&cli.command, &serde_json::json!({}), json).is_err()
            );
        }
    }

    #[test]
    fn owner_selection_is_refused_before_the_local_reader() {
        for selector in [
            vec!["--db", "absent.db"],
            vec!["--user"],
            vec!["--memory-config", "absent.toml"],
            vec!["--store", "unknown"],
            vec!["--profile", "private"],
            vec!["--remote", "absent"],
            vec!["--remote", "absent", "--remote-db", "project"],
            vec!["--remote", "absent", "--remote-config", "absent.json"],
            vec!["--remote", "ssh://absent", "--remote-mcp-port", "1234"],
            vec!["--remote", "absent", "--remote-token-env", "UNUSED"],
        ] {
            for before in [false, true] {
                let mut argv = vec!["mnemed"];
                if !before {
                    argv.push("activity");
                }
                argv.extend(selector.iter().copied());
                if before {
                    argv.push("activity");
                }
                let cli = Cli::try_parse_from(argv).unwrap();
                assert!(require_local_scope(&cli).is_err());
            }
        }
    }
}
