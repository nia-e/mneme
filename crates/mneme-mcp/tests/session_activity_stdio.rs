//! Finite process-boundary checks. Fixtures are private temporary files; no
//! owner, discovery cache, or provider is needed or permitted by this startup.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "mneme-session-activity-stdio-{}",
            ulid::Ulid::new()
        ));
        std::fs::create_dir(&path).unwrap();
        for name in ["home", "config", "workspace"] {
            std::fs::create_dir(path.join(name)).unwrap();
        }
        // Both ordinary config parsing and home-owner admission would leave
        // evidence or fail startup if this exclusive route regressed.
        std::fs::write(
            path.join("config/memory.toml"),
            "not a memory configuration",
        )
        .unwrap();
        Self(path)
    }

    fn inventory(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn collect(root: &Path, output: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
            for entry in std::fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    output.insert(path.clone(), None);
                    collect(&path, output);
                } else {
                    output.insert(path.clone(), Some(std::fs::read(path).unwrap()));
                }
            }
        }
        let mut output = BTreeMap::new();
        collect(&self.0, &mut output);
        output
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mneme-mcp"));
        command
            .current_dir(self.0.join("workspace"))
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("PATH", self.0.join("no-subprocesses"))
            .arg("--activity-config")
            .arg(self.0.join("missing-activity.json"))
            .arg("--cwd")
            .arg(self.0.join("workspace"));
        command
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn finite_run(mut command: Command, input: Vec<u8>) -> (bool, Vec<u8>, Vec<u8>) {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input_thread = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let mut stdout = child.stdout.take().unwrap();
    let output_thread = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let mut stderr = child.stderr.take().unwrap();
    let error_thread = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("activity MCP did not shut down within 15 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    input_thread.join().unwrap();
    (
        status.success(),
        output_thread.join().unwrap(),
        error_thread.join().unwrap(),
    )
}

fn lines(requests: &[Value]) -> Vec<u8> {
    let mut output = Vec::new();
    for request in requests {
        serde_json::to_writer(&mut output, request).unwrap();
        output.push(b'\n');
    }
    output
}

#[test]
fn session_activity_stdio_starts_without_owners_and_preserves_fixture() {
    let scratch = Scratch::new();
    let before = scratch.inventory();
    let (success, output, errors) = finite_run(
        scratch.command(),
        lines(&[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"session_activity","arguments":{}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"session_activity","arguments":{"scope":"global","path":"private-unadmitted-value"}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"query","arguments":{}}}),
            json!({"jsonrpc":"2.0","id":6,"method":"ping"}),
        ]),
    );
    assert!(success, "{}", String::from_utf8_lossy(&errors));
    let responses: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 6);
    assert_eq!(
        responses[0]["result"]["capabilityProfile"],
        "local-session-activity"
    );
    let catalog = responses[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0]["name"], "session_activity");
    let text = responses[2]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.len() <= 48 * 1024);
    let report: Value = serde_json::from_str(text).unwrap();
    assert_eq!(report["status"], "not_configured");
    for response in &responses[3..5] {
        assert_eq!(response["result"]["isError"], true);
        assert!(!response.to_string().contains("private-unadmitted-value"));
    }
    assert_eq!(responses[5]["result"], json!({}));
    assert_eq!(scratch.inventory(), before);
}

#[test]
fn session_activity_stdio_drains_oversized_frame_and_recovers() {
    let scratch = Scratch::new();
    let before = scratch.inventory();
    let mut input = vec![b'x'; 2 * 1024 * 1024 + 1];
    input.push(b'\n');
    input.extend(lines(&[json!({"jsonrpc":"2.0","id":1,"method":"ping"})]));
    let (success, output, errors) = finite_run(scratch.command(), input);
    assert!(success, "{}", String::from_utf8_lossy(&errors));
    let responses: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["error"]["code"], -32600);
    assert_eq!(responses[1]["id"], 1);
    assert_eq!(responses[1]["result"], json!({}));
    assert_eq!(scratch.inventory(), before);
}

#[test]
fn session_activity_startup_refuses_all_owner_and_http_selectors_without_writes() {
    let scratch = Scratch::new();
    let before = scratch.inventory();
    for arguments in [
        vec!["--db", "user=/must/not/open"],
        vec!["--discovery-catalog", "user=/must/not/read"],
        vec!["--memory-config", "/must/not/discover"],
        vec!["--router-config", "/must/not/contact"],
        vec!["--library-config", "/must/not/read"],
        vec!["--library", "any"],
        vec!["--capability-profile", "read-only"],
        vec!["--allow-direct-feedback"],
        vec!["--http", "invalid-bind-address"],
        vec!["--http-token-env", "MUST_NOT_READ"],
        vec!["--cors-origin", "invalid-origin"],
    ] {
        let mut command = scratch.command();
        command.args(&arguments);
        let (success, output, errors) = finite_run(command, Vec::new());
        assert!(!success, "accepted {arguments:?}");
        assert!(output.is_empty());
        let error = String::from_utf8(errors).unwrap();
        assert!(
            error.contains("activity")
                && (error.contains("cannot combine") || error.contains("stdio-only")),
            "{error}"
        );
        assert_eq!(scratch.inventory(), before);
    }
}
