//! Explicitly enrolled, read-only multi-project retrieval. This crate never opens a store.
use async_trait::async_trait;
use futures::{StreamExt, stream};
use mneme_mcp_client::{ClientTimeouts, ConnectionOptions, RemoteClient, is_availability};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fmt,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

mod context;
mod source_routes;
pub use source_routes::{ReplicaPin, ReplicaRoute};
#[cfg(test)]
mod context_tests;
#[cfg(test)]
mod source_routes_tests;

const MAX_STORES: usize = 64;
const MAX_CANDIDATES: usize = 64;
const MAX_CONTEXT_BYTES: usize = 32 * 1024;

#[derive(Debug)]
pub struct LibraryError(pub String);
impl fmt::Display for LibraryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl Error for LibraryError {}
fn err(s: impl Into<String>) -> LibraryError {
    LibraryError(s.into())
}

fn read_metadata(
    path: &Path,
    limit: usize,
    label: &str,
    size: &str,
) -> Result<Vec<u8>, LibraryError> {
    let metadata =
        std::fs::metadata(path).map_err(|e| err(format!("{label} {}: {e}", path.display())))?;
    if !metadata.is_file() {
        return Err(err(format!(
            "{label} {} must be a regular file",
            path.display()
        )));
    }
    let file =
        std::fs::File::open(path).map_err(|e| err(format!("{label} {}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| err(format!("{label} {}: {e}", path.display())))?;
    if bytes.len() > limit {
        return Err(err(format!("{label} exceeds {size}")));
    }
    Ok(bytes)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub url: String,
    #[serde(default = "default_ssh_port")]
    pub ssh_mcp_port: u16,
    #[serde(default)]
    pub token_env: Option<String>,
}
fn default_ssh_port() -> u16 {
    18766
}
impl Endpoint {
    fn options(&self) -> ConnectionOptions {
        ConnectionOptions {
            url: self.url.clone(),
            ssh_mcp_port: self.ssh_mcp_port,
            token_env: self.token_env.clone(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "default_concurrent")]
    pub concurrent: usize,
    #[serde(default = "default_candidates")]
    pub total_candidates: usize,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}
fn default_concurrent() -> usize {
    4
}
fn default_candidates() -> usize {
    64
}
fn default_timeout() -> u64 {
    5000
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            concurrent: 4,
            total_candidates: 64,
            timeout_ms: 5000,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryConfig {
    pub schema: String,
    pub library_id: String,
    pub device_id: String,
    pub catalog_path: PathBuf,
    #[serde(default)]
    pub owners: BTreeMap<String, Endpoint>,
    #[serde(default)]
    pub owner_routes: BTreeMap<String, BTreeMap<String, Endpoint>>,
    #[serde(default)]
    pub replicas: BTreeMap<String, Endpoint>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub core: Option<CorePaths>,
    #[serde(default = "default_rerank")]
    pub rerank: bool,
}
fn default_rerank() -> bool {
    true
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorePaths {
    pub global_service: PathBuf,
    pub global_database: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replica {
    pub source_device_id: String,
    pub database: String,
    pub resolved_path: PathBuf,
    pub generation: String,
    pub captured_at: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub project_id: String,
    pub db_id: String,
    pub owner_device_id: String,
    pub display_name: String,
    pub database: String,
    pub revision: u64,
    #[serde(default)]
    pub withdrawn: bool,
    #[serde(default)]
    pub replicas: Vec<Replica>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub schema: String,
    pub library_id: String,
    pub revision: u64,
    pub entries: Vec<Entry>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryQuery {
    pub text: String,
    #[serde(default)]
    pub project_ids: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryGet {
    pub project_id: String,
    pub id: String,
    pub source_device_id: String,
    pub generation: Option<String>,
}

#[derive(Clone, Debug)]
pub enum SourceFailure {
    Availability(String),
    Unrouted(String),
    Refused(String),
}
#[async_trait]
pub trait Source: Send + Sync {
    async fn call(
        &self,
        endpoint: &Endpoint,
        tool: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<Value, SourceFailure>;
}
struct RemoteSource;
#[async_trait]
impl Source for RemoteSource {
    async fn call(
        &self,
        endpoint: &Endpoint,
        tool: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<Value, SourceFailure> {
        let opts = endpoint.options();
        let timeouts = ClientTimeouts {
            connect: timeout,
            request: timeout,
        };
        let mut client = RemoteClient::connect_with_timeouts(&opts, timeouts)
            .await
            .map_err(classify)?;
        let result = client.call_tool(tool, args).await.map_err(classify);
        client.close().await;
        result
    }
}
fn classify(e: Box<dyn Error + Send + Sync>) -> SourceFailure {
    if is_availability(e.as_ref()) {
        SourceFailure::Availability(e.to_string())
    } else {
        SourceFailure::Refused(e.to_string())
    }
}

#[async_trait]
pub trait Ranker: Send + Sync {
    fn semantic_id(&self) -> &'static str {
        "custom"
    }
    async fn rank(&self, query: &str, summaries: &[String]) -> Result<Vec<f32>, String>;
}
#[cfg(feature = "fast-rerank")]
pub struct CommonFastReranker {
    admission: Arc<tokio::sync::Semaphore>,
    model: Arc<std::sync::Mutex<Option<mneme_embed::FastReranker>>>,
}
#[cfg(feature = "fast-rerank")]
impl CommonFastReranker {
    pub fn new() -> Self {
        Self {
            admission: Arc::new(tokio::sync::Semaphore::new(1)),
            model: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}
#[cfg(feature = "fast-rerank")]
#[async_trait]
impl Ranker for CommonFastReranker {
    fn semantic_id(&self) -> &'static str {
        "fastembed-bge-reranker-base-v1"
    }
    async fn rank(&self, query: &str, summaries: &[String]) -> Result<Vec<f32>, String> {
        let admission = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| "common reranker busy".to_owned())?;
        let model = self.model.clone();
        let q = query.to_owned();
        let docs = summaries.to_vec();
        tokio::task::spawn_blocking(move || {
            // The permit moves into the worker, not merely its awaiting future.
            // Cancellation cannot start a second load or inference concurrently.
            let _admission = admission;
            let mut guard = model
                .lock()
                .map_err(|_| "common reranker mutex poisoned".to_owned())?;
            if guard.is_none() {
                *guard = Some(mneme_embed::FastReranker::new().map_err(|e| e.to_string())?);
            }
            guard
                .as_ref()
                .expect("initialized reranker")
                .rerank_sync(&q, &docs.iter().map(String::as_str).collect::<Vec<_>>())
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

pub struct LibraryRuntime {
    config: LibraryConfig,
    source: Arc<dyn Source>,
    ranker: Option<Arc<dyn Ranker>>,
}
impl LibraryRuntime {
    pub fn from_path(path: &Path) -> Result<Self, LibraryError> {
        let bytes = read_metadata(path, 64 * 1024, "library config", "64 KiB")?;
        let mut config: LibraryConfig =
            serde_json::from_slice(&bytes).map_err(|e| err(format!("library config: {e}")))?;
        if config.catalog_path.is_relative() {
            config.catalog_path = path
                .parent()
                .unwrap_or(Path::new("."))
                .join(&config.catalog_path)
        }
        Self::new(config)
    }
    pub fn new(config: LibraryConfig) -> Result<Self, LibraryError> {
        validate_config(&config)?;
        #[cfg(feature = "fast-rerank")]
        let ranker: Option<Arc<dyn Ranker>> = config
            .rerank
            .then(|| Arc::new(CommonFastReranker::new()) as Arc<dyn Ranker>);
        #[cfg(not(feature = "fast-rerank"))]
        let ranker = None;
        Ok(Self {
            config,
            source: Arc::new(RemoteSource),
            ranker,
        })
    }
    pub fn with_source(mut self, source: Arc<dyn Source>) -> Self {
        self.source = source;
        self
    }
    pub fn with_ranker(mut self, ranker: Arc<dyn Ranker>) -> Self {
        self.ranker = Some(ranker);
        self
    }
    fn read_catalog(&self) -> Result<Catalog, LibraryError> {
        let bytes = read_metadata(
            &self.config.catalog_path,
            512 * 1024,
            "library catalog",
            "512 KiB",
        )?;
        let cat: Catalog =
            serde_json::from_slice(&bytes).map_err(|e| err(format!("library catalog: {e}")))?;
        if cat.schema != "mneme.library.catalog.v1" || cat.library_id != self.config.library_id {
            return Err(err("library catalog schema or library_id mismatch"));
        }
        if cat.entries.len() > MAX_STORES {
            return Err(err("library catalog exceeds 64 entries"));
        }
        let mut ids = HashSet::new();
        for e in &cat.entries {
            if e.project_id.is_empty()
                || e.db_id.is_empty()
                || e.owner_device_id.is_empty()
                || e.database.is_empty()
                || !ids.insert(&e.project_id)
            {
                return Err(err("invalid or duplicate project descriptor"));
            }
            if e.replicas.len() > 8 {
                return Err(err("too many replicas"));
            }
        }
        Ok(cat)
    }
    /// Validated, non-withdrawn descriptors and their optional configured routes.
    /// This is metadata only: no owner is contacted, and no replica/store is opened.
    pub fn known_sources(&self) -> Result<Vec<(Entry, Option<Endpoint>)>, LibraryError> {
        Ok(self
            .read_catalog()?
            .entries
            .into_iter()
            .filter(|entry| !entry.withdrawn)
            .map(|entry| {
                let route = owner_endpoint(&self.config, &entry).cloned();
                (entry, route)
            })
            .collect())
    }
    /// Verified catalog entries with configured live owner routes. Read-only
    /// clients must still check the owner's advertised database identity.
    pub fn live_sources(&self) -> Result<Vec<(Entry, Endpoint)>, LibraryError> {
        Ok(self
            .known_sources()?
            .into_iter()
            .filter_map(|(entry, route)| route.map(|route| (entry, route)))
            .collect())
    }

    pub fn core_paths(&self) -> Option<&CorePaths> {
        self.config.core.as_ref()
    }

    pub fn catalog(&self) -> Result<Value, LibraryError> {
        let cat = self.read_catalog()?;
        Ok(
            json!({"schema":"mneme.library.catalog.v1","library_id":cat.library_id,"revision":cat.revision,"entries":cat.entries.iter().filter(|e|!e.withdrawn).map(|e|json!({"project_id":e.project_id,"db_id":e.db_id,"owner_device_id":e.owner_device_id,"display_name":e.display_name,"revision":e.revision,"live_route":if owner_endpoint(&self.config,e).is_some(){"configured"}else{"unrouted"},"replicas":e.replicas.iter().map(|r|json!({"source_device_id":r.source_device_id,"generation":r.generation,"captured_at":r.captured_at})).collect::<Vec<_>>()})).collect::<Vec<_>>() }),
        )
    }
    pub async fn dispatch(&self, name: &str, args: Value) -> Result<Value, LibraryError> {
        match name {
            "library_catalog" => {
                if args != json!({}) && args != Value::Null {
                    return Err(err("library_catalog accepts no arguments"));
                }
                self.catalog()
            }
            "library_query" => self.query(parse(args)?).await,
            "library_recall_context" => self.recall_context(parse(args)?).await,
            "library_get" => self.get(parse(args)?).await,
            _ => Err(err("unknown library tool")),
        }
    }
    pub async fn query(&self, q: LibraryQuery) -> Result<Value, LibraryError> {
        self.retrieve(q, false).await
    }
    pub async fn recall_context(&self, q: LibraryQuery) -> Result<Value, LibraryError> {
        self.retrieve(q, true).await
    }
    async fn retrieve(&self, q: LibraryQuery, context: bool) -> Result<Value, LibraryError> {
        validate_query(&q)?;
        let cat = self.read_catalog()?;
        let selected: Vec<Entry> = cat
            .entries
            .iter()
            .filter(|e| {
                !e.withdrawn && (q.project_ids.is_empty() || q.project_ids.contains(&e.project_id))
            })
            .cloned()
            .collect();
        for id in &q.project_ids {
            if !selected.iter().any(|e| &e.project_id == id) {
                return Err(err(format!(
                    "project {id:?} is not enrolled or is withdrawn"
                )));
            }
        }
        let count = selected.len();
        if count == 0 && !context {
            return Ok(
                json!({"schema":"mneme.library.query.v2","library_id":cat.library_id,"coverage":[],"primary":[],"ordering":"none","partial":false}),
            );
        }
        let per = (self.config.limits.total_candidates / count.max(1))
            .max(1)
            .min(64);
        let timeout = Duration::from_millis(self.config.limits.timeout_ms);
        let source = self.source.clone();
        let cfg = Arc::new(self.config.clone());
        let results =
            stream::iter(
                selected.into_iter().map(|entry| {
                    let source = source.clone();
                    let cfg = cfg.clone();
                    let tags = q.tags.clone();
                    let text = q.text.clone();
                    async move {
                        retrieve_one(source, cfg, entry, text, tags, per, timeout, context).await
                    }
                }),
            )
            .buffered(self.config.limits.concurrent)
            .collect::<Vec<_>>()
            .await;
        let mut primary = Vec::new();
        let mut coverage = Vec::new();
        let mut episodes = Vec::new();
        for result in results {
            coverage.push(result.coverage);
            primary.extend(result.primary);
            episodes.extend(result.episodes);
        }
        if context {
            for (project_id, db_id) in context::deduplicate_episodes(&mut episodes) {
                for source in &mut coverage {
                    if source["project_id"] == project_id && source["db_id"] == db_id {
                        source["partial"] = json!(true);
                        source["episode_card_refusal"] = json!(
                            "conflicting duplicate immutable edition or incompatible observed-head origins"
                        );
                    }
                }
            }
        }
        let considered = [primary.len(), episodes.len()];
        // Bound provider work before reranking. Episodic pooling must wait for
        // exact metadata feasibility: an oversized first account must not take
        // the candidate slot of a later scene that actually fits.
        if !context {
            primary.truncate(self.config.limits.total_candidates);
        }
        let mut ordering = "grouped_source_local";
        let mut ranker_semantics: Option<&str> = None;
        if let Some(ranker) = &self.ranker {
            // Bound provider work without discarding the later candidates yet:
            // exact facet feasibility can still reject an oversized early card.
            let unranked =
                primary.split_off(primary.len().min(self.config.limits.total_candidates));
            let docs: Vec<String> = primary
                .iter()
                .map(|h| h["summary"].as_str().unwrap_or("").to_owned())
                .collect();
            if !docs.is_empty()
                && let Ok(scores) = ranker.rank(&q.text, &docs).await
            {
                if scores.len() == docs.len() && scores.iter().all(|s| s.is_finite()) {
                    let mut all = primary
                        .into_iter()
                        .enumerate()
                        .map(|(i, h)| (scores[i], i, h))
                        .collect::<Vec<_>>();
                    all.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
                    primary = all
                        .into_iter()
                        .take(if context {
                            self.config.limits.total_candidates
                        } else {
                            context::MAX_ITEMS
                        })
                        .map(|(_, _, h)| h)
                        .collect();
                    ordering = "common_rerank";
                    ranker_semantics = Some(ranker.semantic_id());
                }
            }
            primary.extend(unranked);
        }
        if !context && ordering == "grouped_source_local" {
            primary.truncate(context::MAX_ITEMS);
        }
        let partial = coverage
            .iter()
            .any(|c: &Value| c["state"] != "live" || c["partial"] == true);
        let mut result = json!({"schema":if context{"mneme.library.context.v5"}else{"mneme.library.query.v2"},"library_id":cat.library_id,"catalog_revision":cat.revision,"coverage":coverage,"primary":primary,"ordering":ordering,"ranker_semantics":ranker_semantics,"partial":partial,"context_truncated":false,"receipt":null});
        if context {
            result["episodes"] = json!(episodes);
            result["episodic_coverage_encoding"] = json!("native_report_index_v1");
            result["episodic_reports"] = json!(context::intern_episodic_reports(
                result["coverage"].as_array_mut().expect("source coverage"),
            ));
            result["episode_reference_coverage_encoding"] = json!("native_report_index_v1");
            result["episode_reference_reports"] = json!(context::intern_reference_reports(
                result["coverage"].as_array_mut().expect("source coverage"),
            ));
            result["touchstone_coverage_encoding"] = json!("native_report_index_v1");
            result["touchstone_reports"] = json!(context::intern_touchstone_reports(
                result["coverage"].as_array_mut().expect("source coverage"),
            ));
            result["native_omission_coverage_encoding"] = json!("native_report_index_v1");
            result["native_omission_reports"] = json!(context::intern_omission_reports(
                result["coverage"].as_array_mut().expect("source coverage"),
            ));
            result["omission_scope"] = json!("pooled_returned_windows");
            result["episodic_retrieval"] = json!({"mode":"lexical_reference","ordering":"grouped_source_local","candidate_limit":self.config.limits.total_candidates,"target_items":context::EPISODE_TARGET,"max_items":context::EPISODE_MAX});
            context::remove_unfit_cards(&mut result, considered);
            context::trim_result_pool(&mut result, self.config.limits.total_candidates);
            context::trim_result_items(&mut result);
            context::update_omissions(&mut result, considered);
            while serde_json::to_vec(&result)
                .map_err(|e| err(e.to_string()))?
                .len()
                > MAX_CONTEXT_BYTES
            {
                // Spare episode slots yield before semantic cards; the feasible
                // target survives until semantic payload has been reduced.
                if result["episodes"].as_array().unwrap().len() > context::EPISODE_TARGET {
                    result["episodes"].as_array_mut().unwrap().pop();
                    context::mark_truncated(&mut result, considered);
                    continue;
                }
                let a = result["primary"].as_array_mut().unwrap();
                if !a.is_empty() {
                    a.pop();
                    context::mark_truncated(&mut result, considered);
                    continue;
                }
                if !result["episodes"].as_array().unwrap().is_empty() {
                    result["episodes"].as_array_mut().unwrap().pop();
                    context::mark_truncated(&mut result, considered);
                    continue;
                }
                return Err(err(
                    "library context metadata alone exceeds 32 KiB; select fewer projects with project_ids or --project-id",
                ));
            }
        }
        Ok(result)
    }
    pub async fn get(&self, g: LibraryGet) -> Result<Value, LibraryError> {
        if g.id.is_empty()
            || g.id.len() > 80
            || g.project_id.is_empty()
            || g.source_device_id.is_empty()
        {
            return Err(err("invalid library_get reference"));
        }
        let cat = self.read_catalog()?;
        let e = cat
            .entries
            .iter()
            .find(|e| e.project_id == g.project_id && !e.withdrawn)
            .ok_or_else(|| err("project is not enrolled or is withdrawn"))?;
        let (endpoint, db, source_kind, generation, asof, expected_path) = if g.source_device_id
            == e.owner_device_id
            && g.generation.is_none()
        {
            let ep = owner_endpoint(&self.config,e)
                .ok_or_else(|| err("live route not configured for this project; provision owner_routes or query a snapshot"))?;
            (ep, &e.database, "live", None, None, None)
        } else {
            let generation = g
                .generation
                .as_deref()
                .ok_or_else(|| err("replica reference requires generation"))?;
            let replica = e
                .replicas
                .iter()
                .find(|r| r.source_device_id == g.source_device_id && r.generation == generation)
                .ok_or_else(|| err("snapshot generation expired; run library_query again"))?;
            let ep = self
                .config
                .replicas
                .get(&replica.source_device_id)
                .ok_or_else(|| err("replica endpoint is not configured"))?;
            (
                ep,
                &replica.database,
                "snapshot",
                Some(replica.generation.as_str()),
                Some(replica.captured_at),
                Some(replica.resolved_path.as_path()),
            )
        };
        let alias = verify_db(
            &*self.source,
            endpoint,
            db,
            &e.db_id,
            expected_path,
            Duration::from_millis(self.config.limits.timeout_ms),
        )
        .await
        .map_err(|f| {
            err(format!(
                "library_get identity or source failure: {}",
                failure_text(f)
            ))
        })?;
        let value = self
            .source
            .call(
                endpoint,
                "get",
                json!({"db":alias,"id":g.id,"body":true,"max_body_bytes":4096}),
                Duration::from_millis(self.config.limits.timeout_ms),
            )
            .await
            .map_err(|f| err(failure_text(f)))?;
        Ok(
            json!({"schema":"mneme.library.get.v1","project_id":e.project_id,"db_id":e.db_id,"source":{"device_id":g.source_device_id,"kind":source_kind,"generation":generation,"captured_at":asof},"node":value}),
        )
    }
}
fn parse<T: for<'de> Deserialize<'de>>(v: Value) -> Result<T, LibraryError> {
    serde_json::from_value(v).map_err(|e| err(format!("invalid library arguments: {e}")))
}
fn validate_config(c: &LibraryConfig) -> Result<(), LibraryError> {
    if c.schema != "mneme.library.config.v1" || c.library_id.is_empty() || c.device_id.is_empty() {
        return Err(err("invalid library config schema or identity"));
    }
    if c.limits.concurrent == 0
        || c.limits.concurrent > 4
        || c.limits.total_candidates == 0
        || c.limits.total_candidates > MAX_CANDIDATES
        || c.limits.timeout_ms == 0
        || c.limits.timeout_ms > 30000
    {
        return Err(err("library limits exceed safe bounds"));
    }
    if let Some(core) = &c.core {
        if !core.global_service.is_absolute() || !core.global_database.is_absolute() {
            return Err(err("library core paths must be absolute"));
        }
    }
    if c.owners.len() > MAX_STORES
        || c.owner_routes.len() > MAX_STORES
        || c.owner_routes.iter().any(|(device, routes)| {
            device.is_empty() || routes.len() > MAX_STORES || routes.keys().any(String::is_empty)
        })
    {
        return Err(err(
            "library owner routes exceed bounds or contain empty identities",
        ));
    }
    Ok(())
}
fn owner_endpoint<'a>(cfg: &'a LibraryConfig, e: &Entry) -> Option<&'a Endpoint> {
    cfg.owner_routes
        .get(&e.owner_device_id)
        .and_then(|routes| routes.get(&e.db_id))
        .or_else(|| cfg.owners.get(&e.owner_device_id))
}
fn validate_query(q: &LibraryQuery) -> Result<(), LibraryError> {
    if q.text.is_empty()
        || q.text.len() > 4096
        || q.project_ids.len() > MAX_STORES
        || q.tags.len() > 16
        || q.tags.iter().any(|s| s.len() > 128)
    {
        return Err(err("library query exceeds bounds"));
    }
    let mut seen = HashSet::new();
    if q.project_ids
        .iter()
        .any(|s| s.is_empty() || !seen.insert(s))
    {
        return Err(err("duplicate or empty project selector"));
    }
    Ok(())
}
fn failure_text(f: SourceFailure) -> String {
    match f {
        SourceFailure::Availability(s) => format!("source unavailable: {s}"),
        SourceFailure::Unrouted(s) => format!("source unrouted: {s}"),
        SourceFailure::Refused(s) => format!("source refused: {s}"),
    }
}
struct One {
    coverage: Value,
    primary: Vec<Value>,
    episodes: Vec<Value>,
}
async fn verify_db(
    source: &dyn Source,
    ep: &Endpoint,
    db: &str,
    expected: &str,
    expected_path: Option<&Path>,
    timeout: Duration,
) -> Result<String, SourceFailure> {
    let list = source.call(ep, "databases", json!({}), timeout).await?;
    verify_database_rows(&list, db, expected, expected_path)
}

fn verify_database_rows(
    list: &Value,
    db: &str,
    expected: &str,
    expected_path: Option<&Path>,
) -> Result<String, SourceFailure> {
    let rows = list
        .as_array()
        .ok_or_else(|| SourceFailure::Refused("invalid database catalog".into()))?;
    // A replica descriptor carries an immutable absolute generation path. Match
    // the server's resolved path, not merely db_id (which every generation shares).
    let item = rows
        .iter()
        .find(|v| {
            if Path::new(db).is_absolute() {
                v["resolved_path"] == db
            } else {
                v["db"] == db || v["name"] == db
            }
        })
        .ok_or_else(|| SourceFailure::Refused(format!("database {db:?} not advertised")))?;
    verify_database_row(item, db, expected, expected_path)
}

fn verify_database_row(
    item: &Value,
    db: &str,
    expected: &str,
    expected_path: Option<&Path>,
) -> Result<String, SourceFailure> {
    if item["db_id"] != expected {
        return Err(SourceFailure::Refused(format!(
            "database {db:?} identity mismatch"
        )));
    }
    if let Some(path) = expected_path {
        if item["resolved_path"].as_str() != path.to_str() {
            return Err(SourceFailure::Refused(format!(
                "database {db:?} snapshot generation path mismatch"
            )));
        }
    }
    match item["state"].as_str() {
        Some("open") => {}
        Some("releasing" | "maintenance" | "resuming" | "snapshotting" | "snapshot_recovery") => {
            return Err(SourceFailure::Availability(format!(
                "database {db:?} is {}",
                item["state"]
            )));
        }
        _ => {
            return Err(SourceFailure::Refused(format!(
                "database {db:?} lifecycle state unknown"
            )));
        }
    }
    item["db"]
        .as_str()
        .or_else(|| item["name"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| SourceFailure::Refused("database alias missing".into()))
}
async fn retrieve_one(
    source: Arc<dyn Source>,
    cfg: Arc<LibraryConfig>,
    e: Entry,
    text: String,
    tags: Vec<String>,
    per: usize,
    timeout: Duration,
    context: bool,
) -> One {
    let mut state = "live";
    let mut device = e.owner_device_id.as_str();
    let mut db = e.database.as_str();
    let mut generation: Option<&str> = None;
    let mut asof: Option<u64> = None;
    let mut endpoint = owner_endpoint(&cfg, &e);
    let mut result = match endpoint {
        Some(ep) => {
            query_source_bounded(
                &*source, ep, db, &e.db_id, None, &text, &tags, per, timeout, context,
            )
            .await
        }
        None => Err(SourceFailure::Unrouted("live route not configured".into())),
    };
    let owner_failure = match &result {
        Err(SourceFailure::Availability(reason) | SourceFailure::Unrouted(reason)) => {
            Some(reason.clone())
        }
        _ => None,
    };
    if owner_failure.is_some() {
        if let Some(r) = e
            .replicas
            .iter()
            .filter(|r| cfg.replicas.contains_key(&r.source_device_id))
            .max_by_key(|r| (r.captured_at, r.generation.as_str()))
        {
            state = "snapshot";
            device = &r.source_device_id;
            db = &r.database;
            generation = Some(&r.generation);
            asof = Some(r.captured_at);
            endpoint = cfg.replicas.get(device);
            result = query_source_bounded(
                &*source,
                endpoint.unwrap(),
                db,
                &e.db_id,
                Some(&r.resolved_path),
                &text,
                &tags,
                per,
                timeout,
                context,
            )
            .await
        }
    }
    let source_ref =
        json!({"device_id":device,"kind":state,"generation":generation,"captured_at":asof});
    match result {
        Ok(v) => {
            let mut primary = Vec::new();
            let mut episodes = Vec::new();
            for (lane, out) in [("primary", &mut primary)] {
                let hits = if context {
                    v[lane].as_array()
                } else {
                    v.pointer(&format!("/lanes/{lane}/hits"))
                        .and_then(Value::as_array)
                };
                if let Some(hits) = hits {
                    for h in hits.iter().take(if context { hits.len() } else { per }) {
                        out.push(if context {
                            context::project(h, &e.project_id, &e.db_id, &source_ref, lane)
                        } else {
                            json!({"project_id":e.project_id,"db_id":e.db_id,"source":source_ref,"id":h["id"],"summary":h["summary"],"status":h["status"],"lane":lane,"source_rank":h["lane_rank"]})
                        });
                    }
                }
            }
            let mut coverage = json!({"project_id":e.project_id,"db_id":e.db_id,"state":state,"source":source_ref,"partial":v["partial"],"owner_unavailable":owner_failure});
            if context {
                coverage["context_schema"] = v["schema"].clone();
                coverage["episodic"] = context::coverage(&v);
                coverage["episode_reference_retrieval"] = v["episode_reference_retrieval"].clone();
                coverage["touchstone_retrieval"] = v["touchstone_retrieval"].clone();
                coverage["native_omitted"] = v["omitted"].clone();
                if v["touchstone_retrieval"]["further_tail_unknown"] == true
                    || !v["touchstone_retrieval"]["stop_reason"].is_null()
                {
                    coverage["partial"] = json!(true);
                }
                if v["episode_reference_retrieval"]["further_tail_unknown"] == true
                    || v["episode_reference_retrieval"]
                        .get("stop_reason")
                        .is_some()
                {
                    coverage["partial"] = json!(true);
                }
                if coverage["episodic"]["state"] != "searched" {
                    coverage["partial"] = json!(true);
                }
                if let Some(cards) = v["episodes"].as_array() {
                    episodes.extend(cards.iter().map(|h| {
                        context::project(h, &e.project_id, &e.db_id, &source_ref, "episodic")
                    }));
                }
            }
            One {
                coverage,
                primary,
                episodes,
            }
        }
        Err(f) => One {
            coverage: json!({"project_id":e.project_id,"state":match f {SourceFailure::Availability(_)=>"unavailable",SourceFailure::Unrouted(_)=>"unrouted",SourceFailure::Refused(_)=>"refused"},"reason":failure_text(f)}),
            primary: vec![],
            episodes: vec![],
        },
    }
}
async fn query_source(
    source: &dyn Source,
    ep: &Endpoint,
    db: &str,
    expected: &str,
    expected_path: Option<&Path>,
    text: &str,
    tags: &[String],
    per: usize,
    timeout: Duration,
    context: bool,
) -> Result<Value, SourceFailure> {
    let alias = verify_db(source, ep, db, expected, expected_path, timeout).await?;
    if context {
        let value = source
            .call(
                ep,
                "recall_context",
                json!({"db":alias,"text":text,"tags":tags,"depth":0,"max_nodes":per,"k":per}),
                timeout,
            )
            .await?;
        context::validate_source(&value, per, expected)?;
        return Ok(value);
    }
    let v = source
        .call(
            ep,
            "query",
            json!({"db":alias,"text":text,"tags":tags,"depth":0,"max_nodes":per,"k":per}),
            timeout,
        )
        .await?;
    if v["schema"] != "mneme.query.v3" {
        return Err(SourceFailure::Refused("unexpected query schema".into()));
    }
    if !v["partial"].is_boolean() {
        return Err(SourceFailure::Refused("query partial flag missing".into()));
    }
    if v["lanes"]
        .as_object()
        .is_none_or(|lanes| lanes.len() != 1 || !lanes.contains_key("primary"))
    {
        return Err(SourceFailure::Refused("query lanes invalid".into()));
    }
    if v["lanes"]["primary"].get("seed_coverage").is_none() {
        return Err(SourceFailure::Refused(
            "primary seed coverage missing".into(),
        ));
    }
    for lane in ["primary"] {
        let Some(hits) = v
            .pointer(&format!("/lanes/{lane}/hits"))
            .and_then(Value::as_array)
        else {
            return Err(SourceFailure::Refused("query lanes missing".into()));
        };
        if hits.len() > 256
            || hits.iter().any(|h| {
                h["id"]
                    .as_str()
                    .is_none_or(|s| s.is_empty() || s.len() > 80)
                    || h["summary"].as_str().is_none_or(|s| s.len() > 2048)
                    || h["body"]["state"] != "not_requested"
                    || h["lane_rank"].as_u64().is_none()
                    || !matches!(h["status"].as_str(), Some("active"))
            })
        {
            return Err(SourceFailure::Refused(
                "query hit schema invalid or body disclosed".into(),
            ));
        }
    }
    Ok(v)
}
async fn query_source_bounded(
    source: &dyn Source,
    ep: &Endpoint,
    db: &str,
    expected: &str,
    expected_path: Option<&Path>,
    text: &str,
    tags: &[String],
    per: usize,
    timeout: Duration,
    context: bool,
) -> Result<Value, SourceFailure> {
    tokio::time::timeout(
        timeout,
        query_source(
            source,
            ep,
            db,
            expected,
            expected_path,
            text,
            tags,
            per,
            timeout,
            context,
        ),
    )
    .await
    .unwrap_or_else(|_| {
        Err(SourceFailure::Availability(
            "source query deadline exceeded".into(),
        ))
    })
}

pub fn tool_definitions() -> Value {
    json!([{"name":"library_catalog","description":"List explicitly enrolled projects and snapshot availability","inputSchema":{"type":"object","additionalProperties":false}},{"name":"library_query","description":"Read bounded summaries across enrolled projects","inputSchema":{"type":"object","properties":{"text":{"type":"string"},"project_ids":{"type":"array","items":{"type":"string"}},"tags":{"type":"array","items":{"type":"string"}}},"required":["text"],"additionalProperties":false}},{"name":"library_recall_context","description":"Read library.context.v5: up to 13 semantic and typed lexical/reference episode cards in 32 KiB, pinned to owner or fallback snapshot; compact touchstone facets and discovery coverage are explicit","inputSchema":{"type":"object","properties":{"text":{"type":"string"},"project_ids":{"type":"array","items":{"type":"string"}},"tags":{"type":"array","items":{"type":"string"}}},"required":["text"],"additionalProperties":false}},{"name":"library_get","description":"Get one node pinned to its source and snapshot generation","inputSchema":{"type":"object","properties":{"project_id":{"type":"string"},"id":{"type":"string"},"source_device_id":{"type":"string"},"generation":{"type":"string"}},"required":["project_id","id","source_device_id"],"additionalProperties":false}}])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    #[test]
    fn known_sources_retains_unrouted_and_skips_withdrawn_without_contact() {
        let (library, fake, path) = fixture();
        let mut catalog: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut unrouted = catalog.entries[0].clone();
        unrouted.project_id = "unrouted-project".into();
        unrouted.db_id = "unrouted-identity".into();
        unrouted.owner_device_id = "unconfigured-device".into();
        catalog.entries.push(unrouted.clone());
        unrouted.project_id = "withdrawn-project".into();
        unrouted.withdrawn = true;
        catalog.entries.push(unrouted);
        std::fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
        let known = library.known_sources().unwrap();
        assert_eq!(known.len(), 2);
        assert_eq!(known[1].0.db_id, "unrouted-identity");
        assert!(known[1].1.is_none());
        assert_eq!(library.live_sources().unwrap().len(), 1);
        assert!(fake.calls.lock().unwrap().is_empty());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn metadata_reads_admit_exact_limits_and_refuse_before_decode() {
        let (library, _, catalog_path) = fixture();
        let config_path = catalog_path.with_extension("config.json");
        let mut config_bytes = serde_json::to_vec(&library.config).unwrap();
        config_bytes.resize(64 * 1024, b' ');
        std::fs::write(&config_path, &config_bytes).unwrap();
        assert!(LibraryRuntime::from_path(&config_path).is_ok());
        config_bytes.push(b' ');
        std::fs::write(&config_path, &config_bytes).unwrap();
        assert!(
            LibraryRuntime::from_path(&config_path)
                .err()
                .unwrap()
                .0
                .contains("64 KiB")
        );
        let mut catalog_bytes = std::fs::read(&catalog_path).unwrap();
        catalog_bytes.resize(512 * 1024, b' ');
        std::fs::write(&catalog_path, &catalog_bytes).unwrap();
        assert!(library.known_sources().is_ok());
        catalog_bytes[0] = b'!'; // invalid JSON must not win over the resource bound
        catalog_bytes.push(b' ');
        std::fs::write(&catalog_path, &catalog_bytes).unwrap();
        assert!(library.known_sources().unwrap_err().0.contains("512 KiB"));
        std::fs::remove_file(config_path).unwrap();
        std::fs::remove_file(catalog_path).unwrap();
    }
    #[cfg(feature = "fast-rerank")]
    #[tokio::test]
    async fn common_ranker_busy_degrades_without_loading_model() {
        let ranker = CommonFastReranker::new();
        let held = ranker.admission.clone().try_acquire_owned().unwrap();
        assert!(
            ranker
                .rank("q", &["summary".into()])
                .await
                .unwrap_err()
                .contains("busy")
        );
        drop(held);
        let held = ranker.admission.clone().try_acquire_owned().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let _held = held;
            started_tx.send(()).unwrap();
            rx.recv().unwrap();
        });
        started_rx.recv().unwrap();
        worker.abort();
        assert!(
            ranker
                .rank("q", &["summary".into()])
                .await
                .unwrap_err()
                .contains("busy")
        );
        tx.send(()).unwrap();
    }
    #[derive(Default)]
    pub(super) struct Fake {
        pub(super) calls: Mutex<Vec<(String, String)>>,
        pub(super) errors: Mutex<BTreeMap<String, SourceFailure>>,
        states: Mutex<BTreeMap<String, String>>,
        disclosed_body: Mutex<bool>,
        query_override: Mutex<Option<Value>>,
        pub(super) contexts: Mutex<BTreeMap<String, Value>>,
        pub(super) requests: Mutex<Vec<(String, Value)>>,
        pub(super) database_ids: Mutex<BTreeMap<String, String>>,
        pub(super) get_values: Mutex<BTreeMap<String, Value>>,
    }
    #[async_trait]
    impl Source for Fake {
        async fn call(
            &self,
            ep: &Endpoint,
            tool: &str,
            args: Value,
            _: Duration,
        ) -> Result<Value, SourceFailure> {
            self.requests
                .lock()
                .unwrap()
                .push((tool.into(), args.clone()));
            self.calls
                .lock()
                .unwrap()
                .push((ep.url.clone(), tool.into()));
            if let Some(f) = self.errors.lock().unwrap().get(&ep.url).cloned() {
                return Err(f);
            }
            if tool == "databases" {
                let db_id = self
                    .database_ids
                    .lock()
                    .unwrap()
                    .get(&ep.url)
                    .cloned()
                    .unwrap_or_else(|| if ep.url == "route2" { "db-2" } else { "db-1" }.into());
                return Ok(
                    json!([{"db":if ep.url=="replica"{"copy"}else{"main"},"db_id":db_id,"resolved_path":if ep.url=="replica"{"/immutable/g1/database.db"}else{"/owner/database.db"},"state":self.states.lock().unwrap().get(&ep.url).cloned().unwrap_or("open".into())}]),
                );
            }
            if tool == "get" {
                if let Some(value) = self.get_values.lock().unwrap().get(&ep.url).cloned() {
                    return Ok(value);
                }
                return Ok(json!({"id":args["id"],"body":"test body"}));
            }
            if tool == "recall_context" {
                return Ok(self
                    .contexts
                    .lock()
                    .unwrap()
                    .get(&ep.url)
                    .cloned()
                    .unwrap_or_else(crate::context_tests::owner_context));
            }
            if let Some(value) = self.query_override.lock().unwrap().clone() {
                return Ok(value);
            }
            Ok(
                json!({"schema":"mneme.query.v3","partial":false,"lanes":{"primary":{"seed_coverage":null,"hits":[{"id":"node-1","summary":"summary only","body":if *self.disclosed_body.lock().unwrap(){json!({"state":"included","content":"secret"})}else{json!({"state":"not_requested"})},"status":"active","lane_rank":1}]}}}),
            )
        }
    }
    pub(super) fn fixture() -> (LibraryRuntime, Arc<Fake>, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "mneme-library-test-{}-{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("x")
        ));
        let cat = Catalog {
            schema: "mneme.library.catalog.v1".into(),
            library_id: "lib".into(),
            revision: 1,
            entries: vec![Entry {
                project_id: "p".into(),
                db_id: "db-1".into(),
                owner_device_id: "owner".into(),
                display_name: "Project".into(),
                database: "main".into(),
                revision: 1,
                withdrawn: false,
                replicas: vec![Replica {
                    source_device_id: "mirror".into(),
                    database: "copy".into(),
                    resolved_path: PathBuf::from("/immutable/g1/database.db"),
                    generation: "g1".into(),
                    captured_at: 1_790_380_800,
                }],
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
        let cfg = LibraryConfig {
            schema: "mneme.library.config.v1".into(),
            library_id: "lib".into(),
            device_id: "local".into(),
            catalog_path: path.clone(),
            owners: BTreeMap::from([(
                "owner".into(),
                Endpoint {
                    url: "owner".into(),
                    ssh_mcp_port: 7337,
                    token_env: None,
                },
            )]),
            owner_routes: BTreeMap::new(),
            replicas: BTreeMap::from([(
                "mirror".into(),
                Endpoint {
                    url: "replica".into(),
                    ssh_mcp_port: 7337,
                    token_env: None,
                },
            )]),
            limits: Limits::default(),
            core: None,
            rerank: false,
        };
        let fake = Arc::new(Fake::default());
        let runtime = LibraryRuntime::new(cfg).unwrap().with_source(fake.clone());
        (runtime, fake, path)
    }
    pub(super) fn query() -> LibraryQuery {
        LibraryQuery {
            text: "needle".into(),
            project_ids: vec![],
            tags: vec![],
        }
    }
    #[tokio::test]
    async fn reads_summaries_only_and_no_exposure() {
        let (rt, f, path) = fixture();
        let v = rt.query(query()).await.unwrap();
        assert_eq!(v["primary"][0]["summary"], "summary only");
        assert!(v.to_string().find("test body").is_none());
        assert_eq!(v["primary"][0]["source"]["kind"], "live");
        assert_eq!(
            f.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(_, t)| t.as_str())
                .collect::<Vec<_>>(),
            vec!["databases", "query"]
        );
        std::fs::remove_file(path).unwrap()
    }
    #[tokio::test]
    async fn old_query_envelope_and_empty_compatibility_lane_are_refused() {
        let (rt, fake, path) = fixture();
        let current = json!({"schema":"mneme.query.v3","partial":false,"lanes":{"primary":{"seed_coverage":null,"hits":[]}}});
        for mut old in [
            {
                let mut v = current.clone();
                v["schema"] = json!("mneme.query.v2");
                v
            },
            {
                let mut v = current.clone();
                v["lanes"]["probationary"] = json!({"hits":[]});
                v
            },
        ] {
            *fake.query_override.lock().unwrap() = Some(std::mem::take(&mut old));
            let response = rt.query(query()).await.unwrap();
            assert_eq!(response["coverage"][0]["state"], "refused");
            assert!(response["primary"].as_array().unwrap().is_empty());
        }
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn availability_falls_back_but_refusal_does_not() {
        let (rt, f, path) = fixture();
        f.errors.lock().unwrap().insert(
            "owner".into(),
            SourceFailure::Availability("offline".into()),
        );
        let v = rt.query(query()).await.unwrap();
        assert_eq!(v["coverage"][0]["state"], "snapshot");
        assert_eq!(v["primary"][0]["source"]["generation"], "g1");
        assert!(f.calls.lock().unwrap().iter().any(|(u, _)| u == "replica"));
        f.calls.lock().unwrap().clear();
        f.errors.lock().unwrap().insert(
            "owner".into(),
            SourceFailure::Refused("unauthorized".into()),
        );
        let v = rt.query(query()).await.unwrap();
        assert_eq!(v["coverage"][0]["state"], "refused");
        assert!(!f.calls.lock().unwrap().iter().any(|(u, _)| u == "replica"));
        std::fs::remove_file(path).unwrap()
    }
    #[tokio::test]
    async fn get_pins_generation_and_rejects_expiry() {
        let (rt, _, path) = fixture();
        let g = LibraryGet {
            project_id: "p".into(),
            id: "node-1".into(),
            source_device_id: "mirror".into(),
            generation: Some("g1".into()),
        };
        assert_eq!(
            rt.get(g.clone()).await.unwrap()["source"]["generation"],
            "g1"
        );
        let mut cat: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        cat.entries[0].replicas.clear();
        std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
        assert!(rt.get(g).await.unwrap_err().to_string().contains("expired"));
        std::fs::remove_file(path).unwrap()
    }
    #[tokio::test]
    async fn withdrawn_is_not_queried_but_remains_in_catalog_file() {
        let (rt, f, path) = fixture();
        let mut cat: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        cat.entries[0].withdrawn = true;
        std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
        assert_eq!(
            rt.catalog().unwrap()["entries"].as_array().unwrap().len(),
            0
        );
        assert_eq!(
            rt.query(query()).await.unwrap()["primary"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert!(f.calls.lock().unwrap().is_empty());
        std::fs::remove_file(path).unwrap()
    }
    #[tokio::test]
    async fn known_busy_state_falls_back_and_preserves_reason() {
        let (rt, f, path) = fixture();
        f.states
            .lock()
            .unwrap()
            .insert("owner".into(), "snapshotting".into());
        let v = rt.query(query()).await.unwrap();
        assert_eq!(v["coverage"][0]["state"], "snapshot");
        assert!(
            v["coverage"][0]["owner_unavailable"]
                .as_str()
                .unwrap()
                .contains("snapshotting")
        );
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn source_disclosing_body_is_refused_without_replica_fallback() {
        let (rt, f, path) = fixture();
        *f.disclosed_body.lock().unwrap() = true;
        let v = rt.query(query()).await.unwrap();
        assert_eq!(v["coverage"][0]["state"], "refused");
        assert!(v["primary"].as_array().unwrap().is_empty());
        assert!(!f.calls.lock().unwrap().iter().any(|(u, _)| u == "replica"));
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn bounded_fanout_and_context() {
        let (rt, f, path) = fixture();
        let mut cat: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for i in 1..64 {
            let mut e = cat.entries[0].clone();
            e.project_id = format!("p{i}");
            cat.entries.push(e)
        }
        std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
        let v = rt.recall_context(query()).await.unwrap();
        assert_eq!(v["coverage"].as_array().unwrap().len(), 64);
        assert!(!v["primary"].as_array().unwrap().is_empty());
        assert_eq!(v["episodes"].as_array().unwrap().len(), 1);
        assert!(
            v["primary"].as_array().unwrap().len() + v["episodes"].as_array().unwrap().len()
                <= context::MAX_ITEMS
        );
        assert!(v["omitted"]["primary"].as_u64().unwrap() > 0);
        assert_eq!(v["context_truncated"], true);
        assert!(serde_json::to_vec(&v).unwrap().len() <= MAX_CONTEXT_BYTES);
        assert_eq!(
            f.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, tool)| tool == "recall_context")
                .count(),
            64
        );
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn same_device_two_projects_use_exact_db_id_routes() {
        let (mut rt, f, path) = fixture();
        let mut cat: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut second = cat.entries[0].clone();
        second.project_id = "p2".into();
        second.db_id = "db-2".into();
        second.replicas.clear();
        cat.entries.push(second);
        std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
        rt.config.owner_routes.insert(
            "owner".into(),
            BTreeMap::from([
                (
                    "db-1".into(),
                    Endpoint {
                        url: "route1".into(),
                        ssh_mcp_port: 18766,
                        token_env: None,
                    },
                ),
                (
                    "db-2".into(),
                    Endpoint {
                        url: "route2".into(),
                        ssh_mcp_port: 18766,
                        token_env: None,
                    },
                ),
            ]),
        );
        let v = rt.query(query()).await.unwrap();
        assert_eq!(v["coverage"].as_array().unwrap().len(), 2);
        assert!(
            v["coverage"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["state"] == "live")
        );
        let calls = f.calls.lock().unwrap();
        assert!(calls.iter().any(|(url, _)| url == "route1"));
        assert!(calls.iter().any(|(url, _)| url == "route2"));
        assert!(!calls.iter().any(|(url, _)| url == "owner"));
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn unrouted_live_source_is_visible_and_may_use_configured_replica() {
        let (mut rt, f, path) = fixture();
        rt.config.owners.clear();
        assert_eq!(
            rt.catalog().unwrap()["entries"][0]["live_route"],
            "unrouted"
        );
        let v = rt.query(query()).await.unwrap();
        assert_eq!(v["coverage"][0]["state"], "snapshot");
        assert_eq!(
            v["coverage"][0]["owner_unavailable"],
            "live route not configured"
        );
        assert!(
            !f.calls
                .lock()
                .unwrap()
                .iter()
                .any(|(url, _)| url == "owner")
        );
        rt.config.replicas.clear();
        let v = rt.query(query()).await.unwrap();
        assert_eq!(v["coverage"][0]["state"], "unrouted");
        assert!(v["primary"].as_array().unwrap().is_empty());
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn fallback_selects_newest_configured_replica_not_first_entry() {
        let (rt, f, path) = fixture();
        let mut cat: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut unconfigured = cat.entries[0].replicas[0].clone();
        unconfigured.source_device_id = "other_peer".into();
        unconfigured.generation = "older".into();
        unconfigured.captured_at -= 1;
        cat.entries[0].replicas.insert(0, unconfigured);
        std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
        f.errors.lock().unwrap().insert(
            "owner".into(),
            SourceFailure::Availability("offline".into()),
        );
        let v = rt.query(query()).await.unwrap();
        assert_eq!(v["coverage"][0]["state"], "snapshot");
        assert_eq!(v["primary"][0]["source"]["generation"], "g1");
        assert_eq!(v["primary"][0]["source"]["device_id"], "mirror");
        assert!(
            f.calls
                .lock()
                .unwrap()
                .iter()
                .any(|(url, _)| url == "replica")
        );
        std::fs::remove_file(path).unwrap();
    }
}
