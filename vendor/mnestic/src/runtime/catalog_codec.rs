/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Exact historical relation-catalog codec classification.
//!
//! This module is intentionally an internal staging boundary. Acceptance here
//! proves one exact recursive wire encoding and yields an unattested frozen
//! catalog record. It does not prove that the decoded schema, ids, index
//! manifests, or expressions belong to Mneme's static F7 catalog; that semantic
//! authority belongs to the later raw-catalog oracle.
//!
//! The handle, schema, manifest, and simple enum layouts below are wire-frozen
//! mirrors rather than aliases for their live runtime structs. Two complex
//! leaves remain live serde dependencies because duplicating their custom wire
//! semantics here would create a second interpreter:
//!
//! - Expr in column defaults;
//! - DataValue in tokenizer arguments.
//!
//! Exact byte fixtures pin those dependencies for the catalog variants admitted
//! by the later oracle. This classifier alone must therefore never be treated as
//! semantic protocol admission.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io::{self, Write};

use rmp_serde::Serializer;
use serde::Serialize;

use crate::data::expr::Expr;
use crate::data::msgpack::{
    require_canonical, scan_exact_envelope, CanonicalMode, StoredMsgpackError,
    StoredMsgpackErrorKind, StoredMsgpackProfile, StoredMsgpackRoot,
};
#[cfg(test)]
use crate::data::relation::{
    ColType, ColumnDef, NullableColType, StoredRelationMetadata, VecElementType,
};
use crate::data::value::DataValue;
#[cfg(test)]
use crate::fts::{FtsIndexManifest, TokenizerConfig};
#[cfg(test)]
use crate::parse::sys::HnswDistance;
#[cfg(test)]
use crate::runtime::hnsw::HnswIndexManifest;
#[cfg(test)]
use crate::runtime::minhash_lsh::MinHashLshIndexManifest;
#[cfg(test)]
use crate::runtime::relation::{AccessLevel, RelationHandle, RelationId};

const CATALOG_BYTE_LIMIT: usize = StoredMsgpackProfile::RelationCatalog.byte_limit();

/// The three exact historical recursive catalog encodings.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum CatalogCodec {
    PositionalV0,
    PositionalV1,
    StructMapV1,
}

/// Closed catalog images available to downstream integration tests.
///
/// This is deliberately payload-free: callers can select one reviewed mutation,
/// but cannot supply catalog bytes, names, ids, expressions, or another encoder.
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ManagedCatalogFixtureV1 {
    AllPositionalV0,
    AllPositionalV1,
    AllStructMapV1,
    DeterministicMixed,
    DuplicateTopLevelRelationId,
    RelationCounterBelowMaxTopLevelId,
    NestedChildRelationIdMismatch,
    NestedChildRelationIdCollisionWithOwner,
    ForbiddenTemporaryRelation,
    ForbiddenPutTrigger,
    ForbiddenRemoveTrigger,
    ForbiddenReplaceTrigger,
    ForbiddenDescription,
    ForbiddenTemporalFloor,
    ForbiddenColumnDefault,
    ForbiddenLshIndex,
}

/// The exact historical encoding accepted for one managed catalog entry.
///
/// This is syntax evidence only. Successful decoding does not give any
/// generation semantic authority.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ManagedCatalogEncodingV1 {
    PositionalV0,
    PositionalV1,
    StructMapV1,
}

impl ManagedCatalogEncodingV1 {
    /// Whether the exact classifier identified struct-map v1 syntax.
    pub const fn is_struct_map_v1(self) -> bool {
        matches!(self, Self::StructMapV1)
    }
}

impl From<CatalogCodec> for ManagedCatalogEncodingV1 {
    fn from(value: CatalogCodec) -> Self {
        match value {
            CatalogCodec::PositionalV0 => Self::PositionalV0,
            CatalogCodec::PositionalV1 => Self::PositionalV1,
            CatalogCodec::StructMapV1 => Self::StructMapV1,
        }
    }
}

/// A bounded collection of sanitized managed relation-catalog entries.
///
/// The collection has no public constructor or mutation API. Mnestic's fixed
/// raw reader will assemble it only from scan-proven entries while its SQLite
/// read snapshot remains pinned.
///
/// The payload intentionally does not implement `Clone`, `Debug`, `Default`, or
/// serde traits. For example, whole-catalog cloning is rejected at compile time:
///
/// ```compile_fail
/// use cozo::ManagedCatalogCensusV1;
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<ManagedCatalogCensusV1>();
/// ```
pub struct ManagedCatalogCensusV1 {
    entries: Box<[ManagedCatalogEntryV1]>,
}

impl ManagedCatalogCensusV1 {
    pub(crate) fn from_entries(entries: Vec<ManagedCatalogEntryV1>) -> Self {
        Self {
            entries: entries.into_boxed_slice(),
        }
    }

    /// Every sanitized relation entry in fixed reader order.
    pub fn entries(&self) -> &[ManagedCatalogEntryV1] {
        &self.entries
    }

    /// Number of sanitized relation entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no relation entry was observed.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// One scan-proven and sanitized relation-catalog record.
///
/// Exact decoding is not F7 attestation: the caller must still match every
/// field against its closed-world oracle and bind this entry to its raw key and
/// physical census.
pub struct ManagedCatalogEntryV1 {
    encoding: ManagedCatalogEncodingV1,
    relation: ManagedRelationV1,
}

impl ManagedCatalogEntryV1 {
    /// Historical wire generation accepted by the exact classifier.
    pub const fn encoding(&self) -> ManagedCatalogEncodingV1 {
        self.encoding
    }

    /// Sanitized relation descriptor.
    pub const fn relation(&self) -> &ManagedRelationV1 {
        &self.relation
    }
}

/// Durable access-level syntax from a relation descriptor.
///
/// Hidden and protected remain representable so a semantic oracle can reject
/// them explicitly; their presence here does not admit them to managed F7.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ManagedAccessLevelV1 {
    Hidden,
    ReadOnly,
    Protected,
    Normal,
}

/// Durable vector element syntax from a column or index manifest.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ManagedVectorElementTypeV1 {
    F32,
    F64,
}

/// Durable HNSW distance syntax.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ManagedHnswDistanceV1 {
    L2,
    InnerProduct,
    Cosine,
}

/// A borrowed view of one recursively normalized column type.
#[derive(Clone, Copy)]
pub enum ManagedColumnTypeKindV1<'a> {
    Any,
    Bool,
    Int,
    Float,
    String,
    Bytes,
    Uuid,
    List {
        element: &'a ManagedColumnTypeV1,
        length: Option<u64>,
    },
    Vector {
        element: ManagedVectorElementTypeV1,
        length: u64,
    },
    Tuple(&'a [ManagedColumnTypeV1]),
    Validity,
    TxTime,
    Json,
}

enum ManagedColumnTypeKindOwnedV1 {
    Any,
    Bool,
    Int,
    Float,
    String,
    Bytes,
    Uuid,
    List {
        element: Box<ManagedColumnTypeV1>,
        length: Option<u64>,
    },
    Vector {
        element: ManagedVectorElementTypeV1,
        length: u64,
    },
    Tuple(Box<[ManagedColumnTypeV1]>),
    Validity,
    TxTime,
    Json,
}

/// One recursively normalized column type and its nullability.
///
/// The owned recursive representation is private. Consumers can inspect it only
/// through the bounded borrowed view returned by [`Self::kind`].
pub struct ManagedColumnTypeV1 {
    kind: ManagedColumnTypeKindOwnedV1,
    nullable: bool,
}

impl ManagedColumnTypeV1 {
    /// Syntactic kind, with recursive children borrowed from this payload.
    pub fn kind(&self) -> ManagedColumnTypeKindV1<'_> {
        match &self.kind {
            ManagedColumnTypeKindOwnedV1::Any => ManagedColumnTypeKindV1::Any,
            ManagedColumnTypeKindOwnedV1::Bool => ManagedColumnTypeKindV1::Bool,
            ManagedColumnTypeKindOwnedV1::Int => ManagedColumnTypeKindV1::Int,
            ManagedColumnTypeKindOwnedV1::Float => ManagedColumnTypeKindV1::Float,
            ManagedColumnTypeKindOwnedV1::String => ManagedColumnTypeKindV1::String,
            ManagedColumnTypeKindOwnedV1::Bytes => ManagedColumnTypeKindV1::Bytes,
            ManagedColumnTypeKindOwnedV1::Uuid => ManagedColumnTypeKindV1::Uuid,
            ManagedColumnTypeKindOwnedV1::List { element, length } => {
                ManagedColumnTypeKindV1::List {
                    element,
                    length: *length,
                }
            }
            ManagedColumnTypeKindOwnedV1::Vector { element, length } => {
                ManagedColumnTypeKindV1::Vector {
                    element: *element,
                    length: *length,
                }
            }
            ManagedColumnTypeKindOwnedV1::Tuple(elements) => {
                ManagedColumnTypeKindV1::Tuple(elements)
            }
            ManagedColumnTypeKindOwnedV1::Validity => ManagedColumnTypeKindV1::Validity,
            ManagedColumnTypeKindOwnedV1::TxTime => ManagedColumnTypeKindV1::TxTime,
            ManagedColumnTypeKindOwnedV1::Json => ManagedColumnTypeKindV1::Json,
        }
    }

    /// Whether this exact type node is nullable.
    pub const fn nullable(&self) -> bool {
        self.nullable
    }
}

/// One sanitized column definition.
pub struct ManagedColumnV1 {
    name: Box<str>,
    typing: ManagedColumnTypeV1,
    default_present: bool,
}

impl ManagedColumnV1 {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn typing(&self) -> &ManagedColumnTypeV1 {
        &self.typing
    }

    /// Whether the hostile wire contained any default expression.
    ///
    /// The expression itself is deliberately discarded before this payload
    /// crosses the Mnestic boundary.
    pub const fn default_present(&self) -> bool {
        self.default_present
    }
}

/// One sanitized ordinary-index descriptor and its recursive child relation.
pub struct ManagedNormalIndexV1 {
    name: Box<str>,
    relation: Box<ManagedRelationV1>,
    fields: Box<[u64]>,
}

impl ManagedNormalIndexV1 {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn relation(&self) -> &ManagedRelationV1 {
        &self.relation
    }

    pub fn fields(&self) -> &[u64] {
        &self.fields
    }
}

/// Sanitized HNSW manifest. Floating-point values are exposed only as their
/// exact IEEE-754 bits, so NaNs and signed zero cannot be normalized silently.
pub struct ManagedHnswManifestV1 {
    base_relation: Box<str>,
    index_name: Box<str>,
    vector_dimension: u64,
    dtype: ManagedVectorElementTypeV1,
    vector_fields: Box<[u64]>,
    distance: ManagedHnswDistanceV1,
    ef_construction: u64,
    m_neighbours: u64,
    m_max: u64,
    m_max0: u64,
    level_multiplier_bits: u64,
    index_filter: Option<Box<str>>,
    extend_candidates: bool,
    keep_pruned_connections: bool,
}

impl ManagedHnswManifestV1 {
    pub fn base_relation(&self) -> &str {
        &self.base_relation
    }

    pub fn index_name(&self) -> &str {
        &self.index_name
    }

    pub const fn vector_dimension(&self) -> u64 {
        self.vector_dimension
    }

    pub const fn dtype(&self) -> ManagedVectorElementTypeV1 {
        self.dtype
    }

    pub fn vector_fields(&self) -> &[u64] {
        &self.vector_fields
    }

    pub const fn distance(&self) -> ManagedHnswDistanceV1 {
        self.distance
    }

    pub const fn ef_construction(&self) -> u64 {
        self.ef_construction
    }

    pub const fn m_neighbours(&self) -> u64 {
        self.m_neighbours
    }

    pub const fn m_max(&self) -> u64 {
        self.m_max
    }

    pub const fn m_max0(&self) -> u64 {
        self.m_max0
    }

    pub const fn level_multiplier_bits(&self) -> u64 {
        self.level_multiplier_bits
    }

    pub fn index_filter(&self) -> Option<&str> {
        self.index_filter.as_deref()
    }

    pub const fn extend_candidates(&self) -> bool {
        self.extend_candidates
    }

    pub const fn keep_pruned_connections(&self) -> bool {
        self.keep_pruned_connections
    }
}

/// One sanitized HNSW-index descriptor and its recursive child relation.
pub struct ManagedHnswIndexV1 {
    name: Box<str>,
    relation: Box<ManagedRelationV1>,
    manifest: ManagedHnswManifestV1,
}

impl ManagedHnswIndexV1 {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn relation(&self) -> &ManagedRelationV1 {
        &self.relation
    }

    pub const fn manifest(&self) -> &ManagedHnswManifestV1 {
        &self.manifest
    }
}

/// Sanitized tokenizer descriptor. Argument values never cross the boundary;
/// the count is sufficient for Mneme's zero-argument F7 oracle to reject them.
pub struct ManagedTokenizerV1 {
    name: Box<str>,
    argument_count: u64,
}

impl ManagedTokenizerV1 {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn argument_count(&self) -> u64 {
        self.argument_count
    }
}

/// Sanitized full-text index manifest.
pub struct ManagedFtsManifestV1 {
    base_relation: Box<str>,
    index_name: Box<str>,
    extractor: Box<str>,
    tokenizer: ManagedTokenizerV1,
    filters: Box<[ManagedTokenizerV1]>,
}

impl ManagedFtsManifestV1 {
    pub fn base_relation(&self) -> &str {
        &self.base_relation
    }

    pub fn index_name(&self) -> &str {
        &self.index_name
    }

    pub fn extractor(&self) -> &str {
        &self.extractor
    }

    pub const fn tokenizer(&self) -> &ManagedTokenizerV1 {
        &self.tokenizer
    }

    pub fn filters(&self) -> &[ManagedTokenizerV1] {
        &self.filters
    }
}

/// One sanitized full-text-index descriptor and its recursive child relation.
pub struct ManagedFtsIndexV1 {
    name: Box<str>,
    relation: Box<ManagedRelationV1>,
    manifest: ManagedFtsManifestV1,
}

impl ManagedFtsIndexV1 {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn relation(&self) -> &ManagedRelationV1 {
        &self.relation
    }

    pub const fn manifest(&self) -> &ManagedFtsManifestV1 {
        &self.manifest
    }
}

/// A sanitized recursive relation descriptor.
///
/// This type deliberately has no public constructor, mutable accessors, whole
/// payload `Clone`, large `Debug`, serde traits, or live Cozo domain values.
///
/// ```compile_fail
/// use cozo::ManagedRelationV1;
/// fn requires_debug<T: std::fmt::Debug>() {}
/// requires_debug::<ManagedRelationV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedRelationV1;
/// fn requires_default<T: Default>() {}
/// requires_default::<ManagedRelationV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedRelationV1;
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<ManagedRelationV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedRelationV1;
/// fn requires_deserialize<T: for<'de> serde::Deserialize<'de>>() {}
/// requires_deserialize::<ManagedRelationV1>();
/// ```
pub struct ManagedRelationV1 {
    name: Box<str>,
    id: u64,
    keys: Box<[ManagedColumnV1]>,
    non_keys: Box<[ManagedColumnV1]>,
    put_triggers: Box<[Box<str>]>,
    remove_triggers: Box<[Box<str>]>,
    replace_triggers: Box<[Box<str>]>,
    access_level: ManagedAccessLevelV1,
    temporary: bool,
    normal_indices: Box<[ManagedNormalIndexV1]>,
    hnsw_indices: Box<[ManagedHnswIndexV1]>,
    fts_indices: Box<[ManagedFtsIndexV1]>,
    lsh_index_count: u64,
    description: Box<str>,
    temporal_gc_floor: Option<i64>,
}

impl ManagedRelationV1 {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn keys(&self) -> &[ManagedColumnV1] {
        &self.keys
    }

    pub fn non_keys(&self) -> &[ManagedColumnV1] {
        &self.non_keys
    }

    pub fn put_triggers(&self) -> impl ExactSizeIterator<Item = &str> {
        self.put_triggers.iter().map(Box::as_ref)
    }

    pub fn remove_triggers(&self) -> impl ExactSizeIterator<Item = &str> {
        self.remove_triggers.iter().map(Box::as_ref)
    }

    pub fn replace_triggers(&self) -> impl ExactSizeIterator<Item = &str> {
        self.replace_triggers.iter().map(Box::as_ref)
    }

    pub const fn access_level(&self) -> ManagedAccessLevelV1 {
        self.access_level
    }

    pub const fn is_temporary(&self) -> bool {
        self.temporary
    }

    pub fn normal_indices(&self) -> &[ManagedNormalIndexV1] {
        &self.normal_indices
    }

    pub fn hnsw_indices(&self) -> &[ManagedHnswIndexV1] {
        &self.hnsw_indices
    }

    pub fn fts_indices(&self) -> &[ManagedFtsIndexV1] {
        &self.fts_indices
    }

    /// Count of LSH manifests. Their attacker-controlled contents and children
    /// are deliberately discarded because managed F7 admits no LSH index.
    pub const fn lsh_index_count(&self) -> u64 {
        self.lsh_index_count
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub const fn temporal_gc_floor(&self) -> Option<i64> {
        self.temporal_gc_floor
    }
}

/// A syntactically exact catalog record that has not passed the static F7
/// semantic oracle.
pub(crate) struct UnattestedCatalogRecord {
    codec: CatalogCodec,
    frozen: StructMapRelationHandleV1,
}

impl UnattestedCatalogRecord {
    pub(crate) const fn codec(&self) -> CatalogCodec {
        self.codec
    }

    pub(crate) fn name(&self) -> &str {
        &self.frozen.name
    }

    pub(crate) const fn relation_id(&self) -> u64 {
        self.frozen.id.0
    }

    /// Consume this scan-proven record into the sanitized managed boundary.
    ///
    /// Keeping this consuming and crate-private prevents an unattested frozen
    /// value from remaining available as a parallel mutation or decode path.
    pub(crate) fn into_managed_entry_v1(self) -> ManagedCatalogEntryV1 {
        ManagedCatalogEntryV1 {
            encoding: self.codec.into(),
            relation: self.frozen.into(),
        }
    }

    /// Canonicalize this scan-proven record to the frozen v1 struct-map wire.
    ///
    /// There is deliberately no constructor from an arbitrary live handle:
    /// rmp-serde does not enforce encoder recursion depth. Restricting this
    /// writer to a record decoded from the bounded scanner keeps serialization
    /// within the already-proven depth/token/container envelope. The completed
    /// bytes are scanned again before return.
    pub(crate) fn canonical_struct_map(&self) -> Result<Vec<u8>, CatalogCodecError> {
        encode_frozen_struct_map_v1(&self.frozen)
    }
}

/// Bounded, input-independent classifier diagnostics.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum CatalogCodecError {
    Envelope(StoredMsgpackError),
    CandidateRejected {
        codec: CatalogCodec,
        error: StoredMsgpackError,
    },
    NoCanonicalArray {
        positional_v1: StoredMsgpackErrorKind,
        positional_v0: StoredMsgpackErrorKind,
    },
    OutputByteLimit {
        limit: u64,
        observed: u64,
    },
    OutputEncode,
    OutputEnvelope(StoredMsgpackError),
    OutputWrongRoot,
    CatalogSetInvalidRelationId,
    CatalogSetDuplicateRelationId,
    CatalogSetTemporaryRelation,
    CatalogSetDuplicateRelationName,
    CatalogSetMissingStandaloneChild,
    CatalogSetChildDescriptorMismatch,
    #[cfg(any(test, feature = "test-hooks"))]
    FixturePrecondition,
}

impl Display for CatalogCodecError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Envelope(error) => write!(formatter, "catalog envelope rejected: {error}"),
            Self::CandidateRejected { codec, error } => {
                write!(formatter, "catalog {codec:?} candidate rejected: {error}")
            }
            Self::NoCanonicalArray {
                positional_v1,
                positional_v0,
            } => write!(
                formatter,
                "catalog array matched no canonical codec (v1 {positional_v1:?}, v0 {positional_v0:?})"
            ),
            Self::OutputByteLimit { limit, observed } => write!(
                formatter,
                "canonical catalog output exceeded byte limit {limit} (observed {observed})"
            ),
            Self::OutputEncode => formatter.write_str("canonical catalog output encoding failed"),
            Self::OutputEnvelope(error) => {
                write!(formatter, "canonical catalog output envelope rejected: {error}")
            }
            Self::OutputWrongRoot => {
                formatter.write_str("canonical catalog output did not have a map root")
            }
            Self::CatalogSetInvalidRelationId => {
                formatter.write_str("catalog set contained an invalid persistent relation id")
            }
            Self::CatalogSetDuplicateRelationId => {
                formatter.write_str("catalog set contained a duplicate persistent relation id")
            }
            Self::CatalogSetTemporaryRelation => {
                formatter.write_str("catalog set contained a temporary relation")
            }
            Self::CatalogSetDuplicateRelationName => {
                formatter.write_str("catalog set contained a duplicate relation name")
            }
            Self::CatalogSetMissingStandaloneChild => {
                formatter.write_str("catalog set omitted a nested child's standalone descriptor")
            }
            Self::CatalogSetChildDescriptorMismatch => formatter
                .write_str("catalog set nested and standalone child descriptors differed"),
            #[cfg(any(test, feature = "test-hooks"))]
            Self::FixturePrecondition => {
                formatter.write_str("catalog fixture precondition was not satisfied")
            }
        }
    }
}

impl Error for CatalogCodecError {}

fn encode_frozen_struct_map_v1(
    frozen: &StructMapRelationHandleV1,
) -> Result<Vec<u8>, CatalogCodecError> {
    let mut writer = BoundedCatalogWriter::new();
    let encoded = frozen.serialize(&mut Serializer::new(&mut writer).with_struct_map());
    if let Some(observed) = writer.limit_observed {
        return Err(CatalogCodecError::OutputByteLimit {
            limit: CATALOG_BYTE_LIMIT as u64,
            observed,
        });
    }
    encoded.map_err(|_| CatalogCodecError::OutputEncode)?;
    let bytes = writer.bytes;
    let scanned = scan_exact_envelope(&bytes, StoredMsgpackProfile::RelationCatalog)
        .map_err(CatalogCodecError::OutputEnvelope)?;
    if scanned.root() != StoredMsgpackRoot::Map {
        return Err(CatalogCodecError::OutputWrongRoot);
    }
    Ok(bytes)
}

/// Validate the generic linkage invariants needed before a complete catalog
/// set may be rewritten. This does not attest a Mneme schema: the caller still
/// owns its exact semantic oracle. It only proves that every descriptor is a
/// persistent, uniquely identified top-level record and that every recursive
/// child is byte-identical, under the frozen struct-map ABI, to its standalone
/// catalog record.
pub(crate) fn validate_unattested_catalog_set_v1(
    records: &[UnattestedCatalogRecord],
) -> Result<(), CatalogCodecError> {
    const RELATION_ID_EXCLUSIVE_END: u64 = 1_u64 << 48;

    let mut by_name = BTreeMap::new();
    let mut relation_ids = std::collections::BTreeSet::new();
    for record in records {
        if record.frozen.id.0 == 0 || record.frozen.id.0 >= RELATION_ID_EXCLUSIVE_END {
            return Err(CatalogCodecError::CatalogSetInvalidRelationId);
        }
        if !relation_ids.insert(record.frozen.id.0) {
            return Err(CatalogCodecError::CatalogSetDuplicateRelationId);
        }
        if record.frozen.is_temp {
            return Err(CatalogCodecError::CatalogSetTemporaryRelation);
        }
        if by_name
            .insert(record.frozen.name.as_str(), record)
            .is_some()
        {
            return Err(CatalogCodecError::CatalogSetDuplicateRelationName);
        }
    }

    for record in records {
        for (child, _) in record.frozen.indices.values() {
            validate_standalone_child_v1(child, &by_name)?;
        }
        for (child, _) in record.frozen.hnsw_indices.values() {
            validate_standalone_child_v1(child, &by_name)?;
        }
        for (child, _) in record.frozen.fts_indices.values() {
            validate_standalone_child_v1(child, &by_name)?;
        }
        for (child, inverse, _) in record.frozen.lsh_indices.values() {
            validate_standalone_child_v1(child, &by_name)?;
            validate_standalone_child_v1(inverse, &by_name)?;
        }
    }
    Ok(())
}

fn validate_standalone_child_v1(
    child: &StructMapRelationHandleV1,
    by_name: &BTreeMap<&str, &UnattestedCatalogRecord>,
) -> Result<(), CatalogCodecError> {
    let standalone = by_name
        .get(child.name.as_str())
        .ok_or(CatalogCodecError::CatalogSetMissingStandaloneChild)?;
    let nested_bytes = encode_frozen_struct_map_v1(child)?;
    let standalone_bytes = encode_frozen_struct_map_v1(&standalone.frozen)?;
    if nested_bytes != standalone_bytes {
        return Err(CatalogCodecError::CatalogSetChildDescriptorMismatch);
    }
    Ok(())
}

/// Classify and decode one exact recursive relation-catalog encoding.
pub(crate) fn decode_unattested_catalog(
    encoded: &[u8],
) -> Result<UnattestedCatalogRecord, CatalogCodecError> {
    let scanned = scan_exact_envelope(encoded, StoredMsgpackProfile::RelationCatalog)
        .map_err(CatalogCodecError::Envelope)?;
    match scanned.root() {
        StoredMsgpackRoot::Map => {
            let wire = scanned
                .decode::<StructMapRelationHandleV1>()
                .and_then(|wire| {
                    require_canonical(&wire, encoded, CanonicalMode::StructMap)?;
                    Ok(wire)
                })
                .map_err(|error| CatalogCodecError::CandidateRejected {
                    codec: CatalogCodec::StructMapV1,
                    error,
                })?;
            Ok(record_from_frozen(CatalogCodec::StructMapV1, wire))
        }
        StoredMsgpackRoot::Array => {
            let positional_v1 = scanned
                .decode::<PositionalRelationHandleV1>()
                .and_then(|wire| {
                    require_canonical(&wire, encoded, CanonicalMode::Compact)?;
                    Ok(wire)
                });
            match positional_v1 {
                Ok(wire) => Ok(record_from_frozen(CatalogCodec::PositionalV1, wire.into())),
                Err(positional_v1_error) => {
                    let positional_v0 =
                        scanned
                            .decode::<PositionalRelationHandleV0>()
                            .and_then(|wire| {
                                require_canonical(&wire, encoded, CanonicalMode::Compact)?;
                                Ok(wire)
                            });
                    match positional_v0 {
                        Ok(wire) => Ok(record_from_frozen(CatalogCodec::PositionalV0, wire.into())),
                        Err(positional_v0_error) => Err(CatalogCodecError::NoCanonicalArray {
                            positional_v1: positional_v1_error.kind(),
                            positional_v0: positional_v0_error.kind(),
                        }),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn encode_test_catalog_record(handle: &RelationHandle, codec: CatalogCodec) -> Vec<u8> {
    let mut encoded = Vec::new();
    match codec {
        CatalogCodec::PositionalV0 => PositionalRelationHandleV0::from(handle)
            .serialize(&mut Serializer::new(&mut encoded))
            .expect("trusted positional-v0 test catalog must serialize"),
        CatalogCodec::PositionalV1 => PositionalRelationHandleV1::from(handle)
            .serialize(&mut Serializer::new(&mut encoded))
            .expect("trusted positional-v1 test catalog must serialize"),
        CatalogCodec::StructMapV1 => StructMapRelationHandleV1::from(handle)
            .serialize(&mut Serializer::new(&mut encoded).with_struct_map())
            .expect("trusted struct-map-v1 test catalog must serialize"),
    }
    encoded
}

fn record_from_frozen(
    codec: CatalogCodec,
    frozen: StructMapRelationHandleV1,
) -> UnattestedCatalogRecord {
    UnattestedCatalogRecord { codec, frozen }
}

struct BoundedCatalogWriter {
    bytes: Vec<u8>,
    limit_observed: Option<u64>,
}

impl BoundedCatalogWriter {
    const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            limit_observed: None,
        }
    }
}

impl Write for BoundedCatalogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(observed) = self.bytes.len().checked_add(bytes.len()) else {
            self.limit_observed = Some(u64::MAX);
            return Err(io::Error::other("canonical catalog output length overflow"));
        };
        if observed > CATALOG_BYTE_LIMIT {
            self.limit_observed = Some(observed as u64);
            return Err(io::Error::other(
                "canonical catalog output exceeded byte limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
struct WireRelationId(u64);

#[cfg(test)]
impl From<RelationId> for WireRelationId {
    fn from(value: RelationId) -> Self {
        Self(value.0)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
// Historical wire syntax only. The F7 Normal/ReadOnly vocabulary is pinned by
// exact fixtures; decoding another access variant does not make it G2-valid.
enum WireAccessLevel {
    Hidden,
    ReadOnly,
    Protected,
    Normal,
}

#[cfg(test)]
impl From<AccessLevel> for WireAccessLevel {
    fn from(value: AccessLevel) -> Self {
        match value {
            AccessLevel::Hidden => Self::Hidden,
            AccessLevel::ReadOnly => Self::ReadOnly,
            AccessLevel::Protected => Self::Protected,
            AccessLevel::Normal => Self::Normal,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
enum WireVecElementType {
    F32,
    F64,
}

#[cfg(test)]
impl From<VecElementType> for WireVecElementType {
    fn from(value: VecElementType) -> Self {
        match value {
            VecElementType::F32 => Self::F32,
            VecElementType::F64 => Self::F64,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
enum WireHnswDistance {
    L2,
    InnerProduct,
    Cosine,
}

#[cfg(test)]
impl From<HnswDistance> for WireHnswDistance {
    fn from(value: HnswDistance) -> Self {
        match value {
            HnswDistance::L2 => Self::L2,
            HnswDistance::InnerProduct => Self::InnerProduct,
            HnswDistance::Cosine => Self::Cosine,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
#[serde(deny_unknown_fields)]
struct WireNullableColType {
    coltype: WireColType,
    nullable: bool,
}

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
// This is the historical syntactic enum, not the F7 allow-list. G2 admits
// only the exact leaf vocabulary pinned below and rejects every other decoded
// variant even though G1a must still classify its wire shape safely.
enum WireColType {
    Any,
    Bool,
    Int,
    Float,
    String,
    Bytes,
    Uuid,
    List {
        eltype: Box<WireNullableColType>,
        len: Option<u64>,
    },
    Vec {
        eltype: WireVecElementType,
        len: u64,
    },
    Tuple(Vec<WireNullableColType>),
    Validity,
    TxTime,
    Json,
}

#[cfg(test)]
impl From<NullableColType> for WireNullableColType {
    fn from(value: NullableColType) -> Self {
        Self {
            coltype: value.coltype.into(),
            nullable: value.nullable,
        }
    }
}

#[cfg(test)]
impl From<ColType> for WireColType {
    fn from(value: ColType) -> Self {
        match value {
            ColType::Any => Self::Any,
            ColType::Bool => Self::Bool,
            ColType::Int => Self::Int,
            ColType::Float => Self::Float,
            ColType::String => Self::String,
            ColType::Bytes => Self::Bytes,
            ColType::Uuid => Self::Uuid,
            ColType::List { eltype, len } => Self::List {
                eltype: Box::new((*eltype).into()),
                len: len.map(|value| value as u64),
            },
            ColType::Vec { eltype, len } => Self::Vec {
                eltype: eltype.into(),
                len: len as u64,
            },
            ColType::Tuple(types) => Self::Tuple(types.into_iter().map(Into::into).collect()),
            ColType::Validity => Self::Validity,
            ColType::TxTime => Self::TxTime,
            ColType::Json => Self::Json,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
#[serde(deny_unknown_fields)]
struct WireColumnDef {
    name: String,
    typing: WireNullableColType,
    default_gen: Option<Expr>,
}

#[cfg(test)]
impl From<ColumnDef> for WireColumnDef {
    fn from(value: ColumnDef) -> Self {
        Self {
            name: value.name.to_string(),
            typing: value.typing.into(),
            default_gen: value.default_gen,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
#[serde(deny_unknown_fields)]
struct WireStoredRelationMetadata {
    keys: Vec<WireColumnDef>,
    non_keys: Vec<WireColumnDef>,
}

#[cfg(test)]
impl From<StoredRelationMetadata> for WireStoredRelationMetadata {
    fn from(value: StoredRelationMetadata) -> Self {
        Self {
            keys: value.keys.into_iter().map(Into::into).collect(),
            non_keys: value.non_keys.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
#[serde(deny_unknown_fields)]
struct WireTokenizerConfig {
    name: String,
    args: Vec<DataValue>,
}

#[cfg(test)]
impl From<TokenizerConfig> for WireTokenizerConfig {
    fn from(value: TokenizerConfig) -> Self {
        Self {
            name: value.name.to_string(),
            args: value.args,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
#[serde(deny_unknown_fields)]
struct WireHnswIndexManifest {
    base_relation: String,
    index_name: String,
    vec_dim: u64,
    dtype: WireVecElementType,
    vec_fields: Vec<u64>,
    distance: WireHnswDistance,
    ef_construction: u64,
    m_neighbours: u64,
    m_max: u64,
    m_max0: u64,
    level_multiplier: f64,
    index_filter: Option<String>,
    extend_candidates: bool,
    keep_pruned_connections: bool,
}

#[cfg(test)]
impl From<HnswIndexManifest> for WireHnswIndexManifest {
    fn from(value: HnswIndexManifest) -> Self {
        Self {
            base_relation: value.base_relation.to_string(),
            index_name: value.index_name.to_string(),
            vec_dim: value.vec_dim as u64,
            dtype: value.dtype.into(),
            vec_fields: value
                .vec_fields
                .into_iter()
                .map(|value| value as u64)
                .collect(),
            distance: value.distance.into(),
            ef_construction: value.ef_construction as u64,
            m_neighbours: value.m_neighbours as u64,
            m_max: value.m_max as u64,
            m_max0: value.m_max0 as u64,
            level_multiplier: value.level_multiplier,
            index_filter: value.index_filter,
            extend_candidates: value.extend_candidates,
            keep_pruned_connections: value.keep_pruned_connections,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
#[serde(deny_unknown_fields)]
struct WireFtsIndexManifest {
    base_relation: String,
    index_name: String,
    extractor: String,
    tokenizer: WireTokenizerConfig,
    filters: Vec<WireTokenizerConfig>,
}

#[cfg(test)]
impl From<FtsIndexManifest> for WireFtsIndexManifest {
    fn from(value: FtsIndexManifest) -> Self {
        Self {
            base_relation: value.base_relation.to_string(),
            index_name: value.index_name.to_string(),
            extractor: value.extractor,
            tokenizer: value.tokenizer.into(),
            filters: value.filters.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
#[serde(deny_unknown_fields)]
struct WireMinHashLshIndexManifest {
    base_relation: String,
    index_name: String,
    extractor: String,
    n_gram: u64,
    tokenizer: WireTokenizerConfig,
    filters: Vec<WireTokenizerConfig>,
    num_perm: u64,
    n_bands: u64,
    n_rows_in_band: u64,
    threshold: f64,
    perms: Vec<u8>,
}

#[cfg(test)]
impl From<MinHashLshIndexManifest> for WireMinHashLshIndexManifest {
    fn from(value: MinHashLshIndexManifest) -> Self {
        Self {
            base_relation: value.base_relation.to_string(),
            index_name: value.index_name.to_string(),
            extractor: value.extractor,
            n_gram: value.n_gram as u64,
            tokenizer: value.tokenizer.into(),
            filters: value.filters.into_iter().map(Into::into).collect(),
            num_perm: value.num_perm as u64,
            n_bands: value.n_bands as u64,
            n_rows_in_band: value.n_rows_in_band as u64,
            threshold: value.threshold,
            perms: value.perms,
        }
    }
}

/// Exact 13-field historical handle. Every recursive child uses this same
/// v0 mirror, so no nested temporal-floor field can be accepted or emitted.
#[derive(Debug, Clone, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
#[serde(deny_unknown_fields)]
struct PositionalRelationHandleV0 {
    name: String,
    id: WireRelationId,
    metadata: WireStoredRelationMetadata,
    put_triggers: Vec<String>,
    rm_triggers: Vec<String>,
    replace_triggers: Vec<String>,
    access_level: WireAccessLevel,
    is_temp: bool,
    indices: BTreeMap<String, (PositionalRelationHandleV0, Vec<u64>)>,
    hnsw_indices: BTreeMap<String, (PositionalRelationHandleV0, WireHnswIndexManifest)>,
    fts_indices: BTreeMap<String, (PositionalRelationHandleV0, WireFtsIndexManifest)>,
    lsh_indices: BTreeMap<
        String,
        (
            PositionalRelationHandleV0,
            PositionalRelationHandleV0,
            WireMinHashLshIndexManifest,
        ),
    >,
    description: String,
}

/// Exact 14-field positional handle. Every recursive child uses this same v1
/// mirror and therefore requires its own trailing temporal-floor field.
#[derive(Debug, Clone, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
#[serde(deny_unknown_fields)]
struct PositionalRelationHandleV1 {
    name: String,
    id: WireRelationId,
    metadata: WireStoredRelationMetadata,
    put_triggers: Vec<String>,
    rm_triggers: Vec<String>,
    replace_triggers: Vec<String>,
    access_level: WireAccessLevel,
    is_temp: bool,
    indices: BTreeMap<String, (PositionalRelationHandleV1, Vec<u64>)>,
    hnsw_indices: BTreeMap<String, (PositionalRelationHandleV1, WireHnswIndexManifest)>,
    fts_indices: BTreeMap<String, (PositionalRelationHandleV1, WireFtsIndexManifest)>,
    lsh_indices: BTreeMap<
        String,
        (
            PositionalRelationHandleV1,
            PositionalRelationHandleV1,
            WireMinHashLshIndexManifest,
        ),
    >,
    description: String,
    tt_gc_floor: Option<i64>,
}

/// Exact 14-field struct-map handle. Field declaration order is the canonical
/// map-key order, and every recursive child uses this same map mirror.
#[derive(Debug, Clone, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
#[serde(deny_unknown_fields)]
struct StructMapRelationHandleV1 {
    name: String,
    id: WireRelationId,
    metadata: WireStoredRelationMetadata,
    put_triggers: Vec<String>,
    rm_triggers: Vec<String>,
    replace_triggers: Vec<String>,
    access_level: WireAccessLevel,
    is_temp: bool,
    indices: BTreeMap<String, (StructMapRelationHandleV1, Vec<u64>)>,
    hnsw_indices: BTreeMap<String, (StructMapRelationHandleV1, WireHnswIndexManifest)>,
    fts_indices: BTreeMap<String, (StructMapRelationHandleV1, WireFtsIndexManifest)>,
    lsh_indices: BTreeMap<
        String,
        (
            StructMapRelationHandleV1,
            StructMapRelationHandleV1,
            WireMinHashLshIndexManifest,
        ),
    >,
    description: String,
    tt_gc_floor: Option<i64>,
}

#[cfg(any(test, feature = "test-hooks"))]
const MANAGED_CATALOG_FIXTURE_RELATION_COUNT_V1: usize = 26;
#[cfg(any(test, feature = "test-hooks"))]
const MANAGED_CATALOG_FIXTURE_SENTINEL_V1: &str = "attacker_sentinel_must_never_reach_diagnostics";

#[cfg(any(test, feature = "test-hooks"))]
pub(crate) enum CatalogFixtureRewriteV1 {
    RelationValues(Box<[CatalogFixtureValueUpdateV1]>),
    RelationCounter([u8; 8]),
}

#[cfg(any(test, feature = "test-hooks"))]
pub(crate) struct CatalogFixtureValueUpdateV1 {
    position: usize,
    value: Box<[u8]>,
}

#[cfg(any(test, feature = "test-hooks"))]
impl CatalogFixtureValueUpdateV1 {
    pub(crate) const fn position(&self) -> usize {
        self.position
    }

    pub(crate) fn value(&self) -> &[u8] {
        &self.value
    }
}

#[cfg(any(test, feature = "test-hooks"))]
pub(crate) fn rewrite_catalog_fixture_records_v1(
    values: &[&[u8]],
    fixture: ManagedCatalogFixtureV1,
) -> Result<CatalogFixtureRewriteV1, CatalogCodecError> {
    if values.len() != MANAGED_CATALOG_FIXTURE_RELATION_COUNT_V1 {
        return Err(CatalogCodecError::FixturePrecondition);
    }

    let mut records = Vec::with_capacity(MANAGED_CATALOG_FIXTURE_RELATION_COUNT_V1);
    for value in values {
        records.push(decode_unattested_catalog(value)?);
    }
    if records
        .iter()
        .any(|record| record.codec != CatalogCodec::StructMapV1)
    {
        return Err(CatalogCodecError::FixturePrecondition);
    }

    if fixture == ManagedCatalogFixtureV1::RelationCounterBelowMaxTopLevelId {
        let maximum = records
            .iter()
            .map(|record| record.frozen.id.0)
            .max()
            .filter(|maximum| *maximum > 0)
            .ok_or(CatalogCodecError::FixturePrecondition)?;
        return Ok(CatalogFixtureRewriteV1::RelationCounter(
            maximum.saturating_sub(1).to_be_bytes(),
        ));
    }

    let mut codecs = [CatalogCodec::StructMapV1; MANAGED_CATALOG_FIXTURE_RELATION_COUNT_V1];
    let selected = match fixture {
        ManagedCatalogFixtureV1::AllPositionalV0 => {
            codecs.fill(CatalogCodec::PositionalV0);
            None
        }
        ManagedCatalogFixtureV1::AllPositionalV1 => {
            codecs.fill(CatalogCodec::PositionalV1);
            None
        }
        ManagedCatalogFixtureV1::AllStructMapV1 => None,
        ManagedCatalogFixtureV1::DeterministicMixed => {
            for (position, codec) in codecs.iter_mut().enumerate() {
                *codec = match position % 3 {
                    0 => CatalogCodec::PositionalV0,
                    1 => CatalogCodec::PositionalV1,
                    _ => CatalogCodec::StructMapV1,
                };
            }
            None
        }
        ManagedCatalogFixtureV1::DuplicateTopLevelRelationId => {
            let duplicate = records[0].frozen.id;
            records[1].frozen.id = duplicate;
            Some(1)
        }
        ManagedCatalogFixtureV1::NestedChildRelationIdMismatch => {
            Some(mutate_nested_fixture_id(&mut records, false)?)
        }
        ManagedCatalogFixtureV1::NestedChildRelationIdCollisionWithOwner => {
            Some(mutate_nested_fixture_id(&mut records, true)?)
        }
        ManagedCatalogFixtureV1::ForbiddenTemporaryRelation => {
            records[0].frozen.is_temp = true;
            Some(0)
        }
        ManagedCatalogFixtureV1::ForbiddenPutTrigger => {
            records[0]
                .frozen
                .put_triggers
                .push(MANAGED_CATALOG_FIXTURE_SENTINEL_V1.to_owned());
            Some(0)
        }
        ManagedCatalogFixtureV1::ForbiddenRemoveTrigger => {
            records[0]
                .frozen
                .rm_triggers
                .push(MANAGED_CATALOG_FIXTURE_SENTINEL_V1.to_owned());
            Some(0)
        }
        ManagedCatalogFixtureV1::ForbiddenReplaceTrigger => {
            records[0]
                .frozen
                .replace_triggers
                .push(MANAGED_CATALOG_FIXTURE_SENTINEL_V1.to_owned());
            Some(0)
        }
        ManagedCatalogFixtureV1::ForbiddenDescription => {
            records[0].frozen.description = MANAGED_CATALOG_FIXTURE_SENTINEL_V1.to_owned();
            Some(0)
        }
        ManagedCatalogFixtureV1::ForbiddenTemporalFloor => {
            records[0].frozen.tt_gc_floor = Some(0);
            Some(0)
        }
        ManagedCatalogFixtureV1::ForbiddenColumnDefault => {
            Some(mutate_fixture_column_default(&mut records)?)
        }
        ManagedCatalogFixtureV1::ForbiddenLshIndex => {
            mutate_fixture_lsh_index(&mut records)?;
            Some(0)
        }
        ManagedCatalogFixtureV1::RelationCounterBelowMaxTopLevelId => {
            return Err(CatalogCodecError::FixturePrecondition);
        }
    };

    let positions: Box<dyn Iterator<Item = usize>> = match selected {
        Some(position) => Box::new(std::iter::once(position)),
        None => Box::new(0..records.len()),
    };
    let mut updates = Vec::with_capacity(selected.map_or(records.len(), |_| 1));
    for position in positions {
        updates.push(CatalogFixtureValueUpdateV1 {
            position,
            value: encode_frozen_catalog_fixture_record(&records[position], codecs[position])?
                .into_boxed_slice(),
        });
    }
    Ok(CatalogFixtureRewriteV1::RelationValues(
        updates.into_boxed_slice(),
    ))
}

#[cfg(any(test, feature = "test-hooks"))]
fn encode_frozen_catalog_fixture_record(
    record: &UnattestedCatalogRecord,
    codec: CatalogCodec,
) -> Result<Vec<u8>, CatalogCodecError> {
    if codec == CatalogCodec::PositionalV0
        && !catalog_fixture_is_lossless_positional_v0(&record.frozen)
    {
        return Err(CatalogCodecError::FixturePrecondition);
    }
    let mut writer = BoundedCatalogWriter::new();
    let encoded = match codec {
        CatalogCodec::PositionalV0 => PositionalRelationHandleV0::from(&record.frozen)
            .serialize(&mut Serializer::new(&mut writer)),
        CatalogCodec::PositionalV1 => record.frozen.serialize(&mut Serializer::new(&mut writer)),
        CatalogCodec::StructMapV1 => record
            .frozen
            .serialize(&mut Serializer::new(&mut writer).with_struct_map()),
    };
    if let Some(observed) = writer.limit_observed {
        return Err(CatalogCodecError::OutputByteLimit {
            limit: CATALOG_BYTE_LIMIT as u64,
            observed,
        });
    }
    encoded.map_err(|_| CatalogCodecError::OutputEncode)?;
    let bytes = writer.bytes;
    let classified = decode_unattested_catalog(&bytes)?;
    if classified.codec != codec {
        return Err(CatalogCodecError::FixturePrecondition);
    }
    Ok(bytes)
}

#[cfg(any(test, feature = "test-hooks"))]
fn catalog_fixture_is_lossless_positional_v0(record: &StructMapRelationHandleV1) -> bool {
    record.tt_gc_floor.is_none()
        && record
            .indices
            .values()
            .all(|(child, _)| catalog_fixture_is_lossless_positional_v0(child))
        && record
            .hnsw_indices
            .values()
            .all(|(child, _)| catalog_fixture_is_lossless_positional_v0(child))
        && record
            .fts_indices
            .values()
            .all(|(child, _)| catalog_fixture_is_lossless_positional_v0(child))
        && record.lsh_indices.values().all(|(child, inverse, _)| {
            catalog_fixture_is_lossless_positional_v0(child)
                && catalog_fixture_is_lossless_positional_v0(inverse)
        })
}

#[cfg(any(test, feature = "test-hooks"))]
impl From<&StructMapRelationHandleV1> for PositionalRelationHandleV0 {
    fn from(value: &StructMapRelationHandleV1) -> Self {
        Self {
            name: value.name.clone(),
            id: value.id,
            metadata: value.metadata.clone(),
            put_triggers: value.put_triggers.clone(),
            rm_triggers: value.rm_triggers.clone(),
            replace_triggers: value.replace_triggers.clone(),
            access_level: value.access_level,
            is_temp: value.is_temp,
            indices: value
                .indices
                .iter()
                .map(|(name, (handle, fields))| (name.clone(), (handle.into(), fields.clone())))
                .collect(),
            hnsw_indices: value
                .hnsw_indices
                .iter()
                .map(|(name, (handle, manifest))| (name.clone(), (handle.into(), manifest.clone())))
                .collect(),
            fts_indices: value
                .fts_indices
                .iter()
                .map(|(name, (handle, manifest))| (name.clone(), (handle.into(), manifest.clone())))
                .collect(),
            lsh_indices: value
                .lsh_indices
                .iter()
                .map(|(name, (handle, inverse, manifest))| {
                    (
                        name.clone(),
                        (handle.into(), inverse.into(), manifest.clone()),
                    )
                })
                .collect(),
            description: value.description.clone(),
        }
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[derive(Clone, Copy)]
enum CatalogFixtureNestedKind {
    Normal,
    Hnsw,
    Fts,
}

#[cfg(any(test, feature = "test-hooks"))]
fn fixture_nested_location(
    records: &[UnattestedCatalogRecord],
) -> Option<(usize, CatalogFixtureNestedKind)> {
    records.iter().enumerate().find_map(|(position, record)| {
        if !record.frozen.indices.is_empty() {
            Some((position, CatalogFixtureNestedKind::Normal))
        } else if !record.frozen.hnsw_indices.is_empty() {
            Some((position, CatalogFixtureNestedKind::Hnsw))
        } else if !record.frozen.fts_indices.is_empty() {
            Some((position, CatalogFixtureNestedKind::Fts))
        } else {
            None
        }
    })
}

#[cfg(any(test, feature = "test-hooks"))]
fn fixture_nested_child(
    record: &StructMapRelationHandleV1,
    kind: CatalogFixtureNestedKind,
) -> Option<&StructMapRelationHandleV1> {
    match kind {
        CatalogFixtureNestedKind::Normal => record.indices.values().next().map(|(child, _)| child),
        CatalogFixtureNestedKind::Hnsw => {
            record.hnsw_indices.values().next().map(|(child, _)| child)
        }
        CatalogFixtureNestedKind::Fts => record.fts_indices.values().next().map(|(child, _)| child),
    }
}

#[cfg(any(test, feature = "test-hooks"))]
fn fixture_nested_child_mut(
    record: &mut StructMapRelationHandleV1,
    kind: CatalogFixtureNestedKind,
) -> Option<&mut StructMapRelationHandleV1> {
    match kind {
        CatalogFixtureNestedKind::Normal => {
            record.indices.values_mut().next().map(|(child, _)| child)
        }
        CatalogFixtureNestedKind::Hnsw => record
            .hnsw_indices
            .values_mut()
            .next()
            .map(|(child, _)| child),
        CatalogFixtureNestedKind::Fts => record
            .fts_indices
            .values_mut()
            .next()
            .map(|(child, _)| child),
    }
}

#[cfg(any(test, feature = "test-hooks"))]
fn mutate_nested_fixture_id(
    records: &mut [UnattestedCatalogRecord],
    collide_with_owner: bool,
) -> Result<usize, CatalogCodecError> {
    let (position, kind) =
        fixture_nested_location(records).ok_or(CatalogCodecError::FixturePrecondition)?;
    let owner_id = records[position].frozen.id;
    let child_id = fixture_nested_child(&records[position].frozen, kind)
        .ok_or(CatalogCodecError::FixturePrecondition)?
        .id;
    let replacement = if collide_with_owner {
        owner_id
    } else {
        let maximum = records
            .iter()
            .map(|record| record.frozen.id)
            .max_by_key(|id| id.0)
            .ok_or(CatalogCodecError::FixturePrecondition)?;
        maximum
            .0
            .checked_add(1)
            .filter(|id| *id < (1_u64 << 48))
            .map(WireRelationId)
            .filter(|candidate| *candidate != owner_id && *candidate != child_id)
            .filter(|candidate| records.iter().all(|record| record.frozen.id != *candidate))
            .ok_or(CatalogCodecError::FixturePrecondition)?
    };
    fixture_nested_child_mut(&mut records[position].frozen, kind)
        .ok_or(CatalogCodecError::FixturePrecondition)?
        .id = replacement;
    Ok(position)
}

#[cfg(any(test, feature = "test-hooks"))]
fn mutate_fixture_column_default(
    records: &mut [UnattestedCatalogRecord],
) -> Result<usize, CatalogCodecError> {
    let position = records
        .iter()
        .position(|record| {
            !record.frozen.metadata.keys.is_empty() || !record.frozen.metadata.non_keys.is_empty()
        })
        .ok_or(CatalogCodecError::FixturePrecondition)?;
    let metadata = &mut records[position].frozen.metadata;
    let column = metadata
        .keys
        .first_mut()
        .or_else(|| metadata.non_keys.first_mut())
        .ok_or(CatalogCodecError::FixturePrecondition)?;
    column.default_gen = Some(Expr::Const {
        val: DataValue::Null,
        span: crate::parse::SourceSpan::default(),
    });
    Ok(position)
}

#[cfg(any(test, feature = "test-hooks"))]
fn fixture_leaf_clone(record: &UnattestedCatalogRecord) -> StructMapRelationHandleV1 {
    let mut leaf = record.frozen.clone();
    leaf.put_triggers.clear();
    leaf.rm_triggers.clear();
    leaf.replace_triggers.clear();
    leaf.indices.clear();
    leaf.hnsw_indices.clear();
    leaf.fts_indices.clear();
    leaf.lsh_indices.clear();
    leaf.description.clear();
    leaf.tt_gc_floor = None;
    leaf
}

#[cfg(any(test, feature = "test-hooks"))]
fn mutate_fixture_lsh_index(
    records: &mut [UnattestedCatalogRecord],
) -> Result<(), CatalogCodecError> {
    if records.len() < 3 || !records[0].frozen.lsh_indices.is_empty() {
        return Err(CatalogCodecError::FixturePrecondition);
    }
    let child = fixture_leaf_clone(&records[1]);
    let inverse = fixture_leaf_clone(&records[2]);
    let owner_name = records[0].frozen.name.clone();
    let extractor = records[0]
        .frozen
        .metadata
        .keys
        .first()
        .or_else(|| records[0].frozen.metadata.non_keys.first())
        .map(|column| column.name.clone())
        .ok_or(CatalogCodecError::FixturePrecondition)?;
    records[0].frozen.lsh_indices.insert(
        MANAGED_CATALOG_FIXTURE_SENTINEL_V1.to_owned(),
        (
            child,
            inverse,
            WireMinHashLshIndexManifest {
                base_relation: owner_name,
                index_name: MANAGED_CATALOG_FIXTURE_SENTINEL_V1.to_owned(),
                extractor,
                n_gram: 1,
                tokenizer: WireTokenizerConfig {
                    name: "Simple".to_owned(),
                    args: Vec::new(),
                },
                filters: Vec::new(),
                num_perm: 1,
                n_bands: 1,
                n_rows_in_band: 1,
                threshold: 0.5,
                perms: vec![0],
            },
        ),
    );
    Ok(())
}

fn durable_len(length: usize) -> u64 {
    // All supported Rust targets have a pointer width no greater than 64 bits,
    // and the MessagePack scanner places a much smaller item ceiling on every
    // collection before typed decode. Saturation remains fail-closed if either
    // assumption ever changes.
    u64::try_from(length).unwrap_or(u64::MAX)
}

fn boxed_strings(values: Vec<String>) -> Box<[Box<str>]> {
    values
        .into_iter()
        .map(String::into_boxed_str)
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

impl From<WireAccessLevel> for ManagedAccessLevelV1 {
    fn from(value: WireAccessLevel) -> Self {
        match value {
            WireAccessLevel::Hidden => Self::Hidden,
            WireAccessLevel::ReadOnly => Self::ReadOnly,
            WireAccessLevel::Protected => Self::Protected,
            WireAccessLevel::Normal => Self::Normal,
        }
    }
}

impl From<WireVecElementType> for ManagedVectorElementTypeV1 {
    fn from(value: WireVecElementType) -> Self {
        match value {
            WireVecElementType::F32 => Self::F32,
            WireVecElementType::F64 => Self::F64,
        }
    }
}

impl From<WireHnswDistance> for ManagedHnswDistanceV1 {
    fn from(value: WireHnswDistance) -> Self {
        match value {
            WireHnswDistance::L2 => Self::L2,
            WireHnswDistance::InnerProduct => Self::InnerProduct,
            WireHnswDistance::Cosine => Self::Cosine,
        }
    }
}

impl From<WireNullableColType> for ManagedColumnTypeV1 {
    fn from(value: WireNullableColType) -> Self {
        Self {
            kind: value.coltype.into(),
            nullable: value.nullable,
        }
    }
}

impl From<WireColType> for ManagedColumnTypeKindOwnedV1 {
    fn from(value: WireColType) -> Self {
        match value {
            WireColType::Any => Self::Any,
            WireColType::Bool => Self::Bool,
            WireColType::Int => Self::Int,
            WireColType::Float => Self::Float,
            WireColType::String => Self::String,
            WireColType::Bytes => Self::Bytes,
            WireColType::Uuid => Self::Uuid,
            WireColType::List { eltype, len } => Self::List {
                element: Box::new((*eltype).into()),
                length: len,
            },
            WireColType::Vec { eltype, len } => Self::Vector {
                element: eltype.into(),
                length: len,
            },
            WireColType::Tuple(types) => Self::Tuple(
                types
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ),
            WireColType::Validity => Self::Validity,
            WireColType::TxTime => Self::TxTime,
            WireColType::Json => Self::Json,
        }
    }
}

impl From<WireColumnDef> for ManagedColumnV1 {
    fn from(value: WireColumnDef) -> Self {
        Self {
            name: value.name.into_boxed_str(),
            typing: value.typing.into(),
            default_present: value.default_gen.is_some(),
        }
    }
}

impl From<WireTokenizerConfig> for ManagedTokenizerV1 {
    fn from(value: WireTokenizerConfig) -> Self {
        Self {
            name: value.name.into_boxed_str(),
            argument_count: durable_len(value.args.len()),
        }
    }
}

impl From<WireHnswIndexManifest> for ManagedHnswManifestV1 {
    fn from(value: WireHnswIndexManifest) -> Self {
        Self {
            base_relation: value.base_relation.into_boxed_str(),
            index_name: value.index_name.into_boxed_str(),
            vector_dimension: value.vec_dim,
            dtype: value.dtype.into(),
            vector_fields: value.vec_fields.into_boxed_slice(),
            distance: value.distance.into(),
            ef_construction: value.ef_construction,
            m_neighbours: value.m_neighbours,
            m_max: value.m_max,
            m_max0: value.m_max0,
            level_multiplier_bits: value.level_multiplier.to_bits(),
            index_filter: value.index_filter.map(String::into_boxed_str),
            extend_candidates: value.extend_candidates,
            keep_pruned_connections: value.keep_pruned_connections,
        }
    }
}

impl From<WireFtsIndexManifest> for ManagedFtsManifestV1 {
    fn from(value: WireFtsIndexManifest) -> Self {
        Self {
            base_relation: value.base_relation.into_boxed_str(),
            index_name: value.index_name.into_boxed_str(),
            extractor: value.extractor.into_boxed_str(),
            tokenizer: value.tokenizer.into(),
            filters: value
                .filters
                .into_iter()
                .map(Into::into)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        }
    }
}

impl From<StructMapRelationHandleV1> for ManagedRelationV1 {
    fn from(value: StructMapRelationHandleV1) -> Self {
        let StructMapRelationHandleV1 {
            name,
            id: WireRelationId(id),
            metadata,
            put_triggers,
            rm_triggers,
            replace_triggers,
            access_level,
            is_temp,
            indices,
            hnsw_indices,
            fts_indices,
            lsh_indices,
            description,
            tt_gc_floor,
        } = value;
        let WireStoredRelationMetadata { keys, non_keys } = metadata;
        let lsh_index_count = durable_len(lsh_indices.len());

        Self {
            name: name.into_boxed_str(),
            id,
            keys: keys
                .into_iter()
                .map(Into::into)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            non_keys: non_keys
                .into_iter()
                .map(Into::into)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            put_triggers: boxed_strings(put_triggers),
            remove_triggers: boxed_strings(rm_triggers),
            replace_triggers: boxed_strings(replace_triggers),
            access_level: access_level.into(),
            temporary: is_temp,
            normal_indices: indices
                .into_iter()
                .map(|(name, (relation, fields))| ManagedNormalIndexV1 {
                    name: name.into_boxed_str(),
                    relation: Box::new(relation.into()),
                    fields: fields.into_boxed_slice(),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            hnsw_indices: hnsw_indices
                .into_iter()
                .map(|(name, (relation, manifest))| ManagedHnswIndexV1 {
                    name: name.into_boxed_str(),
                    relation: Box::new(relation.into()),
                    manifest: manifest.into(),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            fts_indices: fts_indices
                .into_iter()
                .map(|(name, (relation, manifest))| ManagedFtsIndexV1 {
                    name: name.into_boxed_str(),
                    relation: Box::new(relation.into()),
                    manifest: manifest.into(),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            lsh_index_count,
            description: description.into_boxed_str(),
            temporal_gc_floor: tt_gc_floor,
        }
    }
}

impl From<PositionalRelationHandleV0> for StructMapRelationHandleV1 {
    fn from(value: PositionalRelationHandleV0) -> Self {
        Self {
            name: value.name,
            id: value.id,
            metadata: value.metadata,
            put_triggers: value.put_triggers,
            rm_triggers: value.rm_triggers,
            replace_triggers: value.replace_triggers,
            access_level: value.access_level,
            is_temp: value.is_temp,
            indices: value
                .indices
                .into_iter()
                .map(|(name, (handle, fields))| (name, (handle.into(), fields)))
                .collect(),
            hnsw_indices: value
                .hnsw_indices
                .into_iter()
                .map(|(name, (handle, manifest))| (name, (handle.into(), manifest)))
                .collect(),
            fts_indices: value
                .fts_indices
                .into_iter()
                .map(|(name, (handle, manifest))| (name, (handle.into(), manifest)))
                .collect(),
            lsh_indices: value
                .lsh_indices
                .into_iter()
                .map(|(name, (handle, inverse, manifest))| {
                    (name, (handle.into(), inverse.into(), manifest))
                })
                .collect(),
            description: value.description,
            tt_gc_floor: None,
        }
    }
}

impl From<PositionalRelationHandleV1> for StructMapRelationHandleV1 {
    fn from(value: PositionalRelationHandleV1) -> Self {
        Self {
            name: value.name,
            id: value.id,
            metadata: value.metadata,
            put_triggers: value.put_triggers,
            rm_triggers: value.rm_triggers,
            replace_triggers: value.replace_triggers,
            access_level: value.access_level,
            is_temp: value.is_temp,
            indices: value
                .indices
                .into_iter()
                .map(|(name, (handle, fields))| (name, (handle.into(), fields)))
                .collect(),
            hnsw_indices: value
                .hnsw_indices
                .into_iter()
                .map(|(name, (handle, manifest))| (name, (handle.into(), manifest)))
                .collect(),
            fts_indices: value
                .fts_indices
                .into_iter()
                .map(|(name, (handle, manifest))| (name, (handle.into(), manifest)))
                .collect(),
            lsh_indices: value
                .lsh_indices
                .into_iter()
                .map(|(name, (handle, inverse, manifest))| {
                    (name, (handle.into(), inverse.into(), manifest))
                })
                .collect(),
            description: value.description,
            tt_gc_floor: value.tt_gc_floor,
        }
    }
}

#[cfg(test)]
impl From<&RelationHandle> for StructMapRelationHandleV1 {
    fn from(value: &RelationHandle) -> Self {
        Self {
            name: value.name.to_string(),
            id: value.id.into(),
            metadata: value.metadata.clone().into(),
            put_triggers: value.put_triggers.clone(),
            rm_triggers: value.rm_triggers.clone(),
            replace_triggers: value.replace_triggers.clone(),
            access_level: value.access_level.into(),
            is_temp: value.is_temp,
            indices: value
                .indices
                .iter()
                .map(|(name, (handle, fields))| {
                    (
                        name.to_string(),
                        (
                            handle.into(),
                            fields.iter().map(|field| *field as u64).collect(),
                        ),
                    )
                })
                .collect(),
            hnsw_indices: value
                .hnsw_indices
                .iter()
                .map(|(name, (handle, manifest))| {
                    (name.to_string(), (handle.into(), manifest.clone().into()))
                })
                .collect(),
            fts_indices: value
                .fts_indices
                .iter()
                .map(|(name, (handle, manifest))| {
                    (name.to_string(), (handle.into(), manifest.clone().into()))
                })
                .collect(),
            lsh_indices: value
                .lsh_indices
                .iter()
                .map(|(name, (handle, inverse, manifest))| {
                    (
                        name.to_string(),
                        (handle.into(), inverse.into(), manifest.clone().into()),
                    )
                })
                .collect(),
            description: value.description.to_string(),
            tt_gc_floor: value.tt_gc_floor,
        }
    }
}

#[cfg(test)]
impl From<&RelationHandle> for PositionalRelationHandleV0 {
    fn from(value: &RelationHandle) -> Self {
        Self {
            name: value.name.to_string(),
            id: value.id.into(),
            metadata: value.metadata.clone().into(),
            put_triggers: value.put_triggers.clone(),
            rm_triggers: value.rm_triggers.clone(),
            replace_triggers: value.replace_triggers.clone(),
            access_level: value.access_level.into(),
            is_temp: value.is_temp,
            indices: value
                .indices
                .iter()
                .map(|(name, (handle, fields))| {
                    (
                        name.to_string(),
                        (
                            handle.into(),
                            fields.iter().map(|field| *field as u64).collect(),
                        ),
                    )
                })
                .collect(),
            hnsw_indices: value
                .hnsw_indices
                .iter()
                .map(|(name, (handle, manifest))| {
                    (name.to_string(), (handle.into(), manifest.clone().into()))
                })
                .collect(),
            fts_indices: value
                .fts_indices
                .iter()
                .map(|(name, (handle, manifest))| {
                    (name.to_string(), (handle.into(), manifest.clone().into()))
                })
                .collect(),
            lsh_indices: value
                .lsh_indices
                .iter()
                .map(|(name, (handle, inverse, manifest))| {
                    (
                        name.to_string(),
                        (handle.into(), inverse.into(), manifest.clone().into()),
                    )
                })
                .collect(),
            description: value.description.to_string(),
        }
    }
}

#[cfg(test)]
impl From<&RelationHandle> for PositionalRelationHandleV1 {
    fn from(value: &RelationHandle) -> Self {
        Self {
            name: value.name.to_string(),
            id: value.id.into(),
            metadata: value.metadata.clone().into(),
            put_triggers: value.put_triggers.clone(),
            rm_triggers: value.rm_triggers.clone(),
            replace_triggers: value.replace_triggers.clone(),
            access_level: value.access_level.into(),
            is_temp: value.is_temp,
            indices: value
                .indices
                .iter()
                .map(|(name, (handle, fields))| {
                    (
                        name.to_string(),
                        (
                            handle.into(),
                            fields.iter().map(|field| *field as u64).collect(),
                        ),
                    )
                })
                .collect(),
            hnsw_indices: value
                .hnsw_indices
                .iter()
                .map(|(name, (handle, manifest))| {
                    (name.to_string(), (handle.into(), manifest.clone().into()))
                })
                .collect(),
            fts_indices: value
                .fts_indices
                .iter()
                .map(|(name, (handle, manifest))| {
                    (name.to_string(), (handle.into(), manifest.clone().into()))
                })
                .collect(),
            lsh_indices: value
                .lsh_indices
                .iter()
                .map(|(name, (handle, inverse, manifest))| {
                    (
                        name.to_string(),
                        (handle.into(), inverse.into(), manifest.clone().into()),
                    )
                })
                .collect(),
            description: value.description.to_string(),
            tt_gc_floor: value.tt_gc_floor,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use serde::ser::SerializeMap;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::runtime::relation::catalog_compat_tests::LEGACY_EDGE_CATALOG;

    #[derive(Debug, Eq, PartialEq)]
    struct TypeSnapshot {
        nullable: bool,
        kind: TypeKindSnapshot,
    }

    #[derive(Debug, Eq, PartialEq)]
    enum TypeKindSnapshot {
        Any,
        Bool,
        Int,
        Float,
        String,
        Bytes,
        Uuid,
        List {
            element: Box<TypeSnapshot>,
            length: Option<u64>,
        },
        Vector {
            element: ManagedVectorElementTypeV1,
            length: u64,
        },
        Tuple(Vec<TypeSnapshot>),
        Validity,
        TxTime,
        Json,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct ColumnSnapshot {
        name: String,
        typing: TypeSnapshot,
        default_present: bool,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct HnswManifestSnapshot {
        base_relation: String,
        index_name: String,
        vector_dimension: u64,
        dtype: ManagedVectorElementTypeV1,
        vector_fields: Vec<u64>,
        distance: ManagedHnswDistanceV1,
        ef_construction: u64,
        m_neighbours: u64,
        m_max: u64,
        m_max0: u64,
        level_multiplier_bits: u64,
        index_filter: Option<String>,
        extend_candidates: bool,
        keep_pruned_connections: bool,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct TokenizerSnapshot {
        name: String,
        argument_count: u64,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct FtsManifestSnapshot {
        base_relation: String,
        index_name: String,
        extractor: String,
        tokenizer: TokenizerSnapshot,
        filters: Vec<TokenizerSnapshot>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct RelationSnapshot {
        name: String,
        id: u64,
        keys: Vec<ColumnSnapshot>,
        non_keys: Vec<ColumnSnapshot>,
        put_triggers: Vec<String>,
        remove_triggers: Vec<String>,
        replace_triggers: Vec<String>,
        access_level: ManagedAccessLevelV1,
        temporary: bool,
        normal_indices: Vec<(String, Box<RelationSnapshot>, Vec<u64>)>,
        hnsw_indices: Vec<(String, Box<RelationSnapshot>, HnswManifestSnapshot)>,
        fts_indices: Vec<(String, Box<RelationSnapshot>, FtsManifestSnapshot)>,
        lsh_index_count: u64,
        description: String,
        temporal_gc_floor: Option<i64>,
    }

    fn snapshot_type(value: &ManagedColumnTypeV1) -> TypeSnapshot {
        let kind = match value.kind() {
            ManagedColumnTypeKindV1::Any => TypeKindSnapshot::Any,
            ManagedColumnTypeKindV1::Bool => TypeKindSnapshot::Bool,
            ManagedColumnTypeKindV1::Int => TypeKindSnapshot::Int,
            ManagedColumnTypeKindV1::Float => TypeKindSnapshot::Float,
            ManagedColumnTypeKindV1::String => TypeKindSnapshot::String,
            ManagedColumnTypeKindV1::Bytes => TypeKindSnapshot::Bytes,
            ManagedColumnTypeKindV1::Uuid => TypeKindSnapshot::Uuid,
            ManagedColumnTypeKindV1::List { element, length } => TypeKindSnapshot::List {
                element: Box::new(snapshot_type(element)),
                length,
            },
            ManagedColumnTypeKindV1::Vector { element, length } => {
                TypeKindSnapshot::Vector { element, length }
            }
            ManagedColumnTypeKindV1::Tuple(elements) => {
                TypeKindSnapshot::Tuple(elements.iter().map(snapshot_type).collect())
            }
            ManagedColumnTypeKindV1::Validity => TypeKindSnapshot::Validity,
            ManagedColumnTypeKindV1::TxTime => TypeKindSnapshot::TxTime,
            ManagedColumnTypeKindV1::Json => TypeKindSnapshot::Json,
        };
        TypeSnapshot {
            nullable: value.nullable(),
            kind,
        }
    }

    fn snapshot_column(value: &ManagedColumnV1) -> ColumnSnapshot {
        ColumnSnapshot {
            name: value.name().to_owned(),
            typing: snapshot_type(value.typing()),
            default_present: value.default_present(),
        }
    }

    fn snapshot_tokenizer(value: &ManagedTokenizerV1) -> TokenizerSnapshot {
        TokenizerSnapshot {
            name: value.name().to_owned(),
            argument_count: value.argument_count(),
        }
    }

    fn snapshot_relation(value: &ManagedRelationV1) -> RelationSnapshot {
        RelationSnapshot {
            name: value.name().to_owned(),
            id: value.id(),
            keys: value.keys().iter().map(snapshot_column).collect(),
            non_keys: value.non_keys().iter().map(snapshot_column).collect(),
            put_triggers: value.put_triggers().map(str::to_owned).collect(),
            remove_triggers: value.remove_triggers().map(str::to_owned).collect(),
            replace_triggers: value.replace_triggers().map(str::to_owned).collect(),
            access_level: value.access_level(),
            temporary: value.is_temporary(),
            normal_indices: value
                .normal_indices()
                .iter()
                .map(|index| {
                    (
                        index.name().to_owned(),
                        Box::new(snapshot_relation(index.relation())),
                        index.fields().to_vec(),
                    )
                })
                .collect(),
            hnsw_indices: value
                .hnsw_indices()
                .iter()
                .map(|index| {
                    let manifest = index.manifest();
                    (
                        index.name().to_owned(),
                        Box::new(snapshot_relation(index.relation())),
                        HnswManifestSnapshot {
                            base_relation: manifest.base_relation().to_owned(),
                            index_name: manifest.index_name().to_owned(),
                            vector_dimension: manifest.vector_dimension(),
                            dtype: manifest.dtype(),
                            vector_fields: manifest.vector_fields().to_vec(),
                            distance: manifest.distance(),
                            ef_construction: manifest.ef_construction(),
                            m_neighbours: manifest.m_neighbours(),
                            m_max: manifest.m_max(),
                            m_max0: manifest.m_max0(),
                            level_multiplier_bits: manifest.level_multiplier_bits(),
                            index_filter: manifest.index_filter().map(str::to_owned),
                            extend_candidates: manifest.extend_candidates(),
                            keep_pruned_connections: manifest.keep_pruned_connections(),
                        },
                    )
                })
                .collect(),
            fts_indices: value
                .fts_indices()
                .iter()
                .map(|index| {
                    let manifest = index.manifest();
                    (
                        index.name().to_owned(),
                        Box::new(snapshot_relation(index.relation())),
                        FtsManifestSnapshot {
                            base_relation: manifest.base_relation().to_owned(),
                            index_name: manifest.index_name().to_owned(),
                            extractor: manifest.extractor().to_owned(),
                            tokenizer: snapshot_tokenizer(manifest.tokenizer()),
                            filters: manifest.filters().iter().map(snapshot_tokenizer).collect(),
                        },
                    )
                })
                .collect(),
            lsh_index_count: value.lsh_index_count(),
            description: value.description().to_owned(),
            temporal_gc_floor: value.temporal_gc_floor(),
        }
    }

    fn metadata() -> StoredRelationMetadata {
        StoredRelationMetadata {
            keys: vec![ColumnDef {
                name: "id".into(),
                typing: NullableColType {
                    coltype: ColType::Int,
                    nullable: false,
                },
                default_gen: None,
            }],
            non_keys: vec![
                ColumnDef {
                    name: "text".into(),
                    typing: NullableColType {
                        coltype: ColType::String,
                        nullable: true,
                    },
                    default_gen: None,
                },
                ColumnDef {
                    name: "embedding".into(),
                    typing: NullableColType {
                        coltype: ColType::Vec {
                            eltype: VecElementType::F32,
                            len: 3,
                        },
                        nullable: false,
                    },
                    default_gen: None,
                },
            ],
        }
    }

    fn leaf(name: &str, id: u64) -> RelationHandle {
        RelationHandle {
            name: name.into(),
            id: RelationId::new(id),
            metadata: metadata(),
            put_triggers: Vec::new(),
            rm_triggers: Vec::new(),
            replace_triggers: Vec::new(),
            access_level: AccessLevel::Normal,
            is_temp: false,
            indices: BTreeMap::new(),
            hnsw_indices: BTreeMap::new(),
            fts_indices: BTreeMap::new(),
            lsh_indices: BTreeMap::new(),
            description: format!("fixture {name}").into(),
            tt_gc_floor: None,
        }
    }

    fn minimal_handle() -> RelationHandle {
        let mut handle = leaf("r", 1);
        handle.metadata = StoredRelationMetadata {
            keys: Vec::new(),
            non_keys: Vec::new(),
        };
        handle.description = "".into();
        handle
    }

    fn rich_handle() -> RelationHandle {
        let mut handle = leaf("catalog-fixture-root", 100);
        handle.put_triggers.push("?[x] := *src{x}".to_owned());
        handle.description = "normal, vector, text, and LSH children".into();

        handle.indices.insert(
            "normal_fixture".into(),
            (leaf("catalog-fixture-normal-child", 101), vec![0, 2]),
        );

        handle.hnsw_indices.insert(
            "hnsw_fixture".into(),
            (
                leaf("catalog-fixture-hnsw-child", 102),
                HnswIndexManifest {
                    base_relation: "catalog-fixture-root".into(),
                    index_name: "hnsw_fixture".into(),
                    vec_dim: 3,
                    dtype: VecElementType::F32,
                    vec_fields: vec![2],
                    distance: HnswDistance::Cosine,
                    ef_construction: 32,
                    m_neighbours: 8,
                    m_max: 16,
                    m_max0: 32,
                    level_multiplier: 0.5,
                    index_filter: Some("text != null".to_owned()),
                    extend_candidates: true,
                    keep_pruned_connections: false,
                },
            ),
        );

        let tokenizer = TokenizerConfig {
            name: "Simple".into(),
            // F7 admits no tokenizer arguments. Keep this exact leaf pinned.
            args: Vec::new(),
        };
        let filters = vec![TokenizerConfig {
            name: "Lowercase".into(),
            args: Vec::new(),
        }];
        handle.fts_indices.insert(
            "fts_fixture".into(),
            (
                leaf("catalog-fixture-fts-child", 103),
                FtsIndexManifest {
                    base_relation: "catalog-fixture-root".into(),
                    index_name: "fts_fixture".into(),
                    extractor: "text".to_owned(),
                    tokenizer: tokenizer.clone(),
                    filters: filters.clone(),
                },
            ),
        );

        handle.lsh_indices.insert(
            "lsh_fixture".into(),
            (
                leaf("catalog-fixture-lsh-child", 104),
                leaf("catalog-fixture-lsh-inverse-child", 105),
                MinHashLshIndexManifest {
                    base_relation: "catalog-fixture-root".into(),
                    index_name: "lsh_fixture".into(),
                    extractor: "text".to_owned(),
                    n_gram: 3,
                    tokenizer,
                    filters,
                    num_perm: 16,
                    n_bands: 4,
                    n_rows_in_band: 4,
                    threshold: 0.7,
                    perms: vec![0, 1, 2, 3, 4, 5, 6, 7],
                },
            ),
        );
        handle
    }

    fn rich_projection_handle() -> RelationHandle {
        let mut handle = rich_handle();
        handle.rm_triggers.push("?[x] := *old{x}".to_owned());
        handle
            .replace_triggers
            .push("?[x] := *replacement{x}".to_owned());
        handle.metadata.non_keys[0].default_gen = Some(Expr::Const {
            val: DataValue::Null,
            span: crate::parse::SourceSpan::default(),
        });
        handle.metadata.non_keys.push(ColumnDef {
            name: "bounded_list".into(),
            typing: NullableColType {
                coltype: ColType::List {
                    eltype: Box::new(NullableColType {
                        coltype: ColType::Int,
                        nullable: true,
                    }),
                    len: Some(7),
                },
                nullable: false,
            },
            default_gen: None,
        });

        let normal_child = &mut handle
            .indices
            .get_mut("normal_fixture")
            .expect("normal fixture")
            .0;
        normal_child.access_level = AccessLevel::ReadOnly;
        normal_child.is_temp = true;
        normal_child
            .put_triggers
            .push("?[id] := *child{id}".to_owned());

        let fts = &mut handle
            .fts_indices
            .get_mut("fts_fixture")
            .expect("FTS fixture")
            .1;
        fts.tokenizer.args = vec![DataValue::Null, DataValue::from(7_i64)];
        fts.filters[0].args = vec![DataValue::Bool(true)];
        handle
    }

    fn complete_f7_leaf_vocabulary_handle() -> RelationHandle {
        let column = |name: &str, coltype, nullable| ColumnDef {
            name: name.into(),
            typing: NullableColType { coltype, nullable },
            // Defaults are not part of the admitted F7 catalog.
            default_gen: None,
        };
        RelationHandle {
            name: "f7-leaf-vocabulary".into(),
            id: RelationId::new(300),
            metadata: StoredRelationMetadata {
                keys: vec![
                    column("string_key", ColType::String, false),
                    column("int_key", ColType::Int, false),
                ],
                non_keys: vec![
                    column("nullable_string", ColType::String, true),
                    column("bool_value", ColType::Bool, false),
                    column("float_value", ColType::Float, false),
                    column(
                        "offsets",
                        ColType::List {
                            eltype: Box::new(NullableColType {
                                coltype: ColType::Int,
                                nullable: false,
                            }),
                            len: None,
                        },
                        false,
                    ),
                    column("hash", ColType::Bytes, true),
                    column(
                        "embedding",
                        ColType::Vec {
                            eltype: VecElementType::F32,
                            len: 768,
                        },
                        false,
                    ),
                ],
            },
            put_triggers: Vec::new(),
            rm_triggers: Vec::new(),
            replace_triggers: Vec::new(),
            access_level: AccessLevel::ReadOnly,
            is_temp: false,
            indices: BTreeMap::new(),
            hnsw_indices: BTreeMap::new(),
            fts_indices: BTreeMap::new(),
            lsh_indices: BTreeMap::new(),
            description: "complete F7 column and access leaf vocabulary".into(),
            tt_gc_floor: None,
        }
    }

    fn normal_parent() -> RelationHandle {
        let mut handle = leaf("mixed-codec-parent", 200);
        handle.indices.insert(
            "only_child".into(),
            (leaf("unique-mixed-codec-child", 201), vec![0]),
        );
        handle
    }

    fn encode_compact<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
        let mut encoded = Vec::new();
        value
            .serialize(&mut Serializer::new(&mut encoded))
            .expect("trusted test fixture must serialize");
        encoded
    }

    fn encode_struct_map<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
        let mut encoded = Vec::new();
        value
            .serialize(&mut Serializer::new(&mut encoded).with_struct_map())
            .expect("trusted test fixture must serialize");
        encoded
    }

    fn assert_exact_fingerprint(encoded: &[u8], expected_len: usize, expected_digest: [u8; 32]) {
        let digest: [u8; 32] = Sha256::digest(encoded).into();
        assert_eq!(encoded.len(), expected_len);
        assert_eq!(digest, expected_digest);
    }

    fn classify(encoded: &[u8]) -> Result<(CatalogCodec, Vec<u8>), CatalogCodecError> {
        let record = decode_unattested_catalog(encoded)?;
        Ok((record.codec(), record.canonical_struct_map()?))
    }

    fn assert_rejected(encoded: &[u8]) {
        assert!(
            decode_unattested_catalog(encoded).is_err(),
            "malformed catalog was unexpectedly accepted"
        );
    }

    fn assert_envelope_kind(encoded: &[u8], expected: StoredMsgpackErrorKind) {
        match decode_unattested_catalog(encoded) {
            Err(CatalogCodecError::Envelope(error)) => assert_eq!(error.kind(), expected),
            Err(error) => panic!("expected envelope {expected:?}, got {error}"),
            Ok(_) => panic!("expected envelope {expected:?}, got success"),
        }
    }

    fn replace_once(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
        let positions: Vec<_> = haystack
            .windows(needle.len())
            .enumerate()
            .filter_map(|(position, candidate)| (candidate == needle).then_some(position))
            .collect();
        assert_eq!(positions.len(), 1, "fixture child encoding must be unique");
        let position = positions[0];
        let mut output = Vec::with_capacity(haystack.len() - needle.len() + replacement.len());
        output.extend_from_slice(&haystack[..position]);
        output.extend_from_slice(replacement);
        output.extend_from_slice(&haystack[position + needle.len()..]);
        output
    }

    #[derive(Clone, Copy)]
    enum MapMutation {
        Reordered,
        Duplicate,
        Unknown,
    }

    struct MutatedRootMap<'a> {
        record: &'a StructMapRelationHandleV1,
        mutation: MapMutation,
    }

    impl Serialize for MutatedRootMap<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let extra = usize::from(matches!(
                self.mutation,
                MapMutation::Duplicate | MapMutation::Unknown
            ));
            let mut map = serializer.serialize_map(Some(14 + extra))?;
            if matches!(self.mutation, MapMutation::Reordered) {
                map.serialize_entry("id", &self.record.id)?;
                map.serialize_entry("name", &self.record.name)?;
            } else {
                map.serialize_entry("name", &self.record.name)?;
                map.serialize_entry("id", &self.record.id)?;
            }
            map.serialize_entry("metadata", &self.record.metadata)?;
            map.serialize_entry("put_triggers", &self.record.put_triggers)?;
            map.serialize_entry("rm_triggers", &self.record.rm_triggers)?;
            map.serialize_entry("replace_triggers", &self.record.replace_triggers)?;
            map.serialize_entry("access_level", &self.record.access_level)?;
            map.serialize_entry("is_temp", &self.record.is_temp)?;
            map.serialize_entry("indices", &self.record.indices)?;
            map.serialize_entry("hnsw_indices", &self.record.hnsw_indices)?;
            map.serialize_entry("fts_indices", &self.record.fts_indices)?;
            map.serialize_entry("lsh_indices", &self.record.lsh_indices)?;
            map.serialize_entry("description", &self.record.description)?;
            map.serialize_entry("tt_gc_floor", &self.record.tt_gc_floor)?;
            match self.mutation {
                MapMutation::Duplicate => {
                    map.serialize_entry("name", &self.record.name)?;
                }
                MapMutation::Unknown => {
                    map.serialize_entry("future_catalog_field", &false)?;
                }
                MapMutation::Reordered => {}
            }
            map.end()
        }
    }

    #[test]
    fn exact_minimal_positional_bytes_are_pinned() {
        const POSITIONAL_V0: &[u8] = &[
            0x9d, 0xa1, b'r', 0x01, 0x92, 0x90, 0x90, 0x90, 0x90, 0x90, 0xa6, b'N', b'o', b'r',
            b'm', b'a', b'l', 0xc2, 0x80, 0x80, 0x80, 0x80, 0xa0,
        ];
        const POSITIONAL_V1: &[u8] = &[
            0x9e, 0xa1, b'r', 0x01, 0x92, 0x90, 0x90, 0x90, 0x90, 0x90, 0xa6, b'N', b'o', b'r',
            b'm', b'a', b'l', 0xc2, 0x80, 0x80, 0x80, 0x80, 0xa0, 0xc0,
        ];

        let handle = minimal_handle();
        assert_eq!(
            encode_compact(&PositionalRelationHandleV0::from(&handle)),
            POSITIONAL_V0
        );
        assert_eq!(
            encode_compact(&PositionalRelationHandleV1::from(&handle)),
            POSITIONAL_V1
        );
        // Pin the live writer too: drift here cannot silently redefine the
        // frozen historical protocol.
        assert_eq!(encode_compact(&handle), POSITIONAL_V1);

        assert_eq!(
            decode_unattested_catalog(POSITIONAL_V0)
                .expect("literal v0 fixture must classify")
                .codec(),
            CatalogCodec::PositionalV0
        );
        assert_eq!(
            decode_unattested_catalog(POSITIONAL_V1)
                .expect("literal v1 fixture must classify")
                .codec(),
            CatalogCodec::PositionalV1
        );
    }

    #[test]
    fn all_exact_codecs_converge_to_one_frozen_map() {
        let handle = rich_handle();
        let expected = StructMapRelationHandleV1::from(&handle);
        let native_map = encode_struct_map(&handle);
        assert_eq!(native_map, encode_struct_map(&expected));

        let fixtures = [
            (
                CatalogCodec::PositionalV0,
                encode_compact(&PositionalRelationHandleV0::from(&handle)),
            ),
            (
                CatalogCodec::PositionalV1,
                encode_compact(&PositionalRelationHandleV1::from(&handle)),
            ),
            (CatalogCodec::StructMapV1, native_map.clone()),
        ];
        for (expected_codec, encoded) in fixtures {
            let record = decode_unattested_catalog(&encoded).expect("exact fixture must classify");
            assert_eq!(record.codec(), expected_codec);
            assert!(record.frozen == expected);
            let canonical = record
                .canonical_struct_map()
                .expect("scan-proven fixture must canonicalize");
            assert_eq!(canonical, native_map);
            let scanned = scan_exact_envelope(&canonical, StoredMsgpackProfile::RelationCatalog)
                .expect("canonical writer must post-scan its output");
            assert_eq!(scanned.root(), StoredMsgpackRoot::Map);
        }
    }

    #[test]
    fn all_exact_codecs_project_to_one_sanitized_recursive_relation() {
        let handle = rich_projection_handle();
        let fixtures = [
            (
                ManagedCatalogEncodingV1::PositionalV0,
                encode_compact(&PositionalRelationHandleV0::from(&handle)),
            ),
            (
                ManagedCatalogEncodingV1::PositionalV1,
                encode_compact(&PositionalRelationHandleV1::from(&handle)),
            ),
            (
                ManagedCatalogEncodingV1::StructMapV1,
                encode_struct_map(&StructMapRelationHandleV1::from(&handle)),
            ),
        ];
        let mut snapshots = Vec::new();
        for (expected_encoding, encoded) in fixtures {
            let entry = decode_unattested_catalog(&encoded)
                .expect("exact fixture must classify")
                .into_managed_entry_v1();
            assert_eq!(entry.encoding(), expected_encoding);
            assert_eq!(
                entry.encoding().is_struct_map_v1(),
                expected_encoding == ManagedCatalogEncodingV1::StructMapV1
            );
            snapshots.push(snapshot_relation(entry.relation()));
        }
        assert_eq!(snapshots[0], snapshots[1]);
        assert_eq!(snapshots[1], snapshots[2]);

        let root = &snapshots[0];
        assert_eq!(root.name, "catalog-fixture-root");
        assert_eq!(root.id, 100);
        assert_eq!(root.put_triggers, ["?[x] := *src{x}"]);
        assert_eq!(root.remove_triggers, ["?[x] := *old{x}"]);
        assert_eq!(root.replace_triggers, ["?[x] := *replacement{x}"]);
        assert_eq!(root.access_level, ManagedAccessLevelV1::Normal);
        assert!(!root.temporary);
        assert_eq!(root.description, "normal, vector, text, and LSH children");
        assert_eq!(root.temporal_gc_floor, None);
        assert_eq!(root.lsh_index_count, 1);

        let text = root
            .non_keys
            .iter()
            .find(|column| column.name == "text")
            .expect("text column");
        assert!(text.default_present);
        assert_eq!(
            text.typing,
            TypeSnapshot {
                nullable: true,
                kind: TypeKindSnapshot::String,
            }
        );
        let vector = root
            .non_keys
            .iter()
            .find(|column| column.name == "embedding")
            .expect("vector column");
        assert_eq!(
            vector.typing,
            TypeSnapshot {
                nullable: false,
                kind: TypeKindSnapshot::Vector {
                    element: ManagedVectorElementTypeV1::F32,
                    length: 3,
                },
            }
        );
        let list = root
            .non_keys
            .iter()
            .find(|column| column.name == "bounded_list")
            .expect("list column");
        assert_eq!(
            list.typing,
            TypeSnapshot {
                nullable: false,
                kind: TypeKindSnapshot::List {
                    element: Box::new(TypeSnapshot {
                        nullable: true,
                        kind: TypeKindSnapshot::Int,
                    }),
                    length: Some(7),
                },
            }
        );

        let (normal_name, normal_child, fields) = &root.normal_indices[0];
        assert_eq!(normal_name, "normal_fixture");
        assert_eq!(fields, &[0, 2]);
        assert_eq!(normal_child.name, "catalog-fixture-normal-child");
        assert_eq!(normal_child.id, 101);
        assert_eq!(normal_child.access_level, ManagedAccessLevelV1::ReadOnly);
        assert!(normal_child.temporary);
        assert_eq!(normal_child.put_triggers, ["?[id] := *child{id}"]);

        let (hnsw_name, hnsw_child, hnsw) = &root.hnsw_indices[0];
        assert_eq!(hnsw_name, "hnsw_fixture");
        assert_eq!(hnsw_child.name, "catalog-fixture-hnsw-child");
        assert_eq!(hnsw_child.id, 102);
        assert_eq!(hnsw.base_relation, "catalog-fixture-root");
        assert_eq!(hnsw.index_name, "hnsw_fixture");
        assert_eq!(hnsw.vector_dimension, 3);
        assert_eq!(hnsw.dtype, ManagedVectorElementTypeV1::F32);
        assert_eq!(hnsw.vector_fields, [2]);
        assert_eq!(hnsw.distance, ManagedHnswDistanceV1::Cosine);
        assert_eq!(hnsw.ef_construction, 32);
        assert_eq!(hnsw.m_neighbours, 8);
        assert_eq!(hnsw.m_max, 16);
        assert_eq!(hnsw.m_max0, 32);
        assert_eq!(hnsw.level_multiplier_bits, 0.5_f64.to_bits());
        assert_eq!(hnsw.index_filter.as_deref(), Some("text != null"));
        assert!(hnsw.extend_candidates);
        assert!(!hnsw.keep_pruned_connections);

        let (fts_name, fts_child, fts) = &root.fts_indices[0];
        assert_eq!(fts_name, "fts_fixture");
        assert_eq!(fts_child.name, "catalog-fixture-fts-child");
        assert_eq!(fts_child.id, 103);
        assert_eq!(fts.base_relation, "catalog-fixture-root");
        assert_eq!(fts.index_name, "fts_fixture");
        assert_eq!(fts.extractor, "text");
        assert_eq!(
            fts.tokenizer,
            TokenizerSnapshot {
                name: "Simple".to_owned(),
                argument_count: 2,
            }
        );
        assert_eq!(
            fts.filters,
            [TokenizerSnapshot {
                name: "Lowercase".to_owned(),
                argument_count: 1,
            }]
        );
    }

    #[test]
    fn managed_projection_preserves_v1_temporal_floors_and_v0_omission_recursively() {
        let mut handle = rich_projection_handle();
        handle.tt_gc_floor = Some(-7);
        handle
            .indices
            .get_mut("normal_fixture")
            .expect("normal fixture")
            .0
            .tt_gc_floor = Some(-8);

        let v0 =
            decode_unattested_catalog(&encode_compact(&PositionalRelationHandleV0::from(&handle)))
                .expect("v0 fixture")
                .into_managed_entry_v1();
        assert_eq!(v0.relation().temporal_gc_floor(), None);
        assert_eq!(
            v0.relation().normal_indices()[0]
                .relation()
                .temporal_gc_floor(),
            None
        );

        for encoded in [
            encode_compact(&PositionalRelationHandleV1::from(&handle)),
            encode_struct_map(&StructMapRelationHandleV1::from(&handle)),
        ] {
            let entry = decode_unattested_catalog(&encoded)
                .expect("v1 fixture")
                .into_managed_entry_v1();
            assert_eq!(entry.relation().temporal_gc_floor(), Some(-7));
            assert_eq!(
                entry.relation().normal_indices()[0]
                    .relation()
                    .temporal_gc_floor(),
                Some(-8)
            );
        }
    }

    #[test]
    fn managed_catalog_payloads_are_send_sync_and_borrow_only() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ManagedCatalogCensusV1>();
        assert_send_sync::<ManagedCatalogEntryV1>();
        assert_send_sync::<ManagedRelationV1>();

        let encoded = encode_struct_map(&StructMapRelationHandleV1::from(&minimal_handle()));
        let entry = decode_unattested_catalog(&encoded)
            .expect("minimal map fixture")
            .into_managed_entry_v1();
        let census = ManagedCatalogCensusV1::from_entries(vec![entry]);
        assert_eq!(census.len(), 1);
        assert!(!census.is_empty());
        assert_eq!(census.entries()[0].relation().name(), "r");
    }

    #[test]
    fn rich_native_map_generation_has_a_fixed_fingerprint() {
        let encoded = encode_struct_map(&rich_handle());
        let digest: [u8; 32] = Sha256::digest(&encoded).into();
        assert_eq!(encoded.len(), 3_281);
        assert_eq!(
            digest,
            [
                121, 76, 169, 58, 79, 57, 213, 48, 46, 1, 79, 155, 119, 47, 168, 126, 36, 241, 37,
                46, 255, 182, 46, 209, 57, 216, 151, 248, 133, 52, 150, 133,
            ]
        );
    }

    #[test]
    fn rich_recursive_positional_generation_has_fixed_fingerprints() {
        let handle = rich_handle();
        assert_exact_fingerprint(
            &encode_compact(&PositionalRelationHandleV0::from(&handle)),
            1_119,
            [
                222, 234, 198, 205, 52, 43, 158, 0, 126, 166, 158, 20, 233, 189, 91, 102, 61, 115,
                239, 9, 231, 75, 162, 163, 241, 112, 178, 22, 33, 74, 217, 232,
            ],
        );
        assert_exact_fingerprint(
            &encode_compact(&PositionalRelationHandleV1::from(&handle)),
            1_125,
            [
                79, 200, 151, 146, 95, 168, 76, 93, 72, 42, 151, 102, 63, 134, 139, 227, 148, 30,
                238, 17, 178, 152, 198, 8, 217, 124, 229, 97, 116, 173, 157, 255,
            ],
        );
    }

    #[test]
    fn complete_f7_leaf_map_generation_has_a_fixed_fingerprint() {
        // This is the complete G2-admitted column/access leaf vocabulary:
        // ReadOnly; String and Int keys; nullable String and Bytes; Bool;
        // Float; unbounded List<Int>; and a fixed F32 vector. Other decoded
        // wire variants remain G1a syntax compatibility only and G2 rejects
        // them semantically.
        let handle = complete_f7_leaf_vocabulary_handle();
        let frozen = StructMapRelationHandleV1::from(&handle);
        let encoded = encode_struct_map(&handle);
        assert_eq!(encoded, encode_struct_map(&frozen));

        let record =
            decode_unattested_catalog(&encoded).expect("complete F7 leaf fixture must classify");
        assert_eq!(record.codec(), CatalogCodec::StructMapV1);
        assert!(record.frozen == frozen);
        assert_eq!(
            record
                .canonical_struct_map()
                .expect("complete F7 leaf fixture must remain canonical"),
            encoded
        );

        let digest: [u8; 32] = Sha256::digest(&encoded).into();
        assert_eq!(encoded.len(), 796);
        assert_eq!(
            digest,
            [
                114, 205, 77, 82, 109, 109, 116, 37, 180, 208, 241, 209, 95, 149, 212, 194, 152,
                113, 197, 186, 79, 83, 77, 78, 162, 195, 117, 84, 225, 83, 112, 210,
            ]
        );
    }

    #[test]
    fn complete_f7_leaf_positional_generation_has_fixed_fingerprints() {
        let handle = complete_f7_leaf_vocabulary_handle();
        assert_exact_fingerprint(
            &encode_compact(&PositionalRelationHandleV0::from(&handle)),
            265,
            [
                209, 187, 86, 191, 236, 152, 81, 97, 176, 63, 193, 144, 178, 160, 200, 57, 49, 217,
                109, 104, 64, 153, 126, 216, 48, 208, 73, 164, 14, 225, 64, 230,
            ],
        );
        assert_exact_fingerprint(
            &encode_compact(&PositionalRelationHandleV1::from(&handle)),
            266,
            [
                1, 76, 101, 6, 51, 129, 86, 255, 92, 235, 147, 49, 230, 162, 76, 230, 210, 206,
                106, 50, 186, 173, 102, 230, 238, 13, 147, 197, 222, 174, 2, 174,
            ],
        );
    }

    #[test]
    fn real_legacy_nested_catalog_normalizes_without_round_trip_drift() {
        let record = decode_unattested_catalog(LEGACY_EDGE_CATALOG)
            .expect("real 13-field production capture must classify");
        assert_eq!(record.codec(), CatalogCodec::PositionalV0);
        assert!(record.frozen.tt_gc_floor.is_none());
        assert!(record.frozen.indices.contains_key("from_idx"));

        let native = RelationHandle::decode(LEGACY_EDGE_CATALOG)
            .expect("the existing compatibility decoder must accept the capture");
        let expected = StructMapRelationHandleV1::from(&native);
        assert!(record.frozen == expected);
        assert_eq!(
            record
                .canonical_struct_map()
                .expect("legacy capture must normalize"),
            encode_struct_map(&native)
        );
    }

    #[test]
    fn temporal_floor_is_omitted_only_by_recursive_v0() {
        let mut handle = minimal_handle();
        handle.tt_gc_floor = Some(-7);

        let positional_v0 =
            decode_unattested_catalog(&encode_compact(&PositionalRelationHandleV0::from(&handle)))
                .expect("v0 fixture must classify");
        assert!(positional_v0.frozen.tt_gc_floor.is_none());

        for encoded in [
            encode_compact(&PositionalRelationHandleV1::from(&handle)),
            encode_struct_map(&StructMapRelationHandleV1::from(&handle)),
        ] {
            let record = decode_unattested_catalog(&encoded).expect("v1 fixture must classify");
            assert_eq!(record.frozen.tt_gc_floor, Some(-7));
        }
    }

    #[test]
    fn map_keys_must_be_exact_unique_and_canonically_ordered() {
        let frozen = StructMapRelationHandleV1::from(&minimal_handle());
        for mutation in [
            MapMutation::Reordered,
            MapMutation::Duplicate,
            MapMutation::Unknown,
        ] {
            assert_rejected(&encode_struct_map(&MutatedRootMap {
                record: &frozen,
                mutation,
            }));
        }

        let canonical = encode_struct_map(&frozen);
        assert_eq!(canonical[0], 0x8e, "14 fields must use fixmap");
        let mut nonminimal_map = Vec::with_capacity(canonical.len() + 2);
        nonminimal_map.extend_from_slice(&[0xde, 0x00, 0x0e]);
        nonminimal_map.extend_from_slice(&canonical[1..]);
        assert_rejected(&nonminimal_map);

        let compact = encode_compact(&PositionalRelationHandleV1::from(&minimal_handle()));
        assert_eq!(compact[3], 0x01, "fixture id must be a positive fixint");
        let mut nonminimal_integer = Vec::with_capacity(compact.len() + 1);
        nonminimal_integer.extend_from_slice(&compact[..3]);
        nonminimal_integer.extend_from_slice(&[0xcc, 0x01]);
        nonminimal_integer.extend_from_slice(&compact[4..]);
        assert_rejected(&nonminimal_integer);
    }

    #[test]
    fn trailing_bytes_and_wrong_roots_are_rejected_before_decode() {
        let mut trailing = encode_struct_map(&StructMapRelationHandleV1::from(&minimal_handle()));
        trailing.push(0xc0);
        assert_envelope_kind(&trailing, StoredMsgpackErrorKind::TrailingBytes);
        assert_envelope_kind(&[0xc0], StoredMsgpackErrorKind::WrongRoot);
        assert_envelope_kind(&[], StoredMsgpackErrorKind::Empty);
    }

    #[test]
    fn recursive_codec_mixing_is_never_promoted() {
        let handle = normal_parent();
        let child = &handle.indices.get("only_child").expect("fixture child").0;

        let child_v0 = encode_compact(&PositionalRelationHandleV0::from(child));
        let child_v1 = encode_compact(&PositionalRelationHandleV1::from(child));
        let child_map = encode_struct_map(&StructMapRelationHandleV1::from(child));

        let root_v0 = encode_compact(&PositionalRelationHandleV0::from(&handle));
        assert_rejected(&replace_once(&root_v0, &child_v0, &child_v1));

        let root_v1 = encode_compact(&PositionalRelationHandleV1::from(&handle));
        assert_rejected(&replace_once(&root_v1, &child_v1, &child_v0));

        let root_map = encode_struct_map(&StructMapRelationHandleV1::from(&handle));
        assert_rejected(&replace_once(&root_map, &child_map, &child_v1));
    }

    #[test]
    fn closed_nested_id_fixtures_have_distinct_exact_invariants() {
        let mut handles = vec![normal_parent()];
        handles.push(leaf("unique-mixed-codec-child", 201));
        handles.extend((0..24_u64).map(|offset| {
            let name = format!("fixture-top-level-{offset}");
            leaf(&name, 300 + offset)
        }));
        let top_level_ids = handles
            .iter()
            .map(|handle| handle.id.0)
            .collect::<BTreeSet<_>>();
        let encoded = handles
            .iter()
            .map(|handle| encode_test_catalog_record(handle, CatalogCodec::StructMapV1))
            .collect::<Vec<_>>();
        let values = encoded.iter().map(Vec::as_slice).collect::<Vec<_>>();

        for (fixture, collides_with_owner) in [
            (
                ManagedCatalogFixtureV1::NestedChildRelationIdMismatch,
                false,
            ),
            (
                ManagedCatalogFixtureV1::NestedChildRelationIdCollisionWithOwner,
                true,
            ),
        ] {
            let CatalogFixtureRewriteV1::RelationValues(updates) =
                rewrite_catalog_fixture_records_v1(&values, fixture)
                    .expect("closed nested-id fixture must encode")
            else {
                panic!("nested-id fixture unexpectedly rewrote the relation counter");
            };
            let [update] = updates.as_ref() else {
                panic!("nested-id fixture must rewrite exactly one catalog value");
            };
            assert_eq!(update.position(), 0);

            let record = decode_unattested_catalog(update.value())
                .expect("nested-id fixture output must remain exact");
            let owner_id = record.frozen.id.0;
            let child_id = record
                .frozen
                .indices
                .values()
                .next()
                .expect("fixture parent has one normal-index child")
                .0
                .id
                .0;
            if collides_with_owner {
                assert_eq!(child_id, owner_id);
            } else {
                assert_ne!(child_id, owner_id);
                assert!(!top_level_ids.contains(&child_id));
            }
        }
    }

    fn rewrite_first_closed_fixture_record(
        first: &RelationHandle,
        fixture: ManagedCatalogFixtureV1,
    ) -> Result<UnattestedCatalogRecord, CatalogCodecError> {
        let mut handles = vec![first.clone()];
        handles.push(leaf("unique-mixed-codec-child", 201));
        handles.extend((0..24_u64).map(|offset| {
            let name = format!("fixture-top-level-{offset}");
            leaf(&name, 300 + offset)
        }));
        let encoded = handles
            .iter()
            .map(|handle| encode_test_catalog_record(handle, CatalogCodec::StructMapV1))
            .collect::<Vec<_>>();
        let values = encoded.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let CatalogFixtureRewriteV1::RelationValues(updates) =
            rewrite_catalog_fixture_records_v1(&values, fixture)?
        else {
            return Err(CatalogCodecError::FixturePrecondition);
        };
        let first = updates
            .iter()
            .find(|update| update.position() == 0)
            .ok_or(CatalogCodecError::FixturePrecondition)?;
        decode_unattested_catalog(first.value())
    }

    #[test]
    fn v0_fixture_rewrites_reject_recursive_temporal_floor_loss() {
        let mut top_level_floor = normal_parent();
        top_level_floor.tt_gc_floor = Some(-7);
        let mut nested_floor = normal_parent();
        nested_floor
            .indices
            .get_mut("only_child")
            .expect("fixture nested child")
            .0
            .tt_gc_floor = Some(-8);

        for source in [&top_level_floor, &nested_floor] {
            for fixture in [
                ManagedCatalogFixtureV1::AllPositionalV0,
                ManagedCatalogFixtureV1::DeterministicMixed,
            ] {
                assert!(matches!(
                    rewrite_first_closed_fixture_record(source, fixture),
                    Err(CatalogCodecError::FixturePrecondition)
                ));
            }

            for fixture in [
                ManagedCatalogFixtureV1::AllPositionalV1,
                ManagedCatalogFixtureV1::AllStructMapV1,
            ] {
                let record = rewrite_first_closed_fixture_record(source, fixture)
                    .expect("v1 fixture codec must preserve recursive temporal floors");
                assert_eq!(record.frozen.tt_gc_floor, source.tt_gc_floor);
                assert_eq!(
                    record
                        .frozen
                        .indices
                        .get("only_child")
                        .expect("rewritten fixture nested child")
                        .0
                        .tt_gc_floor,
                    source
                        .indices
                        .get("only_child")
                        .expect("source fixture nested child")
                        .0
                        .tt_gc_floor
                );
            }
        }
    }

    #[test]
    fn invalid_fourteenth_slot_cannot_fall_back_to_v0() {
        let mut encoded = encode_compact(&PositionalRelationHandleV0::from(&minimal_handle()));
        assert_eq!(encoded[0], 0x9d);
        encoded[0] = 0x9e;
        // The first 13 slots are an exact v0 record; the new v1-only slot is
        // deliberately not Option<i64>.
        encoded.push(0x90);
        match decode_unattested_catalog(&encoded) {
            Err(CatalogCodecError::NoCanonicalArray { .. }) => {}
            Err(error) => panic!("expected both full array candidates to fail: {error}"),
            Ok(_) => panic!("bad v1 tail was incorrectly promoted as v0"),
        }
    }

    #[test]
    fn noncanonical_v1_array_cannot_fall_back_to_v0() {
        let canonical = encode_compact(&PositionalRelationHandleV1::from(&minimal_handle()));
        assert_eq!(canonical[0], 0x9e);
        let mut nonminimal = Vec::with_capacity(canonical.len() + 2);
        nonminimal.extend_from_slice(&[0xdc, 0x00, 0x0e]);
        nonminimal.extend_from_slice(&canonical[1..]);
        match decode_unattested_catalog(&nonminimal) {
            Err(CatalogCodecError::NoCanonicalArray { positional_v1, .. }) => assert!(matches!(
                positional_v1,
                StoredMsgpackErrorKind::CanonicalMismatch
                    | StoredMsgpackErrorKind::CanonicalLengthMismatch
            )),
            Err(error) => panic!("expected full candidate rejection: {error}"),
            Ok(_) => panic!("noncanonical v1 array was accepted"),
        }
    }

    #[test]
    fn envelope_budgets_stop_byte_depth_item_container_and_token_bombs() {
        let over_bytes = vec![0x90; CATALOG_BYTE_LIMIT + 1];
        assert_envelope_kind(&over_bytes, StoredMsgpackErrorKind::ByteLimit);

        let mut over_depth = vec![0x91; 65];
        over_depth.push(0xc0);
        assert_envelope_kind(&over_depth, StoredMsgpackErrorKind::DepthLimit);

        // array32 declaring 65,537 children: rejected before any allocation or
        // attempt to read the missing payload.
        assert_envelope_kind(
            &[0xdd, 0x00, 0x01, 0x00, 0x01],
            StoredMsgpackErrorKind::ItemLimit,
        );

        // The root plus 65,536 empty child arrays exceeds the container cap by
        // exactly one while staying well below the byte and token caps.
        let mut over_containers = Vec::with_capacity(5 + 65_536);
        over_containers.extend_from_slice(&[0xdd, 0x00, 0x01, 0x00, 0x00]);
        over_containers.resize(5 + 65_536, 0x90);
        assert_envelope_kind(&over_containers, StoredMsgpackErrorKind::ContainerLimit);

        // Exactly 65,535 child containers, but 262,145 total tokens. This
        // isolates the token budget without tripping the other limits first.
        let mut over_tokens = Vec::with_capacity(262_145);
        over_tokens.extend_from_slice(&[0xdd, 0x00, 0x00, 0xff, 0xff]);
        over_tokens.push(0x97);
        over_tokens.extend_from_slice(&[0xc0; 7]);
        for _ in 0..65_534 {
            over_tokens.push(0x93);
            over_tokens.extend_from_slice(&[0xc0; 3]);
        }
        assert_envelope_kind(&over_tokens, StoredMsgpackErrorKind::TokenLimit);
    }

    #[test]
    fn arbitrary_bytes_are_deterministic_and_never_unwind() {
        let mut samples = Vec::with_capacity(1_281);
        samples.push(Vec::new());
        samples.extend((u8::MIN..=u8::MAX).map(|marker| vec![marker]));

        let mut state = 0x6a09_e667_f3bc_c909_u64;
        for sample_index in 0..1_024_u64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = ((state ^ sample_index) % 257) as usize;
            let mut sample = Vec::with_capacity(len);
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                sample.push(state as u8);
            }
            samples.push(sample);
        }

        for sample in samples {
            let first = match catch_unwind(AssertUnwindSafe(|| classify(&sample))) {
                Ok(outcome) => outcome,
                Err(_) => panic!("classifier unwound on arbitrary bytes"),
            };
            let second = match catch_unwind(AssertUnwindSafe(|| classify(&sample))) {
                Ok(outcome) => outcome,
                Err(_) => panic!("classifier unwound on repeated arbitrary bytes"),
            };
            assert_eq!(first, second);
        }
    }

    #[test]
    fn diagnostics_do_not_echo_attacker_controlled_catalog_bytes() {
        let marker = "SENSITIVE_ATTACKER_PAYLOAD_DO_NOT_LOG";
        let mut encoded = Vec::new();
        encoded.push(0x81);
        encoded.push(0xdb);
        encoded.extend_from_slice(&(marker.len() as u32).to_be_bytes());
        encoded.extend_from_slice(marker.as_bytes());
        encoded.push(0xc0);

        let diagnostic = match decode_unattested_catalog(&encoded) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("unknown attacker key was accepted"),
        };
        assert!(diagnostic.len() < 256);
        assert!(!diagnostic.contains(marker));
    }

    #[test]
    fn canonical_writer_enforces_its_independent_byte_cap() {
        let mut writer = BoundedCatalogWriter::new();
        let at_limit = vec![0_u8; CATALOG_BYTE_LIMIT];
        assert_eq!(
            writer
                .write(&at_limit)
                .expect("the exact limit must be writable"),
            CATALOG_BYTE_LIMIT
        );
        let error = writer
            .write(&[0])
            .expect_err("one byte beyond the cap must fail");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(writer.limit_observed, Some((CATALOG_BYTE_LIMIT + 1) as u64));
        assert_eq!(writer.bytes.len(), CATALOG_BYTE_LIMIT);
    }

    #[test]
    fn scan_proven_compact_record_cannot_expand_past_writer_cap() {
        let mut handle = minimal_handle();
        for index in 0..1_000_u64 {
            let mut child = minimal_handle();
            child.name = format!("cap-child-{index:04}").into();
            child.id = RelationId::new(1_000 + index);
            handle
                .indices
                .insert(format!("cap-index-{index:04}").into(), (child, Vec::new()));
        }

        let base_len = encode_compact(&PositionalRelationHandleV0::from(&handle)).len();
        let padding = CATALOG_BYTE_LIMIT
            .checked_sub(base_len + 16)
            .expect("fixture base must leave padding room");
        handle.description = "x".repeat(padding).into();
        let encoded = encode_compact(&PositionalRelationHandleV0::from(&handle));
        assert!(encoded.len() <= CATALOG_BYTE_LIMIT);
        assert!(encoded.len() > CATALOG_BYTE_LIMIT - 32);

        let record = decode_unattested_catalog(&encoded)
            .expect("near-cap compact record must pass the input scan");
        match record.canonical_struct_map() {
            Err(CatalogCodecError::OutputByteLimit { limit, observed }) => {
                assert_eq!(limit, CATALOG_BYTE_LIMIT as u64);
                assert!(observed > limit);
            }
            Err(error) => panic!("expected output byte cap, got {error}"),
            Ok(_) => panic!("map expansion incorrectly exceeded the output cap"),
        }
    }

    #[test]
    fn canonical_writer_post_scan_rejects_an_impossible_internal_record() {
        // Production code cannot construct this: the record fields and
        // constructor are private, and no scanned input with this token count
        // can reach the writer. The forged value exercises the independent
        // post-serialization scanner as defense in depth.
        let mut frozen = StructMapRelationHandleV1::from(&minimal_handle());
        frozen.put_triggers = vec![String::new(); 65_536];
        frozen.rm_triggers = vec![String::new(); 65_536];
        frozen.replace_triggers = vec![String::new(); 65_536];
        frozen.indices.insert(
            "token-source".to_owned(),
            (
                StructMapRelationHandleV1::from(&minimal_handle()),
                vec![0; 65_536],
            ),
        );
        let forged = UnattestedCatalogRecord {
            codec: CatalogCodec::StructMapV1,
            frozen,
        };
        match forged.canonical_struct_map() {
            Err(CatalogCodecError::OutputEnvelope(error)) => {
                assert_eq!(error.kind(), StoredMsgpackErrorKind::TokenLimit);
            }
            Err(error) => panic!("expected post-scan token cap, got {error}"),
            Ok(_) => panic!("post-scan admitted an over-token output"),
        }
    }
}
