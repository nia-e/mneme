//! Volatile walk observations and exact-retry authority.
//!
//! Receipt payloads, immutable batch bindings, claim leases and feedback epochs
//! stay private here. Dispatch can complete a walk, claim an observed batch and
//! settle its owned guard; it cannot assemble a partial claim or clear a binding.
//! This state is deliberately not serialized and is not caller authentication.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use mneme_core::NodeId;
use mneme_core::ports::FeedbackRetryScope;
use mneme_engine::ObservedRoute;
use mneme_walk::WalkSession;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::host::AnyErr;
use crate::{MAX_WALK_BUDGET, fresh_state_token};

#[cfg(test)]
mod tests;

/// A live walk session plus which registered db it walks.
pub(super) struct McpSession {
    pub(super) db: String,
    pub(super) session: WalkSession,
    /// Includes `start`; terminal `done`/`abort` remain available when this
    /// reaches [`crate::MAX_WALK_ACTIONS`].
    pub(super) actions: usize,
    /// Last time an admitted action touched this session — drives expiry so
    /// abandoned walks (a client that never sends `done`/`abort`) don't leak.
    pub(super) touched: Instant,
}

/// Completed walks are retained briefly as opaque, single-use training receipts.
/// A caller may no longer submit an invented trail to train arbitrary edges.
const RECEIPT_TTL: Duration = Duration::from_secs(3600);
/// A dropped/cancelled reflect future must not strand a receipt forever. The
/// drop guard normally releases immediately; this lease is the fail-safe when
/// cancellation races another holder of the session-state mutex.
const RECEIPT_CLAIM_TTL: Duration = Duration::from_secs(60);
/// Unconsumed walk results and successful-response tombstones have independent
/// quotas. Tombstones keep the observed payload reachable when a response is
/// lost after commit; without them the durable idempotency ledger is unreachable.
const MAX_RECEIPTS: usize = 128;
const MAX_CONSUMED_RECEIPTS: usize = 1024;
const MAX_RECEIPT_RECORDS: usize = MAX_RECEIPTS + MAX_CONSUMED_RECEIPTS;

#[derive(Clone)]
struct WalkReceipt {
    /// Receipts in this local generation are database-bound integrity handles,
    /// not authorization: they are not yet bound to an MCP/HTTP session, actor,
    /// or audience. Any caller holding the opaque token can attempt to claim it.
    db: String,
    routes: Vec<ObservedRoute>,
    visited: HashSet<NodeId>,
    /// Immutable issue time. Claim/retry activity must never slide the volatile
    /// capability's reachability lifetime.
    issued: Instant,
    /// Wall-clock twin of `issued`. Expire on either clock: monotonic time avoids
    /// rollback extension. A forward jump may expire a capability, but storage
    /// proof reclamation itself is driven only by the resulting reachability floor.
    issued_wall: SystemTime,
    /// A successful reflect keeps this bounded tombstone until receipt expiry so
    /// an exact retry after a lost response can reach `AlreadyApplied`.
    consumed: bool,
    /// Canonical identity of the first receipt operation that claimed this token.
    /// This is bound before any async work and never cleared, even on failure or
    /// cancellation: a post-commit cancellation must not make regrouping the
    /// token or changing its classification possible.
    bound_batch: Option<ReceiptBatchBinding>,
    claim: ClaimState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReceiptBatchBinding {
    key: String,
    reflect_fingerprint: String,
    sequence: u64,
}

struct ReceiptPayload {
    routes: Vec<ObservedRoute>,
    visited: HashSet<NodeId>,
}

#[derive(Default)]
pub(super) struct SessionState {
    pub(super) active: HashMap<String, McpSession>,
    receipts: HashMap<String, WalkReceipt>,
    /// Random volatile capability generation per logical database. Releasing a
    /// database for offline maintenance rotates only that database's authority;
    /// unrelated stores retain their live receipt/retry epoch.
    feedback_epochs: HashMap<String, String>,
    /// Checked first-claim sequence allocator, independent per logical database.
    next_feedback_sequence: HashMap<String, u64>,
}

impl SessionState {
    pub(super) fn feedback_epoch(&mut self, db: &str) -> String {
        self.feedback_epochs
            .entry(db.to_owned())
            .or_insert_with(|| ulid::Ulid::new().to_string())
            .clone()
    }

    pub(super) fn rotate_feedback_epoch(&mut self, db: &str) -> String {
        let epoch = ulid::Ulid::new().to_string();
        self.replace_feedback_epoch(db, epoch.clone());
        epoch
    }
}

const MAX_RECEIPT_VISITED: usize = MAX_WALK_BUDGET;
const MAX_RECEIPT_REACHED: usize = MAX_RECEIPT_VISITED - 1;

/// A lease is all-or-nothing: it cannot be claimed without an owner or timestamp.
/// This is deliberately independent of the permanent retry binding and consumed
/// tombstone state; an exact retry may claim an already-consumed receipt.
#[derive(Clone, Default)]
enum ClaimState {
    #[default]
    Available,
    Claimed(ClaimLease),
}

#[derive(Clone)]
struct ClaimLease {
    id: String,
    started: Instant,
}

impl WalkReceipt {
    fn is_claimed(&self) -> bool {
        matches!(self.claim, ClaimState::Claimed(_))
    }

    fn claim_id(&self) -> Option<&str> {
        match &self.claim {
            ClaimState::Available => None,
            ClaimState::Claimed(lease) => Some(&lease.id),
        }
    }
}

impl SessionState {
    pub(super) fn replace_feedback_epoch(&mut self, db: &str, epoch: String) {
        self.feedback_epochs.insert(db.to_owned(), epoch);
        self.next_feedback_sequence.remove(db);
    }

    pub(super) fn purge_expired_receipts(&mut self) {
        purge_expired_unclaimed_receipts(&mut self.receipts);
    }

    pub(super) fn database_status(&self, db: &str) -> DatabaseSessionStatus {
        let walks = self.active.values().filter(|walk| walk.db == db).count();
        let blocking_receipts = self
            .receipts
            .values()
            .filter(|receipt| receipt.db == db && (!receipt.consumed || receipt.is_claimed()))
            .count();
        let retry_tombstones = self
            .receipts
            .values()
            .filter(|receipt| receipt.db == db && receipt.consumed && !receipt.is_claimed())
            .count();
        DatabaseSessionStatus {
            walks,
            blocking_receipts,
            retry_tombstones,
        }
    }

    pub(super) fn purge_consumed_receipts(&mut self, db: &str) -> usize {
        let before = self.receipts.len();
        self.receipts
            .retain(|_, receipt| receipt.db != db || !receipt.consumed || receipt.is_claimed());
        before.saturating_sub(self.receipts.len())
    }

    #[cfg(all(test, feature = "http"))]
    pub(super) fn is_empty(&self) -> bool {
        self.active.is_empty()
            && self.receipts.is_empty()
            && self.feedback_epochs.is_empty()
            && self.next_feedback_sequence.is_empty()
    }

    #[cfg(test)]
    pub(super) fn receipt_count(&self) -> usize {
        self.receipts.len()
    }
}

pub(super) struct DatabaseSessionStatus {
    pub(super) walks: usize,
    pub(super) blocking_receipts: usize,
    pub(super) retry_tombstones: usize,
}

fn purge_expired_unclaimed_receipts(receipts: &mut HashMap<String, WalkReceipt>) {
    let now = Instant::now();
    for receipt in receipts.values_mut() {
        if let ClaimState::Claimed(lease) = &receipt.claim
            && now.saturating_duration_since(lease.started) >= RECEIPT_CLAIM_TTL
        {
            receipt.claim = ClaimState::Available;
        }
    }
    let wall_now = SystemTime::now();
    receipts.retain(|_, receipt| {
        let monotonic_expired = now.saturating_duration_since(receipt.issued) >= RECEIPT_TTL;
        let wall_expired = wall_now
            .duration_since(receipt.issued_wall)
            .is_ok_and(|age| age >= RECEIPT_TTL);
        receipt.is_claimed() || (!monotonic_expired && !wall_expired)
    });
}

fn ensure_receipt_capacity(receipts: &mut HashMap<String, WalkReceipt>) -> Result<(), AnyErr> {
    purge_expired_unclaimed_receipts(receipts);
    let live = receipts
        .values()
        .filter(|receipt| !receipt.consumed)
        .count();
    if live >= MAX_RECEIPTS {
        return Err(format!(
            "walk receipt capacity exceeded (limit {MAX_RECEIPTS}); reflect or let an unclaimed receipt expire"
        )
        .into());
    }
    Ok(())
}

fn bounded_receipt_payload(
    routes: Vec<ObservedRoute>,
    visited: HashSet<NodeId>,
) -> Result<ReceiptPayload, AnyErr> {
    if visited.len() > MAX_RECEIPT_VISITED || routes.len() > MAX_RECEIPT_REACHED {
        return Err(format!(
            "walk receipt payload exceeds limits (visited {}/{MAX_RECEIPT_VISITED}, reached {}/{MAX_RECEIPT_REACHED})",
            visited.len(),
            routes.len()
        )
        .into());
    }
    Ok(ReceiptPayload { routes, visited })
}

/// Mint a receipt without losing the walk on capacity or payload failure.
pub(super) fn complete_walk(state: &mut SessionState, token: &str) -> Result<Value, AnyErr> {
    if !state.active.contains_key(token) {
        return Err("unknown or finished session token".into());
    }
    ensure_receipt_capacity(&mut state.receipts)?;

    let entry = state.active.get(token).expect("validated in the same lock");
    let trail = serde_json::to_value(entry.session.trail())?;
    let payload = bounded_receipt_payload(
        entry.session.observed_routes(),
        entry.session.visited_nodes(),
    )?;
    let db = entry.db.clone();
    let receipt = fresh_state_token(&state.receipts);

    state.active.remove(token);
    let replaced = state.receipts.insert(
        receipt.clone(),
        WalkReceipt {
            db,
            routes: payload.routes,
            visited: payload.visited,
            issued: Instant::now(),
            issued_wall: SystemTime::now(),
            consumed: false,
            bound_batch: None,
            claim: ClaimState::Available,
        },
    );
    debug_assert!(replaced.is_none(), "fresh receipt token collided");
    debug_assert!(state.receipts.len() <= MAX_RECEIPT_RECORDS);
    Ok(json!({ "trail": trail, "receipt": receipt }))
}

pub(super) struct ClaimedReceiptBatch {
    claim: ReceiptClaim,
    routes: Vec<ObservedRoute>,
    visited: HashSet<NodeId>,
    idempotency_key: String,
    retry: FeedbackRetryScope,
}

/// The owner of a single in-flight attempt, not its immutable retry identity.
/// A late completion/drop can release only its own leases, never a later retry.
struct ReceiptClaim {
    tokens: Vec<String>,
    id: String,
}

impl ClaimedReceiptBatch {
    pub(super) fn into_guard(self, state: &Mutex<SessionState>) -> ReceiptClaimGuard<'_> {
        ReceiptClaimGuard {
            state,
            batch: Some(self),
        }
    }
}

/// Canonical, non-secret durable operation key for a receipt set. The tokens are
/// sorted and length-prefixed before hashing so caller order and delimiters
/// cannot create a second retry identity. Persisting the digest rather than the
/// raw capability tokens also avoids extending their useful lifetime in a DB.
fn receipt_feedback_key(db_name: &str, sorted_tokens: &[String]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mneme-mcp-receipt-feedback-v1\0");
    hash.update((db_name.len() as u64).to_be_bytes());
    hash.update(db_name.as_bytes());
    for token in sorted_tokens {
        hash.update((token.len() as u64).to_be_bytes());
        hash.update(token.as_bytes());
    }
    format!("mcp-receipt-v1:{:x}", hash.finalize())
}

/// Bind the pre-await claim to both the canonical used set and the exact ordered
/// event classifications derived from the sorted receipt set. This is broader
/// than the storage event digest on purpose: changing a visited root that only
/// affects consolidation must still not create an alternate retry operation.
fn receipt_reflect_fingerprint(
    routes: &[ObservedRoute],
    used: &HashSet<NodeId>,
    unhelpful: &HashSet<NodeId>,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mneme-mcp-reflect-binding-v4-explicit\0");
    let mut used = used.iter().copied().collect::<Vec<_>>();
    used.sort_unstable();
    hash.update((used.len() as u64).to_be_bytes());
    for id in &used {
        hash.update(id.0.to_bytes());
    }
    let mut unhelpful = unhelpful.iter().copied().collect::<Vec<_>>();
    unhelpful.sort_unstable();
    hash.update((unhelpful.len() as u64).to_be_bytes());
    for id in &unhelpful {
        hash.update(id.0.to_bytes());
    }
    hash.update((routes.len() as u64).to_be_bytes());
    for route in routes {
        hash.update(route.previous.0.to_bytes());
        hash.update(route.target.0.to_bytes());
        hash.update(route.edge_from.0.to_bytes());
        hash.update(route.edge_to.0.to_bytes());
        hash.update([u8::from(used_contains(used.as_slice(), route.target))]);
    }
    format!("{:x}", hash.finalize())
}

fn used_contains(sorted_used: &[NodeId], id: NodeId) -> bool {
    sorted_used.binary_search(&id).is_ok()
}

fn fresh_claim_id(receipts: &HashMap<String, WalkReceipt>) -> String {
    loop {
        let claim = ulid::Ulid::new().to_string();
        if receipts
            .values()
            .all(|receipt| receipt.claim_id() != Some(claim.as_str()))
        {
            return claim;
        }
    }
}

fn release_walk_receipt_claims(
    receipts: &mut HashMap<String, WalkReceipt>,
    tokens: &[String],
    claim_id: &str,
    consume: bool,
) {
    for token in tokens {
        if let Some(receipt) = receipts.get_mut(token)
            && receipt.claim_id() == Some(claim_id)
        {
            receipt.consumed |= consume;
            receipt.claim = ClaimState::Available;
        }
    }
}

/// Best-effort immediate cancellation cleanup. The one-minute claim lease is the
/// fallback if another task happens to hold the mutex exactly when this guard is
/// dropped.
pub(super) struct ReceiptClaimGuard<'a> {
    state: &'a Mutex<SessionState>,
    batch: Option<ClaimedReceiptBatch>,
}

impl ReceiptClaimGuard<'_> {
    fn batch(&self) -> &ClaimedReceiptBatch {
        self.batch
            .as_ref()
            .expect("only finish consumes the owned batch")
    }

    // The dispatcher can read the admitted feedback, but cannot mutate it into
    // an operation different from the immutable receipt binding.
    pub(super) fn routes(&self) -> &[ObservedRoute] {
        &self.batch().routes
    }

    pub(super) fn visited(&self) -> &HashSet<NodeId> {
        &self.batch().visited
    }

    pub(super) fn idempotency_key(&self) -> &str {
        &self.batch().idempotency_key
    }

    pub(super) fn retry_scope(&self) -> &FeedbackRetryScope {
        &self.batch().retry
    }

    pub(super) fn finish(mut self, state: &mut SessionState, consume: bool) {
        if let Some(batch) = self.batch.take() {
            release_walk_receipt_claims(
                &mut state.receipts,
                &batch.claim.tokens,
                &batch.claim.id,
                consume,
            );
        }
    }
}

impl Drop for ReceiptClaimGuard<'_> {
    fn drop(&mut self) {
        if let Some(batch) = &self.batch
            && let Ok(mut state) = self.state.try_lock()
        {
            release_walk_receipt_claims(
                &mut state.receipts,
                &batch.claim.tokens,
                &batch.claim.id,
                false,
            );
        }
    }
}

/// Validate a whole receipt batch before marking any member claimed. This keeps
/// partial validation failures retryable, rejects duplicate handles, and orders
/// the receipt set canonically before constructing the durable feedback payload.
pub(super) fn claim_walk_receipts(
    state: &mut SessionState,
    receipt_ids: &[String],
    db_name: &str,
    used: &HashSet<NodeId>,
    unhelpful: &HashSet<NodeId>,
) -> Result<ClaimedReceiptBatch, AnyErr> {
    if receipt_ids.is_empty() {
        return Err("reflect requires at least one completed-walk receipt".into());
    }
    if used.intersection(unhelpful).next().is_some() {
        return Err("reflect used and unhelpful nodes must be disjoint".into());
    }
    purge_expired_unclaimed_receipts(&mut state.receipts);

    let mut unique = HashSet::with_capacity(receipt_ids.len());
    for token in receipt_ids {
        if !unique.insert(token.as_str()) {
            return Err(format!("duplicate walk receipt {token:?}").into());
        }
        let receipt = state
            .receipts
            .get(token)
            .ok_or_else(|| format!("unknown, expired, or consumed walk receipt {token:?}"))?;
        if receipt.is_claimed() {
            return Err(format!("walk receipt {token:?} is already being reflected").into());
        }
        if receipt.db != db_name {
            return Err(format!(
                "walk receipt {token:?} belongs to database {:?}, not {db_name:?}",
                receipt.db
            )
            .into());
        }
    }

    let mut tokens = receipt_ids.to_vec();
    tokens.sort_unstable();
    let idempotency_key = receipt_feedback_key(db_name, &tokens);
    let routes = tokens
        .iter()
        .filter_map(|token| state.receipts.get(token))
        .flat_map(|receipt| receipt.routes.iter().copied())
        .collect::<Vec<_>>();
    let reflect_fingerprint = receipt_reflect_fingerprint(&routes, used, unhelpful);

    // Recover an existing immutable binding, if this is an exact retry. Every
    // member must agree on token-set key, used/event fingerprint, and sequence.
    let mut existing_binding: Option<ReceiptBatchBinding> = None;
    for token in &tokens {
        let receipt = state
            .receipts
            .get(token)
            .expect("receipt batch was validated in the same critical section");
        if let Some(bound) = &receipt.bound_batch {
            if bound.key != idempotency_key || bound.reflect_fingerprint != reflect_fingerprint {
                return Err(format!(
                    "walk receipt {token:?} is already bound to a different receipt operation"
                )
                .into());
            }
            if existing_binding
                .as_ref()
                .is_some_and(|existing| existing != bound)
            {
                return Err("walk receipt batch contains inconsistent sequence bindings".into());
            }
            existing_binding = Some(bound.clone());
        }
    }

    let visited: HashSet<NodeId> = tokens
        .iter()
        .filter_map(|token| state.receipts.get(token))
        .flat_map(|receipt| receipt.visited.iter().copied())
        .collect();
    for (label, ids) in [("used", used), ("unhelpful", unhelpful)] {
        if let Some(unvisited) = ids.iter().find(|id| !visited.contains(id)) {
            return Err(format!(
                "{label} node {} was not visited by any supplied walk receipt",
                unvisited.0,
            )
            .into());
        }
    }

    // A bound receipt owns a future tombstone slot even after a failed attempt;
    // otherwise many concurrent/distinct claims could each reserve the same
    // remaining slot and exceed the post-commit retention cap.
    let retained = state
        .receipts
        .values()
        .filter(|receipt| receipt.consumed || receipt.is_claimed() || receipt.bound_batch.is_some())
        .count();
    let newly_reserved = tokens
        .iter()
        .filter(|token| {
            state
                .receipts
                .get(*token)
                .is_some_and(|receipt| receipt.bound_batch.is_none())
        })
        .count();
    if retained
        .checked_add(newly_reserved)
        .is_none_or(|total| total > MAX_CONSUMED_RECEIPTS)
    {
        return Err(format!(
            "consumed receipt retry capacity exceeded (limit {MAX_CONSUMED_RECEIPTS}); wait for an old receipt to expire"
        )
        .into());
    }

    let binding = if let Some(binding) = existing_binding {
        binding
    } else {
        let next = state
            .next_feedback_sequence
            .entry(db_name.to_owned())
            .or_insert(1);
        if *next >= i64::MAX as u64 {
            return Err("feedback first-claim sequence exhausted".into());
        }
        let sequence = *next;
        *next = next
            .checked_add(1)
            .ok_or("feedback first-claim sequence exhausted")?;
        ReceiptBatchBinding {
            key: idempotency_key.clone(),
            reflect_fingerprint,
            sequence,
        }
    };

    let claim_id = fresh_claim_id(&state.receipts);
    let claimed_at = Instant::now();
    for token in &tokens {
        let receipt = state
            .receipts
            .get_mut(token)
            .expect("validated in the same critical section");
        if receipt.bound_batch.is_none() {
            receipt.bound_batch = Some(binding.clone());
        }
        receipt.claim = ClaimState::Claimed(ClaimLease {
            id: claim_id.clone(),
            started: claimed_at,
        });
    }
    let min_live_sequence = state
        .receipts
        .values()
        .filter(|receipt| receipt.db == db_name)
        .filter_map(|receipt| receipt.bound_batch.as_ref().map(|binding| binding.sequence))
        .min()
        .expect("the current claimed batch pins its own live floor");
    let retry = FeedbackRetryScope::new(
        state.feedback_epoch(db_name),
        binding.sequence,
        min_live_sequence,
    )?;
    debug_assert!(state.receipts.len() <= MAX_RECEIPT_RECORDS);
    Ok(ClaimedReceiptBatch {
        claim: ReceiptClaim {
            tokens,
            id: claim_id,
        },
        routes,
        visited,
        idempotency_key,
        retry,
    })
}
