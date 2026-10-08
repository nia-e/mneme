//! Noninteractive admission never opens a store or contacts a source.
use std::process::{Command, Output};
fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mnemed"))
        .env_remove("MNEME_DB")
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn demo_snapshot_is_explicit_and_works_without_a_terminal() {
    let out = run(&["tui", "--demo", "--snapshot"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let screen = String::from_utf8_lossy(&out.stdout);
    assert!(screen.to_lowercase().contains("demo"), "{screen}");
    assert!(
        screen.contains("memory") || screen.contains("MEMORY"),
        "{screen}"
    );
}
#[test]
fn nonterminal_live_invocation_refuses_before_config_or_network() {
    for args in [
        vec!["tui", "--config", "/does/not/exist.json"],
        vec!["--remote", "http://127.0.0.1:1", "tui"],
    ] {
        let out = run(&args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("interactive terminal"));
    }
}
#[test]
fn tui_forbids_local_store_and_json_and_mixed_demo_sources() {
    for args in [
        vec!["--json", "tui", "--demo", "--snapshot"],
        vec!["--db", "/does/not/exist.db", "tui"],
        vec!["tui", "--demo", "--config", "missing.json"],
        vec!["--remote", "http://127.0.0.1:1", "tui", "--demo"],
        vec!["tui", "--snapshot"],
    ] {
        let out = run(&args);
        assert!(!out.status.success(), "{args:?}");
    }
}
#[test]
fn tui_query_validation_precedes_terminal_admission() {
    for text in [" ".to_owned(), "x".repeat(4097)] {
        let out = run(&["tui", "--query", &text]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("4096"));
    }
}
#[test]
fn tui_help_is_discoverable() {
    let out = run(&["tui", "--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--demo") && help.contains("--snapshot") && help.contains("--config"));
}

#[test]
fn scene_preview_and_view_validation_are_offline() {
    let out = run(&["tui", "--demo", "--view", "scenes", "--snapshot"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .to_lowercase()
            .contains("scenes")
    );
    let bad = run(&["tui", "--view", "unknown", "--snapshot", "--demo"]);
    assert!(!bad.status.success());
}

#[test]
fn touchstone_preview_is_offline_and_discoverable() {
    let out = run(&["tui", "--demo", "--view", "touchstones", "--snapshot"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("TOUCHSTONES"));
    let help = run(&["tui", "--help"]);
    assert!(String::from_utf8_lossy(&help.stdout).contains("touchstones"));
}
