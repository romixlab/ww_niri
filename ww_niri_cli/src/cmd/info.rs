//! `wwn info`: niri version, ww_niri version, hostname.

use anyhow::Result;
use clap::{ArgMatches, Command};
use ww_niri_client::Tv;

use crate::connect::json;
use crate::output;
use crate::registry::{BoxFuture, CommandDef};

fn command() -> Command {
    Command::new("info").about("niri version, ww_niri version and hostname")
}

fn run<'a>(tv: &'a Tv, matches: &'a ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let info = tv.info().await?;
        let (value, text) = output::info(&info);
        output::emit(json(matches), value, &text);
        Ok(())
    })
}

inventory::submit! { CommandDef { name: "info", order: 0, command, run } }
