//! The control API over UDP, on the address and port of each WebSocket listener.
//!
//! Pointer motion from a touchpad goes here: over a lossy Wi-Fi, TCP holds every later move
//! until a lost segment is resent, while over UDP a lost move is skipped and the next one goes
//! through. Each host gets a connection of its own (a slot of `ww_device::udp::UdpPeers`),
//! served by the same loop as a WebSocket client. There is no authentication, as on the
//! WebSocket port.

use std::cell::RefCell;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::pin::pin;
use std::rc::Rc;

use async_channel::Receiver;
use calloop::futures::Scheduler;
use calloop::io::Async;
use calloop::LoopHandle;
use futures_util::future::{select, Either};
use ww_device::udp::{udp_server, Readiness, StdUdpSocket, UdpBuffers, UdpPeers};

use super::clock::LoopClock;
use super::connection::{link_config, run, MAX_MESSAGE_LEN};
use super::host::WwHost;
use super::server::Peer;

/// Hosts served at once on one port; with every slot taken, a new host ends the connection heard
/// from least recently.
const PEERS_MAX: usize = 8;

type Peers = UdpPeers<StdUdpSocket<LoopReadiness>, PEERS_MAX>;

/// The loop's readiness of a clone of the socket, which is never read itself.
struct LoopReadiness(RefCell<Async<'static, UdpSocket>>);

impl Readiness for LoopReadiness {
    #[expect(
        clippy::await_holding_refcell_ref,
        reason = "UdpPeers receives from one slot at a time, so one wait at a time"
    )]
    async fn readable(&self) -> io::Result<()> {
        let mut socket = self.0.try_borrow_mut().map_err(io::Error::other)?;
        socket.readable().await;
        Ok(())
    }
}

/// A bound UDP listener; dropping it stops taking new hosts, while open connections stay until
/// their links go down.
pub struct Listener {
    addr: SocketAddr,
    _stop: async_channel::Sender<()>,
}

impl Listener {
    /// The bound address; port 0 asked for any.
    #[cfg(test)]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        debug!("ww: udp {}: no new hosts", self.addr);
    }
}

/// Binds `addr` and takes hosts on the loop.
///
/// # Errors
///
/// The address can't be bound, or the loop can't watch the socket.
pub fn listen<D: WwHost>(
    handle: &LoopHandle<'static, D>,
    scheduler: &Scheduler<()>,
    addr: SocketAddr,
) -> anyhow::Result<Listener> {
    let socket = UdpSocket::bind(addr)?;
    let watched = handle.adapt_io(socket.try_clone()?)?;
    let addr = socket.local_addr()?;
    let socket = StdUdpSocket::new(socket, LoopReadiness(RefCell::new(watched)))?;
    let peers = Rc::new(Peers::new(socket));
    let (stop_tx, stop_rx) = async_channel::bounded(1);
    scheduler.schedule(accept(handle.clone(), scheduler.clone(), peers, stop_rx))?;
    Ok(Listener {
        addr,
        _stop: stop_tx,
    })
}

/// Gives each new host a connection, until the [Listener] is dropped.
async fn accept<D: WwHost>(
    handle: LoopHandle<'static, D>,
    scheduler: Scheduler<()>,
    peers: Rc<Peers>,
    stop: Receiver<()>,
) {
    loop {
        let slot = match select(pin!(peers.accept()), pin!(stop.recv())).await {
            Either::Left((Ok(slot), _)) => slot,
            Either::Left((Err(err), _)) => {
                warn!("ww: udp listener stopped: {err:?}");
                return;
            }
            Either::Right(_) => return,
        };
        let serve = serve(handle.clone(), peers.clone(), slot);
        if let Err(err) = scheduler.schedule(serve) {
            warn!("ww: error scheduling a udp connection: {err:?}");
        }
    }
}

/// Serves the host of `slot` until its link goes down, which frees the slot.
async fn serve<D: WwHost>(handle: LoopHandle<'static, D>, peers: Rc<Peers>, slot: usize) {
    let mut buffers = Box::new(UdpBuffers::<MAX_MESSAGE_LEN>::new());
    let server = udp_server(
        link_config(),
        peers.clone(),
        slot,
        LoopClock::new(handle.clone()),
        &mut buffers,
    );
    let on_up = |backend: &mut super::server::Server| {
        backend.peer = Peer::Udp(peers.peer(slot));
    };
    run(handle, server, Peer::Udp(None), on_up).await;
}
