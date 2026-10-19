//! Owner-native shell over MCP walk/reflect. Browsing never trains; only explicit
//! `done ID…` obtains and spends a receipt. EOF/bare done abort without a receipt.
use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::cli_owner::{OwnerBinding, SelectedRemote};
use crate::remote_commands::{Request, RequestAccess};
use crate::remote_transport::RemoteClient as NativeRemoteClient;
use crate::{AnyErr, parse_id};

enum ReplClient {
    Legacy(NativeRemoteClient),
    Configured {
        client: mneme_store_client::ConfiguredClient,
        name: String,
        catalog: Vec<Value>,
    },
}
impl ReplClient {
    async fn connect(
        options: &crate::remote_transport::ConnectionOptions,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self::Legacy(NativeRemoteClient::connect(options).await?))
    }
    fn advertises(&self, tool: &str) -> bool {
        match self {
            Self::Legacy(client) => client.advertises(tool),
            Self::Configured { catalog, .. } => {
                mneme_mcp_client::CatalogView(catalog).advertises(tool)
            }
        }
    }
    async fn call_tool(
        &mut self,
        tool: &str,
        mut arguments: Value,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Legacy(client) => client.call_tool(tool, arguments).await,
            Self::Configured { client, name, .. } => {
                if arguments.get("db").is_some() {
                    arguments["db"] = json!(name);
                }
                let mut budget =
                    mneme_store_client::OperationBudget::new(1024 * 1024, Duration::from_secs(15))?;
                client
                    .call_owner_with_budget(name, tool, arguments, &mut budget)
                    .await
            }
        }
    }
    async fn close(&mut self) {
        match self {
            Self::Legacy(client) => client.close().await,
            Self::Configured { client, .. } => client.close().await,
        }
    }
}

pub(crate) async fn run_configured(
    snapshot: mneme_config::SelectionSnapshot,
    name: &str,
    json_output: bool,
    start: &str,
    budget: usize,
    query: Option<&str>,
) -> Result<(), AnyErr> {
    validate_start(start, budget, query)?;
    let selected_store = snapshot.explicit_store(name)?;
    selected_store.require_operation(mneme_config::Operation::ExplicitRead)?;
    let owner = &selected_store.store.owner;
    let selected = SelectedRemote {
        remote: crate::remote_config::ResolvedRemote {
            database: owner.db.clone(),
            connection: crate::remote_transport::ConnectionOptions {
                url: owner.url.clone(),
                ssh_mcp_port: owner.ssh_mcp_port.unwrap_or(18766),
                token_env: owner.token_env.clone(),
            },
        },
        owner: None,
        origin: crate::cli_owner::OwnerOrigin::ExplicitRemote,
    };
    let client = mneme_store_client::ConfiguredClient::for_explicit(snapshot, name)
        .map_err(|e| e.to_string())?;
    let mut allowance =
        mneme_store_client::OperationBudget::new(512 * 1024, Duration::from_secs(10))
            .map_err(|e| e.to_string())?;
    let catalog = client
        .catalog_owner_with_budget(name, &mut allowance)
        .await
        .map_err(|e| e.to_string())?;
    let mut client = ReplClient::Configured {
        client,
        name: name.to_owned(),
        catalog,
    };
    run_session(&mut client, &selected, json_output, start, budget, query).await
}

const MAX_LINE_BYTES: usize = 8192;
// Four stdin lines per server action envelope leaves room for help/blank lines
// without letting a pipe keep an abandoned session alive indefinitely.
const MAX_INPUT_LINES: usize = 4 * 256;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

fn validate_start(start: &str, budget: usize, query: Option<&str>) -> Result<(), AnyErr> {
    parse_id(start)?;
    if !(1..=64).contains(&budget) {
        return Err("remote repl --budget must be between 1 and 64".into());
    }
    if let Some(query) = query {
        if query.trim().is_empty() || query.len() > crate::MAX_CLI_QUERY_BYTES {
            return Err("remote repl query must be nonblank and at most 8192 UTF-8 bytes".into());
        }
    }
    Ok(())
}

/// Uses only the selected existing owner. Never opens a local store or retries.
pub(crate) async fn run(
    selected: SelectedRemote,
    json_output: bool,
    start: &str,
    budget: usize,
    query: Option<&str>,
) -> Result<(), AnyErr> {
    validate_start(start, budget, query)?;
    let mut client = tokio::select! {
        result = ReplClient::connect(&selected.remote.connection) => result.map_err(|error| -> AnyErr { error })?,
        _ = tokio::signal::ctrl_c() => return Err("remote repl connection interrupted".into()),
    };
    run_session(&mut client, &selected, json_output, start, budget, query).await
}

async fn run_session(
    client: &mut ReplClient,
    selected: &SelectedRemote,
    json_output: bool,
    start: &str,
    budget: usize,
    query: Option<&str>,
) -> Result<(), AnyErr> {
    let mut active = None;
    let result = tokio::select! {
        result = shell(client, selected, &mut active, json_output, start, budget, query) => result,
        _ = tokio::signal::ctrl_c() => Err("remote repl interrupted; a submitted reflect may have completed, do not blindly retry".into()),
    };
    // Terminal cleanup is best effort and bounded. Cancellation of an unknown
    // start token is recovered by server expiry; there is no invented token or retry.
    if let Some(session) = active.take() {
        let _ = tokio::time::timeout(
            CLEANUP_TIMEOUT,
            call_walk(
                client,
                selected.owner.as_ref(),
                &selected.remote.database,
                json!({"action":"abort","session":session}),
            ),
        )
        .await;
    }
    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, client.close()).await;
    result
}

async fn admit(
    client: &mut ReplClient,
    owner: Option<&OwnerBinding>,
    database: &str,
    request: &mut Request,
) -> Result<(), AnyErr> {
    if let ReplClient::Configured { client, name, .. } = client {
        let selected = client.selection().explicit_store(name)?;
        selected.require_operation(match request.access() {
            RequestAccess::ReadOnly => mneme_config::Operation::ExplicitRead,
            RequestAccess::Mutation => mneme_config::Operation::ExplicitWrite,
        })?;
        request.guard_database(selected.store.id.parse()?);
    }
    if !client.advertises(request.tool()) {
        return Err(format!(
            "remote repl requires advertised {} tool; no fallback attempted",
            request.tool()
        )
        .into());
    }
    if let Some(owner) = owner {
        owner.check_request(request)?;
        let ReplClient::Legacy(client) = client else {
            return Err("legacy owner guard cannot apply to a configured client".into());
        };
        owner.verify_and_guard(client, request, database).await?;
    }
    Ok(())
}

async fn call_walk(
    client: &mut ReplClient,
    owner: Option<&OwnerBinding>,
    database: &str,
    arguments: Value,
) -> Result<Value, AnyErr> {
    let mut request = Request::walk(arguments);
    admit(client, owner, database, &mut request).await?;
    let result = client
        .call_tool(request.tool(), request.into_arguments())
        .await
        .map_err(|error| -> AnyErr { error })?;
    if let Some(owner) = owner {
        owner.check_result(RequestAccess::ReadOnly, &result, database)?;
    }
    Ok(result)
}

// A bounded channel means piped stdin cannot queue an unbounded script while a
// request is pending. A blocking OS stdin read lives outside the async runtime,
// so Ctrl-C can cancel a pending prompt without waiting for the next newline.
fn stdin_lines() -> mpsc::Receiver<Result<String, String>> {
    let (sender, receiver) = mpsc::channel(1);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        loop {
            let value = match bounded_line(&mut input) {
                Ok(Some(value)) => Ok(value),
                Ok(None) => break,
                Err(error) => Err(error.to_string()),
            };
            let failed = value.is_err();
            if sender.blocking_send(value).is_err() || failed {
                break;
            }
        }
    });
    receiver
}

fn bounded_line(input: &mut impl BufRead) -> Result<Option<String>, AnyErr> {
    let mut bytes = Vec::new();
    loop {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let end = chunk.iter().position(|b| *b == b'\n').map(|i| i + 1);
        let count = end.unwrap_or(chunk.len());
        if bytes.len().saturating_add(count) > MAX_LINE_BYTES {
            return Err("remote repl stdin line exceeds 8192 bytes".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
        input.consume(count);
        if end.is_some() {
            break;
        }
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8(bytes).map_err(|_| "remote repl stdin is not UTF-8")?,
    ))
}

#[derive(Debug, PartialEq)]
enum Command {
    Help,
    Continue(&'static str, Option<String>),
    Finish(Vec<String>),
    Abort,
}
fn parse_command(line: &str) -> Result<Option<Command>, AnyErr> {
    let mut parts = line.split_whitespace();
    let Some(command) = parts.next() else {
        return Ok(None);
    };
    if command == "done" {
        let mut used = Vec::new();
        for raw in parts {
            let id = parse_id(raw)?.0.to_string();
            if !used.contains(&id) {
                used.push(id);
            }
            if used.len() > 64 {
                return Err("remote repl done accepts at most 64 used IDs".into());
            }
        }
        return Ok(Some(Command::Finish(used)));
    }
    let action = match command {
        "help" | "h" | "?" => return Ok(Some(Command::Help)),
        "abort" | "quit" | "exit" | "q" => return Ok(Some(Command::Abort)),
        "look" | "node" | "l" => "look",
        "edges" | "e" | "ls" => "edges",
        "body" | "b" => "body",
        "back" | "u" => "back",
        "go" | "g" => "go",
        _ => return Err(format!("unknown command {command:?} (try `help`)").into()),
    };
    let target = if action == "go" {
        let target = parts.next().ok_or("usage: go <index|id>")?;
        if target.parse::<usize>().is_err() {
            parse_id(target)?;
        }
        Some(target.to_string())
    } else {
        None
    };
    if parts.next().is_some() {
        return Err("unexpected shell command argument".into());
    }
    Ok(Some(Command::Continue(action, target)))
}

async fn shell(
    client: &mut ReplClient,
    selected: &SelectedRemote,
    active: &mut Option<String>,
    json_output: bool,
    start: &str,
    budget: usize,
    query: Option<&str>,
) -> Result<(), AnyErr> {
    let database = &selected.remote.database;
    let owner = selected.owner.as_ref();
    let mut arguments = json!({"db":database,"action":"start","start":start,"budget":budget});
    if let Some(query) = query {
        arguments["query"] = json!(query);
    }
    let started = call_walk(client, owner, database, arguments).await?;
    let session = started["session"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .ok_or("remote walk start response lacks a bounded session token")?
        .to_string();
    *active = Some(session.clone());
    let mut visited = HashSet::new();
    observe_view(&started["view"], &mut visited)?;
    if !json_output {
        println!(
            "(read-only remote walk — finish `done <used-id…>` to train; body and edges are bounded)"
        );
    }
    render(&started["view"], json_output);
    let mut lines = stdin_lines();
    let mut input_lines = 0;
    loop {
        if !json_output {
            eprint!("mneme> ");
            let _ = std::io::stderr().flush();
        }
        let command = match lines.recv().await {
            None => Command::Abort,
            Some(line) => {
                input_lines += 1;
                if input_lines > MAX_INPUT_LINES {
                    return Err(
                        "remote repl stdin exceeds 1024 lines; no training submitted".into(),
                    );
                }
                let line = line.map_err(|error| -> AnyErr { error.into() })?;
                let explicit_done = line.split_whitespace().next() == Some("done");
                match parse_command(&line) {
                    Ok(Some(command)) => command,
                    Ok(None) => continue,
                    Err(error) => {
                        // Malformed explicit feedback is fatal, never reinterpreted
                        // as an empty judgment or silently dropped.
                        if explicit_done {
                            return Err(error);
                        }
                        render(&json!({"error":error.to_string()}), json_output);
                        continue;
                    }
                }
            }
        };
        match command {
            Command::Help => render(
                &json!({"commands":["look","edges","body","go <i|id>","back","done [used-id…]","abort"]}),
                json_output,
            ),
            Command::Continue(action, target) => {
                let mut arguments = json!({"action":action,"session":session});
                if let Some(target) = target {
                    arguments["to"] = json!(target);
                }
                match call_walk(client, owner, database, arguments).await {
                    Ok(value) => {
                        if matches!(action, "look" | "go" | "back") {
                            observe_view(&value, &mut visited)?;
                        }
                        render(&value, json_output);
                    }
                    Err(error)
                        if mneme_mcp_client::classify_error(error.as_ref())
                            == mneme_mcp_client::ClientErrorClass::Tool =>
                    {
                        render(&json!({"error":error.to_string()}), json_output);
                    }
                    Err(error) => return Err(error),
                }
            }
            Command::Finish(used) if !used.is_empty() => {
                if used.iter().any(|id| !visited.contains(id)) {
                    return Err("done used ID was not visited; no training submitted".into());
                }
                // Prove reflect is available/guardable before issuing a receipt.
                let mut reflection =
                    Request::reflection(json!({"db":database,"receipts":[],"used":used}));
                admit(client, owner, database, &mut reflection).await?;
                let done = call_walk(
                    client,
                    owner,
                    database,
                    json!({"action":"done","session":session}),
                )
                .await?;
                *active = None;
                let receipt = done["receipt"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 128)
                    .ok_or(
                        "remote walk done response lacks a bounded receipt; no reflect submitted",
                    )?;
                let mut reflection_arguments = reflection.into_arguments();
                reflection_arguments["receipts"] = json!([receipt]);
                let reflected = client.call_tool("reflect", reflection_arguments).await
                    .map_err(|error| -> AnyErr { format!("remote reflect failed: {error}; a submitted write may have completed, do not blindly retry").into() })?;
                if let Some(owner) = owner {
                    owner.check_result(RequestAccess::Mutation, &reflected, database)?;
                }
                if ["reinforced", "interfered", "bridged"]
                    .iter()
                    .any(|key| reflected[key].as_u64().is_none())
                {
                    return Err("remote reflect response lacks feedback statistics; submitted write outcome is unknown, do not blindly retry".into());
                }
                finish(&done, Some(&reflected), visited.len(), json_output)?;
                return Ok(());
            }
            Command::Finish(_) | Command::Abort => {
                // Do not automatically resend an abort whose response is lost.
                // Session close/expiry is the fallback for an ambiguous terminal call.
                *active = None;
                let done = call_walk(
                    client,
                    owner,
                    database,
                    json!({"action":"abort","session":session}),
                )
                .await?;
                *active = None;
                finish(&done, None, visited.len(), json_output)?;
                return Ok(());
            }
        }
    }
}

fn observe_view(value: &Value, visited: &mut HashSet<String>) -> Result<(), AnyErr> {
    let id = value["at"]
        .as_str()
        .ok_or("remote walk response lacks node view")?;
    parse_id(id)?;
    visited.insert(id.to_owned());
    if visited.len() > 64 {
        return Err("remote walk exceeded its 64-node envelope".into());
    }
    Ok(())
}
fn finish(
    done: &Value,
    reflected: Option<&Value>,
    visited: usize,
    json_output: bool,
) -> Result<(), AnyErr> {
    let trail = done["trail"]
        .as_array()
        .ok_or("remote walk completion lacks trail")?;
    if trail.len() > 64 {
        return Err("remote walk trail exceeds 64 nodes".into());
    }
    let output = json!({"trail":trail,"visited":visited,"reflected":reflected});
    if json_output {
        render(&output, true);
    } else {
        println!(
            "\n--- trail ({} nodes, {}) ---",
            trail.len(),
            if reflected.is_some() {
                "reflected"
            } else {
                "no training"
            }
        );
        for step in trail {
            println!(
                "  {} <- {}",
                step["node"].as_str().unwrap_or("?"),
                step["from"].as_str().unwrap_or("start")
            );
        }
        if let Some(reflected) = reflected {
            println!(
                "reflected: +{} reinforced, {} interfered, {} bridged",
                reflected["reinforced"], reflected["interfered"], reflected["bridged"]
            );
        }
    }
    Ok(())
}
fn render(value: &Value, json_output: bool) {
    if json_output {
        println!("{value}");
    } else if let Some(error) = value["error"].as_str() {
        println!("! {error}");
    } else if let Some(body) = value["body"].as_str() {
        println!("{body}");
        if value["has_more"] == true {
            println!(
                "  … body truncated; use `body ID --offset {}` with the same owner selector",
                value["next_offset"]
            );
        }
    } else if let Some(commands) = value["commands"].as_array() {
        for command in commands {
            println!("  {}", command.as_str().unwrap_or("?"));
        }
    } else {
        if let Some(at) = value["at"].as_str() {
            println!(
                "\n@ {at} [{}] {}\n  visited {}/{}  depth {}",
                value["status"].as_str().unwrap_or("?"),
                value["summary"].as_str().unwrap_or("(missing)"),
                value["visited"],
                value["budget"],
                value["depth"]
            );
        }
        if let Some(edges) = value["edges"].as_array() {
            if edges.is_empty() {
                println!("  (no edges)");
            }
            for edge in edges {
                println!(
                    "  [{}] {}[{} w={}] {}  {}",
                    edge["i"],
                    if edge["incoming"] == true {
                        "<--"
                    } else {
                        "-->"
                    },
                    edge["kind"].as_str().unwrap_or("?"),
                    edge["weight"],
                    edge["id"].as_str().unwrap_or("?"),
                    edge["summary"].as_str().unwrap_or("")
                );
            }
            if value["edge_count"]
                .as_u64()
                .is_some_and(|n| n > edges.len() as u64)
            {
                println!(
                    "  … {} bounded edges — `edges` for more",
                    value["edge_count"]
                );
            }
        }
    }
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    #[test]
    fn admission_and_feedback_are_strict() {
        assert!(validate_start(ID, 1, Some("task")).is_ok());
        for budget in [0, 65, usize::MAX] {
            assert!(validate_start(ID, budget, None).is_err());
        }
        assert!(validate_start("bad", 25, None).is_err());
        assert!(validate_start(ID, 25, Some(" ")).is_err());
        assert_eq!(
            parse_command("done").unwrap(),
            Some(Command::Finish(vec![]))
        );
        assert_eq!(
            parse_command(&format!("done {ID} {ID}")).unwrap(),
            Some(Command::Finish(vec![ID.into()]))
        );
        assert!(parse_command("done invalid").is_err());
        assert_eq!(
            parse_command("g 0").unwrap(),
            Some(Command::Continue("go", Some("0".into())))
        );
        assert_eq!(parse_command("q").unwrap(), Some(Command::Abort));
    }
    #[test]
    fn stdin_bounds_apply_before_allocating_the_whole_line() {
        let mut input = std::io::Cursor::new(vec![b'x'; MAX_LINE_BYTES + 1]);
        assert!(bounded_line(&mut input).is_err());
        assert!(bounded_line(&mut std::io::Cursor::new(vec![0xff])).is_err());
        assert_eq!(
            bounded_line(&mut std::io::Cursor::new(b"look\n")).unwrap(),
            Some("look\n".into())
        );
        assert!(
            bounded_line(&mut std::io::Cursor::new([]))
                .unwrap()
                .is_none()
        );
    }
}
