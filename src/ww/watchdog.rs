//! systemd's watchdog: `WATCHDOG=1` from the compositor's own calloop loop.
//!
//! A unit with `WatchdogSec=` gives niri `WATCHDOG_USEC`; niri then beats every half of it from a
//! timer on the main loop. The timer fires only while the loop dispatches, so a stuck loop (a
//! hung frame, a blocking call, a deadlock) stops the beats and systemd kills and restarts niri.
//! Without `NOTIFY_SOCKET` or `WATCHDOG_USEC` (outside systemd, in tests) nothing is sent.

use std::ffi::OsStr;
use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::Duration;

use calloop::timer::{TimeoutAction, Timer};
use calloop::{LoopHandle, RegistrationToken};

/// The datagram systemd reads as one heartbeat.
const BEAT: &[u8] = b"WATCHDOG=1\n";

/// The beat interval is the watchdog's timeout divided by this, as `sd_watchdog_enabled(3)`
/// advises.
const BEATS_PER_WATCHDOG: u32 = 2;

/// A socket to systemd's notify address that sends heartbeats.
#[derive(Debug)]
pub struct Heartbeat {
    socket: UnixDatagram,
}

impl Heartbeat {
    /// Connects to `path`, a socket path or `@name` for an abstract address, as in `NOTIFY_SOCKET`.
    ///
    /// # Errors
    ///
    /// The socket cannot be made or connected.
    pub fn connect(path: &OsStr) -> io::Result<Self> {
        let bytes = path.as_bytes();
        let addr = match bytes.strip_prefix(b"@") {
            Some(name) => SocketAddr::from_abstract_name(name)?,
            None => SocketAddr::from_pathname(path)?,
        };
        let socket = UnixDatagram::unbound()?;
        socket.connect_addr(&addr)?;
        socket.set_nonblocking(true)?;
        Ok(Self { socket })
    }

    /// Sends one `WATCHDOG=1`.
    ///
    /// # Errors
    ///
    /// The datagram was not sent whole.
    pub fn beat(&self) -> io::Result<()> {
        let sent = self.socket.send(BEAT)?;
        if sent == BEAT.len() {
            Ok(())
        } else {
            Err(io::ErrorKind::WriteZero.into())
        }
    }
}

/// The heartbeat systemd asked for: its socket and the beat interval, or `None` without a watchdog.
///
/// # Errors
///
/// `NOTIFY_SOCKET` is set but cannot be connected.
pub fn from_env() -> io::Result<Option<(Heartbeat, Duration)>> {
    let Some(timeout) = sd_notify::watchdog_enabled() else {
        return Ok(None);
    };
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(None);
    };
    Ok(Some((Heartbeat::connect(&path)?, interval(timeout))))
}

/// How often to beat for a watchdog `timeout`.
fn interval(timeout: Duration) -> Duration {
    timeout / BEATS_PER_WATCHDOG
}

/// Beats every `every` from a timer on `handle`'s loop, the first beat on the first dispatch.
///
/// # Errors
///
/// The timer cannot be inserted.
pub fn insert<D>(
    handle: &LoopHandle<'static, D>,
    heartbeat: Heartbeat,
    every: Duration,
) -> calloop::Result<RegistrationToken> {
    handle
        .insert_source(Timer::immediate(), move |_, (), _| {
            if let Err(err) = heartbeat.beat() {
                warn!("error sending the watchdog's heartbeat: {err:?}");
            }
            TimeoutAction::ToDuration(every)
        })
        .map_err(|err| err.error)
}

/// Starts the heartbeat on niri's loop when systemd runs a watchdog; logs why it could not.
pub fn start<D>(handle: &LoopHandle<'static, D>) {
    match from_env() {
        Ok(Some((heartbeat, every))) => match insert(handle, heartbeat, every) {
            Ok(_token) => info!("watchdog heartbeat every {every:?}"),
            Err(err) => warn!("error starting the watchdog's heartbeat: {err:?}"),
        },
        Ok(None) => {}
        Err(err) => warn!("error connecting to NOTIFY_SOCKET for the watchdog: {err:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use calloop::EventLoop;

    use super::*;

    /// A fake `NOTIFY_SOCKET` on an abstract address of its own: its `@name` and the bound socket.
    fn fake_notify_socket() -> (String, UnixDatagram) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "ww_niri-watchdog-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let socket = UnixDatagram::bind_addr(&addr).unwrap();
        socket.set_nonblocking(true).unwrap();
        (format!("@{name}"), socket)
    }

    /// Every datagram waiting on `socket`.
    fn received(socket: &UnixDatagram) -> Vec<Vec<u8>> {
        let mut all = Vec::new();
        let mut buf = [0u8; 64];
        while let Ok(n) = socket.recv(&mut buf) {
            all.push(buf[..n].to_vec());
        }
        all
    }

    #[test]
    fn interval_is_half_the_timeout() {
        assert_eq!(interval(Duration::from_secs(30)), Duration::from_secs(15));
    }

    #[test]
    fn heartbeat_reaches_the_notify_socket() {
        let (path, socket) = fake_notify_socket();
        let mut event_loop = EventLoop::<()>::try_new().unwrap();
        let heartbeat = Heartbeat::connect(OsStr::new(&path)).unwrap();
        insert(&event_loop.handle(), heartbeat, Duration::from_secs(3600)).unwrap();

        event_loop.dispatch(Some(Duration::ZERO), &mut ()).unwrap();

        assert_eq!(received(&socket), vec![BEAT.to_vec()]);
    }

    #[test]
    fn heartbeat_stops_while_the_loop_is_blocked() {
        let (path, socket) = fake_notify_socket();
        let mut event_loop = EventLoop::<()>::try_new().unwrap();
        let heartbeat = Heartbeat::connect(OsStr::new(&path)).unwrap();
        let every = Duration::from_millis(1);
        insert(&event_loop.handle(), heartbeat, every).unwrap();
        event_loop.dispatch(Some(Duration::ZERO), &mut ()).unwrap();
        assert_eq!(received(&socket).len(), 1);

        // The loop is stuck: many intervals pass without a dispatch.
        std::thread::sleep(every * 20);
        assert!(
            received(&socket).is_empty(),
            "a beat came without a dispatch"
        );

        // The loop runs again: one beat, not one per missed interval.
        event_loop.dispatch(Some(Duration::ZERO), &mut ()).unwrap();
        assert_eq!(received(&socket), vec![BEAT.to_vec()]);
    }
}
