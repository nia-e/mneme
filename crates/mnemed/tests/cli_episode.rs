//! Episode admission and CLI continuity against a disposable database. The
//! runtime package may use either backend; no live Mneme configuration is read.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};
use ulid::Ulid;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mneme-cli-episode-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        Self { root }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mnemed"))
            .current_dir(&self.root)
            .env_remove("MNEME_DB")
            .args(args)
            .output()
            .expect("run episode command")
    }

    fn ok(&self, args: &[&str]) -> Value {
        let result = self.run(args);
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice(&result.stdout).unwrap()
    }

    fn write(&self, name: &str, value: &Value) {
        std::fs::write(self.root.join(name), value.to_string()).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn append_request(key: &str) -> Value {
    json!({
        "source":{"namespace":"cli-test","key":key,"reference":"test://episodic-cli"},
        "summary":"A violet evening, not a general lesson",
        "body":"We watched the violet lights.",
        "occurred":{"kind":"point","at":1000},
        "thread":"lantern"
    })
}

#[test]
fn malformed_episode_packets_leave_no_parent_store_lease_or_body_directory() {
    let fixture = Fixture::new();
    let base = append_request("malformed");
    let mut cases = vec![json!(null), json!([])];
    for (key, value) in [
        ("source", Value::Null),
        ("summary", json!(" ")),
        ("thread", json!(" padded ")),
        ("occurred", json!({"kind":"range","start":20,"end":10})),
        ("links", json!([{"to":"not-an-id"}])),
        ("active", json!(true)),
        ("tags", json!(["core"])),
        ("action", json!("revise")),
    ] {
        let mut case = base.clone();
        case[key] = value;
        cases.push(case);
    }
    for (index, case) in cases.iter().enumerate() {
        let name = format!("invalid-{index}.json");
        fixture.write(&name, case);
        let output = fixture.run(&[
            "--db",
            "must-not-exist/store.db",
            "episode",
            "append",
            "--input",
            &name,
        ]);
        assert!(!output.status.success(), "{case}");
        assert!(
            !fixture.root.join("must-not-exist").exists(),
            "{case} admitted a store"
        );
    }
    std::fs::write(
        fixture.root.join("oversized.json"),
        " ".repeat(128 * 1024 + 1),
    )
    .unwrap();
    let result = fixture.run(&[
        "--db",
        "must-not-exist/store.db",
        "episode",
        "append",
        "--input",
        "oversized.json",
    ]);
    assert!(!result.status.success());
    assert!(!fixture.root.join("must-not-exist").exists());
}

#[test]
fn read_and_invalid_cursor_do_not_create_a_missing_database() {
    let fixture = Fixture::new();
    for args in [
        vec!["--db", "missing.db", "episode", "list"],
        vec!["--db", "missing.db", "episode", "search", "violet"],
        vec![
            "--db",
            "missing-parent/store.db",
            "episode",
            "list",
            "--after",
            "bad-cursor",
        ],
        vec![
            "--db",
            "missing-parent/store.db",
            "episode",
            "list",
            "--limit",
            "33",
        ],
    ] {
        let result = fixture.run(&args);
        assert!(!result.status.success(), "{args:?}");
    }
    assert_eq!(std::fs::read_dir(&fixture.root).unwrap().count(), 0);
}

#[test]
fn append_replay_editorial_history_and_semantic_isolation_survive_reopen() {
    let fixture = Fixture::new();
    fixture.write("first.json", &append_request("initial"));
    let append = || {
        fixture.ok(&[
            "--db",
            "episode.db",
            "--json",
            "episode",
            "append",
            "--input",
            "first.json",
        ])
    };
    let first = append();
    assert_eq!(first["action"], "append");
    assert_eq!(first["replayed"], false);
    assert_eq!(first["revision"], 0);
    assert_eq!(first["episode_id"], first["edition_id"]);
    let root = first["episode_id"].as_str().unwrap();
    let replay = append();
    assert_eq!(replay["episode_id"], root);
    assert_eq!(replay["edition_id"], root);
    assert_eq!(replay["replayed"], true);

    let mut edit = append_request("correction");
    edit["summary"] = json!("A violet evening, correctly remembered");
    edit["body"] = json!("We watched the violet lights from the balcony.");
    edit["reason"] = json!("Correct the location without replacing the old account");
    edit["expected_edition_id"] = json!(root);
    fixture.write("edit.json", &edit);
    let revise = || {
        fixture.ok(&[
            "--db",
            "episode.db",
            "--json",
            "episode",
            "revise",
            root,
            "--input",
            "edit.json",
        ])
    };
    let revised = revise();
    assert_eq!(revised["episode_id"], root);
    assert_ne!(revised["edition_id"], root);
    assert_eq!(revised["revision"], 1);
    assert_eq!(revise()["replayed"], true);
    assert_eq!(
        append()["edition_id"],
        root,
        "original capture must replay original edition after revision"
    );

    let current = fixture.ok(&[
        "--db",
        "episode.db",
        "--json",
        "episode",
        "get",
        root,
        "--body",
    ]);
    assert_eq!(current["edition_id"], revised["edition_id"]);
    assert_eq!(current["body"], edit["body"]);
    let original = fixture.ok(&[
        "--db",
        "episode.db",
        "--json",
        "episode",
        "get",
        root,
        "--edition-id",
        root,
        "--body",
    ]);
    assert_eq!(original["body"], append_request("initial")["body"]);
    assert_eq!(original["is_current"], false);
    let raw = fixture.ok(&["--db", "episode.db", "--json", "get", root]);
    assert!(
        raw.get("memory_kind").is_some(),
        "generic get lost the typed facet"
    );

    let timeline = fixture.ok(&["--db", "episode.db", "--json", "episode", "list"]);
    assert_eq!(timeline["items"].as_array().unwrap().len(), 1);
    assert_eq!(timeline["items"][0]["edition_id"], revised["edition_id"]);
    let history = fixture.ok(&[
        "--db",
        "episode.db",
        "--json",
        "episode",
        "history",
        root,
        "--limit",
        "1",
    ]);
    assert_eq!(history["items"][0]["edition_id"], root);
    let next = history["next"]
        .as_str()
        .expect("history has a second edition");
    let remainder = fixture.ok(&[
        "--db",
        "episode.db",
        "--json",
        "episode",
        "history",
        root,
        "--limit",
        "1",
        "--after",
        next,
    ]);
    assert_eq!(remainder["items"][0]["edition_id"], revised["edition_id"]);
    let cue = fixture.ok(&[
        "--db",
        "episode.db",
        "--json",
        "episode",
        "search",
        "violet",
    ]);
    assert_eq!(cue["mode"], "lexical");
    assert_eq!(cue["items"].as_array().unwrap().len(), 1);
    let status = fixture.ok(&["--db", "episode.db", "--json", "status"]);
    assert_eq!(status["episodes"], 1);
    assert_eq!(status["episode_editions"], 2);
    assert!(status.get("candidates").is_none());

    let semantic = fixture.ok(&[
        "--db",
        "episode.db",
        "--json",
        "ingest",
        "--summary",
        "Violet lamps make a reliable evening light",
    ]);
    let context_output = fixture.run(&[
        "--db",
        "episode.db",
        "recall-context",
        "violet",
        "--k",
        "2",
        "--depth",
        "0",
        "--max-nodes",
        "2",
        "--min-relevance",
        "0",
        "--max-content-bytes",
        "4096",
    ]);
    assert!(
        context_output.status.success(),
        "{}",
        String::from_utf8_lossy(&context_output.stderr)
    );
    let context: Value = serde_json::from_slice(&context_output.stdout).unwrap();
    assert_eq!(context["schema"], "mneme.context.v7");
    assert_eq!(context["episodic_retrieval"]["state"], "searched");
    assert_eq!(context["episodic_retrieval"]["mode"], "lexical");
    assert_eq!(context["primary"].as_array().unwrap().len(), 1);
    assert_eq!(context["primary"][0]["id"], semantic["id"]);
    assert_eq!(context["episodes"].as_array().unwrap().len(), 1);
    let episode_card = &context["episodes"][0];
    assert_eq!(episode_card["kind"], "episode");
    assert_eq!(episode_card["episode_id"], root);
    assert_eq!(episode_card["id"], revised["edition_id"]);
    assert_eq!(episode_card["edition_id"], revised["edition_id"]);
    assert_eq!(episode_card["current_edition_id"], revised["edition_id"]);
    assert_eq!(episode_card["revision"], 1);
    assert_eq!(episode_card["occurred"], json!({"kind":"point","at":1000}));
    assert_eq!(context["receipt"], Value::Null);
    assert_eq!(
        context["usage"]["content_bytes"],
        context_output.stdout.len()
    );
    assert!(context_output.stdout.len() <= 4096);
    assert!(!String::from_utf8_lossy(&context_output.stdout).contains("balcony"));

    // Ordinary context includes episodes, but the diagnostic semantic ranking
    // remains semantic-only. Editorial history contributes only its latest card.
    let query = fixture.ok(&[
        "--db",
        "episode.db",
        "--json",
        "query",
        "violet",
        "--k",
        "2",
        "--depth",
        "0",
        "--max-nodes",
        "2",
        "--min-relevance",
        "0",
    ]);
    assert_eq!(
        query["lanes"]["primary"]["hits"].as_array().unwrap().len(),
        1
    );
    assert_eq!(query["lanes"]["primary"]["hits"][0]["id"], semantic["id"]);

    edit["source"]["key"] = json!("stale-correction");
    fixture.write("stale.json", &edit);
    let stale = fixture.run(&[
        "--db",
        "episode.db",
        "episode",
        "revise",
        root,
        "--input",
        "stale.json",
    ]);
    assert!(
        !stale.status.success(),
        "stale editorial CAS must not silently rebase"
    );
    let forbidden = fixture.run(&["--db", "episode.db", "forget", root]);
    assert!(
        !forbidden.status.success(),
        "generic forget erased an episode"
    );
}

#[cfg(feature = "cozo")]
#[test]
fn capture_initialized_single_graph_accepts_episode_and_semantic_writes() {
    let fixture = Fixture::new();
    fixture.ok(&["--db", "capture.db", "--json", "capture", "init"]);
    fixture.write("first.json", &append_request("initial"));
    let result = fixture.run(&[
        "--json",
        "--db",
        "capture.db",
        "episode",
        "append",
        "--input",
        "first.json",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let appended: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(appended["replayed"], false);
    let status = fixture.ok(&["--db", "capture.db", "--json", "status"]);
    assert_eq!(status["nodes"], 1);

    fixture.ok(&[
        "--db",
        "capture.db",
        "--json",
        "ingest",
        "--summary",
        "A violet semantic fact in the same store",
    ]);
    let context = fixture.ok(&[
        "--db",
        "capture.db",
        "recall-context",
        "violet",
        "--depth",
        "0",
        "--min-relevance",
        "0",
    ]);
    assert_eq!(context["schema"], "mneme.context.v7");
    assert_eq!(context["primary"].as_array().unwrap().len(), 1);
    assert_eq!(context["episodes"].as_array().unwrap().len(), 1);
    assert_eq!(context["episodic_retrieval"]["state"], "searched");
}
