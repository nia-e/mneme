//! `mnemed repl` — a constrained traversal shell for a retrieval sub-agent. The
//! traversal *state machine* (free movement, node budget, the trail, and the
//! post-factum `reflect`) lives in [`mneme_walk::WalkSession`]; this module is the
//! **stdin/stdout skin** over it: it parses commands, renders each reply as human
//! text or (with `--json`) one JSON object per line, and runs the read–eval loop.
//!
//! The walk is **read-only** — it browses and gathers a trail, training nothing.
//! To train, finish with `done <used-id…>`: the edges that reached the nodes you
//! name are reinforced; omitted nodes are unknown and receive no learning signal.
//! `done` with no ids, `abort`, or EOF
//! leaves the graph untouched. On exit it prints the **trail**.

use std::io::{BufRead, Write};

use mneme_core::NodeId;
use mneme_core::ports::ColdPath;
use mneme_engine::Memory;
use mneme_walk::{EdgeView, ReflectStats, View, WalkError, WalkSession};
use serde_json::{Value, json};

use crate::{AnyErr, parse_id};

/// Run the shell to EOF or `done`. Returns whether the walk changed the graph
/// (so the caller knows to persist).
pub async fn run(
    mem: &Memory,
    json: bool,
    start: &str,
    budget: usize,
    query: Option<&str>,
) -> Result<bool, AnyErr> {
    let start = parse_id(start)?;
    if mem.get_node(start).await?.is_none() {
        return Err(format!("start node {} not found", start.0).into());
    }

    let mut session = WalkSession::new(start, budget);
    if let Some(q) = query {
        session = session.with_query_relevance(mem.query_relevance(q, 64).await?);
    }
    let mut s = Repl { mem, json, session };
    if !json {
        println!(
            "(read-only walk — move freely; finish `done <used-id…>` to train on what you used)"
        );
    }
    let view = s.session.view(mem).await?;
    s.show(&view);

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut reflected = None;
    let mut bridged = 0;
    loop {
        s.prompt();
        let Some(line) = lines.next() else { break }; // EOF: end, no training
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match s.handle(line).await? {
            Flow::Continue => {}
            Flow::Done(raw_used) => {
                // A bare `done` is the read-only completion path promised by the
                // CLI and skill contract. Previously it reflected an empty used
                // set, misclassifying every traversed edge as negative evidence.
                // Likewise, never turn malformed IDs into an empty set by
                // silently dropping them.
                let Some(used) = parse_reflection_request(&raw_used)? else {
                    break;
                };
                reflected = Some(s.session.reflect(&used, mem).await?);
                // Consolidate the co-used set into cross-cluster bridges (no-op for a
                // single-region walk — bites at a junction merging several walks).
                if used.len() >= 2 {
                    bridged = mem.consolidate(ColdPath::acquire(), &used).await?.len();
                }
                break;
            }
            Flow::Abort => break,
        }
    }
    let mutated = bridged > 0
        || reflected
            .as_ref()
            .is_some_and(|r| r.reinforced + r.interfered > 0);
    s.finish(reflected, bridged);
    Ok(mutated)
}

fn parse_reflection_request(raw: &[String]) -> Result<Option<Vec<NodeId>>, AnyErr> {
    if raw.is_empty() {
        return Ok(None);
    }
    let mut used = Vec::with_capacity(raw.len());
    for value in raw {
        let id = parse_id(value)?;
        if !used.contains(&id) {
            used.push(id);
        }
    }
    Ok(Some(used))
}

enum Flow {
    Continue,
    /// `done` with the node ids the agent actually used; omitted nodes are unknown.
    Done(Vec<String>),
    Abort,
}

struct Repl<'a> {
    mem: &'a Memory,
    json: bool,
    session: WalkSession,
}

impl Repl<'_> {
    async fn handle(&mut self, line: &str) -> Result<Flow, AnyErr> {
        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        match cmd {
            "done" => return Ok(Flow::Done(parts.map(String::from).collect())),
            "abort" | "quit" | "exit" | "q" => return Ok(Flow::Abort),
            _ => {}
        }
        let arg = parts.next();
        match cmd {
            "help" | "h" | "?" => self.help(),
            "look" | "node" | "l" => {
                let v = self.session.view(self.mem).await?;
                self.show(&v);
            }
            "edges" | "e" | "ls" => {
                let edges = self.session.edges(self.mem).await?;
                self.show_edges(&edges);
            }
            "body" | "b" => {
                let body = self.session.body(self.mem).await?;
                self.show_body(&body);
            }
            "go" | "g" => self.do_go(arg).await?,
            "back" | "u" => {
                let outcome = self.session.back(self.mem).await;
                self.step(outcome).await?;
            }
            other => self.err(&format!("unknown command {other:?} (try `help`)")),
        }
        Ok(Flow::Continue)
    }

    async fn do_go(&mut self, arg: Option<&str>) -> Result<(), AnyErr> {
        let Some(arg) = arg else {
            self.err("usage: go <index|id>");
            return Ok(());
        };
        let outcome = self.session.go(arg, self.mem).await;
        self.step(outcome).await
    }

    /// Render a move's outcome: a new view, a reported message on a rule violation
    /// (bad edge / budget / already at start), or a propagated engine error.
    async fn step(&self, outcome: Result<View, WalkError>) -> Result<(), AnyErr> {
        match outcome {
            Ok(view) => self.show(&view),
            Err(WalkError::Rejected(msg)) => self.err(&msg),
            Err(WalkError::Backend(e)) => return Err(e.into()),
        }
        Ok(())
    }

    // ---- rendering ----------------------------------------------------------

    fn show(&self, v: &View) {
        if self.json {
            line(&serde_json::to_value(v).unwrap_or(Value::Null));
            return;
        }
        match &v.summary {
            Some(sum) => println!("\n@ {}  [{}]  {sum}", v.at, v.status.unwrap_or("?")),
            None => println!("\n@ {} (missing)", v.at),
        }
        println!("  visited {}/{}  depth {}", v.visited, v.budget, v.depth);
        self.print_edges(&v.edges);
        if v.edge_count > v.edges.len() {
            println!("  … {} edges total — `edges` for all", v.edge_count);
        }
    }

    fn show_edges(&self, edges: &[EdgeView]) {
        if self.json {
            line(&json!({ "edges": serde_json::to_value(edges).unwrap_or(Value::Null) }));
        } else {
            self.print_edges(edges);
        }
    }

    fn print_edges(&self, edges: &[EdgeView]) {
        if edges.is_empty() {
            println!("  (no edges)");
            return;
        }
        for e in edges {
            let arrow = if e.incoming { "<--" } else { "-->" };
            println!(
                "  [{}] {arrow}[{} w={:.2}] {}  {}",
                e.i, e.kind, e.weight, e.id, e.summary
            );
        }
    }

    fn show_body(&self, body: &str) {
        if self.json {
            line(&json!({ "body": body }));
        } else {
            println!("{body}");
        }
    }

    fn help(&self) {
        if self.json {
            line(
                &json!({ "commands": ["edges", "body", "look", "go <i|id>", "back", "done [used-id…]", "abort"] }),
            );
            return;
        }
        println!(
            "commands (read-only browse):\n  \
             edges            list the current node's edges (your move options)\n  \
             body             print the current node's full body\n  \
             look             reprint the current node\n  \
             go <i|id>        step to neighbor i (or by id)\n  \
             back             retreat to the previous node\n  \
             done [id…]       finish; reinforce edges to the listed (used) nodes,\n  \
             \x20               leave the rest of the trail unchanged\n  \
             abort            finish without training (also on EOF)"
        );
    }

    fn finish(&self, reflected: Option<ReflectStats>, bridged: usize) {
        let trail = self.session.trail();
        if self.json {
            line(&json!({
                "trail": serde_json::to_value(&trail).unwrap_or(Value::Null),
                "visited": self.session.visited_count(),
                "reflected": reflected.map(|r| json!({ "reinforced": r.reinforced, "interfered": r.interfered, "bridged": bridged })),
            }));
            return;
        }
        let outcome = match &reflected {
            Some(r) => format!(
                "reflected: +{} reinforced, {} interfered, {bridged} bridged",
                r.reinforced, r.interfered
            ),
            None => "no training".into(),
        };
        println!("\n--- trail ({} nodes, {outcome}) ---", trail.len());
        for step in &trail {
            let from = step.from.clone().unwrap_or_else(|| "start".into());
            println!("  {} <- {from}", step.node);
        }
    }

    fn prompt(&self) {
        if !self.json {
            eprint!("mneme> ");
            let _ = std::io::stderr().flush();
        }
    }

    fn err(&self, msg: &str) {
        if self.json {
            line(&json!({ "error": msg }));
        } else {
            println!("! {msg}");
        }
    }
}

fn line(v: &Value) {
    println!("{v}");
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use ulid::Ulid;

    #[test]
    fn explicit_used_ids_are_strict_and_deduplicated() {
        let id = Ulid::new().to_string();
        let parsed = parse_reflection_request(&[id.clone(), id])
            .unwrap()
            .unwrap();
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn malformed_used_id_is_not_reinterpreted_as_empty_negative_feedback() {
        assert!(parse_reflection_request(&["not-a-node-id".to_string()]).is_err());
    }

    #[test]
    fn bare_done_has_no_reflection_request() {
        assert_eq!(parse_reflection_request(&[]).unwrap(), None);
    }
}
