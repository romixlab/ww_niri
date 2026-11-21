//! The client against ww_niri_fake through the in-process transport.

mod commands;
mod connection;
mod model;
mod motion;

use std::future::Future;
use std::time::Duration;

use ww_niri_client::{Backoff, Config, KeepTrying, Snapshot, Tv};
use ww_niri_fake::{ClockMode, FakeTv, Options};

/// A fake on a manual clock: nothing happens by itself.
fn fake() -> FakeTv {
    FakeTv::start(Options {
        clock: ClockMode::Manual,
        ..Options::default()
    })
}

/// Retries fast: the in-process server is back within milliseconds.
fn config(tv: &FakeTv) -> Config {
    Config::in_process(tv.path()).keep_trying(KeepTrying {
        backoff: Backoff {
            first: Duration::from_millis(5),
            max: Duration::from_millis(50),
        },
        give_up: None,
    })
}

/// Runs a test on a current-thread runtime; the fake runs on its own thread.
fn run(test: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test);
}

/// Waits for a condition the test caused; the limit only ends a test that hangs.
async fn soon<T>(what: &str, f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), f)
        .await
        .unwrap_or_else(|_| panic!("{what} within 5 s"))
}

/// A connected TV.
async fn connect(fake: &FakeTv) -> Tv {
    let tv = Tv::start(config(fake)).unwrap();
    connected(&tv).await;
    tv
}

async fn connected(tv: &Tv) {
    let mut state = tv.watch_state();
    soon("connected", state.wait_for(|s| s.is_connected()))
        .await
        .unwrap();
}

/// Waits until the snapshot satisfies `f`.
async fn until(tv: &Tv, what: &str, f: impl Fn(&Snapshot) -> bool) -> std::sync::Arc<Snapshot> {
    let mut snapshots = tv.watch_snapshot();
    soon(what, snapshots.wait_for(|s| f(s)))
        .await
        .unwrap()
        .clone()
}

/// Waits until the fake's state satisfies `f`, polling.
async fn until_fake(what: &str, f: impl Fn() -> bool) {
    soon(what, async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
}
