//! Thin CLI encoding of the shared guarded summary-only edit.
use crate::AnyErr;
use clap::Args;
use mneme_app::edit_summary::{self as shared, PreparedSummaryEdit};
use serde_json::{Value, json};
#[derive(Args)]
pub(crate) struct EditSummaryArgs {
    id: String,
    /// Opaque expected_snapshot_sha256 from get summary_snapshot; stale revisions require inspection and a new intent.
    #[arg(long)]
    expected_snapshot_sha256: String,
    /// Nonblank UTF-8 replacement summary, at most 16 KiB.
    #[arg(long)]
    summary: String,
    /// Optional explicit intended owner identity; mismatch refuses without mutation.
    #[arg(long)]
    pub(crate) expected_db_id: Option<String>,
}
pub(crate) struct Prepared {
    pub(crate) request: PreparedSummaryEdit,
    pub(crate) expected_db_id: Option<ulid::Ulid>,
}
impl Prepared {
    pub(crate) fn read(args: &EditSummaryArgs) -> Result<Self, AnyErr> {
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
        Ok(Self {
            request: PreparedSummaryEdit::parse(
                &json!({"id":args.id,"expected_snapshot_sha256":args.expected_snapshot_sha256,"summary":args.summary}),
            )
            .map_err(|error| -> AnyErr { error })?,
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
    use clap::Parser;
    #[test]
    fn summary_admission_and_remote_payload_are_shared() {
        let id = ulid::Ulid::new().to_string();
        let guard = "a".repeat(64);
        let cli = crate::Cli::try_parse_from([
            "mnemed",
            "edit-summary",
            &id,
            "--expected-snapshot-sha256",
            &guard,
            "--summary",
            "replacement",
        ])
        .unwrap();
        let request = crate::remote_commands::prepare(&cli.command, "project").unwrap();
        assert_eq!(request.tool, "edit_summary");
        assert_eq!(
            request.access,
            crate::remote_commands::RequestAccess::Mutation
        );
        assert_eq!(request.arguments["summary"], "replacement");
        assert_eq!(request.arguments["expected_snapshot_sha256"], guard);
        assert_eq!(request.arguments["db"], "project");
        assert!(request.edit_summary.is_some());
    }
}
