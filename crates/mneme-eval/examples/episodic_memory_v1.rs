//! Isolated, authored episodic acceptance: no providers, network or live stores.
#[path = "episodic_memory_v1/actor.rs"]
mod actor;
#[path = "episodic_memory_v1/fixture.rs"]
mod fixture;
#[path = "episodic_memory_v1/rehearsal.rs"]
mod rehearsal;
#[cfg(test)]
#[path = "episodic_memory_v1/tests.rs"]
mod tests;

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

#[tokio::main]
async fn main() {
    match run().await {
        Ok(result) => println!("{}", serde_json::to_string(&result).unwrap()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<Value> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["--rehearse"] => rehearsal::run().await,
        ["--init", dir, case] => actor::initialize(Path::new(dir), case),
        ["--request", dir, request] => {
            require(request.len() <= actor::MAX_REQUEST_BYTES, "request exceeds 4096 bytes")?;
            actor::request(Path::new(dir), serde_json::from_str(request)?).await
        }
        ["--finish", dir, packet] => actor::finish(Path::new(dir), serde_json::from_str(packet)?),
        ["--artifact", dir] => actor::artifact(Path::new(dir)),
        _ => Err("usage: episodic_memory_v1 --rehearse | --init NEW_SESSION_DIR CASE | --request SESSION_DIR JSON | --finish SESSION_DIR JSON | --artifact SESSION_DIR; init/artifact are coordinator-only".into()),
    }
}
