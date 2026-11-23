//! The overlay layer above the windows (`overlay` of the API, docs/ww/Overlay.md): items each
//! connection owns, by its own keys, stacked by priority.
//!
//! [Overlay] keeps the items, who owns each and when each ends; an item lifts at its end, on its
//! owner's `lift`, on a `dismiss` when dismissible, when its owner's connection ends or when its
//! window closes. Windows carry what to draw of their items ([Look]); output items are read from
//! [Overlay::on_output] at each frame. [render] draws both. While any item asked for `mute`, the
//! default sink is muted ([Overlay::mutes], [super::Audio::hold_mute]). A [ShowOnlyOwned] filter
//! keeps the items it does not match up but undrawn.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ww_niri_api::{
    At, BackdropOwned, ContentOwned, FillOwned, ItemEnd, ItemId, ItemInfoOwned, ItemOwned,
    OverlayError, ShowOnlyOwned, ITEMS_MAX, ITEMS_PER_OWNER_MAX,
};

pub mod image;
pub mod render;
mod state;
#[cfg(test)]
mod tests;
pub mod volume;

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
    pub backdrop: BackdropOwned,
    /// The fill's image, read when the item was put.
    pub image: Option<Rc<image::Image>>,
    /// The item's end, where a card's countdown reaches 0:00.
    pub until: Option<Instant>,
    /// The item's first put, where a progress starts counting.
    pub since: Instant,
    /// A frame for the screen drew it since [Overlay::frame_started].
    pub drawn: Cell<bool>,
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

    /// A progress's time since the item's first put at `now`, as `m:ss`, rounded down.
    pub fn elapsed(&self, now: Instant) -> Option<String> {
        let ContentOwned::Progress { .. } = &self.content else {
            return None;
        };
        let secs = now.saturating_duration_since(self.since).as_secs();
        Some(format!("{}:{:02}", secs / 60, secs % 60))
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
    /// The owner waits for [Overlay::take_shown] to report this put.
    report: bool,
    look: Rc<Look>,
}

impl Entry {
    fn shown_by(&self, filter: Option<&ShowOnlyOwned>) -> bool {
        filter.is_none_or(|f| f.shows(self.look.priority, &self.key, ItemId(self.look.id)))
    }
}

/// The items by id, and who owns each.
#[derive(Debug, Default)]
pub struct Overlay {
    items: BTreeMap<u64, Entry>,
    last_id: u64,
    show_only: Option<ShowOnlyOwned>,
    /// Where [FillOwned::Image] names its images; `None`: no images.
    pub backgrounds: Option<PathBuf>,
    images: image::Images,
}

impl Overlay {
    /// Puts up `owner`'s item `key` on `target` until `until`, or changes the one it has by that
    /// key; answers its id.
    ///
    /// # Errors
    ///
    /// `TooMany` for a new item past [ITEMS_PER_OWNER_MAX] or [ITEMS_MAX]; `NoSuchImage` for a
    /// fill's image that is not in [Overlay::backgrounds] or is not a PNG image.
    pub fn put(
        &mut self,
        owner: u64,
        key: &str,
        target: Target,
        item: ItemOwned,
        until: Option<Instant>,
    ) -> Result<u64, OverlayError> {
        let image = match &item.backdrop.fill {
            FillOwned::Image(name) => {
                let dir = self
                    .backgrounds
                    .as_deref()
                    .ok_or(OverlayError::NoSuchImage)?;
                Some(self.images.load(dir, name)?)
            }
            _ => None,
        };
        let found = self.find(owner, key);
        let since =
            (found.and_then(|id| self.items.get(&id))).map_or_else(Instant::now, |e| e.look.since);
        let id = match found {
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
            backdrop: item.backdrop.clone(),
            image,
            until,
            since,
            drawn: Cell::new(false),
        });
        let entry = Entry {
            owner,
            key: key.to_owned(),
            target,
            mute: item.mute,
            report: item.report_shown,
            item,
            look,
        };
        self.items.insert(id, entry);
        self.images
            .keep(self.items.values().filter_map(|e| e.look.image.as_ref()));
        Ok(id)
    }

    /// Draws only the items `filter` matches; `None`: all.
    pub fn set_show_only(&mut self, filter: Option<ShowOnlyOwned>) {
        self.show_only = filter;
    }

    pub fn show_only(&self) -> Option<&ShowOnlyOwned> {
        self.show_only.as_ref()
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

    /// A frame for the screen starts: no item is drawn in it yet.
    pub fn frame_started(&self) {
        for e in self.items.values().filter(|e| e.report) {
            e.look.drawn.set(false);
        }
    }

    /// The items whose put asked to be reported and that the frame just rendered drew, as
    /// (owner, id); each put is reported once.
    pub fn take_shown(&mut self) -> Vec<(u64, u64)> {
        let mut shown = Vec::new();
        for (id, e) in &mut self.items {
            if e.report && e.look.drawn.get() {
                e.report = false;
                shown.push((e.owner, *id));
            }
        }
        shown
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
        let filter = self.show_only.as_ref();
        let mut looks: Vec<Rc<Look>> = (self.items.values())
            .filter(|e| on(&e.target) && e.shown_by(filter))
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
                hidden: !e.shown_by(self.show_only.as_ref()),
                item: e.item.clone(),
            })
            .collect()
    }

    /// When the next item ends, a countdown or a progress shows the next second or a clock the
    /// next minute, after `now`; `wall` is the wall clock at `now`.
    pub fn next_wake(&self, now: Instant, wall: SystemTime) -> Option<Instant> {
        let looks = self.items.values().map(|e| &e.look);
        let counts = (looks.clone())
            .filter(|look| matches!(look.content, ContentOwned::Progress { .. }))
            .map(|look| next_count(look.since, now));
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
        ends.chain(counts).chain(minute).min()
    }
}

/// The next whole second counted from `since` after `now`, where a progress changes.
fn next_count(since: Instant, now: Instant) -> Instant {
    let into = now.saturating_duration_since(since);
    now + (Duration::from_secs(1) - Duration::from_nanos(u64::from(into.subsec_nanos())))
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

/// What the compositor tells the owners of `shown` items, as (owner, id): a frame that drew them
/// reached the screen at `at`, on `CLOCK_MONOTONIC`.
pub fn shown(hub: &super::hub::Hub, shown: &[(u64, u64)], at: Duration) {
    let at_us = u64::try_from(at.as_micros()).expect("the monotonic clock fits 584 000 years");
    for &(owner, id) in shown {
        debug!("ww: overlay item {id} shown at {at_us} us");
        let event = ww_niri_api::EventOwned::OverlayShown {
            id: ItemId(id),
            at: exact_units::Fixed::new(at_us),
        };
        hub.emit_to(owner, &event);
    }
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
