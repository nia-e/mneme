//! A cheap advisory read of the legacy contradiction overlay, not triage.

use crate::ports::{Error, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One read admits 64 KiB of logical canonical records. A valid record has two
/// 26-byte IDs, a u32 count, two storage-range integer timestamps and a nullable
/// fixed resolution enum (at most 17 bytes): below 256 bytes even as a JSON row.
/// This is a work envelope, not a limit on the number of stored contradictions.
pub const MAX_CONTRADICTION_SUMMARY_SCAN_ROWS: usize = (64 * 1024) / 256;
/// The complete optional read, including waiting for adapter admission/locks.
pub const CONTRADICTION_SUMMARY_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContradictionSummaryRequest {
    scan_rows: usize,
    remaining: Duration,
}

impl ContradictionSummaryRequest {
    pub fn new(scan_rows: usize, remaining: Duration) -> Result<Self> {
        let request = Self {
            scan_rows,
            remaining,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<()> {
        if self.scan_rows == 0 || self.scan_rows > MAX_CONTRADICTION_SUMMARY_SCAN_ROWS {
            return Err(Error::InvalidInput(format!(
                "contradiction summary scan rows must be 1..={MAX_CONTRADICTION_SUMMARY_SCAN_ROWS}"
            )));
        }
        if self.remaining.is_zero() || self.remaining > CONTRADICTION_SUMMARY_TIMEOUT {
            return Err(Error::InvalidInput(
                "contradiction summary deadline must be positive and at most 100 ms".into(),
            ));
        }
        Ok(())
    }

    pub const fn scan_rows(&self) -> usize {
        self.scan_rows
    }
    pub const fn remaining(&self) -> Duration {
        self.remaining
    }
}

/// Counts from one bounded physical prefix, filtered only after reading it.
/// `open_found` is exact only when `complete`; otherwise it is a lower bound,
/// including zero. A full admitted prefix conservatively remains incomplete.
/// Errors/unsupported adapters are represented outside this type, never as zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SummaryFields")]
pub struct ContradictionSummary {
    open_found: usize,
    rows_scanned: usize,
    complete: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryFields {
    open_found: usize,
    rows_scanned: usize,
    complete: bool,
}

impl TryFrom<SummaryFields> for ContradictionSummary {
    type Error = Error;
    fn try_from(value: SummaryFields) -> Result<Self> {
        Self::new(value.open_found, value.rows_scanned, value.complete)
    }
}

impl ContradictionSummary {
    pub fn new(open_found: usize, rows_scanned: usize, complete: bool) -> Result<Self> {
        if open_found > rows_scanned
            || rows_scanned > MAX_CONTRADICTION_SUMMARY_SCAN_ROWS
            || (!complete && rows_scanned == 0)
            || (complete && rows_scanned == MAX_CONTRADICTION_SUMMARY_SCAN_ROWS)
        {
            return Err(Error::InvalidInput(
                "invalid contradiction summary counts".into(),
            ));
        }
        Ok(Self {
            open_found,
            rows_scanned,
            complete,
        })
    }

    pub fn validate_for(&self, request: &ContradictionSummaryRequest) -> Result<()> {
        request.validate()?;
        if self.rows_scanned > request.scan_rows
            || self.complete != (self.rows_scanned < request.scan_rows)
        {
            return Err(Error::Backend(
                "contradiction summary exceeds its admitted prefix".into(),
            ));
        }
        Ok(())
    }

    pub const fn open_found(&self) -> usize {
        self.open_found
    }
    pub const fn rows_scanned(&self) -> usize {
        self.rows_scanned
    }
    pub const fn complete(&self) -> bool {
        self.complete
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contradiction_summary_request_bounds_physical_work_and_time() {
        for rows in [0, MAX_CONTRADICTION_SUMMARY_SCAN_ROWS + 1, usize::MAX] {
            assert!(ContradictionSummaryRequest::new(rows, CONTRADICTION_SUMMARY_TIMEOUT).is_err());
        }
        for remaining in [
            Duration::ZERO,
            CONTRADICTION_SUMMARY_TIMEOUT + Duration::from_nanos(1),
        ] {
            assert!(ContradictionSummaryRequest::new(1, remaining).is_err());
        }
        for remaining in [Duration::from_nanos(1), CONTRADICTION_SUMMARY_TIMEOUT] {
            let request =
                ContradictionSummaryRequest::new(MAX_CONTRADICTION_SUMMARY_SCAN_ROWS, remaining)
                    .unwrap();
            assert_eq!(request.scan_rows(), MAX_CONTRADICTION_SUMMARY_SCAN_ROWS);
            assert_eq!(request.remaining(), remaining);
        }
    }

    #[test]
    fn contradiction_summary_deserialization_preserves_checked_counts() {
        for raw in [
            serde_json::json!({"open_found":2,"rows_scanned":1,"complete":true}),
            serde_json::json!({"open_found":0,"rows_scanned":MAX_CONTRADICTION_SUMMARY_SCAN_ROWS+1,"complete":false}),
            serde_json::json!({"open_found":0,"rows_scanned":0,"complete":false}),
            serde_json::json!({"open_found":0,"rows_scanned":MAX_CONTRADICTION_SUMMARY_SCAN_ROWS,"complete":true}),
            serde_json::json!({"open_found":0,"rows_scanned":0,"complete":true,"total":0}),
        ] {
            assert!(serde_json::from_value::<ContradictionSummary>(raw).is_err());
        }
        let request = ContradictionSummaryRequest::new(2, CONTRADICTION_SUMMARY_TIMEOUT).unwrap();
        for summary in [
            ContradictionSummary::new(0, 0, true).unwrap(),
            ContradictionSummary::new(0, 2, false).unwrap(),
        ] {
            summary.validate_for(&request).unwrap();
            assert_eq!(
                serde_json::from_value::<ContradictionSummary>(
                    serde_json::to_value(summary).unwrap()
                )
                .unwrap(),
                summary
            );
        }
        for summary in [
            ContradictionSummary::new(1, 1, false).unwrap(),
            ContradictionSummary::new(2, 2, true).unwrap(),
            ContradictionSummary::new(2, 3, true).unwrap(),
        ] {
            assert!(summary.validate_for(&request).is_err());
        }
    }
}
