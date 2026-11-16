//! The private network through Hetzner firewalls: WireGuard (UDP 51820) is
//! open on Modula's firewalls only to Modula's own servers.
//!
//! Two kinds of Modula firewalls exist in a Hetzner project: the one per
//! workspace (and SSH choice) its provisioned workers share, and the main
//! server's (made by the `modula` CLI, `role=main`). Each carries one rule
//! described [`MESH_RULE`] whose sources are the public IPs of the servers
//! that peer with the ones behind it; `sync_mesh` rewrites that rule (and
//! only it) whenever a server joins or leaves the mesh.

use super::client::{FirewallRule, MESH_RULE};
use crate::mesh::DEFAULT_WG_PORT;

/// The WireGuard rule for `sources` (public IPv4s), or None when there are
/// none (Hetzner refuses an inbound rule without sources).
pub fn mesh_rule(sources: &[String]) -> Option<FirewallRule> {
    let mut cidrs: Vec<String> = sources.iter().map(|ip| format!("{ip}/32")).collect();
    cidrs.sort();
    cidrs.dedup();
    let port = DEFAULT_WG_PORT.to_string();
    (!cidrs.is_empty()).then(|| FirewallRule::inbound("udp", Some(&port), cidrs, MESH_RULE))
}

/// `rules` with their WireGuard rule replaced by the one for `sources`.
pub fn with_mesh_rule(rules: &[FirewallRule], sources: &[String]) -> Vec<FirewallRule> {
    let mut out: Vec<FirewallRule> = rules
        .iter()
        .filter(|r| r.description.as_deref() != Some(MESH_RULE))
        .cloned()
        .collect();
    out.extend(mesh_rule(sources));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_wireguard_rule_is_replaced() {
        let web = FirewallRule::inbound_any("tcp", Some("443"), "HTTPS");
        let old = mesh_rule(&["203.0.113.1".into()]).unwrap();
        let rules = with_mesh_rule(
            &[web.clone(), old],
            &["203.0.113.3".into(), "203.0.113.2".into()],
        );
        assert_eq!(rules[0], web);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[1].source_ips, ["203.0.113.2/32", "203.0.113.3/32"]);
        // No servers left: no rule (Hetzner refuses one without sources).
        assert_eq!(with_mesh_rule(&rules, &[]), [web]);
    }
}
