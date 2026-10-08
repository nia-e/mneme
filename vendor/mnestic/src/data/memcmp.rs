/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
*/

use std::cmp::Reverse;
use std::io::{self, Write};

use byteorder::{BigEndian, ByteOrder, WriteBytesExt};
use thiserror::Error;

use crate::data::value::{DataValue, JsonData, Num, UuidWrapper, Validity, ValidityTs, Vector};

const INIT_TAG: u8 = 0x00;
const NULL_TAG: u8 = 0x01;
const FALSE_TAG: u8 = 0x02;
const TRUE_TAG: u8 = 0x03;
const VEC_TAG: u8 = 0x04;
const NUM_TAG: u8 = 0x05;
const STR_TAG: u8 = 0x06;
const BYTES_TAG: u8 = 0x07;
const UUID_TAG: u8 = 0x08;
const REGEX_TAG: u8 = 0x09;
const LIST_TAG: u8 = 0x0A;
const SET_TAG: u8 = 0x0B;
// pub(crate): the bitemporal walk (data/bitemporal.rs) splices validity
// bounds at the byte level (bitemporality step 6)
pub(crate) const VLD_TAG: u8 = 0x0C;
const JSON_TAG: u8 = 0x0D;
const BOT_TAG: u8 = 0xFF;

const REGEX_PERSISTENCE_ERROR: &str = "Regex is internal-only and cannot be persisted";
const SET_PERSISTENCE_ERROR: &str = "Set is internal-only and cannot be persisted";

const VEC_F32: u8 = 0x01;
const VEC_F64: u8 = 0x02;

const IS_FLOAT: u8 = 0b00010000;
const IS_APPROX_INT: u8 = 0b00000100;
const IS_EXACT_INT: u8 = 0b00000000;
const EXACT_INT_BOUND: i64 = 0x20_0000_0000_0000;

pub(crate) trait MemCmpEncoder: Write {
    fn encode_datavalue(&mut self, v: &DataValue) {
        match v {
            DataValue::Null => self.write_u8(NULL_TAG).unwrap(),
            DataValue::Bool(false) => self.write_u8(FALSE_TAG).unwrap(),
            DataValue::Bool(true) => self.write_u8(TRUE_TAG).unwrap(),
            DataValue::Vec(arr) => {
                self.write_u8(VEC_TAG).unwrap();
                match arr {
                    Vector::F32(a) => {
                        self.write_u8(VEC_F32).unwrap();
                        let l = a.len();
                        self.write_u64::<BigEndian>(l as u64).unwrap();
                        for el in a {
                            self.write_f32::<BigEndian>(*el).unwrap();
                        }
                    }
                    Vector::F64(a) => {
                        self.write_u8(VEC_F64).unwrap();
                        let l = a.len();
                        self.write_u64::<BigEndian>(l as u64).unwrap();
                        for el in a {
                            self.write_f64::<BigEndian>(*el).unwrap();
                        }
                    }
                }
            }
            DataValue::Num(n) => {
                self.write_u8(NUM_TAG).unwrap();
                self.encode_num(*n);
            }
            DataValue::Str(s) => {
                self.write_u8(STR_TAG).unwrap();
                self.encode_bytes(s.as_bytes());
            }
            DataValue::Json(j) => {
                let mut encoded = Vec::new();
                serde_json::to_writer(&mut encoded, &j.0).unwrap();
                self.write_u8(JSON_TAG).unwrap();
                self.encode_bytes(&encoded);
            }
            DataValue::Bytes(b) => {
                self.write_u8(BYTES_TAG).unwrap();
                self.encode_bytes(b)
            }
            DataValue::Uuid(u) => {
                self.write_u8(UUID_TAG).unwrap();
                let (s_l, s_m, s_h, s_rest) = u.0.as_fields();
                self.write_u16::<BigEndian>(s_h).unwrap();
                self.write_u16::<BigEndian>(s_m).unwrap();
                self.write_u32::<BigEndian>(s_l).unwrap();
                self.write_all(s_rest.as_ref()).unwrap();
            }
            DataValue::Regex(rx) => {
                self.write_u8(REGEX_TAG).unwrap();
                let s = rx.0.as_str().as_bytes();
                self.encode_bytes(s)
            }
            DataValue::List(l) => {
                self.write_u8(LIST_TAG).unwrap();
                for el in l {
                    self.encode_datavalue(el);
                }
                self.write_u8(INIT_TAG).unwrap()
            }
            DataValue::Set(s) => {
                self.write_u8(SET_TAG).unwrap();
                for el in s {
                    self.encode_datavalue(el);
                }
                self.write_u8(INIT_TAG).unwrap()
            }
            DataValue::Validity(vld) => {
                let ts = vld.timestamp.0 .0;
                let ts_u64 = order_encode_i64(ts);
                let ts_flipped = !ts_u64;
                self.write_u8(VLD_TAG).unwrap();
                self.write_u64::<BigEndian>(ts_flipped).unwrap();
                self.write_u8(!vld.is_assert.0 as u8).unwrap();
            }
            DataValue::Bot => self.write_u8(BOT_TAG).unwrap(),
        }
    }
    fn encode_num(&mut self, v: Num) {
        let f = v.get_float();
        let u = order_encode_f64(f);
        self.write_u64::<BigEndian>(u).unwrap();
        match v {
            Num::Int(i) => {
                if i > -EXACT_INT_BOUND && i < EXACT_INT_BOUND {
                    self.write_u8(IS_EXACT_INT).unwrap();
                } else {
                    self.write_u8(IS_APPROX_INT).unwrap();
                    let en = order_encode_i64(i);
                    self.write_u64::<BigEndian>(en).unwrap();
                }
            }
            Num::Float(_) => {
                self.write_u8(IS_FLOAT).unwrap();
            }
        }
    }

    fn encode_bytes(&mut self, key: &[u8]) {
        let len = key.len();
        let mut index = 0;
        while index <= len {
            let remain = len - index;
            let mut pad: usize = 0;
            if remain > ENC_GROUP_SIZE {
                self.write_all(&key[index..index + ENC_GROUP_SIZE]).unwrap();
            } else {
                pad = ENC_GROUP_SIZE - remain;
                self.write_all(&key[index..]).unwrap();
                self.write_all(&ENC_ASC_PADDING[..pad]).unwrap();
            }
            self.write_all(&[ENC_MARKER - (pad as u8)]).unwrap();
            index += ENC_GROUP_SIZE;
        }
    }
}

/// Decode one canonical memcmp byte string.
pub fn try_decode_bytes(data: &[u8]) -> Result<(Vec<u8>, &[u8]), MemCmpDecodeError> {
    if data.len() > MAX_ENCODED_KEY_BYTES {
        return Err(MemCmpDecodeError::new(format_args!(
            "encoded byte string exceeds {MAX_ENCODED_KEY_BYTES} bytes"
        )));
    }
    let mut key = Vec::with_capacity(data.len() / (ENC_GROUP_SIZE + 1) * ENC_GROUP_SIZE);
    let mut offset: usize = 0;
    let chunk_len = ENC_GROUP_SIZE + 1;
    loop {
        let next_offset = offset
            .checked_add(chunk_len)
            .ok_or_else(|| MemCmpDecodeError::new("encoded byte string chunk offset overflow"))?;
        if next_offset > data.len() {
            return Err(MemCmpDecodeError::new(
                "truncated encoded byte string group or missing terminator",
            ));
        }
        let chunk = &data[offset..next_offset];
        offset = next_offset;

        let marker = chunk[ENC_GROUP_SIZE];
        if marker < ENC_MARKER - ENC_GROUP_SIZE as u8 {
            return Err(MemCmpDecodeError::new(format_args!(
                "invalid encoded byte string marker 0x{marker:02x}"
            )));
        }
        let bytes = &chunk[..ENC_GROUP_SIZE];
        let pad_size = usize::from(ENC_MARKER - marker);

        if pad_size == 0 {
            key.extend_from_slice(bytes);
            continue;
        }

        let (bytes, padding) = bytes.split_at(ENC_GROUP_SIZE - pad_size);
        if padding.iter().any(|byte| *byte != 0) {
            return Err(MemCmpDecodeError::new(
                "non-zero bytes in encoded byte string padding",
            ));
        }
        key.extend_from_slice(bytes);

        return Ok((key, &data[offset..]));
    }
}

const SIGN_MARK: u64 = 0x8000000000000000;

pub(crate) fn order_encode_i64(v: i64) -> u64 {
    v as u64 ^ SIGN_MARK
}

pub(crate) fn order_decode_i64(u: u64) -> i64 {
    (u ^ SIGN_MARK) as i64
}

fn order_encode_f64(v: f64) -> u64 {
    let u = v.to_bits();
    if v.is_sign_positive() {
        u | SIGN_MARK
    } else {
        !u
    }
}

fn order_decode_f64(u: u64) -> f64 {
    let u = if u & SIGN_MARK > 0 {
        u & (!SIGN_MARK)
    } else {
        !u
    };
    f64::from_bits(u)
}

const ENC_GROUP_SIZE: usize = 8;
const ENC_MARKER: u8 = b'\xff';
const ENC_ASC_PADDING: [u8; ENC_GROUP_SIZE] = [0; ENC_GROUP_SIZE];

/// Maximum encoded size of one persisted tuple key, including its relation prefix.
pub(crate) const MAX_ENCODED_KEY_BYTES: usize = 64 * 1024;
/// Maximum number of [`DataValue`]s decoded from one persisted tuple key.
pub(crate) const MAX_KEY_VALUES: usize = 256;
/// Maximum number of nested list/set containers in one persisted tuple key.
pub(crate) const MAX_KEY_NESTING_DEPTH: usize = 16;
/// Maximum number of scalar elements in one memcmp-encoded vector.
pub(crate) const MAX_KEY_VECTOR_ELEMENTS: usize = 4096;
/// Maximum nested JSON containers in a persisted tuple key.
pub(crate) const MAX_KEY_JSON_DEPTH: usize = 64;
/// Maximum JSON values visited while validating one persisted tuple key.
pub(crate) const MAX_KEY_JSON_NODES: usize = MAX_ENCODED_KEY_BYTES;
const MAX_DECODE_REASON_BYTES: usize = 256;

/// Stable policy identity for persisted-key memcmp decoding and canonical
/// re-encoding. Consumers compose this literal into higher-level evidence;
/// the derivation oracle below pins the tags, resource limits, and codec KAT.
pub(crate) const STORED_MEMCMP_KEY_POLICY_FINGERPRINT_V1: [u8; 32] = [
    59, 214, 103, 189, 49, 87, 145, 180, 8, 244, 236, 110, 105, 109, 231, 199, 58, 162, 26, 137,
    213, 51, 78, 141, 122, 234, 150, 140, 47, 154, 40, 207,
];

#[cfg(test)]
const STORED_MEMCMP_KEY_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.stored-memcmp-key.policy-fingerprint-transcript.v1\0";
#[cfg(test)]
const STORED_MEMCMP_KEY_POLICY_DESCRIPTION_V1: &str = concat!(
    "mnestic.stored-memcmp-key-policy.v1\n",
    "decode=one or more tagged DataValue encodings, exact forward progress, caller enforces tuple EOF\n",
    "canonical=each decoded value is immediately re-encoded byte-for-byte; numeric encodings are independently re-encoded\n",
    "persisted=regex,set,bot rejected; UTF-8 strings required; JSON parsed, bounded, and canonically re-serialized by the caller's whole-key comparison\n",
    "bytes=8-byte ascending groups, zero padding, ff-minus-padding marker, mandatory terminator group\n",
    "numbers=order-preserving f64 plus exact/approx-int discriminator; approximate ints retain an order-encoded i64\n",
    "vectors=f32/f64 element bytes are big-endian in memcmp keys\n",
    "validity=reserved i64 extrema rejected and assertion flag is exactly 0 or 1\n",
    "limits=encoded-key 65536; values 256; nesting 16; vector-elements 4096; json-depth 64; json-nodes 65536\n",
    "kat=scalar/container core plus exact-int,approx-int,float,f32-vector,f64-vector,uuid,validity,json persisted branches\n",
);

/// A bounded failure from the memcmp wire decoder.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{reason}")]
pub(crate) struct MemCmpDecodeError {
    reason: String,
}

impl MemCmpDecodeError {
    pub(crate) fn new(reason: impl std::fmt::Display) -> Self {
        let mut reason = reason.to_string();
        if reason.len() > MAX_DECODE_REASON_BYTES {
            let mut end = MAX_DECODE_REASON_BYTES - '…'.len_utf8();
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
            reason.push('…');
        }
        Self { reason }
    }
}

/// Shared allocation and recursion budget for one persisted tuple key.
pub(crate) struct MemCmpDecodeBudget {
    values_left: usize,
}

impl MemCmpDecodeBudget {
    pub(crate) fn for_stored_key() -> Self {
        Self {
            values_left: MAX_KEY_VALUES,
        }
    }

    fn consume_value(&mut self) -> Result<(), MemCmpDecodeError> {
        self.values_left = self.values_left.checked_sub(1).ok_or_else(|| {
            MemCmpDecodeError::new(format_args!(
                "stored key contains more than {MAX_KEY_VALUES} values"
            ))
        })?;
        Ok(())
    }
}

/// Validate the persisted-key structural contract and return its exact encoded
/// payload size (excluding the eight-byte relation prefix).
pub(crate) fn stored_key_values_encoded_len(
    values: &[DataValue],
) -> Result<usize, MemCmpDecodeError> {
    let mut budget = MemCmpDecodeBudget::for_stored_key();
    let mut encoded_len = 0;
    for value in values {
        encoded_len = checked_key_len_add(
            encoded_len,
            stored_key_value_encoded_len(value, &mut budget, 0)?,
        )?;
    }
    Ok(encoded_len)
}

fn stored_key_value_encoded_len(
    value: &DataValue,
    budget: &mut MemCmpDecodeBudget,
    nesting_depth: usize,
) -> Result<usize, MemCmpDecodeError> {
    budget.consume_value()?;
    match value {
        DataValue::Null | DataValue::Bool(_) => Ok(1),
        DataValue::Bot => Err(MemCmpDecodeError::new(
            "Bot is reserved for internal key bounds and cannot be persisted",
        )),
        DataValue::Num(Num::Int(value))
            if *value <= -EXACT_INT_BOUND || *value >= EXACT_INT_BOUND =>
        {
            Ok(18)
        }
        DataValue::Num(_) => Ok(10),
        DataValue::Str(value) => encoded_key_bytes_len(value.len()),
        DataValue::Json(value) => encoded_key_bytes_len(stored_key_json_serialized_len(&value.0)?),
        DataValue::Bytes(value) => encoded_key_bytes_len(value.len()),
        DataValue::Uuid(_) => Ok(17),
        DataValue::Regex(_) => Err(MemCmpDecodeError::new(REGEX_PERSISTENCE_ERROR)),
        DataValue::List(values) => {
            stored_key_container_encoded_len(values.iter(), budget, nesting_depth)
        }
        DataValue::Set(_) => Err(MemCmpDecodeError::new(SET_PERSISTENCE_ERROR)),
        DataValue::Validity(value) => {
            validate_stored_validity_timestamp(value.timestamp.0 .0)?;
            Ok(10)
        }
        DataValue::Vec(vector) => {
            let (len, width) = match vector {
                Vector::F32(values) => (values.len(), std::mem::size_of::<f32>()),
                Vector::F64(values) => (values.len(), std::mem::size_of::<f64>()),
            };
            if len > MAX_KEY_VECTOR_ELEMENTS {
                return Err(MemCmpDecodeError::new(format_args!(
                    "vector contains {len} elements; limit is {MAX_KEY_VECTOR_ELEMENTS}"
                )));
            }
            let payload_len = len
                .checked_mul(width)
                .ok_or_else(|| MemCmpDecodeError::new("vector byte length overflow"))?;
            checked_key_len_add(10, payload_len)
        }
    }
}

fn validate_stored_json(value: &serde_json::Value) -> Result<(), MemCmpDecodeError> {
    let mut stack = vec![(value, 0_usize)];
    let mut visited = 0_usize;

    while let Some((value, depth)) = stack.pop() {
        visited = visited
            .checked_add(1)
            .ok_or_else(|| MemCmpDecodeError::new("JSON node count overflow"))?;
        if visited > MAX_KEY_JSON_NODES {
            return Err(MemCmpDecodeError::new(format_args!(
                "JSON contains more than {MAX_KEY_JSON_NODES} values"
            )));
        }

        match value {
            serde_json::Value::Array(values) => {
                push_stored_json_children(&mut stack, values.iter(), visited, depth)?;
            }
            serde_json::Value::Object(values) => {
                push_stored_json_children(&mut stack, values.values(), visited, depth)?;
            }
            _ => {}
        }
    }

    Ok(())
}

fn push_stored_json_children<'a>(
    stack: &mut Vec<(&'a serde_json::Value, usize)>,
    children: impl ExactSizeIterator<Item = &'a serde_json::Value>,
    visited: usize,
    depth: usize,
) -> Result<(), MemCmpDecodeError> {
    if depth >= MAX_KEY_JSON_DEPTH {
        return Err(MemCmpDecodeError::new(format_args!(
            "JSON nesting exceeds {MAX_KEY_JSON_DEPTH} containers"
        )));
    }
    let child_count = children.len();
    let projected = visited
        .checked_add(stack.len())
        .and_then(|count| count.checked_add(child_count))
        .ok_or_else(|| MemCmpDecodeError::new("JSON node count overflow"))?;
    if projected > MAX_KEY_JSON_NODES {
        return Err(MemCmpDecodeError::new(format_args!(
            "JSON contains more than {MAX_KEY_JSON_NODES} values"
        )));
    }
    stack
        .try_reserve(child_count)
        .map_err(|_| MemCmpDecodeError::new("cannot reserve JSON validation stack"))?;
    stack.extend(children.map(|child| (child, depth + 1)));
    Ok(())
}

struct BoundedJsonCounter {
    bytes: usize,
    exceeded: bool,
}

impl Write for BoundedJsonCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next) = self.bytes.checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSON serialization length overflow",
            ));
        };
        if next > MAX_ENCODED_KEY_BYTES {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSON serialization work limit exceeded",
            ));
        }
        self.bytes = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn stored_key_json_serialized_len(value: &serde_json::Value) -> Result<usize, MemCmpDecodeError> {
    validate_stored_json(value)?;
    let mut counter = BoundedJsonCounter {
        bytes: 0,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut counter, value) {
        if counter.exceeded {
            return Err(MemCmpDecodeError::new(format_args!(
                "JSON payload exceeds {MAX_ENCODED_KEY_BYTES}-byte serialization work limit"
            )));
        }
        return Err(MemCmpDecodeError::new(format_args!(
            "cannot serialize JSON payload: {error}"
        )));
    }
    Ok(counter.bytes)
}

fn stored_key_container_encoded_len<'a>(
    values: impl Iterator<Item = &'a DataValue>,
    budget: &mut MemCmpDecodeBudget,
    nesting_depth: usize,
) -> Result<usize, MemCmpDecodeError> {
    if nesting_depth >= MAX_KEY_NESTING_DEPTH {
        return Err(MemCmpDecodeError::new(format_args!(
            "stored key nesting exceeds {MAX_KEY_NESTING_DEPTH} containers"
        )));
    }
    let mut encoded_len = 2; // container tag plus terminator
    for value in values {
        encoded_len = checked_key_len_add(
            encoded_len,
            stored_key_value_encoded_len(value, budget, nesting_depth + 1)?,
        )?;
    }
    Ok(encoded_len)
}

fn encoded_key_bytes_len(len: usize) -> Result<usize, MemCmpDecodeError> {
    let groups = len
        .checked_div(ENC_GROUP_SIZE)
        .and_then(|groups| groups.checked_add(1))
        .ok_or_else(|| MemCmpDecodeError::new("encoded byte string length overflow"))?;
    let payload_len = groups
        .checked_mul(ENC_GROUP_SIZE + 1)
        .ok_or_else(|| MemCmpDecodeError::new("encoded byte string length overflow"))?;
    checked_key_len_add(1, payload_len)
}

fn checked_key_len_add(left: usize, right: usize) -> Result<usize, MemCmpDecodeError> {
    left.checked_add(right)
        .ok_or_else(|| MemCmpDecodeError::new("encoded stored-key length overflow"))
}

fn validate_stored_validity_timestamp(timestamp: i64) -> Result<(), MemCmpDecodeError> {
    if timestamp == i64::MIN || timestamp == i64::MAX {
        Err(MemCmpDecodeError::new(format_args!(
            "validity timestamp {timestamp} is a reserved engine sentinel"
        )))
    } else {
        Ok(())
    }
}

impl Num {
    pub(crate) fn try_decode_from_key(bs: &[u8]) -> Result<(Self, &[u8]), MemCmpDecodeError> {
        let float_part = bs
            .get(..8)
            .ok_or_else(|| MemCmpDecodeError::new("truncated numeric float component"))?;
        let remaining = &bs[8..];
        let fu = BigEndian::read_u64(float_part);
        let f = order_decode_f64(fu);
        let (tag, remaining) = remaining
            .split_first()
            .ok_or_else(|| MemCmpDecodeError::new("missing numeric representation subtag"))?;
        let decoded = match *tag {
            IS_FLOAT => (Num::Float(f), remaining),
            IS_EXACT_INT => (Num::Int(f as i64), remaining),
            IS_APPROX_INT => {
                let int_part = remaining
                    .get(..8)
                    .ok_or_else(|| MemCmpDecodeError::new("truncated approximate integer"))?;
                let remaining = &remaining[8..];
                let iu = BigEndian::read_u64(int_part);
                let i = order_decode_i64(iu);
                (Num::Int(i), remaining)
            }
            _ => {
                return Err(MemCmpDecodeError::new(format_args!(
                    "invalid numeric representation subtag 0x{tag:02x}"
                )))
            }
        };

        let consumed = bs.len() - decoded.1.len();
        let mut canonical = Vec::with_capacity(consumed);
        canonical.encode_num(decoded.0);
        if canonical.as_slice() != &bs[..consumed] {
            return Err(MemCmpDecodeError::new(
                "noncanonical or inconsistent numeric encoding",
            ));
        }
        Ok(decoded)
    }
}

impl DataValue {
    pub(crate) fn try_decode_from_key<'a>(
        bs: &'a [u8],
        budget: &mut MemCmpDecodeBudget,
        nesting_depth: usize,
    ) -> Result<(Self, &'a [u8]), MemCmpDecodeError> {
        let (value, remaining) = Self::try_decode_from_key_inner(bs, budget, nesting_depth)?;
        let consumed = bs.len() - remaining.len();
        if consumed == 0 {
            return Err(MemCmpDecodeError::new(
                "memcmp value decoder made no forward progress",
            ));
        }
        let mut canonical = Vec::with_capacity(consumed);
        canonical.encode_datavalue(&value);
        if canonical.as_slice() != &bs[..consumed] {
            return Err(MemCmpDecodeError::new("noncanonical memcmp value encoding"));
        }
        Ok((value, remaining))
    }

    fn try_decode_from_key_inner<'a>(
        bs: &'a [u8],
        budget: &mut MemCmpDecodeBudget,
        nesting_depth: usize,
    ) -> Result<(Self, &'a [u8]), MemCmpDecodeError> {
        let (tag, remaining) = bs
            .split_first()
            .ok_or_else(|| MemCmpDecodeError::new("missing memcmp value tag"))?;
        budget.consume_value()?;
        let decoded = match *tag {
            NULL_TAG => (DataValue::Null, remaining),
            FALSE_TAG => (DataValue::from(false), remaining),
            TRUE_TAG => (DataValue::from(true), remaining),
            NUM_TAG => {
                let (n, remaining) = Num::try_decode_from_key(remaining)?;
                (DataValue::Num(n), remaining)
            }
            STR_TAG => {
                let (bytes, remaining) = try_decode_bytes(remaining)?;
                let s = String::from_utf8(bytes)
                    .map_err(|_| MemCmpDecodeError::new("invalid UTF-8 string payload"))?;
                (DataValue::Str(s.into()), remaining)
            }
            JSON_TAG => {
                let (bytes, remaining) = try_decode_bytes(remaining)?;
                let json = serde_json::from_slice(&bytes).map_err(|error| {
                    MemCmpDecodeError::new(format_args!(
                        "invalid JSON payload at line {}, column {}",
                        error.line(),
                        error.column()
                    ))
                })?;
                validate_stored_json(&json)?;
                (DataValue::Json(JsonData(json)), remaining)
            }
            BYTES_TAG => {
                let (bytes, remaining) = try_decode_bytes(remaining)?;
                (DataValue::Bytes(bytes), remaining)
            }
            UUID_TAG => {
                let uuid_data = remaining
                    .get(..16)
                    .ok_or_else(|| MemCmpDecodeError::new("truncated UUID payload"))?;
                let remaining = &remaining[16..];
                let s_h = BigEndian::read_u16(&uuid_data[0..2]);
                let s_m = BigEndian::read_u16(&uuid_data[2..4]);
                let s_l = BigEndian::read_u32(&uuid_data[4..8]);
                let mut s_rest = [0u8; 8];
                s_rest.copy_from_slice(&uuid_data[8..]);
                let uuid = uuid::Uuid::from_fields(s_l, s_m, s_h, &s_rest);
                (DataValue::Uuid(UuidWrapper(uuid)), remaining)
            }
            REGEX_TAG => return Err(MemCmpDecodeError::new(REGEX_PERSISTENCE_ERROR)),
            LIST_TAG => {
                if nesting_depth >= MAX_KEY_NESTING_DEPTH {
                    return Err(MemCmpDecodeError::new(format_args!(
                        "stored key nesting exceeds {MAX_KEY_NESTING_DEPTH} containers"
                    )));
                }
                let mut collected = vec![];
                let mut remaining = remaining;
                loop {
                    let next_tag = remaining.first().ok_or_else(|| {
                        MemCmpDecodeError::new("unterminated memcmp list container")
                    })?;
                    if *next_tag == INIT_TAG {
                        remaining = &remaining[1..];
                        break;
                    }
                    let before = remaining.len();
                    let (val, next_chunk) =
                        Self::try_decode_from_key_inner(remaining, budget, nesting_depth + 1)?;
                    if next_chunk.len() >= before {
                        return Err(MemCmpDecodeError::new(
                            "memcmp list element made no forward progress",
                        ));
                    }
                    remaining = next_chunk;
                    collected.push(val);
                }
                (DataValue::List(collected), remaining)
            }
            SET_TAG => return Err(MemCmpDecodeError::new(SET_PERSISTENCE_ERROR)),
            VLD_TAG => {
                let ts_flipped_bytes = remaining
                    .get(..8)
                    .ok_or_else(|| MemCmpDecodeError::new("truncated validity timestamp"))?;
                let rest = &remaining[8..];
                let ts_flipped = BigEndian::read_u64(ts_flipped_bytes);
                let ts_u64 = !ts_flipped;
                let ts = order_decode_i64(ts_u64);
                validate_stored_validity_timestamp(ts)?;
                let (is_assert_byte, rest) = rest
                    .split_first()
                    .ok_or_else(|| MemCmpDecodeError::new("missing validity assertion flag"))?;
                let is_assert = match *is_assert_byte {
                    0 => true,
                    1 => false,
                    flag => {
                        return Err(MemCmpDecodeError::new(format_args!(
                            "invalid validity assertion flag 0x{flag:02x}"
                        )))
                    }
                };
                (
                    DataValue::Validity(Validity {
                        timestamp: ValidityTs(Reverse(ts)),
                        is_assert: Reverse(is_assert),
                    }),
                    rest,
                )
            }
            BOT_TAG => {
                return Err(MemCmpDecodeError::new(
                    "Bot is reserved for internal key bounds and cannot be persisted",
                ))
            }
            VEC_TAG => {
                let (t_tag, remaining) = remaining
                    .split_first()
                    .ok_or_else(|| MemCmpDecodeError::new("missing vector element subtag"))?;
                let len_bytes = remaining
                    .get(..8)
                    .ok_or_else(|| MemCmpDecodeError::new("truncated vector length"))?;
                let rest = &remaining[8..];
                let len_u64 = BigEndian::read_u64(len_bytes);
                let len = usize::try_from(len_u64)
                    .map_err(|_| MemCmpDecodeError::new("vector length does not fit usize"))?;
                if len > MAX_KEY_VECTOR_ELEMENTS {
                    return Err(MemCmpDecodeError::new(format_args!(
                        "vector contains {len} elements; limit is {MAX_KEY_VECTOR_ELEMENTS}"
                    )));
                }
                let width = match *t_tag {
                    VEC_F32 => std::mem::size_of::<f32>(),
                    VEC_F64 => std::mem::size_of::<f64>(),
                    tag => {
                        return Err(MemCmpDecodeError::new(format_args!(
                            "invalid vector element subtag 0x{tag:02x}"
                        )))
                    }
                };
                let byte_len = len
                    .checked_mul(width)
                    .ok_or_else(|| MemCmpDecodeError::new("vector byte length overflow"))?;
                let payload = rest
                    .get(..byte_len)
                    .ok_or_else(|| MemCmpDecodeError::new("truncated vector element payload"))?;
                let remaining = &rest[byte_len..];
                match *t_tag {
                    VEC_F32 => {
                        let values = payload.chunks_exact(width).map(BigEndian::read_f32);
                        let array = ndarray::Array1::from_iter(values);
                        (DataValue::Vec(Vector::F32(array)), remaining)
                    }
                    VEC_F64 => {
                        let values = payload.chunks_exact(width).map(BigEndian::read_f64);
                        let array = ndarray::Array1::from_iter(values);
                        (DataValue::Vec(Vector::F64(array)), remaining)
                    }
                    _ => unreachable!("vector subtag validated above"),
                }
            }
            tag => {
                return Err(MemCmpDecodeError::new(format_args!(
                    "unknown memcmp value tag 0x{tag:02x}"
                )))
            }
        };
        Ok(decoded)
    }
}

impl<T: Write> MemCmpEncoder for T {}

#[cfg(test)]
mod policy_fingerprint_tests {
    use std::cmp::Reverse;

    use ndarray::Array1;
    use sha2::{Digest, Sha256};

    use super::*;

    const MEMCMP_CODEC_KAT_V1: &[u8] = &[
        0x01, 0x02, 0x03, 0x06, 0x41, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf8, 0x07, 0x00,
        0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf9, 0x0a, 0x01, 0x03, 0x00,
    ];
    const MEMCMP_PERSISTED_BRANCH_KAT_V1: &[u8] = &[
        5, 192, 28, 0, 0, 0, 0, 0, 0, 0, 5, 195, 64, 0, 0, 0, 0, 0, 0, 4, 128, 32, 0, 0, 0, 0, 0,
        0, 5, 64, 7, 255, 255, 255, 255, 255, 255, 16, 4, 1, 0, 0, 0, 0, 0, 0, 0, 2, 63, 128, 0, 0,
        192, 32, 0, 0, 4, 2, 0, 0, 0, 0, 0, 0, 0, 2, 63, 240, 0, 0, 0, 0, 0, 0, 192, 4, 0, 0, 0, 0,
        0, 0, 8, 102, 119, 68, 85, 0, 17, 34, 51, 136, 153, 170, 187, 204, 221, 238, 255, 12, 127,
        255, 255, 255, 255, 255, 255, 248, 0, 13, 123, 34, 107, 34, 58, 49, 125, 0, 254,
    ];

    fn update_field(hasher: &mut Sha256, field_count: &mut u64, label: &[u8], value: &[u8]) {
        *field_count = field_count.checked_add(1).unwrap();
        hasher.update([0x01]);
        hasher.update(u64::try_from(label.len()).unwrap().to_be_bytes());
        hasher.update(label);
        hasher.update(u64::try_from(value.len()).unwrap().to_be_bytes());
        hasher.update(value);
    }

    fn derive_stored_memcmp_key_policy_fingerprint_v1() -> [u8; 32] {
        let values = vec![
            DataValue::Null,
            DataValue::Bool(false),
            DataValue::Bool(true),
            DataValue::Str("A".into()),
            DataValue::Bytes(vec![0x00, 0xff]),
            DataValue::List(vec![DataValue::Null, DataValue::Bool(true)]),
        ];
        let mut encoded = Vec::new();
        for value in &values {
            encoded.encode_datavalue(value);
        }
        assert_eq!(encoded, MEMCMP_CODEC_KAT_V1);

        let mut decoded = Vec::new();
        let mut remaining = encoded.as_slice();
        let mut budget = MemCmpDecodeBudget::for_stored_key();
        while !remaining.is_empty() {
            let (value, next) = DataValue::try_decode_from_key(remaining, &mut budget, 0).unwrap();
            decoded.push(value);
            remaining = next;
        }
        assert_eq!(decoded, values);

        let persisted_branches = vec![
            DataValue::Num(Num::Int(7)),
            DataValue::Num(Num::Int(EXACT_INT_BOUND)),
            DataValue::Num(Num::Float(-1.5)),
            DataValue::Vec(Vector::F32(Array1::from_vec(vec![1.0, -2.5]))),
            DataValue::Vec(Vector::F64(Array1::from_vec(vec![1.0, -2.5]))),
            DataValue::Uuid(UuidWrapper(
                uuid::Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap(),
            )),
            DataValue::Validity(Validity {
                timestamp: ValidityTs(Reverse(7)),
                is_assert: Reverse(true),
            }),
            DataValue::Json(JsonData(serde_json::json!({"k": 1}))),
        ];
        let mut encoded_branches = Vec::new();
        for value in &persisted_branches {
            encoded_branches.encode_datavalue(value);
        }
        assert_eq!(encoded_branches, MEMCMP_PERSISTED_BRANCH_KAT_V1);
        assert_eq!(
            stored_key_values_encoded_len(&persisted_branches).unwrap(),
            encoded_branches.len()
        );

        let mut decoded_branches = Vec::new();
        let mut remaining = encoded_branches.as_slice();
        let mut budget = MemCmpDecodeBudget::for_stored_key();
        while !remaining.is_empty() {
            let (value, next) = DataValue::try_decode_from_key(remaining, &mut budget, 0).unwrap();
            decoded_branches.push(value);
            remaining = next;
        }
        assert_eq!(decoded_branches, persisted_branches);

        let mut hasher = Sha256::new();
        hasher.update(STORED_MEMCMP_KEY_POLICY_FINGERPRINT_DOMAIN_V1);
        let mut field_count = 0_u64;
        for (label, value) in [
            (
                b"policy-description".as_slice(),
                STORED_MEMCMP_KEY_POLICY_DESCRIPTION_V1.as_bytes(),
            ),
            (
                b"value-tags".as_slice(),
                [
                    INIT_TAG, NULL_TAG, FALSE_TAG, TRUE_TAG, VEC_TAG, NUM_TAG, STR_TAG, BYTES_TAG,
                    UUID_TAG, REGEX_TAG, LIST_TAG, SET_TAG, VLD_TAG, JSON_TAG, BOT_TAG,
                ]
                .as_slice(),
            ),
            (b"vector-tags".as_slice(), [VEC_F32, VEC_F64].as_slice()),
            (
                b"numeric-tags".as_slice(),
                [IS_FLOAT, IS_APPROX_INT, IS_EXACT_INT].as_slice(),
            ),
            (b"codec-kat".as_slice(), MEMCMP_CODEC_KAT_V1),
            (
                b"persisted-branch-kat".as_slice(),
                MEMCMP_PERSISTED_BRANCH_KAT_V1,
            ),
        ] {
            update_field(&mut hasher, &mut field_count, label, value);
        }
        for (label, value) in [
            (b"exact-int-bound".as_slice(), EXACT_INT_BOUND as u64),
            (b"encoded-group-size".as_slice(), ENC_GROUP_SIZE as u64),
            (b"encoded-marker".as_slice(), u64::from(ENC_MARKER)),
            (
                b"encoded-key-limit".as_slice(),
                MAX_ENCODED_KEY_BYTES as u64,
            ),
            (b"key-value-limit".as_slice(), MAX_KEY_VALUES as u64),
            (
                b"key-nesting-limit".as_slice(),
                MAX_KEY_NESTING_DEPTH as u64,
            ),
            (
                b"key-vector-element-limit".as_slice(),
                MAX_KEY_VECTOR_ELEMENTS as u64,
            ),
            (
                b"key-json-depth-limit".as_slice(),
                MAX_KEY_JSON_DEPTH as u64,
            ),
            (b"key-json-node-limit".as_slice(), MAX_KEY_JSON_NODES as u64),
        ] {
            update_field(&mut hasher, &mut field_count, label, &value.to_be_bytes());
        }
        update_field(
            &mut hasher,
            &mut field_count,
            b"ascending-padding",
            &ENC_ASC_PADDING,
        );
        hasher.update([0xff]);
        hasher.update(field_count.to_be_bytes());
        hasher.finalize().into()
    }

    #[test]
    fn stored_memcmp_key_policy_fingerprint_is_a_hard_pinned_literal_oracle() {
        assert_eq!(
            derive_stored_memcmp_key_policy_fingerprint_v1(),
            STORED_MEMCMP_KEY_POLICY_FINGERPRINT_V1
        );
        assert_ne!(STORED_MEMCMP_KEY_POLICY_FINGERPRINT_V1, [0; 32]);
    }
}
