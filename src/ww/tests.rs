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
    EventOwned, InputErrorOwned, LaunchErrorOwned, MediaErrorOwned, OutputOwned, Transition,
    WindowError, WindowId, WindowOwned, WorkspaceId,
};

use super::audio::Target;
use super::browsers::{LoopIo, Settings};
use super::connection::serve;
use super::host::WwHost;
use super::input::{Key, Op};
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
    /// Where the pointer is: the injected motions move it.
    pointer: Arc<Mutex<(f64, f64)>>,
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

    // A 1920x1080 output at (0, 0); window 7 at (100, 50), 800x600; the pointer where the last
    // motion put it, at first (110, 70); keys: a keycode per keysym, Shift with upper case
    // letters.
    fn input_devices(&mut self) -> Result<bool, String> {
        Ok(true)
    }

    fn inject(&mut self, ops: &[Op]) -> Result<(), InputErrorOwned> {
        for op in ops {
            if let Op::Motion { x, y } = *op {
                if !self.on_screen(x, y) {
                    return Err(InputErrorOwned::OffScreen);
                }
            }
        }
        for op in ops {
            if let Op::Motion { x, y } = *op {
                *self.fake.pointer.lock().unwrap() = (x, y);
                self.services().hub.pointer_moved();
            }
        }
        let mut calls = self.fake.calls.lock().unwrap();
        calls.extend(ops.iter().map(|op| format!("{op:?}")));
        Ok(())
    }

    fn on_screen(&mut self, x: f64, y: f64) -> bool {
        (0. ..1920.).contains(&x) && (0. ..1080.).contains(&y)
    }

    fn window_point(&mut self, id: u64, x: f64, y: f64) -> Result<(f64, f64), InputErrorOwned> {
        match id {
            7 => Ok((100. + x, 50. + y)),
            8 => Err(InputErrorOwned::WindowNotVisible),
            _ => Err(InputErrorOwned::NoSuchWindow),
        }
    }

    fn pointer_position(&mut self) -> ww_niri_api::PointerPosition {
        let (x, y) = *self.fake.pointer.lock().unwrap();
        let (wx, wy) = (x - 100., y - 50.);
        let window = ((0. ..800.).contains(&wx) && (0. ..600.).contains(&wy)).then(|| {
            ww_niri_api::WindowPoint {
                id: WindowId(7),
                x: (wx as i32).into(),
                y: (wy as i32).into(),
            }
        });
        ww_niri_api::PointerPosition {
            x: (x as i32).into(),
            y: (y as i32).into(),
            window,
        }
    }

    fn pointer_moved_by(&mut self, dx: f64, dy: f64) -> Result<(f64, f64), InputErrorOwned> {
        let output = smithay::utils::Rectangle::new((0, 0).into(), (1920, 1080).into());
        let from = *self.fake.pointer.lock().unwrap();
        super::input::moved_by(&[output], from, dx, dy).ok_or(InputErrorOwned::OffScreen)
    }

    fn key_for(&mut self, keysym: u32) -> Option<Key> {
        use smithay::input::keyboard::Keysym;

        let sym = Keysym::new(keysym);
        match sym {
            Keysym::Shift_L => {
                return Some(Key {
                    code: 50,
                    shift: false,
                })
            }
            Keysym::Control_L => {
                return Some(Key {
                    code: 37,
                    shift: false,
                })
            }
            _ => (),
        }
        let ch = sym.key_char().filter(|c| c.is_ascii())?;
        Some(Key {
            code: u32::from(ch.to_ascii_lowercase()),
            shift: ch.is_ascii_uppercase(),
        })
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
        pointer: Arc::new(Mutex::new((110., 70.))),
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

/// The device's error of a call that should fail with one.
fn device_err<T: std::fmt::Debug, E: std::fmt::Debug>(res: Result<T, CallError<E>>) -> E {
    match res {
        Err(CallError::Device(err)) => err,
        other => panic!("expected a device error, got {other:?}"),
    }
}

#[test]
fn input_pointer_and_keyboard() {
    use exact_units::Millis;
    use ww_niri_api::{Button, Modifiers, Point};

    bench(Vec::new()).run(async |client, fake| {
        let taken = || std::mem::take(&mut *fake.calls.lock().unwrap());
        {
            let input = client.input();
            let (pointer, keyboard) = (input.pointer(), input.keyboard());
            let window = |x: i32, y: i32| Point::Window {
                id: WindowId(7),
                x: x.into(),
                y: y.into(),
            };

            pointer.move_to(window(10, 20)).call().await.unwrap();
            assert_eq!(taken(), ["Motion { x: 110.0, y: 70.0 }"]);
            let position = pointer.read_position().read().await.unwrap();
            assert_eq!(position.window.unwrap().x.get(), 10);

            // The fake keymap has no Super_L: nothing goes in.
            let super_key = Modifiers {
                super_key: true,
                ..Modifiers::default()
            };
            pointer
                .click(None, Button::Left, 2, super_key)
                .call()
                .await
                .unwrap_err();
            assert_eq!(taken(), Vec::<String>::new());
            pointer
                .click(Some(window(0, 0)), Button::Right, 2, Modifiers::default())
                .call()
                .await
                .unwrap();
            let (down, up) = (
                "Button { code: 273, pressed: true }",
                "Button { code: 273, pressed: false }",
            );
            assert_eq!(
                taken(),
                ["Motion { x: 100.0, y: 50.0 }", down, up, down, up]
            );

            for (point, err) in [
                (
                    Point::Global {
                        x: 5000.into(),
                        y: 0.into(),
                    },
                    InputErrorOwned::OffScreen,
                ),
                (
                    Point::Window {
                        id: WindowId(8),
                        x: 0.into(),
                        y: 0.into(),
                    },
                    InputErrorOwned::WindowNotVisible,
                ),
                (
                    Point::Window {
                        id: WindowId(9),
                        x: 0.into(),
                        y: 0.into(),
                    },
                    InputErrorOwned::NoSuchWindow,
                ),
            ] {
                assert_eq!(device_err(pointer.move_to(point).call().await), err);
            }
            assert!(matches!(
                pointer
                    .click(None, Button::Left, 4, Modifiers::default())
                    .call()
                    .await,
                Err(CallError::Device(InputErrorOwned::BadArgument))
            ));

            pointer
                .scroll(None, 0, -240, Modifiers::default())
                .call()
                .await
                .unwrap();
            assert_eq!(taken(), ["Wheel { dx: 0.0, dy: -240.0 }"]);

            // A drag moves in steps over its time and ends where it was asked to.
            pointer
                .drag(window(0, 0), window(100, 0), Button::Left, Millis::new(64))
                .call()
                .await
                .unwrap();
            let steps = taken();
            assert_eq!(
                steps[..2],
                [
                    "Motion { x: 100.0, y: 50.0 }",
                    "Button { code: 272, pressed: true }"
                ]
            );
            assert_eq!(steps.len(), 2 + 4 + 1);
            assert_eq!(steps[5], "Motion { x: 200.0, y: 50.0 }");
            assert_eq!(steps[6], "Button { code: 272, pressed: false }");

            // Shift around the upper case letter.
            keyboard.type_text("aB".to_owned()).call().await.unwrap();
            assert_eq!(
                taken(),
                [
                    "Key { code: 97, pressed: true }",
                    "Key { code: 97, pressed: false }",
                    "Key { code: 50, pressed: true }",
                    "Key { code: 98, pressed: true }",
                    "Key { code: 98, pressed: false }",
                    "Key { code: 50, pressed: false }",
                ]
            );
            // Nothing typed when one character has no key.
            assert_eq!(
                device_err(keyboard.type_text("aж".to_owned()).call().await),
                InputErrorOwned::NotTypeable(u32::from('ж'))
            );
            assert_eq!(taken(), Vec::<String>::new());

            keyboard
                .press_keys("shift+x".to_owned())
                .call()
                .await
                .unwrap();
            assert_eq!(
                taken(),
                [
                    "Key { code: 50, pressed: true }",
                    "Key { code: 120, pressed: true }",
                    "Key { code: 120, pressed: false }",
                    "Key { code: 50, pressed: false }",
                ]
            );
            assert_eq!(
                device_err(keyboard.press_keys("hyper+x".to_owned()).call().await),
                InputErrorOwned::UnknownKey("hyper".to_owned())
            );

            // What a connection holds goes up when it ends.
            keyboard
                .key_down("x".to_owned(), None)
                .call()
                .await
                .unwrap();
            pointer.press(Button::Middle, None).call().await.unwrap();
            taken();
        }
        drop(client);
        let released = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let calls = fake.calls.lock().unwrap().clone();
                if !calls.is_empty() {
                    break calls;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("released when the connection ends");
        assert_eq!(
            released,
            [
                "Button { code: 274, pressed: false }",
                "Key { code: 120, pressed: false }",
            ]
        );
    });
}

/// The calls of `fake` from now on, once `until` holds for them.
async fn calls_until(fake: &Fake, until: impl Fn(&[String]) -> bool) -> Vec<String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let calls = fake.calls.lock().unwrap().clone();
            if until(&calls) {
                break calls;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("calls: {:?}", fake.calls.lock().unwrap()))
}

#[test]
fn input_for_a_touchpad_and_a_remote() {
    use exact_units::Millis;
    use wire_weaver_client::observe::on_change;
    use ww_niri_api::{Button, Modifiers, PointerPosition, WindowPoint};

    bench(Vec::new()).run(async |client, fake| {
        let taken = || std::mem::take(&mut *fake.calls.lock().unwrap());
        let input = client.input();
        let (pointer, keyboard) = (input.pointer(), input.keyboard());

        // Observers hear where the pointer is, and every move.
        let mut observed = pointer
            .observe_position(on_change(Duration::ZERO))
            .subscribe()
            .await
            .unwrap();
        let mut until = async |x: i32, y: i32| {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let position: PointerPosition = observed.recv().await.unwrap().value;
                    if (position.x.get(), position.y.get()) == (x, y) {
                        break position;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("observers get ({x}, {y})"))
        };
        let at = until(110, 70).await;
        assert_eq!(
            at.window,
            Some(WindowPoint {
                id: WindowId(7),
                x: 10.into(),
                y: 20.into()
            })
        );

        // Relative moves, stopped at the edge of the output.
        pointer.move_by(5.into(), (-3).into()).call().await.unwrap();
        assert_eq!(taken(), ["Motion { x: 115.0, y: 67.0 }"]);
        assert_eq!(until(115, 67).await.window.unwrap().x.get(), 15);
        pointer.move_by(5000.into(), 0.into()).call().await.unwrap();
        assert_eq!(taken(), ["Motion { x: 1919.0, y: 67.0 }"]);
        assert_eq!(until(1919, 67).await.window, None);

        // A quarter notch, with ctrl around it: a pinch.
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        pointer.scroll(None, 0, -30, ctrl).call().await.unwrap();
        assert_eq!(
            taken(),
            [
                "Modifier { code: 37, pressed: true }",
                "Wheel { dx: 0.0, dy: -30.0 }",
                "Modifier { code: 37, pressed: false }",
            ]
        );

        // A hold renewed in time stays down, once; past its limit it comes up by itself.
        let (down, up) = (
            "Button { code: 272, pressed: true }",
            "Button { code: 272, pressed: false }",
        );
        pointer
            .press(Button::Left, Some(Millis::new(60_000)))
            .call()
            .await
            .unwrap();
        pointer
            .press(Button::Left, Some(Millis::new(50)))
            .call()
            .await
            .unwrap();
        assert_eq!(calls_until(fake, |c| c.len() == 2).await, [down, up]);
        taken();

        keyboard
            .key_down("x".to_owned(), Some(Millis::new(60_000)))
            .call()
            .await
            .unwrap();
        keyboard
            .key_down("x".to_owned(), None)
            .call()
            .await
            .unwrap();
        keyboard.key_up("x".to_owned()).call().await.unwrap();
        assert_eq!(
            taken(),
            [
                "Key { code: 120, pressed: true }",
                "Key { code: 120, pressed: false }",
            ]
        );

        assert_eq!(
            device_err(
                pointer
                    .press(Button::Right, Some(Millis::new(0)))
                    .call()
                    .await
            ),
            InputErrorOwned::BadArgument
        );
        assert_eq!(taken(), Vec::<String>::new());
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
                    // As PipeWire: the cookie is ignored, but it must have 256 bytes.
                    protocol::Command::Auth(auth) if auth.cookie.len() != 256 => write(&mut |w| {
                        protocol::write_error(w, seq, &protocol::PulseError::Invalid).unwrap()
                    }),
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

    /// The fake's socket, and no cookie file, as on a box where nothing wrote one.
    fn target(&self) -> Target {
        let missing = self.socket.with_file_name("no-cookie");
        Target::with_cookie_file(self.socket.clone(), Some(missing))
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
fn audio_cookie() {
    let dir = Scratch::new("cookie");
    let socket = dir.0.join("native");
    let file = dir.0.join("cookie");
    std::fs::write(&file, [7u8; 256]).unwrap();
    let target = Target::with_cookie_file(socket.clone(), Some(file.clone()));
    assert_eq!(target.cookie, [7u8; 256]);
    // Missing or empty: zeros of the length PipeWire wants.
    for file in [None, Some(dir.0.join("missing")), Some(file)] {
        if let Some(file) = &file {
            if file.exists() {
                std::fs::write(file, []).unwrap();
            }
        }
        let target = Target::with_cookie_file(socket.clone(), file);
        assert_eq!(target.cookie, [0u8; 256]);
    }
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

/// The page of `live_input`: one text field over the whole window, and the last pointer, key,
/// zoom or text event in the title, which the slot's state reports.
const INPUT_PAGE: &str = r#"<!doctype html><title>ready</title>
<style>html,body{margin:0;height:100%}input{position:fixed;inset:0;width:100%;height:100%;border:0;font-size:40px}</style>
<form action="/done"><input name="q" autocomplete="off"></form>
<script>
const i = document.querySelector("input"), t = s => document.title = s;
let down = null, wheels = 0, zooms = 0;
addEventListener("mousemove", e => t(`move ${e.clientX} ${e.clientY}`));
addEventListener("mousedown", e => { down = [e.clientX, e.clientY]; t(`down ${e.button}`); });
addEventListener("mouseup", e => {
  if (down && Math.hypot(e.clientX - down[0], e.clientY - down[1]) > 5)
    { t(`drag ${down[0]} ${down[1]} ${e.clientX} ${e.clientY}`); down = "dragged"; }
});
addEventListener("click", e => { if (down !== "dragged") t(`click ${e.button} ${e.clientX} ${e.clientY} ${e.ctrlKey}`); down = null; });
addEventListener("dblclick", e => t(`dblclick ${e.clientX} ${e.clientY}`));
addEventListener("contextmenu", e => { e.preventDefault(); t(`context ${e.clientX} ${e.clientY}`); });
addEventListener("wheel", e => t(`wheel ${Math.sign(e.deltaY)} ${++wheels} ${Math.abs(e.deltaY)} ${e.ctrlKey}`));
addEventListener("resize", () => t(`zoom ${++zooms} ${devicePixelRatio}`));
addEventListener("keydown", e => {
  if (/^(Media|Browser)/.test(e.key)) { e.preventDefault(); t(`key ${e.key}`); }
});
i.addEventListener("input", () => t(`text ${i.value}`));
</script>"#;

/// A running ww_niri with a screen: `WW_NIRI_URL=ws://ADDR:7590/ww` and `WW_NIRI_LIVE_INPUT=1`
/// serve [INPUT_PAGE] on the niri host (python3 there, started through `apps.launch`: the host
/// may not reach the test's PC), open it in browser slot 0, then click, double click, right
/// click, scroll, drag, move by deltas, scroll by quarter notches, zoom with ctrl and scroll,
/// press media keys, hold a button past its hold limit and type into it, and read each back
/// from the page's title. Screenshots to `/tmp/ww-input-*.png` there, as `live_screen` takes
/// them.
#[test]
fn live_input() {
    use exact_units::Millis;
    use ww_niri_api::{Button, Modifiers, Point};

    let (Some(url), Some(_)) = (
        std::env::var_os("WW_NIRI_URL"),
        std::env::var_os("WW_NIRI_LIVE_INPUT"),
    ) else {
        eprintln!("live_input: skipped, set WW_NIRI_URL and WW_NIRI_LIVE_INPUT=1 to run against a running niri");
        return;
    };
    let url = url.into_string().unwrap();
    let niri = std::env::var("WW_NIRI_BIN").unwrap_or_else(|_| "niri".to_owned());
    let port = 8799;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let client = NiriClient::config(|c| c.websocket_url(url.clone()))
            .connect()
            .await
            .unwrap();
        // Without an output no window is on screen: say so before anything starts.
        let outputs = client.outputs().list().call().await.unwrap();
        assert!(
            !outputs.is_empty(),
            "live_input needs a connected output (is the screen on?)"
        );
        let long = Duration::from_secs(60);
        let launch = async |argv: Vec<String>| client.apps().launch(argv).call().await.unwrap();
        let screenshot = async |name: &str| {
            let path = format!("/tmp/ww-input-{name}.png");
            let argv = [&niri, "msg", "action", "screenshot-screen", "--path", &path];
            launch(argv.map(|s| s.to_string()).to_vec()).await;
        };

        let server = format!(
            "import http.server\nP={:?}.encode()\nclass H(http.server.BaseHTTPRequestHandler):\n def do_GET(s):\n  s.send_response(200); s.send_header('Content-Type', 'text/html'); s.end_headers(); s.wfile.write(P)\n def log_message(*a): pass\nhttp.server.HTTPServer(('127.0.0.1', {port}), H).serve_forever()\n",
            INPUT_PAGE
        );
        let server = launch(vec!["python3".into(), "-c".into(), server]).await;

        let slot = client.browsers(0);
        slot.start(true).with_timeout(long).call().await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        slot.open(format!("http://127.0.0.1:{port}/"))
            .with_timeout(long)
            .call()
            .await
            .unwrap();
        slot.show(None, Some(true)).call().await.unwrap();

        // Waits until the page's title starts with `want`.
        let title = async |want: &str| {
            let got = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let state = slot.read_state().read().await.unwrap();
                    let title = state.title.unwrap_or_default();
                    if title.starts_with(want) {
                        break title;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            match got {
                Ok(title) => eprintln!("title: {title}"),
                Err(_) => panic!(
                    "the page's title starts with {want:?}; it is {:?}",
                    slot.read_state().read().await.unwrap().title
                ),
            }
        };
        title("ready").await;
        let window = slot.read_state().read().await.unwrap().window.unwrap();
        let at = |x: i32, y: i32| Point::Window {
            id: window,
            x: x.into(),
            y: y.into(),
        };
        let input = client.input();
        let (pointer, keyboard) = (input.pointer(), input.keyboard());
        let none = Modifiers::default();

        pointer.move_to(at(300, 200)).call().await.unwrap();
        let position = pointer.read_position().read().await.unwrap();
        eprintln!("position: {position:?}");

        pointer
            .click(Some(at(300, 200)), Button::Left, 1, none)
            .call()
            .await
            .unwrap();
        title("click 0 300 200").await;
        screenshot("1-clicked").await;
        pointer
            .click(Some(at(500, 250)), Button::Left, 2, none)
            .call()
            .await
            .unwrap();
        title("dblclick 500 250").await;
        pointer
            .click(Some(at(400, 300)), Button::Right, 1, none)
            .call()
            .await
            .unwrap();
        title("context 400 300").await;
        pointer.scroll(None, 0, 120, none).call().await.unwrap();
        title("wheel 1").await;
        pointer
            .drag(at(100, 100), at(600, 400), Button::Left, Millis::new(300))
            .call()
            .await
            .unwrap();
        title("drag 100 100 600 400").await;

        // Relative moves, observed; small ones in a row add up.
        let mut observed = pointer
            .observe_position(wire_weaver_client::observe::on_change(Duration::ZERO))
            .subscribe()
            .await
            .unwrap();
        pointer.move_to(at(300, 200)).call().await.unwrap();
        title("move 300 200").await;
        let start = pointer.read_position().read().await.unwrap();
        pointer.move_by(40.into(), 30.into()).call().await.unwrap();
        title("move 340 230").await;
        for _ in 0..10 {
            pointer.move_by(1.into(), 1.into()).call().await.unwrap();
        }
        title("move 350 240").await;
        let (x, y) = (start.x.get() + 50, start.y.get() + 40);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let seen = observed.recv().await.unwrap().value;
                if (seen.x.get(), seen.y.get()) == (x, y) {
                    break;
                }
            }
        })
        .await
        .expect("observers hear the moves");

        // Quarter notches scroll smoothly: they come as wheel events of their own, each smaller
        // than a notch's.
        let wheels = async |at_least: u32| {
            let parse = |title: &str| -> Option<(u32, f64)> {
                let parts: Vec<&str> = title.split(' ').collect();
                match parts[..] {
                    ["wheel", _, count, delta, ..] => Some((count.parse().ok()?, delta.parse().ok()?)),
                    _ => None,
                }
            };
            let got = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let title = slot.read_state().read().await.unwrap().title;
                    match parse(&title.unwrap_or_default()) {
                        Some((count, delta)) if count >= at_least => break (count, delta),
                        _ => tokio::time::sleep(Duration::from_millis(50)).await,
                    }
                }
            })
            .await;
            got.unwrap_or_else(|_| panic!("{at_least} wheel events"))
        };
        pointer.scroll(None, 0, 120, none).call().await.unwrap();
        let (notches, notch) = wheels(0).await;
        for _ in 0..4 {
            pointer.scroll(None, 0, 30, none).call().await.unwrap();
        }
        // libinput holds back a wheel's first half notch; the rest come one by one.
        let (count, delta) = wheels(notches + 3).await;
        eprintln!("a notch scrolled {notch}, {} quarter notches {delta} each", count - notches);
        assert!(delta < notch, "a quarter notch scrolled {delta}, a notch {notch}");

        // ctrl and scroll up zoom in; ctrl+0 back.
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        pointer.scroll(None, 0, -120, ctrl).call().await.unwrap();
        title("zoom 1").await;
        screenshot("3-zoomed").await;
        keyboard.press_keys("ctrl+0".to_owned()).call().await.unwrap();
        title("zoom 2").await;
        // Modifiers reach niri in order with the click they go with.
        pointer
            .click(Some(at(250, 180)), Button::Left, 1, ctrl)
            .call()
            .await
            .unwrap();
        title("click 0 250 180 true").await;

        for (combo, key) in [
            ("XF86AudioPlay", "MediaPlay"),
            ("XF86Search", "BrowserSearch"),
        ] {
            keyboard.press_keys(combo.to_owned()).call().await.unwrap();
            title(&format!("key {key}")).await;
        }

        // A held button comes up by itself after its hold limit: a click.
        pointer.move_to(at(200, 150)).call().await.unwrap();
        pointer
            .press(Button::Left, Some(Millis::new(500)))
            .call()
            .await
            .unwrap();
        let pressed = std::time::Instant::now();
        title("down 0").await;
        title("click 0 200 150").await;
        eprintln!("held for {:?}", pressed.elapsed());

        // The drag selected nothing worth keeping: select all, then type over it.
        pointer
            .click(Some(at(300, 200)), Button::Left, 1, none)
            .call()
            .await
            .unwrap();
        keyboard.press_keys("ctrl+a".to_owned()).call().await.unwrap();
        keyboard
            .type_text("Hello, ww_niri! 42".to_owned())
            .call()
            .await
            .unwrap();
        title("text Hello, ww_niri! 42").await;
        screenshot("2-typed").await;
        keyboard.press_keys("Return".to_owned()).call().await.unwrap();
        let sent = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let url = slot.read_state().read().await.unwrap().url.unwrap_or_default();
                if url.contains("/done?q=") {
                    break url;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("Return sends the form");
        assert!(sent.ends_with("q=Hello%2C+ww_niri%21+42"), "{sent}");

        slot.stop().with_timeout(long).call().await.unwrap();
        launch(vec!["kill".into(), server.pid.to_string()]).await;
    });
}
