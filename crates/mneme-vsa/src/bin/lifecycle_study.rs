use std::env;
use std::fs::{self, File, OpenOptions};
use std::hint::black_box;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mneme_vsa::{Error, Fhrr, Fingerprint, MUTABLE_BUNDLE_OPERATOR_ID, MutableBundle, Vector};
use serde::Serialize;

const REPORT_SCHEMA_VERSION: u16 = 2;
const STUDY_KIND: &str = "mneme-vsa-fhrr-derived-relation-lifecycle-study-v2";
const WORKLOAD_CEILING: u128 = 100_000_000;
const MAX_OPERATIONS_PER_TRIAL: usize = 1_000_000;
const MAX_TRACE_PAYLOAD_BYTES: u128 = 64 * 1024 * 1024;
const MAX_DICTIONARY_VECTORS: usize = 131_072;
const MAX_PEAK_PAYLOAD_BYTES: u128 = 512 * 1024 * 1024;
const WORKLOAD_MODEL: &str = "conservative-synthetic-admission-v2: charges current finite scans, binding, mutable updates, estimate/unbind/cosine cleanup, every reference and drift phase, staged deletion, independent trace planning, audits, and approximate cleanup sorting; it is not an instruction-count upper bound, so independent operation, trace, dictionary, dimension, and peak-payload caps also apply";
const CLEANUP_ORDER_PROTOCOL: &str = "Clean-first and incremental-first query timing alternate by (trial + source + role) parity so neither path always receives the warmed cleanup dictionary.";
const PROTOCOL: &str = "OracleState independently constructs the initial graph and replays a preplanned replacement trace before any production state exists. LiveAdjacency separately retains u16 target provenance and one live bit per relation slot. Every live state is audited against the oracle initially, after each replacement, at every rebuild boundary, and after semantic deletion. Clean references and gold targets derive only from the oracle; oracle control is excluded from timings.";
const ACCOUNTING: &str = "Payload figures are serialized payload, not RSS. A deployable incremental index includes mutable coordinates, counters, role/filler codebooks, retained u16 target provenance, and the canonical live bitmap. The clean-reference and peak-study payloads are separately stated. Timing classes partition production work: replacement binding is old/new construction only, while setup, reference rebuild, periodic rebuild, and deletion include their own local bindings. Labels, Vec/Arc metadata, allocator overhead, executable code, and oracle control are excluded.";
const INTERPRETATION: &str = "For direct typed source-plus-role lookup, compact exact adjacency wins structurally: it is exact O(1), while this FHRR path retains canonical adjacency, performs O(dimension) updates, and uses O(dimension * cleanup candidates) exhaustive decoding. Any future production study must define a distinct noisy or semantic cue task and compare against ANN/PQ candidate generation.";
const LIMITATIONS: [&str; 7] = [
    "This is a deterministic synthetic probe, not an end-to-end Mneme-memory claim or a confidence interval.",
    "Mutable subtraction/replacement is caller-proven only; canonical provenance remains required and the bundle has no member set.",
    "Cleanup is exhaustive O(dictionary * dimension); a real approximate index would add its own payload, build cost, and failure modes.",
    "The mutable f32 estimate and clean f64 superposition have distinct arithmetic identities; drift is observed rather than normalized away.",
    "The measured source-plus-role lookup is answered exactly in O(1) by compact adjacency; this study cannot motivate FHRR as an adjacency replacement.",
    "Independent random atoms do not model correlated semantic cues, corpus-scale candidate generation, or a fair ANN/PQ baseline.",
    "Deleted and absent relations abstain by protocol and are not sent through cleanup, so this study does not estimate their false-activation rate.",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Config {
    dimensions: Vec<usize>,
    nodes: Vec<usize>,
    degrees: Vec<usize>,
    distractors: Vec<usize>,
    operations: Vec<usize>,
    rebuild_every: Vec<usize>,
    deleted_node_percent: usize,
    trials: usize,
    seed: u64,
    machine_label: String,
    format: OutputFormat,
    output: Option<PathBuf>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            dimensions: vec![128],
            nodes: vec![64],
            degrees: vec![4],
            distractors: vec![16],
            operations: vec![64],
            rebuild_every: vec![0, 32],
            deleted_node_percent: 25,
            trials: 1,
            seed: 0x4d4e_454d_454c_4946,
            machine_label: "unspecified".into(),
            format: OutputFormat::Human,
            output: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct ReportConfig {
    dimensions: Vec<usize>,
    nodes: Vec<usize>,
    degrees: Vec<usize>,
    distractors: Vec<usize>,
    operations: Vec<usize>,
    rebuild_every: Vec<usize>,
    deleted_node_percent: usize,
    trials: usize,
    seed: u64,
    machine_label: String,
    workload_ceiling: u128,
    requested_workload: u128,
    workload_model: &'static str,
    max_operations_per_trial: usize,
    max_trace_payload_bytes: u128,
    max_dictionary_vectors: usize,
    max_peak_payload_bytes: u128,
}
#[derive(Debug, Serialize)]
struct StudyReport {
    schema_version: u16,
    kind: &'static str,
    protocol: &'static str,
    accounting: &'static str,
    interpretation: &'static str,
    limitations: &'static [&'static str],
    control_validation_excluded_from_timing: bool,
    shared_codebook_setup_scope: &'static str,
    cleanup_order_protocol: &'static str,
    mutable_bundle_operator: &'static str,
    configuration: ReportConfig,
    results: Vec<StudyResult>,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
struct Timing {
    shared_codebook_setup_ns: u128,
    trial_setup_ns: u128,
    replacement_binding_ns: u128,
    bundle_mutation_ns: u128,
    exact_mutation_ns: u128,
    stale_probe_ns: u128,
    initial_reference_rebuild_ns: u128,
    initial_drift_ns: u128,
    pre_periodic_reference_rebuild_ns: u128,
    pre_periodic_drift_ns: u128,
    periodic_accumulator_rebuild_ns: u128,
    post_periodic_drift_ns: u128,
    final_predelete_reference_rebuild_ns: u128,
    final_predelete_drift_ns: u128,
    deletion_ns: u128,
    postdelete_reference_rebuild_ns: u128,
    clean_cleanup_ns: u128,
    incremental_cleanup_ns: u128,
    exact_query_ns: u128,
}

#[derive(Debug, Serialize)]
struct TimingRates {
    trial_setup_ns_per_trial: f64,
    replacement_binding_ns_per_operation: f64,
    bundle_mutation_ns_per_operation: f64,
    exact_mutation_ns_per_operation: f64,
    stale_probe_ns_per_operation: f64,
    initial_reference_rebuild_count: usize,
    initial_reference_rebuild_ns_per_rebuild: f64,
    pre_periodic_reference_rebuild_count: usize,
    pre_periodic_reference_rebuild_ns_per_rebuild: f64,
    periodic_accumulator_rebuild_count: usize,
    periodic_accumulator_rebuild_ns_per_rebuild: f64,
    final_predelete_reference_rebuild_count: usize,
    final_predelete_reference_rebuild_ns_per_rebuild: f64,
    postdelete_reference_rebuild_count: usize,
    postdelete_reference_rebuild_ns_per_rebuild: f64,
    deletion_ns_per_node: f64,
    clean_cleanup_ns_per_query: f64,
    incremental_cleanup_ns_per_query: f64,
    exact_query_ns_per_query: f64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct DriftMetric {
    cosine_mean: f64,
    cosine_worst: f64,
    samples: usize,
    passes: usize,
}

#[derive(Debug, Serialize)]
struct DriftReport {
    initial: DriftMetric,
    pre_periodic: DriftMetric,
    post_periodic: DriftMetric,
    final_predelete: DriftMetric,
    combined: DriftMetric,
}
#[derive(Debug, Serialize)]
struct StudyResult {
    fingerprint: Fingerprint,
    mutable_bundle_operator: &'static str,
    node_count: usize,
    degree: usize,
    cleanup_distractors: usize,
    operations_per_trial: usize,
    rebuild_every: usize,
    trials: usize,
    deleted_node_percent: usize,
    deleted_nodes_per_trial: usize,
    present_relation_queries: usize,
    clean_first_queries: usize,
    incremental_first_queries: usize,
    clean_top1_correct: usize,
    clean_top5_correct: usize,
    incremental_top1_correct: usize,
    incremental_top5_correct: usize,
    clean_top1_recall: f64,
    clean_top5_recall: f64,
    incremental_top1_recall: f64,
    incremental_top5_recall: f64,
    drift: DriftReport,
    stale_old_target_top1_count: usize,
    stale_old_target_top1_rate: f64,
    replacement_operations: usize,
    expected_empty_estimates: usize,
    empty_node_abstentions: usize,
    unexpected_empty_estimates: usize,
    empty_estimate_other_errors: usize,
    empty_node_cleanup_attempts: usize,
    live_adjacency_audits: usize,
    live_adjacency_correct: bool,
    rebuild_count: usize,
    timing: Timing,
    timing_rates: TimingRates,
    hologram_coordinate_payload_bytes: usize,
    accumulator_coordinate_payload_bytes: usize,
    accumulator_count_metadata_bytes: usize,
    role_codebook_payload_bytes: usize,
    filler_cleanup_dictionary_payload_bytes: usize,
    incremental_derived_lookup_payload_bytes: usize,
    deployable_incremental_payload_bytes: usize,
    clean_reference_payload_bytes: usize,
    peak_study_payload_bytes: usize,
    target_provenance_payload_bytes: usize,
    live_bitmap_payload_bytes: usize,
    canonical_total_payload_bytes: usize,
    incremental_derived_to_canonical_ratio: f64,
    deployable_incremental_to_canonical_ratio: f64,
    clean_reference_to_canonical_ratio: f64,
    peak_study_to_canonical_ratio: f64,
}
#[derive(Default)]
struct Totals {
    clean_top1: usize,
    clean_top5: usize,
    incremental_top1: usize,
    incremental_top5: usize,
    queries: usize,
    clean_first_queries: usize,
    incremental_first_queries: usize,
    initial_drift: DriftTotals,
    pre_periodic_drift: DriftTotals,
    post_periodic_drift: DriftTotals,
    final_predelete_drift: DriftTotals,
    stale_old_top1: usize,
    replacements: usize,
    expected_empty: usize,
    abstentions: usize,
    unexpected_estimates: usize,
    empty_other_errors: usize,
    empty_cleanup_attempts: usize,
    audits: usize,
    audit_ok: bool,
    rebuilds: usize,
    initial_reference_rebuilds: usize,
    pre_periodic_reference_rebuilds: usize,
    final_predelete_reference_rebuilds: usize,
    postdelete_reference_rebuilds: usize,
    deleted_nodes: usize,
    timing: Timing,
}

#[derive(Clone, Copy, Debug, Default)]
struct DriftTotals {
    sum: f64,
    worst: f32,
    samples: usize,
    passes: usize,
}

#[derive(Clone, Copy)]
enum DriftPhase {
    Initial,
    PrePeriodic,
    PostPeriodic,
    FinalPredelete,
}
#[derive(Clone, Copy)]
struct StudyInput {
    dimension: usize,
    nodes: usize,
    degree: usize,
    distractors: usize,
    operations: usize,
    rebuild_every: usize,
    deleted_percent: usize,
    trials: usize,
    seed: u64,
}

struct Codebook<'a> {
    fhrr: &'a Fhrr,
    roles: &'a [Vector],
    labels: &'a [String],
    nodes: &'a [Vector],
    distractor_labels: &'a [String],
    distractors: &'a [Vector],
}

/// Independent expected semantic state. It intentionally has no dependency on
/// LiveAdjacency, production mutation, or candidate-selection helpers.
#[derive(Clone, Debug)]
struct OracleState {
    targets: Vec<u16>,
    live: Vec<bool>,
    nodes: usize,
    degree: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlannedReplace {
    source: usize,
    role: usize,
    old_target: u16,
    new_target: u16,
}
impl OracleState {
    fn initial(nodes: usize, degree: usize) -> Result<Self, String> {
        let mut targets = Vec::with_capacity(checked_mul(nodes, degree, "oracle entries")?);
        for source in 0..nodes {
            for role in 0..degree {
                targets.push(
                    u16::try_from((source + role + 1) % nodes)
                        .map_err(|_| "oracle target does not fit u16")?,
                );
            }
        }
        Ok(Self {
            live: vec![true; targets.len()],
            targets,
            nodes,
            degree,
        })
    }
    fn slot(&self, source: usize, role: usize) -> usize {
        source * self.degree + role
    }
    fn target(&self, source: usize, role: usize) -> u16 {
        self.targets[self.slot(source, role)]
    }
    fn is_live(&self, source: usize, role: usize) -> bool {
        self.live[self.slot(source, role)]
    }
    fn validate_replace(&self, op: &PlannedReplace) -> Result<(), String> {
        let slot = self.slot(op.source, op.role);
        if !self.live[slot] || self.targets[slot] != op.old_target {
            return Err("oracle trace no longer matches oracle state".into());
        }
        Ok(())
    }
    fn commit_validated_replace(&mut self, op: &PlannedReplace) {
        let slot = self.slot(op.source, op.role);
        self.targets[slot] = op.new_target;
    }
    fn apply(&mut self, op: &PlannedReplace) -> Result<(), String> {
        self.validate_replace(op)?;
        self.commit_validated_replace(op);
        Ok(())
    }
    fn delete_source(&mut self, source: usize) {
        for role in 0..self.degree {
            let slot = self.slot(source, role);
            self.live[slot] = false;
        }
    }
}
/// Production canonical state: targets are retained provenance; `live` carries semantic presence.
#[derive(Clone, Debug)]
struct LiveAdjacency {
    targets: Vec<u16>,
    live: Vec<bool>,
    nodes: usize,
    degree: usize,
}
impl LiveAdjacency {
    /// Construct the production layout independently from `OracleState`.
    /// The deliberately different loop form lets the initial audit detect a
    /// defect in either builder instead of copying one defect into both.
    fn initial(nodes: usize, degree: usize) -> Result<Self, String> {
        let entries = checked_mul(nodes, degree, "live adjacency entries")?;
        let mut targets = vec![0_u16; entries];
        for source in 0..nodes {
            let mut target = if source + 1 == nodes { 0 } else { source + 1 };
            for role in 0..degree {
                targets[source * degree + role] =
                    u16::try_from(target).map_err(|_| "live target does not fit u16")?;
                target += 1;
                if target == nodes {
                    target = 0;
                }
            }
        }
        Ok(Self {
            targets,
            live: vec![true; entries],
            nodes,
            degree,
        })
    }
    fn slot(&self, source: usize, role: usize) -> usize {
        source * self.degree + role
    }
    fn target(&self, source: usize, role: usize) -> u16 {
        self.targets[self.slot(source, role)]
    }
    fn is_live(&self, source: usize, role: usize) -> bool {
        self.live[self.slot(source, role)]
    }
    fn validate_replace(&self, op: &PlannedReplace) -> Result<(), String> {
        let slot = self.slot(op.source, op.role);
        if !self.live[slot] || self.targets[slot] != op.old_target {
            return Err("live replacement provenance mismatch".into());
        }
        Ok(())
    }
    fn commit_validated_replace(&mut self, op: &PlannedReplace) {
        let slot = self.slot(op.source, op.role);
        self.targets[slot] = op.new_target;
    }
    fn delete_source(&mut self, source: usize) {
        for role in 0..self.degree {
            let slot = self.slot(source, role);
            self.live[slot] = false;
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("lifecycle-study: {error}");
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
    };
    Ok(())
}
fn parse_args<I>(args: I) -> Result<Option<Config>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut c = Config::default();
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value =
            |a: &mut I::IntoIter| a.next().ok_or_else(|| format!("{flag} requires a value"));
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "--dimensions" => c.dimensions = parse_list(&value(&mut args)?, "dimensions", false)?,
            "--nodes" => c.nodes = parse_list(&value(&mut args)?, "nodes", false)?,
            "--degrees" => c.degrees = parse_list(&value(&mut args)?, "degrees", false)?,
            "--distractors" => c.distractors = parse_list(&value(&mut args)?, "distractors", true)?,
            "--operations" => c.operations = parse_list(&value(&mut args)?, "operations", true)?,
            "--rebuild-every" => {
                c.rebuild_every = parse_list(&value(&mut args)?, "rebuild-every", true)?
            }
            "--deleted-node-percent" => c.deleted_node_percent = parse_percent(&value(&mut args)?)?,
            "--trials" => c.trials = parse_positive(&value(&mut args)?, "trials")?,
            "--seed" => {
                c.seed = value(&mut args)?
                    .parse()
                    .map_err(|_| "seed must be an unsigned decimal integer")?
            }
            "--machine-label" => c.machine_label = value(&mut args)?,
            "--format" => {
                c.format = match value(&mut args)?.as_str() {
                    "human" => OutputFormat::Human,
                    "json" => OutputFormat::Json,
                    other => {
                        return Err(format!("unknown format {other:?}; expected human or json"));
                    }
                }
            }
            "--output" => c.output = Some(PathBuf::from(value(&mut args)?)),
            other => return Err(format!("unknown argument {other:?}; use --help")),
        }
    }
    if c.output.is_some() && c.format != OutputFormat::Json {
        return Err("--output requires --format json".into());
    }
    validate_config(&c)?;
    Ok(Some(c))
}
fn parse_list(input: &str, label: &str, zero: bool) -> Result<Vec<usize>, String> {
    let mut values = input
        .split(',')
        .map(|part| {
            let value = part
                .parse::<usize>()
                .map_err(|_| format!("{label} must be a comma-separated integer list"))?;
            if !zero && value == 0 {
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
    let n = input
        .parse()
        .map_err(|_| format!("{label} must be a positive integer"))?;
    (n > 0)
        .then_some(n)
        .ok_or_else(|| format!("{label} must be greater than zero"))
}
fn parse_percent(input: &str) -> Result<usize, String> {
    let n = input
        .parse::<usize>()
        .map_err(|_| "deleted-node-percent must be an integer from 0 to 100".to_owned())?;
    (n <= 100)
        .then_some(n)
        .ok_or_else(|| "deleted-node-percent must be in 0..=100".into())
}

fn validate_config(config: &Config) -> Result<(), String> {
    for &dimension in &config.dimensions {
        Fingerprint::new(dimension, config.seed).map_err(|error| error.to_string())?;
    }
    for &operations in &config.operations {
        if operations > MAX_OPERATIONS_PER_TRIAL {
            return Err(format!(
                "operations value {operations} exceeds per-trial cap {MAX_OPERATIONS_PER_TRIAL}"
            ));
        }
        let trace_bytes = checked_u128_mul(
            operations as u128,
            std::mem::size_of::<PlannedReplace>() as u128,
            "replacement trace payload",
        )?;
        if trace_bytes > MAX_TRACE_PAYLOAD_BYTES {
            return Err(format!(
                "replacement trace payload {trace_bytes} exceeds cap {MAX_TRACE_PAYLOAD_BYTES}"
            ));
        }
    }
    for &nodes in &config.nodes {
        if nodes > usize::from(u16::MAX) + 1 {
            return Err(format!(
                "nodes value {nodes} exceeds the u16 exact-adjacency limit of 65536"
            ));
        }
        for &degree in &config.degrees {
            if degree >= nodes {
                return Err(format!(
                    "degree {degree} must be less than node count {nodes}"
                ));
            }
            if config.operations.iter().any(|&n| n > 0) && degree + 1 >= nodes {
                return Err(format!(
                    "degree {degree} leaves no distinct replacement target for nodes={nodes}; churn requires degree <= nodes - 2"
                ));
            }
        }
        for &distractors in &config.distractors {
            let dictionary = checked_add(nodes, distractors, "cleanup dictionary vectors")?;
            if dictionary > MAX_DICTIONARY_VECTORS {
                return Err(format!(
                    "cleanup dictionary has {dictionary} vectors; cap is {MAX_DICTIONARY_VECTORS}"
                ));
            }
        }
    }
    for &dimension in &config.dimensions {
        for &nodes in &config.nodes {
            for &degree in &config.degrees {
                for &distractors in &config.distractors {
                    let peak = peak_payload_estimate(dimension, nodes, degree, distractors)?;
                    if peak > MAX_PEAK_PAYLOAD_BYTES {
                        return Err(format!(
                            "peak serialized study payload {peak} exceeds cap {MAX_PEAK_PAYLOAD_BYTES}"
                        ));
                    }
                }
            }
        }
    }
    let work = grid_workload(config)?;
    if work > WORKLOAD_CEILING {
        return Err(format!(
            "requested workload {work} exceeds lifecycle-study safety ceiling {WORKLOAD_CEILING}"
        ));
    }
    Ok(())
}
fn peak_payload_estimate(
    dimension: usize,
    nodes: usize,
    degree: usize,
    distractors: usize,
) -> Result<u128, String> {
    let vector = checked_u128_mul(
        dimension as u128,
        (2 * std::mem::size_of::<f32>()) as u128,
        "vector coordinate payload",
    )?;
    let node_vectors = checked_u128_mul(vector, nodes as u128, "node vector payload")?;
    let counters = checked_u128_mul(
        nodes as u128,
        std::mem::size_of::<usize>() as u128,
        "counter payload",
    )?;
    let roles = checked_u128_mul(vector, degree as u128, "role payload")?;
    let dictionary_vectors = checked_u128_add(
        nodes as u128,
        distractors as u128,
        "dictionary vector count",
    )?;
    let dictionary = checked_u128_mul(vector, dictionary_vectors, "dictionary coordinate payload")?;
    let relations = checked_u128_mul(nodes as u128, degree as u128, "canonical relation count")?;
    let targets = checked_u128_mul(
        relations,
        std::mem::size_of::<u16>() as u128,
        "target payload",
    )?;
    let bitmap = checked_u128_add(relations, 7, "live bitmap rounding")? / 8;
    sum_work(
        &[
            node_vectors,
            counters,
            roles,
            dictionary,
            targets,
            bitmap,
            node_vectors,
        ],
        "peak serialized study payload",
    )
}
/// A checked, complete work model. Each configuration is summed, not estimated
/// from independent maxima: codebooks, setup bindings, mutation, stale scans,
/// every reference/drift/rebuild pass, deletion, and both query paths count.
fn grid_workload(config: &Config) -> Result<u128, String> {
    let mut total = 0_u128;
    for &d in &config.dimensions {
        for &n in &config.nodes {
            for &r in &config.degrees {
                for &noise in &config.distractors {
                    for &ops in &config.operations {
                        for &cadence in &config.rebuild_every {
                            let dim = d as u128;
                            let nodes = n as u128;
                            let degree = r as u128;
                            let dict = checked_u128_add(nodes, noise as u128, "dictionary work")?;
                            let relations = checked_u128_mul(nodes, degree, "relation work")?;
                            let operations = ops as u128;
                            let periodic = ops.checked_div(cadence).unwrap_or(0) as u128;
                            let deleted = checked_u128_mul(
                                nodes,
                                config.deleted_node_percent as u128,
                                "deleted nodes numerator",
                            )? / 100;
                            let present_nodes = checked_u128_sub(nodes, deleted, "present nodes")?;
                            let deleted_relations =
                                checked_u128_mul(deleted, degree, "deleted relations")?;
                            let present_relations = checked_u128_sub(
                                relations,
                                deleted_relations,
                                "present relations",
                            )?;
                            // Coefficients conservatively count the current public
                            // algebra's coordinate scans, including validation.
                            let codebook = checked_u128_mul(
                                dim,
                                checked_u128_add(dict, degree, "codebook vectors")?,
                                "codebook setup",
                            )?;
                            let accumulator_build = checked_u128_mul(
                                dim,
                                checked_u128_add(
                                    checked_u128_mul(10, relations, "accumulator relation scans")?,
                                    nodes,
                                    "accumulator allocation scans",
                                )?,
                                "accumulator build",
                            )?;
                            let full_reference = checked_u128_mul(
                                dim,
                                checked_u128_add(
                                    checked_u128_mul(6, relations, "reference relation scans")?,
                                    checked_u128_mul(3, nodes, "reference node scans")?,
                                    "reference scans",
                                )?,
                                "full reference rebuild",
                            )?;
                            let full_drift = checked_u128_mul(
                                dim,
                                checked_u128_mul(7, nodes, "drift scans")?,
                                "full drift pass",
                            )?;
                            let replacement = checked_u128_mul(
                                dim,
                                checked_u128_mul(14, operations, "replacement scans")?,
                                "replacement work",
                            )?;
                            let stale_per_operation = checked_u128_add(
                                12,
                                checked_u128_mul(3, dict, "stale cleanup candidate scans")?,
                                "stale estimate, unbind, and cleanup scans",
                            )?;
                            let stale = checked_u128_mul(
                                dim,
                                checked_u128_mul(operations, stale_per_operation, "stale probes")?,
                                "stale probe work",
                            )?;
                            let periodic_checkpoint = sum_work(
                                &[full_reference, full_drift, accumulator_build, full_drift],
                                "periodic reference, pre-drift, rebuild, and post-drift",
                            )?;
                            let periodic_work = checked_u128_mul(
                                periodic,
                                periodic_checkpoint,
                                "periodic checkpoint work",
                            )?;
                            let final_predelete = checked_u128_add(
                                full_reference,
                                full_drift,
                                "final pre-delete checkpoint",
                            )?;
                            let deletion = checked_u128_mul(
                                dim,
                                checked_u128_add(
                                    checked_u128_mul(
                                        10,
                                        deleted_relations,
                                        "delete bind and subtract scans",
                                    )?,
                                    deleted,
                                    "staged delete clone scans",
                                )?,
                                "deletion work",
                            )?;
                            let postdelete_reference = checked_u128_mul(
                                dim,
                                checked_u128_add(
                                    checked_u128_mul(
                                        6,
                                        present_relations,
                                        "post-delete reference relation scans",
                                    )?,
                                    checked_u128_mul(
                                        3,
                                        present_nodes,
                                        "post-delete reference node scans",
                                    )?,
                                    "post-delete reference scans",
                                )?,
                                "post-delete reference",
                            )?;
                            let query_per_relation = checked_u128_add(
                                20,
                                checked_u128_mul(6, dict, "two cleanup candidate scans")?,
                                "clean and incremental query scans",
                            )?;
                            let query = checked_u128_mul(
                                dim,
                                checked_u128_mul(
                                    present_relations,
                                    query_per_relation,
                                    "query relations",
                                )?,
                                "query work",
                            )?;
                            let coordinate_work = sum_work(
                                &[
                                    accumulator_build,
                                    full_reference,
                                    full_drift,
                                    replacement,
                                    stale,
                                    periodic_work,
                                    final_predelete,
                                    deletion,
                                    postdelete_reference,
                                    query,
                                ],
                                "coordinate workload",
                            )?;

                            // Control/oracle work is excluded from timings but not
                            // from the admission ceiling. Cleanup sorts are also
                            // charged so dimension=1 cannot hide a huge dictionary.
                            let trace_planning = checked_u128_mul(
                                operations,
                                checked_u128_mul(
                                    nodes,
                                    checked_u128_add(degree, 1, "trace candidate checks")?,
                                    "trace candidate scan",
                                )?,
                                "trace planning",
                            )?;
                            let audit_count = checked_u128_add(
                                checked_u128_add(operations, periodic, "audit boundaries")?,
                                3,
                                "initial, final, and post-delete audits",
                            )?;
                            let audit_work = checked_u128_mul(
                                audit_count,
                                checked_u128_mul(2, relations, "audit target and live scans")?,
                                "audit work",
                            )?;
                            let cleanup_calls = checked_u128_add(
                                operations,
                                checked_u128_mul(
                                    2,
                                    present_relations,
                                    "clean and incremental cleanup calls",
                                )?,
                                "all cleanup calls",
                            )?;
                            let cleanup_sort = checked_u128_mul(
                                cleanup_calls,
                                checked_u128_mul(
                                    dict,
                                    ceil_log2(dict),
                                    "cleanup sort comparisons",
                                )?,
                                "cleanup sort work",
                            )?;
                            let delete_selection =
                                checked_u128_mul(nodes, ceil_log2(nodes), "delete selection sort")?;
                            let scalar_work = sum_work(
                                &[
                                    trace_planning,
                                    audit_work,
                                    cleanup_sort,
                                    delete_selection,
                                    relations,
                                    operations,
                                    deleted_relations,
                                    present_relations,
                                ],
                                "scalar workload",
                            )?;
                            let per_trial =
                                checked_u128_add(coordinate_work, scalar_work, "trial workload")?;
                            let configuration = checked_u128_add(
                                codebook,
                                checked_u128_mul(
                                    per_trial,
                                    config.trials as u128,
                                    "trials workload",
                                )?,
                                "configuration workload",
                            )?;
                            total = checked_u128_add(total, configuration, "grid workload")?;
                        }
                    }
                }
            }
        }
    }
    Ok(total)
}

fn run_study(config: &Config) -> Result<StudyReport, String> {
    validate_config(config)?;
    let mut results = Vec::new();
    for &dimension in &config.dimensions {
        for &nodes in &config.nodes {
            for &degree in &config.degrees {
                for &distractors in &config.distractors {
                    for &operations in &config.operations {
                        for &rebuild_every in &config.rebuild_every {
                            results.push(study_configuration(StudyInput {
                                dimension,
                                nodes,
                                degree,
                                distractors,
                                operations,
                                rebuild_every,
                                deleted_percent: config.deleted_node_percent,
                                trials: config.trials,
                                seed: config.seed,
                            })?);
                        }
                    }
                }
            }
        }
    }
    Ok(StudyReport {
        schema_version: REPORT_SCHEMA_VERSION,
        kind: STUDY_KIND,
        protocol: PROTOCOL,
        accounting: ACCOUNTING,
        interpretation: INTERPRETATION,
        limitations: &LIMITATIONS,
        control_validation_excluded_from_timing: true,
        shared_codebook_setup_scope: "once_per_configuration",
        cleanup_order_protocol: CLEANUP_ORDER_PROTOCOL,
        mutable_bundle_operator: MUTABLE_BUNDLE_OPERATOR_ID,
        configuration: ReportConfig {
            dimensions: config.dimensions.clone(),
            nodes: config.nodes.clone(),
            degrees: config.degrees.clone(),
            distractors: config.distractors.clone(),
            operations: config.operations.clone(),
            rebuild_every: config.rebuild_every.clone(),
            deleted_node_percent: config.deleted_node_percent,
            trials: config.trials,
            seed: config.seed,
            machine_label: config.machine_label.clone(),
            workload_ceiling: WORKLOAD_CEILING,
            requested_workload: grid_workload(config)?,
            workload_model: WORKLOAD_MODEL,
            max_operations_per_trial: MAX_OPERATIONS_PER_TRIAL,
            max_trace_payload_bytes: MAX_TRACE_PAYLOAD_BYTES,
            max_dictionary_vectors: MAX_DICTIONARY_VECTORS,
            max_peak_payload_bytes: MAX_PEAK_PAYLOAD_BYTES,
        },
        results,
    })
}

fn study_configuration(input: StudyInput) -> Result<StudyResult, String> {
    let fhrr = Fhrr::new(input.dimension, input.seed).map_err(|e| e.to_string())?;
    let codebook_start = Instant::now();
    let roles: Vec<_> = (0..input.degree)
        .map(|role| fhrr.atom(&format!("role:{role}")))
        .collect();
    let labels: Vec<_> = (0..input.nodes)
        .map(|node| format!("node:{node}"))
        .collect();
    let node_vectors: Vec<_> = labels.iter().map(|label| fhrr.atom(label)).collect();
    let distractor_labels: Vec<_> = (0..input.distractors)
        .map(|i| format!("distractor:{i}"))
        .collect();
    let distractors: Vec<_> = distractor_labels
        .iter()
        .map(|label| fhrr.atom(label))
        .collect();
    let codebook = Codebook {
        fhrr: &fhrr,
        roles: &roles,
        labels: &labels,
        nodes: &node_vectors,
        distractor_labels: &distractor_labels,
        distractors: &distractors,
    };
    let mut totals = Totals {
        audit_ok: true,
        timing: Timing {
            shared_codebook_setup_ns: checked_nanos(codebook_start.elapsed())?,
            ..Timing::default()
        },
        ..Totals::default()
    };
    for trial in 0..input.trials {
        let trial_seed = mix64(input.seed ^ (trial as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let mut oracle = OracleState::initial(input.nodes, input.degree)?;
        let trace = plan_replacements(&oracle, input.operations, trial_seed)?;
        let setup_start = Instant::now();
        let mut live = LiveAdjacency::initial(input.nodes, input.degree)?;
        let mut accumulators = build_live_accumulators(&fhrr, &roles, &node_vectors, &live)?;
        totals.timing.trial_setup_ns = checked_u128_add(
            totals.timing.trial_setup_ns,
            checked_nanos(setup_start.elapsed())?,
            "trial setup timing",
        )?;
        audit_live(&live, &oracle, &mut totals)?;
        let clean = timed_oracle_references(
            &fhrr,
            &roles,
            &node_vectors,
            &oracle,
            &mut totals.timing.initial_reference_rebuild_ns,
            "initial reference rebuild timing",
        )?;
        increment(
            &mut totals.initial_reference_rebuilds,
            "initial reference rebuild count",
        )?;
        drift_pass(
            &fhrr,
            &clean,
            &accumulators,
            &oracle,
            &mut totals,
            DriftPhase::Initial,
        )?;
        for (ordinal, op) in trace.iter().enumerate() {
            oracle.validate_replace(op)?;
            let exact_validation_start = Instant::now();
            live.validate_replace(op)?;
            totals.timing.exact_mutation_ns = checked_u128_add(
                totals.timing.exact_mutation_ns,
                checked_nanos(exact_validation_start.elapsed())?,
                "exact validation timing",
            )?;
            let old = bind_term(
                &fhrr,
                &roles,
                &node_vectors,
                op.role,
                usize::from(op.old_target),
                &mut totals.timing,
            )?;
            let new = bind_term(
                &fhrr,
                &roles,
                &node_vectors,
                op.role,
                usize::from(op.new_target),
                &mut totals.timing,
            )?;
            let start = Instant::now();
            accumulators[op.source]
                .replace_known_term(&old, &new)
                .map_err(|e| e.to_string())?;
            totals.timing.bundle_mutation_ns = checked_u128_add(
                totals.timing.bundle_mutation_ns,
                checked_nanos(start.elapsed())?,
                "bundle mutation timing",
            )?;
            let start = Instant::now();
            live.commit_validated_replace(op);
            totals.timing.exact_mutation_ns = checked_u128_add(
                totals.timing.exact_mutation_ns,
                checked_nanos(start.elapsed())?,
                "exact mutation timing",
            )?;
            oracle.commit_validated_replace(op);
            totals.replacements += 1;
            audit_live(&live, &oracle, &mut totals)?;
            stale_probe(
                &codebook,
                &accumulators[op.source],
                op.role,
                usize::from(op.old_target),
                &mut totals,
            )?;
            if input.rebuild_every != 0 && (ordinal + 1) % input.rebuild_every == 0 {
                let refs = timed_oracle_references(
                    &fhrr,
                    &roles,
                    &node_vectors,
                    &oracle,
                    &mut totals.timing.pre_periodic_reference_rebuild_ns,
                    "pre-periodic reference rebuild timing",
                )?;
                increment(
                    &mut totals.pre_periodic_reference_rebuilds,
                    "pre-periodic reference rebuild count",
                )?;
                drift_pass(
                    &fhrr,
                    &refs,
                    &accumulators,
                    &oracle,
                    &mut totals,
                    DriftPhase::PrePeriodic,
                )?;
                let start = Instant::now();
                accumulators = build_live_accumulators(&fhrr, &roles, &node_vectors, &live)?;
                totals.timing.periodic_accumulator_rebuild_ns = checked_u128_add(
                    totals.timing.periodic_accumulator_rebuild_ns,
                    checked_nanos(start.elapsed())?,
                    "periodic rebuild timing",
                )?;
                totals.rebuilds += 1;
                drift_pass(
                    &fhrr,
                    &refs,
                    &accumulators,
                    &oracle,
                    &mut totals,
                    DriftPhase::PostPeriodic,
                )?;
                audit_live(&live, &oracle, &mut totals)?;
            }
        }
        audit_live(&live, &oracle, &mut totals)?;
        let final_predelete = timed_oracle_references(
            &fhrr,
            &roles,
            &node_vectors,
            &oracle,
            &mut totals.timing.final_predelete_reference_rebuild_ns,
            "final pre-delete reference rebuild timing",
        )?;
        increment(
            &mut totals.final_predelete_reference_rebuilds,
            "final pre-delete reference rebuild count",
        )?;
        drift_pass(
            &fhrr,
            &final_predelete,
            &accumulators,
            &oracle,
            &mut totals,
            DriftPhase::FinalPredelete,
        )?;
        let deleted = deleted_nodes(input.nodes, input.deleted_percent, trial_seed)?;
        let start = Instant::now();
        for &source in &deleted {
            let mut staged = accumulators[source].clone();
            for (role, role_vector) in roles.iter().enumerate().take(input.degree) {
                if !live.is_live(source, role) || !oracle.is_live(source, role) {
                    return Err("deletion expected a live canonical relation".into());
                }
                let target = usize::from(live.target(source, role));
                if oracle.target(source, role) != target as u16 {
                    return Err("deletion provenance diverged from oracle".into());
                }
                let term = fhrr
                    .bind(role_vector, &node_vectors[target])
                    .map_err(|e| e.to_string())?;
                staged
                    .subtract_known_term(&term)
                    .map_err(|e| e.to_string())?;
            }
            accumulators[source] = staged;
            live.delete_source(source);
            oracle.delete_source(source);
            classify_empty(&accumulators[source], &mut totals);
        }
        totals.timing.deletion_ns = checked_u128_add(
            totals.timing.deletion_ns,
            checked_nanos(start.elapsed())?,
            "deletion timing",
        )?;
        totals.deleted_nodes += deleted.len();
        audit_live(&live, &oracle, &mut totals)?;
        let clean = timed_oracle_references(
            &fhrr,
            &roles,
            &node_vectors,
            &oracle,
            &mut totals.timing.postdelete_reference_rebuild_ns,
            "post-delete reference rebuild timing",
        )?;
        increment(
            &mut totals.postdelete_reference_rebuilds,
            "post-delete reference rebuild count",
        )?;
        queries(
            &codebook,
            &live,
            &oracle,
            &clean,
            &accumulators,
            trial,
            &mut totals,
        )?;
    }
    payload_result(&fhrr, input, totals)
}

fn plan_replacements(
    initial: &OracleState,
    operations: usize,
    seed: u64,
) -> Result<Vec<PlannedReplace>, String> {
    let mut oracle = initial.clone();
    let mut trace = Vec::with_capacity(operations);
    for ordinal in 0..operations {
        let source = sample_index(seed, ordinal, 0x4c49_4645_5352_4353, oracle.nodes);
        let role = sample_index(seed, ordinal, 0x4c49_4645_524f_4c45, oracle.degree);
        let old_target = oracle.target(source, role);
        let candidates: Vec<_> = (0..oracle.nodes)
            .filter(|&target| {
                target != source
                    && target != usize::from(old_target)
                    && !(0..oracle.degree)
                        .any(|other| other != role && oracle.target(source, other) == target as u16)
            })
            .collect();
        if candidates.is_empty() {
            return Err("no distinct replacement target is available in oracle trace".into());
        }
        let Some(&new_target) = candidates.get(sample_index(
            seed,
            ordinal,
            0x4c49_4645_4e45_5754,
            candidates.len(),
        )) else {
            return Err("replacement target sample escaped the oracle candidate set".into());
        };
        let op = PlannedReplace {
            source,
            role,
            old_target,
            new_target: u16::try_from(new_target).map_err(|_| "oracle target does not fit u16")?,
        };
        oracle.apply(&op)?;
        trace.push(op);
    }
    Ok(trace)
}
fn build_live_accumulators(
    fhrr: &Fhrr,
    roles: &[Vector],
    nodes: &[Vector],
    live: &LiveAdjacency,
) -> Result<Vec<MutableBundle>, String> {
    (0..live.nodes)
        .map(|source| {
            let mut bundle = MutableBundle::new(fhrr);
            for role in 0..live.degree {
                if live.is_live(source, role) {
                    let term = fhrr
                        .bind(&roles[role], &nodes[usize::from(live.target(source, role))])
                        .map_err(|e| e.to_string())?;
                    bundle.add_bound_term(&term).map_err(|e| e.to_string())?;
                }
            }
            Ok(bundle)
        })
        .collect()
}
fn bind_term(
    fhrr: &Fhrr,
    roles: &[Vector],
    nodes: &[Vector],
    role: usize,
    target: usize,
    timing: &mut Timing,
) -> Result<Vector, String> {
    let start = Instant::now();
    let term = fhrr
        .bind(&roles[role], &nodes[target])
        .map_err(|e| e.to_string())?;
    timing.replacement_binding_ns = checked_u128_add(
        timing.replacement_binding_ns,
        checked_nanos(start.elapsed())?,
        "binding timing",
    )?;
    Ok(term)
}
fn build_oracle_references(
    fhrr: &Fhrr,
    roles: &[Vector],
    nodes: &[Vector],
    oracle: &OracleState,
) -> Result<Vec<Option<Vector>>, String> {
    (0..oracle.nodes)
        .map(|source| {
            let terms = (0..oracle.degree)
                .filter(|&role| oracle.is_live(source, role))
                .map(|role| {
                    fhrr.bind(
                        &roles[role],
                        &nodes[usize::from(oracle.target(source, role))],
                    )
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            if terms.is_empty() {
                Ok(None)
            } else {
                fhrr.superpose(&terms).map(Some).map_err(|e| e.to_string())
            }
        })
        .collect()
}
fn timed_oracle_references(
    fhrr: &Fhrr,
    roles: &[Vector],
    nodes: &[Vector],
    oracle: &OracleState,
    timing: &mut u128,
    label: &str,
) -> Result<Vec<Option<Vector>>, String> {
    let start = Instant::now();
    let result = build_oracle_references(fhrr, roles, nodes, oracle)?;
    *timing = checked_u128_add(*timing, checked_nanos(start.elapsed())?, label)?;
    Ok(result)
}
fn audit_live(
    live: &LiveAdjacency,
    oracle: &OracleState,
    totals: &mut Totals,
) -> Result<(), String> {
    totals.audits = totals.audits.checked_add(1).ok_or("audit count overflow")?;
    let ok = live.nodes == oracle.nodes
        && live.degree == oracle.degree
        && live.targets == oracle.targets
        && live.live == oracle.live;
    totals.audit_ok &= ok;
    ok.then_some(())
        .ok_or_else(|| "live adjacency diverged from independent oracle".into())
}
fn drift_pass(
    fhrr: &Fhrr,
    clean: &[Option<Vector>],
    accumulators: &[MutableBundle],
    oracle: &OracleState,
    totals: &mut Totals,
    phase: DriftPhase,
) -> Result<(), String> {
    let start = Instant::now();
    increment(
        &mut drift_bucket_mut(totals, phase).passes,
        "drift pass count",
    )?;
    for source in 0..oracle.nodes {
        match (&clean[source], accumulators[source].estimate()) {
            (None, Err(Error::EmptyBundle)) => {}
            (Some(clean), Ok(incremental)) => {
                let cosine = fhrr
                    .cosine_mutable(clean, &incremental)
                    .map_err(|e| e.to_string())?;
                let bucket = drift_bucket_mut(totals, phase);
                bucket.sum += f64::from(cosine);
                bucket.worst = if bucket.samples == 0 {
                    cosine
                } else {
                    bucket.worst.min(cosine)
                };
                bucket.samples = bucket
                    .samples
                    .checked_add(1)
                    .ok_or("drift sample count overflow")?;
            }
            _ => {
                return Err(
                    "oracle clean reference and mutable bundle disagree on empty state".into(),
                );
            }
        }
    }
    let timing = drift_timing_mut(&mut totals.timing, phase);
    *timing = checked_u128_add(*timing, checked_nanos(start.elapsed())?, "drift timing")?;
    Ok(())
}

fn drift_bucket_mut(totals: &mut Totals, phase: DriftPhase) -> &mut DriftTotals {
    match phase {
        DriftPhase::Initial => &mut totals.initial_drift,
        DriftPhase::PrePeriodic => &mut totals.pre_periodic_drift,
        DriftPhase::PostPeriodic => &mut totals.post_periodic_drift,
        DriftPhase::FinalPredelete => &mut totals.final_predelete_drift,
    }
}

fn drift_timing_mut(timing: &mut Timing, phase: DriftPhase) -> &mut u128 {
    match phase {
        DriftPhase::Initial => &mut timing.initial_drift_ns,
        DriftPhase::PrePeriodic => &mut timing.pre_periodic_drift_ns,
        DriftPhase::PostPeriodic => &mut timing.post_periodic_drift_ns,
        DriftPhase::FinalPredelete => &mut timing.final_predelete_drift_ns,
    }
}
fn stale_probe(
    codebook: &Codebook<'_>,
    bundle: &MutableBundle,
    role: usize,
    old_target: usize,
    totals: &mut Totals,
) -> Result<(), String> {
    let start = Instant::now();
    let estimate = mutable_estimate_for_cleanup(bundle, totals)?;
    let hits = cleanup_mutable_relation(codebook, &estimate, &codebook.roles[role])?;
    totals.stale_old_top1 += usize::from(stale_old_target_hit(&hits, &codebook.labels[old_target]));
    totals.timing.stale_probe_ns = checked_u128_add(
        totals.timing.stale_probe_ns,
        checked_nanos(start.elapsed())?,
        "stale probe timing",
    )?;
    Ok(())
}
fn stale_old_target_hit(hits: &[mneme_vsa::CleanupHit<'_>], old: &str) -> bool {
    hits.first().is_some_and(|hit| hit.label == old)
}
fn classify_empty(bundle: &MutableBundle, totals: &mut Totals) {
    totals.expected_empty += 1;
    match bundle.estimate() {
        Err(Error::EmptyBundle) => totals.abstentions += 1,
        Ok(_) => totals.unexpected_estimates += 1,
        Err(_) => totals.empty_other_errors += 1,
    }
}
fn queries(
    codebook: &Codebook<'_>,
    live: &LiveAdjacency,
    oracle: &OracleState,
    clean: &[Option<Vector>],
    accumulators: &[MutableBundle],
    trial: usize,
    totals: &mut Totals,
) -> Result<(), String> {
    for source in 0..oracle.nodes {
        for (role, role_vector) in codebook.roles.iter().enumerate().take(oracle.degree) {
            if !oracle.is_live(source, role) {
                continue;
            }
            let target = usize::from(oracle.target(source, role));
            let correct = codebook.labels[target].as_str();
            let start = Instant::now();
            let live_present = black_box(live.is_live(source, role));
            let exact = black_box(live.target(source, role));
            totals.timing.exact_query_ns = checked_u128_add(
                totals.timing.exact_query_ns,
                checked_nanos(start.elapsed())?,
                "exact query timing",
            )?;
            if !live_present || exact != oracle.target(source, role) {
                return Err("timed live exact query diverged from oracle gold".into());
            }
            let Some(clean) = &clean[source] else {
                return Err("live oracle relation lacks clean reference".into());
            };
            let clean_first = (trial + source + role).is_multiple_of(2);
            let (clean_hits, incremental_hits) = if clean_first {
                increment(&mut totals.clean_first_queries, "clean-first query count")?;
                let clean_hits = timed_clean_cleanup(codebook, clean, role_vector, totals)?;
                let incremental_hits = timed_incremental_cleanup(
                    codebook,
                    &accumulators[source],
                    role_vector,
                    totals,
                )?;
                (clean_hits, incremental_hits)
            } else {
                increment(
                    &mut totals.incremental_first_queries,
                    "incremental-first query count",
                )?;
                let incremental_hits = timed_incremental_cleanup(
                    codebook,
                    &accumulators[source],
                    role_vector,
                    totals,
                )?;
                let clean_hits = timed_clean_cleanup(codebook, clean, role_vector, totals)?;
                (clean_hits, incremental_hits)
            };
            totals.clean_top1 +=
                usize::from(clean_hits.first().is_some_and(|hit| hit.label == correct));
            totals.clean_top5 += usize::from(clean_hits.iter().any(|hit| hit.label == correct));
            totals.incremental_top1 += usize::from(
                incremental_hits
                    .first()
                    .is_some_and(|hit| hit.label == correct),
            );
            totals.incremental_top5 +=
                usize::from(incremental_hits.iter().any(|hit| hit.label == correct));
            totals.queries += 1;
        }
    }
    Ok(())
}
fn mutable_estimate_for_cleanup(
    bundle: &MutableBundle,
    totals: &mut Totals,
) -> Result<mneme_vsa::MutableEstimate, String> {
    match bundle.estimate() {
        Ok(estimate) => Ok(estimate),
        Err(Error::EmptyBundle) => {
            increment(
                &mut totals.empty_cleanup_attempts,
                "empty cleanup attempt count",
            )?;
            Err("cleanup of an empty mutable bundle is forbidden".into())
        }
        Err(error) => Err(error.to_string()),
    }
}
fn timed_clean_cleanup<'a>(
    codebook: &'a Codebook<'a>,
    hologram: &Vector,
    role: &Vector,
    totals: &mut Totals,
) -> Result<Vec<mneme_vsa::CleanupHit<'a>>, String> {
    let start = Instant::now();
    let hits = cleanup_relation(codebook, hologram, role)?;
    totals.timing.clean_cleanup_ns = checked_u128_add(
        totals.timing.clean_cleanup_ns,
        checked_nanos(start.elapsed())?,
        "clean cleanup timing",
    )?;
    Ok(hits)
}
fn timed_incremental_cleanup<'a>(
    codebook: &'a Codebook<'a>,
    bundle: &MutableBundle,
    role: &Vector,
    totals: &mut Totals,
) -> Result<Vec<mneme_vsa::CleanupHit<'a>>, String> {
    let start = Instant::now();
    let estimate = mutable_estimate_for_cleanup(bundle, totals)?;
    let hits = cleanup_mutable_relation(codebook, &estimate, role)?;
    totals.timing.incremental_cleanup_ns = checked_u128_add(
        totals.timing.incremental_cleanup_ns,
        checked_nanos(start.elapsed())?,
        "incremental cleanup timing",
    )?;
    Ok(hits)
}
fn cleanup_relation<'a>(
    codebook: &'a Codebook<'a>,
    hologram: &Vector,
    role: &Vector,
) -> Result<Vec<mneme_vsa::CleanupHit<'a>>, String> {
    let estimate = codebook
        .fhrr
        .unbind(hologram, role)
        .map_err(|e| e.to_string())?;
    codebook
        .fhrr
        .cleanup(&estimate, dictionary(codebook), 5)
        .map_err(|e| e.to_string())
}
fn cleanup_mutable_relation<'a>(
    codebook: &'a Codebook<'a>,
    hologram: &mneme_vsa::MutableEstimate,
    role: &Vector,
) -> Result<Vec<mneme_vsa::CleanupHit<'a>>, String> {
    let estimate = codebook
        .fhrr
        .unbind_mutable(hologram, role)
        .map_err(|e| e.to_string())?;
    codebook
        .fhrr
        .cleanup_mutable(&estimate, dictionary(codebook), 5)
        .map_err(|e| e.to_string())
}
fn dictionary<'a>(codebook: &'a Codebook<'a>) -> impl Iterator<Item = (&'a str, &'a Vector)> {
    codebook
        .labels
        .iter()
        .zip(codebook.nodes)
        .map(|(l, v)| (l.as_str(), v))
        .chain(
            codebook
                .distractor_labels
                .iter()
                .zip(codebook.distractors)
                .map(|(l, v)| (l.as_str(), v)),
        )
}
fn deleted_nodes(nodes: usize, percent: usize, seed: u64) -> Result<Vec<usize>, String> {
    let count = checked_mul(nodes, percent, "deleted node numerator")? / 100;
    let mut ranked: Vec<_> = (0..nodes)
        .map(|node| {
            (
                mix64(seed ^ (node as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9)),
                node,
            )
        })
        .collect();
    ranked.sort_unstable();
    let mut result: Vec<_> = ranked
        .into_iter()
        .take(count)
        .map(|(_, node)| node)
        .collect();
    result.sort_unstable();
    Ok(result)
}
fn payload_result(fhrr: &Fhrr, input: StudyInput, totals: Totals) -> Result<StudyResult, String> {
    let vector = fhrr.payload_bytes();
    let accumulator = checked_mul(vector, input.nodes, "accumulator coordinates")?;
    let clean = checked_mul(vector, input.nodes, "clean reference coordinates")?;
    let counter = checked_mul(
        MutableBundle::new(fhrr).count_metadata_bytes(),
        input.nodes,
        "counter payload",
    )?;
    let role = checked_mul(vector, input.degree, "role payload")?;
    let fillers = checked_mul(
        vector,
        checked_add(input.nodes, input.distractors, "dictionary vectors")?,
        "dictionary payload",
    )?;
    let targets = checked_mul(
        checked_mul(input.nodes, input.degree, "target entries")?,
        std::mem::size_of::<u16>(),
        "target payload",
    )?;
    let bitmap = checked_mul(input.nodes, input.degree, "live bitmap entries")?.div_ceil(8);
    let canonical = checked_add(targets, bitmap, "canonical payload")?;
    let derived = checked_add(
        checked_add(accumulator, counter, "mutable payload")?,
        checked_add(role, fillers, "codebooks")?,
        "incremental derived lookup payload",
    )?;
    let deployable = checked_add(derived, canonical, "deployable with canonical")?;
    let clean_payload = checked_add(
        checked_add(clean, role, "clean and roles")?,
        checked_add(fillers, canonical, "clean dictionary and canonical")?,
        "clean reference payload",
    )?;
    let peak = checked_add(deployable, clean, "peak study payload")?;
    let drift = drift_report(&totals);
    let timing_rates = timing_rates(&totals, input.trials);
    Ok(StudyResult {
        fingerprint: fhrr.fingerprint().clone(),
        mutable_bundle_operator: MUTABLE_BUNDLE_OPERATOR_ID,
        node_count: input.nodes,
        degree: input.degree,
        cleanup_distractors: input.distractors,
        operations_per_trial: input.operations,
        rebuild_every: input.rebuild_every,
        trials: input.trials,
        deleted_node_percent: input.deleted_percent,
        deleted_nodes_per_trial: totals.deleted_nodes / input.trials,
        present_relation_queries: totals.queries,
        clean_first_queries: totals.clean_first_queries,
        incremental_first_queries: totals.incremental_first_queries,
        clean_top1_correct: totals.clean_top1,
        clean_top5_correct: totals.clean_top5,
        incremental_top1_correct: totals.incremental_top1,
        incremental_top5_correct: totals.incremental_top5,
        clean_top1_recall: ratio(totals.clean_top1, totals.queries),
        clean_top5_recall: ratio(totals.clean_top5, totals.queries),
        incremental_top1_recall: ratio(totals.incremental_top1, totals.queries),
        incremental_top5_recall: ratio(totals.incremental_top5, totals.queries),
        drift,
        stale_old_target_top1_count: totals.stale_old_top1,
        stale_old_target_top1_rate: ratio(totals.stale_old_top1, totals.replacements),
        replacement_operations: totals.replacements,
        expected_empty_estimates: totals.expected_empty,
        empty_node_abstentions: totals.abstentions,
        unexpected_empty_estimates: totals.unexpected_estimates,
        empty_estimate_other_errors: totals.empty_other_errors,
        empty_node_cleanup_attempts: totals.empty_cleanup_attempts,
        live_adjacency_audits: totals.audits,
        live_adjacency_correct: totals.audit_ok,
        rebuild_count: totals.rebuilds,
        timing_rates,
        timing: totals.timing,
        hologram_coordinate_payload_bytes: clean,
        accumulator_coordinate_payload_bytes: accumulator,
        accumulator_count_metadata_bytes: counter,
        role_codebook_payload_bytes: role,
        filler_cleanup_dictionary_payload_bytes: fillers,
        incremental_derived_lookup_payload_bytes: derived,
        deployable_incremental_payload_bytes: deployable,
        clean_reference_payload_bytes: clean_payload,
        peak_study_payload_bytes: peak,
        target_provenance_payload_bytes: targets,
        live_bitmap_payload_bytes: bitmap,
        canonical_total_payload_bytes: canonical,
        incremental_derived_to_canonical_ratio: ratio_bytes(derived, canonical),
        deployable_incremental_to_canonical_ratio: ratio_bytes(deployable, canonical),
        clean_reference_to_canonical_ratio: ratio_bytes(clean_payload, canonical),
        peak_study_to_canonical_ratio: ratio_bytes(peak, canonical),
    })
}

fn sample_index(seed: u64, ordinal: usize, salt: u64, modulus: usize) -> usize {
    (mix64(seed ^ salt ^ (ordinal as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)) % modulus as u64)
        as usize
}
fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}
fn checked_add(a: usize, b: usize, label: &str) -> Result<usize, String> {
    a.checked_add(b).ok_or_else(|| format!("{label} overflow"))
}
fn checked_mul(a: usize, b: usize, label: &str) -> Result<usize, String> {
    a.checked_mul(b).ok_or_else(|| format!("{label} overflow"))
}
fn checked_u128_add(a: u128, b: u128, label: &str) -> Result<u128, String> {
    a.checked_add(b).ok_or_else(|| format!("{label} overflow"))
}
fn checked_u128_sub(a: u128, b: u128, label: &str) -> Result<u128, String> {
    a.checked_sub(b).ok_or_else(|| format!("{label} underflow"))
}
fn checked_u128_mul(a: u128, b: u128, label: &str) -> Result<u128, String> {
    a.checked_mul(b).ok_or_else(|| format!("{label} overflow"))
}
fn sum_work(values: &[u128], label: &str) -> Result<u128, String> {
    values
        .iter()
        .try_fold(0, |sum, value| checked_u128_add(sum, *value, label))
}
fn checked_nanos(duration: Duration) -> Result<u128, String> {
    Ok(duration.as_nanos())
}
fn increment(value: &mut usize, label: &str) -> Result<(), String> {
    *value = value
        .checked_add(1)
        .ok_or_else(|| format!("{label} overflow"))?;
    Ok(())
}
fn ceil_log2(value: u128) -> u128 {
    if value <= 1 {
        0
    } else {
        u128::from(u128::BITS - (value - 1).leading_zeros())
    }
}
fn ratio(a: usize, b: usize) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}
fn ratio_bytes(a: usize, b: usize) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}
fn nanos_per(total: u128, count: usize) -> f64 {
    if count == 0 {
        0.0
    } else {
        total as f64 / count as f64
    }
}
fn drift_metric(totals: DriftTotals) -> DriftMetric {
    DriftMetric {
        cosine_mean: if totals.samples == 0 {
            0.0
        } else {
            totals.sum / totals.samples as f64
        },
        cosine_worst: if totals.samples == 0 {
            0.0
        } else {
            f64::from(totals.worst)
        },
        samples: totals.samples,
        passes: totals.passes,
    }
}
fn combined_drift(buckets: &[DriftTotals]) -> DriftTotals {
    let mut combined = DriftTotals::default();
    for bucket in buckets {
        combined.sum += bucket.sum;
        combined.passes += bucket.passes;
        if bucket.samples != 0 {
            combined.worst = if combined.samples == 0 {
                bucket.worst
            } else {
                combined.worst.min(bucket.worst)
            };
            combined.samples += bucket.samples;
        }
    }
    combined
}
fn drift_report(totals: &Totals) -> DriftReport {
    let buckets = [
        totals.initial_drift,
        totals.pre_periodic_drift,
        totals.post_periodic_drift,
        totals.final_predelete_drift,
    ];
    DriftReport {
        initial: drift_metric(totals.initial_drift),
        pre_periodic: drift_metric(totals.pre_periodic_drift),
        post_periodic: drift_metric(totals.post_periodic_drift),
        final_predelete: drift_metric(totals.final_predelete_drift),
        combined: drift_metric(combined_drift(&buckets)),
    }
}
fn timing_rates(totals: &Totals, trials: usize) -> TimingRates {
    TimingRates {
        trial_setup_ns_per_trial: nanos_per(totals.timing.trial_setup_ns, trials),
        replacement_binding_ns_per_operation: nanos_per(
            totals.timing.replacement_binding_ns,
            totals.replacements,
        ),
        bundle_mutation_ns_per_operation: nanos_per(
            totals.timing.bundle_mutation_ns,
            totals.replacements,
        ),
        exact_mutation_ns_per_operation: nanos_per(
            totals.timing.exact_mutation_ns,
            totals.replacements,
        ),
        stale_probe_ns_per_operation: nanos_per(totals.timing.stale_probe_ns, totals.replacements),
        initial_reference_rebuild_count: totals.initial_reference_rebuilds,
        initial_reference_rebuild_ns_per_rebuild: nanos_per(
            totals.timing.initial_reference_rebuild_ns,
            totals.initial_reference_rebuilds,
        ),
        pre_periodic_reference_rebuild_count: totals.pre_periodic_reference_rebuilds,
        pre_periodic_reference_rebuild_ns_per_rebuild: nanos_per(
            totals.timing.pre_periodic_reference_rebuild_ns,
            totals.pre_periodic_reference_rebuilds,
        ),
        periodic_accumulator_rebuild_count: totals.rebuilds,
        periodic_accumulator_rebuild_ns_per_rebuild: nanos_per(
            totals.timing.periodic_accumulator_rebuild_ns,
            totals.rebuilds,
        ),
        final_predelete_reference_rebuild_count: totals.final_predelete_reference_rebuilds,
        final_predelete_reference_rebuild_ns_per_rebuild: nanos_per(
            totals.timing.final_predelete_reference_rebuild_ns,
            totals.final_predelete_reference_rebuilds,
        ),
        postdelete_reference_rebuild_count: totals.postdelete_reference_rebuilds,
        postdelete_reference_rebuild_ns_per_rebuild: nanos_per(
            totals.timing.postdelete_reference_rebuild_ns,
            totals.postdelete_reference_rebuilds,
        ),
        deletion_ns_per_node: nanos_per(totals.timing.deletion_ns, totals.deleted_nodes),
        clean_cleanup_ns_per_query: nanos_per(totals.timing.clean_cleanup_ns, totals.queries),
        incremental_cleanup_ns_per_query: nanos_per(
            totals.timing.incremental_cleanup_ns,
            totals.queries,
        ),
        exact_query_ns_per_query: nanos_per(totals.timing.exact_query_ns, totals.queries),
    }
}

fn emit_machine_output(path: Option<&Path>, bytes: &[u8]) -> Result<(), String> {
    match path {
        Some(path) => {
            match atomic_write(path, bytes)? {
                PublicationOutcome::PublishedDurable => {
                    eprintln!("wrote {}", path.display());
                }
                PublicationOutcome::PublishedDurabilityUnconfirmed(error) => {
                    eprintln!(
                        "wrote {}, but parent-directory durability confirmation failed: {error}",
                        path.display()
                    );
                }
            }
            Ok(())
        }
        None => {
            let stdout = io::stdout();
            let mut output = stdout.lock();
            output.write_all(bytes).map_err(|e| e.to_string())?;
            output.flush().map_err(|e| e.to_string())
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
enum PublicationOutcome {
    PublishedDurable,
    PublishedDurabilityUnconfirmed(String),
}
#[derive(Debug, PartialEq, Eq)]
struct PublicationError(String);
impl std::fmt::Display for PublicationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pre-publication failure: {}", self.0)
    }
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<PublicationOutcome, String> {
    atomic_write_with(
        path,
        |file| file.write_all(bytes),
        |parent| {
            #[cfg(unix)]
            File::open(parent)?.sync_all()?;
            Ok(())
        },
    )
    .map_err(|e| format!("atomically write {}: {e}", path.display()))
}
fn atomic_write_with(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
    after_rename: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<PublicationOutcome, PublicationError> {
    let filename = path
        .file_name()
        .ok_or_else(|| PublicationError(format!("output path {path:?} has no filename")))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| {
        PublicationError(format!("create output directory {}: {e}", parent.display()))
    })?;
    let mut reserved = None;
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
                reserved = Some((candidate, file));
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(PublicationError(format!(
                    "create temporary output beside {}: {e}",
                    path.display()
                )));
            }
        }
    }
    let (temp, mut file) = reserved.ok_or_else(|| {
        PublicationError(format!(
            "could not reserve a temporary output beside {}",
            path.display()
        ))
    })?;
    let before = (|| -> io::Result<()> {
        write(&mut file)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if let Err(e) = before {
        let _ = fs::remove_file(&temp);
        return Err(PublicationError(e.to_string()));
    }
    Ok(match after_rename(parent) {
        Ok(()) => PublicationOutcome::PublishedDurable,
        Err(error) => PublicationOutcome::PublishedDurabilityUnconfirmed(error.to_string()),
    })
}
fn print_human(report: &StudyReport) {
    println!(
        "FHRR derived-relation lifecycle study (schema v{})",
        report.schema_version
    );
    println!(
        "machine={} · seed={} · safety ceiling={}",
        report.configuration.machine_label,
        report.configuration.seed,
        report.configuration.workload_ceiling
    );
    for row in &report.results {
        println!(
            "dim={} nodes={} degree={} ops={} every={} clean@1={:.3} incremental@1={:.3} drift={:.5} canonical={} B",
            row.fingerprint.dimension,
            row.node_count,
            row.degree,
            row.operations_per_trial,
            row.rebuild_every,
            row.clean_top1_recall,
            row.incremental_top1_recall,
            row.drift.combined.cosine_worst,
            row.canonical_total_payload_bytes
        );
    }
    println!("\n{ACCOUNTING}");
}
fn print_machine_summary(report: &StudyReport) {
    eprintln!(
        "mneme-vsa lifecycle study: {} configurations; oracle control excluded from timings",
        report.results.len()
    );
}
fn print_help() {
    println!(
        "mneme-vsa FHRR lifecycle/churn study\n\nUsage: lifecycle-study [OPTIONS]\n\nOptions:\n  --dimensions LIST\n  --nodes LIST\n  --degrees LIST\n  --distractors LIST\n  --operations LIST\n  --rebuild-every LIST (0 means never)\n  --deleted-node-percent N\n  --trials N\n  --seed N\n  --machine-label TEXT\n  --format human|json\n  --output PATH\n\nAdmission uses a 100,000,000-unit conservative synthetic work model plus independent operation, trace, dictionary, dimension, and peak-payload caps."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tiny(operations: usize, cadence: usize, deleted: usize) -> StudyResult {
        study_configuration(StudyInput {
            dimension: 256,
            nodes: 8,
            degree: 1,
            distractors: 0,
            operations,
            rebuild_every: cadence,
            deleted_percent: deleted,
            trials: 1,
            seed: 7,
        })
        .unwrap()
    }
    #[test]
    fn zero_operation_tiny_study_agrees_and_recalls_every_relation() {
        let row = tiny(0, 0, 0);
        assert_eq!(
            (row.clean_top1_correct, row.incremental_top1_correct),
            (8, 8)
        );
        assert_eq!(row.live_adjacency_audits, 3);
        assert!(row.live_adjacency_correct);
        assert_eq!(row.target_provenance_payload_bytes, 16);
        assert_eq!(row.live_bitmap_payload_bytes, 1);
        assert_eq!(row.canonical_total_payload_bytes, 17);
        assert_eq!(
            row.deployable_incremental_payload_bytes,
            row.incremental_derived_lookup_payload_bytes + row.canonical_total_payload_bytes
        );
        assert_eq!(row.timing_rates.initial_reference_rebuild_count, 1);
        assert_eq!(row.timing_rates.final_predelete_reference_rebuild_count, 1);
        assert_eq!(row.timing_rates.postdelete_reference_rebuild_count, 1);
        assert_eq!(row.clean_first_queries, 4);
        assert_eq!(row.incremental_first_queries, 4);
    }
    #[test]
    fn hard_coded_trace_and_oracle_fault_are_detected() {
        let oracle = OracleState::initial(4, 1).unwrap();
        let trace = plan_replacements(&oracle, 6, 7).unwrap();
        assert_eq!(
            trace,
            vec![
                PlannedReplace {
                    source: 2,
                    role: 0,
                    old_target: 3,
                    new_target: 0,
                },
                PlannedReplace {
                    source: 3,
                    role: 0,
                    old_target: 0,
                    new_target: 2,
                },
                PlannedReplace {
                    source: 0,
                    role: 0,
                    old_target: 1,
                    new_target: 3,
                },
                PlannedReplace {
                    source: 0,
                    role: 0,
                    old_target: 3,
                    new_target: 2,
                },
                PlannedReplace {
                    source: 3,
                    role: 0,
                    old_target: 2,
                    new_target: 1,
                },
                PlannedReplace {
                    source: 0,
                    role: 0,
                    old_target: 2,
                    new_target: 3,
                },
            ]
        );
        let mut live = LiveAdjacency::initial(4, 1).unwrap();
        live.targets[0] = 2;
        let mut totals = Totals {
            audit_ok: true,
            ..Totals::default()
        };
        assert!(audit_live(&live, &oracle, &mut totals).is_err());
        assert!(!totals.audit_ok);
    }
    #[test]
    fn canonical_deletion_clears_live_bit_but_retains_target_provenance() {
        let mut oracle = OracleState::initial(4, 2).unwrap();
        let mut live = LiveAdjacency::initial(4, 2).unwrap();
        let old = oracle.target(1, 0);
        oracle.delete_source(1);
        live.delete_source(1);
        assert!(!oracle.is_live(1, 0));
        assert_eq!(oracle.target(1, 0), old);
        assert_eq!(live.targets, oracle.targets);
        assert_eq!(live.live, oracle.live);
    }
    #[test]
    fn failed_replacement_staging_leaves_bundle_and_live_state_unchanged() {
        let fhrr = Fhrr::new(32, 1).unwrap();
        let incompatible_fhrr = Fhrr::new(32, 2).unwrap();
        let roles = vec![fhrr.atom("role:0")];
        let nodes: Vec<_> = (0..4)
            .map(|node| fhrr.atom(&format!("node:{node}")))
            .collect();
        let oracle = OracleState::initial(4, 1).unwrap();
        let live = LiveAdjacency::initial(4, 1).unwrap();
        let mut bundle = build_live_accumulators(&fhrr, &roles, &nodes, &live).unwrap()[0].clone();
        let op = PlannedReplace {
            source: 0,
            role: 0,
            old_target: 1,
            new_target: 2,
        };
        live.validate_replace(&op).unwrap();
        oracle.validate_replace(&op).unwrap();
        let old = fhrr.bind(&roles[0], &nodes[1]).unwrap();
        let incompatible_new = incompatible_fhrr.atom("incompatible");
        let before_bundle = bundle.clone();
        let before_targets = live.targets.clone();
        let before_bits = live.live.clone();
        assert!(matches!(
            bundle.replace_known_term(&old, &incompatible_new),
            Err(Error::FingerprintMismatch { .. })
        ));
        assert_eq!(bundle, before_bundle);
        assert_eq!(live.targets, before_targets);
        assert_eq!(live.live, before_bits);
    }
    #[test]
    fn exact_cadence_counts_are_honored() {
        for (cadence, expected) in [(0, 0), (1, 10), (2, 5), (5, 2), (6, 1)] {
            let row = tiny(10, cadence, 0);
            assert_eq!(row.rebuild_count, expected);
            assert_eq!(row.drift.initial.passes, 1);
            assert_eq!(row.drift.pre_periodic.passes, expected);
            assert_eq!(row.drift.post_periodic.passes, expected);
            assert_eq!(row.drift.final_predelete.passes, 1);
            assert_eq!(row.live_adjacency_audits, 10 + expected + 3);
            assert_eq!(
                row.timing_rates.pre_periodic_reference_rebuild_count,
                expected
            );
            assert_eq!(
                row.timing_rates.periodic_accumulator_rebuild_count,
                expected
            );
        }
    }
    #[test]
    fn rebuilding_the_same_oracle_state_is_deterministic() {
        let fhrr = Fhrr::new(64, 9).unwrap();
        let roles: Vec<_> = (0..2)
            .map(|role| fhrr.atom(&format!("role:{role}")))
            .collect();
        let nodes: Vec<_> = (0..6)
            .map(|node| fhrr.atom(&format!("node:{node}")))
            .collect();
        let oracle = OracleState::initial(6, 2).unwrap();
        assert_eq!(
            build_oracle_references(&fhrr, &roles, &nodes, &oracle).unwrap(),
            build_oracle_references(&fhrr, &roles, &nodes, &oracle).unwrap()
        );
    }
    #[test]
    fn stale_target_metric_names_the_old_target() {
        let fhrr = Fhrr::new(64, 1).unwrap();
        let old = fhrr.atom("old");
        let new = fhrr.atom("new");
        let hits = fhrr
            .cleanup(&old, [("old", &old), ("new", &new)], 1)
            .unwrap();
        assert!(stale_old_target_hit(&hits, "old"));
        assert!(!stale_old_target_hit(&hits, "new"));

        let row = study_configuration(StudyInput {
            dimension: 1,
            nodes: 4,
            degree: 2,
            distractors: 0,
            operations: 32,
            rebuild_every: 0,
            deleted_percent: 0,
            trials: 1,
            seed: 7,
        })
        .unwrap();
        assert_eq!(row.stale_old_target_top1_count, 2);
        assert_eq!(row.replacement_operations, 32);
        assert_eq!(row.stale_old_target_top1_rate, 2.0 / 32.0);
    }
    #[test]
    fn empty_state_is_classified_and_never_cleaned() {
        let row = tiny(4, 0, 100);
        assert_eq!(row.expected_empty_estimates, 8);
        assert_eq!(row.empty_node_abstentions, 8);
        assert_eq!(row.unexpected_empty_estimates, 0);
        assert_eq!(row.empty_estimate_other_errors, 0);
        assert_eq!(row.empty_node_cleanup_attempts, 0);

        let fhrr = Fhrr::new(8, 3).unwrap();
        let empty = MutableBundle::new(&fhrr);
        let mut totals = Totals::default();
        assert!(mutable_estimate_for_cleanup(&empty, &mut totals).is_err());
        assert_eq!(totals.empty_cleanup_attempts, 1);
    }
    #[test]
    fn deterministic_json_removes_explicit_timing_object() {
        let first = tiny(12, 3, 25);
        let second = tiny(12, 3, 25);
        let mut a = serde_json::to_value(first).unwrap();
        let mut b = serde_json::to_value(second).unwrap();
        a.as_object_mut().unwrap().remove("timing");
        a.as_object_mut().unwrap().remove("timing_rates");
        b.as_object_mut().unwrap().remove("timing");
        b.as_object_mut().unwrap().remove("timing_rates");
        assert_eq!(a, b);
    }
    #[test]
    fn workload_rejects_luna_counterexample_and_sums_grid() {
        let c = Config {
            dimensions: vec![100_000],
            nodes: vec![3],
            degrees: vec![1],
            distractors: vec![0],
            operations: vec![32_000_000],
            rebuild_every: vec![0],
            ..Config::default()
        };
        assert!(validate_config(&c).is_err());
        let one = grid_workload(&Config {
            dimensions: vec![8],
            nodes: vec![4],
            degrees: vec![1],
            distractors: vec![0],
            operations: vec![0],
            rebuild_every: vec![0],
            deleted_node_percent: 0,
            trials: 1,
            ..Config::default()
        })
        .unwrap();
        let two = grid_workload(&Config {
            dimensions: vec![8, 16],
            nodes: vec![4],
            degrees: vec![1],
            distractors: vec![0],
            operations: vec![0],
            rebuild_every: vec![0],
            deleted_node_percent: 0,
            trials: 1,
            ..Config::default()
        })
        .unwrap();
        let second = grid_workload(&Config {
            dimensions: vec![16],
            nodes: vec![4],
            degrees: vec![1],
            distractors: vec![0],
            operations: vec![0],
            rebuild_every: vec![0],
            deleted_node_percent: 0,
            trials: 1,
            ..Config::default()
        })
        .unwrap();
        assert_eq!(two, one + second);

        let smoke = grid_workload(&Config {
            dimensions: vec![128],
            nodes: vec![64],
            degrees: vec![4],
            distractors: vec![16],
            operations: vec![128],
            rebuild_every: vec![0, 32],
            deleted_node_percent: 25,
            trials: 2,
            ..Config::default()
        })
        .unwrap();
        assert_eq!(smoke, 78_138_368);
    }
    #[test]
    fn workload_math_rejects_overflow() {
        assert!(checked_u128_mul(u128::MAX, 2, "test").is_err());
        assert!(
            validate_config(&Config {
                operations: vec![MAX_OPERATIONS_PER_TRIAL + 1],
                ..Config::default()
            })
            .is_err()
        );
        assert!(
            validate_config(&Config {
                nodes: vec![4],
                degrees: vec![1],
                distractors: vec![MAX_DICTIONARY_VECTORS],
                operations: vec![0],
                rebuild_every: vec![0],
                ..Config::default()
            })
            .is_err()
        );
        assert!(
            validate_config(&Config {
                dimensions: vec![1_048_577],
                nodes: vec![4],
                degrees: vec![1],
                operations: vec![0],
                rebuild_every: vec![0],
                ..Config::default()
            })
            .is_err()
        );
    }
    #[test]
    fn atomic_publication_distinguishes_pre_and_post_rename_failures() {
        let dir =
            env::temp_dir().join(format!("mneme-vsa-lifecycle-output-{}", std::process::id()));
        let path = dir.join("result.json");
        assert_eq!(
            atomic_write(&path, b"old\n").unwrap(),
            PublicationOutcome::PublishedDurable
        );
        assert!(matches!(
            atomic_write_with(&path, |_| Err(io::Error::other("injected")), |_| Ok(())),
            Err(PublicationError(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"old\n");
        assert!(matches!(
            atomic_write_with(
                &path,
                |f| f.write_all(b"new\n"),
                |_| Err(io::Error::other("directory sync injected"))
            ),
            Ok(PublicationOutcome::PublishedDurabilityUnconfirmed(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"new\n");
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
