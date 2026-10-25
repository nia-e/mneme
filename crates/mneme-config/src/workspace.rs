//! Filesystem metadata collection, discovery and exact frozen-request replay.
//! This module never opens a memory store or contacts an owner.

use crate::{
    ConfigDeclarations, ConfigSource, DiscoveryBinding, DiscoveryGraph, DiscoveryReceipt,
    DiscoveryResolution, DiscoverySeed, DiscoveryWitness, MAX_CONFIG_BYTES, PrivacyProfile, Result,
    SelectionContext, SelectionPurpose, SelectionSnapshot, WorkspaceKind, declarative_file,
    default_rule, error, legacy_workspace, resolve_discovery, select, sha256, validate_path,
    validate_profile,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SelectionRequestMode {
    Explicit,
    Discover,
}
/// Frozen operator request, including a default user path that was absent. It is
/// not enough to re-read sources: new metadata can create a new privacy boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SelectionRequest {
    pub mode: SelectionRequestMode,
    #[serde(default, skip_serializing_if = "SelectionPurpose::is_workspace")]
    pub purpose: SelectionPurpose,
    pub user_path: Option<PathBuf>,
    pub cwd: PathBuf,
    pub requested_profile: Option<String>,
    pub lexical_pwd: Option<PathBuf>,
    pub user_codex_dir: Option<PathBuf>,
}
pub fn default_user_path() -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            #[cfg(windows)]
            {
                std::env::var_os("APPDATA")
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("USERPROFILE")
                            .filter(|s| !s.is_empty())
                            .map(|s| PathBuf::from(s).join("AppData/Roaming"))
                    })
            }
            #[cfg(not(windows))]
            {
                std::env::var_os("HOME")
                    .filter(|s| !s.is_empty())
                    .map(|s| PathBuf::from(s).join(".config"))
            }
        })
        .ok_or_else(|| {
            error("set XDG_CONFIG_HOME/HOME (APPDATA/USERPROFILE on Windows) or use --config PATH")
        })?;
    validate_path(&dir)?;
    Ok(dir.join("mneme/memory.toml"))
}
fn load_source(path: &Path, role: &str) -> Result<(ConfigDeclarations, ConfigSource)> {
    let source = declarative_file::read(path)?.ok_or_else(|| {
        error("configuration file is missing; create memory.toml or choose --config")
    })?;
    let raw = source.bytes;
    let text = std::str::from_utf8(&raw).map_err(|_| error("configuration must be UTF-8"))?;
    let config = if role == "project" {
        ConfigDeclarations::parse_overlay(text)?
    } else {
        ConfigDeclarations::parse(text)?
    };
    Ok((
        config,
        ConfigSource {
            role: role.into(),
            path: source.path,
            sha256: sha256(&raw),
            bytes: raw.len(),
        },
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyProfile {
    schema: String,
    mode: String,
    library_config: Option<PathBuf>,
    stores: Option<Vec<String>>,
}

/// Single native reader for CLI, MCP and hooks. Filesystem work is metadata-only,
/// bounded across lexical and canonical ancestry, and retains legacy enrollment
/// boundaries without translating legacy policies or contacting their owners.
pub fn select_workspace(
    user_path: &Path,
    cwd: &Path,
    requested_profile: Option<&str>,
) -> Result<SelectionSnapshot> {
    let request = make_request(
        SelectionRequestMode::Explicit,
        Some(user_path),
        cwd,
        requested_profile,
    )?;
    resolve_request(&request)?.ok_or_else(|| error("explicit configuration is unavailable"))
}
fn prepare_workspace_inner(
    user_path: Option<&Path>,
    cwd: &Path,
    requested_profile: Option<&str>,
    origins: Vec<PathBuf>,
    user_codex: Option<&Path>,
) -> Result<(ConfigDeclarations, SelectionContext)> {
    let mut config = ConfigDeclarations::default();
    let mut sources = vec![];
    if let Some(path) = user_path {
        let (loaded, source) = load_source(path, "user")?;
        config = loaded;
        sources.push(source);
    }
    let canonical = cwd
        .canonicalize()
        .map_err(|_| error("cannot resolve workspace directory"))?;
    let mut context = SelectionContext::new(canonical);
    context.origins = origins.clone();
    context.sources = sources;
    context.profile = requested_profile.unwrap_or("default").to_owned();
    let mut context_profile = PrivacyProfile::parse(&context.profile)?;
    let mut seen = BTreeSet::new();
    let mut overlay_root: Option<PathBuf> = None;
    let mut aggregate_bytes = context.sources.iter().map(|s| s.bytes).sum::<usize>();
    for origin in origins {
        let mut found_profile = false;
        for (index, parent) in origin.ancestors().enumerate() {
            if index >= 64 {
                return Err(error("workspace metadata search exceeds 64 ancestors"));
            }
            if !seen.insert(parent.to_owned()) {
                // Every marker on this path was already witnessed, not only the
                // nearest one. Alias chains retain their distinct lexical paths.
                continue;
            }
            let git_present = std::fs::symlink_metadata(parent.join(".git"))
                .map(|_| true)
                .or_else(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Ok(false)
                    } else {
                        Err(e)
                    }
                })
                .map_err(|_| error("cannot inspect project Git boundary"))?;
            if git_present {
                context.project.boundaries.push(parent.to_owned());
            }
            let mneme = parent.join(".mneme");
            if let Some(metadata) = legacy_workspace::inspect(&mneme)
                .map_err(|_| error("cannot inspect project .mneme boundary"))?
            {
                if !metadata.is_dir() {
                    return Err(error("project .mneme must be a non-symlink directory"));
                }
                context.project.configured = true;
                context.project.boundaries.push(parent.to_owned());
                if context.project.root.is_none() {
                    context.project.root = Some(parent.to_owned());
                }
                let overlay = mneme.join("memory.toml");
                if declarative_file::present(&overlay)? {
                    // The nearest configured boundary owns selection. Ancestor
                    // overlays remain witnesses only through configured presence.
                    if overlay_root.is_none() && context.project.root.as_deref() == Some(parent) {
                        let (project_config, source) = load_source(&overlay, "project")?;
                        let same_user_source = context
                            .sources
                            .iter()
                            .find(|s| s.role == "user" && s.path == source.path);
                        if let Some(user_source) = same_user_source {
                            if user_source.sha256 != source.sha256 {
                                return Err(error(
                                    "configuration changed during selection; retry native selection",
                                ));
                            }
                            // Explicitly choosing the project file is not a request
                            // to overlay it onto itself. Infer only its project root.
                            infer_project_roots(&mut config, parent);
                        } else {
                            config = merge_project_declarations(config, project_config, parent)?;
                        }
                        context.project.overlay = Some(source.path.clone());
                        aggregate_bytes += source.bytes;
                        context.sources.push(source);
                        overlay_root = Some(parent.to_owned());
                    } else {
                        let (_, source) = load_source(&overlay, "project")?;
                        aggregate_bytes += source.bytes;
                        context.sources.push(source);
                    }
                    if aggregate_bytes > MAX_CONFIG_BYTES * 4 {
                        return Err(error("workspace metadata exceeds aggregate byte bound"));
                    }
                }
                let profile_path = mneme.join("profile.json");
                if let Some(raw) = legacy_workspace::read(&profile_path)
                    .map_err(|_| error("invalid project privacy profile metadata"))?
                {
                    if raw.len() > 8192 {
                        return Err(error("project privacy profile exceeds 8192 bytes"));
                    }
                    let value = legacy_workspace::unique_json(&raw)
                        .map_err(|_| error("invalid project privacy profile"))?;
                    if value.get("library_config").is_some_and(|v| !v.is_string())
                        || value.get("stores").is_some_and(|v| !v.is_array())
                    {
                        return Err(error("project profile selectors have invalid types"));
                    }
                    let profile: LegacyProfile = serde_json::from_value(value)
                        .map_err(|_| error("invalid project privacy profile"))?;
                    if !matches!(
                        profile.schema.as_str(),
                        "mneme.profile.v1" | "mneme.profile.v2"
                    ) {
                        return Err(error("unsupported project privacy profile"));
                    }
                    let privacy = PrivacyProfile::parse(&profile.mode)?;
                    if profile.schema == "mneme.profile.v1" && profile.stores.is_some() {
                        return Err(error("named stores require project profile v2"));
                    }
                    if profile.stores.is_some() && profile.library_config.is_some() {
                        return Err(error(
                            "project profile cannot select stores and legacy library together",
                        ));
                    }
                    if let Some(names) = &profile.stores {
                        crate::unique_names(names)?;
                    }
                    if let Some(path) = &profile.library_config {
                        validate_path(path)?;
                    }
                    if !found_profile {
                        if let Some(names) = profile.stores {
                            context.project.legacy_allow_stores =
                                Some(match context.project.legacy_allow_stores.take() {
                                    Some(old) => {
                                        old.into_iter().filter(|n| names.contains(n)).collect()
                                    }
                                    None => names,
                                });
                        }
                        if let Some(path) = profile.library_config {
                            if context
                                .project
                                .legacy_library
                                .as_ref()
                                .is_some_and(|old| old != &path)
                            {
                                return Err(error(
                                    "lexical and canonical legacy library profiles disagree",
                                ));
                            }
                            context.project.legacy_library = Some(path);
                        }
                        found_profile = true;
                    }
                    context_profile = context_profile.narrow(privacy);
                    context.profile = context_profile.as_str().to_owned();
                    aggregate_bytes += raw.len();
                    if aggregate_bytes > MAX_CONFIG_BYTES * 4 {
                        return Err(error("workspace metadata exceeds aggregate byte bound"));
                    }
                    context.sources.push(ConfigSource {
                        role: "legacy-profile".into(),
                        path: profile_path,
                        sha256: sha256(&raw),
                        bytes: raw.len(),
                    });
                }
            }
            if legacy_workspace::codex_configured(
                &parent.join(".codex"),
                user_codex,
                &mut context.sources,
                &mut aggregate_bytes,
            )
            .map_err(|_| error("invalid project Codex routing metadata; misc fallback refused"))?
            {
                context.project.configured = true;
                context.project.boundaries.push(parent.to_owned());
                if context.project.root.is_none() {
                    context.project.root = Some(parent.to_owned());
                }
            }
        }
    }
    // Keep all boundary witnesses: a loopback symlink can expose the selected
    // root's own marker before its lexically enclosing nested-project marker.
    context.project.boundaries.sort();
    context.project.boundaries.dedup();
    // A platform alias may expose the same metadata through several ancestry
    // chains. Freeze one canonical witness and refuse conflicting reads.
    let mut unique = BTreeMap::new();
    for mut source in std::mem::take(&mut context.sources) {
        source.path = source
            .path
            .canonicalize()
            .map_err(|_| error("metadata source disappeared during selection"))?;
        let key = (source.role.clone(), source.path.clone());
        if let Some(old) = unique.insert(key, source.clone())
            && old != source
        {
            return Err(error(
                "metadata source changed during selection; retry native selection",
            ));
        }
    }
    context.sources = unique.into_values().collect();
    Ok((config, context))
}

/// Opt-in discovery never parses legacy policy before finding a human native
/// config. Existing adapters retain their own admission/refusal contracts.
pub fn discover_workspace(
    user_path: Option<&Path>,
    cwd: &Path,
    profile: Option<&str>,
) -> Result<Option<SelectionSnapshot>> {
    if let Some(path) = user_path {
        return select_workspace(path, cwd, profile).map(Some);
    }
    let default = default_user_path().ok();
    resolve_request(&make_request(
        SelectionRequestMode::Discover,
        default.as_deref(),
        cwd,
        profile,
    )?)
}
#[cfg(test)]
fn discover_from_default(
    default: &Path,
    cwd: &Path,
    profile: Option<&str>,
) -> Result<Option<SelectionSnapshot>> {
    resolve_request(&make_request(
        SelectionRequestMode::Discover,
        Some(default),
        cwd,
        profile,
    )?)
}
fn absolute_request_path(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|_| error("cannot resolve request directory"))?
            .join(path)
    };
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(error(
                    "selection request paths must not contain parent components",
                ));
            }
            component => clean.push(component.as_os_str()),
        }
    }
    validate_path(&clean)?;
    Ok(clean)
}
fn make_request(
    mode: SelectionRequestMode,
    user_path: Option<&Path>,
    cwd: &Path,
    profile: Option<&str>,
) -> Result<SelectionRequest> {
    let cwd = absolute_request_path(cwd)?;
    let origins = legacy_workspace::workspace_origins(&cwd)
        .map_err(|_| error("cannot resolve workspace directory"))?;
    let lexical_pwd = if origins.len() == 3 {
        Some(origins[0].clone())
    } else {
        None
    };
    let user_codex_dir = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")));
    if let Some(profile) = profile {
        validate_profile(profile)?;
    }
    Ok(SelectionRequest {
        mode,
        purpose: SelectionPurpose::Workspace,
        user_path: user_path.map(absolute_request_path).transpose()?,
        cwd,
        requested_profile: profile.map(str::to_owned),
        lexical_pwd,
        user_codex_dir,
    })
}
pub(crate) fn prepare_request(request: &SelectionRequest) -> Result<Option<WorkspaceDeclarations>> {
    validate_path(&request.cwd)?;
    if let Some(path) = &request.user_path {
        validate_path(path)?;
    }
    if let Some(profile) = &request.requested_profile {
        validate_profile(profile)?;
    }
    let canonical = request
        .cwd
        .canonicalize()
        .map_err(|_| error("workspace disappeared or changed; reselect"))?;
    let mut origins = vec![];
    if let Some(pwd) = &request.lexical_pwd {
        validate_path(pwd)?;
        if pwd.canonicalize().ok().as_ref() != Some(&canonical) {
            return Err(error("lexical workspace identity changed; reselect"));
        }
        origins.push(pwd.clone());
    }
    origins.extend([request.cwd.clone(), canonical]);
    let user_path = match request.mode {
        SelectionRequestMode::Explicit => Some(
            request
                .user_path
                .as_deref()
                .ok_or_else(|| error("explicit selection request requires user_path"))?,
        ),
        SelectionRequestMode::Discover => match request.user_path.as_deref() {
            Some(path) if declarative_file::present(path)? => Some(path),
            _ => None,
        },
    };
    if user_path.is_none() && !has_native_overlay(&origins)? {
        return Ok(None);
    }
    let (local, mut context) = prepare_workspace_inner(
        user_path,
        &request.cwd,
        request.requested_profile.as_deref(),
        origins,
        request.user_codex_dir.as_deref(),
    )?;
    context.purpose = request.purpose;
    Ok(Some(WorkspaceDeclarations {
        local,
        context,
        request: request.clone(),
    }))
}
pub(crate) fn resolve_request(request: &SelectionRequest) -> Result<Option<SelectionSnapshot>> {
    prepare_request(request)?
        .map(|prepared| finish_workspace(prepared, None))
        .transpose()
}

pub(crate) fn replay_request(
    request: &SelectionRequest,
    witness: Option<&DiscoveryWitness>,
) -> Result<Option<SelectionSnapshot>> {
    let Some(prepared) = prepare_request(request)? else {
        return Ok(None);
    };
    match witness {
        None => finish_workspace(prepared, None).map(Some),
        Some(witness) => {
            if prepared.boundary_sha256()? != witness.boundary_sha256 {
                return Err(error(
                    "local discovery boundary changed; reselect native configuration",
                ));
            }
            let receipt = witness.binding.load()?;
            finish_workspace(prepared, Some((&receipt, witness.binding.clone()))).map(Some)
        }
    }
}

/// Local declarations and all privacy-boundary witnesses, collected before any
/// owner contact. The client owns acquisition; this value never opens a store.
#[derive(Debug, Clone)]
pub struct WorkspaceDeclarations {
    pub local: ConfigDeclarations,
    pub context: SelectionContext,
    pub request: SelectionRequest,
}
impl WorkspaceDeclarations {
    /// Reuse the same acquired metadata for an explicit read-only browser. The
    /// purpose changes selection, never which catalog publishers are contacted.
    pub fn for_browsing(mut self) -> Self {
        self.request.purpose = SelectionPurpose::Browse;
        self.context.purpose = SelectionPurpose::Browse;
        self
    }
    pub fn has_discovery(&self) -> bool {
        self.local.has_discovery()
    }
    pub fn discovery_seeds(&self) -> Result<Vec<DiscoverySeed>> {
        self.local
            .discovery_seeds()?
            .into_iter()
            .filter_map(
                |seed| match self.catalog_contact_allowed(&seed.id, &seed.label) {
                    Ok(true) => Some(Ok(seed)),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                },
            )
            .collect()
    }
    /// One referral admission rule for acquisition and frozen replay. A locally
    /// explicit setting wins by canonical ID; no remote alias grants authority.
    pub fn catalog_referral_allowed(
        &self,
        label: &str,
        declaration: &crate::StoreDeclaration,
    ) -> Result<bool> {
        Ok(crate::discovery_opt_in(&self.local, declaration)
            && self.catalog_contact_allowed(&declaration.id, label)?)
    }
    /// A finished receipt must account for every admitted referral. Omitting an
    /// opposing branch cannot turn a partial acquisition into permissive defaults.
    /// Explicitly incomplete graphs may omit bounded work, but remain fail-closed.
    pub fn validate_frozen_graph(&self, graph: &DiscoveryGraph) -> Result<()> {
        graph.validate_for(&self.local)?;
        let expected: BTreeSet<_> = self.discovery_seeds()?.into_iter().map(|s| s.id).collect();
        if graph.seeds.iter().cloned().collect::<BTreeSet<_>>() != expected {
            return Err(error(
                "discovery receipt seed set differs from admitted local seeds",
            ));
        }
        if self.local.has_discovery()
            && self.local.discovery_seeds()?.is_empty()
            && graph.incomplete.is_none()
        {
            return Err(error(
                "discovery has no complete locally routed seed; supply a seed route",
            ));
        }
        for (publisher, catalog) in &graph.catalogs {
            let mut expected = BTreeSet::new();
            for (label, declaration) in &catalog.stores {
                if self.catalog_referral_allowed(label, declaration)? {
                    expected.insert(declaration.id.clone());
                }
            }
            let actual: BTreeSet<_> = graph
                .edges
                .get(publisher)
                .into_iter()
                .flatten()
                .cloned()
                .collect();
            if !actual.is_subset(&expected) || (graph.incomplete.is_none() && actual != expected) {
                return Err(error(
                    "discovery receipt omits or changes an admitted referral; refresh configuration",
                ));
            }
        }
        let reached: BTreeSet<_> = graph
            .seeds
            .iter()
            .chain(graph.edges.values().flatten())
            .collect();
        if graph.incomplete.is_none()
            && reached
                .iter()
                .any(|id| !graph.catalogs.contains_key(*id) && !graph.unavailable.contains_key(*id))
        {
            return Err(error(
                "discovery receipt has unanswered publishers without coverage evidence",
            ));
        }
        Ok(())
    }
    pub fn resolve_graph(&self, graph: &DiscoveryGraph) -> Result<DiscoveryResolution> {
        resolve_discovery(&self.local, graph)
    }
    pub fn boundary_sha256(&self) -> Result<String> {
        // Catalog acquisition is independent of the eventual read purpose.
        // Retain the established receipt key and exact legacy serialization.
        let mut request = self.request.clone();
        request.purpose = SelectionPurpose::Workspace;
        let bytes = serde_json::to_vec(&(
            &request,
            &self.local,
            &self.context.cwd,
            &self.context.origins,
            &self.context.profile,
            &self.context.project,
            &self.context.sources,
        ))
        .map_err(|_| error("cannot encode local discovery boundary"))?;
        Ok(sha256(&bytes))
    }
    pub fn receipt_path(&self) -> Result<PathBuf> {
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|s| !s.is_empty())
                    .map(|h| PathBuf::from(h).join(".cache"))
            })
            .ok_or_else(|| error("set XDG_CACHE_HOME or HOME for native discovery receipts"))?;
        validate_path(&cache)?;
        Ok(cache
            .join("mneme/discovery")
            .join(format!("{}.json", self.boundary_sha256()?)))
    }
    /// Catalog-only contact is not activation. Local disable, explicit exclusion
    /// and privacy selectors remain ceilings, including before routes resolve.
    pub fn catalog_contact_allowed(&self, id: &str, proposed_label: &str) -> Result<bool> {
        crate::validate_id(id)?;
        crate::validate_name(proposed_label)?;
        let local = self.local.stores.iter().find(|(_, store)| store.id == id);
        let label = local
            .map(|(label, _)| label.as_str())
            .unwrap_or(proposed_label);
        let profile = match self.local.workspace.profile.as_deref() {
            Some(p) => {
                PrivacyProfile::parse(&self.context.profile)?.narrow(PrivacyProfile::parse(p)?)
            }
            None => PrivacyProfile::parse(&self.context.profile)?,
        };
        let explicit_allow = self
            .local
            .workspace
            .allow_stores
            .as_ref()
            .is_some_and(|v| v.iter().any(|n| n == label))
            || self
                .context
                .project
                .legacy_allow_stores
                .as_ref()
                .is_some_and(|v| v.iter().any(|n| n == label));
        if (profile == PrivacyProfile::Isolated && !explicit_allow)
            || self
                .local
                .workspace
                .allow_stores
                .as_ref()
                .is_some_and(|v| !v.iter().any(|n| n == label))
            || self
                .context
                .project
                .legacy_allow_stores
                .as_ref()
                .is_some_and(|v| !v.iter().any(|n| n == label))
            || (self.context.project.legacy_library.is_some()
                && self.context.project.legacy_allow_stores.is_none())
        {
            return Ok(false);
        }
        if let Some((_, store)) = local {
            if store.selected_discovery(self.local.device.as_deref()) == Some(false) {
                return Ok(false);
            }
            let base = store.active.as_ref();
            let patch = self
                .local
                .device
                .as_deref()
                .and_then(|d| store.devices.get(d))
                .and_then(|p| p.active.as_ref());
            let enabled = patch
                .and_then(|p| p.enabled)
                .or_else(|| base.and_then(|p| p.enabled))
                .unwrap_or(true);
            let excluded = patch
                .and_then(|p| p.excluded_roots.as_ref())
                .or_else(|| base.and_then(|p| p.excluded_roots.as_ref()));
            let profiles = patch
                .and_then(|p| p.profiles.as_ref())
                .or_else(|| base.and_then(|p| p.profiles.as_ref()));
            let profile_allowed = profiles
                .map(|p| p.iter().any(|p| p == profile.as_str()))
                .unwrap_or(profile != PrivacyProfile::Isolated);
            if !enabled
                || crate::selection::path_match(
                    &self.context.origins,
                    excluded.map(Vec::as_slice).unwrap_or_default(),
                )?
                || (!profile_allowed && !(profile == PrivacyProfile::Isolated && explicit_allow))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
/// Prepare the exact same explicit/discovered local request used by ordinary
/// selection, but defer remote catalogs and effective-store materialization.
pub fn prepare_workspace(
    user_path: Option<&Path>,
    cwd: &Path,
    profile: Option<&str>,
) -> Result<Option<WorkspaceDeclarations>> {
    let mode = if user_path.is_some() {
        SelectionRequestMode::Explicit
    } else {
        SelectionRequestMode::Discover
    };
    let default = if user_path.is_none() {
        default_user_path().ok()
    } else {
        None
    };
    prepare_request(&make_request(
        mode,
        user_path.or(default.as_deref()),
        cwd,
        profile,
    )?)
}
/// Finish from a native receipt already published by the client. No remote
/// I/O occurs here, including when verifying the pinned receipt.
pub fn finish_workspace(
    prepared: WorkspaceDeclarations,
    receipt: Option<(&DiscoveryReceipt, DiscoveryBinding)>,
) -> Result<SelectionSnapshot> {
    if prepared.context.purpose != prepared.request.purpose {
        return Err(error("selection purpose differs from the frozen request"));
    }
    let browse = prepared.request.purpose == SelectionPurpose::Browse;
    let (config, witness, project_roots, ordinary) = if let Some((receipt, binding)) = receipt {
        if !prepared.has_discovery() {
            return Err(error(
                "discovery receipt supplied without a local discovery opt-in",
            ));
        }
        receipt.validate()?;
        let boundary_sha256 = prepared.boundary_sha256()?;
        // Acquisition may have taken time. Do not publish a fresh selection from
        // stale local privacy metadata merely because its receipt matches the old
        // preparation. This recheck is metadata-only and cannot refresh catalogs.
        let current = prepare_request(&prepared.request)?
            .ok_or_else(|| error("local configuration disappeared during discovery"))?;
        if current.boundary_sha256()? != boundary_sha256 {
            return Err(error(
                "local discovery boundary changed during acquisition; reselect",
            ));
        }
        if receipt.boundary_sha256 != boundary_sha256 {
            return Err(error(
                "discovery receipt belongs to a different local boundary; refresh configuration",
            ));
        }
        prepared.validate_frozen_graph(&receipt.graph)?;
        if binding.load()? != *receipt {
            return Err(error("discovery receipt changed before selection; retry"));
        }
        let ordinary = prepared.resolve_graph(&receipt.graph)?;
        let (resolved, ordinary) = if browse {
            (
                crate::discovery::resolve_discovery_for_purpose(
                    &prepared.local,
                    &receipt.graph,
                    SelectionPurpose::Browse,
                )?,
                Some(ordinary),
            )
        } else {
            (ordinary, None)
        };
        (
            resolved.config,
            Some(DiscoveryWitness {
                binding,
                boundary_sha256,
            }),
            resolved.project_roots,
            ordinary,
        )
    } else {
        if prepared.has_discovery() {
            return Err(error(
                "discovery configuration requires native client acquisition or a pinned receipt",
            ));
        }
        if browse {
            let ordinary = prepared.resolve_graph(&DiscoveryGraph::default())?;
            let resolved = crate::discovery::resolve_discovery_for_purpose(
                &prepared.local,
                &DiscoveryGraph::default(),
                SelectionPurpose::Browse,
            )?;
            (
                resolved.config,
                None,
                resolved.project_roots,
                Some(ordinary),
            )
        } else {
            (prepared.local.materialize()?, None, Vec::new(), None)
        }
    };
    let context = context_with_project_roots(prepared.context, project_roots)?;
    // Applicability still provides a useful initial view, but never gates a
    // deliberate browse. An unresolved/ambiguous automatic default is not an
    // error for the observatory; its picker can choose another readable owner.
    let preferred_default = ordinary.and_then(|ordinary| {
        let mut ordinary_context = context.clone();
        ordinary_context.purpose = SelectionPurpose::Workspace;
        context_with_project_roots(ordinary_context, ordinary.project_roots)
            .and_then(|context| crate::selection::select_resolved(&ordinary.config, &context))
            .ok()
            .and_then(|selection| selection.read_default)
    });
    let mut snapshot = if witness.is_some() || browse {
        crate::selection::select_resolved(&config, &context)?
    } else {
        select(&config, &context)?
    };
    if let Some(preferred) = preferred_default.filter(|name| {
        snapshot
            .stores
            .iter()
            .any(|row| row.name == *name && row.eligible.read)
    }) {
        snapshot.read_default = Some(preferred);
    }
    snapshot.request = Some(prepared.request);
    snapshot.discovery = witness;
    snapshot.digest = snapshot.calculated_digest()?;
    Ok(snapshot)
}

fn context_with_project_roots(
    mut context: SelectionContext,
    project_roots: Vec<PathBuf>,
) -> Result<SelectionContext> {
    for root in project_roots {
        if crate::selection::path_match(&context.origins, std::slice::from_ref(&root))? {
            // An unresolved/conflicted project owner still proves configured
            // project presence. Dropping its route cannot authorize misc fallback.
            context.project.configured = true;
            if context.project.root.is_none() {
                context.project.root = Some(root);
            }
        }
    }
    Ok(context)
}

fn infer_project_roots(config: &mut ConfigDeclarations, root: &Path) {
    for store in config.stores.values_mut() {
        let active = store.active.get_or_insert_default();
        if active.workspace.unwrap_or_default() == WorkspaceKind::Project
            && active.roots.as_ref().is_none_or(Vec::is_empty)
            && active.subtree_roots.as_ref().is_none_or(Vec::is_empty)
        {
            active.roots = Some(vec![root.to_owned()]);
        }
    }
}
fn merge_project_declarations(
    mut user: ConfigDeclarations,
    mut project: ConfigDeclarations,
    root: &Path,
) -> Result<ConfigDeclarations> {
    validate_path(root)?;
    for (overlay_value, user_value, default) in [
        (
            project.background.as_ref().and_then(|v| v.model.as_ref()),
            user.background.as_ref().and_then(|v| v.model.as_ref()),
            crate::default_model(),
        ),
        (
            project.background.as_ref().and_then(|v| v.effort.as_ref()),
            user.background.as_ref().and_then(|v| v.effort.as_ref()),
            crate::default_effort(),
        ),
    ] {
        if let Some(value) = overlay_value
            && Some(value) != default.as_ref()
            && Some(value) != user_value.or(default.as_ref())
        {
            return Err(error(
                "project overlay cannot replace user model/effort or read_default",
            ));
        }
    }
    if project
        .read_default
        .as_deref()
        .is_some_and(|v| v != default_rule())
    {
        return Err(error(
            "project overlay cannot replace user model/effort or read_default",
        ));
    }
    if project.device.is_some() && project.device != user.device {
        return Err(error(
            "project overlay cannot replace the client device binding",
        ));
    }
    if project.background.as_ref().and_then(|v| v.enabled) == Some(false) {
        user.background.get_or_insert_default().enabled = Some(false);
    }
    if let Some(setup) = project.setup.take() {
        user.setup.get_or_insert_default().overlay(setup);
    }
    infer_project_roots(&mut project, root);
    let ids: BTreeSet<_> = user.stores.values().map(|s| s.id.clone()).collect();
    for (name, store) in project.stores {
        if ids.contains(&store.id) || user.stores.insert(name, store).is_some() {
            return Err(error(
                "project overlay duplicates a user store name or canonical identity",
            ));
        }
    }
    for (name, library) in project.libraries {
        if user.libraries.insert(name, library).is_some() {
            return Err(error("project overlay duplicates a user library name"));
        }
    }
    user.workspace.profile = crate::privacy::narrow_unvalidated_profiles(
        user.workspace.profile,
        project.workspace.profile,
    );
    fn narrow(old: Option<Vec<String>>, new: Option<Vec<String>>) -> Option<Vec<String>> {
        match (old, new) {
            (Some(old), Some(new)) => Some(old.into_iter().filter(|v| new.contains(v)).collect()),
            (old, None) => old,
            (None, new) => new,
        }
    }
    user.workspace.allow_stores =
        narrow(user.workspace.allow_stores, project.workspace.allow_stores);
    user.workspace.allow_libraries = narrow(
        user.workspace.allow_libraries,
        project.workspace.allow_libraries,
    );
    user.validate()?;
    Ok(user)
}

fn has_native_overlay(origins: &[PathBuf]) -> Result<bool> {
    let mut seen = BTreeSet::new();
    for origin in origins {
        for (index, parent) in origin.ancestors().enumerate() {
            if index >= 64 {
                return Err(error("native configuration search exceeds 64 ancestors"));
            }
            if !seen.insert(parent.to_owned()) {
                continue;
            }
            let marker = parent.join(".mneme");
            let overlay = marker.join("memory.toml");
            if let Ok(metadata) = std::fs::symlink_metadata(&marker)
                && !metadata.is_dir()
                && !metadata.is_symlink()
            {
                break;
            }
            match std::fs::symlink_metadata(&overlay) {
                Ok(_) => return Ok(true), // Strict shared loader rejects invalid present files/parents.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(error("cannot inspect native project configuration")),
            }
            if std::fs::symlink_metadata(&marker).is_ok()
                || std::fs::symlink_metadata(parent.join(".git")).is_ok()
            {
                break;
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn default_discovery_follows_config_symlinks_but_not_broken_link_absence() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("managed.toml");
        let default = dir.path().join("memory.toml");
        assert!(
            discover_from_default(&default, dir.path(), None)
                .unwrap()
                .is_none()
        );
        symlink("managed.toml", &default).unwrap();
        assert!(discover_from_default(&default, dir.path(), None).is_err());
        symlink("memory.toml", &target).unwrap();
        assert!(discover_from_default(&default, dir.path(), None).is_err());
        std::fs::remove_file(&target).unwrap();
        std::fs::write(&target, "schema='mneme.config.v1'").unwrap();
        let selected = discover_from_default(&default, dir.path(), None)
            .unwrap()
            .unwrap();
        selected.revalidate_exact().unwrap();
        std::fs::remove_file(&target).unwrap();
        assert!(selected.revalidate_exact().is_err());
        // A dangling parent directory link is also present invalid metadata.
        assert!(discover_from_default(&default.join("child.toml"), dir.path(), None).is_err());
        std::fs::create_dir(&target).unwrap();
        assert!(discover_from_default(&default, dir.path(), None).is_err());
        assert!(
            discover_from_default(&default.join("child.toml"), dir.path(), None)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn discovered_overlay_retarget_invalidates_even_identical_contents() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("absent-user.toml");
        std::fs::create_dir(dir.path().join(".mneme")).unwrap();
        let old = dir.path().join("old.toml");
        let new = dir.path().join("new.toml");
        let overlay = dir.path().join(".mneme/memory.toml");
        std::fs::write(&old, "schema='mneme.config.v1'").unwrap();
        std::fs::copy(&old, &new).unwrap();
        symlink(&old, &overlay).unwrap();
        let selected = discover_from_default(&default, dir.path(), None)
            .unwrap()
            .unwrap();
        selected.revalidate_exact().unwrap();
        std::fs::remove_file(&overlay).unwrap();
        symlink(&new, &overlay).unwrap();
        assert!(selected.revalidate_exact().is_err());
        discover_from_default(&default, dir.path(), None)
            .unwrap()
            .unwrap()
            .revalidate_exact()
            .unwrap();
    }

    #[test]
    fn absent_user_path_is_frozen_and_new_file_invalidates_original_request() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("user.toml");
        std::fs::create_dir(dir.path().join(".mneme")).unwrap();
        std::fs::write(dir.path().join(".mneme/memory.toml"),"schema='mneme.config.v1'\n[stores.work]\nid='01M3CV5EZP4HWAK56YJCMZQG2Y'\nowner={url='http://127.0.0.1:1',db='project'}").unwrap();
        let snapshot = discover_from_default(&default, dir.path(), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot.request.as_ref().unwrap().user_path.as_deref(),
            Some(default.as_path())
        );
        snapshot.revalidate_exact().unwrap();
        std::fs::write(&default, "schema='mneme.config.v1'").unwrap();
        assert!(snapshot.revalidate_exact().is_err());
    }
    #[test]
    fn absent_native_configuration_does_not_parse_legacy_profiles_or_codex_routing() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("missing.toml");
        std::fs::create_dir(dir.path().join(".mneme")).unwrap();
        std::fs::create_dir(dir.path().join(".codex")).unwrap();
        std::fs::write(dir.path().join(".mneme/profile.json"), "{}").unwrap();
        std::fs::write(dir.path().join(".codex/config.toml"), "invalid=!").unwrap();
        assert!(
            discover_from_default(&default, dir.path(), None)
                .unwrap()
                .is_none()
        );
        std::fs::write(
            dir.path().join(".mneme/memory.toml"),
            "schema='mneme.config.v1'",
        )
        .unwrap();
        assert!(discover_from_default(&default, dir.path(), None).is_err());
    }
    #[test]
    fn absent_default_project_overlay_is_native_but_legacy_presence_is_not_silent_translation() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("missing.toml");
        assert!(
            discover_from_default(&default, dir.path(), None)
                .unwrap()
                .is_none()
        );
        std::fs::create_dir(dir.path().join(".mneme")).unwrap();
        assert!(
            discover_from_default(&default, dir.path(), None)
                .unwrap()
                .is_none()
        );
        let overlay = dir.path().join(".mneme/memory.toml");
        std::fs::write(&overlay,"schema='mneme.config.v1'\n[stores.work]\nid='01M3CV5EZP4HWAK56YJCMZQG2Y'\nowner={url='http://127.0.0.1:1',db='project'}").unwrap();
        let snapshot = discover_from_default(&default, dir.path(), None)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.active.len(), 1);
        assert_eq!(snapshot.sources[0].role, "project");
        std::fs::write(&overlay, "invalid=!").unwrap();
        assert!(discover_from_default(&default, dir.path(), None).is_err());
        assert!(discover_workspace(Some(&default), dir.path(), None).is_err());
    }
}
