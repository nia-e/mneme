//! Offline, frozen-fixture geometry diagnostic; no stores, training, or providers.
use mneme_core::ports::Embedder;
use mneme_embed::FastEmbedder;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Deserialize)]
struct Context {
    id: String,
    text: String,
}
#[derive(Deserialize)]
struct Pair {
    a: String,
    b: String,
    relation: String,
    rationale: String,
}
#[derive(Deserialize)]
struct Fixture {
    contexts: Vec<Context>,
    gold_relations: Vec<Pair>,
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| f64::from(x) * f64::from(y))
        .sum();
    let norm = |v: &[f32]| v.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>().sqrt();
    (dot / (norm(a) * norm(b))).clamp(-1.0, 1.0)
}
fn threshold(pairs: &[Value], include_unknown: bool) -> Value {
    let positive: Vec<_> = pairs
        .iter()
        .filter(|p| p["relation"] == "same_applicability")
        .collect();
    let negative: Vec<_> = pairs
        .iter()
        .filter(|p| {
            p["relation"] == "contrasting_conditions"
                || (include_unknown && p["relation"] == "unknown_vs_known")
        })
        .collect();
    let score = |p: &&Value| p["cosine"].as_f64().unwrap();
    let weakest = positive
        .iter()
        .min_by(|a, b| score(a).total_cmp(&score(b)))
        .unwrap();
    let strongest = negative
        .iter()
        .max_by(|a, b| score(a).total_cmp(&score(b)))
        .unwrap();
    let lo = score(weakest);
    let hi = score(strongest);
    json!({"accept_rule": "cosine >= threshold", "positive_count": positive.len(),
        "excluded_pair_count": negative.len(), "single_threshold_exists": lo > hi,
        "min_positive_cosine": lo, "max_excluded_cosine": hi, "separation_margin": lo-hi,
        "weakest_positive": weakest, "strongest_excluded": strongest,
        "excluded_accepted_at_min_positive": negative.iter().filter(|p| score(p) >= lo).count(),
        "positives_rejected_strictly_above_max_excluded": positive.iter().filter(|p| score(p) <= hi).count()})
}
fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: contextual_geometry_v1 FIXTURE FREEZE_JSON OUTPUT_JSON".into());
    }
    let bytes = fs::read(&args[1])?;
    let freeze: Value = serde_json::from_slice(&fs::read(&args[2])?)?;
    if freeze["fixture_sha256"] != sha(&bytes) {
        return Err("fixture differs from frozen bytes".into());
    }
    let fixture: Fixture = serde_json::from_slice(&bytes)?;
    let ids: BTreeMap<_, _> = fixture
        .contexts
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.clone(), i))
        .collect();
    assert_eq!(ids.len(), fixture.contexts.len(), "duplicate context ID");
    assert!((24..=36).contains(&ids.len()));
    for p in &fixture.gold_relations {
        assert!(ids.contains_key(&p.a) && ids.contains_key(&p.b));
    }
    // Refuse a missing/incomplete frozen cache before FastEmbedder can download anything.
    assert!(
        std::env::var_os("HF_HOME").is_none() && std::env::var_os("FASTEMBED_CACHE_DIR").is_none()
    );
    for asset in freeze["model_assets"]
        .as_array()
        .ok_or("missing cache manifest")?
    {
        let path = asset["path"].as_str().ok_or("missing asset path")?;
        if sha(&fs::read(path)?) != asset["sha256"] {
            return Err(format!("cache changed: {path}").into());
        }
    }
    let started = epoch_ms();
    let load_start = Instant::now();
    let embedder = FastEmbedder::new()?;
    let load_ms = load_start.elapsed().as_secs_f64() * 1000.0;
    let inference_started = epoch_ms();
    let inference = Instant::now();
    let mut vectors = Vec::new();
    let mut latencies = Vec::new();
    for c in &fixture.contexts {
        let start = Instant::now();
        // Only natural context text reaches the encoder; all sides use the real query adapter.
        let v = embedder.embed_query_sync(&c.text)?;
        assert_eq!(v.len(), embedder.dim());
        assert!(v.iter().all(|x| x.is_finite()));
        vectors.push(v);
        latencies.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let inference_ms = inference.elapsed().as_secs_f64() * 1000.0;
    let inference_finished = epoch_ms();
    let matrix: Vec<Vec<_>> = vectors
        .iter()
        .map(|a| vectors.iter().map(|b| cosine(a, b)).collect())
        .collect();
    let angular: Vec<Vec<_>> = matrix
        .iter()
        .map(|row| row.iter().map(|c| c.acos()).collect())
        .collect();
    let mut pairs: Vec<Value> = fixture
        .gold_relations
        .iter()
        .map(|p| {
            let score = matrix[ids[&p.a]][ids[&p.b]];
            json!({"a":p.a,"b":p.b,"relation":p.relation,"rationale":p.rationale,
            "cosine":score,"angular_radians":score.acos()})
        })
        .collect();
    pairs.sort_by(|a, b| {
        b["cosine"]
            .as_f64()
            .unwrap()
            .total_cmp(&a["cosine"].as_f64().unwrap())
    });
    let mut rankings = Vec::new();
    let mut misses = Vec::new();
    for (i, c) in fixture.contexts.iter().enumerate() {
        let mut neighbors: Vec<_> = (0..vectors.len()).filter(|&j| j != i).collect();
        neighbors.sort_by(|&a, &b| matrix[i][b].total_cmp(&matrix[i][a]));
        let ranked: Vec<_> = neighbors
            .iter()
            .enumerate()
            .map(|(rank, &j)| {
                let other = &fixture.contexts[j].id;
                let gold: Vec<_> = fixture
                    .gold_relations
                    .iter()
                    .filter(|p| {
                        (&p.a == &c.id && &p.b == other) || (&p.b == &c.id && &p.a == other)
                    })
                    .map(|p| &p.relation)
                    .collect();
                json!({"rank":rank+1,"id":other,"cosine":matrix[i][j],"gold_relations":gold})
            })
            .collect();
        let positive: Vec<_> = ranked
            .iter()
            .filter(|r| {
                r["gold_relations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v == "same_applicability")
            })
            .collect();
        let contrasts: Vec<_> = ranked
            .iter()
            .filter(|r| {
                r["gold_relations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v == "contrasting_conditions")
            })
            .collect();
        for p in &positive {
            for n in &contrasts {
                if n["cosine"].as_f64() >= p["cosine"].as_f64() {
                    misses.push(json!({"anchor":c.id,"positive":p,"contrast":n}));
                }
            }
        }
        rankings.push(json!({"anchor":c.id,"neighbors":ranked}));
    }
    let vector_bytes: Vec<_> = vectors
        .iter()
        .flatten()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let vector_path = Path::new(&args[3]).with_extension("vectors.f32le");
    fs::write(&vector_path, &vector_bytes)?;
    let norms: Vec<_> = vectors
        .iter()
        .map(|v| v.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>().sqrt())
        .collect();
    let result = json!({"schema":"contextual-geometry-result-v1","fixture_sha256":sha(&bytes),
        "freeze":freeze,"model_fingerprint":embedder.fingerprint(),"dimension":embedder.dim(),
        "mode":"query-query; embed_query_sync for every input; no document-mode vectors",
        "query_instruction":"Represent this sentence for searching relevant passages: ",
        "context_order":fixture.contexts.iter().map(|c| &c.id).collect::<Vec<_>>(),
        "text_sha256":fixture.contexts.iter().map(|c| json!({"id":c.id,"sha256":sha(c.text.as_bytes())})).collect::<Vec<_>>(),
        "vectors":{"path":vector_path,"format":"row-major IEEE754 f32 little endian","sha256":sha(&vector_bytes),"norms":norms},
        "timing":{"load_start_epoch_ms":started,"load_ms":load_ms,
            "inference_start_epoch_ms":inference_started,"inference_finish_epoch_ms":inference_finished,
            "inference_ms":inference_ms,"per_context_ms":latencies},
        "primary_threshold":threshold(&pairs,false),"unknown_equivalence_threshold":threshold(&pairs,true),
        "declared_pair_ranking":pairs,"anchor_rankings":rankings,"contrast_outranks_positive":misses,
        "cosine_matrix":matrix,"angular_radians_matrix":angular,
        "limitations":"Synthetic diagnostic only; hand-picked labels; no accuracy/utility claim; unknown is not negative evidence; thresholds are inspected, never tuned."});
    fs::write(&args[3], serde_json::to_vec_pretty(&result)?)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"output":args[3],"contexts":vectors.len(),
        "primary_threshold":result["primary_threshold"],"timing":result["timing"]}))?
    );
    Ok(())
}
