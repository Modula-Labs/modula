//! Container runtimes. The reconciler only talks to this trait, so Docker can
//! be swapped for a native OCI runtime later without touching it.

pub mod docker;
pub mod fake;

use async_trait::async_trait;
use futures::stream::BoxStream;
use modula_proto::node::v1::Workload;

/// Labels that mark containers and networks as ours. Anything without
/// `modula.managed=true` is never touched.
pub mod labels {
    pub const MANAGED: &str = "modula.managed";
    pub const DEPLOYMENT: &str = "modula.deployment";
    pub const SERVICE: &str = "modula.service";
    pub const ENVIRONMENT: &str = "modula.environment";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub id: String,
    pub deployment_id: String,
    pub environment_id: String,
    pub running: bool,
    pub exit_code: i64,
    pub restart_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub ts_unix_ms: i64,
    pub stderr: bool,
    pub text: String,
}

#[async_trait]
pub trait Runtime: Send + Sync + 'static {
    /// Short name and version, e.g. `docker 28.1.1`.
    async fn describe(&self) -> anyhow::Result<String>;

    /// Ensure the isolated network for an environment exists.
    async fn ensure_network(&self, environment_id: &str) -> anyhow::Result<()>;
    async fn remove_network(&self, environment_id: &str) -> anyhow::Result<()>;
    /// Environment ids that currently have a managed network.
    async fn networks(&self) -> anyhow::Result<Vec<String>>;

    /// Every managed container, running or not.
    async fn containers(&self) -> anyhow::Result<Vec<Container>>;

    async fn pull(&self, image: &str) -> anyhow::Result<()>;
    /// Create (not start) a container for a workload; returns its id.
    async fn create(&self, workload: &Workload) -> anyhow::Result<String>;
    async fn start(&self, container_id: &str) -> anyhow::Result<()>;
    /// Stop gracefully (SIGTERM, then SIGKILL after a grace period) and delete.
    async fn remove(&self, container_id: &str) -> anyhow::Result<()>;

    /// The container's address on its environment network, once it has one.
    async fn address(
        &self,
        container_id: &str,
        environment_id: &str,
    ) -> anyhow::Result<Option<std::net::IpAddr>>;

    /// One HTTP health probe: `GET path` on the container's port inside its
    /// environment network. `Ok(true)` only for a 2xx response.
    async fn probe(
        &self,
        container_id: &str,
        environment_id: &str,
        port: u16,
        path: &str,
    ) -> anyhow::Result<bool>;

    async fn logs(
        &self,
        container_id: &str,
        tail: u32,
        follow: bool,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<LogLine>>>;
}
