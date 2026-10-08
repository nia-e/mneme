//! Catalog-derived snapshot routes. Transport sessions stay with the caller.
use crate::{Endpoint, LibraryError, LibraryRuntime, err, failure_text, verify_database_row};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// An exact retained snapshot reference, including its source device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaPin {
    pub source_device_id: String,
    pub generation: String,
}

/// A snapshot route checked against the current local catalog, not a connection
/// or proof that the remote service still serves it. Construction is private so
/// callers cannot accidentally substitute a moving alias or generation path.
#[derive(Clone, Debug)]
pub struct ReplicaRoute {
    project_id: String,
    endpoint: Endpoint,
    expected_db_id: String,
    database: String,
    resolved_path: PathBuf,
    source_device_id: String,
    generation: String,
    captured_at: u64,
}

impl ReplicaRoute {
    pub fn project_id(&self) -> &str {
        &self.project_id
    }
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
    pub fn expected_db_id(&self) -> &str {
        &self.expected_db_id
    }
    pub fn database(&self) -> &str {
        &self.database
    }
    pub fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }
    pub fn source_device_id(&self) -> &str {
        &self.source_device_id
    }
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub fn captured_at(&self) -> u64 {
        self.captured_at
    }

    /// Check native `databases` rows and return the literal per-generation alias.
    /// Fetch these rows and perform the subsequent read on the SAME connection;
    /// this check does not permit reconnecting or replaying against a new session.
    /// A stable db_id alone does not identify a snapshot edition.
    pub fn verify_databases(&self, databases: &Value) -> Result<String, LibraryError> {
        let rows = databases
            .as_array()
            .ok_or_else(|| err("invalid database catalog"))?;
        // Native db aliases must be literal, unique names. `name` is not a
        // substitute for the advertised canonical `db` on this raw-read route.
        let mut matching = rows.iter().filter(|row| row["db"] == self.database);
        let item = matching.next().ok_or_else(|| {
            err(format!(
                "snapshot alias {:?} is not advertised",
                self.database
            ))
        })?;
        if matching.next().is_some() {
            return Err(err(format!(
                "snapshot alias {:?} must be advertised exactly once",
                self.database
            )));
        }
        verify_database_row(
            item,
            &self.database,
            &self.expected_db_id,
            Some(&self.resolved_path),
        )
        .map_err(|failure| err(failure_text(failure)))
    }
}

impl LibraryRuntime {
    /// Resolve only configured replica endpoints, never a live owner fallback.
    /// Re-read the published catalog on every call. An unpinned call selects the
    /// newest configured replica by (captured_at, generation); a pin must match
    /// that exact retained device/generation or fail rather than silently move.
    /// Raw routes require `<prefix>_<generation>` native aliases and an absolute
    /// path containing a literal generation directory, not current/previous.
    /// The serving operator must never reassign that alias or path to a different
    /// generation; catalog naming is not itself remote immutability enforcement.
    pub fn resolve_replica(
        &self,
        project_id: &str,
        pin: Option<&ReplicaPin>,
    ) -> Result<ReplicaRoute, LibraryError> {
        if project_id.is_empty()
            || pin.is_some_and(|pin| pin.source_device_id.is_empty() || pin.generation.is_empty())
        {
            return Err(err("invalid replica project or exact snapshot reference"));
        }
        let catalog = self.read_catalog()?;
        let entry = catalog
            .entries
            .iter()
            .find(|entry| entry.project_id == project_id && !entry.withdrawn)
            .ok_or_else(|| err("project is not enrolled or is withdrawn"))?;
        let replica = match pin {
            Some(pin) => {
                let mut matching = entry.replicas.iter().filter(|replica| {
                    replica.source_device_id == pin.source_device_id
                        && replica.generation == pin.generation
                });
                let replica = matching.next().ok_or_else(|| {
                    err("snapshot generation expired; resolve the project again for a new reference")
                })?;
                replica
            }
            None => entry
                .replicas
                .iter()
                .filter(|replica| self.config.replicas.contains_key(&replica.source_device_id))
                .max_by_key(|replica| (replica.captured_at, replica.generation.as_str()))
                .ok_or_else(|| err("no configured replica for this project"))?,
        };
        // The emitted pin must resolve uniquely on the next request, including
        // when this request initially selected the newest configured edition.
        if entry
            .replicas
            .iter()
            .filter(|candidate| {
                candidate.source_device_id == replica.source_device_id
                    && candidate.generation == replica.generation
            })
            .nth(1)
            .is_some()
        {
            return Err(err("ambiguous snapshot reference in library catalog"));
        }
        let endpoint = self
            .config
            .replicas
            .get(&replica.source_device_id)
            .ok_or_else(|| err("replica endpoint is not configured"))?;
        validate_snapshot_route(
            &replica.database,
            &replica.resolved_path,
            &replica.generation,
        )?;
        Ok(ReplicaRoute {
            project_id: entry.project_id.clone(),
            endpoint: endpoint.clone(),
            expected_db_id: entry.db_id.clone(),
            database: replica.database.clone(),
            resolved_path: replica.resolved_path.clone(),
            source_device_id: replica.source_device_id.clone(),
            generation: replica.generation.clone(),
            captured_at: replica.captured_at,
        })
    }
}

fn moving_component(value: &str) -> bool {
    value.eq_ignore_ascii_case("current") || value.eq_ignore_ascii_case("previous")
}

fn validate_snapshot_route(alias: &str, path: &Path, generation: &str) -> Result<(), LibraryError> {
    let suffix = format!("_{generation}");
    if generation.is_empty()
        || !generation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || moving_component(generation)
        || !alias.ends_with(&suffix)
        || alias.len() <= suffix.len()
        || !alias
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || alias.split(['_', '-']).any(moving_component)
    {
        return Err(err(
            "snapshot route requires a literal per-generation native alias <prefix>_<generation>",
        ));
    }
    if !path.is_absolute() || path.to_str().is_none() {
        return Err(err(
            "snapshot route requires an absolute UTF-8 generation serving path",
        ));
    }
    let mut generation_directory = false;
    for component in path.parent().into_iter().flat_map(Path::components) {
        match component {
            Component::Normal(value) => {
                let Some(value) = value.to_str() else {
                    return Err(err("invalid snapshot generation path"));
                };
                if moving_component(value) {
                    return Err(err("snapshot serving path cannot use current or previous"));
                }
                generation_directory |= value == generation;
            }
            Component::ParentDir | Component::CurDir => {
                return Err(err(
                    "snapshot serving path must be literal, without traversal",
                ));
            }
            _ => {}
        }
    }
    if !generation_directory {
        return Err(err(
            "snapshot serving path must contain its literal generation directory",
        ));
    }
    Ok(())
}
