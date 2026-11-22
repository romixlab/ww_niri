//! What a connection does between requests, as `wire_weaver_server` asks of its backend: it
//! follows niri's events and the ww side's own for the `events` stream and the observed
//! properties, lets go of held keys and buttons at their hold limit, and when the link goes
//! releases everything the client held.

use std::future::Future;
use std::pin::pin;
use std::time::Instant;

use async_channel::Receiver;
use futures_util::future::select;
use wire_weaver::{EventWriter, MessageSink};
use ww_niri_api::{EventOwned, WindowId};

use super::host::{call, Run};
use super::hub::{Hub, Mark};
use super::input::Input;
use super::server::{api_impl, Server};
use super::Audio;

/// What the client's `events` stream follows while open: niri's events (`None` when niri runs
/// without IPC) and the ww side's own (browser slots, players).
enum Events {
    Off,
    On {
        niri: Option<Receiver<niri_ipc::Event>>,
        ww: Receiver<EventOwned>,
    },
}

/// Something that happened, for [Backend::poll](wire_weaver_server::Backend::poll) to act on.
pub enum Next {
    Niri(niri_ipc::Event),
    Ww(EventOwned),
    /// A queue was dropped for falling behind.
    Behind,
    /// The default sink changed.
    Audio,
    /// One of the ww side's events, for observed properties under `browsers[]` and `players[]`.
    Mark(Mark),
    /// The pointer moved, or the window under it.
    Pointer,
    /// A hold limit of a button or key the client holds passed.
    HoldLimit,
}

/// The first of two to come.
async fn race(a: impl Future<Output = Next>, b: impl Future<Output = Next>) -> Next {
    select(pin!(a), pin!(b)).await.factor_first().0
}

/// What a connection waits on besides the client.
pub struct Sources {
    events: Events,
    /// The default sink changed.
    audio: Receiver<()>,
    /// Observed element properties follow the ww side's events whether or not the events
    /// stream is open.
    marks: Receiver<Mark>,
    pointer: Receiver<()>,
}

impl Sources {
    pub fn new(hub: &Hub, audio: &Audio) -> Self {
        Self {
            events: Events::Off,
            audio: audio.watch(),
            marks: hub.subscribe_marks(),
            pointer: hub.subscribe_pointer(),
        }
    }

    /// Follows niri's events while the client has the stream open.
    async fn follow_events(&mut self, events_open: bool, hub: &Hub, run: &Run) {
        match (&self.events, events_open) {
            (Events::Off, true) => {
                self.events = Events::On {
                    niri: call(run, |host| host.subscribe_events()).await.flatten(),
                    ww: hub.subscribe(),
                };
            }
            (Events::On { .. }, false) => self.events = Events::Off,
            _ => (),
        }
    }

    /// The next thing to happen; `input` for its hold limits. Cancel-safe: the channels keep
    /// what was not taken.
    async fn next(&self, input: &Input) -> Next {
        let event = async {
            let Events::On { niri, ww } = &self.events else {
                return std::future::pending().await;
            };
            let niri = async {
                match niri {
                    Some(rx) => rx.recv().await.map_or(Next::Behind, Next::Niri),
                    None => std::future::pending().await,
                }
            };
            let ww = async { ww.recv().await.map_or(Next::Behind, Next::Ww) };
            race(niri, ww).await
        };
        let audio = async {
            match self.audio.recv().await {
                Ok(()) => Next::Audio,
                Err(_) => std::future::pending().await,
            }
        };
        let mark = async {
            match self.marks.recv().await {
                Ok(event) => Next::Mark(event),
                // Fell behind: resubscribed in `handle`.
                Err(_) => Next::Behind,
            }
        };
        let pointer = async {
            match self.pointer.recv().await {
                Ok(()) => Next::Pointer,
                Err(_) => std::future::pending().await,
            }
        };
        let hold_limit = async {
            input.hold_limit_passes().await;
            Next::HoldLimit
        };
        let changed = race(race(audio, mark), race(pointer, hold_limit));
        race(event, changed).await
    }
}

impl Server {
    /// Acts on what happened; answers the events for the client's stream.
    async fn handle(&mut self, next: Next) -> Vec<EventOwned> {
        let peer = self.peer;
        match next {
            Next::Niri(event) => {
                let mut out = map_event(&self.run, event).await;
                for event in &mut out {
                    if let EventOwned::WindowOpenedOrChanged(window) = event {
                        self.mark_browser(window);
                    }
                }
                out
            }
            Next::Ww(event) => vec![event],
            Next::Mark(event) => {
                self.mark(&event).await;
                Vec::new()
            }
            Next::Behind if self.sources.marks.is_closed() => {
                self.sources.marks = self.hub.subscribe_marks();
                self.mark_all();
                Vec::new()
            }
            Next::Audio => {
                self.mark_changed_audio_volume();
                self.mark_changed_audio_muted();
                self.mark_changed_audio_sink();
                Vec::new()
            }
            Next::Pointer => {
                self.mark_changed_input_pointer_position();
                Vec::new()
            }
            Next::HoldLimit => {
                match self.input.release_expired(Instant::now()).await {
                    Ok(count) => info!("ww: {count} held by {peer} came up: hold limit"),
                    Err(err) => warn!("ww: error releasing what {peer} held: {err:?}"),
                }
                Vec::new()
            }
            Next::Behind => {
                // A subscription was dropped, as niri does for any client too slow to read:
                // subscribe again and have the client list again.
                debug!("ww: {peer} fell behind on events");
                self.sources.events = Events::Off;
                vec![EventOwned::Resync]
            }
        }
    }
}

impl wire_weaver_server::Backend for Server {
    async fn wait(&mut self) {
        if self.due.is_none() {
            self.due = Some(self.sources.next(&self.input).await);
        }
    }

    /// Observed properties: when one is due (a period, a held-back change).
    fn next_wake(&mut self, now: ww_device::Instant) -> Option<ww_device::Instant> {
        self.observe
            .next_wake(now.as_micros())
            .map(ww_device::Instant::from_micros)
    }

    async fn poll(
        &mut self,
        out: &mut EventWriter<'_, impl MessageSink>,
        medium: (),
        now: ww_device::Instant,
    ) {
        let (events_open, hub, run) = (self.events_open, self.hub.clone(), self.run.clone());
        self.sources.follow_events(events_open, &hub, &run).await;
        if let Some(next) = self.due.take() {
            let events = self.handle(next).await;
            if self.events_open {
                for event in events {
                    let _ = api_impl::stream_data_ser().events_send(&event, out).await;
                }
            }
        }
        let mut buf = std::mem::take(&mut self.observe_buf);
        let _ = self
            .poll_properties(out, medium, now.as_micros(), &mut buf)
            .await;
        self.observe_buf = buf;
    }

    /// The link went: the covers the client held lift, as a lease does, and nothing it held
    /// down stays down.
    async fn end(&mut self) {
        debug!("ww: link to {} is down", self.peer);
        let holder = self.holder;
        let _ = call(&self.run, move |host| host.uncover_held_by(holder)).await;
        if let Err(err) = self.input.release_all().await {
            warn!("ww: error releasing what {} held: {err:?}", self.peer);
        }
    }
}

/// The API's events for one of niri's; most of niri's have no counterpart.
async fn map_event(run: &Run, event: niri_ipc::Event) -> Vec<EventOwned> {
    use niri_ipc::Event;

    let changed = |ids: Vec<u64>| async move {
        call(run, move |host| {
            ids.into_iter()
                .filter_map(|id| host.window(id))
                .map(EventOwned::WindowOpenedOrChanged)
                .collect()
        })
        .await
        .unwrap_or_default()
    };

    match event {
        Event::WindowOpenedOrChanged { window } => changed(vec![window.id]).await,
        // Fullscreen and moves between outputs show up as layout changes.
        Event::WindowLayoutsChanged { changes } => {
            changed(changes.into_iter().map(|(id, _)| id).collect()).await
        }
        Event::WindowClosed { id } => vec![EventOwned::WindowClosed(WindowId(id))],
        Event::WindowFocusChanged { id } => {
            vec![EventOwned::WindowFocusChanged(id.map(WindowId))]
        }
        Event::WindowsChanged { .. } => vec![EventOwned::Resync],
        _ => Vec::new(),
    }
}
