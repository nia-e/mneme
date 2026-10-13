//! Declarative adapter admission: fixtures have no stores and no owner services.
use serde_json::{Value, json};
use std::{
    io::Write,
    net::TcpListener,
    path::PathBuf,
    process::{Command, Stdio},
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
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env_remove("MNEME_DB");
        command
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
