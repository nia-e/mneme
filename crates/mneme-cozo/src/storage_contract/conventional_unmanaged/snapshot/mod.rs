//! Canonical logical-store commitments and their closed SQLite visit boundary.
//!
//! The codec and commitment layers remain pure: they consume already-decoded
//! [`cozo::DataValue`] tuples and bind them to the frozen base-relation spec.
//! The visitor layer adapts Mnestic's bounded one-row-at-a-time API, but does
//! not create a backup, frame an artifact, interpret node semantics, or publish
//! a managed generation.

mod codec;
mod commitment;
mod roles;
mod visitor;

#[cfg(test)]
mod tests;

use cozo::DataValue;
use mneme_core::managed::{ManagedSnapshotDigest, ManagedSnapshotRecordCommitment};

pub(crate) use codec::{
    EncodedSnapshotRecord, SnapshotCodecError, SnapshotTupleKind, encode_snapshot_record,
};
pub(crate) use commitment::{
    LogicalRelationCommitment, LogicalSnapshotCommitmentBuilder, LogicalStoreCommitment,
};
pub(crate) use visitor::logical::{ClosedLogicalSnapshotVisitV1, close_logical_snapshot_visit_v1};

use super::spec::{BASE_RELATIONS, BaseRelationSpec};

const BASE_RELATION_COUNT: u16 = 16;
const FIRST_BASE_RELATION_ORDINAL: u16 = 1;
const LAST_BASE_RELATION_ORDINAL: u16 = 16;

pub(crate) const MAX_CANONICAL_PRIMARY_KEY_BYTES: usize = 2_376;
pub(crate) const MAX_CANONICAL_POSITION_BYTES: usize = 2_378;
pub(crate) const MAX_CANONICAL_VALUE_TUPLE_BYTES: usize = 2_097_152;
pub(crate) const MAX_SNAPSHOT_VECTOR_DIMENSION: usize = 4_096;
pub(crate) const MAX_LOGICAL_SNAPSHOT_RECORDS: u64 = 1 << 24;

const _: () = assert!(BASE_RELATIONS.len() == BASE_RELATION_COUNT as usize);
const _: () = assert!(
    MAX_CANONICAL_PRIMARY_KEY_BYTES <= mneme_core::managed::MAX_MANAGED_SNAPSHOT_PRIMARY_KEY_BYTES
);
const _: () =
    assert!(MAX_CANONICAL_POSITION_BYTES == size_of::<u16>() + MAX_CANONICAL_PRIMARY_KEY_BYTES);
const _: () = assert!(MAX_CANONICAL_POSITION_BYTES <= u16::MAX as usize);

fn base_relation(ordinal: u16) -> Result<&'static BaseRelationSpec, SnapshotCodecError> {
    if !(FIRST_BASE_RELATION_ORDINAL..=LAST_BASE_RELATION_ORDINAL).contains(&ordinal) {
        return Err(SnapshotCodecError::UnsupportedRelation { ordinal });
    }
    let relation = &BASE_RELATIONS[usize::from(ordinal - 1)];
    if relation.ordinal as u16 != ordinal {
        return Err(SnapshotCodecError::UnsupportedRelation { ordinal });
    }
    Ok(relation)
}

// The pure codec intentionally lands before the separately scoped raw-row
// visitor. Root only that staged integration surface while leaving dead-code
// linting active for unrelated additions to this module.
type EncodeSnapshotRecordFn =
    fn(u16, &[DataValue], &[DataValue]) -> Result<EncodedSnapshotRecord, SnapshotCodecError>;

const _: EncodeSnapshotRecordFn = encode_snapshot_record;
const _: fn(&EncodedSnapshotRecord) -> &ManagedSnapshotRecordCommitment =
    EncodedSnapshotRecord::commitment;
const _: Option<SnapshotTupleKind> = None;
const _: fn() -> LogicalSnapshotCommitmentBuilder = LogicalSnapshotCommitmentBuilder::new;
const _: fn(&mut LogicalSnapshotCommitmentBuilder, u16, u64) -> Result<(), SnapshotCodecError> =
    LogicalSnapshotCommitmentBuilder::begin_relation;
const _: fn(
    &mut LogicalSnapshotCommitmentBuilder,
    &EncodedSnapshotRecord,
) -> Result<(), SnapshotCodecError> = LogicalSnapshotCommitmentBuilder::push_record;
const _: fn(
    &mut LogicalSnapshotCommitmentBuilder,
) -> Result<LogicalRelationCommitment, SnapshotCodecError> =
    LogicalSnapshotCommitmentBuilder::finish_relation;
const _: fn(
    LogicalSnapshotCommitmentBuilder,
) -> Result<LogicalStoreCommitment, SnapshotCodecError> = LogicalSnapshotCommitmentBuilder::finish;
const _: fn(&LogicalRelationCommitment) -> u16 = LogicalRelationCommitment::ordinal;
const _: fn(&LogicalRelationCommitment) -> u64 = LogicalRelationCommitment::record_count;
const _: fn(&LogicalRelationCommitment) -> ManagedSnapshotDigest =
    LogicalRelationCommitment::digest;
const _: fn(&LogicalStoreCommitment) -> &[LogicalRelationCommitment; BASE_RELATION_COUNT as usize] =
    LogicalStoreCommitment::relations;
const _: fn(&LogicalStoreCommitment) -> u64 = LogicalStoreCommitment::total_record_count;
const _: fn(&LogicalStoreCommitment) -> ManagedSnapshotDigest = LogicalStoreCommitment::digest;
const _: fn(
    cozo::ManagedSqliteSnapshotReader,
) -> Result<ClosedLogicalSnapshotVisitV1, cozo::Error> = close_logical_snapshot_visit_v1;
