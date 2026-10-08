use super::*;
use mneme_core::tagged::{
    TaggedExactWorkLimit, TaggedPhysicalSeedCoverage, TaggedPhysicalStatus,
    TaggedQueryTagSeedCoverage, TaggedSeedCoverage,
};

fn query_args(extra: &[&str]) -> QueryArgs {
    let mut argv = vec!["mnemed", "query", "bounded query"];
    argv.extend_from_slice(extra);
    let cli = Cli::try_parse_from(argv).unwrap();
    let Command::Query(args) = cli.command else {
        panic!("expected query command");
    };
    args
}

#[test]
fn query_work_and_body_limits_are_exact() {
    let base = Budget::default();
    let (budget, k, body) = validate_cli_query(&query_args(&[]), base, 5).unwrap();
    assert_eq!(
        (budget.max_nodes, budget.max_depth, k, body),
        (100, 6, 5, 0)
    );

    let args = query_args(&[
        "--k",
        "64",
        "--max-nodes",
        "256",
        "--depth",
        "12",
        "--bodies",
        "--max-body-bytes",
        "1048576",
        "--tag",
        "pitfall",
    ]);
    let (budget, k, body) = validate_cli_query(&args, base, 5).unwrap();
    assert_eq!((budget.max_nodes, budget.max_depth, k), (256, 12, 64));
    assert_eq!(body, MAX_CLI_BODY_BYTES);

    for extra in [
        vec!["--k", "65"],
        vec!["--max-nodes", "257"],
        vec!["--depth", "13"],
        vec!["--min-relevance", "NaN"],
    ] {
        assert!(validate_cli_query(&query_args(&extra), base, 5).is_err());
    }
    assert!(Cli::try_parse_from(["mnemed", "query", "q", "--max-body-bytes", "1"]).is_err());
}

#[test]
fn query_text_and_tags_are_bounded_and_canonical() {
    let base = Budget::default();
    let mut exact = query_args(&[]);
    exact.tags = vec!["x".repeat(256)];
    assert!(validate_cli_query(&exact, base, 5).is_ok());
    exact.tags = vec!["é".repeat(128)];
    assert!(validate_cli_query(&exact, base, 5).is_ok());
    exact.tags = vec!["é".repeat(129)];
    assert!(validate_cli_query(&exact, base, 5).is_err());
    let mut args = query_args(&["--tag", "same", "--tag", "same"]);
    assert!(validate_cli_query(&args, base, 5).is_err());
    args.tags = vec![" untrimmed".into()];
    assert!(validate_cli_query(&args, base, 5).is_err());
    args.tags = vec!["x".repeat(MAX_CLI_QUERY_TAG_BYTES + 1)];
    assert!(validate_cli_query(&args, base, 5).is_err());
    args.tags.clear();
    args.text = "x".repeat(MAX_CLI_QUERY_BYTES + 1);
    assert!(validate_cli_query(&args, base, 5).is_err());
}

#[test]
fn human_warning_names_partial_seed_coverage_without_claiming_final_coverage() {
    assert!(
        partial_seed_warning(RetrievalLane::Primary, &TaggedSeedCoverage::ExactCosine).is_none()
    );
    let physical = TaggedPhysicalSeedCoverage::new(
        TaggedPhysicalStatus::Active,
        0,
        0,
        vec![TaggedQueryTagSeedCoverage::new(0, 1, 1).unwrap()],
        7,
    )
    .unwrap();
    let coverage = TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter {
        exceeded_limit: TaggedExactWorkLimit::RawMemberships,
        raw_memberships: 4_097,
        physical: vec![physical],
        canonical_candidates_checked: 1,
        matching_candidates: 1,
    };
    let warning = partial_seed_warning(RetrievalLane::Primary, &coverage).unwrap();
    assert!(warning.contains("primary seed_coverage is partial"));
    assert!(warning.contains("not final graph-hit coverage"));
    assert_eq!(warning.matches("seed_coverage").count(), 1);
}
