//! The command catalog is rendered by Clap from `Command`, not copied into a
//! second help table. These tests keep the short workflow guide honest.

use super::{Cli, Command};
use clap::{CommandFactory, Parser, error::ErrorKind};

#[test]
fn root_help_keeps_the_complete_generated_catalog() {
    let mut catalog = Cli::command();
    let names: Vec<_> = catalog
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .map(|command| command.get_name().to_owned())
        .collect();
    let help = catalog.render_help().to_string();
    assert!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with("client ")),
        "hidden integration command leaked into root help"
    );

    for name in &names {
        let matches = help
            .lines()
            .filter(|line| line.trim_start().starts_with(&format!("{name} ")))
            .count();
        assert_eq!(matches, 1, "{name} missing or duplicated in root help");
    }
    assert_eq!(
        names.iter().filter(|name| name.as_str() != "help").count(),
        39,
        "review the workflow guide when commands change"
    );
    for heading in ["Read:", "Inspect:", "Write:", "Curate:", "Maintain:"] {
        assert!(help.contains(heading), "missing workflow heading {heading}");
    }
    assert!(help.contains("recall-context TEXT"));
    assert!(help.contains("episode list"));
    assert!(help.contains("library catalog"));
    assert!(names.iter().any(|name| name == "stores"));
    assert!(
        Cli::try_parse_from(["mnemed", "stores", "--config", "/metadata/library.json"]).is_ok()
    );
    assert!(help.contains("get ID"));
    assert!(help.contains("body ID"));
    assert!(help.contains("save TEXT"));
    assert!(help.contains("concern --input PATH"));
    assert!(help.contains("edit-body ID"));
    assert!(help.contains("edit-summary ID"));
    assert!(names.iter().any(|name| name == "edit-summary"));
    assert!(names.iter().any(|name| name == "edit-body"));
    assert!(help.contains("retag ID"));
    assert!(names.iter().any(|name| name == "retag"));
    assert!(help.contains("capture init"));
    assert!(help.contains("capture add --input PATH"));
    assert!(help.contains("episode append --input PATH"));
    assert!(help.contains("single-graph-upgrade"));
    assert!(!help.contains("episode-upgrade"));
    assert!(!help.contains("upgrade-index"));
    assert!(!help.contains("consolidate"));
    assert!(help.contains(".mneme/cli.json"));
    assert!(help.contains("unavailable owners never fall back to opening a database"));
}

#[test]
fn nested_help_preserves_command_specific_contracts_without_opening_a_store() {
    for (command, expected) in [
        ("recall-context", "always JSON"),
        ("body", "unbounded escape hatch"),
        ("query", "ANN seeds"),
    ] {
        let error = match Cli::try_parse_from(["mnemed", command, "--help"]) {
            Ok(_) => panic!("{command} --help unexpectedly parsed as an operation"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        assert!(
            error.to_string().contains(expected),
            "{command} help lost {expected}"
        );
    }
    let capture_add = match Cli::try_parse_from(["mnemed", "capture", "add", "--help"]) {
        Ok(_) => panic!("capture add --help unexpectedly parsed as an operation"),
        Err(error) => error,
    };
    assert_eq!(capture_add.kind(), ErrorKind::DisplayHelp);
    assert!(capture_add.to_string().contains("namespace/key"));
    for action in [
        "append",
        "list",
        "search",
        "get",
        "revise",
        "history",
        "references",
    ] {
        let error = Cli::try_parse_from(["mnemed", "episode", action, "--help"])
            .err()
            .expect("episode help is not an operation");
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
    }
}

#[test]
fn workflow_guide_does_not_change_parser_or_legacy_alias() {
    assert!(matches!(
        Cli::try_parse_from(["mnemed", "recall-context", "needle"])
            .unwrap()
            .command,
        Command::RecallContext(_)
    ));
    assert!(Cli::try_parse_from(["mnemed", "query", "needle", "--candidates"]).is_err());
    assert!(matches!(
        Cli::try_parse_from(["mnemed", "reindex"]).unwrap().command,
        Command::Reembed
    ));
}

#[test]
fn retired_no_op_consolidate_is_not_callable() {
    for argv in [
        vec!["mnemed", "consolidate"],
        vec![
            "mnemed",
            "--json",
            "consolidate",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        ],
        vec!["mnemed", "--remote", "http://127.0.0.1:1", "consolidate"],
    ] {
        assert_eq!(
            Cli::try_parse_from(argv).err().unwrap().kind(),
            ErrorKind::InvalidSubcommand
        );
    }
}

#[test]
fn retired_whole_graph_communities_dump_is_not_callable() {
    for args in [
        vec!["mnemed", "communities"],
        vec!["mnemed", "--db", "absent.db", "communities"],
    ] {
        assert_eq!(
            Cli::try_parse_from(args).err().unwrap().kind(),
            ErrorKind::InvalidSubcommand
        );
    }
}
