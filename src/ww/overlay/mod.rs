//! The overlay layer above the windows (`overlay` of the API, docs/ww/Overlay.md): items each
//! connection owns, by its own keys, stacked by priority.
//!
//! [Overlay] keeps the items, who owns each and when each ends; an item lifts at its end, on its
//! owner's `lift`, on a `dismiss` when dismissible, when its owner's connection ends or when its
//! window closes. Windows carry what to draw of their items ([Look]); output items are read from
//! [Overlay::on_output] at each frame. [render] draws both. While any item asked for `mute`, the
//! default sink is muted ([Overlay::mutes], [super::Audio::hold_mute]).

use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ww_niri_api::{
    At, Backdrop, ContentOwned, ItemEnd, ItemId, ItemInfoOwned, ItemOwned, OverlayError, ITEMS_MAX,
    ITEMS_PER_OWNER_MAX,
};

pub mod render;
mod state;
#[cfg(test)]
mod tests;

/// What an item is drawn on, with a browser slot resolved to its window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Window(u64),
    /// An output by connector name; `None`: the focused one.
    Output(Option<String>),
}

/// What the renderer draws of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Look {
    pub id: u64,
    pub priority: u8,
    pub at: At,
    pub content: ContentOwned,
    pub backdrop: Backdrop,
    /// The item's end, where a card's countdown reaches 0:00.
    pub until: Option<Instant>,
}

impl Look {
    /// On a window, the item hides the window under its blurred copy.
    pub fn covers(&self) -> bool {
        self.at == At::Fill && self.backdrop.blur > 0
    }

    /// The card's countdown line at `now`: its text, then the time left as `m:ss`, rounded up.
    pub fn countdown(&self, now: Instant) -> Option<String> {
        let ContentOwned::Card(card) = &self.content else {
            return None;
        };
        let label = card.countdown.as_deref()?;
        let left = self.until?.saturating_duration_since(now);
        let secs = left.as_millis().div_ceil(1000);
        Some(format!("{label} {}:{:02}", secs / 60, secs % 60))
    }

    /// It shows a clock.
    fn has_clock(&self) -> bool {
        match &self.content {
            ContentOwned::Card(card) => card.clock,
            ContentOwned::Clock { .. } => true,
            _ => false,
        }
    }

    /// The order of the stack: higher is in front.
    fn rank(&self) -> (u8, u64) {
        (self.priority, self.id)
    }
}

#[derive(Debug)]
struct Entry {
    owner: u64,
    key: String,
    target: Target,
    /// As put, with a slot unresolved: what `list` shows.
    item: ItemOwned,
    mute: bool,
    look: Rc<Look>,
}

/// The items by id, and who owns each.
#[derive(Debug, Default)]
pub struct Overlay {
    items: BTreeMap<u64, Entry>,
    last_id: u64,
}

impl Overlay {
    /// Puts up `owner`'s item `key` on `target` until `until`, or changes the one it has by that
    /// key; answers its id.
    ///
    /// # Errors
    ///
    /// `TooMany` for a new item past [ITEMS_PER_OWNER_MAX] or [ITEMS_MAX].
    pub fn put(
        &mut self,
        owner: u64,
        key: &str,
        target: Target,
        item: ItemOwned,
        until: Option<Instant>,
    ) -> Result<u64, OverlayError> {
        let id = match self.find(owner, key) {
            Some(id) => id,
            None => {
                let owned = self.items.values().filter(|e| e.owner == owner).count();
                if owned >= ITEMS_PER_OWNER_MAX || self.items.len() >= ITEMS_MAX {
                    return Err(OverlayError::TooMany);
                }
                self.last_id += 1;
                self.last_id
            }
        };
        let look = Rc::new(Look {
            id,
            priority: item.priority,
            at: item.place.at,
            content: item.content.clone(),
            backdrop: item.backdrop,
            until,
        });
        let entry = Entry {
            owner,
            key: key.to_owned(),
            target,
            mute: item.mute,
            item,
            look,
        };
        self.items.insert(id, entry);
        Ok(id)
    }

    fn find(&self, owner: u64, key: &str) -> Option<u64> {
        (self.items.iter())
            .find(|(_, e)| e.owner == owner && e.key == key)
            .map(|(id, _)| *id)
    }

    /// Lifts `owner`'s item `key`; answers its id.
    pub fn lift(&mut self, owner: u64, key: &str) -> Option<u64> {
        let id = self.find(owner, key)?;
        self.items.remove(&id);
        Some(id)
    }

    /// Lifts a dismissible item, whoever owns it.
    ///
    /// # Errors
    ///
    /// `NoSuchItem`, or `NotDismissible` for an item not put up as dismissible.
    pub fn dismiss(&mut self, id: u64) -> Result<(), OverlayError> {
        let entry = self.items.get(&id).ok_or(OverlayError::NoSuchItem)?;
        if !entry.item.dismissible {
            return Err(OverlayError::NotDismissible);
        }
        self.items.remove(&id);
        Ok(())
    }

    /// Lifts the items `owner` owns; answers their ids.
    pub fn lift_owned_by(&mut self, owner: u64) -> Vec<u64> {
        self.lift_where(|e| e.owner == owner)
    }

    /// Lifts the items whose end is at or before `now`; answers their ids.
    pub fn lift_due(&mut self, now: Instant) -> Vec<u64> {
        self.lift_where(|e| e.look.until.is_some_and(|until| until <= now))
    }

    /// Lifts the items on windows `alive` denies; answers their ids.
    pub fn lift_gone(&mut self, alive: impl Fn(u64) -> bool) -> Vec<u64> {
        self.lift_where(|e| matches!(e.target, Target::Window(w) if !alive(w)))
    }

    fn lift_where(&mut self, lifts: impl Fn(&Entry) -> bool) -> Vec<u64> {
        let lifted: Vec<u64> = (self.items.iter())
            .filter(|(_, e)| lifts(e))
            .map(|(id, _)| *id)
            .collect();
        for id in &lifted {
            self.items.remove(id);
        }
        lifted
    }

    /// Whether an item mutes the default sink.
    pub fn mutes(&self) -> bool {
        self.items.values().any(|e| e.mute)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether an item is on a window.
    pub fn on_windows(&self) -> bool {
        (self.items.values()).any(|e| matches!(e.target, Target::Window(_)))
    }

    /// What to draw on `window`, front to back.
    pub fn on_window(&self, window: u64) -> Vec<Rc<Look>> {
        self.looks(|t| *t == Target::Window(window))
    }

    /// What to draw on the output `name`, front to back; `focused`: it is the focused one.
    pub fn on_output(&self, name: &str, focused: bool) -> Vec<Rc<Look>> {
        self.looks(|t| match t {
            Target::Output(Some(on)) => on == name,
            Target::Output(None) => focused,
            Target::Window(_) => false,
        })
    }

    fn looks(&self, on: impl Fn(&Target) -> bool) -> Vec<Rc<Look>> {
        let mut looks: Vec<Rc<Look>> = (self.items.values())
            .filter(|e| on(&e.target))
            .map(|e| e.look.clone())
            .collect();
        looks.sort_by_key(|look| std::cmp::Reverse(look.rank()));
        looks
    }

    /// Every item, front to back, as `owner` sees them.
    pub fn list(&self, owner: u64) -> Vec<ItemInfoOwned> {
        let mut entries: Vec<&Entry> = self.items.values().collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.look.rank()));
        entries
            .into_iter()
            .map(|e| ItemInfoOwned {
                id: ItemId(e.look.id),
                key: e.key.clone(),
                own: e.owner == owner,
                item: e.item.clone(),
            })
            .collect()
    }

    /// When the next item ends, a countdown shows the next second or a clock the next minute,
    /// after `now`; `wall` is the wall clock at `now`.
    pub fn next_wake(&self, now: Instant, wall: SystemTime) -> Option<Instant> {
        let looks = self.items.values().map(|e| &e.look);
        let ends = looks.clone().filter_map(|look| {
            let until = look.until?;
            Some(match look.countdown(now) {
                Some(_) => next_tick(until, now),
                None => until.max(now),
            })
        });
        let minute = looks
            .clone()
            .any(|look| look.has_clock())
            .then(|| now + to_next_minute(wall));
        ends.chain(minute).min()
    }
}

/// The next whole second before `until` after `now`, where the countdown changes, or `until`.
fn next_tick(until: Instant, now: Instant) -> Instant {
    let left = until.saturating_duration_since(now);
    let past_second = Duration::from_nanos(u64::from(left.subsec_nanos()));
    if past_second.is_zero() {
        now + left.min(Duration::from_secs(1))
    } else {
        now + past_second
    }
}

/// From `wall` to the next minute of the wall clock; local time turns its minutes there too.
fn to_next_minute(wall: SystemTime) -> Duration {
    let since = wall.duration_since(UNIX_EPOCH).unwrap_or_default();
    let into = Duration::new(since.as_secs() % 60, since.subsec_nanos());
    Duration::from_secs(60) - into
}

/// What the compositor tells the clients when items go.
pub fn ended(hub: &super::hub::Hub, ids: &[u64], why: ItemEnd) {
    for &id in ids {
        debug!("ww: overlay item {id} lifted: {why:?}");
        hub.emit(ww_niri_api::EventOwned::OverlayEnded {
            id: ItemId(id),
            why,
        });
    }
}
