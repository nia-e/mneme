//! Derived calendar presentation; canonical timestamps remain exact epoch milliseconds.
use chrono::{DateTime, Utc};
use mneme_core::episode::{EpisodeTime, OccurrenceSpan};
use mneme_core::{Node, SummarySnapshot, Timestamp};
use serde::Serialize;

// A shared, portable calendar range, also supported by model-facing Python readers.
const LAST_CALENDAR_MILLISECOND: Timestamp = 253_402_300_799_999;

/// UTC calendar time through whole seconds. Fractions are omitted from display,
/// never rounded back into the canonical timestamp. Unsupported dates keep their
/// exact value instead of wrapping, clamping, or inventing a calendar date.
pub fn format_timestamp_millis(ms: Timestamp) -> String {
    (ms <= LAST_CALENDAR_MILLISECOND)
        .then(|| DateTime::<Utc>::from_timestamp_millis(ms as i64))
        .flatten()
        .map(|time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| format!("out of calendar range ({ms} Unix epoch ms)"))
}

/// Readable occurrence alongside, not inside, the strict canonical occurrence
/// union. In particular, an unknown occurrence must not acquire a date.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OccurrenceUtc {
    Unknown,
    Point { at_utc: String },
    Range { start_utc: String, end_utc: String },
}
impl From<&OccurrenceSpan> for OccurrenceUtc {
    fn from(span: &OccurrenceSpan) -> Self {
        match span {
            OccurrenceSpan::Unknown => Self::Unknown,
            OccurrenceSpan::Point { at } => Self::Point {
                at_utc: format_timestamp_millis(at.get()),
            },
            OccurrenceSpan::Range { start, end } => Self::Range {
                start_utc: format_timestamp_millis(start.get()),
                end_utc: format_timestamp_millis(end.get()),
            },
        }
    }
}

/// Derived dates for the three distinct episodic clocks. These fields belong
/// to a read presentation, not an immutable account, capture proof or snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EpisodeTimePresentation {
    pub occurred_utc: OccurrenceUtc,
    pub recorded_at_utc: String,
    pub edition_recorded_at_utc: String,
}
impl EpisodeTimePresentation {
    pub fn new(occurred: &OccurrenceSpan, recorded: EpisodeTime, edition: EpisodeTime) -> Self {
        Self::from_millis(occurred, recorded.get(), edition.get())
    }

    fn from_millis(occurred: &OccurrenceSpan, recorded: Timestamp, edition: Timestamp) -> Self {
        Self {
            occurred_utc: occurred.into(),
            recorded_at_utc: format_timestamp_millis(recorded),
            edition_recorded_at_utc: format_timestamp_millis(edition),
        }
    }
}

/// Calendar companions for explicit node read surfaces, without changing the
/// numeric telemetry or nested canonical `memory_kind` facet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodeTimePresentation {
    pub created_utc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_exposed_utc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_grounded_use_utc: Option<String>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub episode: Option<EpisodeTimePresentation>,
}
impl NodeTimePresentation {
    pub fn new(node: &Node) -> Self {
        Self {
            created_utc: format_timestamp_millis(node.created()),
            last_exposed_utc: node.last_exposed().map(format_timestamp_millis),
            last_grounded_use_utc: node.last_grounded_use().map(format_timestamp_millis),
            episode: node.episode().map(|facet| {
                EpisodeTimePresentation::from_millis(
                    facet.occurrence(),
                    facet.recorded_at().get(),
                    node.created(),
                )
            }),
        }
    }
}

/// Separate GET display for one immutable historical reference. Never add
/// derived fields to `SummarySnapshot`: its canonical bytes and hash stay raw.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SnapshotTimePresentation {
    db_id: ulid::Ulid,
    id: mneme_core::NodeId,
    created_utc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fetched_utc: Option<String>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    episode: Option<EpisodeTimePresentation>,
}
impl SnapshotTimePresentation {
    pub fn new(snapshot: &SummarySnapshot) -> Self {
        Self {
            db_id: snapshot.db_id(),
            id: snapshot.id(),
            created_utc: format_timestamp_millis(snapshot.created()),
            fetched_utc: match snapshot.provenance() {
                mneme_core::Provenance::Web { fetched, .. } => {
                    Some(format_timestamp_millis(*fetched))
                }
                _ => None,
            },
            episode: match snapshot.memory_kind() {
                mneme_core::episode::MemoryKind::Semantic => None,
                mneme_core::episode::MemoryKind::Episode(facet) => {
                    Some(EpisodeTimePresentation::from_millis(
                        facet.occurrence(),
                        facet.recorded_at().get(),
                        snapshot.created(),
                    ))
                }
            },
        }
    }
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;
    #[test]
    fn utc_is_whole_seconds_and_does_not_round_or_wrap_exact_values() {
        for (ms, expected) in [
            (0, "1970-01-01T00:00:00Z"),
            (60_000, "1970-01-01T00:01:00Z"),
            (61_234, "1970-01-01T00:01:01Z"),
            (61_999, "1970-01-01T00:01:01Z"),
            (1_600_000_000_123, "2020-09-13T12:26:40Z"),
            (951_827_696_789, "2000-02-29T12:34:56Z"),
            (LAST_CALENDAR_MILLISECOND, "9999-12-31T23:59:59Z"),
        ] {
            assert_eq!(format_timestamp_millis(ms), expected);
        }
        for ms in [LAST_CALENDAR_MILLISECOND + 1, u64::MAX.into(), u128::MAX] {
            assert_eq!(
                format_timestamp_millis(ms),
                format!("out of calendar range ({ms} Unix epoch ms)")
            );
        }
    }

    #[test]
    fn occurrence_display_preserves_unknown_and_does_not_collapse_short_ranges() {
        assert_eq!(
            serde_json::to_value(OccurrenceUtc::from(&OccurrenceSpan::Unknown)).unwrap(),
            serde_json::json!({"kind":"unknown"})
        );
        let span = OccurrenceSpan::range(
            EpisodeTime::new(1_001).unwrap(),
            EpisodeTime::new(1_999).unwrap(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(OccurrenceUtc::from(&span)).unwrap(),
            serde_json::json!({"kind":"range", "start_utc":"1970-01-01T00:00:01Z", "end_utc":"1970-01-01T00:00:01Z"})
        );
        assert_eq!(
            serde_json::to_value(span).unwrap(),
            serde_json::json!({"kind":"range", "start":1001,"end":1999})
        );
    }
}
