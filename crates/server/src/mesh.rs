//! Private networking between servers (C9).
//!
//! Every node that reports a WireGuard key joins one mesh: the server gives
//! it a /24 of `10.80.0.0/16` (`10.80.<mesh_index>.0/24`, host address `.1`)
//! and every other node it may share workloads with as a peer. Container
//! addresses are assigned here too (`.2`–`.191`; the node keeps `.192/26` for
//! one-off containers), so every node knows which address belongs to which
//! environment: that drives the nodes' nftables isolation and their
//! `<service>.modula.internal` resolvers.
//!
//! Path per pair of nodes: an address both have in one private subnet (a
//! Hetzner Cloud Network, a VPC) if there is one, else the peer's public IP.
//! WireGuard runs over either, so both are encrypted and handled alike.

use std::net::Ipv4Addr;

use modula_id::Id;
use modula_proto::node::v1 as pb;
use sqlx::{PgConnection, PgExecutor, PgPool};

use crate::deploy::state::LIVE;
use crate::error::{AppError, AppResult};

pub const DEFAULT_WG_PORT: u16 = 51820;
/// Node /24s are 10.80.1.0/24 … 10.80.254.0/24.
const FIRST_INDEX: i16 = 1;
const LAST_INDEX: i16 = 254;
/// Container addresses the server assigns within a node's /24.
const FIRST_HOST: i16 = 2;
const LAST_HOST: i16 = 191;
/// Cap on what a node may report (addresses, peers).
const MAX_REPORTED: usize = 32;

/// `10.80.<index>.0/24`.
pub fn subnet(index: i16) -> String {
    format!("10.80.{index}.0/24")
}

/// The lowest number in `first..=last` not in `used`.
pub fn lowest_free(used: &[i16], first: i16, last: i16) -> Option<i16> {
    (first..=last).find(|i| !used.contains(i))
}

/// The host part of a container address in a node's /24 (`10.80.3.7` → 7).
fn host_of(ip: &str, index: i16) -> Option<i16> {
    let ip: Ipv4Addr = ip.parse().ok()?;
    let [a, b, c, d] = ip.octets();
    ((a, b, i16::from(c)) == (10, 80, index)).then_some(i16::from(d))
}

/// What the server knows about a node's networking.
#[derive(Debug, Clone, Default)]
pub struct NodeNet {
    pub id: Id,
    pub name: String,
    pub public_key: String,
    pub port: u16,
    pub public_ip: Option<String>,
    /// "ip/prefix" of the node's interfaces.
    pub addresses: Vec<String>,
    pub index: i16,
}

fn parse_cidr(s: &str) -> Option<(Ipv4Addr, u8)> {
    let (ip, prefix) = s.split_once('/')?;
    let prefix: u8 = prefix.parse().ok()?;
    (prefix <= 32).then_some((ip.parse().ok()?, prefix))
}

fn network(ip: Ipv4Addr, prefix: u8) -> u32 {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    u32::from(ip) & mask
}

/// Addresses worth peering over privately: RFC 1918 and shared (CGNAT)
/// space, never the mesh itself.
fn is_private(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    (ip.is_private() && !(a == 10 && b == 80)) || (a == 100 && (64..128).contains(&b))
}

/// Where `me` sends to reach `peer` (`ip:port`; empty when it can't, then
/// the peer reaches us), and whether that is a private path.
pub fn endpoint(me: &NodeNet, peer: &NodeNet) -> (String, bool) {
    let port = if peer.port == 0 {
        DEFAULT_WG_PORT
    } else {
        peer.port
    };
    for mine in me.addresses.iter().filter_map(|a| parse_cidr(a)) {
        for theirs in peer.addresses.iter().filter_map(|a| parse_cidr(a)) {
            let same = mine.1 == theirs.1
                && mine.1 < 32
                && network(mine.0, mine.1) == network(theirs.0, theirs.1)
                && mine.0 != theirs.0;
            if same && is_private(theirs.0) {
                return (format!("{}:{port}", theirs.0), true);
            }
        }
    }
    match peer
        .public_ip
        .as_deref()
        .and_then(|ip| ip.parse::<Ipv4Addr>().ok())
    {
        Some(ip) if ip != Ipv4Addr::UNSPECIFIED => (format!("{ip}:{port}"), false),
        _ => (String::new(), false),
    }
}

/// A WireGuard public key as nodes send it: 32 bytes, standard base64.
fn valid_key(k: &str) -> bool {
    use base64::Engine as _;
    k.len() == 44
        && base64::engine::general_purpose::STANDARD
            .decode(k)
            .is_ok_and(|b| b.len() == 32)
}

/// Record what a node said about its networking in Hello, giving it a /24
/// the first time. True when its peers' view of it changed (bump them).
pub async fn record_hello(db: &PgPool, node: Id, hello: &pb::Hello) -> AppResult<bool> {
    let info = hello.info.clone().unwrap_or_default();
    if !hello.capabilities.iter().any(|c| c == "mesh") || !valid_key(&info.wg_public_key) {
        return Ok(false);
    }
    let addresses: Vec<String> = info
        .addresses
        .iter()
        .filter_map(|a| parse_cidr(a))
        .take(MAX_REPORTED)
        .map(|(ip, p)| format!("{ip}/{p}"))
        .collect();
    let port = u16::try_from(info.wg_port)
        .ok()
        .filter(|p| *p > 0)
        .unwrap_or(DEFAULT_WG_PORT);
    let mut tx = db.begin().await?;
    #[derive(sqlx::FromRow)]
    struct Before {
        wg_public_key: Option<String>,
        wg_port: Option<i32>,
        addresses: serde_json::Value,
        mesh_index: Option<i16>,
    }
    let before: Option<Before> = sqlx::query_as(
        "SELECT wg_public_key, wg_port, addresses, mesh_index FROM nodes
             WHERE id = $1 AND status <> 'revoked' FOR UPDATE",
    )
    .bind(node)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(Before {
        wg_public_key: key,
        wg_port: old_port,
        addresses: old_addresses,
        mesh_index: index,
    }) = before
    else {
        return Ok(false);
    };
    let addresses_json = serde_json::json!(addresses);
    let index = match index {
        Some(i) => i,
        None => allocate_index(&mut tx).await?,
    };
    sqlx::query(
        "UPDATE nodes SET wg_public_key = $2, wg_port = $3, addresses = $4, mesh_index = $5 WHERE id = $1",
    )
    .bind(node)
    .bind(&info.wg_public_key)
    .bind(i32::from(port))
    .bind(&addresses_json)
    .bind(index)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(key.as_deref() != Some(info.wg_public_key.as_str())
        || old_port != Some(i32::from(port))
        || old_addresses != addresses_json)
}

/// The lowest free /24. Serialized by a transaction-level advisory lock so
/// two nodes saying Hello at once never get the same one.
async fn allocate_index(tx: &mut PgConnection) -> AppResult<i16> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('modula.mesh_index'))")
        .execute(&mut *tx)
        .await?;
    let used: Vec<i16> =
        sqlx::query_scalar("SELECT mesh_index FROM nodes WHERE mesh_index IS NOT NULL")
            .fetch_all(&mut *tx)
            .await?;
    lowest_free(&used, FIRST_INDEX, LAST_INDEX)
        .ok_or_else(|| AppError::Conflict("the private network is full (254 servers)".into()))
}

/// Every other node in the mesh whose peer list includes `node` (bump them
/// after `node`'s networking changed). Returns the ids.
pub async fn bump_peers(db: impl PgExecutor<'_>, node: Id) -> AppResult<Vec<Id>> {
    Ok(sqlx::query_scalar(
        "UPDATE nodes p SET desired_generation = p.desired_generation + 1
         FROM nodes me
         WHERE me.id = $1 AND p.id <> me.id AND p.status <> 'revoked' AND p.mesh_index IS NOT NULL
           AND (me.workspace_id IS NULL OR p.workspace_id IS NULL OR p.workspace_id = me.workspace_id)
         RETURNING p.id",
    )
    .bind(node)
    .fetch_all(db)
    .await?)
}

/// Bump every node running something of `environment` (its address book
/// changed). Returns them to kick after commit.
pub async fn bump_environment(tx: &mut PgConnection, environment: Id) -> AppResult<Vec<Id>> {
    Ok(sqlx::query_scalar(
        "UPDATE nodes SET desired_generation = desired_generation + 1
         WHERE id IN (SELECT r.node_id FROM deployment_replicas r
                      JOIN deployments d ON d.id = r.deployment_id
                      JOIN service_instances i ON i.id = d.service_instance_id
                      WHERE i.environment_id = $1 AND d.status = ANY($2) AND r.node_id IS NOT NULL)
         RETURNING id",
    )
    .bind(environment)
    .bind(LIVE)
    .fetch_all(&mut *tx)
    .await?)
}

/// A free container address on `node`, or None when the node isn't in the
/// mesh. Locks the node row so concurrent deployments get different ones.
pub async fn allocate_ip(tx: &mut PgConnection, node: Id) -> AppResult<Option<String>> {
    let index: Option<i16> = sqlx::query_scalar(
        "SELECT mesh_index FROM nodes WHERE id = $1 AND wg_public_key IS NOT NULL FOR UPDATE",
    )
    .bind(node)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    let Some(index) = index else { return Ok(None) };
    let taken: Vec<String> = sqlx::query_scalar(
        "SELECT r.mesh_ip FROM deployment_replicas r JOIN deployments d ON d.id = r.deployment_id
         WHERE r.node_id = $1 AND r.mesh_ip IS NOT NULL
           AND d.status IN ('queued', 'building', 'deploying', 'active', 'crashed')",
    )
    .bind(node)
    .fetch_all(&mut *tx)
    .await?;
    let used: Vec<i16> = taken.iter().filter_map(|ip| host_of(ip, index)).collect();
    let host = lowest_free(&used, FIRST_HOST, LAST_HOST).ok_or_else(|| {
        AppError::Conflict("this server has no private addresses left (190 containers)".into())
    })?;
    Ok(Some(format!("10.80.{index}.{host}")))
}

#[derive(sqlx::FromRow)]
struct NodeRow {
    id: Id,
    name: String,
    wg_public_key: String,
    wg_port: Option<i32>,
    public_ip: Option<String>,
    addresses: serde_json::Value,
    mesh_index: i16,
}

impl From<NodeRow> for NodeNet {
    fn from(r: NodeRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            public_key: r.wg_public_key,
            port: r
                .wg_port
                .and_then(|p| u16::try_from(p).ok())
                .unwrap_or(DEFAULT_WG_PORT),
            public_ip: r.public_ip,
            addresses: serde_json::from_value(r.addresses).unwrap_or_default(),
            index: r.mesh_index,
        }
    }
}

macro_rules! node_columns {
    () => {
        "n.id, n.name, n.wg_public_key, n.wg_port, n.public_ip, n.addresses, n.mesh_index"
    };
}

/// This node and its peers, or None when it isn't in the mesh.
pub async fn nodes(db: &PgPool, node: Id) -> AppResult<Option<(NodeNet, Vec<NodeNet>)>> {
    let me: Option<NodeRow> = sqlx::query_as(concat!(
        "SELECT ",
        node_columns!(),
        " FROM nodes n
         WHERE n.id = $1 AND n.status <> 'revoked' AND n.mesh_index IS NOT NULL AND n.wg_public_key IS NOT NULL"
    ))
    .bind(node)
    .fetch_optional(db)
    .await?;
    let Some(me) = me else { return Ok(None) };
    let peers: Vec<NodeRow> = sqlx::query_as(concat!(
        "SELECT ",
        node_columns!(),
        " FROM nodes n, nodes me
         WHERE me.id = $1 AND n.id <> me.id AND n.status <> 'revoked'
           AND n.mesh_index IS NOT NULL AND n.wg_public_key IS NOT NULL
           AND (me.workspace_id IS NULL OR n.workspace_id IS NULL OR n.workspace_id = me.workspace_id)
         ORDER BY n.mesh_index"
    ))
    .bind(node)
    .fetch_all(db)
    .await?;
    Ok(Some((
        me.into(),
        peers.into_iter().map(Into::into).collect(),
    )))
}

/// One mesh address of an environment.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Member {
    pub environment_id: Id,
    pub ip: String,
    pub service_name: String,
    pub deployment_id: Id,
    pub node_id: Id,
    pub healthy: bool,
}

/// Every mesh address of the environments `node` runs something of.
pub async fn members(db: &PgPool, node: Id) -> AppResult<Vec<Member>> {
    Ok(sqlx::query_as(
        "SELECT i.environment_id, r.mesh_ip AS ip, s.name AS service_name, d.id AS deployment_id,
                r.node_id, (d.status IN ('active', 'crashed') AND r.status = 'active') AS healthy
         FROM deployment_replicas r
         JOIN deployments d ON d.id = r.deployment_id
         JOIN service_instances i ON i.id = d.service_instance_id
         JOIN services s ON s.id = i.service_id
         WHERE r.mesh_ip IS NOT NULL AND r.node_id IS NOT NULL AND d.status = ANY($2)
           AND i.environment_id IN (
               SELECT i2.environment_id FROM deployment_replicas r2
               JOIN deployments d2 ON d2.id = r2.deployment_id
               JOIN service_instances i2 ON i2.id = d2.service_instance_id
               WHERE r2.node_id = $1 AND d2.status = ANY($2))
         ORDER BY i.environment_id, r.mesh_ip",
    )
    .bind(node)
    .bind(LIVE)
    .fetch_all(db)
    .await?)
}

/// The mesh section of `node`'s desired state.
pub fn render(me: &NodeNet, peers: &[NodeNet], members: &[Member]) -> pb::Mesh {
    let mut environments: Vec<pb::MeshEnvironment> = Vec::new();
    for m in members {
        let env = m.environment_id.to_string();
        if environments.last().is_none_or(|e| e.environment_id != env) {
            environments.push(pb::MeshEnvironment {
                environment_id: env,
                members: Vec::new(),
            });
        }
        if let Some(e) = environments.last_mut() {
            e.members.push(pb::MeshMember {
                ip: m.ip.clone(),
                service_name: m.service_name.clone(),
                deployment_id: m.deployment_id.to_string(),
                node_id: m.node_id.to_string(),
                healthy: m.healthy,
            });
        }
    }
    pb::Mesh {
        subnet: subnet(me.index),
        listen_port: u32::from(me.port),
        peers: peers
            .iter()
            .map(|p| {
                let (endpoint, private_path) = endpoint(me, p);
                pb::MeshPeer {
                    node_id: p.id.to_string(),
                    name: p.name.clone(),
                    public_key: p.public_key.clone(),
                    endpoint,
                    subnet: subnet(p.index),
                    private_path,
                }
            })
            .collect(),
        environments,
    }
}

pub async fn build(db: &PgPool, node: Id) -> AppResult<Option<pb::Mesh>> {
    let Some((me, peers)) = nodes(db, node).await? else {
        return Ok(None);
    };
    let members = members(db, node).await?;
    Ok(Some(render(&me, &peers, &members)))
}

/// A node's report on its tunnels, kept for the Network tab.
pub async fn record_report(db: &PgPool, node: Id, report: &pb::MeshReport) -> AppResult<()> {
    let peers: Vec<serde_json::Value> = report
        .peers
        .iter()
        .filter(|p| p.node_id.parse::<Id>().is_ok())
        .take(256)
        .map(|p| {
            serde_json::json!({
                "nodeId": p.node_id,
                "latencyUs": p.latency_us,
                "lastHandshake": p.last_handshake_unix,
                "rxBytes": p.rx_bytes,
                "txBytes": p.tx_bytes,
            })
        })
        .collect();
    sqlx::query("UPDATE nodes SET mesh_status = $2 WHERE id = $1")
        .bind(node)
        .bind(serde_json::json!({ "at": time::OffsetDateTime::now_utc().unix_timestamp(), "peers": peers }))
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(index: i16, public: Option<&str>, addrs: &[&str]) -> NodeNet {
        NodeNet {
            id: Id::new(),
            name: format!("n{index}"),
            public_key: "k".into(),
            port: 51820,
            public_ip: public.map(str::to_owned),
            addresses: addrs.iter().map(|s| (*s).to_owned()).collect(),
            index,
        }
    }

    #[test]
    fn allocates_the_lowest_free_number() {
        assert_eq!(lowest_free(&[], 1, 254), Some(1));
        assert_eq!(lowest_free(&[1, 2, 4], 1, 254), Some(3));
        assert_eq!(lowest_free(&[2, 3], 2, 3), None);
        let full: Vec<i16> = (1..=254).collect();
        assert_eq!(lowest_free(&full, FIRST_INDEX, LAST_INDEX), None);
        assert_eq!(subnet(7), "10.80.7.0/24");
        assert_eq!(host_of("10.80.7.9", 7), Some(9));
        assert_eq!(host_of("10.80.8.9", 7), None);
        assert_eq!(host_of("nope", 7), None);
    }

    #[test]
    fn prefers_a_shared_private_network() {
        // Hetzner: public /32 on eth0, the Cloud Network on ens10.
        let a = node(1, Some("203.0.113.1"), &["203.0.113.1/32", "10.0.0.2/24"]);
        let b = node(2, Some("203.0.113.2"), &["203.0.113.2/32", "10.0.0.3/24"]);
        assert_eq!(endpoint(&a, &b), ("10.0.0.3:51820".into(), true));
        assert_eq!(endpoint(&b, &a), ("10.0.0.2:51820".into(), true));
    }

    #[test]
    fn falls_back_to_the_public_address() {
        let a = node(1, Some("203.0.113.1"), &["203.0.113.1/32", "10.0.0.2/24"]);
        // A private address in another network, a public /24 neighbour.
        let mut c = node(3, Some("198.51.100.7"), &["10.1.0.2/24", "198.51.100.7/24"]);
        assert_eq!(endpoint(&a, &c), ("198.51.100.7:51820".into(), false));
        c.port = 4000;
        assert_eq!(endpoint(&a, &c).0, "198.51.100.7:4000");
        // Nothing to dial: the peer dials us.
        let d = node(4, None, &["10.9.0.2/24"]);
        assert_eq!(endpoint(&a, &d), (String::new(), false));
        // The mesh's own range is never a path.
        let e = node(5, None, &["10.80.5.1/16"]);
        let f = node(6, None, &["10.80.6.1/16"]);
        assert_eq!(endpoint(&e, &f), (String::new(), false));
    }

    #[test]
    fn renders_peers_and_environment_members() {
        let a = node(1, Some("203.0.113.1"), &["172.30.0.2/16"]);
        let b = node(2, Some("203.0.113.2"), &["172.30.0.3/16"]);
        let env = Id::new();
        let m = |ip: &str, svc: &str, healthy| Member {
            environment_id: env,
            ip: ip.into(),
            service_name: svc.into(),
            deployment_id: Id::new(),
            node_id: b.id,
            healthy,
        };
        let mesh = render(
            &a,
            std::slice::from_ref(&b),
            &[m("10.80.2.2", "db", true), m("10.80.2.3", "api", false)],
        );
        assert_eq!(mesh.subnet, "10.80.1.0/24");
        assert_eq!(mesh.listen_port, 51820);
        assert_eq!(mesh.peers.len(), 1);
        let p = &mesh.peers[0];
        assert_eq!(
            (p.endpoint.as_str(), p.private_path),
            ("172.30.0.3:51820", true)
        );
        assert_eq!(p.subnet, "10.80.2.0/24");
        assert_eq!(mesh.environments.len(), 1);
        let ips: Vec<(&str, bool)> = mesh.environments[0]
            .members
            .iter()
            .map(|m| (m.ip.as_str(), m.healthy))
            .collect();
        assert_eq!(ips, [("10.80.2.2", true), ("10.80.2.3", false)]);
    }

    #[test]
    fn keys_must_be_32_bytes_of_base64() {
        assert!(valid_key("YAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk="));
        assert!(!valid_key("short"));
        assert!(!valid_key("YAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBm=="));
    }
}
