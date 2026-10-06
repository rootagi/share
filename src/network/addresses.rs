//! Chooses which addresses to bind to and which URLs to advertise.
//!
//! All functions take the interface list as an argument so they can be tested
//! without touching the real network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::interfaces::{NetIface, kind_label};
use crate::error::{Result, ShareError};

/// An address a client on the LAN could use to reach this server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanAddr {
    pub iface: String,
    pub label: &'static str,
    pub ip: IpAddr,
}

/// True for addresses that make sense to hand to another device.
pub fn is_usable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast())
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || is_v6_link_local(v6))
        }
    }
}

fn is_v6_link_local(v6: &Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

fn is_cgnat(v4: &Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// Lower is better. Real LAN adapters on private ranges come first,
/// container bridges and VPNs last.
fn priority(label: &str, ip: &IpAddr) -> u8 {
    let base = match label {
        "Ethernet" => 0,
        "Wi-Fi" => 1,
        "Other" => 2,
        "VPN" => 4,
        "Virtual" => 5,
        _ => 6,
    };
    let range = match ip {
        IpAddr::V4(v4) if v4.is_private() => 0,
        IpAddr::V4(v4) if is_cgnat(v4) => 2,
        IpAddr::V4(_) => 1,
        IpAddr::V6(_) => 3,
    };
    base * 4 + range
}

/// All usable IPv4/IPv6 addresses, best first.
pub fn lan_addresses(ifaces: &[NetIface]) -> Vec<LanAddr> {
    let mut out: Vec<LanAddr> = ifaces
        .iter()
        .flat_map(|iface| {
            let label = kind_label(&iface.name);
            iface
                .addrs
                .iter()
                .filter(|ip| is_usable(ip))
                .map(move |ip| LanAddr {
                    iface: iface.name.clone(),
                    label,
                    ip: *ip,
                })
        })
        .collect();
    out.sort_by_key(|a| (priority(a.label, &a.ip), a.iface.clone()));
    out
}

/// Resolve `--interface <name>` to the interface's best IPv4 address.
pub fn resolve_interface(name: &str, ifaces: &[NetIface]) -> Result<IpAddr> {
    let mut v4 = ifaces
        .iter()
        .filter(|i| i.name == name)
        .flat_map(|i| i.addrs.iter())
        .filter(|ip| ip.is_ipv4())
        .copied()
        .collect::<Vec<_>>();
    v4.sort_by_key(|ip| !is_usable(ip));
    v4.first()
        .copied()
        .ok_or_else(|| ShareError::InterfaceUnavailable(name.to_string()))
}

/// The addresses to show to the user for a given bind address.
///
/// * wildcard bind → every usable LAN address (IPv4 only for `0.0.0.0`)
/// * specific bind → exactly that address
/// * nothing usable → loopback, so the user still gets a working URL
pub fn advertised(bind: IpAddr, ifaces: &[NetIface]) -> Vec<LanAddr> {
    if bind.is_unspecified() {
        let want_v6 = bind.is_ipv6();
        let found: Vec<LanAddr> = lan_addresses(ifaces)
            .into_iter()
            .filter(|a| want_v6 || a.ip.is_ipv4())
            .collect();
        if !found.is_empty() {
            return found;
        }
        return vec![loopback()];
    }
    let (iface, label) = ifaces
        .iter()
        .find(|i| i.addrs.contains(&bind))
        .map(|i| (i.name.clone(), kind_label(&i.name)))
        .unwrap_or_else(|| ("custom".to_string(), "Other"));
    vec![LanAddr {
        iface,
        label,
        ip: bind,
    }]
}

fn loopback() -> LanAddr {
    LanAddr {
        iface: "lo".into(),
        label: "Loopback",
        ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, addrs: &[&str]) -> NetIface {
        NetIface {
            name: name.into(),
            addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
        }
    }

    fn sample() -> Vec<NetIface> {
        vec![
            iface("lo", &["127.0.0.1", "::1"]),
            iface("docker0", &["172.17.0.1"]),
            iface("wlan0", &["192.168.1.15", "fe80::1"]),
            iface("eth0", &["192.168.1.20"]),
            iface("tailscale0", &["100.101.102.103"]),
        ]
    }

    #[test]
    fn loopback_and_link_local_are_not_advertised() {
        let addrs = lan_addresses(&sample());
        assert!(addrs.iter().all(|a| !a.ip.is_loopback()));
        assert!(addrs.iter().all(|a| a.ip.to_string() != "fe80::1"));
    }

    #[test]
    fn ethernet_and_wifi_outrank_virtual_and_vpn() {
        let addrs = lan_addresses(&sample());
        let order: Vec<&str> = addrs.iter().map(|a| a.iface.as_str()).collect();
        assert_eq!(order, ["eth0", "wlan0", "tailscale0", "docker0"]);
    }

    #[test]
    fn wildcard_ipv4_bind_lists_only_ipv4() {
        let list = advertised("0.0.0.0".parse().unwrap(), &sample());
        assert!(list.iter().all(|a| a.ip.is_ipv4()));
        assert_eq!(list.len(), 4);
    }

    #[test]
    fn explicit_bind_advertises_only_that_address() {
        let list = advertised("192.168.1.15".parse().unwrap(), &sample());
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].iface, "wlan0");
        assert_eq!(list[0].label, "Wi-Fi");
    }

    #[test]
    fn falls_back_to_loopback_when_offline() {
        let list = advertised("0.0.0.0".parse().unwrap(), &[iface("lo", &["127.0.0.1"])]);
        assert_eq!(list.len(), 1);
        assert!(list[0].ip.is_loopback());
    }

    #[test]
    fn interface_resolution() {
        let ip = resolve_interface("wlan0", &sample()).unwrap();
        assert_eq!(ip.to_string(), "192.168.1.15");
        assert!(matches!(
            resolve_interface("nope0", &sample()),
            Err(ShareError::InterfaceUnavailable(_))
        ));
    }
}
