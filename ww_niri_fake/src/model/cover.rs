//! Covers over windows: set, renewed, lifted at their end, on `uncover`, when the connection
//! ends or when the window closes. While one asks for `mute` the sink is muted, and it gets its
//! earlier mute back when the last such cover lifts.

use exact_units::Millis;
use ww_niri_api::{CardOwned, CoverEnd, CoverError, EventOwned, WindowId};

use super::Model;

/// A window's cover as the fake has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cover {
    pub window: WindowId,
    pub card: CardOwned,
    /// When it lifts by itself, on the fake's clock.
    pub until_us: u64,
    /// It mutes the sink while it holds.
    pub mute: bool,
    /// The connection that set it, or renewed it last.
    pub(crate) holder: u64,
}

impl Model {
    /// Covers the window for connection `holder`.
    pub(crate) fn cover(
        &mut self,
        holder: u64,
        window: u64,
        card: CardOwned,
        lasts: Millis<u32>,
        mute: bool,
    ) -> Result<(), CoverError> {
        let lasts = card.check_cover(lasts)?;
        if self.window(window).is_none() {
            return Err(CoverError::NoSuchWindow);
        }
        let lasts_us =
            u64::try_from(lasts.as_micros()).expect("a checked cover lasts 15 min at most");
        let until_us = self.now_us + lasts_us;
        self.covers.insert(
            window,
            Cover {
                window: WindowId(window),
                card,
                until_us,
                mute,
                holder,
            },
        );
        self.hold_mute();
        Ok(())
    }

    pub(crate) fn uncover(&mut self, window: u64) -> Result<(), CoverError> {
        if self.window(window).is_none() {
            return Err(CoverError::NoSuchWindow);
        }
        self.lift(window, CoverEnd::Uncovered)
            .then_some(())
            .ok_or(CoverError::NotCovered)
    }

    /// The window of a browser slot, for its cover.
    pub(crate) fn slot_window(&self, slot: u8) -> Result<u64, CoverError> {
        (self.slot(slot).and_then(|s| s.window))
            .filter(|id| self.window(*id).is_some())
            .ok_or(CoverError::NoSuchWindow)
    }

    pub(crate) fn covers(&self) -> Vec<Cover> {
        self.covers.values().cloned().collect()
    }

    /// Lifts the window's cover; whether it had one.
    pub(super) fn lift(&mut self, window: u64, why: CoverEnd) -> bool {
        let lifted = self.covers.remove(&window).is_some();
        if lifted {
            self.event(EventOwned::CoverLifted {
                window: WindowId(window),
                why,
            });
            self.hold_mute();
        }
        lifted
    }

    /// Mutes the sink when the first muting cover starts; sets its earlier mute again when the
    /// last one lifts.
    fn hold_mute(&mut self) {
        let mutes = self.covers.values().any(|c| c.mute);
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

    pub(super) fn lift_due_covers(&mut self) {
        let now = self.now_us;
        let due: Vec<u64> = (self.covers.values())
            .filter(|c| c.until_us <= now)
            .map(|c| c.window.0)
            .collect();
        for window in due {
            self.lift(window, CoverEnd::Deadline);
        }
    }

    /// Connection `holder` ended: its covers lift.
    pub(super) fn lift_covers_held_by(&mut self, holder: u64) {
        let windows: Vec<u64> = (self.covers.values())
            .filter(|c| c.holder == holder)
            .map(|c| c.window.0)
            .collect();
        for window in windows {
            self.lift(window, CoverEnd::Disconnected);
        }
    }

    pub(super) fn next_cover_end(&self) -> Option<u64> {
        self.covers.values().map(|c| c.until_us).min()
    }
}
