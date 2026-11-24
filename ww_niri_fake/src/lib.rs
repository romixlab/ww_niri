//! An in-memory TV serving ww_niri's control API (`ww_niri_api`) in-process, for the tests of
//! clients and apps and for their demo modes.
//!
//! [FakeTv::start] serves the generated server on `wire_weaver_server`, as ww_niri does, in a
//! thread of its own, and registers it with `wire_weaver_client::in_process`; generated clients
//! connect to [FakeTv::path] without a socket, any number at once:
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
//! ([FakeTv::fail_next]), adds latency, drops the connections, stays unreachable, moves to
//! another path ([FakeTv::rebind]) and sends a `Resync`. When a connection ends, what it held
//! comes up and its overlay items lift, as on the real server.

mod clock;
mod device;
mod keys;
mod model;
mod mounts;
mod record;
mod server;

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use clock::Clock;
pub use clock::ClockMode;
pub use model::{InputEntry, OverlayItem, ReleasedBy, Sink};
use model::{Model, Out, Setup};
use record::Record;
pub use record::{Call, ErrorKind, Fail, FailError, METHODS};
use tokio::sync::{watch, Notify};
use wire_weaver_server::Fanout;
use ww_niri_api::{InfoOwned, PointerPosition, ShowOnlyOwned, WindowError, WindowId, WindowOwned};

/// How the fake TV starts.
#[derive(Clone, Debug)]
pub struct Options {
    /// The in-process path clients connect to; `None` picks one of its own.
    pub path: Option<String>,
    pub clock: ClockMode,
    /// Browser slots, as `browsers` in ww.kdl.
    pub browser_slots: u8,
    /// The owner of each slot by index, as `browser N owner=...` in ww.kdl; a slot past the
    /// list has none.
    pub slot_owners: Vec<Option<String>>,
    /// From a slot's start until its window maps and it is Running.
    pub start_delay: Duration,
    /// Every page opened in a slot plays media: the slot gets a playing player.
    pub pages_play: bool,
    /// What `info()` answers.
    pub info: InfoOwned,
    /// Reports this hash at link setup instead of the generated `api_hash()`, as a build whose
    /// crate version moved, with no shape change, would (P2618): a client built against the real
    /// API has to see the fake's shape is still the same, instead of every call being refused on
    /// wire w0, the design line. `None`, the default, reports the real `api_hash()`.
    pub api_hash: Option<wire_weaver::ww_version::ApiHashPair<'static>>,
    /// Also serves the API on this Unix socket, as a program a ww_niri mounts at one of its
    /// `ext` slots does: for the tests of mounts. The knobs don't act on it.
    pub socket: Option<std::path::PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            path: None,
            clock: ClockMode::Real,
            browser_slots: 2,
            slot_owners: Vec::new(),
            start_delay: Duration::from_millis(500),
            pages_play: true,
            api_hash: None,
            socket: None,
            info: InfoOwned {
                niri_version: "fake".to_owned(),
                ww_niri_version: "fake".to_owned(),
                hostname: "fake-tv".to_owned(),
            },
        }
    }
}

/// What the test set for the connections.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Control {
    latency: Duration,
    /// [FakeTv::drop_connection]s so far: each ends the connections open at the time.
    drops: u32,
    reachable: bool,
    /// [FakeTv::rebind]s so far: which path the listeners are at.
    binds: u32,
    /// [FakeTv::restart_reporting_hash]es so far: each ends the servers, which start again
    /// reporting [Control::api_hash].
    restarts: u32,
    /// [Options::api_hash], then what the last restart set.
    api_hash: Option<wire_weaver::ww_version::ApiHashPair<'static>>,
    stop: bool,
}

/// Changes on the TV that every connection hears of.
#[derive(Clone, Debug)]
pub(crate) enum News {
    Out(Out),
    /// Changes were lost: the client lists again.
    Resync,
}

/// Changes a connection may fall behind on before it gets a `Resync`.
const NEWS_QUEUE_MAX: usize = 256;

/// State the device thread and the [FakeTv] handle share.
pub(crate) struct Shared {
    model: Mutex<Model>,
    record: Mutex<Record>,
    control: watch::Sender<Control>,
    clock: Clock,
    /// Wakes the ticker: the model changed, its next deadline may have too.
    wake: Notify,
    /// The model's changes, to every connection.
    pub(crate) news: Fanout<News>,
    /// The last connection id handed out, for what each holds.
    holders: AtomicU64,
    /// Links that came up on the main path.
    links_up: AtomicU32,
    /// The rebinds each path's listener follows, main and motion; `None` while unregistered.
    bound: Mutex<[Option<u32>; 2]>,
    /// Signals a change of `bound`.
    bound_changed: Condvar,
    info: InfoOwned,
    /// The `ext` mount slots.
    pub(crate) ext: mounts::Ext,
    /// [Options::socket].
    socket: Option<std::path::PathBuf>,
}

/// A poisoned lock panics: the fake is broken past use.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("the fake's locks are not poisoned")
}

impl Shared {
    /// Runs `f` on the model, brought up to the clock's time; what changed goes to every
    /// connection, and the ticker waits for the model's next deadline.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut Model) -> R) -> R {
        let r = self.apply(f);
        self.wake.notify_one();
        r
    }

    /// Brings the model up to the clock; answers its next deadline.
    pub(crate) fn catch_up(&self) -> Option<u64> {
        self.apply(|m| m.next_deadline())
    }

    fn apply<R>(&self, f: impl FnOnce(&mut Model) -> R) -> R {
        let mut model = lock(&self.model);
        model.tick(self.clock.now_us());
        let r = f(&mut model);
        let (outs, behind) = model.drain();
        if behind {
            self.news.send(News::Resync);
        }
        for out in outs {
            self.news.send(News::Out(out));
        }
        r
    }

    /// Path `index` (0 main, 1 motion) is now registered after `binds` rebinds, or not at all.
    pub(crate) fn set_bound(&self, index: usize, binds: Option<u32>) {
        lock(&self.bound)[index] = binds;
        self.bound_changed.notify_all();
    }

    /// Waits until both listeners follow the control: registered at the path of its rebinds
    /// while reachable, unregistered while not.
    pub(crate) fn wait_listening(&self) {
        let control = self.control();
        let wanted = [control.reachable.then_some(control.binds); 2];
        let bound = lock(&self.bound);
        let listening = self.bound_changed.wait_while(bound, |b| *b != wanted);
        drop(listening.expect("the fake's locks are not poisoned"));
    }

    /// A new connection's id, never handed out before.
    pub(crate) fn next_holder(&self) -> u64 {
        self.holders.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) fn record(&self) -> MutexGuard<'_, Record> {
        lock(&self.record)
    }

    pub(crate) fn control(&self) -> Control {
        *self.control.borrow()
    }

    pub(crate) fn update_control(&self, f: impl FnOnce(&mut Control)) {
        self.control.send_modify(f);
    }

    pub(crate) fn latency(&self) -> Duration {
        self.control().latency
    }
}

/// A running fake TV; dropping it stops the device thread and unregisters its path.
// spirit-level: allow(test-support-gated) apps run it in their demo mode, not only in tests
pub struct FakeTv {
    shared: Arc<Shared>,
    /// The paths of the first bind, which [FakeTv::rebind] moves from.
    base: (String, String),
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
            owners: options.slot_owners.clone(),
            start_delay_us: u64::try_from(options.start_delay.as_micros()).unwrap_or(u64::MAX),
            pages_play: options.pages_play,
        };
        let shared = Arc::new(Shared {
            model: Mutex::new(Model::new(&setup)),
            record: Mutex::new(Record::default()),
            control: watch::Sender::new(Control {
                latency: Duration::ZERO,
                drops: 0,
                reachable: true,
                binds: 0,
                restarts: 0,
                api_hash: options.api_hash,
                stop: false,
            }),
            clock: Clock::new(options.clock),
            wake: Notify::new(),
            news: Fanout::new(NEWS_QUEUE_MAX),
            holders: AtomicU64::new(0),
            links_up: AtomicU32::new(0),
            bound: Mutex::new([None; 2]),
            bound_changed: Condvar::new(),
            info: options.info,
            ext: mounts::Ext::new(),
            socket: options.socket,
        });
        let motion_path = format!("{path}/motion");
        let thread = {
            let (shared, path, motion_path) = (shared.clone(), path.clone(), motion_path.clone());
            std::thread::Builder::new()
                .name("ww_niri_fake".to_owned())
                .spawn(move || {
                    // io: the mounted programs' sockets and the fake's own
                    tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .enable_io()
                        .build()
                        .expect("a current-thread runtime can be made")
                        .block_on(device::run(shared, path, motion_path));
                })
                .expect("the device thread can be spawned")
        };
        Self {
            shared,
            base: (path.clone(), motion_path.clone()),
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
    /// WebSocket: for a client's pointer motion. Up to 8 hosts at once; the knobs act on it as
    /// on [FakeTv::path].
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
        self.shared.with(|_| ());
    }

    // ---- knobs ----

    /// Every request waits this long before it is handled, so calls stay in flight.
    pub fn set_latency(&self, latency: Duration) {
        self.shared.update_control(|c| c.latency = latency);
    }

    /// Ends the open connections, as a TV gone from Wi-Fi; new ones may come at once.
    pub fn drop_connection(&self) {
        self.shared
            .update_control(|c| c.drops = c.drops.wrapping_add(1));
    }

    /// Restarts the TV's servers, as ww_niri does after an update that moved its API: the open
    /// connections end, and the links that come next see `hash` as the API's hash (`None`: the
    /// generated `api_hash()`). The model and the paths stay. Returns once the restart is
    /// ordered; a client that keeps trying finds the TV again by itself.
    pub fn restart_reporting_hash(
        &self,
        hash: Option<wire_weaver::ww_version::ApiHashPair<'static>>,
    ) {
        self.shared.update_control(|c| {
            c.api_hash = hash;
            c.restarts = c.restarts.wrapping_add(1);
            c.drops = c.drops.wrapping_add(1);
        });
    }

    /// Stops listening at [FakeTv::path] and [FakeTv::motion_path] and listens at new ones, as
    /// ww_niri does when its address changes: open connections go on, new clients find nobody
    /// at the old paths. Returns once both listen at their new paths (while unreachable, once
    /// reachable again they do); answers the new [FakeTv::path].
    pub fn rebind(&mut self) -> &str {
        let mut binds = 0;
        self.shared.update_control(|c| {
            c.binds += 1;
            binds = c.binds;
        });
        self.shared.wait_listening();
        self.path = device::bound_path(&self.base.0, binds);
        self.motion_path = device::bound_path(&self.base.1, binds);
        &self.path
    }

    /// While unreachable the connections are dropped and no new one comes. Returns once the
    /// paths are unregistered, or registered again.
    pub fn set_reachable(&self, reachable: bool) {
        self.shared.update_control(|c| c.reachable = reachable);
        self.shared.wait_listening();
    }

    /// Sends a `Resync` on the `events` stream and refreshes every observer, as after missed
    /// events.
    pub fn resync(&self) {
        self.shared.with(Model::resync);
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

    /// Mounts the program serving on the Unix socket `socket` at `ext[name]`, as a `mount`
    /// line of ww.kdl does: its `presence` turns `Up` once the TV reached it and learned its API,
    /// and `Absent` while it is gone. A name mounted already keeps its index and takes the new
    /// socket.
    ///
    /// # Panics
    ///
    /// When `name` is empty or longer than 64 bytes.
    pub fn mount(&self, name: &str, socket: impl Into<std::path::PathBuf>) {
        assert!(!name.is_empty(), "a mount name is not empty");
        assert!(
            name.len() <= mounts::MOUNT_NAME_LEN_MAX,
            "a mount name is at most 64 bytes"
        );
        self.shared.ext.mount(name, socket.into());
    }

    /// Takes `ext[name]` away, as removing its `mount` line does; false when there was none.
    pub fn unmount(&self, name: &str) -> bool {
        self.shared.ext.unmount(name)
    }

    /// Whether the program at `ext[name]` is up: `None` when nothing is mounted there.
    pub fn mount_presence(
        &self,
        name: &str,
    ) -> Option<wire_weaver::ww_api::mount::MountPresenceOwned> {
        let index = self.shared.ext.index_of(name)?;
        Some(self.shared.ext.presence(index))
    }

    /// How many times a client's link came up on [FakeTv::path].
    pub fn links_up(&self) -> u32 {
        self.shared.links_up.load(Ordering::Relaxed)
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

    /// The overlay's items, front to back.
    pub fn overlay(&self) -> Vec<OverlayItem> {
        self.shared.with(|m| m.overlay())
    }

    pub fn pointer(&self) -> PointerPosition {
        self.shared.with(|m| m.pointer())
    }

    /// Volume in percent and mute.
    pub fn audio(&self) -> (u16, bool) {
        self.shared.with(|m| (m.volume, m.muted))
    }

    /// The overlay's `show_only` filter.
    pub fn show_only(&self) -> Option<ShowOnlyOwned> {
        self.shared.with(|m| m.show_only())
    }

    // ---- changes made elsewhere ----

    /// Opens a window on a workspace of its own and shows it, as an app started on the TV would.
    pub fn open_window(&self, app_id: &str, title: &str) -> WindowId {
        WindowId(self.shared.with(|m| m.open_window(app_id, title)))
    }

    /// The app closes the window, as if by itself.
    ///
    /// # Errors
    ///
    /// `NoSuchWindow` when there is none with this id.
    pub fn close_window(&self, id: WindowId) -> Result<(), WindowError> {
        self.shared.with(|m| m.close_window(id.0))
    }

    /// An image named `name` is now in the backgrounds folder, for fills.
    pub fn add_background(&self, name: &str) {
        self.shared.with(|m| m.add_background(name));
    }

    /// Volume (percent, 0 to 150) and mute changed on the TV.
    pub fn set_audio(&self, volume: u16, muted: bool) {
        self.shared.with(|m| m.set_audio(volume, muted));
    }

    /// The default sink changed, or went (`None`).
    pub fn set_sink(&self, sink: Option<Sink>) {
        self.shared.with(|m| m.set_sink(sink));
    }

    /// The TV's screen came on the HDMI port, or went (TV off or unplugged): `outputs.list` is
    /// empty without it, windows have no output, and `OutputsChanged` tells the client.
    pub fn set_screen_connected(&self, connected: bool) {
        self.shared.with(|m| m.set_screen_connected(connected));
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
