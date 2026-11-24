//! `ww.kdl`: where the control API listens, the name it is announced under, how many browser
//! slots there are and whose they are, where the volume indicator sits, where the overlay's
//! images are and which local programs' APIs are mounted in it.
//!
//! ```kdl
//! listen "lan:7590"
//! listen "tailnet:7590"
//! listen "127.0.0.1:7590"
//! advertise "Living room TV"
//! mount "hub" "unix:$XDG_RUNTIME_DIR/pult/hub.sock"
//! browsers 3
//! browser 1 owner="Roman" user-agent="Mozilla/5.0 (SMART-TV; Linux; Tizen 6.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/6.0 TV Safari/538.1"
//! volume-indicator anchor="top-right" dx=-40 dy=40 output="HDMI-A-1" label="Громкость" muted-label="Без звука"
//! backgrounds "/home/tv/backgrounds"
//! ```
//!
//! `volume-indicator` places ww_niri's own volume item (docs/ww/Overlay.md); each property may be
//! left out, the line too: top right, 40 px in, on the focused output. `backgrounds` is the folder
//! of the overlay's fill images, `backgrounds` next to `ww.kdl` when left out.
//!
//! A `browser N` line belongs to slot `N` (below the `browsers` count): `owner` names the person
//! whose sign-ins the slot's profile keeps, and `user-agent` is the user agent its Firefox sends
//! from its next start, as a smart TV's makes youtube.com/tv serve its TV interface.
//!
//! A `mount NAME unix:PATH` line mounts the API a local program serves on the Unix socket `PATH`
//! at the slot `ext[NAME]` (see `mounts.rs`). `$VAR` and `${VAR}` in `PATH` are the environment
//! variable's value when the file is read. Only this file adds a slot: a client never can.
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
use ww_niri_api::{Anchor, ITEM_TEXT_LEN_MAX, OWNER_LEN_MAX, USER_AGENT_LEN_MAX};

use super::interfaces::Interfaces;
use super::overlay::volume::Indicator;

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
    #[knuffel(children(name = "browser"))]
    browser: Vec<RawSlot>,
    #[knuffel(child)]
    volume_indicator: Option<RawVolume>,
    #[knuffel(child, unwrap(argument))]
    backgrounds: Option<String>,
    #[knuffel(children(name = "mount"))]
    mount: Vec<RawMount>,
}

/// One `mount NAME TARGET` line.
#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq, Eq)]
struct RawMount {
    #[knuffel(argument)]
    name: String,
    #[knuffel(argument)]
    target: String,
}

/// The longest name of a mount slot, as `ww_api::mount::MountName` takes it.
pub const MOUNT_NAME_LEN_MAX: usize = 64;

/// One `mount` line: the program serving on `socket` is mounted at `ext[name]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountConfig {
    pub name: String,
    /// The Unix socket, with its variables expanded.
    pub socket: PathBuf,
}

impl RawMount {
    fn parse(self, env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<MountConfig> {
        let name = self.name;
        if name.is_empty() || name.len() > MOUNT_NAME_LEN_MAX {
            bail!("mount {name:?}: the name is empty or longer than {MOUNT_NAME_LEN_MAX} bytes");
        }
        if name.chars().any(|c| c.is_control() || c.is_whitespace()) {
            bail!("mount {name:?}: the name has a space or a control character");
        }
        let Some(path) = self.target.strip_prefix("unix:") else {
            bail!("mount {name:?}: {:?} is not unix:PATH", self.target);
        };
        let socket = PathBuf::from(expand(path, env).with_context(|| format!("mount {name:?}"))?);
        if !socket.is_absolute() {
            bail!("mount {name:?}: {socket:?} is not an absolute path");
        }
        Ok(MountConfig { name, socket })
    }
}

/// `text` with each `$VAR` and `${VAR}` replaced by `env`'s value; a variable it has no value
/// for is an error.
fn expand(text: &str, env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let (name, next) = match after.strip_prefix('{') {
            Some(braced) => {
                let end = braced
                    .find('}')
                    .with_context(|| format!("{text:?}: ${{ without }}"))?;
                (&braced[..end], &braced[end + 1..])
            }
            None => {
                let end = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                (&after[..end], &after[end..])
            }
        };
        if name.is_empty() {
            bail!("{text:?}: $ without a variable name");
        }
        let value = env(name).with_context(|| format!("{text:?}: ${name} is not set"))?;
        out.push_str(&value);
        rest = next;
    }
    out.push_str(rest);
    Ok(out)
}

/// The `volume-indicator` line.
#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq, Eq)]
struct RawVolume {
    #[knuffel(property)]
    anchor: Option<String>,
    #[knuffel(property)]
    dx: Option<i32>,
    #[knuffel(property)]
    dy: Option<i32>,
    #[knuffel(property)]
    output: Option<String>,
    #[knuffel(property)]
    label: Option<String>,
    #[knuffel(property(name = "muted-label"))]
    muted_label: Option<String>,
}

impl RawVolume {
    fn indicator(self) -> anyhow::Result<Indicator> {
        let mut indicator = Indicator::default();
        if let Some(anchor) = self.anchor {
            indicator.anchor = parse_anchor(&anchor)?;
        }
        indicator.dx = self.dx.unwrap_or(indicator.dx);
        indicator.dy = self.dy.unwrap_or(indicator.dy);
        indicator.output = self.output;
        let text = |what: &str, text: String| {
            if text.trim().is_empty() || text.len() > ITEM_TEXT_LEN_MAX {
                bail!("volume-indicator: {what} is empty or longer than {ITEM_TEXT_LEN_MAX} bytes");
            }
            Ok(text)
        };
        if let Some(label) = self.label {
            indicator.label = text("label", label)?;
        }
        if let Some(label) = self.muted_label {
            indicator.muted_label = text("muted-label", label)?;
        }
        Ok(indicator)
    }
}

fn parse_anchor(text: &str) -> anyhow::Result<Anchor> {
    Ok(match text {
        "top-left" => Anchor::TopLeft,
        "top" => Anchor::Top,
        "top-right" => Anchor::TopRight,
        "left" => Anchor::Left,
        "center" => Anchor::Center,
        "right" => Anchor::Right,
        "bottom-left" => Anchor::BottomLeft,
        "bottom" => Anchor::Bottom,
        "bottom-right" => Anchor::BottomRight,
        _ => bail!(
            "volume-indicator: anchor {text:?} is not one of top-left, top, top-right, left, \
             center, right, bottom-left, bottom, bottom-right"
        ),
    })
}

/// One `browser N` line.
#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq, Eq)]
struct RawSlot {
    #[knuffel(argument)]
    index: u8,
    #[knuffel(property)]
    owner: Option<String>,
    #[knuffel(property(name = "user-agent"))]
    user_agent: Option<String>,
}

/// What ww.kdl says of one browser slot.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SlotConfig {
    /// The person the slot belongs to.
    pub owner: Option<String>,
    /// The user agent its Firefox sends; `None`: Firefox's own.
    pub user_agent: Option<String>,
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
    /// The browser slots by index: their count is `browsers`.
    pub slots: Vec<SlotConfig>,
    pub volume_indicator: Indicator,
    /// The `backgrounds` folder; `None`: `backgrounds` next to `ww.kdl`.
    pub backgrounds: Option<PathBuf>,
    /// The `mount` lines, in the file's order, each name once.
    pub mounts: Vec<MountConfig>,
}

impl Default for WwConfig {
    fn default() -> Self {
        Self {
            listen: Vec::new(),
            advertise: None,
            slots: vec![SlotConfig::default(); usize::from(DEFAULT_BROWSERS)],
            volume_indicator: Indicator::default(),
            backgrounds: None,
            mounts: Vec::new(),
        }
    }
}

impl WwConfig {
    pub fn parse(filename: &str, text: &str) -> anyhow::Result<Self> {
        Self::parse_with(filename, text, &|name| std::env::var(name).ok())
    }

    /// As [Self::parse], with the variables of `mount` lines from `env`.
    pub fn parse_with(
        filename: &str,
        text: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Self> {
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

        let count = raw.browsers.unwrap_or(DEFAULT_BROWSERS);
        let mut slots = vec![SlotConfig::default(); usize::from(count)];
        let mut seen = Vec::new();
        for line in raw.browser {
            let index = line.index;
            if seen.contains(&index) {
                bail!("browser {index}: a second line for the slot");
            }
            seen.push(index);
            let slot = slots.get_mut(usize::from(index)).with_context(|| {
                format!("browser {index}: there are only {count} slots (`browsers`)")
            })?;
            *slot = SlotConfig {
                owner: line
                    .owner
                    .map(|owner| checked(index, "owner", owner, OWNER_LEN_MAX))
                    .transpose()?,
                user_agent: line
                    .user_agent
                    .map(|ua| checked(index, "user-agent", ua, USER_AGENT_LEN_MAX))
                    .transpose()?,
            };
        }

        let volume_indicator = (raw.volume_indicator)
            .map(RawVolume::indicator)
            .transpose()?
            .unwrap_or_default();

        let mut mounts: Vec<MountConfig> = Vec::new();
        for line in raw.mount {
            let line = line.parse(env)?;
            if mounts.iter().any(|m| m.name == line.name) {
                bail!("mount {:?}: a second line for the name", line.name);
            }
            mounts.push(line);
        }

        Ok(Self {
            listen,
            advertise,
            slots,
            volume_indicator,
            backgrounds: raw.backgrounds.map(PathBuf::from),
            mounts,
        })
    }

    /// The browser slot count.
    pub fn browsers(&self) -> u8 {
        u8::try_from(self.slots.len()).expect("the slots come from a u8 count")
    }
}

/// A text of slot `index`'s line: not empty, at most `len_max` bytes, no control characters.
fn checked(index: u8, what: &str, text: String, len_max: usize) -> anyhow::Result<String> {
    if text.trim().is_empty() {
        bail!("browser {index}: {what} is empty");
    }
    if text.len() > len_max {
        bail!("browser {index}: {what} is longer than {len_max} bytes");
    }
    if text.chars().any(char::is_control) {
        bail!("browser {index}: {what} has a control character");
    }
    Ok(text)
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
        assert_eq!(config.browsers(), 3);
        assert_eq!(config.slots, vec![SlotConfig::default(); 3]);
    }

    #[test]
    fn slot_owner_and_user_agent() {
        let text = "browsers 3\nbrowser 2 owner=\"Роман\" user-agent=\"TV/1.0\"\n\
                    browser 0 owner=\"Аня\"\n";
        let config = WwConfig::parse("ww.kdl", text).unwrap();
        let slot = |owner: Option<&str>, ua: Option<&str>| SlotConfig {
            owner: owner.map(str::to_owned),
            user_agent: ua.map(str::to_owned),
        };
        assert_eq!(
            config.slots,
            [
                slot(Some("Аня"), None),
                slot(None, None),
                slot(Some("Роман"), Some("TV/1.0")),
            ]
        );
    }

    #[test]
    fn refuses_bad_slot_lines() {
        let long = format!("browser 0 owner={:?}", "x".repeat(OWNER_LEN_MAX + 1));
        let long_ua = format!(
            "browser 0 user-agent={:?}",
            "x".repeat(USER_AGENT_LEN_MAX + 1)
        );
        for text in [
            "browsers 2\nbrowser 2 owner=\"Роман\"",
            "browser 0 owner=\"a\"\nbrowser 0 owner=\"b\"",
            "browser 0 owner=\" \"",
            "browser 0 user-agent=\"TV\\n1\"",
            "browser 0 colour=\"red\"",
            &long,
            &long_ua,
        ] {
            assert!(WwConfig::parse("ww.kdl", text).is_err(), "{text}");
        }
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
    fn volume_indicator_and_backgrounds() {
        let text = "volume-indicator anchor=\"bottom-left\" dy=-60 output=\"HDMI-A-1\" \
                    label=\"Громкость\"\nbackgrounds \"/srv/bg\"\n";
        let config = WwConfig::parse("ww.kdl", text).unwrap();
        let want = Indicator {
            anchor: Anchor::BottomLeft,
            dy: -60,
            output: Some("HDMI-A-1".to_owned()),
            label: "Громкость".to_owned(),
            ..Indicator::default()
        };
        assert_eq!(config.volume_indicator, want);
        assert_eq!(config.backgrounds, Some(PathBuf::from("/srv/bg")));
        for text in [
            "volume-indicator anchor=\"middle\"",
            "volume-indicator label=\" \"",
            "volume-indicator colour=\"red\"",
        ] {
            assert!(WwConfig::parse("ww.kdl", text).is_err(), "{text}");
        }
    }

    #[test]
    fn empty_is_default() {
        let config = WwConfig::parse("ww.kdl", "").unwrap();
        assert_eq!(config, WwConfig::default());
        assert_eq!(config.browsers(), DEFAULT_BROWSERS);
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
    fn mount_lines() {
        let env = |name: &str| (name == "XDG_RUNTIME_DIR").then(|| "/run/user/1000".to_owned());
        let text = "mount \"hub\" \"unix:$XDG_RUNTIME_DIR/pult/hub.sock\"\n\
                    mount \"cast\" \"unix:${XDG_RUNTIME_DIR}x/cast.sock\"\n";
        let config = WwConfig::parse_with("ww.kdl", text, &env).unwrap();
        let mount = |name: &str, socket: &str| MountConfig {
            name: name.to_owned(),
            socket: PathBuf::from(socket),
        };
        assert_eq!(
            config.mounts,
            [
                mount("hub", "/run/user/1000/pult/hub.sock"),
                mount("cast", "/run/user/1000x/cast.sock"),
            ]
        );
    }

    #[test]
    fn refuses_bad_mount_lines() {
        let env = |name: &str| (name == "HOME").then(|| "/home/tv".to_owned());
        let long = format!(
            "mount {:?} \"unix:/run/a.sock\"",
            "x".repeat(MOUNT_NAME_LEN_MAX + 1)
        );
        for text in [
            "mount \"hub\" \"/run/hub.sock\"",
            "mount \"hub\" \"tcp:127.0.0.1:7591\"",
            "mount \"hub\" \"unix:hub.sock\"",
            "mount \"hub\" \"unix:$NOPE/hub.sock\"",
            "mount \"hub\" \"unix:${HOME/hub.sock\"",
            "mount \"hub\" \"unix:$/hub.sock\"",
            "mount \"\" \"unix:/run/hub.sock\"",
            "mount \"my hub\" \"unix:/run/hub.sock\"",
            "mount \"hub\"",
            "mount \"hub\" \"unix:/a.sock\"\nmount \"hub\" \"unix:$HOME/b.sock\"",
            &long,
        ] {
            assert!(
                WwConfig::parse_with("ww.kdl", text, &env).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn refuses_unknown_node() {
        assert!(WwConfig::parse("ww.kdl", "apps 1").is_err());
    }
}
