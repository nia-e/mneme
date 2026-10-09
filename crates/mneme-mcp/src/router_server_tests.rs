use super::*;
const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
fn owner(name: &str, port: u16, id: &str) -> DatabaseConfig {
    DatabaseConfig::Owner {
        name: name.into(),
        url: format!("http://127.0.0.1:{port}/"),
        db: "user".into(),
        expected_db_id: id.into(),
        token_env: None,
    }
}
fn config(databases: Vec<DatabaseConfig>, default: &str) -> RouterConfig {
    RouterConfig {
        schema: "mneme.router.config.v1".into(),
        default_read_db: default.into(),
        databases,
    }
}

#[test]
fn router_startup_mode_is_explicit_and_exclusive() {
    let args = crate::parse_args_from(["--router-config", "/not/a/database"]).unwrap();
    assert!(args.dbs.is_empty());
    assert!(args.library_config.is_none());
    for sibling in [
        vec!["--db", "user=/tmp/owner"],
        vec!["--library-config", "library.json"],
        vec!["--capability-profile", "operator"],
        vec!["--allow-direct-feedback"],
        vec!["--router-config", "again.json"],
    ] {
        let mut args = vec!["--router-config", "router.json"];
        args.extend(sibling);
        assert!(crate::parse_args_from(args).is_err());
    }
}
#[test]
fn config_is_closed_and_loopback_only() {
    for url in [
        "https://127.0.0.1:12/",
        "http://localhost:12/",
        "http://192.0.2.1:12/",
        "http://127.0.0.1/",
        "ssh://host",
        "http://u:p@127.0.0.1:12/",
        "http://127.0.0.1:12/?x=1",
    ] {
        assert!(validate_endpoint(url).is_err(), "{url}");
    }
    for path in ["/", "/mcp", "/another/path"] {
        validate_endpoint(&format!("http://127.0.0.1:12{path}")).unwrap();
    }
    assert!(serde_json::from_value::<RouterConfig>(json!({"schema":"mneme.router.config.v1","default_read_db":"misc","databases":[],"owners":[]})).is_err());
    assert!(serde_json::from_value::<DatabaseConfig>(json!({"kind":"owner","name":"user","url":"http://127.0.0.1:1/","db":"user","expected_db_id":ID,"unexpected":true})).is_err());
    for bad in [
        json!({"source_device_id":"pi","generation":"G1","captured_at":null}),
        json!({"source_device_id":"pi","generation":"G1","captured_at":-1}),
        json!({"source_device_id":"pi","generation":"G1","extra":true}),
    ] {
        assert!(serde_json::from_value::<Snapshot>(bad).is_err());
    }
}
#[test]
fn response_wrapper_preserves_domain_and_errors_without_rewriting_authored_db() {
    let raw = json!({"content":[{"type":"text","text":"{\"db\":\"native\",\"nested\":{\"db\":\"authored\"}}"}],"isError":false,"_meta":{"custom":42}});
    let result = response::wrap(raw.clone(), "misc", None).unwrap();
    assert_eq!(result["_meta"], raw["_meta"]);
    let domain: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        domain,
        json!({"db":"misc","result":{"db":"native","nested":{"db":"authored"}}})
    );
    let raw = json!({"content":[{"type":"text","text":"native refusal"}],"isError":true,"_meta":{"error_code":"native"},"structuredContent":{"error":"native"}});
    let wrapped = response::wrap(raw.clone(), "misc", None).unwrap();
    assert_eq!(wrapped["isError"], true);
    assert_eq!(wrapped["_meta"], raw["_meta"]);
    assert_eq!(
        wrapped["structuredContent"],
        json!({"db":"misc","result":{"error":"native"}})
    );
    for unsupported in [
        json!({"content":[{"type":"text","text":"not JSON"}],"isError":false}),
        json!({"content":[{"type":"text","text":"{}"},{"type":"text","text":"secret"}]}),
        json!({"content":[{"type":"image","data":"x"}]}),
    ] {
        assert!(response::wrap(unsupported, "misc", None).is_err());
    }
}
#[test]
fn catalog_has_one_tool_per_operation_and_groups_equivalent_owner_schemas() {
    let tools = crate::tool_schemas(CapabilityPolicy::new(CapabilityProfile::Operator, false))
        .into_iter()
        .filter(|tool| catalog::TOOLS.contains(&tool["name"].as_str().unwrap()))
        .collect::<Vec<_>>();
    let routes = [
        catalog::RouteCatalog {
            name: "user",
            replica: false,
            tools: &tools,
        },
        catalog::RouteCatalog {
            name: "misc",
            replica: false,
            tools: &tools,
        },
    ];
    let projected = catalog::build(&routes, "misc").unwrap();
    assert_eq!(projected.len(), catalog::TOOLS.len() + 1);
    let get = projected.iter().find(|tool| tool["name"] == "get").unwrap();
    assert_eq!(
        get["inputSchema"]["properties"]["db"]["enum"],
        json!(["user", "misc"])
    );
    assert_eq!(get["inputSchema"]["properties"]["edges"]["const"], false);
    let episode = projected
        .iter()
        .find(|tool| tool["name"] == "episode")
        .unwrap();
    for branch in episode["inputSchema"]["oneOf"].as_array().unwrap() {
        assert_eq!(branch["properties"]["db"]["enum"], json!(["user", "misc"]));
        if matches!(
            branch["properties"]["action"]["const"].as_str(),
            Some("append" | "revise")
        ) {
            assert!(
                branch["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|field| field == "db")
            );
        }
    }
    let list = projected
        .iter()
        .find(|tool| tool["name"] == "list")
        .unwrap();
    for branch in list["inputSchema"]["oneOf"].as_array().unwrap() {
        assert_eq!(branch["properties"]["db"]["enum"], json!(["user", "misc"]));
    }
    let link = projected
        .iter()
        .find(|tool| tool["name"] == "link")
        .unwrap();
    assert!(!link.to_string().contains("to_db"));
    assert_eq!(link["inputSchema"]["oneOf"].as_array().unwrap().len(), 1);
    let native_save = tools.iter().find(|tool| tool["name"] == "save").unwrap();
    let save = projected
        .iter()
        .find(|tool| tool["name"] == "save")
        .unwrap();
    assert_eq!(
        save["inputSchema"]["properties"]["source"],
        native_save["inputSchema"]["properties"]["source"]
    );
}

#[cfg(feature = "http")]
mod http_tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, HeaderValue, StatusCode},
        response::{IntoResponse, Response},
        routing::post,
    };
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Duration;
    struct MockState {
        profile: CapabilityProfile,
        catalog: Vec<Value>,
        databases: StdMutex<Vec<Value>>,
        calls: StdMutex<Vec<(Value, String)>>,
        verification_sessions: StdMutex<Vec<String>>,
        response: StdMutex<Option<Value>>,
        fail_next: AtomicBool,
        delay_next: AtomicBool,
        deletes: AtomicUsize,
        initializes: AtomicUsize,
    }
    struct Mock {
        state: Arc<MockState>,
        port: u16,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl Mock {
        async fn new(id: &str, profile: CapabilityProfile) -> Self {
            Self::with_catalog(
                id,
                profile,
                crate::tool_schemas(CapabilityPolicy::new(profile, false)),
            )
            .await
        }
        async fn with_catalog(id: &str, profile: CapabilityProfile, catalog: Vec<Value>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let state = Arc::new(MockState {
                profile,
                catalog,
                databases: StdMutex::new(vec![
                    json!({"name":"user","db":"user","db_id":id,"state":"open","configured_path":"secret/path","resolved_path":"/secret/path"}),
                ]),
                calls: StdMutex::new(vec![]),
                verification_sessions: StdMutex::new(vec![]),
                response: StdMutex::new(None),
                fail_next: AtomicBool::new(false),
                delay_next: AtomicBool::new(false),
                deletes: AtomicUsize::new(0),
                initializes: AtomicUsize::new(0),
            });
            let app = Router::new()
                .route("/", post(rpc).delete(delete))
                .with_state(state.clone());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self { state, port, task }
        }
        fn wire_count(&self) -> usize {
            self.state.calls.lock().unwrap().len()
                + self.state.verification_sessions.lock().unwrap().len()
        }
    }
    async fn delete(State(state): State<Arc<MockState>>) -> StatusCode {
        state.deletes.fetch_add(1, Ordering::SeqCst);
        StatusCode::OK
    }
    async fn rpc(
        State(state): State<Arc<MockState>>,
        headers: HeaderMap,
        Json(request): Json<Value>,
    ) -> Response {
        let Some(id) = request.get("id") else {
            return StatusCode::ACCEPTED.into_response();
        };
        let session = headers
            .get("mcp-session-id")
            .and_then(|header| header.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let result = match request["method"].as_str().unwrap() {
            "initialize" => {
                state.initializes.fetch_add(1, Ordering::SeqCst);
                json!({"protocolVersion":crate::PROTOCOL_VERSION,"capabilities":{"tools":{}},"serverInfo":{"name":"mneme-mcp","version":"test"},"capabilityProfile":state.profile.as_str()})
            }
            "tools/list" => json!({"tools":state.catalog}),
            "tools/call" if request["params"]["name"] == "databases" => {
                state.verification_sessions.lock().unwrap().push(session);
                crate::response::tool_success(&json!(*state.databases.lock().unwrap()))
            }
            "tools/call" => {
                state
                    .calls
                    .lock()
                    .unwrap()
                    .push((request["params"].clone(), session));
                if state.fail_next.swap(false, Ordering::SeqCst) {
                    return StatusCode::BAD_GATEWAY.into_response();
                }
                if state.delay_next.swap(false, Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                if request["params"]["name"] == "edit_body"
                    && state.response.lock().unwrap().is_none()
                {
                    let args = &request["params"]["arguments"];
                    return Json(json!({"jsonrpc":"2.0","id":id,"result":crate::response::tool_success(&json!({"id":args["id"],"body_revision":"b".repeat(64),"db":args["db"],"db_id":args["expected_db_id"]}))})).into_response();
                }
                if request["params"]["name"] == "edit_summary"
                    && state.response.lock().unwrap().is_none()
                {
                    let args = &request["params"]["arguments"];
                    return Json(json!({"jsonrpc":"2.0","id":id,"result":crate::response::tool_success(&json!({"id":args["id"],"summary_snapshot_sha256":"b".repeat(64),"db":args["db"],"db_id":args["expected_db_id"]}))})).into_response();
                }
                if request["params"]["name"] == "retag" && state.response.lock().unwrap().is_none()
                {
                    let args = &request["params"]["arguments"];
                    return Json(json!({"jsonrpc":"2.0","id":id,"result":crate::response::tool_success(&json!({"id":args["id"],"tags":args["tags"],"changed":args["expected_tags"] != args["tags"],"db":args["db"],"db_id":args["expected_db_id"]}))})).into_response();
                }
                state.response.lock().unwrap().clone().unwrap_or_else(||crate::response::tool_success(&json!({"native_db":request["params"]["arguments"]["db"],"nested":{"db":"authored"},"cursor":"unchanged"})))
            }
            _ => panic!("unexpected request {request}"),
        };
        let mut response = Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response();
        if request["method"] == "initialize" {
            response.headers_mut().insert(
                "mcp-session-id",
                HeaderValue::from_str(&format!(
                    "mock-{}",
                    state.initializes.load(Ordering::SeqCst)
                ))
                .unwrap(),
            );
        }
        response
    }
    fn timeouts() -> ClientTimeouts {
        ClientTimeouts {
            connect: Duration::from_secs(2),
            request: Duration::from_millis(200),
        }
    }
    async fn server(config: RouterConfig, base: &Path) -> RouterServer {
        RouterServer::from_config(config, base, timeouts())
            .await
            .unwrap()
    }
    async fn call(router: &RouterServer, name: &str, args: Value) -> Value {
        router
            .dispatch(
                "tools/call",
                json!({"name":name,"arguments":args,"_meta":{"caller":"preserved"}}),
            )
            .await
            .unwrap()
    }
    fn body(result: &Value) -> Value {
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }
    async fn wait_calls(mock: &Mock, count: usize) {
        for _ in 0..100 {
            if mock.state.calls.lock().unwrap().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("mock call deadline");
    }
    async fn wait_deletes(mock: &Mock, count: usize) {
        for _ in 0..100 {
            if mock.state.deletes.load(Ordering::SeqCst) >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("session close deadline");
    }

    #[tokio::test]
    async fn default_explicit_reads_and_explicit_mutations_share_one_catalog() {
        let user = Mock::new(ID, CapabilityProfile::Operator).await;
        let misc = Mock::new(OTHER_ID, CapabilityProfile::Curator).await;
        let router = server(
            config(
                vec![
                    owner("user", user.port, ID),
                    owner("misc", misc.port, OTHER_ID),
                ],
                "misc",
            ),
            Path::new("."),
        )
        .await;
        let list = router.dispatch("tools/list", json!({})).await.unwrap();
        let names: HashSet<_> = list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), catalog::TOOLS.len() + 1);
        assert!(names.contains("databases"));
        assert!(
            !names
                .iter()
                .any(|name| name.starts_with("user_") || name.starts_with("misc_"))
        );
        let databases = body(&call(&router, "databases", json!({})).await);
        assert!(databases.is_array());
        assert!(!databases.to_string().contains("secret/path"));
        assert_eq!(
            databases
                .as_array()
                .unwrap()
                .iter()
                .find(|db| db["db"] == "misc")
                .unwrap()["default_read"],
            true
        );
        let result = body(&call(&router, "status", json!({})).await);
        assert_eq!(result["db"], "misc");
        assert_eq!(result["result"]["native_db"], "user");
        assert_eq!(result["result"]["nested"]["db"], "authored");
        assert!(user.state.calls.lock().unwrap().is_empty());
        assert_eq!(
            body(&call(&router, "status", json!({"db":"user","expected_db_id":ID})).await)["db"],
            "user"
        );
        let before = user.wire_count() + misc.wire_count();
        for (tool, args) in [
            ("save", json!({"summary":"note"})),
            ("forget", json!({"id":ID})),
            ("link", json!({"from":ID,"to":OTHER_ID})),
            ("supersede", json!({"winner":ID,"loser":OTHER_ID})),
            (
                "episode",
                json!({"action":"append","summary":"scene","source":{"namespace":"manual","key":"scene","reference":"test:scene"}}),
            ),
        ] {
            let result = call(&router, tool, args).await;
            assert_eq!(result["isError"], true, "{result}");
            assert!(
                body(&result).to_string().contains("explicit db"),
                "{result}"
            );
        }
        for args in [
            json!({"db":"unknown"}),
            json!({"db":null}),
            json!({"db":"misc","expected_db_id":ID}),
            json!({"db":"misc","snapshot":{"source_device_id":"pi","generation":"G1"}}),
        ] {
            assert_eq!(call(&router, "status", args).await["isError"], true);
        }
        assert_eq!(
            call(&router, "get", json!({"db":"user","id":ID,"edges":true})).await["isError"],
            true
        );
        assert_eq!(
            call(
                &router,
                "link",
                json!({"db":"user","from":ID,"to":OTHER_ID,"to_db":"project"})
            )
            .await["isError"],
            true
        );
        for name in [
            "walk",
            "reflect",
            "database_control",
            "capture",
            "query",
            "global_save",
            "library_get",
            "future",
        ] {
            assert_eq!(
                call(&router, name, json!({"db":"user"})).await["isError"],
                true
            );
        }
        assert_eq!(user.wire_count() + misc.wire_count(), before);
        assert_eq!(
            call(
                &router,
                "save",
                json!({"db":"misc","summary":"note","source":{"namespace":"manual","key":"exact","reference":"test:exact"}})
            )
            .await["isError"],
            false
        );
        let forwarded = misc.state.calls.lock().unwrap().last().unwrap().0.clone();
        assert_eq!(forwarded["arguments"]["db"], "user");
        assert_eq!(forwarded["arguments"]["expected_db_id"], OTHER_ID);
        assert_eq!(forwarded["_meta"], json!({"caller":"preserved"}));
        router.close().await;
    }

    #[tokio::test]
    async fn per_database_profile_matrix_denies_hidden_tools_and_actions_before_wire() {
        for profile in [
            CapabilityProfile::ReadOnly,
            CapabilityProfile::ReceiptGrounded,
            CapabilityProfile::Curator,
            CapabilityProfile::Operator,
        ] {
            let mock = Mock::new(ID, profile).await;
            let router = server(
                config(vec![owner("memory", mock.port, ID)], "memory"),
                Path::new("."),
            )
            .await;
            let native = crate::tool_schemas(CapabilityPolicy::new(profile, false));
            let expected: HashSet<_> = native
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .filter(|name| catalog::TOOLS.contains(name))
                .chain(["databases"])
                .collect();
            let list = router.dispatch("tools/list", json!({})).await.unwrap();
            let actual: HashSet<_> = list["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert_eq!(actual, expected);
            let before = mock.wire_count();
            for tool in crate::tool_schemas(CapabilityPolicy::operator_with_direct_feedback()) {
                let name = tool["name"].as_str().unwrap();
                if !expected.contains(name) {
                    assert_eq!(
                        call(&router, name, json!({"db":"memory"})).await["isError"],
                        true
                    );
                }
            }
            for (name, args, allowed) in [
                (
                    "save",
                    json!({"db":"memory","summary":"note"}),
                    matches!(
                        profile,
                        CapabilityProfile::Curator | CapabilityProfile::Operator
                    ),
                ),
                (
                    "retag",
                    json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_tags":[],"tags":["possibility"]}),
                    matches!(
                        profile,
                        CapabilityProfile::Curator | CapabilityProfile::Operator
                    ),
                ),
                (
                    "retag",
                    json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_tags":["core"],"tags":[]}),
                    profile == CapabilityProfile::Operator,
                ),
                (
                    "retag",
                    json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_tags":[],"tags":["core"]}),
                    profile == CapabilityProfile::Operator,
                ),
                (
                    "forget",
                    json!({"db":"memory","id":ID}),
                    profile == CapabilityProfile::Operator,
                ),
                (
                    "save",
                    json!({"db":"memory","summary":"core","tags":["core"]}),
                    profile == CapabilityProfile::Operator,
                ),
                (
                    "episode",
                    json!({"db":"memory","action":"append","summary":"experience","source":{"namespace":"manual","key":"experience","reference":"test:experience"}}),
                    matches!(
                        profile,
                        CapabilityProfile::Curator | CapabilityProfile::Operator
                    ),
                ),
                (
                    "episode",
                    json!({"db":"memory","action":"revise","episode_id":ID,"expected_edition_id":OTHER_ID,"reason":"A corrected account","summary":"revised experience","source":{"namespace":"manual","key":"revision","reference":"test:revision"}}),
                    profile == CapabilityProfile::Operator,
                ),
            ] {
                let count = mock.wire_count();
                let result = call(&router, name, args).await;
                assert_eq!(
                    result["isError"],
                    !allowed,
                    "{} {name}: {result}",
                    profile.as_str()
                );
                if !allowed {
                    assert_eq!(mock.wire_count(), count);
                }
            }
            assert!(mock.wire_count() >= before);
            if matches!(
                profile,
                CapabilityProfile::ReadOnly | CapabilityProfile::ReceiptGrounded
            ) {
                let before = mock.wire_count();
                assert_eq!(
                    call(
                        &router,
                        "episode",
                        json!({"db":"memory","action":"append","summary":"experience","source":{"namespace":"manual","key":"experience","reference":"test:experience"}})
                    )
                    .await["isError"],
                    true
                );
                assert_eq!(mock.wire_count(), before);
            }
            router.close().await;
        }
    }

    #[tokio::test]
    async fn edit_body_routes_once_and_checks_acknowledgement() {
        let mock = Mock::new(ID, CapabilityProfile::Operator).await;
        let router = server(
            config(vec![owner("memory", mock.port, ID)], "memory"),
            Path::new("."),
        )
        .await;
        let args = json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_body_revision":"a".repeat(64),"body":"new"});
        let reply = call(&router, "edit_body", args.clone()).await;
        assert_ne!(reply["isError"], true);
        assert_eq!(body(&reply)["result"]["body_revision"], "b".repeat(64));
        *mock.state.response.lock().unwrap() = Some(crate::response::tool_success(
            &json!({"id":ID,"body_revision":"b".repeat(64),"db":"user","db_id":OTHER_ID}),
        ));
        let reply = call(&router, "edit_body", args).await;
        assert_eq!(reply["isError"], true);
        assert!(body(&reply).to_string().contains("no automatic replay"));
        assert_eq!(mock.state.calls.lock().unwrap().len(), 2);
        router.close().await;
    }

    #[tokio::test]
    async fn edit_body_malformed_and_older_contract_denied_before_owner_work() {
        let mut tools =
            crate::tool_schemas(CapabilityPolicy::new(CapabilityProfile::Operator, false));
        tools.retain(|tool| tool["name"] != "edit_body");
        let old = Mock::with_catalog(ID, CapabilityProfile::Operator, tools).await;
        let router = server(
            config(vec![owner("memory", old.port, ID)], "memory"),
            Path::new("."),
        )
        .await;
        let before = old.wire_count();
        assert_eq!(
            call(
                &router,
                "edit_body",
                json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_body_revision":"a".repeat(64),"body":"new"})
            )
            .await["isError"],
            true
        );
        assert_eq!(old.wire_count(), before);
        router.close().await;

        let mock = Mock::new(ID, CapabilityProfile::Operator).await;
        let router = server(
            config(vec![owner("memory", mock.port, ID)], "memory"),
            Path::new("."),
        )
        .await;
        let before = mock.wire_count();
        for args in [
            json!({"db":"memory","id":ID,"expected_body_revision":"a".repeat(64),"body":"new"}),
            json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_body_revision":"A".repeat(64),"body":"new"}),
            json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_body_revision":"a".repeat(64),"body":null}),
            json!({"db":"memory","expected_db_id":OTHER_ID,"id":ID,"expected_body_revision":"a".repeat(64),"body":"new"}),
            json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_body_revision":"a".repeat(64),"body":"new","snapshot":{"source_device_id":"pi","generation":"G1"}}),
        ] {
            assert_eq!(call(&router, "edit_body", args).await["isError"], true);
        }
        assert_eq!(mock.wire_count(), before);
        mock.state.fail_next.store(true, Ordering::SeqCst);
        let result = call(
            &router,
            "edit_body",
            json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_body_revision":"a".repeat(64),"body":"new"}),
        )
        .await;
        assert!(body(&result).to_string().contains("no automatic replay"));
        assert_eq!(mock.state.calls.lock().unwrap().len(), 1);
        router.close().await;
    }

    #[tokio::test]
    async fn edit_summary_routes_once_and_checks_acknowledgement() {
        let mock = Mock::new(ID, CapabilityProfile::Operator).await;
        let router = server(
            config(vec![owner("memory", mock.port, ID)], "memory"),
            Path::new("."),
        )
        .await;
        let args = json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_snapshot_sha256":"a".repeat(64),"summary":"new"});
        let reply = call(&router, "edit_summary", args.clone()).await;
        assert_ne!(reply["isError"], true);
        assert_eq!(
            body(&reply)["result"]["summary_snapshot_sha256"],
            "b".repeat(64)
        );
        *mock.state.response.lock().unwrap() = Some(crate::response::tool_success(
            &json!({"id":ID,"summary_snapshot_sha256":"b".repeat(64),"db":"user","db_id":OTHER_ID}),
        ));
        let reply = call(&router, "edit_summary", args).await;
        assert_eq!(reply["isError"], true);
        assert!(body(&reply).to_string().contains("no automatic replay"));
        assert_eq!(mock.state.calls.lock().unwrap().len(), 2);
        router.close().await;
    }

    #[tokio::test]
    async fn edit_summary_malformed_and_older_contract_denied_before_owner_work() {
        let mut tools =
            crate::tool_schemas(CapabilityPolicy::new(CapabilityProfile::Operator, false));
        tools.retain(|tool| tool["name"] != "edit_summary");
        let old = Mock::with_catalog(ID, CapabilityProfile::Operator, tools).await;
        let router = server(
            config(vec![owner("memory", old.port, ID)], "memory"),
            Path::new("."),
        )
        .await;
        let before = old.wire_count();
        assert_eq!(
            call(
                &router,
                "edit_summary",
                json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_snapshot_sha256":"a".repeat(64),"summary":"new"})
            )
            .await["isError"],
            true
        );
        assert_eq!(old.wire_count(), before);
        router.close().await;

        let mock = Mock::new(ID, CapabilityProfile::Operator).await;
        let router = server(
            config(vec![owner("memory", mock.port, ID)], "memory"),
            Path::new("."),
        )
        .await;
        let before = mock.wire_count();
        for args in [
            json!({"db":"memory","id":ID,"expected_snapshot_sha256":"a".repeat(64),"summary":"new"}),
            json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_snapshot_sha256":"A".repeat(64),"summary":"new"}),
            json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_snapshot_sha256":"a".repeat(64),"summary":null}),
            json!({"db":"memory","expected_db_id":OTHER_ID,"id":ID,"expected_snapshot_sha256":"a".repeat(64),"summary":"new"}),
            json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_snapshot_sha256":"a".repeat(64),"summary":"new","snapshot":{"source_device_id":"pi","generation":"G1"}}),
        ] {
            assert_eq!(call(&router, "edit_summary", args).await["isError"], true);
        }
        assert_eq!(mock.wire_count(), before);
        mock.state.fail_next.store(true, Ordering::SeqCst);
        let result = call(
            &router,
            "edit_summary",
            json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_snapshot_sha256":"a".repeat(64),"summary":"new"}),
        )
        .await;
        assert!(body(&result).to_string().contains("no automatic replay"));
        assert_eq!(mock.state.calls.lock().unwrap().len(), 1);
        router.close().await;
    }

    #[tokio::test]
    async fn retag_malformed_and_older_contract_denied_before_owner_work() {
        let mut tools =
            crate::tool_schemas(CapabilityPolicy::new(CapabilityProfile::Operator, false));
        tools.retain(|tool| tool["name"] != "retag");
        let old = Mock::with_catalog(ID, CapabilityProfile::Operator, tools).await;
        let router = server(
            config(vec![owner("memory", old.port, ID)], "memory"),
            Path::new("."),
        )
        .await;
        let before = old.wire_count();
        assert_eq!(
            call(
                &router,
                "retag",
                json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_tags":[],"tags":[]})
            )
            .await["isError"],
            true
        );
        assert_eq!(old.wire_count(), before);
        router.close().await;

        let mock = Mock::new(ID, CapabilityProfile::Operator).await;
        let router = server(
            config(vec![owner("memory", mock.port, ID)], "memory"),
            Path::new("."),
        )
        .await;
        let before = mock.wire_count();
        for args in [
            json!({"db":"memory","id":ID,"expected_tags":[],"tags":[]}),
            json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_tags":[],"tags":["x","x"]}),
            json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_tags":null,"tags":[]}),
            json!({"db":"memory","expected_db_id":OTHER_ID,"id":ID,"expected_tags":[],"tags":[]}),
            json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_tags":[],"tags":[],"snapshot":{"source_device_id":"pi","generation":"G1"}}),
        ] {
            assert_eq!(call(&router, "retag", args).await["isError"], true);
        }
        assert_eq!(mock.wire_count(), before);
        mock.state.fail_next.store(true, Ordering::SeqCst);
        let result = call(
            &router,
            "retag",
            json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_tags":[],"tags":[]}),
        )
        .await;
        assert!(body(&result).to_string().contains("no automatic replay"));
        assert_eq!(mock.state.calls.lock().unwrap().len(), 1);
        router.close().await;
    }

    #[tokio::test]
    async fn tag_vocabulary_and_content_guards_stay_bound_to_the_selected_owner() {
        let mut legacy_tools = crate::tool_schemas(CapabilityPolicy::operator());
        let retag = legacy_tools
            .iter_mut()
            .find(|tool| tool["name"] == "retag")
            .unwrap();
        for field in ["expected_content_fingerprint", "guard_nodes"] {
            retag["inputSchema"]["properties"]
                .as_object_mut()
                .unwrap()
                .remove(field);
        }
        retag["inputSchema"]
            .as_object_mut()
            .unwrap()
            .remove("dependentRequired");
        let list = legacy_tools
            .iter_mut()
            .find(|tool| tool["name"] == "list")
            .unwrap();
        list["inputSchema"]["properties"]["kind"]["enum"] = json!(["nodes", "touchstones"]);
        list["inputSchema"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("prefix");
        list["inputSchema"]["oneOf"]
            .as_array_mut()
            .unwrap()
            .retain(|branch| branch["properties"]["kind"]["const"] != "tags");
        let legacy = Mock::with_catalog(ID, CapabilityProfile::Operator, legacy_tools).await;
        let current = Mock::new(ID, CapabilityProfile::Operator).await;
        let router = server(
            config(
                vec![
                    owner("legacy", legacy.port, ID),
                    owner("current", current.port, ID),
                ],
                "legacy",
            ),
            Path::new("."),
        )
        .await;
        let catalog = router.dispatch("tools/list", json!({})).await.unwrap();
        let schema = |name| {
            catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap()["inputSchema"]
                .clone()
        };
        let mut guarded = json!({"db":"legacy","expected_db_id":ID,"id":ID,"expected_tags":[],"tags":["people"],"expected_content_fingerprint":"a".repeat(64),"guard_nodes":[{"id":OTHER_ID,"content_fingerprint":"b".repeat(64)}]});
        let tags = json!({"db":"legacy","kind":"tags","prefix":"People","status":"all","limit":1});
        assert!(!crate::tests::schema_accepts(&schema("retag"), &guarded));
        assert!(!crate::tests::schema_accepts(&schema("list"), &tags));
        let before = legacy.wire_count();
        assert_eq!(
            call(&router, "retag", guarded.clone()).await["isError"],
            true
        );
        assert_eq!(call(&router, "list", tags.clone()).await["isError"], true);
        assert_eq!(
            legacy.wire_count(),
            before,
            "legacy refusals must precede identity probes and tool submission"
        );
        // Existing tag-only callers remain supported on the old owner.
        let weak = json!({"db":"legacy","expected_db_id":ID,"id":ID,"expected_tags":[],"tags":[]});
        assert!(crate::tests::schema_accepts(&schema("retag"), &weak));
        assert_ne!(call(&router, "retag", weak).await["isError"], true);
        guarded["db"] = json!("current");
        assert!(crate::tests::schema_accepts(&schema("retag"), &guarded));
        assert_ne!(
            call(&router, "retag", guarded.clone()).await["isError"],
            true
        );
        let mut tags = tags;
        tags["db"] = json!("current");
        assert!(crate::tests::schema_accepts(&schema("list"), &tags));
        assert_ne!(call(&router, "list", tags.clone()).await["isError"], true);
        let forwarded = current.state.calls.lock().unwrap();
        assert_eq!(forwarded.len(), 2);
        assert_eq!(
            forwarded[0].0["arguments"]["expected_content_fingerprint"],
            guarded["expected_content_fingerprint"]
        );
        assert_eq!(
            forwarded[0].0["arguments"]["guard_nodes"],
            guarded["guard_nodes"]
        );
        assert_eq!(forwarded[1].0["arguments"]["kind"], "tags");
        assert_eq!(forwarded[1].0["arguments"]["prefix"], "People");
        drop(forwarded);
        router.close().await;
    }

    #[tokio::test]
    async fn malformed_strong_or_branch_only_tag_catalog_is_not_published() {
        let mut tools = crate::tool_schemas(CapabilityPolicy::operator());
        let retag = tools
            .iter_mut()
            .find(|tool| tool["name"] == "retag")
            .unwrap();
        retag["inputSchema"]["properties"]["guard_nodes"]["maxItems"] = json!(0);
        let list = tools
            .iter_mut()
            .find(|tool| tool["name"] == "list")
            .unwrap();
        // A tags branch is still an advertisement even without its root enum.
        list["inputSchema"]["properties"]["kind"]
            .as_object_mut()
            .unwrap()
            .remove("enum");
        list["inputSchema"]["oneOf"][2]["properties"]["limit"]["maximum"] = json!(0);
        let mock = Mock::with_catalog(ID, CapabilityProfile::Operator, tools).await;
        let router = server(
            config(vec![owner("memory", mock.port, ID)], "memory"),
            Path::new("."),
        )
        .await;
        let catalog = router.dispatch("tools/list", json!({})).await.unwrap();
        assert!(
            !catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "retag" || tool["name"] == "list")
        );
        let before = mock.wire_count();
        assert_eq!(
            call(&router, "list", json!({"db":"memory","kind":"tags"})).await["isError"],
            true
        );
        assert_eq!(
            call(
                &router,
                "retag",
                json!({"db":"memory","expected_db_id":ID,"id":ID,"expected_tags":[],"tags":[]})
            )
            .await["isError"],
            true
        );
        assert_eq!(mock.wire_count(), before);
        router.close().await;
    }

    #[tokio::test]
    async fn older_save_contract_is_denied_from_cached_discovery_before_any_wire() {
        let mut tools =
            crate::tool_schemas(CapabilityPolicy::new(CapabilityProfile::Operator, false));
        let save = tools
            .iter_mut()
            .find(|tool| tool["name"] == "save")
            .unwrap();
        save["inputSchema"]["properties"]["kind"]["enum"] = json!(["note"]);
        save["inputSchema"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("links");
        let mock = Mock::with_catalog(ID, CapabilityProfile::Operator, tools).await;
        let router = server(
            config(vec![owner("user", mock.port, ID)], "user"),
            Path::new("."),
        )
        .await;
        let before = mock.wire_count();
        for args in [
            json!({"db":"user","kind":"episode","summary":"experience"}),
            json!({"db":"user","summary":"note","links":[]}),
        ] {
            assert_eq!(call(&router, "save", args).await["isError"], true);
        }
        assert_eq!(mock.wire_count(), before);
        router.close().await;
    }

    #[tokio::test]
    async fn identity_mismatch_fails_startup_and_missing_defaults_are_rejected() {
        let mock = Mock::new(OTHER_ID, CapabilityProfile::Operator).await;
        assert!(
            RouterServer::from_config(
                config(vec![owner("user", mock.port, ID)], "user"),
                Path::new("."),
                timeouts()
            )
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("identity/state mismatch")
        );
        wait_deletes(&mock, 1).await;
        assert!(mock.state.calls.lock().unwrap().is_empty());
        let before = mock.state.initializes.load(Ordering::SeqCst);
        assert!(
            RouterServer::from_config(
                config(vec![owner("user", mock.port, OTHER_ID)], "missing"),
                Path::new("."),
                timeouts()
            )
            .await
            .is_err()
        );
        assert_eq!(mock.state.initializes.load(Ordering::SeqCst), before);
    }

    #[tokio::test]
    async fn native_errors_metadata_and_ambiguous_outcomes_are_preserved_without_replay() {
        let mock = Mock::new(ID, CapabilityProfile::Operator).await;
        let router = server(
            config(vec![owner("user", mock.port, ID)], "user"),
            Path::new("."),
        )
        .await;
        *mock.state.response.lock().unwrap() = Some(
            json!({"isError":true,"content":[{"type":"text","text":"native refusal"}],"_meta":{"error_code":"unchanged"},"structuredContent":{"error":"native"}}),
        );
        let result = call(&router, "status", json!({})).await;
        assert_eq!(result["isError"], true);
        assert_eq!(result["_meta"], json!({"error_code":"unchanged"}));
        assert_eq!(
            body(&result),
            json!({"db":"user","result":"native refusal"})
        );
        mock.state.fail_next.store(true, Ordering::SeqCst);
        let result = call(&router, "save", json!({"db":"user","summary":"once"})).await;
        assert!(body(&result).to_string().contains("no automatic replay"));
        wait_deletes(&mock, 1).await;
        assert_eq!(mock.state.calls.lock().unwrap().len(), 2);
        assert_eq!(mock.state.initializes.load(Ordering::SeqCst), 1);
        *mock.state.response.lock().unwrap() =
            Some(json!({"isError":false,"content":[{"type":"text","text":"not domain JSON"}]}));
        let result = call(&router, "save", json!({"db":"user","summary":"different"})).await;
        assert!(
            body(&result)
                .to_string()
                .contains("upstream call completed")
        );
        assert_eq!(mock.state.initializes.load(Ordering::SeqCst), 2);
        assert_eq!(mock.state.calls.lock().unwrap().len(), 3);
        router.close().await;
    }

    #[tokio::test]
    async fn busy_timeout_and_cancellation_fence_sessions_without_replay() {
        let user = Mock::new(ID, CapabilityProfile::Operator).await;
        let misc = Mock::new(OTHER_ID, CapabilityProfile::Operator).await;
        let router = Arc::new(
            server(
                config(
                    vec![
                        owner("user", user.port, ID),
                        owner("misc", misc.port, OTHER_ID),
                    ],
                    "misc",
                ),
                Path::new("."),
            )
            .await,
        );
        user.state.delay_next.store(true, Ordering::SeqCst);
        let pending = {
            let router = router.clone();
            tokio::spawn(async move { call(&router, "status", json!({"db":"user"})).await })
        };
        wait_calls(&user, 1).await;
        assert!(
            body(&call(&router, "status", json!({"db":"user"})).await)
                .to_string()
                .contains("busy")
        );
        assert_eq!(
            call(&router, "status", json!({"db":"misc"})).await["isError"],
            false
        );
        assert!(
            body(&pending.await.unwrap())
                .to_string()
                .contains("no automatic replay")
        );
        wait_deletes(&user, 1).await;
        assert_eq!(user.state.calls.lock().unwrap().len(), 1);
        user.state.delay_next.store(true, Ordering::SeqCst);
        let pending = {
            let router = router.clone();
            tokio::spawn(async move { call(&router, "status", json!({"db":"user"})).await })
        };
        wait_calls(&user, 2).await;
        pending.abort();
        let _ = pending.await;
        wait_deletes(&user, 2).await;
        assert_eq!(
            call(&router, "status", json!({"db":"user"})).await["isError"],
            false
        );
        assert_eq!(user.state.initializes.load(Ordering::SeqCst), 3);
        assert_eq!(user.state.calls.lock().unwrap().len(), 3);
        router.close().await;
    }

    struct ReplicaFixture {
        root: PathBuf,
        mock: Mock,
    }
    impl Drop for ReplicaFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    impl ReplicaFixture {
        async fn new() -> Self {
            let mock = Mock::new(ID, CapabilityProfile::ReadOnly).await;
            let root = std::env::temp_dir().join(format!("router-replica-{}", ulid::Ulid::new()));
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("library.json"),serde_json::to_vec(&json!({"schema":"mneme.library.config.v1","library_id":"test","device_id":"pi","catalog_path":"catalog.json","replicas":{"pi":{"url":format!("http://127.0.0.1:{}/",mock.port)}},"rerank":false})).unwrap()).unwrap();
            let fixture = Self { root, mock };
            fixture.publish(&["G1"]);
            fixture
        }
        fn publish(&self, generations: &[&str]) {
            let replicas:Vec<_>=generations.iter().enumerate().map(|(i,generation)|json!({"source_device_id":"pi","database":format!("mneme_{generation}"),"resolved_path":format!("/cache/{generation}/store.db"),"generation":generation,"captured_at":i+1})).collect();
            std::fs::write(self.root.join("catalog.json"),serde_json::to_vec(&json!({"schema":"mneme.library.catalog.v1","library_id":"test","revision":generations.len(),"entries":[{"project_id":"authorized","db_id":ID,"owner_device_id":"pi","display_name":"Mneme","database":"project","revision":1,"replicas":replicas},{"project_id":"hidden","db_id":OTHER_ID,"owner_device_id":"pi","display_name":"private other project","database":"other","revision":1,"replicas":[]}]})).unwrap()).unwrap();
            *self.mock.state.databases.lock().unwrap()=generations.iter().map(|generation|json!({"db":format!("mneme_{generation}"),"name":format!("mneme_{generation}"),"db_id":ID,"state":"open","resolved_path":format!("/cache/{generation}/store.db")})).collect();
        }
        fn config(&self) -> RouterConfig {
            config(
                vec![DatabaseConfig::Replica {
                    name: "mneme".into(),
                    library_config: PathBuf::from("library.json"),
                    project_id: "authorized".into(),
                    expected_db_id: ID.into(),
                }],
                "mneme",
            )
        }
    }

    #[tokio::test]
    async fn mixed_catalog_has_one_native_toolset_for_live_and_cached_databases() {
        let fixture = ReplicaFixture::new().await;
        let user = Mock::new(ID, CapabilityProfile::Operator).await;
        let misc = Mock::new(OTHER_ID, CapabilityProfile::Operator).await;
        let mut cfg = fixture.config();
        cfg.databases.insert(0, owner("user", user.port, ID));
        cfg.databases.insert(1, owner("misc", misc.port, OTHER_ID));
        cfg.default_read_db = "misc".into();
        let router = server(cfg, &fixture.root).await;
        let emitted = router.dispatch("tools/list", json!({})).await.unwrap();
        let names: HashSet<_> = emitted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), catalog::TOOLS.len() + 1);
        assert_eq!(
            emitted["tools"].as_array().unwrap().len(),
            catalog::TOOLS.len() + 1
        );
        let init = router
            .dispatch(
                "initialize",
                json!({"protocolVersion":crate::PROTOCOL_VERSION}),
            )
            .await
            .unwrap();
        assert!(
            init["instructions"]
                .as_str()
                .unwrap()
                .contains(mneme_app::episode::MEMORY_TIME_GUIDANCE)
        );
        let edit = emitted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "edit_body")
            .unwrap();
        assert_eq!(
            edit["inputSchema"]["properties"]["db"]["enum"],
            json!(["misc", "user"])
        );
        let summary_edit = emitted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "edit_summary")
            .unwrap();
        assert_eq!(
            summary_edit["inputSchema"]["properties"]["db"]["enum"],
            json!(["misc", "user"])
        );
        let retag = emitted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "retag")
            .unwrap();
        assert_eq!(
            retag["inputSchema"]["properties"]["db"]["enum"],
            json!(["misc", "user"])
        );
        let before = fixture.mock.wire_count();
        for snapshot in [
            None,
            Some(json!({"source_device_id":"pi","generation":"G1"})),
        ] {
            let mut args = json!({"db":"mneme","expected_db_id":ID,"id":ID,"expected_body_revision":"a".repeat(64),"body":"new"});
            if let Some(snapshot) = snapshot {
                args["snapshot"] = snapshot;
            }
            assert_eq!(
                call(&router, "edit_body", args.clone()).await["isError"],
                true
            );
            args.as_object_mut().unwrap().remove("body");
            args.as_object_mut()
                .unwrap()
                .remove("expected_body_revision");
            args["summary"] = json!("new");
            args["expected_snapshot_sha256"] = json!("a".repeat(64));
            assert_eq!(call(&router, "edit_summary", args).await["isError"], true);
        }
        assert_eq!(fixture.mock.wire_count(), before);
        let support = body(&call(&router, "databases", json!({})).await);
        assert_eq!(support.as_array().unwrap().len(), 3);
        assert!(
            support
                .as_array()
                .unwrap()
                .iter()
                .find(|db| db["db"] == "mneme")
                .unwrap()["read_only"]
                == true
        );
        assert!(
            !support
                .as_array()
                .unwrap()
                .iter()
                .find(|db| db["db"] == "mneme")
                .unwrap()["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "retag")
        );
        router.close().await;
    }

    #[tokio::test]
    async fn replica_rotation_exact_pin_roundtrip_and_retirement_use_same_verified_session() {
        let fixture = ReplicaFixture::new().await;
        let router = server(fixture.config(), &fixture.root).await;
        let first = body(&call(&router, "get", json!({"id":ID})).await);
        assert_eq!(
            first["snapshot"],
            json!({"source_device_id":"pi","generation":"G1","captured_at":1})
        );
        assert_eq!(first["result"]["native_db"], "mneme_G1");
        fixture.publish(&["G1", "G2"]);
        let second = body(&call(&router, "get", json!({"id":ID})).await);
        assert_eq!(second["snapshot"]["generation"], "G2");
        assert_eq!(second["result"]["native_db"], "mneme_G2");
        let retained = body(
            &call(
                &router,
                "get",
                json!({"id":ID,"snapshot":first["snapshot"],"body":true,"body_offset":1}),
            )
            .await,
        );
        assert_eq!(retained["snapshot"], first["snapshot"]);
        assert_eq!(retained["result"]["native_db"], "mneme_G1");
        let before = fixture.mock.wire_count();
        for args in [
            json!({"id":ID,"body":true,"body_offset":1}),
            json!({"action":"get","episode_id":ID,"body":true,"offset":1}),
        ] {
            let tool = if args.get("action").is_some() {
                "episode"
            } else {
                "get"
            };
            let refused = call(&router, tool, args).await;
            assert_eq!(refused["isError"], true);
            assert!(body(&refused).to_string().contains("snapshot"), "{refused}");
        }
        assert_eq!(fixture.mock.wire_count(), before);
        let cursor = format!(
            "nodes-v1:{}",
            json!({"db_id":ID,"status":"all","tag":null,"through":OTHER_ID,"after":ID})
        );
        assert_eq!(
            call(&router, "list", json!({"after":cursor})).await["isError"],
            true
        );
        assert_eq!(fixture.mock.wire_count(), before);
        assert_eq!(
            call(
                &router,
                "list",
                json!({"after":cursor,"snapshot":first["snapshot"]})
            )
            .await["isError"],
            false
        );
        let sessions = fixture.mock.state.verification_sessions.lock().unwrap();
        for (_, session) in fixture.mock.state.calls.lock().unwrap().iter() {
            assert!(sessions.contains(session));
        }
        drop(sessions);
        fixture.publish(&["G2"]);
        let before = fixture.mock.wire_count();
        let expired = call(
            &router,
            "get",
            json!({"id":ID,"snapshot":first["snapshot"]}),
        )
        .await;
        assert!(body(&expired).to_string().contains("expired"));
        assert_eq!(fixture.mock.wire_count(), before);
        let databases = body(&call(&router, "databases", json!({})).await);
        assert_eq!(databases[0]["snapshot"]["generation"], "G2");
        assert!(!databases.to_string().contains("hidden"));
        assert!(!databases.to_string().contains("/cache/"));
        router.close().await;
    }

    #[tokio::test]
    async fn replicas_deny_all_writes_before_wire_and_refuse_wrong_serving_generation() {
        let fixture = ReplicaFixture::new().await;
        let router = server(fixture.config(), &fixture.root).await;
        let before = fixture.mock.wire_count();
        for (name, args) in [
            ("save", json!({"db":"mneme","summary":"no"})),
            ("forget", json!({"db":"mneme","id":ID})),
            (
                "retag",
                json!({"db":"mneme","expected_db_id":ID,"id":ID,"expected_tags":[],"tags":[]}),
            ),
            (
                "episode",
                json!({"db":"mneme","action":"append","summary":"no","source":{"namespace":"manual","key":"no","reference":"test:no"}}),
            ),
            ("link", json!({"db":"mneme","from":ID,"to":OTHER_ID})),
        ] {
            assert_eq!(call(&router, name, args).await["isError"], true);
        }
        assert_eq!(fixture.mock.wire_count(), before);
        fixture.mock.state.databases.lock().unwrap()[0]["resolved_path"] =
            json!("/cache/G2/store.db");
        let result = call(&router, "get", json!({"id":ID})).await;
        assert_eq!(result["isError"], true);
        assert!(body(&result).to_string().contains("mismatched"));
        assert!(!result.to_string().contains("/cache/"));
        assert!(fixture.mock.state.calls.lock().unwrap().is_empty());
        router.close().await;
        let operator = Mock::new(ID, CapabilityProfile::Operator).await;
        let mut library: Value =
            serde_json::from_slice(&std::fs::read(fixture.root.join("library.json")).unwrap())
                .unwrap();
        library["replicas"]["pi"]["url"] = json!(format!("http://127.0.0.1:{}/", operator.port));
        std::fs::write(
            fixture.root.join("library.json"),
            serde_json::to_vec(&library).unwrap(),
        )
        .unwrap();
        assert!(
            RouterServer::from_config(fixture.config(), &fixture.root, timeouts())
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("read-only")
        );
        assert!(operator.state.calls.lock().unwrap().is_empty());
    }
}
