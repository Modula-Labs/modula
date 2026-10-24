//! Private networking (C9): this node's part of the WireGuard mesh.
//!
//! The server gives every node a /24 of `10.80.0.0/16` and its peers. Here
//! that becomes:
//!   * a WireGuard interface (`modula0`, kernel module) holding the node's
//!     `.1`, with one peer and one route per other node's /24;
//!   * a `modula-mesh` Docker network (internal, subnet the whole /16,
//!     gateway the node's `.1`, proxy ARP on) that every container joins
//!     next to its environment's bridge, with the address the server picked.
//!     Containers keep their default route and Docker's names for local
//!     services on the environment bridge; only mesh traffic uses this one;
//!   * nftables (`table inet modula`) letting only addresses of the same
//!     environment talk across the mesh, on every node ([`nft`]); checked
//!     every minute and reloaded when what the host has differs (an
//!     external `nft flush ruleset`, a Docker restart dropping its rules);
//!   * a resolver on the `.1` that answers `<service>.modula.internal` with
//!     the healthy replicas on other servers and forwards everything else
//!     ([`dns`]).
//!
//! Nothing here is fatal: without WireGuard or nftables the node warns and
//! keeps running workloads as a single server does.

pub mod addrs;
pub mod dns;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod nft;

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use modula_proto::node::v1 as pb;

/// The WireGuard interface.
pub const INTERFACE: &str = "modula0";
/// The Docker network (and its bridge) containers join for the mesh.
pub const BRIDGE: &str = "modula-mesh";
pub const DEFAULT_PORT: u16 = 51820;
pub const KEEPALIVE_SECS: u16 = 25;
/// WireGuard's overhead on IPv4.
const WG_OVERHEAD: u32 = 80;
/// Addresses the node hands to one-off containers (pre-deploy commands):
/// Docker's `ip_range` of the mesh network, which the server never assigns.
const LEASE_FIRST: u8 = 192;
const LEASE_LAST: u8 = 254;
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// How often the loaded firewall is compared with the host's.
const FIREWALL_CHECK_EVERY: Duration = Duration::from_secs(60);

/// A node's /24: `10.80.<index>.0/24`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Subnet {
    pub index: u8,
}

impl Subnet {
    /// Parses `10.80.<1–254>.0/24`.
    pub fn parse(s: &str) -> Option<Self> {
        let ip: Ipv4Addr = s.strip_suffix("/24")?.parse().ok()?;
        match ip.octets() {
            [10, 80, index @ 1..=254, 0] => Some(Self { index }),
            _ => None,
        }
    }

    pub fn host(self, h: u8) -> Ipv4Addr {
        Ipv4Addr::new(10, 80, self.index, h)
    }

    /// The node's own address (WireGuard and the bridge's gateway).
    pub fn gateway(self) -> Ipv4Addr {
        self.host(1)
    }

    pub fn contains(self, ip: Ipv4Addr) -> bool {
        matches!(ip.octets(), [10, 80, i, _] if i == self.index)
    }

    /// Docker's dynamic range in the mesh network (one-off containers).
    pub fn lease_range(self) -> String {
        format!("10.80.{}.{LEASE_FIRST}/26", self.index)
    }
}

impl std::fmt::Display for Subnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "10.80.{}.0/24", self.index)
    }
}

/// The WireGuard side: our address and port, and one peer per other node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunnels {
    pub address: Ipv4Addr,
    pub port: u16,
    pub mtu: u32,
    pub peers: Vec<TunnelPeer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelPeer {
    pub public_key: String,
    pub endpoint: Option<SocketAddr>,
    pub subnet: Subnet,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PeerStat {
    pub public_key: String,
    pub last_handshake_unix: i64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// What actually touches the host (Linux: [`linux::LinuxMesh`]).
#[async_trait]
pub trait MeshSystem: Send + Sync + 'static {
    /// Bring the WireGuard interface to exactly `t` (peers and routes).
    async fn apply_tunnels(&self, t: &Tunnels) -> anyhow::Result<()>;
    /// Load `ruleset` (see [`nft::render`]) and the host settings it relies on.
    async fn apply_firewall(&self, subnet: Subnet, ruleset: &str) -> anyhow::Result<()>;
    /// What the host has of the firewall [`MeshSystem::apply_firewall`]
    /// loads, as text that is the same between two calls unless something
    /// changed it (counters aside, see [`nft::normalize`]); None when the
    /// table is gone.
    async fn firewall_state(&self) -> anyhow::Result<Option<String>>;
    async fn peer_stats(&self) -> anyhow::Result<Vec<PeerStat>>;
}

/// Records what it was asked to do (tests). The host's firewall state is
/// the last ruleset loaded, until a test changes `host_firewall`.
#[derive(Default)]
pub struct RecordingSystem {
    pub tunnels: Mutex<Option<Tunnels>>,
    pub rulesets: Mutex<Vec<String>>,
    pub host_firewall: Mutex<Option<String>>,
}

#[async_trait]
impl MeshSystem for RecordingSystem {
    async fn apply_tunnels(&self, t: &Tunnels) -> anyhow::Result<()> {
        *self.tunnels.lock().expect("lock") = Some(t.clone());
        Ok(())
    }
    async fn apply_firewall(&self, _subnet: Subnet, ruleset: &str) -> anyhow::Result<()> {
        self.rulesets.lock().expect("lock").push(ruleset.to_owned());
        *self.host_firewall.lock().expect("lock") = Some(ruleset.to_owned());
        Ok(())
    }
    async fn firewall_state(&self) -> anyhow::Result<Option<String>> {
        Ok(self.host_firewall.lock().expect("lock").clone())
    }
    async fn peer_stats(&self) -> anyhow::Result<Vec<PeerStat>> {
        Ok(Vec::new())
    }
}

/// A peer as the last desired state named it.
#[derive(Debug, Clone)]
struct Peer {
    node_id: String,
    public_key: String,
    subnet: Subnet,
}

#[derive(Default)]
struct Current {
    subnet: Option<Subnet>,
    tunnels: Option<Tunnels>,
    ruleset: Option<String>,
    /// The host's firewall state right after `ruleset` loaded (normalized).
    host_firewall: Option<String>,
    /// Environments' members (the firewall's sets), from the server.
    environments: Vec<(String, Vec<Ipv4Addr>)>,
    peers: Vec<Peer>,
    dns_started: bool,
}

pub struct Mesh {
    sys: Arc<dyn MeshSystem>,
    public_key: String,
    port: u16,
    current: Mutex<Current>,
    /// Serialises firewall loads (a lease's release runs in the background).
    firewall: tokio::sync::Mutex<()>,
    book: Arc<dns::Book>,
    /// One-off containers' addresses → their environment.
    leases: Mutex<BTreeMap<Ipv4Addr, String>>,
}

/// An address on the mesh for one one-off container, in its environment's
/// firewall set until dropped.
pub struct Lease {
    mesh: Arc<Mesh>,
    pub ip: Ipv4Addr,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.mesh
            .leases
            .lock()
            .expect("lease lock")
            .remove(&self.ip);
        let mesh = self.mesh.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { mesh.refresh_firewall().await });
        }
    }
}

impl Mesh {
    pub fn new(sys: Arc<dyn MeshSystem>, public_key: String, port: u16) -> Arc<Self> {
        Arc::new(Self {
            sys,
            public_key,
            port,
            current: Mutex::new(Current::default()),
            firewall: tokio::sync::Mutex::new(()),
            book: Arc::new(dns::Book::default()),
            leases: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Below the host's uplink by WireGuard's overhead.
    pub fn mtu() -> u32 {
        crate::runtime::docker::host_mtu()
            .unwrap_or(1500)
            .saturating_sub(WG_OVERHEAD)
    }

    pub fn subnet(&self) -> Option<Subnet> {
        self.current.lock().expect("mesh lock").subnet
    }

    /// Upstream servers containers fall back to when the node's resolver
    /// doesn't answer (the host's own, unless only loopback).
    pub fn fallback_dns() -> Vec<String> {
        dns::host_upstreams()
            .into_iter()
            .filter(|a| !a.ip().is_loopback())
            .map(|a| a.ip().to_string())
            .collect()
    }

    /// Converge to the server's view of the mesh. Call after the mesh
    /// network exists (its gateway is the address everything binds to).
    pub async fn apply(self: &Arc<Self>, spec: &pb::Mesh) -> anyhow::Result<Subnet> {
        let subnet = Subnet::parse(&spec.subnet)
            .ok_or_else(|| anyhow::anyhow!("invalid mesh subnet {:?}", spec.subnet))?;
        let mut peers = Vec::new();
        let mut tunnel_peers = Vec::new();
        for p in &spec.peers {
            let Some(peer_subnet) = Subnet::parse(&p.subnet).filter(|s| *s != subnet) else {
                tracing::warn!(peer = %p.node_id, "ignoring a mesh peer with an invalid subnet");
                continue;
            };
            if !valid_key(&p.public_key) {
                tracing::warn!(peer = %p.node_id, "ignoring a mesh peer with an invalid key");
                continue;
            }
            peers.push(Peer {
                node_id: p.node_id.clone(),
                public_key: p.public_key.clone(),
                subnet: peer_subnet,
            });
            tunnel_peers.push(TunnelPeer {
                public_key: p.public_key.clone(),
                endpoint: p.endpoint.parse().ok(),
                subnet: peer_subnet,
            });
        }
        let tunnels = Tunnels {
            address: subnet.gateway(),
            port: u16::try_from(spec.listen_port)
                .ok()
                .filter(|p| *p > 0)
                .unwrap_or(self.port),
            mtu: Self::mtu(),
            peers: tunnel_peers,
        };
        let environments = members_by_environment(spec);
        let changed = {
            let c = self.current.lock().expect("mesh lock");
            c.tunnels.as_ref() != Some(&tunnels)
        };
        // Without tunnels (no WireGuard here) the firewall and resolver still
        // come up: containers already use the resolver, which lives on the
        // bridge. Tunnels are retried on the next apply.
        let tunnels = if !changed {
            Some(tunnels)
        } else {
            match self.sys.apply_tunnels(&tunnels).await {
                Ok(()) => {
                    tracing::info!(subnet = %subnet, peers = tunnels.peers.len(), "mesh tunnels configured");
                    Some(tunnels)
                }
                Err(e) => {
                    tracing::warn!(
                        error = format!("{e:#}"),
                        "cannot configure the mesh tunnels; will retry"
                    );
                    None
                }
            }
        };
        let start_dns = {
            let mut c = self.current.lock().expect("mesh lock");
            c.subnet = Some(subnet);
            c.tunnels = tunnels;
            c.environments = environments;
            c.peers = peers.clone();
            !std::mem::replace(&mut c.dns_started, true)
        };
        self.book.update(dns::BookData::from_spec(
            spec,
            subnet,
            &self.leases.lock().expect("lease lock"),
            peers.iter().map(|p| p.subnet.gateway()).collect(),
        ));
        self.refresh_firewall().await;
        if start_dns {
            let book = self.book.clone();
            tokio::spawn(async move {
                if let Err(e) = dns::serve(book, subnet.gateway()).await {
                    tracing::warn!(error = format!("{e:#}"), "the mesh resolver stopped");
                }
            });
            let mesh = Arc::downgrade(self);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(FIREWALL_CHECK_EVERY).await;
                    let Some(mesh) = mesh.upgrade() else { return };
                    mesh.check_firewall().await;
                }
            });
        }
        Ok(subnet)
    }

    /// Reloads the firewall when the host's differs from what was loaded
    /// (flushed, or edited by something else). Idempotent.
    pub async fn check_firewall(&self) {
        let expected = {
            let c = self.current.lock().expect("mesh lock");
            if c.ruleset.is_none() {
                return;
            }
            c.host_firewall.clone()
        };
        let now = match self.sys.firewall_state().await {
            Ok(s) => s.map(|s| nft::normalize(&s)),
            Err(e) => {
                tracing::debug!(error = format!("{e:#}"), "cannot read the mesh firewall");
                return;
            }
        };
        if now.is_some() && now == expected {
            return;
        }
        tracing::warn!("the mesh firewall changed outside Modula; reloading it");
        self.current.lock().expect("mesh lock").ruleset = None;
        self.refresh_firewall().await;
    }

    /// The firewall for the current state and leases, loaded if it changed.
    async fn refresh_firewall(&self) {
        let _one = self.firewall.lock().await;
        let (subnet, ruleset) = {
            let c = self.current.lock().expect("mesh lock");
            let Some(subnet) = c.subnet else { return };
            let mut envs = c.environments.clone();
            for (ip, env) in self.leases.lock().expect("lease lock").iter() {
                match envs.iter_mut().find(|(e, _)| e == env) {
                    Some((_, ips)) => ips.push(*ip),
                    None => envs.push((env.clone(), vec![*ip])),
                }
            }
            let spec = nft::Spec {
                subnet,
                peers: c.peers.iter().map(|p| p.subnet.gateway()).collect(),
                environments: envs,
            };
            let ruleset = nft::render(&spec);
            if c.ruleset.as_deref() == Some(ruleset.as_str()) {
                return;
            }
            (subnet, ruleset)
        };
        match self.sys.apply_firewall(subnet, &ruleset).await {
            Ok(()) => {
                let host = self
                    .sys
                    .firewall_state()
                    .await
                    .ok()
                    .flatten()
                    .map(|s| nft::normalize(&s));
                let mut c = self.current.lock().expect("mesh lock");
                c.ruleset = Some(ruleset);
                c.host_firewall = host;
            }
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                "cannot load the mesh firewall; will retry"
            ),
        }
    }

    /// An address for a one-off container of `environment` (in its firewall
    /// set once this returns), or None outside a mesh or when all are taken.
    pub async fn lease(self: &Arc<Self>, environment: &str) -> Option<Lease> {
        let subnet = self.subnet()?;
        let ip = {
            let mut leases = self.leases.lock().expect("lease lock");
            let ip = (LEASE_FIRST..=LEASE_LAST)
                .map(|h| subnet.host(h))
                .find(|ip| !leases.contains_key(ip))?;
            leases.insert(ip, environment.to_owned());
            ip
        };
        self.book.add_local(ip, environment);
        self.refresh_firewall().await;
        Some(Lease {
            mesh: self.clone(),
            ip,
        })
    }

    /// Tunnel counters and a round trip to every peer's resolver.
    pub async fn report(&self) -> Option<pb::MeshReport> {
        let peers = {
            let c = self.current.lock().expect("mesh lock");
            c.subnet?;
            c.peers.clone()
        };
        let stats = match self.sys.peer_stats().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = format!("{e:#}"), "cannot read tunnel counters");
                Vec::new()
            }
        };
        let probes = futures::future::join_all(
            peers
                .iter()
                .map(|p| dns::probe(p.subnet.gateway(), PROBE_TIMEOUT)),
        )
        .await;
        Some(pb::MeshReport {
            peers: peers
                .iter()
                .zip(probes)
                .map(|(p, rtt)| {
                    let s = stats
                        .iter()
                        .find(|s| s.public_key == p.public_key)
                        .cloned()
                        .unwrap_or_default();
                    pb::PeerStatus {
                        node_id: p.node_id.clone(),
                        latency_us: rtt
                            .map_or(0, |d| d.as_micros().clamp(1, u128::from(u32::MAX)) as u32),
                        last_handshake_unix: s.last_handshake_unix,
                        rx_bytes: s.rx_bytes,
                        tx_bytes: s.tx_bytes,
                    }
                })
                .collect(),
        })
    }
}

/// The environments' member addresses, as the firewall's sets.
fn members_by_environment(spec: &pb::Mesh) -> Vec<(String, Vec<Ipv4Addr>)> {
    spec.environments
        .iter()
        .filter(|e| nft::valid_set_id(&e.environment_id))
        .map(|e| {
            let mut ips: Vec<Ipv4Addr> = e
                .members
                .iter()
                .filter_map(|m| m.ip.parse::<Ipv4Addr>().ok())
                .filter(|ip| matches!(ip.octets(), [10, 80, 1..=254, 2..=254]))
                .collect();
            ips.sort();
            ips.dedup();
            (e.environment_id.clone(), ips)
        })
        .collect()
}

/// A WireGuard key: 32 bytes, standard base64.
pub fn valid_key(k: &str) -> bool {
    use base64::Engine as _;
    k.len() == 44
        && base64::engine::general_purpose::STANDARD
            .decode(k)
            .is_ok_and(|b| b.len() == 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "YAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";

    fn spec() -> pb::Mesh {
        pb::Mesh {
            subnet: "10.80.1.0/24".into(),
            listen_port: 51820,
            peers: vec![
                pb::MeshPeer {
                    node_id: "b".into(),
                    public_key: KEY.into(),
                    endpoint: "172.30.0.3:51820".into(),
                    subnet: "10.80.2.0/24".into(),
                    ..Default::default()
                },
                pb::MeshPeer {
                    node_id: "bad".into(),
                    public_key: "nope".into(),
                    subnet: "10.80.3.0/24".into(),
                    ..Default::default()
                },
            ],
            environments: vec![pb::MeshEnvironment {
                environment_id: "0000000000000env".into(),
                members: vec![pb::MeshMember {
                    ip: "10.80.2.2".into(),
                    service_name: "db".into(),
                    healthy: true,
                    ..Default::default()
                }],
            }],
        }
    }

    #[test]
    fn subnets() {
        let s = Subnet::parse("10.80.7.0/24").unwrap();
        assert_eq!(s.gateway(), Ipv4Addr::new(10, 80, 7, 1));
        assert_eq!(s.to_string(), "10.80.7.0/24");
        assert_eq!(s.lease_range(), "10.80.7.192/26");
        assert!(s.contains(Ipv4Addr::new(10, 80, 7, 200)));
        assert!(!s.contains(Ipv4Addr::new(10, 80, 8, 2)));
        for bad in [
            "10.80.0.0/24",
            "10.80.255.0/24",
            "10.81.1.0/24",
            "10.80.1.0/16",
            "x",
        ] {
            assert_eq!(Subnet::parse(bad), None, "{bad}");
        }
    }

    #[tokio::test]
    async fn renders_peer_config_and_firewall() {
        let sys = Arc::new(RecordingSystem::default());
        let mesh = Mesh::new(sys.clone(), KEY.into(), 51820);
        let subnet = mesh.apply(&spec()).await.unwrap();
        assert_eq!(subnet.index, 1);
        let t = sys.tunnels.lock().unwrap().clone().unwrap();
        assert_eq!(t.address, Ipv4Addr::new(10, 80, 1, 1));
        assert_eq!(t.port, 51820);
        // The peer with a broken key is left out.
        assert_eq!(
            t.peers,
            [TunnelPeer {
                public_key: KEY.into(),
                endpoint: Some("172.30.0.3:51820".parse().unwrap()),
                subnet: Subnet { index: 2 },
            }]
        );
        let rules = sys.rulesets.lock().unwrap().clone();
        assert_eq!(rules.len(), 1);
        assert!(
            rules[0].contains("elements = { 10.80.2.1 }"),
            "{}",
            rules[0]
        );
        assert!(rules[0].contains("set env_0000000000000env"));

        // The same state again changes nothing.
        mesh.apply(&spec()).await.unwrap();
        assert_eq!(sys.rulesets.lock().unwrap().len(), 1);

        // A lease joins its environment's set until dropped.
        let lease = mesh.lease("0000000000000env").await.unwrap();
        assert_eq!(lease.ip, Ipv4Addr::new(10, 80, 1, 192));
        let with = sys.rulesets.lock().unwrap().last().unwrap().clone();
        assert!(with.contains("10.80.1.192"), "{with}");
        drop(lease);
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        let after = sys.rulesets.lock().unwrap().last().unwrap().clone();
        assert!(!after.contains("10.80.1.192"), "{after}");
    }

    /// No WireGuard on this host.
    #[derive(Default)]
    struct NoTunnels(RecordingSystem);

    #[async_trait]
    impl MeshSystem for NoTunnels {
        async fn apply_tunnels(&self, _t: &Tunnels) -> anyhow::Result<()> {
            anyhow::bail!("no wireguard module")
        }
        async fn apply_firewall(&self, subnet: Subnet, ruleset: &str) -> anyhow::Result<()> {
            self.0.apply_firewall(subnet, ruleset).await
        }
        async fn firewall_state(&self) -> anyhow::Result<Option<String>> {
            self.0.firewall_state().await
        }
        async fn peer_stats(&self) -> anyhow::Result<Vec<PeerStat>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn a_firewall_changed_outside_is_reloaded() {
        let sys = Arc::new(RecordingSystem::default());
        let mesh = Mesh::new(sys.clone(), KEY.into(), 51820);
        mesh.apply(&spec()).await.unwrap();
        let loaded = || sys.rulesets.lock().unwrap().len();
        assert_eq!(loaded(), 1);
        // Unchanged (counters moved): nothing to do.
        let host = sys.host_firewall.lock().unwrap().clone().unwrap();
        *sys.host_firewall.lock().unwrap() =
            Some(host.replace("counter drop", "counter packets 12 bytes 960 drop"));
        mesh.check_firewall().await;
        assert_eq!(loaded(), 1);
        // `nft flush ruleset`: the table is gone.
        *sys.host_firewall.lock().unwrap() = None;
        mesh.check_firewall().await;
        assert_eq!(loaded(), 2);
        assert_eq!(sys.rulesets.lock().unwrap()[1], host);
        // Edited: a rule removed.
        *sys.host_firewall.lock().unwrap() = Some(host.replace("\t\tcounter drop\n", ""));
        mesh.check_firewall().await;
        assert_eq!(loaded(), 3);
        mesh.check_firewall().await;
        assert_eq!(loaded(), 3);
    }

    #[tokio::test]
    async fn without_wireguard_the_firewall_and_resolver_still_come_up() {
        let sys = Arc::new(NoTunnels::default());
        let mesh = Mesh::new(sys.clone(), KEY.into(), 51820);
        assert_eq!(mesh.apply(&spec()).await.unwrap().index, 1);
        assert_eq!(sys.0.rulesets.lock().unwrap().len(), 1);
        // Tunnels are retried, not remembered as applied.
        assert!(mesh.current.lock().unwrap().tunnels.is_none());
    }
}
