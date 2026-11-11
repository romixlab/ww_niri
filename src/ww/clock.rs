//! [ww_device::Clock] on the calloop loop's timers.

use std::future::Future;
use std::time::{Duration, Instant};

use calloop::timer::TimeoutFuture;
use calloop::LoopHandle;

pub struct LoopClock<D: 'static> {
    handle: LoopHandle<'static, D>,
    start: Instant,
}

impl<D: 'static> LoopClock<D> {
    pub fn new(handle: LoopHandle<'static, D>) -> Self {
        Self {
            handle,
            start: Instant::now(),
        }
    }
}

impl<D: 'static> ww_device::Clock for LoopClock<D> {
    fn now(&self) -> ww_device::Instant {
        ww_device::Instant::from_micros(self.start.elapsed().as_micros() as u64)
    }

    fn wait_until(&self, at: ww_device::Instant) -> impl Future<Output = ()> {
        let deadline = self.start + Duration::from_micros(at.as_micros());
        TimeoutFuture::from_deadline(&self.handle, deadline)
    }
}
