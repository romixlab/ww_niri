//! `ww.kdl`: where the control API listens and how many browser slots there are.
//!
//! ```kdl
//! listen "100.98.201.15:7590"
//! listen "192.168.1.95:7590"
//! browsers 2
//! ```

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context as _};
use directories::ProjectDirs;

/// Browser slots when `ww.kdl` doesn't say.
pub const DEFAULT_BROWSERS: u8 = 2;

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq, Eq)]
struct RawConfig {
    #[knuffel(children(name = "listen"), unwrap(argument))]
    listen: Vec<String>,
    #[knuffel(child, unwrap(argument))]
    browsers: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WwConfig {
    /// Each address the server listens on, in the file's order without repeats; none: it
    /// doesn't listen.
    pub listen: Vec<SocketAddr>,
    pub browsers: u8,
}

impl Default for WwConfig {
    fn default() -> Self {
        Self {
            listen: Vec::new(),
            browsers: DEFAULT_BROWSERS,
        }
    }
}

impl WwConfig {
    pub fn parse(filename: &str, text: &str) -> anyhow::Result<Self> {
        let raw: RawConfig =
            knuffel::parse(filename, text).map_err(|err| anyhow::anyhow!("{err}"))?;

        let mut listen = Vec::new();
        for text in raw.listen {
            let addr: SocketAddr = text
                .parse()
                .with_context(|| format!("listen {text:?} is not an address with a port"))?;
            // The API runs commands as this user: it listens on the addresses named, never all.
            if addr.ip().is_unspecified() {
                bail!("listen {text:?} would listen on every address: name each address");
            }
            if !listen.contains(&addr) {
                listen.push(addr);
            }
        }

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
        assert_eq!(config.listen, ["100.64.0.12:7590".parse().unwrap()]);
        assert_eq!(config.browsers, 3);
    }

    #[test]
    fn several_listen_lines() {
        let text = "listen \"100.64.0.12:7590\"\nlisten \"192.168.1.95:7590\"\n\
                    listen \"100.64.0.12:7590\"\nlisten \"[fd7a::1]:7590\"\n";
        let config = WwConfig::parse("ww.kdl", text).unwrap();
        let want: Vec<SocketAddr> = ["100.64.0.12:7590", "192.168.1.95:7590", "[fd7a::1]:7590"]
            .iter()
            .map(|a| a.parse().unwrap())
            .collect();
        assert_eq!(config.listen, want);
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
        let text = "listen \"100.64.0.12:7590\"\nlisten \"0.0.0.0:7590\"\n";
        assert!(WwConfig::parse("ww.kdl", text).is_err());
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
