//! One client connection: WebSocket over the accepted TCP stream, a `ww_device::Server` and the
//! API handlers, as one task on the compositor's executor.

use std::future::Future;
use std::net::{SocketAddr, TcpStream};
use std::pin::pin;
use std::time::Instant;

use async_channel::Receiver;
use calloop::io::Async;
use calloop::LoopHandle;
use futures_util::future::{select, Either};
use ww_device::ws::{WsConnection, WsRx, WsTx, WS_OVERHEAD};
use ww_device::LinkEvent;
use ww_niri_api::{EventOwned, WindowId};

use super::clock::LoopClock;
use super::host::{call, runner, Run, WwHost};
use super::hub::Mark;
use super::server::{api_impl, Server};
use super::socket::StreamSocket;
use crate::niri::State;

/// Longest message either way: window and player lists of a busy session fit.
const MAX_MESSAGE_LEN: usize = 32 * 1024;

pub fn spawn(state: &mut State, stream: TcpStream, peer: SocketAddr) {
    let _span = tracy_client::span!("ww::connection::spawn");

    if let Err(err) = stream.set_nodelay(true) {
        warn!("ww: error setting TCP_NODELAY for {peer}: {err:?}");
    }
    let stream = match state.niri.event_loop.adapt_io(stream) {
        Ok(stream) => stream,
        Err(err) => {
            warn!("ww: error making the stream of {peer} async: {err:?}");
            return;
        }
    };

    let handle = state.niri.event_loop.clone();
    let future = serve(handle, stream, peer);
    if let Err(err) = state.niri.scheduler.schedule(future) {
        warn!("ww: error scheduling the connection of {peer}: {err:?}");
    }
}

/// What the client's `events` stream follows while open: niri's events (`None` when niri runs
/// without IPC) and the ww side's own (browser slots, players).
enum Events {
    Off,
    On {
        niri: Option<Receiver<niri_ipc::Event>>,
        ww: Receiver<EventOwned>,
    },
}

enum Next {
    Niri(niri_ipc::Event),
    Ww(EventOwned),
    /// A queue was dropped for falling behind.
    Behind,
    /// The default sink changed.
    Audio,
    /// One of the ww side's events, for observed properties under `browsers[]` and `players[]`.
    Mark(Mark),
    /// The pointer moved, or the window under it.
    Pointer,
    /// A hold limit of a button or key the client holds passed.
    HoldLimit,
}

/// The first of two to come.
async fn race(a: impl Future<Output = Next>, b: impl Future<Output = Next>) -> Next {
    select(pin!(a), pin!(b)).await.factor_first().0
}

pub async fn serve<D: WwHost>(
    handle: LoopHandle<'static, D>,
    stream: Async<'static, TcpStream>,
    peer: SocketAddr,
) {
    debug!("ww: {peer} connected");

    let conn = WsConnection::new(StreamSocket::new(stream));
    let mut tx_buf = vec![0u8; MAX_MESSAGE_LEN + WS_OVERHEAD];
    let mut rx_buf = vec![0u8; MAX_MESSAGE_LEN + WS_OVERHEAD];
    let mut scratch = vec![0u8; MAX_MESSAGE_LEN];
    let mut event_scratch = vec![0u8; MAX_MESSAGE_LEN];

    let config = ww_device::LinkConfig::new(
        ww_niri_api::WW_NIRI_FULL_GID,
        ww_niri_api::WW_NIRI_WIRE_VERSION,
        api_impl::api_hash(),
        api_impl::image_facts(),
        ww_client_server::COMPACT_VERSION,
    );
    let mut server = ww_device::Server::new(
        config,
        WsTx::new(&conn, &mut tx_buf),
        WsRx::new(&conn, &mut rx_buf),
        LoopClock::new(handle.clone()),
        &mut scratch,
        &mut event_scratch,
    );
    let run = runner(handle);
    let Some(services) = call(&run, |host| host.services()).await else {
        return;
    };
    let mut backend = Server::new(run.clone(), peer, services);
    let mut sources = Sources {
        events: Events::Off,
        audio: backend.audio.watch(),
        marks: backend.hub.subscribe_marks(),
        pointer: backend.hub.subscribe_pointer(),
    };
    let mut observe_buf = vec![0u8; MAX_MESSAGE_LEN];

    loop {
        sources.follow_events(&backend, &run).await;
        let step = {
            let next = pin!(sources.next(&backend.input));
            // Observed properties: wake when one is due (a period, a held-back change).
            let wake = backend
                .observe
                .next_wake(server.now().as_micros())
                .map(ww_device::Instant::from_micros);
            match select(pin!(server.wait_until(wake)), next).await {
                Either::Left((ready, _)) => Either::Left(ready),
                Either::Right((next, _)) => Either::Right(next),
            }
        };
        match step {
            Either::Left(ready) => match server.handle(ready, &mut backend).await {
                Some(LinkEvent::Up) => debug!("ww: link to {peer} is up"),
                Some(LinkEvent::Down(reason)) => {
                    // A connection is served once: the client reconnects with a new one.
                    debug!("ww: link to {peer} is down: {reason:?}");
                    break;
                }
                None => {}
            },
            Either::Right(next) => {
                let out = sources.handle(next, &mut backend, &run).await;
                if server.is_up() && backend.events_open {
                    for event in out {
                        let _ = api_impl::stream_data_ser()
                            .events_send(&event, &mut server.sink())
                            .await;
                    }
                }
            }
        }
        if server.is_up() {
            let now = server.now().as_micros();
            let _ = backend
                .poll_properties(&mut server.sink(), (), now, &mut observe_buf)
                .await;
        }
    }
    // Nothing the client held down stays down.
    if let Err(err) = backend.input.release_all().await {
        warn!("ww: error releasing what {peer} held: {err:?}");
    }
}

/// What a connection waits on besides the client.
struct Sources {
    events: Events,
    /// The default sink changed.
    audio: Receiver<()>,
    /// Observed element properties follow the ww side's events whether or not the events
    /// stream is open.
    marks: Receiver<Mark>,
    pointer: Receiver<()>,
}

impl Sources {
    /// Follows niri's events while the client has the stream open.
    async fn follow_events(&mut self, backend: &Server, run: &Run) {
        match (&self.events, backend.events_open) {
            (Events::Off, true) => {
                self.events = Events::On {
                    niri: call(run, |host| host.subscribe_events()).await.flatten(),
                    ww: backend.hub.subscribe(),
                };
            }
            (Events::On { .. }, false) => self.events = Events::Off,
            _ => (),
        }
    }

    /// The next thing to happen; `input` for its hold limits.
    async fn next(&self, input: &super::input::Input) -> Next {
        let event = async {
            let Events::On { niri, ww } = &self.events else {
                return std::future::pending().await;
            };
            let niri = async {
                match niri {
                    Some(rx) => rx.recv().await.map_or(Next::Behind, Next::Niri),
                    None => std::future::pending().await,
                }
            };
            let ww = async { ww.recv().await.map_or(Next::Behind, Next::Ww) };
            race(niri, ww).await
        };
        let audio = async {
            match self.audio.recv().await {
                Ok(()) => Next::Audio,
                Err(_) => std::future::pending().await,
            }
        };
        let mark = async {
            match self.marks.recv().await {
                Ok(event) => Next::Mark(event),
                // Fell behind: resubscribed in `handle`.
                Err(_) => Next::Behind,
            }
        };
        let pointer = async {
            match self.pointer.recv().await {
                Ok(()) => Next::Pointer,
                Err(_) => std::future::pending().await,
            }
        };
        let hold_limit = async {
            input.hold_limit_passes().await;
            Next::HoldLimit
        };
        let changed = race(race(audio, mark), race(pointer, hold_limit));
        race(event, changed).await
    }

    /// Acts on what happened; answers the events for the client's stream.
    async fn handle(&mut self, next: Next, backend: &mut Server, run: &Run) -> Vec<EventOwned> {
        let peer = backend.peer;
        match next {
            Next::Niri(event) => {
                let mut out = map_event(run, event).await;
                for event in &mut out {
                    if let EventOwned::WindowOpenedOrChanged(window) = event {
                        backend.mark_browser(window);
                    }
                }
                out
            }
            Next::Ww(event) => vec![event],
            Next::Mark(event) => {
                backend.mark(&event).await;
                Vec::new()
            }
            Next::Behind if self.marks.is_closed() => {
                self.marks = backend.hub.subscribe_marks();
                backend.mark_all();
                Vec::new()
            }
            Next::Audio => {
                backend.mark_changed_audio_volume();
                backend.mark_changed_audio_muted();
                backend.mark_changed_audio_sink();
                Vec::new()
            }
            Next::Pointer => {
                backend.mark_changed_input_pointer_position();
                Vec::new()
            }
            Next::HoldLimit => {
                match backend.input.release_expired(Instant::now()).await {
                    Ok(count) => info!("ww: {count} held by {peer} came up: hold limit"),
                    Err(err) => warn!("ww: error releasing what {peer} held: {err:?}"),
                }
                Vec::new()
            }
            Next::Behind => {
                // A subscription was dropped, as niri does for any client too slow to read:
                // subscribe again and have the client list again.
                debug!("ww: {peer} fell behind on events");
                self.events = Events::Off;
                vec![EventOwned::Resync]
            }
        }
    }
}

/// The API's events for one of niri's; most of niri's have no counterpart.
async fn map_event(run: &Run, event: niri_ipc::Event) -> Vec<EventOwned> {
    use niri_ipc::Event;

    let changed = |ids: Vec<u64>| async move {
        call(run, move |host| {
            ids.into_iter()
                .filter_map(|id| host.window(id))
                .map(EventOwned::WindowOpenedOrChanged)
                .collect()
        })
        .await
        .unwrap_or_default()
    };

    match event {
        Event::WindowOpenedOrChanged { window } => changed(vec![window.id]).await,
        // Fullscreen and moves between outputs show up as layout changes.
        Event::WindowLayoutsChanged { changes } => {
            changed(changes.into_iter().map(|(id, _)| id).collect()).await
        }
        Event::WindowClosed { id } => vec![EventOwned::WindowClosed(WindowId(id))],
        Event::WindowFocusChanged { id } => {
            vec![EventOwned::WindowFocusChanged(id.map(WindowId))]
        }
        Event::WindowsChanged { .. } => vec![EventOwned::Resync],
        _ => Vec::new(),
    }
}
