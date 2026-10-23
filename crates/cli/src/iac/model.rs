//! `modula.toml` (or `modula.json`): one project as code. Parsing goes
//! through a generic value first so environment overlays can be deep-merged
//! (see [`super::merge`]) before the strict, typed model rejects unknown keys.
//!
//! Service settings use the server's `ServiceConfig` keys exactly
//! ([`CONFIG_KEYS`]); a test in the server crate keeps the two in step.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// Candidate files at the repository root, highest precedence first.
pub const FILES: &[&str] = &["modula.toml", "modula.json"];

/// The server's `ServiceConfig` keys (minus `source`, which has its own
/// shape here). Keep in step with crates/server/src/deploy/config.rs.
pub const CONFIG_KEYS: &[&str] = &[
    "startCommand",
    "port",
    "healthcheckPath",
    "healthcheckTimeoutSecs",
    "restartPolicy",
    "restartPolicyMaxRetries",
    "preDeployCommand",
    "drainingSeconds",
    "memoryMb",
    "cpuMillis",
    "replicas",
    "builder",
    "dockerfilePath",
    "buildCommand",
    "autoDeploy",
    "watchPaths",
];

/// Keys of a service table that aren't settings.
pub const SERVICE_KEYS: &[&str] = &[
    "source",
    "template",
    "domains",
    "generateDomain",
    "volume",
    "tcpProxy",
    "variables",
    "preserve",
];

/// The file, resolved for one environment (overlay merged, no `environments`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectSpec {
    /// Project name or id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "SharedSpec::is_empty")]
    pub shared: SharedSpec,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceSpec>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharedSpec {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, VarSpec>,
    /// Remote-only variables `apply` must not delete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preserve: Vec<String>,
}

impl SharedSpec {
    pub fn is_empty(&self) -> bool {
        self.variables.is_empty() && self.preserve.is_empty()
    }
}

/// One service in one environment. Settings left out take the server's
/// defaults (or, for a template service, the template's).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceSpec>,
    /// A database template (`postgres`, `mysql`, `redis`, `mongo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck_timeout_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<RestartPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_policy_max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_deploy_command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draining_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millis: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builder: Option<Builder>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_deploy: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_paths: Option<Vec<String>>,

    /// Custom domains (generated ones are never deleted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<Vec<DomainSpec>>,
    /// Make sure the service has a generated domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_domain: Option<bool>,
    /// The volume's mount path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    /// Expose this port through the TCP proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_proxy: Option<u16>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, VarSpec>,
    /// Remote-only variables `apply` must not delete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preserve: Vec<String>,
}

/// Where the code comes from: exactly one of `repo`, `image` or `local`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SourceSpec {
    Repo(RepoSource),
    Image(ImageSource),
    Local(LocalSource),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepoSource {
    /// `owner/name` on GitHub.
    pub repo: String,
    #[serde(default = "main_branch")]
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_dir: Option<String>,
}

fn main_branch() -> String {
    "main".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageSource {
    pub image: String,
}

/// A directory deployed with `modula up` (which builds an upload
/// with the service's settings); config never touches the service's
/// configured source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSource {
    pub local: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    OnFailure,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Builder {
    Railpack,
    Dockerfile,
}

/// `"api.example.com"` or `{ host = "api.example.com", port = 3000 }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DomainSpec {
    Host(String),
    WithPort {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
    },
}

impl DomainSpec {
    pub fn host(&self) -> String {
        match self {
            Self::Host(h) | Self::WithPort { host: h, .. } => h.trim().to_ascii_lowercase(),
        }
    }

    pub fn port(&self) -> Option<u16> {
        match self {
            Self::Host(_) => None,
            Self::WithPort { port, .. } => *port,
        }
    }
}

/// A variable: a value (strings, numbers and booleans; references like
/// `${{postgres.DATABASE_URL}}` are kept as written), or a secret whose value
/// stays out of the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VarSpec {
    Value(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    Secret(SecretSpec),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSpec {
    pub secret: bool,
    /// Store it sealed: write-only forever (never revealable or compared).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sealed: bool,
}

impl VarSpec {
    pub fn secret(sealed: bool) -> Self {
        Self::Secret(SecretSpec {
            secret: true,
            sealed,
        })
    }

    /// The literal value, unless it is a secret.
    pub fn literal(&self) -> Option<String> {
        match self {
            Self::Value(v) => Some(v.clone()),
            Self::Integer(n) => Some(n.to_string()),
            Self::Float(f) => Some(f.to_string()),
            Self::Bool(b) => Some(b.to_string()),
            Self::Secret(_) => None,
        }
    }
}

/// The parsed file: the base and the raw overlay of each environment.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    pub path: PathBuf,
    /// Everything but `environments`.
    pub base: toml::Table,
    /// `[environments.<name>]` tables, as written.
    pub environments: BTreeMap<String, toml::Table>,
}

impl ConfigFile {
    /// The directory holding the file (the repository root).
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// The project named in the file.
    pub fn project(&self) -> Option<&str> {
        self.base.get("project").and_then(toml::Value::as_str)
    }

    /// Parse `text` (TOML, or JSON when `path` ends in .json).
    pub fn parse(path: &Path, text: &str) -> anyhow::Result<Self> {
        let mut root: toml::Table = if path.extension().is_some_and(|e| e == "json") {
            let json: serde_json::Value = serde_json::from_str(text)
                .with_context(|| format!("{} is invalid", path.display()))?;
            toml::Table::try_from(json)
                .with_context(|| format!("{}: null values aren't allowed", path.display()))?
        } else {
            toml::from_str(text).with_context(|| format!("{} is invalid", path.display()))?
        };
        let mut environments = BTreeMap::new();
        match root.remove("environments") {
            None => {}
            Some(toml::Value::Table(t)) => {
                for (name, overlay) in t {
                    let toml::Value::Table(overlay) = overlay else {
                        bail!("environments.{name} must be a table");
                    };
                    for key in overlay.keys() {
                        if !matches!(key.as_str(), "services" | "shared") {
                            bail!(
                                "environments.{name}: unknown key `{key}` (an environment can override `services` and `shared`)"
                            );
                        }
                    }
                    environments.insert(name.to_ascii_lowercase(), overlay);
                }
            }
            Some(_) => bail!("`environments` must be a table"),
        }
        let file = Self {
            path: path.to_owned(),
            base: root,
            environments,
        };
        // Catch mistakes early: the base and every environment must resolve.
        file.resolve(None)?;
        for name in file.environments.keys() {
            file.resolve(Some(name))?;
        }
        Ok(file)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(path, &text)
    }

    /// The nearest `modula.toml`/`modula.json` at or above `start`.
    pub fn find(start: &Path) -> Option<PathBuf> {
        start
            .ancestors()
            .flat_map(|dir| FILES.iter().map(move |f| dir.join(f)))
            .find(|p| p.is_file())
    }

    /// The spec for `environment` (None: the base alone).
    pub fn resolve(&self, environment: Option<&str>) -> anyhow::Result<ProjectSpec> {
        let mut merged = self.base.clone();
        let label = match environment {
            Some(env) => {
                if let Some(overlay) = self.environments.get(&env.to_ascii_lowercase()) {
                    super::merge::deep_merge(&mut merged, overlay.clone());
                }
                format!("{} (environment {env})", self.path.display())
            }
            None => self.path.display().to_string(),
        };
        parse_spec(merged).with_context(|| label)
    }
}

/// The typed spec of a merged table, with helpful errors for unknown keys.
pub fn parse_spec(mut table: toml::Table) -> anyhow::Result<ProjectSpec> {
    for key in table.keys() {
        if !matches!(key.as_str(), "project" | "services" | "shared") {
            bail!(
                "unknown top-level key `{key}` (expected project, services, shared, environments)"
            );
        }
    }
    let mut services = BTreeMap::new();
    if let Some(raw) = table.remove("services") {
        let toml::Value::Table(raw) = raw else {
            bail!("`services` must be a table of services");
        };
        for (name, value) in raw {
            validate_service_name(&name)?;
            let toml::Value::Table(t) = value else {
                bail!("services.{name} must be a table");
            };
            check_service_keys(&name, &t)?;
            let spec: ServiceSpec = toml::Value::Table(t)
                .try_into()
                .with_context(|| format!("services.{name} is invalid"))?;
            spec.validate(&name)?;
            services.insert(name, spec);
        }
    }
    let mut spec: ProjectSpec = toml::Value::Table(table)
        .try_into()
        .context("invalid project settings")?;
    for name in spec.shared.variables.keys() {
        validate_var_name(name).with_context(|| format!("shared.variables.{name}"))?;
    }
    spec.services = services;
    Ok(spec)
}

fn check_service_keys(service: &str, t: &toml::Table) -> anyhow::Result<()> {
    for key in t.keys() {
        if CONFIG_KEYS.contains(&key.as_str()) || SERVICE_KEYS.contains(&key.as_str()) {
            continue;
        }
        let camel = to_camel(key);
        let hint = if camel != *key
            && (CONFIG_KEYS.contains(&camel.as_str()) || SERVICE_KEYS.contains(&camel.as_str()))
        {
            format!(" (did you mean `{camel}`?)")
        } else {
            format!(
                " (settings: {}; also {})",
                CONFIG_KEYS.join(", "),
                SERVICE_KEYS.join(", ")
            )
        };
        bail!("services.{service}: unknown key `{key}`{hint}");
    }
    Ok(())
}

fn to_camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper = false;
    for c in key.chars() {
        if c == '_' || c == '-' {
            upper = true;
        } else if upper {
            out.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// The server's rule: lowercase letters, digits and -, starting with a letter.
pub fn validate_service_name(name: &str) -> anyhow::Result<()> {
    let b = name.as_bytes();
    let ok = (1..=63).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-');
    if !ok {
        bail!(
            "services.{name}: service names use lowercase letters, digits and -, start with a letter and are at most 63 characters"
        );
    }
    Ok(())
}

/// The server's rule for variable names.
pub fn validate_var_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 255
        && !name.as_bytes()[0].is_ascii_digit()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !ok {
        bail!(
            "invalid variable name {name:?}: use letters, digits and _ (not starting with a digit)"
        );
    }
    if name.starts_with("MODULA_") {
        bail!("{name}: MODULA_ variables are set by the platform");
    }
    Ok(())
}

impl ServiceSpec {
    fn validate(&self, name: &str) -> anyhow::Result<()> {
        if self.source.is_none() && self.template.is_none() {
            bail!(
                "services.{name}: needs a `source` ({{ repo = … }}, {{ image = … }} or {{ local = \".\" }}) or a `template`"
            );
        }
        if let Some(t) = &self.template
            && !matches!(t.as_str(), "postgres" | "mysql" | "redis" | "mongo")
        {
            bail!("services.{name}: unknown template {t:?} (postgres, mysql, redis or mongo)");
        }
        for (var, spec) in &self.variables {
            validate_var_name(var).with_context(|| format!("services.{name}.variables.{var}"))?;
            if let VarSpec::Secret(s) = spec
                && !s.secret
            {
                bail!("services.{name}.variables.{var}: use a value, or {{ secret = true }}");
            }
        }
        if let Some(domains) = &self.domains {
            let mut seen = std::collections::BTreeSet::new();
            for d in domains {
                if d.host().is_empty() || !seen.insert(d.host()) {
                    bail!(
                        "services.{name}.domains: empty or repeated host {:?}",
                        d.host()
                    );
                }
            }
        }
        if let Some(v) = &self.volume
            && !v.starts_with('/')
        {
            bail!("services.{name}.volume: the mount path must be absolute, e.g. /data");
        }
        Ok(())
    }

    pub fn is_local(&self) -> bool {
        matches!(self.source, Some(SourceSpec::Local(_)))
    }

    /// The settings this spec sets, as the server's `ServiceConfig` JSON keys
    /// (`source` included unless it is local).
    pub fn settings(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut all = match serde_json::to_value(self) {
            Ok(serde_json::Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        all.retain(|k, _| CONFIG_KEYS.contains(&k.as_str()));
        match &self.source {
            Some(SourceSpec::Repo(r)) => {
                let mut s =
                    serde_json::json!({ "type": "repo", "repo": r.repo, "branch": r.branch });
                if let Some(d) = &r.root_dir {
                    s["rootDir"] = d.clone().into();
                }
                all.insert("source".into(), s);
            }
            Some(SourceSpec::Image(i)) => {
                all.insert(
                    "source".into(),
                    serde_json::json!({ "type": "image", "image": i.image }),
                );
            }
            Some(SourceSpec::Local(_)) | None => {}
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
project = "anton"

[services.api]
source = { repo = "acme/anton", branch = "main" }
builder = "dockerfile"
dockerfilePath = "Dockerfile"
preDeployCommand = ["sh", "apps/api/scripts/predeploy.sh"]
healthcheckPath = "/v1/ready"
healthcheckTimeoutSecs = 120
replicas = 1
drainingSeconds = 30
restartPolicy = "on_failure"
restartPolicyMaxRetries = 5
domains = ["api.example.com", { host = "admin.example.com", port = 9000 }]
preserve = ["LEGACY"]

[services.api.variables]
NODE_ENV = "production"
WORKERS = 4
DEBUG = false
DATABASE_URL = "${{postgres.DATABASE_URL}}?schema=public"
AUTH_SECRET = { secret = true }

[services.postgres]
template = "postgres"

[shared.variables]
REGION = "eu"

[environments.staging]
services.api.source = { repo = "acme/anton", branch = "staging" }
services.api.domains = []
services.api.variables.AUTH_URL = "https://staging.example.com"
"#;

    fn parse(text: &str) -> anyhow::Result<ConfigFile> {
        ConfigFile::parse(Path::new("modula.toml"), text)
    }

    #[test]
    fn the_example_parses() {
        let f = parse(EXAMPLE).unwrap();
        assert_eq!(f.project(), Some("anton"));
        let base = f.resolve(None).unwrap();
        let api = &base.services["api"];
        assert_eq!(api.builder, Some(Builder::Dockerfile));
        assert_eq!(api.restart_policy, Some(RestartPolicy::OnFailure));
        assert_eq!(api.domains.as_ref().unwrap()[1].port(), Some(9000));
        assert_eq!(api.variables["WORKERS"].literal().as_deref(), Some("4"));
        assert_eq!(api.variables["DEBUG"].literal().as_deref(), Some("false"));
        assert_eq!(api.variables["AUTH_SECRET"], VarSpec::secret(false));
        assert_eq!(
            base.services["postgres"].template.as_deref(),
            Some("postgres")
        );
        assert_eq!(
            base.shared.variables["REGION"].literal().as_deref(),
            Some("eu")
        );

        // Settings come out as the server's ServiceConfig JSON.
        let s = api.settings();
        assert_eq!(s["source"]["type"], "repo");
        assert_eq!(s["source"]["branch"], "main");
        assert_eq!(s["healthcheckTimeoutSecs"], 120);
        assert_eq!(s["preDeployCommand"][0], "sh");
        assert!(!s.contains_key("domains") && !s.contains_key("variables"));
    }

    #[test]
    fn json_files_are_accepted() {
        let f = ConfigFile::parse(
            Path::new("modula.json"),
            r#"{ "project": "p", "services": { "web": { "source": { "image": "nginx" }, "port": 80 } },
                 "environments": { "staging": { "services": { "web": { "port": 81 } } } } }"#,
        )
        .unwrap();
        assert_eq!(
            f.resolve(Some("staging")).unwrap().services["web"].port,
            Some(81)
        );
        assert!(ConfigFile::parse(Path::new("modula.json"), r#"{ "project": null }"#).is_err());
    }

    #[test]
    fn unknown_keys_are_rejected_helpfully() {
        let err = |text: &str| format!("{:#}", parse(text).unwrap_err());
        let e = err("[services.api]\nsource = { image = \"nginx\" }\nhealthcheck_path = \"/\"\n");
        assert!(e.contains("did you mean `healthcheckPath`"), "{e}");
        let e = err("[services.api]\nsource = { image = \"nginx\" }\nprivileged = true\n");
        assert!(
            e.contains("unknown key `privileged`") && e.contains("startCommand"),
            "{e}"
        );
        let e =
            err("[services.api]\nsource = { image = \"nginx\" }\nrestartPolicy = \"sometimes\"\n");
        assert!(e.contains("services.api is invalid"), "{e}");
        // Overlays are checked too, after merging.
        let e = err(
            "[services.api]\nsource = { image = \"nginx\" }\n[environments.dev]\nservices.api.prot = 1\n",
        );
        assert!(e.contains("environment dev") && e.contains("`prot`"), "{e}");
        let e = err("[environments.dev]\nproject = \"x\"\n");
        assert!(e.contains("environments.dev: unknown key `project`"), "{e}");
        assert!(err("bogus = 1\n").contains("unknown top-level key `bogus`"));
        assert!(err("[services.Api]\ntemplate = \"postgres\"\n").contains("lowercase"));
        assert!(err("[services.api]\nport = 80\n").contains("needs a `source`"));
        assert!(err("[services.db]\ntemplate = \"oracle\"\n").contains("unknown template"));
        let e = err("[services.api]\nsource = { image = \"x\", repo = \"a/b\" }\n");
        assert!(e.contains("services.api is invalid"), "{e}");
        let e = err("[services.api]\nsource = { image = \"x\" }\nvariables.MODULA_X = \"1\"\n");
        assert!(e.contains("platform"), "{e}");
        let e =
            err("[services.api]\nsource = { image = \"x\" }\nvariables.A = { secret = false }\n");
        assert!(e.contains("secret = true"), "{e}");
    }

    #[test]
    fn overlays_deep_merge_over_the_base() {
        let f = parse(EXAMPLE).unwrap();
        let staging = f.resolve(Some("staging")).unwrap();
        let api = &staging.services["api"];
        match &api.source {
            Some(SourceSpec::Repo(r)) => assert_eq!(r.branch, "staging"),
            other => panic!("{other:?}"),
        }
        // Arrays replace; tables merge (base variables stay).
        assert_eq!(api.domains.as_deref(), Some(&[][..]));
        assert_eq!(
            api.variables["NODE_ENV"].literal().as_deref(),
            Some("production")
        );
        assert!(api.variables.contains_key("AUTH_URL"));
        assert_eq!(api.healthcheck_path.as_deref(), Some("/v1/ready"));
        // Environments without an overlay get the base; names are case-insensitive.
        assert_eq!(
            f.resolve(Some("production")).unwrap(),
            f.resolve(None).unwrap()
        );
        assert_eq!(f.resolve(Some("Staging")).unwrap(), staging);
    }
}
