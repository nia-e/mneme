//! Black-box admission tests: library is not a disguised local database opener.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mneme-cli-library-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn run(args: &[&str]) -> (Fixture, Output) {
    let dir = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .current_dir(dir.path())
        .env("HOME", dir.path().join("home"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env_remove("MNEME_DB")
        .args(args)
        .output()
        .unwrap();
    (dir, output)
}

#[test]
fn library_refuses_store_and_remote_selectors_before_config_open() {
    for args in [
        vec![
            "--db",
            "secret.db",
            "library",
            "--config",
            "missing.json",
            "catalog",
        ],
        vec!["--user", "library", "--config", "missing.json", "catalog"],
        vec![
            "--remote",
            "http://127.0.0.1:1/mcp",
            "library",
            "--config",
            "missing.json",
            "catalog",
        ],
    ] {
        let (dir, output) = run(&args);
        assert!(!output.status.success(), "{args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("library forbids"), "{args:?}: {error}");
        assert!(!dir.path().join(".mneme").exists());
        assert!(!dir.path().join("data/mneme").exists());
    }
}

#[test]
fn library_rejects_env_db_and_invalid_query_before_config_open() {
    let dir = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .current_dir(dir.path())
        .env("MNEME_DB", dir.path().join("would-create.db"))
        .args(["library", "--config", "missing.json", "catalog"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("library forbids"));
    assert!(!dir.path().join("would-create.db").exists());

    let (_dir, output) = run(&["library", "--config", "missing.json", "query", "   "]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("nonblank"), "{error}");
    assert!(
        !error.contains("missing.json"),
        "invalid query must precede config IO"
    );
}

#[test]
fn snapshot_create_requires_remote_owner_without_local_residue() {
    let (dir, output) = run(&["snapshot", "create"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no project CLI owner configured"));
    assert!(!dir.path().join(".mneme").exists());
}

fn write_library(fixture: &Fixture) -> PathBuf {
    let config = fixture.path().join("library.json");
    std::fs::write(
        fixture.path().join("catalog.json"),
        r#"{"schema":"mneme.library.catalog.v1","library_id":"test-library","revision":1,"entries":[{"project_id":"p1","db_id":"database-1","owner_device_id":"owner-1","display_name":"Project One","database":"project","revision":1,"replicas":[{"source_device_id":"replica-1","database":"project","resolved_path":"/tmp/replica/database.db","generation":"g1","captured_at":1790294400}]}]}"#,
    )
    .unwrap();
    std::fs::write(
        &config,
        r#"{"schema":"mneme.library.config.v1","library_id":"test-library","device_id":"client-1","catalog_path":"catalog.json","owners":{},"replicas":{}}"#,
    )
    .unwrap();
    config
}

#[test]
fn library_catalog_reads_only_configured_catalog() {
    let dir = Fixture::new();
    let config = write_library(&dir);
    let output = Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .current_dir(dir.path())
        .env_remove("MNEME_DB")
        .args([
            "--json",
            "library",
            "--config",
            config.to_str().unwrap(),
            "catalog",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["entries"][0]["project_id"], "p1");
    assert!(!dir.path().join(".mneme").exists());
}

#[test]
fn unknown_project_and_expired_ref_refuse_before_source_connection() {
    let dir = Fixture::new();
    let config = write_library(&dir);
    let bin = env!("CARGO_BIN_EXE_mnemed");
    let query = Command::new(bin)
        .current_dir(dir.path())
        .env_remove("MNEME_DB")
        .args([
            "library",
            "--config",
            config.to_str().unwrap(),
            "query",
            "needle",
            "--project-id",
            "unknown",
        ])
        .output()
        .unwrap();
    assert!(!query.status.success());
    assert!(String::from_utf8_lossy(&query.stderr).contains("not enrolled"));

    let get = Command::new(bin)
        .current_dir(dir.path())
        .env_remove("MNEME_DB")
        .args([
            "library",
            "--config",
            config.to_str().unwrap(),
            "get",
            "p1",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "replica-1",
            "--generation",
            "expired",
        ])
        .output()
        .unwrap();
    assert!(!get.status.success());
    assert!(String::from_utf8_lossy(&get.stderr).contains("generation expired"));
    assert!(!dir.path().join(".mneme").exists());
}

fn command(dir: &Fixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mnemed"));
    command
        .current_dir(dir.path())
        .env("HOME", dir.path().join("home"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env_remove("MNEME_DB");
    command
}

fn install_fixture_config(dir: &Fixture, relative: &str) -> PathBuf {
    let source = write_library(dir);
    let target = dir.path().join(relative).join("library.json");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::copy(source, &target).unwrap();
    std::fs::copy(
        dir.path().join("catalog.json"),
        target.with_file_name("catalog.json"),
    )
    .unwrap();
    target
}

fn write_profile(root: &std::path::Path, mode: &str, config: Option<&std::path::Path>) {
    let path = root.join(".mneme/profile.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut profile = serde_json::json!({"schema":"mneme.profile.v1", "mode":mode});
    if let Some(config) = config {
        profile["library_config"] = serde_json::json!(config);
    }
    std::fs::write(path, serde_json::to_vec(&profile).unwrap()).unwrap();
}

fn assert_catalog(output: Output, json: bool) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if json {
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["entries"][0]["project_id"], "p1");
    } else {
        assert!(String::from_utf8_lossy(&output.stdout).contains("Project One"));
    }
}

#[test]
fn library_default_uses_xdg_then_home_for_human_and_json() {
    let dir = Fixture::new();
    let xdg = install_fixture_config(&dir, "data/mneme/libraries/personal");
    let home = install_fixture_config(&dir, "home/.local/share/mneme/libraries/personal");
    // Invalid HOME config proves XDG actually wins, rather than accidentally working.
    std::fs::write(&home, "not json").unwrap();
    assert_catalog(
        command(&dir).args(["library", "catalog"]).output().unwrap(),
        false,
    );
    std::fs::copy(&xdg, &home).unwrap();
    std::fs::write(&xdg, "not json").unwrap();
    for empty_xdg in [false, true] {
        let mut cmd = command(&dir);
        if empty_xdg {
            cmd.env("XDG_DATA_HOME", "");
        } else {
            cmd.env_remove("XDG_DATA_HOME");
        }
        assert_catalog(
            cmd.args(["--json", "library", "catalog"]).output().unwrap(),
            true,
        );
    }
    // A broken selected config must not fall back to the valid HOME config.
    assert!(
        !command(&dir)
            .args(["library", "catalog"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(!dir.path().join(".mneme").exists());
}

#[test]
fn library_missing_default_is_actionable_and_creates_nothing() {
    let (dir, output) = run(&["library", "catalog"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("no default library config") && error.contains("--config PATH"),
        "{error}"
    );
    assert!(!dir.path().join("data").exists());
    assert!(!dir.path().join(".mneme").exists());
    let no_home = command(&dir)
        .env_remove("HOME")
        .env_remove("XDG_DATA_HOME")
        .args(["library", "catalog"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&no_home.stderr).contains("--config PATH"));
    let invalid = command(&dir)
        .args(["library", "query", "  "])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("nonblank"));
}

#[test]
fn library_nearest_profile_wins_and_explicit_config_overrides() {
    let dir = Fixture::new();
    let config = write_library(&dir);
    write_profile(dir.path(), "default", Some(&dir.path().join("absent.json")));
    let nested = dir.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    write_profile(&nested, "private", Some(&config));
    let deeper = nested.join("src");
    std::fs::create_dir(&deeper).unwrap();
    assert_catalog(
        command(&dir)
            .current_dir(&deeper)
            .args(["library", "catalog"])
            .output()
            .unwrap(),
        false,
    );
    assert!(
        !command(&dir)
            .args(["library", "catalog"])
            .output()
            .unwrap()
            .status
            .success()
    );
    // Explicit selection preserves the existing behavior, even with a broken profile.
    std::fs::write(dir.path().join(".mneme/profile.json"), "broken").unwrap();
    assert_catalog(
        command(&dir)
            .args(["library", "--config", config.to_str().unwrap(), "catalog"])
            .output()
            .unwrap(),
        false,
    );
}

#[test]
fn library_isolated_profile_never_implicitly_uses_personal_config() {
    let dir = Fixture::new();
    let personal = install_fixture_config(&dir, "data/mneme/libraries/personal");
    write_profile(dir.path(), "isolated", None);
    let output = command(&dir).args(["library", "catalog"]).output().unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("isolated project needs"));
    write_profile(dir.path(), "isolated", Some(&personal));
    let output = command(&dir)
        .args(["--json", "library", "catalog"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("personal default library"));
    let independent = write_library(&dir);
    write_profile(dir.path(), "isolated", Some(&independent));
    assert_catalog(
        command(&dir).args(["library", "catalog"]).output().unwrap(),
        false,
    );
    #[cfg(unix)]
    {
        let alias = dir.path().join("alias.json");
        std::os::unix::fs::symlink(&personal, &alias).unwrap();
        write_profile(dir.path(), "isolated", Some(&alias));
        let output = command(&dir).args(["library", "catalog"]).output().unwrap();
        assert!(String::from_utf8_lossy(&output.stderr).contains("personal default library"));
        let hardlink = dir.path().join("hardlink.json");
        std::fs::hard_link(&personal, &hardlink).unwrap();
        write_profile(dir.path(), "isolated", Some(&hardlink));
        let output = command(&dir).args(["library", "catalog"]).output().unwrap();
        assert!(String::from_utf8_lossy(&output.stderr).contains("personal default library"));
    }
}

#[test]
fn library_invalid_profile_does_not_fall_back_to_personal() {
    let dir = Fixture::new();
    install_fixture_config(&dir, "data/mneme/libraries/personal");
    write_profile(dir.path(), "default", None);
    let profile = dir.path().join(".mneme/profile.json");
    for value in [
        "{}".to_owned(),
        r#"{"schema":"mneme.profile.v1","mode":"default","extra":1}"#.to_owned(),
        r#"{"schema":"mneme.profile.v1","mode":"default","mode":"isolated"}"#.to_owned(),
        r#"{"schema":"mneme.profile.v1","mode":"default","library_config":null}"#.to_owned(),
        r#"{"schema":"mneme.profile.v1","mode":"default","library_config":"relative.json"}"#
            .to_owned(),
        " ".repeat(8193),
    ] {
        std::fs::write(&profile, value).unwrap();
        let output = command(&dir).args(["library", "catalog"]).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("profile"));
    }
}

#[test]
fn library_default_help_and_selector_refusals() {
    let (_dir, help) = run(&["library", "--help"]);
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.contains("[OPTIONS]") && help.contains("XDG_DATA_HOME"),
        "{help}"
    );
    for args in [
        vec!["--user", "library", "catalog"],
        vec!["--db", "no.db", "library", "catalog"],
    ] {
        let (dir, output) = run(&args);
        assert!(String::from_utf8_lossy(&output.stderr).contains("library forbids"));
        assert!(!dir.path().join("no.db").exists());
    }
}
