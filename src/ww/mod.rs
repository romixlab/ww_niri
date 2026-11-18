//! The WireWeaver control API server (`ww_niri_api` over WebSocket at `ws://ADDR/ww`).
//!
//! Everything runs on the compositor's calloop loop, like niri's IPC: the listener is a loop
//! source, each connection a task on the loop's executor. `ww.kdl` says where to listen; without
//! it nothing listens. The file is checked for changes every second and applied when it parses.
//! Each `listen` address that can't be bound yet is tried again at each check, and `lan:PORT` and
//! `tailnet:PORT` are resolved again: a changed address closes its listener and binds the new
//! one, while open connections stay until they end.

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use calloop::futures::Scheduler;
use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};

use crate::niri::State;

mod audio;
mod bidi;
mod browsers;
mod clock;
mod config;
mod connection;
mod host;
mod hub;
pub(crate) mod input;
mod interfaces;
mod players;
mod server;
mod socket;
#[cfg(test)]
mod tests;
mod uinput;

pub use audio::Audio;
pub use browsers::Browsers;
use config::Listen;
pub use config::WwConfig;
pub use host::WwHost;
use interfaces::Interfaces;
pub use players::Players;

const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub struct WwServer {
    path: Option<PathBuf>,
    props: Option<config::Props>,
    config: WwConfig,
    listeners: Listeners<RegistrationToken>,
    services: Rc<Services>,
    /// The keyboard and pointer of `input`, made on first use.
    pub uinput: Option<uinput::Uinput>,
    /// The last pointer motion injected, and where niri had the pointer then: until niri has
    /// read it, relative moves start from there.
    pub pointer_sent: Option<input::Sent>,
    /// The pointer and what is under it as observers last heard.
    pub pointer_seen: Option<input::Seen>,
}

/// What every connection shares: the ww side's events, the browser slots, the players and the
/// audio server.
pub struct Services {
    pub hub: Rc<hub::Hub>,
    pub browsers: Rc<Browsers>,
    pub players: Rc<Players>,
    pub audio: Rc<Audio>,
    /// Timeouts for the handlers, as injected drags take steps.
    pub io: Rc<browsers::LoopIo>,
}

impl Services {
    pub fn new<D: WwHost>(
        handle: &LoopHandle<'static, D>,
        scheduler: &Scheduler<()>,
        settings: browsers::Settings,
        count: u8,
    ) -> Rc<Self> {
        let hub = Rc::new(hub::Hub::default());
        let run = host::runner(handle.clone());
        let browsers = Browsers::new(
            settings,
            run.clone(),
            browsers::LoopIo::new(handle.clone()),
            scheduler.clone(),
            hub.clone(),
            count,
        );
        browsers.follow_windows();
        let players = Players::new(hub.clone(), run, browsers.clone());
        Rc::new(Self {
            hub,
            browsers,
            players,
            audio: Audio::new(),
            io: Rc::new(browsers::LoopIo::new(handle.clone())),
        })
    }
}

impl WwServer {
    pub fn start(event_loop: &LoopHandle<'static, State>, scheduler: &Scheduler<()>) -> Self {
        // Tests never read the user's ww.kdl.
        let path = if cfg!(test) {
            None
        } else {
            std::env::var_os("WW_NIRI_CONFIG")
                .map(PathBuf::from)
                .or_else(config::default_path)
        };

        let config = WwConfig::default();
        let services = Services::new(
            event_loop,
            scheduler,
            browsers::Settings::default(),
            config.browsers,
        );
        if !cfg!(test) {
            services
                .players
                .follow(scheduler, zbus::Connection::session());
            match audio::Target::from_env() {
                Some(target) => services.audio.follow(
                    scheduler,
                    browsers::LoopIo::new(event_loop.clone()),
                    target,
                ),
                None => warn!("ww: no audio server socket found, volume is not available"),
            }
        }
        let mut ww = Self {
            path,
            props: None,
            config,
            listeners: Listeners::default(),
            services,
            uinput: None,
            pointer_sent: None,
            pointer_seen: None,
        };
        ww.reload(event_loop);

        event_loop
            .insert_source(Timer::from_duration(CONFIG_POLL_INTERVAL), |_, _, state| {
                let event_loop = state.niri.event_loop.clone();
                state.niri.ww.check_config(&event_loop);
                TimeoutAction::ToDuration(CONFIG_POLL_INTERVAL)
            })
            .unwrap();

        ww
    }

    pub fn config(&self) -> &WwConfig {
        &self.config
    }

    pub fn services(&self) -> Rc<Services> {
        self.services.clone()
    }

    fn check_config(&mut self, event_loop: &LoopHandle<'static, State>) {
        let Some(path) = &self.path else { return };
        if config::Props::of(path) != self.props {
            self.reload(event_loop);
            return;
        }
        // At boot the tailnet or LAN address may not exist yet, and DHCP may change the LAN
        // address: keep following, quietly.
        let interfaces = match interfaces_for(&self.config.listen) {
            Ok(interfaces) => interfaces,
            Err(err) => {
                debug!("ww: keeping the listeners: {err:?}");
                return;
            }
        };
        let (wanted, _) = resolve(&self.config.listen, &interfaces);
        if !self.listeners.matches(&wanted) {
            self.sync_listeners(event_loop, &wanted);
        }
    }

    /// Binds `wanted` and drops the rest; answers the addresses that failed.
    fn sync_listeners(
        &mut self,
        event_loop: &LoopHandle<'static, State>,
        wanted: &[SocketAddr],
    ) -> Vec<(SocketAddr, anyhow::Error)> {
        self.listeners.sync(
            wanted,
            |addr| listen(event_loop, addr),
            // Open connections stay until they end; only the listener goes.
            |_, token| event_loop.remove(token),
        )
    }

    fn reload(&mut self, event_loop: &LoopHandle<'static, State>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        self.props = config::Props::of(&path);

        let config = match config::load(&path) {
            Ok(config) => config,
            Err(err) => {
                // Keep what runs: a typo in ww.kdl doesn't take the API down.
                warn!("ww: error loading {path:?}, keeping the old settings: {err:?}");
                return;
            }
        };
        let interfaces = interfaces_for(&config.listen).unwrap_or_else(|err| {
            warn!("ww: lan: and tailnet: have no address until this works: {err:?}");
            Interfaces::default()
        });
        let (wanted, missing) = resolve(&config.listen, &interfaces);
        if config == self.config && self.listeners.matches(&wanted) {
            return;
        }
        info!("ww: settings from {path:?}: {config:?}");

        self.services.browsers.set_count(config.browsers);
        self.config = config;
        for line in missing {
            warn!("ww: listen {line}: the host has no such address yet, trying again");
        }
        for (addr, err) in self.sync_listeners(event_loop, &wanted) {
            warn!("ww: error listening on {addr}, trying again: {err:?}");
        }
    }
}

/// The host's interfaces when a line of `listen` needs them; else none, without reading them.
fn interfaces_for(listen: &[Listen]) -> anyhow::Result<Interfaces> {
    if listen.iter().any(|line| line.is_symbolic()) {
        Interfaces::read()
    } else {
        Ok(Interfaces::default())
    }
}

/// The addresses `listen` names on `interfaces`, without repeats, and the lines that name none.
fn resolve(listen: &[Listen], interfaces: &Interfaces) -> (Vec<SocketAddr>, Vec<Listen>) {
    let mut wanted = Vec::new();
    let mut missing = Vec::new();
    for &line in listen {
        match line.resolve(interfaces) {
            Some(addr) if !wanted.contains(&addr) => wanted.push(addr),
            Some(_) => (),
            None => missing.push(line),
        }
    }
    (wanted, missing)
}

/// The listeners, one per address of `ww.kdl`; `T` is what a bound one keeps.
struct Listeners<T> {
    bound: Vec<(SocketAddr, T)>,
}

impl<T> Default for Listeners<T> {
    fn default() -> Self {
        Self { bound: Vec::new() }
    }
}

impl<T> Listeners<T> {
    /// Whether exactly the addresses of `wanted`, which has no repeats, are bound.
    fn matches(&self, wanted: &[SocketAddr]) -> bool {
        self.bound.len() == wanted.len()
            && wanted
                .iter()
                .all(|addr| self.bound.iter().any(|(a, _)| a == addr))
    }

    /// Unbinds the addresses not in `wanted` and binds the missing ones; answers those that
    /// failed, to try again later.
    fn sync<E>(
        &mut self,
        wanted: &[SocketAddr],
        mut bind: impl FnMut(SocketAddr) -> Result<T, E>,
        mut unbind: impl FnMut(SocketAddr, T),
    ) -> Vec<(SocketAddr, E)> {
        let (keep, drop): (Vec<_>, Vec<_>) = std::mem::take(&mut self.bound)
            .into_iter()
            .partition(|(addr, _)| wanted.contains(addr));
        self.bound = keep;
        for (addr, bound) in drop {
            unbind(addr, bound);
            info!("ww: stopped listening on {addr}");
        }
        let mut failed = Vec::new();
        for &addr in wanted {
            if self.bound.iter().any(|(a, _)| *a == addr) {
                continue;
            }
            match bind(addr) {
                Ok(bound) => {
                    info!("ww: listening on ws://{addr}/ww");
                    self.bound.push((addr, bound));
                }
                Err(err) => failed.push((addr, err)),
            }
        }
        failed
    }
}

fn listen(
    event_loop: &LoopHandle<'static, State>,
    addr: SocketAddr,
) -> anyhow::Result<RegistrationToken> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;

    let source = Generic::new(listener, Interest::READ, Mode::Level);
    let token = event_loop
        .insert_source(source, |_, listener, state| {
            match listener.accept() {
                Ok((stream, peer)) => connection::spawn(state, stream, peer),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => (),
                Err(err) => warn!("ww: error accepting a connection: {err:?}"),
            }
            Ok(PostAction::Continue)
        })
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(token)
}

#[cfg(test)]
mod listener_tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::io;

    use super::*;

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn binds_each_address_and_retries_the_missing_one() {
        let tailnet = addr("100.98.201.15:7590");
        let lan = addr("192.168.1.95:7590");
        let wanted = [tailnet, lan];
        // The addresses the host has; the LAN one comes up later.
        let up = RefCell::new(BTreeSet::from([tailnet]));
        let unbound = RefCell::new(Vec::new());
        let bind = |a: SocketAddr| {
            if up.borrow().contains(&a) {
                Ok(a.to_string())
            } else {
                Err(io::Error::from(io::ErrorKind::AddrNotAvailable))
            }
        };
        let unbind = |a, _| unbound.borrow_mut().push(a);

        let mut listeners = Listeners::default();
        let failed = listeners.sync(&wanted, bind, unbind);
        assert_eq!(failed.iter().map(|(a, _)| *a).collect::<Vec<_>>(), [lan]);
        assert!(!listeners.matches(&wanted));

        // Still down: tried again, nothing bound twice.
        assert_eq!(listeners.sync(&wanted, bind, unbind).len(), 1);
        assert_eq!(listeners.bound.len(), 1);

        up.borrow_mut().insert(lan);
        assert!(listeners.sync(&wanted, bind, unbind).is_empty());
        assert!(listeners.matches(&wanted));

        // The tailnet line goes from ww.kdl: only its listener goes.
        assert!(listeners.sync(&[lan], bind, unbind).is_empty());
        assert_eq!(*unbound.borrow(), [tailnet]);
        assert_eq!(listeners.bound, [(lan, lan.to_string())]);
    }

    #[test]
    fn follows_a_changed_lan_address() {
        let listen = [Listen::Lan(7590), Listen::Tailnet(7590)];
        let host = |lan: &str| Interfaces {
            addrs: vec![
                ("tailscale0".to_owned(), "100.98.201.15".parse().unwrap()),
                ("wlan0".to_owned(), lan.parse().unwrap()),
            ],
            default_routes: vec!["wlan0".to_owned()],
        };
        let unbound = RefCell::new(Vec::new());
        let bind = |a: SocketAddr| Ok::<_, io::Error>(a.to_string());
        let unbind = |a, _| unbound.borrow_mut().push(a);

        let mut listeners = Listeners::default();
        let (wanted, missing) = resolve(&listen, &host("192.168.1.95"));
        assert!(missing.is_empty());
        assert!(listeners.sync(&wanted, bind, unbind).is_empty());
        assert!(listeners.matches(&wanted));

        // DHCP hands out another address: only the LAN listener moves.
        let (wanted, _) = resolve(&listen, &host("192.168.1.96"));
        assert!(!listeners.matches(&wanted));
        assert!(listeners.sync(&wanted, bind, unbind).is_empty());
        assert_eq!(*unbound.borrow(), [addr("192.168.1.95:7590")]);
        assert_eq!(
            listeners.bound.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
            [addr("100.98.201.15:7590"), addr("192.168.1.96:7590")]
        );

        // The link goes down: its listener goes, and comes back with the address.
        let (wanted, missing) = resolve(&listen, &Interfaces::default());
        assert!(wanted.is_empty());
        assert_eq!(missing, listen);
        assert!(!listeners.matches(&wanted));
        listeners.sync(&wanted, bind, unbind);
        assert!(listeners.bound.is_empty());
    }

    #[test]
    fn resolve_drops_repeats() {
        let listen = [
            Listen::Lan(7590),
            Listen::Addr(addr("192.168.1.95:7590")),
            Listen::Tailnet(7590),
        ];
        let host = Interfaces {
            addrs: vec![("eth0".to_owned(), "192.168.1.95".parse().unwrap())],
            default_routes: vec!["eth0".to_owned()],
        };
        let (wanted, missing) = resolve(&listen, &host);
        assert_eq!(wanted, [addr("192.168.1.95:7590")]);
        assert_eq!(missing, [Listen::Tailnet(7590)]);
    }

    #[test]
    fn real_sockets_bind_what_exists() {
        // 192.0.2.1 (TEST-NET-1) is on no interface, like a LAN address before the link is up.
        let wanted = [addr("127.0.0.1:0"), addr("192.0.2.1:7590")];
        let mut listeners = Listeners::default();
        let failed = listeners.sync(&wanted, std::net::TcpListener::bind, |_, _| ());
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, wanted[1]);
        assert_eq!(failed[0].1.kind(), io::ErrorKind::AddrNotAvailable);
        assert_eq!(listeners.bound.len(), 1);
        assert_eq!(listeners.bound[0].0, wanted[0]);
    }
}
