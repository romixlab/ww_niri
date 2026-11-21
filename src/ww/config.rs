//! `ww.kdl`: where the control API listens, the name it is announced under and how many browser
//! slots there are.
//!
//! ```kdl
//! listen "lan:7590"
//! listen "tailnet:7590"
//! listen "127.0.0.1:7591"
//! advertise "Living room TV"
//! browsers 2
//! ```
//!
//! `lan:PORT` is the host's LAN address and `tailnet:PORT` its tailnet address, whatever they are
//! now (see [`Interfaces`]); a literal address names itself. Each `lan:` listener is announced
//! on the LAN over DNS-SD under the `advertise` name, the host name when it is left out. Each
//! address takes the API over UDP on the same port too, for pointer motion over a lossy link.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context as _};
use directories::ProjectDirs;
use ww_dns_sd::InstanceName;

use super::interfaces::Interfaces;

/// Browser slots when `ww.kdl` doesn't say.
pub const DEFAULT_BROWSERS: u8 = 2;

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq, Eq)]
struct RawConfig {
    #[knuffel(children(name = "listen"), unwrap(argument))]
    listen: Vec<String>,
    #[knuffel(child, unwrap(argument))]
    advertise: Option<String>,
    #[knuffel(child, unwrap(argument))]
    browsers: Option<u8>,
}

/// One `listen` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listen {
    Addr(SocketAddr),
    /// `lan:PORT`.
    Lan(u16),
    /// `tailnet:PORT`.
    Tailnet(u16),
}

impl Listen {
    /// Whether it names an address that depends on the host's interfaces.
    pub fn is_symbolic(self) -> bool {
        !matches!(self, Self::Addr(_))
    }

    /// The address it names now; `None`: the host doesn't have it (yet).
    pub fn resolve(self, interfaces: &Interfaces) -> Option<SocketAddr> {
        match self {
            Self::Addr(addr) => Some(addr),
            Self::Lan(port) => Some(SocketAddr::new(IpAddr::V4(interfaces.lan()?), port)),
            Self::Tailnet(port) => Some(SocketAddr::new(IpAddr::V4(interfaces.tailnet()?), port)),
        }
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        let port = |port: &str| {
            port.parse()
                .with_context(|| format!("listen {text:?}: {port:?} is not a port"))
        };
        match text.split_once(':') {
            Some(("lan", rest)) => return Ok(Self::Lan(port(rest)?)),
            Some(("tailnet", rest)) => return Ok(Self::Tailnet(port(rest)?)),
            _ => (),
        }

        let addr: SocketAddr = text.parse().with_context(|| {
            format!("listen {text:?} is not an address with a port, lan:PORT or tailnet:PORT")
        })?;
        // The API runs commands as this user: it listens on the addresses named, never all.
        if addr.ip().is_unspecified() {
            bail!("listen {text:?} would listen on every address: name each address");
        }
        Ok(Self::Addr(addr))
    }
}

impl fmt::Display for Listen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Addr(addr) => write!(f, "{addr}"),
            Self::Lan(port) => write!(f, "lan:{port}"),
            Self::Tailnet(port) => write!(f, "tailnet:{port}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WwConfig {
    /// Each `listen` line, in the file's order without repeats; none: it doesn't listen.
    pub listen: Vec<Listen>,
    /// The `advertise` name; `None`: the host name.
    pub advertise: Option<InstanceName>,
    pub browsers: u8,
}

impl Default for WwConfig {
    fn default() -> Self {
        Self {
            listen: Vec::new(),
            advertise: None,
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
            let line = Listen::parse(&text)?;
            if !listen.contains(&line) {
                listen.push(line);
            }
        }

        let advertise = raw
            .advertise
            .map(|name| InstanceName::new(name).context("advertise"))
            .transpose()?;

        Ok(Self {
            listen,
            advertise,
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
        assert_eq!(
            config.listen,
            [Listen::Addr("100.64.0.12:7590".parse().unwrap())]
        );
        assert_eq!(config.browsers, 3);
    }

    #[test]
    fn several_listen_lines() {
        let text = "listen \"100.64.0.12:7590\"\nlisten \"192.168.1.95:7590\"\n\
                    listen \"100.64.0.12:7590\"\nlisten \"[fd7a::1]:7590\"\n";
        let config = WwConfig::parse("ww.kdl", text).unwrap();
        let want: Vec<Listen> = ["100.64.0.12:7590", "192.168.1.95:7590", "[fd7a::1]:7590"]
            .iter()
            .map(|a| Listen::Addr(a.parse().unwrap()))
            .collect();
        assert_eq!(config.listen, want);
    }

    #[test]
    fn symbolic_listen_lines() {
        let text = "listen \"lan:7590\"\nlisten \"tailnet:7590\"\nlisten \"lan:7590\"\n\
                    listen \"lan:7591\"\n";
        let config = WwConfig::parse("ww.kdl", text).unwrap();
        assert_eq!(
            config.listen,
            [Listen::Lan(7590), Listen::Tailnet(7590), Listen::Lan(7591)]
        );
        assert_eq!(config.listen[1].to_string(), "tailnet:7590");
    }

    #[test]
    fn refuses_symbolic_without_port() {
        for text in ["lan:", "lan:http", "tailnet:70000", "wan:7590", "LAN:7590"] {
            let line = format!("listen {text:?}");
            assert!(WwConfig::parse("ww.kdl", &line).is_err(), "{line}");
        }
    }

    #[test]
    fn resolves_against_the_interfaces() {
        let host = Interfaces {
            addrs: vec![
                ("tailscale0".to_owned(), "100.98.201.15".parse().unwrap()),
                ("wlan0".to_owned(), "192.168.1.95".parse().unwrap()),
            ],
            default_routes: vec!["wlan0".to_owned()],
        };
        let addr = |text: &str| Some(text.parse::<SocketAddr>().unwrap());
        assert_eq!(Listen::Lan(7590).resolve(&host), addr("192.168.1.95:7590"));
        assert_eq!(
            Listen::Tailnet(7590).resolve(&host),
            addr("100.98.201.15:7590")
        );
        let literal = Listen::Addr("127.0.0.1:7591".parse().unwrap());
        assert_eq!(
            literal.resolve(&Interfaces::default()),
            addr("127.0.0.1:7591")
        );
        assert_eq!(Listen::Lan(7590).resolve(&Interfaces::default()), None);
    }

    #[test]
    fn advertise_name() {
        let config = WwConfig::parse("ww.kdl", "advertise \"Living room TV\"\n").unwrap();
        assert_eq!(config.advertise.unwrap().as_str(), "Living room TV");
        assert!(WwConfig::parse("ww.kdl", "advertise \"\"").is_err());
        let long = format!("advertise {:?}", "x".repeat(64));
        assert!(WwConfig::parse("ww.kdl", &long).is_err());
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
