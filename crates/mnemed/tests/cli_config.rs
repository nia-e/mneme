//! Declarative adapter admission: fixtures have no stores and no owner services.
#[allow(dead_code)]
#[path = "../../mneme-store-client/src/discovery/fixture.rs"]
mod discovery_fixture;

use serde_json::{Value, json};
use std::{
    io::Write,
    net::TcpListener,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::Ordering,
};

struct Fixture {
    root: PathBuf,
    config: PathBuf,
    listener: TcpListener,
}
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("mneme-declarative-cli-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let config = root.join("memory.toml");
        let fixture = Self {
            root,
            config,
            listener,
        };
        fixture.write(false);
        fixture
    }
    fn write(&self, write: bool) {
        std::fs::write(
            &self.config,
            format!(
                r#"schema = "mneme.config.v1"
read_default = "renamed"
[stores.renamed]
id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
owner = {{ url = "http://{}", db = "native-alias" }}
active = {{ workspace = "any" }}
permissions = {{ read = true, write = {write} }}
"#,
                self.listener.local_addr().unwrap()
            ),
        )
        .unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mnemed"));
        command
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("PWD", &self.root)
            .env("CODEX_HOME", self.root.join(".codex"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env_remove("MNEME_DB");
        command
    }
    fn install_default(&self) -> PathBuf {
        let path = self.root.join("config/mneme/memory.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::copy(&self.config, &path).unwrap();
        path
    }
    fn json(&self, args: &[&str]) -> Value {
        let output = self.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn select(&self) -> Value {
        let output = self
            .command()
            .args(["--json", "config", "select", "--config"])
            .arg(&self.config)
            .arg("--cwd")
            .arg(&self.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn call(&self, digest: &str, request: Value) -> Value {
        let mut child = self
            .command()
            .args(["--json", "config", "call", "--config"])
            .arg(&self.config)
            .arg("--cwd")
            .arg(&self.root)
            .args([
                "--store",
                "renamed",
                "--operation",
                "explicit",
                "--expected-selection-digest",
                digest,
                "--max-native-bytes",
                "1024",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&request).unwrap())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn no_contact(&self) {
        assert!(
            matches!(self.listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock)
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn native_snapshot_and_stores_inventory_are_identical_without_contact() {
    let fixture = Fixture::new();
    let selected = fixture.select();
    let output = fixture
        .command()
        .arg("--memory-config")
        .arg(&fixture.config)
        .args(["--json", "stores"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        selected
    );
    assert_eq!(selected["read_default"], "renamed");
    assert_eq!(
        selected["active"][0]["store"]["owner"]["db"],
        "native-alias"
    );
    fixture.no_contact();
}

#[test]
fn omitted_config_matches_ordinary_selection_with_profile_and_project_overlay() {
    let fixture = Fixture::new();
    fixture.install_default();
    let overlay = fixture.root.join(".mneme/memory.toml");
    for project in [false, true] {
        if project {
            std::fs::create_dir_all(overlay.parent().unwrap()).unwrap();
            std::fs::write(
                &overlay,
                "schema='mneme.config.v1'\n[stores.local]\nid='01ARZ3NDEKTSV4RRFFQ69G5FA2'\nowner={url='http://127.0.0.1:1',db='project'}\nactive={workspace='project'}\n",
            )
            .unwrap();
        }
        for profile in ["default", "private"] {
            let ordinary = fixture.json(&["--json", "--profile", profile, "stores"]);
            let selected = fixture.json(&[
                "--json",
                "--profile",
                profile,
                "config",
                "select",
                "--offline",
            ]);
            assert_eq!(selected, ordinary, "project={project}, profile={profile}");
            assert_eq!(selected["request"]["mode"], "discover");
            assert_eq!(selected["request"]["requested_profile"], profile);
            assert_eq!(selected["request"]["lexical_pwd"], json!(fixture.root));
            assert_eq!(selected["project"]["configured"], project);
            assert_eq!(selected["config"]["stores"].get("local").is_some(), project);
            fixture.no_contact();
        }
    }
}

#[test]
fn omitted_config_refresh_updates_ordinary_receipt_not_explicit_receipt() {
    fn document(port: u16) -> Value {
        serde_json::to_value(
            mneme_config::PublishedCatalog::parse_toml(
                &format!(
                    "schema='mneme.discovery-catalog.v1'\n[stores.target]\nid='01ARZ3NDEKTSV4RRFFQ69G5FAV'\nowner={{url='http://127.0.0.1:{port}/',db='project'}}\nactive={{workspace='any'}}\npermissions={{read=true,write=false}}\n"
                ),
                discovery_fixture::DB,
            )
            .unwrap(),
        )
        .unwrap()
    }
    let publisher = discovery_fixture::Fixture::new(document(19001));
    let fixture = Fixture::new();
    std::fs::write(
        &fixture.config,
        format!(
            "schema='mneme.config.v1'\nread_default='target'\n[stores.seed]\nid='{}'\nowner={{url='{}',db='project'}}\ndiscovery=true\nactive={{workspace='any'}}\n",
            discovery_fixture::DB,
            publisher.url,
        ),
    )
    .unwrap();
    let default = fixture.install_default();
    let ordinary = fixture.json(&["--json", "stores"]);
    assert_eq!(ordinary["request"]["mode"], "discover");
    assert_eq!(
        ordinary["config"]["stores"]["target"]["owner"]["url"],
        "http://127.0.0.1:19001/"
    );
    assert_eq!(publisher.calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        fixture.json(&["--json", "config", "select", "--offline"]),
        ordinary
    );
    assert_eq!(publisher.calls.load(Ordering::Relaxed), 1);

    let explicit = fixture.json(&[
        "--json",
        "config",
        "select",
        "--config",
        default.to_str().unwrap(),
    ]);
    assert_eq!(explicit["request"]["mode"], "explicit");
    assert_ne!(
        explicit["discovery"]["binding"]["path"],
        ordinary["discovery"]["binding"]["path"]
    );
    assert_eq!(publisher.calls.load(Ordering::Relaxed), 2);

    publisher.set_document(document(19002));
    let refreshed = fixture.json(&["--json", "config", "inspect", "--refresh"]);
    assert!(refreshed["error"].is_null(), "{refreshed}");
    assert_eq!(publisher.calls.load(Ordering::Relaxed), 3);
    let current = fixture.json(&["--json", "stores"]);
    assert_eq!(current["request"], ordinary["request"]);
    assert_eq!(
        current["config"]["stores"]["target"]["owner"]["url"],
        "http://127.0.0.1:19002/"
    );
    assert_eq!(
        current["discovery"]["binding"]["path"],
        ordinary["discovery"]["binding"]["path"]
    );
    assert_ne!(current["digest"], ordinary["digest"]);
    assert_eq!(refreshed["selection_digest"], current["digest"]);
    assert_eq!(
        fixture.json(&[
            "--json",
            "config",
            "select",
            "--expected-selection-digest",
            current["digest"].as_str().unwrap(),
        ]),
        current,
    );
    assert_eq!(
        fixture.json(&[
            "--json",
            "config",
            "select",
            "--offline",
            "--config",
            default.to_str().unwrap(),
        ]),
        explicit,
        "refresh must not overwrite another request's retained receipt",
    );
    assert_eq!(publisher.calls.load(Ordering::Relaxed), 3);
    fixture.no_contact();
}

#[test]
fn omitted_missing_config_is_unconfigured_but_explicit_missing_config_is_an_error() {
    let fixture = Fixture::new();
    let automatic = fixture.json(&["--json", "config", "inspect", "--offline"]);
    assert_eq!(automatic["configured"], false);
    assert!(automatic["error"].is_null());
    let missing = fixture.root.join("config/mneme/memory.toml");
    let explicit = fixture.json(&[
        "--json",
        "config",
        "inspect",
        "--offline",
        "--config",
        missing.to_str().unwrap(),
    ]);
    assert!(explicit["configured"].is_null());
    assert!(
        explicit["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    fixture.no_contact();
}

#[test]
fn bridge_denied_write_and_stale_digest_are_precontact_with_known_zero_work() {
    let fixture = Fixture::new();
    let selected = fixture.select();
    let digest = selected["digest"].as_str().unwrap();
    let request = json!({"tool":"forget","arguments":{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}});
    let denial = fixture.call(digest, request.clone());
    assert_eq!(denial["ok"], false);
    assert!(
        denial["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not granted")
    );
    assert_eq!(denial["work"]["decoded_bytes"], 0);
    fixture.write(true);
    let stale = fixture.call(digest, request);
    assert_eq!(stale["ok"], false);
    assert!(
        stale["error"]["message"]
            .as_str()
            .unwrap()
            .contains("digest changed")
    );
    assert_eq!(stale["work"]["decoded_bytes"], 0);
    fixture.no_contact();
}

#[test]
fn local_inspection_reports_policy_and_provenance_without_owner_contact() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args(["--json", "config", "inspect", "--config"])
        .arg(&fixture.config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema"], "mneme.config-inspection.v1");
    assert_eq!(report["configured"], true);
    assert_eq!(report["cache_status"], "local-only");
    assert_eq!(report["stores"][0]["name"], "renamed");
    assert_eq!(report["stores"][0]["id"], "01ARZ3NDEKTSV4RRFFQ69G5FAV");
    assert_eq!(report["stores"][0]["active"], true);
    assert_eq!(report["stores"][0]["eligible"]["write"], false);
    assert!(report["error"].is_null());
    assert!(
        !report["report"].is_null(),
        "local provenance should remain inspectable"
    );
    let human = fixture
        .command()
        .args(["config", "inspect", "--config"])
        .arg(&fixture.config)
        .output()
        .unwrap();
    assert!(human.status.success());
    let human = String::from_utf8(human.stdout).unwrap();
    for evidence in [
        "renamed",
        "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "active=true",
        "local-only",
    ] {
        assert!(human.contains(evidence), "missing {evidence}: {human}");
    }
    fixture.no_contact();
    assert!(
        !fixture.root.join("cache").exists(),
        "local-only inspection must not create a cache"
    );
}

#[test]
fn malformed_inspection_is_diagnostic_not_empty_successful_selection() {
    let fixture = Fixture::new();
    const SECRET: &str = "do-not-echo-this-broken-config-value";
    std::fs::write(&fixture.config, format!("schema = \"{SECRET}\n")).unwrap();
    let output = fixture
        .command()
        .args(["--json", "config", "inspect", "--config"])
        .arg(&fixture.config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "inspect succeeds in reporting a diagnostic"
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema"], "mneme.config-inspection.v1");
    assert!(
        report["configured"].is_null(),
        "unknown is not unconfigured"
    );
    assert!(report["selection_digest"].is_null());
    assert!(
        report["error"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
    fixture.no_contact();
}

#[test]
fn expected_digest_select_replays_offline_and_refuses_local_change() {
    let fixture = Fixture::new();
    let selected = fixture.select();
    let digest = selected["digest"].as_str().unwrap();
    let run = || {
        fixture
            .command()
            .args(["--json", "config", "select", "--config"])
            .arg(&fixture.config)
            .args(["--expected-selection-digest", digest])
            .output()
            .unwrap()
    };
    let replayed = run();
    assert!(
        replayed.status.success(),
        "{}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&replayed.stdout).unwrap(),
        selected
    );
    fixture.write(true);
    let changed = run();
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("digest changed"));
    fixture.no_contact();
}

#[test]
fn frozen_selection_cannot_be_combined_with_refresh() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args(["config", "select", "--config"])
        .arg(&fixture.config)
        .args(["--refresh", "--expected-selection-digest", &"0".repeat(64)])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    fixture.no_contact();
}

#[test]
fn offline_discovery_inspection_does_not_fetch_missing_catalog() {
    let fixture = Fixture::new();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        config.replace("[stores.renamed]", "[stores.renamed]\ndiscovery = true"),
    )
    .unwrap();
    let output = fixture
        .command()
        .args(["--json", "config", "inspect", "--offline", "--config"])
        .arg(&fixture.config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["selection_digest"].is_null());
    assert!(
        report["error"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );
    assert_ne!(report["cache_status"], "cached");
    fixture.no_contact();
}
