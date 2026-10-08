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
        Command::new(env!("CARGO_BIN_EXE_mnemed"))
            .args(args)
            .current_dir(&self.0)
            .env("HOME", self.0.join("home"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
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
    for option in [
        "--root",
        "--no-recording",
        "--no-trust-hooks",
        "--python",
        "--mcp-binary",
        "--codex-binary",
        "--library-config",
    ] {
        assert!(help.contains(option));
    }
    for args in [
        vec!["--json", "--user", "init"],
        vec!["--json", "--db", "memory.db", "init"],
        vec!["--json", "--remote", "http://127.0.0.1:19871/", "init"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["schema"], "mneme.project-init.v1");
        assert_eq!(result["status"], "incomplete");
        assert_eq!(result["stage"], "preflight");
        assert_eq!(result["published"], false);
    }
    let invalid = fixture.run(&["init", "--port", "1"]);
    assert!(!invalid.status.success());
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
    fixture.unchanged();
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
