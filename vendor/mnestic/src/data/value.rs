/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
*/

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ndarray::Array1;
use std::cmp::{Ordering, Reverse};
use std::collections::BTreeSet;
use std::fmt::{Debug, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::ops::Deref;

use crate::data::json::JsonValue;
use crate::data::relation::VecElementType;
use ordered_float::OrderedFloat;
use regex::Regex;
use serde::de::{SeqAccess, Visitor};
use serde::ser::SerializeTuple;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::digest::FixedOutput;
use sha2::{Digest, Sha256};
use smartstring::{LazyCompact, SmartString};
use uuid::Uuid;

/// UUID value in the database
#[derive(Clone, Hash, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
pub struct UuidWrapper(pub Uuid);

impl PartialOrd<Self> for UuidWrapper {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for UuidWrapper {
    fn cmp(&self, other: &Self) -> Ordering {
        let (s_l, s_m, s_h, s_rest) = self.0.as_fields();
        let (o_l, o_m, o_h, o_rest) = other.0.as_fields();
        s_h.cmp(&o_h)
            .then_with(|| s_m.cmp(&o_m))
            .then_with(|| s_l.cmp(&o_l))
            .then_with(|| s_rest.cmp(o_rest))
    }
}

/// A Regex in the database. Used internally in functions.
#[derive(Clone)]
pub struct RegexWrapper(pub Regex);

impl Hash for RegexWrapper {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.as_str().hash(state)
    }
}

impl Serialize for RegexWrapper {
    fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom(
            "regex values are internal-only and cannot be serialized",
        ))
    }
}

impl<'de> Deserialize<'de> for RegexWrapper {
    fn deserialize<D>(_deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Err(serde::de::Error::custom(
            "regex values are internal-only and cannot be deserialized",
        ))
    }
}

impl PartialEq for RegexWrapper {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl Eq for RegexWrapper {}

impl Ord for RegexWrapper {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.as_str().cmp(other.0.as_str())
    }
}

impl PartialOrd for RegexWrapper {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Timestamp part of validity
#[derive(
    Copy,
    Clone,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    serde_derive::Deserialize,
    serde_derive::Serialize,
    Hash,
    Debug,
)]
pub struct ValidityTs(pub Reverse<i64>);

/// Validity for time travel
#[derive(
    Copy,
    Clone,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    serde_derive::Deserialize,
    serde_derive::Serialize,
    Hash,
)]
pub struct Validity {
    /// Timestamp, sorted descendingly
    pub timestamp: ValidityTs,
    /// Whether this validity is an assertion, sorted descendingly
    pub is_assert: Reverse<bool>,
}

impl From<(i64, bool)> for Validity {
    fn from(value: (i64, bool)) -> Self {
        Self {
            timestamp: ValidityTs(Reverse(value.0)),
            is_assert: Reverse(value.1),
        }
    }
}

/// A Value in the database
#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, serde_derive::Deserialize, serde_derive::Serialize, Hash,
)]
pub enum DataValue {
    /// null
    Null,
    /// boolean
    Bool(bool),
    /// number, may be int or float
    Num(Num),
    /// string
    Str(SmartString<LazyCompact>),
    /// bytes
    #[serde(with = "serde_bytes")]
    Bytes(Vec<u8>),
    /// UUID
    Uuid(UuidWrapper),
    /// Regex, used internally only
    Regex(RegexWrapper),
    /// list
    List(Vec<DataValue>),
    /// set, used internally only
    Set(BTreeSet<DataValue>),
    /// Array, mainly for proximity search
    Vec(Vector),
    /// Json
    Json(JsonData),
    /// validity,
    Validity(Validity),
    /// bottom type, used internally only
    Bot,
}

/// Stable identity for the serde DataValue ABI used inside persisted row
/// MessagePack arrays. Vector payloads intentionally retain native byte order;
/// a higher-level boundary must bind or restrict target endianness.
pub(crate) const STORED_ROW_DATAVALUE_CODEC_POLICY_FINGERPRINT_V1: [u8; 32] = [
    228, 218, 49, 135, 127, 222, 184, 0, 183, 0, 140, 76, 232, 49, 242, 131, 173, 36, 235, 13, 11,
    160, 70, 231, 237, 241, 64, 171, 203, 5, 10, 21,
];

#[cfg(test)]
const STORED_ROW_DATAVALUE_CODEC_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.stored-row-datavalue-codec.policy-fingerprint-transcript.v1\0";
#[cfg(test)]
const STORED_ROW_DATAVALUE_CODEC_POLICY_DESCRIPTION_V1: &str = concat!(
    "mnestic.stored-row-datavalue-codec-policy.v1\n",
    "wire=serde-derived externally tagged DataValue enum with exact variant names Null,Bool,Num,Str,Bytes,Uuid,Regex,List,Set,Vec,Json,Validity,Bot\n",
    "regex=internal-only serializer and deserializer reject\n",
    "bytes=serde_bytes MessagePack byte string\n",
    "vector=tuple(tag 0=f32 or 1=f64, byte-string); payload is contiguous host-native float bytes and length must be an exact element multiple\n",
    "json=JsonValue serde representation; validity=Reverse<i64>,Reverse<bool> newtypes\n",
    "scope=syntactic typed decode only; no row arity, column type, canonical MessagePack, or F7 semantic claim\n",
    "kat=non-vector variants plus f32/f64 vector payloads in fixed little-endian and big-endian forms\n",
);

/// Wrapper for JsonValue
#[derive(Clone, PartialEq, Eq, serde_derive::Deserialize, serde_derive::Serialize)]
pub struct JsonData(pub JsonValue);

impl PartialOrd<Self> for JsonData {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JsonData {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.to_string().cmp(&other.0.to_string())
    }
}

impl Deref for JsonData {
    type Target = JsonValue;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Hash for JsonData {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_string().hash(state)
    }
}

/// Vector of floating numbers
#[derive(Debug, Clone)]
pub enum Vector {
    /// 32-bit float array
    F32(Array1<f32>),
    /// 64-bit float array
    F64(Array1<f64>),
}

struct VecBytes<'a>(&'a [u8]);

impl serde::Serialize for VecBytes<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(self.0)
    }
}

impl serde::Serialize for Vector {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_tuple(2)?;
        match self {
            Vector::F32(a) => {
                state.serialize_element(&0u8)?;
                let arr = a.as_slice().ok_or_else(|| {
                    serde::ser::Error::custom(
                        "non-contiguous f32 vectors cannot be serialized for storage",
                    )
                })?;
                let len = std::mem::size_of_val(arr);
                let ptr = arr.as_ptr() as *const u8;
                let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
                state.serialize_element(&VecBytes(bytes))?;
            }
            Vector::F64(a) => {
                state.serialize_element(&1u8)?;
                let arr = a.as_slice().ok_or_else(|| {
                    serde::ser::Error::custom(
                        "non-contiguous f64 vectors cannot be serialized for storage",
                    )
                })?;
                let len = std::mem::size_of_val(arr);
                let ptr = arr.as_ptr() as *const u8;
                let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
                state.serialize_element(&VecBytes(bytes))?;
            }
        }
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for Vector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_tuple(2, VectorVisitor)
    }
}

struct VectorVisitor;

impl<'de> Visitor<'de> for VectorVisitor {
    type Value = Vector;

    fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("vector representation")
    }
    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let tag: u8 = seq
            .next_element()?
            .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
        let bytes: &[u8] = seq
            .next_element()?
            .ok_or_else(|| serde::de::Error::invalid_length(1, &self))?;
        match tag {
            0u8 => {
                let width = std::mem::size_of::<f32>();
                if bytes.len() % width != 0 {
                    return Err(serde::de::Error::custom(format_args!(
                        "f32 vector byte length {} is not a multiple of {width}",
                        bytes.len()
                    )));
                }
                let v = bytes
                    .chunks_exact(width)
                    .map(|chunk| f32::from_ne_bytes(chunk.try_into().unwrap()))
                    .collect::<Vec<_>>();
                Ok(Vector::F32(Array1::from(v)))
            }
            1u8 => {
                let width = std::mem::size_of::<f64>();
                if bytes.len() % width != 0 {
                    return Err(serde::de::Error::custom(format_args!(
                        "f64 vector byte length {} is not a multiple of {width}",
                        bytes.len()
                    )));
                }
                let v = bytes
                    .chunks_exact(width)
                    .map(|chunk| f64::from_ne_bytes(chunk.try_into().unwrap()))
                    .collect::<Vec<_>>();
                Ok(Vector::F64(Array1::from(v)))
            }
            _ => Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Unsigned(tag as u64),
                &self,
            )),
        }
    }
}

impl Vector {
    /// Get the length of the vector
    pub fn len(&self) -> usize {
        match self {
            Vector::F32(v) => v.len(),
            Vector::F64(v) => v.len(),
        }
    }
    /// Check if the vector is empty
    pub fn is_empty(&self) -> bool {
        match self {
            Vector::F32(v) => v.is_empty(),
            Vector::F64(v) => v.is_empty(),
        }
    }
    pub(crate) fn el_type(&self) -> VecElementType {
        match self {
            Vector::F32(_) => VecElementType::F32,
            Vector::F64(_) => VecElementType::F64,
        }
    }
    pub(crate) fn get_hash(&self) -> impl AsRef<[u8]> {
        let mut hasher = Sha256::new();
        match self {
            Vector::F32(v) => {
                for e in v.iter() {
                    hasher.update(e.to_le_bytes());
                }
            }
            Vector::F64(v) => {
                for e in v.iter() {
                    hasher.update(e.to_le_bytes());
                }
            }
        }
        hasher.finalize_fixed()
    }
}

impl PartialEq<Self> for Vector {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Vector::F32(l), Vector::F32(r)) => {
                if l.len() != r.len() {
                    return false;
                }
                for (le, re) in l.iter().zip(r) {
                    if !OrderedFloat(*le).eq(&OrderedFloat(*re)) {
                        return false;
                    }
                }
                true
            }
            (Vector::F64(l), Vector::F64(r)) => {
                if l.len() != r.len() {
                    return false;
                }
                for (le, re) in l.iter().zip(r) {
                    if !OrderedFloat(*le).eq(&OrderedFloat(*re)) {
                        return false;
                    }
                }
                true
            }
            _ => false,
        }
    }
}

impl Eq for Vector {}

impl PartialOrd for Vector {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Vector {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Vector::F32(l), Vector::F32(r)) => {
                match l.len().cmp(&r.len()) {
                    Ordering::Equal => (),
                    o => return o,
                }
                for (le, re) in l.iter().zip(r) {
                    match OrderedFloat(*le).cmp(&OrderedFloat(*re)) {
                        Ordering::Equal => continue,
                        o => return o,
                    }
                }
                Ordering::Equal
            }
            (Vector::F32(_), Vector::F64(_)) => Ordering::Less,
            (Vector::F64(l), Vector::F64(r)) => {
                match l.len().cmp(&r.len()) {
                    Ordering::Equal => (),
                    o => return o,
                }
                for (le, re) in l.iter().zip(r) {
                    match OrderedFloat(*le).cmp(&OrderedFloat(*re)) {
                        Ordering::Equal => continue,
                        o => return o,
                    }
                }
                Ordering::Equal
            }
            (Vector::F64(_), Vector::F32(_)) => Ordering::Greater,
        }
    }
}

impl Hash for Vector {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Vector::F32(a) => {
                for el in a {
                    OrderedFloat(*el).hash(state)
                }
            }
            Vector::F64(a) => {
                for el in a {
                    OrderedFloat(*el).hash(state)
                }
            }
        }
    }
}

impl From<bool> for DataValue {
    fn from(value: bool) -> Self {
        DataValue::Bool(value)
    }
}

impl From<i64> for DataValue {
    fn from(v: i64) -> Self {
        DataValue::Num(Num::Int(v))
    }
}

impl From<i32> for DataValue {
    fn from(v: i32) -> Self {
        DataValue::Num(Num::Int(v as i64))
    }
}

impl From<f64> for DataValue {
    fn from(v: f64) -> Self {
        DataValue::Num(Num::Float(v))
    }
}

impl From<&str> for DataValue {
    fn from(v: &str) -> Self {
        DataValue::Str(SmartString::from(v))
    }
}

impl From<String> for DataValue {
    fn from(v: String) -> Self {
        DataValue::Str(SmartString::from(v))
    }
}

impl From<Vec<u8>> for DataValue {
    fn from(v: Vec<u8>) -> Self {
        DataValue::Bytes(v)
    }
}

impl<T: Into<DataValue>> From<Vec<T>> for DataValue {
    fn from(v: Vec<T>) -> Self
    where
        T: Into<DataValue>,
    {
        DataValue::List(v.into_iter().map(Into::into).collect())
    }
}

/// Representing a number
#[derive(Copy, Clone, serde_derive::Deserialize, serde_derive::Serialize)]
pub enum Num {
    /// intger number
    Int(i64),
    /// float number
    Float(f64),
}

impl Hash for Num {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Num::Int(i) => i.hash(state),
            Num::Float(f) => OrderedFloat(*f).hash(state),
        }
    }
}

impl Num {
    pub(crate) fn get_int(&self) -> Option<i64> {
        match self {
            Num::Int(i) => Some(*i),
            Num::Float(f) => {
                if f.round() == *f {
                    Some(*f as i64)
                } else {
                    None
                }
            }
        }
    }
    /// Integer with **no float coercion**: `Some` only for [`Num::Int`].
    ///
    /// Use this, never [`Num::get_int`], wherever the integer carries a *unit*.
    /// Validity and transaction-time stamps are microseconds since the epoch,
    /// while `now()` and `parse_timestamp()` return float *seconds* — so
    /// coercing an integral float there silently denominates the value 1e6 too
    /// small and lands in 1970, before any row was asserted. `get_int` cannot
    /// tell the two apart; refusing the float is the only way to say so.
    pub(crate) fn get_int_strict(&self) -> Option<i64> {
        match self {
            Num::Int(i) => Some(*i),
            Num::Float(_) => None,
        }
    }
    pub(crate) fn get_float(&self) -> f64 {
        match self {
            Num::Int(i) => *i as f64,
            Num::Float(f) => *f,
        }
    }
}

impl PartialEq for Num {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Num {}

impl Display for Num {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Num::Int(i) => write!(f, "{i}"),
            Num::Float(n) => {
                if n.is_nan() {
                    write!(f, r#"to_float("NAN")"#)
                } else if n.is_infinite() {
                    if n.is_sign_negative() {
                        write!(f, r#"to_float("NEG_INF")"#)
                    } else {
                        write!(f, r#"to_float("INF")"#)
                    }
                } else {
                    write!(f, "{n}")
                }
            }
        }
    }
}

impl Debug for Num {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Num::Int(i) => write!(f, "{i}"),
            Num::Float(n) => write!(f, "{n}"),
        }
    }
}

impl PartialOrd for Num {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Num {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Num::Int(i), Num::Float(r)) => {
                let l = *i as f64;
                match l.total_cmp(r) {
                    Ordering::Less => Ordering::Less,
                    Ordering::Equal => Ordering::Less,
                    Ordering::Greater => Ordering::Greater,
                }
            }
            (Num::Float(l), Num::Int(i)) => {
                let r = *i as f64;
                match l.total_cmp(&r) {
                    Ordering::Less => Ordering::Less,
                    Ordering::Equal => Ordering::Greater,
                    Ordering::Greater => Ordering::Greater,
                }
            }
            (Num::Int(l), Num::Int(r)) => l.cmp(r),
            (Num::Float(l), Num::Float(r)) => l.total_cmp(r),
        }
    }
}

impl Debug for DataValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl Display for DataValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DataValue::Null => f.write_str("null"),
            DataValue::Bool(b) => write!(f, "{b}"),
            DataValue::Num(n) => write!(f, "{n}"),
            DataValue::Str(s) => write!(f, "{s:?}"),
            DataValue::Bytes(b) => {
                let bs = STANDARD.encode(b);
                write!(f, "decode_base64({bs:?})")
            }
            DataValue::Uuid(u) => {
                let us = u.0.to_string();
                write!(f, "to_uuid({us:?})")
            }
            DataValue::Regex(rx) => {
                write!(f, "regex({:?})", rx.0.as_str())
            }
            DataValue::List(ls) => f.debug_list().entries(ls).finish(),
            DataValue::Set(s) => f.debug_list().entries(s).finish(),
            DataValue::Bot => write!(f, "null"),
            DataValue::Validity(v) => f
                .debug_struct("Validity")
                .field("timestamp", &v.timestamp.0)
                .field("retracted", &v.is_assert)
                .finish(),
            DataValue::Vec(a) => match a {
                Vector::F32(a) => {
                    write!(f, "vec({:?})", a.to_vec())
                }
                Vector::F64(a) => {
                    write!(f, "vec({:?}, \"F64\")", a.to_vec())
                }
            },
            DataValue::Json(j) => {
                if j.is_object() {
                    write!(f, "{}", j.0)
                } else {
                    write!(f, "json({})", j.0)
                }
            }
        }
    }
}

impl DataValue {
    /// Returns a slice of bytes if this one is a Bytes
    pub fn get_bytes(&self) -> Option<&[u8]> {
        match self {
            DataValue::Bytes(b) => Some(b),
            _ => None,
        }
    }
    /// Returns a slice of DataValues if this one is a List
    pub fn get_slice(&self) -> Option<&[DataValue]> {
        match self {
            DataValue::List(l) => Some(l),
            _ => None,
        }
    }
    /// Returns the raw str if this one is a Str
    pub fn get_str(&self) -> Option<&str> {
        match self {
            DataValue::Str(s) => Some(s),
            _ => None,
        }
    }
    /// Returns int if this one is an int
    pub fn get_int(&self) -> Option<i64> {
        match self {
            DataValue::Num(n) => n.get_int(),
            _ => None,
        }
    }
    /// Integer with no float coercion — see [`Num::get_int_strict`].
    pub(crate) fn get_int_strict(&self) -> Option<i64> {
        match self {
            DataValue::Num(n) => n.get_int_strict(),
            _ => None,
        }
    }
    pub(crate) fn get_non_neg_int(&self) -> Option<u64> {
        match self {
            DataValue::Num(n) => n
                .get_int()
                .and_then(|i| if i < 0 { None } else { Some(i as u64) }),
            _ => None,
        }
    }
    /// Returns float if this one is.
    pub fn get_float(&self) -> Option<f64> {
        match self {
            DataValue::Num(n) => Some(n.get_float()),
            _ => None,
        }
    }
    /// Returns bool if this one is.
    pub fn get_bool(&self) -> Option<bool> {
        match self {
            DataValue::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub(crate) fn uuid(uuid: Uuid) -> Self {
        Self::Uuid(UuidWrapper(uuid))
    }
    pub(crate) fn get_uuid(&self) -> Option<Uuid> {
        match self {
            DataValue::Uuid(UuidWrapper(uuid)) => Some(*uuid),
            DataValue::Str(s) => uuid::Uuid::try_parse(s).ok(),
            _ => None,
        }
    }
}

pub(crate) const LARGEST_UTF_CHAR: char = '\u{10ffff}';

#[cfg(test)]
mod row_codec_policy_tests {
    use std::cmp::Reverse;
    use std::collections::BTreeSet;

    use ndarray::Array1;
    use sha2::{Digest, Sha256};

    use super::*;

    const NON_VECTOR_KAT_V1: &[u8] = &[
        156, 164, 78, 117, 108, 108, 129, 164, 66, 111, 111, 108, 195, 129, 163, 78, 117, 109, 129,
        163, 73, 110, 116, 249, 129, 163, 78, 117, 109, 129, 165, 70, 108, 111, 97, 116, 203, 63,
        248, 0, 0, 0, 0, 0, 0, 129, 163, 83, 116, 114, 161, 120, 129, 165, 66, 121, 116, 101, 115,
        196, 2, 0, 255, 129, 164, 85, 117, 105, 100, 196, 16, 0, 17, 34, 51, 68, 85, 102, 119, 136,
        153, 170, 187, 204, 221, 238, 255, 129, 164, 76, 105, 115, 116, 146, 164, 78, 117, 108,
        108, 129, 164, 66, 111, 111, 108, 194, 129, 163, 83, 101, 116, 146, 164, 78, 117, 108, 108,
        129, 164, 66, 111, 111, 108, 195, 129, 164, 74, 115, 111, 110, 129, 161, 107, 1, 129, 168,
        86, 97, 108, 105, 100, 105, 116, 121, 146, 7, 195, 163, 66, 111, 116,
    ];
    const VECTOR_F32_LITTLE_ENDIAN_KAT_V1: &[u8] = &[
        0x81, 0xa3, b'V', b'e', b'c', 0x92, 0x00, 0xc4, 0x08, 0x00, 0x00, 0x80, 0x3f, 0x00, 0x00,
        0x20, 0xc0,
    ];
    const VECTOR_F32_BIG_ENDIAN_KAT_V1: &[u8] = &[
        0x81, 0xa3, b'V', b'e', b'c', 0x92, 0x00, 0xc4, 0x08, 0x3f, 0x80, 0x00, 0x00, 0xc0, 0x20,
        0x00, 0x00,
    ];
    const VECTOR_F64_LITTLE_ENDIAN_KAT_V1: &[u8] = &[
        0x81, 0xa3, b'V', b'e', b'c', 0x92, 0x01, 0xc4, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xf0, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0xc0,
    ];
    const VECTOR_F64_BIG_ENDIAN_KAT_V1: &[u8] = &[
        0x81, 0xa3, b'V', b'e', b'c', 0x92, 0x01, 0xc4, 0x10, 0x3f, 0xf0, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0xc0, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    fn non_vector_fixture() -> Vec<DataValue> {
        vec![
            DataValue::Null,
            DataValue::Bool(true),
            DataValue::Num(Num::Int(-7)),
            DataValue::Num(Num::Float(1.5)),
            DataValue::Str("x".into()),
            DataValue::Bytes(vec![0x00, 0xff]),
            DataValue::Uuid(UuidWrapper(
                Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap(),
            )),
            DataValue::List(vec![DataValue::Null, DataValue::Bool(false)]),
            DataValue::Set(BTreeSet::from([DataValue::Null, DataValue::Bool(true)])),
            DataValue::Json(JsonData(serde_json::json!({"k": 1}))),
            DataValue::Validity(Validity {
                timestamp: ValidityTs(Reverse(7)),
                is_assert: Reverse(true),
            }),
            DataValue::Bot,
        ]
    }

    fn update_field(hasher: &mut Sha256, field_count: &mut u64, label: &[u8], value: &[u8]) {
        *field_count = field_count.checked_add(1).unwrap();
        hasher.update([0x01]);
        hasher.update(u64::try_from(label.len()).unwrap().to_be_bytes());
        hasher.update(label);
        hasher.update(u64::try_from(value.len()).unwrap().to_be_bytes());
        hasher.update(value);
    }

    #[test]
    fn stored_row_datavalue_codec_policy_is_a_hard_pinned_literal_oracle() {
        let fixture = non_vector_fixture();
        let encoded = rmp_serde::to_vec(&fixture).unwrap();
        assert_eq!(encoded, NON_VECTOR_KAT_V1);
        assert_eq!(
            rmp_serde::from_slice::<Vec<DataValue>>(&encoded).unwrap(),
            fixture
        );

        let vector = DataValue::Vec(Vector::F32(Array1::from_vec(vec![1.0, -2.5])));
        let encoded_vector = rmp_serde::to_vec(&vector).unwrap();
        let expected_vector = if cfg!(target_endian = "little") {
            VECTOR_F32_LITTLE_ENDIAN_KAT_V1
        } else {
            VECTOR_F32_BIG_ENDIAN_KAT_V1
        };
        assert_eq!(encoded_vector, expected_vector);
        assert_eq!(
            rmp_serde::from_slice::<DataValue>(&encoded_vector).unwrap(),
            vector
        );

        let vector = DataValue::Vec(Vector::F64(Array1::from_vec(vec![1.0, -2.5])));
        let encoded_vector = rmp_serde::to_vec(&vector).unwrap();
        let expected_vector = if cfg!(target_endian = "little") {
            VECTOR_F64_LITTLE_ENDIAN_KAT_V1
        } else {
            VECTOR_F64_BIG_ENDIAN_KAT_V1
        };
        assert_eq!(encoded_vector, expected_vector);
        assert_eq!(
            rmp_serde::from_slice::<DataValue>(&encoded_vector).unwrap(),
            vector
        );

        let mut hasher = Sha256::new();
        hasher.update(STORED_ROW_DATAVALUE_CODEC_POLICY_FINGERPRINT_DOMAIN_V1);
        let mut field_count = 0_u64;
        for (label, value) in [
            (
                b"policy-description".as_slice(),
                STORED_ROW_DATAVALUE_CODEC_POLICY_DESCRIPTION_V1.as_bytes(),
            ),
            (b"non-vector-kat".as_slice(), NON_VECTOR_KAT_V1),
            (
                b"vector-f32-little-endian-kat".as_slice(),
                VECTOR_F32_LITTLE_ENDIAN_KAT_V1,
            ),
            (
                b"vector-f32-big-endian-kat".as_slice(),
                VECTOR_F32_BIG_ENDIAN_KAT_V1,
            ),
            (
                b"vector-f64-little-endian-kat".as_slice(),
                VECTOR_F64_LITTLE_ENDIAN_KAT_V1,
            ),
            (
                b"vector-f64-big-endian-kat".as_slice(),
                VECTOR_F64_BIG_ENDIAN_KAT_V1,
            ),
        ] {
            update_field(&mut hasher, &mut field_count, label, value);
        }
        hasher.update([0xff]);
        hasher.update(field_count.to_be_bytes());
        let derived: [u8; 32] = hasher.finalize().into();
        assert_eq!(derived, STORED_ROW_DATAVALUE_CODEC_POLICY_FINGERPRINT_V1);
        assert_ne!(STORED_ROW_DATAVALUE_CODEC_POLICY_FINGERPRINT_V1, [0; 32]);
    }
}
