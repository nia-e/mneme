/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::fmt;
use std::io::Write;

use serde::de::Visitor;
use serde::{Deserialize, Serialize};

use crate::data::msgpack::{
    decode_exact, require_canonical, scan_exact_envelope, validate_exact_envelope, CanonicalMode,
    ExactBytesWriter, StoredMsgpackErrorKind, StoredMsgpackProfile, StoredMsgpackRoot,
};

fn row(payload: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(payload.len() + 1);
    encoded.push(0x91);
    encoded.extend_from_slice(payload);
    encoded
}

fn assert_kind(encoded: &[u8], profile: StoredMsgpackProfile, kind: StoredMsgpackErrorKind) {
    let error = validate_exact_envelope(encoded, profile).unwrap_err();
    assert_eq!(error.kind(), kind, "unexpected error: {error}");
    // Error text is bounded metadata, never a dump of the input.
    assert!(error.to_string().len() < 160);
}

#[test]
fn scanner_accepts_every_marker_family() {
    let scalars: &[&[u8]] = &[
        &[0x00],
        &[0x7f],
        &[0xe0],
        &[0xff],
        &[0xc0],
        &[0xc2],
        &[0xc3],
        &[0xa0],
        &[0xa1, b'x'],
        &[0xc4, 1, 0xaa],
        &[0xc5, 0, 1, 0xaa],
        &[0xc6, 0, 0, 0, 1, 0xaa],
        &[0xc7, 1, 42, 0xaa],
        &[0xc8, 0, 1, 42, 0xaa],
        &[0xc9, 0, 0, 0, 1, 42, 0xaa],
        &[0xca, 0, 0, 0, 0],
        &[0xcb, 0, 0, 0, 0, 0, 0, 0, 0],
        &[0xcc, 0],
        &[0xcd, 0, 0],
        &[0xce, 0, 0, 0, 0],
        &[0xcf, 0, 0, 0, 0, 0, 0, 0, 0],
        &[0xd0, 0],
        &[0xd1, 0, 0],
        &[0xd2, 0, 0, 0, 0],
        &[0xd3, 0, 0, 0, 0, 0, 0, 0, 0],
        &[0xd4, 42, 0],
        &[0xd5, 42, 0, 0],
        &[0xd6, 42, 0, 0, 0, 0],
        &[0xd7, 42, 0, 0, 0, 0, 0, 0, 0, 0],
        &[0xd8, 42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        &[0xd9, 1, b'x'],
        &[0xda, 0, 1, b'x'],
        &[0xdb, 0, 0, 0, 1, b'x'],
    ];
    for scalar in scalars {
        validate_exact_envelope(&row(scalar), StoredMsgpackProfile::Row)
            .unwrap_or_else(|error| panic!("marker {scalar:02x?}: {error}"));
    }

    for nested in [
        vec![0x91, 0x90],
        vec![0x91, 0xdc, 0, 0],
        vec![0x91, 0xdd, 0, 0, 0, 0],
        vec![0x91, 0x80],
        vec![0x91, 0xde, 0, 0],
        vec![0x91, 0xdf, 0, 0, 0, 0],
    ] {
        validate_exact_envelope(&nested, StoredMsgpackProfile::Row).unwrap();
    }
}

#[test]
fn every_width_reports_truncation() {
    let complete: &[&[u8]] = &[
        &[0xa1, b'x'],
        &[0xc4, 1, 0],
        &[0xc5, 0, 1, 0],
        &[0xc6, 0, 0, 0, 1, 0],
        &[0xc7, 1, 0, 0],
        &[0xc8, 0, 1, 0, 0],
        &[0xc9, 0, 0, 0, 1, 0, 0],
        &[0xca, 0, 0, 0, 0],
        &[0xcb, 0, 0, 0, 0, 0, 0, 0, 0],
        &[0xcc, 0],
        &[0xcd, 0, 0],
        &[0xce, 0, 0, 0, 0],
        &[0xcf, 0, 0, 0, 0, 0, 0, 0, 0],
        &[0xd0, 0],
        &[0xd1, 0, 0],
        &[0xd2, 0, 0, 0, 0],
        &[0xd3, 0, 0, 0, 0, 0, 0, 0, 0],
        &[0xd4, 0, 0],
        &[0xd5, 0, 0, 0],
        &[0xd6, 0, 0, 0, 0, 0],
        &[0xd7, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        &[0xd8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        &[0xd9, 1, 0],
        &[0xda, 0, 1, 0],
        &[0xdb, 0, 0, 0, 1, 0],
        &[0xdc, 0, 0],
        &[0xdd, 0, 0, 0, 0],
        &[0xde, 0, 0],
        &[0xdf, 0, 0, 0, 0],
    ];

    for token in complete {
        for cut in 1..token.len() {
            let encoded = row(&token[..cut]);
            assert_kind(
                &encoded,
                StoredMsgpackProfile::Row,
                StoredMsgpackErrorKind::Truncated,
            );
        }
    }
}

#[test]
fn scanner_rejects_empty_trailing_reserved_and_wrong_roots() {
    assert_kind(
        &[],
        StoredMsgpackProfile::Row,
        StoredMsgpackErrorKind::Empty,
    );
    assert_kind(
        &[0x90, 0xc0],
        StoredMsgpackProfile::Row,
        StoredMsgpackErrorKind::TrailingBytes,
    );
    assert_kind(
        &[0x91, 0xc1],
        StoredMsgpackProfile::Row,
        StoredMsgpackErrorKind::ReservedMarker,
    );
    assert_kind(
        &[0xc0],
        StoredMsgpackProfile::Row,
        StoredMsgpackErrorKind::WrongRoot,
    );
    assert_kind(
        &[0x80],
        StoredMsgpackProfile::Row,
        StoredMsgpackErrorKind::WrongRoot,
    );
    validate_exact_envelope(&[0x80], StoredMsgpackProfile::RelationCatalog).unwrap();
    validate_exact_envelope(&[0x90], StoredMsgpackProfile::RelationCatalog).unwrap();
}

#[test]
fn scanned_evidence_reports_only_admitted_container_roots() {
    let row = scan_exact_envelope(&[0x90], StoredMsgpackProfile::Row).unwrap();
    assert_eq!(row.root(), StoredMsgpackRoot::Array);
    assert_eq!(row.decode::<Vec<u8>>().unwrap(), Vec::<u8>::new());

    let legacy_catalog =
        scan_exact_envelope(&[0x90], StoredMsgpackProfile::RelationCatalog).unwrap();
    assert_eq!(legacy_catalog.root(), StoredMsgpackRoot::Array);
    assert_eq!(
        legacy_catalog.decode::<Vec<u8>>().unwrap(),
        Vec::<u8>::new()
    );

    let current_catalog =
        scan_exact_envelope(&[0x80], StoredMsgpackProfile::RelationCatalog).unwrap();
    assert_eq!(current_catalog.root(), StoredMsgpackRoot::Map);
    assert_eq!(
        current_catalog
            .decode::<std::collections::BTreeMap<String, u8>>()
            .unwrap(),
        std::collections::BTreeMap::new()
    );
}

#[test]
fn scan_token_preserves_profile_root_and_exact_eof_rejections() {
    for encoded in [&[0xc0][..], &[0x00][..], &[0xa0][..]] {
        assert_kind(
            encoded,
            StoredMsgpackProfile::Row,
            StoredMsgpackErrorKind::WrongRoot,
        );
        assert_kind(
            encoded,
            StoredMsgpackProfile::RelationCatalog,
            StoredMsgpackErrorKind::WrongRoot,
        );
    }
    assert_kind(
        &[0x80],
        StoredMsgpackProfile::Row,
        StoredMsgpackErrorKind::WrongRoot,
    );
    assert_kind(
        &[0x90, 0xc0],
        StoredMsgpackProfile::RelationCatalog,
        StoredMsgpackErrorKind::TrailingBytes,
    );
}

#[test]
fn byte_item_and_depth_caps_are_exact() {
    const ROW_BYTE_CAP: usize = 1024 * 1024;
    const ENVELOPE_BYTES: usize = 1 + 1 + 4;
    let payload_length = ROW_BYTE_CAP - ENVELOPE_BYTES;
    let mut exact_byte_cap = Vec::with_capacity(ROW_BYTE_CAP);
    exact_byte_cap.extend_from_slice(&[0x91, 0xc6]);
    exact_byte_cap.extend_from_slice(&(payload_length as u32).to_be_bytes());
    exact_byte_cap.resize(ROW_BYTE_CAP, 0);
    validate_exact_envelope(&exact_byte_cap, StoredMsgpackProfile::Row).unwrap();
    let mut over_byte_cap = exact_byte_cap;
    over_byte_cap.push(0);
    assert_kind(
        &over_byte_cap,
        StoredMsgpackProfile::Row,
        StoredMsgpackErrorKind::ByteLimit,
    );

    let mut exact_items = vec![0xdc, 0x40, 0x00];
    exact_items.resize(3 + 16_384, 0xc0);
    validate_exact_envelope(&exact_items, StoredMsgpackProfile::Row).unwrap();
    let over_items = [0xdc, 0x40, 0x01];
    assert_kind(
        &over_items,
        StoredMsgpackProfile::Row,
        StoredMsgpackErrorKind::ItemLimit,
    );

    let mut exact_depth = vec![0x91; 32];
    exact_depth.push(0x90);
    // The final empty array is container depth 33, so use a scalar leaf for
    // exactly 32 container levels.
    exact_depth.pop();
    exact_depth.push(0xc0);
    validate_exact_envelope(&exact_depth, StoredMsgpackProfile::Row).unwrap();
    let mut over_depth = vec![0x91; 33];
    over_depth.push(0xc0);
    assert_kind(
        &over_depth,
        StoredMsgpackProfile::Row,
        StoredMsgpackErrorKind::DepthLimit,
    );
}

fn many_tokens(container_count: usize, scalars_per_container: usize) -> Vec<u8> {
    assert!(container_count <= u16::MAX as usize);
    assert!(scalars_per_container <= 15);
    let mut encoded = Vec::with_capacity(3 + container_count * (1 + scalars_per_container));
    encoded.push(0xdc);
    encoded.extend_from_slice(&(container_count as u16).to_be_bytes());
    for _ in 0..container_count {
        encoded.push(0x90 | scalars_per_container as u8);
        encoded.extend(std::iter::repeat_n(0xc0, scalars_per_container));
    }
    encoded
}

#[test]
fn token_and_container_caps_are_exact() {
    // 1 root + 13_107 arrays + 4 scalar children each = 65_536 tokens.
    let exact_tokens = many_tokens(13_107, 4);
    validate_exact_envelope(&exact_tokens, StoredMsgpackProfile::Row).unwrap();

    // Same number of containers, but one extra scalar produces token 65_537.
    let mut over_tokens = exact_tokens;
    let last_array = over_tokens.len() - 5;
    over_tokens[last_array] = 0x95;
    over_tokens.push(0xc0);
    assert_kind(
        &over_tokens,
        StoredMsgpackProfile::Row,
        StoredMsgpackErrorKind::TokenLimit,
    );

    // Root + 16_383 empty arrays is exactly 16_384 containers.
    let exact_containers = many_tokens(16_383, 0);
    validate_exact_envelope(&exact_containers, StoredMsgpackProfile::Row).unwrap();
    let over_containers = many_tokens(16_384, 0);
    assert_kind(
        &over_containers,
        StoredMsgpackProfile::Row,
        StoredMsgpackErrorKind::ContainerLimit,
    );
}

#[test]
fn hostile_map32_and_ext32_lengths_fail_without_payload_allocation() {
    assert_kind(
        &[0xdf, 0xff, 0xff, 0xff, 0xff],
        StoredMsgpackProfile::RelationCatalog,
        StoredMsgpackErrorKind::ItemLimit,
    );
    assert_kind(
        &[0x91, 0xc9, 0xff, 0xff, 0xff, 0xff],
        StoredMsgpackProfile::Row,
        StoredMsgpackErrorKind::Truncated,
    );
}

#[test]
fn structural_ext_and_utf8_semantics_are_separate() {
    validate_exact_envelope(&[0x91, 0xc7, 0x00, 0x80], StoredMsgpackProfile::Row).unwrap();

    let invalid_utf8 = [0x91, 0xa1, 0xff];
    validate_exact_envelope(&invalid_utf8, StoredMsgpackProfile::Row).unwrap();
    assert_eq!(
        decode_exact::<Vec<String>>(&invalid_utf8, StoredMsgpackProfile::Row)
            .unwrap_err()
            .kind(),
        StoredMsgpackErrorKind::DecodeFailed
    );
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct Fixture {
    name: String,
    count: u64,
}

#[test]
fn canonical_current_encodings_and_exact_decode() {
    let fixture = Fixture {
        name: "node".to_owned(),
        count: 7,
    };

    let compact = rmp_serde::to_vec(&fixture).unwrap();
    require_canonical(&fixture, &compact, CanonicalMode::Compact).unwrap();
    let decoded: Fixture = decode_exact(&compact, StoredMsgpackProfile::RelationCatalog).unwrap();
    assert_eq!(decoded, fixture);

    let mut struct_map = Vec::new();
    fixture
        .serialize(&mut rmp_serde::Serializer::new(&mut struct_map).with_struct_map())
        .unwrap();
    require_canonical(&fixture, &struct_map, CanonicalMode::StructMap).unwrap();
    let decoded: Fixture =
        decode_exact(&struct_map, StoredMsgpackProfile::RelationCatalog).unwrap();
    assert_eq!(decoded, fixture);
}

#[test]
fn exact_decode_rejects_a_typed_decoder_that_leaves_part_of_the_root() {
    #[derive(Debug)]
    struct PrefixOnly;

    impl<'de> Deserialize<'de> for PrefixOnly {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            struct PrefixVisitor;

            impl<'de> Visitor<'de> for PrefixVisitor {
                type Value = PrefixOnly;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("a value intentionally left for the EOF probe")
                }

                fn visit_some<D>(self, _value: D) -> Result<Self::Value, D::Error>
                where
                    D: serde::Deserializer<'de>,
                {
                    Ok(PrefixOnly)
                }
            }

            deserializer.deserialize_option(PrefixVisitor)
        }
    }

    // This deliberately malicious visitor accepts `Some` without asking its
    // nested deserializer to consume the cached root marker. The EOF probe
    // must reject the otherwise-successful semantic prefix decode.
    let encoded = [0x90];
    assert_eq!(
        decode_exact::<PrefixOnly>(&encoded, StoredMsgpackProfile::RelationCatalog)
            .unwrap_err()
            .kind(),
        StoredMsgpackErrorKind::DecodeDidNotConsumeEnvelope
    );
}

#[test]
fn canonical_check_rejects_reordering_unknown_duplicates_and_nonminimal_numbers() {
    let fixture = Fixture {
        name: "x".to_owned(),
        count: 1,
    };

    // positional fields reversed
    let positional_reordered = [0x92, 0x01, 0xa1, b'x'];
    assert_eq!(
        require_canonical(&fixture, &positional_reordered, CanonicalMode::Compact)
            .unwrap_err()
            .kind(),
        StoredMsgpackErrorKind::CanonicalMismatch
    );

    let cases: &[&[u8]] = &[
        // reordered map fields
        &[
            0x82, 0xa5, b'c', b'o', b'u', b'n', b't', 0x01, 0xa4, b'n', b'a', b'm', b'e', 0xa1,
            b'x',
        ],
        // unknown field
        &[
            0x83, 0xa4, b'n', b'a', b'm', b'e', 0xa1, b'x', 0xa5, b'c', b'o', b'u', b'n', b't',
            0x01, 0xa1, b'z', 0xc0,
        ],
        // duplicate field
        &[
            0x83, 0xa4, b'n', b'a', b'm', b'e', 0xa1, b'x', 0xa5, b'c', b'o', b'u', b'n', b't',
            0x01, 0xa5, b'c', b'o', b'u', b'n', b't', 0x01,
        ],
        // uint8 for a value whose minimal representation is positive fixint
        &[
            0x82, 0xa4, b'n', b'a', b'm', b'e', 0xa1, b'x', 0xa5, b'c', b'o', b'u', b'n', b't',
            0xcc, 0x01,
        ],
    ];
    for encoded in cases {
        assert!(require_canonical(&fixture, encoded, CanonicalMode::StructMap).is_err());
    }
}

#[test]
fn exact_writer_distinguishes_prefix_mismatch_and_overrun() {
    let mut exact = ExactBytesWriter::new(b"abc");
    exact.write_all(b"a").unwrap();
    exact.write_all(b"bc").unwrap();
    assert_eq!(exact.bytes_written(), 3);
    exact.finish().unwrap();

    let mut prefix = ExactBytesWriter::new(b"abc");
    prefix.write_all(b"ab").unwrap();
    assert_eq!(
        prefix.finish().unwrap_err().kind(),
        StoredMsgpackErrorKind::CanonicalLengthMismatch
    );

    let mut mismatch = ExactBytesWriter::new(b"abc");
    mismatch.write_all(b"axc").unwrap();
    let error = mismatch.finish().unwrap_err();
    assert_eq!(error.kind(), StoredMsgpackErrorKind::CanonicalMismatch);
    assert_eq!(error.offset(), 1);

    let mut overrun = ExactBytesWriter::new(b"abc");
    overrun.write_all(b"abcd").unwrap();
    let error = overrun.finish().unwrap_err();
    assert_eq!(error.kind(), StoredMsgpackErrorKind::CanonicalMismatch);
    assert_eq!(error.offset(), 3);
}

#[test]
fn arbitrary_bytes_are_deterministic_and_never_unwind() {
    let mut state = 0x4d_4e_45_4d_45_u64;
    let mut bytes = [0u8; 257];
    for length in 0..=bytes.len() {
        for byte in &mut bytes[..length] {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        let input = &bytes[..length];
        let first =
            std::panic::catch_unwind(|| validate_exact_envelope(input, StoredMsgpackProfile::Row))
                .expect("scanner must not unwind");
        let second = validate_exact_envelope(input, StoredMsgpackProfile::Row);
        assert_eq!(first, second);
    }
}

#[test]
fn error_fields_are_structured_counters() {
    let error = validate_exact_envelope(&[0x91], StoredMsgpackProfile::Row).unwrap_err();
    assert_eq!(error.kind(), StoredMsgpackErrorKind::Truncated);
    assert_eq!(error.offset(), 1);
    assert_eq!(error.limit(), 1);
    assert_eq!(error.observed(), 0);
}
