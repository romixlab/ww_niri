//! One client connection: WebSocket over the accepted TCP stream, a `ww_device::Server` and the
//! API handlers, as one task on the compositor's executor.

use std::net::{SocketAddr, TcpStream};

use calloop::io::Async;
use calloop::LoopHandle;
use ww_device::ws::{WsConnection, WsRx, WsTx, WS_OVERHEAD};
use ww_device::LinkEvent;

use super::clock::LoopClock;
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

pub async fn serve<D: 'static>(
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
        LoopClock::new(handle),
        &mut scratch,
        &mut event_scratch,
    );
    let mut backend = Server::new();

    loop {
        let ready = server.wait().await;
        match server.handle(ready, &mut backend).await {
            Some(LinkEvent::Up) => debug!("ww: link to {peer} is up"),
            Some(LinkEvent::Down(reason)) => {
                // A connection is served once: the client reconnects with a new one.
                debug!("ww: link to {peer} is down: {reason:?}");
                break;
            }
            None => {}
        }
    }
}
