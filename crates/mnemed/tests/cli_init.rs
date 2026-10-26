//! Setup parser/refusals never use the user's HOME, configuration or stores.
use serde_json::Value;
use std::{
    path::PathBuf,
    process::{Command, Output},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("mneme-init-cli-{}", ulid::Ulid::new()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        self.run_at(&self.0, args)
    }
    fn run_at(&self, cwd: &std::path::Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mnemed"))
            .args(args)
            .current_dir(cwd)
            .env("HOME", self.0.join("home"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("CODEX_HOME", self.0.join("home/.codex"))
            .env("MNEME_INIT_PYTHON_TRACE", self.0.join("python-trace"))
            .env_remove("MNEME_DB")
            .output()
            .unwrap()
    }
    fn unchanged(&self) {
        assert_eq!(std::fs::read_dir(&self.0).unwrap().count(), 0);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn init_help_and_illegal_selectors_are_local_and_bounded() {
    let fixture = Fixture::new();
    let help = fixture.run(&["init", "--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    let help_words = help.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(help_words.contains("absolute or parent-free relative path"));
    assert!(help_words.contains("native policy controls recording"));
    assert!(help_words.contains("generated project hooks"));
    for option in [
        "--root",
        "--no-recording",
        "--no-trust-hooks",
        "--python",
        "--mcp-binary",
        "--codex-binary",
        "--library-config",
        "--store-registry",
        "--register",
        "--no-register",
        "--discovery-hub",
        "--advertise-url",
        "--interactive",
    ] {
        assert!(help.contains(option));
    }
    for args in [
        vec!["--json", "--user", "init"],
        vec!["--json", "--db", "memory.db", "init"],
        vec!["--json", "--remote", "http://127.0.0.1:19871/", "init"],
        vec!["--json", "--memory-config", "memory.toml", "init"],
        vec!["--json", "--store", "project", "init"],
        vec!["--json", "--profile", "isolated", "init"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["schema"], "mneme.project-init.v1");
        assert_eq!(result["status"], "incomplete");
        assert_eq!(result["stage"], "preflight");
        assert_eq!(result["published"], false);
        assert!(output.stderr.is_empty());
    }
    for selector in ["python3.14", "./python", "bin/python"] {
        let output = fixture.run(&["--json", "init", "--python", selector]);
        assert!(!output.status.success());
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["stage"], "preflight");
        assert_eq!(result["published"], false);
        #[cfg(feature = "cozo")]
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("absolute interpreter selector")
        );
    }
    let invalid = fixture.run(&["init", "--port", "1"]);
    assert!(!invalid.status.success());
    for args in [
        vec!["init", "--register", "--no-register"],
        vec!["init", "--register", "--interactive"],
        vec!["init", "--no-register", "--interactive"],
        vec!["init", "--discovery-hub", "invalid hub"],
        vec!["init", "--advertise-url", "http://127.0.0.1:1"],
    ] {
        assert!(!fixture.run(&args).status.success());
    }
    fixture.unchanged();
}
#[cfg(not(feature = "cozo"))]
#[test]
fn feature_minimal_init_cannot_masquerade_as_a_project_setup() {
    let fixture = Fixture::new();
    let output = fixture.run(&["--json", "init"]);
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result["error"].as_str().unwrap().contains("cozo-enabled"));
    fixture.unchanged();
}
#[cfg(feature = "cozo")]
#[test]
fn explicitly_unusable_python_does_not_fall_through_or_create_state() {
    let fixture = Fixture::new();
    let output = fixture.run(&["--json", "init", "--python", "/nonexistent/python"]);
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result["error"].as_str().unwrap().contains("Python 3.11+"));
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("/nonexistent/python")
    );
    assert!(output.stderr.is_empty());
    #[cfg(not(windows))]
    assert!(!result["error"].as_str().unwrap().contains("Windows"));
    fixture.unchanged();
}

#[cfg(feature = "cozo")]
#[test]
fn malformed_native_default_or_project_refuses_before_python_or_setup() {
    for relative in ["config/mneme/memory.toml", ".mneme/memory.toml"] {
        let fixture = Fixture::new();
        let config = fixture.0.join(relative);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "schema = [invalid native TOML").unwrap();
        #[cfg(unix)]
        let python = fixture.recording_python();
        #[cfg(not(unix))]
        let python = PathBuf::from("/nonexistent/python");
        let output = fixture.run(&["--json", "init", "--python", python.to_str().unwrap()]);
        assert!(!output.status.success());
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["stage"], "preflight");
        assert_eq!(result["published"], false);
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("cannot prepare native configuration")
        );
        assert!(!fixture.0.join("python-trace").exists());
        assert!(!fixture.0.join(".codex").exists());
        assert!(!fixture.0.join("data").exists());
        let human = fixture.run(&["init", "--python", python.to_str().unwrap()]);
        assert!(!human.status.success());
        assert!(human.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&human.stderr).contains("cannot prepare native configuration")
        );
        assert!(!fixture.0.join("python-trace").exists());
        assert_eq!(
            std::fs::read_to_string(config).unwrap(),
            "schema = [invalid native TOML"
        );
    }
}

#[cfg(all(feature = "cozo", unix))]
#[test]
fn dangling_native_default_or_project_refuses_instead_of_legacy_fallback() {
    for relative in ["config/mneme/memory.toml", ".mneme/memory.toml"] {
        let fixture = Fixture::new();
        let config = fixture.0.join(relative);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("absent-native.toml", &config).unwrap();
        let python = fixture.recording_python();
        let output = fixture.run(&["--json", "init", "--python", python.to_str().unwrap()]);
        assert!(!output.status.success());
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("cannot prepare native configuration")
        );
        assert_eq!(result["published"], false);
        assert!(!fixture.0.join("python-trace").exists());
        assert!(!fixture.0.join(".codex").exists());
        assert!(std::fs::symlink_metadata(config).unwrap().is_symlink());
    }
}

#[cfg(all(feature = "cozo", unix))]
impl Fixture {
    /// A disposable interpreter stand-in records probes/setup arguments without
    /// running project_setup or contacting any owner. It proves CLI routing only.
    fn recording_python(&self) -> PathBuf {
        self.recording_python_result(
            &serde_json::json!({
                "schema": "mneme.project-init.v1", "status": "configured", "stage": "complete",
                "published": false, "store": "adopted", "project_root": self.0,
                "db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "recording": "off",
                "local_registration": true, "hippocampus": "reminder",
                "hook_trust": {"status": "pending", "trusted": 0, "reason": "manual"},
                "fixture_receipt": {"preserved": true}
            })
            .to_string(),
            0,
        )
    }
    fn recording_python_result(&self, result: &str, status: u8) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let python = self.0.join("python");
        let probe = serde_json::json!({"version": [3, 11], "executable": python});
        let result = shlex::try_quote(result).unwrap();
        std::fs::write(&python, format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$MNEME_INIT_PYTHON_TRACE\"\nif [ \"$1\" = -I ]; then\n  printf '%s\\n' '{probe}'\nelse\n  printf '%s\\n' {result}\n  exit {status}\nfi\n"
        )).unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
        python
    }
    fn setup_trace(&self, root: &std::path::Path) -> String {
        let python = self.recording_python();
        let output = self.run(&[
            "--json",
            "init",
            "--root",
            root.to_str().unwrap(),
            "--python",
            python.to_str().unwrap(),
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["status"], "configured");
        assert_eq!(result["fixture_receipt"]["preserved"], true);
        assert!(!root.join(".codex").exists());
        std::fs::read_to_string(self.0.join("python-trace")).unwrap()
    }
}

#[cfg(all(feature = "cozo", unix))]
#[test]
fn absent_native_configuration_keeps_legacy_setup_arguments() {
    let fixture = Fixture::new();
    let trace = fixture.setup_trace(&fixture.0);
    assert!(trace.contains("codex/project_setup.py\n--root\n"));
    assert!(!trace.contains("--memory-config"));
    assert_eq!(trace.lines().filter(|line| *line == "--json").count(), 1);
}

#[cfg(all(feature = "cozo", unix))]
#[test]
fn human_init_also_captures_json_then_renders_legacy_result_once() {
    let fixture = Fixture::new();
    let python = fixture.recording_python();
    let output = fixture.run(&["init", "--python", python.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let human = String::from_utf8(output.stdout).unwrap();
    assert_eq!(human.matches("Project memory adopted:").count(), 1);
    assert!(human.contains("Hooks: pending (0 trusted/enabled); execution unverified."));
    assert!(!human.contains("mneme.project-init.v1"));
    let trace = std::fs::read_to_string(fixture.0.join("python-trace")).unwrap();
    assert_eq!(trace.lines().filter(|line| *line == "--json").count(), 1);
    assert!(!fixture.0.join(".codex").exists());
}

#[cfg(all(feature = "cozo", unix))]
#[test]
fn malformed_helper_result_is_incomplete_json_not_a_successful_init() {
    let fixture = Fixture::new();
    let python = fixture.recording_python_result("not a setup result", 0);
    let output = fixture.run(&["--json", "init", "--python", python.to_str().unwrap()]);
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "incomplete");
    assert_eq!(result["stage"], "setup_result");
    assert_eq!(result["published"], false);
    assert!(output.stderr.is_empty());
    assert!(result["error"].as_str().unwrap().contains("malformed JSON"));
    assert!(
        result["recovery"]
            .as_str()
            .unwrap()
            .contains("rerun mnemed init")
    );
    assert!(!fixture.0.join(".codex").exists());
}

#[cfg(all(feature = "cozo", unix))]
#[test]
fn helper_error_json_is_preserved_once_with_its_recovery_receipt() {
    let fixture = Fixture::new();
    let expected = serde_json::json!({
        "schema": "mneme.project-init.v1", "status": "incomplete", "stage": "installer",
        "published": false, "error": "fixture conflict", "recovery": "Keep and retry.",
        "receipt": "/fixture/pending.json"
    });
    let python = fixture.recording_python_result(&expected.to_string(), 1);
    let output = fixture.run(&["--json", "init", "--python", python.to_str().unwrap()]);
    assert!(!output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        expected
    );
    assert!(output.stderr.is_empty());
    let human = fixture.run(&["init", "--python", python.to_str().unwrap()]);
    assert_eq!(human.status.code(), Some(1));
    assert!(human.stdout.is_empty());
    assert_eq!(
        String::from_utf8(human.stderr).unwrap(),
        "Init incomplete: fixture conflict\nKeep and retry.\nReceipt: /fixture/pending.json\n"
    );
    assert!(!fixture.0.join(".codex").exists());
}

#[cfg(all(feature = "cozo", unix))]
#[test]
fn unsuccessful_helper_cannot_authorize_registration_with_success_shaped_output() {
    let fixture = Fixture::new();
    let success = serde_json::json!({
        "schema": "mneme.project-init.v1", "status": "configured", "stage": "complete",
        "published": false, "store": "created", "project_root": fixture.0,
        "db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "recording": "off", "integration": "native",
        "receipt": "/fixture/retained.json"
    });
    let python = fixture.recording_python_result(&success.to_string(), 1);
    let output = fixture.run(&["--json", "init", "--python", python.to_str().unwrap()]);
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "incomplete");
    assert_eq!(result["stage"], "setup_result");
    assert_eq!(result["receipt"], "/fixture/retained.json");
    assert!(result.get("discovery_registration").is_none());
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("exited unsuccessfully")
    );
}

#[cfg(all(feature = "cozo", unix))]
#[test]
fn relative_requested_root_is_absolute_for_native_and_legacy_setup() {
    for native in [false, true] {
        let fixture = Fixture::new();
        let requested = fixture.0.join("child");
        std::fs::create_dir(&requested).unwrap();
        let overlay = requested.join(".mneme/memory.toml");
        if native {
            std::fs::create_dir(requested.join(".mneme")).unwrap();
            std::fs::write(&overlay, "schema='mneme.config.v1'").unwrap();
        }
        let trace = fixture.setup_trace(std::path::Path::new("child"));
        // current_dir may use the platform's canonical spelling of the temp
        // root, but CLI relative paths must not be forwarded still relative.
        assert!(trace.contains(&format!(
            "--root\n{}\n",
            requested.canonicalize().unwrap().display()
        )));
        assert_eq!(trace.contains("--memory-config"), native);
        if native {
            assert!(trace.contains(&format!(
                "--memory-config\n{}\n",
                overlay.canonicalize().unwrap().display()
            )));
        }
        assert!(!requested.join(".codex").exists());
    }
}

#[cfg(all(feature = "cozo", unix))]
#[test]
fn parent_component_root_refuses_before_python_with_or_without_native_config() {
    for native in [false, true] {
        let fixture = Fixture::new();
        let invoking = fixture.0.join("invoking");
        let requested = fixture.0.join("parentRoot");
        std::fs::create_dir(&invoking).unwrap();
        std::fs::create_dir(&requested).unwrap();
        let overlay = requested.join(".mneme/memory.toml");
        if native {
            std::fs::create_dir(requested.join(".mneme")).unwrap();
            std::fs::write(&overlay, "schema='mneme.config.v1'").unwrap();
        }
        let python = fixture.recording_python();
        let output = fixture.run_at(
            &invoking,
            &[
                "--json",
                "init",
                "--root",
                "../parentRoot",
                "--python",
                python.to_str().unwrap(),
            ],
        );
        assert!(!output.status.success());
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["stage"], "preflight");
        assert_eq!(result["published"], false);
        let error = result["error"].as_str().unwrap();
        assert!(error.contains("paths must not contain parent components"));
        assert!(error.contains("cd into the target"));
        assert!(!fixture.0.join("python-trace").exists());
        assert!(!requested.join(".codex").exists());
        assert!(!fixture.0.join("data").exists());
        assert_eq!(std::fs::read_dir(&invoking).unwrap().count(), 0);
        if native {
            assert_eq!(
                std::fs::read_to_string(overlay).unwrap(),
                "schema='mneme.config.v1'"
            );
            assert_eq!(
                std::fs::read_dir(requested.join(".mneme")).unwrap().count(),
                1
            );
        } else {
            assert_eq!(std::fs::read_dir(&requested).unwrap().count(), 0);
        }
    }
}

#[cfg(all(feature = "cozo", unix))]
#[test]
fn native_project_route_uses_requested_root_not_invoking_directory() {
    let fixture = Fixture::new();
    // The invoking directory has invalid native metadata; an unrelated requested
    // root must not inherit it. Conversely, its own valid overlay is discovered.
    std::fs::create_dir(fixture.0.join(".mneme")).unwrap();
    std::fs::write(
        fixture.0.join(".mneme/memory.toml"),
        "invalid native metadata",
    )
    .unwrap();
    let requested = Fixture::new();
    std::fs::create_dir(requested.0.join(".mneme")).unwrap();
    let overlay = requested.0.join(".mneme/memory.toml");
    std::fs::write(&overlay, "schema='mneme.config.v1'").unwrap();
    let trace = fixture.setup_trace(&requested.0);
    assert!(trace.contains(&format!(
        "--memory-config\n{}\n",
        overlay.canonicalize().unwrap().display()
    )));
    assert!(!trace.contains(&fixture.0.join(".mneme/memory.toml").display().to_string()));
}

#[cfg(all(feature = "cozo", unix))]
#[test]
fn native_user_source_keeps_logical_symlink_even_with_no_active_store() {
    let fixture = Fixture::new();
    let target = fixture.0.join("managed.toml");
    let logical = fixture.0.join("config/mneme/memory.toml");
    std::fs::create_dir_all(logical.parent().unwrap()).unwrap();
    std::fs::write(
        &target,
        "schema='mneme.config.v1'\n[workspace]\nallow_stores=[]\n[stores.denied]\nid='01M3CV5EZP4HWAK56YJCMZQG2Y'\nowner={url='http://127.0.0.1:1',db='uncontacted'}\nactive={enabled=false}\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&target, &logical).unwrap();
    let trace = fixture.setup_trace(&fixture.0);
    assert!(trace.contains(&format!("--memory-config\n{}\n", logical.display())));
    assert!(!trace.contains(&format!("--memory-config\n{}\n", target.display())));
    assert!(std::fs::symlink_metadata(logical).unwrap().is_symlink());
}
#[cfg(feature = "cozo")]
#[test]
fn inspect_is_existing_current_only_inference_free_and_releases_lease() {
    let fixture = Fixture::new();
    let missing = fixture.run(&["--db", "store.db", "capture", "inspect"]);
    assert!(!missing.status.success());
    assert!(!fixture.0.join("store.db").exists());
    assert!(!fixture.0.join("store.db-wal").exists());
    assert!(!fixture.0.join("store.db.bodies").exists());
    let created = fixture.run(&["--db", "store.db", "capture", "init"]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let before = std::fs::read(fixture.0.join("store.db")).unwrap();
    let inspect = || fixture.run(&["--json", "--db", "store.db", "capture", "inspect"]);
    let result = inspect();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(result["status"], "verified");
    assert_eq!(result["db_id"].as_str().unwrap().len(), 26);
    assert_eq!(std::fs::read(fixture.0.join("store.db")).unwrap(), before);
    let lease = mneme_store_path::StoreLease::acquire(&fixture.0.join("store.db")).unwrap();
    let contention = inspect();
    assert!(!contention.status.success());
    drop(lease);
    assert!(inspect().status.success());
}
