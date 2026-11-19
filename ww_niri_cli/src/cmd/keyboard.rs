//! `wwn type`, `wwn keys`, `wwn key-down`: the TV's keyboard.

use std::time::Duration;

use anyhow::Result;
use clap::{Arg, ArgMatches, Command, value_parser};
use ww_niri_client::Tv;

use crate::registry::{BoxFuture, CommandDef};

fn type_command() -> Command {
    Command::new("type")
        .about("Types text with key presses of the TV's keymap")
        .arg(Arg::new("text").required(true))
}

fn type_run<'a>(tv: &'a Tv, matches: &'a ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let text = matches.get_one::<String>("text").expect("required").clone();
        tv.keyboard().type_text(text).await?;
        Ok(())
    })
}

fn keys_command() -> Command {
    Command::new("keys")
        .about("A key or combination: `Return`, `ctrl+l`, `Ctrl+Shift+Tab`")
        .arg(Arg::new("combo").required(true))
}

// spirit-level: allow(near-duplicate-fns) one handler per API method
fn keys_run<'a>(tv: &'a Tv, matches: &'a ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let combo = matches
            .get_one::<String>("combo")
            .expect("required")
            .clone();
        tv.keyboard().press_keys(combo).await?;
        Ok(())
    })
}

fn key_down_command() -> Command {
    Command::new("key-down")
        .about(
            "Holds a key (a keysym name or a modifier) down for a while, then releases it: a \
             one-shot `wwn` call cannot leave it held past its own connection, which the TV \
             releases it on",
        )
        .arg(Arg::new("key").required(true))
        .arg(
            Arg::new("for_ms")
                .long("for-ms")
                .default_value("1000")
                .value_parser(value_parser!(u64)),
        )
}

fn key_down_run<'a>(tv: &'a Tv, matches: &'a ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let key = matches.get_one::<String>("key").expect("required").clone();
        let for_ms = *matches.get_one::<u64>("for_ms").expect("has a default");
        tv.keyboard().key_down(key.clone()).await?;
        tokio::time::sleep(Duration::from_millis(for_ms)).await;
        tv.keyboard().key_up(key).await?;
        Ok(())
    })
}

inventory::submit! { CommandDef { name: "type", order: 50, command: type_command, run: type_run } }
inventory::submit! { CommandDef { name: "keys", order: 51, command: keys_command, run: keys_run } }
inventory::submit! { CommandDef { name: "key-down", order: 52, command: key_down_command, run: key_down_run } }
