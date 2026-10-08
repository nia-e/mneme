//! Closed-world validation of Mneme's conventional unmanaged catalog-visible manifest.
//!
//! This is necessary but deliberately insufficient for snapshot publication.
//! Mnestic's system commands omit relation ids and nested-child ids, synthesize
//! child access as `index`, and omit HNSW document filters. A bounded raw
//! `RelationHandle` preflight must additionally authenticate those durable fields
//! before an artifact can claim a complete physical conventional catalog.

use std::collections::{BTreeMap, BTreeSet};

use cozo::{DataValue, NamedRows};
use serde_json::{Map, Value};

#[cfg(test)]
use super::spec::PERMANENT_VECTOR_GUARD_VALUE;
use super::spec::{
    BASE_RELATIONS, BaseRelationSpec, CatalogContract, ColumnType, FtsIndexSpec, HNSW_CATALOG_SPEC,
    HnswCatalogSpec, IndexSpec, RelationSpec, SEMANTIC_OBLIGATIONS, SemanticObligations,
};

#[cfg(test)]
use super::spec::{DERIVED_RELATIONS, all_physical_relations};

const CATALOG_HEADERS: [&str; 4] = ["name", "type", "relations", "config"];
const RELATION_HEADERS: [&str; 9] = [
    "name",
    "arity",
    "access_level",
    "n_keys",
    "n_non_keys",
    "n_put_triggers",
    "n_rm_triggers",
    "n_replace_triggers",
    "description",
];
const COLUMN_HEADERS: [&str; 6] = [
    "column",
    "is_key",
    "index",
    "type",
    "has_default",
    "default_expr",
];
const TRIGGER_HEADERS: [&str; 3] = ["type", "idx", "trigger"];
const HNSW_CONFIG_KEYS: [&str; 11] = [
    "vec_dim",
    "dtype",
    "vec_fields",
    "distance",
    "ef_construction",
    "m_neighbours",
    "m_max",
    "m_max0",
    "level_multiplier",
    "extend_candidates",
    "keep_pruned_connections",
];

#[derive(Clone, Copy, Debug)]
#[cfg(test)]
pub(crate) struct NormalIndexExpectation<'a> {
    local_name: &'a str,
    child_relation: &'a str,
    ordered_columns: &'a [usize],
}

#[cfg(test)]
impl<'a> NormalIndexExpectation<'a> {
    pub(crate) const fn new(
        local_name: &'a str,
        child_relation: &'a str,
        ordered_columns: &'a [usize],
    ) -> Self {
        Self {
            local_name,
            child_relation,
            ordered_columns,
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[cfg(test)]
pub(crate) struct HnswIndexExpectation<'a> {
    local_name: &'a str,
    child_relation: &'a str,
    dim: usize,
}

#[derive(Clone, Copy, Debug)]
#[cfg(test)]
struct FtsIndexExpectation<'a> {
    local_name: &'a str,
    child_relation: &'a str,
    config: FtsIndexSpec,
}

#[cfg(test)]
impl<'a> FtsIndexExpectation<'a> {
    const fn new(local_name: &'a str, child_relation: &'a str, config: FtsIndexSpec) -> Self {
        Self {
            local_name,
            child_relation,
            config,
        }
    }
}

#[cfg(test)]
impl<'a> HnswIndexExpectation<'a> {
    pub(crate) const fn new(local_name: &'a str, child_relation: &'a str, dim: usize) -> Self {
        Self {
            local_name,
            child_relation,
            dim,
        }
    }
}

enum ExpectedIndex {
    Normal {
        child_relation: String,
        ordered_columns: Vec<u64>,
    },
    Hnsw {
        child_relation: String,
        dim: u64,
        config: HnswCatalogSpec,
    },
    Fts {
        child_relation: String,
        config: FtsIndexSpec,
    },
}

/// Validate an entire `::indices <relation>` result, rejecting any malformed,
/// duplicated, missing, or additional catalog entry.
///
/// This is intentionally named for the catalog-visible contract: Mnestic omits
/// the HNSW document filter from these rows, so success does not authenticate it.
#[cfg(test)]
pub(crate) fn validate_catalog_visible_indices<'a>(
    catalog: &NamedRows,
    normal: &[NormalIndexExpectation<'a>],
    hnsw: &[HnswIndexExpectation<'a>],
) -> Result<(), String> {
    validate_catalog_visible_indices_with_fts(catalog, normal, hnsw, &[])
}

#[cfg(test)]
fn validate_catalog_visible_indices_with_fts<'a>(
    catalog: &NamedRows,
    normal: &[NormalIndexExpectation<'a>],
    hnsw: &[HnswIndexExpectation<'a>],
    fts: &[FtsIndexExpectation<'a>],
) -> Result<(), String> {
    let mut expected = BTreeMap::new();
    for item in normal {
        validate_index_identity(item.local_name, item.child_relation)?;
        if item.ordered_columns.is_empty() {
            return Err(format!(
                "normal index expectation {:?} has no columns",
                item.local_name
            ));
        }
        if !all_unique(item.ordered_columns.iter().copied()) {
            return Err(format!(
                "normal index expectation {:?} repeats a column",
                item.local_name
            ));
        }
        if expected
            .insert(
                item.local_name.to_owned(),
                ExpectedIndex::Normal {
                    child_relation: item.child_relation.to_owned(),
                    ordered_columns: item
                        .ordered_columns
                        .iter()
                        .copied()
                        .map(u64::try_from)
                        .collect::<Result<_, _>>()
                        .map_err(|_| {
                            format!(
                                "normal index expectation {:?} has a column outside u64",
                                item.local_name
                            )
                        })?,
                },
            )
            .is_some()
        {
            return Err(format!(
                "duplicate expected index name {:?}",
                item.local_name
            ));
        }
    }
    for item in hnsw {
        validate_index_identity(item.local_name, item.child_relation)?;
        if item.dim == 0 {
            return Err(format!(
                "HNSW expectation {:?} has zero dimension",
                item.local_name
            ));
        }
        if expected
            .insert(
                item.local_name.to_owned(),
                ExpectedIndex::Hnsw {
                    child_relation: item.child_relation.to_owned(),
                    dim: u64::try_from(item.dim).map_err(|_| {
                        format!(
                            "HNSW expectation {:?} has a dimension outside u64",
                            item.local_name
                        )
                    })?,
                    config: HNSW_CATALOG_SPEC,
                },
            )
            .is_some()
        {
            return Err(format!(
                "duplicate expected index name {:?}",
                item.local_name
            ));
        }
    }
    for item in fts {
        validate_index_identity(item.local_name, item.child_relation)?;
        if expected
            .insert(
                item.local_name.to_owned(),
                ExpectedIndex::Fts {
                    child_relation: item.child_relation.to_owned(),
                    config: item.config,
                },
            )
            .is_some()
        {
            return Err(format!(
                "duplicate expected index name {:?}",
                item.local_name
            ));
        }
    }

    validate_expected_indices(catalog, expected)
}

fn validate_expected_indices(
    catalog: &NamedRows,
    expected: BTreeMap<String, ExpectedIndex>,
) -> Result<(), String> {
    if catalog.headers.len() != CATALOG_HEADERS.len()
        || !catalog
            .headers
            .iter()
            .map(String::as_str)
            .eq(CATALOG_HEADERS)
    {
        return Err(format!(
            "unexpected ::indices headers: {:?}",
            catalog.headers
        ));
    }
    if catalog.next.is_some() {
        return Err("unexpected chained ::indices result".into());
    }
    if catalog.rows.len() != expected.len() {
        return Err(format!(
            "unexpected ::indices row count: got {}, expected {}",
            catalog.rows.len(),
            expected.len()
        ));
    }

    let mut seen = BTreeSet::new();
    for (row_number, row) in catalog.rows.iter().enumerate() {
        if row.len() != CATALOG_HEADERS.len() {
            return Err(format!(
                "malformed ::indices row {row_number}: got {} fields, expected {}",
                row.len(),
                CATALOG_HEADERS.len()
            ));
        }

        let local_name = data_string(&row[0], "index name")?;
        if !seen.insert(local_name) {
            return Err(format!("duplicate ::indices row for {local_name:?}"));
        }
        let Some(expectation) = expected.get(local_name) else {
            return Err(format!("unexpected ::indices row for {local_name:?}"));
        };

        let kind = data_string(&row[1], "index type")?;
        match expectation {
            ExpectedIndex::Normal {
                child_relation,
                ordered_columns,
            } => {
                if kind != "normal" {
                    return Err(format!(
                        "index {local_name:?} has type {kind:?}, expected \"normal\""
                    ));
                }
                validate_child_relation(local_name, &row[2], child_relation)?;
                validate_normal_config(local_name, &row[3], ordered_columns)?;
            }
            ExpectedIndex::Hnsw {
                child_relation,
                dim,
                config,
            } => {
                if kind != "hnsw" {
                    return Err(format!(
                        "index {local_name:?} has type {kind:?}, expected \"hnsw\""
                    ));
                }
                validate_child_relation(local_name, &row[2], child_relation)?;
                validate_hnsw_config(local_name, &row[3], *dim, *config)?;
            }
            ExpectedIndex::Fts {
                child_relation,
                config,
            } => {
                if kind != "fts" {
                    return Err(format!(
                        "index {local_name:?} has type {kind:?}, expected \"fts\""
                    ));
                }
                validate_child_relation(local_name, &row[2], child_relation)?;
                validate_fts_config(local_name, &row[3], *config)?;
            }
        }
    }

    if let Some(missing) = expected.keys().find(|name| !seen.contains(name.as_str())) {
        return Err(format!("missing ::indices row for {missing:?}"));
    }
    Ok(())
}

fn validate_fts_config(
    local_name: &str,
    value: &DataValue,
    expected: FtsIndexSpec,
) -> Result<(), String> {
    let object = json_object(value, local_name)?;
    validate_exact_keys(
        local_name,
        object,
        &["extractor", "tokenizer", "tokenizer_filters"],
    )?;
    expect_json_string(local_name, object, "extractor", expected.extractor)?;
    validate_tokenizer_config(
        local_name,
        config_field(local_name, object, "tokenizer")?,
        "tokenizer",
        expected.tokenizer,
    )?;
    let filters = config_field(local_name, object, "tokenizer_filters")?
        .as_array()
        .ok_or_else(|| {
            format!("index {local_name:?} config field \"tokenizer_filters\" is not an array")
        })?;
    if filters.len() != expected.tokenizer_filters.len() {
        return Err(format!(
            "index {local_name:?} has {} tokenizer filters, expected {}",
            filters.len(),
            expected.tokenizer_filters.len()
        ));
    }
    for (index, (actual, expected)) in filters.iter().zip(expected.tokenizer_filters).enumerate() {
        validate_tokenizer_config(
            local_name,
            actual,
            &format!("tokenizer_filters[{index}]"),
            *expected,
        )?;
    }
    Ok(())
}

fn validate_tokenizer_config(
    local_name: &str,
    value: &Value,
    field: &str,
    expected: super::spec::TokenizerSpec,
) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("index {local_name:?} config field {field:?} is not an object"))?;
    validate_exact_keys(local_name, object, &["args", "name"])?;
    let args = object
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!("index {local_name:?} config field {field:?}.args is not an array")
        })?;
    let argument_count = u64::try_from(args.len()).map_err(|_| {
        format!("index {local_name:?} config field {field:?}.args length does not fit u64")
    })?;
    if argument_count != expected.argument_count {
        if expected.argument_count == 0 {
            return Err(format!(
                "index {local_name:?} config field {field:?}.args is not empty"
            ));
        }
        return Err(format!(
            "index {local_name:?} config field {field:?}.args has length {}, expected {}",
            args.len(),
            expected.argument_count
        ));
    }
    let name = object.get("name").and_then(Value::as_str).ok_or_else(|| {
        format!("index {local_name:?} config field {field:?}.name is not a string")
    })?;
    if name != expected.name {
        return Err(format!(
            "index {local_name:?} config field {field:?}.name is {name:?}, expected {:?}",
            expected.name
        ));
    }
    Ok(())
}

fn validate_index_identity(local_name: &str, child_relation: &str) -> Result<(), String> {
    if local_name.is_empty() || local_name.contains(':') {
        return Err(format!("invalid local index name {local_name:?}"));
    }
    let Some((base_relation, child_local_name)) = child_relation.rsplit_once(':') else {
        return Err(format!(
            "index {local_name:?} has invalid full child relation {child_relation:?}"
        ));
    };
    if base_relation.is_empty() || child_local_name != local_name {
        return Err(format!(
            "index {local_name:?} does not match full child relation {child_relation:?}"
        ));
    }
    Ok(())
}

fn validate_child_relation(
    local_name: &str,
    value: &DataValue,
    expected: &str,
) -> Result<(), String> {
    match value {
        DataValue::List(relations) if relations.len() == 1 => {
            let actual = data_string(&relations[0], "child relation")?;
            if actual == expected {
                Ok(())
            } else {
                Err(format!(
                    "index {local_name:?} has child relation {actual:?}, expected {expected:?}"
                ))
            }
        }
        _ => Err(format!(
            "index {local_name:?} has malformed child relation list"
        )),
    }
}

fn validate_normal_config(
    local_name: &str,
    value: &DataValue,
    expected_columns: &[u64],
) -> Result<(), String> {
    let object = json_object(value, local_name)?;
    validate_exact_keys(local_name, object, &["indices"])?;
    let columns = json_u64_array(
        object
            .get("indices")
            .ok_or_else(|| format!("index {local_name:?} is missing config field \"indices\""))?,
        local_name,
        "indices",
    )?;
    if !all_unique(columns.iter().copied()) {
        return Err(format!("index {local_name:?} repeats an indexed column"));
    }
    if columns != expected_columns {
        return Err(format!(
            "index {local_name:?} has ordered columns {columns:?}, expected {expected_columns:?}"
        ));
    }
    Ok(())
}

fn validate_hnsw_config(
    local_name: &str,
    value: &DataValue,
    dim: u64,
    expected: HnswCatalogSpec,
) -> Result<(), String> {
    let object = json_object(value, local_name)?;
    validate_exact_keys(local_name, object, &HNSW_CONFIG_KEYS)?;

    expect_json_u64(local_name, object, "vec_dim", dim)?;
    expect_json_string(local_name, object, "dtype", expected.dtype.catalog_name())?;
    let vec_fields = json_u64_array(
        config_field(local_name, object, "vec_fields")?,
        local_name,
        "vec_fields",
    )?;
    if vec_fields != expected.vector_fields {
        return Err(format!(
            "index {local_name:?} has vec_fields {vec_fields:?}, expected {:?}",
            expected.vector_fields
        ));
    }
    expect_json_string(
        local_name,
        object,
        "distance",
        expected.distance.catalog_name(),
    )?;
    expect_json_u64(
        local_name,
        object,
        "ef_construction",
        expected.ef_construction,
    )?;
    expect_json_u64(local_name, object, "m_neighbours", expected.m_neighbours)?;
    expect_json_u64(local_name, object, "m_max", expected.m_max)?;
    expect_json_u64(local_name, object, "m_max0", expected.m_max0)?;

    let level_multiplier = config_field(local_name, object, "level_multiplier")?
        .as_f64()
        .ok_or_else(|| {
            format!("index {local_name:?} config field \"level_multiplier\" is not a number")
        })?;
    if level_multiplier.to_bits() != expected.level_multiplier_bits {
        let expected_level_multiplier = f64::from_bits(expected.level_multiplier_bits);
        return Err(format!(
            "index {local_name:?} has level_multiplier {level_multiplier:?}, expected {expected_level_multiplier:?}"
        ));
    }

    expect_json_bool(
        local_name,
        object,
        "extend_candidates",
        expected.extend_candidates,
    )?;
    expect_json_bool(
        local_name,
        object,
        "keep_pruned_connections",
        expected.keep_pruned_connections,
    )?;
    Ok(())
}

fn json_object<'a>(
    value: &'a DataValue,
    local_name: &str,
) -> Result<&'a Map<String, Value>, String> {
    match value {
        DataValue::Json(json) => json
            .0
            .as_object()
            .ok_or_else(|| format!("index {local_name:?} config JSON is not an object")),
        _ => Err(format!("index {local_name:?} config is not a JSON object")),
    }
}

fn validate_exact_keys(
    local_name: &str,
    object: &Map<String, Value>,
    expected_keys: &[&str],
) -> Result<(), String> {
    let actual: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = expected_keys.iter().copied().collect();
    if actual != expected {
        return Err(format!(
            "index {local_name:?} has config keys {actual:?}, expected {expected:?}"
        ));
    }
    Ok(())
}

fn config_field<'a>(
    local_name: &str,
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a Value, String> {
    object
        .get(field)
        .ok_or_else(|| format!("index {local_name:?} is missing config field {field:?}"))
}

fn json_u64(value: &Value, local_name: &str, field: &str) -> Result<u64, String> {
    value.as_u64().ok_or_else(|| {
        format!("index {local_name:?} config field {field:?} is not an unsigned integer")
    })
}

fn json_u64_array(value: &Value, local_name: &str, field: &str) -> Result<Vec<u64>, String> {
    let values = value
        .as_array()
        .ok_or_else(|| format!("index {local_name:?} config field {field:?} is not an array"))?;
    values
        .iter()
        .map(|value| json_u64(value, local_name, field))
        .collect()
}

fn expect_json_u64(
    local_name: &str,
    object: &Map<String, Value>,
    field: &str,
    expected: u64,
) -> Result<(), String> {
    let actual = json_u64(config_field(local_name, object, field)?, local_name, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "index {local_name:?} config field {field:?} is {actual}, expected {expected}"
        ))
    }
}

fn expect_json_string(
    local_name: &str,
    object: &Map<String, Value>,
    field: &str,
    expected: &str,
) -> Result<(), String> {
    let actual = config_field(local_name, object, field)?
        .as_str()
        .ok_or_else(|| format!("index {local_name:?} config field {field:?} is not a string"))?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "index {local_name:?} config field {field:?} is {actual:?}, expected {expected:?}"
        ))
    }
}

fn expect_json_bool(
    local_name: &str,
    object: &Map<String, Value>,
    field: &str,
    expected: bool,
) -> Result<(), String> {
    let actual = config_field(local_name, object, field)?
        .as_bool()
        .ok_or_else(|| format!("index {local_name:?} config field {field:?} is not a boolean"))?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "index {local_name:?} config field {field:?} is {actual}, expected {expected}"
        ))
    }
}

fn data_string<'a>(value: &'a DataValue, description: &str) -> Result<&'a str, String> {
    match value {
        DataValue::Str(value) => Ok(value.as_str()),
        _ => Err(format!(
            "malformed ::indices {description}: expected string"
        )),
    }
}

fn all_unique<T: Ord>(items: impl IntoIterator<Item = T>) -> bool {
    let mut seen = BTreeSet::new();
    items.into_iter().all(|item| seen.insert(item))
}

/// Bounded catalog results collected by a read-only caller. `trigger_catalogs`
/// contains only base relation results: Mnestic's `::show_triggers` grammar
/// cannot name colon-bearing derived relations. Their zero trigger counts are
/// still authenticated by the closed `::relations` result.
#[allow(dead_code)]
pub(crate) struct CatalogResults<'a> {
    pub(crate) relations: &'a NamedRows,
    pub(crate) columns: &'a BTreeMap<String, NamedRows>,
    pub(crate) indices: &'a BTreeMap<String, NamedRows>,
    pub(crate) trigger_catalogs: &'a BTreeMap<String, NamedRows>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogVisibleManifest {
    pub(crate) vector_dimension: u64,
    pub(crate) page_relations: &'static [BaseRelationSpec],
    /// Presentation-safe child names. Raw-only expected facts stay in `spec`.
    pub(crate) derived_relations: Vec<&'static str>,
    pub(crate) semantic_obligations: SemanticObligations,
    /// Always false: `::relations` synthesizes `index` for child relations.
    pub(crate) child_access_levels_catalog_attested: bool,
    /// Always false: the catalog commands omit relation ids and nested-child ids.
    pub(crate) relation_identities_catalog_attested: bool,
    /// Always false with Mnestic 0.13: `::indices` omits HNSW document filters.
    pub(crate) hnsw_status_filters_catalog_attested: bool,
}

/// Validate the complete catalog-visible conventional manifest.
///
/// This validates the system-command presentation only. In particular, callers
/// must separately validate durable relation identity/linkage, actual child
/// access levels, the returned semantic row obligations, and the three HNSW
/// status filters. Raw key/value equality alone does not establish those
/// semantics.
#[allow(dead_code)]
pub(crate) fn validate_catalog_visible_manifest(
    results: CatalogResults<'_>,
) -> Result<CatalogVisibleManifest, String> {
    validate_catalog_visible_manifest_for(results, CatalogContract::Predecessor)
}

pub(crate) fn validate_episode_catalog_visible_manifest(
    results: CatalogResults<'_>,
) -> Result<CatalogVisibleManifest, String> {
    validate_catalog_visible_manifest_for(results, CatalogContract::EpisodeV1)
}

pub(crate) fn validate_single_graph_catalog_visible_manifest(
    results: CatalogResults<'_>,
) -> Result<CatalogVisibleManifest, String> {
    validate_catalog_visible_manifest_for(results, CatalogContract::SingleGraphV1)
}
pub(crate) fn validate_concern_catalog_visible_manifest(
    results: CatalogResults<'_>,
) -> Result<CatalogVisibleManifest, String> {
    validate_catalog_visible_manifest_for(results, CatalogContract::ConcernV1)
}

pub(crate) fn validate_episode_context_catalog_visible_manifest(
    results: CatalogResults<'_>,
) -> Result<CatalogVisibleManifest, String> {
    validate_catalog_visible_manifest_for(results, CatalogContract::EpisodeContextV2)
}

pub(crate) fn validate_touchstones_catalog_visible_manifest(
    results: CatalogResults<'_>,
) -> Result<CatalogVisibleManifest, String> {
    validate_catalog_visible_manifest_for(results, CatalogContract::TouchstonesV1)
}

fn validate_catalog_visible_manifest_for(
    results: CatalogResults<'_>,
    contract: CatalogContract,
) -> Result<CatalogVisibleManifest, String> {
    validate_dimension_binding_spec()?;
    validate_relations(results.relations, contract)?;
    validate_result_map_keys(results.columns, contract.relations(), "::columns")?;
    validate_result_map_keys(results.indices, contract.relations(), "::indices")?;
    validate_result_map_keys(
        results.trigger_catalogs,
        contract.bases().iter().map(|base| base.relation),
        "::show_triggers",
    )?;

    let mut vector_dimension = None;
    for relation in contract.relations() {
        let columns = results
            .columns
            .get(relation.name)
            .ok_or_else(|| format!("missing ::columns result for {:?}", relation.name))?;
        let found_dimension = validate_columns(columns, relation)?;
        if relation.name == HNSW_CATALOG_SPEC.dimension.relation {
            vector_dimension = found_dimension;
        } else if found_dimension.is_some() {
            return Err(format!(
                "non-vector relation {:?} unexpectedly selected a vector dimension",
                relation.name
            ));
        }

        let indices = results
            .indices
            .get(relation.name)
            .ok_or_else(|| format!("missing ::indices result for {:?}", relation.name))?;
        validate_relation_indices(relation.name, indices, vector_dimension, contract)?;
    }

    for base in contract.bases() {
        let relation = base.relation;
        let triggers = results
            .trigger_catalogs
            .get(relation.name)
            .ok_or_else(|| format!("missing ::show_triggers result for {:?}", relation.name))?;
        validate_empty_triggers(relation.name, triggers)?;
    }

    let vector_dimension = vector_dimension
        .ok_or_else(|| "node_vec did not expose one positive F32 vector dimension".to_owned())?;
    Ok(CatalogVisibleManifest {
        vector_dimension,
        page_relations: contract.bases(),
        derived_relations: contract
            .derived()
            .iter()
            .map(|child| child.relation.name)
            .collect(),
        semantic_obligations: SEMANTIC_OBLIGATIONS,
        child_access_levels_catalog_attested: false,
        relation_identities_catalog_attested: false,
        hnsw_status_filters_catalog_attested: false,
    })
}

fn validate_dimension_binding_spec() -> Result<(), String> {
    let binding = HNSW_CATALOG_SPEC.dimension;
    let mut matching_relations = BASE_RELATIONS
        .iter()
        .filter(|base| base.relation.name == binding.relation);
    let source = matching_relations
        .next()
        .ok_or_else(|| {
            format!(
                "conventional vector dimension source {:?} is missing",
                binding.relation
            )
        })?
        .relation;
    if matching_relations.next().is_some() {
        return Err(format!(
            "conventional vector dimension source {:?} is duplicated",
            binding.relation
        ));
    }
    let mut vector_columns = source
        .columns
        .iter()
        .filter(|column| column.ty == ColumnType::F32Vector);
    let column = vector_columns.next().ok_or_else(|| {
        format!(
            "conventional vector dimension source {:?} has no F32 vector column",
            binding.relation
        )
    })?;
    if vector_columns.next().is_some() || column.name != binding.column {
        return Err(format!(
            "conventional vector dimension source {:?} does not bind exactly one {:?} column",
            binding.relation, binding.column
        ));
    }
    Ok(())
}

fn validate_result_map_keys(
    results: &BTreeMap<String, NamedRows>,
    expected: impl Clone + Iterator<Item = RelationSpec>,
    command: &str,
) -> Result<(), String> {
    let expected_len = expected.clone().count();
    if results.len() != expected_len {
        return Err(format!(
            "unexpected {command} result-map size: got {}, expected {expected_len}",
            results.len()
        ));
    }
    for relation in expected {
        if !results.contains_key(relation.name) {
            return Err(format!(
                "missing {command} result for relation {:?}",
                relation.name
            ));
        }
    }
    Ok(())
}

fn validate_named_rows_envelope(
    rows: &NamedRows,
    headers: &[&str],
    command: &str,
) -> Result<(), String> {
    if rows.headers.len() != headers.len()
        || !rows
            .headers
            .iter()
            .map(String::as_str)
            .eq(headers.iter().copied())
    {
        return Err(format!("unexpected {command} headers: {:?}", rows.headers));
    }
    if rows.next.is_some() {
        return Err(format!("unexpected chained {command} result"));
    }
    Ok(())
}

fn validate_relations(catalog: &NamedRows, contract: CatalogContract) -> Result<(), String> {
    validate_named_rows_envelope(catalog, &RELATION_HEADERS, "::relations")?;
    let expected_count = contract.bases().len() + contract.derived().len();
    if catalog.rows.len() != expected_count {
        return Err(format!(
            "unexpected ::relations row count: got {}, expected {expected_count}",
            catalog.rows.len()
        ));
    }
    let expected: BTreeMap<&str, RelationSpec> = contract
        .relations()
        .map(|relation| (relation.name, relation))
        .collect();
    let mut seen = BTreeSet::new();
    for (row_number, row) in catalog.rows.iter().enumerate() {
        if row.len() != RELATION_HEADERS.len() {
            return Err(format!(
                "malformed ::relations row {row_number}: got {} fields, expected {}",
                row.len(),
                RELATION_HEADERS.len()
            ));
        }
        let name = exact_string(&row[0], "::relations name")?;
        if !seen.insert(name) {
            return Err(format!("duplicate ::relations row for {name:?}"));
        }
        let relation = expected
            .get(name)
            .ok_or_else(|| format!("unexpected ::relations row for {name:?}"))?;
        let key_count = relation.key_count();
        expect_int(&row[1], relation.columns.len(), name, "arity")?;
        expect_string(
            &row[2],
            relation.catalog_access.catalog_name(),
            name,
            "access_level",
        )?;
        expect_int(&row[3], key_count, name, "n_keys")?;
        expect_int(
            &row[4],
            relation.columns.len() - key_count,
            name,
            "n_non_keys",
        )?;
        for (index, field) in [
            (5, "n_put_triggers"),
            (6, "n_rm_triggers"),
            (7, "n_replace_triggers"),
        ] {
            expect_int(&row[index], 0, name, field)?;
        }
        expect_string(&row[8], "", name, "description")?;
    }
    Ok(())
}

fn validate_columns(catalog: &NamedRows, relation: RelationSpec) -> Result<Option<u64>, String> {
    validate_named_rows_envelope(catalog, &COLUMN_HEADERS, "::columns")?;
    if catalog.rows.len() != relation.columns.len() {
        return Err(format!(
            "relation {:?} has {} columns, expected {}",
            relation.name,
            catalog.rows.len(),
            relation.columns.len()
        ));
    }
    let mut vector_dimension = None;
    for (index, (row, column)) in catalog.rows.iter().zip(relation.columns).enumerate() {
        if row.len() != COLUMN_HEADERS.len() {
            return Err(format!(
                "malformed ::columns row {index} for {:?}: got {} fields, expected {}",
                relation.name,
                row.len(),
                COLUMN_HEADERS.len()
            ));
        }
        expect_string(&row[0], column.name, relation.name, "column")?;
        match &row[1] {
            DataValue::Bool(actual) if *actual == column.is_key => {}
            actual => {
                return Err(format!(
                    "relation {:?} column {:?} has is_key {actual:?}, expected {}",
                    relation.name, column.name, column.is_key
                ));
            }
        }
        expect_int(&row[2], index, relation.name, "index")?;
        let ty = exact_string(&row[3], "::columns type")?;
        match column.ty {
            ColumnType::F32Vector => {
                let dimension = parse_f32_vector_dimension(ty).ok_or_else(|| {
                    format!(
                        "relation {:?} column {:?} has non-F32-vector type {ty:?}",
                        relation.name, column.name
                    )
                })?;
                if vector_dimension.replace(dimension).is_some() {
                    return Err(format!(
                        "relation {:?} exposes multiple vector columns",
                        relation.name
                    ));
                }
            }
            expected => {
                let expected = expected
                    .exact_catalog_name()
                    .expect("non-vector conventional column type has an exact catalog name");
                if ty != expected {
                    return Err(format!(
                        "relation {:?} column {:?} has type {ty:?}, expected {expected:?}",
                        relation.name, column.name
                    ));
                }
            }
        }
        if !matches!(&row[4], DataValue::Bool(false)) || !matches!(&row[5], DataValue::Null) {
            return Err(format!(
                "relation {:?} column {:?} has a default; conventional columns have none",
                relation.name, column.name
            ));
        }
    }
    Ok(vector_dimension)
}

fn parse_f32_vector_dimension(ty: &str) -> Option<u64> {
    let dimension = ty.strip_prefix("<F32;")?.strip_suffix('>')?;
    if dimension.is_empty() || !dimension.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    dimension.parse().ok().filter(|dimension| *dimension > 0)
}

fn validate_relation_indices(
    relation: &str,
    catalog: &NamedRows,
    vector_dimension: Option<u64>,
    contract: CatalogContract,
) -> Result<(), String> {
    let mut expected = BTreeMap::new();
    for child in contract.indices_for(relation) {
        validate_index_identity(child.local_name, child.relation.name)?;
        let expectation = match child.index {
            IndexSpec::Normal(spec) => {
                if spec.ordered_columns.is_empty() {
                    return Err(format!(
                        "normal index expectation {:?} has no columns",
                        child.local_name
                    ));
                }
                if !all_unique(spec.ordered_columns.iter().copied()) {
                    return Err(format!(
                        "normal index expectation {:?} repeats a column",
                        child.local_name
                    ));
                }
                ExpectedIndex::Normal {
                    child_relation: child.relation.name.to_owned(),
                    ordered_columns: spec.ordered_columns.to_vec(),
                }
            }
            IndexSpec::Fts(spec) => ExpectedIndex::Fts {
                child_relation: child.relation.name.to_owned(),
                config: spec,
            },
            IndexSpec::Hnsw(spec) => {
                if spec.catalog.dimension != HNSW_CATALOG_SPEC.dimension {
                    return Err(format!(
                        "HNSW expectation {:?} has an incoherent dimension binding",
                        child.local_name
                    ));
                }
                let dimension = vector_dimension.ok_or_else(|| {
                    format!(
                        "{relation} indices cannot be checked before its dimension is authenticated"
                    )
                })?;
                ExpectedIndex::Hnsw {
                    child_relation: child.relation.name.to_owned(),
                    dim: dimension,
                    config: spec.catalog,
                }
            }
        };
        if expected
            .insert(child.local_name.to_owned(), expectation)
            .is_some()
        {
            return Err(format!(
                "duplicate expected index name {:?}",
                child.local_name
            ));
        }
    }
    validate_expected_indices(catalog, expected)
}

fn validate_empty_triggers(relation: &str, catalog: &NamedRows) -> Result<(), String> {
    validate_named_rows_envelope(catalog, &TRIGGER_HEADERS, "::show_triggers")?;
    if catalog.rows.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "relation {relation:?} has {} triggers, expected none",
            catalog.rows.len()
        ))
    }
}

fn exact_string<'a>(value: &'a DataValue, field: &str) -> Result<&'a str, String> {
    match value {
        DataValue::Str(value) => Ok(value.as_str()),
        actual => Err(format!("{field} is {actual:?}, expected string")),
    }
}

fn expect_string(
    value: &DataValue,
    expected: &str,
    relation: &str,
    field: &str,
) -> Result<(), String> {
    let actual = exact_string(value, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "relation {relation:?} field {field:?} is {actual:?}, expected {expected:?}"
        ))
    }
}

fn expect_int(
    value: &DataValue,
    expected: usize,
    relation: &str,
    field: &str,
) -> Result<(), String> {
    let expected = i64::try_from(expected)
        .map_err(|_| format!("expected {field} for {relation:?} does not fit i64"))?;
    match value {
        DataValue::Num(cozo::Num::Int(actual)) if *actual == expected => Ok(()),
        actual => Err(format!(
            "relation {relation:?} field {field:?} is {actual:?}, expected integer {expected}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use cozo::{DataValue, DbInstance, NamedRows};
    use serde_json::{Value, json};

    use super::*;

    fn complete_db() -> DbInstance {
        let db = DbInstance::new("mem", "", "").unwrap();
        db.run_default(
            r#"
            {:create node {id: String => data: String, status: String}}
            {:create node_tag {id: String, tag: String}}
            {::index create node_tag:by_tag {tag}}
            {::access_level read_only node_tag}
            {:create node_tag_v2 {tag: String, status: String, sample_hash: Int, id: String}}
            {::index create node_tag_v2:by_id {id}}
            {:create node_search {id: String => summary: String, status: String}}
            {::index create node_search:by_status {status}}
            {::fts create node_search:active_fts {extractor: summary, extract_filter: status == 'active', tokenizer: Simple, filters: [Lowercase]}}
            {::fts create node_search:candidate_fts {extractor: summary, extract_filter: status == 'candidate', tokenizer: Simple, filters: [Lowercase]}}
            {::fts create node_search:archived_fts {extractor: summary, extract_filter: status == 'archived', tokenizer: Simple, filters: [Lowercase]}}
            {:create node_vec {id: String => e: <F32; 8>, status: String}}
            {::hnsw create node_vec:active_idx {dim: 8, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'active'}}
            {::hnsw create node_vec:candidate_idx {dim: 8, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'candidate'}}
            {::hnsw create node_vec:archived_idx {dim: 8, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'archived'}}
            {:create edge {from: String, to: String => weight: Float, kind: String, last_reinforced: Int, trials: Int, interference: Int}}
            {::index create edge:by_to {to}}
            {:create edge_anchor {from: String, to: String => start: Int, end: Int}}
            {:create contradiction {lo: String, hi: String => observations: Int, first_seen: Int, last_seen: Int, resolution: String?}}
            {:create merge_candidate {lo: String, hi: String => observations: Int, first_seen: Int, last_seen: Int, resolution: String?}}
            {:create full_merge_commit {lo: String, hi: String => winner: String, loser: String, applied_at: Int}}
            {:create supersede_commit {lo: String, hi: String => winner: String, loser: String, applied_at: Int}}
            {:create remote_edge {from: String, target_db: String, target: String => weight: Float}}
            {:create feedback_retry {key: String => fingerprint: String, applied_at: Int}}
            {:create feedback_retry_order {epoch: String, sequence: Int, key: String => marker: Bool}}
            {:create meta {k: String => v: String}}
            {:create mneme_reembed_shadow_node_vec {fence: String => generation: String}}
            {::access_level read_only mneme_reembed_shadow_node_vec}
            "#,
        )
        .unwrap();
        db
    }

    #[derive(Clone)]
    struct OwnedCatalog {
        relations: NamedRows,
        columns: BTreeMap<String, NamedRows>,
        indices: BTreeMap<String, NamedRows>,
        triggers: BTreeMap<String, NamedRows>,
    }

    impl OwnedCatalog {
        fn results(&self) -> CatalogResults<'_> {
            CatalogResults {
                relations: &self.relations,
                columns: &self.columns,
                indices: &self.indices,
                trigger_catalogs: &self.triggers,
            }
        }
    }

    fn complete_catalog() -> OwnedCatalog {
        let db = complete_db();
        let relations = db.run_default("::relations").unwrap();
        let columns = all_physical_relations()
            .map(|relation| {
                (
                    relation.name.to_owned(),
                    db.run_default(&format!("::columns {}", relation.name))
                        .unwrap(),
                )
            })
            .collect();
        let indices = all_physical_relations()
            .map(|relation| {
                (
                    relation.name.to_owned(),
                    db.run_default(&format!("::indices {}", relation.name))
                        .unwrap(),
                )
            })
            .collect();
        let triggers = BASE_RELATIONS
            .iter()
            .map(|base| {
                let relation = base.relation;
                (
                    relation.name.to_owned(),
                    db.run_default(&format!("::show_triggers {}", relation.name))
                        .unwrap(),
                )
            })
            .collect();
        OwnedCatalog {
            relations,
            columns,
            indices,
            triggers,
        }
    }

    const NORMAL_COLUMNS: [usize; 4] = [3, 0, 1, 2];
    const NORMAL: NormalIndexExpectation<'static> =
        NormalIndexExpectation::new("by_id", "node_tag_v2:by_id", &NORMAL_COLUMNS);
    const HNSW: HnswIndexExpectation<'static> =
        HnswIndexExpectation::new("active_idx", "node_vec:active_idx", 8);

    fn catalog(rows: Vec<Vec<DataValue>>) -> NamedRows {
        NamedRows::new(
            CATALOG_HEADERS.iter().map(ToString::to_string).collect(),
            rows,
        )
    }

    fn normal_row(name: &str, child: &str, columns: &[usize]) -> Vec<DataValue> {
        vec![
            DataValue::from(name),
            DataValue::from("normal"),
            DataValue::from(json!([child])),
            DataValue::from(json!({"indices": columns})),
        ]
    }

    fn hnsw_config(dim: usize) -> Value {
        json!({
            "vec_dim": dim,
            "dtype": "F32",
            "vec_fields": [1],
            "distance": "Cosine",
            "ef_construction": 200,
            "m_neighbours": 16,
            "m_max": 16,
            "m_max0": 32,
            "level_multiplier": 1.0 / 16.0_f64.ln(),
            "extend_candidates": false,
            "keep_pruned_connections": false,
        })
    }

    fn hnsw_row(name: &str, child: &str, dim: usize) -> Vec<DataValue> {
        vec![
            DataValue::from(name),
            DataValue::from("hnsw"),
            DataValue::from(json!([child])),
            DataValue::from(hnsw_config(dim)),
        ]
    }

    fn fts_row(name: &str, child: &str, lifecycle: &str) -> Vec<DataValue> {
        vec![
            DataValue::from(name),
            DataValue::from("fts"),
            DataValue::from(json!([child])),
            DataValue::from(json!({
                "extractor": format!("if(eq(status, \"{lifecycle}\"), summary)"),
                "tokenizer": {"name": "Simple", "args": []},
                "tokenizer_filters": [{"name": "Lowercase", "args": []}],
            })),
        ]
    }

    fn fts_config_object(row: &mut [DataValue]) -> &mut serde_json::Map<String, Value> {
        let DataValue::Json(config) = &mut row[3] else {
            panic!("test FTS config is not JSON")
        };
        config.0.as_object_mut().unwrap()
    }

    fn assert_full_catalog_rejected(catalog: &OwnedCatalog) {
        assert!(
            validate_catalog_visible_manifest(catalog.results()).is_err(),
            "corrupt complete conventional unmanaged catalog was accepted"
        );
    }

    fn relation_row_mut<'a>(catalog: &'a mut NamedRows, name: &str) -> &'a mut Vec<DataValue> {
        catalog
            .rows
            .iter_mut()
            .find(|row| matches!(&row[0], DataValue::Str(actual) if actual.as_str() == name))
            .unwrap_or_else(|| panic!("missing test relation row {name:?}"))
    }

    fn index_row_mut<'a>(catalog: &'a mut NamedRows, name: &str) -> &'a mut Vec<DataValue> {
        catalog
            .rows
            .iter_mut()
            .find(|row| matches!(&row[0], DataValue::Str(actual) if actual.as_str() == name))
            .unwrap_or_else(|| panic!("missing test index row {name:?}"))
    }

    fn exact_synthetic_catalog() -> NamedRows {
        catalog(vec![
            normal_row("by_id", "node_tag_v2:by_id", &NORMAL_COLUMNS),
            hnsw_row("active_idx", "node_vec:active_idx", 8),
        ])
    }

    fn assert_exact_synthetic_rejected(catalog: &NamedRows) {
        assert!(
            validate_catalog_visible_indices(catalog, &[NORMAL], &[HNSW]).is_err(),
            "malformed catalog was accepted: {catalog:?}"
        );
    }

    fn hnsw_config_object(row: &mut [DataValue]) -> &mut serde_json::Map<String, Value> {
        let DataValue::Json(config) = &mut row[3] else {
            panic!("test HNSW config is not JSON")
        };
        config.0.as_object_mut().unwrap()
    }

    #[test]
    fn accepts_only_the_complete_exact_synthetic_catalog() {
        validate_catalog_visible_indices(&exact_synthetic_catalog(), &[NORMAL], &[HNSW]).unwrap();

        let mut malformed = exact_synthetic_catalog();
        malformed.headers.swap(0, 1);
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        malformed.next = Some(Box::new(catalog(Vec::new())));
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        malformed.rows[0].pop();
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        malformed.rows[0][0] = DataValue::from(json!({"name": "by_id"}));
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        malformed.rows[0][1] = DataValue::from("hnsw");
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        malformed.rows[0][2] = DataValue::from(json!(["node_tag_v2:by_id", "extra"]));
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        malformed.rows[0][3] = DataValue::from("{\"indices\":[3,0,1,2]}");
        assert_exact_synthetic_rejected(&malformed);
    }

    #[test]
    fn rejects_extra_missing_duplicate_and_unknown_catalog_rows() {
        let mut malformed = exact_synthetic_catalog();
        malformed.rows.pop();
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        malformed
            .rows
            .push(hnsw_row("candidate_idx", "node_vec:candidate_idx", 8));
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        malformed.rows[1] = malformed.rows[0].clone();
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        malformed.rows[1][0] = DataValue::from("unknown");
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        malformed.rows[1][1] = DataValue::from("fts");
        assert_exact_synthetic_rejected(&malformed);
    }

    #[test]
    fn normal_index_validation_is_ordered_typed_and_exact() {
        for columns in [
            json!([0, 1, 2, 3]),
            json!([3, 0, 1]),
            json!([3, 0, 1, 1]),
            json!([3, 0, 1, "2"]),
            json!([3, 0, 1, -2]),
            json!([3.0, 0, 1, 2]),
        ] {
            let mut malformed = exact_synthetic_catalog();
            let DataValue::Json(config) = &mut malformed.rows[0][3] else {
                unreachable!()
            };
            config.0["indices"] = columns;
            assert_exact_synthetic_rejected(&malformed);
        }

        let mut malformed = exact_synthetic_catalog();
        let DataValue::Json(config) = &mut malformed.rows[0][3] else {
            unreachable!()
        };
        config
            .0
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), json!(true));
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        let DataValue::Json(config) = &mut malformed.rows[0][3] else {
            unreachable!()
        };
        config.0.as_object_mut().unwrap().remove("indices");
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        malformed.rows[0][2] = DataValue::from(json!(["other:by_id"]));
        assert_exact_synthetic_rejected(&malformed);
    }

    #[test]
    fn hnsw_validation_pins_every_catalog_visible_manifest_field() {
        for (field, wrong) in [
            ("vec_dim", json!(9)),
            ("dtype", json!("F64")),
            ("vec_fields", json!([0])),
            ("distance", json!("L2")),
            ("ef_construction", json!(199)),
            ("m_neighbours", json!(15)),
            ("m_max", json!(15)),
            ("m_max0", json!(31)),
            ("level_multiplier", json!(0.5)),
            ("extend_candidates", json!(true)),
            ("keep_pruned_connections", json!(true)),
        ] {
            let mut malformed = exact_synthetic_catalog();
            hnsw_config_object(&mut malformed.rows[1]).insert(field.into(), wrong);
            assert_exact_synthetic_rejected(&malformed);
        }

        for (field, malformed_value) in [
            ("vec_dim", json!(8.0)),
            ("vec_fields", json!("[1]")),
            ("level_multiplier", json!("derived")),
            ("extend_candidates", json!(0)),
        ] {
            let mut malformed = exact_synthetic_catalog();
            hnsw_config_object(&mut malformed.rows[1]).insert(field.into(), malformed_value);
            assert_exact_synthetic_rejected(&malformed);
        }

        let mut malformed = exact_synthetic_catalog();
        hnsw_config_object(&mut malformed.rows[1]).remove("distance");
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        hnsw_config_object(&mut malformed.rows[1]).insert("filter".into(), json!("status"));
        assert_exact_synthetic_rejected(&malformed);

        let mut malformed = exact_synthetic_catalog();
        malformed.rows[1][2] = DataValue::from(json!(["other:active_idx"]));
        assert_exact_synthetic_rejected(&malformed);
    }

    #[test]
    fn fts_validation_pins_lifecycle_extractor_and_text_pipeline() {
        let active = &DERIVED_RELATIONS[3];
        let IndexSpec::Fts(config) = active.index else {
            panic!("pinned active FTS relation is not FTS")
        };
        let expected = FtsIndexExpectation::new(active.local_name, active.relation.name, config);
        let exact = || {
            catalog(vec![fts_row(
                "active_fts",
                "node_search:active_fts",
                "active",
            )])
        };
        validate_catalog_visible_indices_with_fts(&exact(), &[], &[], &[expected]).unwrap();

        for (field, wrong) in [
            ("extractor", json!("summary")),
            ("tokenizer", json!({"name": "NGram", "args": []})),
            ("tokenizer", json!({"name": "Simple", "args": [1]})),
            ("tokenizer_filters", json!([])),
            (
                "tokenizer_filters",
                json!([{"name": "Uppercase", "args": []}]),
            ),
            (
                "tokenizer_filters",
                json!([{"name": "Lowercase", "args": [1]}]),
            ),
        ] {
            let mut malformed = exact();
            fts_config_object(&mut malformed.rows[0]).insert(field.into(), wrong);
            assert!(
                validate_catalog_visible_indices_with_fts(&malformed, &[], &[], &[expected])
                    .is_err()
            );
        }

        let mut malformed = exact();
        fts_config_object(&mut malformed.rows[0]).insert("extra".into(), json!(true));
        assert!(
            validate_catalog_visible_indices_with_fts(&malformed, &[], &[], &[expected]).is_err()
        );

        let mut malformed = exact();
        malformed.rows[0][2] = DataValue::from(json!(["node_search:candidate_fts"]));
        assert!(
            validate_catalog_visible_indices_with_fts(&malformed, &[], &[], &[expected]).is_err()
        );
    }

    #[test]
    fn real_mnestic_complete_catalog_is_closed_and_page_ordinals_are_stable() {
        let catalog = complete_catalog();
        let manifest = validate_catalog_visible_manifest(catalog.results()).unwrap();
        assert_eq!(manifest.vector_dimension, 8);
        assert_eq!(manifest.page_relations.len(), 16);
        assert_eq!(manifest.derived_relations.len(), 10);
        assert!(!manifest.child_access_levels_catalog_attested);
        assert!(!manifest.relation_identities_catalog_attested);
        assert!(!manifest.hnsw_status_filters_catalog_attested);
        assert_eq!(
            manifest
                .page_relations
                .iter()
                .map(|relation| relation.ordinal as u16)
                .collect::<Vec<_>>(),
            (1_u16..=16).collect::<Vec<_>>()
        );
        assert_eq!(
            manifest
                .page_relations
                .iter()
                .map(|relation| relation.relation.name)
                .collect::<Vec<_>>(),
            vec![
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
        assert!(manifest.derived_relations.iter().all(|child| {
            !manifest
                .page_relations
                .iter()
                .any(|base| base.relation.name == *child)
        }));
        assert_eq!(catalog.relations.rows.len(), 26);
        assert_eq!(catalog.columns.len(), 26);
        assert_eq!(catalog.indices.len(), 26);
        assert_eq!(catalog.triggers.len(), 16);

        let obligations = manifest.semantic_obligations;
        assert_eq!(obligations.node_tag_relation, "node_tag");
        assert_eq!(obligations.node_tag_exact_row_count, 0);
        assert_eq!(obligations.permanent_guard_exact_row_count, 1);
        assert_eq!(
            obligations.permanent_guard_relation,
            "mneme_reembed_shadow_node_vec"
        );
        assert_eq!(obligations.permanent_guard_key_column, "fence");
        assert_eq!(
            obligations.permanent_guard_key,
            crate::vector_projection::GUARD_KEY
        );
        assert_eq!(obligations.permanent_guard_value_column, "generation");
        assert_eq!(
            obligations.permanent_guard_value,
            PERMANENT_VECTOR_GUARD_VALUE
        );

        let fts = &catalog.indices["node_search"];
        for lifecycle in ["active", "candidate", "archived"] {
            let row = fts.rows.iter().find(|row| {
                matches!(&row[0], DataValue::Str(name) if name.as_str() == format!("{lifecycle}_fts"))
            }).unwrap();
            let DataValue::Json(config) = &row[3] else {
                unreachable!()
            };
            assert_eq!(
                config.0["extractor"],
                json!(format!("if(eq(status, \"{lifecycle}\"), summary)"))
            );
        }
        let hnsw = &catalog.indices["node_vec"];
        for row in &hnsw.rows {
            let DataValue::Json(config) = &row[3] else {
                unreachable!()
            };
            assert!(config.0.get("filter").is_none());
            assert!(config.0.get("index_filter").is_none());
        }
    }

    #[test]
    fn complete_catalog_rejects_every_relation_envelope_and_identity_corruption() {
        let exact = complete_catalog();

        let mut malformed = exact.clone();
        malformed.relations.headers.swap(0, 1);
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        malformed.relations.next = Some(Box::new(NamedRows::new(Vec::new(), Vec::new())));
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        malformed.relations.rows.pop();
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        malformed.relations.rows[1] = malformed.relations.rows[0].clone();
        assert_full_catalog_rejected(&malformed);

        for reserved in [
            "foreign_relation",
            "mneme_f7_shadow_node",
            "store_meta",
            "mneme_vector_v3_old_node_vec",
        ] {
            let mut malformed = exact.clone();
            relation_row_mut(&mut malformed.relations, "meta")[0] = DataValue::from(reserved);
            assert_full_catalog_rejected(&malformed);
        }

        let mut malformed = exact.clone();
        relation_row_mut(&mut malformed.relations, "meta").pop();
        assert_full_catalog_rejected(&malformed);

        for field in 1..RELATION_HEADERS.len() {
            let mut malformed = exact.clone();
            relation_row_mut(&mut malformed.relations, "meta")[field] = DataValue::Null;
            assert_full_catalog_rejected(&malformed);
        }
    }

    #[test]
    fn complete_catalog_rejects_columns_map_shape_type_order_and_defaults_corruption() {
        let exact = complete_catalog();

        let mut malformed = exact.clone();
        malformed.columns.remove("meta");
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        malformed.columns.insert(
            "foreign".into(),
            NamedRows::new(COLUMN_HEADERS.map(str::to_owned).to_vec(), Vec::new()),
        );
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        malformed
            .columns
            .get_mut("meta")
            .unwrap()
            .headers
            .swap(0, 1);
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        malformed.columns.get_mut("meta").unwrap().next =
            Some(Box::new(NamedRows::new(Vec::new(), Vec::new())));
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        malformed.columns.get_mut("meta").unwrap().rows.pop();
        assert_full_catalog_rejected(&malformed);

        let wrong = [
            DataValue::from("wrong"),
            DataValue::Bool(false),
            DataValue::from(1_i64),
            DataValue::from("Any"),
            DataValue::Bool(true),
            DataValue::from("now()"),
        ];
        for (field, wrong) in wrong.into_iter().enumerate() {
            let mut malformed = exact.clone();
            malformed.columns.get_mut("meta").unwrap().rows[0][field] = wrong;
            assert_full_catalog_rejected(&malformed);
        }

        for wrong_type in ["<F32;0>", "<F32; 8>", "<F64;8>", "[Float]", "<F32;8"] {
            let mut malformed = exact.clone();
            malformed.columns.get_mut("node_vec").unwrap().rows[1][3] = DataValue::from(wrong_type);
            assert_full_catalog_rejected(&malformed);
        }

        let mut malformed = exact.clone();
        malformed.columns.get_mut("node_vec").unwrap().rows[1][3] = DataValue::from("<F32;9>");
        assert_full_catalog_rejected(&malformed);
    }

    #[test]
    fn complete_catalog_rejects_index_and_trigger_result_corruption() {
        let exact = complete_catalog();

        let mut malformed = exact.clone();
        malformed.indices.remove("node_vec");
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        malformed.indices.insert(
            "store_meta".into(),
            NamedRows::new(CATALOG_HEADERS.map(str::to_owned).to_vec(), Vec::new()),
        );
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        index_row_mut(
            malformed.indices.get_mut("node_search").unwrap(),
            "active_fts",
        )[1] = DataValue::from("hnsw");
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        let row = index_row_mut(
            malformed.indices.get_mut("node_search").unwrap(),
            "candidate_fts",
        );
        fts_config_object(row).insert("extractor".into(), json!("summary"));
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        malformed.triggers.remove("meta");
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        malformed.triggers.insert(
            "store_meta".into(),
            NamedRows::new(TRIGGER_HEADERS.map(str::to_owned).to_vec(), Vec::new()),
        );
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        malformed
            .triggers
            .get_mut("meta")
            .unwrap()
            .headers
            .swap(0, 1);
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact.clone();
        malformed.triggers.get_mut("meta").unwrap().rows.push(vec![
            DataValue::from("put"),
            DataValue::from(0_i64),
            DataValue::from("?[k] := *meta{k}"),
        ]);
        assert_full_catalog_rejected(&malformed);

        let mut malformed = exact;
        malformed.triggers.get_mut("meta").unwrap().next =
            Some(Box::new(NamedRows::new(Vec::new(), Vec::new())));
        assert_full_catalog_rejected(&malformed);
    }

    #[test]
    fn rejects_incoherent_or_duplicate_expectations() {
        assert!(
            validate_catalog_visible_indices(
                &catalog(vec![normal_row(
                    "by_id",
                    "node_tag_v2:by_id",
                    &NORMAL_COLUMNS
                )]),
                &[
                    NORMAL,
                    NormalIndexExpectation::new("by_id", "node_tag_v2:by_id", &NORMAL_COLUMNS)
                ],
                &[]
            )
            .is_err()
        );
        assert!(
            validate_catalog_visible_indices(
                &catalog(Vec::new()),
                &[NormalIndexExpectation::new(
                    "by_id",
                    "node_tag_v2:not_by_id",
                    &NORMAL_COLUMNS
                )],
                &[]
            )
            .is_err()
        );
        assert!(
            validate_catalog_visible_indices(
                &catalog(Vec::new()),
                &[NormalIndexExpectation::new(
                    "by_id",
                    "node_tag_v2:by_id",
                    &[]
                )],
                &[]
            )
            .is_err()
        );
        assert!(
            validate_catalog_visible_indices(
                &catalog(Vec::new()),
                &[],
                &[HnswIndexExpectation::new(
                    "active_idx",
                    "node_vec:active_idx",
                    0
                )]
            )
            .is_err()
        );
    }

    #[test]
    fn real_mnestic_catalog_matches_membership_and_three_hnsws() {
        let db = DbInstance::new("mem", "", "").unwrap();
        db.run_default(
            "{:create node_tag_v2 {tag: String, status: String, sample_hash: Int, id: String}}\n\
             {::index create node_tag_v2:by_id {id}}",
        )
        .unwrap();
        let membership = db.run_default("::indices node_tag_v2").unwrap();
        validate_catalog_visible_indices(&membership, &[NORMAL], &[]).unwrap();

        db.run_default(
            "{:create node_vec {id: String => e: <F32; 8>, status: String}}\n\
             {::hnsw create node_vec:active_idx {dim: 8, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'active'}}\n\
             {::hnsw create node_vec:candidate_idx {dim: 8, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'candidate'}}\n\
             {::hnsw create node_vec:archived_idx {dim: 8, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'archived'}}",
        )
        .unwrap();
        let hnsw = db.run_default("::indices node_vec").unwrap();
        let expectations = [
            HNSW,
            HnswIndexExpectation::new("candidate_idx", "node_vec:candidate_idx", 8),
            HnswIndexExpectation::new("archived_idx", "node_vec:archived_idx", 8),
        ];
        validate_catalog_visible_indices(&hnsw, &[], &expectations).unwrap();

        for row in &hnsw.rows {
            let DataValue::Json(config) = &row[3] else {
                panic!("real HNSW catalog config was not JSON")
            };
            assert!(
                config.0.get("filter").is_none() && config.0.get("index_filter").is_none(),
                "Mnestic unexpectedly exposed an HNSW filter; revisit the trust boundary"
            );
        }
    }
}
