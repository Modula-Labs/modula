//! Docker Engine runtime, driven only through its API (never a shell).
//!
//! Security defaults for every container: all capabilities dropped except a
//! small allowlist, `no-new-privileges`, an init process, pids/memory/CPU
//! limits, and rotated JSON logs. Nothing is published on the host; each
//! environment gets its own bridge network.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::models::{
    ContainerCreateBody, ContainerSummaryStateEnum, EndpointIpamConfig, EndpointSettings,
    HostConfig, HostConfigLogConfig, Ipam, IpamConfig, Mount, MountType, NetworkConnectRequest,
    NetworkCreateRequest, NetworkingConfig, RestartPolicy as DockerRestart, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
    ListNetworksOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    RemoveVolumeOptionsBuilder, StatsOptionsBuilder,
};
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use modula_proto::node::v1::{RestartPolicy, Workload};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Container, LogLine, OutputSink, Runtime, labels};
use crate::mesh::{self, Subnet};

/// Docker's default set minus NET_RAW, MKNOD, AUDIT_WRITE and SYS_CHROOT.
const CAPABILITIES: &[&str] = &[
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "FSETID",
    "KILL",
    "SETGID",
    "SETUID",
    "SETPCAP",
    "NET_BIND_SERVICE",
];
/// Requests that wait on a stopping container (draining is at most 300s)
/// outlive bollard's two-minute default.
const STOP_REQUEST_TIMEOUT: Duration = Duration::from_secs(360);
/// Marks a one-off container (`run_once`); never listed as a workload.
const ONCE_LABEL: &str = "modula.once";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct DockerRuntime {
    docker: Docker,
    /// The mesh network, once [`Runtime::ensure_mesh_network`] made it.
    mesh: std::sync::Mutex<Option<MeshNet>>,
}

#[derive(Clone)]
struct MeshNet {
    subnet: Subnet,
    /// Containers' resolvers: the node's own, then the host's.
    dns: Vec<String>,
}

/// Marks the mesh network with the /24 it was made for.
const MESH_LABEL: &str = "modula.mesh";

impl DockerRuntime {
    /// Connects to the local engine (`DOCKER_HOST` or the default socket).
    pub fn connect() -> anyhow::Result<Self> {
        Ok(Self {
            docker: Docker::connect_with_local_defaults().context("cannot connect to Docker")?,
            mesh: std::sync::Mutex::new(None),
        })
    }

    fn mesh(&self) -> Option<MeshNet> {
        self.mesh.lock().expect("mesh lock").clone()
    }

    /// The mesh network's settings for a workload with an address on it.
    fn mesh_for(&self, w: &Workload) -> Option<(MeshNet, std::net::Ipv4Addr)> {
        let ip: std::net::Ipv4Addr = w.mesh_ip.parse().ok()?;
        let mesh = self.mesh()?;
        mesh.subnet.contains(ip).then_some((mesh, ip))
    }

    async fn connect_mesh(&self, container: &str, ip: std::net::Ipv4Addr) -> anyhow::Result<()> {
        self.docker
            .connect_network(
                mesh::BRIDGE,
                NetworkConnectRequest {
                    container: container.to_owned(),
                    endpoint_config: Some(EndpointSettings {
                        ipam_config: Some(EndpointIpamConfig {
                            ipv4_address: Some(ip.to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                },
            )
            .await
            .with_context(|| format!("cannot attach the container to the mesh as {ip}"))
    }
}

/// MTU of the interface carrying the default route (Linux), if known.
pub(crate) fn host_mtu() -> Option<u32> {
    let routes = std::fs::read_to_string("/proc/net/route").ok()?;
    let iface = routes.lines().skip(1).find_map(|l| {
        let mut f = l.split_whitespace();
        let (iface, dest) = (f.next()?, f.next()?);
        (dest == "00000000").then(|| iface.to_owned())
    })?;
    if iface.contains('/') || iface.contains("..") {
        return None;
    }
    std::fs::read_to_string(format!("/sys/class/net/{iface}/mtu"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn network_name(environment_id: &str) -> String {
    format!("modula-env-{environment_id}")
}

fn managed_filter() -> HashMap<String, Vec<String>> {
    HashMap::from([(
        "label".to_owned(),
        vec![format!("{}=true", labels::MANAGED)],
    )])
}

/// `name[:tag]` → (name, tag), defaulting to `latest`: the pull API would
/// otherwise fetch every tag. Digest references are passed through whole.
fn split_image(image: &str) -> (&str, &str) {
    if image.contains('@') {
        return (image, "");
    }
    let after_slash = image.rfind('/').map_or(0, |i| i + 1);
    match image[after_slash..].rfind(':') {
        Some(i) => (&image[..after_slash + i], &image[after_slash + i + 1..]),
        None => (image, "latest"),
    }
}

#[async_trait]
impl Runtime for DockerRuntime {
    async fn describe(&self) -> anyhow::Result<String> {
        let v = self.docker.version().await?;
        Ok(format!("docker {}", v.version.unwrap_or_default()))
    }

    async fn ensure_network(&self, environment_id: &str) -> anyhow::Result<()> {
        let name = network_name(environment_id);
        if self.docker.inspect_network(&name, None).await.is_ok() {
            return Ok(());
        }
        self.docker
            .create_network(NetworkCreateRequest {
                name,
                driver: Some("bridge".into()),
                // Containers must not send bigger packets than the host's
                // uplink carries (some clouds and nested setups are < 1500).
                options: host_mtu().filter(|m| *m < 1500).map(|m| {
                    HashMap::from([("com.docker.network.driver.mtu".to_owned(), m.to_string())])
                }),
                labels: Some(HashMap::from([
                    (labels::MANAGED.to_owned(), "true".to_owned()),
                    (labels::ENVIRONMENT.to_owned(), environment_id.to_owned()),
                ])),
                ..Default::default()
            })
            .await?;
        // A new bridge: make sure Docker's chain still drops metadata traffic.
        super::firewall::spawn_block_metadata();
        Ok(())
    }

    async fn remove_network(&self, environment_id: &str) -> anyhow::Result<()> {
        self.docker
            .remove_network(&network_name(environment_id))
            .await?;
        Ok(())
    }

    async fn networks(&self) -> anyhow::Result<Vec<String>> {
        let opts = ListNetworksOptionsBuilder::new()
            .filters(&managed_filter())
            .build();
        Ok(self
            .docker
            .list_networks(Some(opts))
            .await?
            .into_iter()
            .filter_map(|n| n.labels?.get(labels::ENVIRONMENT).cloned())
            .collect())
    }

    async fn ensure_mesh_network(&self, subnet: Subnet, mtu: u32) -> anyhow::Result<()> {
        let want = subnet.to_string();
        if let Ok(n) = self.docker.inspect_network(mesh::BRIDGE, None).await {
            let label = n.labels.as_ref().and_then(|l| l.get(MESH_LABEL)).cloned();
            if label.as_deref() == Some(want.as_str()) {
                *self.mesh.lock().expect("mesh lock") = Some(MeshNet {
                    subnet,
                    dns: mesh_dns(subnet),
                });
                return Ok(());
            }
            // Made for another /24 (the node was re-added): replace it.
            self.docker
                .remove_network(mesh::BRIDGE)
                .await
                .context("cannot replace the mesh network (containers still attached)")?;
        }
        self.docker
            .create_network(NetworkCreateRequest {
                name: mesh::BRIDGE.into(),
                driver: Some("bridge".into()),
                // No gateway route or masquerade: containers keep their
                // default route on their environment's bridge. The whole /16
                // is on-link; the host answers ARP for other nodes' /24s.
                internal: Some(true),
                ipam: Some(Ipam {
                    config: Some(vec![IpamConfig {
                        subnet: Some("10.80.0.0/16".into()),
                        ip_range: Some(subnet.lease_range()),
                        gateway: Some(subnet.gateway().to_string()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                options: Some(HashMap::from([
                    (
                        "com.docker.network.bridge.name".to_owned(),
                        mesh::BRIDGE.to_owned(),
                    ),
                    // Same-server traffic stays on environment bridges.
                    (
                        "com.docker.network.bridge.enable_icc".to_owned(),
                        "false".to_owned(),
                    ),
                    ("com.docker.network.driver.mtu".to_owned(), mtu.to_string()),
                ])),
                labels: Some(HashMap::from([
                    (labels::MANAGED.to_owned(), "true".to_owned()),
                    (MESH_LABEL.to_owned(), want),
                ])),
                ..Default::default()
            })
            .await
            .context("cannot create the mesh network")?;
        *self.mesh.lock().expect("mesh lock") = Some(MeshNet {
            subnet,
            dns: mesh_dns(subnet),
        });
        super::firewall::spawn_block_metadata();
        Ok(())
    }

    async fn attach_mesh(&self, container_id: &str, ip: std::net::Ipv4Addr) -> anyhow::Result<()> {
        self.connect_mesh(container_id, ip).await
    }

    async fn stats(&self, container_id: &str) -> anyhow::Result<Option<super::Stats>> {
        // Not one-shot: Docker samples twice a second apart, so the CPU
        // percentage covers that second.
        let opts = StatsOptionsBuilder::new()
            .stream(false)
            .one_shot(false)
            .build();
        let mut s = self.docker.stats(container_id, Some(opts));
        match s.next().await {
            Some(Ok(r)) => Ok(stats_from(&r)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    async fn containers(&self) -> anyhow::Result<Vec<Container>> {
        let opts = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&managed_filter())
            .build();
        let mut out = Vec::new();
        for c in self.docker.list_containers(Some(opts)).await? {
            let (Some(id), Some(labels)) = (c.id, c.labels) else {
                continue;
            };
            let running = c.state == Some(ContainerSummaryStateEnum::RUNNING);
            // Exit code and restart count need an inspect; only pay for it when stopped.
            let (exit_code, restart_count) = if running {
                (0, 0)
            } else {
                match self.docker.inspect_container(&id, None).await {
                    Ok(i) => (
                        i.state.and_then(|s| s.exit_code).unwrap_or(0),
                        i.restart_count.unwrap_or(0).max(0) as u32,
                    ),
                    Err(_) => (0, 0),
                }
            };
            let mesh_ip = c
                .network_settings
                .and_then(|n| n.networks)
                .and_then(|mut n| n.remove(mesh::BRIDGE))
                .and_then(|e| e.ip_address)
                .and_then(|ip| ip.parse().ok());
            out.push(Container {
                mesh_ip,
                deployment_id: labels.get(labels::DEPLOYMENT).cloned().unwrap_or_default(),
                service_id: labels.get(labels::SERVICE).cloned().unwrap_or_default(),
                environment_id: labels.get(labels::ENVIRONMENT).cloned().unwrap_or_default(),
                id,
                running,
                exit_code,
                restart_count,
            });
        }
        Ok(out)
    }

    async fn pull(&self, image: &str) -> anyhow::Result<()> {
        let (from, tag) = split_image(image);
        let opts = CreateImageOptionsBuilder::new()
            .from_image(from)
            .tag(tag)
            .build();
        self.docker
            .create_image(Some(opts), None, None)
            .try_for_each(|_| async { Ok(()) })
            .await
            .with_context(|| format!("cannot pull {image}"))
    }

    async fn create(&self, w: &Workload) -> anyhow::Result<String> {
        let net = network_name(&w.environment_id);
        let restart = match RestartPolicy::try_from(w.restart).unwrap_or(RestartPolicy::OnFailure) {
            RestartPolicy::Always => RestartPolicyNameEnum::ALWAYS,
            RestartPolicy::Never => RestartPolicyNameEnum::NO,
            RestartPolicy::OnFailure | RestartPolicy::RestartUnspecified => {
                RestartPolicyNameEnum::ON_FAILURE
            }
        };
        // Docker only accepts a retry limit with on-failure.
        let max_retries = (restart == RestartPolicyNameEnum::ON_FAILURE
            && w.restart_max_retries > 0)
            .then_some(i64::from(w.restart_max_retries));
        let host = HostConfig {
            restart_policy: Some(DockerRestart {
                name: Some(restart),
                maximum_retry_count: max_retries,
            }),
            // A named volume: Docker creates it on first use, empty, and it
            // outlives containers until the server asks for its removal.
            mounts: w.volume.as_ref().map(|v| {
                vec![Mount {
                    typ: Some(MountType::VOLUME),
                    source: Some(v.name.clone()),
                    target: Some(v.mount_path.clone()),
                    ..Default::default()
                }]
            }),
            ..host_config(w)
        };
        let mesh = self.mesh_for(w);
        let host = HostConfig {
            dns: mesh.as_ref().map(|(m, _)| m.dns.clone()),
            ..host
        };
        let body = ContainerCreateBody {
            // Every later stop (retire, restart) waits this long after SIGTERM.
            stop_timeout: (w.draining_seconds > 0).then_some(i64::from(w.draining_seconds)),
            image: Some(w.image.clone()),
            cmd: (!w.command.is_empty()).then(|| w.command.clone()),
            env: Some(
                w.env
                    .iter()
                    .map(|e| format!("{}={}", e.name, e.value))
                    .collect(),
            ),
            labels: Some(HashMap::from([
                (labels::MANAGED.to_owned(), "true".to_owned()),
                (labels::DEPLOYMENT.to_owned(), w.deployment_id.clone()),
                (labels::SERVICE.to_owned(), w.service_id.clone()),
                (labels::ENVIRONMENT.to_owned(), w.environment_id.clone()),
            ])),
            host_config: Some(host),
            networking_config: Some(NetworkingConfig {
                endpoints_config: Some(HashMap::from([(
                    net,
                    EndpointSettings {
                        aliases: Some(vec![
                            w.service_name.clone(),
                            format!("{}.modula.internal", w.service_name),
                        ]),
                        ..Default::default()
                    },
                )])),
            }),
            ..Default::default()
        };
        let name = format!("modula-{}-{}", w.service_name, w.deployment_id);
        let opts = CreateContainerOptionsBuilder::new().name(&name).build();
        let id = self.docker.create_container(Some(opts), body).await?.id;
        // The mesh endpoint has no aliases: names are per environment, and
        // the mesh network is shared by all of them.
        if let Some((_, ip)) = mesh
            && let Err(e) = self.connect_mesh(&id, ip).await
        {
            let _ = self
                .docker
                .remove_container(&id, Some(force_remove()))
                .await;
            return Err(e);
        }
        Ok(id)
    }

    async fn start(&self, container_id: &str) -> anyhow::Result<()> {
        self.docker.start_container(container_id, None).await?;
        Ok(())
    }

    async fn run_once(
        &self,
        w: &Workload,
        command: &[String],
        timeout: Duration,
        output: OutputSink,
    ) -> anyhow::Result<i64> {
        let name = format!("modula-once-{}", w.deployment_id);
        // A run cut short by a node restart may have left its container.
        let _ = self
            .docker
            .remove_container(&name, Some(force_remove()))
            .await;
        let body = ContainerCreateBody {
            image: Some(w.image.clone()),
            cmd: Some(command.to_vec()),
            env: Some(
                w.env
                    .iter()
                    .map(|e| format!("{}={}", e.name, e.value))
                    .collect(),
            ),
            labels: Some(HashMap::from([(
                ONCE_LABEL.to_owned(),
                w.deployment_id.clone(),
            )])),
            // On the environment network (databases resolve) but without the
            // service's aliases: nothing routes to it. On the mesh with the
            // address the node leased it, so services on other servers do.
            host_config: Some(HostConfig {
                restart_policy: Some(DockerRestart {
                    name: Some(RestartPolicyNameEnum::NO),
                    maximum_retry_count: None,
                }),
                dns: self.mesh_for(w).map(|(m, _)| m.dns),
                ..host_config(w)
            }),
            ..Default::default()
        };
        let opts = CreateContainerOptionsBuilder::new().name(&name).build();
        let id = self.docker.create_container(Some(opts), body).await?.id;
        let _cleanup = RemoveOnDrop {
            docker: self.docker.clone(),
            id: id.clone(),
        };
        if let Some((_, ip)) = self.mesh_for(w) {
            self.connect_mesh(&id, ip).await?;
        }
        let docker = self
            .docker
            .clone()
            .with_timeout(timeout + Duration::from_secs(30));
        let run = async {
            docker.start_container(&id, None).await?;
            let logs = LogsOptionsBuilder::new()
                .stdout(true)
                .stderr(true)
                .timestamps(true)
                .follow(true)
                .build();
            let pump = docker.logs(&id, Some(logs)).for_each(|item| {
                let lines = match item {
                    Ok(LogOutput::StdOut { message } | LogOutput::Console { message }) => {
                        parse_lines(&message, false)
                    }
                    Ok(LogOutput::StdErr { message }) => parse_lines(&message, true),
                    _ => vec![],
                };
                for line in lines.into_iter().flatten() {
                    output(line);
                }
                futures::future::ready(())
            });
            let wait = async {
                match docker.wait_container(&id, None).next().await {
                    Some(Ok(r)) => Ok(r.status_code),
                    Some(Err(bollard::errors::Error::DockerContainerWaitError {
                        code, ..
                    })) => Ok(code),
                    Some(Err(e)) => Err(anyhow::Error::from(e)),
                    None => anyhow::bail!("lost track of the command"),
                }
            };
            let ((), code) = tokio::join!(pump, wait);
            code
        };
        match tokio::time::timeout(timeout, run).await {
            Ok(code) => code,
            Err(_) => anyhow::bail!("timed out after {}s", timeout.as_secs()),
        }
    }

    async fn restart(&self, container_id: &str) -> anyhow::Result<()> {
        // No `t`: Docker waits the container's own stop timeout (draining).
        self.docker
            .clone()
            .with_timeout(STOP_REQUEST_TIMEOUT)
            .restart_container(container_id, None)
            .await?;
        Ok(())
    }

    async fn remove_volume(&self, name: &str) -> anyhow::Result<()> {
        let opts = RemoveVolumeOptionsBuilder::new().force(false).build();
        match self.docker.remove_volume(name, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn remove(&self, container_id: &str) -> anyhow::Result<()> {
        // SIGTERM, then SIGKILL after the container's stop timeout (its
        // draining time, set at create; Docker's 10s when unset).
        // Already-stopped containers make stop fail; removal is what matters.
        let _ = self
            .docker
            .clone()
            .with_timeout(STOP_REQUEST_TIMEOUT)
            .stop_container(container_id, None)
            .await;
        self.docker
            .remove_container(container_id, Some(force_remove()))
            .await?;
        Ok(())
    }

    async fn address(
        &self,
        container_id: &str,
        environment_id: &str,
    ) -> anyhow::Result<Option<std::net::IpAddr>> {
        let info = self.docker.inspect_container(container_id, None).await?;
        let ip = info
            .network_settings
            .and_then(|n| n.networks)
            .and_then(|mut n| n.remove(&network_name(environment_id)))
            .and_then(|e| e.ip_address)
            .filter(|ip| !ip.is_empty());
        Ok(match ip {
            Some(ip) => Some(ip.parse()?),
            None => None,
        })
    }

    async fn probe(
        &self,
        container_id: &str,
        environment_id: &str,
        port: u16,
        path: &str,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            path.starts_with('/') && path.bytes().all(|b| b.is_ascii_graphic()),
            "invalid health check path"
        );
        let ip = self
            .address(container_id, environment_id)
            .await?
            .context("container has no address yet")?;
        let addr = SocketAddr::new(ip, port);
        let attempt = async {
            let mut conn = tokio::net::TcpStream::connect(addr).await?;
            let req = format!(
                "GET {path} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: modula-node\r\nConnection: close\r\n\r\n"
            );
            conn.write_all(req.as_bytes()).await?;
            let mut head = [0u8; 16];
            let n = conn.read(&mut head).await?;
            // "HTTP/1.1 200 ..." → the status code's first digit.
            Ok::<bool, anyhow::Error>(n >= 12 && head.starts_with(b"HTTP/1.") && head[9] == b'2')
        };
        match tokio::time::timeout(PROBE_TIMEOUT, attempt).await {
            Ok(Ok(ok)) => Ok(ok),
            Ok(Err(_)) | Err(_) => Ok(false),
        }
    }

    async fn logs(
        &self,
        container_id: &str,
        tail: u32,
        follow: bool,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<LogLine>>> {
        let opts = LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            .timestamps(true)
            .follow(follow)
            .tail(&tail.to_string())
            .build();
        let stream = self.docker.logs(container_id, Some(opts)).flat_map(|item| {
            let lines: Vec<anyhow::Result<LogLine>> = match item {
                Ok(LogOutput::StdOut { message }) => parse_lines(&message, false),
                Ok(LogOutput::StdErr { message }) => parse_lines(&message, true),
                Ok(LogOutput::Console { message }) => parse_lines(&message, false),
                Ok(LogOutput::StdIn { .. }) => vec![],
                Err(e) => vec![Err(e.into())],
            };
            futures::stream::iter(lines)
        });
        Ok(stream.boxed())
    }

    async fn follow_since(
        &self,
        container_id: &str,
        since_unix_secs: i64,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<LogLine>>> {
        let opts = LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            .timestamps(true)
            .follow(true)
            .since(since_unix_secs.clamp(0, i32::MAX as i64) as i32)
            .build();
        let stream = self.docker.logs(container_id, Some(opts)).flat_map(|item| {
            let lines: Vec<anyhow::Result<LogLine>> = match item {
                Ok(LogOutput::StdOut { message }) => parse_lines(&message, false),
                Ok(LogOutput::StdErr { message }) => parse_lines(&message, true),
                Ok(LogOutput::Console { message }) => parse_lines(&message, false),
                Ok(LogOutput::StdIn { .. }) => vec![],
                Err(e) => vec![Err(e.into())],
            };
            futures::stream::iter(lines)
        });
        Ok(stream.boxed())
    }

    async fn container_env(&self, container_id: &str) -> anyhow::Result<Vec<(String, String)>> {
        super::docker_exec::container_env(&self.docker, container_id).await
    }

    async fn exec_output(
        &self,
        container_id: &str,
        exec: super::Exec,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<bytes::Bytes>>> {
        super::docker_exec::exec_output(&self.docker, container_id, exec).await
    }

    async fn exec_input(
        &self,
        container_id: &str,
        exec: super::Exec,
        stdin: BoxStream<'static, anyhow::Result<bytes::Bytes>>,
    ) -> anyhow::Result<()> {
        super::docker_exec::exec_input(&self.docker, container_id, exec, stdin).await
    }
}

/// What every container gets: limits, dropped capabilities, no new
/// privileges, an init, rotated logs, and only its environment's network.
/// The node's resolver first, the host's as a fallback (when it is down,
/// only names on other servers stop resolving).
fn mesh_dns(subnet: Subnet) -> Vec<String> {
    std::iter::once(subnet.gateway().to_string())
        .chain(mesh::Mesh::fallback_dns())
        .take(3)
        .collect()
}

fn host_config(w: &Workload) -> HostConfig {
    let limits = w.limits.unwrap_or_default();
    HostConfig {
        memory: (limits.memory_bytes > 0).then_some(limits.memory_bytes as i64),
        nano_cpus: (limits.nano_cpus > 0).then_some(limits.nano_cpus as i64),
        pids_limit: (limits.pids > 0).then_some(limits.pids),
        cap_drop: Some(vec!["ALL".into()]),
        cap_add: Some(CAPABILITIES.iter().map(|c| (*c).to_owned()).collect()),
        security_opt: Some(vec!["no-new-privileges:true".into()]),
        init: Some(true),
        log_config: Some(HostConfigLogConfig {
            typ: Some("json-file".into()),
            config: Some(HashMap::from([
                ("max-size".to_owned(), "10m".to_owned()),
                ("max-file".to_owned(), "3".to_owned()),
            ])),
        }),
        network_mode: Some(network_name(&w.environment_id)),
        ..Default::default()
    }
}

fn force_remove() -> bollard::query_parameters::RemoveContainerOptions {
    RemoveContainerOptionsBuilder::new()
        .force(true)
        .v(true)
        .build()
}

/// Removes a one-off container however its run ends (including when the
/// rollout waiting on it is aborted).
struct RemoveOnDrop {
    docker: Docker,
    id: String,
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let (docker, id) = (self.docker.clone(), std::mem::take(&mut self.id));
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = docker.remove_container(&id, Some(force_remove())).await;
            });
        }
    }
}

/// Docker prefixes each line with an RFC 3339 timestamp when asked to.
fn parse_lines(bytes: &[u8], stderr: bool) -> Vec<anyhow::Result<LogLine>> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| {
            let (ts, text) = line.split_once(' ').unwrap_or(("", line));
            let ts_unix_ms =
                time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339)
                    .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
                    .unwrap_or(0);
            Ok(LogLine {
                ts_unix_ms,
                stderr,
                text: text.to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_split() {
        assert_eq!(split_image("nginx"), ("nginx", "latest"));
        assert_eq!(split_image("nginx:1.27"), ("nginx", "1.27"));
        assert_eq!(
            split_image("localhost:5000/app"),
            ("localhost:5000/app", "latest")
        );
        assert_eq!(
            split_image("localhost:5000/app:v2"),
            ("localhost:5000/app", "v2")
        );
        let digest =
            "nginx@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(split_image(digest), (digest, ""));
    }

    #[test]
    fn log_lines() {
        let out = parse_lines(
            b"2026-10-23T10:00:00.5Z hello\n2026-10-23T10:00:01Z world\n",
            true,
        );
        let lines: Vec<LogLine> = out.into_iter().map(Result::unwrap).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "hello");
        assert_eq!(lines[0].ts_unix_ms % 1000, 500);
        assert!(lines[1].stderr);
    }
}

/// Docker's stats response as a [`super::Stats`]: CPU from the two samples'
/// deltas (like `docker stats`), memory without inactive page cache.
fn stats_from(r: &bollard::models::ContainerStatsResponse) -> Option<super::Stats> {
    let cpu = r.cpu_stats.as_ref()?;
    let pre = r.precpu_stats.as_ref();
    let total = |c: &bollard::models::ContainerCpuStats| {
        c.cpu_usage
            .as_ref()
            .and_then(|u| u.total_usage)
            .unwrap_or(0)
    };
    let cpu_delta = total(cpu).saturating_sub(pre.map_or(0, total)) as f64;
    let system_delta =
        cpu.system_cpu_usage
            .unwrap_or(0)
            .saturating_sub(pre.and_then(|p| p.system_cpu_usage).unwrap_or(0)) as f64;
    let online = f64::from(cpu.online_cpus.unwrap_or(1).max(1));
    let cpu_percent = if system_delta > 0.0 {
        cpu_delta / system_delta * online * 100.0
    } else {
        0.0
    };
    let memory_bytes = r.memory_stats.as_ref().map_or(0, |m| {
        let cache = m.stats.as_ref().map_or(0, |s| {
            s.get("inactive_file")
                .or_else(|| s.get("total_inactive_file"))
                .copied()
                .unwrap_or(0)
        });
        m.usage.unwrap_or(0).saturating_sub(cache)
    });
    let (net_rx_bytes, net_tx_bytes) = r.networks.as_ref().map_or((0, 0), |n| {
        n.values().fold((0, 0), |(rx, tx), i| {
            (rx + i.rx_bytes.unwrap_or(0), tx + i.tx_bytes.unwrap_or(0))
        })
    });
    Some(super::Stats {
        cpu_percent,
        memory_bytes,
        net_rx_bytes,
        net_tx_bytes,
    })
}

#[cfg(test)]
mod stats_tests {
    use super::*;

    #[test]
    fn stats_like_docker_stats() {
        let r: bollard::models::ContainerStatsResponse = serde_json::from_value(serde_json::json!({
            "cpu_stats": { "cpu_usage": { "total_usage": 3_000_000_000u64 }, "system_cpu_usage": 104_000_000_000u64, "online_cpus": 4 },
            "precpu_stats": { "cpu_usage": { "total_usage": 2_000_000_000u64 }, "system_cpu_usage": 100_000_000_000u64, "online_cpus": 4 },
            "memory_stats": { "usage": 300_000_000u64, "stats": { "inactive_file": 100_000_000u64 } },
            "networks": { "eth0": { "rx_bytes": 1000, "tx_bytes": 500 }, "eth1": { "rx_bytes": 24, "tx_bytes": 12 } }
        }))
        .unwrap();
        let s = stats_from(&r).unwrap();
        // 1s of CPU over 4s of system time on 4 CPUs: one full core.
        assert!((s.cpu_percent - 100.0).abs() < 1e-9, "{}", s.cpu_percent);
        assert_eq!(s.memory_bytes, 200_000_000);
        assert_eq!((s.net_rx_bytes, s.net_tx_bytes), (1024, 512));
    }
}
