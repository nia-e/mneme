//! Exclusive offline lexical-format transition. No ordinary graph API can
//! obtain the legacy writer held inside this facade.

use std::path::Path;
use std::sync::Arc;

use mneme_store_path::StoreLease;
use sha2::{Digest, Sha256};

use super::*;
use crate::tag_format_migration::{
    MAX_TAG_FORMAT_MIGRATION_BYTES, TagFormatMigrationOptions, TagFormatMigrationPlan,
    TagFormatPlanner, normalized_node,
};

const PAGE_NODES: usize = 64;
const PAGE_MEMBERSHIPS: usize = 256;

/// Owns one exact legacy-tag database and its noncloneable process lease.
///
/// This is not a `GraphStore` and intentionally has no `Deref` or store getter.
/// The only write is a reviewed tag-only format transition. There is no
/// fallback from a failed ordinary current open into this authority.
pub struct SqliteTagFormatMigration {
    store: CozoStore,
    expected_db_id: Ulid,
    #[cfg(test)]
    before_commit_hook: Option<Box<dyn FnOnce() + Send>>,
}

struct SourceRow {
    id: String,
    data: String,
    status: String,
    node: Node,
}

impl SqliteTagFormatMigration {
    pub fn open_legacy(path: &Path, expected_db_id: Ulid, lease: StoreLease) -> Result<Self> {
        if expected_db_id.is_nil() || !path.is_absolute() {
            return Err(Error::InvalidInput(
                "tag-format migration requires an absolute existing path and non-nil expected database identity".into(),
            ));
        }
        let store = CozoStore::open_legacy_tag_migration_source(path, Arc::new(lease))?;
        if store.db_id() != expected_db_id {
            return Err(Error::Conflict(
                "tag migration database identity mismatch; no changes made".into(),
            ));
        }
        Ok(Self {
            store,
            expected_db_id,
            #[cfg(test)]
            before_commit_hook: None,
        })
    }

    /// Complete, bounded canonical inventory and exact semantic-index audit.
    /// No inference, body access, feedback activation or logical writes.
    pub fn plan(&self, options: &TagFormatMigrationOptions) -> Result<TagFormatMigrationPlan> {
        let (_, plan) = self.source_rows(options)?;
        Ok(plan)
    }

    /// Apply only the exact reviewed source/options. Every canonical row is
    /// checked inside the same transaction that changes node tags, semantic
    /// membership and the mandatory old-writer-fencing marker.
    pub fn apply(
        &mut self,
        options: &TagFormatMigrationOptions,
        expected_plan_sha256: &str,
    ) -> Result<TagFormatMigrationPlan> {
        self.apply_inner(options, expected_plan_sha256, FailureInjection::NONE)
    }

    /// Checkpoint and close the migrated SQLite handle before another ordinary
    /// current opener may consume its new policy. No work may follow this call.
    pub fn checkpoint(self) -> Result<()> {
        self.require_live_guards("before tag-format checkpoint")?;
        self.store.prepare_for_file_move()?;
        self.require_live_guards("after tag-format checkpoint")
    }

    fn source_rows(
        &self,
        options: &TagFormatMigrationOptions,
    ) -> Result<(Vec<SourceRow>, TagFormatMigrationPlan)> {
        let mut planner = TagFormatPlanner::new(options)?;
        self.require_live_guards("before tag-format source inventory")?;
        self.require_legacy_marker()?;
        let mut hash = Sha256::new();
        hash.update(b"mneme.sqlite.tag-format.canonical-row-preimage.v1\0");
        hash.update(self.expected_db_id.to_bytes());
        hash_part(
            &mut hash,
            crate::tag_projection::LEGACY_META_VALUE.as_bytes(),
        );
        let mut rows = Vec::new();
        let mut after: Option<String> = None;
        let mut bytes = 0usize;
        loop {
            let page = self.store.scan_primary_key(
                "node",
                PrimaryKeyScan {
                    prefix: Vec::new(),
                    lower: after.as_ref().map_or(PrimaryKeyScanBound::Unbounded, |id| {
                        PrimaryKeyScanBound::Excluded(vec![dv_str(id)])
                    }),
                    upper: PrimaryKeyScanBound::Unbounded,
                    direction: PrimaryKeyScanDirection::Ascending,
                    limit: PAGE_NODES,
                },
            )?;
            if page.rows.rows.is_empty() {
                break;
            }
            for row in &page.rows.rows {
                let node = decode_canonical_node_row(row)?;
                let id = want_str(&row[0])?.to_owned();
                let data = want_str(&row[1])?.to_owned();
                let status = want_str(&row[2])?.to_owned();
                if after.as_ref().is_some_and(|last| id <= *last) {
                    return Err(Error::Backend(
                        "tag migration source scan did not progress".into(),
                    ));
                }
                charge(&mut bytes, id.len() + data.len() + status.len())?;
                hash_part(&mut hash, id.as_bytes());
                hash_part(&mut hash, data.as_bytes());
                hash_part(&mut hash, status.as_bytes());
                planner.observe(&node);
                after = Some(id.clone());
                rows.push(SourceRow {
                    id,
                    data,
                    status,
                    node,
                });
            }
        }
        self.audit_membership(&rows, &mut bytes)?;
        self.require_legacy_marker()?;
        self.require_live_guards("after tag-format source/index inventory")?;
        hash.update((rows.len() as u64).to_be_bytes());
        let plan = planner.finish(
            self.expected_db_id,
            "sqlite-exact-canonical-node-rows-and-tag-policy-v1",
            format!("{:x}", hash.finalize()),
        )?;
        Ok((rows, plan))
    }

    fn require_legacy_marker(&self) -> Result<()> {
        if self.store.db_id != self.expected_db_id
            || self.store.read_meta("db_id")?.as_deref()
                != Some(self.expected_db_id.to_string().as_str())
            || self
                .store
                .read_meta(crate::tag_projection::META_KEY)?
                .as_deref()
                != Some(crate::tag_projection::LEGACY_META_VALUE)
        {
            return Err(Error::Conflict(
                "tag migration source identity/policy changed; preserve evidence and inspect current state, do not blindly retry".into(),
            ));
        }
        Ok(())
    }

    fn require_live_guards(&self, stage: &'static str) -> Result<()> {
        self.store
            .persistent_authority
            .as_ref()
            .ok_or_else(|| {
                Error::Conflict("tag-format migration lost its persistent lease authority".into())
            })?
            .require_live_guards(stage)
    }

    fn audit_membership(&self, rows: &[SourceRow], bytes: &mut usize) -> Result<()> {
        let expected_rows = rows
            .iter()
            .filter(|row| row.node.is_semantic())
            .try_fold(0usize, |sum, row| sum.checked_add(row.node.tag_set().len()))
            .ok_or_else(|| Error::InvalidInput("tag membership count overflow".into()))?;
        let mut actual_rows = 0usize;
        let mut after: Option<Vec<DataValue>> = None;
        loop {
            let page = self.store.scan_primary_key(
                "node_tag_v2",
                PrimaryKeyScan {
                    prefix: Vec::new(),
                    lower: after.clone().map_or(
                        PrimaryKeyScanBound::Unbounded,
                        PrimaryKeyScanBound::Excluded,
                    ),
                    upper: PrimaryKeyScanBound::Unbounded,
                    direction: PrimaryKeyScanDirection::Ascending,
                    limit: PAGE_MEMBERSHIPS
                        .min(expected_rows.saturating_sub(actual_rows).saturating_add(1)),
                },
            )?;
            if page.rows.rows.is_empty() {
                break;
            }
            for row in &page.rows.rows {
                actual_rows += 1;
                if actual_rows > expected_rows || row.len() != 4 {
                    return Err(Error::Conflict("tag migration found orphan/excess membership; repair the source separately".into()));
                }
                let tag = want_str(&row[0])?;
                let status = want_str(&row[1])?;
                let sample = want_i64(&row[2])?;
                let id = want_str(&row[3])?;
                charge(bytes, tag.len() + status.len() + id.len() + 8)?;
                let source = rows
                    .binary_search_by(|source| source.id.as_str().cmp(id))
                    .ok()
                    .map(|index| &rows[index].node)
                    .filter(|node| node.is_semantic())
                    .ok_or_else(|| Error::Conflict("tag migration found orphan/episode semantic membership; repair the source separately".into()))?;
                if !source.has_tag(tag)
                    || status != status_str(source.status())
                    || sample != stable_tag_sample_hash(source.id())
                {
                    return Err(Error::Conflict(
                        "tag migration found stale membership; repair the source separately".into(),
                    ));
                }
                after = Some(row.clone());
            }
        }
        if actual_rows != expected_rows {
            return Err(Error::Conflict(
                "tag migration found missing membership; repair the source separately".into(),
            ));
        }
        Ok(())
    }

    fn apply_inner(
        &mut self,
        options: &TagFormatMigrationOptions,
        expected_plan_sha256: &str,
        failure: FailureInjection,
    ) -> Result<TagFormatMigrationPlan> {
        let (rows, plan) = self.source_rows(options)?;
        plan.require_reviewed(self.expected_db_id, expected_plan_sha256)?;
        self.require_live_guards("immediately before tag-format write transaction")?;
        let tx = self.store.db.multi_transaction(true);
        let result = (|| {
            require_tx_pair(&tx, "db_id", &self.expected_db_id.to_string())?;
            require_tx_pair(
                &tx,
                crate::tag_projection::META_KEY,
                crate::tag_projection::LEGACY_META_VALUE,
            )?;
            let count = tx_run(&tx, "?[count(id)] := *node{id}", BTreeMap::new())?;
            let count = count
                .rows
                .first()
                .and_then(|row| row.first())
                .map(want_i64)
                .transpose()?;
            if count
                != Some(
                    i64::try_from(rows.len())
                        .map_err(|_| Error::InvalidInput("node count overflow".into()))?,
                )
            {
                return Err(Error::Conflict(
                    "tag migration canonical inventory changed inside write transaction".into(),
                ));
            }
            let mut written = 0;
            for source in &rows {
                let current = tx_run(
                    &tx,
                    "?[id,data,status] := id=$id, *node{id:$id,data,status}",
                    BTreeMap::from([("id".into(), dv_str(&source.id))]),
                )?;
                if current.rows.len() != 1
                    || want_str(&current.rows[0][0])? != source.id
                    || want_str(&current.rows[0][1])? != source.data
                    || want_str(&current.rows[0][2])? != source.status
                {
                    return Err(Error::Conflict(
                        "tag migration exact canonical preimage changed inside write transaction"
                            .into(),
                    ));
                }
                let replacement = normalized_node(&source.node, options)?;
                if replacement.tag_set() != source.node.tag_set() {
                    tx_run(
                        &tx,
                        "?[id,data,status] <- [[$id,$data,$status]] :put node {id=>data,status}",
                        BTreeMap::from([
                            ("id".into(), dv_str(&source.id)),
                            ("data".into(), dv_str(&encode_canonical_node(&replacement)?)),
                            ("status".into(), dv_str(&source.status)),
                        ]),
                    )?;
                    if replacement.is_semantic() {
                        // Episode edition tags are canonical authored metadata,
                        // never semantic retrieval membership.
                        maintenance::tx_sync_tag_projection(&tx, &[&replacement])?;
                    }
                    written += 1;
                    failure.check(written)?;
                }
            }
            tx_run(
                &tx,
                "?[k,v] <- [[$k,$v]] :put meta {k=>v}",
                BTreeMap::from([
                    ("k".into(), dv_str(crate::tag_projection::META_KEY)),
                    ("v".into(), dv_str(crate::tag_projection::META_VALUE)),
                ]),
            )?;
            failure.check(usize::MAX)?;
            require_tx_pair(&tx, "db_id", &self.expected_db_id.to_string())?;
            Ok(())
        })();
        #[cfg(test)]
        if let Some(hook) = self.before_commit_hook.take() {
            hook();
        }
        let result = result.and_then(|()| {
            self.require_live_guards("immediately before tag-format transaction commit")
        });
        overlay::finish_transaction(&tx, result)?;
        self.require_live_guards("after tag-format transaction commit; if lost preserve evidence and inspect the committed outcome")?;
        Ok(plan)
    }
}

fn require_tx_pair(tx: &MultiTransaction, key: &str, expected: &str) -> Result<()> {
    let rows = tx_run(
        tx,
        "?[v] := *meta{k:$key,v}",
        BTreeMap::from([("key".into(), dv_str(key))]),
    )?;
    if rows.rows.len() != 1 || want_str(&rows.rows[0][0])? != expected {
        return Err(Error::Conflict(
            "tag migration transaction identity/policy mismatch".into(),
        ));
    }
    Ok(())
}

fn charge(bytes: &mut usize, additional: usize) -> Result<()> {
    *bytes = bytes
        .checked_add(additional)
        .filter(|bytes| *bytes <= MAX_TAG_FORMAT_MIGRATION_BYTES)
        .ok_or_else(|| {
            Error::InvalidInput(
                "tag migration aggregate byte envelope exceeded; no partial migration is published"
                    .into(),
            )
        })?;
    Ok(())
}

fn hash_part(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

#[derive(Clone, Copy)]
struct FailureInjection {
    #[cfg(test)]
    after_write: Option<usize>,
    #[cfg(test)]
    exit_process: bool,
}
impl FailureInjection {
    const NONE: Self = Self {
        #[cfg(test)]
        after_write: None,
        #[cfg(test)]
        exit_process: false,
    };
    fn check(self, _stage: usize) -> Result<()> {
        #[cfg(test)]
        if self.after_write == Some(_stage) {
            if self.exit_process {
                std::process::exit(91);
            }
            return Err(Error::Backend(
                "injected tag-format transaction failure".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
