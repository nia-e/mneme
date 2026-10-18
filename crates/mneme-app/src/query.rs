//! Shared admission and presentation for optional diagnostic QUERY summary bounds.
//!
//! This is a view budget, never a rewrite of authored summaries or retrieval policy.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Maximum source UTF-8 bytes in each diagnostic query summary. A prefix may
/// contain fewer bytes when the next complete Unicode scalar would not fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct SummaryMaxBytes(u16);

#[derive(Clone, Debug, thiserror::Error)]
#[error("summary_max_bytes must be an integer in 1..=16384 UTF-8 bytes")]
pub struct SummaryMaxBytesError;

impl SummaryMaxBytes {
    pub const FULL: Self = Self(mneme_core::MAX_NODE_SUMMARY_BYTES as u16);
    pub const MCP_DEFAULT: Self = Self(2048);

    pub const fn get(self) -> usize {
        self.0 as usize
    }

    pub fn prefix(self, summary: &str) -> (&str, bool) {
        let mut end = summary.len().min(self.get());
        while !summary.is_char_boundary(end) {
            end -= 1;
        }
        (&summary[..end], end < summary.len())
    }

    pub fn schema() -> Value {
        json!({"type":"integer", "minimum":1, "maximum":Self::FULL.get(),
            "description":"Maximum source UTF-8 bytes per summary (not characters); Unicode-safe prefix with summary_truncated. Default 2048 for MCP; presentation only, no ranking or content changes."})
    }

    /// Refuse older owners that accept but silently ignore unknown fields.
    /// This is feature discovery only, not owner identity or authority.
    pub fn advertised(catalog: &[Value]) -> bool {
        catalog.iter().any(|tool| {
            let schema = &tool["inputSchema"]["properties"]["summary_max_bytes"];
            tool["name"] == "query"
                && schema["type"] == "integer"
                && schema["minimum"].as_u64() == Some(1)
                && schema["maximum"].as_u64() == Some(Self::FULL.get() as u64)
        })
    }
}

impl TryFrom<u64> for SummaryMaxBytes {
    type Error = SummaryMaxBytesError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if (1..=Self::FULL.get() as u64).contains(&value) {
            Ok(Self(value as u16))
        } else {
            Err(SummaryMaxBytesError)
        }
    }
}

impl From<SummaryMaxBytes> for u64 {
    fn from(value: SummaryMaxBytes) -> Self {
        u64::from(value.0)
    }
}

impl std::str::FromStr for SummaryMaxBytes {
    type Err = SummaryMaxBytesError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::try_from(value.parse::<u64>().map_err(|_| SummaryMaxBytesError)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summary_budget_is_strict_and_unicode_safe() {
        for value in [
            json!(0),
            json!(16385),
            json!(u64::MAX),
            json!(-1),
            json!(true),
            json!(1.0),
            json!("4"),
            Value::Null,
        ] {
            assert!(serde_json::from_value::<SummaryMaxBytes>(value).is_err());
        }
        for value in [1, 2, 3, 4, 16384] {
            let bound: SummaryMaxBytes = serde_json::from_value(json!(value)).unwrap();
            assert_eq!(u64::from(bound), value);
            assert_eq!(serde_json::to_value(bound).unwrap(), json!(value));
        }
        assert_eq!(
            "1".parse::<SummaryMaxBytes>().unwrap().prefix("🦋é"),
            ("", true)
        );
        assert_eq!(
            "5".parse::<SummaryMaxBytes>().unwrap().prefix("🦋é"),
            ("🦋", true)
        );
        assert_eq!(
            "6".parse::<SummaryMaxBytes>().unwrap().prefix("🦋é"),
            ("🦋é", false)
        );
        assert_eq!(SummaryMaxBytes::FULL.prefix(""), ("", false));
    }
    #[test]
    fn summary_budget_catalog_refuses_silent_old_owner_fallback() {
        assert!(!SummaryMaxBytes::advertised(&[
            json!({"name":"query","inputSchema":{"properties":{}}})
        ]));
        let schema = json!({"name":"query","inputSchema":{"properties":{"summary_max_bytes":SummaryMaxBytes::schema()}}});
        assert!(SummaryMaxBytes::advertised(&[schema]));
    }
}
