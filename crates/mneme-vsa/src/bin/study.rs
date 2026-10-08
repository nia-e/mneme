use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mneme_vsa::{Fhrr, Fingerprint, Vector};
use serde::Serialize;

const REPORT_SCHEMA_VERSION: u16 = 1;
const STUDY_KIND: &str = "mneme-vsa-derived-index-capacity-study";
const COST_WARNING: &str = "A hologram is a rebuildable derived index, not canonical storage. Raw payload bytes exclude labels and allocation metadata; cleanup and role codebooks are reported separately and are not free.";
const RUNTIME_SCOPE: &str = "Total runtime includes deterministic atom/codebook generation, binding, superposition, conjugate unbinding, and exhaustive cosine cleanup; build and cleanup phases are also reported separately.";
const TRIAL_PROTOCOL: &str = "Each trial creates fresh deterministic roles and fillers, bundles every role-filler pair, queries every role, and ranks all true fillers plus the configured independent distractors.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputFormat {
    Human,
    Json,
    Csv,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Config {
    dimensions: Vec<usize>,
    pair_counts: Vec<usize>,
    distractor_counts: Vec<usize>,
    trials: usize,
    top_k: usize,
    seed: u64,
    format: OutputFormat,
    output: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dimensions: vec![128, 256, 512],
            pair_counts: vec![1, 4, 8, 16],
            distractor_counts: vec![0, 16, 64, 256],
            trials: 32,
            top_k: 5,
            seed: 0x4d4e_454d_4556_5341,
            format: OutputFormat::Human,
            output: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct StudyReport {
    schema_version: u16,
    kind: &'static str,
    cost_warning: &'static str,
    runtime_scope: &'static str,
    trial_protocol: &'static str,
    seed: u64,
    trials_per_configuration: usize,
    requested_top_k: usize,
    results: Vec<StudyResult>,
}

#[derive(Debug, Serialize)]
struct StudyResult {
    fingerprint: Fingerprint,
    bundled_pairs: usize,
    cleanup_distractors: usize,
    cleanup_dictionary_vectors: usize,
    role_codebook_vectors: usize,
    total_codebook_vectors: usize,
    trials: usize,
    queries: usize,
    requested_top_k: usize,
    effective_top_k: usize,
    top1_correct: usize,
    top_k_correct: usize,
    top1_recall: f64,
    top_k_recall: f64,
    hologram_payload_bytes: usize,
    cleanup_dictionary_payload_bytes: usize,
    total_codebook_payload_bytes: usize,
    derived_lookup_payload_bytes: usize,
    build_runtime_ns: u64,
    cleanup_runtime_ns: u64,
    total_runtime_ns: u64,
    runtime_ns_per_query: f64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("study: {error}");
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
        OutputFormat::Csv => {
            print_machine_summary(&report);
            let mut bytes = Vec::new();
            write_csv(&mut bytes, &report)?;
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
            "--pairs" => {
                config.pair_counts = parse_list(&value(&mut args)?, "pairs", false)?;
            }
            "--distractors" => {
                config.distractor_counts = parse_list(&value(&mut args)?, "distractors", true)?;
            }
            "--trials" => {
                config.trials = parse_positive(&value(&mut args)?, "trials")?;
            }
            "--top-k" => {
                config.top_k = parse_positive(&value(&mut args)?, "top-k")?;
            }
            "--seed" => {
                config.seed = value(&mut args)?
                    .parse()
                    .map_err(|_| "seed must be an unsigned decimal integer".to_owned())?;
            }
            "--format" => {
                config.format = match value(&mut args)?.as_str() {
                    "human" => OutputFormat::Human,
                    "json" => OutputFormat::Json,
                    "csv" => OutputFormat::Csv,
                    other => {
                        return Err(format!(
                            "unknown format {other:?}; expected human, json, or csv"
                        ));
                    }
                };
            }
            "--output" => config.output = Some(PathBuf::from(value(&mut args)?)),
            other => return Err(format!("unknown argument {other:?}; use --help")),
        }
    }
    if config.output.is_some() && config.format == OutputFormat::Human {
        return Err("--output requires --format json or --format csv".into());
    }
    Ok(Some(config))
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
    let configurations = config
        .dimensions
        .len()
        .checked_mul(config.pair_counts.len())
        .and_then(|n| n.checked_mul(config.distractor_counts.len()))
        .ok_or_else(|| "configuration count overflow".to_owned())?;
    let mut results = Vec::with_capacity(configurations);
    for &dimension in &config.dimensions {
        for &pairs in &config.pair_counts {
            for &distractors in &config.distractor_counts {
                results.push(study_configuration(
                    dimension,
                    pairs,
                    distractors,
                    config.trials,
                    config.top_k,
                    config.seed,
                )?);
            }
        }
    }
    Ok(StudyReport {
        schema_version: REPORT_SCHEMA_VERSION,
        kind: STUDY_KIND,
        cost_warning: COST_WARNING,
        runtime_scope: RUNTIME_SCOPE,
        trial_protocol: TRIAL_PROTOCOL,
        seed: config.seed,
        trials_per_configuration: config.trials,
        requested_top_k: config.top_k,
        results,
    })
}

fn study_configuration(
    dimension: usize,
    pairs: usize,
    distractors: usize,
    trials: usize,
    top_k: usize,
    seed: u64,
) -> Result<StudyResult, String> {
    let fhrr = Fhrr::new(dimension, seed).map_err(|e| e.to_string())?;
    let dictionary_size = pairs
        .checked_add(distractors)
        .ok_or_else(|| "cleanup dictionary size overflow".to_owned())?;
    let effective_top_k = top_k.min(dictionary_size);
    let queries = trials
        .checked_mul(pairs)
        .ok_or_else(|| "query count overflow".to_owned())?;
    let mut top1_correct = 0_usize;
    let mut top_k_correct = 0_usize;
    let mut build_runtime = Duration::ZERO;
    let mut cleanup_runtime = Duration::ZERO;
    let total_start = Instant::now();

    for trial in 0..trials {
        let build_start = Instant::now();
        let role_labels: Vec<_> = (0..pairs)
            .map(|index| format!("trial:{trial}:role:{index}"))
            .collect();
        let filler_labels: Vec<_> = (0..pairs)
            .map(|index| format!("trial:{trial}:filler:{index}"))
            .collect();
        let distractor_labels: Vec<_> = (0..distractors)
            .map(|index| format!("trial:{trial}:distractor:{index}"))
            .collect();
        let roles: Vec<_> = role_labels.iter().map(|label| fhrr.atom(label)).collect();
        let fillers: Vec<_> = filler_labels.iter().map(|label| fhrr.atom(label)).collect();
        let distractor_vectors: Vec<_> = distractor_labels
            .iter()
            .map(|label| fhrr.atom(label))
            .collect();
        let bound_pairs: Vec<_> = roles
            .iter()
            .zip(&fillers)
            .map(|(role, filler)| fhrr.bind(role, filler))
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        let hologram = fhrr.superpose(&bound_pairs).map_err(|e| e.to_string())?;
        build_runtime += build_start.elapsed();

        let cleanup_start = Instant::now();
        for (target, role) in roles.iter().enumerate() {
            let estimate = fhrr.unbind(&hologram, role).map_err(|e| e.to_string())?;
            let dictionary = dictionary(
                &filler_labels,
                &fillers,
                &distractor_labels,
                &distractor_vectors,
            );
            let hits = fhrr
                .cleanup(&estimate, dictionary, effective_top_k)
                .map_err(|e| e.to_string())?;
            let correct = filler_labels[target].as_str();
            if hits.first().is_some_and(|hit| hit.label == correct) {
                top1_correct += 1;
            }
            if hits.iter().any(|hit| hit.label == correct) {
                top_k_correct += 1;
            }
        }
        cleanup_runtime += cleanup_start.elapsed();
    }
    let total_runtime = total_start.elapsed();

    let hologram_payload_bytes = fhrr.payload_bytes();
    let role_codebook_vectors = pairs;
    let total_codebook_vectors = role_codebook_vectors
        .checked_add(dictionary_size)
        .ok_or_else(|| "total codebook size overflow".to_owned())?;
    let cleanup_dictionary_payload_bytes = hologram_payload_bytes
        .checked_mul(dictionary_size)
        .ok_or_else(|| "cleanup dictionary byte size overflow".to_owned())?;
    let total_codebook_payload_bytes =
        hologram_payload_bytes
            .checked_mul(total_codebook_vectors)
            .ok_or_else(|| "total codebook byte size overflow".to_owned())?;
    let derived_lookup_payload_bytes = hologram_payload_bytes
        .checked_add(total_codebook_payload_bytes)
        .ok_or_else(|| "derived lookup byte size overflow".to_owned())?;
    let denominator = queries as f64;

    Ok(StudyResult {
        fingerprint: fhrr.fingerprint().clone(),
        bundled_pairs: pairs,
        cleanup_distractors: distractors,
        cleanup_dictionary_vectors: dictionary_size,
        role_codebook_vectors,
        total_codebook_vectors,
        trials,
        queries,
        requested_top_k: top_k,
        effective_top_k,
        top1_correct,
        top_k_correct,
        top1_recall: top1_correct as f64 / denominator,
        top_k_recall: top_k_correct as f64 / denominator,
        hologram_payload_bytes,
        cleanup_dictionary_payload_bytes,
        total_codebook_payload_bytes,
        derived_lookup_payload_bytes,
        build_runtime_ns: nanos_u64(build_runtime),
        cleanup_runtime_ns: nanos_u64(cleanup_runtime),
        total_runtime_ns: nanos_u64(total_runtime),
        runtime_ns_per_query: total_runtime.as_nanos() as f64 / denominator,
    })
}

fn dictionary<'a>(
    filler_labels: &'a [String],
    fillers: &'a [Vector],
    distractor_labels: &'a [String],
    distractors: &'a [Vector],
) -> impl Iterator<Item = (&'a str, &'a Vector)> {
    filler_labels
        .iter()
        .zip(fillers)
        .map(|(label, vector)| (label.as_str(), vector))
        .chain(
            distractor_labels
                .iter()
                .zip(distractors)
                .map(|(label, vector)| (label.as_str(), vector)),
        )
}

fn nanos_u64(duration: Duration) -> u64 {
    duration.as_nanos().try_into().unwrap_or(u64::MAX)
}

fn print_human(report: &StudyReport) {
    println!("FHRR derived-index capacity study");
    println!(
        "seed={} · trials/config={} · requested top-k={}",
        report.seed, report.trials_per_configuration, report.requested_top_k
    );
    println!(
        "{:>5} {:>5} {:>6} {:>8} {:>8} {:>9} {:>11} {:>10}",
        "dim", "pairs", "noise", "top-1", "top-k", "holo KiB", "codebook KiB", "ms/query"
    );
    for row in &report.results {
        println!(
            "{:>5} {:>5} {:>6} {:>8.3} {:>8.3} {:>9.1} {:>11.1} {:>10.3}",
            row.fingerprint.dimension,
            row.bundled_pairs,
            row.cleanup_distractors,
            row.top1_recall,
            row.top_k_recall,
            row.hologram_payload_bytes as f64 / 1024.0,
            row.total_codebook_payload_bytes as f64 / 1024.0,
            row.runtime_ns_per_query / 1_000_000.0,
        );
    }
    println!("\n{COST_WARNING}");
    println!("{RUNTIME_SCOPE}");
}

fn print_machine_summary(report: &StudyReport) {
    let queries: usize = report.results.iter().map(|row| row.queries).sum();
    eprintln!(
        "mneme-vsa study: {} configurations, {queries} queries; hologram and codebook payload bytes are reported separately (synthetic derived-index study)",
        report.results.len()
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

/// Write beside the destination, sync the complete file, then replace the
/// destination with one rename. A failed/interrupted write leaves the previous
/// artifact intact rather than publishing truncated JSON/CSV.
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

fn write_csv(out: &mut impl Write, report: &StudyReport) -> Result<(), String> {
    writeln!(
        out,
        "schema_version,kind,cost_warning,runtime_scope,trial_protocol,fingerprint_version,dimension,seed,operator,atom_generator,bundled_pairs,cleanup_distractors,cleanup_dictionary_vectors,role_codebook_vectors,total_codebook_vectors,trials,queries,requested_top_k,effective_top_k,top1_correct,top_k_correct,top1_recall,top_k_recall,hologram_payload_bytes,cleanup_dictionary_payload_bytes,total_codebook_payload_bytes,derived_lookup_payload_bytes,build_runtime_ns,cleanup_runtime_ns,total_runtime_ns,runtime_ns_per_query"
    )
    .map_err(|e| e.to_string())?;
    for row in &report.results {
        writeln!(
            out,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:.9},{:.9},{},{},{},{},{},{},{},{:.3}",
            report.schema_version,
            csv_string(report.kind),
            csv_string(report.cost_warning),
            csv_string(report.runtime_scope),
            csv_string(report.trial_protocol),
            row.fingerprint.version,
            row.fingerprint.dimension,
            row.fingerprint.seed,
            csv_string(&row.fingerprint.operator),
            csv_string(&row.fingerprint.atom_generator),
            row.bundled_pairs,
            row.cleanup_distractors,
            row.cleanup_dictionary_vectors,
            row.role_codebook_vectors,
            row.total_codebook_vectors,
            row.trials,
            row.queries,
            row.requested_top_k,
            row.effective_top_k,
            row.top1_correct,
            row.top_k_correct,
            row.top1_recall,
            row.top_k_recall,
            row.hologram_payload_bytes,
            row.cleanup_dictionary_payload_bytes,
            row.total_codebook_payload_bytes,
            row.derived_lookup_payload_bytes,
            row.build_runtime_ns,
            row.cleanup_runtime_ns,
            row.total_runtime_ns,
            row.runtime_ns_per_query,
        )
        .map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())
}

fn csv_string(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn print_help() {
    println!(
        "mneme-vsa FHRR capacity study\n\
         \n\
         Usage: study [OPTIONS]\n\
         \n\
         Options:\n\
           --dimensions LIST   Comma-separated dimensions [default: 128,256,512]\n\
           --pairs LIST        Bundled role-filler pair counts [default: 1,4,8,16]\n\
           --distractors LIST  Extra cleanup candidates [default: 0,16,64,256]\n\
           --trials N          Fixed trials per configuration [default: 32]\n\
           --top-k N           Cleanup recall cutoff [default: 5]\n\
           --seed N            Unsigned decimal atom-codebook seed\n\
           --format FORMAT     human, json, or csv [default: human]\n\
           --output PATH       Atomically retain JSON/CSV instead of stdout\n\
           -h, --help          Show this help\n\
         \n\
         JSON/CSV goes to stdout; its concise human summary goes to stderr."
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
                "--pairs",
                "4,1",
                "--distractors",
                "16,0",
                "--trials",
                "3",
                "--top-k",
                "2",
                "--seed",
                "42",
                "--format",
                "json",
                "--output",
                "results.json",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.dimensions, [64, 256]);
        assert_eq!(config.pair_counts, [1, 4]);
        assert_eq!(config.distractor_counts, [0, 16]);
        assert_eq!(config.trials, 3);
        assert_eq!(config.top_k, 2);
        assert_eq!(config.seed, 42);
        assert_eq!(config.format, OutputFormat::Json);
        assert_eq!(config.output, Some(PathBuf::from("results.json")));
    }

    #[test]
    fn rejects_zero_for_dimensions_pairs_trials_and_top_k() {
        for args in [
            vec!["--dimensions", "0"],
            vec!["--pairs", "0"],
            vec!["--trials", "0"],
            vec!["--top-k", "0"],
        ] {
            assert!(
                parse_args(args.into_iter().map(str::to_owned)).is_err(),
                "accepted a nonsensical zero-valued argument"
            );
        }
    }

    #[test]
    fn tiny_capacity_study_reports_costs_and_recall() {
        let row = study_configuration(256, 2, 8, 4, 3, 17).unwrap();
        assert_eq!(row.queries, 8);
        assert_eq!(row.hologram_payload_bytes, 256 * 2 * 4);
        assert_eq!(row.cleanup_dictionary_vectors, 10);
        assert_eq!(row.total_codebook_vectors, 12);
        assert_eq!(row.derived_lookup_payload_bytes, 13 * 256 * 2 * 4);
        assert_eq!(row.top1_correct, row.queries);
        assert_eq!(row.top_k_correct, row.queries);
    }

    #[test]
    fn csv_strings_quote_embedded_quotes() {
        assert_eq!(csv_string("a,\"b\""), "\"a,\"\"b\"\"\"");
    }

    #[test]
    fn output_is_atomic_and_replaces_a_complete_prior_artifact() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("mneme-vsa-output-{}-{unique}", std::process::id()));
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

    #[test]
    fn output_rejects_human_format() {
        let error =
            parse_args(["--output", "result.txt"].into_iter().map(str::to_owned)).unwrap_err();
        assert!(error.contains("requires --format json or --format csv"));
    }
}
