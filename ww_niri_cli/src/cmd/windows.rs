//! `wwn windows`: list, show, focus, close, set-fullscreen.

use anyhow::Result;
use clap::{FromArgMatches, Subcommand};
use ww_niri_api::WindowId;
use ww_niri_client::Tv;

use crate::args::{CurveArg, OnOff};
use crate::connect::json;
use crate::output;
use crate::registry::{BoxFuture, CommandDef};

#[derive(Subcommand)]
enum Action {
    /// Every window niri knows about.
    List,
    /// Brings a window on screen and focuses it.
    Show {
        id: u64,
        /// Easing curve; needs `--transition-ms`. Default: niri's configured animation.
        #[arg(long, requires = "transition_ms")]
        transition: Option<CurveArg>,
        #[arg(long, requires = "transition")]
        transition_ms: Option<u16>,
        #[arg(long, conflicts_with = "windowed")]
        fullscreen: bool,
        #[arg(long, conflicts_with = "fullscreen")]
        windowed: bool,
    },
    /// Keyboard focus only.
    Focus {
        id: u64,
    },
    /// Asks the window's app to close it.
    Close {
        id: u64,
    },
    SetFullscreen {
        id: u64,
        state: OnOff,
    },
}

fn command() -> clap::Command {
    Action::augment_subcommands(clap::Command::new("windows").about("niri's windows"))
        .subcommand_required(true)
}

fn run<'a>(tv: &'a Tv, matches: &'a clap::ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let action = Action::from_arg_matches(matches)?;
        match action {
            Action::List => {
                let windows = tv.list_windows().await?;
                let (value, text) = output::windows(&windows);
                output::emit(json(matches), value, &text);
            }
            Action::Show {
                id,
                transition,
                transition_ms,
                fullscreen,
                windowed,
            } => {
                let transition = transition
                    .zip(transition_ms)
                    .map(|(c, ms)| c.transition(ms));
                let fullscreen = tri_state(fullscreen, windowed);
                tv.window(WindowId(id)).show(transition, fullscreen).await?;
            }
            Action::Focus { id } => tv.window(WindowId(id)).focus().await?,
            Action::Close { id } => tv.window(WindowId(id)).close().await?,
            Action::SetFullscreen { id, state } => {
                tv.window(WindowId(id))
                    .set_fullscreen(state.as_bool())
                    .await?;
            }
        }
        Ok(())
    })
}

/// `None` when neither flag was given: keep whatever it was.
pub(crate) fn tri_state(on: bool, off: bool) -> Option<bool> {
    if on {
        Some(true)
    } else if off {
        Some(false)
    } else {
        None
    }
}

inventory::submit! { CommandDef { name: "windows", order: 10, command, run } }
