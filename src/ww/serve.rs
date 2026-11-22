//! The listeners of one `listen` address, served by `wire_weaver_server` on the compositor's loop
//! (`ww_calloop`): WebSocket over TCP at `ws://ADDR/ww` (and `/`), and UDP on the same address
//! and port.
//!
//! Pointer motion from a touchpad goes over UDP: over a lossy Wi-Fi, TCP holds every later move
//! until a lost segment is resent, while over UDP a lost move is skipped and the next one goes
//! through. Each UDP host gets a connection of its own, up to [UDP_PEERS] at once. There is no
//! authentication on either.

use std::future::pending;
use std::net::SocketAddr;
use std::rc::Rc;

use calloop::futures::Scheduler;
use calloop::LoopHandle;
use futures_util::future::join3;
use wire_weaver_server::byte_stream::{listen, WsFraming};
use wire_weaver_server::{Connection, Server as WwServer};
use ww_calloop::{bind_udp, LoopClock, LoopUdpStop, TcpIncoming, TcpStop};

use super::host::{runner, WwHost};
use super::server::{api_impl, Peer, Server};
use super::Services;

/// Longest message either way: window and player lists of a busy session fit.
pub const MESSAGE_LEN_MAX: usize = 32 * 1024;

/// UDP hosts served at once on one port; with every slot taken, a new host ends the connection
/// heard from least recently.
pub const UDP_PEERS: usize = 8;

/// The WebSocket path clients use; `/` reaches the same API.
const WS_SERVICE: &str = "ww";

/// The listeners of one address, served on the loop. [Bound::stop] stops them; dropping this
/// leaves them running.
pub struct Bound {
    addr: SocketAddr,
    tcp: TcpStop<'static>,
    udp: LoopUdpStop<'static, UDP_PEERS>,
}

impl Bound {
    /// The address both listen on, with the port the system picked for port 0.
    #[cfg(test)]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Takes no new clients on either listener and frees the address; open connections stay
    /// until their links go.
    pub fn stop(&self) {
        self.tcp.stop();
        self.udp.stop();
        debug!("ww: {}: no new clients", self.addr);
    }
}

/// The link's side of the API, the same on every medium.
pub fn link_config() -> ww_device::LinkConfig<'static> {
    ww_device::LinkConfig::new(
        ww_niri_api::WW_NIRI_FULL_GID,
        ww_niri_api::WW_NIRI_WIRE_VERSION,
        api_impl::api_hash(),
        api_impl::image_facts(),
        ww_client_server::COMPACT_VERSION,
    )
}

/// Binds UDP and TCP on `addr`, both or neither, and serves them on the loop. Port 0 picks one
/// port for both.
///
/// # Errors
///
/// An address can't be bound, the loop can't watch a socket, or the executor is gone.
pub fn bind<D: WwHost>(
    handle: &LoopHandle<'static, D>,
    scheduler: &Scheduler<()>,
    services: &Rc<Services>,
    addr: SocketAddr,
) -> anyhow::Result<Bound> {
    let udp = bind_udp::<_, UDP_PEERS>(handle, addr, MESSAGE_LEN_MAX)?;
    let addr = udp.local_addr()?;
    let tcp = TcpIncoming::bind(handle, addr)?;
    let bound = Bound {
        addr,
        tcp: tcp.stop_handle(),
        udp: udp.stop_handle(),
    };

    let clock = LoopClock::new(handle.clone());
    let root = listen(tcp, WsFraming::new(MESSAGE_LEN_MAX), clock.clone());
    let ww = root.service(WS_SERVICE);
    let run = runner(handle.clone());
    let make = |medium: fn(wire_weaver::Peer) -> Peer| {
        let (run, services) = (run.clone(), services.clone());
        move |connection: &Connection| {
            let peer = medium(connection.peer);
            debug!("ww: link to {peer} is up");
            Server::new(run.clone(), peer, services.clone())
        }
    };
    let (ws, udp_make) = (make(Peer::WebSocket), make(Peer::Udp));
    let serve = async move {
        let server = WwServer::new(clock);
        let (root, ww, udp) = join3(
            server.serve(link_config(), root, &ws, pending::<()>()),
            server.serve(link_config(), ww, &ws, pending::<()>()),
            server.serve(link_config(), udp, udp_make, pending::<()>()),
        )
        .await;
        for res in [root, ww] {
            if let Err(err) = res {
                warn!("ww: WebSocket listener on {addr} failed: {err:?}");
            }
        }
        if let Err(err) = udp {
            warn!("ww: udp listener on {addr} failed: {err:?}");
        }
        debug!("ww: {addr}: its listeners and their connections ended");
    };
    scheduler.schedule(serve)?;
    Ok(bound)
}
