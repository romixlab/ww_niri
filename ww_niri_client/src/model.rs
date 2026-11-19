//! The model task: follows the connection, and on each link lists the TV, opens the observers and
//! the `events` stream, and publishes a [Snapshot] after every change.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use wire_weaver_client::observe::{on_change, ObserveEvent};
use wire_weaver_client::{ConnectionEvent, StreamError};
use ww_niri_api::{
    BrowserStateOwned, CoverEnd, EventOwned, PlaybackStateOwned, PlayerId, PlayerInfoOwned,
    PointerPosition, SinkOwned, WindowId,
};

use crate::api::NiriClient;
use crate::config::OnChange;
use crate::error::TvError;
use crate::snapshot::{ConnectionState, Playback, PlayerState, SlotState, Snapshot};

/// Observed values waiting for the model task; observers wait while it is full.
const UPDATES_MAX: usize = 256;
/// After a session failed while the link stayed up, the wait before listing again.
const RELIST_DELAY: Duration = Duration::from_millis(250);
/// Browser slots the model follows; ww.kdl's `browsers` is far below.
const SLOTS_MAX: usize = 32;
/// Lifted covers kept for a slow [Tv::cover_lifted](crate::Tv::cover_lifted) reader.
const COVER_LIFTED_MAX: usize = 64;

/// A cover that lifted on the TV, from [Tv::cover_lifted](crate::Tv::cover_lifted).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CoverLifted {
    pub window: WindowId,
    pub why: CoverEnd,
}

/// What the [Tv](crate::Tv) handles read, and the model task writes.
pub(crate) struct Shared {
    pub(crate) snapshot: watch::Sender<Arc<Snapshot>>,
    pub(crate) state: watch::Sender<ConnectionState>,
    pub(crate) on_change: Option<OnChange>,
    pub(crate) cover_lifted: broadcast::Sender<CoverLifted>,
    /// The windows this client's connection holds a cover on, as far as it knows.
    held_covers: Mutex<BTreeSet<u64>>,
}

impl Shared {
    pub(crate) fn new(on_change: Option<OnChange>) -> Self {
        Self {
            snapshot: watch::Sender::new(Arc::new(Snapshot::default())),
            state: watch::Sender::new(ConnectionState::Connecting {
                attempt: 0,
                offline_since: None,
                last_error: None,
            }),
            on_change,
            cover_lifted: broadcast::channel(COVER_LIFTED_MAX).0,
            held_covers: Mutex::new(BTreeSet::new()),
        }
    }

    /// The set stays whole whatever panics: every change is one insert, remove or take.
    fn held_covers(&self) -> std::sync::MutexGuard<'_, BTreeSet<u64>> {
        self.held_covers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A cover of this client's connection holds on `window`.
    pub(crate) fn hold_cover(&self, window: WindowId) {
        self.held_covers().insert(window.0);
    }

    /// The window of browser slot `slot`, as the last snapshot has it.
    pub(crate) fn slot_window(&self, slot: u8) -> Option<WindowId> {
        let snapshot = self.snapshot.borrow();
        let state = snapshot
            .slots
            .iter()
            .find(|s| s.index == slot)?
            .state
            .as_ref()?;
        state.window
    }

    fn cover_lifted(&self, lifted: CoverLifted) {
        self.held_covers().remove(&lifted.window.0);
        // Nobody listens: dropped.
        drop(self.cover_lifted.send(lifted));
    }

    /// The link went: the TV lifted the covers this client held.
    fn covers_lost(&self) {
        let windows = std::mem::take(&mut *self.held_covers());
        for window in windows {
            drop(self.cover_lifted.send(CoverLifted {
                window: WindowId(window),
                why: CoverEnd::Disconnected,
            }));
        }
    }

    fn changed(&self) {
        if let Some(f) = &self.on_change {
            f();
        }
    }

    fn publish(&self, snapshot: &mut Snapshot) {
        snapshot.generation += 1;
        self.snapshot.send_replace(Arc::new(snapshot.clone()));
        self.changed();
    }

    fn set_state(&self, state: ConnectionState) {
        if self.state.send_if_modified(|s| {
            let changed = *s != state;
            *s = state;
            changed
        }) {
            self.changed();
        }
    }
}

/// An observed value, from an observer's task.
#[derive(Debug)]
enum Update {
    Volume(u16),
    Muted(bool),
    Sink(SinkOwned),
    Pointer(PointerPosition),
    Slot(u8, BrowserStateOwned),
    SlotPlayback(u8, PlaybackStateOwned),
    PlayerPlayback(PlayerId, PlaybackStateOwned),
}

/// A task that ends when dropped.
struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Why a session ended.
enum End {
    /// The TV missed events: list everything again.
    Resync,
    /// A call or the stream failed while the link may still be up.
    Failed(TvError),
}

impl From<TvError> for End {
    fn from(e: TvError) -> Self {
        End::Failed(e)
    }
}

impl From<wire_weaver_client::Error> for End {
    fn from(e: wire_weaver_client::Error) -> Self {
        End::Failed(e.into())
    }
}

/// Forwards an observer's values as updates until it ends.
macro_rules! forward {
    ($observer:expr, $tx:expr, |$v:ident| $update:expr) => {{
        let mut observer = $observer;
        let tx = $tx.clone();
        Task(tokio::spawn(async move {
            loop {
                match observer.recv_event().await {
                    Ok(ObserveEvent::Update(o)) => {
                        let $v = o.value;
                        if tx.send($update).await.is_err() {
                            return;
                        }
                    }
                    Ok(ObserveEvent::Ended) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        }))
    }};
}

/// One link's view of the TV.
struct Session<'a> {
    client: &'a NiriClient,
    shared: &'a Shared,
    snapshot: Snapshot,
    tx: mpsc::Sender<Update>,
    tasks: Vec<Task>,
    players: HashMap<u32, Task>,
}

impl Session<'_> {
    async fn observe_player(&mut self, id: PlayerId) -> Result<(), End> {
        if self.players.contains_key(&id.0) {
            return Ok(());
        }
        let observer = self
            .client
            .media()
            .players(id.0)
            .playback()
            .observe_state(on_change(Duration::ZERO))
            .subscribe()
            .await?;
        let task = forward!(observer, self.tx, |v| Update::PlayerPlayback(id, v));
        self.players.insert(id.0, task);
        Ok(())
    }

    /// Lists everything and opens the observers.
    async fn open(&mut self) -> Result<(), End> {
        let c = self.client;
        let tx = &self.tx;
        let audio = c.audio();
        self.tasks.push(forward!(
            audio
                .observe_volume(on_change(Duration::ZERO))
                .subscribe()
                .await?,
            tx,
            |v| Update::Volume(v.get())
        ));
        self.tasks.push(forward!(
            audio
                .observe_muted(on_change(Duration::ZERO))
                .subscribe()
                .await?,
            tx,
            |v| Update::Muted(v)
        ));
        self.tasks.push(forward!(
            audio
                .observe_sink(on_change(Duration::ZERO))
                .subscribe()
                .await?,
            tx,
            |v| Update::Sink(v)
        ));
        self.tasks.push(forward!(
            c.input()
                .pointer()
                .observe_position(on_change(Duration::ZERO))
                .subscribe()
                .await?,
            tx,
            |v| Update::Pointer(v)
        ));

        let indices: Vec<u32> = (c.browsers_valid_indices().read().await?.iter())
            .take(SLOTS_MAX)
            .collect();
        let mut slots = Vec::new();
        for index in indices {
            let Ok(slot) = u8::try_from(index) else {
                continue;
            };
            let browser = c.browsers(index);
            self.tasks.push(forward!(
                browser
                    .observe_state(on_change(Duration::ZERO))
                    .subscribe()
                    .await?,
                tx,
                |v| Update::Slot(slot, v)
            ));
            self.tasks.push(forward!(
                browser
                    .playback()
                    .observe_state(on_change(Duration::ZERO))
                    .subscribe()
                    .await?,
                tx,
                |v| Update::SlotPlayback(slot, v)
            ));
            slots.push(SlotState {
                index: slot,
                state: None,
                playback: None,
            });
        }
        self.snapshot.slots = slots;

        let mut windows = c.windows().list().call().await?;
        windows.sort_by_key(|w| w.id.0);
        self.snapshot.windows = windows;
        self.snapshot.outputs = c.outputs().list().call().await?;
        let players = c.media().list().call().await?;
        self.snapshot.players.clear();
        for info in players {
            self.add_player(info).await?;
        }
        Ok(())
    }

    async fn add_player(&mut self, info: PlayerInfoOwned) -> Result<(), End> {
        let id = info.id;
        match self.snapshot.players.iter_mut().find(|p| p.info.id == id) {
            Some(p) => p.info = info,
            None => {
                self.snapshot.players.push(PlayerState {
                    info,
                    playback: None,
                });
                self.snapshot.players.sort_by_key(|p| p.info.id.0);
            }
        }
        self.observe_player(id).await
    }

    fn update(&mut self, update: Update) {
        let now = Instant::now();
        let s = &mut self.snapshot;
        match update {
            Update::Volume(v) => s.audio.volume = Some(v),
            Update::Muted(m) => s.audio.muted = Some(m),
            Update::Sink(sink) => s.audio.sink = Some(sink),
            Update::Pointer(p) => s.pointer = Some(p),
            Update::Slot(i, state) => {
                if let Some(slot) = s.slots.iter_mut().find(|x| x.index == i) {
                    slot.state = Some(state);
                }
            }
            Update::SlotPlayback(i, state) => {
                if let Some(slot) = s.slots.iter_mut().find(|x| x.index == i) {
                    slot.playback = Some(Playback {
                        state,
                        received: now,
                    });
                }
            }
            Update::PlayerPlayback(id, state) => {
                if let Some(p) = s.players.iter_mut().find(|p| p.info.id == id) {
                    p.playback = Some(Playback {
                        state,
                        received: now,
                    });
                }
            }
        }
    }

    async fn event(&mut self, event: EventOwned) -> Result<(), End> {
        let s = &mut self.snapshot;
        match event {
            EventOwned::WindowOpenedOrChanged(w) => {
                match s.windows.iter_mut().find(|x| x.id == w.id) {
                    Some(x) => *x = w,
                    None => {
                        s.windows.push(w);
                        s.windows.sort_by_key(|w| w.id.0);
                    }
                }
            }
            EventOwned::WindowClosed(id) => s.windows.retain(|w| w.id != id),
            EventOwned::WindowFocusChanged(id) => {
                for w in &mut s.windows {
                    w.focused = Some(w.id) == id;
                }
            }
            EventOwned::OutputsChanged => s.outputs = self.client.outputs().list().call().await?,
            EventOwned::PlayerAppeared(info) => self.add_player(info).await?,
            EventOwned::PlayerGone(id) => {
                s.players.retain(|p| p.info.id != id);
                self.players.remove(&id.0);
            }
            EventOwned::Resync => return Err(End::Resync),
            // The slots' observers follow their phases.
            EventOwned::BrowserStarted(_) | EventOwned::BrowserStopped { .. } => {}
            // Covers belong to the connection that set them, not to the snapshot.
            EventOwned::CoverLifted { window, why } => {
                self.shared.cover_lifted(CoverLifted { window, why });
            }
            EventOwned::Unknown(_) => {}
        }
        Ok(())
    }
}

/// Follows the TV on one link until it fails or asks for a resync.
async fn session(client: &NiriClient, shared: &Shared, address: String, delay: Duration) -> End {
    tokio::time::sleep(delay).await;
    match session_inner(client, shared, address).await {
        Ok(never) => match never {},
        Err(end) => end,
    }
}

async fn session_inner(
    client: &NiriClient,
    shared: &Shared,
    address: String,
) -> Result<std::convert::Infallible, End> {
    let info = client.info().call().await?;
    let mut events = client.events().call().await?;
    let (tx, mut rx) = mpsc::channel(UPDATES_MAX);
    let mut s = Session {
        client,
        shared,
        snapshot: Snapshot {
            generation: shared.snapshot.borrow().generation,
            ..Snapshot::default()
        },
        tx,
        tasks: Vec::new(),
        players: HashMap::new(),
    };
    s.open().await?;
    s.shared
        .set_state(ConnectionState::Connected { address, info });
    s.shared.publish(&mut s.snapshot);
    loop {
        tokio::select! {
            event = events.recv() => {
                let event = event.map_err(|e| End::Failed(stream_error(e)))?;
                s.event(event).await?;
            }
            Some(update) = rx.recv() => s.update(update),
        }
        // What else came meanwhile goes into the same snapshot.
        while let Ok(update) = rx.try_recv() {
            s.update(update);
        }
        s.shared.publish(&mut s.snapshot);
    }
}

fn stream_error(e: StreamError) -> TvError {
    match e {
        StreamError::Closed => TvError::Offline,
        e => TvError::Stream(Arc::new(e)),
    }
}

/// The address the client is connected at, if it is.
fn connected_address(client: &NiriClient) -> Option<String> {
    let state = client.cmd.connection_state()?;
    match &*state.borrow() {
        wire_weaver_client::ConnectionState::Connected { address } => Some(address.clone()),
        _ => None,
    }
}

/// A running [session].
type Running<'a> = Pin<Box<dyn Future<Output = End> + Send + 'a>>;

/// Runs the model until the [Tv](crate::Tv) is dropped.
pub(crate) async fn run(client: NiriClient, shared: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    let (client, shared) = (&client, &*shared);
    let start = |address: String, delay: Duration| -> Running<'_> {
        Box::pin(session(client, shared, address, delay))
    };
    let restart = |delay: Duration| connected_address(client).map(|a| start(a, delay));
    let mut link = client.cmd.connection_events();
    let mut offline_since = None;
    let mut last_error = None;
    let mut running = restart(Duration::ZERO);
    loop {
        tokio::select! {
            end = async { running.as_mut().expect("guarded by is_some").await }, if running.is_some() => {
                // The session ended by itself; list again while the link stays.
                let delay = match end {
                    End::Resync => Duration::ZERO,
                    End::Failed(e) => {
                        last_error = Some(e.to_string());
                        RELIST_DELAY
                    }
                };
                running = restart(delay);
            }
            event = link.recv() => match event {
                Ok(ConnectionEvent::Connecting { attempt }) => {
                    if running.is_none() {
                        shared.set_state(ConnectionState::Connecting {
                            attempt,
                            offline_since,
                            last_error: last_error.clone(),
                        });
                    }
                }
                Ok(ConnectionEvent::AttemptFailed { attempt, error, .. }) => {
                    last_error = Some(error);
                    shared.set_state(ConnectionState::Connecting {
                        attempt,
                        offline_since,
                        last_error: last_error.clone(),
                    });
                }
                Ok(ConnectionEvent::Connected { address, .. }) => {
                    // Held on a link that went before this task heard of it.
                    shared.covers_lost();
                    running = Some(start(address, Duration::ZERO));
                }
                // Lagged: what happened is unknown, so start over from where the link is.
                Ok(ConnectionEvent::Reconnected { .. })
                | Err(broadcast::error::RecvError::Lagged(_)) => running = restart(Duration::ZERO),
                Ok(ConnectionEvent::Disconnected { reason, .. }) => {
                    running = None;
                    shared.covers_lost();
                    offline_since = Some(Instant::now());
                    last_error = Some(reason.to_string());
                    shared.set_state(ConnectionState::Connecting {
                        attempt: 0,
                        offline_since,
                        last_error: last_error.clone(),
                    });
                }
                Ok(ConnectionEvent::GaveUp { error }) => {
                    shared.set_state(ConnectionState::Offline {
                        since: offline_since.unwrap_or_else(Instant::now),
                        error,
                    });
                    return;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = stop.changed() => return,
        }
    }
}
