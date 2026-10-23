//! The command catalog is rendered by Clap from `Command`, not copied into a
//! second help table. These tests keep the short workflow guide honest.

use super::{Cli, Command};
use clap::{CommandFactory, Parser, error::ErrorKind};

#[test]
fn root_help_keeps_the_complete_generated_catalog() {
    let catalog = Cli::command();
    let names: Vec<_> = catalog
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .map(|command| command.get_name().to_owned())
        .collect();
    for flag in ["-h", "--help"] {
        let error = Cli::try_parse_from(["mnemed", flag])
            .err()
            .expect("help is not an operation");
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        let help = error.to_string();
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
            43,
            "review the workflow guide when commands change"
        );
        let examples = help
            .split_once("EXAMPLES\n")
            .expect("ordinary examples")
            .1
            .split_once("\n\n")
            .expect("examples end before the generated catalog")
            .0;
        assert_eq!(examples.lines().count(), 4);
        for command in [
            "mnemed save \"a useful note\"",
            "mnemed recall-context \"topic\"",
            "mnemed get NODE_ID",
            "mnemed tui",
        ] {
            assert!(examples.contains(command), "missing example {command}");
        }
        for command in [
            "activity",
            "stores",
            "cache",
            "edit-summary",
            "edit-body",
            "retag",
            "tag-format-migration",
        ] {
            assert!(names.iter().any(|name| name == command));
        }
        assert!(
            Cli::try_parse_from(["mnemed", "stores", "--config", "/metadata/library.json"]).is_ok()
        );
        assert!(!help.contains("episode-upgrade"));
        assert!(!help.contains("upgrade-index"));
        assert!(!help.contains("consolidate"));
        assert!(!help.contains("START HERE"));
        assert!(!help.contains("--target-generation"));
        assert!(help.contains("unavailable owners never fall back to opening a database"));
        assert!(help.contains("--db PATH or MNEME_DB selects offline storage"));
        assert!(help.contains("docs/remote-cli.md"));
        assert!(help.len() < 6000, "root help grew back into an essay");
        assert!(
            help.lines().count() <= 80,
            "root help gained expanded boilerplate"
        );
    }
}

#[test]
fn nested_help_preserves_command_specific_contracts_without_opening_a_store() {
    for (command, expected) in [
        ("activity", "no owner discovery"),
        ("recall-context", "always JSON"),
        ("body", "unbounded escape hatch"),
        ("query", "ANN seeds"),
        ("save", "never creates or upgrades"),
        ("init", "--no-recording"),
        ("capture", "compatibility"),
        ("episode", "semantic ranking and decay do not"),
        (
            "single-graph-upgrade",
            "The source is retained, not activated",
        ),
        ("single-graph-upgrade", "episode-context-v2 for concern-v1"),
        ("tag-format-migration", "never routes to a live owner"),
        ("migrate", "<db>.snapshot.bak"),
        ("reembed", "<db>.reembed.bak"),
        ("feedback", "must not replay an ambiguous invocation"),
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
    for args in [
        vec!["mnemed", "save", "a useful note"],
        vec!["mnemed", "get", "NODE_ID"],
        vec!["mnemed", "tui"],
    ] {
        assert!(Cli::try_parse_from(args).is_ok());
    }
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

#[test]
fn help_is_available_before_owner_selection_in_all_output_modes() {
    for prefix in [
        vec!["mnemed"],
        vec!["mnemed", "--json"],
        vec!["mnemed", "--db", "/absent/memory.db"],
        vec!["mnemed", "--remote", "http://127.0.0.1:1"],
        vec!["mnemed", "--store", "not-configured"],
    ] {
        for suffix in [
            vec!["--help"],
            vec!["save", "--help"],
            vec!["query", "--help"],
        ] {
            let args = prefix.iter().chain(&suffix).copied().collect::<Vec<_>>();
            let error = Cli::try_parse_from(args)
                .err()
                .expect("help is not an operation");
            assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        }
    }
}
