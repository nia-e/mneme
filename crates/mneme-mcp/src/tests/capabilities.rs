//! Profile catalogs and argument-sensitive authority, independent of dispatch success.

use super::support::{
    capability_request_cases, empty_profile_server, empty_server, schema_accepts, tool_input_schema,
};
use crate::*;

const fn expected_profile_permission(
    profile: CapabilityProfile,
    required: CapabilityClass,
) -> bool {
    match profile {
        CapabilityProfile::ReadOnly => matches!(required, CapabilityClass::ReadOnly),
        CapabilityProfile::ReceiptGrounded => matches!(
            required,
            CapabilityClass::ReadOnly | CapabilityClass::ReceiptGrounded
        ),
        CapabilityProfile::Curator => !matches!(required, CapabilityClass::Operator),
        CapabilityProfile::Operator => true,
    }
}

#[test]
fn capability_profile_defaults_and_feedback_compatibility_are_explicit() {
    let defaults = parse_args_from(std::iter::empty::<String>()).unwrap();
    assert_eq!(
        defaults.capability_profile,
        CapabilityProfile::ReceiptGrounded
    );
    assert!(!defaults.allow_direct_feedback);
    assert!(!defaults.direct_feedback_implied_operator);

    for (spelling, expected) in [
        ("read-only", CapabilityProfile::ReadOnly),
        ("receipt-grounded", CapabilityProfile::ReceiptGrounded),
        ("curator", CapabilityProfile::Curator),
        ("operator", CapabilityProfile::Operator),
    ] {
        let parsed = parse_args_from(["--capability-profile", spelling]).unwrap();
        assert_eq!(parsed.capability_profile, expected);
        assert!(!parsed.allow_direct_feedback);
    }

    let enabled = parse_args_from([
        "--allow-direct-feedback",
        "--db",
        "project=/tmp/mneme-project",
    ])
    .unwrap();
    assert_eq!(enabled.capability_profile, CapabilityProfile::Operator);
    assert!(enabled.allow_direct_feedback);
    assert!(enabled.direct_feedback_implied_operator);
    assert_eq!(
        enabled.dbs,
        vec![("project".into(), PathBuf::from("/tmp/mneme-project"))]
    );

    let explicit = parse_args_from([
        "--capability-profile",
        "operator",
        "--allow-direct-feedback",
    ])
    .unwrap();
    assert_eq!(explicit.capability_profile, CapabilityProfile::Operator);
    assert!(explicit.allow_direct_feedback);
    assert!(!explicit.direct_feedback_implied_operator);

    for profile in ["read-only", "receipt-grounded", "curator"] {
        let error = parse_args_from(["--allow-direct-feedback", "--capability-profile", profile])
            .err()
            .expect("weaker explicit profile must reject direct feedback")
            .to_string();
        assert!(error.contains("only valid"), "{error}");
    }
    assert!(
        parse_args_from(["--capability-profile", "god-mode"])
            .err()
            .expect("unknown profile must fail")
            .to_string()
            .contains("read-only|receipt-grounded|curator|operator")
    );
    assert!(
        parse_args_from([
            "--capability-profile",
            "read-only",
            "--capability-profile",
            "operator",
        ])
        .err()
        .expect("duplicate profile flags must fail")
        .to_string()
        .contains("only once")
    );
}

#[tokio::test]
async fn direct_feedback_is_omitted_and_rejected_by_default() {
    let server = empty_profile_server(CapabilityProfile::default());
    let listed = server.dispatch("tools/list", json!({})).await.unwrap();
    assert!(
        listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|schema| schema["name"] != "feedback")
    );

    let rejected = server
        .dispatch(
            "tools/call",
            json!({
                "name": "feedback",
                "arguments": {
                    "db": "project",
                    "to": ulid::Ulid::new().to_string(),
                    "signal": "relevant",
                },
            }),
        )
        .await
        .unwrap();
    assert_eq!(rejected["isError"], true);
    assert!(
        rejected["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("capability profile"))
    );
}

#[tokio::test]
async fn direct_feedback_opt_in_exposes_and_dispatches_the_tool() {
    let server = empty_server(true);
    let listed = server.dispatch("tools/list", json!({})).await.unwrap();
    assert!(
        listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|schema| schema["name"] == "feedback")
    );

    let dispatched = server
        .dispatch(
            "tools/call",
            json!({
                "name": "feedback",
                "arguments": {
                    "db": "project",
                    "to": ulid::Ulid::new().to_string(),
                    "signal": "relevant",
                },
            }),
        )
        .await
        .unwrap();
    assert_eq!(dispatched["isError"], true);
    let error = dispatched["content"][0]["text"].as_str().unwrap();
    assert!(error.contains("unknown database"), "{error}");
    assert!(
        !error.contains("direct feedback compatibility is disabled"),
        "{error}"
    );
}

#[test]
fn capability_classification_is_closed_over_every_public_tool_and_action() {
    let cases = capability_request_cases();
    let mut classified_names = HashSet::new();
    for (name, arguments, expected) in &cases {
        let parsed = ValidatedToolArguments::parse(name, arguments)
            .unwrap_or_else(|error| panic!("{name} valid request rejected: {error}"));
        assert_eq!(parsed.kind().tool_name(), *name, "{arguments}");
        assert_eq!(parsed.kind().capability_class(), *expected, "{arguments}");
        for profile in [
            CapabilityProfile::ReadOnly,
            CapabilityProfile::ReceiptGrounded,
            CapabilityProfile::Curator,
            CapabilityProfile::Operator,
        ] {
            let expected_permission = expected_profile_permission(profile, *expected)
                && (*name != "feedback" || profile == CapabilityProfile::Operator);
            let capability = CapabilityPolicy::new(
                profile,
                *name == "feedback" && profile == CapabilityProfile::Operator,
            );
            assert_eq!(
                capability.authorize(parsed.kind()).is_ok(),
                expected_permission,
                "{} {name} {arguments}",
                profile.as_str()
            );
        }
        classified_names.insert(*name);
    }

    let schemas = unfiltered_tool_schemas();
    let catalog_size = schemas.len();
    let catalog_names = schemas
        .into_iter()
        .map(|schema| schema["name"].as_str().unwrap().to_owned())
        .collect::<HashSet<_>>();
    assert_eq!(catalog_names.len(), catalog_size, "duplicate catalog name");
    assert_eq!(classified_names.len(), catalog_names.len());
    for name in &catalog_names {
        assert!(
            classified_names.contains(name.as_str()),
            "missing case for {name}"
        );
        let expected_minimum = cases
            .iter()
            .filter(|(case_name, _, _)| *case_name == name)
            .map(|(_, _, class)| *class)
            .min_by_key(|class| match class {
                CapabilityClass::ReadOnly => 0,
                CapabilityClass::ReceiptGrounded => 1,
                CapabilityClass::Curator => 2,
                CapabilityClass::Operator => 3,
            })
            .expect("every catalog tool has a request case");
        assert_eq!(
            catalog_minimum_capability(name),
            Some(expected_minimum),
            "catalog minimum for {name}"
        );
    }
    assert!(catalog_minimum_capability("future_tool").is_none());
}

#[test]
fn golden_tool_catalogs_match_capability_profiles() {
    let cases: &[(CapabilityProfile, &[&str])] = &[
        (
            CapabilityProfile::ReadOnly,
            &[
                "databases",
                "activity",
                "database_control",
                "status",
                "query",
                "recall_context",
                "recall",
                "episode",
                "concern",
                "get",
                "list",
                "neighbors",
                "graph",
                "remote_edges",
                "core",
                "contradictions",
                "merges",
                "walk",
            ],
        ),
        (
            CapabilityProfile::ReceiptGrounded,
            &[
                "databases",
                "activity",
                "database_control",
                "status",
                "query",
                "recall_context",
                "recall",
                "episode",
                "concern",
                "get",
                "list",
                "neighbors",
                "graph",
                "remote_edges",
                "core",
                "contradictions",
                "merges",
                "walk",
                "reflect",
            ],
        ),
        (
            CapabilityProfile::Curator,
            &[
                "databases",
                "activity",
                "database_control",
                "status",
                "query",
                "recall_context",
                "recall",
                "episode",
                "concern",
                "retag",
                "get",
                "list",
                "neighbors",
                "graph",
                "remote_edges",
                "core",
                "ingest",
                "save",
                "capture",
                "link",
                "contradict",
                "contradictions",
                "merges",
                "walk",
                "reflect",
            ],
        ),
        (
            CapabilityProfile::Operator,
            &[
                "databases",
                "activity",
                "database_control",
                "snapshot_create",
                "status",
                "decay",
                "prune",
                "query",
                "recall_context",
                "recall",
                "episode",
                "concern",
                "retag",
                "edit_body",
                "edit_summary",
                "get",
                "list",
                "neighbors",
                "graph",
                "remote_edges",
                "core",
                "ingest",
                "save",
                "capture",
                "forget",
                "link",
                "supersede",
                "contradict",
                "contradictions",
                "reconcile",
                "merges",
                "merge",
                "walk",
                "reflect",
            ],
        ),
    ];

    for (profile, expected) in cases {
        let actual = tool_schemas(CapabilityPolicy::new(*profile, false))
            .into_iter()
            .map(|schema| schema["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(actual, *expected, "{} catalog", profile.as_str());
    }

    let feedback_catalog = tool_schemas(CapabilityPolicy::operator_with_direct_feedback());
    assert!(
        feedback_catalog
            .iter()
            .any(|schema| schema["name"] == "feedback")
    );
}

#[test]
fn profile_catalog_schemas_expose_only_permitted_conditional_variants() {
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
    ] {
        let schemas = tool_schemas(CapabilityPolicy::new(profile, false));
        let control = tool_input_schema(&schemas, "database_control");
        assert_eq!(
            control["properties"]["action"]["enum"],
            json!(["status"]),
            "{} database_control schema",
            profile.as_str()
        );
        assert!(schema_accepts(control, &json!({ "action": "status" })));
        assert!(!schema_accepts(
            control,
            &json!({ "db": "project", "action": "release" })
        ));
    }

    let curator = tool_schemas(CapabilityPolicy::new(CapabilityProfile::Curator, false));
    let ingest = tool_input_schema(&curator, "ingest");
    assert!(schema_accepts(
        ingest,
        &json!({ "db": "project", "summary": "candidate" })
    ));
    assert!(!schema_accepts(
        ingest,
        &json!({ "db": "project", "summary": "candidate", "active": false })
    ));
    assert!(!schema_accepts(
        ingest,
        &json!({ "db": "project", "summary": "active", "active": true })
    ));
    assert!(!schema_accepts(
        ingest,
        &json!({ "db": "project", "summary": "core", "tags": ["core"] })
    ));
    let capture = tool_input_schema(&curator, "capture");
    let candidate = json!({ "db": "project", "source": { "namespace": "codex", "key": "claim-1", "reference": "codex://thread/1" }, "summary": "candidate" });
    assert!(schema_accepts(capture, &candidate));
    let mut linked = candidate.clone();
    linked["links"] = json!([{"to": ulid::Ulid::new().to_string()}]);
    assert!(schema_accepts(capture, &linked));
    linked["links"] = json!([{"to": ulid::Ulid::new().to_string(), "kind": "supersedes"}]);
    assert!(!schema_accepts(capture, &linked));
    let mut active = candidate.clone();
    active["active"] = json!(true);
    assert!(!schema_accepts(capture, &active));
    let mut core = candidate;
    core["tags"] = json!(["core"]);
    assert!(!schema_accepts(capture, &core));

    for profile in [
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
    ] {
        let schemas = tool_schemas(CapabilityPolicy::new(profile, false));
        let reflect = tool_input_schema(&schemas, "reflect");
        assert!(!schema_accepts(
            reflect,
            &json!({ "db": "project", "used": [] })
        ));
        assert!(!schema_accepts(
            reflect,
            &json!({ "db": "project", "receipts": [], "used": [] })
        ));
        assert!(schema_accepts(
            reflect,
            &json!({ "db": "project", "receipts": ["receipt"], "used": [] })
        ));
    }

    let operator = tool_schemas(CapabilityPolicy::operator());
    let control = tool_input_schema(&operator, "database_control");
    assert_eq!(
        control["properties"]["action"]["enum"],
        json!(["status", "release", "resume"])
    );
    assert!(schema_accepts(control, &json!({ "action": "status" })));
    assert!(!schema_accepts(control, &json!({ "action": "release" })));
    assert!(schema_accepts(
        control,
        &json!({ "db": "project", "action": "release" })
    ));
    assert!(!schema_accepts(
        tool_input_schema(&operator, "reflect"),
        &json!({ "db": "project", "used": [] })
    ));
}

#[tokio::test]
async fn crafted_excluded_calls_fail_before_checkout_for_every_profile() {
    for profile in [
        CapabilityProfile::ReadOnly,
        CapabilityProfile::ReceiptGrounded,
        CapabilityProfile::Curator,
        CapabilityProfile::Operator,
    ] {
        let server = empty_profile_server(profile);
        for (name, arguments, required) in capability_request_cases() {
            let permitted = expected_profile_permission(profile, required) && name != "feedback";
            if permitted {
                continue;
            }
            let response = server
                .dispatch(
                    "tools/call",
                    json!({ "name": name, "arguments": arguments }),
                )
                .await
                .unwrap();
            assert_eq!(response["isError"], true, "{profile:?} {name}");
            let error = response["content"][0]["text"].as_str().unwrap();
            assert!(
                error.contains("capability profile")
                    || error.contains("direct feedback compatibility is disabled"),
                "{profile:?} {name}: {error}"
            );
            assert!(
                !error.contains("unknown database") && !error.contains("unknown walk session"),
                "{profile:?} {name} reached state checkout before denial: {error}"
            );
        }
    }
}

#[tokio::test]
async fn curator_ingest_and_grounded_reflect_are_argument_sensitive_before_checkout() {
    let curator = empty_profile_server(CapabilityProfile::Curator);
    for arguments in [json!({ "db": "missing", "summary": "ordinary" })] {
        let response = curator
            .dispatch(
                "tools/call",
                json!({ "name": "ingest", "arguments": arguments }),
            )
            .await
            .unwrap();
        let error = response["content"][0]["text"].as_str().unwrap();
        assert!(error.contains("unknown database \"missing\""), "{error}");
    }
    let core = curator
        .dispatch(
            "tools/call",
            json!({"name":"ingest","arguments":{"db":"missing","summary":"core","tags":["core"]}}),
        )
        .await
        .unwrap();
    let error = core["content"][0]["text"].as_str().unwrap();
    assert!(error.contains("capability profile"), "{error}");
    assert!(!error.contains("unknown database"), "{error}");
    let malformed = curator
        .dispatch(
            "tools/call",
            json!({
                "name": "ingest",
                "arguments": {
                    "db": "missing", "summary": "candidate", "active": "false",
                },
            }),
        )
        .await
        .unwrap();
    let error = malformed["content"][0]["text"].as_str().unwrap();
    assert!(error.contains("unknown argument `active`"), "{error}");
    assert!(!error.contains("capability profile"), "{error}");
    assert!(!error.contains("unknown database"), "{error}");

    let grounded = empty_profile_server(CapabilityProfile::ReceiptGrounded);
    for arguments in [
        json!({ "db": "missing", "used": [] }),
        json!({ "db": "missing", "receipts": [], "used": [] }),
    ] {
        let response = grounded
            .dispatch(
                "tools/call",
                json!({ "name": "reflect", "arguments": arguments }),
            )
            .await
            .unwrap();
        let error = response["content"][0]["text"].as_str().unwrap();
        assert!(!error.contains("capability profile"), "{error}");
        assert!(error.contains("receipts"), "{error}");
        assert!(!error.contains("unknown database"), "{error}");
    }
    let receipted = grounded
        .dispatch(
            "tools/call",
            json!({
                "name": "reflect",
                "arguments": {
                    "db": "missing", "receipts": ["not-a-real-receipt"], "used": [],
                },
            }),
        )
        .await
        .unwrap();
    assert!(
        receipted["content"][0]["text"]
            .as_str()
            .is_some_and(|error| error.contains("unknown database \"missing\"")),
        "{receipted}"
    );
}

#[tokio::test]
async fn capture_capability_and_nested_validation_precede_checkout() {
    let curator = empty_profile_server(CapabilityProfile::Curator);
    let base = json!({ "db": "missing", "source": { "namespace": "codex", "key": "claim-1", "reference": "codex://thread/1" }, "summary": "candidate" });
    let candidate = curator
        .dispatch(
            "tools/call",
            json!({ "name": "capture", "arguments": base.clone() }),
        )
        .await
        .unwrap();
    let candidate_error = candidate["content"][0]["text"].as_str().unwrap();
    assert!(
        candidate_error.contains("unknown database"),
        "{candidate_error}"
    );

    for (extra, expected) in [
        (json!({"active":true}), "unknown argument"),
        (json!({"tags":["core"]}), "capability profile"),
    ] {
        let mut args = base.clone();
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        let denied = curator
            .dispatch("tools/call", json!({"name":"capture","arguments":args}))
            .await
            .unwrap();
        let error = denied["content"][0]["text"].as_str().unwrap();
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("unknown database"), "{error}");
    }

    let mut malformed = base;
    malformed["source"]["key"] = json!(7);
    let denied = curator
        .dispatch(
            "tools/call",
            json!({ "name": "capture", "arguments": malformed }),
        )
        .await
        .unwrap();
    let error = denied["content"][0]["text"].as_str().unwrap();
    assert!(
        !error.contains("unknown database") && !error.contains("capability profile"),
        "{error}"
    );
    let mut malformed_links = json!({ "db": "missing", "source": { "namespace": "codex", "key": "claim-1", "reference": "codex://thread/1" }, "summary": "candidate" });
    malformed_links["links"] = json!([{"to": "not-a-ulid"}]);
    let denied = curator
        .dispatch(
            "tools/call",
            json!({ "name": "capture", "arguments": malformed_links }),
        )
        .await
        .unwrap();
    let error = denied["content"][0]["text"].as_str().unwrap();
    assert!(
        !error.contains("unknown database") && !error.contains("capability profile"),
        "{error}"
    );
}

#[tokio::test]
async fn shared_server_handle_exposes_and_enforces_one_effective_profile() {
    let server = empty_profile_server(CapabilityProfile::ReadOnly);
    let initialize = server
        .handle(json!({
            "jsonrpc": "2.0",
            "id": "init",
            "method": "initialize",
            "params": { "protocolVersion": PROTOCOL_VERSION },
        }))
        .await
        .unwrap();
    let initialize: Value = serde_json::from_slice(initialize.bytes()).unwrap();
    assert_eq!(initialize["result"]["capabilityProfile"], "read-only");
    assert_eq!(
        initialize["result"]["serverInfo"]["capabilityProfile"],
        "read-only"
    );
    assert_eq!(
        initialize["result"]["instructions"],
        format!(
            "Mneme memory. Profile: read-only. {}",
            mneme_app::episode::MEMORY_TIME_GUIDANCE
        )
    );

    let listed = server
        .handle(json!({
            "jsonrpc": "2.0",
            "id": "list",
            "method": "tools/list",
            "params": {},
        }))
        .await
        .unwrap();
    let listed: Value = serde_json::from_slice(listed.bytes()).unwrap();
    assert!(
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|schema| schema["name"] != "ingest")
    );

    let denied = server
        .handle(json!({
            "jsonrpc": "2.0",
            "id": "call",
            "method": "tools/call",
            "params": {
                "name": "ingest",
                "arguments": { "db": "missing", "summary": "candidate" },
            },
        }))
        .await
        .unwrap();
    let denied: Value = serde_json::from_slice(denied.bytes()).unwrap();
    let error = denied["result"]["content"][0]["text"].as_str().unwrap();
    assert!(error.contains("capability profile"), "{error}");
    assert!(!error.contains("unknown database"), "{error}");
}
