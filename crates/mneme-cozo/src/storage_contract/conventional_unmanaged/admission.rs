//! Semantic validation of Mneme's raw, primary-index-visible conventional unmanaged catalog.
//!
//! Mnestic has already bounded and exactly decoded the hostile catalog bytes
//! before values reach this module. This oracle matches those sanitized DTOs
//! against [`super::spec`], including durable fields that system-command
//! presentation omits or synthesizes.
//!
//! Success is intentionally only a candidate observation. It does **not** prove
//! catalog completeness, SQLite or b-tree integrity, semantic-row admission,
//! source/path binding, or mutation authority. The consuming boundary below
//! separately joins this oracle to Mnestic's physical census, six semantic-row
//! assertions, strict close, and historical source evidence. It remains
//! recognition only: no result authorizes mutation or publishes a generation.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};

use cozo::{
    ExistingSqliteSnapshotSource, ManagedAccessLevelV1, ManagedCatalogEncodingV1,
    ManagedCatalogFencePolicy, ManagedColumnTypeKindV1, ManagedColumnTypeV1, ManagedColumnV1,
    ManagedExistingSqliteOpenPermitV1, ManagedFtsIndexV1, ManagedHnswDistanceV1,
    ManagedHnswIndexV1, ManagedNormalIndexV1, ManagedRelationV1, ManagedSnapshotPolicy,
    ManagedSqliteCatalogAssertionPlannerV1, ManagedSqliteClosedAuditV1,
    ManagedSqliteClosedCatalogFenceV1, ManagedSqlitePrimaryIndexCatalogV1,
    ManagedSqliteRelationAssertionPlannerV1, ManagedSqliteSnapshotReader, ManagedTokenizerV1,
    ManagedVectorElementTypeV1,
};
use sha2::{Digest, Sha256};

#[cfg(test)]
use super::spec::{
    BASE_RELATIONS, DERIVED_RELATIONS, MANAGED_WRITER_GENERATION_MARKER, all_physical_relations,
};
use super::spec::{
    CAPTURE_V1_CATALOG_GENERATION_MARKER, CONCERN_V1_CATALOG_GENERATION_MARKER, CatalogContract,
    ColumnSpec, ColumnType, DerivedRelationSpec, DurableAccess,
    EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER, EPISODE_V1_CATALOG_GENERATION_MARKER,
    FORBIDDEN_FEATURES, FtsIndexSpec, HnswDistance, HnswIndexSpec, IndexSpec,
    LEGACY_CATALOG_GENERATION_MARKER, PERMANENT_VECTOR_GUARD_VALUE, RelationSpec,
    SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER, STRUCT_MAP_CATALOG_GENERATION_MARKER,
    TOUCHSTONES_V1_CATALOG_GENERATION_MARKER, TokenizerSpec, VectorDimensionBinding,
    VectorElementType,
};

#[cfg(test)]
const TOP_LEVEL_RELATION_COUNT: usize = BASE_RELATIONS.len() + DERIVED_RELATIONS.len();
const RELATION_ID_EXCLUSIVE_LIMIT: u64 = 1_u64 << 48;
const VECTOR_GENERATION_OUTCOME_TAG: &str = "conventional-unmanaged-vector-generation";
const PLAN_NODE_TAG_RELATION: &str = "node_tag";
const PLAN_NODE_TAG_COUNT: u64 = 0;
const PLAN_PERMANENT_GUARD_RELATION: &str = "mneme_reembed_shadow_node_vec";
const PLAN_PERMANENT_GUARD_COUNT: u64 = 1;
const PLAN_META_RELATION: &str = "meta";
const VERIFY_NODE_TAG_RELATION: &str = "node_tag";
const VERIFY_NODE_TAG_COUNT: u64 = 0;
const VERIFY_PERMANENT_GUARD_RELATION: &str = "mneme_reembed_shadow_node_vec";
const VERIFY_PERMANENT_GUARD_COUNT: u64 = 1;
const VERIFY_META_RELATION: &str = "meta";

const MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-relation-assertion.transcript.v1\0";
const MANAGED_CATALOG_FENCE_ASSERTION_TRANSCRIPT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-catalog-fence-assertion.transcript.v1\0";
const MANAGED_CATALOG_POLICY_FINGERPRINT_V1: [u8; 32] = [
    14, 35, 102, 76, 164, 142, 235, 8, 177, 215, 104, 39, 220, 252, 129, 206, 10, 204, 252, 141,
    55, 67, 81, 189, 36, 231, 33, 188, 51, 138, 59, 21,
];
const MANAGED_PHYSICAL_POLICY_FINGERPRINT_V1: [u8; 32] = [
    25, 34, 104, 183, 235, 148, 9, 17, 203, 100, 254, 116, 178, 137, 0, 67, 7, 124, 107, 45, 33,
    219, 179, 184, 162, 58, 21, 206, 31, 41, 44, 253,
];
const MANAGED_ASSERTION_POLICY_FINGERPRINT_V1: [u8; 32] = [
    193, 250, 7, 93, 127, 176, 92, 94, 83, 47, 230, 253, 32, 228, 226, 40, 72, 174, 33, 87, 12,
    192, 203, 184, 160, 78, 87, 229, 147, 102, 231, 58,
];
const MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_V1: [u8; 32] = [
    188, 112, 86, 144, 47, 34, 215, 65, 89, 20, 208, 245, 196, 214, 22, 253, 119, 190, 177, 57, 88,
    55, 153, 148, 108, 88, 168, 152, 146, 189, 250, 50,
];
const MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    196, 62, 40, 190, 206, 23, 138, 178, 241, 29, 93, 139, 70, 3, 194, 169, 218, 247, 88, 214, 192,
    121, 7, 229, 80, 220, 204, 131, 121, 109, 100, 154,
];
const MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    162, 40, 41, 176, 139, 255, 193, 176, 196, 193, 88, 125, 168, 149, 230, 47, 104, 81, 247, 121,
    184, 236, 178, 28, 34, 240, 12, 124, 90, 177, 87, 235,
];
#[cfg(test)]
const OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mneme.cozo.conventional-unmanaged-open-admission.policy-fingerprint.v1\0";
// Repinned only through the independent policy KAT below.
const OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1: [u8; 32] = [
    105, 226, 251, 159, 136, 238, 47, 128, 152, 22, 182, 243, 149, 76, 253, 33, 162, 151, 34, 121,
    1, 101, 56, 224, 229, 240, 238, 160, 165, 127, 81, 204,
];
/// SHA-256 of the capture policy domain, the original closed-open policy
/// fingerprint, and the exact capture generation literal (tested below).
const CAPTURE_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1: [u8; 32] = [
    49, 96, 138, 97, 69, 1, 164, 134, 148, 153, 194, 232, 189, 239, 24, 38, 229, 139, 162, 90, 73,
    99, 187, 87, 118, 40, 147, 179, 110, 228, 129, 119,
];
#[cfg(test)]
const SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mneme.cozo.conventional-unmanaged-source-bound.policy-fingerprint.v1\0";
// Repinned only through the independent policy KAT below.
const SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1: [u8; 32] = [
    120, 200, 243, 94, 119, 205, 212, 68, 144, 146, 61, 200, 27, 177, 219, 195, 118, 203, 211, 251,
    184, 129, 221, 96, 65, 255, 66, 208, 7, 236, 92, 90,
];

const MANAGED_EPISODE_CATALOG_POLICY_FINGERPRINT_V1: [u8; 32] = [
    186, 178, 142, 119, 22, 62, 245, 202, 164, 220, 242, 40, 214, 9, 26, 117, 207, 87, 234, 59,
    159, 147, 154, 249, 142, 49, 106, 216, 113, 40, 229, 199,
];
const MANAGED_EPISODE_PHYSICAL_POLICY_FINGERPRINT_V1: [u8; 32] = [
    185, 223, 9, 190, 143, 225, 216, 254, 36, 221, 13, 64, 252, 149, 145, 133, 200, 133, 183, 203,
    76, 51, 107, 76, 157, 47, 183, 145, 17, 254, 33, 233,
];
const MANAGED_EPISODE_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    229, 208, 243, 171, 239, 125, 35, 224, 71, 248, 242, 22, 240, 199, 105, 39, 58, 131, 33, 142,
    223, 148, 183, 95, 211, 223, 80, 63, 167, 209, 216, 86,
];
const MANAGED_EPISODE_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    151, 212, 248, 90, 157, 124, 92, 192, 22, 33, 67, 107, 133, 123, 176, 31, 199, 39, 86, 65, 138,
    105, 69, 254, 201, 121, 224, 226, 189, 174, 229, 209,
];
const EPISODE_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1: [u8; 32] = [
    246, 126, 27, 224, 218, 66, 115, 231, 42, 26, 104, 162, 246, 150, 158, 184, 68, 79, 169, 138,
    238, 0, 95, 212, 200, 127, 7, 54, 24, 143, 1, 30,
];

const EPISODE_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1: [u8; 32] = [
    190, 95, 58, 74, 79, 44, 245, 255, 56, 72, 193, 62, 20, 0, 228, 227, 67, 197, 137, 213, 69,
    247, 69, 84, 251, 94, 53, 249, 160, 20, 248, 79,
];

const MANAGED_SINGLE_GRAPH_CATALOG_POLICY_FINGERPRINT_V1: [u8; 32] = [
    100, 78, 17, 141, 64, 18, 200, 182, 203, 25, 68, 131, 253, 208, 138, 156, 160, 193, 65, 213,
    139, 12, 14, 94, 140, 28, 49, 166, 204, 255, 254, 189,
];
const MANAGED_SINGLE_GRAPH_PHYSICAL_POLICY_FINGERPRINT_V1: [u8; 32] = [
    13, 92, 233, 204, 58, 46, 48, 103, 24, 12, 92, 91, 234, 105, 138, 37, 127, 184, 150, 232, 89,
    188, 166, 236, 38, 167, 224, 203, 82, 83, 133, 51,
];
const MANAGED_SINGLE_GRAPH_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    66, 226, 254, 49, 184, 240, 210, 118, 223, 68, 48, 5, 27, 62, 206, 139, 153, 187, 136, 14, 183,
    216, 50, 212, 189, 255, 24, 12, 220, 7, 78, 116,
];
const MANAGED_SINGLE_GRAPH_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    51, 137, 166, 205, 77, 224, 53, 168, 115, 236, 152, 172, 225, 230, 254, 65, 212, 109, 179, 156,
    4, 123, 162, 84, 94, 232, 112, 28, 151, 170, 54, 180,
];
const SINGLE_GRAPH_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1: [u8; 32] = [
    66, 202, 22, 209, 205, 29, 55, 188, 143, 132, 93, 236, 87, 128, 118, 86, 47, 53, 57, 147, 173,
    107, 101, 154, 1, 254, 88, 211, 93, 11, 101, 108,
];
const SINGLE_GRAPH_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1: [u8; 32] = [
    233, 211, 171, 127, 244, 111, 35, 171, 77, 90, 102, 112, 203, 81, 227, 208, 22, 154, 131, 242,
    228, 27, 59, 166, 86, 101, 167, 15, 120, 171, 154, 195,
];

const MANAGED_CONCERN_CATALOG_POLICY_FINGERPRINT_V1: [u8; 32] = [
    178, 140, 67, 240, 222, 185, 243, 151, 75, 219, 66, 118, 193, 191, 35, 151, 12, 250, 96, 229,
    113, 26, 56, 145, 167, 250, 36, 39, 112, 183, 39, 76,
];
const MANAGED_CONCERN_PHYSICAL_POLICY_FINGERPRINT_V1: [u8; 32] = [
    195, 109, 242, 83, 9, 8, 92, 151, 201, 154, 106, 55, 6, 35, 29, 189, 97, 30, 29, 196, 125, 7,
    38, 207, 121, 167, 12, 24, 182, 96, 182, 92,
];
const MANAGED_CONCERN_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    37, 171, 178, 229, 11, 201, 75, 183, 35, 165, 111, 225, 51, 45, 102, 30, 156, 71, 71, 154, 175,
    216, 16, 112, 92, 175, 61, 241, 244, 200, 90, 218,
];
const MANAGED_CONCERN_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    246, 7, 157, 232, 94, 146, 19, 97, 238, 155, 152, 253, 56, 193, 218, 65, 83, 111, 213, 159,
    245, 40, 69, 143, 202, 75, 38, 65, 228, 30, 196, 100,
];
const CONCERN_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1: [u8; 32] = [
    250, 12, 55, 21, 17, 227, 252, 40, 130, 165, 45, 25, 192, 58, 242, 51, 222, 10, 51, 188, 1,
    251, 224, 254, 32, 143, 70, 90, 145, 169, 150, 193,
];
const CONCERN_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1: [u8; 32] = [
    81, 56, 184, 19, 92, 220, 50, 210, 123, 15, 211, 35, 197, 140, 120, 112, 161, 207, 182, 56, 67,
    3, 66, 87, 173, 148, 55, 206, 50, 111, 70, 110,
];

const EPISODE_CONTEXT_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V2: [u8; 32] = [
    83, 19, 179, 148, 172, 5, 197, 13, 9, 73, 108, 82, 136, 10, 39, 73, 81, 134, 22, 161, 243, 188,
    46, 124, 223, 64, 72, 65, 95, 62, 28, 83,
];

const EPISODE_CONTEXT_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V2: [u8; 32] = [
    137, 188, 24, 33, 63, 185, 167, 227, 35, 81, 33, 31, 146, 30, 188, 200, 90, 55, 46, 103, 50,
    210, 91, 182, 178, 239, 94, 46, 125, 231, 53, 149,
];

const MANAGED_TOUCHSTONES_CATALOG_POLICY_FINGERPRINT_V1: [u8; 32] = [
    160, 95, 232, 52, 189, 248, 54, 38, 187, 2, 152, 58, 50, 114, 91, 120, 253, 98, 223, 102, 113,
    186, 132, 12, 153, 132, 128, 4, 228, 220, 155, 56,
];
const MANAGED_TOUCHSTONES_PHYSICAL_POLICY_FINGERPRINT_V1: [u8; 32] = [
    80, 93, 29, 230, 128, 115, 116, 48, 51, 96, 63, 139, 97, 99, 176, 74, 144, 181, 39, 13, 107,
    175, 95, 125, 141, 85, 172, 219, 156, 69, 1, 90,
];
const MANAGED_TOUCHSTONES_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    19, 76, 30, 182, 127, 106, 108, 19, 164, 202, 74, 228, 175, 91, 40, 94, 239, 85, 129, 143, 143,
    193, 189, 173, 34, 132, 243, 66, 197, 236, 157, 6,
];
const MANAGED_TOUCHSTONES_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    133, 104, 39, 217, 108, 57, 8, 236, 176, 214, 161, 47, 92, 38, 52, 170, 13, 192, 34, 69, 166,
    174, 28, 91, 147, 29, 160, 26, 72, 251, 79, 109,
];

const TOUCHSTONES_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1: [u8; 32] = [
    235, 237, 104, 178, 104, 219, 85, 9, 198, 125, 250, 224, 44, 240, 156, 159, 151, 75, 210, 135,
    52, 69, 122, 100, 103, 238, 119, 182, 0, 207, 129, 127,
];
const TOUCHSTONES_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1: [u8; 32] = [
    239, 43, 90, 9, 200, 136, 36, 108, 89, 232, 232, 206, 194, 39, 114, 117, 191, 88, 116, 8, 71,
    167, 130, 26, 112, 188, 181, 76, 203, 56, 209, 204,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CatalogSealGenerationV1 {
    LegacyNeedsMigration,
    CurrentStructMap,
    CaptureV1,
    EpisodeV1,
    SingleGraphV1,
    ConcernV1,
    EpisodeContextV2,
    TouchstonesV1,
}

struct OpenAdmissionEvidenceV1 {
    fence: ManagedSqliteClosedCatalogFenceV1,
    codec_classification: CatalogCodecClassification,
    vector_dimension: u64,
    classifier_policy_fingerprint: [u8; 32],
}

impl OpenAdmissionEvidenceV1 {
    const fn diagnostic_fence(&self) -> &ManagedSqliteClosedCatalogFenceV1 {
        &self.fence
    }

    const fn codec_classification(&self) -> CatalogCodecClassification {
        self.codec_classification
    }

    const fn vector_dimension(&self) -> u64 {
        self.vector_dimension
    }

    const fn classifier_policy_fingerprint(&self) -> &[u8; 32] {
        &self.classifier_policy_fingerprint
    }
}

/// Owning, non-cloneable capability for one exact current struct-map conventional unmanaged source.
///
/// The eventual existing-store constructor must take this type **by value**.
/// Borrowed getters expose bounded diagnostics only; none detaches admission
/// authority from the owned closed fence. This is still only routine catalog
/// recognition, not full physical integrity or mutation authority.
#[must_use = "the existing-store constructor must consume this current conventional generation permit"]
pub(crate) struct CurrentOpenPermitV1 {
    evidence: OpenAdmissionEvidenceV1,
}

impl CurrentOpenPermitV1 {
    pub(crate) const fn diagnostic_fence(&self) -> &ManagedSqliteClosedCatalogFenceV1 {
        self.evidence.diagnostic_fence()
    }

    pub(crate) const fn vector_dimension(&self) -> u64 {
        self.evidence.vector_dimension()
    }

    pub(crate) const fn classifier_policy_fingerprint(&self) -> &[u8; 32] {
        self.evidence.classifier_policy_fingerprint()
    }

    /// Consume current-generation evidence into the only aggregate accepted by
    /// Mneme's existing-only runtime constructor.
    ///
    /// The exact caller-supplied path is retained independently of the vendor
    /// permit and must byte-match the path closed by the raw fence. The vector
    /// dimension is converted once into a positive host `usize`; callers never
    /// supply a fallback dimension.
    pub(crate) fn into_existing_open_admission_v1(
        self,
        requested_path: &Path,
    ) -> Result<ExistingOpenAdmissionV1, AdmissionNotEstablished> {
        let OpenAdmissionEvidenceV1 {
            fence,
            codec_classification: _,
            vector_dimension,
            classifier_policy_fingerprint,
        } = self.evidence;
        let supplied_path = fence.source().supplied_path().to_path_buf();
        if supplied_path.as_os_str() != requested_path.as_os_str() {
            return Err(AdmissionNotEstablished);
        }
        let vector_dimension = usize::try_from(vector_dimension)
            .ok()
            .filter(|dimension| *dimension > 0)
            .ok_or(AdmissionNotEstablished)?;
        let runtime_permit = fence.into_existing_sqlite_open_permit_v1();
        Ok(ExistingOpenAdmissionV1 {
            supplied_path,
            vector_dimension,
            admission_receipt: ExistingOpenAdmissionReceiptV1 {
                classifier_policy_fingerprint,
            },
            runtime_permit,
        })
    }
}

/// Durable-in-process receipt retained for the complete lease-backed runtime
/// lifetime. It carries no detached permission to reopen storage.
pub(crate) struct ExistingOpenAdmissionReceiptV1 {
    classifier_policy_fingerprint: [u8; 32],
}

impl ExistingOpenAdmissionReceiptV1 {
    pub(crate) fn uses_current_classifier_policy(&self) -> bool {
        self.classifier_policy_fingerprint == OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1
            || self.classifier_policy_fingerprint
                == CAPTURE_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1
            || self.classifier_policy_fingerprint
                == EPISODE_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1
            || self.classifier_policy_fingerprint
                == SINGLE_GRAPH_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1
            || self.classifier_policy_fingerprint
                == CONCERN_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1
            || self.classifier_policy_fingerprint
                == EPISODE_CONTEXT_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V2
            || self.classifier_policy_fingerprint
                == TOUCHSTONES_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1
    }
}

/// Single-use bridge from raw current-generation recognition to the vendor's
/// existing-only runtime open. Deliberately neither `Clone` nor `Copy`.
pub(crate) struct ExistingOpenAdmissionV1 {
    supplied_path: PathBuf,
    vector_dimension: usize,
    admission_receipt: ExistingOpenAdmissionReceiptV1,
    runtime_permit: ManagedExistingSqliteOpenPermitV1,
}

impl ExistingOpenAdmissionV1 {
    pub(crate) fn into_runtime_parts(
        self,
    ) -> (
        PathBuf,
        usize,
        ExistingOpenAdmissionReceiptV1,
        ManagedExistingSqliteOpenPermitV1,
    ) {
        (
            self.supplied_path,
            self.vector_dimension,
            self.admission_receipt,
            self.runtime_permit,
        )
    }
}

/// Owning, non-cloneable terminal evidence that catalog-codec upgrade is
/// required before ordinary open.
///
/// This cannot be passed where [`CurrentOpenPermitV1`] is required, and it
/// retains the closed fence instead of returning a detachable status bit.
#[must_use = "retain the closed upgrade-required evidence for actionable refusal"]
pub(crate) struct CatalogUpgradeEvidenceV1 {
    evidence: OpenAdmissionEvidenceV1,
}

impl CatalogUpgradeEvidenceV1 {
    pub(crate) const fn diagnostic_fence(&self) -> &ManagedSqliteClosedCatalogFenceV1 {
        self.evidence.diagnostic_fence()
    }

    pub(crate) const fn codec_classification(&self) -> CatalogCodecClassification {
        self.evidence.codec_classification()
    }

    pub(crate) const fn vector_dimension(&self) -> u64 {
        self.evidence.vector_dimension()
    }

    pub(crate) const fn classifier_policy_fingerprint(&self) -> &[u8; 32] {
        self.evidence.classifier_policy_fingerprint()
    }
}

/// Structurally non-detachable result of the bounded open-admission fence.
///
/// Both variants own their closed evidence and neither this enum nor either
/// payload is `Clone` or `Copy`. The old marker deliberately routes to the
/// upgrade variant even with struct-map bytes because an old admitted writer
/// can still rewrite a catalog value using a historical codec.
#[must_use = "consume the owning current permit or retain the terminal upgrade evidence"]
pub(crate) enum OpenAdmissionV1 {
    Current(CurrentOpenPermitV1),
    CatalogCodecUpgradeRequired(CatalogUpgradeEvidenceV1),
}

fn consume_open_admission_for_type_reach(admission: OpenAdmissionV1) {
    match admission {
        OpenAdmissionV1::Current(permit) => drop(permit),
        OpenAdmissionV1::CatalogCodecUpgradeRequired(evidence) => drop(evidence),
    }
}

const _: fn(OpenAdmissionV1) = consume_open_admission_for_type_reach;

// Keep only the staged borrowed diagnostics compile-reachable without
// suppressing dead-code diagnostics for unrelated additions.
const _: fn(&CurrentOpenPermitV1) -> &ManagedSqliteClosedCatalogFenceV1 =
    CurrentOpenPermitV1::diagnostic_fence;
const _: fn(&CurrentOpenPermitV1) -> u64 = CurrentOpenPermitV1::vector_dimension;
const _: fn(&CurrentOpenPermitV1) -> &[u8; 32] = CurrentOpenPermitV1::classifier_policy_fingerprint;
const _: fn(&CatalogUpgradeEvidenceV1) -> &ManagedSqliteClosedCatalogFenceV1 =
    CatalogUpgradeEvidenceV1::diagnostic_fence;
const _: fn(&CatalogUpgradeEvidenceV1) -> CatalogCodecClassification =
    CatalogUpgradeEvidenceV1::codec_classification;
const _: fn(&CatalogUpgradeEvidenceV1) -> u64 = CatalogUpgradeEvidenceV1::vector_dimension;
const _: fn(&CatalogUpgradeEvidenceV1) -> &[u8; 32] =
    CatalogUpgradeEvidenceV1::classifier_policy_fingerprint;

/// Terminal failure after consuming a source for the bounded open admission fence.
///
/// The input source or closed fence has been consumed and no admitted evidence
/// is returned. This result therefore ends the current classification attempt:
/// callers must not fall through to another classifier based on this error. A
/// higher container classifier must establish predecessor, managed, residue, or
/// corrupt states before this consuming boundary, or start a separately
/// reopened and revalidated attempt. Attacker-controlled SQLite diagnostics
/// are intentionally erased.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdmissionNotEstablished;

impl Display for AdmissionNotEstablished {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded open admission was not established")
    }
}

impl Error for AdmissionNotEstablished {}

/// Closed source-bound recognition of the exact conventional unmanaged catalog and semantic seal.
///
/// This retains Mnestic's opaque historical audit. It is not authentication,
/// freshness, ownership, mutation authority, or publication permission.
#[must_use = "retain or explicitly inspect the closed source-bound seal"]
pub(crate) struct SourceBoundSealV1 {
    audit: ManagedSqliteClosedAuditV1,
    generation: CatalogSealGenerationV1,
    codec_classification: CatalogCodecClassification,
    vector_dimension: u64,
    policy_fingerprint: [u8; 32],
}

impl SourceBoundSealV1 {
    pub(crate) const fn audit(&self) -> &ManagedSqliteClosedAuditV1 {
        &self.audit
    }

    pub(crate) const fn generation(&self) -> CatalogSealGenerationV1 {
        self.generation
    }

    pub(crate) const fn codec_classification(&self) -> CatalogCodecClassification {
        self.codec_classification
    }

    pub(crate) const fn vector_dimension(&self) -> u64 {
        self.vector_dimension
    }

    pub(crate) const fn policy_fingerprint(&self) -> &[u8; 32] {
        &self.policy_fingerprint
    }
}

// The opaque seal's narrow read API is the staged existing-open handoff. Root each getter
// explicitly instead of blanketing the module with a lint suppression.
const _: fn(&SourceBoundSealV1) -> &ManagedSqliteClosedAuditV1 = SourceBoundSealV1::audit;
const _: fn(&SourceBoundSealV1) -> CatalogSealGenerationV1 = SourceBoundSealV1::generation;
const _: fn(&SourceBoundSealV1) -> CatalogCodecClassification =
    SourceBoundSealV1::codec_classification;
const _: fn(&SourceBoundSealV1) -> u64 = SourceBoundSealV1::vector_dimension;
const _: fn(&SourceBoundSealV1) -> &[u8; 32] = SourceBoundSealV1::policy_fingerprint;

/// Exact per-entry codec census for a semantically valid conventional unmanaged catalog.
///
/// `MixedExact` means that every individual recursive catalog value had one
/// exact historical encoding, but the 26 values did not all use the same one.
/// It is admissible input to an offline canonicalizer; it is never canonical
/// managed-snapshot input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CatalogCodecClassification {
    AllPositionalV0,
    AllPositionalV1,
    AllStructMapV1,
    MixedExact,
}

/// A borrowed semantic match for the primary-index-visible conventional unmanaged catalog.
///
/// This type is deliberately crate-private, non-cloneable, and tied to the
/// lifetime of Mnestic's live managed reader. It is **not** completeness,
/// physical-integrity, semantic-row-admission, source-binding, sentinel, or
/// mutation-authority evidence. A later consuming audit must establish every
/// one of those independent obligations before publication or mutation.
pub(crate) struct PrimaryIndexVisibleCatalogCandidate<'a> {
    _observation: &'a ManagedSqlitePrimaryIndexCatalogV1,
    codec_classification: CatalogCodecClassification,
    vector_dimension: u64,
    relation_ids: Vec<u64>,
    catalog: CatalogContract,
}

impl PrimaryIndexVisibleCatalogCandidate<'_> {
    pub(crate) const fn codec_classification(&self) -> CatalogCodecClassification {
        self.codec_classification
    }

    /// Positive dimension derived from the raw `node_vec.e` recursive type,
    /// independently of all three HNSW manifests.
    pub(crate) const fn vector_dimension(&self) -> u64 {
        self.vector_dimension
    }

    /// Validated top-level relation id for one trusted static contract name.
    ///
    /// The returned scalar remains only a field of this conditional candidate;
    /// it does not bind the id to a complete physical census or source.
    pub(crate) fn relation_id(&self, trusted_name: &'static str) -> Option<u64> {
        self.catalog
            .relation_slot(trusted_name)
            .map(|slot| self.relation_ids[slot])
    }
}

/// Fixed, bounded diagnostics from the conventional semantic oracle.
///
/// Every carried string is a trusted static name from [`super::spec`]. No
/// attacker-controlled catalog text, expression, slice, or DTO is retained or
/// rendered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RawCatalogError {
    CatalogInventory,
    StorageMetadata,
    RelationCounter,
    RelationId {
        expected_relation: &'static str,
    },
    RelationShape {
        expected_relation: &'static str,
    },
    ColumnLayout {
        expected_relation: &'static str,
    },
    Column {
        expected_relation: &'static str,
        expected_column: &'static str,
    },
    IndexInventory {
        owner_relation: &'static str,
    },
    IndexManifest {
        owner_relation: &'static str,
        expected_index: &'static str,
    },
    NestedChild {
        expected_relation: &'static str,
    },
    VectorDimension {
        expected_relation: &'static str,
        expected_column: &'static str,
    },
    CanonicalStructMapRequired,
}

impl Display for RawCatalogError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::CatalogInventory => formatter.write_str("raw conventional catalog inventory mismatch"),
            Self::StorageMetadata => {
                formatter.write_str("raw conventional catalog storage metadata mismatch")
            }
            Self::RelationCounter => {
                formatter.write_str("raw conventional catalog relation counter mismatch")
            }
            Self::RelationId { expected_relation } => write!(
                formatter,
                "raw conventional catalog relation id mismatch for expected relation {expected_relation}"
            ),
            Self::RelationShape { expected_relation } => write!(
                formatter,
                "raw conventional catalog relation shape mismatch for expected relation {expected_relation}"
            ),
            Self::ColumnLayout { expected_relation } => write!(
                formatter,
                "raw conventional catalog column layout mismatch for expected relation {expected_relation}"
            ),
            Self::Column {
                expected_relation,
                expected_column,
            } => write!(
                formatter,
                "raw conventional catalog column mismatch for expected {expected_relation}.{expected_column}"
            ),
            Self::IndexInventory { owner_relation } => write!(
                formatter,
                "raw conventional catalog index inventory mismatch for expected owner {owner_relation}"
            ),
            Self::IndexManifest {
                owner_relation,
                expected_index,
            } => write!(
                formatter,
                "raw conventional catalog index manifest mismatch for expected {owner_relation}:{expected_index}"
            ),
            Self::NestedChild { expected_relation } => write!(
                formatter,
                "raw conventional catalog nested child mismatch for expected relation {expected_relation}"
            ),
            Self::VectorDimension {
                expected_relation,
                expected_column,
            } => write!(
                formatter,
                "raw conventional catalog vector dimension mismatch for expected {expected_relation}.{expected_column}"
            ),
            Self::CanonicalStructMapRequired => formatter.write_str(
                "managed snapshot requires all conventional unmanaged catalog entries to use exact struct-map v1",
            ),
        }
    }
}

impl Error for RawCatalogError {}

/// Match a sanitized primary-index observation against the closed field-level
/// conventional unmanaged catalog semantics visible through Mnestic's DTO.
///
/// Historical and mixed-exact source codecs are deliberately admitted here so
/// an offline canonicalizer can first establish semantic identity. Call
/// [`require_canonical_struct_map_source`] separately at the managed-snapshot
/// boundary. Neither success path checks semantic rows or the cumulative
/// sentinel.
pub(crate) fn validate_primary_index_visible_catalog(
    observation: &ManagedSqlitePrimaryIndexCatalogV1,
) -> Result<PrimaryIndexVisibleCatalogCandidate<'_>, RawCatalogError> {
    validate_primary_index_visible_catalog_for(observation, CatalogContract::Predecessor)
}

pub(crate) fn validate_episode_primary_index_visible_catalog(
    observation: &ManagedSqlitePrimaryIndexCatalogV1,
) -> Result<PrimaryIndexVisibleCatalogCandidate<'_>, RawCatalogError> {
    validate_primary_index_visible_catalog_for(observation, CatalogContract::EpisodeV1)
}

fn validate_primary_index_visible_catalog_for(
    observation: &ManagedSqlitePrimaryIndexCatalogV1,
    catalog: CatalogContract,
) -> Result<PrimaryIndexVisibleCatalogCandidate<'_>, RawCatalogError> {
    let relation_count = catalog.bases().len() + catalog.derived().len();
    if observation.storage_version() != 0 {
        return Err(RawCatalogError::StorageMetadata);
    }

    let entries = observation.catalog().entries();
    if entries.len() != relation_count {
        return Err(RawCatalogError::CatalogInventory);
    }
    let codec_classification = classify_codecs(entries.iter().map(|entry| entry.encoding()));

    // The Mnestic census is in raw primary-key order, which is intentionally
    // not an conventional semantic ordering. Match into trusted static slots without
    // cloning, retaining, or diagnosing hostile names.
    let mut slots = vec![None; relation_count];
    for entry in entries {
        let relation = entry.relation();
        let Some(slot) = catalog.relation_slot(relation.name()) else {
            return Err(RawCatalogError::CatalogInventory);
        };
        if slots[slot].replace(relation).is_some() {
            return Err(RawCatalogError::CatalogInventory);
        }
    }
    if slots.iter().any(Option::is_none) {
        return Err(RawCatalogError::CatalogInventory);
    }

    let mut ids = BTreeSet::new();
    let mut max_id = 0_u64;
    let mut relation_ids = vec![0_u64; relation_count];
    for (slot, relation_id) in relation_ids.iter_mut().enumerate() {
        let relation = relation_at(&slots, slot);
        let expected = catalog.relation_at(slot);
        let id = relation.id();
        if relation.name() != expected.name
            || id == 0
            || id >= RELATION_ID_EXCLUSIVE_LIMIT
            || !ids.insert(id)
        {
            return Err(RawCatalogError::RelationId {
                expected_relation: expected.name,
            });
        }
        *relation_id = id;
        max_id = max_id.max(id);
    }
    if observation.relation_counter() >= RELATION_ID_EXCLUSIVE_LIMIT
        || max_id > observation.relation_counter()
    {
        return Err(RawCatalogError::RelationCounter);
    }

    for (slot, base) in catalog.bases().iter().enumerate() {
        validate_relation_shape(relation_at(&slots, slot), base.relation)?;
    }
    for (child_offset, child) in catalog.derived().iter().enumerate() {
        let relation = relation_at(&slots, catalog.bases().len() + child_offset);
        validate_leaf_relation(relation, child.relation)?;
    }

    let vector_dimension =
        derive_vector_dimension(&slots, super::spec::HNSW_CATALOG_SPEC.dimension, catalog)?;

    for (slot, base) in catalog.bases().iter().enumerate() {
        validate_indices(
            relation_at(&slots, slot),
            base.relation.name,
            &slots,
            vector_dimension,
            catalog,
        )?;
    }

    Ok(PrimaryIndexVisibleCatalogCandidate {
        _observation: observation,
        codec_classification,
        vector_dimension,
        relation_ids,
        catalog,
    })
}

/// Refuse a semantically valid legacy or mixed-exact source at the canonical
/// managed-snapshot boundary.
///
/// `Ok(())` is deliberately not a transferable proof. It says only that this
/// still-borrowed candidate's 26 catalog values used struct-map v1. Completeness,
/// physical integrity, semantic rows, the conventional generation sentinel, source binding, and
/// mutation authority remain outside this check. Raw and canonical commitments
/// intentionally use distinct domains and are not compared here.
pub(crate) fn require_canonical_struct_map_source(
    candidate: &PrimaryIndexVisibleCatalogCandidate<'_>,
) -> Result<(), RawCatalogError> {
    if candidate.codec_classification != CatalogCodecClassification::AllStructMapV1 {
        return Err(RawCatalogError::CanonicalStructMapRequired);
    }
    Ok(())
}

// Keep the reviewed canonical-source gate compile-reachable while its
// migration caller remains deliberately out of this cut. This is narrower
// than suppressing dead-code diagnostics across the raw oracle module.
const _: fn(&PrimaryIndexVisibleCatalogCandidate<'_>) -> Result<(), RawCatalogError> =
    require_canonical_struct_map_source;

#[derive(Clone, Copy)]
struct SealRelationIdsV1 {
    node_tag: u64,
    permanent_guard: u64,
    meta: u64,
}

#[derive(Clone, Copy)]
struct SealCatalogFactsV1 {
    ids: SealRelationIdsV1,
    codec_classification: CatalogCodecClassification,
    vector_dimension: u64,
}

/// Consume one already-admitted clean SQLite source into the bounded routine
/// conventional-generation recognition result.
///
/// The callback and the post-close classifier are independent. Any failure is
/// terminal for this consumed source and returns
/// [`AdmissionNotEstablished`]; it is never a signal to fall through
/// to another classifier. No `DbInstance` is constructed here.
pub(crate) fn recognize_open_source_v1(
    source: ExistingSqliteSnapshotSource,
) -> Result<OpenAdmissionV1, AdmissionNotEstablished> {
    let fence = source
        .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, plan_open_assertions_v1)
        .map_err(|_| AdmissionNotEstablished)?;
    classify_closed_open_fence_v1(fence)
}

/// Positive capture-generation recognition after the ordinary old/current
/// classifier has failed. This is not a permissive fallback: only the exact
/// capture marker with the complete struct-map catalog can mint a writer permit.
pub(crate) fn recognize_capture_open_source_v1(
    source: ExistingSqliteSnapshotSource,
) -> Result<OpenAdmissionV1, AdmissionNotEstablished> {
    let fence = source
        .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
            plan_open_assertions_for(
                planner,
                [
                    STRUCT_MAP_CATALOG_GENERATION_MARKER,
                    CAPTURE_V1_CATALOG_GENERATION_MARKER,
                ],
                CatalogContract::Predecessor,
            )
        })
        .map_err(|_| AdmissionNotEstablished)?;
    classify_closed_open_fence_for(
        fence,
        [
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
            CAPTURE_V1_CATALOG_GENERATION_MARKER,
        ],
        CAPTURE_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
        Some(CatalogSealGenerationV1::CaptureV1),
        CatalogContract::Predecessor,
    )
}

/// Recognize only the exact episode generation and its distinct closed catalog.
pub(crate) fn recognize_episode_open_source_v1(
    source: ExistingSqliteSnapshotSource,
) -> Result<OpenAdmissionV1, AdmissionNotEstablished> {
    let alternatives = [
        CAPTURE_V1_CATALOG_GENERATION_MARKER,
        EPISODE_V1_CATALOG_GENERATION_MARKER,
    ];
    let fence = source
        .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::EpisodeV1, |planner| {
            plan_open_assertions_for(planner, alternatives, CatalogContract::EpisodeV1)
        })
        .map_err(|_| AdmissionNotEstablished)?;
    classify_closed_open_fence_for(
        fence,
        alternatives,
        EPISODE_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
        Some(CatalogSealGenerationV1::EpisodeV1),
        CatalogContract::EpisodeV1,
    )
}

pub(crate) fn recognize_single_graph_open_source_v1(
    source: ExistingSqliteSnapshotSource,
) -> Result<OpenAdmissionV1, AdmissionNotEstablished> {
    let alternatives = [
        EPISODE_V1_CATALOG_GENERATION_MARKER,
        SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER,
    ];
    let fence = source
        .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::SingleGraphV1, |planner| {
            plan_open_assertions_for(planner, alternatives, CatalogContract::SingleGraphV1)
        })
        .map_err(|_| AdmissionNotEstablished)?;
    classify_closed_open_fence_for(
        fence,
        alternatives,
        SINGLE_GRAPH_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
        Some(CatalogSealGenerationV1::SingleGraphV1),
        CatalogContract::SingleGraphV1,
    )
}

pub(crate) fn recognize_concern_open_source_v1(
    source: ExistingSqliteSnapshotSource,
) -> Result<OpenAdmissionV1, AdmissionNotEstablished> {
    let alternatives = [
        SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER,
        CONCERN_V1_CATALOG_GENERATION_MARKER,
    ];
    let fence = source
        .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::ConcernV1, |planner| {
            plan_open_assertions_for(planner, alternatives, CatalogContract::ConcernV1)
        })
        .map_err(|_| AdmissionNotEstablished)?;
    classify_closed_open_fence_for(
        fence,
        alternatives,
        CONCERN_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
        Some(CatalogSealGenerationV1::ConcernV1),
        CatalogContract::ConcernV1,
    )
}

pub(crate) fn recognize_episode_context_open_source_v2(
    source: ExistingSqliteSnapshotSource,
) -> Result<OpenAdmissionV1, AdmissionNotEstablished> {
    let alternatives = [
        CONCERN_V1_CATALOG_GENERATION_MARKER,
        EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER,
    ];
    let fence = source
        .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::ConcernV1, |planner| {
            plan_open_assertions_for(planner, alternatives, CatalogContract::EpisodeContextV2)
        })
        .map_err(|_| AdmissionNotEstablished)?;
    classify_closed_open_fence_for(
        fence,
        alternatives,
        EPISODE_CONTEXT_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V2,
        Some(CatalogSealGenerationV1::EpisodeContextV2),
        CatalogContract::EpisodeContextV2,
    )
}

pub(crate) fn recognize_touchstones_open_source_v1(
    source: ExistingSqliteSnapshotSource,
) -> Result<OpenAdmissionV1, AdmissionNotEstablished> {
    let alternatives = [
        EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER,
        TOUCHSTONES_V1_CATALOG_GENERATION_MARKER,
    ];
    let fence = source
        .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::TouchstonesV1, |planner| {
            plan_open_assertions_for(planner, alternatives, CatalogContract::TouchstonesV1)
        })
        .map_err(|_| AdmissionNotEstablished)?;
    classify_closed_open_fence_for(
        fence,
        alternatives,
        TOUCHSTONES_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
        Some(CatalogSealGenerationV1::TouchstonesV1),
        CatalogContract::TouchstonesV1,
    )
}

const _: fn(ExistingSqliteSnapshotSource) -> Result<OpenAdmissionV1, AdmissionNotEstablished> =
    recognize_open_source_v1;

fn plan_open_assertions_v1(
    planner: &mut ManagedSqliteCatalogAssertionPlannerV1<'_>,
) -> Result<(), cozo::Error> {
    plan_open_assertions_for(
        planner,
        [
            LEGACY_CATALOG_GENERATION_MARKER,
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
        ],
        CatalogContract::Predecessor,
    )
}

fn plan_open_assertions_for(
    planner: &mut ManagedSqliteCatalogAssertionPlannerV1<'_>,
    alternatives: [&'static str; 2],
    catalog: CatalogContract,
) -> Result<(), cozo::Error> {
    let candidate = match catalog {
        CatalogContract::Predecessor => validate_primary_index_visible_catalog(planner.catalog()),
        CatalogContract::EpisodeV1 => {
            validate_episode_primary_index_visible_catalog(planner.catalog())
        }
        CatalogContract::SingleGraphV1
        | CatalogContract::ConcernV1
        | CatalogContract::EpisodeContextV2
        | CatalogContract::TouchstonesV1 => {
            validate_primary_index_visible_catalog_for(planner.catalog(), catalog)
        }
    }
    .map_err(|error| cozo::Error::msg(error.to_string()))?;
    let node_tag_id = candidate
        .relation_id(PLAN_NODE_TAG_RELATION)
        .ok_or_else(|| seal_error("bounded open admission plan lost the node_tag relation id"))?;
    let permanent_guard_id = candidate
        .relation_id(PLAN_PERMANENT_GUARD_RELATION)
        .ok_or_else(|| {
            seal_error("bounded open admission plan lost the permanent-guard relation id")
        })?;
    let meta_id = candidate
        .relation_id(PLAN_META_RELATION)
        .ok_or_else(|| seal_error("bounded open admission plan lost the meta relation id"))?;

    planner.require_exact_row_count(node_tag_id, PLAN_NODE_TAG_COUNT)?;
    planner.require_exact_row_count(permanent_guard_id, PLAN_PERMANENT_GUARD_COUNT)?;
    planner.require_string_pair(
        permanent_guard_id,
        crate::vector_projection::GUARD_KEY,
        PERMANENT_VECTOR_GUARD_VALUE,
    )?;
    planner.require_string_pair_one_of(
        VECTOR_GENERATION_OUTCOME_TAG,
        meta_id,
        crate::vector_projection::META_KEY,
        alternatives,
    )?;
    planner.require_string_pair(
        meta_id,
        canonical_contract_for(alternatives).0,
        canonical_contract_for(alternatives).1,
    )?;
    planner.require_string_pair(
        meta_id,
        crate::tag_projection::META_KEY,
        crate::tag_projection::META_VALUE,
    )?;
    Ok(())
}

/// Anti-self-attestation verifier for the catalog-only routine fence.
///
/// Unlike [`classify_closed_audit_v1`], this cannot inspect or imply a
/// physical census. It rederives only the exact catalog facts and the fixed
/// bounded assertion transcript that the light fence is permitted to carry.
fn classify_closed_open_fence_v1(
    fence: ManagedSqliteClosedCatalogFenceV1,
) -> Result<OpenAdmissionV1, AdmissionNotEstablished> {
    classify_closed_open_fence_for(
        fence,
        [
            LEGACY_CATALOG_GENERATION_MARKER,
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
        ],
        OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
        None,
        CatalogContract::Predecessor,
    )
}

struct VendorPolicy {
    snapshot: ManagedSnapshotPolicy,
    fence: ManagedCatalogFencePolicy,
    catalog: &'static [u8; 32],
    physical: &'static [u8; 32],
    source: &'static [u8; 32],
    closed_fence: &'static [u8; 32],
}

fn vendor_policy_for(catalog: CatalogContract) -> VendorPolicy {
    match catalog {
        CatalogContract::Predecessor => VendorPolicy {
            snapshot: ManagedSnapshotPolicy::V1,
            fence: ManagedCatalogFencePolicy::V1,
            catalog: &MANAGED_CATALOG_POLICY_FINGERPRINT_V1,
            physical: &MANAGED_PHYSICAL_POLICY_FINGERPRINT_V1,
            source: &MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
            closed_fence: &MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
        },
        CatalogContract::EpisodeV1 => VendorPolicy {
            snapshot: ManagedSnapshotPolicy::EpisodeV1,
            fence: ManagedCatalogFencePolicy::EpisodeV1,
            catalog: &MANAGED_EPISODE_CATALOG_POLICY_FINGERPRINT_V1,
            physical: &MANAGED_EPISODE_PHYSICAL_POLICY_FINGERPRINT_V1,
            source: &MANAGED_EPISODE_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
            closed_fence: &MANAGED_EPISODE_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
        },
        CatalogContract::SingleGraphV1 => VendorPolicy {
            snapshot: ManagedSnapshotPolicy::SingleGraphV1,
            fence: ManagedCatalogFencePolicy::SingleGraphV1,
            catalog: &MANAGED_SINGLE_GRAPH_CATALOG_POLICY_FINGERPRINT_V1,
            physical: &MANAGED_SINGLE_GRAPH_PHYSICAL_POLICY_FINGERPRINT_V1,
            source: &MANAGED_SINGLE_GRAPH_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
            closed_fence: &MANAGED_SINGLE_GRAPH_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
        },
        CatalogContract::TouchstonesV1 => VendorPolicy {
            snapshot: ManagedSnapshotPolicy::TouchstonesV1,
            fence: ManagedCatalogFencePolicy::TouchstonesV1,
            catalog: &MANAGED_TOUCHSTONES_CATALOG_POLICY_FINGERPRINT_V1,
            physical: &MANAGED_TOUCHSTONES_PHYSICAL_POLICY_FINGERPRINT_V1,
            source: &MANAGED_TOUCHSTONES_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
            closed_fence: &MANAGED_TOUCHSTONES_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
        },
        // EpisodeContextV2 adds bounded canonical JSON metadata, not a relation,
        // column, index, assertion kind, or physical census. The vendor's frozen
        // ConcernV1 physical policy is therefore reused; distinct app markers,
        // canonical assertions and classifier identities fence old writers.
        CatalogContract::ConcernV1 | CatalogContract::EpisodeContextV2 => VendorPolicy {
            snapshot: ManagedSnapshotPolicy::ConcernV1,
            fence: ManagedCatalogFencePolicy::ConcernV1,
            catalog: &MANAGED_CONCERN_CATALOG_POLICY_FINGERPRINT_V1,
            physical: &MANAGED_CONCERN_PHYSICAL_POLICY_FINGERPRINT_V1,
            source: &MANAGED_CONCERN_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
            closed_fence: &MANAGED_CONCERN_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
        },
    }
}

fn classify_closed_open_fence_for(
    fence: ManagedSqliteClosedCatalogFenceV1,
    alternatives: [&'static str; 2],
    policy_fingerprint: [u8; 32],
    exact_generation: Option<CatalogSealGenerationV1>,
    catalog: CatalogContract,
) -> Result<OpenAdmissionV1, AdmissionNotEstablished> {
    let policy = vendor_policy_for(catalog);
    let facts = rederive_closed_verifier_catalog_facts_for(fence.catalog(), catalog)
        .map_err(|_| AdmissionNotEstablished)?;
    let assertions = fence.assertions();
    if fence.snapshot_policy() != policy.snapshot
        || fence.selector() != policy.fence
        || fence.catalog_policy_fingerprint() != policy.catalog
        || fence.assertion_policy_fingerprint()
            != &MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_V1
        || fence.source_policy_fingerprint() != policy.source
        || fence.catalog_fence_policy_fingerprint() != policy.closed_fence
        || assertions.assertion_count() != 6
        || assertions.range_probe_count() != 2
        || assertions.point_read_count() != 4
        || assertions.outcomes().len() != 6
    {
        return Err(AdmissionNotEstablished);
    }

    let outcomes = assertions.outcomes();
    if !outcomes[0].matches_exact_row_count(facts.ids.node_tag, VERIFY_NODE_TAG_COUNT)
        || !outcomes[1]
            .matches_exact_row_count(facts.ids.permanent_guard, VERIFY_PERMANENT_GUARD_COUNT)
        || !outcomes[2].matches_string_pair(
            facts.ids.permanent_guard,
            crate::vector_projection::GUARD_KEY,
            PERMANENT_VECTOR_GUARD_VALUE,
        )
        || !outcomes[4].matches_string_pair(
            facts.ids.meta,
            canonical_contract_for(alternatives).0,
            canonical_contract_for(alternatives).1,
        )
        || !outcomes[5].matches_string_pair(
            facts.ids.meta,
            crate::tag_projection::META_KEY,
            crate::tag_projection::META_VALUE,
        )
    {
        return Err(AdmissionNotEstablished);
    }
    let selected_index = outcomes[3]
        .selected_index_if_string_pair_one_of(
            VECTOR_GENERATION_OUTCOME_TAG,
            facts.ids.meta,
            crate::vector_projection::META_KEY,
            alternatives,
        )
        .filter(|selected| *selected <= 1)
        .ok_or(AdmissionNotEstablished)?;

    let expected_transcript =
        catalog_fence_assertion_transcript_commitment_for(facts.ids, selected_index, alternatives);
    if assertions.transcript_commitment() != &expected_transcript {
        return Err(AdmissionNotEstablished);
    }

    let evidence = OpenAdmissionEvidenceV1 {
        fence,
        codec_classification: facts.codec_classification,
        vector_dimension: facts.vector_dimension,
        classifier_policy_fingerprint: policy_fingerprint,
    };
    if exact_generation.is_some() && selected_index != 1 {
        return Err(AdmissionNotEstablished);
    }
    match (selected_index, facts.codec_classification) {
        (
            0,
            CatalogCodecClassification::AllPositionalV0
            | CatalogCodecClassification::AllPositionalV1
            | CatalogCodecClassification::AllStructMapV1
            | CatalogCodecClassification::MixedExact,
        ) => Ok(OpenAdmissionV1::CatalogCodecUpgradeRequired(
            CatalogUpgradeEvidenceV1 { evidence },
        )),
        (1, CatalogCodecClassification::AllStructMapV1) => {
            Ok(OpenAdmissionV1::Current(CurrentOpenPermitV1 { evidence }))
        }
        (
            1,
            CatalogCodecClassification::AllPositionalV0
            | CatalogCodecClassification::AllPositionalV1
            | CatalogCodecClassification::MixedExact,
        ) => Err(AdmissionNotEstablished),
        _ => Err(AdmissionNotEstablished),
    }
}

/// Consume one managed reader into the exact closed conventional semantic seal.
///
/// Every dynamic relation id is derived from `planner.catalog()` inside the
/// HRTB callback. The returned result is historical recognition only and does
/// not admit a writer, publish the reserved marker, or authorize mutation.
pub(crate) fn close_source_bound_seal_v1(
    reader: ManagedSqliteSnapshotReader,
) -> Result<SourceBoundSealV1, cozo::Error> {
    let audit = reader.close_with_relation_assertions_v1(plan_source_bound_assertions_v1)?;
    classify_closed_audit_v1(audit)
}

pub(crate) fn close_capture_source_bound_seal_v1(
    reader: ManagedSqliteSnapshotReader,
) -> Result<SourceBoundSealV1, cozo::Error> {
    let audit = reader.close_with_relation_assertions_v1(|planner| {
        plan_source_bound_assertions_for(
            planner,
            [
                STRUCT_MAP_CATALOG_GENERATION_MARKER,
                CAPTURE_V1_CATALOG_GENERATION_MARKER,
            ],
            CatalogContract::Predecessor,
        )
    })?;
    classify_closed_audit_for(
        audit,
        [
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
            CAPTURE_V1_CATALOG_GENERATION_MARKER,
        ],
        Some(CatalogSealGenerationV1::CaptureV1),
        CatalogContract::Predecessor,
    )
}

/// Closed full-source seal for episode-v1. Predecessor entrypoints remain strict.
pub(crate) fn close_episode_source_bound_seal_v1(
    reader: ManagedSqliteSnapshotReader,
) -> Result<SourceBoundSealV1, cozo::Error> {
    let alternatives = [
        CAPTURE_V1_CATALOG_GENERATION_MARKER,
        EPISODE_V1_CATALOG_GENERATION_MARKER,
    ];
    let audit = reader.close_with_relation_assertions_v1(|planner| {
        plan_source_bound_assertions_for(planner, alternatives, CatalogContract::EpisodeV1)
    })?;
    classify_closed_audit_for(
        audit,
        alternatives,
        Some(CatalogSealGenerationV1::EpisodeV1),
        CatalogContract::EpisodeV1,
    )
}

pub(crate) fn close_single_graph_source_bound_seal_v1(
    reader: ManagedSqliteSnapshotReader,
) -> Result<SourceBoundSealV1, cozo::Error> {
    let alternatives = [
        EPISODE_V1_CATALOG_GENERATION_MARKER,
        SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER,
    ];
    let audit = reader.close_with_relation_assertions_v1(|planner| {
        plan_source_bound_assertions_for(planner, alternatives, CatalogContract::SingleGraphV1)
    })?;
    classify_closed_audit_for(
        audit,
        alternatives,
        Some(CatalogSealGenerationV1::SingleGraphV1),
        CatalogContract::SingleGraphV1,
    )
}

pub(crate) fn close_concern_source_bound_seal_v1(
    reader: ManagedSqliteSnapshotReader,
) -> Result<SourceBoundSealV1, cozo::Error> {
    let alternatives = [
        SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER,
        CONCERN_V1_CATALOG_GENERATION_MARKER,
    ];
    let audit = reader.close_with_relation_assertions_v1(|planner| {
        plan_source_bound_assertions_for(planner, alternatives, CatalogContract::ConcernV1)
    })?;
    classify_closed_audit_for(
        audit,
        alternatives,
        Some(CatalogSealGenerationV1::ConcernV1),
        CatalogContract::ConcernV1,
    )
}

pub(crate) fn close_episode_context_source_bound_seal_v2(
    reader: ManagedSqliteSnapshotReader,
) -> Result<SourceBoundSealV1, cozo::Error> {
    let alternatives = [
        CONCERN_V1_CATALOG_GENERATION_MARKER,
        EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER,
    ];
    let audit = reader.close_with_relation_assertions_v1(|planner| {
        plan_source_bound_assertions_for(planner, alternatives, CatalogContract::EpisodeContextV2)
    })?;
    classify_closed_audit_for(
        audit,
        alternatives,
        Some(CatalogSealGenerationV1::EpisodeContextV2),
        CatalogContract::EpisodeContextV2,
    )
}

pub(crate) fn close_touchstones_source_bound_seal_v1(
    reader: ManagedSqliteSnapshotReader,
) -> Result<SourceBoundSealV1, cozo::Error> {
    let alternatives = [
        EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER,
        TOUCHSTONES_V1_CATALOG_GENERATION_MARKER,
    ];
    let audit = reader.close_with_relation_assertions_v1(|planner| {
        plan_source_bound_assertions_for(planner, alternatives, CatalogContract::TouchstonesV1)
    })?;
    classify_closed_audit_for(
        audit,
        alternatives,
        Some(CatalogSealGenerationV1::TouchstonesV1),
        CatalogContract::TouchstonesV1,
    )
}

// This admission boundary intentionally lands before its production caller. A typed anonymous
// const keeps only this staged entry compile-reachable, so dead-code lints still
// cover unrelated additions in this module.
const _: fn(ManagedSqliteSnapshotReader) -> Result<SourceBoundSealV1, cozo::Error> =
    close_source_bound_seal_v1;

pub(super) fn plan_source_bound_assertions_v1(
    planner: &mut ManagedSqliteRelationAssertionPlannerV1<'_>,
) -> Result<(), cozo::Error> {
    plan_source_bound_assertions_for(
        planner,
        [
            LEGACY_CATALOG_GENERATION_MARKER,
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
        ],
        CatalogContract::Predecessor,
    )
}

fn plan_source_bound_assertions_for(
    planner: &mut ManagedSqliteRelationAssertionPlannerV1<'_>,
    alternatives: [&'static str; 2],
    catalog: CatalogContract,
) -> Result<(), cozo::Error> {
    // Keep plan construction visibly separate from the post-close verifier.
    // In particular, these trusted-name lookups and expectations are not
    // supplied through a shared assertion table that could self-attest a bad
    // name-to-role mapping on both sides.
    let candidate = match catalog {
        CatalogContract::Predecessor => validate_primary_index_visible_catalog(planner.catalog()),
        CatalogContract::EpisodeV1 => {
            validate_episode_primary_index_visible_catalog(planner.catalog())
        }
        CatalogContract::SingleGraphV1
        | CatalogContract::ConcernV1
        | CatalogContract::EpisodeContextV2
        | CatalogContract::TouchstonesV1 => {
            validate_primary_index_visible_catalog_for(planner.catalog(), catalog)
        }
    }
    .map_err(|error| cozo::Error::msg(error.to_string()))?;
    let node_tag_id = candidate
        .relation_id(PLAN_NODE_TAG_RELATION)
        .ok_or_else(|| seal_error("source-bound assertion plan lost the node_tag relation id"))?;
    let permanent_guard_id = candidate
        .relation_id(PLAN_PERMANENT_GUARD_RELATION)
        .ok_or_else(|| {
            seal_error("source-bound assertion plan lost the permanent-guard relation id")
        })?;
    let meta_id = candidate
        .relation_id(PLAN_META_RELATION)
        .ok_or_else(|| seal_error("source-bound assertion plan lost the meta relation id"))?;

    planner.require_exact_row_count(node_tag_id, PLAN_NODE_TAG_COUNT)?;
    planner.require_exact_row_count(permanent_guard_id, PLAN_PERMANENT_GUARD_COUNT)?;
    planner.require_string_pair(
        permanent_guard_id,
        crate::vector_projection::GUARD_KEY,
        PERMANENT_VECTOR_GUARD_VALUE,
    )?;
    planner.require_string_pair_one_of(
        VECTOR_GENERATION_OUTCOME_TAG,
        meta_id,
        crate::vector_projection::META_KEY,
        alternatives,
    )?;
    planner.require_string_pair(
        meta_id,
        canonical_contract_for(alternatives).0,
        canonical_contract_for(alternatives).1,
    )?;
    planner.require_string_pair(
        meta_id,
        crate::tag_projection::META_KEY,
        crate::tag_projection::META_VALUE,
    )?;
    Ok(())
}

/// Anti-self-attestation boundary: tests deliberately feed this function
/// independently planned, generically valid audits that it must reject.
pub(super) fn classify_closed_audit_v1(
    audit: ManagedSqliteClosedAuditV1,
) -> Result<SourceBoundSealV1, cozo::Error> {
    classify_closed_audit_for(
        audit,
        [
            LEGACY_CATALOG_GENERATION_MARKER,
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
        ],
        None,
        CatalogContract::Predecessor,
    )
}

fn classify_closed_audit_for(
    audit: ManagedSqliteClosedAuditV1,
    alternatives: [&'static str; 2],
    exact_generation: Option<CatalogSealGenerationV1>,
    catalog: CatalogContract,
) -> Result<SourceBoundSealV1, cozo::Error> {
    let policy = vendor_policy_for(catalog);
    let facts = rederive_closed_verifier_catalog_facts_for(audit.catalog(), catalog)?;
    let assertions = audit.assertions();
    if audit.policy() != policy.snapshot {
        return Err(seal_error(
            "closed audit used an unsupported snapshot policy",
        ));
    }
    if assertions.policy_fingerprint() != &MANAGED_ASSERTION_POLICY_FINGERPRINT_V1 {
        return Err(seal_error(
            "closed audit used an unsupported generic assertion policy",
        ));
    }
    if audit.catalog().policy_fingerprint() != policy.catalog
        || audit.physical().policy_fingerprint() != policy.physical
        || audit.source().policy_fingerprint() != policy.source
    {
        return Err(seal_error(
            "closed audit used an unsupported catalog, physical, or source policy",
        ));
    }
    if assertions.assertion_count() != 6
        || assertions.point_read_count() != 4
        || assertions.outcomes().len() != 6
    {
        return Err(seal_error(
            "closed audit did not contain the exact source-bound assertion inventory",
        ));
    }

    let outcomes = assertions.outcomes();
    if !outcomes[0].matches_exact_row_count(facts.ids.node_tag, VERIFY_NODE_TAG_COUNT)
        || !outcomes[1]
            .matches_exact_row_count(facts.ids.permanent_guard, VERIFY_PERMANENT_GUARD_COUNT)
        || !outcomes[2].matches_string_pair(
            facts.ids.permanent_guard,
            crate::vector_projection::GUARD_KEY,
            PERMANENT_VECTOR_GUARD_VALUE,
        )
        || !outcomes[4].matches_string_pair(
            facts.ids.meta,
            canonical_contract_for(alternatives).0,
            canonical_contract_for(alternatives).1,
        )
        || !outcomes[5].matches_string_pair(
            facts.ids.meta,
            crate::tag_projection::META_KEY,
            crate::tag_projection::META_VALUE,
        )
    {
        return Err(seal_error(
            "closed audit outcomes did not match the exact ordered source-bound seal",
        ));
    }
    let Some(selected_index) = outcomes[3].selected_index_if_string_pair_one_of(
        VECTOR_GENERATION_OUTCOME_TAG,
        facts.ids.meta,
        crate::vector_projection::META_KEY,
        alternatives,
    ) else {
        return Err(seal_error(
            "closed audit vector-generation outcome did not match the exact source-bound seal",
        ));
    };
    if selected_index > 1 {
        return Err(seal_error(
            "closed audit vector-generation outcome selected an invalid alternative",
        ));
    }

    let expected_transcript =
        generic_assertion_transcript_commitment_for(facts.ids, selected_index, alternatives);
    if assertions.transcript_commitment() != &expected_transcript {
        return Err(seal_error(
            "closed audit generic assertion transcript did not match the independently rebuilt conventional transcript",
        ));
    }
    if exact_generation.is_some() && selected_index != 1 {
        return Err(seal_error("seal did not select its exact generation"));
    }
    let generation = match (selected_index, facts.codec_classification) {
        (
            0,
            CatalogCodecClassification::AllPositionalV0
            | CatalogCodecClassification::AllPositionalV1
            | CatalogCodecClassification::AllStructMapV1
            | CatalogCodecClassification::MixedExact,
        ) => CatalogSealGenerationV1::LegacyNeedsMigration,
        (1, CatalogCodecClassification::AllStructMapV1) => {
            exact_generation.unwrap_or(CatalogSealGenerationV1::CurrentStructMap)
        }
        (
            1,
            CatalogCodecClassification::AllPositionalV0
            | CatalogCodecClassification::AllPositionalV1
            | CatalogCodecClassification::MixedExact,
        ) => {
            return Err(seal_error(
                "catalog-struct-map conventional generation marker was paired with legacy or mixed catalog bytes",
            ));
        }
        _ => {
            return Err(seal_error(
                "closed audit vector-generation selection was outside the exact contract matrix",
            ));
        }
    };

    Ok(SourceBoundSealV1 {
        audit,
        generation,
        codec_classification: facts.codec_classification,
        vector_dimension: facts.vector_dimension,
        policy_fingerprint: if catalog == CatalogContract::TouchstonesV1 {
            TOUCHSTONES_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1
        } else if catalog == CatalogContract::EpisodeContextV2 {
            EPISODE_CONTEXT_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V2
        } else if catalog == CatalogContract::ConcernV1 {
            CONCERN_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1
        } else if catalog == CatalogContract::SingleGraphV1 {
            SINGLE_GRAPH_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1
        } else if catalog == CatalogContract::EpisodeV1 {
            EPISODE_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1
        } else {
            SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1
        },
    })
}

fn rederive_closed_verifier_catalog_facts_for(
    observation: &ManagedSqlitePrimaryIndexCatalogV1,
    catalog: CatalogContract,
) -> Result<SealCatalogFactsV1, cozo::Error> {
    let candidate = match catalog {
        CatalogContract::Predecessor => validate_primary_index_visible_catalog(observation),
        CatalogContract::EpisodeV1 => validate_episode_primary_index_visible_catalog(observation),
        CatalogContract::SingleGraphV1
        | CatalogContract::ConcernV1
        | CatalogContract::EpisodeContextV2
        | CatalogContract::TouchstonesV1 => {
            validate_primary_index_visible_catalog_for(observation, catalog)
        }
    }
    .map_err(|error| cozo::Error::msg(error.to_string()))?;
    let relation_id = |trusted_name| {
        candidate.relation_id(trusted_name).ok_or_else(|| {
            seal_error(
                "validated conventional unmanaged catalog lost a required trusted relation id",
            )
        })
    };
    Ok(SealCatalogFactsV1 {
        ids: SealRelationIdsV1 {
            node_tag: relation_id(VERIFY_NODE_TAG_RELATION)?,
            permanent_guard: relation_id(VERIFY_PERMANENT_GUARD_RELATION)?,
            meta: relation_id(VERIFY_META_RELATION)?,
        },
        codec_classification: candidate.codec_classification(),
        vector_dimension: candidate.vector_dimension(),
    })
}

fn seal_error(message: &'static str) -> cozo::Error {
    cozo::Error::msg(message)
}

#[cfg(test)]
fn generic_assertion_transcript_commitment_v1(
    ids: SealRelationIdsV1,
    selected_index: u8,
) -> [u8; 32] {
    generic_assertion_transcript_commitment_for(
        ids,
        selected_index,
        [
            LEGACY_CATALOG_GENERATION_MARKER,
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
        ],
    )
}

fn generic_assertion_transcript_commitment_for(
    ids: SealRelationIdsV1,
    selected_index: u8,
    alternatives: [&str; 2],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1);
    append_count_transcript_entry(&mut hasher, ids.node_tag, VERIFY_NODE_TAG_COUNT);
    append_count_transcript_entry(
        &mut hasher,
        ids.permanent_guard,
        VERIFY_PERMANENT_GUARD_COUNT,
    );
    append_exact_pair_transcript_entry(
        &mut hasher,
        ids.permanent_guard,
        crate::vector_projection::GUARD_KEY,
        PERMANENT_VECTOR_GUARD_VALUE,
    );
    append_one_of_transcript_entry(
        &mut hasher,
        VECTOR_GENERATION_OUTCOME_TAG,
        ids.meta,
        crate::vector_projection::META_KEY,
        alternatives,
        selected_index,
    );
    append_exact_pair_transcript_entry(
        &mut hasher,
        ids.meta,
        canonical_contract_for(alternatives).0,
        canonical_contract_for(alternatives).1,
    );
    append_exact_pair_transcript_entry(
        &mut hasher,
        ids.meta,
        crate::tag_projection::META_KEY,
        crate::tag_projection::META_VALUE,
    );
    hasher.update([0xff]);
    hasher.update(6_u64.to_be_bytes());
    hasher.finalize().into()
}

fn catalog_fence_assertion_transcript_commitment_for(
    ids: SealRelationIdsV1,
    selected_index: u8,
    alternatives: [&str; 2],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(MANAGED_CATALOG_FENCE_ASSERTION_TRANSCRIPT_DOMAIN_V1);
    append_count_transcript_entry(&mut hasher, ids.node_tag, VERIFY_NODE_TAG_COUNT);
    append_count_transcript_entry(
        &mut hasher,
        ids.permanent_guard,
        VERIFY_PERMANENT_GUARD_COUNT,
    );
    append_exact_pair_transcript_entry(
        &mut hasher,
        ids.permanent_guard,
        crate::vector_projection::GUARD_KEY,
        PERMANENT_VECTOR_GUARD_VALUE,
    );
    append_one_of_transcript_entry(
        &mut hasher,
        VECTOR_GENERATION_OUTCOME_TAG,
        ids.meta,
        crate::vector_projection::META_KEY,
        alternatives,
        selected_index,
    );
    append_exact_pair_transcript_entry(
        &mut hasher,
        ids.meta,
        canonical_contract_for(alternatives).0,
        canonical_contract_for(alternatives).1,
    );
    append_exact_pair_transcript_entry(
        &mut hasher,
        ids.meta,
        crate::tag_projection::META_KEY,
        crate::tag_projection::META_VALUE,
    );
    hasher.update([0xff]);
    hasher.update(6_u64.to_be_bytes());
    hasher.finalize().into()
}

fn append_count_transcript_entry(hasher: &mut Sha256, relation_id: u64, expected: u64) {
    let kind = [0_u8];
    let relation_id = relation_id.to_be_bytes();
    let expected = expected.to_be_bytes();
    append_transcript_entry(
        hasher,
        &[kind.as_slice(), relation_id.as_slice(), expected.as_slice()],
    );
}

fn append_exact_pair_transcript_entry(
    hasher: &mut Sha256,
    relation_id: u64,
    key: &str,
    value: &str,
) {
    let kind = [1_u8];
    let relation_id = relation_id.to_be_bytes();
    append_transcript_entry(
        hasher,
        &[
            kind.as_slice(),
            relation_id.as_slice(),
            key.as_bytes(),
            value.as_bytes(),
        ],
    );
}

fn append_one_of_transcript_entry(
    hasher: &mut Sha256,
    outcome_tag: &str,
    relation_id: u64,
    key: &str,
    alternatives: [&str; 2],
    selected_index: u8,
) {
    let kind = [2_u8];
    let relation_id = relation_id.to_be_bytes();
    let selected_index = [selected_index];
    append_transcript_entry(
        hasher,
        &[
            kind.as_slice(),
            outcome_tag.as_bytes(),
            relation_id.as_slice(),
            key.as_bytes(),
            alternatives[0].as_bytes(),
            alternatives[1].as_bytes(),
            selected_index.as_slice(),
        ],
    );
}

fn append_transcript_entry(hasher: &mut Sha256, fields: &[&[u8]]) {
    hasher.update([0x01]);
    for field in fields {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    hasher.update((fields.len() as u64).to_be_bytes());
}

fn classify_codecs(
    codecs: impl IntoIterator<Item = ManagedCatalogEncodingV1>,
) -> CatalogCodecClassification {
    let mut positional_v0 = 0_usize;
    let mut positional_v1 = 0_usize;
    let mut struct_map_v1 = 0_usize;
    let mut total = 0_usize;
    for codec in codecs {
        total += 1;
        match codec {
            ManagedCatalogEncodingV1::PositionalV0 => positional_v0 += 1,
            ManagedCatalogEncodingV1::PositionalV1 => positional_v1 += 1,
            ManagedCatalogEncodingV1::StructMapV1 => struct_map_v1 += 1,
        }
    }
    if total != 0 && positional_v0 == total {
        CatalogCodecClassification::AllPositionalV0
    } else if total != 0 && positional_v1 == total {
        CatalogCodecClassification::AllPositionalV1
    } else if total != 0 && struct_map_v1 == total {
        CatalogCodecClassification::AllStructMapV1
    } else {
        CatalogCodecClassification::MixedExact
    }
}

fn relation_at<'a>(slots: &[Option<&'a ManagedRelationV1>], slot: usize) -> &'a ManagedRelationV1 {
    // The complete-slot check in the sole caller dominates every use.
    slots[slot].expect("validated conventional relation slot must be populated")
}

fn validate_relation_shape(
    relation: &ManagedRelationV1,
    expected: RelationSpec,
) -> Result<(), RawCatalogError> {
    let expected_access = match expected.durable_access {
        DurableAccess::Normal => ManagedAccessLevelV1::Normal,
        DurableAccess::ReadOnly => ManagedAccessLevelV1::ReadOnly,
    };
    let forbidden = FORBIDDEN_FEATURES;
    if relation.name() != expected.name
        || relation.access_level() != expected_access
        || (!forbidden.allows_temporary_relations() && relation.is_temporary())
        || (!forbidden.allows_triggers()
            && (relation.put_triggers().next().is_some()
                || relation.remove_triggers().next().is_some()
                || relation.replace_triggers().next().is_some()))
        || (!forbidden.allows_descriptions() && !relation.description().is_empty())
        || (!forbidden.allows_temporal_floors() && relation.temporal_gc_floor().is_some())
        || (!forbidden.allows_lsh_indices() && relation.lsh_index_count() != 0)
    {
        return Err(RawCatalogError::RelationShape {
            expected_relation: expected.name,
        });
    }

    validate_columns(relation, expected)
}

fn validate_leaf_relation(
    relation: &ManagedRelationV1,
    expected: RelationSpec,
) -> Result<(), RawCatalogError> {
    validate_relation_shape(relation, expected)?;
    if !relation.normal_indices().is_empty()
        || !relation.hnsw_indices().is_empty()
        || !relation.fts_indices().is_empty()
    {
        return Err(RawCatalogError::NestedChild {
            expected_relation: expected.name,
        });
    }
    Ok(())
}

fn validate_columns(
    relation: &ManagedRelationV1,
    expected: RelationSpec,
) -> Result<(), RawCatalogError> {
    let expected_keys = expected.columns.iter().filter(|column| column.is_key);
    let expected_non_keys = expected.columns.iter().filter(|column| !column.is_key);
    if relation.keys().len() != expected_keys.clone().count()
        || relation.non_keys().len() != expected_non_keys.clone().count()
    {
        return Err(RawCatalogError::ColumnLayout {
            expected_relation: expected.name,
        });
    }

    for (actual, column) in relation.keys().iter().zip(expected_keys) {
        validate_column(actual, expected.name, column)?;
    }
    for (actual, column) in relation.non_keys().iter().zip(expected_non_keys) {
        validate_column(actual, expected.name, column)?;
    }
    Ok(())
}

fn validate_column(
    actual: &ManagedColumnV1,
    expected_relation: &'static str,
    expected: &ColumnSpec,
) -> Result<(), RawCatalogError> {
    if actual.name() != expected.name
        || (!FORBIDDEN_FEATURES.allows_column_defaults() && actual.default_present())
        || !column_type_matches(actual.typing(), expected.ty)
    {
        return Err(RawCatalogError::Column {
            expected_relation,
            expected_column: expected.name,
        });
    }
    Ok(())
}

fn column_type_matches(actual: &ManagedColumnTypeV1, expected: ColumnType) -> bool {
    match expected {
        ColumnType::String => {
            !actual.nullable() && matches!(actual.kind(), ManagedColumnTypeKindV1::String)
        }
        ColumnType::NullableString => {
            actual.nullable() && matches!(actual.kind(), ManagedColumnTypeKindV1::String)
        }
        ColumnType::Bool => {
            !actual.nullable() && matches!(actual.kind(), ManagedColumnTypeKindV1::Bool)
        }
        ColumnType::Int => {
            !actual.nullable() && matches!(actual.kind(), ManagedColumnTypeKindV1::Int)
        }
        ColumnType::NullableInt => {
            actual.nullable() && matches!(actual.kind(), ManagedColumnTypeKindV1::Int)
        }
        ColumnType::Float => {
            !actual.nullable() && matches!(actual.kind(), ManagedColumnTypeKindV1::Float)
        }
        ColumnType::NullableBytes => {
            actual.nullable() && matches!(actual.kind(), ManagedColumnTypeKindV1::Bytes)
        }
        ColumnType::IntList => {
            if actual.nullable() {
                return false;
            }
            matches!(
                actual.kind(),
                ManagedColumnTypeKindV1::List {
                    element,
                    length: None,
                } if !element.nullable()
                    && matches!(element.kind(), ManagedColumnTypeKindV1::Int)
            )
        }
        ColumnType::F32Vector => {
            matches!(
                actual.kind(),
                ManagedColumnTypeKindV1::Vector {
                    element: ManagedVectorElementTypeV1::F32,
                    length,
                } if !actual.nullable() && length > 0
            )
        }
    }
}

fn derive_vector_dimension(
    slots: &[Option<&ManagedRelationV1>],
    binding: VectorDimensionBinding,
    catalog: CatalogContract,
) -> Result<u64, RawCatalogError> {
    let Some(slot) = catalog.relation_slot(binding.relation) else {
        return Err(RawCatalogError::VectorDimension {
            expected_relation: binding.relation,
            expected_column: binding.column,
        });
    };
    let relation = relation_at(slots, slot);
    let mut matching = relation
        .keys()
        .iter()
        .chain(relation.non_keys())
        .filter(|column| column.name() == binding.column);
    let Some(column) = matching.next() else {
        return Err(RawCatalogError::VectorDimension {
            expected_relation: binding.relation,
            expected_column: binding.column,
        });
    };
    if matching.next().is_some() || column.typing().nullable() {
        return Err(RawCatalogError::VectorDimension {
            expected_relation: binding.relation,
            expected_column: binding.column,
        });
    }
    match column.typing().kind() {
        ManagedColumnTypeKindV1::Vector {
            element: ManagedVectorElementTypeV1::F32,
            length,
        } if length > 0 => Ok(length),
        _ => Err(RawCatalogError::VectorDimension {
            expected_relation: binding.relation,
            expected_column: binding.column,
        }),
    }
}

fn validate_indices(
    owner: &ManagedRelationV1,
    owner_name: &'static str,
    slots: &[Option<&ManagedRelationV1>],
    vector_dimension: u64,
    catalog: CatalogContract,
) -> Result<(), RawCatalogError> {
    let expected_normal_count = catalog
        .indices_for(owner_name)
        .filter(|child| matches!(child.index, IndexSpec::Normal(_)))
        .count();
    let expected_fts_count = catalog
        .indices_for(owner_name)
        .filter(|child| matches!(child.index, IndexSpec::Fts(_)))
        .count();
    let expected_hnsw_count = catalog
        .indices_for(owner_name)
        .filter(|child| matches!(child.index, IndexSpec::Hnsw(_)))
        .count();
    if owner.normal_indices().len() != expected_normal_count
        || owner.fts_indices().len() != expected_fts_count
        || owner.hnsw_indices().len() != expected_hnsw_count
    {
        return Err(RawCatalogError::IndexInventory {
            owner_relation: owner_name,
        });
    }

    // Mnestic normalizes index maps lexically. Match by trusted local name;
    // declaration order in spec is a separate concern.
    for child in catalog.indices_for(owner_name) {
        match child.index {
            IndexSpec::Normal(expected) => {
                let actual = unique_normal_index(owner.normal_indices(), child)?;
                if actual.fields() != expected.ordered_columns {
                    return Err(index_manifest_error(child));
                }
                validate_nested_child(actual.relation(), child, slots, catalog)?;
            }
            IndexSpec::Fts(expected) => {
                let actual = unique_fts_index(owner.fts_indices(), child)?;
                validate_fts_manifest(actual, child, expected)?;
                validate_nested_child(actual.relation(), child, slots, catalog)?;
            }
            IndexSpec::Hnsw(expected) => {
                let actual = unique_hnsw_index(owner.hnsw_indices(), child)?;
                let bound_dimension =
                    derive_vector_dimension(slots, expected.catalog.dimension, catalog)?;
                if bound_dimension != vector_dimension {
                    return Err(index_manifest_error(child));
                }
                validate_hnsw_manifest(actual, child, expected, bound_dimension)?;
                validate_nested_child(actual.relation(), child, slots, catalog)?;
            }
        }
    }
    Ok(())
}

fn unique_normal_index<'a>(
    indices: &'a [ManagedNormalIndexV1],
    expected: &DerivedRelationSpec,
) -> Result<&'a ManagedNormalIndexV1, RawCatalogError> {
    let mut matching = indices
        .iter()
        .filter(|index| index.name() == expected.local_name);
    let Some(index) = matching.next() else {
        return Err(index_manifest_error(expected));
    };
    if matching.next().is_some() {
        return Err(index_manifest_error(expected));
    }
    Ok(index)
}

fn unique_fts_index<'a>(
    indices: &'a [ManagedFtsIndexV1],
    expected: &DerivedRelationSpec,
) -> Result<&'a ManagedFtsIndexV1, RawCatalogError> {
    let mut matching = indices
        .iter()
        .filter(|index| index.name() == expected.local_name);
    let Some(index) = matching.next() else {
        return Err(index_manifest_error(expected));
    };
    if matching.next().is_some() {
        return Err(index_manifest_error(expected));
    }
    Ok(index)
}

fn unique_hnsw_index<'a>(
    indices: &'a [ManagedHnswIndexV1],
    expected: &DerivedRelationSpec,
) -> Result<&'a ManagedHnswIndexV1, RawCatalogError> {
    let mut matching = indices
        .iter()
        .filter(|index| index.name() == expected.local_name);
    let Some(index) = matching.next() else {
        return Err(index_manifest_error(expected));
    };
    if matching.next().is_some() {
        return Err(index_manifest_error(expected));
    }
    Ok(index)
}

fn validate_nested_child(
    actual: &ManagedRelationV1,
    expected: &DerivedRelationSpec,
    slots: &[Option<&ManagedRelationV1>],
    catalog: CatalogContract,
) -> Result<(), RawCatalogError> {
    let Some(child_slot) = catalog.relation_slot(expected.relation.name) else {
        return Err(RawCatalogError::NestedChild {
            expected_relation: expected.relation.name,
        });
    };
    let top_level = relation_at(slots, child_slot);
    if actual.id() != top_level.id() {
        return Err(RawCatalogError::NestedChild {
            expected_relation: expected.relation.name,
        });
    }
    validate_leaf_relation(actual, expected.relation)
}

fn validate_fts_manifest(
    actual: &ManagedFtsIndexV1,
    expected_child: &DerivedRelationSpec,
    expected: FtsIndexSpec,
) -> Result<(), RawCatalogError> {
    let manifest = actual.manifest();
    if actual.name() != expected_child.local_name
        || manifest.base_relation() != expected_child.owner_relation
        || manifest.index_name() != expected_child.local_name
        || manifest.extractor() != expected.extractor
        || !tokenizer_matches(manifest.tokenizer(), expected.tokenizer)
        || manifest.filters().len() != expected.tokenizer_filters.len()
        || !manifest
            .filters()
            .iter()
            .zip(expected.tokenizer_filters)
            .all(|(actual, expected)| tokenizer_matches(actual, *expected))
    {
        return Err(index_manifest_error(expected_child));
    }
    Ok(())
}

fn tokenizer_matches(actual: &ManagedTokenizerV1, expected: TokenizerSpec) -> bool {
    actual.name() == expected.name && actual.argument_count() == expected.argument_count
}

fn validate_hnsw_manifest(
    actual: &ManagedHnswIndexV1,
    expected_child: &DerivedRelationSpec,
    expected: HnswIndexSpec,
    vector_dimension: u64,
) -> Result<(), RawCatalogError> {
    let manifest = actual.manifest();
    let catalog = expected.catalog;
    let expected_dtype = match catalog.dtype {
        VectorElementType::F32 => ManagedVectorElementTypeV1::F32,
    };
    let expected_distance = match catalog.distance {
        HnswDistance::Cosine => ManagedHnswDistanceV1::Cosine,
    };
    if actual.name() != expected_child.local_name
        || manifest.base_relation() != expected_child.owner_relation
        || manifest.index_name() != expected_child.local_name
        || manifest.vector_dimension() != vector_dimension
        || manifest.dtype() != expected_dtype
        || manifest.vector_fields() != catalog.vector_fields
        || manifest.distance() != expected_distance
        || manifest.ef_construction() != catalog.ef_construction
        || manifest.m_neighbours() != catalog.m_neighbours
        || manifest.m_max() != catalog.m_max
        || manifest.m_max0() != catalog.m_max0
        || manifest.level_multiplier_bits() != catalog.level_multiplier_bits
        || manifest.index_filter() != Some(expected.index_filter)
        || manifest.extend_candidates() != catalog.extend_candidates
        || manifest.keep_pruned_connections() != catalog.keep_pruned_connections
    {
        return Err(index_manifest_error(expected_child));
    }
    Ok(())
}

fn index_manifest_error(expected: &DerivedRelationSpec) -> RawCatalogError {
    RawCatalogError::IndexManifest {
        owner_relation: expected.owner_relation,
        expected_index: expected.local_name,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use cozo::{
        DbInstance, ExistingSqliteSnapshotSource, ManagedCatalogFixtureV1, ManagedSnapshotPolicy,
        ManagedSqliteSnapshotReader, ScriptMutability,
    };
    use ulid::Ulid;

    use super::*;

    const ATTACKER_SENTINEL: &str = "attacker_sentinel_must_never_reach_diagnostics";

    struct PersistentCatalogFixture {
        root: PathBuf,
        path: PathBuf,
    }

    impl PersistentCatalogFixture {
        fn isolated_path(label: &str) -> (PathBuf, PathBuf) {
            let temp_root = std::fs::canonicalize(std::env::temp_dir())
                .expect("canonical temporary-directory path");
            let root = temp_root.join(format!("{label}-{}", Ulid::new()));
            std::fs::create_dir(&root).expect("create isolated catalog fixture directory");
            let path = root.join("memory.db");
            (root, path)
        }

        fn fresh() -> Self {
            let (root, path) = Self::isolated_path("mneme-conventional-raw");
            let store =
                crate::CozoStore::open_legacy_fixture(path.to_str().expect("UTF-8 test path"), 4)
                    .expect("fresh persistent conventional unmanaged store");
            store
                .prepare_for_file_move()
                .expect("detach fresh persistent conventional unmanaged store");
            drop(store);
            Self { root, path }
        }

        fn mutate(&self, scripts: &[&str]) {
            let db = DbInstance::new("sqlite", self.path.to_str().expect("UTF-8 test path"), "")
                .expect("open raw persistent test store");
            for script in scripts {
                db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
                    .expect("reachable Cozo catalog mutation");
            }
            db.prepare_sqlite_for_file_move()
                .expect("detach mutated persistent test store");
            drop(db);
        }

        fn put_meta(&self, key: &str, value: &str) {
            let script = format!("?[k, v] <- [['{key}', '{value}']] :put meta {{k => v}}");
            self.mutate(&[script.as_str()]);
        }

        fn remove_meta(&self, key: &str) {
            let script = format!("?[k] <- [['{key}']] :rm meta {{k}}");
            self.mutate(&[script.as_str()]);
        }

        fn put_guard(&self, key: &str, value: &str) {
            let make_writable = format!(
                "::access_level normal {}",
                crate::vector_projection::LEGACY_SHADOW_GUARD
            );
            let put = format!(
                "?[fence, generation] <- [['{key}', '{value}']] :put {} {{fence => generation}}",
                crate::vector_projection::LEGACY_SHADOW_GUARD
            );
            let make_read_only = format!(
                "::access_level read_only {}",
                crate::vector_projection::LEGACY_SHADOW_GUARD
            );
            self.mutate(&[
                make_writable.as_str(),
                put.as_str(),
                make_read_only.as_str(),
            ]);
        }

        fn remove_guard(&self, key: &str) {
            let make_writable = format!(
                "::access_level normal {}",
                crate::vector_projection::LEGACY_SHADOW_GUARD
            );
            let remove = format!(
                "?[fence] <- [['{key}']] :rm {} {{fence}}",
                crate::vector_projection::LEGACY_SHADOW_GUARD
            );
            let make_read_only = format!(
                "::access_level read_only {}",
                crate::vector_projection::LEGACY_SHADOW_GUARD
            );
            self.mutate(&[
                make_writable.as_str(),
                remove.as_str(),
                make_read_only.as_str(),
            ]);
        }

        fn insert_node_tag(&self) {
            self.mutate(&[
                "::access_level normal node_tag",
                "?[id, tag] <- [['hostile-node', 'hostile-tag']] :put node_tag {id, tag}",
                "::access_level read_only node_tag",
            ]);
        }

        fn reader(&self) -> ManagedSqliteSnapshotReader {
            ExistingSqliteSnapshotSource::open(&self.path)
                .expect("open genuine SQLite snapshot source")
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .expect("enter managed snapshot reader")
        }

        fn catalog_fixture(&self, fixture: ManagedCatalogFixtureV1) -> Self {
            let (root, path) = Self::isolated_path("mneme-conventional-catalog-fixture");
            let source = ExistingSqliteSnapshotSource::open(&self.path)
                .expect("open genuine conventional fixture source");
            source
                .backup_to_new_with_catalog_fixture_v1_for_tests(&path, fixture)
                .expect("build closed catalog fixture");
            Self { root, path }
        }
    }

    impl Drop for PersistentCatalogFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn validation_error(fixture: &PersistentCatalogFixture) -> RawCatalogError {
        let mut reader = fixture.reader();
        let outcome = validate_primary_index_visible_catalog(
            reader
                .inspect_primary_index_catalog_v1()
                .expect("mutated catalog remains syntactically reachable"),
        )
        .map(|candidate| candidate.vector_dimension());
        reader.close_and_verify().expect("close managed reader");
        outcome.expect_err("corrupt conventional unmanaged catalog must fail semantic validation")
    }

    fn seal_error(fixture: &PersistentCatalogFixture) -> String {
        close_source_bound_seal_v1(fixture.reader())
            .map(|_| ())
            .expect_err("hostile catalog fixture must not produce a closed seal")
            .to_string()
    }

    fn independently_frame_entries(
        domain: &[u8],
        entries: &[Vec<Vec<u8>>],
        count: u64,
    ) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(domain);
        for fields in entries {
            hasher.update([0x01]);
            for field in fields {
                hasher.update((field.len() as u64).to_be_bytes());
                hasher.update(field);
            }
            hasher.update((fields.len() as u64).to_be_bytes());
        }
        hasher.update([0xff]);
        hasher.update(count.to_be_bytes());
        hasher.finalize().into()
    }

    fn independent_generic_entries(
        ids: SealRelationIdsV1,
        selected_index: u8,
    ) -> Vec<Vec<Vec<u8>>> {
        vec![
            vec![
                vec![0],
                ids.node_tag.to_be_bytes().to_vec(),
                0_u64.to_be_bytes().to_vec(),
            ],
            vec![
                vec![0],
                ids.permanent_guard.to_be_bytes().to_vec(),
                1_u64.to_be_bytes().to_vec(),
            ],
            vec![
                vec![1],
                ids.permanent_guard.to_be_bytes().to_vec(),
                crate::vector_projection::GUARD_KEY.as_bytes().to_vec(),
                PERMANENT_VECTOR_GUARD_VALUE.as_bytes().to_vec(),
            ],
            vec![
                vec![2],
                VECTOR_GENERATION_OUTCOME_TAG.as_bytes().to_vec(),
                ids.meta.to_be_bytes().to_vec(),
                crate::vector_projection::META_KEY.as_bytes().to_vec(),
                LEGACY_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
                STRUCT_MAP_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
                vec![selected_index],
            ],
            vec![
                vec![1],
                ids.meta.to_be_bytes().to_vec(),
                crate::canonical_node_contract::META_KEY.as_bytes().to_vec(),
                crate::canonical_node_contract::META_VALUE
                    .as_bytes()
                    .to_vec(),
            ],
            vec![
                vec![1],
                ids.meta.to_be_bytes().to_vec(),
                crate::tag_projection::META_KEY.as_bytes().to_vec(),
                crate::tag_projection::META_VALUE.as_bytes().to_vec(),
            ],
        ]
    }

    #[test]
    fn generic_transcript_has_independent_selected_zero_and_one_kats() {
        let ids = SealRelationIdsV1 {
            node_tag: 0x0102_0304_0506,
            permanent_guard: 0x0a0b_0c0d_0e0f,
            meta: 0x0011_2233_4455,
        };
        let selected_zero_entries = independent_generic_entries(ids, 0);
        let selected_one_entries = independent_generic_entries(ids, 1);
        let selected_zero = independently_frame_entries(
            MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1,
            &selected_zero_entries,
            6,
        );
        let selected_one = independently_frame_entries(
            MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1,
            &selected_one_entries,
            6,
        );

        assert_eq!(
            selected_zero,
            [
                242, 209, 156, 193, 22, 61, 175, 242, 172, 200, 220, 222, 130, 146, 55, 49, 33,
                119, 75, 42, 37, 28, 26, 227, 118, 186, 212, 211, 120, 188, 226, 47,
            ],
            "repin the independent selected-index-0 conventional transcript KAT"
        );
        assert_eq!(
            selected_one,
            [
                215, 28, 202, 19, 100, 213, 36, 252, 19, 185, 244, 115, 65, 195, 0, 171, 206, 195,
                63, 135, 204, 197, 112, 169, 66, 10, 251, 230, 117, 66, 212, 187,
            ],
            "repin the independent selected-index-1 conventional transcript KAT"
        );
        assert_eq!(
            selected_zero,
            generic_assertion_transcript_commitment_v1(ids, 0)
        );
        assert_eq!(
            selected_one,
            generic_assertion_transcript_commitment_v1(ids, 1)
        );
        assert_ne!(selected_zero, selected_one);

        for entries in [&selected_zero_entries, &selected_one_entries] {
            let baseline =
                independently_frame_entries(MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1, entries, 6);
            for (entry_index, entry) in entries.iter().enumerate() {
                for field_index in 0..entry.len() {
                    let mut mutated = entries.clone();
                    mutated[entry_index][field_index][0] ^= 1;
                    assert_ne!(
                        independently_frame_entries(
                            MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1,
                            &mutated,
                            6,
                        ),
                        baseline,
                        "mutating entry {entry_index} field {field_index} must change the transcript"
                    );
                }
                for field_index in 0..entry.len().saturating_sub(1) {
                    let mut reordered_fields = entries.clone();
                    reordered_fields[entry_index].swap(field_index, field_index + 1);
                    assert_ne!(
                        independently_frame_entries(
                            MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1,
                            &reordered_fields,
                            6,
                        ),
                        baseline,
                        "reordering fields must change the transcript"
                    );
                }
            }
            for entry_index in 0..entries.len() - 1 {
                let mut reordered_entries = entries.clone();
                reordered_entries.swap(entry_index, entry_index + 1);
                assert_ne!(
                    independently_frame_entries(
                        MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1,
                        &reordered_entries,
                        6,
                    ),
                    baseline,
                    "reordering entries must change the transcript"
                );
            }
            assert_ne!(
                independently_frame_entries(b"wrong-domain\0", entries, 6),
                baseline
            );
            assert_ne!(
                independently_frame_entries(MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1, entries, 5),
                baseline
            );
            assert_ne!(
                independently_frame_entries(MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1, entries, 7),
                baseline
            );
        }
    }

    #[test]
    fn source_bound_classifier_policy_has_an_independent_kat() {
        let entries = vec![
            vec![
                b"authority".to_vec(),
                b"recognition-only-closed-historical-evidence".to_vec(),
            ],
            vec![
                b"constituent-policies-in-order".to_vec(),
                MANAGED_CATALOG_POLICY_FINGERPRINT_V1.to_vec(),
                MANAGED_PHYSICAL_POLICY_FINGERPRINT_V1.to_vec(),
                MANAGED_ASSERTION_POLICY_FINGERPRINT_V1.to_vec(),
                MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1.to_vec(),
            ],
            vec![
                b"1".to_vec(),
                b"count".to_vec(),
                b"plan".to_vec(),
                PLAN_NODE_TAG_RELATION.as_bytes().to_vec(),
                PLAN_NODE_TAG_COUNT.to_be_bytes().to_vec(),
                b"verify".to_vec(),
                VERIFY_NODE_TAG_RELATION.as_bytes().to_vec(),
                VERIFY_NODE_TAG_COUNT.to_be_bytes().to_vec(),
            ],
            vec![
                b"2".to_vec(),
                b"count".to_vec(),
                b"plan".to_vec(),
                PLAN_PERMANENT_GUARD_RELATION.as_bytes().to_vec(),
                PLAN_PERMANENT_GUARD_COUNT.to_be_bytes().to_vec(),
                b"verify".to_vec(),
                VERIFY_PERMANENT_GUARD_RELATION.as_bytes().to_vec(),
                VERIFY_PERMANENT_GUARD_COUNT.to_be_bytes().to_vec(),
            ],
            vec![
                b"3".to_vec(),
                b"exact-pair".to_vec(),
                b"plan".to_vec(),
                PLAN_PERMANENT_GUARD_RELATION.as_bytes().to_vec(),
                b"verify".to_vec(),
                VERIFY_PERMANENT_GUARD_RELATION.as_bytes().to_vec(),
                crate::vector_projection::GUARD_KEY.as_bytes().to_vec(),
                PERMANENT_VECTOR_GUARD_VALUE.as_bytes().to_vec(),
            ],
            vec![
                b"4".to_vec(),
                b"ordered-one-of".to_vec(),
                b"plan".to_vec(),
                PLAN_META_RELATION.as_bytes().to_vec(),
                b"verify".to_vec(),
                VERIFY_META_RELATION.as_bytes().to_vec(),
                VECTOR_GENERATION_OUTCOME_TAG.as_bytes().to_vec(),
                crate::vector_projection::META_KEY.as_bytes().to_vec(),
                LEGACY_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
                STRUCT_MAP_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
            ],
            vec![
                b"5".to_vec(),
                b"exact-pair".to_vec(),
                b"plan".to_vec(),
                PLAN_META_RELATION.as_bytes().to_vec(),
                b"verify".to_vec(),
                VERIFY_META_RELATION.as_bytes().to_vec(),
                crate::canonical_node_contract::META_KEY.as_bytes().to_vec(),
                crate::canonical_node_contract::META_VALUE
                    .as_bytes()
                    .to_vec(),
            ],
            vec![
                b"6".to_vec(),
                b"exact-pair".to_vec(),
                b"plan".to_vec(),
                PLAN_META_RELATION.as_bytes().to_vec(),
                b"verify".to_vec(),
                VERIFY_META_RELATION.as_bytes().to_vec(),
                crate::tag_projection::META_KEY.as_bytes().to_vec(),
                crate::tag_projection::META_VALUE.as_bytes().to_vec(),
            ],
            vec![
                b"selection".to_vec(),
                b"0=LegacyNeedsMigration".to_vec(),
                b"1=CurrentStructMap-only-with-all-struct-map-v1".to_vec(),
            ],
            vec![b"old+positional-v0=legacy".to_vec()],
            vec![b"old+positional-v1=legacy".to_vec()],
            vec![b"old+struct-map-v1=legacy".to_vec()],
            vec![b"old+mixed-exact=legacy".to_vec()],
            vec![b"new+struct-map-v1=current".to_vec()],
            vec![b"new+positional-or-mixed=corrupt".to_vec()],
            vec![b"missing-or-unknown-or-managed=refuse".to_vec()],
            vec![
                b"recognition-only-marker".to_vec(),
                STRUCT_MAP_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
                MANAGED_WRITER_GENERATION_MARKER.as_bytes().to_vec(),
                b"not-writer-or-open-admission-before-production-integration".to_vec(),
            ],
        ];
        let derived = independently_frame_entries(
            SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_DOMAIN_V1,
            &entries,
            17,
        );
        assert_eq!(
            derived, SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1,
            "repin only after reviewing the independently framed conventional policy"
        );
        assert_ne!(derived, [0; 32]);
    }

    #[test]
    fn open_admission_classifier_policy_has_an_independent_kat() {
        let entries = vec![
            vec![
                b"authority".to_vec(),
                b"bounded-routine-pre-dbinstance-generation-fence".to_vec(),
                b"closed-catalog-fence-only".to_vec(),
            ],
            vec![
                b"construction".to_vec(),
                b"consume ManagedSqliteClosedCatalogFenceV1".to_vec(),
                b"source=initial-ready ExistingSqliteSnapshotSource".to_vec(),
            ],
            vec![
                b"vendor-policies-in-order".to_vec(),
                b"snapshot=ManagedSnapshotPolicy::V1".to_vec(),
                b"selector=ManagedCatalogFencePolicy::V1".to_vec(),
                MANAGED_CATALOG_POLICY_FINGERPRINT_V1.to_vec(),
                MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_V1.to_vec(),
                MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1.to_vec(),
                MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1.to_vec(),
            ],
            vec![
                b"closed-evidence".to_vec(),
                b"exact primary-index-visible catalog".to_vec(),
                b"light ordered assertion transcript".to_vec(),
                b"strict-close and source evidence".to_vec(),
            ],
            vec![
                b"excluded-authority".to_vec(),
                b"no physical census".to_vec(),
                b"no integrity or completeness claim".to_vec(),
                b"no cleanup or mutation authority".to_vec(),
            ],
            vec![
                b"fixed-counts".to_vec(),
                b"assertions=6".to_vec(),
                b"ranges=2".to_vec(),
                b"points=4".to_vec(),
                b"outcomes=6".to_vec(),
            ],
            vec![
                b"light-transcript-domain".to_vec(),
                MANAGED_CATALOG_FENCE_ASSERTION_TRANSCRIPT_DOMAIN_V1.to_vec(),
            ],
            vec![
                b"assertion-1-count".to_vec(),
                b"plan".to_vec(),
                PLAN_NODE_TAG_RELATION.as_bytes().to_vec(),
                PLAN_NODE_TAG_COUNT.to_be_bytes().to_vec(),
                b"verify".to_vec(),
                VERIFY_NODE_TAG_RELATION.as_bytes().to_vec(),
                VERIFY_NODE_TAG_COUNT.to_be_bytes().to_vec(),
            ],
            vec![
                b"assertion-2-count".to_vec(),
                b"plan".to_vec(),
                PLAN_PERMANENT_GUARD_RELATION.as_bytes().to_vec(),
                PLAN_PERMANENT_GUARD_COUNT.to_be_bytes().to_vec(),
                b"verify".to_vec(),
                VERIFY_PERMANENT_GUARD_RELATION.as_bytes().to_vec(),
                VERIFY_PERMANENT_GUARD_COUNT.to_be_bytes().to_vec(),
            ],
            vec![
                b"assertion-3-exact-pair".to_vec(),
                PLAN_PERMANENT_GUARD_RELATION.as_bytes().to_vec(),
                VERIFY_PERMANENT_GUARD_RELATION.as_bytes().to_vec(),
                crate::vector_projection::GUARD_KEY.as_bytes().to_vec(),
                PERMANENT_VECTOR_GUARD_VALUE.as_bytes().to_vec(),
            ],
            vec![
                b"assertion-4-ordered-one-of".to_vec(),
                PLAN_META_RELATION.as_bytes().to_vec(),
                VERIFY_META_RELATION.as_bytes().to_vec(),
                VECTOR_GENERATION_OUTCOME_TAG.as_bytes().to_vec(),
                crate::vector_projection::META_KEY.as_bytes().to_vec(),
                LEGACY_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
                STRUCT_MAP_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
            ],
            vec![
                b"assertion-5-exact-pair".to_vec(),
                PLAN_META_RELATION.as_bytes().to_vec(),
                VERIFY_META_RELATION.as_bytes().to_vec(),
                crate::canonical_node_contract::META_KEY.as_bytes().to_vec(),
                crate::canonical_node_contract::META_VALUE
                    .as_bytes()
                    .to_vec(),
            ],
            vec![
                b"assertion-6-exact-pair".to_vec(),
                PLAN_META_RELATION.as_bytes().to_vec(),
                VERIFY_META_RELATION.as_bytes().to_vec(),
                crate::tag_projection::META_KEY.as_bytes().to_vec(),
                crate::tag_projection::META_VALUE.as_bytes().to_vec(),
            ],
            vec![
                b"old+positional-v0".to_vec(),
                LEGACY_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
                b"CatalogCodecUpgradeRequired owns closed fence".to_vec(),
            ],
            vec![
                b"old+positional-v1".to_vec(),
                LEGACY_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
                b"CatalogCodecUpgradeRequired owns closed fence".to_vec(),
            ],
            vec![
                b"old+struct-map-v1".to_vec(),
                LEGACY_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
                b"CatalogCodecUpgradeRequired owns closed fence".to_vec(),
            ],
            vec![
                b"old+mixed-exact".to_vec(),
                LEGACY_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
                b"CatalogCodecUpgradeRequired owns closed fence".to_vec(),
            ],
            vec![
                b"new+struct-map-v1".to_vec(),
                STRUCT_MAP_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
                b"Current permit owns closed fence and constructor consumes permit".to_vec(),
            ],
            vec![
                b"new+positional-or-mixed".to_vec(),
                STRUCT_MAP_CATALOG_GENERATION_MARKER.as_bytes().to_vec(),
                b"AdmissionNotEstablished terminal no fallthrough".to_vec(),
            ],
            vec![
                b"missing-unknown-or-managed-generation".to_vec(),
                MANAGED_WRITER_GENERATION_MARKER.as_bytes().to_vec(),
                b"AdmissionNotEstablished terminal no generation inference".to_vec(),
            ],
            vec![
                b"current-authority-shape".to_vec(),
                b"non-Clone non-Copy owning CurrentOpenPermitV1".to_vec(),
                b"existing constructor must consume by value".to_vec(),
                b"borrowed diagnostics are non-authoritative".to_vec(),
            ],
            vec![
                b"upgrade-authority-shape".to_vec(),
                b"non-Clone non-Copy owning CatalogUpgradeEvidenceV1".to_vec(),
                b"cannot enter existing constructor".to_vec(),
                b"no detachable disposition bit".to_vec(),
            ],
            vec![
                b"terminal-error".to_vec(),
                b"consumed source or fence returned no admitted evidence".to_vec(),
                b"end classification attempt".to_vec(),
                b"never classifier fallthrough".to_vec(),
                b"erase attacker diagnostics".to_vec(),
            ],
        ];
        assert_eq!(entries.len(), 23);
        let derived = independently_frame_entries(
            OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_DOMAIN_V1,
            &entries,
            23,
        );
        assert_eq!(
            derived, OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
            "repin only after reviewing the independent open-admission policy"
        );
        assert_ne!(derived, [0; 32]);
    }

    #[derive(Debug, Eq, PartialEq)]
    struct SqliteArtifactSnapshot {
        bytes: Vec<u8>,
        len: u64,
        modified: Option<std::time::SystemTime>,
        readonly: bool,
        is_file: bool,
        is_directory: bool,
        is_symlink: bool,
        #[cfg(unix)]
        mode: u32,
        #[cfg(unix)]
        uid: u32,
        #[cfg(unix)]
        gid: u32,
        #[cfg(unix)]
        nlink: u64,
        #[cfg(unix)]
        device: u64,
        #[cfg(unix)]
        inode: u64,
        #[cfg(unix)]
        ctime_seconds: i64,
        #[cfg(unix)]
        ctime_nanoseconds: i64,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct FixtureDirectorySnapshot {
        len: u64,
        modified: Option<std::time::SystemTime>,
        readonly: bool,
        is_directory: bool,
        is_symlink: bool,
        #[cfg(unix)]
        mode: u32,
        #[cfg(unix)]
        uid: u32,
        #[cfg(unix)]
        gid: u32,
        #[cfg(unix)]
        nlink: u64,
        #[cfg(unix)]
        device: u64,
        #[cfg(unix)]
        inode: u64,
        #[cfg(unix)]
        mtime_seconds: i64,
        #[cfg(unix)]
        mtime_nanoseconds: i64,
        #[cfg(unix)]
        ctime_seconds: i64,
        #[cfg(unix)]
        ctime_nanoseconds: i64,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct SqliteFamilySnapshot {
        artifacts: [Option<SqliteArtifactSnapshot>; 4],
        /// Every entry in the isolated fixture directory. This catches a
        /// leaked sidecar or temporary artifact without concurrent-test noise.
        directory_entries: BTreeSet<String>,
        /// The fixture has its own directory, so identity plus mtime/ctime can
        /// detect create-then-unlink and directory replacement without noise
        /// from concurrent tests.
        parent_directory: FixtureDirectorySnapshot,
    }

    fn sqlite_file_images(path: &Path) -> SqliteFamilySnapshot {
        let base = path.as_os_str().to_string_lossy();
        let artifacts = ["", "-wal", "-shm", "-journal"].map(|suffix| {
            let candidate = PathBuf::from(format!("{base}{suffix}"));
            match std::fs::read(candidate) {
                Ok(bytes) => {
                    let metadata = std::fs::symlink_metadata(format!("{base}{suffix}"))
                        .expect("stat SQLite test artifact");
                    let file_type = metadata.file_type();
                    #[cfg(unix)]
                    use std::os::unix::fs::MetadataExt;
                    Some(SqliteArtifactSnapshot {
                        bytes,
                        len: metadata.len(),
                        modified: metadata.modified().ok(),
                        readonly: metadata.permissions().readonly(),
                        is_file: file_type.is_file(),
                        is_directory: file_type.is_dir(),
                        is_symlink: file_type.is_symlink(),
                        #[cfg(unix)]
                        mode: metadata.mode(),
                        #[cfg(unix)]
                        uid: metadata.uid(),
                        #[cfg(unix)]
                        gid: metadata.gid(),
                        #[cfg(unix)]
                        nlink: metadata.nlink(),
                        #[cfg(unix)]
                        device: metadata.dev(),
                        #[cfg(unix)]
                        inode: metadata.ino(),
                        #[cfg(unix)]
                        ctime_seconds: metadata.ctime(),
                        #[cfg(unix)]
                        ctime_nanoseconds: metadata.ctime_nsec(),
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("read SQLite test image: {error}"),
            }
        });
        let parent = path.parent().expect("SQLite fixture has a parent");
        let directory_entries = std::fs::read_dir(parent)
            .expect("read SQLite fixture parent")
            .map(|entry| entry.expect("read SQLite fixture directory entry"))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        let parent_metadata =
            std::fs::symlink_metadata(parent).expect("stat isolated SQLite fixture directory");
        let parent_type = parent_metadata.file_type();
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        let parent_directory = FixtureDirectorySnapshot {
            len: parent_metadata.len(),
            modified: parent_metadata.modified().ok(),
            readonly: parent_metadata.permissions().readonly(),
            is_directory: parent_type.is_dir(),
            is_symlink: parent_type.is_symlink(),
            #[cfg(unix)]
            mode: parent_metadata.mode(),
            #[cfg(unix)]
            uid: parent_metadata.uid(),
            #[cfg(unix)]
            gid: parent_metadata.gid(),
            #[cfg(unix)]
            nlink: parent_metadata.nlink(),
            #[cfg(unix)]
            device: parent_metadata.dev(),
            #[cfg(unix)]
            inode: parent_metadata.ino(),
            #[cfg(unix)]
            mtime_seconds: parent_metadata.mtime(),
            #[cfg(unix)]
            mtime_nanoseconds: parent_metadata.mtime_nsec(),
            #[cfg(unix)]
            ctime_seconds: parent_metadata.ctime(),
            #[cfg(unix)]
            ctime_nanoseconds: parent_metadata.ctime_nsec(),
        };
        SqliteFamilySnapshot {
            artifacts,
            directory_entries,
            parent_directory,
        }
    }

    #[cfg(unix)]
    #[test]
    fn open_admission_preservation_snapshot_detects_create_unlink_and_chmod_restore() {
        use std::os::unix::fs::PermissionsExt;

        let directory_mutation = PersistentCatalogFixture::fresh();
        let before = sqlite_file_images(&directory_mutation.path);
        let transient = directory_mutation.root.join("create-then-unlink");
        std::fs::write(&transient, b"transient").expect("create transient fixture entry");
        std::fs::remove_file(transient).expect("unlink transient fixture entry");
        assert_ne!(
            sqlite_file_images(&directory_mutation.path),
            before,
            "isolated parent mtime/ctime must expose create-then-unlink"
        );

        let mode_mutation = PersistentCatalogFixture::fresh();
        let before = sqlite_file_images(&mode_mutation.path);
        let original = std::fs::symlink_metadata(&mode_mutation.path)
            .expect("stat mode-mutation fixture")
            .permissions();
        let original_mode = original.mode();
        let mut changed = original.clone();
        changed.set_mode(original_mode ^ 0o200);
        std::fs::set_permissions(&mode_mutation.path, changed).expect("change fixture mode");
        std::fs::set_permissions(&mode_mutation.path, original).expect("restore fixture mode");
        assert_ne!(
            sqlite_file_images(&mode_mutation.path),
            before,
            "file ctime must expose chmod-restore"
        );
    }

    fn recognize_routine_fixture(
        fixture: &PersistentCatalogFixture,
    ) -> Result<OpenAdmissionV1, AdmissionNotEstablished> {
        let source = ExistingSqliteSnapshotSource::open(&fixture.path)
            .expect("open clean existing conventional unmanaged source");
        recognize_open_source_v1(source)
    }

    fn assert_routine_admission_not_established(
        result: Result<OpenAdmissionV1, AdmissionNotEstablished>,
    ) {
        match result {
            Err(error) => assert_eq!(error, AdmissionNotEstablished),
            Ok(_) => {
                panic!("refused conventional unmanaged source produced owning admission evidence")
            }
        }
    }

    #[test]
    fn open_admission_classifies_old_and_current_without_changing_bytes() {
        let old_source = PersistentCatalogFixture::fresh();
        for (fixture_kind, expected_codec) in [
            (
                ManagedCatalogFixtureV1::AllPositionalV0,
                CatalogCodecClassification::AllPositionalV0,
            ),
            (
                ManagedCatalogFixtureV1::AllPositionalV1,
                CatalogCodecClassification::AllPositionalV1,
            ),
            (
                ManagedCatalogFixtureV1::AllStructMapV1,
                CatalogCodecClassification::AllStructMapV1,
            ),
            (
                ManagedCatalogFixtureV1::DeterministicMixed,
                CatalogCodecClassification::MixedExact,
            ),
        ] {
            let old = old_source.catalog_fixture(fixture_kind);
            let before = sqlite_file_images(&old.path);
            let admission = recognize_routine_fixture(&old)
                .expect("every exact legacy-marker contract codec needs the offline codec upgrade");
            let OpenAdmissionV1::CatalogCodecUpgradeRequired(evidence) = admission else {
                panic!("old marker yielded a current owning permit");
            };
            assert_eq!(evidence.codec_classification(), expected_codec);
            assert_eq!(evidence.vector_dimension(), 4);
            assert_eq!(
                evidence.classifier_policy_fingerprint(),
                &OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1
            );
            assert_eq!(
                evidence.diagnostic_fence().assertions().assertion_count(),
                6
            );
            assert_eq!(
                evidence.diagnostic_fence().assertions().range_probe_count(),
                2
            );
            assert_eq!(
                evidence.diagnostic_fence().assertions().point_read_count(),
                4
            );
            drop(evidence);
            assert_eq!(sqlite_file_images(&old.path), before);
        }

        let current = PersistentCatalogFixture::fresh();
        current.put_meta(
            crate::vector_projection::META_KEY,
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
        );
        let before = sqlite_file_images(&current.path);
        let admission = recognize_routine_fixture(&current)
            .expect("new marker plus all struct-map catalog must be routine-current conventional generation");
        let OpenAdmissionV1::Current(permit) = admission else {
            panic!("new marker plus all-map catalog yielded upgrade evidence");
        };
        assert_eq!(
            permit.classifier_policy_fingerprint(),
            &OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1
        );
        assert_eq!(permit.vector_dimension(), 4);
        drop(permit);
        assert_eq!(sqlite_file_images(&current.path), before);
    }

    #[test]
    fn open_admission_refuses_torn_or_unknown_generation_without_changing_bytes() {
        let new_source = PersistentCatalogFixture::fresh();
        new_source.put_meta(
            crate::vector_projection::META_KEY,
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
        );
        for fixture_kind in [
            ManagedCatalogFixtureV1::AllPositionalV0,
            ManagedCatalogFixtureV1::AllPositionalV1,
            ManagedCatalogFixtureV1::DeterministicMixed,
        ] {
            let torn = new_source.catalog_fixture(fixture_kind);
            let before = sqlite_file_images(&torn.path);
            assert_routine_admission_not_established(recognize_routine_fixture(&torn));
            assert_eq!(sqlite_file_images(&torn.path), before);
        }

        for marker in [
            "unknown-conventional-generation",
            MANAGED_WRITER_GENERATION_MARKER,
        ] {
            let unknown = PersistentCatalogFixture::fresh();
            unknown.put_meta(crate::vector_projection::META_KEY, marker);
            let before = sqlite_file_images(&unknown.path);
            assert_routine_admission_not_established(recognize_routine_fixture(&unknown));
            assert_eq!(sqlite_file_images(&unknown.path), before);
        }

        let missing = PersistentCatalogFixture::fresh();
        missing.remove_meta(crate::vector_projection::META_KEY);
        let before = sqlite_file_images(&missing.path);
        assert_routine_admission_not_established(recognize_routine_fixture(&missing));
        assert_eq!(sqlite_file_images(&missing.path), before);
    }

    #[derive(Clone, Copy, Debug)]
    enum ValidButWrongRoutinePlan {
        FiveAssertions,
        SevenAssertions,
        ReorderMarkerAndCanonical,
        DifferentEmptyRelation,
        ExactPairInsteadOfOneOf,
        WrongOutcomeTag,
        ReversedAlternatives,
    }

    fn close_with_valid_but_wrong_routine_plan(
        fixture: &PersistentCatalogFixture,
        plan: ValidButWrongRoutinePlan,
    ) -> ManagedSqliteClosedCatalogFenceV1 {
        ExistingSqliteSnapshotSource::open(&fixture.path)
            .expect("open source for valid-but-wrong routine plan")
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
                let candidate = validate_primary_index_visible_catalog(planner.catalog())
                    .map_err(|error| cozo::Error::msg(error.to_string()))?;
                let node_tag = candidate
                    .relation_id("node_tag")
                    .ok_or_else(|| cozo::Error::msg("test plan lost node_tag"))?;
                let guard = candidate
                    .relation_id("mneme_reembed_shadow_node_vec")
                    .ok_or_else(|| cozo::Error::msg("test plan lost permanent guard"))?;
                let meta = candidate
                    .relation_id("meta")
                    .ok_or_else(|| cozo::Error::msg("test plan lost meta"))?;
                let alternate_empty = candidate
                    .relation_id("feedback_retry")
                    .ok_or_else(|| cozo::Error::msg("test plan lost alternate empty relation"))?;
                let extra_empty = candidate
                    .relation_id("edge")
                    .ok_or_else(|| cozo::Error::msg("test plan lost extra empty relation"))?;
                let order: &[u8] = match plan {
                    ValidButWrongRoutinePlan::FiveAssertions => &[0, 1, 2, 3, 4],
                    ValidButWrongRoutinePlan::SevenAssertions => &[0, 1, 2, 3, 4, 5, 6],
                    ValidButWrongRoutinePlan::ReorderMarkerAndCanonical => &[0, 1, 2, 4, 3, 5],
                    _ => &[0, 1, 2, 3, 4, 5],
                };
                for ordinal in order {
                    match ordinal {
                        0 => planner.require_exact_row_count(
                            if matches!(plan, ValidButWrongRoutinePlan::DifferentEmptyRelation) {
                                alternate_empty
                            } else {
                                node_tag
                            },
                            0,
                        )?,
                        1 => planner.require_exact_row_count(guard, 1)?,
                        2 => planner.require_string_pair(
                            guard,
                            crate::vector_projection::GUARD_KEY,
                            PERMANENT_VECTOR_GUARD_VALUE,
                        )?,
                        3 if matches!(plan, ValidButWrongRoutinePlan::ExactPairInsteadOfOneOf) => {
                            planner.require_string_pair(
                                meta,
                                crate::vector_projection::META_KEY,
                                LEGACY_CATALOG_GENERATION_MARKER,
                            )?
                        }
                        3 => planner.require_string_pair_one_of(
                            if matches!(plan, ValidButWrongRoutinePlan::WrongOutcomeTag) {
                                "wrong-conventional-generation"
                            } else {
                                VECTOR_GENERATION_OUTCOME_TAG
                            },
                            meta,
                            crate::vector_projection::META_KEY,
                            if matches!(plan, ValidButWrongRoutinePlan::ReversedAlternatives) {
                                [
                                    STRUCT_MAP_CATALOG_GENERATION_MARKER,
                                    LEGACY_CATALOG_GENERATION_MARKER,
                                ]
                            } else {
                                [
                                    LEGACY_CATALOG_GENERATION_MARKER,
                                    STRUCT_MAP_CATALOG_GENERATION_MARKER,
                                ]
                            },
                        )?,
                        4 => planner.require_string_pair(
                            meta,
                            crate::canonical_node_contract::META_KEY,
                            crate::canonical_node_contract::META_VALUE,
                        )?,
                        5 => planner.require_string_pair(
                            meta,
                            crate::tag_projection::META_KEY,
                            crate::tag_projection::META_VALUE,
                        )?,
                        6 => planner.require_exact_row_count(extra_empty, 0)?,
                        _ => {
                            return Err(cozo::Error::msg("invalid routine test assertion ordinal"));
                        }
                    }
                }
                Ok(())
            })
            .unwrap_or_else(|error| {
                panic!("valid-but-wrong routine plan {plan:?} failed generically: {error}")
            })
    }

    #[test]
    fn valid_but_wrong_light_fences_are_rejected_without_changing_the_source() {
        for plan in [
            ValidButWrongRoutinePlan::FiveAssertions,
            ValidButWrongRoutinePlan::SevenAssertions,
            ValidButWrongRoutinePlan::ReorderMarkerAndCanonical,
            ValidButWrongRoutinePlan::DifferentEmptyRelation,
            ValidButWrongRoutinePlan::ExactPairInsteadOfOneOf,
            ValidButWrongRoutinePlan::WrongOutcomeTag,
            ValidButWrongRoutinePlan::ReversedAlternatives,
        ] {
            let fixture = PersistentCatalogFixture::fresh();
            let before = sqlite_file_images(&fixture.path);
            let fence = close_with_valid_but_wrong_routine_plan(&fixture, plan);
            assert_routine_admission_not_established(classify_closed_open_fence_v1(fence));
            assert_eq!(
                sqlite_file_images(&fixture.path),
                before,
                "valid-but-wrong routine plan {plan:?} changed source bytes, metadata, or family entries"
            );
        }
    }

    fn independent_catalog_encoding_census(
        observation: &ManagedSqlitePrimaryIndexCatalogV1,
    ) -> [usize; 3] {
        let mut census = [0_usize; 3];
        for entry in observation.catalog().entries() {
            let slot = match entry.encoding() {
                ManagedCatalogEncodingV1::PositionalV0 => 0,
                ManagedCatalogEncodingV1::PositionalV1 => 1,
                ManagedCatalogEncodingV1::StructMapV1 => 2,
            };
            census[slot] += 1;
        }
        census
    }

    fn assert_live_seal_transcript_matches_vendor(seal: &SourceBoundSealV1, selected_index: u8) {
        let facts = rederive_closed_verifier_catalog_facts_for(
            seal.audit().catalog(),
            CatalogContract::Predecessor,
        )
        .expect("closed audit must still match the independent raw semantic verifier");
        assert_eq!(
            seal.audit().assertions().transcript_commitment(),
            &generic_assertion_transcript_commitment_v1(facts.ids, selected_index)
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum ValidButWrongPlan {
        FiveAssertions,
        SevenAssertions,
        ReorderMarkerAndCanonical,
        ReorderCanonicalAndTag,
        DifferentEmptyRelation,
        WrongTrueIdKeyValue,
        ExactPairInsteadOfOneOf,
        WrongOutcomeTag,
        ReversedAlternatives,
        AlternateVectorValue(&'static str),
    }

    fn close_with_valid_but_wrong_plan(
        fixture: &PersistentCatalogFixture,
        plan: ValidButWrongPlan,
    ) -> ManagedSqliteClosedAuditV1 {
        fixture
            .reader()
            .close_with_relation_assertions_v1(|planner| {
                let candidate = validate_primary_index_visible_catalog(planner.catalog())
                    .map_err(|error| cozo::Error::msg(error.to_string()))?;
                let node_tag = candidate
                    .relation_id("node_tag")
                    .ok_or_else(|| cozo::Error::msg("test plan lost node_tag"))?;
                let guard = candidate
                    .relation_id("mneme_reembed_shadow_node_vec")
                    .ok_or_else(|| cozo::Error::msg("test plan lost permanent guard"))?;
                let meta = candidate
                    .relation_id("meta")
                    .ok_or_else(|| cozo::Error::msg("test plan lost meta"))?;
                let alternate_empty = candidate
                    .relation_id("feedback_retry")
                    .ok_or_else(|| cozo::Error::msg("test plan lost alternate empty relation"))?;
                let extra = candidate
                    .relation_id("edge")
                    .ok_or_else(|| cozo::Error::msg("test plan lost extra relation"))?;
                let extra_count = planner
                    .physical()
                    .relation_row_counts()
                    .iter()
                    .find(|row| row.relation_id() == extra)
                    .ok_or_else(|| cozo::Error::msg("test plan lost extra relation count"))?
                    .row_count();

                let order: &[u8] = match plan {
                    ValidButWrongPlan::FiveAssertions => &[0, 1, 2, 3, 4],
                    ValidButWrongPlan::SevenAssertions => &[0, 1, 2, 3, 4, 5, 6],
                    ValidButWrongPlan::ReorderMarkerAndCanonical => &[0, 1, 2, 4, 3, 5],
                    ValidButWrongPlan::ReorderCanonicalAndTag => &[0, 1, 2, 3, 5, 4],
                    _ => &[0, 1, 2, 3, 4, 5],
                };
                for ordinal in order {
                    match ordinal {
                        0 => planner.require_exact_row_count(
                            if matches!(plan, ValidButWrongPlan::DifferentEmptyRelation) {
                                alternate_empty
                            } else {
                                node_tag
                            },
                            0,
                        )?,
                        1 => planner.require_exact_row_count(guard, 1)?,
                        2 if matches!(plan, ValidButWrongPlan::WrongTrueIdKeyValue) => {
                            planner.require_string_pair(meta, "max_incident_edges_v1", "1024")?;
                        }
                        2 => planner.require_string_pair(
                            guard,
                            crate::vector_projection::GUARD_KEY,
                            PERMANENT_VECTOR_GUARD_VALUE,
                        )?,
                        3 if matches!(plan, ValidButWrongPlan::ExactPairInsteadOfOneOf) => {
                            planner.require_string_pair(
                                meta,
                                crate::vector_projection::META_KEY,
                                LEGACY_CATALOG_GENERATION_MARKER,
                            )?;
                        }
                        3 => {
                            let outcome_tag = if matches!(plan, ValidButWrongPlan::WrongOutcomeTag)
                            {
                                "wrong-vector-generation"
                            } else {
                                VECTOR_GENERATION_OUTCOME_TAG
                            };
                            let alternatives = match plan {
                                ValidButWrongPlan::ReversedAlternatives => [
                                    STRUCT_MAP_CATALOG_GENERATION_MARKER,
                                    LEGACY_CATALOG_GENERATION_MARKER,
                                ],
                                ValidButWrongPlan::AlternateVectorValue(value) => {
                                    [LEGACY_CATALOG_GENERATION_MARKER, value]
                                }
                                _ => [
                                    LEGACY_CATALOG_GENERATION_MARKER,
                                    STRUCT_MAP_CATALOG_GENERATION_MARKER,
                                ],
                            };
                            planner.require_string_pair_one_of(
                                outcome_tag,
                                meta,
                                crate::vector_projection::META_KEY,
                                alternatives,
                            )?;
                        }
                        4 => planner.require_string_pair(
                            meta,
                            crate::canonical_node_contract::META_KEY,
                            crate::canonical_node_contract::META_VALUE,
                        )?,
                        5 => planner.require_string_pair(
                            meta,
                            crate::tag_projection::META_KEY,
                            crate::tag_projection::META_VALUE,
                        )?,
                        6 => planner.require_exact_row_count(extra, extra_count)?,
                        _ => return Err(cozo::Error::msg("invalid test assertion ordinal")),
                    }
                }
                Ok(())
            })
            .unwrap_or_else(|error| panic!("valid-but-wrong generic plan {plan:?} failed: {error}"))
    }

    #[test]
    fn valid_but_wrong_generic_audits_are_rejected_by_the_private_classifier() {
        let old = PersistentCatalogFixture::fresh();
        for plan in [
            ValidButWrongPlan::FiveAssertions,
            ValidButWrongPlan::SevenAssertions,
            ValidButWrongPlan::ReorderMarkerAndCanonical,
            ValidButWrongPlan::ReorderCanonicalAndTag,
            ValidButWrongPlan::DifferentEmptyRelation,
            ValidButWrongPlan::WrongTrueIdKeyValue,
            ValidButWrongPlan::ExactPairInsteadOfOneOf,
            ValidButWrongPlan::WrongOutcomeTag,
            ValidButWrongPlan::ReversedAlternatives,
        ] {
            let audit = close_with_valid_but_wrong_plan(&old, plan);
            assert!(
                classify_closed_audit_v1(audit).is_err(),
                "private classifier accepted valid-but-wrong plan {plan:?}"
            );
        }

        for marker in [
            "valid-but-unknown-vector-generation",
            MANAGED_WRITER_GENERATION_MARKER,
        ] {
            let fixture = PersistentCatalogFixture::fresh();
            fixture.put_meta(crate::vector_projection::META_KEY, marker);
            let audit = close_with_valid_but_wrong_plan(
                &fixture,
                ValidButWrongPlan::AlternateVectorValue(marker),
            );
            assert!(
                classify_closed_audit_v1(audit).is_err(),
                "private classifier accepted a valid unknown managed-generation plan"
            );
        }
    }

    #[test]
    fn old_and_reserved_new_all_map_seals_match_both_vendor_transcripts() {
        let old = PersistentCatalogFixture::fresh();
        let old_before = sqlite_file_images(&old.path);
        let old_seal = close_source_bound_seal_v1(old.reader())
            .expect("fresh legacy-marker contract must produce a closed seal");
        assert_eq!(
            old_seal.generation(),
            CatalogSealGenerationV1::LegacyNeedsMigration
        );
        assert_eq!(
            old_seal.codec_classification(),
            CatalogCodecClassification::AllStructMapV1
        );
        assert_eq!(old_seal.vector_dimension(), 4);
        assert_eq!(
            old_seal.policy_fingerprint(),
            &SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1
        );
        assert_live_seal_transcript_matches_vendor(&old_seal, 0);
        assert_eq!(sqlite_file_images(&old.path), old_before);

        let current = PersistentCatalogFixture::fresh();
        current.put_meta(
            crate::vector_projection::META_KEY,
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
        );
        let current_before = sqlite_file_images(&current.path);
        let current_seal = close_source_bound_seal_v1(current.reader())
            .expect("reserved struct-map contract must produce a closed seal");
        assert_eq!(
            current_seal.generation(),
            CatalogSealGenerationV1::CurrentStructMap
        );
        assert_eq!(
            current_seal.codec_classification(),
            CatalogCodecClassification::AllStructMapV1
        );
        assert_live_seal_transcript_matches_vendor(&current_seal, 1);
        assert_eq!(sqlite_file_images(&current.path), current_before);
    }

    #[test]
    fn old_and_new_marker_cross_product_obeys_the_exact_codec_matrix() {
        let old_source = PersistentCatalogFixture::fresh();
        let new_source = PersistentCatalogFixture::fresh();
        new_source.put_meta(
            crate::vector_projection::META_KEY,
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
        );

        for (fixture_kind, expected_codec, expected_census) in [
            (
                ManagedCatalogFixtureV1::AllPositionalV0,
                CatalogCodecClassification::AllPositionalV0,
                [26, 0, 0],
            ),
            (
                ManagedCatalogFixtureV1::AllPositionalV1,
                CatalogCodecClassification::AllPositionalV1,
                [0, 26, 0],
            ),
            (
                ManagedCatalogFixtureV1::AllStructMapV1,
                CatalogCodecClassification::AllStructMapV1,
                [0, 0, 26],
            ),
            (
                ManagedCatalogFixtureV1::DeterministicMixed,
                CatalogCodecClassification::MixedExact,
                [9, 9, 8],
            ),
        ] {
            let old = old_source.catalog_fixture(fixture_kind);
            let mut old_reader = old.reader();
            let old_observation = old_reader
                .inspect_primary_index_catalog_v1()
                .expect("old codec fixture must remain independently visible");
            assert_eq!(
                independent_catalog_encoding_census(old_observation),
                expected_census
            );
            let old_codec = validate_primary_index_visible_catalog(old_observation)
                .expect("old codec fixture must remain raw conventional")
                .codec_classification();
            assert_eq!(old_codec, expected_codec);
            let old_seal = close_source_bound_seal_v1(old_reader)
                .expect("every exact legacy-marker contract codec class must seal as legacy");
            assert_eq!(
                old_seal.generation(),
                CatalogSealGenerationV1::LegacyNeedsMigration
            );
            assert_eq!(old_seal.codec_classification(), expected_codec);

            let new = new_source.catalog_fixture(fixture_kind);
            let mut new_reader = new.reader();
            let new_observation = new_reader
                .inspect_primary_index_catalog_v1()
                .expect("new codec fixture must remain independently visible");
            let new_codec = validate_primary_index_visible_catalog(new_observation)
                .expect("new codec fixture must remain raw conventional")
                .codec_classification();
            assert_eq!(new_codec, expected_codec);
            assert_eq!(
                independent_catalog_encoding_census(new_observation),
                expected_census
            );
            let new_result = close_source_bound_seal_v1(new_reader);
            if expected_codec == CatalogCodecClassification::AllStructMapV1 {
                let new_seal = new_result
                    .expect("new marker plus exact all-map catalog must classify current");
                assert_eq!(
                    new_seal.generation(),
                    CatalogSealGenerationV1::CurrentStructMap
                );
            } else {
                assert!(
                    new_result.is_err(),
                    "new marker plus any legacy catalog byte must reject as torn"
                );
            }
        }
    }

    #[test]
    fn fresh_writes_legacy_marker_and_ordinary_open_refuses_reserved_marker() {
        let fresh = PersistentCatalogFixture::fresh();
        let seal = close_source_bound_seal_v1(fresh.reader())
            .expect("fresh committed writer must still publish recognized legacy generation");
        assert_eq!(
            seal.generation(),
            CatalogSealGenerationV1::LegacyNeedsMigration
        );
        drop(seal);

        let reserved = PersistentCatalogFixture::fresh();
        reserved.put_meta(
            crate::vector_projection::META_KEY,
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
        );
        let before_seal = close_source_bound_seal_v1(reserved.reader())
            .expect("reserved marker must seal before ordinary-open refusal");
        let before_table = *before_seal.audit().physical().table_ordered_commitment();
        let before_index = *before_seal.audit().physical().index_ordered_commitment();
        let before_rows = before_seal.audit().physical().table_row_count();
        let before_catalog_raw = *before_seal.audit().catalog().raw_commitment();
        let before_catalog_canonical = *before_seal.audit().catalog().canonical_commitment();
        drop(before_seal);
        let open = crate::CozoStore::open(reserved.path.to_str().expect("UTF-8 test path"), 4);
        assert!(
            open.is_err(),
            "ordinary open must not admit the recognition-only marker before production integration"
        );
        drop(open);
        let still_current = close_source_bound_seal_v1(reserved.reader())
            .expect("failed ordinary open must preserve the recognized current logical source");
        assert_eq!(
            still_current.generation(),
            CatalogSealGenerationV1::CurrentStructMap
        );
        assert_eq!(
            still_current.audit().physical().table_ordered_commitment(),
            &before_table
        );
        assert_eq!(
            still_current.audit().physical().index_ordered_commitment(),
            &before_index
        );
        assert_eq!(
            still_current.audit().physical().table_row_count(),
            before_rows
        );
        assert_eq!(
            still_current.audit().catalog().raw_commitment(),
            &before_catalog_raw
        );
        assert_eq!(
            still_current.audit().catalog().canonical_commitment(),
            &before_catalog_canonical
        );
    }

    #[test]
    fn semantic_row_obligations_fail_independently() {
        let nonempty_node_tag = PersistentCatalogFixture::fresh();
        nonempty_node_tag.insert_node_tag();
        assert!(!seal_error(&nonempty_node_tag).is_empty());

        let missing_guard = PersistentCatalogFixture::fresh();
        missing_guard.remove_guard(crate::vector_projection::GUARD_KEY);
        assert!(!seal_error(&missing_guard).is_empty());

        let extra_guard = PersistentCatalogFixture::fresh();
        extra_guard.put_guard("extra-fence", PERMANENT_VECTOR_GUARD_VALUE);
        assert!(!seal_error(&extra_guard).is_empty());

        let wrong_guard = PersistentCatalogFixture::fresh();
        wrong_guard.put_guard(crate::vector_projection::GUARD_KEY, "wrong-generation");
        assert!(!seal_error(&wrong_guard).is_empty());
    }

    #[test]
    fn vector_canonical_and_tag_markers_fail_independently() {
        for vector_mutation in [
            None,
            Some("unknown-vector-generation"),
            Some(MANAGED_WRITER_GENERATION_MARKER),
        ] {
            let fixture = PersistentCatalogFixture::fresh();
            match vector_mutation {
                Some(value) => fixture.put_meta(crate::vector_projection::META_KEY, value),
                None => fixture.remove_meta(crate::vector_projection::META_KEY),
            }
            assert!(!seal_error(&fixture).is_empty());
        }

        for (key, wrong) in [
            (
                crate::canonical_node_contract::META_KEY,
                "wrong-canonical-node-generation",
            ),
            (crate::tag_projection::META_KEY, "wrong-tag-generation"),
        ] {
            let missing = PersistentCatalogFixture::fresh();
            missing.remove_meta(key);
            assert!(!seal_error(&missing).is_empty());

            let mismatched = PersistentCatalogFixture::fresh();
            mismatched.put_meta(key, wrong);
            assert!(!seal_error(&mismatched).is_empty());
        }
    }

    #[test]
    fn genuine_fresh_persistent_catalog_is_semantic_and_canonical_only_visible() {
        let fixture = PersistentCatalogFixture::fresh();
        let mut reader = fixture.reader();
        let observation = reader
            .inspect_primary_index_catalog_v1()
            .expect("fresh conventional unmanaged catalog must be syntactically visible");
        let candidate = validate_primary_index_visible_catalog(observation)
            .expect("fresh conventional unmanaged catalog must match the raw semantic oracle");

        assert_eq!(candidate.vector_dimension(), 4);
        assert_eq!(
            candidate.codec_classification(),
            CatalogCodecClassification::AllStructMapV1
        );
        require_canonical_struct_map_source(&candidate)
            .expect("genuine struct-map source must pass the canonical-only gate");

        let relation_ids = all_physical_relations()
            .map(|relation| {
                candidate
                    .relation_id(relation.name)
                    .expect("every trusted conventional name has a validated id")
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(relation_ids.len(), TOP_LEVEL_RELATION_COUNT);
        assert_eq!(candidate.relation_id("not_a_contract_relation"), None);

        // Canonical-only admission is a codec check, not commitment equality:
        // Mnestic intentionally domain-separates these two commitments.
        assert_ne!(
            observation.raw_commitment(),
            observation.canonical_commitment()
        );

        reader.close_and_verify().expect("close managed reader");
    }

    #[test]
    fn canonical_only_gate_refuses_legacy_and_mixed_exact_candidates() {
        let fixture = PersistentCatalogFixture::fresh();
        let mut reader = fixture.reader();
        let observation = reader
            .inspect_primary_index_catalog_v1()
            .expect("fresh conventional unmanaged catalog must be syntactically visible");
        let canonical = validate_primary_index_visible_catalog(observation)
            .expect("fresh conventional unmanaged catalog must match the raw semantic oracle");

        for codec_classification in [
            CatalogCodecClassification::AllPositionalV0,
            CatalogCodecClassification::AllPositionalV1,
            CatalogCodecClassification::MixedExact,
        ] {
            let noncanonical = PrimaryIndexVisibleCatalogCandidate {
                _observation: observation,
                codec_classification,
                vector_dimension: canonical.vector_dimension,
                relation_ids: canonical.relation_ids.clone(),
                catalog: CatalogContract::Predecessor,
            };
            assert_eq!(
                require_canonical_struct_map_source(&noncanonical),
                Err(RawCatalogError::CanonicalStructMapRequired)
            );
        }

        reader.close_and_verify().expect("close managed reader");
    }

    #[test]
    fn codec_census_distinguishes_all_exact_profiles() {
        use ManagedCatalogEncodingV1::{PositionalV0, PositionalV1, StructMapV1};

        assert_eq!(
            classify_codecs([PositionalV0; TOP_LEVEL_RELATION_COUNT]),
            CatalogCodecClassification::AllPositionalV0
        );
        assert_eq!(
            classify_codecs([PositionalV1; TOP_LEVEL_RELATION_COUNT]),
            CatalogCodecClassification::AllPositionalV1
        );
        assert_eq!(
            classify_codecs([StructMapV1; TOP_LEVEL_RELATION_COUNT]),
            CatalogCodecClassification::AllStructMapV1
        );
        assert_eq!(
            classify_codecs([PositionalV0, StructMapV1]),
            CatalogCodecClassification::MixedExact
        );
    }

    #[test]
    fn closed_codec_fixtures_pass_raw_and_canonical_contracts() {
        let source = PersistentCatalogFixture::fresh();
        for (fixture_kind, expected, canonical) in [
            (
                ManagedCatalogFixtureV1::AllPositionalV0,
                CatalogCodecClassification::AllPositionalV0,
                false,
            ),
            (
                ManagedCatalogFixtureV1::AllPositionalV1,
                CatalogCodecClassification::AllPositionalV1,
                false,
            ),
            (
                ManagedCatalogFixtureV1::AllStructMapV1,
                CatalogCodecClassification::AllStructMapV1,
                true,
            ),
            (
                ManagedCatalogFixtureV1::DeterministicMixed,
                CatalogCodecClassification::MixedExact,
                false,
            ),
        ] {
            let fixture_db = source.catalog_fixture(fixture_kind);
            let mut reader = fixture_db.reader();
            {
                let observation = reader
                    .inspect_primary_index_catalog_v1()
                    .expect("closed codec fixture must pass raw catalog admission");
                if fixture_kind == ManagedCatalogFixtureV1::DeterministicMixed {
                    let mut census = [0_usize; 3];
                    for entry in observation.catalog().entries() {
                        let slot = match entry.encoding() {
                            ManagedCatalogEncodingV1::PositionalV0 => 0,
                            ManagedCatalogEncodingV1::PositionalV1 => 1,
                            ManagedCatalogEncodingV1::StructMapV1 => 2,
                        };
                        census[slot] += 1;
                    }
                    assert_eq!(census, [9, 9, 8]);
                }
                let candidate = validate_primary_index_visible_catalog(observation)
                    .expect("closed codec fixture must preserve raw conventional semantics");
                assert_eq!(candidate.codec_classification(), expected);
                assert_eq!(
                    require_canonical_struct_map_source(&candidate).is_ok(),
                    canonical
                );
            }
            {
                let census = reader
                    .inspect_physical_census_v1()
                    .expect("closed codec fixture must pass closed physical admission");
                let candidate = validate_primary_index_visible_catalog(census.catalog()).expect(
                    "closed physical catalog must still preserve raw conventional semantics",
                );
                assert_eq!(candidate.codec_classification(), expected);
            }
            reader.close_and_verify().expect("close codec fixture");
        }
    }

    fn closed_fixture_raw_error(
        fixture: ManagedCatalogFixtureV1,
    ) -> (RawCatalogError, Result<(), String>, Result<(), String>) {
        let source = PersistentCatalogFixture::fresh();
        let fixture = source.catalog_fixture(fixture);
        let mut reader = fixture.reader();
        let error = {
            let observation = reader
                .inspect_primary_index_catalog_v1()
                .expect("closed hostile fixture must remain raw-catalog-decodable");
            validate_primary_index_visible_catalog(observation)
                .map(|candidate| candidate.vector_dimension())
                .expect_err("closed hostile fixture must fail the raw conventional oracle")
        };
        let physical = reader
            .inspect_physical_census_v1()
            .map(|_| ())
            .map_err(|error| error.to_string());
        let close = reader.close_and_verify().map_err(|error| error.to_string());
        assert_eq!(close.is_ok(), physical.is_ok());
        let diagnostic = error.to_string();
        assert!(!diagnostic.contains(ATTACKER_SENTINEL));
        assert!(diagnostic.len() <= 160);
        (error, physical, close)
    }

    #[test]
    fn closed_id_counter_and_nested_fixtures_hit_their_independent_oracles() {
        let (duplicate, duplicate_physical, duplicate_close) =
            closed_fixture_raw_error(ManagedCatalogFixtureV1::DuplicateTopLevelRelationId);
        assert!(matches!(duplicate, RawCatalogError::RelationId { .. }));
        assert_eq!(
            duplicate_physical
                .expect_err("closed physical admission must independently reject duplicate ids"),
            "managed SQLite catalog repeated a top-level relation id"
        );
        assert!(
            duplicate_close
                .expect_err(
                    "a reader poisoned by closed physical admission must not close as success"
                )
                .contains("poisoned")
        );

        let (counter, counter_physical, counter_close) =
            closed_fixture_raw_error(ManagedCatalogFixtureV1::RelationCounterBelowMaxTopLevelId);
        assert_eq!(counter, RawCatalogError::RelationCounter);
        assert!(
            counter_physical.is_ok(),
            "counter ordering belongs to the raw oracle"
        );
        assert!(counter_close.is_ok());

        for fixture in [
            ManagedCatalogFixtureV1::NestedChildRelationIdMismatch,
            ManagedCatalogFixtureV1::NestedChildRelationIdCollisionWithOwner,
        ] {
            let (error, physical, close) = closed_fixture_raw_error(fixture);
            assert!(matches!(error, RawCatalogError::NestedChild { .. }));
            assert!(
                physical.is_ok(),
                "nested ids do not expand the closed physical admitted id set"
            );
            assert!(close.is_ok());
        }
    }

    #[test]
    fn closed_forbidden_feature_fixtures_hit_typed_raw_oracle_classes() {
        for fixture in [
            ManagedCatalogFixtureV1::ForbiddenTemporaryRelation,
            ManagedCatalogFixtureV1::ForbiddenPutTrigger,
            ManagedCatalogFixtureV1::ForbiddenRemoveTrigger,
            ManagedCatalogFixtureV1::ForbiddenReplaceTrigger,
            ManagedCatalogFixtureV1::ForbiddenDescription,
            ManagedCatalogFixtureV1::ForbiddenTemporalFloor,
            ManagedCatalogFixtureV1::ForbiddenLshIndex,
        ] {
            let (error, physical, close) = closed_fixture_raw_error(fixture);
            assert!(matches!(error, RawCatalogError::RelationShape { .. }));
            assert!(
                physical.is_ok(),
                "forbidden descriptor fields remain closed-physical-decodable"
            );
            assert!(close.is_ok());
        }

        let (default, physical, close) =
            closed_fixture_raw_error(ManagedCatalogFixtureV1::ForbiddenColumnDefault);
        assert!(matches!(default, RawCatalogError::Column { .. }));
        assert!(
            physical.is_ok(),
            "column defaults remain closed-physical-decodable"
        );
        assert!(close.is_ok());
    }

    #[test]
    fn reachable_hidden_access_is_rejected() {
        let fixture = PersistentCatalogFixture::fresh();
        fixture.mutate(&["::access_level hidden node"]);
        assert_eq!(
            validation_error(&fixture),
            RawCatalogError::RelationShape {
                expected_relation: "node"
            }
        );
    }

    #[test]
    fn reachable_recursive_column_type_corruption_is_rejected() {
        let fixture = PersistentCatalogFixture::fresh();
        fixture.mutate(&[
            "::remove feedback_retry",
            ":create feedback_retry {key: String => fingerprint: Bytes, applied_at: Int}",
        ]);
        assert_eq!(
            validation_error(&fixture),
            RawCatalogError::Column {
                expected_relation: "feedback_retry",
                expected_column: "fingerprint"
            }
        );
    }

    #[test]
    fn reachable_hidden_hnsw_filter_corruption_is_rejected() {
        let fixture = PersistentCatalogFixture::fresh();
        fixture.mutate(&[
            "::hnsw drop node_vec:active_idx",
            "::hnsw create node_vec:active_idx {dim: 4, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'candidate'}",
        ]);
        assert_eq!(
            validation_error(&fixture),
            RawCatalogError::IndexManifest {
                owner_relation: "node_vec",
                expected_index: "active_idx"
            }
        );
    }

    #[test]
    fn hostile_name_never_reaches_bounded_diagnostic() {
        let fixture = PersistentCatalogFixture::fresh();
        fixture.mutate(&["::rename meta -> attacker_sentinel_must_never_reach_diagnostics"]);
        let error = validation_error(&fixture);
        assert_eq!(error, RawCatalogError::CatalogInventory);
        let diagnostic = error.to_string();
        assert!(!diagnostic.contains(ATTACKER_SENTINEL));
        assert!(diagnostic.len() <= 128);
    }
    #[test]
    fn capture_open_policy_fingerprint_binds_the_prior_policy_and_new_marker() {
        let mut hash = Sha256::new();
        hash.update(b"mneme.cozo.capture-open-admission.policy-fingerprint.v1\0");
        hash.update(OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1);
        hash.update(CAPTURE_V1_CATALOG_GENERATION_MARKER.as_bytes());
        let actual: [u8; 32] = hash.finalize().into();
        assert_eq!(
            actual,
            CAPTURE_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1
        );
    }
    #[test]
    fn single_graph_classifier_fingerprints_bind_predecessor_and_exact_successor_catalog() {
        for (domain, prior, expected) in [
            (
                &b"mneme.cozo.single-graph-open-admission.policy-fingerprint.v1\0"[..],
                EPISODE_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
                SINGLE_GRAPH_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
            ),
            (
                &b"mneme.cozo.single-graph-source-bound.policy-fingerprint.v1\0"[..],
                EPISODE_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1,
                SINGLE_GRAPH_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1,
            ),
        ] {
            let mut hash = Sha256::new();
            hash.update(domain);
            hash.update(prior);
            hash.update(SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER.as_bytes());
            hash.update(MANAGED_SINGLE_GRAPH_CATALOG_POLICY_FINGERPRINT_V1);
            let actual: [u8; 32] = hash.finalize().into();
            assert_eq!(actual, expected);
        }
    }
    #[test]
    fn concern_classifier_fingerprints_bind_predecessor_and_exact_successor_catalog() {
        for (domain, prior, expected) in [
            (
                &b"mneme.cozo.concern-open-admission.policy-fingerprint.v1\0"[..],
                SINGLE_GRAPH_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
                CONCERN_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
            ),
            (
                &b"mneme.cozo.concern-source-bound.policy-fingerprint.v1\0"[..],
                SINGLE_GRAPH_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1,
                CONCERN_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1,
            ),
        ] {
            let mut hash = Sha256::new();
            hash.update(domain);
            hash.update(prior);
            hash.update(CONCERN_V1_CATALOG_GENERATION_MARKER.as_bytes());
            hash.update(MANAGED_CONCERN_CATALOG_POLICY_FINGERPRINT_V1);
            let actual: [u8; 32] = hash.finalize().into();
            assert_eq!(actual, expected);
        }
    }
    #[test]
    fn episode_policy_fingerprints_bind_the_prior_policy_and_new_marker() {
        for (domain, prior, expected) in [
            (
                &b"mneme.cozo.episode-open-admission.policy-fingerprint.v1\0"[..],
                OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
                EPISODE_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
            ),
            (
                &b"mneme.cozo.episode-source-bound.policy-fingerprint.v1\0"[..],
                SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1,
                EPISODE_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1,
            ),
        ] {
            let mut hash = Sha256::new();
            hash.update(domain);
            hash.update(prior);
            hash.update(EPISODE_V1_CATALOG_GENERATION_MARKER.as_bytes());
            let actual: [u8; 32] = hash.finalize().into();
            assert_eq!(actual, expected);
        }
    }
    #[test]
    fn context_classifier_identities_bind_distinct_markers_and_canonical_contract() {
        for (domain, prior, expected) in [
            (
                &b"mneme.cozo.episode-context-open-admission.policy-fingerprint.v2\0"[..],
                CONCERN_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
                EPISODE_CONTEXT_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V2,
            ),
            (
                &b"mneme.cozo.episode-context-source-bound.policy-fingerprint.v2\0"[..],
                CONCERN_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1,
                EPISODE_CONTEXT_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V2,
            ),
        ] {
            let mut hash = Sha256::new();
            hash.update(domain);
            hash.update(prior);
            hash.update(EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER.as_bytes());
            hash.update(crate::canonical_node_contract::EPISODE_CONTEXT_META_KEY.as_bytes());
            hash.update(crate::canonical_node_contract::EPISODE_CONTEXT_META_VALUE.as_bytes());
            let actual: [u8; 32] = hash.finalize().into();
            assert_eq!(actual, expected);
            assert_ne!(actual, prior);
        }
    }
    #[test]
    fn touchstones_classifier_identities_bind_predecessor_and_successor_marker() {
        for (domain, prior, expected) in [
            (
                &b"mneme.cozo.touchstones-open-admission.policy-fingerprint.v1\0"[..],
                EPISODE_CONTEXT_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V2,
                TOUCHSTONES_OPEN_ADMISSION_CLASSIFIER_POLICY_FINGERPRINT_V1,
            ),
            (
                &b"mneme.cozo.touchstones-source-bound.policy-fingerprint.v1\0"[..],
                EPISODE_CONTEXT_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V2,
                TOUCHSTONES_SOURCE_BOUND_CLASSIFIER_POLICY_FINGERPRINT_V1,
            ),
        ] {
            let mut hash = Sha256::new();
            hash.update(domain);
            hash.update(prior);
            hash.update(TOUCHSTONES_V1_CATALOG_GENERATION_MARKER.as_bytes());
            let actual: [u8; 32] = hash.finalize().into();
            assert_eq!(actual, expected);
            assert_ne!(actual, prior);
        }
    }
}

fn canonical_contract_for(alternatives: [&str; 2]) -> (&'static str, &'static str) {
    if alternatives[1] == EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
        || alternatives[1] == TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
    {
        (
            crate::canonical_node_contract::EPISODE_CONTEXT_META_KEY,
            crate::canonical_node_contract::EPISODE_CONTEXT_META_VALUE,
        )
    } else if alternatives[1] == SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER
        || alternatives[1] == CONCERN_V1_CATALOG_GENERATION_MARKER
    {
        (
            crate::canonical_node_contract::SINGLE_GRAPH_META_KEY,
            crate::canonical_node_contract::SINGLE_GRAPH_META_VALUE,
        )
    } else {
        (
            crate::canonical_node_contract::META_KEY,
            crate::canonical_node_contract::META_VALUE,
        )
    }
}
