//! Turns the global `--tv`/`--connect-timeout-ms` flags into a connected [Tv].

use std::time::Duration;

use anyhow::{Context, bail};
use clap::ArgMatches;
use ww_niri_client::{Config, Tv};

/// `--tv` default: alma-2tv's control API, over the tailnet so it works away from the LAN too.
pub const DEFAULT_TV: &str = "ws://alma-2tv:7590/ww";

/// `--tv URL` (env `WWN_TV`, else [DEFAULT_TV]).
pub fn tv_url(matches: &ArgMatches) -> String {
    matches
        .get_one::<String>("tv")
        .expect("--tv has a default")
        .clone()
}

/// How long [connect] waits for the link before giving up.
pub fn connect_timeout(matches: &ArgMatches) -> Duration {
    let ms = *matches
        .get_one::<u64>("connect_timeout_ms")
        .expect("--connect-timeout-ms has a default");
    Duration::from_millis(ms)
}

/// `--hold-limit-ms`: how long the TV keeps a held button or key without hearing from this
/// process; `wwn pointer press`/`key-down` renew it every third of this.
pub fn hold_limit(matches: &ArgMatches) -> Duration {
    let ms = *matches
        .get_one::<u64>("hold_limit_ms")
        .expect("--hold-limit-ms has a default");
    Duration::from_millis(ms)
}

/// `--json`: machine-readable output instead of text for people.
pub fn json(matches: &ArgMatches) -> bool {
    matches.get_flag("json")
}

/// Starts the connection and waits for it, or for `--connect-timeout-ms` to pass.
///
/// # Errors
///
/// When the address is malformed, or the TV doesn't answer within the timeout.
pub async fn connect(matches: &ArgMatches) -> anyhow::Result<Tv> {
    let url = tv_url(matches);
    let timeout = connect_timeout(matches);
    let config = Config::websocket(url.clone()).hold_limit(hold_limit(matches));
    let tv = Tv::start(config).with_context(|| format!("connecting to {url}"))?;
    let mut state = tv.watch_state();
    let connected = tokio::time::timeout(timeout, state.wait_for(|s| s.is_connected())).await;
    match connected {
        Ok(Ok(_)) => Ok(tv),
        Ok(Err(_)) => bail!("{url} will never answer"),
        Err(_elapsed) => bail!(
            "{url} did not answer within {}; is the TV on and reachable?",
            crate::output::human_duration(timeout)
        ),
    }
}
