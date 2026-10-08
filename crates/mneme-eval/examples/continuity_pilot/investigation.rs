//! Additive investigation fixtures and public discovery operations; v1 stays frozen.
use super::environment::{Environment, Fixture};
use super::session::Session;
use super::{assessment, digest};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Deserialize)]
struct Source {
    path: String,
    family: String,
    version: String,
    kind: String,
    body: String,
}
pub struct Investigation {
    pub condition: String,
    episode: String,
    sources: Vec<Source>,
}
fn fixture_text(episode: &str) -> &'static str {
    match episode {
        "diagnostics" => {
            include_str!("../../fixtures/continuity/investigation-v2/diagnostics.json")
        }
        "async" => include_str!("../../fixtures/continuity/investigation-v2/async.json"),
        _ => unreachable!("constructor validates episode"),
    }
}
fn source_text(episode: &str) -> &'static str {
    match episode {
        "diagnostics" => {
            include_str!("../../fixtures/continuity/investigation-v2/diagnostics-sources.json")
        }
        "async" => include_str!("../../fixtures/continuity/investigation-v2/async-sources.json"),
        _ => unreachable!("constructor validates episode"),
    }
}
pub fn protocol() -> Value {
    json!({"id":"continuity-investigation-v2-2026-09-06","actors":16,
        "conditions":["empty_memory","notes_search"],"phases":["learn","transfer","revise","return"],
        "decision_rule":{"notes_all_phases_pass_without_repeated_or_obsolete_proxy":true,
            "successful_control_required":true,"zero_control_discovery_is_no_saving":true,
            "minimum_later_discovery_reduction_per_sequence":0.25,
            "notes_whole_episode_attempted_calls_at_most_control_including_authorship":true},
        "report_separately":["admitted and rejected calls","authoring and later reuse","protocol bytes","actual actor inference usage"],
        "discovery_operations":["source_search","source_read","probe"],
        "freeze":"Fixture, source, assessment, executable and run protocol hashes before first actor; authored history hash at each phase boundary; retain failed and incomplete attempts.",
        "telemetry":"Runner inference, cost and elapsed time are unavailable; host usage is separate. Replay time is not retrieval latency."})
}
impl Investigation {
    pub fn new(episode: &str, condition: &str) -> Result<Self, String> {
        if !["diagnostics", "async"].contains(&episode) {
            return Err("episode must be diagnostics or async".into());
        }
        if !["empty_memory", "notes_search"].contains(&condition) {
            return Err("investigation condition must be empty_memory or notes_search".into());
        }
        Ok(Self {
            episode: episode.into(),
            condition: condition.into(),
            sources: serde_json::from_str(source_text(episode)).map_err(|e| e.to_string())?,
        })
    }
    pub fn fixture(&self) -> Result<Fixture, String> {
        serde_json::from_str(fixture_text(&self.episode)).map_err(|e| e.to_string())
    }
    pub fn fixture_hash(&self) -> Value {
        json!(digest(
            &json!({"fixture":fixture_text(&self.episode),"sources":source_text(&self.episode),"assessment":assessment(),"protocol":protocol()})
        ))
    }
    fn available<'a>(&'a self, environment: &Environment) -> Vec<&'a Source> {
        self.sources
            .iter()
            .filter(|s| {
                environment.phase.components.iter().any(|c| {
                    c.source
                        .as_deref()
                        .and_then(|path| path.rsplit_once('/'))
                        .is_some_and(|(directory, _)| {
                            s.path
                                .rsplit_once('/')
                                .is_some_and(|(parent, _)| directory == parent)
                        })
                })
            })
            .collect()
    }
    pub fn packet(&self, environment: &Environment, packet: &mut Value) {
        packet["variant"] = json!("investigation_v2");
        packet["condition"] = json!(self.condition);
        packet["components"] = json!(environment.phase.components.iter().map(|c| {
            let source = self.sources.iter().find(|s| Some(&s.path) == c.source.as_ref()).expect("fixture entry source exists");
            json!({"name":c.name,"dependency_family":source.family,"version":source.version,"source_path":source.path})
        }).collect::<Vec<_>>());
        packet["discovery_operations"] = json!({
            "source_search":"{op:source_search, query:TEXT}; search current installed source/contract text; empty query lists every current artifact. Returns paths, not bodies.",
            "source_read":"{op:source_read, path:PATH}; read a current source/contract artifact. Entry source paths are listed on components.",
            "probe":if self.episode == "diagnostics" {
                "{op:probe, component:NAME, initial_present:BOOL}; invoke its opener on a disposable copy, returning before/after observations. Does not affect actual task storage or satisfy task observation requirements."
            } else {
                "{op:probe, component:NAME, mode:direct|worker}; begin, tick and complete an isolated disposable fetch, returning its scheduling observations. Does not affect actual task work or satisfy task observation requirements."
            }
        });
        packet["memory_operations"] = json!({
            "search":"{op:search, query:TEXT}; up to four summary cards; searches summaries and bodies; empty query lists first four notes",
            "read":"{op:read, id:RETURNED_ID}; full body and status",
            "finish":"{op:finish, rationale:TEXT, used_memory_ids:[ID,...]}; conclude phase without revealing grade",
            "re_explain":"{op:re_explain}; records an unanswered human clarification request"
        });
        packet["authorship_mode"] = json!(if self.condition == "notes_search" {
            "natural actor notes in learn/revise; no supplied notes"
        } else {
            "empty episode memory; no note authoring"
        });
        if self.condition == "notes_search"
            && ["learn", "revise"].contains(&environment.phase.name.as_str())
        {
            packet["memory_operations"]["note"] = json!(
                "{op:note, summary:TEXT, body:TEXT}; optional checkpoint from what you learned; 512 summary and 2048 body bytes; charged to this phase"
            );
            packet["memory_operations"]["supersede"] = json!(
                "{op:supersede, winner:NEW_ID, loser:OLD_ID}; optional correction after a separate successor note; old note stays readable and searchable; preserve version scope"
            );
            packet["checkpoint"] = json!(
                "Consider leaving a useful, scoped checkpoint for a successor. If experience changed, correct it through a successor note and supersede. Author naturally from current evidence; no note is required for task correctness."
            );
        }
    }
    pub fn allow_memory(&self, phase: &str, req: &Value) -> Result<(), String> {
        match req["op"].as_str().unwrap_or("") {
            "link" | "neighbors" => {
                Err("investigation has no graph condition or graph operations".into())
            }
            "note" | "supersede" if self.condition == "empty_memory" => {
                Err("empty-memory condition does not retain actor notes".into())
            }
            "note" | "supersede" if !["learn", "revise"].contains(&phase) => Err(
                "authored history is frozen during transfer and return; author in learn/revise"
                    .into(),
            ),
            _ => Ok(()),
        }
    }
    pub fn discover(&self, environment: &Environment, req: &Value) -> Result<Value, String> {
        match req["op"].as_str().unwrap_or("") {
            "source_search" => {
                let query = req["query"].as_str().ok_or("query required")?;
                if query.len() > 1024 {
                    return Err("query exceeds 1024 bytes".into());
                }
                let terms: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
                let hits: Vec<_> = self.available(environment).into_iter().filter(|s| {
                    let text = format!("{} {}",s.path,s.body).to_lowercase();
                    terms.iter().all(|term| text.contains(term))
                }).map(|s|json!({"path":s.path,"family":s.family,"version":s.version,"kind":s.kind})).collect();
                Ok(json!({"hits":hits,"scope":"only current installed versions; no truncation"}))
            }
            "source_read" => {
                let path = req["path"].as_str().ok_or("path required")?;
                let source = self
                    .available(environment)
                    .into_iter()
                    .find(|s| s.path == path)
                    .ok_or("source unavailable in this phase")?;
                Ok(
                    json!({"path":source.path,"family":source.family,"version":source.version,"kind":source.kind,"body":source.body,"sha256":digest(&json!(source.body))}),
                )
            }
            "probe" => {
                let name = req["component"].as_str().ok_or("component required")?;
                let mut component = environment
                    .phase
                    .components
                    .iter()
                    .find(|c| c.name == name)
                    .ok_or("component unavailable in this phase")?
                    .clone();
                if self.episode == "diagnostics" {
                    component.initial_present = req["initial_present"]
                        .as_bool()
                        .ok_or("initial_present boolean required")?;
                }
                let mut phase = environment.phase.clone();
                phase.components = vec![component];
                let mut probe = Environment::new(&self.episode, phase);
                let observations = if self.episode == "diagnostics" {
                    vec![
                        probe.act(&json!({"action":"inspect","component":name}))?,
                        probe.act(&json!({"action":"open","component":name}))?,
                        probe.act(&json!({"action":"inspect","component":name}))?,
                    ]
                } else {
                    vec![
                        probe
                            .act(&json!({"action":"begin","component":name,"mode":req["mode"]}))?,
                        probe.act(&json!({"action":"tick","component":name}))?,
                        probe.act(&json!({"action":"complete","component":name}))?,
                    ]
                };
                Ok(json!({"disposable":true,"observations":observations,"trace":probe.trace}))
            }
            _ => Err("unknown discovery operation".into()),
        }
    }
}
pub fn operation_counts(log: &[Value]) -> Value {
    let mut counts: BTreeMap<&str, Value> = BTreeMap::new();
    for entry in log {
        let category = match entry["request"]["op"].as_str().unwrap_or("") {
            "source_search" | "source_read" | "probe" => "discovery",
            "search" | "read" | "neighbors" => "memory_access",
            "note" | "supersede" | "link" => "authorship",
            "task" => "task",
            _ => "protocol",
        };
        let count = counts.entry(category).or_insert_with(||json!({"attempted":0,"admitted":0,"rejected":0,"failed_admitted":0,"request_bytes":0,"response_bytes":0}));
        for (key, amount) in [
            ("attempted", 1),
            ("admitted", u64::from(entry["admitted"] != false)),
            ("rejected", u64::from(entry["admitted"] == false)),
            (
                "failed_admitted",
                u64::from(entry["admitted"] != false && entry["response"]["ok"] != true),
            ),
            ("request_bytes", entry["request_bytes"].as_u64().unwrap()),
            ("response_bytes", entry["response_bytes"].as_u64().unwrap()),
        ] {
            count[key] = json!(count[key].as_u64().unwrap() + amount);
        }
    }
    json!(counts)
}

/// A plumbing rehearsal, not an actor simulation: decisions use public versioned
/// contracts read via the protocol. Scripted notes are never supplied to real runs.
pub async fn rehearse() -> Result<Value, String> {
    let mut episodes = vec![];
    for episode in ["diagnostics", "async"] {
        for condition in ["empty_memory", "notes_search"] {
            let mut session = Session::new_investigation(episode, condition, "scripted_rehearsal")?;
            for phase_index in 0..4 {
                scripted_phase(&mut session).await?;
                if phase_index < 3 {
                    require(session.request(json!({"control":"advance"})).await)?;
                }
            }
            episodes.push(session.artifact());
        }
    }
    let mut artifact = json!({"schema":"continuity_investigation_rehearsal_v2","actor_kind":"scripted_rehearsal","protocol":protocol(),"episodes":episodes});
    artifact["semantic_digest"] = json!(digest(&artifact));
    Ok(artifact)
}
fn require(response: Value) -> Result<Value, String> {
    if response["ok"] == true {
        Ok(response["result"].clone())
    } else {
        Err(response.to_string())
    }
}
async fn scripted_phase(session: &mut Session) -> Result<(), String> {
    let packet = require(session.request(json!({"op":"packet"})).await)?;
    let phase = packet["phase"].as_str().unwrap();
    let episode = packet["episode"].as_str().unwrap();
    let notes = packet["condition"] == "notes_search";
    let mut known = BTreeMap::<String, Value>::new();
    let mut used = vec![];
    if notes && phase != "learn" {
        let hits = require(
            session
                .request(json!({"op":"search","query":"adapter contract"}))
                .await,
        )?;
        for hit in hits["hits"].as_array().unwrap() {
            let read = require(session.request(json!({"op":"read","id":hit["id"]})).await)?;
            let remembered: BTreeMap<String, Value> =
                serde_json::from_str(read["body"].as_str().unwrap()).map_err(|e| e.to_string())?;
            known.extend(remembered);
            used.push(hit["id"].clone());
        }
    }
    for component in packet["components"].as_array().unwrap() {
        let path = component["source_path"].as_str().unwrap();
        let version_key = path.rsplit_once('/').unwrap().0.to_owned();
        if !known.contains_key(&version_key) {
            let source = require(
                session
                    .request(json!({"op":"source_read","path":path}))
                    .await,
            )?;
            let contract_path = source["body"]
                .as_str()
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("// Dependency contract: "))
                .ok_or("adapter contract reference missing")?;
            let contract = require(
                session
                    .request(json!({"op":"source_read","path":contract_path}))
                    .await,
            )?;
            known.insert(
                version_key.clone(),
                serde_json::from_str(contract["body"].as_str().unwrap())
                    .map_err(|e| e.to_string())?,
            );
        }
        let contract = &known[&version_key];
        let name = &component["name"];
        let report = if episode == "diagnostics" {
            if phase == "learn"
                || contract["absent_storage"] == "return missing without creating storage"
            {
                require(
                    session
                        .request(json!({"op":"task","action":"open","component":name}))
                        .await,
                )?["result"]
                    .clone()
            } else {
                let inspection = require(
                    session
                        .request(json!({"op":"task","action":"inspect","component":name}))
                        .await,
                )?;
                if inspection["present"] == true {
                    require(
                        session
                            .request(json!({"op":"task","action":"open","component":name}))
                            .await,
                    )?["result"]
                        .clone()
                } else {
                    json!("missing")
                }
            }
        } else {
            let mode = if phase != "learn"
                && contract["pending_direct_call"]
                    == "blocks shared-loop heartbeat until completion"
            {
                "worker"
            } else {
                "direct"
            };
            require(
                session
                    .request(json!({"op":"task","action":"begin","component":name,"mode":mode}))
                    .await,
            )?;
            let observation = require(
                session
                    .request(json!({"op":"task","action":"tick","component":name}))
                    .await,
            )?;
            require(
                session
                    .request(json!({"op":"task","action":"complete","component":name}))
                    .await,
            )?;
            observation["heartbeat"].clone()
        };
        require(
            session
                .request(json!({"op":"task","action":"report","component":name,"value":report}))
                .await,
        )?;
    }
    if notes && ["learn", "revise"].contains(&phase) {
        let created = require(session.request(json!({"op":"note","summary":"Adapter contract observations scoped by version","body":serde_json::to_string(&known).unwrap()})).await)?;
        if phase == "revise" {
            for old in &used {
                require(
                    session
                        .request(json!({"op":"supersede","winner":created["id"],"loser":old}))
                        .await,
                )?;
            }
        }
    }
    require(session.request(json!({"op":"finish","rationale":"Scripted rehearsal applied current source contracts or matching versioned checkpoint observations; no private assessment was read.","used_memory_ids":used})).await)?;
    Ok(())
}
