//! Every MPRIS player on the session bus, whoever started it.
//!
//! A task follows `NameOwnerChanged` for `org.mpris.MediaPlayer2.*` names: each new name gets a
//! [PlayerId] (the same again if that name comes back) and a `PlayerAppeared` event, a name that
//! goes gives `PlayerGone`. State and commands go to the player over the bus when asked.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::rc::Rc;

use calloop::futures::Scheduler;
use exact_units::Fixed;
use futures_util::StreamExt as _;
use ww_niri_api::{
    BrowserSlot, MediaErrorOwned, PlaybackStateOwned, PlaybackStatus, PlayerId, PlayerInfoOwned,
    PlayerSourceOwned,
};
use zbus::fdo::DBusProxy;
use zbus::names::BusName;
use zbus::zvariant::{ObjectPath, OwnedValue};
use zbus::Connection;

use super::host::{call, Run};
use super::hub::Hub;
use super::Browsers;

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

/// A command for a player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    /// Relative, in µs.
    Seek(i64),
    /// On the current track, in µs.
    SetPosition(u64),
}

impl Action {
    fn method(self) -> &'static str {
        match self {
            Action::Play => "Play",
            Action::Pause => "Pause",
            Action::PlayPause => "PlayPause",
            Action::Stop => "Stop",
            Action::Next => "Next",
            Action::Previous => "Previous",
            Action::Seek(_) => "Seek",
            Action::SetPosition(_) => "SetPosition",
        }
    }

    /// The MPRIS property that says whether the player offers it.
    fn capability(self) -> Option<&'static str> {
        match self {
            Action::Play => Some("CanPlay"),
            Action::Pause | Action::PlayPause => Some("CanPause"),
            Action::Next => Some("CanGoNext"),
            Action::Previous => Some("CanGoPrevious"),
            Action::Seek(_) | Action::SetPosition(_) => Some("CanSeek"),
            Action::Stop => None,
        }
    }
}

struct Live {
    name: String,
    /// The unique name that owns `name`, which signals come from.
    owner: Option<String>,
    identity: String,
    pid: Option<u32>,
}

pub struct Players {
    hub: Rc<Hub>,
    run: Run,
    browsers: Rc<Browsers>,
    conn: RefCell<Option<Connection>>,
    /// Every name seen, with its id: a name that comes back gets its old id.
    ids: RefCell<HashMap<String, PlayerId>>,
    next_id: Cell<u32>,
    live: RefCell<BTreeMap<u32, Live>>,
}

fn bus_error(err: zbus::Error) -> MediaErrorOwned {
    MediaErrorOwned::Bus(err.to_string())
}

impl Players {
    pub fn new(hub: Rc<Hub>, run: Run, browsers: Rc<Browsers>) -> Rc<Self> {
        Rc::new(Self {
            hub,
            run,
            browsers,
            conn: RefCell::new(None),
            ids: RefCell::new(HashMap::new()),
            next_id: Cell::new(0),
            live: RefCell::new(BTreeMap::new()),
        })
    }

    /// Follows the players on the bus that `bus` connects to, as a task on `scheduler`.
    pub fn follow(
        self: &Rc<Self>,
        scheduler: &Scheduler<()>,
        bus: impl Future<Output = zbus::Result<Connection>> + 'static,
    ) {
        let this = self.clone();
        let res = scheduler.schedule(async move {
            let res = async {
                let conn = bus.await?;
                this.conn.replace(Some(conn.clone()));
                this.watch(conn).await
            };
            if let Err(err) = res.await {
                warn!("ww: error following MPRIS players: {err}");
            }
        });
        if let Err(err) = res {
            warn!("ww: error scheduling the MPRIS task: {err:?}");
        }
    }

    async fn watch(&self, conn: Connection) -> zbus::Result<()> {
        let dbus = DBusProxy::new(&conn).await?;
        // Changes first, so none falls between the list and the stream.
        let changes = dbus.receive_name_owner_changed().await?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path(PATH)?
            .build();
        let properties = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;
        for name in dbus.list_names().await? {
            if name.starts_with(PREFIX) {
                self.appeared(&conn, &dbus, name.as_str()).await;
            }
        }

        enum Signal {
            Owner(zbus::fdo::NameOwnerChanged),
            Properties(zbus::Message),
        }
        let mut signals = futures_util::stream::select(
            changes.map(Signal::Owner),
            properties.filter_map(|msg| std::future::ready(msg.ok().map(Signal::Properties))),
        );
        while let Some(signal) = signals.next().await {
            let signal = match signal {
                Signal::Owner(signal) => signal,
                Signal::Properties(msg) => {
                    self.properties_changed(&msg).await;
                    continue;
                }
            };
            let Ok(args) = signal.args() else { continue };
            let name = args.name().as_str();
            if !name.starts_with(PREFIX) {
                continue;
            }
            if args.old_owner().is_some() {
                self.gone(name);
            }
            if args.new_owner().is_some() {
                self.appeared(&conn, &dbus, name).await;
            }
        }
        Ok(())
    }

    async fn appeared(&self, conn: &Connection, dbus: &DBusProxy<'_>, name: &str) {
        let pid = match BusName::try_from(name) {
            Ok(bus_name) => dbus.get_connection_unix_process_id(bus_name).await.ok(),
            Err(_) => None,
        };
        let owner = match BusName::try_from(name) {
            Ok(bus_name) => dbus
                .get_name_owner(bus_name)
                .await
                .ok()
                .map(|owner| owner.to_string()),
            Err(_) => None,
        };
        let identity = match proxy(conn, name, ROOT).await {
            Ok(root) => root
                .get_property::<String>("Identity")
                .await
                .unwrap_or_default(),
            Err(_) => String::new(),
        };

        let id = *self
            .ids
            .borrow_mut()
            .entry(name.to_owned())
            .or_insert_with(|| {
                let id = self.next_id.get();
                self.next_id.set(id + 1);
                PlayerId(id)
            });
        debug!(
            "ww: MPRIS player {} is {name} ({identity}, pid {pid:?})",
            id.0
        );
        let live = Live {
            name: name.to_owned(),
            owner,
            identity,
            pid,
        };
        let info = self.info(id, &live).await;
        self.live.borrow_mut().insert(id.0, live);
        self.hub.emit(ww_niri_api::EventOwned::PlayerAppeared(info));
    }

    /// A player's `PropertiesChanged`: a new status or track gives `PlaybackChanged`.
    async fn properties_changed(&self, msg: &zbus::Message) {
        let header = msg.header();
        let Some(sender) = header.sender() else {
            return;
        };
        let Ok((interface, changed, _)) =
            msg.body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
        else {
            return;
        };
        if interface != PLAYER
            || !["PlaybackStatus", "Metadata", "CanSeek", "CanPause", "Rate"]
                .iter()
                .any(|key| changed.contains_key(*key))
        {
            return;
        }
        let found = self
            .live
            .borrow()
            .iter()
            .find(|(_, p)| p.owner.as_deref() == Some(sender.as_str()))
            .map(|(id, p)| (PlayerId(*id), p.name.clone()));
        let Some((player, name)) = found else { return };
        match self.read_state(&name).await {
            Ok(state) => self
                .hub
                .emit(ww_niri_api::EventOwned::PlaybackChanged { player, state }),
            Err(err) => debug!("ww: error reading the state of {name}: {err:?}"),
        }
    }

    fn gone(&self, name: &str) {
        let Some(id) = self.ids.borrow().get(name).copied() else {
            return;
        };
        if self.live.borrow_mut().remove(&id.0).is_some() {
            debug!("ww: MPRIS player {} ({name}) is gone", id.0);
            self.hub.emit(ww_niri_api::EventOwned::PlayerGone(id));
        }
    }

    /// The ids of the players there now.
    pub fn ids(&self) -> Vec<u32> {
        self.live.borrow().keys().copied().collect()
    }

    /// The player of a process, as a browser slot's Firefox has.
    pub fn of_pid(&self, pid: u32) -> Option<PlayerId> {
        let live = self.live.borrow();
        let (id, _) = live.iter().find(|(_, p)| p.pid == Some(pid))?;
        Some(PlayerId(*id))
    }

    pub async fn list(&self) -> Vec<PlayerInfoOwned> {
        let players: Vec<_> = self
            .live
            .borrow()
            .iter()
            .map(|(id, p)| (PlayerId(*id), p.identity.clone(), p.pid))
            .collect();
        let mut infos = Vec::new();
        for (id, identity, pid) in players {
            infos.push(PlayerInfoOwned {
                id,
                identity,
                source: self.source(pid).await,
            });
        }
        infos
    }

    async fn info(&self, id: PlayerId, live: &Live) -> PlayerInfoOwned {
        PlayerInfoOwned {
            id,
            identity: live.identity.clone(),
            source: self.source(live.pid).await,
        }
    }

    pub async fn source_of(&self, id: u32) -> Option<PlayerSourceOwned> {
        let pid = self.live.borrow().get(&id)?.pid;
        Some(self.source(pid).await)
    }

    /// A browser slot's Firefox, else the window of the process or of its parent (apps whose
    /// player runs in a child process), else the process alone.
    async fn source(&self, pid: Option<u32>) -> PlayerSourceOwned {
        let Some(pid) = pid else {
            return PlayerSourceOwned::App { pid: None };
        };
        if let Some(BrowserSlot(slot)) = self.browsers.slot_of_pid(pid) {
            return PlayerSourceOwned::Browser(BrowserSlot(slot));
        }
        let pids = [Some(pid), parent(pid)];
        let window = call(&self.run, move |host| {
            host.windows()
                .into_iter()
                .find(|w| w.pid.is_some() && pids.contains(&w.pid))
        })
        .await
        .flatten();
        match window {
            Some(w) => PlayerSourceOwned::Window {
                id: w.id,
                app_id: w.app_id,
            },
            None => PlayerSourceOwned::App { pid: Some(pid) },
        }
    }

    fn name(&self, id: u32) -> Option<String> {
        self.live.borrow().get(&id).map(|p| p.name.clone())
    }

    fn conn(&self) -> Result<Connection, MediaErrorOwned> {
        self.conn
            .borrow()
            .clone()
            .ok_or_else(|| MediaErrorOwned::Bus("no session bus".to_owned()))
    }

    /// The player's state; `None` when there is no such player.
    pub async fn state(&self, id: u32) -> Option<Result<PlaybackStateOwned, MediaErrorOwned>> {
        let name = self.name(id)?;
        Some(self.read_state(&name).await)
    }

    async fn read_state(&self, name: &str) -> Result<PlaybackStateOwned, MediaErrorOwned> {
        let player = proxy(&self.conn()?, name, PLAYER)
            .await
            .map_err(bus_error)?;
        let status = match player
            .get_property::<String>("PlaybackStatus")
            .await
            .map_err(bus_error)?
            .as_str()
        {
            "Playing" => PlaybackStatus::Playing,
            "Paused" => PlaybackStatus::Paused,
            _ => PlaybackStatus::Stopped,
        };
        let metadata: HashMap<String, OwnedValue> =
            player.get_property("Metadata").await.unwrap_or_default();
        let text = |key: &str| -> Option<String> {
            let value = metadata.get(key)?;
            String::try_from(value.try_clone().ok()?).ok()
        };
        let artist = metadata
            .get("xesam:artist")
            .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
            .map(|artists| artists.join(", "));
        let length = metadata.get("mpris:length").and_then(|v| {
            i64::try_from(v)
                .ok()
                .or_else(|| u64::try_from(v).ok().map(|l| l as i64))
        });
        let position: i64 = player.get_property("Position").await.unwrap_or(0);
        let rate: f64 = player.get_property("Rate").await.unwrap_or(1.);
        let flag = |name: &'static str| {
            let player = &player;
            async move { player.get_property::<bool>(name).await.unwrap_or(false) }
        };

        Ok(PlaybackStateOwned {
            status,
            title: text("xesam:title"),
            artist,
            length: length.map(|l| Fixed::new(l.max(0) as u64)),
            position: Fixed::new(position.max(0) as u64),
            rate: Fixed::new((rate * 1000.).round().clamp(0., f64::from(u16::MAX)) as u16),
            can_seek: flag("CanSeek").await,
            can_pause: flag("CanPause").await,
        })
    }

    /// Runs `action` on the player; `None` when there is no such player.
    pub async fn act(&self, id: u32, action: Action) -> Option<Result<(), MediaErrorOwned>> {
        let name = self.name(id)?;
        Some(self.act_on(&name, action).await)
    }

    async fn act_on(&self, name: &str, action: Action) -> Result<(), MediaErrorOwned> {
        let player = proxy(&self.conn()?, name, PLAYER)
            .await
            .map_err(bus_error)?;
        if let Some(capability) = action.capability() {
            if !player
                .get_property::<bool>(capability)
                .await
                .unwrap_or(false)
            {
                return Err(MediaErrorOwned::NotSupported);
            }
        }
        let res = match action {
            Action::Seek(by) => player.call_method(action.method(), &(by,)).await,
            Action::SetPosition(at) => {
                // SetPosition names the track it is for.
                let metadata: HashMap<String, OwnedValue> =
                    player.get_property("Metadata").await.map_err(bus_error)?;
                let track = metadata
                    .get("mpris:trackid")
                    .and_then(|v| ObjectPath::try_from(v.try_clone().ok()?).ok())
                    .ok_or(MediaErrorOwned::NotSupported)?;
                let at = i64::try_from(at).unwrap_or(i64::MAX);
                player.call_method(action.method(), &(track, at)).await
            }
            _ => player.call_method(action.method(), &()).await,
        };
        res.map(|_| ()).map_err(bus_error)
    }
}

/// A proxy without a property cache: every read asks the player.
async fn proxy<'a>(
    conn: &Connection,
    name: &'a str,
    interface: &'a str,
) -> zbus::Result<zbus::Proxy<'a>> {
    zbus::proxy::Builder::new(conn)
        .destination(name)?
        .path(PATH)?
        .interface(interface)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}

/// The parent of a process, from `/proc`.
fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // pid (comm) state ppid ...; comm may hold spaces and parentheses.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parent;

    #[test]
    fn parent_of_this_process() {
        let ppid = std::os::unix::process::parent_id();
        assert_eq!(parent(std::process::id()), Some(ppid));
    }
}
