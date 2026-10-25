//! Parse one invocation into a complete mode and transport selection.
//!
//! Raw flag combinations remain local to admission. Runtime construction,
//! environment-token lookup, owner opening and transport execution belong to
//! the composition root, not this input contract.

use crate::capability::CapabilityProfile;
use std::path::PathBuf;

type AnyErr = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Startup {
    pub(crate) mode: StartupMode,
    pub(crate) transport: Transport,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StartupMode {
    Owner(OwnerOptions),
    Activity {
        config: PathBuf,
        cwd: PathBuf,
    },
    Library {
        config: PathBuf,
    },
    Router {
        config: PathBuf,
    },
    Configured {
        config: PathBuf,
        cwd: PathBuf,
        library: Option<String>,
    },
}

/// Checked owner-only settings. Private fields keep direct feedback coupled to
/// its admitted operator profile; callers can consume, not reconstruct, them.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OwnerOptions {
    databases: Vec<(String, PathBuf)>,
    discovery_catalogs: Vec<(String, PathBuf)>,
    writable_discovery_catalogs: Vec<String>,
    profile: CapabilityProfile,
    direct_feedback: DirectFeedback,
}

impl OwnerOptions {
    pub(crate) const fn profile(&self) -> CapabilityProfile {
        self.profile
    }

    pub(crate) const fn direct_feedback(&self) -> DirectFeedback {
        self.direct_feedback
    }

    pub(crate) fn discovery_catalogs(&self) -> &[(String, PathBuf)] {
        &self.discovery_catalogs
    }

    pub(crate) fn writable_discovery_catalogs(&self) -> &[String] {
        &self.writable_discovery_catalogs
    }

    pub(crate) fn into_databases(self) -> Vec<(String, PathBuf)> {
        self.databases
    }
}

/// Preserve the deprecated flag's warning provenance independently of the
/// effective operator profile. Both enabled variants expose the same tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirectFeedback {
    Disabled,
    ExplicitOperator,
    ImpliedOperator,
}

impl DirectFeedback {
    pub(crate) const fn enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    Stdio,
    Http {
        addr: String,
        // A variable name, never the secret value. Lookup stays after runtime
        // construction, as do HTTP address/token/origin validation and serving.
        token_env: Option<String>,
        cors_origin: Option<String>,
    },
}

pub(crate) fn parse_args() -> Result<Startup, AnyErr> {
    parse_args_from(std::env::args().skip(1))
}

pub(crate) fn parse_args_from<I, S>(args: I) -> Result<Startup, AnyErr>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    parse_with_cwd(args, std::env::current_dir)
}

// Keep workspace capture lazy: it precedes conflicting configured-mode flags,
// but follows token consumption and HTTP-option validation. The callback also
// lets tests exercise capture failure without changing the process-wide cwd.
fn parse_with_cwd<I, S, F>(args: I, capture_cwd: F) -> Result<Startup, AnyErr>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
    F: FnOnce() -> std::io::Result<PathBuf>,
{
    let mut dbs = Vec::new();
    let mut discovery_catalogs = Vec::new();
    let mut writable_discovery_catalogs = Vec::new();
    let mut library_config = None;
    let mut router_config = None;
    let mut memory_config = None;
    let mut activity_config = None;
    let mut cwd = None;
    let mut configured_library = None;
    let mut http = None;
    let mut http_token_env = None;
    let mut cors_origin = None;
    let mut capability_profile = None;
    let mut allow_direct_feedback = false;
    let mut it = args.into_iter().map(Into::into);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--db" => {
                let spec = it.next().ok_or("--db needs NAME=PATH")?;
                let (name, path) = spec
                    .split_once('=')
                    .ok_or_else(|| format!("--db expects NAME=PATH, got {spec:?}"))?;
                dbs.push((name.to_string(), PathBuf::from(path)));
            }
            "--discovery-catalog" => {
                let spec = it.next().ok_or("--discovery-catalog needs ALIAS=PATH")?;
                let (alias, path) = spec
                    .split_once('=')
                    .filter(|(alias, path)| !alias.is_empty() && alias.chars().count() <= 256 && !path.is_empty())
                    .ok_or("--discovery-catalog needs ALIAS=PATH with a nonempty alias of at most 256 characters and a nonempty path")?;
                if discovery_catalogs.iter().any(|(name, _)| name == alias) {
                    return Err(format!("duplicate --discovery-catalog alias {alias:?}").into());
                }
                discovery_catalogs.push((alias.to_owned(), PathBuf::from(path)));
            }
            "--discovery-catalog-writable" => {
                let alias = it
                    .next()
                    .ok_or("--discovery-catalog-writable needs ALIAS")?;
                if alias.is_empty() || alias.chars().count() > 256 {
                    return Err("--discovery-catalog-writable needs a nonempty alias of at most 256 characters".into());
                }
                if writable_discovery_catalogs.contains(&alias) {
                    return Err(
                        format!("duplicate --discovery-catalog-writable alias {alias:?}").into(),
                    );
                }
                writable_discovery_catalogs.push(alias);
            }
            "--activity-config" => {
                if activity_config.is_some() {
                    return Err("--activity-config may be specified only once".into());
                }
                activity_config = Some(PathBuf::from(
                    it.next().ok_or("--activity-config needs PATH")?,
                ));
            }
            "--memory-config" => {
                if memory_config.is_some() {
                    return Err("--memory-config may be specified only once".into());
                }
                memory_config = Some(PathBuf::from(
                    it.next().ok_or("--memory-config needs PATH")?,
                ));
            }
            "--cwd" => {
                if cwd.is_some() {
                    return Err("--cwd may be specified only once".into());
                }
                cwd = Some(PathBuf::from(it.next().ok_or("--cwd needs WORKSPACE")?));
            }
            "--library" => {
                if configured_library.is_some() {
                    return Err("--library may be specified only once".into());
                }
                configured_library = Some(it.next().ok_or("--library needs NAME")?);
            }
            "--router-config" => {
                if router_config.is_some() {
                    return Err("--router-config may be specified only once".into());
                }
                router_config = Some(PathBuf::from(
                    it.next().ok_or("--router-config needs PATH")?,
                ));
            }
            "--library-config" => {
                if library_config.is_some() {
                    return Err("--library-config may be specified only once".into());
                }
                library_config = Some(PathBuf::from(
                    it.next().ok_or("--library-config needs PATH")?,
                ));
            }
            "--http" => http = Some(it.next().ok_or("--http needs ADDR (e.g. 127.0.0.1:8080)")?),
            "--http-token-env" => {
                http_token_env = Some(
                    it.next()
                        .ok_or("--http-token-env needs an environment variable name")?,
                )
            }
            "--cors-origin" => {
                cors_origin = Some(
                    it.next()
                        .ok_or("--cors-origin needs one exact Origin URL")?,
                )
            }
            "--capability-profile" => {
                if capability_profile.is_some() {
                    return Err("--capability-profile may be specified only once".into());
                }
                capability_profile = Some(CapabilityProfile::parse(
                    &it.next()
                        .ok_or("--capability-profile needs a profile name")?,
                )?)
            }
            "--allow-direct-feedback" => allow_direct_feedback = true,
            "--help" | "-h" => {
                return Err(
                    "usage: mneme-mcp --activity-config PATH [--cwd WORKSPACE] | mneme-mcp --memory-config PATH [--cwd WORKSPACE] [--library NAME] [--http IP:PORT] | mneme-mcp [--db NAME=PATH]... [--discovery-catalog ALIAS=PATH]... [--discovery-catalog-writable ALIAS]... [--capability-profile read-only|receipt-grounded|curator|operator] [--allow-direct-feedback (deprecated; operator only)] [--http IP:PORT [--http-token-env VAR] [--cors-origin ORIGIN]] | mneme-mcp --library-config PATH [--http IP:PORT [--http-token-env VAR] [--cors-origin ORIGIN]] | mneme-mcp --router-config PATH [--http IP:PORT [--http-token-env VAR] [--cors-origin ORIGIN]]"
                        .into(),
                );
            }
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }
    // Diagnostic mode is exclusive. Reject every conflicting selector before
    // workspace capture, config discovery, owner opening, or token lookup.
    if let Some(config) = activity_config {
        if memory_config.is_some()
            || library_config.is_some()
            || router_config.is_some()
            || configured_library.is_some()
            || !dbs.is_empty()
            || !discovery_catalogs.is_empty()
            || !writable_discovery_catalogs.is_empty()
            || capability_profile.is_some()
            || allow_direct_feedback
        {
            return Err(
                "--activity-config cannot combine owner/configured/library/router settings".into(),
            );
        }
        if http.is_some() || http_token_env.is_some() || cors_origin.is_some() {
            return Err("activity mode is stdio-only; HTTP options are not supported".into());
        }
        let cwd = match cwd {
            Some(cwd) => cwd,
            None => capture_cwd().map_err(|_| "cannot capture activity MCP startup workspace")?,
        };
        return Ok(Startup {
            mode: StartupMode::Activity { config, cwd },
            transport: Transport::Stdio,
        });
    }
    if http.is_none() && (http_token_env.is_some() || cors_origin.is_some()) {
        return Err("--http-token-env and --cors-origin require --http".into());
    }
    let transport = match http {
        Some(addr) => Transport::Http {
            addr,
            token_env: http_token_env,
            cors_origin,
        },
        None => Transport::Stdio,
    };
    if let Some(config) = memory_config {
        let cwd = match cwd {
            Some(cwd) => cwd,
            None => {
                capture_cwd().map_err(|_| "cannot capture declarative MCP startup workspace")?
            }
        };
        if router_config.is_some()
            || library_config.is_some()
            || !dbs.is_empty()
            || !discovery_catalogs.is_empty()
            || !writable_discovery_catalogs.is_empty()
            || capability_profile.is_some()
            || allow_direct_feedback
        {
            return Err("--memory-config cannot combine owner/legacy coordinator settings".into());
        }
        return Ok(Startup {
            mode: StartupMode::Configured {
                config,
                cwd,
                library: configured_library,
            },
            transport,
        });
    }
    if cwd.is_some() || configured_library.is_some() {
        return Err("--cwd and --library require --memory-config".into());
    }
    if let Some(config) = router_config {
        if library_config.is_some()
            || !dbs.is_empty()
            || !discovery_catalogs.is_empty()
            || !writable_discovery_catalogs.is_empty()
            || capability_profile.is_some()
            || allow_direct_feedback
        {
            return Err("--router-config cannot be combined with --library-config, --db, --discovery-catalog, --discovery-catalog-writable, --capability-profile, or --allow-direct-feedback".into());
        }
        return Ok(Startup {
            mode: StartupMode::Router { config },
            transport,
        });
    }
    if let Some(config) = library_config {
        if !dbs.is_empty()
            || !discovery_catalogs.is_empty()
            || !writable_discovery_catalogs.is_empty()
            || capability_profile.is_some()
            || allow_direct_feedback
        {
            return Err("--library-config cannot be combined with --db, --discovery-catalog, --discovery-catalog-writable, --capability-profile, or --allow-direct-feedback".into());
        }
        return Ok(Startup {
            mode: StartupMode::Library { config },
            transport,
        });
    }
    for (alias, _) in &discovery_catalogs {
        if !(dbs.iter().any(|(name, _)| name == alias) || dbs.is_empty() && alias == "user") {
            return Err(format!("--discovery-catalog alias {alias:?} must name a startup --db (or default user owner)").into());
        }
    }
    for alias in &writable_discovery_catalogs {
        if !discovery_catalogs.iter().any(|(name, _)| name == alias) {
            return Err(format!("--discovery-catalog-writable alias {alias:?} must name an explicit --discovery-catalog ALIAS=PATH").into());
        }
    }
    let direct_feedback = match (allow_direct_feedback, capability_profile) {
        (false, _) => DirectFeedback::Disabled,
        (true, None) => DirectFeedback::ImpliedOperator,
        (true, Some(_)) => DirectFeedback::ExplicitOperator,
    };
    let profile = capability_profile.unwrap_or_else(|| {
        if allow_direct_feedback {
            CapabilityProfile::Operator
        } else {
            CapabilityProfile::default()
        }
    });
    if allow_direct_feedback && profile != CapabilityProfile::Operator {
        return Err(
            "--allow-direct-feedback is only valid with --capability-profile operator".into(),
        );
    }
    if !writable_discovery_catalogs.is_empty() && profile != CapabilityProfile::Operator {
        return Err("--discovery-catalog-writable requires --capability-profile operator".into());
    }
    Ok(Startup {
        mode: StartupMode::Owner(OwnerOptions {
            databases: dbs,
            discovery_catalogs,
            writable_discovery_catalogs,
            profile,
            direct_feedback,
        }),
        transport,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn activity_startup_is_exclusive_stdio_before_workspace_capture() {
        let startup = parse_with_cwd(
            ["--activity-config", "activity.json", "--cwd", "workspace"],
            || panic!("explicit activity cwd must not be captured"),
        )
        .unwrap();
        assert_eq!(
            startup,
            Startup {
                mode: StartupMode::Activity {
                    config: "activity.json".into(),
                    cwd: "workspace".into()
                },
                transport: Transport::Stdio,
            }
        );
        for extras in [
            vec!["--db", "tripwire=/must/not/open"],
            vec!["--discovery-catalog", "user=/must/not/read"],
            vec!["--memory-config", "/must/not/discover"],
            vec!["--library-config", "/must/not/read"],
            vec!["--router-config", "/must/not/contact"],
            vec!["--library", "any"],
            vec!["--capability-profile", "operator"],
            vec!["--allow-direct-feedback"],
            vec!["--http", "invalid-address"],
            vec!["--http-token-env", "MUST_NOT_LOOK_UP"],
            vec!["--cors-origin", "invalid-origin"],
        ] {
            let mut args = vec!["--activity-config", "activity.json"];
            args.extend(extras);
            assert!(
                parse_with_cwd(args, || panic!(
                    "conflicting mode must fail before cwd capture"
                ))
                .is_err()
            );
        }
        let captured = parse_with_cwd(["--activity-config", "activity.json"], || {
            Ok("/captured".into())
        })
        .unwrap();
        assert!(
            matches!(captured.mode, StartupMode::Activity { cwd, .. } if cwd == PathBuf::from("/captured"))
        );
    }

    #[test]
    fn configured_mode_captures_workspace_once_and_only_when_needed() {
        let captures = Cell::new(0);
        let capture = || {
            captures.set(captures.get() + 1);
            Ok(PathBuf::from("/captured/workspace"))
        };
        let args = parse_with_cwd(
            ["--memory-config", "memory.toml", "--library", "named"],
            capture,
        )
        .unwrap();
        assert_eq!(captures.get(), 1);
        assert_eq!(
            args.mode,
            StartupMode::Configured {
                config: PathBuf::from("memory.toml"),
                cwd: PathBuf::from("/captured/workspace"),
                library: Some("named".into()),
            }
        );
        assert_eq!(args.transport, Transport::Stdio);

        for argv in [
            vec![],
            vec!["--library-config", "library.json"],
            vec!["--router-config", "router.json"],
            vec![
                "--memory-config",
                "memory.toml",
                "--cwd",
                "relative-workspace",
            ],
        ] {
            parse_with_cwd(argv, || panic!("this invocation must not capture cwd")).unwrap();
        }
    }

    #[test]
    fn startup_admission_preserves_error_precedence_and_lazy_workspace_failure() {
        for (argv, expected, capture_count) in [
            (
                vec![
                    "--http-token-env",
                    "TOKEN",
                    "--memory-config",
                    "memory.toml",
                    "--unknown",
                ],
                "unknown argument \"--unknown\"",
                0,
            ),
            (
                vec![
                    "--http-token-env",
                    "TOKEN",
                    "--memory-config",
                    "memory.toml",
                    "--router-config",
                    "router.json",
                ],
                "--http-token-env and --cors-origin require --http",
                0,
            ),
            (
                vec![
                    "--memory-config",
                    "memory.toml",
                    "--router-config",
                    "router.json",
                ],
                "cannot capture declarative MCP startup workspace",
                1,
            ),
            (
                vec![
                    "--memory-config",
                    "memory.toml",
                    "--cwd",
                    "/workspace",
                    "--router-config",
                    "router.json",
                ],
                "--memory-config cannot combine owner/legacy coordinator settings",
                0,
            ),
            (
                vec![
                    "--cwd",
                    "/workspace",
                    "--router-config",
                    "router.json",
                    "--library-config",
                    "library.json",
                ],
                "--cwd and --library require --memory-config",
                0,
            ),
            (
                vec![
                    "--router-config",
                    "router.json",
                    "--library-config",
                    "library.json",
                    "--capability-profile",
                    "curator",
                    "--allow-direct-feedback",
                ],
                "--router-config cannot be combined with --library-config, --db, --discovery-catalog, --discovery-catalog-writable, --capability-profile, or --allow-direct-feedback",
                0,
            ),
            (
                vec![
                    "--library-config",
                    "library.json",
                    "--capability-profile",
                    "curator",
                    "--allow-direct-feedback",
                ],
                "--library-config cannot be combined with --db, --discovery-catalog, --discovery-catalog-writable, --capability-profile, or --allow-direct-feedback",
                0,
            ),
        ] {
            let captures = Cell::new(0);
            let error = parse_with_cwd(argv, || {
                captures.set(captures.get() + 1);
                Err(std::io::Error::other("workspace unavailable"))
            })
            .unwrap_err();
            assert_eq!(error.to_string(), expected);
            assert_eq!(captures.get(), capture_count);
        }
    }

    #[test]
    fn repeatable_flags_keep_their_existing_last_wins_or_duplicate_error_rules() {
        let parsed = parse_args_from([
            "--db",
            "owner=first",
            "--db",
            "owner=second",
            "--http",
            "first",
            "--http",
            "not-yet-an-address",
            "--http-token-env",
            "FIRST",
            "--http-token-env",
            "LAST",
            "--cors-origin",
            "first",
            "--cors-origin",
            "not-yet-an-origin",
            "--allow-direct-feedback",
            "--allow-direct-feedback",
        ])
        .unwrap();
        assert_eq!(
            parsed.transport,
            Transport::Http {
                addr: "not-yet-an-address".into(),
                token_env: Some("LAST".into()),
                cors_origin: Some("not-yet-an-origin".into()),
            }
        );
        let StartupMode::Owner(owner) = parsed.mode else {
            panic!("owner mode expected")
        };
        assert_eq!(owner.profile(), CapabilityProfile::Operator);
        assert_eq!(owner.direct_feedback(), DirectFeedback::ImpliedOperator);
        // Name collisions are still a registry-admission concern, not silently
        // deduplicated or newly rejected by invocation parsing.
        assert_eq!(
            owner.into_databases(),
            vec![
                ("owner".into(), PathBuf::from("first")),
                ("owner".into(), PathBuf::from("second")),
            ]
        );

        for (flag, value) in [
            ("--activity-config", "activity.json"),
            ("--memory-config", "memory.toml"),
            ("--cwd", "/workspace"),
            ("--library", "named"),
            ("--router-config", "router.json"),
            ("--library-config", "library.json"),
            ("--capability-profile", "operator"),
        ] {
            // Duplicate detection precedes trying to consume the missing
            // second value, including flags invalid without a companion mode.
            assert_eq!(
                parse_args_from([flag, value, flag])
                    .unwrap_err()
                    .to_string(),
                format!("{flag} may be specified only once")
            );
        }
    }
}
