//! An in-memory TV serving ww_niri's control API (`ww_niri_api`) in-process, for the tests of
//! clients and apps and for their demo modes.
//!
//! [FakeTv::start] runs the generated server on `ww_device::Server` in a thread of its own and
//! registers it with `wire_weaver_client::in_process`; a generated client connects to
//! [FakeTv::path] without a socket:
//!
//! ```ignore
//! let tv = FakeTv::start(Options { clock: ClockMode::Manual, ..Options::default() });
//! let client = Client::config(|c| c.in_process_path(tv.path().to_owned())).connect().await?;
//! client.browsers(0).start(true).call().await?;
//! tv.advance(Duration::from_secs(1)); // the slot's window maps: Running
//! ```
//!
//! The TV has one 1920x1080 output, a terminal and a video player (a paused MPRIS player) side
//! by side on the first workspace, browser slots that go Stopped, Starting, Running after a start
//! delay, players whose position moves while they play, volume and mute, and a log of the keys,
//! text, buttons and wheel turns injected.
//!
//! For tests it records every call ([FakeTv::calls]), answers with errors on demand
//! ([FakeTv::fail_next]), adds latency, drops the connection, stays unreachable and sends a
//! `Resync`.

mod clock;
mod device;
mod keys;
mod model;
mod record;
mod server;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use clock::Clock;
pub use clock::ClockMode;
pub use model::{Cover, InputEntry, ReleasedBy, Sink};
use model::{Model, Setup};
use record::Record;
pub use record::{Call, ErrorKind, Fail, FailError, METHODS};
use tokio::sync::Notify;
use ww_niri_api::{InfoOwned, PointerPosition, WindowError, WindowId, WindowOwned};

/// How the fake TV starts.
#[derive(Clone, Debug)]
pub struct Options {
    /// The in-process path clients connect to; `None` picks one of its own.
    pub path: Option<String>,
    pub clock: ClockMode,
    /// Browser slots, as `browsers` in ww.kdl.
    pub browser_slots: u8,
    /// From a slot's start until its window maps and it is Running.
    pub start_delay: Duration,
    /// Every page opened in a slot plays media: the slot gets a playing player.
    pub pages_play: bool,
    /// What `info()` answers.
    pub info: InfoOwned,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            path: None,
            clock: ClockMode::Real,
            browser_slots: 2,
            start_delay: Duration::from_millis(500),
            pages_play: true,
            info: InfoOwned {
                niri_version: "fake".to_owned(),
                ww_niri_version: "fake".to_owned(),
                hostname: "fake-tv".to_owned(),
            },
        }
    }
}

/// What the test set for the connection.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Control {
    latency: Duration,
    drop_link: bool,
    /// Connections dropped by [FakeTv::drop_connection] so far, for the motion path.
    drops: u32,
    reachable: bool,
    stop: bool,
    links_up: u32,
}

/// State the device thread and the [FakeTv] handle share.
pub(crate) struct Shared {
    model: Mutex<Model>,
    record: Mutex<Record>,
    control: Mutex<Control>,
    clock: Clock,
    /// Wakes the device thread: the model or the control changed.
    wake: Notify,
    /// Wakes the motion path: the control changed.
    wake_motion: Notify,
    info: InfoOwned,
}

/// A poisoned lock panics: the fake is broken past use.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("the fake's locks are not poisoned")
}

impl Shared {
    /// Runs `f` on the model, brought up to the clock's time.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut Model) -> R) -> R {
        let mut model = lock(&self.model);
        model.tick(self.clock.now_us());
        f(&mut model)
    }

    /// Runs `f` on the model and has the device thread send what changed.
    pub(crate) fn change<R>(&self, f: impl FnOnce(&mut Model) -> R) -> R {
        let r = self.with(f);
        self.wake.notify_one();
        r
    }

    pub(crate) fn record(&self) -> MutexGuard<'_, Record> {
        lock(&self.record)
    }

    pub(crate) fn control(&self) -> Control {
        *lock(&self.control)
    }

    pub(crate) fn update_control(&self, f: impl FnOnce(&mut Control)) {
        f(&mut lock(&self.control));
        self.wake.notify_one();
        self.wake_motion.notify_one();
    }

    pub(crate) fn latency(&self) -> Duration {
        self.control().latency
    }
}

/// A running fake TV; dropping it stops the device thread and unregisters its path.
// spirit-level: allow(test-support-gated) apps run it in their demo mode, not only in tests
pub struct FakeTv {
    shared: Arc<Shared>,
    path: String,
    motion_path: String,
    thread: Option<JoinHandle<()>>,
}

impl FakeTv {
    /// Starts the device thread and registers the path.
    ///
    /// # Panics
    ///
    /// When the thread or its runtime can't be made.
    pub fn start(options: Options) -> Self {
        static STARTED: AtomicU32 = AtomicU32::new(0);
        let path = options.path.clone().unwrap_or_else(|| {
            let n = STARTED.fetch_add(1, Ordering::Relaxed);
            format!("ww_niri_fake/{}/{n}", std::process::id())
        });
        let setup = Setup {
            slots: options.browser_slots,
            start_delay_us: u64::try_from(options.start_delay.as_micros()).unwrap_or(u64::MAX),
            pages_play: options.pages_play,
        };
        let shared = Arc::new(Shared {
            model: Mutex::new(Model::new(&setup)),
            record: Mutex::new(Record::default()),
            control: Mutex::new(Control {
                latency: Duration::ZERO,
                drop_link: false,
                drops: 0,
                reachable: true,
                stop: false,
                links_up: 0,
            }),
            clock: Clock::new(options.clock),
            wake: Notify::new(),
            wake_motion: Notify::new(),
            info: options.info,
        });
        let motion_path = format!("{path}/motion");
        let thread = {
            let (shared, path, motion_path) = (shared.clone(), path.clone(), motion_path.clone());
            std::thread::Builder::new()
                .name("ww_niri_fake".to_owned())
                .spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                        .expect("a current-thread runtime can be made")
                        .block_on(async {
                            tokio::join!(
                                device::run(shared.clone(), path),
                                device::run_motion(shared, motion_path),
                            )
                        });
                })
                .expect("the device thread can be spawned")
        };
        Self {
            shared,
            path,
            motion_path,
            thread: Some(thread),
        }
    }

    /// The in-process path to connect to: `ClientConfig::in_process_path`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The same API on a simulated UDP device, as ww_niri serves it over UDP beside the
    /// WebSocket: for a client's pointer motion. One host at a time; the knobs act on it as on
    /// [FakeTv::path], and events and observed changes go to the client of `path` only.
    pub fn motion_path(&self) -> &str {
        &self.motion_path
    }

    // ---- time ----

    /// The fake's time since it started.
    pub fn now(&self) -> Duration {
        Duration::from_micros(self.shared.clock.now_us())
    }

    /// Moves a [ClockMode::Manual] clock on; what came due happens, and the client hears it.
    ///
    /// # Panics
    ///
    /// On a [ClockMode::Real] clock.
    pub fn advance(&self, by: Duration) {
        assert!(
            self.shared.clock.advance(by),
            "only a manual clock is advanced"
        );
        self.shared.change(|_| ());
    }

    // ---- knobs ----

    /// Every request waits this long before it is handled, so calls stay in flight.
    pub fn set_latency(&self, latency: Duration) {
        self.shared.update_control(|c| c.latency = latency);
    }

    /// Ends the client's connection, as a TV gone from Wi-Fi; a new one may come at once.
    pub fn drop_connection(&self) {
        self.shared.update_control(|c| {
            c.drop_link = true;
            c.drops = c.drops.wrapping_add(1);
        });
    }

    /// While unreachable the connection is dropped and no new one comes.
    pub fn set_reachable(&self, reachable: bool) {
        self.shared.update_control(|c| c.reachable = reachable);
    }

    /// Sends a `Resync` on the `events` stream and refreshes every observer, as after missed
    /// events.
    pub fn resync(&self) {
        self.shared.change(Model::resync);
    }

    /// The next call of `method` (a path of [METHODS], `browsers.open`, whatever the index)
    /// answers `fail` instead of acting. For a property, its next read or write.
    ///
    /// # Errors
    ///
    /// When no method has the path, the method answers another kind of error, or too many
    /// errors wait for it.
    pub fn fail_next(&self, method: &str, fail: Fail) -> Result<(), FailError> {
        self.shared.record().fail_next(method, fail)
    }

    /// How many times a client's link came up.
    pub fn links_up(&self) -> u32 {
        self.shared.control().links_up
    }

    // ---- what happened ----

    /// The calls made so far, oldest first.
    pub fn calls(&self) -> Vec<Call> {
        self.shared.record().calls()
    }

    /// The calls made since the last take.
    pub fn take_calls(&self) -> Vec<Call> {
        self.shared.record().take_calls()
    }

    /// The keys, text, buttons and wheel turns injected so far, oldest first.
    pub fn input_log(&self) -> Vec<InputEntry> {
        self.shared.with(|m| m.input_log())
    }

    /// The input injected since the last take.
    pub fn take_input_log(&self) -> Vec<InputEntry> {
        self.shared.with(Model::take_input_log)
    }

    pub fn windows(&self) -> Vec<WindowOwned> {
        self.shared.with(|m| m.windows())
    }

    /// The covered windows, by id.
    pub fn covers(&self) -> Vec<Cover> {
        self.shared.with(|m| m.covers())
    }

    pub fn pointer(&self) -> PointerPosition {
        self.shared.with(|m| m.pointer())
    }

    /// Volume in percent and mute.
    pub fn audio(&self) -> (u16, bool) {
        self.shared.with(|m| (m.volume, m.muted))
    }

    // ---- changes made elsewhere ----

    /// Opens a window on a workspace of its own and shows it, as an app started on the TV would.
    pub fn open_window(&self, app_id: &str, title: &str) -> WindowId {
        WindowId(self.shared.change(|m| m.open_window(app_id, title)))
    }

    /// The app closes the window, as if by itself.
    ///
    /// # Errors
    ///
    /// `NoSuchWindow` when there is none with this id.
    pub fn close_window(&self, id: WindowId) -> Result<(), WindowError> {
        self.shared.change(|m| m.close_window(id.0))
    }

    /// Volume (percent, 0 to 150) and mute changed on the TV.
    pub fn set_audio(&self, volume: u16, muted: bool) {
        self.shared.change(|m| m.set_audio(volume, muted));
    }

    /// The default sink changed, or went (`None`).
    pub fn set_sink(&self, sink: Option<Sink>) {
        self.shared.change(|m| m.set_sink(sink));
    }
}

impl Drop for FakeTv {
    fn drop(&mut self) {
        self.shared.update_control(|c| c.stop = true);
        if let Some(thread) = self.thread.take() {
            // A panic of the device thread was reported when it happened.
            drop(thread.join());
        }
    }
}
