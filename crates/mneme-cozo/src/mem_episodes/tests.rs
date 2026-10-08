use super::*;
use mneme_core::ports::EpisodeStore;
use mneme_core::{BodyRef, CaptureReplayProof, CaptureRequestCodec, EdgeKind};

fn time(n: u128) -> EpisodeTime {
    EpisodeTime::new(n).unwrap()
}
fn authored_with_codec(
    key: &str,
    summary: &str,
    created: u128,
    codec: CaptureRequestCodec,
) -> Node {
    let source = CaptureSource::new_with_codec(
        "episode-tests",
        key,
        "test://episodes",
        None,
        None,
        [1; 32],
        codec,
    )
    .unwrap();
    Node::try_new(
        source.node_id(),
        summary,
        BodyRef::new(format!("inline://{key}")).unwrap(),
        ["scene"],
        Provenance::External { source },
        0.5,
        0.5,
        NodeStatus::Active,
        created,
    )
    .unwrap()
}
fn authored(key: &str, summary: &str, created: u128) -> Node {
    authored_with_codec(key, summary, created, CaptureRequestCodec::CaptureV2)
}
fn episode(
    key: &str,
    summary: &str,
    created: u128,
    occurrence: OccurrenceSpan,
    thread: Option<&str>,
) -> Node {
    let node = authored_with_codec(key, summary, created, CaptureRequestCodec::EpisodeV1);
    let facet = EpisodeFacet::initial(
        node.id(),
        occurrence,
        thread.map(|t| EpisodeThread::new(t).unwrap()),
        time(created),
    )
    .unwrap();
    node.with_episode(facet).unwrap()
}
fn revision(previous: &Node, key: &str, summary: &str, created: u128) -> Node {
    let before = previous.episode().unwrap();
    let facet = EpisodeFacet::revised(
        before.root(),
        previous.id(),
        before.revision().next().unwrap(),
        before.occurrence().clone(),
        before.thread().cloned(),
        before.recorded_at(),
        EpisodeRevisionReason::new("Correct the account").unwrap(),
    )
    .unwrap();
    authored_with_codec(key, summary, created, CaptureRequestCodec::EpisodeV1)
        .with_episode(facet)
        .unwrap()
}
#[test]
fn named_legacy_episode_decoder_derives_old_codec_without_changing_proof() {
    let old = episode(
        "legacy-edition",
        "Historical account",
        3,
        OccurrenceSpan::Unknown,
        None,
    );
    let digest = source(&old).request_digest();
    let mut wire = serde_json::to_value(&old).unwrap();
    wire["provenance"]["External"]["source"]
        .as_object_mut()
        .unwrap()
        .remove("request_codec");
    let (decoded, original_status) = crate::decode_legacy_node_v1(&wire.to_string()).unwrap();
    assert_eq!(original_status, "active");
    assert_eq!(decoded.episode(), old.episode());
    assert_eq!(
        source(&decoded).request_codec(),
        CaptureRequestCodec::EpisodeV1
    );
    assert_eq!(source(&decoded).request_digest(), digest);
}

async fn append(store: &MemStore, node: &Node) -> EpisodeCommitOutcome {
    store
        .commit_episode(EpisodeCommit {
            node,
            embedding: &[1.0, 0.0],
            links: &[],
            expectation: EpisodeWriteExpectation::NewRoot,
        })
        .await
        .unwrap()
}
async fn revise(store: &MemStore, previous: &Node, node: &Node) -> EpisodeCommitOutcome {
    store
        .commit_episode(EpisodeCommit {
            node,
            embedding: &[1.0, 0.0],
            links: &[],
            expectation: EpisodeWriteExpectation::CurrentEdition(previous.id()),
        })
        .await
        .unwrap()
}
fn source(node: &Node) -> &CaptureSource {
    match node.provenance() {
        Provenance::External { source } => source,
        _ => unreachable!(),
    }
}
fn cue(text: &str) -> EpisodeCueRequest {
    EpisodeCueRequest {
        cue: EpisodeCue::new(text).unwrap(),
        filter: EpisodeFilter::default(),
        limit: EpisodePageLimit::default(),
    }
}

#[tokio::test]
async fn editions_are_atomic_and_replay_original_after_revision_without_recreating_links() {
    let store = MemStore::new(2);
    let lesson = authored("lesson", "A lesson", 1);
    store.put_node(&lesson).await.unwrap();
    let first = episode(
        "first",
        "The wrong turn",
        5,
        OccurrenceSpan::Unknown,
        Some("walk"),
    );
    let link = Edge::new(first.id(), lesson.id(), 0.7, EdgeKind::Associative, 5);
    let initial = store
        .commit_episode(EpisodeCommit {
            node: &first,
            embedding: &[1.0, 0.0],
            links: &[link.clone()],
            expectation: EpisodeWriteExpectation::NewRoot,
        })
        .await
        .unwrap();
    assert!(matches!(initial, EpisodeCommitOutcome::Applied(_)));
    let second = revision(&first, "second", "The corrected account", 4); // clock reversal does not reverse editions
    revise(&store, &first, &second).await;
    store.delete_edge(first.id(), lesson.id()).await.unwrap();
    let replay = store
        .commit_episode(EpisodeCommit {
            node: &first,
            embedding: &[1.0, 0.0],
            links: &[link],
            expectation: EpisodeWriteExpectation::NewRoot,
        })
        .await
        .unwrap();
    assert_eq!(
        replay,
        EpisodeCommitOutcome::AlreadyApplied(identity(&first))
    );
    assert!(
        store
            .get_edge(first.id(), lesson.id())
            .await
            .unwrap()
            .is_none()
    );
    let replay_record = store.lookup_episode(source(&first)).await.unwrap().unwrap();
    assert_eq!(replay_record.node.summary(), first.summary());
    assert_eq!(replay_record.current_edition_id, second.id());
    let history = store
        .episode_history(&EpisodeHistoryRequest {
            episode_id: first.episode().unwrap().root(),
            limit: EpisodePageLimit::default(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(
        history
            .items
            .iter()
            .map(|x| x.identity.edition_id)
            .collect::<Vec<_>>(),
        vec![first.id(), second.id()]
    );
    let old = store
        .get_episode(&EpisodeGet {
            episode_id: first.episode().unwrap().root(),
            edition_id: Some(first.id()),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.node.body(), first.body());
    assert_eq!(old.identity.revision.get(), 0);
    assert!(
        store
            .episode_cue(&cue("wrong"))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        store.episode_cue(&cue("corrected")).await.unwrap().items[0]
            .identity
            .edition_id,
        second.id()
    );
}

#[tokio::test]
async fn stale_successor_and_bad_links_leave_no_prefix() {
    let store = MemStore::new(2);
    let first = episode("cas-root", "Root", 1, OccurrenceSpan::Unknown, None);
    append(&store, &first).await;
    let winner = revision(&first, "cas-win", "Won the edit", 2);
    let loser = revision(&first, "cas-lose", "Lost the edit", 2);
    revise(&store, &first, &winner).await;
    let before = store.export();
    let result = store
        .commit_episode(EpisodeCommit {
            node: &loser,
            embedding: &[1.0, 0.0],
            links: &[],
            expectation: EpisodeWriteExpectation::CurrentEdition(first.id()),
        })
        .await;
    assert!(matches!(result, Err(Error::Conflict(_))));
    assert!(store.get_node(loser.id()).await.unwrap().is_none());
    let after = store.export();
    assert!(same_serialized(&before.nodes, &after.nodes).unwrap());
    assert_eq!(
        before.vectors.into_iter().collect::<BTreeMap<_, _>>(),
        after.vectors.into_iter().collect::<BTreeMap<_, _>>()
    );
    assert!(same_serialized(&before.edges, &after.edges).unwrap());
    assert_eq!(
        store
            .get_episode(&EpisodeGet {
                episode_id: first.episode().unwrap().root(),
                edition_id: None
            })
            .await
            .unwrap()
            .unwrap()
            .identity
            .edition_id,
        winner.id()
    );
    let unattached = episode(
        "bad-link",
        "Not published",
        3,
        OccurrenceSpan::Unknown,
        None,
    );
    let dangling = Edge::new(
        unattached.id(),
        NodeId(Ulid::new()),
        0.8,
        EdgeKind::Associative,
        3,
    );
    assert!(
        store
            .commit_episode(EpisodeCommit {
                node: &unattached,
                embedding: &[1.0, 0.0],
                links: &[dangling],
                expectation: EpisodeWriteExpectation::NewRoot
            })
            .await
            .is_err()
    );
    assert!(store.get_node(unattached.id()).await.unwrap().is_none());
    assert_eq!(
        store
            .episode_timeline(&EpisodeTimelineRequest::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn timeline_is_keyset_ordered_thread_filtered_and_full_span_overlapping() {
    let store = MemStore::new(2);
    let a = episode(
        "timeline-a",
        "Long scene",
        30,
        OccurrenceSpan::range(time(1), time(100)).unwrap(),
        Some("walk"),
    );
    let b = episode(
        "timeline-b",
        "Brief scene",
        30,
        OccurrenceSpan::Point { at: time(50) },
        Some("walk"),
    );
    let c = episode(
        "timeline-c",
        "Other project",
        10,
        OccurrenceSpan::Point { at: time(50) },
        Some("other"),
    );
    let d = episode(
        "timeline-d",
        "Unknown time",
        40,
        OccurrenceSpan::Unknown,
        Some("walk"),
    );
    for node in [&a, &b, &c, &d] {
        append(&store, node).await;
    }
    let mut request = EpisodeTimelineRequest {
        axis: EpisodeTimelineAxis::Occurred,
        order: EpisodeOrder::OldestFirst,
        window: Some(EpisodeTimeWindow::new(Some(time(40)), Some(time(60))).unwrap()),
        filter: EpisodeFilter {
            thread: Some(EpisodeThread::new("walk").unwrap()),
            ..Default::default()
        },
        limit: EpisodePageLimit::new(1).unwrap(),
        after: None,
    };
    let first = store.episode_timeline(&request).await.unwrap();
    assert_eq!(first.items[0].identity.edition_id, a.id());
    assert!(!first.partial);
    request.after = first.next;
    let second = store.episode_timeline(&request).await.unwrap();
    assert_eq!(second.items[0].identity.edition_id, b.id());
    assert!(second.next.is_none());
    request.order = EpisodeOrder::NewestFirst;
    assert!(store.episode_timeline(&request).await.is_err()); // cursor bound to order
    let tied = store
        .episode_timeline(&EpisodeTimelineRequest {
            order: EpisodeOrder::OldestFirst,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(tied.items[0].identity.edition_id, c.id());
    let mut expected = [a.id(), b.id()];
    expected.sort();
    assert_eq!(
        tied.items[1..3]
            .iter()
            .map(|x| x.identity.edition_id)
            .collect::<Vec<_>>(),
        expected
    );
}

#[tokio::test]
async fn timeline_out_of_window_continuation_is_empty_not_a_range_panic() {
    let store = MemStore::new(2);
    let node = episode("bounds", "Scene", 50, OccurrenceSpan::Unknown, None);
    append(&store, &node).await;
    for (order, out) in [
        (EpisodeOrder::OldestFirst, 100),
        (EpisodeOrder::NewestFirst, 1),
    ] {
        let mut request = EpisodeTimelineRequest {
            order,
            window: Some(EpisodeTimeWindow::new(Some(time(40)), Some(time(60))).unwrap()),
            ..Default::default()
        };
        request.after = Some(EpisodeTimelineCursor::new(
            store.db_id,
            &request,
            time(out),
            node.episode().unwrap().root(),
        ));
        let page = store.episode_timeline(&request).await.unwrap();
        assert!(page.items.is_empty());
        assert!(page.next.is_none());
    }
}

#[tokio::test]
async fn sparse_timeline_filter_has_bounded_work_and_progressing_cursor() {
    let store = MemStore::new(2);
    for index in 0..300 {
        let node = episode(
            &format!("sparse-{index}"),
            "Scene",
            index,
            OccurrenceSpan::Unknown,
            None,
        );
        append(&store, &node).await;
    }
    let mut request = EpisodeTimelineRequest {
        filter: EpisodeFilter {
            occurrence: EpisodeOccurrenceFilter::Overlaps(
                EpisodeTimeWindow::new(Some(time(1)), None).unwrap(),
            ),
            ..Default::default()
        },
        ..Default::default()
    };
    let first = store.episode_timeline(&request).await.unwrap();
    assert!(first.items.is_empty());
    assert!(first.partial);
    assert!(first.next.is_some());
    request.after = first.next;
    let second = store.episode_timeline(&request).await.unwrap();
    assert!(second.items.is_empty());
    assert!(!second.partial);
    assert!(second.next.is_none());
}

#[tokio::test]
async fn import_preserves_all_editions_and_rebuilds_only_current_indexes() {
    let store = MemStore::new(2);
    let first = episode(
        "import-root",
        "Mistaken scene",
        1,
        OccurrenceSpan::Unknown,
        Some("walk"),
    );
    append(&store, &first).await;
    let edit = revision(&first, "import-edit", "Correct scene", 2);
    revise(&store, &first, &edit).await;
    let imported = MemStore::from_export(store.export()).unwrap();
    assert_eq!(imported.export().nodes.len(), 2);
    assert_eq!(imported.export().vectors.len(), 2);
    assert_eq!(
        imported.all_nodes(ColdPath::acquire()).await.unwrap().len(),
        2
    );
    assert!(
        imported
            .episode_cue(&cue("mistaken"))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        imported
            .episode_cue(&cue("correct"))
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(
        imported
            .lookup_episode(source(&first))
            .await
            .unwrap()
            .unwrap()
            .node
            .body(),
        first.body()
    );
    let mut missing_vector = store.export();
    missing_vector.vectors.retain(|(id, _)| *id != first.id());
    assert!(MemStore::from_export(missing_vector).is_err());
    let mut missing_root = store.export();
    missing_root.nodes.retain(|node| node.id() != first.id());
    missing_root.vectors.retain(|(id, _)| *id != first.id());
    assert!(MemStore::from_export(missing_root).is_err());
    let mut competing = store.export();
    let fork = revision(&first, "import-fork", "Competing account", 3);
    competing.nodes.push(fork.clone());
    competing.vectors.push((fork.id(), vec![1.0, 0.0]));
    assert!(MemStore::from_export(competing).is_err());
    let mut forged = serde_json::to_value(&first).unwrap();
    forged["provenance"] =
        serde_json::to_value(authored("foreign-source", "Foreign proof", 1).provenance()).unwrap();
    let error = serde_json::from_value::<Node>(forged).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("identity differs from capture source")
    );
}

#[tokio::test]
async fn episodes_cannot_starve_semantic_seeds_or_bridge_spread() {
    let store = MemStore::new(2);
    let lesson = authored("semantic-seed", "Walk scene lesson", 1);
    let far = authored("semantic-far", "Other lesson", 1);
    for node in [&lesson, &far] {
        store.put_node(node).await.unwrap();
        store.upsert(node.id(), &[0.8, 0.2]).await.unwrap();
    }
    let lexical_before = store.search("scene", 1, StatusFilter::ALL).await.unwrap()[0].score;
    let mut episodes = Vec::new();
    for i in 0..20 {
        let node = episode(
            &format!("seed-e-{i}"),
            "Walk scene",
            i,
            OccurrenceSpan::Unknown,
            None,
        );
        append(&store, &node).await;
        store
            .put_edge(&Edge::new(
                lesson.id(),
                node.id(),
                1.0,
                EdgeKind::Associative,
                i,
            ))
            .await
            .unwrap();
        store
            .put_edge(&Edge::new(
                node.id(),
                far.id(),
                1.0,
                EdgeKind::Associative,
                i,
            ))
            .await
            .unwrap();
        episodes.push(node);
    }
    let ann = store.ann(&[1.0, 0.0], 2, StatusFilter::ALL).await.unwrap();
    assert_eq!(ann.len(), 2);
    assert!(ann.iter().all(|x| x.id == lesson.id() || x.id == far.id()));
    let lexical = store.search("scene", 1, StatusFilter::ALL).await.unwrap();
    assert_eq!(lexical[0].id, lesson.id());
    assert_eq!(lexical[0].score, lexical_before);
    assert_eq!(store.lock().tag_projection.active["scene"].len(), 2);
    let spread = store
        .spread(
            &[
                Scored {
                    id: lesson.id(),
                    score: 1.0,
                },
                Scored {
                    id: episodes[0].id(),
                    score: 10.0,
                },
            ],
            Budget {
                max_nodes: 2,
                max_depth: 2,
                ..Default::default()
            },
            None,
            TraversalScope::new(StatusFilter::ALL),
        )
        .await
        .unwrap();
    assert_eq!(spread.len(), 1);
    assert_eq!(spread[0].id, lesson.id());
    let classifications = store
        .get_node_statuses(&[lesson.id(), episodes[0].id()])
        .await
        .unwrap();
    assert!(classifications[0].is_some());
    assert!(classifications[1].is_none());
    assert!(store.get_nodes(&[episodes[0].id()]).await.unwrap()[0].is_some());
    assert_eq!(
        store
            .detect_communities(ColdPath::acquire())
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn generic_ports_cannot_rewrite_episodes_or_forget_evidence_anchors() {
    let store = MemStore::new(2);
    let scene = episode("immutable", "Account", 1, OccurrenceSpan::Unknown, None);
    append(&store, &scene).await;
    assert!(
        store
            .compare_replace_node_body(
                scene.id(),
                &scene.body_revision(),
                &BodyRef::new("inline://replacement").unwrap()
            )
            .await
            .is_err()
    );
    assert!(
        store
            .compare_replace_node_tags(scene.id(), scene.tag_set(), scene.tag_set())
            .await
            .is_err()
    );
    assert!(store.put_node(&scene).await.is_err());
    assert!(
        store
            .put_node(&authored("immutable", "Semantic disguise", 1))
            .await
            .is_err()
    );
    assert!(
        store
            .set_status(scene.id(), NodeStatus::Archived)
            .await
            .is_err()
    );
    assert!(store.delete_node(scene.id()).await.is_err());
    assert!(store.upsert(scene.id(), &[0.0, 1.0]).await.is_err());
    assert!(store.remove(scene.id()).await.is_err());
    let proof = CaptureReplayProof::episode(source(&scene).clone()).unwrap();
    assert!(store.lookup_capture(&proof).await.is_err());
    assert!(
        store
            .commit_capture(&scene, &[1.0, 0.0], &proof)
            .await
            .is_err()
    );
    let lesson = authored("anchor", "A lesson", 1);
    store.put_node(&lesson).await.unwrap();
    assert!(store.lookup_episode(source(&lesson)).await.is_err());
    let evidence = Edge::new(scene.id(), lesson.id(), 0.01, EdgeKind::Associative, 1);
    store.put_edge(&evidence).await.unwrap();
    assert!(store.delete_node(lesson.id()).await.is_err());
    assert!(
        store
            .observe_merge_candidate(scene.id(), lesson.id(), 2)
            .await
            .is_err()
    );
    assert!(
        store
            .observe_contradiction(scene.id(), lesson.id(), 2)
            .await
            .is_err()
    );
    let result = store
        .prune_incident_associations(ColdPath::acquire(), lesson.id(), 0, 32)
        .await
        .unwrap();
    assert_eq!(result.pruned, 0);
    assert!(
        store
            .maintenance_overfull_hubs(ColdPath::acquire(), &[lesson.id()], 0)
            .await
            .unwrap()
            .is_empty()
    );
    let mut changed = evidence.clone();
    changed.decay(&mneme_core::StrengthParams::default());
    let commit = MaintenanceCommit {
        edges: vec![MaintenanceEdgeMutation::Decay {
            expected: evidence.clone(),
            replacement: changed,
        }],
    };
    assert!(
        store
            .commit_maintenance(ColdPath::acquire(), &commit)
            .await
            .is_err()
    );
    assert!(
        same_serialized(
            &store
                .get_edge(scene.id(), lesson.id())
                .await
                .unwrap()
                .unwrap(),
            &evidence
        )
        .unwrap()
    );
}

#[tokio::test]
async fn full_merge_keeps_exact_historical_lesson_anchor_and_incident_reference_directions() {
    let store = MemStore::new(2);
    let winner = authored("winner", "Current lesson", 1);
    let loser = authored("loser", "Historical lesson", 1);
    for node in [&winner, &loser] {
        store.put_node(node).await.unwrap();
    }
    let scene = episode(
        "merge-scene",
        "What happened",
        1,
        OccurrenceSpan::Unknown,
        None,
    );
    append(&store, &scene).await;
    let left = Edge::new(loser.id(), scene.id(), 0.8, EdgeKind::DerivedFrom, 1);
    let right = Edge::new(scene.id(), loser.id(), 0.6, EdgeKind::Transition, 1);
    for edge in [&left, &right] {
        store.put_edge(edge).await.unwrap();
    }
    store
        .observe_merge_candidate(winner.id(), loser.id(), 2)
        .await
        .unwrap();
    let commit = FullMergeCommit::new(winner.id(), loser.id(), 3).unwrap();
    store.commit_full_merge(&commit).await.unwrap();
    assert!(
        store
            .get_node(loser.id())
            .await
            .unwrap()
            .unwrap()
            .is_archived()
    );
    assert!(
        same_serialized(
            &store.get_edge(left.from, left.to).await.unwrap().unwrap(),
            &left
        )
        .unwrap()
    );
    assert!(
        store
            .get_edge(winner.id(), scene.id())
            .await
            .unwrap()
            .is_none()
    );
    let mut request = EpisodeReferencesRequest {
        anchor: loser.id(),
        limit: EpisodePageLimit::new(1).unwrap(),
        after: None,
    };
    let first = store.episode_references(&request).await.unwrap();
    assert_eq!(first.items.len(), 1);
    assert!(first.next.is_some());
    request.after = first.next;
    let second = store.episode_references(&request).await.unwrap();
    assert_eq!(second.items.len(), 1);
    assert!(second.next.is_none());
    assert_ne!(
        (first.items[0].edge.from, first.items[0].edge.to),
        (second.items[0].edge.from, second.items[0].edge.to)
    );
    assert!(MemStore::from_export(store.export()).is_ok());
}

fn contexts(key: &str) -> OccurrenceContexts {
    OccurrenceContexts::new(vec![
        OccurrenceContextRef::new("place", key, Some("The room where it happened")).unwrap(),
        OccurrenceContextRef::new("conversation", "shared-scene", None).unwrap(),
    ])
    .unwrap()
}

fn contextual_episode(key: &str, metadata: OccurrenceContexts) -> Node {
    let node = authored_with_codec(key, "Contextual scene", 10, CaptureRequestCodec::EpisodeV2);
    let facet = EpisodeFacet::initial(node.id(), OccurrenceSpan::Unknown, None, time(10))
        .unwrap()
        .with_occurrence_contexts(metadata);
    node.with_episode(facet).unwrap()
}

#[tokio::test]
async fn contextual_editions_roundtrip_replay_and_headers_without_inheriting_context() {
    let store = MemStore::new(2);
    let initial_context = contexts("old-room");
    let first = contextual_episode("context-roundtrip-original", initial_context.clone());
    append(&store, &first).await;
    // A context-free revision means unknown, not an inherited old room.
    let second = revision(
        &first,
        "context-roundtrip-revision",
        "Contextual corrected scene",
        11,
    );
    revise(&store, &first, &second).await;
    assert!(second.episode().unwrap().occurrence_contexts().is_none());
    let final_context = contexts("new-room");
    let facet = EpisodeFacet::revised(
        first.episode().unwrap().root(),
        second.id(),
        second.episode().unwrap().revision().next().unwrap(),
        OccurrenceSpan::Unknown,
        None,
        first.episode().unwrap().recorded_at(),
        EpisodeRevisionReason::new("The corrected setting").unwrap(),
    )
    .unwrap()
    .with_occurrence_contexts(final_context.clone());
    let third = authored_with_codec(
        "context-roundtrip-third",
        "Contextual final scene",
        12,
        CaptureRequestCodec::EpisodeV2,
    )
    .with_episode(facet)
    .unwrap();
    revise(&store, &second, &third).await;
    let before = store.export().canonical_value().unwrap();
    let path = std::env::temp_dir().join(format!("mneme-context-roundtrip-{}.json", Ulid::new()));
    store.save(&path).unwrap();
    let wire: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(wire["schema"], STORE_EXPORT_SCHEMA_V5);
    let loaded = MemStore::load(&path).unwrap();
    assert_eq!(loaded.export().canonical_value().unwrap(), before);
    assert!(matches!(
        append(&loaded, &first).await,
        EpisodeCommitOutcome::AlreadyApplied(_)
    ));
    let root = first.episode().unwrap().root();
    let historical = loaded
        .get_episode(&EpisodeGet {
            episode_id: root,
            edition_id: Some(first.id()),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        historical.node.episode().unwrap().occurrence_contexts(),
        Some(&initial_context)
    );
    assert_eq!(source(&historical.node), source(&first));
    assert_eq!(historical.node.body(), first.body());
    assert_eq!(historical.current_edition_id, third.id());
    let history = loaded
        .episode_history(&EpisodeHistoryRequest {
            episode_id: root,
            limit: EpisodePageLimit::default(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(history.items.len(), 3);
    assert_eq!(history.items[0].occurrence_contexts, Some(initial_context));
    assert_eq!(history.items[1].occurrence_contexts, None);
    assert_eq!(
        history.items[2].occurrence_contexts,
        Some(final_context.clone())
    );
    assert_eq!(
        loaded
            .episode_timeline(&EpisodeTimelineRequest::default())
            .await
            .unwrap()
            .items[0]
            .occurrence_contexts,
        Some(final_context.clone())
    );
    assert_eq!(
        loaded.episode_cue(&cue("contextual")).await.unwrap().items[0].occurrence_contexts,
        Some(final_context)
    );
    assert_eq!(loaded.export().canonical_value().unwrap(), before);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn frozen_concern_json_upgrade_preserves_editions_and_original_retry_proofs() {
    let store = MemStore::new(2);
    let first = episode(
        "v3-original",
        "Original scene",
        3,
        OccurrenceSpan::Unknown,
        Some("old-label"),
    );
    let second = revision(&first, "v3-revision", "Revised scene", 4);
    append(&store, &first).await;
    revise(&store, &first, &second).await;
    let before = store.export().canonical_value().unwrap();
    let path = std::env::temp_dir().join(format!("mneme-v3-context-upgrade-{}.json", Ulid::new()));
    store.save_concern_v3(&path).unwrap();
    let source_bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&source_bytes).unwrap()["schema"],
        STORE_EXPORT_SCHEMA_V3
    );
    let refusal = match MemStore::load(&path) {
        Ok(_) => panic!("ordinary load admitted ConcernV1 predecessor"),
        Err(error) => error.to_string(),
    };
    assert!(refusal.contains("single-graph-upgrade --target-generation episode-context-v2"));
    let predecessor = MemStore::load_concern_v3(&path).unwrap();
    assert_eq!(predecessor.export().canonical_value().unwrap(), before);
    let target = path.with_extension("v4.json");
    predecessor.save(&target).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), source_bytes);
    assert!(MemStore::load_concern_v3(&target).is_err());
    let successor = MemStore::load(&target).unwrap();
    assert_eq!(successor.export().canonical_value().unwrap(), before);
    assert!(matches!(
        append(&successor, &first).await,
        EpisodeCommitOutcome::AlreadyApplied(_)
    ));
    let original = successor
        .lookup_episode(source(&first))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source(&original.node), source(&first));
    assert_eq!(original.node.body(), first.body());
    assert_eq!(original.current_edition_id, second.id());
    assert!(
        successor.export().nodes.iter().all(|node| node
            .episode()
            .unwrap()
            .occurrence_contexts()
            .is_none())
    );
    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(target).unwrap();
}

#[tokio::test]
async fn every_historical_json_boundary_refuses_context_presence_and_new_codec() {
    let store = MemStore::new(2);
    let old = episode(
        "old-marker-smuggling",
        "Old scene",
        1,
        OccurrenceSpan::Unknown,
        None,
    );
    append(&store, &old).await;
    let clean = serde_json::to_value(StoreExportEnvelopeV3::new(store.export())).unwrap();
    let path = std::env::temp_dir().join(format!("mneme-context-smuggling-{}.json", Ulid::new()));
    for field in [
        serde_json::Value::Null,
        serde_json::json!([]),
        serde_json::to_value(contexts("room")).unwrap(),
    ] {
        let mut forged = clean.clone();
        forged["store"]["nodes"][0]["memory_kind"]["episode"]["occurrence_contexts"] = field;
        std::fs::write(&path, serde_json::to_vec(&forged).unwrap()).unwrap();
        assert!(MemStore::load_concern_v3(&path).is_err());
        assert!(serde_json::from_value::<StoreExportEnvelopeV3>(forged.clone()).is_err());
        forged["schema"] = serde_json::json!(STORE_EXPORT_SCHEMA_V2);
        forged["store"].as_object_mut().unwrap().remove("concerns");
        std::fs::write(&path, serde_json::to_vec(&forged).unwrap()).unwrap();
        assert!(MemStore::load_single_graph_v2(&path).is_err());
        assert!(serde_json::from_value::<StoreExportEnvelopeV2>(forged.clone()).is_err());
        let mut legacy_node = forged["store"]["nodes"][0].clone();
        legacy_node["provenance"]["External"]["source"]
            .as_object_mut()
            .unwrap()
            .remove("request_codec");
        assert!(crate::decode_legacy_node_v1(&legacy_node.to_string()).is_err());
        let mut flat = forged["store"].clone();
        flat["nodes"][0] = legacy_node;
        let legacy: LegacyStoreExportV1 = serde_json::from_value(flat.clone()).unwrap();
        assert!(serde_json::to_value(legacy).is_err());
        std::fs::write(&path, serde_json::to_vec(&flat).unwrap()).unwrap();
        assert!(MemStore::load_legacy_v1(&path).is_err());
    }
    let mut forged = clean;
    forged["store"]["nodes"][0]["provenance"]["External"]["source"]["request_codec"] =
        serde_json::json!("episode_v2");
    let error = serde_json::from_value::<StoreExportEnvelopeV3>(forged.clone())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("predecessor node cannot carry episode_v2"),
        "{error}"
    );
    forged["schema"] = serde_json::json!(STORE_EXPORT_SCHEMA_V2);
    forged["store"].as_object_mut().unwrap().remove("concerns");
    let error = serde_json::from_value::<StoreExportEnvelopeV2>(forged)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("predecessor node cannot carry episode_v2"),
        "{error}"
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn historical_json_writers_refuse_context_without_replacing_destination() {
    let store = MemStore::new(2);
    let scene = contextual_episode("historical-writer-refusal", contexts("room"));
    append(&store, &scene).await;
    let path =
        std::env::temp_dir().join(format!("mneme-context-writer-refusal-{}.json", Ulid::new()));
    std::fs::write(&path, b"do not clobber").unwrap();
    assert!(store.save_concern_v3(&path).is_err());
    assert!(store.save_single_graph_v2(&path).is_err());
    assert!(serde_json::to_value(StoreExportEnvelopeV3::new(store.export())).is_err());
    assert!(serde_json::to_value(StoreExportEnvelopeV2::new(store.export())).is_err());
    assert!(
        StoreExportEnvelopeV3::new(store.export())
            .into_store()
            .is_err()
    );
    assert!(
        StoreExportEnvelopeV2::new(store.export())
            .into_store()
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"do not clobber");
    std::fs::remove_file(path).unwrap();
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
async fn linked_header_reads_exact_old_edition_and_classifies_without_mutation() {
    let store = MemStore::new(2);
    let old = episode(
        "linked-old",
        "Original account",
        10,
        OccurrenceSpan::Unknown,
        Some("pi"),
    );
    let corrected = revision(&old, "linked-corrected", "Corrected account", 20);
    let old = linked_recorded_in(old, "pi-recorder");
    let corrected = linked_recorded_in(corrected, "mac-recorder");
    append(&store, &old).await;
    revise(&store, &old, &corrected).await;
    let lesson = authored("linked-lesson", "Semantic lesson", 21);
    store.put_node(&lesson).await.unwrap();
    let before = store.export().canonical_value().unwrap();
    let request =
        |id| EpisodeHeaderByEditionRequest::new(id, std::time::Duration::from_secs(1)).unwrap();
    let EpisodeHeaderByEdition::Episode(header) = store
        .episode_header_by_edition(&request(old.id()))
        .await
        .unwrap()
    else {
        panic!("expected episode");
    };
    assert_eq!(header.identity.edition_id, old.id());
    assert_eq!(header.current_edition_id, corrected.id());
    assert_eq!(header.summary.as_str(), old.summary());
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
    assert_eq!(store.export().canonical_value().unwrap(), before);
}

#[tokio::test]
async fn linked_incident_mem_seeks_are_fair_bounded_raw_and_reloadable() {
    let store = MemStore::new(2);
    let anchor = NodeId(Ulid::from(1000u128));
    // Dangling endpoints are deliberately retained: this port never hydrates
    // nodes or hides semantic/status filtering behind its row limit.
    for i in 1..=80u128 {
        let endpoint = NodeId(Ulid::from(i));
        store
            .put_edge(&Edge::new(anchor, endpoint, 0.01, EdgeKind::Transition, 1))
            .await
            .unwrap();
        store
            .put_edge(&Edge::new(endpoint, anchor, 0.99, EdgeKind::DerivedFrom, 1))
            .await
            .unwrap();
    }
    let before = store.export().canonical_value().unwrap();
    let request = |after, rows| {
        IncidentEdgesRequest::new(anchor, rows, std::time::Duration::from_secs(1), after).unwrap()
    };
    let first = store
        .incident_edges_page(&request(None, 3))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.work.rows_scanned, 3);
    assert_eq!(first.work.indexed_seeks, 2);
    assert_eq!(first.work.edge_point_reads, 1);
    assert_eq!(first.work.body_anchor_point_reads, 0);
    assert_eq!(first.items.iter().filter(|e| e.from == anchor).count(), 2);
    let second = store
        .incident_edges_page(&request(first.next.clone(), 3))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.items.iter().filter(|e| e.to == anchor).count(), 2);
    assert_eq!(second.work.edge_point_reads, 2);
    assert_eq!(second.work.rows_scanned, 3);
    // Every page is bounded, including the empty seeks proving exact tail
    // exhaustion; no uncharged lookahead at a full page boundary.
    let mut after = None;
    let mut found = BTreeSet::new();
    let mut raw_rows = 0;
    loop {
        let page = store
            .incident_edges_page(&request(after, 7))
            .await
            .unwrap()
            .unwrap();
        assert!(page.work.rows_scanned <= 7);
        assert_eq!(page.work.rows_scanned, page.items.len());
        assert!(page.work.indexed_seeks <= 3);
        raw_rows += page.work.rows_scanned;
        for edge in page.items {
            assert!(found.insert((edge.from, edge.to)));
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(raw_rows, 160);
    assert_eq!(found.len(), 160);
    assert_eq!(store.export().canonical_value().unwrap(), before);
    let imported = MemStore::from_export(store.export()).unwrap();
    assert_eq!(imported.lock().edge_keys_by_to.len(), 160);
    let imported_first = imported
        .incident_edges_page(&request(None, 3))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(imported_first.items).unwrap(),
        serde_json::to_value(first.items).unwrap()
    );
    let path = std::env::temp_dir().join(format!("mneme-linked-read-{}.json", Ulid::new()));
    store.save(&path).unwrap();
    let loaded = MemStore::load(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(loaded.lock().edge_keys_by_to.len(), 160);
    let edge = Edge::new(
        NodeId(Ulid::from(1u128)),
        anchor,
        0.5,
        EdgeKind::Associative,
        2,
    );
    loaded.put_edge(&edge).await.unwrap(); // Replacement must not duplicate keys.
    assert_eq!(loaded.lock().edge_keys_by_to.len(), 160);
    loaded.delete_edge(edge.from, edge.to).await.unwrap();
    assert_eq!(loaded.lock().edge_keys_by_to.len(), 159);
    assert!(!loaded.lock().edge_keys_by_to.contains(&(anchor, edge.from)));
    loaded.put_edge(&edge).await.unwrap();
    assert_eq!(loaded.lock().edge_keys_by_to.len(), 160);
}

#[tokio::test]
async fn linked_incident_mem_self_edge_is_charged_twice_and_tails_are_honest() {
    let store = MemStore::new(2);
    let anchor = NodeId(Ulid::from(1u128));
    store
        .put_edge(&Edge::new(anchor, anchor, 0.5, EdgeKind::Associative, 1))
        .await
        .unwrap();
    let request = |after| {
        IncidentEdgesRequest::new(anchor, 1, std::time::Duration::from_secs(1), after).unwrap()
    };
    let first = store
        .incident_edges_page(&request(None))
        .await
        .unwrap()
        .unwrap();
    let second = store
        .incident_edges_page(&request(first.next))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(second.items.len(), 1);
    assert_eq!(first.work.rows_scanned + second.work.rows_scanned, 2);
    assert_eq!(second.work.edge_point_reads, 1);
    let third = store
        .incident_edges_page(&request(second.next))
        .await
        .unwrap()
        .unwrap();
    assert!(third.items.is_empty());
    assert_eq!(third.work.indexed_seeks, 2);
    assert_eq!(third.work.rows_scanned, 0);
    assert!(third.next.is_none());
}

#[tokio::test]
async fn linked_mem_deadline_includes_lock_wait() {
    let store = MemStore::new(2);
    let guard = store.lock();
    let request = EpisodeHeaderByEditionRequest::new(
        NodeId(Ulid::from(1u128)),
        std::time::Duration::from_millis(2),
    )
    .unwrap();
    assert!(
        matches!(store.episode_header_by_edition(&request).await, Err(Error::Backend(message)) if message.contains("deadline"))
    );
    drop(guard);
}
