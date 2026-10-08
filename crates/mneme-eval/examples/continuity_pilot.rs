//! A bounded continuity episode pilot. No providers, live stores, or production changes.
#[path = "continuity_pilot/environment.rs"]
mod environment;
#[path = "continuity_pilot/investigation.rs"]
mod investigation;
#[path = "continuity_pilot/memory.rs"]
mod memory;
#[path = "continuity_pilot/session.rs"]
mod session;
#[cfg(test)]
#[path = "continuity_pilot/tests.rs"]
mod tests;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, Write};

fn digest(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}
fn fixture(episode: &str) -> Result<environment::Fixture, String> {
    let text = match episode {
        "diagnostics" => include_str!("../fixtures/continuity/diagnostics.json"),
        "async" => include_str!("../fixtures/continuity/async.json"),
        _ => return Err("episode must be diagnostics or async".into()),
    };
    serde_json::from_str(text).map_err(|e| e.to_string())
}
fn authorship(episode: &str) -> Value {
    serde_json::from_str(if episode == "diagnostics" {
        include_str!("../fixtures/continuity/diagnostics-authorship.json")
    } else {
        include_str!("../fixtures/continuity/async-authorship.json")
    })
    .unwrap()
}
fn assessment() -> Value {
    serde_json::from_str(include_str!("../fixtures/continuity/assessment.json")).unwrap()
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--rehearse") {
        let artifact = if args.iter().any(|a| a == "--investigation") {
            investigation::rehearse().await?
        } else {
            session::rehearse().await?
        };
        println!("{}", serde_json::to_string_pretty(&artifact).unwrap());
        return Ok(());
    }
    if !args.iter().any(|a| a == "--session" || a == "--replay") {
        return Err("usage: continuity_pilot --rehearse --json | --session --episode diagnostics|async --condition notes_search|hybrid_graph_off|hybrid_authored_graph [--independent-authorship] | --replay FILE [same options]; replay input is JSONL, output is ONLY the final response; add --investigation for v2 with conditions empty_memory|notes_search (natural authoring enabled for notes)".into());
    }
    let value = |flag: &str, default: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or(default.into())
    };
    let mut session = if args.iter().any(|a| a == "--investigation") {
        session::Session::new_investigation(
            &value("--episode", "diagnostics"),
            &value("--condition", "notes_search"),
            "real_actor",
        )?
    } else {
        session::Session::new(
            &value("--episode", "diagnostics"),
            &value("--condition", "notes_search"),
            args.iter().any(|a| a == "--independent-authorship"),
            "real_actor",
        )?
    };
    if args.iter().any(|a| a == "--replay") {
        let path = value("--replay", "");
        let contents = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let mut last = json!({"ok":false,"error":"empty replay"});
        for line in contents.lines().filter(|s| !s.trim().is_empty()) {
            let request: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
            last = session.request(request).await;
        }
        println!("{last}");
    } else {
        for line in std::io::stdin().lock().lines() {
            let request: Value = match serde_json::from_str(&line.map_err(|e| e.to_string())?) {
                Ok(v) => v,
                Err(e) => {
                    println!(
                        "{}",
                        json!({"ok":false,"error":format!("invalid JSON: {e}")})
                    );
                    continue;
                }
            };
            println!("{}", session.request(request).await);
            std::io::stdout().flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
