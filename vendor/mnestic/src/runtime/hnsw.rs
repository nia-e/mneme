/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use crate::data::expr::{eval_bytecode_pred, Bytecode};
use crate::data::program::HnswSearch;
use crate::data::relation::VecElementType;
use crate::data::tuple::Tuple;
use crate::data::value::Vector;
use crate::parse::sys::HnswDistance;
use crate::runtime::relation::{try_decode_val_only, RelationHandle};
use crate::runtime::transact::SessionTx;
use crate::storage::StoreTx;
use crate::{DataValue, Poison, SourceSpan};
use itertools::Itertools;
use miette::{bail, miette, IntoDiagnostic, Result};
use ordered_float::OrderedFloat;
use priority_queue::PriorityQueue;
use rand::Rng;
use rmp_serde::Serializer;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use sha2::{Digest, Sha256};
use smartstring::{LazyCompact, SmartString};
use std::cmp::{max, Reverse};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use crate::runtime::hnsw_build::{build_threads, FlatHnswBuilder, FlatHnswGraph, NodeMeta};

#[derive(Debug, Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct HnswIndexManifest {
    pub(crate) base_relation: SmartString<LazyCompact>,
    pub(crate) index_name: SmartString<LazyCompact>,
    pub(crate) vec_dim: usize,
    pub(crate) dtype: VecElementType,
    pub(crate) vec_fields: Vec<usize>,
    pub(crate) distance: HnswDistance,
    pub(crate) ef_construction: usize,
    pub(crate) m_neighbours: usize,
    pub(crate) m_max: usize,
    pub(crate) m_max0: usize,
    pub(crate) level_multiplier: f64,
    pub(crate) index_filter: Option<String>,
    pub(crate) extend_candidates: bool,
    pub(crate) keep_pruned_connections: bool,
}

impl HnswIndexManifest {
    pub(crate) fn get_random_level(&self) -> i64 {
        let mut rng = rand::thread_rng();
        let uniform_num: f64 = rng.gen_range(0.0..1.0);
        let r = -uniform_num.ln() * self.level_multiplier;
        // the level is the largest integer smaller than r
        -(r.floor() as i64)
    }
}

/// Compact evidence retained across an off-lock HNSW build. Only rows that
/// actually contributed a vector are present: the row key is enough to remove
/// an old index entry, and the digest detects a changed row without retaining
/// its (potentially large) vector payload until publication.
pub(crate) struct HnswSnapshotRow {
    pub(crate) key: Tuple,
    pub(crate) digest: [u8; 32],
}

#[derive(Default)]
pub(crate) struct HnswBuildSnapshot {
    pub(crate) rows: BTreeMap<Vec<u8>, HnswSnapshotRow>,
}

/// Peak-memory proxy and scan accounting for a bulk build. The vector byte
/// count is exact for the builder's compact slab; decoded scan rows are
/// streaming and add at most one base-row payload at a time.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HnswBuildStats {
    pub(crate) rows_scanned: usize,
    pub(crate) rows_eligible: usize,
    pub(crate) indexed_rows: usize,
    pub(crate) vectors_retained: usize,
    pub(crate) vector_bytes_retained: usize,
    pub(crate) snapshot_rows: usize,
}

pub(crate) struct HnswBuild {
    graph: Option<FlatHnswGraph>,
    snapshot: HnswBuildSnapshot,
    stats: HnswBuildStats,
    scan_elapsed: Duration,
    graph_elapsed: Duration,
}

struct ShaWriter<'a>(&'a mut Sha256);

impl Write for ShaWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Stable, allocation-free row fingerprint used only for unlocked-build
/// reconciliation. Hashing the whole tuple covers both indexed vectors and
/// arbitrary columns referenced by an index filter.
pub(crate) fn hnsw_tuple_digest(tuple: &[DataValue]) -> Result<[u8; 32]> {
    let mut digest = Sha256::new();
    let mut serializer = Serializer::new(ShaWriter(&mut digest));
    tuple.serialize(&mut serializer).into_diagnostic()?;
    Ok(digest.finalize().into())
}

fn add_tuple_to_hnsw_builder(
    builder: &mut FlatHnswBuilder,
    manifest: &HnswIndexManifest,
    key_len: usize,
    tuple: &[DataValue],
    stats: &mut HnswBuildStats,
) -> Result<usize> {
    if tuple.len() < key_len {
        bail!(
            "corrupt base row for HNSW index '{}:{}': expected at least {} key fields, got {} values",
            manifest.base_relation,
            manifest.index_name,
            key_len,
            tuple.len()
        );
    }
    let mut added = 0;
    for idx in &manifest.vec_fields {
        let val = tuple.get(*idx).ok_or_else(|| {
            miette!(
                "corrupt base row for HNSW index '{}:{}': vector field column {} is missing from a {}-value tuple",
                manifest.base_relation,
                manifest.index_name,
                idx + 1,
                tuple.len()
            )
        })?;
        if let DataValue::Vec(v) = val {
            stats.vector_bytes_retained += match v {
                Vector::F32(v) => v.len() * std::mem::size_of::<f32>(),
                Vector::F64(v) => v.len() * std::mem::size_of::<f64>(),
            };
            builder.add_node(
                NodeMeta {
                    key: tuple[..key_len].to_vec(),
                    field_idx: *idx,
                    sub_idx: -1,
                    hash: v.get_hash().as_ref().to_vec(),
                },
                v,
            )?;
            added += 1;
        } else if let DataValue::List(l) = val {
            for (sidx, v) in l.iter().enumerate() {
                if let DataValue::Vec(v) = v {
                    stats.vector_bytes_retained += match v {
                        Vector::F32(v) => v.len() * std::mem::size_of::<f32>(),
                        Vector::F64(v) => v.len() * std::mem::size_of::<f64>(),
                    };
                    builder.add_node(
                        NodeMeta {
                            key: tuple[..key_len].to_vec(),
                            field_idx: *idx,
                            sub_idx: sidx as i32,
                            hash: v.get_hash().as_ref().to_vec(),
                        },
                        v,
                    )?;
                    added += 1;
                }
            }
        }
    }
    stats.vectors_retained += added;
    Ok(added)
}

/// Stream/filter a base relation directly into the compact HNSW builder. No
/// `Vec<Tuple>` is formed: an ineligible decoded row is dropped before any of
/// its vector components are copied, while an eligible row contributes only
/// key metadata and its indexed vector(s).
pub(crate) fn collect_hnsw_build<I>(
    manifest: &HnswIndexManifest,
    orig_table: &RelationHandle,
    filter: Option<&Vec<Bytecode>>,
    tuples: I,
    capture_snapshot: bool,
) -> Result<HnswBuild>
where
    I: IntoIterator<Item = Result<Tuple>>,
{
    let scan_start = Instant::now();
    let key_len = orig_table.metadata.keys.len();
    let mut builder = FlatHnswBuilder::new(manifest);
    let mut snapshot = HnswBuildSnapshot::default();
    let mut stats = HnswBuildStats::default();
    let mut stack = vec![];

    for tuple in tuples {
        let tuple = tuple?;
        stats.rows_scanned += 1;
        if let Some(code) = filter {
            if !eval_bytecode_pred(code, &tuple, &mut stack, Default::default())? {
                continue;
            }
        }
        stats.rows_eligible += 1;
        let added = add_tuple_to_hnsw_builder(&mut builder, manifest, key_len, &tuple, &mut stats)?;
        if added == 0 {
            continue;
        }
        stats.indexed_rows += 1;
        if capture_snapshot {
            let encoded_key =
                orig_table.encode_key_for_store(&tuple[..key_len], Default::default())?;
            snapshot.rows.insert(
                encoded_key,
                HnswSnapshotRow {
                    key: tuple[..key_len].to_vec(),
                    digest: hnsw_tuple_digest(&tuple)?,
                },
            );
        }
    }
    stats.snapshot_rows = snapshot.rows.len();
    let scan_elapsed = scan_start.elapsed();

    let graph_start = Instant::now();
    let graph = if builder.is_empty() {
        None
    } else {
        Some(builder.build(manifest, build_threads()))
    };
    let graph_elapsed = graph_start.elapsed();

    Ok(HnswBuild {
        graph,
        snapshot,
        stats,
        scan_elapsed,
        graph_elapsed,
    })
}

#[cfg(test)]
thread_local! {
    static LAST_HNSW_BUILD_STATS: std::cell::RefCell<Option<HnswBuildStats>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn take_last_hnsw_build_stats() -> Option<HnswBuildStats> {
    LAST_HNSW_BUILD_STATS.with(|slot| slot.borrow_mut().take())
}

#[cfg(test)]
fn record_hnsw_build_stats(stats: HnswBuildStats) {
    LAST_HNSW_BUILD_STATS.with(|slot| *slot.borrow_mut() = Some(stats));
}

#[cfg(not(test))]
fn record_hnsw_build_stats(_stats: HnswBuildStats) {}

type CompoundKey = (Tuple, usize, i32);

#[inline]
pub(crate) fn cosine_distance(a_norm_sq: f64, b_norm_sq: f64, dot: f64) -> f64 {
    let denominator = (a_norm_sq * b_norm_sq).sqrt();
    if denominator > 0.0 {
        1.0 - dot / denominator
    } else {
        // A zero/invalid embedding must be maximally distant, finite, and JSON-safe.
        2.0
    }
}

#[inline]
pub(crate) fn distance_is_closer(candidate: f64, current_furthest: f64) -> bool {
    OrderedFloat(candidate) < OrderedFloat(current_furthest)
}

#[inline]
pub(crate) fn distance_is_farther(candidate: f64, current_furthest: f64) -> bool {
    OrderedFloat(candidate) > OrderedFloat(current_furthest)
}

#[cfg(test)]
mod distance_order_tests {
    use super::{distance_is_closer, distance_is_farther};

    #[test]
    fn heap_comparisons_use_one_total_order_for_nan() {
        assert!(distance_is_closer(0.25, f64::NAN));
        assert!(distance_is_farther(f64::NAN, 0.25));
        assert!(!distance_is_closer(f64::NAN, 0.25));
        assert!(!distance_is_farther(0.25, f64::NAN));
    }
}

struct VectorCache {
    cache: FxHashMap<CompoundKey, Vector>,
    distance: HnswDistance,
}

impl VectorCache {
    fn insert(&mut self, k: CompoundKey, v: Vector) {
        self.cache.insert(k, v);
    }
    fn dist(&self, v1: &Vector, v2: &Vector) -> f64 {
        match self.distance {
            HnswDistance::L2 => match (v1, v2) {
                (Vector::F32(a), Vector::F32(b)) => {
                    let diff = a - b;
                    diff.dot(&diff) as f64
                }
                (Vector::F64(a), Vector::F64(b)) => {
                    let diff = a - b;
                    diff.dot(&diff)
                }
                _ => panic!("Cannot compute L2 distance between {:?} and {:?}", v1, v2),
            },
            HnswDistance::Cosine => match (v1, v2) {
                (Vector::F32(a), Vector::F32(b)) => {
                    let a_norm = a.dot(a) as f64;
                    let b_norm = b.dot(b) as f64;
                    let dot = a.dot(b) as f64;
                    cosine_distance(a_norm, b_norm, dot)
                }
                (Vector::F64(a), Vector::F64(b)) => {
                    let a_norm = a.dot(a);
                    let b_norm = b.dot(b);
                    let dot = a.dot(b);
                    cosine_distance(a_norm, b_norm, dot)
                }
                _ => panic!(
                    "Cannot compute cosine distance between {:?} and {:?}",
                    v1, v2
                ),
            },
            HnswDistance::InnerProduct => match (v1, v2) {
                (Vector::F32(a), Vector::F32(b)) => {
                    let dot = a.dot(b);
                    1. - dot as f64
                }
                (Vector::F64(a), Vector::F64(b)) => {
                    let dot = a.dot(b);
                    1. - dot
                }
                _ => panic!("Cannot compute inner product between {:?} and {:?}", v1, v2),
            },
        }
    }
    fn v_dist(&self, v: &Vector, key: &CompoundKey) -> f64 {
        let v2 = self.cache.get(key).unwrap();
        self.dist(v, v2)
    }
    fn k_dist(&self, k1: &CompoundKey, k2: &CompoundKey) -> f64 {
        let v1 = self.cache.get(k1).unwrap();
        let v2 = self.cache.get(k2).unwrap();
        self.dist(v1, v2)
    }
    fn get_key(&self, key: &CompoundKey) -> &Vector {
        self.cache.get(key).unwrap()
    }
    fn ensure_key(
        &mut self,
        key: &CompoundKey,
        handle: &RelationHandle,
        tx: &SessionTx<'_>,
    ) -> Result<()> {
        if !self.cache.contains_key(key) {
            match handle.get(tx, &key.0)? {
                Some(tuple) => self.insert_from_tuple(key, &tuple)?,
                None => bail!("Cannot find compound key for HNSW: {:?}", key),
            }
        }
        Ok(())
    }

    /// Batch `ensure_key` (mnestic fork): one storage `multi_get` covers every
    /// not-yet-cached key. Used on the search hot path, where serial
    /// per-neighbour point gets were measured as a large share of
    /// disk-resident HNSW query cost.
    fn ensure_keys(
        &mut self,
        keys: &[CompoundKey],
        handle: &RelationHandle,
        tx: &SessionTx<'_>,
    ) -> Result<()> {
        let missing: Vec<&CompoundKey> = keys
            .iter()
            .filter(|k| !self.cache.contains_key(*k))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        if missing.len() == 1 {
            return self.ensure_key(missing[0], handle, tx);
        }
        let key_slices: Vec<&[DataValue]> = missing.iter().map(|k| k.0.as_slice()).collect();
        let tuples = handle.get_batch(tx, &key_slices)?;
        for (key, tuple) in missing.iter().zip(tuples) {
            match tuple {
                Some(tuple) => self.insert_from_tuple(key, &tuple)?,
                None => bail!("Cannot find compound key for HNSW: {:?}", key),
            }
        }
        Ok(())
    }

    fn insert_from_tuple(&mut self, key: &CompoundKey, tuple: &[DataValue]) -> Result<()> {
        let mut field = &tuple[key.1];
        if key.2 >= 0 {
            match field {
                DataValue::List(l) => {
                    field = &l[key.2 as usize];
                }
                _ => bail!("Cannot interpret {} as list", field),
            }
        }
        match field {
            DataValue::Vec(v) => {
                self.cache.insert(key.clone(), v.clone());
            }
            _ => bail!("Cannot interpret {} as vector", field),
        }
        Ok(())
    }
}

impl<'a> SessionTx<'a> {
    // --- index-store routing (mnestic fork) -------------------------------
    // During `create_hnsw_index` the index handle is marked `is_temp` so the
    // whole graph is built in the in-RAM temp store (a plain BTreeMap), instead
    // of round-tripping every neighbour read/write through the pessimistic
    // transaction's RocksDB `WriteBatchWithIndex` overlay (which grows with the
    // index and makes the build superlinear). These helpers route raw index
    // KV ops to the same store the `RelationHandle` read methods already pick
    // via `is_temp`, so build and steady-state writes stay byte-identical.
    #[inline]
    fn idx_put(&mut self, idx_table: &RelationHandle, key: &[u8], val: &[u8]) -> Result<()> {
        if idx_table.is_temp {
            self.temp_store_tx.put(key, val)
        } else {
            self.store_tx.put(key, val)
        }
    }
    #[inline]
    fn idx_get(&self, idx_table: &RelationHandle, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if idx_table.is_temp {
            self.temp_store_tx.get(key, false)
        } else {
            self.store_tx.get(key, false)
        }
    }
    #[inline]
    fn idx_del(&mut self, idx_table: &RelationHandle, key: &[u8]) -> Result<()> {
        if idx_table.is_temp {
            self.temp_store_tx.del(key)
        } else {
            self.store_tx.del(key)
        }
    }
    #[inline]
    fn idx_exists(&self, idx_table: &RelationHandle, key: &[u8]) -> Result<bool> {
        if idx_table.is_temp {
            self.temp_store_tx.exists(key, false)
        } else {
            self.store_tx.exists(key, false)
        }
    }

    fn hnsw_put_vector(
        &mut self,
        tuple: &[DataValue],
        q: &Vector,
        idx: usize,
        subidx: i32,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        vec_cache: &mut VectorCache,
    ) -> Result<()> {
        let tuple_key = &tuple[..orig_table.metadata.keys.len()];
        vec_cache.insert((tuple_key.to_vec(), idx, subidx), q.clone());
        let hash = q.get_hash();
        let mut canary_tuple = vec![DataValue::from(0)];
        for _ in 0..2 {
            canary_tuple.extend_from_slice(tuple_key);
            canary_tuple.push(DataValue::from(idx as i64));
            canary_tuple.push(DataValue::from(subidx as i64));
        }
        if let Some(v) = idx_table.get(self, &canary_tuple)? {
            if let DataValue::Bytes(b) = &v[tuple_key.len() * 2 + 6] {
                if b == hash.as_ref() {
                    return Ok(());
                }
            }
            self.hnsw_remove_vec(tuple_key, idx, subidx, orig_table, idx_table)?;
        }

        let ep_res = idx_table
            .scan_bounded_prefix(
                self,
                &[],
                &[DataValue::from(i64::MIN)],
                &[DataValue::from(0)],
            )
            .next();
        if let Some(ep) = ep_res {
            let ep = ep?;
            // bottom level since we are going up
            let bottom_level = ep[0].get_int().unwrap();
            let ep_t_key = ep[1..orig_table.metadata.keys.len() + 1].to_vec();
            let ep_idx = ep[orig_table.metadata.keys.len() + 1].get_int().unwrap() as usize;
            let ep_subidx = ep[orig_table.metadata.keys.len() + 2].get_int().unwrap() as i32;
            let ep_key = (ep_t_key, ep_idx, ep_subidx);
            vec_cache.ensure_key(&ep_key, orig_table, self)?;
            let ep_distance = vec_cache.v_dist(q, &ep_key);
            // max queue
            let mut found_nn = PriorityQueue::new();
            found_nn.push(ep_key, OrderedFloat(ep_distance));
            let target_level = manifest.get_random_level();
            if target_level < bottom_level {
                // this becomes the entry point
                self.hnsw_put_fresh_at_levels(
                    hash.as_ref(),
                    tuple_key,
                    idx,
                    subidx,
                    orig_table,
                    idx_table,
                    target_level,
                    bottom_level - 1,
                )?;
            }
            for current_level in bottom_level..target_level {
                self.hnsw_search_level(
                    q,
                    1,
                    current_level,
                    orig_table,
                    idx_table,
                    &mut found_nn,
                    vec_cache,
                    None,
                )?;
            }
            let mut self_tuple_key = Vec::with_capacity(orig_table.metadata.keys.len() * 2 + 5);
            self_tuple_key.push(DataValue::from(0));
            for _ in 0..2 {
                self_tuple_key.extend_from_slice(tuple_key);
                self_tuple_key.push(DataValue::from(idx as i64));
                self_tuple_key.push(DataValue::from(subidx as i64));
            }
            let mut self_tuple_val = vec![
                DataValue::from(0.0),
                DataValue::Bytes(hash.as_ref().to_vec()),
                DataValue::from(false),
            ];
            for current_level in max(target_level, bottom_level)..=0 {
                let m_max = if current_level == 0 {
                    manifest.m_max0
                } else {
                    manifest.m_max
                };
                self.hnsw_search_level(
                    q,
                    manifest.ef_construction,
                    current_level,
                    orig_table,
                    idx_table,
                    &mut found_nn,
                    vec_cache,
                    None,
                )?;
                // add bidirectional links to the nearest neighbors
                let neighbours = self.hnsw_select_neighbours_heuristic(
                    q,
                    &found_nn,
                    m_max,
                    current_level,
                    manifest,
                    idx_table,
                    orig_table,
                    vec_cache,
                )?;
                // add self-link
                self_tuple_key[0] = DataValue::from(current_level);
                self_tuple_val[0] = DataValue::from(neighbours.len() as f64);

                let self_tuple_key_bytes =
                    idx_table.encode_key_for_store(&self_tuple_key, Default::default())?;
                let self_tuple_val_bytes =
                    idx_table.encode_val_only_for_store(&self_tuple_val, Default::default())?;
                self.idx_put(idx_table, &self_tuple_key_bytes, &self_tuple_val_bytes)?;

                // add bidirectional links
                for (neighbour, Reverse(OrderedFloat(dist))) in neighbours.iter() {
                    let mut out_key = Vec::with_capacity(orig_table.metadata.keys.len() * 2 + 5);
                    let out_val = vec![
                        DataValue::from(*dist),
                        DataValue::Null,
                        DataValue::from(false),
                    ];
                    out_key.push(DataValue::from(current_level));
                    out_key.extend_from_slice(tuple_key);
                    out_key.push(DataValue::from(idx as i64));
                    out_key.push(DataValue::from(subidx as i64));
                    out_key.extend_from_slice(&neighbour.0);
                    out_key.push(DataValue::from(neighbour.1 as i64));
                    out_key.push(DataValue::from(neighbour.2 as i64));
                    let out_key_bytes =
                        idx_table.encode_key_for_store(&out_key, Default::default())?;
                    let out_val_bytes =
                        idx_table.encode_val_only_for_store(&out_val, Default::default())?;
                    self.idx_put(idx_table, &out_key_bytes, &out_val_bytes)?;

                    let mut in_key = Vec::with_capacity(orig_table.metadata.keys.len() * 2 + 5);
                    let in_val = vec![
                        DataValue::from(*dist),
                        DataValue::Null,
                        DataValue::from(false),
                    ];
                    in_key.push(DataValue::from(current_level));
                    in_key.extend_from_slice(&neighbour.0);
                    in_key.push(DataValue::from(neighbour.1 as i64));
                    in_key.push(DataValue::from(neighbour.2 as i64));
                    in_key.extend_from_slice(tuple_key);
                    in_key.push(DataValue::from(idx as i64));
                    in_key.push(DataValue::from(subidx as i64));

                    let in_key_bytes =
                        idx_table.encode_key_for_store(&in_key, Default::default())?;
                    let in_val_bytes =
                        idx_table.encode_val_only_for_store(&in_val, Default::default())?;
                    self.idx_put(idx_table, &in_key_bytes, &in_val_bytes)?;

                    // shrink links if necessary
                    let mut target_self_key =
                        Vec::with_capacity(orig_table.metadata.keys.len() * 2 + 5);
                    target_self_key.push(DataValue::from(current_level));
                    for _ in 0..2 {
                        target_self_key.extend_from_slice(&neighbour.0);
                        target_self_key.push(DataValue::from(neighbour.1 as i64));
                        target_self_key.push(DataValue::from(neighbour.2 as i64));
                    }
                    let target_self_key_bytes =
                        idx_table.encode_key_for_store(&target_self_key, Default::default())?;
                    let target_self_val_bytes = match self.idx_get(idx_table, &target_self_key_bytes)? {
                        Some(bytes) => bytes,
                        None => bail!("Indexed vector not found, this signifies a bug in the index implementation"),
                    };
                    let mut target_self_val =
                        try_decode_val_only(&target_self_key_bytes, &target_self_val_bytes)?;
                    let mut target_degree = target_self_val
                        .first()
                        .and_then(DataValue::get_float)
                        .ok_or_else(|| {
                        miette!("corrupt HNSW self-row: missing numeric degree")
                    })? as usize
                        + 1;
                    if target_degree > m_max {
                        // shrink links
                        target_degree = self.hnsw_shrink_neighbour(
                            neighbour,
                            m_max,
                            current_level,
                            manifest,
                            idx_table,
                            orig_table,
                            vec_cache,
                        )?;
                    }
                    // update degree
                    target_self_val[0] = DataValue::from(target_degree as f64);
                    let target_self_val_bytes_new = idx_table
                        .encode_val_only_for_store(&target_self_val, Default::default())?;
                    self.idx_put(
                        idx_table,
                        &target_self_key_bytes,
                        &target_self_val_bytes_new,
                    )?;
                }
            }
        } else {
            // This is the first vector in the index.
            let level = manifest.get_random_level();
            self.hnsw_put_fresh_at_levels(
                hash.as_ref(),
                tuple_key,
                idx,
                subidx,
                orig_table,
                idx_table,
                level,
                0,
            )?;
        }
        Ok(())
    }
    fn hnsw_shrink_neighbour(
        &mut self,
        target_key: &CompoundKey,
        m: usize,
        level: i64,
        manifest: &HnswIndexManifest,
        idx_table: &RelationHandle,
        orig_table: &RelationHandle,
        vec_cache: &mut VectorCache,
    ) -> Result<usize> {
        vec_cache.ensure_key(target_key, orig_table, self)?;
        let vec = vec_cache.get_key(target_key).clone();
        let mut candidates = PriorityQueue::new();
        for (neighbour_key, neighbour_dist) in
            self.hnsw_get_neighbours(target_key, level, idx_table, false)?
        {
            candidates.push(neighbour_key, OrderedFloat(neighbour_dist));
        }
        let new_candidates = self.hnsw_select_neighbours_heuristic(
            &vec,
            &candidates,
            m,
            level,
            manifest,
            idx_table,
            orig_table,
            vec_cache,
        )?;
        let mut old_candidate_set = FxHashSet::default();
        for (old, _) in &candidates {
            old_candidate_set.insert(old.clone());
        }
        let mut new_candidate_set = FxHashSet::default();
        for (new, _) in &new_candidates {
            new_candidate_set.insert(new.clone());
        }
        let new_degree = new_candidates.len();
        for (new, Reverse(OrderedFloat(new_dist))) in new_candidates {
            if !old_candidate_set.contains(&new) {
                let mut new_key = Vec::with_capacity(orig_table.metadata.keys.len() * 2 + 5);
                let new_val = vec![
                    DataValue::from(new_dist),
                    DataValue::Null,
                    DataValue::from(false),
                ];
                new_key.push(DataValue::from(level));
                new_key.extend_from_slice(&target_key.0);
                new_key.push(DataValue::from(target_key.1 as i64));
                new_key.push(DataValue::from(target_key.2 as i64));
                new_key.extend_from_slice(&new.0);
                new_key.push(DataValue::from(new.1 as i64));
                new_key.push(DataValue::from(new.2 as i64));
                let new_key_bytes = idx_table.encode_key_for_store(&new_key, Default::default())?;
                let new_val_bytes =
                    idx_table.encode_val_only_for_store(&new_val, Default::default())?;
                self.idx_put(idx_table, &new_key_bytes, &new_val_bytes)?;
            }
        }
        for (old, OrderedFloat(old_dist)) in candidates {
            if !new_candidate_set.contains(&old) {
                let mut old_key = Vec::with_capacity(orig_table.metadata.keys.len() * 2 + 5);
                old_key.push(DataValue::from(level));
                old_key.extend_from_slice(&target_key.0);
                old_key.push(DataValue::from(target_key.1 as i64));
                old_key.push(DataValue::from(target_key.2 as i64));
                old_key.extend_from_slice(&old.0);
                old_key.push(DataValue::from(old.1 as i64));
                old_key.push(DataValue::from(old.2 as i64));
                let old_key_bytes = idx_table.encode_key_for_store(&old_key, Default::default())?;
                let old_existing_val = match self.idx_get(idx_table, &old_key_bytes)? {
                    Some(bytes) => bytes,
                    None => {
                        bail!("Indexed vector not found, this signifies a bug in the index implementation")
                    }
                };
                let old_existing_val = try_decode_val_only(&old_key_bytes, &old_existing_val)?;
                let is_deleted = old_existing_val
                    .get(2)
                    .and_then(DataValue::get_bool)
                    .ok_or_else(|| miette!("corrupt HNSW edge row: missing deletion marker"))?;
                if is_deleted {
                    self.idx_del(idx_table, &old_key_bytes)?;
                } else {
                    let old_val = vec![
                        DataValue::from(old_dist),
                        DataValue::Null,
                        DataValue::from(true),
                    ];
                    let old_val_bytes =
                        idx_table.encode_val_only_for_store(&old_val, Default::default())?;
                    self.idx_put(idx_table, &old_key_bytes, &old_val_bytes)?;
                }
            }
        }

        Ok(new_degree)
    }
    fn hnsw_select_neighbours_heuristic(
        &self,
        q: &Vector,
        found: &PriorityQueue<CompoundKey, OrderedFloat<f64>>,
        m: usize,
        level: i64,
        manifest: &HnswIndexManifest,
        idx_table: &RelationHandle,
        orig_table: &RelationHandle,
        vec_cache: &mut VectorCache,
    ) -> Result<PriorityQueue<CompoundKey, Reverse<OrderedFloat<f64>>>> {
        let mut candidates = PriorityQueue::new();
        // Simple non-heuristic selection
        // let mut temp = found.clone();
        // while temp.len() > m {
        //     temp.pop();
        // }
        // for (item, dist) in temp.iter() {
        //     candidates.push(item.clone(), Reverse(*dist));
        // }
        // return Ok(candidates);
        // End of simple non-heuristic selection

        let mut ret: PriorityQueue<CompoundKey, Reverse<OrderedFloat<_>>> = PriorityQueue::new();
        let mut discarded: PriorityQueue<_, Reverse<OrderedFloat<_>>> = PriorityQueue::new();
        for (item, dist) in found.iter() {
            // Add to candidates
            candidates.push(item.clone(), Reverse(*dist));
        }
        if manifest.extend_candidates {
            for (item, _) in found.iter() {
                // Extend by neighbours
                for (neighbour_key, _) in self.hnsw_get_neighbours(item, level, idx_table, false)? {
                    vec_cache.ensure_key(&neighbour_key, orig_table, self)?;
                    let dist = vec_cache.v_dist(q, &neighbour_key);
                    candidates.push(
                        (neighbour_key.0, neighbour_key.1, neighbour_key.2),
                        Reverse(OrderedFloat(dist)),
                    );
                }
            }
        }
        while !candidates.is_empty() && ret.len() < m {
            let (cand_key, Reverse(OrderedFloat(cand_dist_to_q))) = candidates.pop().unwrap();
            let mut should_add = true;
            for (existing, _) in ret.iter() {
                vec_cache.ensure_key(&cand_key, orig_table, self)?;
                vec_cache.ensure_key(existing, orig_table, self)?;
                let dist_to_existing = vec_cache.k_dist(existing, &cand_key);
                if distance_is_closer(dist_to_existing, cand_dist_to_q) {
                    should_add = false;
                    break;
                }
            }
            if should_add {
                ret.push(cand_key, Reverse(OrderedFloat(cand_dist_to_q)));
            } else if manifest.keep_pruned_connections {
                discarded.push(cand_key, Reverse(OrderedFloat(cand_dist_to_q)));
            }
        }
        if manifest.keep_pruned_connections {
            while !discarded.is_empty() && ret.len() < m {
                let (nearest_triple, Reverse(OrderedFloat(nearest_dist))) =
                    discarded.pop().unwrap();
                ret.push(nearest_triple, Reverse(OrderedFloat(nearest_dist)));
            }
        }
        Ok(ret)
    }
    fn hnsw_search_level(
        &self,
        q: &Vector,
        ef: usize,
        cur_level: i64,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        found_nn: &mut PriorityQueue<CompoundKey, OrderedFloat<f64>>,
        vec_cache: &mut VectorCache,
        poison: Option<&Poison>,
    ) -> Result<()> {
        if let Some(poison) = poison {
            poison.check()?;
        }
        let mut visited: FxHashSet<CompoundKey> = FxHashSet::default();
        // min queue
        let mut candidates: PriorityQueue<CompoundKey, Reverse<OrderedFloat<f64>>> =
            PriorityQueue::new();

        for item in found_nn.iter() {
            visited.insert(item.0.clone());
            candidates.push(item.0.clone(), Reverse(*item.1));
        }

        while let Some((candidate, Reverse(OrderedFloat(candidate_dist)))) = candidates.pop() {
            if let Some(poison) = poison {
                poison.check()?;
            }
            let (_, OrderedFloat(furthest_dist)) = found_nn.peek().unwrap();
            if distance_is_farther(candidate_dist, *furthest_dist) {
                break;
            }
            // Fetch all unvisited neighbours' vectors in one batched read
            // (mnestic fork): on the snapshot read path this is one RocksDB
            // MultiGet instead of a serial point get per neighbour.
            let mut unvisited = Vec::new();
            for (index, (key, _)) in self
                .hnsw_get_neighbours(&candidate, cur_level, idx_table, false)?
                .enumerate()
            {
                if index & 63 == 0 {
                    if let Some(poison) = poison {
                        poison.check()?;
                    }
                }
                if !visited.contains(&key) {
                    unvisited.push(key);
                }
            }
            vec_cache.ensure_keys(&unvisited, orig_table, self)?;
            if let Some(poison) = poison {
                poison.check()?;
            }
            for (index, neighbour_key) in unvisited.into_iter().enumerate() {
                if index & 63 == 0 {
                    if let Some(poison) = poison {
                        poison.check()?;
                    }
                }
                if visited.contains(&neighbour_key) {
                    continue;
                }
                let neighbour_dist = vec_cache.v_dist(q, &neighbour_key);
                let (_, OrderedFloat(candidate_furthest_dist)) = found_nn.peek().unwrap();
                if found_nn.len() < ef
                    || distance_is_closer(neighbour_dist, *candidate_furthest_dist)
                {
                    candidates.push(neighbour_key.clone(), Reverse(OrderedFloat(neighbour_dist)));
                    found_nn.push(neighbour_key.clone(), OrderedFloat(neighbour_dist));
                    if found_nn.len() > ef {
                        found_nn.pop();
                    }
                }
                visited.insert(neighbour_key);
            }
        }

        Ok(())
    }
    fn hnsw_get_neighbours<'b>(
        &'b self,
        cand_key: &'b CompoundKey,
        level: i64,
        idx_handle: &RelationHandle,
        include_deleted: bool,
    ) -> Result<impl Iterator<Item = (CompoundKey, f64)> + 'b> {
        let mut start_tuple = Vec::with_capacity(cand_key.0.len() + 3);
        start_tuple.push(DataValue::from(level));
        start_tuple.extend_from_slice(&cand_key.0);
        start_tuple.push(DataValue::from(cand_key.1 as i64));
        start_tuple.push(DataValue::from(cand_key.2 as i64));
        let key_len = cand_key.0.len();
        let neighbours = idx_handle
            .scan_prefix(self, &start_tuple)
            .map(|res| {
                let tuple = res?;
                let expected_len = 2 * key_len + 8;
                if tuple.len() < expected_len {
                    bail!(
                        "corrupt HNSW edge row: expected at least {expected_len} fields, got {}",
                        tuple.len()
                    );
                }
                let key_idx = tuple[2 * key_len + 3].get_int().ok_or_else(|| {
                    miette!("corrupt HNSW edge row: vector index is not an integer")
                })? as usize;
                let key_subidx = tuple[2 * key_len + 4].get_int().ok_or_else(|| {
                    miette!("corrupt HNSW edge row: vector sub-index is not an integer")
                })? as i32;
                let key_tup = tuple[key_len + 3..2 * key_len + 3].to_vec();
                if key_tup == cand_key.0 {
                    return Ok(None);
                }
                let distance = tuple[2 * key_len + 5]
                    .get_float()
                    .ok_or_else(|| miette!("corrupt HNSW edge row: distance is not numeric"))?;
                if include_deleted {
                    return Ok(Some(((key_tup, key_idx, key_subidx), distance)));
                }
                let is_deleted = tuple[2 * key_len + 7].get_bool().ok_or_else(|| {
                    miette!("corrupt HNSW edge row: deletion marker is not boolean")
                })?;
                Ok((!is_deleted).then_some(((key_tup, key_idx, key_subidx), distance)))
            })
            .filter_map(|res| res.transpose())
            .collect::<Result<Vec<_>>>()?;
        Ok(neighbours.into_iter())
    }
    fn hnsw_put_fresh_at_levels(
        &mut self,
        hash: &[u8],
        tuple: &[DataValue],
        idx: usize,
        subidx: i32,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        bottom_level: i64,
        top_level: i64,
    ) -> Result<()> {
        let mut target_key = vec![DataValue::Null];
        let mut canary_key = vec![DataValue::from(1)];
        for _ in 0..2 {
            for i in 0..orig_table.metadata.keys.len() {
                target_key.push(tuple.get(i).unwrap().clone());
                canary_key.push(DataValue::Null);
            }
            target_key.push(DataValue::from(idx as i64));
            target_key.push(DataValue::from(subidx as i64));
            canary_key.push(DataValue::Null);
            canary_key.push(DataValue::Null);
        }
        let target_value = [
            DataValue::from(0.0),
            DataValue::Bytes(hash.to_vec()),
            DataValue::from(false),
        ];
        let target_key_bytes = idx_table.encode_key_for_store(&target_key, Default::default())?;

        // canary value is for conflict detection: prevent the scenario of disconnected graphs at all levels
        let canary_value = [
            DataValue::from(bottom_level),
            DataValue::Bytes(target_key_bytes),
            DataValue::from(false),
        ];
        let canary_key_bytes = idx_table.encode_key_for_store(&canary_key, Default::default())?;
        let canary_value_bytes =
            idx_table.encode_val_only_for_store(&canary_value, Default::default())?;
        self.idx_put(idx_table, &canary_key_bytes, &canary_value_bytes)?;

        for cur_level in bottom_level..=top_level {
            target_key[0] = DataValue::from(cur_level);
            let key = idx_table.encode_key_for_store(&target_key, Default::default())?;
            let val = idx_table.encode_val_only_for_store(&target_value, Default::default())?;
            self.idx_put(idx_table, &key, &val)?;
        }
        Ok(())
    }
    pub(crate) fn hnsw_put(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        filter: Option<&Vec<Bytecode>>,
        stack: &mut Vec<DataValue>,
        tuple: &[DataValue],
    ) -> Result<bool> {
        // Steady-state path (single upsert): a fresh per-call cache, because a
        // batch can update a vector that is also some other row's neighbour, and
        // a cache shared across the batch could then serve a stale vector.
        let mut vec_cache = VectorCache {
            cache: FxHashMap::default(),
            distance: manifest.distance,
        };
        self.hnsw_put_inner(
            manifest,
            orig_table,
            idx_table,
            filter,
            stack,
            tuple,
            &mut vec_cache,
        )
    }

    /// Serialise a compact bulk build into the index relation's tuple format in
    /// one pass. Produces the same row layout as the incremental path
    /// (`hnsw_put_vector`): per-level self rows carrying degree + vector hash,
    /// directed edge rows carrying distances, and the entry-point canary.
    pub(crate) fn hnsw_write_build(
        &mut self,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        build: HnswBuild,
    ) -> Result<HnswBuildSnapshot> {
        let HnswBuild {
            graph,
            snapshot,
            stats,
            scan_elapsed,
            graph_elapsed,
        } = build;
        let key_len = orig_table.metadata.keys.len();
        let serialise_start = Instant::now();

        // Serialise: per node × level, one self row + one row per out-edge.
        let mut key_buf: Vec<DataValue> = Vec::with_capacity(2 * key_len + 5);
        if let Some(graph) = graph {
            for (i, meta) in graph.metas.iter().enumerate() {
                for (d, list) in graph.towers[i].iter().enumerate() {
                    let level = -(d as i64);
                    key_buf.clear();
                    key_buf.push(DataValue::from(level));
                    for _ in 0..2 {
                        key_buf.extend_from_slice(&meta.key);
                        key_buf.push(DataValue::from(meta.field_idx as i64));
                        key_buf.push(DataValue::from(meta.sub_idx as i64));
                    }
                    let self_val = [
                        DataValue::from(list.len() as f64),
                        DataValue::Bytes(meta.hash.clone()),
                        DataValue::from(false),
                    ];
                    let k = idx_table.encode_key_for_store(&key_buf, Default::default())?;
                    let v = idx_table.encode_val_only_for_store(&self_val, Default::default())?;
                    self.idx_put(idx_table, &k, &v)?;

                    for (nb, dist, ignore_link) in list
                        .iter()
                        .map(|&(nb, dist)| (nb, dist, false))
                        .chain(graph.dead[i][d].iter().map(|&(nb, dist)| (nb, dist, true)))
                    {
                        let nb_meta = &graph.metas[nb as usize];
                        key_buf.clear();
                        key_buf.push(DataValue::from(level));
                        key_buf.extend_from_slice(&meta.key);
                        key_buf.push(DataValue::from(meta.field_idx as i64));
                        key_buf.push(DataValue::from(meta.sub_idx as i64));
                        key_buf.extend_from_slice(&nb_meta.key);
                        key_buf.push(DataValue::from(nb_meta.field_idx as i64));
                        key_buf.push(DataValue::from(nb_meta.sub_idx as i64));
                        let edge_val = [
                            DataValue::from(dist),
                            DataValue::Null,
                            DataValue::from(ignore_link),
                        ];
                        let k = idx_table.encode_key_for_store(&key_buf, Default::default())?;
                        let v =
                            idx_table.encode_val_only_for_store(&edge_val, Default::default())?;
                        self.idx_put(idx_table, &k, &v)?;
                    }
                }
            }

            // Entry-point canary, as written by `hnsw_put_fresh_at_levels`.
            if let Some((ep, ep_top)) = graph.entry {
                let ep_meta = &graph.metas[ep as usize];
                let mut target_key = vec![DataValue::Null];
                let mut canary_key = vec![DataValue::from(1)];
                for _ in 0..2 {
                    for k in ep_meta.key.iter() {
                        target_key.push(k.clone());
                        canary_key.push(DataValue::Null);
                    }
                    target_key.push(DataValue::from(ep_meta.field_idx as i64));
                    target_key.push(DataValue::from(ep_meta.sub_idx as i64));
                    canary_key.push(DataValue::Null);
                    canary_key.push(DataValue::Null);
                }
                let target_key_bytes =
                    idx_table.encode_key_for_store(&target_key, Default::default())?;
                let canary_value = [
                    DataValue::from(ep_top),
                    DataValue::Bytes(target_key_bytes),
                    DataValue::from(false),
                ];
                let canary_key_bytes =
                    idx_table.encode_key_for_store(&canary_key, Default::default())?;
                let canary_value_bytes =
                    idx_table.encode_val_only_for_store(&canary_value, Default::default())?;
                self.idx_put(idx_table, &canary_key_bytes, &canary_value_bytes)?;
            }
        }
        let serialise_elapsed = serialise_start.elapsed();
        if std::env::var_os("MNESTIC_BUILD_PROFILE").is_some() {
            eprintln!(
                "hnsw bulk build: scanned {} rows, eligible {}, indexed {}, retained {} vectors / {} bytes, snapshot {} rows; scan {:?}, graph {:?}, serialise {:?}",
                stats.rows_scanned,
                stats.rows_eligible,
                stats.indexed_rows,
                stats.vectors_retained,
                stats.vector_bytes_retained,
                stats.snapshot_rows,
                scan_elapsed,
                graph_elapsed,
                serialise_elapsed,
            );
        }
        record_hnsw_build_stats(stats);
        Ok(snapshot)
    }

    #[allow(clippy::too_many_arguments)]
    fn hnsw_put_inner(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        filter: Option<&Vec<Bytecode>>,
        stack: &mut Vec<DataValue>,
        tuple: &[DataValue],
        vec_cache: &mut VectorCache,
    ) -> Result<bool> {
        if let Some(code) = filter {
            if !eval_bytecode_pred(code, tuple, stack, Default::default())? {
                self.hnsw_remove(orig_table, idx_table, tuple)?;
                return Ok(false);
            }
        }
        let mut extracted_vectors = vec![];
        for idx in &manifest.vec_fields {
            let val = tuple.get(*idx).ok_or_else(|| {
                miette!(
                    "corrupt base row for HNSW index '{}:{}': vector field column {} is missing from a {}-value tuple",
                    manifest.base_relation,
                    manifest.index_name,
                    idx + 1,
                    tuple.len()
                )
            })?;
            if let DataValue::Vec(v) = val {
                extracted_vectors.push((v, *idx, -1));
            } else if let DataValue::List(l) = val {
                for (sidx, v) in l.iter().enumerate() {
                    if let DataValue::Vec(v) = v {
                        extracted_vectors.push((v, *idx, sidx as i32));
                    }
                }
            }
        }
        let keep: FxHashSet<(usize, i32)> = extracted_vectors
            .iter()
            .map(|(_, idx, sub)| (*idx, *sub))
            .collect();
        self.hnsw_remove_except(orig_table, idx_table, tuple, &keep)?;
        if extracted_vectors.is_empty() {
            return Ok(false);
        }
        for (vec, idx, sub) in extracted_vectors {
            self.hnsw_put_vector(
                tuple, vec, idx, sub, manifest, orig_table, idx_table, vec_cache,
            )?;
        }
        Ok(true)
    }
    pub(crate) fn hnsw_remove(
        &mut self,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        tuple: &[DataValue],
    ) -> Result<()> {
        self.hnsw_remove_except(orig_table, idx_table, tuple, &FxHashSet::default())
    }

    fn hnsw_remove_except(
        &mut self,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        tuple: &[DataValue],
        keep: &FxHashSet<(usize, i32)>,
    ) -> Result<()> {
        let mut prefix = vec![DataValue::from(0)];
        prefix.extend_from_slice(&tuple[0..orig_table.metadata.keys.len()]);
        let candidates: FxHashSet<_> = idx_table
            .scan_prefix(self, &prefix)
            .map(|t| {
                let t = t?;
                let key_len = orig_table.metadata.keys.len();
                if t.len() < key_len + 3 {
                    bail!(
                        "corrupt HNSW self-row: expected at least {} fields, got {}",
                        key_len + 3,
                        t.len()
                    );
                }
                let idx = t[key_len + 1].get_int().ok_or_else(|| {
                    miette!("corrupt HNSW self-row: vector index is not an integer")
                })? as usize;
                let subidx = t[key_len + 2].get_int().ok_or_else(|| {
                    miette!("corrupt HNSW self-row: vector sub-index is not an integer")
                })? as i32;
                Ok((t[1..key_len + 1].to_vec(), idx, subidx))
            })
            .collect::<Result<_>>()?;
        for (tuple_key, idx, subidx) in candidates {
            if !keep.contains(&(idx, subidx)) {
                self.hnsw_remove_vec(&tuple_key, idx, subidx, orig_table, idx_table)?;
            }
        }
        Ok(())
    }
    fn hnsw_remove_vec(
        &mut self,
        tuple_key: &[DataValue],
        idx: usize,
        subidx: i32,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
    ) -> Result<()> {
        let compound_key = (tuple_key.to_vec(), idx, subidx);
        // Go down the layers and remove all the links
        let mut encountered_singletons = false;
        for neg_layer in 0i64.. {
            let layer = -neg_layer;
            let mut self_key = vec![DataValue::from(layer)];
            for _ in 0..2 {
                self_key.extend_from_slice(tuple_key);
                self_key.push(DataValue::from(idx as i64));
                self_key.push(DataValue::from(subidx as i64));
            }
            let self_key_bytes = idx_table.encode_key_for_store(&self_key, Default::default())?;
            if self.idx_exists(idx_table, &self_key_bytes)? {
                self.idx_del(idx_table, &self_key_bytes)?;
            } else {
                break;
            }

            let neigbours = self
                .hnsw_get_neighbours(&compound_key, layer, idx_table, true)?
                .collect_vec();
            encountered_singletons |= neigbours.is_empty();
            for (neighbour_key, _) in neigbours {
                // REMARK: this still has some probability of disconnecting the graph.
                // Should we accept that as a consequence of the probabilistic nature of the algorithm?
                let mut out_key = vec![DataValue::from(layer)];
                out_key.extend_from_slice(tuple_key);
                out_key.push(DataValue::from(idx as i64));
                out_key.push(DataValue::from(subidx as i64));
                out_key.extend_from_slice(&neighbour_key.0);
                out_key.push(DataValue::from(neighbour_key.1 as i64));
                out_key.push(DataValue::from(neighbour_key.2 as i64));
                let out_key_bytes = idx_table.encode_key_for_store(&out_key, Default::default())?;
                self.idx_del(idx_table, &out_key_bytes)?;
                let mut in_key = vec![DataValue::from(layer)];
                in_key.extend_from_slice(&neighbour_key.0);
                in_key.push(DataValue::from(neighbour_key.1 as i64));
                in_key.push(DataValue::from(neighbour_key.2 as i64));
                in_key.extend_from_slice(tuple_key);
                in_key.push(DataValue::from(idx as i64));
                in_key.push(DataValue::from(subidx as i64));
                let in_key_bytes = idx_table.encode_key_for_store(&in_key, Default::default())?;
                // Shrinking can prune just one direction of a paired edge.
                // include_deleted above is needed to clean up both rows, but
                // only a live neighbour -> removed-vector link contributes to
                // that neighbour's degree. Inspect it before deleting it.
                let incoming_is_live = match self.idx_get(idx_table, &in_key_bytes)? {
                    Some(bytes) => {
                        let value = try_decode_val_only(&in_key_bytes, &bytes)?;
                        !value.get(2).and_then(DataValue::get_bool).ok_or_else(|| {
                            miette!("corrupt HNSW edge row: missing deletion marker")
                        })?
                    }
                    None => false,
                };
                self.idx_del(idx_table, &in_key_bytes)?;
                let mut neighbour_self_key = vec![DataValue::from(layer)];
                for _ in 0..2 {
                    neighbour_self_key.extend_from_slice(&neighbour_key.0);
                    neighbour_self_key.push(DataValue::from(neighbour_key.1 as i64));
                    neighbour_self_key.push(DataValue::from(neighbour_key.2 as i64));
                }
                let neighbour_self_key_bytes =
                    idx_table.encode_key_for_store(&neighbour_self_key, Default::default())?;
                let neighbour_val_bytes = self
                    .idx_get(idx_table, &neighbour_self_key_bytes)?
                    .ok_or_else(|| miette!("corrupt HNSW index: neighbour self-row is missing"))?;
                let mut neighbour_val =
                    try_decode_val_only(&neighbour_self_key_bytes, &neighbour_val_bytes)?;
                let degree = neighbour_val
                    .first()
                    .and_then(DataValue::get_float)
                    .ok_or_else(|| miette!("corrupt HNSW self-row: missing numeric degree"))?;
                neighbour_val[0] = DataValue::from(degree - if incoming_is_live { 1. } else { 0. });
                let neighbour_val_bytes_new =
                    idx_table.encode_val_only_for_store(&neighbour_val, Default::default())?;
                self.idx_put(
                    idx_table,
                    &neighbour_self_key_bytes,
                    &neighbour_val_bytes_new,
                )?;
            }
        }

        if encountered_singletons {
            // the entry point is removed, we need to do something
            let ep_res = idx_table
                .scan_bounded_prefix(
                    self,
                    &[],
                    &[DataValue::from(i64::MIN)],
                    &[DataValue::from(1)],
                )
                .next();
            let mut canary_key = vec![DataValue::from(1)];
            for _ in 0..2 {
                for _ in 0..orig_table.metadata.keys.len() {
                    canary_key.push(DataValue::Null);
                }
                canary_key.push(DataValue::Null);
                canary_key.push(DataValue::Null);
            }
            let canary_key_bytes =
                idx_table.encode_key_for_store(&canary_key, Default::default())?;
            if let Some(ep) = ep_res {
                let ep = ep?;
                let target_key_bytes = idx_table.encode_key_for_store(&ep, Default::default())?;
                let bottom_level = ep[0].get_int().unwrap();
                // canary value is for conflict detection: prevent the scenario of disconnected graphs at all levels
                let canary_value = [
                    DataValue::from(bottom_level),
                    DataValue::Bytes(target_key_bytes),
                    DataValue::from(false),
                ];
                let canary_value_bytes =
                    idx_table.encode_val_only_for_store(&canary_value, Default::default())?;
                self.idx_put(idx_table, &canary_key_bytes, &canary_value_bytes)?;
            } else {
                // HA! we have removed the last item in the index
                self.idx_del(idx_table, &canary_key_bytes)?;
            }
        }

        Ok(())
    }
    pub(crate) fn hnsw_knn(
        &self,
        q: Vector,
        config: &HnswSearch,
        filter_bytecode: &Option<(Vec<Bytecode>, SourceSpan)>,
        stack: &mut Vec<DataValue>,
        poison: &Poison,
    ) -> Result<Vec<Tuple>> {
        poison.check()?;
        if q.len() != config.manifest.vec_dim {
            bail!("query vector dimension mismatch");
        }
        let q = match (q, config.manifest.dtype) {
            (v @ Vector::F32(_), VecElementType::F32) => v,
            (v @ Vector::F64(_), VecElementType::F64) => v,
            (Vector::F32(v), VecElementType::F64) => Vector::F64(v.mapv(|x| x as f64)),
            (Vector::F64(v), VecElementType::F32) => Vector::F32(v.mapv(|x| x as f32)),
        };
        poison.check()?;

        let mut vec_cache = VectorCache {
            cache: Default::default(),
            distance: config.manifest.distance,
        };

        let ep_res = config
            .idx_handle
            .scan_bounded_prefix(
                self,
                &[],
                &[DataValue::from(i64::MIN)],
                &[DataValue::from(1)],
            )
            .next();
        poison.check()?;
        if let Some(ep) = ep_res {
            let ep = ep?;
            let bottom_level = ep[0].get_int().unwrap();
            let ep_idx = match ep[config.base_handle.metadata.keys.len() + 1].get_int() {
                Some(x) => x as usize,
                None => {
                    // this occurs if the index is empty
                    return Ok(vec![]);
                }
            };
            let ep_t_key = ep[1..config.base_handle.metadata.keys.len() + 1].to_vec();
            let ep_subidx = ep[config.base_handle.metadata.keys.len() + 2]
                .get_int()
                .unwrap() as i32;
            let ep_key = (ep_t_key, ep_idx, ep_subidx);
            vec_cache.ensure_key(&ep_key, &config.base_handle, self)?;
            poison.check()?;
            let ep_distance = vec_cache.v_dist(&q, &ep_key);
            let mut found_nn = PriorityQueue::new();
            found_nn.push(ep_key, OrderedFloat(ep_distance));
            for current_level in bottom_level..0 {
                poison.check()?;
                self.hnsw_search_level(
                    &q,
                    1,
                    current_level,
                    &config.base_handle,
                    &config.idx_handle,
                    &mut found_nn,
                    &mut vec_cache,
                    Some(poison),
                )?;
            }
            self.hnsw_search_level(
                &q,
                config.ef,
                0,
                &config.base_handle,
                &config.idx_handle,
                &mut found_nn,
                &mut vec_cache,
                Some(poison),
            )?;
            if found_nn.is_empty() {
                return Ok(vec![]);
            }

            if config.filter.is_none() {
                while found_nn.len() > config.k {
                    found_nn.pop();
                }
            }

            let mut ret = vec![];

            let mut inspected = 0usize;
            while let Some((cand_key, OrderedFloat(distance))) = found_nn.pop() {
                if inspected & 63 == 0 {
                    poison.check()?;
                }
                inspected += 1;
                if let Some(r) = config.radius {
                    if distance > r {
                        continue;
                    }
                }

                let mut cand_tuple = config
                    .base_handle
                    .get(self, &cand_key.0)?
                    .ok_or_else(|| miette!("corrupted index"))?;

                // make sure the order is the same as in all_bindings()!!!
                if config.bind_field.is_some() {
                    let field = if cand_key.1 < config.base_handle.metadata.keys.len() {
                        config.base_handle.metadata.keys[cand_key.1].name.clone()
                    } else {
                        config.base_handle.metadata.non_keys
                            [cand_key.1 - config.base_handle.metadata.keys.len()]
                        .name
                        .clone()
                    };
                    cand_tuple.push(DataValue::Str(field));
                }
                if config.bind_field_idx.is_some() {
                    cand_tuple.push(if cand_key.2 < 0 {
                        DataValue::Null
                    } else {
                        DataValue::from(cand_key.2 as i64)
                    });
                }
                if config.bind_distance.is_some() {
                    cand_tuple.push(DataValue::from(distance));
                }
                if config.bind_vector.is_some() {
                    let vec = if cand_key.2 < 0 {
                        cand_tuple[cand_key.1].clone()
                    } else {
                        match &cand_tuple[cand_key.1] {
                            DataValue::List(v) => v[cand_key.2 as usize].clone(),
                            v => bail!("corrupted index value {:?}", v),
                        }
                    };
                    cand_tuple.push(vec);
                }

                if let Some((code, span)) = filter_bytecode {
                    if !eval_bytecode_pred(code, &cand_tuple, stack, *span)? {
                        continue;
                    }
                }

                ret.push(cand_tuple);
            }
            ret.reverse();
            ret.truncate(config.k);
            poison.check()?;

            Ok(ret)
        } else {
            Ok(vec![])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, DbInstance, Storage};
    use rand::Rng;
    use std::collections::BTreeMap;

    // Seed valid contiguous towers, retaining the real self metadata/hashes.
    // All vectors are identical, so every edge's true cosine distance is zero.
    // Nodes 1/2/3 have levels 0 and -1; node 3 alone has level -2 and is the
    // entrypoint. Pair 1<->2 is pruned at both shared levels. This models the
    // stored paired-row contract, not the stochastic history of neighbour selection.
    fn seed_pruned_pair<'s, S: Storage<'s>>(db: &'s Db<S>, pruned: &[(i64, i64)]) {
        let mut tx = db.transact_write().unwrap();
        let base = tx.get_relation("pts", false).unwrap();
        let idx = &base.hnsw_indices["search"].0;
        let key = |level, source, target| {
            Vec::from([level, source, 1, -1, target, 1, -1].map(DataValue::from))
        };
        let self_values: BTreeMap<_, _> = (1..=3)
            .map(|id| {
                let encoded = idx
                    .encode_key_for_store(&key(0, id, id), Default::default())
                    .unwrap();
                let bytes = tx.idx_get(idx, &encoded).unwrap().unwrap();
                (id, try_decode_val_only(&encoded, &bytes).unwrap())
            })
            .collect();
        let old_rows = idx.scan_all(&tx).collect::<Result<Vec<_>>>().unwrap();
        for row in old_rows {
            let encoded = idx
                .encode_key_for_store(&row[..7], Default::default())
                .unwrap();
            tx.idx_del(idx, &encoded).unwrap();
        }
        for level in [-1, 0] {
            for source in 1..=3 {
                for target in 1..=3 {
                    let value = if source == target {
                        let mut value = self_values[&source].clone();
                        let degree = 2 - pruned.iter().filter(|(from, _)| *from == source).count();
                        value[0] = DataValue::from(degree as f64);
                        assert_eq!(value[2], DataValue::from(false));
                        value
                    } else {
                        vec![
                            DataValue::from(0.0),
                            DataValue::Null,
                            DataValue::from(pruned.contains(&(source, target))),
                        ]
                    };
                    let encoded = idx
                        .encode_key_for_store(&key(level, source, target), Default::default())
                        .unwrap();
                    let bytes = idx
                        .encode_val_only_for_store(&value, Default::default())
                        .unwrap();
                    tx.idx_put(idx, &encoded, &bytes).unwrap();
                }
            }
        }
        let mut value = self_values[&3].clone();
        value[0] = DataValue::from(0.0);
        let encoded = idx
            .encode_key_for_store(&key(-2, 3, 3), Default::default())
            .unwrap();
        let bytes = idx
            .encode_val_only_for_store(&value, Default::default())
            .unwrap();
        tx.idx_put(idx, &encoded, &bytes).unwrap();

        // Match hnsw_put_fresh_at_levels' canary encoding, including its null
        // target level (the level itself is recorded in the canary value).
        let mut target = key(-2, 3, 3);
        target[0] = DataValue::Null;
        let target_bytes = idx
            .encode_key_for_store(&target, Default::default())
            .unwrap();
        let mut canary = vec![DataValue::Null; 7];
        canary[0] = DataValue::from(1);
        let encoded = idx
            .encode_key_for_store(&canary, Default::default())
            .unwrap();
        let value = [
            DataValue::from(-2),
            DataValue::Bytes(target_bytes),
            DataValue::from(false),
        ];
        let bytes = idx
            .encode_val_only_for_store(&value, Default::default())
            .unwrap();
        tx.idx_put(idx, &encoded, &bytes).unwrap();
        tx.commit_tx().unwrap();
    }

    fn assert_live_degrees(db: &DbInstance, contains_one: bool) {
        let exported = db
            .export_relations(["pts", "pts:search"].into_iter())
            .unwrap();
        let rows = &exported["pts:search"].rows;
        let self_rows: Vec<_> = rows
            .iter()
            .filter(|row| row[0].get_int().unwrap() <= 0 && row[1..4] == row[4..7])
            .collect();
        assert!(!self_rows.is_empty(), "degree audit must not be vacuous");
        for row in &self_rows {
            let level = row[0].get_int().unwrap();
            let id = row[1].get_int().unwrap();
            assert_eq!(row[2..4], [DataValue::from(1), DataValue::from(-1)]);
            assert!(
                id != 1 || contains_one,
                "removed vector still has a self row"
            );
            let base_row = exported["pts"]
                .rows
                .iter()
                .find(|base| base[0] == row[1])
                .unwrap();
            assert_eq!(base_row[2], DataValue::from("active"));
            let DataValue::Vec(vector) = &base_row[1] else {
                panic!("missing vector")
            };
            assert_eq!(
                row[8],
                DataValue::Bytes(vector.get_hash().as_ref().to_vec())
            );
            assert_eq!(row[9], DataValue::from(false));
            let live = rows
                .iter()
                .filter(|edge| {
                    edge[0..4] == row[0..4]
                        && edge[1..4] != edge[4..7]
                        && edge[9] == DataValue::from(false)
                })
                .count();
            assert_eq!(
                row[7].get_float().unwrap(),
                live as f64,
                "self degree disagrees with live links: {row:?}"
            );
            for lower in level..=0 {
                assert!(
                    self_rows
                        .iter()
                        .any(|candidate| candidate[0] == DataValue::from(lower)
                            && candidate[1..4] == row[1..4]),
                    "non-contiguous tower: {row:?}"
                );
            }
        }
        assert_eq!(
            self_rows
                .iter()
                .any(|row| row[0] == DataValue::from(0) && row[1] == DataValue::from(1)),
            contains_one
        );
        // Survivors keep real nonzero upper-level links even after deletion or
        // filter exit; a level-zero-only or empty graph cannot pass this audit.
        for id in [2, 3] {
            for level in [-1, 0] {
                let row = self_rows
                    .iter()
                    .find(|row| row[0] == DataValue::from(level) && row[1] == DataValue::from(id))
                    .unwrap();
                assert!(row[7].get_float().unwrap() >= 1.0);
            }
        }
        for edge in rows
            .iter()
            .filter(|row| row[0].get_int().unwrap() <= 0 && row[1..4] != row[4..7])
        {
            for endpoint in [&edge[1..4], &edge[4..7]] {
                assert!(self_rows
                    .iter()
                    .any(|row| row[0] == edge[0] && &row[1..4] == endpoint));
            }
            assert!(
                rows.iter().any(|reverse| reverse[0] == edge[0]
                    && reverse[1..4] == edge[4..7]
                    && reverse[4..7] == edge[1..4]),
                "unpaired stored edge: {edge:?}"
            );
        }
        let canaries: Vec<_> = rows
            .iter()
            .filter(|row| row[0] == DataValue::from(1))
            .collect();
        assert_eq!(canaries.len(), 1);
        let top = self_rows
            .iter()
            .map(|row| row[0].get_int().unwrap())
            .min()
            .unwrap();
        assert_eq!(canaries[0][7].get_int().unwrap(), top);
        assert!(top <= -2, "surviving entrypoint tower was lost");
        #[allow(unreachable_patterns)]
        match db {
            DbInstance::Mem(inner) => assert_entrypoint_target(inner),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(inner) => assert_entrypoint_target(inner),
            _ => unreachable!(),
        }
    }

    fn assert_entrypoint_target<'s, S: Storage<'s>>(db: &'s Db<S>) {
        let tx = db.transact().unwrap();
        let base = tx.get_relation("pts", false).unwrap();
        let idx = &base.hnsw_indices["search"].0;
        let rows = idx.scan_all(&tx).collect::<Result<Vec<_>>>().unwrap();
        let canary = rows
            .iter()
            .find(|row| row[0] == DataValue::from(1))
            .unwrap();
        assert!(canary[1..7].iter().all(|value| *value == DataValue::Null));
        assert_eq!(canary[9], DataValue::from(false));
        let DataValue::Bytes(target) = &canary[8] else {
            panic!("missing entrypoint target")
        };
        // Bulk/fresh insertion uses a null target level; removal repair uses
        // the actual top level. Both must identify an extant top self row.
        assert!(
            rows.iter()
                .filter(|row| row[0] == canary[7] && row[1..4] == row[4..7])
                .any(|row| {
                    let mut key = row[..7].to_vec();
                    let actual = idx.encode_key_for_store(&key, Default::default()).unwrap();
                    key[0] = DataValue::Null;
                    let fresh = idx.encode_key_for_store(&key, Default::default()).unwrap();
                    target == &actual || target == &fresh
                }),
            "canary does not reference a surviving top self row"
        );
    }

    fn remove_counts_only_live_reverse_links(engine: &str) {
        for action in ["delete", "replace", "filter-exit"] {
            for pruned in [&[(1, 2)][..], &[(2, 1)][..], &[(1, 2), (2, 1)][..]] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("degree.db");
                let db = DbInstance::new(engine, path.to_str().unwrap(), "").unwrap();
                db.run_default(":create pts {id: Int => emb: <F32; 2>, status: String}")
                    .unwrap();
                db.run_default("?[id, emb, status] <- [[1, vec([1.0, 0.0]), 'active'], [2, vec([1.0, 0.0]), 'active'], [3, vec([1.0, 0.0]), 'active']] :put pts {id => emb, status}").unwrap();
                db.run_default("::hnsw create pts:search {dim: 2, m: 8, ef_construction: 32, fields: [emb], distance: Cosine, filter: status == 'active', keep_pruned_connections: true}").unwrap();
                #[allow(unreachable_patterns)]
                match &db {
                    DbInstance::Mem(inner) => seed_pruned_pair(inner, pruned),
                    #[cfg(feature = "storage-sqlite")]
                    DbInstance::Sqlite(inner) => seed_pruned_pair(inner, pruned),
                    _ => unreachable!(),
                }
                assert_live_degrees(&db, true);
                println!("degree regression: engine={engine} action={action} pruned={pruned:?}");
                db.run_default(match action {
                    "delete" => "?[id] <- [[1]] :rm pts {id}",
                    "replace" => "?[id, emb, status] <- [[1, vec([0.0, 1.0]), 'active']] :put pts {id => emb, status}",
                    _ => "?[id, emb, status] <- [[1, vec([1.0, 0.0]), 'archived']] :put pts {id => emb, status}",
                }).unwrap();
                assert_live_degrees(&db, action == "replace");
                if engine == "sqlite" {
                    let snapshot = db
                        .export_relations(["pts", "pts:search"].into_iter())
                        .unwrap();
                    drop(db);
                    let reopened = DbInstance::new(engine, path.to_str().unwrap(), "").unwrap();
                    let restored = reopened
                        .export_relations(["pts", "pts:search"].into_iter())
                        .unwrap();
                    for relation in ["pts", "pts:search"] {
                        assert_eq!(restored[relation].headers, snapshot[relation].headers);
                        assert_eq!(restored[relation].rows, snapshot[relation].rows);
                    }
                    assert_live_degrees(&reopened, action == "replace");
                    assert_survivor_search(&reopened, action);
                } else {
                    assert_survivor_search(&db, action);
                }
            }
        }
    }

    fn assert_survivor_search(db: &DbInstance, action: &str) {
        let hits = db
            .run_default("?[id] := ~pts:search{id | query: vec([1.0, 0.0]), k: 3, ef: 32}")
            .unwrap();
        assert!(!hits.rows.is_empty());
        if action != "replace" {
            assert!(hits.rows.iter().all(|row| row[0] != DataValue::from(1)));
        }
    }

    #[test]
    fn hnsw_remove_counts_only_live_reverse_links_mem() {
        remove_counts_only_live_reverse_links("mem");
    }

    #[cfg(feature = "storage-sqlite")]
    #[test]
    fn hnsw_remove_counts_only_live_reverse_links_sqlite() {
        remove_counts_only_live_reverse_links("sqlite");
    }

    #[test]
    fn test_random_level() {
        let m = 20;
        let mult = 1. / (m as f64).ln();
        let mut rng = rand::thread_rng();
        let mut collected = BTreeMap::new();
        for _ in 0..10000 {
            let uniform_num: f64 = rng.gen_range(0.0..1.0);
            let r = -uniform_num.ln() * mult;
            // the level is the largest integer smaller than r
            let level = -(r.floor() as i64);
            collected.entry(level).and_modify(|x| *x += 1).or_insert(1);
        }
        println!("{:?}", collected);
    }
}
