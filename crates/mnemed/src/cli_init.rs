//! Ordinary project setup delegates to the shipped integration's existing owners.
use crate::AnyErr;
use clap::Args;
use std::path::PathBuf;
use std::time::Duration;
include!(concat!(env!("OUT_DIR"), "/init_bundle.rs"));

#[derive(Args)]
pub(crate) struct InitArgs {
    /// Project directory (default: current directory). Never selects the user store.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Set up recall/inspection without automatic session recording.
    #[arg(long)]
    no_recording: bool,
    /// Leave the installed project hooks pending manual Codex trust review.
    #[arg(long)]
    no_trust_hooks: bool,
    /// Existing MCP executable; defaults to the sibling of this mnemed, then PATH.
    #[arg(long)]
    mcp_binary: Option<PathBuf>,
    /// Existing Codex executable for the librarian; defaults to PATH.
    #[arg(long)]
    codex_binary: Option<PathBuf>,
    /// Python 3.11+ executable; auto-discovers python3 or versioned Python on PATH.
    #[arg(long)]
    python: Option<PathBuf>,
    /// Explicit device-local registry config (never publishes to peers).
    #[arg(long)]
    library_config: Option<PathBuf>,
    /// Local owner port; otherwise choose a free loopback port for new setup.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1024..))]
    port: Option<u16>,
}

struct BundleDirectory(PathBuf);
impl Drop for BundleDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn extract() -> Result<BundleDirectory, AnyErr> {
    let path = std::env::temp_dir().join(format!("mneme-init-{}", ulid::Ulid::new()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&path)?;
    let bundle = BundleDirectory(path);
    for (name, bytes) in BUNDLE {
        let target = bundle.0.join(name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, bytes)?;
    }
    Ok(bundle)
}

pub(crate) async fn run(args: &InitArgs, json: bool) -> Result<(), AnyErr> {
    if !cfg!(feature = "cozo") {
        return refuse(
            "init requires a cozo-enabled mnemed and mneme-mcp; no project state changed",
            json,
        );
    }
    let candidates = match &args.python {
        Some(explicit) => vec![explicit.clone()],
        None => [
            "python3",
            "python3.14",
            "python3.13",
            "python3.12",
            "python3.11",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect(),
    };
    let mut python = None;
    for candidate in candidates {
        let mut probe = tokio::process::Command::new(&candidate);
        probe
            .args([
                "-c",
                "import sys; sys.exit(0 if sys.version_info >= (3,11) else 1)",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let probe = tokio::time::timeout(Duration::from_secs(5), probe.status()).await;
        if matches!(probe, Ok(Ok(status)) if status.success()) {
            python = Some(candidate);
            break;
        }
    }
    let Some(python) = python else {
        return refuse(
            "init needs Python 3.11+; install it explicitly or pass --python PATH; no project state changed",
            json,
        );
    };
    let bundle = extract()?;
    let executable = std::env::current_exe()?;
    let root = args.root.clone().unwrap_or(std::env::current_dir()?);
    let mut command = tokio::process::Command::new(&python);
    command
        .arg(bundle.0.join("codex/project_setup.py"))
        .arg("--root")
        .arg(root)
        .arg("--mnemed")
        .arg(&executable)
        .arg("--library-helper")
        .arg(bundle.0.join("library/library.py"))
        .kill_on_drop(true);
    if json {
        command.arg("--json");
    }
    if args.no_recording {
        command.arg("--no-recording");
    }
    if args.no_trust_hooks {
        command.arg("--no-trust-hooks");
    }
    for (flag, value) in [
        ("--mcp-binary", &args.mcp_binary),
        ("--codex-binary", &args.codex_binary),
        ("--library-config", &args.library_config),
    ] {
        if let Some(value) = value {
            command.arg(flag).arg(value);
        }
    }
    if let Some(port) = args.port {
        command.arg("--port").arg(port.to_string());
    }
    let status = tokio::time::timeout(Duration::from_secs(180), command.status()).await
        .map_err(|_| "project init timed out; retained state is recoverable with init again or the reported installer receipt; no cleanup of memory data attempted")??;
    if !status.success() {
        return Err("project init incomplete; see setup result and recovery action above".into());
    }
    Ok(())
}

/// Setup refusals have the same deterministic schema as Python coordination.
pub(crate) fn refuse(message: &str, json: bool) -> Result<(), AnyErr> {
    if json {
        println!(
            "{}",
            serde_json::json!({"schema":"mneme.project-init.v1", "status":"incomplete", "stage":"preflight", "published":false, "error":message, "recovery":"Resolve the reported prerequisite/selector, then rerun mnemed init."})
        );
    }
    Err(message.to_owned().into())
}
