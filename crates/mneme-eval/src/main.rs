//! mneme-eval — reproducible memory-substrate evaluation.
//!
//! The no-LLM default builds a synthetic, controllable corpus with known gold ids
//! and compares dependency-free BM25, read-only dense ANN, sparse+dense RRF with
//! spread disabled, the shipped cold graph (automatic similarity topology is
//! disabled), that same graph under the shipped default budget, and a
//! grounded-feedback-trained graph, and that trained graph after a deterministic
//! 1:1 injection of equally reinforced false transitions.
//! It reports **recall@k / MRR / hit@k**
//! plus latency, injected context, build cost, and topology at one or more fixed
//! corpus sizes. `--json` emits a versioned machine-readable report.
//!
//! `--legacy-layer1` retains the original five-condition ladder over the same
//! facts, differing only in the graph/budget:
//! - **flat** — sparse+dense RRF seeds, spread disabled. The controlled baseline.
//! - **mneme** — hybrid seeds + spread over the graph that emerged from ingest
//!   (similarity auto-links) + query-conditioning. The *cold* graph, untrained.
//! - **mneme+self-training** — an explicit ablation with query-time co-retrieval
//!   edge creation enabled; exposure is treated as its own weak signal.
//! - **mneme+feedback** — the graph after explicit, relevant synthetic trails;
//!   the grounded training path used by the primary offline comparison.
//! - **mneme+links** — explicit same-entity edges added by hand: the structure
//!   reflect/feedback would ideally learn — an upper bound on what the graph buys.
//!
//! Single-hop questions name the entity directly (ANN should nail these, so flat
//! and mneme tie). Multi-hop questions identify the entity *indirectly* (by a
//! unique codename) and ask a different attribute — the answer isn't the ANN match,
//! it's a graph hop away. That's where spread should beat flat, and where grounded
//! feedback should climb from cold `mneme` toward the `links` ceiling.
//!
//! With `--agent-*` set the harness switches to **Layer 2** (LLM answers from the
//! retrieved context, scored for accuracy + cost/tokens); `--longmemeval <path>`
//! runs the real conversational benchmark with the same conditions (incl. training).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use mneme_body::InlineStore;
use mneme_core::ports::{
    Budget, Clock, ColdPath, Embedder, GraphStore, LexicalIndex, Scored, StatusFilter, SystemClock,
    Traversal, TraversalScope, VectorIndex,
};
use mneme_core::{EdgeKind, EmbeddingFingerprint, NodeId, Provenance, Signal, Timestamp};
#[cfg(feature = "cozo")]
use mneme_cozo::CozoStore;
#[cfg(not(feature = "cozo"))]
use mneme_cozo::MemStore;
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::{Config, DeterministicNodeIdSource, Ingest, Memory, NodeIdSource};

// ---- tiny deterministic RNG (xorshift64*) ---------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A deterministic permutation of `0..n` (Fisher–Yates with the seeded rng), so each
/// attribute assigns a *unique* value per entity — exact-match on a gold value then
/// identifies exactly one fact, with no cross-entity collisions to false-positive on.
fn perm(rng: &mut Rng, n: usize) -> Vec<usize> {
    let mut p: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        p.swap(i, rng.below(i + 1));
    }
    p
}

// ---- synthetic dataset ----------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cat {
    Single,
    Multi,
    Synth,
    Abstain,
    Conflict,
}

const NCATS: usize = 5;
const CAT_NAMES: [&str; NCATS] = ["1hop", "2hop", "synth", "abst", "confl"];

impl Cat {
    fn idx(self) -> usize {
        match self {
            Cat::Single => 0,
            Cat::Multi => 1,
            Cat::Synth => 2,
            Cat::Abstain => 3,
            Cat::Conflict => 4,
        }
    }
}

struct Fact {
    id: u32,
    text: String,
}

struct Question {
    query: String,
    gold: Vec<u32>,
    /// The gold answer value (e.g. "teal") — the exact-match target for Layer 2.
    answer: String,
    cat: Cat,
}

struct Dataset {
    facts: Vec<Fact>,
    questions: Vec<Question>,
    /// codename fact id and the attribute fact ids, per entity (for explicit links).
    by_entity: Vec<EntityFacts>,
}

struct EntityFacts {
    codename: &'static str,
    codename_fact: u32,
    attr_facts: Vec<u32>,
    /// favorite-color fact (attr 0) + value — the answer target for synthesis.
    color_fact: u32,
    color_value: String,
    /// unique clearance level — the comparison key for synthesis.
    clearance: u32,
    /// the *updated* station fact + value — the current answer for conflict.
    station_new_fact: u32,
    station_new: String,
}

const ENTITIES: &[&str] = &[
    "Mara Quinn",
    "Theo Vance",
    "Iris Bloom",
    "Cole Reyes",
    "Nadia Frost",
    "Owen Park",
    "Lena Cruz",
    "Finn Walsh",
    "Greta Holt",
    "Sam Okoro",
    "Priya Rao",
    "Dmitri Novak",
    "Yuki Tan",
    "Hana Berg",
    "Luca Mori",
    "Esme Fox",
    "Rafa Diaz",
    "Tariq Bello",
    "Ingrid Slot",
    "Ravi Menon",
];
const CODENAMES: &[&str] = &[
    "Lantern",
    "Basalt",
    "Marigold",
    "Quasar",
    "Driftwood",
    "Vellum",
    "Cinder",
    "Halcyon",
    "Tundra",
    "Mosaic",
    "Pyrite",
    "Zephyr",
    "Cobalt",
    "Thistle",
    "Onyx",
    "Ferro",
    "Lichen",
    "Saffron",
    "Galleon",
    "Verdant",
];
// ≥ ENTITIES.len() distinct values per attribute, so each entity gets a *unique*
// value (see `perm`) — exact-match on a gold value then maps to exactly one fact.
const ATTRS: &[(&str, &[&str])] = &[
    (
        "favorite color",
        &[
            "teal",
            "crimson",
            "amber",
            "indigo",
            "ochre",
            "jade",
            "maroon",
            "slate",
            "mauve",
            "vermillion",
            "chartreuse",
            "cerulean",
            "magenta",
            "olive",
            "scarlet",
            "turquoise",
            "sienna",
            "periwinkle",
            "lavender",
            "russet",
        ],
    ),
    (
        "home city",
        &[
            "Osaka",
            "Lisbon",
            "Nairobi",
            "Quito",
            "Bergen",
            "Dakar",
            "Perth",
            "Tbilisi",
            "Reykjavik",
            "Montevideo",
            "Chengdu",
            "Marrakesh",
            "Tallinn",
            "Hobart",
            "Cusco",
            "Gdansk",
            "Kyoto",
            "Valencia",
            "Windhoek",
            "Almaty",
        ],
    ),
    (
        "preferred tool",
        &[
            "lathe",
            "oscilloscope",
            "sextant",
            "trowel",
            "anvil",
            "loom",
            "scalpel",
            "compass",
            "theodolite",
            "micrometer",
            "awl",
            "kiln",
            "chisel",
            "calipers",
            "auger",
            "mallet",
            "pipette",
            "vise",
            "bandsaw",
            "rasp",
        ],
    ),
    (
        "spirit animal",
        &[
            "otter", "falcon", "ibex", "heron", "marten", "lynx", "stork", "gecko", "tapir",
            "osprey", "civet", "dingo", "quokka", "caracal", "ibis", "pangolin", "kestrel",
            "mongoose", "capybara", "serval",
        ],
    ),
];
/// Station names for the conflict (supersession) facts — old then updated.
const STATIONS: &[&str] = &[
    "Reef", "Summit", "Delta", "Vault", "Beacon", "Harbor", "Ridge", "Hollow", "Spire", "Fenwick",
    "Drift", "Cairn", "Mire", "Brink", "Thorn", "Glade", "Marsh", "Knoll", "Crag", "Fjord",
];

fn push_fact(facts: &mut Vec<Fact>, text: String) -> u32 {
    let id = facts.len() as u32;
    facts.push(Fact { id, text });
    id
}

/// Whether an answer signals "not in memory" — the correct response to an abstention
/// question. Generous on phrasing: what matters is the agent *didn't hallucinate* a
/// value, not that it echoed exact words.
fn is_abstention(answer: &str) -> bool {
    let a = answer.to_lowercase();
    [
        "don't know",
        "dont know",
        "do not know",
        "not know",
        "no information",
        "unknown",
        "cannot",
        "can't",
        "not available",
        "n/a",
        "no data",
        "not provided",
        "not in the context",
        "not specified",
        "isn't in",
        "is not in",
    ]
    .iter()
    .any(|m| a.contains(m))
}

fn gen_dataset(seed: u64) -> Dataset {
    let mut rng = Rng(seed | 1);
    let mut facts = Vec::new();
    let mut questions = Vec::new();
    let mut by_entity: Vec<EntityFacts> = Vec::new();
    // Unique value per entity for each attribute, plus a unique clearance level and a
    // unique starting station (permutations).
    let perms: Vec<Vec<usize>> = ATTRS.iter().map(|(_, v)| perm(&mut rng, v.len())).collect();
    let clearances = perm(&mut rng, ENTITIES.len());
    let stations = perm(&mut rng, STATIONS.len());

    for (e, &name) in ENTITIES.iter().enumerate() {
        let codename = CODENAMES[e % CODENAMES.len()];
        let codename_fact = push_fact(&mut facts, format!("{name}'s codename is {codename}."));

        // a fact + a single-hop and a multi-hop question per attribute.
        let mut attr_facts = Vec::new();
        let (mut color_fact, mut color_value) = (0, String::new());
        for (a, &(attr, values)) in ATTRS.iter().enumerate() {
            let value = values[perms[a][e]];
            let id = push_fact(&mut facts, format!("{name}'s {attr} is {value}."));
            attr_facts.push(id);
            if a == 0 {
                color_fact = id;
                color_value = value.to_string();
            }
            questions.push(Question {
                query: format!("What is {name}'s {attr}?"),
                gold: vec![id],
                answer: value.to_string(),
                cat: Cat::Single,
            });
            questions.push(Question {
                query: format!("The agent codenamed {codename} — what is their {attr}?"),
                gold: vec![id],
                answer: value.to_string(),
                cat: Cat::Multi,
            });
        }

        // a unique clearance level (the synthesis comparison key).
        let clearance = (clearances[e] + 1) as u32;
        push_fact(
            &mut facts,
            format!("{name}'s clearance level is {clearance}."),
        );

        // a station that is later *updated* — the conflict pair (old, then new).
        let old = STATIONS[stations[e]];
        let new = STATIONS[(stations[e] + 7) % STATIONS.len()];
        push_fact(&mut facts, format!("{name}'s station is {old}."));
        let station_new_fact = push_fact(
            &mut facts,
            format!("{name} was reassigned; their station is now {new}."),
        );

        by_entity.push(EntityFacts {
            codename,
            codename_fact,
            attr_facts,
            color_fact,
            color_value,
            clearance,
            station_new_fact,
            station_new: new.to_string(),
        });
    }

    // Hard questions, once every entity's facts exist.
    let n = ENTITIES.len();
    for e in 0..n {
        let ef = &by_entity[e];
        let other = &by_entity[(e + 7) % n];
        let name = ENTITIES[e];

        // synthesis: compare two agents' clearance, answer the winner's colour — a
        // different surface, reachable only by hopping from the clearance facts.
        let winner = if ef.clearance >= other.clearance {
            ef
        } else {
            other
        };
        questions.push(Question {
            query: format!(
                "Between the agents codenamed {} and {}, the one with the higher clearance level — what is their favorite color?",
                ef.codename, other.codename
            ),
            gold: vec![winner.color_fact],
            answer: winner.color_value.clone(),
            cat: Cat::Synth,
        });

        // abstention: an attribute that simply does not exist.
        questions.push(Question {
            query: format!("What is {name}'s blood type?"),
            gold: vec![],
            answer: "i don't know".to_string(),
            cat: Cat::Abstain,
        });

        // conflict: a fact was superseded; the *current* value is the newer one.
        questions.push(Question {
            query: format!("What is {name}'s current station?"),
            gold: vec![ef.station_new_fact],
            answer: ef.station_new.clone(),
            cat: Cat::Conflict,
        });
    }

    Dataset {
        facts,
        questions,
        by_entity,
    }
}

/// Deterministically pad the controlled corpus with unrelated-but-plausible project
/// notes. Gold facts and questions stay unchanged, so repeated `--nodes` runs measure
/// how retrieval and graph density behave as distractors accumulate.
fn pad_dataset(mut ds: Dataset, target_nodes: usize, seed: u64) -> Dataset {
    assert!(
        target_nodes >= ds.facts.len(),
        "--nodes target {target_nodes} is smaller than the {}-fact gold corpus",
        ds.facts.len()
    );
    const TOPICS: &[&str] = &[
        "build cache",
        "release pipeline",
        "schema migration",
        "rendering layer",
        "network retry",
        "parser state",
        "worker queue",
        "index shard",
    ];
    const STATES: &[&str] = &[
        "uses a bounded lease",
        "is guarded by a feature flag",
        "requires an idempotent retry",
        "writes an append-only checkpoint",
        "keeps a compact rollback marker",
        "validates ownership before commit",
        "emits a structured diagnostic",
        "runs after dependency resolution",
    ];
    const OWNERS: &[&str] = &[
        "Aster", "Birch", "Cedar", "Dahlia", "Elm", "Flint", "Grove", "Hazel",
    ];

    let mut rng = Rng(seed ^ 0xD15A_C7A0_5EED_u64);
    while ds.facts.len() < target_nodes {
        let i = ds.facts.len();
        let topic = TOPICS[rng.below(TOPICS.len())];
        let state = STATES[rng.below(STATES.len())];
        let owner = OWNERS[rng.below(OWNERS.len())];
        let marker = rng.next() % 100_000;
        push_fact(
            &mut ds.facts,
            format!("Archive record D{i:06}: the {topic} for unit {owner}-{marker:05} {state}."),
        );
    }
    ds
}

// ---- dependency-free sparse baseline -------------------------------------

fn lexical_terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// A deliberately small, auditable BM25 implementation. It is not meant to become
/// a product search engine; it gives the benchmark the sparse/exact-match baseline
/// that a dense-only comparison conspicuously lacks, without adding a dependency.
struct Bm25Index {
    ids: Vec<u32>,
    docs: Vec<Vec<String>>,
    df: HashMap<String, usize>,
    avg_len: f64,
}

impl Bm25Index {
    fn build(facts: &[Fact]) -> Self {
        let mut df = HashMap::new();
        let mut total_len = 0usize;
        let mut ids = Vec::with_capacity(facts.len());
        let mut docs = Vec::with_capacity(facts.len());
        for fact in facts {
            let terms = lexical_terms(&fact.text);
            total_len += terms.len();
            let unique: HashSet<&str> = terms.iter().map(String::as_str).collect();
            for term in unique {
                *df.entry(term.to_string()).or_insert(0) += 1;
            }
            ids.push(fact.id);
            docs.push(terms);
        }
        let avg_len = if docs.is_empty() {
            0.0
        } else {
            total_len as f64 / docs.len() as f64
        };
        Self {
            ids,
            docs,
            df,
            avg_len,
        }
    }

    fn search(&self, query: &str, k: usize) -> Vec<u32> {
        const K1: f64 = 1.2;
        const B: f64 = 0.75;
        if self.docs.is_empty() || self.avg_len == 0.0 || k == 0 {
            return Vec::new();
        }
        let query_terms: HashSet<String> = lexical_terms(query).into_iter().collect();
        let n = self.docs.len() as f64;
        let mut scored = Vec::new();
        for (doc_i, terms) in self.docs.iter().enumerate() {
            let mut tf: HashMap<&str, usize> = HashMap::new();
            for term in terms {
                *tf.entry(term.as_str()).or_insert(0) += 1;
            }
            let dl = terms.len() as f64;
            let mut score = 0.0;
            for term in &query_terms {
                let Some(&freq) = tf.get(term.as_str()) else {
                    continue;
                };
                let df = *self.df.get(term).unwrap_or(&0) as f64;
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                let tf = freq as f64;
                score += idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / self.avg_len));
            }
            if score > 0.0 {
                scored.push((self.ids[doc_i], score));
            }
        }
        scored.sort_by(|(a_id, a), (b_id, b)| b.total_cmp(a).then_with(|| a_id.cmp(b_id)));
        scored.into_iter().take(k).map(|(id, _)| id).collect()
    }
}

// ---- harness --------------------------------------------------------------

#[cfg(feature = "fastembed")]
fn make_embedder(lexical: bool) -> Arc<dyn Embedder> {
    if lexical {
        Arc::new(HashingEmbedder::new(DEFAULT_DIM))
    } else {
        Arc::new(
            mneme_embed::FastEmbedder::new()
                .expect("load the bge-base model (first run downloads it)"),
        )
    }
}
#[cfg(not(feature = "fastembed"))]
fn make_embedder(_lexical: bool) -> Arc<dyn Embedder> {
    Arc::new(HashingEmbedder::new(DEFAULT_DIM))
}

/// The memory plus the ports the offline benchmark needs for read-only dense
/// retrieval and topology inspection. Production code should only need [`Memory`];
/// keeping the ports here is intentionally evaluation-only instrumentation.
struct EvalMemory {
    mem: Memory,
    graph: Arc<dyn GraphStore>,
    vectors: Arc<dyn VectorIndex>,
    traversal: Arc<dyn Traversal>,
    lexical: Arc<dyn LexicalIndex>,
    embedder: Arc<dyn Embedder>,
}

struct EvalClock(Timestamp);

impl Clock for EvalClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

fn eval_memory_with_config_and_clock(
    embedder: Arc<dyn Embedder>,
    clock: Arc<dyn Clock>,
    config: Config,
    node_ids: Option<Arc<dyn NodeIdSource>>,
) -> EvalMemory {
    #[cfg(feature = "cozo")]
    let store = Arc::new(CozoStore::new(DEFAULT_DIM).expect("create ephemeral cozo eval store"));
    #[cfg(not(feature = "cozo"))]
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let graph: Arc<dyn GraphStore> = store.clone();
    let vectors: Arc<dyn VectorIndex> = store.clone();
    let traversal: Arc<dyn Traversal> = store.clone();
    let lexical: Arc<dyn LexicalIndex> = store.clone();
    let mem = Memory::new(
        graph.clone(),
        vectors.clone(),
        traversal.clone(),
        embedder.clone(),
        clock,
        config,
    );
    let mem = match node_ids {
        Some(source) => mem.with_node_id_source(source),
        None => mem,
    }
    .with_lexical_index(lexical.clone())
    .with_body_store(Arc::new(InlineStore::new()));
    EvalMemory {
        mem,
        graph,
        vectors,
        traversal,
        lexical,
        embedder,
    }
}

fn eval_memory(embedder: Arc<dyn Embedder>) -> EvalMemory {
    eval_memory_with_config_and_clock(embedder, Arc::new(SystemClock), Config::default(), None)
}

fn eval_memory_with_config(embedder: Arc<dyn Embedder>, config: Config) -> EvalMemory {
    eval_memory_with_config_and_clock(embedder, Arc::new(SystemClock), config, None)
}

fn fixed_eval_memory_with_config(
    embedder: Arc<dyn Embedder>,
    dataset_seed: u64,
    id_seed: u64,
    config: Config,
) -> EvalMemory {
    // Keep timestamps and node identities stable across separately built
    // conditions while allowing ID-order sensitivity to vary independently of
    // the dataset. Production keeps coordination-free random ULIDs; evaluation
    // needs paired tie-breaks to compare conditions fairly.
    eval_memory_with_config_and_clock(
        embedder,
        Arc::new(EvalClock(1_700_000_000_000 + dataset_seed as Timestamp)),
        config,
        Some(Arc::new(DeterministicNodeIdSource::new(id_seed))),
    )
}

#[cfg(test)]
fn fixed_eval_memory(embedder: Arc<dyn Embedder>, dataset_seed: u64, id_seed: u64) -> EvalMemory {
    fixed_eval_memory_with_config(embedder, dataset_seed, id_seed, Config::default())
}

fn memory(embedder: Arc<dyn Embedder>) -> Memory {
    eval_memory(embedder).mem
}

fn eval_backend_name() -> &'static str {
    if cfg!(feature = "cozo") {
        "cozo-memory-hnsw"
    } else {
        "reference-memory"
    }
}

fn eval_embedder_name() -> &'static str {
    if cfg!(feature = "fastembed") {
        "bge-base-en-v1.5"
    } else {
        "hashing-reference"
    }
}

#[cfg(feature = "fastembed")]
fn eval_embedder_fingerprint() -> EmbeddingFingerprint {
    mneme_embed::fastembed_fingerprint()
}

#[cfg(not(feature = "fastembed"))]
fn eval_embedder_fingerprint() -> EmbeddingFingerprint {
    mneme_embed::hashing_fingerprint(DEFAULT_DIM)
}

/// Ingest every fact; return node→fact-id so retrieval can be scored by fact.
async fn ingest_all(mem: &Memory, ds: &Dataset) -> (HashMap<NodeId, u32>, HashMap<u32, NodeId>) {
    let mut node_of = HashMap::new();
    let mut id_of = HashMap::new();
    for f in &ds.facts {
        let id = mem
            .ingest(Ingest::new(&f.text, b"", &[], Provenance::derived_empty()))
            .await
            .unwrap();
        node_of.insert(id, f.id);
        id_of.insert(f.id, id);
    }
    (node_of, id_of)
}

/// Add explicit same-entity edges (codename fact → each attribute fact): the
/// structured graph that reflect/feedback would learn over real usage.
async fn link_entities(mem: &Memory, ds: &Dataset, id_of: &HashMap<u32, NodeId>) {
    for e in &ds.by_entity {
        let from = id_of[&e.codename_fact];
        for af in &e.attr_facts {
            let to = id_of[af];
            mem.link(from, to, EdgeKind::Associative, 0.7, None)
                .await
                .unwrap();
        }
    }
}

fn budget_flat(k: usize) -> Budget {
    Budget {
        max_nodes: k,
        max_depth: 0,
        min_relevance: 0.0,
        explore: 0.0,
        relevance_ratio: 0.0,
        dedup_similarity: 1.0,
        query_conditioning: 0.0,
    }
}
fn budget_mneme() -> Budget {
    Budget {
        max_nodes: 50,
        max_depth: 4,
        min_relevance: 0.0,
        // Keep the comparison deterministic. Production's 10% keyed
        // exploration is a policy choice, not part of retrieval quality, and
        // fresh ULIDs would otherwise change which weak edges are admitted
        // between benchmark conditions and runs.
        explore: 0.0,
        relevance_ratio: 0.0,
        dedup_similarity: 1.0,
        query_conditioning: 0.4,
    }
}

/// Validated, offline-only policy knobs for causal evaluation. Every field is an
/// override rather than a second default: an empty value must remain exactly
/// equivalent to the checked-in engine and controlled-ablation policies.
#[derive(Clone, Copy, Debug, Default)]
struct OfflinePolicyOverrides {
    query_conditioning: Option<f32>,
    graph_slot_cap: Option<usize>,
    graph_seed_cap: Option<usize>,
    graph_weight: Option<f32>,
    similarity_link_cap: Option<usize>,
    relevance_ratio: Option<f32>,
    dedup_similarity: Option<f32>,
    max_depth: Option<u8>,
}

impl OfflinePolicyOverrides {
    fn parse(args: &[String]) -> Result<Self, String> {
        Ok(Self {
            query_conditioning: parse_unit_interval_override(args, "--query-conditioning")?,
            graph_slot_cap: parse_override(args, "--graph-slot-cap")?,
            graph_seed_cap: parse_override(args, "--graph-seed-cap")?,
            graph_weight: parse_nonnegative_finite_override(args, "--graph-weight")?,
            similarity_link_cap: parse_override(args, "--similarity-link-cap")?,
            relevance_ratio: parse_unit_interval_override(args, "--relevance-ratio")?,
            dedup_similarity: parse_unit_interval_override(args, "--dedup-similarity")?,
            max_depth: parse_override(args, "--max-depth")?,
        })
    }

    fn any(self) -> bool {
        self.query_conditioning.is_some()
            || self.graph_slot_cap.is_some()
            || self.graph_seed_cap.is_some()
            || self.graph_weight.is_some()
            || self.similarity_link_cap.is_some()
            || self.relevance_ratio.is_some()
            || self.dedup_similarity.is_some()
            || self.max_depth.is_some()
    }

    fn apply_budget(self, mut budget: Budget) -> Budget {
        if let Some(value) = self.query_conditioning {
            budget.query_conditioning = value;
        }
        if let Some(value) = self.relevance_ratio {
            budget.relevance_ratio = value;
        }
        if let Some(value) = self.dedup_similarity {
            budget.dedup_similarity = value;
        }
        if let Some(value) = self.max_depth {
            budget.max_depth = value;
        }
        budget
    }

    fn apply_config(self, mut config: Config) -> Config {
        if let Some(value) = self.graph_slot_cap {
            config.graph_slot_cap = value;
        }
        if let Some(value) = self.graph_seed_cap {
            config.graph_seed_cap = value;
        }
        if let Some(value) = self.graph_weight {
            config.graph_weight = value;
        }
        if let Some(value) = self.similarity_link_cap {
            config.similarity_link_cap = value;
        }
        config.budget = self.apply_budget(config.budget);
        config
    }
}

#[derive(Clone, Copy)]
struct OfflineRunOptions {
    graph_diagnostics: bool,
    policy: OfflinePolicyOverrides,
}

async fn retrieve_ids(
    mem: &Memory,
    query: &str,
    seeds: usize,
    budget: Budget,
    node_of: &HashMap<NodeId, u32>,
) -> Vec<u32> {
    mem.retrieve_seeded(query, seeds, budget, StatusFilter::ACTIVE, &[])
        .await
        .unwrap()
        .into_iter()
        .filter_map(|r| node_of.get(&r.node.id()).copied())
        .collect()
}

// ---- metrics --------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct Agg {
    n: usize,
    hit: usize,
    recall: f64,
    mrr: f64,
}
impl Agg {
    fn add(&mut self, gold: &[u32], retrieved: &[u32], k: usize) {
        self.n += 1;
        let topk = &retrieved[..retrieved.len().min(k)];
        let found = gold.iter().filter(|g| topk.contains(g)).count();
        if found > 0 {
            self.hit += 1;
        }
        self.recall += found as f64 / gold.len() as f64;
        if let Some(pos) = topk.iter().position(|r| gold.contains(r)) {
            self.mrr += 1.0 / (pos as f64 + 1.0);
        }
    }
    fn cell(&self) -> String {
        if self.n == 0 {
            return format!("{:>6} {:>6} {:>6}", "-", "-", "-");
        }
        format!(
            "{:>6.3} {:>6.3} {:>6.3}",
            self.hit as f64 / self.n as f64,
            self.mrr / self.n as f64,
            self.recall / self.n as f64,
        )
    }

    fn summary(&self) -> RetrievalSummary {
        if self.n == 0 {
            return RetrievalSummary {
                questions: 0,
                hit_at_k: 0.0,
                mrr: 0.0,
                recall_at_k: 0.0,
            };
        }
        RetrievalSummary {
            questions: self.n,
            hit_at_k: self.hit as f64 / self.n as f64,
            mrr: self.mrr / self.n as f64,
            recall_at_k: self.recall / self.n as f64,
        }
    }
}

#[derive(Clone, Serialize)]
struct RetrievalSummary {
    questions: usize,
    hit_at_k: f64,
    mrr: f64,
    recall_at_k: f64,
}

/// Evaluate one condition (already-ingested memory + a budget) over the test set.
async fn eval(
    mem: &Memory,
    ds: &Dataset,
    seeds: usize,
    budget: Budget,
    k: usize,
    node_of: &HashMap<NodeId, u32>,
) -> (Agg, Agg) {
    let (mut single, mut multi) = (Agg::default(), Agg::default());
    for q in &ds.questions {
        // Recall is the single/multi factoid story; the hard categories live in Layer 2.
        match q.cat {
            Cat::Single | Cat::Multi => {}
            _ => continue,
        }
        let got = retrieve_ids(mem, &q.query, seeds, budget, node_of).await;
        match q.cat {
            Cat::Single => single.add(&q.gold, &got, k),
            Cat::Multi => multi.add(&q.gold, &got, k),
            _ => {}
        }
    }
    (single, multi)
}

/// Explicit self-training ablation: repeatedly retrieve each entity under a config
/// with query-time edge creation enabled. This is deliberately not the primary
/// trained condition: exposure is not grounded proof that the surfaced facts helped.
async fn self_train(mem: &Memory, reps: usize) {
    for _ in 0..reps {
        for &name in ENTITIES {
            let _ = mem
                .retrieve_seeded(name, 8, budget_mneme(), StatusFilter::ACTIVE, &[])
                .await
                .unwrap();
        }
    }
}

/// Train with the substrate's intended strong signal: known-useful transitions.
/// The synthetic corpus gives us exact codename→attribute trails, so feedback is
/// grounded and auditable rather than inferred from mere co-retrieval. This is
/// transductive same-corpus training, not a claim of held-out generalization.
async fn grounded_train(mem: &Memory, ds: &Dataset, id_of: &HashMap<u32, NodeId>, reps: usize) {
    for _ in 0..reps {
        for entity in &ds.by_entity {
            let prior = id_of[&entity.codename_fact];
            mem.apply_feedback(None, prior, Signal::RelevantNew)
                .await
                .expect("ground codename node");
            for fact in &entity.attr_facts {
                mem.apply_feedback(Some(prior), id_of[fact], Signal::RelevantNew)
                    .await
                    .expect("ground codename-to-attribute edge");
            }
        }
    }
}

/// A deterministic derangement used by the adversarial graph condition. A cyclic
/// successor is deliberately simple enough to audit from the artifact while still
/// assigning every codename exactly one wrong entity and every entity exactly one
/// wrong codename.
fn wrong_entity_index(entity: usize, entity_count: usize) -> usize {
    assert!(
        entity_count > 1,
        "adversarial graph corruption needs at least two entities"
    );
    (entity + 1) % entity_count
}

/// The false grounded-looking transitions injected by the adversarial condition.
/// This plan has exactly the same cardinality as the clean codename-to-attribute
/// plan, but no pair points at an attribute of the codename's true entity.
fn noisy_grounded_fact_pairs(ds: &Dataset) -> Vec<(u32, u32)> {
    ds.by_entity
        .iter()
        .enumerate()
        .flat_map(|(entity_index, entity)| {
            let wrong = &ds.by_entity[wrong_entity_index(entity_index, ds.by_entity.len())];
            wrong
                .attr_facts
                .iter()
                .copied()
                .map(move |attribute| (entity.codename_fact, attribute))
        })
        .collect()
}

/// Add false evidence only after the clean grounded condition has been measured.
/// `RelevantNew` creates/reinforces forward-only `Transition` edges, matching the
/// clean edge signal and repetition count without pretending retrieval exposure is
/// proof. Existing clean evidence remains in place, yielding a controlled 1:1
/// signal-to-corruption stress test rather than replacing truth with noise.
async fn inject_noisy_grounded_transitions(
    mem: &Memory,
    ds: &Dataset,
    id_of: &HashMap<u32, NodeId>,
    reps: usize,
) -> usize {
    let pairs = noisy_grounded_fact_pairs(ds);
    for _ in 0..reps {
        for &(prior_fact, wrong_attribute_fact) in &pairs {
            mem.apply_feedback(
                Some(id_of[&prior_fact]),
                id_of[&wrong_attribute_fact],
                Signal::RelevantNew,
            )
            .await
            .expect("inject false codename-to-attribute transition");
        }
    }
    pairs.len()
}

// ---- conditions -----------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    Flat,
    Mneme,
    /// Explicitly enabled co-retrieval self-training ablation.
    Study,
    /// Grounded explicit feedback over known-relevant transitions.
    Train,
    Links,
}

fn budget_for(kind: Kind, k: usize) -> Budget {
    match kind {
        Kind::Flat => budget_flat(k),
        _ => budget_mneme(),
    }
}

/// Build a fresh store for one condition: ingest all facts, then apply the
/// condition's graph treatment — explicit self-training, grounded feedback, or
/// hand-built links. This explicitly preserves the legacy ingest-time similarity
/// graph even though production defaults no longer build it. Each condition gets
/// its own store so training cannot leak between conditions.
async fn build_condition(
    kind: Kind,
    lexical: bool,
    ds: &Dataset,
    training_reps: usize,
) -> (Memory, HashMap<NodeId, u32>) {
    let config = match kind {
        Kind::Study => Config {
            // Preserve the historical prior and self-training treatment rather
            // than inheriting the ordinary zero-floor arrival policy.
            similarity_link_cap: 5,
            min_similarity_links: 2,
            coretrieval_link_cap: 6,
            ..Config::default()
        },
        _ => Config {
            similarity_link_cap: 5,
            min_similarity_links: 2,
            ..Config::default()
        },
    };
    let mem = eval_memory_with_config(make_embedder(lexical), config).mem;
    let (node_of, id_of) = ingest_all(&mem, ds).await;
    match kind {
        Kind::Study => self_train(&mem, training_reps).await,
        Kind::Train => grounded_train(&mem, ds, &id_of, training_reps).await,
        Kind::Links => link_entities(&mem, ds, &id_of).await,
        Kind::Flat | Kind::Mneme => {}
    }
    (mem, node_of)
}

// ---- Offline baseline + scale report -------------------------------------

const OFFLINE_REPORT_SCHEMA_VERSION: u32 = 9;
const OFFLINE_REPORT_SUITE: &str = "mneme-offline-baselines";

#[derive(Serialize)]
struct OfflineReport {
    schema_version: u32,
    suite: &'static str,
    build_profile: &'static str,
    backend: &'static str,
    embedder: &'static str,
    embedder_fingerprint: EmbeddingFingerprint,
    source_revision: String,
    command: Vec<String>,
    machine_label: String,
    variant: String,
    seed: u64,
    id_seeds: Vec<u64>,
    k: usize,
    dense_seeds: usize,
    training_repetitions: usize,
    notes: Vec<&'static str>,
    runs: Vec<ScaleReport>,
}

struct ReportMetadata {
    source_revision: String,
    command: Vec<String>,
    machine_label: String,
    variant: String,
}

#[derive(Serialize)]
struct ScaleReport {
    nodes: usize,
    id_seed: u64,
    retrieval_questions: usize,
    elapsed_ms: f64,
    embedding_dimension: usize,
    raw_vector_bytes_per_index: usize,
    builds: BuildReport,
    topology: TopologyReport,
    adversarial_graph_corruption: AdversarialGraphCorruptionReport,
    conditions: Vec<ConditionReport>,
}

#[derive(Serialize)]
struct BuildReport {
    bm25_ms: f64,
    graph_ingest_ms: f64,
    graph_training_ms: f64,
    graph_corruption_ms: f64,
}

#[derive(Serialize)]
struct TopologyReport {
    cold_before_queries: GraphStats,
    cold_after_queries: GraphStats,
    trained_before_queries: GraphStats,
    trained_after_queries: GraphStats,
    noisy_before_queries: GraphStats,
    noisy_after_queries: GraphStats,
}

#[derive(Serialize)]
struct AdversarialGraphCorruptionReport {
    condition: &'static str,
    applied_after_condition: &'static str,
    wrong_entity_mapping: &'static str,
    edge_kind: &'static str,
    feedback_signal: &'static str,
    source_codename_priors: usize,
    clean_transition_pairs: usize,
    false_transition_pairs: usize,
    repetitions_per_pair: usize,
}

#[derive(Clone, Serialize)]
struct GraphStats {
    nodes: usize,
    active_nodes: usize,
    archived_nodes: usize,
    stored_edges: usize,
    associative_edges: usize,
    transition_edges: usize,
    bridge_edges: usize,
    structural_edges: usize,
    reinforced_edges: usize,
    pending_edge_interference: u64,
    mean_edge_weight: f64,
    mean_stored_edges_per_node: f64,
    max_incident_degree: usize,
    stored_directed_density: f64,
}

#[derive(Serialize)]
struct ConditionReport {
    name: &'static str,
    mechanism: &'static str,
    engine_policy: Option<EffectiveRetrievalPolicy>,
    retrieval: RetrievalBreakdown,
    latency: LatencyReport,
    context: ContextReport,
    /// Opt-in, evaluator-reconstructed stage traces. Omitted from ordinary
    /// artifacts so a release benchmark stays compact.
    #[serde(skip_serializing_if = "Option::is_none")]
    graph_diagnostics: Option<GraphDiagnosticReport>,
}

#[derive(Serialize)]
struct GraphDiagnosticReport {
    method: &'static str,
    summary: GraphDiagnosticSummary,
    queries: Vec<GraphQueryDiagnostic>,
}

#[derive(Default, Serialize)]
struct GraphDiagnosticSummary {
    overall: GraphDiagnosticCounters,
    by_category: BTreeMap<&'static str, GraphDiagnosticCounters>,
}

#[derive(Default, Serialize)]
struct GraphDiagnosticCounters {
    queries: usize,
    base_hits_at_k: usize,
    final_hits_at_k: usize,
    graph_rescues_at_k: usize,
    base_regressions_at_k: usize,
    queries_with_gold_edge_from_root: usize,
    queries_with_trained_gold_edge_from_root: usize,
    queries_with_gold_in_traversal: usize,
    queries_with_gold_admitted_by_graph_quota: usize,
    graph_interventions: usize,
    graph_contributions_at_k: usize,
    displaced_base_hits_at_k: usize,
    direct_order_changes: usize,
    failure_classes: BTreeMap<&'static str, usize>,
}

#[derive(Serialize)]
struct GraphQueryDiagnostic {
    query_index: usize,
    category: &'static str,
    query: String,
    gold_fact_ids: Vec<u32>,
    base_ranking: Vec<DiagnosticRankedHit>,
    base_gold_rank: Option<usize>,
    roots: Vec<DiagnosticRankedHit>,
    gold_edges_from_roots: Vec<GoldEdgeDiagnostic>,
    traversal_candidates: Vec<DiagnosticRankedHit>,
    traversal_gold_rank: Option<usize>,
    expansion_floor: f32,
    eligible_expansions: Vec<DiagnosticRankedHit>,
    eligible_gold_rank: Option<usize>,
    graph_slot_cap: usize,
    graph_interventions: Vec<GraphInterventionDiagnostic>,
    gold_received_graph_intervention: bool,
    final_ranking: Vec<DiagnosticRankedHit>,
    final_gold_rank: Option<usize>,
    graph_contributions_at_k: Vec<u32>,
    displaced_base_hits_at_k: Vec<u32>,
    direct_relative_order_changed: bool,
    failure_class: &'static str,
}

#[derive(Clone, Serialize)]
struct DiagnosticRankedHit {
    rank: usize,
    fact_id: u32,
    score: f32,
}

#[derive(Serialize)]
struct GraphInterventionDiagnostic {
    intervention_rank: usize,
    expansion_rank: usize,
    fact_id: u32,
    kind: &'static str,
    prior_contribution: Option<f32>,
    graph_contribution: f32,
}

#[derive(Serialize)]
struct GoldEdgeDiagnostic {
    root_fact_id: u32,
    gold_fact_id: u32,
    direction: &'static str,
    kind: &'static str,
    weight: f32,
    trials: u32,
    trained: bool,
}

#[derive(Clone, Copy, Serialize)]
struct EffectiveRetrievalPolicy {
    budget: BudgetReport,
    lexical_k: usize,
    rrf_constant: f32,
    dense_weight: f32,
    lexical_weight: f32,
    graph_seed_cap: usize,
    graph_weight: f32,
    graph_slot_cap: usize,
    similarity_link_cap: usize,
    similarity_link_threshold: f32,
    min_similarity_links: usize,
    coretrieval_link_cap: usize,
}

impl EffectiveRetrievalPolicy {
    fn new(config: &Config, budget: Budget) -> Self {
        Self {
            budget: BudgetReport::from(budget),
            lexical_k: config.lexical_k,
            rrf_constant: config.rrf_constant,
            dense_weight: config.dense_weight,
            lexical_weight: config.lexical_weight,
            graph_seed_cap: config.graph_seed_cap,
            graph_weight: config.graph_weight,
            graph_slot_cap: config.graph_slot_cap,
            similarity_link_cap: config.similarity_link_cap,
            similarity_link_threshold: config.similarity_link_threshold,
            min_similarity_links: config.min_similarity_links,
            coretrieval_link_cap: config.coretrieval_link_cap,
        }
    }
}

#[derive(Clone, Copy, Serialize)]
struct BudgetReport {
    max_nodes: usize,
    max_depth: u8,
    min_relevance: f32,
    explore: f32,
    relevance_ratio: f32,
    dedup_similarity: f32,
    query_conditioning: f32,
}

impl From<Budget> for BudgetReport {
    fn from(budget: Budget) -> Self {
        Self {
            max_nodes: budget.max_nodes,
            max_depth: budget.max_depth,
            min_relevance: budget.min_relevance,
            explore: budget.explore,
            relevance_ratio: budget.relevance_ratio,
            dedup_similarity: budget.dedup_similarity,
            query_conditioning: budget.query_conditioning,
        }
    }
}

#[derive(Serialize)]
struct RetrievalBreakdown {
    single_hop: RetrievalSummary,
    multi_hop: RetrievalSummary,
    overall: RetrievalSummary,
}

#[derive(Serialize)]
struct LatencyReport {
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
    queries_per_second: f64,
}

#[derive(Serialize)]
struct ContextReport {
    mean_candidates_returned: f64,
    p95_candidates_returned: usize,
    mean_items_injected_at_k: f64,
    p95_items_injected_at_k: usize,
    mean_chars_injected_at_k: f64,
    p95_chars_injected_at_k: usize,
    mean_estimated_tokens_at_k: f64,
}

#[derive(Default)]
struct OfflineAcc {
    single: Agg,
    multi: Agg,
    overall: Agg,
    latency_ms: Vec<f64>,
    candidates: Vec<usize>,
    injected: Vec<usize>,
    context_chars: Vec<usize>,
}

impl OfflineAcc {
    fn add(
        &mut self,
        q: &Question,
        retrieved: &[u32],
        k: usize,
        elapsed: Duration,
        fact_chars: &HashMap<u32, usize>,
    ) {
        match q.cat {
            Cat::Single => self.single.add(&q.gold, retrieved, k),
            Cat::Multi => self.multi.add(&q.gold, retrieved, k),
            _ => return,
        }
        self.overall.add(&q.gold, retrieved, k);
        self.latency_ms.push(elapsed.as_secs_f64() * 1_000.0);
        self.candidates.push(retrieved.len());
        let top = &retrieved[..retrieved.len().min(k)];
        self.injected.push(top.len());
        // Two bytes per item approximate the "- " list framing used in prompts.
        self.context_chars.push(
            top.iter()
                .map(|id| fact_chars.get(id).copied().unwrap_or(0) + 2)
                .sum(),
        );
    }

    fn finish(
        mut self,
        name: &'static str,
        mechanism: &'static str,
        engine_policy: Option<EffectiveRetrievalPolicy>,
    ) -> ConditionReport {
        self.latency_ms.sort_by(f64::total_cmp);
        self.candidates.sort_unstable();
        self.injected.sort_unstable();
        self.context_chars.sort_unstable();
        let total_seconds = self.latency_ms.iter().sum::<f64>() / 1_000.0;
        let queries_per_second = if total_seconds > 0.0 {
            self.latency_ms.len() as f64 / total_seconds
        } else {
            0.0
        };
        ConditionReport {
            name,
            mechanism,
            engine_policy,
            retrieval: RetrievalBreakdown {
                single_hop: self.single.summary(),
                multi_hop: self.multi.summary(),
                overall: self.overall.summary(),
            },
            latency: LatencyReport {
                mean_ms: mean_f64(&self.latency_ms),
                p50_ms: percentile_f64(&self.latency_ms, 0.50),
                p95_ms: percentile_f64(&self.latency_ms, 0.95),
                max_ms: self.latency_ms.last().copied().unwrap_or(0.0),
                queries_per_second,
            },
            context: ContextReport {
                mean_candidates_returned: mean_usize(&self.candidates),
                p95_candidates_returned: percentile_usize(&self.candidates, 0.95),
                mean_items_injected_at_k: mean_usize(&self.injected),
                p95_items_injected_at_k: percentile_usize(&self.injected, 0.95),
                mean_chars_injected_at_k: mean_usize(&self.context_chars),
                p95_chars_injected_at_k: percentile_usize(&self.context_chars, 0.95),
                // Explicitly an estimate, not provider tokenizer accounting.
                mean_estimated_tokens_at_k: mean_usize(&self.context_chars) / 4.0,
            },
            graph_diagnostics: None,
        }
    }
}

fn mean_f64(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn mean_usize(xs: &[usize]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<usize>() as f64 / xs.len() as f64
    }
}

fn percentile_index(len: usize, p: f64) -> usize {
    if len == 0 {
        return 0;
    }
    (((len - 1) as f64 * p.clamp(0.0, 1.0)).ceil() as usize).min(len - 1)
}

fn percentile_f64(sorted: &[f64], p: f64) -> f64 {
    sorted
        .get(percentile_index(sorted.len(), p))
        .copied()
        .unwrap_or(0.0)
}

fn percentile_usize(sorted: &[usize], p: f64) -> usize {
    sorted
        .get(percentile_index(sorted.len(), p))
        .copied()
        .unwrap_or(0)
}

fn retrieval_questions(ds: &Dataset) -> impl Iterator<Item = &Question> {
    ds.questions
        .iter()
        .filter(|q| matches!(q.cat, Cat::Single | Cat::Multi))
}

fn fact_char_lengths(ds: &Dataset) -> HashMap<u32, usize> {
    ds.facts
        .iter()
        .map(|f| (f.id, f.text.chars().count()))
        .collect()
}

async fn graph_stats(eval: &EvalMemory) -> GraphStats {
    let nodes = eval
        .graph
        .all_nodes(ColdPath::acquire())
        .await
        .expect("inspect eval nodes");
    let edges = eval
        .graph
        .all_edges(ColdPath::acquire())
        .await
        .expect("inspect eval edges");
    let mut active = 0;
    let mut archived = 0;
    for node in &nodes {
        if node.is_active() {
            active += 1;
        } else if node.is_archived() {
            archived += 1;
        }
    }
    let mut associative = 0;
    let mut transitions = 0;
    let mut bridges = 0;
    let mut structural = 0;
    let mut reinforced = 0;
    let mut interference = 0u64;
    let mut weight_sum = 0.0;
    let mut incident: HashMap<NodeId, usize> = HashMap::new();
    for edge in &edges {
        match edge.kind {
            EdgeKind::Associative => associative += 1,
            EdgeKind::Transition => transitions += 1,
            EdgeKind::Bridge => bridges += 1,
            EdgeKind::Supersedes | EdgeKind::DerivedFrom => structural += 1,
        }
        if edge.trials() > 0 {
            reinforced += 1;
        }
        interference += edge.interference() as u64;
        weight_sum += edge.weight() as f64;
        *incident.entry(edge.from).or_insert(0) += 1;
        *incident.entry(edge.to).or_insert(0) += 1;
    }
    let n = nodes.len();
    let e = edges.len();
    GraphStats {
        nodes: n,
        active_nodes: active,
        archived_nodes: archived,
        stored_edges: e,
        associative_edges: associative,
        transition_edges: transitions,
        bridge_edges: bridges,
        structural_edges: structural,
        reinforced_edges: reinforced,
        pending_edge_interference: interference,
        mean_edge_weight: if e == 0 { 0.0 } else { weight_sum / e as f64 },
        mean_stored_edges_per_node: if n == 0 { 0.0 } else { e as f64 / n as f64 },
        max_incident_degree: incident.values().copied().max().unwrap_or(0),
        stored_directed_density: if n < 2 {
            0.0
        } else {
            e as f64 / (n * (n - 1)) as f64
        },
    }
}

fn eval_bm25(index: &Bm25Index, ds: &Dataset, k: usize) -> ConditionReport {
    let chars = fact_char_lengths(ds);
    let mut acc = OfflineAcc::default();
    for q in retrieval_questions(ds) {
        let started = Instant::now();
        let got = index.search(&q.query, k);
        acc.add(q, &got, k, started.elapsed(), &chars);
    }
    acc.finish("bm25", "dependency-free sparse BM25", None)
}

async fn eval_dense(
    eval: &EvalMemory,
    ds: &Dataset,
    seeds: usize,
    k: usize,
    node_of: &HashMap<NodeId, u32>,
) -> ConditionReport {
    let chars = fact_char_lengths(ds);
    let mut acc = OfflineAcc::default();
    for q in retrieval_questions(ds) {
        let started = Instant::now();
        let query = eval
            .embedder
            .embed_query(&q.query)
            .await
            .expect("embed dense eval query");
        let got: Vec<u32> = eval
            .vectors
            .ann(&query, seeds.max(k), StatusFilter::ACTIVE)
            .await
            .expect("dense eval ANN")
            .into_iter()
            .filter_map(|hit| node_of.get(&hit.id).copied())
            .take(k)
            .collect();
        acc.add(q, &got, k, started.elapsed(), &chars);
    }
    acc.finish("dense", "read-only dense ANN, no graph spread", None)
}

fn diagnostic_scored_order(a: &Scored, b: &Scored) -> std::cmp::Ordering {
    b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id))
}

/// Evaluation-only mirror of the engine's dense+sparse RRF stage. Keeping this
/// here lets the diagnostic inspect ranks without widening the production API.
/// The trace records its method/version so a future fusion change cannot be
/// mistaken for ground truth from an opaque engine build.
fn diagnostic_rrf(
    primary: &[Scored],
    secondary: &[Scored],
    constant: f32,
    primary_weight: f32,
    secondary_weight: f32,
) -> Vec<Scored> {
    if secondary.is_empty() {
        return primary.to_vec();
    }
    let primary_weight = primary_weight.max(0.0);
    let secondary_weight = secondary_weight.max(0.0);
    if primary_weight == 0.0 && secondary_weight == 0.0 {
        return primary.to_vec();
    }
    let constant = if constant.is_finite() {
        constant.max(1.0)
    } else {
        60.0
    };
    let mut fused: HashMap<NodeId, f32> = HashMap::new();
    for (rank, hit) in primary.iter().enumerate() {
        *fused.entry(hit.id).or_default() += primary_weight / (constant + rank as f32 + 1.0);
    }
    for (rank, hit) in secondary.iter().enumerate() {
        *fused.entry(hit.id).or_default() += secondary_weight / (constant + rank as f32 + 1.0);
    }
    let raw_top = fused
        .values()
        .copied()
        .max_by(f32::total_cmp)
        .unwrap_or(1.0);
    let target_top = primary
        .iter()
        .map(|hit| hit.score)
        .filter(|score| score.is_finite() && *score > 0.0)
        .max_by(f32::total_cmp)
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
    let scale = if raw_top > 0.0 {
        target_top / raw_top
    } else {
        1.0
    };
    let mut out: Vec<Scored> = fused
        .into_iter()
        .map(|(id, score)| Scored {
            id,
            score: (score * scale).clamp(0.0, 1.0),
        })
        .collect();
    out.sort_by(diagnostic_scored_order);
    out
}

/// Evaluation mirror of the engine's bounded fused/sparse/dense root selector.
/// Eligibility comes from the fused leg; the source lanes contribute a diverse
/// order and every admitted root gets the engine's unit activation.
fn diagnostic_graph_roots(
    eligible: &[Scored],
    sparse: &[Scored],
    dense: &[Scored],
    limit: usize,
) -> Vec<Scored> {
    let eligible_ids: HashSet<NodeId> = eligible
        .iter()
        .filter(|hit| hit.score.is_finite() && hit.score > 0.0)
        .map(|hit| hit.id)
        .collect();
    let lanes = [eligible, dense, sparse];
    let mut cursors = [0usize; 3];
    let mut seen = HashSet::new();
    let mut roots = Vec::with_capacity(limit.min(eligible_ids.len()));
    while roots.len() < limit && roots.len() < eligible_ids.len() {
        let mut progressed = false;
        for (lane_index, lane) in lanes.iter().enumerate() {
            while let Some(hit) = lane.get(cursors[lane_index]) {
                cursors[lane_index] += 1;
                if !eligible_ids.contains(&hit.id) {
                    continue;
                }
                if seen.insert(hit.id) {
                    roots.push(Scored {
                        id: hit.id,
                        score: 1.0,
                    });
                    progressed = true;
                    break;
                }
            }
            if roots.len() >= limit {
                break;
            }
        }
        if !progressed {
            break;
        }
    }
    roots.sort_by_key(|root| root.id);
    roots
}

/// Evaluation mirror of the engine's max-contribution graph fusion admission.
/// The tuple is `(node, expansion_rank, was_direct, prior, graph)`; only entries
/// that actually alter a score consume the bounded intervention quota.
fn diagnostic_graph_interventions(
    primary: &[Scored],
    secondary: &[Scored],
    constant: f32,
    primary_weight: f32,
    secondary_weight: f32,
    cap: usize,
) -> Vec<(NodeId, usize, bool, Option<f32>, f32)> {
    if secondary.is_empty() || cap == 0 || !secondary_weight.is_finite() || secondary_weight <= 0.0
    {
        return Vec::new();
    }
    let constant = if constant.is_finite() {
        constant.max(1.0)
    } else {
        60.0
    };
    let primary_weight = primary_weight.max(0.0);
    let secondary_weight = secondary_weight.min(1.0 - f32::EPSILON);
    let mut scores: HashMap<NodeId, f32> = primary
        .iter()
        .enumerate()
        .map(|(rank, hit)| (hit.id, primary_weight / (constant + rank as f32 + 1.0)))
        .collect();
    let direct_ids: HashSet<NodeId> = primary.iter().map(|hit| hit.id).collect();
    let mut interventions = Vec::with_capacity(cap);
    for (rank, hit) in secondary.iter().enumerate() {
        if interventions.len() >= cap {
            break;
        }
        let graph = secondary_weight / (constant + rank as f32 + 1.0);
        let prior = scores.get(&hit.id).copied();
        if prior.is_some_and(|score| graph <= score) {
            continue;
        }
        scores.insert(hit.id, graph);
        interventions.push((hit.id, rank + 1, direct_ids.contains(&hit.id), prior, graph));
    }
    interventions
}

fn diagnostic_ranked_hits(
    scored: &[Scored],
    node_of: &HashMap<NodeId, u32>,
) -> Vec<DiagnosticRankedHit> {
    scored
        .iter()
        .enumerate()
        .filter_map(|(rank, hit)| {
            node_of.get(&hit.id).map(|fact_id| DiagnosticRankedHit {
                rank: rank + 1,
                fact_id: *fact_id,
                score: hit.score,
            })
        })
        .collect()
}

fn best_gold_rank(ranking: &[DiagnosticRankedHit], gold: &[u32]) -> Option<usize> {
    ranking
        .iter()
        .find(|hit| gold.contains(&hit.fact_id))
        .map(|hit| hit.rank)
}

fn edge_kind_name(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Associative => "associative",
        EdgeKind::Transition => "transition",
        EdgeKind::Bridge => "bridge",
        EdgeKind::Supersedes => "supersedes",
        EdgeKind::DerivedFrom => "derived_from",
    }
}

#[expect(clippy::too_many_arguments)]
fn classify_graph_query(
    base_gold_rank: Option<usize>,
    final_gold_rank: Option<usize>,
    k: usize,
    roots_empty: bool,
    has_gold_edge: bool,
    traversal_gold_rank: Option<usize>,
    eligible_gold_rank: Option<usize>,
    gold_received_graph_intervention: bool,
    graph_contributed_at_k: bool,
) -> &'static str {
    let base_hit = base_gold_rank.is_some_and(|rank| rank <= k);
    let final_hit = final_gold_rank.is_some_and(|rank| rank <= k);
    match (base_hit, final_hit) {
        (true, true) => return "base_hit_retained",
        (false, true) => return "graph_rescue",
        (true, false) => {
            return if graph_contributed_at_k {
                "base_gold_displaced_by_graph_candidate"
            } else {
                "base_gold_removed_by_postprocessing"
            };
        }
        (false, false) => {}
    }
    if roots_empty {
        return "no_eligible_roots";
    }
    let Some(_traversal_rank) = traversal_gold_rank else {
        return if has_gold_edge {
            "gold_edge_not_reached_by_traversal"
        } else {
            "no_gold_edge_from_roots"
        };
    };
    let Some(_eligible_rank) = eligible_gold_rank else {
        return "gold_filtered_by_expansion_floor";
    };
    if !gold_received_graph_intervention {
        return if base_gold_rank.is_some() {
            "direct_gold_graph_evidence_not_admitted"
        } else {
            "gold_outside_graph_intervention_quota"
        };
    }
    if final_gold_rank.is_some() {
        "admitted_gold_outside_top_k"
    } else {
        "gold_dropped_after_graph_admission"
    }
}

#[expect(clippy::too_many_arguments)]
async fn diagnose_graph_query(
    eval: &EvalMemory,
    query_index: usize,
    q: &Question,
    seeds: usize,
    k: usize,
    budget: Budget,
    node_of: &HashMap<NodeId, u32>,
    final_ranking: Vec<DiagnosticRankedHit>,
) -> GraphQueryDiagnostic {
    let config = eval.mem.config();
    let query_embedding = eval
        .embedder
        .embed_query(&q.query)
        .await
        .expect("embed graph diagnostic query");
    let mut dense = eval
        .vectors
        .ann(&query_embedding, seeds, StatusFilter::ACTIVE)
        .await
        .expect("dense graph diagnostic seeds");
    dense.sort_by(diagnostic_scored_order);
    let lexical_k = config.lexical_k.min(seeds).min(64);
    let mut sparse = if lexical_k == 0 {
        Vec::new()
    } else {
        eval.lexical
            .search(&q.query, lexical_k, StatusFilter::ACTIVE)
            .await
            .expect("sparse graph diagnostic seeds")
    };
    sparse.sort_by(diagnostic_scored_order);
    let mut direct = if sparse.is_empty() {
        dense.clone()
    } else {
        diagnostic_rrf(
            &dense,
            &sparse,
            config.rrf_constant,
            config.dense_weight,
            config.lexical_weight,
        )
    };
    direct.sort_by(diagnostic_scored_order);

    let direct_floor = {
        let top = direct.first().map_or(0.0, |hit| hit.score);
        budget.min_relevance.max(top * budget.relevance_ratio)
    };
    let direct_leg: Vec<Scored> = direct
        .iter()
        .copied()
        .filter(|hit| hit.score >= direct_floor)
        .collect();
    let roots = diagnostic_graph_roots(
        &direct_leg,
        &sparse,
        &dense,
        config.graph_seed_cap.min(budget.max_nodes),
    );
    let root_ids: HashSet<NodeId> = roots.iter().map(|hit| hit.id).collect();
    let spread = if budget.max_depth == 0
        || budget.max_nodes == 0
        || config.graph_seed_cap == 0
        || config.graph_slot_cap == 0
        || !config.graph_weight.is_finite()
        || config.graph_weight <= 0.0
        || roots.is_empty()
    {
        Vec::new()
    } else {
        eval.traversal
            .spread(
                &roots,
                budget,
                Some(&query_embedding),
                TraversalScope::new(StatusFilter::ACTIVE),
            )
            .await
            .expect("graph diagnostic traversal")
    };
    let mut expansions: Vec<Scored> = spread
        .into_iter()
        .filter(|hit| !root_ids.contains(&hit.id) && hit.score.is_finite())
        .collect();
    expansions.sort_by(diagnostic_scored_order);
    let expansion_floor = {
        let top = expansions.first().map_or(0.0, |hit| hit.score);
        budget.min_relevance.max(top * budget.relevance_ratio)
    };
    let mut eligible: Vec<Scored> = expansions
        .iter()
        .copied()
        .filter(|hit| hit.score >= expansion_floor)
        .collect();
    eligible.truncate(budget.max_nodes);

    let node_by_fact: HashMap<u32, NodeId> =
        node_of.iter().map(|(node, fact)| (*fact, *node)).collect();
    let graph_slot_cap = config.graph_slot_cap.min(budget.max_nodes);
    let intervention_rows = diagnostic_graph_interventions(
        &direct_leg,
        &eligible,
        config.rrf_constant,
        1.0,
        config.graph_weight,
        graph_slot_cap,
    );
    let gold_node_ids: HashSet<NodeId> = q
        .gold
        .iter()
        .filter_map(|fact| node_by_fact.get(fact).copied())
        .collect();
    let gold_received_graph_intervention = intervention_rows
        .iter()
        .any(|(node, ..)| gold_node_ids.contains(node));
    let graph_interventions: Vec<GraphInterventionDiagnostic> = intervention_rows
        .iter()
        .enumerate()
        .filter_map(
            |(intervention_rank, (node, expansion_rank, was_direct, prior, graph))| {
                node_of
                    .get(node)
                    .map(|fact_id| GraphInterventionDiagnostic {
                        intervention_rank: intervention_rank + 1,
                        expansion_rank: *expansion_rank,
                        fact_id: *fact_id,
                        kind: if *was_direct {
                            "direct_promotion"
                        } else {
                            "novel_candidate"
                        },
                        prior_contribution: *prior,
                        graph_contribution: *graph,
                    })
            },
        )
        .collect();
    let mut gold_edges_from_roots = Vec::new();
    for root in &roots {
        let Some(root_fact_id) = node_of.get(&root.id).copied() else {
            continue;
        };
        for gold_fact_id in &q.gold {
            let Some(gold_node) = node_by_fact.get(gold_fact_id).copied() else {
                continue;
            };
            for (from, to, direction) in [
                (root.id, gold_node, "root_to_gold"),
                (gold_node, root.id, "gold_to_root"),
            ] {
                if let Some(edge) = eval
                    .graph
                    .get_edge(from, to)
                    .await
                    .expect("inspect graph diagnostic edge")
                {
                    gold_edges_from_roots.push(GoldEdgeDiagnostic {
                        root_fact_id,
                        gold_fact_id: *gold_fact_id,
                        direction,
                        kind: edge_kind_name(edge.kind),
                        weight: edge.weight(),
                        trials: edge.trials(),
                        trained: edge.trials() > 0,
                    });
                }
            }
        }
    }

    let base_ranking = diagnostic_ranked_hits(&direct, node_of);
    let roots = diagnostic_ranked_hits(&roots, node_of);
    let traversal_candidates = diagnostic_ranked_hits(&expansions, node_of);
    let eligible_expansions = diagnostic_ranked_hits(&eligible, node_of);
    let base_gold_rank = best_gold_rank(&base_ranking, &q.gold);
    let traversal_gold_rank = best_gold_rank(&traversal_candidates, &q.gold);
    let eligible_gold_rank = best_gold_rank(&eligible_expansions, &q.gold);
    let final_gold_rank = best_gold_rank(&final_ranking, &q.gold);
    let base_top_k: Vec<u32> = base_ranking.iter().take(k).map(|hit| hit.fact_id).collect();
    let final_top_k: Vec<u32> = final_ranking
        .iter()
        .take(k)
        .map(|hit| hit.fact_id)
        .collect();
    let direct_fact_ids: HashSet<u32> = base_ranking.iter().map(|hit| hit.fact_id).collect();
    let graph_contributions_at_k: Vec<u32> = final_top_k
        .iter()
        .copied()
        .filter(|fact| !direct_fact_ids.contains(fact))
        .collect();
    let displaced_base_hits_at_k: Vec<u32> = base_top_k
        .iter()
        .copied()
        .filter(|fact| !final_top_k.contains(fact))
        .collect();
    let final_direct: Vec<u32> = final_ranking
        .iter()
        .filter(|hit| direct_fact_ids.contains(&hit.fact_id))
        .map(|hit| hit.fact_id)
        .collect();
    let final_direct_set: HashSet<u32> = final_direct.iter().copied().collect();
    let expected_direct: Vec<u32> = base_ranking
        .iter()
        .filter(|hit| final_direct_set.contains(&hit.fact_id))
        .map(|hit| hit.fact_id)
        .collect();
    let direct_relative_order_changed = final_direct != expected_direct;
    let failure_class = classify_graph_query(
        base_gold_rank,
        final_gold_rank,
        k,
        roots.is_empty(),
        !gold_edges_from_roots.is_empty(),
        traversal_gold_rank,
        eligible_gold_rank,
        gold_received_graph_intervention,
        !graph_contributions_at_k.is_empty(),
    );

    GraphQueryDiagnostic {
        query_index,
        category: match q.cat {
            Cat::Single => "single_hop",
            Cat::Multi => "multi_hop",
            _ => unreachable!("diagnostics only run factoid retrieval questions"),
        },
        query: q.query.clone(),
        gold_fact_ids: q.gold.clone(),
        base_ranking,
        base_gold_rank,
        roots,
        gold_edges_from_roots,
        traversal_candidates,
        traversal_gold_rank,
        expansion_floor,
        eligible_expansions,
        eligible_gold_rank,
        graph_slot_cap,
        graph_interventions,
        gold_received_graph_intervention,
        final_ranking,
        final_gold_rank,
        graph_contributions_at_k,
        displaced_base_hits_at_k,
        direct_relative_order_changed,
        failure_class,
    }
}

fn add_graph_diagnostic(
    counter: &mut GraphDiagnosticCounters,
    query: &GraphQueryDiagnostic,
    k: usize,
) {
    counter.queries += 1;
    let base_hit = query.base_gold_rank.is_some_and(|rank| rank <= k);
    let final_hit = query.final_gold_rank.is_some_and(|rank| rank <= k);
    counter.base_hits_at_k += usize::from(base_hit);
    counter.final_hits_at_k += usize::from(final_hit);
    counter.graph_rescues_at_k += usize::from(!base_hit && final_hit);
    counter.base_regressions_at_k += usize::from(base_hit && !final_hit);
    counter.queries_with_gold_edge_from_root +=
        usize::from(!query.gold_edges_from_roots.is_empty());
    counter.queries_with_trained_gold_edge_from_root +=
        usize::from(query.gold_edges_from_roots.iter().any(|edge| edge.trained));
    counter.queries_with_gold_in_traversal += usize::from(query.traversal_gold_rank.is_some());
    counter.queries_with_gold_admitted_by_graph_quota +=
        usize::from(query.gold_received_graph_intervention);
    counter.graph_interventions += query.graph_interventions.len();
    counter.graph_contributions_at_k += query.graph_contributions_at_k.len();
    counter.displaced_base_hits_at_k += query.displaced_base_hits_at_k.len();
    counter.direct_order_changes += usize::from(query.direct_relative_order_changed);
    *counter
        .failure_classes
        .entry(query.failure_class)
        .or_default() += 1;
}

fn summarize_graph_diagnostics(
    queries: &[GraphQueryDiagnostic],
    k: usize,
) -> GraphDiagnosticSummary {
    let mut summary = GraphDiagnosticSummary::default();
    for query in queries {
        add_graph_diagnostic(&mut summary.overall, query, k);
        add_graph_diagnostic(
            summary.by_category.entry(query.category).or_default(),
            query,
            k,
        );
    }
    summary
}

#[expect(clippy::too_many_arguments)]
async fn eval_public_retrieval(
    eval: &EvalMemory,
    ds: &Dataset,
    seeds: usize,
    k: usize,
    budget: Budget,
    node_of: &HashMap<NodeId, u32>,
    name: &'static str,
    mechanism: &'static str,
    diagnostics: bool,
) -> ConditionReport {
    let chars = fact_char_lengths(ds);
    let mut acc = OfflineAcc::default();
    let mut final_rankings = Vec::new();
    for q in retrieval_questions(ds) {
        let started = Instant::now();
        let retrieved = eval
            .mem
            .retrieve_seeded(&q.query, seeds, budget, StatusFilter::ACTIVE, &[])
            .await
            .expect("public eval retrieval");
        let facts: Vec<(u32, f32)> = retrieved
            .iter()
            .filter_map(|hit| node_of.get(&hit.node.id()).map(|fact| (*fact, hit.score)))
            .collect();
        let got: Vec<u32> = facts.iter().map(|(fact, _)| *fact).collect();
        acc.add(q, &got, k, started.elapsed(), &chars);
        if diagnostics {
            final_rankings.push(
                facts
                    .into_iter()
                    .enumerate()
                    .map(|(rank, (fact_id, score))| DiagnosticRankedHit {
                        rank: rank + 1,
                        fact_id,
                        score,
                    })
                    .collect(),
            );
        }
    }
    let mut report = acc.finish(
        name,
        mechanism,
        Some(EffectiveRetrievalPolicy::new(eval.mem.config(), budget)),
    );
    if diagnostics {
        let mut queries = Vec::with_capacity(final_rankings.len());
        for ((query_index, q), final_ranking) in
            retrieval_questions(ds).enumerate().zip(final_rankings)
        {
            queries.push(
                diagnose_graph_query(
                    eval,
                    query_index,
                    q,
                    seeds,
                    k,
                    budget,
                    node_of,
                    final_ranking,
                )
                .await,
            );
        }
        let summary = summarize_graph_diagnostics(&queries, k);
        eprintln!(
            "· graph diagnostics {name}: base={}/{} final={}/{} rescue={} regress={} classes={:?}",
            summary.overall.base_hits_at_k,
            summary.overall.queries,
            summary.overall.final_hits_at_k,
            summary.overall.queries,
            summary.overall.graph_rescues_at_k,
            summary.overall.base_regressions_at_k,
            summary.overall.failure_classes,
        );
        report.graph_diagnostics = Some(GraphDiagnosticReport {
            method: "eval reconstruction of engine RRF+stratified-roots+Traversal::spread+bounded-max-intervention fusion; actual public final ranking",
            summary,
            queries,
        });
    }
    report
}

#[expect(clippy::too_many_arguments)]
async fn run_offline_scale(
    nodes: usize,
    dataset_seed: u64,
    id_seed: u64,
    k: usize,
    seeds: usize,
    training_reps: usize,
    options: OfflineRunOptions,
    embedder: Arc<dyn Embedder>,
) -> ScaleReport {
    let scale_started = Instant::now();
    let ds = pad_dataset(gen_dataset(dataset_seed), nodes, dataset_seed);
    let question_count = retrieval_questions(&ds).count();

    let started = Instant::now();
    let bm25 = Bm25Index::build(&ds.facts);
    let bm25_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let bm25_report = eval_bm25(&bm25, &ds, k);

    let config = options.policy.apply_config(Config::default());
    let controlled_budget = options.policy.apply_budget(budget_mneme());

    // Every condition shares one paired index. All cold conditions finish before
    // feedback is applied, so the trained condition is a true before/after
    // intervention over identical node identities, vectors, and query order.
    // Retrieval exposure telemetry does not influence ranking or edge learning.
    let paired = fixed_eval_memory_with_config(embedder, dataset_seed, id_seed, config);
    let started = Instant::now();
    let (node_of, id_of) = ingest_all(&paired.mem, &ds).await;
    let graph_ingest_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let cold_before = graph_stats(&paired).await;
    let dense_report = eval_dense(&paired, &ds, seeds, k, &node_of).await;
    let hybrid_report = eval_public_retrieval(
        &paired,
        &ds,
        seeds,
        k,
        budget_flat(k),
        &node_of,
        "hybrid-flat",
        "public sparse+dense RRF seed stage, graph spread disabled",
        false,
    )
    .await;
    let current_report = eval_public_retrieval(
        &paired,
        &ds,
        seeds,
        k,
        controlled_budget,
        &node_of,
        "graph-current",
        "cold traversal over the shipped evidence graph (automatic embedding-similarity links disabled)",
        options.graph_diagnostics,
    )
    .await;
    let default_budget_report = eval_public_retrieval(
        &paired,
        &ds,
        seeds,
        k,
        paired.mem.config().budget,
        &node_of,
        "graph-default-budget",
        "cold graph spread under the shipped Config::default retrieval policy",
        options.graph_diagnostics,
    )
    .await;
    let cold_after = graph_stats(&paired).await;

    let started = Instant::now();
    grounded_train(&paired.mem, &ds, &id_of, training_reps).await;
    let graph_training_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let trained_before = graph_stats(&paired).await;
    let trained_report = eval_public_retrieval(
        &paired,
        &ds,
        seeds,
        k,
        controlled_budget,
        &node_of,
        "graph-grounded",
        "spread after explicit relevant codename-to-attribute feedback",
        options.graph_diagnostics,
    )
    .await;
    let trained_after = graph_stats(&paired).await;

    // Keep the clean grounded measurement uncontaminated and in its historical
    // position. Only then add an equal-cardinality, equal-strength set of false
    // transitions and measure the resulting graph as a separate condition.
    let started = Instant::now();
    let false_transition_pairs =
        inject_noisy_grounded_transitions(&paired.mem, &ds, &id_of, training_reps).await;
    let graph_corruption_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let noisy_before = graph_stats(&paired).await;
    let noisy_report = eval_public_retrieval(
        &paired,
        &ds,
        seeds,
        k,
        controlled_budget,
        &node_of,
        "graph-noisy-grounded",
        "clean grounded graph plus equally reinforced false codename-to-wrong-entity Transition evidence (deterministic cyclic derangement; 1:1 false:clean pairs)",
        options.graph_diagnostics,
    )
    .await;
    let noisy_after = graph_stats(&paired).await;
    let clean_transition_pairs: usize = ds
        .by_entity
        .iter()
        .map(|entity| entity.attr_facts.len())
        .sum();
    ScaleReport {
        nodes: ds.facts.len(),
        id_seed,
        retrieval_questions: question_count,
        elapsed_ms: scale_started.elapsed().as_secs_f64() * 1_000.0,
        embedding_dimension: DEFAULT_DIM,
        raw_vector_bytes_per_index: ds.facts.len() * DEFAULT_DIM * std::mem::size_of::<f32>(),
        builds: BuildReport {
            bm25_ms,
            graph_ingest_ms,
            graph_training_ms,
            graph_corruption_ms,
        },
        topology: TopologyReport {
            cold_before_queries: cold_before,
            cold_after_queries: cold_after,
            trained_before_queries: trained_before,
            trained_after_queries: trained_after,
            noisy_before_queries: noisy_before,
            noisy_after_queries: noisy_after,
        },
        adversarial_graph_corruption: AdversarialGraphCorruptionReport {
            condition: "graph-noisy-grounded",
            applied_after_condition: "graph-grounded",
            wrong_entity_mapping: "cyclic successor over dataset entity order (i -> (i + 1) mod entity_count)",
            edge_kind: "transition",
            feedback_signal: "relevant_new",
            source_codename_priors: ds.by_entity.len(),
            clean_transition_pairs,
            false_transition_pairs,
            repetitions_per_pair: training_reps,
        },
        conditions: vec![
            bm25_report,
            dense_report,
            hybrid_report,
            current_report,
            default_budget_report,
            trained_report,
            noisy_report,
        ],
    }
}

#[expect(clippy::too_many_arguments)]
async fn offline_report(
    targets: &[usize],
    seed: u64,
    id_seeds: &[u64],
    k: usize,
    seeds: usize,
    training_reps: usize,
    options: OfflineRunOptions,
    metadata: ReportMetadata,
) -> OfflineReport {
    let embedder = make_embedder(false);
    let mut runs = Vec::with_capacity(targets.len() * id_seeds.len());
    for &id_seed in id_seeds {
        for &nodes in targets {
            eprintln!("· offline baseline suite: {nodes} nodes · id seed {id_seed}");
            runs.push(
                run_offline_scale(
                    nodes,
                    seed,
                    id_seed,
                    k,
                    seeds,
                    training_reps,
                    options,
                    embedder.clone(),
                )
                .await,
            );
        }
    }
    let mut notes = vec![
        "dataset seed, engine clock, corpus, and query order are fixed across conditions",
        "each scale run records its independent deterministic ID-order seed for paired condition comparisons",
        "cold and feedback-trained graph conditions are a paired before/after intervention over one ingested index; graph_ingest_ms is therefore paid once per run",
        "graph exploration is disabled in the shipped default and controlled release conditions",
        "dense is a read-only ANN baseline; graph conditions use the public retrieval path",
        "hybrid-flat isolates the public sparse+dense RRF seed stage with graph spread disabled",
        "graph-current is a controlled traversal ablation with relevance ratio, deduplication, and exploration disabled",
        "automatic embedding-similarity links are disabled by default because ANN already supplies that evidence; cold orphan nodes are valid",
        "graph-default-budget exercises the shipped Config::default budget, including its relevance ratio and disabled semantic deduplication",
        "graph max-fusion may promote a non-root direct-tail hit or admit a novel candidate; the total interventions are capped by the serialized graph_slot_cap",
        "graph training uses grounded synthetic codename-to-attribute trails and is transductive, not held-out generalization",
        "graph-noisy-grounded is measured only after graph-grounded, then adds one equally reinforced false forward Transition for every clean codename-to-attribute pair using a deterministic cyclic wrong-entity mapping",
        "the noisy condition is cumulative over the clean trained graph (1:1 false:clean transition pairs), not an independently rebuilt index; graph_corruption_ms excludes its subsequent queries",
        "deterministic eval IDs use a seeded ordinal permutation so tie order is reproducible without matching ingest order",
        "backend ordering among tied non-gold ANN results can still vary and may slightly change injected context",
        "Cozo conditions at most 32 neighbor vectors per frontier node, but incident_for materializes all incident edges and status targets first, so pathological-hub work is not degree-bounded",
        "Traversal max_nodes is enforced as a strict unique-node admission cap in both reference and Cozo backends",
        "context tokens are estimated as UTF-8-independent character count divided by four",
        "timings are wall-clock measurements from this machine, not portable constants",
        "conditions run sequentially in a fixed order without a warmup pass, so latency includes cache and order bias and is not a randomized performance comparison",
    ];
    if id_seeds.len() == 1 {
        notes.push(
            "this release artifact contains one ID-order seed; use a multi-seed companion before claiming robustness to tie order",
        );
    } else {
        notes.push(
            "this sensitivity report varies ID order only; it is not a multi-dataset-seed generalization study",
        );
    }
    if options.graph_diagnostics {
        notes.push(
            "opt-in graph diagnostics reconstruct internal ranks through public eval ports after timed queries; diagnostic work is excluded from each recorded query latency but can warm later conditions",
        );
    }
    if options.policy.any() {
        notes.push(
            "validated CLI policy overrides are applied identically to cold and trained engine configs and to controlled and shipped-default graph budgets",
        );
    }
    OfflineReport {
        schema_version: OFFLINE_REPORT_SCHEMA_VERSION,
        suite: OFFLINE_REPORT_SUITE,
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        backend: eval_backend_name(),
        embedder: eval_embedder_name(),
        embedder_fingerprint: eval_embedder_fingerprint(),
        source_revision: metadata.source_revision,
        command: metadata.command,
        machine_label: metadata.machine_label,
        variant: metadata.variant,
        seed,
        id_seeds: id_seeds.to_vec(),
        k,
        dense_seeds: seeds,
        training_repetitions: training_reps,
        notes,
        runs,
    }
}

fn print_offline_report(report: &OfflineReport) {
    println!(
        "mneme-eval · offline baselines · profile={} · backend={} · embedder={} · seed={} · k={}\n",
        report.build_profile, report.backend, report.embedder, report.seed, report.k
    );
    for run in &report.runs {
        println!(
            "{} nodes · id seed {} · {} retrieval questions",
            run.nodes, run.id_seed, run.retrieval_questions
        );
        println!(
            "condition              1hop hit   1hop mrr   2hop hit   2hop mrr   p95 ms   ctx tok"
        );
        println!(
            "--------------------------------------------------------------------------------"
        );
        for c in &run.conditions {
            println!(
                "{:<22} {:>8.3} {:>10.3} {:>10.3} {:>10.3} {:>9.2} {:>9.0}",
                c.name,
                c.retrieval.single_hop.hit_at_k,
                c.retrieval.single_hop.mrr,
                c.retrieval.multi_hop.hit_at_k,
                c.retrieval.multi_hop.mrr,
                c.latency.p95_ms,
                c.context.mean_estimated_tokens_at_k,
            );
        }
        let t = &run.topology;
        println!(
            "build ms: bm25={:.1}, graph ingest={:.1}, clean training={:.1}, false-evidence injection={:.1}",
            run.builds.bm25_ms,
            run.builds.graph_ingest_ms,
            run.builds.graph_training_ms,
            run.builds.graph_corruption_ms,
        );
        println!(
            "total={:.1} ms; raw vectors/index={:.2} MiB ({} x f32)",
            run.elapsed_ms,
            run.raw_vector_bytes_per_index as f64 / (1024.0 * 1024.0),
            run.embedding_dimension,
        );
        println!(
            "stored edges: cold {}→{} (density {:.6}→{:.6}); trained {}→{} (bridges {}→{}, density {:.6}→{:.6})\n",
            t.cold_before_queries.stored_edges,
            t.cold_after_queries.stored_edges,
            t.cold_before_queries.stored_directed_density,
            t.cold_after_queries.stored_directed_density,
            t.trained_before_queries.stored_edges,
            t.trained_after_queries.stored_edges,
            t.trained_before_queries.bridge_edges,
            t.trained_after_queries.bridge_edges,
            t.trained_before_queries.stored_directed_density,
            t.trained_after_queries.stored_directed_density,
        );
        println!(
            "noisy graph: {}→{} stored edges, {}→{} transitions, density {:.6}→{:.6}; corruption={} false pairs x {} reps ({})\n",
            t.noisy_before_queries.stored_edges,
            t.noisy_after_queries.stored_edges,
            t.noisy_before_queries.transition_edges,
            t.noisy_after_queries.transition_edges,
            t.noisy_before_queries.stored_directed_density,
            t.noisy_after_queries.stored_directed_density,
            run.adversarial_graph_corruption.false_transition_pairs,
            run.adversarial_graph_corruption.repetitions_per_pair,
            run.adversarial_graph_corruption.wrong_entity_mapping,
        );
    }
    println!("Use --json for the complete machine-readable metrics.");
}

static REPORT_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Durably replace a report without exposing a partially-written JSON file.
/// The temporary lives beside the destination so the final rename stays on one
/// filesystem (and is therefore atomic on the supported Unix release hosts).
fn write_report_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("report path has no file name: {}", path.display()),
        )
    })?;

    let mut opened = None;
    for _ in 0..128 {
        let sequence = REPORT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{}.tmp-{}-{sequence}",
            file_name.to_string_lossy(),
            std::process::id(),
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => {
                opened = Some((temp_path, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let Some((temp_path, mut file)) = opened else {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique adjacent report temporary",
        ));
    };

    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    drop(file);
    if let Err(error) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

// ---- Layer 2: backend-agnostic answerer + answer accuracy ------------------

/// A minimal OpenAI-compatible chat client. One `base_url` + `model` + optional
/// bearer key covers Ollama (`localhost:11434/v1`), Anthropic's compat endpoint,
/// OpenAI, OpenRouter, vLLM, … so the answerer is backend-agnostic and weak/local
/// models drop in unchanged — they only *answer* (no tool-calling needed).
struct Llm {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    /// Cost accounting, cumulative across every call through this client. Shared
    /// (`Arc`) and atomic so concurrent answers (the buffered synthetic sweep) all
    /// tally into one place; snapshot with [`Llm::usage`] to get per-condition
    /// deltas. The agent and the judge are separate `Llm`s, so their costs don't
    /// mix — the whole point is to price the *agent's* retrieval, not the grader.
    calls: Arc<AtomicU64>,
    prompt_tokens: Arc<AtomicU64>,
    completion_tokens: Arc<AtomicU64>,
}

/// A cumulative `(calls, prompt_tokens, completion_tokens)` reading — subtract two
/// to get the cost of the work in between.
#[derive(Clone, Copy, Default)]
struct Usage {
    calls: u64,
    prompt: u64,
    completion: u64,
}

impl std::ops::Sub for Usage {
    type Output = Usage;
    fn sub(self, rhs: Usage) -> Usage {
        Usage {
            calls: self.calls - rhs.calls,
            prompt: self.prompt - rhs.prompt,
            completion: self.completion - rhs.completion,
        }
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, rhs: Usage) {
        self.calls += rhs.calls;
        self.prompt += rhs.prompt;
        self.completion += rhs.completion;
    }
}

impl Llm {
    fn new(base_url: String, model: String, api_key: Option<String>) -> Self {
        Llm {
            // A per-request timeout so one hung/stalled completion can't block the
            // whole eval forever (a slow local model returns an error instead, which
            // surfaces as an empty answer — scored, not hung).
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(180))
                .build()
                .unwrap_or_default(),
            base_url,
            model,
            api_key,
            calls: Arc::new(AtomicU64::new(0)),
            prompt_tokens: Arc::new(AtomicU64::new(0)),
            completion_tokens: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A cumulative snapshot of everything spent through this client so far.
    fn usage(&self) -> Usage {
        Usage {
            calls: self.calls.load(Ordering::Relaxed),
            prompt: self.prompt_tokens.load(Ordering::Relaxed),
            completion: self.completion_tokens.load(Ordering::Relaxed),
        }
    }

    async fn complete(&self, system: &str, user: &str) -> Result<String, String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        let mut req = self.http.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let v: serde_json::Value = req
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        // Count the call and its tokens before extracting content, so a
        // reply-that-arrived still counts even if its shape is unexpected. Backends
        // that omit `usage` contribute a call with zero tokens (calls are the floor
        // signal; tokens are best-effort).
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(u) = v.get("usage") {
            if let Some(pt) = u.get("prompt_tokens").and_then(serde_json::Value::as_u64) {
                self.prompt_tokens.fetch_add(pt, Ordering::Relaxed);
            }
            if let Some(ct) = u
                .get("completion_tokens")
                .and_then(serde_json::Value::as_u64)
            {
                self.completion_tokens.fetch_add(ct, Ordering::Relaxed);
            }
        }
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.trim().to_string())
            .ok_or_else(|| format!("unexpected LLM response: {v}"))
    }

    /// Answer a question from retrieved context only (or admit it can't).
    async fn answer(&self, context: &[&str], question: &str) -> String {
        let ctx = if context.is_empty() {
            "(no facts retrieved)".to_string()
        } else {
            context
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let system = "Answer the question using ONLY the facts in the context. Reply with \
            just the answer, in as few words as possible. If the context does not contain the \
            answer, reply exactly: I don't know.";
        let user = format!("Context:\n{ctx}\n\nQuestion: {question}");
        self.complete(system, &user).await.unwrap_or_default()
    }

    /// One turn of an *agentic* loop: given the facts gathered so far, the model emits
    /// either `SEARCH: <query>` (look up more) or `ANSWER: <value>`.
    async fn agent_turn(&self, question: &str, facts: &[&str]) -> String {
        let gathered = if facts.is_empty() {
            "(none yet)".to_string()
        } else {
            facts
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let system = "You answer a question by searching a memory of facts over multiple \
            turns. Reply with EXACTLY ONE line, either:\n\
            SEARCH: <a short query for more facts>\n\
            ANSWER: <the final answer in as few words as possible, or: I don't know>\n\
            You may search several times — look one thing up, then use it to look up the \
            next. Only ANSWER once the gathered facts are sufficient. Never invent facts.";
        let user = format!("Question: {question}\n\nFacts gathered so far:\n{gathered}");
        self.complete(system, &user).await.unwrap_or_default()
    }
}

/// What the model chose to do this agentic turn.
enum Action {
    Search(String),
    Answer(String),
}

/// Parse an agentic turn: the first `SEARCH:`/`ANSWER:` line wins; a reply with neither
/// is treated as a final answer (a model that won't follow the protocol just answers
/// from what it has — itself part of what's measured).
fn parse_action(resp: &str) -> Action {
    for line in resp.lines() {
        let l = line.trim();
        if let Some(rest) = l
            .strip_prefix("ANSWER:")
            .or_else(|| l.strip_prefix("Answer:"))
            .or_else(|| l.strip_prefix("answer:"))
        {
            return Action::Answer(rest.trim().to_string());
        }
        if let Some(rest) = l
            .strip_prefix("SEARCH:")
            .or_else(|| l.strip_prefix("Search:"))
            .or_else(|| l.strip_prefix("search:"))
        {
            return Action::Search(rest.trim().to_string());
        }
    }
    Action::Answer(resp.trim().to_string())
}

#[derive(Default, Clone, Copy)]
struct Acc {
    n: usize,
    in_ctx: usize,
    correct: usize,
}
impl Acc {
    fn add(&mut self, in_ctx: bool, correct: bool) {
        self.n += 1;
        self.in_ctx += in_ctx as usize;
        self.correct += correct as usize;
    }
    fn ctx_cell(&self) -> String {
        self.rate(self.in_ctx)
    }
    fn ans_cell(&self) -> String {
        self.rate(self.correct)
    }
    fn rate(&self, x: usize) -> String {
        if self.n == 0 {
            format!("{:>6}", "-")
        } else {
            format!("{:>6.3}", x as f64 / self.n as f64)
        }
    }
}

/// Answer one question — agentic loop or single-shot — returning (gold-reached, answer).
#[expect(clippy::too_many_arguments)]
async fn answer_one(
    mem: &Memory,
    q: &Question,
    seeds: usize,
    budget: Budget,
    k: usize,
    node_of: &HashMap<NodeId, u32>,
    id_text: &HashMap<u32, &str>,
    llm: &Llm,
    agentic: bool,
    max_steps: usize,
) -> (bool, String) {
    if agentic {
        agent_loop(
            mem, q, seeds, budget, k, node_of, id_text, llm, max_steps, false,
        )
        .await
    } else {
        let topk: Vec<u32> = retrieve_ids(mem, &q.query, seeds, budget, node_of)
            .await
            .into_iter()
            .take(k)
            .collect();
        let in_ctx = q.gold.iter().any(|g| topk.contains(g));
        let ctx: Vec<&str> = topk
            .iter()
            .filter_map(|id| id_text.get(id).copied())
            .collect();
        (in_ctx, llm.answer(&ctx, &q.query).await)
    }
}

/// Layer 2 over one condition: the first `limit` questions of each category, evaluated
/// with up to `concurrency` answers in flight (the LLM calls are the bottleneck, so this
/// is the big speedup for the agentic sweep). Exact-match judging on the unique gold
/// values (abstention = the model declined). In `agentic` mode the model drives multiple
/// searches itself (see [`agent_loop`]) — where a weak agent finally underperforms.
#[expect(clippy::too_many_arguments)]
async fn eval_answer(
    mem: &Memory,
    ds: &Dataset,
    seeds: usize,
    budget: Budget,
    k: usize,
    node_of: &HashMap<NodeId, u32>,
    id_text: &HashMap<u32, &str>,
    llm: &Llm,
    limit: usize,
    agentic: bool,
    max_steps: usize,
    concurrency: usize,
) -> [Acc; NCATS] {
    // The first `limit` questions of each category.
    let mut counts = [0usize; NCATS];
    let mut selected: Vec<&Question> = Vec::new();
    for q in &ds.questions {
        let ci = q.cat.idx();
        if counts[ci] < limit {
            counts[ci] += 1;
            selected.push(q);
        }
    }
    // Evaluate with bounded concurrency. The per-question futures borrow shared state
    // (mem/llm/…) immutably and aren't spawned, so no 'static/Send requirement.
    let tasks = selected.into_iter().map(|q| async move {
        let (in_ctx, answer) = answer_one(
            mem, q, seeds, budget, k, node_of, id_text, llm, agentic, max_steps,
        )
        .await;
        let correct = if matches!(q.cat, Cat::Abstain) {
            is_abstention(&answer)
        } else {
            answer.to_lowercase().contains(&q.answer.to_lowercase())
        };
        (q.cat.idx(), in_ctx, correct)
    });
    let outcomes: Vec<(usize, bool, bool)> = stream::iter(tasks)
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;

    let mut accs = [Acc::default(); NCATS];
    for (ci, in_ctx, correct) in outcomes {
        accs[ci].add(in_ctx, correct);
    }
    accs
}

/// Run the agentic retrieval loop for one question: the model issues `SEARCH`es (each
/// run through the same mneme retrieval), accumulating facts, until it `ANSWER`s or the
/// step budget runs out. `in_ctx` is whether the gold reached *any* search — the
/// agentic answerability ceiling, which can exceed single-shot's.
#[expect(clippy::too_many_arguments)]
async fn agent_loop(
    mem: &Memory,
    q: &Question,
    seeds: usize,
    budget: Budget,
    k: usize,
    node_of: &HashMap<NodeId, u32>,
    id_text: &HashMap<u32, &str>,
    llm: &Llm,
    max_steps: usize,
    trace: bool,
) -> (bool, String) {
    let mut gathered: Vec<u32> = Vec::new();
    let mut in_ctx = false;
    for _ in 0..max_steps {
        let facts: Vec<&str> = gathered
            .iter()
            .filter_map(|id| id_text.get(id).copied())
            .collect();
        match parse_action(&llm.agent_turn(&q.query, &facts).await) {
            Action::Answer(a) if !a.trim().is_empty() => {
                if trace {
                    println!("    ↳ ANSWER: {a:?}");
                }
                return (in_ctx, a);
            }
            // Empty / malformed turn (e.g. a model that returned no content): stop
            // looping and fall back below rather than scoring an empty answer.
            Action::Answer(_) => break,
            Action::Search(query) => {
                let topk: Vec<u32> = retrieve_ids(mem, &query, seeds, budget, node_of)
                    .await
                    .into_iter()
                    .take(k)
                    .collect();
                let hit = q.gold.iter().any(|g| topk.contains(g));
                if hit {
                    in_ctx = true;
                }
                if trace {
                    let preview: Vec<&str> = topk
                        .iter()
                        .take(2)
                        .filter_map(|id| id_text.get(id).copied())
                        .collect();
                    println!(
                        "    ↳ SEARCH {query:?} → {} facts [{}]{}",
                        topk.len(),
                        preview.join(" | "),
                        if hit { "  GOLD✓" } else { "" }
                    );
                }
                for id in topk {
                    if !gathered.contains(&id) {
                        gathered.push(id);
                    }
                }
            }
        }
    }
    // Fallback (empty answer or step budget spent): make sure we have *some* context,
    // then answer from it — so a model that won't drive the loop still gets a
    // single-shot floor instead of an empty (wrong) answer.
    if gathered.is_empty() {
        let topk: Vec<u32> = retrieve_ids(mem, &q.query, seeds, budget, node_of)
            .await
            .into_iter()
            .take(k)
            .collect();
        if q.gold.iter().any(|g| topk.contains(g)) {
            in_ctx = true;
        }
        gathered = topk;
    }
    let facts: Vec<&str> = gathered
        .iter()
        .filter_map(|id| id_text.get(id).copied())
        .collect();
    let a = llm.answer(&facts, &q.query).await;
    if trace {
        println!("    ↳ (fallback) ANSWER: {a:?}");
    }
    (in_ctx, a)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return;
    }
    if args.iter().any(|arg| arg.starts_with("--tagged-")) {
        eprintln!(
            "mneme-eval: historical tagged study retired with the single-graph lifecycle; see the archived source receipt"
        );
        std::process::exit(2);
    }
    let offline_policy = match OfflinePolicyOverrides::parse(&args) {
        Ok(policy) => policy,
        Err(message) => {
            eprintln!("mneme-eval: {message}");
            std::process::exit(2);
        }
    };
    let lexical = args.iter().any(|a| a == "--lexical");
    let k = flag(&args, "--k").unwrap_or(10);
    let seeds = flag(&args, "--seeds").unwrap_or(10);
    let seed = flag(&args, "--seed").unwrap_or(42) as u64;
    let training_reps = flag(&args, "--training-reps")
        .or_else(|| flag(&args, "--study-reps"))
        .unwrap_or(10);
    let agentic = args.iter().any(|a| a == "--agentic");
    // Agentic does up to max_steps LLM calls per question, so default to far fewer.
    let limit = flag(&args, "--limit").unwrap_or(if agentic { 5 } else { 20 });
    let max_steps = flag(&args, "--max-steps").unwrap_or(4);
    let concurrency = flag(&args, "--concurrency").unwrap_or(8);
    let trace = args.iter().any(|a| a == "--trace");
    let trace_n = flag(&args, "--trace").unwrap_or(2);
    let agent = agent_from_args(&args);
    let offline_default_mode = agent.is_none()
        && !args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "--legacy-layer1" | "--longmemeval" | "--multihop"
            )
        });
    if offline_policy.any() && !offline_default_mode {
        eprintln!(
            "mneme-eval: graph policy overrides are supported only by the default offline suite"
        );
        std::process::exit(2);
    }

    // LongMemEval mode: a real long-conversation memory benchmark, judged by an LLM.
    if let Some(path) = flag_str(&args, "--longmemeval") {
        let agent = agent.expect("--longmemeval needs --agent-* or --ollama-model");
        let judge = judge_from_args(&args, &agent);
        let offset = flag(&args, "--lme-offset").unwrap_or(0);
        run_longmemeval(
            &path, limit, offset, k, seeds, lexical, agentic, max_steps, &agent, &judge,
        )
        .await;
        return;
    }

    // 2WikiMultiHopQA: multi-hop QA over a pooled corpus, with a train/test split so
    // grounded graph training on train-split gold can be measured on
    // held-out questions. Needs an agent (and ideally a strong --judge-model).
    if let Some(path) = flag_str(&args, "--multihop") {
        let agent = agent.expect("--multihop needs --agent-* or --ollama-model");
        let judge = judge_from_args(&args, &agent);
        let offset = flag(&args, "--mh-offset").unwrap_or(0);
        let train_frac = flag_str(&args, "--train-frac")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.5);
        run_multihop(
            &path, limit, offset, k, seeds, lexical, train_frac, agentic, max_steps, &agent, &judge,
        )
        .await;
        return;
    }

    // The no-LLM default is the reproducible baseline suite. Keep the original
    // five-condition synthetic ladder available for historical comparisons.
    if agent.is_none() && !args.iter().any(|a| a == "--legacy-layer1") {
        let base_nodes = gen_dataset(seed).facts.len();
        let targets = match parse_node_targets(&args, base_nodes) {
            Ok(targets) => targets,
            Err(message) => {
                eprintln!("mneme-eval: {message}");
                std::process::exit(2);
            }
        };
        let id_seeds = match parse_id_seeds(&args, seed) {
            Ok(id_seeds) => id_seeds,
            Err(message) => {
                eprintln!("mneme-eval: {message}");
                std::process::exit(2);
            }
        };
        let metadata = ReportMetadata {
            source_revision: flag_str(&args, "--source-revision")
                .unwrap_or_else(|| "unrecorded".to_string()),
            command: args.clone(),
            machine_label: flag_str(&args, "--machine-label")
                .unwrap_or_else(|| "unrecorded".to_string()),
            variant: flag_str(&args, "--variant").unwrap_or_else(|| "development".to_string()),
        };
        let options = OfflineRunOptions {
            graph_diagnostics: args.iter().any(|arg| arg == "--graph-diagnostics"),
            policy: offline_policy,
        };
        let report = offline_report(
            &targets,
            seed,
            &id_seeds,
            k,
            seeds,
            training_reps,
            options,
            metadata,
        )
        .await;
        let report_json = serde_json::to_string_pretty(&report).expect("serialize offline report");
        if let Some(path) = flag_str(&args, "--output") {
            let path = Path::new(&path);
            write_report_atomic(path, report_json.as_bytes()).expect("write offline report");
            eprintln!("· wrote report to {}", path.display());
        }
        if args.iter().any(|a| a == "--json") {
            println!("{report_json}");
        } else {
            print_offline_report(&report);
        }
        return;
    }

    let ds = gen_dataset(seed);
    let id_text: HashMap<u32, &str> = ds.facts.iter().map(|f| (f.id, f.text.as_str())).collect();
    let kind = if lexical {
        "lexical (hashing)"
    } else {
        "semantic (bge-base)"
    };
    let conditions = [
        (Kind::Flat, "flat (hybrid)"),
        (Kind::Mneme, "mneme (spread)"),
        (Kind::Study, "mneme + self-train"),
        (Kind::Train, "mneme + feedback"),
        (Kind::Links, "mneme + links"),
    ];

    if let Some(llm) = &agent {
        let mode = if agentic {
            "agentic"
        } else {
            "answer accuracy"
        };
        println!(
            "mneme-eval · Layer 2 ({mode}) · {} facts · embedder: {kind} · agent: {} · {limit} q/cat\n",
            ds.facts.len(),
            llm.model,
        );

        // Trace: show exactly what the agent searches and answers on a few hard
        // questions — so "the numbers are flat" becomes "here's what the model did".
        if agentic && trace {
            println!("── TRACE · mneme+self-training · first {trace_n} of 2hop / synth / confl ──");
            let (mem, node_of) = build_condition(Kind::Study, lexical, &ds, training_reps).await;
            for cat in [Cat::Multi, Cat::Synth, Cat::Conflict] {
                let mut shown = 0;
                for q in &ds.questions {
                    if q.cat != cat || shown >= trace_n {
                        continue;
                    }
                    shown += 1;
                    println!("\n[{}] {}", CAT_NAMES[cat.idx()], q.query);
                    let (in_ctx, ans) = agent_loop(
                        &mem,
                        q,
                        seeds,
                        budget_for(Kind::Study, k),
                        k,
                        &node_of,
                        &id_text,
                        llm,
                        max_steps,
                        true,
                    )
                    .await;
                    let ok = ans.to_lowercase().contains(&q.answer.to_lowercase());
                    println!(
                        "  gold: {:?}  ctx-reached: {in_ctx}  correct: {}",
                        q.answer,
                        if ok { "✓" } else { "✗" }
                    );
                }
            }
            println!("\n────────────────────────────────────────────────\n");
            // --trace is a diagnostic; skip the (expensive) full matrices.
            return;
        }

        let mut results: Vec<(&str, [Acc; NCATS])> = Vec::new();
        for (kc, name) in conditions {
            eprintln!(
                "· evaluating {name} ({limit} q/cat{}, {concurrency} in flight)…",
                if agentic {
                    format!(", ≤{max_steps} steps each")
                } else {
                    String::new()
                }
            );
            let (mem, node_of) = build_condition(kc, lexical, &ds, training_reps).await;
            let accs = eval_answer(
                &mem,
                &ds,
                seeds,
                budget_for(kc, k),
                k,
                &node_of,
                &id_text,
                llm,
                limit,
                agentic,
                max_steps,
                concurrency,
            )
            .await;
            results.push((name, accs));
        }
        let header = || {
            print!("{:<16}", "condition");
            for c in CAT_NAMES {
                print!("  {c:>6}");
            }
            println!();
        };
        println!("ANSWER ACCURACY (ans) — agent answered correctly from the retrieved context");
        header();
        for (name, accs) in &results {
            print!("{name:<16}");
            for a in accs {
                print!("  {}", a.ans_cell());
            }
            println!();
        }
        println!("\nRETRIEVAL CEILING (ctx@k) — gold fact reached top-k (answerable in principle)");
        header();
        for (name, accs) in &results {
            print!("{name:<16}");
            for a in accs {
                print!("  {}", a.ctx_cell());
            }
            println!();
        }
        println!(
            "\n1hop/2hop: factoid recall (answering trivial → ans≈ctx).  synth: compare two\n\
             agents' clearance, answer the winner's colour (multi-fact reasoning).  abst: the\n\
             attribute doesn't exist — correct = abstained (ctx is 0 by design).  confl: a\n\
             fact was superseded — correct = the *current* value. ans<ctx ⇒ the model is the\n\
             bottleneck (weak vs strong diverge here); ans≈ctx ⇒ retrieval-bound."
        );
    } else {
        println!(
            "mneme-eval · Layer 1 (recall) · {} facts, {} questions · embedder: {kind} · k={k}, seeds={seeds}\n",
            ds.facts.len(),
            ds.questions.len()
        );
        println!("                       single-hop                multi-hop");
        println!("condition          hit@k   mrr  rec@k        hit@k   mrr  rec@k");
        println!("---------------------------------------------------------------");
        for (kc, name) in conditions {
            let (mem, node_of) = build_condition(kc, lexical, &ds, training_reps).await;
            let (s, m) = eval(&mem, &ds, seeds, budget_for(kc, k), k, &node_of).await;
            println!("{name:<16} {}     {}", s.cell(), m.cell());
        }
        println!(
            "\nsingle-hop names the entity (ANN nails it; flat≈mneme). multi-hop hides it\n\
             behind a codename — the answer is a graph hop away. `self-train` explicitly\n\
             enables co-retrieval edge creation; `feedback` uses grounded synthetic trails;\n\
             `links` is the hand-built upper bound."
        );
    }
}

// ---- LongMemEval (real benchmark) -----------------------------------------

#[derive(Deserialize)]
struct LmeTurn {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
struct LmeItem {
    question_id: String,
    #[serde(default)]
    question_type: String,
    question: String,
    #[serde(default)]
    answer: String,
    #[serde(default)]
    haystack_dates: Vec<String>,
    #[serde(default)]
    haystack_session_ids: Vec<String>,
    haystack_sessions: Vec<Vec<LmeTurn>>,
    #[serde(default)]
    answer_session_ids: Vec<String>,
}

/// Grade a free-form answer with an LLM judge (yes/no). Abstention questions: correct =
/// the model declined rather than fabricating a value.
async fn judge_answer(
    judge: &Llm,
    question: &str,
    reference: &str,
    response: &str,
    abstention: bool,
) -> bool {
    let system = if abstention {
        "You grade a memory assistant. The question asks about information that is NOT present in the conversation history, so the only correct behaviour is to say it doesn't know or can't find it. Reply with ONLY 'yes' if the response declines / says the information isn't available, or 'no' if it states a specific (fabricated) answer."
    } else {
        "You grade a memory assistant's answer against a reference answer. Reply with ONLY 'yes' if the response is correct — it conveys the same key information as the reference, even if worded differently — or 'no' otherwise."
    };
    let user = format!(
        "Question: {question}\nReference answer: {reference}\nAssistant response: {response}\n\nCorrect? Reply yes or no."
    );
    judge
        .complete(system, &user)
        .await
        .unwrap_or_default()
        .trim_start()
        .to_lowercase()
        .starts_with("yes")
}

/// The judge LLM: a separate `--judge-*` endpoint, or the agent itself if unset (a weak
/// self-judge — pass a strong `--judge-model` for trustworthy numbers).
fn judge_from_args(args: &[String], agent: &Llm) -> Llm {
    match flag_str(args, "--judge-base-url") {
        Some(base_url) => {
            let model = flag_str(args, "--judge-model").unwrap_or_else(|| {
                eprintln!("--judge-model is required with --judge-base-url");
                std::process::exit(2);
            });
            let api_key = flag_str(args, "--judge-key-env").and_then(|v| std::env::var(v).ok());
            Llm::new(base_url, model, api_key)
        }
        None => Llm::new(
            agent.base_url.clone(),
            agent.model.clone(),
            agent.api_key.clone(),
        ),
    }
}

fn merge(a: Acc, b: Acc) -> Acc {
    Acc {
        n: a.n + b.n,
        in_ctx: a.in_ctx + b.in_ctx,
        correct: a.correct + b.correct,
    }
}

/// Fold a retrieval's top-`k` hits into the running set: mark evidence, dedupe by id,
/// keep each hit's summary as a fact. Shared by [`lme_agent_loop`]'s search + floor.
fn absorb_hits(
    hits: Vec<mneme_engine::Retrieved>,
    k: usize,
    evidence: &HashSet<NodeId>,
    gathered: &mut Vec<(NodeId, String)>,
    ev_hit: &mut bool,
) {
    for r in hits.into_iter().take(k) {
        if evidence.contains(&r.node.id()) {
            *ev_hit = true;
        }
        if !gathered.iter().any(|(id, _)| *id == r.node.id()) {
            gathered.push((r.node.id(), r.node.summary().to_string()));
        }
    }
}

/// Agentic answering over a LongMemEval memory: the model issues `SEARCH`es (each a
/// real mneme retrieval), accumulating turn-summaries, until it `ANSWER`s or the step
/// budget runs out — the multi-hop path a real assistant takes. Reuses the generic
/// [`Llm::agent_turn`]/[`parse_action`]; returns `(evidence_reached, answer)` where
/// `evidence_reached` is whether any search surfaced an answer session (the ceiling).
/// Every LLM call is tallied on `agent`, so the caller can price the loop.
#[expect(clippy::too_many_arguments)]
async fn lme_agent_loop(
    mem: &Memory,
    question: &str,
    seeds: usize,
    budget: Budget,
    k: usize,
    evidence: &HashSet<NodeId>,
    agent: &Llm,
    max_steps: usize,
) -> (bool, String) {
    let mut gathered: Vec<(NodeId, String)> = Vec::new();
    let mut ev_hit = false;
    for _ in 0..max_steps {
        let facts: Vec<&str> = gathered.iter().map(|(_, s)| s.as_str()).collect();
        match parse_action(&agent.agent_turn(question, &facts).await) {
            Action::Answer(a) if !a.trim().is_empty() => return (ev_hit, a),
            Action::Answer(_) => break, // malformed/empty turn — fall through to the floor
            Action::Search(query) => {
                let hits = mem
                    .retrieve_seeded(&query, seeds, budget, StatusFilter::ACTIVE, &[])
                    .await
                    .unwrap();
                absorb_hits(hits, k, evidence, &mut gathered, &mut ev_hit);
            }
        }
    }
    // Floor: if the model never searched, do one retrieval on the question itself so
    // a model that won't drive the loop still answers from context, not from nothing.
    if gathered.is_empty() {
        let hits = mem
            .retrieve_seeded(question, seeds, budget, StatusFilter::ACTIVE, &[])
            .await
            .unwrap();
        absorb_hits(hits, k, evidence, &mut gathered, &mut ev_hit);
    }
    let facts: Vec<&str> = gathered.iter().map(|(_, s)| s.as_str()).collect();
    (ev_hit, agent.answer(&facts, question).await)
}

/// Ingest a LongMemEval haystack into `mem` — one node per non-empty turn, prefixed
/// with its date — and return the set of nodes drawn from the gold answer session(s)
/// (the evidence, for the retrieval ceiling). Fresh per condition, so ids are local.
async fn ingest_haystack(mem: &Memory, item: &LmeItem) -> HashSet<NodeId> {
    let answer_sessions: HashSet<&str> =
        item.answer_session_ids.iter().map(String::as_str).collect();
    let mut evidence = HashSet::new();
    for (si, session) in item.haystack_sessions.iter().enumerate() {
        let date = item
            .haystack_dates
            .get(si)
            .map(String::as_str)
            .unwrap_or("");
        let is_ev = item
            .haystack_session_ids
            .get(si)
            .is_some_and(|sid| answer_sessions.contains(sid.as_str()));
        for turn in session {
            if turn.content.trim().is_empty() {
                continue;
            }
            let text = format!("[{date}] {}: {}", turn.role, turn.content);
            let id = mem
                .ingest(Ingest::new(&text, b"", &[], Provenance::derived_empty()))
                .await
                .unwrap();
            if is_ev {
                evidence.insert(id);
            }
        }
    }
    evidence
}

/// Non-empty turn count of a haystack — the progress line, without ingesting.
fn haystack_turns(item: &LmeItem) -> usize {
    item.haystack_sessions
        .iter()
        .flatten()
        .filter(|t| !t.content.trim().is_empty())
        .count()
}

/// Explicit self-training ablation over a freshly-ingested haystack: browse each
/// session once under a config that opts into co-retrieval edge creation. Reads no
/// gold and makes no LLM call, but exposure is not grounded relevance; the condition
/// is labeled accordingly instead of being presented as the primary training path.
async fn lme_train(mem: &Memory, item: &LmeItem, seeds: usize) {
    for session in &item.haystack_sessions {
        if let Some(turn) = session
            .iter()
            .find(|t| t.role == "user" && !t.content.trim().is_empty())
        {
            let probe: String = turn.content.chars().take(200).collect();
            let _ = mem
                .retrieve_seeded(&probe, seeds, budget_mneme(), StatusFilter::ACTIVE, &[])
                .await;
        }
    }
}

/// Run LongMemEval: for each question, ingest its conversation haystack (one node per
/// turn) into a fresh memory, then answer + LLM-judge over flat vs mneme retrieval.
/// Scored by question type; `ctx` = an evidence session reached top-k (the ceiling).
/// With `agentic`, the model drives multi-step search (see [`lme_agent_loop`]); either
/// way every agent call is priced (calls + tokens per question, per condition).
#[expect(clippy::too_many_arguments)]
async fn run_longmemeval(
    path: &str,
    limit: usize,
    offset: usize,
    k: usize,
    seeds: usize,
    lexical: bool,
    agentic: bool,
    max_steps: usize,
    agent: &Llm,
    judge: &Llm,
) {
    let file = std::fs::File::open(path).unwrap_or_else(|e| {
        eprintln!("cannot open {path}: {e}");
        std::process::exit(1);
    });
    let all: Vec<LmeItem> =
        serde_json::from_reader(std::io::BufReader::new(file)).unwrap_or_else(|e| {
            eprintln!("cannot parse LongMemEval JSON ({path}): {e}");
            std::process::exit(1);
        });
    let items: Vec<&LmeItem> = all.iter().skip(offset).take(limit).collect();
    println!(
        "mneme-eval · LongMemEval · {} questions (offset {offset}) · embedder: {} · agent: {} · judge: {}",
        items.len(),
        if lexical { "lexical" } else { "bge-base" },
        agent.model,
        judge.model,
    );
    if judge.model == agent.model && judge.base_url == agent.base_url {
        eprintln!("  (note: no --judge-model set — the agent is grading itself; weak)");
    }

    if agentic {
        println!("  mode: agentic (model drives up to {max_steps} searches/question)");
    }
    let embedder = make_embedder(lexical);
    // `self-training` is an explicit ablation here: LongMemEval does not supply a
    // held-out feedback trace suitable for the primary grounded-training condition.
    let conditions = [Kind::Flat, Kind::Mneme, Kind::Train];
    let cond_names = ["flat", "mneme", "mneme+self-training"];
    println!("  ablation: self-training browses each session once (no gold, no LLM)");
    let mut by_type: Vec<HashMap<String, Acc>> = vec![HashMap::new(); conditions.len()];
    // Per-condition cost: what the agent spent answering under this retrieval regime.
    let mut cost: Vec<Usage> = vec![Usage::default(); conditions.len()];
    let mut answered = 0usize;

    for (qi, item) in items.iter().enumerate() {
        let abstention = item.question_id.ends_with("_abs");
        eprintln!(
            "· [{}/{}] {} · {} · {} turns",
            qi + 1,
            items.len(),
            item.question_id,
            item.question_type,
            haystack_turns(item),
        );

        for (ci, kind) in conditions.iter().enumerate() {
            // A fresh store per condition: retrieval (and training) mutate the graph,
            // so sharing one memory would leak one condition's changes into the next.
            let config = if matches!(kind, Kind::Train) {
                Config {
                    coretrieval_link_cap: 6,
                    ..Config::default()
                }
            } else {
                Config::default()
            };
            let mem = eval_memory_with_config(embedder.clone(), config).mem;
            let evidence = ingest_haystack(&mem, item).await;
            if matches!(kind, Kind::Train) {
                lme_train(&mem, item, seeds).await;
            }
            let budget = budget_for(*kind, k);
            let before = agent.usage();
            let (ev_hit, answer) = if agentic {
                lme_agent_loop(
                    &mem,
                    &item.question,
                    seeds,
                    budget,
                    k,
                    &evidence,
                    agent,
                    max_steps,
                )
                .await
            } else {
                let hits = mem
                    .retrieve_seeded(&item.question, seeds, budget, StatusFilter::ACTIVE, &[])
                    .await
                    .unwrap();
                let topk: Vec<_> = hits.into_iter().take(k).collect();
                let ev_hit = topk.iter().any(|r| evidence.contains(&r.node.id()));
                let ctx: Vec<&str> = topk.iter().map(|r| r.node.summary()).collect();
                (ev_hit, agent.answer(&ctx, &item.question).await)
            };
            cost[ci] += agent.usage() - before;
            let correct =
                judge_answer(judge, &item.question, &item.answer, &answer, abstention).await;
            by_type[ci]
                .entry(item.question_type.clone())
                .or_default()
                .add(ev_hit, correct);
        }
        answered += 1;
    }

    // Table: two sub-columns (ctx, ans) per condition, generalised over N conditions.
    let mut types: Vec<String> = by_type[0].keys().cloned().collect();
    types.sort();
    let width = 28 + conditions.len() * 15;
    print!("\n{:<28}", "");
    for name in cond_names {
        print!("  {name:^13}");
    }
    println!();
    print!("{:<28}", "question type");
    for _ in cond_names {
        print!("     ctx    ans");
    }
    println!();
    println!("{}", "─".repeat(width));
    let mut overall = vec![Acc::default(); conditions.len()];
    for t in &types {
        print!("{t:<28}");
        for (ci, ov) in overall.iter_mut().enumerate() {
            let a = by_type[ci].get(t).copied().unwrap_or_default();
            *ov = merge(*ov, a);
            print!("  {} {}", a.ctx_cell(), a.ans_cell());
        }
        println!();
    }
    println!("{}", "─".repeat(width));
    print!("{:<28}", "OVERALL");
    for ov in &overall {
        print!("  {} {}", ov.ctx_cell(), ov.ans_cell());
    }
    println!();
    // Cost: what the agent spent per question under each retrieval regime — the
    // Layer-2 question the recall numbers can't answer (does the retrieval earn its
    // budget?). Tokens are best-effort (backends that omit `usage` show 0). Note
    // Self-training's cost tracks mneme's: training is local retrieval, not LLM calls.
    if answered > 0 {
        let n = answered as f64;
        println!(
            "\n{:<28}  calls/q   prompt/q   completion/q",
            "cost (agent)"
        );
        for (ci, label) in cond_names.iter().enumerate() {
            let c = cost[ci];
            println!(
                "{label:<28}  {:>6.2}   {:>8.0}   {:>10.0}",
                c.calls as f64 / n,
                c.prompt as f64 / n,
                c.completion as f64 / n,
            );
        }
    }
    println!(
        "\nctx = an evidence session reached top-k retrieval; ans = the judge marked the answer\n\
         correct. flat = sparse+dense RRF · mneme = hybrid + cold spread · self-training explicitly\n\
         creates co-retrieval edges from session probes. Treat it as an ablation, not grounded\n\
         relevance. (Raise --k for more.)"
    );
}

// ---- 2WikiMultiHopQA (real multi-hop benchmark) ---------------------------
//
// The setting where graph training can actually pay off: pool every question's
// context paragraphs into ONE shared corpus (so it's a real retrieval problem —
// find 2 gold paragraphs among thousands, not among a question's own 10), split
// the questions train/test, TRAIN the graph on the train split (feedback on the
// gold each question used), then measure cold-vs-trained retrieval
// and answer accuracy on held-out questions. HippoRAG-comparable.

/// One 2Wiki item. `context`/`supporting_facts` are parsed from `serde_json::Value`
/// (arrays-of-arrays) rather than tuples, so a slightly-off release still loads.
#[derive(Deserialize)]
struct TwikiItem {
    #[serde(default)]
    question: String,
    #[serde(default)]
    answer: String,
    #[serde(default, rename = "type")]
    qtype: String,
    /// Each entry: `[title, [sentence, …]]`.
    #[serde(default)]
    context: Vec<serde_json::Value>,
    /// Each entry: `[title, sentence_idx]` — only the title matters for gold.
    #[serde(default)]
    supporting_facts: Vec<serde_json::Value>,
}

/// `(title, joined-sentences)` from a `["title", ["s1", "s2"]]` context entry.
fn ctx_paragraph(v: &serde_json::Value) -> Option<(String, String)> {
    let arr = v.as_array()?;
    let title = arr.first()?.as_str()?.to_string();
    let text = arr
        .get(1)?
        .as_array()?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    Some((title, text))
}

/// The distinct gold paragraph titles for an item (the answer chain).
fn gold_titles(item: &TwikiItem) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in &item.supporting_facts {
        if let Some(t) = f
            .as_array()
            .and_then(|a| a.first())
            .and_then(|x| x.as_str())
            && !out.iter().any(|s| s == t)
        {
            out.push(t.to_string());
        }
    }
    out
}

/// Answer + retrieval accuracy for one 2Wiki condition.
#[derive(Default, Clone, Copy)]
struct MhAcc {
    n: usize,
    hit: usize,      // gold paragraph reached top-k (any)
    recall_sum: f64, // fraction of gold paragraphs reached top-k
    correct: usize,  // judge marked the answer right
}
impl MhAcc {
    fn add(&mut self, hit: bool, recall: f64, correct: bool) {
        self.n += 1;
        self.hit += hit as usize;
        self.recall_sum += recall;
        self.correct += correct as usize;
    }
    fn cell(&self, x: f64) -> String {
        if self.n == 0 {
            format!("{:>6}", "-")
        } else {
            format!("{:>6.3}", x / self.n as f64)
        }
    }
}

/// Grounded graph training over the train split: for each train question, reflect
/// on the gold paragraphs that jointly answer it and reinforce both directions.
/// Uses only TRAIN gold; the test split is held out, so any lift on test is
/// generalisation rather than test leakage.
async fn train_multihop(
    mem: &Memory,
    train: &[TwikiItem],
    title_of: &HashMap<String, NodeId>,
    seeds: usize,
) {
    for item in train {
        let gold: Vec<NodeId> = gold_titles(item)
            .iter()
            .filter_map(|t| title_of.get(t).copied())
            .collect();
        let _ = mem
            .retrieve_seeded(
                &item.question,
                seeds,
                budget_mneme(),
                StatusFilter::ACTIVE,
                &[],
            )
            .await;
        // The gold paragraphs co-occur in the answer chain: reinforce their pairwise
        // association (both directions), the edge a real recall+reflect would learn.
        for a in 0..gold.len() {
            for b in 0..gold.len() {
                if a != b {
                    let _ = mem
                        .apply_feedback(Some(gold[a]), gold[b], Signal::RelevantNew)
                        .await;
                }
            }
        }
    }
}

/// Run the 2WikiMultiHopQA benchmark. See the module note above the structs.
#[expect(clippy::too_many_arguments)]
async fn run_multihop(
    path: &str,
    limit: usize,
    offset: usize,
    k: usize,
    seeds: usize,
    lexical: bool,
    train_frac: f64,
    agentic: bool,
    max_steps: usize,
    agent: &Llm,
    judge: &Llm,
) {
    let file = std::fs::File::open(path).unwrap_or_else(|e| {
        eprintln!("cannot open {path}: {e}");
        std::process::exit(1);
    });
    let all: Vec<TwikiItem> = serde_json::from_reader(std::io::BufReader::new(file))
        .unwrap_or_else(|e| {
            eprintln!("cannot parse 2Wiki JSON ({path}): {e}");
            std::process::exit(1);
        });
    let items: Vec<&TwikiItem> = all.iter().skip(offset).take(limit).collect();
    if items.is_empty() {
        eprintln!("no items loaded (offset {offset}, limit {limit})");
        return;
    }

    // Pool a shared corpus: one node per distinct paragraph title, across ALL loaded
    // questions — so retrieval is find-the-gold-among-everything, not among 10.
    let mut corpus: BTreeMap<String, String> = BTreeMap::new();
    for it in &items {
        for c in &it.context {
            if let Some((title, text)) = ctx_paragraph(c) {
                corpus.entry(title).or_insert(text);
            }
        }
    }
    let n_train = ((items.len() as f64 * train_frac) as usize).min(items.len());
    let train: Vec<TwikiItem> = items
        .iter()
        .take(n_train)
        .map(|it| clone_item(it))
        .collect();
    let test: Vec<&TwikiItem> = items.iter().skip(n_train).copied().collect();

    println!(
        "mneme-eval · 2WikiMultiHopQA · {} questions ({n_train} train / {} test) · {} corpus paragraphs\n  embedder: {} · agent: {} · judge: {} · k={k}{}",
        items.len(),
        test.len(),
        corpus.len(),
        if lexical { "lexical" } else { "bge-base" },
        agent.model,
        judge.model,
        if agentic {
            format!(" · agentic ≤{max_steps} steps")
        } else {
            String::new()
        },
    );
    if judge.model == agent.model && judge.base_url == agent.base_url {
        eprintln!("  (note: no --judge-model set — the agent is grading itself; weak)");
    }
    if test.is_empty() {
        eprintln!("no held-out test questions — lower --train-frac");
        return;
    }

    let embedder = make_embedder(lexical);
    let conditions = [Kind::Flat, Kind::Mneme, Kind::Train];
    let cond_names = ["flat", "mneme (cold)", "mneme+grounded-train"];
    let mut by_type: Vec<HashMap<String, MhAcc>> = vec![HashMap::new(); conditions.len()];
    let mut cost: Vec<Usage> = vec![Usage::default(); conditions.len()];

    for (ci, kind) in conditions.iter().enumerate() {
        // Fresh store per condition: ingest the shared corpus, then (train only) train.
        let mem = memory(embedder.clone());
        let mut title_of: HashMap<String, NodeId> = HashMap::new();
        for (title, text) in &corpus {
            let summary = format!("{title}: {text}");
            let id = mem
                .ingest(Ingest::new(&summary, b"", &[], Provenance::derived_empty()))
                .await
                .unwrap();
            title_of.insert(title.clone(), id);
        }
        if matches!(kind, Kind::Train) {
            eprintln!("· training on {n_train} questions (grounded feedback on train gold)…");
            train_multihop(&mem, &train, &title_of, seeds).await;
        }
        eprintln!(
            "· {} — testing {} held-out questions…",
            cond_names[ci],
            test.len()
        );
        let budget = budget_for(*kind, k);
        for item in &test {
            let gold: Vec<NodeId> = gold_titles(item)
                .iter()
                .filter_map(|t| title_of.get(t).copied())
                .collect();
            let gold_set: HashSet<NodeId> = gold.iter().copied().collect();
            // Canonical retrieval → retrieval metrics (hit@k, recall@k of gold paras).
            let retrieved: Vec<(NodeId, String)> = mem
                .retrieve_seeded(&item.question, seeds, budget, StatusFilter::ACTIVE, &[])
                .await
                .unwrap()
                .into_iter()
                .take(k)
                .map(|r| (r.node.id(), r.node.summary().to_string()))
                .collect();
            let reached = gold
                .iter()
                .filter(|g| retrieved.iter().any(|(id, _)| id == *g))
                .count();
            let recall = if gold.is_empty() {
                0.0
            } else {
                reached as f64 / gold.len() as f64
            };
            let hit = reached > 0;
            // Answer: agentic (multi-search) or single-shot from the retrieved context.
            let before = agent.usage();
            let answer = if agentic {
                lme_agent_loop(
                    &mem,
                    &item.question,
                    seeds,
                    budget,
                    k,
                    &gold_set,
                    agent,
                    max_steps,
                )
                .await
                .1
            } else {
                let ctx: Vec<&str> = retrieved.iter().map(|(_, s)| s.as_str()).collect();
                agent.answer(&ctx, &item.question).await
            };
            cost[ci] += agent.usage() - before;
            let correct = judge_answer(judge, &item.question, &item.answer, &answer, false).await;
            let qtype = if item.qtype.is_empty() {
                "unknown".to_string()
            } else {
                item.qtype.clone()
            };
            by_type[ci]
                .entry(qtype)
                .or_default()
                .add(hit, recall, correct);
        }
    }

    // Report: hit@k / recall@k / answer-accuracy per condition, by question type.
    let mut types: Vec<String> = by_type[0].keys().cloned().collect();
    types.sort();
    let width = 20 + conditions.len() * 24;
    print!("\n{:<20}", "");
    for name in cond_names {
        print!("  {name:^22}");
    }
    println!();
    print!("{:<20}", "question type");
    for _ in cond_names {
        print!("   hit@k rec@k   ans");
    }
    println!();
    println!("{}", "─".repeat(width));
    let mut overall = vec![MhAcc::default(); conditions.len()];
    for t in &types {
        print!("{t:<20}");
        for (ci, ov) in overall.iter_mut().enumerate() {
            let a = by_type[ci].get(t).copied().unwrap_or_default();
            ov.n += a.n;
            ov.hit += a.hit;
            ov.recall_sum += a.recall_sum;
            ov.correct += a.correct;
            print!(
                "  {} {} {}",
                a.cell(a.hit as f64),
                a.cell(a.recall_sum),
                a.cell(a.correct as f64)
            );
        }
        println!();
    }
    println!("{}", "─".repeat(width));
    print!("{:<20}", "OVERALL");
    for ov in &overall {
        print!(
            "  {} {} {}",
            ov.cell(ov.hit as f64),
            ov.cell(ov.recall_sum),
            ov.cell(ov.correct as f64)
        );
    }
    println!();
    let n = test.len() as f64;
    println!(
        "\n{:<20}  calls/q   prompt/q   completion/q",
        "cost (agent)"
    );
    for (ci, name) in cond_names.iter().enumerate() {
        let c = cost[ci];
        println!(
            "{name:<20}  {:>6.2}   {:>8.0}   {:>10.0}",
            c.calls as f64 / n,
            c.prompt as f64 / n,
            c.completion as f64 / n,
        );
    }
    println!(
        "\nhit@k = any gold paragraph in top-k · rec@k = fraction of gold reached · ans = judged\n\
         correct. flat = sparse+dense RRF · mneme = hybrid + cold spread · grounded-train = spread over a graph\n\
         trained by feedback on TRAIN gold, tested on HELD-OUT questions. Its rec@k/ans above\n\
         mneme's means grounded graph training generalised. Corpus is pooled across\n\
         all questions, so retrieval finds gold among everything (raise --limit to harden)."
    );
}

/// Deep-copy a borrowed item into an owned one (the train split needs owned items
/// after the borrow of `items` is split).
fn clone_item(it: &TwikiItem) -> TwikiItem {
    TwikiItem {
        question: it.question.clone(),
        answer: it.answer.clone(),
        qtype: it.qtype.clone(),
        context: it.context.clone(),
        supporting_facts: it.supporting_facts.clone(),
    }
}

fn print_help() {
    println!(
        "mneme-eval — deterministic memory retrieval evaluation\n\
         \n\
         USAGE:\n\
           mneme-eval [OPTIONS]\n\
         \n\
         DEFAULT (no API key):\n\
           Runs fixed-seed BM25, dense-only, flat-hybrid, controlled-graph,\n\
           shipped-default-graph, clean trained-graph, and 1:1 noisy trained-graph\n\
           conditions over the same corpus and query order. Reports retrieval,\n\
           latency, injected-context, build-cost, and graph-density metrics.\n\
         \n\
         OPTIONS:\n\
           --nodes N[,N...]      Corpus sizes (default: 160; minimum: 160)\n\
           --seed N              Dataset seed (default: 42)\n\
           --id-seeds N[,N...]   Independent deterministic ID-order seeds (default: dataset seed)\n\
           --k N                 Retrieved context size (default: 10)\n\
           --seeds N             Dense ANN seeds for graph retrieval (default: 10)\n\
           --training-reps N     Feedback/self-training passes (default: 10)\n\
           --graph-diagnostics   Add per-query graph-stage traces and failure classes to JSON\n\
           --json                Emit the complete versioned JSON report\n\
           --output PATH         Also retain that JSON report at PATH\n\
           --source-revision REV Source revision recorded in offline JSON\n\
           --machine-label LABEL Machine label recorded in offline JSON\n\
           --variant NAME        Benchmark variant recorded in offline JSON\n\
           --legacy-layer1       Run the historical five-condition recall table\n\
           --lexical             Use hashing embeddings in legacy/LLM modes\n\
           --ollama-model MODEL  Run reconstruction via local Ollama (no key)\n\
           -h, --help            Print this help\n\
         \n\
         OFFLINE POLICY OVERRIDES (default suite only):\n\
           --query-conditioning F  Query-conditioning strength in [0,1]\n\
           --graph-slot-cap N      Maximum graph interventions in final ranking\n\
           --graph-seed-cap N      Maximum direct roots used for traversal\n\
           --graph-weight F        Finite non-negative graph fusion weight\n\
           --similarity-link-cap N Maximum embedding-similarity links per ingest\n\
           --relevance-ratio F     Adaptive relevance ratio in [0,1]\n\
           --dedup-similarity F    Semantic dedup threshold in [0,1]\n\
           --max-depth N           Traversal depth (0..255)\n\
         \n\
         BUILD MODES:\n\
           Default features use ephemeral Cozo/HNSW plus BGE-base. The first run\n\
           may populate fastembed's model cache; later runs are local/offline.\n\
           `cargo run -p mneme-eval --no-default-features -- --json` is a fast\n\
           harness smoke test using the reference store + hashing embedder.\n\
         \n\
         ADVANCED DATASETS / LLM ANSWERING:\n\
           --longmemeval PATH    Run LongMemEval (requires an LLM option)\n\
           --multihop PATH       Run 2WikiMultiHopQA (requires an LLM option)\n\
           --agent-base-url URL  OpenAI-compatible endpoint\n\
           --agent-model MODEL   Answering model name\n\
         \n\
         Examples:\n\
           cargo run --release -p mneme-eval -- --nodes 160,640 --json\n\
           cargo run -p mneme-eval -- --legacy-layer1\n\
           cargo run -p mneme-eval -- --ollama-model qwen3.5:9b --limit 5"
    );
}

fn parse_node_targets(args: &[String], minimum: usize) -> Result<Vec<usize>, String> {
    let Some(raw) = flag_str(args, "--nodes") else {
        return Ok(vec![minimum]);
    };
    let mut targets = Vec::new();
    for value in raw.split(',') {
        let value = value.trim();
        let parsed = value
            .parse::<usize>()
            .map_err(|_| format!("invalid --nodes value {value:?}"))?;
        if parsed < minimum {
            return Err(format!(
                "--nodes value {parsed} is smaller than the {minimum}-fact gold corpus"
            ));
        }
        if !targets.contains(&parsed) {
            targets.push(parsed);
        }
    }
    if targets.is_empty() {
        return Err("--nodes needs at least one comma-separated integer".to_string());
    }
    Ok(targets)
}

fn parse_id_seeds(args: &[String], dataset_seed: u64) -> Result<Vec<u64>, String> {
    let raw = flag_str(args, "--id-seeds").or_else(|| flag_str(args, "--id-seed"));
    let Some(raw) = raw else {
        return Ok(vec![dataset_seed]);
    };
    let mut seeds = Vec::new();
    for value in raw.split(',') {
        let value = value.trim();
        let parsed = value
            .parse::<u64>()
            .map_err(|_| format!("invalid --id-seeds value {value:?}"))?;
        if !seeds.contains(&parsed) {
            seeds.push(parsed);
        }
    }
    if seeds.is_empty() {
        return Err("--id-seeds needs at least one comma-separated integer".to_string());
    }
    Ok(seeds)
}

fn parse_override<T>(args: &[String], name: &str) -> Result<Option<T>, String>
where
    T: std::str::FromStr,
{
    let mut positions = args
        .iter()
        .enumerate()
        .filter_map(|(index, arg)| (arg == name).then_some(index));
    let Some(index) = positions.next() else {
        return Ok(None);
    };
    if positions.next().is_some() {
        return Err(format!("{name} may be specified only once"));
    }
    let raw = args
        .get(index + 1)
        .ok_or_else(|| format!("{name} requires a value"))?;
    raw.parse::<T>()
        .map(Some)
        .map_err(|_| format!("invalid {name} value {raw:?}"))
}

fn parse_unit_interval_override(args: &[String], name: &str) -> Result<Option<f32>, String> {
    let value = parse_override::<f32>(args, name)?;
    if let Some(value) = value
        && (!value.is_finite() || !(0.0..=1.0).contains(&value))
    {
        return Err(format!("{name} must be finite and within [0, 1]"));
    }
    Ok(value)
}

fn parse_nonnegative_finite_override(args: &[String], name: &str) -> Result<Option<f32>, String> {
    let value = parse_override::<f32>(args, name)?;
    if let Some(value) = value
        && (!value.is_finite() || value < 0.0)
    {
        return Err(format!("{name} must be finite and at least 0"));
    }
    Ok(value)
}

fn flag(args: &[String], name: &str) -> Option<usize> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1)?.parse().ok()
}

fn flag_str(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

/// Build the Layer-2 answerer from flags, or `None` (→ Layer 1, no LLM). Presence of
/// `--agent-base-url` switches the harness into answer-accuracy mode.
///
/// e.g. Ollama: `--agent-base-url http://localhost:11434/v1 --agent-model llama3.2:1b`
///      OpenAI: `--agent-base-url https://api.openai.com/v1 --agent-model gpt-4o-mini \
///               --agent-key-env OPENAI_API_KEY`
fn agent_from_args(args: &[String]) -> Option<Llm> {
    if let Some(model) = flag_str(args, "--ollama-model") {
        let base_url = flag_str(args, "--ollama-base-url")
            .unwrap_or_else(|| "http://127.0.0.1:11434/v1".to_string());
        return Some(Llm::new(base_url, model, None));
    }
    let base_url = flag_str(args, "--agent-base-url")?;
    let model = flag_str(args, "--agent-model").unwrap_or_else(|| {
        eprintln!("--agent-model is required with --agent-base-url");
        std::process::exit(2);
    });
    let api_key = flag_str(args, "--agent-key-env").and_then(|v| std::env::var(v).ok());
    Some(Llm::new(base_url, model, api_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixed_eval_memories_assign_identical_node_ids() {
        let embedder: Arc<dyn Embedder> = Arc::new(HashingEmbedder::new(DEFAULT_DIM));
        let first = fixed_eval_memory(embedder.clone(), 42, 7);
        let second = fixed_eval_memory(embedder, 42, 7);
        let mut first_ids = Vec::new();
        let mut second_ids = Vec::new();
        for summary in ["alpha", "beta", "gamma"] {
            first_ids.push(
                first
                    .mem
                    .ingest(Ingest::new(summary, b"", &[], Provenance::derived_empty()))
                    .await
                    .unwrap(),
            );
            second_ids.push(
                second
                    .mem
                    .ingest(Ingest::new(summary, b"", &[], Provenance::derived_empty()))
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(first_ids, second_ids);
    }

    #[tokio::test]
    async fn fixed_eval_id_order_seed_is_independent_of_dataset_clock_seed() {
        let embedder: Arc<dyn Embedder> = Arc::new(HashingEmbedder::new(DEFAULT_DIM));
        let first = fixed_eval_memory(embedder.clone(), 41, 7);
        let second = fixed_eval_memory(embedder.clone(), 42, 7);
        let alternate = fixed_eval_memory(embedder, 42, 8);
        let request = || Ingest::new("alpha", b"", &[], Provenance::derived_empty());
        let first_id = first.mem.ingest(request()).await.unwrap();
        let second_id = second.mem.ingest(request()).await.unwrap();
        let alternate_id = alternate.mem.ingest(request()).await.unwrap();
        assert_eq!(first_id, second_id, "dataset seed must not alter ID order");
        assert_ne!(first_id, alternate_id, "ID seed must alter ID order");
    }

    #[test]
    fn bm25_prefers_the_exact_entity_and_attribute() {
        let facts = vec![
            Fact {
                id: 7,
                text: "Mara keeps a lathe in the north workshop.".to_string(),
            },
            Fact {
                id: 8,
                text: "Mara's favourite colour is teal.".to_string(),
            },
            Fact {
                id: 9,
                text: "Niko's favourite colour is amber.".to_string(),
            },
        ];
        let index = Bm25Index::build(&facts);
        assert_eq!(index.search("What is Mara's favourite colour?", 2)[0], 8);
        assert_eq!(index.search("north workshop lathe", 1), vec![7]);
    }

    #[test]
    fn padded_corpus_is_fixed_seed_and_keeps_gold_questions() {
        let base = gen_dataset(42);
        let base_questions = base.questions.len();
        let a = pad_dataset(base, 168, 42);
        let b = pad_dataset(gen_dataset(42), 168, 42);
        assert_eq!(a.facts.len(), 168);
        assert_eq!(a.questions.len(), base_questions);
        assert_eq!(
            a.facts.iter().map(|f| &f.text).collect::<Vec<_>>(),
            b.facts.iter().map(|f| &f.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn noisy_grounded_plan_is_a_deterministic_one_to_one_derangement() {
        let ds = gen_dataset(42);
        let pairs = noisy_grounded_fact_pairs(&ds);
        let expected_pairs: usize = ds
            .by_entity
            .iter()
            .map(|entity| entity.attr_facts.len())
            .sum();
        assert_eq!(pairs.len(), expected_pairs);

        let mut all_targets = HashSet::new();
        let mut false_pairs = HashSet::new();
        let clean_pairs: HashSet<(u32, u32)> = ds
            .by_entity
            .iter()
            .flat_map(|entity| {
                entity
                    .attr_facts
                    .iter()
                    .copied()
                    .map(move |attribute| (entity.codename_fact, attribute))
            })
            .collect();

        let attributes_per_entity = ds.by_entity[0].attr_facts.len();
        for (entity_index, entity) in ds.by_entity.iter().enumerate() {
            let wrong = &ds.by_entity[(entity_index + 1) % ds.by_entity.len()];
            let start = entity_index * attributes_per_entity;
            let planned = &pairs[start..start + attributes_per_entity];
            assert_eq!(
                planned,
                wrong
                    .attr_facts
                    .iter()
                    .map(|attribute| (entity.codename_fact, *attribute))
                    .collect::<Vec<_>>()
            );
            for pair in planned {
                assert!(false_pairs.insert(*pair), "false pair must be unique");
                assert!(
                    all_targets.insert(pair.1),
                    "wrong mapping must be bijective"
                );
                assert!(!clean_pairs.contains(pair), "false pair cannot be true");
            }
        }
        assert_eq!(false_pairs.len(), clean_pairs.len());
        assert_eq!(all_targets.len(), expected_pairs);
    }

    #[test]
    fn node_targets_are_ordered_deduplicated_and_validated() {
        let args = vec![
            "mneme-eval".to_string(),
            "--nodes".to_string(),
            "160,640,160".to_string(),
        ];
        assert_eq!(parse_node_targets(&args, 160).unwrap(), vec![160, 640]);

        let invalid = vec![
            "mneme-eval".to_string(),
            "--nodes".to_string(),
            "159".to_string(),
        ];
        assert!(parse_node_targets(&invalid, 160).is_err());
    }

    #[test]
    fn id_seeds_are_independent_ordered_and_deduplicated() {
        let args = vec![
            "mneme-eval".to_string(),
            "--id-seeds".to_string(),
            "43,41,43,42".to_string(),
        ];
        assert_eq!(parse_id_seeds(&args, 99).unwrap(), vec![43, 41, 42]);
        assert_eq!(
            parse_id_seeds(&["mneme-eval".to_string()], 99).unwrap(),
            vec![99]
        );
    }

    #[test]
    fn offline_policy_overrides_apply_to_engine_and_both_budget_variants() {
        let args = [
            "mneme-eval",
            "--query-conditioning",
            "0.75",
            "--graph-slot-cap",
            "7",
            "--graph-seed-cap",
            "9",
            "--graph-weight",
            "1.25",
            "--similarity-link-cap",
            "4",
            "--relevance-ratio",
            "0.2",
            "--dedup-similarity",
            "0.9",
            "--max-depth",
            "6",
        ]
        .map(str::to_string);
        let policy = OfflinePolicyOverrides::parse(&args).unwrap();
        assert!(policy.any());

        let config = policy.apply_config(Config::default());
        assert_eq!(config.graph_slot_cap, 7);
        assert_eq!(config.graph_seed_cap, 9);
        assert_eq!(config.graph_weight, 1.25);
        assert_eq!(config.similarity_link_cap, 4);
        assert_eq!(config.budget.query_conditioning, 0.75);
        assert_eq!(config.budget.relevance_ratio, 0.2);
        assert_eq!(config.budget.dedup_similarity, 0.9);
        assert_eq!(config.budget.max_depth, 6);

        let controlled = policy.apply_budget(budget_mneme());
        assert_eq!(
            controlled.query_conditioning,
            config.budget.query_conditioning
        );
        assert_eq!(controlled.relevance_ratio, config.budget.relevance_ratio);
        assert_eq!(controlled.dedup_similarity, config.budget.dedup_similarity);
        assert_eq!(controlled.max_depth, config.budget.max_depth);
    }

    #[test]
    fn empty_offline_policy_preserves_existing_defaults() {
        let args = ["mneme-eval".to_string()];
        let policy = OfflinePolicyOverrides::parse(&args).unwrap();
        assert!(!policy.any());
        let default = Config::default();
        let applied = policy.apply_config(default);
        assert_eq!(applied.graph_slot_cap, default.graph_slot_cap);
        assert_eq!(applied.graph_seed_cap, default.graph_seed_cap);
        assert_eq!(applied.graph_weight, default.graph_weight);
        assert_eq!(applied.similarity_link_cap, default.similarity_link_cap);
        assert_eq!(applied.budget.max_depth, default.budget.max_depth);
        assert_eq!(
            applied.budget.query_conditioning,
            default.budget.query_conditioning
        );

        let controlled = budget_mneme();
        let applied_controlled = policy.apply_budget(controlled);
        assert_eq!(applied_controlled.max_depth, controlled.max_depth);
        assert_eq!(
            applied_controlled.query_conditioning,
            controlled.query_conditioning
        );
        assert_eq!(
            applied_controlled.relevance_ratio,
            controlled.relevance_ratio
        );
        assert_eq!(
            applied_controlled.dedup_similarity,
            controlled.dedup_similarity
        );
    }

    #[test]
    fn offline_policy_rejects_out_of_range_nonfinite_missing_and_duplicate_values() {
        for args in [
            vec!["mneme-eval", "--query-conditioning", "-0.1"],
            vec!["mneme-eval", "--relevance-ratio", "1.01"],
            vec!["mneme-eval", "--dedup-similarity", "NaN"],
            vec!["mneme-eval", "--graph-weight", "-1"],
            vec!["mneme-eval", "--graph-weight", "inf"],
            vec!["mneme-eval", "--graph-slot-cap", "-1"],
            vec!["mneme-eval", "--max-depth", "256"],
            vec!["mneme-eval", "--graph-seed-cap"],
            vec![
                "mneme-eval",
                "--graph-slot-cap",
                "2",
                "--graph-slot-cap",
                "3",
            ],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_string).collect();
            assert!(
                OfflinePolicyOverrides::parse(&args).is_err(),
                "expected invalid policy args: {args:?}"
            );
        }
    }

    #[test]
    fn percentile_uses_nearest_rank_ceiling() {
        let xs = [1.0, 2.0, 3.0, 4.0, 100.0];
        assert_eq!(percentile_f64(&xs, 0.5), 3.0);
        assert_eq!(percentile_f64(&xs, 0.95), 100.0);
        assert_eq!(percentile_usize(&[1, 2, 3, 4, 100], 0.95), 100);
    }

    #[test]
    fn retrieval_summary_has_stable_json_field_names() {
        let value = serde_json::to_value(RetrievalSummary {
            questions: 2,
            hit_at_k: 0.5,
            mrr: 0.25,
            recall_at_k: 0.75,
        })
        .unwrap();
        assert_eq!(value["questions"], 2);
        assert_eq!(value["hit_at_k"], 0.5);
        assert_eq!(value["mrr"], 0.25);
        assert_eq!(value["recall_at_k"], 0.75);
    }

    #[test]
    fn effective_policy_serializes_the_full_budget_and_graph_caps() {
        let config = Config::default();
        let value =
            serde_json::to_value(EffectiveRetrievalPolicy::new(&config, config.budget)).unwrap();
        assert_eq!(value["budget"]["max_nodes"], config.budget.max_nodes);
        assert_eq!(value["budget"]["max_depth"], config.budget.max_depth);
        assert_eq!(
            value["budget"]["min_relevance"],
            config.budget.min_relevance
        );
        assert_eq!(value["budget"]["explore"], 0.0);
        assert_eq!(
            value["budget"]["relevance_ratio"],
            config.budget.relevance_ratio
        );
        assert_eq!(
            value["budget"]["dedup_similarity"],
            config.budget.dedup_similarity
        );
        assert_eq!(
            value["budget"]["query_conditioning"],
            config.budget.query_conditioning
        );
        assert_eq!(value["graph_seed_cap"], config.graph_seed_cap);
        assert_eq!(value["graph_weight"], config.graph_weight);
        assert_eq!(value["graph_slot_cap"], config.graph_slot_cap);
        assert_eq!(value["lexical_k"], config.lexical_k);
        assert_eq!(value["rrf_constant"], config.rrf_constant);
        assert_eq!(value["dense_weight"], config.dense_weight);
        assert_eq!(value["lexical_weight"], config.lexical_weight);
        assert!(value.get("candidate_admission_ratio").is_none());
        assert_eq!(value["similarity_link_cap"], config.similarity_link_cap);
        assert_eq!(
            value["similarity_link_threshold"],
            config.similarity_link_threshold
        );
        assert_eq!(value["min_similarity_links"], config.min_similarity_links);
        assert_eq!(value["coretrieval_link_cap"], config.coretrieval_link_cap);
    }

    #[test]
    fn ordinary_condition_reports_omit_graph_diagnostics() {
        let report = OfflineAcc::default().finish("test", "test", None);
        let value = serde_json::to_value(report).unwrap();
        assert!(value.get("graph_diagnostics").is_none());
    }

    #[test]
    fn graph_failure_classes_separate_rescue_displacement_and_stage_misses() {
        assert_eq!(
            classify_graph_query(None, Some(3), 10, false, true, Some(1), Some(1), true, true),
            "graph_rescue"
        );
        assert_eq!(
            classify_graph_query(Some(9), None, 10, false, true, None, None, false, true,),
            "base_gold_displaced_by_graph_candidate"
        );
        assert_eq!(
            classify_graph_query(None, None, 10, false, false, None, None, false, false),
            "no_gold_edge_from_roots"
        );
        assert_eq!(
            classify_graph_query(None, None, 10, false, true, Some(5), Some(5), false, false),
            "gold_outside_graph_intervention_quota"
        );
        assert_eq!(
            classify_graph_query(
                Some(11),
                None,
                10,
                false,
                true,
                Some(1),
                Some(1),
                false,
                false
            ),
            "direct_gold_graph_evidence_not_admitted"
        );
    }

    #[test]
    fn report_output_atomically_replaces_existing_file_without_temp_leaks() {
        let sequence = REPORT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "mneme-eval-atomic-test-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let output = directory.join("report.json");
        std::fs::write(&output, b"old").unwrap();

        write_report_atomic(&output, br#"{"new":true}"#).unwrap();

        assert_eq!(std::fs::read(&output).unwrap(), br#"{"new":true}"#);
        assert!(
            std::fs::read_dir(&directory).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")),
            "successful replacement must not leak adjacent temporaries"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_report_rename_cleans_up_the_adjacent_temporary() {
        let sequence = REPORT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "mneme-eval-atomic-failure-test-{}-{sequence}",
            std::process::id()
        ));
        let output = directory.join("report.json");
        std::fs::create_dir_all(&output).unwrap();

        assert!(write_report_atomic(&output, b"cannot replace a directory").is_err());
        assert!(
            std::fs::read_dir(&directory).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")),
            "failed replacement must clean up its adjacent temporary"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reciprocal_rank_is_cut_off_at_k() {
        let mut acc = Agg::default();
        acc.add(&[3], &[1, 2, 3], 2);
        let summary = acc.summary();
        assert_eq!(summary.hit_at_k, 0.0);
        assert_eq!(summary.recall_at_k, 0.0);
        assert_eq!(summary.mrr, 0.0);
    }

    #[test]
    fn ollama_shortcut_uses_local_openai_compat_endpoint() {
        let args = vec![
            "mneme-eval".to_string(),
            "--ollama-model".to_string(),
            "qwen3.5:9b".to_string(),
        ];
        let llm = agent_from_args(&args).unwrap();
        assert_eq!(llm.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(llm.model, "qwen3.5:9b");
        assert!(llm.api_key.is_none());
    }

    #[test]
    fn parse_action_picks_first_directive_case_insensitively() {
        // ANSWER wins when present, whatever the casing.
        assert!(
            matches!(parse_action("ANSWER: border collie"), Action::Answer(a) if a == "border collie")
        );
        assert!(matches!(parse_action("answer: teal"), Action::Answer(a) if a == "teal"));
        // SEARCH is recognised and its query extracted.
        assert!(matches!(parse_action("SEARCH: dog breed"), Action::Search(q) if q == "dog breed"));
        // Preamble before the directive is tolerated (first matching line wins).
        assert!(
            matches!(parse_action("thinking...\nSEARCH: car plate"), Action::Search(q) if q == "car plate")
        );
        // A reply with neither directive is treated as a bare final answer.
        assert!(matches!(parse_action("just teal"), Action::Answer(a) if a == "just teal"));
    }

    #[test]
    fn usage_deltas_and_accumulates() {
        let a = Usage {
            calls: 5,
            prompt: 100,
            completion: 40,
        };
        let b = Usage {
            calls: 2,
            prompt: 30,
            completion: 10,
        };
        let d = a - b;
        assert_eq!((d.calls, d.prompt, d.completion), (3, 70, 30));
        let mut acc = Usage::default();
        acc += d;
        acc += d;
        assert_eq!((acc.calls, acc.prompt, acc.completion), (6, 140, 60));
    }

    #[test]
    fn abstention_detects_declines_not_values() {
        assert!(is_abstention("I don't know"));
        assert!(is_abstention("That information is not available."));
        assert!(!is_abstention("border collie"));
    }

    #[test]
    fn twiki_parses_context_and_gold() {
        // A 2Wiki item: context is [[title, [sentences]]], supporting_facts is
        // [[title, sent_idx]] — parse a paragraph and the (deduped) gold titles.
        let item: TwikiItem = serde_json::from_value(serde_json::json!({
            "question": "q", "answer": "a", "type": "bridge",
            "context": [
                ["Ada Lovelace", ["Born 1815.", "Worked on the Analytical Engine."]],
                ["Charles Babbage", ["Designed the Analytical Engine."]]
            ],
            "supporting_facts": [["Ada Lovelace", 1], ["Charles Babbage", 0], ["Ada Lovelace", 0]]
        }))
        .unwrap();
        let (title, text) = ctx_paragraph(&item.context[0]).unwrap();
        assert_eq!(title, "Ada Lovelace");
        assert_eq!(text, "Born 1815. Worked on the Analytical Engine.");
        // Gold titles are deduped and order-preserving (Ada appears twice → once).
        assert_eq!(gold_titles(&item), vec!["Ada Lovelace", "Charles Babbage"]);
    }

    #[test]
    fn twiki_tolerates_malformed_entries() {
        // Missing sentences / non-array entries don't panic — they just drop.
        assert!(ctx_paragraph(&serde_json::json!("not-an-array")).is_none());
        assert!(ctx_paragraph(&serde_json::json!(["only-title"])).is_none());
        let item: TwikiItem = serde_json::from_value(serde_json::json!({
            "question": "q", "supporting_facts": [["Only Title"], "junk"]
        }))
        .unwrap();
        assert_eq!(gold_titles(&item), vec!["Only Title"]);
    }
}
