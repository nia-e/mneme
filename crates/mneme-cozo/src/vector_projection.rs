#[cfg(test)]
use cozo::{DataValue, NamedRows, Num};

pub(crate) const META_KEY: &str = "vector_projection_v2";
/// The committed vector-v3 generation before the sealed canonical Node contract.
/// It is accepted only by the leased offline upgrader, never by ordinary open.
#[cfg(test)]
pub(crate) const PRIOR_V3_META_VALUE: &str = "status_partitioned_hnsw_mnestic_0_13_v3";
/// Genuine pre-0.13 stores use this marker and a different physical generation.
#[cfg(test)]
pub(crate) const LEGACY_V2_META_VALUE: &str = "status_partitioned_hnsw_v2";
/// Combined vector-v3 plus sealed canonical Node generation immediately before
/// the conventional unmanaged contract. Existing predecessor code keeps this
/// name and value.
#[cfg(test)]
pub(crate) const META_VALUE: &str = "status_partitioned_hnsw_mnestic_0_13_v3_canonical_node_v1";
pub(crate) const LEGACY_SHADOW_GUARD: &str = "mneme_reembed_shadow_node_vec";
#[cfg(test)]
pub(crate) const LEGACY_SHADOW_META: &str = "mneme_reembed_shadow_meta";
#[cfg(test)]
pub(crate) const LEGACY_OLD_NODE_VEC: &str = "mneme_reembed_old_node_vec";
#[cfg(test)]
pub(crate) const LEGACY_OLD_META: &str = "mneme_reembed_old_meta";

#[cfg(test)]
pub(crate) const V3_SHADOW_NODE_VEC: &str = "mneme_vector_v3_shadow_node_vec";
#[cfg(test)]
pub(crate) const V3_SHADOW_META: &str = "mneme_vector_v3_shadow_meta";
#[cfg(test)]
pub(crate) const V3_OLD_NODE_VEC: &str = "mneme_vector_v3_old_node_vec";
#[cfg(test)]
pub(crate) const V3_OLD_META: &str = "mneme_vector_v3_old_meta";

/// Detached upgrade vector copies are bounded independently by row count and
/// raw components. At the maximum tagged width (4,096), the component budget
/// admits 64 rows (about 1 MiB of raw f32 values) before Cozo/value copies.
#[cfg(test)]
pub(crate) const UPGRADE_VECTOR_COPY_ROW_CAP: usize = 1_024;
#[cfg(test)]
pub(crate) const UPGRADE_VECTOR_COPY_COMPONENT_BUDGET: usize = 262_144;

/// Select a nonzero page whose component product is checked before any upgrade
/// page-sized allocation or scan is requested.
#[cfg(test)]
pub(crate) fn upgrade_vector_copy_page_rows(dimension: usize) -> Option<usize> {
    if dimension == 0 {
        return None;
    }
    let rows = UPGRADE_VECTOR_COPY_ROW_CAP.min(UPGRADE_VECTOR_COPY_COMPONENT_BUDGET / dimension);
    let components = rows.checked_mul(dimension)?;
    (rows > 0 && components <= UPGRADE_VECTOR_COPY_COMPONENT_BUDGET).then_some(rows)
}

pub(crate) const GUARD_KEY: &str = "mneme-vector-projection-old-writer-fence";
/// Guard generation expected by the committed vector-v3 upgrader. A current
/// store must never retain this value: that upgrader treats an unknown vector
/// marker as legacy and would otherwise republish its old sentinel.
#[cfg(test)]
pub(crate) const PRIOR_GUARD_VALUE: &str = "status-partitioned-hnsw-v3-read-only-guard-v1";
#[cfg(test)]
pub(crate) const GUARD_VALUE: &str =
    "status-partitioned-hnsw-v3-canonical-node-v1-read-only-guard-v1";
pub(crate) fn conventional_guard_install_script() -> String {
    guard_install_script_for(
        LEGACY_SHADOW_GUARD,
        crate::storage_contract::conventional_unmanaged::spec::PERMANENT_VECTOR_GUARD_VALUE,
        "",
        "",
    )
}

fn guard_install_script_for(
    relation: &str,
    generation: &str,
    before_read_only: &str,
    after_read_only: &str,
) -> String {
    format!(
        "{{:create {relation} {{fence: String => generation: String}}}}\n\
             {{?[fence, generation] <- [['{GUARD_KEY}', '{generation}']] \
               :put {relation} {{fence => generation}}}}\n\
             {before_read_only}\n\
             {{::access_level read_only {relation}}}\n\
             {after_read_only}"
    )
}

#[cfg(test)]
pub(crate) fn guard_columns_are_exact(columns: &NamedRows) -> bool {
    columns.rows.len() == 2
        && column_is(&columns.rows[0], "fence", true, 0, |ty| ty == "String")
        && column_is(&columns.rows[1], "generation", false, 1, |ty| {
            ty == "String"
        })
}

#[cfg(test)]
pub(crate) fn legacy_vector_columns_are_recognized(columns: &NamedRows) -> bool {
    if columns.rows.len() != 2 && columns.rows.len() != 3 {
        return false;
    }
    if !column_is(&columns.rows[0], "id", true, 0, |ty| ty == "String")
        || !column_is(&columns.rows[1], "e", false, 1, fixed_f32_vector_type)
    {
        return false;
    }
    columns.rows.len() == 2 || column_is(&columns.rows[2], "status", false, 2, |ty| ty == "String")
}

#[cfg(test)]
fn fixed_f32_vector_type(ty: &str) -> bool {
    let Some(dimension) = ty
        .strip_prefix("<F32;")
        .and_then(|rest| rest.strip_suffix('>'))
    else {
        return false;
    };
    let dimension = dimension.trim();
    !dimension.is_empty()
        && dimension.bytes().all(|byte| byte.is_ascii_digit())
        && dimension
            .parse::<usize>()
            .is_ok_and(|dimension| dimension > 0)
}

#[cfg(test)]
pub(crate) fn meta_columns_are_exact(columns: &NamedRows) -> bool {
    columns.rows.len() == 2
        && column_is(&columns.rows[0], "k", true, 0, |ty| ty == "String")
        && column_is(&columns.rows[1], "v", false, 1, |ty| ty == "String")
}

#[cfg(test)]
fn column_is(
    row: &[DataValue],
    name: &str,
    is_key: bool,
    index: i64,
    type_matches: impl FnOnce(&str) -> bool,
) -> bool {
    matches!(row.first(), Some(DataValue::Str(value)) if value.as_str() == name)
        && matches!(row.get(1), Some(DataValue::Bool(value)) if *value == is_key)
        && matches!(row.get(2), Some(DataValue::Num(Num::Int(value))) if *value == index)
        && matches!(row.get(3), Some(DataValue::Str(value)) if type_matches(value.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(rows: Vec<Vec<DataValue>>) -> NamedRows {
        NamedRows::new(Vec::new(), rows)
    }

    fn column(name: &str, is_key: bool, index: i64, ty: &str) -> Vec<DataValue> {
        vec![
            DataValue::from(name),
            DataValue::Bool(is_key),
            DataValue::from(index),
            DataValue::from(ty),
        ]
    }

    #[test]
    fn artifact_shapes_are_exact_and_do_not_conflate_the_guard_with_vectors() {
        let guard = columns(vec![
            column("fence", true, 0, "String"),
            column("generation", false, 1, "String"),
        ]);
        let vector = columns(vec![
            column("id", true, 0, "String"),
            column("e", false, 1, "<F32; 8>"),
            column("status", false, 2, "String"),
        ]);
        let meta = columns(vec![
            column("k", true, 0, "String"),
            column("v", false, 1, "String"),
        ]);

        assert!(guard_columns_are_exact(&guard));
        assert!(!legacy_vector_columns_are_recognized(&guard));
        assert!(legacy_vector_columns_are_recognized(&vector));
        assert!(!guard_columns_are_exact(&vector));
        assert!(meta_columns_are_exact(&meta));

        for invalid_type in [
            "<F64; 8>",
            "<F32; 8> suffix",
            "prefix <F32; 8>",
            "<F32; 8>>",
            "<F32; eight>",
            "<F32; 0>",
        ] {
            let invalid = columns(vec![
                column("id", true, 0, "String"),
                column("e", false, 1, invalid_type),
            ]);
            assert!(
                !legacy_vector_columns_are_recognized(&invalid),
                "accepted invalid legacy vector type {invalid_type:?}"
            );
        }
    }

    #[test]
    fn vector_upgrade_copy_pages_obey_row_and_checked_component_boundaries() {
        assert_eq!(upgrade_vector_copy_page_rows(0), None);
        assert_eq!(upgrade_vector_copy_page_rows(1), Some(1_024));
        assert_eq!(upgrade_vector_copy_page_rows(256), Some(1_024));
        assert_eq!(upgrade_vector_copy_page_rows(257), Some(1_020));
        assert_eq!(upgrade_vector_copy_page_rows(4_096), Some(64));
        assert_eq!(
            upgrade_vector_copy_page_rows(UPGRADE_VECTOR_COPY_COMPONENT_BUDGET),
            Some(1)
        );
        assert_eq!(
            upgrade_vector_copy_page_rows(UPGRADE_VECTOR_COPY_COMPONENT_BUDGET + 1),
            None
        );

        for dimension in [1, 256, 257, 4_096, UPGRADE_VECTOR_COPY_COMPONENT_BUDGET] {
            let rows = upgrade_vector_copy_page_rows(dimension).unwrap();
            assert!(rows <= UPGRADE_VECTOR_COPY_ROW_CAP);
            assert!(rows.checked_mul(dimension).unwrap() <= UPGRADE_VECTOR_COPY_COMPONENT_BUDGET);
        }
    }

    #[test]
    fn cumulative_vector_generation_literals_are_pairwise_distinct() {
        let generations = [
            LEGACY_V2_META_VALUE,
            PRIOR_V3_META_VALUE,
            META_VALUE,
            crate::storage_contract::conventional_unmanaged::spec::LEGACY_CATALOG_GENERATION_MARKER,
            crate::storage_contract::conventional_unmanaged::spec::STRUCT_MAP_CATALOG_GENERATION_MARKER,
            crate::storage_contract::conventional_unmanaged::spec::MANAGED_WRITER_GENERATION_MARKER,
        ];
        for (index, generation) in generations.iter().enumerate() {
            assert!(
                generations[index + 1..]
                    .iter()
                    .all(|other| generation != other),
                "cumulative vector-generation literals must not collide"
            );
        }
    }
}
