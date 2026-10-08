//! Hidden, bounded local-process bridge to an existing MCP owner. This module
//! never resolves a database path, opens a store, or loads an embedder.

use std::io::{self, BufRead, Write};
use std::time::Duration;

use clap::{Args, ValueEnum};
use mneme_app::capture::PreparedCapture;
use mneme_app::concern::PreparedConcernRequest;
use mneme_app::episode::PreparedEpisode;
use mneme_app::save::{PreparedSave, SaveKind, SaveOrigin};
use mneme_mcp_client::{ClientTimeouts, ConnectionOptions, RemoteClient};
use serde_json::{Value, json};

use crate::AnyErr;

const MAX_FRAME: usize = 128 * 1024;
const MAX_RESPONSE: usize = 512 * 1024;
const MAX_EPISODE_READBACK_CALLS: usize = 8;
// The shared operation packs 32 KiB; allow only its small database envelope.
const MAX_EPISODE_REPLY: usize = 36 * 1024;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum ExpectedServer {
    #[value(name = "mneme-mcp")]
    MnemeMcp,
    #[value(name = "mneme-mcp-library")]
    MnemeMcpLibrary,
}
impl ExpectedServer {
    fn name(self) -> &'static str {
        match self {
            Self::MnemeMcp => "mneme-mcp",
            Self::MnemeMcpLibrary => "mneme-mcp-library",
        }
    }
}

#[derive(Args)]
pub(crate) struct ClientArgs {
    #[arg(long)]
    endpoint: String,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=30_000))]
    timeout_ms: u32,
    #[arg(long)]
    token_env: Option<String>,
    #[arg(long, value_enum)]
    expected_server: ExpectedServer,
    /// Required to make the loopback-only policy explicit at the call site.
    #[arg(long, required = true)]
    local_only: bool,
}

fn frame<R: BufRead>(input: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    loop {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() {
            return if out.is_empty() {
                Ok(None)
            } else {
                Ok(Some(out))
            };
        }
        let end = chunk.iter().position(|&byte| byte == b'\n');
        let take = end.map_or(chunk.len(), |index| index + 1);
        let remaining = MAX_FRAME.saturating_add(1).saturating_sub(out.len());
        out.extend_from_slice(&chunk[..take.min(remaining)]);
        input.consume(take);
        if end.is_some() {
            return Ok(Some(out));
        }
    }
}

fn response(id: Value, outcome: Result<Value, ClientFailure>) -> Value {
    match outcome {
        Ok(result) => json!({"id": id, "ok": true, "result": result}),
        Err(error) => {
            let mut details = json!({"kind": error.kind, "message": error.message});
            if let Some(accepted) = error.accepted {
                details["accepted"] = accepted;
            }
            if let Some(attempted) = error.attempted {
                details["attempted"] = attempted;
            }
            json!({"id": id, "ok": false, "error": details})
        }
    }
}

#[derive(Debug)]
struct ClientFailure {
    kind: &'static str,
    message: String,
    accepted: Option<Value>,
    attempted: Option<Value>,
}
impl ClientFailure {
    fn with_accepted(mut self, receipt: &Value) -> Self {
        self.accepted = Some(receipt.clone());
        self
    }
    fn input(message: impl Into<String>) -> Self {
        Self {
            kind: "input",
            message: message.into(),
            accepted: None,
            attempted: None,
        }
    }
    fn protocol(message: impl Into<String>) -> Self {
        Self {
            kind: "protocol",
            message: message.into(),
            accepted: None,
            attempted: None,
        }
    }
    fn transport(message: impl Into<String>) -> Self {
        Self {
            kind: "transport",
            message: message.into(),
            accepted: None,
            attempted: None,
        }
    }
    fn tool(message: impl Into<String>) -> Self {
        Self {
            kind: "tool",
            message: message.into(),
            accepted: None,
            attempted: None,
        }
    }
}

fn required_object<'a>(request: &'a Value, field: &str) -> Result<&'a Value, ClientFailure> {
    request
        .get(field)
        .filter(|value| value.is_object())
        .ok_or_else(|| ClientFailure::input(format!("{field} must be an object")))
}
fn required_string<'a>(request: &'a Value, field: &str) -> Result<&'a str, ClientFailure> {
    request
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ClientFailure::input(format!("{field} must be a nonempty string")))
}

/// Routing metadata never enters the shared capture/episode domain payload.
fn expected_db_id(request: &Value) -> Result<Option<ulid::Ulid>, ClientFailure> {
    let Some(value) = request.get("expected_db_id") else {
        return Ok(None);
    };
    let invalid = || ClientFailure::input("expected_db_id must be a canonical 26-byte ULID string");
    let text = value
        .as_str()
        .filter(|text| text.len() == 26)
        .ok_or_else(invalid)?;
    let id = ulid::Ulid::from_string(text).map_err(|_| invalid())?;
    if id.to_string() != text {
        return Err(invalid());
    }
    Ok(Some(id))
}

fn attach_expected_db_id(arguments: &mut Value, expected: Option<ulid::Ulid>) {
    if let Some(expected) = expected {
        arguments["expected_db_id"] = json!(expected.to_string());
    }
}

fn matches_expected_db_id(value: &Value, expected: Option<ulid::Ulid>) -> bool {
    expected.is_none_or(|expected| value.get("db_id") == Some(&json!(expected.to_string())))
}

fn hex(bytes: [u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(64);
    for byte in bytes {
        write!(result, "{byte:02x}").expect("writing to String");
    }
    result
}
fn prepare(payload: &Value) -> Result<Value, ClientFailure> {
    let prepared =
        PreparedCapture::parse(payload).map_err(|e| ClientFailure::input(e.to_string()))?;
    let source = prepared
        .expected_source()
        .map_err(|e| ClientFailure::input(e.to_string()))?;
    Ok(json!({
        "payload": prepared.into_json(),
        "id": source.node_id().0.to_string(),
        "digest": hex(source.request_digest()),
        "source": {
            "namespace": source.namespace(), "key": source.key(), "reference": source.reference(),
            "session": source.session(), "revision": source.revision(),
            "request_digest_sha256": hex(source.request_digest()), "request_codec": source.request_codec()
        }
    }))
}

fn prepare_episode(payload: &Value) -> Result<Value, ClientFailure> {
    let prepared =
        PreparedEpisode::parse(payload).map_err(|e| ClientFailure::input(e.to_string()))?;
    let edition_id = prepared
        .expected_edition_id()
        .map_err(|e| ClientFailure::input(e.to_string()))?;
    let mut result = json!({
        "action": prepared.action().as_str(),
        "is_mutation": prepared.is_mutation(),
        "payload": prepared.into_json(),
    });
    if let Some(id) = edition_id {
        result["edition_id"] = json!(id.0.to_string());
    }
    Ok(result)
}

fn prepare_save(payload: &Value) -> Result<Value, ClientFailure> {
    // The host identity is generated once; frozen canonical source owns replay.
    let prepared = PreparedSave::parse(payload, &ulid::Ulid::new().to_string())
        .map_err(|e| ClientFailure::input(e.to_string()))?;
    let id = prepared
        .expected_id()
        .map_err(|e| ClientFailure::input(e.to_string()))?;
    Ok(json!({"schema":1, "id":id.0.to_string(), "payload":prepared.into_json()}))
}

fn registry_name(request: &Value) -> Result<&str, ClientFailure> {
    let db = required_string(request, "db")?;
    if db.len() > 128
        || !db
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(ClientFailure::input(
            "db must be a registry name of at most 128 ASCII bytes",
        ));
    }
    Ok(db)
}

/// Join the bounded body pages of one immutable edition. This only proves byte
/// continuity and a stable header; the shared app verifier proves authored data.
struct EpisodeReadback {
    first: Option<Value>,
    header: Option<Value>,
    body: String,
    expected_len: usize,
}

impl EpisodeReadback {
    fn new(expected_len: usize) -> Self {
        Self {
            first: None,
            header: None,
            body: String::new(),
            expected_len,
        }
    }

    fn offset(&self) -> usize {
        self.body.len()
    }

    fn push(&mut self, chunk: Value) -> Result<bool, ClientFailure> {
        if serde_json::to_vec(&chunk)
            .map_err(|e| ClientFailure::protocol(e.to_string()))?
            .len()
            > MAX_EPISODE_REPLY
        {
            return Err(ClientFailure::protocol(
                "episode readback exceeds the reply budget",
            ));
        }
        let text = chunk
            .get("body")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientFailure::protocol("episode readback body is missing"))?;
        let start = self.body.len();
        let end = start
            .checked_add(text.len())
            .ok_or_else(|| ClientFailure::protocol("episode readback byte range overflows"))?;
        let range = &chunk["body_range"];
        let more = range
            .get("has_more")
            .and_then(Value::as_bool)
            .ok_or_else(|| ClientFailure::protocol("episode readback continuation is missing"))?;
        if end > self.expected_len
            || range.get("source_start").and_then(Value::as_u64) != Some(start as u64)
            || range.get("source_end").and_then(Value::as_u64) != Some(end as u64)
            || if more {
                end <= start
                    || end >= self.expected_len
                    || range.get("next_offset").and_then(Value::as_u64) != Some(end as u64)
            } else {
                end != self.expected_len || range.get("next_offset") != Some(&Value::Null)
            }
        {
            return Err(ClientFailure::protocol(
                "episode readback has a noncontiguous or truncated body",
            ));
        }
        let mut header = chunk.clone();
        let fields = header
            .as_object_mut()
            .ok_or_else(|| ClientFailure::protocol("episode readback must be an object"))?;
        // The exact edition cannot change, but another writer may advance its
        // current head while these body pages are being read.
        for field in ["body", "body_range", "current_edition_id", "is_current"] {
            fields.remove(field);
        }
        if self
            .header
            .as_ref()
            .is_some_and(|previous| previous != &header)
        {
            return Err(ClientFailure::protocol(
                "episode readback changed edition metadata between pages",
            ));
        }
        self.body.push_str(text);
        self.header = Some(header);
        if self.first.is_none() {
            self.first = Some(chunk);
        }
        Ok(!more)
    }

    fn finish(self) -> Result<Value, ClientFailure> {
        let mut result = self
            .first
            .ok_or_else(|| ClientFailure::protocol("episode readback is empty"))?;
        if self.body.len() != self.expected_len {
            return Err(ClientFailure::protocol(
                "episode readback exceeded its bounded page count",
            ));
        }
        result["body_range"] = json!({
            "source_start": 0, "source_end": self.body.len(),
            "next_offset": null, "has_more": false,
        });
        result["body"] = json!(self.body);
        Ok(result)
    }
}

fn map_remote(error: &(dyn std::error::Error + 'static)) -> ClientFailure {
    // The transport owns the finer-grained classifier. Never print credentials
    // or arbitrary server content into a process protocol response.
    let message = error.to_string();
    match mneme_mcp_client::classify_error(error) {
        mneme_mcp_client::ClientErrorClass::Availability => ClientFailure::transport(message),
        mneme_mcp_client::ClientErrorClass::Protocol => ClientFailure::protocol(message),
        mneme_mcp_client::ClientErrorClass::Tool => ClientFailure::tool(message),
    }
}

async fn call_episode(
    client: &mut Option<RemoteClient>,
    arguments: Value,
) -> Result<Value, ClientFailure> {
    call_tool(client, "episode", arguments).await
}

async fn call_tool(
    client: &mut Option<RemoteClient>,
    name: &str,
    arguments: Value,
) -> Result<Value, ClientFailure> {
    let remote = client
        .as_mut()
        .ok_or_else(|| ClientFailure::input("connect before MCP requests"))?;
    let result = remote.call_tool(name, arguments).await;
    if result.as_ref().err().is_some_and(|error| {
        mneme_mcp_client::classify_error(error.as_ref()) != mneme_mcp_client::ClientErrorClass::Tool
    }) {
        remote.close().await;
        *client = None;
    }
    result.map_err(|error| map_remote(error.as_ref()))
}

/// Keep write evidence bounded even if an untrusted peer returns body-shaped
/// metadata. Evidence is a report of acceptance, never mutation authority.
fn save_receipt_evidence(prepared: &PreparedSave, receipt: &Value) -> Value {
    let mut evidence = json!({
        "kind":prepared.kind().as_str(), "origin":prepared.identity().origin.as_str(),
        "replayed":receipt.get("replayed").and_then(Value::as_bool),
    });
    for (field, bound) in [("id", 26), ("db", 128), ("db_id", 26)] {
        evidence[field] = json!(
            receipt
                .get(field)
                .and_then(Value::as_str)
                .filter(|s| s.len() <= bound)
        );
    }
    if prepared.identity().origin == SaveOrigin::ManualSubmission {
        evidence["operation_id"] = json!(prepared.identity().key);
    }
    if prepared.kind() == SaveKind::Episode {
        for field in ["episode_id", "edition_id"] {
            evidence[field] = json!(
                receipt
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|s| s.len() == 26)
            );
        }
        evidence["revision"] = json!(receipt.get("revision").and_then(Value::as_u64));
    }
    evidence
}

async fn verified_save(
    request: &Value,
    client: &mut Option<RemoteClient>,
) -> Result<Value, ClientFailure> {
    let db = registry_name(request)?;
    let expected_db_id = expected_db_id(request)?;
    let payload = required_object(request, "payload")?;
    // Retain the admitted value and freeze the same authored payload for the
    // one write. Neither RPC identity nor a second classification owns replay.
    let prepared = PreparedSave::parse(payload, &ulid::Ulid::new().to_string())
        .map_err(|e| ClientFailure::input(e.to_string()))?;
    let expected_id = prepared
        .expected_id()
        .map_err(|e| ClientFailure::input(e.to_string()))?
        .0
        .to_string();
    let kind = prepared.kind();
    let frozen = prepared.into_json();
    let prepared =
        PreparedSave::parse(&frozen, "").map_err(|e| ClientFailure::input(e.to_string()))?;
    let remote = client
        .as_ref()
        .ok_or_else(|| ClientFailure::input("connect before MCP requests"))?;
    if !remote.supports_save_kind(kind.as_str())
        || (frozen.get("links").is_some() && !remote.supports_save_links(kind.as_str()))
        || match kind {
            SaveKind::Note => !remote.advertises("get"),
            SaveKind::Episode => !remote.supports_episode_action("get"),
        }
    {
        return Err(ClientFailure::protocol(
            "server does not advertise compatible SAVE and exact readback; refusing write without fallback",
        ));
    }
    if frozen.get("touchstone").is_some()
        && !crate::remote_touchstone::advertised(remote.tool_catalog(), "save")
    {
        return Err(ClientFailure::protocol(
            "server does not advertise native touchstone SAVE; refusing write without fallback",
        ));
    }
    let read_tool = if kind == SaveKind::Note {
        "get"
    } else {
        "episode"
    };
    let read_action = (kind == SaveKind::Episode).then_some("get");
    if expected_db_id.is_some()
        && (!remote.supports_expected_db_id("save", Some(kind.as_str()))
            || !remote.supports_expected_db_id(read_tool, read_action))
    {
        return Err(ClientFailure::protocol(
            "server does not advertise expected_db_id for both SAVE and exact readback; refusing guarded write",
        ));
    }
    let mut arguments = frozen;
    arguments["db"] = json!(db);
    attach_expected_db_id(&mut arguments, expected_db_id);
    let mut attempted = json!({
        "kind":kind.as_str(), "id":expected_id, "origin":prepared.identity().origin.as_str(),
        "source":arguments["source"], "db":db,
        "write_status":"unacknowledged", "readback_status":"not_attempted", "retryable":false,
    });
    if prepared.identity().origin == SaveOrigin::ManualSubmission {
        attempted["operation_id"] = json!(prepared.identity().key);
    }
    attach_expected_db_id(&mut attempted, expected_db_id);
    // Exactly one mutation; uncertainty must never cause automatic retry.
    let receipt = call_tool(client, "save", arguments)
        .await
        .map_err(|mut error| {
            if error.kind != "tool" {
                // This process knows which identity it attempted, but no valid
                // receipt proved acceptance. Preserve it without making that claim.
                error.attempted = Some(attempted);
            }
            error
        })?;
    let mut accepted = save_receipt_evidence(&prepared, &receipt);
    accepted["readback_status"] = json!("not_attempted");
    accepted["retryable"] = json!(false);
    if receipt.get("db").and_then(Value::as_str) != Some(db)
        || !matches_expected_db_id(&receipt, expected_db_id)
        || receipt
            .get("db_id")
            .and_then(Value::as_str)
            .is_none_or(|text| {
                text.len() != 26
                    || ulid::Ulid::from_string(text).map_or(true, |id| id.to_string() != text)
            })
        || serde_json::to_vec(&receipt)
            .map_err(|e| ClientFailure::protocol(e.to_string()).with_accepted(&accepted))?
            .len()
            > MAX_EPISODE_REPLY
    {
        return Err(ClientFailure::protocol(
            "SAVE returned invalid database identity or oversized receipt",
        )
        .with_accepted(&accepted));
    }
    prepared
        .verify_write_receipt_json(&receipt)
        .map_err(|e| ClientFailure::protocol(e.to_string()).with_accepted(&accepted))?;
    accepted["readback_status"] = json!("failed");
    let readback = match kind {
        SaveKind::Note => {
            let mut arguments =
                json!({"db":db, "id":expected_id, "body":true, "max_body_bytes":256*1024});
            attach_expected_db_id(&mut arguments, expected_db_id);
            let node = call_tool(client, "get", arguments)
                .await
                .map_err(|e| e.with_accepted(&accepted))?;
            if node.get("db").and_then(Value::as_str) != Some(db)
                || node.get("db_id") != receipt.get("db_id")
            {
                return Err(
                    ClientFailure::protocol("SAVE readback changed database identity")
                        .with_accepted(&accepted),
                );
            }
            node
        }
        SaveKind::Episode => {
            let mut collected = EpisodeReadback::new(prepared.expected_body().len());
            for _ in 0..MAX_EPISODE_READBACK_CALLS {
                let mut arguments = json!({
                    "db":db, "action":"get", "episode_id":receipt["episode_id"],
                    "edition_id":receipt["edition_id"], "body":true,
                    "offset":collected.offset(), "max_bytes":mneme_core::episode::MAX_EPISODE_BODY_BYTES,
                });
                attach_expected_db_id(&mut arguments, expected_db_id);
                let node = call_episode(client, arguments)
                    .await
                    .map_err(|e| e.with_accepted(&accepted))?;
                if node.get("db").and_then(Value::as_str) != Some(db)
                    || node.get("db_id") != receipt.get("db_id")
                {
                    return Err(ClientFailure::protocol(
                        "SAVE episode readback changed database identity",
                    )
                    .with_accepted(&accepted));
                }
                if collected
                    .push(node)
                    .map_err(|e| e.with_accepted(&accepted))?
                {
                    break;
                }
            }
            collected.finish().map_err(|e| e.with_accepted(&accepted))?
        }
    };
    accepted["readback_status"] = json!("mismatch");
    prepared
        .verify_readback_json(&receipt, &readback)
        .map_err(|e| ClientFailure::protocol(e.to_string()).with_accepted(&accepted))?;
    let mut result = save_receipt_evidence(&prepared, &receipt);
    result["readback_status"] = json!("verified");
    Ok(result)
}

async fn verified_episode(
    request: &Value,
    client: &mut Option<RemoteClient>,
) -> Result<Value, ClientFailure> {
    let db = registry_name(request)?;
    let expected_db_id = expected_db_id(request)?;
    let payload = required_object(request, "payload")?;
    let prepared =
        PreparedEpisode::parse(payload).map_err(|e| ClientFailure::input(e.to_string()))?;
    let expected_body = prepared
        .expected_body()
        .ok_or_else(|| ClientFailure::input("episode/verified requires an episode mutation"))?;
    let remote = client
        .as_ref()
        .ok_or_else(|| ClientFailure::input("connect before MCP requests"))?;
    if !remote.supports_episode_action(prepared.action().as_str())
        || !remote.supports_episode_action("get")
    {
        return Err(ClientFailure::protocol(
            "server does not advertise the episode write and exact readback actions",
        ));
    }
    if expected_db_id.is_some()
        && (!remote.supports_expected_db_id("episode", Some(prepared.action().as_str()))
            || !remote.supports_expected_db_id("episode", Some("get")))
    {
        return Err(ClientFailure::protocol(
            "server does not advertise expected_db_id for both the episode write action and episode get; refusing guarded write",
        ));
    }
    let mut arguments = payload.clone();
    arguments["db"] = json!(db);
    attach_expected_db_id(&mut arguments, expected_db_id);
    // Exactly one write. An uncertain response is never retried or reconnected.
    let mut receipt = call_episode(client, arguments).await?;
    let mut accepted = json!({
        "episode_id":receipt.get("episode_id"), "edition_id":receipt.get("edition_id"),
        "revision":receipt.get("revision"), "replayed":receipt.get("replayed"),
        "source":payload.get("source"), "db":receipt.get("db"), "db_id":receipt.get("db_id"),
        "readback_status":"not_attempted", "retryable":false,
    });
    if receipt.get("db").and_then(Value::as_str) != Some(db)
        || !matches_expected_db_id(&receipt, expected_db_id)
        || receipt
            .get("db_id")
            .and_then(Value::as_str)
            .and_then(|value| ulid::Ulid::from_string(value).ok())
            .is_none()
        || serde_json::to_vec(&receipt)
            .map_err(|e| ClientFailure::protocol(e.to_string()))?
            .len()
            > MAX_EPISODE_REPLY
    {
        return Err(ClientFailure::protocol(
            "episode write returned an invalid or unexpected database identity or oversized receipt",
        )
        .with_accepted(&accepted));
    }
    prepared
        .verify_write_receipt_json(&receipt)
        .map_err(|e| ClientFailure::protocol(e.to_string()).with_accepted(&accepted))?;
    accepted["readback_status"] = json!("failed");
    let mut collected = EpisodeReadback::new(expected_body.len());
    for _ in 0..MAX_EPISODE_READBACK_CALLS {
        let mut arguments = json!({
            "db":db, "action":"get", "episode_id":receipt["episode_id"],
            "edition_id":receipt["edition_id"], "body":true,
            "offset":collected.offset(), "max_bytes":mneme_core::episode::MAX_EPISODE_BODY_BYTES,
        });
        attach_expected_db_id(&mut arguments, expected_db_id);
        let chunk = call_episode(client, arguments)
            .await
            .map_err(|e| e.with_accepted(&accepted))?;
        if chunk.get("db").and_then(Value::as_str) != Some(db)
            || chunk.get("db_id") != receipt.get("db_id")
        {
            return Err(
                ClientFailure::protocol("episode readback changed database identity")
                    .with_accepted(&accepted),
            );
        }
        if collected
            .push(chunk)
            .map_err(|e| e.with_accepted(&accepted))?
        {
            break;
        }
    }
    let readback = collected.finish().map_err(|e| e.with_accepted(&accepted))?;
    accepted["readback_status"] = json!("mismatch");
    prepared
        .verify_readback_json(&receipt, &readback)
        .map_err(|e| ClientFailure::protocol(e.to_string()).with_accepted(&accepted))?;
    receipt["readback_status"] = json!("verified");
    Ok(receipt)
}

async fn checked_concern(
    request: &Value,
    client: &mut Option<RemoteClient>,
) -> Result<Value, ClientFailure> {
    let object = request
        .as_object()
        .ok_or_else(|| ClientFailure::input("concern command must be an object"))?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "id" | "op" | "db" | "expected_db_id" | "payload"
        )
    }) {
        return Err(ClientFailure::input("unknown concern command field"));
    }
    let db = registry_name(request)?;
    let expected = expected_db_id(request)?;
    let prepared = PreparedConcernRequest::parse(required_object(request, "payload")?)
        .map_err(|error| ClientFailure::input(error.to_string()))?;
    if prepared.is_mutation() && expected.is_none() {
        return Err(ClientFailure::input(
            "concern mutation requires expected_db_id; write was not sent",
        ));
    }
    let remote = client
        .as_ref()
        .ok_or_else(|| ClientFailure::input("connect before MCP requests"))?;
    if !remote.supports_concern_action(prepared.action())
        || (expected.is_some()
            && !remote.supports_expected_db_id("concern", Some(prepared.action())))
    {
        return Err(ClientFailure::protocol(
            "server does not advertise the checked concern action/owner guard; request was not sent",
        ));
    }
    let mut arguments = prepared.clone().into_json();
    arguments["db"] = json!(db);
    attach_expected_db_id(&mut arguments, expected);
    let attempted = prepared.is_mutation().then(|| {
        json!({"db":db,"expected_db_id":expected.map(|id|id.to_string()),
        "action":prepared.action(),"write_status":"unacknowledged","retryable":false})
    });
    let result = call_tool(client, "concern", arguments)
        .await
        .map_err(|mut error| {
            if error.kind != "tool" {
                error.attempted = attempted.clone();
            }
            error
        })?;
    prepared
        .validate_routed_response_json(&result, db, expected)
        .map_err(|error| {
            let mut failure = ClientFailure::protocol(format!(
                "concern atomic result could not be verified: {error}; do not blindly retry"
            ));
            failure.attempted = attempted;
            failure
        })?;
    Ok(result)
}

async fn handle(
    request: &Value,
    client: &mut Option<RemoteClient>,
    args: &ClientArgs,
) -> Result<Value, ClientFailure> {
    request
        .as_object()
        .ok_or_else(|| ClientFailure::input("command must be an object"))?;
    let op = required_string(request, "op")?;
    match op {
        "capture/prepare" => prepare(required_object(request, "payload")?),
        "episode/prepare" => prepare_episode(required_object(request, "payload")?),
        "episode/verified" => verified_episode(request, client).await,
        "save/prepare" => prepare_save(required_object(request, "payload")?),
        "save/verified" => verified_save(request, client).await,
        "concern/checked" => checked_concern(request, client).await,
        "connect" => {
            if client.is_some() {
                return Err(ClientFailure::input("MCP session is already connected"));
            }
            let options = ConnectionOptions {
                url: args.endpoint.clone(),
                ssh_mcp_port: 0,
                token_env: args.token_env.clone(),
            };
            let timeout = Duration::from_millis(args.timeout_ms.into());
            let remote = RemoteClient::connect_local(
                &options,
                ClientTimeouts {
                    connect: timeout,
                    request: timeout,
                },
                args.expected_server.name(),
            )
            .await
            .map_err(|e| map_remote(e.as_ref()))?;
            let result = remote.initialize_result().clone();
            *client = Some(remote);
            Ok(result)
        }
        "close" => {
            if let Some(mut remote) = client.take() {
                remote.close().await;
            }
            Ok(json!({}))
        }
        "tools/list" => {
            let remote = client
                .as_ref()
                .ok_or_else(|| ClientFailure::input("connect before MCP requests"))?;
            Ok(json!({"tools": remote.tool_catalog()}))
        }
        "tools/call" => {
            let name = required_string(request, "name")?;
            if name.len() > 128 {
                return Err(ClientFailure::input("tool name exceeds 128 bytes"));
            }
            let arguments = required_object(request, "arguments")?.clone();
            let remote = client
                .as_mut()
                .ok_or_else(|| ClientFailure::input("connect before MCP requests"))?;
            let result = remote.call_tool(name, arguments).await;
            if result.as_ref().err().is_some_and(|e| {
                !matches!(
                    mneme_mcp_client::classify_error(e.as_ref()),
                    mneme_mcp_client::ClientErrorClass::Tool
                )
            }) {
                remote.close().await;
                *client = None;
            }
            result.map_err(|e| map_remote(e.as_ref()))
        }
        "rpc" => {
            let method = required_string(request, "method")?;
            let params = required_object(request, "params")?.clone();
            let remote = client
                .as_mut()
                .ok_or_else(|| ClientFailure::input("connect before MCP requests"))?;
            let result = remote.raw_rpc(method, params).await;
            if result.as_ref().err().is_some_and(|e| {
                !matches!(
                    mneme_mcp_client::classify_error(e.as_ref()),
                    mneme_mcp_client::ClientErrorClass::Tool
                )
            }) {
                remote.close().await;
                *client = None;
            }
            result.map_err(|e| map_remote(e.as_ref()))
        }
        "capture/verified" => {
            let db = registry_name(request)?;
            let expected_db_id = expected_db_id(request)?;
            let payload = required_object(request, "payload")?;
            let prepared =
                PreparedCapture::parse(payload).map_err(|e| ClientFailure::input(e.to_string()))?;
            let source = prepared
                .expected_source()
                .map_err(|e| ClientFailure::input(e.to_string()))?;
            let expected_id = source.node_id().0.to_string();
            let remote = client
                .as_mut()
                .ok_or_else(|| ClientFailure::input("connect before MCP requests"))?;
            if payload.get("touchstone").is_some()
                && !crate::remote_touchstone::advertised(remote.tool_catalog(), "capture")
            {
                return Err(ClientFailure::protocol(
                    "server does not advertise native touchstone capture; refusing write without fallback",
                ));
            }
            if prepared.has_links() && !remote.supports_capture_links() {
                return Err(ClientFailure::protocol(
                    "server does not advertise compatible atomic capture links",
                ));
            }
            if expected_db_id.is_some()
                && (!remote.supports_expected_db_id("capture", None)
                    || !remote.supports_expected_db_id("get", None))
            {
                return Err(ClientFailure::protocol(
                    "server does not advertise expected_db_id for both capture and get; refusing guarded write",
                ));
            }
            let capture = remote
                .call_tool("capture", {
                    let mut payload = prepared.into_json();
                    payload["db"] = json!(db);
                    attach_expected_db_id(&mut payload, expected_db_id);
                    payload
                })
                .await;
            let capture = match capture {
                Ok(value) => value,
                Err(e) => {
                    if !matches!(
                        mneme_mcp_client::classify_error(e.as_ref()),
                        mneme_mcp_client::ClientErrorClass::Tool
                    ) {
                        remote.close().await;
                        *client = None;
                    }
                    return Err(map_remote(e.as_ref()));
                }
            };
            let mut accepted = json!({"id": capture.get("id"), "source": source, "replayed": capture.get("replayed"), "db":capture.get("db"), "db_id":capture.get("db_id"), "readback_status": "not_attempted", "retryable": false});
            if capture.get("id").and_then(Value::as_str) != Some(&expected_id)
                || capture.get("db").and_then(Value::as_str) != Some(db)
                || !matches_expected_db_id(&capture, expected_db_id)
                || capture.get("replayed").and_then(Value::as_bool).is_none()
                || capture
                    .get("db_id")
                    .and_then(Value::as_str)
                    .and_then(|value| ulid::Ulid::from_string(value).ok())
                    .is_none()
            {
                return Err(ClientFailure::protocol(
                    "capture returned malformed or unexpected database identity or receipt",
                )
                .with_accepted(&accepted));
            }
            accepted["readback_status"] = json!("failed");
            let mut arguments =
                json!({"db": db, "id": expected_id, "body": true, "max_body_bytes": 256 * 1024});
            attach_expected_db_id(&mut arguments, expected_db_id);
            let readback = remote.call_tool("get", arguments).await;
            let readback = match readback {
                Ok(value) => value,
                Err(e) => {
                    if !matches!(
                        mneme_mcp_client::classify_error(e.as_ref()),
                        mneme_mcp_client::ClientErrorClass::Tool
                    ) {
                        remote.close().await;
                        *client = None;
                    }
                    return Err(map_remote(e.as_ref()).with_accepted(&accepted));
                }
            };
            // Re-parse the same canonical payload for shared verification.
            let prepared =
                PreparedCapture::parse(payload).map_err(|e| ClientFailure::input(e.to_string()))?;
            accepted["readback_status"] = json!("mismatch");
            if expected_db_id.is_some()
                && (readback.get("db").and_then(Value::as_str) != Some(db)
                    || !matches_expected_db_id(&readback, expected_db_id))
            {
                return Err(ClientFailure::protocol(
                    "guarded capture readback returned a missing or changed database identity",
                )
                .with_accepted(&accepted));
            }
            prepared
                .verify_readback_json(&readback, capture["replayed"] == true)
                .map_err(|e| ClientFailure::protocol(e.to_string()).with_accepted(&accepted))?;
            let mut capture = capture;
            capture["readback_status"] = json!("verified");
            Ok(capture)
        }
        _ => Err(ClientFailure::input(format!(
            "unknown client operation {op:?}"
        ))),
    }
}

pub(crate) async fn run(args: ClientArgs) -> Result<(), AnyErr> {
    if !args.local_only {
        return Err("client requires --local-only".into());
    }
    let stdin = io::stdin();
    let mut reader = io::BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    let mut client = None;
    loop {
        let Some(bytes) = frame(&mut reader)? else {
            break;
        };
        let parsed: Result<Value, _> = serde_json::from_slice(&bytes);
        let (id, outcome) = match &parsed {
            Ok(request) => {
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let outcome = if bytes.len() > MAX_FRAME {
                    Err(ClientFailure::input("client frame exceeds 128 KiB"))
                } else if id.is_null() || !(id.is_string() || id.is_number()) {
                    Err(ClientFailure::input("id must be a string or number"))
                } else {
                    handle(&request, &mut client, &args).await
                };
                (id, outcome)
            }
            Err(_) => (
                Value::Null,
                Err(ClientFailure::input("invalid JSON client frame")),
            ),
        };
        let operation = parsed
            .as_ref()
            .ok()
            .and_then(|v| v.get("op"))
            .and_then(Value::as_str);
        let close = operation == Some("close");
        let advertise_bridge = operation == Some("connect") && outcome.is_ok();
        let mut reply = response(id.clone(), outcome);
        if advertise_bridge {
            // Local NDJSON envelope, deliberately outside the remote result.
            // An old bridge cannot acquire this capability by forwarding a
            // server-supplied field with the same name during initialize.
            reply["_mneme_client"] = json!({"expected_db_id": 1, "save": 1, "concern": 1});
        }
        let mut encoded = serde_json::to_vec(&reply)?;
        if encoded.len() > MAX_RESPONSE {
            encoded = serde_json::to_vec(&response(
                id,
                Err(ClientFailure::protocol("client response exceeds 512 KiB")),
            ))?;
        }
        writer.write_all(&encoded)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        if close {
            break;
        }
    }
    if let Some(mut remote) = client {
        remote.close().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_identity_is_optional_bounded_and_canonical() {
        assert_eq!(expected_db_id(&json!({})).unwrap(), None);
        let id = ulid::Ulid::from(73_u128);
        assert_eq!(
            expected_db_id(&json!({"expected_db_id": id.to_string()})).unwrap(),
            Some(id)
        );
        for value in [
            Value::Null,
            json!(true),
            json!(73),
            json!({}),
            json!(""),
            json!("0000000000000000000000002a"),
            json!("80000000000000000000000000"),
            json!("O0000000000000000000000029"),
            json!("0".repeat(27)),
        ] {
            assert!(expected_db_id(&json!({"expected_db_id": value})).is_err());
        }
    }

    fn body_chunk(body: &str, start: usize, more: bool) -> Value {
        let end = start + body.len();
        json!({
            "action":"get", "episode_id":"root", "edition_id":"edition", "db":"project", "db_id":"database",
            "current_edition_id":"edition", "is_current":true,
            "body":body, "body_range":{
                "source_start":start, "source_end":end,
                "has_more":more,"next_offset":if more {json!(end)} else {Value::Null},
            }
        })
    }

    #[test]
    fn episode_readback_joins_encoded_body_pages_with_byte_offsets() {
        let text = "\u{0001}\u{0002}\\\"雪".repeat(2000);
        assert!(text.len() <= 16 * 1024);
        assert!(
            serde_json::to_vec(&body_chunk(&text, 0, false))
                .unwrap()
                .len()
                > 32 * 1024
        );
        let split = 7000; // Whole UTF-8 pattern boundary.
        let mut collector = EpisodeReadback::new(text.len());
        assert!(!collector.push(body_chunk(&text[..split], 0, true)).unwrap());
        let mut last = body_chunk(&text[split..], split, false);
        last["current_edition_id"] = json!("a-newer-edition");
        last["is_current"] = json!(false);
        assert!(collector.push(last).unwrap());
        let result = collector.finish().unwrap();
        assert_eq!(result["body"], text);
        assert_eq!(result["body_range"]["source_end"], text.len());
        assert_eq!(result["body_range"]["next_offset"], Value::Null);
    }

    #[test]
    fn episode_readback_rejects_missing_progress_and_metadata_switches() {
        for chunk in [
            body_chunk("", 0, true),
            body_chunk("ab", 1, true),
            body_chunk("a", 0, false),
            body_chunk("abcde", 0, false),
        ] {
            assert!(EpisodeReadback::new(4).push(chunk).is_err());
        }
        for field in ["edition_id", "episode_id", "db_id"] {
            let mut collector = EpisodeReadback::new(4);
            collector.push(body_chunk("ab", 0, true)).unwrap();
            let mut changed = body_chunk("cd", 2, false);
            changed[field] = json!("different");
            assert!(collector.push(changed).is_err());
        }
        let mut collector = EpisodeReadback::new(4);
        collector.push(body_chunk("ab", 0, true)).unwrap();
        assert!(collector.finish().is_err());
        let mut empty = EpisodeReadback::new(0);
        assert!(empty.push(body_chunk("", 0, false)).unwrap());
        assert_eq!(empty.finish().unwrap()["body"], "");
    }
}
