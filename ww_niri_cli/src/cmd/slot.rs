//! `wwn slot <INDEX> <ACTION>`: a browser slot's lifecycle and its player's playback.

use anyhow::Result;
use clap::{Args, FromArgMatches, Subcommand};
use exact_units::Fixed;
use ww_niri_client::{PlaybackCalls, SlotCalls, Tv, TvError};

use crate::args::{self, CurveArg};
use crate::registry::{BoxFuture, CommandDef};

#[derive(Subcommand)]
enum Action {
    /// Launches the slot's Firefox; answers when it can be driven.
    Start {
        #[arg(long)]
        windowed: bool,
    },
    /// Loads an http or https URL in the slot's tab.
    Open {
        url: String,
    },
    /// Ends the slot's Firefox; its profile stays.
    Stop,
    /// Shows the slot's window.
    Show {
        #[arg(long, requires = "transition_ms")]
        transition: Option<CurveArg>,
        #[arg(long, requires = "transition")]
        transition_ms: Option<u16>,
        #[arg(long, conflicts_with = "windowed")]
        fullscreen: bool,
        #[arg(long, conflicts_with = "fullscreen")]
        windowed: bool,
    },
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    /// Relative; negative goes back. `10s`, `-500ms`, a plain number is seconds.
    Seek {
        #[arg(value_parser = args::parse_signed_micros)]
        by: i64,
    },
    /// Absolute, from the start of the track.
    SetPosition {
        #[arg(value_parser = args::parse_unsigned_micros)]
        at: u64,
    },
}

#[derive(Args)]
struct SlotArgs {
    index: u8,
    #[command(subcommand)]
    action: Action,
}

fn command() -> clap::Command {
    let cmd =
        SlotArgs::augment_args(clap::Command::new("slot").about("A browser slot and its playback"));
    // `seek` takes a signed duration (`-10s` goes back); its own subcommand needs the setting.
    cmd.mut_subcommand("seek", |seek| seek.allow_negative_numbers(true))
}

fn run<'a>(tv: &'a Tv, matches: &'a clap::ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let args = SlotArgs::from_arg_matches(matches)?;
        let slot = tv.slot(args.index);
        match args.action {
            Action::Start { windowed } => slot.start(!windowed).await?,
            Action::Open { url } => slot.open(url).await?,
            Action::Stop => slot.stop().await?,
            Action::Show {
                transition,
                transition_ms,
                fullscreen,
                windowed,
            } => show(&slot, transition, transition_ms, fullscreen, windowed).await?,
            action => playback(&slot.playback(), action).await?,
        }
        Ok(())
    })
}

async fn show(
    slot: &SlotCalls<'_>,
    transition: Option<CurveArg>,
    transition_ms: Option<u16>,
    fullscreen: bool,
    windowed: bool,
) -> Result<(), TvError> {
    let transition = transition
        .zip(transition_ms)
        .map(|(c, ms)| c.transition(ms));
    let fullscreen = super::windows::tri_state(fullscreen, windowed);
    slot.show(transition, fullscreen).await
}

/// The playback actions of [Action]; the slot's own lifecycle is handled in [run].
async fn playback(playback: &PlaybackCalls<'_>, action: Action) -> Result<(), TvError> {
    match action {
        Action::Play => playback.play().await,
        Action::Pause => playback.pause().await,
        Action::PlayPause => playback.play_pause().await,
        Action::Next => playback.next().await,
        Action::Previous => playback.previous().await,
        Action::Seek { by } => playback.seek(Fixed::new(by)).await,
        Action::SetPosition { at } => playback.set_position(Fixed::new(at)).await,
        Action::Start { .. } | Action::Open { .. } | Action::Stop | Action::Show { .. } => {
            unreachable!("run matches the slot's own lifecycle actions before falling through")
        }
    }
}

inventory::submit! { CommandDef { name: "slot", order: 20, command, run } }
