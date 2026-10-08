//! Canonical SAVE admission against disposable paths and hashing-only stores.
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output};
use ulid::Ulid;

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mneme-cli-save-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        Self { root }
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mnemed"))
            .current_dir(&self.root)
            .env_remove("MNEME_DB")
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn malformed_save_is_rejected_before_checkout_or_parent_creation() {
    let f = Fixture::new();
    for (i, raw) in [
        json!(null),
        json!({"summary":" "}),
        json!({"summary":"x","kind":"wrong"}),
        json!({"summary":"x","operation_id":null}),
        json!({"summary":"x","kind":"episode","tags":["core"]}),
        json!({"summary":"x","links":[{"to":"bad"}]}),
        json!({"summary":"x","source":{},"operation_id":"both"}),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("bad-{i}.json");
        std::fs::write(f.root.join(&name), raw.to_string()).unwrap();
        let out = f.run(&["--db", "must-not-exist/store.db", "save", "--input", &name]);
        assert!(!out.status.success());
        assert!(!f.root.join("must-not-exist").exists());
    }
    for args in [
        vec!["save", "text", "--input", "bad-0.json"],
        vec!["save", "--input", "bad-0.json", "--kind", "note"],
        vec!["save", "text", "--body", "a", "--body-file", "missing"],
    ] {
        assert!(!f.run(&args).status.success());
    }
    std::fs::write(f.root.join("huge.json"), vec![b' '; 1024 * 1024 + 1]).unwrap();
    assert!(
        !f.run(&[
            "--db",
            "must-not-exist/store.db",
            "save",
            "--input",
            "huge.json"
        ])
        .status
        .success()
    );
    assert!(!f.root.join("must-not-exist").exists());
    std::fs::write(f.root.join("huge-body"), vec![b'x'; 256 * 1024 + 1]).unwrap();
    assert!(
        !f.run(&[
            "--db",
            "must-not-exist/store.db",
            "save",
            "x",
            "--body-file",
            "huge-body"
        ])
        .status
        .success()
    );
    assert!(!f.root.join("must-not-exist").exists());
}

#[test]
fn save_never_creates_absent_target_or_falls_back_to_legacy() {
    let f = Fixture::new();
    for kind in ["note", "episode"] {
        let out = f.run(&["--db", "absent.db", "save", "x", "--kind", kind]);
        assert!(!out.status.success());
        assert!(!f.root.join("absent.db").exists());
        assert!(!f.root.join("absent.db.bodies").exists());
        assert!(String::from_utf8_lossy(&out.stderr).contains("operation_id"));
    }
    mneme_cozo::MemStore::new(4)
        .save(f.root.join("legacy.db"))
        .unwrap();
    let before = std::fs::read(f.root.join("legacy.db")).unwrap();
    let out = f.run(&["--db", "legacy.db", "save", "x"]);
    assert!(!out.status.success());
    assert_eq!(std::fs::read(f.root.join("legacy.db")).unwrap(), before);
}

#[cfg(all(feature = "cozo", not(feature = "fastembed")))]
#[test]
fn manual_and_sourced_save_receipts_replay_and_conflict_for_both_kinds() {
    let f = Fixture::new();
    f.ok(&["--db", "memory.db", "--json", "capture", "init"]);
    std::fs::write(f.root.join("body.txt"), "grounded full content").unwrap();
    for kind in ["note", "episode"] {
        let op = format!("manual-{kind}");
        let args = [
            "--db",
            "memory.db",
            "--json",
            "save",
            "A useful observation",
            "--kind",
            kind,
            "--operation-id",
            &op,
            "--body-file",
            "body.txt",
            "--tags",
            "test",
        ];
        let first = f.ok(&args);
        assert_eq!(first["kind"], kind);
        assert_eq!(first["operation_id"], op);
        assert_eq!(first["origin"], "manual_submission");
        assert_eq!(first["replayed"], false);
        assert!(first["db"].as_str().unwrap().ends_with("memory.db"));
        assert!(first["db_id"].is_string());
        assert!(first.get("body").is_none());
        let replay = f.ok(&args);
        assert_eq!(replay["id"], first["id"]);
        assert_eq!(replay["replayed"], true);
        let human = f.run(&[
            "--db",
            "memory.db",
            "save",
            "A useful observation",
            "--kind",
            kind,
            "--operation-id",
            &op,
            "--body-file",
            "body.txt",
            "--tags",
            "test",
        ]);
        assert!(human.status.success());
        assert_eq!(
            String::from_utf8_lossy(&human.stdout).trim(),
            mneme_app::save::render_human(&replay)
        );
        let other = if kind == "note" { "episode" } else { "note" };
        assert!(
            !f.run(&[
                "--db",
                "memory.db",
                "save",
                "A useful observation",
                "--kind",
                other,
                "--operation-id",
                &op
            ])
            .status
            .success()
        );
        std::fs::write(f.root.join("body.txt"), "changed").unwrap();
        assert!(!f.run(&args).status.success());
        std::fs::write(f.root.join("body.txt"), "grounded full content").unwrap();
        let raw = json!({"kind":kind,"summary":"genuine sourced observation","source":{"namespace":"cli-save","key":kind,"reference":"fixture://source"}});
        std::fs::write(f.root.join("source.json"), raw.to_string()).unwrap();
        let sourced = f.ok(&[
            "--db",
            "memory.db",
            "--json",
            "save",
            "--input",
            "source.json",
        ]);
        assert_eq!(sourced["origin"], "provided_source");
        assert!(sourced.get("operation_id").is_none());
        let again = f.ok(&[
            "--db",
            "memory.db",
            "--json",
            "save",
            "--input",
            "source.json",
        ]);
        assert_eq!(again["id"], sourced["id"]);
        assert_eq!(again["replayed"], true);
    }
    let generated = f.run(&[
        "--db",
        "memory.db",
        "--json",
        "save",
        "fresh manual submission",
    ]);
    assert!(generated.status.success());
    let result: Value = serde_json::from_slice(&generated.stdout).unwrap();
    let op = result["operation_id"].as_str().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&generated.stderr)
            .matches("save operation_id:")
            .count(),
        1
    );
    assert!(String::from_utf8_lossy(&generated.stderr).contains(op));
    let retry = f.ok(&[
        "--db",
        "memory.db",
        "--json",
        "save",
        "fresh manual submission",
        "--operation-id",
        op,
    ]);
    assert_eq!(retry["id"], result["id"]);
    assert_eq!(retry["replayed"], true);
}

#[test]
fn unknown_save_store_is_preserved_and_missing_parent_is_not_created() {
    let f = Fixture::new();
    std::fs::write(f.root.join("unknown.db"), b"not a native store").unwrap();
    for kind in ["note", "episode"] {
        let result = f.run(&["--db", "unknown.db", "save", "x", "--kind", kind]);
        assert!(!result.status.success());
        assert_eq!(
            std::fs::read(f.root.join("unknown.db")).unwrap(),
            b"not a native store"
        );
        assert!(!f.root.join("unknown.db.bodies").exists());
        let result = f.run(&[
            "--db",
            "missing-parent/memory.db",
            "save",
            "x",
            "--kind",
            kind,
        ]);
        assert!(!result.status.success());
        assert!(!f.root.join("missing-parent").exists());
    }
}

#[cfg(all(feature = "cozo", not(feature = "fastembed")))]
#[test]
fn save_lease_refusal_leaves_current_store_unchanged_for_both_kinds() {
    let f = Fixture::new();
    f.ok(&["--db", "memory.db", "--json", "capture", "init"]);
    let db = f.root.join("memory.db");
    let before = std::fs::read(&db).unwrap();
    let lease = mneme_store_path::StoreLease::acquire(&db).unwrap();
    for kind in ["note", "episode"] {
        let out = f.run(&["--db", "memory.db", "save", "blocked claim", "--kind", kind]);
        assert!(!out.status.success());
        assert_eq!(std::fs::read(&db).unwrap(), before);
    }
    drop(lease);
    assert!(!f.root.join("memory.db.bodies").exists());
}

#[cfg(all(unix, feature = "cozo", not(feature = "fastembed")))]
#[test]
fn save_alias_uses_canonical_lease_and_multiply_linked_targets_refuse() {
    let f = Fixture::new();
    f.ok(&["--db", "memory.db", "--json", "capture", "init"]);
    let db = f.root.join("memory.db");
    let before = std::fs::read(&db).unwrap();
    std::os::unix::fs::symlink(&db, f.root.join("alias.db")).unwrap();
    let lease = mneme_store_path::StoreLease::acquire(&db).unwrap();
    for kind in ["note", "episode"] {
        assert!(
            !f.run(&[
                "--db",
                "alias.db",
                "save",
                "canonical alias",
                "--kind",
                kind
            ])
            .status
            .success()
        );
        assert_eq!(std::fs::read(&db).unwrap(), before);
    }
    drop(lease);
    let receipt = f.ok(&[
        "--db",
        "alias.db",
        "--json",
        "save",
        "canonical alias",
        "--operation-id",
        "alias-save",
    ]);
    assert_eq!(receipt["db"], db.canonicalize().unwrap().to_str().unwrap());
    let replay = f.ok(&[
        "--db",
        "memory.db",
        "--json",
        "save",
        "canonical alias",
        "--operation-id",
        "alias-save",
    ]);
    assert_eq!(receipt["db_id"], replay["db_id"]);
    assert_eq!(receipt["id"], replay["id"]);
    assert_eq!(replay["replayed"], true);
    let before = std::fs::read(&db).unwrap();
    std::fs::hard_link(&db, f.root.join("hard.db")).unwrap();
    for target in ["memory.db", "hard.db"] {
        assert!(
            !f.run(&["--db", target, "save", "multiply linked"])
                .status
                .success()
        );
        assert_eq!(std::fs::read(&db).unwrap(), before);
    }
}

#[test]
fn touchstone_and_list_malformed_packets_never_create_a_parent() {
    let f = Fixture::new();
    for args in [
        vec!["list", "--touchstones", "--tag", "touchstone"],
        vec!["list", "--touchstones", "--status", "active"],
        vec!["list", "--after", "anything"],
        vec!["list", "--touchstones", "--limit", "0"],
        vec!["list", "--touchstones", "--limit", "33"],
        vec!["list", "--touchstones", "--after", "not-a-native-cursor"],
    ] {
        let mut command = vec!["--db", "must-not-exist/memory.db"];
        command.extend(args);
        let out = f.run(&command);
        assert!(!out.status.success(), "{command:?}");
        assert!(!f.root.join("must-not-exist").exists());
    }
    for (i,raw) in [json!({"summary":"note","touchstone":null}),
        json!({"kind":"episode","summary":"scene","touchstone":{"subject":"meaning","references":[]}}),
        json!({"summary":"note","touchstone":{"subject":"meaning","references":[{"db_id":"bad","id":"bad","expected_snapshot_sha256":"a".repeat(64)}]}})]
        .into_iter().enumerate() {
        let path=format!("bad-touchstone-{i}.json"); std::fs::write(f.root.join(&path),raw.to_string()).unwrap();
        let out=f.run(&["--db","must-not-exist/memory.db","save","--input",&path]);
        assert!(!out.status.success()); assert!(!f.root.join("must-not-exist").exists());
    }
}

#[cfg(all(feature = "cozo", not(feature = "fastembed")))]
#[test]
fn touchstone_cli_get_save_capture_pages_and_replay_after_delete() {
    let f = Fixture::new();
    f.ok(&["--db", "memory.db", "--json", "capture", "init"]);
    let target = f.ok(&[
        "--db",
        "memory.db",
        "--json",
        "save",
        "A historical scene",
        "--operation-id",
        "scene",
    ]);
    let id = target["id"].as_str().unwrap();
    let full = f.ok(&[
        "--db",
        "memory.db",
        "--json",
        "get",
        id,
        "--body",
        "--edges",
    ]);
    assert_eq!(full["summary_snapshot"]["coverage"], "summary_only");
    assert!(full["body"].is_object());
    assert!(full["edges"].is_array());
    let mut reference = full["summary_snapshot"].clone();
    reference.as_object_mut().unwrap().remove("coverage");
    let mut owners = Vec::new();
    for (index, tool) in ["save", "capture"].into_iter().enumerate() {
        let raw = json!({"summary":"Why that mattered","body":"Owner body stays out of browse",
            "source":{"namespace":"test","key":format!("annotation-{index}"),"reference":"test://annotation"},
            "touchstone":{"subject":"The moment that changed the checklist","references":[reference.clone()]}});
        let path = format!("annotation-{index}.json");
        std::fs::write(f.root.join(&path), raw.to_string()).unwrap();
        let result = if tool == "save" {
            f.ok(&["--db", "memory.db", "--json", "save", "--input", &path])
        } else {
            f.ok(&[
                "--db",
                "memory.db",
                "--json",
                "capture",
                "add",
                "--input",
                &path,
            ])
        };
        owners.push(result["id"].clone());
    }
    // Bare tag membership is not native touchstone authorship.
    f.ok(&[
        "--db",
        "memory.db",
        "--json",
        "save",
        "Only tagged",
        "--tags",
        "touchstone",
        "--operation-id",
        "tag-only",
    ]);
    let page = f.ok(&["--db", "memory.db", "list", "--touchstones", "--limit", "1"]);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert!(!page.to_string().contains("Owner body stays out of browse"));
    let second = f.ok(&[
        "--db",
        "memory.db",
        "list",
        "--touchstones",
        "--limit",
        "1",
        "--after",
        page["next_cursor"].as_str().unwrap(),
    ]);
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    // A full final page conservatively offers continuation; follow its
    // terminal empty page rather than treating has_more as guaranteed rows.
    let terminal = f.ok(&[
        "--db",
        "memory.db",
        "list",
        "--touchstones",
        "--limit",
        "1",
        "--after",
        second["next_cursor"].as_str().unwrap(),
    ]);
    assert!(terminal["items"].as_array().unwrap().is_empty());
    assert!(terminal["next_cursor"].is_null());
    assert_ne!(page["items"][0]["id"], second["items"][0]["id"]);
    let before = f.ok(&[
        "--db",
        "memory.db",
        "--json",
        "get",
        owners[0].as_str().unwrap(),
    ]);
    f.ok(&["--db", "memory.db", "--json", "forget", id]);
    let replay = f.ok(&[
        "--db",
        "memory.db",
        "--json",
        "save",
        "--input",
        "annotation-0.json",
    ]);
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["id"], owners[0]);
    let after = f.ok(&[
        "--db",
        "memory.db",
        "--json",
        "get",
        owners[0].as_str().unwrap(),
    ]);
    assert_eq!(before["touchstone"], after["touchstone"]);
    assert_ne!(before["touchstone_current"], after["touchstone_current"]);
    f.ok(&["--db", "other.db", "--json", "capture", "init"]);
    let wrong = f.run(&[
        "--db",
        "other.db",
        "list",
        "--touchstones",
        "--after",
        page["next_cursor"].as_str().unwrap(),
    ]);
    assert!(!wrong.status.success());
}

#[cfg(all(feature = "cozo", not(feature = "fastembed")))]
#[test]
fn retag_changes_tags_only_and_refuses_stale_or_wrong_owner_intent() {
    let f = Fixture::new();
    f.ok(&["--db", "memory.db", "--json", "capture", "init"]);
    let saved = f.ok(&[
        "--db",
        "memory.db",
        "--json",
        "save",
        "An authored possibility",
        "--body",
        "full unchanged content",
        "--operation-id",
        "retag-fixture",
        "--tags",
        "possibility",
    ]);
    let id = saved["id"].as_str().unwrap();
    let db_id = saved["db_id"].as_str().unwrap();
    let get = ["--db", "memory.db", "--json", "get", id];
    let before = f.ok(&get);
    let args = [
        "--db",
        "memory.db",
        "--json",
        "retag",
        id,
        "--expected-tags",
        "possibility",
        "--tags",
        "closed,possibility",
        "--expected-db-id",
        db_id,
    ];
    let result = f.ok(&args);
    assert_eq!(result["id"], saved["id"]);
    assert_eq!(result["db_id"], saved["db_id"]);
    assert_eq!(result["tags"], json!(["closed", "possibility"]));
    assert_eq!(result["changed"], true);
    assert_eq!(result.as_object().unwrap().len(), 5);
    let after = f.ok(&get);
    for field in ["id", "summary", "body", "source", "provenance", "status"] {
        assert_eq!(after[field], before[field], "{field}");
    }
    assert_eq!(after["tags"], result["tags"]);
    let stale = f.run(&args);
    assert!(!stale.status.success());
    assert_eq!(f.ok(&get), after);
    let human = f.run(&[
        "--db",
        "memory.db",
        "retag",
        id,
        "--expected-tags",
        "closed,possibility",
        "--tags",
        "closed,possibility",
    ]);
    assert!(human.status.success());
    let mut unchanged = result.clone();
    unchanged["changed"] = json!(false);
    assert_eq!(
        String::from_utf8_lossy(&human.stdout).trim(),
        mneme_app::retag::render_human(&unchanged)
    );
    let wrong = Ulid::new().to_string();
    assert!(
        !f.run(&[
            "--db",
            "memory.db",
            "retag",
            id,
            "--expected-tags",
            "closed,possibility",
            "--tags",
            "--expected-db-id",
            &wrong
        ])
        .status
        .success()
    );
    assert_eq!(f.ok(&get), after);
    let cleared = f.ok(&[
        "--db",
        "memory.db",
        "--json",
        "retag",
        id,
        "--expected-tags",
        "closed,possibility",
        "--tags",
    ]);
    assert_eq!(cleared["tags"], json!([]));
}

#[cfg(all(feature = "cozo", not(feature = "fastembed")))]
#[test]
fn edit_body_preserves_note_fields_and_refuses_stale_owner() {
    let f = Fixture::new();
    f.ok(&["--db", "memory.db", "--json", "capture", "init"]);
    let saved = f.ok(&[
        "--db",
        "memory.db",
        "--json",
        "save",
        "original summary",
        "--body",
        "original body",
        "--tags",
        "unchanged",
    ]);
    let id = saved["id"].as_str().unwrap();
    let get = ["--db", "memory.db", "--json", "get", id];
    let before = f.ok(&get);
    let revision = before["body_revision"].as_str().unwrap();
    std::fs::write(f.root.join("replacement.txt"), "edited body").unwrap();
    let args = [
        "--db",
        "memory.db",
        "--json",
        "edit-body",
        id,
        "--expected-body-revision",
        revision,
        "--body-file",
        "replacement.txt",
        "--expected-db-id",
        saved["db_id"].as_str().unwrap(),
    ];
    let result = f.ok(&args);
    assert_eq!(result["id"], saved["id"]);
    assert_eq!(result["db_id"], saved["db_id"]);
    assert_ne!(result["body_revision"], before["body_revision"]);
    assert_eq!(result.as_object().unwrap().len(), 4);
    let after = f.ok(&get);
    assert_eq!(after["body_revision"], result["body_revision"]);
    assert_eq!(
        f.run(&["--db", "memory.db", "body", id]).stdout,
        b"edited body"
    );
    for field in ["id", "summary", "tags", "source", "provenance", "status"] {
        assert_eq!(after[field], before[field], "{field}");
    }
    assert!(!f.run(&args).status.success());
    assert_eq!(f.ok(&get), after);
    let wrong = Ulid::new().to_string();
    assert!(
        !f.run(&[
            "--db",
            "memory.db",
            "edit-body",
            id,
            "--expected-body-revision",
            after["body_revision"].as_str().unwrap(),
            "--body-file",
            "replacement.txt",
            "--expected-db-id",
            &wrong
        ])
        .status
        .success()
    );
    assert_eq!(f.ok(&get), after);
    std::fs::write(f.root.join("empty.txt"), "").unwrap();
    let human = f.run(&[
        "--db",
        "memory.db",
        "edit-body",
        id,
        "--expected-body-revision",
        after["body_revision"].as_str().unwrap(),
        "--body-file",
        "empty.txt",
    ]);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let final_note = f.ok(&get);
    assert_eq!(f.run(&["--db", "memory.db", "body", id]).stdout, b"");
    assert_eq!(
        String::from_utf8(human.stdout).unwrap().trim(),
        mneme_app::edit_body::render_human(
            &json!({"id":id,"body_revision":final_note["body_revision"]})
        )
    );
}
#[test]
fn edit_body_bad_input_refuses_before_store_creation() {
    let f = Fixture::new();
    let id = Ulid::new().to_string();
    let revision = "a".repeat(64);
    for (name, bytes) in [
        ("big", vec![b'x'; mneme_engine::MAX_CAPTURE_BODY_BYTES + 1]),
        ("invalid", vec![255]),
    ] {
        std::fs::write(f.root.join(name), bytes).unwrap();
        assert!(
            !f.run(&[
                "--db",
                "absent/store.db",
                "edit-body",
                &id,
                "--expected-body-revision",
                &revision,
                "--body-file",
                name
            ])
            .status
            .success()
        );
        assert!(!f.root.join("absent").exists());
    }
    std::fs::write(f.root.join("valid"), "").unwrap();
    assert!(
        !f.run(&[
            "--db",
            "absent/store.db",
            "edit-body",
            &id,
            "--expected-body-revision",
            "wrong",
            "--body-file",
            "valid"
        ])
        .status
        .success()
    );
    assert!(!f.root.join("absent").exists());
    assert!(
        !f.run(&["edit-body", &id, "--body-file", "valid"])
            .status
            .success()
    );
}

#[cfg(all(feature = "cozo", not(feature = "fastembed")))]
#[test]
fn edit_summary_preserves_identity_body_and_refuses_stale_owner() {
    let f = Fixture::new();
    f.ok(&["--db", "memory.db", "--json", "capture", "init"]);
    let saved = f.ok(&[
        "--db",
        "memory.db",
        "--json",
        "save",
        "original summary",
        "--body",
        "original body",
        "--tags",
        "unchanged",
    ]);
    let id = saved["id"].as_str().unwrap();
    let get = ["--db", "memory.db", "--json", "get", id];
    let before = f.ok(&get);
    let guard = before["summary_snapshot"]["expected_snapshot_sha256"]
        .as_str()
        .unwrap();
    let args = [
        "--db",
        "memory.db",
        "--json",
        "edit-summary",
        id,
        "--expected-snapshot-sha256",
        guard,
        "--summary",
        "edited summary",
        "--expected-db-id",
        saved["db_id"].as_str().unwrap(),
    ];
    let result = f.ok(&args);
    assert_eq!(result["id"], saved["id"]);
    assert_eq!(result["db_id"], saved["db_id"]);
    assert_ne!(result["summary_snapshot_sha256"], guard);
    assert_eq!(result.as_object().unwrap().len(), 4);
    let after = f.ok(&get);
    assert_eq!(after["summary"], "edited summary");
    assert_eq!(
        after["summary_snapshot"]["expected_snapshot_sha256"],
        result["summary_snapshot_sha256"]
    );
    assert_eq!(
        f.run(&["--db", "memory.db", "body", id]).stdout,
        b"original body"
    );
    for field in [
        "id",
        "body_revision",
        "tags",
        "source",
        "provenance",
        "status",
    ] {
        assert_eq!(after[field], before[field], "{field}");
    }
    assert!(!f.run(&args).status.success());
    let wrong = Ulid::new().to_string();
    assert!(
        !f.run(&[
            "--db",
            "memory.db",
            "edit-summary",
            id,
            "--expected-snapshot-sha256",
            after["summary_snapshot"]["expected_snapshot_sha256"]
                .as_str()
                .unwrap(),
            "--summary",
            "bad owner",
            "--expected-db-id",
            &wrong
        ])
        .status
        .success()
    );
    assert_eq!(f.ok(&get), after);
    let human = f.run(&[
        "--db",
        "memory.db",
        "edit-summary",
        id,
        "--expected-snapshot-sha256",
        after["summary_snapshot"]["expected_snapshot_sha256"]
            .as_str()
            .unwrap(),
        "--summary",
        "next summary",
    ]);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let final_note = f.ok(&get);
    assert_eq!(
        String::from_utf8(human.stdout).unwrap().trim(),
        mneme_app::edit_summary::render_human(
            &json!({"id":id,"summary_snapshot_sha256":final_note["summary_snapshot"]["expected_snapshot_sha256"]})
        )
    );
}

#[test]
fn edit_summary_bad_input_refuses_before_store_creation() {
    let f = Fixture::new();
    let id = Ulid::new().to_string();
    let guard = "a".repeat(64);
    for summary in [
        "".to_owned(),
        " \n".to_owned(),
        "x".repeat(mneme_core::MAX_NODE_SUMMARY_BYTES + 1),
    ] {
        assert!(
            !f.run(&[
                "--db",
                "absent/store.db",
                "edit-summary",
                &id,
                "--expected-snapshot-sha256",
                &guard,
                "--summary",
                &summary
            ])
            .status
            .success()
        );
        assert!(!f.root.join("absent").exists());
    }
    assert!(
        !f.run(&[
            "--db",
            "absent/store.db",
            "edit-summary",
            &id,
            "--expected-snapshot-sha256",
            "wrong",
            "--summary",
            "valid"
        ])
        .status
        .success()
    );
    assert!(!f.root.join("absent").exists());
}
