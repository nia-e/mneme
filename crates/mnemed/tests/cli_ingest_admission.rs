//! Raw ingestion must validate and own all input before persistent admission,
//! including when the selected store is absent or owned by another process.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use ulid::Ulid;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mneme-cli-ingest-{}", Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        Self { root }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mnemed"));
        command
            .current_dir(&self.root)
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env_remove("MNEME_DB");
        command
    }

    fn run(&self, db: &str, json: bool, args: &[String]) -> Output {
        let mut command = self.command();
        command.args(["--db", db]);
        if json {
            command.arg("--json");
        }
        command.arg("ingest").args(args).output().unwrap()
    }

    fn entries(&self) -> Vec<String> {
        let mut entries: Vec<_> = std::fs::read_dir(&self.root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        entries
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn assert_refused(output: &Output, expected: &str) {
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "unexpected ingest success");
    assert!(
        error.contains(expected),
        "expected {expected:?}, got {error}"
    );
    assert!(
        output.stdout.is_empty(),
        "refusal emitted a success receipt"
    );
}

#[test]
fn invalid_fields_and_unreadable_or_oversized_body_leave_no_store_residue() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.root.join("oversized.body"),
        vec![b'x'; 256 * 1024 + 1],
    )
    .unwrap();
    let before = fixture.entries();
    let mut cases = vec![
        (strings(&["--summary", "   "]), "summary must not be blank"),
        (
            vec![
                "--summary".into(),
                "x".repeat(mneme_core::MAX_NODE_SUMMARY_BYTES + 1),
            ],
            "node summary is",
        ),
        (
            strings(&["--summary", "ok", "--tag", "same", "--tag", "same"]),
            "duplicate tag",
        ),
        (
            strings(&["--summary", "ok", "--tag", " untrimmed"]),
            "leading or trailing whitespace",
        ),
        (
            strings(&["--summary", "ok", "--stability", "NaN"]),
            "stability must be finite",
        ),
        (
            strings(&["--summary", "ok", "--confidence", "1.1"]),
            "confidence must be finite",
        ),
        (
            strings(&["--summary", "ok", "--body-ref", "missing-scheme"]),
            "body reference",
        ),
        (
            strings(&["--summary", "ok", "--body-file", "missing.body"]),
            "No such file",
        ),
        (
            strings(&["--summary", "ok", "--body-file", "oversized.body"]),
            "262144-byte body limit",
        ),
    ];
    let mut too_many_tags = strings(&["--summary", "ok"]);
    for n in 0..=mneme_core::MAX_NODE_TAGS {
        too_many_tags.extend(["--tag".into(), format!("tag-{n}")]);
    }
    cases.push((too_many_tags, "tag set has more than"));

    for json in [false, true] {
        for db in ["absent-parent/store.db", "store.db"] {
            for (args, expected) in &cases {
                let output = fixture.run(db, json, args);
                assert_refused(&output, expected);
                assert_eq!(
                    fixture.entries(),
                    before,
                    "refused input created residue: {args:?}"
                );
            }
        }
    }
}

#[test]
fn oversized_stdin_is_bounded_before_store_admission() {
    let fixture = Fixture::new();
    let mut child = fixture
        .command()
        .args([
            "--db",
            "absent-parent/store.db",
            "ingest",
            "--summary",
            "ok",
            "--body-file",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&vec![b'x'; 256 * 1024 + 1])
        .unwrap();
    assert_refused(
        &child.wait_with_output().unwrap(),
        "stdin body exceeds the 262144-byte body limit",
    );
    assert!(fixture.entries().is_empty());
}

#[cfg(unix)]
#[test]
fn invalid_input_precedes_competing_lease_and_valid_aliases_still_contend() {
    let fixture = Fixture::new();
    let db = fixture.root.join("store.db");
    mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM)
        .save(&db)
        .unwrap();
    std::os::unix::fs::symlink(&db, fixture.root.join("alias.db")).unwrap();
    let before = std::fs::read(&db).unwrap();
    let lease = mneme_store_path::StoreLease::acquire(&db).unwrap();
    let entries = fixture.entries();

    for target in ["store.db", "./store.db", "alias.db"] {
        for json in [false, true] {
            assert_refused(
                &fixture.run(target, json, &strings(&["--summary", "   "])),
                "summary must not be blank",
            );
            assert_refused(
                &fixture.run(
                    target,
                    json,
                    &strings(&[
                        "--summary",
                        "ok",
                        "--body-ref",
                        "https://example.invalid/body",
                    ]),
                ),
                "already owned by another mneme process",
            );
        }
    }
    assert_eq!(std::fs::read(&db).unwrap(), before);
    assert_eq!(fixture.entries(), entries);
    drop(lease);
}

#[test]
fn remote_body_limits_and_binary_refusal_precede_connection() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join("binary.body"), [0, 0xff, 0x80]).unwrap();
    let before = fixture.entries();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    for (args, expected) in [
        (
            strings(&["--summary", "ok", "--body-file", "binary.body"]),
            "remote ingest body must be UTF-8 text",
        ),
        (
            strings(&[
                "--summary",
                "ok",
                "--body-ref",
                "https://example.invalid/body",
            ]),
            "remote ingest --body-ref is unavailable",
        ),
        (
            vec!["--summary".into(), "x".repeat(2049)],
            "summary must be 1..=2048 UTF-8 bytes",
        ),
    ] {
        let output = fixture
            .command()
            .args(["--remote", &url, "ingest"])
            .args(args)
            .output()
            .unwrap();
        assert_refused(&output, expected);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(fixture.entries(), before);
    }
}

#[cfg(not(feature = "fastembed"))]
#[test]
fn local_binary_and_borrowed_bodies_keep_existing_semantics_and_receipts() {
    let fixture = Fixture::new();
    let bytes = [0, 0xff, 0x80];
    std::fs::write(fixture.root.join("binary.body"), bytes).unwrap();
    let output = fixture.run(
        "store.db",
        true,
        &strings(&["--summary", "binary fixture", "--body-file", "binary.body"]),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt.as_object().unwrap().len(), 1);
    let binary_id = receipt["id"].as_str().unwrap();
    let body = fixture
        .command()
        .args(["--db", "store.db", "body", binary_id, "--raw"])
        .output()
        .unwrap();
    assert!(
        body.status.success(),
        "{}",
        String::from_utf8_lossy(&body.stderr)
    );
    assert_eq!(body.stdout, bytes);

    let output = fixture.run(
        "store.db",
        false,
        &strings(&[
            "--summary",
            "borrowed fixture",
            "--body-ref",
            "https://example.invalid/body",
        ]),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let id = text.trim();
    assert!(id.parse::<Ulid>().is_ok());
    let got = fixture
        .command()
        .args(["--db", "store.db", "--json", "get", id])
        .output()
        .unwrap();
    assert!(
        got.status.success(),
        "{}",
        String::from_utf8_lossy(&got.stderr)
    );
    let node: serde_json::Value = serde_json::from_slice(&got.stdout).unwrap();
    assert_eq!(node["body_ref"], "https://example.invalid/body");
    assert_eq!(node["body_ownership"], "borrowed");
}
