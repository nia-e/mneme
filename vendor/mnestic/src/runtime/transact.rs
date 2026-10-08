/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::data::program::ReturnMutation;
use miette::{bail, miette, Diagnostic, Result};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::memcmp::MAX_ENCODED_KEY_BYTES;
use crate::data::tuple::{try_decode_tuple_from_key, TupleT};
use crate::data::value::DataValue;
use crate::data::value::{Validity, ValidityTs, LARGEST_UTF_CHAR};
use crate::fts::TokenizerCache;
use crate::parse::SourceSpan;
use crate::runtime::callback::CallbackCollector;
use crate::runtime::catalog_codec::{
    decode_unattested_catalog, validate_unattested_catalog_set_v1,
};
use crate::runtime::graph_projection::ProjectionCache;
use crate::runtime::relation::{RelationHandle, RelationId};
use crate::runtime::tt_clock::{tt_hwm_key, TtClock};
use crate::storage::temp::TempTx;
use crate::storage::StoreTx;
use crate::{CallbackOp, NamedRows};
use std::cmp::Reverse;

/// Abort cleanup for process-local FTS publications.
///
/// The cleanup is name-only: it cannot distinguish this transaction's cache
/// entry from a later publication of the same child name. SQLite is the only
/// backend whose transaction contract keeps competing writers serialized
/// through [`SessionTx::drop`], so every other backend must leave it disabled.
/// Consequently this makes no same-name cache-reuse claim after an aborted
/// non-SQLite FTS build; safe cleanup there needs publication identity.
pub(crate) enum FtsAbortCacheCleanup {
    Disabled,
    SqliteNameOnly(BTreeSet<SmartString<LazyCompact>>),
}

impl FtsAbortCacheCleanup {
    pub(crate) fn for_storage_kind(storage_kind: &str) -> Self {
        if storage_kind == "sqlite" {
            Self::SqliteNameOnly(BTreeSet::new())
        } else {
            Self::Disabled
        }
    }

    pub(crate) fn arm(&mut self, name: SmartString<LazyCompact>) {
        if let Self::SqliteNameOnly(names) = self {
            names.insert(name);
        }
    }

    fn disarm(&mut self) {
        if let Self::SqliteNameOnly(names) = self {
            names.clear();
        }
    }

    #[cfg(test)]
    pub(crate) fn is_sqlite_name_only(&self) -> bool {
        matches!(self, Self::SqliteNameOnly(_))
    }
}

pub struct SessionTx<'a> {
    pub(crate) store_tx: Box<dyn StoreTx<'a> + 'a>,
    pub(crate) temp_store_tx: TempTx,
    pub(crate) relation_store_id: Arc<AtomicU64>,
    pub(crate) temp_store_id: AtomicU32,
    pub(crate) tokenizers: Arc<TokenizerCache>,
    /// Cross-query cache of FTS corpus stats for legacy indexes (mnestic fork);
    /// see `Db::fts_doc_stats_cache`. Shared `Arc` cloned per transaction.
    pub(crate) fts_doc_stats_cache: Arc<Mutex<BTreeMap<SmartString<LazyCompact>, (u64, u64)>>>,
    /// SQLite-only name cleanup for process-cache entries published by fresh
    /// FTS children. Non-SQLite backends intentionally carry `Disabled`: no
    /// portable writer-serialization contract makes name-only deletion safe.
    pub(crate) fts_abort_cache_cleanup: FtsAbortCacheCleanup,
    /// Transaction-time commit clock (mnestic fork, bitemporality step 2);
    /// shared handle to the `Db`'s high-water mark. See `runtime/tt_clock.rs`.
    pub(crate) tt_clock: Arc<TtClock>,
    /// Per-`Db` critical section serialising tt allocation + HWM persist +
    /// commit for tt-stamped transactions (`docs/specs/bitemporality.md`
    /// §13.10). Held only inside `commit_tx_with_tt`.
    pub(crate) tt_commit_lock: Arc<Mutex<()>>,
    /// Buffered writes to tt-stamped relations (mnestic fork, bitemporality
    /// step 3): rows are collected at statement time and stamped + written at
    /// commit inside `commit_tx_with_tt`, so every row of the transaction
    /// carries the same engine-assigned tt and tt order == commit order.
    /// Consequence (spec §5): writes are NOT visible to later reads in the
    /// same script — a transaction is one belief event.
    pub(crate) pending_tt_writes: Vec<PendingTtWrite>,
    /// Set when the transaction burned a tt outside the buffered-write path
    /// (`::evict`'s audit stamp): forces `commit_tx` through
    /// `commit_tx_with_tt` so the persisted HWM covers the burned value.
    pub(crate) tt_hwm_dirty: bool,
    /// Relations `:reconcile`d in this transaction (provenance semirings
    /// R3). A reconcile declares its relation's COMPLETE belief; any further
    /// write to the relation in the same transaction — including another
    /// reconcile — would silently amend or contradict the declaration
    /// (an idempotent reconcile buffers no rows, so `pending_tt_writes`
    /// alone cannot witness it).
    pub(crate) reconciled_tt_relations: std::collections::BTreeSet<u64>,
    /// Outer wall-clock deadline (mnestic fork, query budget). For ordinary
    /// calls it is the minimum of any per-call timeout and the Db default,
    /// anchored at script start. A bounded read multi-transaction instead sets
    /// it once when the transaction handle is created, and every later query
    /// and direct scan inherits that same instant. `run_query` combines it
    /// (via `min`) with a block's own `:timeout`, which may tighten but never
    /// re-arm the outer guard. `None` means no outer budget.
    pub(crate) script_deadline: Option<Instant>,
    /// Whether this transaction's queries participate in the Db-global
    /// `::running` / `::kill` registry. Bounded read workers deliberately do
    /// not: their immutable deadline is the cancellation mechanism, and a
    /// contended or poisoned registry mutex must not pin their snapshot.
    pub(crate) track_running_queries: bool,
    /// Graph-projection freshness state, shared with the `Db` (mnestic fork;
    /// `runtime/graph_projection.rs`).
    pub(crate) projections: Arc<ProjectionCache>,
    /// `bump_seq` as of **before** this transaction's storage snapshot pinned.
    /// A source relation whose token exceeds this was mutated by a commit this
    /// transaction cannot see, so no cached projection over it may be served.
    pub(crate) watermark: u64,
    /// Every persistent relation this transaction has mutated, marked at
    /// mutation-function entry. Drives the commit-time token bump, and marks
    /// the transaction's own uncommitted writes so it never consults or
    /// populates the cache for them. Temp relations are excluded: their ids
    /// come from a separate per-transaction counter and collide numerically
    /// with persistent ids.
    pub(crate) dirty_relations: BTreeSet<RelationId>,
    /// Set between `begin_commit` and `finish_commit`. If a panic unwinds
    /// through the storage commit, `Drop` reads this and balances the `inflight`
    /// increment — otherwise the sources would be denied cache hits forever.
    pub(crate) commit_inflight: bool,
}

/// One statement's worth of buffered writes to a tt-stamped relation.
pub(crate) struct PendingTtWrite {
    pub(crate) handle: RelationHandle,
    /// Full extracted tuples WITHOUT the trailing TxTime key column
    /// (plain keys [+ vt], then value columns).
    pub(crate) rows: Vec<Vec<DataValue>>,
    /// tt-only deletion (`:rm`): stamp with `is_assert = false`. On
    /// bitemporal relations retraction rides the vt axis instead and this is
    /// always false.
    pub(crate) is_retract: bool,
    pub(crate) span: SourceSpan,
}

pub const CURRENT_STORAGE_VERSION: [u8; 1] = [0x00];

/// Hard vendor ceiling for the generic exact-name catalog rewrite. The caller
/// must additionally declare its smaller operation-specific relation cap.
pub(crate) const CATALOG_CANONICALIZATION_RELATION_LIMIT_V1: usize = 256;
const CATALOG_CANONICALIZATION_CUMULATIVE_BYTE_LIMIT_V1: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) enum CatalogCanonicalizationFailpointV1 {
    None,
    #[cfg(test)]
    AfterPut(usize),
}

impl CatalogCanonicalizationFailpointV1 {
    fn check_after_put(self, _completed_puts: usize) -> Result<()> {
        match self {
            Self::None => Ok(()),
            #[cfg(test)]
            Self::AfterPut(failed_put) if failed_put == _completed_puts => {
                bail!("injected relation catalog canonicalization failure after put")
            }
            #[cfg(test)]
            Self::AfterPut(_) => Ok(()),
        }
    }
}

fn storage_version_key() -> Vec<u8> {
    let storage_version_tuple = vec![DataValue::Null, DataValue::from("STORAGE_VERSION")];
    storage_version_tuple.encode_as_key(RelationId::SYSTEM)
}

const STATUS_STR: &str = "status";
const OK_STR: &str = "OK";

/// A transaction that mutated relations and went away without committing —
/// aborted, errored, or unwound — still forces a token bump (mnestic fork;
/// `runtime/graph_projection.rs` §3.3). An abort is not proof that nothing
/// changed: `mem`'s `del_range_from_persisted` writes straight through to the
/// persisted store. `commit_tx` empties the dirty set, so a committed
/// transaction does no work here.
///
/// The bump is the whole body, so it survives an unwinding panic — including a
/// panic inside the storage commit itself, which leaves `commit_inflight` set
/// and an unbalanced `inflight` that only this impl can put back.
impl Drop for SessionTx<'_> {
    fn drop(&mut self) {
        if let FtsAbortCacheCleanup::SqliteNameOnly(names) = &self.fts_abort_cache_cleanup {
            if !names.is_empty() {
                // Keep the hashed analyzer cache: it is keyed by the complete
                // configuration and is therefore safe to reuse. Only the
                // name-keyed publications can alias a rolled-back child created
                // later with a different manifest.
                let mut named = self
                    .tokenizers
                    .named_cache
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for name in names {
                    named.remove(name);
                }
                drop(named);

                let mut stats = self
                    .fts_doc_stats_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for name in names {
                    stats.remove(name);
                }
            }
        }

        if self.commit_inflight {
            self.projections.finish_commit(&self.dirty_relations);
        } else if !self.dirty_relations.is_empty() {
            self.projections.bump_aborted(&self.dirty_relations);
        }
    }
}

impl<'a> SessionTx<'a> {
    /// Highest relation id observed in relation metadata, plus duplicate-id groups.
    /// Corrupt metadata is skipped so opening a damaged store remains possible.
    pub(crate) fn relation_id_census(&self) -> Result<(u64, BTreeMap<u64, Vec<String>>)> {
        let lower = vec![DataValue::from("")].encode_as_key(RelationId::SYSTEM);
        let upper =
            vec![DataValue::from(String::from(LARGEST_UTF_CHAR))].encode_as_key(RelationId::SYSTEM);
        let mut max_id = 0;
        let mut by_id: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        for pair in self.store_tx.range_scan(&lower, &upper) {
            let (key, value) = pair?;
            if key >= upper {
                break;
            }
            let Ok(handle) = RelationHandle::decode(&value) else {
                continue;
            };
            max_id = max_id.max(handle.id.0);
            by_id
                .entry(handle.id.0)
                .or_default()
                .push(handle.name.to_string());
        }
        by_id.retain(|_, names| names.len() > 1);
        Ok((max_id, by_id))
    }

    /// Canonicalize one complete persistent relation catalog inside this
    /// transaction. The exact caller-supplied name set is authority: the
    /// complete id-zero domain is frozen and validated before the first put,
    /// then every relation value is rewritten and read back in storage-key
    /// order. No live `RelationHandle` serializer participates.
    pub(crate) fn canonicalize_relation_catalog_v1(
        &mut self,
        relation_names: Vec<String>,
        relation_count_cap: usize,
        failpoint: CatalogCanonicalizationFailpointV1,
    ) -> Result<()> {
        if relation_count_cap > CATALOG_CANONICALIZATION_RELATION_LIMIT_V1 {
            bail!("relation catalog canonicalization declared an unsupported relation cap");
        }
        if relation_names.len() > CATALOG_CANONICALIZATION_RELATION_LIMIT_V1 {
            bail!("relation catalog canonicalization received too many requested names");
        }

        let mut expected_names = BTreeSet::new();
        let mut requested_name_bytes = 0_usize;
        for name in relation_names {
            if name.len() > MAX_ENCODED_KEY_BYTES {
                bail!("relation catalog canonicalization name exceeded its byte boundary");
            }
            requested_name_bytes = requested_name_bytes
                .checked_add(name.len())
                .filter(|bytes| *bytes <= CATALOG_CANONICALIZATION_CUMULATIVE_BYTE_LIMIT_V1)
                .ok_or_else(|| {
                    miette!("relation catalog canonicalization names exceeded their byte boundary")
                })?;
            if !expected_names.insert(name) {
                bail!("relation catalog canonicalization received a duplicate requested name");
            }
        }
        if expected_names.len() > relation_count_cap {
            bail!("relation catalog canonicalization requested names exceeded the declared cap");
        }

        let lower = RelationId::SYSTEM.raw_encode();
        let upper = RelationId::new(1).raw_encode();
        let row_cap = relation_count_cap
            .checked_add(2)
            .ok_or_else(|| miette!("relation catalog canonicalization row cap overflowed"))?;
        let mut keys = Vec::with_capacity(expected_names.len());
        let mut records = Vec::with_capacity(expected_names.len());
        let mut canonical_values = Vec::with_capacity(expected_names.len());
        let mut relation_counter = None;
        let mut storage_version_seen = false;
        let mut row_count = 0_usize;
        let mut cumulative_raw = 0_usize;
        let mut cumulative_canonical = 0_usize;

        // StoreTx guarantees ascending range order. Retaining that order here
        // makes the later put/readback sequence deterministic by encoded key.
        for pair in self.store_tx.range_scan(&lower, &upper) {
            let (key, value) = pair?;
            row_count = row_count
                .checked_add(1)
                .filter(|count| *count <= row_cap)
                .ok_or_else(|| {
                    miette!("relation catalog canonicalization exceeded the declared row cap")
                })?;
            cumulative_raw =
                checked_catalog_cumulative_bytes_v1(cumulative_raw, key.len(), value.len(), "raw")?;

            let tuple = try_decode_tuple_from_key(&key, 2)
                .map_err(|_| miette!("relation catalog canonicalization rejected a system key"))?;
            if tuple.encode_as_key(RelationId::SYSTEM) != key {
                bail!("relation catalog canonicalization rejected a noncanonical system key");
            }
            match tuple.as_slice() {
                [DataValue::Str(key_name)] => {
                    if keys.len() >= relation_count_cap {
                        bail!(
                            "relation catalog canonicalization exceeded the declared relation cap"
                        );
                    }
                    if !expected_names.remove(key_name.as_str()) {
                        bail!(
                            "relation catalog canonicalization found an unexpected relation entry"
                        );
                    }
                    let record = decode_unattested_catalog(&value).map_err(|_| {
                        miette!(
                            "relation catalog canonicalization rejected a relation catalog value"
                        )
                    })?;
                    if record.name() != key_name.as_str() {
                        bail!("relation catalog canonicalization found a key/name mismatch");
                    }
                    let canonical = record.canonical_struct_map().map_err(|_| {
                        miette!(
                            "relation catalog canonicalization could not encode canonical bytes"
                        )
                    })?;
                    cumulative_canonical = checked_catalog_cumulative_bytes_v1(
                        cumulative_canonical,
                        key.len(),
                        canonical.len(),
                        "canonical",
                    )?;
                    keys.push(key);
                    records.push(record);
                    canonical_values.push(canonical);
                }
                [DataValue::Null] => {
                    if relation_counter.is_some() {
                        bail!(
                            "relation catalog canonicalization found a duplicate relation counter"
                        );
                    }
                    let counter = RelationId::try_raw_decode(&value).map_err(|_| {
                        miette!("relation catalog canonicalization rejected the relation counter")
                    })?;
                    relation_counter = Some(counter.0);
                    cumulative_canonical = checked_catalog_cumulative_bytes_v1(
                        cumulative_canonical,
                        key.len(),
                        value.len(),
                        "canonical",
                    )?;
                }
                [DataValue::Null, DataValue::Str(marker)] if marker == "STORAGE_VERSION" => {
                    if storage_version_seen || value != CURRENT_STORAGE_VERSION {
                        bail!("relation catalog canonicalization rejected the storage version");
                    }
                    storage_version_seen = true;
                    cumulative_canonical = checked_catalog_cumulative_bytes_v1(
                        cumulative_canonical,
                        key.len(),
                        value.len(),
                        "canonical",
                    )?;
                }
                _ => {
                    bail!("relation catalog canonicalization found a foreign id-zero key shape")
                }
            }
        }

        if !expected_names.is_empty() {
            bail!("relation catalog canonicalization did not find every requested relation");
        }
        if keys.len() > relation_count_cap
            || row_count != keys.len().saturating_add(2)
            || relation_counter.is_none()
            || !storage_version_seen
        {
            bail!("relation catalog canonicalization did not find the exact catalog domain");
        }
        validate_unattested_catalog_set_v1(&records).map_err(|_| {
            miette!("relation catalog canonicalization rejected catalog identity or linkage")
        })?;
        let maximum_relation_id = records
            .iter()
            .map(|record| record.relation_id())
            .max()
            .unwrap_or(0);
        if maximum_relation_id
            > relation_counter
                .ok_or_else(|| miette!("relation catalog canonicalization lost its counter"))?
        {
            bail!("relation catalog canonicalization relation counter was below a live id");
        }

        for (position, (key, canonical)) in keys.iter().zip(&canonical_values).enumerate() {
            self.store_tx.put(key, canonical)?;
            failpoint.check_after_put(position + 1)?;
        }

        for ((key, canonical), record) in keys.iter().zip(&canonical_values).zip(&records) {
            let readback = self
                .store_tx
                .get(key, true)?
                .ok_or_else(|| miette!("canonical relation catalog readback was missing"))?;
            if readback != *canonical {
                bail!("canonical relation catalog readback bytes differed");
            }
            let readback_record = decode_unattested_catalog(&readback).map_err(|_| {
                miette!("canonical relation catalog readback failed exact decoding")
            })?;
            if readback_record.name() != record.name()
                || readback_record
                    .canonical_struct_map()
                    .map_err(|_| miette!("canonical relation catalog readback failed encoding"))?
                    != *canonical
            {
                bail!("canonical relation catalog readback changed descriptor semantics");
            }
        }
        Ok(())
    }

    pub(crate) fn get_returning_rows(
        &self,
        callback_collector: &mut CallbackCollector,
        rel: &str,
        returning: &ReturnMutation,
    ) -> Result<NamedRows> {
        let returned_rows = {
            match returning {
                ReturnMutation::NotReturning => NamedRows::new(
                    vec![STATUS_STR.to_string()],
                    vec![vec![DataValue::from(OK_STR)]],
                ),
                ReturnMutation::Returning => {
                    let meta = self.get_relation(rel, false)?;
                    let target_len = meta.metadata.keys.len() + meta.metadata.non_keys.len();
                    let mut returned_rows = Vec::new();
                    if let Some(collected) = callback_collector.get(&meta.name) {
                        for (kind, insertions, deletions) in collected {
                            let (pos_key, neg_key) = match kind {
                                CallbackOp::Put => ("inserted", "replaced"),
                                CallbackOp::Rm => ("requested", "deleted"),
                            };
                            for row in &insertions.rows {
                                let mut v = Vec::with_capacity(target_len + 1);
                                v.push(DataValue::from(pos_key));
                                v.extend_from_slice(row);
                                while v.len() <= target_len {
                                    v.push(DataValue::Null);
                                }
                                returned_rows.push(v);
                            }
                            for row in &deletions.rows {
                                let mut v = Vec::with_capacity(target_len + 1);
                                v.push(DataValue::from(neg_key));
                                v.extend_from_slice(row);
                                while v.len() <= target_len {
                                    v.push(DataValue::Null);
                                }
                                returned_rows.push(v);
                            }
                        }
                    }
                    let mut header = vec!["_kind".to_string()];
                    header.extend(
                        meta.metadata
                            .keys
                            .iter()
                            .chain(meta.metadata.non_keys.iter())
                            .map(|s| s.name.to_string()),
                    );
                    NamedRows::new(header, returned_rows)
                }
            }
        };
        Ok(returned_rows)
    }

    pub(crate) fn init_storage(&mut self) -> Result<RelationId> {
        let tuple = vec![DataValue::Null];
        let t_encoded = tuple.encode_as_key(RelationId::SYSTEM);
        let found = self.store_tx.get(&t_encoded, false)?;
        let storage_version_key = storage_version_key();
        let ret = match found {
            None => {
                self.store_tx
                    .put(&storage_version_key, &CURRENT_STORAGE_VERSION)?;
                self.store_tx
                    .put(&t_encoded, &RelationId::new(0).raw_encode())?;
                RelationId::SYSTEM
            }
            Some(slice) => {
                let version_found = self.store_tx.get(&storage_version_key, false)?;
                match version_found {
                    None => {
                        bail!("Storage is used but un-versioned, probably created by an ancient version of Cozo.")
                    }
                    Some(v) => {
                        if v != CURRENT_STORAGE_VERSION {
                            bail!(
                                "Version mismatch: expect storage version {:?}, got {:?}",
                                CURRENT_STORAGE_VERSION,
                                v
                            )
                        }
                    }
                }
                RelationId::try_raw_decode(&slice)?
            }
        };
        Ok(ret)
    }

    /// Validate and load the persistent storage header without creating or
    /// repairing it. This is the admission path for an existing-only store,
    /// so absence is an error rather than an invitation to initialize.
    pub(crate) fn require_initialized_storage(&self) -> Result<RelationId> {
        let counter_key = vec![DataValue::Null].encode_as_key(RelationId::SYSTEM);
        let counter = self.store_tx.get(&counter_key, false)?.ok_or_else(|| {
            miette::miette!("Storage is not initialized: relation-id counter is missing")
        })?;

        let version = self
            .store_tx
            .get(&storage_version_key(), false)?
            .ok_or_else(|| {
                miette::miette!(
                    "Storage is used but un-versioned, probably created by an ancient version of Cozo."
                )
            })?;
        if version != CURRENT_STORAGE_VERSION {
            bail!(
                "Version mismatch: expect storage version {:?}, got {:?}",
                CURRENT_STORAGE_VERSION,
                version
            )
        }

        Ok(RelationId::try_raw_decode(&counter)?)
    }

    /// Record that this transaction mutates `handle`'s contents, for the
    /// graph-projection freshness protocol (mnestic fork; spec §3.4). Called
    /// at the entry of every mutation path. Temp relations are skipped: their
    /// ids come from a per-transaction counter and collide numerically with
    /// persistent relation ids.
    pub(crate) fn mark_dirty(&mut self, handle: &RelationHandle) {
        if !handle.is_temp {
            self.dirty_relations.insert(handle.id);
        }
    }

    /// Commit, maintaining the graph-projection freshness protocol
    /// (`runtime/graph_projection.rs` §3.3): raise `inflight` before the
    /// storage commit, and mint fresh tokens after it returns — whether it
    /// succeeded or failed. A destroyed relation's state stays behind as a
    /// permanent tombstone; see `finish_commit` for why a purge here would
    /// re-open a stale-hit window for pre-destroy snapshots.
    ///
    /// This is the single commit funnel for every write path, so no mutation
    /// can escape the protocol. Taking the dirty set here also disarms
    /// [`Drop`], whose bump exists for transactions that never reach a commit.
    pub fn commit_tx(&mut self) -> Result<()> {
        if self.dirty_relations.is_empty() {
            // Read-only transaction: nothing to invalidate, no lock to take.
            let result = self.commit_tx_inner();
            if result.is_ok() {
                self.fts_abort_cache_cleanup.disarm();
            }
            return result;
        }
        self.projections.begin_commit(&self.dirty_relations);
        self.commit_inflight = true;
        let res = self.commit_tx_inner();
        if res.is_ok() {
            // Storage now durably names these exact FTS children. Disarm
            // rollback before any post-commit hook can panic and reach Drop.
            self.fts_abort_cache_cleanup.disarm();
        }
        #[cfg(any(test, feature = "test-hooks"))]
        self.projections.run_commit_fence(&self.dirty_relations);
        self.commit_inflight = false;
        self.projections.finish_commit(&self.dirty_relations);
        self.dirty_relations.clear();
        res
    }

    fn commit_tx_inner(&mut self) -> Result<()> {
        if !self.pending_tt_writes.is_empty() || self.tt_hwm_dirty {
            // Route through the tt commit path (mnestic fork): stamp the
            // buffered rows, persist the HWM, commit — all under the per-Db
            // critical section. Every call site inherits this automatically.
            // `tt_hwm_dirty` covers transactions that burned a tt outside
            // the buffered-write path (`::evict`'s audit stamp): the HWM put
            // must happen under the lock held ACROSS the commit, or an
            // overlapping tt commit could be overwritten by our lower mark.
            self.commit_tx_with_tt()?;
            return Ok(());
        }
        self.store_tx.commit()?;
        Ok(())
    }

    /// Read the persisted tt high-water mark, if any (mnestic fork,
    /// bitemporality step 2). Called at `Db` open to seed the clock. A
    /// malformed value is a **loud** error (mirrors `init_storage`'s
    /// version-mismatch bail): silently falling back to the wall clock would
    /// re-issue committed tts after a backward clock step — the exact
    /// failure the persisted mark exists to prevent. Future values are
    /// accepted (forward clock skew on a previous host is legitimate;
    /// monotonicity is the invariant, not plausibility).
    pub(crate) fn read_persisted_tt_hwm(&self) -> Result<Option<i64>> {
        match self.store_tx.get(&tt_hwm_key(), false)? {
            None => Ok(None),
            Some(v) => {
                let bytes: [u8; 8] = v.as_slice().try_into().map_err(|_| {
                    miette::miette!(
                        "tt high-water mark is corrupt: expected 8 bytes, got {} — \
                         refusing to open with a possibly non-monotonic transaction clock",
                        v.len()
                    )
                })?;
                let val = i64::from_be_bytes(bytes);
                if val < 0 {
                    bail!(
                        "tt high-water mark is corrupt (negative: {val}) — \
                         refusing to open with a possibly non-monotonic transaction clock"
                    );
                }
                Ok(Some(val))
            }
        }
    }

    /// Commit this transaction with an engine-assigned transaction time
    /// (mnestic fork, bitemporality step 2; spec §5/§13.10). Under the
    /// per-`Db` critical section: allocate the tt, persist the high-water
    /// mark **inside this same storage transaction** (so a crash can never
    /// leave the persisted mark behind a committed tt), then commit. Returns
    /// the allocated tt. tt order == commit order == visibility order by
    /// construction.
    ///
    /// Nothing calls this in production yet — step 3 (schema opt-in +
    /// buffered stamping) will route commits of transactions that touched
    /// tt-stamped relations through here, stamping the buffered rows with
    /// the returned tt before `store_tx.commit()`.
    pub(crate) fn commit_tx_with_tt(&mut self) -> Result<i64> {
        let lock = self.tt_commit_lock.clone();
        // Poison recovery is sound here: a panic inside the section can only
        // have advanced the atomic (a burned, never-committed value) — the
        // invariant "atomic HWM >= persisted HWM >= every committed tt"
        // survives, so later committers may proceed rather than fail-stop.
        let _guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tt = self.tt_clock.advance();
        self.stamp_pending_tt_writes(tt)?;
        // put_externally_serialized: the mutex above is the serialization
        // authority for this key; RocksDB must not snapshot-validate it (two
        // overlapping tt commits would otherwise abort the later one).
        self.store_tx
            .put_externally_serialized(&tt_hwm_key(), &tt.to_be_bytes())?;
        self.store_tx.commit()?;
        Ok(tt)
    }

    /// Drain the buffered tt writes: append the allocated tt as the trailing
    /// key component of every row and write it. Same-(key, vt) double-puts in
    /// one transaction collapse to last-write-wins naturally (identical full
    /// key); an assert AND a retract of one (key, vt) in one transaction is
    /// an error (both rows would carry the same tt — an unbreakable
    /// resolution tie, spec §3/§6).
    fn stamp_pending_tt_writes(&mut self, tt: i64) -> Result<()> {
        use std::collections::BTreeMap;
        let writes = std::mem::take(&mut self.pending_tt_writes);

        // One belief event may not both assert and retract the same logical
        // key at its single tt (an unbreakable resolution tie, spec §3/§6).
        // Checked across ALL buffered statements of the transaction, keyed by
        // (relation id, flag-normalized key-prefix bytes); the flag is the vt
        // is_assert on bitemporal relations and !is_retract on tt-only ones.
        {
            let mut seen: BTreeMap<(u64, Vec<u8>), bool> = BTreeMap::new();
            for w in &writes {
                let kpos = w.handle.metadata.keys.len() - 1;
                let is_bitemporal = kpos >= 1
                    && matches!(
                        w.handle.metadata.keys[kpos - 1].typing.coltype,
                        crate::data::relation::ColType::Validity
                    );
                for row in &w.rows {
                    let (norm_key, flag) = if is_bitemporal {
                        let (vt_ts, vt_flag) = match &row[kpos - 1] {
                            DataValue::Validity(v) => (v.timestamp, v.is_assert.0),
                            _ => continue,
                        };
                        let mut norm = row[0..kpos].to_vec();
                        norm[kpos - 1] = DataValue::Validity(Validity {
                            timestamp: vt_ts,
                            is_assert: Reverse(true),
                        });
                        (
                            w.handle.try_encode_partial_key_for_store(&norm, w.span)?,
                            vt_flag,
                        )
                    } else {
                        (
                            w.handle
                                .try_encode_partial_key_for_store(&row[0..kpos], w.span)?,
                            !w.is_retract,
                        )
                    };
                    if let Some(prev) = seen.insert((w.handle.id.0, norm_key), flag) {
                        if prev != flag {
                            #[derive(Debug, Error, Diagnostic)]
                            #[error("transaction asserts AND retracts one key in relation {0}")]
                            #[diagnostic(
                                code(eval::txtime_assert_retract_conflict),
                                help("both rows would carry the same transaction time — an unbreakable resolution tie; split into two transactions")
                            )]
                            struct TtAssertRetractConflict(String);
                            bail!(TtAssertRetractConflict(w.handle.name.to_string()));
                        }
                    }
                }
            }
        }

        for w in writes {
            let kpos = w.handle.metadata.keys.len() - 1;
            let is_bitemporal = kpos >= 1
                && matches!(
                    w.handle.metadata.keys[kpos - 1].typing.coltype,
                    crate::data::relation::ColType::Validity
                );
            let tt_value = DataValue::Validity(Validity {
                timestamp: ValidityTs(Reverse(tt)),
                // tt-only relations carry the retract bit on the tt axis;
                // bitemporal ones must keep the reserved flag byte at 0
                // (assert), retraction riding the vt axis (spec §4).
                is_assert: Reverse(!w.is_retract || is_bitemporal),
            });
            for mut row in w.rows {
                row.insert(kpos, tt_value.clone());
                let key = w.handle.encode_key_for_store(&row, w.span)?;
                let val = w.handle.encode_val_for_store(&row, w.span)?;
                self.store_tx.put(&key, &val)?;
            }
        }
        Ok(())
    }
}

fn checked_catalog_cumulative_bytes_v1(
    current: usize,
    key_bytes: usize,
    value_bytes: usize,
    kind: &'static str,
) -> Result<usize> {
    if key_bytes > MAX_ENCODED_KEY_BYTES {
        bail!("relation catalog canonicalization key exceeded its byte boundary");
    }
    current
        .checked_add(key_bytes)
        .and_then(|total| total.checked_add(value_bytes))
        .filter(|total| *total <= CATALOG_CANONICALIZATION_CUMULATIVE_BYTE_LIMIT_V1)
        .ok_or_else(|| {
            miette!("relation catalog canonicalization {kind} bytes exceeded their boundary")
        })
}
