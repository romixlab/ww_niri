//! The server over a real WebSocket: a calloop loop on a thread serves one connection against a
//! fake host, a generated client calls it.

use std::cell::RefCell;
use std::net::TcpListener;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use wire_weaver_client::{CallError, ClientConfig, Commander, WwClient};
use ww_niri_api::{
    EventOwned, LaunchErrorOwned, OutputOwned, Transition, WindowError, WindowId, WindowOwned,
    WorkspaceId,
};

use super::browsers::{LoopIo, Settings};
use super::connection::serve;
use super::host::{runner, WwHost};
use super::Browsers;

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
    browsers: Option<Rc<Browsers>>,
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

    fn browsers(&mut self) -> Rc<Browsers> {
        self.browsers.clone().unwrap()
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
    bench_with(windows, Settings::default())
}

/// The same, with browser slots as `settings` say.
fn bench_with(windows: Vec<WindowOwned>, settings: Settings) -> Bench {
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
        let mut host = FakeHost {
            fake: host_fake,
            hub: hub.clone(),
            browsers: Some(Browsers::new(
                settings,
                runner(handle.clone()),
                LoopIo::new(handle.clone()),
                scheduler.clone(),
                2,
            )),
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

    bench_with(Vec::new(), settings).run(async |client, fake| {
        let mut events = client.events().call().await.unwrap();
        let mut next = async || {
            tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("an event")
                .unwrap()
        };
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

        // The page's load event moves the URL on.
        let state = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = slot.read_state().read().await.unwrap();
                if state.url.as_deref() == Some("https://example.org/next") {
                    break state;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the URL follows the page");
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

        // The fake ignores browser.close: SIGTERM ends it.
        slot.stop().call().await.unwrap();
        assert_eq!(
            next().await,
            EventOwned::BrowserStopped {
                slot: BrowserSlot(0),
                reason: StopReason::Stopped
            }
        );
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

    bench_with(Vec::new(), settings).run(async |client, _| {
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
