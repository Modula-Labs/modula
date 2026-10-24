//! Builds with BuildKit running rootless in containers of its own, one per
//! workspace (`modula-buildkit-<workspace>`), driven by the `buildctl` and
//! `railpack` binaries.
//!
//! - Railpack: `railpack prepare` writes a build plan, which BuildKit builds
//!   through Railpack's frontend image.
//! - Dockerfile: BuildKit's own Dockerfile frontend.
//!
//! Every child process gets argv only and a cleared environment; variable
//! values are passed as BuildKit secrets through buildctl's environment, never
//! as build args, so they are not baked into image layers. The result is
//! written as a Docker image archive and loaded into the engine.
//!
//! Isolation: a workspace's builds share one daemon and its cache (volume
//! `modula-buildkit-state-<workspace>`), never another workspace's. Daemons
//! start on demand, run with memory/CPU/pids limits, a seccomp profile and
//! BuildKit's process sandbox, on the `modula-build` network (no environment
//! network), and are removed after [`IDLE_FOR`] without builds (the cache
//! volume stays). At most [`Limits::concurrency`] builds run at once.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use async_trait::async_trait;
use bollard::Docker;
use bollard::models::{
    ContainerCreateBody, HostConfig, NetworkCreateRequest, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, DownloadFromContainerOptionsBuilder,
    ImportImageOptionsBuilder, ListContainersOptionsBuilder, RemoveContainerOptionsBuilder,
    RemoveImageOptions,
};
use futures::{StreamExt, TryStreamExt};
use modula_proto::node::v1 as pb;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;

use super::repo_config::{self, RuntimeOverrides};
use super::source::{self, Fetcher};
use super::{BuildLog, Builder, Built};

/// Pinned by digest; bump together with the buildctl in the release.
pub const BUILDKIT_IMAGE: &str = "moby/buildkit:v0.34.1-rootless@sha256:f68752a5a94acfcdad2d07867b7075601a421638e17fc5f28efc1c814d0c4f89";
/// What `railpack prepare` is told a variable's value is: not empty
/// (Railpack drops those), never the real value (argv is visible to `ps`).
const SECRET_PLACEHOLDER: &str = "1";

/// The AppArmor profile install.sh loads where unprivileged user namespaces
/// are restricted (Ubuntu 23.10+): unconfined, plus `userns`.
const APPARMOR_PROFILE: &str = "modula-buildkit";

/// The daemon's seccomp filter. Not Docker's default allowlist (that one
/// refuses the user and mount namespaces rootless BuildKit creates for its
/// process sandbox): everything is allowed except what Docker's default
/// also blocks and BuildKit never needs: kernel modules, kexec, reboot,
/// clocks, swap, accounting, bpf, perf, userfaultfd, handle-based opens,
/// raw I/O ports, other processes' memory. The keyring calls answer ENOSYS,
/// which runc treats as "no keyrings" (EPERM would fail every build step).
/// Build steps additionally get BuildKit's own default profile.
const SECCOMP_PROFILE: &str = include_str!("buildkit-seccomp.json");

/// Whether the kernel has `name` loaded (no AppArmor: false).
fn apparmor_profile_loaded(name: &str) -> bool {
    std::fs::read_to_string("/sys/kernel/security/apparmor/profiles").is_ok_and(|all| {
        all.lines()
            .any(|l| l.split_once(' ').map_or(l, |(n, _)| n) == name)
    })
}

/// [`SECCOMP_PROFILE`] on one line, as Docker's API takes it.
fn seccomp_json() -> &'static str {
    static JSON: OnceLock<String> = OnceLock::new();
    JSON.get_or_init(|| {
        let v: serde_json::Value =
            serde_json::from_str(SECCOMP_PROFILE).expect("the bundled seccomp profile is JSON");
        v.to_string()
    })
}

/// The build engine's security options: our seccomp profile, and our
/// AppArmor profile when it is loaded (it allows the user namespace rootless
/// BuildKit creates), else unconfined.
fn security_opts(profile_loaded: bool) -> Vec<String> {
    let apparmor = if profile_loaded {
        format!("apparmor={APPARMOR_PROFILE}")
    } else {
        "apparmor=unconfined".to_owned()
    };
    vec![format!("seccomp={}", seccomp_json()), apparmor]
}

/// Pinned by digest (the multi-arch index) like [`BUILDKIT_IMAGE`].
pub const RAILPACK_FRONTEND: &str = "ghcr.io/railwayapp/railpack-frontend:v0.43.0@sha256:355ef61fae6f2551103cd8b10c710cb07167dd353d561f15c69b8d7ac1c676cc";
const CONTAINER_PREFIX: &str = "modula-buildkit-";
const VOLUME_PREFIX: &str = "modula-buildkit-state-";
/// Build daemons' network: a plain bridge with internet access, no
/// environment network, and no traffic between daemons.
pub const BUILD_NETWORK: &str = "modula-build";
const ROLE_LABEL: &str = "dev.modula.role";
const KEY_LABEL: &str = "dev.modula.build-key";
/// A hash of the container's settings; another value means recreate.
const CONFIG_LABEL: &str = "dev.modula.config";
/// Rootless BuildKit runs as this user inside its container.
const BUILDKIT_UID: u32 = 1000;
const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A daemon without builds for this long is removed (its cache stays).
pub const IDLE_FOR: Duration = Duration::from_secs(30 * 60);
const REAP_EVERY: Duration = Duration::from_secs(5 * 60);
/// The unprivileged user `railpack prepare` runs as, when it exists
/// (install.sh creates it).
const BUILD_USER: &str = "modula-build";
/// Variables that configure Railpack itself are passed to `prepare` with
/// their values; every other variable is passed by name only.
const CONFIG_PREFIXES: &[&str] = &["RAILPACK_", "NIXPACKS_"];

/// Resources of one build daemon, and how many builds run at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub memory_bytes: i64,
    pub nano_cpus: i64,
    pub pids: i64,
    pub concurrency: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 2 << 30,
            nano_cpus: 2_000_000_000,
            pids: 16_384,
            concurrency: 2,
        }
    }
}

/// The daemon key for a build's `cache_key` (the workspace id): safe in a
/// container, volume and directory name. Keys that had to change get a hash
/// of the original, so two workspaces never share a daemon.
pub fn daemon_key(cache_key: &str) -> String {
    let clean: String = cache_key
        .chars()
        .filter_map(|c| {
            let c = c.to_ascii_lowercase();
            (c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-').then_some(c)
        })
        .take(40)
        .collect();
    let clean = clean.trim_matches('-');
    if !clean.is_empty() && clean == cache_key {
        return clean.to_owned();
    }
    let hash = hex::encode(&Sha256::digest(cache_key.as_bytes())[..6]);
    if clean.is_empty() {
        format!("default-{hash}")
    } else {
        format!("{clean}-{hash}")
    }
}

/// A user from /etc/passwd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct User {
    uid: u32,
    gid: u32,
}

fn lookup_user(passwd: &str, name: &str) -> Option<User> {
    passwd.lines().find_map(|l| {
        let mut f = l.split(':');
        if f.next()? != name {
            return None;
        }
        let _password = f.next()?;
        let uid = f.next()?.parse().ok()?;
        let gid = f.next()?.parse().ok()?;
        (uid != 0).then_some(User { uid, gid })
    })
}

/// `path` and everything under it owned by `user`, never following a
/// symlink (source trees hold the repository's own links).
#[cfg(unix)]
fn chown_tree(path: &Path, user: User) -> std::io::Result<()> {
    std::os::unix::fs::lchown(path, Some(user.uid), Some(user.gid))?;
    if std::fs::symlink_metadata(path)?.is_dir() {
        for e in std::fs::read_dir(path)? {
            chown_tree(&e?.path(), user)?;
        }
    }
    Ok(())
}

/// Lets other users traverse (not list) `dir`.
#[cfg(unix)]
fn traversable(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(dir)?.permissions().mode();
    if mode & 0o001 == 0 {
        std::fs::set_permissions(
            dir,
            std::fs::Permissions::from_mode((mode & 0o7777) | 0o011),
        )?;
    }
    Ok(())
}

/// One workspace's daemon, as far as this process knows.
struct Daemon {
    last_used: Instant,
    /// Builds using it right now; never removed while > 0.
    active: usize,
    /// May have a container (false once removed for being idle).
    up: bool,
    /// Held while the daemon is started or removed.
    lock: Arc<tokio::sync::Mutex<()>>,
}

type Daemons = Arc<Mutex<HashMap<String, Daemon>>>;

/// A build's hold on its daemon; marks it used when dropped.
struct Lease {
    daemons: Daemons,
    key: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(d) = self
            .daemons
            .lock()
            .expect("daemons lock")
            .get_mut(&self.key)
        {
            d.active = d.active.saturating_sub(1);
            d.last_used = Instant::now();
        }
    }
}

pub struct BuildkitBuilder {
    docker: Docker,
    fetcher: Fetcher,
    /// Scratch space for sources and image archives (one dir per build).
    work: PathBuf,
    /// Holds one socket directory per daemon (`<key>/`), each mounted into
    /// its container.
    socket_dir: PathBuf,
    /// Railpack's download cache (mise, language runtimes for detection).
    cache: PathBuf,
    /// HOME (and Railpack's cache) of [`BUILD_USER`]: its own, mode 0700.
    build_home: PathBuf,
    buildctl: PathBuf,
    railpack: PathBuf,
    limits: Limits,
    slots: Arc<Semaphore>,
    daemons: Daemons,
    /// Who runs `railpack prepare`: [`BUILD_USER`], or root without it.
    build_user: Option<User>,
}

impl BuildkitBuilder {
    pub fn new(
        data_dir: &Path,
        socket_dir: PathBuf,
        buildctl: PathBuf,
        railpack: PathBuf,
        limits: Limits,
    ) -> anyhow::Result<Self> {
        let build_user = std::fs::read_to_string("/etc/passwd")
            .ok()
            .and_then(|p| lookup_user(&p, BUILD_USER));
        if build_user.is_none() {
            tracing::warn!(
                "no {BUILD_USER} user on this server: Railpack plans builds as root (re-run install.sh to add it)"
            );
        }
        let this = Self {
            docker: Docker::connect_with_local_defaults().context("cannot connect to Docker")?,
            fetcher: Fetcher::new()?,
            work: data_dir.join("builds"),
            socket_dir,
            cache: data_dir.join("cache/railpack"),
            build_home: data_dir.join("build-home"),
            buildctl,
            railpack,
            limits,
            slots: Arc::new(Semaphore::new(limits.concurrency.max(1))),
            daemons: Daemons::default(),
            build_user,
        };
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(reap(this.docker.clone(), this.daemons.clone()));
        }
        Ok(this)
    }

    fn socket(&self, key: &str) -> String {
        format!(
            "unix://{}",
            self.socket_dir.join(key).join("buildkitd.sock").display()
        )
    }

    /// Count a build against `key`'s daemon until the lease is dropped.
    fn lease(&self, key: &str) -> (Lease, Arc<tokio::sync::Mutex<()>>) {
        let mut daemons = self.daemons.lock().expect("daemons lock");
        let d = daemons.entry(key.to_owned()).or_insert_with(|| Daemon {
            last_used: Instant::now(),
            active: 0,
            up: true,
            lock: Arc::default(),
        });
        d.active += 1;
        d.up = true;
        d.last_used = Instant::now();
        let lock = d.lock.clone();
        (
            Lease {
                daemons: self.daemons.clone(),
                key: key.to_owned(),
            },
            lock,
        )
    }

    /// The `modula-build` network, created if missing.
    async fn ensure_network(&self) -> anyhow::Result<()> {
        if self
            .docker
            .inspect_network(BUILD_NETWORK, None)
            .await
            .is_ok()
        {
            return Ok(());
        }
        let mut options = HashMap::from([(
            "com.docker.network.bridge.enable_icc".to_owned(),
            "false".to_owned(),
        )]);
        if let Some(mtu) = crate::runtime::docker::host_mtu().filter(|m| *m < 1500) {
            options.insert("com.docker.network.driver.mtu".into(), mtu.to_string());
        }
        let created = self
            .docker
            .create_network(NetworkCreateRequest {
                name: BUILD_NETWORK.into(),
                driver: Some("bridge".into()),
                options: Some(options),
                // Not `labels::MANAGED`: that marks environment networks.
                labels: Some(HashMap::from([(ROLE_LABEL.to_owned(), "build".to_owned())])),
                ..Default::default()
            })
            .await;
        // Lost a race with another build: fine if it exists now.
        if let Err(e) = created
            && self
                .docker
                .inspect_network(BUILD_NETWORK, None)
                .await
                .is_err()
        {
            return Err(e).context("cannot create the build network");
        }
        Ok(())
    }

    /// The daemon container for `key`, as it should be.
    fn daemon_body(&self, key: &str, socket_dir: &Path) -> ContainerCreateBody {
        let host = HostConfig {
            binds: Some(vec![
                format!("{}:/run/buildkit", socket_dir.display()),
                format!("{VOLUME_PREFIX}{key}:/home/user/.local/share/buildkit"),
            ]),
            // Rootless BuildKit needs these to create its user namespaces;
            // the daemon itself runs unprivileged (uid 1000), with no
            // Docker socket and no host mounts beyond its own socket.
            security_opt: Some(security_opts(apparmor_profile_loaded(APPARMOR_PROFILE))),
            // `--security-opt systempaths=unconfined` in API terms: the
            // process sandbox mounts its own /proc for build steps.
            masked_paths: Some(vec![]),
            readonly_paths: Some(vec![]),
            pids_limit: Some(self.limits.pids),
            memory: Some(self.limits.memory_bytes),
            // No swap on top of the memory limit.
            memory_swap: Some(self.limits.memory_bytes),
            nano_cpus: Some(self.limits.nano_cpus),
            network_mode: Some(BUILD_NETWORK.into()),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            ..Default::default()
        };
        ContainerCreateBody {
            image: Some(BUILDKIT_IMAGE.into()),
            cmd: Some(vec![
                "--addr".into(),
                "unix:///run/buildkit/buildkitd.sock".into(),
            ]),
            labels: Some(HashMap::from([
                (ROLE_LABEL.into(), "buildkit".into()),
                (KEY_LABEL.into(), key.into()),
            ])),
            host_config: Some(host),
            ..Default::default()
        }
    }

    /// Start `key`'s daemon if it isn't running (the caller holds its lock).
    async fn ensure_daemon(&self, key: &str, log: &BuildLog) -> anyhow::Result<()> {
        let name = format!("{CONTAINER_PREFIX}{key}");
        let socket_dir = self.socket_dir.join(key);
        let mut body = self.daemon_body(key, &socket_dir);
        let config = hex::encode(&Sha256::digest(serde_json::to_vec(&body)?)[..16]);
        if let Some(labels) = body.labels.as_mut() {
            labels.insert(CONFIG_LABEL.into(), config.clone());
        }
        let existing = self.docker.inspect_container(&name, None).await.ok();
        // A container made with other settings (image, limits, security
        // profile) is recreated.
        let current = existing.as_ref().is_some_and(|c| {
            c.config
                .as_ref()
                .and_then(|c| c.labels.as_ref())
                .and_then(|l| l.get(CONFIG_LABEL))
                == Some(&config)
        });
        let running = existing
            .as_ref()
            .and_then(|c| c.state.as_ref())
            .and_then(|s| s.running)
            .unwrap_or(false);
        if current && running {
            return Ok(());
        }
        log.line(false, "Starting the build engine");
        // Root's directory, traversable; the daemon's own is private to it
        // (rootless buildkitd creates its socket there).
        std::fs::create_dir_all(&socket_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.socket_dir, std::fs::Permissions::from_mode(0o711))?;
            std::os::unix::fs::chown(&socket_dir, Some(BUILDKIT_UID), Some(BUILDKIT_UID))?;
            std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        if existing.is_some() && !current {
            let _ = self
                .docker
                .remove_container(
                    &name,
                    Some(RemoveContainerOptionsBuilder::new().force(true).build()),
                )
                .await;
        }
        if existing.is_none() || !current {
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
            self.ensure_network().await?;
            let opts = CreateContainerOptionsBuilder::new().name(&name).build();
            self.docker.create_container(Some(opts), body).await?;
        }
        self.docker.start_container(&name, None).await?;
        // Wait for the socket to accept connections.
        for _ in 0..60 {
            let ok = Command::new(&self.buildctl)
                .args(["--addr", &self.socket(key), "debug", "workers"])
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
        bail!(
            "the build engine did not start (on the server: `docker logs {name}`; on Ubuntu 23.10+ install.sh adds the {APPARMOR_PROFILE} AppArmor profile it needs)"
        )
    }

    /// Hand the build's files and Railpack's cache to [`BUILD_USER`].
    #[cfg(unix)]
    fn prepare_for_build_user(&self, user: User, dir: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // The build directory's ancestors (the data directory included)
        // only gain the traverse bit; their contents keep their own modes.
        let mut cur = Some(self.work.as_path());
        while let Some(p) = cur {
            traversable(p).with_context(|| format!("cannot open up {}", p.display()))?;
            cur = p
                .parent()
                .filter(|p| !p.as_os_str().is_empty() && *p != Path::new("/"));
        }
        chown_tree(dir, user).context("cannot hand the build to the build user")?;
        // Its HOME: private to it. Never /tmp (the node has a private one).
        std::fs::create_dir_all(&self.build_home)?;
        if std::fs::symlink_metadata(&self.build_home)?.uid() != user.uid {
            chown_tree(&self.build_home, user)
                .context("cannot hand the build home to the build user")?;
        }
        std::fs::set_permissions(&self.build_home, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    async fn run_build(
        &self,
        w: &pb::Workload,
        spec: &pb::Build,
        log: &BuildLog,
        dir: &Path,
        key: &str,
        daemon_lock: &tokio::sync::Mutex<()>,
    ) -> anyhow::Result<Built> {
        {
            let _starting = daemon_lock.lock().await;
            self.ensure_daemon(key, log).await?;
        }

        log.line(false, "Downloading source");
        let fetched = source::fetch(&self.fetcher, spec, dir).await?;
        log.line(
            false,
            &format!(
                "Unpacked {} files ({} KB)",
                fetched.files,
                fetched.size / 1024
            ),
        );

        let context = resolve_dir(&fetched.dir, &spec.root_dir)?;
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
            self.socket(key),
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
                self.railpack_prepare(spec, dir, &context, &plan, &secrets, log)
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
        dir: &Path,
        context: &Path,
        plan: &Path,
        secrets: &[(&str, &str)],
        log: &BuildLog,
    ) -> anyhow::Result<()> {
        #[cfg(unix)]
        if let Some(user) = self.build_user {
            self.prepare_for_build_user(user, dir)?;
        }
        #[cfg(not(unix))]
        let _ = dir;
        let home = if self.build_user.is_some() {
            &self.build_home
        } else {
            std::fs::create_dir_all(&self.cache)?;
            &self.cache
        };
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
            // Names declare the secrets the build steps mount (their values
            // come through BuildKit, never argv); only Railpack's own
            // settings carry their values here. Railpack ignores a variable
            // with an empty value, so the others get a placeholder.
            let value = if CONFIG_PREFIXES.iter().any(|p| name.starts_with(p)) {
                value
            } else {
                SECRET_PLACEHOLDER
            };
            args.extend(["--env".into(), format!("{name}={value}")]);
        }
        let mut cmd = Command::new(&self.railpack);
        cmd.args(&args)
            .env_clear()
            .env("HOME", home)
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("NO_COLOR", "1");
        // Railpack runs the repository's tooling detection: never as root
        // when the server has an unprivileged build user.
        #[cfg(unix)]
        if let Some(user) = self.build_user {
            cmd.uid(user.uid).gid(user.gid);
        }
        run_logged(cmd, log)
            .await
            .context("Railpack could not plan the build")
    }
}

impl BuildkitBuilder {
    async fn run_export(
        &self,
        container: &str,
        image: &str,
        dir: &str,
        dest: &Path,
    ) -> anyhow::Result<()> {
        use std::io::Write as _;
        let workdir = self
            .docker
            .inspect_image(image)
            .await?
            .config
            .and_then(|c| c.working_dir)
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "/".into());
        let path = if dir.starts_with('/') {
            dir.to_owned()
        } else {
            format!(
                "{}/{}",
                workdir.trim_end_matches('/'),
                dir.trim_matches('/')
            )
        };
        let body = ContainerCreateBody {
            image: Some(image.to_owned()),
            labels: Some(HashMap::from([("dev.modula.role".into(), "export".into())])),
            ..Default::default()
        };
        let opts = CreateContainerOptionsBuilder::new().name(container).build();
        self.docker.create_container(Some(opts), body).await?;
        let mut stream = self.docker.download_from_container(
            container,
            Some(
                DownloadFromContainerOptionsBuilder::new()
                    .path(&path)
                    .build(),
            ),
        );
        let file = std::fs::File::create(dest)?;
        let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| match e {
                bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                } => anyhow::anyhow!(
                    "the build has no {path}: check the output directory (relative to the root directory)"
                ),
                e => anyhow::Error::from(e).context("cannot copy the output out of the build"),
            })?;
            gz.write_all(&chunk)?;
        }
        gz.finish()?.sync_all()?;
        Ok(())
    }
}

#[async_trait]
impl Builder for BuildkitBuilder {
    async fn export(
        &self,
        w: &pb::Workload,
        dir: &str,
        dest: &Path,
        log: &BuildLog,
    ) -> anyhow::Result<()> {
        log.line(false, &format!("Packing the output directory {dir}"));
        let container = format!("modula-export-{}", w.deployment_id);
        let result = self.run_export(&container, &w.image, dir, dest).await;
        let _ = self
            .docker
            .remove_container(
                &container,
                Some(RemoveContainerOptionsBuilder::new().force(true).build()),
            )
            .await;
        let _ = self
            .docker
            .remove_image(&w.image, None::<RemoveImageOptions>, None)
            .await;
        result
    }

    async fn build(
        &self,
        w: &pb::Workload,
        spec: &pb::Build,
        log: &BuildLog,
    ) -> anyhow::Result<Built> {
        // Wait for a slot before the clock starts.
        let _slot = match self.slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                log.line(false, "Waiting for a free build slot on this server");
                self.slots.clone().acquire_owned().await?
            }
        };
        let key = daemon_key(&spec.cache_key);
        let (_lease, daemon_lock) = self.lease(&key);
        let dir = self.work.join(&w.deployment_id);
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await?;
        let result = tokio::time::timeout(
            BUILD_TIMEOUT,
            self.run_build(w, spec, log, &dir, &key, &daemon_lock),
        )
        .await;
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

/// Every few minutes, removes the daemons idle for [`IDLE_FOR`]. Daemons
/// found at startup (Docker restarts them) count as just used.
async fn reap(docker: Docker, daemons: Daemons) {
    let filters = HashMap::from([("label".to_owned(), vec![format!("{ROLE_LABEL}=buildkit")])]);
    let opts = ListContainersOptionsBuilder::new()
        .all(true)
        .filters(&filters)
        .build();
    if let Ok(found) = docker.list_containers(Some(opts)).await {
        let mut daemons = daemons.lock().expect("daemons lock");
        for key in found
            .into_iter()
            .filter_map(|c| c.labels?.get(KEY_LABEL).cloned())
        {
            daemons.entry(key).or_insert_with(|| Daemon {
                last_used: Instant::now(),
                active: 0,
                up: true,
                lock: Arc::default(),
            });
        }
    }
    let mut tick = tokio::time::interval(REAP_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        // Taking a daemon's lock while it has no builds keeps a new build
        // from using it until it is gone (that build then starts a new one).
        let idle: Vec<(String, tokio::sync::OwnedMutexGuard<()>)> = {
            let mut daemons = daemons.lock().expect("daemons lock");
            daemons
                .iter_mut()
                .filter(|(_, d)| d.up && d.active == 0 && d.last_used.elapsed() >= IDLE_FOR)
                .filter_map(|(k, d)| {
                    let guard = d.lock.clone().try_lock_owned().ok()?;
                    d.up = false;
                    Some((k.clone(), guard))
                })
                .collect()
        };
        for (key, _guard) in idle {
            let name = format!("{CONTAINER_PREFIX}{key}");
            match docker
                .remove_container(
                    &name,
                    Some(RemoveContainerOptionsBuilder::new().force(true).build()),
                )
                .await
            {
                Ok(()) => tracing::info!(daemon = %name, "removed an idle build engine"),
                Err(e) => {
                    tracing::debug!(daemon = %name, error = %e, "cannot remove a build engine")
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_options() {
        let opts = security_opts(true);
        assert_eq!(opts[1], "apparmor=modula-buildkit");
        assert_eq!(security_opts(false)[1], "apparmor=unconfined");
        assert!(!apparmor_profile_loaded("surely-not-a-profile"));
        // Inline, one line, and never unconfined.
        let seccomp = opts[0].strip_prefix("seccomp=").unwrap();
        assert!(!seccomp.contains('\n') && seccomp != "unconfined");
        let v: serde_json::Value = serde_json::from_str(seccomp).unwrap();
        assert_eq!(v["defaultAction"], "SCMP_ACT_ALLOW");
        let denied: Vec<&str> = v["syscalls"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|r| r["names"].as_array().unwrap())
            .map(|n| n.as_str().unwrap())
            .collect();
        for s in [
            "init_module",
            "kexec_load",
            "bpf",
            "keyctl",
            "open_by_handle_at",
        ] {
            assert!(denied.contains(&s), "{s} is blocked");
        }
        // What the rootless process sandbox needs stays allowed.
        for s in [
            "unshare",
            "mount",
            "umount2",
            "pivot_root",
            "setns",
            "clone",
            "clone3",
        ] {
            assert!(!denied.contains(&s), "{s} is allowed");
        }
    }

    #[test]
    fn daemon_keys_are_safe_and_distinct() {
        assert_eq!(daemon_key("k3x9q2"), "k3x9q2");
        assert_eq!(daemon_key("ws-1"), "ws-1");
        for hostile in ["../../etc", "A", "a b", "", "x/y", "-"] {
            let k = daemon_key(hostile);
            assert!(
                !k.is_empty()
                    && k.len() <= 53
                    && k.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{hostile:?} → {k:?}"
            );
        }
        assert_ne!(daemon_key("A"), daemon_key("a"));
        assert_ne!(daemon_key("x/y"), daemon_key("xy"));
        assert_eq!(daemon_key(""), daemon_key(""));
    }

    #[test]
    fn build_user_from_passwd() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      modula-build:x:998:997::/nonexistent:/usr/sbin/nologin\n";
        assert_eq!(
            lookup_user(passwd, "modula-build"),
            Some(User { uid: 998, gid: 997 })
        );
        assert_eq!(lookup_user(passwd, "root"), None, "never root");
        assert_eq!(lookup_user(passwd, "nobody"), None);
    }

    #[test]
    fn idle_daemons_are_kept_while_building() {
        let daemons = Daemons::default();
        let lease = Lease {
            daemons: daemons.clone(),
            key: "k".into(),
        };
        daemons.lock().unwrap().insert(
            "k".into(),
            Daemon {
                last_used: Instant::now() - IDLE_FOR * 2,
                active: 1,
                up: true,
                lock: Arc::default(),
            },
        );
        drop(lease);
        let d = &daemons.lock().unwrap()["k"];
        assert_eq!(d.active, 0);
        assert!(
            d.last_used.elapsed() < IDLE_FOR,
            "dropping a lease marks use"
        );
    }

    /// A real build in a daemon set up like ours (process sandbox, our
    /// seccomp profile, limits, the build network). Needs Docker; run with
    /// `cargo test -p modula-node -- --ignored buildkit_daemon`.
    #[tokio::test]
    #[ignore]
    async fn buildkit_daemon_builds_with_the_process_sandbox() {
        use bollard::exec::{CreateExecOptions, StartExecResults};
        use futures::StreamExt;

        let data = std::env::temp_dir().join(format!("modula-bk-{}", modula_id::Id::new()));
        let b = BuildkitBuilder::new(
            &data,
            data.join("sock"),
            "buildctl".into(),
            "railpack".into(),
            Limits::default(),
        )
        .unwrap();
        let key = format!("test-{}", modula_id::Id::new()).to_lowercase();
        let key = daemon_key(&key);
        let name = format!("{CONTAINER_PREFIX}{key}");
        // Docker Desktop can't bind-mount arbitrary host paths for uid 1000;
        // start the container as ensure_daemon would, without the wait.
        let mut body = b.daemon_body(&key, Path::new("/tmp"));
        if let Some(h) = body.host_config.as_mut() {
            h.binds = Some(vec![format!(
                "{VOLUME_PREFIX}{key}:/home/user/.local/share/buildkit"
            )]);
            h.restart_policy = None;
        }
        body.cmd = None;
        let pull = CreateImageOptionsBuilder::new()
            .from_image(BUILDKIT_IMAGE)
            .build();
        b.docker
            .create_image(Some(pull), None, None)
            .try_for_each(|_| async { Ok(()) })
            .await
            .unwrap();
        b.ensure_network().await.unwrap();
        let opts = CreateContainerOptionsBuilder::new().name(&name).build();
        b.docker.create_container(Some(opts), body).await.unwrap();
        b.docker.start_container(&name, None).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        let script = "mkdir -p /tmp/ctx && printf 'FROM busybox\\nRUN hostname && id\\n' > /tmp/ctx/Dockerfile && \
                      buildctl --addr unix:///run/user/1000/buildkit/buildkitd.sock build --progress plain \
                      --frontend dockerfile.v0 --local context=/tmp/ctx --local dockerfile=/tmp/ctx --no-cache && echo BUILD-OK";
        let exec = b
            .docker
            .create_exec(
                &name,
                CreateExecOptions {
                    cmd: Some(vec!["sh", "-ec", script]), // no-shell: allow (fixed test script, no input)
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut out = String::new();
        if let StartExecResults::Attached { mut output, .. } =
            b.docker.start_exec(&exec.id, None).await.unwrap()
        {
            while let Some(Ok(chunk)) = output.next().await {
                out.push_str(&chunk.to_string());
            }
        }
        let _ = b
            .docker
            .remove_container(
                &name,
                Some(RemoveContainerOptionsBuilder::new().force(true).build()),
            )
            .await;
        let _ = b
            .docker
            .remove_volume(
                &format!("{VOLUME_PREFIX}{key}"),
                None::<bollard::query_parameters::RemoveVolumeOptions>,
            )
            .await;
        let _ = std::fs::remove_dir_all(&data);
        assert!(out.contains("buildkitsandbox"), "{out}");
        assert!(out.contains("BUILD-OK"), "{out}");
    }
}
