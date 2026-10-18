//! Thin CLI encoding of the shared complete tag-set CAS.
use crate::AnyErr;
use clap::Args;
use mneme_app::retag::{self as shared, PreparedRetag};
use serde_json::{Value, json};
#[derive(Args)]
pub(crate) struct RetagArgs {
    id: String,
    /// Complete inspected tag set in one comma-separated argument. For empty, put ID first and pass the flag alone.
    #[arg(long, required=true, num_args=0..=1, value_delimiter=',')]
    expected_tags: Vec<String>,
    /// Complete replacement tag set in one comma-separated argument. For empty, put ID first and pass the flag alone.
    #[arg(long, required=true, num_args=0..=1, value_delimiter=',')]
    tags: Vec<String>,
    /// Optional explicit intended owner identity; mismatch refuses without mutation.
    #[arg(long)]
    pub(crate) expected_db_id: Option<String>,
    /// Canonical content fingerprint returned by get; changed content refuses atomically.
    #[arg(long)]
    expected_content_fingerprint: Option<String>,
    /// Semantic guide ID=FINGERPRINT to guard atomically; repeat as needed.
    #[arg(long = "guard-node", requires = "expected_content_fingerprint")]
    guard_nodes: Vec<String>,
}
pub(crate) struct Prepared {
    pub(crate) request: PreparedRetag,
    pub(crate) expected_db_id: Option<ulid::Ulid>,
}
impl Prepared {
    pub(crate) fn read(args: &RetagArgs) -> Result<Self, AnyErr> {
        let expected_db_id = args
            .expected_db_id
            .as_deref()
            .map(|s| {
                let id: ulid::Ulid = s.parse()?;
                if s != id.to_string() {
                    return Err("expected_db_id must be a canonical ULID".into());
                }
                Ok::<_, AnyErr>(id)
            })
            .transpose()?;
        if args.guard_nodes.len() > mneme_core::MAX_NODE_HYDRATION_BATCH {
            return Err("too many --guard-node entries".into());
        }
        let mut raw = json!({"id":args.id,"expected_tags":args.expected_tags,"tags":args.tags});
        if let Some(fingerprint) = &args.expected_content_fingerprint {
            raw["expected_content_fingerprint"] = json!(fingerprint);
        }
        if !args.guard_nodes.is_empty() {
            let guards = args
                .guard_nodes
                .iter()
                .map(|value| {
                    let (id, fingerprint) = value
                        .split_once('=')
                        .ok_or("--guard-node must be ID=FINGERPRINT")?;
                    Ok::<_, AnyErr>(json!({"id":id,"content_fingerprint":fingerprint}))
                })
                .collect::<Result<Vec<_>, _>>()?;
            raw["guard_nodes"] = json!(guards);
        }
        Ok(Self {
            request: PreparedRetag::parse(&raw).map_err(|error| -> AnyErr { error })?,
            expected_db_id,
        })
    }
    pub(crate) fn payload(&self) -> Value {
        let mut value = self.request.clone().into_json();
        if let Some(id) = self.expected_db_id {
            value["expected_db_id"] = json!(id.to_string());
        }
        value
    }
}
pub(crate) fn render(value: &Value, json_output: bool) {
    if json_output {
        crate::print_json(value)
    } else {
        println!("{}", shared::render_human(value))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    fn parse(args: &[&str]) -> Result<Prepared, AnyErr> {
        let cli = crate::Cli::try_parse_from(args)?;
        let crate::Command::Retag(args) = cli.command else {
            panic!("wrong command")
        };
        Prepared::read(&args)
    }
    #[test]
    fn explicit_empty_and_nonempty_flags() {
        let id = ulid::Ulid::from(1).to_string();
        let p = parse(&["mnemed", "retag", &id, "--expected-tags", "--tags"]).unwrap();
        assert_eq!(p.payload()["tags"], json!([]));
        let p = parse(&[
            "mnemed",
            "retag",
            &id,
            "--expected-tags",
            "fact,possibility",
            "--tags",
            "fact,closed",
        ])
        .unwrap();
        assert_eq!(p.payload()["expected_tags"], json!(["fact", "possibility"]));
        assert!(parse(&["mnemed", "retag", &id, "--tags"]).is_err());
        assert!(parse(&["mnemed", "retag", &id, "--expected-tags", "--tags", ""]).is_err());
        assert!(parse(&["mnemed", "retag", &id, "--expected-tags", "a,a", "--tags"]).is_err());
    }

    #[test]
    fn comma_separated_values_allow_id_before_between_or_after_flags() {
        let id = ulid::Ulid::from(1).to_string();
        for expected in [
            vec!["--expected-tags", "fact,possibility"],
            vec!["--expected-tags=fact,possibility"],
        ] {
            for replacement in [vec!["--tags", "fact,closed"], vec!["--tags=fact,closed"]] {
                for reversed in [false, true] {
                    let (first, second) = if reversed {
                        (&replacement, &expected)
                    } else {
                        (&expected, &replacement)
                    };
                    for id_position in [0, first.len(), first.len() + second.len()] {
                        let mut flags = first.clone();
                        flags.extend(second);
                        flags.insert(id_position, &id);
                        let mut args = vec!["mnemed", "retag"];
                        args.extend(flags);
                        let payload = parse(&args)
                            .unwrap_or_else(|error| panic!("{args:?}: {error}"))
                            .payload();
                        assert_eq!(payload["id"], id, "{args:?}");
                        assert_eq!(payload["expected_tags"], json!(["fact", "possibility"]));
                        assert_eq!(payload["tags"], json!(["closed", "fact"]));
                    }
                }
            }
        }
    }

    #[test]
    fn bare_empty_flags_with_id_first_preserve_each_complete_set() {
        let id = ulid::Ulid::from(1).to_string();
        for expected_empty in [false, true] {
            for replacement_empty in [false, true] {
                for reversed in [false, true] {
                    let mut expected = vec!["--expected-tags"];
                    if !expected_empty {
                        expected.push("fact,possibility");
                    }
                    let mut replacement = vec!["--tags"];
                    if !replacement_empty {
                        replacement.push("fact,closed");
                    }
                    let (first, second) = if reversed {
                        (replacement, expected)
                    } else {
                        (expected, replacement)
                    };
                    let mut args = vec!["mnemed", "retag", &id];
                    args.extend(first);
                    args.extend(second);
                    let payload = parse(&args).unwrap().payload();
                    assert_eq!(
                        payload["expected_tags"],
                        if expected_empty {
                            json!([])
                        } else {
                            json!(["fact", "possibility"])
                        },
                        "{args:?}"
                    );
                    assert_eq!(
                        payload["tags"],
                        if replacement_empty {
                            json!([])
                        } else {
                            json!(["closed", "fact"])
                        },
                        "{args:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn tag_flags_remain_mandatory_and_do_not_accept_whitespace_lists() {
        let id = ulid::Ulid::from(1).to_string();
        for args in [
            vec!["mnemed", "retag", &id],
            vec!["mnemed", "retag", &id, "--tags"],
            vec!["mnemed", "retag", &id, "--expected-tags"],
            vec![
                "mnemed",
                "retag",
                &id,
                "--expected-tags",
                "a",
                "b",
                "--tags",
            ],
            vec![
                "mnemed",
                "retag",
                &id,
                "--expected-tags",
                "--tags",
                "a",
                "b",
            ],
        ] {
            assert!(crate::Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
        // An optional value cannot distinguish a tag from a following ID. Do
        // not special-case ULID-shaped tags: empty sets use ID-first instead.
        assert!(
            crate::Cli::try_parse_from(["mnemed", "retag", "--expected-tags", "--tags", &id,])
                .is_err()
        );
        let p = parse(&["mnemed", "retag", &id, "--expected-tags", &id, "--tags"]).unwrap();
        assert_eq!(p.payload()["expected_tags"], json!([id]));
    }

    #[test]
    fn repeated_flags_still_append_values_for_shared_set_validation() {
        let id = ulid::Ulid::from(1).to_string();
        let args = [
            "mnemed",
            "retag",
            &id,
            "--expected-tags",
            "fact",
            "--expected-tags",
            "possibility",
            "--tags=closed",
            "--tags=fact",
        ];
        let payload = parse(&args).unwrap().payload();
        assert_eq!(payload["expected_tags"], json!(["fact", "possibility"]));
        assert_eq!(payload["tags"], json!(["closed", "fact"]));
        assert!(
            parse(&[
                "mnemed",
                "retag",
                &id,
                "--expected-tags",
                "fact",
                "--expected-tags",
                "fact",
                "--tags",
            ])
            .is_err()
        );
    }

    #[test]
    fn content_and_guide_guards_are_preserved_and_checked() {
        let id = ulid::Ulid::from(1).to_string();
        let guide = ulid::Ulid::from(2).to_string();
        let fingerprint = "a".repeat(64);
        let guard = format!("{guide}={fingerprint}");
        let base = ["mnemed", "retag", &id, "--expected-tags", "--tags"];
        let mut args = base.to_vec();
        args.extend([
            "--expected-content-fingerprint",
            &fingerprint,
            "--guard-node",
            &guard,
        ]);
        let prepared = parse(&args).unwrap();
        assert!(prepared.request.requires_content_guards());
        assert_eq!(
            prepared.payload()["expected_content_fingerprint"],
            fingerprint
        );
        assert_eq!(
            prepared.payload()["guard_nodes"],
            json!([{"id":guide,"content_fingerprint":fingerprint}])
        );
        for extra in [
            vec!["--guard-node", guard.as_str()],
            vec!["--expected-content-fingerprint", "invalid"],
            vec![
                "--expected-content-fingerprint",
                &fingerprint,
                "--guard-node",
                "missing-separator",
            ],
            vec![
                "--expected-content-fingerprint",
                &fingerprint,
                "--guard-node",
                &guard,
                "--guard-node",
                &guard,
            ],
        ] {
            let mut args = base.to_vec();
            args.extend(extra);
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }
}
