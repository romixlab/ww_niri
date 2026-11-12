//! Browser slots: one Firefox process per slot, each with its own persistent profile, driven over
//! WebDriver BiDi on a port of its own on 127.0.0.1.
//!
//! [Browsers] is shared by every connection. `start` spawns the slot's Firefox, reads its stderr
//! until BiDi listens, opens a session and finds the slot's tab; a task per running Firefox (the
//! supervisor) then follows the BiDi events, drains stderr, waits for the window and notices the
//! exit through a pidfd. Browser events go to every connection that has the events stream open.

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::future::Future;
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Child, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use async_channel::Sender;
use calloop::futures::Scheduler;
use calloop::io::Async;
use calloop::timer::TimeoutFuture;
use calloop::LoopHandle;
use directories::ProjectDirs;
use futures_util::future::{select, select_all, Either};
use futures_util::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};
use serde_json::json;
use ww_niri_api::{
    BrowserErrorOwned, BrowserPhase, BrowserSlot, BrowserStateOwned, EventOwned, StopReason,
    WindowOwned,
};

use super::bidi::{self, Bidi, BidiError};
use super::host::{call, Run};
use super::hub::Hub;

/// How long Firefox may take to start listening for BiDi.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(20);
/// Firefox's line on stderr once BiDi listens: `WebDriver BiDi listening on ws://127.0.0.1:9300`.
const LISTENING: &str = "WebDriver BiDi listening on ws://";
/// Events of the slot's tab the supervisor follows.
const SUBSCRIBED: [&str; 4] = [
    "browsingContext.contextCreated",
    "browsingContext.contextDestroyed",
    "browsingContext.navigationStarted",
    "browsingContext.load",
];

pub struct Settings {
    /// The browser's command line, before the slot's own arguments.
    pub command: Vec<String>,
    /// Slot `n` keeps its profile in `<profiles>/browser-<n>`.
    pub profiles: PathBuf,
    /// Slot `n` listens for BiDi on `127.0.0.1:<port_base + n>`.
    pub port_base: u16,
    /// How long `stop` gives Firefox after `browser.close`, and again after SIGTERM.
    pub stop_grace: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        let profiles = ProjectDirs::from("", "", "ww_niri")
            .map(|dirs| dirs.data_dir().to_owned())
            .unwrap_or_else(|| PathBuf::from("/tmp/ww_niri"));
        Self {
            command: vec!["firefox".to_owned()],
            profiles,
            port_base: 9300,
            stop_grace: Duration::from_secs(5),
        }
    }
}

/// What the slots need of the loop, without its data type: fds made async and timeouts.
pub struct LoopIo {
    tcp: Box<dyn Fn(TcpStream) -> io::Result<Async<'static, TcpStream>>>,
    file: Box<dyn Fn(File) -> io::Result<Async<'static, File>>>,
    unix: Box<dyn Fn(UnixStream) -> io::Result<Async<'static, UnixStream>>>,
    deadline: Box<dyn Fn(Instant) -> TimeoutFuture>,
}

impl LoopIo {
    pub fn new<D: 'static>(handle: LoopHandle<'static, D>) -> Self {
        let (tcp, file, unix) = (handle.clone(), handle.clone(), handle.clone());
        Self {
            tcp: Box::new(move |stream| tcp.adapt_io(stream).map_err(io::Error::other)),
            file: Box::new(move |f| file.adapt_io(f).map_err(io::Error::other)),
            unix: Box::new(move |stream| unix.adapt_io(stream).map_err(io::Error::other)),
            deadline: Box::new(move |at| TimeoutFuture::from_deadline(&handle, at)),
        }
    }

    fn timeout(&self, after: Duration) -> TimeoutFuture {
        (self.deadline)(Instant::now() + after)
    }

    pub fn unix(&self, stream: UnixStream) -> io::Result<Async<'static, UnixStream>> {
        (self.unix)(stream)
    }

    pub async fn sleep(&self, time: Duration) {
        self.timeout(time).await;
    }
}

/// Awaits `fut` for at most `after`; `None` when time ran out.
async fn timeout<T>(io: &LoopIo, after: Duration, fut: impl Future<Output = T>) -> Option<T> {
    match select(std::pin::pin!(fut), io.timeout(after)).await {
        Either::Left((out, _)) => Some(out),
        Either::Right(_) => None,
    }
}

#[derive(Default)]
struct Slot {
    run: Option<Running>,
}

/// A slot's Firefox, from spawn until its exit is handled.
struct Running {
    phase: BrowserPhase,
    pid: u32,
    /// `None` until the session is up.
    bidi: Option<Rc<Bidi>>,
    /// The slot's tab: its top-level browsing context.
    context: Option<String>,
    url: Option<String>,
    /// Counts the URLs the browser reported, so `open` won't overwrite a newer one.
    url_seq: u64,
    /// Its window has mapped.
    window: bool,
    stopping: bool,
    lost: bool,
    /// Told when the process has exited.
    exited: Vec<Sender<()>>,
}

pub struct Browsers {
    settings: Settings,
    run: Run,
    io: LoopIo,
    scheduler: Scheduler<()>,
    /// The count from ww.kdl; `slots` is longer while slots past it are still stopping.
    wanted: Cell<u8>,
    slots: RefCell<Vec<Slot>>,
    hub: Rc<Hub>,
}

type Res = Result<(), BrowserErrorOwned>;

fn spawn_error(err: impl std::fmt::Display) -> BrowserErrorOwned {
    BrowserErrorOwned::Spawn(err.to_string())
}

fn bidi_error(err: BidiError) -> BrowserErrorOwned {
    BrowserErrorOwned::Bidi(err.to_string())
}

impl Browsers {
    pub fn new(
        settings: Settings,
        run: Run,
        io: LoopIo,
        scheduler: Scheduler<()>,
        hub: Rc<Hub>,
        count: u8,
    ) -> Rc<Self> {
        let slots = (0..count).map(|_| Slot::default()).collect();
        Rc::new(Self {
            settings,
            run,
            io,
            scheduler,
            wanted: Cell::new(count),
            slots: RefCell::new(slots),
            hub,
        })
    }

    /// The slot count from ww.kdl: indices 0..count are valid.
    pub fn count(&self) -> u8 {
        self.wanted.get()
    }

    /// A new count from ww.kdl: slots past it are stopped, and go once their Firefox has exited.
    pub fn set_count(self: &Rc<Self>, count: u8) {
        let old = self.wanted.replace(count);
        if old == count {
            return;
        }
        {
            let mut slots = self.slots.borrow_mut();
            if slots.len() < usize::from(count) {
                slots.resize_with(usize::from(count), Slot::default);
            }
        }
        for slot in count..old {
            let this = self.clone();
            self.schedule(async move {
                let _ = this.stop(slot).await;
                this.trim();
            });
        }
        self.trim();
        // Clients list again to see the new range.
        self.emit(EventOwned::Resync);
    }

    /// Drops the slots past the count that have no Firefox left.
    fn trim(&self) {
        let mut slots = self.slots.borrow_mut();
        let running = slots
            .iter()
            .rposition(|s| s.run.is_some())
            .map_or(0, |i| i + 1);
        slots.truncate(running.max(usize::from(self.wanted.get())));
    }

    fn emit(&self, event: EventOwned) {
        self.hub.emit(event);
    }

    fn schedule(&self, fut: impl Future<Output = ()> + 'static) {
        if let Err(err) = self.scheduler.schedule(fut) {
            warn!("ww: error scheduling a browser task: {err:?}");
        }
    }

    fn with_run<R>(&self, slot: u8, f: impl FnOnce(&mut Running) -> R) -> Option<R> {
        let mut slots = self.slots.borrow_mut();
        slots.get_mut(usize::from(slot))?.run.as_mut().map(f)
    }

    /// Follows niri's window events, so that observers of a slot's state hear when its window's
    /// title changes or the window goes, whether or not a client has the events stream open.
    pub fn follow_windows(self: &Rc<Self>) {
        let this = self.clone();
        self.schedule(async move {
            // niri drops a subscriber that falls behind: subscribe again.
            while let Some(rx) = call(&this.run, |host| host.subscribe_events())
                .await
                .flatten()
            {
                while let Ok(event) = rx.recv().await {
                    match event {
                        niri_ipc::Event::WindowOpenedOrChanged { window } => {
                            let slot = window.pid.and_then(|pid| this.slot_of_pid(pid as u32));
                            if let Some(BrowserSlot(slot)) = slot {
                                this.hub.browser_changed(slot);
                            }
                        }
                        niri_ipc::Event::WindowClosed { .. }
                        | niri_ipc::Event::WindowsChanged { .. } => this.all_changed(),
                        _ => (),
                    }
                }
                this.all_changed();
            }
        });
    }

    fn all_changed(&self) {
        for slot in 0..self.slots.borrow().len() as u8 {
            self.hub.browser_changed(slot);
        }
    }

    pub fn pid(&self, slot: u8) -> Option<u32> {
        self.with_run(slot, |run| run.pid)
    }

    pub fn slot_of_pid(&self, pid: u32) -> Option<BrowserSlot> {
        let slots = self.slots.borrow();
        let slot = slots
            .iter()
            .position(|s| s.run.as_ref().is_some_and(|run| run.pid == pid))?;
        Some(BrowserSlot(slot as u8))
    }

    /// The slot's state, with its window as the caller found it by pid.
    pub fn state(&self, slot: u8, window: Option<&WindowOwned>) -> BrowserStateOwned {
        let (phase, url) = self
            .with_run(slot, |run| (run.phase, run.url.clone()))
            .unwrap_or((BrowserPhase::Stopped, None));
        BrowserStateOwned {
            phase,
            url,
            title: window.and_then(|w| w.title.clone()),
            window: window.map(|w| w.id),
            player: None,
        }
    }

    pub async fn start(self: &Rc<Self>, slot: u8, kiosk: bool) -> Res {
        let (child, stderr) = {
            let mut slots = self.slots.borrow_mut();
            let Some(s) = slots
                .get_mut(usize::from(slot))
                .filter(|_| slot < self.wanted.get())
            else {
                return Err(BrowserErrorOwned::NotRunning);
            };
            if s.run.is_some() {
                return Err(BrowserErrorOwned::AlreadyRunning);
            }
            let (child, stderr) = self.spawn(slot, kiosk)?;
            s.run = Some(Running {
                phase: BrowserPhase::Starting,
                pid: child.id(),
                bidi: None,
                context: None,
                url: Some("about:blank".to_owned()),
                url_seq: 0,
                window: false,
                stopping: false,
                lost: false,
                exited: Vec::new(),
            });
            (child, stderr)
        };
        let pid = child.id();
        info!("ww: browser slot {slot} started Firefox, pid {pid}");

        // The supervisor owns the process from here: it reaps it whatever happens next.
        let (ready_tx, ready_rx) = async_channel::bounded(1);
        let this = self.clone();
        self.schedule(async move { this.supervise(slot, child, stderr, ready_tx).await });

        // The supervisor hands over the reader once BiDi listens, or ends the slot.
        let Ok(listening) = ready_rx.recv().await else {
            return Err(spawn_error("Firefox exited while starting"));
        };
        let res = self.open_session(slot, listening).await;
        if let Err(err) = &res {
            warn!("ww: browser slot {slot}: {err:?}");
            self.kill(slot, libc::SIGKILL);
        }
        res
    }

    fn spawn(&self, slot: u8, kiosk: bool) -> Result<(Child, File), BrowserErrorOwned> {
        let profile = self.settings.profiles.join(format!("browser-{slot}"));
        std::fs::create_dir_all(&profile).map_err(spawn_error)?;
        let port = self.settings.port_base + u16::from(slot);

        let (program, args) = self
            .settings
            .command
            .split_first()
            .ok_or_else(|| spawn_error("no browser command"))?;
        let mut cmd = crate::utils::spawning::child_command(program);
        cmd.args(args);
        if kiosk {
            cmd.arg("--kiosk");
        }
        cmd.arg("--new-instance")
            .arg("--profile")
            .arg(&profile)
            .arg("--remote-debugging-port")
            .arg(port.to_string())
            .arg("about:blank")
            .stderr(Stdio::piped());
        unsafe {
            cmd.pre_exec(|| {
                crate::utils::spawning::restore_nofile_rlimit();
                // A slot doesn't outlive niri, even when niri dies without cleaning up.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            })
        };

        let mut child = cmd.spawn().map_err(spawn_error)?;
        let stderr = File::from(OwnedFd::from(child.stderr.take().unwrap()));
        Ok((child, stderr))
    }

    /// The session on the slot's port, its tab and the events it follows.
    async fn open_session(&self, slot: u8, listening: Listening) -> Res {
        let Listening { addr, bidi } = listening;
        let session = bidi
            .command("session.new", json!({ "capabilities": {} }))
            .await
            .map_err(bidi_error)?;
        debug!(
            "ww: browser slot {slot} at {addr}: session {}",
            session["sessionId"]
        );

        let tree = bidi
            .command("browsingContext.getTree", json!({ "maxDepth": 0 }))
            .await
            .map_err(bidi_error)?;
        let context = tree["contexts"][0]["context"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| BrowserErrorOwned::Bidi("no browsing context".to_owned()))?;
        bidi.command("session.subscribe", json!({ "events": SUBSCRIBED }))
            .await
            .map_err(bidi_error)?;

        self.with_run(slot, |run| run.context = Some(context))
            .ok_or_else(|| spawn_error("Firefox exited while starting"))?;
        self.try_running(slot);
        Ok(())
    }

    pub async fn open(&self, slot: u8, url: &str) -> Res {
        let scheme = url
            .split_once(':')
            .map(|(scheme, _)| scheme.to_ascii_lowercase());
        if !matches!(scheme.as_deref(), Some("http" | "https")) {
            return Err(BrowserErrorOwned::UrlNotHttp);
        }
        let (bidi, context, seq) = self
            .with_run(slot, |run| {
                (!run.stopping)
                    .then(|| Some((run.bidi.clone()?, run.context.clone()?, run.url_seq)))
                    .flatten()
            })
            .flatten()
            .ok_or(BrowserErrorOwned::NotRunning)?;
        // Answers once the navigation has started: a slow page never holds up the call.
        let params = json!({ "context": context, "url": url, "wait": "none" });
        bidi.command("browsingContext.navigate", params)
            .await
            .map_err(bidi_error)?;
        // The browser's own events may have come with the answer (navigationStarted, a redirect's
        // load): those are newer than the URL asked for.
        let set = self.with_run(slot, |run| {
            let set = run.url_seq == seq;
            if set {
                run.url = Some(url.to_owned());
            }
            set
        });
        if set == Some(true) {
            self.hub.browser_changed(slot);
        }
        Ok(())
    }

    pub async fn stop(&self, slot: u8) -> Res {
        let (bidi, exited) = self
            .with_run(slot, |run| {
                let (tx, rx) = async_channel::bounded(1);
                run.exited.push(tx);
                if run.stopping {
                    return (None, rx);
                }
                run.stopping = true;
                run.phase = BrowserPhase::Stopping;
                (run.bidi.clone(), rx)
            })
            .ok_or(BrowserErrorOwned::NotRunning)?;
        info!("ww: browser slot {slot}: stopping");

        // Firefox answers browser.close and exits; the answer may not make it out first.
        if let Some(bidi) = bidi {
            let _ = timeout(
                &self.io,
                self.settings.stop_grace,
                bidi.command("browser.close", json!({})),
            )
            .await;
        }
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            if timeout(&self.io, self.settings.stop_grace, exited.recv())
                .await
                .is_some()
            {
                return Ok(());
            }
            warn!("ww: browser slot {slot} still runs, sending signal {signal}");
            self.kill(slot, signal);
        }
        let _ = exited.recv().await;
        Ok(())
    }

    fn kill(&self, slot: u8, signal: libc::c_int) {
        // The supervisor reaps the process only after its exit is handled: the pid is still ours.
        if let Some(pid) = self.pid(slot) {
            unsafe { libc::kill(pid as libc::pid_t, signal) };
        }
    }

    /// Owns the slot's process: hands BiDi to `start`, then follows it until the process exits.
    async fn supervise(
        self: Rc<Self>,
        slot: u8,
        child: Child,
        stderr: File,
        ready: Sender<Listening>,
    ) {
        let pid = child.id();
        let mut child = Process(Some(child));
        let mut exit = match pidfd(pid).and_then(|fd| (self.io.file)(fd)) {
            Ok(fd) => Some(fd),
            Err(err) => {
                // Without a pidfd the exit shows as stderr closing.
                warn!("ww: browser slot {slot}: no pidfd, following stderr: {err:?}");
                None
            }
        };
        let mut stderr = match (self.io.file)(stderr) {
            Ok(stderr) => BufReader::new(stderr),
            Err(err) => {
                warn!("ww: browser slot {slot}: error reading stderr: {err:?}");
                drop(child);
                self.exited(slot, child_reason(None));
                return;
            }
        };

        // Until BiDi listens: stderr lines, the timeout, the exit.
        let mut reader = match timeout(&self.io, LISTEN_TIMEOUT, wait_listening(&mut stderr)).await
        {
            Some(Some(addr)) => match self.connect(addr).await {
                Ok((bidi, reader)) => {
                    self.with_run(slot, |run| run.bidi = Some(bidi.clone()));
                    let _ = ready.send(Listening { addr, bidi }).await;
                    Some(reader)
                }
                Err(err) => {
                    warn!("ww: browser slot {slot}: BiDi at {addr}: {err}");
                    None
                }
            },
            Some(None) => {
                warn!("ww: browser slot {slot}: Firefox ended before BiDi listened");
                None
            }
            None => {
                warn!("ww: browser slot {slot}: BiDi not listening after {LISTEN_TIMEOUT:?}");
                None
            }
        };
        drop(ready);
        if reader.is_none() {
            self.kill(slot, libc::SIGKILL);
        }

        // The window, from niri's events: Running once it maps. Without niri's events there is
        // no telling, so it counts as there.
        let mut windows = None;
        if reader.is_some() {
            windows = call(&self.run, |host| host.subscribe_events())
                .await
                .flatten();
            if windows.is_none() || self.window_mapped(pid).await {
                windows = None;
                self.window_mapped_now(slot);
            }
        }

        let mut buf = vec![0u8; 4096];
        let mut stderr_open = true;
        loop {
            let mut steps: Vec<Pin<Box<dyn Future<Output = Step> + '_>>> = Vec::new();
            if let Some(exit) = exit.as_mut() {
                steps.push(Box::pin(async move {
                    let _ = exit.readable().await;
                    Step::Exited
                }));
            }
            if let Some(reader) = reader.as_mut() {
                steps.push(Box::pin(async move {
                    match reader.next_event().await {
                        Some(event) => Step::Bidi(event),
                        None => Step::BidiClosed,
                    }
                }));
            }
            if stderr_open {
                let buf = &mut buf;
                let stderr = &mut stderr;
                steps.push(Box::pin(async move {
                    match stderr.read(buf).await {
                        Ok(0) | Err(_) => Step::StderrClosed,
                        Ok(_) => Step::Stderr,
                    }
                }));
            }
            if let Some(windows) = windows.as_ref() {
                steps.push(Box::pin(async move {
                    match windows.recv().await {
                        Ok(niri_ipc::Event::WindowOpenedOrChanged { window }) => {
                            Step::Window(window.pid == Some(pid as i32))
                        }
                        Ok(_) => Step::Window(false),
                        Err(_) => Step::Window(true),
                    }
                }));
            }
            let (step, _, _) = select_all(steps).await;

            match step {
                Step::Exited => break,
                Step::Bidi(event) => self.bidi_event(slot, event),
                Step::BidiClosed => {
                    reader = None;
                    let stopping = self.with_run(slot, |run| {
                        run.lost = !run.stopping;
                        run.stopping
                    });
                    if stopping == Some(false) {
                        // Never a Firefox the API can't drive.
                        warn!("ww: browser slot {slot}: BiDi connection lost, killing Firefox");
                        self.kill(slot, libc::SIGKILL);
                    }
                }
                Step::Stderr => (),
                Step::StderrClosed => {
                    stderr_open = false;
                    if exit.is_none() {
                        break;
                    }
                }
                Step::Window(found) => {
                    if found {
                        windows = None;
                        self.window_mapped_now(slot);
                    }
                }
            }
        }

        let status = child.reap();
        let lost = self.with_run(slot, |run| run.lost).unwrap_or(false);
        let reason = if lost {
            StopReason::Lost
        } else {
            child_reason(status)
        };
        self.exited(slot, reason);
    }

    async fn connect(&self, addr: SocketAddr) -> Result<(Rc<Bidi>, bidi::Reader), BidiError> {
        let io = |err: io::Error| BidiError::Io(err.to_string());
        // A port on this machine answers at once.
        let stream = TcpStream::connect(addr).map_err(io)?;
        stream.set_nodelay(true).map_err(io)?;
        let stream = (self.io.tcp)(stream).map_err(io)?;
        let (bidi, reader) = bidi::connect(stream, addr).await?;
        Ok((Rc::new(bidi), reader))
    }

    async fn window_mapped(&self, pid: u32) -> bool {
        call(&self.run, move |host| {
            host.windows().iter().any(|w| w.pid == Some(pid))
        })
        .await
        .unwrap_or(false)
    }

    fn window_mapped_now(&self, slot: u8) {
        self.with_run(slot, |run| run.window = true);
        self.hub.browser_changed(slot);
        self.try_running(slot);
    }

    /// Running once the session is up (the tab known) and the window has mapped, in either order.
    fn try_running(&self, slot: u8) {
        let started = self.with_run(slot, |run| {
            let starting =
                run.phase == BrowserPhase::Starting && run.context.is_some() && run.window;
            if starting {
                run.phase = BrowserPhase::Running;
            }
            starting
        });
        if started == Some(true) {
            info!("ww: browser slot {slot} is running");
            self.emit(EventOwned::BrowserStarted(BrowserSlot(slot)));
        }
    }

    /// Runs inside the supervisor, which polls the reader: no commands here, they'd never get
    /// their answer.
    fn bidi_event(self: &Rc<Self>, slot: u8, event: bidi::Event) {
        let context = event.params["context"].as_str().map(str::to_owned);
        let ours = self.with_run(slot, |run| run.context.clone()).flatten();
        match event.method.as_str() {
            "browsingContext.navigationStarted" | "browsingContext.load"
                if context.is_some() && context == ours =>
            {
                let url = event.params["url"].as_str().map(str::to_owned);
                self.with_run(slot, |run| {
                    run.url = url;
                    run.url_seq += 1;
                });
                self.hub.browser_changed(slot);
            }
            "browsingContext.contextDestroyed" if context.is_some() && context == ours => {
                let this = self.clone();
                self.schedule(async move { this.replace_tab(slot).await });
            }
            _ => (),
        }
    }

    /// The slot's tab is gone: the oldest other top-level tab takes its place, or a new one.
    async fn replace_tab(&self, slot: u8) {
        let Some(bidi) = self.with_run(slot, |run| run.bidi.clone()).flatten() else {
            return;
        };
        let tree = bidi
            .command("browsingContext.getTree", json!({ "maxDepth": 0 }))
            .await;
        let found = tree
            .ok()
            .and_then(|tree| tree["contexts"][0]["context"].as_str().map(str::to_owned));
        let context = match found {
            Some(context) => Some(context),
            None => bidi
                .command("browsingContext.create", json!({ "type": "tab" }))
                .await
                .ok()
                .and_then(|res| res["context"].as_str().map(str::to_owned)),
        };
        debug!("ww: browser slot {slot}: its tab is now {context:?}");
        self.with_run(slot, |run| run.context = context);
    }

    fn exited(&self, slot: u8, reason: StopReason) {
        let run = {
            let mut slots = self.slots.borrow_mut();
            slots.get_mut(usize::from(slot)).and_then(|s| s.run.take())
        };
        let Some(run) = run else { return };
        let reason = if run.stopping {
            StopReason::Stopped
        } else {
            reason
        };
        info!("ww: browser slot {slot} stopped: {reason:?}");
        for tx in run.exited {
            let _ = tx.try_send(());
        }
        self.emit(EventOwned::BrowserStopped {
            slot: BrowserSlot(slot),
            reason,
        });
    }
}

/// The slot's process: killed and reaped if its supervisor goes away first (niri exits).
struct Process(Option<Child>);

impl Process {
    fn reap(&mut self) -> Option<std::process::ExitStatus> {
        self.0.take()?.wait().ok()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Where BiDi listens, and the session's commands for `start` to finish with.
struct Listening {
    addr: SocketAddr,
    bidi: Rc<Bidi>,
}

enum Step {
    Exited,
    Bidi(bidi::Event),
    BidiClosed,
    Stderr,
    StderrClosed,
    /// A window event: true when it is the slot's window, or the events ended.
    Window(bool),
}

/// Reads stderr up to Firefox's BiDi line; `None` when stderr ends first.
async fn wait_listening(stderr: &mut BufReader<Async<'static, File>>) -> Option<SocketAddr> {
    let mut line = String::new();
    loop {
        line.clear();
        if stderr.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        if let Some(rest) = line.trim().strip_prefix(LISTENING) {
            match rest.trim_end_matches('/').parse() {
                Ok(addr) => return Some(addr),
                Err(_) => warn!("ww: BiDi line with an address of no known form: {line:?}"),
            }
        }
    }
}

fn pidfd(pid: u32) -> io::Result<File> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(File::from(unsafe {
        OwnedFd::from_raw_fd(fd as libc::c_int)
    }))
}

fn child_reason(status: Option<std::process::ExitStatus>) -> StopReason {
    match status {
        Some(status) => match (status.code(), status.signal()) {
            (Some(code), _) => StopReason::Exited(code),
            (None, Some(signal)) => StopReason::Crashed(signal),
            (None, None) => StopReason::Lost,
        },
        None => StopReason::Lost,
    }
}
