//! Shared protocol fixtures and read-only reader regression coverage.
use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;

const A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const B: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const C: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";

fn catalog(id: &str) -> Value {
    json!([{"db":"project","db_id":id,"resolved_path":"/fixture.db","state":"open","in_flight":2,"backend_jobs":3}])
}
fn center() -> Value {
    json!({"id":A,"summary":"a memory","body":"details","status":"active","body_range":{"has_more":false}})
}
fn edge(id: &str, incoming: bool) -> Value {
    json!({"neighbor":id,"summary":"other memory","kind":"derived_from","incoming":incoming,"weight":0.7})
}
fn query(hits: Vec<Value>) -> Value {
    json!({"schema":"mneme.query.v3","partial":false,"lanes":{"primary":{"seed_coverage":null,"hits":hits}}})
}

fn hits(ids: &[&str]) -> Vec<Value> {
    ids.iter()
        .enumerate()
        .map(|(rank, id)| json!({"id":id,"summary":"seed","status":"active","lane_rank":rank+1}))
        .collect()
}

fn event(seq: u64, database: &str, ids: Vec<Value>) -> Value {
    json!({"seq":seq,"timestamp_ms":42,"tool":"get","db":"project","db_id":database,"node_ids":ids,"node_ids_truncated":false,"db_truncated":false})
}

fn feed_page(latest: u64, events: Vec<Value>) -> Value {
    json!({"schema":"mneme.activity.v1","instance":B,"events":events,"next_after":latest,"oldest_seq":1,"latest_seq":latest,"missed":false,"dropped":0,"has_more":false,"busy":false})
}

#[test]
fn identity_and_state_must_match_before_reading() {
    assert!(registry(&catalog(A), "project", Some(B)).is_err());
    assert!(registry(&catalog(A), "other", None).is_err());
    let (id, activity) = registry(&catalog(A), "project", Some(A)).unwrap();
    assert_eq!(id, A);
    assert_eq!((activity.in_flight, activity.backend_jobs), (2, 3));
    let mut closed = catalog(A);
    closed[0]["state"] = json!("maintenance");
    assert!(registry(&closed, "project", Some(A)).is_err());
    assert!(registry(&json!([catalog(A)[0], catalog(A)[0]]), "project", None).is_err());
}

#[test]
fn tag_coverage_preserves_unknown_missing_and_truncated_metadata() {
    let mut value = center();
    assert!(!node(&value).unwrap().tags_complete);
    value["tags"] = json!([]);
    assert!(node(&value).unwrap().tags_complete); // Authored empty is known.
    value["tags"] = json!(["rust"]);
    assert!(node(&value).unwrap().tags_complete);
    value["tags_truncated"] = json!(true);
    assert!(!node(&value).unwrap().tags_complete);
    value["tags_truncated"] = json!(false);
    for tags in [
        json!(["rust", 42]),
        json!([format!("{}a", "x".repeat(128))]),
        json!(["\u{1b}rust"]),
        json!((0..17).map(|i| format!("tag-{i}")).collect::<Vec<_>>()),
    ] {
        value["tags"] = tags;
        assert!(!node(&value).unwrap().tags_complete);
    }
}

#[test]
fn focus_preserves_real_edge_direction_and_kind() {
    let graph = focus_graph(
        A,
        &center(),
        &json!({"items":[edge(B,true),edge(B,false)],"has_more":false}),
    )
    .unwrap();
    assert_eq!(graph.nodes.len(), 2);
    assert_eq!(graph.edges.len(), 2);
    assert_eq!(
        (graph.edges[0].from.as_str(), graph.edges[0].to.as_str()),
        (B, A)
    );
    assert_eq!(
        (graph.edges[1].from.as_str(), graph.edges[1].to.as_str()),
        (A, B)
    );
    assert_eq!(graph.edges[0].kind, "derived_from");
    assert_eq!(graph.edges[0].weight, 0.7);
    assert!(!graph.partial);
    assert!(focus_graph(B, &center(), &json!({"items":[],"has_more":false})).is_err());
}

#[test]
fn focus_caps_neighbors_and_body_and_sanitizes_terminal_text() {
    let edges = (0..40)
        .map(|i| edge(&format!("{i:026}"), false))
        .collect::<Vec<_>>();
    let mut center = center();
    center["body"] = json!(format!("\u{1b}[31m{}", "é".repeat(BODY_BYTES)));
    center["body_range"]["has_more"] = json!(true);
    let graph = focus_graph(A, &center, &json!({"items":edges,"has_more":true})).unwrap();
    assert_eq!(graph.nodes.len(), 33);
    assert_eq!(graph.edges.len(), 32);
    assert!(graph.partial);
    assert!(graph.note.contains("body excerpt"));
    assert!(graph.note.contains("first 32"));
    assert!(graph.nodes[0].body.len() <= BODY_BYTES);
    assert!(!graph.nodes[0].body.contains('\u{1b}'));
}

#[test]
fn query_has_bounded_nodes_no_invented_edges_and_lane_labels() {
    let hits = (0..20).map(|i| json!({"id":format!("{i:026}"),"summary":"found","status":"active","lane_rank":i+1})).collect();
    let graph = query_graph(&query(hits)).unwrap();
    assert_eq!(graph.nodes.len(), QUERY_NODES);
    assert!(graph.edges.is_empty());
    assert!(graph.partial);
    assert!(graph.nodes[0].provenance.contains("primary lane"));
    assert!(graph.note.contains("stored links within this field"));
}

#[test]
fn old_or_compatibility_lanes_and_nonactive_hits_are_refused() {
    let mut value = query(hits(&[A]));
    value["schema"] = json!("mneme.query.v2");
    assert!(query_graph(&value).is_err());
    let mut value = query(hits(&[A]));
    value["lanes"]["probationary"] = json!({"hits": []});
    assert!(query_graph(&value).is_err());
    let mut value = query(hits(&[A]));
    value["lanes"]["primary"]["hits"][0]["status"] = json!("archived");
    assert!(query_graph(&value).is_err());
}

#[test]
fn input_validation_happens_without_a_connection() {
    assert!(validate_request(&Request::Query(" \n ".into())).is_err());
    assert!(validate_request(&Request::Query("a".repeat(4097))).is_err());
    assert!(validate_request(&Request::Focus("not-an-id".into())).is_err());
    assert!(validate_request(&Request::Focus(A.into())).is_ok());
    assert!(validate_request(&Request::Poll).is_ok());
    for cue in [" ".to_owned(), "x".repeat(4097)] {
        assert!(
            validate_request(&Request::Scenes {
                axis: scenes::SceneAxis::Recorded,
                cue: Some(cue)
            })
            .is_err()
        );
    }
    assert!(
        validate_request(&Request::Scene {
            episode_id: A.into(),
            edition_id: "not-an-id".into()
        })
        .is_err()
    );
    assert!(
        validate_request(&Request::Scene {
            episode_id: A.into(),
            edition_id: B.into()
        })
        .is_ok()
    );
}

#[test]
fn feed_baselines_then_filters_by_database_and_resets_on_restart() {
    let mut observer = Observer::default();
    let mut activity = Activity::default();
    observe(
        &feed_page(10, vec![event(10, A, vec![json!(A)])]),
        A,
        &mut observer,
        &mut activity,
    )
    .unwrap();
    assert!(activity.feed);
    assert_eq!(activity.returns, 0);
    assert_eq!(observer.after, 10);
    let page = feed_page(
        12,
        vec![
            event(11, A, vec![json!(A), json!(B)]),
            event(12, B, vec![json!(B)]),
        ],
    );
    observe(&page, A, &mut observer, &mut activity).unwrap();
    assert_eq!(activity.returns, 1);
    assert_eq!(activity.node_ids, vec![A.to_owned(), B.to_owned()]);
    assert!(!activity.missed);
    let mut restarted = feed_page(1, vec![event(1, A, vec![json!(A)])]);
    restarted["instance"] = json!(A);
    let mut activity = Activity::default();
    observe(&restarted, A, &mut observer, &mut activity).unwrap();
    assert_eq!(activity.returns, 0);
    assert!(activity.node_ids.is_empty());
    assert_eq!(observer.after, 1);
}

#[test]
fn busy_feed_preserves_cursor_and_bounded_feed_marks_omissions() {
    let mut observer = Observer::default();
    observe(
        &feed_page(1, vec![]),
        A,
        &mut observer,
        &mut Activity::default(),
    )
    .unwrap();
    let mut busy = feed_page(2, vec![]);
    busy["busy"] = json!(true);
    observe(&busy, A, &mut observer, &mut Activity::default()).unwrap();
    assert_eq!(observer.after, 1);
    let events = (0..32)
        .map(|i| {
            event(
                i + 2,
                A,
                (0..32)
                    .map(|j| json!(format!("{:026}", i * 32 + j)))
                    .collect(),
            )
        })
        .collect();
    let mut activity = Activity::default();
    observe(&feed_page(33, events), A, &mut observer, &mut activity).unwrap();
    assert_eq!(activity.returns, 32);
    assert_eq!(activity.node_ids.len(), 512);
    assert!(activity.missed);
    let mut dropped = feed_page(34, vec![]);
    dropped["dropped"] = json!(1);
    observe(&dropped, A, &mut observer, &mut activity).unwrap();
    assert!(activity.missed);
}

struct MockOwner {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct EdgeFixture {
    hits: Vec<Value>,
    pages: std::collections::BTreeMap<String, Value>,
    delay: Duration,
    fail_on: Option<String>,
}

impl MockOwner {
    fn start(change_identity_after_error: bool) -> Self {
        Self::start_mode(change_identity_after_error, false)
    }

    fn start_mode(change_identity_after_error: bool, advertise_activity: bool) -> Self {
        Self::start_config(
            change_identity_after_error,
            advertise_activity,
            None,
            false,
            false,
        )
    }

    fn start_edges(edges: EdgeFixture) -> Self {
        Self::start_config(false, false, Some(edges), false, false)
    }

    fn start_scenes() -> Self {
        Self::start_config(false, false, None, true, true)
    }

    fn start_config(
        change_identity_after_error: bool,
        advertise_activity: bool,
        edges: Option<EdgeFixture>,
        advertise_scenes: bool,
        scene_identity_guard: bool,
    ) -> Self {
        Self::start_options(
            change_identity_after_error,
            advertise_activity,
            edges,
            advertise_scenes,
            scene_identity_guard,
            false,
            None,
        )
    }
    fn start_graph(change_identity: bool) -> Self {
        Self::start_options(change_identity, false, None, false, false, true, None)
    }
    fn start_touchstone_catalog(schema: Value) -> Self {
        Self::start_options(false, false, None, true, true, false, Some(schema))
    }
    fn start_options(
        change_identity_after_error: bool,
        advertise_activity: bool,
        edges: Option<EdgeFixture>,
        advertise_scenes: bool,
        scene_identity_guard: bool,
        advertise_graph: bool,
        list_schema: Option<Value>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let (log, stop) = (requests.clone(), stopped.clone());
        let thread = std::thread::spawn(move || {
            let mut registry_calls = 0;
            let mut activity_calls = 0;
            while !stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let (method, input) = read_request(&mut stream);
                if method == "DELETE" {
                    log.lock().unwrap().push(json!({"method":"DELETE"}));
                    reply(&mut stream, 204, None);
                    continue;
                }
                log.lock().unwrap().push(input.clone());
                if input["method"] == "notifications/initialized" {
                    reply(&mut stream, 202, None);
                    continue;
                }
                let result = match input["method"].as_str().unwrap() {
                    "initialize" => {
                        json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
                    }
                    "tools/list" => {
                        let mut names = vec!["databases", "query", "get", "neighbors"];
                        if advertise_graph {
                            names.push("graph");
                        }
                        if advertise_activity {
                            names.push("activity");
                        }
                        if advertise_scenes {
                            names.push("episode");
                            names.push("list");
                        }
                        let guard = json!({"type":"string","minLength":26,"maxLength":26,
                            "pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"});
                        let tools: Vec<_> = names.into_iter().map(|name| match name {
                            "get" if advertise_scenes && !scene_identity_guard => json!({"name":name,"inputSchema":{"properties":{}}}),
                            "get" | "query" | "neighbors" => json!({"name":name,"inputSchema":{"properties":{"expected_db_id":guard}}}),
                            "graph" => json!({"name":name,"inputSchema":{"properties":{"expected_db_id":guard,"action":{"type":"string","enum":["topology","summaries"]}},"oneOf":[{"properties":{"action":{"const":"topology"},"expected_db_id":guard}},{"properties":{"action":{"const":"summaries"},"expected_db_id":guard}}]}}),
                            "list" => json!({"name":name,"inputSchema":list_schema.clone().unwrap_or_else(|| json!({"additionalProperties":false,
                                "required":["db","kind"],"properties":{"expected_db_id":guard,
                                "kind":{"const":"touchstones"},"limit":{"maximum":32},"after":{"maxLength":1024}}}))}),
                            "episode" => {
                                let branches:Vec<_> = ["list","search","get"].into_iter().map(|action| json!({"properties":{
                                    "action":{"type":"string","const":action},"expected_db_id":guard
                                }})).collect();
                                let mut schema = json!({"name":name,"inputSchema":{"properties":{
                                    "action":{"type":"string","enum":["list","search","get"]},"expected_db_id":guard
                                },"oneOf":branches}});
                                if !scene_identity_guard {
                                    schema["inputSchema"]["properties"].as_object_mut().unwrap().remove("expected_db_id");
                                }
                                schema
                            },
                            _ => json!({"name":name}),
                        }).collect();
                        json!({"tools":tools})
                    }
                    "tools/call" => {
                        let tool = input["params"]["name"].as_str().unwrap();
                        let payload = match tool {
                            "databases" => {
                                registry_calls += 1;
                                catalog(if change_identity_after_error && registry_calls > 1 {
                                    B
                                } else {
                                    A
                                })
                            }
                            "graph" => {
                                let args = &input["params"]["arguments"];
                                if args["action"] == "summaries" {
                                    json!({"db_id":A,"action":"summaries","items":args["ids"].as_array().unwrap().iter().map(|id|json!({"id":id,"missing":false,"summary":"visible native card","tags":["viewport"],"status":"active"})).collect::<Vec<_>>()})
                                } else {
                                    let (start, count, next) = match args["after"].as_str() {
                                        None => (0, 64, Some("one")),
                                        Some("one") => (64, 0, Some("two")),
                                        Some("two") => (64, 66, None),
                                        _ => panic!("invalid graph cursor"),
                                    };
                                    json!({"db_id":if change_identity_after_error && args["after"].is_string() {B}else{A},"action":"topology","nodes":(start..start+count).map(|n|json!({"id":format!("{n:026}"),"status":"active","kind":"note"})).collect::<Vec<_>>(),"edges":if next.is_none(){vec![json!({"from":format!("{:026}",0),"to":format!("{:026}",1),"kind":"derived_from","weight":0.7})]}else{vec![]},"has_more":next.is_some(),"next_cursor":next,"coverage":{"snapshot":false,"nodes_done":next.is_none(),"edges_done":next.is_none()}})
                                }
                            }
                            "query" => query(
                                edges
                                    .as_ref()
                                    .map(|e| e.hits.clone())
                                    .unwrap_or_else(|| hits(&[A, B])),
                            ),
                            "get" => {
                                let args = &input["params"]["arguments"];
                                assert!(
                                    args["body"] == true || args.get("max_body_bytes").is_none(),
                                    "body limits require body=true"
                                );
                                let id = args["id"].as_str().unwrap();
                                if id == C {
                                    touchstone_fixture()
                                } else {
                                    let mut value = center();
                                    value["id"] = json!(id);
                                    value["status"] = json!("archived");
                                    value["summary"] = json!("x".repeat(2048));
                                    value["summary_truncated"] = json!(true);
                                    value["memory_kind"] = json!({"kind":"episode","episode":{"episode_id":B,
                                        "revision":0,"revises":null,"occurred":{"kind":"unknown"},
                                        "thread":null,"recorded_at":42,"edit_reason":null}});
                                    value["summary_snapshot"] =
                                        json!({"db_id":A,"id":id,"coverage":"summary_only"});
                                    value
                                }
                            }
                            "list" => json!({"kind":"touchstones","db_id":A,"items":[{
                                "id":C,"summary":"Authored annotation","subject":"A reason to keep this","reference_count":1,"status":"Archived"}],
                                "next_cursor":null,"has_more":false}),
                            "neighbors" => {
                                let id = input["params"]["arguments"]["id"].as_str().unwrap();
                                if let Some(edges) = &edges {
                                    std::thread::sleep(edges.delay);
                                    edges
                                        .pages
                                        .get(id)
                                        .cloned()
                                        .unwrap_or_else(|| json!({"items":[],"has_more":false}))
                                } else {
                                    json!({"items":[if id == A { edge(B,true) } else { edge(A,false) }],"has_more":false,"returned":1})
                                }
                            }
                            "episode" => {
                                let arguments = &input["params"]["arguments"];
                                let header = json!({"episode_id":B,"edition_id":B,"current_edition_id":B,
                                    "revision":0,"summary":"A native scene","occurred":{"kind":"unknown"},
                                    "recorded_at":42,"edition_recorded_at":42,"thread":null});
                                match arguments["action"].as_str().unwrap() {
                                    "list" => {
                                        json!({"db_id":A,"action":"list","items":[header],"next":null,"partial":false})
                                    }
                                    "search" => {
                                        json!({"db_id":A,"action":"search","mode":"lexical","items":[header],"has_more":false,"partial":false})
                                    }
                                    "get" => {
                                        let mut value = header;
                                        value.as_object_mut().unwrap().extend(json!({"db_id":A,"action":"get","current_edition_id":C,"is_current":false,
                                            "body":"The exact original account","body_range":{"has_more":false},
                                            "source":{"namespace":"workshop","key":"one","reference":"session"}}).as_object().unwrap().clone());
                                        value
                                    }
                                    action => panic!("unexpected episode action {action}"),
                                }
                            }
                            "activity" => {
                                activity_calls += 1;
                                feed_page(
                                    activity_calls,
                                    vec![event(activity_calls, A, vec![json!(A)])],
                                )
                            }
                            _ => panic!("viewer called unexpected tool: {tool}"),
                        };
                        if tool == "get"
                            && input["params"]["arguments"]["id"] == "00000000000000000000000000"
                        {
                            json!({"isError":true,"content":[{"type":"text","text":"node not found"}]})
                        } else if (change_identity_after_error && tool == "query")
                            || (tool == "neighbors"
                                && edges.as_ref().and_then(|e| e.fail_on.as_deref())
                                    == input["params"]["arguments"]["id"].as_str())
                        {
                            json!({"isError":true,"content":[{"type":"text","text":"temporary fixture error"}]})
                        } else {
                            json!({"content":[{"type":"text","text":payload.to_string()}]})
                        }
                    }
                    method => panic!("unexpected RPC method {method}"),
                };
                reply(
                    &mut stream,
                    200,
                    Some(json!({"jsonrpc":"2.0","id":input["id"],"result":result})),
                );
            }
        });
        Self {
            url,
            requests,
            stopped,
            thread: Some(thread),
        }
    }

    fn target(&self) -> Target {
        Target {
            name: "fixture".into(),
            database: "project".into(),
            url: self.url.clone(),
            ssh_mcp_port: 18766,
            token_env: None,
            expected_db_id: None,
            expected_path: None,
        }
    }
}

impl Drop for MockOwner {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn read_request(stream: &mut TcpStream) -> (String, Value) {
    let mut raw = Vec::new();
    let mut byte = [0];
    while !raw.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        raw.push(byte[0]);
        assert!(raw.len() <= 8192);
    }
    let header = String::from_utf8(raw).unwrap();
    let method = header.split_whitespace().next().unwrap().to_owned();
    let length = header
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    assert!(length <= 16 * 1024);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    (
        method,
        if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        },
    )
}

fn reply(stream: &mut TcpStream, status: u16, value: Option<Value>) {
    let body = value.map(|value| value.to_string()).unwrap_or_default();
    if let Err(error) = write!(stream,"HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nmcp-session-id: observatory-fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).and_then(|_|stream.flush()) {
        // A deadline test deliberately cancels an in-flight HTTP exchange.
        assert!(matches!(error.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset));
    }
}

async fn receive(responses: &mut Receiver<Response>) -> Response {
    tokio::time::timeout(Duration::from_secs(5), responses.recv())
        .await
        .unwrap()
        .unwrap()
}

fn touchstone_fixture() -> Value {
    json!({"id":C,"summary":"Authored annotation","body":"Deliberate meaning","status":"archived",
        "summary_snapshot":{"db_id":A,"id":C,"coverage":"summary_only"},
        "touchstone":{"owner":C,"subject":"A reason to keep this","references":[{
            "db_id":A,"id":B,"summary":"The old episode edition","provenance":{"kind":"unknown"},
            "created":42,"memory_kind":{"kind":"episode","episode":{"episode_id":B,
                "revision":0,"revises":null,"occurred":{"kind":"unknown"},"thread":null,
                "recorded_at":42,"edit_reason":null}}}]},
        "touchstone_current":{"coverage":"summary_only","references":[{"db_id":A,"id":B,"status":"snapshot_changed"}]},
        "body_range":{"has_more":false}})
}

#[test]
fn touchstone_parser_rejects_wrong_owner_rows_and_bounds_without_losing_snapshots() {
    let fixture = touchstone_fixture();
    for status in ["unchanged", "snapshot_changed", "absent", "unavailable"] {
        let mut value = fixture.clone();
        value["touchstone_current"]["references"][0]["status"] = json!(status);
        assert!(touchstones::parse_detail(&value, A, C).is_ok());
    }
    for pointer in [
        "/touchstone/owner",
        "/summary_snapshot/db_id",
        "/touchstone_current/references/0/id",
        "/touchstone/references/0/db_id",
    ] {
        let mut value = fixture.clone();
        *value.pointer_mut(pointer).unwrap() = json!(C);
        if pointer == "/touchstone/owner" {
            *value.pointer_mut(pointer).unwrap() = json!(B);
        }
        assert!(
            touchstones::parse_detail(&value, A, C).is_err(),
            "{pointer}"
        );
    }
    let mut duplicate = fixture.clone();
    let row = duplicate["touchstone"]["references"][0].clone();
    duplicate["touchstone"]["references"]
        .as_array_mut()
        .unwrap()
        .push(row);
    assert!(touchstones::parse_detail(&duplicate, A, C).is_err());
    let mut large = fixture.clone();
    large["touchstone"]["references"][0]["summary"] = json!("x".repeat(16 * 1024));
    assert_eq!(
        touchstones::parse_detail(&large, A, C).unwrap().references[0]
            .summary
            .len(),
        16 * 1024
    );
    large["touchstone"]["references"][0]["summary"] = json!("x".repeat(16 * 1024 + 1));
    assert!(touchstones::parse_detail(&large, A, C).is_err());
    let page = json!({"kind":"touchstones","db_id":A,"items":[],"next_cursor":"continuation","has_more":true});
    assert!(touchstones::parse_page(&page, A).unwrap().partial);
    assert!(touchstones::parse_page(&page, B).is_err());
    let escaped = json!({"kind":"touchstones","db_id":A,"items":[{"id":C,"summary":"\"".repeat(16*1024),"subject":"Quote test","reference_count":1,"status":"Active"}],"next_cursor":null,"has_more":false});
    assert_eq!(
        touchstones::parse_page(&escaped, A).unwrap().items[0]
            .summary
            .len(),
        16 * 1024
    );
    let mut cropped = fixture.clone();
    cropped["summary"] = json!("x".repeat(2048));
    cropped["summary_truncated"] = json!(true);
    let cropped = touchstones::parse_detail(&cropped, A, C).unwrap();
    assert!(cropped.summary_partial);
    assert!(!cropped.body_partial);
    assert_eq!(cropped.node.summary.len(), 2048);
    let mut full = fixture.clone();
    full["summary"] = json!("x".repeat(16 * 1024));
    assert_eq!(
        touchstones::parse_detail(&full, A, C)
            .unwrap()
            .node
            .summary
            .len(),
        16 * 1024
    );
    let mut bad = page;
    bad["next_cursor"] = json!("x".repeat(1025));
    assert!(touchstones::parse_page(&bad, A).is_err());
}

#[tokio::test]
async fn touchstone_worker_rejects_legacy_or_unguarded_catalog_before_reading() {
    for owner in [
        MockOwner::start(false),
        MockOwner::start_config(false, false, None, true, false),
    ] {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (reply, mut responses) = tokio::sync::mpsc::channel(1);
        let worker = tokio::spawn(serve(owner.target(), rx, reply));
        tx.send(Request::Touchstones { after: None }).await.unwrap();
        assert!(
            matches!(receive(&mut responses).await,Response::Error(message) if message.contains("guarded native"))
        );
        tx.send(Request::Shutdown).await.unwrap();
        worker.await.unwrap();
        assert!(
            owner
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"] == "tools/call")
                .all(|r| r["params"]["name"] == "databases")
        );
    }
}

fn union_touchstone_catalog() -> Value {
    let guard = json!({"type":"string","minLength":26,"maxLength":26,
        "pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"});
    json!({"type":"object","additionalProperties":false,"required":[],
    "properties":{"kind":{"type":"string","enum":["nodes","touchstones"]},
        "expected_db_id":guard},
    "oneOf":[
        {"type":"object","additionalProperties":false,"properties":{
            "kind":{"type":"string","const":"nodes"},"expected_db_id":guard,
            "limit":{"type":"integer","maximum":64},"after":{"type":"string","maxLength":1024}}},
        {"type":"object","additionalProperties":false,"required":["kind"],"properties":{
            "kind":{"type":"string","const":"touchstones"},"expected_db_id":guard,
            "limit":{"type":"integer","maximum":32},"after":{"type":"string","maxLength":1024}}}
    ]})
}

#[tokio::test]
async fn touchstone_worker_accepts_guarded_union_branch_without_root_kind_or_required_db() {
    let owner = MockOwner::start_touchstone_catalog(union_touchstone_catalog());
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let (reply, mut responses) = tokio::sync::mpsc::channel(1);
    let worker = tokio::spawn(serve(owner.target(), rx, reply));
    tx.send(Request::Touchstones { after: None }).await.unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::Touchstones(page) if page.items.len()==1)
    );
    tx.send(Request::Shutdown).await.unwrap();
    worker.await.unwrap();
    let requests = owner.requests.lock().unwrap();
    let list = requests
        .iter()
        .find(|r| r["params"]["name"] == "list")
        .unwrap();
    assert_eq!(
        list["params"]["arguments"],
        json!({"db":"project","expected_db_id":A,"kind":"touchstones","limit":8})
    );
    assert!(
        requests
            .iter()
            .filter(|r| r["method"] == "tools/call")
            .all(|r| matches!(r["params"]["name"].as_str(), Some("databases" | "list")))
    );
}

#[tokio::test]
async fn touchstone_worker_rejects_ordinary_inventory_and_malformed_union_branches() {
    let union = union_touchstone_catalog();
    let mut ordinary_only = union.clone();
    ordinary_only["oneOf"].as_array_mut().unwrap().pop();
    let mut cases = vec![ordinary_only];
    for (pointer, bad) in [
        ("/oneOf/1/additionalProperties", json!(true)),
        ("/oneOf/1/required", json!([])),
        ("/oneOf/1/properties/kind/const", json!("notes")),
        ("/oneOf/1/properties/limit/maximum", json!(7)),
        ("/oneOf/1/properties/after/maxLength", json!(4096)),
    ] {
        let mut schema = union.clone();
        *schema.pointer_mut(pointer).unwrap() = bad;
        cases.push(schema);
    }
    let mut no_guard = union.clone();
    no_guard["properties"]
        .as_object_mut()
        .unwrap()
        .remove("expected_db_id");
    no_guard["oneOf"][1]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("expected_db_id");
    cases.push(no_guard);
    for schema in cases {
        let owner = MockOwner::start_touchstone_catalog(schema);
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (reply, mut responses) = tokio::sync::mpsc::channel(1);
        let worker = tokio::spawn(serve(owner.target(), rx, reply));
        tx.send(Request::Touchstones { after: None }).await.unwrap();
        assert!(
            matches!(receive(&mut responses).await,Response::Error(message) if message.contains("guarded native"))
        );
        tx.send(Request::Shutdown).await.unwrap();
        worker.await.unwrap();
        assert!(
            owner
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"] == "tools/call")
                .all(|r| r["params"]["name"] == "databases")
        );
    }
}

#[tokio::test]
async fn touchstone_worker_is_guarded_bounded_and_keeps_exact_archived_targets() {
    let owner = MockOwner::start_scenes();
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let (reply, mut responses) = tokio::sync::mpsc::channel(1);
    let worker = tokio::spawn(serve(owner.target(), rx, reply));
    tx.send(Request::Touchstones { after: None }).await.unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::Touchstones(page) if page.items[0].status=="Archived")
    );
    tx.send(Request::Touchstone { id: C.into() }).await.unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::Touchstone(detail) if detail.references[0].id==B)
    );
    tx.send(Request::TouchstoneTarget {
        db_id: A.into(),
        id: B.into(),
    })
    .await
    .unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::TouchstoneTarget(target) if target.summary_partial && target.node.as_ref().unwrap().id==B)
    );
    tx.send(Request::TouchstoneTarget {
        db_id: A.into(),
        id: "00000000000000000000000000".into(),
    })
    .await
    .unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::TouchstoneTarget(target) if target.node.is_none())
    );
    tx.send(Request::TouchstoneTarget {
        db_id: B.into(),
        id: B.into(),
    })
    .await
    .unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::Error(message) if message.contains("unavailable"))
    );
    tx.send(Request::Shutdown).await.unwrap();
    worker.await.unwrap();
    let requests = owner.requests.lock().unwrap();
    let reads: Vec<_> = requests
        .iter()
        .filter(|r| r["method"] == "tools/call" && r["params"]["name"] != "databases")
        .collect();
    assert_eq!(reads.len(), 4);
    assert_eq!(
        reads[0]["params"]["arguments"],
        json!({"db":"project","expected_db_id":A,"kind":"touchstones","limit":8})
    );
    assert_eq!(
        reads[2]["params"]["arguments"],
        json!({"db":"project","expected_db_id":A,"id":B,"body":true,"max_body_bytes":8192})
    );
    assert!(
        reads
            .iter()
            .all(|r| matches!(r["params"]["name"].as_str(), Some("get" | "list")))
    );
}

#[tokio::test]
async fn worker_uses_only_bounded_reads_and_poll_is_registry_only() {
    let owner = MockOwner::start(false);
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let (reply, mut responses) = tokio::sync::mpsc::channel(1);
    let worker = tokio::spawn(serve(owner.target(), rx, reply));
    tx.send(Request::Query("memory".into())).await.unwrap();
    assert!(
        matches!(receive(&mut responses).await, Response::Loaded(graph) if graph.nodes.len()==2 && graph.edges.len()==1 && graph.edges[0].from==B && graph.edges[0].to==A && !graph.partial)
    );
    tx.send(Request::Focus(A.into())).await.unwrap();
    assert!(
        matches!(receive(&mut responses).await, Response::Loaded(graph) if graph.nodes.len()==2 && graph.edges.len()==1)
    );
    tx.send(Request::Poll).await.unwrap();
    assert!(
        matches!(receive(&mut responses).await, Response::Activity(activity) if activity.in_flight==2)
    );
    tx.send(Request::Shutdown).await.unwrap();
    worker.await.unwrap();
    let requests = owner.requests.lock().unwrap();
    let calls = requests
        .iter()
        .filter(|r| r["method"] == "tools/call")
        .collect::<Vec<_>>();
    assert_eq!(
        calls
            .iter()
            .map(|r| r["params"]["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "databases",
            "query",
            "neighbors",
            "neighbors",
            "databases",
            "get",
            "neighbors",
            "databases"
        ]
    );
    assert_eq!(
        calls[1]["params"]["arguments"],
        json!({"db":"project","expected_db_id":A,"text":"memory","k":16,"depth":0,"max_nodes":16})
    );
    assert_eq!(
        calls[2]["params"]["arguments"],
        json!({"db":"project","expected_db_id":A,"id":A})
    );
    assert_eq!(
        calls[3]["params"]["arguments"],
        json!({"db":"project","expected_db_id":A,"id":B})
    );
    assert_eq!(
        calls[5]["params"]["arguments"],
        json!({"db":"project","expected_db_id":A,"id":A,"body":true,"max_body_bytes":8192})
    );
    assert_eq!(
        requests.iter().filter(|r| r["method"] == "DELETE").count(),
        1
    );
}

async fn query_fixture(owner: &MockOwner) -> Graph {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let (reply, mut responses) = tokio::sync::mpsc::channel(1);
    let worker = tokio::spawn(serve(owner.target(), rx, reply));
    tx.send(Request::Query("memory".into())).await.unwrap();
    let Response::Loaded(graph) = receive(&mut responses).await else {
        panic!("optional links must not discard search hits");
    };
    tx.send(Request::Shutdown).await.unwrap();
    worker.await.unwrap();
    graph
}

#[tokio::test]
async fn query_links_deduplicate_without_merging_direction_or_kind_or_expanding_field() {
    let mut transition = edge(B, false);
    transition["kind"] = json!("transition");
    transition["weight"] = json!(0.3);
    let owner = MockOwner::start_edges(EdgeFixture {
        hits: hits(&[B, A]),
        pages: BTreeMap::from([
            (
                A.into(),
                json!({"items":[edge(B,true),edge(B,false),transition,edge(C,false)],"has_more":false}),
            ),
            (
                B.into(),
                json!({"items":[edge(A,false),edge(A,true)],"has_more":false}),
            ),
        ]),
        ..EdgeFixture::default()
    });
    let graph = query_fixture(&owner).await;
    assert_eq!(
        graph
            .nodes
            .iter()
            .map(|n| n.id.as_str())
            .collect::<Vec<_>>(),
        [B, A]
    );
    assert_eq!(
        graph
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str(), e.kind.as_str(), e.weight))
            .collect::<Vec<_>>(),
        [
            (A, B, "derived_from", 0.7),
            (A, B, "transition", 0.3),
            (B, A, "derived_from", 0.7),
        ]
    );
    assert!(!graph.partial);
    let requests = owner.requests.lock().unwrap();
    let ids = requests
        .iter()
        .filter(|r| r["params"]["name"] == "neighbors")
        .map(|r| r["params"]["arguments"]["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, [A, B]);
}

#[tokio::test]
async fn singleton_query_skips_edge_reads() {
    let owner = MockOwner::start_edges(EdgeFixture {
        hits: hits(&[A]),
        ..EdgeFixture::default()
    });
    let graph = query_fixture(&owner).await;
    assert_eq!(graph.nodes.len(), 1);
    assert!(graph.edges.is_empty());
    assert!(!graph.partial);
    assert!(
        !owner
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["params"]["name"] == "neighbors")
    );
}

#[tokio::test]
async fn query_links_report_owner_truncation_without_inventing_missing_edges() {
    let owner = MockOwner::start_edges(EdgeFixture {
        hits: hits(&[A, B]),
        pages: BTreeMap::from([(A.into(), json!({"items":[edge(B,false)],"has_more":true}))]),
        ..EdgeFixture::default()
    });
    let graph = query_fixture(&owner).await;
    assert_eq!(graph.nodes.len(), 2);
    assert_eq!(graph.edges.len(), 1);
    assert!(graph.partial);
    assert!(graph.note.contains("links partial"));
}

#[tokio::test]
async fn query_links_cap_each_page_and_final_field_in_canonical_order() {
    let ids = (0..QUERY_NODES)
        .map(|i| format!("{i:026}"))
        .collect::<Vec<_>>();
    let rows = ids
        .iter()
        .enumerate()
        .flat_map(|(i, from)| {
            ids.iter()
                .skip(i + 1)
                .map(move |to| (from.clone(), to.clone()))
        })
        .collect::<Vec<_>>();
    let pages = ids
        .iter()
        .map(|id| {
            let mut items = rows
                .iter()
                .filter(|(from, _)| from == id)
                .map(|(_, to)| edge(to, false))
                .collect::<Vec<_>>();
            items.reverse();
            // Out-of-field rows still consume the per-page budget.
            items.extend((0..40).map(|_| edge(C, false)));
            (id.clone(), json!({"items":items,"has_more":false}))
        })
        .collect();
    let owner = MockOwner::start_edges(EdgeFixture {
        hits: hits(&ids.iter().map(String::as_str).collect::<Vec<_>>()),
        pages,
        ..EdgeFixture::default()
    });
    let graph = query_fixture(&owner).await;
    assert_eq!(graph.nodes.len(), QUERY_NODES);
    assert_eq!(graph.edges.len(), NEIGHBORS);
    assert!(graph.partial);
    assert_eq!(
        graph
            .edges
            .iter()
            .map(|e| (e.from.clone(), e.to.clone()))
            .collect::<Vec<_>>(),
        rows.into_iter().take(NEIGHBORS).collect::<Vec<_>>()
    );
    assert_eq!(
        owner
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["params"]["name"] == "neighbors")
            .count(),
        QUERY_NODES
    );

    let owner = MockOwner::start_edges(EdgeFixture {
        hits: hits(&[A, B]),
        pages: BTreeMap::from([(
            A.into(),
            json!({"items":(0..NEIGHBORS).map(|_|edge(C,false)).chain([edge(B,false)]).collect::<Vec<_>>(),"has_more":false}),
        )]),
        ..EdgeFixture::default()
    });
    let graph = query_fixture(&owner).await;
    assert!(
        graph.edges.is_empty(),
        "row33 must not pass the page budget"
    );
    assert!(graph.partial);
}

#[tokio::test]
async fn malformed_or_failed_links_preserve_hits_and_prior_edges_then_reconnect() {
    for fail in [false, true] {
        let owner = MockOwner::start_edges(EdgeFixture {
            hits: hits(&[A, B, C]),
            pages: BTreeMap::from([
                (A.into(), json!({"items":[edge(B,false)],"has_more":false})),
                (
                    B.into(),
                    json!({"items":[{"neighbor":A,"kind":"invented","incoming":true,"weight":0.7}],"has_more":false}),
                ),
            ]),
            fail_on: fail.then(|| B.into()),
            ..EdgeFixture::default()
        });
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (reply, mut responses) = tokio::sync::mpsc::channel(1);
        let worker = tokio::spawn(serve(owner.target(), rx, reply));
        tx.send(Request::Query("memory".into())).await.unwrap();
        let Response::Loaded(graph) = receive(&mut responses).await else {
            panic!("search was discarded");
        };
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!((&*graph.edges[0].from, &*graph.edges[0].to), (A, B));
        assert!(graph.partial);
        assert!(graph.note.contains("edge read failed"));
        tx.send(Request::Poll).await.unwrap();
        assert!(matches!(
            receive(&mut responses).await,
            Response::Activity(_)
        ));
        tx.send(Request::Shutdown).await.unwrap();
        worker.await.unwrap();
        let requests = owner.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            2
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["params"]["name"] == "query")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["params"]["name"] == "neighbors")
                .count(),
            2
        );
        assert_eq!(
            requests.iter().filter(|r| r["method"] == "DELETE").count(),
            2
        );
    }
}

#[tokio::test]
async fn edge_reads_share_one_deadline_and_keep_completed_results() {
    let owner = MockOwner::start_edges(EdgeFixture {
        hits: hits(&[A, B, C]),
        pages: BTreeMap::from([(A.into(), json!({"items":[edge(B,false)],"has_more":false}))]),
        delay: Duration::from_millis(80),
        ..EdgeFixture::default()
    });
    let target = owner.target();
    let mut connection = RemoteClient::connect(&ConnectionOptions {
        url: target.url,
        ssh_mcp_port: target.ssh_mcp_port,
        token_env: None,
    })
    .await
    .unwrap();
    let mut graph = query_graph(&query(hits(&[A, B, C]))).unwrap();
    let started = tokio::time::Instant::now();
    assert!(
        !query_edges(
            &mut connection,
            "project",
            A,
            &mut graph,
            Duration::from_millis(140)
        )
        .await
    );
    assert!(started.elapsed() < Duration::from_millis(300));
    drop(connection);
    assert_eq!(graph.nodes.len(), 3);
    assert_eq!(graph.edges.len(), 1);
    assert!(graph.partial);
    assert!(graph.note.contains("read budget reached"));
    let requests = owner.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["params"]["name"] == "neighbors")
            .count(),
        2
    );
    // An expired retrieval deadline must still release the HTTP session. Drop
    // alone cannot do this; cleanup has its own finite budget.
    assert_eq!(
        requests.iter().filter(|r| r["method"] == "DELETE").count(),
        1
    );
}

#[tokio::test]
async fn reconnect_does_not_silently_switch_database_identity() {
    let owner = MockOwner::start(true);
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let (reply, mut responses) = tokio::sync::mpsc::channel(1);
    let worker = tokio::spawn(serve(owner.target(), rx, reply));
    tx.send(Request::Query("memory".into())).await.unwrap();
    assert!(
        matches!(receive(&mut responses).await, Response::Error(error) if error.contains("temporary fixture error"))
    );
    tx.send(Request::Poll).await.unwrap();
    assert!(
        matches!(receive(&mut responses).await, Response::Error(error) if error.contains("identity changed"))
    );
    tx.send(Request::Shutdown).await.unwrap();
    worker.await.unwrap();
    let requests = owner.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "initialize")
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["params"]["name"] == "query")
            .count(),
        1
    );
}

#[tokio::test]
async fn worker_reads_optional_feed_without_replaying_history() {
    let owner = MockOwner::start_mode(false, true);
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let (reply, mut responses) = tokio::sync::mpsc::channel(1);
    let worker = tokio::spawn(serve(owner.target(), rx, reply));
    tx.send(Request::Poll).await.unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::Activity(a) if a.feed && a.returns==0)
    );
    tx.send(Request::Poll).await.unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::Activity(a) if a.feed && a.returns==1 && a.node_ids==[A])
    );
    tx.send(Request::Shutdown).await.unwrap();
    worker.await.unwrap();
    let requests = owner.requests.lock().unwrap();
    let activity = requests
        .iter()
        .filter(|r| r["params"]["name"] == "activity")
        .collect::<Vec<_>>();
    assert_eq!(
        activity[0]["params"]["arguments"],
        json!({"after":0,"limit":32})
    );
    assert_eq!(
        activity[1]["params"]["arguments"],
        json!({"after":1,"limit":32})
    );
}

#[tokio::test]
async fn global_target_refuses_wrong_advertised_path_before_memory_read() {
    let owner = MockOwner::start(false);
    let mut target = owner.target();
    target.expected_path = Some("/another-global.db".into());
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let (reply, mut responses) = tokio::sync::mpsc::channel(1);
    let worker = tokio::spawn(serve(target, rx, reply));
    tx.send(Request::Query("memory".into())).await.unwrap();
    assert!(
        matches!(receive(&mut responses).await, Response::Error(error) if error.contains("path does not match"))
    );
    tx.send(Request::Shutdown).await.unwrap();
    worker.await.unwrap();
    let requests = owner.requests.lock().unwrap();
    assert!(!requests.iter().any(|r| r["params"]["name"] == "query"));
}
#[tokio::test]
async fn native_scenes_pin_owner_and_get_the_displayed_edition_without_other_reads() {
    use crate::scenes::SceneAxis;
    let owner = MockOwner::start_scenes();
    let (tx, rx) = mpsc::channel(1);
    let (reply, mut responses) = mpsc::channel(1);
    let worker = tokio::spawn(serve(owner.target(), rx, reply));
    tx.send(Request::Scenes {
        axis: SceneAxis::Recorded,
        cue: None,
    })
    .await
    .unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::Scenes(page) if page.items.len()==1 && page.items[0].revision==0)
    );
    tx.send(Request::Scene {
        episode_id: B.into(),
        edition_id: B.into(),
    })
    .await
    .unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::Scene(scene) if scene.edition_id==B && scene.current_edition_id==C)
    );
    tx.send(Request::Scenes {
        axis: SceneAxis::Occurred,
        cue: Some("native".into()),
    })
    .await
    .unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::Scenes(page) if page.cue.as_deref()==Some("native"))
    );
    tx.send(Request::Shutdown).await.unwrap();
    worker.await.unwrap();
    let calls = owner.requests.lock().unwrap();
    let episode: Vec<_> = calls
        .iter()
        .filter(|r| r["params"]["name"] == "episode")
        .collect();
    assert_eq!(episode.len(), 3);
    assert_eq!(
        episode[0]["params"]["arguments"],
        json!({"db":"project","expected_db_id":A,"action":"list","axis":"recorded","order":"newest_first","limit":32})
    );
    assert_eq!(
        episode[1]["params"]["arguments"],
        json!({"db":"project","expected_db_id":A,"action":"get","episode_id":B,"edition_id":B,"body":true,"max_bytes":8192})
    );
    assert_eq!(
        episode[2]["params"]["arguments"],
        json!({"db":"project","expected_db_id":A,"action":"search","cue":"native","limit":32})
    );
    assert!(!calls.iter().any(|r| matches!(
        r["params"]["name"].as_str(),
        Some("query" | "get" | "neighbors")
    )));
}

#[tokio::test]
async fn absent_episode_surface_is_unavailable_not_an_empty_scene_page() {
    use crate::scenes::SceneAxis;
    let owner = MockOwner::start(false);
    let (tx, rx) = mpsc::channel(1);
    let (reply, mut responses) = mpsc::channel(1);
    let worker = tokio::spawn(serve(owner.target(), rx, reply));
    tx.send(Request::Scenes {
        axis: SceneAxis::Recorded,
        cue: None,
    })
    .await
    .unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::Error(error) if error.contains("does not advertise"))
    );
    tx.send(Request::Shutdown).await.unwrap();
    worker.await.unwrap();
    assert!(
        !owner
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["params"]["name"] == "episode")
    );
}
#[tokio::test]
async fn advertised_episode_without_identity_guard_never_receives_scene_read() {
    use crate::scenes::SceneAxis;
    let owner = MockOwner::start_config(false, false, None, true, false);
    let (tx, rx) = mpsc::channel(1);
    let (reply, mut responses) = mpsc::channel(1);
    let worker = tokio::spawn(serve(owner.target(), rx, reply));
    tx.send(Request::Scenes {
        axis: SceneAxis::Recorded,
        cue: None,
    })
    .await
    .unwrap();
    assert!(
        matches!(receive(&mut responses).await,Response::Error(error) if error.contains("expected_db_id") && error.contains("not advertised"))
    );
    tx.send(Request::Shutdown).await.unwrap();
    worker.await.unwrap();
    assert!(
        !owner
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["params"]["name"] == "episode")
    );
}

#[tokio::test]
async fn graph_worker_crawls_native_topology_then_batches_summaries_without_search_or_neighbors() {
    let owner = MockOwner::start_graph(false);
    let (tx, rx) = mpsc::channel(1);
    let (reply, mut answers) = mpsc::channel(4);
    let task = tokio::spawn(serve(owner.target(), rx, reply));
    let mut count = 0;
    let mut next = None;
    let mut edges = 0;
    loop {
        tx.send(Request::Inventory {
            after: next.clone(),
        })
        .await
        .unwrap();
        let Response::Inventory(page) = receive(&mut answers).await else {
            panic!("inventory failed")
        };
        count += page.graph.nodes.len();
        edges += page.graph.edges.len();
        next = page.graph.inventory.unwrap().next;
        if next.is_none() {
            break;
        }
    }
    assert_eq!((count, edges), (130, 1));
    let ids = vec![format!("{:026}", 0), format!("{:026}", 129)];
    tx.send(Request::Summaries { ids: ids.clone() })
        .await
        .unwrap();
    let Response::Summaries(cards) = receive(&mut answers).await else {
        panic!("summaries failed")
    };
    assert_eq!(cards.nodes.len(), 2);
    assert!(cards.nodes.iter().all(|card| card.body.is_empty()));
    tx.send(Request::Shutdown).await.unwrap();
    task.await.unwrap();
    let log = owner.requests.lock().unwrap();
    let calls = log
        .iter()
        .filter(|row| row["method"] == "tools/call")
        .collect::<Vec<_>>();
    assert!(!calls.iter().any(|row| matches!(
        row["params"]["name"].as_str(),
        Some("query" | "neighbors" | "get")
    )));
    let graph = calls
        .iter()
        .filter(|row| row["params"]["name"] == "graph")
        .collect::<Vec<_>>();
    assert_eq!(graph.len(), 4);
    assert!(
        graph
            .iter()
            .all(|row| row["params"]["arguments"]["db"] == "project"
                && row["params"]["arguments"]["expected_db_id"] == A)
    );
}
#[tokio::test]
async fn graph_worker_refuses_old_owner_without_fallback_and_pins_identity_across_pages() {
    for native in [false, true] {
        let owner = if native {
            MockOwner::start_graph(true)
        } else {
            MockOwner::start(false)
        };
        let (tx, rx) = mpsc::channel(1);
        let (reply, mut answers) = mpsc::channel(4);
        let task = tokio::spawn(serve(owner.target(), rx, reply));
        tx.send(Request::Inventory { after: None }).await.unwrap();
        if native {
            assert!(matches!(
                receive(&mut answers).await,
                Response::Inventory(_)
            ));
            tx.send(Request::Inventory {
                after: Some("one".into()),
            })
            .await
            .unwrap();
        }
        assert!(matches!(receive(&mut answers).await, Response::Error(_)));
        tx.send(Request::Shutdown).await.unwrap();
        task.await.unwrap();
        let log = owner.requests.lock().unwrap();
        assert!(
            !log.iter()
                .any(|row| matches!(row["params"]["name"].as_str(), Some("query" | "neighbors")))
        );
        assert_eq!(
            log.iter()
                .filter(|row| row["params"]["name"] == "graph")
                .count(),
            if native { 2 } else { 0 }
        );
    }
}

#[tokio::test]
async fn edge_lens_reads_no_body_but_explicit_memory_open_does() {
    let owner = MockOwner::start(false);
    let (tx, rx) = mpsc::channel(1);
    let (reply, mut answers) = mpsc::channel(4);
    let task = tokio::spawn(serve(owner.target(), rx, reply));
    tx.send(Request::Lens(A.into())).await.unwrap();
    assert!(matches!(receive(&mut answers).await, Response::Loaded(_)));
    tx.send(Request::Focus(A.into())).await.unwrap();
    assert!(matches!(receive(&mut answers).await, Response::Loaded(_)));
    tx.send(Request::Shutdown).await.unwrap();
    task.await.unwrap();
    let log = owner.requests.lock().unwrap();
    let gets = log
        .iter()
        .filter(|row| row["params"]["name"] == "get")
        .collect::<Vec<_>>();
    assert_eq!(gets.len(), 2);
    assert!(
        gets[0]["params"]["arguments"]
            .get("max_body_bytes")
            .is_none()
    );
    assert_eq!(gets[0]["params"]["arguments"]["body"], false);
    assert_eq!(gets[1]["params"]["arguments"]["body"], true);
}

#[tokio::test]
async fn repeated_cancel_barriers_keep_one_session_and_release_it_at_shutdown() {
    let owner = MockOwner::start_graph(false);
    let (tx, rx) = mpsc::channel(2);
    let (reply, mut answers) = mpsc::channel(4);
    let task = tokio::spawn(serve(owner.target(), rx, reply));
    for _ in 0..40 {
        tx.send(Request::Inventory { after: None }).await.unwrap();
        tx.send(Request::Cancel).await.unwrap();
        assert!(matches!(
            receive(&mut answers).await,
            Response::Inventory(_)
        ));
        assert!(matches!(receive(&mut answers).await, Response::Canceled));
    }
    tx.send(Request::Shutdown).await.unwrap();
    task.await.unwrap();
    let log = owner.requests.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|row| row["method"] == "initialize")
            .count(),
        1
    );
    assert_eq!(
        log.iter().filter(|row| row["method"] == "DELETE").count(),
        1
    );
    assert_eq!(
        log.iter()
            .filter(|row| row["params"]["name"] == "databases")
            .count(),
        1
    );
}
