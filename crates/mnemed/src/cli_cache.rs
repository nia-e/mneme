//! Offline demand diagnostics. This command cannot refresh discovery, contact an
//! owner, transfer a snapshot, or replace the caller's frozen request.
use crate::AnyErr;
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Args)]
pub(crate) struct CacheArgs {
    #[command(subcommand)]
    action: CacheAction,
}
#[derive(Subcommand)]
enum CacheAction {
    /// Revalidate a successful-use demand against its exact workspace request.
    /// Refuses expired or changed policy before any network or snapshot work.
    Plan {
        #[arg(long)]
        demand: PathBuf,
    },
}

pub(crate) fn run(args: &CacheArgs, json_output: bool) -> Result<(), AnyErr> {
    match &args.action {
        CacheAction::Plan { demand } => {
            let plan =
                mneme_store_client::demand::plan(demand).map_err(|error| -> AnyErr { error })?;
            if json_output {
                println!("{}", serde_json::to_string(&plan)?);
            } else {
                println!("{} ({}) → {}", plan.store, plan.db_id, plan.library);
                println!("canonical owner: {} / {}", plan.owner.url, plan.owner.db);
                println!("local catalog: {}", plan.catalog_path.display());
                println!("selection: {}", plan.selection_digest);
                println!(
                    "demand valid until: {} (Unix seconds); offline policy check only",
                    plan.deadline_unix_secs
                );
            }
            Ok(())
        }
    }
}
