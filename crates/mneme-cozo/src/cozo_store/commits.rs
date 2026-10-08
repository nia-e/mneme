use super::maintenance::tx_sync_tag_projection;
use super::*;

struct FeedbackLedgerProof {
    fingerprint: String,
    sequence: i64,
}

struct FeedbackLedgerOrder {
    epoch: String,
    sequence: i64,
    marker: bool,
}

struct FeedbackLedgerCensus {
    proofs: BTreeMap<String, FeedbackLedgerProof>,
    orders: BTreeMap<String, FeedbackLedgerOrder>,
}

pub(super) fn stage_feedback_transaction(
    tx: &MultiTransaction,
    commit: &FeedbackCommit,
) -> Result<FeedbackCommitOutcome> {
    if let Some(idempotency) = &commit.idempotency {
        // Protect an exact retry in this authority generation before any
        // reachability reclamation. The physical proof relation is keyed by the
        // bare receipt-set key, so its order row supplies the epoch half of the
        // logical `(epoch, key)` replay identity. A collision from another epoch
        // is unreachable old-generation state and joins the staged purge below.
        let census = feedback_retry_ledger_census(tx)?;
        match (
            census.proofs.get(&idempotency.key),
            census.orders.get(&idempotency.key),
        ) {
            (None, _) => {
                // A dangling order row has no acknowledgement authority. The
                // staged purge below removes it before publishing a new pair.
            }
            (Some(proof), Some(order)) => {
                debug_assert!(proof.sequence == order.sequence && order.marker);
                if order.epoch == idempotency.retry.epoch {
                    return if proof.fingerprint == idempotency.fingerprint {
                        Ok(FeedbackCommitOutcome::AlreadyApplied)
                    } else {
                        Err(Error::InvalidInput(
                            "feedback idempotency key was reused with a different payload in the same epoch"
                                .into(),
                        ))
                    };
                }
            }
            (Some(_), None) => return Err(feedback_retry_ledger_ambiguous()),
        }

        let sequence = i64::try_from(idempotency.retry.sequence).map_err(|_| {
            Error::InvalidInput("feedback sequence exceeds the storage integer range".into())
        })?;
        let floor = i64::try_from(idempotency.retry.min_live_sequence).map_err(|_| {
            Error::InvalidInput("feedback live floor exceeds the storage integer range".into())
        })?;
        let mut purge = BTreeMap::new();
        purge.insert("epoch".into(), dv_str(&idempotency.retry.epoch));
        purge.insert("floor".into(), dv_int(floor));
        tx_run(
            tx,
            "live[key] := *feedback_retry{key, applied_at: sequence}, \
               *feedback_retry_order{epoch: $epoch, sequence, key, marker}, \
               marker == true, sequence >= $floor\n\
             ?[key] := *feedback_retry{key}, not live[key] \
             :rm feedback_retry {key}",
            purge.clone(),
        )?;
        tx_run(
            tx,
            "live[epoch, sequence, key] := \
               *feedback_retry_order{epoch, sequence, key, marker}, epoch == $epoch, \
               marker == true, sequence >= $floor, \
               *feedback_retry{key, applied_at: sequence}\n\
             ?[epoch, sequence, key] := *feedback_retry_order{epoch, sequence, key}, \
               not live[epoch, sequence, key] \
             :rm feedback_retry_order {epoch, sequence, key}",
            purge,
        )?;

        let count = tx_run(tx, "?[count(key)] := *feedback_retry{key}", BTreeMap::new())?;
        let count = count
            .rows
            .first()
            .map(|row| want_i64(&row[0]))
            .transpose()?
            .unwrap_or(0);
        if count >= MAX_FEEDBACK_RETRY_RECORDS as i64 {
            return Err(crate::feedback_retry_capacity_error());
        }

        debug_assert!(sequence > 0 && floor > 0 && floor <= sequence);
    }

    for update in &commit.nodes {
        graph::require_semantic_node(&update.expected, "feedback")?;
        graph::require_semantic_node(&update.replacement, "feedback")?;
        touchstones::tx_validate_owner_replacement(tx, &update.replacement)?;
    }
    let touched = commit
        .nodes
        .iter()
        .map(|update| update.expected.id())
        .chain(
            commit
                .edges
                .iter()
                .flat_map(|update| [update.replacement.from, update.replacement.to]),
        )
        .collect::<Vec<_>>();
    graph::tx_reject_episode_nodes(tx, &touched, "feedback")?;

    if !commit.nodes.is_empty() {
        let (input, params) = feedback_node_input(tx, commit)?;
        let cas = format!(
            "{input}\n\
             ?[id] := feedback_nodes[id, expected, replacement, status], \
               not *node{{id, data: expected}} :assert none"
        );
        tx_run(tx, &cas, params.clone()).map_err(feedback_conflict)?;

        let write = format!(
            "{input}\n\
             ?[id, data, status] := \
               feedback_nodes[id, expected, data, status] \
               :put node {{id => data, status}}"
        );
        tx_run(tx, &write, params)?;
        let moved = commit
            .nodes
            .iter()
            .filter(|update| {
                TaggedPhysicalStatus::from(update.expected.status())
                    != TaggedPhysicalStatus::from(update.replacement.status())
            })
            .map(|update| &update.replacement)
            .collect::<Vec<_>>();
        tx_sync_tag_projection(tx, &moved)?;
    }

    if !commit.edges.is_empty() {
        let (input, params) = feedback_edge_input(commit);
        let cas = format!(
            "{input}\n\
             bad[from, to] := feedback_edges[from, to, present, ew, ek, elr, et, ei, ap, ast, aen, nw, nk, nlr, nt, ni], \
               present == 0, *edge{{from, to}}\n\
             bad[from, to] := feedback_edges[from, to, present, ew, ek, elr, et, ei, ap, ast, aen, nw, nk, nlr, nt, ni], \
               present == 1, not *edge{{from, to, weight: ew, kind: ek, last_reinforced: elr, trials: et, interference: ei}}\n\
             bad[from, to] := feedback_edges[from, to, present, ew, ek, elr, et, ei, ap, ast, aen, nw, nk, nlr, nt, ni], \
               not *node{{id: from}}\n\
             bad[from, to] := feedback_edges[from, to, present, ew, ek, elr, et, ei, ap, ast, aen, nw, nk, nlr, nt, ni], \
               not *node{{id: to}}\n\
             bad[from, to] := feedback_edges[from, to, present, ew, ek, elr, et, ei, ap, ast, aen, nw, nk, nlr, nt, ni], \
               ap == 0, *edge_anchor{{from, to}}\n\
             bad[from, to] := feedback_edges[from, to, present, ew, ek, elr, et, ei, ap, ast, aen, nw, nk, nlr, nt, ni], \
               ap == 1, not *edge_anchor{{from, to, start: ast, end: aen}}\n\
             ?[from, to] := bad[from, to] :assert none"
        );
        tx_run(tx, &cas, params.clone()).map_err(feedback_conflict)?;

        let capacity = format!(
            "{input}\n\
             new_edge[from, to] := feedback_edges[from, to, present, ew, ek, elr, et, ei, ap, ast, aen, nw, nk, nlr, nt, ni], present == 0\n\
             new_endpoint[endpoint] := new_edge[endpoint, to]\n\
             new_endpoint[endpoint] := new_edge[from, endpoint]\n\
             incident[endpoint, endpoint, to] := new_endpoint[endpoint], *edge{{from: endpoint, to}}\n\
             incident[endpoint, from, endpoint] := new_endpoint[endpoint], *edge{{to: endpoint, from}}\n\
             incident[endpoint, from, to] := new_edge[from, to], endpoint = from\n\
             incident[endpoint, from, to] := new_edge[from, to], endpoint = to, to != from\n\
             degree[endpoint, count(from)] := incident[endpoint, from, to]\n\
             ?[endpoint, n] := degree[endpoint, n], n > $cap :assert none"
        );
        let mut capacity_params = params.clone();
        capacity_params.insert("cap".into(), dv_int(MAX_INCIDENT_EDGES as i64));
        tx_run(tx, &capacity, capacity_params).map_err(feedback_capacity_error)?;

        let write = format!(
            "{input}\n\
             ?[from, to, weight, kind, last_reinforced, trials, interference] := \
               feedback_edges[from, to, present, ew, ek, elr, et, ei, ap, ast, aen, weight, kind, last_reinforced, trials, interference] \
               :put edge {{from, to => weight, kind, last_reinforced, trials, interference}}"
        );
        tx_run(tx, &write, params)?;
    }

    if !commit.merge_observations.is_empty() {
        let (input, params) = feedback_merge_replacement_input(tx, commit)?;
        let endpoints = format!(
            "{input}\n\
             bad[id] := \
               feedback_merge_replacements[id, hi, observations, first_seen, last_seen, resolution], \
               is_null(resolution), not *node{{id}}\n\
             bad[id] := \
               feedback_merge_replacements[lo, id, observations, first_seen, last_seen, resolution], \
               is_null(resolution), not *node{{id}}\n\
             ?[id] := bad[id] :assert none"
        );
        tx_run(tx, &endpoints, params.clone()).map_err(feedback_conflict)?;

        let write = format!(
            "{input}\n\
             ?[lo, hi, observations, first_seen, last_seen, resolution] := \
               feedback_merge_replacements[lo, hi, observations, first_seen, last_seen, resolution] \
               :put merge_candidate {{lo, hi => observations, first_seen, last_seen, resolution}}"
        );
        tx_run(tx, &write, params)?;
    }

    if let Some(idempotency) = &commit.idempotency {
        let mut ledger = BTreeMap::new();
        ledger.insert("key".into(), dv_str(&idempotency.key));
        ledger.insert("fingerprint".into(), dv_str(&idempotency.fingerprint));
        ledger.insert("epoch".into(), dv_str(&idempotency.retry.epoch));
        ledger.insert(
            "sequence".into(),
            dv_int(i64::try_from(idempotency.retry.sequence).map_err(|_| {
                Error::InvalidInput("feedback sequence exceeds the storage integer range".into())
            })?),
        );
        tx_run(
            tx,
            "?[key, fingerprint, applied_at] <- [[$key, $fingerprint, $sequence]] \
             :put feedback_retry {key => fingerprint, applied_at}",
            ledger.clone(),
        )?;
        tx_run(
            tx,
            "?[epoch, sequence, key, marker] <- [[$epoch, $sequence, $key, true]] \
             :put feedback_retry_order {epoch, sequence, key => marker}",
            ledger,
        )?;
    }
    Ok(FeedbackCommitOutcome::Applied)
}

fn feedback_retry_ledger_ambiguous() -> Error {
    Error::Backend(
        "feedback retry ledger has no unique proof-to-order authority; activate a fresh feedback epoch before retrying"
            .into(),
    )
}

fn feedback_retry_ledger_census(tx: &MultiTransaction) -> Result<FeedbackLedgerCensus> {
    let limit = MAX_FEEDBACK_RETRY_RECORDS + 1;
    let proof_rows = tx_run(
        tx,
        &format!(
            "?[key, fingerprint, applied_at] := *feedback_retry{{key, fingerprint, applied_at}} \
             :order key :limit {limit}"
        ),
        BTreeMap::new(),
    )?;
    if proof_rows.rows.len() > MAX_FEEDBACK_RETRY_RECORDS {
        return Err(feedback_retry_ledger_ambiguous());
    }
    let mut proofs = BTreeMap::new();
    for row in &proof_rows.rows {
        let key = want_str(&row[0])?.to_owned();
        let proof = FeedbackLedgerProof {
            fingerprint: want_str(&row[1])?.to_owned(),
            sequence: want_i64(&row[2])?,
        };
        if proof.sequence <= 0 || proof.sequence == i64::MAX {
            return Err(feedback_retry_ledger_ambiguous());
        }
        if proofs.insert(key, proof).is_some() {
            return Err(feedback_retry_ledger_ambiguous());
        }
    }

    let order_rows = tx_run(
        tx,
        &format!(
            "?[epoch, sequence, key, marker] := \
               *feedback_retry_order{{epoch, sequence, key, marker}} \
             :order epoch, sequence, key :limit {limit}"
        ),
        BTreeMap::new(),
    )?;
    if order_rows.rows.len() > MAX_FEEDBACK_RETRY_RECORDS {
        return Err(feedback_retry_ledger_ambiguous());
    }
    let mut orders = BTreeMap::new();
    for row in &order_rows.rows {
        let key = want_str(&row[2])?.to_owned();
        let order = FeedbackLedgerOrder {
            epoch: want_str(&row[0])?.to_owned(),
            sequence: want_i64(&row[1])?,
            marker: matches!(row[3], DataValue::Bool(true)),
        };
        if order.sequence <= 0 || order.sequence == i64::MAX {
            return Err(feedback_retry_ledger_ambiguous());
        }
        if orders.insert(key, order).is_some() {
            return Err(feedback_retry_ledger_ambiguous());
        }
    }

    for (key, proof) in &proofs {
        if let Some(order) = orders.get(key)
            && (proof.sequence != order.sequence || !order.marker)
        {
            return Err(feedback_retry_ledger_ambiguous());
        }
    }
    Ok(FeedbackLedgerCensus { proofs, orders })
}

pub(super) fn stage_full_merge_transaction(
    tx: &MultiTransaction,
    source_db: Ulid,
    commit: &FullMergeCommit,
) -> Result<FullMergeCommitOutcome> {
    // Keep generated scripts and parameter maps predictably small at the hard
    // graph caps. A legal 2,048-row incident union otherwise expands to more
    // than 20,000 inline values in one parser invocation. Every chunk still
    // belongs to this one Cozo transaction, so chunking changes neither
    // visibility nor rollback semantics.
    const KEY_BATCH_ROWS: usize = 256;
    const EDGE_BATCH_ROWS: usize = 64;
    const REMOTE_BATCH_ROWS: usize = 128;

    commit.validate()?;
    let (lo, hi) = canonical(commit.pair());
    let winner = commit.winner.0.to_string();
    let loser = commit.loser.0.to_string();
    let mut identity = BTreeMap::new();
    identity.insert("lo".into(), dv_str(&lo));
    identity.insert("hi".into(), dv_str(&hi));
    identity.insert("winner".into(), dv_str(&winner));
    identity.insert("loser".into(), dv_str(&loser));

    // A proof authorizes retry only together with the minimum immutable
    // post-state of a committed collapse. Mutable winner state is deliberately
    // excluded: lifecycle, adjacency, and even endpoint retention may legally
    // drift after the historical decision.
    if let Some(outcome) = admit_full_merge_retry(tx, commit)? {
        return Ok(outcome);
    }

    touchstones::tx_reject_owner_merge(tx, &[commit.winner, commit.loser])?;
    let winner_node = full_merge_node(tx, commit.winner)?;
    let mut loser_node = full_merge_node(tx, commit.loser)?;
    graph::require_semantic_node(&winner_node, "full merge")?;
    graph::require_semantic_node(&loser_node, "full merge")?;
    if winner_node.is_archived() || loser_node.is_archived() {
        return Err(Error::Conflict(
            "full merge requires live winner and loser nodes".into(),
        ));
    }

    let mut candidate = overlay::load_merge_candidate(tx, commit.pair())?.ok_or(Error::NotFound)?;
    if !candidate.is_open() {
        return Err(Error::Conflict(
            "full merge candidate was already resolved".into(),
        ));
    }
    candidate.resolve(MergeResolution::Full);
    candidate.validate().map_err(|error| {
        Error::InvalidInput(format!(
            "full merge candidate replacement is invalid: {error}"
        ))
    })?;

    let mut incident = full_merge_incident_edges(tx, commit.winner, commit.loser)?;
    let endpoint_ids = incident
        .iter()
        .flat_map(|edge| [edge.from, edge.to])
        .collect::<Vec<_>>();
    let episode_ids = graph::tx_episode_ids(tx, &endpoint_ids)?;
    let retained_winner_evidence = incident
        .iter()
        .filter(|edge| {
            (edge.from == commit.winner || edge.to == commit.winner)
                && (episode_ids.contains(&edge.from) || episode_ids.contains(&edge.to))
        })
        .count();
    // Historical evidence keeps its exact endpoint, including the archived
    // loser. A semantic merge must not rewrite what an episode referred to.
    incident.retain(|edge| !episode_ids.contains(&edge.from) && !episode_ids.contains(&edge.to));
    let final_edges = commit.normalize_local_edges(&incident)?;
    if final_edges.len() + retained_winner_evidence > MAX_INCIDENT_EDGES {
        return Err(crate::incident_edge_capacity_error());
    }
    for edge in &final_edges {
        edge.validate().map_err(|error| {
            Error::InvalidInput(format!(
                "full merge produced an invalid local edge: {error}"
            ))
        })?;
    }
    let winner_remote = full_merge_remote_edges(tx, source_db, commit.winner)?;
    let loser_remote = full_merge_remote_edges(tx, source_db, commit.loser)?;
    let final_remote = commit.normalize_remote_edges(&winner_remote, &loser_remote)?;
    for edge in &final_remote {
        edge.validate_for_source_database(source_db)
            .map_err(|error| {
                Error::InvalidInput(format!(
                    "full merge produced an invalid remote edge: {error}"
                ))
            })?;
    }

    // Remove exactly the bounded rows read above; keyed inline joins avoid an
    // anchor scan even though the compatibility anchor relation has no `to`
    // secondary index.
    let incident_pairs = incident
        .iter()
        .map(|edge| (edge.from, edge.to))
        .collect::<Vec<_>>();
    for pairs in incident_pairs.chunks(KEY_BATCH_ROWS) {
        let (input, params) = edge_pair_input("full_merge_old_edges", pairs);
        tx_run(
            tx,
            &format!(
                "{input}\n\
                 ?[from, to] := full_merge_old_edges[from, to], *edge_anchor{{from, to}} \
                   :rm edge_anchor {{from, to}}"
            ),
            params.clone(),
        )?;
        tx_run(
            tx,
            &format!(
                "{input}\n\
                 ?[from, to] := full_merge_old_edges[from, to], *edge{{from, to}} \
                   :rm edge {{from, to}}"
            ),
            params,
        )?;
    }
    for edges in final_edges.chunks(EDGE_BATCH_ROWS) {
        let (input, params) = full_merge_edge_input(edges)?;
        tx_run(
            tx,
            &format!(
                "{input}\n\
                 ?[from, to, weight, kind, last_reinforced, trials, interference] := \
                   full_merge_edges[from, to, weight, kind, last_reinforced, trials, interference, ap, ast, aen] \
                   :put edge {{from, to => weight, kind, last_reinforced, trials, interference}}"
            ),
            params.clone(),
        )?;
        tx_run(
            tx,
            &format!(
                "{input}\n\
                 ?[from, to, start, end] := \
                   full_merge_edges[from, to, weight, kind, last_reinforced, trials, interference, ap, start, end], ap == 1 \
                   :put edge_anchor {{from, to => start, end}}"
            ),
            params,
        )?;
    }

    let old_remote = winner_remote
        .iter()
        .chain(&loser_remote)
        .cloned()
        .collect::<Vec<_>>();
    for edges in old_remote.chunks(REMOTE_BATCH_ROWS) {
        let (input, params) = full_merge_remote_input("full_merge_old_remote", source_db, edges)?;
        tx_run(
            tx,
            &format!(
                "{input}\n\
                 ?[from, target_db, target] := \
                   full_merge_old_remote[from, target_db, target, weight], \
                   *remote_edge{{from, target_db, target}} \
                   :rm remote_edge {{from, target_db, target}}"
            ),
            params,
        )?;
    }
    for edges in final_remote.chunks(REMOTE_BATCH_ROWS) {
        let (input, params) = full_merge_remote_input("full_merge_new_remote", source_db, edges)?;
        tx_run(
            tx,
            &format!(
                "{input}\n\
                 ?[from, target_db, target, weight] := \
                   full_merge_new_remote[from, target_db, target, weight] \
                   :put remote_edge {{from, target_db, target => weight}}"
            ),
            params,
        )?;
    }

    let loser_lane_changed =
        TaggedPhysicalStatus::from(loser_node.status()) != TaggedPhysicalStatus::Archived;
    loser_node.set_status(NodeStatus::Archived);
    let mut node_params = BTreeMap::new();
    node_params.insert("id".into(), dv_str(&loser));
    node_params.insert("data".into(), dv_str(&encode_canonical_node(&loser_node)?));
    node_params.insert("status".into(), dv_str(status_str(NodeStatus::Archived)));
    node_params.insert("summary".into(), dv_str(loser_node.summary()));
    tx_run(
        tx,
        "?[id, data, status] <- [[$id, $data, $status]] :put node {id => data, status}",
        node_params.clone(),
    )?;
    tx_run(
        tx,
        "?[id, summary, status] <- [[$id, $summary, $status]] \
         :put node_search {id => summary, status}",
        node_params.clone(),
    )?;
    tx_run(
        tx,
        "?[id, e, status] := *node_vec{id, e}, id == $id, status = $status \
         :put node_vec {id => e, status}",
        node_params,
    )?;
    if loser_lane_changed {
        tx_sync_tag_projection(tx, &[&loser_node])?;
    }

    identity.insert(
        "applied_at".into(),
        dv_int(i64::try_from(commit.applied_at).map_err(|_| {
            Error::InvalidInput("full merge timestamp exceeds the storage integer range".into())
        })?),
    );
    overlay::stage_merge_candidate_upsert(tx, &candidate)?;
    tx_run(
        tx,
        "?[lo, hi, winner, loser, applied_at] <- \
           [[$lo, $hi, $winner, $loser, $applied_at]] \
           :put full_merge_commit {lo, hi => winner, loser, applied_at}",
        identity,
    )?;
    Ok(FullMergeCommitOutcome::Applied)
}

fn admit_full_merge_retry(
    tx: &MultiTransaction,
    commit: &FullMergeCommit,
) -> Result<Option<FullMergeCommitOutcome>> {
    let (lo, hi) = canonical(commit.pair());
    let mut params = BTreeMap::new();
    params.insert("lo".into(), dv_str(&lo));
    params.insert("hi".into(), dv_str(&hi));

    let reversed = tx_run(
        tx,
        "?[stored_lo, stored_hi] := \
           *full_merge_commit{lo: stored_lo, hi: stored_hi}, \
           stored_lo == $hi, stored_hi == $lo :limit 1",
        params.clone(),
    )?;
    if !reversed.rows.is_empty() {
        return Err(backend_str(
            "stored full merge proof has a reversed physical key".into(),
        ));
    }

    let proof = tx_run(
        tx,
        "?[lo, hi, winner, loser, applied_at] := \
           *full_merge_commit{lo, hi, winner, loser, applied_at}, \
           lo == $lo, hi == $hi :limit 2",
        params,
    )?;
    if proof.rows.len() > 1 {
        return Err(backend_str(
            "stored full merge proof key has multiple physical rows".into(),
        ));
    }
    let Some(row) = proof.rows.first() else {
        return Ok(None);
    };
    if row.len() != 5 {
        return Err(backend_str(format!(
            "stored full merge proof row has {} values, expected 5",
            row.len()
        )));
    }
    let between = overlay::decode_stored_pair(&row[0], &row[1], "full merge proof")?;
    let record = FullMergeRecord {
        between,
        winner: graph_records::decode_canonical_node_id(&row[2], "full merge proof winner")?,
        loser: graph_records::decode_canonical_node_id(&row[3], "full merge proof loser")?,
        applied_at: graph_records::decode_storage_timestamp(
            &row[4],
            "full merge proof applied_at",
        )?,
    };
    record
        .validate()
        .map_err(|error| backend_str(format!("invalid stored full merge proof: {error}")))?;
    if record.between != commit.pair() {
        return Err(backend_str(
            "stored full merge proof does not match the requested pair".into(),
        ));
    }

    let candidate = overlay::load_merge_candidate(tx, record.between)?.ok_or_else(|| {
        backend_str("stored full merge proof has no terminal merge candidate".into())
    })?;
    if candidate.resolution != Some(MergeResolution::Full) {
        return Err(backend_str(
            "stored full merge proof candidate is not resolved full".into(),
        ));
    }

    if let Some(loser) = full_merge_optional_node(tx, record.loser)? {
        if !loser.is_semantic() || !loser.is_archived() {
            return Err(backend_str(
                "stored full merge proof retains an episode or non-archived loser".into(),
            ));
        }
    }

    let mut loser_params = BTreeMap::new();
    loser_params.insert("loser".into(), dv_str(&record.loser.0.to_string()));
    loser_params.insert("read_cap".into(), dv_int((MAX_INCIDENT_EDGES + 1) as i64));
    let incident = tx_run(
        tx,
        "incident[from, to] := *edge{from: $loser, to}, from = $loser\n\
         incident[from, to] := *edge{to: $loser, from}, to = $loser\n\
         ?[from, to] := incident[from, to] :limit $read_cap",
        loser_params.clone(),
    )?;
    if incident.rows.len() > MAX_INCIDENT_EDGES {
        return Err(crate::incident_edge_capacity_error());
    }
    let pairs = incident
        .rows
        .iter()
        .map(|row| Ok((node_id(want_str(&row[0])?)?, node_id(want_str(&row[1])?)?)))
        .collect::<Result<Vec<_>>>()?;
    let endpoints = pairs
        .iter()
        .flat_map(|(from, to)| [*from, *to])
        .collect::<Vec<_>>();
    let episodes = graph::tx_episode_ids(tx, &endpoints)?;
    if pairs
        .iter()
        .any(|(from, to)| !episodes.contains(from) && !episodes.contains(to))
    {
        return Err(backend_str(
            "stored full merge proof retains semantic local loser adjacency".into(),
        ));
    }
    let remote = tx_run(
        tx,
        "?[target_db, target] := \
           *remote_edge{from: $loser, target_db, target} :limit 1",
        loser_params,
    )?;
    if !remote.rows.is_empty() {
        return Err(backend_str(
            "stored full merge proof retains remote loser adjacency".into(),
        ));
    }

    if record.winner == commit.winner && record.loser == commit.loser {
        Ok(Some(FullMergeCommitOutcome::AlreadyApplied))
    } else {
        debug_assert_eq!(record.winner, commit.loser);
        debug_assert_eq!(record.loser, commit.winner);
        Err(Error::Conflict(
            "full merge pair already committed in the opposite direction".into(),
        ))
    }
}

pub(super) fn stage_supersede_transaction(
    tx: &MultiTransaction,
    commit: &SupersedeCommit,
) -> Result<SupersedeCommitOutcome> {
    commit.validate()?;
    let (lo, hi) = canonical(commit.pair());
    let winner = commit.winner.0.to_string();
    let loser = commit.loser.0.to_string();
    let mut params = BTreeMap::new();
    params.insert("lo".into(), dv_str(&lo));
    params.insert("hi".into(), dv_str(&hi));
    params.insert("winner".into(), dv_str(&winner));
    params.insert("loser".into(), dv_str(&loser));

    // Direction-bound proof comes first: replanning from the already-archived
    // loser must remain archived after a lost acknowledgement.
    let existing = tx_run(
        tx,
        "?[winner, loser] := *supersede_commit{lo: $lo, hi: $hi, winner, loser}",
        params.clone(),
    )?;
    if let Some(row) = existing.rows.first() {
        return if want_str(&row[0])? == winner && want_str(&row[1])? == loser {
            Ok(SupersedeCommitOutcome::AlreadyApplied)
        } else {
            Err(Error::Conflict(
                "supersede pair already committed in the opposite direction".into(),
            ))
        };
    }

    // Both endpoints must exist in the same transaction snapshot. The winner
    // value is intentionally unused beyond proving presence.
    let winner_node = full_merge_node(tx, commit.winner)?;
    let mut loser_node = full_merge_node(tx, commit.loser)?;
    graph::require_semantic_node(&winner_node, "supersede")?;
    graph::require_semantic_node(&loser_node, "supersede")?;
    let exact_edge = tx_run(
        tx,
        "?[weight] := *edge{from: $winner, to: $loser, weight}",
        params.clone(),
    )?;
    if exact_edge.rows.is_empty()
        && (supersede_endpoint_degree(tx, commit.winner)? >= MAX_INCIDENT_EDGES
            || supersede_endpoint_degree(tx, commit.loser)? >= MAX_INCIDENT_EDGES)
    {
        return Err(crate::incident_edge_capacity_error());
    }

    let contradiction_rows = tx_run(
        tx,
        "?[observations, first_seen, last_seen, resolution] := \
         *contradiction{lo: $lo, hi: $hi, observations, first_seen, last_seen, resolution}",
        params.clone(),
    )?;
    let mut contradiction = match contradiction_rows.rows.first() {
        Some(row) => {
            let existing_resolution = resolution_from(&row[3]);
            if existing_resolution.is_some_and(|resolution| resolution != Resolution::Unresolved) {
                return Err(Error::Conflict(
                    "supersede contradiction already has a terminal resolution".into(),
                ));
            }
            let mut contradiction = Contradiction {
                between: commit.pair(),
                observations: u32::try_from(want_i64(&row[0])?)
                    .map_err(|_| backend_str("invalid contradiction observation count".into()))?,
                first_seen: u128::try_from(want_i64(&row[1])?)
                    .map_err(|_| backend_str("invalid contradiction first_seen".into()))?,
                last_seen: u128::try_from(want_i64(&row[2])?)
                    .map_err(|_| backend_str("invalid contradiction last_seen".into()))?,
                resolution: existing_resolution,
            };
            contradiction.observe(commit.applied_at);
            contradiction
        }
        None => Contradiction::new(commit.winner, commit.loser, commit.applied_at),
    };
    contradiction.resolve(Resolution::Superseded);

    let loser_lane_changed =
        TaggedPhysicalStatus::from(loser_node.status()) != TaggedPhysicalStatus::Archived;
    loser_node.set_status(NodeStatus::Archived);
    params.insert("data".into(), dv_str(&encode_canonical_node(&loser_node)?));
    params.insert("status".into(), dv_str(status_str(loser_node.status())));
    params.insert("summary".into(), dv_str(loser_node.summary()));
    params.insert("weight".into(), dv_float(1.0));
    params.insert("kind".into(), dv_str(edge_kind_str(EdgeKind::Supersedes)));
    params.insert(
        "applied_at".into(),
        dv_int(i64::try_from(commit.applied_at).map_err(|_| {
            Error::InvalidInput("supersede timestamp exceeds the storage integer range".into())
        })?),
    );
    params.insert(
        "observations".into(),
        dv_int(i64::from(contradiction.observations)),
    );
    params.insert(
        "first_seen".into(),
        dv_int(i64::try_from(contradiction.first_seen).map_err(|_| {
            Error::InvalidInput("contradiction first_seen exceeds storage range".into())
        })?),
    );
    params.insert(
        "last_seen".into(),
        dv_int(i64::try_from(contradiction.last_seen).map_err(|_| {
            Error::InvalidInput("contradiction last_seen exceeds storage range".into())
        })?),
    );
    params.insert(
        "resolution".into(),
        dv_str(resolution_str(Resolution::Superseded)),
    );

    // Every statement below belongs to the same Cozo transaction. The proof is
    // deliberately last, though any error still aborts all preceding writes.
    tx_run(
        tx,
        "?[from, to, weight, kind, last_reinforced, trials, interference] <- \
         [[$winner, $loser, $weight, $kind, $applied_at, 0, 0]] \
         :put edge {from, to => weight, kind, last_reinforced, trials, interference}",
        params.clone(),
    )?;
    tx_run(
        tx,
        "?[from, to] := *edge_anchor{from, to}, from == $winner, to == $loser \
         :rm edge_anchor {from, to}",
        params.clone(),
    )?;
    tx_run(
        tx,
        "?[id, data, status] <- [[$loser, $data, $status]] \
         :put node {id => data, status}",
        params.clone(),
    )?;
    tx_run(
        tx,
        "?[id, summary, status] <- [[$loser, $summary, $status]] \
         :put node_search {id => summary, status}",
        params.clone(),
    )?;
    tx_run(
        tx,
        "?[id, e, status] := *node_vec{id, e}, id == $loser, status = $status \
         :put node_vec {id => e, status}",
        params.clone(),
    )?;
    if loser_lane_changed {
        tx_sync_tag_projection(tx, &[&loser_node])?;
    }
    tx_run(
        tx,
        "?[lo, hi, observations, first_seen, last_seen, resolution] <- \
         [[$lo, $hi, $observations, $first_seen, $last_seen, $resolution]] \
         :put contradiction {lo, hi => observations, first_seen, last_seen, resolution}",
        params.clone(),
    )?;
    tx_run(
        tx,
        "?[lo, hi, winner, loser, applied_at] <- \
         [[$lo, $hi, $winner, $loser, $applied_at]] \
         :put supersede_commit {lo, hi => winner, loser, applied_at}",
        params,
    )?;
    Ok(SupersedeCommitOutcome::Applied)
}

fn supersede_endpoint_degree(tx: &MultiTransaction, endpoint: NodeId) -> Result<usize> {
    let mut params = BTreeMap::new();
    params.insert("endpoint".into(), dv_str(&endpoint.0.to_string()));
    params.insert("read_cap".into(), dv_int((MAX_INCIDENT_EDGES + 1) as i64));
    let rows = tx_run(
        tx,
        "incident[from, to] := *edge{from, to}, from == $endpoint\n\
         incident[from, to] := *edge{from, to}, to == $endpoint\n\
         ?[from, to] := incident[from, to] :limit $read_cap",
        params,
    )?;
    Ok(rows.rows.len())
}

fn full_merge_node(tx: &MultiTransaction, id: NodeId) -> Result<Node> {
    full_merge_optional_node(tx, id)?.ok_or(Error::NotFound)
}

fn full_merge_optional_node(tx: &MultiTransaction, id: NodeId) -> Result<Option<Node>> {
    let mut params = BTreeMap::new();
    params.insert("id".into(), dv_str(&id.0.to_string()));
    let rows = tx_run(
        tx,
        "?[id, data, status] := *node{id, data, status}, id == $id",
        params,
    )?;
    rows.rows
        .first()
        .map(|row| {
            let stored_id =
                graph_records::decode_canonical_node_id(&row[0], "full merge canonical node key")?;
            if stored_id != id {
                return Err(backend_str(
                    "full merge canonical node query returned the wrong key".into(),
                ));
            }
            decode_canonical_node_row(row)
        })
        .transpose()
}

fn full_merge_incident_edges(
    tx: &MultiTransaction,
    winner: NodeId,
    loser: NodeId,
) -> Result<Vec<Edge>> {
    let mut params = BTreeMap::new();
    params.insert("winner".into(), dv_str(&winner.0.to_string()));
    params.insert("loser".into(), dv_str(&loser.0.to_string()));
    params.insert(
        "read_cap".into(),
        dv_int((MAX_FULL_MERGE_INCIDENT_EDGES + 1) as i64),
    );
    let rows = tx_run(
        tx,
        "incident[from, to, weight, kind, lr, tr, intf] := \
           *edge{from: $winner, to, weight, kind, last_reinforced: lr, trials: tr, interference: intf}, from = $winner\n\
         incident[from, to, weight, kind, lr, tr, intf] := \
           *edge{to: $winner, from, weight, kind, last_reinforced: lr, trials: tr, interference: intf}, to = $winner\n\
         incident[from, to, weight, kind, lr, tr, intf] := \
           *edge{from: $loser, to, weight, kind, last_reinforced: lr, trials: tr, interference: intf}, from = $loser\n\
         incident[from, to, weight, kind, lr, tr, intf] := \
           *edge{to: $loser, from, weight, kind, last_reinforced: lr, trials: tr, interference: intf}, to = $loser\n\
         ?[from, to, weight, kind, lr, tr, intf] := \
           incident[from, to, weight, kind, lr, tr, intf] :limit $read_cap",
        params,
    )?;
    if rows.rows.len() > MAX_FULL_MERGE_INCIDENT_EDGES {
        return Err(Error::CapacityExceeded {
            resource: "full merge incident edge read",
            limit: MAX_FULL_MERGE_INCIDENT_EDGES,
        });
    }
    let mut edges = rows
        .rows
        .iter()
        .map(|row| {
            let from =
                graph_records::decode_canonical_node_id(&row[0], "full merge local edge source")?;
            let to =
                graph_records::decode_canonical_node_id(&row[1], "full merge local edge target")?;
            row_to_edge(from, to, &row[2..])
        })
        .collect::<Result<Vec<_>>>()?;
    preflight_full_merge_source_anchors(tx, winner, loser, &edges)?;
    let pairs = edges
        .iter()
        .map(|edge| (edge.from, edge.to))
        .collect::<Vec<_>>();
    if !pairs.is_empty() {
        let (input, anchor_params) = edge_pair_input("full_merge_anchor_pairs", &pairs);
        let anchors = tx_run(
            tx,
            &format!(
                "{input}\n?[from, to, start, end] := \
                 full_merge_anchor_pairs[from, to], *edge_anchor{{from, to, start, end}}"
            ),
            anchor_params,
        )?;
        let anchors = anchors
            .rows
            .iter()
            .map(|row| {
                Ok((
                    (
                        graph_records::decode_canonical_node_id(
                            &row[0],
                            "full merge edge anchor source",
                        )?,
                        graph_records::decode_canonical_node_id(
                            &row[1],
                            "full merge edge anchor target",
                        )?,
                    ),
                    graph_records::decode_body_span(&row[2], &row[3])?,
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        for edge in &mut edges {
            edge.anchor = anchors.get(&(edge.from, edge.to)).copied();
            edge.validate()
                .map_err(|error| backend_str(format!("invalid stored anchored edge: {error}")))?;
        }
    }
    Ok(edges)
}

fn preflight_full_merge_source_anchors(
    tx: &MultiTransaction,
    winner: NodeId,
    loser: NodeId,
    incident: &[Edge],
) -> Result<()> {
    let mut params = BTreeMap::new();
    params.insert("winner".into(), dv_str(&winner.0.to_string()));
    params.insert("loser".into(), dv_str(&loser.0.to_string()));
    params.insert(
        "read_cap".into(),
        dv_int((MAX_FULL_MERGE_INCIDENT_EDGES + 1) as i64),
    );
    let rows = tx_run(
        tx,
        "endpoint_anchor[from, to, start, end] := \
           *edge_anchor{from: $winner, to, start, end}, from = $winner\n\
         endpoint_anchor[from, to, start, end] := \
           *edge_anchor{from: $loser, to, start, end}, from = $loser\n\
         ?[from, to, start, end] := endpoint_anchor[from, to, start, end] :limit $read_cap",
        params,
    )?;
    if rows.rows.len() > MAX_FULL_MERGE_INCIDENT_EDGES {
        return Err(backend_str(
            "stored full merge endpoint anchors exceed the bounded incident domain".into(),
        ));
    }
    let edge_keys = incident
        .iter()
        .map(|edge| (edge.from, edge.to))
        .collect::<HashSet<_>>();
    for row in &rows.rows {
        let from =
            graph_records::decode_canonical_node_id(&row[0], "full merge endpoint anchor source")?;
        let to =
            graph_records::decode_canonical_node_id(&row[1], "full merge endpoint anchor target")?;
        graph_records::decode_body_span(&row[2], &row[3])?;
        if !edge_keys.contains(&(from, to)) {
            return Err(backend_str(
                "stored full merge endpoint anchor has no canonical edge".into(),
            ));
        }
    }
    Ok(())
}

fn full_merge_remote_edges(
    tx: &MultiTransaction,
    source_db: Ulid,
    from: NodeId,
) -> Result<Vec<RemoteEdge>> {
    let mut params = BTreeMap::new();
    params.insert("from".into(), dv_str(&from.0.to_string()));
    params.insert(
        "read_cap".into(),
        dv_int((MAX_REMOTE_EDGES_PER_SOURCE + 1) as i64),
    );
    let rows = tx_run(
        tx,
        "?[target_db, target, weight] := \
         *remote_edge{from: $from, target_db, target, weight} :limit $read_cap",
        params,
    )?;
    if rows.rows.len() > MAX_REMOTE_EDGES_PER_SOURCE {
        return Err(crate::remote_edge_source_capacity_error());
    }
    rows.rows
        .iter()
        .map(|row| graph_records::decode_remote_edge(source_db, from, &row[0], &row[1], &row[2]))
        .collect()
}

fn full_merge_edge_input(edges: &[Edge]) -> Result<(String, BTreeMap<String, DataValue>)> {
    let mut params = BTreeMap::new();
    let mut rows = Vec::with_capacity(edges.len());
    for (index, edge) in edges.iter().enumerate() {
        edge.validate().map_err(Error::InvalidInput)?;
        let anchor = edge.anchor;
        let values = [
            dv_str(&edge.from.0.to_string()),
            dv_str(&edge.to.0.to_string()),
            dv_float(edge.weight() as f64),
            dv_str(edge_kind_str(edge.kind)),
            dv_int(edge.last_reinforced() as i64),
            dv_int(edge.trials() as i64),
            dv_int(edge.interference() as i64),
            dv_int(i64::from(anchor.is_some())),
            dv_int(anchor.map_or(0, |span| span.start) as i64),
            dv_int(anchor.map_or(0, |span| span.end) as i64),
        ];
        let names = (0..values.len())
            .map(|column| format!("full_merge_edge_{index}_{column}"))
            .collect::<Vec<_>>();
        for (name, value) in names.iter().cloned().zip(values) {
            params.insert(name, value);
        }
        rows.push(format!(
            "[{}]",
            names
                .iter()
                .map(|name| format!("${name}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok((
        format!(
            "full_merge_edges[from, to, weight, kind, last_reinforced, trials, interference, ap, ast, aen] <- [{}]",
            rows.join(", ")
        ),
        params,
    ))
}

fn full_merge_remote_input(
    name: &str,
    source_db: Ulid,
    edges: &[RemoteEdge],
) -> Result<(String, BTreeMap<String, DataValue>)> {
    let mut params = BTreeMap::new();
    let mut rows = Vec::with_capacity(edges.len());
    for (index, edge) in edges.iter().enumerate() {
        edge.validate_for_source_database(source_db)
            .map_err(Error::InvalidInput)?;
        let names = [
            format!("full_merge_remote_from_{index}"),
            format!("full_merge_remote_db_{index}"),
            format!("full_merge_remote_target_{index}"),
            format!("full_merge_remote_weight_{index}"),
        ];
        let values = [
            dv_str(&edge.from.0.to_string()),
            dv_str(&edge.target_db.to_string()),
            dv_str(&edge.target.0.to_string()),
            dv_float(edge.weight() as f64),
        ];
        for (key, value) in names.iter().cloned().zip(values) {
            params.insert(key, value);
        }
        rows.push(format!(
            "[{}]",
            names
                .iter()
                .map(|key| format!("${key}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok((
        format!(
            "{name}[from, target_db, target, weight] <- [{}]",
            rows.join(", ")
        ),
        params,
    ))
}
