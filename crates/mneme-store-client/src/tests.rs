use super::*;
use mneme_config::{Config, SelectionContext};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
const DB: &str = "01M3CPEVDTM9PP4HJTMMBZ8HR9";
const NODE: &str = "01M3CV5EZP4HWAK56YJCMZQG2Y";
fn selection(url: &str, read: bool, write: bool) -> SelectionSnapshot {
    let config = Config::parse(&format!(
        r#"schema="mneme.config.v1"
read_default="alpha"
[stores.alpha]
id="{DB}"
owner={{url="{url}",db="project"}}
active={{workspace="any"}}
permissions={{read={read},write={write}}}
"#
    ))
    .unwrap();
    mneme_config::select(
        &config,
        &SelectionContext::new(std::env::current_dir().unwrap()),
    )
    .unwrap()
}
struct Fixture {
    url: String,
    calls: Arc<AtomicUsize>,
    catalog_calls: Arc<AtomicUsize>,
    requests: Arc<std::sync::Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.join.take().unwrap().join().unwrap();
    }
}
fn advertised_catalog() -> Value {
    let guard = json!({"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"});
    let mut episode = mneme_app::episode::input_schema(&mneme_app::episode::EpisodeAction::ALL);
    episode["properties"]["db"] = json!({"type":"string"});
    episode["properties"]["expected_db_id"] = guard.clone();
    for branch in episode["oneOf"].as_array_mut().unwrap() {
        branch["properties"]["db"] = json!({"type":"string"});
        branch["properties"]["expected_db_id"] = guard.clone();
    }
    let mut tools = json!([{"name":"databases","inputSchema":{"type":"object","properties":{}}},{"name":"get","inputSchema":{"type":"object","properties":{"db":{"type":"string"},"expected_db_id":guard,"id":{"type":"string"}}}},{"name":"forget","inputSchema":{"type":"object","properties":{"db":{"type":"string"},"expected_db_id":guard,"id":{"type":"string"}}}},{"name":"episode","inputSchema":episode}]);
    for (name, mut schema) in [
        ("retag", mneme_app::retag::input_schema()),
        ("edit_body", mneme_app::edit_body::input_schema()),
        ("edit_summary", mneme_app::edit_summary::input_schema()),
    ] {
        schema["properties"]["db"] = json!({"type":"string"});
        schema["properties"]["expected_db_id"] = guard.clone();
        schema["required"]
            .as_array_mut()
            .unwrap()
            .extend([json!("db"), json!("expected_db_id")]);
        tools
            .as_array_mut()
            .unwrap()
            .push(json!({"name":name,"inputSchema":schema}));
    }
    tools
}
#[derive(Default)]
struct CoreFixture {
    feature: Option<Value>,
    catalog_field: Option<Value>,
    notice: Option<Value>,
    wrong_result_identity: bool,
}
fn fixture(profile: &'static str, wrong_identity: bool, http_denied: bool) -> Fixture {
    fixture_with_core(profile, wrong_identity, http_denied, CoreFixture::default())
}
fn fixture_with_core(
    profile: &'static str,
    wrong_identity: bool,
    http_denied: bool,
    core: CoreFixture,
) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let catalog_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let server_calls = calls.clone();
    let server_catalog_calls = catalog_calls.clone();
    let server_requests = requests.clone();
    let server_stop = stop.clone();
    let join = std::thread::spawn(move || {
        while !server_stop.load(Ordering::Relaxed) {
            let (mut stream, _) = match listener.accept() {
                Ok(stream) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut buf = [0; 4096];
                let len = stream.read(&mut buf).unwrap();
                assert!(len > 0);
                bytes.extend_from_slice(&buf[..len]);
                if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let header = String::from_utf8_lossy(&bytes[..header_end]);
            let size = header
                .lines()
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|n| n.parse::<usize>().ok())
                })
                .unwrap_or(0);
            while bytes.len() - header_end < size {
                let mut buf = [0; 4096];
                let len = stream.read(&mut buf).unwrap();
                bytes.extend_from_slice(&buf[..len]);
            }
            if http_denied {
                stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                continue;
            }
            let msg: Value = if size == 0 {
                Value::Null
            } else {
                serde_json::from_slice(&bytes[header_end..header_end + size]).unwrap()
            };
            let body = match msg["method"].as_str() {
                Some("initialize") => {
                    let mut result = json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"mneme-mcp","version":"fixture"},"capabilityProfile":profile});
                    if let Some(feature) = core.feature.as_ref() {
                        result["capabilities"]["experimental"] = json!({
                            mneme_app::core::CONTRADICTION_SUMMARY_FEATURE: feature
                        });
                    }
                    Some(json!({"jsonrpc":"2.0","id":msg["id"],"result":result}))
                }
                Some("tools/list") => {
                    server_catalog_calls.fetch_add(1, Ordering::Relaxed);
                    let mut catalog = advertised_catalog();
                    let mut schema = json!({"name":"core","inputSchema":{"type":"object","properties":{"db":{"type":"string"},"expected_db_id":{"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"}}}});
                    if let Some(field) = core.catalog_field.as_ref() {
                        schema["inputSchema"]["properties"]["include_contradictions"] =
                            field.clone();
                    }
                    catalog.as_array_mut().unwrap().push(schema);
                    Some(json!({"jsonrpc":"2.0","id":msg["id"],"result":{"tools":catalog}}))
                }
                Some("tools/call") => {
                    server_requests.lock().unwrap().push(msg["params"].clone());
                    let name = msg["params"]["name"].as_str().unwrap();
                    let value = if name == "databases" {
                        json!([{"db":"project","db_id":if wrong_identity{NODE}else{DB},"state":"open","resolved_path":"/fixture/project.db"}])
                    } else {
                        server_calls.fetch_add(1, Ordering::Relaxed);
                        let args = &msg["params"]["arguments"];
                        match name {
                            "core" => {
                                let mut result = json!({"db":"project","db_id":if core.wrong_result_identity {NODE} else {DB},"nodes":[{"id":NODE,"summary":"fixture core card"}]});
                                if args["include_contradictions"] == true {
                                    if let Some(notice) = core.notice.as_ref() {
                                        result["contradiction_summary"] = notice.clone();
                                    }
                                }
                                result
                            }
                            "retag" => {
                                json!({"db":"project","db_id":DB,"id":args["id"],"tags":args["tags"],"changed":args["expected_tags"] != args["tags"]})
                            }
                            "edit_body" => {
                                json!({"db":"project","db_id":DB,"id":args["id"],"body_revision":"b".repeat(64)})
                            }
                            "edit_summary" => {
                                json!({"db":"project","db_id":DB,"id":args["id"],"summary_snapshot_sha256":"b".repeat(64)})
                            }
                            _ => {
                                json!({"db":"project","db_id":DB,"id":NODE,"summary":"fixture","summary_truncated":false})
                            }
                        }
                    };
                    Some(
                        json!({"jsonrpc":"2.0","id":msg["id"],"result":{"content":[{"type":"text","text":serde_json::to_string(&value).unwrap()}]}}),
                    )
                }
                _ => None,
            };
            let wire = body.map(|body| serde_json::to_string(&body).unwrap());
            let status = if wire.is_some() {
                "200 OK"
            } else {
                "202 Accepted"
            };
            let wire = wire.unwrap_or_default();
            write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nMcp-Session-Id: fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{wire}",wire.len()).unwrap();
        }
    });
    Fixture {
        url,
        calls,
        catalog_calls,
        requests,
        stop,
        join: Some(join),
    }
}
#[tokio::test]
async fn lazy_optional_outage_and_denied_routes_make_no_contact() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let client = ConfiguredClient::new(selection(
        &format!("http://{}", listener.local_addr().unwrap()),
        false,
        false,
    ))
    .unwrap();
    assert_eq!(client.databases()[0]["state"], "configured");
    assert!(
        client
            .call("alpha", "get", json!({"id":NODE}))
            .await
            .unwrap_err()
            .to_string()
            .contains("permission denied")
    );
    assert!(client.catalog("alpha").await.is_err());
    assert!(client.call("alpha", "nonsense", json!({})).await.is_err());
    assert!(listener.accept().is_err());
}
#[tokio::test]
async fn native_profiles_and_configured_ceiling_do_not_send_mutations() {
    for profile in ["read-only", "receipt-grounded", "curator", "operator"] {
        let server = fixture(profile, false, false);
        let client = ConfiguredClient::new(selection(&server.url, true, false)).unwrap();
        assert!(
            client
                .call("alpha", "forget", json!({"id":NODE}))
                .await
                .is_err()
        );
        assert_eq!(server.calls.load(Ordering::Relaxed), 0);
        client
            .call("alpha", "get", json!({"id":NODE}))
            .await
            .unwrap();
        assert_eq!(server.calls.load(Ordering::Relaxed), 1);
        client.close().await;
    }
}
#[tokio::test]
async fn shared_receive_budget_charges_handshake_and_reuses_session() {
    let server = fixture("read-only", false, false);
    let client = ConfiguredClient::new(selection(&server.url, true, false)).unwrap();
    let mut budget = OperationBudget::new(128 * 1024, Duration::from_secs(2)).unwrap();
    client
        .call_with_budget("alpha", "get", json!({"id":NODE}), &mut budget)
        .await
        .unwrap();
    let first = budget.bytes_used();
    assert!(first > 500);
    client
        .call_with_budget("alpha", "get", json!({"id":NODE}), &mut budget)
        .await
        .unwrap();
    let second = budget.bytes_used() - first;
    assert!(
        second < first,
        "second call must not repeat initialize/catalog"
    );
    let mut small = OperationBudget::new(1, Duration::from_secs(2)).unwrap();
    assert!(
        client
            .call_with_budget("alpha", "get", json!({"id":NODE}), &mut small)
            .await
            .is_err()
    );
    assert!(small.bytes_used() > 0);
    assert_eq!(small.bytes_remaining(), 0);
    client.close().await;
}
#[tokio::test]
async fn wrong_identity_and_auth_denial_never_fall_back() {
    for (identity, auth) in [(true, false), (false, true)] {
        let server = fixture("read-only", identity, auth);
        let mut snapshot = selection(&server.url, true, false);
        // Missing cache would produce a distinct error if a refusal wrongly fell back.
        snapshot.config.stores.get_mut("alpha").unwrap().cache = Some(mneme_config::Cache {
            library: "missing".into(),
            mode: mneme_config::CacheMode::OwnerFirst,
        });
        // Keep original checked snapshot; mutate through fresh selection so digest is genuine.
        snapshot.config.libraries.insert(
            "missing".into(),
            mneme_config::Library {
                catalog: "/nonexistent/fixture.json".into(),
                replicas: BTreeMap::new(),
                profiles: vec!["default".into()],
            },
        );
        snapshot = mneme_config::select(
            &snapshot.config,
            &SelectionContext::new(std::env::current_dir().unwrap()),
        )
        .unwrap();
        let client = ConfiguredClient::new(snapshot).unwrap();
        let error = client
            .call("alpha", "get", json!({"id":NODE}))
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("fixture.json"));
        assert_eq!(server.calls.load(Ordering::Relaxed), 0);
        client.close().await;
    }
}
#[tokio::test]
async fn complete_domain_admission_precedes_owner_contact() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let client = ConfiguredClient::new(selection(
        &format!("http://{}", listener.local_addr().unwrap()),
        true,
        true,
    ))
    .unwrap();
    for (tool, args) in [
        ("get", json!({"id":NODE,"body":"yes"})),
        ("core", json!({"include_contradictions":"yes"})),
        ("core", json!({"include_contradictions":null})),
        ("core", json!({"include_contradictions":1})),
        ("core", json!({"max_body_bytes":0})),
        (
            "core",
            json!({"include_contradictions":true,"unknown":true}),
        ),
        ("query", json!({"text":"x","k":-1})),
        ("query", json!({"text":"x","summary_max_bytes":0})),
        ("query", json!({"text":"x","summary_max_bytes":16385})),
        ("query", json!({"text":"x","summary_max_bytes":true})),
        ("query", json!({"text":"x","summary_max_bytes":1.0})),
        ("query", json!({"text":"x","summary_max_bytes":u64::MAX})),
        ("episode", json!({"action":"revise"})),
        ("save", json!({"summary":"x","source":{"namespace":"test"}})),
        (
            "retag",
            json!({"id":NODE,"tags":[],"expected_tags":[],"guard_nodes":[]}),
        ),
    ] {
        assert!(client.call("alpha", tool, args).await.is_err());
    }
    assert!(listener.accept().is_err());
}
#[test]
fn access_matrix_is_closed() {
    for tool in ORDINARY_TOOLS {
        let args = match *tool {
            "episode" => json!({"action":"get"}),
            "concern" => json!({"action":"list"}),
            "graph" => json!({"action":"topology"}),
            "walk" => json!({"action":"start"}),
            _ => json!({}),
        };
        assert!(request_access(tool, &args).is_ok(), "{tool}");
    }
    for (tool, args) in [
        ("episode", json!({"action":"delete"})),
        ("graph", json!({"action":"save"})),
        ("walk", json!({"action":"reflect"})),
        ("database_control", json!({"action":"release"})),
    ] {
        assert!(request_access(tool, &args).is_err());
    }
}

struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("mneme-configured-client-{}", ulid::Ulid::new()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[tokio::test]
async fn retained_session_rejects_changed_permissions_before_owner_contact() {
    let server = fixture("operator", false, false);
    let scratch = Scratch::new();
    let path = scratch.0.join("memory.toml");
    let mut config = selection(&server.url, true, true).config;
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let snapshot = mneme_config::select_workspace(&path, &scratch.0, None).unwrap();
    let client = ConfiguredClient::new(snapshot).unwrap();
    client
        .call("alpha", "get", json!({"id":NODE}))
        .await
        .unwrap();
    assert_eq!(server.calls.load(Ordering::Relaxed), 1);
    config.stores.get_mut("alpha").unwrap().permissions.write = false;
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let error = client
        .call("alpha", "forget", json!({"id":NODE}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("selection metadata changed"));
    assert_eq!(server.calls.load(Ordering::Relaxed), 1);
    assert!(client.catalog("alpha").await.is_err());
    client.close().await;
}
#[tokio::test]
async fn newly_created_isolation_boundary_fences_retained_session() {
    let server = fixture("operator", false, false);
    let scratch = Scratch::new();
    let path = scratch.0.join("memory.toml");
    std::fs::write(
        &path,
        toml::to_string(&selection(&server.url, true, true).config).unwrap(),
    )
    .unwrap();
    let child = scratch.0.join("workspace");
    std::fs::create_dir(&child).unwrap();
    let client =
        ConfiguredClient::new(mneme_config::select_workspace(&path, &child, None).unwrap())
            .unwrap();
    client
        .call("alpha", "get", json!({"id":NODE}))
        .await
        .unwrap();
    std::fs::create_dir(child.join(".mneme")).unwrap();
    std::fs::write(
        child.join(".mneme/memory.toml"),
        "schema='mneme.config.v1'\n[workspace]\nprofile='isolated'\nallow_stores=[]\n",
    )
    .unwrap();
    assert!(
        client
            .call("alpha", "get", json!({"id":NODE}))
            .await
            .is_err()
    );
    assert_eq!(server.calls.load(Ordering::Relaxed), 1);
    client.close().await;
}
fn catalog(path: &Path, duplicate: bool) {
    let entry = json!({"project_id":"project-hash","db_id":DB,"owner_device_id":"fixture-device","display_name":"fixture","database":"project","revision":1,"replicas":[]});
    std::fs::write(path,serde_json::to_vec(&json!({"schema":"mneme.library.catalog.v1","library_id":"fixture-library","revision":1,"entries":if duplicate {vec![entry.clone(),{let mut other=entry;other["project_id"]=json!("other-project");other}]}else{vec![entry]}})).unwrap()).unwrap();
}
#[test]
fn collection_derives_live_owner_by_native_identity_and_fences_stale_grants() {
    let scratch = Scratch::new();
    let path = scratch.0.join("memory.toml");
    let catalog_path = scratch.0.join("catalog.json");
    catalog(&catalog_path, false);
    let mut config = selection("http://127.0.0.1:1", true, true).config;
    config.libraries.insert(
        "collection".into(),
        mneme_config::Library {
            catalog: catalog_path.clone(),
            replicas: BTreeMap::new(),
            profiles: vec!["default".into()],
        },
    );
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let client =
        ConfiguredClient::new(mneme_config::select_workspace(&path, &scratch.0, None).unwrap())
            .unwrap();
    let runtime = client.library("collection").unwrap();
    let sources = runtime.live_sources().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].0.db_id, DB);
    assert_eq!(sources[0].1.url, "http://127.0.0.1:1");
    config.stores.get_mut("alpha").unwrap().permissions.read = false;
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    assert!(runtime.known_sources().is_err());
}
#[test]
fn collection_refuses_ambiguous_native_id_mapping() {
    let scratch = Scratch::new();
    let catalog_path = scratch.0.join("catalog.json");
    catalog(&catalog_path, true);
    let mut config = selection("http://127.0.0.1:1", true, true).config;
    config.libraries.insert(
        "collection".into(),
        mneme_config::Library {
            catalog: catalog_path.clone(),
            replicas: BTreeMap::new(),
            profiles: vec!["default".into()],
        },
    );
    let snapshot = mneme_config::select(
        &config,
        &SelectionContext::new(std::env::current_dir().unwrap()),
    )
    .unwrap();
    let client = ConfiguredClient::new(snapshot).unwrap();
    assert!(client.library("collection").is_err());
}
#[test]
fn inactive_explicit_route_does_not_change_automatic_selection() {
    let mut config = selection("http://127.0.0.1:1", true, true).config;
    config.read_default = "project-or-misc".into();
    config.stores.get_mut("alpha").unwrap().active.roots = vec!["/unrelated-fixture-root".into()];
    let snapshot = mneme_config::select(
        &config,
        &SelectionContext::new(std::env::current_dir().unwrap()),
    )
    .unwrap();
    assert!(snapshot.active.is_empty());
    assert!(
        ConfiguredClient::new(snapshot.clone())
            .unwrap()
            .selected("alpha")
            .is_err()
    );
    let client = ConfiguredClient::for_explicit(snapshot, "alpha").unwrap();
    assert_eq!(client.selected("alpha").unwrap().name, "alpha");
    assert!(client.selection().active.is_empty());
}

#[tokio::test]
async fn configured_catalog_dispatch_profile_matrix_matches_early_ceiling_and_native_actions() {
    for (profile, rank) in [
        ("read-only", 0),
        ("receipt-grounded", 1),
        ("curator", 2),
        ("operator", 3),
    ] {
        for (read, write) in [(false, false), (true, false), (false, true), (true, true)] {
            let server = fixture(profile, false, false);
            let client = ConfiguredClient::new(selection(&server.url, read, write)).unwrap();
            let tools = client.catalog("alpha").await;
            if !read && !write {
                assert!(tools.is_err());
            } else {
                let tools = tools.unwrap();
                let present = |name: &str| tools.iter().any(|tool| tool["name"] == name);
                assert_eq!(present("get"), read, "{profile} read={read} write={write}");
                assert_eq!(present("forget"), write && rank == 3);
                let actions = tools
                    .iter()
                    .find(|tool| tool["name"] == "episode")
                    .and_then(|tool| tool.pointer("/inputSchema/properties/action/enum"))
                    .and_then(Value::as_array);
                let action =
                    |name: &str| actions.is_some_and(|actions| actions.contains(&json!(name)));
                assert_eq!(action("list"), read);
                assert_eq!(action("append"), write && rank >= 2);
                assert_eq!(action("revise"), write && rank == 3);
            }
            for (tool, args, allowed) in [
                ("get", json!({"id":NODE}), read),
                ("forget", json!({"id":NODE}), write && rank == 3),
                ("episode", json!({"action":"list"}), read),
                (
                    "episode",
                    json!({"action":"append","source":{"namespace":"fixture","key":"matrix","reference":"fixture:matrix"},"summary":"fixture"}),
                    write && rank >= 2,
                ),
            ] {
                let before = server.calls.load(Ordering::Relaxed);
                let result = client.call("alpha", tool, args).await;
                assert_eq!(
                    result.is_ok(),
                    allowed,
                    "{profile} read={read} write={write} {tool}: {result:?}"
                );
                assert_eq!(
                    server.calls.load(Ordering::Relaxed) - before,
                    usize::from(allowed),
                    "denied payload reached owner"
                );
            }
            client.close().await;
        }
    }
}

#[test]
fn every_native_operation_has_an_explicit_capability_matrix() {
    let rows = &[
        ("databases", 0),
        ("status", 0),
        ("core", 0),
        ("query", 0),
        ("recall", 0),
        ("recall_context", 0),
        ("get", 0),
        ("list", 0),
        ("neighbors", 0),
        ("graph", 0),
        ("activity", 0),
        ("save", 2),
        ("capture", 2),
        ("retag", 2),
        ("edit_body", 2),
        ("edit_summary", 2),
        ("episode", 0),
        ("link", 2),
        ("supersede", 3),
        ("forget", 3),
        ("concern", 0),
        ("walk", 0),
        ("reflect", 1),
        ("remote_edges", 0),
        ("contradictions", 0),
        ("merges", 0),
        ("ingest", 2),
        ("contradict", 2),
        ("reconcile", 3),
        ("feedback", 3),
        ("merge", 3),
        ("decay", 3),
        ("prune", 3),
        ("snapshot_create", 3),
    ];
    assert_eq!(rows.len(), ORDINARY_TOOLS.len());
    for &(tool, minimum) in rows {
        assert!(ORDINARY_TOOLS.contains(&tool));
        let args = match tool {
            "episode" => json!({"action":"list"}),
            "concern" => json!({"action":"list"}),
            "walk" => json!({"action":"start"}),
            "graph" => json!({"action":"topology"}),
            _ => json!({}),
        };
        let access = request_access(tool, &args).unwrap();
        assert_eq!(access == RequestAccess::Read, minimum == 0);
        for profile in 0..=3 {
            assert_eq!(
                required_profile(tool, &args, access) <= profile,
                minimum <= profile,
                "{tool} profile={profile}"
            );
        }
    }
    for (tool, action, minimum) in [
        ("episode", "append", 2),
        ("episode", "revise", 3),
        ("concern", "notice", 2),
        ("concern", "record_finding", 2),
    ] {
        let args = json!({"action":action});
        let access = request_access(tool, &args).unwrap();
        assert_eq!(access, RequestAccess::Write);
        for profile in 0..=3 {
            assert_eq!(
                required_profile(tool, &args, access) <= profile,
                minimum <= profile
            );
        }
    }
}

#[tokio::test]
async fn separate_clients_share_small_read_budget_without_mutation_catalog() {
    let server = fixture("operator", false, false);
    let mut budget = OperationBudget::new(4096, Duration::from_secs(2)).unwrap();
    for _ in 0..3 {
        let client = ConfiguredClient::new(selection(&server.url, true, true)).unwrap();
        client
            .call_with_budget("alpha", "get", json!({"id":NODE}), &mut budget)
            .await
            .unwrap();
        client.close().await;
    }
    assert_eq!(server.calls.load(Ordering::Relaxed), 3);
    assert!(budget.bytes_used() < 4096);
    // The fixture advertises a complete episode/mutation catalog, which does
    // not fit this unchanged allowance. A write must discover it and refuse
    // before sending the mutation, rather than use baseline read dispatch.
    let client = ConfiguredClient::new(selection(&server.url, true, true)).unwrap();
    let before = server.calls.load(Ordering::Relaxed);
    assert!(
        client
            .call_with_budget("alpha", "forget", json!({"id":NODE}), &mut budget)
            .await
            .is_err()
    );
    assert_eq!(server.calls.load(Ordering::Relaxed), before);
    client.close().await;
}

#[test]
fn checked_content_authority_matches_core_membership_not_tag_presence() {
    for (expected, replacement, minimum) in [
        (json!([]), json!(["fact"]), 2),
        (json!(["core"]), json!(["core", "fact"]), 2),
        (json!(["core", "fact"]), json!(["core"]), 2),
        (json!(["core"]), json!(["core"]), 2),
        (json!([]), json!(["core"]), 3),
        (json!([]), json!([" Core "]), 3),
        (json!(["core"]), json!([]), 3),
    ] {
        let args = json!({"id":NODE,"expected_tags":expected,"tags":replacement});
        let access = request_access("retag", &args).unwrap();
        let checked = admission::prepare("retag", &args, access).unwrap();
        assert_eq!(
            required_profile("retag", &args, access).max(if checked.operator() { 3 } else { 0 }),
            minimum
        );
    }
    for (tool, args) in [
        (
            "edit_body",
            json!({"id":NODE,"expected_body_revision":"a".repeat(64),"body":"replacement"}),
        ),
        (
            "edit_summary",
            json!({"id":NODE,"expected_snapshot_sha256":"a".repeat(64),"summary":"replacement"}),
        ),
    ] {
        let access = request_access(tool, &args).unwrap();
        let checked = admission::prepare(tool, &args, access).unwrap();
        assert!(!checked.operator());
        assert_eq!(required_profile(tool, &args, access), 2);
    }
    // ASCII casing and separator trimming cannot hide protected membership.
    for tool in ["save", "capture", "ingest"] {
        assert_eq!(
            required_profile(tool, &json!({"tags":[" Core "]}), RequestAccess::Write),
            3
        );
    }
    // The generic core check is creation-only, not ambient write authority.
    for tool in ["save", "capture", "ingest"] {
        assert_eq!(
            required_profile(tool, &json!({"tags":["core"]}), RequestAccess::Write),
            3
        );
    }
}

#[tokio::test]
async fn configured_content_edit_profile_parity_denies_before_operation_send() {
    for (profile, rank) in [
        ("read-only", 0),
        ("receipt-grounded", 1),
        ("curator", 2),
        ("operator", 3),
    ] {
        let server = fixture(profile, false, false);
        let client = ConfiguredClient::new(selection(&server.url, true, true)).unwrap();
        let catalog = client.catalog("alpha").await.unwrap();
        for tool in ["retag", "edit_body", "edit_summary"] {
            assert_eq!(
                catalog.iter().any(|schema| schema["name"] == tool),
                rank >= 2
            );
        }
        for (tool, args, minimum) in [
            (
                "retag",
                json!({"id":NODE,"expected_tags":[],"tags":["fact"]}),
                2,
            ),
            (
                "retag",
                json!({"id":NODE,"expected_tags":["core"],"tags":["core","fact"]}),
                2,
            ),
            (
                "retag",
                json!({"id":NODE,"expected_tags":[],"tags":["core"]}),
                3,
            ),
            (
                "retag",
                json!({"id":NODE,"expected_tags":["core"],"tags":[]}),
                3,
            ),
            (
                "edit_body",
                json!({"id":NODE,"expected_body_revision":"a".repeat(64),"body":"replacement"}),
                2,
            ),
            (
                "edit_summary",
                json!({"id":NODE,"expected_snapshot_sha256":"a".repeat(64),"summary":"replacement"}),
                2,
            ),
            ("forget", json!({"id":NODE}), 3),
        ] {
            let before = server.calls.load(Ordering::Relaxed);
            let result = client.call("alpha", tool, args).await;
            assert_eq!(
                result.is_ok(),
                rank >= minimum,
                "{profile} {tool}: {result:?}"
            );
            assert_eq!(
                server.calls.load(Ordering::Relaxed) - before,
                usize::from(rank >= minimum),
                "denied edit reached owner"
            );
        }
        let before = server.calls.load(Ordering::Relaxed);
        assert!(
            client
                .call(
                    "alpha",
                    "retag",
                    json!({"id":NODE,"expected_tags":["core","core"],"tags":[]})
                )
                .await
                .is_err()
        );
        assert_eq!(server.calls.load(Ordering::Relaxed), before);
        client.close().await;
    }
}

#[tokio::test]
async fn optional_core_notice_uses_initialize_without_catalog_io() {
    let notice = json!({"status":"complete","open_found":2,"rows_scanned":3});
    for feature in [
        None,
        Some(json!(false)),
        Some(json!("true")),
        Some(json!(1)),
        Some(json!({})),
        Some(json!(true)),
    ] {
        let supported = feature == Some(json!(true));
        let server = fixture_with_core(
            "read-only",
            false,
            false,
            CoreFixture {
                feature,
                notice: Some(notice.clone()),
                ..CoreFixture::default()
            },
        );
        let client = ConfiguredClient::new(selection(&server.url, true, false)).unwrap();
        let mut budget = OperationBudget::new(4096, Duration::from_secs(2)).unwrap();
        let result = client
            .call_with_budget(
                "alpha",
                "core",
                json!({"include_contradictions":true}),
                &mut budget,
            )
            .await
            .unwrap();
        assert_eq!(result["nodes"][0]["id"], NODE);
        assert_eq!(
            result["contradiction_summary"],
            if supported {
                notice.clone()
            } else {
                json!({"status":"unavailable"})
            }
        );
        assert_eq!(server.catalog_calls.load(Ordering::Relaxed), 0);
        assert_eq!(server.calls.load(Ordering::Relaxed), 1);
        let requests = server.requests.lock().unwrap().clone();
        let args = &requests
            .iter()
            .find(|request| request["name"] == "core")
            .unwrap()["arguments"];
        assert_eq!(
            args.get("include_contradictions"),
            supported.then_some(&json!(true))
        );
        assert_eq!(args["db"], "project");
        assert_eq!(args["expected_db_id"], DB);
        client.close().await;
    }
}

#[tokio::test]
async fn default_core_read_omits_optional_field_and_result() {
    let server = fixture_with_core(
        "read-only",
        false,
        false,
        CoreFixture {
            feature: Some(json!(true)),
            notice: Some(json!({"status":"complete","open_found":1,"rows_scanned":1})),
            ..CoreFixture::default()
        },
    );
    let client = ConfiguredClient::new(selection(&server.url, true, false)).unwrap();
    for arguments in [json!({}), json!({"include_contradictions":false})] {
        let result = client.call("alpha", "core", arguments).await.unwrap();
        assert!(result.get("contradiction_summary").is_none());
        assert_eq!(result["nodes"][0]["id"], NODE);
    }
    assert_eq!(server.catalog_calls.load(Ordering::Relaxed), 0);
    for request in server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["name"] == "core")
    {
        assert!(request["arguments"].get("include_contradictions").is_none());
    }
    client.close().await;
}

#[tokio::test]
async fn optional_core_notice_can_use_an_already_loaded_catalog() {
    for field in [
        None,
        Some(json!({"type":"string"})),
        Some(json!({"type":"boolean"})),
    ] {
        let supported = field == Some(json!({"type":"boolean"}));
        let server = fixture_with_core(
            "read-only",
            false,
            false,
            CoreFixture {
                catalog_field: field,
                notice: Some(json!({"status":"partial","open_found":1,"rows_scanned":256})),
                ..CoreFixture::default()
            },
        );
        let client = ConfiguredClient::new(selection(&server.url, true, false)).unwrap();
        client.catalog("alpha").await.unwrap();
        let result = client
            .call("alpha", "core", json!({"include_contradictions":true}))
            .await
            .unwrap();
        assert_eq!(result["nodes"][0]["id"], NODE);
        assert_eq!(
            result["contradiction_summary"]["status"],
            if supported { "partial" } else { "unavailable" }
        );
        assert_eq!(server.catalog_calls.load(Ordering::Relaxed), 1);
        let requests = server.requests.lock().unwrap().clone();
        let args = &requests
            .iter()
            .find(|request| request["name"] == "core")
            .unwrap()["arguments"];
        assert_eq!(
            args.get("include_contradictions"),
            supported.then_some(&json!(true))
        );
        client.close().await;
    }
}

#[tokio::test]
async fn optional_core_notice_failures_do_not_erase_cards_or_bypass_identity() {
    for notice in [
        None,
        Some(Value::Null),
        Some(json!({"status":"complete","open_found":2,"rows_scanned":1})),
        Some(json!({"status":"unavailable","open_found":0})),
    ] {
        let server = fixture_with_core(
            "read-only",
            false,
            false,
            CoreFixture {
                feature: Some(json!(true)),
                notice,
                ..CoreFixture::default()
            },
        );
        let client = ConfiguredClient::new(selection(&server.url, true, false)).unwrap();
        let result = client
            .call("alpha", "core", json!({"include_contradictions":true}))
            .await
            .unwrap();
        assert_eq!(
            result["contradiction_summary"],
            json!({"status":"unavailable"})
        );
        assert_eq!(result["nodes"][0]["id"], NODE);
        assert_eq!(server.calls.load(Ordering::Relaxed), 1);
        client.close().await;
    }
    for wrong_handshake in [false, true] {
        let server = fixture_with_core(
            "read-only",
            wrong_handshake,
            false,
            CoreFixture {
                feature: Some(json!(true)),
                wrong_result_identity: true,
                ..CoreFixture::default()
            },
        );
        let client = ConfiguredClient::new(selection(&server.url, true, false)).unwrap();
        assert!(
            client
                .call("alpha", "core", json!({"include_contradictions":true}))
                .await
                .is_err()
        );
        assert_eq!(
            server.calls.load(Ordering::Relaxed),
            usize::from(!wrong_handshake)
        );
        assert_eq!(server.catalog_calls.load(Ordering::Relaxed), 0);
        client.close().await;
    }
}
