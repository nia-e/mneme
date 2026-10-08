use std::time::Duration;

use super::*;

pub(super) fn validate_primary_key_scan_page(
    relation: &str,
    limit: usize,
    page: PrimaryKeyScanPage,
) -> Result<PrimaryKeyScanPage> {
    if page.scanned != page.rows.rows.len() || page.scanned > limit {
        return Err(backend_str(format!(
            "primary-key scan contract violation for {relation}: decoded {}, returned {}, limit {limit}",
            page.scanned,
            page.rows.rows.len()
        )));
    }
    Ok(page)
}

pub(super) struct MaintenanceStageOutcome {
    pub(super) outcome: MaintenanceCommitOutcome,
    pub(super) statements: usize,
}

pub(super) fn run_maintenance_transaction(
    db: &DbInstance,
    commit: &MaintenanceCommit,
) -> Result<MaintenanceStageOutcome> {
    // SQLite can report or panic on BUSY at any script or at commit. Retry the
    // whole bounded transaction so every CAS is re-read from a fresh snapshot;
    // retrying only the failing statement would mix snapshots inside one chunk.
    let mut delay = Duration::from_millis(LOCK_RETRY_BASE_MS);
    for attempt in 0..=LOCK_RETRY_MAX {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let tx = db.multi_transaction(true);
            match stage_maintenance_transaction(&tx, commit) {
                Ok(staged) => {
                    tx.commit().map_err(backend)?;
                    Ok(staged)
                }
                Err(error) => {
                    let _ = tx.abort();
                    Err(error)
                }
            }
        }));
        let last = attempt == LOCK_RETRY_MAX;
        match outcome {
            Ok(Ok(staged)) => return Ok(staged),
            Ok(Err(error)) if !last && is_locked(&error) => {}
            Ok(Err(error)) => return Err(error),
            Err(panic) if panic_is_locked(panic.as_ref()) => {
                if last {
                    return Err(Error::Backend(
                        "database is locked: maintenance gave up after retrying".into(),
                    ));
                }
            }
            Err(panic) => std::panic::resume_unwind(panic),
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(LOCK_RETRY_CAP_MS));
    }
    unreachable!("the loop returns or re-raises on the final attempt")
}

fn stage_maintenance_transaction(
    tx: &MultiTransaction,
    commit: &MaintenanceCommit,
) -> Result<MaintenanceStageOutcome> {
    commit.validate()?;
    let edge_endpoints = commit
        .edges
        .iter()
        .flat_map(|mutation| {
            let key = mutation.key();
            [key.from, key.to]
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    graph::tx_reject_episode_nodes(tx, &edge_endpoints, "maintenance")?;
    let mut outcome = MaintenanceCommitOutcome::default();
    let mut statements = edge_endpoints.len().div_ceil(128);

    if !commit.edges.is_empty() {
        let current = maintenance_edges(tx, &commit.edges)?;
        statements += 2;
        let mut accepted = Vec::with_capacity(commit.edges.len());
        for mutation in &commit.edges {
            let key = mutation.key();
            let Some(edge) = current.get(&key) else {
                continue;
            };
            if !crate::same_serialized(edge, mutation.expected())? {
                continue;
            }
            accepted.push(mutation);
            outcome.applied_edges.push(key);
        }
        if !accepted.is_empty() {
            statements += write_maintenance_edges(tx, &accepted)?;
        }
    }

    Ok(MaintenanceStageOutcome {
        outcome,
        statements,
    })
}

/// Reconcile exact tag-v2 rows for a bounded set of canonical nodes inside an
/// existing write transaction. Callers pass only physical lifecycle changes;
/// same-lane feedback/maintenance therefore executes zero tag statements.
pub(super) fn tx_sync_tag_projection(tx: &MultiTransaction, nodes: &[&Node]) -> Result<usize> {
    if nodes.is_empty() {
        return Ok(0);
    }
    let mut params = BTreeMap::new();
    let mut moving = Vec::with_capacity(nodes.len());
    let mut desired = Vec::new();
    for (node_index, node) in nodes.iter().enumerate() {
        let id_name = format!("tag_sync_id_{node_index}");
        params.insert(id_name.clone(), dv_str(&node.id().0.to_string()));
        moving.push(format!("[${id_name}]"));
        let status = TaggedPhysicalStatus::from(node.status());
        let sample_hash = stable_tag_sample_hash(node.id());
        for (tag_index, tag) in node.tags().enumerate() {
            let tag_name = format!("tag_sync_tag_{node_index}_{tag_index}");
            params.insert(tag_name.clone(), dv_str(tag));
            desired.push(format!(
                "[${tag_name}, '{}', {sample_hash}, ${id_name}]",
                status.as_str()
            ));
        }
    }
    let moving_rule = format!("tag_sync_node[id] <- [{}]", moving.join(", "));
    let mut statements = 0;
    if desired.is_empty() {
        tx_run(
            tx,
            &format!(
                "{moving_rule}\n\
                 ?[tag, status, sample_hash, id] := tag_sync_node[id], \
                   *node_tag_v2:by_id{{id, tag, status, sample_hash}} \
                   :rm node_tag_v2 {{tag, status, sample_hash, id}}"
            ),
            params,
        )?;
        return Ok(1);
    }

    let desired_rule = format!(
        "tag_sync_desired[tag, status, sample_hash, id] <- [{}]",
        desired.join(", ")
    );
    tx_run(
        tx,
        &format!(
            "{moving_rule}\n{desired_rule}\n\
             ?[tag, status, sample_hash, id] := tag_sync_node[id], \
               *node_tag_v2:by_id{{id, tag, status, sample_hash}}, \
               not tag_sync_desired[tag, status, sample_hash, id] \
               :rm node_tag_v2 {{tag, status, sample_hash, id}}"
        ),
        params.clone(),
    )?;
    statements += 1;
    tx_run(
        tx,
        &format!(
            "{desired_rule}\n\
             ?[tag, status, sample_hash, id] := \
               tag_sync_desired[tag, status, sample_hash, id], \
               not *node_tag_v2{{tag, status, sample_hash, id}} \
               :put node_tag_v2 {{tag, status, sample_hash, id}}"
        ),
        params,
    )?;
    statements += 1;
    Ok(statements)
}

fn maintenance_edges(
    tx: &MultiTransaction,
    mutations: &[MaintenanceEdgeMutation],
) -> Result<HashMap<MaintenanceEdgeKey, Edge>> {
    let pairs = mutations
        .iter()
        .map(|mutation| {
            let key = mutation.key();
            (key.from, key.to)
        })
        .collect::<Vec<_>>();
    let (input, params) = edge_pair_input("maintenance_edge_key", &pairs);
    let rows = tx_run(tx, &maintenance_edge_read_query(&input), params)?;
    let mut current = HashMap::with_capacity(rows.rows.len());
    for row in &rows.rows {
        let key =
            MaintenanceEdgeKey::new(node_id(want_str(&row[0])?)?, node_id(want_str(&row[1])?)?);
        let edge = row_to_edge(key.from, key.to, &row[2..])?;
        if current.insert(key, edge).is_some() {
            return Err(backend_str(format!(
                "duplicate edge {key:?} returned during maintenance commit"
            )));
        }
    }

    let (input, params) = edge_pair_input("maintenance_anchor_key", &pairs);
    let anchors = tx_run(tx, &maintenance_anchor_read_query(&input), params)?;
    for row in &anchors.rows {
        let key =
            MaintenanceEdgeKey::new(node_id(want_str(&row[0])?)?, node_id(want_str(&row[1])?)?);
        if let Some(edge) = current.get_mut(&key) {
            edge.anchor = Some(BodySpan::new(
                u32::try_from(want_i64(&row[2])?)
                    .map_err(|_| backend_str("negative/oversized edge anchor start".into()))?,
                u32::try_from(want_i64(&row[3])?)
                    .map_err(|_| backend_str("negative/oversized edge anchor end".into()))?,
            ));
        }
    }
    for edge in current.values() {
        edge.validate().map_err(Error::InvalidInput)?;
    }
    Ok(current)
}

fn write_maintenance_edges(
    tx: &MultiTransaction,
    mutations: &[&MaintenanceEdgeMutation],
) -> Result<usize> {
    let mut params = BTreeMap::new();
    let mut replacements = Vec::new();
    let mut deletions = Vec::new();
    for (index, mutation) in mutations.iter().enumerate() {
        match mutation {
            MaintenanceEdgeMutation::Decay { replacement, .. } => {
                let from = format!("maintenance_edge_from_{index}");
                let to = format!("maintenance_edge_to_{index}");
                let weight = format!("maintenance_edge_weight_{index}");
                let kind = format!("maintenance_edge_kind_{index}");
                let reinforced = format!("maintenance_edge_reinforced_{index}");
                let trials = format!("maintenance_edge_trials_{index}");
                let interference = format!("maintenance_edge_interference_{index}");
                params.insert(from.clone(), dv_str(&replacement.from.0.to_string()));
                params.insert(to.clone(), dv_str(&replacement.to.0.to_string()));
                params.insert(weight.clone(), dv_float(replacement.weight() as f64));
                params.insert(kind.clone(), dv_str(edge_kind_str(replacement.kind)));
                params.insert(
                    reinforced.clone(),
                    dv_int(i64::try_from(replacement.last_reinforced()).map_err(|_| {
                        Error::InvalidInput(
                            "edge timestamp exceeds the storage integer range".into(),
                        )
                    })?),
                );
                params.insert(trials.clone(), dv_int(i64::from(replacement.trials())));
                params.insert(
                    interference.clone(),
                    dv_int(i64::from(replacement.interference())),
                );
                replacements.push(format!(
                    "[${from}, ${to}, ${weight}, ${kind}, ${reinforced}, ${trials}, ${interference}]"
                ));
            }
            MaintenanceEdgeMutation::DeleteWeak { expected } => {
                let from = format!("maintenance_delete_from_{index}");
                let to = format!("maintenance_delete_to_{index}");
                params.insert(from.clone(), dv_str(&expected.from.0.to_string()));
                params.insert(to.clone(), dv_str(&expected.to.0.to_string()));
                deletions.push(format!("[${from}, ${to}]"));
            }
        }
    }

    let mut statements = 0;
    if !replacements.is_empty() {
        tx_run(
            tx,
            &format!(
                "maintenance_edge_replacement[from, to, weight, kind, lr, trials, interference] <- [{}]\n\
             ?[from, to, weight, kind, lr, trials, interference] := \
               maintenance_edge_replacement[from, to, weight, kind, lr, trials, interference] \
               :put edge {{from, to => weight, kind, last_reinforced: lr, trials, interference}}",
                replacements.join(", ")
            ),
            params.clone(),
        )?;
        statements += 1;
    }
    if !deletions.is_empty() {
        let rows = deletions.join(", ");
        tx_run(
            tx,
            &format!(
                "maintenance_edge_delete[from, to] <- [{rows}]\n\
             ?[from, to] := maintenance_edge_delete[from, to], *edge_anchor{{from, to}} \
               :rm edge_anchor {{from, to}}"
            ),
            params.clone(),
        )?;
        tx_run(
            tx,
            &format!(
                "maintenance_edge_delete[from, to] <- [{rows}]\n\
             ?[from, to] := maintenance_edge_delete[from, to], *edge{{from, to}} \
               :rm edge {{from, to}}"
            ),
            params,
        )?;
        statements += 2;
    }
    Ok(statements)
}

pub(super) fn stage_dense_prune_transaction(
    tx: &MultiTransaction,
    hub: NodeId,
    target_degree: usize,
    max_deletes: usize,
) -> Result<DensePruneChunkOutcome> {
    if !(1..=MAX_MAINTENANCE_BATCH_ROWS).contains(&max_deletes) {
        return Err(Error::InvalidInput(format!(
            "maintenance prune limit must be in 1..={MAX_MAINTENANCE_BATCH_ROWS}"
        )));
    }
    graph::tx_reject_episode_nodes(tx, &[hub], "dense pruning")?;
    let mut params = BTreeMap::new();
    params.insert("hub".into(), dv_str(&hub.0.to_string()));
    params.insert("read_cap".into(), dv_int((MAX_INCIDENT_EDGES + 1) as i64));
    let rows = tx_run(
        tx,
        "incident[from, to, weight, kind, lr, trials, interference] := \
           *edge{from, to, weight, kind, last_reinforced: lr, trials, interference}, from == $hub\n\
         incident[from, to, weight, kind, lr, trials, interference] := \
           *edge{from, to, weight, kind, last_reinforced: lr, trials, interference}, to == $hub\n\
         ?[from, to, weight, kind, lr, trials, interference] := \
           incident[from, to, weight, kind, lr, trials, interference] :limit $read_cap",
        params,
    )?;
    if rows.rows.len() > MAX_INCIDENT_EDGES {
        return Err(crate::incident_edge_capacity_error());
    }
    let mut associations = Vec::new();
    for row in &rows.rows {
        let from = node_id(want_str(&row[0])?)?;
        let to = node_id(want_str(&row[1])?)?;
        let edge = row_to_edge(from, to, &row[2..])?;
        if edge.kind.is_undirected() {
            associations.push(edge);
        }
    }
    // Evidence links retain authored weights even when the semantic endpoint is
    // a dense hub. They also do not consume its semantic target-degree budget.
    let endpoints = associations
        .iter()
        .flat_map(|edge| [edge.from, edge.to])
        .collect::<Vec<_>>();
    let episodes = graph::tx_episode_ids(tx, &endpoints)?;
    associations.retain(|edge| !episodes.contains(&edge.from) && !episodes.contains(&edge.to));
    associations.sort_by(|left, right| {
        left.weight()
            .total_cmp(&right.weight())
            .then_with(|| left.trials().cmp(&right.trials()))
            .then_with(|| left.from.cmp(&right.from))
            .then_with(|| left.to.cmp(&right.to))
    });
    let excess = associations.len().saturating_sub(target_degree);
    let pruned = excess.min(max_deletes);
    let keys = associations
        .into_iter()
        .take(pruned)
        .map(|edge| (edge.from, edge.to))
        .collect::<Vec<_>>();
    if !keys.is_empty() {
        let (input, params) = edge_pair_input("dense_prune_edges", &keys);
        tx_run(
            tx,
            &format!(
                "{input}\n\
                 ?[from, to] := dense_prune_edges[from, to], *edge_anchor{{from, to}} \
                   :rm edge_anchor {{from, to}}"
            ),
            params.clone(),
        )?;
        tx_run(
            tx,
            &format!(
                "{input}\n\
                 ?[from, to] := dense_prune_edges[from, to], *edge{{from, to}} \
                   :rm edge {{from, to}}"
            ),
            params,
        )?;
    }
    Ok(DensePruneChunkOutcome {
        pruned,
        remaining_excess: excess.saturating_sub(pruned),
    })
}
