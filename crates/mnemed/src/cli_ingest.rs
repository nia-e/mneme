//! Raw CLI ingestion owns its complete input before store admission. The
//! prepared request cannot contain both copied bytes and a borrowed body URI.
//! Local binary bodies remain supported; the MCP adapter requires UTF-8 text.

use std::io::Read;

use clap::Args;
use mneme_core::{
    BodyRef, BoundedTagSet, Confidence, NodeId, NodeSummary, OriginCommit, Provenance, Stability,
};
use mneme_engine::{Ingest, Memory};
use serde_json::{Value, json};

use crate::AnyErr;

const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_REMOTE_SUMMARY_BYTES: usize = 2048;
const MAX_REMOTE_TAGS: usize = 32;

#[derive(Args)]
pub(crate) struct IngestArgs {
    #[arg(long)]
    summary: String,
    /// Body text, at most 256 KiB. If omitted, the summary is used as the body.
    #[arg(long, conflicts_with_all = ["body_file", "body_ref"])]
    body: Option<String>,
    /// Read at most 256 KiB from a file (use `-` for stdin), before store admission.
    #[arg(long = "body-file", conflicts_with = "body_ref")]
    body_file: Option<String>,
    /// Reference an existing body URI in place (e.g. `fs:///abs/path.md`,
    /// `https://…`) instead of storing bytes — nothing is copied.
    #[arg(long = "body-ref")]
    body_ref: Option<String>,
    #[arg(long = "tag")]
    tags: Vec<String>,
    #[arg(long, default_value_t = 0.5)]
    stability: f32,
    #[arg(long, default_value_t = 0.5)]
    confidence: f32,
}

#[derive(Debug)]
enum PreparedBody {
    Bytes(Vec<u8>),
    Reference(BodyRef),
}

/// Owned, bounded input with canonical node invariants already checked. Only
/// this module can construct it; dispatch never receives raw CLI arguments.
#[derive(Debug)]
pub(crate) struct PreparedIngest {
    summary: NodeSummary,
    body: PreparedBody,
    tags: BoundedTagSet,
    stability: Stability,
    confidence: Confidence,
    origin_commit: Option<OriginCommit>,
}

fn read_bounded_body(reader: impl Read, limit: usize, source: &str) -> Result<Vec<u8>, AnyErr> {
    let mut body = Vec::with_capacity(limit.min(8 * 1024));
    reader.take((limit + 1) as u64).read_to_end(&mut body)?;
    if body.len() > limit {
        return Err(format!("{source} exceeds the {limit}-byte body limit").into());
    }
    Ok(body)
}

fn checked_inline_body(body: &str) -> Result<Vec<u8>, AnyErr> {
    if body.len() > MAX_BODY_BYTES {
        return Err(format!("inline body exceeds the {MAX_BODY_BYTES}-byte body limit").into());
    }
    Ok(body.as_bytes().to_vec())
}

impl PreparedIngest {
    pub(crate) fn read(args: &IngestArgs) -> Result<Self, AnyErr> {
        // Validate cheap scalar fields before file/stdin I/O as well as before
        // any persistent path, lease, backend or model can be admitted.
        let summary = NodeSummary::new(&args.summary).map_err(|error| error.to_string())?;
        let tags = BoundedTagSet::try_from_iter(&args.tags).map_err(|error| error.to_string())?;
        let stability = Stability::new(args.stability).map_err(|error| error.to_string())?;
        let confidence = Confidence::new(args.confidence).map_err(|error| error.to_string())?;
        let body = match (&args.body, &args.body_file, &args.body_ref) {
            (None, None, Some(reference)) => {
                PreparedBody::Reference(BodyRef::new(reference).map_err(|error| error.to_string())?)
            }
            (None, Some(path), None) => PreparedBody::Bytes(if path == "-" {
                read_bounded_body(std::io::stdin().lock(), MAX_BODY_BYTES, "stdin body")?
            } else {
                read_bounded_body(std::fs::File::open(path)?, MAX_BODY_BYTES, "body file")?
            }),
            (body, None, None) => PreparedBody::Bytes(checked_inline_body(
                body.as_deref().unwrap_or(summary.as_str()),
            )?),
            _ => return Err("ingest body sources are mutually exclusive".into()),
        };
        Ok(Self {
            summary,
            body,
            tags,
            stability,
            confidence,
            origin_commit: None,
        })
    }

    pub(crate) fn with_origin_commit(mut self, commit: Option<&str>) -> Result<Self, AnyErr> {
        self.origin_commit = commit
            .map(OriginCommit::parse)
            .transpose()
            .map_err(|error| error.to_string())?;
        Ok(self)
    }

    pub(crate) async fn run(self, memory: &Memory) -> Result<NodeId, AnyErr> {
        let (body, body_ref) = match self.body {
            PreparedBody::Bytes(bytes) => (bytes, None),
            PreparedBody::Reference(reference) => (Vec::new(), Some(reference)),
        };
        let tags: Vec<_> = self.tags.iter().collect();
        Ok(memory
            .ingest(Ingest {
                summary: self.summary.as_str(),
                body: &body,
                body_ref,
                tags: &tags,
                provenance: Provenance::derived_empty(),
                stability: self.stability.get(),
                confidence: self.confidence.get(),
                origin_commit: self.origin_commit,
            })
            .await?)
    }
}

/// MCP's existing raw-ingest envelope has deliberately narrower summary/tag
/// bounds than local ingestion. Never reinterpret a local reference remotely.
pub(crate) fn prepare_remote(args: &IngestArgs) -> Result<Value, AnyErr> {
    if args.body_ref.is_some() {
        return Err("remote ingest --body-ref is unavailable: a local body URI must not be resolved by the remote server; send inline body bytes instead; omit --remote for the local CLI operation".into());
    }
    if args.summary.trim().is_empty() || args.summary.len() > MAX_REMOTE_SUMMARY_BYTES {
        return Err(format!("summary must be 1..={MAX_REMOTE_SUMMARY_BYTES} UTF-8 bytes").into());
    }
    if args.tags.len() > MAX_REMOTE_TAGS {
        return Err(format!("remote accepts at most {MAX_REMOTE_TAGS} tags").into());
    }
    let prepared = PreparedIngest::read(args)?;
    let PreparedBody::Bytes(bytes) = prepared.body else {
        unreachable!("remote references rejected before preparation");
    };
    let body = String::from_utf8(bytes).map_err(|_| "remote ingest body must be UTF-8 text")?;
    Ok(json!({
        "summary": prepared.summary,
        "body": body,
        "tags": prepared.tags,
        "stability": prepared.stability,
        "confidence": prepared.confidence,
    }))
}

pub(crate) fn render(id: NodeId, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "id": id.0.to_string() }))
                .expect("serialize ingest receipt")
        );
    } else {
        println!("{}", id.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::io::Cursor;

    fn args(extra: &[&str]) -> IngestArgs {
        let mut argv = vec!["mnemed", "ingest", "--summary", "summary"];
        argv.extend_from_slice(extra);
        let crate::Command::Ingest(args) = crate::Cli::try_parse_from(argv).unwrap().command else {
            panic!("expected ingest command");
        };
        args
    }

    struct EndlessReader {
        bytes_read: usize,
    }
    impl Read for EndlessReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            buf.fill(b'x');
            self.bytes_read += buf.len();
            Ok(buf.len())
        }
    }

    #[test]
    fn bounded_body_accepts_exact_limit_and_rejects_one_more() {
        let exact = vec![b'x'; 32];
        assert_eq!(
            read_bounded_body(Cursor::new(&exact), 32, "test body").unwrap(),
            exact
        );
        let error = read_bounded_body(Cursor::new(vec![b'x'; 33]), 32, "test body")
            .unwrap_err()
            .to_string();
        assert!(error.contains("32-byte body limit"));
        let mut reader = EndlessReader { bytes_read: 0 };
        assert!(read_bounded_body(&mut reader, 32, "test body").is_err());
        assert_eq!(reader.bytes_read, 33);
        assert!(checked_inline_body(&"x".repeat(MAX_BODY_BYTES)).is_ok());
        assert!(checked_inline_body(&"x".repeat(MAX_BODY_BYTES + 1)).is_err());
    }

    #[test]
    fn body_reference_is_bounded_and_never_copied() {
        let exact = format!(
            "x://{}",
            "a".repeat(mneme_core::MAX_BODY_REF_BYTES - "x://".len())
        );
        let prepared = PreparedIngest::read(&args(&["--body-ref", &exact])).unwrap();
        assert!(matches!(prepared.body, PreparedBody::Reference(_)));
        assert!(PreparedIngest::read(&args(&["--body-ref", &format!("{exact}a")])).is_err());
        assert!(PreparedIngest::read(&args(&["--body-ref", "missing-scheme"])).is_err());
    }

    #[test]
    fn ingest_body_sources_are_mutually_exclusive() {
        for sources in [
            vec!["--body", "text", "--body-file", "body.txt"],
            vec!["--body", "text", "--body-ref", "fs:///tmp/body.txt"],
            vec![
                "--body-file",
                "body.txt",
                "--body-ref",
                "fs:///tmp/body.txt",
            ],
        ] {
            let mut argv = vec!["mnemed", "ingest", "--summary", "summary"];
            argv.extend(sources);
            assert!(crate::Cli::try_parse_from(argv).is_err());
        }
        // The prepared boundary is independently safe, not just a clap invariant.
        let mut raw = args(&["--body", "text"]);
        raw.body_ref = Some("fs:///tmp/body.txt".into());
        assert!(PreparedIngest::read(&raw).is_err());
    }

    #[test]
    fn canonical_fields_are_checked_before_body_io() {
        for extra in [
            vec!["--tag", "duplicate", "--tag", "duplicate"],
            vec!["--tag", " untrimmed"],
            vec!["--stability", "NaN"],
            vec!["--confidence", "inf"],
        ] {
            let mut raw = args(&extra);
            raw.body_file = Some("missing-file-should-not-be-read".into());
            let error = PreparedIngest::read(&raw).unwrap_err().to_string();
            assert!(!error.contains("No such file"), "{error}");
        }
        let mut raw = args(&[]);
        raw.summary = " ".into();
        assert!(PreparedIngest::read(&raw).is_err());
        raw.summary = "x".repeat(mneme_core::MAX_NODE_SUMMARY_BYTES);
        assert!(PreparedIngest::read(&raw).is_ok());
        raw.summary.push('x');
        assert!(PreparedIngest::read(&raw).is_err());
        raw.summary = "valid".into();
        raw.tags = (0..mneme_core::MAX_NODE_TAGS)
            .map(|n| format!("tag-{n}"))
            .collect();
        assert!(PreparedIngest::read(&raw).is_ok());
        raw.tags.push("one-too-many".into());
        assert!(PreparedIngest::read(&raw).is_err());
    }

    #[test]
    fn remote_ingest_keeps_its_narrower_bounds_and_envelope() {
        let prepared = prepare_remote(&args(&["--tag", "tag"])).unwrap();
        assert_eq!(
            prepared,
            json!({
                "summary": "summary", "body": "summary", "tags": ["tag"],
                "stability": 0.5, "confidence": 0.5,
            })
        );
        let mut raw = args(&[]);
        raw.summary = "x".repeat(MAX_REMOTE_SUMMARY_BYTES);
        assert!(prepare_remote(&raw).is_ok());
        raw.summary.push('x');
        assert!(PreparedIngest::read(&raw).is_ok());
        assert!(prepare_remote(&raw).is_err());
        raw.summary = "valid".into();
        raw.tags = (0..MAX_REMOTE_TAGS).map(|n| format!("tag-{n}")).collect();
        assert!(prepare_remote(&raw).is_ok());
        raw.tags.push("one-too-many".into());
        assert!(PreparedIngest::read(&raw).is_ok());
        assert!(prepare_remote(&raw).is_err());
        assert!(prepare_remote(&args(&["--body-ref", "https://example.invalid/body"])).is_err());
    }
}
