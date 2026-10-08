//! Hardened Streamable-HTTP transport (MCP, stable spec 2025-11-25).
//!
//! The endpoint is deliberately local-only unless a bearer token is configured.
//! HTTP sessions are validated after `initialize`, request bodies are bounded, and
//! CORS is disabled unless one exact browser origin is explicitly allowed.

use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::Value;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, oneshot};

use crate::host::AnyErr;
use crate::response::BoundedResponse;
use crate::{SUPPORTED_PROTOCOL_VERSIONS, Server};

const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
/// Process-wide HTTP work admitted by this server. The POST handler acquires a
/// permit before aggregating its body, so rejected requests cannot each allocate
/// a complete 2 MiB request inside the application.
const MAX_HTTP_IN_FLIGHT: usize = 32;
const HTTP_SESSION_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_HTTP_SESSIONS: usize = 64;
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_RELEASE_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_ADMISSION_POLL: Duration = Duration::from_millis(25);

#[derive(Clone)]
struct Security {
    bearer_token: Option<Arc<str>>,
    cors_origin: Option<HeaderValue>,
}

struct Shared {
    server: Server,
    security: Security,
    http_admission: Arc<Semaphore>,
    /// Protocol sessions, separate from the walk-session map held by `Server`.
    http_sessions: Mutex<HashMap<String, HttpSession>>,
}

struct AdmittedBody {
    bytes: Bytes,
    /// Held through dispatch and response construction, not merely aggregation.
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyAdmissionError {
    Busy,
    Rejected,
}

#[derive(Clone)]
struct HttpSession {
    touched: Instant,
    protocol_version: &'static str,
}

type AppState = Arc<Shared>;

pub async fn serve(
    server: Server,
    addr: &str,
    bearer_token: Option<String>,
    cors_origin: Option<&str>,
) -> Result<(), AnyErr> {
    let addr: SocketAddr = addr
        .parse()
        .map_err(|e| format!("--http expects a numeric IP:PORT, got {addr:?}: {e}"))?;
    if bearer_token.as_deref().is_some_and(str::is_empty) {
        return Err("the HTTP bearer token may not be empty".into());
    }
    if !addr.ip().is_loopback() && bearer_token.is_none() {
        return Err(format!(
            "refusing unauthenticated non-loopback bind {addr}; set --http-token-env VAR"
        )
        .into());
    }
    let cors_origin = cors_origin
        .map(HeaderValue::from_str)
        .transpose()
        .map_err(|e| format!("invalid --cors-origin header value: {e}"))?;
    let shared = Arc::new(Shared {
        server,
        security: Security {
            bearer_token: bearer_token.map(Arc::<str>::from),
            cors_origin,
        },
        http_admission: Arc::new(Semaphore::new(MAX_HTTP_IN_FLIGHT)),
        http_sessions: Mutex::new(HashMap::new()),
    });
    let app = Router::new()
        .route(
            "/",
            post(post_handler)
                .get(get_handler)
                .delete(delete_handler)
                .options(preflight),
        )
        .with_state(shared.clone());
    // Register synchronously before the listener becomes available. Creating an
    // unpolled ctrl_c future here would leave an early-signal startup race.
    let shutdown = shutdown_signal()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_until_shutdown(
        listener,
        app,
        shared,
        shutdown,
        SHUTDOWN_DRAIN_TIMEOUT,
        SHUTDOWN_RELEASE_TIMEOUT,
    )
    .await
}

#[cfg(unix)]
fn shutdown_signal() -> std::io::Result<impl Future<Output = ()> + Send> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
    })
}

#[cfg(windows)]
fn shutdown_signal() -> std::io::Result<impl Future<Output = ()> + Send> {
    let mut interrupt = tokio::signal::windows::ctrl_c()?;
    Ok(async move {
        interrupt.recv().await;
    })
}

#[cfg(not(any(unix, windows)))]
fn shutdown_signal() -> std::io::Result<std::future::Ready<()>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "HTTP shutdown signals are unsupported on this target",
    ))
}

async fn serve_until_shutdown(
    listener: tokio::net::TcpListener,
    app: Router,
    shared: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
    drain_timeout: Duration,
    release_timeout: Duration,
) -> Result<(), AnyErr> {
    let (draining, started) = oneshot::channel();
    let serving = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            eprintln!("HTTP shutdown requested; draining active requests");
            let _ = draining.send(());
        })
        .into_future();
    tokio::pin!(serving);
    tokio::select! {
        result = &mut serving => result?,
        _ = started => {
            tokio::time::timeout(drain_timeout, &mut serving).await.map_err(|_| {
                "HTTP shutdown incomplete: request drain timed out; databases were not cleanly released"
            })??;
        }
    }

    // The HTTP listener and requests are gone. Walks and receipts are volatile
    // process state; discard them without training, then release every slot.
    // begin_release still fences cancellation-surviving backend/engine work.
    release_shutdown_databases(&shared, release_timeout).await?;
    eprintln!("HTTP shutdown complete: all databases cleanly released");
    Ok(())
}

async fn release_shutdown_databases(
    shared: &Shared,
    admission_timeout: Duration,
) -> Result<(), AnyErr> {
    if let Some(coordinator) = &shared.server.library {
        coordinator.close().await;
        shared.http_sessions.lock().await.clear();
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + admission_timeout;
    let mut sessions = tokio::time::timeout_at(deadline, shared.server.sessions.lock())
        .await
        .map_err(|_| "HTTP shutdown incomplete: session state did not quiesce")?;
    *sessions = crate::SessionState::default();
    drop(sessions);
    shared.http_sessions.lock().await.clear();

    for (name, slot) in &shared.server.registry.dbs {
        loop {
            // Never resume/reopen a database released by an operator.
            if slot.status()?.state == "maintenance" {
                break;
            }
            match slot.begin_release() {
                Ok(release) => {
                    // Checkpointing is blocking I/O and cannot safely be forcibly
                    // canceled. The worker retains the fence and exact lease
                    // until finish completes; only admission waiting is timed.
                    tokio::task::spawn_blocking(move || release.finish())
                        .await
                        .map_err(|error| format!("HTTP shutdown incomplete: database {name:?} release worker failed: {error}"))?
                        .map_err(|error| format!("HTTP shutdown incomplete: database {name:?} checkpoint failed: {error}"))?;
                    break;
                }
                Err(error) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(format!(
                            "HTTP shutdown incomplete: database {name:?} release admission timed out: {error}"
                        ).into());
                    }
                    tokio::time::sleep_until(
                        deadline.min(tokio::time::Instant::now() + SHUTDOWN_ADMISSION_POLL),
                    )
                    .await;
                }
            }
        }
    }
    Ok(())
}

async fn post_handler(State(shared): State<AppState>, headers: HeaderMap, body: Body) -> Response {
    let response_headers = cors_headers(&shared.security, &headers);
    if !origin_allowed(&shared.security, &headers) {
        return (
            StatusCode::FORBIDDEN,
            response_headers,
            "origin is not allowed",
        )
            .into_response();
    }
    if !authorized(&shared.security, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            response_headers,
            "missing or invalid bearer token",
        )
            .into_response();
    }
    if !accepts_streamable_post(&headers) {
        return (
            StatusCode::NOT_ACCEPTABLE,
            response_headers,
            "Accept must include application/json and text/event-stream",
        )
            .into_response();
    }
    if !has_json_content_type(&headers) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            response_headers,
            "Content-Type must be application/json",
        )
            .into_response();
    }

    // Admission happens before `to_bytes`: accepting `Bytes` as an extractor
    // would aggregate the body before the handler got a chance to shed load.
    let body = match aggregate_admitted_body(&shared.http_admission, body).await {
        Ok(body) => body,
        Err(BodyAdmissionError::Busy) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                response_headers,
                "too many in-flight HTTP requests",
            )
                .into_response();
        }
        Err(BodyAdmissionError::Rejected) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                response_headers,
                format!("request body exceeds {MAX_REQUEST_BYTES} bytes or could not be read"),
            )
                .into_response();
        }
    };

    let msg: Value = match serde_json::from_slice(&body.bytes) {
        Ok(v) => v,
        Err(e) => {
            return json_status(
                StatusCode::BAD_REQUEST,
                BoundedResponse::new(crate::error(
                    Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                )),
                response_headers,
            );
        }
    };

    // Streamable HTTP carries exactly one JSON-RPC message per POST. JSON-RPC
    // batching would make initialization/session state ambiguous and is not MCP.
    if msg.is_array() {
        return (
            StatusCode::BAD_REQUEST,
            response_headers,
            "MCP Streamable HTTP does not accept JSON-RPC batches",
        )
            .into_response();
    }
    let is_init = msg.get("method").and_then(Value::as_str) == Some("initialize");
    if is_init && (msg.get("id").is_none() || headers.contains_key("mcp-session-id")) {
        return (
            StatusCode::BAD_REQUEST,
            response_headers,
            "initialize must be a request without mcp-session-id",
        )
            .into_response();
    }
    if !is_init {
        match validate_session(&shared, &headers).await {
            Ok(()) => {}
            Err(SessionError::Missing) => {
                return (
                    StatusCode::BAD_REQUEST,
                    response_headers,
                    "mcp-session-id is required after initialize",
                )
                    .into_response();
            }
            Err(SessionError::Unknown) => {
                return (
                    StatusCode::NOT_FOUND,
                    response_headers,
                    "unknown or expired mcp-session-id",
                )
                    .into_response();
            }
            Err(SessionError::BadProtocol) => {
                return (
                    StatusCode::BAD_REQUEST,
                    response_headers,
                    "unsupported or mismatched mcp-protocol-version",
                )
                    .into_response();
            }
        }
    }

    let out = shared.server.handle(msg).await;

    match out {
        None => (StatusCode::ACCEPTED, response_headers).into_response(),
        Some(response) => {
            let negotiated = is_init
                .then(|| response_protocol_version(response.value()))
                .flatten();
            let mut resp = json(response, response_headers);
            // A malformed/failed initialize response must not mint a usable
            // session. Read the negotiated revision from the successful result.
            if let Some(version) = negotiated {
                let id = register_session(&shared, version).await;
                if let Ok(id) = HeaderValue::from_str(&id) {
                    resp.headers_mut().insert("mcp-session-id", id);
                }
            }
            resp
        }
    }
}

async fn aggregate_admitted_body(
    admission: &Arc<Semaphore>,
    body: Body,
) -> Result<AdmittedBody, BodyAdmissionError> {
    let permit = admission
        .clone()
        .try_acquire_owned()
        .map_err(|_| BodyAdmissionError::Busy)?;
    let bytes = axum::body::to_bytes(body, MAX_REQUEST_BYTES)
        .await
        .map_err(|_| BodyAdmissionError::Rejected)?;
    Ok(AdmittedBody {
        bytes,
        _permit: permit,
    })
}

async fn get_handler(State(shared): State<AppState>, headers: HeaderMap) -> Response {
    let response_headers = cors_headers(&shared.security, &headers);
    if !origin_allowed(&shared.security, &headers) {
        return (
            StatusCode::FORBIDDEN,
            response_headers,
            "origin is not allowed",
        )
            .into_response();
    }
    if !authorized(&shared.security, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            response_headers,
            "missing or invalid bearer token",
        )
            .into_response();
    }
    (
        StatusCode::METHOD_NOT_ALLOWED,
        response_headers,
        "this server offers no server-initiated SSE stream",
    )
        .into_response()
}

async fn delete_handler(State(shared): State<AppState>, headers: HeaderMap) -> Response {
    let response_headers = cors_headers(&shared.security, &headers);
    if !origin_allowed(&shared.security, &headers) {
        return (
            StatusCode::FORBIDDEN,
            response_headers,
            "origin is not allowed",
        )
            .into_response();
    }
    if !authorized(&shared.security, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            response_headers,
            "missing or invalid bearer token",
        )
            .into_response();
    }
    match validate_session(&shared, &headers).await {
        Ok(()) => {
            let id = headers
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
                .expect("validated session has an id");
            shared.http_sessions.lock().await.remove(id);
            (StatusCode::NO_CONTENT, response_headers).into_response()
        }
        Err(SessionError::Missing) => (
            StatusCode::BAD_REQUEST,
            response_headers,
            "mcp-session-id is required",
        )
            .into_response(),
        Err(SessionError::Unknown) => (
            StatusCode::NOT_FOUND,
            response_headers,
            "unknown or expired mcp-session-id",
        )
            .into_response(),
        Err(SessionError::BadProtocol) => (
            StatusCode::BAD_REQUEST,
            response_headers,
            "unsupported or mismatched mcp-protocol-version",
        )
            .into_response(),
    }
}

async fn preflight(State(shared): State<AppState>, headers: HeaderMap) -> Response {
    let allowed = shared
        .security
        .cors_origin
        .as_ref()
        .zip(headers.get("origin"))
        .is_some_and(|(allowed, actual)| allowed == actual);
    if !allowed {
        return StatusCode::FORBIDDEN.into_response();
    }
    (
        StatusCode::NO_CONTENT,
        cors_headers(&shared.security, &headers),
    )
        .into_response()
}

fn authorized(security: &Security, headers: &HeaderMap) -> bool {
    let Some(expected) = security.bearer_token.as_deref() else {
        return true;
    };
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(|actual| constant_time_eq(actual.as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(actual: &[u8], expected: &[u8]) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    actual
        .iter()
        .zip(expected)
        .fold(0u8, |different, (a, b)| different | (a ^ b))
        == 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionError {
    Missing,
    Unknown,
    BadProtocol,
}

async fn validate_session(shared: &Shared, headers: &HeaderMap) -> Result<(), SessionError> {
    let id = headers
        .get("mcp-session-id")
        .and_then(|h| h.to_str().ok())
        .ok_or(SessionError::Missing)?;
    let now = Instant::now();
    let mut sessions = shared.http_sessions.lock().await;
    sessions.retain(|_, session| now.duration_since(session.touched) < HTTP_SESSION_TTL);
    let session = sessions.get_mut(id).ok_or(SessionError::Unknown)?;
    if let Some(version) = headers.get("mcp-protocol-version") {
        let version = version.to_str().map_err(|_| SessionError::BadProtocol)?;
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) || version != session.protocol_version {
            return Err(SessionError::BadProtocol);
        }
    }
    session.touched = now;
    Ok(())
}

async fn register_session(shared: &Shared, protocol_version: &'static str) -> String {
    let now = Instant::now();
    let mut sessions = shared.http_sessions.lock().await;
    sessions.retain(|_, session| now.duration_since(session.touched) < HTTP_SESSION_TTL);
    while sessions.len() >= MAX_HTTP_SESSIONS {
        let Some(oldest) = sessions
            .iter()
            .min_by_key(|(_, session)| session.touched)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        sessions.remove(&oldest);
    }
    let id = ulid::Ulid::new().to_string();
    sessions.insert(
        id.clone(),
        HttpSession {
            touched: now,
            protocol_version,
        },
    );
    id
}

fn response_protocol_version(response: &Value) -> Option<&'static str> {
    let version = response
        .pointer("/result/protocolVersion")
        .and_then(Value::as_str)?;
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .copied()
        .find(|supported| *supported == version)
}

/// Requests without an Origin are ordinary non-browser MCP traffic. When a
/// browser supplies Origin it must exactly match the one configured by the
/// operator; merely omitting CORS response headers does not prevent DNS rebinding.
fn origin_allowed(security: &Security, headers: &HeaderMap) -> bool {
    let Some(actual) = headers.get("origin") else {
        return true;
    };
    security
        .cors_origin
        .as_ref()
        .is_some_and(|allowed| allowed == actual)
}

fn accepts_streamable_post(headers: &HeaderMap) -> bool {
    let mut json = false;
    let mut sse = false;
    for value in headers.get_all("accept") {
        let Ok(value) = value.to_str() else {
            return false;
        };
        for item in value.split(',') {
            match item
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "application/json" => json = true,
                "text/event-stream" => sse = true,
                _ => {}
            }
        }
    }
    json && sse
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn json(response: BoundedResponse, mut headers: HeaderMap) -> Response {
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let mut out = Response::new(Body::from(response.into_bytes()));
    *out.headers_mut() = headers;
    out
}

fn json_status(status: StatusCode, response: BoundedResponse, headers: HeaderMap) -> Response {
    let mut out = json(response, headers);
    *out.status_mut() = status;
    out
}

fn cors_headers(security: &Security, request: &HeaderMap) -> HeaderMap {
    let mut h = HeaderMap::new();
    let origin_matches = security
        .cors_origin
        .as_ref()
        .zip(request.get("origin"))
        .is_some_and(|(allowed, actual)| allowed == actual);
    if origin_matches {
        h.insert(
            "access-control-allow-origin",
            security.cors_origin.clone().expect("checked above"),
        );
        h.insert("vary", HeaderValue::from_static("Origin"));
        h.insert(
            "access-control-allow-methods",
            HeaderValue::from_static("POST, GET, DELETE, OPTIONS"),
        );
        h.insert(
            "access-control-allow-headers",
            HeaderValue::from_static(
                "authorization, content-type, mcp-session-id, mcp-protocol-version",
            ),
        );
        h.insert(
            "access-control-expose-headers",
            HeaderValue::from_static("mcp-session-id"),
        );
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn library_mode_http_uses_the_same_closed_catalog_and_dispatch() {
        let root = std::env::temp_dir().join(format!("mneme-library-http-{}", ulid::Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let config = root.join("config.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "schema":"mneme.library.config.v1", "library_id":"lib", "device_id":"device",
                "catalog_path":"catalog.json"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("catalog.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema":"mneme.library.catalog.v1", "library_id":"lib", "revision":1, "entries":[]
            }))
            .unwrap(),
        )
        .unwrap();
        let shared = Arc::new(Shared {
            server: crate::Server {
                registry: crate::Registry {
                    activity: crate::activity::ActivityRing::default(),
                    dbs: std::collections::BTreeMap::new(),
                },
                library: Some(crate::Coordinator::Library(
                    crate::library_server::LibraryServer::from_path(&config).unwrap(),
                )),
                sessions: Arc::new(Mutex::new(crate::SessionState::default())),
                cold_work: crate::ColdWorkGate::default(),
                capability: crate::CapabilityPolicy::new(crate::CapabilityProfile::ReadOnly, false),
            },
            security: Security {
                bearer_token: None,
                cors_origin: None,
            },
            http_admission: Arc::new(Semaphore::new(MAX_HTTP_IN_FLIGHT)),
            http_sessions: Mutex::new(HashMap::new()),
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let init = post_handler(State(shared.clone()), headers.clone(), Body::from(serde_json::to_vec(&serde_json::json!({
            "jsonrpc":"2.0", "id":"init", "method":"initialize", "params":{"protocolVersion":crate::PROTOCOL_VERSION}
        })).unwrap())).await;
        assert_eq!(init.status(), StatusCode::OK);
        let session = init.headers().get("mcp-session-id").unwrap().clone();
        assert_eq!(
            response_json(init).await["result"]["capabilityProfile"],
            "library-read-only"
        );
        headers.insert("mcp-session-id", session);
        headers.insert(
            "mcp-protocol-version",
            HeaderValue::from_static(crate::PROTOCOL_VERSION),
        );
        for (method, name, expected_error) in [
            ("tools/list", "", false),
            ("tools/call", "library_catalog", false),
            ("tools/call", "snapshot_create", true),
        ] {
            let frame = post_handler(State(shared.clone()), headers.clone(), Body::from(serde_json::to_vec(&serde_json::json!({
                "jsonrpc":"2.0", "id":name, "method":method,
                "params": if method == "tools/list" { serde_json::json!({}) } else { serde_json::json!({"name":name,"arguments":{}}) }
            })).unwrap())).await;
            assert_eq!(frame.status(), StatusCode::OK);
            let frame = response_json(frame).await;
            if method == "tools/list" {
                assert_eq!(frame["result"]["tools"].as_array().unwrap().len(), 4);
                assert!(
                    frame["result"]["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|tool| tool["name"].as_str().unwrap().starts_with("library_"))
                );
            } else {
                assert_eq!(frame["result"]["isError"], expected_error);
            }
        }
        drop(shared);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn shutdown_fixture() -> (std::path::PathBuf, AppState) {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mneme-http-shutdown-{}", ulid::Ulid::new()));
        let mut sessions = crate::SessionState::default();
        let registry = crate::build_registry(
            vec![
                ("a".into(), root.join("a.db")),
                ("b".into(), root.join("b.db")),
            ],
            &mut sessions,
        )
        .unwrap();
        let shared = Arc::new(Shared {
            server: crate::Server {
                registry,
                sessions: Arc::new(Mutex::new(sessions)),
                cold_work: crate::ColdWorkGate::new(Duration::ZERO),
                capability: crate::CapabilityPolicy::new(crate::CapabilityProfile::ReadOnly, false),
                library: None,
            },
            security: Security {
                bearer_token: None,
                cors_origin: None,
            },
            http_admission: Arc::new(Semaphore::new(MAX_HTTP_IN_FLIGHT)),
            http_sessions: Mutex::new(HashMap::new()),
        });
        (root, shared)
    }

    #[tokio::test]
    async fn shutdown_releases_all_databases_without_reopening_maintenance() {
        let (root, shared) = shutdown_fixture();
        let a = shared.server.registry.slot("a").unwrap();
        a.begin_release().unwrap().finish().unwrap();
        let offline_a = mneme_store_path::StoreLease::acquire(&root.join("a.db")).unwrap();
        shared.http_sessions.lock().await.insert(
            "http-session".into(),
            HttpSession {
                touched: Instant::now(),
                protocol_version: crate::PROTOCOL_VERSION,
            },
        );
        release_shutdown_databases(&shared, Duration::from_secs(2))
            .await
            .unwrap();
        for (_, status) in shared.server.registry.catalog().unwrap() {
            assert_eq!(status.state, "maintenance");
            assert_eq!(status.in_flight, 0);
        }
        assert!(shared.http_sessions.lock().await.is_empty());
        assert!(shared.server.sessions.lock().await.is_empty());
        let offline_b = mneme_store_path::StoreLease::acquire(&root.join("b.db")).unwrap();
        drop((offline_a, offline_b, shared));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn shutdown_admission_timeout_retains_the_busy_database_lease() {
        let (root, shared) = shutdown_fixture();
        let checkout = shared.server.registry.checkout("a").unwrap();
        let error = release_shutdown_databases(&shared, Duration::from_millis(25))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("release admission timed out"), "{error}");
        assert!(error.contains("in-flight MCP operation"), "{error}");
        assert_eq!(
            shared
                .server
                .registry
                .slot("a")
                .unwrap()
                .status()
                .unwrap()
                .state,
            "open"
        );
        assert!(mneme_store_path::StoreLease::acquire(&root.join("a.db")).is_err());
        drop(checkout);
        // An explicit later cleanup attempt can release after the holder ends.
        release_shutdown_databases(&shared, Duration::from_secs(2))
            .await
            .unwrap();
        drop(shared);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn shutdown_drains_requests_before_release_and_reports_a_drain_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for complete_request in [true, false] {
            let (root, shared) = shutdown_fixture();
            let entered = Arc::new(tokio::sync::Notify::new());
            let finish = Arc::new(tokio::sync::Notify::new());
            let app = Router::new().route(
                "/",
                post({
                    let entered = entered.clone();
                    let finish = finish.clone();
                    move || {
                        let entered = entered.clone();
                        let finish = finish.clone();
                        async move {
                            entered.notify_one();
                            finish.notified().await;
                            "done"
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (signal, shutdown) = oneshot::channel();
            let mut serving = tokio::spawn(serve_until_shutdown(
                listener,
                app,
                shared.clone(),
                async {
                    let _ = shutdown.await;
                },
                if complete_request {
                    Duration::from_secs(2)
                } else {
                    Duration::from_millis(50)
                },
                Duration::from_secs(2),
            ));
            let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
            client.write_all(b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .unwrap();
            signal.send(()).unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut serving)
                    .await
                    .is_err()
            );
            assert_eq!(
                shared
                    .server
                    .registry
                    .slot("a")
                    .unwrap()
                    .status()
                    .unwrap()
                    .state,
                "open"
            );
            if complete_request {
                finish.notify_one();
                serving.await.unwrap().unwrap();
                assert_eq!(
                    shared
                        .server
                        .registry
                        .slot("a")
                        .unwrap()
                        .status()
                        .unwrap()
                        .state,
                    "maintenance"
                );
            } else {
                let error = serving.await.unwrap().unwrap_err().to_string();
                assert!(error.contains("request drain timed out"), "{error}");
                assert_eq!(
                    shared
                        .server
                        .registry
                        .slot("a")
                        .unwrap()
                        .status()
                        .unwrap()
                        .state,
                    "open"
                );
                assert!(mneme_store_path::StoreLease::acquire(&root.join("a.db")).is_err());
                finish.notify_one();
            }
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
                .await
                .unwrap()
                .unwrap();
            release_shutdown_databases(&shared, Duration::from_secs(2))
                .await
                .unwrap();
            drop(shared);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    async fn response_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), MAX_REQUEST_BYTES)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn bearer_auth_is_exact() {
        let security = Security {
            bearer_token: Some(Arc::from("secret")),
            cors_origin: None,
        };
        let mut headers = HeaderMap::new();
        assert!(!authorized(&security, &headers));
        headers.insert("authorization", HeaderValue::from_static("Bearer nope"));
        assert!(!authorized(&security, &headers));
        headers.insert("authorization", HeaderValue::from_static("Bearer secret"));
        assert!(authorized(&security, &headers));
    }

    #[test]
    fn cors_is_exact_origin_and_never_wildcard() {
        let security = Security {
            bearer_token: None,
            cors_origin: Some(HeaderValue::from_static("https://example.test")),
        };
        let mut headers = HeaderMap::new();
        headers.insert("origin", HeaderValue::from_static("https://evil.test"));
        assert!(!origin_allowed(&security, &headers));
        assert!(cors_headers(&security, &headers).is_empty());
        headers.insert("origin", HeaderValue::from_static("https://example.test"));
        assert!(origin_allowed(&security, &headers));
        assert_eq!(
            cors_headers(&security, &headers)
                .get("access-control-allow-origin")
                .unwrap(),
            "https://example.test"
        );
    }

    #[test]
    fn requests_without_origin_are_non_browser_traffic() {
        let security = Security {
            bearer_token: None,
            cors_origin: None,
        };
        assert!(origin_allowed(&security, &HeaderMap::new()));

        let mut browser = HeaderMap::new();
        browser.insert("origin", HeaderValue::from_static("https://example.test"));
        assert!(!origin_allowed(&security, &browser));
    }

    #[test]
    fn streamable_http_headers_are_strict() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "accept",
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(
            "content-type",
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        assert!(accepts_streamable_post(&headers));
        assert!(has_json_content_type(&headers));

        headers.insert("accept", HeaderValue::from_static("application/json"));
        assert!(!accepts_streamable_post(&headers));
        headers.insert("content-type", HeaderValue::from_static("text/plain"));
        assert!(!has_json_content_type(&headers));
    }

    #[tokio::test]
    async fn http_reuses_the_bounded_response_bytes_without_reserializing() {
        let bounded = BoundedResponse::new(serde_json::json!({
            "jsonrpc": "2.0",
            "id": "http-bytes",
            "result": { "escaped": "quote\" slash\\ newline\n snowman ☃" },
        }));
        let expected = bounded.bytes().to_vec();
        let response = json(bounded, HeaderMap::new());
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let actual = axum::body::to_bytes(response.into_body(), expected.len() + 1)
            .await
            .unwrap();
        assert_eq!(actual.as_ref(), expected.as_slice());
    }

    #[tokio::test]
    async fn http_admission_happens_before_body_aggregation() {
        let admission = Arc::new(Semaphore::new(1));
        let _occupied = admission.clone().acquire_owned().await.unwrap();
        let oversized = Body::from(vec![0; MAX_REQUEST_BYTES + 1]);

        let result = aggregate_admitted_body(&admission, oversized).await;

        assert!(matches!(result, Err(BodyAdmissionError::Busy)));
    }

    #[tokio::test]
    async fn rejected_http_body_releases_its_admission() {
        let admission = Arc::new(Semaphore::new(1));
        let oversized = Body::from(vec![0; MAX_REQUEST_BYTES + 1]);

        let result = aggregate_admitted_body(&admission, oversized).await;

        assert!(matches!(result, Err(BodyAdmissionError::Rejected)));
        assert_eq!(admission.available_permits(), 1);
    }

    #[tokio::test]
    async fn admitted_http_body_holds_permit_until_dispatch_is_done() {
        let admission = Arc::new(Semaphore::new(1));
        let admitted = aggregate_admitted_body(&admission, Body::from("request"))
            .await
            .unwrap();

        assert_eq!(admitted.bytes.as_ref(), b"request");
        assert!(admission.clone().try_acquire_owned().is_err());

        drop(admitted);
        assert!(admission.clone().try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn http_catalog_and_dispatch_share_the_receipt_grounded_profile() {
        let shared = Arc::new(Shared {
            server: crate::Server {
                registry: crate::Registry {
                    activity: crate::activity::ActivityRing::default(),
                    dbs: std::collections::BTreeMap::new(),
                },
                sessions: Arc::new(Mutex::new(crate::SessionState::default())),
                cold_work: crate::ColdWorkGate::new(Duration::ZERO),
                capability: crate::CapabilityPolicy::new(
                    crate::CapabilityProfile::ReceiptGrounded,
                    false,
                ),
                library: None,
            },
            security: Security {
                bearer_token: None,
                cors_origin: None,
            },
            http_admission: Arc::new(Semaphore::new(MAX_HTTP_IN_FLIGHT)),
            http_sessions: Mutex::new(HashMap::new()),
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let initialize = post_handler(
            State(shared.clone()),
            headers.clone(),
            Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "init",
                    "method": "initialize",
                    "params": { "protocolVersion": crate::PROTOCOL_VERSION },
                }))
                .unwrap(),
            ),
        )
        .await;
        assert_eq!(initialize.status(), StatusCode::OK);
        let session = initialize
            .headers()
            .get("mcp-session-id")
            .expect("successful initialize creates an HTTP session")
            .clone();
        let initialize = response_json(initialize).await;
        assert_eq!(
            initialize["result"]["capabilityProfile"],
            "receipt-grounded"
        );

        headers.insert("mcp-session-id", session);
        headers.insert(
            "mcp-protocol-version",
            HeaderValue::from_static(crate::PROTOCOL_VERSION),
        );
        let listed = post_handler(
            State(shared.clone()),
            headers.clone(),
            Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "list",
                    "method": "tools/list",
                    "params": {},
                }))
                .unwrap(),
            ),
        )
        .await;
        assert_eq!(listed.status(), StatusCode::OK);
        let listed = response_json(listed).await;
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|schema| schema["name"] == "activity"));
        assert!(tools.iter().any(|schema| schema["name"] == "reflect"));
        assert!(tools.iter().all(|schema| schema["name"] != "ingest"));
        let database_control = tools
            .iter()
            .find(|schema| schema["name"] == "database_control")
            .unwrap();
        assert_eq!(
            database_control["inputSchema"]["properties"]["action"]["enum"],
            serde_json::json!(["status"])
        );

        for arguments in [
            serde_json::json!({"limit":1}),
            serde_json::json!({"limit":65}),
        ] {
            let response = post_handler(
                State(shared.clone()),
                headers.clone(),
                Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "jsonrpc":"2.0", "id":"activity", "method":"tools/call",
                        "params":{"name":"activity","arguments":arguments}
                    }))
                    .unwrap(),
                ),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let response = response_json(response).await;
            assert_eq!(response["result"]["isError"], arguments["limit"] == 65);
            if arguments["limit"] == 1 {
                let payload: serde_json::Value = serde_json::from_str(
                    response["result"]["content"][0]["text"].as_str().unwrap(),
                )
                .unwrap();
                assert_eq!(payload["schema"], "mneme.activity.v1");
                assert_eq!(payload["events"], serde_json::json!([]));
                assert_eq!(payload["latest_seq"], 0);
            }
        }

        let denied = post_handler(
            State(shared),
            headers,
            Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "denied",
                    "method": "tools/call",
                    "params": {
                        "name": "database_control",
                        "arguments": { "db": "missing", "action": "release" },
                    },
                }))
                .unwrap(),
            ),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::OK);
        let denied = response_json(denied).await;
        assert_eq!(denied["result"]["isError"], true);
        let error = denied["result"]["content"][0]["text"].as_str().unwrap();
        assert!(error.contains("capability profile"), "{error}");
        assert!(!error.contains("unknown database"), "{error}");
    }
}
