//! Builds with BuildKit running rootless in its own container
//! (`modula-buildkit`), driven by the `buildctl` and `railpack` binaries.
//!
//! - Railpack: `railpack prepare` writes a build plan, which BuildKit builds
//!   through Railpack's frontend image.
//! - Dockerfile: BuildKit's own Dockerfile frontend.
//!
//! Every child process gets argv only and a cleared environment; variable
//! values are passed as BuildKit secrets through buildctl's environment, never
//! as build args, so they are not baked into image layers. The result is
//! written as a Docker image archive and loaded into the engine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use bollard::Docker;
use bollard::models::{ContainerCreateBody, HostConfig, RestartPolicy, RestartPolicyNameEnum};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ImportImageOptionsBuilder,
};
use futures::TryStreamExt;
use modula_proto::node::v1 as pb;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use super::repo_config::{self, RuntimeOverrides};
use super::source::{self, Fetcher};
use super::{BuildLog, Builder, Built};

/// Pinned by digest; bump together with the buildctl in the release.
pub const BUILDKIT_IMAGE: &str = "moby/buildkit:v0.34.1-rootless@sha256:f68752a5a94acfcdad2d07867b7075601a421638e17fc5f28efc1c814d0c4f89";
pub const RAILPACK_FRONTEND: &str = "ghcr.io/railwayapp/railpack-frontend:v0.43.0";
const CONTAINER: &str = "modula-buildkit";
/// Rootless BuildKit runs as this user inside its container.
const BUILDKIT_UID: u32 = 1000;
const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Variables that configure Railpack itself are passed to `prepare` with
/// their values; every other variable is passed by name only.
const CONFIG_PREFIXES: &[&str] = &["RAILPACK_", "NIXPACKS_"];

pub struct BuildkitBuilder {
    docker: Docker,
    fetcher: Fetcher,
    /// Scratch space for sources and image archives (one dir per build).
    work: PathBuf,
    /// Holds BuildKit's socket; mounted into its container.
    socket_dir: PathBuf,
    /// Railpack's download cache (mise, language runtimes for detection).
    cache: PathBuf,
    buildctl: PathBuf,
    railpack: PathBuf,
}

impl BuildkitBuilder {
    pub fn new(
        data_dir: &Path,
        socket_dir: PathBuf,
        buildctl: PathBuf,
        railpack: PathBuf,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            docker: Docker::connect_with_local_defaults().context("cannot connect to Docker")?,
            fetcher: Fetcher::new()?,
            work: data_dir.join("builds"),
            socket_dir,
            cache: data_dir.join("cache/railpack"),
            buildctl,
            railpack,
        })
    }

    fn socket(&self) -> String {
        format!(
            "unix://{}",
            self.socket_dir.join("buildkitd.sock").display()
        )
    }

    /// Start the BuildKit container if it isn't running.
    async fn ensure_daemon(&self, log: &BuildLog) -> anyhow::Result<()> {
        if let Ok(c) = self.docker.inspect_container(CONTAINER, None).await
            && c.state.and_then(|s| s.running).unwrap_or(false)
        {
            return Ok(());
        }
        log.line(false, "Starting the build engine");
        std::fs::create_dir_all(&self.socket_dir)?;
        #[cfg(unix)]
        std::os::unix::fs::chown(&self.socket_dir, Some(BUILDKIT_UID), Some(BUILDKIT_UID))?;
        if self
            .docker
            .inspect_container(CONTAINER, None)
            .await
            .is_err()
        {
            let (from, _) = BUILDKIT_IMAGE
                .split_once('@')
                .unwrap_or((BUILDKIT_IMAGE, ""));
            let pull = CreateImageOptionsBuilder::new()
                .from_image(BUILDKIT_IMAGE)
                .build();
            self.docker
                .create_image(Some(pull), None, None)
                .try_for_each(|_| async { Ok(()) })
                .await
                .with_context(|| format!("cannot pull {from}"))?;
            let host = HostConfig {
                binds: Some(vec![
                    format!("{}:/run/buildkit", self.socket_dir.display()),
                    "modula-buildkit-state:/home/user/.local/share/buildkit".into(),
                ]),
                // Rootless BuildKit needs these to create its user namespace;
                // the daemon itself runs unprivileged (uid 1000), with no
                // Docker socket and no host mounts beyond its own socket.
                security_opt: Some(vec![
                    "seccomp=unconfined".into(),
                    "apparmor=unconfined".into(),
                ]),
                // `--security-opt systempaths=unconfined` in API terms: it
                // mounts its own /proc for the build sandboxes.
                masked_paths: Some(vec![]),
                readonly_paths: Some(vec![]),
                pids_limit: Some(16_384),
                restart_policy: Some(RestartPolicy {
                    name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: None,
                }),
                ..Default::default()
            };
            let body = ContainerCreateBody {
                image: Some(BUILDKIT_IMAGE.into()),
                cmd: Some(vec![
                    "--addr".into(),
                    "unix:///run/buildkit/buildkitd.sock".into(),
                    "--oci-worker-no-process-sandbox".into(),
                ]),
                labels: Some(HashMap::from([(
                    "dev.modula.role".into(),
                    "buildkit".into(),
                )])),
                host_config: Some(host),
                ..Default::default()
            };
            let opts = CreateContainerOptionsBuilder::new().name(CONTAINER).build();
            self.docker.create_container(Some(opts), body).await?;
        }
        self.docker.start_container(CONTAINER, None).await?;
        // Wait for the socket to accept connections.
        for _ in 0..60 {
            let ok = Command::new(&self.buildctl)
                .args(["--addr", &self.socket(), "debug", "workers"])
                .env_clear()
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .is_ok_and(|s| s.success());
            if ok {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        bail!("the build engine did not start")
    }

    async fn run_build(
        &self,
        w: &pb::Workload,
        spec: &pb::Build,
        log: &BuildLog,
        dir: &Path,
    ) -> anyhow::Result<Built> {
        self.ensure_daemon(log).await?;

        log.line(false, "Downloading source");
        let archive = dir.join("source.tar.gz");
        let size = self
            .fetcher
            .download(&spec.source_url, &spec.source_auth, &archive)
            .await?;
        let src = dir.join("src");
        let files = {
            let (a, s) = (archive.clone(), src.clone());
            tokio::task::spawn_blocking(move || source::unpack(&a, &s)).await??
        };
        let _ = tokio::fs::remove_file(&archive).await;
        log.line(
            false,
            &format!("Unpacked {files} files ({} KB)", size / 1024),
        );

        let context = resolve_dir(&src, &spec.root_dir)?;
        // Config as code overrides the dashboard; bad values only warn.
        let (config, warnings) = repo_config::load(&context);
        let stack = super::stack::detect(&context);
        for warning in &warnings {
            log.line(true, &format!("Warning: {warning}"));
        }
        let mut spec = spec.clone();
        if let Some(c) = &config {
            c.apply_build(&mut spec);
            if let Some(summary) = c.summary() {
                log.line(false, &summary);
            }
        }
        let spec = &spec;
        let overrides = RuntimeOverrides {
            command: repo_config::effective_command(spec, config.as_ref()),
            ..config.as_ref().map(|c| c.runtime()).unwrap_or_default()
        };
        let image_tar = dir.join("image.tar");
        let secrets: Vec<(&str, &str)> = w
            .env
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_str()))
            .collect();

        let mut args: Vec<String> = vec![
            "--addr".into(),
            self.socket(),
            "build".into(),
            "--progress".into(),
            "plain".into(),
            "--local".into(),
            format!("context={}", context.display()),
        ];
        match pb::Builder::try_from(spec.builder).unwrap_or(pb::Builder::Railpack) {
            pb::Builder::Railpack => {
                let plan = dir.join("plan");
                std::fs::create_dir_all(&plan)?;
                self.railpack_prepare(spec, &context, &plan, &secrets, log)
                    .await?;
                args.extend([
                    "--local".into(),
                    format!("dockerfile={}", plan.display()),
                    "--frontend".into(),
                    "gateway.v0".into(),
                    "--opt".into(),
                    format!("source={RAILPACK_FRONTEND}"),
                ]);
            }
            pb::Builder::Dockerfile => {
                let rel = if spec.dockerfile_path.is_empty() {
                    "Dockerfile"
                } else {
                    &spec.dockerfile_path
                };
                let file = resolve_file(&context, rel)?;
                log.line(false, &format!("Building with {rel}"));
                args.extend([
                    "--local".into(),
                    format!(
                        "dockerfile={}",
                        file.parent()
                            .context("Dockerfile has no directory")?
                            .display()
                    ),
                    "--frontend".into(),
                    "dockerfile.v0".into(),
                    "--opt".into(),
                    format!(
                        "filename={}",
                        file.file_name()
                            .context("Dockerfile has no name")?
                            .to_string_lossy()
                    ),
                ]);
            }
        }
        if !spec.cache_key.is_empty() {
            args.extend([
                "--opt".into(),
                format!("build-arg:cache-key={}", spec.cache_key),
            ]);
        }
        if !spec.secrets_hash.is_empty() {
            args.extend([
                "--opt".into(),
                format!("build-arg:secrets-hash={}", spec.secrets_hash),
            ]);
        }
        for (name, _) in &secrets {
            args.extend(["--secret".into(), format!("id={name},env={name}")]);
        }
        args.extend([
            "--output".into(),
            format!("type=docker,name={},dest={}", w.image, image_tar.display()),
        ]);

        let mut cmd = Command::new(&self.buildctl);
        cmd.args(&args).env_clear();
        for (name, value) in &secrets {
            cmd.env(name, value);
        }
        run_logged(cmd, log).await.context("the build failed")?;

        log.line(false, "Loading the image");
        let file = tokio::fs::File::open(&image_tar).await?;
        let body = tokio_util::io::ReaderStream::new(file);
        self.docker
            .import_image_stream(
                ImportImageOptionsBuilder::default().quiet(true).build(),
                body,
                None,
            )
            .try_for_each(|_| async { Ok(()) })
            .await
            .context("cannot load the built image")?;
        let id = self
            .docker
            .inspect_image(&w.image)
            .await?
            .id
            .context("the built image has no id")?;
        Ok(Built {
            image: id,
            overrides,
            stack,
        })
    }

    async fn railpack_prepare(
        &self,
        spec: &pb::Build,
        context: &Path,
        plan: &Path,
        secrets: &[(&str, &str)],
        log: &BuildLog,
    ) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.cache)?;
        let mut args: Vec<String> = vec![
            "prepare".into(),
            context.display().to_string(),
            "--plan-out".into(),
            plan.join("railpack-plan.json").display().to_string(),
            "--info-out".into(),
            plan.join("railpack-info.json").display().to_string(),
        ];
        if !spec.build_command.is_empty() {
            args.extend(["--build-cmd".into(), spec.build_command.clone()]);
        }
        if !spec.start_command.is_empty() {
            args.extend(["--start-cmd".into(), spec.start_command.clone()]);
        }
        for (name, value) in secrets {
            // Names declare the secrets the build may mount; only Railpack's
            // own settings carry their values here.
            let value = if CONFIG_PREFIXES.iter().any(|p| name.starts_with(p)) {
                value
            } else {
                ""
            };
            args.extend(["--env".into(), format!("{name}={value}")]);
        }
        let mut cmd = Command::new(&self.railpack);
        cmd.args(&args)
            .env_clear()
            .env("HOME", &self.cache)
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("NO_COLOR", "1");
        run_logged(cmd, log)
            .await
            .context("Railpack could not plan the build")
    }
}

#[async_trait]
impl Builder for BuildkitBuilder {
    async fn build(
        &self,
        w: &pb::Workload,
        spec: &pb::Build,
        log: &BuildLog,
    ) -> anyhow::Result<Built> {
        let dir = self.work.join(&w.deployment_id);
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await?;
        let result = tokio::time::timeout(BUILD_TIMEOUT, self.run_build(w, spec, log, &dir)).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        match result {
            Ok(r) => r,
            Err(_) => bail!(
                "the build took longer than {} minutes",
                BUILD_TIMEOUT.as_secs() / 60
            ),
        }
    }
}

/// Run a child, copying each output line into the build log. The child is
/// killed if the build is cancelled (this future dropped).
async fn run_logged(mut cmd: Command, log: &BuildLog) -> anyhow::Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot run {:?}", cmd.as_std().get_program()))?;
    // buildctl writes its progress to stderr; both streams are plain output.
    let out = copy_lines(child.stdout.take().context("no stdout")?, log.clone());
    let err = copy_lines(child.stderr.take().context("no stderr")?, log.clone());
    let (status, _, _) = tokio::join!(child.wait(), out, err);
    let status = status?;
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(())
}

async fn copy_lines(stream: impl AsyncRead + Unpin, log: BuildLog) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        log.line(false, &line);
    }
}

/// `root/rel` as a directory that really is inside `root`.
fn resolve_dir(root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let root = root.canonicalize()?;
    let dir = root
        .join(rel.trim_matches('/'))
        .canonicalize()
        .with_context(|| format!("the root directory {rel:?} does not exist in the repository"))?;
    if !dir.starts_with(&root) || !dir.is_dir() {
        bail!("the root directory {rel:?} is not a directory inside the repository");
    }
    Ok(dir)
}

fn resolve_file(root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let file = root
        .join(rel.trim_matches('/'))
        .canonicalize()
        .with_context(|| format!("{rel} was not found"))?;
    if !file.starts_with(root) || !file.is_file() {
        bail!("{rel} is not a file inside the root directory");
    }
    Ok(file)
}
