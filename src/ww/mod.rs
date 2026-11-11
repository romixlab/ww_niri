//! The WireWeaver control API server (`ww_niri_api` over WebSocket at `ws://ADDR/ww`).
//!
//! Everything runs on the compositor's calloop loop, like niri's IPC: the listener is a loop
//! source, each connection a task on the loop's executor. `ww.kdl` says where to listen; without
//! it nothing listens. The file is checked for changes every second and applied when it parses.

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
mod players;
mod server;
mod socket;
#[cfg(test)]
mod tests;

pub use audio::Audio;
pub use browsers::Browsers;
pub use config::WwConfig;
pub use host::WwHost;
pub use players::Players;

const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub struct WwServer {
    path: Option<PathBuf>,
    props: Option<config::Props>,
    config: WwConfig,
    listener: Option<(SocketAddr, RegistrationToken)>,
    services: Rc<Services>,
}

/// What every connection shares: the ww side's events, the browser slots, the players and the
/// audio server.
pub struct Services {
    pub hub: Rc<hub::Hub>,
    pub browsers: Rc<Browsers>,
    pub players: Rc<Players>,
    pub audio: Rc<Audio>,
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
            listener: None,
            services,
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
        } else if self.listener.is_none() {
            // At boot the tailnet address may not exist yet: keep trying, quietly.
            if let Some(addr) = self.config.listen {
                if let Ok(token) = listen(event_loop, addr) {
                    info!("ww: listening on ws://{addr}/ww");
                    self.listener = Some((addr, token));
                }
            }
        }
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
        if config == self.config && (config.listen.is_none() || self.listener.is_some()) {
            return;
        }
        info!("ww: settings from {path:?}: {config:?}");

        if self.listener.map(|(addr, _)| addr) != config.listen {
            if let Some((addr, token)) = self.listener.take() {
                // Open connections stay until they end; only the listener goes.
                event_loop.remove(token);
                info!("ww: stopped listening on {addr}");
            }
            if let Some(addr) = config.listen {
                match listen(event_loop, addr) {
                    Ok(token) => {
                        info!("ww: listening on ws://{addr}/ww");
                        self.listener = Some((addr, token));
                    }
                    Err(err) => warn!("ww: error listening on {addr}, trying again: {err:?}"),
                }
            }
        }

        self.services.browsers.set_count(config.browsers);
        self.config = config;
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
