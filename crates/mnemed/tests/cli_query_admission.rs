//! Explicit retrieval input refuses before any persistent path or lease admission.
use std::process::Command;
struct TestDirectory(std::path::PathBuf);
impl TestDirectory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("mneme-query-admission-{}", ulid::Ulid::new()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn refused(
    root: &std::path::Path,
    db: &str,
    operation: &str,
    extra: &[&str],
    json: bool,
    expected: &str,
) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mnemed"));
    command
        .current_dir(root)
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env_remove("MNEME_DB")
        .args(["--db", db]);
    if json {
        command.arg("--json");
    }
    let output = command
        .args([operation, "query"])
        .args(extra)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(expected), "{operation} {extra:?}: {error}");
    assert!(output.stdout.is_empty());
}

#[test]
fn invalid_query_and_context_input_leave_absent_parent_and_store_untouched() {
    let root = TestDirectory::new();
    for operation in ["query", "recall-context"] {
        for json in [false, true] {
            for (extra, expected) in [
                (vec!["--tag", "C++"], "unsupported character"),
                (vec!["--tag", "/"], "no letters or digits"),
                (vec!["--k", "65"], "--k must be"),
                (vec!["--depth", "13"], "--depth must be"),
                (vec!["--max-nodes", "257"], "--max-nodes must be"),
                (vec!["--min-relevance", "NaN"], "--min-relevance must be"),
            ] {
                refused(
                    root.path(),
                    "absent-parent/store.db",
                    operation,
                    &extra,
                    json,
                    expected,
                );
                assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
            }
        }
    }
    refused(
        root.path(),
        "absent-parent/store.db",
        "query",
        &["--bodies", "--max-body-bytes", "1048577"],
        false,
        "body byte limit",
    );
    refused(
        root.path(),
        "absent-parent/store.db",
        "recall-context",
        &["--max-content-bytes", "4095"],
        false,
        "--max-content-bytes",
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn invalid_retrieval_tags_precede_competing_store_lease() {
    let root = TestDirectory::new();
    let db = root.path().join("store.db");
    mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM)
        .save(&db)
        .unwrap();
    let before = std::fs::read(&db).unwrap();
    let _lease = mneme_store_path::StoreLease::acquire(&db).unwrap();
    for operation in ["query", "recall-context"] {
        refused(
            root.path(),
            "store.db",
            operation,
            &["--tag", "C++"],
            false,
            "unsupported character",
        );
    }
    assert_eq!(std::fs::read(&db).unwrap(), before);
}
