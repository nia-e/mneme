//! Disposable protocol fixtures; no configured owners or store files are used.
use super::*;
use mneme_config::{
    ActivationDeclaration, ApplicabilityDeclaration, DeviceStoreDeclaration,
    OperationScopeDeclaration, SetupDeclaration, SetupDiscoveryDeclaration,
};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PROJECT_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const HUB_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const OTHER_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";

fn root() -> PathBuf {
    std::env::temp_dir().join("mneme-registration-fixture-project")
}
fn owner() -> Owner {
    Owner {
        url: "http://127.0.0.1:43127/".into(),
        db: "project".into(),
        ssh_mcp_port: None,
        token_env: None,
        path: Some(root().join("memory.db")),
    }
}
fn source() -> StoreDeclaration {
    StoreDeclaration {
        id: PROJECT_ID.into(),
        owner: Some(OwnerDeclaration {
            url: Some(owner().url),
            db: Some("project".into()),
            path: owner().path,
            ..Default::default()
        }),
        ..Default::default()
    }
}
fn configured() -> ConfigDeclarations {
    ConfigDeclarations {
        device: Some("mac".into()),
        setup: Some(SetupDeclaration {
            discovery: Some(SetupDiscoveryDeclaration {
                mode: Some(SetupDiscoveryMode::Auto),
                hubs: Some(vec!["home".into()]),
                advertise_url: Some("ssh://nia@mac".into()),
            }),
        }),
        stores: BTreeMap::from([
            ("my-project".into(), source()),
            (
                "home".into(),
                StoreDeclaration {
                    id: HUB_ID.into(),
                    owner: Some(OwnerDeclaration {
                        url: Some("http://127.0.0.1:43128/".into()),
                        db: Some("hub".into()),
                        ..Default::default()
                    }),
                    active: Some(ActivationDeclaration {
                        enabled: Some(false),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ),
        ]),
        ..Default::default()
    }
}
fn preferences() -> Preferences {
    Preferences::resolve(&configured(), &RegistrationArgs::default())
}
fn snapshot(local: &ConfigDeclarations) -> mneme_config::SelectionSnapshot {
    mneme_config::select(
        &local.materialize().unwrap(),
        &mneme_config::SelectionContext::new(root()),
    )
    .unwrap()
}

#[test]
fn local_preferences_and_explicit_overrides_are_distinct() {
    let local = configured();
    let inherited = Preferences::resolve(&local, &RegistrationArgs::default());
    assert_eq!(inherited.mode, SetupDiscoveryMode::Auto);
    assert_eq!(inherited.hubs, ["home"]);
    assert_eq!(inherited.device.as_deref(), Some("mac"));
    let replaced = Preferences::resolve(
        &local,
        &RegistrationArgs {
            no_register: true,
            discovery_hub: vec!["other".into()],
            advertise_url: Some("https://memory.example/{port}/".into()),
            ..Default::default()
        },
    );
    assert_eq!(replaced.mode, SetupDiscoveryMode::Off);
    assert_eq!(replaced.hubs, ["other"]);
    assert_eq!(
        replaced.advertise_url.as_deref(),
        Some("https://memory.example/{port}/")
    );
    assert_eq!(
        Preferences::resolve(
            &ConfigDeclarations::default(),
            &RegistrationArgs {
                discovery_hub: vec!["home".into()],
                ..Default::default()
            }
        )
        .mode,
        SetupDiscoveryMode::Off
    );
    assert_eq!(
        Preferences::resolve(
            &local,
            &RegistrationArgs {
                interactive: true,
                ..Default::default()
            }
        )
        .mode,
        SetupDiscoveryMode::Ask
    );
}

#[test]
fn explicit_register_enables_default_off_without_expanding_targets() {
    let local = ConfigDeclarations::default();
    let requested = Preferences::resolve(
        &local,
        &RegistrationArgs {
            register: true,
            ..Default::default()
        },
    );
    assert_eq!(requested.mode, SetupDiscoveryMode::Auto);
    assert!(requested.hubs.is_empty());
    assert!(requested.advertise_url.is_none());
    assert!(requested.device.is_none());
}

#[test]
fn published_base_is_reachable_sparse_and_local_root_is_device_scoped() {
    let mut original = source();
    original.active = Some(ActivationDeclaration {
        enabled: Some(false),
        workspace: Some(WorkspaceKind::Any),
        roots: Some(vec![root()]),
        profiles: Some(vec!["private".into()]),
        ..Default::default()
    });
    original.permissions = Some(mneme_config::PermissionsDeclaration {
        write: Some(false),
        ..Default::default()
    });
    original.devices.insert(
        "unrelated-device".into(),
        DeviceStoreDeclaration {
            owner: Some(owner()),
            ..Default::default()
        },
    );
    let exported = publication(&original, &owner(), &root(), "mac", "ssh://nia@mac:2222").unwrap();
    let shared = exported.owner.as_ref().unwrap();
    assert_eq!(shared.url.as_deref(), Some("ssh://nia@mac:2222"));
    assert_eq!(shared.ssh_mcp_port, Some(43127));
    assert!(shared.path.is_none() && shared.token_env.is_none());
    let active = exported.active.as_ref().unwrap();
    assert_eq!(active.workspace, Some(WorkspaceKind::Project));
    assert_eq!(active.enabled, Some(false));
    assert_eq!(
        active.profiles.as_deref(),
        Some(["private".into()].as_slice())
    );
    assert!(
        active.roots.is_none() && active.subtree_roots.is_none() && active.excluded_roots.is_none()
    );
    assert!(exported.permissions.as_ref().unwrap().read.is_none());
    assert_eq!(exported.permissions.as_ref().unwrap().write, Some(false));
    assert_eq!(exported.devices.len(), 1);
    let device = &exported.devices["mac"];
    assert_eq!(
        device.active.as_ref().unwrap().roots.as_deref(),
        Some([root()].as_slice())
    );
    assert_eq!(device.owner.as_ref().unwrap().url, owner().url);
    assert!(device.owner.as_ref().unwrap().path.is_none());
    assert_eq!(
        original.devices.len(),
        1,
        "export must not mutate source declarations"
    );
    exported.validate(true).unwrap();
}

#[test]
fn advertised_http_port_uses_bound_owner_port_not_ssh_connection_port() {
    let exported = publication(
        &source(),
        &owner(),
        &root(),
        "mac",
        "https://memory.example:{port}/mcp",
    )
    .unwrap();
    let shared = exported.owner.unwrap();
    assert_eq!(
        shared.url.as_deref(),
        Some("https://memory.example:43127/mcp")
    );
    assert_eq!(shared.ssh_mcp_port, None);
    for invalid in [
        "http://127.0.0.1:{port}/",
        "http://localhost:{port}/",
        "ssh://nia@localhost",
        "http://0.0.0.0:{port}/",
    ] {
        assert!(
            publication(&source(), &owner(), &root(), "mac", invalid).is_err(),
            "{invalid}"
        );
    }
    let mut altered = owner();
    altered.token_env = Some("SHOULD_NOT_EXPORT".into());
    assert!(publication(&source(), &altered, &root(), "mac", "ssh://nia@mac").is_err());
}

#[test]
fn publication_merges_sparse_device_applicability_without_losing_base_scope() {
    let context_root = root().join("context");
    let record_root = root().join("record");
    let excluded = root().join("private");
    let mut original = source();
    original.applicability = Some(ApplicabilityDeclaration {
        context: Some(OperationScopeDeclaration {
            roots: Some(vec![context_root.clone()]),
            ..Default::default()
        }),
        record: Some(OperationScopeDeclaration {
            excluded_roots: Some(vec![excluded.clone()]),
            ..Default::default()
        }),
        ..Default::default()
    });
    original.devices.insert(
        "mac".into(),
        DeviceStoreDeclaration {
            applicability: Some(ApplicabilityDeclaration {
                record: Some(OperationScopeDeclaration {
                    roots: Some(vec![record_root.clone()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let exported = publication(&original, &owner(), &root(), "mac", "ssh://nia@mac").unwrap();
    assert!(exported.applicability.is_none());
    let scopes = exported.devices["mac"].applicability.as_ref().unwrap();
    assert_eq!(
        scopes.context.as_ref().unwrap().roots.as_deref(),
        Some([context_root].as_slice())
    );
    let record = scopes.record.as_ref().unwrap();
    assert_eq!(record.roots.as_deref(), Some([record_root].as_slice()));
    assert_eq!(
        record.excluded_roots.as_deref(),
        Some([excluded].as_slice())
    );
}

#[test]
fn plan_uses_canonical_identity_and_local_inactive_hub_routes() {
    let local = configured();
    let plan = make_plan(
        &local,
        &snapshot(&local),
        PROJECT_ID,
        &owner().url,
        &root(),
        &preferences(),
    )
    .unwrap();
    assert_eq!(plan.label, "my-project");
    assert_eq!(plan.store.id, PROJECT_ID);
    assert_eq!(plan.hubs[0].label, "home");
    assert_eq!(plan.hubs[0].id, HUB_ID);
    let mut duplicate = snapshot(&local);
    duplicate.stores.push(
        duplicate
            .stores
            .iter()
            .find(|s| s.store.id == PROJECT_ID)
            .unwrap()
            .clone(),
    );
    assert!(
        make_plan(
            &local,
            &duplicate,
            PROJECT_ID,
            &owner().url,
            &root(),
            &preferences()
        )
        .is_err()
    );
    let mut missing_device = preferences();
    missing_device.device = None;
    assert!(
        make_plan(
            &local,
            &snapshot(&local),
            PROJECT_ID,
            &owner().url,
            &root(),
            &missing_device
        )
        .unwrap_err()
        .to_string()
        .contains("device")
    );
}

#[test]
fn json_and_nonterminal_modes_never_admit_prompting() {
    for stdin in [false, true] {
        for stderr in [false, true] {
            assert!(!can_prompt(true, stdin, stderr));
        }
    }
    assert!(!can_prompt(false, false, true));
    assert!(!can_prompt(false, true, false));
    assert!(can_prompt(false, true, true));
}

#[test]
fn interactive_answers_are_bounded_and_blank_or_eof_skip() {
    let mut out = Vec::new();
    assert_eq!(
        prompt(&mut Cursor::new(b""), &mut out, "question").unwrap(),
        None
    );
    assert_eq!(
        prompt(&mut Cursor::new(b" \n"), &mut out, "question").unwrap(),
        None
    );
    assert_eq!(
        prompt(&mut Cursor::new(b" yes \n"), &mut out, "question")
            .unwrap()
            .as_deref(),
        Some("yes")
    );
    assert!(
        prompt(
            &mut Cursor::new(vec![b'x'; MAX_PROMPT_BYTES + 1]),
            &mut out,
            "question"
        )
        .is_err()
    );
    let mut missing = Preferences {
        mode: SetupDiscoveryMode::Ask,
        hubs: vec![],
        advertise_url: None,
        device: None,
    };
    assert!(
        complete_preferences(
            &mut missing,
            &mut Cursor::new(b"home,other\nssh://nia@mac\nmac\n"),
            &mut out
        )
        .unwrap()
    );
    assert_eq!(missing.hubs, ["home", "other"]);
    assert_eq!(missing.device.as_deref(), Some("mac"));
    assert!(
        !complete_preferences(
            &mut Preferences {
                mode: SetupDiscoveryMode::Ask,
                hubs: vec![],
                advertise_url: None,
                device: None
            },
            &mut Cursor::new(b"\n"),
            &mut out
        )
        .unwrap()
    );
}

fn registration_catalog() -> Value {
    let id = json!({"type":"string","minLength":26,"maxLength":26,"pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"});
    json!([{"name":"discovery_register","inputSchema":{
        "type":"object","additionalProperties":false,"required":["db","expected_db_id","label","store"],
        "properties":{
            "db":{"type":"string","minLength":1,"maxLength":256},
            "expected_db_id":id,
            "label":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_.-]+$"},
            "store":{"type":"object","additionalProperties":false,"required":["id"],"properties":{"id":id}},
            "expected_catalog_sha256":{"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$"}
        }
    }}])
}
struct Fixture {
    url: String,
    calls: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn start(receipt_override: Option<Value>, delay: Duration) -> Self {
        Self::start_inner(receipt_override, delay, false).await
    }
    async fn start_inner(
        receipt_override: Option<Value>,
        delay: Duration,
        drop_call_reply: bool,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://127.0.0.1:{}/",
            listener.local_addr().unwrap().port()
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let task = tokio::spawn(async move {
            let mut original_label = None;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut next = [0; 4096];
                    let n = stream.read(&mut next).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&next[..n]);
                    assert!(bytes.len() <= 128 * 1024);
                    if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let header = std::str::from_utf8(&bytes[..header_end]).unwrap();
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while bytes.len() - header_end < length {
                    let mut next = [0; 4096];
                    let n = stream.read(&mut next).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&next[..n]);
                }
                let msg: Value = if length == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
                };
                let method = msg["method"].as_str().unwrap_or("close");
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let response = match method {
                    "initialize" => Some(
                        json!({"jsonrpc":"2.0","id":msg["id"],"result":{"protocolVersion":"2025-11-25","serverInfo":{"name":"mneme-mcp"},"capabilities":{"tools":{}}}}),
                    ),
                    "tools/list" => Some(
                        json!({"jsonrpc":"2.0","id":msg["id"],"result":{"tools":registration_catalog()}}),
                    ),
                    "tools/call" => {
                        assert_eq!(msg["params"]["name"], "discovery_register");
                        let args = msg["params"]["arguments"].clone();
                        assert_eq!(args["expected_db_id"], HUB_ID);
                        assert!(args.get("expected_catalog_sha256").is_none());
                        let retry = original_label.is_some();
                        let label = original_label
                            .get_or_insert_with(|| args["label"].as_str().unwrap().to_owned())
                            .clone();
                        observed.lock().unwrap().push(args);
                        if drop_call_reply {
                            // The request reached the hub, but no response can
                            // establish whether its mutation became durable.
                            continue;
                        }
                        let receipt=receipt_override.clone().unwrap_or_else(||json!({"status":if retry {"already_present"} else {"added"},"publisher_db_id":HUB_ID,"label":label,"before_catalog_sha256":"a".repeat(64),"catalog_sha256":"b".repeat(64),"durable":true,"warning":null}));
                        Some(
                            json!({"jsonrpc":"2.0","id":msg["id"],"result":{"content":[{"type":"text","text":serde_json::to_string(&receipt).unwrap()}]}}),
                        )
                    }
                    "notifications/initialized" | "close" => None,
                    _ => panic!("unexpected fixture method {method}"),
                };
                let body = response.as_ref().map(Value::to_string).unwrap_or_default();
                let status = if response.is_some() {
                    "200 OK"
                } else {
                    "202 Accepted"
                };
                let wire = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nmcp-session-id: registration-test\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                if stream.write_all(wire.as_bytes()).await.is_err() {
                    continue;
                }
            }
        });
        Self { url, calls, task }
    }
    fn hub(&self) -> SetupDiscoveryHub {
        SetupDiscoveryHub {
            label: "home".into(),
            id: HUB_ID.into(),
            owner: Owner {
                url: self.url.clone(),
                db: "hub".into(),
                ssh_mcp_port: None,
                token_env: None,
                path: None,
            },
        }
    }
}
fn plan(hubs: Vec<SetupDiscoveryHub>) -> RegistrationPlan {
    RegistrationPlan {
        label: "my-project".into(),
        store: publication(&source(), &owner(), &root(), "mac", "ssh://nia@mac").unwrap(),
        hubs,
    }
}

#[tokio::test]
async fn one_unavailable_hub_does_not_erase_successful_registration() {
    let fixture = Fixture::start(None, Duration::ZERO).await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unavailable_url = format!(
        "http://127.0.0.1:{}/",
        listener.local_addr().unwrap().port()
    );
    drop(listener);
    let mut unavailable = fixture.hub();
    unavailable.label = "unavailable".into();
    unavailable.owner.url = unavailable_url;
    let report = execute(plan(vec![unavailable, fixture.hub()])).await;
    assert_eq!(report.status, "partial");
    assert!(report.published);
    assert_eq!(
        report
            .hubs
            .iter()
            .map(|h| h.hub.as_str())
            .collect::<Vec<_>>(),
        ["unavailable", "home"]
    );
    assert_eq!(report.hubs[0].status, "failed");
    assert_eq!(report.hubs[1].status, "added");
    assert!(report.recovery.is_some());
    assert!(report.bytes_received > 0);
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn retry_accepts_existing_identity_under_original_catalog_label() {
    let fixture = Fixture::start(None, Duration::ZERO).await;
    assert_eq!(execute(plan(vec![fixture.hub()])).await.status, "complete");
    let mut renamed = plan(vec![fixture.hub()]);
    renamed.label = "renamed-local-alias".into();
    let report = execute(renamed).await;
    assert_eq!(report.status, "complete");
    assert!(report.published);
    assert_eq!(report.hubs[0].status, "already_present");
    assert_eq!(report.hubs[0].receipt.as_ref().unwrap().label, "my-project");
    assert_eq!(fixture.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn malformed_identity_or_undurable_receipt_remains_pending() {
    for (publisher, durable) in [(OTHER_ID, true), (HUB_ID, false)] {
        let receipt = json!({"status":"added","publisher_db_id":publisher,"label":"my-project","before_catalog_sha256":"a".repeat(64),"catalog_sha256":"b".repeat(64),"durable":durable,"warning":null});
        let fixture = Fixture::start(Some(receipt), Duration::ZERO).await;
        let report = execute(plan(vec![fixture.hub()])).await;
        assert_eq!(report.status, "pending");
        assert_eq!(report.published, publisher == HUB_ID);
        assert_eq!(report.hubs[0].status, "pending");
        assert!(report.recovery.is_some());
    }
}

#[tokio::test]
async fn receive_budget_is_shared_across_attempts_and_blocks_later_contact() {
    let fixture = Fixture::start(None, Duration::ZERO).await;
    let budget = ReceiveBudget::new(32);
    let first = call_hub(
        fixture.hub(),
        "my-project".into(),
        source(),
        budget.clone(),
        Instant::now() + Duration::from_secs(2),
    )
    .await;
    assert_eq!(first.status, "failed");
    assert!(budget.used() > 32);
    let second = call_hub(
        fixture.hub(),
        "my-project".into(),
        source(),
        budget.clone(),
        Instant::now() + Duration::from_secs(2),
    )
    .await;
    assert_eq!(second.status, "pending");
    assert!(second.detail.unwrap().contains("not attempted"));
    assert!(fixture.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stalled_connection_obeys_aggregate_deadline_without_waiting_for_server() {
    let fixture = Fixture::start(None, Duration::from_secs(5)).await;
    let started = Instant::now();
    let outcome = call_hub(
        fixture.hub(),
        "my-project".into(),
        source(),
        ReceiveBudget::new(4096),
        started + Duration::from_millis(500),
    )
    .await;
    assert!(matches!(outcome.status, "pending" | "failed"));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(fixture.calls.lock().unwrap().is_empty());
}

struct WorkspaceFixture(PathBuf);
impl Drop for WorkspaceFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl WorkspaceFixture {
    fn new(hub_url: &str, mode: &str) -> (Self, SetupResult) {
        let directory =
            std::env::temp_dir().join(format!("mneme-registration-{}", ulid::Ulid::new()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("memory.toml");
        let config = format!(
            r#"schema = "mneme.config.v1"
device = "mac"
[setup.discovery]
mode = {mode:?}
hubs = ["home"]
advertise_url = "ssh://nia@mac"
[stores.my-project]
id = {PROJECT_ID:?}
owner = {{ url = "http://127.0.0.1:43127/", db = "project" }}
active = {{ workspace = "project" }}
[stores.home]
id = {HUB_ID:?}
owner = {{ url = {hub_url:?}, db = "hub" }}
active = {{ enabled = false }}
"#
        );
        std::fs::write(&path, config).unwrap();
        let prepared = mneme_config::prepare_workspace(Some(&path), &directory, None)
            .unwrap()
            .unwrap();
        let snapshot = mneme_config::finish_workspace(prepared, None).unwrap();
        let result = SetupResult {
            schema: "mneme.project-init.v1".into(), status: "configured".into(), stage: "complete".into(), published: false,
            extra: json!({"integration":"native","store":"adopted","db_id":PROJECT_ID,"project_root":directory,"memory_config":path,"owner_url":"http://127.0.0.1:43127/","selection_digest":snapshot.digest}).as_object().unwrap().clone(),
        };
        (Self(directory), result)
    }
}

#[tokio::test]
async fn adopted_native_result_registers_idempotently_with_separate_publication_report() {
    let hub = Fixture::start(None, Duration::ZERO).await;
    let (_workspace, mut result) = WorkspaceFixture::new(&hub.url, "auto");
    after_init(&RegistrationArgs::default(), &mut result, true).await;
    assert_eq!(result.extra["store"], "adopted");
    assert!(
        !result.published,
        "top-level setup publication contract is unchanged"
    );
    assert!(result.extra.get("registration").is_none());
    assert_eq!(result.extra["discovery_registration"]["published"], true);
    assert_eq!(result.extra["discovery_registration"]["status"], "complete");
    assert_eq!(
        result.extra["discovery_registration"]["hubs"][0]["status"],
        "added"
    );
    after_init(&RegistrationArgs::default(), &mut result, true).await;
    assert_eq!(
        result.extra["discovery_registration"]["hubs"][0]["status"],
        "already_present"
    );
    assert_eq!(hub.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn configured_ask_with_json_defers_without_contacting_any_hub() {
    let hub = Fixture::start(None, Duration::ZERO).await;
    let (_workspace, mut result) = WorkspaceFixture::new(&hub.url, "ask");
    after_init(&RegistrationArgs::default(), &mut result, true).await;
    assert_eq!(result.extra["discovery_registration"]["status"], "deferred");
    assert_eq!(result.extra["discovery_registration"]["published"], false);
    assert!(!result.published);
    assert!(hub.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn lost_reply_after_submitted_write_is_pending_and_never_automatically_retried() {
    let fixture = Fixture::start_inner(None, Duration::ZERO, true).await;
    let report = execute(plan(vec![fixture.hub()])).await;
    assert_eq!(report.status, "pending");
    assert!(
        !report.published,
        "no receipt means publication is unconfirmed"
    );
    let outcome = &report.hubs[0];
    assert_eq!(outcome.status, "pending");
    assert!(outcome.receipt.is_none());
    let detail = outcome.detail.as_deref().unwrap();
    assert!(detail.contains("write may have completed"), "{detail}");
    assert_eq!(
        fixture.calls.lock().unwrap().len(),
        1,
        "uncertain mutations must not be retried automatically"
    );
}
