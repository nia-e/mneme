use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use cozo::{
    DbInstance, ExistingSqliteSnapshotSource, ManagedCatalogEncodingV1, ManagedSnapshotPolicy,
};
use mneme_core::managed::DatabaseId;
use mneme_store_path::StoreLease;
use ulid::Ulid;

use super::*;
use crate::storage_contract::conventional_unmanaged::admission::{
    OpenAdmissionV1, recognize_open_source_v1,
};
use crate::storage_contract::conventional_unmanaged::spec::{
    LEGACY_CATALOG_GENERATION_MARKER, STRUCT_MAP_CATALOG_GENERATION_MARKER,
};

fn fixture_path(label: &str) -> PathBuf {
    fs::canonicalize(std::env::temp_dir())
        .expect("canonical temp directory")
        .join(format!("mneme-current-schema-{label}-{}.db", Ulid::new()))
}

fn remove_fixture(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        let _ = fs::remove_file(PathBuf::from(candidate));
    }
}

fn fresh_sqlite(label: &str) -> (PathBuf, DbInstance) {
    let path = fixture_path(label);
    remove_fixture(&path);
    let database = DbInstance::new("sqlite", &path, "").expect("fresh private SQLite artifact");
    (path, database)
}

fn database_id() -> DatabaseId {
    DatabaseId::new(Ulid::from(0x1234_u128)).expect("non-nil fixture database id")
}

fn assert_no_relations(database: &DbInstance) {
    let rows = database
        .run_default("::relations")
        .expect("inspect relation inventory");
    assert!(
        rows.rows.is_empty(),
        "failed current-schema transaction leaked relations: {:?}",
        rows.rows
    );
}

#[test]
fn request_validation_precedes_every_transaction_mutation() {
    assert!(validate_request(1, Ulid::from(1_u128)).is_ok());
    assert!(validate_request(MAX_TAGGED_VECTOR_DIMENSION, Ulid::from(1_u128)).is_ok());
    assert!(validate_request(0, Ulid::from(1_u128)).is_err());
    assert!(validate_request(MAX_TAGGED_VECTOR_DIMENSION + 1, Ulid::from(1_u128)).is_err());
    assert!(validate_request(4, Ulid::nil()).is_err());

    let (path, database) = fresh_sqlite("invalid-request");
    assert!(stage_current_schema(&database, 0, database_id()).is_err());
    assert_no_relations(&database);
    assert!(
        stage_current_schema(&database, MAX_TAGGED_VECTOR_DIMENSION + 1, database_id(),).is_err()
    );
    assert_no_relations(&database);
    drop(database);
    remove_fixture(&path);
}

#[test]
fn marker_free_builder_is_exactly_the_shared_physical_contract() {
    let statements = marker_free_schema_statements(4);
    let script = statements.join("\n");
    assert_eq!(statements.len(), 29);
    assert_eq!(BASE_RELATIONS.len(), 16);
    assert_eq!(DERIVED_RELATIONS.len(), 10);
    assert_eq!(all_physical_relations().count(), 26);
    assert!(!script.contains(LEGACY_CATALOG_GENERATION_MARKER));
    assert!(!script.contains(STRUCT_MAP_CATALOG_GENERATION_MARKER));
    assert_eq!(script, marker_free_schema_script(4));
}

#[test]
fn every_precommit_failure_boundary_rolls_back_durable_catalog_and_retries() {
    let dimension = 4;
    let pre_marker = pre_marker_transaction_steps(dimension);
    let total = total_transaction_steps(dimension);
    assert_eq!(pre_marker + 1 + 3, total);

    for boundary in 1..=total {
        let (path, database) = fresh_sqlite(&format!("rollback-{boundary}"));
        let error =
            stage_current_schema_failing_after(&database, dimension, database_id(), boundary)
                .expect_err("injected transaction failure must refuse publication");
        assert!(
            error
                .to_string()
                .contains("injected current-schema failure"),
            "boundary {boundary} returned the wrong error: {error}"
        );
        assert_no_relations(&database);
        // This proves durable catalog rollback and functional retry. It does
        // not claim restoration of Mnestic's process-local relation-id
        // allocator; exact counter-zero remains a publisher prerequisite.
        stage_current_schema(&database, dimension, database_id()).unwrap_or_else(|error| {
            panic!("boundary {boundary} left durable catalog residue that broke retry: {error}")
        });
        assert_eq!(
            database.run_default("::relations").unwrap().rows.len(),
            EXPECTED_PHYSICAL_RELATIONS,
            "boundary {boundary} retry did not publish the exact catalog"
        );
        drop(database);
        remove_fixture(&path);
    }
}

#[test]
fn failed_commit_does_not_publish_schema_or_marker() {
    let (path, database) = fresh_sqlite("commit-failure");
    database.fail_next_commit_for_tests();
    let error = stage_current_schema(&database, 4, database_id())
        .expect_err("injected commit failure must be reported");
    assert!(error.to_string().contains("commit"), "{error}");
    assert_no_relations(&database);
    drop(database);
    remove_fixture(&path);
}

#[test]
fn successful_sqlite_kernel_closes_as_raw_current_struct_map() {
    let (path, database) = fresh_sqlite("raw-current");
    stage_current_schema(&database, 4, database_id()).expect("stage current schema");

    let metadata = database
        .run_default("?[k, v] := *meta{k, v} :order k")
        .expect("read exact committed metadata before import");
    assert_eq!(
        metadata.rows,
        vec![
            vec![
                data_string("canonical_node_contract_v1"),
                data_string("bounded_refs_summary_arc_tags_derived64_unit_scalars_v1"),
            ],
            vec![
                data_string("db_id"),
                data_string(&database_id().to_string())
            ],
            vec![data_string("dim"), data_string("4")],
            vec![
                data_string("lexical_projection_v1"),
                data_string("complete")
            ],
            vec![data_string("max_incident_edges_v1"), data_string("1024")],
            vec![
                data_string("max_remote_edges_per_source_v1"),
                data_string("256"),
            ],
            vec![
                data_string("tag_projection_v2"),
                data_string("canonical_node_tag_membership_v2_f7"),
            ],
            vec![
                data_string("vector_projection_v2"),
                data_string(STRUCT_MAP_CATALOG_GENERATION_MARKER),
            ],
        ]
    );

    let permanent_guard = database
        .run_default(
            "?[fence, generation] := *mneme_reembed_shadow_node_vec{fence, generation} :order fence",
        )
        .expect("read permanent old-writer guard before import");
    assert_eq!(
        permanent_guard.rows,
        vec![vec![
            data_string("mneme-vector-projection-old-writer-fence"),
            data_string(
                "status-partitioned-hnsw-v3-canonical-node-v1-tag-v2-f7-read-only-guard-v1",
            ),
        ]]
    );

    let retired_node_tags = database
        .run_default("?[id, tag] := *node_tag{id, tag}")
        .expect("read retired node-tag relation before import");
    assert!(
        retired_node_tags.rows.is_empty(),
        "fresh current schema retained retired node-tag rows: {:?}",
        retired_node_tags.rows
    );

    assert!(
        database
            .run_default(&format!(
                "?[k] := *meta{{k, v: '{LEGACY_CATALOG_GENERATION_MARKER}'}}"
            ))
            .expect("search for legacy generation marker")
            .rows
            .is_empty()
    );

    database
        .prepare_sqlite_for_file_move()
        .expect("checkpoint and close SQLite sidecars");
    drop(database);

    let source = ExistingSqliteSnapshotSource::open(&path).expect("open clean SQLite source");
    let mut reader = source
        .into_managed_reader(ManagedSnapshotPolicy::V1)
        .expect("open pinned raw SQLite catalog reader");
    let raw_catalog = reader
        .inspect_primary_index_catalog_v1()
        .expect("inspect primary-index-visible Mnestic catalog");
    assert_eq!(raw_catalog.relation_counter(), 26);
    assert_eq!(raw_catalog.catalog().len(), 26);

    let relation_ids = raw_catalog
        .catalog()
        .entries()
        .iter()
        .map(|entry| entry.relation().id())
        .collect::<BTreeSet<_>>();
    assert_eq!(relation_ids, (1_u64..=26).collect::<BTreeSet<_>>());

    let relation_names = raw_catalog
        .catalog()
        .entries()
        .iter()
        .map(|entry| entry.relation().name())
        .collect::<BTreeSet<_>>();
    let expected_relation_names = [
        "contradiction",
        "edge",
        "edge:by_to",
        "edge_anchor",
        "feedback_retry",
        "feedback_retry_order",
        "full_merge_commit",
        "merge_candidate",
        "meta",
        "mneme_reembed_shadow_node_vec",
        "node",
        "node_search",
        "node_search:active_fts",
        "node_search:archived_fts",
        "node_search:by_status",
        "node_search:candidate_fts",
        "node_tag",
        "node_tag:by_tag",
        "node_tag_v2",
        "node_tag_v2:by_id",
        "node_vec",
        "node_vec:active_idx",
        "node_vec:archived_idx",
        "node_vec:candidate_idx",
        "remote_edge",
        "supersede_commit",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    assert_eq!(relation_names, expected_relation_names);

    let non_struct_map_relations = raw_catalog
        .catalog()
        .entries()
        .iter()
        .filter(|entry| entry.encoding() != ManagedCatalogEncodingV1::StructMapV1)
        .map(|entry| entry.relation().name())
        .collect::<Vec<_>>();
    assert!(
        non_struct_map_relations.is_empty(),
        "fresh current catalog retained non-struct-map codecs: {non_struct_map_relations:?}"
    );
    reader
        .close_and_verify()
        .expect("close pinned raw SQLite catalog reader");

    let source = ExistingSqliteSnapshotSource::open(&path).expect("open clean SQLite source");
    let admission = recognize_open_source_v1(source)
        .expect("closed raw catalog and semantic KAT must admit current generation");
    let OpenAdmissionV1::Current(current) = admission else {
        panic!("fresh kernel artifact was classified as legacy")
    };
    assert_eq!(current.vector_dimension(), 4);
    drop(current);

    let lease = StoreLease::acquire(&path).expect("lease fresh current artifact");
    assert!(
        super::super::CozoStore::open_existing_current(&path, lease).is_err(),
        "normal successor opener refuses frozen predecessor kernel"
    );
    remove_fixture(&path);
}

#[test]
fn episode_schema_has_a_distinct_closed_inventory_and_generation_fence() {
    use crate::storage_contract::conventional_unmanaged::admission::{
        CatalogSealGenerationV1, close_episode_source_bound_seal_v1,
        recognize_capture_open_source_v1, recognize_episode_open_source_v1,
        validate_episode_primary_index_visible_catalog, validate_primary_index_visible_catalog,
    };
    use crate::storage_contract::conventional_unmanaged::visible_catalog::{
        CatalogResults, validate_catalog_visible_manifest,
        validate_episode_catalog_visible_manifest,
    };

    let (path, database) = fresh_sqlite("episode-catalog");
    stage_episode_schema(&database, 4, database_id()).expect("stage episode schema");
    let relations = database.run_default("::relations").unwrap();
    assert_eq!(relations.rows.len(), 31);
    let mut columns = BTreeMap::new();
    let mut indices = BTreeMap::new();
    let mut triggers = BTreeMap::new();
    for relation in CatalogContract::EpisodeV1.relations() {
        columns.insert(
            relation.name.to_owned(),
            database
                .run_default(&format!("::columns {}", relation.name))
                .unwrap(),
        );
        indices.insert(
            relation.name.to_owned(),
            database
                .run_default(&format!("::indices {}", relation.name))
                .unwrap(),
        );
    }
    for base in CatalogContract::EpisodeV1.bases() {
        triggers.insert(
            base.relation.name.to_owned(),
            database
                .run_default(&format!("::show_triggers {}", base.relation.name))
                .unwrap(),
        );
    }
    let results = || CatalogResults {
        relations: &relations,
        columns: &columns,
        indices: &indices,
        trigger_catalogs: &triggers,
    };
    let manifest =
        validate_episode_catalog_visible_manifest(results()).expect("exact episode presentation");
    assert_eq!(manifest.vector_dimension, 4);
    assert_eq!(manifest.page_relations.len(), 20);
    assert_eq!(manifest.derived_relations.len(), 11);
    assert!(
        validate_catalog_visible_manifest(results()).is_err(),
        "predecessor presentation must stay closed"
    );
    database.prepare_sqlite_for_file_move().unwrap();
    drop(database);

    let reader = || {
        ExistingSqliteSnapshotSource::open(&path)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::EpisodeV1)
            .unwrap()
    };
    let mut raw = reader();
    let catalog = raw.inspect_primary_index_catalog_v1().unwrap();
    assert_eq!(catalog.relation_counter(), 31);
    assert_eq!(catalog.catalog().len(), 31);
    let candidate =
        validate_episode_primary_index_visible_catalog(catalog).expect("exact episode raw catalog");
    assert_eq!(candidate.vector_dimension(), 4);
    assert!(candidate.relation_id("episode_head").is_some());
    assert!(
        validate_primary_index_visible_catalog(catalog).is_err(),
        "predecessor raw inventory must stay closed"
    );
    raw.close_and_verify().unwrap();

    let sealed = close_episode_source_bound_seal_v1(reader()).expect("closed episode source seal");
    assert_eq!(sealed.generation(), CatalogSealGenerationV1::EpisodeV1);
    drop(sealed);
    assert!(recognize_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap()).is_err());
    assert!(
        recognize_capture_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
            .is_err()
    );
    let admitted =
        recognize_episode_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
            .expect("positive episode permit");
    let OpenAdmissionV1::Current(permit) = admitted else {
        panic!("episode cannot need codec upgrade")
    };
    assert_eq!(permit.vector_dimension(), 4);
    assert!(
        permit
            .into_existing_open_admission_v1(&path)
            .unwrap()
            .into_runtime_parts()
            .2
            .uses_current_classifier_policy()
    );
    remove_fixture(&path);
}

#[test]
fn episode_recognition_refuses_predecessors_and_torn_successors() {
    use crate::storage_contract::conventional_unmanaged::admission::{
        close_episode_source_bound_seal_v1, recognize_capture_open_source_v1,
        recognize_episode_open_source_v1,
    };
    for (label, episode_catalog, marker) in [
        (
            "capture-predecessor",
            false,
            CAPTURE_V1_CATALOG_GENERATION_MARKER,
        ),
        (
            "episode-marker-only",
            false,
            EPISODE_V1_CATALOG_GENERATION_MARKER,
        ),
        (
            "episode-catalog-only",
            true,
            CAPTURE_V1_CATALOG_GENERATION_MARKER,
        ),
        ("episode-unknown-marker", true, "unknown-episode-generation"),
    ] {
        let (path, database) = fresh_sqlite(label);
        if episode_catalog {
            stage_episode_schema(&database, 4, database_id()).unwrap();
        } else {
            stage_capture_schema(&database, 4, database_id()).unwrap();
        }
        database
            .run_default(&format!(
                "?[k, v] <- [['vector_projection_v2', '{marker}']] :put meta {{k => v}}"
            ))
            .unwrap();
        database.prepare_sqlite_for_file_move().unwrap();
        drop(database);
        assert!(
            recognize_episode_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
                .is_err(),
            "{label}"
        );
        let source = ExistingSqliteSnapshotSource::open(&path).unwrap();
        let reader = source
            .into_managed_reader(ManagedSnapshotPolicy::EpisodeV1)
            .unwrap();
        assert!(
            close_episode_source_bound_seal_v1(reader).is_err(),
            "{label}"
        );
        assert_eq!(
            recognize_capture_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
                .is_ok(),
            label == "capture-predecessor"
        );
        remove_fixture(&path);
    }
}

#[test]
fn episode_schema_rolls_back_each_transaction_boundary_and_retries() {
    let mut request = validate_request(4, database_id().get()).unwrap();
    request.catalog = CatalogContract::EpisodeV1;
    request.generation_marker = EPISODE_V1_CATALOG_GENERATION_MARKER;
    // Empty preflight, each schema statement, metadata, canonicalization,
    // final marker and three bounded verification reads.
    let steps = 1 + schema_statements_for(4, request.catalog).len() + 2 + 1 + 3;
    for boundary in 1..=steps {
        let (path, database) = fresh_sqlite(&format!("episode-rollback-{boundary}"));
        let error = stage_validated_current_schema(
            &database,
            &request,
            FailureInjection::after_completed_step(boundary),
        )
        .expect_err("injected episode schema failure");
        assert!(
            error
                .to_string()
                .contains("injected current-schema failure"),
            "{boundary}: {error}"
        );
        assert_no_relations(&database);
        stage_episode_schema(&database, 4, database_id()).unwrap();
        assert_eq!(database.run_default("::relations").unwrap().rows.len(), 31);
        drop(database);
        remove_fixture(&path);
    }
}

#[test]
fn single_graph_exact_catalog_and_markers_refuse_predecessor_recognizers() {
    use crate::storage_contract::conventional_unmanaged::admission::{
        CatalogSealGenerationV1, close_single_graph_source_bound_seal_v1,
        recognize_capture_open_source_v1, recognize_episode_open_source_v1,
        recognize_single_graph_open_source_v1,
    };
    let (path, database) = fresh_sqlite("single-graph-exact");
    stage_single_graph_schema(&database, 4, database_id()).unwrap();
    let rows = database.run_default("::relations").unwrap();
    assert_eq!(rows.rows.len(), 29);
    let names = CatalogContract::SingleGraphV1
        .relations()
        .map(|r| r.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(names.len(), 29);
    assert!(!names.contains("node_vec:candidate_idx"));
    assert!(!names.contains("node_search:candidate_fts"));
    assert!(names.contains("episode_history"));
    database.prepare_sqlite_for_file_move().unwrap();
    drop(database);
    let reader = ExistingSqliteSnapshotSource::open(&path)
        .unwrap()
        .into_managed_reader(ManagedSnapshotPolicy::SingleGraphV1)
        .unwrap();
    let seal = close_single_graph_source_bound_seal_v1(reader).unwrap();
    assert_eq!(seal.generation(), CatalogSealGenerationV1::SingleGraphV1);
    drop(seal);
    assert!(
        recognize_capture_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
            .is_err()
    );
    assert!(
        recognize_episode_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
            .is_err()
    );
    let admitted =
        recognize_single_graph_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
            .unwrap();
    drop(admitted);
    let lease = StoreLease::acquire(&path).unwrap();
    assert!(super::super::CozoStore::open_existing_current(&path, lease).is_err());
    remove_fixture(&path);
}

#[test]
fn concern_exact_catalog_and_markers_refuse_predecessor_recognizers() {
    use crate::storage_contract::conventional_unmanaged::admission::{
        CatalogSealGenerationV1, close_concern_source_bound_seal_v1,
        recognize_capture_open_source_v1, recognize_concern_open_source_v1,
        recognize_episode_open_source_v1, recognize_single_graph_open_source_v1,
    };
    let (path, database) = fresh_sqlite("concern-exact");
    stage_concern_schema(&database, 4, database_id()).unwrap();
    let rows = database.run_default("::relations").unwrap();
    assert_eq!(rows.rows.len(), 31);
    let names = CatalogContract::ConcernV1
        .relations()
        .map(|r| r.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(names.len(), 31);
    assert!(!names.contains("node_vec:candidate_idx"));
    assert!(!names.contains("node_search:candidate_fts"));
    assert!(names.contains("episode_history"));
    database.prepare_sqlite_for_file_move().unwrap();
    drop(database);
    let reader = ExistingSqliteSnapshotSource::open(&path)
        .unwrap()
        .into_managed_reader(ManagedSnapshotPolicy::ConcernV1)
        .unwrap();
    let seal = close_concern_source_bound_seal_v1(reader).unwrap();
    assert_eq!(seal.generation(), CatalogSealGenerationV1::ConcernV1);
    drop(seal);
    assert!(
        recognize_capture_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
            .is_err()
    );
    assert!(
        recognize_episode_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
            .is_err()
    );
    assert!(
        recognize_single_graph_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
            .is_err()
    );
    let admitted =
        recognize_concern_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
            .unwrap();
    drop(admitted);
    let before = fs::read(&path).unwrap();
    let lease = std::sync::Arc::new(StoreLease::acquire(&path).unwrap());
    assert!(super::super::CozoStore::require_existing_current(&path, &lease).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    let store = super::super::CozoStore::open_leased_concern(&path, lease).unwrap();
    assert_eq!(store.db_id(), database_id().get());
    store.prepare_for_file_move().unwrap();
    drop(store);
    remove_fixture(&path);
}

#[test]
fn concern_closed_catalog_refuses_extra_relation_and_missing_incoming_index() {
    use crate::storage_contract::conventional_unmanaged::admission::recognize_concern_open_source_v1;
    for alteration in [
        "{:create unknown_concern {id: String}}",
        "{::index drop concern:by_hi}",
    ] {
        let (path, database) = fresh_sqlite("concern-altered");
        stage_concern_schema(&database, 4, database_id()).unwrap();
        database.run_default(alteration).unwrap();
        database.prepare_sqlite_for_file_move().unwrap();
        drop(database);
        assert!(
            recognize_concern_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
                .is_err()
        );
        remove_fixture(&path);
    }
}

#[test]
fn concern_schema_rolls_back_each_transaction_boundary_and_retries() {
    let mut request = validate_request(4, database_id().get()).unwrap();
    request.catalog = CatalogContract::ConcernV1;
    request.generation_marker = CONCERN_V1_CATALOG_GENERATION_MARKER;
    // Empty preflight, each schema statement, metadata, canonicalization,
    // final marker and three bounded verification reads.
    let steps = 1 + schema_statements_for(4, request.catalog).len() + 2 + 1 + 3;
    for boundary in 1..=steps {
        let (path, database) = fresh_sqlite(&format!("concern-rollback-{boundary}"));
        let error = stage_validated_current_schema(
            &database,
            &request,
            FailureInjection::after_completed_step(boundary),
        )
        .expect_err("injected concern schema failure");
        assert!(
            error
                .to_string()
                .contains("injected current-schema failure"),
            "{boundary}: {error}"
        );
        assert_no_relations(&database);
        stage_concern_schema(&database, 4, database_id()).unwrap();
        assert_eq!(database.run_default("::relations").unwrap().rows.len(), 31);
        drop(database);
        remove_fixture(&path);
    }
}

#[test]
fn context_generation_reuses_physical_catalog_but_fences_old_app_contracts() {
    use crate::storage_contract::conventional_unmanaged::admission::{
        CatalogSealGenerationV1, close_episode_context_source_bound_seal_v2,
        recognize_concern_open_source_v1, recognize_episode_context_open_source_v2,
    };
    let (path, database) = fresh_sqlite("context-exact");
    stage_episode_context_schema(&database, 4, database_id()).unwrap();
    let new_names = CatalogContract::EpisodeContextV2
        .relations()
        .map(|r| r.name)
        .collect::<BTreeSet<_>>();
    let old_names = CatalogContract::ConcernV1
        .relations()
        .map(|r| r.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        new_names, old_names,
        "vendor physical policy reuse requires identical inventory"
    );
    assert_eq!(new_names.len(), 31);
    database.prepare_sqlite_for_file_move().unwrap();
    drop(database);
    let before = fs::read(&path).unwrap();
    assert!(
        recognize_concern_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
            .is_err()
    );
    let admitted = recognize_episode_context_open_source_v2(
        ExistingSqliteSnapshotSource::open(&path).unwrap(),
    )
    .unwrap();
    drop(admitted);
    let reader = ExistingSqliteSnapshotSource::open(&path)
        .unwrap()
        .into_managed_reader(ManagedSnapshotPolicy::ConcernV1)
        .unwrap();
    let seal = close_episode_context_source_bound_seal_v2(reader).unwrap();
    assert_eq!(seal.generation(), CatalogSealGenerationV1::EpisodeContextV2);
    drop(seal);
    assert_eq!(fs::read(&path).unwrap(), before);
    remove_fixture(&path);
}

#[test]
fn touchstones_generation_extends_closed_inventory_and_refuses_prior_admission_unchanged() {
    use crate::storage_contract::conventional_unmanaged::admission::{
        CatalogSealGenerationV1, close_touchstones_source_bound_seal_v1,
        recognize_episode_context_open_source_v2, recognize_touchstones_open_source_v1,
    };
    let (path, database) = fresh_sqlite("touchstones-exact");
    stage_touchstones_schema(&database, 4, database_id()).unwrap();
    let names = CatalogContract::TouchstonesV1
        .relations()
        .map(|r| r.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(names.len(), 34);
    for name in [
        "touchstone",
        "touchstone_target",
        "touchstone_target:by_owner",
    ] {
        assert!(names.contains(name));
    }
    database.prepare_sqlite_for_file_move().unwrap();
    drop(database);
    let before = fs::read(&path).unwrap();
    assert!(
        recognize_episode_context_open_source_v2(
            ExistingSqliteSnapshotSource::open(&path).unwrap()
        )
        .is_err()
    );
    drop(
        recognize_touchstones_open_source_v1(ExistingSqliteSnapshotSource::open(&path).unwrap())
            .unwrap(),
    );
    let reader = ExistingSqliteSnapshotSource::open(&path)
        .unwrap()
        .into_managed_reader(ManagedSnapshotPolicy::TouchstonesV1)
        .unwrap();
    let seal = close_touchstones_source_bound_seal_v1(reader).unwrap();
    assert_eq!(seal.generation(), CatalogSealGenerationV1::TouchstonesV1);
    drop(seal);
    assert_eq!(fs::read(&path).unwrap(), before);
    remove_fixture(&path);
}
