//! The device thread: `wire_weaver_server` on the in-process transport, as ww_niri serves its
//! WebSocket, and the motion path, the same API on a simulated UDP socket, as ww_niri serves it
//! over UDP; a ticker that brings the model up to its deadlines, so every connection hears what
//! came due.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::watch;
use wire_weaver_client::in_process::{self, TokioClock};
use wire_weaver_server::in_process::InProcessListener;
use wire_weaver_server::udp::UdpListener;
use wire_weaver_server::{Accept, Accepted, Connection, Listener};
use ww_device::MessageRx;

use crate::server::{api_impl, Server};
use crate::{Control, Shared};

/// Longest message either way: window and player lists fit.
pub(crate) const MESSAGE_LEN_MAX: usize = 32 * 1024;

/// Hosts the motion path serves at once, as ww_niri's UDP listener.
const MOTION_PEERS: usize = 8;

/// The link's side of the API, the same on both paths; [Shared::api_hash] overrides `api_hash()`.
fn link_config(shared: &Shared) -> ww_device::LinkConfig<'static> {
    let mut config = ww_device::LinkConfig::new(
        ww_niri_api::WW_NIRI_FULL_GID,
        ww_niri_api::WW_NIRI_WIRE_VERSION,
        api_impl::api_hash(),
        api_impl::image_facts(),
        ww_client_server::COMPACT_VERSION,
    );
    if let Some(hash) = shared.api_hash {
        config.api_hash = hash;
    }
    config
}

/// Where the listener of `base` is after `binds` rebinds.
pub(crate) fn bound_path(base: &str, binds: u32) -> String {
    if binds == 0 {
        base.to_owned()
    } else {
        format!("{base}~{binds}")
    }
}

/// Serves both paths until the fake is dropped.
pub(crate) async fn run(shared: Arc<Shared>, path: String, motion_path: String) {
    let server = wire_weaver_server::Server::new(TokioClock::new());
    let main = Paths::new(&shared, 0, path, |path| {
        InProcessListener::new(path, MESSAGE_LEN_MAX)
    });
    let motion = Paths::new(&shared, 1, motion_path, |path| {
        UdpListener::<_, MOTION_PEERS>::new(in_process::udp_device(path), MESSAGE_LEN_MAX)
    });
    let make_main = |_: &Connection| {
        shared.links_up.fetch_add(1, Ordering::Relaxed);
        Server::new(shared.clone())
    };
    let make_motion = |_: &Connection| Server::new(shared.clone());
    let (main, motion, ()) = tokio::join!(
        server.serve(link_config(&shared), main, make_main, stopped(&shared)),
        server.serve(link_config(&shared), motion, make_motion, stopped(&shared)),
        tick(&shared),
    );
    if let Err(never) = main {
        match never {}
    }
    // The simulated socket fails only once its path is unregistered: nothing is served there
    // any more either way.
    let _motion_ended = motion;
}

/// Completes once the fake is dropped.
async fn stopped(shared: &Shared) {
    let mut control = shared.control.subscribe();
    // The sender lives as long as `shared`.
    let _stopped = control.wait_for(|c| c.stop).await;
}

/// Brings the model up to the clock at each of its deadlines, until the fake is dropped.
async fn tick(shared: &Shared) {
    let mut control = shared.control.subscribe();
    loop {
        if control.borrow_and_update().stop {
            return;
        }
        let deadline = shared.catch_up();
        tokio::select! {
            () = shared.wake.notified() => {}
            () = shared.clock.until(deadline) => {}
            changed = control.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
    }
}

/// The listener of one path as the knobs have it: unregistered while unreachable, moved by
/// [FakeTv::rebind](crate::FakeTv::rebind), and its connections [Cut] by the knobs.
struct Paths<L, B> {
    shared: Arc<Shared>,
    /// Which path for [Shared::set_bound]: 0 main, 1 motion.
    index: usize,
    base: String,
    bind: B,
    /// The listener, and the rebinds and drops it was made after: a drop registers the path
    /// again, so a client on the simulated socket sees its link go at once.
    bound: Option<((u32, u32), L)>,
    control: watch::Receiver<Control>,
}

impl<L, B: Fn(&str) -> L> Paths<L, B> {
    fn new(shared: &Arc<Shared>, index: usize, base: String, bind: B) -> Self {
        Self {
            shared: shared.clone(),
            index,
            base,
            bind,
            bound: None,
            control: shared.control.subscribe(),
        }
    }
}

impl<L: Listener, B: Fn(&str) -> L> Listener for Paths<L, B> {
    type Tx = L::Tx;
    type Rx = Cut<L::Rx>;
    type Error = L::Error;

    /// Cancel-safe, as the inner listener's accept and the control's changes are.
    async fn accept(&mut self) -> Accept<L::Tx, Cut<L::Rx>, L::Error> {
        loop {
            let control = *self.control.borrow_and_update();
            if control.stop {
                return Ok(None);
            }
            let wanted = (control.reachable).then_some((control.binds, control.drops));
            if self.bound.as_ref().map(|(made, _)| *made) != wanted {
                // The old path goes before the new one is registered.
                self.bound = None;
                self.bound = wanted.map(|made| {
                    let listener = (self.bind)(&bound_path(&self.base, made.0));
                    (made, listener)
                });
                (self.shared).set_bound(self.index, wanted.map(|made| made.0));
            }
            let Self {
                bound, control: rx, ..
            } = self;
            let accepted = match bound {
                Some((_, listener)) => tokio::select! {
                    accepted = listener.accept() => Some(accepted),
                    changed = rx.changed() => changed.is_err().then_some(Ok(None)),
                },
                None => rx.changed().await.is_err().then_some(Ok(None)),
            };
            match accepted {
                Some(Ok(Some(Accepted { tx, rx, peer }))) => {
                    let rx = Cut {
                        inner: rx,
                        control: self.shared.control.subscribe(),
                        drops: control.drops,
                    };
                    return Ok(Some(Accepted { tx, rx, peer }));
                }
                Some(other) => return other.map(|_| None),
                None => (),
            }
        }
    }
}

/// A connection's receiving half that fails once a knob ends the connection: dropped, the TV
/// unreachable, or the fake dropped.
pub(crate) struct Cut<R> {
    inner: R,
    control: watch::Receiver<Control>,
    /// [Control::drops] when the connection came.
    drops: u32,
}

impl<R: MessageRx> MessageRx for Cut<R> {
    /// `None`: a knob ended the connection.
    type Error = Option<R::Error>;

    async fn wait_message(&mut self) -> Result<(), Option<R::Error>> {
        let drops = self.drops;
        tokio::select! {
            received = self.inner.wait_message() => received.map_err(Some),
            _ = self.control.wait_for(|c| c.stop || !c.reachable || c.drops != drops) => Err(None),
        }
    }

    fn message(&self) -> Option<(u8, &[u8])> {
        self.inner.message()
    }

    fn consume(&mut self) {
        self.inner.consume();
    }

    async fn wait_connected(&mut self) {
        self.inner.wait_connected().await;
    }

    fn reset(&mut self) {
        self.inner.reset();
    }

    fn max_message_len(&self) -> usize {
        self.inner.max_message_len()
    }
}
