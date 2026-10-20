//! Ordinary project setup delegates to the shipped integration's existing owners.
use crate::AnyErr;
use clap::Args;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncReadExt;
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
    /// Python 3.11+ executable; otherwise discover Python (or Windows py) on PATH.
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

pub(crate) async fn run(args: &InitArgs, json: bool) -> Result<(), AnyErr> {
    if !cfg!(feature = "cozo") {
        return refuse(
            "init requires a cozo-enabled mnemed and mneme-mcp; no project state changed",
            json,
        );
    }
    let python = discover_python(args.python.as_deref()).await;
    let Some(python) = python else {
        return refuse(
            "init needs Python 3.11+; install it explicitly or pass --python PATH to the actual interpreter (not a Windows Store alias); no project state changed",
            json,
        );
    };
    let bundle = match extract() {
        Ok(bundle) => bundle,
        Err(error) => {
            return refuse(
                &format!(
                    "init could not extract its private runtime bundle: {error}; use a writable private temporary directory (owned local NTFS on Windows), then retry; no project state changed"
                ),
                json,
            );
        }
    };
    let executable = std::env::current_exe()?;
    let root = args.root.clone().unwrap_or(std::env::current_dir()?);
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
        ("--store-registry", &args.store_registry),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn unicode_bundle() -> BundleDirectory {
        extract_at(
            std::env::temp_dir().join(format!("mneme init café 日本語 {}", ulid::Ulid::new())),
        )
        .unwrap()
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
            "capture_source.py",
            "claude_observer.py",
            "claude_runtime.py",
            "claude_config.py",
            "claude_hooks.py",
            "review_contract.py",
            "stable_hooks.py",
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
