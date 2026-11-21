//! Client of ww_niri's control API (`ww_niri_api`): a [Tv] handle that connects in the background
//! and again whenever the link goes, follows the TV in a live [Snapshot], and sends typed
//! commands, merged so a fast UI never floods the link.
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use ww_niri_client::{Config, Tv};
//!
//! let tv = Tv::start(Config::websocket("ws://192.168.1.95:7590/ww"))?;
//! let mut state = tv.watch_state();
//! state.wait_for(|s| s.is_connected()).await?;
//! tv.slot(0).open("https://example.org/".to_owned()).await?;
//! tv.slot(0).playback().play_pause().await?;
//! tv.pointer().move_by(20, -5).await?;
//! let volume = tv.snapshot().audio.volume;
//! # Ok(()) }
//! ```
//!
//! Pointer motion from a touchpad goes over UDP with a [MotionSender], so a lossy Wi-Fi drops a
//! move instead of holding the later ones up; everything else stays on the [Tv]'s WebSocket.
//!
//! Without a tokio runtime, [Tv::start] runs one on a thread of its own; an egui app reads
//! [Tv::snapshot] each frame, sends with [Tv::send] and passes `ctx.request_repaint` to
//! [Config::on_change].

mod api;
mod command;
mod config;
mod error;
mod model;
mod motion;
mod queue;
mod runtime;
mod snapshot;
mod tv;

pub use command::{Command, PlaybackAction, PlaybackTarget};
pub use config::{Address, Config, HOLD_LIMIT_MAX, HOLD_LIMIT_MIN, OnChange};
pub use error::{StartError, TvError};
pub use model::CoverLifted;
pub use motion::{MotionAddress, MotionSender};
pub use queue::Failed;
pub use snapshot::{AudioState, ConnectionState, Playback, PlayerState, SlotState, Snapshot};
pub use tv::{AudioCalls, KeyboardCalls, PlaybackCalls, PointerCalls, SlotCalls, Tv, WindowCalls};
pub use wire_weaver_client::{Backoff, KeepTrying};
pub use ww_niri_api;
