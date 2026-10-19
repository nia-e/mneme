//! Optional, bounded context about legacy contradiction flags. Core cards do not
//! depend on this advisory read; unsupported, failed and timed-out reads stay
//! unavailable rather than becoming an invented zero.
use mneme_core::ports::{
    CONTRADICTION_SUMMARY_TIMEOUT, ContradictionSummary, ContradictionSummaryRequest,
    MAX_CONTRADICTION_SUMMARY_SCAN_ROWS,
};
use mneme_engine::Memory;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Additive initialize feature advertisement for native reads that intentionally
/// skip catalog discovery. It grants no authority beyond the ordinary core read.
pub const CONTRADICTION_SUMMARY_FEATURE: &str = "mneme/core-contradiction-summary-v1";

#[derive(Clone, Copy, Debug, Default)]
pub struct CoreOptions {
    include_contradictions: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("argument `include_contradictions` must be a boolean")]
pub struct CoreOptionsError;

impl CoreOptions {
    /// Parse only the optional field; each frontend retains its own core body
    /// bounds and complete-field admission before selecting the database.
    pub fn parse(arguments: &Value) -> Result<Self, CoreOptionsError> {
        Ok(Self {
            include_contradictions: match arguments.get("include_contradictions") {
                None => false,
                Some(value) => value.as_bool().ok_or(CoreOptionsError)?,
            },
        })
    }

    pub const fn new(include_contradictions: bool) -> Self {
        Self {
            include_contradictions,
        }
    }

    pub const fn include_contradictions(self) -> bool {
        self.include_contradictions
    }

    pub async fn contradiction_summary(self, memory: &Memory) -> Option<ContradictionNotice> {
        if !self.include_contradictions {
            return None;
        }
        let request = ContradictionSummaryRequest::new(
            MAX_CONTRADICTION_SUMMARY_SCAN_ROWS,
            CONTRADICTION_SUMMARY_TIMEOUT,
        )
        .expect("fixed summary envelope is valid");
        Some(summary_with_timeout(memory.contradiction_summary(&request)).await)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContradictionNotice {
    Complete {
        open_found: usize,
        rows_scanned: usize,
    },
    Partial {
        open_found: usize,
        rows_scanned: usize,
    },
    Unavailable,
}

impl ContradictionNotice {
    fn from_summary(summary: ContradictionSummary) -> Self {
        let open_found = summary.open_found();
        let rows_scanned = summary.rows_scanned();
        if summary.complete() {
            Self::Complete {
                open_found,
                rows_scanned,
            }
        } else {
            Self::Partial {
                open_found,
                rows_scanned,
            }
        }
    }

    pub fn from_json(value: &Value) -> Option<Self> {
        let notice: Self = serde_json::from_value(value.clone()).ok()?;
        let (open_found, rows_scanned, complete) = match notice {
            Self::Complete {
                open_found,
                rows_scanned,
            } => (open_found, rows_scanned, true),
            Self::Partial {
                open_found,
                rows_scanned,
            } => (open_found, rows_scanned, false),
            Self::Unavailable => return Some(notice),
        };
        let request = ContradictionSummaryRequest::new(
            MAX_CONTRADICTION_SUMMARY_SCAN_ROWS,
            CONTRADICTION_SUMMARY_TIMEOUT,
        )
        .ok()?;
        ContradictionSummary::new(open_found, rows_scanned, complete)
            .ok()?
            .validate_for(&request)
            .ok()?;
        Some(notice)
    }
}

impl std::fmt::Display for ContradictionNotice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Complete { open_found, .. } => {
                write!(f, "{open_found} open contradictions.")
            }
            Self::Partial { open_found: 0, .. } => f.write_str("Contradiction scan incomplete."),
            Self::Partial { open_found, .. } => {
                write!(f, "At least {open_found} open contradictions.")
            }
            Self::Unavailable => f.write_str("Contradictions unavailable."),
        }
    }
}

async fn summary_with_timeout(
    future: impl std::future::Future<Output = mneme_core::ports::Result<Option<ContradictionSummary>>>,
) -> ContradictionNotice {
    match tokio::time::timeout(CONTRADICTION_SUMMARY_TIMEOUT, future).await {
        Ok(Ok(Some(summary))) => ContradictionNotice::from_summary(summary),
        Ok(Ok(None) | Err(_)) | Err(_) => ContradictionNotice::Unavailable,
    }
}

pub fn advertised(catalog: &[Value]) -> bool {
    catalog.iter().any(|tool| {
        tool["name"] == "core"
            && tool["inputSchema"]["properties"]["include_contradictions"]["type"] == "boolean"
    })
}

pub fn initialized_support(initialize: &Value) -> bool {
    initialize["capabilities"]["experimental"][CONTRADICTION_SUMMARY_FEATURE] == true
}

/// Do not send a newly introduced optional field to owners whose strict parser
/// predates it. False is always omitted. Returns whether the caller requested it.
pub fn prepare_remote_arguments(arguments: &mut Value, catalog: &[Value]) -> bool {
    prepare_remote_arguments_with_support(arguments, advertised(catalog))
}

pub fn prepare_remote_arguments_with_support(arguments: &mut Value, supported: bool) -> bool {
    let requested = arguments["include_contradictions"] == true;
    if !requested || !supported {
        if let Some(object) = arguments.as_object_mut() {
            object.remove("include_contradictions");
        }
    }
    requested
}

/// Optional upstream metadata may fail independently of the core cards. Never
/// replace a successful core response with a parse error from this extra field.
pub fn complete_remote_result(result: &mut Value, requested: bool, max_result_bytes: usize) {
    if requested && result.is_object() {
        let notice = result
            .get("contradiction_summary")
            .and_then(ContradictionNotice::from_json)
            .unwrap_or(ContradictionNotice::Unavailable);
        attach_contradiction_summary(result, notice, max_result_bytes);
    }
}

/// A notice never displaces otherwise valid core cards at an encoded-size
/// boundary. Omission under byte pressure means unavailable to consumers.
/// The writer counts only and stops at the existing caller-owned ceiling.
pub fn attach_contradiction_summary(
    result: &mut Value,
    notice: ContradictionNotice,
    max_result_bytes: usize,
) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    object.insert(
        "contradiction_summary".into(),
        serde_json::to_value(notice).expect("small contradiction notice is serializable"),
    );
    struct Remaining(usize);
    impl std::io::Write for Remaining {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("optional notice exceeds result allowance"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    if serde_json::to_writer(Remaining(max_result_bytes), &result).is_err() {
        result
            .as_object_mut()
            .unwrap()
            .remove("contradiction_summary");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strict_opt_in_and_old_owner_fallback_preserve_default() {
        assert!(
            !CoreOptions::parse(&json!({}))
                .unwrap()
                .include_contradictions()
        );
        for bad in [Value::Null, json!("true"), json!(1), json!({}), json!([])] {
            assert!(CoreOptions::parse(&json!({"include_contradictions":bad})).is_err());
        }
        for field in [None, Some(false), Some(true)] {
            let mut args = json!({"db":"selected","expected_db_id":"exact"});
            if let Some(value) = field {
                args["include_contradictions"] = json!(value);
            }
            let requested = prepare_remote_arguments(&mut args, &[]);
            assert_eq!(requested, field == Some(true));
            assert!(args.get("include_contradictions").is_none());
            assert_eq!(args["db"], "selected");
            assert_eq!(args["expected_db_id"], "exact");
            let mut result = json!({"nodes":[{"id":"card"}]});
            complete_remote_result(&mut result, requested, 4096);
            assert_eq!(result.get("contradiction_summary").is_some(), requested);
            assert_eq!(result["nodes"][0]["id"], "card");
        }
    }

    #[test]
    fn malformed_optional_result_is_unavailable_without_losing_cards() {
        for notice in [
            json!({"status":"complete","open_found":2,"rows_scanned":1}),
            json!({"status":"complete","open_found":0,"rows_scanned":257}),
            json!({"status":"complete","open_found":0,"rows_scanned":256}),
            json!({"status":"partial","open_found":0,"rows_scanned":0}),
            json!({"status":"unavailable","open_found":0}),
            Value::Null,
        ] {
            let mut result = json!({"nodes":[{"id":"card"}],"contradiction_summary":notice});
            complete_remote_result(&mut result, true, 4096);
            assert_eq!(
                result["contradiction_summary"],
                json!({"status":"unavailable"})
            );
            assert_eq!(result["nodes"][0]["id"], "card");
        }
    }

    #[test]
    fn optional_summary_never_pushes_cards_past_encoded_result_allowance() {
        let base = json!({"nodes":[{"body":"\"\\\n".repeat(100)}]});
        let limit = serde_json::to_vec(&base).unwrap().len();
        for (requested, room) in [(false, 0), (true, 0), (true, 10), (true, 200)] {
            let mut result = base.clone();
            complete_remote_result(&mut result, requested, limit + room);
            assert_eq!(result["nodes"], base["nodes"]);
            assert!(serde_json::to_vec(&result).unwrap().len() <= limit + room);
            assert_eq!(
                result.get("contradiction_summary").is_some(),
                requested && room == 200
            );
        }
    }

    #[tokio::test]
    async fn optional_failure_unsupported_and_timeout_are_unavailable() {
        assert_eq!(
            summary_with_timeout(async { Ok(None) }).await,
            ContradictionNotice::Unavailable
        );
        assert_eq!(
            summary_with_timeout(async {
                Err(mneme_core::ports::Error::Backend(
                    "optional read failed".into(),
                ))
            })
            .await,
            ContradictionNotice::Unavailable
        );
        assert_eq!(
            summary_with_timeout(std::future::pending()).await,
            ContradictionNotice::Unavailable
        );
    }
}
