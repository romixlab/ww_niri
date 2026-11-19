//! `wwn`'s commands against `ww_niri_fake` through the in-process transport.

mod cli;
mod registered_commands;

use std::ffi::OsString;
use std::future::Future;
use std::time::Duration;

use clap::ArgMatches;
use ww_niri_client::{Backoff, Config, KeepTrying, Tv};
use ww_niri_fake::{ClockMode, FakeTv, Options};

fn fake() -> FakeTv {
    FakeTv::start(Options {
        clock: ClockMode::Manual,
        ..Options::default()
    })
}

fn config(tv: &FakeTv) -> Config {
    Config::in_process(tv.path()).keep_trying(KeepTrying {
        backoff: Backoff {
            first: Duration::from_millis(5),
            max: Duration::from_millis(50),
        },
        give_up: None,
    })
}

fn run(test: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test);
}

async fn soon<T>(what: &str, f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), f)
        .await
        .unwrap_or_else(|_| panic!("{what} within 5 s"))
}

async fn connect(fake: &FakeTv) -> Tv {
    let tv = Tv::start(config(fake)).unwrap();
    let mut state = tv.watch_state();
    soon("connected", state.wait_for(|s| s.is_connected()))
        .await
        .unwrap();
    tv
}

/// `wwn`'s own command line, parsed for a test: no `--tv`/connect flags needed, since the test
/// connects itself.
fn matches<'a>(words: impl IntoIterator<Item = &'a str>) -> ArgMatches {
    let args: Vec<OsString> = std::iter::once("wwn")
        .chain(words)
        .map(OsString::from)
        .collect();
    ww_niri_cli::root_command()
        .try_get_matches_from(args)
        .expect("the test's command line parses")
}

/// Runs `words` against `fake` through the real dispatch: the registry, clap parsing and the
/// typed client call.
async fn wwn(fake: &FakeTv, words: &[&str]) -> anyhow::Result<()> {
    let tv = connect(fake).await;
    ww_niri_cli::dispatch(&tv, &matches(words.iter().copied())).await
}
