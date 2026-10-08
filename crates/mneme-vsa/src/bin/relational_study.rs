use std::env;
use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mneme_vsa::{Fhrr, Fingerprint, Vector};
use serde::Serialize;

const REPORT_SCHEMA_VERSION: u16 = 1;
const STUDY_KIND: &str = "mneme-vsa-node-centric-typed-relation-study";
const TOP_K: usize = 5;
const EXACT_TIMING_REPETITIONS: usize = 4_096;
const CANONICAL_WARNING: &str = "Exact typed adjacency remains canonical. FHRR holograms are rebuildable derived indexes and must not be the only copy of a relation graph.";
const ACCOUNTING_ASSUMPTION: &str = "Payload accounting includes every node hologram plus one globally shared role/filler cleanup codebook. It excludes labels, Vec/Arc metadata, allocator overhead, and executable code for both representations; claiming hologram bytes alone would be benchmark cosplay.";
const EXACT_ENCODING: &str = "Compact exact baseline: one flat u16 target index per (node, implicit role-slot), serialized little-endian, so payload is nodes * degree * 2 bytes. Node counts above 65,536 are rejected. Exact build/query timings use a flat Vec<u16> with the same entry width and layout.";
const GRAPH_PROTOCOL: &str = "Every node bundles degree globally shared typed roles. For source s and role r, the unique target is (s + r + 1) mod node_count. Queries are deterministic samples with replacement.";
const TWO_HOP_PROTOCOL: &str = "Sequential two-hop retrieval cleans the first unbinding to top-1, selects that predicted node's hologram, then unbinds and cleans the second role. A distractor first hit is not routable. End-to-end success requires both the exact intermediate and final target, so first-hop errors compound rather than receiving an oracle repair.";
const RUNTIME_SCOPE: &str = "Wall-clock timings are totals for each configuration, not calibrated microbenchmarks. FHRR query time includes unbinding and exhaustive cosine cleanup; two-hop work stops when the first hit is a distractor. Exact lookup is direct flat-array indexing; its deterministic sample bank is repeated 4,096 times behind optimization barriers so the sub-microsecond operation is measurable.";
const DELETION_PROTOCOL: &str = "Not measured: the current prototype intentionally exposes no subtraction API. In-place deletion would also need exact bound-term provenance and careful normalization; rebuilding derived holograms from canonical adjacency is the safe protocol.";
const LIMITATIONS: [&str; 4] = [
    "This is one deterministic seed on a regular synthetic graph, with no confidence intervals; it is a reproducible capacity probe, not a general quality estimate.",
    "FHRR cleanup is an exhaustive O((nodes + distractors) * dimension) scan. An approximate index could trade additional memory and build complexity for query speed and potentially lower recall.",
    "Byte counts are coordinate/adjacency payloads, not measured resident memory, serialized file size, or prompt/context tokens.",
    "The exact baseline assumes fixed degree and globally known role slots. Variable-degree or open-schema relations would require offsets and/or explicit role identifiers.",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Config {
    dimensions: Vec<usize>,
    node_counts: Vec<usize>,
    degrees: Vec<usize>,
    distractor_counts: Vec<usize>,
    one_hop_queries: usize,
    two_hop_queries: usize,
    seed: u64,
    machine_label: String,
    format: OutputFormat,
    output: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dimensions: vec![128, 256, 512],
            node_counts: vec![64, 256],
            degrees: vec![2, 8, 16],
            distractor_counts: vec![0, 64],
            one_hop_queries: 128,
            two_hop_queries: 128,
            seed: 0x4d4e_454d_4552_454c,
            machine_label: "unspecified".into(),
            format: OutputFormat::Human,
            output: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct StudyReport {
    schema_version: u16,
    kind: &'static str,
    canonical_warning: &'static str,
    accounting_assumption: &'static str,
    exact_adjacency_encoding: &'static str,
    graph_protocol: &'static str,
    sequential_two_hop_protocol: &'static str,
    runtime_scope: &'static str,
    deletion_protocol: &'static str,
    limitations: &'static [&'static str],
    machine_label: String,
    seed: u64,
    requested_top_k: usize,
    exact_timing_repetitions: usize,
    one_hop_queries_per_configuration: usize,
    two_hop_queries_per_configuration: usize,
    results: Vec<StudyResult>,
}

#[derive(Debug, Serialize)]
struct StudyResult {
    fingerprint: Fingerprint,
    node_count: usize,
    degree: usize,
    cleanup_distractors: usize,
    cleanup_dictionary_vectors: usize,
    role_codebook_vectors: usize,
    filler_codebook_vectors: usize,
    total_codebook_vectors: usize,
    hologram_vectors: usize,
    one_hop_queries: usize,
    one_hop_top1_correct: usize,
    one_hop_top5_correct: usize,
    one_hop_top1_recall: f64,
    one_hop_top5_recall: f64,
    two_hop_queries: usize,
    two_hop_routable_intermediates: usize,
    two_hop_first_top1_correct: usize,
    two_hop_second_top1_given_correct_first: usize,
    two_hop_second_top5_given_correct_first: usize,
    two_hop_end_to_end_top1_correct: usize,
    two_hop_end_to_end_top5_correct: usize,
    two_hop_first_top1_recall: f64,
    two_hop_conditional_second_top1_recall: f64,
    two_hop_conditional_second_top5_recall: f64,
    two_hop_end_to_end_top1_recall: f64,
    two_hop_end_to_end_top5_recall: f64,
    two_hop_first_stage_error_rate: f64,
    two_hop_additional_error_after_correct_first_rate: f64,
    two_hop_end_to_end_error_rate: f64,
    exact_one_hop_correct: usize,
    exact_two_hop_correct: usize,
    vector_payload_bytes: usize,
    hologram_payload_bytes: usize,
    role_codebook_payload_bytes: usize,
    filler_codebook_payload_bytes: usize,
    total_codebook_payload_bytes: usize,
    codebook_payload_bytes_per_node: f64,
    derived_lookup_payload_bytes: usize,
    derived_lookup_payload_bytes_per_node: f64,
    exact_adjacency_payload_bytes: usize,
    exact_adjacency_payload_bytes_per_node: usize,
    derived_to_exact_payload_ratio: f64,
    exact_adjacency_build_runtime_ns: u64,
    fhrr_codebook_build_runtime_ns: u64,
    fhrr_hologram_build_runtime_ns: u64,
    exact_one_hop_query_runtime_ns: u64,
    exact_two_hop_query_runtime_ns: u64,
    exact_timed_one_hop_queries: usize,
    exact_timed_two_hop_queries: usize,
    fhrr_one_hop_query_runtime_ns: u64,
    fhrr_two_hop_query_runtime_ns: u64,
    exact_one_hop_runtime_ns_per_query: f64,
    exact_two_hop_runtime_ns_per_query: f64,
    fhrr_one_hop_runtime_ns_per_query: f64,
    fhrr_two_hop_runtime_ns_per_query: f64,
}

#[derive(Clone, Copy, Debug)]
struct OneHopQuery {
    source: usize,
    role: usize,
    target: usize,
}

#[derive(Clone, Copy, Debug)]
struct TwoHopQuery {
    source: usize,
    first_role: usize,
    intermediate: usize,
    second_role: usize,
    target: usize,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("relational-study: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let Some(config) = parse_args(env::args().skip(1))? else {
        print_help();
        return Ok(());
    };
    let report = run_study(&config)?;
    match config.format {
        OutputFormat::Human => print_human(&report),
        OutputFormat::Json => {
            print_machine_summary(&report);
            let mut bytes = serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?;
            bytes.push(b'\n');
            emit_machine_output(config.output.as_deref(), &bytes)?;
        }
    }
    Ok(())
}

fn parse_args<I>(args: I) -> Result<Option<Config>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut config = Config::default();
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = |args: &mut I::IntoIter| {
            args.next()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "--dimensions" => {
                config.dimensions = parse_list(&value(&mut args)?, "dimensions", false)?;
            }
            "--nodes" => {
                config.node_counts = parse_list(&value(&mut args)?, "nodes", false)?;
            }
            "--degrees" => {
                config.degrees = parse_list(&value(&mut args)?, "degrees", false)?;
            }
            "--distractors" => {
                config.distractor_counts = parse_list(&value(&mut args)?, "distractors", true)?;
            }
            "--one-hop-queries" => {
                config.one_hop_queries = parse_positive(&value(&mut args)?, "one-hop-queries")?;
            }
            "--two-hop-queries" => {
                config.two_hop_queries = parse_positive(&value(&mut args)?, "two-hop-queries")?;
            }
            "--seed" => {
                config.seed = value(&mut args)?
                    .parse()
                    .map_err(|_| "seed must be an unsigned decimal integer".to_owned())?;
            }
            "--machine-label" => config.machine_label = value(&mut args)?,
            "--format" => {
                config.format = match value(&mut args)?.as_str() {
                    "human" => OutputFormat::Human,
                    "json" => OutputFormat::Json,
                    other => {
                        return Err(format!("unknown format {other:?}; expected human or json"));
                    }
                };
            }
            "--output" => config.output = Some(PathBuf::from(value(&mut args)?)),
            other => return Err(format!("unknown argument {other:?}; use --help")),
        }
    }
    if config.output.is_some() && config.format != OutputFormat::Json {
        return Err("--output requires --format json".into());
    }
    validate_config(&config)?;
    Ok(Some(config))
}

fn validate_config(config: &Config) -> Result<(), String> {
    for &nodes in &config.node_counts {
        if nodes > usize::from(u16::MAX) + 1 {
            return Err(format!(
                "nodes value {nodes} exceeds the u16 exact-adjacency limit of 65536"
            ));
        }
        for &degree in &config.degrees {
            if degree >= nodes {
                return Err(format!(
                    "degree {degree} must be less than node count {nodes} so each node has unique non-self targets"
                ));
            }
        }
    }
    Ok(())
}

fn parse_list(input: &str, label: &str, allow_zero: bool) -> Result<Vec<usize>, String> {
    let mut values = input
        .split(',')
        .map(|part| {
            let value: usize = part
                .parse()
                .map_err(|_| format!("{label} must be a comma-separated integer list"))?;
            if !allow_zero && value == 0 {
                return Err(format!("{label} values must be greater than zero"));
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>, String>>()?;
    if values.is_empty() {
        return Err(format!("{label} may not be empty"));
    }
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

fn parse_positive(input: &str, label: &str) -> Result<usize, String> {
    let value = input
        .parse()
        .map_err(|_| format!("{label} must be a positive integer"))?;
    if value == 0 {
        Err(format!("{label} must be greater than zero"))
    } else {
        Ok(value)
    }
}

fn run_study(config: &Config) -> Result<StudyReport, String> {
    validate_config(config)?;
    let configurations = config
        .dimensions
        .len()
        .checked_mul(config.node_counts.len())
        .and_then(|n| n.checked_mul(config.degrees.len()))
        .and_then(|n| n.checked_mul(config.distractor_counts.len()))
        .ok_or_else(|| "configuration count overflow".to_owned())?;
    let mut results = Vec::with_capacity(configurations);
    for &dimension in &config.dimensions {
        for &nodes in &config.node_counts {
            for &degree in &config.degrees {
                for &distractors in &config.distractor_counts {
                    results.push(study_configuration(
                        dimension,
                        nodes,
                        degree,
                        distractors,
                        config.one_hop_queries,
                        config.two_hop_queries,
                        config.seed,
                    )?);
                }
            }
        }
    }
    Ok(StudyReport {
        schema_version: REPORT_SCHEMA_VERSION,
        kind: STUDY_KIND,
        canonical_warning: CANONICAL_WARNING,
        accounting_assumption: ACCOUNTING_ASSUMPTION,
        exact_adjacency_encoding: EXACT_ENCODING,
        graph_protocol: GRAPH_PROTOCOL,
        sequential_two_hop_protocol: TWO_HOP_PROTOCOL,
        runtime_scope: RUNTIME_SCOPE,
        deletion_protocol: DELETION_PROTOCOL,
        limitations: &LIMITATIONS,
        machine_label: config.machine_label.clone(),
        seed: config.seed,
        requested_top_k: TOP_K,
        exact_timing_repetitions: EXACT_TIMING_REPETITIONS,
        one_hop_queries_per_configuration: config.one_hop_queries,
        two_hop_queries_per_configuration: config.two_hop_queries,
        results,
    })
}

#[allow(clippy::too_many_arguments)]
fn study_configuration(
    dimension: usize,
    node_count: usize,
    degree: usize,
    distractors: usize,
    one_hop_query_count: usize,
    two_hop_query_count: usize,
    seed: u64,
) -> Result<StudyResult, String> {
    if node_count == 0 || degree == 0 || degree >= node_count {
        return Err("node count must be positive and degree must be in 1..node_count".into());
    }
    if node_count > usize::from(u16::MAX) + 1 {
        return Err("node count exceeds the u16 exact-adjacency limit".into());
    }

    let exact_build_start = Instant::now();
    let adjacency = build_exact_adjacency(node_count, degree)?;
    let exact_adjacency_build_runtime = exact_build_start.elapsed();

    let one_hop_queries =
        make_one_hop_queries(&adjacency, node_count, degree, one_hop_query_count, seed);
    let two_hop_queries =
        make_two_hop_queries(&adjacency, node_count, degree, two_hop_query_count, seed);

    let mut exact_one_hop_correct = 0_usize;
    for query in &one_hop_queries {
        let target = exact_target(&adjacency, degree, query.source, query.role);
        exact_one_hop_correct += usize::from(target == query.target);
    }

    let mut exact_two_hop_correct = 0_usize;
    for query in &two_hop_queries {
        let intermediate = exact_target(&adjacency, degree, query.source, query.first_role);
        let target = exact_target(&adjacency, degree, intermediate, query.second_role);
        exact_two_hop_correct +=
            usize::from(intermediate == query.intermediate && target == query.target);
    }

    let exact_one_start = Instant::now();
    let mut exact_checksum = 0_usize;
    for _ in 0..EXACT_TIMING_REPETITIONS {
        for query in black_box(&one_hop_queries) {
            let target = exact_target(
                black_box(&adjacency),
                degree,
                black_box(query.source),
                black_box(query.role),
            );
            exact_checksum = exact_checksum.wrapping_add(target);
        }
    }
    black_box(exact_checksum);
    let exact_one_hop_runtime = exact_one_start.elapsed();

    let exact_two_start = Instant::now();
    let mut exact_checksum = 0_usize;
    for _ in 0..EXACT_TIMING_REPETITIONS {
        for query in black_box(&two_hop_queries) {
            let intermediate = exact_target(
                black_box(&adjacency),
                degree,
                black_box(query.source),
                black_box(query.first_role),
            );
            let target = exact_target(
                black_box(&adjacency),
                degree,
                black_box(intermediate),
                black_box(query.second_role),
            );
            exact_checksum = exact_checksum
                .wrapping_add(intermediate)
                .wrapping_add(target);
        }
    }
    black_box(exact_checksum);
    let exact_two_hop_runtime = exact_two_start.elapsed();

    let codebook_start = Instant::now();
    let fhrr = Fhrr::new(dimension, seed).map_err(|e| e.to_string())?;
    let role_labels: Vec<_> = (0..degree).map(|index| format!("role:{index}")).collect();
    let node_labels: Vec<_> = (0..node_count)
        .map(|index| format!("node:{index}"))
        .collect();
    let distractor_labels: Vec<_> = (0..distractors)
        .map(|index| format!("distractor:{index}"))
        .collect();
    let role_vectors: Vec<_> = role_labels.iter().map(|label| fhrr.atom(label)).collect();
    let node_vectors: Vec<_> = node_labels.iter().map(|label| fhrr.atom(label)).collect();
    let distractor_vectors: Vec<_> = distractor_labels
        .iter()
        .map(|label| fhrr.atom(label))
        .collect();
    let fhrr_codebook_build_runtime = codebook_start.elapsed();

    let hologram_start = Instant::now();
    let mut holograms = Vec::with_capacity(node_count);
    for source in 0..node_count {
        let bound: Vec<_> = (0..degree)
            .map(|role| {
                let target = exact_target(&adjacency, degree, source, role);
                fhrr.bind(&role_vectors[role], &node_vectors[target])
            })
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        holograms.push(fhrr.superpose(&bound).map_err(|e| e.to_string())?);
    }
    let fhrr_hologram_build_runtime = hologram_start.elapsed();

    let effective_top_k = TOP_K.min(node_count.saturating_add(distractors));
    let fhrr_one_start = Instant::now();
    let mut one_hop_top1_correct = 0_usize;
    let mut one_hop_top5_correct = 0_usize;
    for query in &one_hop_queries {
        let estimate = fhrr
            .unbind(&holograms[query.source], &role_vectors[query.role])
            .map_err(|e| e.to_string())?;
        let hits = fhrr
            .cleanup(
                &estimate,
                dictionary(
                    &node_labels,
                    &node_vectors,
                    &distractor_labels,
                    &distractor_vectors,
                ),
                effective_top_k,
            )
            .map_err(|e| e.to_string())?;
        let correct = node_labels[query.target].as_str();
        one_hop_top1_correct += usize::from(hits.first().is_some_and(|hit| hit.label == correct));
        one_hop_top5_correct += usize::from(hits.iter().any(|hit| hit.label == correct));
    }
    let fhrr_one_hop_runtime = fhrr_one_start.elapsed();

    let fhrr_two_start = Instant::now();
    let mut two_hop_routable_intermediates = 0_usize;
    let mut two_hop_first_top1_correct = 0_usize;
    let mut two_hop_second_top1_given_correct_first = 0_usize;
    let mut two_hop_second_top5_given_correct_first = 0_usize;
    for query in &two_hop_queries {
        let first_estimate = fhrr
            .unbind(&holograms[query.source], &role_vectors[query.first_role])
            .map_err(|e| e.to_string())?;
        let first_hits = fhrr
            .cleanup(
                &first_estimate,
                dictionary(
                    &node_labels,
                    &node_vectors,
                    &distractor_labels,
                    &distractor_vectors,
                ),
                effective_top_k,
            )
            .map_err(|e| e.to_string())?;
        let Some(first_label) = first_hits.first().map(|hit| hit.label) else {
            continue;
        };
        let first_correct = first_label == node_labels[query.intermediate];
        two_hop_first_top1_correct += usize::from(first_correct);

        let Some(predicted_intermediate) = node_index(first_label, node_count) else {
            continue;
        };
        two_hop_routable_intermediates += 1;
        let second_estimate = fhrr
            .unbind(
                &holograms[predicted_intermediate],
                &role_vectors[query.second_role],
            )
            .map_err(|e| e.to_string())?;
        let second_hits = fhrr
            .cleanup(
                &second_estimate,
                dictionary(
                    &node_labels,
                    &node_vectors,
                    &distractor_labels,
                    &distractor_vectors,
                ),
                effective_top_k,
            )
            .map_err(|e| e.to_string())?;
        if first_correct {
            let correct = node_labels[query.target].as_str();
            two_hop_second_top1_given_correct_first +=
                usize::from(second_hits.first().is_some_and(|hit| hit.label == correct));
            two_hop_second_top5_given_correct_first +=
                usize::from(second_hits.iter().any(|hit| hit.label == correct));
        }
    }
    let fhrr_two_hop_runtime = fhrr_two_start.elapsed();

    let vector_payload_bytes = fhrr.payload_bytes();
    let role_codebook_vectors = degree;
    let filler_codebook_vectors = checked_add(node_count, distractors, "filler codebook vectors")?;
    let total_codebook_vectors = checked_add(
        role_codebook_vectors,
        filler_codebook_vectors,
        "total codebook vectors",
    )?;
    let hologram_payload_bytes =
        checked_mul(vector_payload_bytes, node_count, "hologram payload bytes")?;
    let role_codebook_payload_bytes = checked_mul(
        vector_payload_bytes,
        role_codebook_vectors,
        "role codebook payload bytes",
    )?;
    let filler_codebook_payload_bytes = checked_mul(
        vector_payload_bytes,
        filler_codebook_vectors,
        "filler codebook payload bytes",
    )?;
    let total_codebook_payload_bytes = checked_mul(
        vector_payload_bytes,
        total_codebook_vectors,
        "total codebook payload bytes",
    )?;
    let derived_lookup_payload_bytes = checked_add(
        hologram_payload_bytes,
        total_codebook_payload_bytes,
        "derived lookup payload bytes",
    )?;
    let exact_adjacency_payload_bytes = checked_mul(
        checked_mul(node_count, degree, "exact adjacency entries")?,
        std::mem::size_of::<u16>(),
        "exact adjacency payload bytes",
    )?;

    let one_denominator = one_hop_query_count as f64;
    let two_denominator = two_hop_query_count as f64;
    let correct_first_denominator = two_hop_first_top1_correct as f64;
    let first_recall = two_hop_first_top1_correct as f64 / two_denominator;
    let conditional_second_top1 = ratio_or_zero(
        two_hop_second_top1_given_correct_first,
        two_hop_first_top1_correct,
    );
    let exact_timed_one_hop_queries = checked_mul(
        one_hop_query_count,
        EXACT_TIMING_REPETITIONS,
        "exact timed one-hop queries",
    )?;
    let exact_timed_two_hop_queries = checked_mul(
        two_hop_query_count,
        EXACT_TIMING_REPETITIONS,
        "exact timed two-hop queries",
    )?;

    Ok(StudyResult {
        fingerprint: fhrr.fingerprint().clone(),
        node_count,
        degree,
        cleanup_distractors: distractors,
        cleanup_dictionary_vectors: filler_codebook_vectors,
        role_codebook_vectors,
        filler_codebook_vectors,
        total_codebook_vectors,
        hologram_vectors: node_count,
        one_hop_queries: one_hop_query_count,
        one_hop_top1_correct,
        one_hop_top5_correct,
        one_hop_top1_recall: one_hop_top1_correct as f64 / one_denominator,
        one_hop_top5_recall: one_hop_top5_correct as f64 / one_denominator,
        two_hop_queries: two_hop_query_count,
        two_hop_routable_intermediates,
        two_hop_first_top1_correct,
        two_hop_second_top1_given_correct_first,
        two_hop_second_top5_given_correct_first,
        two_hop_end_to_end_top1_correct: two_hop_second_top1_given_correct_first,
        two_hop_end_to_end_top5_correct: two_hop_second_top5_given_correct_first,
        two_hop_first_top1_recall: first_recall,
        two_hop_conditional_second_top1_recall: conditional_second_top1,
        two_hop_conditional_second_top5_recall: if correct_first_denominator == 0.0 {
            0.0
        } else {
            two_hop_second_top5_given_correct_first as f64 / correct_first_denominator
        },
        two_hop_end_to_end_top1_recall: two_hop_second_top1_given_correct_first as f64
            / two_denominator,
        two_hop_end_to_end_top5_recall: two_hop_second_top5_given_correct_first as f64
            / two_denominator,
        two_hop_first_stage_error_rate: 1.0 - first_recall,
        two_hop_additional_error_after_correct_first_rate: first_recall
            * (1.0 - conditional_second_top1),
        two_hop_end_to_end_error_rate: 1.0
            - (two_hop_second_top1_given_correct_first as f64 / two_denominator),
        exact_one_hop_correct,
        exact_two_hop_correct,
        vector_payload_bytes,
        hologram_payload_bytes,
        role_codebook_payload_bytes,
        filler_codebook_payload_bytes,
        total_codebook_payload_bytes,
        codebook_payload_bytes_per_node: total_codebook_payload_bytes as f64 / node_count as f64,
        derived_lookup_payload_bytes,
        derived_lookup_payload_bytes_per_node: derived_lookup_payload_bytes as f64
            / node_count as f64,
        exact_adjacency_payload_bytes,
        exact_adjacency_payload_bytes_per_node: degree * std::mem::size_of::<u16>(),
        derived_to_exact_payload_ratio: derived_lookup_payload_bytes as f64
            / exact_adjacency_payload_bytes as f64,
        exact_adjacency_build_runtime_ns: nanos_u64(exact_adjacency_build_runtime),
        fhrr_codebook_build_runtime_ns: nanos_u64(fhrr_codebook_build_runtime),
        fhrr_hologram_build_runtime_ns: nanos_u64(fhrr_hologram_build_runtime),
        exact_one_hop_query_runtime_ns: nanos_u64(exact_one_hop_runtime),
        exact_two_hop_query_runtime_ns: nanos_u64(exact_two_hop_runtime),
        exact_timed_one_hop_queries,
        exact_timed_two_hop_queries,
        fhrr_one_hop_query_runtime_ns: nanos_u64(fhrr_one_hop_runtime),
        fhrr_two_hop_query_runtime_ns: nanos_u64(fhrr_two_hop_runtime),
        exact_one_hop_runtime_ns_per_query: exact_one_hop_runtime.as_nanos() as f64
            / exact_timed_one_hop_queries as f64,
        exact_two_hop_runtime_ns_per_query: exact_two_hop_runtime.as_nanos() as f64
            / exact_timed_two_hop_queries as f64,
        fhrr_one_hop_runtime_ns_per_query: fhrr_one_hop_runtime.as_nanos() as f64 / one_denominator,
        fhrr_two_hop_runtime_ns_per_query: fhrr_two_hop_runtime.as_nanos() as f64 / two_denominator,
    })
}

fn build_exact_adjacency(node_count: usize, degree: usize) -> Result<Vec<u16>, String> {
    let entries = checked_mul(node_count, degree, "exact adjacency entries")?;
    let mut adjacency = Vec::with_capacity(entries);
    for source in 0..node_count {
        for role in 0..degree {
            let target = (source + role + 1) % node_count;
            adjacency.push(
                u16::try_from(target)
                    .map_err(|_| "target does not fit the u16 exact encoding".to_owned())?,
            );
        }
    }
    Ok(adjacency)
}

fn exact_target(adjacency: &[u16], degree: usize, source: usize, role: usize) -> usize {
    usize::from(adjacency[source * degree + role])
}

fn make_one_hop_queries(
    adjacency: &[u16],
    node_count: usize,
    degree: usize,
    count: usize,
    seed: u64,
) -> Vec<OneHopQuery> {
    (0..count)
        .map(|ordinal| {
            let source = sample_index(seed, ordinal, 0x4f4e_455f_534f_5552, node_count);
            let role = sample_index(seed, ordinal, 0x4f4e_455f_524f_4c45, degree);
            OneHopQuery {
                source,
                role,
                target: exact_target(adjacency, degree, source, role),
            }
        })
        .collect()
}

fn make_two_hop_queries(
    adjacency: &[u16],
    node_count: usize,
    degree: usize,
    count: usize,
    seed: u64,
) -> Vec<TwoHopQuery> {
    (0..count)
        .map(|ordinal| {
            let source = sample_index(seed, ordinal, 0x5457_4f5f_534f_5552, node_count);
            let first_role = sample_index(seed, ordinal, 0x5457_4f5f_524f_4c31, degree);
            let intermediate = exact_target(adjacency, degree, source, first_role);
            let second_role = sample_index(seed, ordinal, 0x5457_4f5f_524f_4c32, degree);
            let target = exact_target(adjacency, degree, intermediate, second_role);
            TwoHopQuery {
                source,
                first_role,
                intermediate,
                second_role,
                target,
            }
        })
        .collect()
}

fn sample_index(seed: u64, ordinal: usize, salt: u64, modulus: usize) -> usize {
    let ordinal = u64::try_from(ordinal).unwrap_or(u64::MAX);
    let mixed = mix64(seed ^ salt ^ ordinal.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    (mixed % modulus as u64) as usize
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn node_index(label: &str, node_count: usize) -> Option<usize> {
    let index = label.strip_prefix("node:")?.parse().ok()?;
    (index < node_count).then_some(index)
}

fn dictionary<'a>(
    node_labels: &'a [String],
    node_vectors: &'a [Vector],
    distractor_labels: &'a [String],
    distractor_vectors: &'a [Vector],
) -> impl Iterator<Item = (&'a str, &'a Vector)> {
    node_labels
        .iter()
        .zip(node_vectors)
        .map(|(label, vector)| (label.as_str(), vector))
        .chain(
            distractor_labels
                .iter()
                .zip(distractor_vectors)
                .map(|(label, vector)| (label.as_str(), vector)),
        )
}

fn checked_add(left: usize, right: usize, label: &str) -> Result<usize, String> {
    left.checked_add(right)
        .ok_or_else(|| format!("{label} overflow"))
}

fn checked_mul(left: usize, right: usize, label: &str) -> Result<usize, String> {
    left.checked_mul(right)
        .ok_or_else(|| format!("{label} overflow"))
}

fn ratio_or_zero(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn nanos_u64(duration: Duration) -> u64 {
    duration.as_nanos().try_into().unwrap_or(u64::MAX)
}

fn print_human(report: &StudyReport) {
    println!("FHRR node-centric typed-relation study");
    println!(
        "seed={} · machine={} · one-hop/config={} · two-hop/config={}",
        report.seed,
        report.machine_label,
        report.one_hop_queries_per_configuration,
        report.two_hop_queries_per_configuration
    );
    println!(
        "{:>5} {:>5} {:>4} {:>5} {:>7} {:>7} {:>7} {:>9} {:>9}",
        "dim", "nodes", "deg", "noise", "1h@1", "1h@5", "2h@1", "FHRR KiB", "exact KiB"
    );
    for row in &report.results {
        println!(
            "{:>5} {:>5} {:>4} {:>5} {:>7.3} {:>7.3} {:>7.3} {:>9.1} {:>9.1}",
            row.fingerprint.dimension,
            row.node_count,
            row.degree,
            row.cleanup_distractors,
            row.one_hop_top1_recall,
            row.one_hop_top5_recall,
            row.two_hop_end_to_end_top1_recall,
            row.derived_lookup_payload_bytes as f64 / 1024.0,
            row.exact_adjacency_payload_bytes as f64 / 1024.0,
        );
    }
    println!("\n{CANONICAL_WARNING}");
    println!("{ACCOUNTING_ASSUMPTION}");
    println!("{DELETION_PROTOCOL}");
}

fn print_machine_summary(report: &StudyReport) {
    eprintln!(
        "mneme-vsa relational study: {} configurations, {} one-hop and {} sequential two-hop queries; exact adjacency remains canonical",
        report.results.len(),
        report
            .results
            .iter()
            .map(|row| row.one_hop_queries)
            .sum::<usize>(),
        report
            .results
            .iter()
            .map(|row| row.two_hop_queries)
            .sum::<usize>(),
    );
}

fn emit_machine_output(path: Option<&Path>, bytes: &[u8]) -> Result<(), String> {
    match path {
        Some(path) => {
            atomic_write(path, bytes)?;
            eprintln!("wrote {}", path.display());
            Ok(())
        }
        None => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            out.write_all(bytes).map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())
        }
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let filename = path
        .file_name()
        .ok_or_else(|| format!("output path {:?} has no filename", path))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|e| format!("create output directory {}: {e}", parent.display()))?;

    let mut temp_path = None;
    let mut temp_file = None;
    for nonce in 0_u8..100 {
        let mut name = filename.to_os_string();
        name.push(format!(".tmp-{}-{nonce}", std::process::id()));
        let candidate = parent.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temp_path = Some(candidate);
                temp_file = Some(file);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "create temporary output beside {}: {error}",
                    path.display()
                ));
            }
        }
    }
    let temp_path = temp_path.ok_or_else(|| {
        format!(
            "could not reserve a temporary output beside {}",
            path.display()
        )
    })?;
    let mut temp_file = temp_file.expect("temporary path and file are reserved together");
    let result = (|| -> io::Result<()> {
        temp_file.write_all(bytes)?;
        temp_file.sync_all()?;
        drop(temp_file);
        fs::rename(&temp_path, path)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(format!("atomically write {}: {error}", path.display()));
    }
    Ok(())
}

fn print_help() {
    println!(
        "mneme-vsa node-centric typed-relation study\n\
         \n\
         Usage: relational-study [OPTIONS]\n\
         \n\
         Options:\n\
           --dimensions LIST      FHRR dimensions [default: 128,256,512]\n\
           --nodes LIST           Node counts [default: 64,256]\n\
           --degrees LIST         Typed outgoing roles per node [default: 2,8,16]\n\
           --distractors LIST     Extra cleanup candidates [default: 0,64]\n\
           --one-hop-queries N    Deterministic samples/config [default: 128]\n\
           --two-hop-queries N    Sequential samples/config [default: 128]\n\
           --seed N               Unsigned decimal atom/query seed\n\
           --machine-label TEXT   Descriptive benchmark host label\n\
           --format FORMAT        human or json [default: human]\n\
           --output PATH          Atomically retain JSON instead of stdout\n\
           -h, --help             Show this help\n\
         \n\
         JSON goes to stdout; its concise summary goes to stderr."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes_sweep_arguments() {
        let config = parse_args(
            [
                "--dimensions",
                "256,64,256",
                "--nodes",
                "32,16",
                "--degrees",
                "4,1",
                "--distractors",
                "16,0",
                "--one-hop-queries",
                "7",
                "--two-hop-queries",
                "9",
                "--seed",
                "42",
                "--machine-label",
                "test host",
                "--format",
                "json",
                "--output",
                "result.json",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.dimensions, [64, 256]);
        assert_eq!(config.node_counts, [16, 32]);
        assert_eq!(config.degrees, [1, 4]);
        assert_eq!(config.distractor_counts, [0, 16]);
        assert_eq!(config.one_hop_queries, 7);
        assert_eq!(config.two_hop_queries, 9);
        assert_eq!(config.seed, 42);
        assert_eq!(config.machine_label, "test host");
        assert_eq!(config.format, OutputFormat::Json);
        assert_eq!(config.output, Some(PathBuf::from("result.json")));
    }

    #[test]
    fn rejects_invalid_exact_graph_shapes_and_output() {
        for args in [
            vec!["--nodes", "8", "--degrees", "8"],
            vec!["--nodes", "65537"],
            vec!["--one-hop-queries", "0"],
            vec!["--two-hop-queries", "0"],
            vec!["--output", "result.json"],
        ] {
            assert!(
                parse_args(args.into_iter().map(str::to_owned)).is_err(),
                "accepted invalid arguments"
            );
        }
    }

    #[test]
    fn tiny_relational_study_reports_equal_costs_and_compounding() {
        let row = study_configuration(256, 16, 3, 4, 32, 32, 17).unwrap();
        let vector_bytes = 256 * 2 * 4;
        assert_eq!(row.exact_adjacency_payload_bytes, 16 * 3 * 2);
        assert_eq!(row.hologram_payload_bytes, 16 * vector_bytes);
        assert_eq!(row.role_codebook_payload_bytes, 3 * vector_bytes);
        assert_eq!(row.filler_codebook_payload_bytes, 20 * vector_bytes);
        assert_eq!(row.total_codebook_payload_bytes, 23 * vector_bytes);
        assert_eq!(row.derived_lookup_payload_bytes, 39 * vector_bytes);
        assert_eq!(row.exact_one_hop_correct, 32);
        assert_eq!(row.exact_two_hop_correct, 32);
        assert!(row.one_hop_top5_correct >= row.one_hop_top1_correct);
        assert!(row.two_hop_second_top1_given_correct_first <= row.two_hop_first_top1_correct);
        assert!(
            row.two_hop_second_top5_given_correct_first
                >= row.two_hop_second_top1_given_correct_first
        );
        assert!(row.two_hop_end_to_end_top1_recall <= row.two_hop_first_top1_recall);
    }

    #[test]
    fn accuracy_is_deterministic_even_though_timing_is_not() {
        let first = study_configuration(128, 12, 4, 7, 25, 25, 99).unwrap();
        let second = study_configuration(128, 12, 4, 7, 25, 25, 99).unwrap();
        assert_eq!(first.one_hop_top1_correct, second.one_hop_top1_correct);
        assert_eq!(first.one_hop_top5_correct, second.one_hop_top5_correct);
        assert_eq!(
            first.two_hop_first_top1_correct,
            second.two_hop_first_top1_correct
        );
        assert_eq!(
            first.two_hop_second_top1_given_correct_first,
            second.two_hop_second_top1_given_correct_first
        );
        assert_eq!(
            first.two_hop_second_top5_given_correct_first,
            second.two_hop_second_top5_given_correct_first
        );
    }

    #[test]
    fn exact_targets_are_unique_per_node_and_queries_match() {
        let adjacency = build_exact_adjacency(16, 8).unwrap();
        for source in 0..16 {
            let mut targets: Vec<_> = (0..8)
                .map(|role| exact_target(&adjacency, 8, source, role))
                .collect();
            assert!(targets.iter().all(|&target| target != source));
            targets.sort_unstable();
            targets.dedup();
            assert_eq!(targets.len(), 8);
        }
        let queries = make_two_hop_queries(&adjacency, 16, 8, 50, 123);
        for query in queries {
            assert_eq!(
                query.intermediate,
                exact_target(&adjacency, 8, query.source, query.first_role)
            );
            assert_eq!(
                query.target,
                exact_target(&adjacency, 8, query.intermediate, query.second_role)
            );
        }
    }

    #[test]
    fn output_is_atomic_and_replaces_a_complete_prior_artifact() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "mneme-vsa-relational-output-{}-{unique}",
            std::process::id()
        ));
        let path = dir.join("nested").join("result.json");

        atomic_write(&path, b"{\"complete\":false}\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{\"complete\":false}\n");
        atomic_write(&path, b"{\"complete\":true}\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{\"complete\":true}\n");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);

        fs::remove_file(path).unwrap();
        fs::remove_dir(dir.join("nested")).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
