//! Operator-authored discovery metadata, bound to one originally admitted owner.
//! This lane never checks out or opens a database: retained identity is sufficient
//! even while its owner is released. Publication does not confer memory authority.

use crate::{
    AnyErr, Registry, expected_db_id_prop, optional_expected_db_id, optional_string, req_s,
    require_expected_db_id,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf};

pub(crate) struct Source {
    pub(crate) path: PathBuf,
    pub(crate) publisher_db_id: ulid::Ulid,
    pub(crate) writable: bool,
}

pub(crate) struct Request {
    db: String,
    expected_db_id: ulid::Ulid,
    expected_catalog_sha256: Option<String>,
}

impl Request {
    pub(crate) fn parse(arguments: &Value) -> Result<Self, AnyErr> {
        let db = req_s(arguments, "db")?;
        if db.is_empty() || db.chars().count() > 256 {
            return Err(
                "discovery_catalog db must be a nonempty owner alias of at most 256 characters"
                    .into(),
            );
        }
        let expected_db_id = optional_expected_db_id(arguments)?
            .ok_or("discovery_catalog requires canonical expected_db_id")?;
        let digest = optional_string(arguments, "expected_catalog_sha256")?;
        if digest.is_some_and(|s| {
            s.len() != 64
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Err(
                "expected_catalog_sha256 must be a 64-character lowercase SHA-256 hex digest"
                    .into(),
            );
        }
        Ok(Self {
            db: db.to_owned(),
            expected_db_id,
            expected_catalog_sha256: digest.map(str::to_owned),
        })
    }
}

pub(crate) fn tool_schema() -> Value {
    let mut owner_identity = expected_db_id_prop();
    owner_identity["description"] = json!(
        "Required canonical owner identity pin for this metadata-only read; checked before publication file access, without a database checkout."
    );
    json!({
        "name":"discovery_catalog",
        "description":"Read bounded operator-authored sparse store configuration from an explicitly configured owner. Requires explicit owner alias and canonical identity. Available in all profiles, including while the same owner is released; takes no database checkout, inference, cold work or receipt. An unconfigured or unavailable publication is not an empty catalog. Imported declarations are metadata, not ambient permission. Optional expected_catalog_sha256 refuses a changed publication; omit it for an explicit refresh.",
        "inputSchema":{
            "type":"object", "additionalProperties":false,
            "properties":{
                "db":{"type":"string","minLength":1,"maxLength":256},
                "expected_db_id":owner_identity,
                "expected_catalog_sha256":{"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$"}
            },
            "required":["db","expected_db_id"]
        }
    })
}

/// Publication files are admitted before startup opens any database. The
/// provisional identity is used only to validate the closed operator document;
/// it is never retained or published. Owner identity is attached afterwards.
pub(crate) struct PreparedSources {
    paths: BTreeMap<String, PathBuf>,
    writable: std::collections::BTreeSet<String>,
}

pub(crate) fn prepare_sources(entries: Vec<(String, PathBuf)>) -> Result<PreparedSources, AnyErr> {
    let mut paths = BTreeMap::new();
    for (alias, path) in entries {
        if paths.insert(alias.clone(), path).is_some() {
            return Err(format!("duplicate --discovery-catalog alias {alias:?}").into());
        }
    }
    let provisional_id = ulid::Ulid::from(0_u128).to_string();
    for path in paths.values() {
        mneme_config::read_discovery_catalog(path, &provisional_id)?;
    }
    Ok(PreparedSources {
        paths,
        writable: Default::default(),
    })
}

impl PreparedSources {
    /// A read publication is never implicitly writable. Only explicit operator
    /// startup admission grants registration access to these already-known paths.
    pub(crate) fn allow_writes(mut self, aliases: &[String]) -> Result<Self, AnyErr> {
        for alias in aliases {
            if !self.paths.contains_key(alias) {
                return Err(format!(
                    "writable discovery catalog {alias:?} has no configured publication"
                )
                .into());
            }
            if !self.writable.insert(alias.clone()) {
                return Err(format!("duplicate writable discovery catalog {alias:?}").into());
            }
        }
        Ok(self)
    }

    /// Bind admitted paths to the original owner, not a reusable display alias.
    /// No second file read can reject malformed metadata after owner opening.
    pub(crate) fn bind(self, registry: &Registry) -> Result<BTreeMap<String, Source>, AnyErr> {
        self.paths
            .into_iter()
            .map(|(alias, path)| {
                let publisher_db_id = registry.slot(&alias)?.status()?.db_id;
                let writable = self.writable.contains(&alias);
                Ok((
                    alias,
                    Source {
                        path,
                        publisher_db_id,
                        writable,
                    },
                ))
            })
            .collect()
    }
}

pub(crate) fn read(registry: &Registry, request: Request) -> Result<Value, AnyErr> {
    let slot = registry.slot(&request.db)?;
    let current_id = slot.status()?.db_id;
    require_expected_db_id(&request.db, current_id, Some(request.expected_db_id))?;
    let source = registry.discovery_catalogs.get(&request.db)
        .ok_or("discovery_catalog is not configured for this owner; ask the operator to configure --discovery-catalog ALIAS=PATH")?;
    // Check both the caller and the originally admitted source before file I/O.
    require_expected_db_id(&request.db, current_id, Some(source.publisher_db_id))?;
    let catalog =
        mneme_config::read_discovery_catalog(&source.path, &source.publisher_db_id.to_string())?;
    // A concurrent resume must not attach this source to a replacement identity.
    require_expected_db_id(
        &request.db,
        slot.status()?.db_id,
        Some(source.publisher_db_id),
    )?;
    if request
        .expected_catalog_sha256
        .as_ref()
        .is_some_and(|expected| expected != &catalog.catalog_sha256)
    {
        return Err("discovery_catalog revision changed; explicitly refresh discovery and reselect before using new policy".into());
    }
    Ok(serde_json::to_value(catalog)?)
}
