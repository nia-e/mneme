//! Checked access to configured native owners and retained read-only editions.
//! No store opener, service launcher or mutation replay lives here.
mod admission;
use mneme_config::{ActiveStore, SelectionSnapshot};
use mneme_library::{Endpoint, LibraryRuntime, ReplicaPin, ReplicaRoute};
use mneme_mcp_client::{
    ClientTimeouts, ConnectionOptions, ReceiveBudget, RemoteClient, is_availability,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::{sync::Mutex, time::Instant};

pub type AnyErr = Box<dyn std::error::Error + Send + Sync>;
pub const MAX_REQUEST_BYTES: usize = 128 * 1024;
pub const ORDINARY_TOOLS: &[&str] = &[
    "databases",
    "status",
    "core",
    "query",
    "recall",
    "recall_context",
    "get",
    "list",
    "neighbors",
    "graph",
    "activity",
    "save",
    "capture",
    "retag",
    "edit_body",
    "edit_summary",
    "episode",
    "link",
    "supersede",
    "forget",
    "concern",
    "walk",
    "reflect",
    "remote_edges",
    "contradictions",
    "merges",
    "ingest",
    "contradict",
    "reconcile",
    "feedback",
    "merge",
    "decay",
    "prune",
    "snapshot_create",
];

/// Caller-owned allowance. Every transport attempt/page uses the same receive
/// counter and deadline; framing, catalog and failed responses are not free.
#[derive(Debug)]
pub struct OperationBudget {
    receive: ReceiveBudget,
    started: Instant,
    deadline: Instant,
}
impl OperationBudget {
    pub fn new(bytes: usize, duration: Duration) -> Result<Self, AnyErr> {
        if bytes == 0
            || bytes > 16 * 1024 * 1024
            || duration.is_zero()
            || duration > Duration::from_secs(30)
        {
            return Err("operation budget needs 1..=16MiB and 0..=30 seconds".into());
        }
        let started = Instant::now();
        Ok(Self {
            receive: ReceiveBudget::new(bytes),
            started,
            deadline: started + duration,
        })
    }
    pub fn bytes_used(&self) -> usize {
        self.receive.used()
    }
    pub fn bytes_remaining(&self) -> usize {
        self.receive.remaining()
    }
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
    fn timeouts(&self) -> Result<ClientTimeouts, AnyErr> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || self.bytes_remaining() == 0 {
            return Err("shared operation budget exhausted".into());
        }
        Ok(ClientTimeouts {
            connect: remaining,
            request: remaining,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestAccess {
    Read,
    Write,
}
/// Closed authority classification; unknown tools/actions never inherit reads.
pub fn request_access(tool: &str, args: &Value) -> Result<RequestAccess, AnyErr> {
    if !args.is_object() {
        return Err("tool arguments must be an object".into());
    }
    use RequestAccess::*;
    Ok(match tool {
        "databases" | "status" | "core" | "query" | "recall" | "recall_context" | "get"
        | "list" | "neighbors" | "activity" | "remote_edges" | "contradictions" | "merges" => Read,
        "graph" => match args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("topology")
        {
            "topology" | "summaries" => Read,
            _ => return Err("unsupported graph action".into()),
        },
        "walk" => match args.get("action").and_then(Value::as_str) {
            Some("start" | "look" | "edges" | "body" | "go" | "back" | "done" | "abort") => Read,
            _ => return Err("unsupported walk action".into()),
        },
        "episode" => match args.get("action").and_then(Value::as_str) {
            Some("list" | "search" | "get" | "history" | "references") => Read,
            Some("append" | "revise") => Write,
            _ => return Err("unsupported episode action".into()),
        },
        "concern" => match args.get("action").and_then(Value::as_str) {
            Some("list") => Read,
            Some("notice" | "record_finding") => Write,
            _ => return Err("unsupported concern action".into()),
        },
        "save" | "capture" | "retag" | "edit_body" | "edit_summary" | "link" | "supersede"
        | "forget" | "reflect" | "ingest" | "contradict" | "reconcile" | "feedback" | "merge"
        | "decay" | "prune" | "snapshot_create" => Write,
        _ => return Err("unknown configured tool; no owner was contacted".into()),
    })
}
fn required_profile(tool: &str, args: &Value, access: RequestAccess) -> u8 {
    if access == RequestAccess::Read {
        return 0;
    }
    if tool == "reflect" {
        return 1;
    }
    if matches!(
        tool,
        "edit_body"
            | "edit_summary"
            | "supersede"
            | "forget"
            | "reconcile"
            | "feedback"
            | "merge"
            | "decay"
            | "prune"
            | "snapshot_create"
    ) || (tool == "episode" && args["action"] == "revise")
        || args
            .get("tags")
            .and_then(Value::as_array)
            .is_some_and(|tags| tags.iter().any(|tag| tag == "core"))
    {
        3
    } else {
        2
    }
}
pub fn native_profile(client: &RemoteClient, replica: bool) -> Result<u8, AnyErr> {
    if client.server_name() != "mneme-mcp" {
        return Err("upstream is not a native mneme-mcp owner".into());
    }
    let profile = match client
        .initialize_result()
        .get("capabilityProfile")
        .and_then(Value::as_str)
    {
        Some("read-only") => 0,
        Some("receipt-grounded") => 1,
        Some("curator") => 2,
        Some("operator") => 3,
        _ => return Err("upstream native capability profile missing or unsupported".into()),
    };
    if replica && profile != 0 {
        return Err("cache endpoint must use the native read-only profile".into());
    }
    Ok(profile)
}
/// Native alias must be unique, open and pinned to the canonical identity.
/// An optional path is extra owner-side evidence, never a local open target.
pub fn verify_owner_databases(
    rows: &Value,
    db: &str,
    expected: &str,
    path: Option<&Path>,
) -> Result<Value, AnyErr> {
    let rows = rows.as_array().ok_or("invalid upstream database catalog")?;
    let mut matching = rows.iter().filter(|row| row["db"] == db);
    let row = matching
        .next()
        .ok_or("configured owner alias is not advertised")?;
    if matching.next().is_some() || row["db_id"] != expected || row["state"] != "open" {
        return Err("configured owner identity/state mismatch; no operation was sent".into());
    }
    if let Some(path) = path {
        if row["resolved_path"].as_str().map(Path::new) != Some(path) {
            return Err("configured owner path mismatch; no operation was sent".into());
        }
    }
    Ok(row.clone())
}

/// Exact returned cache witness. Alias/path are checked as well as the pin: a
/// catalog edit cannot turn an old continuation into today's edition.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReadSource {
    pub owner_db_id: String,
    pub source_device_id: String,
    pub generation: String,
    pub captured_at: u64,
    pub database: String,
    pub resolved_path: std::path::PathBuf,
}
impl ReadSource {
    fn from_route(route: &ReplicaRoute) -> Self {
        Self {
            owner_db_id: route.expected_db_id().into(),
            source_device_id: route.source_device_id().into(),
            generation: route.generation().into(),
            captured_at: route.captured_at(),
            database: route.database().into(),
            resolved_path: route.resolved_path().into(),
        }
    }
    fn pin(&self) -> ReplicaPin {
        ReplicaPin {
            source_device_id: self.source_device_id.clone(),
            generation: self.generation.clone(),
        }
    }
}

pub struct ConfiguredClient {
    snapshot: SelectionSnapshot,
    active: BTreeMap<String, ActiveStore>,
    sessions: Mutex<BTreeMap<String, RemoteClient>>,
    route_locks: BTreeMap<String, Mutex<()>>,
}
impl ConfiguredClient {
    /// Metadata-only construction. Optional owners and catalogs remain lazy.
    pub fn new(snapshot: SelectionSnapshot) -> Result<Self, AnyErr> {
        snapshot.verify_digest()?;
        let mut active = BTreeMap::new();
        for selected in &snapshot.active {
            if active
                .insert(selected.name.clone(), selected.clone())
                .is_some()
            {
                return Err("duplicate active store binding".into());
            }
        }
        let route_locks = active
            .keys()
            .map(|name| (name.clone(), Mutex::new(())))
            .collect();
        Ok(Self {
            snapshot,
            active,
            sessions: Mutex::new(BTreeMap::new()),
            route_locks,
        })
    }
    /// Deliberate inventory access does not widen automatic activation.
    pub fn for_explicit(snapshot: SelectionSnapshot, name: &str) -> Result<Self, AnyErr> {
        snapshot.revalidate_exact()?;
        let selected = snapshot.explicit_store(name)?;
        let mut client = Self::new(snapshot)?;
        client
            .route_locks
            .entry(name.into())
            .or_insert_with(|| Mutex::new(()));
        client.active.insert(name.into(), selected);
        Ok(client)
    }
    pub fn selection(&self) -> &SelectionSnapshot {
        &self.snapshot
    }
    pub fn selected(&self, name: &str) -> Result<&ActiveStore, AnyErr> {
        self.snapshot.revalidate_exact()?;
        self.active.get(name).ok_or_else(|| "store is inactive or outside the selected workspace boundary; no owner was contacted".into())
    }
    pub fn databases(&self) -> Value {
        Value::Array(self.active.values().map(|selected| json!({"db":selected.name,"db_id":selected.store.id,"state":"configured","read":selected.eligible.read,"write":selected.eligible.write,"default_read":self.snapshot.read_default.as_deref()==Some(selected.name.as_str())})).collect())
    }
    pub fn library(&self, name: &str) -> Result<LibraryRuntime, AnyErr> {
        self.snapshot.revalidate_exact()?;
        let library = self
            .snapshot
            .libraries
            .get(name)
            .ok_or("unknown configured library")?;
        let replicas = library
            .replicas
            .iter()
            .map(|(name, endpoint)| {
                (
                    name.clone(),
                    Endpoint {
                        url: endpoint.url.clone(),
                        ssh_mcp_port: endpoint.ssh_mcp_port.unwrap_or(18766),
                        token_env: endpoint.token_env.clone(),
                    },
                )
            })
            .collect();
        let read_authority = self
            .snapshot
            .stores
            .iter()
            .filter(|store| store.admitted && store.eligible.read)
            .map(|store| store.store.id.clone())
            .collect();
        let native_owners = self
            .snapshot
            .stores
            .iter()
            .filter(|store| store.admitted && store.eligible.read)
            .map(|selected| {
                let owner = &selected.store.owner;
                (
                    selected.store.id.clone(),
                    mneme_library::NativeOwnerRoute {
                        endpoint: Endpoint {
                            url: owner.url.clone(),
                            ssh_mcp_port: owner.ssh_mcp_port.unwrap_or(18766),
                            token_env: owner.token_env.clone(),
                        },
                        database: owner.db.clone(),
                        expected_path: owner.path.clone(),
                    },
                )
            })
            .collect();
        let snapshot = self.snapshot.clone();
        Ok(
            LibraryRuntime::from_catalog(library.catalog.clone(), replicas)?
                .with_read_authority(read_authority)
                .with_admission_guard(std::sync::Arc::new(move || {
                    snapshot
                        .revalidate_exact()
                        .map_err(|error| mneme_library::LibraryError(error.to_string()))
                }))
                .with_native_owners(native_owners)?,
        )
    }
    fn cache_route(
        &self,
        selected: &ActiveStore,
        source: Option<&ReadSource>,
    ) -> Result<ReplicaRoute, AnyErr> {
        let cache = selected
            .store
            .cache
            .as_ref()
            .ok_or("canonical owner unavailable and no cache is configured")?;
        let library = self.library(&cache.library)?;
        let route = library
            .resolve_replica_by_db_id(&selected.store.id, source.map(ReadSource::pin).as_ref())?;
        if route.expected_db_id() != selected.store.id {
            return Err("cache native owner identity mismatch".into());
        }
        if let Some(source) = source {
            if source != &ReadSource::from_route(&route) {
                return Err(
                    "exact cache reference changed or expired; reselect deliberately".into(),
                );
            }
        }
        Ok(route)
    }
    async fn connect(
        &self,
        selected: &ActiveStore,
        replica: Option<&ReplicaRoute>,
        budget: &OperationBudget,
    ) -> Result<(RemoteClient, Value), AnyErr> {
        let options = match replica {
            None => ConnectionOptions {
                url: selected.store.owner.url.clone(),
                token_env: selected.store.owner.token_env.clone(),
                ssh_mcp_port: selected.store.owner.ssh_mcp_port.unwrap_or(18766),
            },
            Some(route) => ConnectionOptions {
                url: route.endpoint().url.clone(),
                token_env: route.endpoint().token_env.clone(),
                ssh_mcp_port: route.endpoint().ssh_mcp_port,
            },
        };
        let key = session_key(selected, replica);
        let retained = self.sessions.lock().await.remove(&key);
        let mut client = match retained {
            Some(mut client) => {
                client.set_operation_budget(budget.receive.clone(), budget.timeouts()?.request)?;
                client
            }
            None => {
                RemoteClient::connect_native_reads_with_budget(
                    &options,
                    budget.timeouts()?,
                    budget.receive.clone(),
                )
                .await?
            }
        };
        let checked = async {
            native_profile(&client, replica.is_some())?;
            let rows = tokio::time::timeout_at(
                budget.deadline,
                client.call_native_read("databases", json!({})),
            )
            .await
            .map_err(|_| "shared operation deadline exhausted")??;
            if let Some(route) = replica {
                route.verify_databases(&rows)?;
                Ok(rows
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["db"] == route.database())
                    .unwrap()
                    .clone())
            } else {
                verify_owner_databases(
                    &rows,
                    &selected.store.owner.db,
                    &selected.store.id,
                    selected.store.owner.path.as_deref(),
                )
            }
        }
        .await;
        match checked {
            Ok(row) => Ok((client, row)),
            Err(error) => {
                client.close().await;
                Err(error)
            }
        }
    }
    pub async fn catalog(&self, store: &str) -> Result<Vec<Value>, AnyErr> {
        let mut budget = OperationBudget::new(512 * 1024, Duration::from_secs(10))?;
        self.catalog_with_budget(store, &mut budget).await
    }
    pub async fn catalog_with_budget(
        &self,
        store: &str,
        budget: &mut OperationBudget,
    ) -> Result<Vec<Value>, AnyErr> {
        self.catalog_mode(store, budget, false).await
    }
    pub async fn catalog_owner_with_budget(
        &self,
        store: &str,
        budget: &mut OperationBudget,
    ) -> Result<Vec<Value>, AnyErr> {
        self.catalog_mode(store, budget, true).await
    }
    async fn catalog_mode(
        &self,
        store: &str,
        budget: &mut OperationBudget,
        canonical_only: bool,
    ) -> Result<Vec<Value>, AnyErr> {
        let selected = self.selected(store)?;
        let _route_guard = tokio::time::timeout_at(budget.deadline, self.route_locks[store].lock())
            .await
            .map_err(|_| "route busy; shared operation deadline exhausted")?;
        // Identity/catalog metadata is necessary for a write-only destination;
        // it grants no access to authored payloads.
        if !selected.eligible.read && !selected.eligible.write {
            return Err("configured read permission denied; no owner was contacted".into());
        }
        let mut replica = None;
        let (mut client, _) = match self.connect(selected, None, budget).await {
            Ok(client) => client,
            Err(error)
                if !canonical_only
                    && is_availability(error.as_ref())
                    && selected.eligible.read
                    && selected.store.cache.is_some() =>
            {
                replica = Some(self.cache_route(selected, None)?);
                self.connect(selected, replica.as_ref(), budget).await?
            }
            Err(error) => return Err(error),
        };
        tokio::time::timeout_at(budget.deadline, client.ensure_catalog())
            .await
            .map_err(|_| "shared catalog deadline exhausted")??;
        let profile = native_profile(&client, false)?;
        let mut tools = Vec::new();
        for tool in client.tool_catalog() {
            let Some(name) = tool["name"].as_str() else {
                continue;
            };
            if !ORDINARY_TOOLS.contains(&name) || name == "databases" {
                continue;
            }
            let mut tool = tool.clone();
            if matches!(name, "episode" | "concern" | "walk") {
                let allowed: Vec<Value> = tool
                    .pointer("/inputSchema/properties/action/enum")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|action| {
                        let args = json!({"action":action});
                        request_access(name, &args).is_ok_and(|access| {
                            ((access == RequestAccess::Read && selected.eligible.read)
                                || (access == RequestAccess::Write && selected.eligible.write))
                                && required_profile(name, &args, access) <= profile
                        })
                    })
                    .cloned()
                    .collect();
                if allowed.is_empty() {
                    continue;
                }
                tool["inputSchema"]["properties"]["action"]["enum"] = json!(allowed);
                if let Some(branches) = tool["inputSchema"]["oneOf"].as_array_mut() {
                    branches.retain(|branch| {
                        allowed.contains(&branch["properties"]["action"]["const"])
                    });
                }
            } else {
                let args = if name == "graph" {
                    json!({"action":"topology"})
                } else {
                    json!({})
                };
                let access = request_access(name, &args)?;
                if (access == RequestAccess::Write && !selected.eligible.write)
                    || (access == RequestAccess::Read && !selected.eligible.read)
                    || required_profile(name, &args, access) > profile
                {
                    continue;
                }
            }
            tools.push(tool);
        }
        self.sessions
            .lock()
            .await
            .insert(session_key(selected, replica.as_ref()), client);
        Ok(tools)
    }
    pub async fn call(&self, store: &str, tool: &str, args: Value) -> Result<Value, AnyErr> {
        let mut budget = OperationBudget::new(1024 * 1024, Duration::from_secs(15))?;
        self.call_with_budget(store, tool, args, &mut budget).await
    }
    pub async fn call_with_budget(
        &self,
        store: &str,
        tool: &str,
        args: Value,
        budget: &mut OperationBudget,
    ) -> Result<Value, AnyErr> {
        self.call_mode(store, tool, args, budget, false).await
    }
    /// Reads used to plan/guard a live write must not fall back to stale editions.
    pub async fn call_owner_with_budget(
        &self,
        store: &str,
        tool: &str,
        args: Value,
        budget: &mut OperationBudget,
    ) -> Result<Value, AnyErr> {
        self.call_mode(store, tool, args, budget, true).await
    }
    // The recursive read-only fallback edge must expose its Send contract at
    // this boundary. Boxing only that edge leaves stable rustc trying to infer
    // Send for an opaque async return type from itself.
    fn call_mode<'a>(
        &'a self,
        store: &'a str,
        tool: &'a str,
        mut args: Value,
        budget: &'a mut OperationBudget,
        canonical_only: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, AnyErr>> + Send + 'a>>
    {
        Box::pin(async move {
            let selected = self.selected(store)?;
            let _route_guard =
                tokio::time::timeout_at(budget.deadline, self.route_locks[store].lock())
                    .await
                    .map_err(|_| "route busy; shared operation deadline exhausted")?;
            let access = request_access(tool, &args)?;
            if (access == RequestAccess::Read
                && !selected.eligible.read
                && !(tool == "databases" && selected.eligible.write))
                || (access == RequestAccess::Write && !selected.eligible.write)
            {
                return Err(
                    "configured operation permission denied; no owner was contacted".into(),
                );
            }
            if serde_json::to_vec(&args)?.len() > MAX_REQUEST_BYTES {
                return Err("configured tool arguments exceed request envelope".into());
            }
            bounded_json(&args, 0)?;
            let checked = admission::prepare(tool, &args, access)?;
            let admitted_args = args.clone();
            let source: Option<ReadSource> = args
                .as_object_mut()
                .unwrap()
                .remove("_mneme_source")
                .map(serde_json::from_value)
                .transpose()?;
            if source.is_some() && (access == RequestAccess::Write || canonical_only) {
                return Err("cached read references cannot authorize live-owner writes".into());
            }
            if args.get("db").is_some_and(|db| db != store) {
                return Err("tool db does not match selected logical store".into());
            }
            if args
                .get("expected_db_id")
                .is_some_and(|id| id != selected.store.id.as_str())
            {
                return Err("expected_db_id does not match configured native owner".into());
            }
            if tool == "remote_edges" && args["resolve"] == true {
                return Err("cross-owner payload hydration is not authorized by one selected store; resolve each admitted target explicitly".into());
            }
            if tool == "link" && args.get("to_db").is_some() {
                return Err("cross-owner links are not exposed".into());
            }
            if tool == "get" && args["edges"] == true {
                return Err("routed get edges:true is unsupported; use neighbors".into());
            }
            let route = if let Some(source) = source.as_ref() {
                Some(self.cache_route(selected, Some(source))?)
            } else {
                None
            };
            let mut route = route;
            let connected = self.connect(selected, route.as_ref(), budget).await;
            let (mut client, row) = match connected {
                Ok(connected) => connected,
                Err(error)
                    if !canonical_only
                        && access == RequestAccess::Read
                        && route.is_none()
                        && is_availability(error.as_ref())
                        && selected.store.cache.is_some() =>
                {
                    if continuation(&args) {
                        return Err("cache continuation needs the exact returned source; owner unavailable, no latest-edition substitution".into());
                    }
                    route = Some(self.cache_route(selected, None)?);
                    self.connect(selected, route.as_ref(), budget).await?
                }
                Err(error) => return Err(error),
            };
            let operation: Result<Value, AnyErr> = async {
                if route.is_some() && access == RequestAccess::Write {
                    return Err("cache writes are forbidden".into());
                }
                if required_profile(tool, &args, access).max(if checked.operator() { 3 } else { 0 })
                    > native_profile(&client, route.is_some())?
                {
                    return Err(
                        "native capability profile denies operation; no operation was sent".into(),
                    );
                }

                if tool != "activity" {
                    args["db"] = json!(
                        route
                            .as_ref()
                            .map(ReplicaRoute::database)
                            .unwrap_or(&selected.store.owner.db)
                    );
                    args["expected_db_id"] = json!(selected.store.id);
                    if tool == "walk" && args["action"] != "start" {
                        args.as_object_mut().unwrap().remove("db");
                    }
                } else {
                    args.as_object_mut().unwrap().remove("db");
                    args.as_object_mut().unwrap().remove("expected_db_id");
                }
                let native_db = route
                    .as_ref()
                    .map(ReplicaRoute::database)
                    .unwrap_or(&selected.store.owner.db);
                let expected_node = args.get("id").cloned();
                let result = if tool == "databases" {
                    json!([row])
                } else {
                    tokio::time::timeout_at(budget.deadline, async {
                    if access == RequestAccess::Read {
                        client.call_native_read(tool, args).await
                    } else {
                        client.call_tool(tool, args).await
                    }
                })
                .await
                .map_err(
                    |_| "shared operation deadline exceeded; outcome may be ambiguous, no replay",
                )??
                };
                if let Some(db) = result.get("db") {
                    if db != native_db {
                        return Err("upstream returned another database alias; no replay".into());
                    }
                }
                if let Some(id) = result.get("db_id") {
                    if id != selected.store.id.as_str() {
                        return Err("upstream returned another database identity; no replay".into());
                    }
                }
                if tool == "get"
                    && (result["db"] != native_db
                        || result["db_id"] != selected.store.id
                        || result.get("id") != expected_node.as_ref())
                {
                    return Err("GET result does not match the pinned owner/node; no replay".into());
                }
                checked.verify(&result, native_db, &selected.store.id)?;
                let mut result = result;
                if tool == "activity" {
                    let events = result
                        .get_mut("events")
                        .and_then(Value::as_array_mut)
                        .ok_or("invalid owner activity events")?;
                    events.retain(|event| event["db_id"] == selected.store.id);
                }
                if let Some(route) = route.as_ref() {
                    let source = json!(ReadSource::from_route(route));
                    if let Some(object) = result.as_object_mut() {
                        if object.contains_key("_mneme_source") {
                            return Err("upstream used reserved source witness field".into());
                        }
                        object.insert("_mneme_source".into(), source);
                    } else {
                        result = json!({"result":result,"_mneme_source":source});
                    }
                }
                Ok(result)
            }
            .await;
            if operation.is_ok() {
                self.sessions
                    .lock()
                    .await
                    .insert(session_key(selected, route.as_ref()), client);
            } else {
                client.close().await;
            }
            if operation
                .as_ref()
                .err()
                .is_some_and(|error| is_availability(error.as_ref()))
                && access == RequestAccess::Read
                && !canonical_only
                && route.is_none()
                && selected.store.cache.is_some()
            {
                if continuation(&admitted_args) {
                    return Err("owner read became unavailable; cache continuation needs its exact returned source".into());
                }
                let replica = self.cache_route(selected, None)?;
                let mut fallback = admitted_args;
                fallback["_mneme_source"] = json!(ReadSource::from_route(&replica));
                drop(_route_guard);
                return self.call_mode(store, tool, fallback, budget, false).await;
            }
            operation
        })
    }
    pub async fn close(&self) {
        let clients = std::mem::take(&mut *self.sessions.lock().await);
        for (_, mut client) in clients {
            client.close().await;
        }
    }
}
fn continuation(args: &Value) -> bool {
    args.get("after").is_some()
        || args.get("cursor").is_some()
        || args.get("session").is_some()
        || ["body_offset", "offset"]
            .iter()
            .any(|key| args[*key].as_u64().is_some_and(|offset| offset > 0))
}
fn bounded_json(value: &Value, depth: usize) -> Result<(), AnyErr> {
    if depth > 32 {
        return Err("tool nesting exceeds 32 levels".into());
    }
    match value {
        Value::Array(values) => {
            for value in values {
                bounded_json(value, depth + 1)?;
            }
        }
        Value::Object(fields) => {
            for value in fields.values() {
                bounded_json(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn session_key(selected: &ActiveStore, route: Option<&ReplicaRoute>) -> String {
    match route {
        None => format!("owner:{}:{}", selected.name, selected.store.id),
        Some(route) => format!(
            "replica:{}:{}:{}:{}",
            selected.name,
            selected.store.id,
            route.source_device_id(),
            route.generation()
        ),
    }
}

#[cfg(test)]
mod tests;
