//! Where a [Tv](crate::Tv) or a [MotionSender](crate::MotionSender) runs its tasks.

use tokio::sync::watch;

use crate::error::StartError;

/// Runs `start` on the caller's tokio runtime when there is one, else on a runtime thread of its
/// own, which ends when the `stop` of what `start` made says so.
///
/// # Errors
///
/// What `start` answers, or [StartError::Runtime] when the runtime or its thread can't be made.
pub(crate) fn start<T>(
    stop: impl FnOnce(&T) -> watch::Receiver<bool>,
    start: impl FnOnce() -> Result<T, StartError>,
) -> Result<T, StartError> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return start();
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(StartError::Runtime)?;
    let started = {
        let _entered = runtime.enter();
        start()?
    };
    let mut stop = stop(&started);
    std::thread::Builder::new()
        .name("ww_niri_client".to_owned())
        .spawn(move || {
            runtime.block_on(async move {
                // Ends when the last handle is dropped; the runtime's tasks go with it.
                while !*stop.borrow_and_update() {
                    if stop.changed().await.is_err() {
                        return;
                    }
                }
            });
        })
        .map_err(StartError::Runtime)?;
    Ok(started)
}
