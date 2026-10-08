//! Canonical bounded SAVE admission. Freeze manual identity before checkout or
//! transport; domain validation and receipt semantics belong to mneme-app.

use crate::AnyErr;
use clap::{Args, ValueEnum};
use mneme_app::save::{self as shared, SaveOrigin};
use serde_json::{Value, json};
use std::io::Read;

const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_BODY_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum Kind {
    Note,
    Episode,
}
impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Episode => "episode",
        }
    }
}

#[derive(Args)]
pub(crate) struct SaveArgs {
    /// A useful note summary. Mutually exclusive with --input.
    #[arg(required_unless_present = "input", conflicts_with = "input")]
    text: Option<String>,
    /// Complete JSON request; `-` reads stdin. Maximum encoded input: 1 MiB.
    /// Notes may include immutable touchstone subject/references; episodes may not.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["text", "kind", "body", "body_file", "tags", "operation_id"])]
    input: Option<String>,
    /// Save a note (default) or an immutable experience episode.
    #[arg(long, value_enum)]
    kind: Option<Kind>,
    /// Full content; defaults to the summary. Kind-specific native bounds apply.
    #[arg(long, conflicts_with = "body_file")]
    body: Option<String>,
    /// UTF-8 content file, read before checkout; at most 256 KiB.
    #[arg(long, value_name = "PATH", conflicts_with = "body")]
    body_file: Option<String>,
    /// Comma-separated tags; native tag bounds and kind restrictions apply.
    #[arg(long, value_delimiter = ',')]
    tags: Vec<String>,
    /// Stable manual submission identity. Retrying requires unchanged content.
    #[arg(long)]
    operation_id: Option<String>,
}

fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>, AnyErr> {
    let mut bytes = Vec::new();
    reader.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(format!("save input exceeds {limit} UTF-8 bytes").into());
    }
    Ok(bytes)
}

pub(crate) struct PreparedSave {
    pub(crate) prepared: shared::PreparedSave,
    pub(crate) payload: Value,
}
impl PreparedSave {
    pub(crate) fn read(args: &SaveArgs) -> Result<Self, AnyErr> {
        let raw = if let Some(path) = &args.input {
            let bytes = if path == "-" {
                read_bounded(std::io::stdin().lock(), MAX_INPUT_BYTES)?
            } else {
                read_bounded(std::fs::File::open(path)?, MAX_INPUT_BYTES)?
            };
            serde_json::from_slice(&bytes)?
        } else {
            let mut raw =
                json!({"summary": args.text.as_ref().ok_or("save requires TEXT or --input PATH")?});
            if let Some(kind) = args.kind {
                raw["kind"] = json!(kind.as_str());
            }
            if let Some(body) = &args.body {
                raw["body"] = json!(body);
            }
            if let Some(path) = &args.body_file {
                raw["body"] = json!(String::from_utf8(read_bounded(
                    std::fs::File::open(path)?,
                    MAX_BODY_BYTES
                )?)?);
            }
            if !args.tags.is_empty() {
                raw["tags"] = json!(args.tags);
            }
            if let Some(id) = &args.operation_id {
                raw["operation_id"] = json!(id);
            }
            raw
        };
        // One nonce, frozen into the canonical source before any submission.
        let nonce = ulid::Ulid::new().to_string();
        let initial = shared::PreparedSave::parse(&raw, &nonce).map_err(|e| -> AnyErr { e })?;
        let payload = initial.into_json();
        let prepared = shared::PreparedSave::parse(&payload, "").map_err(|e| -> AnyErr { e })?;
        if prepared.identity().origin == SaveOrigin::ManualSubmission {
            eprintln!(
                "save operation_id: {} (retain with the unchanged request for exact retry)",
                prepared.identity().key
            );
        }
        Ok(Self { prepared, payload })
    }
}

pub(crate) fn render(receipt: &Value, json_output: bool) {
    if json_output {
        crate::print_json(receipt);
    } else {
        println!("{}", shared::render_human(receipt));
    }
}

/// SAVE never admits reference snapshots or creates absent targets.
pub(crate) fn check_target(
    db: &std::path::Path,
    lease: &mneme_store_path::StoreLease,
) -> Result<(), AnyErr> {
    #[cfg(feature = "cozo")]
    {
        crate::cli_capture::check_add_target(db, lease)
    }
    #[cfg(not(feature = "cozo"))]
    {
        let _ = (db, lease);
        Err("save requires a cozo-enabled build and an existing current database".into())
    }
}
