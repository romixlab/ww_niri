//! How a [Tv](crate::Tv) connects: its addresses, retries, hold limit and change callback.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use wire_weaver_client::KeepTrying;

/// Shortest hold limit [Config::hold_limit] takes: renewals come every third of it.
pub const HOLD_LIMIT_MIN: Duration = Duration::from_millis(100);
/// Longest hold limit [Config::hold_limit] takes.
pub const HOLD_LIMIT_MAX: Duration = Duration::from_secs(60);

/// Where the TV answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Address {
    /// ww_niri over WebSocket: `ws://192.168.1.95:7590/ww`.
    WebSocket(String),
    /// A server in this process: `ww_niri_fake`'s `FakeTv::path`.
    InProcess(String),
}

/// Called after every change of the live model or of the connection state.
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

/// How a [Tv](crate::Tv) connects.
#[derive(Clone)]
pub struct Config {
    pub(crate) addresses: Vec<Address>,
    pub(crate) keep_trying: KeepTrying,
    pub(crate) hold_limit: Duration,
    pub(crate) on_change: Option<OnChange>,
}

impl Config {
    /// Each attempt opens all the addresses at once, the first to answer wins, so a LAN
    /// address that is gone costs nothing while a tailnet one answers (wire_weaver CLT-80).
    pub fn new(addresses: impl IntoIterator<Item = Address>) -> Self {
        Self {
            addresses: addresses.into_iter().collect(),
            keep_trying: KeepTrying::default(),
            hold_limit: Duration::from_secs(3),
            on_change: None,
        }
    }

    /// One WebSocket address.
    pub fn websocket(url: impl Into<String>) -> Self {
        Self::new([Address::WebSocket(url.into())])
    }

    /// One in-process server, as `ww_niri_fake` serves.
    pub fn in_process(path: impl Into<String>) -> Self {
        Self::new([Address::InProcess(path.into())])
    }

    /// The wait between attempts and when to stop: by default 100 ms doubling up to 5 s, never
    /// stopping.
    #[must_use]
    pub fn keep_trying(mut self, keep_trying: KeepTrying) -> Self {
        self.keep_trying = keep_trying;
        self
    }

    /// How long the TV keeps a button or key held through [Command::Press] or
    /// [Command::KeyDown] without hearing from this client (default 3 s,
    /// [HOLD_LIMIT_MIN] to [HOLD_LIMIT_MAX]). The client renews held input every third of it,
    /// so a phone that vanishes leaves nothing held for longer.
    ///
    /// [Command::Press]: crate::Command::Press
    /// [Command::KeyDown]: crate::Command::KeyDown
    #[must_use]
    pub fn hold_limit(mut self, limit: Duration) -> Self {
        self.hold_limit = limit;
        self
    }

    /// Called after every change of the live model or the connection state, on the client's
    /// runtime: an egui app passes `ctx.request_repaint`.
    #[must_use]
    pub fn on_change(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_change = Some(Arc::new(f));
        self
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("addresses", &self.addresses)
            .field("keep_trying", &self.keep_trying)
            .field("hold_limit", &self.hold_limit)
            .field("on_change", &self.on_change.is_some())
            .finish()
    }
}
