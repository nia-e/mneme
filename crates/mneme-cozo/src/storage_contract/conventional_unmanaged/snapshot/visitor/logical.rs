use cozo::{
    ManagedSqliteClosedAuditV1, ManagedSqliteClosedRecordVisitV1, ManagedSqliteRecordVisitEventV1,
    ManagedSqliteRecordVisitEvidenceV1, ManagedSqliteSnapshotReader,
};

use crate::storage_contract::conventional_unmanaged::admission::{
    CatalogSealGenerationV1, SourceBoundSealV1, classify_closed_audit_v1,
};

use super::super::{
    LogicalSnapshotCommitmentBuilder, LogicalStoreCommitment, SnapshotCodecError,
    encode_snapshot_record,
    roles::{plan_logical_snapshot_roles_v1, verify_closed_logical_snapshot_roles_v1},
};

/// Closed, independently verified logical visit of one prepared SQLite reader.
///
/// The retained source seal is historical evidence only. In particular, this
/// result does not claim that the reader came from Mnestic's backup handoff, and
/// it grants no authentication, freshness, mutation, or publication authority.
#[must_use = "retain the closed source, visitor, and logical commitment evidence together"]
pub(crate) struct ClosedLogicalSnapshotVisitV1 {
    source_seal: SourceBoundSealV1,
    visit_evidence: ManagedSqliteRecordVisitEvidenceV1,
    logical_commitment: LogicalStoreCommitment,
}

impl ClosedLogicalSnapshotVisitV1 {
    pub(crate) const fn source_seal(&self) -> &SourceBoundSealV1 {
        &self.source_seal
    }

    pub(crate) const fn visit_evidence(&self) -> &ManagedSqliteRecordVisitEvidenceV1 {
        &self.visit_evidence
    }

    pub(crate) const fn logical_commitment(&self) -> &LogicalStoreCommitment {
        &self.logical_commitment
    }
}

/// Consume an already-prepared managed reader into one exact logical commitment.
///
/// Preparation and provenance are intentionally outside this function: it does
/// not open a path or make a backup claim. No result is constructed until the
/// combined assertion/visit operation has reconciled the transaction, strictly
/// closed SQLite, validated the historical source bracket, classified the exact
/// current struct-map seal, finished the logical sink, and passed an independent
/// literal role-map verification.
pub(crate) fn close_logical_snapshot_visit_v1(
    reader: ManagedSqliteSnapshotReader,
) -> Result<ClosedLogicalSnapshotVisitV1, cozo::Error> {
    let closed = reader.close_with_relation_assertions_and_record_visit_v1(
        LogicalVisitSinkV1::new(),
        plan_logical_snapshot_roles_v1,
        visit_logical_snapshot_event_v1,
    )?;
    finish_closed_logical_snapshot_visit_v1(closed)
}

pub(super) struct LogicalVisitSinkV1 {
    builder: LogicalSnapshotCommitmentBuilder,
}

impl LogicalVisitSinkV1 {
    pub(super) fn new() -> Self {
        Self {
            builder: LogicalSnapshotCommitmentBuilder::new(),
        }
    }

    fn visit(&mut self, event: ManagedSqliteRecordVisitEventV1<'_>) -> Result<(), cozo::Error> {
        match event {
            ManagedSqliteRecordVisitEventV1::BeginRelation {
                ordinal,
                expected_rows,
            } => self
                .builder
                .begin_relation(ordinal, expected_rows)
                .map_err(codec_error),
            ManagedSqliteRecordVisitEventV1::Record {
                ordinal,
                key,
                value,
            } => {
                let record = encode_snapshot_record(ordinal, key, value).map_err(codec_error)?;
                self.builder.push_record(&record).map_err(codec_error)
            }
            ManagedSqliteRecordVisitEventV1::EndRelation { ordinal, rows } => {
                let relation = self.builder.finish_relation().map_err(codec_error)?;
                if relation.ordinal() != ordinal || relation.record_count() != rows {
                    return Err(cozo::Error::msg(
                        "logical snapshot sink end event did not match its completed relation",
                    ));
                }
                Ok(())
            }
        }
    }

    fn finish(self) -> Result<LogicalStoreCommitment, cozo::Error> {
        self.builder.finish().map_err(codec_error)
    }
}

pub(super) fn visit_logical_snapshot_event_v1(
    sink: &mut LogicalVisitSinkV1,
    event: ManagedSqliteRecordVisitEventV1<'_>,
) -> Result<(), cozo::Error> {
    sink.visit(event)
}

pub(super) fn finish_closed_logical_snapshot_visit_v1(
    closed: ManagedSqliteClosedRecordVisitV1<LogicalVisitSinkV1>,
) -> Result<ClosedLogicalSnapshotVisitV1, cozo::Error> {
    // `into_parts` is called only after Mnestic has returned its opaque closed
    // wrapper; the tentative sink is never available on an ordinary close error.
    let (audit, visit_evidence, sink) = closed.into_parts();
    finish_closed_logical_snapshot_parts_v1(audit, visit_evidence, sink)
}

fn finish_closed_logical_snapshot_parts_v1(
    audit: ManagedSqliteClosedAuditV1,
    visit_evidence: ManagedSqliteRecordVisitEvidenceV1,
    sink: LogicalVisitSinkV1,
) -> Result<ClosedLogicalSnapshotVisitV1, cozo::Error> {
    let source_seal = classify_closed_audit_v1(audit)?;
    if source_seal.generation() != CatalogSealGenerationV1::CurrentStructMap {
        return Err(cozo::Error::msg(
            "logical managed snapshot requires the current struct-map generation",
        ));
    }
    let logical_commitment = sink.finish()?;
    verify_closed_logical_snapshot_roles_v1(
        source_seal.audit(),
        &visit_evidence,
        &logical_commitment,
    )?;
    Ok(ClosedLogicalSnapshotVisitV1 {
        source_seal,
        visit_evidence,
        logical_commitment,
    })
}

fn codec_error(error: SnapshotCodecError) -> cozo::Error {
    cozo::Error::msg(error.to_string())
}

// Keep the staged integration API compile-reachable without weakening dead-code
// linting for unrelated additions.
const _: fn(ManagedSqliteSnapshotReader) -> Result<ClosedLogicalSnapshotVisitV1, cozo::Error> =
    close_logical_snapshot_visit_v1;
const _: fn(&ClosedLogicalSnapshotVisitV1) -> &SourceBoundSealV1 =
    ClosedLogicalSnapshotVisitV1::source_seal;
const _: fn(&ClosedLogicalSnapshotVisitV1) -> &ManagedSqliteRecordVisitEvidenceV1 =
    ClosedLogicalSnapshotVisitV1::visit_evidence;
const _: fn(&ClosedLogicalSnapshotVisitV1) -> &LogicalStoreCommitment =
    ClosedLogicalSnapshotVisitV1::logical_commitment;
