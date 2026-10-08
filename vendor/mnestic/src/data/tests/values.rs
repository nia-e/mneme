/*
 *  Copyright 2022, The Cozo Project Authors.
 *
 *  This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 *  If a copy of the MPL was not distributed with this file,
 *  You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 */

use std::collections::{BTreeMap, HashMap};
use std::mem::size_of;

use crate::data::symb::Symbol;
use crate::data::value::{DataValue, RegexWrapper, Vector};

#[derive(serde_derive::Serialize)]
struct RawVector<'a>(u8, #[serde(with = "serde_bytes")] &'a [u8]);

fn decode_raw_vector(tag: u8, bytes: &[u8]) -> Result<Vector, rmp_serde::decode::Error> {
    let encoded = rmp_serde::to_vec(&RawVector(tag, bytes)).unwrap();
    rmp_serde::from_slice(&encoded)
}

#[test]
fn vector_deserialize_rejects_partial_elements() {
    for tail_len in 1..std::mem::size_of::<f32>() {
        let error = decode_raw_vector(0, &vec![0; tail_len]).unwrap_err();
        assert!(
            error.to_string().contains("f32 vector byte length"),
            "unexpected error for {tail_len}-byte f32 tail: {error}"
        );
    }

    for tail_len in 1..std::mem::size_of::<f64>() {
        let error = decode_raw_vector(1, &vec![0; tail_len]).unwrap_err();
        assert!(
            error.to_string().contains("f64 vector byte length"),
            "unexpected error for {tail_len}-byte f64 tail: {error}"
        );
    }
}

#[test]
fn vector_deserialize_preserves_native_wire_bytes() {
    let f32_values = [1.25_f32, -9.5];
    let f32_bytes = f32_values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect::<Vec<_>>();
    let Vector::F32(decoded) = decode_raw_vector(0, &f32_bytes).unwrap() else {
        panic!("decoded f32 wire bytes as the wrong vector type");
    };
    assert_eq!(decoded.as_slice().unwrap(), f32_values);

    let f64_values = [1.25_f64, -9.5];
    let f64_bytes = f64_values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect::<Vec<_>>();
    let Vector::F64(decoded) = decode_raw_vector(1, &f64_bytes).unwrap() else {
        panic!("decoded f64 wire bytes as the wrong vector type");
    };
    assert_eq!(decoded.as_slice().unwrap(), f64_values);
}

#[test]
fn regex_wrapper_deserialize_returns_an_error() {
    let encoded = rmp_serde::to_vec("a.*").unwrap();
    let result: Result<RegexWrapper, _> = rmp_serde::from_slice(&encoded);
    let error = match result {
        Ok(_) => panic!("deserialized an internal-only regex value"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("internal-only"));
}

#[test]
fn regex_wrapper_serialize_returns_an_error() {
    let regex = RegexWrapper(regex::Regex::new("a.*").unwrap());
    let error = rmp_serde::to_vec(&regex).unwrap_err();
    assert!(error.to_string().contains("internal-only"));
}

#[test]
fn show_size() {
    dbg!(size_of::<DataValue>());
    dbg!(size_of::<Symbol>());
    dbg!(size_of::<String>());
    dbg!(size_of::<HashMap<String, String>>());
    dbg!(size_of::<BTreeMap<String, String>>());
}

#[test]
fn utf8() {
    let c = char::from_u32(0x10FFFF).unwrap();
    let mut s = String::new();
    s.push(c);
    println!("{}", s);
    println!(
        "{:b} {:b} {:b} {:b}",
        s.as_bytes()[0],
        s.as_bytes()[1],
        s.as_bytes()[2],
        s.as_bytes()[3]
    );
    dbg!(s);
}

#[test]
fn display_datavalues() {
    println!("{}", DataValue::Null);
    println!("{}", DataValue::from(true));
    println!("{}", DataValue::from(-1));
    println!("{}", DataValue::from(-1_121_212_121.331_212));
    println!("{}", DataValue::from(f64::NAN));
    println!("{}", DataValue::from(f64::NEG_INFINITY));
    println!("{}", DataValue::from(vec![10, 20]));
    println!("{}", DataValue::from(vec!["hello", "你好"]));
    println!(
        "{}",
        DataValue::List(vec![
            DataValue::from(false),
            DataValue::from(r###"abc"你"好'啊👌"###),
            DataValue::from(f64::NEG_INFINITY),
        ])
    );
}
