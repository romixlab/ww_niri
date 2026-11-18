//! The fake TV's time: real for demos, moved by the test for tests.

use std::time::{Duration, Instant};

use tokio::sync::watch;

/// How the fake's time passes: slot start delays, playing positions and hold limits follow it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ClockMode {
    /// Time passes as it does: for demos and a UI tried by hand.
    #[default]
    Real,
    /// Time stands until [FakeTv::advance](crate::FakeTv::advance): for tests.
    Manual,
}

pub(crate) enum Clock {
    Real(Instant),
    /// Microseconds since the start.
    Manual(watch::Sender<u64>),
}

impl Clock {
    pub(crate) fn new(mode: ClockMode) -> Self {
        match mode {
            ClockMode::Real => Self::Real(Instant::now()),
            ClockMode::Manual => Self::Manual(watch::Sender::new(0)),
        }
    }

    /// Microseconds since the fake started.
    pub(crate) fn now_us(&self) -> u64 {
        match self {
            Self::Real(start) => u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX),
            Self::Manual(now) => *now.borrow(),
        }
    }

    /// Moves a manual clock on; `false` for a real one, which can't be moved.
    pub(crate) fn advance(&self, by: Duration) -> bool {
        match self {
            Self::Real(_) => false,
            Self::Manual(now) => {
                let by = u64::try_from(by.as_micros()).unwrap_or(u64::MAX);
                now.send_modify(|t| *t = t.saturating_add(by));
                true
            }
        }
    }

    /// Resolves once the clock is at `at_us`; never for `None`.
    pub(crate) async fn until(&self, at_us: Option<u64>) {
        let Some(at_us) = at_us else {
            return std::future::pending().await;
        };
        match self {
            Self::Real(start) => {
                let at = *start + Duration::from_micros(at_us);
                tokio::time::sleep_until(at.into()).await;
            }
            Self::Manual(now) => {
                let mut rx = now.subscribe();
                rx.wait_for(|t| *t >= at_us)
                    .await
                    .expect("the sender lives as long as the clock");
            }
        }
    }
}
