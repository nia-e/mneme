use std::fmt;

use cozo::{DataValue, Num, Vector};
use mneme_core::managed::{
    ManagedSnapshotDigest, ManagedSnapshotPosition, ManagedSnapshotRecordCommitment,
};
use sha2::{Digest, Sha256};

use super::super::spec::{BaseRelationSpec, ColumnSpec, ColumnType};
use super::{
    MAX_CANONICAL_POSITION_BYTES, MAX_CANONICAL_PRIMARY_KEY_BYTES, MAX_CANONICAL_VALUE_TUPLE_BYTES,
    MAX_SNAPSHOT_VECTOR_DIMENSION, base_relation,
};

const RECORD_DIGEST_DOMAIN: &[u8] = b"mneme-managed-snapshot-record-v1\0";

const KEY_STRING_TAG: u8 = 1;
const KEY_INTEGER_TAG: u8 = 2;
const VALUE_NULL_TAG: u8 = 0;
const VALUE_STRING_TAG: u8 = 1;
const VALUE_BOOL_TAG: u8 = 2;
const VALUE_INTEGER_TAG: u8 = 3;
const VALUE_FLOAT_TAG: u8 = 4;
const VALUE_VECTOR_TAG: u8 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotTupleKind {
    Key,
    Value,
}

impl fmt::Display for SnapshotTupleKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key => formatter.write_str("key"),
            Self::Value => formatter.write_str("value"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotCodecError {
    UnsupportedRelation {
        ordinal: u16,
    },
    WrongArity {
        relation: u16,
        tuple: SnapshotTupleKind,
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        relation: u16,
        column: &'static str,
        expected: ColumnType,
        actual: &'static str,
    },
    UnsupportedBaseColumnType {
        relation: u16,
        column: &'static str,
        ty: ColumnType,
    },
    NonFiniteScalarFloat {
        relation: u16,
        column: &'static str,
    },
    NegativeZeroScalarFloat {
        relation: u16,
        column: &'static str,
    },
    NonRoundTrippingScalarFloat {
        relation: u16,
        column: &'static str,
    },
    VectorDimensionOutOfRange {
        relation: u16,
        column: &'static str,
        actual: usize,
    },
    CanonicalPrimaryKeyTooLarge {
        actual_at_least: usize,
        limit: usize,
    },
    CanonicalValueTupleTooLarge {
        actual: usize,
        limit: usize,
    },
    CanonicalPositionTooLarge {
        actual: usize,
        limit: usize,
    },
    RecordLimitExceeded {
        actual: u64,
        limit: u64,
    },
    RelationAlreadyOpen {
        ordinal: u16,
    },
    NoOpenRelation,
    UnexpectedRelation {
        expected: u16,
        actual: u16,
    },
    RecordBelongsToWrongRelation {
        expected: u16,
        actual: u16,
    },
    DuplicatePosition {
        position: ManagedSnapshotPosition,
    },
    DecreasingPosition {
        previous: ManagedSnapshotPosition,
        actual: ManagedSnapshotPosition,
    },
    RelationCountExceeded {
        ordinal: u16,
        declared: u64,
    },
    RelationCountMismatch {
        ordinal: u16,
        declared: u64,
        observed: u64,
    },
    TotalRecordCountMismatch {
        declared_relation_total: u64,
        observed_total: u64,
    },
    IncompleteRelationInventory {
        next_expected: u16,
    },
}

impl fmt::Display for SnapshotCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedRelation { ordinal } => {
                write!(
                    formatter,
                    "snapshot relation ordinal {ordinal} is not in 1..=16"
                )
            }
            Self::WrongArity {
                relation,
                tuple,
                expected,
                actual,
            } => write!(
                formatter,
                "snapshot relation {relation} requires {expected} {tuple} components, got {actual}"
            ),
            Self::WrongColumnType {
                relation,
                column,
                expected,
                actual,
            } => write!(
                formatter,
                "snapshot relation {relation} column {column:?} requires {expected:?}, got {actual}"
            ),
            Self::UnsupportedBaseColumnType {
                relation,
                column,
                ty,
            } => write!(
                formatter,
                "snapshot relation {relation} column {column:?} has unsupported base type {ty:?}"
            ),
            Self::NonFiniteScalarFloat { relation, column } => write!(
                formatter,
                "snapshot relation {relation} scalar float column {column:?} must be finite"
            ),
            Self::NegativeZeroScalarFloat { relation, column } => write!(
                formatter,
                "snapshot relation {relation} scalar float column {column:?} must not be negative zero"
            ),
            Self::NonRoundTrippingScalarFloat { relation, column } => write!(
                formatter,
                "snapshot relation {relation} scalar float column {column:?} must round-trip exactly through f32"
            ),
            Self::VectorDimensionOutOfRange {
                relation,
                column,
                actual,
            } => write!(
                formatter,
                "snapshot relation {relation} vector column {column:?} has dimension {actual}, expected 1..={MAX_SNAPSHOT_VECTOR_DIMENSION}"
            ),
            Self::CanonicalPrimaryKeyTooLarge {
                actual_at_least,
                limit,
            } => write!(
                formatter,
                "canonical snapshot primary key is at least {actual_at_least} bytes, limit {limit}"
            ),
            Self::CanonicalValueTupleTooLarge { actual, limit } => write!(
                formatter,
                "canonical snapshot value tuple is {actual} bytes, limit {limit}"
            ),
            Self::CanonicalPositionTooLarge { actual, limit } => write!(
                formatter,
                "canonical snapshot position is {actual} bytes, limit {limit}"
            ),
            Self::RecordLimitExceeded { actual, limit } => write!(
                formatter,
                "logical snapshot declares {actual} records, limit {limit}"
            ),
            Self::RelationAlreadyOpen { ordinal } => {
                write!(formatter, "snapshot relation {ordinal} is already open")
            }
            Self::NoOpenRelation => formatter.write_str("no snapshot relation is open"),
            Self::UnexpectedRelation { expected, actual } => write!(
                formatter,
                "snapshot relation segments must be exactly 1..=16; expected {expected}, got {actual}"
            ),
            Self::RecordBelongsToWrongRelation { expected, actual } => write!(
                formatter,
                "snapshot record belongs to relation {actual}, but relation {expected} is open"
            ),
            Self::DuplicatePosition { position } => {
                write!(formatter, "duplicate snapshot position {position}")
            }
            Self::DecreasingPosition { previous, actual } => write!(
                formatter,
                "snapshot position {actual} follows greater position {previous}"
            ),
            Self::RelationCountExceeded { ordinal, declared } => write!(
                formatter,
                "snapshot relation {ordinal} contains more than its declared {declared} records"
            ),
            Self::RelationCountMismatch {
                ordinal,
                declared,
                observed,
            } => write!(
                formatter,
                "snapshot relation {ordinal} declared {declared} records but observed {observed}"
            ),
            Self::TotalRecordCountMismatch {
                declared_relation_total,
                observed_total,
            } => write!(
                formatter,
                "snapshot relation declarations total {declared_relation_total} records but observed {observed_total}"
            ),
            Self::IncompleteRelationInventory { next_expected } => write!(
                formatter,
                "snapshot ended before required relation segment {next_expected}"
            ),
        }
    }
}

impl std::error::Error for SnapshotCodecError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EncodedSnapshotRecord {
    commitment: ManagedSnapshotRecordCommitment,
}

impl EncodedSnapshotRecord {
    pub(crate) fn commitment(&self) -> &ManagedSnapshotRecordCommitment {
        &self.commitment
    }
}

pub(crate) fn encode_snapshot_record(
    relation_ordinal: u16,
    key: &[DataValue],
    values: &[DataValue],
) -> Result<EncodedSnapshotRecord, SnapshotCodecError> {
    let relation = base_relation(relation_ordinal)?;
    let position = encode_position(relation, key)?;
    let value_tuple = encode_value_tuple(relation, values)?;

    let position_length = u16::try_from(position.as_bytes().len()).map_err(|_| {
        SnapshotCodecError::CanonicalPositionTooLarge {
            actual: position.as_bytes().len(),
            limit: MAX_CANONICAL_POSITION_BYTES,
        }
    })?;
    let mut hasher = Sha256::new();
    hasher.update(RECORD_DIGEST_DOMAIN);
    hasher.update(position_length.to_be_bytes());
    hasher.update(position.as_bytes());
    hasher.update(&value_tuple);
    let digest = ManagedSnapshotDigest::from_bytes(hasher.finalize().into());
    Ok(EncodedSnapshotRecord {
        commitment: ManagedSnapshotRecordCommitment::new(position, digest),
    })
}

pub(super) fn encode_position(
    relation: &BaseRelationSpec,
    key: &[DataValue],
) -> Result<ManagedSnapshotPosition, SnapshotCodecError> {
    let relation_ordinal = relation.ordinal as u16;
    let expected_arity = relation.relation.key_count();
    if key.len() != expected_arity {
        return Err(SnapshotCodecError::WrongArity {
            relation: relation_ordinal,
            tuple: SnapshotTupleKind::Key,
            expected: expected_arity,
            actual: key.len(),
        });
    }

    let mut encoded = Vec::with_capacity(MAX_CANONICAL_PRIMARY_KEY_BYTES.min(64));
    encoded
        .push(u8::try_from(expected_arity).expect("the frozen base key arities fit in one byte"));
    for (column, value) in relation
        .relation
        .columns
        .iter()
        .filter(|column| column.is_key)
        .zip(key)
    {
        encode_key_component(&mut encoded, relation_ordinal, column, value)?;
    }
    if encoded.len() > MAX_CANONICAL_PRIMARY_KEY_BYTES {
        return Err(SnapshotCodecError::CanonicalPrimaryKeyTooLarge {
            actual_at_least: encoded.len(),
            limit: MAX_CANONICAL_PRIMARY_KEY_BYTES,
        });
    }

    let position_length = encoded.len() + size_of::<u16>();
    if position_length > MAX_CANONICAL_POSITION_BYTES {
        return Err(SnapshotCodecError::CanonicalPositionTooLarge {
            actual: position_length,
            limit: MAX_CANONICAL_POSITION_BYTES,
        });
    }
    ManagedSnapshotPosition::new(relation_ordinal, encoded).map_err(|_| {
        SnapshotCodecError::CanonicalPositionTooLarge {
            actual: position_length,
            limit: MAX_CANONICAL_POSITION_BYTES,
        }
    })
}

pub(super) fn encode_key_component(
    encoded: &mut Vec<u8>,
    relation: u16,
    column: &ColumnSpec,
    value: &DataValue,
) -> Result<(), SnapshotCodecError> {
    match (column.ty, value) {
        (ColumnType::String, DataValue::Str(value)) => {
            ensure_key_capacity(encoded.len(), 3)?;
            encoded.push(KEY_STRING_TAG);
            for byte in value.as_bytes() {
                let expansion = if *byte == 0 { 2 } else { 1 };
                // Reserve the two-byte terminator before copying another byte.
                ensure_key_capacity(encoded.len(), expansion + 2)?;
                if *byte == 0 {
                    encoded.extend_from_slice(&[0, 0xff]);
                } else {
                    encoded.push(*byte);
                }
            }
            encoded.extend_from_slice(&[0, 0]);
            Ok(())
        }
        (ColumnType::Int, DataValue::Num(Num::Int(value))) => {
            ensure_key_capacity(encoded.len(), 9)?;
            encoded.push(KEY_INTEGER_TAG);
            encoded.extend_from_slice(&ordered_i64(*value).to_be_bytes());
            Ok(())
        }
        (ColumnType::String | ColumnType::Int, _) => {
            Err(wrong_column_type(relation, column, value))
        }
        (ty, _) => Err(SnapshotCodecError::UnsupportedBaseColumnType {
            relation,
            column: column.name,
            ty,
        }),
    }
}

fn ensure_key_capacity(current: usize, additional: usize) -> Result<(), SnapshotCodecError> {
    let actual_at_least = current.saturating_add(additional);
    if actual_at_least > MAX_CANONICAL_PRIMARY_KEY_BYTES {
        Err(SnapshotCodecError::CanonicalPrimaryKeyTooLarge {
            actual_at_least,
            limit: MAX_CANONICAL_PRIMARY_KEY_BYTES,
        })
    } else {
        Ok(())
    }
}

pub(super) fn encode_value_tuple(
    relation: &BaseRelationSpec,
    values: &[DataValue],
) -> Result<Vec<u8>, SnapshotCodecError> {
    let relation_ordinal = relation.ordinal as u16;
    let expected_arity = relation
        .relation
        .columns
        .iter()
        .filter(|column| !column.is_key)
        .count();
    if values.len() != expected_arity {
        return Err(SnapshotCodecError::WrongArity {
            relation: relation_ordinal,
            tuple: SnapshotTupleKind::Value,
            expected: expected_arity,
            actual: values.len(),
        });
    }

    let mut encoded = Vec::with_capacity(64);
    encoded
        .push(u8::try_from(expected_arity).expect("the frozen base value arities fit in one byte"));
    for (column, value) in relation
        .relation
        .columns
        .iter()
        .filter(|column| !column.is_key)
        .zip(values)
    {
        encode_value_component(&mut encoded, relation_ordinal, column, value)?;
    }
    Ok(encoded)
}

fn encode_value_component(
    encoded: &mut Vec<u8>,
    relation: u16,
    column: &ColumnSpec,
    value: &DataValue,
) -> Result<(), SnapshotCodecError> {
    match (column.ty, value) {
        (ColumnType::NullableString, DataValue::Null) => {
            append_value_header(encoded, VALUE_NULL_TAG, 0)
        }
        (ColumnType::String | ColumnType::NullableString, DataValue::Str(value)) => {
            append_value_header(encoded, VALUE_STRING_TAG, value.len())?;
            encoded.extend_from_slice(value.as_bytes());
            Ok(())
        }
        (ColumnType::Bool, DataValue::Bool(value)) => {
            append_value_header(encoded, VALUE_BOOL_TAG, 1)?;
            encoded.push(u8::from(*value));
            Ok(())
        }
        (ColumnType::Int, DataValue::Num(Num::Int(value))) => {
            append_value_header(encoded, VALUE_INTEGER_TAG, size_of::<u64>())?;
            encoded.extend_from_slice(&ordered_i64(*value).to_be_bytes());
            Ok(())
        }
        (ColumnType::Float, DataValue::Num(Num::Float(value))) => {
            if !value.is_finite() {
                return Err(SnapshotCodecError::NonFiniteScalarFloat {
                    relation,
                    column: column.name,
                });
            }
            if *value == 0.0 && value.is_sign_negative() {
                return Err(SnapshotCodecError::NegativeZeroScalarFloat {
                    relation,
                    column: column.name,
                });
            }
            let narrowed = *value as f32;
            if f64::from(narrowed) != *value {
                return Err(SnapshotCodecError::NonRoundTrippingScalarFloat {
                    relation,
                    column: column.name,
                });
            }
            append_value_header(encoded, VALUE_FLOAT_TAG, size_of::<u32>())?;
            encoded.extend_from_slice(&narrowed.to_bits().to_be_bytes());
            Ok(())
        }
        (ColumnType::F32Vector, DataValue::Vec(Vector::F32(values))) => {
            let count = values.len();
            if !(1..=MAX_SNAPSHOT_VECTOR_DIMENSION).contains(&count) {
                return Err(SnapshotCodecError::VectorDimensionOutOfRange {
                    relation,
                    column: column.name,
                    actual: count,
                });
            }
            let payload_length = size_of::<u32>() + count * size_of::<u32>();
            append_value_header(encoded, VALUE_VECTOR_TAG, payload_length)?;
            encoded.extend_from_slice(&(count as u32).to_be_bytes());
            for value in values {
                encoded.extend_from_slice(&value.to_bits().to_be_bytes());
            }
            Ok(())
        }
        (
            ColumnType::String
            | ColumnType::NullableString
            | ColumnType::Bool
            | ColumnType::Int
            | ColumnType::Float
            | ColumnType::F32Vector,
            _,
        ) => Err(wrong_column_type(relation, column, value)),
        (ty, _) => Err(SnapshotCodecError::UnsupportedBaseColumnType {
            relation,
            column: column.name,
            ty,
        }),
    }
}

fn append_value_header(
    encoded: &mut Vec<u8>,
    tag: u8,
    payload_length: usize,
) -> Result<(), SnapshotCodecError> {
    let actual = encoded
        .len()
        .checked_add(1 + size_of::<u32>())
        .and_then(|length| length.checked_add(payload_length))
        .unwrap_or(usize::MAX);
    if actual > MAX_CANONICAL_VALUE_TUPLE_BYTES {
        return Err(SnapshotCodecError::CanonicalValueTupleTooLarge {
            actual,
            limit: MAX_CANONICAL_VALUE_TUPLE_BYTES,
        });
    }
    let payload_length = u32::try_from(payload_length).map_err(|_| {
        SnapshotCodecError::CanonicalValueTupleTooLarge {
            actual,
            limit: MAX_CANONICAL_VALUE_TUPLE_BYTES,
        }
    })?;
    encoded.push(tag);
    encoded.extend_from_slice(&payload_length.to_be_bytes());
    Ok(())
}

fn wrong_column_type(relation: u16, column: &ColumnSpec, value: &DataValue) -> SnapshotCodecError {
    SnapshotCodecError::WrongColumnType {
        relation,
        column: column.name,
        expected: column.ty,
        actual: data_value_kind(value),
    }
}

fn data_value_kind(value: &DataValue) -> &'static str {
    match value {
        DataValue::Null => "null",
        DataValue::Bool(_) => "bool",
        DataValue::Num(Num::Int(_)) => "integer",
        DataValue::Num(Num::Float(_)) => "float",
        DataValue::Str(_) => "string",
        DataValue::Bytes(_) => "bytes",
        DataValue::Uuid(_) => "uuid",
        DataValue::Regex(_) => "regex",
        DataValue::List(_) => "list",
        DataValue::Set(_) => "set",
        DataValue::Vec(Vector::F32(_)) => "f32 vector",
        DataValue::Vec(Vector::F64(_)) => "f64 vector",
        DataValue::Json(_) => "json",
        DataValue::Validity(_) => "validity",
        DataValue::Bot => "bottom",
    }
}

fn ordered_i64(value: i64) -> u64 {
    (value as u64) ^ (1_u64 << 63)
}
