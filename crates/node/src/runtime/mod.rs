//! Container runtimes. The reconciler only talks to this trait, so Docker can
//! be swapped for a native OCI runtime later without touching it.

pub mod docker;
mod docker_exec;
pub mod fake;
pub mod firewall;

use async_trait::async_trait;
use bytes::Bytes;
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
    pub service_id: String,
    pub environment_id: String,
    pub running: bool,
    pub exit_code: i64,
    pub restart_count: u32,
}

/// One resource sample of a running container.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    /// Percent of one CPU (100 = one full core).
    pub cpu_percent: f64,
    /// In use, page cache excluded.
    pub memory_bytes: u64,
    /// Totals since the container started.
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
}

/// Receives a one-off container's output, line by line.
pub type OutputSink = Box<dyn Fn(LogLine) + Send + Sync>;

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
    /// Run `command` once in a new container from the workload's image, with
    /// its environment on its environment network (no aliases, volume or
    /// restart policy), sending each output line to `output`. Returns the
    /// exit code. The container is always removed: on exit, on `timeout`
    /// (an error) and when the caller is cancelled. It is never listed by
    /// [`Runtime::containers`].
    async fn run_once(
        &self,
        workload: &Workload,
        command: &[String],
        timeout: std::time::Duration,
        output: OutputSink,
    ) -> anyhow::Result<i64>;
    /// Stop gracefully and start again (same container, same config).
    async fn restart(&self, container_id: &str) -> anyhow::Result<()>;
    /// Stop gracefully (SIGTERM, then SIGKILL after a grace period) and delete.
    async fn remove(&self, container_id: &str) -> anyhow::Result<()>;
    /// Delete a managed volume and its data. Missing is fine; one still in use
    /// fails (retried on the next reconcile).
    async fn remove_volume(&self, name: &str) -> anyhow::Result<()>;

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

    /// Follows a container's output from `since_unix_secs` on (whole
    /// seconds, so the first lines may repeat ones seen before). Runtimes
    /// that can't follow return an error and nothing is shipped.
    async fn follow_since(
        &self,
        _container_id: &str,
        _since_unix_secs: i64,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<LogLine>>> {
        anyhow::bail!("this runtime can't follow logs")
    }

    /// A resource sample of a running container (None: not running, or the
    /// runtime can't measure it).
    async fn stats(&self, _container_id: &str) -> anyhow::Result<Option<Stats>> {
        Ok(None)
    }

    /// The environment a container was created with, as `(name, value)`.
    async fn container_env(&self, container_id: &str) -> anyhow::Result<Vec<(String, String)>>;

    /// Run a command inside a running container and stream its stdout. The
    /// stream ends with an error (carrying the tail of stderr) if it fails.
    async fn exec_output(
        &self,
        container_id: &str,
        exec: Exec,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<Bytes>>>;

    /// Run a command inside a running container with `stdin` as its input;
    /// returns once it exited successfully.
    async fn exec_input(
        &self,
        container_id: &str,
        exec: Exec,
        stdin: BoxStream<'static, anyhow::Result<Bytes>>,
    ) -> anyhow::Result<()>;
}

/// A command run inside a container: argv (never a shell string) and extra
/// `NAME=value` environment, which may hold credentials (so no `Debug`).
#[derive(Clone, PartialEq, Eq)]
pub struct Exec {
    pub cmd: Vec<String>,
    pub env: Vec<String>,
}
