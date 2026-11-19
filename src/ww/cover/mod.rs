//! Covers over windows: a blur of what the window shows, dimmed, under a card with a title, a
//! countdown and a clock (`windows.cover` and `browsers[].cover` of the API).
//!
//! A cover is a lease: [Covers] keeps which connection holds each and when it ends, and lifts it
//! at the end, on `uncover`, when the holder's connection ends or when the window closes. The
//! window carries what to draw ([Look]); [render] draws it in the tile instead of the window.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ww_niri_api::{CardOwned, CoverEnd};

pub mod render;

/// What a covered window draws instead of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Look {
    pub card: CardOwned,
    /// The countdown reaches 0:00 here.
    pub until: Instant,
}

impl Look {
    /// The countdown line at `now`: the card's text, then the time left as `m:ss`, rounded up.
    pub fn countdown(&self, now: Instant) -> Option<String> {
        let label = self.card.countdown.as_deref()?;
        let left = self.until.saturating_duration_since(now);
        let secs = left.as_millis().div_ceil(1000);
        Some(format!("{label} {}:{:02}", secs / 60, secs % 60))
    }
}

#[derive(Debug)]
struct Lease {
    /// The connection that set it last.
    holder: u64,
    look: Rc<Look>,
}

/// The covers by window id, and who holds each.
#[derive(Debug, Default)]
pub struct Covers {
    leases: HashMap<u64, Lease>,
}

impl Covers {
    /// Covers `window` for `holder` until `look.until`, replacing a cover it has; answers what
    /// the window draws.
    pub fn set(&mut self, window: u64, holder: u64, look: Look) -> Rc<Look> {
        let look = Rc::new(look);
        self.leases.insert(
            window,
            Lease {
                holder,
                look: look.clone(),
            },
        );
        look
    }

    /// Lifts the cover of `window`; whether it had one.
    pub fn lift(&mut self, window: u64) -> bool {
        self.leases.remove(&window).is_some()
    }

    /// Lifts the covers `holder` holds; answers their windows.
    pub fn lift_held_by(&mut self, holder: u64) -> Vec<u64> {
        self.lift_where(|_, lease| lease.holder == holder)
    }

    /// Lifts the covers whose end is at or before `now`; answers their windows.
    pub fn lift_due(&mut self, now: Instant) -> Vec<u64> {
        self.lift_where(|_, lease| lease.look.until <= now)
    }

    /// Lifts the covers of windows `alive` denies; answers them.
    pub fn lift_gone(&mut self, alive: impl Fn(u64) -> bool) -> Vec<u64> {
        self.lift_where(|window, _| !alive(window))
    }

    fn lift_where(&mut self, lifts: impl Fn(u64, &Lease) -> bool) -> Vec<u64> {
        let mut lifted: Vec<u64> = self
            .leases
            .iter()
            .filter(|(window, lease)| lifts(**window, lease))
            .map(|(window, _)| *window)
            .collect();
        lifted.sort_unstable();
        for window in &lifted {
            self.leases.remove(window);
        }
        lifted
    }

    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }

    /// When the next cover ends or a countdown shows the next second, after `now`.
    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        self.leases
            .values()
            .map(|lease| next_tick(lease.look.until, now))
            .min()
    }
}

/// The next whole second before `until` after `now`, where the countdown changes, or `until`.
fn next_tick(until: Instant, now: Instant) -> Instant {
    let left = until.saturating_duration_since(now);
    let past_second = Duration::from_nanos((left.as_nanos() % 1_000_000_000) as u64);
    if past_second.is_zero() {
        now + left.min(Duration::from_secs(1))
    } else {
        now + past_second
    }
}

/// What the compositor does when a cover goes: the window drops its look, the clients hear why.
pub fn lifted(hub: &super::hub::Hub, window: u64, why: CoverEnd) {
    debug!("ww: cover of window {window} lifted: {why:?}");
    hub.emit(ww_niri_api::EventOwned::CoverLifted {
        window: ww_niri_api::WindowId(window),
        why,
    });
}

mod niri_state {
    use std::rc::Rc;
    use std::time::Instant;

    use calloop::timer::{TimeoutAction, Timer};
    use ww_niri_api::{CoverEnd, CoverError};

    use super::{lifted, Look};
    use crate::niri::State;
    use crate::window::mapped::MappedId;

    impl State {
        /// Covers the window; the window draws `look` from the next frame.
        pub(crate) fn ww_cover(
            &mut self,
            window: u64,
            holder: u64,
            look: Look,
        ) -> Result<(), CoverError> {
            if !self.ww_window_ids().contains(&window) {
                return Err(CoverError::NoSuchWindow);
            }
            let look = self.niri.ww.covers.set(window, holder, look);
            self.ww_set_look(window, Some(look));
            self.ww_arm_cover_timer();
            Ok(())
        }

        pub(crate) fn ww_uncover(&mut self, window: u64) -> Result<(), CoverError> {
            if !self.niri.ww.covers.lift(window) {
                return Err(if self.ww_window_ids().contains(&window) {
                    CoverError::NotCovered
                } else {
                    CoverError::NoSuchWindow
                });
            }
            self.ww_lifted(&[window], CoverEnd::Uncovered);
            Ok(())
        }

        pub(crate) fn ww_uncover_held_by(&mut self, holder: u64) {
            let windows = self.niri.ww.covers.lift_held_by(holder);
            self.ww_lifted(&windows, CoverEnd::Disconnected);
        }

        /// Gives the window `look` to draw, or none; whether the window exists.
        fn ww_set_look(&mut self, window: u64, look: Option<Rc<Look>>) -> bool {
            let mut look = Some(look);
            let mut found = None;
            self.niri.layout.with_windows_mut(|mapped, output| {
                if mapped.id() == MappedId::specific(window) {
                    if let Some(look) = look.take() {
                        mapped.set_cover(look);
                    }
                    found = Some(output.cloned());
                }
            });
            match found {
                Some(Some(output)) => self.niri.queue_redraw(&output),
                Some(None) => self.niri.queue_redraw_all(),
                None => return false,
            }
            true
        }

        fn ww_window_ids(&self) -> Vec<u64> {
            let mut ids = Vec::new();
            self.niri
                .layout
                .with_windows(|mapped, _, _, _| ids.push(mapped.id().get()));
            ids
        }

        fn ww_lifted(&mut self, windows: &[u64], why: CoverEnd) {
            let hub = self.niri.ww.services().hub.clone();
            for &window in windows {
                self.ww_set_look(window, None);
                lifted(&hub, window, why);
            }
            self.ww_arm_cover_timer();
        }

        /// One timer for every cover: at each end and each second of the countdowns.
        fn ww_arm_cover_timer(&mut self) {
            if let Some(token) = self.niri.ww.cover_timer.take() {
                self.niri.event_loop.remove(token);
            }
            let Some(wake) = self.niri.ww.covers.next_wake(Instant::now()) else {
                return;
            };
            let timer = Timer::from_deadline(wake);
            let token = self
                .niri
                .event_loop
                .insert_source(timer, |_, _, state| {
                    state.niri.ww.cover_timer = None;
                    state.ww_cover_tick();
                    TimeoutAction::Drop
                })
                .expect("the loop takes a timer");
            self.niri.ww.cover_timer = Some(token);
        }

        fn ww_cover_tick(&mut self) {
            let due = self.niri.ww.covers.lift_due(Instant::now());
            let ids = self.ww_window_ids();
            let gone = self.niri.ww.covers.lift_gone(|w| ids.contains(&w));
            let hub = self.niri.ww.services().hub.clone();
            for window in gone {
                lifted(&hub, window, CoverEnd::WindowClosed);
            }
            self.ww_lifted(&due, CoverEnd::Deadline);
            // The countdowns and clocks move on.
            if !self.niri.ww.covers.is_empty() {
                self.niri.queue_redraw_all();
            }
        }
    }
}

#[cfg(test)]
mod tests;
