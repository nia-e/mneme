//! Known-source metadata must not connect, enroll, or treat missing routes as empty stores.
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};
const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mneme-sources-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(root.join("project/.git")).unwrap();
        Self(root)
    }
    fn write(&self, path: &str, value: Value) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    fn owner(&self, path: &str, database: &str) {
        self.write(path,json!({"schema":"mneme.cli.owner.v1","url":"http://127.0.0.1:1","database":database,"db_id":ID}));
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mnemed"))
            .args(args)
            .current_dir(self.0.join("project"))
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env_remove("MNEME_DB")
            .output()
            .unwrap()
    }
    fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        fn collect(path: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    collect(&path, out)
                } else {
                    out.push((path.clone(), std::fs::read(path).unwrap()));
                }
            }
        }
        let mut files = Vec::new();
        collect(&self.0, &mut files);
        files.sort();
        files
    }
    fn json(&self, args: &[&str]) -> Value {
        let result = self.run(args);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice(&result.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn known_metadata_retains_invalid_entries_and_never_probes_or_writes() {
    let fixture = Fixture::new();
    fixture.owner("project/.mneme/cli.json", "user-alias-not-scope");
    fixture.owner("config/mneme/misc.json", "project");
    fixture.write("config/mneme/cli.json", json!({"broken":true}));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut record: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("project/.mneme/cli.json")).unwrap())
            .unwrap();
    record["url"] = json!(format!("http://{}", listener.local_addr().unwrap()));
    fixture.write("project/.mneme/cli.json", record);
    let before = fixture.snapshot();
    let value = fixture.json(&["--json", "stores"]);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(value["schema"], "mneme.stores.v1");
    let sources = value["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 3);
    assert_eq!(sources[0]["name"], "project (user-alias-not-scope)");
    assert_eq!(sources[0]["db_id"], ID);
    assert_eq!(sources[0]["route"], "configured");
    assert!(
        sources
            .iter()
            .any(|source| source["name"] == "global" && source["route"] == "unavailable")
    );
    assert!(
        sources
            .iter()
            .any(|source| source["name"] == "misc (project)")
    );
    assert_eq!(fixture.snapshot(), before);
}
#[test]
fn explicit_remote_and_user_filters_do_not_append_ambient_sources() {
    let fixture = Fixture::new();
    fixture.owner("project/.mneme/cli.json", "work");
    fixture.owner("config/mneme/cli.json", "personal");
    fixture.owner("config/mneme/misc.json", "project");
    let before = fixture.snapshot();
    let remote = fixture.json(&["--json", "--remote", "http://127.0.0.1:1", "stores"]);
    assert_eq!(remote["sources"].as_array().unwrap().len(), 1);
    assert!(remote["sources"][0]["db_id"].is_null());
    let user = fixture.json(&["--json", "--user", "stores"]);
    assert_eq!(user["sources"].as_array().unwrap().len(), 1);
    assert_eq!(user["sources"][0]["name"], "Global");
    assert_eq!(fixture.snapshot(), before);
    let refusal = fixture.run(&["--db", "missing.db", "stores"]);
    assert!(!refusal.status.success());
    assert!(String::from_utf8_lossy(&refusal.stderr).contains("metadata"));
    assert!(!fixture.0.join("project/missing.db").exists());
}
#[test]
fn selected_library_retains_unrouted_identity_and_ignores_ambient_owners() {
    let fixture = Fixture::new();
    fixture.owner("project/.mneme/cli.json", "work");
    fixture.write("library/catalog.json",json!({"schema":"mneme.library.catalog.v1","library_id":"fixture","revision":1,"entries":[{"project_id":"a","db_id":"identity-a","owner_device_id":"mac","display_name":"Routed","database":"not-a-scope","revision":1},{"project_id":"b","db_id":"identity-b","owner_device_id":"elsewhere","display_name":"Unavailable","database":"project","revision":1}]}));
    fixture.write("library/config.json",json!({"schema":"mneme.library.config.v1","library_id":"fixture","device_id":"mac","catalog_path":"catalog.json","rerank":false,"owner_routes":{"mac":{"identity-a":{"url":"http://127.0.0.1:1"}}}}));
    let before = fixture.snapshot();
    let config = fixture.0.join("library/config.json");
    let value = fixture.json(&["--json", "stores", "--config", config.to_str().unwrap()]);
    let sources = value["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[1]["db_id"], "identity-b");
    assert_eq!(sources[1]["route"], "unavailable");
    assert!(
        sources[1]["reason"]
            .as_str()
            .unwrap()
            .contains("descriptor retained")
    );
    assert_eq!(fixture.snapshot(), before);
}
