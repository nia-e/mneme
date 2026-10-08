use super::*;
use mneme_core::ports::{IncidentEdgeLeg, IncidentEdgesRequest};
use mneme_core::{BodyRef, BodySpan};

const VECTOR: [f32; 4] = [1.0, 0.0, 0.0, 0.0];

fn time(value: u128) -> EpisodeTime {
    EpisodeTime::new(value).unwrap()
}
fn limit(value: usize) -> EpisodePageLimit {
    EpisodePageLimit::new(value).unwrap()
}
fn base(key: &str, summary: &str, at: u128, codec: mneme_core::CaptureRequestCodec) -> Node {
    let source = CaptureSource::new_with_codec(
        "episode-read-test",
        key,
        "file:///episode-read-test",
        None,
        None,
        [1; 32],
        codec,
    )
    .unwrap();
    Node::try_new(
        source.node_id(),
        summary,
        BodyRef::new("inline://episode-read").unwrap(),
        ["episode-test"],
        Provenance::External { source },
        0.5,
        0.5,
        NodeStatus::Active,
        at,
    )
    .unwrap()
}
fn initial(
    key: &str,
    summary: &str,
    at: u128,
    occurred: OccurrenceSpan,
    thread: Option<&str>,
) -> Node {
    let node = base(key, summary, at, mneme_core::CaptureRequestCodec::EpisodeV1);
    let facet = EpisodeFacet::initial(
        node.id(),
        occurred,
        thread.map(|s| EpisodeThread::new(s).unwrap()),
        time(at),
    )
    .unwrap();
    node.with_episode(facet).unwrap()
}
fn revision(
    key: &str,
    summary: &str,
    at: u128,
    previous: &Node,
    occurred: OccurrenceSpan,
    thread: Option<&str>,
) -> Node {
    let old = previous.episode().unwrap();
    let facet = EpisodeFacet::revised(
        old.root(),
        previous.id(),
        old.revision().next().unwrap(),
        occurred,
        thread.map(|s| EpisodeThread::new(s).unwrap()),
        old.recorded_at(),
        EpisodeRevisionReason::new("Corrected the account").unwrap(),
    )
    .unwrap();
    base(key, summary, at, mneme_core::CaptureRequestCodec::EpisodeV1)
        .with_episode(facet)
        .unwrap()
}
async fn append(store: &CozoStore, node: &Node) {
    let expectation = node.episode().unwrap().revises().map_or(
        EpisodeWriteExpectation::NewRoot,
        EpisodeWriteExpectation::CurrentEdition,
    );
    store
        .commit_episode(EpisodeCommit {
            node,
            embedding: &VECTOR,
            links: &[],
            expectation,
        })
        .await
        .unwrap();
}
fn timeline() -> EpisodeTimelineRequest {
    EpisodeTimelineRequest::default()
}
fn cue(text: &str) -> EpisodeCueRequest {
    EpisodeCueRequest {
        cue: EpisodeCue::new(text).unwrap(),
        filter: EpisodeFilter::default(),
        limit: limit(8),
    }
}

#[tokio::test]
async fn episode_read_current_and_history_do_not_rewrite_the_old_scene() {
    let store = super::tests::episode_kernel_store();
    let first = initial(
        "first",
        "We thought the amber relay worked",
        100,
        OccurrenceSpan::Point { at: time(10) },
        Some("workshop"),
    );
    append(&store, &first).await;
    // A clock rollback during editing does not reorder recorded chronology.
    let revised = revision(
        "second",
        "The violet relay failed under load",
        90,
        &first,
        OccurrenceSpan::Range {
            start: time(5),
            end: time(20),
        },
        Some("bench"),
    );
    append(&store, &revised).await;
    let root = first.episode().unwrap().root();
    let current = store
        .get_episode(&EpisodeGet {
            episode_id: root,
            edition_id: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.node.id(), revised.id());
    assert_eq!(current.identity.revision.get(), 1);
    let old = store
        .get_episode(&EpisodeGet {
            episode_id: root,
            edition_id: Some(first.id()),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.node.summary(), first.summary());
    assert_eq!(old.current_edition_id, revised.id());
    let page = store.episode_timeline(&timeline()).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].identity.edition_id, revised.id());
    assert_eq!(page.items[0].recorded_at, time(100));
    assert_eq!(page.items[0].edition_recorded_at, time(90));
    assert!(
        store
            .episode_cue(&cue("amber"))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        store.episode_cue(&cue("violet")).await.unwrap().items[0]
            .identity
            .edition_id,
        revised.id()
    );
    let mut request = EpisodeHistoryRequest {
        episode_id: root,
        limit: limit(1),
        after: None,
    };
    let first_page = store.episode_history(&request).await.unwrap();
    assert_eq!(first_page.items[0].identity.edition_id, first.id());
    request.after = first_page.next;
    let second_page = store.episode_history(&request).await.unwrap();
    assert_eq!(second_page.items[0].identity.edition_id, revised.id());
    assert!(second_page.next.is_none());
    let mut prior_thread = timeline();
    prior_thread.filter.thread = Some(EpisodeThread::new("workshop").unwrap());
    assert!(
        store
            .episode_timeline(&prior_thread)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    prior_thread.filter.thread = Some(EpisodeThread::new("bench").unwrap());
    assert_eq!(
        store
            .episode_timeline(&prior_thread)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn episode_read_keysets_preserve_ties_and_bind_scope() {
    let store = super::tests::episode_kernel_store();
    let mut ids = Vec::new();
    for key in ["tie-a", "tie-b", "tie-c"] {
        let node = initial(
            key,
            "Same clock different scenes",
            100,
            OccurrenceSpan::Unknown,
            None,
        );
        ids.push(node.id());
        append(&store, &node).await;
    }
    ids.sort();
    for (order, expected) in [
        (EpisodeOrder::OldestFirst, ids.clone()),
        (
            EpisodeOrder::NewestFirst,
            ids.iter().rev().copied().collect(),
        ),
    ] {
        let mut request = EpisodeTimelineRequest {
            order,
            limit: limit(1),
            ..timeline()
        };
        let mut actual = Vec::new();
        loop {
            let page = store.episode_timeline(&request).await.unwrap();
            assert_eq!(page.items.len(), 1);
            assert!(!page.partial);
            actual.push(page.items[0].identity.edition_id);
            if page.next.is_none() {
                break;
            }
            request.after = page.next;
        }
        assert_eq!(actual, expected);
    }
    let mut request = EpisodeTimelineRequest {
        limit: limit(1),
        ..timeline()
    };
    request.after = store.episode_timeline(&request).await.unwrap().next;
    let other = super::tests::episode_kernel_store();
    assert!(matches!(
        other.episode_timeline(&request).await,
        Err(Error::InvalidInput(_))
    ));
    request.filter.occurrence = EpisodeOccurrenceFilter::Unknown;
    assert!(matches!(
        store.episode_timeline(&request).await,
        Err(Error::InvalidInput(_))
    ));
    let mut outside = EpisodeTimelineRequest {
        order: EpisodeOrder::OldestFirst,
        window: Some(EpisodeTimeWindow::new(Some(time(100)), Some(time(100))).unwrap()),
        ..timeline()
    };
    outside.after = Some(EpisodeTimelineCursor::new(
        store.db_id(),
        &outside,
        time(101),
        EpisodeId::new(ids[0]),
    ));
    assert!(
        store
            .episode_timeline(&outside)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn episode_read_occurrence_windows_overlap_full_ranges_and_keep_unknown_distinct() {
    let store = super::tests::episode_kernel_store();
    let range = initial(
        "range",
        "Long scene",
        100,
        OccurrenceSpan::Range {
            start: time(1),
            end: time(1000),
        },
        None,
    );
    let point = initial(
        "point",
        "Boundary scene",
        200,
        OccurrenceSpan::Point { at: time(500) },
        None,
    );
    let unknown = initial(
        "unknown",
        "Undated scene",
        300,
        OccurrenceSpan::Unknown,
        None,
    );
    for node in [&range, &point, &unknown] {
        append(&store, node).await;
    }
    let mut request = EpisodeTimelineRequest {
        axis: EpisodeTimelineAxis::Occurred,
        order: EpisodeOrder::OldestFirst,
        window: Some(EpisodeTimeWindow::new(Some(time(500)), Some(time(500))).unwrap()),
        ..timeline()
    };
    let page = store.episode_timeline(&request).await.unwrap();
    assert_eq!(
        page.items
            .iter()
            .map(|h| h.identity.edition_id)
            .collect::<Vec<_>>(),
        [range.id(), point.id()]
    );
    request.filter.occurrence = EpisodeOccurrenceFilter::Unknown;
    assert!(matches!(
        store.episode_timeline(&request).await,
        Err(Error::InvalidInput(_))
    ));
    request.axis = EpisodeTimelineAxis::Recorded;
    request.window = None;
    let page = store.episode_timeline(&request).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].identity.edition_id, unknown.id());
}

#[tokio::test]
async fn episode_read_filter_scan_budget_can_return_empty_continuable_page() {
    let store = super::tests::episode_kernel_store();
    for index in 0..257 {
        let occurred = if index == 256 {
            OccurrenceSpan::Point { at: time(99) }
        } else {
            OccurrenceSpan::Unknown
        };
        let node = initial(
            &format!("budget-{index}"),
            "An observed scene",
            index + 1,
            occurred,
            None,
        );
        append(&store, &node).await;
    }
    let mut request = EpisodeTimelineRequest {
        order: EpisodeOrder::OldestFirst,
        ..timeline()
    };
    request.filter.occurrence = EpisodeOccurrenceFilter::Overlaps(
        EpisodeTimeWindow::new(Some(time(99)), Some(time(99))).unwrap(),
    );
    let first = store.episode_timeline(&request).await.unwrap();
    assert!(first.items.is_empty());
    assert!(first.partial);
    assert!(first.next.is_some());
    request.after = first.next;
    let second = store.episode_timeline(&request).await.unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].recorded_at, time(257));
    assert!(!second.partial);
    assert!(second.next.is_none());
}

#[tokio::test]
async fn episode_read_cue_filters_before_top_k_and_treats_input_as_text() {
    let store = super::tests::episode_kernel_store();
    for index in 0..5 {
        let node = initial(
            &format!("distractor-{index}"),
            "Violet violet violet",
            index + 1,
            OccurrenceSpan::Unknown,
            Some("elsewhere"),
        );
        append(&store, &node).await;
    }
    let wanted = initial(
        "cue-wanted",
        "The violet relay failed under sustained load",
        10,
        OccurrenceSpan::Range {
            start: time(1),
            end: time(100),
        },
        Some("bench"),
    );
    append(&store, &wanted).await;
    let mut request = cue("(violet)");
    request.limit = limit(1);
    request.filter.thread = Some(EpisodeThread::new("bench").unwrap());
    request.filter.occurrence = EpisodeOccurrenceFilter::Overlaps(
        EpisodeTimeWindow::new(Some(time(80)), Some(time(80))).unwrap(),
    );
    let page = store.episode_cue(&request).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].identity.edition_id, wanted.id());
    assert!(!page.has_more);
    request = cue("violet");
    request.limit = limit(1);
    let page = store.episode_cue(&request).await.unwrap();
    assert!(page.has_more);
    assert!(page.partial);
    assert!(
        store
            .episode_cue(&cue("() : **"))
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn episode_read_references_keep_direction_weak_weight_anchor_and_historical_endpoint() {
    let store = super::tests::episode_kernel_store();
    let first = initial(
        "reference-first",
        "A scene",
        1,
        OccurrenceSpan::Unknown,
        None,
    );
    append(&store, &first).await;
    let revised = revision(
        "reference-second",
        "The corrected scene",
        2,
        &first,
        OccurrenceSpan::Unknown,
        None,
    );
    append(&store, &revised).await;
    let semantic = base(
        "semantic",
        "What we learned",
        3,
        mneme_core::CaptureRequestCodec::CaptureV2,
    );
    store.put_node(&semantic).await.unwrap();
    let semantic_only = base(
        "other-semantic",
        "An unrelated principle",
        3,
        mneme_core::CaptureRequestCodec::CaptureV2,
    );
    store.put_node(&semantic_only).await.unwrap();
    store
        .put_edge(&Edge::new(
            semantic.id(),
            semantic_only.id(),
            0.99,
            EdgeKind::Associative,
            3,
        ))
        .await
        .unwrap();
    let mut incoming = Edge::new(first.id(), semantic.id(), 0.01, EdgeKind::DerivedFrom, 3);
    incoming.anchor = Some(BodySpan::new(1, 4));
    store.put_edge(&incoming).await.unwrap();
    let outgoing = Edge::new(semantic.id(), revised.id(), 0.02, EdgeKind::Transition, 3);
    store.put_edge(&outgoing).await.unwrap();
    let mut request = EpisodeReferencesRequest {
        anchor: semantic.id(),
        limit: limit(1),
        after: None,
    };
    let mut found = Vec::new();
    loop {
        let page = store.episode_references(&request).await.unwrap();
        found.extend(page.items);
        if page.next.is_none() {
            break;
        }
        request.after = page.next;
    }
    assert_eq!(found.len(), 2);
    let kept = found.iter().find(|r| r.edge.from == first.id()).unwrap();
    assert_eq!(kept.edge.to, semantic.id());
    assert_eq!(kept.edge.kind, EdgeKind::DerivedFrom);
    assert_eq!(kept.edge.weight(), 0.01);
    assert_eq!(kept.edge.anchor, incoming.anchor);
    assert_eq!(kept.from_episode.unwrap().edition_id, first.id());
    assert_eq!(kept.from_episode.unwrap().revision.get(), 0);
    assert!(kept.to_episode.is_none());
    let root_only = store
        .episode_references(&EpisodeReferencesRequest {
            anchor: first.id(),
            limit: limit(8),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(root_only.items.len(), 1);
    assert_eq!(root_only.items[0].edge.from, first.id());
}

#[tokio::test]
async fn episode_read_generation_and_live_database_identity_are_checked_in_snapshot() {
    let store = CozoStore::new(4).unwrap();
    assert!(
        store
            .episode_timeline(&timeline())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    // Simulate a recognized predecessor marker explicitly; new stores have the lane.
    store
        .put_meta(
            VECTOR_PROJECTION_META_KEY,
            CAPTURE_V1_CATALOG_GENERATION_MARKER,
        )
        .unwrap();
    assert!(matches!(
        store.episode_timeline(&timeline()).await,
        Err(Error::EpisodeUnavailable(
            mneme_core::episode::EpisodeUnavailableReason::StoreNotUpgraded
        ))
    ));
    let store = super::tests::episode_kernel_store();
    store.put_meta("db_id", &Ulid::new().to_string()).unwrap();
    assert!(matches!(
        store.episode_timeline(&timeline()).await,
        Err(Error::Backend(_))
    ));
}

// Synthetic authoring metadata is fixed before commit, not patched in storage.
fn linked_recorded_in(node: Node, session: &str) -> Node {
    let Provenance::External { source } = node.provenance() else {
        panic!("expected source");
    };
    let source = CaptureSource::new_with_codec(
        source.namespace(),
        source.key(),
        source.reference(),
        Some(session),
        source.revision(),
        source.request_digest(),
        source.request_codec(),
    )
    .unwrap();
    Node::try_new(
        node.id(),
        node.summary(),
        node.body().clone(),
        node.tags(),
        Provenance::External { source },
        node.stability(),
        node.confidence(),
        node.status(),
        node.created(),
    )
    .unwrap()
    .with_episode(node.episode().unwrap().clone())
    .unwrap()
}

#[tokio::test]
async fn linked_exact_header_preserves_old_edition_and_observed_head() {
    let store = super::tests::episode_kernel_store();
    let old = initial(
        "linked-old",
        "Original Pi scene",
        10,
        OccurrenceSpan::Point { at: time(5) },
        Some("pi"),
    );
    let corrected = revision(
        "linked-new",
        "Corrected Mac scene",
        20,
        &old,
        OccurrenceSpan::Unknown,
        Some("mac"),
    );
    let old = linked_recorded_in(old, "pi-recorder");
    let corrected = linked_recorded_in(corrected, "mac-recorder");
    append(&store, &old).await;
    append(&store, &corrected).await;
    let lesson = base(
        "linked-semantic",
        "Lesson",
        21,
        mneme_core::CaptureRequestCodec::CaptureV2,
    );
    store.put_node(&lesson).await.unwrap();
    let before = store.export().await.unwrap().canonical_value().unwrap();
    let request =
        |id| EpisodeHeaderByEditionRequest::new(id, std::time::Duration::from_secs(1)).unwrap();
    let EpisodeHeaderByEdition::Episode(header) = store
        .episode_header_by_edition(&request(old.id()))
        .await
        .unwrap()
    else {
        panic!("expected episode header");
    };
    assert_eq!(header.identity.edition_id, old.id());
    assert_eq!(header.current_edition_id, corrected.id());
    assert_eq!(header.summary.as_str(), old.summary());
    assert_eq!(header.occurred, old.episode().unwrap().occurrence().clone());
    assert_eq!(header.recorded_at, time(10));
    assert_eq!(header.edition_recorded_at, time(10));
    assert_eq!(
        header.recording_session.as_ref().unwrap().as_str(),
        "pi-recorder"
    );
    let EpisodeHeaderByEdition::Episode(current) = store
        .episode_header_by_edition(&request(corrected.id()))
        .await
        .unwrap()
    else {
        panic!("expected current header");
    };
    assert_eq!(
        current.recording_session.as_ref().unwrap().as_str(),
        "mac-recorder"
    );
    assert_eq!(header.thread.as_ref().unwrap().as_str(), "pi");
    assert!(matches!(
        store
            .episode_header_by_edition(&request(lesson.id()))
            .await
            .unwrap(),
        EpisodeHeaderByEdition::Semantic
    ));
    assert!(matches!(
        store
            .episode_header_by_edition(&request(NodeId(Ulid::from(u128::MAX))))
            .await
            .unwrap(),
        EpisodeHeaderByEdition::Missing
    ));
    assert_eq!(
        store.export().await.unwrap().canonical_value().unwrap(),
        before
    );
}

#[tokio::test]
async fn linked_native_incident_seeks_bound_both_hub_legs_and_match_mem() {
    let store = super::tests::episode_kernel_store();
    let mem = crate::MemStore::new(4);
    let anchor = NodeId(Ulid::from(1000u128));
    for i in 1..=80u128 {
        let endpoint = NodeId(Ulid::from(i));
        for mut edge in [
            Edge::new(anchor, endpoint, 0.01, EdgeKind::Transition, 1),
            Edge::new(endpoint, anchor, 0.99, EdgeKind::DerivedFrom, 2),
        ] {
            edge.anchor = Some(BodySpan::new(1, 4));
            store.put_edge(&edge).await.unwrap();
            mem.put_edge(&edge).await.unwrap();
        }
    }
    let before = store.export().await.unwrap().canonical_value().unwrap();
    let mut after = None;
    let mut found = std::collections::BTreeSet::new();
    let mut raw_rows = 0;
    loop {
        let request =
            IncidentEdgesRequest::new(anchor, 7, std::time::Duration::from_secs(1), after).unwrap();
        let native = store.incident_edges_page(&request).await.unwrap().unwrap();
        let modeled = mem.incident_edges_page(&request).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&native.items).unwrap(),
            serde_json::to_value(&modeled.items).unwrap()
        );
        assert_eq!(native.next, modeled.next);
        assert_eq!(native.work.rows_scanned, native.items.len());
        assert!(native.work.rows_scanned <= 7);
        assert!(native.work.indexed_seeks <= 3);
        assert_eq!(native.work.indexed_seeks, modeled.work.indexed_seeks);
        assert_eq!(
            native.work.edge_point_reads,
            native.items.iter().filter(|e| e.to == anchor).count()
        );
        assert_eq!(native.work.body_anchor_point_reads, native.items.len());
        raw_rows += native.work.rows_scanned;
        for edge in native.items {
            assert!(found.insert((edge.from, edge.to)));
        }
        after = native.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(raw_rows, 160);
    assert_eq!(found.len(), 160);
    assert_eq!(
        store.export().await.unwrap().canonical_value().unwrap(),
        before
    );
    let request =
        IncidentEdgesRequest::new(anchor, 1, std::time::Duration::from_secs(1), None).unwrap();
    let first = store.incident_edges_page(&request).await.unwrap().unwrap();
    assert_eq!(first.items[0].from, anchor);
    assert_eq!(
        first.next.as_ref().unwrap().next_leg(),
        IncidentEdgeLeg::Incoming
    );
    let request =
        IncidentEdgesRequest::new(anchor, 1, std::time::Duration::from_secs(1), first.next)
            .unwrap();
    let second = store.incident_edges_page(&request).await.unwrap().unwrap();
    assert_eq!(second.items[0].to, anchor);
    assert_eq!(second.work.rows_scanned, 1);
    assert_eq!(second.work.edge_point_reads, 1);
}

#[tokio::test]
async fn linked_native_raw_edges_include_episodes_missing_and_duplicate_self_rows() {
    let store = super::tests::episode_kernel_store();
    let scene = initial(
        "linked-raw-scene",
        "A scene",
        10,
        OccurrenceSpan::Unknown,
        None,
    );
    append(&store, &scene).await;
    let lesson = base(
        "linked-raw-lesson",
        "A lesson",
        11,
        mneme_core::CaptureRequestCodec::CaptureV2,
    );
    store.put_node(&lesson).await.unwrap();
    let missing = NodeId(Ulid::from(u128::MAX));
    let edges = [
        Edge::new(scene.id(), lesson.id(), 0.01, EdgeKind::DerivedFrom, 1),
        Edge::new(lesson.id(), missing, 0.01, EdgeKind::Transition, 1),
        Edge::new(lesson.id(), lesson.id(), 0.01, EdgeKind::Associative, 1),
    ];
    for edge in &edges {
        store.put_edge(edge).await.unwrap();
    }
    let request =
        IncidentEdgesRequest::new(lesson.id(), 8, std::time::Duration::from_secs(1), None).unwrap();
    let page = store.incident_edges_page(&request).await.unwrap().unwrap();
    assert!(page.next.is_none());
    assert_eq!(page.work.rows_scanned, 4);
    assert_eq!(page.work.indexed_seeks, 2);
    assert_eq!(page.work.edge_point_reads, 2);
    assert_eq!(page.work.body_anchor_point_reads, 4);
    assert_eq!(
        page.items
            .iter()
            .filter(|e| e.from == lesson.id() && e.to == lesson.id())
            .count(),
        2
    );
    assert!(page.items.iter().any(|e| e.from == scene.id()));
    assert!(page.items.iter().any(|e| e.to == missing));
}
