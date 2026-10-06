//! Enumerates the machine's network interfaces.

use std::collections::BTreeMap;
use std::net::IpAddr;

/// One network interface and all of its addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetIface {
    pub name: String,
    pub addrs: Vec<IpAddr>,
}

/// Query the operating system for interfaces. Failures yield an empty list;
/// the caller falls back to loopback and says so.
pub fn list() -> Vec<NetIface> {
    let Ok(all) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut grouped: BTreeMap<String, Vec<IpAddr>> = BTreeMap::new();
    for iface in all {
        grouped
            .entry(iface.name.clone())
            .or_default()
            .push(iface.ip());
    }
    grouped
        .into_iter()
        .map(|(name, addrs)| NetIface { name, addrs })
        .collect()
}

/// Rough, name-based classification used for labels and ordering.
/// Linux predictable names: `wl*` Wi-Fi, `en*`/`eth*` Ethernet.
pub fn kind_label(name: &str) -> &'static str {
    let n = name.to_ascii_lowercase();
    if n.starts_with("wl") || n.contains("wifi") || n.contains("wi-fi") {
        "Wi-Fi"
    } else if n == "lo" || n.starts_with("lo0") {
        "Loopback"
    } else if n.starts_with("en") || n.starts_with("eth") {
        "Ethernet"
    } else if n.starts_with("docker")
        || n.starts_with("br-")
        || n.starts_with("veth")
        || n.starts_with("virbr")
        || n.starts_with("vmnet")
        || n.starts_with("vboxnet")
        || n.starts_with("cni")
        || n.starts_with("flannel")
    {
        "Virtual"
    } else if n.starts_with("tun")
        || n.starts_with("tap")
        || n.starts_with("wg")
        || n.starts_with("tailscale")
        || n.starts_with("zt")
    {
        "VPN"
    } else {
        "Other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_linux_names() {
        assert_eq!(kind_label("wlan0"), "Wi-Fi");
        assert_eq!(kind_label("wlp3s0"), "Wi-Fi");
        assert_eq!(kind_label("enp0s31f6"), "Ethernet");
        assert_eq!(kind_label("eth0"), "Ethernet");
        assert_eq!(kind_label("lo"), "Loopback");
        assert_eq!(kind_label("docker0"), "Virtual");
        assert_eq!(kind_label("br-1a2b3c"), "Virtual");
        assert_eq!(kind_label("tailscale0"), "VPN");
        assert_eq!(kind_label("wg0"), "VPN");
    }
}
