//! Private transaction kernel for a fresh current conventional schema.
//!
//! This module deliberately owns no pathname, lease, staging, checkpoint, or
//! publication policy. Its caller must already own one exact fresh database
//! artifact. The only guarantee here is narrower: after preflight, every schema
//! and metadata write is staged in one write transaction, the exact catalog is
//! canonicalized, and the current marker is the transaction's final mutation.

use std::collections::BTreeMap;

use cozo::{DataValue, DbInstance, MultiTransaction, NamedRows};
use mneme_core::managed::DatabaseId;
use mneme_core::ports::{Error, Result};
use mneme_core::tagged::MAX_TAGGED_VECTOR_DIMENSION;
use mneme_core::{MAX_INCIDENT_EDGES, MAX_REMOTE_EDGES_PER_SOURCE};
use ulid::Ulid;

use crate::storage_contract::conventional_unmanaged::spec::{
    BASE_RELATIONS, CONCERN_V1_CATALOG_GENERATION_MARKER, CatalogContract, DERIVED_RELATIONS,
    EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER, SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER,
    STRUCT_MAP_CATALOG_GENERATION_MARKER, TOUCHSTONES_V1_CATALOG_GENERATION_MARKER,
    all_physical_relations,
};

#[cfg(test)]
use crate::storage_contract::conventional_unmanaged::spec::{
    CAPTURE_V1_CATALOG_GENERATION_MARKER, EPISODE_V1_CATALOG_GENERATION_MARKER,
};

use super::{
    CANONICAL_NODE_META_KEY, CANONICAL_NODE_META_VALUE, INCIDENT_EDGE_CAP_META_KEY,
    LEXICAL_PROJECTION_META_KEY, REMOTE_EDGE_SOURCE_CAP_META_KEY, VECTOR_PROJECTION_META_KEY,
};

const EXPECTED_BASE_RELATIONS: usize = 16;
const EXPECTED_DERIVED_RELATIONS: usize = 10;
const EXPECTED_PHYSICAL_RELATIONS: usize = EXPECTED_BASE_RELATIONS + EXPECTED_DERIVED_RELATIONS;
const EXPECTED_CURRENT_METADATA_ROWS: usize = 8;
const INJECTED_FAILURE_SCRIPT: &str = "?[must_be_empty] <- [[1]] :assert none";

#[derive(Debug)]
struct ValidatedCurrentSchemaRequest {
    dimension: usize,
    database_id: String,
    generation_marker: &'static str,
    catalog: CatalogContract,
}

#[derive(Clone, Copy, Debug)]
struct FailureInjection {
    after_completed_step: Option<usize>,
}

impl FailureInjection {
    const NONE: Self = Self {
        after_completed_step: None,
    };

    #[cfg(test)]
    const fn after_completed_step(step: usize) -> Self {
        Self {
            after_completed_step: Some(step),
        }
    }
}

struct TransactionSteps {
    completed: usize,
    failure: FailureInjection,
}

impl TransactionSteps {
    const fn new(failure: FailureInjection) -> Self {
        Self {
            completed: 0,
            failure,
        }
    }

    fn complete(&mut self, tx: &MultiTransaction, label: &'static str) -> Result<()> {
        self.completed += 1;
        if self.failure.after_completed_step != Some(self.completed) {
            return Ok(());
        }

        match tx.run_script(INJECTED_FAILURE_SCRIPT, BTreeMap::new()) {
            Err(error) => Err(Error::Backend(format!(
                "injected current-schema failure after {label}: {error}"
            ))),
            Ok(_) => Err(Error::Backend(format!(
                "current-schema failure injection after {label} unexpectedly succeeded"
            ))),
        }
    }
}

/// Install a fresh current conventional schema in one write transaction.
///
/// Deliberately private and unrouted: a future artifact publisher must compose
/// this kernel with fresh-file authority, crash recovery, checkpointing, and a
/// closed-source admission fence before returning a usable store.
#[cfg(test)]
pub(super) fn stage_current_schema(
    database: &DbInstance,
    dimension: usize,
    database_id: DatabaseId,
) -> Result<()> {
    let request = validate_request(dimension, database_id.get())?;
    stage_validated_current_schema(database, &request, FailureInjection::NONE)
}

/// The same closed fresh schema, with a distinct old-writer-fenced capture generation.
#[cfg(test)]
pub(super) fn stage_capture_schema(
    database: &DbInstance,
    dimension: usize,
    database_id: DatabaseId,
) -> Result<()> {
    let mut request = validate_request(dimension, database_id.get())?;
    request.generation_marker = CAPTURE_V1_CATALOG_GENERATION_MARKER;
    stage_validated_current_schema(database, &request, FailureInjection::NONE)
}

/// Fresh episode generation, including exact current-head/history/time/cue projections.
#[cfg(test)]
pub(super) fn stage_episode_schema(
    database: &DbInstance,
    dimension: usize,
    database_id: DatabaseId,
) -> Result<()> {
    let mut request = validate_request(dimension, database_id.get())?;
    request.generation_marker = EPISODE_V1_CATALOG_GENERATION_MARKER;
    request.catalog = CatalogContract::EpisodeV1;
    stage_validated_current_schema(database, &request, FailureInjection::NONE)
}

pub(super) fn stage_single_graph_schema(
    database: &DbInstance,
    dimension: usize,
    database_id: DatabaseId,
) -> Result<()> {
    let mut request = validate_request(dimension, database_id.get())?;
    request.generation_marker = SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER;
    request.catalog = CatalogContract::SingleGraphV1;
    stage_validated_current_schema(database, &request, FailureInjection::NONE)
}
pub(super) fn stage_concern_schema(
    database: &DbInstance,
    dimension: usize,
    database_id: DatabaseId,
) -> Result<()> {
    let mut request = validate_request(dimension, database_id.get())?;
    request.generation_marker = CONCERN_V1_CATALOG_GENERATION_MARKER;
    request.catalog = CatalogContract::ConcernV1;
    stage_validated_current_schema(database, &request, FailureInjection::NONE)
}

pub(super) fn stage_episode_context_schema(
    database: &DbInstance,
    dimension: usize,
    database_id: DatabaseId,
) -> Result<()> {
    let mut request = validate_request(dimension, database_id.get())?;
    request.generation_marker = EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER;
    request.catalog = CatalogContract::EpisodeContextV2;
    stage_validated_current_schema(database, &request, FailureInjection::NONE)
}

pub(super) fn stage_touchstones_schema(
    database: &DbInstance,
    dimension: usize,
    database_id: DatabaseId,
) -> Result<()> {
    let mut request = validate_request(dimension, database_id.get())?;
    request.generation_marker = TOUCHSTONES_V1_CATALOG_GENERATION_MARKER;
    request.catalog = CatalogContract::TouchstonesV1;
    stage_validated_current_schema(database, &request, FailureInjection::NONE)
}
pub(super) fn touchstones_memory_schema(dimension: usize) -> String {
    schema_statements_for(dimension, CatalogContract::TouchstonesV1).join("\n")
}

pub(super) fn episode_context_memory_schema(dimension: usize) -> String {
    schema_statements_for(dimension, CatalogContract::EpisodeContextV2).join("\n")
}

#[cfg(test)]
pub(super) fn single_graph_memory_schema(dimension: usize) -> String {
    schema_statements_for(dimension, CatalogContract::SingleGraphV1).join("\n")
}

#[cfg(test)]
const _: fn(usize) -> String = single_graph_memory_schema;

// Keep the intentionally unrouted kernel type-checked without making it a
// crate-wide API or suppressing dead-code diagnostics for this module.
#[cfg(test)]
const _: fn(&DbInstance, usize, DatabaseId) -> Result<()> = stage_current_schema;

fn validate_request(
    dimension: usize,
    raw_database_id: Ulid,
) -> Result<ValidatedCurrentSchemaRequest> {
    if !(1..=MAX_TAGGED_VECTOR_DIMENSION).contains(&dimension) {
        return Err(Error::InvalidInput(format!(
            "current schema vector dimension must be in 1..={MAX_TAGGED_VECTOR_DIMENSION}; got {dimension}"
        )));
    }

    let database_id = DatabaseId::new(raw_database_id).map_err(|error| {
        Error::InvalidInput(format!(
            "current schema database id must be non-nil: {error}"
        ))
    })?;
    let canonical = database_id.to_string();
    let reparsed = Ulid::from_string(&canonical).map_err(|_| {
        Error::InvalidInput("current schema database id did not produce canonical ULID text".into())
    })?;
    if reparsed != database_id.get() || reparsed.to_string() != canonical {
        return Err(Error::InvalidInput(
            "current schema database id did not round-trip as canonical ULID text".into(),
        ));
    }

    if BASE_RELATIONS.len() != EXPECTED_BASE_RELATIONS
        || DERIVED_RELATIONS.len() != EXPECTED_DERIVED_RELATIONS
        || all_physical_relations().count() != EXPECTED_PHYSICAL_RELATIONS
    {
        return Err(Error::Backend(
            "current schema relation inventory disagrees with the frozen 16+10 contract".into(),
        ));
    }

    Ok(ValidatedCurrentSchemaRequest {
        dimension,
        database_id: canonical,
        generation_marker: STRUCT_MAP_CATALOG_GENERATION_MARKER,
        catalog: CatalogContract::Predecessor,
    })
}

fn stage_validated_current_schema(
    database: &DbInstance,
    request: &ValidatedCurrentSchemaRequest,
    failure: FailureInjection,
) -> Result<()> {
    let transaction = database.multi_transaction(true);
    let staged = stage_transaction(&transaction, request, failure);
    match staged {
        Ok(()) => transaction.commit().map_err(|error| {
            Error::Backend(format!("commit fresh current-schema transaction: {error}"))
        }),
        Err(stage_error) => match transaction.abort() {
            Ok(()) => Err(stage_error),
            Err(abort_error) => Err(Error::Backend(format!(
                "{stage_error}; additionally, aborting the fresh current-schema transaction failed: {abort_error}"
            ))),
        },
    }
}

fn stage_transaction(
    transaction: &MultiTransaction,
    request: &ValidatedCurrentSchemaRequest,
    failure: FailureInjection,
) -> Result<()> {
    let mut steps = TransactionSteps::new(failure);

    // This rejects every relation and foreign id-zero system key before the
    // first mutation. Exact relation-counter-zero authority belongs to the
    // future fresh-artifact stage; Mnestic currently accepts a counter above
    // the maximum live id, including a nonzero counter for an empty catalog.
    transaction
        .canonicalize_relation_catalog_v1(Vec::new(), 0)
        .map_err(|error| Error::Backend(format!("preflight fresh relation catalog: {error}")))?;
    steps.complete(transaction, "empty-catalog preflight")?;

    for statement in schema_statements_for(request.dimension, request.catalog) {
        let statement = statement
            .strip_prefix('{')
            .and_then(|statement| statement.strip_suffix('}'))
            .ok_or_else(|| {
                Error::Backend(
                    "marker-free schema builder emitted an unframed imperative statement".into(),
                )
            })?;
        if statement.starts_with("::") {
            transaction
                .run_sqlite_catalog_system_operation(statement)
                .map_err(|error| {
                    Error::Backend(format!(
                        "stage fresh current-schema catalog operation: {error}"
                    ))
                })?;
        } else {
            transaction
                .run_script(statement, BTreeMap::new())
                .map_err(|error| {
                    Error::Backend(format!("stage fresh current-schema statement: {error}"))
                })?;
        }
        steps.complete(transaction, "schema statement")?;
    }

    write_initial_metadata(transaction, request)?;
    steps.complete(transaction, "mandatory metadata")?;

    let relation_names = request
        .catalog
        .relations()
        .map(|relation| relation.name.to_owned())
        .collect::<Vec<_>>();
    let expected_physical_relations =
        request.catalog.bases().len() + request.catalog.derived().len();
    if relation_names.len() != expected_physical_relations {
        return Err(Error::Backend(
            "current schema lost its exact canonicalization set".into(),
        ));
    }
    transaction
        .canonicalize_relation_catalog_v1(relation_names, expected_physical_relations)
        .map_err(|error| {
            Error::Backend(format!(
                "canonicalize fresh current-schema relation catalog: {error}"
            ))
        })?;
    steps.complete(transaction, "catalog canonicalization")?;

    // This is deliberately the final mutation in the transaction. Everything
    // below is a bounded read of state staged above.
    write_current_generation_marker(transaction, request)?;
    steps.complete(transaction, "current generation marker")?;

    verify_current_metadata(transaction, request)?;
    steps.complete(transaction, "current metadata reread")?;
    verify_permanent_guard(transaction)?;
    steps.complete(transaction, "permanent guard reread")?;
    verify_retired_tag_guard(transaction)?;
    steps.complete(transaction, "retired tag guard reread")?;
    Ok(())
}

fn write_initial_metadata(
    transaction: &MultiTransaction,
    request: &ValidatedCurrentSchemaRequest,
) -> Result<()> {
    let metadata = required_metadata(request, false);
    let mut params = BTreeMap::new();
    let mut input_rows = Vec::with_capacity(metadata.len());
    for (index, (key, value)) in metadata.into_iter().enumerate() {
        let key_param = format!("key_{index}");
        let value_param = format!("value_{index}");
        params.insert(key_param.clone(), data_string(&key));
        params.insert(value_param.clone(), data_string(&value));
        input_rows.push(format!("[${key_param}, ${value_param}]"));
    }
    let script = format!(
        "?[k, v] <- [{}] :put meta {{k => v}}",
        input_rows.join(", ")
    );
    transaction.run_script(&script, params).map_err(|error| {
        Error::Backend(format!(
            "write fresh current-schema mandatory metadata: {error}"
        ))
    })?;
    Ok(())
}

fn write_current_generation_marker(
    transaction: &MultiTransaction,
    request: &ValidatedCurrentSchemaRequest,
) -> Result<()> {
    let mut params = BTreeMap::new();
    params.insert("key".into(), data_string(VECTOR_PROJECTION_META_KEY));
    params.insert("value".into(), data_string(request.generation_marker));
    transaction
        .run_script("?[k, v] <- [[$key, $value]] :put meta {k => v}", params)
        .map_err(|error| {
            Error::Backend(format!(
                "write fresh current-schema generation marker: {error}"
            ))
        })?;
    Ok(())
}

fn required_metadata(
    request: &ValidatedCurrentSchemaRequest,
    include_generation: bool,
) -> BTreeMap<String, String> {
    let mut expected = BTreeMap::from([
        ("db_id".to_owned(), request.database_id.clone()),
        ("dim".to_owned(), request.dimension.to_string()),
        (
            INCIDENT_EDGE_CAP_META_KEY.to_owned(),
            MAX_INCIDENT_EDGES.to_string(),
        ),
        (
            REMOTE_EDGE_SOURCE_CAP_META_KEY.to_owned(),
            MAX_REMOTE_EDGES_PER_SOURCE.to_string(),
        ),
        (
            LEXICAL_PROJECTION_META_KEY.to_owned(),
            "complete".to_owned(),
        ),
        (
            if matches!(
                request.catalog,
                CatalogContract::EpisodeContextV2 | CatalogContract::TouchstonesV1
            ) {
                crate::canonical_node_contract::EPISODE_CONTEXT_META_KEY
            } else if matches!(
                request.catalog,
                CatalogContract::SingleGraphV1 | CatalogContract::ConcernV1
            ) {
                crate::canonical_node_contract::SINGLE_GRAPH_META_KEY
            } else {
                CANONICAL_NODE_META_KEY
            }
            .to_owned(),
            if matches!(
                request.catalog,
                CatalogContract::EpisodeContextV2 | CatalogContract::TouchstonesV1
            ) {
                crate::canonical_node_contract::EPISODE_CONTEXT_META_VALUE
            } else if matches!(
                request.catalog,
                CatalogContract::SingleGraphV1 | CatalogContract::ConcernV1
            ) {
                crate::canonical_node_contract::SINGLE_GRAPH_META_VALUE
            } else {
                CANONICAL_NODE_META_VALUE
            }
            .to_owned(),
        ),
        (
            crate::tag_projection::META_KEY.to_owned(),
            crate::tag_projection::META_VALUE.to_owned(),
        ),
    ]);
    if include_generation {
        expected.insert(
            VECTOR_PROJECTION_META_KEY.to_owned(),
            request.generation_marker.to_owned(),
        );
    }
    expected
}

fn verify_current_metadata(
    transaction: &MultiTransaction,
    request: &ValidatedCurrentSchemaRequest,
) -> Result<()> {
    let rows = transaction
        .run_script("?[k, v] := *meta{k, v} :order k :limit 9", BTreeMap::new())
        .map_err(|error| Error::Backend(format!("reread fresh current metadata: {error}")))?;
    let actual = exact_string_map(&rows, "fresh current metadata")?;
    let expected = required_metadata(request, true);
    if rows.next.is_some() || actual.len() != EXPECTED_CURRENT_METADATA_ROWS || actual != expected {
        return Err(Error::Backend(
            "fresh current metadata did not exactly match the mandatory eight-row contract".into(),
        ));
    }
    Ok(())
}

fn verify_permanent_guard(transaction: &MultiTransaction) -> Result<()> {
    let rows = transaction
        .run_script(
            "?[fence, generation] := *mneme_reembed_shadow_node_vec{fence, generation} :limit 2",
            BTreeMap::new(),
        )
        .map_err(|error| Error::Backend(format!("reread fresh permanent guard: {error}")))?;
    if rows.next.is_some()
        || rows.rows.len() != 1
        || rows.rows[0].len() != 2
        || data_as_str(&rows.rows[0][0]) != Some(crate::vector_projection::GUARD_KEY)
        || data_as_str(&rows.rows[0][1])
            != Some(
                crate::storage_contract::conventional_unmanaged::spec::PERMANENT_VECTOR_GUARD_VALUE,
            )
    {
        return Err(Error::Backend(
            "fresh current permanent guard did not match its exact one-row contract".into(),
        ));
    }
    Ok(())
}

fn verify_retired_tag_guard(transaction: &MultiTransaction) -> Result<()> {
    let rows = transaction
        .run_script("?[id] := *node_tag{id} :limit 1", BTreeMap::new())
        .map_err(|error| Error::Backend(format!("reread fresh retired tag guard: {error}")))?;
    if rows.next.is_some() || !rows.rows.is_empty() {
        return Err(Error::Backend(
            "fresh current retired tag guard was not exactly empty".into(),
        ));
    }
    Ok(())
}

fn exact_string_map(rows: &NamedRows, label: &'static str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for row in &rows.rows {
        let [key, value] = row.as_slice() else {
            return Err(Error::Backend(format!(
                "{label} returned a row with the wrong width"
            )));
        };
        let Some(key) = data_as_str(key) else {
            return Err(Error::Backend(format!("{label} returned a non-string key")));
        };
        let Some(value) = data_as_str(value) else {
            return Err(Error::Backend(format!(
                "{label} returned a non-string value"
            )));
        };
        if values.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(Error::Backend(format!("{label} returned a duplicate key")));
        }
    }
    Ok(values)
}

fn data_string(value: &str) -> DataValue {
    DataValue::Str(value.into())
}

fn data_as_str(value: &DataValue) -> Option<&str> {
    match value {
        DataValue::Str(value) => Some(value.as_str()),
        _ => None,
    }
}

/// Exact schema DDL without any generation or companion metadata marker.
///
/// The old conventional constructor and this kernel intentionally share these
/// statements so their physical contract cannot drift. Only their publication
/// metadata differs.
#[cfg(test)]
pub(super) fn marker_free_schema_script(dimension: usize) -> String {
    marker_free_schema_statements(dimension).join("\n")
}

fn schema_statements_for(dimension: usize, catalog: CatalogContract) -> Vec<String> {
    let mut statements = marker_free_schema_statements(dimension);
    if matches!(
        catalog,
        CatalogContract::EpisodeV1
            | CatalogContract::SingleGraphV1
            | CatalogContract::ConcernV1
            | CatalogContract::EpisodeContextV2
            | CatalogContract::TouchstonesV1
    ) {
        statements.extend([
            "{:create episode_head {root: String => head: String, revision: Int, recorded_at: Int}}".into(),
            "{:create episode_history {root: String, revision: Int => edition: String}}".into(),
            "{:create episode_time {axis: String, thread: String, at: Int, root: String => head: String, occurred_start: Int?, occurred_end: Int?}}".into(),
            "{:create episode_search {root: String => head: String, summary: String, thread: String, recorded_at: Int, occurred_start: Int?, occurred_end: Int?}}".into(),
            "{::fts create episode_search:fts {extractor: summary, tokenizer: Simple, filters: [Lowercase]}}".into(),
        ]);
    }
    if matches!(
        catalog,
        CatalogContract::SingleGraphV1
            | CatalogContract::ConcernV1
            | CatalogContract::EpisodeContextV2
            | CatalogContract::TouchstonesV1
    ) {
        statements.retain(|s| {
            !s.contains("node_vec:candidate_idx") && !s.contains("node_search:candidate_fts")
        });
    }
    if matches!(
        catalog,
        CatalogContract::ConcernV1
            | CatalogContract::EpisodeContextV2
            | CatalogContract::TouchstonesV1
    ) {
        statements.extend([
            "{:create concern {lo: String, hi: String, kind: String => data: String}}".into(),
            "{::index create concern:by_hi {hi, lo, kind}}".into(),
        ]);
    }
    if catalog == CatalogContract::TouchstonesV1 {
        statements.extend([
            "{:create touchstone {owner: String => data: String}}".into(),
            "{:create touchstone_target {target: String, owner: String}}".into(),
            "{::index create touchstone_target:by_owner {owner, target}}".into(),
        ]);
    }
    statements
}

fn marker_free_schema_statements(dimension: usize) -> Vec<String> {
    let mut statements = vec![
        "{:create node {id: String => data: String, status: String}}".into(),
        "{:create node_tag_v2 {tag: String, status: String, sample_hash: Int, id: String}}"
            .into(),
        "{::index create node_tag_v2:by_id {id}}".into(),
        "{:create node_tag {id: String, tag: String}}".into(),
        "{::index create node_tag:by_tag {tag}}".into(),
        "{::access_level read_only node_tag}".into(),
        "{:create node_search {id: String => summary: String, status: String}}".into(),
        "{::index create node_search:by_status {status}}".into(),
        "{::fts create node_search:active_fts {extractor: summary, extract_filter: status == 'active', tokenizer: Simple, filters: [Lowercase]}}".into(),
        "{::fts create node_search:candidate_fts {extractor: summary, extract_filter: status == 'candidate', tokenizer: Simple, filters: [Lowercase]}}".into(),
        "{::fts create node_search:archived_fts {extractor: summary, extract_filter: status == 'archived', tokenizer: Simple, filters: [Lowercase]}}".into(),
        format!("{{:create node_vec {{id: String => e: <F32; {dimension}>, status: String}}}}"),
        format!("{{::hnsw create node_vec:active_idx {{dim: {dimension}, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'active'}}}}"),
        format!("{{::hnsw create node_vec:candidate_idx {{dim: {dimension}, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'candidate'}}}}"),
        format!("{{::hnsw create node_vec:archived_idx {{dim: {dimension}, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'archived'}}}}"),
        "{:create edge {from: String, to: String => weight: Float, kind: String, last_reinforced: Int, trials: Int, interference: Int}}".into(),
        "{::index create edge:by_to {to}}".into(),
        "{:create edge_anchor {from: String, to: String => start: Int, end: Int}}".into(),
        "{:create contradiction {lo: String, hi: String => observations: Int, first_seen: Int, last_seen: Int, resolution: String?}}".into(),
        "{:create merge_candidate {lo: String, hi: String => observations: Int, first_seen: Int, last_seen: Int, resolution: String?}}".into(),
        "{:create full_merge_commit {lo: String, hi: String => winner: String, loser: String, applied_at: Int}}".into(),
        "{:create supersede_commit {lo: String, hi: String => winner: String, loser: String, applied_at: Int}}".into(),
        "{:create remote_edge {from: String, target_db: String, target: String => weight: Float}}".into(),
        "{:create feedback_retry {key: String => fingerprint: String, applied_at: Int}}".into(),
        "{:create feedback_retry_order {epoch: String, sequence: Int, key: String => marker: Bool}}".into(),
        "{:create meta {k: String => v: String}}".into(),
    ];
    statements.extend(
        crate::vector_projection::conventional_guard_install_script()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned),
    );
    statements
}

#[cfg(test)]
fn stage_current_schema_failing_after(
    database: &DbInstance,
    dimension: usize,
    database_id: DatabaseId,
    completed_step: usize,
) -> Result<()> {
    let total_steps = total_transaction_steps(dimension);
    if completed_step == 0 || completed_step > total_steps {
        return Err(Error::InvalidInput(format!(
            "current-schema failure boundary must be in 1..={total_steps}; got {completed_step}"
        )));
    }
    let request = validate_request(dimension, database_id.get())?;
    stage_validated_current_schema(
        database,
        &request,
        FailureInjection::after_completed_step(completed_step),
    )
}

#[cfg(test)]
fn pre_marker_transaction_steps(dimension: usize) -> usize {
    // Empty-catalog preflight + every schema statement + metadata + exact-name
    // canonicalization. The following step writes the marker.
    1 + marker_free_schema_statements(dimension).len() + 2
}

#[cfg(test)]
fn total_transaction_steps(dimension: usize) -> usize {
    pre_marker_transaction_steps(dimension) + 1 + 3
}

#[cfg(test)]
mod tests;
