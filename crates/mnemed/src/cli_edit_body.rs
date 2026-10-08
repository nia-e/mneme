//! Thin CLI encoding of the shared guarded body-only edit.
use crate::AnyErr;
use clap::Args;
use mneme_app::edit_body::{self as shared, PreparedBodyEdit};
use serde_json::{Value, json};
#[derive(Args)]
pub(crate) struct EditBodyArgs {
    id: String,
    /// Opaque revision from get; stale revisions require inspection and a new intent.
    #[arg(long)]
    expected_body_revision: String,
    /// UTF-8 replacement body file; '-' reads bounded stdin. Empty is valid.
    #[arg(long)]
    body_file: std::path::PathBuf,
    /// Optional explicit intended owner identity; mismatch refuses without mutation.
    #[arg(long)]
    pub(crate) expected_db_id: Option<String>,
}
pub(crate) struct Prepared {
    pub(crate) request: PreparedBodyEdit,
    pub(crate) expected_db_id: Option<ulid::Ulid>,
}
impl Prepared {
    pub(crate) fn read(args: &EditBodyArgs) -> Result<Self, AnyErr> {
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
        use std::io::Read;
        let reader: Box<dyn Read> = if args.body_file.as_os_str() == "-" {
            Box::new(std::io::stdin())
        } else {
            Box::new(std::fs::File::open(&args.body_file)?)
        };
        let mut bytes = Vec::new();
        reader
            .take((mneme_engine::MAX_CAPTURE_BODY_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > mneme_engine::MAX_CAPTURE_BODY_BYTES {
            return Err("edit-body body file exceeds its byte allowance".into());
        }
        let body = String::from_utf8(bytes)?;
        Ok(Self {
            request: PreparedBodyEdit::parse(
                &json!({"id":args.id,"expected_body_revision":args.expected_body_revision,"body":body}),
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
    fn file_admission_and_remote_payload_are_shared() {
        let path = std::env::temp_dir().join(format!("mneme-edit-body-{}", ulid::Ulid::new()));
        std::fs::write(&path, "replacement").unwrap();
        let id = ulid::Ulid::new().to_string();
        let revision = "a".repeat(64);
        let cli = crate::Cli::try_parse_from([
            "mnemed",
            "edit-body",
            &id,
            "--expected-body-revision",
            &revision,
            "--body-file",
            path.to_str().unwrap(),
        ])
        .unwrap();
        let request = crate::remote_commands::prepare(&cli.command, "project").unwrap();
        assert_eq!(request.tool, "edit_body");
        assert_eq!(
            request.access,
            crate::remote_commands::RequestAccess::Mutation
        );
        assert_eq!(request.arguments["body"], "replacement");
        assert_eq!(request.arguments["db"], "project");
        assert!(request.edit_body.is_some());
        std::fs::write(&path, [255]).unwrap();
        assert!(crate::remote_commands::prepare(&cli.command, "project").is_err());
        std::fs::write(&path, vec![b'x'; mneme_engine::MAX_CAPTURE_BODY_BYTES + 1]).unwrap();
        assert!(crate::remote_commands::prepare(&cli.command, "project").is_err());
        std::fs::remove_file(path).unwrap();
    }
}
