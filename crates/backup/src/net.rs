//! Which addresses user-supplied URLs (webhooks, backup storage) may reach,
//! shared by the control plane and modula-node so there is one SSRF policy.
//!
//! [`public_ip`] is the strict test: globally routable unicast only, with
//! IPv6 forms that embed an IPv4 address (mapped, compatible, NAT64, 6to4)
//! judged by that address. [`backup_ip`] relaxes it for storage endpoints
//! when the operator opts in to private ones (MinIO on a LAN), but never for
//! link-local addresses, where cloud metadata services live.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

/// Cloud metadata services: always refused, even with private endpoints on.
const METADATA_V4: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);
const METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);

/// The opt-in for private (and loopback) backup endpoints.
pub const ALLOW_PRIVATE_ENV: &str = "MODULA_BACKUP_ALLOW_PRIVATE_ENDPOINTS";

/// Whether the environment opts in to private backup endpoints.
pub fn private_endpoints_from_env() -> bool {
    std::env::var(ALLOW_PRIVATE_ENV).is_ok_and(|v| v == "1")
}

/// The IPv4 address an IPv6 one stands for, if it embeds one: IPv4-mapped
/// (`::ffff:a.b.c.d`), IPv4-compatible (`::a.b.c.d`), NAT64 (`64:ff9b::/96`,
/// `64:ff9b:1::/48`) and 6to4 (`2002:AABB:CCDD::/48`).
fn embedded_v4(a: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = a.segments();
    let v4 = |hi: u16, lo: u16| Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
    if let Some(m) = a.to_ipv4_mapped() {
        return Some(m);
    }
    // `::` and `::1` are themselves, not 0.0.0.0 and 0.0.0.1.
    if s[..6] == [0; 6] && !(s[6] == 0 && s[7] <= 1) {
        return Some(v4(s[6], s[7]));
    }
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] || (s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1) {
        return Some(v4(s[6], s[7]));
    }
    if s[0] == 0x2002 {
        return Some(v4(s[1], s[2]));
    }
    None
}

fn public_v4(a: Ipv4Addr) -> bool {
    let [o0, o1, o2, _] = a.octets();
    !(a.is_unspecified()
        || a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_broadcast()
        || a.is_multicast()
        || a.is_documentation()
        || o0 == 0
        // Shared address space (CGNAT).
        || (o0 == 100 && (64..128).contains(&o1))
        // IETF protocol assignments.
        || (o0 == 192 && o1 == 0 && o2 == 0)
        // Benchmarking.
        || (o0 == 198 && (o1 == 18 || o1 == 19))
        // Reserved, and the broadcast address.
        || o0 >= 240)
}

fn public_v6(a: Ipv6Addr) -> bool {
    // The local-use NAT64 prefix (64:ff9b:1::/48) is never global.
    if a.segments()[..3] == [0x64, 0xff9b, 1] {
        return false;
    }
    if let Some(v4) = embedded_v4(a) {
        return public_v4(v4);
    }
    let s = a.segments();
    !(a.is_unspecified()
        || a.is_loopback()
        || a.is_multicast()
        // Link-local fe80::/10 and the deprecated site-local fec0::/10.
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xffc0) == 0xfec0
        // Unique local fc00::/7.
        || (s[0] & 0xfe00) == 0xfc00
        // Teredo 2001::/32 (tunnels to arbitrary IPv4).
        || (s[0] == 0x2001 && s[1] == 0)
        // Documentation 2001:db8::/32.
        || (s[0] == 0x2001 && s[1] == 0x0db8)
        // Discard-only 100::/64.
        || (s[..4] == [0x100, 0, 0, 0]))
}

/// Only globally routable unicast addresses.
pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => public_v4(a),
        IpAddr::V6(a) => public_v6(a),
    }
}

/// Link-local (169.254/16, fe80::/10) or a cloud metadata address, in any
/// IPv6 disguise: never reachable for user-supplied URLs.
pub fn link_local_or_metadata(ip: IpAddr) -> bool {
    let v4 = match ip {
        IpAddr::V4(a) => Some(a),
        IpAddr::V6(a) => embedded_v4(a),
    };
    if let Some(a) = v4 {
        return a.is_link_local() || a == METADATA_V4;
    }
    match ip {
        IpAddr::V6(a) => (a.segments()[0] & 0xffc0) == 0xfe80 || a == METADATA_V6,
        IpAddr::V4(_) => false,
    }
}

/// Loopback (127/8, ::1), also inside an IPv6 form that embeds IPv4.
pub fn loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_loopback(),
        IpAddr::V6(a) => a.is_loopback() || embedded_v4(a).is_some_and(|v| v.is_loopback()),
    }
}

/// Whether a backup storage endpoint may be at `ip`: public addresses, plus
/// private and loopback ones when `allow_private` (the operator's opt-in).
/// Link-local and metadata addresses, unspecified and multicast never.
pub fn backup_ip(ip: IpAddr, allow_private: bool) -> bool {
    if public_ip(ip) {
        return true;
    }
    if !allow_private || link_local_or_metadata(ip) {
        return false;
    }
    let ip = match ip {
        IpAddr::V6(a) => embedded_v4(a).map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(a) => !(a.is_unspecified() || a.is_broadcast() || a.is_multicast()),
        IpAddr::V6(a) => !(a.is_unspecified() || a.is_multicast()),
    }
}

/// A reqwest resolver that resolves like the system one, minus the addresses
/// `allowed` refuses, so a DNS name can't point a guarded client somewhere a
/// literal address couldn't. Literal IP hosts skip resolvers: check those up
/// front. Build guarded clients with `.no_proxy()` too, or the proxy resolves.
pub struct GuardedResolver {
    allowed: Arc<dyn Fn(IpAddr) -> bool + Send + Sync>,
    /// For the error: "<host> has no <what>".
    what: &'static str,
}

impl GuardedResolver {
    pub fn new(
        allowed: impl Fn(IpAddr) -> bool + Send + Sync + 'static,
        what: &'static str,
    ) -> Self {
        Self {
            allowed: Arc::new(allowed),
            what,
        }
    }
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let (allowed, what) = (self.allowed.clone(), self.what);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| allowed(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} has no {what}").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn public_ip_table() {
        let cases: &[(&str, bool)] = &[
            // Public.
            ("1.1.1.1", true),
            ("8.8.8.8", true),
            ("100.63.255.255", true),
            ("100.128.0.0", true),
            ("198.17.255.255", true),
            ("198.20.0.0", true),
            ("223.255.255.255", true),
            ("2606:4700:4700::1111", true),
            ("2a00:1450:4001::1", true),
            // Unspecified, loopback, broadcast, multicast.
            ("0.0.0.0", false),
            ("0.1.2.3", false),
            ("127.0.0.1", false),
            ("127.255.255.254", false),
            ("255.255.255.255", false),
            ("224.0.0.1", false),
            ("239.255.255.255", false),
            ("::", false),
            ("::1", false),
            ("ff02::1", false),
            // Link-local and metadata.
            ("169.254.0.1", false),
            ("169.254.169.254", false),
            ("fe80::1", false),
            ("febf::1", false),
            ("fd00:ec2::254", false),
            // RFC 1918, CGNAT, IETF, documentation, benchmarking, reserved.
            ("10.0.0.1", false),
            ("172.16.0.1", false),
            ("172.31.255.255", false),
            ("192.168.1.1", false),
            ("100.64.0.1", false),
            ("100.127.255.255", false),
            ("192.0.0.8", false),
            ("192.0.2.1", false),
            ("198.51.100.1", false),
            ("203.0.113.1", false),
            ("198.18.0.1", false),
            ("198.19.255.255", false),
            ("240.0.0.1", false),
            ("2001:db8::1", false),
            // ULA, site-local, Teredo, discard.
            ("fc00::1", false),
            ("fdff::1", false),
            ("fec0::1", false),
            ("2001::1", false),
            ("2001:0:4136:e378::1", false),
            ("100::1", false),
            // IPv4 inside IPv6: judged by the IPv4 address.
            ("::ffff:127.0.0.1", false),
            ("::ffff:169.254.169.254", false),
            ("::ffff:10.0.0.1", false),
            ("::ffff:8.8.8.8", true),
            ("::127.0.0.1", false),
            ("::8.8.8.8", true),
            ("64:ff9b::7f00:1", false),
            ("64:ff9b::a9fe:a9fe", false),
            ("64:ff9b::808:808", true),
            ("64:ff9b:1::a00:1", false),
            ("64:ff9b:1::808:808", false),
            ("2002:7f00:1::", false),
            ("2002:a9fe:a9fe::1", false),
            ("2002:c0a8:101::1", false),
            ("2002:808:808::1", true),
        ];
        for (addr, want) in cases {
            assert_eq!(public_ip(ip(addr)), *want, "{addr}");
        }
    }

    #[test]
    fn backup_endpoints() {
        // Strict without the opt-in.
        for a in ["10.0.0.5", "192.168.1.10", "127.0.0.1", "fd12::1", "::1"] {
            assert!(!backup_ip(ip(a), false), "{a}");
            assert!(backup_ip(ip(a), true), "{a}");
        }
        assert!(backup_ip(ip("1.1.1.1"), false));
        assert!(loopback(ip("::ffff:127.0.0.1")) && loopback(ip("::1")));
        assert!(!loopback(ip("10.0.0.1")));
        // Metadata and link-local never, in any form.
        for a in [
            "169.254.169.254",
            "169.254.1.1",
            "fe80::1",
            "fd00:ec2::254",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "2002:a9fe:a9fe::",
            "0.0.0.0",
            "::",
            "224.0.0.1",
        ] {
            assert!(!backup_ip(ip(a), true), "{a}");
        }
    }
}
