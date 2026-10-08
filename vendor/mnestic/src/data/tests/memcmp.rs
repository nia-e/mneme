/*
 *  Copyright 2022, The Cozo Project Authors.
 *
 *  This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 *  If a copy of the MPL was not distributed with this file,
 *  You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 */

use uuid::Uuid;

use crate::data::memcmp::{
    try_decode_bytes, MemCmpDecodeBudget, MemCmpDecodeError, MemCmpEncoder, MAX_ENCODED_KEY_BYTES,
    MAX_KEY_JSON_DEPTH, MAX_KEY_NESTING_DEPTH, MAX_KEY_VALUES, MAX_KEY_VECTOR_ELEMENTS,
};
use crate::data::tuple::try_decode_tuple_from_key;
use crate::data::value::{DataValue, JsonData, Num, UuidWrapper, Vector};

const NULL_TAG: u8 = 0x01;
const FALSE_TAG: u8 = 0x02;
const TRUE_TAG: u8 = 0x03;
const VEC_TAG: u8 = 0x04;
const NUM_TAG: u8 = 0x05;
const STR_TAG: u8 = 0x06;
const UUID_TAG: u8 = 0x08;
const REGEX_TAG: u8 = 0x09;
const LIST_TAG: u8 = 0x0A;
const SET_TAG: u8 = 0x0B;
const VLD_TAG: u8 = 0x0C;
const JSON_TAG: u8 = 0x0D;
const BOT_TAG: u8 = 0xFF;
const CONTAINER_END: u8 = 0x00;

fn stored_key(wire: &[u8]) -> Vec<u8> {
    let mut key = vec![0; 8];
    key.extend_from_slice(wire);
    key
}

fn assert_corrupt_key(wire: &[u8], expected: &str) {
    let key = stored_key(wire);
    let outcome = std::panic::catch_unwind(|| try_decode_tuple_from_key(&key, 16));
    let result = outcome.expect("fallible stored-key decoding must never unwind");
    let error = result.expect_err("malformed stored key unexpectedly decoded");
    assert!(
        error.to_string().contains(expected),
        "expected error containing {expected:?}, got {error}"
    );
}

fn nested_json(depth: usize) -> serde_json::Value {
    let mut value = serde_json::Value::Null;
    for _ in 0..depth {
        value = serde_json::Value::Array(vec![value]);
    }
    value
}

fn try_decode_value(wire: &[u8]) -> (DataValue, &[u8]) {
    let mut budget = MemCmpDecodeBudget::for_stored_key();
    DataValue::try_decode_from_key(wire, &mut budget, 0).unwrap()
}

#[test]
fn encode_decode_num() {
    use rand::prelude::*;

    let n = i64::MAX;
    let mut collected = vec![];

    let mut test_num = |n: Num| {
        let mut encoder = vec![];
        encoder.encode_num(n);
        let (decoded, rest) = Num::try_decode_from_key(&encoder).unwrap();
        assert_eq!(decoded, n);
        assert!(rest.is_empty());
        collected.push(encoder);
    };
    for i in 0..54 {
        for j in 0..1000 {
            let vb = (n >> i) - j;
            for v in [vb, -vb - 1] {
                test_num(Num::Int(v));
            }
        }
    }
    test_num(Num::Float(f64::INFINITY));
    test_num(Num::Float(f64::NEG_INFINITY));
    test_num(Num::Float(f64::NAN));
    for _ in 0..100000 {
        let f = (thread_rng().gen::<f64>() - 0.5) * 2.0;
        test_num(Num::Float(f));
        test_num(Num::Float(1. / f));
    }
    let mut collected_copy = collected.clone();
    collected.sort();
    collected_copy.sort_by_key(|c| Num::try_decode_from_key(c).unwrap().0);
    assert_eq!(collected, collected_copy);
}

#[test]
fn test_encode_decode_uuid() {
    let uuid = DataValue::Uuid(UuidWrapper(
        Uuid::parse_str("dd85b19a-5fde-11ed-a88e-1774a7698039").unwrap(),
    ));
    let mut encoder = vec![];
    encoder.encode_datavalue(&uuid);
    let (decoded, remaining) = try_decode_value(&encoder);
    assert_eq!(decoded, uuid);
    assert!(remaining.is_empty());
}

#[test]
fn encode_decode_bytes() {
    let target = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit...";
    for i in 0..target.len() {
        let bs = &target[i..];
        let mut encoder: Vec<u8> = vec![];
        encoder.encode_bytes(bs);
        let (decoded, remaining) = try_decode_bytes(&encoder).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(bs, decoded);

        let mut encoder: Vec<u8> = vec![];
        encoder.encode_bytes(target);
        encoder.encode_bytes(bs);
        encoder.encode_bytes(bs);
        encoder.encode_bytes(target);

        let (decoded, remaining) = try_decode_bytes(&encoder).unwrap();
        assert_eq!(&target[..], decoded);

        let (decoded, remaining) = try_decode_bytes(remaining).unwrap();
        assert_eq!(bs, decoded);

        let (decoded, remaining) = try_decode_bytes(remaining).unwrap();
        assert_eq!(bs, decoded);

        let (decoded, remaining) = try_decode_bytes(remaining).unwrap();
        assert_eq!(&target[..], decoded);
        assert!(remaining.is_empty());
    }
}

#[test]
fn specific_encode() {
    let mut encoder = vec![];
    encoder.encode_datavalue(&DataValue::from(2095));
    // println!("e1 {:?}", encoder);
    encoder.encode_datavalue(&DataValue::from("MSS"));
    // println!("e2 {:?}", encoder);
    let (a, remaining) = try_decode_value(&encoder);
    // println!("r  {:?}", remaining);
    let (b, remaining) = try_decode_value(remaining);
    assert!(remaining.is_empty());
    assert_eq!(a, DataValue::from(2095));
    assert_eq!(b, DataValue::from("MSS"));
}

#[test]
fn invalid_utf8_memcmp_strings_return_typed_errors() {
    let mut encoded = vec![STR_TAG];
    encoded.encode_bytes(&[0xff]);

    let key = stored_key(&encoded);
    let error = try_decode_tuple_from_key(&key, 1).unwrap_err();
    assert!(error.to_string().contains("invalid UTF-8"));
}

#[test]
fn malformed_memcmp_byte_groups_return_errors() {
    for malformed in [
        vec![],
        vec![0; 8],
        vec![0, 0, 0, 0, 0, 0, 0, 0, 0x00],
        vec![1, 0, 0, 0, 0, 0, 0, 0, 0xF7],
        vec![0, 0, 0, 0, 0, 0, 0, 0, 0xFF],
    ] {
        let outcome = std::panic::catch_unwind(|| try_decode_bytes(&malformed));
        assert!(outcome.is_ok(), "byte decoder unwound for {malformed:?}");
        assert!(outcome.unwrap().is_err(), "accepted {malformed:?}");
    }
}

#[test]
fn memcmp_decode_reasons_are_bounded_in_bytes() {
    let error = MemCmpDecodeError::new("é".repeat(300));
    let rendered = error.to_string();
    assert!(rendered.len() <= 256, "reason was {} bytes", rendered.len());
    assert!(rendered.ends_with('…'));
}

#[test]
fn malformed_value_shapes_return_bounded_errors() {
    assert_corrupt_key(&[0x7E], "unknown memcmp value tag");
    assert_corrupt_key(&[BOT_TAG], "reserved for internal key bounds");
    assert_corrupt_key(
        &[LIST_TAG, BOT_TAG, CONTAINER_END],
        "reserved for internal key bounds",
    );
    assert_corrupt_key(&[NUM_TAG], "truncated numeric");

    let mut bad_num = vec![NUM_TAG];
    bad_num.extend_from_slice(&[0; 8]);
    bad_num.push(0x7F);
    assert_corrupt_key(&bad_num, "numeric representation subtag");

    let mut noncanonical_num = vec![];
    noncanonical_num.encode_num(Num::Float(1.0));
    noncanonical_num[8] = 0x04;
    noncanonical_num.extend_from_slice(&(1_u64 ^ (1_u64 << 63)).to_be_bytes());
    noncanonical_num.insert(0, NUM_TAG);
    assert_corrupt_key(&noncanonical_num, "noncanonical");

    assert_corrupt_key(&[UUID_TAG], "truncated UUID");

    let mut bad_json = vec![JSON_TAG];
    bad_json.encode_bytes(b"{");
    assert_corrupt_key(&bad_json, "invalid JSON");

    assert_corrupt_key(&[REGEX_TAG], "Regex is internal-only");
    assert_corrupt_key(
        &[LIST_TAG, REGEX_TAG, CONTAINER_END],
        "Regex is internal-only",
    );

    assert_corrupt_key(&[LIST_TAG, NULL_TAG], "unterminated memcmp list");
    assert_corrupt_key(&[SET_TAG], "Set is internal-only");
    assert_corrupt_key(&[LIST_TAG, SET_TAG, CONTAINER_END], "Set is internal-only");

    let mut bad_validity = vec![VLD_TAG];
    bad_validity.extend_from_slice(&0x7fff_ffff_ffff_ffff_u64.to_be_bytes());
    bad_validity.push(2);
    assert_corrupt_key(&bad_validity, "validity assertion flag");

    assert_corrupt_key(&[VEC_TAG], "vector element subtag");
    let mut bad_vector_tag = vec![VEC_TAG, 0x7F];
    bad_vector_tag.extend_from_slice(&0_u64.to_be_bytes());
    assert_corrupt_key(&bad_vector_tag, "vector element subtag");
    assert_corrupt_key(&[VEC_TAG, 0x01, 0], "truncated vector length");
    let mut short_vector = vec![VEC_TAG, 0x01];
    short_vector.extend_from_slice(&1_u64.to_be_bytes());
    assert_corrupt_key(&short_vector, "truncated vector element payload");

    assert_corrupt_key(&[NULL_TAG, NUM_TAG, 0], "truncated numeric");
}

#[test]
fn stored_json_depth_cap_is_symmetric() {
    let accepted = DataValue::Json(JsonData(nested_json(MAX_KEY_JSON_DEPTH)));
    let mut accepted_key = vec![0; 8];
    accepted_key.encode_datavalue(&accepted);
    assert_eq!(
        try_decode_tuple_from_key(&accepted_key, 1).unwrap(),
        vec![accepted]
    );

    let rejected = DataValue::Json(JsonData(nested_json(MAX_KEY_JSON_DEPTH + 1)));
    let mut rejected_key = vec![0; 8];
    rejected_key.encode_datavalue(&rejected);
    let error = try_decode_tuple_from_key(&rejected_key, 1).unwrap_err();
    assert!(error.to_string().contains("JSON nesting exceeds 64"));
}

#[test]
fn stored_key_resource_limits_accept_cap_and_reject_cap_plus_one() {
    let mut value_cap = vec![0; 8];
    value_cap.resize(value_cap.len() + MAX_KEY_VALUES, NULL_TAG);
    assert_eq!(
        try_decode_tuple_from_key(&value_cap, usize::MAX)
            .unwrap()
            .len(),
        MAX_KEY_VALUES
    );
    value_cap.push(NULL_TAG);
    assert!(try_decode_tuple_from_key(&value_cap, 16)
        .unwrap_err()
        .to_string()
        .contains("more than"));

    let mut at_depth = DataValue::Null;
    for _ in 0..MAX_KEY_NESTING_DEPTH {
        at_depth = DataValue::List(vec![at_depth]);
    }
    let mut depth_key = vec![0; 8];
    depth_key.encode_datavalue(&at_depth);
    try_decode_tuple_from_key(&depth_key, 1).unwrap();
    depth_key.clear();
    depth_key.extend_from_slice(&[0; 8]);
    depth_key.encode_datavalue(&DataValue::List(vec![at_depth]));
    assert!(try_decode_tuple_from_key(&depth_key, 1)
        .unwrap_err()
        .to_string()
        .contains("nesting exceeds"));

    let vector_cap = DataValue::Vec(Vector::F64(ndarray::Array1::zeros(MAX_KEY_VECTOR_ELEMENTS)));
    let mut vector_key = vec![0; 8];
    vector_key.encode_datavalue(&vector_cap);
    try_decode_tuple_from_key(&vector_key, 1).unwrap();
    vector_key.clear();
    vector_key.extend_from_slice(&[0; 8]);
    vector_key.encode_datavalue(&DataValue::Vec(Vector::F64(ndarray::Array1::zeros(
        MAX_KEY_VECTOR_ELEMENTS + 1,
    ))));
    assert!(try_decode_tuple_from_key(&vector_key, 1)
        .unwrap_err()
        .to_string()
        .contains("vector contains"));

    let groups = (MAX_ENCODED_KEY_BYTES - 8 - 1) / 9;
    let payload_len = (groups - 1) * 8;
    let mut byte_cap = vec![0; 8];
    byte_cap.encode_datavalue(&DataValue::Bytes(vec![0; payload_len]));
    byte_cap.resize(MAX_ENCODED_KEY_BYTES, NULL_TAG);
    assert_eq!(byte_cap.len(), MAX_ENCODED_KEY_BYTES);
    try_decode_tuple_from_key(&byte_cap, 1).unwrap();
    byte_cap.push(NULL_TAG);
    let error = try_decode_tuple_from_key(&byte_cap, 1).unwrap_err();
    let rendered = error.to_string();
    assert!(rendered.contains("limit is 65536"));
    assert!(
        rendered.len() < 512,
        "diagnostic was not bounded: {rendered}"
    );
}

#[test]
fn arbitrary_stored_key_payload_bytes_never_unwind() {
    let mut state = 0xA076_1D64_78BD_642F_u64;
    for case in 0..4096 {
        state ^= state << 7;
        state ^= state >> 9;
        state ^= state << 8;
        let payload_len = if case % 257 == 0 {
            MAX_ENCODED_KEY_BYTES - 7 + usize::try_from(state % 17).unwrap()
        } else {
            usize::try_from(state % 1025).unwrap()
        };
        let relation_id = state & ((1_u64 << 48) - 1);
        let mut key = relation_id.to_be_bytes().to_vec();
        key.resize(8 + payload_len, 0);
        for byte in &mut key[8..] {
            state ^= state << 7;
            state ^= state >> 9;
            state ^= state << 8;
            *byte = state as u8;
        }
        if payload_len != 0 {
            // Exercise every possible first value tag sixteen times instead
            // of letting invalid relation IDs or a mostly-random first byte
            // mask the deeper decoder.
            key[8] = case as u8;
        }
        let size_hint = if case % 509 == 0 {
            usize::MAX
        } else {
            usize::try_from(state & 0x3FF).unwrap()
        };
        let outcome = std::panic::catch_unwind(|| try_decode_tuple_from_key(&key, size_hint));
        assert!(outcome.is_ok(), "decoder unwound for corpus case {case}");
    }
}

#[test]
fn short_relation_prefixes_never_unwind() {
    for len in 0..8 {
        let key = vec![0xa5; len];
        let outcome = std::panic::catch_unwind(|| try_decode_tuple_from_key(&key, usize::MAX));
        assert!(outcome.is_ok(), "decoder unwound for {len}-byte prefix");
        assert!(outcome.unwrap().is_err(), "accepted {len}-byte prefix");
    }
}

#[test]
fn encode_decode_datavalues() {
    let mut dv = vec![
        DataValue::Null,
        DataValue::from(false),
        DataValue::from(true),
        DataValue::from(1),
        DataValue::from(1.0),
        DataValue::from(i64::MAX),
        DataValue::from(i64::MAX - 1),
        DataValue::from(i64::MAX - 2),
        DataValue::from(i64::MIN),
        DataValue::from(i64::MIN + 1),
        DataValue::from(i64::MIN + 2),
        DataValue::from(f64::INFINITY),
        DataValue::from(f64::NEG_INFINITY),
        DataValue::List(vec![]),
    ];
    dv.push(DataValue::List(dv.clone()));
    dv.push(DataValue::List(dv.clone()));
    let mut encoded = vec![];
    let v = DataValue::List(dv);
    encoded.encode_datavalue(&v);
    let (decoded, remaining) = try_decode_value(&encoded);
    assert!(remaining.is_empty());
    assert_eq!(decoded, v);
}
