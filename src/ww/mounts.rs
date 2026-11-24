//! Mount slots `ext[NAME]`: the APIs other local programs serve on Unix sockets, forwarded by
//! `wire_weaver_server::mount` on the compositor's loop.
//!
//! Each `mount NAME unix:PATH` line of ww.kdl is one element of `ext`, its key the name. An
//! element keeps its index while its line stays, so a client's view of it (a
//! `wire_weaver_client::MountView`) keeps following it. Whether its program is up is the
//! element's `presence`: a program that restarts turns it `Absent`, then `Up` again once the slot's
//! control link is back (tried every [RETRY_AFTER]). Requests into it meanwhile fail at once with
//! `Unavailable`; ww_niri's own API and the screen are untouched.
//!
//! Each client connection gets a link of its own to each program it uses (a [Forwarder]), so
//! sequence numbers pass through unchanged and one slow client never holds up another.

use std::cell::RefCell;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use calloop::futures::Scheduler;
use calloop::LoopHandle;
use futures_util::io::{AsyncRead, AsyncWrite};
use wire_weaver::prelude::UNib32;
use wire_weaver::ww_api::mount::MountPresenceOwned;
use wire_weaver_server::mount::{Connect, Forwarder, MountOptions, MountRoutes, Mounts};
use ww_calloop::{LoopClock, UnixConnector};
use ww_client_server::ResourceIndex;
use ww_device::Clock;

use super::config::MountConfig;

/// Id of `ext` in the root of `ww_niri_api::WwNiri`.
pub const EXT_ID: u32 = 10;
/// Id of a slot's `api` below its element.
const API_ID: u32 = 1;
/// How soon a slot's control link tries again after its program went away or wasn't there.
pub const RETRY_AFTER: Duration = Duration::from_secs(1);
/// Slots handed out by one niri, counting those whose line went: indices are never reused.
pub const SLOTS_MAX: u32 = 1 << 20;

/// A byte stream to a mounted program, whatever loop it was made on.
pub trait Duplex: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> Duplex for T {}

/// Opens a stream to the socket at a path.
type Open = dyn Fn(&Path) -> io::Result<Box<dyn Duplex>>;

/// How a slot reaches its program: the socket of its `mount` line, opened on the loop.
pub struct Connector {
    socket: PathBuf,
    open: Rc<Open>,
}

impl Connect for Connector {
    type Stream = Box<dyn Duplex>;
    type Opening = std::future::Ready<io::Result<Self::Stream>>;

    fn connect(&self) -> Self::Opening {
        std::future::ready((self.open)(&self.socket))
    }
}

/// Opens Unix sockets as streams the loop of `handle` watches.
fn unix_opener<D: 'static>(handle: &LoopHandle<'static, D>) -> Rc<Open> {
    let handle = handle.clone();
    Rc::new(move |path: &Path| {
        let opening = UnixConnector::new(&handle, path).connect();
        let stream = opening.into_inner()?;
        Ok(Box::new(stream) as Box<dyn Duplex>)
    })
}

/// The loop's clock, whatever the loop's data: a forwarder's timers on the same clock as its
/// connection's server.
#[derive(Clone)]
pub struct MountClock(Rc<dyn DynClock>);

trait DynClock {
    fn now(&self) -> ww_device::Instant;
    fn wait_until(&self, at: ww_device::Instant) -> Pin<Box<dyn Future<Output = ()> + '_>>;
}

impl<D: 'static> DynClock for LoopClock<'static, D> {
    fn now(&self) -> ww_device::Instant {
        Clock::now(self)
    }

    fn wait_until(&self, at: ww_device::Instant) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(Clock::wait_until(self, at))
    }
}

impl MountClock {
    pub fn new<D: 'static>(clock: LoopClock<'static, D>) -> Self {
        Self(Rc::new(clock))
    }
}

impl Clock for MountClock {
    fn now(&self) -> ww_device::Instant {
        self.0.now()
    }

    fn wait_until(&self, at: ww_device::Instant) -> impl Future<Output = ()> {
        self.0.wait_until(at)
    }
}

/// One element of `ext`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Slot {
    index: u32,
    name: String,
    socket: PathBuf,
}

/// The `ext` slots and their mount table, shared by every connection.
pub struct ExtMounts {
    mounts: Mounts<Connector>,
    open: Rc<Open>,
    slots: RefCell<Vec<Slot>>,
    /// The index the next new name gets.
    next: std::cell::Cell<u32>,
}

impl ExtMounts {
    /// An empty table; its control links run on `scheduler` from now on.
    pub fn new<D: 'static>(handle: &LoopHandle<'static, D>, scheduler: &Scheduler<()>) -> Rc<Self> {
        let ext = Self::with_opener(unix_opener(handle));
        let (mounts, clock) = (ext.mounts.clone(), LoopClock::new(handle.clone()));
        if let Err(err) = scheduler.schedule(async move { mounts.run(clock).await }) {
            warn!("ww: error scheduling the mount table: {err:?}");
        }
        ext
    }

    fn with_opener(open: Rc<Open>) -> Rc<Self> {
        Rc::new(Self {
            mounts: Mounts::new(Arc::new(own_api())),
            open,
            slots: RefCell::new(Vec::new()),
            next: std::cell::Cell::new(0),
        })
    }

    /// Mounts exactly the `mount` lines of ww.kdl: a new name gets a new index, a line gone
    /// unmounts its slot, a changed socket mounts it again at the same index.
    pub fn sync(&self, lines: &[MountConfig]) {
        let mut slots = self.slots.borrow_mut();
        slots.retain(|slot| {
            let keep = lines.iter().any(|line| line.name == slot.name);
            if !keep {
                info!("ww: unmounting ext[{:?}]", slot.name);
                self.mounts.unmount(&api_path(slot.index));
            }
            keep
        });
        for line in lines {
            let index = match slots.iter_mut().find(|slot| slot.name == line.name) {
                Some(slot) if slot.socket == line.socket => continue,
                Some(slot) => {
                    slot.socket = line.socket.clone();
                    slot.index
                }
                None => {
                    let index = self.next.get();
                    if index >= SLOTS_MAX {
                        warn!(
                            "ww: not mounting ext[{:?}]: {SLOTS_MAX} slots handed out",
                            line.name
                        );
                        continue;
                    }
                    self.next.set(index + 1);
                    slots.push(Slot {
                        index,
                        name: line.name.clone(),
                        socket: line.socket.clone(),
                    });
                    index
                }
            };
            info!("ww: mounting {:?} at ext[{:?}]", line.socket, line.name);
            let connector = Connector {
                socket: line.socket.clone(),
                open: self.open.clone(),
            };
            let options = MountOptions {
                retry_after: RETRY_AFTER,
                ..MountOptions::default()
            };
            self.mounts.mount(&api_path(index), connector, options);
        }
    }

    /// The table, for each connection's [Forwarder].
    pub fn mounts(&self) -> &Mounts<Connector> {
        &self.mounts
    }

    /// Global trait paths into the mounted APIs, for the server.
    pub fn routes(&self) -> MountRoutes {
        self.mounts.routes()
    }

    /// The indices of the slots, in the order of their lines.
    pub fn indices(&self) -> Vec<UNib32> {
        self.slots
            .borrow()
            .iter()
            .map(|slot| UNib32(slot.index))
            .collect()
    }

    pub fn index_of(&self, name: &str) -> Option<u32> {
        self.slots
            .borrow()
            .iter()
            .find(|slot| slot.name == name)
            .map(|slot| slot.index)
    }

    pub fn name_of(&self, index: u32) -> Option<String> {
        self.slots
            .borrow()
            .iter()
            .find(|slot| slot.index == index)
            .map(|slot| slot.name.clone())
    }

    /// The names, in the order of their lines: for the DNS-SD record.
    pub fn names(&self) -> Vec<String> {
        self.slots
            .borrow()
            .iter()
            .map(|slot| slot.name.clone())
            .collect()
    }

    pub fn presence(&self, index: u32) -> MountPresenceOwned {
        self.mounts.presence(&api_path(index))
    }

    /// A forwarder for a new connection.
    pub fn forwarder(
        &self,
        connection: &wire_weaver_server::Connection,
        clock: MountClock,
    ) -> Forwarder<Connector, MountClock> {
        Forwarder::new(&self.mounts, connection, clock)
    }
}

/// The path of `ext[index].api`.
pub fn api_path(index: u32) -> Vec<ResourceIndex> {
    [EXT_ID, index, API_ID]
        .into_iter()
        .map(|id| ResourceIndex::new(UNib32(id)))
        .collect()
}

/// The `ext` index a path of the mount table names; `None` for another path.
pub fn index_of_path(path: &[ResourceIndex]) -> Option<u32> {
    match path {
        [ext, index, api] if ext.get().0 == EXT_ID && api.get().0 == API_ID => Some(index.get().0),
        _ => None,
    }
}

/// ww_niri's own API, whose traits come before the mounted ones' for global paths.
fn own_api() -> ww_self::ApiBundleOwned {
    use wire_weaver::shrink_wrap::DeserializeShrinkWrapOwned;

    let bytes = ww_self::compressed::bundle_bytes(&super::server::api_impl::WW_SELF_BYTES)
        .expect("the generated introspection data decompresses");
    ww_self::ApiBundleOwned::from_ww_bytes_owned(&bytes)
        .expect("the generated introspection data decodes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(name: &str, socket: &str) -> MountConfig {
        MountConfig {
            name: name.to_owned(),
            socket: PathBuf::from(socket),
        }
    }

    fn refused() -> Rc<Open> {
        Rc::new(|_: &Path| Err(io::Error::from(io::ErrorKind::NotFound)))
    }

    #[test]
    fn indices_stay_with_their_names() {
        let ext = ExtMounts::with_opener(refused());
        ext.sync(&[line("hub", "/run/hub.sock"), line("cast", "/run/cast.sock")]);
        assert_eq!(ext.indices(), [UNib32(0), UNib32(1)]);
        assert_eq!(ext.index_of("cast"), Some(1));
        assert_eq!(ext.mounts().slots(), [api_path(0), api_path(1)]);

        // hub's socket moves, cast goes, tv comes: hub keeps 0, tv gets a new index
        ext.sync(&[line("tv", "/run/tv.sock"), line("hub", "/run/hub2.sock")]);
        assert_eq!(ext.names(), ["hub", "tv"]);
        assert_eq!(ext.index_of("hub"), Some(0));
        assert_eq!(ext.index_of("tv"), Some(2));
        assert_eq!(ext.name_of(1), None);
        assert_eq!(ext.mounts().slots(), [api_path(0), api_path(2)]);

        // cast is back: a new index, never its old one
        ext.sync(&[line("cast", "/run/cast.sock")]);
        assert_eq!(ext.indices(), [UNib32(3)]);
        assert_eq!(ext.mounts().slots(), [api_path(3)]);
        assert!(
            matches!(ext.presence(3), MountPresenceOwned::Absent),
            "nothing is up"
        );
    }

    #[test]
    fn paths_of_the_table() {
        assert_eq!(index_of_path(&api_path(7)), Some(7));
        assert_eq!(index_of_path(&api_path(7)[..2]), None);
    }
}
