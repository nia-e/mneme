//! Ordinary project setup delegates to the shipped integration's existing owners.
use crate::AnyErr;
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::AsyncReadExt;
include!(concat!(env!("OUT_DIR"), "/init_bundle.rs"));

#[derive(Args)]
pub(crate) struct InitArgs {
    /// Project directory: absolute or parent-free relative path (default: current directory; no ..).
    #[arg(long)]
    root: Option<PathBuf>,
    /// Non-native setup only: recall/inspection without automatic recording; native policy controls recording.
    #[arg(long)]
    no_recording: bool,
    /// Non-native setup: leave generated project hooks pending manual Codex trust review.
    #[arg(long)]
    no_trust_hooks: bool,
    /// Explicit MCP runtime selector and installation source; omitted requires mneme-mcp on PATH.
    #[arg(long)]
    mcp_binary: Option<PathBuf>,
    /// Existing Codex executable for the librarian; defaults to PATH.
    #[arg(long)]
    codex_binary: Option<PathBuf>,
    /// Runtime Python selector: python3 or an absolute interpreter path; omitted discovers compatible Python on PATH.
    #[arg(long)]
    python: Option<PathBuf>,
    /// Explicit device-local registry config (never publishes to peers).
    #[arg(long)]
    library_config: Option<PathBuf>,
    /// Explicit named-store policy registry; does not enroll or create its stores.
    #[arg(long)]
    store_registry: Option<PathBuf>,
    /// Local owner port; otherwise choose a free loopback port for new setup.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1024..))]
    port: Option<u16>,
    #[command(flatten)]
    registration: crate::cli_registration::RegistrationArgs,
}

const SETUP_RESULT_BYTES: usize = 1024 * 1024;
const SETUP_RECOVERY: &str =
    "Setup may be partial. Fix the error, then rerun mnemed init. Keep existing memory data.";

/// The coordinator owns setup facts; native code owns subsequent registration.
/// Preserve extension fields so adding registration does not erase helper receipts.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct SetupResult {
    pub schema: String,
    pub status: String,
    pub stage: String,
    pub published: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl SetupResult {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            schema: "mneme.project-init.v1".into(),
            status: "incomplete".into(),
            stage: "setup_result".into(),
            published: false,
            extra: Map::from_iter([
                ("error".into(), Value::String(message.into())),
                ("recovery".into(), Value::String(SETUP_RECOVERY.into())),
            ]),
        }
    }

    fn text(&self, key: &str) -> &str {
        self.extra.get(key).and_then(Value::as_str).unwrap_or("")
    }

    fn human(&self) -> String {
        if self.status != "configured" {
            let mut output = format!(
                "Init incomplete: {}\n{}\n",
                self.text("error"),
                self.text("recovery")
            );
            if !self.text("receipt").is_empty() {
                output.push_str(&format!("Receipt: {}\n", self.text("receipt")));
            }
            return output;
        }
        let registration = self.extra.get("discovery_registration");
        let visible_registration = registration
            .and_then(|value| value.get("published"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let attempted_registration = registration
            .and_then(|value| value.get("hubs"))
            .and_then(Value::as_array)
            .is_some_and(|hubs| !hubs.is_empty());
        let publication = if self.published || visible_registration || attempted_registration {
            "discovery registration reported below"
        } else {
            "no publishing performed"
        };
        let mut output = format!(
            "Project memory {}: {}\n",
            self.text("store"),
            self.text("project_root")
        );
        if self.text("integration") == "native" {
            output.push_str(&format!(
                "Owner {} ready; recording: {} (native policy unchanged).\nDiscovery: {publication}.\nStart a new Codex session. Device hook execution is unverified; no project hooks installed.\n",
                self.text("db_id"), self.text("recording")
            ));
        } else {
            let local_registration =
                if self.extra.get("local_registration") == Some(&Value::Bool(true)) {
                    "locally registered"
                } else {
                    "not registered: existing exclusion/withdrawal preserved"
                };
            let trust = &self.extra["hook_trust"];
            let trust_status = trust["status"].as_str().unwrap_or("");
            output.push_str(&format!(
                "Owner {} ready; {local_registration} ({publication}).\nRecall hook: {}; recording: {}.\nHooks: {trust_status} ({} trusted/enabled); execution unverified.\n",
                self.text("db_id"), self.text("hippocampus"), self.text("recording"), trust["trusted"]
            ));
            if trust_status != "trusted" {
                output.push_str(&format!(
                    "Review hooks with Codex /hooks ({}).\n",
                    trust["reason"].as_str().unwrap_or("pending")
                ));
            }
        }
        if let Some(registration) = registration {
            if let Some(summary) = registration.get("summary").and_then(Value::as_str) {
                if summary.starts_with("Discovery registration") {
                    output.push_str(&format!("{summary}\n"));
                } else {
                    output.push_str(&format!("Discovery registration: {summary}\n"));
                }
            } else if let Some(status) = registration.get("status").and_then(Value::as_str) {
                output.push_str(&format!("Discovery registration: {status}.\n"));
            }
            if let Some(hubs) = registration.get("hubs").and_then(Value::as_array) {
                for hub in hubs.iter().take(64) {
                    let label = hub.get("hub").and_then(Value::as_str).unwrap_or("hub");
                    let status = hub
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("pending");
                    output.push_str(&format!("  {label}: {status}"));
                    let detail = hub
                        .get("detail")
                        .and_then(Value::as_str)
                        .or_else(|| hub.get("receipt")?.get("warning")?.as_str());
                    if let Some(detail) = detail {
                        // Native reports are already bounded; retain that
                        // property at the human presentation boundary too.
                        output.push_str(" — ");
                        output.extend(detail.chars().take(2048));
                    }
                    output.push('\n');
                }
            }
            if let Some(recovery) = registration.get("recovery").and_then(Value::as_str) {
                output.push_str(&format!("{recovery}\n"));
            }
        }
        output
    }

    fn render(&self, json: bool) -> Result<(), AnyErr> {
        if json {
            println!("{}", serde_json::to_string(self)?);
        } else if self.status == "configured" {
            print!("{}", self.human());
        } else {
            eprint!("{}", self.human());
        }
        Ok(())
    }
}

fn parse_setup_result(bytes: &[u8]) -> Result<SetupResult, AnyErr> {
    if bytes.len() > SETUP_RESULT_BYTES {
        return Err("project setup result exceeds the 1 MiB limit".into());
    }
    let result: SetupResult = serde_json::from_slice(bytes).map_err(
        |_| "project setup returned malformed JSON; no discovery registration attempted",
    )?;
    if result.schema != "mneme.project-init.v1"
        || !matches!(result.status.as_str(), "configured" | "incomplete")
        || result.stage.is_empty()
        || result.stage.len() > 128
    {
        return Err("project setup returned an unsupported result contract; no discovery registration attempted".into());
    }
    let required = if result.status == "configured" {
        &["store", "project_root", "db_id", "recording"][..]
    } else {
        &["error", "recovery"][..]
    };
    if required.iter().any(|key| result.text(key).is_empty()) {
        return Err("project setup result is missing required text fields; no discovery registration attempted".into());
    }
    if result.status == "configured" {
        if result.stage != "complete"
            || result
                .extra
                .get("integration")
                .is_some_and(|v| !v.is_string())
        {
            return Err("project setup returned an inconsistent success result; no discovery registration attempted".into());
        }
        if result.text("integration") != "native" {
            let trust = result.extra.get("hook_trust");
            if result
                .extra
                .get("local_registration")
                .and_then(Value::as_bool)
                .is_none()
                || result.text("hippocampus").is_empty()
                || trust
                    .and_then(|v| v.get("status"))
                    .and_then(Value::as_str)
                    .is_none()
                || trust
                    .and_then(|v| v.get("trusted"))
                    .and_then(Value::as_u64)
                    .is_none()
            {
                return Err("project setup returned invalid legacy setup fields; no discovery registration attempted".into());
            }
        }
    }
    Ok(result)
}

async fn capture_setup(
    command: &mut tokio::process::Command,
) -> Result<(ExitStatus, Vec<u8>), AnyErr> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or("project setup stdout unavailable")?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = stdout.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        if read > SETUP_RESULT_BYTES.saturating_sub(bytes.len()) {
            return Err(
                "project setup output exceeds the 1 MiB limit; no discovery registration attempted"
                    .into(),
            );
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok((child.wait().await?, bytes))
}

struct BundleDirectory {
    path: PathBuf,
    #[cfg(windows)]
    guard: mneme_store_path::windows::Directory,
}
impl Drop for BundleDirectory {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.guard.require_named().is_err() {
            // A renamed bundle is not authority to remove its replacement.
            return;
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
fn extract() -> Result<BundleDirectory, AnyErr> {
    let path = std::env::temp_dir().join(format!("mneme-init-{}", ulid::Ulid::new()));
    extract_at(path)
}

fn extract_at(path: PathBuf) -> Result<BundleDirectory, AnyErr> {
    #[cfg(windows)]
    let guard = mneme_store_path::windows::Directory::create(&path)?;
    #[cfg(not(windows))]
    {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
    }
    let bundle = BundleDirectory {
        path,
        #[cfg(windows)]
        guard,
    };
    #[cfg(windows)]
    let directories = ["codex", "library"]
        .into_iter()
        .map(|name| Ok((name, bundle.guard.create_directory(OsStr::new(name))?)))
        .collect::<Result<std::collections::BTreeMap<_, _>, std::io::Error>>()?;
    for (name, bytes) in BUNDLE {
        #[cfg(windows)]
        {
            use std::io::Write;
            let (parent, filename) = name.split_once('/').ok_or("invalid bundle path")?;
            let directory = directories.get(parent).ok_or("unknown bundle directory")?;
            // Creation is private at the NT call, not a chmod after exposure.
            directory
                .create_file(OsStr::new(filename))?
                .write_all(bytes)?;
        }
        #[cfg(not(windows))]
        {
            let target = bundle.path.join(name);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(target, bytes)?;
        }
    }
    Ok(bundle)
}

// Fits one maximum-length Windows UTF-16 path even after JSON ASCII escaping;
// also bounds the launcher's multi-interpreter inventory before parsing it.
const PYTHON_PROBE_BYTES: usize = 256 * 1024;
const PYTHON_PROBE: &str = "import json, sys; print(json.dumps({'version': list(sys.version_info[:2]), 'executable': sys.executable}, ensure_ascii=True))";

#[derive(Debug)]
struct PythonCandidate {
    executable: PathBuf,
    launcher: bool,
}

fn python_candidates(explicit: Option<&Path>, path: Option<&OsStr>) -> Vec<PythonCandidate> {
    let names: Vec<OsString> = match explicit {
        Some(value) => vec![value.as_os_str().to_owned()],
        None => {
            #[cfg(windows)]
            let names = [
                "python3.exe",
                "python.exe",
                "python3.14.exe",
                "python3.13.exe",
                "python3.12.exe",
                "python3.11.exe",
                "py.exe",
            ];
            #[cfg(not(windows))]
            let names = [
                "python3",
                "python3.14",
                "python3.13",
                "python3.12",
                "python3.11",
            ];
            names.into_iter().map(OsString::from).collect()
        }
    };
    let mut result = Vec::new();
    for name in names {
        let named = Path::new(&name);
        let launcher = cfg!(windows)
            && named
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|value| {
                    value.eq_ignore_ascii_case("py.exe") || value.eq_ignore_ascii_case("py")
                });
        let paths = if named.components().count() > 1 || named.is_absolute() {
            vec![named.to_path_buf()]
        } else {
            path.map(std::env::split_paths)
                .into_iter()
                .flatten()
                .map(|directory| {
                    let candidate = directory.join(named);
                    #[cfg(windows)]
                    let candidate = if candidate.extension().is_none() {
                        candidate.with_extension("exe")
                    } else {
                        candidate
                    };
                    candidate
                })
                .collect()
        };
        for executable in paths {
            // Use absolute paths so neither Windows' implicit current-dir
            // executable lookup nor a later cwd change picks another file.
            if let Ok(executable) = std::path::absolute(executable) {
                result.push(PythonCandidate {
                    executable,
                    launcher,
                });
            }
        }
    }
    result
}

fn admissible_python_executable(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // App Execution Aliases can open the Store instead of Python. Do not
        // execute reparse-point stubs (including an explicitly supplied alias).
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
        })
    }
    #[cfg(not(windows))]
    path.is_file()
}

async fn inspect_python_executable(path: &Path) -> Option<bool> {
    let path = path.to_path_buf();
    // PATH can include slow/unavailable network locations. Do not block the
    // async deadline on their metadata calls. At most one metadata worker can
    // remain pending when discovery times out; it never launches or writes.
    // Use a detached thread, not spawn_blocking: Tokio runtime shutdown would
    // otherwise wait indefinitely for that non-cancellable filesystem call.
    let (send, receive) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("mneme-python-path".to_owned())
        .spawn(move || {
            let result = match std::fs::symlink_metadata(&path) {
                Ok(_) => Some(admissible_python_executable(&path)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => Some(false),
            };
            let _ = send.send(result);
        })
        .ok()?;
    receive.await.ok().flatten()
}

fn parse_python_probe(bytes: &[u8]) -> Option<PathBuf> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Probe {
        version: [u64; 2],
        executable: PathBuf,
    }
    if bytes.len() > PYTHON_PROBE_BYTES {
        return None;
    }
    let probe: Probe = serde_json::from_slice(bytes).ok()?;
    if (probe.version[0], probe.version[1]) < (3, 11) || !probe.executable.is_absolute() {
        return None;
    }
    Some(probe.executable)
}

async fn read_python_command(executable: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = tokio::process::Command::new(executable);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // Discovery must not turn into installation. These cover the legacy
        // launcher and the newer Python Install Manager respectively.
        .env_remove("PYLAUNCHER_ALLOW_INSTALL")
        .env_remove("PYLAUNCHER_ALWAYS_INSTALL")
        .env_remove("PYLAUNCHER_DRYRUN")
        .env("PYTHON_MANAGER_AUTOMATIC_INSTALL", "false")
        .kill_on_drop(true);
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?.take((PYTHON_PROBE_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    stdout.read_to_end(&mut bytes).await.ok()?;
    if bytes.len() > PYTHON_PROBE_BYTES || !child.wait().await.ok()?.success() {
        return None;
    }
    Some(bytes)
}

async fn probe_python(candidate: &PythonCandidate) -> Option<PathBuf> {
    let bytes = read_python_command(
        &candidate.executable,
        &["-I", "-S", "-B", "-c", PYTHON_PROBE],
    )
    .await?;
    parse_python_probe(&bytes)
}

fn launcher_paths(bytes: &[u8]) -> Vec<PythonCandidate> {
    // `py -0p` is the installed-path listing supported by both the legacy
    // launcher and the Python Install Manager. It does not launch an interpreter
    // (whose descendants a timeout on py.exe could otherwise leave running).
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let start = line.as_bytes().windows(3).position(|bytes| {
                bytes[0].is_ascii_alphabetic()
                    && bytes[1] == b':'
                    && matches!(bytes[2], b'\\' | b'/')
            })?;
            if start > 0 && !matches!(line.as_bytes()[start - 1], b' ' | b'\t' | b'"') {
                return None;
            }
            Some(PythonCandidate {
                executable: PathBuf::from(line[start..].trim().trim_end_matches('"')),
                launcher: false,
            })
        })
        .collect()
}

async fn find_python(candidates: Vec<PythonCandidate>, explicit: bool) -> Option<PathBuf> {
    // Bound the aggregate wait; each failed/hung subprocess gets at most five
    // seconds. One non-cancellable read-only OS metadata thread may outlive the
    // deadline, but does not delay runtime shutdown or process exit.
    tokio::time::timeout(Duration::from_secs(20), async {
        for candidate in candidates {
            match inspect_python_executable(&candidate.executable).await {
                None => continue,
                Some(false) => {
                    if explicit {
                        return None;
                    }
                    continue;
                }
                Some(true) => {}
            }
            if candidate.launcher {
                if let Ok(Some(bytes)) = tokio::time::timeout(
                    Duration::from_secs(5),
                    read_python_command(&candidate.executable, &["-0p"]),
                )
                .await
                {
                    for interpreter in launcher_paths(&bytes) {
                        if inspect_python_executable(&interpreter.executable).await != Some(true) {
                            continue;
                        }
                        if let Ok(Some(python)) =
                            tokio::time::timeout(Duration::from_secs(5), probe_python(&interpreter))
                                .await
                        {
                            if inspect_python_executable(&python).await == Some(true) {
                                return Some(python);
                            }
                        }
                    }
                }
            } else if let Ok(Some(python)) =
                tokio::time::timeout(Duration::from_secs(5), probe_python(&candidate)).await
            {
                if inspect_python_executable(&python).await == Some(true) {
                    return Some(python);
                }
            }
            // An explicit bare name binds the first existing PATH entry, not
            // whichever later executable happens to satisfy the probe.
            if explicit {
                return None;
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

pub(crate) async fn discover_python(explicit: Option<&Path>) -> Option<PathBuf> {
    let path = std::env::var_os("PATH");
    find_python(
        python_candidates(explicit, path.as_deref()),
        explicit.is_some(),
    )
    .await
}

/// Native declarations select the setup route, not an admitted/active store.
/// Keep the logical user path (including managed symlinks); project-only
/// declarations instead use the nearest overlay witnessed by the shared reader.
fn native_setup_source(
    prepared: Option<mneme_config::WorkspaceDeclarations>,
) -> Result<Option<PathBuf>, AnyErr> {
    let Some(prepared) = prepared else {
        return Ok(None);
    };
    let source = if prepared.context.sources.iter().any(|s| s.role == "user") {
        prepared.request.user_path
    } else {
        prepared.context.project.overlay
    };
    source.map(Some).ok_or_else(|| {
        "native setup declarations have no configuration source; repair native configuration".into()
    })
}

pub(crate) async fn run(args: &InitArgs, json: bool) -> Result<(), AnyErr> {
    if !cfg!(feature = "cozo") {
        return refuse(
            "init needs cozo-enabled mnemed and mneme-mcp; install the full runtime. No project state changed",
            json,
        );
    }
    if args
        .python
        .as_deref()
        .is_some_and(|selector| !selector.is_absolute() && selector != Path::new("python3"))
    {
        return refuse(
            "--python must be python3 or an absolute interpreter selector; no project state changed",
            json,
        );
    }
    // Resolve relative CLI roots without canonicalizing away lexical privacy
    // boundaries; the shared workspace reader examines both ancestry chains.
    let root = match std::path::absolute(args.root.as_deref().unwrap_or(Path::new("."))) {
        Ok(root) => root,
        Err(error) => {
            return refuse(
                &format!(
                    "init cannot resolve its project directory: {error}; no project state changed"
                ),
                json,
            );
        }
    };
    // Parse present metadata before launching even the Python probe. Invalid
    // native declarations must never become permission for legacy setup.
    // Effective native selection/admission remains the coordinator's job.
    let memory_config = match mneme_config::prepare_workspace(None, &root, None)
        .map_err(AnyErr::from)
        .and_then(native_setup_source)
    {
        Ok(source) => source,
        Err(error) => {
            let root_hint = if root
                .components()
                .any(|part| part == std::path::Component::ParentDir)
            {
                "; use a parent-free --root path, or cd into the target before rerunning init"
            } else {
                ""
            };
            return refuse(
                &format!(
                    "init cannot prepare native configuration: {error}{root_hint}; no project state changed"
                ),
                json,
            );
        }
    };
    let python = discover_python(args.python.as_deref()).await;
    let Some(python) = python else {
        #[cfg(windows)]
        let hint = "install Python 3.11+ or pass --python PATH to the interpreter, not a Windows Store alias";
        #[cfg(not(windows))]
        let hint = "install Python 3.11+ or pass --python /absolute/path";
        let unavailable = match args.python.as_deref() {
            Some(path) => format!("cannot use Python 3.11+ at {}", path.display()),
            None => "Python 3.11+ not found on PATH".into(),
        };
        return refuse(
            &format!("{unavailable}; {hint}. No project state changed"),
            json,
        );
    };
    let bundle = match extract() {
        Ok(bundle) => bundle,
        Err(error) => {
            #[cfg(windows)]
            let hint = "set TEMP to a writable private directory on local NTFS";
            #[cfg(not(windows))]
            let hint = "set TMPDIR to a writable private directory";
            return refuse(
                &format!(
                    "cannot extract setup runtime: {error}; {hint}, then retry. No project state changed"
                ),
                json,
            );
        }
    };
    let executable = std::env::current_exe()?;
    let mut command = tokio::process::Command::new(&python);
    #[cfg(windows)]
    command.env("PYTHONUTF8", "1").args(["-X", "utf8"]);
    command
        .arg("-B")
        .arg(bundle.path.join("codex/project_setup.py"))
        .arg("--root")
        .arg(root)
        .arg("--mnemed")
        .arg(&executable)
        .arg("--library-helper")
        .arg(bundle.path.join("library/library.py"))
        .arg("--json")
        .kill_on_drop(true);
    if let Some(config) = memory_config {
        command.arg("--memory-config").arg(config);
    }
    if args.no_recording {
        command.arg("--no-recording");
    }
    if args.no_trust_hooks {
        command.arg("--no-trust-hooks");
    }
    for (flag, value) in [
        ("--python", &args.python),
        ("--mcp-binary", &args.mcp_binary),
        ("--codex-binary", &args.codex_binary),
        ("--library-config", &args.library_config),
        ("--store-registry", &args.store_registry),
    ] {
        if let Some(value) = value {
            command.arg(flag).arg(value);
        }
    }
    if let Some(port) = args.port {
        command.arg("--port").arg(port.to_string());
    }
    let captured = tokio::time::timeout(Duration::from_secs(180), capture_setup(&mut command))
        .await
        .map_err(|_| -> AnyErr { "project setup timed out".into() })
        .and_then(|result| result);
    let mut result = match captured {
        Ok((status, bytes)) => match parse_setup_result(&bytes) {
            Ok(result) if status.success() || result.status != "configured" => result,
            Ok(mut result) => {
                // A failing coordinator may still have useful setup receipts.
                // Keep them, but never let its success-shaped stdout authorize
                // a subsequent publication after an unsuccessful process exit.
                result.status = "incomplete".into();
                result.stage = "setup_result".into();
                result.extra.insert("error".into(), Value::String(
                    "project setup exited unsuccessfully after reporting success; no discovery registration attempted".into(),
                ));
                result
                    .extra
                    .insert("recovery".into(), Value::String(SETUP_RECOVERY.into()));
                result
            }
            Err(error) => SetupResult::failed(error.to_string()),
        },
        Err(error) => SetupResult::failed(error.to_string()),
    };
    if result.status == "configured" {
        crate::cli_registration::after_init(&args.registration, &mut result, json).await;
    }
    result.render(json)?;
    if result.status != "configured" {
        return Err(crate::cli_diagnostic::reported("project init incomplete"));
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
        return Err(crate::cli_diagnostic::reported(message.to_owned()));
    }
    Err(message.to_owned().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn setup_fixture(native: bool) -> Value {
        let mut value = serde_json::json!({
            "schema": "mneme.project-init.v1", "status": "configured", "stage": "complete",
            "published": false, "store": "adopted", "project_root": "/project café",
            "db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "recording": "off",
            "local_registration": true, "hippocampus": "reminder",
            "hook_trust": {"status": "pending", "trusted": 0, "reason": "manual"},
            "receipt": "/project café/receipt.json", "future_detail": {"preserved": true}
        });
        if native {
            value["integration"] = Value::String("native".into());
        }
        value
    }

    #[test]
    fn setup_result_roundtrip_preserves_receipts_and_extension_fields() {
        for native in [false, true] {
            let value = setup_fixture(native);
            let result = parse_setup_result(&serde_json::to_vec(&value).unwrap()).unwrap();
            assert_eq!(serde_json::to_value(&result).unwrap(), value);
            let human = result.human();
            assert!(human.starts_with("Project memory adopted: /project café\n"));
            assert!(human.contains("no publishing performed"));
            if native {
                assert!(human.contains("native policy unchanged"));
                assert!(human.contains("no project hooks installed"));
                assert!(human.contains("Start a new Codex session"));
                assert!(!human.contains("Review hooks"));
            } else {
                assert!(human.contains("locally registered"));
                assert!(
                    human.contains("Hooks: pending (0 trusted/enabled); execution unverified.")
                );
                assert!(human.contains("Review hooks with Codex /hooks (manual)"));
            }
        }
    }

    #[test]
    fn setup_result_refuses_malformed_wrong_types_and_false_success() {
        for bytes in [b"not json".as_slice(), b"{}", b"{}\n{}"] {
            assert!(parse_setup_result(bytes).is_err());
        }
        for (key, value) in [
            ("schema", Value::String("other".into())),
            ("published", Value::String("false".into())),
            ("status", Value::String("ready".into())),
            ("stage", Value::String("preflight".into())),
            ("store", Value::Bool(true)),
            ("project_root", Value::Null),
            ("db_id", Value::Number(3.into())),
            ("integration", Value::Bool(true)),
            ("local_registration", Value::String("true".into())),
            ("hook_trust", Value::String("trusted".into())),
        ] {
            let mut fixture = setup_fixture(false);
            fixture[key] = value;
            assert!(
                parse_setup_result(&serde_json::to_vec(&fixture).unwrap()).is_err(),
                "{key}"
            );
        }
        assert!(parse_setup_result(&vec![b' '; SETUP_RESULT_BYTES + 1]).is_err());
    }

    #[test]
    fn setup_failure_and_registration_have_one_actionable_rendering() {
        let result = SetupResult::failed("malformed helper result");
        let encoded = serde_json::to_vec(&result).unwrap();
        let decoded = parse_setup_result(&encoded).unwrap();
        assert_eq!(decoded.status, "incomplete");
        assert_eq!(
            decoded.human(),
            format!("Init incomplete: malformed helper result\n{SETUP_RECOVERY}\n")
        );

        let mut result =
            parse_setup_result(&serde_json::to_vec(&setup_fixture(true)).unwrap()).unwrap();
        result.extra.insert("discovery_registration".into(), serde_json::json!({
            "status": "partial", "published": true, "summary": "one hub added; one pending", "recovery": "Rerun mnemed init."
        }));
        let human = result.human();
        assert!(!human.contains("no publishing performed"));
        assert!(
            human.ends_with(
                "Discovery registration: one hub added; one pending\nRerun mnemed init.\n"
            )
        );
    }

    #[test]
    fn setup_human_registration_shows_each_hub_and_uncertain_publication() {
        let mut result =
            parse_setup_result(&serde_json::to_vec(&setup_fixture(true)).unwrap()).unwrap();
        result.extra.insert(
            "discovery_registration".into(),
            serde_json::json!({
                "status": "pending", "summary": "Discovery registration: 0/2 hubs confirmed.",
                "hubs": [
                    {"hub": "home", "status": "pending", "detail": "timeout; write may have completed"},
                    {"hub": "work", "status": "already_present", "receipt": {"warning": "durability unconfirmed"}}
                ],
                "recovery": "Rerun mnemed init."
            }),
        );
        let human = result.human();
        assert!(!human.contains("no publishing performed"));
        assert!(!human.contains("Discovery registration: Discovery registration"));
        assert!(human.contains("  home: pending — timeout; write may have completed\n"));
        assert!(human.contains("  work: already_present — durability unconfirmed\n"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn setup_capture_bounds_output_and_includes_process_wait_in_deadline() {
        let mut valid = tokio::process::Command::new("/bin/sh");
        valid.args(["-c", "printf 'fixture'; exit 7"]);
        let (status, bytes) = capture_setup(&mut valid).await.unwrap();
        assert_eq!(status.code(), Some(7));
        assert_eq!(bytes, b"fixture");

        let mut verbose = tokio::process::Command::new("/usr/bin/yes");
        verbose.arg("x");
        let error = tokio::time::timeout(Duration::from_secs(2), capture_setup(&mut verbose))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("1 MiB"));

        // EOF is not completion: a helper can close stdout and continue running.
        let mut hung = tokio::process::Command::new("/bin/sh");
        hung.args(["-c", "exec 1>&-; exec /bin/sleep 10"]);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), capture_setup(&mut hung))
                .await
                .is_err()
        );
    }

    fn unicode_bundle() -> BundleDirectory {
        extract_at(
            std::env::temp_dir().join(format!("mneme init café 日本語 {}", ulid::Ulid::new())),
        )
        .unwrap()
    }

    #[test]
    fn native_setup_source_is_presence_not_store_activation() {
        assert!(native_setup_source(None).unwrap().is_none());
        let bundle = unicode_bundle();
        let user = bundle.path.join("user.toml");
        // Empty declarations still require native setup; they are not absence.
        std::fs::write(&user, "schema='mneme.config.v1'").unwrap();
        let prepared = mneme_config::prepare_workspace(Some(&user), &bundle.path, None)
            .unwrap()
            .unwrap();
        assert!(prepared.local.stores.is_empty());
        assert_eq!(native_setup_source(Some(prepared)).unwrap(), Some(user));
    }

    #[test]
    fn native_setup_source_uses_overlay_only_without_an_actual_user_source() {
        let bundle = unicode_bundle();
        let user = bundle.path.join("user.toml");
        std::fs::write(&user, "schema='mneme.config.v1'").unwrap();
        let mut prepared = mneme_config::prepare_workspace(Some(&user), &bundle.path, None)
            .unwrap()
            .unwrap();
        let overlay = bundle.path.join(".mneme/memory.toml");
        prepared.context.project.overlay = Some(overlay.clone());
        assert_eq!(
            native_setup_source(Some(prepared.clone())).unwrap(),
            Some(user)
        );
        // Discover requests freeze even an absent user path. It cannot take
        // precedence over a project source unless it was actually read.
        prepared.context.sources.retain(|s| s.role != "user");
        assert_eq!(
            native_setup_source(Some(prepared.clone())).unwrap(),
            Some(overlay)
        );
        prepared.context.project.overlay = None;
        assert!(native_setup_source(Some(prepared)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn native_setup_source_preserves_logical_user_symlink() {
        let bundle = unicode_bundle();
        let target = bundle.path.join("managed.toml");
        let logical = bundle.path.join("memory.toml");
        std::fs::write(&target, "schema='mneme.config.v1'").unwrap();
        std::os::unix::fs::symlink(&target, &logical).unwrap();
        let prepared = mneme_config::prepare_workspace(Some(&logical), &bundle.path, None)
            .unwrap()
            .unwrap();
        assert!(
            prepared
                .context
                .sources
                .iter()
                .any(|s| s.path == target.canonicalize().unwrap())
        );
        assert_eq!(native_setup_source(Some(prepared)).unwrap(), Some(logical));
    }

    #[test]
    fn extracted_bundle_is_exact_complete_and_private() {
        let bundle = unicode_bundle();
        let root = bundle.path.clone();
        let mut inventory = BTreeSet::new();
        for directory in std::fs::read_dir(&root).unwrap() {
            let directory = directory.unwrap();
            assert!(directory.file_type().unwrap().is_dir());
            for entry in std::fs::read_dir(directory.path()).unwrap() {
                let entry = entry.unwrap();
                assert!(entry.file_type().unwrap().is_file());
                inventory.insert(format!(
                    "{}/{}",
                    directory.file_name().to_str().unwrap(),
                    entry.file_name().to_str().unwrap()
                ));
            }
        }
        assert_eq!(
            inventory,
            BUNDLE.iter().map(|(name, _)| (*name).to_owned()).collect()
        );
        let installer = std::str::from_utf8(
            BUNDLE
                .iter()
                .find(|(name, _)| *name == "codex/install.py")
                .unwrap()
                .1,
        )
        .unwrap();
        let programs = installer
            .split_once("PROGRAMS = (")
            .unwrap()
            .1
            .split_once(')')
            .unwrap()
            .0;
        let mut expected = BTreeSet::from([
            "codex/install.py".to_owned(),
            "codex/project_setup.py".to_owned(),
            "codex/hook_trust.py".to_owned(),
            "library/library.py".to_owned(),
            "library/cache.py".to_owned(),
            "library/publication_policy.py".to_owned(),
            "library/host_process.py".to_owned(),
        ]);
        for name in programs
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            expected.insert(format!("codex/{}", name.trim_matches('"')));
        }
        assert_eq!(inventory, expected);
        for helper in [
            "host_files.py",
            "host_process.py",
            "host_platform.py",
            "store_registry.py",
            "registry_config.py",
            "registry_runtime.py",
            "registry_reader.py",
            "registry_recording.py",
            "core_hook.py",
            "assessment.py",
            "packet_accounting.py",
            "capture_source.py",
            "claude_observer.py",
            "claude_runtime.py",
            "claude_config.py",
            "claude_hooks.py",
            "review_contract.py",
            "stable_hooks.py",
            "session_activity.py",
        ] {
            assert!(
                inventory.contains(&format!("codex/{helper}")),
                "missing runtime helper: {helper}"
            );
        }
        for (name, bytes) in BUNDLE {
            assert_eq!(std::fs::read(root.join(name)).unwrap(), *bytes);
        }
        assert_eq!(
            std::fs::read(root.join("library/host_process.py")).unwrap(),
            std::fs::read(root.join("codex/host_process.py")).unwrap(),
            "standalone library dependency must match its one maintained source"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        #[cfg(windows)]
        {
            bundle.guard.require_private().unwrap();
            for (name, _) in BUNDLE {
                let (parent, file) = name.split_once('/').unwrap();
                let parent = bundle.guard.open_directory(OsStr::new(parent)).unwrap();
                parent.require_private().unwrap();
                let file = parent.open_file(OsStr::new(file), false).unwrap();
                mneme_store_path::windows::require_private_file(&file).unwrap();
            }
        }
        assert!(
            extract_at(root.clone()).is_err(),
            "existing bundle must not be clobbered"
        );
        assert_eq!(std::fs::read(root.join(BUNDLE[0].0)).unwrap(), BUNDLE[0].1);
        drop(bundle);
        assert!(!root.exists(), "temporary bundle cleanup");
    }

    #[tokio::test]
    async fn extracted_stable_hook_helper_imports_and_help_without_source_checkout() {
        let Some(python) = discover_python(None).await else {
            eprintln!("Python 3.11+ unavailable; stable hook import smoke not exercised");
            return;
        };
        let bundle = unicode_bundle();
        let codex = bundle.path.join("codex");
        let script = "import pathlib, sys; root = pathlib.Path(sys.argv[1]); sys.path.insert(0, str(root)); import stable_hooks, registry_config; assert pathlib.Path(stable_hooks.__file__) == root / 'stable_hooks.py'; assert pathlib.Path(registry_config.__file__) == root / 'registry_config.py'; print('stable-hook-import-ok')";
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            read_python_command(
                &python,
                &["-I", "-S", "-B", "-c", script, codex.to_str().unwrap()],
            ),
        )
        .await
        .unwrap()
        .expect("embedded stable hook helper dependencies");
        assert_eq!(
            std::str::from_utf8(&result).unwrap().trim_end(),
            "stable-hook-import-ok"
        );
        let helper = codex.join("stable_hooks.py");
        let help = tokio::time::timeout(
            Duration::from_secs(5),
            read_python_command(
                &python,
                &["-E", "-S", "-B", helper.to_str().unwrap(), "--help"],
            ),
        )
        .await
        .unwrap()
        .expect("embedded stable hook helper CLI");
        assert!(
            std::str::from_utf8(&help)
                .unwrap()
                .contains("{prepare,install,select}")
        );
    }

    #[tokio::test]
    async fn extracted_session_activity_helper_imports_without_source_checkout() {
        let Some(python) = discover_python(None).await else {
            eprintln!("Python 3.11+ unavailable; activity import smoke not exercised");
            return;
        };
        let bundle = unicode_bundle();
        let codex = bundle.path.join("codex");
        let script = "import pathlib, sys; root = pathlib.Path(sys.argv[1]); sys.path.insert(0, str(root)); import session_activity, registry_config; assert pathlib.Path(session_activity.__file__) == root / 'session_activity.py'; assert callable(session_activity.emit); assert callable(registry_config.prepare_activity); print('activity-import-ok')";
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            read_python_command(
                &python,
                &["-I", "-S", "-B", "-c", script, codex.to_str().unwrap()],
            ),
        )
        .await
        .unwrap()
        .expect("embedded activity helper dependencies");
        assert_eq!(
            std::str::from_utf8(&result).unwrap().trim_end(),
            "activity-import-ok"
        );
    }

    #[tokio::test]
    async fn extracted_library_publication_helper_imports_without_source_checkout() {
        let Some(python) = discover_python(None).await else {
            eprintln!("Python 3.11+ unavailable; library import smoke not exercised");
            return;
        };
        let bundle = unicode_bundle();
        let library = bundle.path.join("library");
        let native = bundle.path.join("uncontacted-native-cli");
        let config = bundle.path.join("unread-memory.toml");
        let script = "import sys; sys.path.insert(0, sys.argv[1]); import publication_policy, host_process; publication_policy.NativePublicationPolicy(sys.argv[2], sys.argv[3]); print('library-publication-import-ok')";
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            read_python_command(
                &python,
                &[
                    "-I",
                    "-S",
                    "-B",
                    "-c",
                    script,
                    library.to_str().unwrap(),
                    native.to_str().unwrap(),
                    config.to_str().unwrap(),
                ],
            ),
        )
        .await
        .unwrap()
        .expect("embedded standalone publication helper dependencies");
        assert_eq!(
            std::str::from_utf8(&result).unwrap().trim_end(),
            "library-publication-import-ok"
        );
        assert!(
            !native.exists(),
            "metadata constructor must not contact/start its native CLI"
        );
        assert!(
            !config.exists(),
            "metadata constructor must not create its configuration"
        );
    }

    #[test]
    fn python_probe_contract_preserves_unicode_and_refuses_bad_versions_or_shapes() {
        let executable = std::env::temp_dir().join("Python café 日本語/python.exe");
        let valid = serde_json::json!({"version": [3, 11], "executable": executable});
        assert_eq!(
            parse_python_probe(&serde_json::to_vec(&valid).unwrap()),
            Some(executable)
        );
        for invalid in [
            serde_json::json!({"version": [3, 10], "executable": "/python"}),
            serde_json::json!({"version": [3, 11, 0], "executable": "/python"}),
            serde_json::json!({"version": ["3", "11"], "executable": "/python"}),
            serde_json::json!({"version": [3, 11], "executable": "python"}),
            serde_json::json!({"version": [3, 11], "executable": "/python", "extra": true}),
        ] {
            assert!(parse_python_probe(&serde_json::to_vec(&invalid).unwrap()).is_none());
        }
        assert!(parse_python_probe(&vec![b' '; PYTHON_PROBE_BYTES + 1]).is_none());
        assert!(parse_python_probe(b"not Python").is_none());
    }

    #[tokio::test]
    async fn explicit_python_never_falls_through_to_another_candidate() {
        let bundle = unicode_bundle();
        let executable = bundle.path.join(if cfg!(windows) {
            "python.exe"
        } else {
            "python3"
        });
        std::fs::write(&executable, b"fixture").unwrap();
        let path = std::env::join_paths([&bundle.path]).unwrap();
        let candidates = python_candidates(Some(&executable), Some(&path));
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].executable, executable);
        assert!(!candidates[0].launcher);
        assert!(
            find_python(
                python_candidates(Some(&bundle.path.join("missing-python")), Some(&path)),
                true
            )
            .await
            .is_none()
        );
        assert!(
            python_candidates(None, Some(&path))
                .iter()
                .any(|candidate| candidate.executable == executable)
        );
    }

    #[cfg(unix)]
    fn script(bundle: &BundleDirectory, name: &str, body: &str) -> PythonCandidate {
        use std::os::unix::fs::PermissionsExt;
        let executable = bundle.path.join(name);
        std::fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        PythonCandidate {
            executable,
            launcher: false,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn python_probe_binds_reported_interpreter_and_bounds_output_and_time() {
        let bundle = unicode_bundle();
        let actual = std::env::current_exe().unwrap();
        let payload = serde_json::json!({"version": [3, 11], "executable": actual}).to_string();
        let candidate = script(
            &bundle,
            "python café",
            &format!("printf '%s\\n' {}", shlex::try_quote(&payload).unwrap()),
        );
        assert_eq!(probe_python(&candidate).await, Some(actual));
        let verbose = script(&bundle, "verbose", "exec /usr/bin/yes x");
        assert!(
            tokio::time::timeout(Duration::from_secs(2), probe_python(&verbose))
                .await
                .unwrap()
                .is_none()
        );
        let hung = script(&bundle, "hung", "exec /bin/sleep 10");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), probe_python(&hung))
                .await
                .is_err()
        );
        let old = unicode_bundle();
        let new = unicode_bundle();
        script(&old, "python3", "exit 1");
        script(
            &new,
            "python3",
            &format!("printf '%s\\n' {}", shlex::try_quote(&payload).unwrap()),
        );
        let path = std::env::join_paths([&old.path, &new.path]).unwrap();
        assert!(
            find_python(
                python_candidates(Some(Path::new("python3")), Some(&path)),
                true
            )
            .await
            .is_none()
        );
        assert_eq!(
            find_python(python_candidates(None, Some(&path)), false).await,
            Some(std::env::current_exe().unwrap())
        );
    }

    #[test]
    fn launcher_inventory_preserves_paths_and_ignores_headers() {
        let paths = launcher_paths("Installed Pythons found by py Launcher\n -V:3.13 * C:\\Users\\Nia café\\Python313\\python.exe\n -3.11-64  C:\\Python311\\python.exe\n".as_bytes());
        assert_eq!(paths.len(), 2);
        assert_eq!(
            paths[0].executable,
            PathBuf::from(r"C:\Users\Nia café\Python313\python.exe")
        );
        assert_eq!(
            paths[1].executable,
            PathBuf::from(r"C:\Python311\python.exe")
        );
        assert!(paths.iter().all(|candidate| !candidate.launcher));
    }

    #[cfg(windows)]
    #[test]
    fn windows_launcher_is_resolved_explicitly_and_reparse_aliases_are_refused() {
        let bundle = unicode_bundle();
        let launcher = bundle.path.join("py.exe");
        std::fs::write(&launcher, b"fixture").unwrap();
        let path = std::env::join_paths([&bundle.path]).unwrap();
        let candidates = python_candidates(Some(Path::new("py")), Some(&path));
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].launcher);
        assert_eq!(candidates[0].executable, launcher);
        // symlink creation may require Developer Mode; the production check is
        // unconditional even where the test environment cannot create one.
        let alias = bundle.path.join("python.exe");
        if std::os::windows::fs::symlink_file(&launcher, &alias).is_ok() {
            assert!(!admissible_python_executable(&alias));
            assert!(!admissible_python_executable(
                &python_candidates(Some(&alias), Some(&path))[0].executable
            ));
        }
    }
}
