//! Bounded client for an already-running Streamable-HTTP MCP server.
//!
//! SSH is only a loopback port forward. It does not execute a remote command,
//! start a daemon, or acquire/open a local database.

use std::collections::HashSet;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};
use url::{Host, Url};

type AnyErr = Box<dyn std::error::Error + Send + Sync>;

const PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOLS: &[&str] = &[PROTOCOL_VERSION, "2025-06-18", "2025-03-26"];
const MAX_WIRE_BYTES: usize = 2 * 1024 * 1024;
const CODEX_REQUEST_BYTES: usize = 128 * 1024;
const CODEX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_TOOL_TEXT_BYTES: usize = 256 * 1024;
const MAX_TOOLS: usize = 1024;
const MAX_CATALOG_PAGES: usize = 16;
const SSH_READY_TIMEOUT: Duration = Duration::from_secs(10);
const RPC_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);

/// Total handshake deadline and per-tool-call deadline. A timeout never retries
/// a tool call: the server may already have performed a mutation.
#[derive(Clone, Copy, Debug)]
pub struct ClientTimeouts {
    pub connect: Duration,
    pub request: Duration,
}

impl Default for ClientTimeouts {
    fn default() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            request: RPC_TIMEOUT,
        }
    }
}

#[derive(Debug)]
struct AvailabilityError(String);

impl std::fmt::Display for AvailabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AvailabilityError {}

/// True only for failures that plausibly mean the endpoint is unavailable.
/// Authentication, identity, protocol, and tool failures are deliberately false.
pub fn is_availability(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(err) = current {
        if err.is::<AvailabilityError>() {
            return true;
        }
        current = err.source();
    }
    false
}

/// Stable failure categories for callers; never infer these from message text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientErrorClass {
    Availability,
    Protocol,
    Tool,
}

#[derive(Debug)]
struct ToolError(String);
impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ToolError {}

pub fn classify_error(error: &(dyn std::error::Error + 'static)) -> ClientErrorClass {
    let mut current = Some(error);
    while let Some(err) = current {
        if err.is::<AvailabilityError>() {
            return ClientErrorClass::Availability;
        }
        if err.is::<ToolError>() {
            return ClientErrorClass::Tool;
        }
        current = err.source();
    }
    ClientErrorClass::Protocol
}

#[derive(Clone, Copy, Debug)]
struct WirePolicy {
    request_bytes: usize,
    response_bytes: usize,
    expected_server: Option<&'static str>,
}
impl Default for WirePolicy {
    fn default() -> Self {
        Self {
            request_bytes: MAX_WIRE_BYTES,
            response_bytes: MAX_WIRE_BYTES,
            expected_server: None,
        }
    }
}

const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
const ACCEPT_MCP: &str = "application/json, text/event-stream";

/// An older peer may ignore unknown nested fields rather than reject them.
/// Require a recognizable linked-capture shape before forwarding a mutation;
/// do not pin incidental schema details such as enum order or maxItems.
fn capture_links_schema_supported(tool: &Value) -> bool {
    let Some(links) = tool
        .get("inputSchema")
        .and_then(|schema| schema.get("properties"))
        .and_then(|properties| properties.get("links"))
    else {
        return false;
    };
    if links.get("type").and_then(Value::as_str) != Some("array") {
        return false;
    }
    let Some(items) = links.get("items") else {
        return false;
    };
    if items.get("type").and_then(Value::as_str) != Some("object")
        || !items
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| required.iter().any(|name| name.as_str() == Some("to")))
        || items
            .get("properties")
            .and_then(|properties| properties.get("to"))
            .and_then(|to| to.get("type"))
            .and_then(Value::as_str)
            != Some("string")
    {
        return false;
    }
    let properties = &items["properties"];
    let Some(kind) = properties.get("kind") else {
        return false;
    };
    if kind.get("type").and_then(Value::as_str) != Some("string") {
        return false;
    }
    let Some(variants) = kind.get("enum").and_then(Value::as_array) else {
        return false;
    };
    if !["associative", "transition", "derived_from"]
        .iter()
        .all(|expected| {
            variants
                .iter()
                .any(|value| value.as_str() == Some(expected))
        })
    {
        return false;
    }
    properties
        .get("weight")
        .and_then(|weight| weight.get("type"))
        .and_then(Value::as_str)
        == Some("number")
}

fn episode_action_advertised(tool: &Value, action: &str) -> bool {
    let schema = &tool["inputSchema"]["properties"]["action"];
    schema.get("type").and_then(Value::as_str) == Some("string")
        && schema
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|actions| actions.iter().any(|value| value.as_str() == Some(action)))
}

fn schema_requires(schema: &Value, name: &str) -> bool {
    schema["required"]
        .as_array()
        .is_some_and(|fields| fields.iter().any(|field| field.as_str() == Some(name)))
}

/// Check the grouped action and recognizable closed native payload, not merely
/// a callable name on a peer which might ignore nested fields.
fn concern_action_advertised(tool: &Value, action: &str) -> bool {
    if !matches!(action, "list" | "notice" | "record_finding")
        || !episode_action_advertised(tool, action)
    {
        return false;
    }
    let schema = &tool["inputSchema"];
    if schema["type"] != "object" || schema["additionalProperties"] != false {
        return false;
    }
    let Some(branch) = schema["oneOf"].as_array().and_then(|branches| {
        branches
            .iter()
            .find(|branch| branch["properties"]["action"]["const"].as_str() == Some(action))
    }) else {
        return false;
    };
    let guarded_owner = (schema_requires(branch, "db")
        && schema_requires(branch, "expected_db_id"))
        || schema["allOf"].as_array().is_some_and(|guards| {
            guards.iter().any(|guard| {
                schema_requires(&guard["if"], "action")
                    && guard["if"]["properties"]["action"]["enum"]
                        .as_array()
                        .is_some_and(|actions| {
                            actions.iter().any(|value| value.as_str() == Some(action))
                        })
                    && schema_requires(&guard["then"], "db")
                    && schema_requires(&guard["then"], "expected_db_id")
            })
        });
    let props = &schema["properties"];
    let closed =
        |value: &Value| value["type"] == "object" && value["additionalProperties"] == false;
    let notice = &props["notice"];
    let finding = &props["finding"];
    let notice_ok = closed(notice)
        && ["binding", "concern", "missing_fact"]
            .iter()
            .all(|field| schema_requires(notice, field))
        && closed(&notice["properties"]["binding"])
        && notice["properties"]["concern"]["type"] == "string"
        && notice["properties"]["missing_fact"]["type"] == "string";
    match action {
        "list" => {
            schema_requires(branch, "endpoint")
                && props["endpoint"]["type"] == "string"
                && props["limit"]["type"] == "integer"
                && props["limit"]["minimum"] == 1
                && props["limit"]["maximum"]
                    .as_u64()
                    .is_some_and(|max| max > 0)
        }
        "notice" => notice_ok && schema_requires(branch, "notice") && guarded_owner,
        "record_finding" => {
            notice_ok
                && schema_requires(branch, "expected")
                && schema_requires(branch, "finding")
                && guarded_owner
                && closed(&props["expected"])
                && schema_requires(&props["expected"], "notice")
                && schema_requires(&props["expected"], "finding")
                && closed(finding)
                && ["scope", "observation", "evidence"]
                    .iter()
                    .all(|field| schema_requires(finding, field))
                && finding["properties"]["evidence"]["type"] == "array"
        }
        _ => false,
    }
}

/// SAVE owns a root union schema; kind branches only refine that schema.
/// A recognizable kind and explicit database owner are required, not just a
/// callable name on a peer that might ignore unknown input fields.
fn save_kind_advertised(tool: &Value, kind: &str) -> bool {
    let schema = &tool["inputSchema"];
    matches!(kind, "note" | "episode")
        && schema["properties"]["kind"]["type"].as_str() == Some("string")
        && schema["properties"]["kind"]["enum"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|value| value.as_str() == Some(kind)))
        && schema["properties"]["db"]["type"].as_str() == Some("string")
        && schema["required"]
            .as_array()
            .is_some_and(|required| required.iter().any(|value| value.as_str() == Some("db")))
}

/// Recognize the complete compare-and-replace contract, never a legacy tag edit.
fn retag_advertised(tool: &Value) -> bool {
    let schema = &tool["inputSchema"];
    let props = &schema["properties"];
    schema["type"] == "object"
        && schema["additionalProperties"] == false
        && ["db", "expected_db_id", "id", "expected_tags", "tags"]
            .iter()
            .all(|field| schema_requires(schema, field))
        && props["db"]["type"] == "string"
        && expected_db_id_schema_supported(schema)
        && props["id"]["type"] == "string"
        && props["id"]["minLength"] == 26
        && props["id"]["maxLength"] == 26
        && ["expected_tags", "tags"].iter().all(|field| {
            let tags = &props[field];
            tags["type"] == "array"
                && tags["maxItems"] == 64
                && tags["uniqueItems"] == true
                && tags.get("minItems").is_none_or(|value| value == 0)
                && tags["items"]["type"] == "string"
                && tags["items"]["minLength"] == 1
                && tags["items"]["maxLength"] == 256
        })
}

/// Recognize the guarded body-only replacement contract, not a generic edit.
fn edit_body_advertised(tool: &Value) -> bool {
    let schema = &tool["inputSchema"];
    let props = &schema["properties"];
    schema["type"] == "object"
        && schema["additionalProperties"] == false
        && [
            "db",
            "expected_db_id",
            "id",
            "expected_body_revision",
            "body",
        ]
        .iter()
        .all(|field| schema_requires(schema, field))
        && props["db"]["type"] == "string"
        && expected_db_id_schema_supported(schema)
        && props["id"]["type"] == "string"
        && props["id"]["minLength"] == 26
        && props["id"]["maxLength"] == 26
        && props["id"]["pattern"] == "^[0-7][0-9A-HJKMNP-TV-Z]{25}$"
        && props["expected_body_revision"]["type"] == "string"
        && props["expected_body_revision"]["minLength"] == 64
        && props["expected_body_revision"]["maxLength"] == 64
        && props["expected_body_revision"]["pattern"] == "^[0-9a-f]{64}$"
        && props["body"]["type"] == "string"
        && props["body"]["maxLength"] == 262144
        && props["body"]
            .get("minLength")
            .is_none_or(|value| value == 0)
}

/// Recognize only the checked semantic summary replacement contract.
fn edit_summary_advertised(tool: &Value) -> bool {
    let schema = &tool["inputSchema"];
    let props = &schema["properties"];
    schema["type"] == "object"
        && schema["additionalProperties"] == false
        && [
            "db",
            "expected_db_id",
            "id",
            "expected_snapshot_sha256",
            "summary",
        ]
        .iter()
        .all(|field| schema_requires(schema, field))
        && props["db"]["type"] == "string"
        && expected_db_id_schema_supported(schema)
        && props["id"]["type"] == "string"
        && props["id"]["minLength"] == 26
        && props["id"]["maxLength"] == 26
        && props["id"]["pattern"] == "^[0-7][0-9A-HJKMNP-TV-Z]{25}$"
        && props["expected_snapshot_sha256"]["type"] == "string"
        && props["expected_snapshot_sha256"]["minLength"] == 64
        && props["expected_snapshot_sha256"]["maxLength"] == 64
        && props["expected_snapshot_sha256"]["pattern"] == "^[0-9a-f]{64}$"
        && props["summary"]["type"] == "string"
        && props["summary"]["maxLength"] == 16384
        && props["summary"]["minLength"] == 1
}

/// A guard is effective only if the peer advertises the complete canonical
/// database-ID contract. A field name alone may be silently ignored by old peers.
fn expected_db_id_schema_supported(schema: &Value) -> bool {
    let guard = &schema["properties"]["expected_db_id"];
    guard.get("type").and_then(Value::as_str) == Some("string")
        && guard.get("minLength").and_then(Value::as_u64) == Some(26)
        && guard.get("maxLength").and_then(Value::as_u64) == Some(26)
        && guard.get("pattern").and_then(Value::as_str) == Some("^[0-7][0-9A-HJKMNP-TV-Z]{25}$")
}

fn action_branch<'a>(schema: &'a Value, action: &str) -> Option<&'a Value> {
    let mut matches = schema["oneOf"]
        .as_array()?
        .iter()
        .filter(|branch| branch["properties"]["action"]["const"].as_str() == Some(action));
    let branch = matches.next()?;
    matches.next().is_none().then_some(branch)
}

fn composed_guard_schema_supported(schema: &Value) -> bool {
    let root = expected_db_id_schema_supported(schema);
    // A present malformed field must not be hidden by a valid sibling branch.
    if schema["properties"].get("expected_db_id").is_some() && !root {
        return false;
    }
    let Some(raw_branches) = schema.get("oneOf") else {
        return root;
    };
    let Some(branches) = raw_branches.as_array() else {
        return false;
    };
    !branches.is_empty()
        && branches.iter().all(|branch| {
            if branch["properties"].get("expected_db_id").is_some() {
                expected_db_id_schema_supported(branch)
            } else {
                root
            }
        })
}

pub struct ConnectionOptions {
    pub url: String,
    pub ssh_mcp_port: u16,
    pub token_env: Option<String>,
}

pub struct RemoteClient {
    http: reqwest::Client,
    endpoint: Url,
    token: Option<String>,
    session: Option<String>,
    protocol: Option<String>,
    next_id: u64,
    advertised: HashSet<String>,
    catalog: Vec<Value>,
    initialize_result: Value,
    server_name: String,
    capture_links_supported: bool,
    ssh: Option<Child>,
    request_timeout: Duration,
    wire_policy: WirePolicy,
}

impl RemoteClient {
    /// Feature discovery for optional read-only client affordances.
    pub fn advertises(&self, name: &str) -> bool {
        self.advertised.contains(name)
    }

    /// Feature discovery for linked capture. Older servers may accept unknown
    /// JSON fields without applying them, so tool-name presence is insufficient.
    pub fn supports_capture_links(&self) -> bool {
        self.capture_links_supported
    }

    /// Episode actions are profile-sensitive. A tool name alone does not say
    /// whether this server advertises an editorial write or only bounded reads.
    /// This is discovery, not authority: the server still authorizes every call.
    pub fn supports_episode_action(&self, action: &str) -> bool {
        self.catalog.iter().any(|tool| {
            tool.get("name").and_then(Value::as_str) == Some("episode")
                && episode_action_advertised(tool, action)
        })
    }

    pub fn supports_concern_action(&self, action: &str) -> bool {
        self.catalog
            .iter()
            .any(|tool| tool["name"] == "concern" && concern_action_advertised(tool, action))
    }

    pub fn supports_save_kind(&self, kind: &str) -> bool {
        self.catalog
            .iter()
            .any(|tool| tool["name"].as_str() == Some("save") && save_kind_advertised(tool, kind))
    }

    /// Complete ordinary SAVE discovery. Servers still authorize each call.
    pub fn supports_save(&self) -> bool {
        self.supports_save_kind("note") && self.supports_save_kind("episode")
    }

    pub fn supports_save_links(&self, kind: &str) -> bool {
        self.catalog.iter().any(|tool| {
            tool["name"].as_str() == Some("save")
                && save_kind_advertised(tool, kind)
                && capture_links_schema_supported(tool)
        })
    }

    /// A complete guarded tag-set CAS contract is required before any write.
    pub fn supports_retag(&self) -> bool {
        self.catalog
            .iter()
            .any(|tool| tool["name"] == "retag" && retag_advertised(tool))
    }

    /// Full guarded body-edit advertisement is required; unsupported peers never receive writes.
    pub fn supports_edit_body(&self) -> bool {
        self.catalog
            .iter()
            .any(|tool| tool["name"] == "edit_body" && edit_body_advertised(tool))
    }

    pub fn supports_edit_summary(&self) -> bool {
        self.catalog
            .iter()
            .any(|tool| tool["name"] == "edit_summary" && edit_summary_advertised(tool))
    }

    /// Discover a canonical identity precondition on a known DB-scoped tool.
    /// Union branches may inherit a root guard; without a root, every branch
    /// must advertise it. Episode retains its stronger per-action contract.
    /// Discovery does not replace server-side authorization or identity checks.
    pub fn supports_expected_db_id(&self, name: &str, action: Option<&str>) -> bool {
        match (name, action) {
            ("episode", Some(action)) if self.supports_episode_action(action) => {}
            ("concern", Some(action)) if self.supports_concern_action(action) => {}
            ("save", Some(kind)) if self.supports_save_kind(kind) => {}
            ("retag", None) if self.supports_retag() => {}
            ("edit_body", None) if self.supports_edit_body() => {}
            ("edit_summary", None) if self.supports_edit_summary() => {}
            (
                "walk",
                Some("start" | "look" | "edges" | "body" | "go" | "back" | "done" | "abort"),
            ) => {}
            ("database_control", Some("status" | "release" | "resume")) => {}
            ("graph", Some("topology" | "summaries")) => {}
            (
                "capture" | "get" | "list" | "status" | "query" | "recall_context" | "recall"
                | "core" | "neighbors" | "remote_edges" | "contradictions" | "merges" | "ingest"
                | "link" | "supersede" | "contradict" | "reconcile" | "feedback" | "merge"
                | "forget" | "decay" | "prune" | "snapshot_create" | "reflect",
                None,
            ) => {}
            _ => return false,
        }
        self.catalog.iter().any(|tool| {
            if tool["name"].as_str() != Some(name) {
                return false;
            }
            let schema = &tool["inputSchema"];
            match name {
                "episode" => {
                    expected_db_id_schema_supported(schema)
                        && action_branch(schema, action.unwrap())
                            .is_some_and(expected_db_id_schema_supported)
                }
                "save" | "concern" => expected_db_id_schema_supported(schema),
                "walk" | "database_control" | "graph" => {
                    episode_action_advertised(tool, action.unwrap())
                        && (schema.get("oneOf").is_none()
                            || action_branch(schema, action.unwrap()).is_some())
                        && composed_guard_schema_supported(schema)
                }
                _ => composed_guard_schema_supported(schema),
            }
        })
    }

    /// The validated initialize result and complete, paginated tool catalog.
    pub fn initialize_result(&self) -> &Value {
        &self.initialize_result
    }
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
    pub fn tool_catalog(&self) -> &[Value] {
        &self.catalog
    }

    /// Restricted local connection for passive Codex hooks. The caller may
    /// choose shorter deadlines, but cannot exceed thirty seconds.
    pub async fn connect_local(
        options: &ConnectionOptions,
        timeouts: ClientTimeouts,
        expected_server: &str,
    ) -> Result<Self, AnyErr> {
        let expected = match expected_server {
            "mneme-mcp" => "mneme-mcp",
            "mneme-mcp-library" => "mneme-mcp-library",
            _ => return Err("unsupported expected local MCP server identity".into()),
        };
        if timeouts.connect.is_zero()
            || timeouts.request.is_zero()
            || timeouts.connect > Duration::from_secs(30)
            || timeouts.request > Duration::from_secs(30)
        {
            return Err("local MCP deadlines must be nonzero and at most 30 seconds".into());
        }
        tokio::time::timeout(
            timeouts.connect,
            Self::connect_inner_with_policy(
                options,
                timeouts,
                WirePolicy {
                    request_bytes: CODEX_REQUEST_BYTES,
                    response_bytes: CODEX_RESPONSE_BYTES,
                    expected_server: Some(expected),
                },
            ),
        )
        .await
        .map_err(|_| -> AnyErr {
            Box::new(AvailabilityError("local MCP connection timed out".into()))
        })?
    }

    pub async fn connect(options: &ConnectionOptions) -> Result<Self, AnyErr> {
        Self::connect_with_timeouts(options, ClientTimeouts::default()).await
    }

    pub async fn connect_with_timeouts(
        options: &ConnectionOptions,
        timeouts: ClientTimeouts,
    ) -> Result<Self, AnyErr> {
        if timeouts.connect.is_zero() || timeouts.request.is_zero() {
            return Err("remote MCP timeouts must be nonzero".into());
        }
        tokio::time::timeout(timeouts.connect, Self::connect_inner(options, timeouts))
            .await
            .map_err(|_| -> AnyErr {
                Box::new(AvailabilityError("remote MCP connection timed out".into()))
            })?
    }

    async fn connect_inner(
        options: &ConnectionOptions,
        timeouts: ClientTimeouts,
    ) -> Result<Self, AnyErr> {
        Self::connect_inner_with_policy(options, timeouts, WirePolicy::default()).await
    }

    async fn connect_inner_with_policy(
        options: &ConnectionOptions,
        timeouts: ClientTimeouts,
        wire_policy: WirePolicy,
    ) -> Result<Self, AnyErr> {
        let parsed = Url::parse(&options.url).map_err(|_| "invalid remote URL")?;
        if wire_policy.expected_server.is_some() && !numeric_loopback_http(&parsed) {
            return Err("local MCP endpoint must be numeric loopback HTTP".into());
        }
        let token = match options.token_env.as_deref() {
            Some(name) => {
                if name.is_empty() || name.contains('=') || name.contains('\0') {
                    return Err("invalid bearer-token environment variable name".into());
                }
                let value = std::env::var(name).map_err(|_| {
                    format!("bearer-token environment variable {name:?} is unset or not Unicode")
                })?;
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
                    return Err(format!(
                        "bearer-token environment variable {name:?} is empty or invalid"
                    )
                    .into());
                }
                Some(value)
            }
            None => None,
        };

        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeouts.request)
            .build()?;
        let (endpoint, ssh) = match parsed.scheme() {
            "http" | "https" => {
                validate_http_url(&parsed, token.is_some())?;
                (parsed, None)
            }
            "ssh" => {
                let (endpoint, child) = open_ssh_tunnel(&parsed, options.ssh_mcp_port).await?;
                (endpoint, Some(child))
            }
            _ => return Err("remote URL must use ssh://, http://, or https://".into()),
        };

        let mut remote = Self {
            http,
            endpoint,
            token,
            session: None,
            protocol: None,
            next_id: 1,
            advertised: HashSet::new(),
            catalog: Vec::new(),
            initialize_result: Value::Null,
            server_name: String::new(),
            capture_links_supported: false,
            ssh,
            request_timeout: timeouts.request,
            wire_policy,
        };
        if let Err(error) = remote.handshake().await {
            remote.close().await;
            return Err(error);
        }
        Ok(remote)
    }

    pub async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value, AnyErr> {
        let params = json!({ "name": name, "arguments": args });
        self.validate_tool_call(&params)?;
        let response = self.raw_rpc("tools/call", params).await;
        let result = match response {
            Ok(value) => decode_tool_result(&value),
            Err(error) => Err(error),
        };
        if result
            .as_ref()
            .err()
            .is_some_and(|error| classify_error(error.as_ref()) != ClientErrorClass::Tool)
        {
            // No automatic retry or re-initialize: a failed request may already
            // have mutated the remote store.
            self.close().await;
        }
        result
    }

    /// Raw MCP result on the same session as `call_tool`. Tool-level
    /// `isError` is deliberately preserved for forwarding clients.
    pub async fn raw_rpc(&mut self, method: &str, params: Value) -> Result<Value, AnyErr> {
        match method {
            "ping" | "tools/list" => {
                if !params.is_object() {
                    return Err("remote MCP RPC params must be an object".into());
                }
            }
            "tools/call" => self.validate_tool_call(&params)?,
            _ => return Err("remote MCP RPC method is not allowlisted".into()),
        }
        let result = tokio::time::timeout(self.request_timeout, self.request(method, params))
            .await
            .unwrap_or_else(|_| {
                Err(Box::new(AvailabilityError(
                    "remote MCP RPC timed out".into(),
                )))
            });
        if result.is_err() {
            self.close().await;
        }
        result
    }

    fn validate_tool_call(&self, params: &Value) -> Result<(), AnyErr> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or("remote MCP tools/call missing name")?;
        if !self.advertised.contains(name) {
            return Err(format!("remote MCP server does not advertise tool {name:?}").into());
        }
        let args = params
            .get("arguments")
            .ok_or("remote MCP tools/call missing arguments")?;
        if !args.is_object() {
            return Err("remote tool arguments must be a JSON object".into());
        }
        if name == "capture" && args.get("links").is_some() && !self.capture_links_supported {
            return Err("remote MCP capture.links is not advertised in the tool schema".into());
        }
        if name == "episode"
            && !args
                .get("action")
                .and_then(Value::as_str)
                .is_some_and(|action| self.supports_episode_action(action))
        {
            return Err("remote MCP episode action is not advertised in the tool schema".into());
        }
        if name == "concern" {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .ok_or("remote MCP concern requires an action")?;
            if !self.supports_concern_action(action) {
                return Err("remote MCP concern action/checked contract is not advertised; no legacy fallback".into());
            }
            if action != "list"
                && (args
                    .get("db")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                    || args.get("expected_db_id").is_none())
            {
                return Err("remote MCP concern mutation requires explicit db and expected_db_id; write was not sent".into());
            }
        }
        if name == "edit_body" {
            if !self.supports_edit_body() {
                return Err("remote MCP edit_body checked contract is not advertised; write was not sent, no legacy fallback".into());
            }
            if args
                .get("db")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
                || args
                    .get("expected_db_id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                return Err("remote MCP edit_body requires explicit db and expected_db_id; write was not sent".into());
            }
        }
        if name == "edit_summary" {
            if !self.supports_edit_summary() {
                return Err("remote MCP edit_summary checked contract is not advertised; write was not sent, no legacy fallback".into());
            }
            if args
                .get("db")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
                || args
                    .get("expected_db_id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                return Err("remote MCP edit_summary requires explicit db and expected_db_id; write was not sent".into());
            }
        }
        if name == "retag" {
            if !self.supports_retag() {
                return Err("remote MCP retag checked contract is not advertised; write was not sent, no legacy fallback".into());
            }
            if args
                .get("db")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
                || args
                    .get("expected_db_id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                return Err(
                    "remote MCP retag requires explicit db and expected_db_id; write was not sent"
                        .into(),
                );
            }
        }
        if name == "save" {
            let kind = match args.get("kind") {
                None => "note",
                Some(Value::String(kind)) => kind,
                Some(_) => return Err("remote MCP save kind must be a string".into()),
            };
            if !self.supports_save_kind(kind) {
                return Err("remote MCP save kind is not advertised in the tool schema".into());
            }
            if args
                .get("db")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err("remote MCP save requires an explicit database owner".into());
            }
            if args.get("links").is_some() && !self.supports_save_links(kind) {
                return Err("remote MCP save.links is not advertised in the tool schema".into());
            }
        }
        let action = match name {
            "episode" | "concern" | "walk" | "database_control" | "graph" => {
                args.get("action").and_then(Value::as_str)
            }
            "save" => Some(args.get("kind").and_then(Value::as_str).unwrap_or("note")),
            _ => None,
        };
        if args.get("expected_db_id").is_some() && !self.supports_expected_db_id(name, action) {
            return Err(format!(
                "remote MCP {name}.expected_db_id is not advertised for this tool/action; refusing an unguarded call"
            )
            .into());
        }
        Ok(())
    }

    pub async fn close(&mut self) {
        if let Some(session) = self.session.take() {
            let mut request = self
                .http
                .delete(self.endpoint.clone())
                .header("mcp-session-id", session);
            if let Some(protocol) = self.protocol.take() {
                request = request.header("mcp-protocol-version", protocol);
            }
            if let Some(token) = &self.token {
                request = request.bearer_auth(token);
            }
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, request.send()).await;
        }
        self.protocol = None;
        self.stop_ssh();
    }

    async fn handshake(&mut self) -> Result<(), AnyErr> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "mnemed-remote", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        let protocol = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or("remote MCP initialize omitted protocolVersion")?;
        if !SUPPORTED_PROTOCOLS.contains(&protocol) {
            return Err(format!("remote MCP protocol {protocol:?} is unsupported").into());
        }
        let server_name = result
            .pointer("/serverInfo/name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty());
        if let Some(expected) = self.wire_policy.expected_server {
            if server_name != Some(expected) {
                return Err(
                    format!("remote MCP server identity mismatch: expected {expected:?}").into(),
                );
            }
        }
        self.server_name = server_name.unwrap_or_default().to_owned();
        self.initialize_result = result.clone();
        if self.session.is_none() {
            return Err("remote MCP initialize omitted mcp-session-id".into());
        }
        self.protocol = Some(protocol.to_owned());
        self.notify("notifications/initialized", json!({})).await?;

        let mut cursor: Option<String> = None;
        for _ in 0..MAX_CATALOG_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |c| json!({ "cursor": c }));
            let page = self.request("tools/list", params).await?;
            let tools = page
                .get("tools")
                .and_then(Value::as_array)
                .ok_or("remote MCP tools/list omitted tools array")?;
            if self.advertised.len().saturating_add(tools.len()) > MAX_TOOLS {
                return Err("remote MCP tool catalog exceeds limit".into());
            }
            for tool in tools {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or("remote MCP tool catalog contains an unnamed tool")?;
                if !self.advertised.insert(name.to_owned()) {
                    return Err("remote MCP tool catalog contains a duplicate tool".into());
                }
                if name == "capture" {
                    self.capture_links_supported = capture_links_schema_supported(tool);
                }
                self.catalog.push(tool.clone());
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                return Ok(());
            }
        }
        Err("remote MCP tool catalog has too many pages".into())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, AnyErr> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or("remote MCP request ID exhausted")?;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let (headers, response) = self.post_json(&msg).await?;
        if method == "initialize" {
            let session = headers
                .get("mcp-session-id")
                .ok_or("remote MCP initialize omitted mcp-session-id")?
                .to_str()
                .map_err(|_| "remote MCP session ID is not valid text")?;
            if session.is_empty()
                || session.len() > 256
                || !session.bytes().all(|b| b.is_ascii_graphic())
            {
                return Err("remote MCP session ID is invalid".into());
            }
            self.session = Some(session.to_owned());
        }
        let msg = response.ok_or("remote MCP request returned no JSON-RPC response")?;
        decode_rpc_response(msg, id)
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), AnyErr> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let (_, response) = self.post_json(&msg).await?;
        if response.is_some() {
            return Err("remote MCP notification unexpectedly returned a JSON-RPC response".into());
        }
        Ok(())
    }

    async fn post_json(
        &self,
        msg: &Value,
    ) -> Result<(reqwest::header::HeaderMap, Option<Value>), AnyErr> {
        let body = serde_json::to_vec(msg)?;
        if body.len() > self.wire_policy.request_bytes {
            return Err("remote MCP request exceeds wire limit".into());
        }
        let mut request = self
            .http
            .post(self.endpoint.clone())
            .header(ACCEPT, ACCEPT_MCP)
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(session) = &self.session {
            request = request.header("mcp-session-id", session);
        }
        if let Some(protocol) = &self.protocol {
            request = request.header("mcp-protocol-version", protocol);
        }
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let mut response = request
            .send()
            .await
            .map_err(|error| request_error("remote MCP request failed", &error))?;
        let status = response.status();
        if !status.is_success() {
            let reason = match status.as_u16() {
                401 | 403 => "authentication or origin denied; check the remote token and endpoint",
                404 => "endpoint or MCP session not found; reconnect before retrying manually",
                406 => "server rejected MCP Accept types",
                413 => "request exceeds the remote server limit",
                429 | 503 => "remote server is busy; retry manually if safe",
                _ => "remote server rejected the request",
            };
            let message = format!("remote MCP HTTP {status}: {reason}");
            return if http_is_availability(status) {
                Err(Box::new(AvailabilityError(message)))
            } else {
                Err(message.into())
            };
        }
        if status == reqwest::StatusCode::ACCEPTED || status == reqwest::StatusCode::NO_CONTENT {
            return Ok((response.headers().clone(), None));
        }
        let headers = response.headers().clone();
        let mime = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_owned();
        if mime != "application/json" && mime != "text/event-stream" {
            return Err("remote MCP response has an unsupported content type".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| request_error("remote MCP response failed", &error))?
        {
            if bytes.len().saturating_add(chunk.len()) > self.wire_policy.response_bytes {
                return Err("remote MCP response exceeds wire limit".into());
            }
            bytes.extend_from_slice(&chunk);
            if mime == "text/event-stream" {
                if let Some(value) = parse_sse(&bytes)? {
                    return Ok((headers, Some(value)));
                }
            }
        }
        if mime == "text/event-stream" {
            return Err("remote MCP event stream ended without a response".into());
        }
        let value =
            serde_json::from_slice(&bytes).map_err(|_| "remote MCP response is invalid JSON")?;
        Ok((headers, Some(value)))
    }

    fn stop_ssh(&mut self) {
        if let Some(mut child) = self.ssh.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for RemoteClient {
    fn drop(&mut self) {
        self.stop_ssh();
    }
}

fn validate_http_url(url: &Url, bearer: bool) -> Result<(), AnyErr> {
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err(
            "remote HTTP URL must have a host and no credentials, query, or fragment".into(),
        );
    }
    if url.scheme() == "http" && bearer {
        let loopback = match url.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if !loopback {
            return Err(
                "refusing a bearer token over non-loopback cleartext HTTP; use HTTPS or SSH".into(),
            );
        }
    }
    Ok(())
}

fn numeric_loopback_http(url: &Url) -> bool {
    url.scheme() == "http"
        && match url.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        }
}

async fn open_ssh_tunnel(url: &Url, remote_port: u16) -> Result<(Url, Child), AnyErr> {
    if remote_port == 0
        || url.host_str().is_none()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(
            "ssh remote must be ssh://[user@]host[:ssh-port] with no path, query, or fragment"
                .into(),
        );
    }
    let host = match url.host().expect("checked host") {
        Host::Ipv6(ip) => ip.to_string(),
        _ => url.host_str().expect("checked host").to_owned(),
    };
    if host.starts_with('-') || host.contains('%') || host.chars().any(char::is_whitespace) {
        return Err("invalid SSH host or alias".into());
    }
    let username = url.username();
    if !username.is_empty()
        && !username
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err("invalid SSH username".into());
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let local_port = listener.local_addr()?.port();
    drop(listener);

    let mut args = vec![
        "-N",
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ForkAfterAuthentication=no",
        "-o",
        "PermitLocalCommand=no",
        "-o",
        "StrictHostKeyChecking=yes",
    ];
    let port_string = url.port().map(|port| port.to_string());
    if port_string.is_some() {
        args.extend(["-p", port_string.as_deref().expect("port exists")]);
    }
    if !username.is_empty() {
        args.extend(["-l", username]);
    }
    let forwarding = format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}");
    args.extend(["-L", &forwarding, "--", &host]);
    // `ClearAllForwardings=yes` would also erase this command-line -L. Use
    // OpenSSH's effective configuration to reject any additional forwards from
    // user config instead, while retaining Host aliases and IdentityFile.
    let mut inspect = tokio::process::Command::new("ssh");
    inspect.arg("-G").args(&args).kill_on_drop(true);
    let config = tokio::time::timeout(Duration::from_secs(3), inspect.output())
        .await
        .map_err(|_| "SSH configuration inspection timed out")?
        .map_err(|error| format!("could not inspect SSH configuration: {error}"))?;
    if !config.status.success() {
        return Err("SSH configuration could not be inspected".into());
    }
    let effective =
        String::from_utf8(config.stdout).map_err(|_| "SSH configuration is not UTF-8")?;
    let forwards: Vec<_> = effective
        .lines()
        .filter(|line| line.starts_with("localforward "))
        .collect();
    let expected_listen = format!("[127.0.0.1]:{local_port}");
    let expected_target = format!("[127.0.0.1]:{remote_port}");
    if forwards.len() != 1
        || !forwards[0].contains(&expected_listen)
        || !forwards[0].contains(&expected_target)
        || effective
            .lines()
            .any(|line| line.starts_with("remoteforward ") || line.starts_with("dynamicforward "))
    {
        return Err("SSH configuration adds or removes port forwarding; this connection permits only its loopback MCP tunnel".into());
    }
    let mut command = Command::new("ssh");
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command
        .spawn()
        .map_err(|error| format!("could not start ssh: {error}"))?;
    // If the connect future is cancelled (including Ctrl-C), the child is
    // killed and reaped even before it has been transferred into RemoteClient.
    let mut child = SshGuard(Some(child));
    let deadline = Instant::now() + SSH_READY_TIMEOUT;
    loop {
        match child.0.as_mut().expect("SSH guard owns child").try_wait() {
            Ok(Some(_)) => {
                return Err("SSH tunnel exited before forwarding became ready; check host key, credentials, and remote host".into());
            }
            Err(error) => {
                return Err(format!("could not monitor SSH tunnel: {error}").into());
            }
            Ok(None) => {}
        }
        if tokio::time::timeout_at(
            deadline,
            tokio::net::TcpStream::connect(("127.0.0.1", local_port)),
        )
        .await
        .is_ok_and(|result| result.is_ok())
        {
            // ExitOnForwardFailure reports a failed bind via process exit.
            match child.0.as_mut().expect("SSH guard owns child").try_wait() {
                Ok(None) => {
                    let endpoint = Url::parse(&format!("http://127.0.0.1:{local_port}/"))?;
                    return Ok((endpoint, child.0.take().expect("SSH guard owns child")));
                }
                Ok(Some(_)) => {}
                Err(error) => {
                    return Err(format!("could not monitor SSH tunnel: {error}").into());
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(Box::new(AvailabilityError(
                "SSH tunnel did not become ready within 10 seconds".into(),
            )));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

struct SshGuard(Option<Child>);

impl Drop for SshGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn decode_rpc_response(response: Value, id: u64) -> Result<Value, AnyErr> {
    if response.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || response.get("id").and_then(Value::as_u64) != Some(id)
    {
        return Err("remote MCP response has an invalid JSON-RPC version or request ID".into());
    }
    match (response.get("result"), response.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(error)) => {
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .ok_or("remote MCP error has no code")?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or("remote MCP error has no message")?;
            Err(format!("remote MCP error {code}: {}", safe_error(message)).into())
        }
        _ => Err("remote MCP response must contain exactly one of result or error".into()),
    }
}

fn decode_tool_result(result: &Value) -> Result<Value, AnyErr> {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let message = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("tool failed");
        return Err(Box::new(ToolError(format!(
            "remote MCP tool failed: {}",
            safe_error(message)
        ))));
    }
    if result.get("isError").is_some() && result.get("isError").and_then(Value::as_bool).is_none() {
        return Err("remote MCP tool result has invalid isError".into());
    }
    if let Some(structured) = result.get("structuredContent") {
        return Ok(structured.clone());
    }
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or("remote MCP tool result has no content array")?;
    if content.len() != 1 || content[0].get("type").and_then(Value::as_str) != Some("text") {
        return Err("remote MCP tool result needs exactly one JSON text block".into());
    }
    let text = content[0]
        .get("text")
        .and_then(Value::as_str)
        .ok_or("remote MCP text block is missing text")?;
    if text.len() > MAX_TOOL_TEXT_BYTES {
        return Err("remote MCP tool text exceeds 256 KiB".into());
    }
    serde_json::from_str(text).map_err(|_| "remote MCP tool text is not JSON".into())
}

fn parse_sse(bytes: &[u8]) -> Result<Option<Value>, AnyErr> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if error.error_len().is_none() => return Ok(None),
        Err(_) => return Err("remote MCP event stream is not UTF-8".into()),
    };
    let normalized = text.replace("\r\n", "\n");
    let mut frames = normalized.split("\n\n");
    // The final segment has no blank-line terminator yet and is not an event.
    while let Some(frame) = frames.next() {
        if frames.clone().next().is_none() {
            break;
        }
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(part) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(part.strip_prefix(' ').unwrap_or(part));
            }
        }
        if data.is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(&data).map_err(|_| "remote MCP event contains invalid JSON")?;
        if value.get("id").is_some() {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn safe_error(message: &str) -> String {
    let mut out = String::new();
    for ch in message.chars().filter(|ch| !ch.is_control()).take(512) {
        out.push(ch);
    }
    if message.chars().count() > 512 {
        out.push('…');
    }
    out
}

fn http_is_availability(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 503)
}

fn request_error(context: &str, error: &reqwest::Error) -> AnyErr {
    let message = format!("{context}: {}", classify_reqwest_error(error));
    // reqwest's is_connect() includes the entire connector, notably TLS
    // certificate/peer-identity failures. A snapshot fallback must not hide
    // those as mere owner unavailability. Require a concrete transport I/O
    // cause; unknown connector failures are conservatively refusals.
    if error.is_timeout() || (error.is_connect() && transport_io_unavailable(error)) {
        Box::new(AvailabilityError(message))
    } else {
        message.into()
    }
}

fn transport_io_unavailable(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(err) = current {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::AddrNotAvailable
                    | std::io::ErrorKind::NetworkUnreachable
                    | std::io::ErrorKind::HostUnreachable
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::TimedOut
            );
        }
        current = err.source();
    }
    false
}

fn classify_reqwest_error(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "connection setup or peer identity failed"
    } else {
        "connection or response failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::future::Future;
    use std::io::{Read, Write};

    #[test]
    fn concern_catalog_is_closed_action_specific_and_owner_guarded() {
        let notice = json!({"type":"object","additionalProperties":false,"required":["binding","concern","missing_fact"],"properties":{
            "binding":{"type":"object","additionalProperties":false},"concern":{"type":"string"},"missing_fact":{"type":"string"}}});
        let mut tool = json!({"name":"concern","inputSchema":{"type":"object","additionalProperties":false,"properties":{
            "action":{"type":"string","enum":["list","notice","record_finding"]},
            "endpoint":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":2},
            "notice":notice,"expected":{"type":"object","additionalProperties":false,"required":["notice","finding"]},
            "finding":{"type":"object","additionalProperties":false,"required":["scope","observation","evidence"],"properties":{"evidence":{"type":"array"}}}},
            "oneOf":[{"properties":{"action":{"const":"list"}},"required":["endpoint"]},
                {"properties":{"action":{"const":"notice"}},"required":["notice"]},
                {"properties":{"action":{"const":"record_finding"}},"required":["expected","finding"]}],
            "allOf":[{"if":{"properties":{"action":{"enum":["notice","record_finding"]}},"required":["action"]},"then":{"required":["db","expected_db_id"]}}]}});
        for action in ["list", "notice", "record_finding"] {
            assert!(concern_action_advertised(&tool, action));
        }
        assert!(!concern_action_advertised(&tool, "defer"));
        let original = tool.clone();
        tool["inputSchema"]["allOf"] = json!([]);
        assert!(concern_action_advertised(&tool, "list"));
        assert!(!concern_action_advertised(&tool, "notice"));
        tool = original.clone();
        tool["inputSchema"]["additionalProperties"] = json!(true);
        assert!(!concern_action_advertised(&tool, "list"));
        tool = original;
        tool["inputSchema"]["properties"]["action"]["enum"] = json!(["list"]);
        assert!(concern_action_advertised(&tool, "list"));
        assert!(!concern_action_advertised(&tool, "notice"));
    }

    #[test]
    fn episode_action_discovery_is_profile_specific() {
        let mut tool = json!({"name":"episode","inputSchema":{"properties":{
            "action":{"type":"string","enum":["list","search","get","history","references"]}
        }}});
        assert!(episode_action_advertised(&tool, "get"));
        assert!(!episode_action_advertised(&tool, "append"));
        assert!(!episode_action_advertised(&tool, "revise"));
        tool["inputSchema"]["properties"]["action"]["enum"] = json!(["get", "append"]);
        assert!(episode_action_advertised(&tool, "append"));
        assert!(!episode_action_advertised(&tool, "revise"));
        tool["inputSchema"]["properties"]["action"]["type"] = json!("number");
        assert!(!episode_action_advertised(&tool, "append"));
        assert!(!episode_action_advertised(
            &json!({"name":"episode"}),
            "get"
        ));
    }

    #[test]
    fn linked_capture_catalog_requires_structural_schema_not_just_a_key() {
        let mut tool = json!({"inputSchema":{"properties":{"links":{
            "type":"array", "items":{"type":"object", "required":["to"],
            "properties":{"to":{"type":"string"},"kind":{"type":"string","enum":["transition","derived_from","associative"]},"weight":{"type":"number"}}}
        }}}});
        assert!(capture_links_schema_supported(&tool));
        tool["inputSchema"]["properties"]["links"]["items"]["required"] = json!([]);
        assert!(!capture_links_schema_supported(&tool));
        tool["inputSchema"]["properties"]["links"]["items"]["required"] = json!(["to"]);
        tool["inputSchema"]["properties"]["links"]["items"]["properties"]["to"]["type"] =
            json!("number");
        assert!(!capture_links_schema_supported(&tool));
        tool["inputSchema"]["properties"]["links"]["items"]["properties"]["to"]["type"] =
            json!("string");
        tool["inputSchema"]["properties"]["links"]["items"]["properties"]["weight"]["type"] =
            json!("string");
        assert!(!capture_links_schema_supported(&tool));
        tool["inputSchema"]["properties"]["links"]["items"]["properties"]["weight"]["type"] =
            json!("number");
        tool["inputSchema"]["properties"]["links"]["items"]["properties"]["kind"]["enum"] =
            json!(["associative"]);
        assert!(!capture_links_schema_supported(&tool));
        let to_only = json!({"inputSchema":{"properties":{"links":{
            "type":"array", "items":{"type":"object", "required":["to"],
            "properties":{"to":{"type":"string"}}}
        }}}});
        assert!(!capture_links_schema_supported(&to_only));
        let mut missing_weight = json!({"inputSchema":{"properties":{"links":{
            "type":"array", "items":{"type":"object", "required":["to"],
            "properties":{"to":{"type":"string"},"kind":{"type":"string","enum":["associative","transition","derived_from"]}}}
        }}}});
        assert!(!capture_links_schema_supported(&missing_weight));
        missing_weight["inputSchema"]["properties"]["links"]["items"]["properties"]["weight"] =
            json!({"type":"number"});
        assert!(capture_links_schema_supported(&missing_weight));
    }

    fn edit_body_catalog() -> Value {
        json!([{"name":"edit_body","inputSchema":{"type":"object","additionalProperties":false,
            "required":["db","expected_db_id","id","expected_body_revision","body"],"properties":{
                "db":{"type":"string"},"id":{"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"},
                "expected_db_id":{"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"},
                "expected_body_revision":{"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$"},"body":{"type":"string","maxLength":262144}}}}])
    }

    #[tokio::test]
    async fn edit_body_requires_complete_advertised_contract_and_owner_before_send() {
        let args = json!({"db":"project","expected_db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","expected_body_revision":"a".repeat(64),"body":""});
        let mut cases = vec![(json!([]), args.clone())];
        for field in ["expected_body_revision", "body", "expected_db_id"] {
            let mut old = edit_body_catalog();
            old[0]["inputSchema"]["required"]
                .as_array_mut()
                .unwrap()
                .retain(|value| value != field);
            cases.push((old, args.clone()));
        }
        let mut bad_guard = edit_body_catalog();
        bad_guard[0]["inputSchema"]["properties"]["expected_db_id"]["pattern"] = json!(".*");
        cases.push((bad_guard, args.clone()));
        for field in ["db", "expected_db_id"] {
            let mut missing = args.clone();
            missing.as_object_mut().unwrap().remove(field);
            cases.push((edit_body_catalog(), missing));
        }
        for (catalog, args) in cases {
            let (options, server) = fixture_server_with_catalog("mneme-mcp", catalog, Some(vec![]));
            let mut client = RemoteClient::connect_local(
                &options,
                ClientTimeouts {
                    connect: Duration::from_secs(2),
                    request: Duration::from_secs(2),
                },
                "mneme-mcp",
            )
            .await
            .unwrap();
            let before = client.next_id;
            for raw in [false, true] {
                let result = if raw {
                    client
                        .raw_rpc("tools/call", json!({"name":"edit_body","arguments":args}))
                        .await
                } else {
                    client.call_tool("edit_body", args.clone()).await
                };
                assert!(result.is_err());
                assert_eq!(client.next_id, before);
                assert!(client.session.is_some());
            }
            client.close().await;
            server.join().unwrap();
        }
    }

    fn edit_summary_catalog() -> Value {
        json!([{"name":"edit_summary","inputSchema":{"type":"object","additionalProperties":false,
            "required":["db","expected_db_id","id","expected_snapshot_sha256","summary"],"properties":{
                "db":{"type":"string"},"id":{"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"},
                "expected_db_id":{"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"},
                "expected_snapshot_sha256":{"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$"},"summary":{"type":"string","minLength":1,"maxLength":16384}}}}])
    }

    #[tokio::test]
    async fn edit_summary_requires_complete_advertised_contract_and_owner_before_send() {
        let args = json!({"db":"project","expected_db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","expected_snapshot_sha256":"a".repeat(64),"summary":"replacement"});
        let mut cases = vec![(json!([]), args.clone())];
        for field in ["expected_snapshot_sha256", "summary", "expected_db_id"] {
            let mut old = edit_summary_catalog();
            old[0]["inputSchema"]["required"]
                .as_array_mut()
                .unwrap()
                .retain(|value| value != field);
            cases.push((old, args.clone()));
        }
        let mut bad_guard = edit_summary_catalog();
        bad_guard[0]["inputSchema"]["properties"]["expected_db_id"]["pattern"] = json!(".*");
        cases.push((bad_guard, args.clone()));
        for field in ["db", "expected_db_id"] {
            let mut missing = args.clone();
            missing.as_object_mut().unwrap().remove(field);
            cases.push((edit_summary_catalog(), missing));
        }
        for (catalog, args) in cases {
            let (options, server) = fixture_server_with_catalog("mneme-mcp", catalog, Some(vec![]));
            let mut client = RemoteClient::connect_local(
                &options,
                ClientTimeouts {
                    connect: Duration::from_secs(2),
                    request: Duration::from_secs(2),
                },
                "mneme-mcp",
            )
            .await
            .unwrap();
            let before = client.next_id;
            for raw in [false, true] {
                let result = if raw {
                    client
                        .raw_rpc(
                            "tools/call",
                            json!({"name":"edit_summary","arguments":args}),
                        )
                        .await
                } else {
                    client.call_tool("edit_summary", args.clone()).await
                };
                assert!(result.is_err());
                assert_eq!(client.next_id, before);
                assert!(client.session.is_some());
            }
            client.close().await;
            server.join().unwrap();
        }
    }

    fn retag_catalog() -> Value {
        let tags = json!({"type":"array","maxItems":64,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":256}});
        json!([{"name":"retag","inputSchema":{"type":"object","additionalProperties":false,
            "required":["db","expected_db_id","id","expected_tags","tags"],"properties":{
                "db":{"type":"string"},"id":{"type":"string","minLength":26,"maxLength":26},
                "expected_db_id":{"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"},
                "expected_tags":tags,"tags":tags}}}])
    }

    #[tokio::test]
    async fn retag_requires_complete_advertised_contract_and_owner_before_send() {
        let args = json!({"db":"project","expected_db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","expected_tags":[],"tags":[]});
        let mut cases = vec![(json!([]), args.clone())];
        for field in ["expected_tags", "tags", "expected_db_id"] {
            let mut old = retag_catalog();
            old[0]["inputSchema"]["required"]
                .as_array_mut()
                .unwrap()
                .retain(|value| value != field);
            cases.push((old, args.clone()));
        }
        for field in ["expected_tags", "tags"] {
            let mut old = retag_catalog();
            old[0]["inputSchema"]["properties"][field]["uniqueItems"] = json!(false);
            cases.push((old, args.clone()));
        }
        let mut bad_guard = retag_catalog();
        bad_guard[0]["inputSchema"]["properties"]["expected_db_id"]["pattern"] = json!(".*");
        cases.push((bad_guard, args.clone()));
        for field in ["db", "expected_db_id"] {
            let mut missing = args.clone();
            missing.as_object_mut().unwrap().remove(field);
            cases.push((retag_catalog(), missing));
        }
        for (catalog, args) in cases {
            let (options, server) = fixture_server_with_catalog("mneme-mcp", catalog, Some(vec![]));
            let mut client = RemoteClient::connect_local(
                &options,
                ClientTimeouts {
                    connect: Duration::from_secs(2),
                    request: Duration::from_secs(2),
                },
                "mneme-mcp",
            )
            .await
            .unwrap();
            let before = client.next_id;
            for raw in [false, true] {
                let result = if raw {
                    client
                        .raw_rpc("tools/call", json!({"name":"retag","arguments":args}))
                        .await
                } else {
                    client.call_tool("retag", args.clone()).await
                };
                assert!(result.is_err());
                assert_eq!(client.next_id, before);
                assert!(client.session.is_some());
            }
            client.close().await;
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn edit_body_forwards_exact_guarded_payload_once_without_fallback() {
        let args = json!({"db":"project","expected_db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","expected_body_revision":"a".repeat(64),"body":"replacement"});
        let (options, server) = fixture_server_with_catalog(
            "mneme-mcp",
            edit_body_catalog(),
            Some(vec![("edit_body", args.clone())]),
        );
        let mut client = RemoteClient::connect_local(
            &options,
            ClientTimeouts {
                connect: Duration::from_secs(2),
                request: Duration::from_secs(2),
            },
            "mneme-mcp",
        )
        .await
        .unwrap();
        assert!(client.supports_edit_body());
        assert!(client.supports_expected_db_id("edit_body", None));
        client.call_tool("edit_body", args).await.unwrap();
        client.close().await;
        server.join().unwrap();
    }

    #[tokio::test]
    async fn edit_summary_forwards_exact_guarded_payload_once_without_fallback() {
        let args = json!({"db":"project","expected_db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","expected_snapshot_sha256":"a".repeat(64),"summary":"replacement"});
        let (options, server) = fixture_server_with_catalog(
            "mneme-mcp",
            edit_summary_catalog(),
            Some(vec![("edit_summary", args.clone())]),
        );
        let mut client = RemoteClient::connect_local(
            &options,
            ClientTimeouts {
                connect: Duration::from_secs(2),
                request: Duration::from_secs(2),
            },
            "mneme-mcp",
        )
        .await
        .unwrap();
        assert!(client.supports_edit_summary());
        assert!(client.supports_expected_db_id("edit_summary", None));
        client.call_tool("edit_summary", args).await.unwrap();
        client.close().await;
        server.join().unwrap();
    }

    #[tokio::test]
    async fn retag_forwards_exact_guarded_payload_once_without_fallback() {
        let args = json!({"db":"project","expected_db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","expected_tags":["possibility"],"tags":[]});
        let (options, server) = fixture_server_with_catalog(
            "mneme-mcp",
            retag_catalog(),
            Some(vec![("retag", args.clone())]),
        );
        let mut client = RemoteClient::connect_local(
            &options,
            ClientTimeouts {
                connect: Duration::from_secs(2),
                request: Duration::from_secs(2),
            },
            "mneme-mcp",
        )
        .await
        .unwrap();
        assert!(client.supports_retag());
        assert!(client.supports_expected_db_id("retag", None));
        client.call_tool("retag", args).await.unwrap();
        client.close().await;
        server.join().unwrap();
    }

    fn save_catalog() -> Value {
        json!([{"name":"save","inputSchema":{
            "type":"object","required":["db","summary"],"properties":{
                "db":{"type":"string"}, "kind":{"type":"string","enum":["note","episode"]},
                "expected_db_id":{"type":"string","minLength":26,"maxLength":26,
                    "pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"},
                "links":{"type":"array","items":{"type":"object","required":["to"],"properties":{
                    "to":{"type":"string"},"kind":{"type":"string","enum":["associative","transition","derived_from"]},
                    "weight":{"type":"number"}}}}
            },
            "oneOf":[{"properties":{"kind":{"const":"note"}}},{"properties":{"kind":{"const":"episode"}}}]
        }}])
    }

    #[tokio::test]
    async fn save_catalog_predicates_compose_root_kind_links_owner_and_guard() {
        let mut cases = vec![(
            save_catalog(),
            "note",
            json!({"db":"project","kind":"unknown"}),
        )];
        let mut hidden = save_catalog();
        hidden[0]["inputSchema"]["properties"]["kind"]["enum"] = json!(["episode"]);
        cases.push((hidden, "note", json!({"db":"project","summary":"x"})));
        for field in ["kind", "db"] {
            let mut wrong = save_catalog();
            wrong[0]["inputSchema"]["properties"][field]["type"] = json!("number");
            cases.push((wrong, "note", json!({"db":"project","kind":"note"})));
        }
        let mut implicit_owner = save_catalog();
        implicit_owner[0]["inputSchema"]["required"] = json!(["summary"]);
        cases.push((
            implicit_owner,
            "note",
            json!({"db":"project","kind":"note"}),
        ));
        let mut malformed_links = save_catalog();
        malformed_links[0]["inputSchema"]["properties"]["links"]["items"]["properties"]["weight"]
            ["type"] = json!("string");
        cases.push((
            malformed_links,
            "note",
            json!({"db":"project","kind":"note","links":[]}),
        ));
        let mut malformed_guard = save_catalog();
        malformed_guard[0]["inputSchema"]["properties"]["expected_db_id"]["pattern"] = json!(".*");
        cases.push((
            malformed_guard,
            "episode",
            json!({"db":"project","kind":"episode","expected_db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}),
        ));
        cases.push((save_catalog(), "note", json!({"kind":"note"})));
        cases.push((save_catalog(), "note", json!({"db":null,"kind":"note"})));
        cases.push((save_catalog(), "note", json!({"db":"project","kind":null})));
        cases.push((json!([]), "note", json!({"db":"project","kind":"note"})));
        for (catalog, kind, arguments) in cases {
            let (options, server) = fixture_server_with_catalog("mneme-mcp", catalog, Some(vec![]));
            let mut client = RemoteClient::connect_local(
                &options,
                ClientTimeouts {
                    connect: Duration::from_secs(2),
                    request: Duration::from_secs(2),
                },
                "mneme-mcp",
            )
            .await
            .unwrap();
            assert!(!client.supports_save_kind("unknown"));
            let request_id = client.next_id;
            for raw in [false, true] {
                let error = if raw {
                    client
                        .raw_rpc("tools/call", json!({"name":"save","arguments":arguments}))
                        .await
                } else {
                    client.call_tool("save", arguments.clone()).await
                }
                .unwrap_err();
                assert_eq!(classify_error(error.as_ref()), ClientErrorClass::Protocol);
                assert_eq!(
                    client.next_id, request_id,
                    "SAVE refusal must precede HTTP mutation"
                );
                assert!(client.session.is_some());
            }
            // Kind and links/guards are separate composable capabilities.
            if client.supports_save_kind(kind) && arguments.get("links").is_some() {
                assert!(!client.supports_save_links(kind));
            }
            client.close().await;
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn save_kind_links_and_guard_forward_exact_payload_without_fallback() {
        let mut calls = Vec::new();
        for kind in ["note", "episode"] {
            let args = json!({"db":"project","kind":kind,"summary":"A claim", "source":{
                "namespace":"manual","key":"operation","reference":"manual-submission:operation"},
                "links":[{"to":"01ARZ3NDEKTSV4RRFFQ69G5FAV","kind":"derived_from","weight":0.5}],
                "expected_db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"});
            calls.extend([("save", args.clone()), ("save", args)]);
        }
        let (options, server) =
            fixture_server_with_catalog("mneme-mcp", save_catalog(), Some(calls.clone()));
        let mut client = RemoteClient::connect_local(
            &options,
            ClientTimeouts {
                connect: Duration::from_secs(2),
                request: Duration::from_secs(2),
            },
            "mneme-mcp",
        )
        .await
        .unwrap();
        assert!(client.supports_save());
        for (index, (tool, args)) in calls.into_iter().enumerate() {
            let kind = args["kind"].as_str().unwrap();
            assert!(client.supports_save_kind(kind));
            assert!(client.supports_save_links(kind));
            assert!(client.supports_expected_db_id("save", Some(kind)));
            assert!(!client.supports_expected_db_id("save", None));
            let received = if index % 2 == 0 {
                client.call_tool(tool, args.clone()).await.unwrap()
            } else {
                decode_tool_result(
                    &client
                        .raw_rpc("tools/call", json!({"name":tool,"arguments":args}))
                        .await
                        .unwrap(),
                )
                .unwrap()
            };
            assert_eq!(received, args);
        }
        client.close().await;
        server.join().unwrap();
    }

    fn fixture_server(name: &'static str) -> (ConnectionOptions, std::thread::JoinHandle<()>) {
        fixture_server_with_catalog(
            name,
            json!([
                {"name":"capture","inputSchema":{"type":"object","properties":{"links":{"type":"array","items":{"type":"object","required":["to"],"properties":{"to":{"type":"string"},"kind":{"type":"string","enum":["derived_from","associative","transition"]},"weight":{"type":"number"}}}}}}},
                {"name":"episode","inputSchema":{"type":"object","properties":{"action":{"type":"string","enum":["list","get"]}}}}
            ]),
            None,
        )
    }

    fn fixture_server_with_catalog(
        name: &'static str,
        catalog: Value,
        expected_calls: Option<Vec<(&'static str, Value)>>,
    ) -> (ConnectionOptions, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        let handle = std::thread::spawn(move || {
            let requests =
                expected_calls
                    .as_ref()
                    .map_or(if name == "mneme-mcp" { 8 } else { 2 }, |calls| {
                        calls.len() + 4 // initialize, initialized, catalog, close
                    });
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut calls_seen = 0;
            for _ in 0..requests {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "fixture timed out");
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut buf = [0; 4096];
                    let count = stream.read(&mut buf).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buf[..count]);
                    if let Some(pos) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let header = std::str::from_utf8(&bytes[..header_end]).unwrap();
                let length: usize = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|n| n.trim().parse().ok())
                    })
                    .unwrap_or(0);
                while bytes.len() - header_end < length {
                    let mut buf = [0; 4096];
                    let count = stream.read(&mut buf).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buf[..count]);
                }
                let msg: Value = if length == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
                };
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("close");
                let body = match method {
                    "initialize" => {
                        json!({"jsonrpc":"2.0","id":msg["id"],"result":{"protocolVersion":PROTOCOL_VERSION,"serverInfo":{"name":name}}})
                    }
                    "tools/list" => {
                        json!({"jsonrpc":"2.0","id":msg["id"],"result":{"tools":catalog}})
                    }
                    "ping" => json!({"jsonrpc":"2.0","id":msg["id"],"result":{}}),
                    "tools/call" => {
                        if let Some(expected) = &expected_calls {
                            let (tool, arguments) = expected
                                .get(calls_seen)
                                .expect("unexpected tool call reached HTTP fixture");
                            assert_eq!(msg["params"]["name"], *tool);
                            assert_eq!(&msg["params"]["arguments"], arguments);
                            calls_seen += 1;
                            json!({"jsonrpc":"2.0","id":msg["id"],"result":{"isError":false,"content":[{"type":"text","text":arguments.to_string()}]}})
                        } else {
                            json!({"jsonrpc":"2.0","id":msg["id"],"result":{"isError":true,"content":[{"type":"text","text":"denied"}]}})
                        }
                    }
                    _ => Value::Null,
                };
                let response = if method == "notifications/initialized" || method == "close" {
                    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned()
                } else {
                    let body = body.to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: fixture-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
            if let Some(expected) = expected_calls {
                assert_eq!(calls_seen, expected.len());
            }
        });
        (
            ConnectionOptions {
                url,
                ssh_mcp_port: 0,
                token_env: None,
            },
            handle,
        )
    }

    fn guarded_target_catalog() -> Value {
        let guard = json!({
            "type":"string", "minLength":26, "maxLength":26,
            "pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"
        });
        let branches: Vec<_> = ["get", "append", "revise"]
            .into_iter()
            .map(|action| {
                json!({"type":"object", "properties":{
                    "action":{"type":"string", "const":action}, "expected_db_id":guard
                }})
            })
            .collect();
        json!([
            {"name":"capture", "inputSchema":{"properties":{"expected_db_id":guard}}},
            {"name":"get", "inputSchema":{"properties":{"expected_db_id":guard}}},
            {"name":"episode", "inputSchema":{
                "properties":{"action":{"type":"string","enum":["get","append","revise"]},
                    "expected_db_id":guard}, "oneOf":branches
            }},
            {"name":"list", "inputSchema":{"properties":{"kind":{"const":"touchstones"},"expected_db_id":guard}}}
        ])
    }

    const ORDINARY_GUARDED_TOOLS: &[&str] = &[
        "status",
        "query",
        "recall_context",
        "recall",
        "core",
        "neighbors",
        "remote_edges",
        "contradictions",
        "merges",
        "ingest",
        "link",
        "supersede",
        "contradict",
        "reconcile",
        "feedback",
        "merge",
        "forget",
        "decay",
        "prune",
        "snapshot_create",
        "reflect",
    ];

    fn extended_guard_catalog() -> Value {
        let guard =
            guarded_target_catalog()[0]["inputSchema"]["properties"]["expected_db_id"].clone();
        let mut tools: Vec<Value> = ORDINARY_GUARDED_TOOLS
            .iter()
            .map(|tool| json!({"name":tool,"inputSchema":{"properties":{"expected_db_id":guard}}}))
            .collect();
        for (tool, actions) in [
            (
                "walk",
                vec![
                    "start", "look", "edges", "body", "go", "back", "done", "abort",
                ],
            ),
            ("database_control", vec!["status", "release", "resume"]),
        ] {
            tools.push(json!({"name":tool,"inputSchema":{"properties":{"expected_db_id":guard,"action":{"type":"string","enum":actions}}}}));
        }
        tools.push(json!({"name":"list","inputSchema":{"oneOf":[
            {"properties":{"kind":{"const":"touchstones"},"expected_db_id":guard}},
            {"properties":{"kind":{"const":"nodes"},"expected_db_id":guard}}
        ]}}));
        tools.push(json!({"name":"graph","inputSchema":{"properties":{"expected_db_id":guard,"action":{"type":"string","enum":["topology","summaries"]}}}}));
        json!(tools)
    }

    fn catalog_client(catalog: &Value) -> RemoteClient {
        let catalog = catalog.as_array().unwrap().clone();
        RemoteClient {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            endpoint: Url::parse("http://127.0.0.1:0/").unwrap(),
            token: None,
            session: None,
            protocol: None,
            next_id: 1,
            advertised: catalog
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_owned())
                .collect(),
            catalog,
            initialize_result: Value::Null,
            server_name: "mneme-mcp".into(),
            capture_links_supported: false,
            ssh: None,
            request_timeout: RPC_TIMEOUT,
            wire_policy: WirePolicy::default(),
        }
    }

    #[test]
    fn expected_db_id_extended_guards_are_closed_and_structural() {
        let catalog = extended_guard_catalog();
        for (index, tool) in ORDINARY_GUARDED_TOOLS.iter().enumerate() {
            let client = catalog_client(&catalog);
            assert!(client.supports_expected_db_id(tool, None), "{tool}");
            for replacement in [
                json!({}),
                json!({"type":"string"}),
                json!({"type":"string","minLength":26,"maxLength":26,"pattern":".*"}),
            ] {
                let mut bad = catalog.clone();
                bad[index]["inputSchema"]["properties"]["expected_db_id"] = replacement;
                let client = catalog_client(&bad);
                assert!(!client.supports_expected_db_id(tool, None), "{tool}");
                assert!(client.validate_tool_call(&json!({"name":tool,"arguments":{"expected_db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}})).is_err());
            }
        }
        let client = catalog_client(&catalog);
        for tool in ["walk", "database_control", "graph"] {
            assert!(!client.supports_expected_db_id(tool, None));
            assert!(!client.supports_expected_db_id(tool, Some("unknown")));
        }
        let walk = ORDINARY_GUARDED_TOOLS.len();
        let mut hidden = catalog.clone();
        hidden[walk]["inputSchema"]["properties"]["action"]["enum"] = json!(["start"]);
        assert!(!catalog_client(&hidden).supports_expected_db_id("walk", Some("done")));
        let mut malformed = catalog.clone();
        malformed[walk]["inputSchema"]["oneOf"] =
            json!([{"properties":{"action":{"const":"start"},"expected_db_id":{"type":"string"}}}]);
        assert!(!catalog_client(&malformed).supports_expected_db_id("walk", Some("start")));
        let list = walk + 2;
        for branch in 0..2 {
            let mut bad = catalog.clone();
            bad[list]["inputSchema"]["oneOf"][branch]["properties"]["expected_db_id"] = Value::Null;
            assert!(!catalog_client(&bad).supports_expected_db_id("list", None));
        }
        let mut foreign = catalog.clone();
        foreign
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"unknown","inputSchema":catalog[0]["inputSchema"]}));
        assert!(!catalog_client(&foreign).supports_expected_db_id("unknown", None));
    }

    #[tokio::test]
    async fn expected_db_id_extended_guards_forward_through_raw_and_parsed_calls() {
        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let mut calls = Vec::new();
        for tool in ORDINARY_GUARDED_TOOLS {
            calls.push((*tool, json!({"db":"project","expected_db_id":id})));
        }
        for (tool, actions) in [
            (
                "walk",
                vec![
                    "start", "look", "edges", "body", "go", "back", "done", "abort",
                ],
            ),
            ("database_control", vec!["status", "release", "resume"]),
        ] {
            for action in actions {
                calls.push((tool, json!({"action":action,"expected_db_id":id})));
            }
        }
        for kind in ["touchstones", "nodes"] {
            calls.push((
                "list",
                json!({"db":"project","kind":kind,"expected_db_id":id}),
            ));
        }
        for action in ["topology", "summaries"] {
            calls.push((
                "graph",
                json!({"db":"project","action":action,"expected_db_id":id}),
            ));
        }
        let calls: Vec<_> = calls
            .into_iter()
            .flat_map(|(tool, args)| [(tool, args.clone()), (tool, args)])
            .collect();
        let (options, server) =
            fixture_server_with_catalog("mneme-mcp", extended_guard_catalog(), Some(calls.clone()));
        let mut client = RemoteClient::connect_local(
            &options,
            ClientTimeouts {
                connect: Duration::from_secs(2),
                request: Duration::from_secs(2),
            },
            "mneme-mcp",
        )
        .await
        .unwrap();
        for (index, (tool, arguments)) in calls.into_iter().enumerate() {
            let received = if index % 2 == 0 {
                client.call_tool(tool, arguments.clone()).await.unwrap()
            } else {
                decode_tool_result(
                    &client
                        .raw_rpc("tools/call", json!({"name":tool,"arguments":arguments}))
                        .await
                        .unwrap(),
                )
                .unwrap()
            };
            assert_eq!(received, arguments);
        }
        client.close().await;
        server.join().unwrap();
    }

    #[tokio::test]
    async fn expected_db_id_requires_exact_tool_and_action_schema_before_send() {
        let mut cases = Vec::new();
        for (index, tool) in [(0, "capture"), (1, "get"), (3, "list")] {
            let mut absent = guarded_target_catalog();
            absent[index]["inputSchema"] = json!({});
            cases.push((absent, tool, None));
            // Each field is required, with exactly the canonical contract.
            for field in ["type", "minLength", "maxLength", "pattern"] {
                for replacement in [None, Some(json!("wrong"))] {
                    let mut malformed = guarded_target_catalog();
                    let guard = malformed[index]["inputSchema"]["properties"]["expected_db_id"]
                        .as_object_mut()
                        .unwrap();
                    if let Some(value) = replacement {
                        guard.insert(field.to_owned(), value);
                    } else {
                        guard.remove(field);
                    }
                    cases.push((malformed, tool, None));
                }
            }
            for (field, value) in [
                ("minLength", json!(25)),
                ("maxLength", json!(27)),
                ("pattern", json!("^[0-9A-Z]{26}$")),
            ] {
                let mut wrong = guarded_target_catalog();
                wrong[index]["inputSchema"]["properties"]["expected_db_id"][field] = value;
                cases.push((wrong, tool, None));
            }
        }
        for pointer in [
            "/2/inputSchema/properties/expected_db_id",
            "/2/inputSchema/oneOf/1/properties/expected_db_id",
            "/2/inputSchema/oneOf",
        ] {
            let mut absent = guarded_target_catalog();
            *absent.pointer_mut(pointer).unwrap() = Value::Null;
            cases.push((absent, "episode", Some("append")));
        }
        for (index, action) in [(0, "get"), (1, "append"), (2, "revise")] {
            let mut wrong = guarded_target_catalog();
            wrong[2]["inputSchema"]["oneOf"][index]["properties"]["expected_db_id"]["maxLength"] =
                json!(27);
            cases.push((wrong, "episode", Some(action)));
        }
        let mut wrong_branch = guarded_target_catalog();
        wrong_branch[2]["inputSchema"]["oneOf"][1]["properties"]["action"]["const"] =
            json!("other");
        cases.push((wrong_branch, "episode", Some("append")));
        let mut hidden_action = guarded_target_catalog();
        hidden_action[2]["inputSchema"]["properties"]["action"]["enum"] = json!(["get"]);
        cases.push((hidden_action, "episode", Some("append")));
        cases.push((guarded_target_catalog(), "episode", None));

        for (catalog, tool, action) in cases {
            let (options, server) =
                fixture_server_with_catalog("mneme-mcp", catalog, Some(Vec::new()));
            let mut client = RemoteClient::connect_local(
                &options,
                ClientTimeouts {
                    connect: Duration::from_secs(2),
                    request: Duration::from_secs(2),
                },
                "mneme-mcp",
            )
            .await
            .unwrap();
            assert!(!client.supports_expected_db_id(tool, action));
            let mut arguments =
                json!({"db":"project", "expected_db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"});
            if let Some(action) = action {
                arguments["action"] = json!(action);
            }
            if tool == "list" {
                arguments["kind"] = json!("touchstones");
            }
            let request_id = client.next_id;
            for raw in [false, true] {
                let error = if raw {
                    client
                        .raw_rpc("tools/call", json!({"name":tool,"arguments":arguments}))
                        .await
                } else {
                    client.call_tool(tool, arguments.clone()).await
                }
                .unwrap_err();
                assert_eq!(classify_error(error.as_ref()), ClientErrorClass::Protocol);
                assert_eq!(
                    client.next_id, request_id,
                    "rejected call reached request()"
                );
                assert!(
                    client.session.is_some(),
                    "local refusal must preserve the session"
                );
            }
            client.close().await;
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn expected_db_id_supported_guards_are_forwarded_without_fallback() {
        let mut calls = Vec::new();
        for (tool, action) in [
            ("capture", None),
            ("get", None),
            ("list", None),
            ("episode", Some("append")),
            ("episode", Some("get")),
            ("episode", Some("revise")),
        ] {
            let mut arguments =
                json!({"db":"project", "expected_db_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"});
            if let Some(action) = action {
                arguments["action"] = json!(action);
            }
            if tool == "list" {
                arguments["kind"] = json!("touchstones");
            }
            calls.extend([(tool, arguments.clone()), (tool, arguments)]);
        }
        let (options, server) =
            fixture_server_with_catalog("mneme-mcp", guarded_target_catalog(), Some(calls.clone()));
        let mut client = RemoteClient::connect_local(
            &options,
            ClientTimeouts {
                connect: Duration::from_secs(2),
                request: Duration::from_secs(2),
            },
            "mneme-mcp",
        )
        .await
        .unwrap();
        assert!(!client.supports_expected_db_id("get", Some("get")));
        assert!(!client.supports_expected_db_id("list", Some("touchstones")));
        assert!(!client.supports_expected_db_id("episode", None));
        assert!(!client.supports_expected_db_id("unknown", None));
        for (index, (tool, arguments)) in calls.into_iter().enumerate() {
            assert!(client.supports_expected_db_id(tool, arguments["action"].as_str()));
            let received = if index % 2 == 0 {
                client.call_tool(tool, arguments.clone()).await.unwrap()
            } else {
                decode_tool_result(
                    &client
                        .raw_rpc("tools/call", json!({"name":tool,"arguments":arguments}))
                        .await
                        .unwrap(),
                )
                .unwrap()
            };
            assert_eq!(received, arguments);
        }
        client.close().await;
        server.join().unwrap();
    }

    #[tokio::test]
    async fn local_session_catalog_raw_and_parsed_tool_results() {
        let (options, server) = fixture_server("mneme-mcp");
        let mut client = RemoteClient::connect_local(
            &options,
            ClientTimeouts {
                connect: Duration::from_secs(2),
                request: Duration::from_secs(2),
            },
            "mneme-mcp",
        )
        .await
        .unwrap();
        assert_eq!(client.server_name(), "mneme-mcp");
        assert_eq!(client.tool_catalog().len(), 2);
        assert!(client.supports_capture_links());
        assert!(client.supports_episode_action("get"));
        assert!(!client.supports_episode_action("revise"));
        // Neither the high-level call nor raw RPC can send a profile-hidden
        // episode mutation. The finite fixture expects no extra HTTP request.
        for raw in [false, true] {
            let args = json!({"action":"revise"});
            let error = if raw {
                client
                    .raw_rpc("tools/call", json!({"name":"episode","arguments":args}))
                    .await
            } else {
                client.call_tool("episode", args).await
            }
            .unwrap_err();
            assert_eq!(classify_error(error.as_ref()), ClientErrorClass::Protocol);
        }
        assert_eq!(client.raw_rpc("ping", json!({})).await.unwrap(), json!({}));
        let raw = client
            .raw_rpc("tools/call", json!({"name":"capture","arguments":{}}))
            .await
            .unwrap();
        assert_eq!(raw["isError"], true);
        client.capture_links_supported = false;
        let error = client
            .raw_rpc(
                "tools/call",
                json!({"name":"capture","arguments":{"links":[]}}),
            )
            .await
            .unwrap_err();
        assert_eq!(classify_error(error.as_ref()), ClientErrorClass::Protocol);
        assert!(client.raw_rpc("initialize", json!({})).await.is_err());
        let error = client.call_tool("capture", json!({})).await.unwrap_err();
        assert_eq!(classify_error(error.as_ref()), ClientErrorClass::Tool);
        assert_eq!(client.raw_rpc("ping", json!({})).await.unwrap(), json!({}));
        client.close().await;
        server.join().unwrap();
    }

    #[tokio::test]
    async fn local_handshake_refuses_wrong_server_identity() {
        let (options, server) = fixture_server("impostor");
        let error = RemoteClient::connect_local(
            &options,
            ClientTimeouts {
                connect: Duration::from_secs(2),
                request: Duration::from_secs(2),
            },
            "mneme-mcp",
        )
        .await
        .err()
        .unwrap();
        assert_eq!(classify_error(error.as_ref()), ClientErrorClass::Protocol);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn local_handshake_deadline_is_availability_without_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let options = ConnectionOptions {
            url: format!(
                "http://127.0.0.1:{}/mcp",
                listener.local_addr().unwrap().port()
            ),
            ssh_mcp_port: 0,
            token_env: None,
        };
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut bytes = [0; 4096];
            let _ = stream.read(&mut bytes);
            std::thread::sleep(Duration::from_millis(100));
        });
        let error = RemoteClient::connect_local(
            &options,
            ClientTimeouts {
                connect: Duration::from_millis(20),
                request: Duration::from_millis(20),
            },
            "mneme-mcp",
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            classify_error(error.as_ref()),
            ClientErrorClass::Availability
        );
        server.join().unwrap();
    }

    #[test]
    fn local_policy_rejects_non_numeric_loopback() {
        assert!(!numeric_loopback_http(
            &Url::parse("http://localhost:1234/mcp").unwrap()
        ));
        assert!(!numeric_loopback_http(
            &Url::parse("https://127.0.0.1:1234/mcp").unwrap()
        ));
        assert!(numeric_loopback_http(
            &Url::parse("http://[::1]:1234/mcp").unwrap()
        ));
    }

    #[cfg(unix)]
    fn fake_ssh_child() -> Child {
        Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("/bin/sleep is available on Unix")
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        // SAFETY: kill with signal zero does not signal the process; it only
        // checks whether this test-owned child PID still exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[test]
    fn only_availability_failures_allow_fallback() {
        use std::io::{Error as IoError, ErrorKind};
        assert!(transport_io_unavailable(&IoError::from(
            ErrorKind::ConnectionRefused
        )));
        assert!(transport_io_unavailable(&IoError::from(
            ErrorKind::NetworkUnreachable
        )));
        assert!(!transport_io_unavailable(&IoError::from(
            ErrorKind::InvalidData
        )));
        assert!(!transport_io_unavailable(&IoError::from(
            ErrorKind::PermissionDenied
        )));
        assert!(http_is_availability(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(http_is_availability(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(!http_is_availability(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!http_is_availability(reqwest::StatusCode::FORBIDDEN));
        assert!(!http_is_availability(reqwest::StatusCode::NOT_FOUND));
        assert!(!http_is_availability(reqwest::StatusCode::BAD_REQUEST));
        assert!(!http_is_availability(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        let unavailable: AnyErr = Box::new(AvailabilityError("timed out".into()));
        assert!(is_availability(unavailable.as_ref()));
        let unavailable: AnyErr = Box::new(AvailabilityError("remote MCP HTTP 503".into()));
        assert!(is_availability(unavailable.as_ref()));
        for error in [
            decode_rpc_response(
                json!({ "jsonrpc": "2.0", "id": 1,
                "error": { "code": -32001, "message": "not authorized" } }),
                1,
            )
            .unwrap_err(),
            decode_tool_result(&json!({ "isError": true,
                "content": [{ "type": "text", "text": "tool failed" }] }))
            .unwrap_err(),
            decode_rpc_response(json!({ "jsonrpc": "1.0", "id": 1, "result": {} }), 1).unwrap_err(),
            decode_rpc_response(json!({ "jsonrpc": "2.0", "id": 2, "result": {} }), 1).unwrap_err(),
            decode_tool_result(&json!({ "isError": true,
                "content": [{ "type": "text", "text": "invalid database ID" }] }))
            .unwrap_err(),
            decode_tool_result(&json!({ "content": [{ "type": "image", "data": "?" }] }))
                .unwrap_err(),
        ] {
            assert!(!is_availability(error.as_ref()), "{error}");
        }
    }

    #[tokio::test]
    async fn timeout_configuration_is_bounded_and_rejects_zero() {
        let options = ConnectionOptions {
            url: "http://127.0.0.1:1/mcp".into(),
            ssh_mcp_port: 18766,
            token_env: None,
        };
        let error = RemoteClient::connect_with_timeouts(
            &options,
            ClientTimeouts {
                connect: Duration::ZERO,
                request: Duration::from_secs(1),
            },
        )
        .await
        .err()
        .expect("zero connect timeout must fail");
        assert!(!is_availability(error.as_ref()));
    }

    #[test]
    fn bearer_requires_tls_or_numeric_loopback() {
        assert!(validate_http_url(&Url::parse("http://example.test/mcp").unwrap(), true).is_err());
        assert!(validate_http_url(&Url::parse("http://localhost/mcp").unwrap(), true).is_err());
        assert!(
            validate_http_url(&Url::parse("http://127.0.0.1:18766/mcp").unwrap(), true).is_ok()
        );
        assert!(validate_http_url(&Url::parse("https://example.test/mcp").unwrap(), true).is_ok());
        assert!(
            validate_http_url(&Url::parse("https://u:p@example.test/mcp").unwrap(), false).is_err()
        );
    }

    #[test]
    fn response_id_and_shape_are_strict() {
        let ok = json!({ "jsonrpc": "2.0", "id": 3, "result": { "ok": true } });
        assert_eq!(
            decode_rpc_response(ok.clone(), 3).unwrap(),
            json!({ "ok": true })
        );
        assert!(decode_rpc_response(ok, 4).is_err());
        assert!(
            decode_rpc_response(
                json!({ "jsonrpc": "2.0", "id": 3, "result": {}, "error": {} }),
                3
            )
            .is_err()
        );
    }

    #[test]
    fn tool_result_requires_unambiguous_json_and_propagates_error() {
        assert_eq!(
            decode_tool_result(
                &json!({ "content": [{ "type": "text", "text": "{\"x\":1}" }], "isError": false })
            )
            .unwrap(),
            json!({ "x": 1 })
        );
        assert!(
            decode_tool_result(&json!({ "content": [{ "type": "text", "text": "not json" }] }))
                .is_err()
        );
        assert!(decode_tool_result(&json!({ "content": [{ "type": "text", "text": "{}" }, { "type": "text", "text": "{}" }] })).is_err());
        assert!(
            decode_tool_result(
                &json!({ "content": [{ "type": "text", "text": "failed" }], "isError": true })
            )
            .is_err()
        );
    }

    #[test]
    fn sse_only_accepts_complete_response_event() {
        assert!(
            parse_sse(b"data: {\"jsonrpc\":\"2.0\",\"id\":1}\n")
                .unwrap()
                .is_none()
        );
        assert!(
            parse_sse(b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1}\n\n")
                .unwrap()
                .is_some()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_drops_and_reaps_ssh_child() {
        let child = fake_ssh_child();
        let pid = child.id();
        assert!(process_exists(pid));
        // This is the same guard held across the tunnel readiness await. A
        // cancelled connect future must not leave its ssh process behind.
        let connect = async move {
            let _guard = SshGuard(Some(child));
            std::future::pending::<()>().await;
        };
        let mut connect = Box::pin(connect);
        std::future::poll_fn(|cx| {
            assert!(connect.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(connect);
        assert!(!process_exists(pid), "cancelled SSH child was not reaped");
    }

    #[cfg(unix)]
    #[test]
    fn dropping_connected_client_reaps_ssh_child() {
        let child = fake_ssh_child();
        let pid = child.id();
        let remote = RemoteClient {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            endpoint: Url::parse("http://127.0.0.1:0/").unwrap(),
            token: None,
            session: None,
            protocol: None,
            next_id: 1,
            advertised: HashSet::new(),
            catalog: Vec::new(),
            initialize_result: Value::Null,
            server_name: String::new(),
            capture_links_supported: false,
            ssh: Some(child),
            request_timeout: RPC_TIMEOUT,
            wire_policy: WirePolicy::default(),
        };
        assert!(process_exists(pid));
        drop(remote);
        assert!(
            !process_exists(pid),
            "dropped client left SSH child running"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_handshake_closes_ssh_child() {
        let child = fake_ssh_child();
        let pid = child.id();
        let mut remote = RemoteClient {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            endpoint: Url::parse("http://127.0.0.1:0/").unwrap(),
            token: None,
            session: None,
            protocol: None,
            next_id: 1,
            advertised: HashSet::new(),
            catalog: Vec::new(),
            initialize_result: Value::Null,
            server_name: String::new(),
            capture_links_supported: false,
            ssh: Some(child),
            request_timeout: RPC_TIMEOUT,
            wire_policy: WirePolicy::default(),
        };
        let result = tokio::time::timeout(Duration::from_secs(2), remote.handshake())
            .await
            .expect("failed handshake should return promptly");
        assert!(result.is_err());
        remote.close().await;
        assert!(
            !process_exists(pid),
            "failed-handshake SSH child was not reaped"
        );
    }
}
