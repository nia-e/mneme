//! Portable store snapshots and their explicit predecessor codecs.
//!
//! This module owns wire shape and format-specific validation, independently of
//! either backend. It does not open stores or promise that a decoded export is
//! ready for every operation: strict import and repair-oriented load deliberately
//! retain different validation paths in their callers.

use std::collections::{BTreeMap, BTreeSet};

use mneme_core::concern::ConcernRow;
use mneme_core::ports::{Error, Result};
use mneme_core::touchstone::TouchstoneRecord;
use mneme_core::{
    Contradiction, Edge, EmbeddingFingerprint, FullMergeRecord, MemoryKind, MergeCandidate, Node,
    NodeId, Provenance, RemoteEdge, SupersedeRecord,
};
use serde::{Deserialize, Serialize};
use ulid::Ulid;
use unordered_pair::UnorderedPair;

/// Explicit predecessor-JSON admission limits; normal successor snapshots use
/// their own read path. The byte cap is checked both before and during reading
/// so a growing file cannot bypass the metadata check.
pub const MAX_LEGACY_JSON_UPGRADE_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_LEGACY_JSON_UPGRADE_NODES: usize = 100_000;

/// Portable semantic migration form of a store. Maps are flattened to lists
/// because JSON object keys must be strings, and our keys are node ids / unordered
/// pairs. Canonical graph state is lossless; volatile feedback retry diagnostics
/// are exported but deliberately not rehydrated by load/import.
///
/// `db_id` retains logical database identity across a migration/replacement. This
/// format deliberately contains no managed storage identity, schema/fence state,
/// or mutation epoch and therefore cannot represent an exact managed restore.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreExport {
    pub db_id: Ulid,
    pub dim: usize,
    /// Missing in v1 snapshots. Populated legacy stores are rejected by normal
    /// opens until `mnemed reembed` rebuilds and stamps their vectors.
    #[serde(default)]
    pub embedding_fingerprint: Option<EmbeddingFingerprint>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub vectors: Vec<(NodeId, Vec<f32>)>,
    pub contradictions: Vec<Contradiction>,
    #[serde(default)]
    pub merges: Vec<MergeCandidate>,
    #[serde(default)]
    pub full_merge_commits: Vec<FullMergeRecord>,
    #[serde(default)]
    pub supersede_commits: Vec<SupersedeRecord>,
    #[serde(default)]
    pub remote_edges: Vec<RemoteEdge>,
    #[serde(default)]
    pub feedback_retries: Vec<FeedbackRetryRecord>,
    #[serde(default)]
    pub concerns: Vec<ConcernRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touchstones: Vec<TouchstoneRecord>,
}

impl StoreExport {
    /// Exact logical migration comparison. Flattened keyed collections and
    /// unordered tags are canonicalized; no field/value is omitted or rounded.
    pub fn canonical_value(mut self) -> Result<serde_json::Value> {
        let export = &mut self;
        export.nodes.sort_by_key(Node::id);
        export.touchstones.sort_by_key(TouchstoneRecord::owner);
        export.concerns.sort_by_key(|row| {
            let key = row.binding().key();
            let [lo, hi] = key.endpoints();
            (lo, hi, key.kind())
        });
        export.edges.sort_by_key(|edge| (edge.from, edge.to));
        export.vectors.sort_by_key(|(id, _)| *id);
        for contradiction in &mut export.contradictions {
            let (lo, hi) = if contradiction.between.0 <= contradiction.between.1 {
                (contradiction.between.0, contradiction.between.1)
            } else {
                (contradiction.between.1, contradiction.between.0)
            };
            contradiction.between = UnorderedPair(lo, hi);
        }
        export
            .contradictions
            .sort_by_key(|item| (item.between.0, item.between.1));
        for merge in &mut export.merges {
            let (lo, hi) = if merge.between.0 <= merge.between.1 {
                (merge.between.0, merge.between.1)
            } else {
                (merge.between.1, merge.between.0)
            };
            merge.between = UnorderedPair(lo, hi);
        }
        export
            .merges
            .sort_by_key(|item| (item.between.0, item.between.1));
        for record in &mut export.full_merge_commits {
            let (lo, hi) = if record.between.0 <= record.between.1 {
                (record.between.0, record.between.1)
            } else {
                (record.between.1, record.between.0)
            };
            record.between = UnorderedPair(lo, hi);
        }
        export
            .full_merge_commits
            .sort_by_key(|item| (item.between.0, item.between.1));
        for record in &mut export.supersede_commits {
            let (lo, hi) = if record.between.0 <= record.between.1 {
                (record.between.0, record.between.1)
            } else {
                (record.between.1, record.between.0)
            };
            record.between = UnorderedPair(lo, hi);
        }
        export
            .supersede_commits
            .sort_by_key(|item| (item.between.0, item.between.1));
        export
            .remote_edges
            .sort_by_key(|edge| (edge.from, edge.target_db, edge.target));
        export.feedback_retries.sort_by(|a, b| a.key.cmp(&b.key));

        let mut value = serde_json::to_value(self).map_err(|error| {
            Error::Backend(format!("serialize migration verification: {error}"))
        })?;
        normalize_export_tag_arrays(&mut value);
        Ok(value)
    }
}

pub(crate) fn normalize_export_tag_arrays(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if key == "tags" {
                    if let serde_json::Value::Array(tags) = value {
                        tags.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
                    }
                } else {
                    normalize_export_tag_arrays(value);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_export_tag_arrays(item);
            }
        }
        _ => {}
    }
}

/// Canonical-tag writable successor. The schema change is intentional: genuine
/// v5 writers reject it before reconstructing a mutable store.
pub const STORE_EXPORT_SCHEMA_V6: &str = "mneme.store-export.v6";

#[derive(Clone, Debug)]
pub struct StoreExportEnvelopeV6 {
    pub store: StoreExport,
}
impl StoreExportEnvelopeV6 {
    pub fn new(store: StoreExport) -> Self {
        Self { store }
    }
    pub fn into_store(self) -> Result<StoreExport> {
        validate_canonical_export_tags(&self.store)?;
        validate_touchstone_import(&self.store)?;
        Ok(self.store)
    }
}
fn validate_canonical_export_tags(store: &StoreExport) -> Result<()> {
    if store
        .nodes
        .iter()
        .any(|node| node.tags().any(|tag| !mneme_core::is_normalized_tag(tag)))
    {
        return Err(Error::InvalidInput(
            "v6 snapshot contains noncanonical materialized tags; explicit migration required"
                .into(),
        ));
    }
    Ok(())
}
impl Serialize for StoreExportEnvelopeV6 {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        validate_canonical_export_tags(&self.store).map_err(serde::ser::Error::custom)?;
        let mut value = serde_json::to_value(StoreExportEnvelopeV5::new(self.store.clone()))
            .map_err(serde::ser::Error::custom)?;
        value["schema"] = STORE_EXPORT_SCHEMA_V6.into();
        value["tag_normalization_version"] = mneme_core::TAG_NORMALIZATION_VERSION.into();
        value.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for StoreExportEnvelopeV6 {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: String,
            tag_normalization_version: String,
            store: serde_json::Value,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.schema != STORE_EXPORT_SCHEMA_V6
            || wire.tag_normalization_version != mneme_core::TAG_NORMALIZATION_VERSION
        {
            return Err(serde::de::Error::custom(
                "unsupported snapshot schema or tag normalization version",
            ));
        }
        let legacy = serde_json::json!({"schema": STORE_EXPORT_SCHEMA_V5, "store": wire.store});
        let predecessor: StoreExportEnvelopeV5 =
            serde_json::from_value(legacy).map_err(serde::de::Error::custom)?;
        let store = predecessor.into_store().map_err(serde::de::Error::custom)?;
        validate_canonical_export_tags(&store).map_err(serde::de::Error::custom)?;
        Ok(Self { store })
    }
}

pub const STORE_EXPORT_SCHEMA_V5: &str = "mneme.store-export.v5";

#[derive(Clone, Debug)]
pub struct StoreExportEnvelopeV5 {
    pub schema: String,
    pub store: StoreExport,
}
impl Serialize for StoreExportEnvelopeV5 {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut store = serde_json::to_value(&self.store).map_err(serde::ser::Error::custom)?;
        store
            .as_object_mut()
            .ok_or_else(|| serde::ser::Error::custom("invalid export object"))?
            .insert(
                "touchstones".into(),
                serde_json::to_value(&self.store.touchstones).map_err(serde::ser::Error::custom)?,
            );
        #[derive(Serialize)]
        struct Wire<'a> {
            schema: &'a str,
            store: serde_json::Value,
        }
        Wire {
            schema: &self.schema,
            store,
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for StoreExportEnvelopeV5 {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: String,
            store: serde_json::Value,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.schema != STORE_EXPORT_SCHEMA_V5
            || wire.store.get("concerns").is_none()
            || wire.store.get("touchstones").is_none()
        {
            return Err(serde::de::Error::custom(
                "v5 snapshot requires exact schema, concerns and touchstones fields",
            ));
        }
        let store = serde_json::from_value(wire.store).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema: wire.schema,
            store,
        })
    }
}
impl StoreExportEnvelopeV5 {
    pub fn new(store: StoreExport) -> Self {
        Self {
            schema: STORE_EXPORT_SCHEMA_V5.into(),
            store,
        }
    }
    pub fn into_store(self) -> Result<StoreExport> {
        if self.schema != STORE_EXPORT_SCHEMA_V5 {
            return Err(Error::InvalidInput(
                "unsupported store-export schema".into(),
            ));
        }
        validate_touchstone_import(&self.store)?;
        Ok(self.store)
    }
}

pub const STORE_EXPORT_SCHEMA_V4: &str = "mneme.store-export.v4";

#[derive(Clone, Debug)]
pub struct StoreExportEnvelopeV4 {
    pub schema: String,
    pub store: StoreExport,
}

impl Serialize for StoreExportEnvelopeV4 {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        validate_pre_touchstone_export(&self.store).map_err(serde::ser::Error::custom)?;
        #[derive(Serialize)]
        struct Wire<'a> {
            schema: &'a str,
            store: &'a StoreExport,
        }
        Wire {
            schema: &self.schema,
            store: &self.store,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StoreExportEnvelopeV4 {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: String,
            store: serde_json::Value,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.schema != STORE_EXPORT_SCHEMA_V4 {
            return Err(serde::de::Error::custom(
                "unsupported store-export schema; predecessor requires explicit upgrade",
            ));
        }
        if wire.store.get("concerns").is_none() {
            return Err(serde::de::Error::custom(
                "v4 snapshot requires concerns field",
            ));
        }
        if wire.store.get("touchstones").is_some() {
            return Err(serde::de::Error::custom(
                "v4 predecessor cannot contain touchstones",
            ));
        }
        let store = serde_json::from_value(wire.store).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema: wire.schema,
            store,
        })
    }
}

impl StoreExportEnvelopeV4 {
    pub fn new(store: StoreExport) -> Self {
        Self {
            schema: STORE_EXPORT_SCHEMA_V4.into(),
            store,
        }
    }
    pub fn into_store(self) -> Result<StoreExport> {
        if self.schema != STORE_EXPORT_SCHEMA_V4 {
            return Err(Error::InvalidInput(
                "unsupported store-export schema".into(),
            ));
        }
        validate_pre_touchstone_export(&self.store)?;
        Ok(self.store)
    }
}

pub const STORE_EXPORT_SCHEMA_V3: &str = "mneme.store-export.v3";

#[derive(Clone, Debug)]
pub struct StoreExportEnvelopeV3 {
    pub schema: String,
    pub store: StoreExport,
}

impl Serialize for StoreExportEnvelopeV3 {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        validate_pre_context_export(&self.store).map_err(serde::ser::Error::custom)?;
        #[derive(Serialize)]
        struct Wire<'a> {
            schema: &'a str,
            store: &'a StoreExport,
        }
        Wire {
            schema: &self.schema,
            store: &self.store,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StoreExportEnvelopeV3 {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: String,
            store: serde_json::Value,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.schema != STORE_EXPORT_SCHEMA_V3 {
            return Err(serde::de::Error::custom(
                "unsupported store-export schema; predecessor requires explicit upgrade",
            ));
        }
        if wire.store.get("concerns").is_none() {
            return Err(serde::de::Error::custom(
                "v3 snapshot requires concerns field",
            ));
        }
        validate_pre_context_store_value(&wire.store).map_err(serde::de::Error::custom)?;
        let store = serde_json::from_value(wire.store).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema: wire.schema,
            store,
        })
    }
}

impl StoreExportEnvelopeV3 {
    pub fn new(store: StoreExport) -> Self {
        Self {
            schema: STORE_EXPORT_SCHEMA_V3.into(),
            store,
        }
    }
    pub fn into_store(self) -> Result<StoreExport> {
        if self.schema != STORE_EXPORT_SCHEMA_V3 {
            return Err(Error::InvalidInput(
                "unsupported store-export schema".into(),
            ));
        }
        validate_pre_context_export(&self.store)?;
        Ok(self.store)
    }
}

pub const STORE_EXPORT_SCHEMA_V2: &str = "mneme.store-export.v2";

/// Incompatible successor JSON envelope. The former flat parser requires a
/// top-level `dim`, so it cannot silently admit this generation as writable.
#[derive(Clone, Debug)]
pub struct StoreExportEnvelopeV2 {
    pub schema: String,
    pub store: StoreExport,
}

impl Serialize for StoreExportEnvelopeV2 {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        validate_pre_context_export(&self.store).map_err(serde::ser::Error::custom)?;
        if !self.store.concerns.is_empty() {
            return Err(serde::ser::Error::custom("v2 cannot contain concerns"));
        }
        let mut store = serde_json::to_value(&self.store).map_err(serde::ser::Error::custom)?;
        store
            .as_object_mut()
            .expect("export object")
            .remove("concerns");
        #[derive(Serialize)]
        struct Wire<'a> {
            schema: &'a str,
            store: serde_json::Value,
        }
        Wire {
            schema: &self.schema,
            store,
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for StoreExportEnvelopeV2 {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: String,
            store: serde_json::Value,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.schema != STORE_EXPORT_SCHEMA_V2 {
            return Err(serde::de::Error::custom("unsupported store-export schema"));
        }
        if wire.store.get("concerns").is_some() {
            return Err(serde::de::Error::custom("v2 cannot contain concerns field"));
        }
        validate_pre_context_store_value(&wire.store).map_err(serde::de::Error::custom)?;
        let store = serde_json::from_value(wire.store).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema: wire.schema,
            store,
        })
    }
}

impl StoreExportEnvelopeV2 {
    pub fn new(store: StoreExport) -> Self {
        Self {
            schema: STORE_EXPORT_SCHEMA_V2.into(),
            store,
        }
    }
    pub fn into_store(self) -> Result<StoreExport> {
        if self.schema != STORE_EXPORT_SCHEMA_V2 {
            return Err(Error::InvalidInput(
                "unsupported store-export schema".into(),
            ));
        }
        validate_pre_context_export(&self.store)?;
        Ok(self.store)
    }
}

/// Named predecessor DTO. Only this explicit import path accepts pre-codec
/// canonical nodes; it retains every old field before normalizing membership.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyStoreExportV1 {
    pub db_id: Ulid,
    pub dim: usize,
    #[serde(default)]
    pub embedding_fingerprint: Option<EmbeddingFingerprint>,
    #[serde(serialize_with = "serialize_pre_context_nodes")]
    pub nodes: Vec<serde_json::Value>,
    pub edges: Vec<Edge>,
    pub vectors: Vec<(NodeId, Vec<f32>)>,
    pub contradictions: Vec<Contradiction>,
    #[serde(default)]
    pub merges: Vec<MergeCandidate>,
    #[serde(default)]
    pub full_merge_commits: Vec<FullMergeRecord>,
    #[serde(default)]
    pub supersede_commits: Vec<SupersedeRecord>,
    #[serde(default)]
    pub remote_edges: Vec<RemoteEdge>,
    #[serde(default)]
    pub feedback_retries: Vec<FeedbackRetryRecord>,
}

fn serialize_pre_context_nodes<S: serde::Serializer>(
    nodes: &[serde_json::Value],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    for node in nodes {
        validate_pre_context_node_value(node).map_err(serde::ser::Error::custom)?;
    }
    nodes.serialize(serializer)
}

/// Raw predecessor fence, shared by named JSON and SQLite migrations. Presence
/// is checked before typed decoding can turn explicit null into unknown.
pub(crate) fn validate_pre_context_node_value(value: &serde_json::Value) -> Result<()> {
    if value
        .get("memory_kind")
        .and_then(|kind| kind.get("episode"))
        .and_then(serde_json::Value::as_object)
        .is_some_and(|facet| facet.contains_key("occurrence_contexts"))
    {
        return Err(Error::InvalidInput(
            "predecessor episode cannot contain occurrence_contexts field".into(),
        ));
    }
    if value
        .get("provenance")
        .and_then(|provenance| provenance.get("External"))
        .and_then(|external| external.get("source"))
        .and_then(|source| source.get("request_codec"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|codec| matches!(codec, "episode_v2" | "touchstone_v1"))
    {
        return Err(Error::InvalidInput(
            "predecessor node cannot carry episode_v2 request codec".into(),
        ));
    }
    Ok(())
}

fn validate_pre_context_store_value(value: &serde_json::Value) -> Result<()> {
    if value.get("touchstones").is_some() {
        return Err(Error::InvalidInput(
            "predecessor cannot contain touchstones field".into(),
        ));
    }
    if let Some(nodes) = value.get("nodes").and_then(serde_json::Value::as_array) {
        for node in nodes {
            validate_pre_context_node_value(node)?;
        }
    }
    Ok(())
}

pub(crate) fn validate_pre_context_export(export: &StoreExport) -> Result<()> {
    validate_pre_touchstone_export(export)?;
    for node in &export.nodes {
        if node
            .episode()
            .is_some_and(|facet| facet.occurrence_contexts().is_some())
            || matches!(node.provenance(), Provenance::External { source }
                if source.request_codec() == mneme_core::CaptureRequestCodec::EpisodeV2)
        {
            return Err(Error::InvalidInput(
                "predecessor export cannot contain occurrence contexts or episode_v2 codec".into(),
            ));
        }
    }
    Ok(())
}

/// Decode one predecessor canonical row at the named migration boundary.
/// The status result is its validated *original* physical-key status, not the
/// normalized destination membership. Source commitments use raw predecessor
/// bytes; destination commitments must be computed independently.
#[cfg(any(feature = "cozo", test))]
pub(crate) fn decode_legacy_node_v1(data: &str) -> Result<(Node, &'static str)> {
    let value: serde_json::Value = serde_json::from_str(data)
        .map_err(|e| Error::InvalidInput(format!("invalid legacy node JSON: {e}")))?;
    decode_legacy_node_v1_value(value)
}

fn decode_legacy_node_v1_value(mut value: serde_json::Value) -> Result<(Node, &'static str)> {
    validate_pre_context_node_value(&value)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| Error::InvalidInput("legacy node is not an object".into()))?;
    let status = object
        .get_mut("status")
        .ok_or_else(|| Error::InvalidInput("legacy node lacks status".into()))?;
    let original_status = match status {
        serde_json::Value::String(s) if s == "Active" => "active",
        serde_json::Value::String(s) if s == "Archived" => "archived",
        serde_json::Value::Object(map) if map.len() == 1 && map.contains_key("Candidate") => {
            let candidate = map.get("Candidate").unwrap();
            let count = candidate
                .get("use_count")
                .and_then(serde_json::Value::as_u64);
            if count.is_none_or(|count| count > u32::MAX as u64)
                || candidate.as_object().is_none_or(|fields| fields.len() != 1)
            {
                return Err(Error::InvalidInput(
                    "invalid legacy Candidate status".into(),
                ));
            }
            *status = serde_json::Value::String("Active".into());
            "candidate"
        }
        _ => return Err(Error::InvalidInput("unknown legacy node status".into())),
    };
    let episode = match object.get("memory_kind") {
        None => false,
        Some(kind) => matches!(
            serde_json::from_value::<MemoryKind>(kind.clone())
                .map_err(|e| Error::InvalidInput(format!("invalid legacy memory kind: {e}")))?,
            MemoryKind::Episode(_)
        ),
    };
    if let Some(provenance) = object.get_mut("provenance") {
        if let Some(source) = provenance
            .get_mut("External")
            .and_then(|v| v.get_mut("source"))
        {
            let source = source.as_object_mut().ok_or_else(|| {
                Error::InvalidInput("legacy capture source is not an object".into())
            })?;
            if source.contains_key("request_codec") {
                return Err(Error::InvalidInput(
                    "legacy capture source carries successor codec".into(),
                ));
            }
            source.insert(
                "request_codec".into(),
                serde_json::Value::String(if episode { "episode_v1" } else { "capture_v1" }.into()),
            );
        }
    }
    let node: Node = serde_json::from_value(value)
        .map_err(|e| Error::InvalidInput(format!("invalid legacy node: {e}")))?;
    node.validate()
        .map_err(|e| Error::InvalidInput(e.to_string()))?;
    Ok((node, original_status))
}

impl LegacyStoreExportV1 {
    pub fn into_current(self) -> Result<StoreExport> {
        if self.nodes.len() > MAX_LEGACY_JSON_UPGRADE_NODES {
            return Err(Error::InvalidInput(
                "legacy snapshot exceeds node limit".into(),
            ));
        }
        let mut nodes = Vec::with_capacity(self.nodes.len());
        for value in self.nodes {
            nodes.push(decode_legacy_node_v1_value(value)?.0);
        }
        Ok(StoreExport {
            db_id: self.db_id,
            dim: self.dim,
            embedding_fingerprint: self.embedding_fingerprint,
            nodes,
            edges: self.edges,
            vectors: self.vectors,
            contradictions: self.contradictions,
            merges: self.merges,
            touchstones: Vec::new(),
            full_merge_commits: self.full_merge_commits,
            supersede_commits: self.supersede_commits,
            remote_edges: self.remote_edges,
            feedback_retries: self.feedback_retries,
            concerns: Vec::new(),
        })
    }
}

/// Volatile adapter record proving an exact receipt-feedback payload has already
/// committed in the current host authority generation. Public only because
/// [`StoreExport`] exposes operational diagnostics; load/import discards it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackRetryRecord {
    pub key: String,
    pub fingerprint: String,
    #[serde(default = "legacy_feedback_epoch")]
    pub epoch: String,
    #[serde(default)]
    pub sequence: u64,
}

pub(crate) const LEGACY_FEEDBACK_EPOCH: &str = "legacy-wall-clock-v1";

fn legacy_feedback_epoch() -> String {
    LEGACY_FEEDBACK_EPOCH.into()
}

pub(crate) fn validate_pre_touchstone_export(export: &StoreExport) -> Result<()> {
    if !export.touchstones.is_empty()
        || export.nodes.iter().any(|node| {
            matches!(node.provenance(), Provenance::External { source }
            if source.request_codec() == mneme_core::CaptureRequestCodec::TouchstoneV1)
        })
    {
        return Err(Error::InvalidInput(
            "predecessor export cannot contain touchstones or touchstone_v1 codec".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_touchstone_import(export: &StoreExport) -> Result<()> {
    let nodes = export
        .nodes
        .iter()
        .map(|node| (node.id(), node))
        .collect::<BTreeMap<_, _>>();
    let vectors = export
        .vectors
        .iter()
        .map(|(id, _)| *id)
        .collect::<BTreeSet<_>>();
    let mut owners = BTreeSet::new();
    for record in &export.touchstones {
        if !owners.insert(record.owner()) {
            return Err(Error::InvalidInput("duplicate touchstone owner".into()));
        }
        let node = nodes
            .get(&record.owner())
            .ok_or_else(|| Error::InvalidInput("touchstone has missing owner".into()))?;
        record.validate_owner(export.db_id, node)?;
        if !vectors.contains(&record.owner()) {
            return Err(Error::InvalidInput(
                "touchstone owner has no searchable vector projection".into(),
            ));
        }
    }
    for node in &export.nodes {
        if matches!(node.provenance(), Provenance::External {source}
            if source.request_codec() == mneme_core::CaptureRequestCodec::TouchstoneV1)
            && !owners.contains(&node.id())
        {
            return Err(Error::InvalidInput(
                "touchstone capture owner is missing immutable record".into(),
            ));
        }
    }
    Ok(())
}
