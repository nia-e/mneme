use super::graph_records::{
    decode_canonical_node_id, decode_storage_timestamp, decode_storage_u32,
    encode_storage_timestamp,
};
use super::*;

#[cfg(test)]
use std::sync::{Condvar, Mutex, OnceLock};

impl CozoStore {
    pub(super) fn upsert_contradiction(&self, contradiction: &Contradiction) -> Result<()> {
        contradiction.validate().map_err(Error::InvalidInput)?;
        let tx = self.db.multi_transaction(true);
        let staged = stage_contradiction_upsert(&tx, contradiction);
        finish_transaction(&tx, staged)
    }

    pub(super) fn upsert_merge(&self, candidate: &MergeCandidate) -> Result<()> {
        candidate.validate().map_err(Error::InvalidInput)?;
        let tx = self.db.multi_transaction(true);
        let staged = stage_merge_candidate_upsert(&tx, candidate);
        finish_transaction(&tx, staged)
    }
}

pub(super) fn finish_transaction(tx: &MultiTransaction, staged: Result<()>) -> Result<()> {
    match staged {
        Ok(()) => tx.commit().map_err(backend),
        Err(error) => {
            let _ = tx.abort();
            Err(error)
        }
    }
}

fn require_open_endpoints(
    tx: &MultiTransaction,
    between: UnorderedPair<NodeId>,
    overlay: &str,
) -> Result<()> {
    let (lo, hi) = canonical(between);
    let mut params = BTreeMap::new();
    params.insert("lo".into(), dv_str(&lo));
    params.insert("hi".into(), dv_str(&hi));
    let rows = tx_run(
        tx,
        "wanted[id] <- [[$lo], [$hi]]\n\
         ?[id] := wanted[id], *node{id}",
        params,
    )?;
    if rows.rows.len() != 2 {
        return Err(Error::InvalidInput(format!(
            "open {overlay} {} <-> {} has a missing node endpoint",
            between.0.0, between.1.0
        )));
    }
    Ok(())
}

fn reject_reversed_contradiction(
    tx: &MultiTransaction,
    between: UnorderedPair<NodeId>,
) -> Result<()> {
    let (lo, hi) = canonical(between);
    let mut params = BTreeMap::new();
    params.insert("lo".into(), dv_str(&lo));
    params.insert("hi".into(), dv_str(&hi));
    let rows = tx_run(
        tx,
        "?[observations] := \
         *contradiction{lo: $hi, hi: $lo, observations} :limit 1",
        params,
    )?;
    if !rows.rows.is_empty() {
        return Err(backend_str(
            "stored contradiction endpoints are in reversed physical-key order".into(),
        ));
    }
    Ok(())
}

fn reject_reversed_merge_candidate(
    tx: &MultiTransaction,
    between: UnorderedPair<NodeId>,
) -> Result<()> {
    let (lo, hi) = canonical(between);
    let mut params = BTreeMap::new();
    params.insert("lo".into(), dv_str(&lo));
    params.insert("hi".into(), dv_str(&hi));
    let rows = tx_run(
        tx,
        "?[observations] := \
         *merge_candidate{lo: $hi, hi: $lo, observations} :limit 1",
        params,
    )?;
    if !rows.rows.is_empty() {
        return Err(backend_str(
            "stored merge candidate endpoints are in reversed physical-key order".into(),
        ));
    }
    Ok(())
}

pub(super) fn load_contradiction(
    tx: &MultiTransaction,
    between: UnorderedPair<NodeId>,
) -> Result<Option<Contradiction>> {
    reject_reversed_contradiction(tx, between)?;
    let (lo, hi) = canonical(between);
    let mut params = BTreeMap::new();
    params.insert("lo".into(), dv_str(&lo));
    params.insert("hi".into(), dv_str(&hi));
    let rows = tx_run(
        tx,
        "?[observations, first_seen, last_seen, resolution] := \
         *contradiction{lo: $lo, hi: $hi, observations, first_seen, last_seen, resolution}",
        params,
    )?;
    rows.rows
        .first()
        .map(|row| decode_contradiction_values(between, row))
        .transpose()
}

pub(super) fn stage_contradiction_upsert(
    tx: &MultiTransaction,
    contradiction: &Contradiction,
) -> Result<()> {
    contradiction.validate().map_err(Error::InvalidInput)?;
    graph::tx_reject_episode_nodes(
        tx,
        &[contradiction.between.0, contradiction.between.1],
        "contradiction",
    )?;
    reject_reversed_contradiction(tx, contradiction.between)?;
    if contradiction.is_open() {
        require_open_endpoints(tx, contradiction.between, "contradiction")?;
        #[cfg(test)]
        if pause_is_armed(ObservationKind::Contradiction, contradiction.between) {
            acquire_writer_lock_for_tests(tx, contradiction.between)?;
            pause_for_tests(ObservationKind::Contradiction, contradiction.between);
        }
    }
    let (lo, hi) = canonical(contradiction.between);
    let mut params = BTreeMap::new();
    params.insert("lo".into(), dv_str(&lo));
    params.insert("hi".into(), dv_str(&hi));
    params.insert(
        "observations".into(),
        dv_int(i64::from(contradiction.observations)),
    );
    params.insert(
        "first_seen".into(),
        dv_int(encode_storage_timestamp(
            contradiction.first_seen,
            "contradiction first_seen",
        )?),
    );
    params.insert(
        "last_seen".into(),
        dv_int(encode_storage_timestamp(
            contradiction.last_seen,
            "contradiction last_seen",
        )?),
    );
    params.insert(
        "resolution".into(),
        contradiction
            .resolution
            .map(|resolution| dv_str(resolution_str(resolution)))
            .unwrap_or(DataValue::Null),
    );
    tx_run(
        tx,
        "?[lo, hi, observations, first_seen, last_seen, resolution] <- \
         [[$lo, $hi, $observations, $first_seen, $last_seen, $resolution]] \
         :put contradiction {lo, hi => observations, first_seen, last_seen, resolution}",
        params,
    )?;
    Ok(())
}

pub(super) fn load_merge_candidate(
    tx: &MultiTransaction,
    between: UnorderedPair<NodeId>,
) -> Result<Option<MergeCandidate>> {
    reject_reversed_merge_candidate(tx, between)?;
    let (lo, hi) = canonical(between);
    let mut params = BTreeMap::new();
    params.insert("lo".into(), dv_str(&lo));
    params.insert("hi".into(), dv_str(&hi));
    let rows = tx_run(
        tx,
        "?[observations, first_seen, last_seen, resolution] := \
         *merge_candidate{lo: $lo, hi: $hi, observations, first_seen, last_seen, resolution}",
        params,
    )?;
    rows.rows
        .first()
        .map(|row| decode_merge_candidate_values(between, row))
        .transpose()
}

pub(super) fn stage_merge_candidate_upsert(
    tx: &MultiTransaction,
    candidate: &MergeCandidate,
) -> Result<()> {
    candidate.validate().map_err(Error::InvalidInput)?;
    graph::tx_reject_episode_nodes(
        tx,
        &[candidate.between.0, candidate.between.1],
        "merge candidate",
    )?;
    reject_reversed_merge_candidate(tx, candidate.between)?;
    if candidate.is_open() {
        require_open_endpoints(tx, candidate.between, "merge candidate")?;
        #[cfg(test)]
        if pause_is_armed(ObservationKind::MergeCandidate, candidate.between) {
            acquire_writer_lock_for_tests(tx, candidate.between)?;
            pause_for_tests(ObservationKind::MergeCandidate, candidate.between);
        }
    }
    let (lo, hi) = canonical(candidate.between);
    let mut params = BTreeMap::new();
    params.insert("lo".into(), dv_str(&lo));
    params.insert("hi".into(), dv_str(&hi));
    params.insert(
        "observations".into(),
        dv_int(i64::from(candidate.observations)),
    );
    params.insert(
        "first_seen".into(),
        dv_int(encode_storage_timestamp(
            candidate.first_seen,
            "merge candidate first_seen",
        )?),
    );
    params.insert(
        "last_seen".into(),
        dv_int(encode_storage_timestamp(
            candidate.last_seen,
            "merge candidate last_seen",
        )?),
    );
    params.insert(
        "resolution".into(),
        candidate
            .resolution
            .map(|resolution| dv_str(merge_resolution_str(resolution)))
            .unwrap_or(DataValue::Null),
    );
    tx_run(
        tx,
        "?[lo, hi, observations, first_seen, last_seen, resolution] <- \
         [[$lo, $hi, $observations, $first_seen, $last_seen, $resolution]] \
         :put merge_candidate {lo, hi => observations, first_seen, last_seen, resolution}",
        params,
    )?;
    Ok(())
}

pub(super) fn decode_stored_pair(
    lo: &DataValue,
    hi: &DataValue,
    overlay: &str,
) -> Result<UnorderedPair<NodeId>> {
    let lo = decode_canonical_node_id(lo, &format!("{overlay} low endpoint"))?;
    let hi = decode_canonical_node_id(hi, &format!("{overlay} high endpoint"))?;
    if lo >= hi {
        return Err(backend_str(format!(
            "stored {overlay} endpoints are not in strict canonical order"
        )));
    }
    Ok(UnorderedPair(lo, hi))
}

fn decode_resolution(value: &DataValue) -> Result<Option<Resolution>> {
    match value {
        DataValue::Null => Ok(None),
        DataValue::Str(value) => match value.as_str() {
            "superseded" => Ok(Some(Resolution::Superseded)),
            "context_dependent" => Ok(Some(Resolution::ContextDependent)),
            "unresolved" => Ok(Some(Resolution::Unresolved)),
            _ => Err(backend_str(
                "stored contradiction resolution is invalid".into(),
            )),
        },
        _ => Err(backend_str(
            "stored contradiction resolution is not text or null".into(),
        )),
    }
}

fn decode_merge_resolution(value: &DataValue) -> Result<Option<MergeResolution>> {
    match value {
        DataValue::Null => Ok(None),
        DataValue::Str(value) => match value.as_str() {
            "full" => Ok(Some(MergeResolution::Full)),
            "partial" => Ok(Some(MergeResolution::Partial)),
            "keep" => Ok(Some(MergeResolution::Keep)),
            _ => Err(backend_str(
                "stored merge candidate resolution is invalid".into(),
            )),
        },
        _ => Err(backend_str(
            "stored merge candidate resolution is not text or null".into(),
        )),
    }
}

pub(super) fn decode_contradiction_values(
    between: UnorderedPair<NodeId>,
    row: &[DataValue],
) -> Result<Contradiction> {
    if row.len() != 4 {
        return Err(backend_str(format!(
            "stored contradiction row has {} values, expected 4",
            row.len()
        )));
    }
    let contradiction = Contradiction {
        between,
        observations: decode_storage_u32(&row[0], "contradiction observations")?,
        first_seen: decode_storage_timestamp(&row[1], "contradiction first_seen")?,
        last_seen: decode_storage_timestamp(&row[2], "contradiction last_seen")?,
        resolution: decode_resolution(&row[3])?,
    };
    contradiction
        .validate()
        .map_err(|error| backend_str(format!("invalid stored contradiction: {error}")))?;
    Ok(contradiction)
}

pub(super) fn decode_merge_candidate_values(
    between: UnorderedPair<NodeId>,
    row: &[DataValue],
) -> Result<MergeCandidate> {
    if row.len() != 4 {
        return Err(backend_str(format!(
            "stored merge candidate row has {} values, expected 4",
            row.len()
        )));
    }
    let candidate = MergeCandidate {
        between,
        observations: decode_storage_u32(&row[0], "merge candidate observations")?,
        first_seen: decode_storage_timestamp(&row[1], "merge candidate first_seen")?,
        last_seen: decode_storage_timestamp(&row[2], "merge candidate last_seen")?,
        resolution: decode_merge_resolution(&row[3])?,
    };
    candidate
        .validate()
        .map_err(|error| backend_str(format!("invalid stored merge candidate: {error}")))?;
    Ok(candidate)
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ObservationKind {
    Contradiction,
    MergeCandidate,
}

#[cfg(test)]
#[derive(Default)]
struct TestState {
    target: Option<(ObservationKind, UnorderedPair<NodeId>)>,
    entered: bool,
    released: bool,
}

#[cfg(test)]
#[derive(Default)]
struct TestHook {
    state: Mutex<TestState>,
    wake: Condvar,
}

#[cfg(test)]
fn test_hook() -> &'static TestHook {
    static HOOK: OnceLock<TestHook> = OnceLock::new();
    HOOK.get_or_init(TestHook::default)
}

#[cfg(test)]
fn pause_is_armed(kind: ObservationKind, between: UnorderedPair<NodeId>) -> bool {
    test_hook()
        .state
        .lock()
        .expect("overlay test hook lock")
        .target
        == Some((kind, between))
}

#[cfg(test)]
pub(super) fn arm_pause(kind: ObservationKind, between: UnorderedPair<NodeId>) {
    let hook = test_hook();
    let mut state = hook.state.lock().expect("overlay test hook lock");
    assert!(state.target.is_none(), "overlay test hook is already armed");
    *state = TestState {
        target: Some((kind, between)),
        entered: false,
        released: false,
    };
}

#[cfg(test)]
pub(super) fn wait_for_pause() {
    let hook = test_hook();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut state = hook.state.lock().expect("overlay test hook lock");
    while !state.entered {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .expect("overlay observation did not reach the test hook");
        let (next, timeout) = hook
            .wake
            .wait_timeout(state, remaining)
            .expect("overlay test hook wait");
        state = next;
        assert!(
            !timeout.timed_out() || state.entered,
            "overlay observation did not reach the test hook"
        );
    }
}

#[cfg(test)]
pub(super) fn release_pause() {
    let hook = test_hook();
    let mut state = hook.state.lock().expect("overlay test hook lock");
    assert!(state.entered, "overlay test hook was not entered");
    state.released = true;
    hook.wake.notify_all();
}

#[cfg(test)]
fn pause_for_tests(kind: ObservationKind, between: UnorderedPair<NodeId>) {
    let hook = test_hook();
    let mut state = hook.state.lock().expect("overlay test hook lock");
    if state.target != Some((kind, between)) {
        return;
    }
    state.entered = true;
    hook.wake.notify_all();
    while !state.released {
        state = hook.wake.wait(state).expect("overlay test hook wait");
    }
    *state = TestState::default();
    hook.wake.notify_all();
}

#[cfg(test)]
fn acquire_writer_lock_for_tests(
    tx: &MultiTransaction,
    between: UnorderedPair<NodeId>,
) -> Result<()> {
    let (lo, _) = canonical(between);
    let mut params = BTreeMap::new();
    params.insert("id".into(), dv_str(&lo));
    // Re-put one already-admitted endpoint without changing its value. This
    // makes the pause hook observably hold the transaction's SQLite writer lock,
    // so the cross-handle race test proves transaction scope rather than timing.
    tx_run(
        tx,
        "?[id, data, status] := *node{id, data, status}, id == $id \
         :put node {id => data, status}",
        params,
    )?;
    Ok(())
}
