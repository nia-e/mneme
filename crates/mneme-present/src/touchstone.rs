//! Compact, query-local authored-reference views. These are navigation data,
//! never semantic learning receipts or substitutes for the immutable GET record.
use mneme_core::NodeId;
use serde::{Deserialize, Serialize};

use crate::{InputError, SummaryText};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TouchstoneOrigin {
    Direct,
    Referrer { anchor_id: NodeId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchstoneResolution {
    MatchesSnapshot,
    ChangedSnapshot,
    Missing,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TouchstoneReferenceView {
    pub db_id: NodeId,
    pub id: NodeId,
    pub snapshot_sha256: String,
    pub summary: SummaryText,
    pub resolution: TouchstoneResolution,
}

impl TouchstoneReferenceView {
    pub fn new(
        db_id: NodeId,
        id: NodeId,
        digest: String,
        summary: &str,
        prefix_bytes: usize,
        resolution: TouchstoneResolution,
    ) -> Result<Self, InputError> {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(InputError::InvalidTouchstone("invalid snapshot digest"));
        }
        let source_bytes = u32::try_from(summary.len()).map_err(|_| InputError::SummaryTooLarge)?;
        let mut end = summary.len().min(prefix_bytes);
        while !summary.is_char_boundary(end) {
            end -= 1;
        }
        Ok(Self {
            db_id,
            id,
            snapshot_sha256: digest,
            summary: SummaryText {
                text: summary[..end].to_owned(),
                complete: end == summary.len(),
                source_bytes,
            },
            resolution,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TouchstoneView {
    schema: &'static str,
    pub subject: String,
    coverage: &'static str,
    pub references: Vec<TouchstoneReferenceView>,
    pub references_omitted: u32,
    pub origins: Vec<TouchstoneOrigin>,
}

impl TouchstoneView {
    pub fn new(
        subject: String,
        references: Vec<TouchstoneReferenceView>,
        references_omitted: u32,
        mut origins: Vec<TouchstoneOrigin>,
    ) -> Result<Self, InputError> {
        if mneme_core::TouchstoneSubject::new(&subject).is_err() || origins.is_empty() {
            return Err(InputError::InvalidTouchstone(
                "invalid subject or empty origins",
            ));
        }
        origins
            .sort_by_cached_key(|origin| serde_json::to_vec(origin).expect("typed origin encodes"));
        origins.dedup();
        let mut ids = std::collections::BTreeSet::new();
        if references
            .iter()
            .any(|row| !ids.insert((row.db_id, row.id)))
        {
            return Err(InputError::InvalidTouchstone(
                "duplicate captured reference",
            ));
        }
        Ok(Self {
            schema: "mneme.touchstone-view.v1",
            subject,
            coverage: "summary_only",
            references,
            references_omitted,
            origins,
        })
    }

    pub fn partial(&self) -> bool {
        self.references_omitted != 0
            || self.references.iter().any(|row| {
                !row.summary.complete || row.resolution != TouchstoneResolution::MatchesSnapshot
            })
    }

    pub fn navigation_only(&self) -> bool {
        !self.origins.contains(&TouchstoneOrigin::Direct)
    }
}

/// Validate the exact emitted facet once, shared by library projection. Do not
/// normalize, truncate or silently manufacture a facet for a tag-only note.
pub fn validate_touchstone_card_metadata(card: &serde_json::Value) -> Result<(), InputError> {
    let Some(view) = card.get("touchstone") else {
        return Ok(());
    };
    if card.get("kind").is_some_and(|kind| kind != "semantic") {
        return Err(InputError::InvalidTouchstone(
            "touchstone owner is not semantic",
        ));
    }
    let invalid = || InputError::InvalidTouchstone("invalid compact touchstone view");
    let subject = view["subject"].as_str().ok_or_else(invalid)?;
    if subject.trim().is_empty()
        || subject.len() > 256
        || view["schema"] != "mneme.touchstone-view.v1"
        || view["coverage"] != "summary_only"
        || view["references_omitted"]
            .as_u64()
            .is_none_or(|n| n > u32::MAX.into())
    {
        return Err(invalid());
    }
    let origins: Vec<TouchstoneOrigin> =
        serde_json::from_value(view["origins"].clone()).map_err(|_| invalid())?;
    let rows = view["references"].as_array().ok_or_else(invalid)?;
    let mut references = Vec::new();
    for row in rows {
        let db_id = serde_json::from_value(row["db_id"].clone()).map_err(|_| invalid())?;
        let id = serde_json::from_value(row["id"].clone()).map_err(|_| invalid())?;
        let digest = row["snapshot_sha256"]
            .as_str()
            .ok_or_else(invalid)?
            .to_owned();
        let summary = row["summary"]["text"].as_str().ok_or_else(invalid)?;
        let source_bytes = row["summary"]["source_bytes"]
            .as_u64()
            .filter(|n| *n <= u32::MAX.into())
            .ok_or_else(invalid)? as u32;
        let complete = row["summary"]["complete"].as_bool().ok_or_else(invalid)?;
        if source_bytes < summary.len() as u32 || (complete && source_bytes != summary.len() as u32)
        {
            return Err(invalid());
        }
        let resolution =
            serde_json::from_value(row["resolution"].clone()).map_err(|_| invalid())?;
        let mut projected =
            TouchstoneReferenceView::new(db_id, id, digest, summary, summary.len(), resolution)?;
        projected.summary.source_bytes = source_bytes;
        projected.summary.complete = complete;
        references.push(projected);
    }
    let checked = TouchstoneView::new(
        subject.to_owned(),
        references,
        view["references_omitted"].as_u64().unwrap() as u32,
        origins,
    )?;
    if serde_json::to_value(checked).map_err(|_| invalid())? != *view {
        return Err(invalid());
    }
    Ok(())
}

/// Counts are admitted native-operation attempts, including failed/timed-out
/// calls, not fabricated physical endpoint or completed-row measurements.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TouchstoneRetrieval {
    pub searched: bool,
    pub read_limit: u32,
    pub catalog_reads: u32,
    pub record_reads: u32,
    pub referrer_page_reads: u32,
    pub target_reads: u32,
    pub anchors_total: u32,
    pub anchors_examined: u32,
    pub owners_discovered: u32,
    pub further_tail_unknown: bool,
    pub stop_reason: Option<TouchstoneStopReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchstoneStopReason {
    Budget,
    Deadline,
    Unsupported,
    ReadError,
}

impl TouchstoneRetrieval {
    pub fn reads(&self) -> u32 {
        self.catalog_reads + self.record_reads + self.referrer_page_reads + self.target_reads
    }
    pub fn partial(&self) -> bool {
        self.further_tail_unknown || self.stop_reason.is_some()
    }
    pub fn validate(&self) -> Result<(), InputError> {
        if u64::from(self.catalog_reads)
            + u64::from(self.record_reads)
            + u64::from(self.referrer_page_reads)
            + u64::from(self.target_reads)
            > u64::from(self.read_limit)
            || self.anchors_examined > self.anchors_total
            || (!self.searched && *self != Self::default())
        {
            return Err(InputError::InvalidTouchstone(
                "invalid touchstone read coverage",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BodyBudget, Lane, LaneBudgets, LaneLimit, LaneWindow, PackingInput, PresentationBudget,
        PrimaryInputCard, pack,
    };
    use std::num::{NonZeroU16, NonZeroU32};
    fn id(n: u128) -> NodeId {
        NodeId(ulid::Ulid::from(n))
    }
    fn view(resolution: TouchstoneResolution) -> TouchstoneView {
        let row = TouchstoneReferenceView::new(
            id(99),
            id(1),
            "a".repeat(64),
            "Exact historical summary",
            128,
            resolution,
        )
        .unwrap();
        TouchstoneView::new(
            "Example Agent".into(),
            vec![row],
            0,
            vec![TouchstoneOrigin::Referrer { anchor_id: id(1) }],
        )
        .unwrap()
    }
    fn budget(lane_bytes: u32) -> PresentationBudget {
        let disabled = LaneLimit::new(0, 0, 0, 0).unwrap();
        PresentationBudget::new(
            NonZeroU32::new(8192).unwrap(),
            NonZeroU32::new(PresentationBudget::minimum_control_reserve_bytes()).unwrap(),
            NonZeroU16::new(2).unwrap(),
            NonZeroU16::new(256).unwrap(),
            BodyBudget::disabled(),
            LaneBudgets::new(
                disabled,
                LaneLimit::new(0, 1, 2, lane_bytes).unwrap(),
                disabled,
            ),
        )
        .unwrap()
    }
    fn input(view: TouchstoneView) -> PackingInput {
        PackingInput::new(
            crate::minimum_retrieval_metadata(),
            LaneWindow::complete(Vec::new()),
            LaneWindow::complete(vec![
                PrimaryInputCard::new(
                    id(10),
                    NonZeroU16::new(1).unwrap(),
                    "The author's own meaning",
                )
                .unwrap()
                .with_touchstone(view),
            ]),
            LaneWindow::complete(Vec::new()),
        )
        .unwrap()
        .with_touchstone_retrieval(TouchstoneRetrieval {
            searched: true,
            ..Default::default()
        })
        .unwrap()
    }
    #[test]
    fn compact_facet_is_atomic_and_emitted_identity_binds_health_origins_and_subject() {
        let original = view(TouchstoneResolution::MatchesSnapshot);
        let first = pack(&budget(8192), &input(original.clone())).unwrap();
        let json: serde_json::Value = serde_json::from_str(first.rendered_content()).unwrap();
        assert_eq!(json["schema"], "mneme.context.v7");
        assert_eq!(
            json["primary"][0]["touchstone"],
            serde_json::to_value(&original).unwrap()
        );
        validate_touchstone_card_metadata(&json["primary"][0]).unwrap();
        for change in 0..3 {
            let mut modified = original.clone();
            match change {
                0 => modified.references[0].resolution = TouchstoneResolution::Missing,
                1 => modified.subject = "Owner".into(),
                _ => modified.origins = vec![TouchstoneOrigin::Direct],
            };
            let changed = pack(&budget(8192), &input(modified)).unwrap();
            assert_ne!(
                first.manifest().cards()[0].card_sha256(),
                changed.manifest().cards()[0].card_sha256()
            );
        }
        let omitted = pack(&budget(1), &input(original)).unwrap();
        assert!(omitted.envelope().primary().is_empty());
        assert_eq!(
            omitted
                .envelope()
                .omitted()
                .get(Lane::Primary)
                .bounded_window_budget(),
            1
        );
    }
    #[test]
    fn canonical_projection_refuses_metadata_loss_and_unsearched_owner() {
        let mut card = serde_json::json!({"id":id(10),"touchstone":view(TouchstoneResolution::MatchesSnapshot)});
        validate_touchstone_card_metadata(&card).unwrap();
        card["touchstone"]["references"][0]["summary"]["source_bytes"] = serde_json::json!(true);
        assert!(validate_touchstone_card_metadata(&card).is_err());
        let input = PackingInput::new(
            crate::minimum_retrieval_metadata(),
            LaneWindow::complete(Vec::new()),
            LaneWindow::complete(vec![
                PrimaryInputCard::new(id(10), NonZeroU16::new(1).unwrap(), "meaning")
                    .unwrap()
                    .with_touchstone(view(TouchstoneResolution::MatchesSnapshot)),
            ]),
            LaneWindow::complete(Vec::new()),
        )
        .unwrap();
        assert!(pack(&budget(8192), &input).is_err());
    }
}
