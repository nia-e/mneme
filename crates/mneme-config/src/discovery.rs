//! Sparse declarations and bounded, pure catalog resolution. This module never
//! contacts an owner, reads a file, or confers native owner capabilities.
use crate::*;
use serde_json::Value;
use std::collections::VecDeque;

pub const DISCOVERY_CATALOG_SCHEMA: &str = "mneme.discovery-catalog.v1";
pub const MAX_CATALOG_BYTES: usize = 65_536;
pub const MAX_DISCOVERY_PUBLISHERS: usize = 64;
pub const MAX_DISCOVERY_EDGES: usize = MAX_DISCOVERY_PUBLISHERS * MAX_STORES;
pub const MAX_DISCOVERY_BYTES: usize = 1_048_576;
pub const MAX_DISCOVERY_REPORT_BYTES: usize = 1_048_576;
const MAX_DECLARATIONS: usize = MAX_DISCOVERY_PUBLISHERS * MAX_STORES;

macro_rules! sparse {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $(#[serde(default, skip_serializing_if="Option::is_none")] pub $field: Option<$ty>,)* }
    };
}
sparse!(OwnerDeclaration {
    url: String,
    db: String,
    ssh_mcp_port: u16,
    token_env: String,
    path: PathBuf
});
sparse!(BackgroundDeclaration {
    enabled: bool,
    model: String,
    effort: String
});
sparse!(ActivationDeclaration { enabled: bool, workspace: WorkspaceKind, roots: Vec<PathBuf>, subtree_roots: Vec<PathBuf>, profiles: Vec<String>, excluded_roots: Vec<PathBuf> });
sparse!(PermissionsDeclaration {
    read: bool,
    write: bool
});
sparse!(DeliveryDeclaration {
    core: bool,
    context: bool,
    contradiction_notice: bool
});
sparse!(StoreBackgroundDeclaration {
    recall: bool,
    record: bool,
    maintenance: bool,
    core_cleanup: bool
});
sparse!(GuidanceDeclaration {
    tag_guide: GuidanceSelector
});
sparse!(OperationScopeDeclaration { roots: Vec<PathBuf>, subtree_roots: Vec<PathBuf>, excluded_roots: Vec<PathBuf> });
sparse!(ApplicabilityDeclaration {
    core: OperationScopeDeclaration,
    context: OperationScopeDeclaration,
    recall: OperationScopeDeclaration,
    record: OperationScopeDeclaration,
    maintenance: OperationScopeDeclaration
});
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptureDeclaration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<CaptureIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<CaptureKind>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_kinds: Option<Vec<EvidenceKind>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "optional_required_tags"
    )]
    pub required_tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_associations: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_summary_bytes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_bytes: Option<u32>,
}
fn optional_required_tags<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error> {
    deserialize_required_tags(deserializer).map(Some)
}

/// Device patches deliberately cannot carry owner routes, secret references,
/// executable settings, local library catalogs or arbitrary local-open targets.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeviceStoreDeclaration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<ActivationDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionsDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<StoreBackgroundDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance: Option<GuidanceDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<Cache>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval: Option<RetrievalFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applicability: Option<ApplicabilityDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureDeclaration>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoreDeclaration {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<OwnerDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<ActivationDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionsDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<StoreBackgroundDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance: Option<GuidanceDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<Cache>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval: Option<RetrievalFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applicability: Option<ApplicabilityDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureDeclaration>,
    /// Explicit nullable clears. Omission and null in serialized DTOs are not clears.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub devices: BTreeMap<String, DeviceStoreDeclaration>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfigDeclarations {
    pub schema: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<BackgroundDeclaration>,
    #[serde(default)]
    pub workspace: Workspace,
    #[serde(default)]
    pub stores: BTreeMap<String, StoreDeclaration>,
    #[serde(default)]
    pub libraries: BTreeMap<String, Library>,
}
impl Default for ConfigDeclarations {
    fn default() -> Self {
        Self {
            schema: CONFIG_SCHEMA.into(),
            device: None,
            read_default: None,
            background: None,
            workspace: Workspace::default(),
            stores: BTreeMap::new(),
            libraries: BTreeMap::new(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoverySeed {
    pub label: String,
    pub id: String,
    pub owner: Owner,
}

impl OwnerDeclaration {
    pub fn materialize(&self) -> Result<Owner> {
        let owner = Owner {
            url: self
                .url
                .clone()
                .ok_or_else(|| error("owner route requires url"))?,
            db: self
                .db
                .clone()
                .ok_or_else(|| error("owner route requires db"))?,
            ssh_mcp_port: self.ssh_mcp_port,
            token_env: self.token_env.clone(),
            path: self.path.clone(),
        };
        validate_owner(&owner)?;
        Ok(owner)
    }
    fn overlay(&self, base: &Owner) -> Result<Owner> {
        let owner = Owner {
            url: self.url.clone().unwrap_or_else(|| base.url.clone()),
            db: self.db.clone().unwrap_or_else(|| base.db.clone()),
            ssh_mcp_port: self.ssh_mcp_port.or(base.ssh_mcp_port),
            token_env: self.token_env.clone(),
            path: self.path.clone().or_else(|| base.path.clone()),
        };
        validate_owner(&owner)?;
        Ok(owner)
    }
}
impl ConfigDeclarations {
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() > MAX_CONFIG_BYTES {
            return Err(error("configuration exceeds 65536 bytes"));
        }
        let declarations: Self = toml::from_str(text).map_err(|_| {
            error("invalid Mneme TOML: check closed fields, types and duplicate keys")
        })?;
        declarations.validate()?;
        Ok(declarations)
    }
    pub fn parse_overlay(text: &str) -> Result<Self> {
        Self::parse(text)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != CONFIG_SCHEMA {
            return Err(error(
                "unsupported configuration schema; expected mneme.config.v1",
            ));
        }
        if let Some(device) = &self.device {
            validate_name(device)?;
        }
        if self.stores.len() > MAX_STORES || self.libraries.len() > MAX_LIBRARIES {
            return Err(error("configuration exceeds store/library count bound"));
        }
        if let Some(name) = &self.read_default {
            validate_name(name)?;
        }
        let mut skeleton = Config {
            schema: self.schema.clone(),
            read_default: default_rule(),
            background: self.materialize_background(),
            workspace: self.workspace.clone(),
            stores: BTreeMap::new(),
            libraries: self.libraries.clone(),
        };
        skeleton.workspace.allow_stores = None;
        skeleton.validate_inner(false)?;
        if let Some(names) = &self.workspace.allow_stores {
            unique_names(names)?;
        }
        let mut ids = BTreeSet::new();
        for (label, declaration) in &self.stores {
            validate_name(label)?;
            if !ids.insert(&declaration.id) {
                return Err(error(
                    "each canonical database ID must have exactly one local binding",
                ));
            }
            declaration.validate(false)?;
        }
        Ok(())
    }
    fn materialize_background(&self) -> Background {
        let mut background = Background::default();
        if let Some(value) = &self.background {
            if let Some(enabled) = value.enabled {
                background.enabled = enabled;
            }
            if let Some(model) = &value.model {
                background.model = Some(model.clone());
            }
            if let Some(effort) = &value.effort {
                background.effort = Some(effort.clone());
            }
        }
        background
    }
    pub fn has_discovery(&self) -> bool {
        self.stores
            .values()
            .any(|s| s.selected_discovery(self.device.as_deref()) == Some(true))
    }
    pub fn discovery_seeds(&self) -> Result<Vec<DiscoverySeed>> {
        self.validate()?;
        let seeds: Vec<_> = self
            .stores
            .iter()
            .filter(|(_, store)| store.selected_discovery(self.device.as_deref()) == Some(true))
            .filter_map(|(label, store)| {
                store
                    .owner
                    .as_ref()
                    .and_then(|o| o.materialize().ok())
                    .map(|owner| DiscoverySeed {
                        label: label.clone(),
                        id: store.id.clone(),
                        owner,
                    })
            })
            .collect();
        Ok(seeds)
    }
    /// Local-only materialization refuses unresolved local-only policy. With
    /// discovery enabled, sparse imported bindings may remain absent; use
    /// resolve_discovery for diagnostics.
    pub fn materialize(&self) -> Result<Config> {
        let resolution = resolve_discovery(self, &DiscoveryGraph::default())?;
        if !self.has_discovery() && resolution.report.stores.values().any(|s| s.blocked) {
            return Err(error(
                "local configuration has unresolved owner or invalid policy",
            ));
        }
        if !self.has_discovery() {
            resolution.config.validate()?;
        }
        Ok(resolution.config)
    }
}

impl StoreDeclaration {
    pub fn selected_discovery(&self, device: Option<&str>) -> Option<bool> {
        device
            .and_then(|d| self.devices.get(d))
            .and_then(|p| p.discovery)
            .or(self.discovery)
    }
    pub fn validate(&self, imported: bool) -> Result<()> {
        validate_id(&self.id)?;
        if self.devices.len() > 32 {
            return Err(error("store declaration exceeds device count bound"));
        }
        if let Some(owner) = &self.owner {
            if imported && owner.token_env.is_some() {
                return Err(error(
                    "published declarations cannot choose local secret references",
                ));
            }
            if owner.token_env.is_some() && owner.materialize().is_err() {
                return Err(error(
                    "local token_env requires an explicitly selected complete local route",
                ));
            }
            if imported {
                owner.materialize()?;
            } else {
                if let Some(url) = &owner.url {
                    let ssh = url.starts_with("ssh:");
                    validate_endpoint(
                        url,
                        if ssh {
                            owner.ssh_mcp_port.or(Some(1))
                        } else {
                            owner.ssh_mcp_port
                        },
                        owner.token_env.as_deref(),
                    )?;
                }
                if let Some(db) = &owner.db {
                    validate_name(db)?;
                }
                if owner.ssh_mcp_port == Some(0) {
                    return Err(error("SSH port must be nonzero"));
                }
                if let Some(path) = &owner.path {
                    validate_owner_path(path)?;
                }
            }
        }
        let fields = self.fields(None)?;
        validate_fields(&fields, !imported)?;
        validate_clears(&self.clear, &fields)?;
        for (device, patch) in &self.devices {
            validate_name(device)?;
            let patch_fields = declaration_fields(patch)?;
            validate_fields(&patch_fields, true)?;
            validate_clears(&patch.clear, &patch_fields)?;
        }
        Ok(())
    }
    fn fields(&self, device: Option<&str>) -> Result<BTreeMap<String, Value>> {
        let mut fields = declaration_fields(self)?;
        if let Some(patch) = device.and_then(|d| self.devices.get(d)) {
            fields.extend(declaration_fields(patch)?);
        }
        Ok(fields)
    }
}
fn validate_owner_path(path: &Path) -> Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| error("owner path must be UTF-8"))?;
    bounded_text(text, 4096)?;
    if !text.starts_with('/')
        && !(text.len() >= 3
            && text.as_bytes()[0].is_ascii_alphabetic()
            && text.as_bytes()[1] == b':'
            && matches!(text.as_bytes()[2], b'\\' | b'/'))
    {
        return Err(error("owner path evidence must be owner-side absolute"));
    }
    Ok(())
}
fn validate_owner(owner: &Owner) -> Result<()> {
    validate_endpoint(&owner.url, owner.ssh_mcp_port, owner.token_env.as_deref())?;
    validate_name(&owner.db)?;
    if let Some(path) = &owner.path {
        validate_owner_path(path)?;
    }
    Ok(())
}
fn declaration_fields<T: Serialize>(declaration: &T) -> Result<BTreeMap<String, Value>> {
    let value =
        serde_json::to_value(declaration).map_err(|_| error("cannot serialize declaration"))?;
    let mut fields = BTreeMap::new();
    fn walk(prefix: &str, value: &Value, fields: &mut BTreeMap<String, Value>) {
        if matches!(prefix, "id" | "devices" | "clear") {
            return;
        }
        if matches!(
            prefix,
            "owner" | "cache" | "retrieval" | "guidance.tag_guide"
        ) {
            fields.insert(prefix.into(), value.clone());
        } else if let Some(object) = value.as_object() {
            for (key, value) in object {
                walk(
                    &if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    },
                    value,
                    fields,
                );
            }
        } else if !prefix.is_empty() {
            fields.insert(prefix.into(), value.clone());
        }
    }
    walk("", &value, &mut fields);
    if let Some(clears) = value.get("clear").and_then(Value::as_array) {
        for field in clears.iter().filter_map(Value::as_str) {
            if fields.contains_key(field) {
                return Err(error("cannot set and clear the same nullable field"));
            }
            fields.insert(field.into(), Value::Null);
        }
    }
    Ok(fields)
}
fn validate_clears(clears: &[String], fields: &BTreeMap<String, Value>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for field in clears {
        if !matches!(field.as_str(), "cache" | "retrieval" | "guidance.tag_guide")
            || !seen.insert(field)
        {
            return Err(error(
                "clear must contain unique nullable fields: cache, retrieval, guidance.tag_guide",
            ));
        }
        // The serializer normalizes a clear to null; conflicting set+clear is
        // rejected separately below using the declaration's raw representation.
        if fields.get(field).is_none_or(|v| !v.is_null()) {
            return Err(error("invalid nullable clear"));
        }
    }
    Ok(())
}
fn validate_fields(fields: &BTreeMap<String, Value>, paths_allowed: bool) -> Result<()> {
    for scope in [
        "active",
        "applicability.core",
        "applicability.context",
        "applicability.recall",
        "applicability.record",
        "applicability.maintenance",
    ] {
        let paths = |suffix: &str| -> Result<Vec<PathBuf>> {
            fields
                .get(&format!("{scope}.{suffix}"))
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map(|v| v.unwrap_or_default())
                .map_err(|_| error("invalid scope paths"))
        };
        validate_path_lists(&[&paths("roots")?, &paths("subtree_roots")?])?;
    }
    for (field, value) in fields {
        if field == "owner" || value.is_null() {
            continue;
        }
        if field.ends_with(".roots")
            || field.ends_with(".subtree_roots")
            || field.ends_with(".excluded_roots")
        {
            if !paths_allowed {
                return Err(error(
                    "published workspace paths require a device-qualified patch",
                ));
            }
            let paths: Vec<PathBuf> =
                serde_json::from_value(value.clone()).map_err(|_| error("invalid path list"))?;
            validate_path_lists(&[&paths])?;
        }
        if field == "active.profiles" {
            validate_profiles(
                &serde_json::from_value::<Vec<String>>(value.clone())
                    .map_err(|_| error("invalid profiles"))?,
            )?;
        }
        if field == "guidance.tag_guide" {
            let selector: GuidanceSelector = serde_json::from_value(value.clone())
                .map_err(|_| error("invalid guidance selector"))?;
            validate_id(&selector.id)?;
        }
        if field == "cache" {
            let cache: Cache =
                serde_json::from_value(value.clone()).map_err(|_| error("invalid cache"))?;
            validate_name(&cache.library)?;
        }
        if field == "retrieval" {
            let filter: RetrievalFilter =
                serde_json::from_value(value.clone()).map_err(|_| error("invalid retrieval"))?;
            if filter.tags.is_empty() || filter.tags.len() > 32 {
                return Err(error("retrieval tags must contain 1..32 bounded tags"));
            }
            let mut seen = BTreeSet::new();
            for tag in filter.tags {
                bounded_text(&tag, 128)?;
                if !seen.insert(tag) {
                    return Err(error("duplicate retrieval tag"));
                }
            }
        }
        if field == "capture.required_tags" {
            let tags: Vec<String> =
                serde_json::from_value(value.clone()).map_err(|_| error("invalid capture tags"))?;
            if tags.len() > 32 {
                return Err(error("capture required_tags exceeds bound"));
            }
            let mut seen = BTreeSet::new();
            for tag in tags {
                bounded_text(&tag, 128)?;
                if matches!(
                    tag.as_str(),
                    "core" | "routing-judgment" | "possibility" | "pursuing" | "closed"
                ) || !mneme_core::is_normalized_tag(&tag)
                    || !seen.insert(tag)
                {
                    return Err(error(
                        "capture required_tags must be unique non-reserved ordinary tags",
                    ));
                }
            }
        }
        if matches!(field.as_str(), "capture.kinds" | "capture.evidence_kinds") {
            let values = value
                .as_array()
                .ok_or_else(|| error("invalid capture kinds"))?;
            if values.is_empty()
                || values
                    .iter()
                    .map(Value::to_string)
                    .collect::<BTreeSet<_>>()
                    .len()
                    != values.len()
            {
                return Err(error("capture kinds/evidence must be nonempty and unique"));
            }
        }
        if field == "capture.max_summary_bytes" && value.as_u64().is_none_or(|v| v == 0 || v > 512)
            || field == "capture.max_body_bytes"
                && value.as_u64().is_none_or(|v| v == 0 || v > 2048)
        {
            return Err(error("capture bounds exceed host envelope"));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublishedCatalog {
    pub schema: String,
    pub publisher_db_id: String,
    pub catalog_sha256: String,
    #[serde(default)]
    pub stores: BTreeMap<String, StoreDeclaration>,
}
impl PublishedCatalog {
    pub fn parse_toml(text: &str, publisher_db_id: &str) -> Result<Self> {
        if text.len() > MAX_CATALOG_BYTES {
            return Err(error("discovery catalog exceeds byte bound"));
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Document {
            schema: String,
            #[serde(default)]
            stores: BTreeMap<String, StoreDeclaration>,
        }
        let document: Document = toml::from_str(text).map_err(|_| {
            error("invalid discovery catalog TOML: check closed fields, types and duplicate keys")
        })?;
        let catalog = Self {
            schema: document.schema,
            publisher_db_id: publisher_db_id.into(),
            catalog_sha256: catalog_hash(&document.stores)?,
            stores: document.stores,
        };
        catalog.validate()?;
        Ok(catalog)
    }
    pub fn parse_json(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_CATALOG_BYTES {
            return Err(error("discovery catalog exceeds byte bound"));
        }
        let value = crate::legacy_workspace::unique_json(bytes)
            .map_err(|_| error("invalid discovery catalog JSON"))?;
        let catalog: Self =
            serde_json::from_value(value).map_err(|_| error("invalid discovery catalog JSON"))?;
        catalog.validate()?;
        Ok(catalog)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != DISCOVERY_CATALOG_SCHEMA {
            return Err(error("unsupported discovery catalog schema"));
        }
        validate_id(&self.publisher_db_id)?;
        if self.stores.len() > MAX_STORES {
            return Err(error("catalog exceeds store count bound"));
        }
        if serde_json::to_vec(self)
            .map_err(|_| error("cannot serialize discovery catalog"))?
            .len()
            > MAX_CATALOG_BYTES
        {
            return Err(error("discovery catalog exceeds byte bound"));
        }
        if self.catalog_sha256 != catalog_hash(&self.stores)? {
            return Err(error("discovery catalog content revision mismatch"));
        }
        let mut ids = BTreeSet::new();
        for (label, store) in &self.stores {
            validate_name(label)?;
            if !ids.insert(&store.id) {
                return Err(error("catalog has duplicate canonical database identity"));
            }
            store.validate(true)?;
        }
        Ok(())
    }
}
fn catalog_hash(stores: &BTreeMap<String, StoreDeclaration>) -> Result<String> {
    Ok(sha256(&serde_json::to_vec(stores).map_err(|_| {
        error("cannot serialize discovery catalog")
    })?))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryGraph {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<String>,
    pub seeds: Vec<String>,
    pub catalogs: BTreeMap<String, PublishedCatalog>,
    pub edges: BTreeMap<String, Vec<String>>,
    pub unavailable: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<String>,
    pub stores: BTreeMap<String, StoreDiscoveryReport>,
    pub conflicts: Vec<DiscoveryConflict>,
    pub unavailable: BTreeMap<String, String>,
    pub publisher_revisions: BTreeMap<String, String>,
    pub omitted_details: usize,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoreDiscoveryReport {
    pub label: String,
    pub blocked: bool,
    pub fields: BTreeMap<String, FieldDiscoveryReport>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FieldDiscoveryReport {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_value"
    )]
    pub effective: Option<Value>,
    pub local_override: bool,
    pub contributors: Vec<DiscoveryContributor>,
    pub shadowed: Vec<DiscoveryContributor>,
    pub conflict: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryContributor {
    pub publisher_db_id: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_value"
    )]
    pub value: Option<Value>,
    pub witness: Vec<String>,
}
// Serde's ordinary Option<Value> maps both an absent field and explicit null
// to None. Report omission must remain distinct from a known nullable clear.
fn deserialize_present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConflict {
    pub store_id: String,
    pub field: String,
    pub contributors: Vec<String>,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryResolution {
    pub config: Config,
    /// Lossless catalog-only routes, independent of activation and bounded report trimming.
    pub routes: BTreeMap<String, Owner>,
    /// Blocked project bindings still establish configured project presence.
    /// These qualified scope witnesses confer no owner activation or route.
    pub project_roots: Vec<PathBuf>,
    pub report: DiscoveryReport,
}
/// Explicit local discovery policy wins by canonical identity. A mentioned
/// target can inherit a local opt-in even when its publisher omits or denies it.
pub fn discovery_opt_in(local: &ConfigDeclarations, declaration: &StoreDeclaration) -> bool {
    local
        .stores
        .values()
        .find(|s| s.id == declaration.id)
        .and_then(|s| s.selected_discovery(local.device.as_deref()))
        .or_else(|| declaration.selected_discovery(local.device.as_deref()))
        == Some(true)
}

impl DiscoveryGraph {
    /// Validate supplied evidence, never acquire it. Every recorded edge must be
    /// justified by an explicit publisher opt-in; labels do not identify nodes.
    pub fn validate(&self) -> Result<()> {
        self.validate_core(None)
    }
    pub fn validate_for(&self, local: &ConfigDeclarations) -> Result<()> {
        self.validate_core(Some(local))
    }
    fn validate_core(&self, local: Option<&ConfigDeclarations>) -> Result<()> {
        if let Some(reason) = &self.incomplete {
            bounded_text(reason, 1024)?;
        }
        if self.catalogs.len() + self.unavailable.len() > MAX_DISCOVERY_PUBLISHERS
            || self.seeds.len() > MAX_DISCOVERY_PUBLISHERS
            || self.edges.values().map(Vec::len).sum::<usize>() > MAX_DISCOVERY_EDGES
        {
            return Err(error("discovery graph exceeds publisher/edge bound"));
        }
        let mut seed_set = BTreeSet::new();
        for seed in &self.seeds {
            validate_id(seed)?;
            if !seed_set.insert(seed)
                || local.is_some_and(|local| {
                    !local.stores.values().any(|s| {
                        &s.id == seed
                            && s.selected_discovery(local.device.as_deref()) == Some(true)
                            && s.owner
                                .as_ref()
                                .is_some_and(|owner| owner.materialize().is_ok())
                    })
                })
            {
                return Err(error("discovery graph seed lacks a unique local opt-in"));
            }
        }
        let mut bytes = 0usize;
        let mut declarations = 0usize;
        for (id, catalog) in &self.catalogs {
            if id != &catalog.publisher_db_id {
                return Err(error("discovery catalog publisher identity mismatch"));
            }
            catalog.validate()?;
            bytes = bytes.saturating_add(
                serde_json::to_vec(catalog)
                    .map_err(|_| error("cannot encode catalog"))?
                    .len(),
            );
            declarations = declarations.saturating_add(catalog.stores.len());
        }
        if bytes > MAX_DISCOVERY_BYTES || declarations > MAX_DECLARATIONS {
            return Err(error("discovery graph exceeds aggregate catalog bound"));
        }
        for (source, targets) in &self.edges {
            let catalog = self
                .catalogs
                .get(source)
                .ok_or_else(|| error("discovery edge has no source catalog"))?;
            let mut seen = BTreeSet::new();
            for target in targets {
                validate_id(target)?;
                if !seen.insert(target) {
                    return Err(error("duplicate discovery edge"));
                }
                let opted_in = catalog.stores.values().any(|s| {
                    &s.id == target && local.is_none_or(|local| discovery_opt_in(local, s))
                });
                let disabled = local.is_some_and(|local| {
                    local.stores.values().any(|s| {
                        &s.id == target
                            && (s.selected_discovery(local.device.as_deref()) == Some(false)
                                || s.fields(local.device.as_deref()).is_ok_and(|fields| {
                                    fields.get("active.enabled") == Some(&Value::Bool(false))
                                }))
                    })
                });
                if !opted_in || disabled {
                    return Err(error("discovery edge lacks an admitted explicit opt-in"));
                }
            }
        }
        for (id, reason) in &self.unavailable {
            validate_id(id)?;
            bounded_text(reason, 1024)?;
            if self.catalogs.contains_key(id) {
                return Err(error("publisher cannot be both fetched and unavailable"));
            }
        }
        let reachable = self.reachable();
        if reachable.len() > MAX_DISCOVERY_PUBLISHERS {
            return Err(error("discovery graph exceeds reachable publisher bound"));
        }
        if self
            .catalogs
            .keys()
            .chain(self.unavailable.keys())
            .any(|id| !reachable.contains(id))
        {
            return Err(error("discovery graph contains unreachable evidence"));
        }
        Ok(())
    }
    fn reachable(&self) -> BTreeSet<String> {
        let mut visited: BTreeSet<_> = self.seeds.iter().cloned().collect();
        let mut queue: VecDeque<_> = visited.iter().cloned().collect();
        while let Some(id) = queue.pop_front() {
            if let Some(edges) = self.edges.get(&id) {
                for next in edges {
                    if visited.insert(next.clone()) {
                        queue.push_back(next.clone());
                    }
                }
            }
        }
        visited
    }
}

/// First explicit declarations absorb only their own branches. The two-state
/// walk also retains one bounded witness for shadowing, including a contributor
/// reached independently by an unblocked bypass. Each (publisher, state) is
/// visited once, so cycles do not enumerate paths or give global depth priority.
fn contributors(
    graph: &DiscoveryGraph,
    declarations: &BTreeMap<String, BTreeMap<String, BTreeMap<String, Value>>>,
    target: &str,
    field: &str,
) -> (Vec<DiscoveryContributor>, Vec<DiscoveryContributor>) {
    let mut queue = VecDeque::new();
    let mut visited = BTreeSet::new();
    let mut seeds = graph.seeds.clone();
    seeds.sort();
    for seed in seeds {
        if visited.insert((seed.clone(), false)) {
            queue.push_back((seed.clone(), false, vec![seed]));
        }
    }
    let mut selected = BTreeMap::new();
    let mut shadowed = BTreeMap::new();
    while let Some((publisher, blocked, witness)) = queue.pop_front() {
        let declaration = declarations
            .get(&publisher)
            .and_then(|stores| stores.get(target))
            .and_then(|fields| fields.get(field));
        if let Some(value) = declaration {
            let contributor = DiscoveryContributor {
                publisher_db_id: publisher.clone(),
                value: Some(value.clone()),
                witness: witness.clone(),
            };
            if blocked {
                shadowed.insert(publisher.clone(), contributor);
            } else {
                selected.insert(publisher.clone(), contributor);
            }
        }
        let next_blocked = blocked || declaration.is_some();
        if let Some(edges) = graph.edges.get(&publisher) {
            let mut edges = edges.clone();
            edges.sort();
            for next in edges {
                if !visited.insert((next.clone(), next_blocked)) {
                    continue;
                }
                let mut witness = witness.clone();
                witness.push(next.clone());
                queue.push_back((next, next_blocked, witness));
            }
        }
    }
    (
        selected.into_values().collect(),
        shadowed.into_values().collect(),
    )
}

pub fn resolve_discovery(
    local: &ConfigDeclarations,
    graph: &DiscoveryGraph,
) -> Result<DiscoveryResolution> {
    local.validate()?;
    graph.validate_for(local)?;
    let reachable = graph.reachable();
    let mut report = DiscoveryReport {
        incomplete: graph.incomplete.clone(),
        unavailable: graph.unavailable.clone(),
        publisher_revisions: graph
            .catalogs
            .iter()
            .map(|(id, c)| (id.clone(), c.catalog_sha256.clone()))
            .collect(),
        ..DiscoveryReport::default()
    };
    for id in &reachable {
        if !graph.catalogs.contains_key(id) && !report.unavailable.contains_key(id) {
            report
                .unavailable
                .insert(id.clone(), "publisher snapshot unavailable".into());
        }
    }
    let mut declarations: BTreeMap<String, BTreeMap<String, BTreeMap<String, Value>>> =
        BTreeMap::new();
    let mut labels: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let local_by_id: BTreeMap<_, _> = local
        .stores
        .iter()
        .map(|(label, s)| (s.id.clone(), (label, s)))
        .collect();
    for (publisher, catalog) in &graph.catalogs {
        let mut stores = BTreeMap::new();
        for (label, store) in &catalog.stores {
            labels
                .entry(store.id.clone())
                .or_default()
                .insert(label.clone());
            stores.insert(store.id.clone(), store.fields(local.device.as_deref())?);
        }
        declarations.insert(publisher.clone(), stores);
    }
    for (label, store) in &local.stores {
        labels
            .entry(store.id.clone())
            .or_default()
            .insert(label.clone());
    }
    if labels.len() > MAX_STORES {
        return Err(error(
            "resolved discovery exceeds canonical store count bound",
        ));
    }
    let mut config = Config {
        schema: CONFIG_SCHEMA.into(),
        read_default: local.read_default.clone().unwrap_or_else(default_rule),
        background: local.materialize_background(),
        workspace: local.workspace.clone(),
        stores: BTreeMap::new(),
        libraries: local.libraries.clone(),
    };
    let mut routes = BTreeMap::new();
    let mut project_roots = BTreeSet::new();
    let mut used_labels: BTreeSet<String> = local.stores.keys().cloned().collect();
    let mut label_ids: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (id, names) in &labels {
        for name in names {
            label_ids
                .entry(name.clone())
                .or_default()
                .insert(id.clone());
        }
    }
    let original_labels: BTreeSet<_> = label_ids.keys().cloned().collect();
    for (id, names) in labels {
        let local_entry = local_by_id.get(&id);
        let local_store = local_entry.map(|(_, store)| *store);
        let local_fields = local_store
            .map(|s| s.fields(local.device.as_deref()))
            .transpose()?
            .unwrap_or_default();
        let preferred = names.iter().next().cloned().unwrap_or_else(|| id.clone());
        let label_collision =
            local_entry.is_none() && label_ids.get(&preferred).is_some_and(|ids| ids.len() > 1);
        let label = if let Some((label, _)) = local_entry {
            (*label).clone()
        } else {
            if !label_collision && used_labels.insert(preferred.clone()) {
                preferred
            } else {
                let base = format!("discovered-{id}");
                (0..=MAX_DECLARATIONS + MAX_STORES)
                    .map(|suffix| {
                        if suffix == 0 {
                            base.clone()
                        } else {
                            format!("{base}-{suffix}")
                        }
                    })
                    .find(|candidate| {
                        !original_labels.contains(candidate)
                            && used_labels.insert(candidate.clone())
                    })
                    .ok_or_else(|| error("cannot allocate unique discovered store label"))?
            }
        };
        let mut store_report = StoreDiscoveryReport {
            label: label.clone(),
            ..StoreDiscoveryReport::default()
        };
        if label_collision {
            store_report.fields.insert(
                "label".into(),
                FieldDiscoveryReport {
                    effective: Some(Value::String(label.clone())),
                    conflict: true,
                    ..FieldDiscoveryReport::default()
                },
            );
            report.conflicts.push(DiscoveryConflict{store_id:id.clone(),field:"label".into(),contributors:vec![],reason:"imported label refers to multiple canonical IDs; assigned a canonical generated label instead".into()});
        }
        let mut field_names: BTreeSet<String> = local_fields.keys().cloned().collect();
        for stores in declarations.values() {
            if let Some(fields) = stores.get(&id) {
                field_names.extend(fields.keys().cloned());
            }
        }
        field_names.insert("owner".into());
        let mut resolved = BTreeMap::new();
        for field in field_names {
            let (imported, shadowed) = contributors(graph, &declarations, &id, &field);
            let mut field_report = FieldDiscoveryReport {
                contributors: imported.clone(),
                shadowed,
                ..FieldDiscoveryReport::default()
            };
            let local_value = local_fields.get(&field);
            let value = if field == "owner" {
                let patch = local_store.and_then(|s| s.owner.as_ref());
                field_report.local_override = patch.is_some();
                resolve_route(patch, &imported)
            } else if let Some(value) = local_value {
                field_report.local_override = true;
                Ok(Some(value.clone()))
            } else {
                merge_values(&imported)
            };
            match value {
                Ok(Some(value)) => {
                    if !field_report.local_override
                        && value.is_boolean()
                        && imported.iter().any(|c| c.value.as_ref() != Some(&value))
                    {
                        field_report.conflict = true;
                        report.conflicts.push(DiscoveryConflict {store_id:id.clone(),field:field.clone(),contributors:imported.iter().map(|c|c.publisher_db_id.clone()).collect(),reason:"publisher boolean declarations disagree; conservatively resolved to false".into()});
                    }
                    field_report.effective = Some(value.clone());
                    resolved.insert(field.clone(), value);
                }
                Ok(None) => {}
                Err(reason) => {
                    field_report.conflict = true;
                    store_report.blocked = true;
                    report.conflicts.push(DiscoveryConflict {
                        store_id: id.clone(),
                        field: field.clone(),
                        contributors: imported.iter().map(|c| c.publisher_db_id.clone()).collect(),
                        reason,
                    });
                }
            }
            store_report.fields.insert(field, field_report);
        }
        if let Some(owner) = resolved.get("owner") {
            routes.insert(
                id.clone(),
                serde_json::from_value(owner.clone())
                    .map_err(|_| error("resolved owner route has invalid shape"))?,
            );
        }
        let imported_target = declarations.values().any(|stores| stores.contains_key(&id));
        if (report.incomplete.is_some() || !report.unavailable.is_empty())
            && (imported_target
                || local_store
                    .is_some_and(|s| s.selected_discovery(local.device.as_deref()) == Some(true)))
        {
            block(
                &mut store_report,
                &mut report,
                &id,
                "discovery.coverage",
                "incomplete discovery cannot activate discovery-dependent configuration",
            );
        }
        let mut device_scopes = BTreeSet::new();
        for declaration in local_store.into_iter().chain(
            graph
                .catalogs
                .values()
                .flat_map(|c| c.stores.values())
                .filter(|s| s.id == id),
        ) {
            for patch in declaration.devices.values() {
                for field in declaration_fields(patch)?
                    .keys()
                    .filter(|field| is_path_field(field))
                {
                    if let Some((scope, _)) = field.rsplit_once('.') {
                        device_scopes.insert(scope.to_owned());
                    }
                }
            }
        }
        for scope in device_scopes {
            let qualified = resolved.keys().any(|field| {
                is_path_field(field)
                    && field
                        .rsplit_once('.')
                        .is_some_and(|(prefix, _)| prefix == scope)
            }) || (scope == "active"
                && local_fields.contains_key("active.workspace"));
            if !qualified {
                block(
                    &mut store_report,
                    &mut report,
                    &id,
                    &format!("{scope}.device"),
                    "device-qualified scope has no selected device path policy or explicit local replacement",
                );
            }
        }
        if !store_report.blocked {
            let owner = resolved
                .get("owner")
                .cloned()
                .ok_or_else(|| error("resolved owner missing after route validation"))?;
            let mut value = serde_json::to_value(Store {
                id: id.clone(),
                owner: serde_json::from_value(owner)
                    .map_err(|_| error("resolved owner is invalid"))?,
                active: Activation::default(),
                permissions: Permissions::default(),
                delivery: Delivery::default(),
                background: StoreBackground::default(),
                guidance: Guidance::default(),
                cache: None,
                retrieval: None,
                applicability: Applicability::default(),
                capture: CapturePolicy::default(),
            })
            .map_err(|_| error("cannot materialize store defaults"))?;
            for (field, setting) in &resolved {
                if field != "owner" && field != "discovery" {
                    set_field(&mut value, field, setting.clone())?;
                }
            }
            let store: Store = serde_json::from_value(value)
                .map_err(|_| error("resolved store has invalid policy shape"))?;
            let check = Config {
                schema: CONFIG_SCHEMA.into(),
                read_default: default_rule(),
                background: Background::default(),
                workspace: Workspace::default(),
                stores: BTreeMap::from([(label.clone(), store.clone())]),
                libraries: local.libraries.clone(),
            };
            if check.validate().is_err() {
                block(
                    &mut store_report,
                    &mut report,
                    &id,
                    "configuration",
                    "resolved policy fails configuration invariants; provide a coherent local override",
                );
            } else {
                for (field, effective) in declaration_fields(&store)? {
                    store_report.fields.entry(field).or_default().effective = Some(effective);
                }
                if config.stores.insert(label, store).is_some() {
                    return Err(error(
                        "resolved store label would replace another canonical binding",
                    ));
                }
            }
        }
        if store_report.blocked
            && !resolved
                .get("active.workspace")
                .is_some_and(|v| v == "any" || v == "no-project")
        {
            for field in ["active.roots", "active.subtree_roots"] {
                let candidates: Vec<&Value> = if let Some(value) = resolved.get(field) {
                    vec![value]
                } else {
                    store_report
                        .fields
                        .get(field)
                        .map(|r| {
                            r.contributors
                                .iter()
                                .filter_map(|c| c.value.as_ref())
                                .collect()
                        })
                        .unwrap_or_default()
                };
                for value in candidates {
                    if let Ok(paths) = serde_json::from_value::<Vec<PathBuf>>(value.clone()) {
                        project_roots.extend(paths);
                    }
                }
            }
        }
        report.stores.insert(id, store_report);
    }
    report.bound()?;
    Ok(DiscoveryResolution {
        config,
        routes,
        project_roots: project_roots.into_iter().collect(),
        report,
    })
}
fn is_path_field(field: &str) -> bool {
    field.ends_with(".roots")
        || field.ends_with(".subtree_roots")
        || field.ends_with(".excluded_roots")
}
fn block(
    store: &mut StoreDiscoveryReport,
    report: &mut DiscoveryReport,
    id: &str,
    field: &str,
    reason: &str,
) {
    store.blocked = true;
    store.fields.entry(field.into()).or_default().conflict = true;
    report.conflicts.push(DiscoveryConflict {
        store_id: id.into(),
        field: field.into(),
        contributors: vec![],
        reason: reason.into(),
    });
}
fn merge_values(
    contributors: &[DiscoveryContributor],
) -> std::result::Result<Option<Value>, String> {
    let values: Vec<_> = contributors
        .iter()
        .filter_map(|c| c.value.as_ref())
        .collect();
    let Some(first) = values.first() else {
        return Ok(None);
    };
    if values.iter().all(|v| *v == *first) {
        return Ok(Some((*first).clone()));
    }
    if values.iter().all(|v| v.is_boolean()) {
        return Ok(Some(Value::Bool(
            values.iter().all(|v| v.as_bool() == Some(true)),
        )));
    }
    Err("independent publisher declarations disagree; provide an explicit local override".into())
}
fn resolve_route(
    local: Option<&OwnerDeclaration>,
    contributors: &[DiscoveryContributor],
) -> std::result::Result<Option<Value>, String> {
    let failure = || {
        "owner route is missing, incoherent or ambiguous; provide a complete local owner route"
            .to_owned()
    };
    if let Some(owner) = local.and_then(|o| o.materialize().ok()) {
        return serde_json::to_value(owner).map(Some).map_err(|_| failure());
    }
    let mut candidates = Vec::new();
    for contributor in contributors {
        let declaration: OwnerDeclaration =
            serde_json::from_value(contributor.value.clone().ok_or_else(failure)?)
                .map_err(|_| failure())?;
        let owner = declaration.materialize().map_err(|_| failure())?;
        let owner = if let Some(patch) = local {
            patch.overlay(&owner).map_err(|_| failure())?
        } else {
            owner
        };
        candidates.push(serde_json::to_value(owner).map_err(|_| failure())?);
    }
    let Some(first) = candidates.first() else {
        return Err(failure());
    };
    if candidates.iter().all(|v| v == first) {
        Ok(Some(first.clone()))
    } else {
        Err(failure())
    }
}
fn set_field(root: &mut Value, path: &str, value: Value) -> Result<()> {
    let mut pieces = path.split('.').peekable();
    let mut cursor = root;
    while let Some(piece) = pieces.next() {
        let object = cursor
            .as_object_mut()
            .ok_or_else(|| error("invalid policy field path"))?;
        if pieces.peek().is_none() {
            object.insert(piece.into(), value);
            return Ok(());
        }
        cursor = object
            .entry(piece)
            .or_insert_with(|| Value::Object(Default::default()));
    }
    Err(error("empty policy field path"))
}
impl DiscoveryReport {
    pub fn to_json_bounded(&self) -> Result<Vec<u8>> {
        let bytes =
            serde_json::to_vec(self).map_err(|_| error("cannot serialize discovery report"))?;
        if bytes.len() > MAX_DISCOVERY_REPORT_BYTES {
            return Err(error("discovery report exceeds byte bound"));
        }
        Ok(bytes)
    }
    fn bound(&mut self) -> Result<()> {
        if self.to_json_bounded().is_ok() {
            return Ok(());
        }
        for store in self.stores.values_mut() {
            for field in store.fields.values_mut() {
                for contributor in field
                    .contributors
                    .iter_mut()
                    .chain(field.shadowed.iter_mut())
                {
                    self.omitted_details +=
                        contributor.witness.len() + usize::from(contributor.value.is_some());
                    contributor.witness.clear();
                    contributor.value = None;
                }
            }
        }
        if self.to_json_bounded().is_ok() {
            return Ok(());
        }
        for store in self.stores.values_mut() {
            for field in store.fields.values_mut() {
                self.omitted_details += field.shadowed.len();
                field.shadowed.clear();
            }
        }
        if self.to_json_bounded().is_ok() {
            return Ok(());
        }
        for store in self.stores.values_mut() {
            for field in store.fields.values_mut() {
                self.omitted_details += field.contributors.len();
                field.contributors.clear();
            }
        }
        for conflict in &mut self.conflicts {
            self.omitted_details += conflict.contributors.len();
            conflict.contributors.clear();
        }
        if self.to_json_bounded().is_ok() {
            return Ok(());
        }
        for store in self.stores.values_mut() {
            for field in store.fields.values_mut() {
                self.omitted_details += usize::from(field.effective.is_some());
                field.effective = None;
            }
        }
        if self.to_json_bounded().is_ok() {
            return Ok(());
        }
        for conflict in &mut self.conflicts {
            self.omitted_details += 1;
            conflict.reason = "conflict details omitted by report byte bound".into();
        }
        self.to_json_bounded().map(|_| ())
    }
}
