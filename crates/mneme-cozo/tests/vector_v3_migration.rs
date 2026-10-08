//! Admission boundary for a genuine Mnestic 0.8.6 vector-v2 database.
//! Its former in-place v3 upgrader is retired; the fixture remains a physical
//! format canary, not a supported SingleGraph predecessor.
#![cfg(feature = "cozo")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cozo::{DataValue, DbInstance, Num};
use mneme_cozo::CozoStore;
use mneme_store_path::StoreLease;
use ulid::Ulid;

const DIM: usize = 4;
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/pre_0_13_v2/memory.sqlite"
);

struct TempDb(PathBuf);

impl TempDb {
    fn copy_fixture() -> Self {
        let path = std::env::temp_dir().join(format!("mneme-pre013-v2-{}.sqlite", Ulid::new()));
        std::fs::copy(FIXTURE, &path).expect("copy immutable pre-0.13 fixture");
        Self(path)
    }

    fn canonical_path(&self) -> PathBuf {
        // On macOS /var and /private/var name the same file. Snapshot admission
        // requires a single canonical spelling for its source identity fence.
        self.0.canonicalize().unwrap()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let base = self.0.as_os_str().to_string_lossy();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{base}{suffix}"));
        }
    }
}

fn source_bytes(path: &Path) -> Vec<Option<Vec<u8>>> {
    let base = path.as_os_str().to_string_lossy();
    ["", "-wal", "-shm", "-journal"]
        .map(|suffix| std::fs::read(format!("{base}{suffix}")).ok())
        .into_iter()
        .collect()
}

#[test]
fn genuine_pre013_v2_fixture_is_recognizable_but_refuses_current_admission() {
    let temp = TempDb::copy_fixture();
    let path = temp.canonical_path();
    let raw = DbInstance::new("sqlite", &path, "").unwrap();
    let marker = raw
        .run_default("?[v] := *meta{k: 'vector_projection_v2', v}")
        .unwrap();
    assert_eq!(
        marker.rows[0][0].get_str(),
        Some("status_partitioned_hnsw_v2")
    );

    let projected = raw
        .run_default("?[id, data, summary, status] := *node{id, data, status}, *node_search{id, summary} :order id")
        .unwrap();
    assert_eq!(projected.rows.len(), 96);
    let mut statuses = [0_usize; 3];
    let mut punctuation_only = 0;
    for row in &projected.rows {
        let canonical: serde_json::Value = serde_json::from_str(row[1].get_str().unwrap()).unwrap();
        let summary = row[2].get_str().unwrap();
        assert_eq!(canonical["summary"].as_str(), Some(summary));
        punctuation_only += usize::from(summary == "!!!");
        let lane = match row[3].get_str().unwrap() {
            "active" => 0,
            "candidate" => 1,
            "archived" => 2,
            other => panic!("unexpected physical legacy status {other:?}"),
        };
        statuses[lane] += 1;
    }
    assert_eq!(statuses, [64, 16, 16]);
    assert_eq!(punctuation_only, 8);

    let score = raw
        .run_default("?[id, score] := ~node_search:active_fts{id | query: 'needle', k: 1, bind_score: score} :order -score, id")
        .unwrap();
    let top = match &score.rows[0][1] {
        DataValue::Num(Num::Float(value)) => *value,
        other => panic!("expected BM25 float, got {other:?}"),
    };
    assert!((top - 1.409_795_823_433_880_5).abs() < 1e-12);
    drop(raw);

    let before = source_bytes(&path);
    let lease = Arc::new(StoreLease::acquire(&path).unwrap());
    let preflight = CozoStore::require_existing_current(&path, &lease).unwrap_err();
    assert!(
        preflight
            .to_string()
            .contains("touchstones-v1 generation required")
    );
    assert!(CozoStore::open_existing_persistent(&path, DIM, lease).is_err());
    assert_eq!(
        source_bytes(&path),
        before,
        "refused admission mutated the old source"
    );
}
