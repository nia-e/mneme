use std::collections::BTreeSet;

use cozo::{
    ManagedCatalogEncodingV1, ManagedSqliteClosedAuditV1, ManagedSqliteRecordVisitEvidenceV1,
};

use super::super::LogicalStoreCommitment;

const BASE_RELATION_NAMES_V1: [&str; 16] = [
    "meta",
    "node",
    "node_tag",
    "node_tag_v2",
    "node_search",
    "node_vec",
    "edge",
    "edge_anchor",
    "contradiction",
    "merge_candidate",
    "full_merge_commit",
    "supersede_commit",
    "remote_edge",
    "feedback_retry",
    "feedback_retry_order",
    "mneme_reembed_shadow_node_vec",
];

const DERIVED_RELATION_NAMES_V1: [&str; 10] = [
    "node_tag:by_tag",
    "node_tag_v2:by_id",
    "node_search:by_status",
    "node_search:active_fts",
    "node_search:candidate_fts",
    "node_search:archived_fts",
    "node_vec:active_idx",
    "node_vec:candidate_idx",
    "node_vec:archived_idx",
    "edge:by_to",
];

// Literal Mnestic v1 record-visitor policy identity. This verifier deliberately
// does not import a vendor constant: a vendor policy change must fail closed
// until this integration is independently reviewed and repinned.
pub(in crate::storage_contract::conventional_unmanaged::snapshot) const MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_V1: [u8; 32] = [
    239, 198, 251, 90, 175, 145, 95, 155, 127, 112, 219, 59, 247, 119, 16, 150, 121, 86, 44, 137,
    198, 227, 3, 65, 195, 148, 65, 189, 136, 135, 190, 218,
];

const RELATION_ID_EXCLUSIVE_LIMIT: u64 = 1_u64 << 48;

/// Rebuild the literal role map and join it to the physical, visit, and logical
/// counts only after Mnestic has strictly closed the reader.
///
/// Do not import the planning module or Mneme's static schema tables here. The
/// duplication is the point: it prevents one bad shared name-to-role mapping
/// from planning and then self-verifying a semantically swapped visit.
pub(in crate::storage_contract::conventional_unmanaged::snapshot) fn verify_closed_logical_snapshot_roles_v1(
    audit: &ManagedSqliteClosedAuditV1,
    visit: &ManagedSqliteRecordVisitEvidenceV1,
    logical: &LogicalStoreCommitment,
) -> Result<(), cozo::Error> {
    if visit.policy_fingerprint() != &MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_V1 {
        return Err(role_error(
            "closed logical snapshot used an unsupported record-visitor policy",
        ));
    }

    let entries = audit.catalog().catalog().entries();
    if entries.len() != BASE_RELATION_NAMES_V1.len() + DERIVED_RELATION_NAMES_V1.len() {
        return Err(role_error(
            "closed logical snapshot catalog did not contain the literal role inventory",
        ));
    }

    let mut base_ids = [0_u64; 16];
    let mut base_seen = [false; 16];
    let mut derived_seen = [false; 10];
    let mut all_ids = BTreeSet::new();
    for entry in entries {
        if entry.encoding() != ManagedCatalogEncodingV1::StructMapV1 {
            return Err(role_error(
                "closed logical snapshot catalog was not entirely struct-map v1",
            ));
        }
        let relation = entry.relation();
        let id = relation.id();
        if id == 0 || id >= RELATION_ID_EXCLUSIVE_LIMIT || !all_ids.insert(id) {
            return Err(role_error(
                "closed logical snapshot catalog had an invalid or repeated relation id",
            ));
        }

        if let Some(index) = BASE_RELATION_NAMES_V1
            .iter()
            .position(|expected| relation.name() == *expected)
        {
            if base_seen[index] {
                return Err(role_error(
                    "closed logical snapshot repeated a literal base relation role",
                ));
            }
            base_seen[index] = true;
            base_ids[index] = id;
        } else if let Some(index) = DERIVED_RELATION_NAMES_V1
            .iter()
            .position(|expected| relation.name() == *expected)
        {
            if derived_seen[index] {
                return Err(role_error(
                    "closed logical snapshot repeated a literal derived relation role",
                ));
            }
            derived_seen[index] = true;
        } else {
            return Err(role_error(
                "closed logical snapshot catalog contained an unknown relation role",
            ));
        }
    }
    if base_seen.iter().any(|seen| !seen) || derived_seen.iter().any(|seen| !seen) {
        return Err(role_error(
            "closed logical snapshot omitted a literal relation role",
        ));
    }

    if visit.relation_ids() != &base_ids {
        return Err(role_error(
            "closed logical snapshot visited ids did not match the independent base-role map",
        ));
    }

    let visit_counts = visit.relation_row_counts();
    let mut physical_total = 0_u64;
    for (index, relation_id) in base_ids.iter().copied().enumerate() {
        let physical_count = audit
            .physical()
            .relation_row_counts()
            .iter()
            .find(|row| row.relation_id() == relation_id)
            .map(|row| row.row_count())
            .ok_or_else(|| {
                role_error("closed logical snapshot base role was absent from the physical census")
            })?;
        if visit_counts[index] != physical_count {
            return Err(role_error(
                "closed logical snapshot visit count did not match the physical census",
            ));
        }
        physical_total = physical_total.checked_add(physical_count).ok_or_else(|| {
            role_error("closed logical snapshot physical base-row total overflowed")
        })?;
    }
    if visit.total_row_count() != physical_total {
        return Err(role_error(
            "closed logical snapshot visit total did not match the physical base-row total",
        ));
    }

    for (index, relation) in logical.relations().iter().enumerate() {
        let ordinal = u16::try_from(index + 1)
            .map_err(|_| role_error("closed logical snapshot relation ordinal overflowed"))?;
        if relation.ordinal() != ordinal || relation.record_count() != visit_counts[index] {
            return Err(role_error(
                "closed logical snapshot logical counts did not match the ordered visit",
            ));
        }
    }
    if logical.total_record_count() != visit.total_row_count() {
        return Err(role_error(
            "closed logical snapshot logical total did not match the visit total",
        ));
    }
    Ok(())
}

fn role_error(message: &'static str) -> cozo::Error {
    cozo::Error::msg(message)
}
