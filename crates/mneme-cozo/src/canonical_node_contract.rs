/// Canonical metadata gate for the complete sealed Node wire contract.
///
/// The generation covers bounded BodyRef/WebUrl values, full Git object ids,
/// bounded nonblank summaries, reclaimable canonical tag backing, ordered and
/// unique DerivedSources with at most 64 entries, canonical finite unit scalars,
/// and keyed id/status projection agreement. Any change to those rules requires
/// another explicit leased audit and generation bump.
pub(crate) const META_KEY: &str = "canonical_node_contract_v1";
pub(crate) const META_VALUE: &str = "bounded_refs_summary_arc_tags_derived64_unit_scalars_v1";

/// Maximum serialized UTF-8 size of one canonical Node JSON value.
///
/// The storage backend necessarily receives an invalid raw value before it can
/// inspect its length, but rejects it before allocating typed Node substructure
/// or invoking serde.
pub const MAX_CANONICAL_NODE_JSON_BYTES: usize = 256 * 1024;

/// Historical rows predate a persisted byte-length projection, and the direct
/// scan API must decode each complete stored value before Rust can enforce the
/// wire cap. Audit one row per response so one corrupt legacy value cannot be
/// multiplied by a page width. This is deliberately conservative archaeology;
/// future formats must persist an authenticated length at write time.
#[cfg(test)]
pub(crate) const AUDIT_PAGE_SIZE: usize = 1;

/// Successor contract: old literals above are retained for named predecessor admission.
pub(crate) const SINGLE_GRAPH_META_KEY: &str = "canonical_node_contract_v2";
pub(crate) const SINGLE_GRAPH_META_VALUE: &str =
    "bounded_refs_summary_arc_tags_derived64_unit_scalars_active_archived_source_codec_v2";

/// Context-bearing episode successor. Historical contracts above remain frozen.
pub(crate) const EPISODE_CONTEXT_META_KEY: &str = "canonical_node_contract_v3";
pub(crate) const EPISODE_CONTEXT_META_VALUE: &str = "bounded_refs_summary_arc_tags_derived64_unit_scalars_active_archived_source_codec_occurrence_contexts_v3";
