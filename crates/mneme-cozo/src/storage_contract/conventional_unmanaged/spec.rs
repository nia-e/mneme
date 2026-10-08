//! Authoritative typed specification for Mneme's conventional unmanaged catalog.
//!
//! This module contains facts only. It does not inspect Cozo presentation rows,
//! deserialize Mnestic storage, or derive authority from the illustrative
//! [`crate::SCHEMA`] string. Presentation and raw-storage validators must both
//! consume this same closed specification.

/// Historical catalog-generation marker published by the conventional writer.
///
/// This persisted value is frozen even though the Rust vocabulary around it is
/// now semantic rather than tied to an internal review label.
pub(crate) const LEGACY_CATALOG_GENERATION_MARKER: &str =
    "status_partitioned_hnsw_mnestic_0_13_v3_canonical_node_v1_tag_v2_f7";

/// Reserved generation marker for an exact struct-map-v1 catalog.
pub(crate) const STRUCT_MAP_CATALOG_GENERATION_MARKER: &str =
    "status_partitioned_hnsw_mnestic_0_13_v3_canonical_node_v1_tag_v2_catalog_struct_map_v1";

/// Fresh-only capture generation. The rotated vector marker is the durable
/// old-writer fence; an additive metadata row would not fence old binaries.
pub(crate) const CAPTURE_V1_CATALOG_GENERATION_MARKER: &str = "status_partitioned_hnsw_mnestic_0_13_v3_canonical_node_v1_tag_v2_catalog_struct_map_v1_capture_v1";

/// Episode editions and their projections require a separate exact catalog and
/// durable old-writer fence. The capture-v1 contract above stays frozen.
pub(crate) const EPISODE_V1_CATALOG_GENERATION_MARKER: &str = "status_partitioned_hnsw_mnestic_0_13_v3_canonical_node_v1_tag_v2_catalog_struct_map_v1_episode_v1";

/// Reserved successor marker for the first managed-writer generation.
pub(crate) const MANAGED_WRITER_GENERATION_MARKER: &str =
    "status_partitioned_hnsw_mnestic_0_13_v3_canonical_node_v1_tag_v2_managed_writer_v1";

/// Permanent old-writer guard required by this storage contract.
pub(crate) const PERMANENT_VECTOR_GUARD_VALUE: &str =
    "status-partitioned-hnsw-v3-canonical-node-v1-tag-v2-f7-read-only-guard-v1";

/// How one relation is exposed through Mnestic's system-command catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogAccess {
    Normal,
    ReadOnly,
    /// Mnestic's catalog presentation synthesizes this for every child relation.
    SynthesizedIndex,
}

impl CatalogAccess {
    pub(crate) const fn catalog_name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::ReadOnly => "read_only",
            Self::SynthesizedIndex => "index",
        }
    }
}

/// Actual durable access stored in a `RelationHandle`.
///
/// There is deliberately no synthesized-index variant: a durable child handle
/// claiming catalog presentation access is unrepresentable in this spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DurableAccess {
    Normal,
    ReadOnly,
}

/// One admitted column type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColumnType {
    String,
    NullableString,
    Bool,
    Int,
    NullableInt,
    Float,
    NullableBytes,
    IntList,
    /// The positive dimension is authenticated from the `node_vec.e` type.
    F32Vector,
}

impl ColumnType {
    pub(crate) const fn exact_catalog_name(self) -> Option<&'static str> {
        match self {
            Self::String => Some("String"),
            Self::NullableString => Some("String?"),
            Self::Bool => Some("Bool"),
            Self::Int => Some("Int"),
            Self::NullableInt => Some("Int?"),
            Self::Float => Some("Float"),
            Self::NullableBytes => Some("Bytes?"),
            Self::IntList => Some("[Int]"),
            Self::F32Vector => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ColumnSpec {
    pub(crate) name: &'static str,
    pub(crate) is_key: bool,
    pub(crate) ty: ColumnType,
}

impl ColumnSpec {
    const fn key(name: &'static str, ty: ColumnType) -> Self {
        Self {
            name,
            is_key: true,
            ty,
        }
    }

    const fn value(name: &'static str, ty: ColumnType) -> Self {
        Self {
            name,
            is_key: false,
            ty,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RelationSpec {
    pub(crate) name: &'static str,
    /// Access emitted by `::relations`.
    pub(crate) catalog_access: CatalogAccess,
    /// Access that must be present in the durable `RelationHandle`.
    pub(crate) durable_access: DurableAccess,
    pub(crate) columns: &'static [ColumnSpec],
}

impl RelationSpec {
    const fn base(
        name: &'static str,
        access: DurableAccess,
        columns: &'static [ColumnSpec],
    ) -> Self {
        let catalog_access = match access {
            DurableAccess::Normal => CatalogAccess::Normal,
            DurableAccess::ReadOnly => CatalogAccess::ReadOnly,
        };
        Self {
            name,
            catalog_access,
            durable_access: access,
            columns,
        }
    }

    const fn child(name: &'static str, columns: &'static [ColumnSpec]) -> Self {
        Self {
            name,
            catalog_access: CatalogAccess::SynthesizedIndex,
            durable_access: DurableAccess::Normal,
            columns,
        }
    }

    pub(crate) fn key_count(self) -> usize {
        self.columns.iter().filter(|column| column.is_key).count()
    }
}

/// Stable, positive, never-reused page ordinals for the base vocabulary.
/// Derived child relations deliberately do not consume page ordinals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u16)]
pub(crate) enum BaseRelationOrdinal {
    Meta = 1,
    Node = 2,
    NodeTag = 3,
    NodeTagV2 = 4,
    NodeSearch = 5,
    NodeVec = 6,
    Edge = 7,
    EdgeAnchor = 8,
    Contradiction = 9,
    MergeCandidate = 10,
    FullMergeCommit = 11,
    SupersedeCommit = 12,
    RemoteEdge = 13,
    FeedbackRetry = 14,
    FeedbackRetryOrder = 15,
    PermanentVectorGuard = 16,
    EpisodeHead = 17,
    EpisodeHistory = 18,
    EpisodeTime = 19,
    EpisodeSearch = 20,
    Concern = 21,
    Touchstone = 22,
    TouchstoneTarget = 23,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BaseRelationSpec {
    pub(crate) ordinal: BaseRelationOrdinal,
    pub(crate) relation: RelationSpec,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NormalIndexSpec {
    /// Source-column ordinals in the exact order used as the child's key prefix.
    pub(crate) ordered_columns: &'static [u64],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TokenizerSpec {
    pub(crate) name: &'static str,
    pub(crate) argument_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FtsIndexSpec {
    /// The durable extractor after Mnestic combines `extract_filter` with the
    /// requested source expression.
    pub(crate) extractor: &'static str,
    pub(crate) tokenizer: TokenizerSpec,
    pub(crate) tokenizer_filters: &'static [TokenizerSpec],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VectorElementType {
    F32,
}

impl VectorElementType {
    pub(crate) const fn catalog_name(self) -> &'static str {
        match self {
            Self::F32 => "F32",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HnswDistance {
    Cosine,
}

impl HnswDistance {
    pub(crate) const fn catalog_name(self) -> &'static str {
        match self {
            Self::Cosine => "Cosine",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VectorDimensionBinding {
    pub(crate) relation: &'static str,
    pub(crate) column: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HnswCatalogSpec {
    pub(crate) dimension: VectorDimensionBinding,
    pub(crate) dtype: VectorElementType,
    pub(crate) vector_fields: &'static [u64],
    pub(crate) distance: HnswDistance,
    pub(crate) ef_construction: u64,
    pub(crate) m_neighbours: u64,
    pub(crate) m_max: u64,
    pub(crate) m_max0: u64,
    pub(crate) level_multiplier_bits: u64,
    pub(crate) extend_candidates: bool,
    pub(crate) keep_pruned_connections: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HnswIndexSpec {
    pub(crate) catalog: HnswCatalogSpec,
    /// This field is omitted by `::indices` and must be checked in raw storage.
    pub(crate) index_filter: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexSpec {
    Normal(NormalIndexSpec),
    Fts(FtsIndexSpec),
    Hnsw(HnswIndexSpec),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DerivedRelationSpec {
    pub(crate) owner_relation: &'static str,
    pub(crate) local_name: &'static str,
    pub(crate) relation: RelationSpec,
    pub(crate) index: IndexSpec,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SemanticObligations {
    pub(crate) node_tag_relation: &'static str,
    pub(crate) node_tag_exact_row_count: u64,
    pub(crate) permanent_guard_relation: &'static str,
    pub(crate) permanent_guard_exact_row_count: u64,
    pub(crate) permanent_guard_key_column: &'static str,
    pub(crate) permanent_guard_key: &'static str,
    pub(crate) permanent_guard_value_column: &'static str,
    pub(crate) permanent_guard_value: &'static str,
}

/// Global absence rules shared by presentation and raw-storage validation.
/// They are deliberately policy rather than repeated per-relation empty lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct ForbiddenFeatures;

#[allow(dead_code)]
impl ForbiddenFeatures {
    pub(crate) const fn allows_lsh_indices(self) -> bool {
        false
    }

    pub(crate) const fn allows_column_defaults(self) -> bool {
        false
    }

    pub(crate) const fn allows_triggers(self) -> bool {
        false
    }

    pub(crate) const fn allows_descriptions(self) -> bool {
        false
    }

    pub(crate) const fn allows_temporal_floors(self) -> bool {
        false
    }

    pub(crate) const fn allows_temporary_relations(self) -> bool {
        false
    }
}

#[allow(dead_code)]
pub(crate) const FORBIDDEN_FEATURES: ForbiddenFeatures = ForbiddenFeatures;

const META_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("k", ColumnType::String),
    ColumnSpec::value("v", ColumnType::String),
];
const NODE_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("id", ColumnType::String),
    ColumnSpec::value("data", ColumnType::String),
    ColumnSpec::value("status", ColumnType::String),
];
const NODE_TAG_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("id", ColumnType::String),
    ColumnSpec::key("tag", ColumnType::String),
];
const NODE_TAG_V2_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("tag", ColumnType::String),
    ColumnSpec::key("status", ColumnType::String),
    ColumnSpec::key("sample_hash", ColumnType::Int),
    ColumnSpec::key("id", ColumnType::String),
];
const NODE_SEARCH_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("id", ColumnType::String),
    ColumnSpec::value("summary", ColumnType::String),
    ColumnSpec::value("status", ColumnType::String),
];
const NODE_VEC_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("id", ColumnType::String),
    ColumnSpec::value("e", ColumnType::F32Vector),
    ColumnSpec::value("status", ColumnType::String),
];
const EDGE_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("from", ColumnType::String),
    ColumnSpec::key("to", ColumnType::String),
    ColumnSpec::value("weight", ColumnType::Float),
    ColumnSpec::value("kind", ColumnType::String),
    ColumnSpec::value("last_reinforced", ColumnType::Int),
    ColumnSpec::value("trials", ColumnType::Int),
    ColumnSpec::value("interference", ColumnType::Int),
];
const EDGE_ANCHOR_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("from", ColumnType::String),
    ColumnSpec::key("to", ColumnType::String),
    ColumnSpec::value("start", ColumnType::Int),
    ColumnSpec::value("end", ColumnType::Int),
];
const CANDIDATE_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("lo", ColumnType::String),
    ColumnSpec::key("hi", ColumnType::String),
    ColumnSpec::value("observations", ColumnType::Int),
    ColumnSpec::value("first_seen", ColumnType::Int),
    ColumnSpec::value("last_seen", ColumnType::Int),
    ColumnSpec::value("resolution", ColumnType::NullableString),
];
const COMMIT_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("lo", ColumnType::String),
    ColumnSpec::key("hi", ColumnType::String),
    ColumnSpec::value("winner", ColumnType::String),
    ColumnSpec::value("loser", ColumnType::String),
    ColumnSpec::value("applied_at", ColumnType::Int),
];
const REMOTE_EDGE_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("from", ColumnType::String),
    ColumnSpec::key("target_db", ColumnType::String),
    ColumnSpec::key("target", ColumnType::String),
    ColumnSpec::value("weight", ColumnType::Float),
];
const FEEDBACK_RETRY_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("key", ColumnType::String),
    ColumnSpec::value("fingerprint", ColumnType::String),
    ColumnSpec::value("applied_at", ColumnType::Int),
];
const FEEDBACK_RETRY_ORDER_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("epoch", ColumnType::String),
    ColumnSpec::key("sequence", ColumnType::Int),
    ColumnSpec::key("key", ColumnType::String),
    ColumnSpec::value("marker", ColumnType::Bool),
];
const VECTOR_GUARD_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("fence", ColumnType::String),
    ColumnSpec::value("generation", ColumnType::String),
];

const NODE_TAG_BY_TAG_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("tag", ColumnType::String),
    ColumnSpec::key("id", ColumnType::String),
];
const NODE_TAG_V2_BY_ID_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("id", ColumnType::String),
    ColumnSpec::key("tag", ColumnType::String),
    ColumnSpec::key("status", ColumnType::String),
    ColumnSpec::key("sample_hash", ColumnType::Int),
];
const NODE_SEARCH_BY_STATUS_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("status", ColumnType::String),
    ColumnSpec::key("id", ColumnType::String),
];
const FTS_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("word", ColumnType::String),
    ColumnSpec::key("src_id", ColumnType::String),
    ColumnSpec::value("offset_from", ColumnType::IntList),
    ColumnSpec::value("offset_to", ColumnType::IntList),
    ColumnSpec::value("position", ColumnType::IntList),
    ColumnSpec::value("total_length", ColumnType::Int),
];
const HNSW_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("layer", ColumnType::Int),
    ColumnSpec::key("fr_id", ColumnType::String),
    ColumnSpec::key("fr__field", ColumnType::Int),
    ColumnSpec::key("fr__sub_idx", ColumnType::Int),
    ColumnSpec::key("to_id", ColumnType::String),
    ColumnSpec::key("to__field", ColumnType::Int),
    ColumnSpec::key("to__sub_idx", ColumnType::Int),
    ColumnSpec::value("dist", ColumnType::Float),
    ColumnSpec::value("hash", ColumnType::NullableBytes),
    ColumnSpec::value("ignore_link", ColumnType::Bool),
];
const EDGE_BY_TO_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("to", ColumnType::String),
    ColumnSpec::key("from", ColumnType::String),
];

pub(crate) const BASE_RELATIONS: [BaseRelationSpec; 16] = [
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::Meta,
        relation: RelationSpec::base("meta", DurableAccess::Normal, META_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::Node,
        relation: RelationSpec::base("node", DurableAccess::Normal, NODE_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::NodeTag,
        relation: RelationSpec::base("node_tag", DurableAccess::ReadOnly, NODE_TAG_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::NodeTagV2,
        relation: RelationSpec::base("node_tag_v2", DurableAccess::Normal, NODE_TAG_V2_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::NodeSearch,
        relation: RelationSpec::base("node_search", DurableAccess::Normal, NODE_SEARCH_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::NodeVec,
        relation: RelationSpec::base("node_vec", DurableAccess::Normal, NODE_VEC_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::Edge,
        relation: RelationSpec::base("edge", DurableAccess::Normal, EDGE_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::EdgeAnchor,
        relation: RelationSpec::base("edge_anchor", DurableAccess::Normal, EDGE_ANCHOR_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::Contradiction,
        relation: RelationSpec::base("contradiction", DurableAccess::Normal, CANDIDATE_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::MergeCandidate,
        relation: RelationSpec::base("merge_candidate", DurableAccess::Normal, CANDIDATE_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::FullMergeCommit,
        relation: RelationSpec::base("full_merge_commit", DurableAccess::Normal, COMMIT_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::SupersedeCommit,
        relation: RelationSpec::base("supersede_commit", DurableAccess::Normal, COMMIT_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::RemoteEdge,
        relation: RelationSpec::base("remote_edge", DurableAccess::Normal, REMOTE_EDGE_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::FeedbackRetry,
        relation: RelationSpec::base(
            "feedback_retry",
            DurableAccess::Normal,
            FEEDBACK_RETRY_COLUMNS,
        ),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::FeedbackRetryOrder,
        relation: RelationSpec::base(
            "feedback_retry_order",
            DurableAccess::Normal,
            FEEDBACK_RETRY_ORDER_COLUMNS,
        ),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::PermanentVectorGuard,
        relation: RelationSpec::base(
            "mneme_reembed_shadow_node_vec",
            DurableAccess::ReadOnly,
            VECTOR_GUARD_COLUMNS,
        ),
    },
];

const SIMPLE_TOKENIZER: TokenizerSpec = TokenizerSpec {
    name: "Simple",
    argument_count: 0,
};
const LOWERCASE_FILTERS: &[TokenizerSpec] = &[TokenizerSpec {
    name: "Lowercase",
    argument_count: 0,
}];
const NODE_VEC_DIMENSION: VectorDimensionBinding = VectorDimensionBinding {
    relation: "node_vec",
    column: "e",
};
const HNSW_VECTOR_FIELDS: &[u64] = &[1];
const HNSW_LEVEL_MULTIPLIER_BITS: u64 = 0x3fd7_1547_652b_82fe;

pub(crate) const HNSW_CATALOG_SPEC: HnswCatalogSpec = HnswCatalogSpec {
    dimension: NODE_VEC_DIMENSION,
    dtype: VectorElementType::F32,
    vector_fields: HNSW_VECTOR_FIELDS,
    distance: HnswDistance::Cosine,
    ef_construction: 200,
    m_neighbours: 16,
    m_max: 16,
    m_max0: 32,
    level_multiplier_bits: HNSW_LEVEL_MULTIPLIER_BITS,
    extend_candidates: false,
    keep_pruned_connections: false,
};

const fn fts(extractor: &'static str) -> IndexSpec {
    IndexSpec::Fts(FtsIndexSpec {
        extractor,
        tokenizer: SIMPLE_TOKENIZER,
        tokenizer_filters: LOWERCASE_FILTERS,
    })
}

const fn hnsw(index_filter: &'static str) -> IndexSpec {
    IndexSpec::Hnsw(HnswIndexSpec {
        catalog: HNSW_CATALOG_SPEC,
        index_filter,
    })
}

pub(crate) const DERIVED_RELATIONS: [DerivedRelationSpec; 10] = [
    DerivedRelationSpec {
        owner_relation: "node_tag",
        local_name: "by_tag",
        relation: RelationSpec::child("node_tag:by_tag", NODE_TAG_BY_TAG_COLUMNS),
        index: IndexSpec::Normal(NormalIndexSpec {
            ordered_columns: &[1, 0],
        }),
    },
    DerivedRelationSpec {
        owner_relation: "node_tag_v2",
        local_name: "by_id",
        relation: RelationSpec::child("node_tag_v2:by_id", NODE_TAG_V2_BY_ID_COLUMNS),
        index: IndexSpec::Normal(NormalIndexSpec {
            ordered_columns: &[3, 0, 1, 2],
        }),
    },
    DerivedRelationSpec {
        owner_relation: "node_search",
        local_name: "by_status",
        relation: RelationSpec::child("node_search:by_status", NODE_SEARCH_BY_STATUS_COLUMNS),
        index: IndexSpec::Normal(NormalIndexSpec {
            ordered_columns: &[2, 0],
        }),
    },
    DerivedRelationSpec {
        owner_relation: "node_search",
        local_name: "active_fts",
        relation: RelationSpec::child("node_search:active_fts", FTS_COLUMNS),
        index: fts("if(eq(status, \"active\"), summary)"),
    },
    DerivedRelationSpec {
        owner_relation: "node_search",
        local_name: "candidate_fts",
        relation: RelationSpec::child("node_search:candidate_fts", FTS_COLUMNS),
        index: fts("if(eq(status, \"candidate\"), summary)"),
    },
    DerivedRelationSpec {
        owner_relation: "node_search",
        local_name: "archived_fts",
        relation: RelationSpec::child("node_search:archived_fts", FTS_COLUMNS),
        index: fts("if(eq(status, \"archived\"), summary)"),
    },
    DerivedRelationSpec {
        owner_relation: "node_vec",
        local_name: "active_idx",
        relation: RelationSpec::child("node_vec:active_idx", HNSW_COLUMNS),
        index: hnsw("status == 'active'"),
    },
    DerivedRelationSpec {
        owner_relation: "node_vec",
        local_name: "candidate_idx",
        relation: RelationSpec::child("node_vec:candidate_idx", HNSW_COLUMNS),
        index: hnsw("status == 'candidate'"),
    },
    DerivedRelationSpec {
        owner_relation: "node_vec",
        local_name: "archived_idx",
        relation: RelationSpec::child("node_vec:archived_idx", HNSW_COLUMNS),
        index: hnsw("status == 'archived'"),
    },
    DerivedRelationSpec {
        owner_relation: "edge",
        local_name: "by_to",
        relation: RelationSpec::child("edge:by_to", EDGE_BY_TO_COLUMNS),
        index: IndexSpec::Normal(NormalIndexSpec {
            ordered_columns: &[1, 0],
        }),
    },
];

// Episode projections are derived from immutable canonical Node facets. Their
// separate inventory must never broaden the supported capture-v1 predecessor.
const EPISODE_HEAD_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("root", ColumnType::String),
    ColumnSpec::value("head", ColumnType::String),
    ColumnSpec::value("revision", ColumnType::Int),
    ColumnSpec::value("recorded_at", ColumnType::Int),
];
const EPISODE_HISTORY_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("root", ColumnType::String),
    ColumnSpec::key("revision", ColumnType::Int),
    ColumnSpec::value("edition", ColumnType::String),
];
const EPISODE_TIME_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("axis", ColumnType::String),
    ColumnSpec::key("thread", ColumnType::String),
    ColumnSpec::key("at", ColumnType::Int),
    ColumnSpec::key("root", ColumnType::String),
    ColumnSpec::value("head", ColumnType::String),
    ColumnSpec::value("occurred_start", ColumnType::NullableInt),
    ColumnSpec::value("occurred_end", ColumnType::NullableInt),
];
const EPISODE_SEARCH_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("root", ColumnType::String),
    ColumnSpec::value("head", ColumnType::String),
    ColumnSpec::value("summary", ColumnType::String),
    ColumnSpec::value("thread", ColumnType::String),
    ColumnSpec::value("recorded_at", ColumnType::Int),
    ColumnSpec::value("occurred_start", ColumnType::NullableInt),
    ColumnSpec::value("occurred_end", ColumnType::NullableInt),
];
// FTS child source keys retain the owner's column name, unlike node_search.id.
const EPISODE_FTS_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("word", ColumnType::String),
    ColumnSpec::key("src_root", ColumnType::String),
    ColumnSpec::value("offset_from", ColumnType::IntList),
    ColumnSpec::value("offset_to", ColumnType::IntList),
    ColumnSpec::value("position", ColumnType::IntList),
    ColumnSpec::value("total_length", ColumnType::Int),
];

pub(crate) const EPISODE_BASE_RELATIONS: [BaseRelationSpec; 20] = [
    BASE_RELATIONS[0],
    BASE_RELATIONS[1],
    BASE_RELATIONS[2],
    BASE_RELATIONS[3],
    BASE_RELATIONS[4],
    BASE_RELATIONS[5],
    BASE_RELATIONS[6],
    BASE_RELATIONS[7],
    BASE_RELATIONS[8],
    BASE_RELATIONS[9],
    BASE_RELATIONS[10],
    BASE_RELATIONS[11],
    BASE_RELATIONS[12],
    BASE_RELATIONS[13],
    BASE_RELATIONS[14],
    BASE_RELATIONS[15],
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::EpisodeHead,
        relation: RelationSpec::base("episode_head", DurableAccess::Normal, EPISODE_HEAD_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::EpisodeHistory,
        relation: RelationSpec::base(
            "episode_history",
            DurableAccess::Normal,
            EPISODE_HISTORY_COLUMNS,
        ),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::EpisodeTime,
        relation: RelationSpec::base("episode_time", DurableAccess::Normal, EPISODE_TIME_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::EpisodeSearch,
        relation: RelationSpec::base(
            "episode_search",
            DurableAccess::Normal,
            EPISODE_SEARCH_COLUMNS,
        ),
    },
];
pub(crate) const EPISODE_DERIVED_RELATIONS: [DerivedRelationSpec; 11] = [
    DERIVED_RELATIONS[0],
    DERIVED_RELATIONS[1],
    DERIVED_RELATIONS[2],
    DERIVED_RELATIONS[3],
    DERIVED_RELATIONS[4],
    DERIVED_RELATIONS[5],
    DERIVED_RELATIONS[6],
    DERIVED_RELATIONS[7],
    DERIVED_RELATIONS[8],
    DERIVED_RELATIONS[9],
    DerivedRelationSpec {
        owner_relation: "episode_search",
        local_name: "fts",
        relation: RelationSpec::child("episode_search:fts", EPISODE_FTS_COLUMNS),
        index: fts("summary"),
    },
];

pub(crate) const SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER: &str = "status_partitioned_hnsw_mnestic_0_13_v4_canonical_node_v2_tag_v2_catalog_struct_map_v1_capture_v2_episode_v1";
pub(crate) const SINGLE_GRAPH_DERIVED_RELATIONS: [DerivedRelationSpec; 9] = [
    DERIVED_RELATIONS[0],
    DERIVED_RELATIONS[1],
    DERIVED_RELATIONS[2],
    DERIVED_RELATIONS[3],
    DERIVED_RELATIONS[5],
    DERIVED_RELATIONS[6],
    DERIVED_RELATIONS[8],
    DERIVED_RELATIONS[9],
    EPISODE_DERIVED_RELATIONS[10],
];

/// Advisory cache successor. V1 catalogs and markers above remain frozen.
pub(crate) const CONCERN_V1_CATALOG_GENERATION_MARKER: &str = "status_partitioned_hnsw_mnestic_0_13_v4_canonical_node_v2_tag_v2_catalog_struct_map_v1_capture_v2_episode_v1_concern_v1";
/// Episode account metadata successor; physical relation inventory is unchanged.
pub(crate) const EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER: &str = "status_partitioned_hnsw_mnestic_0_13_v4_canonical_node_v3_tag_v2_catalog_struct_map_v1_capture_v2_episode_v2_concern_v1";
const CONCERN_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("lo", ColumnType::String),
    ColumnSpec::key("hi", ColumnType::String),
    ColumnSpec::key("kind", ColumnType::String),
    ColumnSpec::value("data", ColumnType::String),
];
const CONCERN_BY_HI_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("hi", ColumnType::String),
    ColumnSpec::key("lo", ColumnType::String),
    ColumnSpec::key("kind", ColumnType::String),
];
pub(crate) const CONCERN_BASE_RELATIONS: [BaseRelationSpec; 21] = [
    EPISODE_BASE_RELATIONS[0],
    EPISODE_BASE_RELATIONS[1],
    EPISODE_BASE_RELATIONS[2],
    EPISODE_BASE_RELATIONS[3],
    EPISODE_BASE_RELATIONS[4],
    EPISODE_BASE_RELATIONS[5],
    EPISODE_BASE_RELATIONS[6],
    EPISODE_BASE_RELATIONS[7],
    EPISODE_BASE_RELATIONS[8],
    EPISODE_BASE_RELATIONS[9],
    EPISODE_BASE_RELATIONS[10],
    EPISODE_BASE_RELATIONS[11],
    EPISODE_BASE_RELATIONS[12],
    EPISODE_BASE_RELATIONS[13],
    EPISODE_BASE_RELATIONS[14],
    EPISODE_BASE_RELATIONS[15],
    EPISODE_BASE_RELATIONS[16],
    EPISODE_BASE_RELATIONS[17],
    EPISODE_BASE_RELATIONS[18],
    EPISODE_BASE_RELATIONS[19],
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::Concern,
        relation: RelationSpec::base("concern", DurableAccess::Normal, CONCERN_COLUMNS),
    },
];
pub(crate) const CONCERN_DERIVED_RELATIONS: [DerivedRelationSpec; 10] = [
    SINGLE_GRAPH_DERIVED_RELATIONS[0],
    SINGLE_GRAPH_DERIVED_RELATIONS[1],
    SINGLE_GRAPH_DERIVED_RELATIONS[2],
    SINGLE_GRAPH_DERIVED_RELATIONS[3],
    SINGLE_GRAPH_DERIVED_RELATIONS[4],
    SINGLE_GRAPH_DERIVED_RELATIONS[5],
    SINGLE_GRAPH_DERIVED_RELATIONS[6],
    SINGLE_GRAPH_DERIVED_RELATIONS[7],
    SINGLE_GRAPH_DERIVED_RELATIONS[8],
    DerivedRelationSpec {
        owner_relation: "concern",
        local_name: "by_hi",
        relation: RelationSpec::child("concern:by_hi", CONCERN_BY_HI_COLUMNS),
        index: IndexSpec::Normal(NormalIndexSpec {
            ordered_columns: &[1, 0, 2],
        }),
    },
];

/// Typed immutable annotations require a distinct physical generation.
pub(crate) const TOUCHSTONES_V1_CATALOG_GENERATION_MARKER: &str = "status_partitioned_hnsw_mnestic_0_13_v4_canonical_node_v3_tag_v2_catalog_struct_map_v1_capture_v2_episode_v2_concern_v1_touchstones_v1";
const TOUCHSTONE_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("owner", ColumnType::String),
    ColumnSpec::value("data", ColumnType::String),
];
const TOUCHSTONE_TARGET_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("target", ColumnType::String),
    ColumnSpec::key("owner", ColumnType::String),
];
const TOUCHSTONE_TARGET_BY_OWNER_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::key("owner", ColumnType::String),
    ColumnSpec::key("target", ColumnType::String),
];
pub(crate) const TOUCHSTONES_BASE_RELATIONS: [BaseRelationSpec; 23] = [
    CONCERN_BASE_RELATIONS[0],
    CONCERN_BASE_RELATIONS[1],
    CONCERN_BASE_RELATIONS[2],
    CONCERN_BASE_RELATIONS[3],
    CONCERN_BASE_RELATIONS[4],
    CONCERN_BASE_RELATIONS[5],
    CONCERN_BASE_RELATIONS[6],
    CONCERN_BASE_RELATIONS[7],
    CONCERN_BASE_RELATIONS[8],
    CONCERN_BASE_RELATIONS[9],
    CONCERN_BASE_RELATIONS[10],
    CONCERN_BASE_RELATIONS[11],
    CONCERN_BASE_RELATIONS[12],
    CONCERN_BASE_RELATIONS[13],
    CONCERN_BASE_RELATIONS[14],
    CONCERN_BASE_RELATIONS[15],
    CONCERN_BASE_RELATIONS[16],
    CONCERN_BASE_RELATIONS[17],
    CONCERN_BASE_RELATIONS[18],
    CONCERN_BASE_RELATIONS[19],
    CONCERN_BASE_RELATIONS[20],
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::Touchstone,
        relation: RelationSpec::base("touchstone", DurableAccess::Normal, TOUCHSTONE_COLUMNS),
    },
    BaseRelationSpec {
        ordinal: BaseRelationOrdinal::TouchstoneTarget,
        relation: RelationSpec::base(
            "touchstone_target",
            DurableAccess::Normal,
            TOUCHSTONE_TARGET_COLUMNS,
        ),
    },
];
pub(crate) const TOUCHSTONES_DERIVED_RELATIONS: [DerivedRelationSpec; 11] = [
    CONCERN_DERIVED_RELATIONS[0],
    CONCERN_DERIVED_RELATIONS[1],
    CONCERN_DERIVED_RELATIONS[2],
    CONCERN_DERIVED_RELATIONS[3],
    CONCERN_DERIVED_RELATIONS[4],
    CONCERN_DERIVED_RELATIONS[5],
    CONCERN_DERIVED_RELATIONS[6],
    CONCERN_DERIVED_RELATIONS[7],
    CONCERN_DERIVED_RELATIONS[8],
    CONCERN_DERIVED_RELATIONS[9],
    DerivedRelationSpec {
        owner_relation: "touchstone_target",
        local_name: "by_owner",
        relation: RelationSpec::child(
            "touchstone_target:by_owner",
            TOUCHSTONE_TARGET_BY_OWNER_COLUMNS,
        ),
        index: IndexSpec::Normal(NormalIndexSpec {
            ordered_columns: &[1, 0],
        }),
    },
];

/// Closed alternatives, never an inventory inferred from untrusted input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogContract {
    Predecessor,
    EpisodeV1,
    SingleGraphV1,
    ConcernV1,
    EpisodeContextV2,
    TouchstonesV1,
}
impl CatalogContract {
    pub(crate) const fn bases(self) -> &'static [BaseRelationSpec] {
        match self {
            Self::Predecessor => &BASE_RELATIONS,
            Self::EpisodeV1 | Self::SingleGraphV1 => &EPISODE_BASE_RELATIONS,
            Self::ConcernV1 | Self::EpisodeContextV2 => &CONCERN_BASE_RELATIONS,
            Self::TouchstonesV1 => &TOUCHSTONES_BASE_RELATIONS,
        }
    }
    pub(crate) const fn derived(self) -> &'static [DerivedRelationSpec] {
        match self {
            Self::Predecessor => &DERIVED_RELATIONS,
            Self::EpisodeV1 => &EPISODE_DERIVED_RELATIONS,
            Self::SingleGraphV1 => &SINGLE_GRAPH_DERIVED_RELATIONS,
            Self::ConcernV1 | Self::EpisodeContextV2 => &CONCERN_DERIVED_RELATIONS,
            Self::TouchstonesV1 => &TOUCHSTONES_DERIVED_RELATIONS,
        }
    }
    pub(crate) fn relations(self) -> impl Clone + Iterator<Item = RelationSpec> {
        self.bases()
            .iter()
            .map(|base| base.relation)
            .chain(self.derived().iter().map(|child| child.relation))
    }
    pub(crate) fn indices_for(
        self,
        owner: &str,
    ) -> impl Iterator<Item = &'static DerivedRelationSpec> {
        self.derived()
            .iter()
            .filter(move |child| child.owner_relation == owner)
    }
    pub(crate) fn relation_slot(self, name: &str) -> Option<usize> {
        self.relations().position(|relation| relation.name == name)
    }
    pub(crate) fn relation_at(self, slot: usize) -> RelationSpec {
        if slot < self.bases().len() {
            self.bases()[slot].relation
        } else {
            self.derived()[slot - self.bases().len()].relation
        }
    }
}

pub(crate) const SEMANTIC_OBLIGATIONS: SemanticObligations = SemanticObligations {
    node_tag_relation: "node_tag",
    node_tag_exact_row_count: 0,
    permanent_guard_relation: "mneme_reembed_shadow_node_vec",
    permanent_guard_exact_row_count: 1,
    permanent_guard_key_column: "fence",
    permanent_guard_key: crate::vector_projection::GUARD_KEY,
    permanent_guard_value_column: "generation",
    permanent_guard_value: PERMANENT_VECTOR_GUARD_VALUE,
};

pub(crate) fn all_physical_relations() -> impl Clone + Iterator<Item = RelationSpec> {
    BASE_RELATIONS
        .iter()
        .map(|base| base.relation)
        .chain(DERIVED_RELATIONS.iter().map(|child| child.relation))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn base(name: &str) -> RelationSpec {
        BASE_RELATIONS
            .iter()
            .find(|base| base.relation.name == name)
            .unwrap_or_else(|| panic!("missing conventional base relation {name:?}"))
            .relation
    }

    fn child(name: &str) -> DerivedRelationSpec {
        DERIVED_RELATIONS
            .iter()
            .find(|child| child.relation.name == name)
            .unwrap_or_else(|| panic!("missing conventional child relation {name:?}"))
            .to_owned()
    }

    #[test]
    fn clean_struct_map_generation_marker_is_frozen_pre_1_0_abi() {
        assert_eq!(
            STRUCT_MAP_CATALOG_GENERATION_MARKER.as_bytes(),
            b"status_partitioned_hnsw_mnestic_0_13_v3_canonical_node_v1_tag_v2_catalog_struct_map_v1"
        );
        assert_ne!(
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
            LEGACY_CATALOG_GENERATION_MARKER
        );
        assert_ne!(
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
            MANAGED_WRITER_GENERATION_MARKER
        );
    }

    #[test]
    fn capture_generation_marker_is_distinct_and_frozen() {
        assert_eq!(
            CAPTURE_V1_CATALOG_GENERATION_MARKER.as_bytes(),
            b"status_partitioned_hnsw_mnestic_0_13_v3_canonical_node_v1_tag_v2_catalog_struct_map_v1_capture_v1"
        );
        for predecessor in [
            LEGACY_CATALOG_GENERATION_MARKER,
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
            MANAGED_WRITER_GENERATION_MARKER,
        ] {
            assert_ne!(CAPTURE_V1_CATALOG_GENERATION_MARKER, predecessor);
        }
    }

    #[test]
    fn base_ordinals_and_closed_relation_inventory_are_pinned() {
        assert_eq!(
            BASE_RELATIONS
                .iter()
                .map(|base| base.ordinal as u16)
                .collect::<Vec<_>>(),
            (1_u16..=16).collect::<Vec<_>>()
        );
        assert_eq!(BASE_RELATIONS.len(), 16);
        assert_eq!(DERIVED_RELATIONS.len(), 10);
        assert_eq!(
            BASE_RELATIONS.map(|base| base.relation.name),
            [
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
            ]
        );
        assert_eq!(
            DERIVED_RELATIONS.map(|child| child.relation.name),
            [
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
            ]
        );

        let names = all_physical_relations()
            .map(|relation| relation.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), 26);
        for child in DERIVED_RELATIONS {
            assert_eq!(
                child.relation.name,
                format!("{}:{}", child.owner_relation, child.local_name)
            );
        }
    }

    #[test]
    fn child_catalog_and_durable_access_are_deliberately_distinct() {
        for child in DERIVED_RELATIONS {
            assert_eq!(
                child.relation.catalog_access,
                CatalogAccess::SynthesizedIndex
            );
            assert_eq!(child.relation.durable_access, DurableAccess::Normal);
        }
        for base in BASE_RELATIONS {
            assert!(matches!(
                (base.relation.catalog_access, base.relation.durable_access),
                (CatalogAccess::Normal, DurableAccess::Normal)
                    | (CatalogAccess::ReadOnly, DurableAccess::ReadOnly)
            ));
        }
    }

    #[test]
    fn normal_index_mappings_are_exact_child_key_prefixes() {
        let expected = [
            ("node_tag:by_tag", &[1, 0][..]),
            ("node_tag_v2:by_id", &[3, 0, 1, 2][..]),
            ("node_search:by_status", &[2, 0][..]),
            ("edge:by_to", &[1, 0][..]),
        ];
        for (child_name, expected_mapping) in expected {
            let child = child(child_name);
            let IndexSpec::Normal(normal) = child.index else {
                panic!("{child_name:?} is not a normal conventional index")
            };
            assert_eq!(normal.ordered_columns, expected_mapping);
            let owner = base(child.owner_relation);
            assert_eq!(child.relation.columns.len(), normal.ordered_columns.len());
            for (child_column, owner_index) in
                child.relation.columns.iter().zip(normal.ordered_columns)
            {
                assert!(child_column.is_key);
                let owner_index = usize::try_from(*owner_index).unwrap();
                let owner_column = owner.columns[owner_index];
                assert_eq!(child_column.name, owner_column.name);
                assert_eq!(child_column.ty, owner_column.ty);
            }
        }
    }

    #[test]
    fn fts_lifecycle_manifests_are_exact() {
        for (lifecycle, extractor) in [
            ("active", "if(eq(status, \"active\"), summary)"),
            ("candidate", "if(eq(status, \"candidate\"), summary)"),
            ("archived", "if(eq(status, \"archived\"), summary)"),
        ] {
            let child = child(&format!("node_search:{lifecycle}_fts"));
            let IndexSpec::Fts(fts) = child.index else {
                panic!("{lifecycle:?} FTS child has the wrong index kind")
            };
            assert_eq!(fts.extractor, extractor);
            assert_eq!(fts.tokenizer, SIMPLE_TOKENIZER);
            assert_eq!(fts.tokenizer_filters, LOWERCASE_FILTERS);
        }
    }

    #[test]
    fn hnsw_lifecycle_manifests_bind_dimension_and_hidden_filters() {
        let source = base(HNSW_CATALOG_SPEC.dimension.relation);
        let vector_columns = source
            .columns
            .iter()
            .filter(|column| column.ty == ColumnType::F32Vector)
            .collect::<Vec<_>>();
        assert_eq!(vector_columns.len(), 1);
        assert_eq!(vector_columns[0].name, HNSW_CATALOG_SPEC.dimension.column);
        for (lifecycle, filter) in [
            ("active", "status == 'active'"),
            ("candidate", "status == 'candidate'"),
            ("archived", "status == 'archived'"),
        ] {
            let child = child(&format!("node_vec:{lifecycle}_idx"));
            let IndexSpec::Hnsw(hnsw) = child.index else {
                panic!("{lifecycle:?} HNSW child has the wrong index kind")
            };
            let catalog = hnsw.catalog;
            assert_eq!(catalog.dimension, NODE_VEC_DIMENSION);
            assert_eq!(catalog.dtype, VectorElementType::F32);
            assert_eq!(catalog.vector_fields, &[1]);
            assert_eq!(catalog.distance, HnswDistance::Cosine);
            assert_eq!(catalog.ef_construction, 200);
            assert_eq!(catalog.m_neighbours, 16);
            assert_eq!(catalog.m_max, 16);
            assert_eq!(catalog.m_max0, 32);
            assert_eq!(catalog.level_multiplier_bits, 0x3fd7_1547_652b_82fe);
            assert_eq!(
                catalog.level_multiplier_bits,
                (1.0 / 16.0_f64.ln()).to_bits()
            );
            assert_eq!(hnsw.index_filter, filter);
            assert!(!catalog.extend_candidates);
            assert!(!catalog.keep_pruned_connections);
        }
    }

    #[test]
    fn closed_spec_forbids_unmodeled_durable_features() {
        assert!(!FORBIDDEN_FEATURES.allows_lsh_indices());
        assert!(!FORBIDDEN_FEATURES.allows_column_defaults());
        assert!(!FORBIDDEN_FEATURES.allows_triggers());
        assert!(!FORBIDDEN_FEATURES.allows_descriptions());
        assert!(!FORBIDDEN_FEATURES.allows_temporal_floors());
        assert!(!FORBIDDEN_FEATURES.allows_temporary_relations());
        assert!(DERIVED_RELATIONS.iter().all(|child| matches!(
            child.index,
            IndexSpec::Normal(_) | IndexSpec::Fts(_) | IndexSpec::Hnsw(_)
        )));
    }
    #[test]
    fn episode_inventory_extends_without_broadening_the_predecessor() {
        assert_eq!(CatalogContract::Predecessor.relations().count(), 26);
        assert_eq!(CatalogContract::EpisodeV1.relations().count(), 31);
        assert_eq!(&EPISODE_BASE_RELATIONS[..16], &BASE_RELATIONS);
        assert_eq!(&EPISODE_DERIVED_RELATIONS[..10], &DERIVED_RELATIONS);
        assert_eq!(
            EPISODE_BASE_RELATIONS
                .iter()
                .map(|base| base.ordinal as u16)
                .collect::<Vec<_>>(),
            (1_u16..=20).collect::<Vec<_>>()
        );
        assert_eq!(
            EPISODE_V1_CATALOG_GENERATION_MARKER,
            "status_partitioned_hnsw_mnestic_0_13_v3_canonical_node_v1_tag_v2_catalog_struct_map_v1_episode_v1"
        );
        for marker in [
            LEGACY_CATALOG_GENERATION_MARKER,
            STRUCT_MAP_CATALOG_GENERATION_MARKER,
            CAPTURE_V1_CATALOG_GENERATION_MARKER,
            MANAGED_WRITER_GENERATION_MARKER,
        ] {
            assert_ne!(EPISODE_V1_CATALOG_GENERATION_MARKER, marker);
        }
        assert!(
            CatalogContract::Predecessor
                .relation_slot("episode_head")
                .is_none()
        );
        let IndexSpec::Fts(fts) = EPISODE_DERIVED_RELATIONS[10].index else {
            panic!("episode cue requires FTS")
        };
        assert_eq!(fts.extractor, "summary");
        assert_eq!(fts.tokenizer_filters, LOWERCASE_FILTERS);
    }
}
