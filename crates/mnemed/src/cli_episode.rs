//! Thin CLI encoding for the shared, bounded episode operation. Parse the whole
//! request before database resolution or a remote connection; the app owns all
//! domain validation and execution.

use std::io::Read;
use std::path::Path;

use clap::{Args, Subcommand, ValueEnum};
use mneme_app::episode::{self as shared, PreparedEpisode};
use serde_json::{Value, json};

use crate::AnyErr;

const MAX_INPUT_BYTES: usize = 128 * 1024;

/// Episode reads never turn a typo into a database. Appending to an absent
/// target positively creates the current single-graph generation; it must not fall
/// through to the ordinary predecessor-store constructor.
pub(crate) async fn admit_target(db: &Path, action: shared::EpisodeAction) -> Result<(), AnyErr> {
    match std::fs::symlink_metadata(db) {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if action != shared::EpisodeAction::Append {
        return Err(format!(
            "episode database {} does not exist; append an episode or select an existing store",
            db.display()
        )
        .into());
    }
    #[cfg(feature = "cozo")]
    mneme_cozo::CozoStore::materialize_fresh_current(
        db,
        ulid::Ulid::new(),
        &mneme_cozo::MemStore::new(mneme_embed::DEFAULT_DIM),
    )
    .await?;
    Ok(())
}

pub(crate) fn check_target(db: &Path, lease: &mneme_store_path::StoreLease) -> Result<(), AnyErr> {
    #[cfg(feature = "cozo")]
    if !crate::is_snapshot(db)? {
        mneme_cozo::CozoStore::require_existing_current(db, lease)?;
    }
    #[cfg(not(feature = "cozo"))]
    let _ = (db, lease);
    Ok(())
}

#[derive(Subcommand)]
pub(crate) enum EpisodeAction {
    /// Append one selectively authored episode. JSON has source, summary and
    /// optional body, tags, occurred, thread, occurrence_contexts, links. Contexts
    /// name where it happened, not its recorder; omission means unknown. Their
    /// nonempty canonical compact JSON collection must fit 1024 UTF-8 bytes.
    /// Not a transcript import.
    Append(EpisodeInput),
    /// Page current editions in recording or occurrence order. Cursors are
    /// keyset continuations, not snapshots of a changing timeline.
    List(EpisodeList),
    /// Lexical cue search over current episode summaries, not semantic recall.
    Search(EpisodeSearch),
    /// Read an episode's current edition, or an explicit historical edition.
    Get(EpisodeGet),
    /// Append a full replacement edition. JSON requires source, summary,
    /// expected_edition_id and reason; omitted optional fields are reset.
    /// Omitted occurrence_contexts mean unknown, never inherited context.
    /// Exact source-key retries replay their original edition, not a new edit.
    Revise(EpisodeRevise),
    /// Page immutable editions in revision order.
    History(EpisodeHistory),
    /// Read incident evidence links in both directions. ID addresses an exact
    /// semantic node or episode edition, not every edition under a root.
    References(EpisodeReferences),
}

#[derive(Args)]
pub(crate) struct EpisodeInput {
    /// JSON request file; `-` reads stdin. Maximum encoded input: 128 KiB.
    #[arg(long, value_name = "PATH")]
    input: String,
}

#[derive(Args)]
pub(crate) struct EpisodeRevise {
    /// Stable episode root ID.
    episode_id: String,
    #[command(flatten)]
    input: EpisodeInput,
}

#[derive(Clone, Copy, ValueEnum)]
enum Axis {
    Recorded,
    Occurred,
}

#[derive(Clone, Copy, ValueEnum)]
enum Order {
    NewestFirst,
    OldestFirst,
}

#[derive(Args)]
pub(crate) struct EpisodePage {
    /// Number of results, 1..=32 (default 8).
    #[arg(long)]
    limit: Option<usize>,
    /// Opaque cursor returned by the previous matching request.
    #[arg(long)]
    after: Option<String>,
}

#[derive(Args)]
struct EpisodeFilter {
    /// Exact optional episode thread.
    #[arg(long)]
    thread: Option<String>,
    /// Keep only episodes whose occurrence time is unknown.
    #[arg(long, conflicts_with_all = ["occurred_from", "occurred_through"])]
    unknown_occurrence: bool,
    /// Include occurrences overlapping this lower Unix epoch millisecond bound.
    #[arg(long)]
    occurred_from: Option<u64>,
    /// Include occurrences overlapping this upper Unix epoch millisecond bound.
    #[arg(long)]
    occurred_through: Option<u64>,
}

#[derive(Args)]
pub(crate) struct EpisodeList {
    /// Timeline axis (default recorded); occurred excludes unknown times.
    #[arg(long, value_enum)]
    axis: Option<Axis>,
    /// Timeline order (default newest-first).
    #[arg(long, value_enum)]
    order: Option<Order>,
    /// Inclusive lower bound on the selected axis, Unix epoch milliseconds.
    #[arg(long)]
    from: Option<u64>,
    /// Inclusive upper bound on the selected axis, Unix epoch milliseconds.
    #[arg(long)]
    through: Option<u64>,
    #[command(flatten)]
    filter: EpisodeFilter,
    #[command(flatten)]
    page: EpisodePage,
}

#[derive(Args)]
pub(crate) struct EpisodeSearch {
    /// Plain-text lexical cue. At most 4096 UTF-8 bytes.
    cue: String,
    #[command(flatten)]
    filter: EpisodeFilter,
    /// Number of results, 1..=32 (default 8). Search has no ranking cursor.
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Args)]
pub(crate) struct EpisodeGet {
    episode_id: String,
    /// Read this concrete historical edition instead of the current head.
    #[arg(long)]
    edition_id: Option<String>,
    /// Include a bounded body range.
    #[arg(long)]
    body: bool,
    /// Source byte offset; requires --body.
    #[arg(long, requires = "body")]
    offset: Option<u64>,
    /// Body byte budget, 1..=16384; requires --body.
    #[arg(long, requires = "body")]
    max_bytes: Option<usize>,
}

#[derive(Args)]
pub(crate) struct EpisodeHistory {
    episode_id: String,
    #[command(flatten)]
    page: EpisodePage,
}

#[derive(Args)]
pub(crate) struct EpisodeReferences {
    anchor: String,
    #[command(flatten)]
    page: EpisodePage,
}

fn optional(raw: &mut Value, name: &str, value: impl Into<Value>) {
    let value = value.into();
    if !value.is_null() {
        raw[name] = value;
    }
}

impl EpisodePage {
    fn add(&self, raw: &mut Value) {
        optional(raw, "limit", json!(self.limit));
        optional(raw, "after", json!(self.after));
    }
}

impl EpisodeFilter {
    fn add(&self, raw: &mut Value) {
        optional(raw, "thread", json!(self.thread));
        if self.unknown_occurrence {
            raw["occurrence"] = json!({"kind":"unknown"});
        } else if self.occurred_from.is_some() || self.occurred_through.is_some() {
            let mut occurrence = json!({"kind":"overlaps"});
            optional(&mut occurrence, "from", json!(self.occurred_from));
            optional(&mut occurrence, "through", json!(self.occurred_through));
            raw["occurrence"] = occurrence;
        }
    }
}

fn read_bounded(mut reader: impl Read) -> Result<Value, AnyErr> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(format!("episode input exceeds {MAX_INPUT_BYTES} UTF-8 bytes").into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn add_identity(raw: &mut Value, field: &str, expected: &str) -> Result<(), AnyErr> {
    let object = raw
        .as_object_mut()
        .ok_or("episode input must be a JSON object")?;
    if object.get(field).is_some_and(|actual| actual != expected) {
        return Err(format!("episode input `{field}` disagrees with the CLI command").into());
    }
    object.insert(field.into(), json!(expected));
    Ok(())
}

impl EpisodeInput {
    fn read(&self, action: &str) -> Result<Value, AnyErr> {
        let mut raw = if self.input == "-" {
            read_bounded(std::io::stdin().lock())?
        } else {
            read_bounded(std::fs::File::open(&self.input)?)?
        };
        add_identity(&mut raw, "action", action)?;
        Ok(raw)
    }
}

impl EpisodeAction {
    pub(crate) fn prepare(&self) -> Result<PreparedEpisode, AnyErr> {
        let raw = match self {
            Self::Append(input) => input.read("append")?,
            Self::Revise(args) => {
                let mut raw = args.input.read("revise")?;
                add_identity(&mut raw, "episode_id", &args.episode_id)?;
                raw
            }
            Self::List(args) => {
                let mut raw = json!({"action":"list"});
                optional(
                    &mut raw,
                    "axis",
                    args.axis.map(|axis| match axis {
                        Axis::Recorded => "recorded",
                        Axis::Occurred => "occurred",
                    }),
                );
                optional(
                    &mut raw,
                    "order",
                    args.order.map(|order| match order {
                        Order::NewestFirst => "newest_first",
                        Order::OldestFirst => "oldest_first",
                    }),
                );
                optional(&mut raw, "from", json!(args.from));
                optional(&mut raw, "through", json!(args.through));
                args.filter.add(&mut raw);
                args.page.add(&mut raw);
                raw
            }
            Self::Search(args) => {
                let mut raw = json!({"action":"search","cue":args.cue});
                args.filter.add(&mut raw);
                optional(&mut raw, "limit", json!(args.limit));
                raw
            }
            Self::Get(args) => {
                let mut raw = json!({"action":"get","episode_id":args.episode_id});
                optional(&mut raw, "edition_id", json!(args.edition_id));
                if args.body {
                    raw["body"] = json!(true);
                }
                optional(&mut raw, "offset", json!(args.offset));
                optional(&mut raw, "max_bytes", json!(args.max_bytes));
                raw
            }
            Self::History(args) => {
                let mut raw = json!({"action":"history","episode_id":args.episode_id});
                args.page.add(&mut raw);
                raw
            }
            Self::References(args) => {
                let mut raw = json!({"action":"references","anchor":args.anchor});
                args.page.add(&mut raw);
                raw
            }
        };
        PreparedEpisode::parse(&raw).map_err(|error| -> AnyErr { error })
    }
}

pub(crate) fn render(result: &Value, json_output: bool) {
    if json_output {
        crate::print_json(result);
    } else {
        println!("{}", shared::render_human(result));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::io::Cursor;

    fn prepare(argv: &[&str]) -> Result<PreparedEpisode, AnyErr> {
        let mut args = vec!["mnemed", "episode"];
        args.extend_from_slice(argv);
        let crate::Command::Episode { action } = crate::Cli::try_parse_from(args)?.command else {
            unreachable!()
        };
        action.prepare()
    }

    #[test]
    fn flags_encode_shared_wire_without_null_optionals() {
        assert_eq!(
            prepare(&["list"]).unwrap().into_json(),
            json!({"action":"list"})
        );
        assert_eq!(
            prepare(&[
                "list",
                "--axis",
                "occurred",
                "--order",
                "oldest-first",
                "--from",
                "10",
                "--thread",
                "garden",
                "--limit",
                "4"
            ])
            .unwrap()
            .into_json(),
            json!({"action":"list","axis":"occurred","order":"oldest_first","from":10,"thread":"garden","limit":4})
        );
        assert_eq!(
            prepare(&[
                "search",
                "violet",
                "--occurred-from",
                "10",
                "--occurred-through",
                "20"
            ])
            .unwrap()
            .into_json(),
            json!({"action":"search","cue":"violet","occurrence":{"kind":"overlaps","from":10,"through":20}})
        );
    }

    #[test]
    fn all_cli_shapes_reach_shared_validation() {
        for args in [
            vec!["list", "--limit", "0"],
            vec!["list", "--limit", "33"],
            vec!["list", "--from", "20", "--through", "10"],
            vec!["list", "--axis", "occurred", "--unknown-occurrence"],
            vec!["list", "--after", "not-a-cursor"],
            vec!["search", " "],
            vec!["get", "not-an-id"],
            vec!["history", "not-an-id"],
            vec!["references", "not-an-id"],
        ] {
            assert!(prepare(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn input_size_and_identity_are_not_silently_changed() {
        assert!(read_bounded(Cursor::new(vec![b' '; MAX_INPUT_BYTES + 1])).is_err());
        let mut raw = json!({"action":"revise"});
        assert!(add_identity(&mut raw, "action", "append").is_err());
        assert!(add_identity(&mut json!([]), "action", "append").is_err());
        assert!(add_identity(&mut json!({"action":null}), "action", "append").is_err());
    }
}
