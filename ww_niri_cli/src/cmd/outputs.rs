//! `wwn outputs list`.

use anyhow::Result;
use clap::{ArgMatches, Command};
use ww_niri_client::Tv;

use crate::connect::json;
use crate::output;
use crate::registry::{BoxFuture, CommandDef};

fn command() -> Command {
    Command::new("outputs")
        .about("The TV's monitors")
        .subcommand_required(true)
        .subcommand(Command::new("list").about("Every connected output"))
}

fn run<'a>(tv: &'a Tv, matches: &'a ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let outputs = tv.list_outputs().await?;
        let (value, text) = output::outputs(&outputs);
        output::emit(json(matches), value, &text);
        Ok(())
    })
}

inventory::submit! { CommandDef { name: "outputs", order: 11, command, run } }
