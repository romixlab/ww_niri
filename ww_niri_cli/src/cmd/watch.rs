//! `wwn watch`: the live model, one line per change, until Ctrl+C.

use anyhow::Result;
use clap::{ArgMatches, Command};
use ww_niri_client::Tv;

use crate::connect::json;
use crate::output;
use crate::registry::{BoxFuture, CommandDef};

fn command() -> Command {
    Command::new("watch").about("Prints the live model, one line per change, until Ctrl+C")
}

fn run<'a>(tv: &'a Tv, matches: &'a ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let json = json(matches);
        let mut snapshots = tv.watch_snapshot();
        println!(
            "{}",
            output::snapshot_line(json, &snapshots.borrow_and_update())
        );
        loop {
            tokio::select! {
                changed = snapshots.changed() => {
                    changed?;
                    println!("{}", output::snapshot_line(json, &snapshots.borrow_and_update()));
                }
                _ = tokio::signal::ctrl_c() => return Ok(()),
            }
        }
    })
}

inventory::submit! { CommandDef { name: "watch", order: 60, command, run } }
