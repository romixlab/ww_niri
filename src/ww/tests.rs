//! The server over a real WebSocket: a calloop loop on a thread serves one connection against a
//! fake host, a generated client calls it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::TcpListener;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use wire_weaver_client::{CallError, ClientConfig, Commander, WwClient};
use ww_niri_api::{
    EventOwned, LaunchErrorOwned, MediaErrorOwned, OutputOwned, Transition, WindowError, WindowId,
    WindowOwned, WorkspaceId,
};

use super::audio::Target;
use super::browsers::{LoopIo, Settings};
use super::connection::serve;
use super::host::WwHost;
use super::Services;

struct NiriClient {
    cmd: Commander,
}

impl WwClient for NiriClient {
    fn default_config() -> ClientConfig {
        ClientConfig::new()
    }

    fn try_from_cmd(cmd: Commander) -> Result<Self, wire_weaver_client::Error> {
        Ok(Self { cmd })
    }
}

mod api_client {
    wire_weaver::ww_codegen!(ww_niri_api :: WwNiri for super::NiriClient, client = "std_client");
}

/// What a test sees of the fake host: its calls, niri's events it sends and the windows.
#[derive(Clone)]
struct Fake {
    calls: Arc<Mutex<Vec<String>>>,
    events: Sender<niri_ipc::Event>,
    windows: Arc<Mutex<Vec<WindowOwned>>>,
}

/// niri's events as the fake host has them: every subscriber gets all of them, from the first
/// on, so a test never races the subscription.
#[derive(Default)]
struct Hub {
    history: Vec<niri_ipc::Event>,
    subscribers: Vec<Sender<niri_ipc::Event>>,
}

struct FakeHost {
    fake: Fake,
    hub: Rc<RefCell<Hub>>,
    services: Option<Rc<Services>>,
    stop: bool,
}

fn window(id: u64, app_id: &str) -> WindowOwned {
    WindowOwned {
        id: WindowId(id),
        app_id: Some(app_id.to_owned()),
        title: None,
        pid: None,
        workspace: Some(WorkspaceId(1)),
        output: Some("HDMI-A-1".to_owned()),
        focused: false,
        fullscreen: false,
        browser: None,
    }
}

/// niri's IPC view of a firefox window.
fn ipc_window(id: u64, pid: Option<u32>) -> niri_ipc::Window {
    niri_ipc::Window {
        id,
        title: None,
        app_id: Some("firefox".to_owned()),
        pid: pid.map(|pid| pid as i32),
        workspace_id: Some(1),
        is_focused: false,
        is_floating: false,
        is_urgent: false,
        layout: niri_ipc::WindowLayout {
            pos_in_scrolling_layout: None,
            tile_size: (0., 0.),
            window_size: (0, 0),
            tile_pos_in_workspace_view: None,
            window_offset_in_tile: (0., 0.),
        },
        focus_timestamp: None,
    }
}

impl FakeHost {
    fn op(&mut self, call: String, id: u64) -> bool {
        self.fake.calls.lock().unwrap().push(call);
        self.windows().iter().any(|w| w.id.0 == id)
    }
}

impl WwHost for FakeHost {
    fn windows(&mut self) -> Vec<WindowOwned> {
        self.fake.windows.lock().unwrap().clone()
    }

    fn window(&mut self, id: u64) -> Option<WindowOwned> {
        self.windows().into_iter().find(|w| w.id.0 == id)
    }

    fn outputs(&mut self) -> Vec<OutputOwned> {
        Vec::new()
    }

    fn focus(&mut self, id: u64) -> bool {
        self.op(format!("focus {id}"), id)
    }

    fn show_window(&mut self, id: u64, _: Option<Transition>, fullscreen: Option<bool>) -> bool {
        self.op(format!("show {id} {fullscreen:?}"), id)
    }

    fn close_window(&mut self, id: u64) -> bool {
        self.op(format!("close {id}"), id)
    }

    fn set_fullscreen(&mut self, id: u64, on: bool) -> bool {
        self.op(format!("fullscreen {id} {on}"), id)
    }

    fn services(&mut self) -> Rc<Services> {
        self.services.clone().unwrap()
    }

    fn subscribe_events(&mut self) -> Option<Receiver<niri_ipc::Event>> {
        let (tx, rx) = async_channel::unbounded();
        let mut hub = self.hub.borrow_mut();
        for event in &hub.history {
            tx.try_send(event.clone()).unwrap();
        }
        hub.subscribers.push(tx);
        Some(rx)
    }
}

struct Bench {
    url: String,
    fake: Fake,
    stop: calloop::channel::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

/// Serves one connection against a fake host with `windows`, on a calloop loop of its own.
fn bench(windows: Vec<WindowOwned>) -> Bench {
    bench_with(windows, Settings::default(), None, None)
}

/// The same, with browser slots as `settings` say, players on the D-Bus at `bus` and the audio
/// server at `audio`.
fn bench_with(
    windows: Vec<WindowOwned>,
    settings: Settings,
    bus: Option<String>,
    audio: Option<Target>,
) -> Bench {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/ww", listener.local_addr().unwrap());
    let (events_tx, events_rx) = async_channel::unbounded();
    let fake = Fake {
        calls: Arc::default(),
        events: events_tx,
        windows: Arc::new(Mutex::new(windows)),
    };
    let (stop_tx, stop_rx) = calloop::channel::channel::<()>();

    let host_fake = fake.clone();
    let thread = std::thread::spawn(move || {
        let mut event_loop = calloop::EventLoop::<FakeHost>::try_new().unwrap();
        let handle = event_loop.handle();
        let (executor, scheduler) = calloop::futures::executor().unwrap();
        handle.insert_source(executor, |(), _, _| {}).unwrap();
        handle
            .insert_source(stop_rx, |_, _, host| host.stop = true)
            .unwrap();

        let hub = Rc::new(RefCell::new(Hub::default()));
        let services = Services::new(&handle, &scheduler, settings, 2);
        if let Some(bus) = bus {
            services.players.follow(&scheduler, async move {
                zbus::connection::Builder::address(bus.as_str())?
                    .build()
                    .await
            });
        }
        if let Some(target) = audio {
            services
                .audio
                .follow(&scheduler, LoopIo::new(handle.clone()), target);
        }
        let mut host = FakeHost {
            fake: host_fake,
            hub: hub.clone(),
            services: Some(services),
            stop: false,
        };
        scheduler
            .schedule(async move {
                while let Ok(event) = events_rx.recv().await {
                    let mut hub = hub.borrow_mut();
                    for tx in &hub.subscribers {
                        let _ = tx.try_send(event.clone());
                    }
                    hub.history.push(event);
                }
            })
            .unwrap();

        let (stream, peer) = listener.accept().unwrap();
        stream.set_nodelay(true).unwrap();
        let stream = handle.adapt_io(stream).unwrap();
        scheduler
            .schedule(serve(handle.clone(), stream, peer))
            .unwrap();

        while !host.stop {
            event_loop.dispatch(None, &mut host).unwrap();
        }
    });

    Bench {
        url,
        fake,
        stop: stop_tx,
        thread,
    }
}

impl Bench {
    fn run(self, test: impl AsyncFnOnce(NiriClient, &Fake)) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = NiriClient::config(|c| c.websocket_url(self.url.clone()))
                .connect()
                .await
                .unwrap();
            test(client, &self.fake).await;
        });
        self.stop.send(()).unwrap();
        self.thread.join().unwrap();
    }
}

#[test]
fn info_over_websocket() {
    bench(Vec::new()).run(async |client, _| {
        let info = client.info().call().await.unwrap();
        assert_eq!(info.ww_niri_version, env!("CARGO_PKG_VERSION"));
        assert!(!info.niri_version.is_empty());
    });
}

#[test]
fn windows_list_and_actions() {
    let windows = vec![window(1, "firefox"), window(2, "org.telegram.desktop")];
    bench(windows.clone()).run(async |client, fake| {
        assert_eq!(client.windows().list().call().await.unwrap(), windows);

        client.windows().focus(WindowId(2)).call().await.unwrap();
        client
            .windows()
            .show(WindowId(1), None, Some(true))
            .call()
            .await
            .unwrap();
        client
            .windows()
            .set_fullscreen(WindowId(1), false)
            .call()
            .await
            .unwrap();
        client.windows().close(WindowId(2)).call().await.unwrap();
        assert!(matches!(
            client.windows().focus(WindowId(99)).call().await,
            Err(CallError::Device(WindowError::NoSuchWindow))
        ));

        assert_eq!(
            *fake.calls.lock().unwrap(),
            [
                "focus 2",
                "show 1 Some(true)",
                "fullscreen 1 false",
                "close 2",
                "focus 99"
            ]
        );
    });
}

#[test]
fn events_follow_niri() {
    bench(vec![window(1, "firefox")]).run(async |client, fake| {
        let mut events = client.events().call().await.unwrap();

        let mut ipc_window = ipc_window(1, None);
        ipc_window.is_focused = true;
        fake.events
            .send(niri_ipc::Event::WindowOpenedOrChanged { window: ipc_window })
            .await
            .unwrap();
        fake.events
            .send(niri_ipc::Event::WindowFocusChanged { id: Some(1) })
            .await
            .unwrap();
        fake.events
            .send(niri_ipc::Event::WindowClosed { id: 1 })
            .await
            .unwrap();

        let mut next = async || {
            tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("an event")
                .unwrap()
        };
        assert_eq!(
            next().await,
            EventOwned::WindowOpenedOrChanged(window(1, "firefox"))
        );
        assert_eq!(
            next().await,
            EventOwned::WindowFocusChanged(Some(WindowId(1)))
        );
        assert_eq!(next().await, EventOwned::WindowClosed(WindowId(1)));
    });
}

#[test]
fn apps_launch() {
    bench(Vec::new()).run(async |client, _| {
        // The launched shell writes its own pid: the one `launch` hands back.
        let file = std::env::temp_dir().join(format!("ww-launch-{}", std::process::id()));
        let script = format!("echo $$ > {}.tmp && mv {0}.tmp {0}", file.display());
        let argv = ["sh", "-c", script.as_str()].map(String::from).to_vec();
        let launched = client.apps().launch(argv).call().await.unwrap();

        let written = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&file) {
                    break text;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the launched shell writes its pid");
        let _ = std::fs::remove_file(&file);
        assert_eq!(written.trim(), launched.pid.to_string());

        assert!(matches!(
            client.apps().launch(Vec::new()).call().await,
            Err(CallError::Device(LaunchErrorOwned::EmptyArgv))
        ));
        let missing = vec!["/nonexistent/ww-launch".to_owned()];
        match client.apps().launch(missing).call().await {
            Err(CallError::Device(LaunchErrorOwned::Spawn(err))) => {
                assert!(err.contains("No such file"), "{err}")
            }
            other => panic!("{other:?}"),
        }
    });
}

/// A browser's BiDi end on a thread: answers what a slot sends, closes after `browser.close`,
/// and returns the methods it got.
fn fake_bidi() -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<String>>) {
    use async_tungstenite::tungstenite::{self, Message};
    use serde_json::{json, Value};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        let mut methods = Vec::new();
        loop {
            let Ok(msg) = ws.read() else { break };
            let Ok(text) = msg.to_text() else { continue };
            let cmd: Value = serde_json::from_str(text).unwrap();
            let method = cmd["method"].as_str().unwrap().to_owned();
            let result = match method.as_str() {
                "session.new" => json!({"sessionId": "s1", "capabilities": {}}),
                "browsingContext.getTree" => json!({"contexts": [{"context": "tab1"}]}),
                "browsingContext.navigate" => {
                    json!({"navigation": "n1", "url": cmd["params"]["url"]})
                }
                _ => json!({}),
            };
            let answer = json!({"type": "success", "id": cmd["id"], "result": result});
            ws.send(Message::text(answer.to_string())).unwrap();
            if method == "browsingContext.navigate" {
                // A link clicked in the page.
                let event = json!({"type": "event", "method": "browsingContext.load",
                    "params": {"context": "tab1", "url": "https://example.org/next"}});
                ws.send(Message::text(event.to_string())).unwrap();
            }
            methods.push(method.clone());
            if method == "browser.close" {
                let _ = ws.close(None);
            }
        }
        methods
    });
    (addr, thread)
}

/// A scratch folder of this test process, gone when dropped.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ww-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Waits for the file the fake Firefox writes its pid to.
async fn read_pid(path: &std::path::Path) -> u32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the fake Firefox writes its pid")
}

#[test]
fn browser_slot_lifecycle() {
    use ww_niri_api::{
        BrowserErrorOwned, BrowserPhase, BrowserSlot, BrowserStateOwned, StopReason,
    };

    let (bidi_addr, bidi) = fake_bidi();
    let scratch = Scratch::new("browser-slot");
    let pid_file = scratch.0.join("pid");
    // Firefox's line on stderr, then a process that runs until killed.
    let script = format!(
        "echo $$ > {} && echo 'WebDriver BiDi listening on ws://{bidi_addr}' >&2 && exec sleep 30",
        pid_file.display()
    );
    let settings = Settings {
        command: vec!["sh".into(), "-c".into(), script, "fake-firefox".into()],
        profiles: scratch.0.clone(),
        port_base: 9300,
        stop_grace: Duration::from_millis(100),
    };

    bench_with(Vec::new(), settings, None, None).run(async |client, fake| {
        let mut events = client.events().call().await.unwrap();
        let mut next = async || {
            tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("an event")
                .unwrap()
        };
        let slot = client.browsers(0);
        // Observed on change: each phase the slot goes through reaches the client.
        let mut phases = slot
            .observe_state(wire_weaver_client::observe::on_change(Duration::ZERO))
            .subscribe()
            .await
            .unwrap();
        let mut until = async |what: &str, ok: &dyn Fn(&ww_niri_api::BrowserStateOwned) -> bool| {
            let mut seen = Vec::new();
            let got = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let state = phases.recv().await.unwrap().value;
                    let done = ok(&state);
                    seen.push(state);
                    if done {
                        break;
                    }
                }
            })
            .await;
            if got.is_err() {
                panic!("observers get {what}; they got {seen:#?}");
            }
        };
        until("Stopped", &|s| s.phase == BrowserPhase::Stopped).await;

        let slot = client.browsers(0);

        assert!(matches!(
            slot.open("https://example.org/".to_owned()).call().await,
            Err(CallError::Device(BrowserErrorOwned::NotRunning))
        ));
        slot.start(true).call().await.unwrap();
        assert!(matches!(
            slot.start(true).call().await,
            Err(CallError::Device(BrowserErrorOwned::AlreadyRunning))
        ));
        assert!(scratch.0.join("browser-0").is_dir());

        // Driven, but not Running until its window maps.
        let state = slot.read_state().read().await.unwrap();
        assert_eq!(state.phase, BrowserPhase::Starting);
        assert!(matches!(
            slot.show(None, None).call().await,
            Err(CallError::Device(BrowserErrorOwned::NoWindow))
        ));

        let pid = read_pid(&pid_file).await;
        let mut firefox = window(7, "firefox");
        firefox.pid = Some(pid);
        firefox.title = Some("Mozilla Firefox".to_owned());
        fake.windows.lock().unwrap().push(firefox);
        fake.events
            .send(niri_ipc::Event::WindowOpenedOrChanged {
                window: ipc_window(7, Some(pid)),
            })
            .await
            .unwrap();
        // The niri event first, then the slot's.
        assert!(matches!(next().await, EventOwned::WindowOpenedOrChanged(w)
            if w.browser == Some(BrowserSlot(0))));
        assert_eq!(next().await, EventOwned::BrowserStarted(BrowserSlot(0)));
        until("Running", &|s| s.phase == BrowserPhase::Running).await;

        assert!(matches!(
            slot.open("file:///etc/passwd".to_owned()).call().await,
            Err(CallError::Device(BrowserErrorOwned::UrlNotHttp))
        ));
        slot.open("https://example.org/".to_owned())
            .call()
            .await
            .unwrap();
        slot.show(None, Some(true)).call().await.unwrap();
        let listed = client.windows().list().call().await.unwrap();
        assert_eq!(listed[0].browser, Some(BrowserSlot(0)));

        // The page's load event moves the URL on, and observers hear it.
        until("the next URL", &|s| {
            s.url.as_deref() == Some("https://example.org/next")
        })
        .await;
        let state = slot.read_state().read().await.unwrap();
        assert_eq!(
            state,
            BrowserStateOwned {
                phase: BrowserPhase::Running,
                url: Some("https://example.org/next".to_owned()),
                title: Some("Mozilla Firefox".to_owned()),
                window: Some(WindowId(7)),
                player: None,
            }
        );

        // A new title of its window reaches observers too, with no events stream needed.
        fake.windows.lock().unwrap()[0].title = Some("Next — Mozilla Firefox".to_owned());
        fake.events
            .send(niri_ipc::Event::WindowOpenedOrChanged {
                window: ipc_window(7, Some(pid)),
            })
            .await
            .unwrap();
        until("the new title", &|s| {
            s.title.as_deref() == Some("Next — Mozilla Firefox")
        })
        .await;
        // The events stream, open here, has it as well.
        assert!(matches!(next().await, EventOwned::WindowOpenedOrChanged(w)
            if w.title.as_deref() == Some("Next — Mozilla Firefox")));

        // The fake ignores browser.close: SIGTERM ends it.
        slot.stop().call().await.unwrap();
        assert_eq!(
            next().await,
            EventOwned::BrowserStopped {
                slot: BrowserSlot(0),
                reason: StopReason::Stopped
            }
        );
        until("Stopped", &|s| s.phase == BrowserPhase::Stopped).await;
        let state = slot.read_state().read().await.unwrap();
        assert_eq!(state.phase, BrowserPhase::Stopped);
        assert!(matches!(
            slot.stop().call().await,
            Err(CallError::Device(BrowserErrorOwned::NotRunning))
        ));
        assert_eq!(*fake.calls.lock().unwrap(), ["show 7 Some(true)"]);
    });

    assert_eq!(
        bidi.join().unwrap(),
        [
            "session.new",
            "browsingContext.getTree",
            "session.subscribe",
            "browsingContext.navigate",
            "browser.close"
        ]
    );
}

/// Serves one small page to every request, on a thread, until the test ends.
fn page_server() -> std::net::SocketAddr {
    use std::io::{Read as _, Write as _};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf);
            let body = "<html><head><title>ww</title></head><body>ww</body></html>";
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    addr
}

/// A real headless Firefox where one is installed: start, open, stop.
#[test]
fn browser_slot_real_firefox() {
    use ww_niri_api::{BrowserPhase, BrowserSlot, StopReason};

    let Ok(firefox) = which("firefox") else {
        eprintln!("browser_slot_real_firefox: skipped, no firefox on PATH");
        return;
    };
    let scratch = Scratch::new("browser-firefox");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let settings = Settings {
        command: vec![firefox, "--headless".to_owned()],
        profiles: scratch.0.clone(),
        port_base: port,
        stop_grace: Duration::from_secs(5),
    };
    let page = format!("http://{}/page", page_server());

    bench_with(Vec::new(), settings, None, None).run(async |client, _| {
        let mut events = client.events().call().await.unwrap();
        let slot = client.browsers(0);
        slot.start(false)
            .with_timeout(Duration::from_secs(30))
            .call()
            .await
            .unwrap();
        slot.open(page.clone())
            .with_timeout(Duration::from_secs(10))
            .call()
            .await
            .unwrap();

        let state = slot.read_state().read().await.unwrap();
        // Headless: no window, so never Running.
        assert_eq!(state.phase, BrowserPhase::Starting);
        assert_eq!(state.url.as_deref(), Some(page.as_str()));

        slot.stop()
            .with_timeout(Duration::from_secs(20))
            .call()
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an event")
            .unwrap();
        assert_eq!(
            event,
            EventOwned::BrowserStopped {
                slot: BrowserSlot(0),
                reason: StopReason::Stopped
            }
        );
    });
}

fn which(program: &str) -> Result<String, ()> {
    let path = std::env::var_os("PATH").ok_or(())?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or(())
}

/// A D-Bus daemon of the test's own, so no test sees the session's players.
struct PrivateBus {
    daemon: std::process::Child,
    address: String,
}

impl PrivateBus {
    fn start() -> Option<Self> {
        use std::io::BufRead as _;

        let mut daemon = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        let stdout = daemon.stdout.take().unwrap();
        std::io::BufReader::new(stdout).read_line(&mut line).ok()?;
        Some(Self {
            daemon,
            address: line.trim().to_owned(),
        })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// An MPRIS player that records what it is asked.
struct FakePlayer {
    calls: Arc<Mutex<Vec<String>>>,
    status: Mutex<&'static str>,
}

struct FakeRoot;

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl FakeRoot {
    #[zbus(property)]
    fn identity(&self) -> String {
        "Fake Player".to_owned()
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl FakePlayer {
    fn play(&self) {
        self.calls.lock().unwrap().push("Play".to_owned());
    }
    async fn pause(&self, #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>) {
        self.calls.lock().unwrap().push("Pause".to_owned());
        *self.status.lock().unwrap() = "Paused";
        self.playback_status_changed(&emitter).await.unwrap();
    }
    fn next(&self) {
        self.calls.lock().unwrap().push("Next".to_owned());
    }
    fn seek(&self, by: i64) {
        self.calls.lock().unwrap().push(format!("Seek {by}"));
    }
    fn set_position(&self, track: zbus::zvariant::ObjectPath<'_>, at: i64) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("SetPosition {track} {at}"));
    }

    #[zbus(property)]
    fn playback_status(&self) -> String {
        (*self.status.lock().unwrap()).to_owned()
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, zbus::zvariant::OwnedValue> {
        use zbus::zvariant::{ObjectPath, Value};

        let value = |v: Value<'_>| v.try_into().unwrap();
        HashMap::from([
            ("xesam:title".to_owned(), value(Value::from("A Song"))),
            (
                "xesam:artist".to_owned(),
                value(Value::from(vec!["Someone", "Another"])),
            ),
            (
                "mpris:length".to_owned(),
                value(Value::from(180_000_000i64)),
            ),
            (
                "mpris:trackid".to_owned(),
                value(Value::from(ObjectPath::try_from("/track/1").unwrap())),
            ),
        ])
    }
    #[zbus(property)]
    fn position(&self) -> i64 {
        42_000_000
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        false
    }
}

#[test]
fn media_players() {
    use exact_units::Fixed;
    use ww_niri_api::{PlaybackStatus, PlayerId, PlayerInfoOwned, PlayerSourceOwned};

    let Some(bus) = PrivateBus::start() else {
        eprintln!("media_players: skipped, no dbus-daemon");
        return;
    };
    // The fake player runs in this process: its window is the one with our pid.
    let mut mpv = window(3, "mpv");
    mpv.pid = Some(std::process::id());

    let address = bus.address.clone();
    bench_with(vec![mpv], Settings::default(), Some(address.clone()), None).run(
        async |client, _| {
            let mut events = client.events().call().await.unwrap();
            let mut next = async || {
                tokio::time::timeout(Duration::from_secs(5), events.recv())
                    .await
                    .expect("an event")
                    .unwrap()
            };

            let calls = Arc::default();
            let player = zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .name("org.mpris.MediaPlayer2.fake")
                .unwrap()
                .serve_at("/org/mpris/MediaPlayer2", FakeRoot)
                .unwrap()
                .serve_at(
                    "/org/mpris/MediaPlayer2",
                    FakePlayer {
                        calls: Arc::clone(&calls),
                        status: Mutex::new("Playing"),
                    },
                )
                .unwrap()
                .build()
                .await
                .unwrap();

            let info = PlayerInfoOwned {
                id: PlayerId(0),
                identity: "Fake Player".to_owned(),
                source: PlayerSourceOwned::Window {
                    id: WindowId(3),
                    app_id: Some("mpv".to_owned()),
                },
            };
            assert_eq!(next().await, EventOwned::PlayerAppeared(info.clone()));
            assert_eq!(client.media().list().call().await.unwrap(), [info]);

            let media = client.media();
            let player0 = media.players(0);
            let playback = player0.playback();
            let mut observed = playback
                .observe_state(wire_weaver_client::observe::on_change(Duration::ZERO))
                .subscribe()
                .await
                .unwrap();
            let first = tokio::time::timeout(Duration::from_secs(5), observed.recv())
                .await
                .expect("the observed state comes at once")
                .unwrap();
            assert_eq!(first.value.status, PlaybackStatus::Playing);
            let state = playback.read_state().read().await.unwrap();
            assert_eq!(state.status, PlaybackStatus::Playing);
            assert_eq!(state.title.as_deref(), Some("A Song"));
            assert_eq!(state.artist.as_deref(), Some("Someone, Another"));
            assert_eq!(state.length, Some(Fixed::new(180_000_000)));
            assert_eq!(state.position, Fixed::new(42_000_000));
            assert_eq!(state.rate, Fixed::new(1000));
            assert!(state.can_seek && state.can_pause);

            playback.pause().call().await.unwrap();
            playback.play().call().await.unwrap();
            playback.seek(Fixed::new(-5_000_000)).call().await.unwrap();
            playback
                .set_position(Fixed::new(60_000_000))
                .call()
                .await
                .unwrap();
            assert!(matches!(
                playback.next().call().await,
                Err(CallError::Device(MediaErrorOwned::NotSupported))
            ));
            assert_eq!(
                *calls.lock().unwrap(),
                [
                    "Pause",
                    "Play",
                    "Seek -5000000",
                    "SetPosition /track/1 60000000"
                ]
            );
            // The pause changed its status: the player said so, and observers hear it.
            tokio::time::timeout(Duration::from_secs(5), async {
                while observed.recv().await.unwrap().value.status != PlaybackStatus::Paused {}
            })
            .await
            .expect("observers get Paused");

            // No browser slot runs: its playback has no player.
            let slot = client.browsers(0);
            assert!(matches!(
                slot.playback().pause().call().await,
                Err(CallError::Device(MediaErrorOwned::NoPlayer))
            ));

            drop(player);
            assert_eq!(next().await, EventOwned::PlayerGone(PlayerId(0)));
            // Its observers end with it.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match observed.recv_event().await {
                        Ok(wire_weaver_client::observe::ObserveEvent::Ended) => break,
                        Err(err) => panic!("{err:?} before Ended"),
                        Ok(_) => (),
                    }
                }
            })
            .await
            .expect("the observer ends when the player goes");
            assert!(client.media().list().call().await.unwrap().is_empty());
            assert!(playback.pause().call().await.is_err());
        },
    );
    drop(bus);
}

/// A PulseAudio server with one sink, `fake_sink`, on a thread: it serves one connection with what
/// the audio service asks for and sends a sink event after each change.
struct FakePulse {
    socket: std::path::PathBuf,
    /// The connection, to send events on as if the sink changed elsewhere.
    conn: Arc<Mutex<Option<std::os::unix::net::UnixStream>>>,
    sink: Arc<Mutex<pulseaudio::protocol::SinkInfo>>,
    /// The commands that changed something, as text.
    calls: Arc<Mutex<Vec<String>>>,
}

impl FakePulse {
    fn start(dir: &std::path::Path) -> Self {
        use pulseaudio::protocol::{self, ChannelVolume, Volume};

        let socket = dir.join("native");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let mut sink = protocol::SinkInfo::new_dummy(7);
        sink.name = c"fake_sink".to_owned();
        sink.description = Some(c"Fake HDMI".to_owned());
        let mut volume = ChannelVolume::empty();
        volume.push(Volume::from_u32_clamped(0x10000 * 40 / 100));
        volume.push(Volume::from_u32_clamped(0x10000 * 30 / 100));
        sink.cvolume = volume;

        let fake = Self {
            socket,
            conn: Arc::default(),
            sink: Arc::new(Mutex::new(sink)),
            calls: Arc::default(),
        };
        let (conn, sink, calls) = (fake.conn.clone(), fake.sink.clone(), fake.calls.clone());
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            *conn.lock().unwrap() = Some(stream.try_clone().unwrap());
            let mut reader = std::io::BufReader::new(stream);
            let version = protocol::MAX_VERSION;
            let write = |f: &mut dyn FnMut(&mut std::os::unix::net::UnixStream)| {
                f(conn.lock().unwrap().as_mut().unwrap())
            };
            while let Ok((seq, command)) = protocol::read_command_message(&mut reader, version) {
                match command {
                    protocol::Command::Auth(_) => write(&mut |w| {
                        let reply = protocol::AuthReply {
                            version,
                            ..Default::default()
                        };
                        protocol::write_reply_message(w, seq, &reply, version).unwrap()
                    }),
                    protocol::Command::SetClientName(_) => write(&mut |w| {
                        let reply = protocol::SetClientNameReply { client_id: 1 };
                        protocol::write_reply_message(w, seq, &reply, version).unwrap()
                    }),
                    protocol::Command::Subscribe(_) => {
                        write(&mut |w| protocol::write_ack_message(w, seq).unwrap())
                    }
                    protocol::Command::GetServerInfo => write(&mut |w| {
                        let info = protocol::ServerInfo {
                            default_sink_name: Some(c"fake_sink".to_owned()),
                            ..Default::default()
                        };
                        protocol::write_reply_message(w, seq, &info, version).unwrap()
                    }),
                    protocol::Command::GetSinkInfo(get) => {
                        assert_eq!(get.name.as_deref(), Some(c"fake_sink"));
                        let info = sink.lock().unwrap().clone();
                        write(&mut |w| {
                            protocol::write_reply_message(w, seq, &info, version).unwrap()
                        })
                    }
                    protocol::Command::SetSinkVolume(set) => {
                        calls.lock().unwrap().push(format!(
                            "volume {:?} {:?}",
                            set.device_name,
                            set.volume
                                .channels()
                                .iter()
                                .map(|v| v.as_u32())
                                .collect::<Vec<_>>()
                        ));
                        sink.lock().unwrap().cvolume = set.volume;
                        write(&mut |w| {
                            protocol::write_ack_message(w, seq).unwrap();
                            sink_changed(w);
                        })
                    }
                    protocol::Command::SetSinkMute(set) => {
                        calls
                            .lock()
                            .unwrap()
                            .push(format!("mute {:?} {}", set.device_name, set.mute));
                        sink.lock().unwrap().muted = set.mute;
                        write(&mut |w| {
                            protocol::write_ack_message(w, seq).unwrap();
                            sink_changed(w);
                        })
                    }
                    other => panic!("the fake PulseAudio server got {other:?}"),
                }
            }
        });
        fake
    }

    fn target(&self) -> Target {
        Target {
            socket: self.socket.clone(),
            cookie: vec![0; 256],
        }
    }

    /// Changes the sink as another client would.
    fn mute_elsewhere(&self) {
        self.sink.lock().unwrap().muted = true;
        sink_changed(self.conn.lock().unwrap().as_mut().unwrap());
    }

    /// Ends the connection, as a server that stops.
    fn hang_up(&self) {
        let conn = self.conn.lock().unwrap();
        conn.as_ref()
            .unwrap()
            .shutdown(std::net::Shutdown::Both)
            .unwrap();
    }
}

fn sink_changed(w: &mut std::os::unix::net::UnixStream) {
    use pulseaudio::protocol::{
        self, SubscriptionEvent, SubscriptionEventFacility, SubscriptionEventType,
    };
    let event = protocol::Command::SubscribeEvent(SubscriptionEvent {
        event_facility: SubscriptionEventFacility::Sink,
        event_type: SubscriptionEventType::Changed,
        index: Some(7),
    });
    protocol::write_command_message(w, u32::MAX, &event, protocol::MAX_VERSION).unwrap();
}

#[test]
fn audio_volume_and_mute() {
    use wire_weaver_client::observe::on_change;

    let volume = |percent| ww_niri_api::Volume::try_new(percent).unwrap();

    let dir = Scratch::new("audio");
    let pulse = FakePulse::start(&dir.0);
    bench_with(Vec::new(), Settings::default(), None, Some(pulse.target())).run(
        async |client, _| {
            let audio = client.audio();
            let mut sinks = audio
                .observe_sink(on_change(Duration::ZERO))
                .subscribe()
                .await
                .unwrap();
            let mut volumes = audio
                .observe_volume(on_change(Duration::ZERO))
                .subscribe()
                .await
                .unwrap();
            let mut mutes = audio
                .observe_muted(on_change(Duration::ZERO))
                .subscribe()
                .await
                .unwrap();

            // The task connects on its own; the sink comes once it has read it.
            let sink = tokio::time::timeout(Duration::from_secs(5), sinks.recv())
                .await
                .expect("the audio service reads the sink")
                .unwrap()
                .value;
            assert_eq!(sink.name, "fake_sink");
            assert_eq!(sink.description, "Fake HDMI");
            assert_eq!(audio.read_sink().read().await.unwrap(), sink);
            // The loudest channel.
            assert_eq!(audio.read_volume().read().await.unwrap(), volume(40));
            assert!(!audio.read_muted().read().await.unwrap());

            // Each change comes back from the server as a sink event, the service reads the sink
            // again and observers get the new value (a write may first send the old one).
            async fn until<T: PartialEq + std::fmt::Debug>(
                observer: &mut wire_weaver_client::observe::Observer<
                    impl wire_weaver_client::observe::LevelUpdate,
                    T,
                >,
                value: T,
            ) {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while observer.recv().await.unwrap().value != value {}
                })
                .await
                .unwrap_or_else(|_| panic!("observers get {value:?}"))
            }
            audio.write_volume(volume(125)).write().await.unwrap();
            until(&mut volumes, volume(125)).await;
            audio.write_muted(true).write().await.unwrap();
            until(&mut mutes, true).await;
            audio.write_muted(false).write().await.unwrap();
            until(&mut mutes, false).await;
            assert_eq!(
                *pulse.calls.lock().unwrap(),
                [
                    "volume Some(\"fake_sink\") [81920, 81920]",
                    "mute Some(\"fake_sink\") true",
                    "mute Some(\"fake_sink\") false",
                ]
            );

            // A change made elsewhere reaches observers too.
            pulse.mute_elsewhere();
            until(&mut mutes, true).await;
            assert_eq!(audio.read_volume().read().await.unwrap(), volume(125));

            // Without the server there is nothing to read or set.
            pulse.hang_up();
            tokio::time::timeout(Duration::from_secs(5), async {
                while audio.read_sink().read().await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the service notices the server is gone");
            assert!(audio.read_volume().read().await.is_err());
            assert!(audio.write_volume(volume(50)).write().await.is_err());
        },
    );
}

/// The fake checked against a real server: `WW_REAL_PULSE=1` reads this session's default sink
/// and sets its volume and mute to what they are, so nothing audible changes.
#[test]
fn audio_real_server() {
    if std::env::var_os("WW_REAL_PULSE").is_none() {
        eprintln!(
            "audio_real_server: skipped, set WW_REAL_PULSE=1 to run against this session's server"
        );
        return;
    }
    let target = Target::from_env().expect("a PulseAudio socket");
    bench_with(Vec::new(), Settings::default(), None, Some(target)).run(async |client, _| {
        let audio = client.audio();
        let sink = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(sink) = audio.read_sink().read().await {
                    break sink;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the audio service reads the sink");
        let volume = audio.read_volume().read().await.unwrap();
        let muted = audio.read_muted().read().await.unwrap();
        eprintln!(
            "audio_real_server: {sink:?} at {} %, muted {muted}",
            volume.get()
        );
        audio.write_volume(volume).write().await.unwrap();
        audio.write_muted(muted).write().await.unwrap();
        assert_eq!(audio.read_volume().read().await.unwrap(), volume);
    });
}

/// A running ww_niri, read only: `WW_NIRI_URL=ws://ADDR:7590/ww` lists what it serves.
#[test]
fn live_read_only() {
    let Some(url) = std::env::var_os("WW_NIRI_URL") else {
        eprintln!("live_read_only: skipped, set WW_NIRI_URL=ws://ADDR:7590/ww to run against a running niri");
        return;
    };
    let url = url.into_string().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let client = NiriClient::config(|c| c.websocket_url(url.clone()))
            .connect()
            .await
            .unwrap();
        eprintln!("info: {:?}", client.info().call().await.unwrap());
        for output in client.outputs().list().call().await.unwrap() {
            eprintln!("output: {output:?}");
        }
        for window in client.windows().list().call().await.unwrap() {
            eprintln!("window: {window:?}");
        }
        for player in client.media().list().call().await.unwrap() {
            eprintln!("player: {player:?}");
        }
        let audio = client.audio();
        eprintln!(
            "audio: {:?} at {:?} %, muted {:?}",
            audio.read_sink().read().await.ok(),
            audio.read_volume().read().await.ok().map(|v| v.get()),
            audio.read_muted().read().await.ok()
        );
    });
}

/// A running ww_niri, with actions that leave it as it was: `WW_NIRI_URL=ws://ADDR:7590/ww` and
/// `WW_NIRI_LIVE_ACTIONS=1` set the volume and put it back, then start browser slot 1, open a
/// page in it and stop it again.
#[test]
fn live_actions() {
    let (Some(url), Some(_)) = (
        std::env::var_os("WW_NIRI_URL"),
        std::env::var_os("WW_NIRI_LIVE_ACTIONS"),
    ) else {
        eprintln!("live_actions: skipped, set WW_NIRI_URL and WW_NIRI_LIVE_ACTIONS=1 to run against a running niri");
        return;
    };
    let url = url.into_string().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let client = NiriClient::config(|c| c.websocket_url(url.clone()))
            .connect()
            .await
            .unwrap();

        let audio = client.audio();
        let before = audio.read_volume().read().await.unwrap();
        let other = ww_niri_api::Volume::try_new(if before.get() == 50 { 60 } else { 50 }).unwrap();
        audio.write_volume(other).write().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while audio.read_volume().read().await.unwrap() != other {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the volume follows");
        audio.write_volume(before).write().await.unwrap();
        eprintln!("audio: {} % -> {} % -> back", before.get(), other.get());

        let mut events = client.events().call().await.unwrap();
        let slot = client.browsers(1);
        slot.start(true)
            .with_timeout(Duration::from_secs(60))
            .call()
            .await
            .unwrap();
        slot.open("https://example.org/".to_owned())
            .with_timeout(Duration::from_secs(20))
            .call()
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        eprintln!("browser 1: {:?}", slot.read_state().read().await.unwrap());
        for window in client.windows().list().call().await.unwrap() {
            eprintln!("window: {window:?}");
        }
        slot.stop()
            .with_timeout(Duration::from_secs(20))
            .call()
            .await
            .unwrap();
        while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        {
            eprintln!("event: {event:?}");
        }
    });
}

/// A running ww_niri on a screen: `WW_NIRI_URL=ws://ADDR:7590/ww` and
/// `WW_NIRI_LIVE_SCREEN=VIDEO_URL` show two browser slots in turn with a slow transition, play
/// the video in slot 1 and follow its observed playback, then pause it and stop both slots.
/// Screenshots go to `/tmp/ww-screen-*.png` on the niri host, taken by `WW_NIRI_BIN` (default
/// `niri`) started through `apps.launch`.
#[test]
fn live_screen() {
    use exact_units::Millis;
    use ww_niri_api::{BrowserPhase, Curve, PlaybackStatus};

    let (Some(url), Some(video)) = (
        std::env::var_os("WW_NIRI_URL"),
        std::env::var_os("WW_NIRI_LIVE_SCREEN"),
    ) else {
        eprintln!("live_screen: skipped, set WW_NIRI_URL and WW_NIRI_LIVE_SCREEN=VIDEO_URL to run against a running niri");
        return;
    };
    let url = url.into_string().unwrap();
    let video = video.into_string().unwrap();
    let niri = std::env::var("WW_NIRI_BIN").unwrap_or_else(|_| "niri".to_owned());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let client = NiriClient::config(|c| c.websocket_url(url.clone()))
            .connect()
            .await
            .unwrap();
        let screenshot = async |name: &str| {
            let path = format!("/tmp/ww-screen-{name}.png");
            let argv = [&niri, "msg", "action", "screenshot-screen", "--path", &path];
            let argv = argv.map(|s| s.to_string()).to_vec();
            client.apps().launch(argv).call().await.unwrap();
            eprintln!("screenshot: {path}");
        };
        let long = Duration::from_secs(60);

        let (page, tv) = (client.browsers(0), client.browsers(1));
        page.start(true).with_timeout(long).call().await.unwrap();
        page.open("https://example.org/".to_owned())
            .with_timeout(long)
            .call()
            .await
            .unwrap();
        tv.start(true).with_timeout(long).call().await.unwrap();
        tv.open(video).with_timeout(long).call().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        for window in client.windows().list().call().await.unwrap() {
            eprintln!("window: {window:?}");
        }
        tv.show(None, None).call().await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        screenshot("1-video").await;

        // 4 s linear: halfway is half of each window on screen.
        let transition = ww_niri_api::Transition {
            duration: Millis::new(4000),
            curve: Curve::Linear,
        };
        page.show(Some(transition), None).call().await.unwrap();
        tokio::time::sleep(Duration::from_millis(2000)).await;
        screenshot("2-halfway").await;
        tokio::time::sleep(Duration::from_millis(3000)).await;
        screenshot("3-page").await;
        tv.show(Some(transition), None).call().await.unwrap();
        tokio::time::sleep(Duration::from_millis(5000)).await;

        // The slot's player, observed while it plays and once paused.
        let playback = tv.playback();
        if playback.read_state().read().await.is_err() {
            eprintln!("no player yet (autoplay blocked?): play()");
        }
        let _ = playback.play().call().await;
        let state = tv.read_state().read().await.unwrap();
        eprintln!("slot 1: {state:?}");
        assert_eq!(state.phase, BrowserPhase::Running);
        let player = state.player.expect("slot 1 has a player");
        let mut observed = client
            .media()
            .players(player.0)
            .playback()
            .observe_state(wire_weaver_client::observe::on_change(Duration::ZERO))
            .subscribe()
            .await
            .unwrap();
        let mut until = async |status: PlaybackStatus| {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let state = observed.recv().await.unwrap().value;
                    eprintln!("players[{}].playback.state: {state:?}", player.0);
                    if state.status == status {
                        break;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("observers get {status:?}"));
        };
        until(PlaybackStatus::Playing).await;
        screenshot("4-playing").await;
        playback.pause().call().await.unwrap();
        until(PlaybackStatus::Paused).await;
        screenshot("5-paused").await;
        tokio::time::sleep(Duration::from_secs(1)).await;

        for slot in [&page, &tv] {
            slot.stop().with_timeout(long).call().await.unwrap();
        }
    });
}
