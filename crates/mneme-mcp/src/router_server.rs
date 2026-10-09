//! A native-named tool set across explicit existing owners and immutable replicas.
//! No store opener, implicit owner fallback, or automatic request replay lives here.
use crate::{
    CapabilityPolicy, CapabilityProfile, host::AnyErr, router_catalog as catalog,
    router_response as response,
};
use mneme_library::{LibraryRuntime, ReplicaPin, ReplicaRoute};
use mneme_mcp_client::{ClientTimeouts, ConnectionOptions, RemoteClient};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use url::{Host, Url};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RouterConfig {
    schema: String,
    default_read_db: String,
    databases: Vec<DatabaseConfig>,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum DatabaseConfig {
    Owner {
        name: String,
        url: String,
        db: String,
        expected_db_id: String,
        #[serde(default)]
        token_env: Option<String>,
    },
    Replica {
        name: String,
        library_config: PathBuf,
        project_id: String,
        expected_db_id: String,
    },
}
impl DatabaseConfig {
    fn name(&self) -> &str {
        match self {
            Self::Owner { name, .. } | Self::Replica { name, .. } => name,
        }
    }
    fn expected(&self) -> &str {
        match self {
            Self::Owner { expected_db_id, .. } | Self::Replica { expected_db_id, .. } => {
                expected_db_id
            }
        }
    }
    fn validate(&self) -> Result<(), AnyErr> {
        valid_alias(self.name())?;
        crate::optional_expected_db_id(&json!({"expected_db_id":self.expected()}))?;
        match self {
            Self::Owner { url, db, .. } => {
                validate_endpoint(url)?;
                valid_alias(db)?;
            }
            Self::Replica {
                project_id,
                library_config,
                ..
            } => {
                if project_id.is_empty()
                    || project_id.len() > 256
                    || project_id.trim() != project_id
                    || project_id.chars().any(char::is_control)
                    || library_config.as_os_str().is_empty()
                {
                    return Err("invalid replica project/config binding".into());
                }
            }
        }
        Ok(())
    }
}
fn valid_alias(value: &str) -> Result<(), AnyErr> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(
            "database aliases must be 1..=256 ASCII letters, digits, underscores or hyphens".into(),
        );
    }
    Ok(())
}
fn validate_endpoint(value: &str) -> Result<(), AnyErr> {
    let url = Url::parse(value).map_err(|_| "invalid router endpoint")?;
    let loopback = match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if url.scheme() != "http"
        || !loopback
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("router endpoint must be numeric loopback HTTP with explicit port and no credentials, query or fragment".into());
    }
    Ok(())
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    source_device_id: String,
    generation: String,
    #[serde(default, deserialize_with = "deserialize_capture")]
    captured_at: Option<u64>,
}
fn deserialize_capture<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    u64::deserialize(deserializer).map(Some)
}
impl Snapshot {
    fn pin(&self) -> Result<ReplicaPin, AnyErr> {
        for value in [&self.source_device_id, &self.generation] {
            if value.is_empty()
                || value.len() > 256
                || value.trim() != value
                || value.chars().any(char::is_control)
            {
                return Err("snapshot fields must be bounded nonempty strings".into());
            }
        }
        Ok(ReplicaPin {
            source_device_id: self.source_device_id.clone(),
            generation: self.generation.clone(),
        })
    }
}
enum Binding {
    Owner {
        options: ConnectionOptions,
        db: String,
    },
    Replica {
        library: LibraryRuntime,
        project_id: String,
    },
}
struct Route {
    name: String,
    expected: String,
    binding: Binding,
    policy: CapabilityPolicy,
    tools: Vec<Value>,
    request_support: RequestSupport,
    session: Mutex<Option<Session>>,
    timeouts: ClientTimeouts,
}
#[derive(Default, Clone, Copy, PartialEq, Eq)]
struct RequestSupport {
    note: bool,
    episode: bool,
    note_links: bool,
    episode_links: bool,
    retag_content_guards: bool,
    list_tags: bool,
}
impl RequestSupport {
    fn from_client(client: &RemoteClient) -> Self {
        Self {
            note: client.supports_save_kind("note"),
            episode: client.supports_save_kind("episode"),
            note_links: client.supports_save_links("note"),
            episode_links: client.supports_save_links("episode"),
            retag_content_guards: client.supports_retag_content_guards(),
            list_tags: client.supports_list_tags(),
        }
    }
    fn admits_save(self, args: &Value) -> bool {
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("note");
        match kind {
            "note" => self.note && (args.get("links").is_none() || self.note_links),
            "episode" => self.episode && (args.get("links").is_none() || self.episode_links),
            _ => false,
        }
    }
}
struct Session {
    url: String,
    token_env: Option<String>,
    client: RemoteClient,
}
struct InFlight(Option<Session>);
impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(mut session) = self.0.take() {
            tokio::spawn(async move {
                session.client.close().await;
            });
        }
    }
}

impl Route {
    fn replica(&self) -> bool {
        matches!(self.binding, Binding::Replica { .. })
    }
    fn resolve(&self, pin: Option<&ReplicaPin>) -> Result<Option<ReplicaRoute>, AnyErr> {
        let Binding::Replica {
            library,
            project_id,
        } = &self.binding
        else {
            return Ok(None);
        };
        let route=library.resolve_replica(project_id,pin).map_err(|_|"snapshot unavailable or expired; select a new snapshot, or restore the exact retained generation")?;
        if route.expected_db_id() != self.expected {
            return Err("snapshot project identity changed; check configured pin, do not follow a replacement".into());
        }
        validate_endpoint(&route.endpoint().url)?;
        Ok(Some(route))
    }
    fn options(&self, replica: Option<&ReplicaRoute>) -> ConnectionOptions {
        match &self.binding {
            Binding::Owner { options, .. } => ConnectionOptions {
                url: options.url.clone(),
                token_env: options.token_env.clone(),
                ssh_mcp_port: 0,
            },
            Binding::Replica { .. } => {
                let endpoint = replica.expect("resolved replica").endpoint();
                ConnectionOptions {
                    url: endpoint.url.clone(),
                    token_env: endpoint.token_env.clone(),
                    ssh_mcp_port: 0,
                }
            }
        }
    }
    fn native_db(&self, replica: Option<&ReplicaRoute>) -> String {
        match &self.binding {
            Binding::Owner { db, .. } => db.clone(),
            Binding::Replica { .. } => replica.expect("resolved replica").database().into(),
        }
    }
    async fn connect(
        &self,
        options: &ConnectionOptions,
    ) -> Result<(Session, Vec<Value>, CapabilityPolicy, RequestSupport), AnyErr> {
        let mut flight = InFlight(Some(Session {
            url: options.url.clone(),
            token_env: options.token_env.clone(),
            client: RemoteClient::connect_with_timeouts(options, self.timeouts)
                .await
                .map_err(|_| "upstream unavailable; check its existing service and credentials")?,
        }));
        let client = &mut flight.0.as_mut().expect("connected session").client;
        if client.server_name() != "mneme-mcp" {
            return Err("upstream is not a native mneme-mcp owner".into());
        }
        let profile = client
            .initialize_result()
            .get("capabilityProfile")
            .and_then(Value::as_str)
            .ok_or("upstream omitted native capability profile")?;
        let profile = CapabilityProfile::parse(profile)
            .map_err(|_| "upstream capability profile is unsupported")?;
        if self.replica() && profile != CapabilityProfile::ReadOnly {
            return Err("replica endpoint must run the read-only native profile".into());
        }
        let policy = CapabilityPolicy::new(
            if self.replica() {
                CapabilityProfile::ReadOnly
            } else {
                profile
            },
            false,
        );
        let tools = catalog::ordinary_catalog(client, policy)?;
        let request_support = RequestSupport::from_client(client);
        Ok((
            flight.0.take().expect("checked session"),
            tools,
            policy,
            request_support,
        ))
    }
    async fn verify(
        &self,
        client: &mut RemoteClient,
        replica: Option<&ReplicaRoute>,
    ) -> Result<(), AnyErr> {
        let databases = client
            .call_tool("databases", json!({}))
            .await
            .map_err(|_| "upstream database discovery failed; no operation was sent")?;
        if let Some(replica) = replica {
            replica.verify_databases(&databases).map_err(|_|"snapshot serving identity/path is unavailable or mismatched; no operation was sent")?;
        } else {
            let db = self.native_db(None);
            let rows = databases
                .as_array()
                .ok_or("invalid upstream database catalog")?;
            let matches: Vec<_> = rows.iter().filter(|row| row["db"] == db).collect();
            if matches.len() != 1
                || matches[0]["db_id"] != self.expected
                || matches[0]["state"] != "open"
            {
                return Err("upstream database identity/state mismatch; check pinned owner, do not follow a replacement".into());
            }
        }
        Ok(())
    }
    async fn call(
        &self,
        name: &str,
        mut params: Value,
        explicit_db: bool,
    ) -> Result<Value, AnyErr> {
        let mut args = params.get("arguments").cloned().unwrap_or(json!({}));
        let object = args
            .as_object_mut()
            .ok_or("tool arguments must be an object")?;
        let snapshot: Option<Snapshot> = object
            .remove("snapshot")
            .map(serde_json::from_value)
            .transpose()?;
        let pin = snapshot.as_ref().map(Snapshot::pin).transpose()?;
        if pin.is_some() && !self.replica() {
            return Err("snapshot is only valid on a read-only replica database".into());
        }
        if name == "get" && object.get("edges") == Some(&json!(true)) {
            return Err(
                "hosted get cannot hydrate remote edges; use neighbors for selected-database edges"
                    .into(),
            );
        }
        if name == "link" && object.contains_key("to_db") {
            return Err("cross-database links are not exposed; to_db is forbidden".into());
        }
        if matches!(name, "retag" | "edit_body" | "edit_summary")
            && !object.contains_key("expected_db_id")
        {
            return Err(
                format!("{name} requires explicit expected_db_id; no operation was sent").into(),
            );
        }
        if let Some(expected) = crate::optional_expected_db_id(&args)? {
            if expected.to_string() != self.expected {
                return Err(
                    "expected_db_id does not match this logical database's configured identity"
                        .into(),
                );
            }
        }
        // Prepare against the configured identity before resolving a catalog,
        // reconnecting, or touching any upstream session.
        args["db"] = json!(self.name);
        args["expected_db_id"] = json!(self.expected);
        let prepared = crate::ValidatedToolArguments::parse(name, &args)?;
        if prepared.kind().requires_explicit_db() && !explicit_db {
            return Err("mutations require explicit db; no operation was sent".into());
        }
        self.policy.authorize(prepared.kind()).map_err(|_|"this database does not authorize the requested operation/action; no operation was sent")?;
        if name == "save" && !self.request_support.admits_save(&args) {
            return Err(
                "save kind/links are not advertised for this database; no operation was sent"
                    .into(),
            );
        }
        if prepared
            .prepared_retag
            .as_ref()
            .is_some_and(|request| request.inner.requires_content_guards())
            && !self.request_support.retag_content_guards
        {
            return Err(
                "retag content guards are not advertised for this database; no operation was sent"
                    .into(),
            );
        }
        if name == "list" && args["kind"] == "tags" && !self.request_support.list_tags {
            return Err(
                "tag vocabulary is not advertised for this database; no operation was sent".into(),
            );
        }
        if !catalog::advertises_action(&self.tools, name, &args) {
            return Err(
                "operation/action is not advertised for this database; no operation was sent"
                    .into(),
            );
        }
        if self.replica()
            && pin.is_none()
            && ((matches!(name, "list" | "neighbors" | "episode")
                && (args.get("after").is_some() || args.get("cursor").is_some()))
                || (name == "get"
                    && args
                        .get("body_offset")
                        .and_then(Value::as_u64)
                        .is_some_and(|offset| offset > 0))
                || (name == "episode"
                    && args
                        .get("offset")
                        .and_then(Value::as_u64)
                        .is_some_and(|offset| offset > 0)))
        {
            return Err("replica pagination requires the snapshot returned by the first page; repeat with that exact snapshot, not the latest generation".into());
        }
        let replica = self.resolve(pin.as_ref())?;
        if let (Some(snapshot), Some(replica)) = (snapshot.as_ref(), replica.as_ref()) {
            if snapshot
                .captured_at
                .is_some_and(|captured| captured != replica.captured_at())
            {
                return Err(
                    "snapshot capture metadata mismatch; reuse the returned reference unchanged"
                        .into(),
                );
            }
        }
        let checked_retag = prepared
            .prepared_retag
            .as_ref()
            .map(|request| request.inner.clone());
        let checked_body_edit = prepared
            .prepared_body_edit
            .as_ref()
            .map(|request| request.inner.clone());
        let checked_summary_edit = prepared
            .prepared_summary_edit
            .as_ref()
            .map(|request| request.inner.clone());
        let capture = replica.as_ref().map(snapshot_metadata);
        let outcome: Result<Value, AnyErr> = async {
        args["db"] = json!(self.native_db(replica.as_ref()));
        params["arguments"] = args;
        let options = self.options(replica.as_ref());
        let mut guard = self
            .session
            .try_lock()
            .map_err(|_| "database is busy; no operation was sent")?;
        if guard.as_ref().is_some_and(|session| {
            session.url != options.url || session.token_env != options.token_env
        }) {
            if let Some(mut old) = guard.take() {
                old.client.close().await;
            }
        }
        if guard.is_none() {
            let (mut session, tools, policy, request_support) = self.connect(&options).await?;
            if tools != self.tools || policy != self.policy || request_support != self.request_support {
                session.client.close().await;
                return Err(
                    "upstream tool/profile contract changed; restart router to review it".into(),
                );
            }
            *guard = Some(session);
        }
        let mut flight = InFlight(guard.take());
        let session = flight.0.as_mut().expect("connected route session");
        // Replica selection can advance while the stable logical alias remains
        // unchanged. Reverify the exact edition on this session before EVERY read.
        self.verify(&mut session.client, replica.as_ref()).await?;
        let native =
            session.client.raw_rpc("tools/call", params).await.map_err(
                |_| "upstream call failed; outcome may be ambiguous, no automatic replay",
            )?;
        let wrapped = response::wrap(native, &self.name, capture.as_ref()).map_err(|_|"upstream call completed but its response could not be represented; outcome may be ambiguous, no automatic replay")?;
        if let Some(request) = checked_retag.as_ref() {
            if wrapped["isError"] != true {
                let envelope: Value = serde_json::from_str(wrapped["content"][0]["text"].as_str().ok_or("invalid retag response")?)?;
                request.validate_routed_response_json(&envelope["result"], &self.native_db(replica.as_ref()), Some(self.expected.parse()?))
                    .map_err(|_| "upstream retag acknowledgement could not be verified; outcome may be ambiguous, no automatic replay")?;
            }
        }
        if let Some(request) = checked_body_edit.as_ref() {
            if wrapped["isError"] != true {
                let envelope: Value = serde_json::from_str(wrapped["content"][0]["text"].as_str().ok_or("invalid edit_body response")?)?;
                request.validate_routed_response_json(&envelope["result"], &self.native_db(replica.as_ref()), Some(self.expected.parse()?))
                    .map_err(|_| "upstream edit_body acknowledgement could not be verified; outcome may be ambiguous, no automatic replay")?;
            }
        }
        if let Some(request) = checked_summary_edit.as_ref() {
            if wrapped["isError"] != true {
                let envelope: Value = serde_json::from_str(wrapped["content"][0]["text"].as_str().ok_or("invalid edit_summary response")?)?;
                request.validate_routed_response_json(&envelope["result"], &self.native_db(replica.as_ref()), Some(self.expected.parse()?))
                    .map_err(|_| "upstream edit_summary acknowledgement could not be verified; outcome may be ambiguous, no automatic replay")?;
            }
        }
        *guard = flight.0.take();
        Ok(wrapped)
        }.await;
        Ok(match outcome {
            Ok(result) => result,
            Err(error) => response::refusal(
                &self.name,
                capture.as_ref(),
                &format!("database {:?}: {error}", self.name),
            ),
        })
    }
    async fn close(&self) {
        if let Some(mut session) = self.session.lock().await.take() {
            session.client.close().await;
        }
    }
}
fn snapshot_metadata(replica: &ReplicaRoute) -> Value {
    json!({"source_device_id":replica.source_device_id(),"generation":replica.generation(),"captured_at":replica.captured_at()})
}

pub struct RouterServer {
    routes: BTreeMap<String, Arc<Route>>,
    default_read_db: String,
    tools: Vec<Value>,
}
impl RouterServer {
    pub async fn from_path(path: &Path) -> Result<Self, AnyErr> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take((crate::MAX_STDIO_REQUEST_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > crate::MAX_STDIO_REQUEST_BYTES {
            return Err("router config exceeds MCP request envelope".into());
        }
        let config: RouterConfig = serde_json::from_slice(&bytes)?;
        Self::from_config(
            config,
            path.parent().unwrap_or(Path::new(".")),
            ClientTimeouts::default(),
        )
        .await
    }
    async fn from_config(
        config: RouterConfig,
        base: &Path,
        timeouts: ClientTimeouts,
    ) -> Result<Self, AnyErr> {
        if config.schema != "mneme.router.config.v1" || config.databases.is_empty() {
            return Err("router needs supported config schema and explicit databases".into());
        }
        valid_alias(&config.default_read_db)?;
        let mut names = HashSet::new();
        for db in &config.databases {
            db.validate()?;
            if !names.insert(db.name()) {
                return Err("duplicate logical database alias".into());
            }
        }
        if !names.contains(config.default_read_db.as_str()) {
            return Err("default_read_db must name a configured database".into());
        }
        let deadline = tokio::time::Instant::now() + timeouts.connect;
        let mut server = Self {
            routes: BTreeMap::new(),
            default_read_db: config.default_read_db,
            tools: Vec::new(),
        };
        for db in config.databases {
            let name = db.name().to_owned();
            let expected = db.expected().to_owned();
            let binding = match db {
                DatabaseConfig::Owner {
                    url, db, token_env, ..
                } => Binding::Owner {
                    options: ConnectionOptions {
                        url,
                        token_env,
                        ssh_mcp_port: 0,
                    },
                    db,
                },
                DatabaseConfig::Replica {
                    library_config,
                    project_id,
                    ..
                } => {
                    let path = if library_config.is_absolute() {
                        library_config
                    } else {
                        base.join(library_config)
                    };
                    Binding::Replica {
                        library: match LibraryRuntime::from_path(&path) {
                            Ok(library) => library,
                            Err(_) => {
                                server.close().await;
                                return Err(format!("database {name:?}: replica library config unavailable or invalid").into());
                            }
                        },
                        project_id,
                    }
                }
            };
            let mut route = Route {
                name: name.clone(),
                expected,
                binding,
                policy: CapabilityPolicy::new(CapabilityProfile::ReadOnly, false),
                tools: Vec::new(),
                request_support: RequestSupport::default(),
                session: Mutex::new(None),
                timeouts,
            };
            let initialized = tokio::time::timeout_at(deadline, async {
                let replica = route.resolve(None)?;
                let options = route.options(replica.as_ref());
                let (session, tools, policy, request_support) = route.connect(&options).await?;
                let mut flight = InFlight(Some(session));
                route
                    .verify(
                        &mut flight.0.as_mut().expect("startup client").client,
                        replica.as_ref(),
                    )
                    .await?;
                route.tools = tools;
                route.policy = policy;
                route.request_support = request_support;
                *route.session.get_mut() = flight.0.take();
                Ok::<(), AnyErr>(())
            })
            .await;
            match initialized {
                Ok(Ok(())) => {
                    server.routes.insert(name, Arc::new(route));
                }
                Ok(Err(error)) => {
                    server.close().await;
                    return Err(format!("database {name:?}: {error}").into());
                }
                Err(_) => {
                    server.close().await;
                    return Err(format!(
                        "database {name:?}: aggregate router startup deadline exceeded"
                    )
                    .into());
                }
            }
        }
        let contracts: Vec<_> = server
            .routes
            .values()
            .map(|route| catalog::RouteCatalog {
                name: &route.name,
                replica: route.replica(),
                tools: &route.tools,
            })
            .collect();
        match catalog::build(&contracts, &server.default_read_db) {
            Ok(tools) => server.tools = tools,
            Err(error) => {
                server.close().await;
                return Err(error);
            }
        }
        let wire = serde_json::to_vec(&crate::result_response(
            json!(1),
            json!({"tools":server.tools}),
        ))?;
        if wire.len() >= crate::response::MAX_JSONRPC_FRAME_BYTES {
            server.close().await;
            return Err("aggregate router catalog exceeds MCP response envelope".into());
        }
        Ok(server)
    }
    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, AnyErr> {
        if !params.is_object() {
            return Err("router MCP params must be an object".into());
        }
        if serde_json::to_vec(&params)?.len() > crate::MAX_STDIO_REQUEST_BYTES {
            return Err("router params exceed MCP request envelope".into());
        }
        match method {
            "initialize" => Ok(
                json!({"protocolVersion":crate::negotiated_protocol(&params)?,"capabilities":{"tools":{}},"serverInfo":{"name":"mneme-mcp-router","version":env!("CARGO_PKG_VERSION"),"capabilityProfile":"configured-databases"},"capabilityProfile":"configured-databases","instructions":format!("Mneme memory. One shared toolset; db selects a configured existing owner or read-only replica. Omitted reads use {:?}; every mutation requires explicit db. Retain returned snapshot for replica continuations. No fallback or automatic replay. {}",self.default_read_db, mneme_app::episode::MEMORY_TIME_GUIDANCE)}),
            ),
            "tools/list" => Ok(json!({"tools":self.tools})),
            "ping" => Ok(json!({})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                if name == "databases" {
                    return Ok(if args == json!({}) {
                        crate::response::tool_success(&self.databases())
                    } else {
                        response::refusal("router", None, "databases accepts no arguments")
                    });
                }
                let Some(object) = args.as_object() else {
                    return Ok(response::refusal(
                        "router",
                        None,
                        "tool arguments must be an object",
                    ));
                };
                let alias = match object.get("db") {
                    None => self.default_read_db.as_str(),
                    Some(Value::String(alias)) => alias.as_str(),
                    Some(_) => {
                        return Ok(response::refusal(
                            "router",
                            None,
                            "db must be a logical database string",
                        ));
                    }
                };
                if !catalog::TOOLS.contains(&name) {
                    return Ok(response::refusal(
                        alias,
                        None,
                        "unknown router tool; use tools/list for the closed ordinary-work surface",
                    ));
                }
                let Some(route) = self.routes.get(alias) else {
                    return Ok(response::refusal(
                        alias,
                        None,
                        "unknown logical database; use databases, no fallback was attempted",
                    ));
                };
                Ok(
                    match route
                        .call(name, params.clone(), object.contains_key("db"))
                        .await
                    {
                        Ok(result) => result,
                        Err(error) => {
                            response::refusal(alias, None, &format!("database {alias:?}: {error}"))
                        }
                    },
                )
            }
            other => Err(format!("unknown router MCP method {other:?}").into()),
        }
    }
    fn databases(&self) -> Value {
        Value::Array(self.routes.values().map(|route|{
        let mut value=json!({"db":route.name,"db_id":route.expected,"read_only":!route.policy.profile.permits(crate::CapabilityClass::Curator),"default_read":route.name==self.default_read_db,"capability_profile":route.policy.profile.as_str(),"state":"configured","tools":catalog::support(&route.tools)});
        if route.replica(){match route.resolve(None){Ok(Some(replica))=>value["snapshot"]=snapshot_metadata(&replica),_=>{value["state"]=json!("unavailable");value["reason"]=json!("configured replica unavailable or expired; no fallback");}}}
        value
    }).collect())
    }
    pub async fn close(&self) {
        let mut tasks = tokio::task::JoinSet::new();
        for route in self.routes.values() {
            let route = route.clone();
            tasks.spawn(async move {
                route.close().await;
            });
        }
        while tasks.join_next().await.is_some() {}
    }
}
#[cfg(test)]
#[path = "router_server_tests.rs"]
mod tests;
