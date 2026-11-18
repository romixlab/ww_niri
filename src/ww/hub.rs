//! The ww side's own events (browser slots, players) for every connection that has the events
//! stream open, and the changes behind observed properties under `browsers[]` and `players[]`
//! and of the pointer's position for every connection.

use std::cell::RefCell;

use async_channel::{Receiver, Sender, TrySendError};
use ww_niri_api::EventOwned;

/// Events wait here for a connection that reads slowly; past it, it gets `Resync`.
const EVENT_QUEUE: usize = 64;

/// What may have changed an observed element property.
#[derive(Debug, Clone)]
pub enum Mark {
    Event(EventOwned),
    /// A player's status, track, rate or capabilities.
    Playback(ww_niri_api::PlayerId),
    /// A browser slot's URL or window.
    Browser(u8),
}

#[derive(Default)]
pub struct Hub {
    subscribers: RefCell<Vec<Sender<EventOwned>>>,
    marks: RefCell<Vec<Sender<Mark>>>,
    pointer: RefCell<Vec<Sender<()>>>,
}

impl Hub {
    /// Events from now on, for one connection.
    pub fn subscribe(&self) -> Receiver<EventOwned> {
        let (tx, rx) = async_channel::bounded(EVENT_QUEUE);
        self.subscribers.borrow_mut().push(tx);
        rx
    }

    /// Changes from now on, for one connection's observed properties.
    pub fn subscribe_marks(&self) -> Receiver<Mark> {
        let (tx, rx) = async_channel::bounded(EVENT_QUEUE);
        self.marks.borrow_mut().push(tx);
        rx
    }

    /// Moves of the pointer from now on, for one connection: a move it has not read yet stands
    /// for every move since.
    pub fn subscribe_pointer(&self) -> Receiver<()> {
        let (tx, rx) = async_channel::bounded(1);
        self.pointer.borrow_mut().push(tx);
        rx
    }

    /// The pointer moved, or the window under it.
    pub fn pointer_moved(&self) {
        self.pointer
            .borrow_mut()
            .retain(|tx| !matches!(tx.try_send(()), Err(TrySendError::Closed(()))));
    }

    fn mark(&self, mark: Mark) {
        self.marks
            .borrow_mut()
            .retain(|tx| tx.try_send(mark.clone()).is_ok());
    }

    pub fn playback_changed(&self, player: ww_niri_api::PlayerId) {
        self.mark(Mark::Playback(player));
    }

    pub fn browser_changed(&self, slot: u8) {
        self.mark(Mark::Browser(slot));
    }

    pub fn emit(&self, event: EventOwned) {
        self.mark(Mark::Event(event.clone()));
        // A full queue: that connection fell behind, it notices the closed channel and resyncs.
        self.subscribers
            .borrow_mut()
            .retain(|tx| tx.try_send(event.clone()).is_ok());
    }
}
