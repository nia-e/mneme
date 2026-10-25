//! Configured-cache protocol regressions. The catalog and every owner response
//! are synthetic; no database, body directory, service, or user config is opened.
use super::*;
use crate::model::ConfiguredTarget;
use mneme_config::{Config, SelectionContext};
use mneme_store_client::ReadSource;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

const DB: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const NODE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const OTHER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
const ANNOTATION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAY";
const ABSENT: &str = "00000000000000000000000000";
const DEVICE: &str = "synthetic-cache";
const CAPTURED: u64 = 1_800_000_000;

struct CacheFixture {
    dir: PathBuf,
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stopped: Arc<AtomicBool>,
    malformed_neighbors: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CacheFixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "mneme-tui-cache-{}-{timestamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let malformed_neighbors = Arc::new(AtomicBool::new(false));
        let malformed = malformed_neighbors.clone();
        let (log, stop, root) = (requests.clone(), stopped.clone(), dir.clone());
        let thread = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("cache fixture accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let (method, path, request) = read_request(&mut stream);
                let canonical = path == "/canonical";
                if method == "DELETE" {
                    log.lock()
                        .unwrap()
                        .push(json!({"method":"DELETE","_fixture_path":path}));
                    reply(&mut stream, 204, None);
                    continue;
                }
                let mut logged = request.clone();
                logged["_fixture_path"] = json!(path);
                log.lock().unwrap().push(logged);
                if request["method"] == "notifications/initialized" {
                    reply(&mut stream, 202, None);
                    continue;
                }
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => json!({
                        "protocolVersion":"2025-11-25", "capabilities":{"tools":{}},
                        "serverInfo":{"name":"mneme-mcp","version":"synthetic-cache"},
                        "capabilityProfile":"read-only"
                    }),
                    "tools/list" => json!({"tools":tool_catalog()}),
                    "tools/call" => {
                        let name = request["params"]["name"].as_str().unwrap();
                        let args = &request["params"]["arguments"];
                        assert!(
                            args.get("_mneme_source").is_none(),
                            "client witness leaked upstream"
                        );
                        if canonical && name == "neighbors" {
                            // Explicit availability failure after a successful
                            // live query/get. An arbitrary dropped POST is not
                            // classified as safe availability fallback.
                            reply(&mut stream, 503, None);
                            continue;
                        }
                        if name == "get" && args["id"] == ABSENT {
                            json!({"isError":true,"content":[{"type":"text","text":"node not found"}]})
                        } else {
                            let payload = if name == "neighbors"
                                && !canonical
                                && malformed.load(Ordering::Relaxed)
                            {
                                // Raw MCP success is not accepted domain
                                // evidence. A valid prefix must not leak when
                                // the second edge lacks its required weight.
                                json!({"items":[
                                    {"neighbor":OTHER,"summary":"Cached prefix","kind":"derived_from","incoming":false,"weight":0.7},
                                    {"neighbor":OTHER,"summary":"Malformed suffix","kind":"derived_from","incoming":false}
                                ],"has_more":false})
                            } else {
                                payload(name, args, &root, canonical)
                            };
                            json!({"content":[{"type":"text","text":payload.to_string()}]})
                        }
                    }
                    method => panic!("unexpected cache fixture method {method}"),
                };
                reply(
                    &mut stream,
                    200,
                    Some(json!({
                        "jsonrpc":"2.0", "id":request["id"], "result":result
                    })),
                );
            }
        });
        let fixture = Self {
            dir,
            url,
            requests,
            stopped,
            malformed_neighbors,
            thread: Some(thread),
        };
        fixture.catalog(&["g1"]);
        fixture
    }

    fn catalog(&self, generations: &[&str]) {
        let replicas: Vec<_> = generations
            .iter()
            .map(|generation| {
                json!({
                    "source_device_id":DEVICE,
                    "database":format!("project_{generation}"),
                    "resolved_path":self.dir.join(generation).join("store.db"),
                    "generation":generation,
                    "captured_at":CAPTURED + u64::from(*generation == "g2")
                })
            })
            .collect();
        let catalog = json!({
            "schema":"mneme.library.catalog.v1", "library_id":"synthetic-library", "revision":1,
            "entries":[{
                "project_id":"synthetic-project", "db_id":DB, "owner_device_id":"synthetic-owner",
                "display_name":"Synthetic fixture", "database":"project", "revision":1,
                "replicas":replicas
            }]
        });
        std::fs::write(
            self.dir.join("catalog.json"),
            serde_json::to_vec(&catalog).unwrap(),
        )
        .unwrap();
    }

    fn target(&self) -> Target {
        // Port zero cannot be a listening service; no real owner is probed.
        self.target_with_owner("http://127.0.0.1:0/mcp")
    }

    fn target_with_owner(&self, owner: &str) -> Target {
        let config = Config::parse(&format!(
            r#"schema="mneme.config.v1"
read_default="synthetic"
[stores.synthetic]
id="{DB}"
owner={{url="{owner}",db="project"}}
active={{workspace="any"}}
permissions={{read=true,write=false}}
cache={{library="reference"}}
[libraries.reference]
catalog={catalog}
[libraries.reference.replicas.synthetic-cache]
url={url}
"#,
            catalog = json!(self.dir.join("catalog.json")),
            url = json!(self.url),
        ))
        .unwrap();
        let selection =
            mneme_config::select(&config, &SelectionContext::new(self.dir.clone())).unwrap();
        Target {
            name: "Synthetic cache".into(),
            database: "project".into(),
            url: owner.into(),
            ssh_mcp_port: 18766,
            token_env: None,
            expected_db_id: Some(DB.into()),
            expected_path: None,
            configured: Some(ConfiguredTarget {
                selection: Arc::new(selection),
                store: "synthetic".into(),
            }),
        }
    }

    fn witness(&self, generation: &str) -> ReadSource {
        ReadSource {
            owner_db_id: DB.into(),
            source_device_id: DEVICE.into(),
            generation: generation.into(),
            captured_at: CAPTURED + u64::from(generation == "g2"),
            database: format!("project_{generation}"),
            resolved_path: self.dir.join(generation).join("store.db"),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| {
                if request["method"] == "tools/call" {
                    request["params"]["name"].as_str().unwrap().into()
                } else {
                    request["method"].as_str().unwrap().into()
                }
            })
            .collect()
    }
}

impl Drop for CacheFixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        let result = self.thread.take().unwrap().join();
        std::fs::remove_dir_all(&self.dir).unwrap();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

fn tool_catalog() -> Value {
    let guard = json!({"type":"string","minLength":26,"maxLength":26,
        "pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"});
    let mut tools: Vec<_> = ["databases", "get", "query", "neighbors", "activity"].into_iter()
        .map(|name| json!({"name":name,"inputSchema":{"type":"object","properties":{"expected_db_id":guard}}}))
        .collect();
    for (name, actions) in [
        ("graph", vec!["topology", "summaries"]),
        ("episode", vec!["list", "search", "get"]),
    ] {
        let branches: Vec<_> = actions
            .iter()
            .map(|action| {
                json!({
                    "properties":{"action":{"const":action},"expected_db_id":guard}
                })
            })
            .collect();
        tools.push(json!({"name":name,"inputSchema":{
            "properties":{"action":{"type":"string","enum":actions},"expected_db_id":guard},
            "oneOf":branches
        }}));
    }
    tools.push(json!({"name":"list","inputSchema":{
        "additionalProperties":false,"required":["kind"],
        "properties":{"expected_db_id":guard,"kind":{"const":"touchstones"},
            "limit":{"maximum":32},"after":{"maxLength":1024}}
    }}));
    json!(tools)
}

fn payload(tool: &str, args: &Value, root: &std::path::Path, canonical: bool) -> Value {
    if tool == "databases" {
        if canonical {
            return json!([{"db":"project","db_id":DB,"state":"open",
                "resolved_path":root.join("canonical").join("store.db"),
                "in_flight":0,"backend_jobs":0}]);
        }
        return json!(["g1", "g2"].map(|generation| json!({
            "db":format!("project_{generation}"), "db_id":DB, "state":"open",
            "resolved_path":root.join(generation).join("store.db"),
            "in_flight":0,"backend_jobs":0
        })));
    }
    if tool != "activity" {
        if canonical {
            assert_eq!(args["db"], "project");
        } else {
            assert!(matches!(
                args["db"].as_str(),
                Some("project_g1" | "project_g2")
            ));
        }
        assert_eq!(args["expected_db_id"], DB);
    }
    match tool {
        "graph" if args["action"] == "topology" => json!({
            "db_id":DB,"action":"topology",
            "nodes":[{"id":NODE,"status":"active","kind":"note"}],"edges":[],
            "has_more":false,"next_cursor":null,
            "coverage":{"snapshot":false,"nodes_done":true,"edges_done":true}
        }),
        "graph" => {
            json!({"db_id":DB,"action":"summaries", "items":args["ids"].as_array().unwrap().iter().map(|id| json!({
            "id":id,"missing":false,"summary":"Synthetic viewport card","status":"active","tags":[]
        })).collect::<Vec<_>>()})
        }
        "query" => json!({"schema":"mneme.query.v3","partial":false,
        "lanes":{"primary":{"seed_coverage":null,"hits":[
                {"id":NODE,"summary":if canonical {"Canonical live hit"} else {"Synthetic hit"},"status":"active","lane_rank":1},
            {"id":OTHER,"summary":"Another synthetic hit","status":"active","lane_rank":2}
        ]}}}),
        "neighbors" => json!({"items":[],"has_more":false}),
        "get" => {
            let id = args["id"].as_str().unwrap();
            let mut value = json!({"db":args["db"],"db_id":DB,"id":id,
                "summary":if canonical {"Canonical live memory"} else {"Synthetic exact memory"},
                "body":"Synthetic body","status":"active",
                "body_range":{"has_more":false},
                "summary_snapshot":{"db_id":DB,"id":id,"coverage":"summary_only"}});
            if id == ANNOTATION {
                value["touchstone"] = json!({"owner":ANNOTATION,"subject":"Synthetic authored meaning",
                    "references":[{"db_id":DB,"id":NODE,"summary":"Synthetic citation",
                        "provenance":{"kind":"unknown"},"created":42,"memory_kind":{"kind":"semantic"}}]});
                value["touchstone_current"] = json!({"coverage":"summary_only",
                    "references":[{"db_id":DB,"id":NODE,"status":"unchanged"}]});
            }
            value
        }
        "list" => json!({"db_id":DB,"kind":"touchstones","items":[{
            "id":ANNOTATION,"summary":"Synthetic annotation","subject":"Synthetic authored meaning",
            "reference_count":1,"status":"Active"
        }],"next_cursor":null,"has_more":false}),
        "episode" => {
            let mut header = json!({"episode_id":OTHER,"edition_id":OTHER,"current_edition_id":OTHER,
                "revision":0,"summary":"Synthetic scene","occurred":{"kind":"unknown"},
                "recorded_at":42,"edition_recorded_at":42,"thread":null});
            match args["action"].as_str().unwrap() {
                "list" => {
                    json!({"db_id":DB,"action":"list","items":[header],"next":null,"partial":false})
                }
                "search" => {
                    json!({"db_id":DB,"action":"search","mode":"lexical","items":[header],"has_more":false,"partial":false})
                }
                "get" => {
                    header.as_object_mut().unwrap().extend(
                        json!({"db_id":DB,"action":"get","is_current":true,
                        "body":"Synthetic scene body","body_range":{"has_more":false},
                        "source":{"namespace":"fixture","key":"scene","reference":"synthetic"}})
                        .as_object()
                        .unwrap()
                        .clone(),
                    );
                    header
                }
                action => panic!("unexpected fixture episode action {action}"),
            }
        }
        "activity" => json!({"schema":"mneme.activity.v1","instance":OTHER,"events":[],
            "next_after":0,"oldest_seq":0,"latest_seq":0,"missed":false,"dropped":0,"has_more":false,"busy":false}),
        tool => panic!("unexpected cache fixture tool {tool}"),
    }
}

fn read_request(stream: &mut TcpStream) -> (String, String, Value) {
    let mut raw = Vec::new();
    let mut byte = [0];
    while !raw.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        raw.push(byte[0]);
        assert!(raw.len() <= 8192);
    }
    let header = String::from_utf8(raw).unwrap();
    let mut request_line = header.lines().next().unwrap().split_whitespace();
    let method = request_line.next().unwrap().into();
    let path = request_line.next().unwrap().into();
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
        path,
        if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        },
    )
}

fn reply(stream: &mut TcpStream, status: u16, value: Option<Value>) {
    let body = value.map(|value| value.to_string()).unwrap_or_default();
    write!(stream, "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nMcp-Session-Id: cache-fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    stream.flush().unwrap();
}

async fn receive(responses: &mut Receiver<Response>) -> Response {
    tokio::time::timeout(Duration::from_secs(8), responses.recv())
        .await
        .unwrap()
        .unwrap()
}

fn assert_source(response: &Response, expected: &ReadSource) {
    let source = match response {
        Response::Inventory(page) => &page.graph.read_source,
        Response::Summaries(page) => &page.read_source,
        Response::Loaded(graph) => &graph.read_source,
        Response::Search(page) => &page.read_source,
        Response::Scenes(page) => &page.read_source,
        Response::Scene(scene) => &scene.read_source,
        Response::Touchstones(page) => &page.read_source,
        Response::Touchstone(detail) => &detail.read_source,
        Response::TouchstoneTarget(target) => &target.read_source,
        Response::Error(error) => panic!("configured cached read failed: {error}"),
        _ => panic!("expected a content response"),
    };
    assert_eq!(source.snapshot.as_ref(), Some(expected));
    assert!(!source.mixed, "registry metadata is not live content");
}

#[tokio::test]
async fn configured_cache_provenance_covers_all_content_without_extra_probes() {
    let fixture = CacheFixture::new();
    let target = fixture.target();
    assert!(
        fixture.calls().is_empty(),
        "selection must remain metadata-only"
    );
    let (requests, rx) = tokio::sync::mpsc::channel(4);
    let (tx, mut responses) = tokio::sync::mpsc::channel(4);
    let task = tokio::spawn(serve(target, rx, tx));
    let expected = fixture.witness("g1");
    requests
        .send(Request::Inventory { after: None })
        .await
        .unwrap();
    assert_source(&receive(&mut responses).await, &expected);
    // One catalog handshake; the selected source is learned from ordinary
    // registry/content replies, not another provenance lookup.
    assert_eq!(
        fixture.calls(),
        [
            "initialize",
            "notifications/initialized",
            "databases",
            "tools/list",
            "databases",
            "databases",
            "graph"
        ]
    );

    // A new newest generation must not redirect any already-selected source.
    fixture.catalog(&["g1", "g2"]);
    let reads = [
        (
            Request::Summaries {
                ids: vec![NODE.into()],
            },
            vec!["databases", "graph"],
        ),
        (
            Request::Query("synthetic".into()),
            vec!["databases", "databases", "query"],
        ),
        (
            Request::Focus(NODE.into()),
            vec!["databases", "databases", "get", "databases", "neighbors"],
        ),
        (
            Request::Lens(NODE.into()),
            vec!["databases", "databases", "get", "databases", "neighbors"],
        ),
        (
            Request::Scenes {
                axis: scenes::SceneAxis::Recorded,
                cue: None,
            },
            vec!["databases", "databases", "episode"],
        ),
        (
            Request::Scenes {
                axis: scenes::SceneAxis::Recorded,
                cue: Some("synthetic".into()),
            },
            vec!["databases", "databases", "episode"],
        ),
        (
            Request::Scene {
                episode_id: OTHER.into(),
                edition_id: OTHER.into(),
            },
            vec!["databases", "databases", "episode"],
        ),
        (
            Request::Touchstones { after: None },
            vec!["databases", "databases", "list"],
        ),
        (
            Request::Touchstone {
                id: ANNOTATION.into(),
            },
            vec!["databases", "databases", "get"],
        ),
        (
            Request::TouchstoneTarget {
                db_id: DB.into(),
                id: NODE.into(),
            },
            vec!["databases", "databases", "get"],
        ),
    ];
    for (request, expected_calls) in reads {
        let before = fixture.calls().len();
        requests.send(request).await.unwrap();
        assert_source(&receive(&mut responses).await, &expected);
        assert_eq!(&fixture.calls()[before..], expected_calls);
    }

    let before = fixture.calls().len();
    requests.send(Request::Poll).await.unwrap();
    assert!(matches!(
        receive(&mut responses).await,
        Response::Activity(_)
    ));
    assert_eq!(
        &fixture.calls()[before..],
        ["databases", "databases", "activity"]
    );
    requests
        .send(Request::Summaries {
            ids: vec![NODE.into()],
        })
        .await
        .unwrap();
    assert_source(&receive(&mut responses).await, &expected);

    // An exact negative answer is still an answer from the selected snapshot.
    let before = fixture.calls().len();
    requests
        .send(Request::TouchstoneTarget {
            db_id: DB.into(),
            id: ABSENT.into(),
        })
        .await
        .unwrap();
    let absent = receive(&mut responses).await;
    assert_source(&absent, &expected);
    assert!(matches!(absent, Response::TouchstoneTarget(target) if target.node.is_none()));
    assert_eq!(
        &fixture.calls()[before..],
        ["databases", "databases", "get", "DELETE"]
    );

    // The reserved source witness is consumed by the configured client; the
    // native endpoint receives only the literal generation alias and identity.
    for request in fixture.requests.lock().unwrap().iter().filter(|request| {
        request["method"] == "tools/call"
            && !matches!(
                request["params"]["name"].as_str(),
                Some("databases" | "activity")
            )
    }) {
        assert_eq!(request["params"]["arguments"]["db"], "project_g1");
        assert_eq!(request["params"]["arguments"]["expected_db_id"], DB);
    }
    requests.send(Request::Shutdown).await.unwrap();
    task.await.unwrap();

    // Prove the newer catalog entry is viable, rather than an ignored fixture:
    // a deliberate fresh reader selects g2 and returns the real client witness.
    let target = fixture.target();
    let mut fresh = ReadClient::configured(target.configured.as_ref().unwrap())
        .await
        .unwrap();
    let value = fresh
        .call_tool("get", json!({"id":NODE,"body":false}))
        .await
        .unwrap();
    assert_eq!(value["_mneme_source"], json!(fixture.witness("g2")));
    fresh.close().await;
}

#[tokio::test]
async fn expired_cache_pin_refuses_newer_generation_and_survives_failure() {
    let fixture = CacheFixture::new();
    let (requests, rx) = tokio::sync::mpsc::channel(4);
    let (tx, mut responses) = tokio::sync::mpsc::channel(4);
    let task = tokio::spawn(serve(fixture.target(), rx, tx));
    let expected = fixture.witness("g1");
    requests.send(Request::Focus(NODE.into())).await.unwrap();
    assert_source(&receive(&mut responses).await, &expected);

    fixture.catalog(&["g2"]);
    let before = fixture.calls().len();
    requests.send(Request::Focus(NODE.into())).await.unwrap();
    let error = receive(&mut responses).await;
    assert!(matches!(error, Response::Error(error) if error.contains("expired")));
    // Refusal is local to the exact route check. Only disposal of the retained
    // session is allowed; neither the newer generation nor an owner is read.
    assert_eq!(&fixture.calls()[before..], ["DELETE"]);

    fixture.catalog(&["g1", "g2"]);
    requests.send(Request::Focus(NODE.into())).await.unwrap();
    assert_source(&receive(&mut responses).await, &expected);
    requests.send(Request::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn canonical_content_then_cached_neighbors_marks_focus_mixed() {
    let fixture = CacheFixture::new();
    let canonical_url = format!("{}/canonical", fixture.url.trim_end_matches("/mcp"));
    let before = fixture.requests.lock().unwrap().len();
    let (requests, rx) = tokio::sync::mpsc::channel(4);
    let (tx, mut responses) = tokio::sync::mpsc::channel(4);
    let task = tokio::spawn(serve(fixture.target_with_owner(&canonical_url), rx, tx));
    requests.send(Request::Focus(NODE.into())).await.unwrap();
    let graph = match receive(&mut responses).await {
        Response::Loaded(graph) => graph,
        Response::Error(error) => panic!("mixed-source fixture failed: {error}"),
        _ => panic!("expected mixed-source graph"),
    };
    assert_eq!(graph.read_source.snapshot, Some(fixture.witness("g1")));
    assert!(
        graph.read_source.mixed,
        "live content must not become snapshot content"
    );
    assert!(
        graph
            .nodes
            .iter()
            .any(|node| node.id == NODE && node.summary == "Canonical live memory")
    );

    let domain_calls: Vec<_> = fixture.requests.lock().unwrap()[before..]
        .iter()
        .filter(|request| {
            request["method"] == "tools/call" && request["params"]["name"] != "databases"
        })
        .map(|request| {
            (
                request["_fixture_path"].as_str().unwrap().to_owned(),
                request["params"]["name"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let expected_calls = vec![
        ("/canonical".into(), "get".into()),
        ("/canonical".into(), "neighbors".into()),
        ("/mcp".into(), "neighbors".into()),
    ];
    assert_eq!(
        domain_calls, expected_calls,
        "no extra content or provenance lookup"
    );
    requests.send(Request::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn canonical_search_stays_canonical_without_reading_cached_neighbors() {
    let fixture = CacheFixture::new();
    fixture.malformed_neighbors.store(true, Ordering::Relaxed);
    let canonical_url = format!("{}/canonical", fixture.url.trim_end_matches("/mcp"));
    let (requests, rx) = tokio::sync::mpsc::channel(4);
    let (tx, mut responses) = tokio::sync::mpsc::channel(4);
    let task = tokio::spawn(serve(fixture.target_with_owner(&canonical_url), rx, tx));
    requests
        .send(Request::Query("synthetic".into()))
        .await
        .unwrap();
    let page = match receive(&mut responses).await {
        Response::Search(page) => page,
        Response::Error(error) => panic!("canonical search failed: {error}"),
        _ => panic!("expected ranked search results"),
    };
    assert!(!page.partial);
    assert!(
        page.nodes
            .iter()
            .any(|node| node.id == NODE && node.summary == "Canonical live hit")
    );
    assert!(page.read_source.snapshot.is_none());
    assert!(!page.read_source.mixed);
    assert!(
        !fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["params"]["name"] == "neighbors")
    );
    requests.send(Request::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn rejected_cached_neighbors_keep_the_selected_pin_for_later_focus() {
    let fixture = CacheFixture::new();
    fixture.malformed_neighbors.store(true, Ordering::Relaxed);
    let canonical_url = format!("{}/canonical", fixture.url.trim_end_matches("/mcp"));
    let (requests, rx) = tokio::sync::mpsc::channel(4);
    let (tx, mut responses) = tokio::sync::mpsc::channel(4);
    let task = tokio::spawn(serve(fixture.target_with_owner(&canonical_url), rx, tx));
    requests.send(Request::Focus(NODE.into())).await.unwrap();
    assert!(matches!(receive(&mut responses).await, Response::Error(_)));
    assert_eq!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| {
                request["_fixture_path"] == "/mcp" && request["params"]["name"] == "neighbors"
            })
            .count(),
        1,
        "the rejected page must actually have reached the cache"
    );

    fixture.malformed_neighbors.store(false, Ordering::Relaxed);
    let before = fixture.requests.lock().unwrap().len();
    requests.send(Request::Focus(NODE.into())).await.unwrap();
    assert_source(&receive(&mut responses).await, &fixture.witness("g1"));
    assert!(
        fixture.requests.lock().unwrap()[before..]
            .iter()
            .all(|request| request["_fixture_path"] == "/mcp"),
        "discarding invalid payload evidence must not discard the selected cache pin"
    );
    requests.send(Request::Shutdown).await.unwrap();
    task.await.unwrap();
}
