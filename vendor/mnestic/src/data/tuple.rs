/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use crate::data::functions::TERMINAL_VALIDITY;
use miette::{Diagnostic, Result};
use std::cmp::Reverse;
use std::fmt::Write as _;
use thiserror::Error;

use crate::data::memcmp::{
    MemCmpDecodeBudget, MemCmpEncoder, MAX_ENCODED_KEY_BYTES, MAX_KEY_VALUES,
};
use crate::data::value::{DataValue, Validity, ValidityTs};
use crate::runtime::relation::RelationId;

pub type Tuple = Vec<DataValue>;

pub(crate) type TupleIter<'a> = Box<dyn Iterator<Item = Result<Tuple>> + 'a>;

pub(crate) trait TupleT {
    fn encode_as_key(&self, prefix: RelationId) -> Vec<u8>;
}

impl<T> TupleT for T
where
    T: AsRef<[DataValue]>,
{
    fn encode_as_key(&self, prefix: RelationId) -> Vec<u8> {
        let len = self.as_ref().len();
        let mut ret = Vec::with_capacity(4 + 4 * len + 10 * len);
        let prefix_bytes = prefix.0.to_be_bytes();
        ret.extend(prefix_bytes);
        for val in self.as_ref().iter() {
            ret.encode_datavalue(val);
        }
        ret
    }
}

const KEY_DIAGNOSTIC_PREFIX_BYTES: usize = 64;
const KEY_DIAGNOSTIC_REASON_BYTES: usize = 256;

#[derive(Debug, Diagnostic, Error)]
#[error("corrupt stored key {key}: {reason}")]
#[diagnostic(
    code(eval::corrupt_stored_key),
    help("restore from a trusted backup or remove the corrupt row through the repair boundary")
)]
pub(crate) struct CorruptStoredKey {
    key: String,
    reason: String,
}

impl CorruptStoredKey {
    pub(crate) fn new(key: &[u8], reason: impl std::fmt::Display) -> Self {
        let mut reason = reason.to_string();
        if reason.len() > KEY_DIAGNOSTIC_REASON_BYTES {
            let mut end = KEY_DIAGNOSTIC_REASON_BYTES - '…'.len_utf8();
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
            reason.push('…');
        }
        Self {
            key: bounded_key_label(key),
            reason,
        }
    }
}

pub(crate) fn bounded_key_label(key: &[u8]) -> String {
    let shown = key.len().min(KEY_DIAGNOSTIC_PREFIX_BYTES);
    let mut label = String::with_capacity(shown * 2 + 24);
    for byte in &key[..shown] {
        write!(label, "{byte:02x}").expect("writing to a String cannot fail");
    }
    if key.len() > shown {
        write!(label, "…(+{} bytes)", key.len() - shown).expect("writing to a String cannot fail");
    }
    label
}

/// Decode one complete persisted tuple key under the stored-key resource limits.
pub fn try_decode_tuple_from_key(key: &[u8], size_hint: usize) -> Result<Tuple> {
    if key.len() > MAX_ENCODED_KEY_BYTES {
        return Err(CorruptStoredKey::new(
            key,
            format_args!(
                "encoded key is {} bytes; limit is {MAX_ENCODED_KEY_BYTES}",
                key.len()
            ),
        )
        .into());
    }
    let mut remaining = key.get(ENCODED_KEY_MIN_LEN..).ok_or_else(|| {
        CorruptStoredKey::new(
            key,
            format_args!(
                "encoded key is {} bytes; expected an {ENCODED_KEY_MIN_LEN}-byte relation prefix",
                key.len()
            ),
        )
    })?;
    RelationId::try_raw_decode_prefix(key).map_err(|error| CorruptStoredKey::new(key, error))?;
    let mut ret = Vec::with_capacity(size_hint.min(MAX_KEY_VALUES));
    let mut budget = MemCmpDecodeBudget::for_stored_key();
    while !remaining.is_empty() {
        let before = remaining.len();
        let (val, next) = DataValue::try_decode_from_key(remaining, &mut budget, 0)
            .map_err(|error| CorruptStoredKey::new(key, error))?;
        if next.len() >= before {
            return Err(CorruptStoredKey::new(
                key,
                "memcmp tuple decoder made no forward progress",
            )
            .into());
        }
        ret.push(val);
        remaining = next;
    }
    Ok(ret)
}

pub(crate) const DEFAULT_SIZE_HINT: usize = 16;

/// Check if the tuple key passed in should be a valid return for a validity query.
///
/// Returns two elements, the first element contains `Some(tuple)` if the key should be included
/// in the return set and `None` otherwise,
/// the second element gives the next binary key for the seek to be used as an inclusive
/// lower bound.
pub fn check_key_for_validity(
    key: &[u8],
    valid_at: ValidityTs,
    size_hint: Option<usize>,
) -> Result<(Option<Tuple>, Vec<u8>)> {
    let mut decoded = try_decode_tuple_from_key(key, size_hint.unwrap_or(DEFAULT_SIZE_HINT))?;
    let rel_id = RelationId::try_raw_decode_prefix(key)?;
    let vld = match decoded.last() {
        Some(DataValue::Validity(vld)) => *vld,
        Some(_) => {
            return Err(CorruptStoredKey::new(
                key,
                "validity scan key does not end in a validity value",
            )
            .into())
        }
        None => {
            return Err(CorruptStoredKey::new(
                key,
                "validity scan key has no values after its relation prefix",
            )
            .into())
        }
    };
    let (ret, nxt_seek) = if vld.timestamp < valid_at {
        *decoded.last_mut().expect("validity tail was checked above") =
            DataValue::Validity(Validity {
                timestamp: valid_at,
                is_assert: Reverse(true),
            });
        let nxt_seek = decoded.encode_as_key(rel_id);
        (None, nxt_seek)
    } else if !vld.is_assert.0 {
        *decoded.last_mut().expect("validity tail was checked above") =
            DataValue::Validity(TERMINAL_VALIDITY);
        let nxt_seek = decoded.encode_as_key(rel_id);
        (None, nxt_seek)
    } else {
        let ret = decoded.clone();
        *decoded.last_mut().expect("validity tail was checked above") =
            DataValue::Validity(TERMINAL_VALIDITY);
        let nxt_seek = decoded.encode_as_key(rel_id);
        (Some(ret), nxt_seek)
    };
    if nxt_seek.as_slice() <= key {
        return Err(CorruptStoredKey::new(
            key,
            "validity scan did not produce a strictly increasing seek key",
        )
        .into());
    }
    Ok((ret, nxt_seek))
}

pub(crate) const ENCODED_KEY_MIN_LEN: usize = 8;

#[cfg(test)]
mod decode_error_tests {
    use std::cmp::Reverse;

    use super::{check_key_for_validity, CorruptStoredKey, TupleT, KEY_DIAGNOSTIC_REASON_BYTES};
    use crate::data::functions::{MAX_VALIDITY_TS, TERMINAL_VALIDITY};
    use crate::data::value::{DataValue, Validity, ValidityTs};
    use crate::runtime::relation::RelationId;

    #[test]
    fn corrupt_key_reasons_are_bounded_in_bytes() {
        let error = CorruptStoredKey::new(&[], "é".repeat(300));
        assert!(error.reason.len() <= KEY_DIAGNOSTIC_REASON_BYTES);
        assert!(error.reason.ends_with('…'));
    }

    #[test]
    fn persisted_validity_sentinels_are_rejected_before_seek() {
        for validity in [
            TERMINAL_VALIDITY,
            Validity {
                timestamp: MAX_VALIDITY_TS,
                is_assert: Reverse(true),
            },
        ] {
            let key = vec![DataValue::Validity(validity)].encode_as_key(RelationId::new(1));
            let error = check_key_for_validity(&key, ValidityTs(Reverse(0)), None).unwrap_err();
            assert!(error.to_string().contains("reserved engine sentinel"));
        }
    }
}
