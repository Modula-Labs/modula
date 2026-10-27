//! The node's own IPv4 addresses, for the server to find a private path
//! between two nodes (an address of each in one subnet).
//!
//! Read with iproute2 (`ip -o -4 addr` / `ip -o -4 route`). An address
//! configured as /32 (Hetzner's Cloud Networks, some VPCs) is reported with
//! the prefix of the route through its interface that covers it, e.g.
//! `10.0.0.2/32` with `10.0.0.0/16 via 10.0.0.1 dev ens10` → `10.0.0.2/16`.

use std::net::Ipv4Addr;

/// Interfaces that are never a path to another server.
fn skipped(iface: &str) -> bool {
    iface == "lo"
        || [
            "docker", "br-", "veth", "modula", "wg", "cni", "flannel", "virbr",
        ]
        .iter()
        .any(|p| iface.starts_with(p))
}

fn parse_cidr(s: &str) -> Option<(Ipv4Addr, u8)> {
    let (ip, prefix) = s.split_once('/')?;
    let p: u8 = prefix.parse().ok()?;
    (p <= 32).then_some((ip.parse().ok()?, p))
}

fn contains((net, prefix): (Ipv4Addr, u8), ip: Ipv4Addr) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    u32::from(net) & mask == u32::from(ip) & mask
}

/// `ip -o -4 addr show` and `ip -o -4 route show` → "ip/prefix" list.
pub fn parse(addr_output: &str, route_output: &str) -> Vec<String> {
    // (interface, destination) of routes through a gateway.
    let routes: Vec<(String, (Ipv4Addr, u8))> = route_output
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let dest = parse_cidr(f.first()?)?;
            let dev = f
                .iter()
                .position(|w| *w == "dev")
                .and_then(|i| f.get(i + 1))?;
            Some(((*dev).to_owned(), dest))
        })
        .collect();
    let mut out = Vec::new();
    for line in addr_output.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let Some(iface) = f.get(1).map(|i| i.trim_end_matches(':')) else {
            continue;
        };
        let iface = iface.split('@').next().unwrap_or(iface);
        if skipped(iface) {
            continue;
        }
        let Some((ip, mut prefix)) = f
            .iter()
            .position(|w| *w == "inet")
            .and_then(|i| f.get(i + 1))
            .and_then(|c| parse_cidr(c))
        else {
            continue;
        };
        if ip.is_loopback() || ip.is_link_local() || ip.octets()[..2] == [10, 80] {
            continue;
        }
        if prefix == 32
            && let Some((_, (_, p))) = routes
                .iter()
                .filter(|(dev, dest)| {
                    dev == iface && dest.1 > 0 && dest.1 < 32 && contains(*dest, ip)
                })
                .max_by_key(|(_, dest)| dest.1)
        {
            prefix = *p;
        }
        out.push(format!("{ip}/{prefix}"));
    }
    out.sort();
    out.dedup();
    out
}

/// This host's addresses (empty when iproute2 isn't there).
pub async fn detect() -> Vec<String> {
    let (Ok(a), Ok(r)) = (
        crate::access::run("ip", &["-o", "-4", "addr", "show"]).await,
        crate::access::run("ip", &["-o", "-4", "route", "show"]).await,
    ) else {
        return Vec::new();
    };
    parse(&a, &r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hetzner_cloud_network() {
        let addrs = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
2: eth0    inet 203.0.113.10/32 metric 100 scope global dynamic eth0\\       valid_lft 85000sec preferred_lft 85000sec
3: ens10    inet 10.0.0.2/32 brd 10.0.0.2 scope global dynamic ens10\\       valid_lft 85000sec preferred_lft 85000sec
4: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0\\       valid_lft forever preferred_lft forever
9: modula-mesh    inet 10.80.3.1/16 brd 10.80.255.255 scope global modula-mesh\\       valid_lft forever preferred_lft forever
10: modula0    inet 10.80.3.1/32 scope global modula0\\       valid_lft forever preferred_lft forever";
        let routes = "default via 172.31.1.1 dev eth0 proto dhcp src 203.0.113.10 metric 100
10.0.0.0/16 via 10.0.0.1 dev ens10
10.0.0.1 dev ens10 scope link
172.17.0.0/16 dev docker0 proto kernel scope link src 172.17.0.1
172.31.1.1 dev eth0 proto dhcp scope link src 203.0.113.10 metric 100";
        assert_eq!(parse(addrs, routes), ["10.0.0.2/16", "203.0.113.10/32"]);
    }

    #[test]
    fn plain_lan_and_containers() {
        let addrs = "11: eth0@if12    inet 172.30.0.3/16 brd 172.30.255.255 scope global eth0\\       valid_lft forever preferred_lft forever
13: br-1a2b    inet 172.18.0.1/16 scope global br-1a2b";
        assert_eq!(parse(addrs, ""), ["172.30.0.3/16"]);
        assert!(parse("garbage", "more garbage").is_empty());
    }
}
