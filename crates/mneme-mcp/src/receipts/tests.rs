use super::*;
use std::collections::BTreeMap;

use mneme_core::ports::FeedbackCommitOutcome;
use mneme_core::{EdgeKind, Provenance};
use mneme_engine::Ingest;

use crate::{
    ColdWorkGate, DatabaseControlRequest, DatabaseSlot, MAX_SESSIONS, MAX_WALK_ACTIONS,
    MAX_WALK_PATH_DEPTH, Registry, SESSION_TTL, WalkRequest, admit_walk_action,
    database_control_tool, ensure_session_capacity, ensure_walk_path_capacity, host, walk_tool,
};

#[cfg(unix)]
#[tokio::test]
async fn maintenance_release_requires_session_quiescence_and_rotates_only_its_epoch() {
    let root = std::env::temp_dir().join(format!("mneme-mcp-control-{}", ulid::Ulid::new()));
    let path = root.join("memory.db");
    let mut initial_sessions = SessionState::default();
    let initial_epoch = initial_sessions.feedback_epoch("fixture");
    let registry = Registry {
        activity: crate::activity::ActivityRing::default(),
        dbs: BTreeMap::from([(
            "fixture".into(),
            std::sync::Arc::new(
                DatabaseSlot::open(
                    path,
                    std::sync::Arc::new(host::InferenceRuntime::new()),
                    initial_epoch,
                )
                .unwrap(),
            ),
        )]),
    };
    let sessions = std::sync::Arc::new(Mutex::new(initial_sessions));
    let cold_work = ColdWorkGate::new(Duration::ZERO);
    let (old_epoch, other_epoch) = {
        let mut state = sessions.lock().await;
        state.next_feedback_sequence.insert("fixture".into(), 41);
        let old = state.feedback_epoch("fixture");
        let other = state.feedback_epoch("other");
        state.active.insert(
            "walk".into(),
            McpSession {
                db: "fixture".into(),
                session: WalkSession::new(NodeId(ulid::Ulid::new()), 2),
                actions: 1,
                touched: Instant::now(),
            },
        );
        (old, other)
    };

    let walk_error = database_control_tool(
        &registry,
        &sessions,
        &cold_work,
        DatabaseControlRequest::Release,
        &json!({ "db": "fixture", "action": "release" }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(walk_error.contains("1 active walk"), "{walk_error}");

    {
        let mut state = sessions.lock().await;
        state.active.clear();
        let mut receipt = receipt_touched(false, 0);
        receipt.db = "fixture".into();
        state.receipts.insert("receipt".into(), receipt);
    }
    let receipt_error = database_control_tool(
        &registry,
        &sessions,
        &cold_work,
        DatabaseControlRequest::Release,
        &json!({ "db": "fixture", "action": "release" }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        receipt_error.contains("1 unconsumed or claimed receipt"),
        "{receipt_error}"
    );

    {
        let mut state = sessions.lock().await;
        let receipt = state.receipts.get_mut("receipt").unwrap();
        receipt.consumed = true;
        receipt.claim = ClaimState::Claimed(ClaimLease {
            id: "still-running".into(),
            started: Instant::now(),
        });
    }
    let claimed_error = database_control_tool(
        &registry,
        &sessions,
        &cold_work,
        DatabaseControlRequest::Release,
        &json!({ "db": "fixture", "action": "release" }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        claimed_error.contains("1 unconsumed or claimed receipt"),
        "{claimed_error}"
    );

    {
        let mut state = sessions.lock().await;
        let receipt = state.receipts.get_mut("receipt").unwrap();
        receipt.claim = ClaimState::Available;
        let mut other = receipt_touched(false, 0);
        other.db = "other".into();
        other.consumed = true;
        state.receipts.insert("other-tombstone".into(), other);
    }
    let released = database_control_tool(
        &registry,
        &sessions,
        &cold_work,
        DatabaseControlRequest::Release,
        &json!({ "db": "fixture", "action": "release" }),
    )
    .await
    .unwrap();
    assert_eq!(released["state"], "maintenance");
    assert_eq!(released["purged_retry_tombstones"], 1);
    assert!(registry.checkout("fixture").is_err());
    {
        let state = sessions.lock().await;
        assert_ne!(state.feedback_epochs["fixture"], old_epoch);
        assert_eq!(state.feedback_epochs["other"], other_epoch);
        assert!(!state.next_feedback_sequence.contains_key("fixture"));
        assert!(!state.receipts.contains_key("receipt"));
        assert!(state.receipts.contains_key("other-tombstone"));
    }

    let resumed = database_control_tool(
        &registry,
        &sessions,
        &cold_work,
        DatabaseControlRequest::Resume,
        &json!({ "db": "fixture", "action": "resume" }),
    )
    .await
    .unwrap();
    assert_eq!(resumed["state"], "open");
    assert!(registry.checkout("fixture").is_ok());

    drop(registry);
    std::fs::remove_dir_all(root).unwrap();
}

fn session_touched(secs_ago: u64) -> McpSession {
    McpSession {
        db: "d".into(),
        session: WalkSession::new(NodeId(ulid::Ulid::new()), 5),
        actions: 1,
        touched: Instant::now() - Duration::from_secs(secs_ago),
    }
}

fn receipt_touched(claimed: bool, secs_ago: u64) -> WalkReceipt {
    let node = NodeId(ulid::Ulid::new());
    WalkReceipt {
        db: "d".into(),
        routes: Vec::new(),
        visited: HashSet::from([node]),
        issued: Instant::now() - Duration::from_secs(secs_ago),
        issued_wall: SystemTime::now()
            .checked_sub(Duration::from_secs(secs_ago))
            .unwrap_or(SystemTime::UNIX_EPOCH),
        consumed: false,
        bound_batch: None,
        claim: if claimed {
            ClaimState::Claimed(ClaimLease {
                id: "claim".into(),
                started: Instant::now(),
            })
        } else {
            ClaimState::Available
        },
    }
}

fn state_with_receipts(receipts: HashMap<String, WalkReceipt>) -> SessionState {
    SessionState {
        receipts,
        ..SessionState::default()
    }
}

fn bind_singleton(receipt: &mut WalkReceipt, token: &str, sequence: u64) {
    receipt.bound_batch = Some(ReceiptBatchBinding {
        key: receipt_feedback_key(&receipt.db, &[token.to_owned()]),
        reflect_fingerprint: receipt_reflect_fingerprint(
            &receipt.routes,
            &HashSet::new(),
            &HashSet::new(),
        ),
        sequence,
    });
}

fn payload_with_nodes(count: usize) -> (ReceiptPayload, Vec<NodeId>) {
    let nodes: Vec<_> = (0..count).map(|_| NodeId(ulid::Ulid::new())).collect();
    let routes = nodes
        .windows(2)
        .map(|pair| ObservedRoute::new(pair[0], pair[1], pair[0], pair[1]).unwrap())
        .collect();
    let visited = nodes.iter().copied().collect();
    (ReceiptPayload { routes, visited }, nodes)
}

async fn walk_fixture() -> (mneme_engine::Memory, NodeId, NodeId) {
    use std::sync::Arc;

    use mneme_core::ports::{Clock, GraphStore, SystemClock, Traversal, VectorIndex};

    let dim = mneme_embed::DEFAULT_DIM;
    let store = Arc::new(mneme_cozo::MemStore::new(dim));
    let graph: Arc<dyn GraphStore> = store.clone();
    let vectors: Arc<dyn VectorIndex> = store.clone();
    let traversal: Arc<dyn Traversal> = store;
    let embedder = Arc::new(mneme_embed::HashingEmbedder::new(dim));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let config = mneme_engine::Config {
        min_similarity_links: 0,
        ..mneme_engine::Config::default()
    };
    let mem = mneme_engine::Memory::new(graph, vectors, traversal, embedder, clock, config)
        .with_body_store(Arc::new(mneme_body::InlineStore::new()));
    let provenance = Provenance::derived_empty;
    let first = mem
        .ingest(Ingest::new("first", b"", &[], provenance()))
        .await
        .unwrap();
    let second = mem
        .ingest(Ingest::new("second", b"", &[], provenance()))
        .await
        .unwrap();
    mem.link(first, second, EdgeKind::Associative, 0.5, None)
        .await
        .unwrap();
    (mem, first, second)
}

#[test]
fn session_admission_purges_expired_entries() {
    let mut sessions = HashMap::new();
    sessions.insert("fresh".into(), session_touched(1));
    sessions.insert("stale".into(), session_touched(SESSION_TTL.as_secs() + 1));
    ensure_session_capacity(&mut sessions).unwrap();
    assert!(sessions.contains_key("fresh"));
    assert!(!sessions.contains_key("stale"), "idle-past-TTL was purged");
}

#[test]
fn full_live_session_pool_rejects_without_eviction() {
    let mut sessions = HashMap::new();
    for i in 0..MAX_SESSIONS {
        sessions.insert(format!("s{i}"), session_touched((MAX_SESSIONS - i) as u64));
    }

    assert!(ensure_session_capacity(&mut sessions).is_err());
    assert_eq!(sessions.len(), MAX_SESSIONS);
    assert!(
        sessions.contains_key("s0"),
        "oldest live walk was preserved"
    );

    sessions.insert("s0".into(), session_touched(SESSION_TTL.as_secs() + 1));
    ensure_session_capacity(&mut sessions).unwrap();
    assert_eq!(sessions.len(), MAX_SESSIONS - 1);
}

#[test]
fn walk_action_limit_is_exact() {
    let mut session = session_touched(0);
    session.actions = MAX_WALK_ACTIONS - 1;

    admit_walk_action(&mut session).unwrap();
    assert_eq!(session.actions, MAX_WALK_ACTIONS);
    assert!(admit_walk_action(&mut session).is_err());
    assert_eq!(session.actions, MAX_WALK_ACTIONS);
}

#[tokio::test]
async fn walk_path_depth_bounds_revisits_and_back_reopens_capacity() {
    let (mem, first, second) = walk_fixture().await;
    let mut session = WalkSession::new(first, MAX_WALK_BUDGET);

    for _ in 0..MAX_WALK_PATH_DEPTH {
        ensure_walk_path_capacity(&session).unwrap();
        let target = if session.current() == first {
            second
        } else {
            first
        };
        session.go(&target.0.to_string(), &mem).await.unwrap();
    }

    assert_eq!(session.depth(), MAX_WALK_PATH_DEPTH);
    assert_eq!(session.visited_count(), 2);
    assert_eq!(session.trail().len(), 2, "revisits do not grow receipts");
    assert!(ensure_walk_path_capacity(&session).is_err());

    session.back(&mem).await.unwrap();
    assert_eq!(session.depth(), MAX_WALK_PATH_DEPTH - 1);
    ensure_walk_path_capacity(&session).unwrap();
    let target = if session.current() == first {
        second
    } else {
        first
    };
    session.go(&target.0.to_string(), &mem).await.unwrap();
    assert_eq!(session.depth(), MAX_WALK_PATH_DEPTH);
}

#[test]
fn receipt_payload_limits_accept_the_exact_boundary() {
    let (at_limit, _) = payload_with_nodes(MAX_RECEIPT_VISITED);
    let payload = bounded_receipt_payload(at_limit.routes, at_limit.visited).unwrap();
    assert_eq!(payload.visited.len(), MAX_RECEIPT_VISITED);
    assert_eq!(payload.routes.len(), MAX_RECEIPT_REACHED);

    let (too_many_visited, _) = payload_with_nodes(MAX_RECEIPT_VISITED + 1);
    assert!(bounded_receipt_payload(too_many_visited.routes, too_many_visited.visited).is_err());

    let (mut too_many_routes, _) = payload_with_nodes(MAX_RECEIPT_VISITED);
    too_many_routes.routes.push(too_many_routes.routes[0]);
    assert!(bounded_receipt_payload(too_many_routes.routes, too_many_routes.visited).is_err());
}

#[test]
fn full_live_receipt_pool_rejects_without_eviction() {
    let mut receipts = HashMap::new();
    for i in 0..MAX_RECEIPTS {
        receipts.insert(format!("r{i}"), receipt_touched(false, 1));
    }

    assert!(ensure_receipt_capacity(&mut receipts).is_err());
    assert_eq!(receipts.len(), MAX_RECEIPTS);
    assert!(receipts.contains_key("r0"));

    receipts.get_mut("r0").unwrap().issued = Instant::now() - RECEIPT_TTL - Duration::from_secs(1);
    ensure_receipt_capacity(&mut receipts).unwrap();
    assert_eq!(receipts.len(), MAX_RECEIPTS - 1);
}

#[test]
fn claimed_receipts_cannot_be_purged_to_overfill_the_pool() {
    let mut receipts = HashMap::new();
    for i in 0..MAX_RECEIPTS {
        receipts.insert(
            format!("r{i}"),
            receipt_touched(true, RECEIPT_TTL.as_secs() + 1),
        );
    }

    assert!(ensure_receipt_capacity(&mut receipts).is_err());
    assert_eq!(receipts.len(), MAX_RECEIPTS);
    assert!(receipts.values().all(|receipt| receipt.is_claimed()));
}

#[test]
fn expired_receipt_claim_is_released_without_dropping_live_payload() {
    let mut receipt = receipt_touched(true, 1);
    receipt.claim = ClaimState::Claimed(ClaimLease {
        id: "claim".into(),
        started: Instant::now() - RECEIPT_CLAIM_TTL - Duration::from_secs(1),
    });
    let mut receipts = HashMap::from([("r".into(), receipt)]);

    purge_expired_unclaimed_receipts(&mut receipts);

    let receipt = &receipts["r"];
    assert!(!receipt.is_claimed());
    assert!(receipt.claim_id().is_none());
    assert!(matches!(receipt.claim, ClaimState::Available));
}

#[test]
fn successful_claim_becomes_retryable_tombstone_without_sliding_expiry() {
    let (payload, nodes) = payload_with_nodes(2);
    let issued = Instant::now() - RECEIPT_TTL - Duration::from_secs(1);
    let mut receipts = HashMap::from([(
        "r".into(),
        WalkReceipt {
            db: "d".into(),
            routes: payload.routes,
            visited: payload.visited,
            issued,
            issued_wall: SystemTime::now()
                .checked_sub(RECEIPT_TTL + Duration::from_secs(1))
                .unwrap_or(SystemTime::UNIX_EPOCH),
            consumed: false,
            bound_batch: None,
            claim: ClaimState::Available,
        },
    )]);
    let used = HashSet::from([nodes[1]]);

    // Directly claim the fixture to model a call that began just before its
    // fixed expiry, then mark the successful result consumed.
    let claim_id = "claim";
    let batch_key = receipt_feedback_key("d", &["r".into()]);
    {
        let receipt = receipts.get_mut("r").unwrap();
        receipt.bound_batch = Some(ReceiptBatchBinding {
            key: batch_key.clone(),
            reflect_fingerprint: receipt_reflect_fingerprint(
                &receipt.routes,
                &used,
                &HashSet::new(),
            ),
            sequence: 1,
        });
        receipt.claim = ClaimState::Claimed(ClaimLease {
            id: claim_id.into(),
            started: Instant::now(),
        });
    }
    release_walk_receipt_claims(&mut receipts, &["r".into()], claim_id, true);
    assert!(receipts["r"].consumed);
    assert_eq!(
        receipts["r"]
            .bound_batch
            .as_ref()
            .map(|binding| binding.key.as_str()),
        Some(batch_key.as_str()),
    );
    assert_eq!(receipts["r"].issued, issued);

    purge_expired_unclaimed_receipts(&mut receipts);
    assert!(
        receipts.is_empty(),
        "claim/release did not extend receipt TTL"
    );
}

#[test]
fn consumed_tombstone_is_claimable_for_exact_retry() {
    let (payload, nodes) = payload_with_nodes(2);
    let mut receipt = receipt_touched(false, 0);
    receipt.routes = payload.routes;
    receipt.visited = payload.visited;
    let mut state = state_with_receipts(HashMap::from([("r".into(), receipt)]));
    let used = HashSet::from([nodes[1]]);

    let first =
        claim_walk_receipts(&mut state, &["r".into()], "d", &used, &HashSet::new()).unwrap();
    release_walk_receipt_claims(
        &mut state.receipts,
        &first.claim.tokens,
        &first.claim.id,
        true,
    );
    let retry =
        claim_walk_receipts(&mut state, &["r".into()], "d", &used, &HashSet::new()).unwrap();
    assert_eq!(retry.claim.tokens, vec!["r"]);
    assert_eq!(retry.retry, first.retry);
    assert!(state.receipts["r"].is_claimed());
    assert!(state.receipts["r"].consumed);
}

#[test]
fn consumed_retry_capacity_rejects_before_claiming_new_payload() {
    let mut receipts = HashMap::new();
    for i in 0..MAX_CONSUMED_RECEIPTS {
        let token = format!("done{i}");
        let mut receipt = receipt_touched(false, 0);
        receipt.consumed = true;
        bind_singleton(&mut receipt, &token, i as u64 + 1);
        receipts.insert(token, receipt);
    }
    let live = receipt_touched(false, 0);
    let used = live.visited.clone();
    receipts.insert("live".into(), live);
    let mut state = state_with_receipts(receipts);

    assert!(
        claim_walk_receipts(&mut state, &["live".into()], "d", &used, &HashSet::new()).is_err()
    );
    assert!(!state.receipts["live"].is_claimed());
    assert!(!state.receipts["live"].consumed);
}

#[test]
fn concurrent_claims_reserve_tombstone_capacity_before_commit() {
    let mut receipts = HashMap::new();
    for i in 0..MAX_CONSUMED_RECEIPTS - 1 {
        let token = format!("done{i}");
        let mut receipt = receipt_touched(false, 0);
        receipt.consumed = true;
        bind_singleton(&mut receipt, &token, i as u64 + 1);
        receipts.insert(token, receipt);
    }
    receipts.insert("first".into(), receipt_touched(false, 0));
    receipts.insert("second".into(), receipt_touched(false, 0));
    let used = HashSet::new();
    let mut state = state_with_receipts(receipts);

    let first =
        claim_walk_receipts(&mut state, &["first".into()], "d", &used, &HashSet::new()).unwrap();
    assert!(state.receipts["first"].is_claimed());
    assert!(state.receipts["first"].bound_batch.is_some());
    assert!(
        claim_walk_receipts(&mut state, &["second".into()], "d", &used, &HashSet::new()).is_err()
    );
    assert!(!state.receipts["second"].is_claimed());
    assert!(state.receipts["second"].bound_batch.is_none());

    // A failed/cancelled first attempt keeps its reservation; otherwise a
    // later distinct claim could steal the same final tombstone slot.
    release_walk_receipt_claims(
        &mut state.receipts,
        &first.claim.tokens,
        &first.claim.id,
        false,
    );
    assert!(!state.receipts["first"].is_claimed());
    assert!(state.receipts["first"].bound_batch.is_some());
    assert!(
        claim_walk_receipts(&mut state, &["second".into()], "d", &used, &HashSet::new()).is_err()
    );
}

#[test]
fn first_claim_sequences_are_per_database_and_live_floor_survives_holes() {
    let mut old_a = receipt_touched(false, 0);
    old_a.db = "a".into();
    let mut hole_a = receipt_touched(false, 0);
    hole_a.db = "a".into();
    let mut late_a = receipt_touched(false, 0);
    late_a.db = "a".into();
    let mut first_b = receipt_touched(false, 0);
    first_b.db = "b".into();
    let mut state = state_with_receipts(HashMap::from([
        ("old-a".into(), old_a),
        ("hole-a".into(), hole_a),
        ("late-a".into(), late_a),
        ("first-b".into(), first_b),
    ]));
    let used = HashSet::new();

    let oldest =
        claim_walk_receipts(&mut state, &["old-a".into()], "a", &used, &HashSet::new()).unwrap();
    assert_eq!(
        (oldest.retry.sequence, oldest.retry.min_live_sequence),
        (1, 1)
    );
    release_walk_receipt_claims(
        &mut state.receipts,
        &oldest.claim.tokens,
        &oldest.claim.id,
        false,
    );

    let other_db =
        claim_walk_receipts(&mut state, &["first-b".into()], "b", &used, &HashSet::new()).unwrap();
    assert_eq!(
        (other_db.retry.sequence, other_db.retry.min_live_sequence),
        (1, 1),
        "database B has an independent sequence and floor"
    );
    release_walk_receipt_claims(
        &mut state.receipts,
        &other_db.claim.tokens,
        &other_db.claim.id,
        false,
    );

    let hole =
        claim_walk_receipts(&mut state, &["hole-a".into()], "a", &used, &HashSet::new()).unwrap();
    assert_eq!(hole.retry.sequence, 2);
    release_walk_receipt_claims(
        &mut state.receipts,
        &hole.claim.tokens,
        &hole.claim.id,
        false,
    );
    state.receipts.remove("hole-a"); // expired failed/no-op claim: sequence hole

    let late =
        claim_walk_receipts(&mut state, &["late-a".into()], "a", &used, &HashSet::new()).unwrap();
    assert_eq!((late.retry.sequence, late.retry.min_live_sequence), (3, 1));
    release_walk_receipt_claims(
        &mut state.receipts,
        &late.claim.tokens,
        &late.claim.id,
        false,
    );
    state.receipts.remove("old-a");
    let late_retry =
        claim_walk_receipts(&mut state, &["late-a".into()], "a", &used, &HashSet::new()).unwrap();
    assert_eq!(
        late_retry.retry.min_live_sequence, 3,
        "the floor advances across holes only after the oldest binding disappears"
    );
}

#[test]
fn server_restart_changes_epoch_and_resets_per_database_sequence() {
    let mut first =
        state_with_receipts(HashMap::from([("first".into(), receipt_touched(false, 0))]));
    let mut restarted = state_with_receipts(HashMap::from([(
        "second".into(),
        receipt_touched(false, 0),
    )]));
    let used = HashSet::new();
    let before =
        claim_walk_receipts(&mut first, &["first".into()], "d", &used, &HashSet::new()).unwrap();
    let after = claim_walk_receipts(
        &mut restarted,
        &["second".into()],
        "d",
        &used,
        &HashSet::new(),
    )
    .unwrap();
    assert_ne!(before.retry.epoch, after.retry.epoch);
    assert_eq!((before.retry.sequence, after.retry.sequence), (1, 1));
}

#[test]
fn receipt_feedback_key_is_order_independent_and_database_bound() {
    let a = vec!["a".to_string(), "bb".to_string()];
    let b = vec!["bb".to_string(), "a".to_string()];
    let mut b_sorted = b;
    b_sorted.sort_unstable();

    assert_eq!(
        receipt_feedback_key("d", &a),
        receipt_feedback_key("d", &b_sorted)
    );
    assert_ne!(
        receipt_feedback_key("d", &a),
        receipt_feedback_key("other", &a)
    );
    assert_ne!(
        receipt_feedback_key("d", &["a".into(), "bb".into()]),
        receipt_feedback_key("d", &["aa".into(), "b".into()]),
        "length prefixes make concatenation unambiguous"
    );
}

#[test]
fn reflect_binding_fingerprint_includes_stored_route_orientation() {
    let a = NodeId(ulid::Ulid::new());
    let b = NodeId(ulid::Ulid::new());
    let used = HashSet::from([b]);
    let outgoing = ObservedRoute::new(a, b, a, b).unwrap();
    let incoming = ObservedRoute::new(a, b, b, a).unwrap();

    assert_ne!(
        receipt_reflect_fingerprint(&[outgoing], &used, &HashSet::new()),
        receipt_reflect_fingerprint(&[incoming], &used, &HashSet::new()),
        "the same movement over the opposite stored arrow is a different payload"
    );
}

#[test]
fn used_id_seen_only_by_an_omitted_receipt_is_rejected() {
    let included = receipt_touched(false, 0);
    let omitted = receipt_touched(false, 0);
    let used = omitted.visited.clone();
    let mut state = state_with_receipts(HashMap::from([
        ("included".into(), included),
        ("omitted".into(), omitted),
    ]));

    assert!(
        claim_walk_receipts(
            &mut state,
            &["included".into()],
            "d",
            &used,
            &HashSet::new()
        )
        .is_err()
    );
    assert!(state.receipts.values().all(|receipt| {
        !receipt.is_claimed() && receipt.bound_batch.is_none() && !receipt.consumed
    }));
}

#[tokio::test]
async fn start_only_receipt_credits_candidate_once_across_exact_retry() {
    let (mem, _first, _second) = walk_fixture().await;
    let candidate = mem
        .ingest(Ingest::new(
            "start-only candidate",
            b"",
            &[],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();
    let receipt = WalkReceipt {
        db: "d".into(),
        routes: Vec::new(),
        visited: HashSet::from([candidate]),
        issued: Instant::now(),
        issued_wall: SystemTime::now(),
        consumed: false,
        bound_batch: None,
        claim: ClaimState::Available,
    };
    let mut state = state_with_receipts(HashMap::from([("start".into(), receipt)]));
    let used = HashSet::from([candidate]);

    let first =
        claim_walk_receipts(&mut state, &["start".into()], "d", &used, &HashSet::new()).unwrap();
    let applied = mneme_walk::reflect_observed_idempotent(
        &mem,
        &first.idempotency_key,
        &first.retry,
        &first.routes,
        &first.visited,
        &used,
    )
    .await
    .unwrap();
    assert_eq!(applied.commit, FeedbackCommitOutcome::Applied);
    release_walk_receipt_claims(
        &mut state.receipts,
        &first.claim.tokens,
        &first.claim.id,
        true,
    );

    let retry =
        claim_walk_receipts(&mut state, &["start".into()], "d", &used, &HashSet::new()).unwrap();
    let replay = mneme_walk::reflect_observed_idempotent(
        &mem,
        &retry.idempotency_key,
        &retry.retry,
        &retry.routes,
        &retry.visited,
        &used,
    )
    .await
    .unwrap();
    assert_eq!(replay.commit, FeedbackCommitOutcome::AlreadyApplied);
    let candidate = mem.get_node(candidate).await.unwrap().unwrap();
    assert_eq!(candidate.grounded_use_count(), 1);
    assert!(matches!(candidate.status(), mneme_core::NodeStatus::Active));
}

#[test]
fn negative_receipt_claims_are_observed_disjoint_and_payload_bound() {
    let (payload, nodes) = payload_with_nodes(2);
    let mut receipt = receipt_touched(false, 0);
    receipt.routes = payload.routes;
    receipt.visited = payload.visited;
    let mut state = state_with_receipts(HashMap::from([("r".into(), receipt)]));
    let used = HashSet::new();
    let unhelpful = HashSet::from([nodes[1]]);
    assert!(claim_walk_receipts(&mut state, &[], "d", &used, &unhelpful).is_err());
    assert!(claim_walk_receipts(&mut state, &["r".into()], "d", &unhelpful, &unhelpful).is_err());
    assert!(
        claim_walk_receipts(
            &mut state,
            &["r".into()],
            "d",
            &used,
            &HashSet::from([NodeId(ulid::Ulid::new())])
        )
        .is_err()
    );
    assert!(!state.receipts["r"].is_claimed());
    assert!(state.receipts["r"].bound_batch.is_none());
    let first = claim_walk_receipts(&mut state, &["r".into()], "d", &used, &unhelpful).unwrap();
    release_walk_receipt_claims(
        &mut state.receipts,
        &first.claim.tokens,
        &first.claim.id,
        true,
    );
    assert!(claim_walk_receipts(&mut state, &["r".into()], "d", &used, &HashSet::new()).is_err());
    let replay = claim_walk_receipts(&mut state, &["r".into()], "d", &used, &unhelpful).unwrap();
    assert_eq!(replay.idempotency_key, first.idempotency_key);
    assert_eq!(replay.retry, first.retry);
}

#[test]
fn receipt_full_done_failure_leaves_walk_resumable() {
    let mut state = SessionState::default();
    let token = "walk".to_string();
    let mut session = session_touched(0);
    session.actions = MAX_WALK_ACTIONS;
    state.active.insert(token.clone(), session);
    for i in 0..MAX_RECEIPTS {
        state
            .receipts
            .insert(format!("r{i}"), receipt_touched(false, 1));
    }

    assert!(complete_walk(&mut state, &token).is_err());
    assert!(state.active.contains_key(&token));
    assert_eq!(state.receipts.len(), MAX_RECEIPTS);

    state.receipts.remove("r0");
    let completed = complete_walk(&mut state, &token).unwrap();
    let receipt = completed["receipt"].as_str().unwrap();
    assert!(!state.active.contains_key(&token));
    assert!(state.receipts.contains_key(receipt));
    assert_eq!(state.receipts.len(), MAX_RECEIPTS);
}

#[tokio::test]
async fn abort_remains_available_after_action_exhaustion() {
    let registry = Registry {
        activity: crate::activity::ActivityRing::default(),
        dbs: BTreeMap::new(),
    };
    let mut state = SessionState::default();
    let mut session = session_touched(0);
    session.actions = MAX_WALK_ACTIONS;
    state.active.insert("walk".into(), session);

    walk_tool(
        &registry,
        &mut state,
        WalkRequest::Abort,
        &json!({ "action": "abort", "session": "walk" }),
    )
    .await
    .unwrap();
    assert!(state.active.is_empty());
}

#[test]
fn receipt_claims_are_atomic_and_reject_duplicates() {
    let (payload, nodes) = payload_with_nodes(3);
    let routes = payload.routes.clone();
    let receipts = HashMap::from([
        (
            "good".into(),
            WalkReceipt {
                db: "d".into(),
                routes: payload.routes,
                visited: payload.visited,
                issued: Instant::now(),
                issued_wall: SystemTime::now(),
                consumed: false,
                bound_batch: None,
                claim: ClaimState::Available,
            },
        ),
        ("busy".into(), receipt_touched(true, 0)),
    ]);
    let mut state = state_with_receipts(receipts);
    let used = HashSet::from([nodes[1]]);

    let batch = vec!["good".into(), "busy".into()];
    assert!(claim_walk_receipts(&mut state, &batch, "d", &used, &HashSet::new()).is_err());
    assert!(
        !state.receipts["good"].is_claimed(),
        "failed batch claimed no prefix"
    );

    release_walk_receipt_claims(&mut state.receipts, &["busy".into()], "claim", false);
    let duplicate = vec!["good".into(), "good".into()];
    assert!(claim_walk_receipts(&mut state, &duplicate, "d", &used, &HashSet::new()).is_err());
    assert!(!state.receipts["good"].is_claimed());

    let claimed =
        claim_walk_receipts(&mut state, &["good".into()], "d", &used, &HashSet::new()).unwrap();
    assert_eq!(claimed.routes, routes);
    assert!(state.receipts["good"].is_claimed());
    assert_eq!(
        state.receipts.len(),
        2,
        "claiming never grows the receipt map"
    );
}

#[test]
fn reordered_receipt_retry_has_identical_key_and_event_order() {
    let a = NodeId(ulid::Ulid::new());
    let b = NodeId(ulid::Ulid::new());
    let c = NodeId(ulid::Ulid::new());
    let make = || {
        HashMap::from([
            (
                "z".into(),
                WalkReceipt {
                    db: "d".into(),
                    routes: vec![ObservedRoute::new(b, c, b, c).unwrap()],
                    visited: HashSet::from([b, c]),
                    issued: Instant::now(),
                    issued_wall: SystemTime::now(),
                    consumed: false,
                    bound_batch: None,
                    claim: ClaimState::Available,
                },
            ),
            (
                "a".into(),
                WalkReceipt {
                    db: "d".into(),
                    routes: vec![ObservedRoute::new(a, b, a, b).unwrap()],
                    visited: HashSet::from([a, b]),
                    issued: Instant::now(),
                    issued_wall: SystemTime::now(),
                    consumed: false,
                    bound_batch: None,
                    claim: ClaimState::Available,
                },
            ),
        ])
    };
    let used = HashSet::from([b]);
    let mut state = state_with_receipts(make());
    let first = claim_walk_receipts(
        &mut state,
        &["z".into(), "a".into()],
        "d",
        &used,
        &HashSet::new(),
    )
    .unwrap();
    release_walk_receipt_claims(
        &mut state.receipts,
        &first.claim.tokens,
        &first.claim.id,
        true,
    );
    let second = claim_walk_receipts(
        &mut state,
        &["a".into(), "z".into()],
        "d",
        &used,
        &HashSet::new(),
    )
    .unwrap();

    assert_eq!(first.idempotency_key, second.idempotency_key);
    assert_eq!(first.routes, second.routes);
    assert_eq!(first.claim.tokens, second.claim.tokens);
    assert!(
        second
            .claim
            .tokens
            .iter()
            .all(|token| state.receipts[token].consumed)
    );
}

#[test]
fn consumed_receipts_reject_subset_and_superset_regrouping_even_without_edges() {
    let receipts = HashMap::from([
        ("a".into(), receipt_touched(false, 0)),
        ("b".into(), receipt_touched(false, 0)),
        ("c".into(), receipt_touched(false, 0)),
    ]);
    let mut state = state_with_receipts(receipts);
    let used = HashSet::new();
    let first = claim_walk_receipts(
        &mut state,
        &["b".into(), "a".into()],
        "d",
        &used,
        &HashSet::new(),
    )
    .unwrap();
    assert!(first.routes.is_empty(), "fixture models a no-route walk");
    release_walk_receipt_claims(
        &mut state.receipts,
        &first.claim.tokens,
        &first.claim.id,
        true,
    );

    assert!(claim_walk_receipts(&mut state, &["a".into()], "d", &used, &HashSet::new()).is_err());
    assert!(!state.receipts["a"].is_claimed());
    assert!(
        claim_walk_receipts(
            &mut state,
            &["a".into(), "b".into(), "c".into()],
            "d",
            &used,
            &HashSet::new()
        )
        .is_err()
    );
    assert!(state.receipts.values().all(|receipt| !receipt.is_claimed()));

    let exact = claim_walk_receipts(
        &mut state,
        &["a".into(), "b".into()],
        "d",
        &used,
        &HashSet::new(),
    )
    .unwrap();
    assert_eq!(exact.idempotency_key, first.idempotency_key);
    assert!(exact.routes.is_empty());
}

#[tokio::test]
async fn dropped_claim_guard_releases_receipt_immediately() {
    let (payload, nodes) = payload_with_nodes(2);
    let state = Mutex::new(state_with_receipts(HashMap::from([(
        "r".into(),
        WalkReceipt {
            db: "d".into(),
            routes: payload.routes,
            visited: payload.visited,
            issued: Instant::now(),
            issued_wall: SystemTime::now(),
            consumed: false,
            bound_batch: None,
            claim: ClaimState::Available,
        },
    )])));
    let used = HashSet::from([nodes[1]]);
    let claimed = {
        let mut locked = state.lock().await;
        claim_walk_receipts(&mut locked, &["r".into()], "d", &used, &HashSet::new()).unwrap()
    };
    let batch_key = claimed.idempotency_key.clone();
    let retry_scope = claimed.retry.clone();

    {
        let _guard = claimed.into_guard(&state);
    }

    let mut locked = state.lock().await;
    assert!(!locked.receipts["r"].is_claimed());
    assert!(locked.receipts["r"].claim_id().is_none());
    assert!(!locked.receipts["r"].consumed);
    assert_eq!(
        locked.receipts["r"]
            .bound_batch
            .as_ref()
            .map(|binding| binding.key.as_str()),
        Some(batch_key.as_str()),
        "cancellation cleanup must never clear the pre-commit batch binding"
    );
    let alternate_used = HashSet::from([nodes[0], nodes[1]]);
    assert!(
        claim_walk_receipts(
            &mut locked,
            &["r".into()],
            "d",
            &alternate_used,
            &HashSet::new()
        )
        .is_err(),
        "cancellation must not permit an alternate used/event classification"
    );
    let retry =
        claim_walk_receipts(&mut locked, &["r".into()], "d", &used, &HashSet::new()).unwrap();
    assert_eq!(retry.idempotency_key, batch_key);
    assert_eq!(retry.retry, retry_scope);
}

#[tokio::test]
async fn cancelled_guard_under_lock_recovers_via_lease_without_rebinding() {
    let state = Mutex::new(state_with_receipts(HashMap::from([(
        "r".into(),
        receipt_touched(false, 0),
    )])));
    let used = HashSet::new();
    let mut locked = state.lock().await;
    let first = claim_walk_receipts(&mut locked, &["r".into()], "d", &used, &used).unwrap();
    let retry_scope = first.retry.clone();
    let binding = locked.receipts["r"].bound_batch.clone();
    let guard = first.into_guard(&state);

    // Drop cannot acquire this lock. The unchanged lease is the fallback, not a
    // cleared binding or an invented successful reflection.
    drop(guard);
    assert!(locked.receipts["r"].is_claimed());
    let ClaimState::Claimed(lease) = &mut locked.receipts.get_mut("r").unwrap().claim else {
        panic!("cancelled claim must remain leased until expiry");
    };
    lease.started = Instant::now() - RECEIPT_CLAIM_TTL - Duration::from_secs(1);
    locked.purge_expired_receipts();
    assert!(!locked.receipts["r"].is_claimed());
    assert!(!locked.receipts["r"].consumed);
    assert_eq!(locked.receipts["r"].bound_batch, binding);
    let retry = claim_walk_receipts(&mut locked, &["r".into()], "d", &used, &used).unwrap();
    assert_eq!(retry.retry, retry_scope);
}

#[tokio::test]
async fn late_attempt_cleanup_cannot_release_or_consume_a_new_claim_owner() {
    for cleanup in ["drop", "failure", "success"] {
        let state = Mutex::new(state_with_receipts(HashMap::from([(
            "r".into(),
            receipt_touched(false, 0),
        )])));
        let used = HashSet::new();
        let mut locked = state.lock().await;
        let first = claim_walk_receipts(&mut locked, &["r".into()], "d", &used, &used).unwrap();
        let first_id = first.claim.id.clone();
        let old_guard = first.into_guard(&state);
        let ClaimState::Claimed(lease) = &mut locked.receipts.get_mut("r").unwrap().claim else {
            panic!("first claim owns a lease");
        };
        lease.started = Instant::now() - RECEIPT_CLAIM_TTL - Duration::from_secs(1);
        let retry = claim_walk_receipts(&mut locked, &["r".into()], "d", &used, &used).unwrap();
        let retry_id = retry.claim.id.clone();
        let binding = locked.receipts["r"].bound_batch.clone();
        assert_ne!(first_id, retry_id);
        let retry_guard = retry.into_guard(&state);

        if cleanup == "drop" {
            drop(locked);
            drop(old_guard); // The mutex is available, so this probes owner matching.
            locked = state.lock().await;
        } else {
            old_guard.finish(&mut locked, cleanup == "success");
        }
        assert_eq!(locked.receipts["r"].claim_id(), Some(retry_id.as_str()));
        assert!(
            !locked.receipts["r"].consumed,
            "late {cleanup} consumed a newer lease"
        );
        assert_eq!(locked.receipts["r"].bound_batch, binding);

        retry_guard.finish(&mut locked, true);
        assert!(!locked.receipts["r"].is_claimed());
        assert!(locked.receipts["r"].consumed);
        let retry = claim_walk_receipts(&mut locked, &["r".into()], "d", &used, &used).unwrap();
        retry.into_guard(&state).finish(&mut locked, false);
        assert!(
            locked.receipts["r"].consumed,
            "failed exact retry lost its tombstone"
        );
        assert_eq!(locked.receipts["r"].bound_batch, binding);
    }
}

#[test]
fn either_clock_expires_an_unclaimed_receipt_without_extending_a_claim() {
    let mut monotonic_expired = receipt_touched(false, 0);
    monotonic_expired.issued = Instant::now() - RECEIPT_TTL - Duration::from_secs(1);
    monotonic_expired.issued_wall = SystemTime::now() + RECEIPT_TTL;
    let mut wall_expired = receipt_touched(false, 0);
    wall_expired.issued_wall = SystemTime::now() - RECEIPT_TTL - Duration::from_secs(1);
    let mut wall_rollback = receipt_touched(false, 0);
    wall_rollback.issued_wall = SystemTime::now() + RECEIPT_TTL;
    let claimed = receipt_touched(true, RECEIPT_TTL.as_secs() + 1);
    let mut receipts = HashMap::from([
        ("monotonic".into(), monotonic_expired),
        ("wall".into(), wall_expired),
        ("rollback".into(), wall_rollback),
        ("claimed".into(), claimed),
    ]);
    purge_expired_unclaimed_receipts(&mut receipts);
    assert!(!receipts.contains_key("monotonic"));
    assert!(!receipts.contains_key("wall"));
    assert!(receipts.contains_key("rollback"));
    assert!(receipts.contains_key("claimed"));

    let ClaimState::Claimed(lease) = &mut receipts.get_mut("claimed").unwrap().claim else {
        panic!("live claim pins an otherwise expired receipt");
    };
    lease.started = Instant::now() - RECEIPT_CLAIM_TTL - Duration::from_secs(1);
    purge_expired_unclaimed_receipts(&mut receipts);
    assert!(!receipts.contains_key("claimed"));
    assert_eq!(receipts.len(), 1);
}

#[test]
fn receipt_admission_errors_do_not_partially_bind_or_allocate_sequences() {
    let mut other = receipt_touched(false, 0);
    other.db = "other".into();
    let mut state = state_with_receipts(HashMap::from([
        ("good".into(), receipt_touched(false, 0)),
        ("other".into(), other),
    ]));
    for rejected in ["missing", "other"] {
        assert!(
            claim_walk_receipts(
                &mut state,
                &["good".into(), rejected.into()],
                "d",
                &HashSet::new(),
                &HashSet::new(),
            )
            .is_err()
        );
        assert!(state.receipts.values().all(|receipt| {
            !receipt.is_claimed() && receipt.bound_batch.is_none() && !receipt.consumed
        }));
        assert!(state.next_feedback_sequence.is_empty());
    }
    state
        .next_feedback_sequence
        .insert("d".into(), i64::MAX as u64);
    assert!(
        claim_walk_receipts(
            &mut state,
            &["good".into()],
            "d",
            &HashSet::new(),
            &HashSet::new(),
        )
        .is_err()
    );
    assert!(!state.receipts["good"].is_claimed());
    assert!(state.receipts["good"].bound_batch.is_none());
    assert_eq!(state.next_feedback_sequence["d"], i64::MAX as u64);
}
