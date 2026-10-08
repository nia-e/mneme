//! One inventory request and renderer for owner-backed and explicit offline CLI.
use clap::Args;
use mneme_app::list::PreparedList;
use serde_json::{Value, json};

use crate::{AnyErr, StatusArg};

#[derive(Args)]
pub(crate) struct ListArgs {
    /// Browse native touchstone annotations, not nodes with a matching tag.
    #[arg(long, conflicts_with_all = ["status", "tag"])]
    pub(crate) touchstones: bool,
    /// Opaque continuation returned by list. Bound to database and filters.
    #[arg(long)]
    after: Option<String>,
    #[arg(long, value_enum)]
    status: Option<StatusArg>,
    #[arg(long = "tag")]
    tag: Option<String>,
    /// Nodes per page (default 50, maximum 64), or touchstones (default/max 32).
    #[arg(long)]
    limit: Option<usize>,
}

impl ListArgs {
    pub(crate) fn prepare(&self) -> Result<PreparedList, AnyErr> {
        let mut input = json!({"kind": if self.touchstones { "touchstones" } else { "nodes" }});
        if let Some(status) = self.status {
            input["status"] = json!(match status {
                StatusArg::Active => "active",
                StatusArg::Archived => "archived",
            });
        }
        if let Some(tag) = &self.tag {
            input["tag"] = json!(tag);
        }
        if let Some(after) = &self.after {
            input["after"] = json!(after);
        }
        if let Some(limit) = self.limit {
            input["limit"] = json!(limit);
        }
        PreparedList::parse(&input).map_err(|error| -> AnyErr { error })
    }
}

pub(crate) fn render(page: &Value, json_output: bool) -> Result<(), AnyErr> {
    if json_output || page["kind"] == "touchstones" {
        crate::print_json(page);
        return Ok(());
    }
    let items = page["items"]
        .as_array()
        .ok_or("list response lacks items")?;
    if items.is_empty() {
        println!("(no matching nodes in this page)");
    }
    for node in items {
        println!(
            "{}  {:<9}  {}{}",
            node["id"].as_str().unwrap_or("?"),
            node["status"].as_str().unwrap_or("?"),
            node["summary"].as_str().unwrap_or(""),
            if node["summary_truncated"] == true {
                " …"
            } else {
                ""
            }
        );
    }
    if let Some(cursor) = page["next_cursor"].as_str() {
        // Cursor is data, not shell-escaped text. JSON quoting makes embedded
        // quotes visible; do not hand out a command with unsafe interpolation.
        eprintln!(
            "[mnemed: more nodes; repeat list with the same filters and --after {}]",
            serde_json::to_string(cursor)?
        );
    }
    Ok(())
}
