//! `wwn`: every ww_niri control API call as a subcommand, driving one TV at `--tv URL`.
//!
//! Thin by design: this crate only builds the command line and dispatches; the actual calls are
//! [ww_niri_client::Tv]'s. Commands register themselves from their own module in [cmd] through
//! [registry]; nothing here names them all.

pub mod args;
pub mod cmd;
pub mod complete;
pub mod connect;
pub mod output;
pub mod registry;

use clap::{Arg, ArgAction, Command, value_parser};

/// `wwn --version`: "0.1.0 (a1b2c3d-dirty, commit 3 Oct 2026 18:20 UTC)".
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("GIT_SHA"),
    ", commit ",
    env!("BUILD_TIME"),
    ")"
);

/// `--hold-limit-ms` must fit [ww_niri_client::HOLD_LIMIT_MIN]..=[ww_niri_client::HOLD_LIMIT_MAX].
const HOLD_LIMIT_MS_RANGE: std::ops::RangeInclusive<u64> = 100..=60_000;

/// The whole command line: global flags plus every registered subcommand.
pub fn root_command() -> Command {
    let root = Command::new("wwn")
        .about("Drives a TV box running ww_niri's control API")
        .version(VERSION)
        .subcommand_required(true)
        .arg_required_else_help(true)
        .arg(
            Arg::new("tv")
                .long("tv")
                .global(true)
                .env("WWN_TV")
                .default_value(connect::DEFAULT_TV)
                .help("The TV's control API, e.g. ws://alma-2tv:7590/ww"),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Machine-readable output instead of text for people"),
        )
        .arg(
            Arg::new("connect_timeout_ms")
                .long("connect-timeout-ms")
                .global(true)
                .default_value("5000")
                .value_parser(value_parser!(u64))
                .help("How long to wait for the TV to answer before giving up"),
        )
        .arg(
            Arg::new("hold_limit_ms")
                .long("hold-limit-ms")
                .global(true)
                .default_value("3000")
                .value_parser(value_parser!(u64).range(HOLD_LIMIT_MS_RANGE))
                .help("How long the TV keeps a held button or key without hearing from wwn"),
        );
    registry::commands()
        .into_iter()
        .fold(root, |root, def| root.subcommand((def.command)()))
}

/// Runs the subcommand `matches` named, against `tv`: tests build both themselves, against
/// [ww_niri_fake](https://docs.rs/ww_niri_fake); [run] is `main`'s way of getting here.
///
/// # Panics
///
/// When `matches` is not the root command's own (no subcommand, or one no module registered).
pub async fn dispatch(tv: &ww_niri_client::Tv, matches: &clap::ArgMatches) -> anyhow::Result<()> {
    let (name, sub) = matches
        .subcommand()
        .expect("subcommand_required, enforced by clap");
    let def = registry::find(name).expect("clap only names registered subcommands");
    (def.run)(tv, sub).await
}

/// Parses `args`, connects to `--tv`, and runs the named subcommand: what `main` does.
///
/// # Errors
///
/// When the TV can't be reached within `--connect-timeout-ms`, or the command fails. A bad
/// command line, `--help` or `--version` exit the process themselves (clap's own behaviour).
pub async fn run(args: impl IntoIterator<Item = std::ffi::OsString>) -> anyhow::Result<()> {
    complete::maybe_complete(root_command);
    let matches = root_command().get_matches_from(args);
    let tv = connect::connect(&matches).await?;
    dispatch(&tv, &matches).await
}
