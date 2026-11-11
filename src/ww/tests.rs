//! The server over a real WebSocket: a calloop loop on a thread serves one connection against a
//! fake host, a generated client calls it.

use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use wire_weaver_client::{CallError, ClientConfig, Commander, WwClient};
use ww_niri_api::{
    EventOwned, OutputOwned, Transition, WindowError, WindowId, WindowOwned, WorkspaceId,
};

use super::connection::serve;
use super::host::WwHost;

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

/// What a test sees of the fake host's calls.
type Calls = Arc<Mutex<Vec<String>>>;

struct FakeHost {
    windows: Vec<WindowOwned>,
    calls: Calls,
    events: Option<Receiver<niri_ipc::Event>>,
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

impl FakeHost {
    fn op(&mut self, call: String, id: u64) -> bool {
        self.calls.lock().unwrap().push(call);
        self.windows.iter().any(|w| w.id.0 == id)
    }
}

impl WwHost for FakeHost {
    fn windows(&mut self) -> Vec<WindowOwned> {
        self.windows.clone()
    }

    fn window(&mut self, id: u64) -> Option<WindowOwned> {
        self.windows.iter().find(|w| w.id.0 == id).cloned()
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

    fn subscribe_events(&mut self) -> Option<Receiver<niri_ipc::Event>> {
        self.events.take()
    }
}

struct Bench {
    url: String,
    calls: Calls,
    events: Sender<niri_ipc::Event>,
    stop: calloop::channel::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

/// Serves one connection against a fake host with `windows`, on a calloop loop of its own.
fn bench(windows: Vec<WindowOwned>) -> Bench {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/ww", listener.local_addr().unwrap());
    let calls = Calls::default();
    let (events_tx, events_rx) = async_channel::unbounded();
    let (stop_tx, stop_rx) = calloop::channel::channel::<()>();

    let host_calls = calls.clone();
    let thread = std::thread::spawn(move || {
        let mut host = FakeHost {
            windows,
            calls: host_calls,
            events: Some(events_rx),
            stop: false,
        };
        let mut event_loop = calloop::EventLoop::<FakeHost>::try_new().unwrap();
        let handle = event_loop.handle();
        let (executor, scheduler) = calloop::futures::executor().unwrap();
        handle.insert_source(executor, |(), _, _| {}).unwrap();
        handle
            .insert_source(stop_rx, |_, _, host| host.stop = true)
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
        calls,
        events: events_tx,
        stop: stop_tx,
        thread,
    }
}

impl Bench {
    fn run(self, test: impl AsyncFnOnce(NiriClient, &Calls, &Sender<niri_ipc::Event>)) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = NiriClient::config(|c| c.websocket_url(self.url.clone()))
                .connect()
                .await
                .unwrap();
            test(client, &self.calls, &self.events).await;
        });
        self.stop.send(()).unwrap();
        self.thread.join().unwrap();
    }
}

#[test]
fn info_over_websocket() {
    bench(Vec::new()).run(async |client, _, _| {
        let info = client.info().call().await.unwrap();
        assert_eq!(info.ww_niri_version, env!("CARGO_PKG_VERSION"));
        assert!(!info.niri_version.is_empty());
    });
}

#[test]
fn windows_list_and_actions() {
    let windows = vec![window(1, "firefox"), window(2, "org.telegram.desktop")];
    bench(windows.clone()).run(async |client, calls, _| {
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
            *calls.lock().unwrap(),
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
    bench(vec![window(1, "firefox")]).run(async |client, _, niri_events| {
        let mut events = client.events().call().await.unwrap();

        let mut ipc_window = niri_ipc::Window {
            id: 1,
            title: None,
            app_id: Some("firefox".to_owned()),
            pid: None,
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
        };
        ipc_window.is_focused = true;
        niri_events
            .send(niri_ipc::Event::WindowOpenedOrChanged { window: ipc_window })
            .await
            .unwrap();
        niri_events
            .send(niri_ipc::Event::WindowFocusChanged { id: Some(1) })
            .await
            .unwrap();
        niri_events
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
