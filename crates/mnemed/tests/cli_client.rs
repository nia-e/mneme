use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn run(frames: &[Value], endpoint: &str) -> (Vec<Value>, String) {
    run_with_timeout(frames, endpoint, "100")
}

fn run_with_timeout(frames: &[Value], endpoint: &str, timeout: &str) -> (Vec<Value>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .args([
            "client",
            "--endpoint",
            endpoint,
            "--timeout-ms",
            timeout,
            "--expected-server",
            "mneme-mcp",
            "--local-only",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        for frame in frames {
            writeln!(input, "{}", frame).unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = String::from_utf8(output.stdout).unwrap();
    (
        lines
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn protocol_fixture(
    catalog: Value,
    mut call: impl FnMut(&Value, &Value) -> Option<Value> + Send + 'static,
) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    use std::{
        io::Read,
        net::TcpListener,
        time::{Duration, Instant},
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "http://127.0.0.1:{}/mcp",
        listener.local_addr().unwrap().port()
    );
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut calls = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "episode fixture was not closed");
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let headers_end = loop {
                let mut buf = [0; 4096];
                let count = stream.read(&mut buf).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buf[..count]);
                if let Some(index) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = std::str::from_utf8(&bytes[..headers_end]).unwrap();
            let close = headers.starts_with("DELETE ");
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            while bytes.len() - headers_end < length {
                let mut buf = [0; 4096];
                let count = stream.read(&mut buf).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buf[..count]);
            }
            let request = if length == 0 {
                Value::Null
            } else {
                serde_json::from_slice::<Value>(&bytes[headers_end..headers_end + length]).unwrap()
            };
            let method = request["method"].as_str().unwrap_or("close");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion":"2025-11-25", "serverInfo":{"name":"mneme-mcp"},
                    "_mneme_client":{"expected_db_id":999},
                }),
                "tools/list" => catalog.clone(),
                "tools/call" => {
                    let arguments = &request["params"]["arguments"];
                    calls.push(arguments.clone());
                    let Some(value) = call(&request["params"]["name"], arguments) else {
                        drop(stream);
                        continue;
                    };
                    value
                }
                _ => Value::Null,
            };
            let response = if close || method == "notifications/initialized" {
                "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
            } else {
                let body = json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: episode-native\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            stream.write_all(response.as_bytes()).unwrap();
            if close {
                break;
            }
        }
        calls
    });
    (endpoint, server)
}

#[test]
fn prepare_is_pure_and_precedes_any_connection() {
    let capture = json!({"source":{"namespace":"codex","key":"claim-1","reference":"codex://thread/1"},"summary":"A recorded claim"});
    let (responses, stderr) = run(
        &[
            json!({"id":1,"op":"capture/prepare","payload":capture}),
            json!({"id":2,"op":"close"}),
        ],
        "http://127.0.0.1:1/mcp",
    );
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["ok"], true);
    assert_eq!(responses[0]["result"]["payload"], capture);
    assert_eq!(responses[0]["result"]["digest"].as_str().unwrap().len(), 64);
    assert!(stderr.is_empty());
}

#[test]
fn episode_prepare_is_pure_for_reads_and_writes() {
    let append = json!({"action":"append","source":{"namespace":"codex","key":"episode-1","reference":"codex://thread/1"},"summary":"A recorded scene"});
    let read = json!({"action":"list","limit":3});
    let (responses, stderr) = run(
        &[
            json!({"id":1,"op":"episode/prepare","payload":append}),
            json!({"id":2,"op":"episode/prepare","payload":read}),
            json!({"id":3,"op":"episode/prepare","payload":{"action":"append","source":null}}),
            json!({"id":4,"op":"close"}),
        ],
        "http://127.0.0.1:1/mcp",
    );
    assert_eq!(responses[0]["ok"], true);
    assert_eq!(responses[0]["result"]["action"], "append");
    assert_eq!(responses[0]["result"]["is_mutation"], true);
    assert_eq!(responses[0]["result"]["payload"], append);
    assert_eq!(responses[1]["ok"], true);
    assert_eq!(responses[1]["result"]["action"], "list");
    assert_eq!(responses[1]["result"]["is_mutation"], false);
    assert_eq!(responses[1]["result"]["payload"], read);
    assert_eq!(responses[2]["error"]["kind"], "input");
    assert!(stderr.is_empty());
}

#[test]
fn episode_prepare_bounds_combined_metadata_before_connect() {
    let mut payload = json!({
        "action":"append",
        "source":{"namespace":"codex","key":"bounded-metadata","reference":"codex://thread/1"},
        "summary":"A compact scene",
    });
    let tags = (0..32)
        .map(|index| format!("{index:02}{}", "\"".repeat(254)))
        .collect::<Vec<_>>();
    payload["tags"] = json!(&tags[..24]);
    let mut oversized = payload.clone();
    oversized["tags"] = json!(tags);
    let (responses, stderr) = run(
        &[
            json!({"id":1,"op":"episode/prepare","payload":payload}),
            json!({"id":2,"op":"episode/prepare","payload":oversized}),
            json!({"id":3,"op":"episode/verified","db":"project","payload":oversized}),
            json!({"id":4,"op":"close"}),
        ],
        "http://127.0.0.1:1/mcp",
    );
    assert_eq!(responses[0]["ok"], true, "{}", responses[0]);
    for result in &responses[1..3] {
        assert_eq!(result["error"]["kind"], "input");
        assert!(
            !result["error"]["message"]
                .as_str()
                .unwrap()
                .contains("connect")
        );
    }
    assert!(stderr.is_empty());
}

#[derive(Clone, Copy, Debug)]
enum VerifiedFault {
    None,
    WrongDatabase,
    WrongBody,
    LostMutationReply,
    LostReadbackReply,
    MissingReadAction,
    MalformedReceipt,
    OversizedReceipt,
    MissingAction,
    MissingGuard,
    MissingWriteGuard,
    MissingReadGuard,
    RotationBeforeWrite,
    RotationBeforeRead,
    WrongReceiptDatabase,
    MissingReadbackIdentity,
    Replayed,
}

fn guard_schema() -> Value {
    json!({"type":"string", "minLength":26, "maxLength":26,
        "pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"})
}

fn tool_refusal() -> Value {
    json!({"isError":true,"content":[{"type":"text","text":"expected_db_id mismatch; no operation performed"}]})
}

fn capture_fixture(fault: VerifiedFault) -> (String, Value, std::thread::JoinHandle<Vec<Value>>) {
    let payload = json!({"source":{"namespace":"codex","key":"guarded-claim","reference":"codex://thread/native"},
        "summary":"A sourced claim", "body":"The original body"});
    let prepared = mneme_app::capture::PreparedCapture::parse(&payload).unwrap();
    let source = prepared.expected_source().unwrap();
    let id = source.node_id().0.to_string();
    let digest = source
        .request_digest()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let database = ulid::Ulid::from(73_u128).to_string();
    let receipt = json!({"id":id,"db":"project","db_id":database,
        "replayed":matches!(fault, VerifiedFault::Replayed)});
    let readback = json!({"id":id,"db":"project","db_id":database,
        "summary":payload["summary"],"summary_truncated":false,
        "provenance":{"type":"external","source":{"namespace":"codex","key":"guarded-claim",
            "reference":"codex://thread/native","session":null,"revision":null,"request_digest_sha256":digest,"request_codec":"capture_v2"}},
        "body":payload["body"], "body_range":{"source_start":0,"source_end":17,"next_offset":null,"has_more":false}});
    let mut capture = json!({"name":"capture","inputSchema":{"properties":{}}});
    let mut get = json!({"name":"get","inputSchema":{"properties":{}}});
    if !matches!(
        fault,
        VerifiedFault::MissingGuard | VerifiedFault::MissingWriteGuard
    ) {
        capture["inputSchema"]["properties"]["expected_db_id"] = guard_schema();
    }
    if !matches!(
        fault,
        VerifiedFault::MissingGuard | VerifiedFault::MissingReadGuard
    ) {
        get["inputSchema"]["properties"]["expected_db_id"] = guard_schema();
    }
    let (endpoint, server) =
        protocol_fixture(json!({"tools":[capture,get]}), move |name, arguments| {
            assert_eq!(arguments["db"], "project");
            if let Some(guard) = arguments.get("expected_db_id") {
                assert_eq!(guard, &database);
            }
            let is_read = name == "get";
            if matches!(
                (fault, is_read),
                (VerifiedFault::RotationBeforeWrite, false)
                    | (VerifiedFault::RotationBeforeRead, true)
            ) {
                return Some(tool_refusal());
            }
            let mut value = if is_read {
                assert_eq!(arguments["id"], id);
                readback.clone()
            } else {
                assert_eq!(name, "capture");
                if matches!(fault, VerifiedFault::LostMutationReply) {
                    return None;
                }
                receipt.clone()
            };
            if matches!(
                (fault, is_read),
                (VerifiedFault::WrongDatabase, true) | (VerifiedFault::WrongReceiptDatabase, false)
            ) {
                value["db_id"] = json!(ulid::Ulid::from(99_u128).to_string());
            }
            if is_read && matches!(fault, VerifiedFault::MissingReadbackIdentity) {
                value.as_object_mut().unwrap().remove("db_id");
            }
            Some(json!({"content":[{"type":"text","text":value.to_string()}]}))
        });
    (endpoint, payload, server)
}

fn episode_fixture(
    body: &str,
    revise: bool,
    fault: VerifiedFault,
) -> (String, Value, std::thread::JoinHandle<Vec<Value>>) {
    use mneme_core::{
        NodeId,
        episode::{EpisodeId, EpisodeRevision, EpisodeRevisionReason, OccurrenceSpan},
    };
    use mneme_engine::episode::EpisodeWrite;
    let root = NodeId(ulid::Ulid::from(47_u128));
    let key = if revise {
        "episode-native-revision"
    } else {
        "episode-native-initial"
    };
    let mut payload = json!({
        "action":if revise {"revise"} else {"append"},
        "source":{"namespace":"codex","key":key,"reference":"codex://thread/native"},
        "summary":"An authored scene", "body":body,
    });
    let write = EpisodeWrite::new(
        "codex",
        key,
        "codex://thread/native",
        None,
        None,
        "An authored scene",
        body.as_bytes(),
        &[],
        OccurrenceSpan::Unknown,
        None,
    );
    let source = if revise {
        payload["episode_id"] = json!(root);
        payload["expected_edition_id"] = json!(root);
        payload["reason"] = json!("Corrected the account");
        write
            .validated_revision_source(
                EpisodeId::new(root),
                root,
                EpisodeRevision::new(1),
                &EpisodeRevisionReason::new("Corrected the account").unwrap(),
            )
            .unwrap()
    } else {
        write.validated_source().unwrap()
    };
    let edition = source.node_id();
    let episode = if revise { root } else { edition };
    let database = ulid::Ulid::from(73_u128).to_string();
    let receipt = json!({
        "action":payload["action"],"episode_id":episode,"edition_id":edition,
        "revision":if revise {1} else {0},"replayed":matches!(fault, VerifiedFault::Replayed),
        "db":"project","db_id":database,
    });
    let digest = source
        .request_digest()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let readback = json!({
        "action":"get","episode_id":episode,"edition_id":edition,"revision":receipt["revision"],
        "db":"project","db_id":database,"summary":"An authored scene","occurred":{"kind":"unknown"},
        "recorded_at":1,"edition_recorded_at":if revise {2} else {1},"thread":null,
        "current_edition_id":edition,"is_current":true,"tags":[],
        "revises":if revise {json!(root)} else {Value::Null},
        "edit_reason":if revise {json!("Corrected the account")} else {Value::Null},
        "source":{"namespace":"codex","key":key,"reference":"codex://thread/native","session":null,"revision":null,"request_digest_sha256":digest,"request_codec":"episode_v1"},
    });
    let body = body.to_owned();
    let actions = if matches!(fault, VerifiedFault::MissingAction) {
        &mneme_app::episode::EpisodeAction::READ_ONLY[..]
    } else {
        &mneme_app::episode::EpisodeAction::ALL[..]
    };
    let mut schema = mneme_app::episode::input_schema(actions);
    if !matches!(fault, VerifiedFault::MissingGuard) {
        schema["properties"]["expected_db_id"] = guard_schema();
        for branch in schema["oneOf"].as_array_mut().unwrap() {
            let is_read = branch["properties"]["action"]["const"] == "get";
            if !matches!(
                (fault, is_read),
                (VerifiedFault::MissingReadGuard, true) | (VerifiedFault::MissingWriteGuard, false)
            ) {
                branch["properties"]["expected_db_id"] = guard_schema();
            }
        }
    }
    // An unrelated standalone get guard must not substitute for episode get.
    let catalog = json!({"tools":[{"name":"episode","inputSchema":schema},
        {"name":"get","inputSchema":{"properties":{"expected_db_id":guard_schema()}}}]});
    let (endpoint, server) = protocol_fixture(catalog, move |name, arguments| {
        assert_eq!(name, "episode");
        assert_eq!(arguments["db"], "project");
        if let Some(guard) = arguments.get("expected_db_id") {
            assert_eq!(guard, &database);
        }
        let is_read = arguments["action"] == "get";
        if matches!(
            (fault, is_read),
            (VerifiedFault::RotationBeforeWrite, false) | (VerifiedFault::RotationBeforeRead, true)
        ) {
            return Some(tool_refusal());
        }
        let value = if arguments["action"] == "get" {
            assert_eq!(arguments["edition_id"], receipt["edition_id"]);
            assert_eq!(arguments["episode_id"], receipt["episode_id"]);
            let start = arguments["offset"].as_u64().unwrap() as usize;
            let mut end = (start + 7000).min(body.len());
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            let mut value = readback.clone();
            value["body"] = json!(&body[start..end]);
            value["body_range"] = json!({"source_start":start,"source_end":end,"next_offset":if end<body.len() {json!(end)} else {Value::Null},"has_more":end<body.len()});
            if matches!(fault, VerifiedFault::WrongDatabase) {
                value["db_id"] = json!(ulid::Ulid::from(99_u128).to_string());
            }
            if matches!(fault, VerifiedFault::WrongBody) {
                value["summary"] = json!("A different scene");
            }
            if matches!(fault, VerifiedFault::MissingReadbackIdentity) {
                value.as_object_mut().unwrap().remove("db_id");
            }
            value
        } else {
            if matches!(fault, VerifiedFault::LostMutationReply) {
                return None;
            }
            let mut value = receipt.clone();
            if matches!(fault, VerifiedFault::WrongReceiptDatabase) {
                value["db_id"] = json!(ulid::Ulid::from(99_u128).to_string());
            }
            value
        };
        Some(json!({"content":[{"type":"text","text":value.to_string()}]}))
    });
    (endpoint, payload, server)
}

#[test]
fn episode_verified_uses_one_write_then_same_exact_edition_pages() {
    let body = "\u{0001}\u{0002}\\\"雪".repeat(2000);
    for revise in [false, true] {
        let (endpoint, payload, server) = episode_fixture(&body, revise, VerifiedFault::None);
        let (responses, stderr) = run_with_timeout(
            &[
                json!({"id":1,"op":"connect"}),
                json!({"id":2,"op":"episode/verified","db":"project","payload":payload}),
                json!({"id":3,"op":"close"}),
            ],
            &endpoint,
            "1000",
        );
        let calls = server.join().unwrap();
        assert!(stderr.is_empty());
        assert_eq!(responses[1]["ok"], true, "{}", responses[1]);
        assert_eq!(responses[1]["result"]["readback_status"], "verified");
        assert!(responses[1]["result"].get("body").is_none());
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls.iter().filter(|call| call["action"] != "get").count(),
            1
        );
        assert_eq!(calls[1]["offset"], 0);
        assert_eq!(calls[2]["offset"], 7000);
    }
}

#[test]
fn episode_verified_never_retries_uncertain_or_unverified_mutations() {
    for fault in [
        VerifiedFault::WrongDatabase,
        VerifiedFault::WrongBody,
        VerifiedFault::LostMutationReply,
        VerifiedFault::MissingAction,
    ] {
        let (endpoint, payload, server) = episode_fixture("A short scene", false, fault);
        let (responses, _) = run_with_timeout(
            &[
                json!({"id":1,"op":"connect"}),
                json!({"id":2,"op":"episode/verified","db":"project","payload":payload}),
                json!({"id":3,"op":"close"}),
            ],
            &endpoint,
            "1000",
        );
        let calls = server.join().unwrap();
        assert_eq!(responses[1]["ok"], false);
        assert_eq!(
            calls.iter().filter(|call| call["action"] != "get").count(),
            if matches!(fault, VerifiedFault::MissingAction) {
                0
            } else {
                1
            }
        );
        if matches!(
            fault,
            VerifiedFault::WrongDatabase | VerifiedFault::WrongBody
        ) {
            assert_eq!(responses[1]["error"]["accepted"]["retryable"], false);
            assert!(responses[1]["error"]["accepted"]["edition_id"].is_string());
        }
    }
}

#[test]
fn guarded_verified_writes_require_write_and_exact_read_action_support() {
    for episode in [false, true] {
        for fault in [
            VerifiedFault::MissingGuard,
            VerifiedFault::MissingWriteGuard,
            VerifiedFault::MissingReadGuard,
        ] {
            let (endpoint, payload, server) = if episode {
                episode_fixture("A short scene", false, fault)
            } else {
                capture_fixture(fault)
            };
            let operation = if episode {
                "episode/verified"
            } else {
                "capture/verified"
            };
            let (responses, _) = run_with_timeout(
                &[
                    json!({"id":1,"op":"connect"}),
                    json!({"id":2,"op":operation,"db":"project","expected_db_id":ulid::Ulid::from(73_u128).to_string(),"payload":payload}),
                    json!({"id":3,"op":"close"}),
                ],
                &endpoint,
                "1000",
            );
            assert_eq!(
                responses[0]["_mneme_client"]["expected_db_id"], 1,
                "bridge capability must be on the local envelope"
            );
            assert_eq!(
                responses[0]["result"]["_mneme_client"]["expected_db_id"], 999,
                "remote metadata is separate and cannot prove bridge support"
            );
            assert_eq!(responses[1]["error"]["kind"], "protocol");
            assert!(responses[1]["error"]["accepted"].is_null());
            assert!(
                server.join().unwrap().is_empty(),
                "unsupported guarded write was sent"
            );
        }
    }
}

#[test]
fn guarded_verified_capture_and_episode_keep_original_identity_through_readback() {
    let original = ulid::Ulid::from(73_u128).to_string();
    for episode in [false, true] {
        for fault in [
            VerifiedFault::None,
            VerifiedFault::Replayed,
            VerifiedFault::RotationBeforeWrite,
            VerifiedFault::RotationBeforeRead,
            VerifiedFault::WrongDatabase,
            VerifiedFault::WrongReceiptDatabase,
            VerifiedFault::MissingReadbackIdentity,
            VerifiedFault::LostMutationReply,
        ] {
            let (endpoint, payload, server) = if episode {
                episode_fixture("A short scene", false, fault)
            } else {
                capture_fixture(fault)
            };
            let operation = if episode {
                "episode/verified"
            } else {
                "capture/verified"
            };
            let (responses, _) = run_with_timeout(
                &[
                    json!({"id":1,"op":"connect"}),
                    json!({"id":2,"op":operation,"db":"project","expected_db_id":original,"payload":payload}),
                    json!({"id":3,"op":"close"}),
                ],
                &endpoint,
                "1000",
            );
            let calls = server.join().unwrap();
            let mutation_count = calls
                .iter()
                .filter(|call| {
                    if episode {
                        call["action"] != "get"
                    } else {
                        call.get("source").is_some()
                    }
                })
                .count();
            assert_eq!(mutation_count, 1, "guarded calls never retry/fallback");
            assert!(calls.iter().all(|call| call["expected_db_id"] == original));
            let success = matches!(fault, VerifiedFault::None | VerifiedFault::Replayed);
            assert_eq!(responses[1]["ok"], success, "{}", responses[1]);
            if success {
                assert_eq!(responses[1]["result"]["db_id"], original);
                assert_eq!(responses[1]["result"]["readback_status"], "verified");
                assert_eq!(
                    responses[1]["result"]["replayed"],
                    matches!(fault, VerifiedFault::Replayed)
                );
            } else if matches!(
                fault,
                VerifiedFault::RotationBeforeWrite | VerifiedFault::LostMutationReply
            ) {
                assert!(responses[1]["error"]["accepted"].is_null());
                assert_eq!(calls.len(), 1);
            } else {
                let accepted = &responses[1]["error"]["accepted"];
                assert_eq!(accepted["db"], "project");
                assert_eq!(accepted["retryable"], false);
                assert_eq!(
                    accepted["db_id"],
                    if matches!(fault, VerifiedFault::WrongReceiptDatabase) {
                        ulid::Ulid::from(99_u128).to_string()
                    } else {
                        original.clone()
                    }
                );
                assert_ne!(accepted["readback_status"], "verified");
            }
        }
    }
}

#[test]
fn capture_verified_remains_unguarded_on_older_server_when_not_requested() {
    let (endpoint, payload, server) = capture_fixture(VerifiedFault::MissingGuard);
    let (responses, _) = run_with_timeout(
        &[
            json!({"id":1,"op":"connect"}),
            json!({"id":2,"op":"capture/verified","db":"project","payload":payload}),
            json!({"id":3,"op":"close"}),
        ],
        &endpoint,
        "1000",
    );
    assert_eq!(responses[1]["ok"], true, "{}", responses[1]);
    assert!(
        server
            .join()
            .unwrap()
            .iter()
            .all(|call| call.get("expected_db_id").is_none())
    );
}

#[test]
fn malformed_routing_guard_is_rejected_before_connection_or_domain_payload_changes() {
    let capture = json!({"source":{"namespace":"codex","key":"guard-check","reference":"codex://thread/guard"},"summary":"A claim"});
    let mut episode = capture.clone();
    episode["action"] = json!("append");
    for (operation, payload) in [("capture/verified", capture), ("episode/verified", episode)] {
        for guard in [
            Value::Null,
            json!(true),
            json!(2),
            json!("bad"),
            json!("0".repeat(27)),
            json!("0000000000000000000000002a"),
        ] {
            let (responses, _) = run(
                &[
                    json!({"id":1,"op":operation,"db":"project","expected_db_id":guard,"payload":payload}),
                    json!({"id":2,"op":"close"}),
                ],
                "http://127.0.0.1:1/mcp",
            );
            assert_eq!(responses[0]["error"]["kind"], "input");
            assert!(
                responses[0]["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("expected_db_id")
            );
        }
        let mut misplaced = payload.clone();
        misplaced["expected_db_id"] = json!(ulid::Ulid::from(73_u128).to_string());
        let (responses, _) = run(
            &[
                json!({"id":1,"op":operation,"db":"project","payload":misplaced}),
                json!({"id":2,"op":"close"}),
            ],
            "http://127.0.0.1:1/mcp",
        );
        assert_eq!(responses[0]["error"]["kind"], "input");
    }
}

#[test]
fn refuses_requests_before_connect_and_invalid_frames() {
    let (responses, _) = run(
        &[
            json!({"id":1,"op":"tools/list"}),
            json!({"id":2,"op":"capture/prepare","payload":{"source":null}}),
            json!({"id":3,"op":"close"}),
        ],
        "http://127.0.0.1:1/mcp",
    );
    assert_eq!(responses[0]["error"]["kind"], "input");
    assert_eq!(responses[1]["error"]["kind"], "input");
    assert_eq!(responses[2]["ok"], true);
}

#[test]
fn connect_refusal_is_bounded_and_nonretrying() {
    let (responses, _) = run(
        &[json!({"id":1,"op":"connect"}), json!({"id":2,"op":"close"})],
        "http://127.0.0.1:1/mcp",
    );
    assert_eq!(responses[0]["error"]["kind"], "transport");
    assert_eq!(responses[1]["ok"], true);
}

#[test]
fn malformed_and_oversize_frames_do_not_trigger_network() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .args([
            "client",
            "--endpoint",
            "http://127.0.0.1:1/mcp",
            "--timeout-ms",
            "100",
            "--expected-server",
            "mneme-mcp",
            "--local-only",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        writeln!(input, "not json").unwrap();
        input.write_all(&vec![b'x'; 128 * 1024 + 1]).unwrap();
        input.write_all(b"\n").unwrap();
        writeln!(input, "{}", json!({"id":3,"op":"close"})).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let lines: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["error"]["kind"], "input");
    assert_eq!(lines[1]["error"]["kind"], "input");
    assert_eq!(lines[2]["ok"], true);
}

#[test]
fn save_prepare_freezes_manual_and_provided_identity_without_connection() {
    for kind in ["note", "episode"] {
        for explicit in [false, true] {
            let mut payload = json!({"kind":kind,"summary":"A remembered claim"});
            if explicit {
                payload["operation_id"] = json!("fixed-operation");
            }
            let (responses, stderr) = run(
                &[
                    json!({"id":"rpc-not-an-operation","op":"save/prepare","payload":payload}),
                    json!({"id":2,"op":"close"}),
                ],
                "http://127.0.0.1:1/mcp",
            );
            assert!(stderr.is_empty());
            let result = &responses[0]["result"];
            assert_eq!(result["schema"], 1);
            assert_eq!(result.as_object().unwrap().len(), 3);
            let frozen = &result["payload"];
            assert_eq!(frozen["kind"], kind);
            assert_eq!(frozen["source"]["namespace"], "manual");
            assert!(frozen.get("operation_id").is_none());
            assert!(frozen.get("action").is_none());
            assert_ne!(frozen["source"]["key"], "rpc-not-an-operation");
            if explicit {
                assert_eq!(frozen["source"]["key"], "fixed-operation");
            } else {
                assert!(ulid::Ulid::from_string(frozen["source"]["key"].as_str().unwrap()).is_ok());
            }
            let (retry, _) = run(
                &[
                    json!({"id":3,"op":"save/prepare","payload":frozen}),
                    json!({"id":4,"op":"close"}),
                ],
                "http://127.0.0.1:1/mcp",
            );
            assert_eq!(retry[0]["result"], *result);
        }
        let payload = json!({"kind":kind,"summary":"A sourced claim", "source":{
            "namespace":"codex","key":"event-item","reference":"codex://thread","session":"session","revision":"v1"}});
        let (responses, _) = run(
            &[
                json!({"id":1,"op":"save/prepare","payload":payload}),
                json!({"id":2,"op":"close"}),
            ],
            "http://127.0.0.1:1/mcp",
        );
        assert_eq!(
            responses[0]["result"]["payload"]["source"],
            payload["source"]
        );
    }
}

fn source_readback_json(source: &mneme_core::CaptureSource) -> Value {
    let digest = source
        .request_digest()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    json!({"namespace":source.namespace(),"key":source.key(),"reference":source.reference(),
        "session":source.session(),"revision":source.revision(),
        "request_digest_sha256":digest,"request_codec":source.request_codec()})
}

/// Disposable HTTP owner: derives native proofs from the exact frozen write,
/// never from a separately generated identity or a client RPC request ID.
fn save_fixture(
    kind: &str,
    manual: bool,
    fault: VerifiedFault,
    body: &str,
) -> (String, Value, std::thread::JoinHandle<Vec<Value>>) {
    let mut payload = json!({"kind":kind, "summary":"An authored memory", "body":body});
    if manual {
        payload["operation_id"] = json!("native-manual-op");
    } else {
        payload["source"] = json!({"namespace":"codex", "key":"event-item",
            "reference":"codex://thread/native", "session":"session", "revision":"v1"});
    }
    let database = ulid::Ulid::from(73_u128).to_string();
    let mut schema = mneme_app::save::input_schema();
    schema["properties"]["db"] = json!({"type":"string"});
    schema["required"].as_array_mut().unwrap().push(json!("db"));
    let mut get = json!({"name":"get","inputSchema":{"properties":{}}});
    let mut episode =
        mneme_app::episode::input_schema(&mneme_app::episode::EpisodeAction::READ_ONLY);
    if !matches!(
        fault,
        VerifiedFault::MissingGuard | VerifiedFault::MissingWriteGuard
    ) {
        schema["properties"]["expected_db_id"] = guard_schema();
    }
    if !matches!(
        fault,
        VerifiedFault::MissingGuard | VerifiedFault::MissingReadGuard
    ) {
        get["inputSchema"]["properties"]["expected_db_id"] = guard_schema();
        episode["properties"]["expected_db_id"] = guard_schema();
        for branch in episode["oneOf"].as_array_mut().unwrap() {
            branch["properties"]["expected_db_id"] = guard_schema();
        }
    }
    let mut catalog = json!({"tools":[{"name":"save","inputSchema":schema},get,
        {"name":"episode","inputSchema":episode},
        {"name":"capture","inputSchema":{"properties":{}}}]});
    if matches!(fault, VerifiedFault::MissingAction) {
        catalog["tools"].as_array_mut().unwrap().remove(0);
    }
    if matches!(fault, VerifiedFault::MissingReadAction) {
        catalog["tools"]
            .as_array_mut()
            .unwrap()
            .retain(|tool| tool["name"] != "get");
        catalog["tools"][1]["inputSchema"]["properties"]["action"]["enum"] = json!(["list"]);
    }
    let mut write = Value::Null;
    let mut writes_seen = 0;
    let (endpoint, server) = protocol_fixture(catalog, move |name, args| {
        assert_eq!(args["db"], "project");
        if let Some(guard) = args.get("expected_db_id") {
            assert_eq!(guard, &database);
        }
        let is_write = name == "save";
        if is_write {
            writes_seen += 1;
            write = args.clone();
            write.as_object_mut().unwrap().remove("db");
            write.as_object_mut().unwrap().remove("expected_db_id");
            assert!(write.get("operation_id").is_none());
            assert!(write.get("action").is_none());
            if matches!(fault, VerifiedFault::LostMutationReply) {
                return None;
            }
        } else {
            assert!(name == "get" || name == "episode");
            assert!(write.is_object());
            if matches!(fault, VerifiedFault::LostReadbackReply) {
                return None;
            }
        }
        if matches!(
            (fault, is_write),
            (VerifiedFault::RotationBeforeWrite, true) | (VerifiedFault::RotationBeforeRead, false)
        ) {
            return Some(tool_refusal());
        }
        let prepared = mneme_app::save::PreparedSave::parse(&write, "").unwrap();
        let id = prepared.expected_id().unwrap();
        let mut receipt = json!({"kind":write["kind"], "id":id, "replayed":matches!(fault,VerifiedFault::Replayed) || writes_seen > 1,
            "origin":prepared.identity().origin.as_str(), "db":"project", "db_id":database});
        if prepared.identity().origin == mneme_app::save::SaveOrigin::ManualSubmission {
            receipt["operation_id"] = json!(prepared.identity().key);
        }
        let is_episode = write["kind"] == "episode";
        if is_episode {
            receipt["episode_id"] = json!(id);
            receipt["edition_id"] = json!(id);
            receipt["revision"] = json!(0);
        }
        let mut value = if is_write {
            if matches!(fault, VerifiedFault::MalformedReceipt) {
                receipt.as_object_mut().unwrap().remove("origin");
            }
            if matches!(fault, VerifiedFault::OversizedReceipt) {
                receipt["body"] = json!("x".repeat(40 * 1024));
            }
            receipt
        } else {
            assert_eq!(
                args[if is_episode { "edition_id" } else { "id" }],
                json!(id)
            );
            let mut node = match prepared.into_request() {
                mneme_app::save::SaveRequest::Note(request) => {
                    let source = request.expected_source().unwrap();
                    json!({"id":id, "summary":write["summary"],"summary_truncated":false,
                        "provenance":{"type":"external","source":source_readback_json(&source)}})
                }
                mneme_app::save::SaveRequest::Episode(_) => {
                    let source = &write["source"];
                    let proof = mneme_engine::episode::EpisodeWrite::new(
                        source["namespace"].as_str().unwrap(),
                        source["key"].as_str().unwrap(),
                        source["reference"].as_str().unwrap(),
                        source["session"].as_str(),
                        source["revision"].as_str(),
                        write["summary"].as_str().unwrap(),
                        write["body"].as_str().unwrap().as_bytes(),
                        &[],
                        mneme_core::episode::OccurrenceSpan::Unknown,
                        None,
                    )
                    .validated_source()
                    .unwrap();
                    json!({"action":"get", "episode_id":id, "edition_id":id, "revision":0,
                        "summary":write["summary"], "occurred":{"kind":"unknown"}, "thread":null,
                        "recorded_at":1, "edition_recorded_at":1, "current_edition_id":id,"is_current":true,
                        "tags":[], "revises":null,"edit_reason":null,"source":source_readback_json(&proof)})
                }
            };
            let body = write["body"].as_str().unwrap();
            let start = if is_episode {
                args["offset"].as_u64().unwrap() as usize
            } else {
                0
            };
            let mut end = if is_episode {
                (start + 7000).min(body.len())
            } else {
                body.len()
            };
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            node["body"] = json!(&body[start..end]);
            node["body_range"] = json!({"source_start":start,"source_end":end,
                "next_offset":if end<body.len() {json!(end)} else {Value::Null},"has_more":end<body.len()});
            node["db"] = json!("project");
            node["db_id"] = json!(database);
            node
        };
        if matches!(
            (fault, is_write),
            (VerifiedFault::WrongDatabase, false) | (VerifiedFault::WrongReceiptDatabase, true)
        ) {
            value["db_id"] = json!(ulid::Ulid::from(99_u128).to_string());
        }
        if !is_write && matches!(fault, VerifiedFault::MissingReadbackIdentity) {
            value.as_object_mut().unwrap().remove("db_id");
        }
        if !is_write && matches!(fault, VerifiedFault::WrongBody) {
            value["summary"] = json!("Changed the memory");
        }
        Some(json!({"content":[{"type":"text","text":value.to_string()}]}))
    });
    (endpoint, payload, server)
}

#[test]
fn save_verified_preserves_kind_origin_and_one_frozen_write_with_exact_readback() {
    for kind in ["note", "episode"] {
        for manual in [false, true] {
            for fault in [VerifiedFault::None, VerifiedFault::Replayed] {
                let body = if kind == "episode" {
                    "雪\\\"".repeat(3000)
                } else {
                    "A claim body".into()
                };
                let (endpoint, payload, server) = save_fixture(kind, manual, fault, &body);
                let (responses, stderr) = run_with_timeout(
                    &[
                        json!({"id":1,"op":"connect"}),
                        json!({"id":2,"op":"save/verified","db":"project","payload":payload}),
                        json!({"id":3,"op":"close"}),
                    ],
                    &endpoint,
                    "1000",
                );
                let calls = server.join().unwrap();
                assert!(stderr.is_empty());
                assert_eq!(responses[0]["_mneme_client"]["save"], 1);
                assert_eq!(responses[1]["ok"], true, "{}", responses[1]);
                let result = &responses[1]["result"];
                assert_eq!(result["kind"], kind);
                assert_eq!(
                    result["origin"],
                    if manual {
                        "manual_submission"
                    } else {
                        "provided_source"
                    }
                );
                assert_eq!(result["readback_status"], "verified");
                assert_eq!(result["replayed"], matches!(fault, VerifiedFault::Replayed));
                assert!(result.get("body").is_none());
                assert!(result.get("action").is_none());
                if manual {
                    assert_eq!(result["operation_id"], "native-manual-op");
                } else {
                    assert!(result.get("operation_id").is_none());
                }
                assert_eq!(
                    calls
                        .iter()
                        .filter(|args| args.get("kind").is_some())
                        .count(),
                    1
                );
                assert_eq!(
                    calls[0]["source"]["key"],
                    if manual {
                        "native-manual-op"
                    } else {
                        "event-item"
                    }
                );
                if kind == "episode" {
                    assert_eq!(result["id"], result["edition_id"]);
                    assert_eq!(calls.len(), 4);
                } else {
                    assert_eq!(calls.len(), 2);
                }
            }
        }
    }
}

#[test]
fn save_verified_refuses_unsupported_guard_and_retains_acceptance_without_retry() {
    for kind in ["note", "episode"] {
        for fault in [
            VerifiedFault::MissingAction,
            VerifiedFault::MissingWriteGuard,
            VerifiedFault::MissingReadGuard,
            VerifiedFault::RotationBeforeWrite,
            VerifiedFault::RotationBeforeRead,
            VerifiedFault::WrongReceiptDatabase,
            VerifiedFault::WrongDatabase,
            VerifiedFault::WrongBody,
            VerifiedFault::MissingReadbackIdentity,
            VerifiedFault::LostMutationReply,
            VerifiedFault::LostReadbackReply,
            VerifiedFault::MissingReadAction,
            VerifiedFault::MalformedReceipt,
            VerifiedFault::OversizedReceipt,
        ] {
            let (endpoint, payload, server) = save_fixture(kind, true, fault, "Original body");
            let (responses, _) = run_with_timeout(
                &[
                    json!({"id":1,"op":"connect"}),
                    json!({"id":2,"op":"save/verified","db":"project",
                    "expected_db_id":ulid::Ulid::from(73_u128).to_string(), "payload":payload}),
                    json!({"id":3,"op":"close"}),
                ],
                &endpoint,
                "1000",
            );
            let calls = server.join().unwrap();
            assert_eq!(responses[1]["ok"], false, "{kind}: {fault:?}");
            let preflight = matches!(
                fault,
                VerifiedFault::MissingAction
                    | VerifiedFault::MissingReadAction
                    | VerifiedFault::MissingWriteGuard
                    | VerifiedFault::MissingReadGuard
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|args| args.get("kind").is_some())
                    .count(),
                usize::from(!preflight)
            );
            let accepted = &responses[1]["error"]["accepted"];
            if preflight
                || matches!(
                    fault,
                    VerifiedFault::RotationBeforeWrite | VerifiedFault::LostMutationReply
                )
            {
                assert!(accepted.is_null());
            } else {
                assert_eq!(accepted["kind"], kind);
                assert_eq!(accepted["operation_id"], "native-manual-op");
                assert_eq!(accepted["retryable"], false);
                assert!(accepted["id"].is_string());
                assert!(accepted.get("body").is_none());
            }
        }
    }
}

#[test]
fn save_verified_generated_identity_is_once_frozen_not_the_rpc_identity() {
    let (endpoint, mut payload, server) = save_fixture("note", true, VerifiedFault::None, "A body");
    payload.as_object_mut().unwrap().remove("operation_id");
    let (responses, _) = run_with_timeout(
        &[
            json!({"id":1,"op":"connect"}),
            json!({"id":"not-a-manual-op","op":"save/verified","db":"project","payload":payload}),
            json!({"id":3,"op":"close"}),
        ],
        &endpoint,
        "1000",
    );
    let calls = server.join().unwrap();
    assert_eq!(responses[1]["ok"], true, "{}", responses[1]);
    let operation = responses[1]["result"]["operation_id"].as_str().unwrap();
    assert_ne!(operation, "not-a-manual-op");
    assert!(ulid::Ulid::from_string(operation).is_ok());
    assert_eq!(calls[0]["source"]["key"], operation);
    assert_eq!(calls[1]["id"], responses[1]["result"]["id"]);
}

#[test]
fn save_verified_explicit_canonical_retry_resubmits_the_same_frozen_payload() {
    for kind in ["note", "episode"] {
        let (endpoint, payload, server) = save_fixture(kind, true, VerifiedFault::None, "A body");
        let frozen = mneme_app::save::PreparedSave::parse(&payload, "")
            .unwrap()
            .into_json();
        let (responses, _) = run_with_timeout(
            &[
                json!({"id":1,"op":"connect"}),
                json!({"id":2,"op":"save/verified","db":"project","payload":frozen}),
                json!({"id":3,"op":"save/verified","db":"project","payload":frozen}),
                json!({"id":4,"op":"close"}),
            ],
            &endpoint,
            "1000",
        );
        let calls = server.join().unwrap();
        assert_eq!(responses[1]["ok"], true);
        assert_eq!(responses[2]["ok"], true);
        assert_eq!(responses[1]["result"]["replayed"], false);
        assert_eq!(responses[2]["result"]["replayed"], true);
        assert_eq!(responses[1]["result"]["id"], responses[2]["result"]["id"]);
        let writes = calls
            .iter()
            .filter(|args| args.get("kind").is_some())
            .collect::<Vec<_>>();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0], writes[1]);
    }
}

#[test]
fn save_lost_write_reply_retains_generated_attempt_not_acceptance() {
    for kind in ["note", "episode"] {
        let (endpoint, mut payload, server) =
            save_fixture(kind, true, VerifiedFault::LostMutationReply, "A body");
        payload.as_object_mut().unwrap().remove("operation_id");
        let (responses, _) = run_with_timeout(
            &[
                json!({"id":1,"op":"connect"}),
                json!({"id":"not-an-operation","op":"save/verified","db":"project","payload":payload}),
                json!({"id":3,"op":"close"}),
            ],
            &endpoint,
            "1000",
        );
        let calls = server.join().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "lost response must not cause retry or readback"
        );
        let error = &responses[1]["error"];
        assert_eq!(responses[1]["ok"], false);
        assert!(error.get("accepted").is_none());
        let attempted = &error["attempted"];
        assert_eq!(attempted["write_status"], "unacknowledged");
        assert_eq!(attempted["readback_status"], "not_attempted");
        assert_eq!(attempted["retryable"], false);
        assert_eq!(attempted["source"], calls[0]["source"]);
        assert_eq!(attempted["operation_id"], calls[0]["source"]["key"]);
        assert_ne!(attempted["operation_id"], "not-an-operation");
        assert!(attempted.get("body").is_none());
        let mut frozen = calls[0].clone();
        frozen.as_object_mut().unwrap().remove("db");
        let prepared = mneme_app::save::PreparedSave::parse(&frozen, "").unwrap();
        assert_eq!(attempted["id"], json!(prepared.expected_id().unwrap()));
    }
}

#[test]
fn save_bridge_complete_admission_precedes_connection_or_any_work() {
    for payload in [
        json!({"summary":"A claim","source":null}),
        json!({"summary":"A claim","kind":"not-save"}),
        json!({"summary":"A claim","operation_id":null}),
        json!({"summary":"A claim","operation_id":"op","source":{}}),
        json!({"summary":"A claim","action":"revise"}),
        json!({"summary":"A claim","kind":"episode","core":true}),
        json!({"summary":"A claim","expected_db_id":"bad"}),
        json!({"summary":"A claim","kind":"episode","body":"x".repeat(17*1024)}),
    ] {
        let (responses, _) = run(
            &[
                json!({"id":1,"op":"save/prepare","payload":payload}),
                json!({"id":2,"op":"save/verified","db":"project","payload":payload}),
                json!({"id":3,"op":"close"}),
            ],
            "http://127.0.0.1:1/mcp",
        );
        assert_eq!(responses[0]["error"]["kind"], "input", "{}", responses[0]);
        assert_eq!(responses[1]["error"]["kind"], "input");
    }
    for db in [
        Value::Null,
        json!(""),
        json!("../global"),
        json!("x".repeat(129)),
    ] {
        let (responses, _) = run(
            &[
                json!({"id":1,"op":"save/verified","db":db,"payload":{"summary":"A claim"}}),
                json!({"id":2,"op":"close"}),
            ],
            "http://127.0.0.1:1/mcp",
        );
        assert_eq!(responses[0]["error"]["kind"], "input");
    }
}

fn concern_catalog(actions: &[&str]) -> Value {
    let mut schema = mneme_app::concern::input_schema(actions);
    schema["properties"]["db"] = json!({"type":"string"});
    schema["properties"]["expected_db_id"] = guard_schema();
    schema["allOf"] = json!([{"if":{"properties":{"action":{"enum":["notice","record_finding"]}},"required":["action"]},"then":{"required":["db","expected_db_id"]}}]);
    json!({"tools":[{"name":"concern","inputSchema":schema}]})
}
fn concern_notice() -> Value {
    use mneme_core::{
        ConcernBinding, ConcernDigest, ConcernEndpoint, ConcernKind, ConcernNotice, ConcernUpdate,
        NodeId,
    };
    let binding = ConcernBinding::new(
        ConcernKind::Disagreement,
        ConcernEndpoint::new(NodeId(ulid::Ulid::from(1)), ConcernDigest::of_bytes(b"a")),
        ConcernEndpoint::new(NodeId(ulid::Ulid::from(2)), ConcernDigest::of_bytes(b"b")),
    )
    .unwrap();
    serde_json::to_value(ConcernUpdate::Notice(
        ConcernNotice::new(binding, "Claims differ", "Which context applies?").unwrap(),
    ))
    .unwrap()
}
#[test]
fn concern_bridge_admits_before_call_and_returns_only_one_atomic_result() {
    let payload = concern_notice();
    let id = ulid::Ulid::from(29).to_string();
    let ack = json!({"db":"project","db_id":id,"action":"notice","outcome":{"status":"applied","row":{"notice":payload["notice"],"finding":null}}});
    let expected = ack.clone();
    let (endpoint, server) = protocol_fixture(
        concern_catalog(&["list", "notice", "record_finding"]),
        move |name, _| {
            assert_eq!(name, "concern");
            Some(json!({"structuredContent":ack,"content":[]}))
        },
    );
    let (responses, _) = run_with_timeout(
        &[
            json!({"id":1,"op":"connect"}),
            json!({"id":2,"op":"concern/checked","db":"project","payload":payload}),
            json!({"id":3,"op":"concern/checked","db":"project","expected_db_id":id,"payload":{"action":"list","endpoint":"bad"}}),
            json!({"id":4,"op":"concern/checked","db":"project","expected_db_id":id,"payload":payload}),
            json!({"id":5,"op":"close"}),
        ],
        &endpoint,
        "1000",
    );
    assert_eq!(responses[0]["_mneme_client"]["concern"], 1);
    assert_eq!(responses[1]["error"]["kind"], "input");
    assert_eq!(responses[2]["error"]["kind"], "input");
    assert_eq!(responses[3]["result"], expected);
    let calls = server.join().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["action"], "notice");
    assert_eq!(calls[0]["expected_db_id"], id);
}
#[test]
fn concern_bridge_old_profile_and_wrong_atomic_results_never_retry_or_readback() {
    let payload = concern_notice();
    let id = ulid::Ulid::from(29).to_string();
    for actions in [
        &[][..],
        &["list"][..],
        &["list", "notice", "record_finding"][..],
    ] {
        let (endpoint, server) = protocol_fixture(concern_catalog(actions), |name, args| {
            assert_eq!(name, "concern");
            Some(
                json!({"structuredContent":{"db":"other","db_id":args["expected_db_id"],"action":"notice","outcome":{"status":"refused","reason":"missing_endpoint","row":null}},"content":[]}),
            )
        });
        let (responses, _) = run_with_timeout(
            &[
                json!({"id":1,"op":"connect"}),
                json!({"id":2,"op":"concern/checked","db":"project","expected_db_id":id,"payload":payload}),
                json!({"id":3,"op":"close"}),
            ],
            &endpoint,
            "1000",
        );
        assert_eq!(responses[1]["ok"], false);
        let calls = server.join().unwrap();
        assert_eq!(calls.len(), usize::from(actions.contains(&"notice")));
        if !calls.is_empty() {
            assert_eq!(responses[1]["error"]["attempted"]["retryable"], false);
        }
    }
    // Refused CAS is an exact successful domain response, not a repair signal.
    let expected = json!({"db":"project","db_id":id,"action":"notice","outcome":{"status":"refused","reason":"missing_endpoint","row":null}});
    let ack = expected.clone();
    let (endpoint, server) = protocol_fixture(concern_catalog(&["notice"]), move |_, _| {
        Some(json!({"structuredContent":ack,"content":[]}))
    });
    let (responses, _) = run_with_timeout(
        &[
            json!({"id":1,"op":"connect"}),
            json!({"id":2,"op":"concern/checked","db":"project","expected_db_id":id,"payload":payload}),
            json!({"id":3,"op":"close"}),
        ],
        &endpoint,
        "1000",
    );
    assert_eq!(responses[1]["result"], expected);
    assert_eq!(server.join().unwrap().len(), 1);
}
#[test]
fn concern_bridge_maximum_escaped_native_page_fits_actual_mcp_and_ndjson_envelopes() {
    use mneme_core::*;
    let escaped = |max: usize| format!("x{}", "\u{1}".repeat(max - 1));
    let finding = ScopedConcernFinding::new(
        escaped(MAX_CONCERN_SCOPE_BYTES),
        escaped(MAX_CONCERN_FINDING_BYTES),
        [256, 256, 256, 48]
            .into_iter()
            .enumerate()
            .map(|(i, n)| {
                ConcernEvidence::new(escaped(n), ConcernDigest::of_bytes(&[i as u8])).unwrap()
            })
            .collect(),
    )
    .unwrap();
    let endpoint = NodeId(ulid::Ulid::from(1));
    let mut rows = Vec::new();
    for i in 0..mneme_app::concern::MAX_CONCERN_PUBLIC_PAGE_ROWS {
        let binding = ConcernBinding::new(
            ConcernKind::Disagreement,
            ConcernEndpoint::new(endpoint, ConcernDigest::of_bytes(b"a")),
            ConcernEndpoint::new(
                NodeId(ulid::Ulid::from(i as u128 + 2)),
                ConcernDigest::of_bytes(b"b"),
            ),
        )
        .unwrap();
        let row = ConcernRow::from_notice(
            ConcernNotice::new(
                binding,
                escaped(MAX_CONCERN_BYTES),
                escaped(MAX_CONCERN_MISSING_FACT_BYTES),
            )
            .unwrap(),
        );
        let mut raw = serde_json::to_value(row).unwrap();
        raw["finding"] = serde_json::to_value(&finding).unwrap();
        rows.push(serde_json::from_value::<ConcernRow>(raw).unwrap());
    }
    let last = rows.last().unwrap().binding().key();
    let cursor =
        ConcernPageCursor::new(endpoint, last.other(endpoint).unwrap(), last.kind()).unwrap();
    let expected = json!({"db":"project","db_id":ulid::Ulid::from(29).to_string(),"action":"list","page":{"items":rows,"next":cursor}});
    let native = expected.clone();
    let (url, server) = protocol_fixture(concern_catalog(&["list"]), move |name, _| {
        assert_eq!(name, "concern");
        let result = json!({"structuredContent":native,"content":[{"type":"text","text":native.to_string()}]});
        assert!(
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"result":result}))
                .unwrap()
                .len()
                <= 512 * 1024
        );
        Some(result)
    });
    let (responses, _) = run_with_timeout(
        &[
            json!({"id":1,"op":"connect"}),
            json!({"id":2,"op":"concern/checked","db":"project","payload":{"action":"list","endpoint":endpoint}}),
            json!({"id":3,"op":"close"}),
        ],
        &url,
        "1000",
    );
    assert_eq!(responses[1]["result"], expected);
    assert!(serde_json::to_vec(&responses[1]).unwrap().len() + 1 <= 512 * 1024);
    let calls = server.join().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["limit"],
        mneme_app::concern::MAX_CONCERN_PUBLIC_PAGE_ROWS
    );
}

#[test]
fn verified_touchstone_save_rejects_missing_typed_readback_despite_matching_source_digest() {
    let database = ulid::Ulid::from(7_u128).to_string();
    let payload = json!({"summary":"Why this mattered","body":"The annotation body","operation_id":"typed-readback",
        "touchstone":{"subject":"A historical scene","references":[{"db_id":database,
            "id":ulid::Ulid::from(8_u128).to_string(),"expected_snapshot_sha256":"a".repeat(64)}]}});
    let frozen = mneme_app::save::PreparedSave::parse(&payload, "")
        .unwrap()
        .into_json();
    let prepared = mneme_app::save::PreparedSave::parse(&frozen, "").unwrap();
    let id = prepared.expected_id().unwrap();
    let mneme_app::save::SaveRequest::Note(note) = prepared.into_request() else {
        panic!("note")
    };
    let mut schema = mneme_app::save::input_schema();
    schema["properties"]["db"] = json!({"type":"string"});
    schema["required"].as_array_mut().unwrap().push(json!("db"));
    let catalog = json!({"tools":[{"name":"save","inputSchema":schema},{"name":"get","inputSchema":{"type":"object","properties":{}}}]});
    let source = note.expected_source().unwrap();
    let (endpoint, worker) = protocol_fixture(catalog, move |name, _args| {
        let result = if name == "save" {
            json!({"kind":"note","id":id,"db":"project","db_id":database,
            "origin":"manual_submission","operation_id":"typed-readback","replayed":false})
        } else {
            json!({"id":id,"db":"project","db_id":database,"summary":frozen["summary"],"summary_truncated":false,"body":frozen["body"],
            "body_range":{"source_start":0,"source_end":19,"next_offset":null,"has_more":false},
            "provenance":{"type":"external","source":source_readback_json(&source)}})
        };
        Some(json!({"content":[{"type":"text","text":result.to_string()}],"isError":false}))
    });
    let (responses, _) = run_with_timeout(
        &[
            json!({"id":1,"op":"connect"}),
            json!({"id":2,"op":"save/verified","db":"project","payload":payload}),
            json!({"id":3,"op":"close"}),
        ],
        &endpoint,
        "1000",
    );
    assert_eq!(responses[1]["ok"], false);
    assert!(
        responses[1]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("TouchstoneRecord"),
        "{}",
        responses[1]
    );
    assert_eq!(
        responses[1]["error"]["accepted"]["readback_status"],
        "mismatch"
    );
    assert_eq!(responses[1]["error"]["accepted"]["id"], json!(id));
    assert_eq!(worker.join().unwrap().len(), 2);
}
