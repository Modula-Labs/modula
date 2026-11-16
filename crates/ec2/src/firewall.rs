//! Security groups, kept in the shape every EC2 quota allows.
//!
//! An instance has at most [`GROUPS_PER_INSTANCE`] security groups and a
//! group [`RULES_PER_GROUP`] IPv4 rules, where one source is one rule. Each
//! firewall is therefore a *set* of up to five groups attached together,
//! tagged `modula-slot` 0–4: slot 0 holds the fixed rules, and WireGuard
//! sources fill whatever room the set has.

use std::collections::BTreeSet;

use crate::IpRule;

pub const RULES_PER_GROUP: usize = 60;
pub const GROUPS_PER_INSTANCE: usize = 5;
/// `modula-node`'s WireGuard port.
pub const WG_PORT: i32 = 51820;
/// Description of the rules letting Modula's servers reach WireGuard.
pub const MESH_RULE: &str = "Modula private network";

fn anywhere(protocol: &str, from: i32, to: i32, description: &str) -> IpRule {
    IpRule {
        protocol: protocol.to_owned(),
        from,
        to,
        // Instances in a default VPC have no IPv6 address to protect.
        cidrs: vec!["0.0.0.0/0".to_owned()],
        description: Some(description.to_owned()),
    }
}

/// What a worker serves; see `integrations::hetzner::firewall_rules`.
pub fn worker_rules(ssh: bool) -> Vec<IpRule> {
    let mut rules = vec![
        anywhere("tcp", 80, 80, "HTTP"),
        anywhere("tcp", 443, 443, "HTTPS"),
        anywhere("udp", 443, 443, "HTTP/3"),
        anywhere("tcp", 20000, 29999, "Modula TCP proxies"),
        anywhere("icmp", -1, -1, "Ping"),
    ];
    if ssh {
        rules.push(anywhere("tcp", 22, 22, "SSH"));
    }
    rules
}

/// A main server: a worker's rules plus SSH and the node endpoint.
pub fn main_rules(node_port: u16) -> Vec<IpRule> {
    let port = i32::from(node_port);
    let mut rules = worker_rules(true);
    rules.push(anywhere("tcp", port, port, "Modula nodes"));
    rules
}

/// The WireGuard rule for `cidrs`.
pub fn mesh_rule(cidrs: Vec<String>) -> IpRule {
    IpRule {
        protocol: "udp".to_owned(),
        from: WG_PORT,
        to: WG_PORT,
        cidrs,
        description: Some(MESH_RULE.to_owned()),
    }
}

/// One group's state, as far as the mesh is concerned.
#[derive(Debug, Clone, Default)]
pub struct Slot {
    /// Rules that aren't WireGuard sources (ours or the user's).
    pub others: usize,
    /// WireGuard source CIDRs it holds.
    pub mesh: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotChange {
    pub revoke: Vec<String>,
    pub authorize: Vec<String>,
}

/// `sources` as sorted, unique /32s.
pub fn mesh_cidrs(sources: &[String]) -> Vec<String> {
    let set: BTreeSet<String> = sources.iter().map(|ip| format!("{ip}/32")).collect();
    set.into_iter().collect()
}

/// What to change so `slots` hold exactly `desired`. Sources already in
/// place stay where they are, so a sync never opens a gap for a working
/// peer. `Err` is the number of sources that fit, nothing having changed.
pub fn plan_mesh(slots: &[Slot], desired: &[String]) -> Result<Vec<SlotChange>, usize> {
    let want: BTreeSet<&String> = desired.iter().collect();
    let mut placed: BTreeSet<&String> = BTreeSet::new();
    let mut changes = vec![SlotChange::default(); slots.len()];
    let mut free = vec![0usize; slots.len()];
    for (i, slot) in slots.iter().enumerate() {
        let mut kept = 0;
        for cidr in &slot.mesh {
            // Also drops a duplicate another slot already holds.
            if want.contains(cidr) && placed.insert(cidr) {
                kept += 1;
            } else {
                changes[i].revoke.push(cidr.clone());
            }
        }
        free[i] = RULES_PER_GROUP.saturating_sub(slot.others + kept);
    }
    let missing: Vec<&String> = desired.iter().filter(|c| !placed.contains(*c)).collect();
    let room: usize = free.iter().sum();
    if missing.len() > room {
        return Err(placed.len() + room);
    }
    let mut pending = missing.into_iter();
    for (i, room) in free.iter().enumerate() {
        changes[i]
            .authorize
            .extend(pending.by_ref().take(*room).cloned());
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ips(range: std::ops::Range<u32>) -> Vec<String> {
        range
            .map(|n| format!("203.0.{}.{}/32", n / 256, n % 256))
            .collect()
    }

    fn slot(others: usize, mesh: &[String]) -> Slot {
        Slot {
            others,
            mesh: mesh.iter().cloned().collect(),
        }
    }

    #[test]
    fn worker_rules_are_only_what_it_serves() {
        let summary = |ssh| {
            worker_rules(ssh)
                .into_iter()
                .map(|r| format!("{}/{}-{}", r.protocol, r.from, r.to))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            summary(false),
            [
                "tcp/80-80",
                "tcp/443-443",
                "udp/443-443",
                "tcp/20000-29999",
                "icmp/-1--1"
            ]
        );
        assert!(summary(true).contains(&"tcp/22-22".to_owned()));
        assert!(main_rules(7101).iter().any(|r| r.from == 7101));
    }

    #[test]
    fn sources_become_unique_slash_32s() {
        assert_eq!(
            mesh_cidrs(&[
                "203.0.113.2".into(),
                "203.0.113.1".into(),
                "203.0.113.2".into()
            ]),
            ["203.0.113.1/32", "203.0.113.2/32"]
        );
    }

    #[test]
    fn an_in_step_set_changes_nothing() {
        let a = ips(0..3);
        let plan = plan_mesh(&[slot(5, &a)], &a).unwrap();
        assert_eq!(plan, [SlotChange::default()]);
    }

    #[test]
    fn only_differences_are_applied() {
        let current = ips(0..3);
        let desired = ips(1..5);
        let plan = plan_mesh(&[slot(5, &current)], &desired).unwrap();
        assert_eq!(plan[0].revoke, ips(0..1));
        assert_eq!(plan[0].authorize, ips(3..5));
    }

    #[test]
    fn the_first_slot_keeps_room_for_its_fixed_rules() {
        let desired = ips(0..120);
        let slots = vec![slot(6, &[]), slot(0, &[]), slot(0, &[])];
        let plan = plan_mesh(&slots, &desired).unwrap();
        assert_eq!(plan[0].authorize.len(), 54);
        assert_eq!(plan[1].authorize.len(), 60);
        assert_eq!(plan[2].authorize.len(), 6);
    }

    #[test]
    fn peers_in_place_never_move() {
        let held: Vec<String> = ips(0..10);
        let mut desired = ips(0..10);
        desired.extend(ips(10..12));
        let slots = vec![slot(6, &[]), slot(0, &held)];
        let plan = plan_mesh(&slots, &desired).unwrap();
        assert!(plan[1].revoke.is_empty());
        assert_eq!(plan[0].authorize, ips(10..12));
    }

    #[test]
    fn a_duplicate_held_in_two_slots_is_dropped_once() {
        let a = ips(0..1);
        let plan = plan_mesh(&[slot(0, &a), slot(0, &a)], &a).unwrap();
        assert!(plan[0].revoke.is_empty());
        assert_eq!(plan[1].revoke, a);
    }

    #[test]
    fn beyond_capacity_nothing_is_planned() {
        let slots: Vec<Slot> = (0..5).map(|_| slot(0, &[])).collect();
        assert!(plan_mesh(&slots, &ips(0..300)).is_ok());
        assert_eq!(plan_mesh(&slots, &ips(0..301)), Err(300));
    }
}
