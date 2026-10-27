//! The network this device is on (ADR-0016): the default gateway's MAC address and the
//! subnet, read without location permissions. The daemon hashes it into a profile
//! fingerprint; mesh routers that change the gateway MAC give several fingerprints.

// The /proc parsers are used on Linux only but tested everywhere.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::net::Ipv4Addr;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NetworkInfo {
    pub gateway: Ipv4Addr,
    /// From the neighbour table; `None` until the gateway answered ARP.
    pub gateway_mac: Option<[u8; 6]>,
    pub subnet: Ipv4Addr,
    pub prefix_len: u8,
}

impl NetworkInfo {
    /// What identifies the network: the gateway MAC (or its address without one) and
    /// the subnet.
    pub fn identity_bytes(&self) -> Vec<u8> {
        let mut v = match self.gateway_mac {
            Some(m) => m.to_vec(),
            None => self.gateway.octets().to_vec(),
        };
        v.extend_from_slice(&self.subnet.octets());
        v.push(self.prefix_len);
        v
    }
}

fn prefix_of(mask: Ipv4Addr) -> u8 {
    u32::from(mask).leading_ones() as u8
}

/// Parses `/proc/net/route`: the default route's interface and gateway, and the
/// on-link route of that interface for the subnet. Addresses are little-endian hex.
pub(crate) fn parse_proc_route(text: &str) -> Option<(String, Ipv4Addr, Ipv4Addr, u8)> {
    let hex = |s: &str| {
        u32::from_str_radix(s, 16)
            .ok()
            .map(|v| Ipv4Addr::from(v.swap_bytes()))
    };
    let rows: Vec<Vec<&str>> = text
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().collect())
        .collect();
    let mut best: Option<(&str, Ipv4Addr, u32)> = None;
    for r in &rows {
        if r.len() < 8 {
            continue;
        }
        let flags = u32::from_str_radix(r[3], 16).unwrap_or(0);
        let metric: u32 = r[6].parse().unwrap_or(u32::MAX);
        if r[1] == "00000000"
            && r[7] == "00000000"
            && flags & 0x3 == 0x3
            && best.is_none_or(|b| metric < b.2)
        {
            best = Some((r[0], hex(r[2])?, metric));
        }
    }
    let (iface, gw, _) = best?;
    let (subnet, mask) = rows
        .iter()
        .filter(|r| r.len() >= 8 && r[0] == iface && r[2] == "00000000" && r[7] != "00000000")
        .filter_map(|r| Some((hex(r[1])?, hex(r[7])?)))
        .max_by_key(|(_, m)| u32::from(*m))?;
    Some((iface.to_owned(), gw, subnet, prefix_of(mask)))
}

/// Parses `/proc/net/arp` for the MAC of `ip`.
pub(crate) fn parse_proc_arp(text: &str, ip: Ipv4Addr) -> Option<[u8; 6]> {
    let ip = ip.to_string();
    let line = text
        .lines()
        .skip(1)
        .find(|l| l.split_whitespace().next() == Some(ip.as_str()))?;
    let mac = line.split_whitespace().nth(3)?;
    parse_mac(mac)
}

fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<u8> = s
        .split(':')
        .filter_map(|p| u8::from_str_radix(p, 16).ok())
        .collect();
    let mac: [u8; 6] = parts.try_into().ok()?;
    (mac != [0; 6]).then_some(mac)
}

/// The current network, or `None` without a default route.
pub fn current_network() -> Option<NetworkInfo> {
    #[cfg(target_os = "linux")]
    {
        let route = std::fs::read_to_string("/proc/net/route").ok()?;
        let (_, gateway, subnet, prefix_len) = parse_proc_route(&route)?;
        let arp = std::fs::read_to_string("/proc/net/arp").unwrap_or_default();
        Some(NetworkInfo {
            gateway,
            gateway_mac: parse_proc_arp(&arp, gateway),
            subnet,
            prefix_len,
        })
    }
    #[cfg(windows)]
    {
        crate::windows::current_network()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        // TODO(macOS-final): sysctl NET_RT_FLAGS for the gateway and its MAC.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTE: &str =
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eth0\t00000000\t0150A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
eth0\t0050A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
wlan0\t00000000\t01AFA8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
wlan0\t00AFA8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0
";

    const ARP: &str =
        "IP address       HW type     Flags       HW address            Mask     Device
192.168.80.1     0x1         0x2         a4:2b:b0:11:22:33     *        eth0
192.168.175.1    0x1         0x0         00:00:00:00:00:00     *        wlan0
";

    #[test]
    fn linux_tables_give_the_gateway_and_subnet() {
        let (iface, gw, subnet, prefix) = parse_proc_route(ROUTE).unwrap();
        assert_eq!(iface, "eth0", "lowest metric wins");
        assert_eq!(gw, Ipv4Addr::new(192, 168, 80, 1));
        assert_eq!(subnet, Ipv4Addr::new(192, 168, 80, 0));
        assert_eq!(prefix, 24);
        assert_eq!(
            parse_proc_arp(ARP, gw),
            Some([0xa4, 0x2b, 0xb0, 0x11, 0x22, 0x33])
        );
        assert_eq!(
            parse_proc_arp(ARP, Ipv4Addr::new(192, 168, 175, 1)),
            None,
            "incomplete entry"
        );
        assert_eq!(parse_proc_route("Iface\tDestination\n"), None);
    }

    #[test]
    fn identity_prefers_the_gateway_mac() {
        let mut n = NetworkInfo {
            gateway: Ipv4Addr::new(10, 0, 0, 1),
            gateway_mac: Some([1, 2, 3, 4, 5, 6]),
            subnet: Ipv4Addr::new(10, 0, 0, 0),
            prefix_len: 24,
        };
        assert_eq!(n.identity_bytes(), vec![1, 2, 3, 4, 5, 6, 10, 0, 0, 0, 24]);
        n.gateway_mac = None;
        assert_eq!(n.identity_bytes()[..4], [10, 0, 0, 1]);
        assert_eq!(prefix_of(Ipv4Addr::new(255, 255, 240, 0)), 20);
    }

    #[test]
    fn this_machine_has_a_network_or_none() {
        // Only checks that reading the OS tables never panics.
        let _ = current_network();
    }
}
