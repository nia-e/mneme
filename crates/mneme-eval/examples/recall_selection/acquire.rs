//! One real retrieval per case/variant against a fresh disposable store.
//! Assessment files are deliberately not represented in this module.

use super::pool::{Candidate, POOL_SCHEMA, Pool, PoolCase, Stamp};
use mneme_body::InlineStore;
use mneme_core::ports::{
    Clock, Embedder, GraphStore, LexicalIndex, StatusFilter, Traversal, VectorIndex,
};
use mneme_core::{NodeId, Provenance, Timestamp};
use mneme_engine::{Config, DeterministicNodeIdSource, Ingest, Memory};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

#[derive(Deserialize)]
struct Inputs {
    schema: String,
    split: String,
    cases: Vec<InputCase>,
}

#[derive(Deserialize)]
struct InputCase {
    id: String,
    query: String,
    budget_bytes: u32,
    nodes: Vec<InputNode>,
}

#[derive(Deserialize)]
struct InputNode {
    key: String,
    status: String,
    summary: String,
}

struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        1_700_000_000_000
    }
}

fn seed(case: &str, variant: &str) -> u64 {
    let hash = Sha256::digest(format!("recall-selection-v1:{case}:{variant}").as_bytes());
    u64::from_le_bytes(hash[..8].try_into().unwrap())
}

fn embedder() -> Result<Arc<dyn Embedder>, String> {
    #[cfg(feature = "fastembed")]
    {
        Ok(Arc::new(
            mneme_embed::FastEmbedder::new().map_err(|e| e.to_string())?,
        ))
    }
    #[cfg(not(feature = "fastembed"))]
    {
        Ok(Arc::new(mneme_embed::HashingEmbedder::new(
            mneme_embed::DEFAULT_DIM,
        )))
    }
}

pub async fn acquire(text: &str) -> Result<Pool, String> {
    let inputs: Inputs = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if inputs.schema != "mneme.recall-selection.inputs.v1"
        || !["dev", "test"].contains(&inputs.split.as_str())
    {
        return Err("invalid input fixture schema/split".into());
    }
    let setup_started = Instant::now();
    let embedder = embedder()?;
    let setup_ms = setup_started.elapsed().as_secs_f64() * 1_000.0;
    let mut cases = Vec::new();
    for case in &inputs.cases {
        // Freeze exactly one real retrieval. The paired variant changes only
        // how the selector sees this same admitted pool, never admission,
        // source ranks, IDs, embeddings, or the disposable store's history.
        let original = acquire_case(case, embedder.clone()).await?;
        let mut reversed = original.clone();
        reversed.variant = "reversed".into();
        cases.push(original);
        cases.push(reversed);
    }
    let pool = Pool {
        schema: POOL_SCHEMA.into(),
        split: inputs.split,
        cases,
        setup_ms,
    };
    pool.validate()?;
    Ok(pool)
}

async fn acquire_case(case: &InputCase, embedder: Arc<dyn Embedder>) -> Result<PoolCase, String> {
    let started = Instant::now();
    #[cfg(feature = "cozo")]
    let store = Arc::new(mneme_cozo::CozoStore::new(embedder.dim()).map_err(|e| e.to_string())?);
    #[cfg(not(feature = "cozo"))]
    let store = Arc::new(mneme_cozo::MemStore::new(embedder.dim()));
    let graph: Arc<dyn GraphStore> = store.clone();
    let vectors: Arc<dyn VectorIndex> = store.clone();
    let traversal: Arc<dyn Traversal> = store.clone();
    let lexical: Arc<dyn LexicalIndex> = store.clone();
    let mut config = Config::default();
    config.budget.max_nodes = 32;
    config.budget.dedup_similarity = 1.0;
    let memory = Memory::new(
        graph,
        vectors,
        traversal,
        embedder.clone(),
        Arc::new(FixedClock),
        config,
    )
    .with_lexical_index(lexical)
    .with_body_store(Arc::new(InlineStore::new()))
    .with_node_id_source(Arc::new(DeterministicNodeIdSource::new(seed(
        &case.id, "frozen",
    ))));
    let mut keys: HashMap<NodeId, String> = HashMap::new();
    for node in &case.nodes {
        let ingest = Ingest::candidate(&node.summary, b"", &[], Provenance::derived_empty());
        let ingest = match node.status.as_str() {
            "active" => ingest.active(),
            "candidate" => ingest,
            _ => return Err(format!("invalid node status in {}", case.id)),
        };
        let id = memory.ingest(ingest).await.map_err(|e| e.to_string())?;
        if keys.insert(id, node.key.clone()).is_some() {
            return Err(format!("duplicate generated ID in {}", case.id));
        }
    }
    let batch = memory
        .retrieve_batch_seeded(&case.query, 16, config.budget, StatusFilter::default(), &[])
        .await
        .map_err(|e| e.to_string())?;
    let mut candidates = Vec::new();
    for (lane, hits) in [
        ("primary", batch.primary),
        ("probationary", batch.probationary),
    ] {
        for hit in hits {
            candidates.push(Candidate {
                node_key: keys
                    .get(&hit.node.id())
                    .ok_or("retrieval returned unknown ID")?
                    .clone(),
                id: hit.node.id(),
                lane: lane.into(),
                source_rank: hit.lane_rank.get(),
                summary: hit.node.summary().into(),
                embedding: Vec::new(),
            });
        }
    }
    let feature_started = Instant::now();
    if !candidates.is_empty() {
        let summaries: Vec<_> = candidates.iter().map(|c| c.summary.as_str()).collect();
        let embeddings = embedder
            .embed(&summaries)
            .await
            .map_err(|e| e.to_string())?;
        if embeddings.len() != candidates.len() {
            return Err("embedder returned wrong document count".into());
        }
        for (candidate, embedding) in candidates.iter_mut().zip(embeddings) {
            candidate.embedding = embedding;
        }
    }
    let feature_ms = feature_started.elapsed().as_secs_f64() * 1_000.0;
    Ok(PoolCase {
        case_id: case.id.clone(),
        variant: "original".into(),
        query: case.query.clone(),
        budget_bytes: case.budget_bytes,
        corpus_keys: case.nodes.iter().map(|n| n.key.clone()).collect(),
        candidates,
        stamp: Stamp {
            policy_contract: batch.stamp.retrieval_policy.contract.into(),
            policy_fingerprint: batch.stamp.retrieval_policy.fingerprint,
            embedding: batch.stamp.index_set.embedding,
            vector_semantics: batch.stamp.index_set.vector_semantics.into(),
            lexical_semantics: batch.stamp.index_set.lexical_semantics.map(str::to_owned),
            reranker_semantics: batch.stamp.index_set.reranker_semantics.map(str::to_owned),
        },
        acquisition_ms: started.elapsed().as_secs_f64() * 1_000.0,
        feature_ms,
    })
}

#[cfg(all(test, not(feature = "fastembed")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn paired_variants_share_one_frozen_retrieval() {
        let input = serde_json::json!({
            "schema":"mneme.recall-selection.inputs.v1",
            "split":"dev",
            "cases":[{"id":"tiny","query":"orchard keys","budget_bytes":4096,
                "nodes":[
                    {"key":"n01","status":"active","summary":"The orchard keys are in the blue box."},
                    {"key":"n02","status":"active","summary":"The bridge closes at dusk."},
                    {"key":"n03","status":"candidate","summary":"Candidate note on orchard access."}
                ]
            }]
        });
        let pool = acquire(&input.to_string()).await.unwrap();
        assert_eq!(pool.cases.len(), 2);
        let a = &pool.cases[0];
        let b = &pool.cases[1];
        assert_eq!(a.variant, "original");
        assert_eq!(b.variant, "reversed");
        assert_eq!(
            serde_json::to_value(&a.candidates).unwrap(),
            serde_json::to_value(&b.candidates).unwrap()
        );
        assert_eq!(a.stamp.policy_fingerprint, b.stamp.policy_fingerprint);
        assert_eq!(a.acquisition_ms, b.acquisition_ms);
    }
}
