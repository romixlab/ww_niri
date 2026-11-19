//! The live model: what the TV shows and plays, as of the last update, and the connection.

use std::time::{Duration, Instant};

use ww_niri_api::{
    BrowserStateOwned, InfoOwned, OutputOwned, PlaybackStateOwned, PlaybackStatus, PlayerId,
    PlayerInfoOwned, PointerPosition, SinkOwned, WindowId, WindowOwned,
};

/// Where the connection to the TV is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Not connected, and trying: attempt `attempt` runs or waits for its turn (0 before the
    /// first). `offline_since` is when the last link went, `last_error` why the last attempt
    /// failed or the link went.
    Connecting {
        attempt: u32,
        offline_since: Option<Instant>,
        last_error: Option<String>,
    },
    /// Connected at `address`, and the model follows the TV.
    Connected { address: String, info: InfoOwned },
    /// Not connected, and no longer trying: the give-up time of
    /// [Config::keep_trying](crate::Config::keep_trying) passed.
    Offline { since: Instant, error: String },
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionState::Connected { .. })
    }
}

/// One immutable state of the TV, replaced as a whole on every change.
///
/// It keeps the last state seen while the TV is offline; [ConnectionState] tells whether it is
/// current.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Every window, by id.
    pub windows: Vec<WindowOwned>,
    pub outputs: Vec<OutputOwned>,
    /// The browser slots, by index.
    pub slots: Vec<SlotState>,
    /// Every MPRIS player, by id: the slots' players and the others (mpv, Telegram).
    pub players: Vec<PlayerState>,
    pub audio: AudioState,
    /// Where the pointer is, and the window under it.
    pub pointer: Option<PointerPosition>,
    /// Counts the changes since the [Tv](crate::Tv) started.
    pub generation: u64,
}

impl Snapshot {
    pub fn window(&self, id: WindowId) -> Option<&WindowOwned> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// The window with keyboard focus.
    pub fn focused(&self) -> Option<&WindowOwned> {
        self.windows.iter().find(|w| w.focused)
    }

    pub fn slot(&self, index: u8) -> Option<&SlotState> {
        self.slots.iter().find(|s| s.index == index)
    }

    pub fn player(&self, id: PlayerId) -> Option<&PlayerState> {
        self.players.iter().find(|p| p.info.id == id)
    }
}

/// A browser slot.
#[derive(Clone, Debug)]
pub struct SlotState {
    pub index: u8,
    /// Phase, URL, title, window and player; `None` until the TV first sends it.
    pub state: Option<BrowserStateOwned>,
    /// The playback of the slot's player, while [BrowserStateOwned::player] is set.
    pub playback: Option<Playback>,
}

/// An MPRIS player.
#[derive(Clone, Debug)]
pub struct PlayerState {
    pub info: PlayerInfoOwned,
    /// `None` until the TV first sends it.
    pub playback: Option<Playback>,
}

/// A playback state as it arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Playback {
    pub state: PlaybackStateOwned,
    /// When it arrived, for [Playback::position_at].
    pub received: Instant,
}

impl Playback {
    /// The position at `at`, moved on from the received one with the rate while playing, up to
    /// the track's length: a seek bar moves smoothly between updates.
    pub fn position_at(&self, at: Instant) -> Duration {
        let position_us = self.state.position.get();
        if self.state.status != PlaybackStatus::Playing {
            return Duration::from_micros(position_us);
        }
        let elapsed_us = at.saturating_duration_since(self.received).as_micros();
        let moved_us = elapsed_us * u128::from(self.state.rate.get()) / 1000;
        let mut now_us = u128::from(position_us) + moved_us;
        if let Some(length) = self.state.length {
            now_us = now_us.min(u128::from(length.get()));
        }
        Duration::from_micros(u64::try_from(now_us).unwrap_or(u64::MAX))
    }
}

/// The default sink's volume, mute and name; each `None` until the TV first sends it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioState {
    /// Percent, 0 to 150.
    pub volume: Option<u16>,
    pub muted: Option<bool>,
    pub sink: Option<SinkOwned>,
}
