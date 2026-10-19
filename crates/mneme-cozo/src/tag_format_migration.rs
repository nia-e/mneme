//! Explicit lexical migration of materialized node tags, never submitted proofs.
//!
//! Ordinary edition/touchstone edit guards are deliberately not weakened. This
//! module plans a versioned format transition and changes only the canonical
//! node's top-level tag field. Embedded witnesses, source digests and all other
//! graph state remain exact historical evidence.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use mneme_core::ports::{Error, Result};
use mneme_core::{
    BoundedTagSet, Node, TAG_NORMALIZATION_VERSION, is_normalized_tag, normalize_tag,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ulid::Ulid;

use crate::StoreExport;

/// The existing explicit portable-migration envelope. This limits aggregate
/// serialization/work, not the number of useful memories allowed in a store.
pub const MAX_TAG_FORMAT_MIGRATION_BYTES: usize = 512 * 1024 * 1024;
pub const MAX_TAG_FORMAT_OPTIONS_BYTES: usize = 1024 * 1024;
const PLAN_SCHEMA: &str = "mneme.tag-format-migration-plan.v1";
const PROTECTED: &[&str] = &[
    "core",
    "routing-judgment",
    "collaboration-preference",
    "possibility",
    "pursuing",
    "closed",
];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagFormatMigrationOptions {
    /// Exact old spelling -> deliberately chosen canonical alias. This is not
    /// a fuzzy alias service: normalizable spellings cannot be semantically
    /// redirected, except an explicit nonprotected destination may prevent a
    /// noncanonical protected-name alias from acquiring protected membership.
    /// Literal protected names cannot be redirected or newly granted.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
}

impl TagFormatMigrationOptions {
    pub fn validate(&self) -> Result<()> {
        bounded_digest(self, MAX_TAG_FORMAT_OPTIONS_BYTES)?;
        for (original, replacement) in &self.aliases {
            mneme_core::validate_tag(original)
                .map_err(|error| Error::InvalidInput(error.to_string()))?;
            if !is_normalized_tag(replacement) {
                return Err(Error::InvalidInput(format!(
                    "explicit tag alias {original:?} has a noncanonical replacement"
                )));
            }
            if let Ok(lexical) = normalize_tag(original)
                && lexical != *replacement
                && !(lexical != *original
                    && PROTECTED.contains(&lexical.as_str())
                    && !PROTECTED.contains(&replacement.as_str()))
            {
                return Err(Error::InvalidInput(format!(
                    "tag alias {original:?} changes meaning beyond lexical normalization; only an explicit nonprotected destination for a noncanonical protected-name spelling is allowed"
                )));
            }
            if original != replacement && PROTECTED.contains(&replacement.as_str()) {
                return Err(Error::InvalidInput(format!(
                    "tag alias {original:?} would acquire protected tag {replacement:?}; choose a reviewed nonprotected canonical alias instead"
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn replacement(&self, tag: &str) -> std::result::Result<String, String> {
        let replacement = self
            .aliases
            .get(tag)
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| normalize_tag(tag).map_err(|error| error.to_string()))?;
        if replacement != tag && PROTECTED.contains(&replacement.as_str()) {
            return Err(format!(
                "normalization would acquire protected tag {replacement:?}; choose an explicit reviewed nonprotected canonical alias; immutable editions and touchstones cannot use ordinary retagging"
            ));
        }
        Ok(replacement)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagFormatRename {
    pub original: String,
    pub replacement: String,
    pub affected_nodes: usize,
    pub explicit_alias: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagFormatCollision {
    pub replacement: String,
    pub originals: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagFormatBlocker {
    pub original: String,
    pub affected_nodes: usize,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagFormatMigrationPlan {
    pub schema: String,
    pub normalization_version: String,
    pub db_id: Ulid,
    /// The preimage domain is explicit: a portable snapshot binds its entire
    /// canonical export; SQLite binds every exact canonical node row and marker.
    pub preimage_scope: String,
    pub preimage_sha256: String,
    pub options: TagFormatMigrationOptions,
    pub nodes_scanned: usize,
    pub nodes_changed: usize,
    pub episode_nodes_changed: usize,
    pub renames: Vec<TagFormatRename>,
    pub collisions: Vec<TagFormatCollision>,
    pub blockers: Vec<TagFormatBlocker>,
    pub unused_aliases: Vec<String>,
    pub ready: bool,
    pub plan_sha256: String,
}

impl TagFormatMigrationPlan {
    pub fn require_reviewed(&self, expected_db_id: Ulid, expected_plan: &str) -> Result<()> {
        if self.db_id != expected_db_id {
            return Err(Error::Conflict(
                "tag migration database identity mismatch; no changes made".into(),
            ));
        }
        if !self.ready {
            return Err(Error::Conflict(
                "tag migration has unresolved spellings/protected aliases; review the plan and choose explicit nonprotected aliases where allowed; ordinary immutable-node edit guards remain unchanged".into(),
            ));
        }
        if expected_plan.len() != 64 || expected_plan != self.plan_sha256 {
            return Err(Error::Conflict(
                "tag migration plan/preimage changed; review a fresh plan instead of retrying blindly"
                    .into(),
            ));
        }
        Ok(())
    }
}

pub(crate) struct TagFormatPlanner {
    options: TagFormatMigrationOptions,
    nodes: usize,
    changed: usize,
    episodes_changed: usize,
    spellings: BTreeMap<String, usize>,
}

impl TagFormatPlanner {
    pub(crate) fn new(options: &TagFormatMigrationOptions) -> Result<Self> {
        options.validate()?;
        Ok(Self {
            options: options.clone(),
            nodes: 0,
            changed: 0,
            episodes_changed: 0,
            spellings: BTreeMap::new(),
        })
    }

    pub(crate) fn observe(&mut self, node: &Node) {
        self.nodes += 1;
        let mut changed = false;
        for tag in node.tags() {
            *self.spellings.entry(tag.to_owned()).or_default() += 1;
            changed |= self
                .options
                .replacement(tag)
                .is_ok_and(|value| value != tag);
        }
        self.changed += usize::from(changed);
        self.episodes_changed += usize::from(changed && !node.is_semantic());
    }

    pub(crate) fn finish(
        self,
        db_id: Ulid,
        preimage_scope: &str,
        preimage_sha256: String,
    ) -> Result<TagFormatMigrationPlan> {
        let mut renames = Vec::new();
        let mut blockers = Vec::new();
        let mut by_replacement: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (original, affected_nodes) in &self.spellings {
            match self.options.replacement(original) {
                Ok(replacement) => {
                    by_replacement
                        .entry(replacement.clone())
                        .or_default()
                        .push(original.clone());
                    if replacement != *original {
                        renames.push(TagFormatRename {
                            original: original.clone(),
                            replacement,
                            affected_nodes: *affected_nodes,
                            explicit_alias: self.options.aliases.contains_key(original),
                        });
                    }
                }
                Err(reason) => blockers.push(TagFormatBlocker {
                    original: original.clone(),
                    affected_nodes: *affected_nodes,
                    reason,
                }),
            }
        }
        let collisions = by_replacement
            .into_iter()
            .filter(|(_, originals)| originals.len() > 1)
            .map(|(replacement, originals)| TagFormatCollision {
                replacement,
                originals,
            })
            .collect();
        let unused_aliases = self
            .options
            .aliases
            .keys()
            .filter(|tag| !self.spellings.contains_key(*tag))
            .cloned()
            .collect();
        let mut plan = TagFormatMigrationPlan {
            schema: PLAN_SCHEMA.into(),
            normalization_version: TAG_NORMALIZATION_VERSION.into(),
            db_id,
            preimage_scope: preimage_scope.into(),
            preimage_sha256,
            options: self.options,
            nodes_scanned: self.nodes,
            nodes_changed: self.changed,
            episode_nodes_changed: self.episodes_changed,
            renames,
            collisions,
            ready: blockers.is_empty(),
            blockers,
            unused_aliases,
            plan_sha256: String::new(),
        };
        plan.plan_sha256 = bounded_digest(&plan, MAX_TAG_FORMAT_MIGRATION_BYTES)?;
        Ok(plan)
    }
}

/// Plan a portable snapshot transition. No mutation, body access or inference.
pub fn plan_tag_format_export(
    export: &StoreExport,
    options: &TagFormatMigrationOptions,
) -> Result<TagFormatMigrationPlan> {
    // Bound before cloning/reordering the already-owned export.
    bounded_digest(export, MAX_TAG_FORMAT_MIGRATION_BYTES)?;
    let canonical = export.clone().canonical_value()?;
    let preimage = bounded_digest(&canonical, MAX_TAG_FORMAT_MIGRATION_BYTES)?;
    let mut planner = TagFormatPlanner::new(options)?;
    for node in &export.nodes {
        planner.observe(node);
    }
    planner.finish(export.db_id, "canonical-store-export-v1", preimage)
}

/// Apply only a reviewed exact portable preimage. Collisions coalesce complete
/// node sets, but remain explicit in the reviewed plan. Historical source proof
/// and nested witness bytes are never repaired or regenerated.
pub fn normalize_tag_format_export(
    export: &mut StoreExport,
    options: &TagFormatMigrationOptions,
    expected_db_id: Ulid,
    expected_plan: &str,
) -> Result<TagFormatMigrationPlan> {
    let plan = plan_tag_format_export(export, options)?;
    plan.require_reviewed(expected_db_id, expected_plan)?;
    // Prepare everything before replacing the first node, so even a validation
    // error leaves the caller's complete original export intact.
    let replacements = export
        .nodes
        .iter()
        .map(|node| normalized_node(node, options))
        .collect::<Result<Vec<_>>>()?;
    export.nodes = replacements;
    Ok(plan)
}

pub(crate) fn normalized_node(node: &Node, options: &TagFormatMigrationOptions) -> Result<Node> {
    let tags = node
        .tags()
        .map(|tag| options.replacement(tag).map_err(Error::InvalidInput))
        .collect::<Result<BTreeSet<_>>>()?;
    let mut replacement = node.clone();
    replacement
        .replace_tags(
            BoundedTagSet::try_from_iter(tags)
                .map_err(|error| Error::InvalidInput(error.to_string()))?,
        )
        .map_err(|error| Error::InvalidInput(error.to_string()))?;
    // This narrow format operation has no authority to alter any other field.
    let mut before = serde_json::to_value(node).map_err(serialize_error)?;
    let mut after = serde_json::to_value(&replacement).map_err(serialize_error)?;
    before.as_object_mut().expect("Node object").remove("tags");
    after.as_object_mut().expect("Node object").remove("tags");
    if before != after {
        return Err(Error::Conflict(
            "tag-only migration attempted to alter non-tag canonical content".into(),
        ));
    }
    Ok(replacement)
}

pub(crate) fn serialize_error(error: serde_json::Error) -> Error {
    Error::InvalidInput(format!("tag migration serialization failed: {error}"))
}

pub(crate) fn bounded_digest(value: &impl Serialize, limit: usize) -> Result<String> {
    let mut writer = DigestWriter {
        hash: Sha256::new(),
        bytes: 0,
        limit,
    };
    serde_json::to_writer(&mut writer, value).map_err(serialize_error)?;
    Ok(format!("{:x}", writer.hash.finalize()))
}

struct DigestWriter {
    hash: Sha256,
    bytes: usize,
    limit: usize,
}
impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|bytes| *bytes <= self.limit)
            .ok_or_else(|| std::io::Error::other("aggregate migration byte envelope exceeded"))?;
        self.hash.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use mneme_core::{BodyRef, NodeId, NodeStatus, Provenance};

    fn fixture(tags: &[&str]) -> StoreExport {
        let mut export = MemStore::new(4).export();
        export.nodes.push(
            Node::try_new(
                NodeId(Ulid::new()),
                "authored content is not tag spelling",
                BodyRef::new("inline://migration").unwrap(),
                tags.iter().copied(),
                Provenance::Conversation {
                    session: Ulid::new(),
                    turn: 0,
                },
                0.5,
                0.5,
                NodeStatus::Archived,
                42,
            )
            .unwrap(),
        );
        export
    }

    #[test]
    fn exact_plan_aliases_collisions_and_tag_only_replacement() {
        let mut export = fixture(&["Rust Compiler", "rust_compiler", "C++"]);
        let options = TagFormatMigrationOptions {
            aliases: BTreeMap::from([("C++".into(), "cpp".into())]),
        };
        let plan = plan_tag_format_export(&export, &options).unwrap();
        assert!(plan.ready);
        assert_eq!(plan.collisions[0].replacement, "rust-compiler");
        let before = export.nodes[0].clone();
        let id = export.db_id;
        normalize_tag_format_export(&mut export, &options, id, &plan.plan_sha256).unwrap();
        assert_eq!(
            export.nodes[0].tags().collect::<Vec<_>>(),
            ["cpp", "rust-compiler"]
        );
        let mut restored = export.nodes[0].clone();
        restored.replace_tags(before.tag_set().clone()).unwrap();
        assert_eq!(
            serde_json::to_value(restored).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        assert!(normalize_tag_format_export(&mut export, &options, id, &plan.plan_sha256).is_err());
    }

    #[test]
    fn rejects_unsupported_protected_and_ontology_aliases() {
        let mut export = fixture(&["C++", "Core"]);
        let options = TagFormatMigrationOptions::default();
        let plan = plan_tag_format_export(&export, &options).unwrap();
        assert!(!plan.ready);
        assert_eq!(plan.blockers.len(), 2);
        let before = export.clone().canonical_value().unwrap();
        let id = export.db_id;
        assert!(normalize_tag_format_export(&mut export, &options, id, &plan.plan_sha256).is_err());
        assert_eq!(export.canonical_value().unwrap(), before);
        for (original, replacement) in [
            ("rust", "java"),
            ("C++", "core"),
            ("C++", "CPP"),
            ("Core", "core"),
            ("core", "historical-core"),
            ("Core", "pursuing"),
            ("routing-judgment", "historical-routing"),
        ] {
            assert!(
                TagFormatMigrationOptions {
                    aliases: BTreeMap::from([(original.into(), replacement.into())]),
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn explicit_nonprotected_alias_avoids_protected_acquisition_only() {
        for original in [
            "Core",
            "CORE",
            "Routing_Judgment",
            "Collaboration Preference",
            "Possibility",
            "Pursuing",
            "Closed",
        ] {
            let mut export = fixture(&[original]);
            let options = TagFormatMigrationOptions {
                aliases: BTreeMap::from([(original.into(), "historical-topic".into())]),
            };
            let plan = plan_tag_format_export(&export, &options).unwrap();
            assert!(plan.ready);
            assert!(plan.renames[0].explicit_alias);
            let id = export.db_id;
            normalize_tag_format_export(&mut export, &options, id, &plan.plan_sha256).unwrap();
            assert_eq!(
                export.nodes[0].tags().collect::<Vec<_>>(),
                ["historical-topic"]
            );
        }
    }
}
