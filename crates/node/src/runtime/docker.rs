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
    ContainerCreateBody, ContainerSummaryStateEnum, EndpointSettings, HostConfig,
    HostConfigLogConfig, NetworkCreateRequest, NetworkingConfig, RestartPolicy as DockerRestart,
    RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
    ListNetworksOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    StopContainerOptionsBuilder,
};
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use modula_proto::node::v1::{RestartPolicy, Workload};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Container, LogLine, Runtime, labels};

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
const STOP_GRACE_SECS: i32 = 10;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct DockerRuntime {
    docker: Docker,
}

impl DockerRuntime {
    /// Connects to the local engine (`DOCKER_HOST` or the default socket).
    pub fn connect() -> anyhow::Result<Self> {
        Ok(Self {
            docker: Docker::connect_with_local_defaults().context("cannot connect to Docker")?,
        })
    }
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
                labels: Some(HashMap::from([
                    (labels::MANAGED.to_owned(), "true".to_owned()),
                    (labels::ENVIRONMENT.to_owned(), environment_id.to_owned()),
                ])),
                ..Default::default()
            })
            .await?;
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
            out.push(Container {
                deployment_id: labels.get(labels::DEPLOYMENT).cloned().unwrap_or_default(),
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
        let limits = w.limits.unwrap_or_default();
        let restart = match RestartPolicy::try_from(w.restart).unwrap_or(RestartPolicy::OnFailure) {
            RestartPolicy::Always => RestartPolicyNameEnum::ALWAYS,
            RestartPolicy::Never => RestartPolicyNameEnum::NO,
            RestartPolicy::OnFailure | RestartPolicy::RestartUnspecified => {
                RestartPolicyNameEnum::ON_FAILURE
            }
        };
        let host = HostConfig {
            memory: (limits.memory_bytes > 0).then_some(limits.memory_bytes as i64),
            nano_cpus: (limits.nano_cpus > 0).then_some(limits.nano_cpus as i64),
            pids_limit: (limits.pids > 0).then_some(limits.pids),
            restart_policy: Some(DockerRestart {
                name: Some(restart),
                maximum_retry_count: None,
            }),
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
            network_mode: Some(net.clone()),
            ..Default::default()
        };
        let body = ContainerCreateBody {
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
        let short = &w.deployment_id[..w.deployment_id.len().min(8)];
        let name = format!("modula-{}-{short}", w.service_name);
        let opts = CreateContainerOptionsBuilder::new().name(&name).build();
        Ok(self.docker.create_container(Some(opts), body).await?.id)
    }

    async fn start(&self, container_id: &str) -> anyhow::Result<()> {
        self.docker.start_container(container_id, None).await?;
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> anyhow::Result<()> {
        let stop = StopContainerOptionsBuilder::new()
            .t(STOP_GRACE_SECS)
            .build();
        // Already-stopped containers make stop fail; removal is what matters.
        let _ = self.docker.stop_container(container_id, Some(stop)).await;
        let rm = RemoveContainerOptionsBuilder::new()
            .force(true)
            .v(true)
            .build();
        self.docker.remove_container(container_id, Some(rm)).await?;
        Ok(())
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
        let info = self.docker.inspect_container(container_id, None).await?;
        let ip = info
            .network_settings
            .and_then(|n| n.networks)
            .and_then(|mut n| n.remove(&network_name(environment_id)))
            .and_then(|e| e.ip_address)
            .filter(|ip| !ip.is_empty())
            .context("container has no address yet")?;
        let addr = SocketAddr::new(ip.parse()?, port);
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
