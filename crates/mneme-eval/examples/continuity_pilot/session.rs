use super::environment::{Environment, Fixture};
use super::memory::{DIMENSION, NOW, PilotMemory, SEED};
use super::{assessment, authorship, digest, fixture, investigation};
use serde_json::{Value, json};

pub const MAX_CALLS: usize = 32;
pub const MAX_CONTEXT_BYTES: usize = 65536;
pub const MAX_RESPONSE_BYTES: usize = 8192;
pub struct Session {
    fixture: Fixture,
    pub memory: PilotMemory,
    pub environment: Environment,
    phase: usize,
    pub finished: bool,
    pub calls: usize,
    pub request_bytes: usize,
    pub response_bytes: usize,
    log: Vec<Value>,
    completed: Vec<Value>,
    authored: Value,
    authored_log: Vec<Value>,
    pub independent: bool,
    actor_kind: String,
    re_explanations: usize,
    rejected_calls: usize,
    rejected_request_bytes: usize,
    rejected_response_bytes: usize,
    exhausted: bool,
    investigation: Option<investigation::Investigation>,
    natural_history: Vec<Value>,
    phase_start_history_hash: String,
}
impl Session {
    pub fn new(
        episode: &str,
        condition: &str,
        independent: bool,
        actor_kind: &str,
    ) -> Result<Self, String> {
        let fixture = fixture(episode)?;
        let environment = Environment::new(episode, fixture.phases[0].clone());
        Ok(Self {
            fixture,
            memory: PilotMemory::new(condition, episode)?,
            environment,
            phase: 0,
            finished: false,
            calls: 0,
            request_bytes: 0,
            response_bytes: 0,
            log: vec![],
            completed: vec![],
            authored: authorship(episode),
            authored_log: vec![],
            independent,
            actor_kind: actor_kind.into(),
            re_explanations: 0,
            rejected_calls: 0,
            rejected_request_bytes: 0,
            rejected_response_bytes: 0,
            exhausted: false,
            investigation: None,
            natural_history: vec![],
            phase_start_history_hash: digest(&json!([])),
        })
    }
    pub fn new_investigation(
        episode: &str,
        condition: &str,
        actor_kind: &str,
    ) -> Result<Self, String> {
        let investigation = investigation::Investigation::new(episode, condition)?;
        let mut session = Self::new(episode, "notes_search", true, actor_kind)?;
        session.fixture = investigation.fixture()?;
        session.environment = Environment::new(episode, session.fixture.phases[0].clone());
        session.investigation = Some(investigation);
        Ok(session)
    }
    pub async fn request(&mut self, req: Value) -> Value {
        if req.get("control").is_some() {
            return self.control(&req).await;
        }
        let request_len = req.to_string().len();
        if self.calls >= MAX_CALLS
            || self.request_bytes + self.response_bytes + request_len + MAX_RESPONSE_BYTES
                > MAX_CONTEXT_BYTES
        {
            // Reserve maximum response space before effects; rejected calls have no effects.
            let response = json!({"ok":false,"error":"phase budget exhausted; coordinator may advance with force:true","budget_rejected":true});
            self.rejected_calls += 1;
            self.rejected_request_bytes += request_len;
            self.rejected_response_bytes += response.to_string().len();
            self.exhausted = true;
            self.log.push(json!({"request":req,"response":response,"admitted":false,"request_bytes":request_len,"response_bytes":response.to_string().len()}));
            return response;
        }
        self.calls += 1;
        self.request_bytes += request_len;
        let result = self.actor(&req).await;
        let mut response = match result {
            Ok(v) => json!({"ok":true,"result":v}),
            Err(e) => json!({"ok":false,"error":e}),
        };
        if response.to_string().len() > MAX_RESPONSE_BYTES {
            response = json!({"ok":false,"error":"response exceeds per-call envelope; operation executed, output omitted","output_omitted":true});
        }
        let response_len = response.to_string().len();
        self.response_bytes += response_len;
        let entry = json!({"request":req,"response":response,"request_bytes":request_len,"response_bytes":response_len});
        if self.investigation.is_some()
            && response["ok"] == true
            && matches!(req["op"].as_str(), Some("note" | "supersede"))
        {
            self.natural_history
                .push(json!({"phase":self.environment.phase.name,"operation":entry}));
        }
        self.log.push(entry);
        response
    }
    async fn actor(&mut self, req: &Value) -> Result<Value, String> {
        if self.finished {
            return Err("phase finished; await coordinator transition".into());
        }
        match req["op"].as_str().unwrap_or("") {
            "packet"=>{
                let mut packet=self.environment.packet();
                packet["episode"]=json!(self.fixture.episode);
                packet["authorship_mode"]=json!(if self.independent {"independent actor authorship"} else {"controlled frozen fixture authorship"});
                packet["memory_operations"]=json!({"search":"{op:search, query:TEXT}; up to four summary cards, iterative searches allowed; notes empty query lists","read":"{op:read, id:RETURNED_ID}; body and status","neighbors":"{op:neighbors, id:RETURNED_ID}; bounded links","note":"{op:note, summary:TEXT, body:TEXT}; independent authorship only","link":"{op:link, from:ID, to:ID}; independent authorship only","supersede":"{op:supersede, winner:SUCCESSOR_ID, loser:OLD_ID}; independent authorship only; requires separate successor note","finish":"{op:finish, rationale:TEXT, used_memory_ids:[ID,...]}; conclude phase, no grade returned","re_explain":"{op:re_explain}; records request for human explanation; no human answer is fabricated"});
                packet["budget"]=json!({"total_calls_including_packet":MAX_CALLS,"total_request_and_response_bytes":MAX_CONTEXT_BYTES,"max_response_bytes":MAX_RESPONSE_BYTES,"calls_before_this_response":self.calls,"consumed_bytes_before_this_response":self.request_bytes+self.response_bytes,"admission":"reserves maximum response bytes before executing; per-phase context resets","whole_episode_max_calls":4*MAX_CALLS,"whole_episode_max_context_bytes":4*MAX_CONTEXT_BYTES});
                packet["report_requirement"]=json!("Use actual task observations to support reports. Each new phase uses a fresh actor context; only memory persists.");
                if let Some(investigation) = &self.investigation {
                    investigation.packet(&self.environment, &mut packet);
                }
                Ok(packet)
            },
            "source_search"|"source_read"|"probe" if self.investigation.is_some()=>self.investigation.as_ref().unwrap().discover(&self.environment,req),
            "task"=>self.environment.act(req),
            "finish"=>{self.finished=true;Ok(json!({"phase_finished":true}))},
            "re_explain"=>{self.re_explanations+=1;Ok(json!({"requested":true,"available":false,"reason":"pilot has no human-answer provider"}))},
            "note"|"link"|"supersede" if !self.independent=>Err("controlled comparison: frozen authorship is replayed by coordinator; use --independent-authorship for a separate authorship experiment".into()),
            "search"|"read"|"neighbors"|"note"|"link"|"supersede"=>{
                if let Some(investigation) = &self.investigation {
                    investigation.allow_memory(&self.environment.phase.name, req)?;
                }
                self.memory.call(req).await
            },
            "feedback"=>Err("ordinary search returns no feedback receipt; this pilot does not invent feedback authority".into()),
            _=>Err("unknown actor operation".into()),
        }
    }
    fn phase_artifact(&self) -> Value {
        let mut artifact = json!({"phase":self.environment.phase.name,"actor_kind":self.actor_kind,"finished":self.finished,"budget_exhausted":self.exhausted,"phase_passed":self.finished && self.environment.grade(&assessment())["passed"] == true,"assessment":self.environment.grade(&assessment()),"actor_requests":self.log,"budget":{"calls":self.calls,"attempted_calls":self.calls+self.rejected_calls,"rejected_calls":self.rejected_calls,"rejected_request_bytes":self.rejected_request_bytes,"rejected_response_bytes":self.rejected_response_bytes,"actual_total_wire_bytes":self.request_bytes+self.response_bytes+self.rejected_request_bytes+self.rejected_response_bytes,"request_bytes":self.request_bytes,"response_bytes":self.response_bytes,"total_context_bytes":self.request_bytes+self.response_bytes},"re_explanation_requests":self.re_explanations});
        if self.investigation.is_some() {
            artifact["operation_counts"] = investigation::operation_counts(&self.log);
            artifact["authored_history_at_start"] = json!(self.phase_start_history_hash);
            artifact["authored_history_at_end"] = json!(digest(&json!(self.natural_history)));
        }
        artifact
    }
    async fn control(&mut self, req: &Value) -> Value {
        match req["control"].as_str().unwrap_or("") {
            "grade" => json!({"ok":true,"result":self.phase_artifact()}),
            "artifact" => json!({"ok":true,"result":self.artifact()}),
            "advance" => {
                if !self.finished && req["force"] != true {
                    return json!({"ok":false,"error":"actor must finish before coordinator advance; coordinator may force:true to record an incomplete phase"});
                }
                if self.phase >= 3 {
                    return json!({"ok":false,"error":"final phase; request artifact"});
                }
                self.completed.push(self.phase_artifact());
                if !self.independent {
                    let key = format!("after_{}", self.environment.phase.name);
                    let common = self.authored[&key].as_array().cloned().unwrap_or_default();
                    let common_log = self.memory.replay_authorship(&common, false).await;
                    self.authored_log.push(json!({"after_phase":self.environment.phase.name,"kind":"common_frozen_inputs","operations":common_log}));
                    if self.memory.condition == "hybrid_authored_graph" {
                        let graph_key = format!("graph_after_{}", self.environment.phase.name);
                        let graph = self.authored[&graph_key]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                        let graph_log = self.memory.replay_authorship(&graph, true).await;
                        self.authored_log.push(json!({"after_phase":self.environment.phase.name,"kind":"graph_intervention","operations":graph_log}));
                    }
                }
                self.phase += 1;
                self.phase_start_history_hash = digest(&json!(self.natural_history));
                self.environment = Environment::new(
                    &self.fixture.episode,
                    self.fixture.phases[self.phase].clone(),
                );
                self.finished = false;
                self.calls = 0;
                self.request_bytes = 0;
                self.response_bytes = 0;
                self.re_explanations = 0;
                self.rejected_calls = 0;
                self.rejected_request_bytes = 0;
                self.rejected_response_bytes = 0;
                self.exhausted = false;
                self.log.clear();
                json!({"ok":true,"result":{"advanced":true,"phase":self.environment.phase.name,"reset_actor_context":true}})
            }
            _ => json!({"ok":false,"error":"unknown coordinator control"}),
        }
    }
    pub fn artifact(&self) -> Value {
        let mut phases = self.completed.clone();
        phases.push(self.phase_artifact());
        let mut common = self.authored.clone();
        common
            .as_object_mut()
            .unwrap()
            .retain(|k, _| !k.starts_with("graph_"));
        let source = if self.fixture.episode == "diagnostics" {
            include_str!("../../fixtures/continuity/diagnostics.json")
        } else {
            include_str!("../../fixtures/continuity/async.json")
        };
        let mut artifact = json!({"schema":"continuity_pilot_v1","episode":self.fixture.episode,"condition":self.memory.condition,"actor_kind":self.actor_kind,"authorship_mode":if self.independent {"independent"} else {"controlled_frozen"},
            "fixture_hash":digest(&json!({"public":source,"authorship":self.authored,"assessment":assessment()})),"common_authorship_hash":digest(&common),
            "configuration":{"store":"independent MemStore per episode and condition","embedder":"HashingEmbedder","dimension":DIMENSION,"fixed_clock":NOW,"seed":SEED+u64::from(self.fixture.episode=="async"),"engine":self.memory.config,"max_calls_per_phase":MAX_CALLS,"max_context_bytes_per_phase":MAX_CONTEXT_BYTES,"max_response_bytes":MAX_RESPONSE_BYTES},
            "phases":phases,"authorship_operations":self.authored_log,
            "measurements":{"task_outcomes":"deterministic operation/state model, not application performance","elapsed_time":null,"model_inference_tokens":null,"provider_cost":null,"natural_authorship_effort":null,"controlled_authorship_cost":"exact synthetic replay requests and bytes recorded separately from actor budgets","memory_use":"finish used_memory_ids and rationale are self-reports; exposure and action consequences are distinct","obsolete_workaround_proxy":"unnecessary worker dispatch or omitted new opener; does not establish memory caused the action"},
            "claims":{"scripted_is_product_evidence":false,"hashing_is_semantic_quality_evidence":false,"graph_must_win":false}});
        if let Some(investigation) = &self.investigation {
            artifact["schema"] = json!("continuity_investigation_v2");
            artifact["condition"] = json!(investigation.condition);
            artifact["fixture_hash"] = investigation.fixture_hash();
            artifact
                .as_object_mut()
                .unwrap()
                .remove("common_authorship_hash");
            artifact["authorship_operations"] = json!(self.natural_history);
            artifact["authored_history_hash"] = json!(digest(&json!(self.natural_history)));
            artifact["measurements"]["natural_authorship_effort"] = json!(
                "Successful and failed actor note/supersede calls and bytes are included in phase budgets; inference requires separate host telemetry."
            );
            artifact["measurements"]
                .as_object_mut()
                .unwrap()
                .remove("controlled_authorship_cost");
            artifact["protocol"] = investigation::protocol();
        }
        let hash = digest(&artifact);
        artifact["semantic_digest"] = json!(hash);
        artifact
    }
}

/// The rehearsal exercises the actor protocol using only current packet, actual
/// search/read results and task observations. It never receives assessment data.
async fn scripted_phase(session: &mut Session) -> Result<(), String> {
    let packet = session.request(json!({"op":"packet"})).await;
    let public = &packet["result"];
    let phase = public["phase"]
        .as_str()
        .ok_or("packet unavailable")?
        .to_owned();
    let episode = public["episode"].as_str().unwrap().to_owned();
    let components = public["components"].as_array().unwrap().clone();
    let mut notes = String::new();
    let mut used = vec![];
    if phase != "learn" {
        let query = if episode == "diagnostics" {
            "backend diagnostics opening"
        } else {
            "synchronous heartbeat scheduling"
        };
        let response = session.request(json!({"op":"search","query":query})).await;
        for hit in response["result"]["hits"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let read = session.request(json!({"op":"read","id":hit["id"]})).await;
            notes.push_str(read["result"]["body"].as_str().unwrap_or(""));
            used.push(hit["id"].clone());
        }
    }
    for c in components {
        let name = &c["name"];
        let api = c["api"].as_str().unwrap();
        let report = if episode == "diagnostics" {
            if phase == "learn" || api == "inspect_existing" {
                let observed = session
                    .request(json!({"op":"task","action":"open","component":name}))
                    .await;
                observed["result"]["result"]
                    .as_str()
                    .unwrap_or("missing")
                    .to_owned()
            } else if notes.contains("created") {
                let observed = session
                    .request(json!({"op":"task","action":"inspect","component":name}))
                    .await;
                if observed["result"]["present"] == true {
                    session
                        .request(json!({"op":"task","action":"open","component":name}))
                        .await;
                    "usable".into()
                } else {
                    "missing".into()
                }
            } else {
                let observed = session
                    .request(json!({"op":"task","action":"open","component":name}))
                    .await;
                observed["result"]["result"]
                    .as_str()
                    .unwrap_or("missing")
                    .to_owned()
            }
        } else {
            let mode = if phase != "learn" && api == "synchronous" && notes.contains("worker") {
                "worker"
            } else {
                "direct"
            };
            session
                .request(json!({"op":"task","action":"begin","component":name,"mode":mode}))
                .await;
            let observed = session
                .request(json!({"op":"task","action":"tick","component":name}))
                .await;
            session
                .request(json!({"op":"task","action":"complete","component":name}))
                .await;
            observed["result"]["heartbeat"]
                .as_str()
                .unwrap_or("blocked")
                .into()
        };
        session
            .request(json!({"op":"task","action":"report","component":name,"value":report}))
            .await;
    }
    session.request(json!({"op":"finish","used_memory_ids":used,"rationale":"scripted_rehearsal policy used current API names, retrieved body text and task observations"})).await;
    Ok(())
}
pub async fn rehearse() -> Result<Value, String> {
    let mut episodes = vec![];
    for episode in ["diagnostics", "async"] {
        for condition in ["notes_search", "hybrid_graph_off", "hybrid_authored_graph"] {
            let mut session = Session::new(episode, condition, false, "scripted_rehearsal")?;
            for phase in 0..4 {
                scripted_phase(&mut session).await?;
                if phase < 3 {
                    let response = session.request(json!({"control":"advance"})).await;
                    if response["ok"] != true {
                        return Err(response.to_string());
                    }
                }
            }
            episodes.push(session.artifact());
        }
    }
    let mut artifact = json!({"schema":"continuity_pilot_rehearsal_v1","actor_kind":"scripted_rehearsal","episodes":episodes});
    artifact["semantic_digest"] = json!(digest(&artifact));
    Ok(artifact)
}
