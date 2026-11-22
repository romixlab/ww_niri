//! [Tv], the handle an app keeps: it starts the connection, the model task and the command task,
//! and hands out typed calls.

use std::sync::Arc;
use std::time::Duration;

use exact_units::Micros;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use ww_niri_api::{
    Button, InfoOwned, ItemId, ItemOwned, Modifiers, OutputOwned, PlayerId, PlayerInfoOwned, Point,
    Transition, WindowId, WindowOwned,
};

use crate::api::NiriClient;
use crate::command::{Command, PlaybackAction, PlaybackTarget};
use crate::config::{Address, Config, HOLD_LIMIT_MAX, HOLD_LIMIT_MIN};
use crate::error::{StartError, TvError};
use crate::model::{self, OverlayEnded, Shared};
use crate::queue::{self, Failed, Request};
use crate::runtime;
use crate::snapshot::{ConnectionState, Snapshot};

/// Commands on their way to the command task; [Tv::send] fails with [TvError::QueueFull] past it.
const REQUESTS_MAX: usize = 1024;
/// Failed commands kept for a slow [Tv::errors] reader; older ones are dropped.
const ERRORS_MAX: usize = 32;

/// What every clone of a [Tv] shares; dropping the last one stops its tasks.
struct Inner {
    client: NiriClient,
    shared: Arc<Shared>,
    requests: mpsc::Sender<Request>,
    errors: broadcast::Sender<Failed>,
    stop: watch::Sender<bool>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

/// A TV running ww_niri: connects in the background and again whenever the link goes, follows
/// the TV in a live [Snapshot], and sends commands.
///
/// Cheap to clone and `Send`; every clone is the same TV.
#[derive(Clone)]
pub struct Tv {
    inner: Arc<Inner>,
}

impl Tv {
    /// Starts connecting and returns at once.
    ///
    /// Runs on the caller's tokio runtime when there is one, else on a runtime thread of its own
    /// (an egui app, a sync CLI).
    ///
    /// # Errors
    ///
    /// When the config names no address, its hold limit is out of range, or the runtime thread
    /// can't be made.
    pub fn start(config: Config) -> Result<Self, StartError> {
        if config.addresses.is_empty() {
            return Err(StartError::NoAddress);
        }
        if !(HOLD_LIMIT_MIN..=HOLD_LIMIT_MAX).contains(&config.hold_limit) {
            return Err(StartError::HoldLimit);
        }
        runtime::start(
            |tv: &Self| tv.inner.stop.subscribe(),
            || Self::start_here(config),
        )
    }

    fn start_here(config: Config) -> Result<Self, StartError> {
        let (ws, in_process): (Vec<_>, Vec<_>) = config
            .addresses
            .iter()
            .partition(|a| matches!(a, Address::WebSocket(_)));
        let ws: Vec<String> = ws.into_iter().map(address).collect();
        let in_process: Vec<String> = in_process.into_iter().map(address).collect();
        let keep_trying = config.keep_trying;
        let client = NiriClient::config(move |mut c| {
            if !ws.is_empty() {
                c = c.websocket_urls(ws.clone());
            }
            if !in_process.is_empty() {
                c = c.in_process_paths(in_process.clone());
            }
            c.keep_trying(keep_trying)
        })
        .connect_keep_trying()?;

        let shared = Arc::new(Shared::new(config.on_change));
        let (requests, requests_rx) = mpsc::channel(REQUESTS_MAX);
        let (errors, _) = broadcast::channel(ERRORS_MAX);
        let (stop, _) = watch::channel(false);
        tokio::spawn(model::run(client.clone(), shared.clone(), stop.subscribe()));
        tokio::spawn(queue::run(
            client.clone(),
            shared.clone(),
            requests_rx,
            errors.clone(),
            config.hold_limit,
            stop.subscribe(),
        ));
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                shared,
                requests,
                errors,
                stop,
            }),
        })
    }

    // ---- the model ----

    /// The TV as of the last update.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.inner.shared.snapshot.borrow().clone()
    }

    /// Every new [Snapshot].
    pub fn watch_snapshot(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.inner.shared.snapshot.subscribe()
    }

    pub fn state(&self) -> ConnectionState {
        self.inner.shared.state.borrow().clone()
    }

    /// Every change of the [ConnectionState].
    pub fn watch_state(&self) -> watch::Receiver<ConnectionState> {
        self.inner.shared.state.subscribe()
    }

    /// Every overlay item that lifts on the TV from now on, whoever owns it, with the key of
    /// this client's own. This client's items when its link goes come as
    /// [ItemEnd::Disconnected](ww_niri_api::ItemEnd), made here: the TV lifted them, and the
    /// link to tell was gone. A slow reader misses the oldest and gets `Lagged`.
    pub fn overlay_ended(&self) -> broadcast::Receiver<OverlayEnded> {
        self.inner.shared.overlay_ended.subscribe()
    }

    // ---- commands ----

    /// Queues a command and returns at once; a failure comes on [Tv::errors].
    ///
    /// Each resource's commands go one call at a time, in order: pointer and keyboard as one,
    /// each browser slot, each player, windows, volume, mute. While a call is in flight, the
    /// commands queued behind it merge where one call does both: moves and wheel turns
    /// (without `at`) add up, a volume or mute replaces the one before, seeks add up and a
    /// position replaces the seeks before it. So a fast UI never floods the link, and the rate
    /// follows the round trip by itself. Clicks, keys and text keep their order.
    ///
    /// # Errors
    ///
    /// [TvError::QueueFull] when the TV is far behind.
    pub fn send(&self, command: Command) -> Result<(), TvError> {
        self.inner
            .requests
            .try_send(Request {
                command,
                reply: None,
            })
            .map_err(|_full| TvError::QueueFull)
    }

    /// The commands of [Tv::send] that failed, from now on.
    pub fn errors(&self) -> broadcast::Receiver<Failed> {
        self.inner.errors.subscribe()
    }

    /// Queues a command as [Tv::send] does and waits for its call's answer.
    async fn request(&self, command: Command) -> Result<(), TvError> {
        let (reply, answer) = oneshot::channel();
        self.inner
            .requests
            .try_send(Request {
                command,
                reply: Some(reply),
            })
            .map_err(|_full| TvError::QueueFull)?;
        answer.await.map_err(|_dropped| TvError::Offline)?
    }

    // ---- calls that answer something ----

    /// niri version, ww_niri version, hostname.
    ///
    /// # Errors
    ///
    /// When the TV is offline or the call fails.
    pub async fn info(&self) -> Result<InfoOwned, TvError> {
        Ok(self.inner.client.info().call().await?)
    }

    /// The windows as the TV lists them now; [Snapshot::windows] follows them.
    ///
    /// # Errors
    ///
    /// When the TV is offline or the call fails.
    pub async fn list_windows(&self) -> Result<Vec<WindowOwned>, TvError> {
        Ok(self.inner.client.windows().list().call().await?)
    }

    /// # Errors
    ///
    /// When the TV is offline or the call fails.
    pub async fn list_outputs(&self) -> Result<Vec<OutputOwned>, TvError> {
        Ok(self.inner.client.outputs().list().call().await?)
    }

    /// # Errors
    ///
    /// When the TV is offline or the call fails.
    pub async fn list_players(&self) -> Result<Vec<PlayerInfoOwned>, TvError> {
        Ok(self.inner.client.media().list().call().await?)
    }

    /// Runs `argv` as the TV's user, as niri's `spawn` does; answers the pid.
    ///
    /// # Errors
    ///
    /// When the TV is offline, `argv` is empty, or the spawn fails.
    pub async fn launch(&self, argv: Vec<String>) -> Result<u32, TvError> {
        Ok(self.inner.client.apps().launch(argv).call().await?.pid)
    }

    // ---- typed commands ----

    pub fn window(&self, id: WindowId) -> WindowCalls<'_> {
        WindowCalls { tv: self, id }
    }

    pub fn slot(&self, index: u8) -> SlotCalls<'_> {
        SlotCalls { tv: self, index }
    }

    /// The playback of any MPRIS player; a slot's is [SlotCalls::playback].
    pub fn player(&self, id: PlayerId) -> PlaybackCalls<'_> {
        PlaybackCalls {
            tv: self,
            target: PlaybackTarget::Player(id),
        }
    }

    pub fn audio(&self) -> AudioCalls<'_> {
        AudioCalls { tv: self }
    }

    /// This client's overlay items.
    pub fn overlay(&self) -> OverlayCalls<'_> {
        OverlayCalls { tv: self }
    }

    pub fn pointer(&self) -> PointerCalls<'_> {
        PointerCalls { tv: self }
    }

    pub fn keyboard(&self) -> KeyboardCalls<'_> {
        KeyboardCalls { tv: self }
    }

    /// Releases every button and key held through this client: call it when the app goes to the
    /// background.
    ///
    /// # Errors
    ///
    /// When the TV is offline (it released them itself) or the queue is full.
    pub async fn release_all(&self) -> Result<(), TvError> {
        self.request(Command::ReleaseAll).await
    }
}

fn address(a: &Address) -> String {
    match a {
        Address::WebSocket(s) | Address::InProcess(s) => s.clone(),
    }
}

/// Declares typed calls that queue a [Command] and wait for its answer.
macro_rules! calls {
    ($(
        $(#[$doc:meta])*
        fn $name:ident(&$self:ident $(, $arg:ident: $ty:ty)*) => $command:expr;
    )*) => {$(
        $(#[$doc])*
        ///
        /// # Errors
        ///
        /// When the TV is offline, answers with an error, or the queue is full.
        pub async fn $name(&$self $(, $arg: $ty)*) -> Result<(), TvError> {
            $self.tv.request($command).await
        }
    )*};
}

/// Calls on one window.
pub struct WindowCalls<'a> {
    tv: &'a Tv,
    id: WindowId,
}

impl WindowCalls<'_> {
    calls! {
        /// Brings the window on screen and focuses it; `transition: None` uses niri's animations.
        fn show(&self, transition: Option<Transition>, fullscreen: Option<bool>) =>
            Command::ShowWindow { id: self.id, transition, fullscreen };
        /// Keyboard focus only.
        fn focus(&self) => Command::FocusWindow(self.id);
        /// Asks the app to close the window.
        fn close(&self) => Command::CloseWindow(self.id);
        fn set_fullscreen(&self, on: bool) => Command::SetFullscreen { id: self.id, on };
        /// Hides the window: alive, but never drawn or focused until [WindowCalls::show], which
        /// brings it back on the active workspace.
        fn hide(&self) => Command::HideWindow(self.id);
    }
}

/// Calls on one browser slot.
pub struct SlotCalls<'a> {
    tv: &'a Tv,
    index: u8,
}

impl<'a> SlotCalls<'a> {
    calls! {
        /// Launches the slot's Firefox; answers when it can be driven.
        fn start(&self, fullscreen: bool) => Command::SlotStart { slot: self.index, fullscreen };
        /// Loads an http or https URL; answers when the navigation is committed.
        fn open(&self, url: String) => Command::SlotOpen { slot: self.index, url };
        fn stop(&self) => Command::SlotStop { slot: self.index };
        /// Shows the slot's window, as [WindowCalls::show].
        fn show(&self, transition: Option<Transition>, fullscreen: Option<bool>) =>
            Command::SlotShow { slot: self.index, transition, fullscreen };
    }

    /// The slot's current player.
    pub fn playback(&self) -> PlaybackCalls<'a> {
        PlaybackCalls {
            tv: self.tv,
            target: PlaybackTarget::Slot(self.index),
        }
    }
}

/// Playback controls of a slot's player or any player.
pub struct PlaybackCalls<'a> {
    tv: &'a Tv,
    target: PlaybackTarget,
}

impl PlaybackCalls<'_> {
    fn command(&self, action: PlaybackAction) -> Command {
        Command::Playback {
            target: self.target,
            action,
        }
    }

    calls! {
        fn play(&self) => self.command(PlaybackAction::Play);
        fn pause(&self) => self.command(PlaybackAction::Pause);
        fn play_pause(&self) => self.command(PlaybackAction::PlayPause);
        fn stop(&self) => self.command(PlaybackAction::Stop);
        fn next(&self) => self.command(PlaybackAction::Next);
        fn previous(&self) => self.command(PlaybackAction::Previous);
        /// Relative; negative goes back. Seeks queued behind one in flight add up.
        fn seek(&self, by: Micros<i64>) => self.command(PlaybackAction::SeekBy { by });
        /// Absolute on the current track; latest-wins while a call is in flight.
        fn set_position(&self, at: Micros<u64>) => self.command(PlaybackAction::SetPosition { at });
    }
}

/// This client's items on the TV's overlay layer.
pub struct OverlayCalls<'a> {
    tv: &'a Tv,
}

impl OverlayCalls<'_> {
    calls! {
        /// Puts up this client's item `key`, or changes it; a put queued behind one of the same
        /// key replaces it. It lifts at the end of its `lasts`, or when this client's connection
        /// ends, so renew it while it should stay. With `mute` the TV's sink is muted from the
        /// first muting item until the last one lifts, then gets its mute from before; changes
        /// leave it as it is. [Tv::overlay_ended] tells when and why it lifted.
        fn put(&self, key: String, item: ItemOwned) =>
            Command::OverlayPut { key, item: Box::new(item) };
        /// Lifts this client's item `key`.
        fn lift(&self, key: String) => Command::OverlayLift(key);
        /// Lifts a dismissible item, whoever owns it: a person's "show it".
        fn dismiss(&self, id: ItemId) => Command::OverlayDismiss(id);
    }
}

/// The default sink.
pub struct AudioCalls<'a> {
    tv: &'a Tv,
}

impl AudioCalls<'_> {
    calls! {
        /// Percent, 0 to 150; latest-wins while a call is in flight.
        fn set_volume(&self, percent: u16) => Command::SetVolume(percent);
        fn set_muted(&self, on: bool) => Command::SetMuted(on);
    }
}

/// The TV's pointer.
pub struct PointerCalls<'a> {
    tv: &'a Tv,
}

impl PointerCalls<'_> {
    calls! {
        fn move_to(&self, to: Point) => Command::MoveTo(to);
        /// Relative, in logical pixels; moves queued behind one in flight add up.
        fn move_by(&self, dx: i32, dy: i32) => Command::MoveBy { dx, dy };
        /// `count` 1 to 3 at `at`, or where the pointer is, with `modifiers` held around.
        fn click(&self, at: Option<Point>, button: Button, count: u8, modifiers: Modifiers) =>
            Command::Click { at, button, count, modifiers };
        /// Holds the button until [PointerCalls::release]; renewed within the hold limit.
        fn press(&self, button: Button) => Command::Press(button);
        fn release(&self, button: Button) => Command::Release(button);
        fn drag(&self, from: Point, to: Point, button: Button, duration: Duration) =>
            Command::Drag { from, to, button, duration };
        /// v120 units, positive down and right; turns queued behind one in flight add up.
        fn scroll(&self, at: Option<Point>, dx: i16, dy: i16, modifiers: Modifiers) =>
            Command::Scroll { at, dx, dy, modifiers };
    }
}

/// The TV's keyboard.
pub struct KeyboardCalls<'a> {
    tv: &'a Tv,
}

impl KeyboardCalls<'_> {
    calls! {
        /// Types the text with key presses of the TV's keymap.
        fn type_text(&self, text: String) => Command::TypeText(text);
        /// A key or a combination: `Return`, `ctrl+l`.
        fn press_keys(&self, combo: String) => Command::PressKeys(combo);
        /// Holds a key until [KeyboardCalls::key_up]; renewed within the hold limit.
        fn key_down(&self, key: String) => Command::KeyDown(key);
        fn key_up(&self, key: String) => Command::KeyUp(key);
    }
}
