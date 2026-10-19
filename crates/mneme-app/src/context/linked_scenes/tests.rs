use std::{
    collections::{BTreeMap, VecDeque},
    num::NonZeroU16,
    time::Duration,
};

use mneme_core::NodeId;
use mneme_core::episode::{EpisodeHeader, EpisodeHeaderByEdition, EpisodeHeaderByEditionRequest};
use mneme_core::ports::{Error, IncidentEdgesCursor, IncidentEdgesPage, IncidentEdgesRequest};
use mneme_engine::Memory;
use mneme_present::{EpisodeInputCard, EpisodeReferenceStopReason, LaneWindow};

use super::{CachedEndpoint, LinkedSceneReader, compose_linked_scenes};
use crate::context::discovery::LINKED_READ_TIMEOUT;
use crate::context::test_support::{episode_write, fixture};

struct ScriptedLinkedReader {
    pages: std::sync::Mutex<BTreeMap<NodeId, VecDeque<Result<Option<IncidentEdgesPage>, Error>>>>,
    endpoints: BTreeMap<NodeId, CachedEndpoint>,
    raw_calls: std::sync::Mutex<Vec<(NodeId, bool, Duration)>>,
    header_calls: std::sync::Mutex<Vec<(NodeId, Duration)>>,
    failed_raw_rows: std::sync::Mutex<usize>,
    delay: Duration,
}
impl ScriptedLinkedReader {
    fn new() -> Self {
        Self {
            pages: Default::default(),
            endpoints: BTreeMap::new(),
            raw_calls: Default::default(),
            header_calls: Default::default(),
            failed_raw_rows: Default::default(),
            delay: Duration::ZERO,
        }
    }
}
impl LinkedSceneReader for ScriptedLinkedReader {
    async fn incident_page(
        &self,
        request: &IncidentEdgesRequest,
    ) -> Result<Option<IncidentEdgesPage>, Error> {
        assert_eq!(request.scan_rows(), 1);
        self.raw_calls.lock().unwrap().push((
            request.anchor(),
            request.after().is_some(),
            request.remaining(),
        ));
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay.min(request.remaining())).await;
        }
        let result = self
            .pages
            .lock()
            .unwrap()
            .get_mut(&request.anchor())
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| {
                Ok(Some(IncidentEdgesPage {
                    items: Vec::new(),
                    next: None,
                    work: mneme_core::ports::IncidentEdgeReadWork {
                        indexed_seeks: 2,
                        ..Default::default()
                    },
                }))
            });
        if result.is_err() {
            // Fault injection consumes its admitted raw row, then loses the
            // work report. Only the test's physical-work observer can see it.
            *self.failed_raw_rows.lock().unwrap() += 1;
        }
        result
    }
    async fn exact_header(
        &self,
        request: &EpisodeHeaderByEditionRequest,
    ) -> Result<EpisodeHeaderByEdition, Error> {
        self.header_calls
            .lock()
            .unwrap()
            .push((request.edition_id(), request.remaining()));
        match self.endpoints.get(&request.edition_id()) {
            Some(CachedEndpoint::Episode(header)) => {
                Ok(EpisodeHeaderByEdition::Episode(header.clone()))
            }
            Some(CachedEndpoint::Semantic) => Ok(EpisodeHeaderByEdition::Semantic),
            Some(CachedEndpoint::Failed) => Err(Error::Backend("scripted endpoint failure".into())),
            _ => Ok(EpisodeHeaderByEdition::Missing),
        }
    }
}
fn scripted_edge_page(edge: mneme_core::Edge, anchor: NodeId, has_more: bool) -> IncidentEdgesPage {
    use mneme_core::ports::{IncidentEdgeLeg, MaintenanceEdgeKey};
    let outgoing = edge.from == anchor;
    let key = MaintenanceEdgeKey::from_edge(&edge);
    let next = has_more.then(|| {
        IncidentEdgesCursor::resume(
            anchor,
            outgoing.then_some(key),
            (!outgoing).then_some(key),
            false,
            false,
            if outgoing {
                IncidentEdgeLeg::Incoming
            } else {
                IncidentEdgeLeg::Outgoing
            },
        )
        .unwrap()
    });
    IncidentEdgesPage {
        items: vec![edge],
        next,
        work: mneme_core::ports::IncidentEdgeReadWork {
            rows_scanned: 1,
            indexed_seeks: 1,
            edge_point_reads: 1,
            body_anchor_point_reads: 1,
        },
    }
}
fn test_edge(from: NodeId, to: NodeId) -> mneme_core::Edge {
    mneme_core::Edge::new(from, to, 0.5, mneme_core::EdgeKind::Associative, 0)
}
fn test_id(value: u128) -> NodeId {
    NodeId(ulid::Ulid::from(value))
}
async fn test_header(mem: &Memory, edition: NodeId, head: NodeId) -> EpisodeHeader {
    EpisodeHeader::from_node(&mem.get_node(edition).await.unwrap().unwrap(), head).unwrap()
}

#[tokio::test]
async fn linked_scheduler_charges_raw_work_caches_all_outcomes_and_services_rare_incoming_anchor() {
    let mem = fixture();
    let scene = mem
        .append_episode(episode_write("rare", "A rare scene"))
        .await
        .unwrap();
    let header = test_header(&mem, scene.identity.edition_id, scene.identity.edition_id).await;
    let hub = test_id(1);
    let rare = test_id(2);
    let missing = test_id(3);
    let non_episode = test_id(4);
    let mut reader = ScriptedLinkedReader::new();
    reader
        .endpoints
        .insert(scene.identity.edition_id, CachedEndpoint::Episode(header));
    reader
        .endpoints
        .insert(non_episode, CachedEndpoint::Semantic);
    reader.pages.lock().unwrap().insert(
        hub,
        [
            Ok(Some(scripted_edge_page(test_edge(hub, missing), hub, true))),
            Ok(Some(scripted_edge_page(
                test_edge(hub, non_episode),
                hub,
                true,
            ))),
            Ok(Some(scripted_edge_page(
                test_edge(hub, scene.identity.edition_id),
                hub,
                true,
            ))),
            Ok(Some(scripted_edge_page(test_edge(hub, missing), hub, true))),
        ]
        .into(),
    );
    reader.pages.lock().unwrap().insert(
        rare,
        [Ok(Some(scripted_edge_page(
            test_edge(scene.identity.edition_id, rare),
            rare,
            false,
        )))]
        .into(),
    );
    let (coverage, scenes) = compose_linked_scenes(
        &reader,
        &[hub, rare],
        LaneWindow::complete(Vec::new()),
        3,
        LINKED_READ_TIMEOUT,
    )
    .await
    .unwrap();
    let counts = coverage.counts();
    assert_eq!(counts.raw_edges_scanned, 5);
    assert_eq!(counts.endpoint_reads, 3);
    assert_eq!(counts.missing, 1);
    assert_eq!(counts.non_episode, 1);
    assert_eq!(counts.cache_hits, 2);
    assert_eq!(counts.edge_point_reads, 5);
    assert_eq!(counts.body_anchor_point_reads, 5);
    assert_eq!(counts.indexed_seeks, 7); // Five admitted rows plus one empty dual-index seek.
    assert!(!coverage.partial());
    let calls = reader.raw_calls.lock().unwrap();
    assert_eq!(calls[0].0, hub);
    assert_eq!(calls[1].0, rare, "the hub starved a later incoming anchor");
    assert!(calls.iter().skip(2).all(|call| call.1));
    assert_eq!(scenes.cards().len(), 1);
    assert_eq!(scenes.cards()[0].origins().len(), 2);
    assert_eq!(
        reader
            .header_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == scene.identity.edition_id)
            .count(),
        1
    );
}

#[tokio::test]
async fn linked_budget_fairly_charges_non_scene_endpoints_and_unexamined_tail() {
    let mut reader = ScriptedLinkedReader::new();
    let anchors = [test_id(1), test_id(2), test_id(3)];
    for (index, anchor) in anchors.iter().enumerate() {
        let endpoint = test_id(10 + index as u128);
        reader.endpoints.insert(endpoint, CachedEndpoint::Semantic);
        reader.pages.lock().unwrap().insert(
            *anchor,
            [Ok(Some(scripted_edge_page(
                test_edge(*anchor, endpoint),
                *anchor,
                true,
            )))]
            .into(),
        );
    }
    let (coverage, scenes) = compose_linked_scenes(
        &reader,
        &anchors,
        LaneWindow::complete(Vec::new()),
        1,
        LINKED_READ_TIMEOUT,
    )
    .await
    .unwrap();
    assert!(scenes.cards().is_empty());
    assert_eq!(coverage.counts().raw_edges_scanned, 2);
    assert_eq!(coverage.counts().endpoint_reads, 1);
    assert_eq!(coverage.counts().non_episode, 1);
    assert_eq!(coverage.counts().unread_anchors, 1);
    assert!(coverage.partial());
    assert_eq!(
        coverage.stop_reason(),
        Some(EpisodeReferenceStopReason::Budget)
    );
    assert_eq!(
        reader
            .raw_calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.0)
            .collect::<Vec<_>>(),
        anchors[..2]
    );
}

#[tokio::test]
async fn linked_failures_and_shared_deadline_keep_lexical_results() {
    let mem = fixture();
    let scene = mem
        .append_episode(episode_write("preserved", "violet preserved"))
        .await
        .unwrap();
    let header = test_header(&mem, scene.identity.edition_id, scene.identity.edition_id).await;
    let lexical = LaneWindow::complete(vec![
        EpisodeInputCard::new(header, NonZeroU16::new(1).unwrap()).unwrap(),
    ]);
    let anchor = test_id(1);
    for response in [
        Ok(None),
        Err(Error::Backend("reference read failed".into())),
    ] {
        let reader = ScriptedLinkedReader::new();
        reader
            .pages
            .lock()
            .unwrap()
            .insert(anchor, [response].into());
        let (coverage, cards) =
            compose_linked_scenes(&reader, &[anchor], lexical.clone(), 8, LINKED_READ_TIMEOUT)
                .await
                .unwrap();
        assert!(coverage.partial());
        assert_eq!(cards.cards()[0].identity(), scene.identity);
        assert!(reader.header_calls.lock().unwrap().is_empty());
    }
    let mut reader = ScriptedLinkedReader::new();
    reader.delay = Duration::from_millis(20);
    reader.pages.lock().unwrap().insert(
        anchor,
        [Ok(Some(scripted_edge_page(
            test_edge(anchor, test_id(2)),
            anchor,
            true,
        )))]
        .into(),
    );
    let (coverage, cards) =
        compose_linked_scenes(&reader, &[anchor], lexical, 8, Duration::from_millis(1))
            .await
            .unwrap();
    assert_eq!(
        coverage.stop_reason(),
        Some(EpisodeReferenceStopReason::Deadline)
    );
    assert_eq!(cards.cards().len(), 1);
    assert!(reader.header_calls.lock().unwrap().is_empty());
    assert_eq!(
        reader.raw_calls.lock().unwrap().len(),
        1,
        "deadline was reset for the lexical anchor"
    );
    assert!(reader.raw_calls.lock().unwrap()[0].2 <= Duration::from_millis(1));
}

#[tokio::test]
async fn frozen_semantic_and_lexical_anchors_interleave_and_cached_head_does_not_reroll() {
    let mem = fixture();
    let first = mem
        .append_episode(episode_write(
            "cached-head",
            "violet initial lexical account",
        ))
        .await
        .unwrap();
    let discovered = mem
        .append_episode(episode_write("discovered", "A joke, not a lesson"))
        .await
        .unwrap();
    let header = test_header(&mem, first.identity.edition_id, first.identity.edition_id).await;
    let lexical = LaneWindow::complete(vec![
        EpisodeInputCard::new(header.clone(), NonZeroU16::new(1).unwrap()).unwrap(),
    ]);
    let a = test_id(1);
    let b = test_id(2);
    let mut reader = ScriptedLinkedReader::new();
    let mut later_head = header;
    later_head.current_edition_id = test_id(999);
    reader.endpoints.insert(
        first.identity.edition_id,
        CachedEndpoint::Episode(later_head),
    );
    reader.endpoints.insert(
        discovered.identity.edition_id,
        CachedEndpoint::Episode(
            test_header(
                &mem,
                discovered.identity.edition_id,
                discovered.identity.edition_id,
            )
            .await,
        ),
    );
    for (anchor, edge) in [
        (a, test_edge(a, first.identity.edition_id)),
        (
            first.identity.edition_id,
            test_edge(first.identity.edition_id, discovered.identity.edition_id),
        ),
        (b, test_edge(b, first.identity.edition_id)),
    ] {
        reader.pages.lock().unwrap().insert(
            anchor,
            [Ok(Some(scripted_edge_page(edge, anchor, false)))].into(),
        );
    }
    let (coverage, scenes) =
        compose_linked_scenes(&reader, &[a, b], lexical, 2, LINKED_READ_TIMEOUT)
            .await
            .unwrap();
    assert_eq!(
        reader
            .raw_calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.0)
            .collect::<Vec<_>>(),
        vec![a, first.identity.edition_id, b]
    );
    assert_eq!(coverage.counts().anchors_total, 3);
    assert_eq!(coverage.counts().raw_edges_scanned, 3);
    assert_eq!(coverage.counts().endpoint_reads, 1);
    assert_eq!(coverage.counts().cache_hits, 2);
    assert_eq!(scenes.cards().len(), 2);
    let initial = &scenes.cards()[0];
    assert_eq!(initial.identity(), first.identity);
    assert_eq!(initial.origins().len(), 3); // Lexical plus both semantic references.
    assert_eq!(
        initial.header().current_edition_id,
        first.identity.edition_id
    );
    assert_eq!(
        reader.header_calls.lock().unwrap()[0].0,
        discovered.identity.edition_id
    );
    assert!(!coverage.partial());
}

#[tokio::test]
async fn failed_endpoint_is_cached_without_rehydration_or_loss_of_other_anchors() {
    let mut reader = ScriptedLinkedReader::new();
    let failed = test_id(9);
    reader.endpoints.insert(failed, CachedEndpoint::Failed);
    for anchor in [test_id(1), test_id(2)] {
        reader.pages.lock().unwrap().insert(
            anchor,
            [Ok(Some(scripted_edge_page(
                test_edge(anchor, failed),
                anchor,
                false,
            )))]
            .into(),
        );
    }
    let (coverage, scenes) = compose_linked_scenes(
        &reader,
        &[test_id(1), test_id(2)],
        LaneWindow::complete(Vec::new()),
        2,
        LINKED_READ_TIMEOUT,
    )
    .await
    .unwrap();
    assert!(scenes.cards().is_empty());
    assert_eq!(coverage.counts().anchors_examined, 2);
    assert_eq!(coverage.counts().raw_edges_scanned, 2);
    assert_eq!(coverage.counts().endpoint_reads, 1);
    assert_eq!(coverage.counts().cache_hits, 1);
    assert_eq!(reader.header_calls.lock().unwrap().len(), 1);
    assert_eq!(
        coverage.stop_reason(),
        Some(EpisodeReferenceStopReason::ReadError)
    );
    assert!(coverage.partial());
}

#[tokio::test]
async fn duplicate_lexical_references_do_not_hide_distinct_indirect_presentation_candidate() {
    let mem = fixture();
    let mut lexical = Vec::new();
    let mut editions = Vec::new();
    for index in 0..3 {
        let episode = mem
            .append_episode(episode_write(
                &format!("lexical-{index}"),
                "violet lexical scene",
            ))
            .await
            .unwrap();
        editions.push(episode.identity.edition_id);
        lexical.push(
            EpisodeInputCard::new(
                test_header(
                    &mem,
                    episode.identity.edition_id,
                    episode.identity.edition_id,
                )
                .await,
                NonZeroU16::new(index + 1).unwrap(),
            )
            .unwrap(),
        );
    }
    let indirect = mem
        .append_episode(episode_write("indirect-scene", "An unrelated moth scene"))
        .await
        .unwrap();
    let mut reader = ScriptedLinkedReader::new();
    reader.endpoints.insert(
        indirect.identity.edition_id,
        CachedEndpoint::Episode(
            test_header(
                &mem,
                indirect.identity.edition_id,
                indirect.identity.edition_id,
            )
            .await,
        ),
    );
    editions.push(indirect.identity.edition_id);
    let anchors = (1..=4).map(test_id).collect::<Vec<_>>();
    for (anchor, edition) in anchors.iter().zip(editions) {
        reader.pages.lock().unwrap().insert(
            *anchor,
            [Ok(Some(scripted_edge_page(
                test_edge(*anchor, edition),
                *anchor,
                false,
            )))]
            .into(),
        );
    }
    let (_, scenes) = compose_linked_scenes(
        &reader,
        &anchors,
        LaneWindow::complete(lexical),
        4,
        LINKED_READ_TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!(scenes.cards().len(), 4);
    assert_eq!(
        scenes.cards()[1].identity(),
        indirect.identity,
        "shared identities consumed reference presentation slots"
    );
    assert!(
        scenes
            .cards()
            .iter()
            .filter(|card| card.identity() != indirect.identity)
            .all(|card| card.origins().len() == 2)
    );
}

#[tokio::test]
async fn first_failed_raw_page_stops_linked_work_before_hidden_rows_can_exceed_allowance() {
    let mem = fixture();
    let scene = mem
        .append_episode(episode_write(
            "before-raw-failure",
            "A scene collected before the failing read",
        ))
        .await
        .unwrap();
    let mut reader = ScriptedLinkedReader::new();
    reader.endpoints.insert(
        scene.identity.edition_id,
        CachedEndpoint::Episode(
            test_header(&mem, scene.identity.edition_id, scene.identity.edition_id).await,
        ),
    );
    let anchors = [test_id(1), test_id(2), test_id(3)];
    reader.pages.lock().unwrap().insert(
        anchors[0],
        [Ok(Some(scripted_edge_page(
            test_edge(anchors[0], scene.identity.edition_id),
            anchors[0],
            true,
        )))]
        .into(),
    );
    reader.pages.lock().unwrap().insert(
        anchors[1],
        [Err(Error::Backend(
            "failed after consuming one raw row".into(),
        ))]
        .into(),
    );
    // If the scheduler continued, this next raw row would be real work number
    // three while public successful-row counters still incorrectly allowed it.
    reader.pages.lock().unwrap().insert(
        anchors[2],
        [Ok(Some(scripted_edge_page(
            test_edge(anchors[2], test_id(99)),
            anchors[2],
            false,
        )))]
        .into(),
    );
    let (coverage, scenes) = compose_linked_scenes(
        &reader,
        &anchors,
        LaneWindow::complete(Vec::new()),
        1,
        LINKED_READ_TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!(coverage.counts().raw_edge_limit, 2);
    assert_eq!(
        coverage.counts().raw_edges_scanned,
        1,
        "failed work was invented or reported as completed"
    );
    assert_eq!(*reader.failed_raw_rows.lock().unwrap(), 1);
    assert_eq!(
        coverage.counts().raw_edges_scanned as usize + *reader.failed_raw_rows.lock().unwrap(),
        2
    );
    assert_eq!(coverage.counts().indexed_seeks, 1);
    assert_eq!(coverage.counts().edge_point_reads, 1);
    assert_eq!(coverage.counts().body_anchor_point_reads, 1);
    assert_eq!(
        reader
            .raw_calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.0)
            .collect::<Vec<_>>(),
        anchors[..2],
        "a follow-on raw read escaped the hidden failed-row charge"
    );
    assert_eq!(coverage.counts().unread_anchors, 1);
    assert_eq!(
        coverage.stop_reason(),
        Some(EpisodeReferenceStopReason::ReadError)
    );
    assert!(coverage.partial());
    assert!(coverage.counts().further_tail_unknown);
    assert_eq!(
        scenes.cards().len(),
        1,
        "a raw error discarded previously collected scenes"
    );
    assert_eq!(scenes.cards()[0].identity(), scene.identity);
}
