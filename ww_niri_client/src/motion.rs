//! [MotionSender]: pointer motion over UDP, beside a [Tv](crate::Tv) on WebSocket.
//!
//! Over a lossy Wi-Fi, TCP holds every later move until a lost segment is resent, and the
//! pointer stutters. Over UDP a lost datagram is skipped. Each datagram carries the sum of all
//! moves so far, so the next one brings the distance a lost one had.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use tokio::sync::{Notify, watch};
use wire_weaver_client::{ConnectionState, KeepTrying};
use ww_niri_api::{MotionSeq, PointerMotion, Px};

use crate::api::NiriClient;
use crate::error::StartError;
use crate::runtime;

/// Where a [MotionSender] sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MotionAddress {
    /// ww_niri over UDP, at the address and port of its WebSocket: `192.168.1.95:7590`.
    Udp(String),
    /// A server in this process: `ww_niri_fake`'s `FakeTv::motion_path`.
    InProcess(String),
}

impl MotionAddress {
    /// The UDP address beside a WebSocket URL: `ws://192.168.1.95:7590/ww` gives
    /// `192.168.1.95:7590`. `None` for a URL that is not `ws://` or names no port.
    pub fn beside(websocket_url: &str) -> Option<Self> {
        let rest = websocket_url.strip_prefix("ws://")?;
        let authority = rest.split('/').next()?;
        let (host, port) = authority.rsplit_once(':')?;
        if host.is_empty() || port.parse::<u16>().is_err() {
            return None;
        }
        Some(Self::Udp(authority.to_owned()))
    }
}

/// The moves added up so far, wrapping, and a wake for the sending task.
#[derive(Default)]
struct Totals {
    dx: AtomicI32,
    dy: AtomicI32,
    changed: Notify,
}

impl Totals {
    fn now(&self) -> (i32, i32) {
        (
            self.dx.load(Ordering::Relaxed),
            self.dy.load(Ordering::Relaxed),
        )
    }
}

/// What every clone of a [MotionSender] shares; dropping the last one stops its task.
struct Inner {
    totals: Arc<Totals>,
    state: watch::Receiver<ConnectionState>,
    stop: watch::Sender<bool>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

/// Moves the TV's pointer over UDP: [MotionSender::move_by] never waits, and a lost datagram
/// loses no distance. Connects in the background and again whenever the link goes; moves made
/// while offline, or as the link comes up, are not sent.
///
/// Clicks, keys and everything else stay on the [Tv](crate::Tv)'s WebSocket link.
///
/// Cheap to clone and `Send`; every clone is the same sender.
#[derive(Clone)]
pub struct MotionSender {
    inner: Arc<Inner>,
}

impl MotionSender {
    /// Starts connecting and returns at once; `keep_trying` as in
    /// [Config::keep_trying](crate::Config::keep_trying).
    ///
    /// Runs on the caller's tokio runtime when there is one, else on a runtime thread of its own.
    ///
    /// # Errors
    ///
    /// When wire_weaver refuses the address, or the runtime thread can't be made.
    pub fn start(address: MotionAddress, keep_trying: KeepTrying) -> Result<Self, StartError> {
        runtime::start(
            |sender: &Self| sender.inner.stop.subscribe(),
            || Self::start_here(address, keep_trying),
        )
    }

    fn start_here(address: MotionAddress, keep_trying: KeepTrying) -> Result<Self, StartError> {
        let client = NiriClient::config(move |c| {
            match &address {
                MotionAddress::Udp(addr) => c.udp_addr(addr.clone()),
                MotionAddress::InProcess(path) => c.in_process_path(path.clone()),
            }
            .keep_trying(keep_trying)
        })
        .connect_keep_trying()?;
        let state = client
            .cmd
            .connection_state()
            .expect("a connection that keeps trying has a state");
        let totals = Arc::new(Totals::default());
        let (stop, _) = watch::channel(false);
        tokio::spawn(run(client, totals.clone(), state.clone(), stop.subscribe()));
        Ok(Self {
            inner: Arc::new(Inner {
                totals,
                state,
                stop,
            }),
        })
    }

    /// Moves the pointer by `dx`, `dy` logical pixels, as
    /// [PointerCalls::move_by](crate::PointerCalls::move_by) does. Never waits: the move is
    /// added to the totals, which go with the next datagram.
    pub fn move_by(&self, dx: i32, dy: i32) {
        let totals = &self.inner.totals;
        totals.dx.fetch_add(dx, Ordering::Relaxed);
        totals.dy.fetch_add(dy, Ordering::Relaxed);
        totals.changed.notify_one();
    }

    /// Whether the UDP link is up: an app may move the pointer over the [Tv](crate::Tv) while
    /// it is not.
    pub fn is_connected(&self) -> bool {
        matches!(
            *self.inner.state.borrow(),
            ConnectionState::Connected { .. }
        )
    }

    /// Every change of the UDP link's state.
    pub fn watch_state(&self) -> watch::Receiver<ConnectionState> {
        self.inner.state.clone()
    }
}

/// Sends the totals whenever they change, one datagram at a time: moves made while one is on
/// its way add up into the next. After each connect, the totals as they were when the link came
/// up go first: the TV takes a connection's first write as where its totals start.
async fn run(
    client: NiriClient,
    totals: Arc<Totals>,
    mut state: watch::Receiver<ConnectionState>,
    mut stop: watch::Receiver<bool>,
) {
    let mut seq = MotionSeq::new(0);
    let mut sink = None;
    // The totals when the link came up, the TV's baseline, until its first write goes.
    let mut base = None;
    // The totals the TV has: `None` until the first write of this connection.
    let mut sent = None;
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        let connected = matches!(
            *state.borrow_and_update(),
            ConnectionState::Connected { .. }
        );
        if !connected {
            (sink, base, sent) = (None, None, None);
        } else {
            let want = match sent {
                None => *base.get_or_insert_with(|| totals.now()),
                Some(_) => totals.now(),
            };
            if sent != Some(want) {
                if sink.is_none() {
                    sink = client.input().pointer().motion().await.ok();
                }
                if let Some(s) = &mut sink {
                    seq = seq.next();
                    let motion = PointerMotion {
                        seq,
                        dx_total: Px::new(want.0),
                        dy_total: Px::new(want.1),
                    };
                    if s.send(motion).await.is_ok() {
                        sent = Some(want);
                        // More may have come while it was sent.
                        continue;
                    }
                    // The link went: its state says so next.
                    sink = None;
                }
            }
        }
        tokio::select! {
            changed = stop.changed() => if changed.is_err() { return },
            changed = state.changed() => if changed.is_err() { return },
            () = totals.changed.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MotionAddress;

    #[test]
    fn udp_beside_a_websocket_url() {
        let udp = |a: &str| Some(MotionAddress::Udp(a.to_owned()));
        assert_eq!(
            MotionAddress::beside("ws://192.168.1.95:7590/ww"),
            udp("192.168.1.95:7590")
        );
        assert_eq!(
            MotionAddress::beside("ws://alma-2tv:7590/ww"),
            udp("alma-2tv:7590")
        );
        assert_eq!(
            MotionAddress::beside("ws://[::1]:7590/ww"),
            udp("[::1]:7590")
        );
        assert_eq!(MotionAddress::beside("ws://alma-2tv/ww"), None);
        assert_eq!(MotionAddress::beside("ws://[fe80::1]/ww"), None);
        assert_eq!(MotionAddress::beside("wss://alma-2tv:7590/ww"), None);
        assert_eq!(MotionAddress::beside("ws://:7590/ww"), None);
    }
}
