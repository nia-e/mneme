//! Enrolled CLI commands use the existing owner, never a second local opener.
//! The peer and every configured path are disposable; no installed service or
//! real memory store is involved.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const DB_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const DATABASE: &str = "enrolled-project";

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mneme-cli-owner-{}", ulid::Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        Self { root }
    }

    fn config_at(&self, directory: &Path, bytes: impl AsRef<[u8]>) {
        std::fs::create_dir_all(directory.join(".mneme")).unwrap();
        std::fs::write(directory.join(".mneme/cli.json"), bytes).unwrap();
    }

    fn enroll(&self, peer: &McpPeer) {
        self.config_at(&self.root, owner_config(&peer.url).to_string());
    }

    fn enroll_misc(&self, peer: &McpPeer) -> PathBuf {
        let config = self.root.join("config/mneme/misc.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let mut value = owner_config(&peer.url);
        value["database"] = json!("project");
        std::fs::write(&config, value.to_string()).unwrap();
        config
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_at(&self.root, args, None)
    }

    fn run_at(&self, cwd: &Path, args: &[&str], env_db: Option<&Path>) -> Output {
        self.run_at_with_config_home(cwd, args, env_db, Some(&self.root.join("config")))
    }

    fn run_at_with_config_home(
        &self,
        cwd: &Path,
        args: &[&str],
        env_db: Option<&Path>,
        config_home: Option<&Path>,
    ) -> Output {
        self.run_at_with_env(cwd, args, env_db, config_home, &[])
    }

    fn run_at_with_env(
        &self,
        cwd: &Path,
        args: &[&str],
        env_db: Option<&Path>,
        config_home: Option<&Path>,
        env: &[(&str, &Path)],
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mnemed"));
        command
            .current_dir(cwd)
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("MNEME_DB")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(args);
        if let Some(config) = config_home {
            command.env("XDG_CONFIG_HOME", config);
        }
        if let Some(db) = env_db {
            command.env("MNEME_DB", db);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("run mnemed");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "mnemed exceeded 10 seconds for {args:?}: {}",
                    stderr(&output)
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn snapshot(&self) -> BTreeMap<PathBuf, Entry> {
        fn visit(root: &Path, directory: &Path, entries: &mut BTreeMap<PathBuf, Entry>) {
            for row in std::fs::read_dir(directory).unwrap() {
                let path = row.unwrap().path();
                let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
                let value = if kind.is_symlink() {
                    Entry::Symlink(std::fs::read_link(&path).unwrap())
                } else if kind.is_dir() {
                    visit(root, &path, entries);
                    Entry::Directory
                } else {
                    Entry::File(std::fs::read(&path).unwrap())
                };
                entries.insert(path.strip_prefix(root).unwrap().to_owned(), value);
            }
        }
        let mut entries = BTreeMap::new();
        visit(&self.root, &self.root, &mut entries);
        entries
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn owner_config(url: &str) -> Value {
    json!({"schema":"mneme.cli.owner.v1", "url":url, "database":DATABASE, "db_id":DB_ID})
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[derive(Clone, Copy)]
enum Guard {
    Supported,
    Missing,
    Malformed,
}

#[derive(Clone, Copy)]
enum SaveReply {
    Receipt,
    IdentityChanged,
    WrongIdentityReceipt,
}

struct McpPeer {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl McpPeer {
    fn start() -> Self {
        Self::with_catalog(open_databases(), Guard::Supported, true)
    }

    fn with_catalog(databases: Value, guard: Guard, advertise_databases: bool) -> Self {
        Self::with_reply(databases, guard, advertise_databases, SaveReply::Receipt)
    }

    fn with_reply(
        databases: Value,
        guard: Guard,
        advertise_databases: bool,
        save_reply: SaveReply,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let logged = requests.clone();
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => serve_one(
                        &mut stream,
                        &databases,
                        guard,
                        advertise_databases,
                        save_reply,
                        &logged,
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept MCP request: {error}"),
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn calls(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| request["method"] == "tools/call")
            .map(|request| request["params"].clone())
            .collect()
    }

    fn assert_no_operation(&self) {
        assert!(
            self.calls().iter().all(|call| call["name"] == "databases"),
            "{:?}",
            self.calls()
        );
    }
}

impl Drop for McpPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn open_databases() -> Value {
    // Shape emitted by mneme-mcp's database_status_json, not a CLI invention.
    json!([{"db":DATABASE, "name":DATABASE, "db_id":DB_ID, "state":"open",
        "configured_path":"/fixture/memory.db", "resolved_path":"/fixture/memory.db",
        "in_flight":0, "backend_jobs":0}])
}

fn catalog(guard: Guard, advertise_databases: bool) -> Value {
    let mut tools: Vec<Value> = ["core", "query", "ingest"]
        .into_iter()
        .map(|name| json!({"name":name,"inputSchema":{"type":"object","properties":{}}}))
        .collect();
    // Native guards are required for enrolled reads as well as writes. Do
    // not turn a registry preflight into a pretend atomic identity guarantee.
    for tool in tools
        .iter_mut()
        .filter(|tool| tool["name"] == "core" || tool["name"] == "query")
    {
        match guard {
            Guard::Supported => {
                tool["inputSchema"]["properties"]["expected_db_id"] = json!({"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"})
            }
            Guard::Malformed => {
                tool["inputSchema"]["properties"]["expected_db_id"] = json!({"type":"string"})
            }
            Guard::Missing => {}
        }
    }
    if advertise_databases {
        tools.push(json!({"name":"databases","inputSchema":{"type":"object","properties":{}}}));
    }
    let mut schema = mneme_app::save::input_schema();
    schema["properties"]["db"] = json!({"type":"string"});
    schema["required"].as_array_mut().unwrap().push(json!("db"));
    match guard {
        Guard::Supported => {
            schema["properties"]["expected_db_id"] = json!({"type":"string",
            "minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"})
        }
        Guard::Malformed => schema["properties"]["expected_db_id"] = json!({"type":"string"}),
        Guard::Missing => {}
    }
    tools.push(json!({"name":"save","inputSchema":schema}));
    tools.push(json!({"name":"snapshot_create","inputSchema":{"type":"object","properties":{
        "db":{"type":"string"},"expected_db_id":{"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"}}}}));
    json!({"tools":tools})
}

fn serve_one(
    stream: &mut TcpStream,
    databases: &Value,
    guard: Guard,
    advertised: bool,
    save_reply: SaveReply,
    requests: &Mutex<Vec<Value>>,
) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).expect("read MCP headers");
        assert!(count > 0 && bytes.len() + count <= 1024 * 1024);
        bytes.extend_from_slice(&chunk[..count]);
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).expect("read MCP body");
        assert!(count > 0 && bytes.len() + count <= 1024 * 1024);
        bytes.extend_from_slice(&chunk[..count]);
    }
    let message: Value = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
    };
    if headers.starts_with("DELETE ") {
        requests
            .lock()
            .unwrap()
            .push(json!({"method":"fixture/delete"}));
        respond(stream, "202 Accepted", None, false);
        return;
    }
    requests.lock().unwrap().push(message.clone());
    if message["method"] == "notifications/initialized" {
        respond(stream, "202 Accepted", None, false);
        return;
    }
    let result = match message["method"].as_str() {
        Some("initialize") => json!({"protocolVersion":"2025-11-25", "capabilities":{"tools":{}},
            "serverInfo":{"name":"mneme-mcp","version":"fixture"}}),
        Some("tools/list") => catalog(guard, advertised),
        Some("tools/call")
            if matches!(
                message["params"]["name"].as_str(),
                Some("save" | "core" | "query")
            ) && matches!(save_reply, SaveReply::IdentityChanged) =>
        {
            // Simulate replacement after databases but before the operation's
            // owning checkout. The guard refuses before reads or mutations.
            json!({"content":[{"type":"text","text":"expected_db_id mismatch: target changed; do not retry against a replacement"}],"isError":true})
        }
        Some("tools/call") => {
            let arguments = &message["params"]["arguments"];
            let value = match message["params"]["name"].as_str().unwrap() {
                "databases" => databases.clone(),
                "core" => json!({"nodes":[{"id":DB_ID,"summary":"owner-only","body":"owned core"}],
                    "total":1,"truncated":false,"nodes_truncated":false,"bodies_truncated":false,"body_bytes_limit":1048576}),
                "query" => {
                    json!({"lanes":{"primary":{"hits":[{"id":DB_ID,"summary":"owner-only","status":"active"}]}}})
                }
                "snapshot_create" => {
                    json!({"db":arguments["db"],"db_id":DB_ID,"generation":"fixture","bundle":"/fixture/snapshot"})
                }
                "save" => {
                    let mut claim = arguments.clone();
                    claim.as_object_mut().unwrap().remove("db");
                    claim.as_object_mut().unwrap().remove("expected_db_id");
                    let prepared = mneme_app::save::PreparedSave::parse(&claim, "").unwrap();
                    let mut receipt = json!({"kind":prepared.kind().as_str(),
                        "id":prepared.expected_id().unwrap().0.to_string(), "replayed":false,
                        "origin":prepared.identity().origin.as_str(), "operation_id":prepared.identity().key,
                        "db":arguments["db"], "db_id":DB_ID});
                    if prepared.kind() == mneme_app::save::SaveKind::Episode {
                        receipt["episode_id"] = receipt["id"].clone();
                        receipt["edition_id"] = receipt["id"].clone();
                        receipt["revision"] = json!(0);
                    }
                    if matches!(save_reply, SaveReply::WrongIdentityReceipt) {
                        receipt["db_id"] = json!(OTHER_ID);
                    }
                    receipt
                }
                tool => json!({"fixture_unexpected_operation":tool}),
            };
            json!({"content":[{"type":"text","text":value.to_string()}],"isError":false})
        }
        method => panic!("unexpected MCP method {method:?}"),
    };
    respond(
        stream,
        "200 OK",
        Some(&json!({"jsonrpc":"2.0","id":message["id"],"result":result})),
        message["method"] == "initialize",
    );
}

fn respond(stream: &mut TcpStream, status: &str, body: Option<&Value>, session: bool) {
    let bytes = body
        .map(|value| serde_json::to_vec(value).unwrap())
        .unwrap_or_default();
    let session = if session {
        "mcp-session-id: owner-fixture\r\n"
    } else {
        ""
    };
    write!(stream, "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{session}connection: close\r\n\r\n", bytes.len()).unwrap();
    stream.write_all(&bytes).unwrap();
    stream.flush().unwrap();
}

fn assert_owner_handshake(peer: &McpPeer, operation: &str) {
    let requests = peer.requests();
    let methods: Vec<_> = requests
        .iter()
        .filter_map(|row| row["method"].as_str())
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call",
            "tools/call",
            "fixture/delete"
        ]
    );
    let calls = peer.calls();
    assert_eq!(calls[0], json!({"name":"databases","arguments":{}}));
    assert_eq!(calls[1]["name"], operation);
    assert_eq!(calls[1]["arguments"]["db"], DATABASE);
}

#[test]
fn enrolled_core_and_query_bind_owner_before_read_in_both_output_modes() {
    for args in [
        vec!["core"],
        vec!["--json", "core"],
        vec!["query", "needle", "--k", "3"],
        vec!["--json", "query", "needle"],
    ] {
        let fixture = Fixture::new();
        let peer = McpPeer::start();
        fixture.enroll(&peer);
        let before = fixture.snapshot();
        let output = fixture.run(&args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        assert!(
            stdout(&output).contains("owner-only"),
            "{}",
            stdout(&output)
        );
        assert_owner_handshake(
            &peer,
            if args.contains(&"query") {
                "query"
            } else {
                "core"
            },
        );
        assert_eq!(peer.calls()[1]["arguments"]["expected_db_id"], DB_ID);
        assert_eq!(fixture.snapshot(), before, "owner read changed local files");
    }
}

#[test]
fn enrolled_reads_refuse_old_or_malformed_owner_guards_before_any_database_call() {
    for guard in [Guard::Missing, Guard::Malformed] {
        for args in [
            vec!["core"],
            vec!["--json", "core"],
            vec!["query", "needle"],
            vec!["--json", "query", "needle"],
            vec!["--user", "core"],
        ] {
            let fixture = Fixture::new();
            let peer = McpPeer::with_catalog(open_databases(), guard, true);
            fixture.enroll(&peer);
            let config = fixture.root.join("config/mneme/cli.json");
            std::fs::create_dir_all(config.parent().unwrap()).unwrap();
            std::fs::write(config, owner_config(&peer.url).to_string()).unwrap();
            let before = fixture.snapshot();
            let output = fixture.run(&args);
            assert!(!output.status.success(), "accepted {args:?}");
            let error = stderr(&output);
            assert!(error.contains("expected_db_id"), "{error}");
            assert!(error.contains("owner's mneme-mcp runtime"), "{error}");
            assert!(error.contains("--db"), "{error}");
            assert!(
                peer.calls().is_empty(),
                "unguarded read reached registry or operation"
            );
            assert!(output.stdout.is_empty());
            assert_eq!(fixture.snapshot(), before);
        }
    }
}

#[test]
fn guarded_reads_refuse_replacement_after_registry_without_local_fallback() {
    for args in [
        vec!["core"],
        vec!["--json", "core"],
        vec!["query", "needle"],
        vec!["--json", "query", "needle"],
    ] {
        let fixture = Fixture::new();
        let peer = McpPeer::with_reply(
            open_databases(),
            Guard::Supported,
            true,
            SaveReply::IdentityChanged,
        );
        fixture.enroll(&peer);
        let before = fixture.snapshot();
        let output = fixture.run(&args);
        assert!(!output.status.success());
        assert!(stderr(&output).contains("expected_db_id mismatch"));
        assert!(output.stdout.is_empty());
        assert_owner_handshake(
            &peer,
            if args.contains(&"query") {
                "query"
            } else {
                "core"
            },
        );
        assert_eq!(peer.calls()[1]["arguments"]["expected_db_id"], DB_ID);
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn explicit_remote_remains_a_deliberate_unbound_connection_to_an_old_owner() {
    let fixture = Fixture::new();
    let peer = McpPeer::with_catalog(open_databases(), Guard::Missing, true);
    fixture.enroll(&peer);
    let before = fixture.snapshot();
    let output = fixture.run(&["--remote", &peer.url, "--remote-db", DATABASE, "core"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        peer.calls(),
        [json!({"name":"core","arguments":{"db":DATABASE}})]
    );
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn implicit_snapshot_uses_configured_owner_with_identity_guard() {
    let fixture = Fixture::new();
    let peer = McpPeer::start();
    fixture.enroll(&peer);
    let before = fixture.snapshot();
    let output = fixture.run(&["--json", "snapshot", "create"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_owner_handshake(&peer, "snapshot_create");
    assert_eq!(peer.calls()[1]["arguments"]["expected_db_id"], DB_ID);
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["db_id"], DB_ID);
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn enrolled_save_sends_checked_identity_for_note_and_episode() {
    for kind in ["note", "episode"] {
        let fixture = Fixture::new();
        let peer = McpPeer::start();
        fixture.enroll(&peer);
        let before = fixture.snapshot();
        let output = fixture.run(&[
            "--json",
            "save",
            "An authored memory",
            "--kind",
            kind,
            "--operation-id",
            "owner-test",
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_owner_handshake(&peer, "save");
        let calls = peer.calls();
        let args = &calls[1]["arguments"];
        assert_eq!(args["expected_db_id"], DB_ID);
        assert_eq!(args["kind"], kind);
        assert_eq!(args["summary"], "An authored memory");
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["db_id"], DB_ID);
        assert_eq!(receipt["operation_id"], "owner-test");
        assert_eq!(fixture.snapshot(), before, "owner SAVE changed local files");
    }
}

#[test]
fn missing_wrong_released_or_ambiguous_database_never_reaches_an_operation() {
    let mut wrong_id = open_databases();
    wrong_id[0]["db_id"] = json!(OTHER_ID);
    let mut wrong_alias = open_databases();
    wrong_alias[0]["db"] = json!("other");
    wrong_alias[0]["name"] = json!("other");
    let mut released = open_databases();
    released[0]["state"] = json!("released");
    let mut duplicate = open_databases();
    let repeated = duplicate[0].clone();
    duplicate.as_array_mut().unwrap().push(repeated);
    let mut no_identity = open_databases();
    no_identity[0].as_object_mut().unwrap().remove("db_id");
    for databases in [
        wrong_id,
        wrong_alias,
        released,
        duplicate,
        no_identity,
        json!([]),
        json!({"databases":[]}),
    ] {
        for args in [vec!["core"], vec!["save", "Must not be submitted"]] {
            let fixture = Fixture::new();
            let peer = McpPeer::with_catalog(databases.clone(), Guard::Supported, true);
            fixture.enroll(&peer);
            let before = fixture.snapshot();
            let output = fixture.run(&args);
            assert!(
                !output.status.success(),
                "accepted {databases} for {args:?}"
            );
            peer.assert_no_operation();
            assert_eq!(fixture.snapshot(), before);
        }
    }
}

#[test]
fn implicit_save_requires_advertised_canonical_identity_guard() {
    for guard in [Guard::Missing, Guard::Malformed] {
        let fixture = Fixture::new();
        let peer = McpPeer::with_catalog(open_databases(), guard, true);
        fixture.enroll(&peer);
        let before = fixture.snapshot();
        let output = fixture.run(&["save", "Do not discard the identity guard"]);
        assert!(!output.status.success());
        assert!(
            stderr(&output).contains("expected_db_id"),
            "{}",
            stderr(&output)
        );
        peer.assert_no_operation();
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn implicit_unguarded_operator_write_is_not_submitted() {
    let fixture = Fixture::new();
    let peer = McpPeer::start();
    fixture.enroll(&peer);
    let before = fixture.snapshot();
    let output = fixture.run(&["ingest", "--summary", "Not an implicitly guarded operation"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("expected_db_id"),
        "{}",
        stderr(&output)
    );
    peer.assert_no_operation();
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn absent_database_catalog_refuses_without_local_fallback() {
    let fixture = Fixture::new();
    let peer = McpPeer::with_catalog(open_databases(), Guard::Supported, false);
    fixture.enroll(&peer);
    let before = fixture.snapshot();
    let output = fixture.run(&["core"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("databases"), "{}", stderr(&output));
    assert!(peer.calls().is_empty());
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn malformed_enrollment_is_not_treated_as_unenrolled() {
    let fixture = Fixture::new();
    let peer = McpPeer::start();
    let mut unknown = owner_config(&peer.url);
    unknown["unexpected"] = json!(true);
    let mut null_optional = owner_config(&peer.url);
    null_optional["token_env"] = Value::Null;
    for bytes in [
        b"{".to_vec(),
        unknown.to_string().into_bytes(),
        null_optional.to_string().into_bytes(),
        vec![b' '; 8193],
    ] {
        fixture.config_at(&fixture.root, bytes);
        let before = fixture.snapshot();
        let output = fixture.run(&["core"]);
        assert!(!output.status.success());
        assert!(
            peer.requests().is_empty(),
            "invalid enrollment contacted owner"
        );
        assert_eq!(
            fixture.snapshot(),
            before,
            "invalid enrollment created local state"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinked_enrollment_file_or_directory_fails_closed() {
    for link_directory in [false, true] {
        let fixture = Fixture::new();
        let peer = McpPeer::start();
        if link_directory {
            let target = fixture.root.join("external-config");
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("cli.json"), owner_config(&peer.url).to_string()).unwrap();
            std::os::unix::fs::symlink(target, fixture.root.join(".mneme")).unwrap();
        } else {
            let target = fixture.root.join("owner.json");
            std::fs::write(&target, owner_config(&peer.url).to_string()).unwrap();
            std::fs::create_dir(fixture.root.join(".mneme")).unwrap();
            std::os::unix::fs::symlink(target, fixture.root.join(".mneme/cli.json")).unwrap();
        }
        let before = fixture.snapshot();
        let output = fixture.run(&["core"]);
        assert!(!output.status.success());
        assert!(peer.requests().is_empty());
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn unavailable_owner_does_not_open_a_project_or_user_store() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    drop(listener);
    fixture.config_at(&fixture.root, owner_config(&url).to_string());
    let before = fixture.snapshot();
    for args in [vec!["core"], vec!["save", "No fallback please"]] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn explicit_remote_bypasses_even_invalid_project_enrollment() {
    let fixture = Fixture::new();
    let peer = McpPeer::start();
    fixture.config_at(&fixture.root, "not valid JSON");
    let before = fixture.snapshot();
    let output = fixture.run(&["--remote", &peer.url, "--remote-db", "explicit", "core"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        peer.calls(),
        [json!({"name":"core","arguments":{"db":"explicit"}})]
    );
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn explicit_local_selectors_bypass_even_invalid_project_enrollment() {
    let fixture = Fixture::new();
    fixture.config_at(&fixture.root, "not valid JSON");
    let before = fixture.snapshot();
    // A missing parent proves local dispatch without opening even a disposable store.
    let path = fixture.root.join("missing-parent/explicit.db");
    for (args, env_db) in [
        (
            vec![
                "--db",
                path.to_str().unwrap(),
                "save",
                "Explicit local target",
            ],
            None,
        ),
        (
            vec![
                "--user",
                "--db",
                path.to_str().unwrap(),
                "save",
                "Explicit user offline target",
            ],
            None,
        ),
        (
            vec!["save", "Explicit environment target"],
            Some(path.as_path()),
        ),
    ] {
        let output = fixture.run_at(&fixture.root, &args, env_db);
        assert!(!output.status.success());
        assert!(
            stderr(&output).contains("existing parent directory"),
            "{args:?}: {}",
            stderr(&output)
        );
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn user_owner_uses_native_global_record_and_ignores_project_enrollment() {
    for args in [
        vec!["--user", "core"],
        vec!["--user", "--json", "query", "needle"],
    ] {
        let fixture = Fixture::new();
        let peer = McpPeer::start();
        fixture.config_at(&fixture.root, "invalid project owner JSON");
        let config = fixture.root.join("config/mneme/cli.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        // This owner may advertise any registry alias; --user selects the
        // global record, not a hardcoded rewrite of its database identity.
        std::fs::write(config, owner_config(&peer.url).to_string()).unwrap();
        let before = fixture.snapshot();
        let output = fixture.run(&args);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_owner_handshake(
            &peer,
            if args.contains(&"query") {
                "query"
            } else {
                "core"
            },
        );
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn user_owner_falls_back_to_home_config_only_when_xdg_is_unset() {
    let fixture = Fixture::new();
    let peer = McpPeer::start();
    let config = fixture.root.join("home/.config/mneme/cli.json");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, owner_config(&peer.url).to_string()).unwrap();
    let before = fixture.snapshot();
    let output = fixture.run_at_with_config_home(&fixture.root, &["--user", "core"], None, None);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_owner_handshake(&peer, "core");
    assert_eq!(fixture.snapshot(), before);
    // Explicit XDG location must not quietly borrow an owner from HOME.
    let output = fixture.run(&["--user", "core"]);
    assert!(!output.status.success());
    assert_eq!(peer.calls().len(), 2);
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn project_without_owner_does_not_borrow_the_configured_global_owner() {
    let fixture = Fixture::new();
    let peer = McpPeer::start();
    std::fs::create_dir(fixture.root.join(".git")).unwrap();
    let config = fixture.root.join("config/mneme/cli.json");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, owner_config(&peer.url).to_string()).unwrap();
    let before = fixture.snapshot();
    let output = fixture.run(&["core"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("no project CLI owner configured"));
    assert!(peer.requests().is_empty());
    assert_eq!(fixture.snapshot(), before);
}

fn misc_peer() -> McpPeer {
    let mut databases = open_databases();
    databases[0]["db"] = json!("project");
    databases[0]["name"] = json!("project");
    McpPeer::with_catalog(databases, Guard::Supported, true)
}

#[test]
fn genuinely_unconfigured_cwd_uses_misc_with_stderr_notice_and_clean_json() {
    for args in [
        vec!["core"],
        vec!["--json", "core"],
        vec!["--json", "query", "needle"],
        vec!["--json", "save", "Shared ordinary note"],
    ] {
        let fixture = Fixture::new();
        let peer = misc_peer();
        std::fs::create_dir(fixture.root.join(".git")).unwrap();
        fixture.enroll_misc(&peer);
        let before = fixture.snapshot();
        let output = fixture.run(&args);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(stderr(&output).contains("using the configured shared misc memory owner"));
        assert!(!stdout(&output).contains("no project owner configured"));
        if args.contains(&"--json") {
            serde_json::from_slice::<Value>(&output.stdout).unwrap();
        }
        let calls = peer.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["name"], "databases");
        assert_eq!(calls[1]["arguments"]["db"], "project");
        assert_eq!(calls[1]["arguments"]["expected_db_id"], DB_ID);
        assert_eq!(fixture.snapshot(), before);
        assert!(!fixture.root.join(".mneme").exists());
    }
}

#[test]
fn configured_project_wins_and_never_reads_or_contacts_misc() {
    let fixture = Fixture::new();
    let project = McpPeer::start();
    let misc = misc_peer();
    fixture.enroll(&project);
    let config = fixture.enroll_misc(&misc);
    std::fs::write(config, "malformed misc must be irrelevant").unwrap();
    let before = fixture.snapshot();
    let output = fixture.run(&["--json", "core"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_owner_handshake(&project, "core");
    assert!(misc.requests().is_empty());
    assert!(!stderr(&output).contains("using the configured shared misc"));
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn local_mneme_markers_without_owner_never_become_misc_absence() {
    for marker in [
        "",
        "memory.db",
        "codex-memory.db",
        "current",
        "config.json",
        "service.json",
        "profile.json",
    ] {
        let fixture = Fixture::new();
        let misc = misc_peer();
        fixture.enroll_misc(&misc);
        let directory = fixture.root.join(".mneme");
        std::fs::create_dir(&directory).unwrap();
        if !marker.is_empty() {
            std::fs::write(directory.join(marker), "{}").unwrap();
        }
        let before = fixture.snapshot();
        let output = fixture.run(&["core"]);
        assert!(!output.status.success(), "accepted {marker:?}");
        assert!(
            stderr(&output).contains("without a CLI owner")
                || stderr(&output).contains("shared misc fallback is disabled"),
            "{}",
            stderr(&output)
        );
        assert!(stderr(&output).contains("shared misc fallback is disabled"));
        assert!(misc.requests().is_empty());
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn any_inherited_explicit_profile_survives_nested_git_boundary() {
    for mode in ["default", "private", "isolated"] {
        let fixture = Fixture::new();
        let misc = misc_peer();
        fixture.enroll_misc(&misc);
        fixture.config_at(
            &fixture.root,
            "outer owner must not be inherited across nested Git",
        );
        std::fs::write(
            fixture.root.join(".mneme/profile.json"),
            json!({"schema":"mneme.profile.v1","mode":mode}).to_string(),
        )
        .unwrap();
        let nested = fixture.root.join("nested");
        std::fs::create_dir_all(nested.join(".git")).unwrap();
        let before = fixture.snapshot();
        let output = fixture.run_at(&nested, &["core"], None);
        assert!(!output.status.success());
        assert!(
            stderr(&output).contains("profile forbids shared misc fallback"),
            "{}",
            stderr(&output)
        );
        assert!(misc.requests().is_empty());
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn malformed_inherited_profile_or_project_record_never_contacts_misc() {
    let fixture = Fixture::new();
    let misc = misc_peer();
    fixture.enroll_misc(&misc);
    fixture.config_at(&fixture.root, "invalid owner");
    std::fs::write(
        fixture.root.join(".mneme/profile.json"),
        "invalid native profile",
    )
    .unwrap();
    let nested = fixture.root.join("nested");
    std::fs::create_dir_all(nested.join(".git")).unwrap();
    let before = fixture.snapshot();
    for cwd in [&fixture.root, &nested] {
        let output = fixture.run_at(cwd, &["core"], None);
        assert!(!output.status.success());
        assert!(stderr(&output).contains("invalid"));
        assert!(misc.requests().is_empty());
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn explicit_selectors_ignore_misc_and_user_never_borrows_it() {
    let fixture = Fixture::new();
    let misc = misc_peer();
    fixture.enroll_misc(&misc);
    let remote = McpPeer::start();
    let before = fixture.snapshot();
    let explicit = fixture.run(&["--remote", &remote.url, "core"]);
    assert!(explicit.status.success());
    assert!(!stderr(&explicit).contains("using the configured shared misc"));
    let user = fixture.run(&["--user", "core"]);
    assert!(!user.status.success());
    assert!(misc.requests().is_empty());
    let db = fixture.root.join("missing-parent/offline.db");
    let offline = fixture.run(&["--db", db.to_str().unwrap(), "save", "Offline"]);
    assert!(!offline.status.success());
    assert!(stderr(&offline).contains("existing parent directory"));
    assert!(misc.requests().is_empty());
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn malformed_missing_wrong_scope_or_unavailable_misc_fails_without_local_artifacts() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.root.join(".git")).unwrap();
    let peer = misc_peer();
    let config = fixture.enroll_misc(&peer);
    let mut wrong_scope = owner_config(&peer.url);
    wrong_scope["database"] = json!("user");
    for bytes in ["not JSON".to_owned(), wrong_scope.to_string()] {
        std::fs::write(&config, bytes).unwrap();
        let before = fixture.snapshot();
        let output = fixture.run(&["--json", "core"]);
        assert!(!output.status.success());
        assert!(stderr(&output).contains("misc"));
        assert!(stderr(&output).contains("--db"));
        assert!(output.stdout.is_empty());
        assert!(peer.requests().is_empty());
        assert_eq!(fixture.snapshot(), before);
    }
    std::fs::remove_file(&config).unwrap();
    let before = fixture.snapshot();
    let output = fixture.run(&["core"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("misc.json"));
    assert_eq!(fixture.snapshot(), before);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut unavailable = owner_config(&format!("http://{}/mcp", listener.local_addr().unwrap()));
    unavailable["database"] = json!("project");
    drop(listener);
    std::fs::write(&config, unavailable.to_string()).unwrap();
    let before = fixture.snapshot();
    let output = fixture.run(&["core"]);
    assert!(!output.status.success());
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn missing_enrollment_never_opens_local_project_or_global_storage() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.root.join(".git")).unwrap();
    let before = fixture.snapshot();
    for args in [
        vec!["core"],
        vec!["save", "No implicit local storage"],
        vec!["--user", "core"],
        vec!["--user", "save", "No global fallback"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(stderr(&output).contains("--db"), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("no local fallback"),
            "{}",
            stderr(&output)
        );
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn malformed_or_unavailable_user_owner_fails_without_side_effects() {
    let fixture = Fixture::new();
    let config = fixture.root.join("config/mneme/cli.json");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    drop(listener);
    for bytes in ["not JSON".to_owned(), owner_config(&url).to_string()] {
        std::fs::write(&config, bytes).unwrap();
        let before = fixture.snapshot();
        for args in [
            vec!["--user", "core"],
            vec!["--user", "save", "No local fallback"],
        ] {
            let output = fixture.run(&args);
            assert!(!output.status.success());
            assert_eq!(fixture.snapshot(), before);
        }
    }
}

#[cfg(unix)]
#[test]
fn symlinked_user_record_or_owner_directory_fails_closed() {
    for link_directory in [false, true] {
        let fixture = Fixture::new();
        let peer = McpPeer::start();
        let external = fixture.root.join("external-owner");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(
            external.join("cli.json"),
            owner_config(&peer.url).to_string(),
        )
        .unwrap();
        let directory = fixture.root.join("config/mneme");
        std::fs::create_dir_all(directory.parent().unwrap()).unwrap();
        if link_directory {
            std::os::unix::fs::symlink(&external, &directory).unwrap();
        } else {
            std::fs::create_dir(&directory).unwrap();
            std::os::unix::fs::symlink(external.join("cli.json"), directory.join("cli.json"))
                .unwrap();
        }
        let before = fixture.snapshot();
        let output = fixture.run(&["--user", "core"]);
        assert!(!output.status.success());
        assert!(peer.requests().is_empty());
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn owner_search_is_bounded_before_any_local_admission() {
    let fixture = Fixture::new();
    fixture.config_at(&fixture.root, "must not reach this owner record");
    let mut cwd = fixture.root.clone();
    for _ in 0..65 {
        cwd.push("nested");
    }
    std::fs::create_dir_all(&cwd).unwrap();
    let before = fixture.snapshot();
    let output = fixture.run_at(&cwd, &["core"], None);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("64 ancestors"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn offline_maintenance_always_requires_explicit_database() {
    let fixture = Fixture::new();
    fixture.config_at(
        &fixture.root,
        "malformed owner must not be parsed for maintenance",
    );
    let before = fixture.snapshot();
    for args in [
        vec!["migrate"],
        vec!["--user", "migrate"],
        vec!["reembed"],
        vec!["capture", "init"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        assert!(
            stderr(&output).contains("explicit --db"),
            "{}",
            stderr(&output)
        );
        assert!(!stderr(&output).contains("invalid CLI owner config"));
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn subdirectories_inherit_owner_and_nearest_enrollment_wins() {
    let fixture = Fixture::new();
    let outer = McpPeer::start();
    let inner = McpPeer::start();
    fixture.enroll(&outer);
    let child = fixture.root.join("nested");
    let cwd = child.join("src");
    std::fs::create_dir_all(&cwd).unwrap();
    let before = fixture.snapshot();
    let inherited = fixture.run_at(&cwd, &["core"], None);
    assert!(inherited.status.success(), "{}", stderr(&inherited));
    assert_owner_handshake(&outer, "core");
    assert!(inner.requests().is_empty());
    assert_eq!(fixture.snapshot(), before);
    fixture.config_at(&child, owner_config(&inner.url).to_string());
    let before = fixture.snapshot();
    let nearer = fixture.run_at(&cwd, &["core"], None);
    assert!(nearer.status.success(), "{}", stderr(&nearer));
    assert_owner_handshake(&inner, "core");
    assert_eq!(
        outer.calls().len(),
        2,
        "parent owner was reused over nearer owner"
    );
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn replacement_between_catalog_and_save_is_refused_once_without_fallback() {
    let fixture = Fixture::new();
    let peer = McpPeer::with_reply(
        open_databases(),
        Guard::Supported,
        true,
        SaveReply::IdentityChanged,
    );
    fixture.enroll(&peer);
    let before = fixture.snapshot();
    let output = fixture.run(&[
        "--json",
        "save",
        "Do not write to a replacement",
        "--operation-id",
        "replacement-test",
    ]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("expected_db_id mismatch"),
        "{}",
        stderr(&output)
    );
    assert!(
        output.stdout.is_empty(),
        "refusal emitted a success receipt"
    );
    assert_owner_handshake(&peer, "save");
    assert_eq!(peer.calls()[1]["arguments"]["expected_db_id"], DB_ID);
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn mismatched_write_receipt_is_not_rendered_as_success_or_retried() {
    let fixture = Fixture::new();
    let peer = McpPeer::with_reply(
        open_databases(),
        Guard::Supported,
        true,
        SaveReply::WrongIdentityReceipt,
    );
    fixture.enroll(&peer);
    let before = fixture.snapshot();
    let output = fixture.run(&["--json", "save", "Do not trust a replacement receipt"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("receipt identity mismatch"),
        "{}",
        stderr(&output)
    );
    assert!(stderr(&output).contains("unknown"), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert_owner_handshake(&peer, "save");
    assert_eq!(peer.calls()[1]["arguments"]["expected_db_id"], DB_ID);
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn nested_project_boundaries_do_not_inherit_outer_enrollment() {
    for boundary in [".git", ".mneme/profile.json", ".mneme/memory.db", ".mneme"] {
        let fixture = Fixture::new();
        // If discovery escapes the boundary, parsing this must fail before local dispatch.
        fixture.config_at(&fixture.root, "not valid JSON");
        let project = fixture.root.join("independent");
        let cwd = project.join("src");
        std::fs::create_dir_all(&cwd).unwrap();
        let marker = project.join(boundary);
        if boundary == ".mneme" {
            std::fs::create_dir(&marker).unwrap();
        } else {
            std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
            std::fs::write(marker, "{}").unwrap();
        }
        let before = fixture.snapshot();
        let output = fixture.run_at(&cwd, &["core"], None);
        assert!(!output.status.success());
        assert!(
            stderr(&output).contains("no project CLI owner configured")
                || stderr(&output).contains("without a CLI owner")
                || stderr(&output).contains("shared misc fallback is disabled"),
            "{boundary}: {}",
            stderr(&output)
        );
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn enrolled_offline_operations_require_explicit_database_without_contacting_owner() {
    let fixture = Fixture::new();
    let peer = McpPeer::start();
    fixture.enroll(&peer);
    let before = fixture.snapshot();
    for args in [vec!["migrate"], vec!["reembed"], vec!["capture", "init"]] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(
            stderr(&output).contains("--db"),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(
            peer.requests().is_empty(),
            "offline command contacted owner: {args:?}"
        );
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn independent_bootstrap_and_library_workflows_ignore_enrollment() {
    let fixture = Fixture::new();
    fixture.config_at(&fixture.root, "not valid JSON");
    let clean = fixture.root.join("clean-project");
    std::fs::create_dir_all(clean.join(".git")).unwrap();
    let before = fixture.snapshot();
    let inspect = fixture.run(&[
        "--json",
        "bootstrap-inspect",
        "--root",
        clean.to_str().unwrap(),
    ]);
    assert!(inspect.status.success(), "{}", stderr(&inspect));
    assert!(
        stdout(&inspect).contains("greenfield_absent"),
        "{}",
        stdout(&inspect)
    );
    let library = fixture.run(&["library", "--config", "missing-library.json", "catalog"]);
    assert!(!library.status.success());
    assert!(
        stderr(&library).contains("missing-library.json"),
        "{}",
        stderr(&library)
    );
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn independent_client_and_demo_ignore_enrollment() {
    let fixture = Fixture::new();
    fixture.config_at(&fixture.root, "not valid JSON");
    let peer = McpPeer::start();
    let before = fixture.snapshot();
    // EOF has no client operation to submit, but must reach its standalone loop.
    let client = fixture.run(&[
        "client",
        "--endpoint",
        &peer.url,
        "--timeout-ms",
        "1000",
        "--expected-server",
        "mneme-mcp",
        "--local-only",
    ]);
    assert!(client.status.success(), "{}", stderr(&client));
    assert!(peer.requests().is_empty());
    let demo = fixture.run(&["demo", "--query", "Rust futures"]);
    assert!(demo.status.success(), "{}", stderr(&demo));
    assert_eq!(fixture.snapshot(), before);
}

/// Shared structural cases with integrations/codex/test_misc_binding.py:
/// configured routing is not absence; arbitrary text mentioning Mneme is.
#[test]
fn codex_owned_enrollment_blocks_misc_across_nested_git_without_network() {
    let cases = [
        (
            "config.toml",
            "[mcp_servers.mneme_project]\ncommand = 'echo'\n",
        ),
        (
            "config.toml",
            "[mcp_servers.other]\ncommand = 'python'\nargs = ['/neutral/launcher.py', '--service-config', '/neutral/service.json']\n",
        ),
        (
            "config.toml",
            "[mcp_servers.other]\ncommand = '/neutral/mneme-mcp'\n",
        ),
        ("config.toml", "# BEGIN mneme-codex-v1\n"),
        ("config.toml", "notify = ['python', '/neutral/hooks.py']\n"),
        (
            "config.toml",
            "[[hooks.UserPromptSubmit]]\n[[hooks.UserPromptSubmit.hooks]]\ntype = 'command'\ncommand = 'python /neutral/hook_launcher.py'\n",
        ),
        (
            "config.toml",
            "[[hooks.UserPromptSubmit]]\n[[hooks.UserPromptSubmit.hooks]]\ntype = 'mcp_tool'\nserver = 'mneme_project'\ntool = 'recall'\n",
        ),
        (
            "hooks.json",
            r#"{"schema":"mneme.codex.hooks.v1","mode":"off"}"#,
        ),
        (
            "hooks.json",
            r#"{"schema":"mneme.codex.hooks.v1","mode":"reminder"}"#,
        ),
        (
            "hooks.json",
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"python '/neutral path/hooks.py' --config /neutral/config.json"}]}]}}"#,
        ),
        (
            "hooks.json",
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"mcp_tool","server":"mneme_project","tool":"recall"}]}]}}"#,
        ),
    ];
    for (file, raw) in cases {
        let fixture = Fixture::new();
        let misc = misc_peer();
        fixture.enroll_misc(&misc);
        std::fs::create_dir(fixture.root.join(".codex")).unwrap();
        std::fs::write(fixture.root.join(".codex").join(file), raw).unwrap();
        let nested = fixture.root.join("nested");
        std::fs::create_dir_all(nested.join(".git")).unwrap();
        let before = fixture.snapshot();
        let output = fixture.run_at(&nested, &["--json", "core"], None);
        assert!(!output.status.success(), "{file}: {raw}");
        assert!(
            stderr(&output).contains("shared misc fallback is disabled"),
            "{}",
            stderr(&output)
        );
        assert!(misc.requests().is_empty());
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn unrelated_codex_metadata_and_device_hooks_do_not_self_suppress_misc() {
    let cases = [
        (
            "config.toml",
            "developer_instructions = 'remember Mneme someday'\n[mcp_servers.other]\ncommand = 'echo'\n",
        ),
        (
            "config.toml",
            "[[hooks.UserPromptSubmit]]\n[[hooks.UserPromptSubmit.hooks]]\ntype = 'command'\ncommand = 'echo okay'\nstatusMessage = 'Working on Mneme'\n[hooks.state]\nmneme = 'not routing'\n",
        ),
        (
            "hooks.json",
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"echo okay","statusMessage":"Working on Mneme"}]}]}}"#,
        ),
    ];
    for (file, raw) in cases {
        let fixture = Fixture::new();
        let misc = misc_peer();
        fixture.enroll_misc(&misc);
        std::fs::create_dir(fixture.root.join(".codex")).unwrap();
        std::fs::write(fixture.root.join(".codex").join(file), raw).unwrap();
        let before = fixture.snapshot();
        let output = fixture.run(&["--json", "core"]);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(fixture.snapshot(), before);
    }
    let fixture = Fixture::new();
    let misc = misc_peer();
    fixture.enroll_misc(&misc);
    let home = fixture.root.join("home");
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(
        home.join(".codex/config.toml"),
        "[mcp_servers.mneme_global]\ncommand = 'mneme-mcp'\n",
    )
    .unwrap();
    let before = fixture.snapshot();
    let output = fixture.run_at(&home, &["--json", "core"], None);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn malformed_bounded_codex_configuration_never_becomes_misc_absence() {
    let cases = [
        ("config.toml", "malformed = [".to_owned()),
        (
            "config.toml",
            "[mcp_servers.other]\ncommand = 'echo'\nurl = 'http://localhost'\n".to_owned(),
        ),
        (
            "config.toml",
            "[mcp_servers.other]\ncommand = 'echo'\nargs = 3\n".to_owned(),
        ),
        ("config.toml", "#".repeat(65537)),
        ("hooks.json", "{}".to_owned()),
        ("hooks.json", r#"{"hooks":{},"hooks":{}}"#.to_owned()),
        (
            "hooks.json",
            r#"{"hooks":{"UserPromptSubmit":{}}}"#.to_owned(),
        ),
        (
            "hooks.json",
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":4}]}]}}"#
                .to_owned(),
        ),
        (
            "hooks.json",
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"mcp_tool","server":4}]}]}}"#
                .to_owned(),
        ),
    ];
    for (file, raw) in cases {
        let fixture = Fixture::new();
        let misc = misc_peer();
        fixture.enroll_misc(&misc);
        std::fs::create_dir(fixture.root.join(".codex")).unwrap();
        std::fs::write(fixture.root.join(".codex").join(file), raw).unwrap();
        let before = fixture.snapshot();
        let output = fixture.run(&["--json", "core"]);
        assert!(!output.status.success());
        assert!(misc.requests().is_empty());
        assert_eq!(fixture.snapshot(), before);
    }
}

#[cfg(unix)]
#[test]
fn symlinked_codex_configuration_is_not_misc_absence() {
    for directory_link in [false, true] {
        let fixture = Fixture::new();
        let misc = misc_peer();
        fixture.enroll_misc(&misc);
        std::fs::create_dir(fixture.root.join("actual")).unwrap();
        std::fs::write(fixture.root.join("actual/config.toml"), "").unwrap();
        if directory_link {
            std::os::unix::fs::symlink("actual", fixture.root.join(".codex")).unwrap();
        } else {
            std::fs::create_dir(fixture.root.join(".codex")).unwrap();
            std::os::unix::fs::symlink(
                "../actual/config.toml",
                fixture.root.join(".codex/config.toml"),
            )
            .unwrap();
        }
        let before = fixture.snapshot();
        let output = fixture.run(&["core"]);
        assert!(!output.status.success());
        assert!(stderr(&output).contains("must not be a symlink"));
        assert!(misc.requests().is_empty());
        assert_eq!(fixture.snapshot(), before);
    }
}

#[cfg(unix)]
#[test]
fn verified_shell_alias_preserves_private_ancestry_but_stale_pwd_is_ignored() {
    let fixture = Fixture::new();
    let misc = misc_peer();
    fixture.enroll_misc(&misc);
    let actual = fixture.root.join("actual");
    std::fs::create_dir_all(actual.join(".git")).unwrap();
    let private = fixture.root.join("private");
    std::fs::create_dir_all(private.join(".mneme")).unwrap();
    std::fs::write(
        private.join(".mneme/profile.json"),
        r#"{"schema":"mneme.profile.v1","mode":"private"}"#,
    )
    .unwrap();
    let alias = private.join("alias");
    std::os::unix::fs::symlink(&actual, &alias).unwrap();
    let before = fixture.snapshot();
    let output = fixture.run_at_with_env(
        &actual,
        &["--json", "core"],
        None,
        Some(&fixture.root.join("config")),
        &[("PWD", &alias)],
    );
    assert!(!output.status.success());
    assert!(stderr(&output).contains("profile forbids shared misc fallback"));
    assert!(misc.requests().is_empty());
    assert_eq!(fixture.snapshot(), before);

    // A valid but different PWD cannot redirect/fence the operation's cwd.
    let output = fixture.run_at_with_env(
        &actual,
        &["--json", "core"],
        None,
        Some(&fixture.root.join("config")),
        &[("PWD", &private)],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn custom_codex_home_is_device_configuration_not_project_enrollment() {
    let fixture = Fixture::new();
    let misc = misc_peer();
    fixture.enroll_misc(&misc);
    let custom = fixture.root.join(".codex");
    std::fs::create_dir(&custom).unwrap();
    std::fs::write(
        custom.join("config.toml"),
        "[mcp_servers.mneme_global]\ncommand = 'mneme-mcp'\n",
    )
    .unwrap();
    let before = fixture.snapshot();
    let output = fixture.run_at_with_env(
        &fixture.root,
        &["--json", "core"],
        None,
        Some(&fixture.root.join("config")),
        &[("CODEX_HOME", &custom)],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn codex_hook_shapes_match_python_routing_not_authored_text() {
    let cases = [
        (
            "[[hooks.UserPromptSubmit]]\n[[hooks.UserPromptSubmit.hooks]]\ntype = 'prompt'\nprompt = 'remember Mneme'\n",
            true,
        ),
        (
            "[[hooks.UserPromptSubmit]]\n[[hooks.UserPromptSubmit.hooks]]\ntype = 'agent'\nprompt = 'Mneme is a memory tool'\n",
            true,
        ),
        (
            "[hooks]\nenabled = false\n[hooks.state]\nmneme = 'metadata'\n",
            true,
        ),
        ("[hooks]\nmanaged_dir = '/neutral/handlers'\n", false),
        (
            "[[hooks.UserPromptSubmit]]\n[[hooks.UserPromptSubmit.hooks]]\ntype = 'mcp_tool'\nserver = 'other'\ntool = 'mneme_read'\n",
            false,
        ),
        ("[hooks]\nUserPromptSubmit = 'unknown'\n", false),
        ("[hooks]\nenabled = 'wrong'\n", false),
        (
            "[[hooks.UserPromptSubmit]]\n[[hooks.UserPromptSubmit.hooks]]\ntype = 'other'\n",
            false,
        ),
        (
            "[[hooks.UserPromptSubmit]]\n[[hooks.UserPromptSubmit.hooks]]\ntype = 'mcp_tool'\nserver = 'other'\n",
            false,
        ),
    ];
    for (raw, eligible) in cases {
        let fixture = Fixture::new();
        let misc = misc_peer();
        fixture.enroll_misc(&misc);
        std::fs::create_dir(fixture.root.join(".codex")).unwrap();
        std::fs::write(fixture.root.join(".codex/config.toml"), raw).unwrap();
        let before = fixture.snapshot();
        let output = fixture.run(&["--json", "core"]);
        assert_eq!(
            output.status.success(),
            eligible,
            "{raw}: {}",
            stderr(&output)
        );
        if !eligible {
            assert!(misc.requests().is_empty());
        }
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn misc_device_exclusions_refuse_ancestry_without_network_but_not_explicit_sources() {
    let fixture = Fixture::new();
    let misc = misc_peer();
    let config = fixture.enroll_misc(&misc);
    let private = fixture.root.join("private");
    let cwd = private.join("nested");
    std::fs::create_dir_all(&cwd).unwrap();
    for excluded in [&private, &private.canonicalize().unwrap()] {
        let mut value: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        value["excluded_roots"] = json!([excluded]);
        std::fs::write(&config, value.to_string()).unwrap();
        let before = fixture.snapshot();
        let output = fixture.run_at(&cwd, &["--json", "core"], None);
        assert!(!output.status.success());
        assert!(stderr(&output).contains("workspace is excluded from shared misc memory"));
        assert!(misc.requests().is_empty());
        assert_eq!(fixture.snapshot(), before);
    }
    // Prefixes are path components, not textual prefixes; private-other is not private.
    let sibling = fixture.root.join("private-other");
    std::fs::create_dir(&sibling).unwrap();
    let output = fixture.run_at(&sibling, &["--json", "core"], None);
    assert!(output.status.success(), "{}", stderr(&output));
    let output = fixture.run_at(&cwd, &["--remote", &misc.url, "--json", "core"], None);
    assert!(output.status.success(), "{}", stderr(&output));
    let user_config = config.with_file_name("cli.json");
    let mut record = owner_config(&misc.url);
    record["database"] = json!("project");
    std::fs::write(user_config, record.to_string()).unwrap();
    let before = fixture.snapshot();
    let output = fixture.run_at(&cwd, &["--user", "--json", "core"], None);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stderr(&output).contains("shared misc"));
    assert_eq!(fixture.snapshot(), before);
}

#[cfg(unix)]
#[test]
fn misc_exclusions_compare_verified_alias_and_resolved_excluded_roots() {
    let fixture = Fixture::new();
    let misc = misc_peer();
    let config = fixture.enroll_misc(&misc);
    let actual = fixture.root.join("actual");
    let private = fixture.root.join("private");
    std::fs::create_dir(&actual).unwrap();
    std::fs::create_dir(&private).unwrap();
    let alias = private.join("alias");
    std::os::unix::fs::symlink(&actual, &alias).unwrap();
    for (excluded, pwd) in [(&private, &alias), (&alias, &actual)] {
        let mut value: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        value["excluded_roots"] = json!([excluded]);
        std::fs::write(&config, value.to_string()).unwrap();
        let before = fixture.snapshot();
        let output = fixture.run_at_with_env(
            &actual,
            &["--json", "core"],
            None,
            Some(&fixture.root.join("config")),
            &[("PWD", pwd)],
        );
        assert!(!output.status.success());
        assert!(stderr(&output).contains("workspace is excluded from shared misc memory"));
        assert!(misc.requests().is_empty());
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn invalid_misc_exclusion_metadata_is_not_absence_and_project_global_reject_it() {
    let fixture = Fixture::new();
    let misc = misc_peer();
    let config = fixture.enroll_misc(&misc);
    let mut record = owner_config(&misc.url);
    record["database"] = json!("project");
    for excluded in [
        Value::Null,
        json!("/private"),
        json!([7]),
        json!(["relative"]),
        json!(["/private/../other"]),
        json!(["/private/./other"]),
        json!(["/private//other"]),
        json!(["/private/"]),
        json!(["/private", "/private"]),
        json!(["/nul\u{0}path"]),
        json!(vec!["/private"; 65]),
        json!([format!("/{}", "x".repeat(4096))]),
    ] {
        record["excluded_roots"] = excluded;
        std::fs::write(&config, record.to_string()).unwrap();
        let before = fixture.snapshot();
        let output = fixture.run(&["--json", "core"]);
        assert!(!output.status.success());
        assert!(misc.requests().is_empty());
        assert_eq!(fixture.snapshot(), before);
    }
    record["excluded_roots"] = json!([]);
    fixture.config_at(&fixture.root, record.to_string());
    let before = fixture.snapshot();
    let output = fixture.run(&["core"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("supported only in shared misc.json"));
    assert!(misc.requests().is_empty());
    assert_eq!(fixture.snapshot(), before);
    std::fs::write(config.with_file_name("cli.json"), record.to_string()).unwrap();
    let before = fixture.snapshot();
    let output = fixture.run(&["--user", "core"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("supported only in shared misc.json"));
    assert!(misc.requests().is_empty());
    assert_eq!(fixture.snapshot(), before);
}

#[cfg(unix)]
#[test]
fn workspace_codex_symlink_to_device_config_still_refuses_misc() {
    let fixture = Fixture::new();
    let misc = misc_peer();
    fixture.enroll_misc(&misc);
    let device = fixture.root.join("device-codex");
    std::fs::create_dir(&device).unwrap();
    std::fs::write(device.join("config.toml"), "").unwrap();
    std::os::unix::fs::symlink(&device, fixture.root.join(".codex")).unwrap();
    let before = fixture.snapshot();
    let output = fixture.run_at_with_env(
        &fixture.root,
        &["core"],
        None,
        Some(&fixture.root.join("config")),
        &[("CODEX_HOME", &device)],
    );
    assert!(!output.status.success());
    assert!(stderr(&output).contains("must not be a symlink"));
    assert!(misc.requests().is_empty());
    assert_eq!(fixture.snapshot(), before);
}
