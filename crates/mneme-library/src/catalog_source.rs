//! A configured catalog source keeps routing facts and emitted provenance together.
//!
//! This is local metadata, not proof of remote identity or snapshot immutability.
//! Reads still verify the advertised database, and raw `ReplicaRoute` callers keep
//! their stricter per-generation alias/path and same-connection requirements.
use crate::{
    Endpoint, Entry, LibraryConfig, LibraryError, LibraryGet, NativeOwnerRoute, Replica, err,
    owner_endpoint,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

pub(super) enum CatalogSource<'a> {
    Live {
        entry: &'a Entry,
        endpoint: &'a Endpoint,
        expected_path: Option<&'a Path>,
    },
    Snapshot {
        entry: &'a Entry,
        endpoint: &'a Endpoint,
        replica: &'a Replica,
    },
}

impl<'a> CatalogSource<'a> {
    pub(super) fn live(
        config: &'a LibraryConfig,
        native_owners: &'a BTreeMap<String, NativeOwnerRoute>,
        entry: &'a Entry,
    ) -> Option<Self> {
        let native_owner = native_owners.get(&entry.db_id);
        let endpoint = native_owner
            .map(|owner| &owner.endpoint)
            .or_else(|| owner_endpoint(config, entry))?;
        Some(Self::Live {
            entry,
            endpoint,
            expected_path: native_owner.and_then(|owner| owner.expected_path.as_deref()),
        })
    }

    pub(super) fn exact(
        config: &'a LibraryConfig,
        native_owners: &'a BTreeMap<String, NativeOwnerRoute>,
        entry: &'a Entry,
        reference: &LibraryGet,
    ) -> Result<Self, LibraryError> {
        if reference.source_device_id == entry.owner_device_id && reference.generation.is_none() {
            return Self::live(config, native_owners, entry).ok_or_else(|| {
                err("live route not configured for this project; provision owner_routes or query a snapshot")
            });
        }
        // A device can serve both the live owner and retained snapshots. The
        // explicit generation, not the device alone, distinguishes those routes.
        let generation = reference
            .generation
            .as_deref()
            .ok_or_else(|| err("replica reference requires generation"))?;
        let replica = entry
            .replicas
            .iter()
            .find(|replica| {
                replica.source_device_id == reference.source_device_id
                    && replica.generation == generation
            })
            .ok_or_else(|| err("snapshot generation expired; run library_query again"))?;
        let endpoint = config
            .replicas
            .get(&replica.source_device_id)
            .ok_or_else(|| err("replica endpoint is not configured"))?;
        Ok(Self::Snapshot {
            entry,
            endpoint,
            replica,
        })
    }

    pub(super) fn newest_snapshot(config: &'a LibraryConfig, entry: &'a Entry) -> Option<Self> {
        let (replica, endpoint) = newest_configured_replica(config, entry)?;
        Some(Self::Snapshot {
            entry,
            endpoint,
            replica,
        })
    }

    pub(super) fn endpoint(&self) -> &Endpoint {
        match self {
            Self::Live { endpoint, .. } | Self::Snapshot { endpoint, .. } => endpoint,
        }
    }

    pub(super) fn database(&self) -> &str {
        match self {
            Self::Live { entry, .. } => &entry.database,
            Self::Snapshot { replica, .. } => &replica.database,
        }
    }

    pub(super) fn expected_db_id(&self) -> &str {
        match self {
            Self::Live { entry, .. } | Self::Snapshot { entry, .. } => &entry.db_id,
        }
    }

    pub(super) fn expected_path(&self) -> Option<&Path> {
        match self {
            Self::Live { expected_path, .. } => *expected_path,
            Self::Snapshot { replica, .. } => Some(&replica.resolved_path),
        }
    }

    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::Live { .. } => "live",
            Self::Snapshot { .. } => "snapshot",
        }
    }

    pub(super) fn provenance(&self) -> Value {
        match self {
            Self::Live { entry, .. } => json!({
                "device_id": entry.owner_device_id,
                "kind": self.kind(),
                "generation": null,
                "captured_at": null,
            }),
            Self::Snapshot { replica, .. } => json!({
                "device_id": replica.source_device_id,
                "kind": self.kind(),
                "generation": replica.generation,
                "captured_at": replica.captured_at,
            }),
        }
    }
}

/// Generation breaks capture-time ties; the last eligible catalog entry wins exact ties.
/// This selects metadata only. It does not apply `ReplicaRoute`'s raw-read guard.
pub(super) fn newest_configured_replica<'a>(
    config: &'a LibraryConfig,
    entry: &'a Entry,
) -> Option<(&'a Replica, &'a Endpoint)> {
    entry
        .replicas
        .iter()
        .filter_map(|replica| {
            config
                .replicas
                .get(&replica.source_device_id)
                .map(|endpoint| (replica, endpoint))
        })
        .max_by_key(|(replica, _)| (replica.captured_at, replica.generation.as_str()))
}
