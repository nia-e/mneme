//! Library reads are immutable and never resolve `--db`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};
use mneme_library::{LibraryGet, LibraryQuery, LibraryRuntime};
use serde_json::Value;

use crate::AnyErr;

#[derive(Args)]
pub(crate) struct LibraryArgs {
    /// Override the project profile or personal-library config (not a database path).
    /// Defaults to the nearest .mneme/profile.json library_config, then
    /// $XDG_DATA_HOME/mneme/libraries/personal/library.json (or ~/.local/share).
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub action: LibraryAction,
}

#[derive(Subcommand)]
pub(crate) enum LibraryAction {
    /// List enrolled projects and snapshot capture times.
    Catalog,
    /// Search across selected project sources (default: all configured).
    Query(SearchArgs),
    /// Return bounded semantic + lexical episodic context with source provenance.
    RecallContext(SearchArgs),
    /// Read one pinned result; supply --generation for a snapshot hit.
    Get(GetArgs),
}

#[derive(Args)]
pub(crate) struct SearchArgs {
    text: String,
    /// Restrict search to a project; repeatable, at most 64.
    #[arg(long = "project-id")]
    project_ids: Vec<String>,
    /// Restrict retrieval to a tag; repeatable, at most 16.
    #[arg(long = "tag")]
    tags: Vec<String>,
}

#[derive(Args)]
pub(crate) struct GetArgs {
    project_id: String,
    id: String,
    source_device_id: String,
    /// Snapshot generation from the query hit; omit for live-owner hits.
    #[arg(long)]
    generation: Option<String>,
}

fn search(args: &SearchArgs) -> Result<LibraryQuery, AnyErr> {
    if args.text.trim().is_empty() || args.text.len() > 4096 {
        return Err("library query text must be nonblank and at most 4096 UTF-8 bytes".into());
    }
    if args.project_ids.len() > 64 || args.tags.len() > 16 {
        return Err("library accepts at most 64 project IDs and 16 tags".into());
    }
    if args
        .tags
        .iter()
        .any(|tag| tag.is_empty() || tag.len() > 128)
    {
        return Err("library tags must be 1..=128 UTF-8 bytes".into());
    }
    Ok(LibraryQuery {
        text: args.text.clone(),
        project_ids: args.project_ids.clone(),
        tags: args.tags.clone(),
    })
}

fn printable(value: &Value) -> String {
    if value.is_null() {
        return "-".to_owned();
    }
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn age(value: &Value) -> String {
    let Some(captured) = value.as_u64() else {
        return "live".to_owned();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    if captured > now {
        format!("future by {}s", captured - now)
    } else {
        format!("{}s old", now - captured)
    }
}

fn render(action: &LibraryAction, value: &Value, json: bool) {
    if json {
        crate::print_json(value);
        return;
    }
    // Keep the default view useful without hiding machine-readable provenance.
    // The exact bounded envelope remains available with --json.
    match action {
        LibraryAction::Catalog => {
            if let Some(entries) = value.get("entries").and_then(Value::as_array) {
                for entry in entries {
                    println!(
                        "{}  {}  owner={}  revision={}  replicas={}",
                        printable(&entry["project_id"]),
                        printable(&entry["display_name"]),
                        printable(&entry["owner_device_id"]),
                        printable(&entry["revision"]),
                        entry["replicas"].as_array().map_or(0, Vec::len),
                    );
                    if let Some(replicas) = entry["replicas"].as_array() {
                        for replica in replicas {
                            println!(
                                "  snapshot {}  generation={}  captured_at={} ({})",
                                printable(&replica["source_device_id"]),
                                printable(&replica["generation"]),
                                printable(&replica["captured_at"]),
                                age(&replica["captured_at"]),
                            );
                        }
                    }
                }
                if entries.is_empty() {
                    println!("(no library sources)");
                }
            } else {
                crate::print_json(value);
            }
        }
        LibraryAction::Query(_) | LibraryAction::RecallContext(_) => {
            if let Some(primary) = value.get("primary").and_then(Value::as_array) {
                for hit in primary {
                    println!(
                        "{}  {}  {}  {} ({})  [{}:{}] {}",
                        printable(&hit["project_id"]),
                        printable(&hit["id"]),
                        printable(&hit["source"]["device_id"]),
                        printable(&hit["source"]["generation"]),
                        age(&hit["source"]["captured_at"]),
                        printable(&hit["lane"]),
                        printable(&hit["source_rank"]),
                        printable(&hit["summary"]),
                    );
                }
                let episodes = value.get("episodes").and_then(Value::as_array);
                for hit in episodes.into_iter().flatten() {
                    println!(
                        "{}  {}  {}  {} ({})  [episode:{} recorded={}] {}",
                        printable(&hit["project_id"]),
                        printable(&hit["id"]),
                        printable(&hit["source"]["device_id"]),
                        printable(&hit["source"]["generation"]),
                        age(&hit["source"]["captured_at"]),
                        printable(&hit["source_rank"]),
                        printable(&hit["recorded_at"]),
                        printable(&hit["summary"])
                    );
                    println!(
                        "  root={}  revision={}  occurred={}  thread={}",
                        printable(&hit["episode_id"]),
                        printable(&hit["revision"]),
                        printable(&hit["occurred"]),
                        printable(&hit["thread"])
                    );
                }
                if primary.is_empty() && episodes.is_none_or(Vec::is_empty) {
                    println!("(no library hits)");
                }
                if let Some(coverage) = value["coverage"].as_array() {
                    for source in coverage {
                        println!(
                            "coverage {}: {}{}{}",
                            printable(&source["project_id"]),
                            printable(&source["state"]),
                            source["source"]["captured_at"]
                                .as_u64()
                                .map(|captured| format!(
                                    " (captured {captured}, {})",
                                    age(&source["source"]["captured_at"])
                                ))
                                .unwrap_or_default(),
                            source
                                .get("episodic")
                                .map(|episode| format!(
                                    "; episodes={} ({}){}",
                                    printable(&episode["state"]),
                                    printable(&episode["mode"]),
                                    episode
                                        .get("unavailable_reason")
                                        .map(|reason| format!(": {}", printable(reason)))
                                        .unwrap_or_default()
                                ))
                                .unwrap_or_default(),
                        );
                    }
                }
            } else {
                crate::print_json(value);
            }
        }
        LibraryAction::Get(_) => {
            println!(
                "{}  {}  source={} ({})  generation={}  captured_at={} ({})",
                printable(&value["project_id"]),
                printable(&value["db_id"]),
                printable(&value["source"]["device_id"]),
                printable(&value["source"]["kind"]),
                printable(&value["source"]["generation"]),
                printable(&value["source"]["captured_at"]),
                age(&value["source"]["captured_at"]),
            );
            println!(
                "{}\n{}",
                printable(&value["node"]["summary"]),
                printable(&value["node"]["body"])
            );
        }
    }
    if value["partial"] == true {
        eprintln!(
            "[mnemed: library result is partial; inspect --json for unavailable or expired sources]"
        );
    }
}

pub(crate) async fn run(args: &LibraryArgs, json: bool) -> Result<(), AnyErr> {
    // Validate the complete typed request before config load or source access.
    let query = match &args.action {
        LibraryAction::Query(search_args) | LibraryAction::RecallContext(search_args) => {
            Some(search(search_args)?)
        }
        _ => None,
    };
    let config = crate::cli_library_config::resolve(args.config.as_deref())?;
    let library = LibraryRuntime::from_path(&config)?;
    let value = match &args.action {
        LibraryAction::Catalog => library.catalog()?,
        LibraryAction::Query(_) => library.query(query.expect("validated query")).await?,
        LibraryAction::RecallContext(_) => {
            library
                .recall_context(query.expect("validated query"))
                .await?
        }
        LibraryAction::Get(get) => {
            library
                .get(LibraryGet {
                    project_id: get.project_id.clone(),
                    id: get.id.clone(),
                    source_device_id: get.source_device_id.clone(),
                    generation: get.generation.clone(),
                })
                .await?
        }
    };
    render(&args.action, &value, json);
    Ok(())
}
