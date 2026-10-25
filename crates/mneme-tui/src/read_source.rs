//! Provenance of displayed reads, separate from authored memory provenance and
//! current connection/activity state. A witness describes a read-only snapshot,
//! never permission to write or evidence that other retained reads used it.

use mneme_store_client::ReadSource;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadProvenance {
    pub snapshot: Option<ReadSource>,
    /// The displayed composition also contains a read without this exact witness.
    pub mixed: bool,
}

impl ReadProvenance {
    pub(crate) fn snapshot(source: ReadSource) -> Self {
        Self {
            snapshot: Some(source),
            mixed: false,
        }
    }

    /// Merge actual retained content, not empty UI placeholders or registry
    /// telemetry. Missing provenance is unknown, not an assertion of live data.
    pub(crate) fn merge(&mut self, other: &Self) {
        self.mixed |= other.mixed || self.snapshot != other.snapshot;
        if self.snapshot.is_none() {
            self.snapshot.clone_from(&other.snapshot);
        }
    }
}

impl crate::model::Response {
    pub(crate) fn set_read_source(&mut self, source: ReadProvenance) {
        use crate::model::Response;
        match self {
            Response::Inventory(page) => page.graph.read_source = source,
            Response::Summaries(page) => page.read_source = source,
            Response::Loaded(graph) => graph.read_source = source,
            Response::Search(page) => page.read_source = source,
            Response::Scenes(page) => page.read_source = source,
            Response::Scene(scene) => scene.read_source = source,
            Response::Touchstones(page) => page.read_source = source,
            Response::Touchstone(detail) => detail.read_source = source,
            Response::TouchstoneTarget(target) => target.read_source = source,
            Response::Activity(_) | Response::Canceled | Response::Error(_) => {}
        }
    }
}

#[cfg(test)]
pub(crate) fn example() -> ReadProvenance {
    ReadProvenance::snapshot(ReadSource {
        owner_db_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        source_device_id: "test-device".into(),
        generation: "generation-a".into(),
        captured_at: 1_700_000_000,
        database: "replica-a".into(),
        resolved_path: "/synthetic/replica.mnemed".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composition_preserves_exact_witness_and_marks_mixed_reads() {
        let snapshot = example();
        let mut same = snapshot.clone();
        same.merge(&snapshot);
        assert_eq!(same, snapshot);

        let mut from_unknown = ReadProvenance::default();
        from_unknown.merge(&snapshot);
        assert_eq!(from_unknown.snapshot, snapshot.snapshot);
        assert!(from_unknown.mixed);

        let mut from_snapshot = snapshot.clone();
        from_snapshot.merge(&ReadProvenance::default());
        assert_eq!(from_snapshot, from_unknown);

        let mut changed = snapshot.clone();
        changed.snapshot.as_mut().unwrap().generation = "generation-b".into();
        same.merge(&changed);
        assert!(same.mixed);
        assert_eq!(same.snapshot, snapshot.snapshot);
    }
}
