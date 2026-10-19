//! Pinned client metadata, not a memory-store generation or remote authority.
use crate::{
    DiscoveryGraph, PublishedCatalog, Result, declarative_file, error, sha256, validate_path,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DISCOVERY_RECEIPT_SCHEMA: &str = "mneme.discovery-receipt.v1";
pub const MAX_DISCOVERY_RECEIPT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryReceipt {
    pub schema: String,
    pub boundary_sha256: String,
    pub graph: DiscoveryGraph,
}
impl DiscoveryReceipt {
    pub fn new(boundary_sha256: String, graph: DiscoveryGraph) -> Result<Self> {
        let value = Self {
            schema: DISCOVERY_RECEIPT_SCHEMA.into(),
            boundary_sha256,
            graph,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_DISCOVERY_RECEIPT_BYTES {
            return Err(error("discovery receipt exceeds byte bound"));
        }
        let unique = crate::legacy_workspace::unique_json(bytes)
            .map_err(|_| error("invalid discovery receipt; refresh native configuration"))?;
        let value: Self = serde_json::from_value(unique)
            .map_err(|_| error("invalid discovery receipt; refresh native configuration"))?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != DISCOVERY_RECEIPT_SCHEMA {
            return Err(error("unsupported discovery receipt schema"));
        }
        validate_hash(&self.boundary_sha256)?;
        self.graph.validate()?;
        if serde_json::to_vec(self)
            .map_err(|_| error("cannot encode discovery receipt"))?
            .len()
            > MAX_DISCOVERY_RECEIPT_BYTES
        {
            return Err(error("discovery receipt exceeds byte bound"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryBinding {
    pub path: PathBuf,
    pub sha256: String,
}
impl DiscoveryBinding {
    /// Load exactly the pinned bytes. A different valid receipt still invalidates
    /// the old selection: revalidation never refreshes or expands a grant.
    pub fn load(&self) -> Result<DiscoveryReceipt> {
        validate_path(&self.path)?;
        validate_hash(&self.sha256)?;
        let (receipt, current) = read_discovery_receipt(&self.path)?.ok_or_else(|| {
            error("pinned discovery receipt is missing; reselect native configuration")
        })?;
        if current != *self {
            return Err(error(
                "pinned discovery receipt changed; reselect native configuration",
            ));
        }
        Ok(receipt)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryWitness {
    pub binding: DiscoveryBinding,
    pub boundary_sha256: String,
}
fn validate_hash(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(error("invalid discovery SHA-256 witness"));
    }
    Ok(())
}
/// Read bounded operator-authored publication metadata without opening an owner.
pub fn read_discovery_catalog(path: &Path, publisher_db_id: &str) -> Result<PublishedCatalog> {
    let source = declarative_file::read_bounded(path, crate::MAX_CATALOG_BYTES)?
        .ok_or_else(|| error("discovery catalog file is missing"))?;
    let text =
        std::str::from_utf8(&source.bytes).map_err(|_| error("discovery catalog must be UTF-8"))?;
    PublishedCatalog::parse_toml(text, publisher_db_id)
}

/// Shared guarded reader for the client cache and exact replay. Receipt leaves
/// must be regular files, not configuration-style symlink indirections.
pub fn read_discovery_receipt(path: &Path) -> Result<Option<(DiscoveryReceipt, DiscoveryBinding)>> {
    validate_path(path)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(error(
                "discovery receipt must be a regular non-symlink file",
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(error("cannot inspect discovery receipt")),
    }
    validate_receipt_ownership(path)?;
    let Some(source) = declarative_file::read_bounded(path, MAX_DISCOVERY_RECEIPT_BYTES)? else {
        return Ok(None);
    };
    validate_receipt_ownership(&source.path)?;
    let receipt = DiscoveryReceipt::parse(&source.bytes)?;
    let binding = DiscoveryBinding {
        path: source.path,
        sha256: sha256(&source.bytes),
    };
    Ok(Some((receipt, binding)))
}

fn validate_receipt_ownership(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // These are native client metadata, not operator config symlinks. Reject
        // writable-by-others receipts and their containing cache directory.
        let uid = unsafe { libc::geteuid() };
        for entry in [
            path,
            path.parent()
                .ok_or_else(|| error("receipt has no parent"))?,
        ] {
            let metadata = std::fs::symlink_metadata(entry)
                .map_err(|_| error("cannot inspect discovery receipt ownership"))?;
            if metadata.file_type().is_symlink()
                || metadata.uid() != uid
                || metadata.mode() & 0o022 != 0
            {
                return Err(error(
                    "discovery receipt and parent must be owned by this user and not writable by others",
                ));
            }
        }
    }
    Ok(())
}
