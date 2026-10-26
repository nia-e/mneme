//! The activity frontend is local and independent of graph-owner admission.
use mneme_app::session_activity::{self, ActivityRequest, MAX_OUTPUT_BYTES};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    manifest: PathBuf,
    activity: PathBuf,
    listener: TcpListener,
}

impl Fixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let root = std::env::temp_dir().join(format!("mneme-cli-activity-{}", ulid::Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let workspace = root.join("workspace");
        let activity = root.join("activity");
        let config = root.join("config/mneme");
        for path in [&workspace, &activity, &config, &root.join("bin")] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::create_dir(workspace.join(".git")).unwrap();
        // If the ordinary configured route is reached, it has a real discovery
        // seed. The listener must see no connection, including on refusals.
        std::fs::write(
            config.join("memory.toml"),
            format!(
                "schema='mneme.config.v1'\nread_default='tripwire'\n[stores.tripwire]\nid='01ARZ3NDEKTSV4RRFFQ69G5FAV'\nowner={{url='http://{}',db='native'}}\ndiscovery=true\nactive={{workspace='any'}}\n",
                listener.local_addr().unwrap()
            ),
        )
        .unwrap();
        std::fs::write(workspace.join("owner-sentinel"), "unchanged graph sentinel").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["git", "ssh", "python", "python3", "codex"] {
                let path = root.join("bin").join(name);
                std::fs::write(
                    &path,
                    "#!/bin/sh\nprintf called > \"$HOME/subprocess-called\"\nexit 99\n",
                )
                .unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        let manifest = config.join("activity.json");
        Self {
            root,
            workspace,
            manifest,
            activity,
            listener,
        }
    }

    fn configure(&self) {
        std::fs::write(
            &self.manifest,
            serde_json::to_vec(&json!({
                "schema": "mneme.session-activity.config.v1",
                "root": self.activity,
                "workspaces": [{"path": self.workspace, "source": "codex"}],
                "device": null,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn record(&self, id: &str, at: u64) {
        let path = self
            .activity
            .join(format!("codex-{:x}.json", Sha256::digest(id.as_bytes())));
        let mut record: Value = serde_json::from_str(include_str!(
            "../../../integrations/codex/fixtures/session-activity.json"
        ))
        .unwrap();
        let original = record["first_observed"].as_u64().unwrap();
        record["session_id"] = json!(id);
        record["workspace"] = json!(self.workspace);
        record["cwd"] = json!(self.workspace);
        record["first_observed"] = json!(at);
        record["last_observed"] = json!(at + 10);
        for event in record["events"].as_array_mut().unwrap() {
            event["at"] = json!(at + event["at"].as_u64().unwrap() - original);
        }
        std::fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mnemed"));
        command
            .env_clear()
            .current_dir(&self.workspace)
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("CODEX_HOME", self.root.join("poison-codex"))
            .env("PWD", self.root.join("not-the-workspace"))
            .env("PATH", self.root.join("bin"));
        #[cfg(windows)]
        for key in ["SYSTEMROOT", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
    }

    fn run(&self, command: &mut Command) -> Output {
        let before = inventory(&self.root);
        let mut child = command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if start.elapsed() > Duration::from_secs(10) {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!("activity command timed out: {output:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            before,
            inventory(&self.root),
            "reporting changed local files"
        );
        assert_eq!(
            self.listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "reporting contacted the owner/discovery tripwire"
        );
        assert!(output.stdout.len() <= MAX_OUTPUT_BYTES);
        output
    }

    fn report(&self, args: &[&str]) -> Value {
        let output = self.run(self.command().args(["--json", "activity"]).args(args));
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn inventory(root: &Path) -> BTreeMap<PathBuf, String> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, String>) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        let value = if metadata.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), entries);
            }
            "directory".into()
        } else if metadata.file_type().is_symlink() {
            format!("symlink:{:?}", std::fs::read_link(path).unwrap())
        } else {
            format!("file:{:x}", Sha256::digest(std::fs::read(path).unwrap()))
        };
        entries.insert(path.strip_prefix(root).unwrap().to_owned(), value);
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

#[test]
fn missing_manifest_is_honest_and_does_not_start_owner_discovery() {
    let fixture = Fixture::new();
    let report = fixture.report(&[]);
    assert_eq!(report["status"], "not_configured");
    assert!(report["sessions"].as_array().unwrap().is_empty());
    let human = fixture.run(fixture.command().arg("activity"));
    assert!(human.status.success(), "{human:?}");
    let expected = session_activity::report(
        &fixture.manifest,
        &fixture.workspace,
        ActivityRequest::default(),
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(human.stdout).unwrap(),
        session_activity::render_human(&expected)
    );
}

#[test]
fn chooser_and_selected_details_use_the_shared_report_in_both_output_modes() {
    let fixture = Fixture::new();
    fixture.configure();
    fixture.record("a17c9e-first-host-session", 1_792_770_000);
    fixture.record("b09c2e-later-host-session", 1_792_770_100);
    assert_eq!(fixture.report(&[])["sessions"].as_array().unwrap().len(), 2);
    for selector in [
        "a17c9e",
        "a17c9e-first-host-session",
        "codex:a17c9e-first-host-session",
    ] {
        let expected = session_activity::report(
            &fixture.manifest,
            &fixture.workspace,
            ActivityRequest {
                session: Some(selector.into()),
                details: true,
            },
        )
        .unwrap();
        let report = fixture.report(&["--session", selector, "--details"]);
        assert_eq!(report, serde_json::to_value(&expected).unwrap());
        assert_eq!(report["sessions"].as_array().unwrap().len(), 1);
        let human =
            fixture.run(
                fixture
                    .command()
                    .args(["activity", "--session", selector, "--details"]),
            );
        assert!(human.status.success(), "{human:?}");
        assert_eq!(
            String::from_utf8(human.stdout).unwrap(),
            session_activity::render_human(&expected)
        );
    }
}

#[test]
fn explicit_manifest_and_process_cwd_do_not_use_producer_env_or_pwd() {
    let fixture = Fixture::new();
    fixture.configure();
    fixture.record("fixture-session", 1_792_770_000);
    let explicit = fixture.root.join("explicit.json");
    std::fs::rename(&fixture.manifest, &explicit).unwrap();
    let ignored = fixture.run(
        fixture
            .command()
            .env("MNEME_ACTIVITY_CONFIG", &explicit)
            .args(["activity", "--json"]),
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&ignored.stdout).unwrap()["status"],
        "not_configured"
    );
    let subdir = fixture.workspace.join("src");
    std::fs::create_dir(&subdir).unwrap();
    let output = fixture.run(
        fixture
            .command()
            .current_dir(&subdir)
            .args(["activity", "--json", "--activity-config"])
            .arg(&explicit),
    );
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["sessions"].as_array().unwrap().len(), 1);
}

#[test]
fn every_owner_selector_is_refused_before_manifest_or_owner_access() {
    let fixture = Fixture::new();
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
            let mut command = fixture.command();
            command.arg("--json");
            if !before {
                command.arg("activity");
            }
            command.args(&selector);
            if before {
                command.arg("activity");
            }
            command.args(["--activity-config", "does-not-exist.json"]);
            let output = fixture.run(&mut command);
            assert!(!output.status.success(), "{selector:?}");
            let error: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(error["code"], "unsupported_selectors");
            assert!(output.stderr.is_empty());
        }
    }
    for value in ["ambient.db", ""] {
        let output = fixture.run(
            fixture
                .command()
                .env("MNEME_DB", value)
                .args(["--json", "activity"]),
        );
        assert!(!output.status.success());
        if value.is_empty() {
            // Clap treats a present-but-empty MNEME_DB as a missing --db value,
            // rather than silently discarding the ambient selector.
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8_lossy(&output.stderr).contains("--db"));
        } else {
            let error: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(error["code"], "unsupported_selectors");
            assert!(error["error"].as_str().unwrap().contains("MNEME_DB"));
            assert!(output.stderr.is_empty());
        }
    }
}

#[test]
fn unreported_errors_use_display_once_and_leave_json_stdout_empty() {
    let fixture = Fixture::new();
    for json in [false, true] {
        let mut command = fixture.command();
        if json {
            command.arg("--json");
        }
        // Rejected before owner configuration, on both output paths. The outer
        // renderer must not invent a second JSON schema or Debug-format String.
        command.args(["--db", "unused", "config", "inspect"]);
        let output = fixture.run(&mut command);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "error: config commands use their own --config/--cwd selectors, not database or global owner selectors\n"
        );
    }
}

#[test]
fn human_activity_failure_keeps_the_code_and_cause_once() {
    let fixture = Fixture::new();
    let output = fixture.run(fixture.command().args(["--db", "unused", "activity"]));
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("error: unsupported_selectors: activity is local;"));
    assert_eq!(stderr.lines().count(), 1);
    assert_eq!(stderr.matches("MNEME_DB").count(), 1);
    assert!(stderr.contains("--activity-config PATH"));
}

#[test]
fn unsupported_graph_and_global_scope_arguments_are_parser_refusals() {
    let fixture = Fixture::new();
    for option in [
        "--global", "--graph", "--db-id", "--query", "--body", "--cwd",
    ] {
        let output = fixture.run(fixture.command().args(["activity", option]));
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn invalid_session_is_bounded_and_does_not_echo_arbitrary_input() {
    let fixture = Fixture::new();
    let session = "PRIVATE-UNTRUSTED-INPUT".repeat(30);
    let output = fixture.run(
        fixture
            .command()
            .args(["activity", "--json", "--session", &session]),
    );
    assert!(!output.status.success());
    let expected = session_activity::report(
        &fixture.manifest,
        &fixture.workspace,
        ActivityRequest {
            session: Some(session.clone()),
            details: false,
        },
    )
    .unwrap_err();
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["code"], expected.code());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE-UNTRUSTED"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE-UNTRUSTED"));
}

#[test]
fn malformed_activity_manifest_is_local_and_does_not_echo_its_contents() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.manifest, "private-payload-not-json").unwrap();
    let report = fixture.report(&[]);
    assert_eq!(report["status"], "invalid_manifest");
    let human = fixture.run(fixture.command().arg("activity"));
    assert!(human.status.success(), "{human:?}");
    assert!(!String::from_utf8_lossy(&human.stdout).contains("private-payload"));
    assert!(!String::from_utf8_lossy(&human.stderr).contains("private-payload"));
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains("private-payload")
    );
}
