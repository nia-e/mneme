//! Bounded concern admission over an existing metadata-only native owner.
use crate::{AnyErr, MemStore};
use clap::Args;
use mneme_app::concern::{self as shared, PreparedConcernRequest};
use mneme_core::ports::GraphStore;
use serde_json::{Value, json};
use std::{io::Read, path::Path, sync::Arc};
use ulid::Ulid;

#[derive(Args)]
pub(crate) struct ConcernArgs {
    /// Closed JSON action list|notice|record_finding; `-` reads stdin (64 KiB).
    /// Mutations require caller-supplied expected_db_id. List limit is optional;
    /// the native byte-derived page default applies; continue with its cursor.
    #[arg(long, value_name = "PATH")]
    input: String,
}

pub(crate) struct PreparedConcern {
    pub(crate) request: PreparedConcernRequest,
    pub(crate) expected_db_id: Option<Ulid>,
}
impl PreparedConcern {
    pub(crate) fn read(args: &ConcernArgs) -> Result<Self, AnyErr> {
        let reader: Box<dyn Read> = if args.input == "-" {
            Box::new(std::io::stdin())
        } else {
            Box::new(std::fs::File::open(&args.input)?)
        };
        let mut bytes = Vec::new();
        reader
            .take((shared::MAX_CONCERN_REQUEST_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > shared::MAX_CONCERN_REQUEST_BYTES {
            return Err("concern input exceeds 65536 encoded bytes".into());
        }
        let mut raw: Value = serde_json::from_slice(&bytes)?;
        let object = raw
            .as_object_mut()
            .ok_or("concern input must be an object")?;
        let expected_db_id = object
            .remove("expected_db_id")
            .map(|value| {
                let spelling = value
                    .as_str()
                    .ok_or("expected_db_id must be a canonical ULID")?;
                let id: Ulid = spelling.parse()?;
                if id.to_string() != spelling {
                    return Err("expected_db_id must be a canonical ULID".into());
                }
                Ok::<_, AnyErr>(id)
            })
            .transpose()?;
        let request = PreparedConcernRequest::parse(&raw).map_err(|e| -> AnyErr { e })?;
        if request.is_mutation() && expected_db_id.is_none() {
            return Err("concern mutations require caller-supplied expected_db_id from the intended owner; no automatic rebinding".into());
        }
        Ok(Self {
            request,
            expected_db_id,
        })
    }

    pub(crate) fn payload(&self) -> Value {
        let mut raw = self.request.clone().into_json();
        if let Some(id) = self.expected_db_id {
            raw["expected_db_id"] = json!(id.to_string());
        }
        raw
    }

    async fn execute(
        &self,
        graph: &dyn GraphStore,
        db: &Path,
        db_id: Ulid,
    ) -> Result<Value, AnyErr> {
        if self
            .expected_db_id
            .is_some_and(|expected| expected != db_id)
        {
            return Err(
                "expected_db_id does not match the admitted database; no concern action performed"
                    .into(),
            );
        }
        let store = graph
            .concerns()
            .ok_or("this native owner does not support concern maintenance")?;
        let mut result = self
            .request
            .execute(store)
            .await
            .map_err(|e| -> AnyErr { e })?;
        result["db"] = json!(db.to_string_lossy());
        result["db_id"] = json!(db_id.to_string());
        self.request
            .validate_routed_response_json(&result, &db.to_string_lossy(), self.expected_db_id)
            .map_err(|e| -> AnyErr { e })?;
        Ok(result)
    }
}

/// No generic open: no create, embedding identity initialization, bodies, or
/// inference wiring. Keep the exact target lease through native completion.
pub(crate) async fn run(
    prepared: PreparedConcern,
    db: &Path,
    json_output: bool,
) -> Result<(), AnyErr> {
    let db = crate::cli_capture::canonical_target(db)?;
    let lease = Arc::new(mneme_store_path::StoreLease::acquire(&db)?);
    let metadata = std::fs::symlink_metadata(&db).map_err(|error| {
        format!(
            "concern requires an existing current database {}: {error}",
            db.display()
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("concern requires an existing regular database file, not a symlink".into());
    }
    #[cfg(feature = "cozo")]
    if !crate::is_snapshot(&db)? {
        crate::CozoStore::require_existing_current(&db, lease.as_ref())?;
        let store =
            crate::CozoStore::open_existing_persistent(&db, crate::DEFAULT_DIM, lease.clone())?;
        let result = prepared.execute(&store, &db, store.db_id()).await?;
        render(&result, json_output);
        return Ok(());
    }
    let store = MemStore::load(&db)?;
    let result = prepared.execute(&store, &db, store.db_id()).await?;
    // Reads, refusals and exact replays must not rewrite a reference snapshot.
    if result["outcome"]["status"] == "applied" {
        store.save(&db)?;
    }
    render(&result, json_output);
    drop(lease);
    Ok(())
}

pub(crate) fn render(result: &Value, json_output: bool) {
    if json_output {
        crate::print_json(result);
    } else {
        println!("{}", shared::render_human(result));
    }
}
