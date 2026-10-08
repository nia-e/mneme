//! Disposable status-normalization probe. No successor policy, persistent store,
//! providers, training, or semantic-quality scoring. Both arms use native ranking
//! and native lane-aware presentation unchanged.

use mneme_body::InlineStore;
use mneme_core::ports::{
    BodyStore, Budget, Clock, Embedder, EmbeddingMetadataStore, Error, GraphStore, StatusFilter,
    TraversalHop, VectorIndex,
};
use mneme_core::{
    Edge, EdgeKind, EmbeddingFingerprint, Node, NodeId, NodeStatus, Provenance, StrengthParams,
    Timestamp,
};
use mneme_cozo::{MemStore, StoreExport};
use mneme_engine::{Config, Memory, RetrievalHit};
use mneme_present::{BodyBudget, LaneBudgets, LaneLimit, PresentationBudget};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io::Read,
    num::{NonZeroU16, NonZeroU32},
    pin::Pin,
    sync::Arc,
};

const FIXTURE_SHA: &str = "cef375b167147b1ccf707c1e0e39f4c7048e087eba0d38c730d57fb6dbab608b";
const FREEZE_SHA: &str = "255113e36a3964a44b0e265aaa70da0184ec7f0324755492652be62be440d38f";
const INPUT_CAP: usize = 262_144;
const OUTPUT_CAP: usize = 65_536;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn require(ok: bool, message: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}
fn sha(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
fn field<T: DeserializeOwned>(v: &Value, name: &str) -> Result<T> {
    Ok(serde_json::from_value(
        v.get(name)
            .ok_or_else(|| format!("missing {name}"))?
            .clone(),
    )?)
}
fn nid(id: u64) -> NodeId {
    NodeId(u128::from(id).into())
}
fn number(id: NodeId) -> Result<u64> {
    Ok(u64::try_from(u128::from(id.0))?)
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NodeRow {
    id: u64,
    summary: String,
    body: String,
    status: String,
    candidate_use_count: Option<u32>,
    confidence: f32,
    stability: f32,
    vector: [f32; 8],
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EdgeRow {
    from: u64,
    to: u64,
    kind: EdgeKind,
    anchor: Option<mneme_core::BodySpan>,
    weight: f32,
    last_reinforced: Timestamp,
    trials: u32,
    interference: u32,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    id: String,
    nodes: Vec<NodeRow>,
    edges: Vec<EdgeRow>,
    contradictions: Vec<Value>,
    declared_changes_from_previous: Vec<Value>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Query {
    id: String,
    text: String,
    vector: [f32; 8],
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Run {
    id: String,
    checkpoint: String,
    query_id: String,
    repeat: usize,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    title: String,
    checkpoints: Vec<Checkpoint>,
    queries: Vec<Query>,
    runs: Vec<Run>,
    observed_ids: BTreeMap<String, Vec<u64>>,
    extra_integrity_checks: Vec<Value>,
}

fn budget(v: &Value) -> Result<Budget> {
    Ok(Budget {
        max_nodes: field(v, "max_nodes")?,
        max_depth: field(v, "max_depth")?,
        min_relevance: field(v, "min_relevance")?,
        explore: field(v, "explore")?,
        relevance_ratio: field(v, "relevance_ratio")?,
        dedup_similarity: field(v, "dedup_similarity")?,
        query_conditioning: field(v, "query_conditioning")?,
    })
}
fn strength(v: &Value) -> Result<StrengthParams> {
    Ok(StrengthParams {
        reinforce_gain: field(v, "reinforce_gain")?,
        interference_retention: field(v, "interference_retention")?,
        interference_resist: field(v, "interference_resist")?,
        resist_trials_half: field(v, "resist_trials_half")?,
    })
}
fn config(v: &Value) -> Result<Config> {
    require(
        v["default_body_scheme"] == "inline",
        "expected inline bodies",
    )?;
    Ok(Config {
        ann_k: field(v, "ann_k")?,
        lexical_k: field(v, "lexical_k")?,
        rrf_constant: field(v, "rrf_constant")?,
        dense_weight: field(v, "dense_weight")?,
        lexical_weight: field(v, "lexical_weight")?,
        graph_seed_cap: field(v, "graph_seed_cap")?,
        graph_weight: field(v, "graph_weight")?,
        graph_slot_cap: field(v, "graph_slot_cap")?,
        candidate_admission_limit: field(v, "candidate_admission_limit")?,
        similarity_link_cap: field(v, "similarity_link_cap")?,
        similarity_link_threshold: field(v, "similarity_link_threshold")?,
        min_similarity_links: field(v, "min_similarity_links")?,
        coretrieval_link_cap: field(v, "coretrieval_link_cap")?,
        strength: strength(&v["strength"])?,
        bridge_strength: strength(&v["bridge_strength"])?,
        budget: budget(&v["budget"])?,
        default_body_scheme: "inline",
        confidence_interference_retention: field(v, "confidence_interference_retention")?,
        confidence_restore: field(v, "confidence_restore")?,
        confidence_floor: field(v, "confidence_floor")?,
        promote_use_threshold: field(v, "promote_use_threshold")?,
        archive_floor: field(v, "archive_floor")?,
        bridge_probability: field(v, "bridge_probability")?,
        bridge_weight: field(v, "bridge_weight")?,
        prune_weight_floor: field(v, "prune_weight_floor")?,
        dense_degree_threshold: field(v, "dense_degree_threshold")?,
    })
}
fn presentation(v: &Value) -> Result<PresentationBudget> {
    let lane = |name| -> Result<LaneLimit> {
        let l = &v["lanes"][name];
        Ok(LaneLimit::new(
            field(l, "hard_min_items")?,
            field(l, "target_items")?,
            field(l, "max_items")?,
            field(l, "max_bytes")?,
        )?)
    };
    require(
        v["body"]
            == json!({"max_fetches":0,"max_source_bytes_total":0,"max_rendered_bytes_total":0,"max_source_bytes_each":0}),
        "body work must be disabled",
    )?;
    require(
        field::<u32>(v, "expected_native_minimum_control_reserve_bytes")?
            == PresentationBudget::minimum_control_reserve_bytes(),
        "native control reserve changed; preserve failure and review fixture",
    )?;
    let p = PresentationBudget::new(
        NonZeroU32::new(field(v, "max_content_bytes")?).ok_or("zero content cap")?,
        NonZeroU32::new(field(v, "control_reserve_bytes")?).ok_or("zero reserve")?,
        NonZeroU16::new(field(v, "max_items")?).ok_or("zero item cap")?,
        NonZeroU16::new(field(v, "max_summary_bytes_each")?).ok_or("zero summary cap")?,
        BodyBudget::disabled(),
        LaneBudgets::new(
            lane("core")?,
            lane("primary")?,
            lane("probationary")?,
            lane("expansion")?,
        )
        .with_episodic(lane("episodic")?),
    )?;
    require(
        p.normal_content_limit() == field::<u32>(v, "normal_content_limit")?,
        "incorrect byte accounting",
    )?;
    require(
        v["lane_byte_caps_are_additive"] == false,
        "lane caps must not be added",
    )?;
    Ok(p)
}

struct Controls {
    cfg: Config,
    budget: Budget,
    packing: PresentationBudget,
    k: usize,
    status: StatusFilter,
    now: Timestamp,
    fingerprint: EmbeddingFingerprint,
}
impl Controls {
    fn parse(v: &Value) -> Result<Self> {
        require(
            v["schema_version"] == 1 && v["dimension"] == 8,
            "unsupported fixture version/dimension",
        )?;
        let now = field(v, "fixed_clock_ms")?;
        require(
            v["node_init"]
                == json!({"memory_kind":"semantic","created":now,"tags":[],
            "provenance":{"kind":"conversation","session_u128":42,"turn":"node.id"},
            "body_ownership":"borrowed","origin_commit":null,"last_exposed":null,"exposure_count":0,
            "last_grounded_use":null,"grounded_use_count":0,"interference":0}),
            "unsupported node initialization",
        )?;
        let r = &v["retrieval"];
        require(
            r["tags"] == json!([])
                && r["lexical_index_enabled"] == true
                && r["reranker_enabled"] == false,
            "unsupported retrieval wiring",
        )?;
        require(
            r["budget"] == v["config"]["budget"],
            "retrieval/config budget mismatch",
        )?;
        let status: StatusFilter = field(r, "status_filter")?;
        require(
            status == StatusFilter::ACTIVE,
            "expected unchanged Active filter",
        )?;
        let fingerprint: EmbeddingFingerprint = field(v, "embedding_fingerprint")?;
        fingerprint.validate()?;
        require(fingerprint.dimension == 8, "embedding dimension mismatch")?;
        Ok(Self {
            cfg: config(&v["config"])?,
            budget: budget(&r["budget"])?,
            packing: presentation(&v["presentation"])?,
            k: field(r, "k")?,
            status,
            now,
            fingerprint,
        })
    }
}

fn vector(v: &[f32; 8]) -> Result<()> {
    let norm = v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    require(
        v.iter().all(|x| x.is_finite()) && (norm - 1.0).abs() <= 1e-5,
        "invalid/nonunit vector",
    )
}
fn status(n: &NodeRow) -> Result<NodeStatus> {
    Ok(match (n.status.as_str(), n.candidate_use_count) {
        ("ordinary", None) => NodeStatus::Active,
        ("archived", None) => NodeStatus::Archived,
        ("candidate", Some(0)) => NodeStatus::Candidate { use_count: 0 },
        _ => return Err("invalid fixture node status/count".into()),
    })
}
fn validate_case(c: &Case) -> Result<()> {
    require(
        !c.checkpoints.is_empty()
            && c.checkpoints.len() <= 2
            && !c.queries.is_empty()
            && c.queries.len() <= 8
            && !c.runs.is_empty()
            && c.runs.len() <= 8,
        "invalid case counts",
    )?;
    let mut checkpoints = BTreeSet::new();
    for cp in &c.checkpoints {
        require(
            checkpoints.insert(&cp.id)
                && !cp.nodes.is_empty()
                && cp.nodes.len() <= 80
                && cp.edges.len() <= 80
                && cp.contradictions.is_empty(),
            "invalid checkpoint",
        )?;
        let mut ids = BTreeSet::new();
        for n in &cp.nodes {
            require(
                ids.insert(n.id) && u32::try_from(n.id).is_ok(),
                "invalid/duplicate numeric ID",
            )?;
            require(
                !n.summary.trim().is_empty() && n.summary.len() <= 256 && n.body.len() <= 512,
                "invalid fixture text",
            )?;
            status(n)?;
            vector(&n.vector)?;
            require(
                n.confidence.is_finite()
                    && (0.0..=1.0).contains(&n.confidence)
                    && n.stability.is_finite()
                    && (0.0..=1.0).contains(&n.stability),
                "invalid node scalar",
            )?;
        }
        let mut pairs = BTreeSet::new();
        for e in &cp.edges {
            require(
                ids.contains(&e.from)
                    && ids.contains(&e.to)
                    && e.from != e.to
                    && pairs.insert((e.from, e.to)),
                "invalid/duplicate edge",
            )?;
            stored_edge(e)?.validate()?;
        }
        require(
            c.observed_ids.values().flatten().all(|id| ids.contains(id)),
            "unknown observed ID",
        )?;
    }
    let mut queries = BTreeSet::new();
    for q in &c.queries {
        require(
            queries.insert(&q.id) && !q.text.trim().is_empty() && q.text.len() <= 512,
            "invalid query",
        )?;
        vector(&q.vector)?;
    }
    let mut runs = BTreeSet::new();
    for r in &c.runs {
        require(
            runs.insert(&r.id)
                && checkpoints.contains(&r.checkpoint)
                && queries.contains(&r.query_id)
                && (1..=32).contains(&r.repeat),
            "invalid run",
        )?;
    }
    Ok(())
}
fn stored_edge(e: &EdgeRow) -> Result<Edge> {
    Ok(Edge::from_stored(
        nid(e.from),
        nid(e.to),
        e.kind,
        e.anchor,
        e.weight,
        e.last_reinforced,
        e.trials,
        e.interference,
    ))
}

struct Scripted {
    fingerprint: EmbeddingFingerprint,
    vectors: BTreeMap<String, Vec<f32>>,
}
impl Scripted {
    fn new(c: &Case, fingerprint: EmbeddingFingerprint) -> Result<Self> {
        let mut vectors = BTreeMap::new();
        for (text, v) in c
            .checkpoints
            .iter()
            .flat_map(|cp| cp.nodes.iter().map(|n| (&n.summary, &n.vector)))
            .chain(c.queries.iter().map(|q| (&q.text, &q.vector)))
        {
            vector(v)?;
            if let Some(old) = vectors.insert(text.clone(), v.to_vec()) {
                require(old == v, "conflicting exact-text embedding")?;
            }
        }
        Ok(Self {
            fingerprint,
            vectors,
        })
    }
}
impl Embedder for Scripted {
    fn dim(&self) -> usize {
        8
    }
    fn fingerprint(&self) -> EmbeddingFingerprint {
        self.fingerprint.clone()
    }
    fn embed<'a, 'b, 'c, 'f>(
        &'a self,
        texts: &'b [&'c str],
    ) -> Pin<Box<dyn Future<Output = mneme_core::ports::Result<Vec<Vec<f32>>>> + Send + 'f>>
    where
        'a: 'f,
        'b: 'f,
        'c: 'f,
        Self: 'f,
    {
        let result = texts
            .iter()
            .map(|s| {
                self.vectors
                    .get(*s)
                    .cloned()
                    .ok_or_else(|| Error::InvalidInput(format!("undeclared scripted text: {s}")))
            })
            .collect();
        Box::pin(async move { result })
    }
}
struct FixedClock(Timestamp);
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

async fn base_snapshot(
    cp: &Checkpoint,
    ctrl: &Controls,
) -> Result<(StoreExport, Arc<InlineStore>)> {
    let store = MemStore::new(8);
    store.set_embedding_fingerprint(&ctrl.fingerprint)?;
    let bodies = Arc::new(InlineStore::new());
    for n in &cp.nodes {
        let body = bodies.put(n.body.as_bytes()).await?;
        let node = Node::try_new(
            nid(n.id),
            &n.summary,
            body,
            [] as [&str; 0],
            Provenance::Conversation {
                session: 42u128.into(),
                turn: u32::try_from(n.id)?,
            },
            n.stability,
            n.confidence,
            status(n)?,
            ctrl.now,
        )?;
        store.put_node(&node).await?;
        store.upsert(nid(n.id), &n.vector).await?;
    }
    for e in &cp.edges {
        store.put_edge(&stored_edge(e)?).await?;
    }
    Ok((store.export(), bodies))
}
fn normalized(mut snap: StoreExport) -> StoreExport {
    for n in &mut snap.nodes {
        if n.is_candidate() {
            n.set_status(NodeStatus::Active);
        }
    }
    snap
}
fn canonical(snap: &StoreExport) -> Result<Value> {
    let mut value = serde_json::to_value(snap)?;
    // Every export relation is an array. Its order is not canonical store state.
    for v in value
        .as_object_mut()
        .ok_or("export must be object")?
        .values_mut()
    {
        if let Some(rows) = v.as_array_mut() {
            rows.sort_by_cached_key(Value::to_string);
        }
    }
    Ok(value)
}
fn state_sha(store: &MemStore) -> Result<String> {
    Ok(sha(serde_json::to_vec(&canonical(&store.export())?)?))
}
fn path(path: Option<&[TraversalHop]>) -> Result<Value> {
    path.map(|hops| {
        hops.iter()
            .map(|h| {
                Ok(
                    json!({"previous":number(h.previous)?,"target":number(h.target)?,
        "edge_from":number(h.edge.from)?,"edge_to":number(h.edge.to)?,"kind":h.edge.kind}),
                )
            })
            .collect::<Result<Vec<_>>>()
    })
    .transpose()
    .map(|p| json!(p))
}
fn hits(hits: &[RetrievalHit]) -> Result<Vec<Value>> {
    hits.iter().map(|h|Ok(json!({"id":number(h.node.id())?,"rank":h.lane_rank,
        "evidence":{"dense":h.evidence.dense_rank,"sparse":h.evidence.sparse_rank,"graph":h.evidence.graph_rank,"rerank":h.evidence.rerank_rank},
        "winning_path":path(h.graph_path.as_deref())?}))).collect()
}
fn forbid_archived(sample: &Value, archived: &BTreeSet<u64>) -> Result<()> {
    for item in sample["raw"]["primary"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(
            sample["raw"]["probationary"]
                .as_array()
                .into_iter()
                .flatten(),
        )
        .chain(sample["delivered"].as_array().into_iter().flatten())
    {
        require(
            !archived.contains(&field::<u64>(item, "id")?),
            "Archived node surfaced",
        )?;
        for hop in item["winning_path"].as_array().into_iter().flatten() {
            for name in ["previous", "target", "edge_from", "edge_to"] {
                require(
                    !archived.contains(&field::<u64>(hop, name)?),
                    "Archived node in winning path",
                )?;
            }
        }
    }
    Ok(())
}

async fn probe(
    memory: &Memory,
    q: &Query,
    ctrl: &Controls,
    stamps: &mut BTreeMap<String, Value>,
    calls: &mut usize,
) -> Result<Value> {
    *calls += 1;
    let raw = memory
        .retrieve_batch_seeded_observed(&q.text, ctrl.k, ctrl.budget, ctrl.status, &[])
        .await?;
    let policy = serde_json::to_value(mneme_app::presentation_retrieval_metadata(&raw)?)?;
    let policy_sha = sha(serde_json::to_vec(&policy)?);
    stamps.insert(policy_sha.clone(), policy);
    *calls += 1;
    let delivered = mneme_app::recall_context_observed(
        memory,
        &q.text,
        ctrl.k,
        ctrl.budget,
        &[],
        &ctrl.packing,
    )
    .await?;
    let envelope = serde_json::to_value(delivered.plan.envelope())?;
    let content = delivered.plan.rendered_content();
    require(
        content.len() <= ctrl.packing.normal_content_limit() as usize
            && delivered.observations.len() <= usize::from(ctrl.packing.max_items()),
        "native packing exceeded fixture budget",
    )?;
    let cards = delivered
        .observations
        .iter()
        .map(|c| {
            Ok(json!({"id":number(c.node_id)?,"lane":c.lane,
        "card_sha256":c.card_sha256,"winning_path":path(c.graph_path.as_deref())?}))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"policy_stamp_sha256":policy_sha,"raw":{"primary":hits(&raw.primary)?,"probationary":hits(&raw.probationary)?},
        "delivered":cards,"context_sha256":sha(content),"context_bytes":content.len(),"omitted":envelope["omitted"]}),
    )
}

fn delivered_ids(v: &Value) -> BTreeSet<u64> {
    v["delivered"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| h["id"].as_u64())
        .collect()
}
fn raw_ids(v: &Value) -> BTreeSet<u64> {
    ["primary", "probationary"]
        .into_iter()
        .flat_map(|lane| v["raw"][lane].as_array().into_iter().flatten())
        .filter_map(|h| h["id"].as_u64())
        .collect()
}

async fn run_case(
    c: &Case,
    ctrl: &Controls,
    stamps: &mut BTreeMap<String, Value>,
    calls: &mut usize,
) -> Result<Value> {
    validate_case(c)?;
    let embedder = Arc::new(Scripted::new(c, ctrl.fingerprint.clone())?);
    let mut checkpoints = Vec::new();
    let mut results: BTreeMap<(String, String), Value> = BTreeMap::new();
    for cp in &c.checkpoints {
        let (base, bodies) = base_snapshot(cp, ctrl).await?;
        let changed: Vec<_> = cp
            .nodes
            .iter()
            .filter(|n| n.status == "candidate")
            .map(|n| n.id)
            .collect();
        let normalized = normalized(base.clone());
        let mut expected = canonical(&base)?;
        for n in expected["nodes"].as_array_mut().ok_or("missing nodes")? {
            if n["status"].get("Candidate").is_some() {
                n["status"] = json!("Active");
            }
        }
        expected["nodes"]
            .as_array_mut()
            .unwrap()
            .sort_by_cached_key(Value::to_string);
        require(
            canonical(&normalized)? == expected,
            "normalization changed nonstatus state",
        )?;
        let archived = cp
            .nodes
            .iter()
            .filter(|n| n.status == "archived")
            .map(|n| n.id)
            .collect();
        let mut arms = serde_json::Map::new();
        for (name, snapshot) in [("current", base), ("normalized", normalized)] {
            let expected = canonical(&snapshot)?;
            let store = Arc::new(MemStore::from_export(snapshot)?);
            require(
                canonical(&store.export())? == expected,
                "detached import changed canonical state",
            )?;
            let before = state_sha(&store)?;
            let memory = Memory::new(
                store.clone(),
                store.clone(),
                store.clone(),
                embedder.clone(),
                Arc::new(FixedClock(ctrl.now)),
                ctrl.cfg,
            )
            .with_lexical_index(store.clone())
            .with_body_store(bodies.clone());
            let mut runs = Vec::new();
            for r in c.runs.iter().filter(|r| r.checkpoint == cp.id) {
                let q = c
                    .queries
                    .iter()
                    .find(|q| q.id == r.query_id)
                    .ok_or("missing query")?;
                let mut first = None;
                let mut repeat_hash = Sha256::new();
                for _ in 0..r.repeat {
                    let sample = probe(&memory, q, ctrl, stamps, calls).await?;
                    forbid_archived(&sample, &archived)?;
                    if let Some(previous) = &first {
                        require(previous == &sample, "identical repeated read changed")?;
                    }
                    repeat_hash.update(serde_json::to_vec(&sample)?);
                    first = Some(sample);
                    require(
                        state_sha(&store)? == before,
                        "query mutated canonical state",
                    )?;
                }
                let sample = first.ok_or("empty run")?;
                results.insert((name.into(), r.id.clone()), sample.clone());
                runs.push(json!({"id":r.id,"repeat":r.repeat,"repetition_sha256":format!("{:x}",repeat_hash.finalize()),"sample":sample}));
            }
            arms.insert(name.into(),json!({"state_before_sha256":before,"state_after_sha256":state_sha(&store)?,"runs":runs}));
        }
        checkpoints.push(json!({"id":cp.id,"normalized_node_ids":changed,"arms":arms}));
    }
    for check in &c.extra_integrity_checks {
        match check["kind"].as_str() {
            Some("identical_read_results") => {
                let ids: Vec<String> = field(check, "runs")?;
                require(ids.len() == 2, "expected two comparison runs")?;
                for arm in ["current", "normalized"] {
                    require(
                        results.get(&(arm.into(), ids[0].clone()))
                            == results.get(&(arm.into(), ids[1].clone()))
                            && results.contains_key(&(arm.into(), ids[0].clone())),
                        "before/after pure-read outputs changed",
                    )?;
                }
            }
            Some("identical_repeated_read_results") => {
                let id: String = field(check, "run")?;
                require(
                    c.runs.iter().any(|r| r.id == id && r.repeat > 1),
                    "missing repeated run",
                )?;
            }
            Some("identical_arm_read_results") => {
                for r in &c.runs {
                    require(
                        results.get(&("current".into(), r.id.clone()))
                            == results.get(&("normalized".into(), r.id.clone())),
                        "negative-control arms differ",
                    )?;
                }
            }
            _ => return Err("unknown integrity check".into()),
        }
    }
    let differences=c.runs.iter().map(|r| {
        let a=&results[&("current".into(),r.id.clone())];let b=&results[&("normalized".into(),r.id.clone())];
        let raw_a=raw_ids(a);let raw_b=raw_ids(b);let sent_a=delivered_ids(a);let sent_b=delivered_ids(b);
        json!({"run":r.id,"raw_lost":raw_a.difference(&raw_b).collect::<Vec<_>>(),"raw_gained":raw_b.difference(&raw_a).collect::<Vec<_>>(),
            "delivered_lost":sent_a.difference(&sent_b).collect::<Vec<_>>(),"delivered_gained":sent_b.difference(&sent_a).collect::<Vec<_>>()})
    }).collect::<Vec<_>>();
    Ok(
        json!({"id":c.id,"observed_ids":c.observed_ids,"checkpoints":checkpoints,"differences":differences}),
    )
}

fn checked_inputs(fixture: &[u8], freeze: &[u8]) -> Result<Value> {
    require(
        fixture.len() <= INPUT_CAP && freeze.len() <= INPUT_CAP,
        "input cap exceeded",
    )?;
    require(
        sha(fixture) == FIXTURE_SHA && sha(freeze) == FREEZE_SHA,
        "frozen input hash mismatch",
    )?;
    let manifest: Value = serde_json::from_slice(freeze)?;
    require(
        manifest["fixture_sha256"] == FIXTURE_SHA && manifest["fixture_bytes"] == fixture.len(),
        "freeze/fixture disagreement",
    )?;
    Ok(serde_json::from_slice(fixture)?)
}
fn read_bounded(path: &std::ffi::OsStr) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((INPUT_CAP + 1) as u64)
        .read_to_end(&mut bytes)?;
    require(bytes.len() <= INPUT_CAP, "input cap exceeded")?;
    Ok(bytes)
}
async fn execute(v: &Value) -> Result<Value> {
    let ctrl = Controls::parse(v)?;
    let cases: Vec<Case> = field(v, "cases")?;
    require(cases.len() == 6, "expected six frozen cases")?;
    let mut ids = BTreeSet::new();
    let mut logical = 0;
    for c in &cases {
        validate_case(c)?;
        require(ids.insert(&c.id), "duplicate case ID")?;
        logical += c.runs.iter().map(|r| r.repeat).sum::<usize>();
    }
    require(logical == 40, "expected 40 logical probes per arm")?;
    let mut stamps = BTreeMap::new();
    let mut calls = 0;
    let mut rows = Vec::new();
    for c in &cases {
        rows.push(
            run_case(c, &ctrl, &mut stamps, &mut calls)
                .await
                .map_err(|e| format!("case {}: {e}", c.id))?,
        );
    }
    require(calls == 160, "retrieval accounting mismatch")?;
    Ok(
        json!({"schema":"single-graph-eligibility-result-v1","fixture_sha256":FIXTURE_SHA,"freeze_sha256":FREEZE_SHA,
        "code_sha256":sha(include_bytes!("single_graph_eligibility_v1.rs")),"scope":"Current native policy with status-only normalized detached copies; not successor or utility qualification",
        "state_digest_scope":"Run-local: actual InlineStore body references and base database IDs are allocated once per checkpoint and preserved in paired arms",
        "logical_probes_per_arm":logical,"engine_retrieval_calls":calls,"integrity_checks":"passed","policy_stamps":stamps,"cases":rows}),
    )
}
fn bounded_output(v: &Value) -> Result<Vec<u8>> {
    let mut out = serde_json::to_vec(v)?;
    out.push(b'\n');
    require(
        out.len() <= OUTPUT_CAP,
        "whole-report cap exceeded; preserve failure, do not truncate",
    )?;
    Ok(out)
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let result = async {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        require(
            args.len() == 2,
            "usage: single_graph_eligibility_v1 FIXTURE.json FREEZE.json",
        )?;
        let v = checked_inputs(&read_bounded(&args[0])?, &read_bounded(&args[1])?)?;
        bounded_output(&execute(&v).await?)
    }
    .await;
    match result {
        Ok(bytes) => {
            use std::io::Write;
            if let Err(error) = std::io::stdout().write_all(&bytes) {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[path = "single_graph_eligibility_v1/tests.rs"]
mod tests;
