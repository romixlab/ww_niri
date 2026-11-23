//! The overlay's items, each owned by the connection that put it up: put, changed, lifted at
//! their end, on the owner's `lift`, on a `dismiss`, when the owner's connection ends or when
//! their window closes. While one asks for `mute` the sink is muted, and it gets its earlier
//! mute back when the last such item lifts. A put with `report_shown` is shown at once: its
//! owner hears `OverlayShown` at the fake's clock.

use ww_niri_api::{
    EventOwned, ItemEnd, ItemId, ItemInfoOwned, ItemOwned, OverlayError, SurfaceOwned, WindowId,
    ITEMS_MAX, ITEMS_PER_OWNER_MAX,
};

use super::{Model, Out};

/// An overlay item as the fake has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayItem {
    pub id: ItemId,
    /// The connection that owns it.
    pub owner: u64,
    pub key: String,
    /// The window it is on, a slot's found at its `put`.
    pub window: Option<WindowId>,
    pub item: ItemOwned,
    /// When it lifts by itself, on the fake's clock.
    pub until_us: Option<u64>,
}

impl OverlayItem {
    /// The order of the stack: higher is in front.
    fn rank(&self) -> (u8, ItemId) {
        (self.item.priority, self.id)
    }
}

impl Model {
    /// Puts up `owner`'s item `key`, or changes it.
    pub(crate) fn overlay_put(
        &mut self,
        owner: u64,
        key: String,
        item: ItemOwned,
    ) -> Result<ItemId, OverlayError> {
        let lasts = item.check(&key)?;
        let window = match &item.place.on {
            SurfaceOwned::Window(id) => Some(*id),
            SurfaceOwned::Slot(slot) => Some(WindowId(
                (self.slot(slot.0).and_then(|s| s.window)).ok_or(OverlayError::NoSuchWindow)?,
            )),
            SurfaceOwned::Output(_) => None,
            SurfaceOwned::Unknown(_) => unreachable!("check refuses an unknown surface"),
        };
        if window.is_some_and(|w| self.window(w.0).is_none()) {
            return Err(OverlayError::NoSuchWindow);
        }
        let found = self
            .overlay
            .values()
            .find(|i| i.owner == owner && i.key == key);
        let id = match found {
            Some(found) => found.id,
            None => {
                let owned = self.overlay.values().filter(|i| i.owner == owner).count();
                if owned >= ITEMS_PER_OWNER_MAX || self.overlay.len() >= ITEMS_MAX {
                    return Err(OverlayError::TooMany);
                }
                self.next_item_id += 1;
                ItemId(self.next_item_id)
            }
        };
        let until_us = lasts.map(|lasts| {
            self.now_us
                + u64::try_from(lasts.as_micros()).expect("a checked item lasts 15 min at most")
        });
        let report = item.report_shown;
        self.overlay.insert(
            id.0,
            OverlayItem {
                id,
                owner,
                key,
                window,
                item,
                until_us,
            },
        );
        self.hold_mute();
        if report {
            let at = exact_units::Fixed::new(self.now_us);
            self.push(Out::EventTo(owner, EventOwned::OverlayShown { id, at }));
        }
        Ok(id)
    }

    pub(crate) fn overlay_lift(&mut self, owner: u64, key: &str) -> Result<(), OverlayError> {
        let id = (self.overlay.values())
            .find(|i| i.owner == owner && i.key == key)
            .map(|i| i.id.0)
            .ok_or(OverlayError::NoSuchItem)?;
        self.lift_item(id, ItemEnd::Lifted);
        Ok(())
    }

    pub(crate) fn overlay_dismiss(&mut self, id: ItemId) -> Result<(), OverlayError> {
        let item = self.overlay.get(&id.0).ok_or(OverlayError::NoSuchItem)?;
        if !item.item.dismissible {
            return Err(OverlayError::NotDismissible);
        }
        self.lift_item(id.0, ItemEnd::Dismissed);
        Ok(())
    }

    /// Every item, front to back.
    pub(crate) fn overlay(&self) -> Vec<OverlayItem> {
        let mut items: Vec<OverlayItem> = self.overlay.values().cloned().collect();
        items.sort_by_key(|i| std::cmp::Reverse(i.rank()));
        items
    }

    /// Every item, front to back, as `owner` sees them.
    pub(crate) fn overlay_list(&self, owner: u64) -> Vec<ItemInfoOwned> {
        (self.overlay().into_iter())
            .map(|i| ItemInfoOwned {
                id: i.id,
                key: i.key,
                own: i.owner == owner,
                item: i.item,
            })
            .collect()
    }

    fn lift_item(&mut self, id: u64, why: ItemEnd) {
        if self.overlay.remove(&id).is_some() {
            self.event(EventOwned::OverlayEnded {
                id: ItemId(id),
                why,
            });
            self.hold_mute();
        }
    }

    fn lift_items(&mut self, lifts: impl Fn(&OverlayItem) -> bool, why: ItemEnd) {
        let ids: Vec<u64> = (self.overlay.values())
            .filter(|i| lifts(i))
            .map(|i| i.id.0)
            .collect();
        for id in ids {
            self.lift_item(id, why);
        }
    }

    /// Mutes the sink when the first muting item goes up; sets its earlier mute again when the
    /// last one lifts.
    fn hold_mute(&mut self) {
        let mutes = self.overlay.values().any(|i| i.item.mute);
        match (mutes, self.mute_held) {
            (true, None) => {
                self.mute_held = Some(self.muted);
                self.set_audio(self.volume, true);
            }
            (false, Some(before)) => {
                self.mute_held = None;
                self.set_audio(self.volume, before);
            }
            _ => {}
        }
    }

    pub(super) fn lift_due_items(&mut self) {
        let now = self.now_us;
        self.lift_items(|i| i.until_us.is_some_and(|u| u <= now), ItemEnd::Deadline);
    }

    /// Connection `owner` ended: its items lift.
    pub(super) fn lift_items_owned_by(&mut self, owner: u64) {
        self.lift_items(|i| i.owner == owner, ItemEnd::Disconnected);
    }

    /// The window closed: its items lift.
    pub(super) fn lift_items_on(&mut self, window: u64) {
        self.lift_items(|i| i.window == Some(WindowId(window)), ItemEnd::TargetGone);
    }

    pub(super) fn next_item_end(&self) -> Option<u64> {
        self.overlay.values().filter_map(|i| i.until_us).min()
    }
}
