//! A generated client against the fake TV through the in-process transport.

mod every_method;
mod hash_moved;
mod hide;
mod knobs;
mod links;
mod observers;
mod overlay;

use std::future::Future;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use wire_weaver_client::snapshots::Resolver;
use wire_weaver_client::{ClientConfig, Commander, WwClient};
use ww_niri_fake::{ClockMode, FakeTv, Options};

pub struct NiriClient {
    cmd: Commander,
}

impl WwClient for NiriClient {
    fn default_config() -> ClientConfig {
        ClientConfig::new()
    }

    fn try_from_cmd(cmd: Commander) -> Result<Self, wire_weaver_client::Error> {
        Ok(Self { cmd })
    }
}

mod api_client {
    wire_weaver::ww_codegen!(ww_niri_api :: WwNiri for super::NiriClient, client = "std_client");
}

/// Snapshots from this workspace's build only: nothing of the host's `~/.wire_weaver`, no fetch.
fn resolver() -> &'static Resolver {
    static RESOLVER: LazyLock<Resolver> =
        LazyLock::new(|| Resolver::offline(Path::new(env!("CARGO_MANIFEST_DIR"))));
    &RESOLVER
}

/// A fake on a manual clock: nothing happens by itself.
fn fake() -> FakeTv {
    FakeTv::start(Options {
        clock: ClockMode::Manual,
        ..Options::default()
    })
}

async fn connect(tv: &FakeTv) -> anyhow::Result<NiriClient> {
    NiriClient::config(|c| c.in_process_path(tv.path().to_owned()).resolver(resolver()))
        .connect()
        .await
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
