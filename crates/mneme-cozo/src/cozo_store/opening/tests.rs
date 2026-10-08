use std::fs;
use std::sync::{Barrier, mpsc};

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use mneme_core::ports::{FeedbackCommit, FeedbackIdempotency, FeedbackRetryScope, GraphStore};

use super::*;
use crate::canonical_node_contract::EPISODE_CONTEXT_META_KEY as CANONICAL_NODE_META_KEY;
use crate::storage_contract::conventional_unmanaged::spec::{
    CatalogContract, LEGACY_CATALOG_GENERATION_MARKER,
};

#[derive(Clone, Copy)]
enum FixtureGeneration {
    Legacy,
    Current,
}

#[derive(Clone, Copy)]
enum FixtureMutation {
    None,
    RemoveMeta(&'static str),
    PutMeta(&'static str, &'static str),
    MissingPermanentGuard,
    WrongPermanentGuard,
    ExtraPermanentGuard,
    NonemptyRetiredTagGuard,
    WrongNodeAccess,
}

fn fixture_path(label: &str) -> PathBuf {
    let parent = fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
    parent.join(format!("mneme-existing-open-{label}-{}.db", Ulid::new()))
}

fn remove_fixture(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let _ = fs::remove_file(PathBuf::from(name));
    }
    if let Ok(lock) = mneme_store_path::store_lock_path(path) {
        let _ = fs::remove_file(lock);
    }
}

fn create_fixture(
    label: &str,
    dim: usize,
    generation: FixtureGeneration,
    mutation: FixtureMutation,
) -> (PathBuf, Ulid) {
    let path = fixture_path(label);
    remove_fixture(&path);
    let db_id = populate_fixture(&path, dim, generation, mutation);
    (path, db_id)
}

fn populate_fixture(
    path: &Path,
    dim: usize,
    generation: FixtureGeneration,
    mutation: FixtureMutation,
) -> Ulid {
    let database = DbInstance::new("sqlite", path, "").unwrap();
    let db_id = Ulid::new();
    let identity = DatabaseId::new(db_id).unwrap();
    match generation {
        FixtureGeneration::Current => {
            super::super::creation::stage_touchstones_schema(&database, dim, identity).unwrap()
        }
        FixtureGeneration::Legacy => {
            super::super::creation::stage_current_schema(&database, dim, identity).unwrap()
        }
    }
    let store = super::super::fresh_current::store_over_staged_database(database, dim, db_id);

    match mutation {
        FixtureMutation::MissingPermanentGuard
        | FixtureMutation::WrongPermanentGuard
        | FixtureMutation::ExtraPermanentGuard => {
            store
                .run(
                    "::access_level normal mneme_reembed_shadow_node_vec",
                    BTreeMap::new(),
                    true,
                )
                .expect("make fixture guard writable");
            let mut params = BTreeMap::new();
            params.insert(
                "fence".into(),
                super::super::dv_str(crate::vector_projection::GUARD_KEY),
            );
            match mutation {
                FixtureMutation::MissingPermanentGuard => {
                    store
                        .run(
                            "?[fence] <- [[$fence]] :rm mneme_reembed_shadow_node_vec {fence}",
                            params,
                            true,
                        )
                        .expect("remove fixture guard");
                }
                FixtureMutation::WrongPermanentGuard => {
                    params.insert("generation".into(), super::super::dv_str("wrong"));
                    store
                        .run(
                            "?[fence, generation] <- [[$fence, $generation]] :put mneme_reembed_shadow_node_vec {fence => generation}",
                            params,
                            true,
                        )
                        .expect("replace fixture guard");
                }
                FixtureMutation::ExtraPermanentGuard => {
                    params.insert("fence".into(), super::super::dv_str("unexpected"));
                    params.insert("generation".into(), super::super::dv_str("unexpected"));
                    store
                        .run(
                            "?[fence, generation] <- [[$fence, $generation]] :put mneme_reembed_shadow_node_vec {fence => generation}",
                            params,
                            true,
                        )
                        .expect("add fixture guard");
                }
                _ => unreachable!(),
            }
            store
                .run(
                    "::access_level read_only mneme_reembed_shadow_node_vec",
                    BTreeMap::new(),
                    true,
                )
                .expect("restore fixture guard access");
        }
        FixtureMutation::NonemptyRetiredTagGuard => {
            store
                .run("::access_level normal node_tag", BTreeMap::new(), true)
                .expect("make retired fixture guard writable");
            let mut params = BTreeMap::new();
            params.insert("id".into(), super::super::dv_str("retired-id"));
            params.insert("tag".into(), super::super::dv_str("retired-tag"));
            store
                .run(
                    "?[id, tag] <- [[$id, $tag]] :put node_tag {id, tag}",
                    params,
                    true,
                )
                .expect("populate retired fixture guard");
            store
                .run("::access_level read_only node_tag", BTreeMap::new(), true)
                .expect("restore retired fixture guard access");
        }
        FixtureMutation::WrongNodeAccess => {
            store
                .run("::access_level read_only node", BTreeMap::new(), true)
                .expect("mutate fixture access");
        }
        FixtureMutation::None | FixtureMutation::RemoveMeta(_) | FixtureMutation::PutMeta(_, _) => {
        }
    }

    let catalog = match generation {
        FixtureGeneration::Current => CatalogContract::TouchstonesV1,
        FixtureGeneration::Legacy => CatalogContract::Predecessor,
    };
    let names = catalog
        .relations()
        .map(|relation| relation.name.to_owned())
        .collect::<Vec<_>>();
    let tx = store.db.multi_transaction(true);
    tx.canonicalize_relation_catalog_v1(names.clone(), names.len())
        .expect("canonicalize fixture catalog");
    if matches!(generation, FixtureGeneration::Legacy) {
        let mut params = BTreeMap::new();
        params.insert("k".into(), super::super::dv_str(VECTOR_PROJECTION_META_KEY));
        params.insert(
            "v".into(),
            super::super::dv_str(LEGACY_CATALOG_GENERATION_MARKER),
        );
        tx.run_script("?[k, v] <- [[$k, $v]] :put meta {k => v}", params)
            .expect("publish fixture marker");
    }
    match mutation {
        FixtureMutation::RemoveMeta(key) => {
            let mut params = BTreeMap::new();
            params.insert("k".into(), super::super::dv_str(key));
            tx.run_script("?[k] <- [[$k]] :rm meta {k}", params)
                .expect("remove fixture metadata");
        }
        FixtureMutation::PutMeta(key, value) => {
            let mut params = BTreeMap::new();
            params.insert("k".into(), super::super::dv_str(key));
            params.insert("v".into(), super::super::dv_str(value));
            tx.run_script("?[k, v] <- [[$k, $v]] :put meta {k => v}", params)
                .expect("replace fixture metadata");
        }
        _ => {}
    }
    tx.commit().expect("commit fixture catalog");
    drop(tx);
    store
        .prepare_for_file_move()
        .expect("checkpoint fixture sidecars");
    drop(store);
    db_id
}

#[cfg(unix)]
fn published_fixture(label: &str, dim: usize) -> (PathBuf, Ulid, PublishedFreshStoreV1) {
    published_fixture_with_mutation(label, dim, FixtureMutation::None)
}

#[cfg(unix)]
fn published_fixture_with_mutation(
    label: &str,
    dim: usize,
    mutation: FixtureMutation,
) -> (PathBuf, Ulid, PublishedFreshStoreV1) {
    let path = fixture_path(label);
    remove_fixture(&path);
    let lease = StoreLease::acquire(&path).expect("published fixture lease");
    let stage = lease
        .begin_fresh_store_v1(
            &path,
            mneme_store_path::FreshStoreStageSpecV1 {
                operation_id: Ulid::new(),
                policy_binding: [0x5a; 32],
            },
        )
        .expect("begin published fixture stage");
    let db_id = populate_fixture(
        stage.database_path(),
        dim,
        FixtureGeneration::Current,
        mutation,
    );
    let prepared = stage
        .seal_closed_file()
        .expect("seal published fixture stage");
    let published = prepared.publish().expect("publish fixture");
    assert_eq!(published.path(), path);
    (path, db_id, published)
}

fn raw_fixture_store(path: &Path) -> super::super::CozoStore {
    super::super::CozoStore {
        db: DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap(),
        persistent_authority: None,
        dim: 4,
        db_id: Ulid::new(),
        backend_activity: BackendActivity::default(),
        tagged_read_admission: TaggedReadAdmission::default(),
        tagged_read_test_hook: Arc::new(super::super::TaggedReadTestHook::default()),
        query_count: std::sync::atomic::AtomicUsize::new(0),
        last_maintenance_statements: std::sync::atomic::AtomicUsize::new(0),
    }
}

fn admitted_fixture(label: &str) -> (PathBuf, super::super::CozoStore) {
    let (path, _) = create_fixture(label, 4, FixtureGeneration::Current, FixtureMutation::None);
    let lease = StoreLease::acquire(&path).expect("fixture lease");
    let store = super::super::CozoStore::open_existing_current(&path, lease)
        .expect("admitted existing fixture");
    (path, store)
}

fn put_feedback_proof(
    store: &super::super::CozoStore,
    key: &str,
    fingerprint: &str,
    sequence: i64,
) {
    let mut params = BTreeMap::new();
    params.insert("key".into(), super::super::dv_str(key));
    params.insert("fingerprint".into(), super::super::dv_str(fingerprint));
    params.insert("sequence".into(), super::super::dv_int(sequence));
    store
        .run(
            "?[key, fingerprint, applied_at] <- [[$key, $fingerprint, $sequence]] \
             :put feedback_retry {key => fingerprint, applied_at}",
            params,
            true,
        )
        .expect("insert feedback proof fixture");
}

fn put_feedback_order(
    store: &super::super::CozoStore,
    epoch: &str,
    sequence: i64,
    key: &str,
    marker: bool,
) {
    let mut params = BTreeMap::new();
    params.insert("epoch".into(), super::super::dv_str(epoch));
    params.insert("sequence".into(), super::super::dv_int(sequence));
    params.insert("key".into(), super::super::dv_str(key));
    params.insert("marker".into(), DataValue::Bool(marker));
    store
        .run(
            "?[epoch, sequence, key, marker] <- [[$epoch, $sequence, $key, $marker]] \
             :put feedback_retry_order {epoch, sequence, key => marker}",
            params,
            true,
        )
        .expect("insert feedback order fixture");
}

fn put_feedback_order_range(store: &super::super::CozoStore, epoch: &str, count: usize) {
    for chunk_start in (0..count).step_by(128) {
        let chunk_end = (chunk_start + 128).min(count);
        let mut params = BTreeMap::new();
        params.insert("epoch".into(), super::super::dv_str(epoch));
        let rows = (chunk_start..chunk_end)
            .map(|index| {
                let sequence = format!("sequence_{index}");
                let key = format!("key_{index}");
                params.insert(
                    sequence.clone(),
                    super::super::dv_int(i64::try_from(index + 1).unwrap()),
                );
                params.insert(
                    key.clone(),
                    super::super::dv_str(&format!("overflow:{index}")),
                );
                format!("[$epoch, ${sequence}, ${key}, true]")
            })
            .collect::<Vec<_>>()
            .join(", ");
        store
            .run(
                &format!(
                    "?[epoch, sequence, key, marker] <- [{rows}] \
                     :put feedback_retry_order {{epoch, sequence, key => marker}}"
                ),
                params,
                true,
            )
            .expect("insert feedback order range fixture");
    }
}

fn put_feedback_proof_range(store: &super::super::CozoStore, count: usize) {
    for chunk_start in (0..count).step_by(128) {
        let chunk_end = (chunk_start + 128).min(count);
        let mut params = BTreeMap::new();
        params.insert("fingerprint".into(), super::super::dv_str(&"a".repeat(64)));
        let rows = (chunk_start..chunk_end)
            .map(|index| {
                let key = format!("key_{index}");
                let sequence = format!("sequence_{index}");
                params.insert(
                    key.clone(),
                    super::super::dv_str(&format!("proof-overflow:{index}")),
                );
                params.insert(
                    sequence.clone(),
                    super::super::dv_int(i64::try_from(index + 1).unwrap()),
                );
                format!("[${key}, $fingerprint, ${sequence}]")
            })
            .collect::<Vec<_>>()
            .join(", ");
        store
            .run(
                &format!(
                    "?[key, fingerprint, applied_at] <- [{rows}] \
                     :put feedback_retry {{key => fingerprint, applied_at}}"
                ),
                params,
                true,
            )
            .expect("insert feedback proof range fixture");
    }
}

fn feedback_ledger_snapshot(
    store: &super::super::CozoStore,
) -> (Vec<Vec<DataValue>>, Vec<Vec<DataValue>>) {
    let proofs = store
        .run(
            "?[key, fingerprint, applied_at] := *feedback_retry{key, fingerprint, applied_at} \
             :order key",
            BTreeMap::new(),
            false,
        )
        .unwrap()
        .rows;
    let orders = store
        .run(
            "?[epoch, sequence, key, marker] := \
               *feedback_retry_order{epoch, sequence, key, marker} \
             :order epoch, sequence, key",
            BTreeMap::new(),
            false,
        )
        .unwrap()
        .rows;
    (proofs, orders)
}

fn feedback_commit(key: &str, payload: &str, epoch: &str) -> FeedbackCommit {
    let digest = payload
        .bytes()
        .fold(0u64, |hash, byte| hash.wrapping_mul(257) ^ u64::from(byte));
    FeedbackCommit {
        idempotency: Some(
            FeedbackIdempotency::new(
                key,
                format!("{digest:064x}"),
                FeedbackRetryScope::new(epoch, 1, 1).unwrap(),
            )
            .unwrap(),
        ),
        applied_at: 1,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    }
}

fn expect_refusal(result: Result<super::super::CozoStore>, message: &'static str) -> Error {
    match result {
        Ok(_) => panic!("{message}"),
        Err(error) => error,
    }
}

#[test]
fn absent_database_refuses_without_creating_any_sqlite_family_file() {
    let path = fixture_path("absent");
    remove_fixture(&path);
    let lease = StoreLease::acquire(&path).expect("lease for absent fixture path");

    let error = expect_refusal(
        super::super::CozoStore::open_existing_current(&path, lease),
        "an absent database must refuse",
    );
    assert!(
        error
            .to_string()
            .contains("existing_store_admission_failed"),
        "unexpected absent-source refusal: {error}"
    );
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        assert!(
            !PathBuf::from(name).exists(),
            "existing-only refusal created SQLite family member {suffix:?}"
        );
    }
    remove_fixture(&path);
}

#[test]
fn admitted_existing_open_uses_permit_dimension_and_exact_database_id() {
    let (path, expected_db_id) = create_fixture(
        "current",
        7,
        FixtureGeneration::Current,
        FixtureMutation::None,
    );
    let lease = StoreLease::acquire(&path).expect("fixture lease");
    let store = super::super::CozoStore::open_existing_current(&path, lease)
        .expect("admitted existing open");
    assert_eq!(store.dim, 7);
    assert_eq!(store.db_id(), expected_db_id);
    assert!(store.persistent_authority.is_some());
    drop(store);
    remove_fixture(&path);
}

#[cfg(unix)]
#[test]
fn published_open_retains_capability_across_runtime_sidecars_and_lease_contention() {
    let (path, expected_db_id, published) = published_fixture("published-current", 7);
    let published = Arc::new(published);
    let store = super::super::CozoStore::open_published_current(Arc::clone(&published))
        .expect("admit published current store");
    drop(published);
    assert_eq!(store.dim, 7);
    assert_eq!(store.db_id(), expected_db_id);

    store
        .put_meta("published_runtime_guard_test", "before")
        .expect("seed write through published store handle");
    let peer = DbInstance::new("sqlite", &path, "").expect("open runtime sidecar peer");
    let reader = peer.multi_transaction(false);
    reader
        .run_script(
            "?[v] := *meta{k: 'published_runtime_guard_test', v}",
            BTreeMap::new(),
        )
        .expect("pin peer SQLite snapshot");
    store
        .put_meta("published_runtime_guard_test", "after")
        .expect("write WAL frame through published store handle");
    assert_eq!(
        store
            .read_meta("published_runtime_guard_test")
            .expect("read runtime write")
            .as_deref(),
        Some("after")
    );
    let runtime_sidecar_exists = ["-wal", "-shm", "-journal"].into_iter().any(|suffix| {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        PathBuf::from(candidate).exists()
    });
    assert!(
        runtime_sidecar_exists,
        "runtime write did not create a SQLite sidecar"
    );
    store
        .persistent_authority
        .as_ref()
        .expect("published store authority")
        .require_live_guards("published runtime-sidecar test")
        .expect("runtime guards must permit legitimate SQLite sidecars and file growth");
    assert_eq!(
        StoreLease::acquire(&path).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "store handle must retain the published capability's lease"
    );

    reader.abort().expect("release peer SQLite snapshot");
    drop(reader);
    drop(peer);
    drop(store);
    let replacement = StoreLease::acquire(&path)
        .expect("dropping the published store handle must release its lease");
    drop(replacement);
    remove_fixture(&path);
}

#[cfg(unix)]
#[test]
fn rejected_published_open_leaves_outer_capability_live_and_exclusive() {
    let (path, _, published) = published_fixture_with_mutation(
        "published-current-rejection",
        4,
        FixtureMutation::RemoveMeta("db_id"),
    );
    let published = Arc::new(published);
    let error = expect_refusal(
        super::super::CozoStore::open_published_current(Arc::clone(&published)),
        "published current store with missing mandatory metadata must refuse",
    );
    assert!(
        error.to_string().contains("rejected database id"),
        "unexpected published-current refusal: {error}"
    );
    assert_eq!(
        Arc::strong_count(&published),
        1,
        "failed open must release only its cloned publication authority"
    );
    published
        .require_guards()
        .expect("outer publication capability must retain all pre-open guards after rejection");
    assert_eq!(
        StoreLease::acquire(&path).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "outer publication capability must retain its exclusive lease after rejection"
    );

    drop(published);
    let replacement = StoreLease::acquire(&path)
        .expect("dropping the final publication capability must release its lease");
    drop(replacement);
    remove_fixture(&path);
}

#[test]
fn legacy_marker_returns_exact_upgrade_action_without_mutating_database() {
    let (path, _) = create_fixture(
        "legacy",
        4,
        FixtureGeneration::Legacy,
        FixtureMutation::None,
    );
    let lease = StoreLease::acquire(&path).expect("fixture lease");
    let before = fs::read(&path).expect("read fixture before refusal");
    let error = expect_refusal(
        super::super::CozoStore::open_existing_current(&path, lease),
        "legacy generation must refuse",
    );
    assert_eq!(
        error.to_string(),
        "backend: episode_context_upgrade_required_or_invalid: exact current admission failed; ConcernV1 predecessors require single-graph-upgrade --target-generation episode-context-v2 --backend sqlite --output <ABSENT_PATH>; earlier predecessors require the named historical targets first"
    );
    assert_eq!(fs::read(&path).expect("read fixture after refusal"), before);
    remove_fixture(&path);
}

#[test]
fn persistent_router_admits_current_and_retains_the_shared_lease() {
    let (path, expected_id) = create_fixture(
        "persistent-current",
        4,
        FixtureGeneration::Current,
        FixtureMutation::None,
    );
    let lease = Arc::new(StoreLease::acquire(&path).expect("fixture lease"));
    let store = super::super::CozoStore::open_persistent(&path, 99, lease.clone())
        .expect("route exact current generation");
    assert_eq!(store.db_id(), expected_id);
    assert_eq!(
        store.dim, 4,
        "current admission owns the persisted dimension"
    );
    assert!(
        store
            .persistent_authority
            .as_ref()
            .expect("persistent authority")
            .has_current_admission_receipt()
    );
    drop(lease);
    assert_eq!(
        StoreLease::acquire(&path).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(store);
    drop(StoreLease::acquire(&path).expect("store release returns lease"));
    remove_fixture(&path);
}

#[test]
fn persistent_router_refuses_legacy_without_writable_fallback() {
    let (path, _) = create_fixture(
        "persistent-legacy",
        4,
        FixtureGeneration::Legacy,
        FixtureMutation::None,
    );
    let before = fs::read(&path).unwrap();
    let lease = Arc::new(StoreLease::acquire(&path).unwrap());
    assert!(super::super::CozoStore::open_persistent(&path, 99, lease.clone()).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(lease);
    remove_fixture(&path);
}

#[test]
fn persistent_router_never_falls_back_after_torn_current_classification() {
    let (path, _) = create_fixture(
        "persistent-torn",
        4,
        FixtureGeneration::Current,
        FixtureMutation::RemoveMeta("dim"),
    );
    let lease = Arc::new(StoreLease::acquire(&path).expect("fixture lease"));
    let before = fs::read(&path).expect("read torn fixture");
    let error = expect_refusal(
        super::super::CozoStore::open_persistent(&path, 4, lease.clone()),
        "torn current generation must refuse",
    );
    assert!(
        error
            .to_string()
            .contains("post-constructor current validation rejected vector dimension")
    );
    assert_eq!(fs::read(&path).expect("reread torn fixture"), before);
    assert_eq!(Arc::strong_count(&lease), 1);
    drop(lease);
    remove_fixture(&path);
}

#[test]
fn persistent_router_preserves_pre_1_0_absent_store_creation() {
    let path = fixture_path("persistent-absent");
    remove_fixture(&path);
    let lease = Arc::new(StoreLease::acquire(&path).expect("absent target lease"));
    let store = super::super::CozoStore::open_persistent(&path, 4, lease.clone())
        .expect("create conventional store under retained lease");
    assert!(path.is_file());
    assert_eq!(store.dim, 4);
    assert!(store.persistent_authority.is_some());
    drop(lease);
    assert_eq!(
        StoreLease::acquire(&path).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    store
        .prepare_for_file_move()
        .expect("checkpoint created fixture");
    drop(store);
    drop(StoreLease::acquire(&path).expect("created store releases lease"));
    remove_fixture(&path);
}

#[test]
fn existing_persistent_router_refuses_absence_without_creating() {
    let path = fixture_path("persistent-existing-absent");
    remove_fixture(&path);
    let lease = Arc::new(StoreLease::acquire(&path).expect("absent target lease"));
    let error = expect_refusal(
        super::super::CozoStore::open_existing_persistent(&path, 4, lease.clone()),
        "existing-only route must refuse absence",
    );
    assert!(
        error
            .to_string()
            .contains("existing_store_admission_failed")
    );
    assert!(!path.exists());
    assert_eq!(Arc::strong_count(&lease), 1);
    drop(lease);
    remove_fixture(&path);
}

#[test]
fn missing_mandatory_metadata_refuses_after_admission_without_repair() {
    let (path, _) = create_fixture(
        "missing-db-id",
        4,
        FixtureGeneration::Current,
        FixtureMutation::RemoveMeta("db_id"),
    );
    let lease = StoreLease::acquire(&path).expect("fixture lease");
    let error = expect_refusal(
        super::super::CozoStore::open_existing_current(&path, lease),
        "missing db_id must refuse",
    );
    assert!(error.to_string().contains("rejected database id"));

    let raw = raw_fixture_store(&path);
    assert_eq!(raw.read_meta("db_id").unwrap(), None);
    drop(raw);
    remove_fixture(&path);
}

#[test]
fn mandatory_metadata_and_fingerprint_corruption_refuse_post_open_without_repair() {
    struct Case {
        label: &'static str,
        mutation: FixtureMutation,
        key: &'static str,
        expected: Option<&'static str>,
    }

    let cases = [
        Case {
            label: "invalid-db-id",
            mutation: FixtureMutation::PutMeta("db_id", "not-a-ulid"),
            key: "db_id",
            expected: Some("not-a-ulid"),
        },
        Case {
            label: "nil-db-id",
            mutation: FixtureMutation::PutMeta("db_id", "00000000000000000000000000"),
            key: "db_id",
            expected: Some("00000000000000000000000000"),
        },
        Case {
            label: "noncanonical-db-id",
            mutation: FixtureMutation::PutMeta("db_id", "01arz3ndektsv4rrffq69g5fav"),
            key: "db_id",
            expected: Some("01arz3ndektsv4rrffq69g5fav"),
        },
        Case {
            label: "missing-dim",
            mutation: FixtureMutation::RemoveMeta("dim"),
            key: "dim",
            expected: None,
        },
        Case {
            label: "wrong-dim",
            mutation: FixtureMutation::PutMeta("dim", "5"),
            key: "dim",
            expected: Some("5"),
        },
        Case {
            label: "missing-incident-cap",
            mutation: FixtureMutation::RemoveMeta(INCIDENT_EDGE_CAP_META_KEY),
            key: INCIDENT_EDGE_CAP_META_KEY,
            expected: None,
        },
        Case {
            label: "wrong-incident-cap",
            mutation: FixtureMutation::PutMeta(INCIDENT_EDGE_CAP_META_KEY, "1025"),
            key: INCIDENT_EDGE_CAP_META_KEY,
            expected: Some("1025"),
        },
        Case {
            label: "missing-remote-cap",
            mutation: FixtureMutation::RemoveMeta(REMOTE_EDGE_SOURCE_CAP_META_KEY),
            key: REMOTE_EDGE_SOURCE_CAP_META_KEY,
            expected: None,
        },
        Case {
            label: "wrong-remote-cap",
            mutation: FixtureMutation::PutMeta(REMOTE_EDGE_SOURCE_CAP_META_KEY, "257"),
            key: REMOTE_EDGE_SOURCE_CAP_META_KEY,
            expected: Some("257"),
        },
        Case {
            label: "missing-lexical-marker",
            mutation: FixtureMutation::RemoveMeta(LEXICAL_PROJECTION_META_KEY),
            key: LEXICAL_PROJECTION_META_KEY,
            expected: None,
        },
        Case {
            label: "wrong-lexical-marker",
            mutation: FixtureMutation::PutMeta(LEXICAL_PROJECTION_META_KEY, "partial"),
            key: LEXICAL_PROJECTION_META_KEY,
            expected: Some("partial"),
        },
        Case {
            label: "malformed-fingerprint",
            mutation: FixtureMutation::PutMeta(super::super::EMBEDDING_FINGERPRINT_META_KEY, "{"),
            key: super::super::EMBEDDING_FINGERPRINT_META_KEY,
            expected: Some("{"),
        },
        Case {
            label: "wrong-fingerprint-dimension",
            mutation: FixtureMutation::PutMeta(
                super::super::EMBEDDING_FINGERPRINT_META_KEY,
                r#"{"format_version":1,"embedding_id":"test:wrong-dim","dimension":5,"normalization":"l2-f32-v1","query_mode":"symmetric-v1"}"#,
            ),
            key: super::super::EMBEDDING_FINGERPRINT_META_KEY,
            expected: Some(
                r#"{"format_version":1,"embedding_id":"test:wrong-dim","dimension":5,"normalization":"l2-f32-v1","query_mode":"symmetric-v1"}"#,
            ),
        },
    ];

    for case in cases {
        let (path, _) = create_fixture(case.label, 4, FixtureGeneration::Current, case.mutation);
        let lease = StoreLease::acquire(&path).expect("fixture lease");
        let error = expect_refusal(
            super::super::CozoStore::open_existing_current(&path, lease),
            "mandatory metadata corruption must refuse",
        );
        assert!(
            error
                .to_string()
                .contains("post-constructor current validation rejected"),
            "{} unexpectedly refused before the post-open validator: {error}",
            case.label
        );

        let raw = raw_fixture_store(&path);
        assert_eq!(
            raw.read_meta(case.key).unwrap().as_deref(),
            case.expected,
            "{} was repaired during refusal",
            case.label
        );
        drop(raw);
        remove_fixture(&path);
    }
}

#[test]
fn companion_marker_corruption_refuses_in_raw_fence_without_repair() {
    struct Case {
        label: &'static str,
        mutation: FixtureMutation,
        key: &'static str,
        expected: Option<&'static str>,
    }

    let cases = [
        Case {
            label: "missing-vector-marker",
            mutation: FixtureMutation::RemoveMeta(VECTOR_PROJECTION_META_KEY),
            key: VECTOR_PROJECTION_META_KEY,
            expected: None,
        },
        Case {
            label: "wrong-vector-marker",
            mutation: FixtureMutation::PutMeta(VECTOR_PROJECTION_META_KEY, "wrong"),
            key: VECTOR_PROJECTION_META_KEY,
            expected: Some("wrong"),
        },
        Case {
            label: "missing-canonical-marker",
            mutation: FixtureMutation::RemoveMeta(CANONICAL_NODE_META_KEY),
            key: CANONICAL_NODE_META_KEY,
            expected: None,
        },
        Case {
            label: "wrong-canonical-marker",
            mutation: FixtureMutation::PutMeta(CANONICAL_NODE_META_KEY, "wrong"),
            key: CANONICAL_NODE_META_KEY,
            expected: Some("wrong"),
        },
        Case {
            label: "missing-tag-marker",
            mutation: FixtureMutation::RemoveMeta(crate::tag_projection::META_KEY),
            key: crate::tag_projection::META_KEY,
            expected: None,
        },
        Case {
            label: "wrong-tag-marker",
            mutation: FixtureMutation::PutMeta(crate::tag_projection::META_KEY, "wrong"),
            key: crate::tag_projection::META_KEY,
            expected: Some("wrong"),
        },
    ];

    for case in cases {
        let (path, _) = create_fixture(case.label, 4, FixtureGeneration::Current, case.mutation);
        let lease = StoreLease::acquire(&path).expect("fixture lease");
        let error = expect_refusal(
            super::super::CozoStore::open_existing_current(&path, lease),
            "companion marker corruption must refuse",
        );
        assert!(
            error
                .to_string()
                .contains("episode_context_upgrade_required_or_invalid"),
            "{} unexpectedly reached the runtime validator: {error}",
            case.label
        );

        let raw = raw_fixture_store(&path);
        assert_eq!(
            raw.read_meta(case.key).unwrap().as_deref(),
            case.expected,
            "{} was repaired during raw-fence refusal",
            case.label
        );
        drop(raw);
        remove_fixture(&path);
    }
}

#[test]
fn semantic_guard_corruption_refuses_in_raw_fence_without_repair() {
    let cases = [
        ("missing-guard", FixtureMutation::MissingPermanentGuard, 0),
        ("wrong-guard", FixtureMutation::WrongPermanentGuard, 1),
        ("extra-guard", FixtureMutation::ExtraPermanentGuard, 2),
    ];
    for (label, mutation, expected_rows) in cases {
        let (path, _) = create_fixture(label, 4, FixtureGeneration::Current, mutation);
        let lease = StoreLease::acquire(&path).expect("fixture lease");
        let error = expect_refusal(
            super::super::CozoStore::open_existing_current(&path, lease),
            "guard corruption must refuse",
        );
        assert!(
            error
                .to_string()
                .contains("episode_context_upgrade_required_or_invalid")
        );

        let raw = raw_fixture_store(&path);
        let rows = raw
            .run(
                "?[fence, generation] := *mneme_reembed_shadow_node_vec{fence, generation} :order fence :limit 3",
                BTreeMap::new(),
                false,
            )
            .unwrap();
        assert_eq!(rows.rows.len(), expected_rows, "{label} was repaired");
        match mutation {
            FixtureMutation::WrongPermanentGuard => assert!(exact_string_pair(
                &rows,
                crate::vector_projection::GUARD_KEY,
                "wrong"
            )),
            FixtureMutation::ExtraPermanentGuard => assert!(rows.rows.iter().any(|row| {
                matches!(row.as_slice(), [DataValue::Str(left), DataValue::Str(right)]
                    if left.as_str() == "unexpected" && right.as_str() == "unexpected")
            })),
            FixtureMutation::MissingPermanentGuard => {}
            _ => unreachable!(),
        }
        drop(raw);
        remove_fixture(&path);
    }
}

#[test]
fn retired_guard_and_catalog_access_corruption_refuse_before_constructor() {
    for (label, mutation) in [
        (
            "nonempty-retired-guard",
            FixtureMutation::NonemptyRetiredTagGuard,
        ),
        ("wrong-node-access", FixtureMutation::WrongNodeAccess),
    ] {
        let (path, _) = create_fixture(label, 4, FixtureGeneration::Current, mutation);
        let lease = StoreLease::acquire(&path).expect("fixture lease");
        let error = expect_refusal(
            super::super::CozoStore::open_existing_current(&path, lease),
            "raw catalog or semantic corruption must refuse",
        );
        assert!(
            error
                .to_string()
                .contains("episode_context_upgrade_required_or_invalid")
        );

        let raw = raw_fixture_store(&path);
        match mutation {
            FixtureMutation::NonemptyRetiredTagGuard => {
                let rows = raw
                    .run(
                        "?[id, tag] := *node_tag{id, tag} :limit 2",
                        BTreeMap::new(),
                        false,
                    )
                    .unwrap();
                assert_eq!(rows.rows.len(), 1, "retired guard was repaired");
            }
            FixtureMutation::WrongNodeAccess => assert_eq!(
                raw.relation_access_level("node").unwrap().as_deref(),
                Some("read_only"),
                "catalog access was repaired"
            ),
            _ => unreachable!(),
        }
        drop(raw);
        remove_fixture(&path);
    }
}

#[test]
fn mismatched_lease_refuses_before_source_admission() {
    let (path, _) = create_fixture(
        "mismatched-lease",
        4,
        FixtureGeneration::Current,
        FixtureMutation::None,
    );
    let sibling = fixture_path("sibling-lease");
    let lease = StoreLease::acquire(&sibling).expect("sibling lease");
    let before = fs::read(&path).unwrap();
    let error = expect_refusal(
        super::super::CozoStore::open_existing_current(&path, lease),
        "wrong lease must refuse",
    );
    assert!(error.to_string().contains("store_source_changed"));
    assert_eq!(fs::read(&path).unwrap(), before);
    remove_fixture(&path);
    remove_fixture(&sibling);
}

#[cfg(unix)]
#[test]
fn replaced_lease_inode_refuses_before_source_admission() {
    let (path, _) = create_fixture(
        "replaced-lease",
        4,
        FixtureGeneration::Current,
        FixtureMutation::None,
    );
    let lease = StoreLease::acquire(&path).expect("fixture lease");
    let lock = mneme_store_path::store_lock_path(&path).unwrap();
    fs::remove_file(&lock).unwrap();
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&lock)
        .unwrap();
    let error = expect_refusal(
        super::super::CozoStore::open_existing_current(&path, lease),
        "replaced lease inode must refuse",
    );
    assert!(error.to_string().contains("store_source_changed"));
    remove_fixture(&path);
}

#[test]
fn backend_activity_observer_does_not_retain_persistent_lease() {
    let (path, _) = create_fixture(
        "observer",
        4,
        FixtureGeneration::Current,
        FixtureMutation::None,
    );
    let lease = StoreLease::acquire(&path).expect("fixture lease");
    let store = super::super::CozoStore::open_existing_current(&path, lease).unwrap();
    let observer = store.backend_activity();
    drop(store);

    let replacement = StoreLease::acquire(&path)
        .expect("counter-only observer must not retain the persistent lease");
    assert_eq!(observer.in_flight(), 0);
    drop(replacement);
    drop(observer);
    remove_fixture(&path);
}

#[tokio::test]
async fn shared_lease_supports_sequential_current_opens_without_reacquisition() {
    let (path, _) = create_fixture(
        "shared-lease",
        4,
        FixtureGeneration::Current,
        FixtureMutation::None,
    );
    let lease = Arc::new(StoreLease::acquire(&path).expect("fixture lease"));

    let first = super::super::CozoStore::open_leased_current(&path, lease.clone())
        .expect("first admitted open");
    let expected_id = first.db_id();
    first
        .prepare_for_file_move()
        .expect("checkpoint first verification open");
    drop(first);
    assert_eq!(
        StoreLease::acquire(&path).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "the caller's shared authority must outlive a completed store open"
    );

    let second = super::super::CozoStore::open_leased_current(&path, lease.clone())
        .expect("second admitted open under the same lease");
    assert_eq!(second.db_id(), expected_id);
    let export = second.export().await.expect("export current fixture");
    assert_eq!(export.db_id, expected_id);
    second
        .prepare_for_file_move()
        .expect("checkpoint second verification open");
    drop(second);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        assert!(
            !PathBuf::from(sidecar).exists(),
            "sequential verification left SQLite sidecar {suffix:?}"
        );
    }
    drop(lease);

    drop(StoreLease::acquire(&path).expect("final shared authority released the lease"));
    remove_fixture(&path);
}

#[test]
fn failed_shared_lease_open_returns_authority_to_the_caller() {
    let (path, _) = create_fixture(
        "shared-lease-failure",
        4,
        FixtureGeneration::Current,
        FixtureMutation::RemoveMeta("dim"),
    );
    let lease = Arc::new(StoreLease::acquire(&path).expect("fixture lease"));

    let error = expect_refusal(
        super::super::CozoStore::open_leased_current(&path, lease.clone()),
        "invalid current artifact must refuse",
    );
    assert!(
        error
            .to_string()
            .contains("post-constructor current validation rejected vector dimension")
    );
    assert_eq!(Arc::strong_count(&lease), 1);
    assert_eq!(
        StoreLease::acquire(&path).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "failed open must not consume the caller's shared lease"
    );

    drop(lease);
    drop(StoreLease::acquire(&path).expect("caller can explicitly release authority"));
    remove_fixture(&path);
}

#[test]
fn detached_job_retains_database_then_lease_then_activity_visibility() {
    let (path, _) = create_fixture(
        "detached-job",
        4,
        FixtureGeneration::Current,
        FixtureMutation::None,
    );
    let lease = StoreLease::acquire(&path).expect("fixture lease");
    let store = super::super::CozoStore::open_existing_current(&path, lease).unwrap();
    let observer = store.backend_activity();
    let job = super::super::runtime::ActiveBackendJob {
        db: store.db.clone(),
        authority: store.persistent_authority.clone(),
        activity: observer.enter(),
    };
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        job.run(|_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
    });
    entered_rx.recv().unwrap();
    drop(store);

    assert_eq!(observer.in_flight(), 1);
    assert_eq!(
        StoreLease::acquire(&path).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    release_tx.send(()).unwrap();
    worker.join().unwrap();
    assert_eq!(observer.in_flight(), 0);
    let replacement = StoreLease::acquire(&path).expect(
        "job completion must release the database and lease before visibility reaches zero",
    );
    drop(replacement);
    remove_fixture(&path);
}

#[test]
fn feedback_epoch_activation_requires_admitted_authority_and_valid_identity() {
    let ephemeral = super::super::CozoStore::new(4).unwrap();
    assert!(matches!(
        ephemeral.activate_feedback_epoch("epoch-a"),
        Err(Error::Conflict(_))
    ));

    let (path, store) = admitted_fixture("feedback-activation-validation");
    assert!(matches!(
        store.activate_feedback_epoch(""),
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        store.activate_feedback_epoch(&"x".repeat(mneme_core::MAX_FEEDBACK_EPOCH_BYTES + 1)),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(store.activate_feedback_epoch("epoch-a").unwrap(), 0);
    drop(store);
    remove_fixture(&path);
}

#[test]
fn concurrent_feedback_epoch_activation_serializes_to_one_live_identity() {
    let (path, store) = admitted_fixture("feedback-activation-concurrent");
    let store = Arc::new(store);
    let barrier = Arc::new(Barrier::new(3));
    let spawn = |epoch: &'static str| {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            (epoch, store.activate_feedback_epoch(epoch))
        })
    };
    let first = spawn("epoch-a");
    let second = spawn("epoch-b");
    barrier.wait();
    let outcomes = [first.join().unwrap(), second.join().unwrap()];
    let winner = outcomes
        .iter()
        .find_map(|(epoch, result)| result.as_ref().ok().map(|_| *epoch))
        .expect("one activation must win");
    assert_eq!(
        outcomes.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    assert!(
        outcomes
            .iter()
            .any(|(_, result)| matches!(result, Err(Error::Conflict(_))))
    );
    assert_eq!(store.activate_feedback_epoch(winner).unwrap(), 0);

    drop(store);
    remove_fixture(&path);
}

#[test]
fn feedback_epoch_activation_purges_every_preexisting_retry_row() {
    let (path, store) = admitted_fixture("feedback-activation-census");
    let fingerprint = |byte: char| byte.to_string().repeat(64);

    put_feedback_proof(&store, "keep", &fingerprint('a'), 1);
    put_feedback_order(&store, "epoch-a", 1, "keep", true);

    put_feedback_proof(&store, "old", &fingerprint('b'), 2);
    put_feedback_order(&store, "epoch-old", 2, "old", true);
    put_feedback_proof(&store, "legacy", &fingerprint('c'), 0);
    put_feedback_order(&store, crate::LEGACY_FEEDBACK_EPOCH, 0, "legacy", true);
    put_feedback_proof(&store, "orphan-proof", &fingerprint('d'), 4);
    put_feedback_order(&store, "epoch-a", 5, "orphan-order", true);
    put_feedback_proof(&store, "mismatch", &fingerprint('e'), 6);
    put_feedback_order(&store, "epoch-a", 7, "mismatch", true);
    put_feedback_proof(&store, "duplicate", &fingerprint('f'), 8);
    put_feedback_order(&store, "epoch-a", 8, "duplicate", true);
    put_feedback_order(&store, "epoch-old", 8, "duplicate", true);
    put_feedback_proof(&store, "invalid-sequence", &fingerprint('1'), i64::MAX);
    put_feedback_order(&store, "epoch-a", i64::MAX, "invalid-sequence", true);
    put_feedback_proof(&store, "false-marker", &fingerprint('2'), 9);
    put_feedback_order(&store, "epoch-a", 9, "false-marker", false);
    put_feedback_proof(&store, "bad-fingerprint", "not-a-fingerprint", 10);
    put_feedback_order(&store, "epoch-a", 10, "bad-fingerprint", true);
    put_feedback_proof(&store, "oversized-fingerprint", &"a".repeat(512 * 1024), 11);
    put_feedback_order(&store, "epoch-a", 11, "oversized-fingerprint", true);

    assert_eq!(store.activate_feedback_epoch("epoch-a").unwrap(), 11);
    let expected = feedback_ledger_snapshot(&store);
    assert_eq!(expected, (Vec::new(), Vec::new()));

    assert_eq!(
        store.activate_feedback_epoch("epoch-a").unwrap(),
        0,
        "same-epoch activation is an idempotent in-memory no-op"
    );
    assert_eq!(feedback_ledger_snapshot(&store), expected);
    assert!(matches!(
        store.activate_feedback_epoch("epoch-b"),
        Err(Error::Conflict(_))
    ));
    assert_eq!(feedback_ledger_snapshot(&store), expected);

    drop(store);
    remove_fixture(&path);
}

#[tokio::test]
async fn admitted_feedback_requires_activation_and_the_handle_bound_epoch() {
    let (path, store) = admitted_fixture("feedback-activation-commit-gate");
    let epoch_a = feedback_commit("receipt", "payload-a", "epoch-a");
    let mut unscoped = feedback_commit("unused", "payload-unscoped", "unused-epoch");
    unscoped.idempotency = None;
    assert!(matches!(
        store.commit_feedback(&epoch_a).await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        store.commit_feedback(&unscoped).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(feedback_ledger_snapshot(&store), (Vec::new(), Vec::new()));

    assert_eq!(store.activate_feedback_epoch("epoch-a").unwrap(), 0);
    assert_eq!(
        store.commit_feedback(&unscoped).await.unwrap(),
        mneme_core::ports::FeedbackCommitOutcome::Applied
    );
    let epoch_b = feedback_commit("receipt", "payload-b", "epoch-b");
    assert!(matches!(
        store.commit_feedback(&epoch_b).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(feedback_ledger_snapshot(&store), (Vec::new(), Vec::new()));
    assert_eq!(
        store.commit_feedback(&epoch_a).await.unwrap(),
        mneme_core::ports::FeedbackCommitOutcome::Applied
    );
    assert_eq!(
        store.commit_feedback(&epoch_a).await.unwrap(),
        mneme_core::ports::FeedbackCommitOutcome::AlreadyApplied
    );

    drop(store);
    remove_fixture(&path);
}

#[test]
fn feedback_epoch_activation_overflow_refuses_without_mutation_or_authority() {
    let (path, store) = admitted_fixture("feedback-activation-overflow");
    put_feedback_order_range(
        &store,
        "old-epoch",
        mneme_core::MAX_FEEDBACK_RETRY_RECORDS + 1,
    );
    let before = feedback_ledger_snapshot(&store);
    assert_eq!(before.1.len(), mneme_core::MAX_FEEDBACK_RETRY_RECORDS + 1);
    assert!(matches!(
        store.activate_feedback_epoch("epoch-a"),
        Err(Error::CapacityExceeded { .. })
    ));
    assert_eq!(feedback_ledger_snapshot(&store), before);
    let active = store
        .persistent_authority
        .as_ref()
        .unwrap()
        .lock_feedback_epoch()
        .unwrap();
    assert!(active.is_none());
    drop(active);

    drop(store);
    remove_fixture(&path);
}

#[test]
fn feedback_epoch_activation_proof_and_union_overflow_are_also_atomic() {
    for (label, proof_count, add_disjoint_order) in [
        (
            "feedback-activation-proof-overflow",
            mneme_core::MAX_FEEDBACK_RETRY_RECORDS + 1,
            false,
        ),
        (
            "feedback-activation-union-overflow",
            mneme_core::MAX_FEEDBACK_RETRY_RECORDS,
            true,
        ),
    ] {
        let (path, store) = admitted_fixture(label);
        put_feedback_proof_range(&store, proof_count);
        if add_disjoint_order {
            put_feedback_order(&store, "old-epoch", 1, "order-only", true);
        }
        let before = feedback_ledger_snapshot(&store);
        assert!(matches!(
            store.activate_feedback_epoch("epoch-a"),
            Err(Error::CapacityExceeded { .. })
        ));
        assert_eq!(feedback_ledger_snapshot(&store), before);
        assert!(
            store
                .persistent_authority
                .as_ref()
                .unwrap()
                .lock_feedback_epoch()
                .unwrap()
                .is_none()
        );

        drop(store);
        remove_fixture(&path);
    }
}

#[test]
fn dropped_feedback_epoch_activation_transaction_restores_every_ledger_row() {
    let (path, store) = admitted_fixture("feedback-activation-rollback");
    put_feedback_proof(&store, "old", &"a".repeat(64), 1);
    put_feedback_order(&store, "old-epoch", 1, "old", true);
    let before = feedback_ledger_snapshot(&store);

    let tx = store.db.multi_transaction(true);
    assert_eq!(
        super::super::feedback::stage_feedback_epoch_activation(&tx, "epoch-a").unwrap(),
        1
    );
    drop(tx);
    assert_eq!(feedback_ledger_snapshot(&store), before);
    assert!(
        store
            .persistent_authority
            .as_ref()
            .unwrap()
            .lock_feedback_epoch()
            .unwrap()
            .is_none()
    );

    drop(store);
    remove_fixture(&path);
}
