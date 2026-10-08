//! Relation-role planning and independent post-close verification.

mod plan;
mod verify;

pub(super) use plan::plan_logical_snapshot_roles_v1;
pub(super) use verify::verify_closed_logical_snapshot_roles_v1;

#[cfg(test)]
pub(super) use verify::MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_V1;
