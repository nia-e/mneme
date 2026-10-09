//! One inventory request and renderer for owner-backed and explicit offline CLI.
use clap::{Args, ValueEnum};
use mneme_app::list::PreparedList;
use serde_json::{Value, json};

use crate::AnyErr;

#[derive(Clone, Copy, ValueEnum)]
enum ListStatus {
    Active,
    Archived,
    All,
}

#[derive(Args)]
pub(crate) struct ListArgs {
    /// Browse native touchstone annotations, not nodes with a matching tag.
    #[arg(long, conflicts_with_all = ["status", "tag", "tags", "prefix"])]
    pub(crate) touchstones: bool,
    /// Browse the indexed semantic tag vocabulary, not matching nodes.
    #[arg(long, conflicts_with_all = ["touchstones", "tag"])]
    tags: bool,
    /// Exact tag prefix, including an empty prefix; no case or spelling normalization.
    #[arg(long, requires = "tags")]
    prefix: Option<String>,
    /// Opaque continuation returned by list. Bound to database and filters.
    #[arg(long)]
    after: Option<String>,
    #[arg(long, value_enum)]
    status: Option<ListStatus>,
    #[arg(long = "tag")]
    tag: Option<String>,
    /// Nodes or tags per page (default 50, maximum 64), or touchstones (default/max 32).
    #[arg(long)]
    limit: Option<usize>,
}

impl ListArgs {
    pub(crate) fn prepare(&self) -> Result<PreparedList, AnyErr> {
        let mut input = json!({"kind": if self.touchstones { "touchstones" } else if self.tags { "tags" } else { "nodes" }});
        if let Some(status) = self.status {
            input["status"] = json!(match status {
                ListStatus::Active => "active",
                ListStatus::Archived => "archived",
                ListStatus::All => "all",
            });
        }
        if let Some(tag) = &self.tag {
            input["tag"] = json!(tag);
        }
        if let Some(prefix) = &self.prefix {
            input["prefix"] = json!(prefix);
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
    if json_output || matches!(page["kind"].as_str(), Some("touchstones" | "tags")) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Value, AnyErr> {
        let cli = crate::Cli::try_parse_from(args)?;
        let crate::Command::List(args) = cli.command else {
            panic!("wrong command")
        };
        Ok(args.prepare()?.into_json())
    }

    #[test]
    fn tag_vocabulary_has_distinct_selection_and_exact_prefix() {
        let tags = parse(&[
            "mnemed", "list", "--tags", "--prefix", " People", "--status", "all", "--limit", "1",
        ])
        .unwrap();
        assert_eq!(tags["kind"], "tags");
        assert_eq!(tags["prefix"], " People");
        assert_eq!(tags["status"], "all");
        assert_eq!(tags["limit"], 1);
        let empty = parse(&["mnemed", "list", "--tags", "--prefix", ""]).unwrap();
        assert_eq!(empty["prefix"], "");
        let nodes = parse(&["mnemed", "list", "--tag", "people"]).unwrap();
        assert_eq!(nodes["kind"], "nodes");
        assert_eq!(nodes["tag"], "people");
        assert_eq!(
            parse(&["mnemed", "list", "--touchstones"]).unwrap()["kind"],
            "touchstones"
        );
    }

    #[test]
    fn incompatible_tag_vocabulary_flags_fail_admission() {
        for args in [
            vec!["mnemed", "list", "--prefix", "people"],
            vec!["mnemed", "list", "--tags", "--tag", "people"],
            vec!["mnemed", "list", "--tags", "--touchstones"],
            vec!["mnemed", "list", "--touchstones", "--status", "all"],
            vec!["mnemed", "list", "--tags", "--limit", "65"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }
}
