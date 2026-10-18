//! Bounded declarative metadata. This crate never contacts, opens or starts an owner.
//! Activation, authority, delivery and authored guidance selectors are independent.

mod declarative_file;
mod legacy_workspace;
mod selection;
pub use selection::*;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const CONFIG_SCHEMA: &str = "mneme.config.v1";
pub const SELECTION_SCHEMA: &str = "mneme.config-selection.v1";
pub const MAX_CONFIG_BYTES: usize = 65536;
pub const MAX_STORES: usize = 64;
pub const MAX_LIBRARIES: usize = 32;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);
pub type Result<T> = std::result::Result<T, ConfigError>;
pub(crate) fn error(message: &str) -> ConfigError {
    ConfigError(message.into())
}
pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn profiles() -> Vec<String> {
    vec!["default".into(), "private".into()]
}
fn yes() -> bool {
    true
}
fn default_rule() -> String {
    "project-or-misc".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: String,
    #[serde(default = "default_rule")]
    pub read_default: String,
    #[serde(default)]
    pub background: Background,
    #[serde(default)]
    pub workspace: Workspace,
    #[serde(default)]
    pub stores: BTreeMap<String, Store>,
    #[serde(default)]
    pub libraries: BTreeMap<String, Library>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Background {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_model")]
    pub model: Option<String>,
    #[serde(default = "default_effort")]
    pub effort: Option<String>,
}
fn default_model() -> Option<String> {
    Some("gpt-6.1-sol".into())
}
fn default_effort() -> Option<String> {
    Some("medium".into())
}
impl Default for Background {
    fn default() -> Self {
        Self {
            enabled: true,
            model: default_model(),
            effort: default_effort(),
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub profile: Option<String>,
    pub allow_stores: Option<Vec<String>>,
    pub allow_libraries: Option<Vec<String>>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Store {
    /// Canonical native database identity, never an alias.
    pub id: String,
    pub owner: Owner,
    #[serde(default)]
    pub active: Activation,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub delivery: Delivery,
    #[serde(default)]
    pub background: StoreBackground,
    #[serde(default)]
    pub guidance: Guidance,
    pub cache: Option<Cache>,
    pub retrieval: Option<RetrievalFilter>,
    #[serde(default)]
    pub applicability: Applicability,
    #[serde(default)]
    pub capture: CapturePolicy,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub url: String,
    pub db: String,
    pub ssh_mcp_port: Option<u16>,
    pub token_env: Option<String>,
    /// Owner-side identity evidence, not a local database open target.
    pub path: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub workspace: WorkspaceKind,
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    #[serde(default = "profiles")]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub excluded_roots: Vec<PathBuf>,
}
impl Default for Activation {
    fn default() -> Self {
        Self {
            enabled: true,
            workspace: WorkspaceKind::Project,
            roots: vec![],
            profiles: profiles(),
            excluded_roots: vec![],
        }
    }
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceKind {
    Any,
    #[default]
    Project,
    NoProject,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    #[serde(default = "yes")]
    pub read: bool,
    #[serde(default = "yes")]
    pub write: bool,
}
impl Default for Permissions {
    fn default() -> Self {
        Self {
            read: true,
            write: true,
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    #[serde(default = "yes")]
    pub core: bool,
    #[serde(default = "yes")]
    pub context: bool,
}
impl Default for Delivery {
    fn default() -> Self {
        Self {
            core: true,
            context: true,
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoreBackground {
    #[serde(default = "yes")]
    pub recall: bool,
    #[serde(default = "yes")]
    pub record: bool,
    #[serde(default = "yes")]
    pub maintenance: bool,
}
impl Default for StoreBackground {
    fn default() -> Self {
        Self {
            recall: true,
            record: true,
            maintenance: true,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapturePolicy {
    /// Semantic recording purpose, independent of owner names and ordinary tags.
    #[serde(default, skip_serializing_if = "CaptureIntent::is_ordinary")]
    pub intent: CaptureIntent,
    #[serde(default = "capture_kinds")]
    pub kinds: Vec<CaptureKind>,
    #[serde(default = "evidence_kinds")]
    pub evidence_kinds: Vec<EvidenceKind>,
    #[serde(default)]
    pub required_tags: Vec<String>,
    #[serde(default = "yes")]
    pub allow_associations: bool,
    #[serde(default = "summary_bound")]
    pub max_summary_bytes: u32,
    #[serde(default = "body_bound")]
    pub max_body_bytes: u32,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureIntent {
    #[default]
    Ordinary,
    CollaborationPreferences,
    CrossWorkspace,
}
impl CaptureIntent {
    fn is_ordinary(&self) -> bool {
        *self == Self::Ordinary
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum CaptureKind {
    Lesson,
    Episode,
    Possibility,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    UserStatement,
    AssistantAssertion,
    ToolCall,
    ToolResult,
    MemoryDelivery,
}
fn capture_kinds() -> Vec<CaptureKind> {
    vec![
        CaptureKind::Lesson,
        CaptureKind::Episode,
        CaptureKind::Possibility,
    ]
}
fn evidence_kinds() -> Vec<EvidenceKind> {
    vec![
        EvidenceKind::UserStatement,
        EvidenceKind::AssistantAssertion,
        EvidenceKind::ToolCall,
        EvidenceKind::ToolResult,
        EvidenceKind::MemoryDelivery,
    ]
}
fn summary_bound() -> u32 {
    512
}
fn body_bound() -> u32 {
    2048
}
impl Default for CapturePolicy {
    fn default() -> Self {
        Self {
            intent: CaptureIntent::Ordinary,
            kinds: capture_kinds(),
            evidence_kinds: evidence_kinds(),
            required_tags: vec![],
            allow_associations: true,
            max_summary_bytes: 512,
            max_body_bytes: 2048,
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Guidance {
    pub tag_guide: Option<GuidanceSelector>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GuidanceSelector {
    pub id: String,
    pub content: GuidanceContent,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GuidanceContent {
    Summary,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Cache {
    pub library: String,
    #[serde(default)]
    pub mode: CacheMode,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CacheMode {
    #[default]
    OwnerFirst,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetrievalFilter {
    pub tags: Vec<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Applicability {
    #[serde(default)]
    pub core: OperationScope,
    #[serde(default)]
    pub context: OperationScope,
    #[serde(default)]
    pub recall: OperationScope,
    #[serde(default)]
    pub record: OperationScope,
    #[serde(default)]
    pub maintenance: OperationScope,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationScope {
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    #[serde(default)]
    pub excluded_roots: Vec<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Library {
    pub catalog: PathBuf,
    #[serde(default)]
    pub replicas: BTreeMap<String, Replica>,
    #[serde(default = "profiles")]
    pub profiles: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Replica {
    pub url: String,
    pub ssh_mcp_port: Option<u16>,
    pub token_env: Option<String>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() > MAX_CONFIG_BYTES {
            return Err(error("configuration exceeds 65536 bytes"));
        }
        // Never return TOML diagnostics: they may echo credentials or authored text.
        let config: Self = toml::from_str(text).map_err(|_| {
            error("invalid Mneme TOML: check closed fields, types and duplicate keys")
        })?;
        config.validate()?;
        Ok(config)
    }
    pub fn parse_overlay(text: &str) -> Result<Self> {
        if text.len() > MAX_CONFIG_BYTES {
            return Err(error("configuration exceeds 65536 bytes"));
        }
        let config: Self = toml::from_str(text).map_err(|_| {
            error("invalid Mneme project TOML: check closed fields, types and duplicate keys")
        })?;
        config.validate_inner(false)?;
        Ok(config)
    }
    pub fn load(path: &Path) -> Result<Self> {
        let source = declarative_file::read(path)?
            .ok_or_else(|| error("configuration file does not exist"))?;
        let text =
            std::str::from_utf8(&source.bytes).map_err(|_| error("configuration must be UTF-8"))?;
        Self::parse(text)
    }
    /// Overlay addition is explicit: duplicate names/identities cannot replace a user owner.
    /// Model/process settings remain user-owned; background may only narrow; the same schema is used, not another policy language.
    pub fn merge_project(mut self, mut overlay: Self, root: &Path) -> Result<Self> {
        validate_path(root)?;
        if (overlay.background.model != default_model()
            && overlay.background.model != self.background.model)
            || (overlay.background.effort != default_effort()
                && overlay.background.effort != self.background.effort)
            || overlay.read_default != default_rule()
        {
            return Err(error(
                "project overlay cannot replace user model/effort or read_default",
            ));
        }
        self.background.enabled &= overlay.background.enabled;
        for store in overlay.stores.values_mut() {
            if store.active.workspace == WorkspaceKind::Project && store.active.roots.is_empty() {
                store.active.roots.push(root.to_owned());
            }
        }
        for (name, store) in overlay.stores {
            if self.stores.insert(name, store).is_some() {
                return Err(error("project overlay duplicates a user store name"));
            }
        }
        for (name, library) in overlay.libraries {
            if self.libraries.insert(name, library).is_some() {
                return Err(error("project overlay duplicates a user library name"));
            }
        }
        if overlay.workspace.profile.as_ref().is_some_and(|p| {
            self.workspace.profile.as_ref().is_none_or(|old| {
                crate::selection::profile_rank(p) > crate::selection::profile_rank(old)
            })
        }) {
            self.workspace.profile = overlay.workspace.profile;
        }
        fn narrow(old: Option<Vec<String>>, new: Option<Vec<String>>) -> Option<Vec<String>> {
            match (old, new) {
                (Some(old), Some(new)) => {
                    Some(old.into_iter().filter(|n| new.contains(n)).collect())
                }
                (old, None) => old,
                (None, new) => new,
            }
        }
        self.workspace.allow_stores =
            narrow(self.workspace.allow_stores, overlay.workspace.allow_stores);
        self.workspace.allow_libraries = narrow(
            self.workspace.allow_libraries,
            overlay.workspace.allow_libraries,
        );
        self.validate()?;
        Ok(self)
    }
    pub fn validate(&self) -> Result<()> {
        self.validate_inner(true)
    }
    fn validate_inner(&self, complete: bool) -> Result<()> {
        if self.schema != CONFIG_SCHEMA {
            return Err(error(
                "unsupported configuration schema; expected mneme.config.v1",
            ));
        }
        if self.stores.len() > MAX_STORES || self.libraries.len() > MAX_LIBRARIES {
            return Err(error("configuration exceeds store/library count bound"));
        }
        if self.read_default != default_rule() && !self.stores.contains_key(&self.read_default) {
            return Err(error(
                "read_default must be project-or-misc or a configured store name",
            ));
        }
        for text in [&self.background.model, &self.background.effort]
            .into_iter()
            .flatten()
        {
            bounded_text(text, 128)?;
        }
        if let Some(profile) = &self.workspace.profile {
            validate_profile(profile)?;
        }
        if let Some(names) = &self.workspace.allow_stores {
            unique_names(names)?;
        }
        if let Some(names) = &self.workspace.allow_libraries {
            unique_names(names)?;
        }
        let mut ids = BTreeSet::new();
        for (name, store) in &self.stores {
            validate_name(name)?;
            validate_id(&store.id)?;
            if !ids.insert(&store.id) {
                return Err(error(
                    "each canonical database ID must have exactly one configured store binding",
                ));
            }
            validate_endpoint(
                &store.owner.url,
                store.owner.ssh_mcp_port,
                store.owner.token_env.as_deref(),
            )?;
            validate_name(&store.owner.db)?;
            if let Some(path) = &store.owner.path {
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
            }
            validate_paths(&store.active.roots)?;
            validate_paths(&store.active.excluded_roots)?;
            validate_profiles(&store.active.profiles)?;
            for scope in [
                &store.applicability.core,
                &store.applicability.context,
                &store.applicability.recall,
                &store.applicability.record,
                &store.applicability.maintenance,
            ] {
                validate_paths(&scope.roots)?;
                validate_paths(&scope.excluded_roots)?;
            }
            if let Some(selector) = &store.guidance.tag_guide {
                validate_id(&selector.id)?;
            }
            if let Some(cache) = &store.cache {
                validate_name(&cache.library)?;
                if complete && !self.libraries.contains_key(&cache.library) {
                    return Err(error("cache references an unconfigured library"));
                }
            }
            let policy = &store.capture;
            if policy.kinds.is_empty()
                || policy.kinds.iter().collect::<BTreeSet<_>>().len() != policy.kinds.len()
                || policy.evidence_kinds.is_empty()
                || policy.evidence_kinds.iter().collect::<BTreeSet<_>>().len()
                    != policy.evidence_kinds.len()
            {
                return Err(error("capture kinds/evidence must be nonempty and unique"));
            }
            if policy.max_summary_bytes == 0
                || policy.max_summary_bytes > 512
                || policy.max_body_bytes == 0
                || policy.max_body_bytes > 2048
            {
                return Err(error(
                    "capture bounds may narrow but not exceed the host 512/2048 byte envelope",
                ));
            }
            if policy.required_tags.len() > 32 {
                return Err(error("capture required_tags exceeds bound"));
            }
            let mut required = BTreeSet::new();
            for tag in &policy.required_tags {
                bounded_text(tag, 128)?;
                if matches!(
                    tag.as_str(),
                    "core" | "routing-judgment" | "possibility" | "pursuing" | "closed"
                ) || tag.chars().any(char::is_whitespace)
                    || !required.insert(tag)
                {
                    return Err(error(
                        "capture required_tags must be unique non-reserved ordinary tags",
                    ));
                }
            }
            if policy.intent == CaptureIntent::CollaborationPreferences
                && (policy.kinds != [CaptureKind::Lesson]
                    || policy.evidence_kinds != [EvidenceKind::UserStatement]
                    || policy.allow_associations
                    || policy.max_summary_bytes > 384
                    || policy.max_body_bytes > 1024
                    || !policy
                        .required_tags
                        .iter()
                        .any(|tag| tag == "collaboration-preference"))
            {
                return Err(error(
                    "collaboration-preferences capture requires only lesson/user_statement, no associations, at most 384/1024 summary/body bytes and the collaboration-preference tag",
                ));
            }
            if let Some(filter) = &store.retrieval {
                if filter.tags.is_empty() || filter.tags.len() > 32 {
                    return Err(error(
                        "retrieval tags must contain 1..32 unique bounded tags",
                    ));
                }
                let mut tags = BTreeSet::new();
                for tag in &filter.tags {
                    bounded_text(tag, 128)?;
                    if !tags.insert(tag) {
                        return Err(error("duplicate retrieval tag"));
                    }
                }
            }
        }
        for (name, library) in &self.libraries {
            validate_name(name)?;
            validate_path(&library.catalog)?;
            validate_profiles(&library.profiles)?;
            if library.replicas.len() > 16 {
                return Err(error("library exceeds 16 replica endpoints"));
            }
            for (name, replica) in &library.replicas {
                validate_name(name)?;
                validate_endpoint(
                    &replica.url,
                    replica.ssh_mcp_port,
                    replica.token_env.as_deref(),
                )?;
            }
        }
        for name in self.workspace.allow_stores.iter().flatten() {
            if complete && !self.stores.contains_key(name) {
                return Err(error(
                    "workspace allow_stores references an unconfigured store",
                ));
            }
        }
        for name in self.workspace.allow_libraries.iter().flatten() {
            if complete && !self.libraries.contains_key(name) {
                return Err(error(
                    "workspace allow_libraries references an unconfigured library",
                ));
            }
        }
        Ok(())
    }
}
fn bounded_text(text: &str, max: usize) -> Result<()> {
    if text.is_empty() || text.len() > max || text.chars().any(char::is_control) {
        return Err(error("configuration contains invalid bounded text"));
    }
    Ok(())
}
pub(crate) fn validate_name(text: &str) -> Result<()> {
    bounded_text(text, 128)?;
    if !text
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(error(
            "names must use ASCII letters, digits, dot, underscore or hyphen",
        ));
    }
    Ok(())
}
fn validate_id(text: &str) -> Result<()> {
    if text.len() != 26
        || ulid::Ulid::from_string(text)
            .ok()
            .is_none_or(|id| id.to_string() != text)
    {
        return Err(error(
            "identity/guide ID must be a canonical uppercase ULID",
        ));
    }
    Ok(())
}
fn validate_endpoint(text: &str, port: Option<u16>, env: Option<&str>) -> Result<()> {
    bounded_text(text, 4096)?;
    let url = url::Url::parse(text).map_err(|_| error("invalid owner endpoint"))?;
    if !matches!(url.scheme(), "http" | "https" | "ssh")
        || url.host_str().is_none()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (url.scheme() != "ssh" && !url.username().is_empty())
    {
        return Err(error(
            "endpoint must be HTTP(S) or SSH without credentials, query or fragment",
        ));
    }
    if url.scheme() == "ssh" {
        if port.is_none_or(|p| p == 0) || !matches!(url.path(), "" | "/") {
            return Err(error("SSH requires ssh_mcp_port and no endpoint path"));
        }
    } else if port.is_some() {
        return Err(error("ssh_mcp_port is only valid for SSH"));
    }
    if let Some(env) = env {
        bounded_text(env, 128)?;
        if !env.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || env.as_bytes()[0].is_ascii_digit()
        {
            return Err(error(
                "token_env must be an environment variable name, not a secret",
            ));
        }
    }
    Ok(())
}
pub(crate) fn validate_path(path: &Path) -> Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| error("configuration paths must be UTF-8"))?;
    if text.len() > 4096
        || text.contains('\0')
        || !path.is_absolute()
        || path.components().any(|p| {
            matches!(
                p,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
        || path.components().collect::<PathBuf>().to_str() != Some(text)
    {
        return Err(error("paths must be bounded normalized absolute paths"));
    }
    Ok(())
}
fn validate_paths(paths: &[PathBuf]) -> Result<()> {
    if paths.len() > 64 || paths.iter().map(|p| p.as_os_str().len()).sum::<usize>() > 16384 {
        return Err(error("path list exceeds bound"));
    }
    let mut seen = BTreeSet::new();
    for path in paths {
        validate_path(path)?;
        if !seen.insert(path) {
            return Err(error("duplicate path"));
        }
    }
    Ok(())
}
pub(crate) fn validate_profile(profile: &str) -> Result<()> {
    if !matches!(profile, "default" | "private" | "isolated") {
        return Err(error("profile must be default, private or isolated"));
    }
    Ok(())
}
fn validate_profiles(values: &[String]) -> Result<()> {
    unique_names(values)?;
    for value in values {
        validate_profile(value)?;
    }
    Ok(())
}
fn unique_names(values: &[String]) -> Result<()> {
    if values.len() > 64 {
        return Err(error("name list exceeds bound"));
    }
    let mut seen = BTreeSet::new();
    for name in values {
        validate_name(name)?;
        if !seen.insert(name) {
            return Err(error("duplicate name"));
        }
    }
    Ok(())
}
