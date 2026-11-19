//! The generated client of `ww_niri_api`, which [Tv](crate::Tv) wraps.

use wire_weaver_client::{ClientConfig, Commander, WwClient};

/// The generated client: the API's resources as methods, `client.browsers(0).open(url)`.
#[derive(Clone)]
pub(crate) struct NiriClient {
    pub(crate) cmd: Commander,
}

impl WwClient for NiriClient {
    fn default_config() -> ClientConfig {
        ClientConfig::new()
    }

    fn try_from_cmd(cmd: Commander) -> Result<Self, wire_weaver_client::Error> {
        Ok(Self { cmd })
    }
}

mod generated {
    wire_weaver::ww_codegen!(ww_niri_api :: WwNiri for super::NiriClient, client = "std_client");
}
