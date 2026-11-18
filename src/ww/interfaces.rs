//! The host's IPv4 addresses and default routes, which `lan:PORT` and `tailnet:PORT` resolve to.

use std::io::Read as _;
use std::net::Ipv4Addr;

use anyhow::Context as _;

/// The interface Tailscale makes; its address is the tailnet address.
const TAILNET_INTERFACE: &str = "tailscale0";

/// The kernel's IPv4 routing table: the main table, without Tailscale's policy routes.
const ROUTE_TABLE_PATH: &str = "/proc/net/route";

/// `/proc/net/route` is about 128 bytes a route; more than this is not a TV box's table.
// spirit-level: allow(exact-units-quantity) the argument of Read::take, a plain u64 of bytes
const ROUTE_TABLE_BYTES_MAX: u64 = 1 << 20;

/// `RTF_UP` in the route flags.
const ROUTE_FLAG_UP: u32 = 0x1;

/// What the host has now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Interfaces {
    /// Each IPv4 address and its interface, in the kernel's order.
    pub addrs: Vec<(String, Ipv4Addr)>,
    /// The interfaces of the IPv4 default routes that are up, lowest metric first.
    pub default_routes: Vec<String>,
}

impl Interfaces {
    /// Reads the addresses (getifaddrs) and the routing table.
    pub fn read() -> anyhow::Result<Self> {
        let addrs = nix::ifaddrs::getifaddrs()
            .context("error reading the interface addresses")?
            .filter_map(|ifaddr| {
                let ip = ifaddr.address?.as_sockaddr_in()?.ip();
                Some((ifaddr.interface_name, ip))
            })
            .collect();

        let mut text = String::new();
        std::fs::File::open(ROUTE_TABLE_PATH)
            .and_then(|file| file.take(ROUTE_TABLE_BYTES_MAX).read_to_string(&mut text))
            .with_context(|| format!("error reading {ROUTE_TABLE_PATH}"))?;

        Ok(Self {
            addrs,
            default_routes: default_routes(&text),
        })
    }

    /// The LAN address: the first private IPv4 address on the interface of the best default
    /// route, never the tailnet's interface.
    pub fn lan(&self) -> Option<Ipv4Addr> {
        self.default_routes
            .iter()
            .filter(|name| *name != TAILNET_INTERFACE)
            .find_map(|name| {
                self.addrs
                    .iter()
                    .find(|(iface, ip)| iface == name && ip.is_private())
                    .map(|(_, ip)| *ip)
            })
    }

    /// The tailnet address: the address in 100.64.0.0/10 on `tailscale0`.
    pub fn tailnet(&self) -> Option<Ipv4Addr> {
        self.addrs
            .iter()
            .find(|(iface, ip)| iface == TAILNET_INTERFACE && is_shared(*ip))
            .map(|(_, ip)| *ip)
    }
}

/// Whether `ip` is in 100.64.0.0/10, the shared address space Tailscale takes its addresses from.
fn is_shared(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && b & 0xc0 == 64
}

/// The interfaces of the default routes in `/proc/net/route`, lowest metric first; lines that
/// don't parse are skipped.
fn default_routes(text: &str) -> Vec<String> {
    let mut routes: Vec<(u32, &str)> = text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [iface, destination, _gateway, flags, _refcnt, _use, metric, mask, ..] = fields[..]
            else {
                return None;
            };
            let flags = u32::from_str_radix(flags, 16).ok()?;
            let is_default = destination == "00000000" && mask == "00000000";
            if !is_default || flags & ROUTE_FLAG_UP == 0 {
                return None;
            }
            Some((metric.parse().ok()?, iface))
        })
        .collect();
    routes.sort_by_key(|(metric, _)| *metric);
    let mut names: Vec<String> = Vec::new();
    for (_, iface) in routes {
        if !names.iter().any(|name| name == iface) {
            names.push(iface.to_owned());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str =
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n";

    fn interfaces(addrs: &[(&str, &str)], routes: &str) -> Interfaces {
        Interfaces {
            addrs: addrs
                .iter()
                .map(|(iface, ip)| ((*iface).to_owned(), ip.parse().unwrap()))
                .collect(),
            default_routes: default_routes(&format!("{HEADER}{routes}")),
        }
    }

    #[test]
    fn default_routes_lowest_metric_first() {
        let routes = "wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
                      wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n\
                      enp1s0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
                      down0\t00000000\t0101A8C0\t0002\t0\t0\t50\t00000000\t0\t0\t0\n\
                      broken\t00000000\n";
        assert_eq!(
            default_routes(&format!("{HEADER}{routes}")),
            ["enp1s0", "wlan0"]
        );
    }

    #[test]
    fn lan_and_tailnet() {
        let host = interfaces(
            &[
                ("lo", "127.0.0.1"),
                ("tailscale0", "100.98.201.15"),
                ("wlan0", "192.168.1.95"),
                ("docker0", "172.17.0.1"),
            ],
            "wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n",
        );
        assert_eq!(host.lan(), Some(Ipv4Addr::new(192, 168, 1, 95)));
        assert_eq!(host.tailnet(), Some(Ipv4Addr::new(100, 98, 201, 15)));
    }

    #[test]
    fn lan_skips_public_and_tailnet_routes() {
        // A default route through tailscale0 (exit node in the main table) and one through an
        // interface with a public address come before the LAN's route.
        let host = interfaces(
            &[
                ("tailscale0", "100.98.201.15"),
                ("wwan0", "203.0.113.7"),
                ("eth0", "10.0.0.5"),
            ],
            "tailscale0\t00000000\t00000000\t0001\t0\t0\t0\t00000000\t0\t0\t0\n\
             wwan0\t00000000\t00000000\t0001\t0\t0\t10\t00000000\t0\t0\t0\n\
             eth0\t00000000\t0100000A\t0003\t0\t0\t100\t00000000\t0\t0\t0\n",
        );
        assert_eq!(host.lan(), Some(Ipv4Addr::new(10, 0, 0, 5)));
    }

    #[test]
    fn nothing_yet() {
        // At boot: no default route, Tailscale not up.
        let host = interfaces(&[("lo", "127.0.0.1"), ("wlan0", "192.168.1.95")], "");
        assert_eq!(host.lan(), None);
        assert_eq!(host.tailnet(), None);

        // tailscale0 exists without a tailnet address.
        let host = interfaces(&[("tailscale0", "192.168.7.1")], "");
        assert_eq!(host.tailnet(), None);
    }

    #[test]
    fn shared_address_space() {
        assert!(is_shared(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(is_shared(Ipv4Addr::new(100, 127, 255, 254)));
        assert!(!is_shared(Ipv4Addr::new(100, 128, 0, 1)));
        assert!(!is_shared(Ipv4Addr::new(100, 63, 0, 1)));
    }

    #[test]
    fn reads_this_host() {
        let host = Interfaces::read().unwrap();
        assert!(host.addrs.iter().any(|(_, ip)| ip.is_loopback()));
    }
}
