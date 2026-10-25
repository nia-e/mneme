//! Local setup preferences and metadata-only registration target selection.
//! These declarations are neither imported nor part of the executable Config.

use crate::{
    ConfigDeclarations, MAX_STORES, Owner, Result, bounded_text, error, unique_names,
    validate_endpoint, validate_name,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SetupDeclaration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<SetupDiscoveryDeclaration>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SetupDiscoveryDeclaration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SetupDiscoveryMode>,
    /// Local configured labels, not labels learned from another publisher.
    /// Omission inherits; an explicitly empty list clears inherited targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hubs: Option<Vec<String>>,
    /// Client-reachable route, optionally containing the source owner's {port}.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertise_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SetupDiscoveryMode {
    Auto,
    Ask,
    #[default]
    Off,
}

impl SetupDeclaration {
    pub fn validate(&self) -> Result<()> {
        if let Some(discovery) = &self.discovery {
            discovery.validate()?;
        }
        Ok(())
    }

    pub(crate) fn overlay(&mut self, overlay: Self) {
        if let Some(discovery) = overlay.discovery {
            let base = self.discovery.get_or_insert_default();
            if discovery.mode.is_some() {
                base.mode = discovery.mode;
            }
            if discovery.hubs.is_some() {
                base.hubs = discovery.hubs;
            }
            if discovery.advertise_url.is_some() {
                base.advertise_url = discovery.advertise_url;
            }
        }
    }
}

impl SetupDiscoveryDeclaration {
    pub fn mode(&self) -> SetupDiscoveryMode {
        self.mode.unwrap_or_default()
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(hubs) = &self.hubs {
            validate_hubs(hubs)?;
        }
        if let Some(template) = &self.advertise_url {
            // The bound port is only known after setup. Validate the template
            // now with a nonzero placeholder and validate its actual route later.
            setup_advertised_owner(template, "memory", 1)?;
        }
        Ok(())
    }
}

fn validate_hubs(hubs: &[String]) -> Result<()> {
    if hubs.len() > MAX_STORES {
        return Err(error(
            "setup discovery hubs exceed configured store count bound",
        ));
    }
    unique_names(hubs)
}

/// A locally configured destination for registration, not an imported seed or
/// a memory read/write selection. Discovery import opt-in does not govern this.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SetupDiscoveryHub {
    pub label: String,
    pub id: String,
    pub owner: Owner,
}

impl ConfigDeclarations {
    /// Resolve chosen labels without fetching catalogs or requiring activation,
    /// memory read/write eligibility, or discovery import opt-in.
    pub fn setup_discovery_hubs(&self, names: &[String]) -> Result<Vec<SetupDiscoveryHub>> {
        self.validate()?;
        validate_hubs(names)?;
        names
            .iter()
            .map(|label| {
                let store = self.stores.get(label).ok_or_else(|| {
                    error(&format!(
                        "setup discovery hub '{label}' is not locally configured; add its store declaration"
                    ))
                })?;
                let owner = store.selected_owner(self.device.as_deref()).ok_or_else(|| {
                    error(&format!(
                        "setup discovery hub '{label}' has no complete local owner route for this device; configure its owner"
                    ))
                })?;
                Ok(SetupDiscoveryHub {
                    label: label.clone(),
                    id: store.id.clone(),
                    owner,
                })
            })
            .collect()
    }
}

/// Render a publication route using the verified source owner's actual MCP
/// port. An SSH URL's explicit port remains its SSH connection port; the MCP
/// tunnel destination is always `bound_mcp_port`.
pub fn setup_advertised_owner(template: &str, db: &str, bound_mcp_port: u16) -> Result<Owner> {
    validate_name(db)?;
    let url = render_setup_advertise_url(template, bound_mcp_port)?;
    let ssh_mcp_port = url::Url::parse(&url)
        .map_err(|_| error("invalid setup advertised owner endpoint"))?
        .scheme()
        .eq("ssh")
        .then_some(bound_mcp_port);
    Ok(Owner {
        url,
        db: db.into(),
        ssh_mcp_port,
        token_env: None,
        path: None,
    })
}

/// Render and validate an advertised URL without guessing DNS or network
/// reachability. A shared route cannot point each client back at itself.
pub fn render_setup_advertise_url(template: &str, bound_mcp_port: u16) -> Result<String> {
    if bound_mcp_port == 0 {
        return Err(error(
            "setup advertised owner requires a nonzero bound MCP port",
        ));
    }
    bounded_text(template, 4096)?;
    let url = template.replace("{port}", &bound_mcp_port.to_string());
    if url.contains(['{', '}']) {
        return Err(error(
            "setup advertise_url supports only the {port} substitution",
        ));
    }
    let parsed =
        url::Url::parse(&url).map_err(|_| error("invalid setup advertised owner endpoint"))?;
    if parsed.port() == Some(0) {
        return Err(error("setup advertised owner URL port must be nonzero"));
    }
    let ssh_mcp_port = (parsed.scheme() == "ssh").then_some(bound_mcp_port);
    validate_endpoint(&url, ssh_mcp_port, None)?;
    // SSH is a non-special URL scheme, so Url leaves numeric hosts opaque.
    // Parse its host consistently with HTTP, including abbreviated IPv4 forms.
    let host = url::Host::parse(
        parsed
            .host_str()
            .ok_or_else(|| error("setup advertised owner requires a host"))?,
    )
    .map_err(|_| error("invalid setup advertised owner host"))?;
    let unsuitable = match host {
        url::Host::Domain(host) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host == "localhost" || host.ends_with(".localhost") || host.contains('*')
        }
        url::Host::Ipv4(host) => host.is_loopback() || host.is_unspecified(),
        url::Host::Ipv6(host) => {
            host.is_loopback()
                || host.is_unspecified()
                || host
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_loopback() || v4.is_unspecified())
        }
    };
    if unsuitable {
        return Err(error(
            "setup advertise_url must use a client-reachable host, not localhost, loopback or a wildcard address",
        ));
    }
    Ok(url)
}
