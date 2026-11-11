//! One client connection: WebSocket over the accepted TCP stream, a `ww_device::Server` and the
//! API handlers, as one task on the compositor's executor.

use std::net::{SocketAddr, TcpStream};
use std::pin::pin;

use async_channel::Receiver;
use calloop::io::Async;
use calloop::LoopHandle;
use futures_util::future::{select, Either};
use ww_device::ws::{WsConnection, WsRx, WsTx, WS_OVERHEAD};
use ww_device::LinkEvent;
use ww_niri_api::{EventOwned, WindowId};

use super::clock::LoopClock;
use super::host::{call, runner, Run, WwHost};
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
/// without IPC) and the browser slots'.
enum Events {
    Off,
    On {
        niri: Option<Receiver<niri_ipc::Event>>,
        browsers: Receiver<EventOwned>,
    },
}

enum Next {
    Niri(niri_ipc::Event),
    Ww(EventOwned),
    /// A queue was dropped for falling behind.
    Behind,
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
    let Some(browsers) = call(&run, |host| host.browsers()).await else {
        return;
    };
    let mut backend = Server::new(run.clone(), peer, browsers);
    let mut events = Events::Off;

    loop {
        // Follow niri's events while the client has the stream open.
        match (&events, backend.events_open) {
            (Events::Off, true) => {
                events = Events::On {
                    niri: call(&run, |host| host.subscribe_events()).await.flatten(),
                    browsers: backend.browsers.subscribe(),
                };
            }
            (Events::On { .. }, false) => events = Events::Off,
            _ => (),
        }

        let step = {
            let next_event = pin!(async {
                let Events::On { niri, browsers } = &events else {
                    return std::future::pending().await;
                };
                let niri = pin!(async {
                    match niri {
                        Some(rx) => rx.recv().await.map_or(Next::Behind, Next::Niri),
                        None => std::future::pending().await,
                    }
                });
                let ww = pin!(async { browsers.recv().await.map_or(Next::Behind, Next::Ww) });
                select(niri, ww).await.factor_first().0
            });
            match select(pin!(server.wait()), next_event).await {
                Either::Left((ready, _)) => Either::Left(ready),
                Either::Right((event, _)) => Either::Right(event),
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
            Either::Right(event) => {
                let out = match event {
                    Next::Niri(event) => {
                        let mut out = map_event(&run, event).await;
                        for event in &mut out {
                            if let EventOwned::WindowOpenedOrChanged(window) = event {
                                backend.mark_browser(window);
                            }
                        }
                        out
                    }
                    Next::Ww(event) => vec![event],
                    Next::Behind => {
                        // A subscription was dropped, as niri does for any client too slow to
                        // read: subscribe again and have the client list again.
                        debug!("ww: {peer} fell behind on events");
                        events = Events::Off;
                        vec![EventOwned::Resync]
                    }
                };
                if server.is_up() && backend.events_open {
                    for event in out {
                        let _ = api_impl::stream_data_ser()
                            .events_send(&event, &mut server.sink())
                            .await;
                    }
                }
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
