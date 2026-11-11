//! `ww.kdl`: where the control API listens and how many browser slots there are.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context as _};
use directories::ProjectDirs;

/// Browser slots when `ww.kdl` doesn't say.
pub const DEFAULT_BROWSERS: u8 = 2;

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq, Eq)]
struct RawConfig {
    #[knuffel(child, unwrap(argument))]
    listen: Option<String>,
    #[knuffel(child, unwrap(argument))]
    browsers: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WwConfig {
    /// `None`: the server doesn't listen.
    pub listen: Option<SocketAddr>,
    pub browsers: u8,
}

impl Default for WwConfig {
    fn default() -> Self {
        Self {
            listen: None,
            browsers: DEFAULT_BROWSERS,
        }
    }
}

impl WwConfig {
    pub fn parse(filename: &str, text: &str) -> anyhow::Result<Self> {
        let raw: RawConfig =
            knuffel::parse(filename, text).map_err(|err| anyhow::anyhow!("{err}"))?;

        let listen = match raw.listen {
            Some(listen) => {
                let addr: SocketAddr = listen
                    .parse()
                    .with_context(|| format!("listen {listen:?} is not an address with a port"))?;
                // The API runs commands as this user: it listens on one address, never all of them.
                if addr.ip().is_unspecified() {
                    bail!(
                        "listen {listen:?} would listen on every address: give the tailnet address"
                    );
                }
                Some(addr)
            }
            None => None,
        };

        Ok(Self {
            listen,
            browsers: raw.browsers.unwrap_or(DEFAULT_BROWSERS),
        })
    }
}

/// `~/.config/niri/ww.kdl`, next to niri's own config.
pub fn default_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("", "", "niri")?;
    Some(dirs.config_dir().join("ww.kdl"))
}

/// What tells a changed file from an unchanged one, as niri's config watcher does: the
/// modification time and the canonical path (for a symlink whose target changes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Props {
    mtime: SystemTime,
    canonical: PathBuf,
}

impl Props {
    pub fn of(path: &Path) -> Option<Self> {
        let canonical = path.canonicalize().ok()?;
        let mtime = canonical.metadata().ok()?.modified().ok()?;
        Some(Self { mtime, canonical })
    }
}

/// Reads and parses `path`; a missing file is the default config (no listener).
pub fn load(path: &Path) -> anyhow::Result<WwConfig> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(WwConfig::default()),
        Err(err) => return Err(err).with_context(|| format!("error reading {path:?}")),
    };
    WwConfig::parse(&path.to_string_lossy(), &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_and_browsers() {
        let config =
            WwConfig::parse("ww.kdl", "listen \"100.64.0.12:7590\"\nbrowsers 3\n").unwrap();
        assert_eq!(config.listen, Some("100.64.0.12:7590".parse().unwrap()));
        assert_eq!(config.browsers, 3);
    }

    #[test]
    fn empty_is_default() {
        let config = WwConfig::parse("ww.kdl", "").unwrap();
        assert_eq!(config, WwConfig::default());
        assert_eq!(config.browsers, DEFAULT_BROWSERS);
    }

    #[test]
    fn refuses_every_address() {
        assert!(WwConfig::parse("ww.kdl", "listen \"0.0.0.0:7590\"").is_err());
        assert!(WwConfig::parse("ww.kdl", "listen \"[::]:7590\"").is_err());
    }

    #[test]
    fn refuses_address_without_port() {
        assert!(WwConfig::parse("ww.kdl", "listen \"100.64.0.12\"").is_err());
    }

    #[test]
    fn refuses_unknown_node() {
        assert!(WwConfig::parse("ww.kdl", "apps 1").is_err());
    }
}
