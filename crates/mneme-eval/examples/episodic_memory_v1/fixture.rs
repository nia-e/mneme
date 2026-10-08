use super::{Result, digest, require};
use mneme_app::{capture::PreparedCapture, episode::PreparedEpisode};
use mneme_body::InlineStore;
use mneme_core::ports::{Budget, Clock, ColdPath, StatusFilter};
use mneme_core::{NodeId, Timestamp};
use mneme_cozo::MemStore;
use mneme_embed::HashingEmbedder;
use mneme_engine::{Config, Memory};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const DIMENSION: usize = 256;
const CORPUS: &str = include_str!("../../fixtures/episodic-v1/corpus.json");
const ASSESSMENT: &str = include_str!("../../fixtures/episodic-v1/assessment.json");
const DATABASE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

pub fn corpus() -> Value {
    serde_json::from_str(CORPUS).unwrap()
}
pub fn assessment() -> Value {
    serde_json::from_str(ASSESSMENT).unwrap()
}
pub fn fingerprint() -> String {
    digest(CORPUS.as_bytes())
}
pub fn parse_id(value: &Value) -> Result<NodeId> {
    Ok(serde_json::from_value(value.clone())?)
}

#[derive(Default)]
pub struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        self.0.load(Ordering::SeqCst).into()
    }
}
impl TestClock {
    pub fn set(&self, value: u64) {
        self.0.store(value, Ordering::SeqCst);
    }
}

pub struct Fixture {
    pub memory: Memory,
    pub store: Arc<MemStore>,
    pub bodies: Arc<InlineStore>,
    pub clock: Arc<TestClock>,
    pub aliases: BTreeMap<String, NodeId>,
    pub writes: BTreeMap<String, Value>,
    pub trace: Vec<Value>,
}

pub fn memory(store: Arc<MemStore>, bodies: Arc<InlineStore>, clock: Arc<TestClock>) -> Memory {
    memory_with_config(store, bodies, clock, config())
}

pub fn config() -> Config {
    Config {
        ann_k: 3,
        lexical_k: 3,
        candidate_admission_limit: 1,
        graph_seed_cap: 3,
        graph_slot_cap: 3,
        similarity_link_cap: 0,
        coretrieval_link_cap: 0,
        bridge_probability: 0.0,
        budget: Budget {
            max_nodes: 8,
            max_depth: 2,
            min_relevance: 0.0,
            explore: 0.0,
            relevance_ratio: 0.0,
            dedup_similarity: 1.0,
            query_conditioning: 0.0,
        },
        ..Config::default()
    }
}

pub fn memory_with_config(
    store: Arc<MemStore>,
    bodies: Arc<InlineStore>,
    clock: Arc<TestClock>,
    config: Config,
) -> Memory {
    Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(DIMENSION)),
        clock,
        config,
    )
    .with_lexical_index(store)
    .with_body_store(bodies)
}

impl Fixture {
    pub async fn load() -> Result<Self> {
        let corpus = corpus();
        let mut empty = MemStore::new(DIMENSION).export();
        empty.db_id = parse_id(&json!(DATABASE))?.0;
        let store = Arc::new(MemStore::from_export(empty)?);
        let bodies = Arc::new(InlineStore::new());
        let clock = Arc::new(TestClock::default());
        let mut this = Self {
            memory: memory(store.clone(), bodies.clone(), clock.clone()),
            store,
            bodies,
            clock,
            aliases: BTreeMap::new(),
            writes: BTreeMap::new(),
            trace: Vec::new(),
        };
        for step in corpus["schedule"].as_array().ok_or("schedule missing")? {
            this.clock
                .set(step["at_ms"].as_u64().ok_or("schedule time missing")?);
            let op = step["op"].as_str().ok_or("schedule operation missing")?;
            if op == "supersede_semantic" {
                this.memory
                    .supersede(
                        ColdPath::acquire(),
                        this.id(step["winner"].as_str().unwrap())?,
                        this.id(step["loser"].as_str().unwrap())?,
                    )
                    .await?;
                this.trace
                    .push(json!({"step":step,"result":{"superseded":true}}));
                continue;
            }
            let alias = step["alias"].as_str().ok_or("scheduled alias missing")?;
            let lane = match op {
                "append_episode" => "episodes",
                "capture_semantic" => "semantic_notes",
                "revise_episode" => "editorial_revisions",
                _ => return Err("unknown fixture operation".into()),
            };
            let definition = corpus[lane]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["alias"] == alias)
                .ok_or("definition missing")?;
            let mut raw = definition.clone();
            let object = raw.as_object_mut().ok_or("definition is not object")?;
            object.remove("alias");
            if let Some(links) = object.get_mut("links").and_then(Value::as_array_mut) {
                for link in links {
                    link["to"] = json!(this.id(link["to"].as_str().ok_or("link alias missing")?)?);
                }
            }
            let result = if op == "capture_semantic" {
                let result = PreparedCapture::parse(&raw)?
                    .run(&this.memory, None)
                    .await?;
                json!({"id":result.id,"replayed":result.replayed})
            } else {
                let occurrence = raw
                    .as_object_mut()
                    .unwrap()
                    .remove("occurrence")
                    .ok_or("occurrence missing")?;
                raw["occurred"] = json!({"kind":"range","start":occurrence["start_ms"],"end":occurrence["end_ms"]});
                raw["action"] = json!(if op == "append_episode" {
                    "append"
                } else {
                    "revise"
                });
                if op == "revise_episode" {
                    let root = raw.as_object_mut().unwrap().remove("root").unwrap();
                    let expected = raw
                        .as_object_mut()
                        .unwrap()
                        .remove("expected_edition")
                        .unwrap();
                    raw["episode_id"] = json!(this.id(root.as_str().unwrap())?);
                    raw["expected_edition_id"] = json!(this.id(expected.as_str().unwrap())?);
                }
                this.episode(raw.clone()).await?
            };
            let id = parse_id(if op == "capture_semantic" {
                &result["id"]
            } else {
                &result["edition_id"]
            })?;
            require(
                this.aliases.insert(alias.into(), id).is_none(),
                "duplicate alias",
            )?;
            this.writes.insert(alias.into(), raw);
            this.trace.push(json!({"step":step,"result":result}));
        }
        this.clock.set(corpus["as_of_ms"].as_u64().unwrap());
        Ok(this)
    }

    pub fn id(&self, alias: &str) -> Result<NodeId> {
        self.aliases
            .get(alias)
            .copied()
            .ok_or_else(|| format!("unknown fixture alias {alias}").into())
    }

    pub async fn episode(&self, raw: Value) -> Result<Value> {
        PreparedEpisode::parse(&raw)?
            .run(&self.memory, self.store.db_id(), None)
            .await
    }

    pub async fn exact(&self, id: NodeId, max_bytes: usize) -> Result<Value> {
        let node = self.memory.get_node(id).await?.ok_or("node not found")?;
        let body = self.memory.resolve_body(&node).await?;
        let text = String::from_utf8(body)?;
        let mut end = max_bytes.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Ok(
            json!({"action":"exact","id":id,"summary":node.summary(),"status":node.status(),
                  "memory_kind":node.memory_kind(),"body":&text[..end],"body_truncated":end<text.len()}),
        )
    }

    pub async fn semantic_search(&self, text: &str, limit: usize) -> Result<Value> {
        let batch = self
            .memory
            .retrieve_batch_seeded(
                text,
                limit,
                Budget {
                    max_nodes: limit,
                    max_depth: 2,
                    min_relevance: 0.0,
                    explore: 0.0,
                    relevance_ratio: 0.0,
                    dedup_similarity: 1.0,
                    query_conditioning: 0.0,
                },
                StatusFilter::default(),
                &[],
            )
            .await?;
        let mut hits = Vec::new();
        for (lane, nodes) in [
            ("primary", batch.primary),
            ("probationary", batch.probationary),
        ] {
            for hit in nodes {
                hits.push(json!({"id":hit.node.id(),"summary":hit.node.summary(),
                                  "status":hit.node.status(),"lane":lane}));
            }
        }
        Ok(
            json!({"action":"semantic_search","method":"engine retrieval with reference hashing embeddings","hits":hits}),
        )
    }
}
