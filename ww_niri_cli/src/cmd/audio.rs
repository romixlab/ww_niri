//! `wwn audio` (current volume, mute, sink), `wwn volume PERCENT`, `wwn mute on|off`.

use std::time::Duration;

use anyhow::Result;
use clap::{Arg, ArgMatches, Command, value_parser};
use ww_niri_client::Tv;

use crate::args::OnOff;
use crate::connect::json;
use crate::output;
use crate::registry::{BoxFuture, CommandDef};

/// The audio state is observed, not fetched: give it a moment to arrive after connecting.
const AUDIO_WAIT: Duration = Duration::from_secs(2);

fn audio_command() -> Command {
    Command::new("audio").about("The default sink's volume, mute and name")
}

fn audio_run<'a>(tv: &'a Tv, matches: &'a ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let mut snapshots = tv.watch_snapshot();
        let _ = tokio::time::timeout(AUDIO_WAIT, snapshots.wait_for(|s| s.audio.volume.is_some()))
            .await;
        let (value, text) = output::audio(&tv.snapshot().audio);
        output::emit(json(matches), value, &text);
        Ok(())
    })
}

fn volume_command() -> Command {
    Command::new("volume")
        .about("Sets the default sink's volume, 0 to 150 %")
        .arg(
            Arg::new("percent")
                .required(true)
                .value_parser(value_parser!(u16).range(0..=150)),
        )
}

fn volume_run<'a>(tv: &'a Tv, matches: &'a ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let percent = *matches.get_one::<u16>("percent").expect("required");
        tv.audio().set_volume(percent).await?;
        Ok(())
    })
}

fn mute_command() -> Command {
    Command::new("mute")
        .about("Mutes or unmutes the default sink")
        .arg(
            Arg::new("state")
                .required(true)
                .value_parser(clap::value_parser!(OnOff)),
        )
}

fn mute_run<'a>(tv: &'a Tv, matches: &'a ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let state = *matches.get_one::<OnOff>("state").expect("required");
        tv.audio().set_muted(state.as_bool()).await?;
        Ok(())
    })
}

inventory::submit! { CommandDef { name: "audio", order: 30, command: audio_command, run: audio_run } }
inventory::submit! { CommandDef { name: "volume", order: 31, command: volume_command, run: volume_run } }
inventory::submit! { CommandDef { name: "mute", order: 32, command: mute_command, run: mute_run } }
