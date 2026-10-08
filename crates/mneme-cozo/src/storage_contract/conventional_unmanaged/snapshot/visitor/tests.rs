use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use cozo::{
    DbInstance, ExistingSqliteSnapshotSource, ManagedCatalogFixtureV1, ManagedSnapshotPolicy,
    ManagedSqliteSnapshotReader, ScriptMutability,
};
use ulid::Ulid;

use super::super::roles::{
    MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_V1, plan_logical_snapshot_roles_v1,
};
use super::logical::{
    LogicalVisitSinkV1, close_logical_snapshot_visit_v1, finish_closed_logical_snapshot_visit_v1,
    visit_logical_snapshot_event_v1,
};
use crate::storage_contract::conventional_unmanaged::{
    admission::{CatalogCodecClassification, CatalogSealGenerationV1},
    spec::{LEGACY_CATALOG_GENERATION_MARKER, STRUCT_MAP_CATALOG_GENERATION_MARKER},
};

const CURRENT_WITH_REMOTE_ROW_COUNTS: [u64; 16] = [8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1];

struct PersistentSnapshotFixture {
    root: PathBuf,
    path: PathBuf,
}

impl PersistentSnapshotFixture {
    fn isolated_path(label: &str) -> (PathBuf, PathBuf) {
        let temporary_root = std::fs::canonicalize(std::env::temp_dir())
            .expect("canonical temporary-directory path");
        let root = temporary_root.join(format!("{label}-{}", Ulid::new()));
        std::fs::create_dir(&root).expect("create isolated logical snapshot directory");
        #[cfg(unix)]
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("make logical snapshot directory private");
        let path = root.join("memory.db");
        (root, path)
    }

    fn fresh() -> Self {
        let (root, path) = Self::isolated_path("mneme-logical-snapshot");
        let store =
            crate::CozoStore::open_legacy_fixture(path.to_str().expect("UTF-8 test path"), 4)
                .expect("create fresh conventional unmanaged store");
        store
            .prepare_for_file_move()
            .expect("detach fresh conventional unmanaged store");
        drop(store);
        let fixture = Self { root, path };
        fixture.set_vector_generation(STRUCT_MAP_CATALOG_GENERATION_MARKER);
        fixture
    }

    fn reader(&self) -> ManagedSqliteSnapshotReader {
        ExistingSqliteSnapshotSource::open(&self.path)
            .expect("open logical snapshot source")
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .expect("prepare logical snapshot reader")
    }

    fn catalog_fixture(&self, fixture: ManagedCatalogFixtureV1) -> Self {
        let (root, path) = Self::isolated_path("mneme-logical-snapshot-catalog");
        ExistingSqliteSnapshotSource::open(&self.path)
            .expect("open logical snapshot fixture source")
            .backup_to_new_with_catalog_fixture_v1_for_tests(&path, fixture)
            .expect("create closed catalog fixture");
        Self { root, path }
    }

    fn set_vector_generation(&self, value: &str) {
        self.mutate(&format!(
            "?[k, v] <- [['{}', '{value}']] :put meta {{k => v}}",
            crate::vector_projection::META_KEY,
        ));
    }

    fn insert_remote_edge_row(&self) {
        self.mutate(
            "?[from, target_db, target, weight] <- [['source-node', 'target-database', 'target-node', 0.5]] :put remote_edge {from, target_db, target => weight}",
        );
    }

    fn mutate(&self, script: &str) {
        let database = DbInstance::new("sqlite", self.path.to_str().expect("UTF-8 test path"), "")
            .expect("open raw persistent logical snapshot fixture");
        database
            .run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
            .expect("mutate logical snapshot fixture");
        database
            .prepare_sqlite_for_file_move()
            .expect("detach mutated logical snapshot fixture");
        drop(database);
    }

    fn images(&self) -> BTreeMap<OsString, Vec<u8>> {
        directory_images(&self.root)
    }
}

#[tokio::test]
async fn frozen_logical_visitor_refuses_successor_catalog_without_source_changes() {
    let (root, path) =
        PersistentSnapshotFixture::isolated_path("mneme-logical-snapshot-published-current");
    let fixture = PersistentSnapshotFixture { root, path };
    crate::CozoStore::materialize_fresh_current(
        &fixture.path,
        Ulid::new(),
        &crate::MemStore::new(4),
    )
    .await
    .expect("materialize successor");
    let before = fixture.images();
    assert!(close_logical_snapshot_visit_v1(fixture.reader()).is_err());
    assert_eq!(fixture.images(), before);
}

impl Drop for PersistentSnapshotFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn directory_images(root: &Path) -> BTreeMap<OsString, Vec<u8>> {
    std::fs::read_dir(root)
        .expect("read logical snapshot fixture directory")
        .map(|entry| entry.expect("read logical snapshot fixture entry"))
        .filter(|entry| entry.file_type().expect("stat fixture entry").is_file())
        .map(|entry| {
            let name = entry.file_name();
            let bytes = std::fs::read(entry.path()).expect("read logical snapshot fixture image");
            (name, bytes)
        })
        .collect()
}

fn close_error(reader: ManagedSqliteSnapshotReader) -> String {
    match close_logical_snapshot_visit_v1(reader) {
        Ok(_) => panic!("hostile logical snapshot source unexpectedly closed"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn genuine_current_struct_map_source_closes_to_one_exact_logical_store() {
    let fixture = PersistentSnapshotFixture::fresh();
    fixture.insert_remote_edge_row();
    let before = fixture.images();
    let closed = close_logical_snapshot_visit_v1(fixture.reader())
        .expect("genuine current struct-map source must close exactly");

    assert_eq!(
        closed.source_seal().generation(),
        CatalogSealGenerationV1::CurrentStructMap
    );
    assert_eq!(
        closed.source_seal().codec_classification(),
        CatalogCodecClassification::AllStructMapV1
    );
    assert_eq!(closed.source_seal().vector_dimension(), 4);
    assert_eq!(closed.logical_commitment().relations().len(), 16);
    assert_eq!(
        closed.visit_evidence().relation_row_counts(),
        &CURRENT_WITH_REMOTE_ROW_COUNTS
    );
    assert_eq!(closed.visit_evidence().total_row_count(), 10);
    assert_eq!(
        closed.logical_commitment().total_record_count(),
        closed.visit_evidence().total_row_count()
    );
    for (index, relation) in closed.logical_commitment().relations().iter().enumerate() {
        assert_eq!(relation.ordinal(), index as u16 + 1);
        assert_eq!(
            relation.record_count(),
            closed.visit_evidence().relation_row_counts()[index]
        );
    }
    assert_eq!(fixture.images(), before);
}

#[test]
fn retained_backup_handoff_composes_with_the_logical_close_boundary() {
    let source_fixture = PersistentSnapshotFixture::fresh();
    source_fixture.insert_remote_edge_row();
    let source_before = source_fixture.images();
    let (backup_root, backup_path) =
        PersistentSnapshotFixture::isolated_path("mneme-logical-snapshot-backup");
    let backup_fixture = PersistentSnapshotFixture {
        root: backup_root,
        path: backup_path,
    };

    let source = ExistingSqliteSnapshotSource::open(&source_fixture.path)
        .expect("open retained-backup integration source");
    let backup_source = source
        .backup_to_new_snapshot_source_v1(&backup_fixture.path)
        .expect("backup directly into a custody-retaining source");
    source
        .into_managed_reader(ManagedSnapshotPolicy::V1)
        .expect("prepare original source after backup")
        .close_and_verify()
        .expect("strictly close original source after backup");
    let backup_before_close = backup_fixture.images();
    let closed = close_logical_snapshot_visit_v1(
        backup_source
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .expect("prepare retained backup source"),
    )
    .expect("retained backup source must compose with logical close");

    assert_eq!(
        closed.visit_evidence().relation_row_counts(),
        &CURRENT_WITH_REMOTE_ROW_COUNTS
    );
    assert_eq!(closed.visit_evidence().total_row_count(), 10);
    assert_eq!(source_fixture.images(), source_before);
    assert_eq!(backup_fixture.images(), backup_before_close);
}

#[test]
fn repeated_logical_visits_are_deterministic_and_leave_the_source_unchanged() {
    let fixture = PersistentSnapshotFixture::fresh();
    let before = fixture.images();
    let first = close_logical_snapshot_visit_v1(fixture.reader())
        .expect("first genuine logical visit must close");
    assert_eq!(fixture.images(), before);
    let second = close_logical_snapshot_visit_v1(fixture.reader())
        .expect("second genuine logical visit must close");

    assert_eq!(first.logical_commitment(), second.logical_commitment());
    assert_eq!(
        first.visit_evidence().relation_ids(),
        second.visit_evidence().relation_ids()
    );
    assert_eq!(
        first.visit_evidence().relation_row_counts(),
        second.visit_evidence().relation_row_counts()
    );
    assert_eq!(fixture.images(), before);
}

#[test]
fn legacy_generation_is_not_a_managed_logical_snapshot() {
    let fixture = PersistentSnapshotFixture::fresh();
    fixture.set_vector_generation(LEGACY_CATALOG_GENERATION_MARKER);
    let before = fixture.images();
    let error = close_error(fixture.reader());

    assert!(error.contains("current struct-map generation"));
    assert_eq!(fixture.images(), before);

    fixture.set_vector_generation(STRUCT_MAP_CATALOG_GENERATION_MARKER);
    let _closed = close_logical_snapshot_visit_v1(fixture.reader())
        .expect("restored current marker must close");
}

#[test]
fn positional_and_mixed_catalog_codecs_are_rejected_without_source_changes() {
    let source = PersistentSnapshotFixture::fresh();
    for kind in [
        ManagedCatalogFixtureV1::AllPositionalV0,
        ManagedCatalogFixtureV1::AllPositionalV1,
        ManagedCatalogFixtureV1::DeterministicMixed,
    ] {
        let fixture = source.catalog_fixture(kind);
        let before = fixture.images();
        let error = close_error(fixture.reader());
        assert!(error.contains("struct-map v1"), "{kind:?}: {error}");
        assert_eq!(fixture.images(), before, "{kind:?}");
    }
}

#[test]
fn independent_post_close_verifier_rejects_swapped_valid_empty_base_ids() {
    let fixture = PersistentSnapshotFixture::fresh();
    let before = fixture.images();
    let closed = fixture
        .reader()
        .close_with_relation_assertions_and_record_visit_v1(
            LogicalVisitSinkV1::new(),
            |planner| {
                let mut ids = plan_logical_snapshot_roles_v1(planner)?;
                for index in [2_usize, 3_usize] {
                    let rows = planner
                        .physical()
                        .relation_row_counts()
                        .iter()
                        .find(|row| row.relation_id() == ids[index])
                        .map(|row| row.row_count());
                    if rows != Some(0) {
                        return Err(cozo::Error::msg(
                            "swap-attack fixture relations were unexpectedly nonempty",
                        ));
                    }
                }
                ids.swap(2, 3);
                Ok(ids)
            },
            visit_logical_snapshot_event_v1,
        )
        .expect("generic visitor must accept two valid swapped empty ranges");
    let error = match finish_closed_logical_snapshot_visit_v1(closed) {
        Ok(_) => panic!("independent role verifier accepted swapped empty relation ids"),
        Err(error) => error.to_string(),
    };

    assert!(error.contains("visited ids did not match"), "{error}");
    assert_eq!(fixture.images(), before);
}

#[test]
fn mnestic_record_visitor_policy_identity_is_a_pinned_literal() {
    assert_eq!(
        MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_V1,
        [
            239, 198, 251, 90, 175, 145, 95, 155, 127, 112, 219, 59, 247, 119, 16, 150, 121, 86,
            44, 137, 198, 227, 3, 65, 195, 148, 65, 189, 136, 135, 190, 218,
        ]
    );

    let fixture = PersistentSnapshotFixture::fresh();
    let closed = close_logical_snapshot_visit_v1(fixture.reader())
        .expect("genuine visitor evidence must use the pinned policy");
    assert_eq!(
        closed.visit_evidence().policy_fingerprint(),
        &MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_V1
    );
}
