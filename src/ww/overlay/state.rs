//! The overlay on niri's `State`: windows get their items to draw, one timer wakes the loop at
//! each end, countdown second and clock minute, and the sink stays muted while an item asks.

use std::time::{Instant, SystemTime};

use calloop::timer::{TimeoutAction, Timer};
use ww_niri_api::{ItemEnd, ItemId, ItemInfoOwned, ItemOwned, OverlayError};

use super::{ended, Target};
use crate::niri::State;

impl State {
    /// Puts up `owner`'s item `key`; it is drawn from the next frame.
    pub(crate) fn ww_overlay_put(
        &mut self,
        owner: u64,
        key: &str,
        target: Target,
        item: ItemOwned,
        until: Option<Instant>,
    ) -> Result<ItemId, OverlayError> {
        if let Target::Window(window) = target {
            if !self.ww_window_ids().contains(&window) {
                return Err(OverlayError::NoSuchWindow);
            }
        }
        let id = self.niri.ww.overlay.put(owner, key, target, item, until)?;
        self.ww_overlay_changed();
        Ok(ItemId(id))
    }

    pub(crate) fn ww_overlay_lift(&mut self, owner: u64, key: &str) -> Result<(), OverlayError> {
        let id = (self.niri.ww.overlay.lift(owner, key)).ok_or(OverlayError::NoSuchItem)?;
        self.ww_overlay_ended(&[id], ItemEnd::Lifted);
        Ok(())
    }

    pub(crate) fn ww_overlay_dismiss(&mut self, id: ItemId) -> Result<(), OverlayError> {
        self.niri.ww.overlay.dismiss(id.0)?;
        self.ww_overlay_ended(&[id.0], ItemEnd::Dismissed);
        Ok(())
    }

    pub(crate) fn ww_overlay_lift_owned_by(&mut self, owner: u64) {
        let ids = self.niri.ww.overlay.lift_owned_by(owner);
        self.ww_overlay_ended(&ids, ItemEnd::Disconnected);
    }

    pub(crate) fn ww_overlay_list(&self, owner: u64) -> Vec<ItemInfoOwned> {
        self.niri.ww.overlay.list(owner)
    }

    /// Lifts the items of windows that closed.
    pub(crate) fn ww_overlay_refresh(&mut self) {
        if !self.niri.ww.overlay.on_windows() {
            return;
        }
        let ids = self.ww_window_ids();
        let gone = self.niri.ww.overlay.lift_gone(|w| ids.contains(&w));
        self.ww_overlay_ended(&gone, ItemEnd::TargetGone);
    }

    fn ww_window_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        self.niri
            .layout
            .with_windows(|mapped, _, _, _| ids.push(mapped.id().get()));
        ids
    }

    fn ww_overlay_ended(&mut self, ids: &[u64], why: ItemEnd) {
        if ids.is_empty() {
            return;
        }
        ended(&self.niri.ww.services().hub, ids, why);
        self.ww_overlay_changed();
    }

    /// Gives every window its items to draw, redraws, and follows the items' ends and mutes.
    fn ww_overlay_changed(&mut self) {
        let overlay = &self.niri.ww.overlay;
        self.niri.layout.with_windows_mut(|mapped, _| {
            let looks = overlay.on_window(mapped.id().get());
            mapped.set_overlay(looks);
        });
        self.niri.queue_redraw_all();
        self.ww_arm_overlay_timer();
        let mutes = self.niri.ww.overlay.mutes();
        self.niri.ww.services().audio.hold_mute(mutes);
    }

    /// One timer for every item: at each end, countdown second and clock minute.
    fn ww_arm_overlay_timer(&mut self) {
        if let Some(token) = self.niri.ww.overlay_timer.take() {
            self.niri.event_loop.remove(token);
        }
        let wake = self
            .niri
            .ww
            .overlay
            .next_wake(Instant::now(), SystemTime::now());
        let Some(wake) = wake else {
            return;
        };
        let timer = Timer::from_deadline(wake);
        let token = self
            .niri
            .event_loop
            .insert_source(timer, |_, _, state| {
                state.niri.ww.overlay_timer = None;
                state.ww_overlay_tick();
                TimeoutAction::Drop
            })
            .expect("the loop takes a timer");
        self.niri.ww.overlay_timer = Some(token);
    }

    fn ww_overlay_tick(&mut self) {
        let due = self.niri.ww.overlay.lift_due(Instant::now());
        ended(&self.niri.ww.services().hub, &due, ItemEnd::Deadline);
        // Countdowns and clocks move on, ended or not.
        self.ww_overlay_changed();
    }
}
