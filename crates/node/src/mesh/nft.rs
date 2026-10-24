//! The mesh firewall: one nftables table, replaced atomically on change.
//!
//! Only decisions to *drop* live here: Docker's own (iptables) FORWARD
//! chain still has to accept mesh traffic, which `linux.rs` arranges with
//! two `DOCKER-USER` rules. Both chains see every packet; a drop in either wins.
//!
//! What it enforces, on every node:
//!   * only the mesh bridge sends into the tunnel, and only from this node's
//!     own container addresses (no container can borrow the host's identity
//!     through an environment bridge's masquerade, or spoof another's);
//!   * what arrives through the tunnel only goes to the mesh bridge;
//!   * a new connection is allowed when source and destination are in the
//!     same environment's set, or the source is a node (Caddy balancing over
//!     replicas on other servers);
//!   * containers reach nothing of the host over the mesh but its resolver,
//!     and other nodes' containers nothing of it at all (replies to the
//!     host's own connections, e.g. Caddy's, excepted).

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
\t\tiifname \"{INTERFACE}\" oifname != \"{BRIDGE}\" drop
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
}}
"
    );
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
    }

    #[test]
    fn a_single_node_has_empty_sets() {
        let r = render(&Spec {
            subnet: Subnet { index: 4 },
            peers: vec![],
            environments: vec![],
        });
        assert!(
            r.contains("\tset nodes {\n\t\ttype ipv4_addr\n\t}\n"),
            "{r}"
        );
        assert!(!r.contains("@env_"));
    }
}
