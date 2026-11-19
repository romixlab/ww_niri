//! `wwn player`: every MPRIS player on the box (the slots' and others, mpv, Telegram) and its
//! playback, by [PlayerId].

use anyhow::Result;
use clap::{ArgMatches, Args, Command, FromArgMatches, Subcommand};
use exact_units::Fixed;
use ww_niri_api::PlayerId;
use ww_niri_client::{PlaybackCalls, Tv, TvError};

use crate::args;
use crate::connect::json;
use crate::output;
use crate::registry::{BoxFuture, CommandDef};

#[derive(Args)]
struct Id {
    id: u32,
}

#[derive(Args)]
struct Seek {
    id: u32,
    /// Relative; negative goes back. `10s`, `-500ms`, a plain number is seconds.
    #[arg(value_parser = args::parse_signed_micros)]
    by: i64,
}

#[derive(Args)]
struct SetPosition {
    id: u32,
    /// Absolute, from the start of the track.
    #[arg(value_parser = args::parse_unsigned_micros)]
    at: u64,
}

#[derive(Subcommand)]
enum Action {
    /// Every MPRIS player on the session bus.
    List,
    Play(Id),
    Pause(Id),
    PlayPause(Id),
    Stop(Id),
    Next(Id),
    Previous(Id),
    Seek(Seek),
    SetPosition(SetPosition),
}

fn command() -> Command {
    let cmd = Action::augment_subcommands(Command::new("player").about("MPRIS players"))
        .subcommand_required(true);
    // `seek` takes a signed duration (`-10s` goes back); its own subcommand needs the setting.
    cmd.mut_subcommand("seek", |seek| seek.allow_negative_numbers(true))
}

fn run<'a>(tv: &'a Tv, matches: &'a ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let action = Action::from_arg_matches(matches)?;
        match action {
            Action::List => {
                let players = tv.list_players().await?;
                let (value, text) = output::players(&players);
                output::emit(json(matches), value, &text);
            }
            action => playback(tv, action).await?,
        }
        Ok(())
    })
}

/// The actions of [Action] that take a [PlayerId]; `List` is handled in [run].
async fn playback(tv: &Tv, action: Action) -> Result<(), TvError> {
    fn of(tv: &Tv, id: u32) -> PlaybackCalls<'_> {
        tv.player(PlayerId(id))
    }
    match action {
        Action::Play(Id { id }) => of(tv, id).play().await,
        Action::Pause(Id { id }) => of(tv, id).pause().await,
        Action::PlayPause(Id { id }) => of(tv, id).play_pause().await,
        Action::Stop(Id { id }) => of(tv, id).stop().await,
        Action::Next(Id { id }) => of(tv, id).next().await,
        Action::Previous(Id { id }) => of(tv, id).previous().await,
        Action::Seek(Seek { id, by }) => of(tv, id).seek(Fixed::new(by)).await,
        Action::SetPosition(SetPosition { id, at }) => {
            of(tv, id).set_position(Fixed::new(at)).await
        }
        Action::List => unreachable!("run matches List before falling through"),
    }
}

inventory::submit! { CommandDef { name: "player", order: 21, command, run } }
