use mneme_body::InlineStore;
use mneme_core::ports::{Budget, Clock, ColdPath};
use mneme_core::{EdgeKind, Node, NodeId, Provenance, Timestamp};
use mneme_cozo::MemStore;
use mneme_embed::HashingEmbedder;
use mneme_engine::{Config, DeterministicNodeIdSource, Ingest, Memory};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const DIMENSION: usize = 256;
pub const SEED: u64 = 20260906;
pub const NOW: Timestamp = 1_700_000_000_000;
struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        NOW
    }
}
pub struct PilotMemory {
    pub engine: Memory,
    pub condition: String,
    pub aliases: BTreeMap<String, NodeId>,
    ids: Vec<NodeId>,
    pub config: String,
}
fn card(node: &Node) -> Value {
    json!({"id":node.id(),"summary":node.summary(),"status":node.status(),"confidence":node.confidence()})
}
fn id(value: &Value) -> Result<NodeId, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("invalid node id: {e}"))
}
impl PilotMemory {
    pub fn new(condition: &str, episode: &str) -> Result<Self, String> {
        if !["notes_search", "hybrid_graph_off", "hybrid_authored_graph"].contains(&condition) {
            return Err("unknown condition".into());
        }
        let graph = condition == "hybrid_authored_graph";
        let config = Config {
            ann_k: 4,
            lexical_k: 4,
            candidate_admission_limit: 1,
            graph_seed_cap: if graph { 2 } else { 0 },
            graph_slot_cap: if graph { 2 } else { 0 },
            similarity_link_cap: 0,
            coretrieval_link_cap: 0,
            bridge_probability: 0.0,
            budget: Budget {
                max_nodes: 4,
                max_depth: 2,
                min_relevance: 0.0,
                explore: 0.0,
                relevance_ratio: 0.0,
                dedup_similarity: 1.0,
                query_conditioning: 0.0,
            },
            ..Config::default()
        };
        let store = Arc::new(MemStore::new(DIMENSION));
        let engine = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(HashingEmbedder::new(DIMENSION)),
            Arc::new(FixedClock),
            config,
        )
        .with_node_id_source(Arc::new(DeterministicNodeIdSource::new(
            SEED + u64::from(episode == "async"),
        )))
        .with_lexical_index(store)
        .with_body_store(Arc::new(InlineStore::new()));
        Ok(Self {
            engine,
            condition: condition.into(),
            aliases: BTreeMap::new(),
            ids: vec![],
            config: format!("{config:?}"),
        })
    }
    pub async fn call(&mut self, req: &Value) -> Result<Value, String> {
        match req["op"].as_str().unwrap_or("") {
            "search" => {
                let query = req["query"].as_str().ok_or("query required")?;
                if query.len() > 1024 {
                    return Err("query exceeds 1024 bytes".into());
                }
                if self.condition == "notes_search" {
                    let terms: Vec<String> = query
                        .split(|c: char| !c.is_alphanumeric())
                        .filter(|s| !s.is_empty())
                        .map(str::to_lowercase)
                        .collect();
                    let mut hits = Vec::new();
                    for node_id in &self.ids {
                        let node = self
                            .engine
                            .get_node(*node_id)
                            .await
                            .map_err(|e| e.to_string())?
                            .ok_or("node missing")?;
                        let body = self
                            .engine
                            .resolve_body(&node)
                            .await
                            .map_err(|e| e.to_string())?;
                        let text = format!("{} {}", node.summary(), String::from_utf8_lossy(&body))
                            .to_lowercase();
                        let score = terms
                            .iter()
                            .filter(|term| text.contains(term.as_str()))
                            .count();
                        if score > 0 || terms.is_empty() {
                            hits.push((score, *node_id, card(&node)));
                        }
                    }
                    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                    Ok(
                        json!({"hits":hits.into_iter().take(4).map(|(_,_,card)|card).collect::<Vec<_>>(),"method":"ordinary case-insensitive term search over summaries and bodies; empty query lists first four","body_read":"read by returned id"}),
                    )
                } else {
                    let batch = self
                        .engine
                        .retrieve_batch(query)
                        .await
                        .map_err(|e| e.to_string())?;
                    let mut hits = vec![];
                    for (lane, lane_hits) in [
                        ("primary", batch.primary),
                        ("probationary", batch.probationary),
                    ] {
                        for hit in lane_hits {
                            let mut c = card(&hit.node);
                            c["lane"] = json!(lane);
                            c["evidence"] = json!(format!("{:?}", hit.evidence));
                            hits.push(c);
                        }
                    }
                    Ok(
                        json!({"hits":hits,"method":"engine retrieve_batch; hashing reference embeddings","policy":format!("{:?}",batch.stamp)}),
                    )
                }
            }
            "read" => {
                let node = self
                    .engine
                    .get_node(id(&req["id"])?)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or("node not found")?;
                let body = self
                    .engine
                    .resolve_body(&node)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut c = card(&node);
                c["body"] = json!(String::from_utf8_lossy(&body));
                Ok(c)
            }
            "neighbors" => {
                let neighbors = self
                    .engine
                    .neighbors(id(&req["id"])?, 4)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(
                    json!({"neighbors":neighbors.iter().map(|n|format!("{n:?}")).collect::<Vec<_>>()}),
                )
            }
            "note" => {
                if self.ids.len() >= 64 {
                    return Err("pilot note cap is 64".into());
                }
                let summary = req["summary"].as_str().ok_or("summary required")?;
                let body = req["body"].as_str().ok_or("body required")?;
                if summary.len() > 512 || body.len() > 2048 {
                    return Err("note limit: 512 summary bytes, 2048 body bytes".into());
                }
                let alias = req["alias"].as_str();
                if alias.is_some_and(|a| self.aliases.contains_key(a)) {
                    return Err("alias already exists".into());
                }
                let node_id = self
                    .engine
                    .ingest(
                        Ingest::candidate(
                            summary,
                            body.as_bytes(),
                            &[],
                            Provenance::derived_empty(),
                        )
                        .active(),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                self.ids.push(node_id);
                if let Some(a) = alias {
                    self.aliases.insert(a.into(), node_id);
                }
                Ok(json!({"id":node_id,"active":true}))
            }
            "link" => {
                self.engine
                    .link(
                        id(&req["from"])?,
                        id(&req["to"])?,
                        EdgeKind::Associative,
                        0.7,
                        None,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(json!({"linked":true}))
            }
            "supersede" => {
                let winner = id(&req["winner"])?;
                let loser = id(&req["loser"])?;
                self.engine
                    .supersede(ColdPath::acquire(), winner, loser)
                    .await
                    .map_err(|e| e.to_string())?;
                let node = self
                    .engine
                    .get_node(loser)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or("loser missing")?;
                Ok(
                    json!({"superseded":true,"loser":card(&node),"meaning":"successor supersedes loser; loser remains readable and may appear in the probationary lane"}),
                )
            }
            _ => Err("unknown memory operation".into()),
        }
    }
    /// Frozen aliases refer only to IDs returned by earlier successful ingests.
    pub async fn replay_authorship(&mut self, requests: &[Value], graph: bool) -> Vec<Value> {
        let mut log = vec![];
        for authored in requests {
            let mut req = authored.clone();
            for key in ["from", "to", "winner", "loser"] {
                if let Some(alias) = req[key].as_str() {
                    match self.aliases.get(alias) {
                        Some(node_id) => req[key] = json!(node_id),
                        None => {
                            log.push(json!({"request":authored,"error":"authorship references unavailable prior alias"}));
                        }
                    }
                }
            }
            let result = match self.call(&req).await {
                Ok(v) => json!({"ok":true,"result":v}),
                Err(e) => json!({"ok":false,"error":e}),
            };
            log.push(json!({"request":authored,"resolved_request":req,"response":result,"graph_intervention":graph,"request_bytes":req.to_string().len(),"response_bytes":result.to_string().len()}));
        }
        log
    }
}
