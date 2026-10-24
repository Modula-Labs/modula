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
pub mod output;
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

    /// Packs directory `dir` of the image built for `w` (relative to the
    /// image's working directory, or absolute) into a gzipped tarball at
    /// `dest` with that directory at its top, then deletes the image (it
    /// never runs here).
    async fn export(
        &self,
        w: &pb::Workload,
        dir: &str,
        dest: &std::path::Path,
        log: &BuildLog,
    ) -> anyhow::Result<()>;
}

/// Pretends to build (tests): logs a line and returns a fixed image id, or
/// fails when the source URL contains "fail". `overrides` stand in for a
/// repository config file. With `fetch`, the source really is downloaded,
/// checked and unpacked first, and every unpacked path is logged.
#[derive(Default)]
pub struct FakeBuilder {
    pub overrides: RuntimeOverrides,
    pub fetch: bool,
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
        if self.fetch {
            let dir = std::env::temp_dir().join(format!("modula-fake-build-{}", w.deployment_id));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)?;
            let fetcher = source::Fetcher::new()?;
            let fetched = source::fetch(&fetcher, spec, &dir).await;
            if let Ok(f) = &fetched {
                log.line(false, &format!("Unpacked {} files", f.files));
                for path in list_files(&f.dir, &f.dir) {
                    log.line(false, &format!("file {path}"));
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
            fetched?;
        }
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

    /// A tiny site: `index.html` and `assets/app.js`.
    async fn export(
        &self,
        _w: &pb::Workload,
        dir: &str,
        dest: &std::path::Path,
        log: &BuildLog,
    ) -> anyhow::Result<()> {
        let top = dir.trim_matches('/').rsplit('/').next().unwrap_or("out");
        log.line(false, &format!("exporting {dir}"));
        output::pack(
            dest,
            top,
            &[
                ("index.html", b"<!doctype html><h1>fake</h1>"),
                ("assets/app.js", b"console.log('fake')"),
            ],
        )
    }
}

/// Relative paths of the regular files under `dir`, sorted (FakeBuilder logs).
fn list_files(root: &std::path::Path, dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => out.extend(list_files(root, &p)),
            Ok(t) if t.is_file() => {
                if let Ok(rel) = p.strip_prefix(root) {
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
            _ => {}
        }
    }
    out.sort();
    out
}
