//! The command registry: every `wwn` subcommand registers itself from its own module with
//! `inventory::submit!`, so the command line, its help and completion come from one list and no
//! central enum or match names them all.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use clap::{ArgMatches, Command};
use ww_niri_client::Tv;

/// A future a command's `run` returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// One `wwn` subcommand, registered from its module.
pub struct CommandDef {
    /// The subcommand's name on the command line.
    pub name: &'static str,
    /// Its place in `--help`, lower first.
    pub order: u16,
    /// Builds the subcommand: its about text, arguments and subcommands.
    pub command: fn() -> Command,
    /// Runs it against the connected TV, with its own matches (global flags included).
    pub run: for<'a> fn(&'a Tv, &'a ArgMatches) -> BoxFuture<'a, Result<()>>,
}

inventory::collect!(CommandDef);

/// Every registered command, by their order in the help.
pub fn commands() -> Vec<&'static CommandDef> {
    let mut all: Vec<_> = inventory::iter::<CommandDef>().collect();
    all.sort_by_key(|c| (c.order, c.name));
    all
}

/// The registered command of that name.
pub fn find(name: &str) -> Option<&'static CommandDef> {
    inventory::iter::<CommandDef>().find(|c| c.name == name)
}
