//! The ww side's own events (browser slots, players) for every connection that has the events
//! stream open.

use std::cell::RefCell;

use async_channel::{Receiver, Sender};
use ww_niri_api::EventOwned;

/// Events wait here for a connection that reads slowly; past it, it gets `Resync`.
const EVENT_QUEUE: usize = 64;

#[derive(Default)]
pub struct Hub {
    subscribers: RefCell<Vec<Sender<EventOwned>>>,
}

impl Hub {
    /// Events from now on, for one connection.
    pub fn subscribe(&self) -> Receiver<EventOwned> {
        let (tx, rx) = async_channel::bounded(EVENT_QUEUE);
        self.subscribers.borrow_mut().push(tx);
        rx
    }

    pub fn emit(&self, event: EventOwned) {
        // A full queue: that connection fell behind, it notices the closed channel and resyncs.
        self.subscribers
            .borrow_mut()
            .retain(|tx| tx.try_send(event.clone()).is_ok());
    }
}
