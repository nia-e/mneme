//! Atomic, idempotent insertion of an externally captured node.
//!
//! Ordinary `put_node` and vector `upsert` deliberately retain their existing
//! contracts. Capture needs a stronger one: its acknowledgement covers the
//! canonical node and every searchable projection in one database transaction.

use super::runtime::LOCK_RETRY_WAIT_CEILING_MS;
use super::*;
use crate::storage_contract::conventional_unmanaged::spec::{
    CONCERN_V1_CATALOG_GENERATION_MARKER, EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER,
    SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER, TOUCHSTONES_V1_CATALOG_GENERATION_MARKER,
};
use mneme_core::ports::{CaptureCommitOutcome, CaptureSimilarityPrior, MAX_CAPTURE_EDGES};
use mneme_core::{CaptureReplayProof, Provenance};

impl CozoStore {
    pub(super) async fn lookup_capture_atomic(
        &self,
        proof: &CaptureReplayProof,
    ) -> Result<Option<NodeId>> {
        let proof = proof.clone();
        let dim = self.dim;
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        tokio::task::spawn_blocking(move || job.run(|db| lookup_capture_sync(db, &proof, dim)))
            .await
            .map_err(|error| backend_str(format!("join capture lookup worker: {error}")))?
    }

    pub(super) async fn commit_capture_atomic(
        &self,
        node: &Node,
        embedding: &[f32],
        edges: &[Edge],
        priors: &[CaptureSimilarityPrior],
        generated_link_budget: mneme_core::ports::CapturePriorBudget,
        proof: &CaptureReplayProof,
        touchstone: Option<&TouchstoneInput>,
    ) -> Result<CaptureCommitOutcome> {
        if !node.is_semantic() {
            return Err(Error::InvalidInput(
                "use the episode port to append an episode edition".into(),
            ));
        }
        mneme_core::ports::validate_capture_edges(node.id(), edges)?;
        let source = match node.provenance() {
            Provenance::External { source } => source.clone(),
            _ => {
                return Err(Error::InvalidInput(
                    "capture node requires external provenance".into(),
                ));
            }
        };
        if source != *proof.source() {
            return Err(Error::InvalidInput(
                "capture node provenance differs from replay proof".into(),
            ));
        }
        let proof = proof.clone();
        let touchstone = touchstone.cloned();
        if source.node_id() != node.id() {
            return Err(Error::InvalidInput(
                "capture node ID does not match its source key".into(),
            ));
        }
        if embedding.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: embedding.len(),
            });
        }
        crate::validate_cosine_vector(embedding, "capture vector embedding")?;
        let canonical = encode_canonical_node(node)?;
        let node = node.clone();
        let embedding = embedding.to_vec();
        let edges = edges.to_vec();
        let priors = priors
            .iter()
            .take(mneme_core::ports::MAX_CAPTURE_PRIOR_CANDIDATES + 1)
            .cloned()
            .collect::<Vec<_>>();
        let dim = self.dim;
        // The guard belongs in the detached worker, not in the cancellable
        // future. A lost acknowledgement can therefore race a still-running
        // commit safely: the next attempt observes its durable marker.
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        tokio::task::spawn_blocking(move || {
            job.run(|db| {
                commit_capture_sync(
                    db,
                    &node,
                    &proof,
                    &canonical,
                    &embedding,
                    &edges,
                    &priors,
                    generated_link_budget,
                    dim,
                    touchstone.as_ref(),
                )
            })
        })
        .await
        .map_err(|error| backend_str(format!("join capture commit worker: {error}")))?
    }
}

fn lookup_capture_sync(
    db: &DbInstance,
    proof: &CaptureReplayProof,
    dim: usize,
) -> Result<Option<NodeId>> {
    let tx = db.multi_transaction(false);
    ensure_capture_generation(&tx)?;
    let id = proof.source().node_id();
    let mut params = BTreeMap::new();
    params.insert("id".into(), dv_str(&id.0.to_string()));
    let rows = tx_run(
        &tx,
        "?[id, data, status] := *node{id, data, status}, id == $id",
        params,
    )?;
    let result = if let Some(row) = rows.rows.first() {
        let existing = decode_canonical_node_row(row)?;
        match existing.provenance() {
            Provenance::External { source: stored }
                if proof.matches_source(stored) && existing.is_semantic() =>
            {
                verify_capture_projections(&tx, &existing, dim).map_err(|error| {
                    Error::Conflict(format!(
                        "capture replay has incomplete projections: {error}"
                    ))
                })?;
                touchstones::tx_verify_replay(&tx, &existing, None)?;
                Some(id)
            }
            _ => {
                return Err(Error::Conflict(format!(
                    "capture source key collides with existing node {}",
                    id.0
                )));
            }
        }
    } else {
        None
    };
    tx.commit().map_err(backend)?;
    Ok(result)
}

fn commit_capture_sync(
    db: &DbInstance,
    node: &Node,
    proof: &CaptureReplayProof,
    canonical: &str,
    embedding: &[f32],
    edges: &[Edge],
    priors: &[CaptureSimilarityPrior],
    generated_link_budget: mneme_core::ports::CapturePriorBudget,
    dim: usize,
    touchstone: Option<&TouchstoneInput>,
) -> Result<CaptureCommitOutcome> {
    let mut delay = std::time::Duration::from_millis(LOCK_RETRY_BASE_MS);
    for attempt in 0..=LOCK_RETRY_MAX {
        // SQLite can return *or panic* on BUSY. A panicked MultiTransaction
        // rolls back on drop; retry the whole guard/read/write transaction,
        // never only a statement whose observation might now be stale.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let tx = db.multi_transaction(true);
            let staged = stage_capture(
                &tx,
                node,
                proof,
                canonical,
                embedding,
                edges,
                priors,
                generated_link_budget,
                dim,
                touchstone,
            );
            match staged {
                Ok(outcome) => tx.commit().map_err(backend).map(|()| outcome),
                Err(error) => {
                    let _ = tx.abort();
                    Err(error)
                }
            }
        }));
        match result {
            Ok(Ok(outcome)) => return Ok(outcome),
            Ok(Err(error)) if is_locked(&error) => {
                if attempt == LOCK_RETRY_MAX {
                    return Err(backend_str(format!(
                        "database is locked: capture gave up after at most {LOCK_RETRY_WAIT_CEILING_MS} ms of contention wait"
                    )));
                }
            }
            Ok(Err(error)) => return Err(error),
            Err(panic) if panic_is_locked(panic.as_ref()) => {
                if attempt == LOCK_RETRY_MAX {
                    return Err(backend_str(format!(
                        "database is locked: capture gave up after at most {LOCK_RETRY_WAIT_CEILING_MS} ms of contention wait"
                    )));
                }
            }
            Err(panic) => std::panic::resume_unwind(panic),
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(std::time::Duration::from_millis(LOCK_RETRY_CAP_MS));
    }
    unreachable!("the final capture attempt returns or re-raises")
}

fn stage_capture(
    tx: &MultiTransaction,
    node: &Node,
    proof: &CaptureReplayProof,
    canonical: &str,
    embedding: &[f32],
    edges: &[Edge],
    priors: &[CaptureSimilarityPrior],
    generated_link_budget: mneme_core::ports::CapturePriorBudget,
    dim: usize,
    touchstone: Option<&TouchstoneInput>,
) -> Result<CaptureCommitOutcome> {
    let id = node.id().0.to_string();
    // Refuse an ordinary generation without even a no-op metadata write.
    // Recheck after the writer lock below; that second read is the commit fence.
    ensure_capture_generation(tx)?;
    // The no-op metadata put takes SQLite's writer lock before the key read.
    // Two independent Cozo handles must not both decide that the ID is empty.
    tx.run_script(
        "?[k, v] := *meta{k, v}, k == 'db_id' :put meta {k => v}",
        BTreeMap::new(),
    )
    .map_err(|error| {
        if is_locked(&error) {
            backend_str(format!("database is locked: {error:?}"))
        } else {
            backend(error)
        }
    })?;
    ensure_capture_generation(tx)?;
    pause_capture_for_tests(node.id());
    let mut params = BTreeMap::new();
    params.insert("id".into(), dv_str(&id));
    let rows = tx_run(
        tx,
        "?[id, data, status] := *node{id, data, status}, id == $id",
        params.clone(),
    )?;
    if let Some(row) = rows.rows.first() {
        let existing = decode_canonical_node_row(row)?;
        match existing.provenance() {
            Provenance::External { source: stored }
                if proof.matches_source(stored) && existing.is_semantic() =>
            {
                verify_capture_projections(tx, &existing, dim).map_err(|error| {
                    Error::Conflict(format!(
                        "capture replay has incomplete projections: {error}"
                    ))
                })?;
                touchstones::tx_verify_replay(tx, &existing, touchstone)?;
                return Ok(CaptureCommitOutcome::AlreadyApplied);
            }
            _ => {
                return Err(Error::Conflict(format!(
                    "capture source key collides with existing node {id}"
                )));
            }
        }
    }

    // The writer lock above makes these observations and the later insert one
    // serializable unit. Replay exits before checking targets: removed links
    // are learned state, not missing capture projections.
    mneme_core::ports::validate_capture_prior_candidates(priors)?;
    validate_authored_links(tx, node.id(), edges)?;
    let admitted = admit_similarity_priors(tx, node.id(), edges, priors, generated_link_budget)?;

    let record = touchstones::tx_prepare_record(tx, node, touchstone)?;
    params.insert("data".into(), dv_str(canonical));
    params.insert("status".into(), dv_str(status_str(node.status())));
    tx_run(
        tx,
        "?[id, data, status] <- [[$id, $data, $status]] :put node {id => data, status}",
        params.clone(),
    )?;
    maybe_fail_capture(node.id(), 1)?;
    params.insert("e".into(), dv_float_list(embedding));
    tx_run(
        tx,
        "?[id, e, status] := id = $id, e = vec($e), status = $status \
         :put node_vec {id => e, status}",
        params.clone(),
    )?;
    maybe_fail_capture(node.id(), 2)?;
    params.insert("summary".into(), dv_str(node.summary()));
    tx_run(
        tx,
        "?[id, summary, status] <- [[$id, $summary, $status]] \
         :put node_search {id => summary, status}",
        params,
    )?;
    maybe_fail_capture(node.id(), 3)?;
    maintenance::tx_sync_tag_projection(tx, &[node])?;
    maybe_fail_capture(node.id(), 4)?;
    write_authored_links(tx, node.id(), &admitted)?;
    if let Some(record) = record {
        touchstones::tx_write_record(tx, &record)?;
    }
    verify_capture_projections(tx, node, dim)?;
    Ok(CaptureCommitOutcome::Applied)
}

// Both degree dimensions are read from the indexed, storage-bounded incident
// set under the capture writer transaction. No engine-owned degree snapshot.
fn incident_count(tx: &MultiTransaction, id: NodeId) -> Result<usize> {
    let mut p = BTreeMap::new();
    p.insert("id".into(), dv_str(&id.0.to_string()));
    p.insert("cap".into(), dv_int((MAX_INCIDENT_EDGES + 1) as i64));
    let rows = tx_run(
        tx,
        "inc[from, to] := *edge{from, to}, from == $id\ninc[from, to] := *edge{from, to}, to == $id\n?[from, to] := inc[from, to] :limit $cap",
        p,
    )?;
    if rows.rows.len() > MAX_INCIDENT_EDGES {
        return Err(crate::incident_edge_capacity_error());
    }
    Ok(rows.rows.len())
}

fn admit_similarity_priors(
    tx: &MultiTransaction,
    id: NodeId,
    authored: &[Edge],
    priors: &[CaptureSimilarityPrior],
    generated_link_budget: mneme_core::ports::CapturePriorBudget,
) -> Result<Vec<Edge>> {
    let mut admitted = authored.to_vec();
    if priors.is_empty()
        || generated_link_budget.limit() == 0
        || authored.len() == MAX_CAPTURE_EDGES
    {
        return Ok(admitted);
    }
    let source_total = incident_count(tx, id)?;
    let limit = generated_link_budget
        .limit()
        .min(MAX_CAPTURE_EDGES.saturating_sub(authored.len()));
    for prior in priors {
        if admitted.len() - authored.len() >= limit {
            break;
        }
        let edge = prior.edge();
        if edge.from != id
            || admitted.iter().any(|required| required.to == edge.to)
            || source_total.saturating_add(admitted.len()) >= MAX_INCIDENT_EDGES
        {
            continue;
        }
        let mut p = BTreeMap::new();
        p.insert("id".into(), dv_str(&edge.to.0.to_string()));
        let target = tx_run(
            tx,
            "?[id, data, status] := *node{id, data, status}, id == $id",
            p,
        )?;
        let Some(row) = target.rows.first() else {
            continue;
        };
        if !prior.matches_target(&decode_canonical_node_row(row)?) {
            continue;
        }
        let mut p = BTreeMap::new();
        p.insert("from".into(), dv_str(&id.0.to_string()));
        p.insert("to".into(), dv_str(&edge.to.0.to_string()));
        if !tx_run(
            tx,
            "?[from, to] := *edge{from, to}, from == $from, to == $to",
            p,
        )?
        .rows
        .is_empty()
        {
            continue;
        }
        let total = incident_count(tx, edge.to)?;
        if total >= MAX_INCIDENT_EDGES {
            continue;
        }
        admitted.push(edge.clone());
    }
    Ok(admitted)
}

/// Shared atomic authored-link kernel for semantic captures and episode editions.
pub(super) fn validate_authored_links(
    tx: &MultiTransaction,
    id_value: NodeId,
    edges: &[Edge],
) -> Result<()> {
    mneme_core::ports::validate_capture_edges(id_value, edges)?;
    let id = id_value.0.to_string();
    if !edges.is_empty() {
        let mut p = BTreeMap::new();
        p.insert("id".into(), dv_str(&id));
        p.insert("cap".into(), dv_int((MAX_INCIDENT_EDGES + 1) as i64));
        let incident = tx_run(
            tx,
            "inc[from, to] := *edge{from, to}, from == $id\ninc[from, to] := *edge{from, to}, to == $id\n?[from, to] := inc[from, to] :limit $cap",
            p,
        )?;
        if incident.rows.len().saturating_add(edges.len()) > MAX_INCIDENT_EDGES {
            return Err(crate::incident_edge_capacity_error());
        }
    }
    for edge in edges {
        let mut p = BTreeMap::new();
        p.insert("to".into(), dv_str(&edge.to.0.to_string()));
        let target = tx_run(
            tx,
            "?[target_id] := *node{id: target_id}, target_id == $to",
            p.clone(),
        )?;
        if target.rows.is_empty() {
            return Err(Error::InvalidInput(format!(
                "capture edge target {} does not exist",
                edge.to.0
            )));
        }
        p.insert("from".into(), dv_str(&id));
        let existing = tx_run(
            tx,
            "?[edge_from, edge_to] := *edge{from: edge_from, to: edge_to}, edge_from == $from, edge_to == $to",
            p.clone(),
        )?;
        if !existing.rows.is_empty() {
            return Err(Error::Conflict(format!(
                "capture edge {id} -> {} already exists",
                edge.to.0
            )));
        }
        // Each distinct target gets exactly one new incident edge.
        p.insert("cap".into(), dv_int((MAX_INCIDENT_EDGES + 1) as i64));
        let incident = tx_run(
            tx,
            "inc[from, to] := *edge{from, to}, from == $to\ninc[from, to] := *edge{from, to}, to == $to\n?[from, to] := inc[from, to] :limit $cap",
            p,
        )?;
        if incident.rows.len() >= MAX_INCIDENT_EDGES {
            return Err(crate::incident_edge_capacity_error());
        }
    }

    Ok(())
}

pub(super) fn write_authored_links(
    tx: &MultiTransaction,
    id_value: NodeId,
    edges: &[Edge],
) -> Result<()> {
    let id = id_value.0.to_string();
    for edge in edges {
        let last_reinforced = i64::try_from(edge.last_reinforced()).map_err(|_| {
            Error::InvalidInput("edge timestamp exceeds storage integer range".into())
        })?;
        let mut p = BTreeMap::new();
        p.insert("from".into(), dv_str(&id));
        p.insert("to".into(), dv_str(&edge.to.0.to_string()));
        p.insert("weight".into(), dv_float(edge.weight() as f64));
        p.insert("kind".into(), dv_str(edge_kind_str(edge.kind)));
        p.insert("last_reinforced".into(), dv_int(last_reinforced));
        p.insert("trials".into(), dv_int(i64::from(edge.trials())));
        p.insert(
            "interference".into(),
            dv_int(i64::from(edge.interference())),
        );
        tx_run(
            tx,
            "?[from, to, weight, kind, last_reinforced, trials, interference] <- [[$from, $to, $weight, $kind, $last_reinforced, $trials, $interference]] :put edge {from, to => weight, kind, last_reinforced, trials, interference}",
            p.clone(),
        )?;
        if let Some(span) = edge.anchor {
            p.insert("start".into(), dv_int(i64::from(span.start)));
            p.insert("end".into(), dv_int(i64::from(span.end)));
            tx_run(
                tx,
                "?[from, to, start, end] <- [[$from, $to, $start, $end]] :put edge_anchor {from, to => start, end}",
                p,
            )?;
        }
        maybe_fail_capture(id_value, 5)?;
    }
    Ok(())
}

#[cfg(test)]
#[derive(Default)]
struct CaptureFailureHook {
    armed: std::sync::Mutex<BTreeMap<NodeId, usize>>,
}

#[cfg(test)]
fn capture_failure_hook() -> &'static CaptureFailureHook {
    static HOOK: std::sync::OnceLock<CaptureFailureHook> = std::sync::OnceLock::new();
    HOOK.get_or_init(CaptureFailureHook::default)
}

#[cfg(test)]
pub(super) fn arm_capture_failure(id: NodeId, after_step: usize) {
    assert!((1..=7).contains(&after_step));
    let mut armed = capture_failure_hook().armed.lock().unwrap();
    assert!(
        armed.insert(id, after_step).is_none(),
        "capture failure hook already armed for node"
    );
}

#[cfg(test)]
pub(super) fn maybe_fail_capture(id: NodeId, after_step: usize) -> Result<()> {
    let mut armed = capture_failure_hook().armed.lock().unwrap();
    if armed.get(&id) == Some(&after_step) {
        armed.remove(&id);
        return Err(backend_str(format!(
            "injected capture failure after transaction step {after_step}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
pub(super) fn maybe_fail_capture(_id: NodeId, _after_step: usize) -> Result<()> {
    Ok(())
}

#[cfg(test)]
#[derive(Default)]
struct CapturePauseState {
    target: Option<NodeId>,
    entered: bool,
    released: bool,
}

#[cfg(test)]
#[derive(Default)]
struct CapturePauseHook {
    state: std::sync::Mutex<CapturePauseState>,
    wake: std::sync::Condvar,
}

#[cfg(test)]
fn capture_pause_hook() -> &'static CapturePauseHook {
    static HOOK: std::sync::OnceLock<CapturePauseHook> = std::sync::OnceLock::new();
    HOOK.get_or_init(CapturePauseHook::default)
}

#[cfg(test)]
pub(super) fn arm_capture_pause(id: NodeId) {
    let hook = capture_pause_hook();
    let mut state = hook.state.lock().unwrap();
    assert!(state.target.is_none(), "capture pause already armed");
    *state = CapturePauseState {
        target: Some(id),
        ..CapturePauseState::default()
    };
}

#[cfg(test)]
pub(super) fn wait_for_capture_pause() {
    let hook = capture_pause_hook();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut state = hook.state.lock().unwrap();
    while !state.entered {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "capture worker did not reach pause");
        let (next, result) = hook.wake.wait_timeout(state, remaining).unwrap();
        state = next;
        assert!(
            !result.timed_out() || state.entered,
            "capture pause timed out"
        );
    }
}

#[cfg(test)]
pub(super) fn release_capture_pause() {
    let hook = capture_pause_hook();
    let mut state = hook.state.lock().unwrap();
    assert!(state.entered, "capture pause was never entered");
    state.released = true;
    hook.wake.notify_all();
}

#[cfg(test)]
fn pause_capture_for_tests(id: NodeId) {
    let hook = capture_pause_hook();
    let mut state = hook.state.lock().unwrap();
    if state.target != Some(id) {
        return;
    }
    state.entered = true;
    hook.wake.notify_all();
    while !state.released {
        state = hook.wake.wait(state).unwrap();
    }
    *state = CapturePauseState::default();
    hook.wake.notify_all();
}

#[cfg(not(test))]
fn pause_capture_for_tests(_id: NodeId) {}

/// Fresh capture admission is the *existing generation cell*, not an additive
/// marker an older writer could ignore. The writer path calls this after taking
/// its SQLite writer lock; the cheap lookup checks it in the same read snapshot.
fn ensure_capture_generation(tx: &MultiTransaction) -> Result<()> {
    let mut params = BTreeMap::new();
    params.insert("key".into(), dv_str(VECTOR_PROJECTION_META_KEY));
    let rows = tx_run(tx, "?[v] := *meta{k: $key, v}", params)?;
    match rows.rows.as_slice() {
        [row]
            if matches!(
                want_str(&row[0])?,
                SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER
                    | CONCERN_V1_CATALOG_GENERATION_MARKER
                    | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                    | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
            ) =>
        {
            Ok(())
        }
        _ => Err(Error::InvalidInput(
            "capture requires a fresh capture-enabled store; create a separate database".into(),
        )),
    }
}

fn verify_capture_projections(tx: &MultiTransaction, node: &Node, dim: usize) -> Result<()> {
    let id = node.id().0.to_string();
    let mut params = BTreeMap::new();
    params.insert("id".into(), dv_str(&id));
    let vector = tx_run(
        tx,
        "?[e, status] := *node_vec{id: $id, e, status}",
        params.clone(),
    )?;
    let Some(row) = vector.rows.first() else {
        return Err(backend_str(format!(
            "capture node {id} has no vector projection"
        )));
    };
    let stored = want_vector(&row[0])?;
    if stored.len() != dim || want_str(&row[1])? != status_str(node.status()) {
        return Err(backend_str(format!(
            "capture node {id} has inconsistent vector projection"
        )));
    }
    crate::validate_cosine_vector(&stored, "stored capture vector embedding")?;

    let lexical = tx_run(
        tx,
        "?[summary, status] := *node_search{id: $id, summary, status}",
        params.clone(),
    )?;
    let Some(row) = lexical.rows.first() else {
        return Err(backend_str(format!(
            "capture node {id} has no lexical projection"
        )));
    };
    if want_str(&row[0])? != node.summary() || want_str(&row[1])? != status_str(node.status()) {
        return Err(backend_str(format!(
            "capture node {id} has inconsistent lexical projection"
        )));
    }

    params.insert("cap".into(), dv_int((mneme_core::MAX_NODE_TAGS + 1) as i64));
    let tags = tx_run(
        tx,
        "?[tag, status, sample_hash] := \
         *node_tag_v2:by_id{id: $id, tag, status, sample_hash} :limit $cap",
        params,
    )?;
    if tags.rows.len() > mneme_core::MAX_NODE_TAGS {
        return Err(backend_str(format!(
            "capture node {id} exceeds tag projection bound"
        )));
    }
    let mut actual = BTreeSet::new();
    for row in &tags.rows {
        if want_str(&row[1])? != status_str(node.status())
            || want_i64(&row[2])? != stable_tag_sample_hash(node.id())
            || !actual.insert(want_str(&row[0])?.to_owned())
        {
            return Err(backend_str(format!(
                "capture node {id} has malformed tag projection"
            )));
        }
    }
    let expected = node.tags().map(str::to_owned).collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(backend_str(format!(
            "capture node {id} has incomplete tag projection"
        )));
    }
    Ok(())
}
