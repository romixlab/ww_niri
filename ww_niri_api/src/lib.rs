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
    /// Items each client puts above the windows: covers, texts, a clock, notices.
    ww_impl!(overlay: Overlay);

    /// Windows, outputs, browsers and players coming and going, in order. After
    /// [Event::Resync] the client lists again.
    stream!(events: Event<'i>);
}

#[ww_trait]
pub trait Windows {
    fn list() -> RefVec<'i, Window<'i>>;

    /// Brings the window on screen: switches to its workspace and output, scrolls to its column
    /// and focuses it. `transition: None` uses niri's configured animations; `fullscreen` sets
    /// fullscreen on the way when given. A hidden window ([Windows::hide]) comes back on the
    /// active workspace of the focused output, with the size and fullscreen it had.
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

    /// Hides the window: it leaves its workspace and keeps running, mapped, but niri never
    /// draws or focuses it, in no workspace, overview or switch, until [Windows::show]. Listed
    /// with `hidden` set and no workspace or output; `focus` and `set_fullscreen` leave it
    /// hidden. Hiding a hidden window changes nothing.
    fn hide(id: WindowId) -> Result<(), WindowError>;
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
    /// Hidden by [Windows::hide]: never drawn or focused until [Windows::show].
    pub hidden: bool,
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

    /// Moves the pointer as [Pointer::move_by] does, without a reply: for a touchpad over UDP,
    /// where a lost move must not hold the later ones up. Each write carries the running totals
    /// of the sender's moves, so a lost write loses no distance: the next one brings it. The
    /// pointer moves by the change since the last write applied; a session's first write only
    /// sets where its totals start.
    sink!(motion: Totals<Delta>);

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

/// One layer above the windows where each connection owns its items (docs/ww/Overlay.md).
#[ww_trait]
pub trait Overlay {
    /// Puts up this connection's item `key`, or changes it: content, place, priority and end.
    /// The same key of another connection is another item. Answers the item's id, kept by a
    /// change. The item lifts at the end of `lasts`, on [Overlay::lift], on [Overlay::dismiss]
    /// when dismissible, when its window closes, or when this connection ends; then
    /// [Event::OverlayEnded] says why.
    fn put(key: &'i str, item: Item<'i>) -> Result<ItemId, OverlayError>;

    /// Lifts this connection's item `key`.
    fn lift(key: &'i str) -> Result<(), OverlayError>;

    /// Lifts an item put up as `dismissible`, whoever owns it: a person's "show it".
    fn dismiss(id: ItemId) -> Result<(), OverlayError>;

    /// Every item, front to back, with `own` set on this connection's.
    fn list() -> RefVec<'i, ItemInfo<'i>>;
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

/// The card of a cover, centred on its surface, line by line from the top.
#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq))]
pub struct Card<'i> {
    /// The large first line, like "Реклама".
    pub title: &'i str,
    /// The text before the countdown, like "Передача вернётся через": the line ends with the
    /// time left to the item's end as `m:ss`. `None`, or an item without an end: no countdown.
    pub countdown: Option<&'i str>,
    /// A large clock, `HH:MM` in the compositor's local time.
    pub clock: bool,
    /// A small last line, like the channel's name.
    pub footer: Option<&'i str>,
}

/// Longest `owner` of a browser slot, in bytes.
pub const OWNER_LEN_MAX: usize = 64;
/// Longest user agent of a browser slot, in bytes.
pub const USER_AGENT_LEN_MAX: usize = 512;

/// An overlay item's id: given by its first [Overlay::put], never reused while the compositor
/// runs.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)
)]
pub struct ItemId(pub u64);

/// What an overlay item is, where, and for how long.
#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq))]
pub struct Item<'i> {
    pub place: Place<'i>,
    pub content: Content<'i>,
    pub backdrop: Backdrop,
    /// Higher is drawn on top; equal ones by id, the later on top. By convention 0-63 ambient
    /// (a clock), 64-127 notices, 128-191 covers, 192-255 status of what plays.
    pub priority: u8,
    /// 1 ms to 15 min. `None`: until lifted, dismissed or the connection ends.
    pub lasts: Option<Millis<u32>>,
    /// The default sink is muted while the item is up; when the last muting item lifts, the
    /// sink gets back the mute it had before the first.
    pub mute: bool,
    /// Any connection may [Overlay::dismiss] it.
    pub dismissible: bool,
}

/// A surface and where on it.
#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq))]
pub struct Place<'i> {
    pub on: Surface<'i>,
    pub at: At,
}

/// What an overlay item is drawn on.
#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum Surface<'i> {
    /// The window: the item moves, scales and hides with it.
    Window(WindowId),
    /// The window of a browser slot, found at each [Overlay::put].
    Slot(BrowserSlot),
    /// An output by connector name; `None` is the focused one.
    Output(Option<&'i str>),
}

/// Where on its surface an item sits.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, PartialEq, Eq),
    ww_repr = unib32
)]
pub enum At {
    /// The whole surface, the content centred on it.
    Fill,
    /// The item's point named `anchor` at the surface's same point, moved by `dx` and `dy`;
    /// the item is as large as its content.
    Anchor { anchor: Anchor, dx: Px, dy: Px },
}

/// A corner, the middle of an edge, or the centre.
#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Copy, Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum Anchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// What an overlay item draws over its backdrop.
#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum Content<'i> {
    /// A cover's card; only at [At::Fill].
    Card(Card<'i>),
    /// Lines of text.
    Text { text: &'i str, size: TextSize },
    /// `HH:MM` in the compositor's local time.
    Clock { size: TextSize },
    /// A bold title and a smaller body.
    Notice {
        title: &'i str,
        body: Option<&'i str>,
    },
}

/// A text's size, laid out for a surface 1080 px high and scaled to the real one.
#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Copy, Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum TextSize {
    Small,
    Medium,
    Large,
}

/// What an item has under its content: a box around an anchored item's content, the whole
/// surface at [At::Fill]. At [At::Fill] on a window with `blur`, the window is not drawn, only
/// its blurred copy, and its popups are hidden.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, Default, PartialEq, Eq)
)]
pub struct Backdrop {
    /// Passes of the blur, 0 (none) to [BLUR_MAX].
    pub blur: u8,
    /// Percent of a warm dark over the blur, 0 to 100.
    pub dim: u8,
}

/// An item as [Overlay::list] shows it.
#[derive_shrink_wrap(owned(feature = "std"), derive(Clone, Debug, PartialEq, Eq))]
pub struct ItemInfo<'i> {
    pub id: ItemId,
    /// The owner's key.
    pub key: &'i str,
    /// This connection owns it.
    pub own: bool,
    pub item: Item<'i>,
}

/// Why an overlay item lifted.
#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Copy, Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum ItemEnd {
    /// Its `lasts` passed.
    Deadline,
    /// Its owner's [Overlay::lift].
    Lifted,
    /// [Overlay::dismiss].
    Dismissed,
    /// The owner's connection ended.
    Disconnected,
    /// Its window closed.
    TargetGone,
}

/// Longest `lasts` of an overlay item.
pub const ITEM_LASTS_MAX: core::time::Duration = core::time::Duration::from_secs(15 * 60);
/// Longest text of an overlay item, and longest line of a card, in bytes.
pub const ITEM_TEXT_LEN_MAX: usize = 500;
/// Longest key of an overlay item, in bytes.
pub const ITEM_KEY_LEN_MAX: usize = 64;
/// Most passes of a backdrop's blur.
pub const BLUR_MAX: u8 = 4;
/// Most items one connection has up at once.
pub const ITEMS_PER_OWNER_MAX: usize = 16;
/// Most items up at once.
pub const ITEMS_MAX: usize = 128;

#[cfg(feature = "std")]
impl ItemOwned {
    /// Checks the item and its `key` against the limits; answers how long it lasts.
    ///
    /// # Errors
    ///
    /// `BadArgument` for a content or place this side does not know, an empty or long key, a
    /// `lasts` of 0 or past [ITEM_LASTS_MAX], a blur past [BLUR_MAX], a dim past 100, a text past
    /// [ITEM_TEXT_LEN_MAX] or a card not at [At::Fill].
    pub fn check(&self, key: &str) -> Result<Option<core::time::Duration>, OverlayError> {
        let lasts = self
            .lasts
            .map(|l| core::time::Duration::from_millis(u64::from(l.get())));
        let texts: [Option<&str>; 4] = match &self.content {
            ContentOwned::Card(card) => [
                Some(card.title.as_str()),
                card.countdown.as_deref(),
                card.footer.as_deref(),
                None,
            ],
            ContentOwned::Text { text, .. } => [Some(text.as_str()), None, None, None],
            ContentOwned::Clock { .. } => [None; 4],
            ContentOwned::Notice { title, body } => {
                [Some(title.as_str()), body.as_deref(), None, None]
            }
            // From a newer client: this side can't draw it.
            ContentOwned::Unknown(_) => return Err(OverlayError::BadArgument),
        };
        let unknown_place = matches!(self.place.on, SurfaceOwned::Unknown(_))
            || matches!(
                self.place.at,
                At::Unknown(_)
                    | At::Anchor {
                        anchor: Anchor::Unknown(_),
                        ..
                    }
            );
        let bad = unknown_place
            || key.is_empty()
            || key.len() > ITEM_KEY_LEN_MAX
            || lasts.is_some_and(|l| l.is_zero() || l > ITEM_LASTS_MAX)
            || self.backdrop.blur > BLUR_MAX
            || self.backdrop.dim > 100
            || texts
                .into_iter()
                .flatten()
                .any(|t| t.len() > ITEM_TEXT_LEN_MAX)
            || (matches!(self.content, ContentOwned::Card(_)) && self.place.at != At::Fill);
        if bad {
            return Err(OverlayError::BadArgument);
        }
        Ok(lasts)
    }
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
    /// The person the slot belongs to, from `owner` of its `browser` line in ww.kdl: their
    /// sign-ins live in its profile.
    pub owner: Option<&'i str>,
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
    /// An overlay item lifted; not sent when a `put` changed it.
    OverlayEnded {
        id: ItemId,
        why: ItemEnd,
    },
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

/// A pointer move: how far right and down. [Pointer::motion] carries the running totals of them.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Copy, Clone, Debug, Default, PartialEq, Eq),
    final_structure
)]
pub struct Delta {
    pub dx: Px,
    pub dy: Px,
}

impl Accumulate for Delta {
    fn accumulate(self, change: Self) -> Self {
        Self {
            dx: self.dx.accumulate(change.dx),
            dy: self.dy.accumulate(change.dy),
        }
    }

    fn since(self, earlier: Self) -> Self {
        Self {
            dx: self.dx.since(earlier.dx),
            dy: self.dy.since(earlier.dy),
        }
    }
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

#[derive_shrink_wrap(borrowed, owned(feature = "std"), derive(Copy, Clone, Debug, PartialEq, Eq), ww_repr = unib32)]
pub enum OverlayError {
    NoSuchWindow,
    /// No item of this connection by that key, or none by that id.
    NoSuchItem,
    /// [Overlay::dismiss] of an item not put up as dismissible.
    NotDismissible,
    /// The connection has [ITEMS_PER_OWNER_MAX] items up, or the layer [ITEMS_MAX].
    TooMany,
    /// See [ItemOwned::check].
    BadArgument,
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
ww_api::api_error!(OverlayError);
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

#[cfg(test)]
mod tests {
    use super::{Accumulate, Delta, Px};

    fn delta(dx: i32, dy: i32) -> Delta {
        Delta {
            dx: Px::new(dx),
            dy: Px::new(dy),
        }
    }

    #[test]
    fn delta_adds_up_field_by_field_and_wraps() {
        let start = delta(i32::MAX - 1, i32::MIN);
        let total = start.accumulate(delta(3, -1));
        assert_eq!(total, delta(i32::MIN + 1, i32::MAX));
        assert_eq!(total.since(start), delta(3, -1));
    }
}
