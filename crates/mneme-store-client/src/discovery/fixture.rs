//! Disposable native-owner protocol fixture. Never opens a database or launches a service.
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) const DB: &str = "01M3CPEVDTM9PP4HJTMMBZ8HR9";

pub(crate) struct Options {
    pub profile: &'static str,
    pub server_name: &'static str,
    pub advertised_discovery: bool,
    pub database_id: &'static str,
    pub database_state: &'static str,
    pub alias: &'static str,
    /// Delay only the discovery result, interruptible when the fixture is dropped.
    pub response_delay: Duration,
    /// Inflate tools/list, independently of the mutable discovery document.
    pub catalog_padding_bytes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            profile: "operator",
            server_name: "mneme-mcp",
            advertised_discovery: true,
            database_id: DB,
            database_state: "open",
            alias: "project",
            response_delay: Duration::ZERO,
            catalog_padding_bytes: 0,
        }
    }
}

pub(crate) struct Fixture {
    pub url: String,
    /// Number of discovery_catalog calls, including calls whose response times out.
    pub calls: Arc<AtomicUsize>,
    pub catalog_calls: Arc<AtomicUsize>,
    pub database_calls: Arc<AtomicUsize>,
    /// Captured tools/call params, including databases identity checks.
    pub requests: Arc<Mutex<Vec<Value>>>,
    document: Arc<Mutex<Value>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Fixture {
    pub fn new(document: Value) -> Self {
        Self::with_options(document, Options::default())
    }

    pub fn with_options(document: Value, options: Options) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let catalog_calls = Arc::new(AtomicUsize::new(0));
        let database_calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let document = Arc::new(Mutex::new(document));
        let stop = Arc::new(AtomicBool::new(false));
        let server_calls = calls.clone();
        let server_catalog_calls = catalog_calls.clone();
        let server_database_calls = database_calls.clone();
        let server_requests = requests.clone();
        let server_document = document.clone();
        let server_stop = stop.clone();
        let join = thread::spawn(move || {
            while !server_stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::park_timeout(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(20)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                let Some(msg) = read_request(&mut stream, &server_stop) else {
                    continue;
                };
                let result = match msg["method"].as_str() {
                    Some("initialize") => Some(json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": options.server_name, "version": "fixture"},
                        "capabilityProfile": options.profile,
                    })),
                    Some("tools/list") => {
                        server_catalog_calls.fetch_add(1, Ordering::Relaxed);
                        Some(json!({"tools": advertised_tools(&options)}))
                    }
                    Some("tools/call") => {
                        server_requests.lock().unwrap().push(msg["params"].clone());
                        let value = match msg["params"]["name"].as_str() {
                            Some("databases") => {
                                server_database_calls.fetch_add(1, Ordering::Relaxed);
                                json!([{
                                    "db": options.alias,
                                    "db_id": options.database_id,
                                    "state": options.database_state,
                                    "resolved_path": "/fixture/project.db",
                                }])
                            }
                            Some("discovery_catalog") if options.advertised_discovery => {
                                server_calls.fetch_add(1, Ordering::Relaxed);
                                if !delay(options.response_delay, &server_stop) {
                                    break;
                                }
                                server_document.lock().unwrap().clone()
                            }
                            other => panic!("unexpected fixture tool call: {other:?}"),
                        };
                        Some(json!({"content": [{
                            "type": "text",
                            "text": serde_json::to_string(&value).unwrap(),
                        }]}))
                    }
                    _ => None,
                };
                let wire = result.map(|result| {
                    serde_json::to_string(&json!({
                        "jsonrpc": "2.0", "id": msg["id"], "result": result,
                    }))
                    .unwrap()
                });
                let status = if wire.is_some() {
                    "200 OK"
                } else {
                    "202 Accepted"
                };
                let wire = wire.unwrap_or_default();
                // Budget tests deliberately disconnect before consuming the response.
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nMcp-Session-Id: discovery-fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{wire}",
                    wire.len()
                );
            }
        });
        Self {
            url,
            calls,
            catalog_calls,
            database_calls,
            requests,
            document,
            stop,
            join: Some(join),
        }
    }

    pub fn set_document(&self, document: Value) {
        *self.document.lock().unwrap() = document;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            join.thread().unpark();
            let result = join.join();
            // Preserve the originating test failure rather than panicking twice.
            if !thread::panicking() {
                result.expect("discovery fixture thread failed");
            }
        }
    }
}

fn advertised_tools(options: &Options) -> Value {
    let mut tools = vec![json!({
        "name": "databases",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
    })];
    if options.advertised_discovery {
        tools.push(json!({
            "name": "discovery_catalog",
            "description": "x".repeat(options.catalog_padding_bytes),
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["db", "expected_db_id"],
                "properties": {
                    "db": {"type": "string", "minLength": 1, "maxLength": 256},
                    "expected_db_id": {
                        "type": "string", "minLength": 26, "maxLength": 26,
                        "pattern": "^[0-7][0-9A-HJKMNP-TV-Z]{25}$",
                    },
                    "expected_catalog_sha256": {
                        "type": "string", "minLength": 64, "maxLength": 64,
                        "pattern": "^[0-9a-f]{64}$",
                    },
                },
            },
        }));
    }
    Value::Array(tools)
}

fn delay(duration: Duration, stop: &AtomicBool) -> bool {
    let end = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        thread::park_timeout(remaining);
    }
    false
}

fn read_request(stream: &mut TcpStream, stop: &AtomicBool) -> Option<Value> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    let mut framing = None;
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        let mut buf = [0; 4096];
        match stream.read(&mut buf) {
            Ok(0) => return None,
            Ok(len) => bytes.extend_from_slice(&buf[..len]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => return None,
        }
        assert!(bytes.len() <= 128 * 1024, "fixture request is too large");
        if framing.is_none() {
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let end = end + 4;
                let size = String::from_utf8_lossy(&bytes[..end])
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map_or(0, |(_, size)| size.trim().parse::<usize>().unwrap());
                assert!(size <= 128 * 1024, "fixture request is too large");
                framing = Some((end, size));
            }
        }
        if let Some((end, size)) = framing {
            if bytes.len() - end >= size {
                return Some(if size == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[end..end + size]).unwrap()
                });
            }
        }
    }
    None
}
