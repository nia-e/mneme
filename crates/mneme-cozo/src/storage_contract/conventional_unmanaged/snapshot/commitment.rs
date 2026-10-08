use mneme_core::managed::{
    ManagedSnapshotDigest, ManagedSnapshotPosition, ManagedSnapshotRecordCommitment,
};
use sha2::{Digest, Sha256};

use super::{
    BASE_RELATION_COUNT, EncodedSnapshotRecord, FIRST_BASE_RELATION_ORDINAL,
    LAST_BASE_RELATION_ORDINAL, MAX_CANONICAL_POSITION_BYTES, MAX_LOGICAL_SNAPSHOT_RECORDS,
    SnapshotCodecError, base_relation,
};

const RELATION_DIGEST_DOMAIN: &[u8] = b"mneme-managed-snapshot-relation-v1\0";
const STORE_DIGEST_DOMAIN: &[u8] = b"mneme-managed-snapshot-store-v1\0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogicalRelationCommitment {
    ordinal: u16,
    record_count: u64,
    digest: ManagedSnapshotDigest,
}

impl LogicalRelationCommitment {
    pub(crate) const fn ordinal(&self) -> u16 {
        self.ordinal
    }

    pub(crate) const fn record_count(&self) -> u64 {
        self.record_count
    }

    pub(crate) const fn digest(&self) -> ManagedSnapshotDigest {
        self.digest
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogicalStoreCommitment {
    relations: [LogicalRelationCommitment; BASE_RELATION_COUNT as usize],
    total_record_count: u64,
    digest: ManagedSnapshotDigest,
}

impl LogicalStoreCommitment {
    pub(crate) fn relations(&self) -> &[LogicalRelationCommitment; BASE_RELATION_COUNT as usize] {
        &self.relations
    }

    pub(crate) const fn total_record_count(&self) -> u64 {
        self.total_record_count
    }

    pub(crate) const fn digest(&self) -> ManagedSnapshotDigest {
        self.digest
    }
}

struct OpenRelation {
    ordinal: u16,
    declared_count: u64,
    observed_count: u64,
    previous: Option<ManagedSnapshotPosition>,
    hasher: Sha256,
}

/// Streaming relation and whole-store commitment builder.
///
/// Callers must open and close every ordinal in `1..=16`, including empty
/// relations. Their declared counts establish the bounded store total as they
/// are opened. A record is borrowed only long enough to hash it, so the builder
/// retains bounded state independent of the number or size of records.
pub(crate) struct LogicalSnapshotCommitmentBuilder {
    declared_relation_total: u64,
    observed_total_count: u64,
    next_relation: u16,
    open_relation: Option<OpenRelation>,
    relations: Vec<LogicalRelationCommitment>,
    store_hasher: Sha256,
}

impl LogicalSnapshotCommitmentBuilder {
    pub(crate) fn new() -> Self {
        let mut store_hasher = Sha256::new();
        store_hasher.update(STORE_DIGEST_DOMAIN);
        store_hasher.update(BASE_RELATION_COUNT.to_be_bytes());
        Self {
            declared_relation_total: 0,
            observed_total_count: 0,
            next_relation: FIRST_BASE_RELATION_ORDINAL,
            open_relation: None,
            relations: Vec::with_capacity(BASE_RELATION_COUNT as usize),
            store_hasher,
        }
    }

    pub(crate) fn begin_relation(
        &mut self,
        ordinal: u16,
        declared_count: u64,
    ) -> Result<(), SnapshotCodecError> {
        base_relation(ordinal)?;
        if let Some(open) = &self.open_relation {
            return Err(SnapshotCodecError::RelationAlreadyOpen {
                ordinal: open.ordinal,
            });
        }
        if ordinal != self.next_relation {
            return Err(SnapshotCodecError::UnexpectedRelation {
                expected: self.next_relation,
                actual: ordinal,
            });
        }
        let attempted_total = self
            .declared_relation_total
            .checked_add(declared_count)
            .ok_or(SnapshotCodecError::RecordLimitExceeded {
                actual: u64::MAX,
                limit: MAX_LOGICAL_SNAPSHOT_RECORDS,
            })?;
        if attempted_total > MAX_LOGICAL_SNAPSHOT_RECORDS {
            return Err(SnapshotCodecError::RecordLimitExceeded {
                actual: attempted_total,
                limit: MAX_LOGICAL_SNAPSHOT_RECORDS,
            });
        }

        let mut relation_hasher = Sha256::new();
        relation_hasher.update(RELATION_DIGEST_DOMAIN);
        relation_hasher.update(ordinal.to_be_bytes());

        self.store_hasher.update([1]);
        self.store_hasher.update(ordinal.to_be_bytes());
        self.declared_relation_total = attempted_total;
        self.open_relation = Some(OpenRelation {
            ordinal,
            declared_count,
            observed_count: 0,
            previous: None,
            hasher: relation_hasher,
        });
        Ok(())
    }

    pub(crate) fn push_record(
        &mut self,
        record: &EncodedSnapshotRecord,
    ) -> Result<(), SnapshotCodecError> {
        let record = record.commitment();
        let open = self
            .open_relation
            .as_mut()
            .ok_or(SnapshotCodecError::NoOpenRelation)?;
        let actual_relation = record.position().relation();
        base_relation(actual_relation)?;
        if actual_relation != open.ordinal {
            return Err(SnapshotCodecError::RecordBelongsToWrongRelation {
                expected: open.ordinal,
                actual: actual_relation,
            });
        }
        if let Some(previous) = &open.previous {
            if record.position() == previous {
                return Err(SnapshotCodecError::DuplicatePosition {
                    position: record.position().clone(),
                });
            }
            if record.position() < previous {
                return Err(SnapshotCodecError::DecreasingPosition {
                    previous: previous.clone(),
                    actual: record.position().clone(),
                });
            }
        }
        if open.observed_count == open.declared_count {
            return Err(SnapshotCodecError::RelationCountExceeded {
                ordinal: open.ordinal,
                declared: open.declared_count,
            });
        }
        if self.observed_total_count == MAX_LOGICAL_SNAPSHOT_RECORDS {
            return Err(SnapshotCodecError::RecordLimitExceeded {
                actual: MAX_LOGICAL_SNAPSHOT_RECORDS + 1,
                limit: MAX_LOGICAL_SNAPSHOT_RECORDS,
            });
        }

        update_record_entry(&mut open.hasher, record)?;
        update_record_entry(&mut self.store_hasher, record)?;
        open.observed_count += 1;
        self.observed_total_count += 1;
        open.previous = Some(record.position().clone());
        Ok(())
    }

    pub(crate) fn finish_relation(
        &mut self,
    ) -> Result<LogicalRelationCommitment, SnapshotCodecError> {
        let open = self
            .open_relation
            .as_ref()
            .ok_or(SnapshotCodecError::NoOpenRelation)?;
        if open.observed_count != open.declared_count {
            return Err(SnapshotCodecError::RelationCountMismatch {
                ordinal: open.ordinal,
                declared: open.declared_count,
                observed: open.observed_count,
            });
        }

        let mut open = self
            .open_relation
            .take()
            .expect("open relation was checked above");
        open.hasher.update([0xff]);
        open.hasher.update(open.observed_count.to_be_bytes());
        let commitment = LogicalRelationCommitment {
            ordinal: open.ordinal,
            record_count: open.observed_count,
            digest: ManagedSnapshotDigest::from_bytes(open.hasher.finalize().into()),
        };

        self.store_hasher.update([3]);
        self.store_hasher.update(open.observed_count.to_be_bytes());
        self.relations.push(commitment.clone());
        self.next_relation += 1;
        Ok(commitment)
    }

    pub(crate) fn finish(mut self) -> Result<LogicalStoreCommitment, SnapshotCodecError> {
        if let Some(open) = self.open_relation {
            return Err(SnapshotCodecError::RelationAlreadyOpen {
                ordinal: open.ordinal,
            });
        }
        if self.next_relation != LAST_BASE_RELATION_ORDINAL + 1 {
            return Err(SnapshotCodecError::IncompleteRelationInventory {
                next_expected: self.next_relation,
            });
        }
        // Every closed relation proved its declared/observed equality. Keep the
        // store-level equality explicit so a future builder edit cannot weaken it.
        if self.observed_total_count != self.declared_relation_total {
            return Err(SnapshotCodecError::TotalRecordCountMismatch {
                declared_relation_total: self.declared_relation_total,
                observed_total: self.observed_total_count,
            });
        }

        self.store_hasher.update([0xff]);
        self.store_hasher
            .update(self.observed_total_count.to_be_bytes());
        let relations = self
            .relations
            .try_into()
            .expect("the exact 16-relation inventory was checked above");
        Ok(LogicalStoreCommitment {
            relations,
            total_record_count: self.observed_total_count,
            digest: ManagedSnapshotDigest::from_bytes(self.store_hasher.finalize().into()),
        })
    }
}

fn update_record_entry(
    hasher: &mut Sha256,
    record: &ManagedSnapshotRecordCommitment,
) -> Result<(), SnapshotCodecError> {
    let position = record.position().as_bytes();
    if position.len() > MAX_CANONICAL_POSITION_BYTES {
        return Err(SnapshotCodecError::CanonicalPositionTooLarge {
            actual: position.len(),
            limit: MAX_CANONICAL_POSITION_BYTES,
        });
    }
    let position_length = u16::try_from(position.len()).map_err(|_| {
        SnapshotCodecError::CanonicalPositionTooLarge {
            actual: position.len(),
            limit: MAX_CANONICAL_POSITION_BYTES,
        }
    })?;
    hasher.update([2]);
    hasher.update(position_length.to_be_bytes());
    hasher.update(position);
    hasher.update(record.digest().as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_rejects_observed_total_mismatch() {
        let mut builder = LogicalSnapshotCommitmentBuilder::new();
        for ordinal in FIRST_BASE_RELATION_ORDINAL..=LAST_BASE_RELATION_ORDINAL {
            builder.begin_relation(ordinal, 0).unwrap();
            builder.finish_relation().unwrap();
        }

        // This state is unreachable through the public builder protocol. The
        // mutation pins the final defense against a future accounting bug.
        builder.observed_total_count = 1;
        assert!(matches!(
            builder.finish(),
            Err(SnapshotCodecError::TotalRecordCountMismatch {
                declared_relation_total: 0,
                observed_total: 1,
            })
        ));
    }
}
