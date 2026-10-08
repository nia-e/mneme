//! Read-only post-admission recall-selection comparison. `acquire` is the only
//! store operation; replay and provider preparation consume a frozen pool.
#[path = "recall_selection/acquire.rs"]
mod acquire;
#[path = "recall_selection/policy.rs"]
mod policy;
#[path = "recall_selection/pool.rs"]
mod pool;
#[path = "recall_selection/ranking.rs"]
mod ranking;

use std::path::Path;

fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

fn write<T: serde::Serialize>(path: &str, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("{path}: {e}"))
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("recall_selection: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["acquire", input, output] => {
            let pool = acquire::acquire(&read(input)?).await?;
            write(output, &pool)
        }
        ["replay", input, output] => {
            let pool: pool::Pool = serde_json::from_str(&read(input)?).map_err(|e| e.to_string())?;
            let (runs, _) = policy::replay(&pool, false, None, false).await?;
            write(output, &runs)
        }
        ["replay", input, output, "--caps", caps_file] => {
            let pool: pool::Pool = serde_json::from_str(&read(input)?).map_err(|e| e.to_string())?;
            let caps_json: serde_json::Value = serde_json::from_str(&read(caps_file)?).map_err(|e| e.to_string())?;
            let caps: policy::Caps = serde_json::from_value(caps_json.get("caps").unwrap_or(&caps_json).clone())
                .map_err(|e| e.to_string())?;
            let (runs, _) = policy::replay(&pool, false, Some(caps), false).await?;
            write(output, &runs)
        }
        ["cap-sweep", input, output] => {
            let pool: pool::Pool = serde_json::from_str(&read(input)?).map_err(|e| e.to_string())?;
            let (runs, _) = policy::replay(&pool, false, None, true).await?;
            write(output, &runs)
        }
        ["prepare-jev", input, output] => {
            let pool: pool::Pool = serde_json::from_str(&read(input)?).map_err(|e| e.to_string())?;
            let (_, requests) = policy::replay(&pool, true, None, false).await?;
            write(output, &requests.ok_or("Jev request preparation failed")?)
        }
        ["consume-jev", pool_file, request_file, receipt_file, output] => {
            let pool: pool::Pool = serde_json::from_str(&read(pool_file)?).map_err(|e| e.to_string())?;
            let requests: policy::RequestFile = serde_json::from_str(&read(request_file)?).map_err(|e| e.to_string())?;
            let receipts: serde_json::Value = serde_json::from_str(&read(receipt_file)?).map_err(|e| e.to_string())?;
            write(output, &policy::consume(&pool, &requests, &receipts)?)
        }
        _ => Err("usage: recall_selection acquire INPUTS.json POOL.json | cap-sweep DEVPOOL.json RUNS.json | replay POOL.json RUNS.json [--caps FROZEN-CAPS.json] | prepare-jev POOL.json REQUESTS.json | consume-jev POOL.json REQUESTS.json RECEIPTS.json RUNS.json".into()),
    }
}
