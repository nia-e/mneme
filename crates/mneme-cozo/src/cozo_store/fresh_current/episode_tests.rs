// Kept inside `fresh_current::tests` to share the canonical-component fixture.

#[test]
fn single_graph_policy_binding_does_not_reuse_predecessors() {
    let binding = fresh_current_policy_binding(4, Ulid::from(0x1234_u128));
    let hex = binding
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_ne!(
        hex,
        "d02a0fd4431255f7d72bd309e155d134d6dab362da21c82faaae909f5ae7a5c3"
    );
    assert_ne!(
        hex,
        "5d02615027ed3f659de615dbca236de39d7b7b98bc514a3d846699285cfb1830"
    );
}

#[tokio::test]
async fn episodic_materialization_preserves_nonempty_canonical_export_and_capture_source() {
    let fixture = Fixture::new();
    let source = exact_component_source().await;
    let (external, external_id) = external_source().await;
    let external_node = external.get_node(external_id).await.unwrap().unwrap();
    source.put_node(&external_node).await.unwrap();
    source
        .upsert(external_id, &[0.0, 1.0, 0.0, 0.0])
        .await
        .unwrap();
    let expected = source.export();

    CozoStore::materialize_fresh_current(&fixture.target, Ulid::new(), &source)
        .await
        .expect("materialize nonempty source into explicit episodic target");
    require_no_sqlite_sidecars(&fixture.target, "episodic post-return")
        .expect("episodic materializer returned sidecar-free");
    require_raw_closed_generation(&fixture.target, 4)
        .expect("new file has exact closed episodic catalog");

    let lease = StoreLease::acquire(&fixture.target).expect("reacquire published episode lease");
    let reopened = CozoStore::open_persistent(&fixture.target, 4, Arc::new(lease))
        .expect("normal admission recognizes episodic generation");
    assert_eq!(reopened.db_id(), expected.db_id);
    reopened
        .verify_import(&expected)
        .await
        .expect("verify every canonical component after reopen");
    assert_eq!(
        serde_json::to_value(reopened.get_node(external_id).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(external_node).unwrap(),
    );
    let actual = reopened.export().await.unwrap();
    assert!(
        actual.feedback_retries.is_empty(),
        "volatile retry authority must not migrate"
    );
    assert_eq!(actual.embedding_fingerprint, expected.embedding_fingerprint);
    reopened.prepare_for_file_move().unwrap();
    drop(reopened);
    require_no_sqlite_sidecars(&fixture.target, "episodic reopened close").unwrap();
}

#[tokio::test]
async fn episodic_materialization_empty_init_is_no_clobber() {
    let fixture = Fixture::new();
    let source = crate::MemStore::new(4);
    CozoStore::materialize_fresh_current(&fixture.target, Ulid::new(), &source)
        .await
        .expect("fresh episodic initialization is the empty-export case");
    let published_bytes = fs::read(&fixture.target).unwrap();
    let error = CozoStore::materialize_fresh_current(&fixture.target, Ulid::new(), &source)
        .await
        .expect_err("episode construction cannot replace an existing target");
    assert_eq!(
        error.target_publication_state(),
        FreshCurrentTargetPublicationStateV1::NotPublished
    );
    assert_eq!(fs::read(&fixture.target).unwrap(), published_bytes);
}

#[tokio::test]
async fn episodic_postflight_failure_keeps_published_target_and_lease() {
    let fixture = Fixture::new();
    let source = crate::MemStore::new(4);
    let error = materialize_fresh_generation_with_observers(
        &fixture.target,
        Ulid::new(),
        &source,
        CatalogContract::TouchstonesV1, // Explicit current typed-annotation successor.
        |target| assert!(!target.exists()),
        |_, current| {
            current
                .persistent_authority
                .as_ref()
                .unwrap()
                .require_live_guards("episodic postflight test")?;
            Err(Error::Backend(
                "injected episodic postflight failure".into(),
            ))
        },
    )
    .await
    .expect_err("post-boundary failure must retain recovery authority");
    assert_eq!(
        error.target_publication_state(),
        FreshCurrentTargetPublicationStateV1::Published
    );
    assert_eq!(
        error.failure_phase(),
        FreshCurrentMaterializationFailurePhaseV1::PublishedPostflight
    );
    assert!(error.retains_authority());
    assert!(fixture.target.is_file());
    assert_eq!(
        StoreLease::acquire(&fixture.target).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        error.relinquish_for_unpublished_outer_discard(),
        FreshCurrentTargetPublicationStateV1::Published
    );
    drop(StoreLease::acquire(&fixture.target).expect("explicit relinquishment releases authority"));
}

#[tokio::test]
async fn episodic_invalid_operation_refuses_without_target_or_lease_artifact() {
    let fixture = Fixture::new();
    let source = crate::MemStore::new(4);
    let lock = mneme_store_path::store_lock_path(&fixture.target).unwrap();
    let error = CozoStore::materialize_fresh_current(&fixture.target, Ulid::nil(), &source)
        .await
        .expect_err("nil operation must fail before acquiring target authority");
    assert_eq!(
        error.target_publication_state(),
        FreshCurrentTargetPublicationStateV1::NotPublished
    );
    assert_eq!(
        error.failure_phase(),
        FreshCurrentMaterializationFailurePhaseV1::PrePublication
    );
    assert!(!error.retains_authority());
    assert!(!fixture.target.exists());
    assert!(!lock.exists());
}

#[tokio::test]
async fn episodic_target_appearing_after_verification_is_not_clobbered() {
    let fixture = Fixture::new();
    let source = crate::MemStore::new(4);
    let error = materialize_fresh_generation_with_observers(
        &fixture.target,
        Ulid::new(),
        &source,
        CatalogContract::TouchstonesV1, // Explicit current typed-annotation successor.
        |target| fs::write(target, b"another owner arrived").unwrap(),
        |_, _| panic!("conflicting target must never open as the published episode store"),
    )
    .await
    .expect_err("target admission must still hold at publication");
    assert_eq!(
        error.target_publication_state(),
        FreshCurrentTargetPublicationStateV1::NotPublished
    );
    assert_eq!(fs::read(&fixture.target).unwrap(), b"another owner arrived");
}

fn materialization_episode_node(key: &str, summary: &str, created: u128) -> Node {
    let source = CaptureSource::new_with_codec(
        "episode-materialization",
        key,
        "file:///synthetic/episode-materialization",
        None::<&str>,
        None::<&str>,
        [17; 32],
        mneme_core::CaptureRequestCodec::EpisodeV1,
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

async fn materialization_episode_source() -> (crate::MemStore, Node, Node) {
    use mneme_core::episode::{
        EpisodeCommit, EpisodeFacet, EpisodeRevisionReason, EpisodeStore, EpisodeThread,
        EpisodeTime, EpisodeWriteExpectation, OccurrenceSpan,
    };
    let source = exact_component_source().await;
    let root = materialization_episode_node("initial", "I thought the bridge was broken", 100);
    let facet = EpisodeFacet::initial(
        root.id(),
        OccurrenceSpan::Unknown,
        Some(EpisodeThread::new("field-notes").unwrap()),
        EpisodeTime::new(100).unwrap(),
    )
    .unwrap();
    let root = root.with_episode(facet).unwrap();
    let lesson = NodeId(Ulid::from(1_u128));
    source
        .commit_episode(EpisodeCommit {
            node: &root,
            embedding: &[1.0, 0.0, 0.0, 0.0],
            links: &[Edge::new(
                root.id(),
                lesson,
                0.7,
                EdgeKind::DerivedFrom,
                100,
            )],
            expectation: EpisodeWriteExpectation::NewRoot,
        })
        .await
        .expect("append initial immutable edition");
    let initial = root.episode().unwrap();
    let edition = materialization_episode_node("revision", "The connector was unplugged", 200)
        .with_episode(
            EpisodeFacet::revised(
                initial.root(),
                root.id(),
                initial.revision().next().unwrap(),
                OccurrenceSpan::Point {
                    at: EpisodeTime::new(50).unwrap(),
                },
                Some(EpisodeThread::new("field-notes").unwrap()),
                initial.recorded_at(),
                EpisodeRevisionReason::new("The later check corrected my first account").unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    source
        .commit_episode(EpisodeCommit {
            node: &edition,
            embedding: &[0.0, 1.0, 0.0, 0.0],
            links: &[Edge::new(
                edition.id(),
                lesson,
                0.9,
                EdgeKind::DerivedFrom,
                200,
            )],
            expectation: EpisodeWriteExpectation::CurrentEdition(root.id()),
        })
        .await
        .expect("append editorial revision without replacing the old edition");
    (source, root, edition)
}

#[tokio::test]
async fn episodic_materialization_preserves_editions_and_rebuilds_current_head() {
    use mneme_core::episode::{
        EpisodeGet, EpisodeHistoryRequest, EpisodeStore, EpisodeTimelineRequest,
    };
    let fixture = Fixture::new();
    let (source, root, edition) = materialization_episode_source().await;
    let expected = source.export();
    let episode_id = root.episode().unwrap().root();
    CozoStore::materialize_fresh_current(&fixture.target, Ulid::new(), &source)
        .await
        .expect("publish the complete canonical edition chain");
    let lease = StoreLease::acquire(&fixture.target).unwrap();
    let reopened = CozoStore::open_persistent(&fixture.target, 4, Arc::new(lease)).unwrap();
    reopened.verify_import(&expected).await.unwrap();
    assert_eq!(
        serde_json::to_value(reopened.get_node(root.id()).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(&root).unwrap(),
    );
    assert_eq!(
        serde_json::to_value(reopened.get_node(edition.id()).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(&edition).unwrap(),
    );
    let current = reopened
        .get_episode(&EpisodeGet {
            episode_id,
            edition_id: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&current.node).unwrap(),
        serde_json::to_value(&edition).unwrap()
    );
    assert_eq!(current.current_edition_id, edition.id());
    let original = reopened
        .get_episode(&EpisodeGet {
            episode_id,
            edition_id: Some(root.id()),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&original.node).unwrap(),
        serde_json::to_value(&root).unwrap()
    );
    assert_eq!(original.current_edition_id, edition.id());
    let history = reopened
        .episode_history(&EpisodeHistoryRequest {
            episode_id,
            limit: Default::default(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(history.items.len(), 2);
    assert_eq!(history.items[0].identity.edition_id, root.id());
    assert_eq!(history.items[1].identity.edition_id, edition.id());
    assert!(history.next.is_none());
    let timeline = reopened
        .episode_timeline(&EpisodeTimelineRequest::default())
        .await
        .unwrap();
    assert_eq!(timeline.items.len(), 1);
    assert_eq!(timeline.items[0].identity.edition_id, edition.id());
    // The authored links remain attached to exact old/new editions, not retargeted to the head.
    let actual = reopened.export().await.unwrap();
    assert!(actual.edges.iter().any(|edge| edge.from == root.id()));
    assert!(actual.edges.iter().any(|edge| edge.from == edition.id()));
    reopened.prepare_for_file_move().unwrap();
}

#[tokio::test]
async fn ordinary_materializer_preserves_episode_editions_in_successor() {
    let (source, root, edition) = materialization_episode_source().await;
    let fixture = Fixture::new();
    CozoStore::materialize_fresh_current(&fixture.target, Ulid::new(), &source)
        .await
        .unwrap();
    let lease = StoreLease::acquire(&fixture.target).unwrap();
    let reopened = CozoStore::open_persistent(&fixture.target, 4, Arc::new(lease)).unwrap();
    assert!(reopened.get_node(root.id()).await.unwrap().is_some());
    assert!(reopened.get_node(edition.id()).await.unwrap().is_some());
}

#[tokio::test]
async fn context_upgrade_preserves_old_proofs_then_persists_contextual_revision() {
    use mneme_core::episode::{
        EpisodeCommit, EpisodeFacet, EpisodeGet, EpisodeRevisionReason, EpisodeStore,
        EpisodeWriteExpectation, OccurrenceContextRef, OccurrenceContexts,
    };
    let predecessor_fixture = Fixture::new();
    let fixture = Fixture::new();
    let (source, root, edition) = materialization_episode_source().await;
    let old_root = serde_json::to_value(&root).unwrap();
    let old_edition = serde_json::to_value(&edition).unwrap();
    CozoStore::materialize_concern(&predecessor_fixture.target, Ulid::new(), &source)
        .await
        .unwrap();
    let lease = StoreLease::acquire(&predecessor_fixture.target).unwrap();
    let source_bytes = fs::read(&predecessor_fixture.target).unwrap();
    assert!(CozoStore::require_existing_current(&predecessor_fixture.target, &lease).is_err());
    let export = CozoStore::export_episode_context_predecessor(&predecessor_fixture.target, &lease)
        .await
        .unwrap();
    assert_eq!(fs::read(&predecessor_fixture.target).unwrap(), source_bytes);
    let mut expected_predecessor = source.export();
    expected_predecessor.feedback_retries.clear(); // Host-scoped retry authority is not migratable.
    assert_eq!(
        export.clone().canonical_value().unwrap(),
        expected_predecessor.canonical_value().unwrap()
    );
    drop(lease);
    let migrated = crate::MemStore::from_export(export).unwrap();
    CozoStore::materialize_fresh_current(&fixture.target, Ulid::new(), &migrated)
        .await
        .unwrap();
    let lease = Arc::new(StoreLease::acquire(&fixture.target).unwrap());
    let current = CozoStore::open_leased_current(&fixture.target, lease).unwrap();
    let prior = edition.episode().unwrap();
    let contexts = OccurrenceContexts::new(vec![
        OccurrenceContextRef::new("session", "pi-17", Some("Pi field session")).unwrap(),
        OccurrenceContextRef::new("workspace", "/work/bridge", None).unwrap(),
    ])
    .unwrap();
    let source = CaptureSource::new_with_codec(
        "episode-materialization",
        "context-revision",
        "file:///synthetic/context-revision",
        Some("mac-recorder"),
        None::<&str>,
        [23; 32],
        mneme_core::CaptureRequestCodec::EpisodeV2,
    )
    .unwrap();
    let contextual = Node::try_new(
        source.node_id(),
        "The bridge was intact; we were in the Pi session",
        BodyRef::new("inline://context-revision").unwrap(),
        ["scene"],
        Provenance::External { source },
        0.5,
        0.5,
        NodeStatus::Active,
        300,
    )
    .unwrap()
    .with_episode(
        EpisodeFacet::revised(
            prior.root(),
            edition.id(),
            prior.revision().next().unwrap(),
            prior.occurrence().clone(),
            prior.thread().cloned(),
            prior.recorded_at(),
            EpisodeRevisionReason::new("Identified original work context").unwrap(),
        )
        .unwrap()
        .with_occurrence_contexts(contexts.clone()),
    )
    .unwrap();
    let request = || EpisodeCommit {
        node: &contextual,
        embedding: &[1., 0., 0., 0.],
        links: &[],
        expectation: EpisodeWriteExpectation::CurrentEdition(edition.id()),
    };
    current.commit_episode(request()).await.unwrap();
    assert!(matches!(
        current.commit_episode(request()).await.unwrap(),
        mneme_core::episode::EpisodeCommitOutcome::AlreadyApplied(_)
    ));
    current.prepare_for_file_move().unwrap();
    drop(current);
    let current = CozoStore::open_leased_current(
        &fixture.target,
        Arc::new(StoreLease::acquire(&fixture.target).unwrap()),
    )
    .unwrap();
    for (id, expected) in [(root.id(), old_root), (edition.id(), old_edition)] {
        let record = current
            .get_episode(&EpisodeGet {
                episode_id: prior.root(),
                edition_id: Some(id),
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(record.node).unwrap(),
            expected,
            "old edition and source proof remain immutable"
        );
    }
    let head = current
        .get_episode(&EpisodeGet {
            episode_id: prior.root(),
            edition_id: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        head.node.episode().unwrap().occurrence_contexts(),
        Some(&contexts)
    );
    assert_eq!(
        head.node.episode().unwrap().recorded_at(),
        prior.recorded_at()
    );
    let contextual_snapshot =
        crate::MemStore::from_export(current.export().await.unwrap()).unwrap();
    current.prepare_for_file_move().unwrap();
    drop(current);
    for historical in [CatalogContract::SingleGraphV1, CatalogContract::ConcernV1] {
        let target = Fixture::new();
        let refused = materialize_fresh_generation_with_observers(
            &target.target,
            Ulid::new(),
            &contextual_snapshot,
            historical,
            |_| {},
            |_, _| Ok(()),
        )
        .await
        .unwrap_err();
        assert_eq!(
            refused.target_publication_state(),
            FreshCurrentTargetPublicationStateV1::NotPublished
        );
        assert!(!target.target.exists());
        assert!(
            !mneme_store_path::store_lock_path(&target.target)
                .unwrap()
                .exists()
        );
    }
}

#[tokio::test]
async fn binding_codec_upgrade_keeps_learned_edges_and_historical_concern_evidence_without_rebinding()
 {
    use mneme_core::concern::*;
    use mneme_core::ports::routing_content_fingerprint;
    let predecessor = Fixture::new();
    let target = Fixture::new();
    let (source, original_episode, revision_episode) = materialization_episode_source().await;
    let a = crate::concern_tests::node(101, "bounded left claim");
    let b = crate::concern_tests::node(102, "bounded right claim");
    source.put_node(&a).await.unwrap();
    source.put_node(&b).await.unwrap();
    let witness_bytes = b"Historical witness: the two claims were inspected in deployment A.\n";
    let witness = Node::try_new(
        NodeId(Ulid::from(103_u128)),
        "A stored witness is evidence, not a freshly applicable routing opinion",
        BodyRef::new("fs://witness").unwrap(),
        ["witness"],
        Provenance::derived_empty(),
        0.5,
        0.5,
        NodeStatus::Active,
        3,
    )
    .unwrap();
    source.put_node(&witness).await.unwrap();
    let source_bodies = predecessor.target.with_extension("bodies");
    fs::create_dir(&source_bodies).unwrap();
    fs::write(source_bodies.join("witness"), witness_bytes).unwrap();
    let learned = Edge::from_stored(
        a.id(),
        b.id(),
        EdgeKind::Associative,
        Some(BodySpan::new(2, 8)),
        0.3125,
        123,
        43,
        7,
    );
    learned.validate().unwrap();
    source.put_edge(&learned).await.unwrap();
    // Frozen v1 literals for these exact semantic nodes, from the pre-context
    // length-framed Debug projection. No legacy encoder remains executable.
    let old_binding = ConcernBinding::new(
        ConcernKind::Disagreement,
        ConcernEndpoint::new(
            a.id(),
            ConcernDigest::from_hex(
                "b069ca90a1ca30642c5d125afc440d15e6863ea5d33ee27dbc77c6652eb757d3",
            )
            .unwrap(),
        ),
        ConcernEndpoint::new(
            b.id(),
            ConcernDigest::from_hex(
                "8b9ba55d40a8adcf76afd15959618f3232e35b423be439d8077cebffd44fe106",
            )
            .unwrap(),
        ),
    )
    .unwrap();
    assert_ne!(
        old_binding.endpoints()[0].meaning(),
        ConcernDigest::from_hex(&routing_content_fingerprint(&a)).unwrap()
    );
    assert_ne!(
        old_binding.endpoints()[1].meaning(),
        ConcernDigest::from_hex(&routing_content_fingerprint(&b)).unwrap()
    );
    let notice = ConcernNotice::new(
        old_binding,
        "The old inspection found differing assertions",
        "Which deployment was inspected?",
    )
    .unwrap();
    let historical_finding = ScopedConcernFinding::new(
        "deployment A",
        "The witness records only that earlier inspection",
        vec![ConcernEvidence::new("fs://witness", ConcernDigest::of_bytes(witness_bytes)).unwrap()],
    )
    .unwrap();
    let mut row_value = serde_json::to_value(ConcernRow::from_notice(notice)).unwrap();
    row_value["finding"] = serde_json::to_value(&historical_finding).unwrap();
    let row: ConcernRow = serde_json::from_value(row_value).unwrap();
    let row_bytes = serde_json::to_string(&row).unwrap();
    let mut expected = source.export();
    expected.concerns = vec![row.clone()];
    let historical = crate::MemStore::from_export(expected.clone()).unwrap();
    CozoStore::materialize_concern(&predecessor.target, Ulid::new(), &historical)
        .await
        .unwrap();
    let lease = StoreLease::acquire(&predecessor.target).unwrap();
    let source_bytes = fs::read(&predecessor.target).unwrap();
    let exported = CozoStore::export_episode_context_predecessor(&predecessor.target, &lease)
        .await
        .unwrap();
    assert_eq!(fs::read(&predecessor.target).unwrap(), source_bytes);
    assert_eq!(exported.concerns, vec![row.clone()]);
    let detached = crate::MemStore::from_export(exported).unwrap();
    CozoStore::materialize_fresh_current(&target.target, Ulid::new(), &detached)
        .await
        .unwrap();
    let current = CozoStore::open_leased_current(
        &target.target,
        Arc::new(StoreLease::acquire(&target.target).unwrap()),
    )
    .unwrap();
    current.verify_import(&expected).await.unwrap();
    let actual = current.export().await.unwrap();
    let copied_edge = actual
        .edges
        .iter()
        .find(|edge| edge.from == a.id() && edge.to == b.id())
        .unwrap();
    assert_eq!(copied_edge.weight().to_bits(), learned.weight().to_bits());
    assert_eq!(copied_edge.last_reinforced(), 123);
    assert_eq!((copied_edge.trials(), copied_edge.interference()), (43, 7));
    assert_eq!(
        serde_json::to_value(copied_edge).unwrap(),
        serde_json::to_value(&learned).unwrap()
    );
    assert_eq!(actual.concerns, vec![row.clone()]);
    let persisted = current
        .run(
            "?[data] := *concern{data}",
            std::collections::BTreeMap::new(),
            false,
        )
        .unwrap();
    assert_eq!(
        super::super::want_str(&persisted.rows[0][0]).unwrap(),
        row_bytes
    );
    for edition in [&original_episode, &revision_episode] {
        assert_eq!(
            serde_json::to_value(current.get_node(edition.id()).await.unwrap().unwrap()).unwrap(),
            serde_json::to_value(edition).unwrap(),
            "episode/source proofs are evidence, not binding hashes to rewrite"
        );
    }
    assert_eq!(
        serde_json::to_value(current.get_node(witness.id()).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(&witness).unwrap()
    );
    assert_eq!(
        fs::read(source_bodies.join("witness")).unwrap(),
        witness_bytes
    );
    let stale = ConcernUpdate::RecordScopedFinding {
        expected: row.clone(),
        finding: historical_finding,
    };
    assert!(
        matches!(current.update_concern(&stale).await.unwrap(), ConcernCommitOutcome::Refused { reason: ConcernRefusal::StaleMeanings, row: Some(stored) } if stored == row)
    );
    assert_eq!(
        current.get_concern(&row.binding().key()).await.unwrap(),
        Some(row.clone())
    );
    let fresh = crate::concern_tests::resulting(
        current
            .update_concern(&crate::concern_tests::notice(
                &a,
                &b,
                ConcernKind::Disagreement,
            ))
            .await
            .unwrap(),
    );
    assert_ne!(fresh.binding(), old_binding);
    assert!(
        fresh.finding().is_none(),
        "a fresh notice may replace the latest cache; it is not an indefinite history"
    );
    assert_eq!(fs::read(&predecessor.target).unwrap(), source_bytes);
    assert_eq!(
        fs::read(source_bodies.join("witness")).unwrap(),
        witness_bytes
    );
    assert_eq!(
        serde_json::to_value(current.get_node(witness.id()).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(witness).unwrap()
    );
    current.prepare_for_file_move().unwrap();
}
