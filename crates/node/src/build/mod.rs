//! Building images from source on this server.
//!
//! A build is the first phase of a repository deployment's rollout: fetch the
//! source archive at one commit, build it in BuildKit (Railpack or a
//! Dockerfile) and load the result into the local engine under the
//! deployment's node-local tag. User variables only ever reach a build as
//! BuildKit secrets. Output goes to a per-deployment log file that the server
//! streams like container logs.

pub mod buildkit;
pub mod logs;
pub mod repo_config;
pub mod source;
pub mod stack;

use async_trait::async_trait;
use modula_proto::node::v1 as pb;

pub use logs::{BuildLog, BuildLogs};
pub use repo_config::RuntimeOverrides;

/// What a node needs to build images: the builder and where logs go.
#[derive(Clone)]
pub struct BuildSupport {
    pub builder: std::sync::Arc<dyn Builder>,
    pub logs: BuildLogs,
}

/// Images built here (and rollbacks to them) are never pulled.
pub const LOCAL_PREFIX: &str = "modula-build/";

/// A finished build.
#[derive(Debug, Clone, PartialEq)]
pub struct Built {
    /// The image id (`sha256:…`).
    pub image: String,
    /// How the container must run, from the repository's config file.
    pub overrides: RuntimeOverrides,
    /// The app's stack, for the dashboard's icon (see [`stack::detect`]).
    pub stack: String,
}

#[async_trait]
pub trait Builder: Send + Sync + 'static {
    /// Build `w.image` from `spec`.
    async fn build(
        &self,
        w: &pb::Workload,
        spec: &pb::Build,
        log: &BuildLog,
    ) -> anyhow::Result<Built>;
}

/// Pretends to build (tests): logs a line and returns a fixed image id, or
/// fails when the source URL contains "fail". `overrides` stand in for a
/// repository config file.
#[derive(Default)]
pub struct FakeBuilder {
    pub overrides: RuntimeOverrides,
}

#[async_trait]
impl Builder for FakeBuilder {
    async fn build(
        &self,
        w: &pb::Workload,
        spec: &pb::Build,
        log: &BuildLog,
    ) -> anyhow::Result<Built> {
        log.line(
            false,
            &format!("building {} from {}", w.image, spec.source_url),
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if spec.source_url.contains("fail") {
            anyhow::bail!("the build failed");
        }
        Ok(Built {
            image: format!("sha256:{}", "ab".repeat(32)),
            overrides: self.overrides.clone(),
            stack: String::new(),
        })
    }
}
