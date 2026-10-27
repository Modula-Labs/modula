//! The mesh on a Linux host: kernel WireGuard over netlink
//! (`defguard_wireguard_rs`), routes and nftables through their own tools
//! (argv only), and the Docker/ufw rules the mesh needs.
//!
//! The private key is generated here once, kept in the node's directory
//! (`wg.key`, 0600) and never leaves the node.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;

use anyhow::{Context, bail};
use async_trait::async_trait;
use defguard_wireguard_rs::key::Key;
use defguard_wireguard_rs::net::IpAddrMask;
use defguard_wireguard_rs::peer::Peer;
use defguard_wireguard_rs::{InterfaceConfiguration, Kernel, WGApi, WireguardInterfaceApi};
use tokio::io::AsyncWriteExt;

use super::{BRIDGE, INTERFACE, KEEPALIVE_SECS, MeshSystem, PeerStat, Subnet, Tunnels, VIP_RANGE};
use crate::access::run;

const KEY_FILE: &str = "wg.key";

pub struct LinuxMesh {
    private_key: Key,
    /// Routes we added (one per peer /24), to drop the ones of removed peers.
    routes: Mutex<Vec<Subnet>>,
}

impl LinuxMesh {
    /// Loads this node's key (creating it the first time) and checks that
    /// WireGuard is usable. Returns the system and its public key (base64).
    pub fn open(dir: &Path) -> anyhow::Result<(Self, String)> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let path = dir.join(KEY_FILE);
        let private_key = match std::fs::read_to_string(&path) {
            Ok(s) => Key::try_from(s.trim()).map_err(|e| anyhow::anyhow!("{path:?}: {e:?}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Key::generate();
                write_secret(&path, b64.encode(key.as_slice()).as_bytes())?;
                key
            }
            Err(e) => return Err(e).with_context(|| format!("cannot read {path:?}")),
        };
        let public = b64.encode(private_key.public_key().as_slice());
        Ok((
            Self {
                private_key,
                routes: Mutex::new(Vec::new()),
            },
            public,
        ))
    }
}

fn write_secret(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot write {tmp:?}"))?;
    f.write_all(contents)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn wg_error(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("WireGuard: {e}")
}

#[async_trait]
impl MeshSystem for LinuxMesh {
    async fn apply_tunnels(&self, t: &Tunnels) -> anyhow::Result<()> {
        let key = self.private_key.clone();
        let t2 = t.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut api = WGApi::<Kernel>::new(INTERFACE).map_err(wg_error)?;
            if api.read_interface_data().is_err() {
                api.create_interface().map_err(wg_error).context(
                    "cannot create the WireGuard interface (is the wireguard kernel module available?)",
                )?;
            }
            let peers: Vec<Peer> = t2
                .peers
                .iter()
                .filter_map(|p| {
                    let mut peer = Peer::new(Key::try_from(p.public_key.as_str()).ok()?);
                    peer.endpoint = p.endpoint;
                    peer.persistent_keepalive_interval = Some(KEEPALIVE_SECS);
                    // Its /24 and, on the hub's peer, the relayed nodes'
                    // (replaced on every configure).
                    peer.allowed_ips = std::iter::once(p.subnet)
                        .chain(p.via.iter().copied())
                        .map(|s| IpAddrMask::new(IpAddr::V4(s.host(0)), 24))
                        .collect();
                    Some(peer)
                })
                .collect();
            let config = InterfaceConfiguration {
                name: INTERFACE.into(),
                prvkey: key.to_string(),
                addresses: vec![IpAddrMask::new(IpAddr::V4(t2.address), 32)],
                port: t2.port,
                peers: peers.clone(),
                mtu: Some(t2.mtu),
                fwmark: None,
            };
            api.configure_interface(&config).map_err(wg_error)?;
            // configure_interface adds peers; drop the ones no longer wanted.
            if let Ok(host) = api.read_interface_data() {
                for k in host.peers.keys() {
                    if !peers.iter().any(|p| &p.public_key == k) {
                        api.remove_peer(k).map_err(wg_error)?;
                    }
                }
            }
            Ok(())
        })
        .await??;
        // Interface up, then one route per peer's /24, relayed ones too (the
        // source is our address on it; WireGuard picks the tunnel). Not the
        // crate's peer routing: it also pins endpoints to the default
        // gateway, which breaks private paths.
        run("ip", &["link", "set", INTERFACE, "up"]).await?;
        let wanted: Vec<Subnet> = t
            .peers
            .iter()
            .flat_map(|p| std::iter::once(p.subnet).chain(p.via.iter().copied()))
            .collect();
        let src = t.address.to_string();
        for s in &wanted {
            run(
                "ip",
                &[
                    "route",
                    "replace",
                    &s.to_string(),
                    "dev",
                    INTERFACE,
                    "src",
                    &src,
                ],
            )
            .await?;
        }
        let stale: Vec<Subnet> = {
            let mut routes = self.routes.lock().expect("routes lock");
            let stale = routes
                .iter()
                .filter(|s| !wanted.contains(s))
                .copied()
                .collect();
            *routes = wanted;
            stale
        };
        for s in stale {
            let _ = run("ip", &["route", "del", &s.to_string(), "dev", INTERFACE]).await;
        }
        Ok(())
    }

    async fn apply_firewall(&self, subnet: Subnet, ruleset: &str) -> anyhow::Result<()> {
        // Containers ask the bridge for other nodes' addresses (they are
        // on-link in the /16); the host answers for those it routes.
        std::fs::write(format!("/proc/sys/net/ipv4/conf/{BRIDGE}/proxy_arp"), "1")
            .context("cannot enable proxy ARP on the mesh bridge")?;
        // Our table first: Docker's firewall only lets the mesh through
        // once the table that isolates it is in place.
        nft_load(ruleset).await?;
        for (i, o) in DOCKER_USER {
            if !docker_user_has(i, o).await {
                run(
                    "iptables",
                    &["-w", "-I", "DOCKER-USER", "-i", i, "-o", o, "-j", "ACCEPT"],
                )
                .await
                .context("cannot let mesh traffic through Docker's firewall")?;
            }
        }
        // Services' virtual IPs are local, so the host answers ARP for them
        // on the bridge and the nat chain sees the packets.
        run(
            "ip",
            &[
                "route", "replace", "local", VIP_RANGE, "dev", "lo", "table", "local",
            ],
        )
        .await
        .context("cannot route the services' virtual IPs")?;
        open_ufw(subnet).await;
        Ok(())
    }

    async fn firewall_state(&self) -> anyhow::Result<Option<String>> {
        // A missing table (or nft) reads as gone: the reload says why.
        let Ok(mut state) = run("nft", &["list", "table", "inet", super::nft::TABLE]).await else {
            return Ok(None);
        };
        for (i, o) in DOCKER_USER {
            let has = docker_user_has(i, o).await;
            state.push_str(&format!("\ndocker-user {i} {o} {has}"));
        }
        let arp = std::fs::read_to_string(format!("/proc/sys/net/ipv4/conf/{BRIDGE}/proxy_arp"))
            .unwrap_or_default();
        state.push_str(&format!("\nproxy_arp {}", arp.trim()));
        let vips = run("ip", &["route", "show", "table", "local", VIP_RANGE])
            .await
            .unwrap_or_default();
        state.push_str(&format!("\nvip route {}", vips.trim()));
        Ok(Some(state))
    }

    async fn peer_stats(&self) -> anyhow::Result<Vec<PeerStat>> {
        tokio::task::spawn_blocking(|| {
            use base64::Engine as _;
            let api = WGApi::<Kernel>::new(INTERFACE).map_err(wg_error)?;
            let host = api.read_interface_data().map_err(wg_error)?;
            Ok(host
                .peers
                .values()
                .map(|p| PeerStat {
                    public_key: base64::engine::general_purpose::STANDARD
                        .encode(p.public_key.as_slice()),
                    last_handshake_unix: p
                        .last_handshake
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_secs() as i64),
                    rx_bytes: p.rx_bytes,
                    tx_bytes: p.tx_bytes,
                })
                .collect())
        })
        .await?
    }
}

/// Docker's FORWARD chain drops traffic into its networks and, with its
/// policy, anything it forwards; these let the mesh through, tunnel to
/// tunnel for the relay hub, bridge to bridge for a virtual IP whose replica
/// is on this node (the mesh network has inter-container traffic off;
/// `table inet modula` decides what may pass).
const DOCKER_USER: [(&str, &str); 4] = [
    (INTERFACE, BRIDGE),
    (BRIDGE, INTERFACE),
    (INTERFACE, INTERFACE),
    (BRIDGE, BRIDGE),
];

async fn docker_user_has(i: &str, o: &str) -> bool {
    run(
        "iptables",
        &["-w", "-C", "DOCKER-USER", "-i", i, "-o", o, "-j", "ACCEPT"],
    )
    .await
    .is_ok()
}

/// `nft -f -` with the script on stdin (one atomic transaction).
async fn nft_load(script: &str) -> anyhow::Result<()> {
    let mut child = tokio::process::Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run nft (install nftables)")?;
    let mut stdin = child.stdin.take().context("nft stdin")?;
    stdin.write_all(script.as_bytes()).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "nft rejected the mesh firewall: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// With ufw active, open the WireGuard port and the resolver on the mesh
/// bridge (ufw's default-deny would drop both). Idempotent; never fatal.
async fn open_ufw(subnet: Subnet) {
    let active = run("ufw", &["status"])
        .await
        .is_ok_and(|s| s.starts_with("Status: active"));
    if !active {
        return;
    }
    let gw: Ipv4Addr = subnet.gateway();
    let gw = gw.to_string();
    let rules: [&[&str]; 2] = [
        &["allow", "in", "on", BRIDGE, "to", &gw, "port", "53"],
        &["allow", "in", "on", INTERFACE],
    ];
    for r in rules {
        if let Err(e) = run("ufw", r).await {
            tracing::warn!(error = format!("{e:#}"), "cannot open ufw for the mesh");
        }
    }
}

/// With ufw active, open the WireGuard UDP port (once, at startup).
pub async fn open_ufw_port(port: u16) {
    let active = run("ufw", &["status"])
        .await
        .is_ok_and(|s| s.starts_with("Status: active"));
    if active {
        let rule = format!("{port}/udp");
        if let Err(e) = run("ufw", &["allow", &rule]).await {
            tracing::warn!(
                error = format!("{e:#}"),
                "cannot open the WireGuard port in ufw"
            );
        }
    }
}
