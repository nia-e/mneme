use super::*;
use mneme_core::{
    BodyRef, Node, NodeStatus, NodeSummary, Provenance, TouchstoneHeader, TouchstoneInput,
    TouchstoneReference, TouchstoneSubject,
};
use std::sync::Mutex;

fn id(n: u128) -> NodeId {
    NodeId(ulid::Ulid::from(n))
}
fn snapshot(n: u128, summary: &str, body: &str) -> SummarySnapshot {
    let node = Node::try_new(
        id(n),
        summary,
        BodyRef::new(body).unwrap(),
        Vec::<&str>::new(),
        Provenance::derived_empty(),
        0.5,
        0.5,
        NodeStatus::Active,
        10,
    )
    .unwrap();
    SummarySnapshot::from_node(ulid::Ulid::from(99_u128), &node).unwrap()
}
fn record(owner: u128, snapshots: Vec<SummarySnapshot>) -> TouchstoneRecord {
    let input = TouchstoneInput::new(
        TouchstoneSubject::new("Example Agent").unwrap(),
        snapshots
            .iter()
            .map(|s| TouchstoneReference::new(s.db_id(), s.id(), s.digest()))
            .collect(),
    )
    .unwrap();
    TouchstoneRecord::new(id(owner), &input, snapshots).unwrap()
}
fn header(record: &TouchstoneRecord) -> TouchstoneHeader {
    TouchstoneHeader {
        id: record.owner(),
        summary: NodeSummary::new("A deliberately authored annotation").unwrap(),
        subject: record.subject().clone(),
        reference_count: record.references().len(),
        status: NodeStatus::Active,
    }
}
struct Reader {
    records: BTreeMap<NodeId, TouchstoneRecord>,
    current: BTreeMap<NodeId, SummarySnapshot>,
    calls: Mutex<Vec<(&'static str, NodeId)>>,
    slow: bool,
}
impl TouchstoneReader for Reader {
    async fn catalog(&self) -> Result<TouchstonePage, Error> {
        self.calls.lock().unwrap().push(("catalog", id(0)));
        if self.slow {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        Ok(TouchstonePage {
            items: self
                .records
                .values()
                .next()
                .map(header)
                .into_iter()
                .collect(),
            next: None,
        })
    }
    async fn referrers(
        &self,
        target: NodeId,
        after: Option<TouchstoneCursor>,
    ) -> Result<TouchstonePage, Error> {
        self.calls.lock().unwrap().push(("referrers", target));
        let items = self
            .records
            .values()
            .filter(|r| {
                r.references().iter().any(|s| s.id() == target)
                    && after.as_ref().is_none_or(|c| r.owner() > c.after())
            })
            .map(header)
            .collect::<Vec<_>>();
        let next = (items.len() > 1)
            .then(|| TouchstoneCursor::referrers(ulid::Ulid::from(99_u128), target, items[0].id));
        Ok(TouchstonePage {
            items: items.into_iter().take(1).collect(),
            next,
        })
    }
    async fn record(&self, owner: NodeId) -> Result<Option<TouchstoneRecord>, Error> {
        self.calls.lock().unwrap().push(("record", owner));
        Ok(self.records.get(&owner).cloned())
    }
    async fn snapshot(&self, target: NodeId) -> Result<Option<SummarySnapshot>, Error> {
        self.calls.lock().unwrap().push(("snapshot", target));
        Ok(self.current.get(&target).cloned())
    }
}
fn reader(records: Vec<TouchstoneRecord>, current: Vec<SummarySnapshot>) -> Reader {
    Reader {
        records: records.into_iter().map(|r| (r.owner(), r)).collect(),
        current: current.into_iter().map(|s| (s.id(), s)).collect(),
        calls: Mutex::new(Vec::new()),
        slow: false,
    }
}
#[tokio::test]
async fn forward_material_and_reverse_annotations_do_not_recurse() {
    let scene = snapshot(1, "The exact old account", "inline://old");
    let unrelated = snapshot(2, "This scene is not an original anchor", "inline://other");
    let a = record(10, vec![scene.clone(), unrelated]);
    let b = record(
        20,
        vec![snapshot(
            10,
            "A deliberately authored annotation",
            "inline://owner",
        )],
    );
    let r = reader(vec![a.clone(), b], vec![scene.clone()]);
    let reverse = compose_touchstones(
        &r,
        &[id(1)],
        &[],
        16,
        Instant::now() + Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(reverse.incoming.len(), 1);
    assert!(reverse.views[&id(10)].navigation_only());
    assert_eq!(reverse.views[&id(10)].references.len(), 1);
    assert_eq!(reverse.views[&id(10)].references[0].id, id(1));
    assert_eq!(reverse.views[&id(10)].references_omitted, 1);
    assert!(
        !r.calls
            .lock()
            .unwrap()
            .iter()
            .any(|(kind, target)| *kind == "referrers" && *target == id(10))
    );
    let forward = compose_touchstones(
        &r,
        &[id(10)],
        &[id(10)],
        16,
        Instant::now() + Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(forward.views[&id(10)].references.len(), 2);
    assert_eq!(
        forward.views[&id(10)].references[0].summary.text(),
        scene.summary().as_str()
    );
}
#[tokio::test]
async fn missing_changed_and_body_only_edits_keep_historical_material() {
    let old = snapshot(1, "Old summary", "inline://old");
    let same_body_changed = snapshot(2, "Same summary", "inline://oldbody");
    let absent = snapshot(3, "Now missing", "inline://missing");
    let r = reader(
        vec![record(
            10,
            vec![old.clone(), same_body_changed.clone(), absent],
        )],
        vec![
            snapshot(1, "Corrected summary", "inline://new"),
            snapshot(2, "Same summary", "inline://newbody"),
        ],
    );
    let result = compose_touchstones(
        &r,
        &[id(10)],
        &[id(10)],
        32,
        Instant::now() + Duration::from_secs(1),
    )
    .await
    .unwrap();
    let refs = &result.views[&id(10)].references;
    assert_eq!(refs[0].summary.text(), "Old summary");
    assert_eq!(refs[0].resolution, TouchstoneResolution::ChangedSnapshot);
    assert_eq!(refs[1].resolution, TouchstoneResolution::MatchesSnapshot);
    assert_eq!(refs[2].resolution, TouchstoneResolution::Missing);
    assert_eq!(refs[1].snapshot_sha256, same_body_changed.digest().to_hex());
}
#[tokio::test]
async fn indexed_degree_is_work_bounded_and_scope_stays_local() {
    let scene = snapshot(1, "scene", "inline://scene");
    let r = reader(
        (10..80)
            .map(|owner| record(owner, vec![scene.clone()]))
            .collect(),
        vec![scene],
    );
    let result = compose_touchstones(
        &r,
        &[id(1)],
        &[],
        4,
        Instant::now() + Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert!(result.coverage.reads() <= 4);
    assert!(r.calls.lock().unwrap().len() <= 4);
    assert!(result.coverage.partial());
    assert_eq!(
        result.coverage.stop_reason,
        Some(TouchstoneStopReason::Budget)
    );
    for view in result.views.values() {
        assert!(view.references.iter().all(|s| s.db_id == id(99)));
    }
}
#[tokio::test]
async fn empty_catalog_is_one_read_and_slow_catalog_is_deadline_not_absence() {
    let r = reader(Vec::new(), Vec::new());
    let empty = compose_touchstones(
        &r,
        &[id(1), id(2)],
        &[id(1)],
        16,
        Instant::now() + Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(empty.coverage.reads(), 1);
    assert!(!empty.coverage.partial());
    let mut r = reader(Vec::new(), Vec::new());
    r.slow = true;
    let start = Instant::now();
    let slow = compose_touchstones(&r, &[id(1)], &[], 16, start + Duration::from_millis(10))
        .await
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(
        slow.coverage.stop_reason,
        Some(TouchstoneStopReason::Deadline)
    );
    assert!(slow.coverage.partial());
    assert_eq!(slow.coverage.catalog_reads, 1);
}

#[tokio::test]
async fn episode_citation_uses_exact_edition_without_loading_or_substituting_head() {
    use mneme_core::episode::{EpisodeFacet, EpisodeTime, OccurrenceSpan};
    use mneme_core::{CaptureRequestCodec, CaptureSource};
    let source = CaptureSource::new_with_codec(
        "touchstone-test",
        "historical-edition",
        "fixture://old-account",
        None,
        None,
        [7; 32],
        CaptureRequestCodec::EpisodeV1,
    )
    .unwrap();
    let edition = source.node_id();
    let node = Node::try_new(
        edition,
        "An original event account",
        BodyRef::new("inline://never-resolve-this-body").unwrap(),
        Vec::<&str>::new(),
        Provenance::External { source },
        0.5,
        0.5,
        NodeStatus::Active,
        10,
    )
    .unwrap()
    .with_episode(
        EpisodeFacet::initial(
            edition,
            OccurrenceSpan::Unknown,
            None,
            EpisodeTime::new(10).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let old = SummarySnapshot::from_node(ulid::Ulid::from(99_u128), &node).unwrap();
    let r = reader(
        vec![record(10, vec![old.clone()])],
        vec![
            old.clone(),
            snapshot(999, "A later corrected account", "inline://correction"),
        ],
    );
    let result = compose_touchstones(
        &r,
        &[edition],
        &[],
        16,
        Instant::now() + Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(result.incoming.len(), 1);
    let reference = &result.views[&id(10)].references[0];
    assert_eq!(reference.id, edition);
    assert_eq!(reference.snapshot_sha256, old.digest().to_hex());
    assert_eq!(reference.summary.text(), "An original event account");
    assert!(
        !r.calls
            .lock()
            .unwrap()
            .iter()
            .any(|(_, target)| *target == id(999))
    );
}
