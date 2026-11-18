//! WireWeaver control API of ww_niri, a niri fork for a TV box: windows and outputs, launching
//! apps, browser slots, media players (MPRIS), audio, and pointer and keyboard input, with changes
//! pushed to the client.
//!
//! The server lives in ww_niri (`src/ww/`) and is reached over WebSocket at `ws://HOST:7590/ww`.
//! Clients generate theirs with `ww_codegen!(ww_niri_api::WwNiri for ..., client = "std_client")`.
#![cfg_attr(not(feature = "std"), no_std)]

use ww_api::prelude::*;
use ww_si::prefix::{Centi, Milli, One};
use ww_si::units::{Hertz, Ratio};
use ww_si::{Fixed, Micros, Millis, Range};

ww_si::unit! {
    /// A screen pixel, a counted base of its own.
    pub struct Pixel = "px", base;
}

#[ww_api_root]
pub trait WwNiri {
    /// niri version, ww_niri version, hostname.
    fn info() -> Info<'i>;

    ww_impl!(windows: Windows);
    ww_impl!(outputs: Outputs);
    ww_impl!(apps: Apps);
    /// The browser slots, 0..count with the count from `browsers` in ww.kdl.
    ww_impl!(browsers[]: Browser);
    ww_impl!(media: Media);
    ww_impl!(audio: Audio);
    /// Pointer and keyboard, injected through kernel uinput devices as real devices.
    ww_impl!(input: Input);

    /// Windows, outputs, browsers and players coming and going, in order. After
    /// [Event::Resync] the client lists again.
    stream!(events: Event<'i>);
}

#[ww_trait]
pub trait Windows {
    fn list() -> RefVec<'i, Window<'i>>;

    /// Brings the window on screen: switches to its workspace and output, scrolls to its column
    /// and focuses it. `transition: None` uses niri's configured animations; `fullscreen` sets
    /// fullscreen on the way when given.
    fn show(
        id: WindowId,
        transition: Option<Transition>,
        fullscreen: Option<bool>,
    ) -> Result<(), WindowError>;

    /// Keyboard focus only, as niri's `focus-window`.
    fn focus(id: WindowId) -> Result<(), WindowError>;

    /// Asks the app to close the window; the app may refuse or ask first.
    fn close(id: WindowId) -> Result<(), WindowError>;

    fn set_fullscreen(id: WindowId, on: bool) -> Result<(), WindowError>;
}

#[ww_trait]
pub trait Outputs {
    fn list() -> RefVec<'i, Output<'i>>;
}

#[ww_trait]
pub trait Apps {
    /// Runs `argv` as the compositor's user, as niri's `spawn` does: no shell, `argv[0]` looked
    /// up in PATH. Any command is allowed.
    fn launch(argv: RefVec<'i, &'i str>) -> Result<Launched, LaunchError<'i>>;
}

/// One Firefox slot with its own persistent profile: `start`, then `open` as often as wanted,
/// then `stop`.
#[ww_trait]
pub trait Browser {
    /// Launches the slot's Firefox (kiosk, its profile, `about:blank`) and returns when it can be
    /// driven.
    fn start(#[default = true] fullscreen: bool) -> Result<(), BrowserError<'i>>;

    /// Loads `url` (http or https) in the slot's tab and returns when the navigation is
    /// committed.
    fn open(url: &'i str) -> Result<(), BrowserError<'i>>;

    /// Ends the slot's Firefox; its profile stays.
    fn stop() -> Result<(), BrowserError<'i>>;

    /// Shows the slot's window, as [Windows::show].
    fn show(
        transition: Option<Transition>,
        fullscreen: Option<bool>,
    ) -> Result<(), BrowserError<'i>>;

    /// Phase, URL, window and current player of the slot.
    property!(ro+observe state: BrowserState<'i>);

    /// Playback of the slot's current player, the MPRIS player of its Firefox process.
    ww_impl!(playback: Playback);
}

#[ww_trait]
pub trait Media {
    /// Every MPRIS player on the session bus, whoever started it; indexed by [PlayerId].
    ww_impl!(players[]: Player);

    fn list() -> RefVec<'i, PlayerInfo<'i>>;
}

#[ww_trait]
pub trait Player {
    /// Where the player comes from: a browser slot, or a window or app.
    property!(ro source: PlayerSource<'i>);

    ww_impl!(playback: Playback);
}

/// Shared by `browsers[i].playback` and `media.players[j].playback`.
#[ww_trait]
pub trait Playback {
    /// Status, track, position and capabilities.
    property!(ro+observe state: PlaybackState<'i>);

    fn play() -> Result<(), MediaError<'i>>;
    fn pause() -> Result<(), MediaError<'i>>;
    fn play_pause() -> Result<(), MediaError<'i>>;
    fn stop() -> Result<(), MediaError<'i>>;
    fn next() -> Result<(), MediaError<'i>>;
    fn previous() -> Result<(), MediaError<'i>>;

    /// Relative, MPRIS `Seek`; negative goes back.
    fn seek(by: Micros<i64>) -> Result<(), MediaError<'i>>;

    /// Absolute, MPRIS `SetPosition` on the current track.
    fn set_position(at: Micros<u64>) -> Result<(), MediaError<'i>>;
}

#[ww_trait]
pub trait Audio {
    /// The default sink's volume, up to 150 %.
    property!(rw+observe volume: Volume);

    property!(rw+observe muted: bool);

    /// The default sink, to show where the sound goes.
    property!(ro+observe sink: Sink<'i>);
}

/// Volume in percent, 0 to 150.
pub type Volume = Fixed<Ratio, Centi, u16, Range<0, 150>>;

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq))]
pub struct Info<'i> {
    pub niri_version: &'i str,
    pub ww_niri_version: &'i str,
    pub hostname: &'i str,
}

/// niri's window id, the same as in `niri msg windows`.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, PartialEq, Eq, Hash)
)]
pub struct WindowId(pub u64);

/// niri's workspace id.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, PartialEq, Eq, Hash)
)]
pub struct WorkspaceId(pub u64);

/// The index of `browsers[]`.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, PartialEq, Eq, Hash)
)]
pub struct BrowserSlot(pub u8);

/// The index of `media.players[]`: given when an MPRIS name appears, never reused while the
/// compositor runs.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, PartialEq, Eq, Hash)
)]
pub struct PlayerId(pub u32);

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq))]
pub struct Window<'i> {
    pub id: WindowId,
    pub app_id: Option<&'i str>,
    pub title: Option<&'i str>,
    pub pid: Option<u32>,
    pub workspace: Option<WorkspaceId>,
    pub output: Option<&'i str>,
    pub focused: bool,
    pub fullscreen: bool,
    /// Set when the window is a browser slot's Firefox.
    pub browser: Option<BrowserSlot>,
}

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq))]
pub struct Output<'i> {
    /// Connector name, like `HDMI-A-1`.
    pub name: &'i str,
    pub make: &'i str,
    pub model: &'i str,
    pub width: Fixed<Pixel, One, u16>,
    pub height: Fixed<Pixel, One, u16>,
    pub refresh: Fixed<Hertz, Milli, u32>,
    pub scale: Fixed<Ratio, Centi, u16>,
    pub focused: bool,
}

#[ww_trait]
pub trait Input {
    ww_impl!(pointer: Pointer);
    ww_impl!(keyboard: Keyboard);
}

#[ww_trait]
pub trait Pointer {
    /// Where the pointer is, and the window under it with the point in its own coordinates.
    /// Observers hear when either changes, whatever moved the pointer or the window.
    property!(ro+observe position: PointerPosition);

    /// Moves the pointer there.
    fn move_to(to: Point) -> Result<(), InputError<'i>>;

    /// Moves the pointer by a distance from where it is, as a mouse does: past the edge of the
    /// outputs it stops at the edge of the one it was on.
    fn move_by(dx: Px, dy: Px) -> Result<(), InputError<'i>>;

    /// Presses and releases `button` `count` times (2 is a double click, at most 3), at `at` when
    /// given, else where the pointer is, with `modifiers` held around the clicks.
    fn click(
        at: Option<Point>,
        button: Button,
        count: u8,
        modifiers: Modifiers,
    ) -> Result<(), InputError<'i>>;

    /// Holds a button down until [Pointer::release], the connection ends, or `hold` passes
    /// without another `press` of it (`None`: 10 s; 0 is a `BadArgument`).
    fn press(button: Button, hold: Option<Millis<u16>>) -> Result<(), InputError<'i>>;
    fn release(button: Button) -> Result<(), InputError<'i>>;

    /// Presses at `from`, moves to `to` in steps over `duration`, releases there.
    fn drag(
        from: Point,
        to: Point,
        button: Button,
        duration: Millis<u16>,
    ) -> Result<(), InputError<'i>>;

    /// Turns the wheel by `dx` and `dy` in v120 units, positive down and right: 120 is one
    /// notch, less scrolls smoothly. At `at` when given, with `modifiers` held around it, so
    /// ctrl and scroll zoom as a pinch does.
    fn scroll(
        at: Option<Point>,
        dx: i16,
        dy: i16,
        modifiers: Modifiers,
    ) -> Result<(), InputError<'i>>;
}

#[ww_trait]
pub trait Keyboard {
    /// Types the text with key presses of the configured keymap; nothing is typed when a
    /// character is on no key. A character the active layout lacks is typed on another layout
    /// of the keymap (`us,ru` types Cyrillic): niri switches to it for those keys and back after.
    fn type_text(text: &'i str) -> Result<(), InputError<'i>>;

    /// A key or a combination, pressed in order and released in reverse: `Return`, `ctrl+l`,
    /// `Ctrl+Shift+Tab`. The last part is an xkb keysym name, the others the modifiers ctrl,
    /// shift, alt and super in any case.
    fn press_keys(combo: &'i str) -> Result<(), InputError<'i>>;

    /// Holds a key (a keysym name or a modifier) down until [Keyboard::key_up], the connection
    /// ends, or `hold` passes without another `key_down` of it (`None`: 10 s; 0 is a
    /// `BadArgument`). Media and remote keys are keysyms too: `XF86AudioPlay`, `XF86Back`.
    fn key_down(key: &'i str, hold: Option<Millis<u16>>) -> Result<(), InputError<'i>>;
    fn key_up(key: &'i str) -> Result<(), InputError<'i>>;
}

/// An eased move, for [Windows::show].
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, PartialEq, Eq)
)]
pub struct Transition {
    pub duration: Millis<u16>,
    pub curve: Curve,
}

/// niri's easing curves.
#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Copy, Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum Curve {
    Linear,
    EaseOutQuad,
    EaseOutCubic,
    EaseOutExpo,
}

#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, PartialEq, Eq)
)]
pub struct Launched {
    pub pid: u32,
}

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq))]
pub struct BrowserState<'i> {
    pub phase: BrowserPhase,
    /// The tab's current URL.
    pub url: Option<&'i str>,
    /// The window title.
    pub title: Option<&'i str>,
    pub window: Option<WindowId>,
    /// The slot's MPRIS player, while its page plays media.
    pub player: Option<PlayerId>,
}

#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Copy, Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum BrowserPhase {
    Stopped,
    /// Launched; not yet driven or not yet mapped.
    Starting,
    Running,
    Stopping,
}

/// Why a browser slot stopped.
#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Copy, Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum StopReason {
    /// [Browser::stop], or the slot went away with a smaller count in ww.kdl.
    Stopped,
    /// The process exited by itself, with this code.
    Exited(i32),
    /// The process was killed by this signal.
    Crashed(i32),
    /// The remote-control connection went away while the process lived; it was killed.
    Lost,
}

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum PlayerSource<'i> {
    Browser(BrowserSlot),
    /// The window of the player's process.
    Window {
        id: WindowId,
        app_id: Option<&'i str>,
    },
    /// No window found for its process.
    App {
        pid: Option<u32>,
    },
}

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq))]
pub struct PlayerInfo<'i> {
    pub id: PlayerId,
    /// MPRIS `Identity`, like "Mozilla Firefox".
    pub identity: &'i str,
    pub source: PlayerSource<'i>,
}

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq))]
pub struct PlaybackState<'i> {
    pub status: PlaybackStatus,
    pub title: Option<&'i str>,
    pub artist: Option<&'i str>,
    pub length: Option<Micros<u64>>,
    /// Read when the state is sent; with `rate` the client extrapolates between updates.
    pub position: Micros<u64>,
    pub rate: Fixed<Ratio, Milli, u16>,
    pub can_seek: bool,
    pub can_pause: bool,
}

#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Copy, Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum PlaybackStatus {
    Stopped,
    Playing,
    Paused,
}

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq))]
pub struct Sink<'i> {
    pub name: &'i str,
    pub description: &'i str,
}

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum Event<'i> {
    WindowOpenedOrChanged(Window<'i>),
    WindowClosed(WindowId),
    WindowFocusChanged(Option<WindowId>),
    /// List the outputs again.
    OutputsChanged,
    /// The slot's Firefox is up and can be driven.
    BrowserStarted(BrowserSlot),
    BrowserStopped {
        slot: BrowserSlot,
        reason: StopReason,
    },
    PlayerAppeared(PlayerInfo<'i>),
    PlayerGone(PlayerId),
    /// The client fell behind and missed events: list everything again.
    Resync,
}

/// Logical pixels, as niri lays the outputs out.
pub type Px = Fixed<Pixel, One, i32>;

/// A point for the pointer.
#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Copy, Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum Point {
    /// niri's global space: the outputs as laid out.
    Global { x: Px, y: Px },
    /// The window's own coordinates: (0, 0) is the top left of what it draws.
    Window { id: WindowId, x: Px, y: Px },
}

#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, PartialEq, Eq)
)]
pub struct WindowPoint {
    pub id: WindowId,
    pub x: Px,
    pub y: Px,
}

#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, PartialEq, Eq)
)]
pub struct PointerPosition {
    pub x: Px,
    pub y: Px,
    /// The window under the pointer, and the point in its coordinates.
    pub window: Option<WindowPoint>,
}

#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Copy, Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Modifiers held around a click or a scroll.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, Default, PartialEq, Eq)
)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
}

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum InputError<'i> {
    NoSuchWindow,
    /// The window is not on screen: another workspace, scrolled out, or the overview is open.
    WindowNotVisible,
    /// The point is on no output.
    OffScreen,
    /// Not a keysym name, or no key of the keymap makes it.
    UnknownKey(&'i str),
    /// A character on no key of any layout of the configured keymap, as a Unicode scalar value.
    NotTypeable(u32),
    /// `button`, `count` or `hold` out of range.
    BadArgument,
    /// The input devices can't be made or written (no access to /dev/uinput).
    Unavailable(&'i str),
}

#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Copy, Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum WindowError {
    NoSuchWindow,
}

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum LaunchError<'i> {
    EmptyArgv,
    Spawn(&'i str),
}

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum BrowserError<'i> {
    NotRunning,
    AlreadyRunning,
    UrlNotHttp,
    Spawn(&'i str),
    /// The remote-control protocol answered with an error.
    Bidi(&'i str),
    NoWindow,
}

#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum MediaError<'i> {
    /// A browser slot whose page plays nothing.
    NoPlayer,
    /// The player doesn't offer this (MPRIS `CanSeek` and friends).
    NotSupported,
    Bus(&'i str),
}

ww_api::api_error!(WindowError);
ww_api::api_error!(LaunchError<'i>);
ww_api::api_error!(BrowserError<'i>);
ww_api::api_error!(MediaError<'i>);
ww_api::api_error!(InputError<'i>);
#[cfg(feature = "std")]
ww_api::api_error!(LaunchErrorOwned);
#[cfg(feature = "std")]
ww_api::api_error!(BrowserErrorOwned);
#[cfg(feature = "std")]
ww_api::api_error!(MediaErrorOwned);
#[cfg(feature = "std")]
ww_api::api_error!(InputErrorOwned);
