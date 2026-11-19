//! What a call to the TV, or starting a [Tv](crate::Tv), answers when it fails.

use std::sync::Arc;

use wire_weaver_client::CallError;
use wire_weaver_client::ww_error::CommonErrorOwned;
use ww_niri_api::{
    BrowserErrorOwned, InputErrorOwned, LaunchErrorOwned, MediaErrorOwned, WindowError,
};

/// Why a call failed: the TV is offline, the API answered with its own error, or the link did.
#[derive(Clone, Debug, thiserror::Error)]
pub enum TvError {
    /// Not connected: the call was not sent, or the link went while it was in flight.
    #[error("the TV is offline")]
    Offline,
    #[error("window: {0:?}")]
    Window(WindowError),
    #[error("launch: {0:?}")]
    Launch(LaunchErrorOwned),
    #[error("browser slot: {0:?}")]
    Browser(BrowserErrorOwned),
    #[error("player: {0:?}")]
    Media(MediaErrorOwned),
    #[error("input: {0:?}")]
    Input(InputErrorOwned),
    /// A protocol-level answer: no such index, busy, the property can't be read or written.
    #[error("the TV refused: {0:?}")]
    Refused(CommonErrorOwned),
    /// The TV's API has no such method: an older ww_niri.
    #[error("the TV does not implement {resource}")]
    Unimplemented { resource: String },
    /// A value outside what the API takes, caught before sending.
    #[error("{what} is out of range")]
    BadArgument { what: &'static str },
    /// The command queue of [Tv::send](crate::Tv::send) is full: the TV is far behind.
    #[error("too many commands wait for the TV")]
    QueueFull,
    /// The link or the client failed otherwise: a timeout, a decode error.
    #[error(transparent)]
    Client(Arc<wire_weaver_client::Error>),
    /// The `events` stream failed otherwise than by the link going.
    #[error(transparent)]
    Stream(Arc<wire_weaver_client::StreamError>),
}

impl From<wire_weaver_client::Error> for TvError {
    fn from(e: wire_weaver_client::Error) -> Self {
        match e {
            wire_weaver_client::Error::Disconnected => TvError::Offline,
            e => TvError::Client(Arc::new(e)),
        }
    }
}

/// An API error type that a [TvError] wraps.
pub(crate) trait ApiError {
    fn into_tv(self) -> TvError;
}

macro_rules! api_error {
    ($($ty:ty => $variant:ident),* $(,)?) => {$(
        impl ApiError for $ty {
            fn into_tv(self) -> TvError {
                TvError::$variant(self)
            }
        }
    )*};
}

api_error!(
    WindowError => Window,
    LaunchErrorOwned => Launch,
    BrowserErrorOwned => Browser,
    MediaErrorOwned => Media,
    InputErrorOwned => Input,
    CommonErrorOwned => Refused,
);

impl<E: ApiError> From<CallError<E>> for TvError {
    fn from(e: CallError<E>) -> Self {
        match e {
            CallError::Device(e) => e.into_tv(),
            CallError::Common(e) => TvError::Refused(e),
            CallError::Unimplemented { resource } => TvError::Unimplemented { resource },
            CallError::Client(e) => e.into(),
        }
    }
}

/// Why [Tv::start](crate::Tv::start) failed.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// [Config](crate::Config) names no address.
    #[error("no address to connect to")]
    NoAddress,
    /// [Config::hold_limit](crate::Config::hold_limit) out of range.
    #[error("the hold limit must be 100 ms to 60 s")]
    HoldLimit,
    /// The runtime thread of a [Tv](crate::Tv) started outside a tokio runtime could not be made.
    #[error("the TV's runtime: {0}")]
    Runtime(#[source] std::io::Error),
    /// wire_weaver refused the connection's settings.
    #[error(transparent)]
    Client(#[from] anyhow::Error),
}
