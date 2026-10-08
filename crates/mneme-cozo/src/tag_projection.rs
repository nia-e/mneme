#[cfg(test)]
use cozo::{DataValue, NamedRows, Num};

/// Canonical metadata entry proving the v2 physical tag membership is live.
pub(crate) const META_KEY: &str = "tag_projection_v2";
/// Frozen marker published by deployed conventional-format writers.
pub(crate) const META_VALUE: &str = "canonical_node_tag_membership_v2_f7";

/// Frozen crash-residue names from the shipped conventional publisher. Their
/// historical bytes make interrupted pre-publication work recognizable and
/// safely replaceable on retry.
#[cfg(test)]
pub(crate) const SHADOW_NODE_TAG: &str = "mneme_f7_shadow_node_tag_v2";
#[cfg(test)]
pub(crate) const SHADOW_NODE_TAG_BY_ID: &str = "by_id";
#[cfg(test)]
pub(crate) const SHADOW_LEGACY_GUARD: &str = "mneme_f7_shadow_legacy_node_tag_guard";
#[cfg(test)]
pub(crate) const SHADOW_NODE: &str = "mneme_f7_shadow_node";
#[cfg(test)]
pub(crate) const SHADOW_NODE_VEC: &str = "mneme_f7_shadow_node_vec";
#[cfg(test)]
pub(crate) const SHADOW_VECTOR_GUARD: &str = "mneme_f7_shadow_vector_guard";
#[cfg(test)]
pub(crate) const SHADOW_META: &str = "mneme_f7_shadow_meta";

/// Frozen post-publication rollback names. They are never authoritative and
/// cleanup removes them only after checking their complete schemas and indexes.
#[cfg(test)]
pub(crate) const OLD_LEGACY_NODE_TAG: &str = "mneme_f7_old_node_tag";
#[cfg(test)]
pub(crate) const OLD_NODE: &str = "mneme_f7_old_node";
#[cfg(test)]
pub(crate) const OLD_NODE_VEC: &str = "mneme_f7_old_node_vec";
#[cfg(test)]
pub(crate) const OLD_META: &str = "mneme_f7_old_meta";

/// Canonical rows are authenticated one-at-a-time by the raw archaeology
/// audit. Once that succeeds, detached builders and rechecks may safely use a
/// wider, still-small keyset page.
#[cfg(test)]
pub(crate) const NODE_AUDIT_PAGE: usize = 64;
#[cfg(test)]
pub(crate) const MEMBERSHIP_WRITE_CHUNK: usize = 256;
/// A complete trusted node page may project at most 64 tags per node. The
/// extra row is a corruption canary: a full result at this limit is rejected
/// before callers collect an attacker-controlled membership set.
#[cfg(test)]
pub(crate) const MEMBERSHIP_VERIFY_RESULT_CAP: usize =
    NODE_AUDIT_PAGE * mneme_core::MAX_NODE_TAGS + 1;

#[cfg(test)]
pub(crate) fn membership_verification_query(input: &str, relation: &str) -> String {
    format!(
        "{input}\n\
         ?[id, tag, status, sample_hash] := wanted[id], \
           *{relation}:{SHADOW_NODE_TAG_BY_ID}{{id, tag, status, sample_hash}} \
           :limit {MEMBERSHIP_VERIFY_RESULT_CAP}"
    )
}

#[cfg(test)]
pub(crate) fn create_membership_script(relation: &str) -> String {
    format!(
        "{{:create {relation} {{tag: String, status: String, sample_hash: Int, id: String}}}}\n\
         {{::index create {relation}:{SHADOW_NODE_TAG_BY_ID} {{id}}}}"
    )
}

#[cfg(test)]
pub(crate) fn create_legacy_guard_script(relation: &str) -> String {
    format!(
        "{{:create {relation} {{id: String, tag: String}}}}\n\
         {{::index create {relation}:by_tag {{tag}}}}"
    )
}

#[cfg(test)]
pub(crate) fn membership_columns_are_exact(columns: &NamedRows) -> bool {
    columns.rows.len() == 4
        && column_is(&columns.rows[0], "tag", true, 0, |ty| ty == "String")
        && column_is(&columns.rows[1], "status", true, 1, |ty| ty == "String")
        && column_is(&columns.rows[2], "sample_hash", true, 2, |ty| ty == "Int")
        && column_is(&columns.rows[3], "id", true, 3, |ty| ty == "String")
}

#[cfg(test)]
pub(crate) fn legacy_writable_columns_are_exact(columns: &NamedRows) -> bool {
    columns.rows.len() == 2
        && column_is(&columns.rows[0], "id", true, 0, |ty| ty == "String")
        && column_is(&columns.rows[1], "tag", true, 1, |ty| ty == "String")
}

#[cfg(test)]
pub(crate) fn canonical_node_columns_are_exact(columns: &NamedRows) -> bool {
    columns.rows.len() == 3
        && column_is(&columns.rows[0], "id", true, 0, |ty| ty == "String")
        && column_is(&columns.rows[1], "data", false, 1, |ty| ty == "String")
        && column_is(&columns.rows[2], "status", false, 2, |ty| ty == "String")
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
    fn conventional_shapes_do_not_conflate_membership_or_either_legacy_generation() {
        let membership = columns(vec![
            column("tag", true, 0, "String"),
            column("status", true, 1, "String"),
            column("sample_hash", true, 2, "Int"),
            column("id", true, 3, "String"),
        ]);
        let old = columns(vec![
            column("id", true, 0, "String"),
            column("tag", true, 1, "String"),
        ]);
        assert!(membership_columns_are_exact(&membership));
        assert!(!legacy_writable_columns_are_exact(&membership));
        assert!(legacy_writable_columns_are_exact(&old));
    }

    #[test]
    fn physical_primary_key_order_is_part_of_the_shape() {
        let wrong = columns(vec![
            column("tag", true, 0, "String"),
            column("status", true, 1, "String"),
            column("id", true, 2, "String"),
            column("sample_hash", true, 3, "Int"),
        ]);
        assert!(!membership_columns_are_exact(&wrong));
    }

    #[test]
    fn generated_scripts_pin_conventional_keys_and_do_not_lower_scratch_access() {
        let membership = create_membership_script("shadow");
        assert!(membership.contains("{tag: String, status: String, sample_hash: Int, id: String}"));
        assert!(membership.contains("::index create shadow:by_id {id}"));

        let guard = create_legacy_guard_script("guard");
        assert!(guard.contains("{id: String, tag: String}"));
        assert!(guard.contains("::index create guard:by_tag {tag}"));
        assert!(!guard.contains("access_level"));
    }

    #[test]
    fn migration_pages_and_membership_canary_match_canonical_tag_bounds() {
        assert_eq!(NODE_AUDIT_PAGE, 64);
        assert_eq!(MEMBERSHIP_WRITE_CHUNK, 256);
        assert_eq!(MEMBERSHIP_VERIFY_RESULT_CAP, 64 * 64 + 1);
        let query = membership_verification_query("wanted[id] <- [[$id]]", "shadow");
        assert!(query.contains("*shadow:by_id"));
        assert!(query.contains(":limit 4097"));
    }
}
