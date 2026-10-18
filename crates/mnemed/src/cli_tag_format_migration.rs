//! Explicit offline lexical migration. Never resolves an ambient owner, starts
//! a service, opens a second live writer, or invokes an inference runtime.

use std::io::Read;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use mneme_cozo::tag_format_migration::{MAX_TAG_FORMAT_OPTIONS_BYTES, TagFormatMigrationOptions};
use serde_json::{Value, json};
use ulid::Ulid;

use crate::AnyErr;

#[derive(Args)]
pub(crate) struct TagFormatMigrationArgs {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Read a complete legacy tag plan under the explicit offline source lease.
    Plan(Source),
    /// Apply the exact reviewed plan: SQLite transaction or distinct JSON successor.
    Apply {
        #[command(flatten)]
        source: Source,
        /// plan.plan_sha256 from the reviewed complete plan for this source.
        #[arg(long)]
        expected_plan_sha256: String,
        /// Required for JSON apply only: distinct absent successor; source stays intact.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Args)]
struct Source {
    #[arg(long, value_enum, default_value = "sqlite")]
    backend: Backend,
    /// Existing absolute source path. Release/stop its legitimate owner first;
    /// the ordinary exclusive lease is required, never bypassed.
    #[arg(long)]
    source: PathBuf,
    /// Exact logical database identity from the intended owner/enrollment.
    #[arg(long)]
    expected_db_id: String,
    /// Optional reviewed JSON {"aliases":{"C++":"cpp"}}. No alias guesses.
    #[arg(long, value_name = "PATH")]
    aliases: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Backend {
    Sqlite,
    Json,
}

struct PreparedSource {
    path: PathBuf,
    expected_db_id: Ulid,
    options: TagFormatMigrationOptions,
}

impl Source {
    fn prepare(&self) -> Result<PreparedSource, AnyErr> {
        let expected_db_id: Ulid = self.expected_db_id.parse()?;
        if expected_db_id.is_nil() || expected_db_id.to_string() != self.expected_db_id {
            return Err("expected-db-id must be the canonical non-nil database ULID".into());
        }
        if !self.source.is_absolute() {
            return Err(
                "tag-format migration --source must be an absolute existing source path".into(),
            );
        }
        let options = if let Some(path) = &self.aliases {
            let reader = std::fs::File::open(path)?;
            let mut bytes = Vec::new();
            reader
                .take((MAX_TAG_FORMAT_OPTIONS_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_TAG_FORMAT_OPTIONS_BYTES {
                return Err("tag alias input exceeds its 1 MiB encoded envelope".into());
            }
            serde_json::from_slice::<TagFormatMigrationOptions>(&bytes)?
        } else {
            TagFormatMigrationOptions::default()
        };
        options.validate()?;
        let path = mneme_store_path::resolve_configured_store_path(&self.source)?;
        if !std::fs::symlink_metadata(&path)?.is_file() {
            return Err("tag-format migration requires an existing regular source file".into());
        }
        Ok(PreparedSource {
            path,
            expected_db_id,
            options,
        })
    }
}

pub(crate) async fn run(args: &TagFormatMigrationArgs, json_output: bool) -> Result<(), AnyErr> {
    let (source, expected_plan, output) = match &args.action {
        Action::Plan(source) => (source, None, None),
        Action::Apply {
            source,
            expected_plan_sha256,
            output,
        } => {
            if expected_plan_sha256.len() != 64
                || !expected_plan_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err("expected-plan-sha256 must be the reviewed 64 lowercase hexadecimal plan digest".into());
            }
            (
                source,
                Some(expected_plan_sha256.as_str()),
                output.as_deref(),
            )
        }
    };
    let backend = source.backend;
    match backend {
        Backend::Sqlite if output.is_some() => return Err("SQLite tag-format migration is same-path; --output is only for distinct-successor JSON migration".into()),
        Backend::Json if expected_plan.is_some() && output.is_none() => return Err("JSON apply requires a distinct absent --output; in-place JSON migration cannot fence an already-loaded old writer".into()),
        _ => {}
    }
    let source = source.prepare()?;
    match backend {
        Backend::Sqlite => run_sqlite(&source, expected_plan, json_output),
        Backend::Json => {
            let result = crate::cli_single_graph_upgrade::run_tag_format_json(
                &source.path,
                output,
                source.expected_db_id,
                &source.options,
                expected_plan,
            )
            .await;
            match result {
                Ok(value) => render(&value, json_output),
                Err(error) => {
                    // The typed error retains both leases until its exact
                    // publication/residue state has reached the caller.
                    if let Some(value) =
                        crate::cli_single_graph_upgrade::tag_format_failure_report(&error)
                    {
                        render(value, json_output)?;
                    }
                    Err(error)
                }
            }
        }
    }
}

#[cfg(feature = "cozo")]
fn run_sqlite(
    source: &PreparedSource,
    expected_plan: Option<&str>,
    json_output: bool,
) -> Result<(), AnyErr> {
    let lease = mneme_store_path::StoreLease::acquire(&source.path)?;
    let mut migration = mneme_cozo::SqliteTagFormatMigration::open_legacy(
        &source.path,
        source.expected_db_id,
        lease,
    )?;
    let plan = if let Some(expected_plan) = expected_plan {
        migration.apply(&source.options, expected_plan)?
    } else {
        migration.plan(&source.options)?
    };
    // Acknowledgement follows checkpoint/close, never before the new marker is
    // durable. Lost acknowledgement is not an invitation to invent a replay.
    migration.checkpoint()?;
    let value = json!({
        "schema":"mneme.tag-format-migration.v1", "source":source.path,
        "db_id":source.expected_db_id.to_string(),
        "action":if expected_plan.is_some() {"apply"} else {"plan"},
        "applied":expected_plan.is_some(), "plan":plan,
        "source_tag_policy":"canonical_node_tag_membership_v2_f7",
        "target_tag_policy":"canonical_node_tag_membership_v2_ascii_slug_v1",
        "historical_source_proofs":"preserved_exactly",
        "activation":"same_path_atomic_sqlite_transaction; owner remains deliberately released until explicit operator resume",
    });
    render(&value, json_output)
}

#[cfg(not(feature = "cozo"))]
fn run_sqlite(
    _source: &PreparedSource,
    _expected_plan: Option<&str>,
    _json_output: bool,
) -> Result<(), AnyErr> {
    Err("SQLite tag-format migration requires a cozo-enabled native CLI; JSON snapshots use their explicit distinct-successor workflow, never this SQLite writer fence".into())
}

fn render(value: &Value, json_output: bool) -> Result<(), AnyErr> {
    if json_output {
        crate::print_json(value);
    } else if value["status"] == "failed" {
        println!(
            "tag-format migration failed: publication_state={} phase={} operation_id={} db_id={}",
            value["publication_state"], value["phase"], value["operation_id"], value["db_id"]
        );
        println!(
            "source={} output={} bodies={} stage={}",
            value["source"], value["output"], value["bodies"], value["stage"]
        );
        println!(
            "Not activated; preserve every reported path and inspect the outcome before choosing a new absent output. Never replace the source path."
        );
    } else {
        let plan = &value["plan"];
        println!(
            "{} tag-format migration: {} node(s), {} changed, ready={}",
            value["action"].as_str().unwrap_or("unknown"),
            plan["nodes_scanned"],
            plan["nodes_changed"],
            plan["ready"]
        );
        println!(
            "source={} db_id={} policy={}",
            value["source"], value["db_id"], plan["normalization_version"]
        );
        println!(
            "preimage_sha256={}",
            plan["preimage_sha256"].as_str().unwrap_or("missing")
        );
        for rename in plan["renames"].as_array().into_iter().flatten() {
            println!(
                "  {} -> {} ({} node(s))",
                rename["original"], rename["replacement"], rename["affected_nodes"]
            );
        }
        for blocker in plan["blockers"].as_array().into_iter().flatten() {
            println!("  BLOCKED {}: {}", blocker["original"], blocker["reason"]);
        }
        for collision in plan["collisions"].as_array().into_iter().flatten() {
            println!(
                "  COLLAPSE {} -> {}",
                collision["originals"], collision["replacement"]
            );
        }
        for unused in plan["unused_aliases"].as_array().into_iter().flatten() {
            println!("  UNUSED EXPLICIT ALIAS {}", unused);
        }
        if plan["options"]["aliases"]
            .as_object()
            .is_some_and(|aliases| !aliases.is_empty())
        {
            println!("explicit_aliases={}", plan["options"]["aliases"]);
        }
        println!(
            "Use --json to retain the complete exact plan and its scope; the digest approves the displayed collision/alias decisions too."
        );
        println!(
            "plan_sha256={}",
            plan["plan_sha256"].as_str().unwrap_or("missing")
        );
        if value["applied"] == true {
            println!(
                "Checkpointed; explicitly resume/restart the matched normalized-tag owner. Historical proofs and witnesses were not rewritten."
            );
        }
    }
    Ok(())
}

// Standalone explicit-path command: keep the shape check separate from running
// it so refusal tests establish no lease, store, model or owner contact.
pub(crate) fn require_no_ambient_selectors(
    db: Option<&Path>,
    user: bool,
    remote: bool,
    memory_config: bool,
    store: bool,
    profile: bool,
) -> Result<(), AnyErr> {
    if db.is_some() || user || remote || memory_config || store || profile {
        return Err("tag-format-migration is an explicit offline --source operation; unset MNEME_DB and omit --db/--user/--remote/--memory-config/--store/--profile; release the legitimate owner separately".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn tag_format_cli_catalog_matches_explicit_backend_shapes() {
        let source = [
            "--source",
            "/not-opened/source",
            "--expected-db-id",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        ];
        let parse = |tail: &[&str]| {
            crate::Cli::try_parse_from(
                ["mnemed", "tag-format-migration"]
                    .into_iter()
                    .chain(tail.iter().copied()),
            )
        };
        let mut plan = vec!["plan"];
        plan.extend(source);
        assert!(parse(&plan).is_ok());
        let mut json_plan = plan.clone();
        json_plan.extend(["--backend", "json"]);
        assert!(parse(&json_plan).is_ok());
        json_plan.extend(["--output", "/not-opened/successor"]);
        assert!(parse(&json_plan).is_err(), "plan never publishes an output");
        let mut apply = vec!["apply"];
        apply.extend(source);
        assert!(parse(&apply).is_err(), "apply requires the reviewed digest");
        let digest = "a".repeat(64);
        apply.extend(["--expected-plan-sha256", digest.as_str()]);
        assert!(parse(&apply).is_ok());
        apply.extend(["--backend", "unknown"]);
        assert!(parse(&apply).is_err());
    }

    #[test]
    fn tag_format_cli_preflight_refuses_before_source_contact() {
        let source = Source {
            backend: Backend::Sqlite,
            source: PathBuf::from("relative-not-opened"),
            expected_db_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            aliases: None,
        };
        assert!(
            source
                .prepare()
                .err()
                .unwrap()
                .to_string()
                .contains("absolute")
        );
        let nil = Source {
            expected_db_id: Ulid::nil().to_string(),
            ..source
        };
        assert!(nil.prepare().err().unwrap().to_string().contains("non-nil"));
    }

    #[test]
    fn tag_format_cli_rejects_ambient_routes_without_a_store() {
        assert!(require_no_ambient_selectors(None, false, false, false, false, false).is_ok());
        assert!(
            require_no_ambient_selectors(
                Some(Path::new("/not-opened")),
                false,
                false,
                false,
                false,
                false
            )
            .is_err()
        );
        for index in 0..5 {
            let mut flags = [false; 5];
            flags[index] = true;
            assert!(
                require_no_ambient_selectors(
                    None, flags[0], flags[1], flags[2], flags[3], flags[4]
                )
                .is_err()
            );
        }
    }
}
