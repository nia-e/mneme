//! Black-box remote CLI tests. The server is deliberately a small MCP peer,
//! not a local Mneme store: remote mode must never admit one by accident.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const NODE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mneme-cli-remote-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&root).unwrap();
        Self { root }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mnemed"))
            .current_dir(&self.root)
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env_remove("MNEME_DB")
            .args(args)
            .output()
            .expect("run mnemed")
    }

    fn assert_no_local_store(&self) {
        assert!(
            !self.root.join(".mneme").exists(),
            "remote command created .mneme"
        );
        assert!(
            !self.root.join("data/mneme").exists(),
            "remote command created user store"
        );
        assert!(!self.root.join("home/.local/share/mneme").exists());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[derive(Clone, Copy)]
enum ReplyMode {
    Success,
    TouchstoneList,
    CaptureOld,
    CaptureMalformed,
    CaptureLinked,
    Save,
    SaveMalformed,
    SaveRefused,
    Concern,
    ConcernWrongOwner,
    SummaryEdit,
    SummaryEditWrongOwner,
    SummaryEditUnchangedGuard,
    RpcError,
    ToolError,
    Hidden,
}

struct McpPeer {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl McpPeer {
    fn start(mode: ReplyMode) -> Self {
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
                    Ok((mut stream, _)) => serve_one(&mut stream, mode, &logged),
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

    fn calls(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for McpPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn serve_one(stream: &mut TcpStream, mode: ReplyMode, requests: &Mutex<Vec<Value>>) {
    // Accepted sockets can inherit the listener's nonblocking mode on macOS.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut data = Vec::new();
    let header_end = loop {
        if let Some(position) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).expect("read MCP headers");
        assert!(count > 0 && data.len() + count <= 1024 * 1024);
        data.extend_from_slice(&chunk[..count]);
    };
    let headers = String::from_utf8_lossy(&data[..header_end]).into_owned();
    let first = headers.lines().next().unwrap_or("");
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0);
    while data.len() < header_end + length {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).expect("read MCP body");
        assert!(count > 0 && data.len() + count <= 1024 * 1024);
        data.extend_from_slice(&chunk[..count]);
    }
    let message: Value = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&data[header_end..header_end + length]).expect("JSON-RPC request")
    };
    requests
        .lock()
        .unwrap()
        .push(json!({"request_line": first, "headers": headers.to_string(), "message": message}));

    if first.starts_with("DELETE ") || message["method"] == "notifications/initialized" {
        write_response(stream, "202 Accepted", None, false);
        return;
    }
    let id = message["id"].clone();
    let result = match message["method"].as_str() {
        Some("initialize") => {
            json!({"protocolVersion":"2025-11-25", "capabilities":{"tools":{}}, "serverInfo":{"name":"mcp-fixture","version":"1"}})
        }
        Some("tools/list") => {
            let names: &[&str] = match mode {
                ReplyMode::Hidden => &["status", "get", "query"],
                _ => &[
                    "status",
                    "get",
                    "query",
                    "ingest",
                    "remote_edges",
                    "core",
                    "recall_context",
                    "episode",
                ],
            };
            let mut tools: Vec<Value> = names
                .iter()
                .map(|name| {
                    let schema = if *name == "episode" {
                        mneme_app::episode::input_schema(&mneme_app::episode::EpisodeAction::ALL)
                    } else {
                        json!({"type":"object","properties":{}})
                    };
                    json!({"name":name,"inputSchema":schema})
                })
                .collect();
            if matches!(mode, ReplyMode::TouchstoneList) {
                let mut schema = mneme_app::touchstone::list_input_schema();
                schema["properties"]["db"] = json!({"type":"string"});
                schema["required"].as_array_mut().unwrap().push(json!("db"));
                tools.push(json!({"name":"list","inputSchema":schema}));
            }
            if matches!(
                mode,
                ReplyMode::CaptureOld | ReplyMode::CaptureMalformed | ReplyMode::CaptureLinked
            ) {
                let properties = match mode {
                    ReplyMode::CaptureLinked => {
                        json!({"links":{"type":"array","items":{"type":"object","required":["to"],"properties":{"to":{"type":"string"},"kind":{"type":"string","enum":["derived_from","associative","transition"]},"weight":{"type":"number"}}}}})
                    }
                    ReplyMode::CaptureMalformed => {
                        json!({"links":{"type":"array","items":{"type":"object","properties":{"to":{"type":"number"}}}}})
                    }
                    _ => json!({}),
                };
                tools.push(json!({"name":"capture","inputSchema":{"type":"object","properties":properties}}));
            }
            if matches!(
                mode,
                ReplyMode::Save | ReplyMode::SaveMalformed | ReplyMode::SaveRefused
            ) {
                let mut schema = mneme_app::save::input_schema();
                schema["properties"]["db"] = json!({"type":"string"});
                schema["properties"]["expected_db_id"] = json!({"type":"string"});
                schema["required"].as_array_mut().unwrap().push(json!("db"));
                tools.push(json!({"name":"save","inputSchema":schema}));
            }
            if matches!(mode, ReplyMode::Concern | ReplyMode::ConcernWrongOwner) {
                let mut schema =
                    mneme_app::concern::input_schema(&["list", "notice", "record_finding"]);
                schema["properties"]["db"] = json!({"type":"string"});
                schema["properties"]["expected_db_id"] = json!({"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"});
                schema["allOf"] = json!([{"if":{"properties":{"action":{"enum":["notice","record_finding"]}},"required":["action"]},"then":{"required":["db","expected_db_id"]}}]);
                tools.push(json!({"name":"concern","inputSchema":schema}));
            }
            if matches!(
                mode,
                ReplyMode::SummaryEdit
                    | ReplyMode::SummaryEditWrongOwner
                    | ReplyMode::SummaryEditUnchangedGuard
            ) {
                let mut schema = mneme_app::edit_summary::input_schema();
                schema["properties"]["db"] = json!({"type":"string"});
                schema["properties"]["expected_db_id"] = json!({"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"});
                schema["required"]
                    .as_array_mut()
                    .unwrap()
                    .extend([json!("db"), json!("expected_db_id")]);
                tools.push(json!({"name":"edit_summary","inputSchema":schema}));
            }
            json!({"tools": tools})
        }
        Some("tools/call") if matches!(mode, ReplyMode::RpcError) => {
            let response = json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":"fixture RPC refusal"}});
            write_response(stream, "200 OK", Some(&response), false);
            return;
        }
        Some("tools/call") if matches!(mode, ReplyMode::ToolError) => {
            json!({"content":[{"type":"text","text":"fixture tool refusal"}],"isError":true})
        }
        Some("tools/call") if matches!(mode, ReplyMode::SaveRefused) => {
            json!({"content":[{"type":"text","text":"SAVE owner is released; explicitly resume the existing owner"}],"isError":true})
        }
        Some("tools/call") => {
            let tool = message["params"]["name"].as_str().unwrap_or("");
            let db = message["params"]["arguments"]["db"].as_str().unwrap_or("");
            let value = match tool {
                "list" => {
                    json!({"db":db,"db_id":NODE,"kind":"touchstones","items":[],"next_cursor":null,"has_more":false})
                }
                "concern" => {
                    let args = &message["params"]["arguments"];
                    let owner = if matches!(mode, ReplyMode::ConcernWrongOwner) {
                        "other"
                    } else {
                        db
                    };
                    if args["action"] == "list" {
                        json!({"db":owner,"db_id":NODE,"action":"list","page":{"items":[],"next":null}})
                    } else {
                        json!({"db":owner,"db_id":NODE,"action":args["action"],"outcome":{"status":"refused","reason":"missing_endpoint","row":null}})
                    }
                }
                "edit_summary" => {
                    let args = &message["params"]["arguments"];
                    let owner = if matches!(mode, ReplyMode::SummaryEditWrongOwner) {
                        "other"
                    } else {
                        db
                    };
                    let guard = if matches!(mode, ReplyMode::SummaryEditUnchangedGuard) {
                        args["expected_snapshot_sha256"].clone()
                    } else {
                        json!("b".repeat(64))
                    };
                    json!({"db":owner,"db_id":NODE,"id":args["id"],"summary_snapshot_sha256":guard})
                }
                "save" => {
                    let mut raw = message["params"]["arguments"].clone();
                    raw.as_object_mut().unwrap().remove("db");
                    let prepared = mneme_app::save::PreparedSave::parse(&raw, "").unwrap();
                    let mut receipt = json!({"kind":prepared.kind().as_str(),"id":prepared.expected_id().unwrap().0.to_string(),"replayed":false,"origin":prepared.identity().origin.as_str(),"operation_id":prepared.identity().key,"db":db,"db_id":NODE});
                    if prepared.identity().origin == mneme_app::save::SaveOrigin::ProvidedSource {
                        receipt.as_object_mut().unwrap().remove("operation_id");
                    }
                    if prepared.kind() == mneme_app::save::SaveKind::Episode {
                        receipt["episode_id"] = receipt["id"].clone();
                        receipt["edition_id"] = receipt["id"].clone();
                        receipt["revision"] = json!(0);
                    }
                    if matches!(mode, ReplyMode::SaveMalformed) {
                        receipt["id"] = json!(NODE);
                    }
                    receipt
                }
                "status" => {
                    json!({"fixture_tool":tool,"fixture_db":db,"fixture_marker":"remote-only","nodes":1,"active":1,"archived":0,"open_contradictions":0,"open_merge_candidates":0,"edge_decay_pending":0})
                }
                "get" => {
                    let mut value = json!({"fixture_tool":tool,"fixture_db":db,"id":NODE,"summary":"remote-only","status":"active","tags":[],"edges":[]});
                    if message["params"]["arguments"]["body"] == true {
                        let offset = message["params"]["arguments"]["body_offset"]
                            .as_u64()
                            .unwrap_or(0);
                        let (body, next) = if offset == 0 {
                            ("part", json!(4))
                        } else {
                            ("tail", Value::Null)
                        };
                        value["body"] = json!(body);
                        value["body_range"] = json!({"source_start":offset,"source_end":offset+4,"next_offset":next,"has_more":next.is_number()});
                    }
                    value
                }
                "query" => {
                    json!({"fixture_tool":tool,"fixture_db":db,"lanes":{"primary":{"hits":[{"id":NODE,"summary":"remote-only","status":"active"}]}}})
                }
                "remote_edges" => {
                    json!({"fixture_tool":tool,"fixture_db":db,"items":[{"target":NODE,"target_db":"user","weight":0.5}],"next":null})
                }
                "core" => {
                    json!({"fixture_tool":tool,"fixture_db":db,"nodes":[{"id":NODE,"summary":"remote-only","body":"core body"}],"total":1,"truncated":false,"nodes_truncated":false,"bodies_truncated":false,"body_bytes_limit":1048576})
                }
                "recall_context" => {
                    json!({"schema":"mneme.recall-context.v1","fixture_marker":"remote-only","items":[]})
                }
                "episode" => {
                    json!({"action":message["params"]["arguments"]["action"],"fixture_tool":tool,"fixture_db":db,"fixture_marker":"remote-only","items":[],"next":null,"partial":false})
                }
                _ => json!({"fixture_tool":tool,"fixture_db":db,"fixture_marker":"remote-only"}),
            };
            json!({"content":[{"type":"text","text":value.to_string()}],"isError":false})
        }
        other => panic!("unexpected MCP method: {other:?}"),
    };
    let response = json!({"jsonrpc":"2.0","id":id,"result":result});
    write_response(
        stream,
        "200 OK",
        Some(&response),
        message["method"] == "initialize",
    );
}

fn write_response(stream: &mut TcpStream, status: &str, body: Option<&Value>, session: bool) {
    let bytes = body
        .map(|value| serde_json::to_vec(value).unwrap())
        .unwrap_or_default();
    let session_header = if session {
        "mcp-session-id: fixture-session\r\n"
    } else {
        ""
    };
    write!(stream, "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{session_header}connection: close\r\n\r\n", bytes.len()).unwrap();
    stream.write_all(&bytes).unwrap();
    stream.flush().unwrap();
}

fn assert_handshake(peer: &McpPeer) {
    let requests = peer.calls();
    let methods: Vec<_> = requests
        .iter()
        .filter_map(|request| request["message"]["method"].as_str())
        .collect();
    assert_eq!(methods.first(), Some(&"initialize"), "{requests:?}");
    assert!(
        methods.contains(&"notifications/initialized"),
        "{requests:?}"
    );
    assert!(methods.contains(&"tools/list"), "{requests:?}");
    assert!(
        requests.iter().any(|request| request["request_line"]
            .as_str()
            .unwrap()
            .starts_with("DELETE ")),
        "session not closed: {requests:?}"
    );
    for request in &requests {
        assert!(
            request["request_line"]
                .as_str()
                .unwrap()
                .contains(" /mcp HTTP/1.1")
        );
    }
}

#[test]
fn remote_status_uses_mcp_without_opening_local_store() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Success);
    let output = fixture.run(&["--remote", &peer.url, "--json", "status"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("remote-only"),
        "{}",
        stdout(&output)
    );
    assert_handshake(&peer);
    let calls = peer.calls();
    let call = calls
        .iter()
        .find(|request| request["message"]["method"] == "tools/call")
        .unwrap();
    assert_eq!(call["message"]["params"]["name"], "status");
    assert_eq!(call["message"]["params"]["arguments"]["db"], "project");
    fixture.assert_no_local_store();
}

#[test]
fn episode_actions_forward_one_shared_request_without_local_state() {
    let fixture = Fixture::new();
    let cases = [
        (
            vec!["episode", "list", "--thread", "lantern", "--limit", "2"],
            json!({"action":"list","thread":"lantern","limit":2,"db":"user"}),
        ),
        (
            vec!["episode", "search", "violet", "--unknown-occurrence"],
            json!({"action":"search","cue":"violet","occurrence":{"kind":"unknown"},"db":"user"}),
        ),
        (
            vec![
                "episode",
                "get",
                NODE,
                "--body",
                "--offset",
                "4",
                "--max-bytes",
                "16",
            ],
            json!({"action":"get","episode_id":NODE,"body":true,"offset":4,"max_bytes":16,"db":"user"}),
        ),
        (
            vec!["episode", "history", NODE],
            json!({"action":"history","episode_id":NODE,"db":"user"}),
        ),
        (
            vec!["episode", "references", NODE],
            json!({"action":"references","anchor":NODE,"db":"user"}),
        ),
    ];
    for (argv, expected) in cases {
        let peer = McpPeer::start(ReplyMode::Success);
        let mut args = vec!["--remote", &peer.url, "--remote-db", "user", "--json"];
        args.extend_from_slice(&argv);
        let output = fixture.run(&args);
        assert!(output.status.success(), "{argv:?}: {}", stderr(&output));
        let calls = peer.calls();
        let calls: Vec<_> = calls
            .iter()
            .filter(|row| row["message"]["method"] == "tools/call")
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["message"]["params"]["name"], "episode");
        assert_eq!(calls[0]["message"]["params"]["arguments"], expected);
        assert_handshake(&peer);
        fixture.assert_no_local_store();
    }

    let request = json!({"source":{"namespace":"cli-test","key":"one","reference":"test://remote"},"summary":"A brief experience"});
    std::fs::write(fixture.root.join("episode.json"), request.to_string()).unwrap();
    let peer = McpPeer::start(ReplyMode::Success);
    let output = fixture.run(&[
        "--remote",
        &peer.url,
        "--json",
        "episode",
        "append",
        "--input",
        "episode.json",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let mut expected = request;
    expected["action"] = json!("append");
    expected["db"] = json!("project");
    let calls = peer.calls();
    let calls: Vec<_> = calls
        .iter()
        .filter(|row| row["message"]["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["message"]["params"]["arguments"], expected);
    fixture.assert_no_local_store();

    expected["action"] = json!("revise");
    expected["episode_id"] = json!(NODE);
    expected["expected_edition_id"] = json!(NODE);
    expected["reason"] = json!("Correct the account");
    let mut input = expected.clone();
    for field in ["action", "episode_id", "db"] {
        input.as_object_mut().unwrap().remove(field);
    }
    std::fs::write(fixture.root.join("revision.json"), input.to_string()).unwrap();
    let peer = McpPeer::start(ReplyMode::Success);
    let output = fixture.run(&[
        "--remote",
        &peer.url,
        "--json",
        "episode",
        "revise",
        NODE,
        "--input",
        "revision.json",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let calls = peer.calls();
    let calls: Vec<_> = calls
        .iter()
        .filter(|row| row["message"]["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["message"]["params"]["arguments"], expected);
    fixture.assert_no_local_store();
}

#[test]
fn invalid_episode_or_local_upgrade_refuses_before_remote_connection() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Success);
    for args in [
        vec!["--remote", &peer.url, "episode", "list", "--limit", "33"],
        vec![
            "--remote", &peer.url, "episode", "list", "--after", "invalid",
        ],
        vec!["--remote", &peer.url, "episode", "search", " "],
        vec![
            "--remote",
            &peer.url,
            "single-graph-upgrade",
            "--backend",
            "json",
            "--output",
            "/unused/single-graph.db",
        ],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}");
    }
    assert!(
        peer.calls().is_empty(),
        "invalid episode made network requests"
    );
    fixture.assert_no_local_store();
}

#[test]
fn old_remote_server_refuses_episode_without_sending_a_tool_call() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Hidden);
    let output = fixture.run(&["--remote", &peer.url, "episode", "list"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("episode"));
    assert!(
        !peer
            .calls()
            .iter()
            .any(|row| row["message"]["method"] == "tools/call")
    );
    fixture.assert_no_local_store();
}

#[test]
fn remote_user_and_named_database_select_logical_registry_entries() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Success);
    for (selectors, expected) in [
        (&["--user"][..], "user"),
        (&["--remote-db", "staging"][..], "staging"),
    ] {
        let mut args = vec!["--remote", peer.url.as_str()];
        args.extend_from_slice(selectors);
        args.extend_from_slice(&["--json", "status"]);
        let output = fixture.run(&args);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(stdout(&output).contains(expected), "{}", stdout(&output));
    }
    let dbs: Vec<_> = peer
        .calls()
        .into_iter()
        .filter(|request| request["message"]["method"] == "tools/call")
        .map(|request| {
            request["message"]["params"]["arguments"]["db"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(dbs, ["user", "staging"]);
    fixture.assert_no_local_store();
}

#[test]
fn remote_get_and_query_keep_command_arguments_and_human_output() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Success);
    let get = fixture.run(&["--remote", &peer.url, "get", NODE, "--edges"]);
    assert!(get.status.success(), "{}", stderr(&get));
    assert!(stdout(&get).contains("remote-only"), "{}", stdout(&get));
    let query = fixture.run(&["--remote", &peer.url, "query", "needle", "--k", "3"]);
    assert!(query.status.success(), "{}", stderr(&query));
    assert!(stdout(&query).contains("remote-only"), "{}", stdout(&query));
    let calls: Vec<_> = peer
        .calls()
        .into_iter()
        .filter(|request| request["message"]["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["message"]["params"]["name"], "get");
    assert_eq!(calls[0]["message"]["params"]["arguments"]["id"], NODE);
    assert_eq!(calls[0]["message"]["params"]["arguments"]["edges"], true);
    assert_eq!(calls[1]["message"]["params"]["name"], "query");
    assert_eq!(calls[1]["message"]["params"]["arguments"]["text"], "needle");
    assert_eq!(calls[1]["message"]["params"]["arguments"]["k"], 3);
    fixture.assert_no_local_store();
}

#[test]
fn core_and_recall_context_render_native_mcp_payloads() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Success);
    let core_json = fixture.run(&["--remote", &peer.url, "--json", "core"]);
    assert!(core_json.status.success(), "{}", stderr(&core_json));
    let core: Value = serde_json::from_slice(&core_json.stdout).unwrap();
    assert_eq!(core["nodes"][0]["summary"], "remote-only");
    assert_eq!(core["nodes"][0]["body"], "core body");
    assert_eq!(core["total"], 1);

    let core_human = fixture.run(&["--remote", &peer.url, "core"]);
    assert!(core_human.status.success(), "{}", stderr(&core_human));
    assert!(stdout(&core_human).contains("# remote-only\ncore body"));

    // The real MCP server puts compact context JSON in one text block. After
    // transport decoding it is an Object, not a quoted JSON string.
    let context = fixture.run(&["--remote", &peer.url, "recall-context", "needle"]);
    assert!(context.status.success(), "{}", stderr(&context));
    let context_json: Value = serde_json::from_slice(&context.stdout).unwrap();
    assert!(context_json.is_object(), "{}", stdout(&context));
    assert_eq!(context_json["fixture_marker"], "remote-only");
    assert_eq!(stdout(&context).trim(), context_json.to_string());

    let calls: Vec<_> = peer
        .calls()
        .into_iter()
        .filter(|request| request["message"]["method"] == "tools/call")
        .map(|request| request["message"]["params"]["name"].clone())
        .collect();
    assert_eq!(calls, ["core", "core", "recall_context"]);
    fixture.assert_no_local_store();
}

#[test]
fn get_body_and_body_command_preserve_source_byte_continuation() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Success);
    let first = fixture.run(&["--remote", &peer.url, "body", NODE, "--max-bytes", "4"]);
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(stdout(&first), "part");
    assert!(stderr(&first).contains("continue with --offset 4"));

    let resumed = fixture.run(&[
        "--remote",
        &peer.url,
        "--json",
        "body",
        NODE,
        "--offset",
        "4",
        "--max-bytes",
        "4",
    ]);
    assert!(resumed.status.success(), "{}", stderr(&resumed));
    let body: Value = serde_json::from_slice(&resumed.stdout).unwrap();
    assert_eq!(body["body"], "tail");
    assert_eq!(body["source_start"], 4);
    assert_eq!(body["source_end"], 8);
    assert_eq!(body["next_offset"], Value::Null);
    assert_eq!(body["has_more"], false);

    let get = fixture.run(&[
        "--remote",
        &peer.url,
        "--json",
        "get",
        NODE,
        "--body",
        "--body-offset",
        "4",
        "--max-body-bytes",
        "4",
    ]);
    assert!(get.status.success(), "{}", stderr(&get));
    let node: Value = serde_json::from_slice(&get.stdout).unwrap();
    assert_eq!(node["body"], "tail");
    assert_eq!(node["body_range"]["next_offset"], Value::Null);
    let calls: Vec<_> = peer
        .calls()
        .into_iter()
        .filter(|request| request["message"]["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 3);
    for call in &calls {
        assert_eq!(call["message"]["params"]["name"], "get");
        assert_eq!(call["message"]["params"]["arguments"]["body"], true);
        assert_eq!(call["message"]["params"]["arguments"]["max_body_bytes"], 4);
    }
    assert_eq!(calls[1]["message"]["params"]["arguments"]["body_offset"], 4);
    assert_eq!(calls[2]["message"]["params"]["arguments"]["body_offset"], 4);
    fixture.assert_no_local_store();
}

#[test]
fn hidden_catalog_tool_cannot_be_dispatched() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Hidden);
    let output = fixture.run(&["--remote", &peer.url, "ingest", "--summary", "nope"]);
    assert!(!output.status.success(), "{}", stdout(&output));
    assert!(
        !peer
            .calls()
            .iter()
            .any(|request| request["message"]["method"] == "tools/call"),
        "hidden ingest was dispatched"
    );
    fixture.assert_no_local_store();
}

#[test]
fn remote_error_forms_are_failures_not_success_json() {
    for (mode, expected) in [
        (ReplyMode::RpcError, "fixture RPC refusal"),
        (ReplyMode::ToolError, "fixture tool refusal"),
    ] {
        let fixture = Fixture::new();
        let peer = McpPeer::start(mode);
        let output = fixture.run(&["--remote", &peer.url, "--json", "status"]);
        assert!(!output.status.success(), "{}", stdout(&output));
        assert!(stderr(&output).contains(expected), "{}", stderr(&output));
        fixture.assert_no_local_store();
    }
}

#[test]
fn mutating_tool_error_has_one_call_and_no_success_shaped_output() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::ToolError);
    let output = fixture.run(&[
        "--remote",
        &peer.url,
        "--json",
        "ingest",
        "--summary",
        "remote write",
        "--body",
        "inline body",
    ]);
    assert!(!output.status.success(), "{}", stdout(&output));
    assert!(stderr(&output).contains("fixture tool refusal"));
    assert!(
        output.stdout.is_empty(),
        "write failure printed success: {}",
        stdout(&output)
    );
    let calls: Vec<_> = peer
        .calls()
        .into_iter()
        .filter(|request| request["message"]["method"] == "tools/call")
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "write was retried after an ambiguous failure"
    );
    assert_eq!(calls[0]["message"]["params"]["name"], "ingest");
    assert_eq!(
        calls[0]["message"]["params"]["arguments"]["summary"],
        "remote write"
    );
    assert_eq!(
        calls[0]["message"]["params"]["arguments"]["body"],
        "inline body"
    );
    assert_eq!(calls[0]["message"]["params"]["arguments"]["db"], "project");
    fixture.assert_no_local_store();
}

#[test]
fn token_is_read_from_environment_and_sent_only_as_bearer_header() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Success);
    let output = Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .current_dir(&fixture.root)
        .env("HOME", fixture.root.join("home"))
        .env("XDG_DATA_HOME", fixture.root.join("data"))
        .env_remove("MNEME_DB")
        .env("MNEME_REMOTE_TEST_TOKEN", "fixture-token")
        .args([
            "--remote",
            &peer.url,
            "--remote-token-env",
            "MNEME_REMOTE_TEST_TOKEN",
            "--json",
            "status",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    for request in peer.calls() {
        let headers = request["headers"].as_str().unwrap().to_ascii_lowercase();
        if !headers.starts_with("delete ") {
            assert!(headers.contains("authorization: bearer fixture-token"));
        }
        assert!(
            !request["request_line"]
                .as_str()
                .unwrap()
                .contains("fixture-token")
        );
    }
    assert!(!stdout(&output).contains("fixture-token"));
    fixture.assert_no_local_store();
}

#[test]
fn selector_conflicts_and_offline_commands_fail_before_network_or_store() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Success);
    for args in [
        vec![
            "--remote",
            peer.url.as_str(),
            "--db",
            "would-create/store.db",
            "status",
        ],
        vec![
            "--remote",
            peer.url.as_str(),
            "--user",
            "--remote-db",
            "custom",
            "status",
        ],
        vec!["--remote-db", "custom", "status"],
        vec!["--remote", peer.url.as_str(), "bootstrap-inspect"],
        vec!["--remote", peer.url.as_str(), "migrate"],
        vec!["--remote", peer.url.as_str(), "body", NODE, "--raw"],
        vec!["--remote", peer.url.as_str(), "query", "text", "--bodies"],
        vec!["--remote", peer.url.as_str(), "query", ""],
        vec!["--remote", peer.url.as_str(), "ingest", "--summary", "   "],
        vec![
            "--remote",
            peer.url.as_str(),
            "link",
            "--from",
            NODE,
            "--to",
            NODE,
            "--anchor",
            "4:2",
        ],
        vec![
            "--remote",
            peer.url.as_str(),
            "neighbors",
            NODE,
            "--limit",
            "65",
        ],
    ] {
        let before = peer.calls().len();
        let output = fixture.run(&args);
        assert!(
            !output.status.success(),
            "accepted {args:?}: {}",
            stdout(&output)
        );
        assert_eq!(peer.calls().len(), before, "network touched for {args:?}");
        fixture.assert_no_local_store();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .current_dir(&fixture.root)
        .env("MNEME_DB", "would-create/env.db")
        .args(["--remote", &peer.url, "status"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!fixture.root.join("would-create").exists());
}

#[test]
fn named_connection_requires_explicit_remote_and_config() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Success);
    let config = fixture.root.join("remotes.json");
    std::fs::write(
        &config,
        json!({"version":1,"remotes":{"pi":{"url":peer.url,"ssh_mcp_port":null,"token_env":null,"database":"user"}}}).to_string(),
    ).unwrap();
    let path = config.to_str().unwrap();
    let missing = fixture.run(&["--remote", "pi", "--json", "status"]);
    assert!(!missing.status.success());
    let missing_value = fixture.run(&["--remote"]);
    assert!(!missing_value.status.success());
    let local = fixture.run(&["--remote-config", path, "status"]);
    assert!(!local.status.success());
    assert!(peer.calls().is_empty());
    let named = fixture.run(&[
        "--remote",
        "pi",
        "--remote-config",
        path,
        "--json",
        "status",
    ]);
    assert!(named.status.success(), "{}", stderr(&named));
    let call = peer
        .calls()
        .into_iter()
        .find(|request| request["message"]["method"] == "tools/call")
        .unwrap();
    assert_eq!(call["message"]["params"]["arguments"]["db"], "user");
    fixture.assert_no_local_store();
}

#[test]
fn remote_subcommand_is_cross_database_pager_not_endpoint_selector() {
    let fixture = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Success);
    let output = fixture.run(&["--remote", &peer.url, "remote", NODE]);
    assert!(output.status.success(), "{}", stderr(&output));
    let calls = peer.calls();
    let call = calls
        .iter()
        .find(|request| request["message"]["method"] == "tools/call")
        .unwrap();
    assert_eq!(call["message"]["params"]["name"], "remote_edges");
    assert_eq!(call["message"]["params"]["arguments"]["id"], NODE);
    assert!(stdout(&output).contains("@user"), "{}", stdout(&output));
    let json_output = fixture.run(&["--remote", &peer.url, "--json", "remote", NODE]);
    assert!(json_output.status.success(), "{}", stderr(&json_output));
    let page: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(page["items"][0]["target"], NODE);
    assert_eq!(page["items"][0]["target_db"], "user");
    fixture.assert_no_local_store();
}

#[test]
fn linked_capture_requires_advertised_schema_and_forwards_exact_links() {
    let fixture = Fixture::new();
    let input = fixture.root.join("capture.json");
    std::fs::write(
        &input,
        json!({
            "source": {"namespace":"codex", "key":"claim-1", "reference":"codex://thread/1"},
            "summary":"sourced claim",
            "links":[{"to":NODE,"kind":"derived_from","weight":0.7}]
        })
        .to_string(),
    )
    .unwrap();
    let path = input.to_str().unwrap();

    let old = McpPeer::start(ReplyMode::CaptureOld);
    let refused = fixture.run(&[
        "--remote", &old.url, "--user", "capture", "add", "--input", path,
    ]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("compatible `links` schema"),
        "{}",
        stderr(&refused)
    );
    assert!(
        !old.calls()
            .iter()
            .any(|call| call["message"]["method"] == "tools/call")
    );
    assert_handshake(&old);
    let plain_input = fixture.root.join("plain.json");
    std::fs::write(
        &plain_input,
        json!({
            "source": {"namespace":"codex", "key":"claim-plain", "reference":"codex://thread/1"},
            "summary":"plain sourced claim"
        })
        .to_string(),
    )
    .unwrap();
    let plain = fixture.run(&[
        "--remote",
        &old.url,
        "--user",
        "--json",
        "capture",
        "add",
        "--input",
        plain_input.to_str().unwrap(),
    ]);
    assert!(plain.status.success(), "{}", stderr(&plain));
    let plain_call = old
        .calls()
        .into_iter()
        .find(|call| call["message"]["method"] == "tools/call")
        .unwrap();
    assert!(
        plain_call["message"]["params"]["arguments"]
            .get("links")
            .is_none()
    );

    let malformed = McpPeer::start(ReplyMode::CaptureMalformed);
    let refused = fixture.run(&[
        "--remote",
        &malformed.url,
        "--user",
        "capture",
        "add",
        "--input",
        path,
    ]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("compatible `links` schema"),
        "{}",
        stderr(&refused)
    );
    assert!(
        !malformed
            .calls()
            .iter()
            .any(|call| call["message"]["method"] == "tools/call")
    );

    let linked = McpPeer::start(ReplyMode::CaptureLinked);
    let accepted = fixture.run(&[
        "--remote",
        &linked.url,
        "--user",
        "--json",
        "capture",
        "add",
        "--input",
        path,
    ]);
    assert!(accepted.status.success(), "{}", stderr(&accepted));
    let call = linked
        .calls()
        .into_iter()
        .find(|call| call["message"]["method"] == "tools/call")
        .unwrap();
    assert_eq!(call["message"]["params"]["name"], "capture");
    let forwarded = &call["message"]["params"]["arguments"]["links"][0];
    assert_eq!(forwarded["to"], NODE);
    assert_eq!(forwarded["kind"], "derived_from");
    assert!((forwarded["weight"].as_f64().unwrap() - 0.7).abs() < 1e-6);
    assert_eq!(call["message"]["params"]["arguments"]["db"], "user");
    fixture.assert_no_local_store();
}

#[test]
fn remote_save_refuses_old_server_without_write_and_freezes_nonce_once() {
    let f = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Success);
    let out = f.run(&["--remote", &peer.url, "save", "a manual note"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("remote save is unavailable"));
    assert_eq!(stderr(&out).matches("save operation_id:").count(), 1);
    assert!(
        !peer
            .calls()
            .iter()
            .any(|r| r["message"]["method"] == "tools/call")
    );
    f.assert_no_local_store();
}

#[test]
fn remote_save_uses_canonical_contract_and_shared_human_json_receipt() {
    let f = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Save);
    let out = f.run(&[
        "--remote",
        &peer.url,
        "--json",
        "save",
        "a manual note",
        "--operation-id",
        "remote-save-1",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let receipt: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(receipt["kind"], "note");
    assert_eq!(receipt["operation_id"], "remote-save-1");
    assert_eq!(receipt["origin"], "manual_submission");
    assert!(receipt.get("body").is_none());
    let human = f.run(&[
        "--remote",
        &peer.url,
        "save",
        "a manual note",
        "--operation-id",
        "remote-save-1",
    ]);
    assert!(human.status.success(), "{}", stderr(&human));
    assert_eq!(
        stdout(&human).trim(),
        mneme_app::save::render_human(&receipt)
    );
    let calls: Vec<_> = peer
        .calls()
        .into_iter()
        .filter(|r| r["message"]["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 2);
    for c in calls {
        assert_eq!(c["message"]["params"]["name"], "save");
        let a = &c["message"]["params"]["arguments"];
        assert_eq!(a["source"]["key"], "remote-save-1");
        assert!(a.get("operation_id").is_none());
        assert_eq!(a["kind"], "note");
    }
    f.assert_no_local_store();
}

#[test]
fn remote_save_checks_receipt_identity_without_retrying_unknown_write() {
    let f = Fixture::new();
    let peer = McpPeer::start(ReplyMode::SaveMalformed);
    let out = f.run(&[
        "--remote",
        &peer.url,
        "--json",
        "save",
        "a manual note",
        "--operation-id",
        "remote-save-bad-receipt",
    ]);
    assert!(!out.status.success());
    assert!(stdout(&out).is_empty());
    assert_eq!(
        peer.calls()
            .iter()
            .filter(|r| r["message"]["method"] == "tools/call")
            .count(),
        1
    );
    f.assert_no_local_store();
}

#[test]
fn remote_episode_save_and_sourced_json_keep_the_same_domain_contract() {
    let f = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Save);
    let request = json!({"kind":"episode","summary":"One remembered experience","source":{"namespace":"fixture","key":"episode-save","reference":"fixture://experience"}});
    std::fs::write(f.root.join("save.json"), request.to_string()).unwrap();
    let out = f.run(&[
        "--remote",
        &peer.url,
        "--json",
        "save",
        "--input",
        "save.json",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let receipt: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(receipt["kind"], "episode");
    assert_eq!(receipt["origin"], "provided_source");
    assert_eq!(receipt["edition_id"], receipt["id"]);
    assert_eq!(receipt["episode_id"], receipt["id"]);
    assert_eq!(receipt["revision"], 0);
    assert!(receipt.get("operation_id").is_none());
    assert!(!stderr(&out).contains("save operation_id:"));
    let calls: Vec<_> = peer
        .calls()
        .into_iter()
        .filter(|r| r["message"]["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["message"]["params"]["arguments"]["source"],
        request["source"]
    );
    f.assert_no_local_store();
}

#[test]
fn remote_save_release_refusal_is_one_write_attempt_and_no_success_receipt() {
    let f = Fixture::new();
    let peer = McpPeer::start(ReplyMode::SaveRefused);
    let out = f.run(&["--remote", &peer.url, "--json", "save", "a note"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("released"));
    assert!(stdout(&out).is_empty());
    assert_eq!(
        peer.calls()
            .iter()
            .filter(|r| r["message"]["method"] == "tools/call")
            .count(),
        1
    );
    f.assert_no_local_store();
}

#[test]
fn malformed_remote_save_is_rejected_before_any_connection() {
    let f = Fixture::new();
    let peer = McpPeer::start(ReplyMode::Save);
    std::fs::write(
        f.root.join("bad-save.json"),
        r#"{"summary":"x","kind":"wrong"}"#,
    )
    .unwrap();
    let out = f.run(&["--remote", &peer.url, "save", "--input", "bad-save.json"]);
    assert!(!out.status.success());
    assert!(peer.calls().is_empty());
    f.assert_no_local_store();
}

#[test]
fn remote_concern_lists_or_returns_atomic_refusal_once_without_local_owner_or_fallback() {
    let f = Fixture::new();
    let endpoint = NODE;
    let list = json!({"action":"list","endpoint":endpoint});
    std::fs::write(f.root.join("concern.json"), list.to_string()).unwrap();
    let old = McpPeer::start(ReplyMode::Hidden);
    let out = f.run(&["--remote", &old.url, "concern", "--input", "concern.json"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("concern"));
    assert!(
        !old.calls()
            .iter()
            .any(|r| r["message"]["method"] == "tools/call")
    );
    let peer = McpPeer::start(ReplyMode::Concern);
    let out = f.run(&[
        "--remote",
        &peer.url,
        "--json",
        "concern",
        "--input",
        "concern.json",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["page"], json!({"items":[],"next":null}));
    let calls: Vec<_> = peer
        .calls()
        .into_iter()
        .filter(|r| r["message"]["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["message"]["params"]["arguments"]["limit"],
        mneme_app::concern::MAX_CONCERN_PUBLIC_PAGE_ROWS
    );
    let human = f.run(&["--remote", &peer.url, "concern", "--input", "concern.json"]);
    assert!(human.status.success());
    assert!(stdout(&human).contains("concerns in"));
    use mneme_core::*;
    let binding = ConcernBinding::new(
        ConcernKind::Disagreement,
        ConcernEndpoint::new(NodeId(ulid::Ulid::from(1)), ConcernDigest::of_bytes(b"a")),
        ConcernEndpoint::new(NodeId(ulid::Ulid::from(2)), ConcernDigest::of_bytes(b"b")),
    )
    .unwrap();
    let mut notice = serde_json::to_value(ConcernUpdate::Notice(
        ConcernNotice::new(binding, "Claims differ", "Which context applies?").unwrap(),
    ))
    .unwrap();
    notice["expected_db_id"] = json!(NODE);
    std::fs::write(f.root.join("concern.json"), notice.to_string()).unwrap();
    let before = peer.calls().len();
    let out = f.run(&[
        "--remote",
        &peer.url,
        "--json",
        "concern",
        "--input",
        "concern.json",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["outcome"]["status"], "refused");
    let calls: Vec<_> = peer
        .calls()
        .into_iter()
        .skip(before)
        .filter(|r| r["message"]["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["message"]["params"]["name"], "concern");
    assert_eq!(
        calls[0]["message"]["params"]["arguments"]["expected_db_id"],
        NODE
    );
    let bad = McpPeer::start(ReplyMode::ConcernWrongOwner);
    let out = f.run(&[
        "--remote",
        &bad.url,
        "--json",
        "concern",
        "--input",
        "concern.json",
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("do not blindly retry"));
    assert_eq!(
        bad.calls()
            .iter()
            .filter(|r| r["message"]["method"] == "tools/call")
            .count(),
        1
    );
    f.assert_no_local_store();
}

#[test]
fn remote_touchstone_list_forwards_one_indexed_request_without_local_store() {
    let f = Fixture::new();
    let peer = McpPeer::start(ReplyMode::TouchstoneList);
    let out = f.run(&[
        "--remote",
        &peer.url,
        "--remote-db",
        "project",
        "--json",
        "list",
        "--touchstones",
        "--limit",
        "2",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["kind"], "touchstones");
    assert!(result["next_cursor"].is_null());
    let calls = peer
        .calls()
        .into_iter()
        .filter(|call| call["message"]["method"] == "tools/call")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["message"]["params"]["name"], "list");
    assert_eq!(
        calls[0]["message"]["params"]["arguments"],
        json!({"db":"project","kind":"touchstones","limit":2})
    );
    f.assert_no_local_store();
    let before = peer.calls().len();
    for args in [
        vec!["list", "--limit", "65"],
        vec!["list", "--touchstones", "--tag", "touchstone"],
        vec!["list", "--touchstones", "--limit", "33"],
        vec!["list", "--touchstones", "--after", "bad-cursor"],
    ] {
        let mut command = vec!["--remote", peer.url.as_str()];
        command.extend(args);
        assert!(!f.run(&command).status.success());
        assert_eq!(peer.calls().len(), before);
    }
}

#[test]
fn old_remote_capture_cannot_silently_ignore_native_touchstone_metadata() {
    let f = Fixture::new();
    let peer = McpPeer::start(ReplyMode::CaptureOld);
    let raw = json!({"summary":"Why the scene mattered","source":{"namespace":"test","key":"scene","reference":"test://scene"},
        "touchstone":{"subject":"Historical meaning","references":[{"db_id":NODE,"id":NODE,"expected_snapshot_sha256":"a".repeat(64)}]}});
    std::fs::write(f.root.join("touchstone.json"), raw.to_string()).unwrap();
    let out = f.run(&[
        "--remote",
        &peer.url,
        "capture",
        "add",
        "--input",
        "touchstone.json",
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("touchstone"));
    assert!(
        !peer
            .calls()
            .iter()
            .any(|call| call["message"]["method"] == "tools/call")
    );
    f.assert_no_local_store();
}

#[test]
fn remote_summary_edit_is_one_shot_owner_bound_and_never_opens_local_storage() {
    for mode in [
        ReplyMode::SummaryEdit,
        ReplyMode::SummaryEditWrongOwner,
        ReplyMode::SummaryEditUnchangedGuard,
        ReplyMode::Success,
    ] {
        let f = Fixture::new();
        let peer = McpPeer::start(mode);
        let guard = "a".repeat(64);
        let output = f.run(&[
            "--remote",
            &peer.url,
            "--remote-db",
            "project",
            "--json",
            "edit-summary",
            NODE,
            "--expected-snapshot-sha256",
            &guard,
            "--summary",
            "replacement",
            "--expected-db-id",
            NODE,
        ]);
        let calls: Vec<_> = peer
            .calls()
            .into_iter()
            .filter(|call| call["message"]["method"] == "tools/call")
            .collect();
        match mode {
            ReplyMode::SummaryEdit => {
                assert!(output.status.success(), "{}", stderr(&output));
                let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(reply["summary_snapshot_sha256"], "b".repeat(64));
            }
            ReplyMode::Success => {
                assert!(!output.status.success());
                assert!(stderr(&output).contains("no fallback"));
                assert!(calls.is_empty());
            }
            _ => {
                assert!(!output.status.success());
                assert!(stderr(&output).contains("outcome is unknown"));
            }
        }
        if !matches!(mode, ReplyMode::Success) {
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0]["message"]["params"],
                json!({"name":"edit_summary","arguments":{"db":"project","expected_db_id":NODE,"id":NODE,"expected_snapshot_sha256":guard,"summary":"replacement"}})
            );
        }
        f.assert_no_local_store();
    }
}
