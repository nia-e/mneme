//! Source selection, exact provenance, and admission fencing across library reads.
use crate::{
    Catalog, Endpoint, LibraryGet, NativeOwnerRoute, SourceFailure, err,
    tests::{fixture, query},
};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

#[tokio::test]
async fn same_device_owner_and_snapshot_keep_exact_provenance() {
    let (mut rt, fake, path) = fixture();
    let mut catalog: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    catalog.entries[0].replicas[0].source_device_id = "owner".into();
    std::fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let endpoint = rt.config.replicas.remove("mirror").unwrap();
    rt.config.replicas.insert("owner".into(), endpoint);
    let live = json!({"device_id":"owner","kind":"live","generation":null,"captured_at":null});
    let snapshot = json!({"device_id":"owner","kind":"snapshot","generation":"g1","captured_at":1_790_380_800});
    let reference = LibraryGet {
        project_id: "p".into(),
        id: "node-1".into(),
        source_device_id: "owner".into(),
        generation: None,
    };
    assert_eq!(rt.get(reference.clone()).await.unwrap()["source"], live);
    let queried = rt.query(query()).await.unwrap();
    assert_eq!(queried["coverage"][0]["source"], live);
    assert_eq!(queried["primary"][0]["source"], live);

    fake.errors.lock().unwrap().insert(
        "owner".into(),
        SourceFailure::Availability("offline".into()),
    );
    for response in [
        rt.query(query()).await.unwrap(),
        rt.recall_context(query()).await.unwrap(),
    ] {
        assert_eq!(response["coverage"][0]["state"], "snapshot");
        assert_eq!(response["coverage"][0]["source"], snapshot);
        assert_eq!(response["primary"][0]["source"], snapshot);
        if let Some(episodes) = response["episodes"].as_array() {
            assert!(!episodes.is_empty());
            assert!(episodes.iter().all(|episode| episode["source"] == snapshot));
        }
    }
    // A newer edition on the same device must not move an exact get pin.
    let mut newer = catalog.entries[0].replicas[0].clone();
    newer.generation = "g2".into();
    newer.database = "copy_g2".into();
    newer.resolved_path = "/immutable/g2/database.db".into();
    newer.captured_at += 1;
    catalog.entries[0].replicas.push(newer);
    std::fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let pinned = LibraryGet {
        generation: Some("g1".into()),
        ..reference
    };
    let fetched = rt.get(pinned).await.unwrap();
    assert_eq!(fetched["source"], snapshot);
    assert_eq!(fetched["project_id"], "p");
    assert_eq!(fetched["db_id"], "db-1");
    assert_eq!(
        fake.requests.lock().unwrap().last().unwrap(),
        &(
            "get".into(),
            json!({"db":"copy","id":"node-1","body":true,"max_body_bytes":4096})
        )
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn native_owner_selection_keeps_endpoint_identity_and_path_together() {
    let (rt, fake, path) = fixture();
    let mut rt = rt
        .with_native_owners(BTreeMap::from([(
            "db-1".into(),
            NativeOwnerRoute {
                endpoint: Endpoint {
                    url: "route1".into(),
                    ssh_mcp_port: 18766,
                    token_env: None,
                },
                database: "main".into(),
                expected_path: Some("/owner/database.db".into()),
            },
        )]))
        .unwrap();
    let reference = LibraryGet {
        project_id: "p".into(),
        id: "node-1".into(),
        source_device_id: "owner".into(),
        generation: None,
    };
    let live = json!({"device_id":"owner","kind":"live","generation":null,"captured_at":null});
    assert_eq!(
        rt.query(query()).await.unwrap()["primary"][0]["source"],
        live
    );
    assert_eq!(rt.get(reference.clone()).await.unwrap()["source"], live);
    assert!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(url, _)| url == "route1")
    );
    for wrong_identity in [false, true] {
        fake.calls.lock().unwrap().clear();
        rt.native_owners.get_mut("db-1").unwrap().expected_path = Some(
            if wrong_identity {
                "/owner/database.db"
            } else {
                "/other/database.db"
            }
            .into(),
        );
        if wrong_identity {
            fake.database_ids
                .lock()
                .unwrap()
                .insert("route1".into(), "wrong-owner".into());
        }
        let response = rt.query(query()).await.unwrap();
        assert_eq!(response["coverage"][0]["state"], "refused");
        assert!(response["primary"].as_array().unwrap().is_empty());
        assert!(
            rt.get(reference.clone())
                .await
                .unwrap_err()
                .0
                .contains("mismatch")
        );
        assert!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .all(|(url, tool)| url == "route1" && tool == "databases")
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn snapshot_catalog_path_is_checked_for_query_and_exact_get() {
    let (rt, fake, path) = fixture();
    let mut catalog: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    catalog.entries[0].replicas[0].resolved_path = "/immutable/g2/database.db".into();
    std::fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    fake.errors.lock().unwrap().insert(
        "owner".into(),
        SourceFailure::Availability("offline".into()),
    );
    let response = rt.query(query()).await.unwrap();
    assert_eq!(response["coverage"][0]["state"], "refused");
    assert!(
        response["coverage"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("generation path mismatch")
    );
    assert!(
        rt.get(LibraryGet {
            project_id: "p".into(),
            id: "node-1".into(),
            source_device_id: "mirror".into(),
            generation: Some("g1".into()),
        })
        .await
        .unwrap_err()
        .0
        .contains("generation path mismatch")
    );
    assert!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(_, tool)| tool == "databases")
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn all_library_reads_honor_selection_and_fresh_admission_before_contact() {
    let (rt, fake, path) = fixture();
    let denied = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let guard_denied = denied.clone();
    let rt = rt.with_admission_guard(Arc::new(move || {
        if guard_denied.load(std::sync::atomic::Ordering::SeqCst) {
            Err(err("selection changed"))
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rt.query(query()).await.unwrap()["coverage"][0]["state"],
        "live"
    );
    fake.calls.lock().unwrap().clear();
    let rt = rt.with_read_authority(HashSet::new());
    assert_eq!(rt.catalog().unwrap()["entries"], json!([]));
    for (operation, arguments) in [
        (
            "library_query",
            json!({"text":"needle","project_ids":["p"]}),
        ),
        (
            "library_recall_context",
            json!({"text":"needle","project_ids":["p"]}),
        ),
        (
            "library_get",
            json!({"project_id":"p","id":"node-1","source_device_id":"owner"}),
        ),
    ] {
        assert!(
            rt.dispatch(operation, arguments)
                .await
                .unwrap_err()
                .0
                .contains("not enrolled")
        );
    }
    denied.store(true, std::sync::atomic::Ordering::SeqCst);
    for (operation, arguments) in [
        ("library_catalog", json!({})),
        ("library_query", json!({"text":"needle"})),
        ("library_recall_context", json!({"text":"needle"})),
        (
            "library_get",
            json!({"project_id":"p","id":"node-1","source_device_id":"owner"}),
        ),
    ] {
        assert_eq!(
            rt.dispatch(operation, arguments).await.unwrap_err().0,
            "selection changed"
        );
    }
    assert!(fake.calls.lock().unwrap().is_empty());
    std::fs::remove_file(path).unwrap();
}
