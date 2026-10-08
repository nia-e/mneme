use std::{collections::BTreeMap, env, fs, path::Path};

use cozo::{DataValue, DbInstance, ScriptMutability};
use mneme_core::{BodyRef, EmbeddingFingerprint, Node, NodeId, NodeStatus, Provenance};
use ulid::Ulid;

const DIM: usize = 4;

fn run(db: &DbInstance, script: &str, params: BTreeMap<String, DataValue>) {
    db.run_script(script, params, ScriptMutability::Mutable)
        .unwrap_or_else(|error| panic!("script failed: {error:?}\n{script}"));
}

fn status_name(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Candidate { .. } => "candidate",
        NodeStatus::Active => "active",
        NodeStatus::Archived => "archived",
    }
}

fn vector(index: usize) -> Vec<f64> {
    // Deterministic, finite, non-zero vectors with enough near-neighbour churn to
    // make the old bulk HNSW builder exercise pruning at production m=16.
    let angle = (index as f64) * 0.173_205_080_756_887_73;
    vec![
        angle.cos() + 1.25,
        angle.sin() + 1.25,
        ((index * 17 % 31) as f64 + 1.0) / 32.0,
        ((index * 29 % 37) as f64 + 1.0) / 38.0,
    ]
}

fn main() {
    let output = env::args().nth(1).expect("usage: generator OUTPUT.sqlite");
    let output = Path::new(&output);
    assert!(
        !output.exists(),
        "refusing to overwrite {}",
        output.display()
    );

    let db = DbInstance::new("sqlite", output.to_str().unwrap(), "").unwrap();
    db.run_default(
        "{:create node {id: String => data: String, status: String}}\n\
         {:create node_tag {id: String, tag: String}}\n\
         {::index create node_tag:by_tag {tag}}\n\
         {:create node_search {id: String => summary: String, status: String}}\n\
         {::index create node_search:by_status {status}}\n\
         {:create node_vec {id: String => e: <F32; 4>, status: String}}\n\
         {:create edge {from: String, to: String => weight: Float, kind: String, last_reinforced: Int, trials: Int, interference: Int}}\n\
         {::index create edge:by_to {to}}\n\
         {:create edge_anchor {from: String, to: String => start: Int, end: Int}}\n\
         {:create contradiction {lo: String, hi: String => observations: Int, first_seen: Int, last_seen: Int, resolution: String?}}\n\
         {:create merge_candidate {lo: String, hi: String => observations: Int, first_seen: Int, last_seen: Int, resolution: String?}}\n\
         {:create full_merge_commit {lo: String, hi: String => winner: String, loser: String, applied_at: Int}}\n\
         {:create supersede_commit {lo: String, hi: String => winner: String, loser: String, applied_at: Int}}\n\
         {:create remote_edge {from: String, target_db: String, target: String => weight: Float}}\n\
         {:create feedback_retry {key: String => fingerprint: String, applied_at: Int}}\n\
         {:create feedback_retry_order {epoch: String, sequence: Int, key: String => marker: Bool}}\n\
         {:create meta {k: String => v: String}}",
    )
    .unwrap();

    for index in 0..96usize {
        let id = NodeId(Ulid::from((index + 1) as u128));
        let status = match index % 12 {
            0..=7 => NodeStatus::Active,
            8..=9 => NodeStatus::Candidate { use_count: 0 },
            _ => NodeStatus::Archived,
        };
        // Eight active rows deliberately tokenize to no FTS terms. They make
        // the 0.8.6 base-row N and 0.13 index-document N observably different
        // while remaining valid, ordinary Node summaries. The exact same
        // value is written to the canonical blob and its search projection.
        let summary = if index % 12 == 7 {
            "!!!".to_owned()
        } else if index % 5 == 0 {
            format!("needle common fixture document {index}")
        } else {
            format!("common fixture document {index}")
        };
        let node = Node::new(
            id,
            summary.clone(),
            BodyRef::new(format!("inline://fixture/{index}")),
            ["fixture"],
            Provenance::Derived { from: Vec::new() },
            0.8,
            0.9,
            status,
            1_700_000_000_000 + index as u128,
        );

        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::from(id.0.to_string()));
        params.insert(
            "data".into(),
            DataValue::from(serde_json::to_string(&node).unwrap()),
        );
        params.insert("summary".into(), DataValue::from(summary));
        params.insert("status".into(), DataValue::from(status_name(status)));
        params.insert("tag".into(), DataValue::from("fixture"));
        params.insert("raw".into(), DataValue::from(vector(index)));
        run(
            &db,
            "{?[id, data, status] <- [[$id, $data, $status]] :put node {id => data, status}}\n\
             {?[id, tag] <- [[$id, $tag]] :put node_tag {id, tag}}\n\
             {?[id, summary, status] <- [[$id, $summary, $status]] :put node_search {id => summary, status}}\n\
             {?[id, e, status] <- [[$id, vec($raw), $status]] :put node_vec {id => e, status}}",
            params,
        );
    }

    // Build every derived index over pre-existing rows with the real 0.8.6 code.
    // In particular this selects its old bulk HNSW path, not steady-state inserts.
    db.run_default(
        "{::fts create node_search:active_fts {extractor: summary, extract_filter: status == 'active', tokenizer: Simple, filters: [Lowercase]}}\n\
         {::fts create node_search:candidate_fts {extractor: summary, extract_filter: status == 'candidate', tokenizer: Simple, filters: [Lowercase]}}\n\
         {::fts create node_search:archived_fts {extractor: summary, extract_filter: status == 'archived', tokenizer: Simple, filters: [Lowercase]}}\n\
         {::hnsw create node_vec:active_idx {dim: 4, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'active'}}\n\
         {::hnsw create node_vec:candidate_idx {dim: 4, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'candidate'}}\n\
         {::hnsw create node_vec:archived_idx {dim: 4, m: 16, ef_construction: 200, fields: [e], distance: Cosine, filter: status == 'archived'}}",
    )
    .unwrap();

    let mut meta: BTreeMap<String, String> = BTreeMap::new();
    meta.insert("db_id".into(), "0000000000000000000000F13A".into());
    meta.insert("dim".into(), "4".into());
    meta.insert(
        "vector_projection_v2".into(),
        "status_partitioned_hnsw_v2".into(),
    );
    meta.insert("lexical_projection_v1".into(), "complete".into());
    meta.insert("max_incident_edges_v1".into(), "256".into());
    meta.insert("max_remote_edges_per_source_v1".into(), "256".into());
    meta.insert(
        "embedding_fingerprint_v1".into(),
        serde_json::to_string(&EmbeddingFingerprint::new(
            "mneme:hashing-fnv1a64-token-count-lower-alnum-v1",
            DIM,
            "l2-f32-v1",
            "symmetric-document-v1",
        ))
        .unwrap(),
    );
    for (index, (key, value)) in meta.into_iter().enumerate() {
        let mut params = BTreeMap::new();
        params.insert(format!("k{index}"), DataValue::from(key));
        params.insert(format!("v{index}"), DataValue::from(value));
        run(
            &db,
            &format!("?[k, v] <- [[$k{index}, $v{index}]] :put meta {{k => v}}"),
            params,
        );
    }

    let old_score = db
        .run_default(
            "?[id, score] := ~node_search:active_fts{id | query: 'needle', k: 1, bind_score: score} :order -score, id",
        )
        .unwrap();
    println!("old_0_8_6_bm25_top={:?}", old_score.rows);

    drop(db);
    let bytes = fs::metadata(output).unwrap().len();
    println!(
        "fixture={} bytes={bytes} dim={DIM} nodes=96",
        output.display()
    );
}
