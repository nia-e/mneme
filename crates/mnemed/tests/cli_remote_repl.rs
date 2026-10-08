//! Native shell protocol tests with an isolated bounded MCP peer; no live owner.
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

const ROOT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const NEXT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const DB_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    HiddenWalk,
    HiddenReflect,
    WrongDb,
    NoGuard,
    BadReceipt,
    ReflectError,
    ReflectRpcError,
    AbortError,
    BadStart,
    GoError,
}
struct Peer {
    url: String,
    calls: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Peer {
    fn start(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, stopping) = (calls.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, mode, &log),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            }
        });
        Self {
            url,
            calls,
            stop,
            worker: Some(worker),
        }
    }
    fn tools(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call["method"] == "tools/call")
            .map(|call| call["params"].clone())
            .collect()
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn view(id: &str) -> Value {
    json!({"at":id,"summary":"memory","status":"active","visited":if id == ROOT {1} else {2},"budget":25,"depth":if id == ROOT {0} else {1},"edges":[],"edge_count":0})
}
fn catalog(mode: Mode) -> Vec<Value> {
    let guard = json!({"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"});
    let mut walk = json!({"name":"walk","inputSchema":{"type":"object","properties":{"action":{"type":"string","enum":["start","look","edges","body","go","back","done","abort"]},"expected_db_id":guard}}});
    let mut reflect = json!({"name":"reflect","inputSchema":{"type":"object","properties":{"expected_db_id":guard}}});
    if mode == Mode::NoGuard {
        walk["inputSchema"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("expected_db_id");
        reflect["inputSchema"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("expected_db_id");
    }
    let mut tools = vec![json!({"name":"databases","inputSchema":{"type":"object"}})];
    if mode != Mode::HiddenWalk {
        tools.push(walk);
    }
    if mode != Mode::HiddenReflect {
        tools.push(reflect);
    }
    tools
}
fn serve(mut stream: TcpStream, mode: Mode, log: &Mutex<Vec<Value>>) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut data = Vec::new();
    let end = loop {
        if let Some(index) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).unwrap();
        if n == 0 {
            return;
        }
        assert!(data.len() + n <= 128 * 1024);
        data.extend_from_slice(&chunk[..n]);
    };
    let headers = String::from_utf8_lossy(&data[..end]).into_owned();
    let len = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while data.len() < end + len {
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0 && data.len() + n <= 128 * 1024);
        data.extend_from_slice(&chunk[..n]);
    }
    if headers.starts_with("DELETE ") {
        assert!(
            headers
                .to_lowercase()
                .contains("mcp-session-id: fixture-session")
        );
        log.lock().unwrap().push(json!({"method":"DELETE"}));
        respond(&mut stream, None);
        return;
    }
    let message: Value = serde_json::from_slice(&data[end..end + len]).unwrap();
    if message["method"] != "initialize" {
        assert!(
            headers
                .to_lowercase()
                .contains("mcp-session-id: fixture-session")
        );
    }
    log.lock().unwrap().push(message.clone());
    if message["method"] == "notifications/initialized" {
        respond(&mut stream, None);
        return;
    }
    if mode == Mode::ReflectRpcError
        && message["method"] == "tools/call"
        && message["params"]["name"] == "reflect"
    {
        respond(
            &mut stream,
            Some(
                json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32602,"message":"bad reflect request"}}),
            ),
        );
        return;
    }
    let mut tool_error = false;
    let result = match message["method"].as_str().unwrap() {
        "initialize" => {
            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"mneme-mcp","version":"fixture"}})
        }
        "tools/list" => json!({"tools":catalog(mode)}),
        "tools/call" => {
            let args = &message["params"]["arguments"];
            let payload = match message["params"]["name"].as_str().unwrap() {
                "databases" => {
                    json!([{"db":"project","db_id":if mode == Mode::WrongDb {ROOT} else {DB_ID},"state":"open"},{"db":"user","db_id":DB_ID,"state":"open"}])
                }
                "walk" => match args["action"].as_str().unwrap() {
                    "start" if mode == Mode::BadStart => {
                        json!({"session":"session-token","view":{}})
                    }
                    "start" => json!({"session":"session-token","view":view(ROOT)}),
                    "go" if mode == Mode::GoError => {
                        tool_error = true;
                        json!("no such neighbor")
                    }
                    "go" => view(NEXT),
                    "look" | "back" => view(ROOT),
                    "edges" => json!({"edges":[]}),
                    "body" => json!({"body":"bounded body","has_more":true,"next_offset":4096}),
                    "abort" if mode == Mode::AbortError => {
                        tool_error = true;
                        json!("abort refused")
                    }
                    "abort" => json!({"trail":[{"node":ROOT,"from":null}]}),
                    "done" if mode == Mode::BadReceipt => {
                        json!({"trail":[{"node":ROOT,"from":null}]})
                    }
                    "done" => {
                        json!({"receipt":"receipt-token","trail":[{"node":ROOT,"from":null},{"node":NEXT,"from":ROOT}]})
                    }
                    action => panic!("unknown action {action}"),
                },
                "reflect" if mode == Mode::ReflectError => {
                    tool_error = true;
                    json!("reflection refused")
                }
                "reflect" => {
                    json!({"db":args["db"],"db_id":DB_ID,"reinforced":1,"interfered":0,"bridged":0})
                }
                name => panic!("unknown tool {name}"),
            };
            json!({"content":[{"type":"text","text": if tool_error {payload.as_str().unwrap().to_string()} else {payload.to_string()}}],"isError":tool_error})
        }
        method => panic!("unknown method {method}"),
    };
    respond(
        &mut stream,
        Some(json!({"jsonrpc":"2.0","id":message["id"],"result":result})),
    );
}
fn respond(stream: &mut TcpStream, payload: Option<Value>) {
    let body = payload.map(|p| p.to_string()).unwrap_or_default();
    write!(stream, "HTTP/1.1 {}\r\nContent-Type: application/json\r\nMcp-Session-Id: fixture-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", if body.is_empty() {"202 Accepted"} else {"200 OK"}, body.len(), body).unwrap();
}
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("mneme-remote-repl-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(path.join(".git")).unwrap();
        Self(path)
    }
    fn enroll(&self, peer: &Peer, user: bool) {
        let dir = if user {
            self.0.join("config/mneme")
        } else {
            self.0.join(".mneme")
        };
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cli.json"), json!({"schema":"mneme.cli.owner.v1","url":peer.url,"database":if user {"user"} else {"project"},"db_id":DB_ID}).to_string()).unwrap();
    }
    fn run(&self, args: &[&str], input: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mnemed"))
            .current_dir(&self.0)
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env_remove("MNEME_DB")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() > deadline {
                child.kill().unwrap();
                panic!("remote repl did not terminate within 15 seconds");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(!self.0.join(".mneme/project.db").exists());
        assert!(!self.0.join("data/mneme").exists());
        output
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn shell(peer: &Peer, fixture: &Fixture, input: &str) -> Output {
    fixture.run(
        &[
            "--remote",
            &peer.url,
            "--remote-db",
            "project",
            "--json",
            "repl",
            ROOT,
        ],
        input,
    )
}
#[test]
fn read_only_completions_abort_without_receipts_or_reflection() {
    for input in ["", "done\n", "abort\n", "q\n"] {
        let (peer, fixture) = (Peer::start(Mode::Normal), Fixture::new());
        let output = shell(&peer, &fixture, input);
        success(&output);
        let calls = peer.tools();
        assert_eq!(calls.len(), 2);
        assert!(
            peer.calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call["method"] == "DELETE")
        );
        assert_eq!(calls[0]["arguments"]["db"], "project");
        assert_eq!(
            calls[1]["arguments"],
            json!({"action":"abort","session":"session-token"})
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("\"reflected\":null"));
    }
}
#[test]
fn explicit_feedback_uses_exact_walk_receipt_and_owner_database() {
    for user in [false, true] {
        let (peer, fixture) = (Peer::start(Mode::Normal), Fixture::new());
        fixture.enroll(&peer, user);
        let argv = if user {
            vec!["--user", "--json", "repl", ROOT]
        } else {
            vec!["--json", "repl", ROOT]
        };
        let output = fixture.run(&argv, &format!("go 0\nbody\nlook\ndone {NEXT} {NEXT}\n"));
        success(&output);
        let calls = peer.tools();
        let scoped: Vec<_> = calls
            .iter()
            .filter(|call| call["name"] != "databases")
            .collect();
        assert_eq!(scoped.len(), 6);
        let database = if user { "user" } else { "project" };
        assert_eq!(scoped[0]["arguments"]["db"], database);
        for call in &scoped {
            assert_eq!(call["arguments"]["expected_db_id"], DB_ID);
        }
        for call in &scoped[1..5] {
            assert!(call["arguments"].get("db").is_none());
            assert_eq!(call["arguments"]["session"], "session-token");
        }
        assert_eq!(scoped[5]["name"], "reflect");
        assert_eq!(scoped[5]["arguments"]["db"], database);
        assert_eq!(scoped[5]["arguments"]["receipts"], json!(["receipt-token"]));
        assert_eq!(scoped[5]["arguments"]["used"], json!([NEXT]));
        assert!(scoped[5]["arguments"].get("unhelpful").is_none());
    }
}
#[test]
fn invalid_or_unvisited_feedback_never_mints_or_spends_receipts() {
    for input in ["done invalid\n".to_string(), format!("done {NEXT}\n")] {
        let (peer, fixture) = (Peer::start(Mode::Normal), Fixture::new());
        let output = shell(&peer, &fixture, &input);
        assert!(!output.status.success());
        let calls = peer.tools();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1]["arguments"]["action"], "abort");
    }
}
#[test]
fn capabilities_identity_and_response_refusals_never_fall_back() {
    for mode in [
        Mode::HiddenWalk,
        Mode::HiddenReflect,
        Mode::WrongDb,
        Mode::NoGuard,
        Mode::BadReceipt,
        Mode::BadStart,
    ] {
        let (peer, fixture) = (Peer::start(mode), Fixture::new());
        fixture.enroll(&peer, false);
        let output = fixture.run(&["--json", "repl", ROOT], &format!("done {ROOT}\n"));
        assert!(!output.status.success(), "mode unexpectedly succeeded");
        let calls = peer.tools();
        assert!(calls.iter().all(|call| call["name"] != "reflect"));
        if matches!(mode, Mode::HiddenReflect | Mode::NoGuard) {
            assert!(
                calls
                    .iter()
                    .all(|call| call["arguments"]["action"] != "done")
            );
        }
        if matches!(mode, Mode::WrongDb | Mode::HiddenWalk) {
            assert!(calls.iter().all(|call| call["name"] != "walk"));
        }
    }
}
#[test]
fn tool_errors_remain_visible_and_reflect_is_not_retried() {
    let (peer, fixture) = (Peer::start(Mode::GoError), Fixture::new());
    let output = shell(&peer, &fixture, "go 0\ndone\n");
    success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("no such neighbor"));
    let (peer, fixture) = (Peer::start(Mode::ReflectError), Fixture::new());
    let output = shell(&peer, &fixture, &format!("done {ROOT}\n"));
    assert!(!output.status.success());
    assert_eq!(
        peer.tools()
            .iter()
            .filter(|call| call["name"] == "reflect")
            .count(),
        1
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("do not blindly retry"));
}
#[test]
fn invalid_start_budget_and_query_refuse_before_connection() {
    let (peer, fixture) = (Peer::start(Mode::Normal), Fixture::new());
    for args in [
        vec!["repl", "invalid"],
        vec!["repl", ROOT, "--budget", "0"],
        vec!["repl", ROOT, "--budget", "65"],
        vec!["repl", ROOT, "--query", " "],
    ] {
        let mut argv = vec!["--remote", peer.url.as_str()];
        argv.extend(args);
        assert!(!fixture.run(&argv, "").status.success());
    }
    assert!(peer.calls.lock().unwrap().is_empty());
}
#[test]
fn oversized_stdin_aborts_without_training() {
    let (peer, fixture) = (Peer::start(Mode::Normal), Fixture::new());
    let output = shell(&peer, &fixture, &format!("{}\n", "x".repeat(8193)));
    assert!(!output.status.success());
    assert_eq!(peer.tools().last().unwrap()["arguments"]["action"], "abort");
}

#[cfg(unix)]
#[test]
fn interrupt_while_waiting_for_stdin_aborts_and_closes_promptly() {
    let (peer, fixture) = (Peer::start(Mode::Normal), Fixture::new());
    let mut child = Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .current_dir(&fixture.0)
        .env_remove("MNEME_DB")
        .args([
            "--remote",
            &peer.url,
            "--remote-db",
            "project",
            "--json",
            "repl",
            ROOT,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !peer
        .tools()
        .iter()
        .any(|call| call["arguments"]["action"] == "start")
    {
        assert!(Instant::now() < deadline, "walk did not start");
        thread::sleep(Duration::from_millis(10));
    }
    // Wait for the start response to be observed before interrupting the idle
    // prompt. The stdin pipe deliberately stays open and supplies no newline.
    thread::sleep(Duration::from_millis(100));
    // SAFETY: this is the child process spawned and still held by this test.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("interrupt waited for stdin");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let calls = peer.tools();
    assert!(
        calls
            .iter()
            .any(|call| call["arguments"]["action"] == "abort")
    );
    assert!(calls.iter().all(|call| call["name"] != "reflect"));
    assert!(
        peer.calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call["method"] == "DELETE")
    );
}

#[test]
fn human_and_json_browse_report_bounded_body_without_learning() {
    for json_mode in [false, true] {
        let (peer, fixture) = (Peer::start(Mode::Normal), Fixture::new());
        let mut args = vec!["--remote", peer.url.as_str(), "--remote-db", "project"];
        if json_mode {
            args.push("--json");
        }
        args.extend(["repl", ROOT]);
        let output = fixture.run(&args, "body\nedges\nback\ndone\n");
        success(&output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("bounded body"));
        assert!(stdout.contains(if json_mode {
            "\"has_more\":true"
        } else {
            "body truncated"
        }));
        assert!(peer.tools().iter().all(|call| call["name"] != "reflect"));
    }
}

#[test]
fn terminal_rpc_and_abort_errors_are_not_automatically_retried() {
    for mode in [Mode::ReflectRpcError, Mode::AbortError] {
        let (peer, fixture) = (Peer::start(mode), Fixture::new());
        let input = if mode == Mode::ReflectRpcError {
            format!("done {ROOT}\n")
        } else {
            "abort\n".into()
        };
        let output = shell(&peer, &fixture, &input);
        assert!(!output.status.success());
        let calls = peer.tools();
        let target = if mode == Mode::ReflectRpcError {
            "reflect"
        } else {
            "walk"
        };
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["name"] == target
                    && (target == "reflect" || call["arguments"]["action"] == "abort"))
                .count(),
            1
        );
    }
}
