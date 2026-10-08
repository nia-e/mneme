use std::fmt::Write as _;

use cozo::{DataValue, Num, Vector};
use serde::de::{
    IntoDeserializer, SeqAccess,
    value::{BorrowedBytesDeserializer, Error},
};
use serde::{Deserialize, Deserializer};

use super::codec::{encode_key_component, encode_position, encode_value_tuple};
use super::*;

fn string(value: &str) -> DataValue {
    DataValue::Str(value.into())
}

fn integer(value: i64) -> DataValue {
    DataValue::Num(Num::Int(value))
}

fn float(value: f64) -> DataValue {
    DataValue::Num(Num::Float(value))
}

fn vector_from_bytes(tag: u8, bytes: &[u8]) -> Vector {
    struct Wire<'a> {
        tag: u8,
        bytes: &'a [u8],
    }

    struct Parts<'a> {
        tag: u8,
        bytes: &'a [u8],
        index: u8,
    }

    impl<'de> SeqAccess<'de> for Parts<'de> {
        type Error = Error;

        fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
        where
            T: serde::de::DeserializeSeed<'de>,
        {
            let value = match self.index {
                0 => seed.deserialize(self.tag.into_deserializer()).map(Some),
                1 => seed
                    .deserialize(BorrowedBytesDeserializer::new(self.bytes))
                    .map(Some),
                _ => return Ok(None),
            };
            self.index += 1;
            value
        }
    }

    impl<'de> Deserializer<'de> for Wire<'de> {
        type Error = Error;

        fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: serde::de::Visitor<'de>,
        {
            self.deserialize_tuple(2, visitor)
        }

        fn deserialize_tuple<V>(self, _length: usize, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: serde::de::Visitor<'de>,
        {
            visitor.visit_seq(Parts {
                tag: self.tag,
                bytes: self.bytes,
                index: 0,
            })
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple_struct
            map struct enum identifier ignored_any
        }
    }

    Vector::deserialize(Wire { tag, bytes }).expect("test vector must decode")
}

fn f32_vector(bits: &[u32]) -> DataValue {
    let bytes = bits
        .iter()
        .flat_map(|bits| f32::from_bits(*bits).to_ne_bytes())
        .collect::<Vec<_>>();
    DataValue::Vec(vector_from_bytes(0, &bytes))
}

fn f64_vector(bits: &[u64]) -> DataValue {
    let bytes = bits
        .iter()
        .flat_map(|bits| f64::from_bits(*bits).to_ne_bytes())
        .collect::<Vec<_>>();
    DataValue::Vec(vector_from_bytes(1, &bytes))
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
}

fn encode(relation: u16, key: &[DataValue], values: &[DataValue]) -> EncodedSnapshotRecord {
    encode_snapshot_record(relation, key, values).unwrap()
}

fn finish_empty_relations(builder: &mut LogicalSnapshotCommitmentBuilder, first_ordinal: u16) {
    for ordinal in first_ordinal..=LAST_BASE_RELATION_ORDINAL {
        builder.begin_relation(ordinal, 0).unwrap();
        builder.finish_relation().unwrap();
    }
}

#[test]
fn every_published_logical_codec_vector_is_literal_and_pinned() {
    let meta_empty = encode_position(base_relation(1).unwrap(), &[string("")]).unwrap();
    let meta_a = encode_position(base_relation(1).unwrap(), &[string("a")]).unwrap();
    let meta_nul = encode_position(base_relation(1).unwrap(), &[string("a\0b")]).unwrap();
    assert_eq!(meta_empty.to_string(), "000101010000");
    assert_eq!(meta_a.to_string(), "00010101610000");
    assert_eq!(meta_nul.to_string(), "000101016100ff620000");

    let integer_column = &base_relation(4).unwrap().relation.columns[2];
    for (value, expected) in [
        (-1, "027fffffffffffffff"),
        (0, "028000000000000000"),
        (1, "028000000000000001"),
    ] {
        let mut encoded = Vec::new();
        encode_key_component(&mut encoded, 4, integer_column, &integer(value)).unwrap();
        assert_eq!(hex(&encoded), expected);
    }

    let node_id = string("00000000000000000000000001");
    let tag_v2 = encode(
        4,
        &[
            string("tag"),
            string("active"),
            integer(-2_339_287_341_433_096_402),
            node_id.clone(),
        ],
        &[],
    );
    assert_eq!(
        tag_v2.commitment().position().to_string(),
        "000404017461670000016163746976650000025f892e8e9a4ff72e0130303030303030303030303030303030303030303030303030310000"
    );
    assert_eq!(
        tag_v2.commitment().digest().to_string(),
        "390a03abd5c01dd8ca943b3d6d0978af432e95277bbf4dcd81727df16475e410"
    );

    let meta_k_empty = encode(1, &[string("k")], &[string("")]);
    assert_eq!(
        meta_k_empty.commitment().digest().to_string(),
        "b19e83dd00e2b391f8ed1e6ceaa867a67f136aceec26f5ebb3da612715dfbfe4"
    );

    let nullable_key = [string("a"), string("b")];
    let nullable_null = encode(
        9,
        &nullable_key,
        &[integer(1), integer(2), integer(3), DataValue::Null],
    );
    let nullable_empty = encode(
        9,
        &nullable_key,
        &[integer(1), integer(2), integer(3), string("")],
    );
    assert_eq!(
        nullable_null.commitment().digest().to_string(),
        "fd9088c28c88b5d06c574c16cd892d1b78b5396f48d621bb4904a795e6ded6a3"
    );
    assert_eq!(
        nullable_empty.commitment().digest().to_string(),
        "a9b451c7e213be524c965eeb12fb95f280fe2d6623d5598e617c91e55be0dd04"
    );

    let vector = encode(
        6,
        std::slice::from_ref(&node_id),
        &[
            f32_vector(&[0x0000_0000, 0x8000_0000, 0x7fc0_1234]),
            string("active"),
        ],
    );
    assert_eq!(
        vector.commitment().digest().to_string(),
        "fcf60fb3acc1aac20c95b8e6e4f26627e5519bfa489811908678a99446ec4628"
    );

    let remote = encode(
        13,
        &[
            node_id,
            string("00000000000000000000000002"),
            string("00000000000000000000000003"),
        ],
        &[float(0.5)],
    );
    assert_eq!(
        remote.commitment().digest().to_string(),
        "b7ccaaf8f29e81ff07e22bbcfe8cbed81969474221085f22269ab2c850c97182"
    );

    let retry_order = encode(
        15,
        &[string("epoch"), integer(1), string("key")],
        &[DataValue::Bool(true)],
    );
    assert_eq!(
        retry_order.commitment().digest().to_string(),
        "03b68a432fc40cdf5a77afd475255e9e106672172ffed6203a596278a1335ed2"
    );

    let meta_x = encode(1, &[string("a")], &[string("x")]);
    let meta_y = encode(1, &[string("b")], &[string("y")]);
    let mut builder = LogicalSnapshotCommitmentBuilder::new();
    builder.begin_relation(1, 2).unwrap();
    builder.push_record(&meta_x).unwrap();
    builder.push_record(&meta_y).unwrap();
    let meta_relation = builder.finish_relation().unwrap();
    assert_eq!(
        meta_relation.digest().to_string(),
        "2b73983c6d9f9dfd17335e7d51b2dab3e5c8b487d61beb008c3d9ed5c6d26d4a"
    );
    finish_empty_relations(&mut builder, 2);
    let store = builder.finish().unwrap();
    assert_eq!(
        store.digest().to_string(),
        "615f115cab07f05debcd39baaae0ad3d22c244eb2476845b7f1f721058f1ff6f"
    );

    let mut empty = LogicalSnapshotCommitmentBuilder::new();
    empty.begin_relation(1, 0).unwrap();
    let empty_meta = empty.finish_relation().unwrap();
    assert_eq!(
        empty_meta.digest().to_string(),
        "fab60c7450e52e681ccbc3cd4ae9dc0d936c60459f3905b44231d21a289d25c8"
    );
    finish_empty_relations(&mut empty, 2);
    let empty_store = empty.finish().unwrap();
    assert_eq!(
        empty_store.digest().to_string(),
        "86acf9aa567df6e35951f7b29fccc829afca699ad854fd9800b03bcec82ce49e"
    );
    assert_eq!(empty_store.total_record_count(), 0);
    assert_eq!(empty_store.relations().len(), 16);
    for (index, relation) in empty_store.relations().iter().enumerate() {
        assert_eq!(relation.ordinal(), index as u16 + 1);
        assert_eq!(relation.record_count(), 0);
    }
}

#[test]
fn typed_tuples_reject_wrong_relations_arities_and_types() {
    for ordinal in [0, 17] {
        assert!(matches!(
            encode_snapshot_record(ordinal, &[], &[]),
            Err(SnapshotCodecError::UnsupportedRelation { ordinal: actual })
                if actual == ordinal
        ));
    }
    assert!(matches!(
        encode_snapshot_record(1, &[], &[string("v")]),
        Err(SnapshotCodecError::WrongArity {
            tuple: SnapshotTupleKind::Key,
            expected: 1,
            actual: 0,
            ..
        })
    ));
    assert!(matches!(
        encode_snapshot_record(1, &[string("k")], &[]),
        Err(SnapshotCodecError::WrongArity {
            tuple: SnapshotTupleKind::Value,
            expected: 1,
            actual: 0,
            ..
        })
    ));
    assert!(matches!(
        encode_snapshot_record(1, &[integer(1)], &[string("v")]),
        Err(SnapshotCodecError::WrongColumnType { column: "k", .. })
    ));
    assert!(matches!(
        encode_snapshot_record(1, &[string("k")], &[DataValue::Null]),
        Err(SnapshotCodecError::WrongColumnType { column: "v", .. })
    ));
    assert!(matches!(
        encode_snapshot_record(
            15,
            &[string("epoch"), integer(1), string("key")],
            &[integer(1)]
        ),
        Err(SnapshotCodecError::WrongColumnType {
            column: "marker",
            ..
        })
    ));
    assert!(matches!(
        encode_snapshot_record(
            6,
            &[string("id")],
            &[f64_vector(&[1.0_f64.to_bits()]), string("active")]
        ),
        Err(SnapshotCodecError::WrongColumnType { column: "e", .. })
    ));
}

#[test]
fn scalar_floats_reject_nonfinite_negative_zero_and_lossy_f32_values() {
    let key = [string("from"), string("db"), string("target")];
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(matches!(
            encode_snapshot_record(13, &key, &[float(value)]),
            Err(SnapshotCodecError::NonFiniteScalarFloat { .. })
        ));
    }
    assert!(matches!(
        encode_snapshot_record(13, &key, &[float(-0.0)]),
        Err(SnapshotCodecError::NegativeZeroScalarFloat { .. })
    ));
    assert!(matches!(
        encode_snapshot_record(13, &key, &[float(16_777_217.0)]),
        Err(SnapshotCodecError::NonRoundTrippingScalarFloat { .. })
    ));
    assert!(matches!(
        encode_snapshot_record(13, &key, &[integer(0)]),
        Err(SnapshotCodecError::WrongColumnType { .. })
    ));
    encode_snapshot_record(13, &key, &[float(0.0)]).unwrap();
    encode_snapshot_record(13, &key, &[float(-0.5)]).unwrap();
}

#[test]
fn canonical_key_value_and_vector_caps_are_exact() {
    // 1 arity + 1 tag + N bytes + 2 terminator = 2,376.
    let maximum_key = "x".repeat(MAX_CANONICAL_PRIMARY_KEY_BYTES - 4);
    let position = encode_position(base_relation(1).unwrap(), &[string(&maximum_key)]).unwrap();
    assert_eq!(
        position.primary_key().len(),
        MAX_CANONICAL_PRIMARY_KEY_BYTES
    );
    assert_eq!(position.as_bytes().len(), MAX_CANONICAL_POSITION_BYTES);
    let oversized_key = format!("{maximum_key}x");
    assert!(matches!(
        encode_position(base_relation(1).unwrap(), &[string(&oversized_key)]),
        Err(SnapshotCodecError::CanonicalPrimaryKeyTooLarge { .. })
    ));

    // 1 arity + 1 tag + 4 payload length + N bytes = 2,097,152.
    let maximum_value = "x".repeat(MAX_CANONICAL_VALUE_TUPLE_BYTES - 6);
    let value = encode_value_tuple(base_relation(1).unwrap(), &[string(&maximum_value)]).unwrap();
    assert_eq!(value.len(), MAX_CANONICAL_VALUE_TUPLE_BYTES);
    let oversized_value = format!("{maximum_value}x");
    assert!(matches!(
        encode_value_tuple(base_relation(1).unwrap(), &[string(&oversized_value)]),
        Err(SnapshotCodecError::CanonicalValueTupleTooLarge { .. })
    ));

    let maximum_vector = vec![0_u32; MAX_SNAPSHOT_VECTOR_DIMENSION];
    encode_snapshot_record(
        6,
        &[string("id")],
        &[f32_vector(&maximum_vector), string("active")],
    )
    .unwrap();
    assert!(matches!(
        encode_snapshot_record(6, &[string("id")], &[f32_vector(&[]), string("active")]),
        Err(SnapshotCodecError::VectorDimensionOutOfRange { actual: 0, .. })
    ));
    let oversized_vector = vec![0_u32; MAX_SNAPSHOT_VECTOR_DIMENSION + 1];
    assert!(matches!(
        encode_snapshot_record(
            6,
            &[string("id")],
            &[f32_vector(&oversized_vector), string("active")]
        ),
        Err(SnapshotCodecError::VectorDimensionOutOfRange { actual: 4_097, .. })
    ));
}

#[test]
fn builder_rejects_malformed_counts_relation_sequence_and_record_order() {
    let mut exact_cap = LogicalSnapshotCommitmentBuilder::new();
    exact_cap
        .begin_relation(1, MAX_LOGICAL_SNAPSHOT_RECORDS)
        .unwrap();
    let mut cap_plus_one = LogicalSnapshotCommitmentBuilder::new();
    assert!(matches!(
        cap_plus_one.begin_relation(1, MAX_LOGICAL_SNAPSHOT_RECORDS + 1),
        Err(SnapshotCodecError::RecordLimitExceeded {
            actual,
            limit: MAX_LOGICAL_SNAPSHOT_RECORDS,
        }) if actual == MAX_LOGICAL_SNAPSHOT_RECORDS + 1
    ));

    let mut builder = LogicalSnapshotCommitmentBuilder::new();
    for ordinal in [0, 17] {
        assert!(matches!(
            builder.begin_relation(ordinal, 0),
            Err(SnapshotCodecError::UnsupportedRelation { .. })
        ));
    }
    assert!(matches!(
        builder.begin_relation(2, 0),
        Err(SnapshotCodecError::UnexpectedRelation {
            expected: 1,
            actual: 2
        })
    ));
    builder.begin_relation(1, 0).unwrap();
    assert!(matches!(
        builder.begin_relation(1, 0),
        Err(SnapshotCodecError::RelationAlreadyOpen { ordinal: 1 })
    ));
    builder.finish_relation().unwrap();
    assert!(matches!(
        builder.begin_relation(1, 0),
        Err(SnapshotCodecError::UnexpectedRelation {
            expected: 2,
            actual: 1
        })
    ));

    let mut incomplete = LogicalSnapshotCommitmentBuilder::new();
    incomplete.begin_relation(1, 0).unwrap();
    incomplete.finish_relation().unwrap();
    assert!(matches!(
        incomplete.finish(),
        Err(SnapshotCodecError::IncompleteRelationInventory { next_expected: 2 })
    ));

    let mut count_mismatch = LogicalSnapshotCommitmentBuilder::new();
    count_mismatch.begin_relation(1, 1).unwrap();
    assert!(matches!(
        count_mismatch.finish_relation(),
        Err(SnapshotCodecError::RelationCountMismatch {
            declared: 1,
            observed: 0,
            ..
        })
    ));

    let record_a = encode(1, &[string("a")], &[string("x")]);
    let record_b = encode(1, &[string("b")], &[string("y")]);
    let mut duplicate = LogicalSnapshotCommitmentBuilder::new();
    duplicate.begin_relation(1, 2).unwrap();
    duplicate.push_record(&record_a).unwrap();
    assert!(matches!(
        duplicate.push_record(&record_a),
        Err(SnapshotCodecError::DuplicatePosition { .. })
    ));

    let mut decreasing = LogicalSnapshotCommitmentBuilder::new();
    decreasing.begin_relation(1, 2).unwrap();
    decreasing.push_record(&record_b).unwrap();
    assert!(matches!(
        decreasing.push_record(&record_a),
        Err(SnapshotCodecError::DecreasingPosition { .. })
    ));

    let wrong_relation = encode(2, &[string("id")], &[string("{}"), string("active")]);
    let mut wrong_segment = LogicalSnapshotCommitmentBuilder::new();
    wrong_segment.begin_relation(1, 1).unwrap();
    assert!(matches!(
        wrong_segment.push_record(&wrong_relation),
        Err(SnapshotCodecError::RecordBelongsToWrongRelation {
            expected: 1,
            actual: 2
        })
    ));

    let mut exceeded = LogicalSnapshotCommitmentBuilder::new();
    exceeded.begin_relation(1, 0).unwrap();
    assert!(matches!(
        exceeded.push_record(&record_a),
        Err(SnapshotCodecError::RelationCountExceeded {
            ordinal: 1,
            declared: 0
        })
    ));

    let mut accumulated_cap = LogicalSnapshotCommitmentBuilder::new();
    accumulated_cap.begin_relation(1, 1).unwrap();
    accumulated_cap.push_record(&record_a).unwrap();
    accumulated_cap.finish_relation().unwrap();
    assert!(matches!(
        accumulated_cap.begin_relation(2, MAX_LOGICAL_SNAPSHOT_RECORDS),
        Err(SnapshotCodecError::RecordLimitExceeded {
            actual,
            limit: MAX_LOGICAL_SNAPSHOT_RECORDS,
        }) if actual == MAX_LOGICAL_SNAPSHOT_RECORDS + 1
    ));

    let mut declared_overflow = LogicalSnapshotCommitmentBuilder::new();
    declared_overflow.begin_relation(1, 1).unwrap();
    declared_overflow.push_record(&record_a).unwrap();
    declared_overflow.finish_relation().unwrap();
    assert!(matches!(
        declared_overflow.begin_relation(2, u64::MAX),
        Err(SnapshotCodecError::RecordLimitExceeded {
            actual: u64::MAX,
            limit: MAX_LOGICAL_SNAPSHOT_RECORDS,
        })
    ));
}
