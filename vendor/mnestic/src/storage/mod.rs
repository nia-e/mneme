/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::sync::TryLockError;
use std::time::{Duration, Instant};

use crossbeam::sync::{ShardedLock, ShardedLockReadGuard};
use miette::{bail, Result};

use crate::data::tuple::Tuple;
use crate::data::value::ValidityTs;
use crate::try_decode_tuple_from_kv;

pub(crate) mod mem;
#[cfg(feature = "storage-new-rocksdb")]
pub mod newrocks;
#[cfg(feature = "storage-rocksdb")]
pub(crate) mod rocks;
#[cfg(feature = "storage-sled")]
pub(crate) mod sled;
#[cfg(feature = "storage-sqlite")]
pub(crate) mod sqlite;
pub(crate) mod temp;
#[cfg(feature = "storage-tikv")]
pub(crate) mod tikv;
// pub(crate) mod re;

/// Swappable storage trait for Cozo's storage engine
pub trait Storage<'s>: Send + Sync + Clone {
    /// The associated transaction type used by this engine
    type Tx: StoreTx<'s>;

    /// Returns a string that identifies the storage kind
    fn storage_kind(&self) -> &'static str;

    /// Create a transaction object. Write ops will only be called when `write == true`.
    fn transact(&'s self, write: bool) -> Result<Self::Tx>;

    /// Open a read snapshot without waiting past `deadline` on storage-owned
    /// process-local locks.
    ///
    /// Backends must opt in: falling back to [`Self::transact`] would turn a
    /// bounded read transaction into a false cancellation promise whenever
    /// snapshot acquisition blocks in a lock manager or remote client.
    fn transact_read_with_deadline(&'s self, _deadline: Instant) -> Result<Self::Tx> {
        bail!(
            "bounded read transactions are not supported by the {} storage backend",
            self.storage_kind()
        )
    }

    /// Compact the key range. Can be a no-op if the storage engine does not
    /// have the concept of compaction.
    fn range_compact(&'s self, lower: &[u8], upper: &[u8]) -> Result<()>;

    /// Put multiple key-value pairs into the database.
    /// No duplicate data will be sent, and the order data come in is strictly ascending.
    /// There will be no other access to the database while this function is running.
    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()>;

    /// Whether this engine can bulk-publish a freshly-built index by ingesting a
    /// sorted-string table (SST) file — see [`ingest_sorted`](Self::ingest_sorted).
    /// Defaults to `false`; only the RocksDB backend overrides this. (mnestic fork)
    fn supports_sst_ingest(&self) -> bool {
        false
    }

    /// Bulk-publish strictly-ascending key-value `entries` into the *live*
    /// database by building an SST file and atomically ingesting it, bypassing
    /// the transaction write-batch overlay entirely. The engine manages the
    /// temporary file. Keys MUST arrive in strictly ascending order. (mnestic fork)
    ///
    /// Unlike a transactional `put`, ingested keys become visible to new reads
    /// as soon as this returns, independent of any open transaction's commit.
    /// Callers relying on this for index publishing must therefore ingest the
    /// index data *before* the metadata that references it becomes visible.
    ///
    /// The default implementation errors; engines without SST support should use
    /// the per-key `put` path instead.
    fn ingest_sorted<'a>(
        &'a self,
        _entries: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        bail!("this storage engine does not support SST ingest")
    }
}

/// Acquire one crossbeam read lock cooperatively, checking the caller's
/// monotonic deadline between short parks. This is shared by the Mem/SQLite
/// snapshot locks and the runtime's per-relation scan lock.
pub(crate) fn read_sharded_before_deadline<'a, T>(
    lock: &'a ShardedLock<T>,
    deadline: Instant,
) -> Result<ShardedLockReadGuard<'a, T>> {
    loop {
        crate::runtime::db::check_deadline(deadline)?;
        match lock.try_read() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(TryLockError::WouldBlock) => park_before_deadline(deadline)?,
        }
    }
}

/// Deadline-aware read acquisition for metadata registries whose poisoned
/// state must fail closed instead of exposing a potentially partial update.
pub(crate) fn read_sharded_before_deadline_strict<'a, T>(
    lock: &'a ShardedLock<T>,
    deadline: Instant,
    name: &str,
) -> Result<ShardedLockReadGuard<'a, T>> {
    loop {
        crate::runtime::db::check_deadline(deadline)?;
        match lock.try_read() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => bail!("{name} lock is poisoned"),
            Err(TryLockError::WouldBlock) => park_before_deadline(deadline)?,
        }
    }
}

/// Yield without busy-spinning while retaining a sub-millisecond deadline
/// check cadence under ordinary contention.
pub(crate) fn park_before_deadline(deadline: Instant) -> Result<()> {
    crate::runtime::db::check_deadline(deadline)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    std::thread::sleep(remaining.min(Duration::from_micros(250)));
    crate::runtime::db::check_deadline(deadline)
}

/// Trait for the associated transaction type of a storage engine.
/// A transaction needs to guarantee MVCC semantics for all operations.
pub trait StoreTx<'s>: Sync {
    /// Get a key. If `for_update` is `true` (only possible in a write transaction),
    /// then the database needs to guarantee that `commit()` can only succeed if
    /// the key has not been modified outside the transaction.
    fn get(&self, key: &[u8], for_update: bool) -> Result<Option<Vec<u8>>>;

    /// Get multiple keys. If `for_update` is `true` (only possible in a write transaction),
    /// then the database needs to guarantee that `commit()` can only succeed if
    /// the keys have not been modified outside the transaction.
    fn multi_get(&self, keys: &[Vec<u8>], for_update: bool) -> Result<Vec<Option<Vec<u8>>>> {
        keys.iter().map(|k| self.get(k, for_update)).collect()
    }

    /// Put a key-value pair into the storage. In case of existing key,
    /// the storage engine needs to overwrite the old value.
    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()>;

    /// Put a key whose writes are serialized by a higher-level authority
    /// (mnestic fork; sole current user: the tt commit clock's high-water
    /// mark, `runtime/tt_clock.rs`). Backends whose write transactions
    /// snapshot-validate first-locks (RocksDB pessimistic transactions) must
    /// skip that validation for this key — otherwise any two temporally
    /// overlapping transactions writing it make the later committer abort
    /// spuriously (`Resource busy`; the 0.8.4 `avgdl` hot-key failure mode).
    /// Default: a plain `put` (sqlite/mem take storage-level write locks, so
    /// overlapping write transactions cannot exist there).
    fn put_externally_serialized(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.put(key, val)
    }

    /// Should return true if the engine supports parallel put, false otherwise.
    fn supports_par_put(&self) -> bool;

    /// Put a key-value pair into the storage. In case of existing key,
    /// the storage engine needs to overwrite the old value.
    /// The difference between this one and `put` is the mutability of self.
    /// It is OK to always panic if `supports_par_put` returns `false`.
    fn par_put(&self, _key: &[u8], _val: &[u8]) -> Result<()> {
        panic!("par_put is not supported")
    }

    /// Delete a key-value pair from the storage.
    fn del(&mut self, key: &[u8]) -> Result<()>;

    /// Delete a key-value pair from the storage.
    /// The difference between this one and `del` is the mutability of self.
    /// It is OK to always panic if `supports_par_put` returns `false`.
    fn par_del(&self, _key: &[u8]) -> Result<()> {
        panic!("par_del is not supported")
    }

    /// Delete a range from persisted data only.
    fn del_range_from_persisted(&mut self, lower: &[u8], upper: &[u8]) -> Result<()>;

    /// Check if a key exists. If `for_update` is `true` (only possible in a write transaction),
    /// then the database needs to guarantee that `commit()` can only succeed if
    /// the key has not been modified outside the transaction.
    fn exists(&self, key: &[u8], for_update: bool) -> Result<bool>;

    /// Commit a transaction. Must return an `Err` if MVCC consistency cannot be guaranteed,
    /// and discard all changes introduced by this transaction.
    fn commit(&mut self) -> Result<()>;

    /// Scan on a range. `lower` is inclusive whereas `upper` is exclusive.
    /// The default implementation calls [`range_scan_owned`](Self::range_scan) and converts the results.
    ///
    /// The implementation must call
    /// [`try_decode_tuple_from_kv`](crate::try_decode_tuple_from_kv) to obtain a decoded tuple in
    /// the loop of the iterator.
    fn range_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        let it = self.range_scan(lower, upper);
        Box::new(it.map(|pair| {
            let (key, value) = pair?;
            try_decode_tuple_from_kv(&key, &value, None)
        }))
    }

    /// Bounded analogue of [`range_scan_tuple`](Self::range_scan_tuple).
    ///
    /// The default preserves laziness by stopping the underlying iterator at
    /// `limit`. Backends whose query engine would otherwise do more work than
    /// the iterator exposes (notably SQLite) should override this and push the
    /// limit into the native range operation.
    fn range_scan_tuple_limited<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        limit: usize,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        Box::new(self.range_scan_tuple(lower, upper).take(limit))
    }

    /// Reverse analogue of [`range_scan_tuple`](Self::range_scan_tuple).
    /// Engines must override this with a native lazy iterator to support direct
    /// descending primary-key pages. The default fails explicitly; it must never
    /// collect and reverse an unbounded forward range.
    fn range_scan_tuple_rev<'a>(
        &'a self,
        _lower: &[u8],
        _upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        Box::new(std::iter::once(Err(miette::miette!(
            "storage engine does not support native reverse range scans"
        ))))
    }

    /// Bounded reverse analogue of
    /// [`range_scan_tuple_limited`](Self::range_scan_tuple_limited).
    /// Native reverse support is still mandatory: the default delegates to
    /// [`range_scan_tuple_rev`](Self::range_scan_tuple_rev), whose default
    /// fails explicitly instead of collecting a forward scan.
    fn range_scan_tuple_rev_limited<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        limit: usize,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        Box::new(self.range_scan_tuple_rev(lower, upper).take(limit))
    }

    /// Scan on a range with a certain validity.
    ///
    /// `lower` is inclusive whereas `upper` is exclusive.
    /// For tuples that differ only with respect to their validity, which must be at
    /// the last slot of the key,
    /// only the tuple that has validity equal to or earlier than (i.e. greater by the comparator)
    /// `valid_at` should be considered for returning, and only those with an assertive validity
    /// should be returned. Every other tuple should be skipped.
    ///
    /// Ideally, implementations should take advantage of seeking capabilities of the
    /// underlying storage so that not every tuple within the `lower` and `upper` range
    /// need to be looked at.
    ///
    /// For custom implementations, it is OK to return an iterator that always error out,
    /// in which case the database with the engine does not support time travelling.
    /// You should indicate this clearly in your error message.
    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        valid_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>;

    /// Two-level bitemporal scan (mnestic fork, bitemporality step 4b; see
    /// `data/bitemporal.rs`). The default implementation drives the generic
    /// probe loop over `range_scan` — one fresh range per probe. Correct on
    /// every backend; hot backends may override with a pinned-iterator seek
    /// loop (step 6 measures before optimizing).
    fn range_bitemporal_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        vt_at: Option<ValidityTs>,
        tt_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let upper = upper.to_vec();
        Box::new(crate::data::bitemporal::BitemporalIter::new(
            move |bound: &[u8], _far: bool| -> Result<Option<(Vec<u8>, Vec<u8>)>> {
                // A probe bound past the range end means the walk is done —
                // never hand an inverted range to the backend (mem's
                // BTreeMap::range panics on start > end).
                if bound >= upper.as_slice() {
                    return Ok(None);
                }
                match self.range_scan(bound, &upper).next() {
                    None => Ok(None),
                    Some(kv) => Ok(Some(kv?)),
                }
            },
            lower.to_vec(),
            vt_at,
            tt_at,
        ))
    }

    /// Scan on a range and return the raw results.
    /// `lower` is inclusive whereas `upper` is exclusive.
    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a;

    /// Return the number of rows in the range.
    fn range_count<'a>(&'a self, lower: &[u8], upper: &[u8]) -> Result<usize>
    where
        's: 'a;

    /// Scan for all rows. The rows are required to be in ascending order.
    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a;
}
