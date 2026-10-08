/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
*/

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};

use itertools::Itertools;
use log::error;
use miette::{bail, ensure, Diagnostic, IntoDiagnostic, Result};
use pest::Parser;
use rmp_serde::Serializer;
use serde::Serialize;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Bytecode;
use crate::data::memcmp::{stored_key_values_encoded_len, MemCmpEncoder, MAX_ENCODED_KEY_BYTES};
use crate::data::msgpack::{decode_exact, validate_exact_envelope, StoredMsgpackProfile};
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::symb::Symbol;
use crate::data::tuple::{
    bounded_key_label, try_decode_tuple_from_key, Tuple, TupleT, ENCODED_KEY_MIN_LEN,
};
use crate::data::value::{DataValue, ValidityTs};
use crate::fts::indexing::encode_fts_rows_for_tuple;
use crate::fts::FtsIndexManifest;
use crate::parse::expr::build_expr;
use crate::parse::sys::{FtsIndexConfig, HnswIndexConfig, MinHashLshConfig};
use crate::parse::{CozoScriptParser, Rule, SourceSpan};
use crate::query::compile::IndexPositionUse;
use crate::runtime::hnsw::{
    collect_hnsw_build, hnsw_tuple_digest, HnswBuildSnapshot, HnswIndexManifest,
};
use crate::runtime::minhash_lsh::{HashPermutations, LshParams, MinHashLshIndexManifest, Weights};
use crate::runtime::transact::SessionTx;
use crate::utils::TempCollector;
use crate::{NamedRows, PrimaryKeyScanBound, PrimaryKeyScanDirection, StoreTx};

#[derive(
    Copy,
    Clone,
    Eq,
    PartialEq,
    Debug,
    serde_derive::Serialize,
    serde_derive::Deserialize,
    PartialOrd,
    Ord,
)]
pub(crate) struct RelationId(pub(crate) u64);

const RELATION_ID_EXCLUSIVE_END: u64 = 2u64.pow(6 * 8);

/// Stable identity for persisted relation-id construction, prefix encoding,
/// fallible decoding, and the exclusive scan sentinel.
pub(crate) const STORED_RELATION_ID_POLICY_FINGERPRINT_V1: [u8; 32] = [
    83, 189, 41, 185, 99, 139, 152, 233, 179, 167, 168, 165, 44, 186, 51, 57, 199, 207, 174, 12,
    254, 104, 79, 231, 11, 181, 59, 60, 176, 32, 167, 142,
];

#[cfg(test)]
const STORED_RELATION_ID_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.stored-relation-id.policy-fingerprint-transcript.v1\0";
#[cfg(test)]
const STORED_RELATION_ID_POLICY_DESCRIPTION_V1: &str = concat!(
    "mnestic.stored-relation-id-policy.v1\n",
    "wire=exact 8-byte unsigned big-endian prefix\n",
    "persisted-domain=0..2^48; constructor and decoders reject values >=2^48\n",
    "scan-bound=next(max-persisted) may internally construct the exclusive 2^48 sentinel; sentinel is never decodable or allocatable\n",
    "allocation=atomic monotonic checked increment below the exclusive end\n",
);

#[derive(Debug, Diagnostic, Error)]
#[error("corrupt relation id: {reason}")]
#[diagnostic(code(eval::corrupt_relation_id))]
pub(crate) struct CorruptRelationId {
    reason: String,
}

#[derive(Debug, Diagnostic, Error)]
#[error(
    "relation id space exhausted at {last}; persisted relation ids must be below {exclusive_end}"
)]
#[diagnostic(code(eval::relation_id_exhausted))]
struct RelationIdExhausted {
    last: u64,
    exclusive_end: u64,
}

impl RelationId {
    pub(crate) fn new(u: u64) -> Self {
        if u >= RELATION_ID_EXCLUSIVE_END {
            panic!("StoredRelId overflow: {u}")
        } else {
            Self(u)
        }
    }
    pub(crate) fn next(&self) -> Self {
        let next = self
            .0
            .checked_add(1)
            .unwrap_or_else(|| panic!("StoredRelId scan bound overflow: {}", self.0));
        if next > RELATION_ID_EXCLUSIVE_END {
            panic!("StoredRelId scan bound overflow: {next}")
        }
        // The exclusive endpoint is a private range-scan sentinel. It must be
        // constructible here but remains rejected by `new` and both decoders.
        Self(next)
    }
    pub(crate) const SYSTEM: Self = Self(0);
    pub(crate) fn raw_encode(&self) -> [u8; 8] {
        self.0.to_be_bytes()
    }
    pub(crate) fn try_raw_decode(src: &[u8]) -> std::result::Result<Self, CorruptRelationId> {
        let bytes: [u8; 8] = src.try_into().map_err(|_| CorruptRelationId {
            reason: format!("expected exactly 8 bytes, got {}", src.len()),
        })?;
        Self::try_from_raw_bytes(bytes)
    }

    pub(crate) fn try_raw_decode_prefix(
        src: &[u8],
    ) -> std::result::Result<Self, CorruptRelationId> {
        let prefix = src.get(..8).ok_or_else(|| CorruptRelationId {
            reason: format!("expected an 8-byte prefix, got {} bytes", src.len()),
        })?;
        let bytes: [u8; 8] = prefix.try_into().map_err(|_| CorruptRelationId {
            reason: "relation-id prefix width changed during decode".to_owned(),
        })?;
        Self::try_from_raw_bytes(bytes)
    }

    fn try_from_raw_bytes(bytes: [u8; 8]) -> std::result::Result<Self, CorruptRelationId> {
        let u = u64::from_be_bytes(bytes);
        if u >= RELATION_ID_EXCLUSIVE_END {
            Err(CorruptRelationId {
                reason: format!("value {u} is outside 0..{RELATION_ID_EXCLUSIVE_END}"),
            })
        } else {
            Ok(Self(u))
        }
    }

    fn allocate_persisted(counter: &AtomicU64) -> Result<Self> {
        let previous = counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| {
                let next = last.checked_add(1)?;
                (next < RELATION_ID_EXCLUSIVE_END).then_some(next)
            })
            .map_err(|last| RelationIdExhausted {
                last,
                exclusive_end: RELATION_ID_EXCLUSIVE_END,
            })?;
        Ok(Self::new(previous + 1))
    }
}

#[derive(Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct RelationHandle {
    pub(crate) name: SmartString<LazyCompact>,
    pub(crate) id: RelationId,
    pub(crate) metadata: StoredRelationMetadata,
    pub(crate) put_triggers: Vec<String>,
    pub(crate) rm_triggers: Vec<String>,
    pub(crate) replace_triggers: Vec<String>,
    pub(crate) access_level: AccessLevel,
    pub(crate) is_temp: bool,
    pub(crate) indices: BTreeMap<SmartString<LazyCompact>, (RelationHandle, Vec<usize>)>,
    pub(crate) hnsw_indices:
        BTreeMap<SmartString<LazyCompact>, (RelationHandle, HnswIndexManifest)>,
    pub(crate) fts_indices: BTreeMap<SmartString<LazyCompact>, (RelationHandle, FtsIndexManifest)>,
    pub(crate) lsh_indices: BTreeMap<
        SmartString<LazyCompact>,
        (RelationHandle, RelationHandle, MinHashLshIndexManifest),
    >,
    pub(crate) description: SmartString<LazyCompact>,
    /// mnestic fork, bitemporality step 5: after `::history_gc rel cutoff`,
    /// as-of reads below the cutoff would silently return a post-hoc
    /// reconstruction, so they error instead. `None` = never garbage
    /// collected.
    ///
    /// MUST stay the LAST field. rmp_serde encodes structs as positional
    /// arrays on the pre-`with_struct_map` catalog-write paths, and
    /// `#[serde(default)]` only rescues a *missing trailing* element. A
    /// mid-struct position here bricks every legacy (13-field) catalog on
    /// upgrade — see the `legacy_catalog_without_tt_gc_floor` regression test.
    #[serde(default)]
    pub(crate) tt_gc_floor: Option<i64>,
}

impl RelationHandle {
    pub(crate) fn has_index(&self, index_name: &str) -> bool {
        self.indices.contains_key(index_name)
            || self.hnsw_indices.contains_key(index_name)
            || self.fts_indices.contains_key(index_name)
            || self.lsh_indices.contains_key(index_name)
    }
    pub(crate) fn has_no_index(&self) -> bool {
        self.indices.is_empty()
            && self.hnsw_indices.is_empty()
            && self.fts_indices.is_empty()
            && self.lsh_indices.is_empty()
    }
}

#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    PartialEq,
    serde_derive::Serialize,
    serde_derive::Deserialize,
    Default,
    Ord,
    PartialOrd,
)]
pub enum AccessLevel {
    Hidden,
    ReadOnly,
    Protected,
    #[default]
    Normal,
}

impl Display for AccessLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessLevel::Normal => f.write_str("normal"),
            AccessLevel::Protected => f.write_str("protected"),
            AccessLevel::ReadOnly => f.write_str("read_only"),
            AccessLevel::Hidden => f.write_str("hidden"),
        }
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("Arity mismatch for stored relation {name}: expect {expect_arity}, got {actual_arity}")]
#[diagnostic(code(eval::stored_rel_arity_mismatch))]
struct StoredRelArityMismatch {
    name: String,
    expect_arity: usize,
    actual_arity: usize,
    #[label]
    span: SourceSpan,
}

#[derive(Debug, Error, Diagnostic)]
#[error("Cannot encode key for stored relation {name}: {reason}")]
#[diagnostic(code(eval::stored_key_limit))]
struct StoredKeyLimit {
    name: String,
    reason: String,
    #[label]
    span: SourceSpan,
}

#[derive(Debug, Error, Diagnostic)]
#[error("Cannot encode a stored relation value: {reason}")]
#[diagnostic(code(eval::stored_value_limit))]
struct StoredValueLimit {
    reason: String,
    #[label]
    span: SourceSpan,
}

fn encode_stored_key_components(
    relation_id: RelationId,
    relation_name: &str,
    values: &[DataValue],
    span: SourceSpan,
) -> Result<Vec<u8>> {
    if relation_id.0 >= RELATION_ID_EXCLUSIVE_END {
        bail!(StoredKeyLimit {
            name: relation_name.to_owned(),
            reason: format!(
                "relation id {} is not persistable; stored ids must be below {}",
                relation_id.0, RELATION_ID_EXCLUSIVE_END
            ),
            span
        });
    }
    let payload_len = stored_key_values_encoded_len(values).map_err(|error| StoredKeyLimit {
        name: relation_name.to_owned(),
        reason: error.to_string(),
        span,
    })?;
    let encoded_len = ENCODED_KEY_MIN_LEN
        .checked_add(payload_len)
        .ok_or_else(|| StoredKeyLimit {
            name: relation_name.to_owned(),
            reason: "encoded stored-key length overflow".to_owned(),
            span,
        })?;
    if encoded_len > MAX_ENCODED_KEY_BYTES {
        bail!(StoredKeyLimit {
            name: relation_name.to_owned(),
            reason: format!("encoded key is {encoded_len} bytes; limit is {MAX_ENCODED_KEY_BYTES}"),
            span
        });
    }

    let mut ret = Vec::with_capacity(encoded_len);
    ret.extend(relation_id.0.to_be_bytes());
    for value in values {
        ret.encode_datavalue(value);
    }
    debug_assert_eq!(ret.len(), encoded_len);
    Ok(ret)
}

fn encode_catalog_name_for_store(
    name: &SmartString<LazyCompact>,
    span: SourceSpan,
) -> Result<Vec<u8>> {
    encode_stored_key_components(
        RelationId::SYSTEM,
        "system catalog",
        &[DataValue::Str(name.clone())],
        span,
    )
}

impl RelationHandle {
    pub(crate) fn raw_binding_map(&self) -> BTreeMap<Symbol, usize> {
        let mut ret = BTreeMap::new();
        for (i, col) in self.metadata.keys.iter().enumerate() {
            ret.insert(Symbol::new(col.name.clone(), Default::default()), i);
        }
        for (i, col) in self.metadata.non_keys.iter().enumerate() {
            ret.insert(
                Symbol::new(col.name.clone(), Default::default()),
                i + self.metadata.keys.len(),
            );
        }
        ret
    }
    pub(crate) fn has_triggers(&self) -> bool {
        !self.put_triggers.is_empty() || !self.rm_triggers.is_empty()
    }
    fn encode_key_prefix(&self, len: usize) -> Vec<u8> {
        let mut ret = Vec::with_capacity(4 + 4 * len + 10 * len);
        let prefix_bytes = self.id.0.to_be_bytes();
        ret.extend(prefix_bytes);
        ret
    }
    pub(crate) fn as_named_rows(&self, tx: &SessionTx<'_>) -> Result<NamedRows> {
        let rows: Vec<_> = self.scan_all(tx).try_collect()?;
        let mut headers = self
            .metadata
            .keys
            .iter()
            .map(|col| col.name.to_string())
            .collect_vec();
        headers.extend(
            self.metadata
                .non_keys
                .iter()
                .map(|col| col.name.to_string()),
        );
        Ok(NamedRows::new(headers, rows))
    }
    #[allow(dead_code)]
    pub(crate) fn amend_key_prefix(&self, data: &mut [u8]) {
        let prefix_bytes = self.id.0.to_be_bytes();
        data[0..8].copy_from_slice(&prefix_bytes);
    }
    pub(crate) fn choose_index(
        &self,
        arg_uses: &[IndexPositionUse],
        validity_query: bool,
    ) -> Option<(RelationHandle, Vec<usize>, bool)> {
        if self.indices.is_empty() {
            return None;
        }
        if *arg_uses.first().unwrap() == IndexPositionUse::Join {
            return None;
        }
        let mut max_prefix_len = 0;
        let required_positions = arg_uses
            .iter()
            .enumerate()
            .filter_map(|(i, pos_use)| {
                if *pos_use != IndexPositionUse::Ignored {
                    Some(i)
                } else {
                    None
                }
            })
            .collect_vec();
        let mut chosen = None;
        for (manifest, mapper) in self.indices.values() {
            if validity_query && *mapper.last().unwrap() != self.metadata.keys.len() - 1 {
                continue;
            }

            let mut cur_prefix_len = 0;
            for i in mapper {
                if arg_uses[*i] == IndexPositionUse::Join {
                    cur_prefix_len += 1;
                } else {
                    break;
                }
            }
            if cur_prefix_len > max_prefix_len {
                max_prefix_len = cur_prefix_len;
                let mut need_join = false;
                for need_pos in required_positions.iter() {
                    if !mapper.contains(need_pos) {
                        need_join = true;
                        break;
                    }
                }
                chosen = Some((manifest.clone(), mapper.clone(), need_join))
            }
        }
        chosen
    }
    pub(crate) fn encode_key_for_store(
        &self,
        tuple: &[DataValue],
        span: SourceSpan,
    ) -> Result<Vec<u8>> {
        let len = self.metadata.keys.len();
        ensure!(
            tuple.len() >= len,
            StoredRelArityMismatch {
                name: self.name.to_string(),
                expect_arity: self.arity(),
                actual_arity: tuple.len(),
                span
            }
        );
        self.encode_key_components_for_store(&tuple[..len], span)
    }
    pub(crate) fn try_encode_partial_key_for_store(
        &self,
        tuple: &[DataValue],
        span: SourceSpan,
    ) -> Result<Vec<u8>> {
        self.encode_key_components_for_store(tuple, span)
    }
    pub(crate) fn encode_partial_key_for_store(&self, tuple: &[DataValue]) -> Vec<u8> {
        let mut ret = self.encode_key_prefix(tuple.len());
        for val in tuple {
            ret.encode_datavalue(val);
        }
        ret
    }
    fn encode_key_components_for_store(
        &self,
        values: &[DataValue],
        span: SourceSpan,
    ) -> Result<Vec<u8>> {
        encode_stored_key_components(self.id, &self.name, values, span)
    }
    pub(crate) fn encode_val_for_store(
        &self,
        tuple: &[DataValue],
        span: SourceSpan,
    ) -> Result<Vec<u8>> {
        let start = self.metadata.keys.len();
        let values = tuple.get(start..).ok_or_else(|| StoredValueLimit {
            reason: format!(
                "tuple has {} values but the stored key consumes {start}",
                tuple.len()
            ),
            span,
        })?;
        self.encode_value_components_for_store(values, span)
    }
    pub(crate) fn encode_val_only_for_store(
        &self,
        tuple: &[DataValue],
        span: SourceSpan,
    ) -> Result<Vec<u8>> {
        self.encode_value_components_for_store(tuple, span)
    }
    fn encode_value_components_for_store(
        &self,
        values: &[DataValue],
        span: SourceSpan,
    ) -> Result<Vec<u8>> {
        if self.id.0 >= RELATION_ID_EXCLUSIVE_END {
            bail!(StoredValueLimit {
                reason: format!(
                    "relation id {} is not persistable; stored ids must be below {}",
                    self.id.0, RELATION_ID_EXCLUSIVE_END
                ),
                span
            });
        }
        let mut ret = self.encode_key_prefix(values.len());
        values
            .serialize(&mut Serializer::new(&mut ret))
            .map_err(|error| StoredValueLimit {
                reason: bounded_decode_reason(error),
                span,
            })?;
        validate_exact_envelope(&ret[ENCODED_KEY_MIN_LEN..], StoredMsgpackProfile::Row).map_err(
            |error| StoredValueLimit {
                reason: bounded_decode_reason(error),
                span,
            },
        )?;
        Ok(ret)
    }
    pub(crate) fn ensure_compatible(
        &self,
        inp: &InputRelationHandle,
        op: crate::data::program::RelationOp,
    ) -> Result<()> {
        use crate::data::program::RelationOp;
        let is_remove_or_update =
            matches!(op, RelationOp::Rm | RelationOp::Delete | RelationOp::Update);
        let InputRelationHandle { metadata, .. } = inp;
        // check that every given key is found and compatible
        for col in metadata.keys.iter().chain(self.metadata.non_keys.iter()) {
            self.metadata.compatible_with_col(col)?
        }
        // check that every key is provided or has default
        let n = self.metadata.keys.len();
        for (i, col) in self.metadata.keys.iter().enumerate() {
            // mnestic fork, bitemporality 4c: on a bitemporal relation the
            // ops that target the CURRENT belief (:update, :ensure,
            // :ensure_not) must not bind the vt column — the engine resolves
            // it — so it is not required of their input.
            if self.has_txtime()
                && !self.is_tt_only()
                && i == n - 2
                && matches!(
                    op,
                    RelationOp::Update | RelationOp::Ensure | RelationOp::EnsureNot
                )
            {
                continue;
            }
            metadata.satisfied_by_required_col(col)?;
        }
        if !is_remove_or_update {
            for col in &self.metadata.non_keys {
                metadata.satisfied_by_required_col(col)?;
            }
        }
        Ok(())
    }
}

/// Enforce the temporal-axis rule at `:create` time (mnestic fork,
/// bitemporality step 3; `docs/specs/bitemporality.md` §4/§13.1): temporal
/// axes are the trailing key columns in the fixed order vt-then-tt, at most
/// one of each. `TxTime` is new (no shipped uses), so malformed declarations
/// fail HERE, loudly, with the corrected declaration in the message —
/// deliberately stricter than `Validity`'s shipped query-time-only check.
fn validate_temporal_axes(input_meta: &InputRelationHandle) -> Result<()> {
    use crate::data::relation::{ColType, ColumnDef};

    let keys = &input_meta.metadata.keys;
    let non_keys = &input_meta.metadata.non_keys;
    let is_tt = |c: &&ColumnDef| matches!(c.typing.coltype, ColType::TxTime);
    let is_vt = |c: &&ColumnDef| matches!(c.typing.coltype, ColType::Validity);

    let tt_in_keys = keys.iter().filter(is_tt).count();
    let tt_in_vals = non_keys.iter().filter(is_tt).count();
    if tt_in_keys == 0 && tt_in_vals == 0 {
        return Ok(());
    }

    // The copy-pasteable corrected declaration: non-temporal keys in declared
    // order, then the (single) Validity, then the (single) TxTime, then the
    // non-temporal value columns. Defaults are omitted from the rendering.
    let corrected = {
        let plain_keys = keys
            .iter()
            .filter(|c| !is_tt(c) && !is_vt(c))
            .map(|c| format!("{}: {}", c.name, c.typing))
            .collect::<Vec<_>>();
        let vt = keys
            .iter()
            .chain(non_keys.iter())
            .find(|c| is_vt(c))
            .map(|c| format!("{}: {}", c.name, c.typing));
        let tt = keys
            .iter()
            .chain(non_keys.iter())
            .find(|c| is_tt(c))
            .map(|c| format!("{}: TxTime", c.name));
        let mut key_parts = plain_keys;
        key_parts.extend(vt);
        key_parts.extend(tt);
        let val_parts = non_keys
            .iter()
            .filter(|c| !is_tt(c))
            .map(|c| format!("{}: {}", c.name, c.typing))
            .collect::<Vec<_>>();
        if val_parts.is_empty() {
            format!(
                ":create {} {{{}}}",
                input_meta.name.name,
                key_parts.join(", ")
            )
        } else {
            format!(
                ":create {} {{{} => {}}}",
                input_meta.name.name,
                key_parts.join(", "),
                val_parts.join(", ")
            )
        }
    };

    #[derive(Debug, Error, Diagnostic)]
    #[error("invalid temporal-axis declaration: {reason}")]
    #[diagnostic(
        code(eval::invalid_temporal_axes),
        help("temporal axes must be the trailing key columns, in the order vt (Validity) then tt (TxTime), at most one of each; corrected declaration: `{corrected}`")
    )]
    struct InvalidTemporalAxes {
        reason: String,
        corrected: String,
        #[label]
        span: SourceSpan,
    }
    let err = |reason: &str| InvalidTemporalAxes {
        reason: reason.to_string(),
        corrected: corrected.clone(),
        span: input_meta.span,
    };

    if input_meta.name.is_temp_store_name() {
        bail!(err("TxTime is not supported on transaction-temp (`_`-prefixed) relations — temp stores have no commit clock"));
    }
    if tt_in_vals > 0 {
        bail!(err("TxTime must be a key column, not a value column"));
    }
    if tt_in_keys > 1 {
        bail!(err("at most one TxTime column is allowed"));
    }
    if keys
        .iter()
        .find(is_tt)
        .expect("tt_in_keys == 1")
        .typing
        .nullable
    {
        bail!(err(
            "TxTime cannot be nullable (it is engine-assigned at every commit)"
        ));
    }
    let vt_in_keys = keys.iter().filter(is_vt).count();
    if vt_in_keys > 1 {
        bail!(err(
            "at most one Validity column is allowed when TxTime is declared"
        ));
    }
    if keys.iter().any(|c| is_vt(&&c.clone()) && c.typing.nullable) {
        bail!(err(
            "the Validity axis cannot be nullable when TxTime is declared (the two-level resolution has no semantics for a null vt)"
        ));
    }
    let n = keys.len();
    if !matches!(keys[n - 1].typing.coltype, ColType::TxTime) {
        bail!(err("TxTime must be the last key column"));
    }
    if vt_in_keys == 1 && (n < 2 || !matches!(keys[n - 2].typing.coltype, ColType::Validity)) {
        bail!(err(
            "Validity must immediately precede TxTime (the vt-then-tt trailing pair)"
        ));
    }
    Ok(())
}

/// How a read resolves against a relation's temporal axes (mnestic fork).
#[derive(Debug, Clone, Copy)]
pub(crate) enum TemporalRead {
    /// no temporal machinery: plain scan
    Plain,
    /// single trailing-axis skip-scan at the point (vt relations with `@`,
    /// and tt-only relations — where the point is on the tt axis)
    AsOf(ValidityTs),
    /// two-level (vt, tt) resolution on a bitemporal relation
    Bitemporal {
        vt: Option<ValidityTs>,
        tt: ValidityTs,
    },
}

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct InputRelationHandle {
    pub(crate) name: Symbol,
    pub(crate) metadata: StoredRelationMetadata,
    pub(crate) key_bindings: Vec<Symbol>,
    pub(crate) dep_bindings: Vec<Symbol>,
    pub(crate) span: SourceSpan,
}

impl Debug for RelationHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Relation<{}>", self.name)
    }
}

#[derive(thiserror::Error, miette::Diagnostic, Debug)]
#[error("Cannot deserialize relation")]
#[diagnostic(code(deser::relation))]
#[diagnostic(help(
    "This could indicate a bug, or you are using an incompatible DB version. \
Consider file a bug report."
))]
pub(crate) struct RelationDeserError;

impl RelationHandle {
    /// Whether this relation is tt-stamped (its last key column is `TxTime`;
    /// mnestic fork, bitemporality). Guaranteed by `validate_temporal_axes`
    /// to be the only possible TxTime position.
    pub(crate) fn has_txtime(&self) -> bool {
        matches!(
            self.metadata.keys.last().map(|c| &c.typing.coltype),
            Some(crate::data::relation::ColType::TxTime)
        )
    }

    /// tt-only (system-versioned): tt-stamped with no vt axis.
    pub(crate) fn is_tt_only(&self) -> bool {
        self.has_txtime() && {
            let n = self.metadata.keys.len();
            n < 2
                || !matches!(
                    self.metadata.keys[n - 2].typing.coltype,
                    crate::data::relation::ColType::Validity
                )
        }
    }

    /// After `::history_gc`, an as-of read below the persisted floor would
    /// silently return a post-hoc reconstruction as if it were the historical
    /// belief — error instead (mnestic fork, bitemporality step 5).
    fn check_tt_gc_floor(&self, tt: Option<ValidityTs>, span: SourceSpan) -> Result<()> {
        if let (Some(t), Some(floor)) = (tt, self.tt_gc_floor) {
            if t.0 .0 < floor {
                #[derive(Debug, Error, Diagnostic)]
                #[error("as-of read below the ::history_gc floor of relation {0} ({1} < {2})")]
                #[diagnostic(
                    code(eval::txtime_below_gc_floor),
                    help("records below the floor were garbage-collected; the belief at that time is no longer reconstructible")
                )]
                struct BelowGcFloor(String, i64, i64, #[label] SourceSpan);
                bail!(BelowGcFloor(self.name.to_string(), t.0 .0, floor, span));
            }
        }
        Ok(())
    }

    /// Resolve a read's temporal selectors against this relation's axes
    /// (mnestic fork, bitemporality steps 4a/4b; spec §4 semantics table).
    /// On tt-only relations the tt axis DEFAULTS to end-of-tt-time — the
    /// default read is the current state, so adding a `tt: TxTime` column
    /// changes no existing query's results (the migration invariant); only an
    /// explicit `@ (tt: …)` reaches history. On bitemporal relations the vt
    /// axis keeps its shipped semantics (no selector = every vt record,
    /// resolved to the belief at T) and the tt axis defaults to current
    /// belief — the same invariant, two axes.
    pub(crate) fn resolve_temporal_read(
        &self,
        vt: Option<ValidityTs>,
        tt: Option<ValidityTs>,
        span: SourceSpan,
    ) -> Result<TemporalRead> {
        use crate::data::functions::MAX_VALIDITY_TS;
        if self.has_txtime() {
            if self.is_tt_only() {
                if vt.is_some() {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("relation {0} is system-versioned: it has no valid-time axis")]
                    #[diagnostic(
                        code(eval::txtime_no_vt_axis),
                        help("select transaction time with `@ (tt: …)`; bare `@ E` always means valid time")
                    )]
                    struct NoVtAxis(String, #[label] SourceSpan);
                    bail!(NoVtAxis(self.name.to_string(), span));
                }
                self.check_tt_gc_floor(tt, span)?;
                Ok(TemporalRead::AsOf(tt.unwrap_or(MAX_VALIDITY_TS)))
            } else {
                // Bitemporal (step 4b): the two-level resolution. No vt
                // selector = every vt record (resolve-groups); a vt selector
                // = the single belief per key (resolve-key). tt defaults to
                // current belief.
                self.check_tt_gc_floor(tt, span)?;
                Ok(TemporalRead::Bitemporal {
                    vt,
                    tt: tt.unwrap_or(MAX_VALIDITY_TS),
                })
            }
        } else {
            if tt.is_some() {
                #[derive(Debug, Error, Diagnostic)]
                #[error("relation {0} has no transaction-time axis")]
                #[diagnostic(
                    code(eval::txtime_no_tt_axis),
                    help("declare a trailing `tt: TxTime` key column to make the relation transaction-time-stamped")
                )]
                struct NoTtAxis(String, #[label] SourceSpan);
                bail!(NoTtAxis(self.name.to_string(), span));
            }
            match vt {
                None => Ok(TemporalRead::Plain),
                Some(v) => {
                    if self.metadata.keys.last().map(|c| &c.typing.coltype)
                        != Some(&crate::data::relation::ColType::Validity)
                        || self.metadata.keys.last().unwrap().typing.nullable
                    {
                        bail!(crate::query::ra::InvalidTimeTravelScanning(
                            self.name.to_string(),
                            span
                        ));
                    }
                    Ok(TemporalRead::AsOf(v))
                }
            }
        }
    }

    pub(crate) fn arity(&self) -> usize {
        self.metadata.non_keys.len() + self.metadata.keys.len()
    }
    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        Ok(
            decode_exact(data, StoredMsgpackProfile::RelationCatalog).map_err(|error| {
                error!(
                    "Cannot deserialize {}-byte relation metadata: {}",
                    data.len(),
                    error
                );
                RelationDeserError
            })?,
        )
    }
    pub(crate) fn scan_all<'a>(
        &self,
        tx: &'a SessionTx<'_>,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let lower = Tuple::default().encode_as_key(self.id);
        let upper = Tuple::default().encode_as_key(self.id.next());
        if self.is_temp {
            tx.temp_store_tx.range_scan_tuple(&lower, &upper)
        } else {
            tx.store_tx.range_scan_tuple(&lower, &upper)
        }
    }

    pub(crate) fn scan_primary_key_range<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        prefix: &[DataValue],
        lower: &PrimaryKeyScanBound,
        upper: &PrimaryKeyScanBound,
        direction: PrimaryKeyScanDirection,
        limit: usize,
    ) -> Result<Box<dyn Iterator<Item = Result<Tuple>> + 'a>> {
        let mut lower_tuple = prefix.to_vec();
        match lower {
            PrimaryKeyScanBound::Unbounded => {}
            PrimaryKeyScanBound::Included(values) => lower_tuple.extend_from_slice(values),
            PrimaryKeyScanBound::Excluded(values) => {
                lower_tuple.extend_from_slice(values);
                lower_tuple.push(DataValue::Bot);
            }
        }
        let mut upper_tuple = prefix.to_vec();
        match upper {
            PrimaryKeyScanBound::Unbounded => upper_tuple.push(DataValue::Bot),
            PrimaryKeyScanBound::Included(values) => {
                upper_tuple.extend_from_slice(values);
                upper_tuple.push(DataValue::Bot);
            }
            PrimaryKeyScanBound::Excluded(values) => upper_tuple.extend_from_slice(values),
        }
        let lower_encoded = lower_tuple.encode_as_key(self.id);
        let upper_encoded = upper_tuple.encode_as_key(self.id);
        if lower_encoded >= upper_encoded {
            return Ok(Box::new(std::iter::empty()));
        }
        let iter = match (self.is_temp, direction) {
            (true, PrimaryKeyScanDirection::Ascending) => tx
                .temp_store_tx
                .range_scan_tuple_limited(&lower_encoded, &upper_encoded, limit),
            (false, PrimaryKeyScanDirection::Ascending) => {
                tx.store_tx
                    .range_scan_tuple_limited(&lower_encoded, &upper_encoded, limit)
            }
            (true, PrimaryKeyScanDirection::Descending) => tx
                .temp_store_tx
                .range_scan_tuple_rev_limited(&lower_encoded, &upper_encoded, limit),
            (false, PrimaryKeyScanDirection::Descending) => tx
                .store_tx
                .range_scan_tuple_rev_limited(&lower_encoded, &upper_encoded, limit),
        };
        Ok(iter)
    }

    pub(crate) fn skip_scan_all<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        valid_at: ValidityTs,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let lower = Tuple::default().encode_as_key(self.id);
        let upper = Tuple::default().encode_as_key(self.id.next());
        if self.is_temp {
            tx.temp_store_tx
                .range_skip_scan_tuple(&lower, &upper, valid_at)
        } else {
            tx.store_tx.range_skip_scan_tuple(&lower, &upper, valid_at)
        }
    }

    pub(crate) fn get(&self, tx: &SessionTx<'_>, key: &[DataValue]) -> Result<Option<Tuple>> {
        let key_data = key.encode_as_key(self.id);
        if self.is_temp {
            tx.temp_store_tx
                .get(&key_data, false)?
                .map(|val_data| try_decode_tuple_from_kv(&key_data, &val_data, Some(self.arity())))
                .transpose()
        } else {
            tx.store_tx
                .get(&key_data, false)?
                .map(|val_data| try_decode_tuple_from_kv(&key_data, &val_data, Some(self.arity())))
                .transpose()
        }
    }

    /// Batched point lookups (mnestic fork): encode all keys and serve them
    /// through one `StoreTx::multi_get` — a true RocksDB `MultiGet` on the
    /// snapshot read path (shared filter probes, batched block reads).
    /// Returns one entry per key, in order.
    pub(crate) fn get_batch(
        &self,
        tx: &SessionTx<'_>,
        keys: &[&[DataValue]],
    ) -> Result<Vec<Option<Tuple>>> {
        let encoded: Vec<Vec<u8>> = keys.iter().map(|k| k.encode_as_key(self.id)).collect();
        let raw = if self.is_temp {
            tx.temp_store_tx.multi_get(&encoded, false)?
        } else {
            tx.store_tx.multi_get(&encoded, false)?
        };
        raw.into_iter()
            .zip(encoded.iter())
            .map(|(v, k)| {
                v.map(|val| try_decode_tuple_from_kv(k, &val, Some(self.arity())))
                    .transpose()
            })
            .collect()
    }

    pub(crate) fn get_val_only(
        &self,
        tx: &SessionTx<'_>,
        key: &[DataValue],
    ) -> Result<Option<Tuple>> {
        let key_data = key.encode_as_key(self.id);
        if self.is_temp {
            tx.temp_store_tx
                .get(&key_data, false)?
                .map(|val_data| try_decode_val_only(&key_data, &val_data))
                .transpose()
        } else {
            tx.store_tx
                .get(&key_data, false)?
                .map(|val_data| try_decode_val_only(&key_data, &val_data))
                .transpose()
        }
    }

    pub(crate) fn exists(&self, tx: &SessionTx<'_>, key: &[DataValue]) -> Result<bool> {
        let key_data = key.encode_as_key(self.id);
        if self.is_temp {
            tx.temp_store_tx.exists(&key_data, false)
        } else {
            tx.store_tx.exists(&key_data, false)
        }
    }

    pub(crate) fn scan_prefix<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        prefix: &Tuple,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let mut lower = prefix.clone();
        lower.truncate(self.metadata.keys.len());
        let mut upper = lower.clone();
        upper.push(DataValue::Bot);
        let prefix_encoded = lower.encode_as_key(self.id);
        let upper_encoded = upper.encode_as_key(self.id);
        if self.is_temp {
            tx.temp_store_tx
                .range_scan_tuple(&prefix_encoded, &upper_encoded)
        } else {
            tx.store_tx
                .range_scan_tuple(&prefix_encoded, &upper_encoded)
        }
    }

    /// Two-level bitemporal scans (mnestic fork, step 4b): whole relation.
    pub(crate) fn bitemporal_scan_all<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        vt_at: Option<ValidityTs>,
        tt_at: ValidityTs,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let lower = Tuple::default().encode_as_key(self.id);
        let upper = Tuple::default().encode_as_key(self.id.next());
        tx.store_tx
            .range_bitemporal_scan_tuple(&lower, &upper, vt_at, tt_at)
    }

    /// Two-level bitemporal scan narrowed to a key prefix (step 4b).
    pub(crate) fn bitemporal_scan_prefix<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        prefix: &Tuple,
        vt_at: Option<ValidityTs>,
        tt_at: ValidityTs,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let mut lower = prefix.clone();
        lower.truncate(self.metadata.keys.len());
        let mut upper = lower.clone();
        upper.push(DataValue::Bot);
        let prefix_encoded = lower.encode_as_key(self.id);
        let upper_encoded = upper.encode_as_key(self.id);
        tx.store_tx
            .range_bitemporal_scan_tuple(&prefix_encoded, &upper_encoded, vt_at, tt_at)
    }

    pub(crate) fn skip_scan_prefix<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        prefix: &Tuple,
        valid_at: ValidityTs,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let mut lower = prefix.clone();
        lower.truncate(self.metadata.keys.len());
        let mut upper = lower.clone();
        upper.push(DataValue::Bot);
        let prefix_encoded = lower.encode_as_key(self.id);
        let upper_encoded = upper.encode_as_key(self.id);
        if self.is_temp {
            tx.temp_store_tx
                .range_skip_scan_tuple(&prefix_encoded, &upper_encoded, valid_at)
        } else {
            tx.store_tx
                .range_skip_scan_tuple(&prefix_encoded, &upper_encoded, valid_at)
        }
    }

    pub(crate) fn scan_bounded_prefix<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        prefix: &[DataValue],
        lower: &[DataValue],
        upper: &[DataValue],
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let mut lower_t = prefix.to_vec();
        lower_t.extend_from_slice(lower);
        let mut upper_t = prefix.to_vec();
        upper_t.extend_from_slice(upper);
        upper_t.push(DataValue::Bot);
        let lower_encoded = lower_t.encode_as_key(self.id);
        let upper_encoded = upper_t.encode_as_key(self.id);
        if self.is_temp {
            tx.temp_store_tx
                .range_scan_tuple(&lower_encoded, &upper_encoded)
        } else {
            tx.store_tx.range_scan_tuple(&lower_encoded, &upper_encoded)
        }
    }
    pub(crate) fn skip_scan_bounded_prefix<'a>(
        &self,
        tx: &'a SessionTx<'_>,
        prefix: &Tuple,
        lower: &[DataValue],
        upper: &[DataValue],
        valid_at: ValidityTs,
    ) -> impl Iterator<Item = Result<Tuple>> + 'a {
        let mut lower_t = prefix.clone();
        lower_t.extend_from_slice(lower);
        let mut upper_t = prefix.clone();
        upper_t.extend_from_slice(upper);
        upper_t.push(DataValue::Bot);
        let lower_encoded = lower_t.encode_as_key(self.id);
        let upper_encoded = upper_t.encode_as_key(self.id);
        if self.is_temp {
            tx.temp_store_tx
                .range_skip_scan_tuple(&lower_encoded, &upper_encoded, valid_at)
        } else {
            tx.store_tx
                .range_skip_scan_tuple(&lower_encoded, &upper_encoded, valid_at)
        }
    }
}

const DEFAULT_SIZE_HINT: usize = 16;

#[derive(Debug, Error, Diagnostic)]
#[error("corrupt value blob for key {key}: {reason}")]
#[diagnostic(
    code(eval::corrupt_value_blob),
    help("run `::repair_corrupt <relation>` to remove unreadable rows, then `::reindex <relation>` for indexed relations; if startup logged a relation-id collision, do NOT repair those relations — restore the original backup into a fresh store")
)]
struct CorruptValueBlob {
    key: String,
    reason: String,
}

fn decode_val_blob(val: &[u8], key: &[u8]) -> Result<Vec<DataValue>> {
    let key_label = bounded_key_label(key);
    let key_relation =
        RelationId::try_raw_decode_prefix(key).map_err(|error| CorruptValueBlob {
            key: key_label.clone(),
            reason: bounded_decode_reason(error),
        })?;
    let value_prefix = val
        .get(..ENCODED_KEY_MIN_LEN)
        .ok_or_else(|| CorruptValueBlob {
            key: key_label.clone(),
            reason: format!(
                "value is {} bytes; expected an {ENCODED_KEY_MIN_LEN}-byte relation prefix",
                val.len()
            ),
        })?;
    let value_relation =
        RelationId::try_raw_decode(value_prefix).map_err(|error| CorruptValueBlob {
            key: key_label.clone(),
            reason: bounded_decode_reason(error),
        })?;
    if value_relation != key_relation {
        return Err(CorruptValueBlob {
            key: key_label,
            reason: format!(
                "value relation id {} does not match key relation id {}",
                value_relation.0, key_relation.0
            ),
        }
        .into());
    }
    let encoded = val
        .get(ENCODED_KEY_MIN_LEN..)
        .ok_or_else(|| CorruptValueBlob {
            key: bounded_key_label(key),
            reason: format!(
                "value is {} bytes; expected at least {ENCODED_KEY_MIN_LEN}",
                val.len()
            ),
        })?;
    decode_exact(encoded, StoredMsgpackProfile::Row).map_err(|error| {
        CorruptValueBlob {
            key: bounded_key_label(key),
            reason: bounded_decode_reason(error),
        }
        .into()
    })
}

fn bounded_decode_reason(reason: impl Display) -> String {
    const LIMIT: usize = 256;
    let mut reason = reason.to_string();
    if reason.len() > LIMIT {
        let mut end = LIMIT - '…'.len_utf8();
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
        reason.push('…');
    }
    reason
}

/// Fallible decoder for storage implementations and engine read paths.
pub fn try_decode_tuple_from_kv(key: &[u8], val: &[u8], size_hint: Option<usize>) -> Result<Tuple> {
    let mut tup = try_decode_tuple_from_key(key, size_hint.unwrap_or(DEFAULT_SIZE_HINT))?;
    if !val.is_empty() {
        let vals = decode_val_blob(val, key)?;
        tup.extend(vals);
    }
    Ok(tup)
}

pub(crate) fn try_extend_tuple_from_v(
    key: &mut Tuple,
    encoded_key: &[u8],
    val: &[u8],
) -> Result<()> {
    if !val.is_empty() {
        let vals = decode_val_blob(val, encoded_key)?;
        key.extend(vals);
    }
    Ok(())
}

pub(crate) fn try_decode_val_only(key: &[u8], val: &[u8]) -> Result<Tuple> {
    RelationId::try_raw_decode_prefix(key).map_err(|error| CorruptValueBlob {
        key: bounded_key_label(key),
        reason: bounded_decode_reason(error),
    })?;
    if val.is_empty() {
        Ok(Vec::new())
    } else {
        decode_val_blob(val, key)
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("index {0} for relation {1} already exists")]
#[diagnostic(code(tx::index_already_exists))]
pub(crate) struct IndexAlreadyExists(String, String);

#[derive(Debug, Diagnostic, Error)]
#[error("Cannot create relation {0} as one with the same name already exists")]
#[diagnostic(code(eval::rel_name_conflict))]
struct RelNameConflictError(String);

impl<'a> SessionTx<'a> {
    pub(crate) fn relation_exists(&self, name: &str) -> Result<bool> {
        let key = DataValue::from(name);
        let encoded = vec![key].encode_as_key(RelationId::SYSTEM);
        if name.starts_with('_') {
            self.temp_store_tx.exists(&encoded, false)
        } else {
            self.store_tx.exists(&encoded, false)
        }
    }
    /// Bail if `rel` is tt-stamped: triggers and secondary/search indexes are
    /// not yet supported on TxTime relations (mnestic fork, bitemporality
    /// step 3 — buffered commit-time stamping is incompatible with
    /// statement-time trigger/index maintenance; B-tree index support is
    /// step 5, `docs/specs/bitemporality.md` §8).
    fn reject_txtime_relation(rel: &RelationHandle, what: &str) -> Result<()> {
        if rel.has_txtime() {
            #[derive(Debug, Error, Diagnostic)]
            #[error("{0} is not supported on TxTime (transaction-time) relations: {1} (statement-time index/trigger maintenance is incompatible with buffered commit-time stamping; revisit on real pull)")]
            #[diagnostic(code(eval::txtime_unsupported_op))]
            struct TxTimeUnsupported(String, String);
            bail!(TxTimeUnsupported(what.to_string(), rel.name.to_string()));
        }
        Ok(())
    }

    pub(crate) fn set_relation_triggers(
        &mut self,
        name: &Symbol,
        puts: &[String],
        rms: &[String],
        replaces: &[String],
    ) -> Result<()> {
        if name.name.starts_with('_') {
            bail!("Cannot set triggers for temp store")
        }
        let mut original = self.get_relation(name, true)?;
        Self::reject_txtime_relation(&original, "::set_triggers")?;
        if original.access_level < AccessLevel::Protected {
            bail!(InsufficientAccessLevel(
                original.name.to_string(),
                "set triggers".to_string(),
                original.access_level
            ))
        }
        original.put_triggers = puts.to_vec();
        original.rm_triggers = rms.to_vec();
        original.replace_triggers = replaces.to_vec();

        let name_key =
            vec![DataValue::Str(original.name.clone())].encode_as_key(RelationId::SYSTEM);

        let mut meta_val = vec![];
        original
            .serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&name_key, &meta_val)?;

        Ok(())
    }
    pub(crate) fn create_relation(
        &mut self,
        input_meta: InputRelationHandle,
    ) -> Result<RelationHandle> {
        validate_temporal_axes(&input_meta)?;
        let encoded = encode_catalog_name_for_store(&input_meta.name.name, input_meta.span)?;

        let is_temp = input_meta.name.is_temp_store_name();

        if is_temp {
            if self.store_tx.exists(&encoded, true)? {
                bail!(RelNameConflictError(input_meta.name.to_string()))
            };
        } else if self.temp_store_tx.exists(&encoded, true)? {
            bail!(RelNameConflictError(input_meta.name.to_string()))
        }

        let metadata = input_meta.metadata.clone();
        let relation_id = if is_temp {
            let last_id = self.temp_store_id.fetch_add(1, Ordering::Relaxed) as u64;
            RelationId::new(last_id + 1)
        } else {
            RelationId::allocate_persisted(&self.relation_store_id)?
        };
        let meta = RelationHandle {
            name: input_meta.name.name,
            id: relation_id,
            metadata,
            put_triggers: vec![],
            rm_triggers: vec![],
            replace_triggers: vec![],
            access_level: AccessLevel::Normal,
            is_temp,
            tt_gc_floor: None,
            indices: Default::default(),
            hnsw_indices: Default::default(),
            fts_indices: Default::default(),
            lsh_indices: Default::default(),
            description: Default::default(),
        };

        let name_key = encoded.clone();
        let mut meta_val = vec![];
        meta.serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        let tuple = vec![DataValue::Null];
        let t_encoded = tuple.encode_as_key(RelationId::SYSTEM);

        if is_temp {
            self.temp_store_tx.put(&encoded, &meta.id.raw_encode())?;
            self.temp_store_tx.put(&name_key, &meta_val)?;
            self.temp_store_tx.put(&t_encoded, &meta.id.raw_encode())?;
        } else {
            self.store_tx.put(&encoded, &meta.id.raw_encode())?;
            self.store_tx.put(&name_key, &meta_val)?;
            self.store_tx.put(&t_encoded, &meta.id.raw_encode())?;
        }

        Ok(meta)
    }
    pub(crate) fn get_relation(&self, name: &str, lock: bool) -> Result<RelationHandle> {
        #[derive(Error, Diagnostic, Debug)]
        #[error("Cannot find requested stored relation '{0}'")]
        #[diagnostic(code(query::relation_not_found))]
        struct StoredRelationNotFoundError(String);

        let key = DataValue::from(name);
        let encoded = vec![key].encode_as_key(RelationId::SYSTEM);

        let found = if name.starts_with('_') {
            self.temp_store_tx
                .get(&encoded, lock)?
                .ok_or_else(|| StoredRelationNotFoundError(name.to_string()))?
        } else {
            self.store_tx
                .get(&encoded, lock)?
                .ok_or_else(|| StoredRelationNotFoundError(name.to_string()))?
        };
        let metadata = RelationHandle::decode(&found)?;
        Ok(metadata)
    }
    pub(crate) fn describe_relation(&mut self, name: &str, description: &str) -> Result<()> {
        let mut meta = self.get_relation(name, true)?;

        meta.description = SmartString::from(description);
        let name_key = vec![DataValue::Str(meta.name.clone())].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        meta.serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        if meta.is_temp {
            self.temp_store_tx.put(&name_key, &meta_val)?;
        } else {
            self.store_tx.put(&name_key, &meta_val)?;
        }

        Ok(())
    }
    pub(crate) fn destroy_relation(&mut self, name: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        if self
            .pending_tt_writes
            .iter()
            .any(|w| w.handle.name.as_str() == name as &str)
        {
            bail!(
                "relation {} has pending transaction-time writes in this transaction; \
                 commit them in their own transaction before removing the relation",
                name
            );
        }
        let is_temp = name.starts_with('_');
        let mut to_clean = vec![];

        // if name.starts_with('_') {
        //     bail!("Cannot destroy temp relation");
        // }
        let store = self.get_relation(name, true)?;
        if !store.has_no_index() {
            bail!(
                "Cannot remove stored relation `{}` with indices attached.",
                name
            );
        }
        if store.access_level < AccessLevel::Normal {
            bail!(InsufficientAccessLevel(
                store.name.to_string(),
                "relation removal".to_string(),
                store.access_level
            ))
        }

        // Graph-projection dirty-set hook (spec §3.4 rows 5 and 9). This is the
        // single funnel for relation destruction: `::remove`, `::index drop`,
        // and `:replace`'s destroy-then-recreate all land here. Destruction is
        // just a dirtying like any other: the id's freshness state is KEPT as
        // a permanent tombstone, because a transaction whose snapshot predates
        // the destroy still resolves this id, and only the tombstone's bumped
        // token denies it a cache exchange (Phase 3/4 review, 2026-07-10).
        self.mark_dirty(&store);

        for k in store.indices.keys() {
            let more_to_clean = self.destroy_relation(&format!("{name}:{k}"))?;
            to_clean.extend(more_to_clean);
        }

        for k in store.hnsw_indices.keys() {
            let more_to_clean = self.destroy_relation(&format!("{name}:{k}"))?;
            to_clean.extend(more_to_clean);
        }

        let key = DataValue::from(name);
        let encoded = vec![key].encode_as_key(RelationId::SYSTEM);
        if is_temp {
            self.temp_store_tx.del(&encoded)?;
        } else {
            self.store_tx.del(&encoded)?;
        }
        let lower_bound = Tuple::default().encode_as_key(store.id);
        let upper_bound = Tuple::default().encode_as_key(store.id.next());
        to_clean.push((lower_bound, upper_bound));
        Ok(to_clean)
    }
    pub(crate) fn set_access_level(&mut self, rel: &Symbol, level: AccessLevel) -> Result<()> {
        let mut meta = self.get_relation(rel, true)?;
        meta.access_level = level;

        let name_key = vec![DataValue::Str(meta.name.clone())].encode_as_key(RelationId::SYSTEM);

        let mut meta_val = vec![];
        meta.serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&name_key, &meta_val)?;

        Ok(())
    }

    pub(crate) fn create_minhash_lsh_index(&mut self, config: &MinHashLshConfig) -> Result<()> {
        // Get relation handle
        let mut rel_handle = self.get_relation(&config.base_relation, true)?;
        Self::reject_txtime_relation(&rel_handle, "::lsh create")?;

        // Check if index already exists
        if rel_handle.has_index(&config.index_name) {
            bail!(IndexAlreadyExists(
                config.index_name.to_string(),
                config.index_name.to_string()
            ));
        }

        let inv_idx_keys = rel_handle.metadata.keys.clone();
        let inv_idx_vals = vec![ColumnDef {
            name: SmartString::from("minhash"),
            typing: NullableColType {
                coltype: ColType::Bytes,
                nullable: false,
            },
            default_gen: None,
        }];

        let mut idx_keys = vec![ColumnDef {
            name: SmartString::from("hash"),
            typing: NullableColType {
                coltype: ColType::Bytes,
                nullable: false,
            },
            default_gen: None,
        }];
        for k in rel_handle.metadata.keys.iter() {
            idx_keys.push(ColumnDef {
                name: format!("src_{}", k.name).into(),
                typing: k.typing.clone(),
                default_gen: None,
            });
        }
        let idx_vals = vec![];

        let idx_handle = self.write_idx_relation(
            &config.base_relation,
            &config.index_name,
            idx_keys,
            idx_vals,
        )?;

        let inv_idx_handle = self.write_idx_relation(
            &config.base_relation,
            &format!("{}:inv", config.index_name),
            inv_idx_keys,
            inv_idx_vals,
        )?;

        // add index to relation
        let params = LshParams::find_optimal_params(
            config.target_threshold.0,
            config.n_perm,
            &Weights(
                config.false_positive_weight.0,
                config.false_negative_weight.0,
            ),
        );
        let num_perm = params.b * params.r;
        let perms = HashPermutations::new(num_perm);
        let manifest = MinHashLshIndexManifest {
            base_relation: config.base_relation.clone(),
            index_name: config.index_name.clone(),
            extractor: config.extractor.clone(),
            n_gram: config.n_gram,
            tokenizer: config.tokenizer.clone(),
            filters: config.filters.clone(),
            num_perm,
            n_bands: params.b,
            n_rows_in_band: params.r,
            threshold: config.target_threshold.0,
            perms: perms.as_bytes().to_vec(),
        };

        // populate index
        let tokenizer =
            self.tokenizers
                .get(&idx_handle.name, &manifest.tokenizer, &manifest.filters)?;
        let parsed = CozoScriptParser::parse(Rule::expr, &manifest.extractor)
            .into_diagnostic()?
            .next()
            .unwrap();
        let mut code_expr = build_expr(parsed, &Default::default())?;
        let binding_map = rel_handle.raw_binding_map();
        code_expr.fill_binding_indices(&binding_map)?;
        let extractor = code_expr.compile()?;

        let mut stack = vec![];

        let hash_perms = manifest.get_hash_perms();
        let mut existing = TempCollector::default();
        for tuple in rel_handle.scan_all(self) {
            existing.push(tuple?);
        }

        for tuple in existing.into_iter() {
            self.put_lsh_index_item(
                &tuple,
                &extractor,
                &mut stack,
                &tokenizer,
                &rel_handle,
                &idx_handle,
                &inv_idx_handle,
                &manifest,
                &hash_perms,
            )?;
        }

        rel_handle.lsh_indices.insert(
            manifest.index_name.clone(),
            (idx_handle, inv_idx_handle, manifest),
        );

        // update relation metadata
        let new_encoded =
            vec![DataValue::from(&rel_handle.name as &str)].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        rel_handle
            .serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&new_encoded, &meta_val)?;

        Ok(())
    }

    pub(crate) fn create_fts_index(&mut self, config: &FtsIndexConfig) -> Result<()> {
        // Get relation handle
        let mut rel_handle = self.get_relation(&config.base_relation, true)?;
        Self::reject_txtime_relation(&rel_handle, "::fts create")?;

        // Check if index already exists
        if rel_handle.has_index(&config.index_name) {
            bail!(IndexAlreadyExists(
                config.index_name.to_string(),
                config.index_name.to_string()
            ));
        }

        // Build key columns definitions
        let mut idx_keys: Vec<ColumnDef> = vec![ColumnDef {
            name: SmartString::from("word"),
            typing: NullableColType {
                coltype: ColType::String,
                nullable: false,
            },
            default_gen: None,
        }];

        for k in rel_handle.metadata.keys.iter() {
            idx_keys.push(ColumnDef {
                name: format!("src_{}", k.name).into(),
                typing: k.typing.clone(),
                default_gen: None,
            });
        }

        let col_type = NullableColType {
            coltype: ColType::List {
                eltype: Box::new(NullableColType {
                    coltype: ColType::Int,
                    nullable: false,
                }),
                len: None,
            },
            nullable: false,
        };

        let non_idx_keys: Vec<ColumnDef> = vec![
            ColumnDef {
                name: SmartString::from("offset_from"),
                typing: col_type.clone(),
                default_gen: None,
            },
            ColumnDef {
                name: SmartString::from("offset_to"),
                typing: col_type.clone(),
                default_gen: None,
            },
            ColumnDef {
                name: SmartString::from("position"),
                typing: col_type,
                default_gen: None,
            },
            ColumnDef {
                name: SmartString::from("total_length"),
                typing: NullableColType {
                    coltype: ColType::Int,
                    nullable: false,
                },
                default_gen: None,
            },
        ];

        let idx_handle = self.write_idx_relation(
            &config.base_relation,
            &config.index_name,
            idx_keys,
            non_idx_keys,
        )?;

        // The tokenizer and corpus-stat caches below are process-local rather
        // than transactional. SQLite arms exact-name rollback after the child
        // is staged but before either cache can observe it. Other backends
        // deliberately do not: they provide no writer-serialization contract
        // that prevents a later same-name publication from being deleted.
        self.fts_abort_cache_cleanup.arm(idx_handle.name.clone());

        // add index to relation
        let manifest = FtsIndexManifest {
            base_relation: config.base_relation.clone(),
            index_name: config.index_name.clone(),
            extractor: config.extractor.clone(),
            tokenizer: config.tokenizer.clone(),
            filters: config.filters.clone(),
        };

        // populate index
        let tokenizer =
            self.tokenizers
                .get(&idx_handle.name, &manifest.tokenizer, &manifest.filters)?;

        let parsed = CozoScriptParser::parse(Rule::expr, &manifest.extractor)
            .into_diagnostic()?
            .next()
            .unwrap();
        let mut code_expr = build_expr(parsed, &Default::default())?;
        let binding_map = rel_handle.raw_binding_map();
        code_expr.fill_binding_indices(&binding_map)?;
        let extractor = code_expr.compile()?;

        let mut existing = TempCollector::default();
        for tuple in rel_handle.scan_all(self) {
            existing.push(tuple?);
        }
        let tuples: Vec<Tuple> = existing.into_iter().collect();

        // Bulk-populate (mnestic fork). The index relation above is freshly
        // created and empty, so no del pass is needed (the old code tokenised
        // every document a second time to delete postings that could not
        // exist). Tokenisation + row encoding are pure, so they fan out across
        // worker threads; writes and doc-stats stay on this thread.
        let threads = crate::runtime::hnsw_build::build_threads()
            .max(1)
            .min(tuples.len().max(1));
        let mut total_tokens: u64 = 0;
        let mut n_docs: u64 = 0;
        if threads <= 1 || tuples.len() < 64 {
            let mut stack = vec![];
            for tuple in &tuples {
                let (rows, count) = encode_fts_rows_for_tuple(
                    tuple,
                    &extractor,
                    &mut stack,
                    &tokenizer,
                    &rel_handle,
                    &idx_handle,
                )?;
                if count > 0 {
                    total_tokens += count as u64;
                    n_docs += 1;
                }
                for (key_bytes, val_bytes) in rows {
                    self.store_tx.put(&key_bytes, &val_bytes)?;
                }
            }
        } else {
            let chunk_size = tuples.len().div_ceil(threads);
            type EncodedChunk = (Vec<(Vec<u8>, Vec<u8>)>, u64, u64);
            let chunk_results: Vec<Result<EncodedChunk>> = std::thread::scope(|s| {
                let handles: Vec<_> = tuples
                    .chunks(chunk_size)
                    .map(|chunk| {
                        let extractor = &extractor;
                        let tokenizer = &tokenizer;
                        let rel_handle = &rel_handle;
                        let idx_handle = &idx_handle;
                        s.spawn(move || {
                            let mut stack = vec![];
                            let mut rows = vec![];
                            let mut total_tokens: u64 = 0;
                            let mut n_docs: u64 = 0;
                            for tuple in chunk {
                                let (tuple_rows, count) = encode_fts_rows_for_tuple(
                                    tuple, extractor, &mut stack, tokenizer, rel_handle, idx_handle,
                                )?;
                                if count > 0 {
                                    total_tokens += count as u64;
                                    n_docs += 1;
                                }
                                rows.extend(tuple_rows);
                            }
                            Ok((rows, total_tokens, n_docs))
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("FTS index build worker panicked"))
                    .collect()
            });
            for result in chunk_results {
                let (rows, chunk_tokens, chunk_docs) = result?;
                total_tokens += chunk_tokens;
                n_docs += chunk_docs;
                for (key_bytes, val_bytes) in rows {
                    self.store_tx.put(&key_bytes, &val_bytes)?;
                }
            }
        }

        // Publish the authoritative corpus doc-stats counter for this freshly
        // built index (mnestic fork, Bet 1b) so `avgdl` is an O(1) read rather
        // than a per-query scan. Totals were counted exactly during the build,
        // so no index scan is needed.
        self.seed_fts_doc_stats(&idx_handle, total_tokens, n_docs)?;

        rel_handle
            .fts_indices
            .insert(manifest.index_name.clone(), (idx_handle, manifest));

        // update relation metadata
        let new_encoded =
            vec![DataValue::from(&rel_handle.name as &str)].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        rel_handle
            .serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&new_encoded, &meta_val)?;

        Ok(())
    }

    /// Validate an HNSW index config, create its (empty) index relation, and
    /// build the manifest + compiled filter (mnestic fork). Shared by the
    /// in-transaction build (`create_hnsw_index`) and the non-blocking off-lock
    /// build orchestrated at the `Db` level. Does NOT scan/build/publish.
    pub(crate) fn prepare_hnsw_index(
        &mut self,
        config: &HnswIndexConfig,
    ) -> Result<(
        RelationHandle,
        RelationHandle,
        HnswIndexManifest,
        Vec<Bytecode>,
    )> {
        // Get relation handle
        let rel_handle = self.get_relation(&config.base_relation, true)?;
        Self::reject_txtime_relation(&rel_handle, "::hnsw create")?;

        // Check if index already exists
        if rel_handle.has_index(&config.index_name) {
            bail!(IndexAlreadyExists(
                config.index_name.to_string(),
                config.index_name.to_string()
            ));
        }

        // Check that what we are indexing are really vectors
        if config.vec_fields.is_empty() {
            bail!("Cannot create HNSW index without vector fields");
        }
        let mut vec_field_indices = vec![];
        for field in config.vec_fields.iter() {
            let mut found = false;
            for (i, col) in rel_handle
                .metadata
                .keys
                .iter()
                .chain(rel_handle.metadata.non_keys.iter())
                .enumerate()
            {
                if col.name == *field {
                    let mut col_type = col.typing.coltype.clone();
                    if let ColType::List { eltype, .. } = &col_type {
                        col_type = eltype.coltype.clone();
                    }

                    if let ColType::Vec { eltype, len } = col_type {
                        if eltype != config.dtype {
                            bail!("Cannot create HNSW index with field {} of type {:?} (expected {:?})", field, eltype, config.dtype);
                        }
                        if len != config.vec_dim {
                            bail!("Cannot create HNSW index with field {} of dimension {} (expected {})", field, len, config.vec_dim);
                        }
                    } else {
                        bail!("Cannot create HNSW index with non-vector field {}", field)
                    }

                    found = true;
                    vec_field_indices.push(i);
                    break;
                }
            }
            if !found {
                bail!("Cannot create HNSW index with non-existent field {}", field);
            }
        }

        // Build key columns definitions
        let mut idx_keys: Vec<ColumnDef> = vec![ColumnDef {
            // layer -1 stores the self-loops
            name: SmartString::from("layer"),
            typing: NullableColType {
                coltype: ColType::Int,
                nullable: false,
            },
            default_gen: None,
        }];
        // for self-loops, fr and to are identical
        for prefix in ["fr", "to"] {
            for col in rel_handle.metadata.keys.iter() {
                let mut col = col.clone();
                col.name = SmartString::from(format!("{}_{}", prefix, col.name));
                idx_keys.push(col);
            }
            idx_keys.push(ColumnDef {
                name: SmartString::from(format!("{}__field", prefix)),
                typing: NullableColType {
                    coltype: ColType::Int,
                    nullable: false,
                },
                default_gen: None,
            });
            idx_keys.push(ColumnDef {
                name: SmartString::from(format!("{}__sub_idx", prefix)),
                typing: NullableColType {
                    coltype: ColType::Int,
                    nullable: false,
                },
                default_gen: None,
            });
        }

        // Build non-key columns definitions
        let non_idx_keys = vec![
            // For self-loops, stores the number of neighbours
            ColumnDef {
                name: SmartString::from("dist"),
                typing: NullableColType {
                    coltype: ColType::Float,
                    nullable: false,
                },
                default_gen: None,
            },
            // For self-loops, stores a hash of the neighbours, for conflict detection
            ColumnDef {
                name: SmartString::from("hash"),
                typing: NullableColType {
                    coltype: ColType::Bytes,
                    nullable: true,
                },
                default_gen: None,
            },
            ColumnDef {
                name: SmartString::from("ignore_link"),
                typing: NullableColType {
                    coltype: ColType::Bool,
                    nullable: false,
                },
                default_gen: None,
            },
        ];
        // create index relation
        let idx_handle = self.write_idx_relation(
            &config.base_relation,
            &config.index_name,
            idx_keys,
            non_idx_keys,
        )?;

        // add index to relation
        let manifest = HnswIndexManifest {
            base_relation: config.base_relation.clone(),
            index_name: config.index_name.clone(),
            vec_dim: config.vec_dim,
            dtype: config.dtype,
            vec_fields: vec_field_indices,
            distance: config.distance,
            ef_construction: config.ef_construction,
            m_neighbours: config.m_neighbours,
            m_max: config.m_neighbours,
            m_max0: config.m_neighbours * 2,
            level_multiplier: 1. / (config.m_neighbours as f64).ln(),
            index_filter: config.index_filter.clone(),
            extend_candidates: config.extend_candidates,
            keep_pruned_connections: config.keep_pruned_connections,
        };

        let filter = if let Some(f_code) = &manifest.index_filter {
            let parsed = CozoScriptParser::parse(Rule::expr, f_code)
                .into_diagnostic()?
                .next()
                .unwrap();
            let mut code_expr = build_expr(parsed, &Default::default())?;
            let binding_map = rel_handle.raw_binding_map();
            code_expr.fill_binding_indices(&binding_map)?;
            code_expr.compile()?
        } else {
            vec![]
        };

        Ok((rel_handle, idx_handle, manifest, filter))
    }

    /// Build an HNSW index entirely within this transaction (mnestic fork): the
    /// graph is constructed in the in-RAM temp store, then the caller publishes
    /// the data (SST ingest, or the per-key flush). Used by non-RocksDB backends
    /// and the `skip_locking` import/restore path; RocksDB uses the non-blocking
    /// off-lock build orchestrated at the `Db` level.
    pub(crate) fn create_hnsw_index(&mut self, config: &HnswIndexConfig) -> Result<RelationId> {
        let (rel_handle, mut idx_handle, manifest, filter) = self.prepare_hnsw_index(config)?;

        let filter_ref = if filter.is_empty() {
            None
        } else {
            Some(&filter)
        };
        // Scan directly into the compact builder. In particular, a filtered
        // index never retains the full unfiltered base relation alongside its
        // vector slab.
        let build = collect_hnsw_build(
            &manifest,
            &rel_handle,
            filter_ref,
            rel_handle.scan_all(self),
            false,
        )?;
        // Build the whole graph in the in-RAM temp store (mnestic fork): marking
        // the index handle `is_temp` routes every neighbour read/write to the
        // temp BTreeMap instead of the pessimistic transaction's RocksDB
        // `WriteBatchWithIndex` overlay, whose cost grows with the index and
        // made the build superlinear.
        idx_handle.is_temp = true;
        self.hnsw_write_build(&rel_handle, &idx_handle, build)?;
        idx_handle.is_temp = false;
        let idx_id = idx_handle.id;

        self.insert_hnsw_index_meta(rel_handle, idx_handle, manifest, &config.index_name)?;
        Ok(idx_id)
    }

    /// Publish a built HNSW index by registering it in the base relation's
    /// metadata, so reads and incremental maintenance pick it up (mnestic fork).
    /// Transactional: becomes visible at `commit`.
    pub(crate) fn insert_hnsw_index_meta(
        &mut self,
        mut rel_handle: RelationHandle,
        idx_handle: RelationHandle,
        manifest: HnswIndexManifest,
        index_name: &str,
    ) -> Result<()> {
        rel_handle
            .hnsw_indices
            .insert(SmartString::from(index_name), (idx_handle, manifest));
        let new_encoded =
            vec![DataValue::from(&rel_handle.name as &str)].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        rel_handle
            .serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&new_encoded, &meta_val)?;
        Ok(())
    }

    /// Reconcile an off-lock-built HNSW index against base-relation mutations
    /// that committed during the unlocked build window (mnestic fork). `idx_table`
    /// must be the live (non-temp) handle; the bulk graph for `snapshot`
    /// has already been ingested. Diffs a current-row stream against the snapshot and
    /// applies exactly the steady-state incremental maintenance for the delta:
    /// inserts for new rows, remove+insert for changed rows, removes for deleted
    /// rows. A no-op when nothing changed during the build.
    pub(crate) fn reconcile_hnsw_index<I>(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        filter: Option<&Vec<Bytecode>>,
        snapshot: HnswBuildSnapshot,
        current: I,
    ) -> Result<()>
    where
        I: IntoIterator<Item = Result<Tuple>>,
    {
        let nkeys = orig_table.metadata.keys.len();
        let mut snap = snapshot.rows;
        let mut stack = vec![];
        for cur in current {
            let cur = cur?;
            let k = orig_table.encode_key_for_store(&cur[0..nkeys], Default::default())?;
            let eligible = match filter {
                Some(code) => crate::data::expr::eval_bytecode_pred(
                    code,
                    &cur,
                    &mut stack,
                    Default::default(),
                )?,
                None => true,
            };
            match snap.remove(&k) {
                None => {
                    // New (or previously ineligible/non-vector) row. The
                    // predicate is already known true, so do not evaluate it a
                    // second time or issue a pointless remove for false rows.
                    if eligible {
                        self.hnsw_put(manifest, orig_table, idx_table, None, &mut stack, &cur)?;
                    }
                }
                Some(old) => {
                    if !eligible {
                        self.hnsw_remove(orig_table, idx_table, &old.key)?;
                    } else if old.digest != hnsw_tuple_digest(&cur)? {
                        // Row changed in place during the build.
                        self.hnsw_remove(orig_table, idx_table, &old.key)?;
                        self.hnsw_put(manifest, orig_table, idx_table, None, &mut stack, &cur)?;
                    }
                }
            }
        }
        // rows present at snapshot but gone now: deleted during the build
        for old in snap.into_values() {
            self.hnsw_remove(orig_table, idx_table, &old.key)?;
        }
        Ok(())
    }

    /// Bulk-copy a freshly-built index relation from the in-RAM temp store to
    /// the persistent store (mnestic fork). The entries arrive in key-sorted
    /// order (the temp store is a `BTreeMap`). Used by engines that do not
    /// support SST ingest; the RocksDB path publishes via `ingest_sorted`.
    pub(crate) fn flush_temp_index_to_store(&mut self, idx_id: RelationId) -> Result<()> {
        let lower = Tuple::default().encode_as_key(idx_id);
        let upper = Tuple::default().encode_as_key(idx_id.next());
        // These are disjoint fields: retain the temp-store iterator while
        // writing each cloned entry straight to the persistent transaction.
        // Collecting first duplicated the complete serialized HNSW index in a
        // second Vec on SQLite. The graph has dropped by this point, but the
        // duplicate could itself become the operation's memory peak.
        let (temp_store_tx, store_tx) = (&self.temp_store_tx, &mut self.store_tx);
        for entry in temp_store_tx.range_scan(&lower, &upper) {
            let (key, value) = entry?;
            store_tx.put(&key, &value)?;
        }
        Ok(())
    }

    fn write_idx_relation(
        &mut self,
        base_name: &str,
        idx_name: &str,
        idx_keys: Vec<ColumnDef>,
        non_idx_keys: Vec<ColumnDef>,
    ) -> Result<RelationHandle> {
        let key_bindings = idx_keys
            .iter()
            .map(|col| Symbol::new(col.name.clone(), Default::default()))
            .collect();
        let dep_bindings = non_idx_keys
            .iter()
            .map(|col| Symbol::new(col.name.clone(), Default::default()))
            .collect();
        let idx_handle = InputRelationHandle {
            name: Symbol::new(format!("{}:{}", base_name, idx_name), Default::default()),
            metadata: StoredRelationMetadata {
                keys: idx_keys,
                non_keys: non_idx_keys,
            },
            key_bindings,
            dep_bindings,
            span: Default::default(),
        };
        let idx_handle = self.create_relation(idx_handle)?;
        Ok(idx_handle)
    }

    pub(crate) fn create_index(
        &mut self,
        rel_name: &Symbol,
        idx_name: &Symbol,
        cols: &[Symbol],
    ) -> Result<()> {
        // Get relation handle
        let mut rel_handle = self.get_relation(rel_name, true)?;
        Self::reject_txtime_relation(&rel_handle, "::index create")?;

        // Check if index already exists
        if rel_handle.has_index(&idx_name.name) {
            bail!(IndexAlreadyExists(
                idx_name.name.to_string(),
                rel_name.name.to_string()
            ));
        }

        // Build column definitions
        let mut col_defs = vec![];
        'outer: for col in cols.iter() {
            for orig_col in rel_handle
                .metadata
                .keys
                .iter()
                .chain(rel_handle.metadata.non_keys.iter())
            {
                if orig_col.name == col.name {
                    col_defs.push(orig_col.clone());
                    continue 'outer;
                }
            }

            #[derive(Debug, Error, Diagnostic)]
            #[error("column {0} in index {1} for relation {2} not found")]
            #[diagnostic(code(tx::col_in_idx_not_found))]
            pub(crate) struct ColInIndexNotFound(String, String, String);

            bail!(ColInIndexNotFound(
                col.name.to_string(),
                idx_name.name.to_string(),
                rel_name.name.to_string()
            ));
        }

        'outer: for key in rel_handle.metadata.keys.iter() {
            for col in cols.iter() {
                if col.name == key.name {
                    continue 'outer;
                }
            }
            col_defs.push(key.clone());
        }

        let key_bindings = col_defs
            .iter()
            .map(|col| Symbol::new(col.name.clone(), Default::default()))
            .collect_vec();
        let idx_meta = StoredRelationMetadata {
            keys: col_defs,
            non_keys: vec![],
        };

        // create index relation
        let idx_handle = InputRelationHandle {
            name: Symbol::new(
                format!("{}:{}", rel_name.name, idx_name.name),
                Default::default(),
            ),
            metadata: idx_meta,
            key_bindings,
            dep_bindings: vec![],
            span: Default::default(),
        };

        let idx_handle = self.create_relation(idx_handle)?;

        // populate index
        let extraction_indices = idx_handle
            .metadata
            .keys
            .iter()
            .map(|col| {
                for (i, kc) in rel_handle.metadata.keys.iter().enumerate() {
                    if kc.name == col.name {
                        return i;
                    }
                }
                for (i, kc) in rel_handle.metadata.non_keys.iter().enumerate() {
                    if kc.name == col.name {
                        return i + rel_handle.metadata.keys.len();
                    }
                }
                unreachable!()
            })
            .collect_vec();

        // A corrupt/truncated stored tuple (e.g. from an interrupted write)
        // must degrade to "left out of this index" with a loud error — not
        // panic the whole index build. Index builds run inside `::index
        // create`, which applications may execute while (re)initializing a
        // database: a panic here turns one bad row into an unopenable
        // database (observed in production 2026-06-12: a 3-element tuple in
        // a relation whose index needed column 5 made every open attempt
        // panic until the tenant was blacklisted).
        let mut skipped_corrupt = 0usize;
        let mut check_tuple = |tuple: &Tuple| -> bool {
            match extraction_indices.iter().find(|idx| **idx >= tuple.len()) {
                None => true,
                Some(&bad) => {
                    skipped_corrupt += 1;
                    error!(
                        "skipping corrupt tuple in '{}' while building index '{}': \
                         tuple has {} values, index needs column {}",
                        rel_handle.name,
                        idx_handle.name,
                        tuple.len(),
                        bad + 1
                    );
                    false
                }
            }
        };
        if self.store_tx.supports_par_put() {
            for tuple in rel_handle.scan_all(self) {
                let tuple = tuple?;
                if !check_tuple(&tuple) {
                    continue;
                }
                let extracted = extraction_indices
                    .iter()
                    .map(|idx| tuple[*idx].clone())
                    .collect_vec();
                let key = idx_handle.encode_key_for_store(&extracted, Default::default())?;
                self.store_tx.par_put(&key, &[])?;
            }
        } else {
            let mut existing = TempCollector::default();
            for tuple in rel_handle.scan_all(self) {
                existing.push(tuple?);
            }
            for tuple in existing.into_iter() {
                if !check_tuple(&tuple) {
                    continue;
                }
                let extracted = extraction_indices
                    .iter()
                    .map(|idx| tuple[*idx].clone())
                    .collect_vec();
                let key = idx_handle.encode_key_for_store(&extracted, Default::default())?;
                self.store_tx.put(&key, &[])?;
            }
        }
        if skipped_corrupt > 0 {
            error!(
                "index '{}' built with {} corrupt tuple(s) skipped — the base \
                 relation '{}' needs repair",
                idx_handle.name, skipped_corrupt, rel_handle.name
            );
        }

        // add index to relation
        rel_handle
            .indices
            .insert(idx_name.name.clone(), (idx_handle, extraction_indices));

        // update relation metadata
        let new_encoded =
            vec![DataValue::from(&rel_name.name as &str)].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        rel_handle
            .serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&new_encoded, &meta_val)?;

        Ok(())
    }

    pub(crate) fn remove_index(
        &mut self,
        rel_name: &Symbol,
        idx_name: &Symbol,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut rel = self.get_relation(rel_name, true)?;
        let is_lsh = rel.lsh_indices.contains_key(&idx_name.name);
        let is_fts = rel.fts_indices.contains_key(&idx_name.name);
        if is_lsh || is_fts {
            self.tokenizers.named_cache.write().unwrap().clear();
            self.tokenizers.hashed_cache.write().unwrap().clear();
        }
        if rel.indices.remove(&idx_name.name).is_none()
            && rel.hnsw_indices.remove(&idx_name.name).is_none()
            && rel.lsh_indices.remove(&idx_name.name).is_none()
            && rel.fts_indices.remove(&idx_name.name).is_none()
        {
            #[derive(Debug, Error, Diagnostic)]
            #[error("index {0} for relation {1} not found")]
            #[diagnostic(code(tx::idx_not_found))]
            pub(crate) struct IndexNotFound(String, String);

            bail!(IndexNotFound(idx_name.to_string(), rel_name.to_string()));
        }

        let mut to_clean =
            self.destroy_relation(&format!("{}:{}", rel_name.name, idx_name.name))?;
        if is_lsh {
            to_clean.extend(
                self.destroy_relation(&format!("{}:{}:inv", rel_name.name, idx_name.name))?,
            );
        }

        let new_encoded =
            vec![DataValue::from(&rel_name.name as &str)].encode_as_key(RelationId::SYSTEM);
        let mut meta_val = vec![];
        rel.serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.put(&new_encoded, &meta_val)?;

        Ok(to_clean)
    }

    pub(crate) fn rename_relation(&mut self, old: &Symbol, new: &Symbol) -> Result<()> {
        if old.name.starts_with('_') || new.name.starts_with('_') {
            bail!("Bad name given");
        }
        let mut rel = self.get_relation(old, true)?;
        if rel.access_level < AccessLevel::Normal {
            bail!(InsufficientAccessLevel(
                rel.name.to_string(),
                "renaming relation".to_string(),
                rel.access_level
            ));
        }

        // Index row keys contain relation IDs rather than relation names, so a
        // base-relation rename must leave their data in place. The relation
        // catalogue, however, contains a standalone handle for every index as
        // well as copies embedded in the base handle. Move all of those
        // catalogue entries together or the renamed base points at children
        // that can no longer be dropped (and manifests still target the old
        // base name).
        let mut child_moves = Vec::new();
        let mut source_names = BTreeSet::from([old.name.clone()]);
        let mut destination_names = BTreeSet::from([new.name.clone()]);

        for (index_name, (handle, _)) in &rel.indices {
            self.stage_index_relation_rename(
                handle,
                format!("{}:{}", new.name, index_name).into(),
                &mut source_names,
                &mut destination_names,
                &mut child_moves,
            )?;
        }
        for (index_name, (handle, _)) in &rel.hnsw_indices {
            self.stage_index_relation_rename(
                handle,
                format!("{}:{}", new.name, index_name).into(),
                &mut source_names,
                &mut destination_names,
                &mut child_moves,
            )?;
        }
        for (index_name, (handle, _)) in &rel.fts_indices {
            self.stage_index_relation_rename(
                handle,
                format!("{}:{}", new.name, index_name).into(),
                &mut source_names,
                &mut destination_names,
                &mut child_moves,
            )?;
        }
        for (index_name, (handle, inverse_handle, _)) in &rel.lsh_indices {
            self.stage_index_relation_rename(
                handle,
                format!("{}:{}", new.name, index_name).into(),
                &mut source_names,
                &mut destination_names,
                &mut child_moves,
            )?;
            self.stage_index_relation_rename(
                inverse_handle,
                format!("{}:{}:inv", new.name, index_name).into(),
                &mut source_names,
                &mut destination_names,
                &mut child_moves,
            )?;
        }

        // Check every destination before deleting a single source key. Apart
        // from producing a useful conflict error, this keeps a failed rename
        // from exposing a partially rewritten catalogue even to transaction
        // implementations whose write overlays are observable internally.
        let mut destination_keys = BTreeMap::new();
        for destination in &destination_names {
            let encoded = encode_catalog_name_for_store(destination, new.span)?;
            if self.store_tx.exists(&encoded, true)? {
                bail!(RelNameConflictError(destination.to_string()))
            }
            destination_keys.insert(destination.clone(), encoded);
        }

        for (index_name, (handle, _)) in &mut rel.indices {
            handle.name = format!("{}:{}", new.name, index_name).into();
        }
        for (index_name, (handle, manifest)) in &mut rel.hnsw_indices {
            handle.name = format!("{}:{}", new.name, index_name).into();
            manifest.base_relation = new.name.clone();
        }
        for (index_name, (handle, manifest)) in &mut rel.fts_indices {
            handle.name = format!("{}:{}", new.name, index_name).into();
            manifest.base_relation = new.name.clone();
        }
        for (index_name, (handle, inverse_handle, manifest)) in &mut rel.lsh_indices {
            handle.name = format!("{}:{}", new.name, index_name).into();
            inverse_handle.name = format!("{}:{}:inv", new.name, index_name).into();
            manifest.base_relation = new.name.clone();
        }

        for (old_name, new_name, mut child) in child_moves {
            let old_encoded = vec![DataValue::Str(old_name)].encode_as_key(RelationId::SYSTEM);
            let new_encoded = destination_keys
                .get(&new_name)
                .expect("every staged child destination was preflighted above");
            child.name = new_name;

            let mut meta_val = vec![];
            child
                .serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
                .unwrap();
            self.store_tx.del(&old_encoded)?;
            self.store_tx.put(new_encoded, &meta_val)?;
        }

        let old_encoded = vec![DataValue::Str(old.name.clone())].encode_as_key(RelationId::SYSTEM);
        let new_encoded = destination_keys
            .get(&new.name)
            .expect("base destination was preflighted above");
        rel.name = new.name.clone();

        let mut meta_val = vec![];
        rel.serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.store_tx.del(&old_encoded)?;
        self.store_tx.put(new_encoded, &meta_val)?;

        if !rel.fts_indices.is_empty() {
            // BM25 corpus stats are keyed by the FTS child relation's full
            // `base:index` name. Invalidate both sides rather than moving an
            // entry: a multi-relation swap is applied one rename at a time, so
            // the destination name may still cache the generation displaced by
            // an earlier pair in this same transaction.
            let mut stats = self.fts_doc_stats_cache.lock().unwrap();
            for index_name in rel.fts_indices.keys() {
                stats.remove(format!("{}:{}", old.name, index_name).as_str());
                stats.remove(format!("{}:{}", new.name, index_name).as_str());
            }
        }

        if !rel.fts_indices.is_empty() || !rel.lsh_indices.is_empty() {
            self.tokenizers.named_cache.write().unwrap().clear();
            self.tokenizers.hashed_cache.write().unwrap().clear();
        }

        Ok(())
    }

    fn stage_index_relation_rename(
        &self,
        embedded: &RelationHandle,
        destination: SmartString<LazyCompact>,
        source_names: &mut BTreeSet<SmartString<LazyCompact>>,
        destination_names: &mut BTreeSet<SmartString<LazyCompact>>,
        moves: &mut Vec<(
            SmartString<LazyCompact>,
            SmartString<LazyCompact>,
            RelationHandle,
        )>,
    ) -> Result<()> {
        ensure!(
            source_names.insert(embedded.name.clone()),
            "relation metadata contains duplicate index child `{}`",
            embedded.name
        );
        ensure!(
            destination_names.insert(destination.clone()),
            "relation metadata maps multiple index children to `{destination}`"
        );

        let stored = self.get_relation(&embedded.name as &str, true)?;
        ensure!(
            stored.id == embedded.id,
            "index child `{}` has relation ID {:?}, but its base metadata records {:?}",
            embedded.name,
            stored.id,
            embedded.id
        );
        moves.push((embedded.name.clone(), destination, stored));
        Ok(())
    }
    pub(crate) fn rename_temp_relation(&mut self, old: Symbol, new: Symbol) -> Result<()> {
        let new_encoded = encode_catalog_name_for_store(&new.name, new.span)?;

        if self.temp_store_tx.exists(&new_encoded, true)? {
            bail!(RelNameConflictError(new.name.to_string()))
        };

        let old_key = DataValue::Str(old.name.clone());
        let old_encoded = vec![old_key].encode_as_key(RelationId::SYSTEM);

        let mut rel = self.get_relation(&old, true)?;
        rel.name = new.name;

        let mut meta_val = vec![];
        rel.serialize(&mut Serializer::new(&mut meta_val).with_struct_map())
            .unwrap();
        self.temp_store_tx.del(&old_encoded)?;
        self.temp_store_tx.put(&new_encoded, &meta_val)?;

        Ok(())
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("Insufficient access level {2} for {1} on stored relation '{0}'")]
#[diagnostic(code(tx::insufficient_access_level))]
pub(crate) struct InsufficientAccessLevel(
    pub(crate) String,
    pub(crate) String,
    pub(crate) AccessLevel,
);

#[cfg(test)]
mod relation_id_decode_tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use sha2::{Digest, Sha256};

    use super::{
        bounded_decode_reason, try_decode_tuple_from_kv, try_decode_val_only, RelationId,
        RELATION_ID_EXCLUSIVE_END, STORED_RELATION_ID_POLICY_DESCRIPTION_V1,
        STORED_RELATION_ID_POLICY_FINGERPRINT_DOMAIN_V1, STORED_RELATION_ID_POLICY_FINGERPRINT_V1,
    };
    use crate::data::memcmp::MemCmpEncoder;
    use crate::data::value::DataValue;

    #[test]
    fn relation_id_decoding_is_fallible_and_width_specific() {
        assert!(RelationId::try_raw_decode(&[0; 7]).is_err());
        assert!(RelationId::try_raw_decode(&[0; 9]).is_err());
        assert_eq!(
            RelationId::try_raw_decode(&42_u64.to_be_bytes()).unwrap().0,
            42
        );

        let mut prefixed = 42_u64.to_be_bytes().to_vec();
        prefixed.push(0x01);
        assert_eq!(RelationId::try_raw_decode_prefix(&prefixed).unwrap().0, 42);
        assert!(RelationId::try_raw_decode_prefix(&[0; 7]).is_err());

        let max_storable = (RELATION_ID_EXCLUSIVE_END - 1).to_be_bytes();
        assert!(RelationId::try_raw_decode(&max_storable).is_ok());
        assert!(RelationId::try_raw_decode_prefix(&max_storable).is_ok());
        assert_eq!(
            RelationId::new(RELATION_ID_EXCLUSIVE_END - 1).next().0,
            RELATION_ID_EXCLUSIVE_END
        );

        let terminal = RELATION_ID_EXCLUSIVE_END.to_be_bytes();
        assert!(RelationId::try_raw_decode(&terminal).is_err());
        assert!(RelationId::try_raw_decode_prefix(&terminal).is_err());
        assert!(std::panic::catch_unwind(|| RelationId::new(RELATION_ID_EXCLUSIVE_END)).is_err());
    }

    #[test]
    fn stored_relation_id_policy_fingerprint_is_a_hard_pinned_literal_oracle() {
        let max = RelationId::new(RELATION_ID_EXCLUSIVE_END - 1);
        let max_wire = [0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        assert_eq!(max.raw_encode(), max_wire);
        assert_eq!(RelationId::try_raw_decode(&max_wire).unwrap(), max);
        assert_eq!(RelationId::try_raw_decode_prefix(&max_wire).unwrap(), max);
        assert_eq!(max.next().0, RELATION_ID_EXCLUSIVE_END);

        let mut hasher = Sha256::new();
        hasher.update(STORED_RELATION_ID_POLICY_FINGERPRINT_DOMAIN_V1);
        let mut field_count = 0_u64;
        for (label, value) in [
            (
                b"policy-description".as_slice(),
                STORED_RELATION_ID_POLICY_DESCRIPTION_V1.as_bytes(),
            ),
            (b"maximum-persisted-kat".as_slice(), max_wire.as_slice()),
            (
                b"exclusive-end".as_slice(),
                RELATION_ID_EXCLUSIVE_END.to_be_bytes().as_slice(),
            ),
            (b"prefix-width".as_slice(), 8_u64.to_be_bytes().as_slice()),
        ] {
            field_count += 1;
            hasher.update([0x01]);
            hasher.update(u64::try_from(label.len()).unwrap().to_be_bytes());
            hasher.update(label);
            hasher.update(u64::try_from(value.len()).unwrap().to_be_bytes());
            hasher.update(value);
        }
        hasher.update([0xff]);
        hasher.update(field_count.to_be_bytes());
        let derived: [u8; 32] = hasher.finalize().into();
        assert_eq!(derived, STORED_RELATION_ID_POLICY_FINGERPRINT_V1);
        assert_ne!(STORED_RELATION_ID_POLICY_FINGERPRINT_V1, [0; 32]);
    }

    #[test]
    fn allocation_never_issues_or_persists_the_scan_sentinel() {
        let counter = AtomicU64::new(RELATION_ID_EXCLUSIVE_END - 2);
        let last_storable = RelationId::allocate_persisted(&counter).unwrap();
        assert_eq!(last_storable.0, RELATION_ID_EXCLUSIVE_END - 1);
        assert!(RelationId::try_raw_decode(&last_storable.raw_encode()).is_ok());

        let error = RelationId::allocate_persisted(&counter).unwrap_err();
        assert!(error.to_string().contains("space exhausted"));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            RELATION_ID_EXCLUSIVE_END - 1
        );
    }

    #[test]
    fn value_prefix_must_be_exact_valid_and_match_its_key() {
        let relation_id = 42_u64;
        let mut key = relation_id.to_be_bytes().to_vec();
        key.encode_datavalue(&DataValue::Null);

        let mut value = relation_id.to_be_bytes().to_vec();
        value.extend(rmp_serde::to_vec(&vec![DataValue::from(7)]).unwrap());
        assert_eq!(
            try_decode_tuple_from_kv(&key, &value, None).unwrap(),
            vec![DataValue::Null, DataValue::from(7)]
        );
        assert_eq!(
            try_decode_val_only(&key, &value).unwrap(),
            vec![DataValue::from(7)]
        );

        assert!(try_decode_tuple_from_kv(&key, &[0; 7], None).is_err());

        let mut mismatch = 43_u64.to_be_bytes().to_vec();
        mismatch.extend(rmp_serde::to_vec(&Vec::<DataValue>::new()).unwrap());
        assert!(try_decode_tuple_from_kv(&key, &mismatch, None)
            .unwrap_err()
            .to_string()
            .contains("does not match"));
        assert!(try_decode_val_only(&key, &mismatch).is_err());

        let mut terminal = RELATION_ID_EXCLUSIVE_END.to_be_bytes().to_vec();
        terminal.extend(rmp_serde::to_vec(&Vec::<DataValue>::new()).unwrap());
        assert!(try_decode_tuple_from_kv(&key, &terminal, None).is_err());
        assert!(try_decode_val_only(&key, &terminal).is_err());
    }

    #[test]
    fn value_messagepack_must_be_one_exact_bounded_array() {
        let relation_id = 42_u64;
        let mut key = relation_id.to_be_bytes().to_vec();
        key.encode_datavalue(&DataValue::Null);

        let mut wrong_root = relation_id.to_be_bytes().to_vec();
        wrong_root.push(0xc0); // nil scalar, not the required tuple array
        let error = try_decode_val_only(&key, &wrong_root).unwrap_err();
        assert!(error.to_string().contains("WrongRoot"));

        let mut trailing = relation_id.to_be_bytes().to_vec();
        trailing.extend(rmp_serde::to_vec(&Vec::<DataValue>::new()).unwrap());
        trailing.push(0xc0);
        let error = try_decode_val_only(&key, &trailing).unwrap_err();
        assert!(error.to_string().contains("TrailingBytes"));

        let mut oversized = relation_id.to_be_bytes().to_vec();
        oversized.resize(8 + 1024 * 1024 + 1, 0x90);
        let error = try_decode_val_only(&key, &oversized).unwrap_err();
        assert!(error.to_string().contains("ByteLimit"));
        assert!(error.to_string().len() < 512);
    }

    #[test]
    fn value_decode_reasons_are_bounded_in_bytes() {
        let reason = bounded_decode_reason("é".repeat(300));
        assert!(reason.len() <= 256, "reason was {} bytes", reason.len());
        assert!(reason.ends_with('…'));
    }
}

#[cfg(test)]
mod stored_key_writer_tests {
    use std::cmp::Reverse;
    use std::collections::BTreeSet;

    use ndarray::Array1;
    use regex::RegexBuilder;

    use super::{
        encode_catalog_name_for_store, AccessLevel, RelationHandle, RelationId,
        RELATION_ID_EXCLUSIVE_END,
    };
    use crate::data::functions::{MAX_VALIDITY_TS, TERMINAL_VALIDITY};
    use crate::data::memcmp::{
        MAX_ENCODED_KEY_BYTES, MAX_KEY_JSON_DEPTH, MAX_KEY_JSON_NODES, MAX_KEY_NESTING_DEPTH,
        MAX_KEY_VALUES, MAX_KEY_VECTOR_ELEMENTS,
    };
    use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
    use crate::data::tuple::try_decode_tuple_from_key;
    use crate::data::value::{DataValue, JsonData, RegexWrapper, Validity, Vector};

    fn one_key_handle() -> RelationHandle {
        RelationHandle {
            name: "writer_limits".into(),
            id: RelationId::new(1),
            metadata: StoredRelationMetadata {
                keys: vec![ColumnDef {
                    name: "key".into(),
                    typing: NullableColType {
                        coltype: ColType::Any,
                        nullable: false,
                    },
                    default_gen: None,
                }],
                non_keys: vec![],
            },
            put_triggers: vec![],
            rm_triggers: vec![],
            replace_triggers: vec![],
            access_level: AccessLevel::Normal,
            is_temp: false,
            indices: Default::default(),
            hnsw_indices: Default::default(),
            fts_indices: Default::default(),
            lsh_indices: Default::default(),
            description: Default::default(),
            tt_gc_floor: None,
        }
    }

    fn nested_lists(depth: usize) -> DataValue {
        let mut value = DataValue::Null;
        for _ in 0..depth {
            value = DataValue::List(vec![value]);
        }
        value
    }

    fn nested_json(depth: usize) -> DataValue {
        let mut value = serde_json::Value::Null;
        for _ in 0..depth {
            value = serde_json::Value::Array(vec![value]);
        }
        DataValue::Json(JsonData(value))
    }

    #[test]
    fn persisted_writer_structural_caps_are_exact() {
        let handle = one_key_handle();

        let value_cap = DataValue::List(vec![DataValue::Null; MAX_KEY_VALUES - 1]);
        handle
            .encode_key_for_store(&[value_cap], Default::default())
            .unwrap();
        let value_over = DataValue::List(vec![DataValue::Null; MAX_KEY_VALUES]);
        assert!(handle
            .encode_key_for_store(&[value_over], Default::default())
            .unwrap_err()
            .to_string()
            .contains("more than 256 values"));

        handle
            .encode_key_for_store(&[nested_lists(MAX_KEY_NESTING_DEPTH)], Default::default())
            .unwrap();
        assert!(handle
            .encode_key_for_store(
                &[nested_lists(MAX_KEY_NESTING_DEPTH + 1)],
                Default::default(),
            )
            .unwrap_err()
            .to_string()
            .contains("nesting exceeds 16"));

        let vector_cap = DataValue::Vec(Vector::F32(Array1::zeros(MAX_KEY_VECTOR_ELEMENTS)));
        handle
            .encode_key_for_store(&[vector_cap], Default::default())
            .unwrap();
        let vector_over = DataValue::Vec(Vector::F32(Array1::zeros(MAX_KEY_VECTOR_ELEMENTS + 1)));
        assert!(handle
            .encode_key_for_store(&[vector_over], Default::default())
            .unwrap_err()
            .to_string()
            .contains("limit is 4096"));

        for validity in [
            TERMINAL_VALIDITY,
            Validity {
                timestamp: MAX_VALIDITY_TS,
                is_assert: Reverse(true),
            },
        ] {
            assert!(handle
                .encode_key_for_store(&[DataValue::Validity(validity)], Default::default())
                .unwrap_err()
                .to_string()
                .contains("reserved engine sentinel"));
        }
    }

    #[test]
    fn persisted_writer_encoded_byte_cap_is_exact() {
        let handle = one_key_handle();
        let mut elements = vec![DataValue::Bytes(vec![0; 58_232])];
        elements.resize(6, DataValue::Null);
        let encoded = handle
            .encode_key_for_store(&[DataValue::List(elements.clone())], Default::default())
            .unwrap();
        assert_eq!(encoded.len(), MAX_ENCODED_KEY_BYTES);

        elements.push(DataValue::Null);
        let error = handle
            .encode_key_for_store(&[DataValue::List(elements)], Default::default())
            .unwrap_err();
        assert!(error.to_string().contains("65537 bytes"));
    }

    #[test]
    fn persisted_json_keys_have_bounded_and_symmetric_work() {
        let handle = one_key_handle();
        let accepted = nested_json(MAX_KEY_JSON_DEPTH);
        let encoded = handle
            .encode_key_for_store(std::slice::from_ref(&accepted), Default::default())
            .unwrap();
        assert_eq!(
            try_decode_tuple_from_key(&encoded, 1).unwrap(),
            vec![accepted]
        );

        let error = handle
            .encode_key_for_store(&[nested_json(MAX_KEY_JSON_DEPTH + 1)], Default::default())
            .unwrap_err();
        assert!(error.to_string().contains("JSON nesting exceeds 64"));

        let too_many_nodes = DataValue::Json(JsonData(serde_json::Value::Array(vec![
            serde_json::Value::Null;
            MAX_KEY_JSON_NODES
        ])));
        let error = handle
            .encode_key_for_store(&[too_many_nodes], Default::default())
            .unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("more than 65536 values"));
        assert!(
            rendered.len() < 512,
            "diagnostic was not bounded: {rendered}"
        );

        let oversized_scalar = DataValue::Json(JsonData(serde_json::Value::String(
            "x".repeat(MAX_ENCODED_KEY_BYTES),
        )));
        let error = handle
            .encode_key_for_store(&[oversized_scalar], Default::default())
            .unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("serialization work limit"));
        assert!(
            rendered.len() < 512,
            "diagnostic was not bounded: {rendered}"
        );
    }

    #[test]
    fn checked_tt_partial_and_catalog_name_writes_share_the_cap() {
        let handle = one_key_handle();
        assert_eq!(
            handle
                .encode_partial_key_for_store(&[DataValue::Bot])
                .last(),
            Some(&0xff),
            "the unchecked partial encoder remains available for internal bounds"
        );
        for value in [DataValue::Bot, DataValue::List(vec![DataValue::Bot])] {
            assert!(handle
                .encode_key_for_store(&[value], Default::default())
                .unwrap_err()
                .to_string()
                .contains("reserved for internal key bounds"));
        }

        let mut regex_builder = RegexBuilder::new("mneme");
        regex_builder.case_insensitive(true);
        let custom_regex = DataValue::Regex(RegexWrapper(regex_builder.build().unwrap()));
        let internal_set = DataValue::Set(BTreeSet::from([DataValue::Null]));
        for (value, expected) in [
            (custom_regex.clone(), "Regex is internal-only"),
            (
                DataValue::List(vec![custom_regex]),
                "Regex is internal-only",
            ),
            (internal_set.clone(), "Set is internal-only"),
            (DataValue::List(vec![internal_set]), "Set is internal-only"),
        ] {
            assert!(handle
                .encode_key_for_store(&[value], Default::default())
                .unwrap_err()
                .to_string()
                .contains(expected));
        }

        let too_many = DataValue::List(vec![DataValue::Null; MAX_KEY_VALUES]);
        assert!(handle
            .try_encode_partial_key_for_store(&[too_many], Default::default())
            .is_err());

        let oversized_name = "x".repeat(58_240).into();
        assert!(encode_catalog_name_for_store(&oversized_name, Default::default()).is_err());

        let mut sentinel_handle = handle;
        // Construct the private exclusive scan sentinel directly: the public
        // persisted-id constructor must reject it.
        sentinel_handle.id = RelationId(RELATION_ID_EXCLUSIVE_END);
        assert!(sentinel_handle
            .encode_key_for_store(&[DataValue::Null], Default::default())
            .unwrap_err()
            .to_string()
            .contains("not persistable"));
    }

    #[test]
    fn stored_value_writers_return_layout_and_envelope_errors() {
        use ndarray::s;

        let handle = one_key_handle();
        let reversed = Array1::from_vec(vec![1.0_f32, 2.0, 3.0]).slice_move(s![..;-1]);
        assert!(reversed.as_slice().is_none());
        let error = handle
            .encode_val_only_for_store(&[DataValue::Vec(Vector::F32(reversed))], Default::default())
            .unwrap_err();
        assert!(error.to_string().contains("non-contiguous f32"));

        let error = handle
            .encode_val_only_for_store(
                &[DataValue::Bytes(vec![0; 1024 * 1024])],
                Default::default(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("ByteLimit"));
    }
}

#[cfg(test)]
pub(crate) mod catalog_compat_tests {
    use super::RelationHandle;
    use rmp_serde::Serializer;
    use serde::Serialize;

    /// Real, on-disk `edge` relation catalog captured from a production
    /// mindgraph graph created before mnestic 0.10.0. It is a **13-field
    /// positional msgpack array** (rmp_serde struct-as-array, written by an
    /// index-creation path predating `with_struct_map`) — i.e. it has NO
    /// `tt_gc_floor` element.
    ///
    /// Regression guard for the 0.10.0 outage: `tt_gc_floor` was added
    /// mid-struct, so positional decode misread the `indices` map as
    /// `Option<i64>` and every legacy graph failed to open with
    /// "Cannot deserialize relation metadata from bytes". Keeping the field
    /// last (with `#[serde(default)]`) makes the trailing element optional.
    #[rustfmt::skip]
    pub(crate) const LEGACY_EDGE_CATALOG: &[u8] = &[
    157, 164, 101, 100, 103, 101, 2, 146, 145, 147, 163, 117, 105, 100, 146, 166,
    83, 116, 114, 105, 110, 103, 194, 192, 155, 147, 168, 102, 114, 111, 109, 95,
    117, 105, 100, 146, 166, 83, 116, 114, 105, 110, 103, 194, 192, 147, 166, 116,
    111, 95, 117, 105, 100, 146, 166, 83, 116, 114, 105, 110, 103, 194, 192, 147,
    169, 101, 100, 103, 101, 95, 116, 121, 112, 101, 146, 166, 83, 116, 114, 105,
    110, 103, 194, 192, 147, 165, 108, 97, 121, 101, 114, 146, 166, 83, 116, 114,
    105, 110, 103, 194, 192, 147, 170, 99, 114, 101, 97, 116, 101, 100, 95, 97,
    116, 146, 165, 70, 108, 111, 97, 116, 194, 192, 147, 170, 117, 112, 100, 97,
    116, 101, 100, 95, 97, 116, 146, 165, 70, 108, 111, 97, 116, 194, 192, 147,
    167, 118, 101, 114, 115, 105, 111, 110, 146, 163, 73, 110, 116, 194, 129, 165,
    67, 111, 110, 115, 116, 145, 129, 163, 78, 117, 109, 129, 163, 73, 110, 116,
    1, 147, 170, 99, 111, 110, 102, 105, 100, 101, 110, 99, 101, 146, 165, 70,
    108, 111, 97, 116, 194, 129, 165, 67, 111, 110, 115, 116, 145, 129, 163, 78,
    117, 109, 129, 165, 70, 108, 111, 97, 116, 203, 63, 240, 0, 0, 0, 0,
    0, 0, 147, 166, 119, 101, 105, 103, 104, 116, 146, 165, 70, 108, 111, 97,
    116, 194, 129, 165, 67, 111, 110, 115, 116, 145, 129, 163, 78, 117, 109, 129,
    165, 70, 108, 111, 97, 116, 203, 63, 224, 0, 0, 0, 0, 0, 0, 147,
    172, 116, 111, 109, 98, 115, 116, 111, 110, 101, 95, 97, 116, 146, 165, 70,
    108, 111, 97, 116, 194, 129, 165, 67, 111, 110, 115, 116, 145, 129, 163, 78,
    117, 109, 129, 165, 70, 108, 111, 97, 116, 203, 0, 0, 0, 0, 0, 0,
    0, 0, 147, 165, 112, 114, 111, 112, 115, 146, 164, 74, 115, 111, 110, 194,
    129, 165, 65, 112, 112, 108, 121, 146, 174, 79, 80, 95, 74, 83, 79, 78,
    95, 79, 66, 74, 69, 67, 84, 144, 144, 144, 144, 166, 78, 111, 114, 109,
    97, 108, 194, 133, 168, 102, 114, 111, 109, 95, 105, 100, 120, 146, 157, 173,
    101, 100, 103, 101, 58, 102, 114, 111, 109, 95, 105, 100, 120, 8, 146, 147,
    147, 168, 102, 114, 111, 109, 95, 117, 105, 100, 146, 166, 83, 116, 114, 105,
    110, 103, 194, 192, 147, 169, 101, 100, 103, 101, 95, 116, 121, 112, 101, 146,
    166, 83, 116, 114, 105, 110, 103, 194, 192, 147, 163, 117, 105, 100, 146, 166,
    83, 116, 114, 105, 110, 103, 194, 192, 144, 144, 144, 144, 166, 78, 111, 114,
    109, 97, 108, 194, 128, 128, 128, 128, 160, 147, 1, 3, 0, 171, 102, 114,
    111, 109, 95, 116, 111, 95, 105, 100, 120, 146, 157, 176, 101, 100, 103, 101,
    58, 102, 114, 111, 109, 95, 116, 111, 95, 105, 100, 120, 25, 146, 147, 147,
    168, 102, 114, 111, 109, 95, 117, 105, 100, 146, 166, 83, 116, 114, 105, 110,
    103, 194, 192, 147, 166, 116, 111, 95, 117, 105, 100, 146, 166, 83, 116, 114,
    105, 110, 103, 194, 192, 147, 163, 117, 105, 100, 146, 166, 83, 116, 114, 105,
    110, 103, 194, 192, 144, 144, 144, 144, 166, 78, 111, 114, 109, 97, 108, 194,
    128, 128, 128, 128, 160, 147, 1, 2, 0, 166, 116, 111, 95, 105, 100, 120,
    146, 157, 171, 101, 100, 103, 101, 58, 116, 111, 95, 105, 100, 120, 9, 146,
    147, 147, 166, 116, 111, 95, 117, 105, 100, 146, 166, 83, 116, 114, 105, 110,
    103, 194, 192, 147, 169, 101, 100, 103, 101, 95, 116, 121, 112, 101, 146, 166,
    83, 116, 114, 105, 110, 103, 194, 192, 147, 163, 117, 105, 100, 146, 166, 83,
    116, 114, 105, 110, 103, 194, 192, 144, 144, 144, 144, 166, 78, 111, 114, 109,
    97, 108, 194, 128, 128, 128, 128, 160, 147, 2, 3, 0, 173, 116, 111, 109,
    98, 115, 116, 111, 110, 101, 95, 105, 100, 120, 146, 157, 178, 101, 100, 103,
    101, 58, 116, 111, 109, 98, 115, 116, 111, 110, 101, 95, 105, 100, 120, 23,
    146, 146, 147, 172, 116, 111, 109, 98, 115, 116, 111, 110, 101, 95, 97, 116,
    146, 165, 70, 108, 111, 97, 116, 194, 129, 165, 67, 111, 110, 115, 116, 145,
    129, 163, 78, 117, 109, 129, 165, 70, 108, 111, 97, 116, 203, 0, 0, 0,
    0, 0, 0, 0, 0, 147, 163, 117, 105, 100, 146, 166, 83, 116, 114, 105,
    110, 103, 194, 192, 144, 144, 144, 144, 166, 78, 111, 114, 109, 97, 108, 194,
    128, 128, 128, 128, 160, 146, 10, 0, 168, 116, 121, 112, 101, 95, 105, 100,
    120, 146, 157, 173, 101, 100, 103, 101, 58, 116, 121, 112, 101, 95, 105, 100,
    120, 10, 146, 146, 147, 169, 101, 100, 103, 101, 95, 116, 121, 112, 101, 146,
    166, 83, 116, 114, 105, 110, 103, 194, 192, 147, 163, 117, 105, 100, 146, 166,
    83, 116, 114, 105, 110, 103, 194, 192, 144, 144, 144, 144, 166, 78, 111, 114,
    109, 97, 108, 194, 128, 128, 128, 128, 160, 146, 3, 0, 128, 128, 128, 160,
    ];

    #[test]
    fn legacy_catalog_without_tt_gc_floor() {
        // The exact failure mode from prod: this MUST decode.
        let handle = RelationHandle::decode(LEGACY_EDGE_CATALOG)
            .expect("legacy 13-field catalog must still decode after the field move");
        assert_eq!(handle.name, "edge");
        assert!(
            handle.tt_gc_floor.is_none(),
            "missing trailing field must default to None"
        );
        // full nested decode really happened (secondary index survived)
        assert!(handle.indices.contains_key("from_idx"));

        // And the current write format (self-describing map via with_struct_map)
        // round-trips too, so both on-disk encodings are readable.
        let mut buf = Vec::new();
        handle
            .serialize(&mut Serializer::new(&mut buf).with_struct_map())
            .unwrap();
        let reread = RelationHandle::decode(&buf).expect("map-encoded catalog must decode");
        assert_eq!(reread.name, "edge");
        assert!(reread.indices.contains_key("from_idx"));

        let mut trailing = buf;
        trailing.push(0xc0);
        assert!(RelationHandle::decode(&trailing).is_err());
        assert!(RelationHandle::decode(&[0xc0]).is_err());
    }
}
