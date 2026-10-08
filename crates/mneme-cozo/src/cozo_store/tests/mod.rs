use std::time::Duration;

use super::*;
use cozo::SQLITE_BUSY_TIMEOUT_MS;
use mneme_core::ports::{
    FeedbackEdgeUpdate, FeedbackIdempotency, FeedbackMergeObservation, FeedbackNodeUpdate,
    FeedbackRetryScope, TaggedAnnWorkLimits,
};
use mneme_core::{BodyRef, Provenance};

mod capture;
mod touchstones;

fn active_node(id: NodeId) -> Node {
    Node::try_new(
        id,
        "node",
        BodyRef::new("inline://x").unwrap(),
        std::iter::empty::<&str>(),
        Provenance::derived_empty(),
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap()
}

fn tagged_node(id: NodeId, tags: &[&str]) -> Node {
    Node::try_new(
        id,
        "tagged node",
        BodyRef::new("inline://tagged").unwrap(),
        tags.iter().copied(),
        Provenance::derived_empty(),
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap()
}

// Explicit predecessor fixtures. These helpers never stand in for a production
// opener: the old catalog is intentional input to legacy audit/upgrade tests.
fn legacy_store(dim: usize) -> Result<CozoStore> {
    let db = DbInstance::new("mem", "", "").map_err(backend)?;
    let store = fresh_current::store_over_staged_database(db, dim, Ulid::new());
    initialize_legacy_fixture(&store)?;
    Ok(store)
}

fn legacy_persistent_store(path: &str, dim: usize) -> Result<CozoStore> {
    install_lock_panic_filter();
    let store = raw_persistent_test_store(Path::new(path), dim);
    assert!(
        !store.has_schema()?,
        "legacy fixture construction requires an empty catalog"
    );
    initialize_legacy_fixture(&store)?;
    Ok(store)
}

fn initialize_legacy_fixture(store: &CozoStore) -> Result<()> {
    store.run(&schema(store.dim), BTreeMap::new(), true)?;
    store.put_meta("db_id", &store.db_id.to_string())?;
    store.put_meta("dim", &store.dim.to_string())?;
    store.put_meta(INCIDENT_EDGE_CAP_META_KEY, &MAX_INCIDENT_EDGES.to_string())?;
    store.put_meta(
        REMOTE_EDGE_SOURCE_CAP_META_KEY,
        &MAX_REMOTE_EDGES_PER_SOURCE.to_string(),
    )?;
    Ok(())
}

pub(super) fn reopen_current_test_store(path: &Path) -> Result<CozoStore> {
    // Match the positive-admission contract, including macOS /var -> /private/var.
    let path = std::fs::canonicalize(path).map_err(backend)?;
    let lease = mneme_store_path::StoreLease::acquire(&path)
        .map_err(|error| backend_str(error.to_string()))?;
    CozoStore::open_existing_current(&path, lease)
}

const TEST_MANAGED_STORE_META_SCHEMA: &str = ":create store_meta {storage_id: String => db_id: String, storage_generation: Int, managed_schema_version: Int, minimum_writer_schema: Int, writer_fence: String, mutation_epoch: Int}";

#[derive(Clone)]
struct TestManagedStoreMetaRow {
    storage_id: DataValue,
    db_id: DataValue,
    storage_generation: DataValue,
    managed_schema_version: DataValue,
    minimum_writer_schema: DataValue,
    writer_fence: DataValue,
    mutation_epoch: DataValue,
}

impl TestManagedStoreMetaRow {
    fn valid(store: &CozoStore) -> Self {
        Self {
            storage_id: dv_str(&Ulid::from(0xfeed_u128).to_string()),
            db_id: dv_str(&store.db_id.to_string()),
            storage_generation: dv_int(1),
            managed_schema_version: dv_int(i64::from(MANAGED_V1_SCHEMA_VERSION)),
            minimum_writer_schema: dv_int(i64::from(MANAGED_V1_MINIMUM_WRITER_SCHEMA)),
            writer_fence: dv_str(MANAGED_V1_WRITER_FENCE),
            mutation_epoch: dv_int(0),
        }
    }
}

fn create_test_managed_store_meta(store: &CozoStore) {
    store
        .run(TEST_MANAGED_STORE_META_SCHEMA, BTreeMap::new(), true)
        .unwrap();
}

fn put_test_managed_store_meta(store: &CozoStore, row: &TestManagedStoreMetaRow) {
    let mut params = BTreeMap::new();
    params.insert("storage_id".into(), row.storage_id.clone());
    params.insert("db_id".into(), row.db_id.clone());
    params.insert("storage_generation".into(), row.storage_generation.clone());
    params.insert(
        "managed_schema_version".into(),
        row.managed_schema_version.clone(),
    );
    params.insert(
        "minimum_writer_schema".into(),
        row.minimum_writer_schema.clone(),
    );
    params.insert("writer_fence".into(), row.writer_fence.clone());
    params.insert("mutation_epoch".into(), row.mutation_epoch.clone());
    store
            .run(
                "?[storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch] <- [[$storage_id, $db_id, $storage_generation, $managed_schema_version, $minimum_writer_schema, $writer_fence, $mutation_epoch]] :put store_meta {storage_id => db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch}",
                params,
                true,
            )
            .unwrap();
}

fn test_managed_v1_store_with_row() -> CozoStore {
    let store = legacy_store(4).unwrap();
    create_test_managed_store_meta(&store);
    put_test_managed_store_meta(&store, &TestManagedStoreMetaRow::valid(&store));
    store
        .put_meta(
            VECTOR_PROJECTION_META_KEY,
            MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
        )
        .unwrap();
    store
}

fn assert_managed_row_corrupt(
    case: &str,
    mutate: impl FnOnce(&CozoStore, &mut TestManagedStoreMetaRow),
) {
    let store = legacy_store(4).unwrap();
    create_test_managed_store_meta(&store);
    let mut row = TestManagedStoreMetaRow::valid(&store);
    mutate(&store, &mut row);
    put_test_managed_store_meta(&store, &row);
    store
        .put_meta(
            VECTOR_PROJECTION_META_KEY,
            MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
        )
        .unwrap();
    let error = store.storage_contract_generation().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("managed v1 storage contract is corrupt"),
        "{case} was not rejected as managed corruption: {error}"
    );
}

fn downgrade_contract_to_canonical_node_for_test(store: &CozoStore) {
    use crate::vector_projection::{GUARD_KEY, GUARD_VALUE, LEGACY_SHADOW_GUARD};

    store
        .put_meta(
            VECTOR_PROJECTION_META_KEY,
            CANONICAL_NODE_V1_VECTOR_PROJECTION_META_VALUE,
        )
        .unwrap();
    store.delete_meta(crate::tag_projection::META_KEY).unwrap();
    let mut params = BTreeMap::new();
    params.insert("guard_key".into(), dv_str(GUARD_KEY));
    params.insert("guard_value".into(), dv_str(GUARD_VALUE));
    store
        .run(
            &format!(
                "{{::access_level normal {LEGACY_SHADOW_GUARD}}}\n\
                     {{?[fence, generation] <- [[$guard_key, $guard_value]] \
                       :put {LEGACY_SHADOW_GUARD} {{fence => generation}}}}\n\
                     {{::access_level read_only {LEGACY_SHADOW_GUARD}}}"
            ),
            params,
            true,
        )
        .unwrap();
    store
        .run("::index drop node_tag_v2:by_id", BTreeMap::new(), true)
        .unwrap();
    store
        .run("::remove node_tag_v2", BTreeMap::new(), true)
        .unwrap();
    store
        .run("::access_level normal node_tag", BTreeMap::new(), true)
        .unwrap();
    assert_eq!(
        store.storage_contract_generation().unwrap(),
        StorageContractGeneration::CanonicalNodeV1
    );
}

fn conventional_audit_source_snapshot(store: &CozoStore) -> String {
    let mut parts = Vec::new();
    for script in [
        "::relations",
        "::indices node",
        "::indices meta",
        "::indices node_tag",
        "::indices node_vec",
        "?[k, v] := *meta{k, v} :order k",
        "?[id, data, status] := *node{id, data, status} :order id",
        "?[id, tag] := *node_tag{id, tag} :order id, tag",
        "?[id, e, status] := *node_vec{id, e, status} :order id",
    ] {
        parts.push(format!(
            "{script}\n{:?}",
            store.run(script, BTreeMap::new(), false).unwrap().rows
        ));
    }
    parts.join("\n")
}

fn physical_tags(store: &CozoStore, id: NodeId) -> Vec<(String, String, i64)> {
    let mut params = BTreeMap::new();
    params.insert("id".into(), dv_str(&id.0.to_string()));
    let rows = store
        .run(
            "?[tag, status, sample_hash] := \
                 *node_tag_v2:by_id{id, tag, status, sample_hash}, id == $id",
            params,
            false,
        )
        .unwrap();
    let mut tags = rows
        .rows
        .iter()
        .map(|row| {
            (
                want_str(&row[0]).unwrap().to_owned(),
                want_str(&row[1]).unwrap().to_owned(),
                want_i64(&row[2]).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    tags.sort();
    tags
}

fn test_managed_storage_identity() -> StorageIdentity {
    StorageIdentity::new(
        StorageId::new(Ulid::from(0xfeed_u128)).unwrap(),
        StorageGeneration::new(1).unwrap(),
    )
}

fn managed_delete_fixture(
    canonical: bool,
    anchor: bool,
    mutate_row: impl FnOnce(&CozoStore, &mut TestManagedStoreMetaRow),
    vector_sentinel: &str,
) -> (CozoStore, NodeId, NodeId) {
    let store = legacy_store(4).unwrap();
    let from = NodeId(Ulid::from(0x101_u128));
    let to = NodeId(Ulid::from(0x202_u128));
    let mut params = BTreeMap::new();
    params.insert("from".into(), dv_str(&from.0.to_string()));
    params.insert("to".into(), dv_str(&to.0.to_string()));
    if canonical {
        store
                .run(
                    "?[from, to, weight, kind, last_reinforced, trials, interference] <- [[$from, $to, 0.75, 'learned', 7, 2, 1]] :put edge {from, to => weight, kind, last_reinforced, trials, interference}",
                    params.clone(),
                    true,
                )
                .unwrap();
    }
    if anchor {
        store
                .run(
                    "?[from, to, start, end] <- [[$from, $to, 3, 9]] :put edge_anchor {from, to => start, end}",
                    params,
                    true,
                )
                .unwrap();
    }

    create_test_managed_store_meta(&store);
    let mut row = TestManagedStoreMetaRow::valid(&store);
    mutate_row(&store, &mut row);
    put_test_managed_store_meta(&store, &row);
    store
        .put_meta(VECTOR_PROJECTION_META_KEY, vector_sentinel)
        .unwrap();
    (store, from, to)
}

fn exact_managed_edge_snapshot(
    store: &CozoStore,
    from: NodeId,
    to: NodeId,
) -> (Vec<Vec<DataValue>>, Vec<Vec<DataValue>>, i64) {
    let mut params = BTreeMap::new();
    params.insert("from".into(), dv_str(&from.0.to_string()));
    params.insert("to".into(), dv_str(&to.0.to_string()));
    let edge = store
            .run(
                "?[from, to, weight, kind, last_reinforced, trials, interference] := *edge{from, to, weight, kind, last_reinforced, trials, interference}, from == $from, to == $to",
                params.clone(),
                false,
            )
            .unwrap()
            .rows;
    let anchor = store
            .run(
                "?[from, to, start, end] := *edge_anchor{from, to, start, end}, from == $from, to == $to",
                params,
                false,
            )
            .unwrap()
            .rows;
    let epoch = store
        .run(
            "?[mutation_epoch] := *store_meta{mutation_epoch} :limit 2",
            BTreeMap::new(),
            false,
        )
        .unwrap();
    assert_eq!(epoch.rows.len(), 1);
    assert_eq!(epoch.rows[0].len(), 1);
    let DataValue::Num(Num::Int(epoch)) = epoch.rows[0][0] else {
        panic!("fixture mutation_epoch was not an exact Int");
    };
    (edge, anchor, epoch)
}

#[test]
fn managed_v1_classifier_accepts_only_exact_metadata_and_maps_the_head() {
    let conventional = legacy_store(4).unwrap();
    assert_eq!(
        conventional.storage_contract_generation().unwrap(),
        StorageContractGeneration::ConventionalUnmanaged
    );

    let store = test_managed_v1_store_with_row();
    let StorageContractGeneration::ManagedV1(head) = store.storage_contract_generation().unwrap()
    else {
        panic!("exact managed v1 contract was not classified as managed");
    };
    assert_eq!(head.database_id().get(), store.db_id);
    let storage = head.storage().unwrap();
    assert_eq!(storage.id().get(), Ulid::from(0xfeed_u128));
    assert_eq!(storage.generation().get(), 1);
    assert_eq!(
        head.managed_schema_version().unwrap().get(),
        MANAGED_V1_SCHEMA_VERSION
    );
    assert_eq!(
        head.minimum_writer_schema().unwrap().get(),
        MANAGED_V1_MINIMUM_WRITER_SCHEMA
    );
    assert_eq!(head.mutation_epoch().unwrap().get(), 0);
}

#[test]
fn private_managed_edge_delete_counts_present_absent_and_orphan_anchor() {
    for (case, canonical, anchor, changed) in [
        ("canonical and anchor", true, true, true),
        ("canonical only", true, false, true),
        ("absent", false, false, false),
        ("orphan anchor", false, true, true),
    ] {
        let (store, from, to) = managed_delete_fixture(
            canonical,
            anchor,
            |_, _| {},
            MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
        );
        let outcome = managed_test_delete_edge(
            &store,
            test_managed_storage_identity(),
            from,
            to,
            ManagedTestFailure::None,
        )
        .unwrap_or_else(|error| panic!("{case} delete failed: {error}"));
        assert_eq!(outcome.changed, changed, "wrong decision for {case}");
        assert_eq!(
            outcome.epoch.get(),
            u64::from(changed),
            "wrong epoch for {case}"
        );
        let after = exact_managed_edge_snapshot(&store, from, to);
        assert!(after.0.is_empty(), "{case} retained the canonical edge");
        assert!(after.1.is_empty(), "{case} retained the edge anchor");
        assert_eq!(after.2, i64::from(changed), "{case} stored the wrong epoch");
    }
}

#[test]
fn private_managed_edge_delete_rejects_wrong_fence_identity_and_sentinel_unchanged() {
    type Mutate = fn(&CozoStore, &mut TestManagedStoreMetaRow);
    let cases: [(&str, Mutate, &str); 6] = [
        (
            "writer fence",
            |_, row| row.writer_fence = dv_str("wrong-fence"),
            MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
        ),
        (
            "storage identity",
            |_, row| row.storage_id = dv_str(&Ulid::from(0xbeef_u128).to_string()),
            MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
        ),
        (
            "database identity",
            |_, row| row.db_id = dv_str(&Ulid::from(0xdead_u128).to_string()),
            MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
        ),
        (
            "vector sentinel",
            |_, _| {},
            CONVENTIONAL_VECTOR_PROJECTION_META_VALUE,
        ),
        (
            "managed schema",
            |_, row| row.managed_schema_version = dv_int(2),
            MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
        ),
        (
            "writer schema",
            |_, row| row.minimum_writer_schema = dv_int(2),
            MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
        ),
    ];
    for (case, mutate, sentinel) in cases {
        let (store, from, to) = managed_delete_fixture(true, true, mutate, sentinel);
        let before = exact_managed_edge_snapshot(&store, from, to);
        let error = managed_test_delete_edge(
            &store,
            test_managed_storage_identity(),
            from,
            to,
            ManagedTestFailure::None,
        )
        .expect_err(case);
        assert!(
            error
                .to_string()
                .contains("managed v1 storage contract is corrupt"),
            "{case} failed for the wrong reason: {error}"
        );
        assert_eq!(
            exact_managed_edge_snapshot(&store, from, to),
            before,
            "{case} leaked a row, anchor, or epoch prefix"
        );
    }
}

#[test]
fn private_managed_edge_delete_checks_epoch_overflow_only_for_changes() {
    let max_epoch = |_: &CozoStore, row: &mut TestManagedStoreMetaRow| {
        row.mutation_epoch = dv_int(i64::MAX);
    };
    let (no_op, from, to) = managed_delete_fixture(
        false,
        false,
        max_epoch,
        MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
    );
    let outcome = managed_test_delete_edge(
        &no_op,
        test_managed_storage_identity(),
        from,
        to,
        ManagedTestFailure::None,
    )
    .unwrap();
    assert!(!outcome.changed);
    assert_eq!(outcome.epoch.get(), i64::MAX as u64);

    let (changed, from, to) = managed_delete_fixture(
        true,
        true,
        max_epoch,
        MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
    );
    let before = exact_managed_edge_snapshot(&changed, from, to);
    let error = managed_test_delete_edge(
        &changed,
        test_managed_storage_identity(),
        from,
        to,
        ManagedTestFailure::None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("no representable successor"));
    assert_eq!(exact_managed_edge_snapshot(&changed, from, to), before);
}

#[test]
fn private_managed_edge_delete_rolls_back_statement_and_commit_failures() {
    let (statement, from, to) = managed_delete_fixture(
        true,
        true,
        |_, _| {},
        MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
    );
    let before = exact_managed_edge_snapshot(&statement, from, to);
    let error = managed_test_delete_edge(
        &statement,
        test_managed_storage_identity(),
        from,
        to,
        ManagedTestFailure::AfterMutationStatement,
    )
    .unwrap_err();
    assert!(error.to_string().contains("assert"));
    assert_eq!(exact_managed_edge_snapshot(&statement, from, to), before);

    let (commit, from, to) = managed_delete_fixture(
        true,
        true,
        |_, _| {},
        MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
    );
    let before = exact_managed_edge_snapshot(&commit, from, to);
    commit.db.fail_next_commit_for_tests();
    let error = managed_test_delete_edge(
        &commit,
        test_managed_storage_identity(),
        from,
        to,
        ManagedTestFailure::None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("injected commit failure"));
    assert_eq!(exact_managed_edge_snapshot(&commit, from, to), before);
}

#[test]
fn managed_v1_marker_and_store_meta_torn_pairings_fail_closed() {
    let conventional_with_side_relation = legacy_store(4).unwrap();
    create_test_managed_store_meta(&conventional_with_side_relation);
    let error = conventional_with_side_relation
        .storage_contract_generation()
        .unwrap_err();
    assert!(error.to_string().contains("additive metadata alone"));

    let managed_without_side_relation = legacy_store(4).unwrap();
    managed_without_side_relation
        .put_meta(
            VECTOR_PROJECTION_META_KEY,
            MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
        )
        .unwrap();
    let error = managed_without_side_relation
        .storage_contract_generation()
        .unwrap_err();
    assert!(error.to_string().contains("publication is torn"));

    for missing_marker in [CANONICAL_NODE_META_KEY, crate::tag_projection::META_KEY] {
        let store = test_managed_v1_store_with_row();
        store.delete_meta(missing_marker).unwrap();
        let error = store.storage_contract_generation().unwrap_err();
        assert!(
            error.to_string().contains("companion markers"),
            "missing {missing_marker:?} was not rejected: {error}"
        );
    }

    let no_meta_relation = test_managed_v1_store_with_row();
    no_meta_relation
        .run("::remove meta", BTreeMap::new(), true)
        .unwrap();
    let error = no_meta_relation.storage_contract_generation().unwrap_err();
    assert!(error.to_string().contains("without the canonical meta"));
}

#[test]
fn managed_v1_store_meta_requires_exactly_one_row() {
    let empty = legacy_store(4).unwrap();
    create_test_managed_store_meta(&empty);
    empty
        .put_meta(
            VECTOR_PROJECTION_META_KEY,
            MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
        )
        .unwrap();
    let error = empty.storage_contract_generation().unwrap_err();
    assert!(error.to_string().contains("exactly one row, found 0"));

    let multiple = legacy_store(4).unwrap();
    create_test_managed_store_meta(&multiple);
    let first = TestManagedStoreMetaRow::valid(&multiple);
    put_test_managed_store_meta(&multiple, &first);
    let mut second = first.clone();
    second.storage_id = dv_str(&Ulid::from(0xbeef_u128).to_string());
    put_test_managed_store_meta(&multiple, &second);
    multiple
        .put_meta(
            VECTOR_PROJECTION_META_KEY,
            MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
        )
        .unwrap();
    let error = multiple.storage_contract_generation().unwrap_err();
    assert!(error.to_string().contains("exactly one row, found 2"));
}

#[test]
fn managed_v1_store_meta_value_corruption_matrix_fails_closed() {
    assert_managed_row_corrupt("malformed storage id", |_, row| {
        row.storage_id = dv_str("not-a-ulid");
    });
    assert_managed_row_corrupt("non-canonical storage id", |_, row| {
        row.storage_id = dv_str(&Ulid::from(0xfeed_u128).to_string().to_lowercase());
    });
    assert_managed_row_corrupt("nil storage id", |_, row| {
        row.storage_id = dv_str(&Ulid::from(0_u128).to_string());
    });
    assert_managed_row_corrupt("malformed database id", |_, row| {
        row.db_id = dv_str("not-a-ulid");
    });
    assert_managed_row_corrupt("nil database id", |_, row| {
        row.db_id = dv_str(&Ulid::from(0_u128).to_string());
    });
    assert_managed_row_corrupt("database id mismatch", |_, row| {
        row.db_id = dv_str(&Ulid::from(0xdead_u128).to_string());
    });
    assert_managed_row_corrupt("zero storage generation", |_, row| {
        row.storage_generation = dv_int(0);
    });
    assert_managed_row_corrupt("negative storage generation", |_, row| {
        row.storage_generation = dv_int(-1);
    });
    assert_managed_row_corrupt("zero managed schema", |_, row| {
        row.managed_schema_version = dv_int(0);
    });
    assert_managed_row_corrupt("future managed schema under managed v1", |_, row| {
        row.managed_schema_version = dv_int(2);
    });
    assert_managed_row_corrupt("managed schema outside u32", |_, row| {
        row.managed_schema_version = dv_int(i64::MAX);
    });
    assert_managed_row_corrupt("zero minimum writer schema", |_, row| {
        row.minimum_writer_schema = dv_int(0);
    });
    assert_managed_row_corrupt("future minimum writer schema under managed v1", |_, row| {
        row.minimum_writer_schema = dv_int(2);
    });
    assert_managed_row_corrupt("wrong writer fence", |_, row| {
        row.writer_fence = dv_str("trust-me-bro");
    });
    assert_managed_row_corrupt("negative mutation epoch", |_, row| {
        row.mutation_epoch = dv_int(-1);
    });
    assert_managed_row_corrupt("missing canonical database id", |store, _| {
        store.delete_meta("db_id").unwrap();
    });
    assert_managed_row_corrupt("non-canonical canonical database id", |store, _| {
        store
            .put_meta("db_id", &store.db_id.to_string().to_lowercase())
            .unwrap();
    });
}

#[test]
fn managed_contract_corruption_errors_bound_hostile_metadata_previews() {
    const HOSTILE_VALUE_BYTES: usize = 100_000;
    // Mnestic fences encoded keys at 64 KiB. A 50 KiB string remains a
    // hostile preview input while its memcomparable encoding stays inside
    // that independent storage boundary.
    const HOSTILE_KEY_BYTES: usize = 50_000;
    let hostile_value = "x".repeat(HOSTILE_VALUE_BYTES);
    let hostile_key = "x".repeat(HOSTILE_KEY_BYTES);
    let mut errors = Vec::new();

    let marker = legacy_store(4).unwrap();
    create_test_managed_store_meta(&marker);
    marker
        .put_meta(VECTOR_PROJECTION_META_KEY, &hostile_value)
        .unwrap();
    errors.push((
        "non-managed v1 marker",
        HOSTILE_VALUE_BYTES,
        marker.storage_contract_generation().unwrap_err(),
    ));

    for (case, hostile, hostile_bytes, mutate) in [
        (
            "ULID",
            hostile_key.as_str(),
            HOSTILE_KEY_BYTES,
            (|row: &mut TestManagedStoreMetaRow, value: &str| {
                row.storage_id = dv_str(value);
            }) as fn(&mut TestManagedStoreMetaRow, &str),
        ),
        (
            "writer fence",
            hostile_value.as_str(),
            HOSTILE_VALUE_BYTES,
            (|row: &mut TestManagedStoreMetaRow, value: &str| {
                row.writer_fence = dv_str(value);
            }) as fn(&mut TestManagedStoreMetaRow, &str),
        ),
    ] {
        let store = legacy_store(4).unwrap();
        create_test_managed_store_meta(&store);
        let mut row = TestManagedStoreMetaRow::valid(&store);
        mutate(&mut row, hostile);
        put_test_managed_store_meta(&store, &row);
        store
            .put_meta(
                VECTOR_PROJECTION_META_KEY,
                MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
            )
            .unwrap();
        errors.push((
            case,
            hostile_bytes,
            store.storage_contract_generation().unwrap_err(),
        ));
    }

    for (case, hostile_bytes, error) in errors {
        let rendered = error.to_string();
        assert!(
            rendered.len() < 1_024,
            "{case} error reflected hostile metadata: {} bytes",
            rendered.len()
        );
        assert!(
            rendered.contains(&format!("{hostile_bytes} bytes")),
            "{case} error omitted the bounded length diagnostic: {rendered}"
        );
        assert!(rendered.contains("(truncated)"));
    }
}

#[test]
fn managed_v1_store_meta_catalog_corruption_matrix_fails_closed() {
    for (case, schema) in [
        (
            "extra column",
            ":create store_meta {storage_id: String => db_id: String, storage_generation: Int, managed_schema_version: Int, minimum_writer_schema: Int, writer_fence: String, mutation_epoch: Int, extra: String}",
        ),
        (
            "wrong key set",
            ":create store_meta {storage_id: String, db_id: String => storage_generation: Int, managed_schema_version: Int, minimum_writer_schema: Int, writer_fence: String, mutation_epoch: Int}",
        ),
        (
            "wrong epoch type",
            ":create store_meta {storage_id: String => db_id: String, storage_generation: Int, managed_schema_version: Int, minimum_writer_schema: Int, writer_fence: String, mutation_epoch: Float}",
        ),
    ] {
        let store = legacy_store(4).unwrap();
        store.run(schema, BTreeMap::new(), true).unwrap();
        store
            .put_meta(
                VECTOR_PROJECTION_META_KEY,
                MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
            )
            .unwrap();
        let error = store.storage_contract_generation().unwrap_err();
        assert!(
            error.to_string().contains("unexpected schema"),
            "{case} was not rejected: {error}"
        );
    }

    for (case, mutation) in [
        ("read-only access", "::access_level read_only store_meta"),
        ("child index", "::index create store_meta:by_db {db_id}"),
        (
            "trigger",
            "::set_triggers store_meta on put { ?[storage_id] := _new[storage_id] }",
        ),
    ] {
        let store = test_managed_v1_store_with_row();
        store.run(mutation, BTreeMap::new(), true).unwrap();
        let error = store.storage_contract_generation().unwrap_err();
        assert!(
            error.to_string().contains("unexpected schema"),
            "{case} was not rejected: {error}"
        );
    }
}

#[test]
fn managed_store_meta_shape_rejects_catalog_defaults_and_header_drift() {
    let store = test_managed_v1_store_with_row();
    let exact = store.relation_columns(MANAGED_STORE_META_RELATION).unwrap();
    assert!(managed_store_meta_columns_are_exact(&exact));

    let mut defaulted = exact.clone();
    defaulted.rows[6][4] = DataValue::Bool(true);
    defaulted.rows[6][5] = dv_str("0");
    assert!(!managed_store_meta_columns_are_exact(&defaulted));

    let mut wrong_headers = exact.clone();
    wrong_headers.headers[0] = "name".into();
    assert!(!managed_store_meta_columns_are_exact(&wrong_headers));

    let mut extra_field = exact;
    extra_field.rows[0].push(DataValue::Null);
    assert!(!managed_store_meta_columns_are_exact(&extra_field));
}

#[test]
fn managed_v1_generation_gate_refuses_without_logical_changes() {
    let store = test_managed_v1_store_with_row();
    let snapshot = |store: &CozoStore| {
        [
                "::relations",
                "?[k, v] := *meta{k, v} :order k",
                "?[storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch] := *store_meta{storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch} :order storage_id",
            ]
            .into_iter()
            .map(|script| format!("{:?}", store.run(script, BTreeMap::new(), false).unwrap()))
            .collect::<Vec<_>>()
    };
    let before = snapshot(&store);
    let error = store.ensure_vector_projection_ready().unwrap_err();
    assert!(error.to_string().contains("production entry refuses"));
    assert_eq!(snapshot(&store), before);
}

#[test]
fn persistent_managed_v1_reopen_refuses_before_logical_repairs() {
    let path = std::env::temp_dir().join(format!("mneme-managed-v1-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let store = legacy_persistent_store(p, 4).unwrap();
    create_test_managed_store_meta(&store);
    put_test_managed_store_meta(&store, &TestManagedStoreMetaRow::valid(&store));
    store
        .put_meta(
            VECTOR_PROJECTION_META_KEY,
            MANAGED_V1_VECTOR_PROJECTION_META_VALUE,
        )
        .unwrap();

    // These are deliberate witnesses for the mutations ordinary legacy
    // open would perform after classification. Managed v1 must refuse before it
    // recreates an additive relation or stamps missing invariants.
    store
        .run("::remove feedback_retry_order", BTreeMap::new(), true)
        .unwrap();
    store.delete_meta("dim").unwrap();
    store.delete_meta(INCIDENT_EDGE_CAP_META_KEY).unwrap();
    store.delete_meta(REMOTE_EDGE_SOURCE_CAP_META_KEY).unwrap();
    drop(store);

    let logical_snapshot = |store: &CozoStore| {
        [
                "::relations",
                "?[k, v] := *meta{k, v} :order k",
                "?[storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch] := *store_meta{storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch} :order storage_id",
                "?[epoch, sequence, key, marker] := *feedback_retry_order{epoch, sequence, key, marker} :order epoch, sequence, key",
            ]
            .into_iter()
            .map(|script| format!("{script}\n{:?}", store.run(script, BTreeMap::new(), false)))
            .collect::<Vec<_>>()
    };

    let raw = raw_persistent_test_store(&path, 4);
    let before = logical_snapshot(&raw);
    drop(raw);

    let error = match CozoStore::open(p, 4) {
        Ok(_) => panic!("normal persistent open admitted managed v1"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("leased current-generation opener")
    );

    let raw = raw_persistent_test_store(&path, 4);
    assert_eq!(logical_snapshot(&raw), before);
    drop(raw);
    remove_sqlite_test_files(&path);
}

#[test]
fn persistent_open_refuses_invalid_database_ids_before_additive_repairs() {
    for (case, db_id, expected) in [
        (
            "non-canonical",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_ascii_lowercase(),
            "is not canonical uppercase ULID text",
        ),
        ("nil", Ulid::from(0_u128).to_string(), "must be non-nil"),
    ] {
        let path = std::env::temp_dir().join(format!(
            "mneme-invalid-open-db-id-{case}-{}.db",
            Ulid::new()
        ));
        let p = path.to_str().unwrap();
        let fresh = CozoStore::open(p, 4).unwrap();
        fresh.prepare_for_file_move().unwrap();
        drop(fresh);

        let raw = raw_persistent_test_store(&path, 4);
        raw.put_meta("db_id", &db_id).unwrap();
        raw.run("::remove feedback_retry_order", BTreeMap::new(), true)
            .unwrap();
        let before = conventional_audit_source_snapshot(&raw);
        raw.prepare_for_file_move().unwrap();
        drop(raw);

        let error = match reopen_current_test_store(&path) {
            Ok(_) => panic!("persistent open blessed a {case} database id"),
            Err(error) => error,
        };
        assert!(!error.to_string().is_empty(), "{case}: {expected}");

        let raw = raw_persistent_test_store(&path, 4);
        assert_eq!(conventional_audit_source_snapshot(&raw), before);
        assert!(
            !raw.relation_exists("feedback_retry_order").unwrap(),
            "{case} database id failure leaked an additive repair"
        );
        raw.prepare_for_file_move().unwrap();
        drop(raw);
        remove_sqlite_test_files(&path);
    }
}

#[test]
fn persistent_open_refuses_missing_database_id_without_repair() {
    let path = std::env::temp_dir().join(format!("mneme-absent-open-db-id-{}.db", Ulid::new()));
    let fresh = CozoStore::open(path.to_str().unwrap(), 4).unwrap();
    fresh.prepare_for_file_move().unwrap();
    drop(fresh);
    let raw = raw_persistent_test_store(&path, 4);
    raw.delete_meta("db_id").unwrap();
    let before = conventional_audit_source_snapshot(&raw);
    raw.prepare_for_file_move().unwrap();
    drop(raw);
    assert!(reopen_current_test_store(&path).is_err());
    let raw = raw_persistent_test_store(&path, 4);
    assert!(raw.read_meta("db_id").unwrap().is_none());
    assert_eq!(conventional_audit_source_snapshot(&raw), before);
    raw.prepare_for_file_move().unwrap();
    drop(raw);
    remove_sqlite_test_files(&path);
}

#[test]
fn persistent_store_meta_only_catalog_is_not_misclassified_as_fresh() {
    let path = std::env::temp_dir().join(format!("mneme-store-meta-only-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let raw = raw_persistent_test_store(&path, 4);
    create_test_managed_store_meta(&raw);
    put_test_managed_store_meta(&raw, &TestManagedStoreMetaRow::valid(&raw));
    let before_catalog = raw.run("::relations", BTreeMap::new(), false).unwrap().rows;
    let before_rows = raw
            .run(
                "?[storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch] := *store_meta{storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch} :order storage_id",
                BTreeMap::new(),
                false,
            )
            .unwrap()
            .rows;
    drop(raw);

    let error = match CozoStore::open(p, 4) {
        Ok(_) => panic!("store_meta-only catalog was initialized as fresh conventional"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("leased current-generation opener")
    );

    let raw = raw_persistent_test_store(&path, 4);
    assert_eq!(
        raw.run("::relations", BTreeMap::new(), false).unwrap().rows,
        before_catalog
    );
    assert_eq!(
            raw.run(
                "?[storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch] := *store_meta{storage_id, db_id, storage_generation, managed_schema_version, minimum_writer_schema, writer_fence, mutation_epoch} :order storage_id",
                BTreeMap::new(),
                false,
            )
            .unwrap()
            .rows,
            before_rows
        );
    drop(raw);
    remove_sqlite_test_files(&path);
}

#[test]
fn persistent_partial_mneme_catalog_is_not_misclassified_as_fresh() {
    let path = std::env::temp_dir().join(format!("mneme-partial-catalog-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let raw = raw_persistent_test_store(&path, 4);
    raw.run(
        ":create feedback_retry_order {epoch: String, sequence: Int, key: String => marker: Bool}",
        BTreeMap::new(),
        true,
    )
    .unwrap();
    raw.run(
            "?[epoch, sequence, key, marker] <- [['old-authority', 7, 'proof', true]] :put feedback_retry_order {epoch, sequence, key => marker}",
            BTreeMap::new(),
            true,
        )
        .unwrap();
    let before_catalog = raw.run("::relations", BTreeMap::new(), false).unwrap().rows;
    let before_rows = raw
            .run(
                "?[epoch, sequence, key, marker] := *feedback_retry_order{epoch, sequence, key, marker} :order epoch, sequence, key",
                BTreeMap::new(),
                false,
            )
            .unwrap()
            .rows;
    drop(raw);

    let error = match CozoStore::open(p, 4) {
        Ok(_) => panic!("partial Mneme catalog was initialized as fresh conventional"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("leased current-generation opener")
    );

    let raw = raw_persistent_test_store(&path, 4);
    assert_eq!(
        raw.run("::relations", BTreeMap::new(), false).unwrap().rows,
        before_catalog
    );
    assert_eq!(
            raw.run(
                "?[epoch, sequence, key, marker] := *feedback_retry_order{epoch, sequence, key, marker} :order epoch, sequence, key",
                BTreeMap::new(),
                false,
            )
            .unwrap()
            .rows,
            before_rows
        );
    drop(raw);
    remove_sqlite_test_files(&path);
}

#[test]
fn fresh_store_is_single_graph_with_episode_facets_and_no_candidate_partitions() {
    let store = CozoStore::new(4).unwrap();
    assert_eq!(
        store
            .read_meta(VECTOR_PROJECTION_META_KEY)
            .unwrap()
            .as_deref(),
        Some(TOUCHSTONES_V1_CATALOG_GENERATION_MARKER)
    );
    for relation in [
        "episode_head",
        "episode_history",
        "episode_time",
        "episode_search",
    ] {
        assert!(store.relation_exists(relation).unwrap());
    }
    for relation in ["node_vec", "node_search"] {
        let rows = store
            .run(&format!("::indices {relation}"), BTreeMap::new(), false)
            .unwrap();
        let catalog = format!("{:?}", rows.rows);
        assert!(!catalog.contains("candidate_idx"), "{catalog}");
        assert!(!catalog.contains("candidate_fts"), "{catalog}");
    }
}

#[tokio::test]
async fn conventional_fresh_projection_replaces_tags_moves_lifecycle_and_clears_delete() {
    let store = CozoStore::new(4).unwrap();
    let id = NodeId(Ulid::from(79u128));
    let hash = stable_tag_sample_hash(id);
    store.put_node(&tagged_node(id, &["a", "b"])).await.unwrap();
    assert_eq!(
        physical_tags(&store, id),
        vec![
            ("a".into(), "active".into(), hash),
            ("b".into(), "active".into(), hash)
        ]
    );

    store.put_node(&tagged_node(id, &["b", "c"])).await.unwrap();
    assert_eq!(
        physical_tags(&store, id),
        vec![
            ("b".into(), "active".into(), hash),
            ("c".into(), "active".into(), hash)
        ]
    );
    let mut archived = tagged_node(id, &["b", "c"]);
    archived.set_status(NodeStatus::Archived);
    store.put_node(&archived).await.unwrap();
    assert_eq!(
        physical_tags(&store, id),
        vec![
            ("b".into(), "archived".into(), hash),
            ("c".into(), "archived".into(), hash),
        ]
    );

    store.put_node(&tagged_node(id, &[])).await.unwrap();
    assert!(physical_tags(&store, id).is_empty());
    store
        .put_node(&tagged_node(id, &["delete-me"]))
        .await
        .unwrap();
    store.delete_node(id).await.unwrap();
    assert!(physical_tags(&store, id).is_empty());
}

#[test]
fn conventional_empty_tag_sync_batch_emits_zero_statements() {
    let store = CozoStore::new(4).unwrap();
    let tx = store.db.multi_transaction(true);
    assert_eq!(tx_sync_tag_projection(&tx, &[]).unwrap(), 0);
    tx.abort().unwrap();
}

#[test]
fn canonical_node_source_audit_is_strictly_mutation_free() {
    let store = legacy_store(4).unwrap();
    downgrade_contract_to_canonical_node_for_test(&store);
    let before = conventional_audit_source_snapshot(&store);
    let audit = store
        .audit_conventional_upgrade_source(StorageContractGeneration::CanonicalNodeV1)
        .unwrap();
    assert_eq!((audit.nodes, audit.vectors, audit.memberships), (0, 0, 0));
    assert_eq!(conventional_audit_source_snapshot(&store), before);
}

#[tokio::test]
async fn failed_conventional_canonical_audit_leaves_source_and_catalog_unchanged() {
    let store = legacy_store(4).unwrap();
    let id = NodeId(Ulid::from(77u128));
    let node = tagged_node(id, &["valid"]);
    store.put_node(&node).await.unwrap();
    downgrade_contract_to_canonical_node_for_test(&store);
    let mut params = BTreeMap::new();
    params.insert("id".into(), dv_str(&id.0.to_string()));
    params.insert("data".into(), dv_str("{}"));
    store
        .run(
            "?[id, data, status] <- [[$id, $data, 'active']] :put node {id => data, status}",
            params,
            true,
        )
        .unwrap();
    let before = conventional_audit_source_snapshot(&store);
    assert!(
        store
            .audit_conventional_upgrade_source(StorageContractGeneration::CanonicalNodeV1)
            .is_err()
    );
    assert_eq!(conventional_audit_source_snapshot(&store), before);
}

#[test]
fn conventional_tag_order_normalization_preserves_omitted_serde_defaults() {
    let id = NodeId(Ulid::from(78u128));
    let node = tagged_node(id, &["a", "b"]);
    let mut value = serde_json::to_value(&node).unwrap();
    let object = value.as_object_mut().unwrap();
    assert!(object.remove("body_ownership").is_some());
    object.insert("tags".into(), serde_json::json!(["b", "a"]));
    let raw = serde_json::to_string(&value).unwrap();
    let row = vec![dv_str(&id.0.to_string()), dv_str(&raw), dv_str("active")];
    let audited = decode_upgrade_canonical_row(&row).unwrap();
    let normalized = audited.normalized.expect("tag ordering needs rewrite");
    let normalized_value: serde_json::Value = serde_json::from_str(&normalized).unwrap();
    assert!(normalized_value.get("body_ownership").is_none());
    assert_eq!(normalized_value["tags"], serde_json::json!(["a", "b"]));
}

fn remove_sqlite_test_files(path: &Path) {
    let base = path.as_os_str().to_string_lossy();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{base}{suffix}"));
    }
}

// Raw backend fixture access only: corruption/setup and deliberate competing
// SQLite handles. This bypass is not evidence of supported frontend admission.
fn raw_persistent_test_store(path: &Path, dim: usize) -> CozoStore {
    CozoStore {
        db: DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap(),
        persistent_authority: None,
        dim,
        db_id: Ulid::new(),
        backend_activity: BackendActivity::default(),
        tagged_read_admission: TaggedReadAdmission::default(),
        tagged_read_test_hook: Arc::new(TaggedReadTestHook::default()),
        query_count: AtomicUsize::new(0),
        last_maintenance_statements: AtomicUsize::new(0),
    }
}

#[test]
fn sqlite_lock_retry_wait_ceiling_stays_under_five_seconds() {
    assert_eq!(SQLITE_BUSY_TIMEOUT_MS, 250);
    assert_eq!(lock_retry_backoff_ceiling_ms(), 1_515);
    assert_eq!(LOCK_RETRY_WAIT_CEILING_MS, 4_765);
}

fn remove_vector_guard_for_test(store: &CozoStore) {
    use crate::vector_projection::LEGACY_SHADOW_GUARD;

    if store.relation_exists(LEGACY_SHADOW_GUARD).unwrap() {
        store
            .run(
                &format!("::access_level normal {LEGACY_SHADOW_GUARD}"),
                BTreeMap::new(),
                true,
            )
            .unwrap();
        store
            .run(
                &format!("::remove {LEGACY_SHADOW_GUARD}"),
                BTreeMap::new(),
                true,
            )
            .unwrap();
    }
}

#[test]
fn conventional_old_writer_fence_rejects_put_remove_and_relation_drop() {
    use crate::storage_contract::conventional_unmanaged::spec::PERMANENT_VECTOR_GUARD_VALUE;
    use crate::vector_projection::{GUARD_KEY, LEGACY_SHADOW_GUARD};

    let store = CozoStore::new(4).unwrap();
    let attacks = [
        format!(
            "?[fence, generation] <- [['new-fence', 'new-generation']] :put {LEGACY_SHADOW_GUARD} {{fence => generation}}"
        ),
        format!(
            "?[fence, generation] <- [['{GUARD_KEY}', 'overwritten']] :put {LEGACY_SHADOW_GUARD} {{fence => generation}}"
        ),
        format!(
            "?[fence] := *{LEGACY_SHADOW_GUARD}{{fence}}, fence == '{GUARD_KEY}' :rm {LEGACY_SHADOW_GUARD} {{fence}}"
        ),
        format!("::remove {LEGACY_SHADOW_GUARD}"),
    ];
    for attack in attacks {
        assert!(
            store.run(&attack, BTreeMap::new(), true).is_err(),
            "read-only fence unexpectedly admitted {attack:?}"
        );
        store.ensure_legacy_shadow_guard_ready().unwrap();
    }
    let sentinel = store
        .run(
            &format!("?[generation] := *{LEGACY_SHADOW_GUARD}{{fence: '{GUARD_KEY}', generation}}"),
            BTreeMap::new(),
            false,
        )
        .unwrap();
    assert_eq!(
        sentinel.rows,
        vec![vec![dv_str(PERMANENT_VECTOR_GUARD_VALUE)]]
    );
}

#[test]
fn fresh_persistent_schema_and_conventional_markers_are_one_retryable_transaction() {
    let path = std::env::temp_dir().join(format!("mneme-conventional-schema-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let db = DbInstance::new("sqlite", p, "").unwrap();
    let error = run_db(&db, &schema_with_marker_failure(4), BTreeMap::new(), true)
        .expect_err("forced post-marker failure must abort the full schema install");
    assert!(error.to_string().contains("assert"));
    let relations = run_db(&db, "::relations", BTreeMap::new(), false).unwrap();
    assert!(
        relations.rows.is_empty(),
        "failed marker publication leaked a partial schema: {:?}",
        relations.rows
    );
    drop(db);

    let store = legacy_persistent_store(p, 4).unwrap();
    assert_eq!(
        store.storage_contract_generation().unwrap(),
        StorageContractGeneration::ConventionalUnmanaged
    );
    store.ensure_legacy_shadow_guard_ready().unwrap();
    drop(store);
    remove_sqlite_test_files(&path);
}

#[test]
fn conventional_source_audit_rejects_node_and_meta_child_indexes_without_writing() {
    for (relation, column) in [("node", "status"), ("meta", "v")] {
        let store = legacy_store(4).unwrap();
        downgrade_contract_to_canonical_node_for_test(&store);
        store
            .run(
                &format!("::index create {relation}:unexpected {{ {column} }}"),
                BTreeMap::new(),
                true,
            )
            .unwrap();
        let before = conventional_audit_source_snapshot(&store);
        let error = store
            .audit_conventional_upgrade_source(StorageContractGeneration::CanonicalNodeV1)
            .unwrap_err();
        assert!(
            error.to_string().contains("child index"),
            "unexpected {relation} catalog error: {error}"
        );
        assert_eq!(conventional_audit_source_snapshot(&store), before);
    }
}

#[test]
fn vector_upgrade_scratch_rejects_partial_child_sets() {
    use crate::tag_projection::SHADOW_NODE_VEC;

    let store = legacy_store(4).unwrap();
    downgrade_contract_to_canonical_node_for_test(&store);
    store
        .run(
            &create_vector_relation_script(SHADOW_NODE_VEC, 4),
            BTreeMap::new(),
            true,
        )
        .unwrap();
    store
            .run(
                &format!(
                    "::hnsw create {SHADOW_NODE_VEC}:{ACTIVE_VECTOR_INDEX} \
                     {{dim: 4, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'active'}}"
                ),
                BTreeMap::new(),
                true,
            )
            .unwrap();
    let error = store
        .audit_conventional_upgrade_source(StorageContractGeneration::CanonicalNodeV1)
        .unwrap_err();
    assert!(error.to_string().contains("partial or unexpected"));
    assert!(store.relation_exists(SHADOW_NODE_VEC).unwrap());
}

#[test]
fn vector_upgrade_scratch_rejects_legacy_or_wrong_dimension_schemas() {
    use crate::tag_projection::SHADOW_NODE_VEC;

    for create in [
        format!(":create {SHADOW_NODE_VEC} {{id: String => e: <F32; 4>}}"),
        format!(":create {SHADOW_NODE_VEC} {{id: String => e: <F32; 5>, status: String}}"),
    ] {
        let store = legacy_store(4).unwrap();
        downgrade_contract_to_canonical_node_for_test(&store);
        store.run(&create, BTreeMap::new(), true).unwrap();
        let error = store
            .audit_conventional_upgrade_source(StorageContractGeneration::CanonicalNodeV1)
            .unwrap_err();
        assert!(
            error.to_string().contains("unexpected conventional schema"),
            "impossible conventional scratch shape was misclassified: {error}"
        );
        assert!(
            store.relation_exists(SHADOW_NODE_VEC).unwrap(),
            "audit must not delete an untrusted reserved-name relation"
        );
    }
}

#[test]
fn pre_v3_named_lifecycle_hnsws_still_require_a_vector_shadow() {
    let store = legacy_store(4).unwrap();
    downgrade_contract_to_canonical_node_for_test(&store);
    store.delete_meta(VECTOR_PROJECTION_META_KEY).unwrap();
    store.delete_meta(CANONICAL_NODE_META_KEY).unwrap();
    remove_vector_guard_for_test(&store);
    assert_eq!(
        store.storage_contract_generation().unwrap(),
        StorageContractGeneration::LegacyVector
    );
    let audit = store
        .audit_conventional_upgrade_source(StorageContractGeneration::LegacyVector)
        .unwrap();
    assert!(audit.source_vector_has_status);
    assert!(
        audit.vector_shadow_required,
        "catalog names cannot authenticate the hidden HNSW filters"
    );
}

#[test]
fn fixed_name_vector_artifact_with_triggers_is_never_destroyed() {
    let store = legacy_store(4).unwrap();
    downgrade_contract_to_canonical_node_for_test(&store);
    store.delete_meta(VECTOR_PROJECTION_META_KEY).unwrap();
    store.delete_meta(CANONICAL_NODE_META_KEY).unwrap();
    remove_vector_guard_for_test(&store);
    let relation = crate::vector_projection::LEGACY_SHADOW_GUARD;
    store
        .run(
            &create_vector_relation_script(relation, 4),
            BTreeMap::new(),
            true,
        )
        .unwrap();
    store
        .run(
            &format!(
                "::set_triggers {relation} on put {{ ?[id, e, status] := _new[id, e, status] }}"
            ),
            BTreeMap::new(),
            true,
        )
        .unwrap();

    let error = store
        .audit_conventional_upgrade_source(StorageContractGeneration::LegacyVector)
        .unwrap_err();
    assert!(error.to_string().contains("unrecognized shape"));
    assert!(store.relation_exists(relation).unwrap());
    assert_eq!(
        store
            .run(
                &format!("::show_triggers {relation}"),
                BTreeMap::new(),
                false,
            )
            .unwrap()
            .rows
            .len(),
        1,
        "failed audit destroyed or rewrote the foreign trigger"
    );
}

#[test]
fn invalid_db_ids_fail_before_early_fence_or_shadow_work() {
    let lowercase = "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_ascii_lowercase();
    let nil = Ulid::from(0_u128).to_string();
    let hostile = "x".repeat(8 * 1024);
    for (case, db_id, expected) in [
        (
            "malformed",
            "definitely-not-a-ulid".to_owned(),
            "source db_id is not a valid ULID",
        ),
        (
            "non-canonical",
            lowercase,
            "source db_id is not canonical uppercase ULID text",
        ),
        ("nil", nil, "source db_id must be non-nil"),
        (
            "hostile malformed",
            hostile,
            "source db_id is not a valid ULID",
        ),
    ] {
        let store = legacy_store(4).unwrap();
        downgrade_contract_to_canonical_node_for_test(&store);
        store.delete_meta(VECTOR_PROJECTION_META_KEY).unwrap();
        store.delete_meta(CANONICAL_NODE_META_KEY).unwrap();
        remove_vector_guard_for_test(&store);
        store.put_meta("db_id", &db_id).unwrap();
        let before = conventional_audit_source_snapshot(&store);

        let error = store
            .audit_conventional_upgrade_source(StorageContractGeneration::LegacyVector)
            .unwrap_err();
        let rendered = error.to_string();
        assert!(
            rendered.contains(expected),
            "{case} db_id failed for the wrong reason: {rendered}"
        );
        assert!(
            rendered.len() < 256,
            "{case} db_id produced an unbounded error: {} bytes",
            rendered.len()
        );
        assert_eq!(conventional_audit_source_snapshot(&store), before);
        for relation in [
            crate::vector_projection::LEGACY_SHADOW_GUARD,
            crate::tag_projection::SHADOW_NODE_TAG,
            crate::tag_projection::SHADOW_LEGACY_GUARD,
            crate::tag_projection::SHADOW_NODE,
            crate::tag_projection::SHADOW_NODE_VEC,
            crate::tag_projection::SHADOW_META,
        ] {
            assert!(
                !store.relation_exists(relation).unwrap(),
                "{case} db_id audit created migration artifact {relation:?}"
            );
        }
    }
}

#[test]
fn legacy_readonly_verification_refuses_invalid_database_ids() {
    for (case, db_id, expected) in [
        (
            "non-canonical",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_ascii_lowercase(),
            "is not canonical uppercase ULID text",
        ),
        ("nil", Ulid::from(0_u128).to_string(), "must be non-nil"),
    ] {
        let store = legacy_store(4).unwrap();
        store.put_meta("db_id", &db_id).unwrap();
        let before = conventional_audit_source_snapshot(&store);

        let verify_error = store
            .verify_current_conventional_generation(false)
            .unwrap_err();
        assert!(
            verify_error.to_string().contains(expected),
            "{case} current db_id failed verification for the wrong reason: {verify_error}"
        );
        assert_eq!(conventional_audit_source_snapshot(&store), before);
    }
}

#[test]
fn legacy_readonly_database_id_verification_does_not_repair_absence() {
    let canonical = legacy_store(4).unwrap();
    let canonical_text = canonical.read_meta("db_id").unwrap().unwrap();
    parse_canonical_database_id(&canonical_text).unwrap();
    assert_eq!(
        canonical
            .verify_current_conventional_generation(false)
            .unwrap(),
        (0, 0, 0)
    );
    assert_eq!(
        canonical.read_meta("db_id").unwrap().as_deref(),
        Some(canonical_text.as_str())
    );

    let absent = legacy_store(4).unwrap();
    absent.delete_meta("db_id").unwrap();
    let before = conventional_audit_source_snapshot(&absent);
    assert_eq!(
        absent
            .verify_current_conventional_generation(false)
            .unwrap(),
        (0, 0, 0)
    );
    assert_eq!(conventional_audit_source_snapshot(&absent), before);
    assert!(absent.read_meta("db_id").unwrap().is_none());

    assert!(absent.verify_current_conventional_generation(true).is_err());
    assert_eq!(conventional_audit_source_snapshot(&absent), before);
}

#[test]
fn sqlite_file_detach_fails_closed_while_an_external_reader_pins_wal() {
    let path = std::env::temp_dir().join(format!("mneme-detach-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let writer = CozoStore::open(p, 4).unwrap();
    let peer = raw_persistent_test_store(&path, 4);
    writer.put_meta("detach_test", "before").unwrap();

    // Establish a peer snapshot, then put a newer frame in WAL that this
    // reader prevents a TRUNCATE checkpoint from retiring.
    let reader = peer.db.multi_transaction(false);
    reader
        .run_script("?[v] := *meta{k: 'detach_test', v}", BTreeMap::new())
        .unwrap();
    writer.put_meta("detach_test", "after").unwrap();

    let error = writer.prepare_for_file_move().unwrap_err().to_string();
    assert!(
        error.contains("checkpoint incomplete") || error.contains("locked"),
        "unexpected detach failure: {error}"
    );
    let wal = std::path::PathBuf::from(format!("{}-wal", path.to_string_lossy()));
    assert!(
        wal.exists(),
        "a failed detach must preserve the WAL that may still own live state"
    );

    reader.abort().unwrap();
    drop(reader);
    drop(peer);
    let mut detached = false;
    for _ in 0..50 {
        match writer.prepare_for_file_move() {
            Ok(()) => {
                detached = true;
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    assert!(
        detached,
        "detach should succeed after the peer reader closes"
    );
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(
            !std::path::PathBuf::from(format!("{}{suffix}", path.to_string_lossy())).exists(),
            "successful detach left SQLite sidecar {suffix}"
        );
    }
    drop(writer);
    remove_sqlite_test_files(&path);
}

#[test]
fn conventional_canonical_node_audit_is_physically_paged_and_reports_the_tail_id() {
    let store = legacy_store(4).unwrap();
    let ids = (1u128..=(crate::canonical_node_contract::AUDIT_PAGE_SIZE as u128 + 1))
        .map(|raw| NodeId(Ulid::from(raw)))
        .collect::<Vec<_>>();
    raw_insert_nodes(&store, &ids);

    store.reset_query_count();
    assert_eq!(
        store
            .audit_canonical_nodes_for_conventional_upgrade()
            .unwrap(),
        (ids.len(), 0, false)
    );
    assert_eq!(
        store.query_count(),
        7,
        "four exact catalog checks plus one full page, one tail page, and one bounded empty probe"
    );

    let tail = *ids.last().unwrap();
    let mut invalid = serde_json::to_value(active_node(tail)).unwrap();
    invalid["body"] = serde_json::json!(format!(
        "x://{}",
        "a".repeat(mneme_core::MAX_BODY_REF_BYTES)
    ));
    let mut params = BTreeMap::new();
    params.insert("id".into(), dv_str(&tail.0.to_string()));
    params.insert(
        "data".into(),
        dv_str(&serde_json::to_string(&invalid).unwrap()),
    );
    store
        .run(
            "?[id, data, status] <- [[$id, $data, 'active']] \
                 :put node {id => data, status}",
            params,
            true,
        )
        .unwrap();

    store.reset_query_count();
    let error = store
        .audit_canonical_nodes_for_conventional_upgrade()
        .unwrap_err();
    assert!(
        error.to_string().contains(&tail.0.to_string()),
        "tail failure did not identify its canonical row: {error}"
    );
    assert_eq!(
        store.query_count(),
        6,
        "four exact catalog checks precede the two pages reaching the invalid tail"
    );
}

fn seed_legacy_membership_rows(
    store: &CozoStore,
    rows: &[(String, TaggedPhysicalStatus, i64, NodeId)],
) {
    for batch in rows.chunks(crate::tag_projection::MEMBERSHIP_WRITE_CHUNK) {
        let data = DataValue::List(
            batch
                .iter()
                .map(|(tag, status, hash, id)| {
                    DataValue::List(vec![
                        dv_str(tag),
                        dv_str(status.as_str()),
                        dv_int(*hash),
                        dv_str(&id.0.to_string()),
                    ])
                })
                .collect(),
        );
        store.run(&format!("?[tag, status, sample_hash, id] <- $rows :put {} {{tag, status, sample_hash, id}}", crate::tag_projection::SHADOW_NODE_TAG),
            BTreeMap::from([("rows".into(), data)]), true).unwrap();
    }
}

fn seed_legacy_membership_fixture(store: &CozoStore, nodes: &[Node]) {
    store
        .run(
            &crate::tag_projection::create_membership_script(
                crate::tag_projection::SHADOW_NODE_TAG,
            ),
            BTreeMap::new(),
            true,
        )
        .unwrap();
    let rows: Vec<_> = nodes
        .iter()
        .flat_map(|node| {
            node.tags().map(move |tag| {
                (
                    tag.to_owned(),
                    TaggedPhysicalStatus::from(node.status()),
                    stable_tag_sample_hash(node.id()),
                    node.id(),
                )
            })
        })
        .collect();
    seed_legacy_membership_rows(store, &rows);
}

#[test]
fn legacy_membership_verification_scales_by_node_page() {
    use crate::tag_projection as tag;

    let store = legacy_store(4).unwrap();
    let dense_tags = (0..mneme_core::MAX_NODE_TAGS)
        .map(|index| format!("dense-{index:02}"))
        .collect::<Vec<_>>();
    let dense_tag_refs = dense_tags.iter().map(String::as_str).collect::<Vec<_>>();
    let nodes = (1u128..=129)
        .map(|raw| {
            let id = NodeId(Ulid::from(raw));
            if (2..=6).contains(&raw) {
                tagged_node(id, &dense_tag_refs)
            } else {
                // Includes a zero-tag node in every trusted page, including
                // the one-row tail page.
                active_node(id)
            }
        })
        .collect::<Vec<_>>();
    raw_insert_node_values(&store, &nodes);

    let pages = nodes.len().div_ceil(tag::NODE_AUDIT_PAGE);
    let memberships = 5 * mneme_core::MAX_NODE_TAGS;
    seed_legacy_membership_fixture(&store, &nodes);

    store.reset_query_count();
    store
        .verify_membership_upgrade_projection(tag::SHADOW_NODE_TAG, memberships, false)
        .unwrap();
    assert_eq!(
        store.query_count(),
        1 + (pages + 1) + pages,
        "one aggregate count, one trusted scan per page plus the empty probe, and one capped by_id query per nonempty page"
    );

    let explain_ids = nodes
        .iter()
        .take(tag::NODE_AUDIT_PAGE)
        .map(Node::id)
        .collect::<Vec<_>>();
    let (input, params) = id_input("wanted", &explain_ids);
    let query = tag::membership_verification_query(&input, tag::SHADOW_NODE_TAG);
    let plan = store
        .run(&format!("::explain {{ {query} }}"), params, false)
        .unwrap();
    let rendered = format!("{:?}", plan.rows);
    assert!(
        !rendered.contains("stored_mat_join") && rendered.contains("stored_prefix_join"),
        "conventional membership verification must be prefix-backed: {rendered}"
    );
    let references = plan
        .rows
        .iter()
        .filter_map(|row| row.get(5))
        .filter_map(|value| want_str(value).ok())
        .collect::<Vec<_>>();
    let expected_index = format!(":{}:{}", tag::SHADOW_NODE_TAG, tag::SHADOW_NODE_TAG_BY_ID);
    assert!(
        references.contains(&expected_index.as_str()),
        "conventional membership verification did not enter the by_id index: {rendered}"
    );
}

#[test]
fn membership_upgrade_verification_caps_hostile_page_results() {
    use crate::tag_projection as tag;

    let store = legacy_store(4).unwrap();
    let id = NodeId(Ulid::from(1u128));
    raw_insert_nodes(&store, &[id]);
    store
        .run(
            &tag::create_membership_script(tag::SHADOW_NODE_TAG),
            BTreeMap::new(),
            true,
        )
        .unwrap();
    let hash = stable_tag_sample_hash(id);
    for start in (0..tag::MEMBERSHIP_VERIFY_RESULT_CAP).step_by(tag::MEMBERSHIP_WRITE_CHUNK) {
        let end = (start + tag::MEMBERSHIP_WRITE_CHUNK).min(tag::MEMBERSHIP_VERIFY_RESULT_CAP);
        let batch = (start..end)
            .map(|index| {
                (
                    format!("hostile-{index:04}"),
                    TaggedPhysicalStatus::Active,
                    hash,
                    id,
                )
            })
            .collect::<Vec<_>>();
        seed_legacy_membership_rows(&store, &batch);
    }

    store.reset_query_count();
    let error = store
        .verify_membership_upgrade_projection(
            tag::SHADOW_NODE_TAG,
            tag::MEMBERSHIP_VERIFY_RESULT_CAP,
            false,
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("verification cap"),
        "hostile membership page was not rejected at its canary: {error}"
    );
    assert_eq!(
        store.query_count(),
        3,
        "aggregate, one trusted node page, and one capped membership query"
    );
}

#[test]
fn membership_upgrade_verification_rejects_balanced_orphans_and_wrong_tuples() {
    use crate::tag_projection as tag;

    let store = legacy_store(4).unwrap();
    let source = tagged_node(NodeId(Ulid::from(1u128)), &["expected"]);
    let untagged = active_node(NodeId(Ulid::from(2u128)));
    raw_insert_node_values(&store, &[source.clone(), untagged]);
    seed_legacy_membership_fixture(&store, std::slice::from_ref(&source));

    let orphan = NodeId(Ulid::from(999u128));
    let mut params = BTreeMap::new();
    params.insert("source_id".into(), dv_str(&source.id().0.to_string()));
    params.insert(
        "source_hash".into(),
        dv_int(stable_tag_sample_hash(source.id())),
    );
    params.insert("orphan_id".into(), dv_str(&orphan.0.to_string()));
    params.insert("orphan_hash".into(), dv_int(stable_tag_sample_hash(orphan)));
    store
        .run(
            &format!(
                "{{?[tag, status, sample_hash, id] <- \
                         [['expected', 'active', $source_hash, $source_id]] \
                       :rm {} {{tag, status, sample_hash, id}}}}\n\
                     {{?[tag, status, sample_hash, id] <- \
                         [['orphan', 'active', $orphan_hash, $orphan_id]] \
                       :put {} {{tag, status, sample_hash, id}}}}",
                tag::SHADOW_NODE_TAG,
                tag::SHADOW_NODE_TAG
            ),
            params,
            true,
        )
        .unwrap();
    let error = store
        .verify_membership_upgrade_projection(tag::SHADOW_NODE_TAG, 1, false)
        .unwrap_err();
    assert!(
        error.to_string().contains(&source.id().0.to_string()),
        "balanced missing/orphan corruption did not identify the source node: {error}"
    );

    for (label, replacement_tag, replacement_status, hash_delta) in [
        ("tag", "wrong", "active", 0),
        ("status", "expected", "candidate", 0),
        ("hash", "expected", "active", 1),
    ] {
        let store = legacy_store(4).unwrap();
        let source = tagged_node(NodeId(Ulid::from(1u128)), &["expected"]);
        raw_insert_node_values(&store, std::slice::from_ref(&source));
        seed_legacy_membership_fixture(&store, std::slice::from_ref(&source));
        let hash = stable_tag_sample_hash(source.id());
        let mut params = BTreeMap::new();
        params.insert("id".into(), dv_str(&source.id().0.to_string()));
        params.insert("hash".into(), dv_int(hash));
        params.insert("replacement_tag".into(), dv_str(replacement_tag));
        params.insert("replacement_status".into(), dv_str(replacement_status));
        params.insert(
            "replacement_hash".into(),
            dv_int(hash.wrapping_add(hash_delta)),
        );
        store
            .run(
                &format!(
                    "{{?[tag, status, sample_hash, id] <- \
                             [['expected', 'active', $hash, $id]] \
                           :rm {} {{tag, status, sample_hash, id}}}}\n\
                         {{?[tag, status, sample_hash, id] <- \
                             [[$replacement_tag, $replacement_status, $replacement_hash, $id]] \
                           :put {} {{tag, status, sample_hash, id}}}}",
                    tag::SHADOW_NODE_TAG,
                    tag::SHADOW_NODE_TAG
                ),
                params,
                true,
            )
            .unwrap();
        let error = store
            .verify_membership_upgrade_projection(tag::SHADOW_NODE_TAG, 1, false)
            .unwrap_err();
        assert!(
            error.to_string().contains(&source.id().0.to_string()),
            "wrong {label} tuple was not rejected exactly: {error}"
        );
    }
}

#[tokio::test]
async fn batch_node_hydration_is_one_query_and_positional() {
    let store = CozoStore::new(4).unwrap();
    let first = Node::try_new(
        NodeId(Ulid::from(1u128)),
        "shared first",
        BodyRef::new("inline://shared-first").unwrap(),
        ["shared-tag"],
        Provenance::derived([NodeId(Ulid::from(99u128)), NodeId(Ulid::from(98u128))]).unwrap(),
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    let mut second = active_node(NodeId(Ulid::from(2u128)));
    second.set_status(NodeStatus::Archived);
    store.put_node(&first).await.unwrap();
    store.put_node(&second).await.unwrap();
    let missing = NodeId(Ulid::from(3u128));

    store.reset_query_count();
    let hydrated = store
        .get_nodes(&[second.id(), missing, first.id(), second.id()])
        .await
        .unwrap();
    assert_eq!(store.query_count(), 1, "one bounded ID set is one query");
    assert_eq!(hydrated.len(), 4);
    assert_eq!(hydrated[0].as_ref().map(Node::id), Some(second.id()));
    assert!(hydrated[0].as_ref().unwrap().is_archived());
    assert_eq!(hydrated[0].as_ref().unwrap().status(), NodeStatus::Archived);
    assert!(hydrated[1].is_none());
    assert_eq!(hydrated[2].as_ref().map(Node::id), Some(first.id()));
    assert_eq!(hydrated[3].as_ref().map(Node::id), Some(second.id()));

    store.reset_query_count();
    let repeated = vec![first.id(); MAX_NODE_HYDRATION_BATCH];
    let repeated = store.get_nodes(&repeated).await.unwrap();
    assert_eq!(
        store.query_count(),
        1,
        "duplicates hydrate one canonical row"
    );
    let canonical = repeated[0].as_ref().unwrap();
    match canonical.provenance() {
        Provenance::Derived { from } => assert_eq!(
            from.as_slice(),
            &[NodeId(Ulid::from(99u128)), NodeId(Ulid::from(98u128))]
        ),
        _ => panic!("pointer-sharing fixture must use Derived provenance"),
    }
    for cloned in repeated.iter().map(|node| node.as_ref().unwrap()).skip(1) {
        assert_eq!(cloned.summary().as_ptr(), canonical.summary().as_ptr());
        assert_eq!(
            cloned.tags().next().unwrap().as_ptr(),
            canonical.tags().next().unwrap().as_ptr()
        );
        match (cloned.provenance(), canonical.provenance()) {
            (Provenance::Derived { from: cloned }, Provenance::Derived { from: canonical }) => {
                assert_eq!(cloned.as_slice().as_ptr(), canonical.as_slice().as_ptr())
            }
            _ => panic!("pointer-sharing fixture must use Derived provenance"),
        }
    }

    store.reset_query_count();
    assert!(store.get_nodes(&[]).await.unwrap().is_empty());
    assert_eq!(store.query_count(), 0, "an empty batch does no DB work");

    let oversized = vec![first.id(); MAX_NODE_HYDRATION_BATCH + 1];
    assert!(matches!(
        store.get_nodes(&oversized).await,
        Err(Error::CapacityExceeded {
            resource: "node hydration batch",
            limit: MAX_NODE_HYDRATION_BATCH,
        })
    ));
    assert_eq!(
        store.query_count(),
        0,
        "oversized input fails before DB work"
    );
}

#[tokio::test]
async fn batch_node_status_lookup_is_one_query_positional_and_bounded() {
    let store = CozoStore::new(4).unwrap();
    let active = active_node(NodeId(Ulid::from(11u128)));
    let mut archived = active_node(NodeId(Ulid::from(12u128)));
    archived.set_status(NodeStatus::Archived);
    store.put_node(&active).await.unwrap();
    store.put_node(&archived).await.unwrap();
    let missing = NodeId(Ulid::from(13u128));

    store.reset_query_count();
    assert_eq!(
        store
            .get_node_statuses(&[archived.id(), missing, active.id(), archived.id()])
            .await
            .unwrap(),
        vec![
            Some(TaggedPhysicalStatus::Archived),
            None,
            Some(TaggedPhysicalStatus::Active),
            Some(TaggedPhysicalStatus::Archived),
        ]
    );
    assert_eq!(store.query_count(), 1, "one bounded ID set is one query");

    store.reset_query_count();
    assert!(store.get_node_statuses(&[]).await.unwrap().is_empty());
    assert_eq!(store.query_count(), 0, "an empty batch does no DB work");

    let repeated = vec![active.id(); MAX_NODE_STATUS_BATCH];
    store.reset_query_count();
    assert_eq!(
        store.get_node_statuses(&repeated).await.unwrap(),
        vec![Some(TaggedPhysicalStatus::Active); MAX_NODE_STATUS_BATCH]
    );
    assert_eq!(
        store.query_count(),
        1,
        "duplicate identities query one canonical status row"
    );

    let oversized = vec![active.id(); MAX_NODE_STATUS_BATCH + 1];
    store.reset_query_count();
    assert!(matches!(
        store.get_node_statuses(&oversized).await,
        Err(Error::CapacityExceeded {
            resource: "node status batch",
            limit: MAX_NODE_STATUS_BATCH,
        })
    ));
    assert_eq!(
        store.query_count(),
        0,
        "oversized input fails before DB work"
    );
}

#[tokio::test]
async fn maintenance_edges_skip_stale_rows_independently_and_prune_current_degree() {
    let store = CozoStore::new(4).unwrap();
    let cold = ColdPath::acquire();
    let first_id = NodeId(Ulid::from(10u128));
    let second_id = NodeId(Ulid::from(20u128));
    let hub = NodeId(Ulid::from(30u128));
    for id in [first_id, second_id, hub] {
        store.put_node(&active_node(id)).await.unwrap();
    }

    // Generic canonical inventory remains independent from retired node decay.
    let through = store
        .maintenance_node_upper_bound(cold)
        .await
        .unwrap()
        .unwrap();
    let page = store
        .maintenance_nodes_page(cold, None, through, 2)
        .await
        .unwrap();
    assert_eq!(
        page.items.iter().map(Node::id).collect::<Vec<_>>(),
        vec![first_id, second_id]
    );
    assert_eq!(page.next, Some(second_id));

    let mut weak = Edge::new(first_id, second_id, 0.05, EdgeKind::Associative, 1);
    weak.anchor = Some(BodySpan::new(4, 9));
    store.put_edge(&weak).await.unwrap();
    let independent = Edge::new(second_id, hub, 0.04, EdgeKind::Associative, 1);
    store.put_edge(&independent).await.unwrap();
    let through = store
        .maintenance_edge_upper_bound(cold)
        .await
        .unwrap()
        .unwrap();
    let page = store
        .maintenance_edges_page(cold, None, through, 1)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.next.is_some());
    let mut reinforced = weak.clone();
    reinforced.reinforce(4, &mneme_core::StrengthParams::default());
    store.put_edge(&reinforced).await.unwrap();
    let stale_delete = store
        .commit_maintenance(
            cold,
            &MaintenanceCommit {
                edges: vec![
                    MaintenanceEdgeMutation::DeleteWeak { expected: weak },
                    MaintenanceEdgeMutation::DeleteWeak {
                        expected: independent.clone(),
                    },
                ],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        stale_delete.applied_edges,
        vec![MaintenanceEdgeKey::from_edge(&independent)]
    );
    assert!(store.get_edge(second_id, hub).await.unwrap().is_none());
    let survived = store.get_edge(first_id, second_id).await.unwrap().unwrap();
    assert_eq!(survived.trials(), 1);
    assert_eq!(survived.anchor, Some(BodySpan::new(4, 9)));

    let spokes = [
        NodeId(Ulid::from(31u128)),
        NodeId(Ulid::from(32u128)),
        NodeId(Ulid::from(33u128)),
    ];
    for (index, spoke) in spokes.into_iter().enumerate() {
        store.put_node(&active_node(spoke)).await.unwrap();
        store
            .put_edge(&Edge::new(
                hub,
                spoke,
                0.1 + index as f32 * 0.1,
                EdgeKind::Associative,
                1,
            ))
            .await
            .unwrap();
    }
    let first_chunk = store
        .prune_incident_associations(cold, hub, 1, 1)
        .await
        .unwrap();
    assert_eq!(
        first_chunk,
        DensePruneChunkOutcome {
            pruned: 1,
            remaining_excess: 1
        }
    );
    let second_chunk = store
        .prune_incident_associations(cold, hub, 1, 1)
        .await
        .unwrap();
    assert_eq!(
        second_chunk,
        DensePruneChunkOutcome {
            pruned: 1,
            remaining_excess: 0
        }
    );
    assert!(store.get_edge(hub, spokes[2]).await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_full_edge_maintenance_chunk_is_constant_statement_and_reader_safe() {
    let path = std::env::temp_dir().join(format!("mneme-maint-batch-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let writer = Arc::new(CozoStore::open(p, 4).unwrap());
    let peer = Arc::new(raw_persistent_test_store(&path, 4));
    let cold = ColdPath::acquire();
    let hub = NodeId(Ulid::from(1u128));
    writer.put_node(&active_node(hub)).await.unwrap();
    let mut edges = Vec::new();
    for raw in 2u128..2 + MAX_MAINTENANCE_BATCH_ROWS as u128 + 1 {
        let id = NodeId(Ulid::from(raw));
        writer.put_node(&active_node(id)).await.unwrap();
        let mut edge = Edge::new(hub, id, 0.05, EdgeKind::Associative, 1);
        edge.anchor = Some(BodySpan::new(2, 5));
        writer.put_edge(&edge).await.unwrap();
        edges.push(edge);
    }
    let deletions = edges
        .iter()
        .cloned()
        .map(|expected| MaintenanceEdgeMutation::DeleteWeak { expected })
        .collect::<Vec<_>>();
    writer.reset_query_count();
    assert!(matches!(
        writer
            .commit_maintenance(
                cold,
                &MaintenanceCommit {
                    edges: deletions.clone()
                }
            )
            .await,
        Err(Error::CapacityExceeded {
            resource: "maintenance batch rows",
            limit: MAX_MAINTENANCE_BATCH_ROWS,
        })
    ));
    assert_eq!(
        writer.query_count(),
        0,
        "oversized edge batches fail before SQLite work"
    );

    // A newer edge wins its CAS without blocking the other independent rows.
    let mut reinforced = edges[0].clone();
    reinforced.reinforce(9, &mneme_core::StrengthParams::default());
    peer.put_edge(&reinforced).await.unwrap();
    let stale_key = MaintenanceEdgeKey::from_edge(&reinforced);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let reader_store = Arc::clone(&peer);
    let reader_stop = Arc::clone(&stop);
    let reader_reads = Arc::clone(&reads);
    let (reader_started_tx, reader_started_rx) = std::sync::mpsc::channel();
    let reader = tokio::spawn(async move {
        let mut reader_started_tx = Some(reader_started_tx);
        while !reader_stop.load(AtomicOrdering::Acquire) {
            let node = reader_store.get_node(hub).await?;
            if node.is_none() {
                return Err(backend_str(
                    "persistent maintenance reader lost its node".into(),
                ));
            }
            reader_reads.fetch_add(1, AtomicOrdering::Relaxed);
            if let Some(started) = reader_started_tx.take() {
                let _ = started.send(());
            }
            tokio::task::yield_now().await;
        }
        Ok::<(), Error>(())
    });
    reader_started_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let reads_before = reads.load(AtomicOrdering::Relaxed);
    writer.reset_query_count();
    let started = std::time::Instant::now();
    let outcome = writer
        .commit_maintenance(
            cold,
            &MaintenanceCommit {
                edges: deletions
                    .into_iter()
                    .take(MAX_MAINTENANCE_BATCH_ROWS)
                    .collect(),
            },
        )
        .await;
    let elapsed = started.elapsed();
    stop.store(true, AtomicOrdering::Release);
    let reader_result = reader.await.unwrap();
    assert!(
        reader_result.is_ok(),
        "persistent reader failed: {reader_result:?}"
    );
    let outcome = outcome.unwrap();
    assert_eq!(outcome.applied_edges.len(), MAX_MAINTENANCE_BATCH_ROWS - 1);
    assert!(!outcome.applied_edges.contains(&stale_key));
    assert_eq!(writer.query_count(), 1, "one adapter commit call");
    assert_eq!(
        writer.last_maintenance_statements(),
        5,
        "one semantic-endpoint read, two edge/anchor CAS reads and two deletes"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "bounded SQLite maintenance took {elapsed:?}"
    );
    assert!(
        reads.load(AtomicOrdering::Relaxed) > reads_before,
        "persistent reader must make progress during the bounded write"
    );
    assert_eq!(
        writer
            .get_edge(reinforced.from, reinforced.to)
            .await
            .unwrap()
            .unwrap()
            .trials(),
        1
    );
    assert_eq!(
        writer
            .get_anchor(reinforced.from, reinforced.to)
            .await
            .unwrap(),
        reinforced.anchor
    );
    assert!(
        writer
            .get_edge(edges[1].from, edges[1].to)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        writer
            .get_anchor(edges[1].from, edges[1].to)
            .await
            .unwrap()
            .is_none()
    );
    assert!(writer.get_node(hub).await.unwrap().unwrap().is_active());
    drop(peer);
    drop(writer);
    remove_sqlite_test_files(&path);
}

fn feedback_idempotency(
    key: &str,
    payload: &str,
    epoch: &str,
    sequence: u64,
    floor: u64,
) -> FeedbackIdempotency {
    let digest = payload
        .bytes()
        .fold(0u64, |hash, byte| hash.wrapping_mul(257) ^ u64::from(byte));
    FeedbackIdempotency::new(
        key,
        format!("{digest:064x}"),
        FeedbackRetryScope::new(epoch, sequence, floor).unwrap(),
    )
    .unwrap()
}

#[test]
fn full_merge_endpoint_queries_use_primary_and_by_to_indexes() {
    let store = CozoStore::new(4).unwrap();
    let mut params = BTreeMap::new();
    params.insert(
        "winner".into(),
        dv_str(&NodeId(Ulid::from(1u128)).0.to_string()),
    );

    let cases = [
        (
            "?[from, to] := *edge{from: $winner, to}, from = $winner",
            ":edge",
        ),
        (
            "?[from, to] := *edge{to: $winner, from}, to = $winner",
            ":edge:by_to",
        ),
        (
            "?[from, to] := *edge_anchor{from: $winner, to}, from = $winner",
            ":edge_anchor",
        ),
    ];
    for (query, expected_relation) in cases {
        let plan = store
            .run(&format!("::explain {{ {query} }}"), params.clone(), false)
            .unwrap();
        let rendered = format!("{:?}", plan.rows);
        assert!(
            !rendered.contains("stored_mat_join") && rendered.contains("stored_prefix_join"),
            "full-merge endpoint lookup must stay prefix-backed: {rendered}"
        );
        let references = plan
            .rows
            .iter()
            .filter_map(|row| row.get(5))
            .filter_map(|value| want_str(value).ok())
            .collect::<Vec<_>>();
        assert!(
            references.contains(&expected_relation),
            "expected {expected_relation:?} in full-merge plan references {references:?}"
        );
    }
}

#[test]
fn maintenance_preflights_are_prefix_joined_not_relation_materialized() {
    let path = std::env::temp_dir().join(format!("mneme-maint-plan-{}.db", Ulid::new()));
    let store = CozoStore::open(path.to_str().unwrap(), 4).unwrap();
    let ids = [NodeId(Ulid::from(1u128)), NodeId(Ulid::from(2u128))];

    let (input, mut params) = id_input("candidate_hub", &ids);
    params.insert("target".into(), dv_int(8));
    params.insert("fetch".into(), dv_int(3));
    let plan = store
        .run(
            &format!(
                "::explain {{ {} }}",
                maintenance_overfull_hubs_query(&input)
            ),
            params,
            false,
        )
        .unwrap();
    let rendered = format!("{:?}", plan.rows);
    assert!(
        !rendered.contains("stored_mat_join"),
        "hub preflight must never materialize the edge relation: {rendered}"
    );
    assert!(
        rendered.contains("stored_prefix_join"),
        "hub preflight must enter both adjacency directions by prefix: {rendered}"
    );
    let references = plan
        .rows
        .iter()
        .filter_map(|row| row.get(5))
        .filter_map(|value| want_str(value).ok())
        .collect::<Vec<_>>();
    assert!(
        references.contains(&":edge"),
        "missing primary edge lookup: {rendered}"
    );
    assert!(
        references.contains(&":edge:by_to"),
        "missing reverse edge-index lookup: {rendered}"
    );

    let (input, params) = id_input("wanted", &ids);
    let plan = store
        .run(
            &format!("::explain {{ {input}\n?[id, status] := wanted[id], *node{{id, status}} }}"),
            params,
            false,
        )
        .unwrap();
    let rendered = format!("{:?}", plan.rows);
    assert!(
        !rendered.contains("stored_mat_join") && rendered.contains("stored_prefix_join"),
        "legacy migration status lookup must be prefix-backed: {rendered}"
    );

    let pairs = [(ids[0], ids[1]), (ids[1], ids[0])];
    let (edge_input, edge_params) = edge_pair_input("maintenance_edge_key", &pairs);
    let (anchor_input, anchor_params) = edge_pair_input("maintenance_anchor_key", &pairs);
    let cases = [
        (
            "maintenance edge CAS read",
            maintenance_edge_read_query(&edge_input),
            edge_params,
            ":edge",
        ),
        (
            "maintenance anchor CAS read",
            maintenance_anchor_read_query(&anchor_input),
            anchor_params,
            ":edge_anchor",
        ),
    ];
    for (label, query, params, expected_relation) in cases {
        let plan = store
            .run(&format!("::explain {{ {query} }}"), params, false)
            .unwrap();
        let rendered = format!("{:?}", plan.rows);
        assert!(
            !rendered.contains("stored_mat_join") && rendered.contains("stored_prefix_join"),
            "{label} must use a prefix join: {rendered}"
        );
        let references = plan
            .rows
            .iter()
            .filter_map(|row| row.get(5))
            .filter_map(|value| want_str(value).ok())
            .collect::<Vec<_>>();
        assert!(
            references.contains(&expected_relation),
            "{label} must enter {expected_relation}: {rendered}"
        );
    }
    drop(store);
    remove_sqlite_test_files(&path);
}
async fn collect_remote_edges(store: &(impl GraphStore + ?Sized), from: NodeId) -> Vec<RemoteEdge> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .remote_edges_page(from, after, mneme_core::MAX_REMOTE_EDGE_PAGE_SIZE)
            .await
            .unwrap();
        out.extend(page.items);
        let Some(next) = page.next else {
            return out;
        };
        after = Some(next);
    }
}

fn raw_node_search_projection(store: &CozoStore, id: NodeId) -> (String, String) {
    let mut params = BTreeMap::new();
    params.insert("id".into(), dv_str(&id.0.to_string()));
    let rows = store
        .run(
            "?[summary, status] := *node_search{id: $id, summary, status}",
            params,
            false,
        )
        .unwrap();
    let row = rows.rows.first().expect("node_search projection");
    (
        want_str(&row[0]).unwrap().to_owned(),
        want_str(&row[1]).unwrap().to_owned(),
    )
}

fn raw_node_vector_status(store: &CozoStore, id: NodeId) -> String {
    let mut params = BTreeMap::new();
    params.insert("id".into(), dv_str(&id.0.to_string()));
    let rows = store
        .run("?[status] := *node_vec{id: $id, status}", params, false)
        .unwrap();
    want_str(&rows.rows.first().expect("node_vec projection")[0])
        .unwrap()
        .to_owned()
}

/// Test-only escape hatch that simulates a legacy/direct Cozo writer. It is
/// intentionally not a store API: boundary and reopen tests need to create
/// pre-invariant data without making production code capable of doing so.
fn raw_insert_edges(store: &CozoStore, pairs: &[(NodeId, NodeId)]) {
    for chunk in pairs.chunks(128) {
        let (input, params) = edge_pair_input("incoming", chunk);
        let script = format!(
            "{input}\n\
                 ?[from, to, weight, kind, last_reinforced, trials, interference] := \
                   incoming[from, to], weight = 0.5, kind = 'associative', \
                   last_reinforced = 1, trials = 1, interference = 0 \
                   :put edge {{from, to => weight, kind, \
                     last_reinforced, trials, interference}}"
        );
        store.run(&script, params, true).unwrap();
    }
}

fn raw_insert_node_values(store: &CozoStore, nodes: &[Node]) {
    for chunk in nodes.chunks(128) {
        let mut params = BTreeMap::new();
        let rows = chunk
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let id_name = format!("node_id_{index}");
                let data_name = format!("node_data_{index}");
                params.insert(id_name.clone(), dv_str(&node.id().0.to_string()));
                params.insert(
                    data_name.clone(),
                    dv_str(&serde_json::to_string(node).unwrap()),
                );
                format!(
                    "[${id_name}, ${data_name}, '{}']",
                    status_str(node.status())
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        store
            .run(
                &format!(
                    "incoming[id, data, status] <- [{rows}]\n\
                         ?[id, data, status] := incoming[id, data, status] \
                           :put node {{id => data, status}}"
                ),
                params,
                true,
            )
            .unwrap();
    }
}

fn raw_insert_tag_memberships(store: &CozoStore, nodes: &[Node]) {
    let memberships: Vec<_> = nodes
        .iter()
        .flat_map(|node| {
            let status = TaggedPhysicalStatus::from(node.status());
            node.tags().map(move |tag| {
                (
                    tag.to_owned(),
                    status,
                    stable_tag_sample_hash(node.id()),
                    node.id(),
                )
            })
        })
        .collect();
    for chunk in memberships.chunks(128) {
        let mut params = BTreeMap::new();
        let rows = chunk
            .iter()
            .enumerate()
            .map(|(index, (tag, status, sample_hash, id))| {
                let tag_name = format!("membership_tag_{index}");
                let id_name = format!("membership_id_{index}");
                params.insert(tag_name.clone(), dv_str(tag));
                params.insert(id_name.clone(), dv_str(&id.0.to_string()));
                format!(
                    "[${tag_name}, '{}', {sample_hash}, ${id_name}]",
                    status.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        store
            .run(
                &format!(
                    "incoming[tag, status, sample_hash, id] <- [{rows}]\n\
                         ?[tag, status, sample_hash, id] := \
                           incoming[tag, status, sample_hash, id] \
                           :put node_tag_v2 {{tag, status, sample_hash, id}}"
                ),
                params,
                true,
            )
            .unwrap();
    }
}

fn raw_insert_nodes(store: &CozoStore, ids: &[NodeId]) {
    let nodes = ids.iter().copied().map(active_node).collect::<Vec<_>>();
    raw_insert_node_values(store, &nodes);
}

fn raw_insert_projected_nodes(
    store: &CozoStore,
    ids: &[NodeId],
    status: NodeStatus,
    vector: &[f32],
) {
    for chunk in ids.chunks(128) {
        let mut params = BTreeMap::new();
        params.insert("projection_vector".into(), dv_float_list(vector));
        let rows = chunk
            .iter()
            .enumerate()
            .map(|(index, id)| {
                let mut node = active_node(*id);
                node.set_status(status);
                let id_name = format!("projection_id_{index}");
                let data_name = format!("projection_data_{index}");
                params.insert(id_name.clone(), dv_str(&id.0.to_string()));
                params.insert(
                    data_name.clone(),
                    dv_str(&serde_json::to_string(&node).unwrap()),
                );
                format!("[${id_name}, ${data_name}, '{}']", status_str(status))
            })
            .collect::<Vec<_>>()
            .join(", ");
        store
            .run(
                &format!(
                    "incoming[id, data, status] <- [{rows}]\n\
                         ?[id, data, status] := incoming[id, data, status] \
                           :put node {{id => data, status}}"
                ),
                params.clone(),
                true,
            )
            .unwrap();
        store
            .run(
                &format!(
                    "incoming[id, data, status] <- [{rows}]\n\
                         ?[id, e, status] := incoming[id, data, status], \
                           e = vec($projection_vector) \
                           :put node_vec {{id => e, status}}"
                ),
                params,
                true,
            )
            .unwrap();
    }
}

fn raw_insert_vectors_for_nodes(store: &CozoStore, nodes: &[Node], vector: &[f32]) {
    for chunk in nodes.chunks(128) {
        let mut params = BTreeMap::new();
        params.insert("projection_vector".into(), dv_float_list(vector));
        let rows = chunk
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let id_name = format!("vector_id_{index}");
                params.insert(id_name.clone(), dv_str(&node.id().0.to_string()));
                format!("[${id_name}, '{}']", status_str(node.status()))
            })
            .collect::<Vec<_>>()
            .join(", ");
        store
            .run(
                &format!(
                    "incoming[id, status] <- [{rows}]\n\
                         ?[id, e, status] := incoming[id, status], \
                           e = vec($projection_vector) \
                           :put node_vec {{id => e, status}}"
                ),
                params,
                true,
            )
            .unwrap();
    }
}

fn create_current_vector_indices_for_test(relation: &str, dim: usize) -> String {
    vector_lanes().into_iter().map(|(index, status)| format!(
        "{{::hnsw create {relation}:{index} {{dim: {dim}, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == '{status}'}}}}"
    )).collect::<Vec<_>>().join("\n")
}

#[tokio::test]
async fn active_hnsw_does_not_starve_beyond_legacy_scan_ceiling() {
    const DISTRACTORS: usize = 4_097;
    let store = CozoStore::new(4).unwrap();
    // Bulk-load the adversarial fixture before building the two graphs.
    // Incrementally maintaining 4K HNSW inserts would make this correctness
    // regression dominate the crate's test runtime for no added coverage.
    store.drop_hnsw_indices("node_vec").unwrap();
    let archived: Vec<_> = (1u128..=DISTRACTORS as u128)
        .map(|value| NodeId(Ulid::from(value)))
        .collect();
    let active = NodeId(Ulid::from(10_000u128));
    let second_active = NodeId(Ulid::from(10_001u128));

    raw_insert_projected_nodes(
        &store,
        &archived,
        NodeStatus::Archived,
        &[1.0, 0.0, 0.0, 0.0],
    );
    raw_insert_projected_nodes(&store, &[active], NodeStatus::Active, &[1.0, 0.4, 0.0, 0.0]);
    raw_insert_projected_nodes(
        &store,
        &[second_active],
        NodeStatus::Active,
        &[1.0, 0.6, 0.0, 0.0],
    );
    store
        .run(
            &create_current_vector_indices_for_test("node_vec", 4),
            BTreeMap::new(),
            true,
        )
        .unwrap();

    store.reset_query_count();
    let active_hits = store
        .ann(&[1.0, 0.0, 0.0, 0.0], 1, StatusFilter::ACTIVE)
        .await
        .unwrap();
    assert_eq!(active_hits.first().map(|hit| hit.id), Some(active));
    assert_eq!(
        store.query_count(),
        1,
        "ANN must not issue a count/widen/fallback query"
    );

    store.reset_query_count();
    let mixed = store
        .ann(&[1.0, 0.0, 0.0, 0.0], 2, StatusFilter::ACTIVE)
        .await
        .unwrap();
    assert_eq!(mixed.len(), 2);
    assert!(mixed.iter().any(|hit| hit.id == active));
    assert!(mixed.iter().any(|hit| hit.id == second_active));
    assert!(mixed.iter().all(|hit| !archived.contains(&hit.id)));
    assert_eq!(
        store.query_count(),
        1,
        "one active partition must use one Cozo script/snapshot"
    );
}

fn tagged_request<'a>(
    query: &'a [f32],
    tags: impl IntoIterator<Item = &'a str>,
    lanes: &'a [TaggedAnnLaneRequest],
) -> TaggedAnnRequest<'a> {
    tagged_request_with_limits(query, tags, lanes, TaggedAnnWorkLimits::default())
}

fn tagged_request_with_limits<'a>(
    query: &'a [f32],
    tags: impl IntoIterator<Item = &'a str>,
    lanes: &'a [TaggedAnnLaneRequest],
    limits: TaggedAnnWorkLimits,
) -> TaggedAnnRequest<'a> {
    TaggedAnnRequest::new(query, tags, lanes, limits).unwrap()
}

fn tagged_exceeded_limit(batch: &TaggedAnnBatch) -> TaggedExactWorkLimit {
    match &batch.lanes[0].seed_coverage {
        TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter {
            exceeded_limit, ..
        }
        | TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter { exceeded_limit, .. } => {
            *exceeded_limit
        }
        TaggedSeedCoverage::ExactCosine => panic!("expected bounded tagged fallback"),
    }
}

#[tokio::test]
async fn cozo_exact_tagged_results_match_memstore_for_active_membership() {
    let cozo = CozoStore::new(2).unwrap();
    let mem = crate::MemStore::new(2);
    let mut archived = tagged_node(NodeId(Ulid::from(51_003u128)), &["a"]);
    archived.set_status(NodeStatus::Archived);
    let fixtures = [
        (
            tagged_node(NodeId(Ulid::from(51_001u128)), &["a", "b"]),
            [1.0, 0.0],
        ),
        (
            tagged_node(NodeId(Ulid::from(51_002u128)), &["b"]),
            [0.0, 1.0],
        ),
        (archived, [0.8, 0.2]),
    ];
    for (node, vector) in &fixtures {
        cozo.put_node(node).await.unwrap();
        cozo.upsert(node.id(), vector).await.unwrap();
        mem.put_node(node).await.unwrap();
        mem.upsert(node.id(), vector).await.unwrap();
    }
    let lanes = [TaggedAnnLaneRequest::new(
        mneme_core::tagged::RetrievalLifecycleLane::Primary,
        StatusFilter::ACTIVE,
        2,
        2,
    )
    .unwrap()];
    let query = [1.0, 0.0];
    let cozo_batch = cozo
        .tagged_ann(tagged_request(&query, ["b", "a"], &lanes))
        .await
        .unwrap();
    let mem_batch = mem
        .tagged_ann(tagged_request(&query, ["b", "a"], &lanes))
        .await
        .unwrap();
    assert_eq!(cozo_batch.lanes, mem_batch.lanes);
    assert_eq!(cozo_batch.lanes[0].hits.len(), 2);
    assert!(
        cozo_batch.lanes[0]
            .hits
            .iter()
            .all(|hit| hit.id != fixtures[2].0.id())
    );
    assert_eq!(cozo_batch.work, mem_batch.work);
    assert_eq!(
        cozo_batch.projection_generation.as_str(),
        crate::tag_projection::META_VALUE
    );
}

#[tokio::test]
async fn cozo_tagged_exact_boundary_falls_back_at_the_4097th_raw_membership() {
    let store = CozoStore::new(2).unwrap();
    let nodes: Vec<_> = (1u128..=4_097)
        .map(|value| tagged_node(NodeId(Ulid::from(60_000u128 + value)), &["popular"]))
        .collect();
    raw_insert_node_values(&store, &nodes[..4_096]);
    raw_insert_tag_memberships(&store, &nodes[..4_096]);

    let lanes = [TaggedAnnLaneRequest::new(
        mneme_core::tagged::RetrievalLifecycleLane::Primary,
        StatusFilter::ACTIVE,
        1,
        8,
    )
    .unwrap()];
    let query = [1.0, 0.0];
    let exact = store
        .tagged_ann(tagged_request(&query, ["popular"], &lanes))
        .await
        .unwrap();
    assert_eq!(exact.work.raw_memberships, 4_096);
    assert_eq!(exact.work.unique_exact_ids, 4_096);
    assert_eq!(
        exact.lanes[0].seed_coverage,
        TaggedSeedCoverage::ExactCosine
    );

    raw_insert_node_values(&store, &nodes[4_096..]);
    raw_insert_tag_memberships(&store, &nodes[4_096..]);
    let noise = active_node(NodeId(Ulid::from(70_000u128)));
    store.put_node(&noise).await.unwrap();
    store.upsert(noise.id(), &[1.0, 0.0]).await.unwrap();
    let partial = store
        .tagged_ann(tagged_request(&query, ["popular"], &lanes))
        .await
        .unwrap();
    assert_eq!(partial.work.raw_memberships, 4_097);
    assert_eq!(partial.work.unique_exact_ids, 4_097);
    assert!(partial.lanes[0].seed_coverage.is_partial());
    assert!(
        partial.lanes[0].hits.is_empty(),
        "untagged HNSW noise leaked"
    );
    assert_eq!(partial.work.fallback_hnsw_inspected, 1);
    assert_eq!(partial.work.fallback_sample_inspected, 4);
    let TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter { physical, .. } =
        &partial.lanes[0].seed_coverage
    else {
        panic!("persistent fallback returned the wrong strategy")
    };
    assert_eq!(
        physical
            .iter()
            .map(|item| item.total_quota().unwrap())
            .sum::<usize>(),
        8
    );
    assert!(partial.work.fallback_unique_ids <= 8);
}

#[tokio::test]
async fn persistent_tagged_scan_charges_multitag_duplicates_and_probes_the_final_range() {
    let path = std::env::temp_dir().join(format!("mneme-tagged-scan-{}.db", Ulid::new()));
    let store = CozoStore::open(path.to_str().unwrap(), 2).unwrap();
    let nodes: Vec<_> = (1u128..=2_048)
        .map(|value| tagged_node(NodeId(Ulid::from(75_000u128 + value)), &["a", "b"]))
        .collect();
    raw_insert_node_values(&store, &nodes);
    raw_insert_tag_memberships(&store, &nodes);

    let lanes = [TaggedAnnLaneRequest::new(
        mneme_core::tagged::RetrievalLifecycleLane::Primary,
        StatusFilter::ACTIVE,
        1,
        8,
    )
    .unwrap()];
    let query = [1.0, 0.0];
    store.tagged_read_test_hook.begin_scan_recording();
    let exact = store
        .tagged_ann(tagged_request(&query, ["b", "a"], &lanes))
        .await
        .unwrap();
    let scans = store.tagged_read_test_hook.finish_scan_recording();
    assert_eq!(exact.work.raw_memberships, 4_096);
    assert_eq!(exact.work.unique_exact_ids, 2_048);
    assert_eq!(
        exact.lanes[0].seed_coverage,
        TaggedSeedCoverage::ExactCosine
    );
    assert!(
        scans
            .iter()
            .all(|scan| scan.limit <= TAGGED_SCAN_PAGE && scan.returned <= scan.limit),
        "every exact prefix page must obey its physical scan limit: {scans:?}"
    );
    assert_eq!(
        scans.last(),
        Some(&TaggedScanObservation {
            tag: "b".into(),
            status: TaggedPhysicalStatus::Active,
            continued: true,
            limit: 1,
            returned: 0,
        }),
        "exactness at the boundary requires an empty canary probe after the final tag"
    );

    let extra = tagged_node(NodeId(Ulid::from(80_000u128)), &["b"]);
    raw_insert_node_values(&store, std::slice::from_ref(&extra));
    raw_insert_tag_memberships(&store, std::slice::from_ref(&extra));
    let partial = store
        .tagged_ann(tagged_request(&query, ["a", "b"], &lanes))
        .await
        .unwrap();
    assert_eq!(partial.work.raw_memberships, 4_097);
    assert_eq!(partial.work.unique_exact_ids, 2_049);
    assert_eq!(
        tagged_exceeded_limit(&partial),
        TaggedExactWorkLimit::RawMemberships
    );

    drop(store);
    remove_sqlite_test_files(&path);
}

#[tokio::test]
async fn persistent_tagged_exact_fuses_stage_unique_ids_before_components() {
    let path = std::env::temp_dir().join(format!("mneme-tagged-fuses-{}.db", Ulid::new()));
    let store = CozoStore::open(path.to_str().unwrap(), 2).unwrap();
    let nodes: Vec<_> = (1u128..=3)
        .map(|value| tagged_node(NodeId(Ulid::from(81_000u128 + value)), &["overflow"]))
        .collect();
    raw_insert_node_values(&store, &nodes);
    raw_insert_tag_memberships(&store, &nodes);
    let lanes = [TaggedAnnLaneRequest::new(
        mneme_core::tagged::RetrievalLifecycleLane::Primary,
        StatusFilter::ACTIVE,
        1,
        4,
    )
    .unwrap()];
    let query = [1.0, 0.0];

    let unique_limits = TaggedAnnWorkLimits::new(8, 2, 8, 2, 4, 4, 8).unwrap();
    let unique = store
        .tagged_ann(tagged_request_with_limits(
            &query,
            ["overflow"],
            &lanes,
            unique_limits,
        ))
        .await
        .unwrap();
    assert_eq!(unique.work.raw_memberships, 3);
    assert_eq!(unique.work.unique_exact_ids, 3);
    assert_eq!(
        tagged_exceeded_limit(&unique),
        TaggedExactWorkLimit::UniqueExactIds
    );

    let component_limits = TaggedAnnWorkLimits::new(8, 4, 4, 4, 4, 4, 8).unwrap();
    let components = store
        .tagged_ann(tagged_request_with_limits(
            &query,
            ["overflow"],
            &lanes,
            component_limits,
        ))
        .await
        .unwrap();
    assert_eq!(components.work.raw_memberships, 3);
    assert_eq!(components.work.unique_exact_ids, 3);
    assert_eq!(
        tagged_exceeded_limit(&components),
        TaggedExactWorkLimit::ExactVectorComponents
    );
    assert_eq!(components.work.exact_hydrated_ids, 0);
    assert_eq!(components.work.exact_vector_components, 0);

    drop(store);
    remove_sqlite_test_files(&path);
}

#[tokio::test]
async fn persistent_tagged_fallback_wraps_to_a_useful_sample_when_hnsw_misses() {
    let path = std::env::temp_dir().join(format!("mneme-tagged-wrap-{}.db", Ulid::new()));
    let store = CozoStore::open(path.to_str().unwrap(), 2).unwrap();
    let mut hashed_ids: Vec<_> = (90_000u128..)
        .map(|value| NodeId(Ulid::from(value)))
        .map(|id| (stable_tag_sample_hash(id), id))
        .filter(|(hash, _)| *hash < 0)
        .take(2)
        .collect();
    hashed_ids.sort_unstable();
    assert_ne!(hashed_ids[0].0, hashed_ids[1].0);
    let sampled = hashed_ids[0].1;
    let other = hashed_ids[1].1;
    let max_hash = hashed_ids[1].0;
    let lanes = [TaggedAnnLaneRequest::new(
        mneme_core::tagged::RetrievalLifecycleLane::Primary,
        StatusFilter::ACTIVE,
        1,
        2,
    )
    .unwrap()];
    let limits = TaggedAnnWorkLimits::new(1, 2, 4, 1, 2, 2, 4).unwrap();
    let (query, pivot) = (1..=256)
        .find_map(|step| {
            let query = [1.0, step as f32 / 257.0];
            let request = tagged_request_with_limits(&query, ["popular"], &lanes, limits);
            let pivot = tagged_sample_pivot(
                &request,
                TaggedPhysicalStatus::Active,
                mneme_core::tagged::RetrievalLifecycleLane::Primary,
                <CozoStore as VectorIndex>::semantic_id(&store),
            )
            .unwrap();
            (pivot > max_hash).then_some((query, pivot))
        })
        .expect("bounded query search must find a pivot above both membership hashes");

    let sampled_node = tagged_node(sampled, &["popular"]);
    let other_node = tagged_node(other, &["popular"]);
    for (node, vector) in [(&sampled_node, [0.0, 1.0]), (&other_node, [0.0, -1.0])] {
        store.put_node(node).await.unwrap();
        store.upsert(node.id(), &vector).await.unwrap();
    }
    let noise = active_node(NodeId(Ulid::from(99_000u128)));
    store.put_node(&noise).await.unwrap();
    store.upsert(noise.id(), &query).await.unwrap();

    let request = tagged_request_with_limits(&query, ["popular"], &lanes, limits);
    let batch = store.tagged_ann(request.clone()).await.unwrap();
    let repeated = store.tagged_ann(request).await.unwrap();
    assert_eq!(batch, repeated, "fallback selection must be deterministic");
    assert_eq!(batch.work.raw_memberships, 2);
    assert_eq!(
        tagged_exceeded_limit(&batch),
        TaggedExactWorkLimit::RawMemberships
    );
    assert_eq!(batch.work.fallback_hnsw_inspected, 1);
    assert_eq!(batch.work.fallback_sample_inspected, 1);
    assert_eq!(batch.work.fallback_unique_ids, 2);
    assert_eq!(batch.work.fallback_canonical_candidates_checked, 2);
    assert_eq!(batch.work.fallback_matching_candidates, 1);
    assert_eq!(batch.lanes[0].hits[0].id, sampled);
    let TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter { physical, .. } =
        &batch.lanes[0].seed_coverage
    else {
        panic!("persistent fallback returned the wrong strategy")
    };
    assert_eq!(physical.len(), 1);
    assert_eq!(physical[0].sample_pivot, pivot);
    assert!(
        pivot > max_hash,
        "the selected member is only reachable through the wrapped prefix range"
    );
    assert_eq!(physical[0].hnsw_quota, 1);
    assert_eq!(physical[0].hnsw_inspected, 1);
    assert_eq!(physical[0].tag_samples[0].quota, 1);
    assert_eq!(physical[0].tag_samples[0].inspected, 1);

    drop(store);
    remove_sqlite_test_files(&path);
}

#[tokio::test]
async fn cozo_tagged_fallback_keeps_32_tag_active_reserve_and_excludes_archived() {
    let store = CozoStore::new(2).unwrap();
    store.drop_hnsw_indices("node_vec").unwrap();
    let tags: Vec<_> = (0..32).map(|index| format!("tag-{index:02}")).collect();
    let active_tagged: Vec<_> = tags
        .iter()
        .enumerate()
        .map(|(index, tag)| tagged_node(NodeId(Ulid::from(110_000u128 + index as u128)), &[tag]))
        .collect();
    let archived_tagged: Vec<_> = tags
        .iter()
        .enumerate()
        .map(|(index, tag)| {
            let mut node = tagged_node(NodeId(Ulid::from(120_000u128 + index as u128)), &[tag]);
            node.set_status(NodeStatus::Archived);
            node
        })
        .collect();
    raw_insert_node_values(&store, &active_tagged);
    raw_insert_node_values(&store, &archived_tagged);
    raw_insert_tag_memberships(&store, &active_tagged);
    raw_insert_tag_memberships(&store, &archived_tagged);
    raw_insert_vectors_for_nodes(&store, &active_tagged, &[0.0, 1.0]);
    raw_insert_vectors_for_nodes(&store, &archived_tagged, &[0.0, 1.0]);

    let active_noise: Vec<_> = (0..96)
        .map(|index| NodeId(Ulid::from(130_000u128 + index)))
        .collect();
    let archived_noise: Vec<_> = (0..32)
        .map(|index| NodeId(Ulid::from(140_000u128 + index)))
        .collect();
    raw_insert_projected_nodes(&store, &active_noise, NodeStatus::Active, &[1.0, 0.0]);
    raw_insert_projected_nodes(&store, &archived_noise, NodeStatus::Archived, &[1.0, 0.0]);
    store
        .run(
            &create_current_vector_indices_for_test("node_vec", 2),
            BTreeMap::new(),
            true,
        )
        .unwrap();

    let lanes = [TaggedAnnLaneRequest::new(
        mneme_core::tagged::RetrievalLifecycleLane::Primary,
        StatusFilter::ACTIVE,
        1,
        mneme_core::tagged::TAGGED_ACTIVE_FALLBACK_CANDIDATES,
    )
    .unwrap()];
    let limits = TaggedAnnWorkLimits::new(1, 2, 4, 1, 256, 256, 512).unwrap();
    let query = [1.0, 0.0];
    let batch = store
        .tagged_ann(tagged_request_with_limits(
            &query,
            tags.iter().map(String::as_str),
            &lanes,
            limits,
        ))
        .await
        .unwrap();
    assert_eq!(batch.work.raw_memberships, 2);
    assert_eq!(
        tagged_exceeded_limit(&batch),
        TaggedExactWorkLimit::RawMemberships
    );
    // HNSW is approximate and may legitimately return fewer than `k` even
    // when the lane contains enough rows. The requested reserves are exact;
    // inspected coverage is honest partiality, not a test oracle for ANN
    // internals.
    assert!(batch.work.fallback_hnsw_inspected <= 96);
    assert_eq!(batch.work.fallback_sample_inspected, 32);
    assert!(
        (32..=batch.work.fallback_hnsw_inspected + 32).contains(&batch.work.fallback_unique_ids)
    );
    assert_eq!(
        batch.work.fallback_canonical_candidates_checked,
        batch.work.fallback_unique_ids
    );
    assert_eq!(batch.work.fallback_matching_candidates, 32);
    assert_eq!(
        batch.work.fallback_hydrated_ids,
        batch.work.fallback_unique_ids
    );
    assert_eq!(
        batch.work.fallback_vector_components,
        batch.work.fallback_unique_ids * 2
    );

    let active_ids: HashSet<_> = active_tagged.iter().map(Node::id).collect();
    assert!(active_ids.contains(&batch.lanes[0].hits[0].id));
    assert_eq!(batch.lanes.len(), 1);
    let lane = &batch.lanes[0];
    let TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter {
        physical,
        canonical_candidates_checked,
        matching_candidates,
        ..
    } = &lane.seed_coverage
    else {
        panic!("persistent fallback returned the wrong strategy")
    };
    assert_eq!(physical.len(), 1);
    let physical = &physical[0];
    assert_eq!(physical.status, TaggedPhysicalStatus::Active);
    assert_eq!(physical.hnsw_quota, 96);
    assert!(physical.hnsw_inspected <= 96);
    assert_eq!(physical.sample_quota().unwrap(), 96);
    assert_eq!(physical.sample_inspected().unwrap(), 32);
    assert_eq!(physical.total_quota().unwrap(), 192);
    assert_eq!(physical.tag_samples.len(), 32);
    assert!(
        physical
            .tag_samples
            .iter()
            .all(|sample| sample.inspected == 1)
    );
    assert!(
        (physical.hnsw_inspected.max(32)..=physical.hnsw_inspected + 32)
            .contains(canonical_candidates_checked)
    );
    assert_eq!(*matching_candidates, 32);
    assert_eq!(batch.work.fallback_hnsw_inspected, physical.hnsw_inspected);
}

#[tokio::test]
async fn cozo_tagged_scan_fails_closed_on_corrupt_sample_hash() {
    let store = CozoStore::new(2).unwrap();
    let node = tagged_node(NodeId(Ulid::from(80_001u128)), &["corrupt"]);
    store.put_node(&node).await.unwrap();
    let expected_hash = stable_tag_sample_hash(node.id());
    let mut params = BTreeMap::new();
    params.insert("tag".into(), dv_str("corrupt"));
    params.insert("status".into(), dv_str("active"));
    params.insert("expected".into(), dv_int(expected_hash));
    params.insert("wrong".into(), dv_int(expected_hash.wrapping_add(1)));
    params.insert("id".into(), dv_str(&node.id().0.to_string()));
    store
        .run(
            "{?[tag, status, sample_hash, id] <- [[$tag, $status, $expected, $id]] \
                   :rm node_tag_v2 {tag, status, sample_hash, id}}\n\
                 {?[tag, status, sample_hash, id] <- [[$tag, $status, $wrong, $id]] \
                   :put node_tag_v2 {tag, status, sample_hash, id}}",
            params,
            true,
        )
        .unwrap();
    let lanes = [TaggedAnnLaneRequest::new(
        mneme_core::tagged::RetrievalLifecycleLane::Primary,
        StatusFilter::ACTIVE,
        1,
        1,
    )
    .unwrap()];
    let query = [1.0, 0.0];
    let error = store
        .tagged_ann(tagged_request(&query, ["corrupt"], &lanes))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("sample hash"), "{error}");
}

#[test]
fn tagged_read_admission_is_fail_fast_and_reusable() {
    let admission = TaggedReadAdmission::default();
    let permits: Vec<_> = (0..MAX_CONCURRENT_TAGGED_READS)
        .map(|_| admission.try_acquire().unwrap())
        .collect();
    assert!(admission.try_acquire().is_err());
    drop(permits);
    assert!(admission.try_acquire().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_tagged_waiters_keep_activity_and_permits_until_workers_exit() {
    let store = Arc::new(CozoStore::new(2).unwrap());
    let hold = store.tagged_read_test_hook.hold();
    let mut waiters = Vec::new();
    for _ in 0..MAX_CONCURRENT_TAGGED_READS {
        let store = store.clone();
        waiters.push(tokio::spawn(async move {
            let query = [1.0, 0.0];
            let lanes = [TaggedAnnLaneRequest::new(
                mneme_core::tagged::RetrievalLifecycleLane::Primary,
                StatusFilter::ACTIVE,
                1,
                1,
            )
            .unwrap()];
            let request = tagged_request(&query, ["held"], &lanes);
            store.tagged_ann(request).await
        }));
    }

    assert!(
        store
            .tagged_read_test_hook
            .wait_for_entered(MAX_CONCURRENT_TAGGED_READS, Duration::from_secs(2)),
        "all four blocking workers must enter the test hold"
    );
    assert_eq!(store.backend_activity.in_flight(), 4);
    assert!(
        store.prepare_for_file_move().is_err(),
        "lease handoff must fail while detached backend work is live"
    );

    for waiter in waiters {
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
    }
    assert_eq!(
        store.backend_activity.in_flight(),
        4,
        "cancelling Tokio waiters must not release detached-job activity"
    );

    let query = [1.0, 0.0];
    let lanes = [TaggedAnnLaneRequest::new(
        mneme_core::tagged::RetrievalLifecycleLane::Primary,
        StatusFilter::ACTIVE,
        1,
        1,
    )
    .unwrap()];
    let error = store
        .tagged_ann(tagged_request(&query, ["held"], &lanes))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("capacity exhausted"),
        "a fifth read must not reuse a cancelled waiter's live permit: {error}"
    );

    drop(hold);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while (store.backend_activity.in_flight() != 0 || store.tagged_read_admission.live() != 0)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        store.backend_activity.in_flight(),
        0,
        "activity must clear after every detached worker and transaction exits"
    );
    assert_eq!(
        store.tagged_read_admission.live(),
        0,
        "permits must clear only after detached worker teardown"
    );

    let permits: Vec<_> = (0..MAX_CONCURRENT_TAGGED_READS)
        .map(|_| store.tagged_read_admission.try_acquire().unwrap())
        .collect();
    drop(permits);
    assert!(
        store
            .tagged_ann(tagged_request(&query, ["held"], &lanes))
            .await
            .is_ok(),
        "joined worker teardown must make tagged reads reusable"
    );
}

#[test]
fn tagged_fallback_membership_hydration_is_candidate_bound_by_id_work() {
    let store = CozoStore::new(2).unwrap();
    let ids = [
        NodeId(Ulid::from(90_001u128)),
        NodeId(Ulid::from(90_002u128)),
    ];
    let (wanted, mut params) = id_input("wanted_tagged", &ids);
    params.insert("membership_cap".into(), dv_int(129));
    let query = tagged_membership_hydration_query(&wanted);
    let plan = store
        .run(&format!("::explain {{ {query} }}"), params, false)
        .unwrap();
    let rendered = format!("{:?}", plan.rows);
    assert!(
        !rendered.contains("stored_mat_join") && rendered.contains("stored_prefix_join"),
        "tagged candidate membership hydration must remain prefix-backed: {rendered}"
    );
    let references = plan
        .rows
        .iter()
        .filter_map(|row| row.get(5))
        .filter_map(|value| want_str(value).ok())
        .collect::<Vec<_>>();
    assert!(
        references.contains(&":node_tag_v2:by_id"),
        "tagged fallback did not bind candidates into by_id: {rendered}"
    );
}

/// Test-only bulk fixture for the bounded retry ledger. Production writes
/// must go through `commit_feedback`, which owns reachability reclamation.
fn raw_insert_feedback_retries(store: &CozoStore, epoch: &str, count: usize) {
    for chunk_start in (0..count).step_by(128) {
        let chunk_end = (chunk_start + 128).min(count);
        let mut params = BTreeMap::new();
        let rows = (chunk_start..chunk_end)
            .map(|index| {
                let key_name = format!("retry_key_{index}");
                let fingerprint_name = format!("retry_fingerprint_{index}");
                let sequence_name = format!("retry_sequence_{index}");
                params.insert(key_name.clone(), dv_str(&format!("retry:{index}")));
                params.insert(fingerprint_name.clone(), dv_str(&format!("{index:064x}")));
                params.insert(
                    sequence_name.clone(),
                    dv_int(i64::try_from(index + 1).unwrap()),
                );
                format!("[${key_name}, ${fingerprint_name}, ${sequence_name}, true]")
            })
            .collect::<Vec<_>>()
            .join(", ");
        params.insert("retry_epoch".into(), dv_str(epoch));
        store
            .run(
                &format!(
                    "incoming[key, fingerprint, sequence, marker] <- [{rows}]\n\
                         ?[key, fingerprint, applied_at] := \
                           incoming[key, fingerprint, applied_at, marker] \
                           :put feedback_retry {{key => fingerprint, applied_at}}"
                ),
                params.clone(),
                true,
            )
            .unwrap();
        store
            .run(
                &format!(
                    "incoming[key, fingerprint, sequence, marker] <- [{rows}]\n\
                         ?[epoch, sequence, key, marker] := \
                           incoming[key, fingerprint, sequence, marker], \
                           epoch = $retry_epoch \
                           :put feedback_retry_order \
                             {{epoch, sequence, key => marker}}"
                ),
                params,
                true,
            )
            .unwrap();
    }
}

/// Test-only fixture for filling the bounded full-merge proof relation.
/// Production inserts belong to the atomic collapse transaction.
fn raw_insert_full_merge_records(store: &CozoStore, records: &[FullMergeRecord]) {
    for chunk in records.chunks(128) {
        let mut params = BTreeMap::new();
        let rows = chunk
            .iter()
            .enumerate()
            .map(|(index, record)| {
                record.validate().unwrap();
                let (lo, hi) = canonical(record.between);
                let lo_name = format!("merge_lo_{index}");
                let hi_name = format!("merge_hi_{index}");
                let winner_name = format!("merge_winner_{index}");
                let loser_name = format!("merge_loser_{index}");
                let applied_name = format!("merge_applied_{index}");
                params.insert(lo_name.clone(), dv_str(&lo));
                params.insert(hi_name.clone(), dv_str(&hi));
                params.insert(winner_name.clone(), dv_str(&record.winner.0.to_string()));
                params.insert(loser_name.clone(), dv_str(&record.loser.0.to_string()));
                params.insert(
                    applied_name.clone(),
                    dv_int(i64::try_from(record.applied_at).unwrap()),
                );
                format!("[${lo_name}, ${hi_name}, ${winner_name}, ${loser_name}, ${applied_name}]")
            })
            .collect::<Vec<_>>()
            .join(", ");
        store
            .run(
                &format!(
                    "incoming[lo, hi, winner, loser, applied_at] <- [{rows}]\n\
                         ?[lo, hi, winner, loser, applied_at] := \
                           incoming[lo, hi, winner, loser, applied_at] \
                           :put full_merge_commit \
                             {{lo, hi => winner, loser, applied_at}}"
                ),
                params,
                true,
            )
            .unwrap();
    }
}

/// Test-only legacy/direct writer for remote rows. Production callers must
/// always go through the transactionally guarded GraphStore method.
fn raw_insert_remote_edges(store: &CozoStore, edges: &[RemoteEdge]) {
    for chunk in edges.chunks(128) {
        let mut params = BTreeMap::new();
        let rows = chunk
            .iter()
            .enumerate()
            .map(|(index, edge)| {
                let from = format!("remote_from_{index}");
                let target_db = format!("remote_db_{index}");
                let target = format!("remote_target_{index}");
                params.insert(from.clone(), dv_str(&edge.from.0.to_string()));
                params.insert(target_db.clone(), dv_str(&edge.target_db.to_string()));
                params.insert(target.clone(), dv_str(&edge.target.0.to_string()));
                format!("[${from}, ${target_db}, ${target}]")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let script = format!(
            "incoming[from, target_db, target] <- [{rows}]\n\
                 ?[from, target_db, target, weight] := \
                   incoming[from, target_db, target], weight = 0.5 \
                   :put remote_edge {{from, target_db, target => weight}}"
        );
        store.run(&script, params, true).unwrap();
    }
}

fn remote_edges_for(source: NodeId, target_db: Ulid, count: usize) -> Vec<RemoteEdge> {
    (0..count)
        .map(|offset| {
            RemoteEdge::new(
                source,
                target_db,
                NodeId(Ulid::from(200_000u128 + offset as u128)),
                0.5,
            )
        })
        .collect()
}

#[tokio::test]
async fn direct_graph_writers_reject_invalid_domain_values_without_mutation() {
    let store = CozoStore::new(4).unwrap();
    let a = NodeId(Ulid::from(139_000u128));
    let b = NodeId(Ulid::from(139_001u128));
    let missing = NodeId(Ulid::from(139_002u128));
    store.put_node(&active_node(a)).await.unwrap();
    store.put_node(&active_node(b)).await.unwrap();

    let mut reversed_anchor = Edge::from_stored(a, b, EdgeKind::Associative, None, 0.5, 1, 1, 0);
    reversed_anchor.anchor = Some(BodySpan::new(8, 4));
    for edge in [
        Edge::from_stored(a, b, EdgeKind::Associative, None, f32::NAN, 1, 0, 0),
        Edge::from_stored(a, b, EdgeKind::Associative, None, -0.0, 1, 0, 0),
        Edge::from_stored(
            a,
            b,
            EdgeKind::Associative,
            None,
            0.5,
            i64::MAX as Timestamp + 1,
            0,
            0,
        ),
        Edge::from_stored(a, b, EdgeKind::Associative, None, 0.5, 1, 0, 1),
        reversed_anchor,
    ] {
        assert!(matches!(
            store.put_edge(&edge).await,
            Err(Error::InvalidInput(_))
        ));
        assert!(store.get_edge(a, b).await.unwrap().is_none());
    }

    for edge in [
        RemoteEdge::new(a, Ulid::nil(), b, 0.5),
        RemoteEdge::new(a, store.db_id(), b, 0.5),
    ] {
        assert!(matches!(
            store.put_remote_edge(&edge).await,
            Err(Error::InvalidInput(_))
        ));
    }
    assert!(collect_remote_edges(&store, a).await.is_empty());

    for result in [
        store.observe_contradiction(a, a, 1).await,
        store.observe_contradiction(a, missing, 1).await,
        store
            .observe_contradiction(a, b, i64::MAX as Timestamp + 1)
            .await,
        store.observe_merge_candidate(a, a, 1).await,
        store.observe_merge_candidate(a, missing, 1).await,
        store
            .observe_merge_candidate(a, b, i64::MAX as Timestamp + 1)
            .await,
    ] {
        assert!(matches!(result, Err(Error::InvalidInput(_))));
    }
    assert!(
        store
            .open_contradictions(ColdPath::acquire())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .open_merge_candidates(ColdPath::acquire())
            .await
            .unwrap()
            .is_empty()
    );

    let invalid_contradiction = Contradiction {
        between: UnorderedPair(a, b),
        observations: 0,
        first_seen: 1,
        last_seen: 1,
        resolution: None,
    };
    let invalid_merge = MergeCandidate {
        between: UnorderedPair(a, b),
        observations: 1,
        first_seen: i64::MAX as Timestamp + 1,
        last_seen: i64::MAX as Timestamp + 1,
        resolution: None,
    };
    assert!(matches!(
        store.upsert_contradiction(&invalid_contradiction),
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        store.upsert_merge(&invalid_merge),
        Err(Error::InvalidInput(_))
    ));
}

#[tokio::test]
async fn terminal_overlay_history_may_dangle_but_open_overlays_may_not() {
    let store = CozoStore::new(4).unwrap();
    let a = NodeId(Ulid::from(139_100u128));
    let b = NodeId(Ulid::from(139_101u128));
    let pair = UnorderedPair(a, b);
    store.put_node(&active_node(a)).await.unwrap();
    store.put_node(&active_node(b)).await.unwrap();
    store.observe_contradiction(a, b, 1).await.unwrap();
    store.observe_merge_candidate(a, b, 1).await.unwrap();
    store
        .resolve_contradiction(pair, Resolution::ContextDependent)
        .await
        .unwrap();
    store
        .resolve_merge_candidate(pair, MergeResolution::Keep)
        .await
        .unwrap();
    store.delete_node(b).await.unwrap();

    store.observe_contradiction(a, b, 2).await.unwrap();
    store.observe_merge_candidate(a, b, 2).await.unwrap();
    let export = store.export().await.unwrap();
    assert_eq!(export.contradictions[0].last_seen, 2);
    assert_eq!(export.merges[0].last_seen, 2);

    let open_contradiction = Contradiction::new(a, b, 3);
    let open_merge = MergeCandidate::new(a, b, 3);
    assert!(matches!(
        store.upsert_contradiction(&open_contradiction),
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        store.upsert_merge(&open_merge),
        Err(Error::InvalidInput(_))
    ));
}

async fn assert_invalid_import_leaves_destination_empty(source: &crate::MemStore) {
    let mut destination = CozoStore::new(4).unwrap();
    assert!(matches!(
        destination.import_mem(source).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(destination.ensure_empty_import_target().is_ok());
}

#[tokio::test]
async fn graph_import_preflight_rejects_invalid_rows_before_any_mutation() {
    let a = NodeId(Ulid::from(139_200u128));
    let b = NodeId(Ulid::from(139_201u128));

    let invalid_edge_source = crate::MemStore::new(4);
    invalid_edge_source.put_node(&active_node(a)).await.unwrap();
    invalid_edge_source.put_node(&active_node(b)).await.unwrap();
    let invalid_edge = Edge::from_stored(
        a,
        b,
        EdgeKind::Associative,
        None,
        0.5,
        i64::MAX as Timestamp + 1,
        0,
        0,
    );
    invalid_edge_source
        .lock()
        .edges
        .insert((a, b), invalid_edge);
    assert_invalid_import_leaves_destination_empty(&invalid_edge_source).await;

    let dangling_overlay_source = crate::MemStore::new(4);
    dangling_overlay_source
        .put_node(&active_node(a))
        .await
        .unwrap();
    let pair = UnorderedPair(a, b);
    dangling_overlay_source
        .lock()
        .contradictions
        .insert(pair, Contradiction::new(a, b, 1));
    assert_invalid_import_leaves_destination_empty(&dangling_overlay_source).await;

    let invalid_merge_source = crate::MemStore::new(4);
    invalid_merge_source
        .put_node(&active_node(a))
        .await
        .unwrap();
    invalid_merge_source
        .put_node(&active_node(b))
        .await
        .unwrap();
    invalid_merge_source.lock().merges.insert(
        pair,
        MergeCandidate {
            between: pair,
            observations: 0,
            first_seen: 1,
            last_seen: 1,
            resolution: None,
        },
    );
    assert_invalid_import_leaves_destination_empty(&invalid_merge_source).await;

    let invalid_remote_source = crate::MemStore::new(4);
    invalid_remote_source
        .put_node(&active_node(a))
        .await
        .unwrap();
    let source_db = invalid_remote_source.db_id();
    let invalid_remote = RemoteEdge::new(a, source_db, b, 0.5);
    invalid_remote_source.lock().remote_edges.insert(
        (
            invalid_remote.from,
            invalid_remote.target_db,
            invalid_remote.target,
        ),
        invalid_remote,
    );
    assert_invalid_import_leaves_destination_empty(&invalid_remote_source).await;
}

#[tokio::test]
async fn strict_graph_record_decoders_reject_noncanonical_storage_without_rewriting_it() {
    let store = CozoStore::new(4).unwrap();
    let a = NodeId(Ulid::from(139_300u128));
    let b = NodeId(Ulid::from(139_301u128));
    store.put_node(&active_node(a)).await.unwrap();
    store.put_node(&active_node(b)).await.unwrap();

    let mut edge = BTreeMap::new();
    edge.insert("from".into(), dv_str(&a.0.to_string()));
    edge.insert("to".into(), dv_str(&b.0.to_string()));
    edge.insert("weight".into(), dv_float(-0.0));
    store
        .run(
            "?[from, to, weight, kind, last_reinforced, trials, interference] <- \
             [[$from, $to, $weight, 'associative', 1, 0, 0]] \
             :put edge {from, to => weight, kind, last_reinforced, trials, interference}",
            edge,
            true,
        )
        .unwrap();
    assert!(matches!(store.get_edge(a, b).await, Err(Error::Backend(_))));
    let stored_weight = store
        .run(
            "?[weight] := *edge{from: $from, to: $to, weight}",
            BTreeMap::from([
                ("from".into(), dv_str(&a.0.to_string())),
                ("to".into(), dv_str(&b.0.to_string())),
            ]),
            false,
        )
        .unwrap();
    assert!(
        want_f64(&stored_weight.rows[0][0])
            .unwrap()
            .is_sign_negative()
    );

    store
        .run(
            "?[from, to, weight, kind, last_reinforced, trials, interference] <- \
             [[$from, $to, 0.5, 'associative', -1, 0, 0]] \
             :put edge {from, to => weight, kind, last_reinforced, trials, interference}",
            BTreeMap::from([
                ("from".into(), dv_str(&a.0.to_string())),
                ("to".into(), dv_str(&b.0.to_string())),
            ]),
            true,
        )
        .unwrap();
    assert!(matches!(store.get_edge(a, b).await, Err(Error::Backend(_))));

    let target_db = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let lowercase_target_db = target_db.to_string().to_ascii_lowercase();
    assert_ne!(lowercase_target_db, target_db.to_string());
    store
        .run(
            "?[from, target_db, target, weight] <- \
             [[$from, $target_db, $target, 0.5]] \
             :put remote_edge {from, target_db, target => weight}",
            BTreeMap::from([
                ("from".into(), dv_str(&a.0.to_string())),
                ("target_db".into(), dv_str(&lowercase_target_db)),
                ("target".into(), dv_str(&b.0.to_string())),
            ]),
            true,
        )
        .unwrap();
    assert!(matches!(
        store
            .remote_edges_page(a, None, mneme_core::MAX_REMOTE_EDGE_PAGE_SIZE)
            .await,
        Err(Error::Backend(_))
    ));
    let stored_target_db = store
        .run(
            "?[target_db] := *remote_edge{from: $from, target_db}",
            BTreeMap::from([("from".into(), dv_str(&a.0.to_string()))]),
            false,
        )
        .unwrap();
    assert_eq!(
        want_str(&stored_target_db.rows[0][0]).unwrap(),
        lowercase_target_db
    );
}

#[tokio::test]
async fn overlay_observation_rejects_reversed_and_invalid_physical_rows_without_a_twin() {
    let store = CozoStore::new(4).unwrap();
    let ids = (0..4)
        .map(|offset| NodeId(Ulid::from(139_400u128 + offset)))
        .collect::<Vec<_>>();
    for id in &ids {
        store.put_node(&active_node(*id)).await.unwrap();
    }

    let (lo, hi) = canonical(UnorderedPair(ids[0], ids[1]));
    let reversed = BTreeMap::from([("lo".into(), dv_str(&hi)), ("hi".into(), dv_str(&lo))]);
    store
        .run(
            "?[lo, hi, observations, first_seen, last_seen, resolution] <- \
             [[$lo, $hi, 1, 1, 1, null]] \
             :put contradiction {lo, hi => observations, first_seen, last_seen, resolution}",
            reversed.clone(),
            true,
        )
        .unwrap();
    store
        .run(
            "?[lo, hi, observations, first_seen, last_seen, resolution] <- \
             [[$lo, $hi, 1, 1, 1, null]] \
             :put merge_candidate {lo, hi => observations, first_seen, last_seen, resolution}",
            reversed,
            true,
        )
        .unwrap();
    assert!(matches!(
        store.observe_contradiction(ids[0], ids[1], 2).await,
        Err(Error::Backend(_))
    ));
    assert!(matches!(
        store.observe_merge_candidate(ids[0], ids[1], 2).await,
        Err(Error::Backend(_))
    ));

    let (lo2, hi2) = canonical(UnorderedPair(ids[2], ids[3]));
    let canonical_pair = BTreeMap::from([("lo".into(), dv_str(&lo2)), ("hi".into(), dv_str(&hi2))]);
    store
        .run(
            "?[lo, hi, observations, first_seen, last_seen, resolution] <- \
             [[$lo, $hi, -1, 1, 1, null]] \
             :put contradiction {lo, hi => observations, first_seen, last_seen, resolution}",
            canonical_pair.clone(),
            true,
        )
        .unwrap();
    store
        .run(
            "?[lo, hi, observations, first_seen, last_seen, resolution] <- \
             [[$lo, $hi, 1, -1, 1, null]] \
             :put merge_candidate {lo, hi => observations, first_seen, last_seen, resolution}",
            canonical_pair,
            true,
        )
        .unwrap();
    assert!(matches!(
        store.observe_contradiction(ids[2], ids[3], 2).await,
        Err(Error::Backend(_))
    ));
    assert!(matches!(
        store.observe_merge_candidate(ids[2], ids[3], 2).await,
        Err(Error::Backend(_))
    ));

    for relation in ["contradiction", "merge_candidate"] {
        let rows = store
            .run(
                &format!("?[lo, hi] := *{relation}{{lo, hi}} :order lo, hi"),
                BTreeMap::new(),
                false,
            )
            .unwrap();
        assert_eq!(rows.rows.len(), 2);
        assert!(
            rows.rows.iter().any(|row| {
                want_str(&row[0]).unwrap() == hi && want_str(&row[1]).unwrap() == lo
            })
        );
        assert!(
            !rows.rows.iter().any(|row| {
                want_str(&row[0]).unwrap() == lo && want_str(&row[1]).unwrap() == hi
            })
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlay_endpoint_admission_holds_the_writer_lock_until_the_overlay_is_staged() {
    for (kind, suffix) in [
        (overlay::ObservationKind::Contradiction, "contradiction"),
        (overlay::ObservationKind::MergeCandidate, "merge"),
    ] {
        let path = std::env::temp_dir().join(format!("mneme-overlay-{suffix}-{}.db", Ulid::new()));
        let p = path.to_str().unwrap();
        let observer_store = std::sync::Arc::new(CozoStore::open(p, 4).unwrap());
        let deleter_store = std::sync::Arc::new(raw_persistent_test_store(&path, 4));
        let a = NodeId(Ulid::new());
        let b = NodeId(Ulid::new());
        let pair = UnorderedPair(a, b);
        observer_store.put_node(&active_node(a)).await.unwrap();
        observer_store.put_node(&active_node(b)).await.unwrap();

        overlay::arm_pause(kind, pair);
        let observer = {
            let store = observer_store.clone();
            tokio::spawn(async move {
                match kind {
                    overlay::ObservationKind::Contradiction => {
                        store.observe_contradiction(a, b, 1).await
                    }
                    overlay::ObservationKind::MergeCandidate => {
                        store.observe_merge_candidate(a, b, 1).await
                    }
                }
            })
        };
        overlay::wait_for_pause();

        let mut delete_params = BTreeMap::new();
        delete_params.insert("id".into(), dv_str(&b.0.to_string()));
        let deletion = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            deleter_store.db.run_script(
                &graph::delete_node_script(),
                delete_params,
                cozo::ScriptMutability::Mutable,
            )
        }));
        match deletion {
            Ok(Err(error)) => assert!(
                is_locked(&error),
                "paused endpoint admission returned a non-lock delete error: {error:?}"
            ),
            Err(panic) => assert!(
                panic_is_locked(panic.as_ref()),
                "paused endpoint admission raised a non-lock delete panic"
            ),
            Ok(Ok(_)) => panic!("node deletion committed while overlay admission was paused"),
        }

        overlay::release_pause();
        observer.await.unwrap().unwrap();
        match kind {
            overlay::ObservationKind::Contradiction => assert_eq!(
                observer_store
                    .open_contradictions(ColdPath::acquire())
                    .await
                    .unwrap()
                    .len(),
                1
            ),
            overlay::ObservationKind::MergeCandidate => assert_eq!(
                observer_store
                    .open_merge_candidates(ColdPath::acquire())
                    .await
                    .unwrap()
                    .len(),
                1
            ),
        }

        // This is the separate cleanup invariant: after admission commits,
        // delete_node removes the now-open overlay and its endpoint atomically.
        deleter_store.delete_node(b).await.unwrap();
        assert!(observer_store.get_node(b).await.unwrap().is_none());
        assert!(
            observer_store
                .open_contradictions(ColdPath::acquire())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            observer_store
                .open_merge_candidates(ColdPath::acquire())
                .await
                .unwrap()
                .is_empty()
        );

        drop(deleter_store);
        drop(observer_store);
        let _ = std::fs::remove_file(path);
    }
}

#[tokio::test]
async fn cozo_remote_edge_cap_is_source_owned_and_allows_exact_updates() {
    let store = CozoStore::new(4).unwrap();
    let source = NodeId(Ulid::from(140_000u128));
    let other_source = NodeId(Ulid::from(140_001u128));
    let target_db = Ulid::from(140_002u128);
    let shared_target = NodeId(Ulid::from(300_000u128));
    let initial: Vec<_> = (0..MAX_REMOTE_EDGES_PER_SOURCE - 1)
        .map(|offset| {
            RemoteEdge::new(
                source,
                Ulid::from(141_000u128 + offset as u128),
                shared_target,
                0.5,
            )
        })
        .collect();
    raw_insert_remote_edges(&store, &initial);

    // Identity includes target_db: the same target in 256 databases reaches
    // the same source-owned ceiling as 256 different target node IDs.
    let boundary = RemoteEdge::new(source, target_db, shared_target, 0.6);
    store.put_remote_edge(&boundary).await.unwrap();
    let overflow = RemoteEdge::new(source, Ulid::from(140_003u128), shared_target, 0.7);
    assert!(matches!(
        store.put_remote_edge(&overflow).await,
        Err(Error::CapacityExceeded {
            resource: "remote edges per source",
            limit: MAX_REMOTE_EDGES_PER_SOURCE,
        })
    ));

    let updated = RemoteEdge::new(source, target_db, initial[0].target, 0.95);
    store.put_remote_edge(&updated).await.unwrap();
    let stored = collect_remote_edges(&store, source).await;
    assert_eq!(stored.len(), MAX_REMOTE_EDGES_PER_SOURCE);
    assert_eq!(
        stored
            .iter()
            .find(|edge| { edge.target_db == updated.target_db && edge.target == updated.target })
            .unwrap()
            .weight(),
        0.95
    );

    // The cap is neither global nor target-owned: another local source may
    // point at the same remote target even while this source is saturated.
    store
        .put_remote_edge(&RemoteEdge::new(
            other_source,
            target_db,
            boundary.target,
            0.8,
        ))
        .await
        .unwrap();
    assert_eq!(collect_remote_edges(&store, other_source).await.len(), 1);

    store
        .delete_remote_edge(source, target_db, boundary.target)
        .await
        .unwrap();
    store.put_remote_edge(&overflow).await.unwrap();
    assert_eq!(
        collect_remote_edges(&store, source).await.len(),
        MAX_REMOTE_EDGES_PER_SOURCE
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_single_sqlite_handle_cannot_oversubscribe_remote_source() {
    let path = std::env::temp_dir().join(format!("mneme-remote-one-{}.db", Ulid::new()));
    let source = NodeId(Ulid::from(310_000u128));
    let target_db = Ulid::from(310_001u128);
    let store = std::sync::Arc::new(CozoStore::open(path.to_str().unwrap(), 4).unwrap());
    let spare = 8;
    raw_insert_remote_edges(
        &store,
        &remote_edges_for(source, target_db, MAX_REMOTE_EDGES_PER_SOURCE - spare),
    );

    let mut tasks = Vec::new();
    for offset in 0..(spare * 2) {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            store
                .put_remote_edge(&RemoteEdge::new(
                    source,
                    target_db,
                    NodeId(Ulid::from(320_000u128 + offset as u128)),
                    0.5,
                ))
                .await
        }));
    }
    let mut inserted = 0;
    let mut rejected = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(()) => inserted += 1,
            Err(Error::CapacityExceeded {
                resource: "remote edges per source",
                limit: MAX_REMOTE_EDGES_PER_SOURCE,
            }) => rejected += 1,
            Err(error) => panic!("unexpected remote-edge insertion error: {error}"),
        }
    }
    assert_eq!((inserted, rejected), (spare, spare));
    assert_eq!(
        collect_remote_edges(store.as_ref(), source).await.len(),
        MAX_REMOTE_EDGES_PER_SOURCE
    );

    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sqlite_handles_recheck_remote_source_after_conflict() {
    let path = std::env::temp_dir().join(format!("mneme-remote-many-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let first = std::sync::Arc::new(CozoStore::open(p, 4).unwrap());
    let source = NodeId(Ulid::from(330_000u128));
    let target_db = Ulid::from(330_001u128);
    raw_insert_remote_edges(
        &first,
        &remote_edges_for(source, target_db, MAX_REMOTE_EDGES_PER_SOURCE - 1),
    );
    let second = std::sync::Arc::new(raw_persistent_test_store(&path, 4));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

    let tasks: Vec<_> = [first.clone(), second.clone()]
        .into_iter()
        .enumerate()
        .map(|(offset, store)| {
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait();
                store
                    .put_remote_edge(&RemoteEdge::new(
                        source,
                        target_db,
                        NodeId(Ulid::from(340_000u128 + offset as u128)),
                        0.5,
                    ))
                    .await
            })
        })
        .collect();

    let mut inserted = 0;
    let mut rejected = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(()) => inserted += 1,
            Err(Error::CapacityExceeded {
                resource: "remote edges per source",
                limit: MAX_REMOTE_EDGES_PER_SOURCE,
            }) => rejected += 1,
            Err(error) => panic!("unexpected remote-edge insertion error: {error}"),
        }
    }
    assert_eq!((inserted, rejected), (1, 1));
    assert_eq!(
        collect_remote_edges(first.as_ref(), source).await.len(),
        MAX_REMOTE_EDGES_PER_SOURCE
    );

    drop(second);
    drop(first);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn legacy_remote_source_cap_validator_and_read_are_bounded() {
    let overfull_path =
        std::env::temp_dir().join(format!("mneme-remote-overfull-{}.db", Ulid::new()));
    let p = overfull_path.to_str().unwrap();
    let store = legacy_persistent_store(p, 4).unwrap();
    let source = NodeId(Ulid::from(350_000u128));
    let target_db = Ulid::from(350_001u128);
    let shared_target = NodeId(Ulid::from(350_002u128));
    let overfull: Vec<_> = (0..=MAX_REMOTE_EDGES_PER_SOURCE)
        .map(|offset| {
            RemoteEdge::new(
                source,
                Ulid::from(351_000u128 + offset as u128),
                shared_target,
                0.5,
            )
        })
        .collect();
    raw_insert_remote_edges(&store, &overfull);
    store.delete_meta(REMOTE_EDGE_SOURCE_CAP_META_KEY).unwrap();
    assert!(matches!(
        store
            .remote_edges_page(source, None, mneme_core::MAX_REMOTE_EDGE_PAGE_SIZE)
            .await,
        Err(Error::CapacityExceeded {
            resource: "remote edges per source",
            limit: MAX_REMOTE_EDGES_PER_SOURCE,
        })
    ));
    assert!(matches!(
        store.ensure_remote_edge_source_bound(),
        Err(Error::CapacityExceeded {
            resource: "remote edges per source",
            limit: MAX_REMOTE_EDGES_PER_SOURCE,
        })
    ));
    drop(store);
    assert!(CozoStore::open(p, 4).is_err());
    let _ = std::fs::remove_file(overfull_path);

    let bounded_path =
        std::env::temp_dir().join(format!("mneme-remote-bounded-{}.db", Ulid::new()));
    let p = bounded_path.to_str().unwrap();
    let store = legacy_persistent_store(p, 4).unwrap();
    let edge = RemoteEdge::new(source, target_db, NodeId(Ulid::from(360_000u128)), 0.5);
    store.put_remote_edge(&edge).await.unwrap();
    store.delete_meta(REMOTE_EDGE_SOURCE_CAP_META_KEY).unwrap();
    drop(store);

    assert!(
        CozoStore::open(p, 4).is_err(),
        "normal open cannot silently repair a predecessor"
    );
    let reopened = raw_persistent_test_store(&bounded_path, 4);
    reopened.ensure_remote_edge_source_bound().unwrap();
    assert_eq!(
        reopened.read_meta(REMOTE_EDGE_SOURCE_CAP_META_KEY).unwrap(),
        Some(MAX_REMOTE_EDGES_PER_SOURCE.to_string())
    );
    assert_eq!(collect_remote_edges(&reopened, source).await, vec![edge]);
    drop(reopened);
    let _ = std::fs::remove_file(bounded_path);
}

#[tokio::test]
async fn remote_edge_page_rejects_corrupt_stored_weight() {
    let store = CozoStore::new(4).unwrap();
    let from = NodeId(Ulid::from(365_000u128));
    let target_db = Ulid::from(365_001u128);
    let target = NodeId(Ulid::from(365_002u128));
    let mut params = BTreeMap::new();
    params.insert("from".into(), dv_str(&from.0.to_string()));
    params.insert("target_db".into(), dv_str(&target_db.to_string()));
    params.insert("target".into(), dv_str(&target.0.to_string()));
    params.insert("weight".into(), dv_float(1.5));
    store
        .run(
            "?[from, target_db, target, weight] <- \
                    [[$from, $target_db, $target, $weight]] \
                 :put remote_edge {from, target_db, target => weight}",
            params,
            true,
        )
        .unwrap();

    assert!(matches!(
        store
            .remote_edges_page(from, None, mneme_core::MAX_REMOTE_EDGE_PAGE_SIZE)
            .await,
        Err(Error::Backend(_))
    ));
}

#[tokio::test]
async fn overfull_remote_import_preflight_leaves_destination_empty() {
    let source = crate::MemStore::new(4);
    let from = NodeId(Ulid::from(370_000u128));
    let target_db = Ulid::from(370_001u128);
    {
        // Deliberately emulate an old/direct writer without teaching the
        // production MemStore API how to violate its invariant.
        let mut inner = source.lock();
        for edge in remote_edges_for(from, target_db, MAX_REMOTE_EDGES_PER_SOURCE + 1) {
            inner
                .remote_edges
                .insert((edge.from, edge.target_db, edge.target), edge);
        }
    }
    let mut destination = CozoStore::new(4).unwrap();
    assert!(matches!(
        destination.import_mem(&source).await,
        Err(Error::CapacityExceeded {
            resource: "remote edges per source",
            limit: MAX_REMOTE_EDGES_PER_SOURCE,
        })
    ));
    assert!(destination.ensure_empty_import_target().is_ok());
    let export = destination.export().await.unwrap();
    assert!(export.nodes.is_empty());
    assert!(export.edges.is_empty());
    assert!(export.vectors.is_empty());
    assert!(export.remote_edges.is_empty());
}

#[tokio::test]
async fn cozo_incident_degree_cap_is_a_transactional_storage_invariant() {
    let store = CozoStore::new(4).unwrap();
    let hub = NodeId(Ulid::from(60_000u128));
    let mut initial = vec![(hub, hub)];
    initial.extend(
        (1..MAX_INCIDENT_EDGES - 1)
            .map(|offset| (hub, NodeId(Ulid::from(60_000u128 + offset as u128)))),
    );
    assert_eq!(initial.len(), MAX_INCIDENT_EDGES - 1);
    raw_insert_edges(&store, &initial);

    let boundary = NodeId(Ulid::from(70_000u128));
    store
        .put_edge(&Edge::new(hub, boundary, 0.5, EdgeKind::Associative, 1))
        .await
        .unwrap();
    let overflow = NodeId(Ulid::from(70_001u128));
    let mut overflow_edge = Edge::new(hub, overflow, 0.5, EdgeKind::Associative, 1);
    overflow_edge.anchor = Some(BodySpan::new(10, 20));
    assert!(matches!(
        store.put_edge(&overflow_edge).await,
        Err(Error::CapacityExceeded {
            resource: "incident edge degree",
            limit: MAX_INCIDENT_EDGES,
        })
    ));
    assert!(store.get_edge(hub, overflow).await.unwrap().is_none());
    assert!(store.get_anchor(hub, overflow).await.unwrap().is_none());
    assert!(matches!(
        store
            .put_edge(&Edge::new(overflow, hub, 0.5, EdgeKind::Associative, 1,))
            .await,
        Err(Error::CapacityExceeded {
            resource: "incident edge degree",
            limit: MAX_INCIDENT_EDGES,
        })
    ));

    // Exact-pair updates remain legal at the cap, including the anchor side
    // projection, and a self-loop consumed only one slot.
    let mut updated = Edge::new(hub, hub, 0.9, EdgeKind::Transition, 2);
    updated.anchor = Some(BodySpan::new(3, 9));
    store.put_edge(&updated).await.unwrap();
    assert_eq!(
        store.neighbors(hub, usize::MAX).await.unwrap().len(),
        MAX_INCIDENT_EDGES
    );
    let stored_anchor = store
        .get_edge(hub, hub)
        .await
        .unwrap()
        .unwrap()
        .anchor
        .unwrap();
    assert_eq!((stored_anchor.start, stored_anchor.end), (3, 9));

    store.delete_edge(hub, boundary).await.unwrap();
    store.put_edge(&overflow_edge).await.unwrap();
    let replacement_anchor = store.get_anchor(hub, overflow).await.unwrap().unwrap();
    assert_eq!((replacement_anchor.start, replacement_anchor.end), (10, 20));
    assert_eq!(
        store.neighbors(hub, usize::MAX).await.unwrap().len(),
        MAX_INCIDENT_EDGES
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cozo_edge_inserts_cannot_oversubscribe_a_hub() {
    let store = std::sync::Arc::new(CozoStore::new(4).unwrap());
    let hub = NodeId(Ulid::from(80_000u128));
    let spare = 8;
    let initial: Vec<_> = (0..MAX_INCIDENT_EDGES - spare)
        .map(|offset| (hub, NodeId(Ulid::from(90_000u128 + offset as u128))))
        .collect();
    raw_insert_edges(&store, &initial);

    let mut tasks = Vec::new();
    for offset in 0..(spare * 2) {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            let child = NodeId(Ulid::from(100_000u128 + offset as u128));
            store
                .put_edge(&Edge::new(hub, child, 0.5, EdgeKind::Associative, 1))
                .await
        }));
    }
    let mut inserted = 0;
    let mut rejected = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(()) => inserted += 1,
            Err(Error::CapacityExceeded {
                resource: "incident edge degree",
                limit: MAX_INCIDENT_EDGES,
            }) => rejected += 1,
            Err(error) => panic!("unexpected edge insertion error: {error}"),
        }
    }
    assert_eq!(inserted, spare);
    assert_eq!(rejected, spare);
    assert_eq!(
        store.neighbors(hub, usize::MAX).await.unwrap().len(),
        MAX_INCIDENT_EDGES
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sqlite_instances_recheck_degree_after_write_conflict() {
    let path = std::env::temp_dir().join(format!("mneme-degree-race-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let first = std::sync::Arc::new(CozoStore::open(p, 4).unwrap());
    let hub = NodeId(Ulid::from(105_000u128));
    let initial: Vec<_> = (0..MAX_INCIDENT_EDGES - 1)
        .map(|offset| (hub, NodeId(Ulid::from(106_000u128 + offset as u128))))
        .collect();
    raw_insert_edges(&first, &initial);
    let second = std::sync::Arc::new(raw_persistent_test_store(&path, 4));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

    let tasks: Vec<_> = [first.clone(), second.clone()]
        .into_iter()
        .enumerate()
        .map(|(offset, store)| {
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait();
                let child = NodeId(Ulid::from(108_000u128 + offset as u128));
                store
                    .put_edge(&Edge::new(hub, child, 0.5, EdgeKind::Associative, 1))
                    .await
            })
        })
        .collect();

    let mut inserted = 0;
    let mut rejected = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(()) => inserted += 1,
            Err(Error::CapacityExceeded {
                resource: "incident edge degree",
                limit: MAX_INCIDENT_EDGES,
            }) => rejected += 1,
            Err(error) => panic!("unexpected edge insertion error: {error}"),
        }
    }
    assert_eq!(inserted, 1);
    assert_eq!(rejected, 1);
    assert_eq!(
        first.neighbors(hub, usize::MAX).await.unwrap().len(),
        MAX_INCIDENT_EDGES
    );

    drop(second);
    drop(first);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn persistent_legacy_overfull_hubs_fail_closed_on_read_and_reopen() {
    let path = std::env::temp_dir().join(format!("mneme-overfull-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let store = legacy_persistent_store(p, 4).unwrap();
    let hub = NodeId(Ulid::from(110_000u128));
    let pairs: Vec<_> = (0..=MAX_INCIDENT_EDGES)
        .map(|offset| (hub, NodeId(Ulid::from(120_000u128 + offset as u128))))
        .collect();
    raw_insert_edges(&store, &pairs);
    // Simulate a database from before this invariant was stamped. Supported
    // writers never remove the marker or bypass `put_edge`.
    store.delete_meta(INCIDENT_EDGE_CAP_META_KEY).unwrap();

    assert!(matches!(
        store.neighbors(hub, usize::MAX).await,
        Err(Error::CapacityExceeded {
            resource: "incident edge degree",
            limit: MAX_INCIDENT_EDGES,
        })
    ));
    assert!(matches!(
        store.incident_for(&[hub]).await,
        Err(Error::CapacityExceeded {
            resource: "incident edge degree",
            limit: MAX_INCIDENT_EDGES,
        })
    ));
    assert!(matches!(
        store.ensure_incident_degree_bound(),
        Err(Error::CapacityExceeded {
            resource: "incident edge degree",
            limit: MAX_INCIDENT_EDGES,
        })
    ));
    drop(store);
    assert!(CozoStore::open(p, 4).is_err());
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn bounded_legacy_graph_validator_stamps_only_explicitly() {
    let path = std::env::temp_dir().join(format!("mneme-cap-migration-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let store = legacy_persistent_store(p, 4).unwrap();
    let from = NodeId(Ulid::from(130_000u128));
    let to = NodeId(Ulid::from(130_001u128));
    store
        .put_edge(&Edge::new(from, to, 0.5, EdgeKind::Associative, 1))
        .await
        .unwrap();
    store.delete_meta(INCIDENT_EDGE_CAP_META_KEY).unwrap();
    drop(store);

    assert!(
        CozoStore::open(p, 4).is_err(),
        "normal open cannot silently repair a predecessor"
    );
    let reopened = raw_persistent_test_store(&path, 4);
    reopened.ensure_incident_degree_bound().unwrap();
    assert_eq!(
        reopened.read_meta(INCIDENT_EDGE_CAP_META_KEY).unwrap(),
        Some(MAX_INCIDENT_EDGES.to_string())
    );
    assert!(reopened.get_edge(from, to).await.unwrap().is_some());
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn neighbor_anchor_lookup_only_touches_selected_edge_pairs() {
    let store = CozoStore::new(4).unwrap();
    let root = NodeId(Ulid::from(1u128));
    let selected = NodeId(Ulid::from(2u128));
    let ignored = NodeId(Ulid::from(3u128));
    for id in [root, selected, ignored] {
        store.put_node(&active_node(id)).await.unwrap();
    }

    let mut strong = Edge::new(root, selected, 0.9, EdgeKind::Associative, 1);
    strong.anchor = Some(BodySpan::new(4, 12));
    store.put_edge(&strong).await.unwrap();
    store
        .put_edge(&Edge::new(root, ignored, 0.1, EdgeKind::Associative, 1))
        .await
        .unwrap();

    // The side relation has no foreign key. A corrupt, unrelated row is a
    // useful canary: a global anchor scan would try to parse it and fail.
    store
        .run(
            "?[from, to, start, end] <- \
                 [[\"not-a-node-id\", \"also-not-a-node-id\", 99, 100]] \
                 :put edge_anchor {from, to => start, end}",
            BTreeMap::new(),
            true,
        )
        .unwrap();

    let neighbors = store.neighbors(root, 1).await.unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].node, selected);
    let anchor = neighbors[0].edge.anchor.unwrap();
    assert_eq!((anchor.start, anchor.end), (4, 12));
}

#[tokio::test]
async fn zero_neighbor_cap_performs_no_queries() {
    let store = CozoStore::new(4).unwrap();
    store.reset_query_count();
    assert!(
        store
            .neighbors(NodeId(Ulid::from(1u128)), 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.query_count(), 0);
}

#[tokio::test]
async fn cozo_spread_enforces_a_true_unique_node_cap() {
    crate::tests::assert_spread_node_cap(&CozoStore::new(4).unwrap()).await;
}

#[tokio::test]
async fn spread_queries_scale_with_layers_not_visited_nodes() {
    let store = CozoStore::new(4).unwrap();
    let root = NodeId(Ulid::new());
    store.put_node(&active_node(root)).await.unwrap();

    let mut first = Vec::new();
    for _ in 0..SPREAD_FANOUT {
        let id = NodeId(Ulid::new());
        store.put_node(&active_node(id)).await.unwrap();
        store
            .put_edge(&Edge::new(root, id, 0.5, EdgeKind::Associative, 1))
            .await
            .unwrap();
        first.push(id);
    }
    for parent in &first {
        for _ in 0..SPREAD_FANOUT {
            let id = NodeId(Ulid::new());
            store.put_node(&active_node(id)).await.unwrap();
            store
                .put_edge(&Edge::new(*parent, id, 0.9, EdgeKind::Associative, 1))
                .await
                .unwrap();
        }
    }

    store.reset_query_count();
    let spread = store
        .spread(
            &[Scored {
                id: root,
                score: 1.0,
            }],
            Budget {
                max_nodes: 100,
                max_depth: 2,
                explore: 0.0,
                query_conditioning: 0.0,
                ..Budget::default()
            },
            None,
            TraversalScope::new(StatusFilter::ACTIVE),
        )
        .await
        .unwrap();

    assert_eq!(spread.len(), 1 + SPREAD_FANOUT + SPREAD_FANOUT.pow(2));
    assert_eq!(
        store.query_count(),
        5,
        "one seed-status query plus one edge and one status query per depth layer"
    );
}

#[tokio::test]
async fn cozo_spread_conditions_neighbors_before_final_fanout() {
    let store = CozoStore::new(4).unwrap();
    let root = NodeId(Ulid::from(1u128));
    let relevant = NodeId(Ulid::from(2u128));
    let distractors: Vec<NodeId> = (3u128..11).map(|id| NodeId(Ulid::from(id))).collect();
    for id in std::iter::once(root)
        .chain(std::iter::once(relevant))
        .chain(distractors.iter().copied())
    {
        store.put_node(&active_node(id)).await.unwrap();
    }
    store.upsert(relevant, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
    store
        .put_edge(&Edge::new(root, relevant, 0.5, EdgeKind::Associative, 1))
        .await
        .unwrap();
    for id in distractors {
        store.upsert(id, &[0.0, 1.0, 0.0, 0.0]).await.unwrap();
        store
            .put_edge(&Edge::new(root, id, 0.9, EdgeKind::Associative, 1))
            .await
            .unwrap();
    }

    let spread = store
        .spread(
            &[Scored {
                id: root,
                score: 1.0,
            }],
            Budget {
                max_nodes: 32,
                max_depth: 1,
                min_relevance: 0.0,
                explore: 0.0,
                query_conditioning: 1.0,
                ..Budget::default()
            },
            Some(&[1.0, 0.0, 0.0, 0.0]),
            TraversalScope::new(StatusFilter::ACTIVE),
        )
        .await
        .unwrap();
    assert!(
        spread.iter().any(|hit| hit.id == relevant),
        "the query-relevant ninth raw edge must survive the saturated fanout"
    );
}

#[tokio::test]
async fn cozo_feedback_commit_is_atomic_idempotent_and_keeps_active_projection() {
    let store = CozoStore::new(4).unwrap();
    let a = NodeId(Ulid::from(1u128));
    let b = NodeId(Ulid::from(2u128));
    store.put_node(&active_node(a)).await.unwrap();
    let ordinary = Node::try_new(
        b,
        "ordinary",
        BodyRef::new("inline://x").unwrap(),
        std::iter::empty::<&str>(),
        Provenance::derived_empty(),
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    store.put_node(&ordinary).await.unwrap();
    store.upsert(b, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
    let mut edge = Edge::new(a, b, 0.2, EdgeKind::Associative, 1);
    edge.anchor = Some(BodySpan::new(2, 5));
    store.put_edge(&edge).await.unwrap();

    let mut replacement = ordinary.clone();
    replacement.record_grounded_use(10);
    let mut replacement_edge = edge.clone();
    replacement_edge.reinforce(10, &mneme_core::StrengthParams::default());
    let commit = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:receipt",
            "payload:a",
            "cozo-epoch",
            1,
            1,
        )),
        applied_at: 10,
        nodes: vec![FeedbackNodeUpdate {
            expected: ordinary,
            replacement,
        }],
        edges: vec![FeedbackEdgeUpdate {
            expected: Some(edge),
            replacement: replacement_edge,
        }],
        merge_observations: vec![FeedbackMergeObservation::new(a, b).unwrap()],
    };
    assert_eq!(
        store.commit_feedback(&commit).await.unwrap(),
        FeedbackCommitOutcome::Applied
    );
    assert_eq!(
        store.commit_feedback(&commit).await.unwrap(),
        FeedbackCommitOutcome::AlreadyApplied
    );
    let unrelated = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:unrelated",
            "payload:unrelated",
            "cozo-epoch",
            2,
            1,
        )),
        applied_at: i64::MAX as Timestamp,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    assert_eq!(
        store.commit_feedback(&unrelated).await.unwrap(),
        FeedbackCommitOutcome::Applied
    );
    let mut after_clock_jump = commit.clone();
    after_clock_jump.applied_at = 0;
    assert_eq!(
        store.commit_feedback(&after_clock_jump).await.unwrap(),
        FeedbackCommitOutcome::AlreadyApplied,
        "unrelated forward/back wall-clock values cannot collect a reachable proof"
    );
    let node = store.get_node(b).await.unwrap().unwrap();
    assert!(matches!(node.status(), NodeStatus::Active));
    assert_eq!(node.grounded_use_count(), 1);
    assert_eq!(node.confidence(), 0.5);
    assert_eq!(node.stability(), 0.5);
    assert_eq!(node.interference(), 0);
    let candidates = store
        .open_merge_candidates(ColdPath::acquire())
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].between, UnorderedPair(a, b));
    assert_eq!(candidates[0].observations, 1, "exact replay is a no-op");
    assert_eq!(
        store
            .ann(&[1.0, 0.0, 0.0, 0.0], 1, StatusFilter::ACTIVE)
            .await
            .unwrap()[0]
            .id,
        b,
        "feedback must not desynchronize the active side index"
    );
    assert_eq!(
        store.get_anchor(a, b).await.unwrap(),
        Some(BodySpan::new(2, 5))
    );

    let mut mismatch = commit.clone();
    mismatch.idempotency.as_mut().unwrap().fingerprint =
        feedback_idempotency("unused", "payload:b", "cozo-epoch", 1, 1).fingerprint;
    assert!(matches!(
        store.commit_feedback(&mismatch).await,
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(
        store
            .get_node(b)
            .await
            .unwrap()
            .unwrap()
            .grounded_use_count(),
        1
    );
    assert_eq!(
        store
            .open_merge_candidates(ColdPath::acquire())
            .await
            .unwrap()[0]
            .observations,
        1,
        "same-key/different-payload rejection cannot bump the overlay"
    );
}

#[tokio::test]
async fn cozo_feedback_replay_identity_is_epoch_and_key() {
    let store = CozoStore::new(4).unwrap();
    let id = NodeId(Ulid::from(74_101u128));
    store.put_node(&active_node(id)).await.unwrap();
    let commit = |payload, epoch| FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:epoch-collision",
            payload,
            epoch,
            1,
            1,
        )),
        applied_at: 1,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };

    let epoch_a = commit("same-path-effects", "epoch-a");
    assert_eq!(
        store.commit_feedback(&epoch_a).await.unwrap(),
        FeedbackCommitOutcome::Applied
    );
    let mut epoch_b = commit("same-path-effects", "epoch-b");
    let expected = store.get_node(id).await.unwrap().unwrap();
    let mut replacement = expected.clone();
    replacement.record_grounded_use(2);
    epoch_b.nodes.push(FeedbackNodeUpdate {
        expected,
        replacement,
    });
    assert_eq!(
        store.commit_feedback(&epoch_b).await.unwrap(),
        FeedbackCommitOutcome::Applied,
        "epoch A's matching bare key and fingerprint is not epoch B's replay"
    );
    assert_eq!(
        store
            .get_node(id)
            .await
            .unwrap()
            .unwrap()
            .grounded_use_count(),
        1
    );
    assert_eq!(
        store.commit_feedback(&epoch_b).await.unwrap(),
        FeedbackCommitOutcome::AlreadyApplied
    );
    assert!(matches!(
        store
            .commit_feedback(&commit("different-path-effects", "epoch-b"))
            .await,
        Err(Error::InvalidInput(_))
    ));

    let epoch_c = commit("different-path-effects", "epoch-c");
    assert_eq!(
        store.commit_feedback(&epoch_c).await.unwrap(),
        FeedbackCommitOutcome::Applied,
        "bare-key payload reuse is legal in a different epoch"
    );
    let export = store.export().await.unwrap();
    assert_eq!(export.feedback_retries.len(), 1);
    assert_eq!(export.feedback_retries[0].key, "cozo:epoch-collision");
    assert_eq!(export.feedback_retries[0].epoch, "epoch-c");
    assert_eq!(
        export.feedback_retries[0].fingerprint,
        epoch_c.idempotency.unwrap().fingerprint
    );
}

#[tokio::test]
async fn cozo_feedback_ambiguous_epoch_ownership_fails_closed() {
    let store = CozoStore::new(4).unwrap();
    let id = NodeId(Ulid::from(74_102u128));
    store.put_node(&active_node(id)).await.unwrap();
    let epoch_a = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:ambiguous-epoch",
            "same-path-effects",
            "epoch-a",
            1,
            1,
        )),
        applied_at: 1,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    store.commit_feedback(&epoch_a).await.unwrap();

    let mut duplicate = BTreeMap::new();
    duplicate.insert("key".into(), dv_str("cozo:ambiguous-epoch"));
    duplicate.insert("epoch".into(), dv_str("epoch-b"));
    store
        .run(
            "?[epoch, sequence, key, marker] <- [[$epoch, 1, $key, true]] \
             :put feedback_retry_order {epoch, sequence, key => marker}",
            duplicate,
            true,
        )
        .unwrap();

    let expected = store.get_node(id).await.unwrap().unwrap();
    let mut replacement = expected.clone();
    replacement.record_grounded_use(2);
    let epoch_b = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:ambiguous-epoch",
            "same-path-effects",
            "epoch-b",
            1,
            1,
        )),
        applied_at: 2,
        nodes: vec![FeedbackNodeUpdate {
            expected: expected.clone(),
            replacement,
        }],
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    let error = store.commit_feedback(&epoch_b).await.unwrap_err();
    assert!(matches!(&error, Error::Backend(_)));
    assert!(
        error
            .to_string()
            .contains("no unique proof-to-order authority")
    );
    assert_eq!(
        store
            .get_node(id)
            .await
            .unwrap()
            .unwrap()
            .grounded_use_count(),
        expected.grounded_use_count(),
        "ambiguous epoch ownership must not manufacture a replay or apply a graph prefix"
    );
    let unrelated = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:unrelated-while-ambiguous",
            "unrelated-payload",
            "epoch-b",
            2,
            1,
        )),
        applied_at: 3,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    assert!(matches!(
        store.commit_feedback(&unrelated).await,
        Err(Error::Backend(_))
    ));
    let export = store.export().await.unwrap();
    assert_eq!(export.feedback_retries.len(), 2);
    assert!(export.feedback_retries.iter().all(|proof| {
        proof.key == "cozo:ambiguous-epoch"
            && proof.fingerprint == epoch_a.idempotency.as_ref().unwrap().fingerprint
    }));
}

#[tokio::test]
async fn cozo_feedback_invalid_stored_sequences_never_authorize_replay() {
    for (label, sequence) in [("negative", -1), ("zero", 0), ("reserved-max", i64::MAX)] {
        let store = CozoStore::new(4).unwrap();
        let key = format!("cozo:invalid-sequence:{label}");
        let commit = FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                &key,
                "same-path-effects",
                "current-epoch",
                1,
                1,
            )),
            applied_at: 1,
            nodes: Vec::new(),
            edges: Vec::new(),
            merge_observations: Vec::new(),
        };
        let fingerprint = commit.idempotency.as_ref().unwrap().fingerprint.clone();
        let mut params = BTreeMap::new();
        params.insert("key".into(), dv_str(&key));
        params.insert("fingerprint".into(), dv_str(&fingerprint));
        params.insert("epoch".into(), dv_str("current-epoch"));
        params.insert("sequence".into(), dv_int(sequence));
        store
            .run(
                "?[key, fingerprint, applied_at] <- [[$key, $fingerprint, $sequence]] \
                 :put feedback_retry {key => fingerprint, applied_at}",
                params.clone(),
                true,
            )
            .unwrap();
        store
            .run(
                "?[epoch, sequence, key, marker] <- [[$epoch, $sequence, $key, true]] \
                 :put feedback_retry_order {epoch, sequence, key => marker}",
                params,
                true,
            )
            .unwrap();

        let error = store.commit_feedback(&commit).await.unwrap_err();
        assert!(
            matches!(&error, Error::Backend(_))
                && error
                    .to_string()
                    .contains("no unique proof-to-order authority"),
            "{label} stored sequence unexpectedly gained replay authority: {error}"
        );
    }
}

#[tokio::test]
async fn cozo_feedback_orphan_proof_fails_closed_then_is_reclaimed_by_new_work() {
    let store = CozoStore::new(4).unwrap();
    let orphan = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:orphan-proof",
            "orphan-payload",
            "epoch-a",
            1,
            1,
        )),
        applied_at: 1,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    store.commit_feedback(&orphan).await.unwrap();
    let mut remove = BTreeMap::new();
    remove.insert("key".into(), dv_str("cozo:orphan-proof"));
    store
        .run(
            "?[epoch, sequence, key] := *feedback_retry_order{epoch, sequence, key}, key == $key \
             :rm feedback_retry_order {epoch, sequence, key}",
            remove,
            true,
        )
        .unwrap();

    let error = store.commit_feedback(&orphan).await.unwrap_err();
    assert!(matches!(&error, Error::Backend(_)));
    assert!(
        error
            .to_string()
            .contains("no unique proof-to-order authority")
    );

    let fresh = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:fresh-after-orphan",
            "fresh-payload",
            "epoch-a",
            2,
            1,
        )),
        applied_at: 2,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    assert_eq!(
        store.commit_feedback(&fresh).await.unwrap(),
        FeedbackCommitOutcome::Applied
    );
    let export = store.export().await.unwrap();
    assert_eq!(export.feedback_retries.len(), 1);
    assert_eq!(export.feedback_retries[0].key, "cozo:fresh-after-orphan");

    let dangling_store = CozoStore::new(4).unwrap();
    dangling_store.commit_feedback(&orphan).await.unwrap();
    let mut remove = BTreeMap::new();
    remove.insert("key".into(), dv_str("cozo:orphan-proof"));
    dangling_store
        .run(
            "?[key] := *feedback_retry{key}, key == $key :rm feedback_retry {key}",
            remove,
            true,
        )
        .unwrap();
    assert_eq!(
        dangling_store.commit_feedback(&fresh).await.unwrap(),
        FeedbackCommitOutcome::Applied,
        "a dangling order row without a proof has no replay authority"
    );
    let order_rows = dangling_store
        .run(
            "?[epoch, sequence, key] := *feedback_retry_order{epoch, sequence, key} \
             :order epoch, sequence, key",
            BTreeMap::new(),
            false,
        )
        .unwrap();
    assert_eq!(order_rows.rows.len(), 1);
    assert_eq!(
        want_str(&order_rows.rows[0][2]).unwrap(),
        "cozo:fresh-after-orphan"
    );
}

#[tokio::test]
async fn cozo_feedback_cas_accepts_legacy_node_json_defaults() {
    let store = CozoStore::new(4).unwrap();
    let id = NodeId(Ulid::from(1u128));
    let mut legacy = serde_json::to_value(active_node(id)).unwrap();
    let object = legacy.as_object_mut().unwrap();
    for field in [
        "body_ownership",
        "origin_commit",
        "last_exposed",
        "exposure_count",
        "last_grounded_use",
        "grounded_use_count",
        "interference",
    ] {
        object.remove(field);
    }
    let mut params = BTreeMap::new();
    params.insert("id".into(), dv_str(&id.0.to_string()));
    params.insert(
        "data".into(),
        dv_str(&serde_json::to_string(&legacy).unwrap()),
    );
    store
        .run(
            "?[id, data, status] <- [[$id, $data, 'active']] \
                 :put node {id => data, status}",
            params,
            true,
        )
        .unwrap();

    let expected = store.get_node(id).await.unwrap().unwrap();
    let mut replacement = expected.clone();
    replacement.record_grounded_use(10);
    let commit = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:legacy-json",
            "payload",
            "legacy-test-epoch",
            1,
            1,
        )),
        applied_at: 10,
        nodes: vec![FeedbackNodeUpdate {
            expected,
            replacement,
        }],
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    assert_eq!(
        store.commit_feedback(&commit).await.unwrap(),
        FeedbackCommitOutcome::Applied
    );
    assert_eq!(
        store
            .get_node(id)
            .await
            .unwrap()
            .unwrap()
            .grounded_use_count(),
        1
    );
}

#[tokio::test]
async fn cozo_feedback_anchor_cas_and_capacity_failure_roll_back_every_row() {
    let store = CozoStore::new(4).unwrap();
    let hub = NodeId(Ulid::from(1u128));
    let first = NodeId(Ulid::from(2u128));
    let second = NodeId(Ulid::from(3u128));
    for id in [hub, first, second] {
        store.put_node(&active_node(id)).await.unwrap();
    }
    let mut expected_edge = Edge::new(hub, first, 0.2, EdgeKind::Associative, 1);
    expected_edge.anchor = Some(BodySpan::new(1, 2));
    store.put_edge(&expected_edge).await.unwrap();
    let mut replacement_edge = expected_edge.clone();
    replacement_edge.reinforce(10, &mneme_core::StrengthParams::default());
    let expected_node = store.get_node(first).await.unwrap().unwrap();
    let mut replacement_node = expected_node.clone();
    replacement_node.record_grounded_use(2);

    let mut concurrent = expected_edge.clone();
    concurrent.anchor = Some(BodySpan::new(4, 8));
    store.put_edge(&concurrent).await.unwrap();
    let anchor_stale = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:anchor",
            "payload",
            "rollback-epoch",
            1,
            1,
        )),
        applied_at: 10,
        nodes: vec![FeedbackNodeUpdate {
            expected: expected_node.clone(),
            replacement: replacement_node,
        }],
        edges: vec![FeedbackEdgeUpdate {
            expected: Some(expected_edge),
            replacement: replacement_edge,
        }],
        merge_observations: vec![FeedbackMergeObservation::new(hub, first).unwrap()],
    };
    assert!(matches!(
        store.commit_feedback(&anchor_stale).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        store.get_anchor(hub, first).await.unwrap(),
        concurrent.anchor
    );
    assert_eq!(
        store
            .get_node(first)
            .await
            .unwrap()
            .unwrap()
            .grounded_use_count(),
        expected_node.grounded_use_count(),
        "an edge CAS failure rolls back the earlier staged node write"
    );
    assert!(
        store
            .open_merge_candidates(ColdPath::acquire())
            .await
            .unwrap()
            .is_empty(),
        "an edge CAS failure cannot publish the later merge observation"
    );

    let existing: Vec<_> = (10u128..10 + (MAX_INCIDENT_EDGES - 1) as u128)
        .map(|id| (hub, NodeId(Ulid::from(id))))
        .collect();
    raw_insert_edges(&store, &existing);
    let capacity = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:capacity",
            "payload",
            "rollback-epoch",
            2,
            1,
        )),
        applied_at: 11,
        nodes: vec![FeedbackNodeUpdate {
            expected: expected_node.clone(),
            replacement: {
                let mut node = expected_node.clone();
                node.record_grounded_use(2);
                node
            },
        }],
        edges: vec![
            FeedbackEdgeUpdate {
                expected: None,
                replacement: Edge::new(hub, second, 0.1, EdgeKind::Transition, 11),
            },
            FeedbackEdgeUpdate {
                expected: None,
                replacement: Edge::new(first, second, 0.1, EdgeKind::Transition, 11),
            },
        ],
        merge_observations: vec![FeedbackMergeObservation::new(hub, second).unwrap()],
    };
    assert!(matches!(
        store.commit_feedback(&capacity).await,
        Err(Error::CapacityExceeded { .. })
    ));
    assert!(store.get_edge(hub, second).await.unwrap().is_none());
    assert!(store.get_edge(first, second).await.unwrap().is_none());
    assert_eq!(
        store
            .get_node(first)
            .await
            .unwrap()
            .unwrap()
            .grounded_use_count(),
        expected_node.grounded_use_count()
    );
    assert!(
        store
            .open_merge_candidates(ColdPath::acquire())
            .await
            .unwrap()
            .is_empty(),
        "capacity rejection cannot publish the later merge observation"
    );
}

#[tokio::test]
async fn cozo_feedback_malformed_merge_state_rolls_back_the_whole_commit() {
    let malformed = [
        (
            "zero-count",
            false,
            dv_int(0),
            dv_int(1),
            dv_int(1),
            DataValue::Null,
        ),
        (
            "reversed-time",
            false,
            dv_int(1),
            dv_int(2),
            dv_int(1),
            DataValue::Null,
        ),
        (
            "unknown-resolution",
            false,
            dv_int(1),
            dv_int(1),
            dv_int(1),
            dv_str("bogus"),
        ),
        (
            "reversed-key",
            true,
            dv_int(1),
            dv_int(1),
            dv_int(1),
            DataValue::Null,
        ),
    ];

    for (label, reversed_key, observations, first_seen, last_seen, resolution) in malformed {
        let store = CozoStore::new(4).unwrap();
        let a = NodeId(Ulid::from(1u128));
        let b = NodeId(Ulid::from(2u128));
        store.put_node(&active_node(a)).await.unwrap();
        store.put_node(&active_node(b)).await.unwrap();

        let (lo, hi) = canonical(UnorderedPair(a, b));
        let (stored_lo, stored_hi) = if reversed_key {
            (hi.as_str(), lo.as_str())
        } else {
            (lo.as_str(), hi.as_str())
        };
        let mut params = BTreeMap::new();
        params.insert("lo".into(), dv_str(stored_lo));
        params.insert("hi".into(), dv_str(stored_hi));
        params.insert("observations".into(), observations.clone());
        params.insert("first_seen".into(), first_seen.clone());
        params.insert("last_seen".into(), last_seen.clone());
        params.insert("resolution".into(), resolution.clone());
        store
            .run(
                "?[lo, hi, observations, first_seen, last_seen, resolution] <- \
                     [[$lo, $hi, $observations, $first_seen, $last_seen, $resolution]] \
                     :put merge_candidate \
                       {lo, hi => observations, first_seen, last_seen, resolution}",
                params,
                true,
            )
            .unwrap();

        let expected = store.get_node(b).await.unwrap().unwrap();
        let mut replacement = expected.clone();
        replacement.record_grounded_use(10);
        let commit = FeedbackCommit {
            idempotency: Some(feedback_idempotency(
                &format!("cozo:malformed-merge:{label}"),
                label,
                "malformed-merge-epoch",
                1,
                1,
            )),
            applied_at: 10,
            nodes: vec![FeedbackNodeUpdate {
                expected: expected.clone(),
                replacement,
            }],
            edges: vec![FeedbackEdgeUpdate {
                expected: None,
                replacement: Edge::new(a, b, 0.2, EdgeKind::Transition, 10),
            }],
            merge_observations: vec![FeedbackMergeObservation::new(a, b).unwrap()],
        };
        assert!(
            matches!(store.commit_feedback(&commit).await, Err(Error::Backend(_))),
            "{label} stored merge state unexpectedly committed"
        );
        assert_eq!(
            store
                .get_node(b)
                .await
                .unwrap()
                .unwrap()
                .grounded_use_count(),
            expected.grounded_use_count(),
            "{label} merge failure did not roll back the staged node write"
        );
        assert!(store.get_edge(a, b).await.unwrap().is_none());
        for query in [
            "?[key] := *feedback_retry{key}",
            "?[key] := *feedback_retry_order{key}",
        ] {
            assert!(
                store
                    .run(query, BTreeMap::new(), false)
                    .unwrap()
                    .rows
                    .is_empty(),
                "{label} merge failure published retry authority"
            );
        }
        let raw = store
            .run(
                "?[observations, first_seen, last_seen, resolution] := \
                 *merge_candidate{lo: $lo, hi: $hi, observations, first_seen, last_seen, resolution}",
                BTreeMap::from([
                    ("lo".into(), dv_str(stored_lo)),
                    ("hi".into(), dv_str(stored_hi)),
                ]),
                false,
            )
            .unwrap();
        assert_eq!(
            raw.rows,
            vec![vec![observations, first_seen, last_seen, resolution]],
            "{label} merge failure rewrote the malformed source row"
        );
    }
}

#[tokio::test]
async fn cozo_feedback_terminal_dangling_merge_observation_succeeds() {
    let store = CozoStore::new(4).unwrap();
    let a = NodeId(Ulid::from(74_201u128));
    let b = NodeId(Ulid::from(74_202u128));
    let pair = UnorderedPair(a, b);
    store.put_node(&active_node(a)).await.unwrap();
    store.put_node(&active_node(b)).await.unwrap();
    store.observe_merge_candidate(a, b, 1).await.unwrap();
    store
        .resolve_merge_candidate(pair, MergeResolution::Keep)
        .await
        .unwrap();
    store.delete_node(b).await.unwrap();

    let commit = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:terminal-dangling-merge",
            "terminal-dangling-merge",
            "terminal-dangling-epoch",
            1,
            1,
        )),
        applied_at: 2,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: vec![FeedbackMergeObservation::new(a, b).unwrap()],
    };
    assert_eq!(
        store.commit_feedback(&commit).await.unwrap(),
        FeedbackCommitOutcome::Applied
    );
    assert_eq!(
        store.commit_feedback(&commit).await.unwrap(),
        FeedbackCommitOutcome::AlreadyApplied
    );

    let export = store.export().await.unwrap();
    let candidate = export
        .merges
        .iter()
        .find(|candidate| candidate.between == pair)
        .unwrap();
    assert_eq!(candidate.observations, 2);
    assert_eq!(candidate.last_seen, 2);
    assert_eq!(candidate.resolution, Some(MergeResolution::Keep));
    assert!(store.get_node(b).await.unwrap().is_none());
}

#[tokio::test]
async fn cozo_feedback_open_dangling_merge_observation_rolls_back() {
    let store = CozoStore::new(4).unwrap();
    let a = NodeId(Ulid::from(74_203u128));
    let missing = NodeId(Ulid::from(74_204u128));
    store.put_node(&active_node(a)).await.unwrap();
    let expected = store.get_node(a).await.unwrap().unwrap();
    let mut replacement = expected.clone();
    replacement.record_grounded_use(2);
    let commit = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:open-dangling-merge",
            "open-dangling-merge",
            "open-dangling-epoch",
            1,
            1,
        )),
        applied_at: 2,
        nodes: vec![FeedbackNodeUpdate {
            expected: expected.clone(),
            replacement,
        }],
        edges: Vec::new(),
        merge_observations: vec![FeedbackMergeObservation::new(a, missing).unwrap()],
    };
    assert!(matches!(
        store.commit_feedback(&commit).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        store
            .get_node(a)
            .await
            .unwrap()
            .unwrap()
            .grounded_use_count(),
        expected.grounded_use_count(),
        "missing open-overlay endpoint did not roll back the staged node write"
    );
    assert!(
        store
            .run("?[lo] := *merge_candidate{lo}", BTreeMap::new(), false,)
            .unwrap()
            .rows
            .is_empty()
    );
    for query in [
        "?[key] := *feedback_retry{key}",
        "?[key] := *feedback_retry_order{key}",
    ] {
        assert!(
            store
                .run(query, BTreeMap::new(), false)
                .unwrap()
                .rows
                .is_empty(),
            "missing open-overlay endpoint published retry authority"
        );
    }
}

#[tokio::test]
async fn cozo_feedback_reachability_survives_holes_until_new_epoch_after_reopen() {
    let path = std::env::temp_dir().join(format!("mneme-feedback-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let store = CozoStore::open(p, 4).unwrap();

    let oldest = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:oldest",
            "payload:oldest",
            "server-epoch-a",
            1,
            1,
        )),
        applied_at: i64::MAX as Timestamp,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    let late = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:late",
            "payload:late",
            "server-epoch-a",
            100,
            1,
        )),
        applied_at: 0,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    let middle = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:middle",
            "payload:middle",
            "server-epoch-a",
            2,
            1,
        )),
        applied_at: 1,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    for commit in [&oldest, &late, &middle] {
        assert_eq!(
            store.commit_feedback(commit).await.unwrap(),
            FeedbackCommitOutcome::Applied
        );
    }
    assert_eq!(
        store.commit_feedback(&oldest).await.unwrap(),
        FeedbackCommitOutcome::AlreadyApplied,
        "the oldest live capability pins its proof despite holes and out-of-order commits"
    );
    assert_eq!(store.export().await.unwrap().feedback_retries.len(), 3);

    store.prepare_for_file_move().unwrap();
    drop(store);
    let reopened = reopen_current_test_store(&path).unwrap();
    // Existing-only admission is read-only. It does not make historical rows
    // live authority: the host must explicitly activate this handle's epoch.
    assert_eq!(reopened.export().await.unwrap().feedback_retries.len(), 3);
    assert!(
        matches!(reopened.commit_feedback(&oldest).await, Err(Error::Conflict(message))
        if message.contains("not activated"))
    );
    assert_eq!(
        reopened.activate_feedback_epoch("server-epoch-b").unwrap(),
        3
    );
    assert!(reopened.export().await.unwrap().feedback_retries.is_empty());
    assert!(
        matches!(reopened.commit_feedback(&oldest).await, Err(Error::Conflict(message))
        if message.contains("does not match"))
    );
    assert_eq!(
        reopened.activate_feedback_epoch("server-epoch-b").unwrap(),
        0
    );

    let restarted = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:restart",
            "payload:restart",
            "server-epoch-b",
            1,
            1,
        )),
        applied_at: 2,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    assert_eq!(
        reopened.commit_feedback(&restarted).await.unwrap(),
        FeedbackCommitOutcome::Applied
    );
    let export = reopened.export().await.unwrap();
    assert_eq!(export.feedback_retries.len(), 1);
    assert_eq!(export.feedback_retries[0].key, "cozo:restart");
    assert_eq!(export.feedback_retries[0].epoch, "server-epoch-b");
    assert_eq!(export.feedback_retries[0].sequence, 1);

    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn cozo_feedback_reopen_refuses_torn_v1_ledger_without_repair() {
    let path = std::env::temp_dir().join(format!("mneme-feedback-v1-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let store = CozoStore::open(p, 4).unwrap();
    let mut legacy = BTreeMap::new();
    legacy.insert("key".into(), dv_str("cozo:legacy-proof"));
    legacy.insert("fingerprint".into(), dv_str(&"a".repeat(64)));
    legacy.insert("applied_at".into(), dv_int(123));
    store
        .run(
            "?[key, fingerprint, applied_at] <- \
                   [[$key, $fingerprint, $applied_at]] \
                 :put feedback_retry {key => fingerprint, applied_at}",
            legacy,
            true,
        )
        .unwrap();
    store
        .run("::remove feedback_retry_order", BTreeMap::new(), true)
        .unwrap();
    drop(store);

    assert!(reopen_current_test_store(&path).is_err());
    let raw = raw_persistent_test_store(&path, 4);
    assert!(!raw.relation_exists("feedback_retry_order").unwrap());
    let rows = raw
        .run(
            "?[key, fingerprint, applied_at] := *feedback_retry{key, fingerprint, applied_at}",
            BTreeMap::new(),
            false,
        )
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![
            dv_str("cozo:legacy-proof"),
            dv_str(&"a".repeat(64)),
            dv_int(123)
        ]]
    );
    drop(raw);
    remove_sqlite_test_files(&path);
}

#[tokio::test]
async fn cozo_import_drops_live_source_feedback_proofs() {
    let source = crate::MemStore::new(4);
    let commit = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "source:proof",
            "payload",
            "source-authority",
            1,
            1,
        )),
        applied_at: 1,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    source.commit_feedback(&commit).await.unwrap();
    let expected = source.export();
    assert_eq!(expected.feedback_retries.len(), 1);

    let mut destination = CozoStore::new(4).unwrap();
    destination.import_mem(&source).await.unwrap();
    assert!(
        destination
            .export()
            .await
            .unwrap()
            .feedback_retries
            .is_empty()
    );
    destination.verify_import(&expected).await.unwrap();
}

#[tokio::test]
async fn cozo_import_rejects_false_merge_proof_before_writing() {
    let winner = NodeId(Ulid::from(900_000u128));
    let loser = NodeId(Ulid::from(900_001u128));
    let false_source = crate::MemStore::new(4);
    {
        let record = FullMergeRecord::new(winner, loser, 1);
        false_source
            .lock()
            .full_merge_commits
            .insert(record.between, record);
    }
    let mut destination = CozoStore::new(4).unwrap();
    assert!(matches!(
        destination.import_mem(&false_source).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(destination.ensure_empty_import_target().is_ok());
}

#[tokio::test]
async fn cozo_feedback_retry_capacity_reclaims_only_below_the_live_floor() {
    let store = CozoStore::new(4).unwrap();
    raw_insert_feedback_retries(&store, "capacity-epoch", MAX_FEEDBACK_RETRY_RECORDS);
    let mut commit = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:capacity-retry",
            "payload:new",
            "capacity-epoch",
            MAX_FEEDBACK_RETRY_RECORDS as u64 + 1,
            1,
        )),
        applied_at: 0,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    assert!(matches!(
        store.commit_feedback(&commit).await,
        Err(Error::CapacityExceeded {
            resource: "feedback retry records",
            limit: MAX_FEEDBACK_RETRY_RECORDS,
        })
    ));
    assert_eq!(
        store.export().await.unwrap().feedback_retries.len(),
        MAX_FEEDBACK_RETRY_RECORDS,
        "a rejected transaction must roll back its attempted reclamation"
    );

    commit.idempotency.as_mut().unwrap().retry.min_live_sequence =
        MAX_FEEDBACK_RETRY_RECORDS as u64 + 1;
    assert_eq!(
        store.commit_feedback(&commit).await.unwrap(),
        FeedbackCommitOutcome::Applied
    );
    let export = store.export().await.unwrap();
    assert_eq!(export.feedback_retries.len(), 1);
    assert_eq!(export.feedback_retries[0].key, "cozo:capacity-retry");

    let collision_store = CozoStore::new(4).unwrap();
    raw_insert_feedback_retries(
        &collision_store,
        "old-capacity-epoch",
        MAX_FEEDBACK_RETRY_RECORDS,
    );
    let cross_epoch_collision = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "retry:0",
            "payload:replacement",
            "new-capacity-epoch",
            1,
            1,
        )),
        applied_at: 1,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    assert_eq!(
        collision_store
            .commit_feedback(&cross_epoch_collision)
            .await
            .unwrap(),
        FeedbackCommitOutcome::Applied,
        "a colliding old-epoch key at the ceiling is reclaimable"
    );
    let export = collision_store.export().await.unwrap();
    assert_eq!(export.feedback_retries.len(), 1);
    assert_eq!(export.feedback_retries[0].key, "retry:0");
    assert_eq!(export.feedback_retries[0].epoch, "new-capacity-epoch");

    let orphan_store = CozoStore::new(4).unwrap();
    raw_insert_feedback_retries(
        &orphan_store,
        "orphan-capacity-epoch",
        MAX_FEEDBACK_RETRY_RECORDS,
    );
    let mut remove = BTreeMap::new();
    remove.insert("key".into(), dv_str("retry:0"));
    orphan_store
        .run(
            "?[epoch, sequence, key] := *feedback_retry_order{epoch, sequence, key}, key == $key \
             :rm feedback_retry_order {epoch, sequence, key}",
            remove,
            true,
        )
        .unwrap();
    let replacement = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:orphan-capacity-replacement",
            "payload:replacement",
            "orphan-capacity-epoch",
            MAX_FEEDBACK_RETRY_RECORDS as u64 + 1,
            1,
        )),
        applied_at: 2,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    assert_eq!(
        orphan_store.commit_feedback(&replacement).await.unwrap(),
        FeedbackCommitOutcome::Applied,
        "an orphan proof cannot consume retry capacity forever"
    );
    let export = orphan_store.export().await.unwrap();
    assert_eq!(export.feedback_retries.len(), MAX_FEEDBACK_RETRY_RECORDS);
    assert!(
        export
            .feedback_retries
            .iter()
            .all(|proof| proof.key != "retry:0")
    );
    assert!(
        export
            .feedback_retries
            .iter()
            .any(|proof| proof.key == "cozo:orphan-capacity-replacement")
    );
}

#[tokio::test]
async fn dropped_cozo_feedback_transaction_is_a_crash_safe_rollback() {
    let store = CozoStore::new(4).unwrap();
    let a = NodeId(Ulid::from(1u128));
    let b = NodeId(Ulid::from(2u128));
    store.put_node(&active_node(a)).await.unwrap();
    store.put_node(&active_node(b)).await.unwrap();
    let old_proof = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:crash-collision",
            "payload:old",
            "old-server-epoch",
            1,
            1,
        )),
        applied_at: 9,
        nodes: Vec::new(),
        edges: Vec::new(),
        merge_observations: Vec::new(),
    };
    assert_eq!(
        store.commit_feedback(&old_proof).await.unwrap(),
        FeedbackCommitOutcome::Applied
    );
    let expected = store.get_node(b).await.unwrap().unwrap();
    let mut replacement = expected.clone();
    replacement.record_grounded_use(10);
    let commit = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:crash-collision",
            "payload:new",
            "new-server-epoch",
            1,
            1,
        )),
        applied_at: 10,
        nodes: vec![FeedbackNodeUpdate {
            expected: expected.clone(),
            replacement,
        }],
        edges: vec![FeedbackEdgeUpdate {
            expected: None,
            replacement: Edge::new(a, b, 0.2, EdgeKind::Transition, 10),
        }],
        merge_observations: Vec::new(),
    };

    let tx = store.db.multi_transaction(true);
    assert_eq!(
        stage_feedback_transaction(&tx, &commit).unwrap(),
        FeedbackCommitOutcome::Applied
    );
    drop(tx); // process/task disappears before the one commit point

    assert_eq!(
        store
            .get_node(b)
            .await
            .unwrap()
            .unwrap()
            .grounded_use_count(),
        0
    );
    assert!(store.get_edge(a, b).await.unwrap().is_none());
    let export = store.export().await.unwrap();
    assert_eq!(export.feedback_retries.len(), 1);
    assert_eq!(export.feedback_retries[0].key, "cozo:crash-collision");
    assert_eq!(export.feedback_retries[0].epoch, "old-server-epoch");
    assert_eq!(
        export.feedback_retries[0].fingerprint,
        old_proof.idempotency.as_ref().unwrap().fingerprint,
        "dropping the staged bare-key replacement must restore the old proof"
    );
    assert_eq!(
        store.commit_feedback(&old_proof).await.unwrap(),
        FeedbackCommitOutcome::AlreadyApplied,
        "dropping a staged transaction must roll back epoch GC too"
    );
}

#[tokio::test]
async fn cozo_feedback_commit_accepts_the_exact_batch_ceiling() {
    let store = CozoStore::new(4).unwrap();
    let hub = NodeId(Ulid::from(1u128));
    let targets: Vec<_> = (2..=mneme_core::MAX_FEEDBACK_BATCH_EVENTS as u128 + 1)
        .map(|value| NodeId(Ulid::from(value)))
        .collect();
    let mut nodes = Vec::with_capacity(targets.len() + 1);
    nodes.push(hub);
    nodes.extend(targets.iter().copied());
    raw_insert_nodes(&store, &nodes);
    let pairs: Vec<_> = targets.iter().map(|target| (hub, *target)).collect();
    raw_insert_edges(&store, &pairs);

    let updates = targets
        .iter()
        .map(|target| {
            let expected =
                Edge::from_stored(hub, *target, EdgeKind::Associative, None, 0.5, 1, 1, 0);
            let mut replacement = expected.clone();
            replacement.reinforce(10, &mneme_core::StrengthParams::default());
            FeedbackEdgeUpdate {
                expected: Some(expected),
                replacement,
            }
        })
        .collect();
    let commit = FeedbackCommit {
        idempotency: Some(feedback_idempotency(
            "cozo:max",
            "payload",
            "max-epoch",
            1,
            1,
        )),
        applied_at: 10,
        nodes: Vec::new(),
        edges: updates,
        merge_observations: Vec::new(),
    };
    assert_eq!(commit.edges.len(), mneme_core::MAX_FEEDBACK_BATCH_EVENTS);
    assert_eq!(
        store.commit_feedback(&commit).await.unwrap(),
        FeedbackCommitOutcome::Applied
    );
    assert_eq!(
        store
            .get_edge(hub, *targets.last().unwrap())
            .await
            .unwrap()
            .unwrap()
            .trials(),
        2
    );

    let mut oversized = commit;
    oversized.idempotency = None;
    oversized.edges.push(oversized.edges[0].clone());
    assert!(matches!(
        store.commit_feedback(&oversized).await,
        Err(Error::CapacityExceeded { .. })
    ));
}

#[tokio::test]
async fn cozo_full_merge_resolution_is_terminal_after_commit() {
    let store = CozoStore::new(4).unwrap();
    let winner = NodeId(Ulid::from(920_000u128));
    let loser = NodeId(Ulid::from(920_001u128));
    raw_insert_nodes(&store, &[winner, loser]);
    store
        .observe_merge_candidate(winner, loser, 1)
        .await
        .unwrap();
    let commit = FullMergeCommit::new(winner, loser, 2).unwrap();
    assert_eq!(
        store.commit_full_merge(&commit).await.unwrap(),
        FullMergeCommitOutcome::Applied
    );
    store
        .resolve_merge_candidate(commit.pair(), MergeResolution::Full)
        .await
        .unwrap();
    assert!(matches!(
        store
            .resolve_merge_candidate(commit.pair(), MergeResolution::Keep)
            .await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        store.commit_full_merge(&commit).await.unwrap(),
        FullMergeCommitOutcome::AlreadyApplied
    );
}

const FULL_MERGE_STATE_QUERIES: [&str; 9] = [
    "?[id, data, status] := *node{id, data, status} :order id",
    "?[id, summary, status] := *node_search{id, summary, status} :order id",
    "?[id, e, status] := *node_vec{id, e, status} :order id",
    "?[tag, status, sample_hash, id] := *node_tag_v2{tag, status, sample_hash, id} \
       :order tag, status, sample_hash, id",
    "?[from, to, weight, kind, last_reinforced, trials, interference] := \
       *edge{from, to, weight, kind, last_reinforced, trials, interference} :order from, to",
    "?[from, to, start, end] := *edge_anchor{from, to, start, end} :order from, to",
    "?[from, target_db, target, weight] := *remote_edge{from, target_db, target, weight} \
       :order from, target_db, target",
    "?[lo, hi, observations, first_seen, last_seen, resolution] := \
       *merge_candidate{lo, hi, observations, first_seen, last_seen, resolution} :order lo, hi",
    "?[lo, hi, winner, loser, applied_at] := \
       *full_merge_commit{lo, hi, winner, loser, applied_at} :order lo, hi",
];

fn full_merge_store_state(store: &CozoStore) -> String {
    FULL_MERGE_STATE_QUERIES
        .iter()
        .map(|query| {
            format!(
                "{query}\n{:?}",
                store.run(query, BTreeMap::new(), false).unwrap().rows
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn full_merge_transaction_state(tx: &MultiTransaction) -> String {
    FULL_MERGE_STATE_QUERIES
        .iter()
        .map(|query| {
            format!(
                "{query}\n{:?}",
                tx_run(tx, query, BTreeMap::new()).unwrap().rows
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn raw_put_merge_candidate(
    store: &CozoStore,
    pair: UnorderedPair<NodeId>,
    observations: DataValue,
    first_seen: DataValue,
    last_seen: DataValue,
    resolution: DataValue,
) {
    let (lo, hi) = canonical(pair);
    store
        .run(
            "?[lo, hi, observations, first_seen, last_seen, resolution] <- \
             [[$lo, $hi, $observations, $first_seen, $last_seen, $resolution]] \
             :put merge_candidate \
               {lo, hi => observations, first_seen, last_seen, resolution}",
            BTreeMap::from([
                ("lo".into(), dv_str(&lo)),
                ("hi".into(), dv_str(&hi)),
                ("observations".into(), observations),
                ("first_seen".into(), first_seen),
                ("last_seen".into(), last_seen),
                ("resolution".into(), resolution),
            ]),
            true,
        )
        .unwrap();
}

fn raw_put_full_merge_proof(
    store: &CozoStore,
    physical_lo: &str,
    physical_hi: &str,
    winner: &str,
    loser: &str,
    applied_at: DataValue,
) {
    store
        .run(
            "?[lo, hi, winner, loser, applied_at] <- \
             [[$lo, $hi, $winner, $loser, $applied_at]] \
             :put full_merge_commit {lo, hi => winner, loser, applied_at}",
            BTreeMap::from([
                ("lo".into(), dv_str(physical_lo)),
                ("hi".into(), dv_str(physical_hi)),
                ("winner".into(), dv_str(winner)),
                ("loser".into(), dv_str(loser)),
                ("applied_at".into(), applied_at),
            ]),
            true,
        )
        .unwrap();
}

fn put_terminal_merge_candidate(
    store: &CozoStore,
    pair: UnorderedPair<NodeId>,
    resolution: MergeResolution,
) {
    let mut candidate = MergeCandidate::new(pair.0, pair.1, 1);
    candidate.resolve(resolution);
    store.upsert_merge(&candidate).unwrap();
}

fn raw_put_edge_anchor(store: &CozoStore, from: NodeId, to: NodeId, start: i64, end: i64) {
    store
        .run(
            "?[from, to, start, end] <- [[$from, $to, $start, $end]] \
             :put edge_anchor {from, to => start, end}",
            BTreeMap::from([
                ("from".into(), dv_str(&from.0.to_string())),
                ("to".into(), dv_str(&to.0.to_string())),
                ("start".into(), dv_int(start)),
                ("end".into(), dv_int(end)),
            ]),
            true,
        )
        .unwrap();
}

fn raw_put_remote_edge(
    store: &CozoStore,
    from: NodeId,
    target_db: &str,
    target: &str,
    weight: f64,
) {
    store
        .run(
            "?[from, target_db, target, weight] <- \
             [[$from, $target_db, $target, $weight]] \
             :put remote_edge {from, target_db, target => weight}",
            BTreeMap::from([
                ("from".into(), dv_str(&from.0.to_string())),
                ("target_db".into(), dv_str(target_db)),
                ("target".into(), dv_str(target)),
                ("weight".into(), dv_float(weight)),
            ]),
            true,
        )
        .unwrap();
}

fn full_merge_corruption_fixture(seed: u128) -> (CozoStore, FullMergeCommit, NodeId) {
    let store = CozoStore::new(4).unwrap();
    let winner = NodeId(Ulid::from(seed));
    let loser = NodeId(Ulid::from(seed + 1));
    let local_target = NodeId(Ulid::from(seed + 2));
    raw_insert_nodes(&store, &[winner, loser, local_target]);
    raw_insert_edges(&store, &[(loser, local_target)]);
    (
        store,
        FullMergeCommit::new(winner, loser, 10).unwrap(),
        local_target,
    )
}

async fn assert_full_merge_corruption_is_prefix_free(
    store: &CozoStore,
    commit: &FullMergeCommit,
    label: &str,
) {
    let before = full_merge_store_state(store);
    let tx = store.db.multi_transaction(true);
    assert!(
        matches!(
            stage_full_merge_transaction(&tx, store.db_id, commit),
            Err(Error::Backend(_))
        ),
        "{label} was not rejected as corrupt stored state"
    );
    assert_eq!(
        full_merge_transaction_state(&tx),
        before,
        "{label} staged a prefix mutation before rejecting the corrupt row"
    );
    tx.abort().unwrap();
    assert_eq!(
        full_merge_store_state(store),
        before,
        "{label} changed durable state after abort"
    );

    assert!(
        matches!(
            store.commit_full_merge(commit).await,
            Err(Error::Backend(_))
        ),
        "{label} was not rejected through GraphStore"
    );
    assert_eq!(
        full_merge_store_state(store),
        before,
        "{label} leaked a prefix mutation through GraphStore rollback"
    );
}

#[tokio::test]
async fn cozo_full_merge_rejects_malformed_candidate_without_a_prefix_mutation() {
    let malformed = [
        (
            "zero candidate count",
            dv_int(0),
            dv_int(1),
            dv_int(1),
            DataValue::Null,
        ),
        (
            "reversed candidate time",
            dv_int(1),
            dv_int(2),
            dv_int(1),
            DataValue::Null,
        ),
        (
            "unknown candidate resolution",
            dv_int(1),
            dv_int(1),
            dv_int(1),
            dv_str("bogus"),
        ),
    ];

    for (index, (label, observations, first_seen, last_seen, resolution)) in
        malformed.into_iter().enumerate()
    {
        let (store, commit, _) = full_merge_corruption_fixture(940_000 + index as u128 * 10);
        raw_put_merge_candidate(
            &store,
            commit.pair(),
            observations,
            first_seen,
            last_seen,
            resolution,
        );
        assert_full_merge_corruption_is_prefix_free(&store, &commit, label).await;
    }
}

#[tokio::test]
async fn cozo_full_merge_retry_rejects_reversed_or_malformed_proofs_without_mutation() {
    let winner = NodeId(Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap());
    let loser = NodeId(Ulid::from_string("01BX5ZZKBKACTAV9WEVGEMMVRZ").unwrap());
    let pair = UnorderedPair(winner, loser);
    let (lo, hi) = canonical(pair);

    let reversed_store = CozoStore::new(4).unwrap();
    put_terminal_merge_candidate(&reversed_store, pair, MergeResolution::Full);
    raw_put_full_merge_proof(
        &reversed_store,
        &lo,
        &hi,
        &winner.0.to_string(),
        &loser.0.to_string(),
        dv_int(1),
    );
    raw_put_full_merge_proof(
        &reversed_store,
        &hi,
        &lo,
        &winner.0.to_string(),
        &loser.0.to_string(),
        dv_int(1),
    );
    let commit = FullMergeCommit::new(winner, loser, 99).unwrap();
    assert_full_merge_corruption_is_prefix_free(&reversed_store, &commit, "reversed proof twin")
        .await;

    let negative_time_store = CozoStore::new(4).unwrap();
    put_terminal_merge_candidate(&negative_time_store, pair, MergeResolution::Full);
    raw_put_full_merge_proof(
        &negative_time_store,
        &lo,
        &hi,
        &winner.0.to_string(),
        &loser.0.to_string(),
        dv_int(-1),
    );
    assert_full_merge_corruption_is_prefix_free(
        &negative_time_store,
        &commit,
        "negative proof timestamp",
    )
    .await;

    let noncanonical_payload_store = CozoStore::new(4).unwrap();
    put_terminal_merge_candidate(&noncanonical_payload_store, pair, MergeResolution::Full);
    raw_put_full_merge_proof(
        &noncanonical_payload_store,
        &lo,
        &hi,
        &winner.0.to_string().to_ascii_lowercase(),
        &loser.0.to_string(),
        dv_int(1),
    );
    assert_full_merge_corruption_is_prefix_free(
        &noncanonical_payload_store,
        &commit,
        "noncanonical proof payload",
    )
    .await;
}

#[tokio::test]
async fn cozo_full_merge_retry_requires_a_coherent_terminal_post_state() {
    enum Incoherence {
        MissingCandidate,
        NonFullCandidate,
        LiveLoser,
        LocalLoserAdjacency,
        RemoteLoserAdjacency,
    }

    for (index, (label, incoherence)) in [
        ("missing terminal candidate", Incoherence::MissingCandidate),
        ("non-full terminal candidate", Incoherence::NonFullCandidate),
        ("live retained loser", Incoherence::LiveLoser),
        (
            "retained local loser edge",
            Incoherence::LocalLoserAdjacency,
        ),
        (
            "retained remote loser edge",
            Incoherence::RemoteLoserAdjacency,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let store = CozoStore::new(4).unwrap();
        let winner = NodeId(Ulid::from(944_000u128 + index as u128 * 10));
        let loser = NodeId(Ulid::from(944_001u128 + index as u128 * 10));
        let target = NodeId(Ulid::from(944_002u128 + index as u128 * 10));
        let record = FullMergeRecord::new(winner, loser, 1);
        raw_insert_full_merge_records(&store, &[record]);
        match incoherence {
            Incoherence::MissingCandidate => {}
            Incoherence::NonFullCandidate => {
                put_terminal_merge_candidate(&store, record.between, MergeResolution::Keep);
            }
            Incoherence::LiveLoser => {
                put_terminal_merge_candidate(&store, record.between, MergeResolution::Full);
                store.put_node(&active_node(loser)).await.unwrap();
            }
            Incoherence::LocalLoserAdjacency => {
                put_terminal_merge_candidate(&store, record.between, MergeResolution::Full);
                raw_insert_edges(&store, &[(loser, target)]);
            }
            Incoherence::RemoteLoserAdjacency => {
                put_terminal_merge_candidate(&store, record.between, MergeResolution::Full);
                let target_db = Ulid::from(945_000u128 + index as u128);
                assert_ne!(target_db, store.db_id);
                raw_put_remote_edge(
                    &store,
                    loser,
                    &target_db.to_string(),
                    &target.0.to_string(),
                    0.5,
                );
            }
        }
        let commit = FullMergeCommit::new(winner, loser, 999).unwrap();
        assert_full_merge_corruption_is_prefix_free(&store, &commit, label).await;
    }
}

#[tokio::test]
async fn cozo_full_merge_retry_is_historical_and_direction_bound() {
    let store = CozoStore::new(4).unwrap();
    let winner = NodeId(Ulid::from(946_000u128));
    let loser = NodeId(Ulid::from(946_001u128));
    raw_insert_nodes(&store, &[winner, loser]);
    store
        .observe_merge_candidate(winner, loser, 1)
        .await
        .unwrap();
    let original = FullMergeCommit::new(winner, loser, 2).unwrap();
    assert_eq!(
        store.commit_full_merge(&original).await.unwrap(),
        FullMergeCommitOutcome::Applied
    );

    let opposite = FullMergeCommit::new(loser, winner, 500).unwrap();
    assert!(matches!(
        store.commit_full_merge(&opposite).await,
        Err(Error::Conflict(_))
    ));

    store.delete_node(loser).await.unwrap();
    store.delete_node(winner).await.unwrap();
    assert!(store.get_node(loser).await.unwrap().is_none());
    assert!(store.get_node(winner).await.unwrap().is_none());
    let later_retry = FullMergeCommit::new(winner, loser, 999).unwrap();
    assert_eq!(
        store.commit_full_merge(&later_retry).await.unwrap(),
        FullMergeCommitOutcome::AlreadyApplied
    );
}

#[tokio::test]
async fn cozo_full_merge_rejects_non_u32_anchors_without_a_prefix_mutation() {
    for (index, (label, start, end)) in [
        ("negative edge anchor", -1, 1),
        ("out-of-range edge anchor", 0, i64::from(u32::MAX) + 1),
    ]
    .into_iter()
    .enumerate()
    {
        let (store, commit, target) = full_merge_corruption_fixture(941_000 + index as u128 * 10);
        store
            .observe_merge_candidate(commit.winner, commit.loser, 1)
            .await
            .unwrap();
        raw_put_edge_anchor(&store, commit.loser, target, start, end);
        assert_full_merge_corruption_is_prefix_free(&store, &commit, label).await;
    }
}

#[tokio::test]
async fn cozo_full_merge_rejects_source_endpoint_orphan_anchor_without_a_prefix_mutation() {
    let (store, commit, _) = full_merge_corruption_fixture(941_100);
    let orphan_target = NodeId(Ulid::from(941_103u128));
    raw_insert_nodes(&store, &[orphan_target]);
    store
        .observe_merge_candidate(commit.winner, commit.loser, 1)
        .await
        .unwrap();
    // This is the bounded direction supported by edge_anchor's `(from, to)`
    // primary key. Detecting a target-only orphan would require a reverse index
    // or an O(total anchors) scan and is intentionally not smuggled into merge.
    raw_put_edge_anchor(&store, commit.loser, orphan_target, 0, 1);
    assert_full_merge_corruption_is_prefix_free(&store, &commit, "loser-source orphan anchor")
        .await;
}

#[tokio::test]
async fn cozo_full_merge_rejects_noncanonical_remote_rows_without_a_prefix_mutation() {
    enum Corruption {
        TargetDb(String),
        Target(String),
        Weight(f64),
    }

    let canonical_db = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let canonical_target = Ulid::from_string("01BX5ZZKBKACTAV9WEVGEMMVRZ").unwrap();
    let cases = [
        (
            "nil remote target database",
            Corruption::TargetDb(Ulid::nil().to_string()),
        ),
        (
            "local remote target database",
            Corruption::TargetDb(String::new()),
        ),
        (
            "lowercase remote target database",
            Corruption::TargetDb(canonical_db.to_string().to_ascii_lowercase()),
        ),
        (
            "lowercase remote target node",
            Corruption::Target(canonical_target.to_string().to_ascii_lowercase()),
        ),
        ("noncanonical remote weight", Corruption::Weight(0.1)),
        ("negative-zero remote weight", Corruption::Weight(-0.0)),
    ];

    for (index, (label, corruption)) in cases.into_iter().enumerate() {
        let (store, commit, _) = full_merge_corruption_fixture(942_000 + index as u128 * 10);
        store
            .observe_merge_candidate(commit.winner, commit.loser, 1)
            .await
            .unwrap();
        let mut target_db = canonical_db.to_string();
        let mut target = canonical_target.to_string();
        let mut weight = 0.5;
        match corruption {
            Corruption::TargetDb(value) if value.is_empty() => {
                target_db = store.db_id.to_string();
            }
            Corruption::TargetDb(value) => target_db = value,
            Corruption::Target(value) => target = value,
            Corruption::Weight(value) => weight = value,
        }
        raw_put_remote_edge(&store, commit.loser, &target_db, &target, weight);
        assert_full_merge_corruption_is_prefix_free(&store, &commit, label).await;
    }
}

#[tokio::test]
async fn cozo_full_merge_counter_accumulation_saturates_to_valid_state() {
    // Strict inputs guarantee `interference <= trials`, and saturating addition
    // is monotone, so an invalid accumulated counter pair is unreachable. Hit
    // the double-overflow boundary anyway: the explicit final-edge validation
    // must admit the canonical saturated result rather than wrapping either
    // counter into invalid state.
    let store = CozoStore::new(4).unwrap();
    let winner = NodeId(Ulid::from(943_000u128));
    let loser = NodeId(Ulid::from(943_001u128));
    let target = NodeId(Ulid::from(943_002u128));
    raw_insert_nodes(&store, &[winner, loser, target]);
    let mut params = BTreeMap::new();
    params.insert("winner".into(), dv_str(&winner.0.to_string()));
    params.insert("loser".into(), dv_str(&loser.0.to_string()));
    params.insert("target".into(), dv_str(&target.0.to_string()));
    params.insert("counter".into(), dv_int(i64::from(u32::MAX)));
    store
        .run(
            "incoming[from, to] <- [[$winner, $target], [$loser, $target]] \
             ?[from, to, weight, kind, last_reinforced, trials, interference] := \
               incoming[from, to], weight = 0.5, kind = 'associative', \
               last_reinforced = 1, trials = $counter, interference = $counter \
               :put edge {from, to => weight, kind, last_reinforced, trials, interference}",
            params,
            true,
        )
        .unwrap();
    store
        .observe_merge_candidate(winner, loser, 1)
        .await
        .unwrap();

    let commit = FullMergeCommit::new(winner, loser, 2).unwrap();
    assert_eq!(
        store.commit_full_merge(&commit).await.unwrap(),
        FullMergeCommitOutcome::Applied
    );
    let edge = store.get_edge(winner, target).await.unwrap().unwrap();
    assert_eq!(edge.trials(), u32::MAX);
    assert_eq!(edge.interference(), u32::MAX);
    edge.validate().unwrap();
}

#[tokio::test]
async fn cozo_supersede_capacity_rejection_rolls_back_every_relation() {
    let store = CozoStore::new(4).unwrap();
    let winner = NodeId(Ulid::from(925_000u128));
    let loser = NodeId(Ulid::from(925_001u128));
    raw_insert_nodes(&store, &[winner, loser]);
    let saturated: Vec<_> = (0..MAX_INCIDENT_EDGES)
        .map(|offset| (winner, NodeId(Ulid::from(926_000u128 + offset as u128))))
        .collect();
    raw_insert_edges(&store, &saturated);
    store.observe_contradiction(winner, loser, 1).await.unwrap();
    let pair = UnorderedPair(winner, loser);
    store
        .resolve_contradiction(pair, Resolution::Unresolved)
        .await
        .unwrap();

    let commit = SupersedeCommit::new(winner, loser, 2).unwrap();
    assert!(matches!(
        store.commit_supersede(&commit).await,
        Err(Error::CapacityExceeded {
            resource: "incident edge degree",
            limit: MAX_INCIDENT_EDGES,
        })
    ));
    assert!(store.get_edge(winner, loser).await.unwrap().is_none());
    let after = store.get_node(loser).await.unwrap().unwrap();
    assert_eq!(after.confidence(), 0.5);
    assert_eq!(after.status(), NodeStatus::Active);
    let export = store.export().await.unwrap();
    assert_eq!(export.edges.len(), MAX_INCIDENT_EDGES);
    let contradiction = export
        .contradictions
        .iter()
        .find(|item| item.between == pair)
        .unwrap();
    assert_eq!(contradiction.observations, 1);
    assert_eq!(contradiction.resolution, Some(Resolution::Unresolved));
    assert!(export.supersede_commits.is_empty());
}

#[tokio::test]
async fn cozo_supersede_late_failure_rolls_back_and_success_updates_projections() {
    let store = CozoStore::new(4).unwrap();
    let winner = NodeId(Ulid::from(927_000u128));
    let loser = NodeId(Ulid::from(927_001u128));
    raw_insert_projected_nodes(&store, &[winner], NodeStatus::Active, &[1.0, 0.0, 0.0, 0.0]);
    raw_insert_projected_nodes(&store, &[loser], NodeStatus::Active, &[0.0, 1.0, 0.0, 0.0]);
    // The bulk vector fixture deliberately omits lexical rows; publish the
    // canonical/search half through the production node transaction while
    // retaining the vectors above.
    store.put_node(&active_node(winner)).await.unwrap();
    store.put_node(&active_node(loser)).await.unwrap();
    let mut original_edge = Edge::new(winner, loser, 0.4, EdgeKind::Associative, 3);
    original_edge.anchor = Some(BodySpan::new(2, 4));
    store.put_edge(&original_edge).await.unwrap();
    store.observe_contradiction(winner, loser, 4).await.unwrap();
    let pair = UnorderedPair(winner, loser);
    store
        .resolve_contradiction(pair, Resolution::Unresolved)
        .await
        .unwrap();
    let commit = SupersedeCommit::new(winner, loser, 5).unwrap();

    let tx = store.db.multi_transaction(true);
    assert_eq!(
        stage_supersede_transaction(&tx, &commit).unwrap(),
        SupersedeCommitOutcome::Applied
    );
    // Inject a failure after edge/anchor, canonical node, search/vector
    // projections, contradiction, and retry proof have all been staged.
    assert!(
        tx_run(
            &tx,
            "?[must_be_empty] <- [[1]] :assert none",
            BTreeMap::new(),
        )
        .is_err()
    );
    tx.abort().unwrap();

    let rolled_back_edge = store.get_edge(winner, loser).await.unwrap().unwrap();
    assert_eq!(rolled_back_edge.kind, original_edge.kind);
    assert_eq!(rolled_back_edge.anchor, original_edge.anchor);
    assert_eq!(rolled_back_edge.weight(), original_edge.weight());
    assert_eq!(
        rolled_back_edge.last_reinforced(),
        original_edge.last_reinforced()
    );
    assert_eq!(rolled_back_edge.trials(), original_edge.trials());
    assert_eq!(
        rolled_back_edge.interference(),
        original_edge.interference()
    );
    let rolled_back = store.get_node(loser).await.unwrap().unwrap();
    assert_eq!(rolled_back.confidence(), 0.5);
    assert_eq!(rolled_back.status(), NodeStatus::Active);
    assert_eq!(
        raw_node_search_projection(&store, loser),
        ("node".into(), "active".into())
    );
    assert_eq!(raw_node_vector_status(&store, loser), "active");
    let rolled_back_export = store.export().await.unwrap();
    let contradiction = rolled_back_export
        .contradictions
        .iter()
        .find(|item| item.between == pair)
        .unwrap();
    assert_eq!(contradiction.observations, 1);
    assert_eq!(contradiction.resolution, Some(Resolution::Unresolved));
    assert!(rolled_back_export.supersede_commits.is_empty());

    assert_eq!(
        store.commit_supersede(&commit).await.unwrap(),
        SupersedeCommitOutcome::Applied
    );
    let applied = store.get_node(loser).await.unwrap().unwrap();
    assert_eq!(applied.confidence(), rolled_back.confidence());
    assert_eq!(applied.status(), NodeStatus::Archived);
    assert_eq!(
        applied.body(),
        rolled_back.body(),
        "supersede retains the historical body reference"
    );
    let applied_edge = store.get_edge(winner, loser).await.unwrap().unwrap();
    assert_eq!(applied_edge.kind, EdgeKind::Supersedes);
    assert!(applied_edge.anchor.is_none());
    assert_eq!(
        raw_node_search_projection(&store, loser),
        ("node".into(), "archived".into())
    );
    assert_eq!(raw_node_vector_status(&store, loser), "archived");
    let applied_export = store.export().await.unwrap();
    assert_eq!(applied_export.supersede_commits, vec![commit.record()]);
    let contradiction = applied_export
        .contradictions
        .iter()
        .find(|item| item.between == pair)
        .unwrap();
    assert_eq!(contradiction.observations, 2);
    assert_eq!(contradiction.resolution, Some(Resolution::Superseded));
}

#[tokio::test]
async fn cozo_supersede_archives_active_tags_but_exact_retry_respects_later_curation() {
    let store = CozoStore::new(4).unwrap();
    let winner = NodeId(Ulid::from(927_100u128));
    let loser = NodeId(Ulid::from(927_101u128));
    store.put_node(&active_node(winner)).await.unwrap();
    let old = tagged_node(loser, &["stale", "workshop"]);
    store.put_node(&old).await.unwrap();
    store.upsert(loser, &[0.0, 1.0, 0.0, 0.0]).await.unwrap();
    let commit = SupersedeCommit::new(winner, loser, 7).unwrap();

    assert_eq!(
        store.commit_supersede(&commit).await.unwrap(),
        SupersedeCommitOutcome::Applied
    );
    let archived = store.get_node(loser).await.unwrap().unwrap();
    assert_eq!(archived.status(), NodeStatus::Archived);
    assert_eq!(archived.body(), old.body());
    assert_eq!(archived.confidence(), old.confidence());
    assert_eq!(raw_node_vector_status(&store, loser), "archived");
    assert_eq!(raw_node_search_projection(&store, loser).1, "archived");
    assert!(
        physical_tags(&store, loser)
            .iter()
            .all(|(_, status, _)| status == "archived")
    );
    assert_eq!(
        store.get_edge(winner, loser).await.unwrap().unwrap().kind,
        EdgeKind::Supersedes
    );

    // Exact replay is a no-op even after a later deliberate lifecycle edit.
    store.set_status(loser, NodeStatus::Active).await.unwrap();
    let curated = store.get_node(loser).await.unwrap().unwrap();
    assert_eq!(
        store.commit_supersede(&commit).await.unwrap(),
        SupersedeCommitOutcome::AlreadyApplied
    );
    let after_retry = store.get_node(loser).await.unwrap().unwrap();
    assert_eq!(after_retry.status(), NodeStatus::Active);
    assert_eq!(after_retry.confidence(), curated.confidence());
    assert_eq!(
        store.get_edge(winner, loser).await.unwrap().unwrap().kind,
        EdgeKind::Supersedes
    );
}

#[tokio::test]
async fn cozo_supersede_exact_retry_does_not_rewrite_loser_from_existing_history() {
    // This is a current-generation history-replay fixture, not a predecessor migration.
    let store = CozoStore::new(4).unwrap();
    let winner = NodeId(Ulid::from(927_200u128));
    let loser = NodeId(Ulid::from(927_201u128));
    store.put_node(&active_node(winner)).await.unwrap();
    let historical_loser = tagged_node(loser, &["historical"]);
    store.put_node(&historical_loser).await.unwrap();
    store.upsert(loser, &[0.0, 1.0, 0.0, 0.0]).await.unwrap();
    let committed_before_change = SupersedeCommit::new(winner, loser, 3).unwrap();
    store
        .upsert_supersede_record(&committed_before_change.record())
        .unwrap();

    assert_eq!(
        store
            .commit_supersede(&committed_before_change)
            .await
            .unwrap(),
        SupersedeCommitOutcome::AlreadyApplied
    );
    let retained = store.get_node(loser).await.unwrap().unwrap();
    assert_eq!(retained.status(), NodeStatus::Active);
    assert_eq!(retained.confidence(), historical_loser.confidence());
    assert_eq!(raw_node_vector_status(&store, loser), "active");
    assert_eq!(raw_node_search_projection(&store, loser).1, "active");
    assert_eq!(physical_tags(&store, loser)[0].1, "active");
}

#[tokio::test]
async fn cozo_many_full_merge_proofs_preserve_exact_retry_and_allow_new_pair() {
    const HISTORICAL_PROOFS: usize = 256;
    let store = CozoStore::new(4).unwrap();
    let exact = FullMergeRecord::new(
        NodeId(Ulid::from(930_000u128)),
        NodeId(Ulid::from(930_001u128)),
        1,
    );
    let mut records = vec![exact];
    records.extend((1..HISTORICAL_PROOFS).map(|index| {
        FullMergeRecord::new(
            NodeId(Ulid::from(940_000u128 + index as u128 * 2)),
            NodeId(Ulid::from(940_001u128 + index as u128 * 2)),
            index as Timestamp,
        )
    }));
    raw_insert_full_merge_records(&store, &records);
    put_terminal_merge_candidate(&store, exact.between, MergeResolution::Full);
    let exact_commit = FullMergeCommit::new(exact.winner, exact.loser, exact.applied_at).unwrap();
    assert_eq!(
        store.commit_full_merge(&exact_commit).await.unwrap(),
        FullMergeCommitOutcome::AlreadyApplied
    );

    let winner = NodeId(Ulid::from(990_000u128));
    let loser = NodeId(Ulid::from(990_001u128));
    raw_insert_nodes(&store, &[winner, loser]);
    store
        .observe_merge_candidate(winner, loser, 2)
        .await
        .unwrap();
    let new_commit = FullMergeCommit::new(winner, loser, 3).unwrap();
    assert_eq!(
        store.commit_full_merge(&new_commit).await.unwrap(),
        FullMergeCommitOutcome::Applied
    );
    let export = store.export().await.unwrap();
    assert_eq!(export.full_merge_commits.len(), HISTORICAL_PROOFS + 1);
    assert!(store.get_node(loser).await.unwrap().unwrap().is_archived());
}

#[tokio::test]
async fn cozo_full_merge_late_failure_rolls_back_every_staged_relation() {
    let store = CozoStore::new(4).unwrap();
    let winner = NodeId(Ulid::from(910_000u128));
    let loser = NodeId(Ulid::from(910_001u128));
    let target = NodeId(Ulid::from(910_002u128));
    raw_insert_nodes(&store, &[winner, loser, target]);
    raw_insert_edges(&store, &[(loser, target)]);
    let target_db = Ulid::from(910_003u128);
    let remote_target = NodeId(Ulid::from(910_004u128));
    store
        .put_remote_edge(&RemoteEdge::new(loser, target_db, remote_target, 0.8))
        .await
        .unwrap();
    store
        .observe_merge_candidate(winner, loser, 5)
        .await
        .unwrap();

    let commit = FullMergeCommit::new(winner, loser, 6).unwrap();
    let tx = store.db.multi_transaction(true);
    assert_eq!(
        stage_full_merge_transaction(&tx, store.db_id, &commit).unwrap(),
        FullMergeCommitOutcome::Applied
    );
    // Fail after local/remote rewrites, loser archival, candidate closure,
    // and ledger insertion have all been staged. None may leak through the
    // transaction boundary.
    assert!(
        tx_run(
            &tx,
            "?[must_be_empty] <- [[1]] :assert none",
            BTreeMap::new()
        )
        .is_err()
    );
    tx.abort().unwrap();

    assert!(store.get_node(loser).await.unwrap().unwrap().is_active());
    assert!(store.get_edge(loser, target).await.unwrap().is_some());
    assert!(store.get_edge(winner, target).await.unwrap().is_none());
    assert_eq!(collect_remote_edges(&store, loser).await.len(), 1);
    assert!(collect_remote_edges(&store, winner).await.is_empty());
    assert_eq!(
        store
            .open_merge_candidates(ColdPath::acquire())
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(store.export().await.unwrap().full_merge_commits.is_empty());
}

mod concerns;

#[tokio::test]
async fn cozo_compare_replace_node_tags_contract() {
    let store = CozoStore::new(4).unwrap();
    crate::tag_replacement_tests::contract(&store).await;
    let canonical = store.all_nodes(ColdPath::acquire()).await.unwrap();
    let rows = store
        .run(
            "?[tag,status,sample_hash,id] := *node_tag_v2{tag,status,sample_hash,id}",
            BTreeMap::new(),
            false,
        )
        .unwrap();
    let expected_count: usize = canonical.iter().map(|node| node.tag_set().len()).sum();
    assert_eq!(rows.rows.len(), expected_count);
    for node in canonical {
        for tag in node.tags() {
            assert!(rows.rows.iter().any(|row| want_str(&row[0]).unwrap() == tag
                && want_str(&row[1]).unwrap() == status_str(node.status())
                && want_i64(&row[2]).unwrap() == stable_tag_sample_hash(node.id())
                && want_str(&row[3]).unwrap() == node.id().0.to_string()));
        }
    }
}
