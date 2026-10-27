//! Config as code: `modula.toml`, `railway.toml` or `railway.json` at the
//! build root, with Railway's keys (`[build]`: builder, buildCommand,
//! dockerfilePath; `[deploy]`: startCommand, healthcheckPath,
//! healthcheckTimeout, restartPolicyType, restartPolicyMaxRetries,
//! preDeployCommand). `watchPatterns` is not read: whether a push deploys is
//! decided before anything is built, from the dashboard's watch paths.
//!
//! The first file found in that order wins (files are not merged), and its
//! values override the dashboard's. A `modula.toml` that describes a whole
//! project (`modula config`) is not a build config and is skipped. The file is repository content, so it is
//! untrusted: only the keys above are read, unknown keys are ignored, and an
//! invalid value is reported as a warning and falls back to the dashboard.

use std::io::Read;
use std::path::{Component, Path};

use modula_proto::node::v1 as pb;
use serde_json::{Map, Value};

/// Candidate files, highest precedence first.
pub const FILES: &[&str] = &["modula.toml", "railway.toml", "railway.json"];
const MAX_FILE: u64 = 64 * 1024;
const MAX_COMMAND: usize = 4096;
const MAX_HEALTH_PATH: usize = 512;
const MAX_DOCKERFILE_PATH: usize = 1024;
/// Health check timeout when the file sets a path but the dashboard had none.
const DEFAULT_HEALTH_TIMEOUT: u32 = 300;

/// Valid settings read from one file; `None` means "use the dashboard's".
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RepoConfig {
    /// The file the settings came from.
    pub file: &'static str,
    pub builder: Option<pb::Builder>,
    pub build_command: Option<String>,
    pub dockerfile_path: Option<String>,
    pub start_command: Option<String>,
    pub healthcheck_path: Option<String>,
    pub healthcheck_timeout: Option<u32>,
    pub restart_policy: Option<pb::RestartPolicy>,
    pub restart_max_retries: Option<u32>,
    /// Shell commands, run in order before the deployment starts.
    pub pre_deploy_command: Option<Vec<String>>,
}

/// What a build changes about how its container runs.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RuntimeOverrides {
    /// Replaces `Workload.command` (an empty argv runs the image default).
    pub command: Option<Vec<String>>,
    /// Where `command` came from (see `Workload.command_source`).
    pub command_source: Option<String>,
    pub healthcheck_path: Option<String>,
    pub healthcheck_timeout: Option<u32>,
    pub restart_policy: Option<pb::RestartPolicy>,
    pub restart_max_retries: Option<u32>,
    /// Replaces `Workload.pre_deploy_command` (container argv).
    pub pre_deploy_command: Option<Vec<String>>,
}

impl RuntimeOverrides {
    /// Reported to the server with BUILT so it can record them.
    pub fn to_output(&self) -> pb::BuildOutput {
        pb::BuildOutput {
            has_command: self.command.is_some(),
            command: self.command.clone().unwrap_or_default(),
            healthcheck_path: self.healthcheck_path.clone().unwrap_or_default(),
            healthcheck_timeout_secs: self.healthcheck_timeout.unwrap_or(0),
            restart: self.restart_policy.map_or(0, |r| r as i32),
            stack: String::new(),
            has_pre_deploy: self.pre_deploy_command.is_some(),
            pre_deploy_command: self.pre_deploy_command.clone().unwrap_or_default(),
            restart_max_retries: self.restart_max_retries.unwrap_or(0),
        }
    }

    pub fn apply(&self, w: &mut pb::Workload) {
        if let Some(cmd) = &self.command {
            w.command = cmd.clone();
            w.command_source = self.command_source.clone().unwrap_or_default();
        }
        if self.healthcheck_path.is_some() || self.healthcheck_timeout.is_some() {
            let health = w.health.get_or_insert_with(Default::default);
            if let Some(path) = &self.healthcheck_path {
                health.path = path.clone();
            }
            match self.healthcheck_timeout {
                Some(t) => health.timeout_secs = t,
                None if health.timeout_secs == 0 => health.timeout_secs = DEFAULT_HEALTH_TIMEOUT,
                None => {}
            }
        }
        if let Some(r) = self.restart_policy {
            w.restart = r as i32;
        }
        if let Some(n) = self.restart_max_retries {
            w.restart_max_retries = n;
        }
        // A retry limit only means something for on-failure.
        if matches!(
            pb::RestartPolicy::try_from(w.restart),
            Ok(pb::RestartPolicy::Always | pb::RestartPolicy::Never)
        ) {
            w.restart_max_retries = 0;
        }
        if let Some(cmd) = &self.pre_deploy_command {
            w.pre_deploy_command = cmd.clone();
        }
    }
}

/// The container command for a start command in a Dockerfile-built image:
/// run inside the app container (never on the node), as Railway does.
pub fn container_shell(cmd: &str) -> Vec<String> {
    const SHELL: [&str; 2] = ["/bin/sh", "-c"];
    let mut argv: Vec<String> = SHELL.iter().map(|s| (*s).to_owned()).collect();
    argv.push(cmd.to_owned());
    argv
}

/// Read the highest-precedence config file in `dir`, if any. Problems with
/// the file are returned as warnings; they never fail the build.
pub fn load(dir: &Path) -> (Option<RepoConfig>, Vec<String>) {
    for &file in FILES {
        let path = dir.join(file);
        // Not following symlinks: a regular file at the build root only.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            return (None, vec![format!("{file} is not a regular file; ignored")]);
        }
        if meta.len() > MAX_FILE {
            return (
                None,
                vec![format!(
                    "{file} is larger than {} KB; ignored",
                    MAX_FILE / 1024
                )],
            );
        }
        let mut text = String::new();
        let read =
            std::fs::File::open(&path).and_then(|f| f.take(MAX_FILE).read_to_string(&mut text));
        if let Err(e) = read {
            return (None, vec![format!("cannot read {file}: {e}")]);
        }
        if file == "modula.toml" && is_project_file(&text) {
            continue;
        }
        let (cfg, warnings) = parse(file, &text);
        return (cfg, warnings);
    }
    (None, vec![])
}

/// A `modula.toml` describing a whole project (`modula config`:
/// `project`/`services`, no `[build]` or `[deploy]`) isn't a build config,
/// so a `railway.toml`/`railway.json` next to it still applies.
fn is_project_file(text: &str) -> bool {
    toml::from_str::<Map<String, Value>>(text).is_ok_and(|root| {
        !root.contains_key("build")
            && !root.contains_key("deploy")
            && (root.contains_key("services") || root.contains_key("project"))
    })
}

/// Parse one file's contents (`file` picks TOML or JSON by extension).
pub fn parse(file: &'static str, text: &str) -> (Option<RepoConfig>, Vec<String>) {
    let root: Result<Value, String> = if file.ends_with(".json") {
        serde_json::from_str(text).map_err(|e| e.to_string())
    } else {
        toml::from_str(text).map_err(|e| e.to_string())
    };
    let root = match root {
        Ok(Value::Object(m)) => m,
        Ok(_) => return (None, vec![format!("{file} is not a table/object; ignored")]),
        Err(e) => {
            return (
                None,
                vec![format!(
                    "{file} could not be parsed; ignored: {}",
                    one_line(&e)
                )],
            );
        }
    };
    let mut p = Parser {
        file,
        warnings: vec![],
    };
    let empty = Map::new();
    let build = p.section(&root, "build").unwrap_or(&empty);
    let deploy = p.section(&root, "deploy").unwrap_or(&empty);
    let cfg = RepoConfig {
        file,
        builder: p.choice(build, "build.builder", "builder", |s| match s {
            "RAILPACK" | "NIXPACKS" => Some(pb::Builder::Railpack),
            "DOCKERFILE" => Some(pb::Builder::Dockerfile),
            _ => None,
        }),
        build_command: p.command(build, "build.buildCommand", "buildCommand"),
        dockerfile_path: p.string(
            build,
            "build.dockerfilePath",
            "dockerfilePath",
            valid_dockerfile_path,
        ),
        start_command: p.command(deploy, "deploy.startCommand", "startCommand"),
        healthcheck_path: p.string(
            deploy,
            "deploy.healthcheckPath",
            "healthcheckPath",
            valid_health_path,
        ),
        healthcheck_timeout: p.timeout(deploy),
        restart_policy: p.choice(
            deploy,
            "deploy.restartPolicyType",
            "restartPolicyType",
            |s| match s {
                "ON_FAILURE" => Some(pb::RestartPolicy::OnFailure),
                "ALWAYS" => Some(pb::RestartPolicy::Always),
                "NEVER" => Some(pb::RestartPolicy::Never),
                _ => None,
            },
        ),
        restart_max_retries: p.max_retries(deploy),
        pre_deploy_command: p.pre_deploy(deploy),
    };
    (Some(cfg), p.warnings)
}

impl RepoConfig {
    /// Override the dashboard's build settings with the file's.
    pub fn apply_build(&self, spec: &mut pb::Build) {
        if let Some(b) = self.builder {
            spec.builder = b as i32;
        }
        if let Some(c) = &self.build_command {
            spec.build_command = c.clone();
        }
        if let Some(p) = &self.dockerfile_path {
            spec.dockerfile_path = p.clone();
        }
        if let Some(c) = &self.start_command {
            spec.start_command = c.clone();
        }
    }

    /// Runtime overrides from the file (the start command is decided by the
    /// builder, see `effective_command`).
    pub fn runtime(&self) -> RuntimeOverrides {
        RuntimeOverrides {
            command: None,
            command_source: None,
            healthcheck_path: self.healthcheck_path.clone(),
            healthcheck_timeout: self.healthcheck_timeout,
            restart_policy: self.restart_policy,
            restart_max_retries: self.restart_max_retries,
            // Railway runs each in a shell, in order, stopping at the first failure.
            pre_deploy_command: self
                .pre_deploy_command
                .as_ref()
                .map(|cmds| container_shell(&cmds.join(" && "))),
        }
    }

    /// "Using railway.toml: start command, health check /health", or None
    /// when the file set nothing.
    pub fn summary(&self) -> Option<String> {
        let mut parts: Vec<String> = vec![];
        if let Some(b) = self.builder {
            parts.push(format!(
                "builder {}",
                if b == pb::Builder::Dockerfile {
                    "Dockerfile"
                } else {
                    "Railpack"
                }
            ));
        }
        if self.build_command.is_some() {
            parts.push("build command".into());
        }
        if let Some(p) = &self.dockerfile_path {
            parts.push(format!("Dockerfile {p}"));
        }
        if self.start_command.is_some() {
            parts.push("start command".into());
        }
        if let Some(p) = &self.healthcheck_path {
            parts.push(format!("health check {p}"));
        }
        if let Some(t) = self.healthcheck_timeout {
            parts.push(format!("health check timeout {t}s"));
        }
        if let Some(r) = self.restart_policy {
            parts.push(format!("restart policy {}", r.as_str_name()));
        }
        if let Some(n) = self.restart_max_retries {
            parts.push(format!("max {n} restarts"));
        }
        if self.pre_deploy_command.is_some() {
            parts.push("pre-deploy command".into());
        }
        (!parts.is_empty()).then(|| format!("Using {}: {}", self.file, parts.join(", ")))
    }
}

/// The start command a built container runs, given the effective spec (after
/// `apply_build`, with the builder decided): a replacement for the
/// workload's argv and where it came from, or None to keep the dashboard's.
/// Railpack bakes its start command into the image; a Dockerfile image runs
/// a start command the user set, else its own CMD/ENTRYPOINT.
pub fn effective_command(
    spec: &pb::Build,
    cfg: Option<&RepoConfig>,
) -> Option<(Vec<String>, String)> {
    let file = cfg.filter(|c| c.start_command.is_some()).map(|c| c.file);
    match pb::Builder::try_from(spec.builder).unwrap_or(pb::Builder::Railpack) {
        pb::Builder::Dockerfile => match file {
            Some(f) => Some((container_shell(&spec.start_command), f.to_owned())),
            // Filled in by detection for Railpack: not the user's choice.
            None if spec.start_command_detected => Some((vec![], String::new())),
            None => None,
        },
        pb::Builder::Railpack => Some((vec![], String::new())),
        _ => None,
    }
}

/// Auto: the Dockerfile when the build root has one (or `dockerfile_path`),
/// else Railpack, with the reason for the build log.
pub fn resolve_auto(context: &Path, dockerfile_path: &str) -> (pb::Builder, String) {
    let rel = if dockerfile_path.is_empty() {
        "Dockerfile"
    } else {
        dockerfile_path
    };
    let found = context
        .join(rel.trim_matches('/'))
        .canonicalize()
        .is_ok_and(|f| f.starts_with(context) && f.is_file());
    if found {
        (
            pb::Builder::Dockerfile,
            format!("Dockerfile (found {rel} at the build root)"),
        )
    } else {
        (
            pb::Builder::Railpack,
            format!("Railpack (no {rel} at the build root)"),
        )
    }
}

struct Parser {
    file: &'static str,
    warnings: Vec<String>,
}

impl Parser {
    fn warn(&mut self, key: &str, why: &str) {
        self.warnings.push(format!(
            "{}: ignoring {key} ({why}); using the dashboard setting",
            self.file
        ));
    }

    fn section<'a>(
        &mut self,
        root: &'a Map<String, Value>,
        name: &str,
    ) -> Option<&'a Map<String, Value>> {
        match root.get(name)? {
            Value::Object(m) => Some(m),
            _ => {
                self.warn(name, "not a table");
                None
            }
        }
    }

    /// A non-empty string checked by `valid` (empty = unset).
    fn string(
        &mut self,
        sec: &Map<String, Value>,
        key: &str,
        field: &str,
        valid: fn(&str) -> Result<(), &'static str>,
    ) -> Option<String> {
        match sec.get(field)? {
            Value::String(s) if s.trim().is_empty() => None,
            Value::String(s) => match valid(s) {
                Ok(()) => Some(s.clone()),
                Err(why) => {
                    self.warn(key, why);
                    None
                }
            },
            _ => {
                self.warn(key, "not a string");
                None
            }
        }
    }

    fn command(&mut self, sec: &Map<String, Value>, key: &str, field: &str) -> Option<String> {
        self.string(sec, key, field, valid_command)
    }

    fn choice<T>(
        &mut self,
        sec: &Map<String, Value>,
        key: &str,
        field: &str,
        pick: fn(&str) -> Option<T>,
    ) -> Option<T> {
        let s = self.string(sec, key, field, |s| {
            if s.len() <= 64 {
                Ok(())
            } else {
                Err("too long")
            }
        })?;
        let v = pick(&s.trim().to_ascii_uppercase());
        if v.is_none() {
            self.warn(key, "unsupported value");
        }
        v
    }

    fn timeout(&mut self, sec: &Map<String, Value>) -> Option<u32> {
        let key = "deploy.healthcheckTimeout";
        let v = sec.get("healthcheckTimeout")?;
        match v.as_u64() {
            Some(t @ 1..=3600) => Some(t as u32),
            Some(_) => {
                self.warn(key, "must be 1 to 3600 seconds");
                None
            }
            None => {
                self.warn(key, "not a whole number of seconds");
                None
            }
        }
    }

    fn max_retries(&mut self, sec: &Map<String, Value>) -> Option<u32> {
        let key = "deploy.restartPolicyMaxRetries";
        match sec.get("restartPolicyMaxRetries")?.as_u64() {
            // Railway's 0 means "use the default": no limit here.
            Some(0) => None,
            Some(n @ 1..=1000) => Some(n as u32),
            _ => {
                self.warn(key, "must be a whole number from 0 to 1000");
                None
            }
        }
    }

    /// A string or a list of strings (Railway accepts both).
    fn pre_deploy(&mut self, sec: &Map<String, Value>) -> Option<Vec<String>> {
        let key = "deploy.preDeployCommand";
        let items: Vec<&Value> = match sec.get("preDeployCommand")? {
            Value::Array(a) => a.iter().collect(),
            v => vec![v],
        };
        let mut out = vec![];
        for v in items {
            match v {
                Value::String(s) if s.trim().is_empty() => {}
                Value::String(s) => match valid_command(s) {
                    Ok(()) => out.push(s.clone()),
                    Err(why) => {
                        self.warn(key, why);
                        return None;
                    }
                },
                _ => {
                    self.warn(key, "not a string or list of strings");
                    return None;
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }
}

fn valid_command(s: &str) -> Result<(), &'static str> {
    if s.len() > MAX_COMMAND {
        return Err("longer than 4096 bytes");
    }
    if s.contains('\0') {
        return Err("contains a NUL byte");
    }
    Ok(())
}

fn valid_health_path(s: &str) -> Result<(), &'static str> {
    if !s.starts_with('/') {
        return Err("must start with /");
    }
    if s.len() > MAX_HEALTH_PATH {
        return Err("longer than 512 bytes");
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("contains whitespace or control characters");
    }
    Ok(())
}

fn valid_dockerfile_path(s: &str) -> Result<(), &'static str> {
    if s.len() > MAX_DOCKERFILE_PATH || s.chars().any(char::is_control) {
        return Err("not a valid path");
    }
    let ok = Path::new(s)
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !ok {
        return Err("must be a relative path without ..");
    }
    Ok(())
}

fn one_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(file: &'static str, text: &str) -> (RepoConfig, Vec<String>) {
        let (cfg, warnings) = parse(file, text);
        (cfg.expect("parsed"), warnings)
    }

    #[test]
    fn reads_railway_json() {
        let (cfg, warnings) = parsed(
            "railway.json",
            r#"{
              "$schema": "https://railway.com/railway.schema.json",
              "build": { "builder": "dockerfile", "dockerfilePath": "docker/Dockerfile.prod", "watchPatterns": ["src/**"] },
              "deploy": { "startCommand": "node server.js", "healthcheckPath": "/health",
                          "healthcheckTimeout": 60, "restartPolicyType": "NEVER",
                          "restartPolicyMaxRetries": 3, "preDeployCommand": ["npm run migrate"] },
              "environments": { "staging": {} }
            }"#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.builder, Some(pb::Builder::Dockerfile));
        assert_eq!(
            cfg.dockerfile_path.as_deref(),
            Some("docker/Dockerfile.prod")
        );
        assert_eq!(cfg.start_command.as_deref(), Some("node server.js"));
        assert_eq!(cfg.healthcheck_path.as_deref(), Some("/health"));
        assert_eq!(cfg.healthcheck_timeout, Some(60));
        assert_eq!(cfg.restart_policy, Some(pb::RestartPolicy::Never));
        assert_eq!(
            cfg.pre_deploy_command,
            Some(vec!["npm run migrate".to_owned()])
        );
        assert_eq!(cfg.restart_max_retries, Some(3));
        // The pre-deploy command runs in a shell inside the new image.
        let rt = cfg.runtime();
        assert_eq!(
            rt.pre_deploy_command.as_deref(),
            Some(&["/bin/sh".to_owned(), "-c".into(), "npm run migrate".into()][..])
        );
        let mut w = pb::Workload {
            restart_max_retries: 7,
            ..Default::default()
        };
        rt.apply(&mut w);
        assert_eq!(w.pre_deploy_command[2], "npm run migrate");
        assert_eq!(w.restart_max_retries, 0, "the file's policy is NEVER");
        let out = rt.to_output();
        assert!(out.has_pre_deploy);
        assert_eq!(out.restart_max_retries, 3);
    }

    #[test]
    fn reads_toml_and_nixpacks_means_railpack() {
        let (cfg, warnings) = parsed(
            "railway.toml",
            "[build]\nbuilder = \"NIXPACKS\"\nbuildCommand = \"npm run build\"\n\n[deploy]\nrestartPolicyType = \"on_failure\"\npreDeployCommand = \"./migrate\"\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.builder, Some(pb::Builder::Railpack));
        assert_eq!(cfg.build_command.as_deref(), Some("npm run build"));
        assert_eq!(cfg.restart_policy, Some(pb::RestartPolicy::OnFailure));
        assert_eq!(cfg.pre_deploy_command, Some(vec!["./migrate".to_owned()]));
        assert_eq!(cfg.start_command, None);
    }

    #[test]
    fn invalid_values_are_ignored_with_a_warning() {
        let (cfg, warnings) = parsed(
            "modula.toml",
            "[build]\nbuilder = \"HEROKU\"\ndockerfilePath = \"../../etc/passwd\"\nbuildCommand = \"a\\u0000b\"\n\
             [deploy]\nstartCommand = 42\nhealthcheckPath = \"health check\"\nhealthcheckTimeout = 0\n\
             restartPolicyType = \"SOMETIMES\"\nprivileged = true\n",
        );
        assert_eq!(
            cfg,
            RepoConfig {
                file: "modula.toml",
                ..Default::default()
            }
        );
        // One per invalid key; the unknown key is silently ignored.
        assert_eq!(warnings.len(), 7, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .all(|w| w.starts_with("modula.toml: ignoring "))
        );

        let (cfg, warnings) = parsed(
            "railway.json",
            r#"{"deploy": {"healthcheckPath": "health", "healthcheckTimeout": 3601}}"#,
        );
        assert_eq!(
            (cfg.healthcheck_path, cfg.healthcheck_timeout),
            (None, None)
        );
        assert_eq!(warnings.len(), 2);
        let (cfg, _) = parsed(
            "railway.json",
            r#"{"build": {"dockerfilePath": "/Dockerfile"}}"#,
        );
        assert_eq!(cfg.dockerfile_path, None);
    }

    #[test]
    fn unparseable_files_are_ignored() {
        for (file, text) in [
            ("railway.json", "{nope"),
            ("railway.toml", "[build"),
            ("railway.json", "[1]"),
        ] {
            let (cfg, warnings) = parse(file, text);
            assert!(cfg.is_none());
            assert_eq!(warnings.len(), 1);
        }
        let (cfg, warnings) = parse("railway.json", r#"{"build": "x", "deploy": {}}"#);
        assert_eq!(cfg.unwrap().summary(), None);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn precedence_and_size_cap() {
        let dir = std::env::temp_dir().join(format!("modula-repo-config-{}", modula_id::Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load(&dir), (None, vec![]));
        std::fs::write(
            dir.join("railway.json"),
            r#"{"deploy": {"startCommand": "json"}}"#,
        )
        .unwrap();
        assert_eq!(load(&dir).0.unwrap().start_command.as_deref(), Some("json"));
        std::fs::write(
            dir.join("railway.toml"),
            "[deploy]\nstartCommand = \"railway\"\n",
        )
        .unwrap();
        assert_eq!(
            load(&dir).0.unwrap().start_command.as_deref(),
            Some("railway")
        );
        std::fs::write(
            dir.join("modula.toml"),
            "[deploy]\nstartCommand = \"modula\"\n",
        )
        .unwrap();
        let cfg = load(&dir).0.unwrap();
        assert_eq!(
            (cfg.file, cfg.start_command.as_deref()),
            ("modula.toml", Some("modula"))
        );
        // A project file (modula config) is skipped.
        std::fs::write(
            dir.join("modula.toml"),
            "project = \"shop\"\n[services.api]\nport = 80\n",
        )
        .unwrap();
        let cfg = load(&dir).0.unwrap();
        assert_eq!(
            (cfg.file, cfg.start_command.as_deref()),
            ("railway.toml", Some("railway"))
        );
        std::fs::write(dir.join("modula.toml"), "#".repeat(65 * 1024)).unwrap();
        let (cfg, warnings) = load(&dir);
        assert!(cfg.is_none() && warnings[0].contains("larger than"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn overrides_apply_over_the_dashboard() {
        let (cfg, _) = parsed(
            "railway.toml",
            "[build]\nbuilder = \"DOCKERFILE\"\n[deploy]\nstartCommand = \"npm start\"\nhealthcheckPath = \"/up\"\nrestartPolicyType = \"ALWAYS\"\n",
        );
        let mut spec = pb::Build {
            build_command: "make".into(),
            ..Default::default()
        };
        cfg.apply_build(&mut spec);
        assert_eq!(spec.builder, pb::Builder::Dockerfile as i32);
        assert_eq!(spec.build_command, "make");
        let mut rt = cfg.runtime();
        rt.command = effective_command(&spec, Some(&cfg)).map(|(argv, _)| argv);
        assert_eq!(rt.command, Some(container_shell("npm start")));

        let mut w = pb::Workload {
            command: vec!["old".into()],
            health: Some(pb::HealthCheck {
                path: "/old".into(),
                timeout_secs: 0,
            }),
            ..Default::default()
        };
        rt.apply(&mut w);
        assert_eq!(w.command, container_shell("npm start"));
        assert_eq!(w.command.last().map(String::as_str), Some("npm start"));
        assert_eq!(
            w.health,
            Some(pb::HealthCheck {
                path: "/up".into(),
                timeout_secs: DEFAULT_HEALTH_TIMEOUT
            })
        );
        assert_eq!(w.restart, pb::RestartPolicy::Always as i32);
        assert_eq!(
            cfg.summary().as_deref(),
            Some(
                "Using railway.toml: builder Dockerfile, start command, health check /up, restart policy ALWAYS"
            )
        );

        // Railpack: the start command is baked in, so a dashboard argv is dropped.
        spec.builder = pb::Builder::Railpack as i32;
        assert_eq!(
            effective_command(&spec, Some(&cfg)),
            Some((vec![], String::new()))
        );
        assert_eq!(
            effective_command(&spec, None),
            Some((vec![], String::new()))
        );
    }

    #[test]
    fn a_dockerfile_image_runs_its_cmd_unless_the_user_set_a_start_command() {
        let dockerfile = |detected| pb::Build {
            builder: pb::Builder::Dockerfile as i32,
            start_command: "pnpm --filter @a/api start".into(),
            start_command_detected: detected,
            ..Default::default()
        };
        // Detected for Railpack (a monorepo app): the image's CMD runs.
        assert_eq!(
            effective_command(&dockerfile(true), None),
            Some((vec![], String::new()))
        );
        // Set in the dashboard: its argv (already on the workload) runs.
        assert_eq!(effective_command(&dockerfile(false), None), None);
        // A config file's start command wins either way.
        let (cfg, _) = parsed("modula.toml", "[deploy]\nstartCommand = \"bun start\"\n");
        let mut spec = dockerfile(true);
        cfg.apply_build(&mut spec);
        assert_eq!(
            effective_command(&spec, Some(&cfg)),
            Some((container_shell("bun start"), "modula.toml".to_owned()))
        );
    }

    #[test]
    fn auto_picks_the_dockerfile_when_the_build_root_has_one() {
        let dir = std::env::temp_dir().join(format!("modula-auto-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("apps/api")).unwrap();
        let dir = dir.canonicalize().unwrap();
        assert_eq!(resolve_auto(&dir, "").0, pb::Builder::Railpack);
        std::fs::write(dir.join("Dockerfile"), "FROM scratch\n").unwrap();
        // A .dockerignore hiding package.json doesn't matter: the file is there.
        std::fs::write(dir.join(".dockerignore"), "*\n!apps/api/**\n").unwrap();
        assert_eq!(resolve_auto(&dir, "").0, pb::Builder::Dockerfile);
        assert_eq!(
            resolve_auto(&dir, "apps/api/Dockerfile").0,
            pb::Builder::Railpack
        );
        std::fs::write(dir.join("apps/api/Dockerfile"), "FROM scratch\n").unwrap();
        assert_eq!(
            resolve_auto(&dir, "apps/api/Dockerfile").0,
            pb::Builder::Dockerfile
        );
        // The reported repo: no config file, so Auto builds the Dockerfile
        // and the image's CMD runs, even with a start command detected for
        // Railpack on the service.
        assert_eq!(load(&dir).0, None);
        let (builder, _) = resolve_auto(&dir, "");
        let spec = pb::Build {
            builder: builder as i32,
            start_command: "pnpm --filter @a/api start".into(),
            start_command_detected: true,
            ..Default::default()
        };
        let mut w = pb::Workload {
            command: vec![
                "pnpm".into(),
                "--filter".into(),
                "@a/api".into(),
                "start".into(),
            ],
            command_source: "the dashboard".into(),
            ..Default::default()
        };
        let (argv, source) = effective_command(&spec, None).unwrap();
        RuntimeOverrides {
            command: Some(argv),
            command_source: Some(source),
            ..Default::default()
        }
        .apply(&mut w);
        assert!(w.command.is_empty() && w.command_source.is_empty(), "{w:?}");
        // Outside the build root doesn't count.
        assert_eq!(
            resolve_auto(&dir.join("apps/api"), "../../Dockerfile").0,
            pb::Builder::Railpack
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
