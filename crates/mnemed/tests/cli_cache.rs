//! The cache worker's CLI bridge replays recorded metadata, never ambient policy.
#![cfg(unix)]

use mneme_config::{SelectionRequest, SelectionRequestMode};
use serde_json::{Value, json};
use std::{net::TcpListener, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

const DB: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

struct Fixture {
    root: PathBuf,
    config: PathBuf,
    demand: PathBuf,
    listener: TcpListener,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mneme-cli-cache-{}", ulid::Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let config = root.join("memory.toml");
        std::fs::write(
            &config,
            format!(
                r#"schema = "mneme.config.v1"
read_default = "alpha"
[stores.alpha]
id = "{DB}"
owner = {{url="http://{}", db="native"}}
active = {{workspace="any"}}
cache = {{library="offline", populate={{}}}}
[libraries.offline]
catalog = "{}"
"#,
                listener.local_addr().unwrap(),
                root.join("catalog.json").display()
            ),
        )
        .unwrap();
        let request = SelectionRequest {
            mode: SelectionRequestMode::Explicit,
            user_path: Some(config.clone()),
            cwd: root.clone(),
            requested_profile: Some("private".into()),
            lexical_pwd: Some(root.clone()),
            user_codex_dir: Some(root.join("codex")),
        };
        let snapshot = request.replay_offline(None).unwrap().unwrap();
        let parent = root.join("cache-demand");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let demand = parent.join(format!("{DB}.json"));
        let observed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(
            &demand,
            serde_json::to_vec(&json!({
                "schema":"mneme.cache-demand.v1", "request":request,
                "selection_digest":snapshot.digest, "store":"alpha", "db_id":DB,
                "library":"offline", "explicit_store":false, "observed_at_unix_secs":observed,
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&demand, std::fs::Permissions::from_mode(0o600)).unwrap();
        Self {
            root,
            config,
            demand,
            listener,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mnemed"));
        // Deliberately unrelated cwd and ambient HOME: only the frozen request counts.
        command
            .current_dir(self.root.parent().unwrap())
            .env("HOME", self.root.join("unused-home"))
            .env("PWD", self.root.parent().unwrap())
            .env("CODEX_HOME", self.root.join("unused-codex"))
            .env("XDG_CONFIG_HOME", self.root.join("unused-config"))
            .env_remove("MNEME_DB");
        command
    }
    fn assert_offline(&self) {
        assert_eq!(
            self.listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(!self.root.join("catalog.json").exists());
        assert!(!self.root.join("unused-home").exists());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn cache_plan_uses_exact_recorded_request_in_human_and_json_modes() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args(["--json", "cache", "plan", "--demand"])
        .arg(&fixture.demand)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["db_id"], DB);
    assert_eq!(plan["request"]["cwd"], fixture.root.to_str().unwrap());
    assert_eq!(plan["request"]["requested_profile"], "private");
    assert_eq!(plan["cache"]["refresh_after_secs"], 900);
    let human = fixture
        .command()
        .args(["cache", "plan", "--demand"])
        .arg(&fixture.demand)
        .output()
        .unwrap();
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).contains("offline policy check only"));
    fixture.assert_offline();
}

#[test]
fn changed_policy_refuses_without_reselecting_or_contacting_owner() {
    let fixture = Fixture::new();
    let text = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        text.replace("populate={}", "populate={refresh_after_secs=1800}"),
    )
    .unwrap();
    let output = fixture
        .command()
        .args(["--json", "cache", "plan", "--demand"])
        .arg(&fixture.demand)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("selection changed"));
    fixture.assert_offline();
}

#[test]
fn ambient_selector_overrides_are_denied_before_even_reading_demand() {
    let fixture = Fixture::new();
    for args in [
        vec!["--db", "/absent/cache-plan.db"],
        vec!["--user"],
        vec!["--profile", "default"],
        vec!["--store", "alpha"],
        vec!["--memory-config", "/absent/cache-plan.toml"],
        vec!["--remote", "http://127.0.0.1:1"],
        vec!["--remote", "http://127.0.0.1:1", "--remote-db", "native"],
        vec![
            "--remote",
            "named",
            "--remote-config",
            "/absent/remote.json",
        ],
        vec!["--remote", "ssh://localhost", "--remote-mcp-port", "1234"],
        vec![
            "--remote",
            "http://127.0.0.1:1",
            "--remote-token-env",
            "UNUSED",
        ],
    ] {
        let output = fixture
            .command()
            .args(&args)
            .args(["cache", "plan", "--demand", "/absent/cache-demand.json"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("overrides are not accepted"),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fixture.assert_offline();
}
