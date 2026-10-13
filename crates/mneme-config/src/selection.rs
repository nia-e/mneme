use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectState {
    pub configured: bool,
    pub root: Option<PathBuf>,
    pub overlay: Option<PathBuf>,
    #[serde(default)]
    pub boundaries: Vec<PathBuf>,
    pub legacy_library: Option<PathBuf>,
    pub legacy_allow_stores: Option<Vec<String>>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfigSource {
    pub role: String,
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: usize,
}
#[derive(Debug, Clone)]
pub struct SelectionContext {
    pub cwd: PathBuf,
    pub profile: String,
    pub project: ProjectState,
    pub sources: Vec<ConfigSource>,
    /// Validated lexical aliases plus canonical cwd; used for inherited exclusions.
    pub origins: Vec<PathBuf>,
}
impl SelectionContext {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            origins: vec![cwd.clone()],
            cwd,
            profile: "default".into(),
            project: ProjectState::default(),
            sources: vec![],
        }
    }
}
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
    pub user_path: Option<PathBuf>,
    pub cwd: PathBuf,
    pub requested_profile: Option<String>,
    pub lexical_pwd: Option<PathBuf>,
    pub user_codex_dir: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SelectionSnapshot {
    pub schema: String,
    pub digest: String,
    #[serde(default)]
    pub request: Option<SelectionRequest>,
    pub sources: Vec<ConfigSource>,
    pub config: Config,
    pub cwd: PathBuf,
    pub origins: Vec<PathBuf>,
    pub profile: String,
    pub project: ProjectState,
    pub stores: Vec<StoreSelection>,
    pub active: Vec<ActiveStore>,
    pub read_default: Option<String>,
    pub libraries: BTreeMap<String, Library>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoreSelection {
    pub name: String,
    pub store: Store,
    pub active: bool,
    pub admitted: bool,
    pub reason: Option<String>,
    pub eligible: Eligibility,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActiveStore {
    pub name: String,
    pub store: Store,
    pub eligible: Eligibility,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Eligibility {
    pub read: bool,
    pub write: bool,
    pub core: bool,
    pub context: bool,
    pub recall: bool,
    pub record: bool,
    pub maintenance: bool,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Operation {
    ExplicitRead,
    ExplicitWrite,
    Core,
    Context,
    Recall,
    Record,
    Maintenance,
}
impl ActiveStore {
    pub fn require_operation(&self, operation: Operation) -> Result<()> {
        let permitted = match operation {
            Operation::ExplicitRead => self.eligible.read,
            Operation::ExplicitWrite => self.eligible.write,
            Operation::Core => self.eligible.core,
            Operation::Context => self.eligible.context,
            Operation::Recall => self.eligible.recall,
            Operation::Record => self.eligible.record,
            Operation::Maintenance => self.eligible.maintenance,
        };
        if !permitted {
            return Err(error(
                "store operation not granted by current selection; check profile, permissions and delivery/background applicability",
            ));
        }
        Ok(())
    }
}
impl SelectionSnapshot {
    /// Deliberate non-active access does not activate background delivery. Profile,
    /// isolation, exclusions and disabled-store boundaries still apply.
    pub fn explicit_store(&self, name: &str) -> Result<ActiveStore> {
        let store = self
            .stores
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| error("store is not configured"))?;
        if !store.admitted {
            return Err(error(
                "store is outside the admitted workspace/profile boundary",
            ));
        }
        Ok(ActiveStore {
            name: store.name.clone(),
            store: store.store.clone(),
            eligible: store.eligible,
        })
    }
    pub fn verify_digest(&self) -> Result<()> {
        if self.schema != SELECTION_SCHEMA || self.digest != self.calculated_digest()? {
            return Err(error(
                "selection snapshot digest/schema mismatch; reselect native configuration",
            ));
        }
        Ok(())
    }
    /// Refuse stale admission; never hot-swap a client or a paid proposal. Pure
    /// `select` snapshots deliberately have no filesystem request: they are
    /// immutable caller-owned in-memory metadata, not filesystem continuity proof.
    pub fn revalidate_exact(&self) -> Result<()> {
        self.verify_digest()?;
        if let Some(request) = &self.request {
            let current = resolve_request(request)?.ok_or_else(|| {
                error("native selection no longer configured; reselect and reassess")
            })?;
            if current.digest != self.digest {
                return Err(error(
                    "selection metadata changed; reselect and reassess the original operation",
                ));
            }
        }
        Ok(())
    }
    fn calculated_digest(&self) -> Result<String> {
        let mut frozen = self.clone();
        frozen.digest.clear();
        let bytes =
            serde_json::to_vec(&frozen).map_err(|_| error("cannot encode selection witness"))?;
        // A source envelope is bounded at admission; refuse explosive overlay expansion too.
        if bytes.len() > MAX_CONFIG_BYTES * 8 {
            return Err(error("resolved selection snapshot exceeds 524288 bytes"));
        }
        Ok(sha256(&bytes))
    }
}
fn path_match(origins: &[PathBuf], roots: &[PathBuf]) -> Result<bool> {
    for root in roots {
        let canonical = match root.canonicalize() {
            Ok(root) => Some(root),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(error("cannot resolve configured workspace root")),
        };
        if origins
            .iter()
            .any(|p| p.starts_with(root) || canonical.as_ref().is_some_and(|r| p.starts_with(r)))
        {
            return Ok(true);
        }
    }
    Ok(false)
}
fn positive_match(origins: &[PathBuf], roots: &[PathBuf]) -> Result<bool> {
    for origin in origins {
        if !path_match(std::slice::from_ref(origin), roots)? {
            return Ok(false);
        }
    }
    Ok(true)
}
fn project_boundary_admitted(project: &ProjectState, roots: &[PathBuf]) -> Result<bool> {
    for boundary in &project.boundaries {
        if path_match(std::slice::from_ref(boundary), roots)?
            && !roots.iter().any(|root| {
                root == boundary
                    || root
                        .canonicalize()
                        .ok()
                        .zip(boundary.canonicalize().ok())
                        .is_some_and(|(a, b)| a == b)
            })
        {
            return Ok(false);
        }
    }
    Ok(true)
}
// Positive roots grant work in that project, not every nested project beneath
// it. Test each candidate independently: explicitly listing a child root may
// admit it even when an ancestor candidate is fenced by the child's marker.
fn positive_boundary_match(
    origins: &[PathBuf],
    project: &ProjectState,
    roots: &[PathBuf],
) -> Result<bool> {
    for origin in origins {
        let mut admitted = false;
        for root in roots {
            if path_match(std::slice::from_ref(origin), std::slice::from_ref(root))?
                && project_boundary_admitted(project, std::slice::from_ref(root))?
            {
                admitted = true;
                break;
            }
        }
        if !admitted {
            return Ok(false);
        }
    }
    Ok(true)
}
fn scope_match(
    origins: &[PathBuf],
    project: &ProjectState,
    scope: &OperationScope,
) -> Result<bool> {
    Ok(
        (scope.roots.is_empty() || positive_boundary_match(origins, project, &scope.roots)?)
            && !path_match(origins, &scope.excluded_roots)?,
    )
}
pub(crate) fn profile_rank(profile: &str) -> u8 {
    match profile {
        "isolated" => 2,
        "private" => 1,
        _ => 0,
    }
}

pub fn select(config: &Config, context: &SelectionContext) -> Result<SelectionSnapshot> {
    config.validate()?;
    validate_path(&context.cwd)?;
    validate_profile(&context.profile)?;
    if context.origins.len() > 3 {
        return Err(error("workspace origin witness exceeds bound"));
    }
    for origin in &context.origins {
        validate_path(origin)?;
    }
    let mut origins = context.origins.clone();
    if !origins.contains(&context.cwd) {
        origins.push(context.cwd.clone());
    }
    let profile = config
        .workspace
        .profile
        .as_ref()
        .filter(|p| profile_rank(p) > profile_rank(&context.profile))
        .unwrap_or(&context.profile)
        .clone();
    let mut project = context.project.clone();
    // Disabled project entries still prove configured presence. Availability is
    // intentionally not an input, so a connection failure can never activate misc.
    for store in config.stores.values() {
        if store.active.workspace == WorkspaceKind::Project
            && !store.active.roots.is_empty()
            && path_match(&origins, &store.active.roots)?
        {
            project.configured = true;
            if project.root.is_none() {
                project.root = store
                    .active
                    .roots
                    .iter()
                    .find(|r| origins.iter().any(|p| p.starts_with(r)))
                    .cloned();
            }
        }
    }
    let mut stores = vec![];
    let mut active = vec![];
    for (name, store) in &config.stores {
        let isolated = profile == "isolated";
        let allowlisted = config
            .workspace
            .allow_stores
            .as_ref()
            .is_some_and(|v| v.contains(name))
            || project
                .legacy_allow_stores
                .as_ref()
                .is_some_and(|v| v.contains(name));
        let allowlist_admitted = config
            .workspace
            .allow_stores
            .as_ref()
            .is_none_or(|v| v.contains(name))
            && project
                .legacy_allow_stores
                .as_ref()
                .is_none_or(|v| v.contains(name));
        let legacy_admitted =
            project.legacy_library.is_none() || project.legacy_allow_stores.is_some();
        let profile_allowed = store.active.profiles.contains(&profile) || (isolated && allowlisted);
        let excluded = path_match(&origins, &store.active.excluded_roots)?;
        let admitted = store.active.enabled
            && profile_allowed
            && (!isolated || allowlisted)
            && allowlist_admitted
            && legacy_admitted
            && !excluded;
        let workspace_matches = match store.active.workspace {
            WorkspaceKind::Any => true,
            WorkspaceKind::Project => {
                project.configured
                    && !store.active.roots.is_empty()
                    && positive_boundary_match(&origins, &project, &store.active.roots)?
            }
            WorkspaceKind::NoProject => !project.configured,
        };
        let roots_match =
            store.active.roots.is_empty() || positive_match(&origins, &store.active.roots)?;
        let is_active = admitted && workspace_matches && roots_match;
        let read = admitted && store.permissions.read;
        let write = admitted && store.permissions.write;
        let bg = is_active && config.background.enabled;
        let eligible = Eligibility {
            read,
            write,
            core: is_active
                && read
                && store.delivery.core
                && scope_match(&origins, &project, &store.applicability.core)?,
            context: is_active
                && read
                && store.delivery.context
                && scope_match(&origins, &project, &store.applicability.context)?,
            recall: bg
                && read
                && store.delivery.context
                && store.background.recall
                && scope_match(&origins, &project, &store.applicability.context)?
                && scope_match(&origins, &project, &store.applicability.recall)?,
            record: bg
                && write
                && store.background.record
                && scope_match(&origins, &project, &store.applicability.record)?,
            maintenance: bg
                && read
                && write
                && store.background.maintenance
                && scope_match(&origins, &project, &store.applicability.maintenance)?,
        };
        let reason = if !store.active.enabled {
            Some("disabled")
        } else if excluded {
            Some("excluded-root")
        } else if isolated && !allowlisted {
            Some("isolated-not-allowlisted")
        } else if !allowlist_admitted {
            Some("workspace-allowlist")
        } else if !legacy_admitted {
            Some("explicit-legacy-library")
        } else if !profile_allowed {
            Some("profile-not-admitted")
        } else if !workspace_matches {
            Some("workspace-not-applicable")
        } else if !roots_match {
            Some("root-not-applicable")
        } else {
            None
        }
        .map(str::to_owned);
        stores.push(StoreSelection {
            name: name.clone(),
            store: store.clone(),
            admitted,
            active: is_active,
            reason,
            eligible,
        });
        if is_active {
            active.push(ActiveStore {
                name: name.clone(),
                store: store.clone(),
                eligible,
            });
        }
    }
    let read_default = if config.read_default == "project-or-misc" {
        let kind = if project.configured {
            WorkspaceKind::Project
        } else {
            WorkspaceKind::NoProject
        };
        let candidates: Vec<_> = active
            .iter()
            .filter(|s| s.eligible.read && s.store.active.workspace == kind)
            .collect();
        if candidates.len() > 1 {
            return Err(error(
                "ambiguous project-or-misc read default; choose a configured store name explicitly",
            ));
        }
        candidates.first().map(|s| s.name.clone())
    } else {
        active
            .iter()
            .find(|s| s.name == config.read_default && s.eligible.read)
            .map(|s| s.name.clone())
    };
    let libraries = config
        .libraries
        .iter()
        .filter(|(name, library)| {
            project.legacy_library.is_none()
                && config
                    .workspace
                    .allow_libraries
                    .as_ref()
                    .is_none_or(|v| v.contains(name))
                && if profile == "isolated" {
                    config
                        .workspace
                        .allow_libraries
                        .as_ref()
                        .is_some_and(|v| v.contains(name))
                } else {
                    library.profiles.contains(&profile)
                }
        })
        .map(|(name, library)| (name.clone(), library.clone()))
        .collect();
    let mut snapshot = SelectionSnapshot {
        schema: SELECTION_SCHEMA.into(),
        digest: String::new(),
        request: None,
        sources: context.sources.clone(),
        config: config.clone(),
        cwd: context.cwd.clone(),
        origins,
        profile,
        project,
        stores,
        active,
        read_default,
        libraries,
    };
    snapshot.digest = snapshot.calculated_digest()?;
    Ok(snapshot)
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
fn load_source(path: &Path, role: &str) -> Result<(Config, ConfigSource)> {
    let raw = legacy_workspace::read(path)
        .map_err(|_| {
            error("cannot read configuration: repair missing, oversized or symlinked metadata")
        })?
        .ok_or_else(|| {
            error("configuration file is missing; create memory.toml or choose --config")
        })?;
    let text = std::str::from_utf8(&raw).map_err(|_| error("configuration must be UTF-8"))?;
    let config = if role == "project" {
        Config::parse_overlay(text)?
    } else {
        Config::parse(text)?
    };
    let path = path
        .canonicalize()
        .map_err(|_| error("cannot resolve configuration source"))?;
    Ok((
        config,
        ConfigSource {
            role: role.into(),
            path,
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
fn select_workspace_inner(
    user_path: Option<&Path>,
    cwd: &Path,
    requested_profile: Option<&str>,
    origins: Vec<PathBuf>,
    user_codex: Option<&Path>,
) -> Result<SelectionSnapshot> {
    let mut config = Config {
        schema: CONFIG_SCHEMA.into(),
        read_default: default_rule(),
        background: Background::default(),
        workspace: Workspace::default(),
        stores: BTreeMap::new(),
        libraries: BTreeMap::new(),
    };
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
    validate_profile(&context.profile)?;
    let mut seen = BTreeSet::new();
    let mut overlay_root: Option<PathBuf> = None;
    let mut profile_rank_seen = 0;
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
                if legacy_workspace::inspect(&overlay)
                    .map_err(|_| error("invalid project memory.toml metadata"))?
                    .is_some()
                {
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
                            for store in config.stores.values_mut() {
                                if store.active.workspace == WorkspaceKind::Project
                                    && store.active.roots.is_empty()
                                {
                                    store.active.roots.push(parent.to_owned());
                                }
                            }
                        } else {
                            config = config.merge_project(project_config, parent)?;
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
                    validate_profile(&profile.mode)?;
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
                    if profile_rank(&profile.mode) > profile_rank_seen {
                        profile_rank_seen = profile_rank(&profile.mode);
                        if profile_rank_seen > profile_rank(&context.profile) {
                            context.profile = profile.mode;
                        }
                    }
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
        if let Some(old) = unique.insert(key, source.clone()) {
            if old != source {
                return Err(error(
                    "metadata source changed during selection; retry native selection",
                ));
            }
        }
    }
    context.sources = unique.into_values().collect();
    select(&config, &context)
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
        user_path: user_path.map(absolute_request_path).transpose()?,
        cwd,
        requested_profile: profile.map(str::to_owned),
        lexical_pwd,
        user_codex_dir,
    })
}
fn resolve_request(request: &SelectionRequest) -> Result<Option<SelectionSnapshot>> {
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
            Some(path)
                if legacy_workspace::inspect(path)
                    .map_err(|_| error("cannot inspect default memory.toml"))?
                    .is_some() =>
            {
                Some(path)
            }
            _ => None,
        },
    };
    if user_path.is_none() && !has_native_overlay(&origins)? {
        return Ok(None);
    }
    let mut snapshot = select_workspace_inner(
        user_path,
        &request.cwd,
        request.requested_profile.as_deref(),
        origins,
        request.user_codex_dir.as_deref(),
    )?;
    snapshot.request = Some(request.clone());
    snapshot.digest = snapshot.calculated_digest()?;
    Ok(Some(snapshot))
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
            if let Ok(metadata) = std::fs::symlink_metadata(&marker) {
                if !metadata.is_dir() && !metadata.is_symlink() {
                    break;
                }
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
