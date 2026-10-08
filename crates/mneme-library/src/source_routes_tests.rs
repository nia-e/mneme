use crate::{Catalog, ReplicaPin, tests::fixture};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn update_catalog(path: &Path, update: impl FnOnce(&mut Catalog)) {
    let mut catalog = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    update(&mut catalog);
    std::fs::write(path, serde_json::to_vec(&catalog).unwrap()).unwrap();
}

fn snapshot_fixture() -> (
    crate::LibraryRuntime,
    std::sync::Arc<crate::tests::Fake>,
    PathBuf,
) {
    let (runtime, fake, path) = fixture();
    update_catalog(&path, |catalog| {
        catalog.entries[0].replicas[0].database = "project_g1".into();
    });
    (runtime, fake, path)
}

fn rows(route: &crate::ReplicaRoute) -> Value {
    json!([{"db":route.database(),"db_id":route.expected_db_id(),"resolved_path":route.resolved_path(),"state":"open"}])
}

#[test]
fn newest_configured_replica_and_exact_retained_pin() {
    let (runtime, fake, path) = snapshot_fixture();
    update_catalog(&path, |catalog| {
        let entry = &mut catalog.entries[0];
        let mut newer = entry.replicas[0].clone();
        newer.generation = "g2".into();
        newer.database = "project_g2".into();
        newer.resolved_path = "/immutable/g2/database.db".into();
        // Same capture time: generation is the deterministic tie breaker.
        entry.replicas.push(newer.clone());
        newer.source_device_id = "unconfigured".into();
        newer.captured_at += 100;
        entry.replicas.push(newer);
    });
    let newest = runtime.resolve_replica("p", None).unwrap();
    assert_eq!(newest.generation(), "g2");
    assert_eq!(newest.endpoint().url, "replica");
    assert_eq!(newest.project_id(), "p");
    assert_eq!(newest.source_device_id(), "mirror");
    let pin = ReplicaPin {
        source_device_id: "mirror".into(),
        generation: "g1".into(),
    };
    let exact = runtime.resolve_replica("p", Some(&pin)).unwrap();
    assert_eq!(exact.database(), "project_g1");
    assert!(exact.captured_at() > 0);
    assert_eq!(exact.verify_databases(&rows(&exact)).unwrap(), "project_g1");
    assert!(fake.calls.lock().unwrap().is_empty());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn expired_or_unconfigured_pin_refuses_without_owner_or_newest_fallback() {
    let (mut runtime, fake, path) = snapshot_fixture();
    let pin = ReplicaPin {
        source_device_id: "mirror".into(),
        generation: "expired".into(),
    };
    assert!(
        runtime
            .resolve_replica("p", Some(&pin))
            .unwrap_err()
            .0
            .contains("expired")
    );
    runtime.config.replicas.clear();
    assert!(
        runtime
            .resolve_replica("p", None)
            .unwrap_err()
            .0
            .contains("no configured replica")
    );
    let pin = ReplicaPin {
        generation: "g1".into(),
        ..pin
    };
    assert!(
        runtime
            .resolve_replica("p", Some(&pin))
            .unwrap_err()
            .0
            .contains("endpoint is not configured")
    );
    assert!(fake.calls.lock().unwrap().is_empty());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn publication_between_resolutions_changes_newest_but_preserves_exact_reference() {
    let (runtime, _, path) = snapshot_fixture();
    let original = runtime.resolve_replica("p", None).unwrap();
    let pin = ReplicaPin {
        source_device_id: original.source_device_id().into(),
        generation: original.generation().into(),
    };
    update_catalog(&path, |catalog| {
        catalog.revision += 1;
        let mut newer = catalog.entries[0].replicas[0].clone();
        newer.generation = "g2".into();
        newer.database = "project_g2".into();
        newer.resolved_path = "/immutable/g2/database.db".into();
        newer.captured_at += 1;
        catalog.entries[0].replicas.push(newer);
    });
    assert_eq!(
        runtime.resolve_replica("p", None).unwrap().generation(),
        "g2"
    );
    let exact = runtime.resolve_replica("p", Some(&pin)).unwrap();
    assert_eq!(exact.generation(), original.generation());
    assert_eq!(exact.resolved_path(), original.resolved_path());
    update_catalog(&path, |catalog| {
        catalog.entries[0].replicas.remove(0);
    });
    assert!(
        runtime
            .resolve_replica("p", Some(&pin))
            .unwrap_err()
            .0
            .contains("expired")
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn native_rows_refuse_wrong_identity_path_state_alias_and_duplicate() {
    let (runtime, _, path) = snapshot_fixture();
    let route = runtime.resolve_replica("p", None).unwrap();
    for (field, value) in [
        ("db_id", json!("another-db")),
        ("resolved_path", json!("/immutable/g2/database.db")),
        ("state", json!("maintenance")),
        ("state", json!("unknown")),
        ("db", json!("project_current")),
        ("db", Value::Null),
    ] {
        let mut wrong = rows(&route);
        wrong[0][field] = value;
        assert!(route.verify_databases(&wrong).is_err(), "{field}: {wrong}");
    }
    let mut duplicate = rows(&route);
    let second_row = duplicate[0].clone();
    duplicate.as_array_mut().unwrap().push(second_row);
    assert!(route.verify_databases(&duplicate).is_err());
    assert!(route.verify_databases(&json!({})).is_err());
    // A sibling's `name` cannot shadow the requested literal native db alias.
    let good = rows(&route)[0].clone();
    assert_eq!(
        route
            .verify_databases(&json!([
                {"db":"other","name":"project_g1","db_id":"wrong"}, good
            ]))
            .unwrap(),
        "project_g1"
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn mutable_alias_or_path_is_not_snapshot_proof() {
    let (runtime, _, path) = snapshot_fixture();
    for alias in [
        "copy",
        "project_current",
        "project_previous",
        "project_current_g1",
        "project_g2",
        "/immutable/g1/database.db",
    ] {
        update_catalog(&path, |catalog| {
            catalog.entries[0].replicas[0].database = alias.into()
        });
        assert!(
            runtime
                .resolve_replica("p", None)
                .unwrap_err()
                .0
                .contains("per-generation native alias")
        );
    }
    update_catalog(&path, |catalog| {
        catalog.entries[0].replicas[0].database = "project_g1".into()
    });
    for serving_path in [
        "/immutable/current/database.db",
        "/immutable/previous/g1/database.db",
        "/immutable/g2/database.db",
        "/immutable/g1/../g2/database.db",
        "relative/g1/database.db",
    ] {
        update_catalog(&path, |catalog| {
            catalog.entries[0].replicas[0].resolved_path = serving_path.into()
        });
        assert!(
            runtime.resolve_replica("p", None).is_err(),
            "{serving_path}"
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn withdrawn_unknown_and_empty_reference_refuse_without_remote_contact() {
    let (runtime, fake, path) = snapshot_fixture();
    assert!(runtime.resolve_replica("", None).is_err());
    assert!(runtime.resolve_replica("not-enrolled", None).is_err());
    assert!(
        runtime
            .resolve_replica(
                "p",
                Some(&ReplicaPin {
                    source_device_id: "".into(),
                    generation: "g1".into()
                })
            )
            .is_err()
    );
    update_catalog(&path, |catalog| catalog.entries[0].withdrawn = true);
    assert!(runtime.resolve_replica("p", None).is_err());
    assert!(fake.calls.lock().unwrap().is_empty());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn ambiguous_pin_and_unsafe_newest_refuse_instead_of_selecting_another_edition() {
    let (runtime, _, path) = snapshot_fixture();
    update_catalog(&path, |catalog| {
        let duplicate = catalog.entries[0].replicas[0].clone();
        catalog.entries[0].replicas.push(duplicate);
    });
    let pin = ReplicaPin {
        source_device_id: "mirror".into(),
        generation: "g1".into(),
    };
    assert!(
        runtime
            .resolve_replica("p", Some(&pin))
            .unwrap_err()
            .0
            .contains("ambiguous")
    );
    assert!(
        runtime
            .resolve_replica("p", None)
            .unwrap_err()
            .0
            .contains("ambiguous")
    );
    update_catalog(&path, |catalog| {
        let newest = &mut catalog.entries[0].replicas[1];
        newest.generation = "g2".into();
        newest.captured_at += 1;
        newest.database = "project_current".into();
        newest.resolved_path = "/immutable/g2/database.db".into();
    });
    assert!(
        runtime
            .resolve_replica("p", None)
            .unwrap_err()
            .0
            .contains("per-generation")
    );
    assert_eq!(
        runtime
            .resolve_replica("p", Some(&pin))
            .unwrap()
            .generation(),
        "g1"
    );
    std::fs::remove_file(path).unwrap();
}
