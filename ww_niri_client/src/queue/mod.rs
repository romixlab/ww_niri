//! The command task: lanes of queued commands, one call in flight per lane, merging while a call
//! is in flight, and the renewal of held buttons and keys.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use wire_weaver_client::{ConnectionEvent, ConnectionState};
use ww_niri_api::Button;

use crate::api::NiriClient;
use crate::command::{Command, Lane, execute};
use crate::error::TvError;
use crate::model::Shared;

/// Commands waiting in one lane; more are refused with [TvError::QueueFull].
pub(crate) const LANE_PENDING_MAX: usize = 256;

/// Where a typed call hears its answer.
pub(crate) type Reply = oneshot::Sender<Result<(), TvError>>;

/// A command on its way to the command task.
pub(crate) struct Request {
    pub(crate) command: Command,
    pub(crate) reply: Option<Reply>,
}

/// A command sent with [Tv::send](crate::Tv::send) that failed.
#[derive(Clone, Debug)]
pub struct Failed {
    /// As sent, after merging: the sum of the moves merged into it.
    pub command: Command,
    pub error: TvError,
}

/// A queued command and everyone waiting for it: merged commands share one answer.
struct Pending {
    command: Command,
    replies: Vec<Reply>,
}

#[derive(Default)]
struct LaneQueue {
    pending: VecDeque<Pending>,
    busy: bool,
}

/// The queued commands, by lane.
#[derive(Default)]
struct Lanes {
    lanes: HashMap<Lane, LaneQueue>,
}

impl Lanes {
    /// Queues `command`, merged into the last queued one of its lane where one call does both.
    fn push(&mut self, mut command: Command, reply: Option<Reply>) -> Result<(), Option<Reply>> {
        let queue = self.lanes.entry(command.lane()).or_default();
        if let Some(last) = queue.pending.back_mut() {
            match last.command.merge(command) {
                Ok(()) => {
                    last.replies.extend(reply);
                    return Ok(());
                }
                Err(next) => command = next,
            }
        }
        if queue.pending.len() >= LANE_PENDING_MAX {
            return Err(reply);
        }
        queue.pending.push_back(Pending {
            command,
            replies: reply.into_iter().collect(),
        });
        Ok(())
    }

    /// A command to send now: the first of a lane with nothing in flight.
    fn next_ready(&mut self) -> Option<(Lane, Pending)> {
        let (lane, queue) = self
            .lanes
            .iter_mut()
            .find(|(_, q)| !q.busy && !q.pending.is_empty())?;
        queue.busy = true;
        let pending = queue
            .pending
            .pop_front()
            .expect("the lane was found with a pending command");
        Some((lane.clone(), pending))
    }

    /// The lane's call answered.
    fn done(&mut self, lane: &Lane) {
        if let Some(queue) = self.lanes.get_mut(lane) {
            queue.busy = false;
            if queue.pending.is_empty() {
                self.lanes.remove(lane);
            }
        }
    }

    fn is_queued(&self, command: &Command) -> bool {
        self.lanes
            .get(&command.lane())
            .is_some_and(|q| q.pending.iter().any(|p| p.command == *command))
    }
}

/// Buttons and keys this client holds down.
#[derive(Default)]
struct Held {
    buttons: BTreeSet<ButtonKey>,
    keys: BTreeSet<String>,
}

/// [Button] in a set: the API type has no order.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ButtonKey(u8);

const BUTTONS: [Button; 5] = [
    Button::Left,
    Button::Right,
    Button::Middle,
    Button::Back,
    Button::Forward,
];

impl ButtonKey {
    fn of(button: Button) -> Option<Self> {
        let i = BUTTONS.iter().position(|b| *b == button)?;
        Some(Self(u8::try_from(i).expect("five buttons fit a u8")))
    }

    fn button(self) -> Button {
        BUTTONS[usize::from(self.0)]
    }
}

impl Held {
    /// Follows what `command` holds or lets go.
    fn note(&mut self, command: &Command) {
        match command {
            Command::Press(b) => self.buttons.extend(ButtonKey::of(*b)),
            Command::Release(b) => {
                if let Some(k) = ButtonKey::of(*b) {
                    self.buttons.remove(&k);
                }
            }
            Command::KeyDown(k) => {
                self.keys.insert(k.clone());
            }
            Command::KeyUp(k) => {
                self.keys.remove(k);
            }
            _ => {}
        }
    }

    /// The presses that renew what is held.
    fn renewals(&self) -> Vec<Command> {
        let buttons = self.buttons.iter().map(|b| Command::Press(b.button()));
        let keys = self.keys.iter().cloned().map(Command::KeyDown);
        buttons.chain(keys).collect()
    }

    /// The releases of everything held, and nothing held after.
    fn release_all(&mut self) -> Vec<Command> {
        let buttons = self.buttons.iter().map(|b| Command::Release(b.button()));
        let keys = self.keys.iter().cloned().map(Command::KeyUp);
        let releases = buttons.chain(keys).collect();
        self.buttons.clear();
        self.keys.clear();
        releases
    }

    fn clear(&mut self) {
        self.buttons.clear();
        self.keys.clear();
    }
}

/// The command task's state.
struct Queue {
    lanes: Lanes,
    held: Held,
    errors: broadcast::Sender<Failed>,
}

impl Queue {
    fn accept(&mut self, request: Request) {
        let Request { command, reply } = request;
        if command == Command::ReleaseAll {
            let releases = self.held.release_all();
            let last = releases.len().saturating_sub(1);
            let mut reply = reply;
            for (i, release) in releases.into_iter().enumerate() {
                let r = if i == last { reply.take() } else { None };
                self.queue(release, r);
            }
            if let Some(reply) = reply {
                drop(reply.send(Ok(())));
            }
            return;
        }
        self.held.note(&command);
        self.queue(command, reply);
    }

    fn queue(&mut self, command: Command, reply: Option<Reply>) {
        let failed = command.clone();
        if let Err(reply) = self.lanes.push(command, reply) {
            self.answer(failed, reply.into_iter().collect(), Err(TvError::QueueFull));
        }
    }

    fn renew(&mut self) {
        for command in self.held.renewals() {
            if !self.lanes.is_queued(&command) {
                self.queue(command, None);
            }
        }
    }

    /// Gives the result to the typed calls waiting for it, or to [Tv::errors](crate::Tv::errors)
    /// when none waits and it failed.
    fn answer(&self, command: Command, replies: Vec<Reply>, result: Result<(), TvError>) {
        if replies.is_empty() {
            if let Err(error) = result {
                // Nobody listens for errors: they are dropped.
                drop(self.errors.send(Failed { command, error }));
            }
            return;
        }
        for reply in replies {
            // The typed call was dropped before its answer.
            drop(reply.send(result.clone()));
        }
    }
}

/// Runs the commands of `requests` until the [Tv](crate::Tv) is dropped.
pub(crate) async fn run(
    client: NiriClient,
    shared: Arc<Shared>,
    mut requests: mpsc::Receiver<Request>,
    errors: broadcast::Sender<Failed>,
    hold: Duration,
    mut stop: watch::Receiver<bool>,
) {
    let mut queue = Queue {
        lanes: Lanes::default(),
        held: Held::default(),
        errors,
    };
    let mut in_flight = JoinSet::new();
    let mut renew = tokio::time::interval(hold / 3);
    renew.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut link = client.cmd.connection_events();
    let mut link_open = true;
    let state = client.cmd.connection_state();
    let client = Arc::new(client);
    loop {
        while let Some((lane, pending)) = queue.lanes.next_ready() {
            let client = client.clone();
            in_flight.spawn(async move {
                let result = execute(&client, &pending.command, hold).await;
                (lane, pending, result)
            });
        }
        tokio::select! {
            request = requests.recv() => match request {
                Some(request) => queue.accept(request),
                None => return,
            },
            Some(done) = in_flight.join_next() => match done {
                Ok((lane, pending, result)) => {
                    queue.lanes.done(&lane);
                    let result = result.map(|put| {
                        if let (Some(id), Command::OverlayPut { key, .. }) = (put, &pending.command) {
                            shared.own_item(id, key);
                        }
                    });
                    queue.answer(pending.command, pending.replies, result);
                }
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Err(_cancelled) => return,
            },
            _ = renew.tick() => {
                let connected = state
                    .as_ref()
                    .is_some_and(|s| matches!(*s.borrow(), ConnectionState::Connected { .. }));
                if connected {
                    queue.renew();
                }
            }
            event = link.recv(), if link_open => match event {
                // The TV released what this client held when the link went.
                Ok(ConnectionEvent::Disconnected { .. }) => queue.held.clear(),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => link_open = false,
            },
            _ = stop.changed() => return,
        }
    }
}

#[cfg(test)]
mod tests;
