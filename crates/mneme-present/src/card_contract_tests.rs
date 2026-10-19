//! Exact pre-refactor bytes: field ordering and flattening are part of the
//! emitted manifest digest, even where JSON value equality cannot see them.
use super::*;

fn id(value: u128) -> NodeId {
    NodeId(ulid::Ulid::from(value))
}

fn rank(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value).unwrap()
}

fn budget() -> PresentationBudget {
    let lane = LaneLimit::new(0, 1, 4, 8192).unwrap();
    PresentationBudget::new(
        NonZeroU32::new(32768).unwrap(),
        NonZeroU32::new(PresentationBudget::minimum_control_reserve_bytes()).unwrap(),
        NonZeroU16::new(8).unwrap(),
        NonZeroU16::new(64).unwrap(),
        BodyBudget::disabled(),
        LaneBudgets::new(lane, lane, lane).with_episodic(lane),
    )
    .unwrap()
}

fn semantic() -> PrimaryInputCard {
    PrimaryInputCard::new(id(1), 1234, rank(2), "A \"semantic\" note 🦋\n").unwrap()
}

fn touchstone() -> CoreInputCard {
    let reference = TouchstoneReferenceView::new(
        id(90),
        id(91),
        "ab".repeat(32),
        "Remember 🦋 precisely",
        12,
        TouchstoneResolution::ChangedSnapshot,
    )
    .unwrap();
    let view = TouchstoneView::new(
        "Author's meaning".into(),
        vec![reference],
        1,
        vec![
            TouchstoneOrigin::Referrer { anchor_id: id(3) },
            TouchstoneOrigin::Direct,
        ],
    )
    .unwrap();
    CoreInputCard::new(id(2), 9999, rank(1), "Deliberately chosen")
        .unwrap()
        .with_touchstone(view)
}

fn episode() -> EpisodeInputCard {
    use mneme_core::episode::{EpisodeId, EpisodeRevision};
    let occurred = OccurrenceSpan::range(
        EpisodeTime::new(1000).unwrap(),
        EpisodeTime::new(2345).unwrap(),
    )
    .unwrap();
    let header = EpisodeHeader {
        identity: EpisodeIdentity {
            episode_id: EpisodeId::new(id(7)),
            edition_id: id(8),
            revision: EpisodeRevision::new(1),
        },
        summary: NodeSummary::new("An exact account\n🦋").unwrap(),
        occurred,
        recorded_at: EpisodeTime::new(3000).unwrap(),
        edition_recorded_at: EpisodeTime::new(2000).unwrap(),
        thread: Some(EpisodeThread::new("a \"thread\"").unwrap()),
        occurrence_contexts: Some(
            OccurrenceContexts::new(vec![
                OccurrenceContextRef::new("conversation", "scene", Some("then")).unwrap(),
            ])
            .unwrap(),
        ),
        recording_session: Some(EpisodeRecordingSession::new("session-1").unwrap()),
        current_edition_id: id(8),
    };
    let mut card = EpisodeInputCard::new(header.clone(), rank(3)).unwrap();
    let mut edge = Edge::new(id(3), id(8), 0.5, EdgeKind::DerivedFrom, 42);
    edge.anchor = Some(BodySpan::new(2, 11));
    let reference = EpisodeInputCard::referenced(
        header,
        rank(3),
        EpisodeAnchor::Semantic { node_id: id(3) },
        &edge,
    )
    .unwrap();
    card.merge_origins(&reference).unwrap();
    card
}

fn fixture_input(case: &str) -> PackingInput {
    let has_semantic = case == "semantic" || case == "mixed";
    let has_touchstone = case == "touchstone" || case == "mixed";
    let has_episode = case == "episode" || case == "mixed";
    let mut input = PackingInput::new(
        minimum_retrieval_metadata(),
        LaneWindow::complete(if has_touchstone {
            vec![touchstone()]
        } else {
            vec![]
        }),
        LaneWindow::bounded(
            if has_semantic {
                vec![semantic()]
            } else {
                vec![]
            },
            has_semantic,
        ),
        LaneWindow::complete(if case == "mixed" {
            vec![ExpansionInputCard::new(id(3), 0, rank(4), "Expansion").unwrap()]
        } else {
            vec![]
        }),
    )
    .unwrap();
    if has_touchstone {
        input = input
            .with_touchstone_retrieval(TouchstoneRetrieval {
                searched: true,
                read_limit: 8,
                record_reads: 1,
                target_reads: 1,
                anchors_total: 1,
                anchors_examined: 1,
                owners_discovered: 1,
                ..Default::default()
            })
            .unwrap();
    }
    if has_episode {
        input = input
            .with_episodic(
                EpisodicRetrieval::searched(true, false),
                LaneWindow::complete(vec![episode()]),
            )
            .unwrap()
            .with_episode_reference_retrieval(
                EpisodeReferenceRetrieval::searched(
                    EpisodeReferenceCounts {
                        anchors_total: 1,
                        anchors_examined: 1,
                        raw_edges_scanned: 1,
                        raw_edge_limit: 8,
                        endpoint_reads: 1,
                        endpoint_read_limit: 8,
                        indexed_seeks: 2,
                        edge_point_reads: 1,
                        body_anchor_point_reads: 1,
                        ..Default::default()
                    },
                    None,
                )
                .unwrap(),
            )
            .unwrap();
    }
    input
}

#[test]
fn exact_card_wire_and_manifest_contracts() {
    for (case, expected_content, expected_manifest) in [
        (
            "semantic",
            include_str!("../tests/fixtures/semantic.context.json"),
            include_str!("../tests/fixtures/semantic.manifest.json"),
        ),
        (
            "episode",
            include_str!("../tests/fixtures/episode.context.json"),
            include_str!("../tests/fixtures/episode.manifest.json"),
        ),
        (
            "touchstone",
            include_str!("../tests/fixtures/touchstone.context.json"),
            include_str!("../tests/fixtures/touchstone.manifest.json"),
        ),
        (
            "mixed",
            include_str!("../tests/fixtures/mixed.context.json"),
            include_str!("../tests/fixtures/mixed.manifest.json"),
        ),
    ] {
        let plan = pack(&budget(), &fixture_input(case)).unwrap();
        assert_eq!(
            plan.rendered_content(),
            expected_content,
            "{case} content bytes"
        );
        assert_eq!(
            serde_json::to_string(plan.manifest()).unwrap(),
            expected_manifest,
            "{case} manifest bytes"
        );
    }
}

#[test]
fn mutable_touchstone_views_are_revalidated_even_when_the_card_would_be_omitted() {
    let valid = TouchstoneView::new(
        "Authored meaning".into(),
        vec![],
        0,
        vec![TouchstoneOrigin::Direct],
    )
    .unwrap();
    let mut invalid = valid;
    invalid.origins.clear();
    let input = PackingInput::from_complete(
        minimum_retrieval_metadata(),
        vec![],
        vec![semantic().with_touchstone(invalid)],
        vec![],
    )
    .unwrap()
    .with_touchstone_retrieval(TouchstoneRetrieval {
        searched: true,
        ..Default::default()
    })
    .unwrap();
    let disabled = LaneLimit::new(0, 0, 0, 0).unwrap();
    let omit_all = PresentationBudget::new(
        NonZeroU32::new(32768).unwrap(),
        NonZeroU32::new(PresentationBudget::minimum_control_reserve_bytes()).unwrap(),
        NonZeroU16::new(8).unwrap(),
        NonZeroU16::new(64).unwrap(),
        BodyBudget::disabled(),
        LaneBudgets::new(disabled, disabled, disabled),
    )
    .unwrap();
    for budget in [budget(), omit_all] {
        assert!(matches!(
            pack(&budget, &input),
            Err(PackError::InvalidInput(InputError::InvalidTouchstone(_)))
        ));
    }
}
