//! `wwn launch -- ARGV..`: runs a command as the TV's user, as niri's `spawn` does.

use anyhow::Result;
use clap::{Arg, ArgAction, ArgMatches, Command};
use ww_niri_client::Tv;

use crate::connect::json;
use crate::registry::{BoxFuture, CommandDef};

fn command() -> Command {
    Command::new("launch")
        .about("Runs argv as the TV's user; answers its pid")
        .arg(
            Arg::new("argv")
                .required(true)
                .num_args(1..)
                .trailing_var_arg(true)
                .allow_hyphen_values(true)
                .action(ArgAction::Set),
        )
}

fn run<'a>(tv: &'a Tv, matches: &'a ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let argv: Vec<String> = matches
            .get_many::<String>("argv")
            .expect("argv is required")
            .cloned()
            .collect();
        let pid = tv.launch(argv).await?;
        crate::output::emit(
            json(matches),
            serde_json::json!({"pid": pid}),
            &pid.to_string(),
        );
        Ok(())
    })
}

inventory::submit! { CommandDef { name: "launch", order: 12, command, run } }
