//! Selection policy and its stable metadata snapshot contract.

use crate::{
    Config, Library, MAX_CONFIG_BYTES, OperationScope, PrivacyProfile, Result, SELECTION_SCHEMA,
    SelectionRequest, Store, WorkspaceKind, error, sha256, validate_path,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

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
// Wire snapshots retain their established fields. Internally, admission,
// activation and the explanation are one decision rather than independent facts.
#[derive(Debug, Clone, Copy)]
enum StoreDisposition {
    Denied(DenialReason),
    Inactive(InactiveReason),
    Active,
}

#[derive(Debug, Clone, Copy)]
enum DenialReason {
    Disabled,
    ExcludedRoot,
    IsolatedNotAllowlisted,
    WorkspaceAllowlist,
    ExplicitLegacyLibrary,
    ProfileNotAdmitted,
}

#[derive(Debug, Clone, Copy)]
enum InactiveReason {
    WorkspaceNotApplicable,
    RootNotApplicable,
}

impl StoreDisposition {
    fn is_admitted(self) -> bool {
        !matches!(self, Self::Denied(_))
    }

    fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    fn reason(self) -> Option<&'static str> {
        match self {
            Self::Denied(reason) => Some(match reason {
                DenialReason::Disabled => "disabled",
                DenialReason::ExcludedRoot => "excluded-root",
                DenialReason::IsolatedNotAllowlisted => "isolated-not-allowlisted",
                DenialReason::WorkspaceAllowlist => "workspace-allowlist",
                DenialReason::ExplicitLegacyLibrary => "explicit-legacy-library",
                DenialReason::ProfileNotAdmitted => "profile-not-admitted",
            }),
            Self::Inactive(reason) => Some(match reason {
                InactiveReason::WorkspaceNotApplicable => "workspace-not-applicable",
                InactiveReason::RootNotApplicable => "root-not-applicable",
            }),
            Self::Active => None,
        }
    }

    fn into_selection(self, name: &str, store: &Store, eligible: Eligibility) -> StoreSelection {
        StoreSelection {
            name: name.to_owned(),
            store: store.clone(),
            active: self.is_active(),
            admitted: self.is_admitted(),
            reason: self.reason().map(str::to_owned),
            eligible,
        }
    }
}

/// Admitted store access. `SelectionSnapshot::explicit_store` can return this
/// for an inactive store; `eligible` determines which operations are available.
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
            let current = crate::workspace::resolve_request(request)?.ok_or_else(|| {
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
    pub(crate) fn calculated_digest(&self) -> Result<String> {
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
fn positive_match(
    origins: &[PathBuf],
    roots: &[PathBuf],
    subtree_roots: &[PathBuf],
) -> Result<bool> {
    for origin in origins {
        if !path_match(std::slice::from_ref(origin), roots)?
            && !path_match(std::slice::from_ref(origin), subtree_roots)?
        {
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
// Ordinary roots grant work in that project, not every nested project beneath
// it. Test each candidate independently: explicitly listing a child root may
// admit it even when an ancestor candidate is fenced by the child's marker.
// Subtree roots deliberately include nested projects, but must cover every
// lexical/canonical origin just like ordinary positive roots. Neither grant can
// bypass exclusions or the separately parsed privacy/overlay metadata.
fn positive_boundary_match(
    origins: &[PathBuf],
    project: &ProjectState,
    roots: &[PathBuf],
    subtree_roots: &[PathBuf],
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
        if !admitted && !path_match(std::slice::from_ref(origin), subtree_roots)? {
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
    Ok(((scope.roots.is_empty() && scope.subtree_roots.is_empty())
        || positive_boundary_match(origins, project, &scope.roots, &scope.subtree_roots)?)
        && !path_match(origins, &scope.excluded_roots)?)
}
pub fn select(config: &Config, context: &SelectionContext) -> Result<SelectionSnapshot> {
    config.validate()?;
    validate_path(&context.cwd)?;
    let context_profile = PrivacyProfile::parse(&context.profile)?;
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
    let profile = match config.workspace.profile.as_deref() {
        Some(profile) => context_profile.narrow(PrivacyProfile::parse(profile)?),
        None => context_profile,
    };
    let mut project = context.project.clone();
    // Disabled project entries still prove configured presence. Availability is
    // intentionally not an input, so a connection failure can never activate misc.
    for store in config.stores.values() {
        if store.active.workspace == WorkspaceKind::Project
            && store.active.has_positive_roots()
            && (path_match(&origins, &store.active.roots)?
                || path_match(&origins, &store.active.subtree_roots)?)
        {
            project.configured = true;
            if project.root.is_none() {
                project.root = store
                    .active
                    .roots
                    .iter()
                    .chain(&store.active.subtree_roots)
                    .find(|r| origins.iter().any(|p| p.starts_with(r)))
                    .cloned();
            }
        }
    }
    let mut stores = vec![];
    let mut active = vec![];
    for (name, store) in &config.stores {
        let isolated = profile == PrivacyProfile::Isolated;
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
        let profile_allowed = store.active.profiles.iter().any(|p| p == profile.as_str())
            || (isolated && allowlisted);
        let excluded = path_match(&origins, &store.active.excluded_roots)?;
        let workspace_matches = match store.active.workspace {
            WorkspaceKind::Any => true,
            WorkspaceKind::Project => {
                project.configured
                    && store.active.has_positive_roots()
                    && positive_boundary_match(
                        &origins,
                        &project,
                        &store.active.roots,
                        &store.active.subtree_roots,
                    )?
            }
            WorkspaceKind::NoProject => !project.configured,
        };
        let roots_match = !store.active.has_positive_roots()
            || positive_match(&origins, &store.active.roots, &store.active.subtree_roots)?;
        // Do not short-circuit path checks on denial: even disabled entries
        // retain their metadata-validation errors. Only operation scopes below
        // are conditional on eligibility.
        let disposition = if !store.active.enabled {
            StoreDisposition::Denied(DenialReason::Disabled)
        } else if excluded {
            StoreDisposition::Denied(DenialReason::ExcludedRoot)
        } else if isolated && !allowlisted {
            StoreDisposition::Denied(DenialReason::IsolatedNotAllowlisted)
        } else if !allowlist_admitted {
            StoreDisposition::Denied(DenialReason::WorkspaceAllowlist)
        } else if !legacy_admitted {
            StoreDisposition::Denied(DenialReason::ExplicitLegacyLibrary)
        } else if !profile_allowed {
            StoreDisposition::Denied(DenialReason::ProfileNotAdmitted)
        } else if !workspace_matches {
            StoreDisposition::Inactive(InactiveReason::WorkspaceNotApplicable)
        } else if !roots_match {
            StoreDisposition::Inactive(InactiveReason::RootNotApplicable)
        } else {
            StoreDisposition::Active
        };
        let is_active = disposition.is_active();
        let read = disposition.is_admitted() && store.permissions.read;
        let write = disposition.is_admitted() && store.permissions.write;
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
        stores.push(disposition.into_selection(name, store, eligible));
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
                && if profile == PrivacyProfile::Isolated {
                    config
                        .workspace
                        .allow_libraries
                        .as_ref()
                        .is_some_and(|v| v.contains(name))
                } else {
                    library.profiles.iter().any(|p| p == profile.as_str())
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
        profile: profile.as_str().to_owned(),
        project,
        stores,
        active,
        read_default,
        libraries,
    };
    snapshot.digest = snapshot.calculated_digest()?;
    Ok(snapshot)
}
