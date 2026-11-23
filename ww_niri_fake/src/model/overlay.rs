//! The overlay's items, each owned by the connection that put it up: put, changed, lifted at
//! their end, on the owner's `lift`, on a `dismiss`, when the owner's connection ends or when
//! their window closes. While one asks for `mute` the sink is muted, and it gets its earlier
//! mute back when the last such item lifts. A put with `report_shown` is shown at once: its
//! owner hears `OverlayShown` at the fake's clock. A fill's image must be one of the fake's
//! backgrounds. The `show_only` filter marks the items it hides in `list`. A change of volume or
//! mute puts up the compositor's own volume indicator, as ww_niri does.

use ww_niri_api::{
    Anchor, At, BackdropOwned, ContentOwned, EventOwned, FillOwned, ItemEnd, ItemId, ItemInfoOwned,
    ItemOwned, OverlayError, PlaceOwned, Px, ShowOnlyOwned, SurfaceOwned, WindowId, ITEMS_MAX,
    ITEMS_PER_OWNER_MAX,
};

use super::{Mark, Model, Out};

/// The owner of the compositor's own items; connections are numbered from 1.
pub const COMPOSITOR: u64 = 0;
/// The key of the volume indicator.
pub const VOLUME_KEY: &str = "volume";

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
        if let FillOwned::Image(name) = &item.backdrop.fill {
            if !self.backgrounds.contains(name) {
                return Err(OverlayError::NoSuchImage);
            }
        }
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
                hidden: (self.show_only.as_ref())
                    .is_some_and(|f| !f.shows(i.item.priority, &i.key, i.id)),
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
                self.set_level(self.volume, true);
            }
            (false, Some(before)) => {
                self.mute_held = None;
                self.set_level(self.volume, before);
            }
            _ => {}
        }
    }

    /// Draws only the items `filter` matches; `None`: all.
    pub(crate) fn set_show_only(&mut self, filter: Option<ShowOnlyOwned>) {
        self.show_only = filter;
        self.mark(Mark::ShowOnly);
    }

    pub(crate) fn show_only(&self) -> Option<ShowOnlyOwned> {
        self.show_only.clone()
    }

    /// Adds an image to the backgrounds folder.
    pub(crate) fn add_background(&mut self, name: &str) {
        self.backgrounds.insert(name.to_owned());
    }

    /// The compositor's volume indicator, top right, for 1.5 s.
    pub(super) fn show_volume(&mut self) {
        let label = if self.muted { "Muted" } else { "Volume" };
        let item = ItemOwned {
            place: PlaceOwned {
                on: SurfaceOwned::Output(None),
                at: At::Anchor {
                    anchor: Anchor::TopRight,
                    dx: Px::new(-40),
                    dy: Px::new(40),
                },
            },
            content: ContentOwned::Meter {
                label: label.to_owned(),
                percent: self.volume,
            },
            backdrop: BackdropOwned {
                blur: 2,
                dim: 70,
                ..BackdropOwned::default()
            },
            priority: 224,
            lasts: Some(exact_units::Fixed::new(1500)),
            mute: false,
            dismissible: false,
            report_shown: false,
        };
        if let Err(err) = self.overlay_put(COMPOSITOR, VOLUME_KEY.to_owned(), item) {
            unreachable!("the volume indicator is a valid item and the compositor's only: {err:?}");
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
