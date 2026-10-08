use cozo::ManagedSqliteRelationAssertionPlannerV1;

use crate::storage_contract::conventional_unmanaged::{
    admission::{
        plan_source_bound_assertions_v1, require_canonical_struct_map_source,
        validate_primary_index_visible_catalog,
    },
    spec::BASE_RELATIONS,
};

/// Plan the existing source-bound assertions and select the sixteen base roles.
///
/// This is intentionally the planning half of a split authority. The selected
/// ids remain tentative until the independent post-close verifier rederives the
/// roles without importing this module or the static schema tables used here.
pub(in crate::storage_contract::conventional_unmanaged::snapshot) fn plan_logical_snapshot_roles_v1(
    planner: &mut ManagedSqliteRelationAssertionPlannerV1<'_>,
) -> Result<[u64; 16], cozo::Error> {
    plan_source_bound_assertions_v1(planner)?;

    let candidate = validate_primary_index_visible_catalog(planner.catalog())
        .map_err(|error| cozo::Error::msg(error.to_string()))?;
    require_canonical_struct_map_source(&candidate)
        .map_err(|error| cozo::Error::msg(error.to_string()))?;

    let mut relation_ids = [0_u64; 16];
    for (index, base) in BASE_RELATIONS.iter().enumerate() {
        relation_ids[index] = candidate.relation_id(base.relation.name).ok_or_else(|| {
            cozo::Error::msg("logical snapshot planner lost a validated base relation id")
        })?;
    }
    Ok(relation_ids)
}
