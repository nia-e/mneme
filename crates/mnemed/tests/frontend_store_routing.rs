use std::path::Path;
use std::process::{Command, Output};

const FROM: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const TO: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .current_dir(root)
        .args(args)
        .output()
        .expect("run mnemed fixture")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn custom_relative_store_reopens_and_cross_database_targets_fail_closed() {
    let root = std::env::temp_dir().join(format!(
        "mneme-frontend-routing-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();

    #[cfg(feature = "cozo")]
    let initial = run(&root, &["--db", "source.db", "--json", "status"]);
    #[cfg(not(feature = "cozo"))]
    let initial = run(
        &root,
        &[
            "--db",
            "source.db",
            "--json",
            "ingest",
            "--summary",
            "routing fixture",
            "--body-ref",
            "inline://routing-fixture",
        ],
    );
    assert!(
        initial.status.success(),
        "initial custom store creation failed: {}",
        stderr(&initial)
    );

    for attempt in 1..=2 {
        let output = run(&root, &["--db", "source.db", "--json", "status"]);
        assert!(
            output.status.success(),
            "relative custom store open {attempt} failed: {}",
            stderr(&output)
        );
    }

    let typo = run(
        &root,
        &[
            "--db",
            "source.db",
            "--user",
            "link",
            "--from",
            FROM,
            "--to",
            TO,
            "--to-db",
            "missing/target.db",
        ],
    );
    assert!(!typo.status.success());
    assert!(
        stderr(&typo).contains("must already exist"),
        "{}",
        stderr(&typo)
    );
    assert!(
        !root.join("missing").exists(),
        "validation created residue for a typoed target"
    );

    let alias = run(
        &root,
        &[
            "--db",
            "source.db",
            "--user",
            "link",
            "--from",
            FROM,
            "--to",
            TO,
            "--to-db",
            "./source.db",
        ],
    );
    assert!(!alias.status.success());
    assert!(
        stderr(&alias).contains("resolves to the source database"),
        "{}",
        stderr(&alias)
    );

    std::fs::remove_dir_all(root).unwrap();
}
