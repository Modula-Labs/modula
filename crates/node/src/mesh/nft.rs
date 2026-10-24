//! The mesh firewall: one nftables table, replaced atomically on change.
//!
//! Only decisions to *drop* live here: Docker's own (iptables) FORWARD
//! chain still has to accept mesh traffic, which `linux.rs` arranges with
//! `DOCKER-USER` rules. Both chains see every packet; a drop in either wins.
//!
//! What it enforces, on every node:
//!   * only the mesh bridge sends into the tunnel, and only from this node's
//!     own container addresses (no container can borrow the host's identity
//!     through an environment bridge's masquerade, or spoof another's);
//!   * what arrives through the tunnel only goes to the mesh bridge (on the
//!     relay hub also back into the tunnel, between mesh addresses);
//!   * a new connection is allowed when source and destination are in the
//!     same environment's set, or the source is a node (Caddy balancing over
//!     replicas on other servers);
//!   * containers reach nothing of the host over the mesh but its resolver,
//!     and other nodes' containers nothing of it at all (replies to the
//!     host's own connections, e.g. Caddy's, excepted);
//!   * a service's virtual IP takes new connections from its environment
//!     only, each DNATed to a random healthy replica on any node (the
//!     isolation above then applies to the replica's address).

use std::fmt::Write as _;
use std::net::Ipv4Addr;

use super::{BRIDGE, INTERFACE, Subnet};

pub const TABLE: &str = "modula";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub subnet: Subnet,
    /// Every peer node's host address.
    pub peers: Vec<Ipv4Addr>,
    /// (environment id, every mesh address of it on any node).
    pub environments: Vec<(String, Vec<Ipv4Addr>)>,
    /// This node relays between peers: tunnel-to-tunnel traffic inside the
    /// mesh passes (the receiving node's firewall still decides).
    pub relay_hub: bool,
    /// Services' virtual IPs ([`super::VIP_RANGE`]).
    pub vips: Vec<Vip>,
}

/// A service's virtual IP within its environment, and its healthy replicas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vip {
    pub environment: String,
    pub vip: Ipv4Addr,
    pub backends: Vec<Ipv4Addr>,
}

/// Environment ids become set names (`env_<id>`): ids are 16 lowercase
/// base36 characters; anything else is left out.
pub fn valid_set_id(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn set(out: &mut String, name: &str, ips: &[Ipv4Addr]) {
    let _ = writeln!(out, "\tset {name} {{\n\t\ttype ipv4_addr");
    if !ips.is_empty() {
        let list: Vec<String> = ips.iter().map(Ipv4Addr::to_string).collect();
        let _ = writeln!(out, "\t\telements = {{ {} }}", list.join(", "));
    }
    out.push_str("\t}\n");
}

/// The complete script for `nft -f -`: (re)creates the table in one transaction.
pub fn render(spec: &Spec) -> String {
    let own = spec.subnet;
    let gw = own.gateway();
    let mut out = format!("table inet {TABLE}\ndelete table inet {TABLE}\ntable inet {TABLE} {{\n");
    let mut peers = spec.peers.clone();
    peers.sort();
    peers.dedup();
    set(&mut out, "nodes", &peers);
    let mut envs: Vec<&(String, Vec<Ipv4Addr>)> = spec
        .environments
        .iter()
        .filter(|(id, ips)| valid_set_id(id) && !ips.is_empty())
        .collect();
    envs.sort_by(|a, b| a.0.cmp(&b.0));
    envs.dedup_by(|a, b| a.0 == b.0);
    for (id, ips) in &envs {
        let mut ips = ips.clone();
        ips.sort();
        ips.dedup();
        set(&mut out, &format!("env_{id}"), &ips);
    }
    let _ = write!(
        out,
        "\tchain forward {{
\t\ttype filter hook forward priority filter - 1; policy accept;
"
    );
    if spec.relay_hub {
        let _ = writeln!(
            out,
            "\t\tiifname \"{INTERFACE}\" oifname \"{INTERFACE}\" ip saddr 10.80.0.0/16 ip daddr 10.80.0.0/16 accept"
        );
    }
    let _ = write!(
        out,
        "\t\tiifname \"{INTERFACE}\" oifname != \"{BRIDGE}\" drop
\t\toifname \"{INTERFACE}\" iifname != \"{BRIDGE}\" drop
\t\tiifname \"{BRIDGE}\" meta nfproto ipv6 drop
\t\tiifname \"{BRIDGE}\" ip saddr != {own} drop
\t\tiifname \"{BRIDGE}\" ip saddr {gw} drop
\t\tiifname \"{BRIDGE}\" ip daddr != 10.80.0.0/16 drop
\t\tiifname != \"{BRIDGE}\" oifname != \"{BRIDGE}\" accept
\t\tct state established,related accept
\t\tip saddr @nodes accept
"
    );
    for (id, _) in &envs {
        let _ = writeln!(out, "\t\tip saddr @env_{id} ip daddr @env_{id} accept");
    }
    let _ = write!(
        out,
        "\t\tcounter drop
\t}}
\tchain input {{
\t\ttype filter hook input priority filter - 1; policy accept;
\t\tct state established,related accept
\t\tiifname \"{INTERFACE}\" ip saddr != @nodes drop
\t\tiifname \"{BRIDGE}\" ip daddr {gw} meta l4proto {{ tcp, udp }} th dport 53 accept
\t\tiifname \"{BRIDGE}\" drop
\t}}
"
    );
    // Virtual IPs: a new connection from the service's environment goes to
    // one of its healthy replicas, picked at random (any port, unchanged);
    // before Docker's nat (the first NAT binding wins). A replica on this
    // node answers through the host (masquerade), or its reply would skip
    // the translation on the shared bridge.
    let known: Vec<&str> = envs.iter().map(|(id, _)| id.as_str()).collect();
    let mut vips: Vec<&Vip> = spec
        .vips
        .iter()
        .filter(|v| !v.backends.is_empty() && known.contains(&v.environment.as_str()))
        .collect();
    vips.sort_by(|a, b| (&a.environment, a.vip).cmp(&(&b.environment, b.vip)));
    if !vips.is_empty() {
        out.push_str(
            "\tchain prerouting {\n\t\ttype nat hook prerouting priority dstnat - 10; policy accept;\n",
        );
        for v in vips {
            let mut backends = v.backends.clone();
            backends.sort();
            backends.dedup();
            let map: Vec<String> = backends
                .iter()
                .enumerate()
                .map(|(i, ip)| format!("{i} : {ip}"))
                .collect();
            let _ = writeln!(
                out,
                "\t\tiifname \"{BRIDGE}\" ip saddr @env_{} ip daddr {} dnat ip to numgen random mod {} map {{ {} }}",
                v.environment,
                v.vip,
                backends.len(),
                map.join(", ")
            );
        }
        let _ = write!(
            out,
            "\t}}
\tchain postrouting {{
\t\ttype nat hook postrouting priority srcnat - 10; policy accept;
\t\toifname \"{BRIDGE}\" ip saddr {own} ct status dnat masquerade
\t}}
"
        );
    }
    out.push_str("}\n");
    out
}

/// The host's listing without what changes on its own: counters' packets
/// and bytes, and whitespace.
pub fn normalize(listing: &str) -> String {
    let mut out = String::with_capacity(listing.len());
    for line in listing.lines() {
        let mut words = line.split_whitespace();
        let mut kept: Vec<&str> = Vec::new();
        while let Some(w) = words.next() {
            if w == "packets" || w == "bytes" {
                words.next();
                continue;
            }
            kept.push(w);
        }
        if !kept.is_empty() {
            out.push_str(&kept.join(" "));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizing_drops_counter_values_only() {
        let a = "table inet modula {\n\tchain forward {\n\t\tcounter packets 3 bytes 180 drop\n\t}\n}\n";
        let b =
            "table inet modula {\n  chain forward {\n    counter packets 9001 bytes 5 drop\n  }\n}";
        assert_eq!(normalize(a), normalize(b));
        assert_eq!(
            normalize(a),
            "table inet modula {\nchain forward {\ncounter drop\n}\n}\n"
        );
        assert_ne!(normalize(a), normalize(&a.replace("drop", "accept")));
    }

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn renders_one_set_and_rule_per_environment() {
        let spec = Spec {
            subnet: Subnet { index: 1 },
            peers: vec![ip("10.80.3.1"), ip("10.80.2.1"), ip("10.80.2.1")],
            environments: vec![
                (
                    "bbbbbbbbbbbbbbbb".into(),
                    vec![ip("10.80.2.5"), ip("10.80.1.2")],
                ),
                ("aaaaaaaaaaaaaaaa".into(), vec![ip("10.80.1.3")]),
                // Never a set: bad id, or nobody in it.
                ("Robert'); drop".into(), vec![ip("10.80.1.9")]),
                ("cccccccccccccccc".into(), vec![]),
            ],
            relay_hub: false,
            vips: vec![],
        };
        let expected = "table inet modula
delete table inet modula
table inet modula {
\tset nodes {
\t\ttype ipv4_addr
\t\telements = { 10.80.2.1, 10.80.3.1 }
\t}
\tset env_aaaaaaaaaaaaaaaa {
\t\ttype ipv4_addr
\t\telements = { 10.80.1.3 }
\t}
\tset env_bbbbbbbbbbbbbbbb {
\t\ttype ipv4_addr
\t\telements = { 10.80.1.2, 10.80.2.5 }
\t}
\tchain forward {
\t\ttype filter hook forward priority filter - 1; policy accept;
\t\tiifname \"modula0\" oifname != \"modula-mesh\" drop
\t\toifname \"modula0\" iifname != \"modula-mesh\" drop
\t\tiifname \"modula-mesh\" meta nfproto ipv6 drop
\t\tiifname \"modula-mesh\" ip saddr != 10.80.1.0/24 drop
\t\tiifname \"modula-mesh\" ip saddr 10.80.1.1 drop
\t\tiifname \"modula-mesh\" ip daddr != 10.80.0.0/16 drop
\t\tiifname != \"modula-mesh\" oifname != \"modula-mesh\" accept
\t\tct state established,related accept
\t\tip saddr @nodes accept
\t\tip saddr @env_aaaaaaaaaaaaaaaa ip daddr @env_aaaaaaaaaaaaaaaa accept
\t\tip saddr @env_bbbbbbbbbbbbbbbb ip daddr @env_bbbbbbbbbbbbbbbb accept
\t\tcounter drop
\t}
\tchain input {
\t\ttype filter hook input priority filter - 1; policy accept;
\t\tct state established,related accept
\t\tiifname \"modula0\" ip saddr != @nodes drop
\t\tiifname \"modula-mesh\" ip daddr 10.80.1.1 meta l4proto { tcp, udp } th dport 53 accept
\t\tiifname \"modula-mesh\" drop
\t}
}
";
        assert_eq!(render(&spec), expected);

        // The relay hub forwards tunnel to tunnel inside the mesh, before
        // the rule that sends tunnel traffic only to the bridge.
        let hub = render(&Spec {
            relay_hub: true,
            ..spec
        });
        assert_eq!(
            hub,
            expected.replace(
                "\t\tiifname \"modula0\" oifname != \"modula-mesh\" drop\n",
                "\t\tiifname \"modula0\" oifname \"modula0\" ip saddr 10.80.0.0/16 ip daddr 10.80.0.0/16 accept\n\
                 \t\tiifname \"modula0\" oifname != \"modula-mesh\" drop\n"
            )
        );
    }

    #[test]
    fn a_single_node_has_empty_sets() {
        let r = render(&Spec {
            subnet: Subnet { index: 4 },
            peers: vec![],
            environments: vec![],
            relay_hub: false,
            vips: vec![],
        });
        assert!(
            r.contains("\tset nodes {\n\t\ttype ipv4_addr\n\t}\n"),
            "{r}"
        );
        assert!(!r.contains("@env_"));
        assert!(!r.contains("nat hook"));
    }

    #[test]
    fn virtual_ips_balance_over_healthy_replicas_of_their_environment() {
        let env = "aaaaaaaaaaaaaaaa";
        let vip = |v: &str, backends: &[&str]| Vip {
            environment: env.into(),
            vip: ip(v),
            backends: backends.iter().map(|b| ip(b)).collect(),
        };
        let r = render(&Spec {
            subnet: Subnet { index: 1 },
            peers: vec![ip("10.80.2.1")],
            environments: vec![(
                env.into(),
                vec![ip("10.80.1.2"), ip("10.80.2.2"), ip("10.80.1.5")],
            )],
            relay_hub: false,
            vips: vec![
                vip("10.80.255.2", &["10.80.2.2", "10.80.1.2"]),
                // No healthy replica: no rule (the address answers nothing).
                vip("10.80.255.3", &[]),
                // An environment without members here: no set, no rule.
                Vip {
                    environment: "bbbbbbbbbbbbbbbb".into(),
                    ..vip("10.80.255.4", &["10.80.3.2"])
                },
                vip("10.80.255.1", &["10.80.1.5"]),
            ],
        });
        let nat = &r[r.find("\tchain prerouting").unwrap()..];
        assert_eq!(
            nat,
            "\tchain prerouting {
\t\ttype nat hook prerouting priority dstnat - 10; policy accept;
\t\tiifname \"modula-mesh\" ip saddr @env_aaaaaaaaaaaaaaaa ip daddr 10.80.255.1 dnat ip to numgen random mod 1 map { 0 : 10.80.1.5 }
\t\tiifname \"modula-mesh\" ip saddr @env_aaaaaaaaaaaaaaaa ip daddr 10.80.255.2 dnat ip to numgen random mod 2 map { 0 : 10.80.1.2, 1 : 10.80.2.2 }
\t}
\tchain postrouting {
\t\ttype nat hook postrouting priority srcnat - 10; policy accept;
\t\toifname \"modula-mesh\" ip saddr 10.80.1.0/24 ct status dnat masquerade
\t}
}
"
        );
    }
}
