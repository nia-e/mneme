//! Bounded, typed capture input. Complete this preflight before resolving or
//! opening the CLI database: malformed automation packets do not create stores.

use std::io::Read;
use std::path::Path;

use clap::{Args, Subcommand};
use mneme_app::capture as shared;
use mneme_engine::Memory;
use serde_json::{Value, json};

use crate::{AnyErr, git_head_cwd, print_json};

#[cfg(feature = "cozo")]
use mneme_cozo::{CozoStore, MemStore};
#[cfg(feature = "cozo")]
use mneme_embed::DEFAULT_DIM;
#[cfg(feature = "cozo")]
use ulid::Ulid;

const MAX_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Args)]
pub(crate) struct CaptureArgs {
    /// Structured JSON request file; use `-` for stdin. Read and validated
    /// before database checkout. At most 1 MiB of JSON is accepted.
    #[arg(long, value_name = "PATH", required = true)]
    pub(crate) input: String,
}

#[derive(Subcommand)]
pub(crate) enum CaptureAction {
    /// Initialize an explicitly selected, absent database as a fresh single
    /// graph. Existing databases are never modified or converted.
    Init,
    /// Verify an explicit existing current store and its identity without inference or creation.
    Inspect,
    /// Add or exactly replay one sourced claim; namespace/key is its stable
    /// logical identity, not a content hash. JSON has `source` (namespace, key,
    /// reference, optional session/revision), summary, and optional body,
    /// tags, stability, confidence, links, and note-only touchstone (subject and
    /// references containing local db_id, id, expected_snapshot_sha256 from GET). New memories are searchable immediately.
    /// links atomically connect this claim to up to 8 existing nodes in the
    /// same database; no similarity links are inferred.
    Add(CaptureArgs),
}

/// Publication and the capture-generation classifier require the exact
/// absolute target spelling. Canonicalize only its existing parent so the
/// final component remains visible to their no-symlink/no-clobber checks.
pub(crate) fn canonical_target(db: &Path) -> Result<std::path::PathBuf, AnyErr> {
    let parent = db
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(format!("database target needs an existing parent directory {}; create that directory explicitly, then retry", parent.display()).into());
    }
    let name = db.file_name().ok_or("database target needs a file name")?;
    Ok(parent.canonicalize()?.join(name))
}

#[cfg(feature = "cozo")]
pub(crate) async fn init(db: &Path, json_output: bool) -> Result<(), AnyErr> {
    let parent = db
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(format!("capture init needs an existing parent directory {}; create that directory explicitly, then retry", parent.display()).into());
    }
    let operation_id = Ulid::new();
    CozoStore::materialize_fresh_current(db, operation_id, &MemStore::new(DEFAULT_DIM)).await?;
    if json_output {
        print_json(
            &json!({ "status": "initialized", "db": db, "operation_id": operation_id.to_string() }),
        );
    } else {
        println!(
            "initialized capture database {} (operation {operation_id})",
            db.display()
        );
    }
    Ok(())
}

#[cfg(not(feature = "cozo"))]
pub(crate) async fn init(_db: &Path, _json_output: bool) -> Result<(), AnyErr> {
    Err("capture init requires a cozo-enabled build".into())
}

/// Existing-only setup admission. This never assembles the embedding/runtime policy.
#[cfg(feature = "cozo")]
pub(crate) fn inspect(db: &Path, json_output: bool) -> Result<(), AnyErr> {
    let lease = std::sync::Arc::new(mneme_store_path::StoreLease::acquire(db)?);
    CozoStore::require_existing_current(db, lease.as_ref())?;
    let store = CozoStore::open_existing_persistent(db, DEFAULT_DIM, lease)?;
    let identity = store.db_id();
    if json_output {
        print_json(&json!({"status":"verified", "db":db, "db_id":identity.to_string()}));
    } else {
        println!(
            "verified existing capture database {} ({identity})",
            db.display()
        );
    }
    Ok(())
}
#[cfg(not(feature = "cozo"))]
pub(crate) fn inspect(_db: &Path, _json_output: bool) -> Result<(), AnyErr> {
    Err("capture inspect requires a cozo-enabled build".into())
}

#[cfg(feature = "cozo")]
pub(crate) fn check_add_target(
    db: &Path,
    lease: &mneme_store_path::StoreLease,
) -> Result<(), AnyErr> {
    CozoStore::require_existing_current(db, lease)?;
    Ok(())
}

#[cfg(not(feature = "cozo"))]
pub(crate) fn check_add_target(
    _db: &Path,
    _lease: &mneme_store_path::StoreLease,
) -> Result<(), AnyErr> {
    Err("capture add requires a cozo-enabled build".into())
}

pub(crate) struct PreparedCapture(shared::PreparedCapture);

fn read_bounded(mut reader: impl Read) -> Result<Vec<u8>, AnyErr> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(format!("capture input exceeds {MAX_INPUT_BYTES} UTF-8 bytes").into());
    }
    Ok(bytes)
}

impl PreparedCapture {
    fn parse_bytes(bytes: &[u8]) -> Result<Self, AnyErr> {
        let raw: Value = serde_json::from_slice(bytes)?;
        Ok(Self(
            shared::PreparedCapture::parse(&raw).map_err(|error| -> AnyErr { error })?,
        ))
    }

    pub(crate) fn has_links(&self) -> bool {
        self.0.has_links()
    }
    pub(crate) fn into_json(self) -> Value {
        self.0.into_json()
    }

    pub(crate) fn read(args: &CaptureArgs) -> Result<Self, AnyErr> {
        let bytes = if args.input == "-" {
            read_bounded(std::io::stdin().lock())?
        } else {
            read_bounded(std::fs::File::open(&args.input)?)?
        };
        Self::parse_bytes(&bytes)
    }

    pub(crate) async fn run(
        self,
        mem: &Memory,
        json_output: bool,
        user: bool,
    ) -> Result<(), AnyErr> {
        let commit = (!user).then(git_head_cwd).flatten();
        let outcome = self
            .0
            .run(mem, commit.as_deref())
            .await
            .map_err(|error| -> AnyErr { error })?;
        if json_output {
            print_json(&json!({ "id": outcome.id.0.to_string(), "replayed": outcome.replayed }));
        } else {
            println!(
                "{}{}",
                outcome.id.0,
                if outcome.replayed { " (replayed)" } else { "" }
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const GOOD: &str = r#"{"source":{"namespace":"codex","key":"claim-1","reference":"codex://thread/1"},"summary":"A recorded claim"}"#;

    #[test]
    fn bounded_reader_accepts_exact_limit_and_rejects_sentinel() {
        assert_eq!(
            read_bounded(Cursor::new(vec![b'x'; MAX_INPUT_BYTES]))
                .unwrap()
                .len(),
            MAX_INPUT_BYTES
        );
        assert!(read_bounded(Cursor::new(vec![b'x'; MAX_INPUT_BYTES + 1])).is_err());
    }

    #[test]
    fn structured_capture_is_preflighted_before_store() {
        assert!(PreparedCapture::parse_bytes(GOOD.as_bytes()).is_ok());
        for invalid in [
            r#"{"source":null,"summary":"x"}"#,
            r#"{"source":{"namespace":"codex","key":"k","reference":"r","session":null},"summary":"x"}"#,
            r#"{"source":{"namespace":"bad namespace","key":"k","reference":"r"},"summary":"x"}"#,
        ] {
            assert!(
                PreparedCapture::parse_bytes(invalid.as_bytes()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn forwarding_preserves_optional_omission() {
        let plain = PreparedCapture::parse_bytes(GOOD.as_bytes()).unwrap();
        assert!(!plain.has_links());
        assert!(plain.into_json().get("links").is_none());
    }
}
