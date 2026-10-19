//! Publication metadata cannot become a database operation or follow an alias replacement.
use crate::*;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("mneme-discovery-owner-{}", ulid::Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> (Scratch, Server, PathBuf, String) {
    let root = Scratch::new();
    let mut sessions = SessionState::default();
    let mut registry = build_registry(
        vec![("fixture".into(), root.0.join("memory.db"))],
        &mut sessions,
    )
    .unwrap();
    let id = registry
        .slot("fixture")
        .unwrap()
        .status()
        .unwrap()
        .db_id
        .to_string();
    let path = root.0.join("catalog.toml");
    std::fs::write(&path, "schema = 'mneme.discovery-catalog.v1'\n[stores]\n").unwrap();
    registry.discovery_catalogs =
        crate::discovery_catalog::prepare_sources(vec![("fixture".into(), path.clone())])
            .unwrap()
            .bind(&registry)
            .unwrap();
    let server = Server {
        registry,
        sessions: std::sync::Arc::new(Mutex::new(sessions)),
        cold_work: ColdWorkGate::default(),
        capability: CapabilityPolicy::new(CapabilityProfile::ReadOnly, false),
        library: None,
    };
    (root, server, path, id)
}

async fn call(server: &Server, args: Value) -> Value {
    server
        .dispatch(
            "tools/call",
            json!({"name":"discovery_catalog","arguments":args}),
        )
        .await
        .unwrap()
}
fn payload(result: &Value) -> Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}
fn refusal(result: &Value) -> &str {
    assert_eq!(result["isError"], true, "{result}");
    result["content"][0]["text"].as_str().unwrap()
}

#[test]
fn publication_startup_is_owner_only_known_and_distinct() {
    for argv in [
        vec!["--discovery-catalog"],
        vec!["--discovery-catalog", "wrong"],
        vec!["--discovery-catalog", "=path"],
        vec!["--discovery-catalog", "user="],
        vec!["--discovery-catalog", "unknown=path"],
        vec![
            "--discovery-catalog",
            "user=a",
            "--discovery-catalog",
            "user=b",
        ],
        vec!["--db", "fixture=db", "--discovery-catalog", "user=a"],
        vec![
            "--library-config",
            "library.json",
            "--discovery-catalog",
            "user=a",
        ],
        vec![
            "--router-config",
            "router.json",
            "--discovery-catalog",
            "user=a",
        ],
        vec![
            "--memory-config",
            "memory.toml",
            "--discovery-catalog",
            "user=a",
        ],
    ] {
        assert!(parse_args_from(argv.iter().copied()).is_err(), "{argv:?}");
    }
    let oversized_alias = "a".repeat(257);
    assert!(
        parse_args_from(vec![
            "--db".into(),
            format!("{oversized_alias}=db"),
            "--discovery-catalog".into(),
            format!("{oversized_alias}=catalog")
        ])
        .is_err()
    );
    for argv in [
        vec!["--discovery-catalog", "user=a"],
        vec![
            "--db",
            "a=db",
            "--db",
            "b=other",
            "--discovery-catalog",
            "a=x",
            "--discovery-catalog",
            "b=y",
        ],
    ] {
        let StartupMode::Owner(options) = parse_args_from(argv).unwrap().mode else {
            panic!()
        };
        assert!(!options.discovery_catalogs().is_empty());
    }
}

#[test]
fn publication_invalid_startup_files_precede_owner_creation() {
    let root = Scratch::new();
    let db = root.0.join("never-created.db");
    let catalog = root.0.join("invalid.toml");
    for bytes in [
        None,
        Some(b"invalid TOML".to_vec()),
        Some(vec![b' '; mneme_config::MAX_CATALOG_BYTES + 1]),
    ] {
        if let Some(bytes) = bytes {
            std::fs::write(&catalog, bytes).unwrap();
        }
        let StartupMode::Owner(options) = parse_args_from([
            "--db".to_owned(),
            format!("owner={}", db.display()),
            "--discovery-catalog".to_owned(),
            format!("owner={}", catalog.display()),
        ])
        .unwrap()
        .mode
        else {
            panic!()
        };
        assert!(build_owner_registry(options, &mut SessionState::default()).is_err());
        assert!(!db.exists());
        assert!(!db.with_extension("bodies").exists());
    }
}

#[test]
fn publication_is_not_an_ordinary_router_forwarded_operation() {
    assert!(!crate::router_catalog::TOOLS.contains(&"discovery_catalog"));
}

#[test]
fn publication_schema_and_typed_admission_agree() {
    use super::support::schema_accepts;
    let schema = crate::discovery_catalog::tool_schema();
    let identity_description = schema["inputSchema"]["properties"]["expected_db_id"]["description"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(identity_description.contains("required"));
    assert!(identity_description.contains("metadata-only"));
    assert!(!identity_description.contains("optional"));
    let id = ulid::Ulid::new().to_string();
    for arguments in [
        json!({"db":"owner","expected_db_id":id}),
        json!({"db":"owner","expected_db_id":id,"expected_catalog_sha256":"a".repeat(64)}),
        json!({}),
        json!({"db":"owner"}),
        json!({"expected_db_id":id}),
        json!({"db":"","expected_db_id":id}),
        json!({"db":"x".repeat(257),"expected_db_id":id}),
        json!({"db":"owner","expected_db_id":"bad"}),
        json!({"db":"owner","expected_db_id":id,"expected_catalog_sha256":"A".repeat(64)}),
        json!({"db":"owner","expected_db_id":id,"extra":true}),
    ] {
        assert_eq!(
            schema_accepts(&schema["inputSchema"], &arguments),
            ValidatedToolArguments::parse("discovery_catalog", &arguments).is_ok(),
            "{arguments}"
        );
    }
}

#[tokio::test]
async fn publication_all_profiles_no_checkout_cold_work_activity_or_receipts_even_released() {
    let (_root, mut server, _path, id) = fixture();
    let admission = server.cold_work.admit("blocked-test-lane").unwrap();
    let before = server.registry.activity.page(0, 256);
    let before_activity = serde_json::to_value(before).unwrap();
    for released in [false, true] {
        if released {
            server
                .registry
                .slot("fixture")
                .unwrap()
                .begin_release()
                .unwrap()
                .finish()
                .unwrap();
        }
        for profile in [
            CapabilityProfile::ReadOnly,
            CapabilityProfile::ReceiptGrounded,
            CapabilityProfile::Curator,
            CapabilityProfile::Operator,
        ] {
            server.capability = CapabilityPolicy::new(profile, false);
            let response = call(&server, json!({"db":"fixture","expected_db_id":id})).await;
            assert_eq!(response["isError"], false, "{response}");
            let catalog = payload(&response);
            assert_eq!(catalog["publisher_db_id"], id);
            assert_eq!(catalog["schema"], "mneme.discovery-catalog.v1");
            assert_eq!(catalog["stores"], json!({}));
            assert_eq!(
                server
                    .registry
                    .slot("fixture")
                    .unwrap()
                    .status()
                    .unwrap()
                    .in_flight,
                0
            );
            let tools = server.dispatch("tools/list", json!({})).await.unwrap();
            assert!(
                tools["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == "discovery_catalog")
            );
        }
    }
    assert_eq!(
        serde_json::to_value(server.registry.activity.page(0, 256)).unwrap(),
        before_activity
    );
    assert!(server.sessions.lock().await.active.is_empty());
    assert_eq!(server.sessions.lock().await.receipt_count(), 0);
    drop(admission);
}

#[tokio::test]
async fn publication_absence_is_not_empty_and_same_identity_resume_preserves_metadata() {
    let (_root, mut server, _path, id) = fixture();
    let slot = server.registry.slot("fixture").unwrap().clone();
    slot.begin_release().unwrap().finish().unwrap();
    slot.begin_resume("same-owner".into())
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(slot.status().unwrap().db_id.to_string(), id);
    let result = call(&server, json!({"db":"fixture","expected_db_id":id})).await;
    assert_eq!(result["isError"], false, "{result}");
    server.registry.discovery_catalogs.clear();
    assert!(
        refusal(&call(&server, json!({"db":"fixture","expected_db_id":id})).await)
            .contains("not configured")
    );
}

#[tokio::test]
async fn publication_admission_and_identity_precede_file_read() {
    let (_root, server, path, id) = fixture();
    std::fs::remove_file(path).unwrap();
    for args in [
        json!({}),
        json!({"db":"fixture"}),
        json!({"expected_db_id":id}),
        json!({"db":"fixture","expected_db_id":null}),
        json!({"db":"fixture","expected_db_id":id,"extra":true}),
        json!({"db":"fixture","expected_db_id":id,"expected_catalog_sha256":"A".repeat(64)}),
        json!({"db":"fixture","expected_db_id":id,"expected_catalog_sha256":false}),
    ] {
        let response = call(&server, args.clone()).await;
        let error = refusal(&response);
        assert!(!error.contains("cannot read"), "{args}: {error}");
    }
    let response = call(
        &server,
        json!({"db":"fixture","expected_db_id":ulid::Ulid::new().to_string()}),
    )
    .await;
    assert!(refusal(&response).contains("expected_db_id mismatch"));
    let response = call(&server, json!({"db":"missing","expected_db_id":id})).await;
    assert!(refusal(&response).contains("unknown database"));
    assert_eq!(
        server
            .registry
            .slot("fixture")
            .unwrap()
            .status()
            .unwrap()
            .in_flight,
        0
    );
}

#[tokio::test]
async fn publication_revision_refuses_changes_and_malformed_or_oversized_source() {
    let (_root, server, path, id) = fixture();
    let original = payload(&call(&server, json!({"db":"fixture","expected_db_id":id})).await);
    let args = json!({"db":"fixture","expected_db_id":id,"expected_catalog_sha256":original["catalog_sha256"]});
    assert_eq!(call(&server, args.clone()).await["isError"], false);
    std::fs::write(
        &path,
        format!(
            "schema = 'mneme.discovery-catalog.v1'\n[stores.self]\nid = '{id}'\ndiscovery = true\n"
        ),
    )
    .unwrap();
    assert!(refusal(&call(&server, args).await).contains("revision changed"));
    let changed = payload(&call(&server, json!({"db":"fixture","expected_db_id":id})).await);
    assert_ne!(changed["catalog_sha256"], original["catalog_sha256"]);
    for bytes in [
        b"not toml".to_vec(),
        vec![b' '; mneme_config::MAX_CATALOG_BYTES + 1],
        b"schema = 'wrong'\n[stores]\n".to_vec(),
    ] {
        std::fs::write(&path, bytes).unwrap();
        refusal(&call(&server, json!({"db":"fixture","expected_db_id":id})).await);
    }
}

#[tokio::test]
async fn publication_cannot_rebind_to_replacement_after_release_resume() {
    let (root, server, path, id) = fixture();
    let slot = server.registry.slot("fixture").unwrap();
    slot.begin_release().unwrap().finish().unwrap();
    // The replacement is constructed only in this disposable fixture.
    let old = root.0.join("memory.db");
    let moved = root.0.join("original.db");
    std::fs::rename(&old, &moved).unwrap();
    // A fresh unguarded maintenance resume is an existing operator capability.
    slot.begin_resume("replacement-test".into())
        .unwrap()
        .finish()
        .unwrap();
    let replacement = slot.status().unwrap().db_id.to_string();
    assert_ne!(replacement, id);
    std::fs::remove_file(path).unwrap();
    let response = call(
        &server,
        json!({"db":"fixture","expected_db_id":replacement}),
    )
    .await;
    assert!(refusal(&response).contains("expected_db_id mismatch"));
}

#[cfg(unix)]
#[tokio::test]
async fn publication_admits_regular_symlink_targets_and_refuses_fifo_or_directory() {
    use std::os::unix::fs::symlink;
    let (root, server, path, id) = fixture();
    let target = root.0.join("target.toml");
    std::fs::rename(&path, &target).unwrap();
    symlink(&target, &path).unwrap();
    assert_eq!(
        call(&server, json!({"db":"fixture","expected_db_id":id})).await["isError"],
        false
    );
    std::fs::remove_file(&path).unwrap();
    symlink(root.0.join("missing"), &path).unwrap();
    refusal(&call(&server, json!({"db":"fixture","expected_db_id":id})).await);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    refusal(&call(&server, json!({"db":"fixture","expected_db_id":id})).await);
    std::fs::remove_dir(&path).unwrap();
    let path_c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: owned NUL-terminated path; creates a FIFO in a disposable test directory.
    assert_eq!(unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);
    refusal(&call(&server, json!({"db":"fixture","expected_db_id":id})).await);
}
