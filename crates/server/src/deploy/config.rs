//! Per-environment service configuration (`service_instances.config`), snapshotted
//! into each deployment. Everything here ends up on a node, so it is validated
//! strictly and never interpreted by a shell.

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "type",
    deny_unknown_fields
)]
pub enum Source {
    /// A prebuilt image, e.g. `ghcr.io/acme/api:1.2` or `nginx@sha256:…`.
    Image { image: String },
    /// A GitHub repository, built on the node for every deployment.
    Repo {
        /// `owner/name`.
        repo: String,
        branch: String,
        /// Build from this directory instead of the repository root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_dir: Option<String>,
    },
    /// Deployed from local files with `modula up` (like `railway up`): a
    /// service with no repository or image of its own. Deploying it without
    /// an upload redeploys the latest kept upload.
    Local {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_dir: Option<String>,
    },
    /// Files uploaded with `modula up` (a `source_uploads` row). Only
    /// ever in a deployment's snapshot: an instance can't be configured with it.
    Upload {
        upload: modula_id::Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_dir: Option<String>,
    },
    /// A Cloudflare R2 bucket (see `api::buckets`): nothing runs or deploys,
    /// the service only carries the bucket's credentials as variables.
    R2,
}

/// How a repository source is turned into an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Builder {
    /// Detects the language and builds without a Dockerfile (Railway's builder).
    #[default]
    Railpack,
    Dockerfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    #[default]
    OnFailure,
    Always,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceConfig {
    pub source: Source,
    /// argv; empty/None = image default. Never joined into a shell string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_command: Option<Vec<String>>,
    /// Port the app listens on; injected as PORT.
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck_path: Option<String>,
    #[serde(default = "default_hc_timeout")]
    pub healthcheck_timeout_secs: u32,
    #[serde(default)]
    pub restart_policy: RestartPolicy,
    /// `on_failure` only: stop restarting after this many tries (None = never give up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_policy_max_retries: Option<u32>,
    /// argv run once from the new image before it starts (migrations). Its
    /// failure fails the deployment; the previous one keeps serving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_deploy_command: Option<Vec<String>>,
    /// How long a retired container gets between SIGTERM and SIGKILL
    /// (0 = the node's default). Railway's RAILWAY_DEPLOYMENT_DRAINING_SECONDS.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub draining_seconds: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millis: Option<u32>,
    /// Copies of the service, each on a different server of the private
    /// network (one server scales up with memory and CPU instead). Services
    /// with a volume run one.
    #[serde(default = "one")]
    pub replicas: u32,

    // Repository sources only.
    #[serde(default)]
    pub builder: Builder,
    /// Relative to the root directory; default `Dockerfile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile_path: Option<String>,
    /// Replaces the detected build command (runs inside the build container).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_command: Option<String>,
    /// Deploy on every push to the branch.
    #[serde(default = "yes")]
    pub auto_deploy: bool,
    /// Only pushes touching these paths (gitignore-style globs) deploy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watch_paths: Vec<String>,
    /// A push waits for the commit's other GitHub checks to pass before it
    /// builds (`waiting_ci`), and fails if any of them fails.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wait_for_ci: bool,

    /// Where a built static site is served from; None = this instance's own
    /// servers (the built image runs behind the edge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosting: Option<Hosting>,

    /// A 5-field cron expression in UTC (`0 3 * * *`; `@daily` and the other
    /// nicknames too). Set: nothing runs continuously; at each due time the
    /// service runs once to completion (see `crate::cron`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron_schedule: Option<String>,
    /// How long one run may take before it is stopped (default
    /// [`DEFAULT_CRON_TIMEOUT_SECS`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron_timeout_secs: Option<u32>,
}

pub const DEFAULT_CRON_TIMEOUT_SECS: u32 = 3600;
pub const MAX_CRON_TIMEOUT_SECS: u32 = 24 * 3600;

/// Hosting a static site's build output somewhere other than the servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "type",
    deny_unknown_fields
)]
pub enum Hosting {
    /// Built on a server, then uploaded as Cloudflare Workers static assets
    /// with the workspace's Cloudflare connection.
    Cloudflare {
        /// The build output, relative to the root directory (e.g. `dist`).
        output_dir: String,
        /// Serve `/index.html` for paths that match no file (client-side routing).
        #[serde(default)]
        spa: bool,
        /// The Worker's name; default: `<service>-<environment>`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        script_name: Option<String>,
    },
}

/// A config copied from another environment (`copied`) keeps the Worker name
/// this environment had (`own`), or none: two environments never publish to
/// the same Worker.
pub fn keep_script_name(copied: &mut serde_json::Value, own: &serde_json::Value) {
    let Some(hosting) = copied
        .get_mut("hosting")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    match own.pointer("/hosting/scriptName") {
        Some(name) => {
            hosting.insert("scriptName".into(), name.clone());
        }
        None => {
            hosting.remove("scriptName");
        }
    }
}

/// Cloudflare's rules for Worker names, kept to the DNS-label-safe subset
/// (the name becomes `<name>.<subdomain>.workers.dev`).
pub fn validate_script_name(n: &str) -> AppResult<()> {
    let ok = (1..=63).contains(&n.len())
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !n.starts_with('-')
        && !n.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(AppError::bad_request(
            "Worker name must be 1–63 lowercase letters, digits or dashes",
        ))
    }
}

fn yes() -> bool {
    true
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn one() -> u32 {
    1
}

/// Upper bound for `draining_seconds` (and its variable override).
/// Each on its own server.
pub const MAX_REPLICAS: u32 = 16;
pub const MAX_DRAINING_SECS: u32 = 300;

fn default_port() -> u16 {
    8080
}
fn default_hc_timeout() -> u32 {
    300
}

impl ServiceConfig {
    /// What the deployment runs, for humans: an image, or `owner/repo@branch`.
    pub fn source_label(&self) -> String {
        match &self.source {
            Source::Image { image } => image.clone(),
            Source::Repo { repo, branch, .. } => format!("{repo}@{branch}"),
            Source::Upload { .. } => "Uploaded via CLI".into(),
            Source::Local { .. } => "Local files (modula up)".into(),
            Source::R2 => "Cloudflare R2".into(),
        }
    }

    pub fn repo(&self) -> Option<(&str, &str, Option<&str>)> {
        match &self.source {
            Source::Repo {
                repo,
                branch,
                root_dir,
            } => Some((repo, branch, root_dir.as_deref())),
            Source::Image { .. } | Source::Upload { .. } | Source::Local { .. } | Source::R2 => {
                None
            }
        }
    }

    /// The upload a deployment builds, for uploaded sources.
    pub fn upload(&self) -> Option<modula_id::Id> {
        match &self.source {
            Source::Upload { upload, .. } => Some(*upload),
            _ => None,
        }
    }

    /// Whether deployments build an image on the node (repository or upload).
    pub fn builds_from_source(&self) -> bool {
        !matches!(self.source, Source::Image { .. } | Source::R2)
    }

    /// Whether anything runs: an R2 bucket has no container to deploy.
    pub fn deployable(&self) -> bool {
        !matches!(self.source, Source::R2)
    }

    /// The directory to build from, for sources that are built.
    pub fn root_dir(&self) -> Option<&str> {
        match &self.source {
            Source::Repo { root_dir, .. }
            | Source::Upload { root_dir, .. }
            | Source::Local { root_dir } => root_dir.as_deref(),
            Source::Image { .. } | Source::R2 => None,
        }
    }

    /// The run timeout of a cron service.
    pub fn cron_timeout(&self) -> u32 {
        self.cron_timeout_secs.unwrap_or(DEFAULT_CRON_TIMEOUT_SECS)
    }

    /// The Cloudflare hosting settings, when the build output is hosted there.
    pub fn cloudflare(&self) -> Option<CloudflareHosting<'_>> {
        self.hosting.as_ref().map(|h| match h {
            Hosting::Cloudflare {
                output_dir,
                spa,
                script_name,
            } => CloudflareHosting {
                output_dir: output_dir.trim_matches('/'),
                spa: *spa,
                script_name: script_name.as_deref(),
            },
        })
    }

    /// For instance configs (what the dashboard and API set): everything
    /// [`validate`](Self::validate) checks, and no snapshot-only source.
    pub fn validate_for_instance(&self) -> AppResult<()> {
        if matches!(self.source, Source::Upload { .. }) {
            return Err(AppError::bad_request(
                "a service's source must be an image, a repository or local files (`modula up`)",
            ));
        }
        self.validate()
    }

    pub fn validate(&self) -> AppResult<()> {
        match &self.source {
            Source::Image { image } => validate_image_ref(image)?,
            Source::R2 => {}
            Source::Upload { root_dir, .. } | Source::Local { root_dir } => {
                if let Some(d) = root_dir {
                    validate_rel_path(d, "root directory")?;
                }
            }
            Source::Repo {
                repo,
                branch,
                root_dir,
            } => {
                validate_repo(repo)?;
                validate_branch(branch)?;
                if let Some(d) = root_dir {
                    validate_rel_path(d, "root directory")?;
                }
            }
        }
        if let Some(p) = &self.dockerfile_path {
            validate_rel_path(p, "Dockerfile path")?;
        }
        if let Some(cf) = self.cloudflare() {
            if !self.builds_from_source() {
                return Err(AppError::bad_request(
                    "Cloudflare hosting serves a build's output: use a repository or local files as the source",
                ));
            }
            validate_rel_path(cf.output_dir, "output directory")?;
            if cf.output_dir.is_empty() {
                return Err(AppError::bad_request(
                    "output directory is required (e.g. dist)",
                ));
            }
            if let Some(n) = cf.script_name {
                validate_script_name(n)?;
            }
            if self.replicas != 1 {
                return Err(AppError::bad_request(
                    "a site hosted on Cloudflare is built once: set replicas to 1",
                ));
            }
        }
        if let Some(expr) = &self.cron_schedule {
            crate::cron::parse(expr).map_err(AppError::bad_request)?;
            if self.hosting.is_some() {
                return Err(AppError::bad_request(
                    "a site hosted on Cloudflare can't run on a cron schedule",
                ));
            }
            if self.replicas != 1 {
                return Err(AppError::bad_request(
                    "a cron service runs once per due time: set replicas to 1",
                ));
            }
        }
        if let Some(t) = self.cron_timeout_secs {
            if self.cron_schedule.is_none() {
                return Err(AppError::bad_request(
                    "a cron timeout needs a cron schedule",
                ));
            }
            if !(1..=MAX_CRON_TIMEOUT_SECS).contains(&t) {
                return Err(AppError::bad_request(
                    "cron timeout must be 1–86400 seconds",
                ));
            }
        }
        if !(1..=MAX_REPLICAS).contains(&self.replicas) {
            return Err(AppError::bad_request(format!(
                "replicas must be between 1 and {MAX_REPLICAS}"
            )));
        }
        if let Some(c) = &self.build_command
            && (c.len() > 4096 || c.contains('\0'))
        {
            return Err(AppError::bad_request(
                "build command is too long or contains NUL bytes",
            ));
        }
        if self.watch_paths.len() > 50
            || self
                .watch_paths
                .iter()
                .any(|w| w.is_empty() || w.len() > 256 || w.contains('\0'))
        {
            return Err(AppError::bad_request("invalid watch paths"));
        }
        if let Some(cmd) = &self.start_command
            && (cmd.len() > 64 || cmd.iter().any(|a| a.len() > 4096 || a.contains('\0')))
        {
            return Err(AppError::bad_request(
                "start command is too long or contains NUL bytes",
            ));
        }
        if let Some(cmd) = &self.pre_deploy_command
            && (cmd.len() > 64 || cmd.iter().any(|a| a.len() > 4096 || a.contains('\0')))
        {
            return Err(AppError::bad_request(
                "pre-deploy command is too long or contains NUL bytes",
            ));
        }
        if self.draining_seconds > MAX_DRAINING_SECS {
            return Err(AppError::bad_request("draining time must be 0–300 seconds"));
        }
        if let Some(n) = self.restart_policy_max_retries {
            if self.restart_policy != RestartPolicy::OnFailure {
                return Err(AppError::bad_request(
                    "max retries only applies to the on-failure restart policy",
                ));
            }
            if !(1..=1000).contains(&n) {
                return Err(AppError::bad_request("max retries must be 1–1000"));
            }
        }
        if self.port == 0 {
            return Err(AppError::bad_request("port must be 1–65535"));
        }
        if let Some(p) = &self.healthcheck_path {
            let ok =
                p.starts_with('/') && p.len() <= 512 && p.bytes().all(|b| b.is_ascii_graphic());
            if !ok {
                return Err(AppError::bad_request(
                    "healthcheck path must start with / and contain no spaces",
                ));
            }
        }
        if !(1..=3600).contains(&self.healthcheck_timeout_secs) {
            return Err(AppError::bad_request(
                "healthcheck timeout must be 1–3600 seconds",
            ));
        }
        if let Some(m) = self.memory_mb
            && !(16..=1_048_576).contains(&m)
        {
            return Err(AppError::bad_request("memory limit must be 16 MB – 1 TB"));
        }
        if let Some(c) = self.cpu_millis
            && !(10..=256_000).contains(&c)
        {
            return Err(AppError::bad_request(
                "CPU limit must be 10–256000 millicores",
            ));
        }
        Ok(())
    }
}

/// `owner/name` as GitHub allows it.
pub fn validate_repo(r: &str) -> AppResult<()> {
    let part = |p: &str| {
        !p.is_empty()
            && p.len() <= 100
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    match r.split_once('/') {
        Some((o, n)) if part(o) && part(n) && !o.starts_with('-') => Ok(()),
        _ => Err(AppError::bad_request("repository must be owner/name")),
    }
}

/// A git branch name, conservatively (git check-ref-format plus no spaces).
pub fn validate_branch(b: &str) -> AppResult<()> {
    let ok = !b.is_empty()
        && b.len() <= 255
        && !b.starts_with(['-', '/', '.'])
        && !b.ends_with(['/', '.'])
        && !b.contains("..")
        && !b.contains("//")
        && !b.contains("@{")
        && !b.ends_with(".lock")
        && b.bytes()
            .all(|c| c.is_ascii_graphic() && !b"~^:?*[\\".contains(&c));
    if ok {
        Ok(())
    } else {
        Err(AppError::bad_request("invalid branch name"))
    }
}

/// [`Hosting::Cloudflare`], borrowed.
#[derive(Debug, Clone, Copy)]
pub struct CloudflareHosting<'a> {
    /// Without leading or trailing slashes.
    pub output_dir: &'a str,
    pub spa: bool,
    pub script_name: Option<&'a str>,
}

/// A relative path inside the repository: no absolute paths, no `..`.
pub fn validate_rel_path(p: &str, what: &str) -> AppResult<()> {
    let p = p.trim_matches('/');
    let ok = p.len() <= 512
        && !p.contains('\0')
        && !p.contains('\\')
        && (p.is_empty()
            || p.split('/')
                .all(|seg| !seg.is_empty() && seg != "." && seg != ".."));
    if ok {
        Ok(())
    } else {
        Err(AppError::bad_request(format!(
            "{what} must be a path inside the repository"
        )))
    }
}

/// A registry given as an IP literal (`10.0.0.5:5000`) must not be loopback,
/// link-local, cloud metadata or unspecified: the node's Docker daemon would
/// fetch from its own host. Private (LAN) registries are legitimate and stay
/// allowed; so do names (`localhost:5000` included: a registry beside each
/// node is a common setup, and names are resolved by Docker, not us).
fn registry_ip_ok(host_port: &str) -> bool {
    let host = host_port.split(':').next().unwrap_or_default();
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => {
            !(modula_backup::net::loopback(ip)
                || modula_backup::net::link_local_or_metadata(ip)
                || ip.is_unspecified()
                || ip.is_multicast())
        }
        Err(_) => true,
    }
}

/// Docker reference grammar, conservatively: [registry[:port]/]path[:tag][@sha256:hex].
pub fn validate_image_ref(r: &str) -> AppResult<()> {
    let bad = || Err(AppError::bad_request("invalid image reference"));
    if r.is_empty() || r.len() > 512 || r.starts_with('-') {
        return bad();
    }
    let (name_tag, digest) = match r.split_once('@') {
        Some((n, d)) => (n, Some(d)),
        None => (r, None),
    };
    if let Some(d) = digest {
        let hex = d.strip_prefix("sha256:");
        if !hex.is_some_and(|h| {
            h.len() == 64
                && h.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        }) {
            return bad();
        }
    }
    // A tag is after the last ':' that comes after the last '/'.
    let last_slash = name_tag.rfind('/').map_or(0, |i| i + 1);
    let (name, tag) = match name_tag[last_slash..].rfind(':') {
        Some(i) => (
            &name_tag[..last_slash + i],
            Some(&name_tag[last_slash + i + 1..]),
        ),
        None => (name_tag, None),
    };
    if let Some(t) = tag {
        let ok = !t.is_empty()
            && t.len() <= 128
            && t.as_bytes()[0] != b'.'
            && t.as_bytes()[0] != b'-'
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
        if !ok {
            return bad();
        }
    }
    let mut parts = name.split('/').peekable();
    let mut first = true;
    while let Some(p) = parts.next() {
        let is_registry = first
            && parts.peek().is_some()
            && (p.contains('.') || p.contains(':') || p == "localhost");
        first = false;
        let ok = if is_registry {
            // A host: starts with a letter or digit and has no empty labels.
            p.as_bytes()[0].is_ascii_alphanumeric()
                && !p.contains("..")
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
                && registry_ip_ok(p)
        } else {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
                && p.as_bytes()[0].is_ascii_alphanumeric()
        };
        if !ok {
            return bad();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_refs() {
        for ok in [
            "nginx",
            "nginx:1.27-alpine",
            "library/nginx",
            "ghcr.io/acme/api:v1.2.3",
            "localhost:5000/app",
            "registry.example.com:8443/team/app_name:latest",
            // LAN registries are fine.
            "10.0.0.5:5000/team/app",
            "192.168.1.2/app:v1",
            "nginx@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            assert!(validate_image_ref(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "-rm",
            "Nginx",
            "nginx:",
            "nginx:-x",
            "nginx; rm -rf /",
            "nginx $(id)",
            "a//b",
            "nginx@sha256:short",
            "nginx@md5:0123",
            "../etc",
            // The node's own host and cloud metadata are not registries.
            "127.0.0.1:5000/app",
            "169.254.169.254/latest",
            "0.0.0.0:5000/app",
        ] {
            assert!(validate_image_ref(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn repo_sources() {
        let c: ServiceConfig = serde_json::from_value(serde_json::json!({
            "source": { "type": "repo", "repo": "acme/api", "branch": "main", "rootDir": "services/api" },
            "builder": "dockerfile",
            "dockerfilePath": "docker/Dockerfile.prod",
        }))
        .unwrap();
        c.validate().unwrap();
        assert!(c.auto_deploy);
        assert_eq!(c.source_label(), "acme/api@main");
        for bad in [
            "acme", "acme/", "/api", "a/b/c", "acme/a b", "-x/y", "acme/..",
        ] {
            assert!(validate_repo(bad).is_err(), "{bad}");
        }
        for ok in ["main", "release/1.2", "feat_x-y"] {
            assert!(validate_branch(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "a..b", "a b", "a~1", "x.lock", "a/", "@{u}"] {
            assert!(validate_branch(bad).is_err(), "{bad}");
        }
        for bad in ["../x", "a/../b", "a//b", "a\\b"] {
            assert!(validate_rel_path(bad, "x").is_err(), "{bad}");
        }
        assert!(validate_rel_path("/apps/web/", "x").is_ok());
    }

    #[test]
    fn config_roundtrip_and_validation() {
        let c: ServiceConfig = serde_json::from_value(serde_json::json!({
            "source": { "type": "image", "image": "nginx:alpine" },
            "port": 80,
            "healthcheckPath": "/",
        }))
        .unwrap();
        assert_eq!(c.restart_policy, RestartPolicy::OnFailure);
        assert_eq!(c.healthcheck_timeout_secs, 300);
        c.validate().unwrap();
        let mut bad = c.clone();
        bad.healthcheck_path = Some("/a b".into());
        assert!(bad.validate().is_err());
        let mut bad = c.clone();
        bad.start_command = Some(vec!["a\0b".into()]);
        assert!(bad.validate().is_err());

        // Deploy settings: pre-deploy argv, draining and max retries.
        let d: ServiceConfig = serde_json::from_value(serde_json::json!({
            "source": { "type": "image", "image": "api:1" },
            "preDeployCommand": ["bunx", "prisma", "migrate", "deploy"],
            "drainingSeconds": 30,
            "restartPolicyMaxRetries": 5,
        }))
        .unwrap();
        d.validate().unwrap();
        assert_eq!(d.pre_deploy_command.as_deref().unwrap()[0], "bunx");
        assert_eq!(
            (d.draining_seconds, d.restart_policy_max_retries),
            (30, Some(5))
        );
        let mut bad = d.clone();
        bad.draining_seconds = 301;
        assert!(bad.validate().is_err());
        let mut bad = d.clone();
        bad.restart_policy_max_retries = Some(0);
        assert!(bad.validate().is_err());
        let mut bad = d.clone();
        bad.restart_policy = RestartPolicy::Always;
        assert!(bad.validate().is_err(), "max retries needs on_failure");
        let mut bad = d.clone();
        bad.pre_deploy_command = Some(vec!["x\0".into()]);
        assert!(bad.validate().is_err());
        // Defaults stay out of the stored JSON.
        let v = serde_json::to_value(&c).unwrap();
        assert!(v.get("drainingSeconds").is_none() && v.get("preDeployCommand").is_none());
        assert!(
            serde_json::from_value::<ServiceConfig>(serde_json::json!({
                "source": { "type": "image", "image": "nginx" },
                "privileged": true
            }))
            .is_err(),
            "unknown (privileged) fields are rejected"
        );
    }
}
