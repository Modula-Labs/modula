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
    /// A JavaScript function: a module worker (`export default { fetch }`)
    /// that workerd runs on the servers, or Cloudflare Workers runs with
    /// `hosting` set. Its code is written in the dashboard (`code`, plain
    /// JavaScript) or bundled from a repository (`repo`, `branch`,
    /// `root_dir`, `entry`; TypeScript and npm dependencies welcome).
    Function {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_dir: Option<String>,
        /// The entry module, relative to the root directory (default
        /// [`DEFAULT_FUNCTION_ENTRY`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<String>,
        /// The Workers runtime version the code is written for (YYYY-MM-DD).
        compatibility_date: String,
    },
}

/// Inline function code is at most this big.
pub const MAX_FUNCTION_CODE_BYTES: usize = 1024 * 1024;
pub const DEFAULT_FUNCTION_ENTRY: &str = "index.ts";
/// The newest compatibility date the workerd that nodes run supports (its
/// release date; see `scripts/fetch-build-tools.sh`).
pub const WORKERD_COMPATIBILITY_DATE: &str = "2026-10-14";
/// Cloudflare's first compatibility date.
const FIRST_COMPATIBILITY_DATE: &str = "2021-11-02";

/// [`Source::Function`], borrowed.
#[derive(Debug, Clone, Copy)]
pub struct FunctionSource<'a> {
    /// Inline code; None = bundled from the repository.
    pub code: Option<&'a str>,
    /// The entry module of a repository function.
    pub entry: &'a str,
    pub compatibility_date: &'a str,
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

    /// Where a built static site or a function is served from; None = this
    /// instance's own servers (behind the edge).
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

    /// Rules the edge applies to requests for the service's domains, in
    /// order (see [`super::edge`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edge_rules: Vec<super::edge::EdgeRule>,

    /// Scale to zero: the container stops after this long without requests
    /// and starts again on the next one (a cold start). One replica, no
    /// volume; only requests to its domains wake it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serverless: Option<Serverless>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Serverless {
    #[serde(default = "default_idle_minutes")]
    pub idle_minutes: u32,
}

pub const DEFAULT_IDLE_MINUTES: u32 = 10;
pub const MAX_IDLE_MINUTES: u32 = 24 * 60;

fn default_idle_minutes() -> u32 {
    DEFAULT_IDLE_MINUTES
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
    /// Uploaded to Cloudflare Workers with the workspace's Cloudflare
    /// connection: a static site's build output as static assets (built on
    /// a server), or a function as the Worker's script.
    Cloudflare {
        /// Static sites: the build output, relative to the root directory
        /// (e.g. `dist`). Empty for functions.
        #[serde(default)]
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
            Source::Function {
                repo: Some(repo),
                branch: Some(branch),
                ..
            } => format!("{repo}@{branch}"),
            Source::Function { .. } => "Function".into(),
        }
    }

    pub fn repo(&self) -> Option<(&str, &str, Option<&str>)> {
        match &self.source {
            Source::Repo {
                repo,
                branch,
                root_dir,
            }
            | Source::Function {
                repo: Some(repo),
                branch: Some(branch),
                root_dir,
                ..
            } => Some((repo, branch, root_dir.as_deref())),
            Source::Image { .. }
            | Source::Upload { .. }
            | Source::Local { .. }
            | Source::R2
            | Source::Function { .. } => None,
        }
    }

    /// The function's code settings, for function sources.
    pub fn function(&self) -> Option<FunctionSource<'_>> {
        match &self.source {
            Source::Function {
                code,
                entry,
                compatibility_date,
                ..
            } => Some(FunctionSource {
                code: code.as_deref(),
                entry: entry.as_deref().unwrap_or(DEFAULT_FUNCTION_ENTRY),
                compatibility_date,
            }),
            _ => None,
        }
    }

    /// The upload a deployment builds, for uploaded sources.
    pub fn upload(&self) -> Option<modula_id::Id> {
        match &self.source {
            Source::Upload { upload, .. } => Some(*upload),
            _ => None,
        }
    }

    /// Whether deployments build on the node (repository or upload, or a
    /// function's repository).
    pub fn builds_from_source(&self) -> bool {
        match &self.source {
            Source::Image { .. } | Source::R2 => false,
            Source::Function { code, .. } => code.is_none(),
            Source::Repo { .. } | Source::Upload { .. } | Source::Local { .. } => true,
        }
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
            | Source::Local { root_dir }
            | Source::Function { root_dir, .. } => root_dir.as_deref(),
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
            Source::Function {
                code,
                repo,
                branch,
                root_dir,
                entry,
                compatibility_date,
            } => {
                match (code, repo, branch) {
                    (Some(code), None, None) => {
                        if code.trim().is_empty() {
                            return Err(AppError::bad_request("the function has no code"));
                        }
                        if code.len() > MAX_FUNCTION_CODE_BYTES || code.contains('\0') {
                            return Err(AppError::bad_request(
                                "function code must be at most 1 MB, without NUL bytes: deploy bigger functions from a repository",
                            ));
                        }
                        if entry.is_some() || root_dir.is_some() {
                            return Err(AppError::bad_request(
                                "an entry file and root directory only apply to a function from a repository",
                            ));
                        }
                    }
                    (None, Some(repo), Some(branch)) => {
                        validate_repo(repo)?;
                        validate_branch(branch)?;
                        if let Some(d) = root_dir {
                            validate_rel_path(d, "root directory")?;
                        }
                        if let Some(e) = entry {
                            validate_function_entry(e)?;
                        }
                    }
                    _ => {
                        return Err(AppError::bad_request(
                            "a function has either code or a repository and branch",
                        ));
                    }
                }
                validate_compatibility_date(compatibility_date, self.hosting.is_some())?;
                if self.cron_schedule.is_some() {
                    return Err(AppError::bad_request(
                        "a function answers requests: it can't run on a cron schedule",
                    ));
                }
                if self.start_command.is_some() || self.pre_deploy_command.is_some() {
                    return Err(AppError::bad_request(
                        "a function has no start or pre-deploy command",
                    ));
                }
                if self
                    .cloudflare()
                    .is_some_and(|cf| !cf.output_dir.is_empty() || cf.spa)
                {
                    return Err(AppError::bad_request(
                        "a function on Cloudflare has no output directory or single-page app setting",
                    ));
                }
            }
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
        if let Some(cf) = self.cloudflare()
            && self.function().is_none()
        {
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
        }
        if let Some(cf) = self.cloudflare() {
            if let Some(n) = cf.script_name {
                validate_script_name(n)?;
            }
            if self.replicas != 1 {
                return Err(AppError::bad_request(
                    "a service hosted on Cloudflare is published once: set replicas to 1",
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
        if let Some(s) = &self.serverless {
            if !(1..=MAX_IDLE_MINUTES).contains(&s.idle_minutes) {
                return Err(AppError::bad_request(
                    "serverless idle time must be 1–1440 minutes",
                ));
            }
            if self.replicas != 1 {
                return Err(AppError::bad_request(
                    "a serverless service runs one replica: set replicas to 1",
                ));
            }
            if self.cron_schedule.is_some() {
                return Err(AppError::bad_request(
                    "a cron service already runs only when due: it can't also be serverless",
                ));
            }
            if self.hosting.is_some() {
                return Err(AppError::bad_request(
                    "a site hosted on Cloudflare doesn't run on the servers: it can't be serverless",
                ));
            }
            if self.function().is_some() {
                return Err(AppError::bad_request(
                    "a function is always warm and costs next to nothing idle: it can't be serverless",
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
        super::edge::validate(&self.edge_rules)?;
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

/// A repository function's entry module: a JavaScript or TypeScript file
/// inside the repository, named plainly (it reaches the bundler's argv).
pub fn validate_function_entry(e: &str) -> AppResult<()> {
    let plain = e.len() <= 256
        && !e.starts_with(['-', '/'])
        && e.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b));
    let script = [".js", ".mjs", ".ts", ".mts", ".jsx", ".tsx"]
        .iter()
        .any(|x| e.ends_with(x));
    if plain && script && validate_rel_path(e, "entry file").is_ok() {
        Ok(())
    } else {
        Err(AppError::bad_request(
            "entry file must be a .js, .mjs, .ts, .mts, .jsx or .tsx file inside the repository",
        ))
    }
}

/// `YYYY-MM-DD`, from Cloudflare's first compatibility date to the newest
/// the runtime supports: the servers' workerd, or today on Cloudflare.
pub fn validate_compatibility_date(d: &str, on_cloudflare: bool) -> AppResult<()> {
    let Some(date) = parse_date(d) else {
        return Err(AppError::bad_request(
            "compatibility date must be a date like 2026-10-01",
        ));
    };
    let newest = if on_cloudflare {
        time::OffsetDateTime::now_utc().date()
    } else {
        parse_date(WORKERD_COMPATIBILITY_DATE).expect("a valid date")
    };
    let first = parse_date(FIRST_COMPATIBILITY_DATE).expect("a valid date");
    if date < first {
        return Err(AppError::bad_request(format!(
            "compatibility date must be {FIRST_COMPATIBILITY_DATE} or later"
        )));
    }
    if date > newest {
        return Err(AppError::bad_request(if on_cloudflare {
            "compatibility date can't be in the future".to_owned()
        } else {
            format!(
                "compatibility date must be {WORKERD_COMPATIBILITY_DATE} or earlier (the newest the servers' runtime supports)"
            )
        }));
    }
    Ok(())
}

/// `YYYY-MM-DD` exactly.
fn parse_date(d: &str) -> Option<time::Date> {
    let b = d.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' || !digits(0..4) || !digits(5..7) {
        return None;
    }
    if !digits(8..10) {
        return None;
    }
    let month = time::Month::try_from(d[5..7].parse::<u8>().ok()?).ok()?;
    time::Date::from_calendar_date(d[..4].parse().ok()?, month, d[8..].parse().ok()?).ok()
}

/// [`Hosting::Cloudflare`], borrowed.
#[derive(Debug, Clone, Copy)]
pub struct CloudflareHosting<'a> {
    /// Without leading or trailing slashes; empty for a function.
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
    fn serverless_settings() {
        let c: ServiceConfig = serde_json::from_value(serde_json::json!({
            "source": { "type": "image", "image": "api:1" },
            "serverless": {},
        }))
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.serverless.unwrap().idle_minutes, DEFAULT_IDLE_MINUTES);
        let with = |f: &dyn Fn(&mut ServiceConfig)| {
            let mut c = c.clone();
            f(&mut c);
            c.validate()
        };
        assert!(with(&|c| c.serverless = Some(Serverless { idle_minutes: 1440 })).is_ok());
        assert!(with(&|c| c.serverless = Some(Serverless { idle_minutes: 0 })).is_err());
        assert!(with(&|c| c.serverless = Some(Serverless { idle_minutes: 1441 })).is_err());
        assert!(with(&|c| c.replicas = 2).is_err(), "single replica");
        assert!(with(&|c| c.cron_schedule = Some("@daily".into())).is_err());
        let site = serde_json::json!({
            "source": { "type": "repo", "repo": "acme/web", "branch": "main" },
            "hosting": { "type": "cloudflare", "outputDir": "dist" },
            "serverless": { "idleMinutes": 5 },
        });
        let site: ServiceConfig = serde_json::from_value(site).unwrap();
        assert!(site.validate().is_err(), "Cloudflare hosting");
        assert!(
            serde_json::from_value::<ServiceConfig>(serde_json::json!({
                "source": { "type": "image", "image": "api:1" },
                "serverless": { "idleSeconds": 5 },
            }))
            .is_err()
        );
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
        assert!(v.get("serverless").is_none());
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

    fn function(v: serde_json::Value) -> AppResult<ServiceConfig> {
        let c: ServiceConfig =
            serde_json::from_value(v).map_err(|e| AppError::bad_request(e.to_string()))?;
        c.validate_for_instance()?;
        Ok(c)
    }

    #[test]
    fn functions_have_inline_code_or_a_repository() {
        let inline = function(serde_json::json!({
            "source": { "type": "function", "code": "export default {}", "compatibilityDate": "2026-10-01" }
        }))
        .unwrap();
        assert!(!inline.builds_from_source(), "inline code isn't built");
        assert!(inline.repo().is_none());
        assert_eq!(inline.source_label(), "Function");
        let f = inline.function().unwrap();
        assert_eq!(
            (f.code, f.compatibility_date),
            (Some("export default {}"), "2026-10-01")
        );

        let repo = function(serde_json::json!({
            "source": { "type": "function", "repo": "acme/fns", "branch": "main",
                        "rootDir": "hello", "entry": "src/index.ts", "compatibilityDate": "2025-01-01" },
            "hosting": { "type": "cloudflare" }
        }))
        .unwrap();
        assert!(repo.builds_from_source());
        assert_eq!(repo.repo(), Some(("acme/fns", "main", Some("hello"))));
        assert_eq!(repo.function().unwrap().entry, "src/index.ts");
        assert_eq!(repo.source_label(), "acme/fns@main");
        assert!(repo.cloudflare().is_some(), "functionTarget cloudflare");
        let default_entry = function(serde_json::json!({
            "source": { "type": "function", "repo": "acme/fns", "branch": "main", "compatibilityDate": "2025-01-01" }
        }))
        .unwrap();
        assert_eq!(
            default_entry.function().unwrap().entry,
            DEFAULT_FUNCTION_ENTRY
        );

        let big = "x".repeat(MAX_FUNCTION_CODE_BYTES + 1);
        for (bad, why) in [
            (
                serde_json::json!({ "type": "function", "compatibilityDate": "2026-10-01" }),
                "no code",
            ),
            (
                serde_json::json!({ "type": "function", "code": "  ", "compatibilityDate": "2026-10-01" }),
                "blank",
            ),
            (
                serde_json::json!({ "type": "function", "code": big, "compatibilityDate": "2026-10-01" }),
                "too big",
            ),
            (
                serde_json::json!({ "type": "function", "code": "a\u{0}", "compatibilityDate": "2026-10-01" }),
                "NUL",
            ),
            (
                serde_json::json!({ "type": "function", "code": "x", "repo": "a/b", "branch": "main", "compatibilityDate": "2026-10-01" }),
                "both",
            ),
            (
                serde_json::json!({ "type": "function", "repo": "a/b", "compatibilityDate": "2026-10-01" }),
                "no branch",
            ),
            (
                serde_json::json!({ "type": "function", "code": "x", "entry": "a.ts", "compatibilityDate": "2026-10-01" }),
                "entry inline",
            ),
            (
                serde_json::json!({ "type": "function", "repo": "a/b", "branch": "main", "entry": "../x.ts", "compatibilityDate": "2026-10-01" }),
                "entry outside",
            ),
            (
                serde_json::json!({ "type": "function", "repo": "a/b", "branch": "main", "entry": "-x.ts", "compatibilityDate": "2026-10-01" }),
                "entry flag",
            ),
            (
                serde_json::json!({ "type": "function", "repo": "a/b", "branch": "main", "entry": "x.py", "compatibilityDate": "2026-10-01" }),
                "not js",
            ),
            (
                serde_json::json!({ "type": "function", "code": "x", "compatibilityDate": "2026-13-01" }),
                "no such month",
            ),
            (
                serde_json::json!({ "type": "function", "code": "x", "compatibilityDate": "26-10-01" }),
                "format",
            ),
            (
                serde_json::json!({ "type": "function", "code": "x", "compatibilityDate": "2020-01-01" }),
                "too old",
            ),
            (
                serde_json::json!({ "type": "function", "code": "x", "compatibilityDate": "2099-01-01" }),
                "newer than workerd",
            ),
        ] {
            assert!(
                function(serde_json::json!({ "source": bad })).is_err(),
                "{why}"
            );
        }
        let src = serde_json::json!({ "type": "function", "code": "x", "compatibilityDate": "2026-10-01" });
        for (extra, why) in [
            (serde_json::json!({ "cronSchedule": "@daily" }), "cron"),
            (
                serde_json::json!({ "startCommand": ["node"] }),
                "start command",
            ),
            (
                serde_json::json!({ "preDeployCommand": ["x"] }),
                "pre-deploy",
            ),
            (
                serde_json::json!({ "hosting": { "type": "cloudflare", "outputDir": "dist" } }),
                "output dir",
            ),
            (
                serde_json::json!({ "hosting": { "type": "cloudflare", "spa": true } }),
                "spa",
            ),
            (
                serde_json::json!({ "hosting": { "type": "cloudflare" }, "replicas": 2 }),
                "replicas on Cloudflare",
            ),
            (
                serde_json::json!({ "serverless": { "idleMinutes": 5 } }),
                "serverless",
            ),
        ] {
            let mut v = extra.clone();
            v["source"] = src.clone();
            assert!(function(v).is_err(), "{why}");
        }
        // Replicas on the servers are fine: each runs it in its workerd.
        function(serde_json::json!({ "source": src, "replicas": 3 })).unwrap();
        // A static site still needs its output directory.
        assert!(
            function(serde_json::json!({
                "source": { "type": "repo", "repo": "a/b", "branch": "main" },
                "hosting": { "type": "cloudflare" }
            }))
            .is_err()
        );
    }

    #[test]
    fn compatibility_dates() {
        validate_compatibility_date(WORKERD_COMPATIBILITY_DATE, false).unwrap();
        validate_compatibility_date("2021-11-02", false).unwrap();
        assert!(validate_compatibility_date("2021-11-01", false).is_err());
        assert!(validate_compatibility_date("2099-12-31", true).is_err());
        assert!(validate_compatibility_date("2026-02-30", false).is_err());
        let today = time::OffsetDateTime::now_utc().date();
        let today = format!(
            "{:04}-{:02}-{:02}",
            today.year(),
            u8::from(today.month()),
            today.day()
        );
        validate_compatibility_date(&today, true).unwrap();
    }
}
