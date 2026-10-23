//! The local diagnostic must never become an owner or forwarding operation.

use super::*;
#[cfg(feature = "http")]
use std::path::Path;

const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "mneme-session-activity-surfaces-{}",
            ulid::Ulid::new()
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn write_json(&self, name: &str, value: Value) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        path
    }

    fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        std::fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assert_hidden(catalog: &Value) {
    assert!(
        catalog["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["name"] != "session_activity"),
        "{catalog}"
    );
}

fn calls() -> [Value; 4] {
    [
        json!({"name":"session_activity","arguments":{}}),
        json!({"name":"session_activity","arguments":{"session":"codex:fixture","details":true}}),
        json!({"name":"session_activity","arguments":{"db":"memory","session":"codex:fixture"}}),
        json!({"name":"session_activity","arguments":{"path":"/unadmitted","scope":"global"}}),
    ]
}

#[tokio::test]
async fn session_activity_is_hidden_and_denied_in_every_owner_profile() {
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        let server = crate::tests::empty_profile_server(profile);
        let catalog = server.dispatch("tools/list", json!({})).await.unwrap();
        assert_hidden(&catalog);
        assert!(
            catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "activity")
        );
        for params in calls() {
            let response = server.dispatch("tools/call", params).await.unwrap();
            assert_eq!(response["isError"], true, "{profile:?}: {response}");
            assert!(response.to_string().contains("unknown tool"), "{response}");
        }
        // The unrelated, process-local visualization feed retains its own API.
        let activity = server
            .dispatch("tools/call", json!({"name":"activity","arguments":{}}))
            .await
            .unwrap();
        assert_eq!(activity["isError"], false);
        let gate = server.cold_work.inner.state.lock().unwrap();
        assert!(gate.in_flight.is_none());
        assert!(gate.next_admission.is_none());
        assert!(server.registry.dbs.is_empty());
    }
    assert!(ValidatedToolArguments::parse("session_activity", &json!({})).is_err());
    assert!(!router_catalog::TOOLS.contains(&"session_activity"));
}

fn library_config(scratch: &Scratch) -> PathBuf {
    scratch.write_json(
        "catalog.json",
        json!({
            "schema":"mneme.library.catalog.v1", "library_id":"fixture", "revision":1, "entries":[]
        }),
    );
    scratch.write_json(
        "library.json",
        json!({
            "schema":"mneme.library.config.v1", "library_id":"fixture", "device_id":"fixture",
            "catalog_path":"catalog.json", "rerank":false
        }),
    )
}

#[tokio::test]
async fn session_activity_is_hidden_and_denied_in_library_without_writes() {
    let scratch = Scratch::new();
    let config = library_config(&scratch);
    let before = scratch.files();
    let server = library_server::LibraryServer::from_path(&config).unwrap();
    assert_hidden(&server.dispatch("tools/list", json!({})).await.unwrap());
    for params in calls() {
        let response = server.dispatch("tools/call", params).await.unwrap();
        assert_eq!(response["isError"], true);
        assert!(response.to_string().contains("unknown library tool"));
    }
    assert_eq!(scratch.files(), before);
}

fn configured_path(scratch: &Scratch, url: &str, readable: bool) -> PathBuf {
    let path = scratch.0.join("memory.toml");
    std::fs::write(&path, format!(
        "schema='mneme.config.v1'\n[stores.memory]\nid='{ID}'\nowner={{url='{url}',db='user'}}\nactive={{workspace='any'}}\npermissions={{read={readable},write=false}}\n"
    )).unwrap();
    path
}

#[tokio::test]
async fn session_activity_is_hidden_and_denied_in_configured_without_owner_contact() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let scratch = Scratch::new();
    let config = configured_path(
        &scratch,
        &format!("http://{}/", listener.local_addr().unwrap()),
        false,
    );
    let before = scratch.files();
    let server = configured_server::ConfiguredServer::from_path(&config, &scratch.0)
        .await
        .unwrap();
    assert_hidden(&server.dispatch("tools/list", json!({})).await.unwrap());
    for params in calls() {
        let response = server.dispatch("tools/call", params).await.unwrap();
        assert_eq!(response["isError"], true);
        assert!(response.to_string().contains("unknown configured MCP tool"));
    }
    server.close().await;
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(scratch.files(), before);
}

#[cfg(feature = "http")]
mod forwarding {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderValue, StatusCode},
        response::{IntoResponse, Response},
        routing::post,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct MockState {
        requests: AtomicUsize,
        catalog: Vec<Value>,
        databases: Value,
    }
    struct Mock {
        state: Arc<MockState>,
        url: String,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl Mock {
        async fn new(replica: bool) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let mut catalog =
                tool_schemas(CapabilityPolicy::new(CapabilityProfile::ReadOnly, false));
            // Even an upstream incorrectly advertising this local tool cannot
            // widen either coordinator's closed forwarding catalog.
            catalog.push(json!({"name":"session_activity","description":"must not forward","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}));
            let databases = if replica {
                json!([{"name":"mneme_G1","db":"mneme_G1","db_id":ID,"state":"open","resolved_path":"/fixture/G1/store.db"}])
            } else {
                json!([{"name":"user","db":"user","db_id":ID,"state":"open"}])
            };
            let state = Arc::new(MockState {
                requests: AtomicUsize::new(0),
                catalog,
                databases,
            });
            let app = Router::new()
                .route("/", post(rpc).delete(close))
                .with_state(state.clone());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self { state, url, task }
        }
        fn count(&self) -> usize {
            self.state.requests.load(Ordering::SeqCst)
        }
    }
    async fn close(State(state): State<Arc<MockState>>) -> StatusCode {
        state.requests.fetch_add(1, Ordering::SeqCst);
        StatusCode::OK
    }
    async fn rpc(State(state): State<Arc<MockState>>, Json(request): Json<Value>) -> Response {
        state.requests.fetch_add(1, Ordering::SeqCst);
        let Some(id) = request.get("id") else {
            return StatusCode::ACCEPTED.into_response();
        };
        let result = match request["method"].as_str().unwrap() {
            "initialize" => {
                json!({"protocolVersion":PROTOCOL_VERSION,"capabilities":{"tools":{}},"serverInfo":{"name":"mneme-mcp","version":"fixture"},"capabilityProfile":"read-only"})
            }
            "tools/list" => json!({"tools":state.catalog}),
            "tools/call" if request["params"]["name"] == "databases" => {
                response::tool_success(&state.databases)
            }
            _ => panic!("unexpected forwarded operation: {request}"),
        };
        let mut reply = Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response();
        if request["method"] == "initialize" {
            reply
                .headers_mut()
                .insert("mcp-session-id", HeaderValue::from_static("fixture"));
        }
        reply
    }
    async fn router(path: &Path) -> router_server::RouterServer {
        tokio::time::timeout(
            Duration::from_secs(5),
            router_server::RouterServer::from_path(path),
        )
        .await
        .unwrap()
        .unwrap()
    }

    #[tokio::test]
    async fn session_activity_is_hidden_and_denied_in_active_configured_without_forwarding() {
        let mock = Mock::new(false).await;
        let scratch = Scratch::new();
        let config = configured_path(&scratch, &mock.url, true);
        let before = scratch.files();
        let server = configured_server::ConfiguredServer::from_path(&config, &scratch.0)
            .await
            .unwrap();
        let catalog = tokio::time::timeout(
            Duration::from_secs(5),
            server.dispatch("tools/list", json!({})),
        )
        .await
        .unwrap()
        .unwrap();
        assert_hidden(&catalog);
        assert_eq!(catalog["omissions"], json!([]), "{catalog}");
        assert!(
            catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "get")
        );
        let requests = mock.count();
        assert!(requests > 0);
        for params in calls() {
            let response = server.dispatch("tools/call", params).await.unwrap();
            assert_eq!(response["isError"], true);
            assert!(response.to_string().contains("unknown configured MCP tool"));
        }
        assert_eq!(mock.count(), requests);
        assert_eq!(scratch.files(), before);
        server.close().await;
    }

    #[tokio::test]
    async fn session_activity_is_hidden_and_denied_in_router_owner_and_replica_without_forwarding()
    {
        for replica in [false, true] {
            let mock = Mock::new(replica).await;
            let scratch = Scratch::new();
            let database = if replica {
                scratch.write_json("library.json", json!({"schema":"mneme.library.config.v1","library_id":"fixture","device_id":"fixture","catalog_path":"catalog.json","replicas":{"fixture":{"url":mock.url}},"rerank":false}));
                scratch.write_json("catalog.json", json!({"schema":"mneme.library.catalog.v1","library_id":"fixture","revision":1,"entries":[{"project_id":"workspace","db_id":ID,"owner_device_id":"fixture","display_name":"Fixture","database":"project","revision":1,"replicas":[{"source_device_id":"fixture","database":"mneme_G1","resolved_path":"/fixture/G1/store.db","generation":"G1","captured_at":1}]}]}));
                json!({"kind":"replica","name":"memory","library_config":"library.json","project_id":"workspace","expected_db_id":ID})
            } else {
                json!({"kind":"owner","name":"memory","url":mock.url,"db":"user","expected_db_id":ID})
            };
            let config = scratch.write_json("router.json", json!({"schema":"mneme.router.config.v1","default_read_db":"memory","databases":[database]}));
            let before = scratch.files();
            let server = router(&config).await;
            assert_hidden(&server.dispatch("tools/list", json!({})).await.unwrap());
            let requests = mock.count();
            for params in calls() {
                let response = server.dispatch("tools/call", params).await.unwrap();
                assert_eq!(response["isError"], true);
                assert!(response.to_string().contains("unknown router tool"));
            }
            assert_eq!(mock.count(), requests);
            assert_eq!(scratch.files(), before);
            server.close().await;
        }
    }
}
