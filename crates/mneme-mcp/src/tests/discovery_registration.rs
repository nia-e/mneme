//! Writable hub registration is explicit operator authority, not database checkout.
use crate::*;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mneme-discovery-registration-{}",
            ulid::Ulid::new()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(writable: bool) -> (Scratch, Server, PathBuf, String) {
    let root = Scratch::new();
    let path = root.0.join("catalog.toml");
    std::fs::write(&path, "schema = 'mneme.discovery-catalog.v1'\n[stores]\n").unwrap();
    let mut args = vec![
        "--db".to_owned(),
        format!("hub={}", root.0.join("memory.db").display()),
        "--discovery-catalog".to_owned(),
        format!("hub={}", path.display()),
        "--capability-profile".to_owned(),
        "operator".to_owned(),
    ];
    if writable {
        args.extend(["--discovery-catalog-writable".to_owned(), "hub".to_owned()]);
    }
    let StartupMode::Owner(options) = parse_args_from(args).unwrap().mode else {
        panic!("owner expected")
    };
    let mut sessions = SessionState::default();
    let registry = build_owner_registry(options, &mut sessions).unwrap();
    let id = registry
        .slot("hub")
        .unwrap()
        .status()
        .unwrap()
        .db_id
        .to_string();
    let server = Server {
        registry,
        sessions: std::sync::Arc::new(Mutex::new(sessions)),
        cold_work: ColdWorkGate::default(),
        capability: CapabilityPolicy::new(CapabilityProfile::Operator, false),
        library: None,
    };
    (root, server, path, id)
}

fn arguments(publisher: &str) -> Value {
    json!({
        "db":"hub", "expected_db_id":publisher, "label":"new-project",
        "store":{"id":ulid::Ulid::new().to_string(), "discovery":true}
    })
}
async fn call(server: &Server, arguments: Value) -> Value {
    server
        .dispatch(
            "tools/call",
            json!({"name":"discovery_register", "arguments":arguments}),
        )
        .await
        .unwrap()
}
fn payload(result: &Value) -> Value {
    assert_eq!(result["isError"], false, "{result}");
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}
fn refusal(result: &Value) -> &str {
    assert_eq!(result["isError"], true, "{result}");
    result["content"][0]["text"].as_str().unwrap()
}

#[test]
fn registration_startup_requires_unique_known_catalog_and_operator_owner() {
    for args in [
        vec!["--discovery-catalog-writable"],
        vec!["--discovery-catalog-writable", ""],
        vec!["--discovery-catalog-writable", "user"],
        vec![
            "--discovery-catalog",
            "user=hub.toml",
            "--discovery-catalog-writable",
            "missing",
            "--capability-profile",
            "operator",
        ],
        vec![
            "--discovery-catalog",
            "user=hub.toml",
            "--discovery-catalog-writable",
            "user",
            "--capability-profile",
            "read-only",
        ],
        vec![
            "--discovery-catalog",
            "user=hub.toml",
            "--discovery-catalog-writable",
            "user",
            "--capability-profile",
            "receipt-grounded",
        ],
        vec![
            "--discovery-catalog",
            "user=hub.toml",
            "--discovery-catalog-writable",
            "user",
            "--capability-profile",
            "curator",
        ],
        vec![
            "--discovery-catalog",
            "user=hub.toml",
            "--discovery-catalog-writable",
            "user",
            "--discovery-catalog-writable",
            "user",
            "--capability-profile",
            "operator",
        ],
    ] {
        assert!(parse_args_from(args.iter().copied()).is_err(), "{args:?}");
    }
    for coordinator in [
        "--activity-config",
        "--memory-config",
        "--library-config",
        "--router-config",
    ] {
        let args = [
            coordinator,
            "config",
            "--discovery-catalog-writable",
            "user",
        ];
        assert!(parse_args_from(args).is_err(), "{args:?}");
    }
    assert!(
        parse_args_from([
            "--discovery-catalog",
            "user=hub.toml",
            "--discovery-catalog-writable",
            "user",
            "--capability-profile",
            "operator",
        ])
        .is_ok()
    );
    assert!(!crate::router_catalog::TOOLS.contains(&"discovery_register"));
}

#[test]
fn registration_schema_and_closed_typed_admission_agree() {
    let schema = crate::discovery_registration::tool_schema();
    let valid = arguments(&ulid::Ulid::new().to_string());
    let mut nullable_store_fields = valid.clone();
    nullable_store_fields["store"]["discovery"] = Value::Null;
    let mut owner = valid.clone();
    owner["store"]["owner"] = json!({"url":"http://127.0.0.1:9001/mcp", "db":"project"});
    let mut nullable_owner = owner.clone();
    nullable_owner["store"]["owner"]["path"] = Value::Null;
    nullable_owner["store"]["owner"]["ssh_mcp_port"] = Value::Null;
    nullable_owner["store"]["owner"]["token_env"] = Value::Null;
    let mut examples = vec![valid.clone(), nullable_store_fields, owner, nullable_owner];
    let mut revision = valid.clone();
    revision["expected_catalog_sha256"] = json!("a".repeat(64));
    examples.push(revision);
    for field in ["db", "expected_db_id", "label", "store"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        examples.push(missing);
    }
    for (key, value) in [
        ("db", json!("")),
        ("db", json!("a".repeat(257))),
        ("expected_db_id", json!("bad")),
        ("label", json!("")),
        ("store", json!([])),
        ("store", json!({})),
        ("expected_catalog_sha256", json!("A".repeat(64))),
        ("expected_catalog_sha256", json!(false)),
        ("expected_catalog_sha256", Value::Null),
        ("path", json!("/not-an-input")),
    ] {
        let mut bad = valid.clone();
        bad[key] = value;
        examples.push(bad);
    }
    let mut nested_unknown = valid.clone();
    nested_unknown["store"]["surprise"] = json!(true);
    examples.push(nested_unknown);
    for owner in [
        json!({}),
        json!({"url":"http://127.0.0.1:9001/mcp"}),
        json!({"db":"project"}),
        json!({"url":null, "db":"project"}),
        json!({"url":"http://127.0.0.1:9001/mcp", "db":"project", "token_env":"NOT_A_SECRET"}),
    ] {
        let mut incomplete_owner = valid.clone();
        incomplete_owner["store"]["owner"] = owner;
        examples.push(incomplete_owner);
    }
    let mut null_required_tags = valid.clone();
    null_required_tags["store"]["capture"] = json!({"required_tags":null});
    examples.push(null_required_tags);
    for value in examples {
        assert_eq!(
            super::schema_accepts(&schema["inputSchema"], &value),
            ValidatedToolArguments::parse("discovery_register", &value).is_ok(),
            "schema/admission mismatch: {value}"
        );
    }
}

#[tokio::test]
async fn registration_read_publication_does_not_grant_writes() {
    let (_root, server, path, id) = fixture(false);
    let before = std::fs::read(&path).unwrap();
    let result = call(&server, arguments(&id)).await;
    refusal(&result);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let read = server
        .dispatch(
            "tools/call",
            json!({
                "name":"discovery_catalog", "arguments":{"db":"hub", "expected_db_id":id}
            }),
        )
        .await
        .unwrap();
    assert_eq!(payload(&read)["stores"], json!({}));
}

#[tokio::test]
async fn registration_profile_catalog_and_dispatch_deny_before_file_io() {
    let (_root, mut server, path, id) = fixture(true);
    std::fs::remove_file(&path).unwrap();
    let _cold = server.cold_work.admit("blocked-test-lane").unwrap();
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        server.capability = CapabilityPolicy::new(profile, false);
        let catalog = server.dispatch("tools/list", json!({})).await.unwrap();
        assert_eq!(
            catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "discovery_register"),
            profile == CapabilityProfile::Operator
        );
        let result = call(&server, arguments(&id)).await;
        let error = refusal(&result);
        if profile != CapabilityProfile::Operator {
            assert!(
                error.contains("does not authorize discovery_register"),
                "{error}"
            );
        } else {
            assert!(!error.contains("does not authorize"), "{error}");
        }
        assert_eq!(
            server
                .registry
                .slot("hub")
                .unwrap()
                .status()
                .unwrap()
                .in_flight,
            0
        );
        assert!(!path.exists());
    }
}

#[tokio::test]
async fn registration_released_owner_uses_retained_identity_without_memory_work() {
    let (_root, server, path, id) = fixture(true);
    let before_activity = serde_json::to_value(server.registry.activity.page(0, 256)).unwrap();
    let slot = server.registry.slot("hub").unwrap();
    slot.begin_release().unwrap().finish().unwrap();
    let _cold = server.cold_work.admit("unrelated-memory-work").unwrap();
    let args = arguments(&id);
    let result = payload(&call(&server, args.clone()).await);
    assert_eq!(result["status"], "added");
    assert_eq!(result["publisher_db_id"], id);
    assert_eq!(result["label"], "new-project");
    assert_eq!(result["durable"], true);
    assert_ne!(result["before_catalog_sha256"], result["catalog_sha256"]);
    let catalog = mneme_config::read_discovery_catalog(&path, &id).unwrap();
    assert_eq!(catalog.catalog_sha256, result["catalog_sha256"]);
    let after_bytes = std::fs::read(&path).unwrap();
    let mut retry = args;
    retry["expected_catalog_sha256"] = result["before_catalog_sha256"].clone();
    let retried = payload(&call(&server, retry).await);
    assert_eq!(retried["status"], "already_present");
    assert_eq!(std::fs::read(&path).unwrap(), after_bytes);
    assert_eq!(slot.status().unwrap().in_flight, 0);
    assert!(
        slot.checkout().is_err(),
        "registration must not resume a released owner"
    );
    assert_eq!(
        serde_json::to_value(server.registry.activity.page(0, 256)).unwrap(),
        before_activity
    );
    assert!(server.sessions.lock().await.active.is_empty());
    assert_eq!(server.sessions.lock().await.receipt_count(), 0);
}

#[tokio::test]
async fn registration_invalid_requests_and_identity_do_not_touch_catalog() {
    let (root, server, path, id) = fixture(true);
    std::fs::remove_file(&path).unwrap();
    let before_paths = std::fs::read_dir(&root.0).unwrap().count();
    let mut unknown = arguments(&id);
    unknown["store"]["unrecognized"] = json!("must fail closed");
    let mut wrong_type = arguments(&id);
    wrong_type["store"]["discovery"] = json!("yes");
    let mut wrong_revision = arguments(&id);
    wrong_revision["expected_catalog_sha256"] = json!("A".repeat(64));
    let mut oversized = arguments(&id);
    oversized["store"]["owner"] = json!({
        "url":"http://127.0.0.1:9001/mcp", "db":"project",
        "path":format!("/{}", "x".repeat(mneme_app::discovery_registration::MAX_REGISTRATION_BYTES + 1024))
    });
    for args in [unknown, wrong_type, wrong_revision, oversized] {
        refusal(&call(&server, args).await);
    }
    let error_result = call(&server, arguments(&ulid::Ulid::new().to_string())).await;
    assert!(refusal(&error_result).contains("expected_db_id mismatch"));
    assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), before_paths);
    assert!(!path.exists());
    assert_eq!(
        server
            .registry
            .slot("hub")
            .unwrap()
            .status()
            .unwrap()
            .in_flight,
        0
    );
}

#[tokio::test]
async fn registration_original_publisher_does_not_follow_alias_replacement() {
    let (root, server, path, id) = fixture(true);
    let slot = server.registry.slot("hub").unwrap();
    slot.begin_release().unwrap().finish().unwrap();
    std::fs::rename(root.0.join("memory.db"), root.0.join("original.db")).unwrap();
    slot.begin_resume("registration-replacement-test".into())
        .unwrap()
        .finish()
        .unwrap();
    let replacement_id = slot.status().unwrap().db_id.to_string();
    assert_ne!(replacement_id, id);
    let before = std::fs::read(&path).unwrap();
    let result = call(&server, arguments(&replacement_id)).await;
    assert!(refusal(&result).contains("expected_db_id mismatch"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn registration_identity_guard_serializes_resume_and_refuses_transitional_owner() {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;
    let (_root, server, _path, _id) = fixture(true);
    let slot = server.registry.slot("hub").unwrap().clone();
    let id = slot.status().unwrap().db_id;
    slot.begin_release().unwrap().finish().unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let registration_slot = Arc::clone(&slot);
    let registration = std::thread::spawn(move || {
        registration_slot.with_retained_identity(id, || {
            entered_tx.send(()).unwrap();
            finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (attempting_tx, attempting_rx) = mpsc::channel();
    let (resumed_tx, resumed_rx) = mpsc::channel();
    let resume_slot = Arc::clone(&slot);
    let resume = std::thread::spawn(move || {
        attempting_tx.send(()).unwrap();
        let result = resume_slot
            .begin_resume("registration-concurrent-resume".into())
            .and_then(|resume| resume.finish());
        resumed_tx.send(result.is_ok()).unwrap();
        result
    });
    attempting_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        resumed_rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    finish_tx.send(()).unwrap();
    registration.join().unwrap().unwrap();
    assert!(resumed_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    resume.join().unwrap().unwrap();
    assert_eq!(slot.status().unwrap().db_id, id);
    assert_eq!(slot.status().unwrap().in_flight, 0);

    slot.begin_release().unwrap().finish().unwrap();
    let resume = slot
        .begin_resume("registration-transition-refusal".into())
        .unwrap();
    let entered = std::sync::atomic::AtomicBool::new(false);
    assert!(
        slot.with_retained_identity(id, || {
            entered.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .is_err()
    );
    assert!(!entered.load(std::sync::atomic::Ordering::SeqCst));
    resume.finish().unwrap();
}

#[tokio::test]
async fn registration_conflicts_and_stale_new_entries_never_overwrite() {
    let (_root, mut server, path, id) = fixture(true);
    let args = arguments(&id);
    let added = payload(&call(&server, args.clone()).await);
    let before = std::fs::read(&path).unwrap();
    let same_label = arguments(&id);
    let mut changed_declaration = args.clone();
    changed_declaration["store"]["discovery"] = json!(false);
    let mut stale_new_entry = arguments(&id);
    stale_new_entry["label"] = json!("another-project");
    stale_new_entry["expected_catalog_sha256"] = added["before_catalog_sha256"].clone();
    for rejected in [same_label, changed_declaration, stale_new_entry] {
        refusal(&call(&server, rejected).await);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    let mut retry = args;
    retry["label"] = json!("different-requested-label");
    retry["expected_catalog_sha256"] = added["before_catalog_sha256"].clone();
    let existing = payload(&call(&server, retry).await);
    assert_eq!(existing["status"], "already_present");
    assert_eq!(existing["label"], "new-project");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    server.registry.discovery_catalogs.clear();
    refusal(&call(&server, arguments(&id)).await);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn registration_generated_server_schema_satisfies_native_client_contract() {
    let catalog = [crate::discovery_registration::tool_schema()];
    let view = mneme_mcp_client::CatalogView(&catalog);
    assert!(view.supports_discovery_register());
    assert!(view.supports_expected_db_id("discovery_register", None));
    assert!(!view.supports_expected_db_id("discovery_register", Some("register")));
}
