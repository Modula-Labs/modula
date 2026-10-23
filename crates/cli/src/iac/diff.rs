//! The plan: what `apply` would change to make one environment match the
//! file. Pure (no I/O), so it is tested without a server.
//!
//! Rules:
//! - Settings left out of the file take the server's defaults (or, for a
//!   template service, the template's), so removing a line resets it.
//! - Only custom domains are managed; generated ones are never deleted.
//! - Variables not in the file are deleted unless listed in `preserve` (or
//!   created by the service's template). A service (or `shared`) that lists
//!   no variables and no `preserve` leaves its variables alone: they are
//!   managed elsewhere (the dashboard, `modula variables`). A `{ secret = true }` variable needs
//!   a value from the secrets file or the environment unless it exists.
//! - Sealed values can't be compared: they are only set when missing (or with
//!   `--update-sealed`).
//! - Some things need a re-authenticated dashboard session (deleting a
//!   service or a volume); the plan lists them as manual steps.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::model::{ProjectSpec, ServiceSpec, VarSpec};
use super::remote::{RemoteEnv, RemoteService, RemoteVar};

/// A value that must never be printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Where `{ secret = true }` values come from.
pub trait SecretSource {
    fn get(&self, name: &str) -> Option<String>;
}

impl SecretSource for BTreeMap<String, String> {
    fn get(&self, name: &str) -> Option<String> {
        BTreeMap::get(self, name).cloned()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Overwrite sealed variables with the local secret value.
    pub update_sealed: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvPlan {
    pub environment: String,
    /// The environment doesn't exist yet; `apply` creates it.
    pub create_environment: bool,
    pub shared: Vec<VarChange>,
    pub services: Vec<ServicePlan>,
    /// Changes `apply` can't make with an API token.
    pub manual: Vec<String>,
    pub warnings: Vec<String>,
    /// Problems that stop `apply` (e.g. a secret without a value).
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAction {
    Create,
    Update,
    Unchanged,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicePlan {
    pub service: String,
    pub action: ServiceAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(skip)]
    pub instance_id: Option<String>,
    #[serde(skip)]
    pub service_id: Option<String>,
    pub settings: Vec<SettingChange>,
    /// The whole desired config (sent when a setting changes or the
    /// service is created). No secrets: variables are separate.
    pub config: Value,
    pub variables: Vec<VarChange>,
    pub domains: Vec<DomainChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    pub tcp_proxy: Vec<TcpProxyChange>,
}

impl ServicePlan {
    /// Whether the running deployment is out of date afterwards.
    pub fn needs_deploy(&self) -> bool {
        !self.settings.is_empty()
            || !self.variables.is_empty()
            || self.volume.is_some()
            || !self.tcp_proxy.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.action == ServiceAction::Unchanged && !self.needs_deploy() && self.domains.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingChange {
    pub key: String,
    /// Null = the default.
    pub from: Value,
    pub to: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VarOp {
    Add,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VarChange {
    pub name: String,
    pub op: VarOp,
    /// The value is a secret: never shown.
    pub secret: bool,
    /// Stored sealed.
    pub sealed: bool,
    /// Old and new values, for non-secrets only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// What `apply` sends (None for deletes).
    #[serde(skip)]
    pub value: Option<Secret>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "op")]
pub enum DomainChange {
    Add {
        host: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
    },
    Update {
        host: String,
        #[serde(skip)]
        id: String,
        from: Option<u16>,
        to: Option<u16>,
        #[serde(skip)]
        rate_limit: Option<u32>,
    },
    Delete {
        host: String,
        #[serde(skip)]
        id: String,
    },
    Generate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "op")]
pub enum TcpProxyChange {
    Add {
        port: u16,
    },
    Delete {
        port: u16,
        #[serde(skip)]
        id: String,
    },
}

impl EnvPlan {
    pub fn is_empty(&self) -> bool {
        !self.create_environment
            && self.shared.is_empty()
            && self.services.iter().all(ServicePlan::is_empty)
    }

    /// Counts for the summary line: (to add, to change, to delete).
    pub fn counts(&self) -> (usize, usize, usize) {
        let (mut add, mut change, mut delete) = (0, 0, 0);
        let mut var = |v: &VarChange| match v.op {
            VarOp::Add => add += 1,
            VarOp::Update => change += 1,
            VarOp::Delete => delete += 1,
        };
        self.shared.iter().for_each(&mut var);
        for s in &self.services {
            s.variables.iter().for_each(&mut var);
        }
        for s in &self.services {
            match s.action {
                ServiceAction::Create => add += 1,
                ServiceAction::Update | ServiceAction::Unchanged => {
                    change += s.settings.len();
                }
            }
            add += usize::from(s.volume.is_some());
            for d in &s.domains {
                match d {
                    DomainChange::Add { .. } | DomainChange::Generate => add += 1,
                    DomainChange::Update { .. } => change += 1,
                    DomainChange::Delete { .. } => delete += 1,
                }
            }
            for t in &s.tcp_proxy {
                match t {
                    TcpProxyChange::Add { .. } => add += 1,
                    TcpProxyChange::Delete { .. } => delete += 1,
                }
            }
        }
        add += usize::from(self.create_environment);
        (add, change, delete)
    }
}

// ------------------------------------------------------------------ settings

/// Server defaults of `ServiceConfig` (crates/server/src/deploy/config.rs).
fn defaults() -> Map<String, Value> {
    let Value::Object(m) = serde_json::json!({
        "port": 8080,
        "healthcheckTimeoutSecs": 300,
        "restartPolicy": "on_failure",
        "replicas": 1,
        "builder": "railpack",
        "autoDeploy": true,
        "drainingSeconds": 0,
    }) else {
        unreachable!()
    };
    m
}

/// A config without its defaults (and without nulls or empty lists), so two
/// configs compare equal when they behave the same.
pub fn normalize(config: &Value) -> Map<String, Value> {
    let defaults = defaults();
    let mut out = config.as_object().cloned().unwrap_or_default();
    out.retain(|k, v| {
        !(v.is_null() || v.as_array().is_some_and(Vec::is_empty) || defaults.get(k) == Some(v))
    });
    out
}

/// The config `spec` asks for: the template's (or the defaults) with the
/// file's settings on top. A local or omitted source keeps the current one;
/// a new service with a local source is created as one (`modula up` deploys it).
/// A service created from a template keeps the template's settings as its
/// base even when the file doesn't name the template.
pub fn desired_config(spec: &ServiceSpec, remote: Option<&RemoteService>) -> Value {
    let mut config = match remote.and_then(|r| r.template_config.as_ref()) {
        Some(Value::Object(t)) => t.clone(),
        _ => Map::new(),
    };
    config.extend(spec.settings());
    if !config.contains_key("source")
        && let Some(source) = remote.and_then(|r| r.config.get("source"))
    {
        config.insert("source".into(), source.clone());
    }
    if remote.is_none() && spec.is_local() {
        config.insert("source".into(), json!({ "type": "local" }));
    }
    Value::Object(config)
}

fn setting_changes(desired: &Value, current: &Value) -> Vec<SettingChange> {
    let (want, have) = (normalize(desired), normalize(current));
    let defaults = defaults();
    let keys: BTreeSet<&String> = want.keys().chain(have.keys()).collect();
    keys.into_iter()
        .filter(|k| want.get(*k) != have.get(*k))
        .map(|k| {
            let shown = |m: &Map<String, Value>| {
                m.get(k).or(defaults.get(k)).cloned().unwrap_or(Value::Null)
            };
            SettingChange {
                key: k.clone(),
                from: shown(&have),
                to: shown(&want),
            }
        })
        .collect()
}

// ----------------------------------------------------------------- variables

/// Names that look like credentials (for `pull`'s `{ secret = true }`).
pub fn looks_secret(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    [
        "PASSWORD",
        "SECRET",
        "TOKEN",
        "KEY",
        "PRIVATE",
        "CREDENTIAL",
        "PASSWD",
    ]
    .iter()
    .any(|w| n.contains(w))
}

/// Variables of one scope (a service, or the shared ones).
pub struct VarScope<'a> {
    pub label: &'a str,
    pub desired: &'a BTreeMap<String, VarSpec>,
    pub preserve: &'a [String],
    /// Managed elsewhere (a template's variables), unless listed.
    pub unmanaged: &'a [String],
    pub remote: &'a [RemoteVar],
}

#[derive(Default)]
struct Notes {
    warnings: Vec<String>,
    errors: Vec<String>,
}

fn diff_variables(
    scope: &VarScope<'_>,
    secrets: &dyn SecretSource,
    opts: Options,
    notes: &mut Notes,
) -> Vec<VarChange> {
    let remote: BTreeMap<&str, &RemoteVar> =
        scope.remote.iter().map(|v| (v.name.as_str(), v)).collect();
    let mut out = Vec::new();
    for (name, spec) in scope.desired {
        let have = remote.get(name.as_str()).copied();
        let change = |op, secret, sealed, from, to, value: String| VarChange {
            name: name.clone(),
            op,
            secret,
            sealed,
            from,
            to,
            value: Some(Secret(value)),
        };
        match (spec.literal(), have) {
            // A plain value.
            (Some(want), None) => out.push(change(
                VarOp::Add,
                false,
                false,
                None,
                Some(want.clone()),
                want,
            )),
            (Some(want), Some(h)) if h.sealed => {
                if opts.update_sealed {
                    out.push(change(VarOp::Update, true, true, None, None, want));
                } else {
                    notes.warnings.push(format!(
                        "{}: {name} is sealed on the server, so its value can't be compared; \
                         write it as {{ secret = true, sealed = true }} (or pass --update-sealed)",
                        scope.label
                    ));
                }
            }
            (Some(want), Some(h)) => {
                if h.value.as_deref() != Some(want.as_str()) {
                    out.push(change(
                        VarOp::Update,
                        false,
                        false,
                        h.value.clone(),
                        Some(want.clone()),
                        want,
                    ));
                }
            }
            // A secret.
            (None, have) => {
                let VarSpec::Secret(s) = spec else {
                    unreachable!("literal() is None only for secrets")
                };
                let local = secrets.get(name);
                match (local, have) {
                    (None, None) => notes.errors.push(format!(
                        "{}: {name} is a secret that doesn't exist yet; put it in the secrets file \
                         (.modula/secrets.<environment>.env), --secrets-file, or the environment with --from-env",
                        scope.label
                    )),
                    (None, Some(_)) => {}
                    (Some(v), None) => {
                        out.push(change(VarOp::Add, true, s.sealed, None, None, v));
                    }
                    (Some(v), Some(h)) if h.sealed => {
                        if opts.update_sealed {
                            out.push(change(VarOp::Update, true, true, None, None, v));
                        }
                    }
                    (Some(v), Some(h)) => {
                        // Same value but now sealed counts as a change too.
                        if h.value.as_deref() != Some(v.as_str()) || s.sealed {
                            out.push(change(VarOp::Update, true, s.sealed, None, None, v));
                        }
                    }
                }
            }
        }
    }
    // Nothing listed: these variables aren't managed by the file.
    let managed = !scope.desired.is_empty() || !scope.preserve.is_empty();
    for (name, h) in &remote {
        let keep = !managed
            || scope.desired.contains_key(*name)
            || scope.preserve.iter().any(|p| p == name)
            || scope.unmanaged.iter().any(|p| p == name);
        if !keep {
            out.push(VarChange {
                name: (*name).to_owned(),
                op: VarOp::Delete,
                secret: h.sealed || looks_secret(name),
                sealed: h.sealed,
                from: None,
                to: None,
                value: None,
            });
        }
    }
    out
}

// ------------------------------------------------------------------ services

fn diff_service(
    name: &str,
    spec: &ServiceSpec,
    remote: Option<&RemoteService>,
    secrets: &dyn SecretSource,
    opts: Options,
    notes: &mut Notes,
    manual: &mut Vec<String>,
) -> ServicePlan {
    let empty = RemoteService::default();
    let r = remote.unwrap_or(&empty);
    let config = desired_config(spec, remote);
    let action = if remote.is_some() {
        ServiceAction::Update
    } else {
        ServiceAction::Create
    };
    if let Some(have) = remote
        && spec.template.is_some()
        && have.template != spec.template
    {
        manual.push(format!(
            "{name}: the file says template {}, the server has {}; a service's template can't change (recreate it in the dashboard)",
            spec.template.as_deref().unwrap_or("none"),
            have.template.as_deref().unwrap_or("none"),
        ));
    }
    // New services are created with the whole config: no setting diff.
    let settings = match remote {
        Some(have) => setting_changes(&config, &have.config),
        None => Vec::new(),
    };

    let unmanaged: Vec<String> = if spec.template.is_some() || r.template.is_some() {
        r.template_variables.clone()
    } else {
        Vec::new()
    };
    let label = format!("services.{name}");
    let variables = diff_variables(
        &VarScope {
            label: &label,
            desired: &spec.variables,
            preserve: &spec.preserve,
            unmanaged: &unmanaged,
            remote: &r.variables,
        },
        secrets,
        opts,
        notes,
    );

    // Custom domains only; generated ones are left alone.
    let mut domains = Vec::new();
    let wanted: Vec<(String, Option<u16>)> = spec
        .domains
        .iter()
        .flatten()
        .map(|d| (d.host(), d.port()))
        .collect();
    let custom: Vec<_> = r.domains.iter().filter(|d| d.kind == "custom").collect();
    for d in &custom {
        match wanted.iter().find(|(h, _)| *h == d.hostname) {
            None => domains.push(DomainChange::Delete {
                host: d.hostname.clone(),
                id: d.id.clone(),
            }),
            Some((_, port)) if *port != d.target_port => domains.push(DomainChange::Update {
                host: d.hostname.clone(),
                id: d.id.clone(),
                from: d.target_port,
                to: *port,
                rate_limit: d.rate_limit_per_minute,
            }),
            Some(_) => {}
        }
    }
    for (host, port) in &wanted {
        if !custom.iter().any(|d| d.hostname == *host) {
            domains.push(DomainChange::Add {
                host: host.clone(),
                port: *port,
            });
        }
    }
    if spec.generate_domain == Some(true) && !r.domains.iter().any(|d| d.kind == "generated") {
        domains.push(DomainChange::Generate);
    }

    // A template creates its own volume; otherwise the file is the truth.
    let mut volume = None;
    let template = spec.template.is_some() || r.template.is_some();
    match (&spec.volume, &r.volume) {
        (Some(want), None) if !(template && remote.is_none()) => volume = Some(want.clone()),
        (Some(want), Some(have)) if *want != have.mount_path => manual.push(format!(
            "{name}: move the volume from {} to {want} (delete it in the dashboard first: it holds data)",
            have.mount_path
        )),
        (None, Some(have)) if !template => manual.push(format!(
            "{name}: delete the volume at {} in the dashboard (it holds data), or add `volume = \"{}\"`",
            have.mount_path, have.mount_path
        )),
        _ => {}
    }

    let mut tcp_proxy = Vec::new();
    match (spec.tcp_proxy, &r.tcp_proxy) {
        (Some(want), Some(have)) if want != have.target_port => {
            tcp_proxy.push(TcpProxyChange::Delete {
                port: have.target_port,
                id: have.id.clone(),
            });
            tcp_proxy.push(TcpProxyChange::Add { port: want });
        }
        (Some(want), None) => tcp_proxy.push(TcpProxyChange::Add { port: want }),
        (None, Some(have)) => tcp_proxy.push(TcpProxyChange::Delete {
            port: have.target_port,
            id: have.id.clone(),
        }),
        _ => {}
    }

    let mut plan = ServicePlan {
        service: name.to_owned(),
        action,
        template: spec.template.clone(),
        instance_id: remote.map(|r| r.instance_id.clone()),
        service_id: remote.map(|r| r.service_id.clone()),
        settings,
        config,
        variables,
        domains,
        volume,
        tcp_proxy,
    };
    if plan.action == ServiceAction::Update && !plan.needs_deploy() && plan.domains.is_empty() {
        plan.action = ServiceAction::Unchanged;
    }
    plan
}

/// Compare one environment's spec with what the server has (`remote` is None
/// for an environment that doesn't exist yet).
pub fn plan(
    environment: &str,
    spec: &ProjectSpec,
    remote: Option<&RemoteEnv>,
    secrets: &dyn SecretSource,
    opts: Options,
) -> EnvPlan {
    let empty = RemoteEnv::default();
    let r = remote.unwrap_or(&empty);
    let mut notes = Notes::default();
    let mut manual = Vec::new();
    let mut services = Vec::new();
    for (name, s) in &spec.services {
        let have = r.service(name);
        services.push(diff_service(
            name,
            s,
            have,
            secrets,
            opts,
            &mut notes,
            &mut manual,
        ));
    }
    for have in &r.services {
        if !spec.services.contains_key(&have.service_name) {
            manual.push(format!(
                "{}: not in the file; delete it in the dashboard (deleting needs a re-authenticated session) or add it to the file",
                have.service_name
            ));
        }
    }
    let shared = diff_variables(
        &VarScope {
            label: "shared",
            desired: &spec.shared.variables,
            preserve: &spec.shared.preserve,
            unmanaged: &[],
            remote: &r.shared,
        },
        secrets,
        opts,
        &mut notes,
    );
    EnvPlan {
        environment: environment.to_owned(),
        create_environment: remote.is_none(),
        shared,
        services,
        manual,
        warnings: notes.warnings,
        errors: notes.errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iac::model::ConfigFile;
    use crate::iac::remote::{RemoteDomain, RemoteTcpProxy, RemoteVolume};
    use serde_json::json;

    fn spec(text: &str) -> ProjectSpec {
        ConfigFile::parse(std::path::Path::new("modula.toml"), text)
            .unwrap()
            .resolve(None)
            .unwrap()
    }

    fn var(name: &str, value: Option<&str>, sealed: bool) -> RemoteVar {
        RemoteVar {
            id: format!("v-{name}"),
            name: name.into(),
            sealed,
            value: value.map(Into::into),
        }
    }

    fn api_remote() -> RemoteService {
        RemoteService {
            service_id: "s1".into(),
            service_name: "api".into(),
            instance_id: "i1".into(),
            config: json!({
                "source": { "type": "repo", "repo": "acme/api", "branch": "main" },
                "port": 8080, "healthcheckTimeoutSecs": 300, "restartPolicy": "on_failure",
                "replicas": 1, "builder": "railpack", "autoDeploy": true,
                "healthcheckPath": "/health",
            }),
            domains: vec![
                RemoteDomain {
                    id: "d1".into(),
                    hostname: "api.example.com".into(),
                    kind: "custom".into(),
                    ..Default::default()
                },
                RemoteDomain {
                    id: "d2".into(),
                    hostname: "api-x1y2.apps.example.com".into(),
                    kind: "generated".into(),
                    ..Default::default()
                },
            ],
            variables: vec![
                var("NODE_ENV", Some("production"), false),
                var("OLD", Some("x"), false),
                var("LEGACY", Some("keep"), false),
                var("SEALED", None, true),
            ],
            ..Default::default()
        }
    }

    fn remote(services: Vec<RemoteService>) -> RemoteEnv {
        RemoteEnv {
            id: "e1".into(),
            name: "production".into(),
            services,
            ..Default::default()
        }
    }

    const API: &str = r#"
[services.api]
source = { repo = "acme/api", branch = "main" }
healthcheckPath = "/health"
domains = ["api.example.com"]
preserve = ["LEGACY"]
variables = { NODE_ENV = "production", SEALED = { secret = true, sealed = true } }
"#;

    #[test]
    fn a_matching_environment_has_no_changes() {
        let r = remote(vec![api_remote()]);
        let p = plan(
            "production",
            &spec(API),
            Some(&r),
            &BTreeMap::new(),
            Options::default(),
        );
        // OLD isn't in the file: the only change.
        let api = &p.services[0];
        assert_eq!(api.variables.len(), 1, "{:?}", api.variables);
        assert_eq!(
            (api.variables[0].name.as_str(), api.variables[0].op),
            ("OLD", VarOp::Delete)
        );
        let with_old = API.replace(
            "NODE_ENV = \"production\"",
            "NODE_ENV = \"production\", OLD = \"x\"",
        );
        let p = plan(
            "production",
            &spec(&with_old),
            Some(&r),
            &BTreeMap::new(),
            Options::default(),
        );
        assert!(p.is_empty(), "{p:?}");
        assert!(
            p.errors.is_empty() && p.manual.is_empty() && p.warnings.is_empty(),
            "{p:?}"
        );
        assert_eq!(p.services[0].action, ServiceAction::Unchanged);
    }

    #[test]
    fn settings_variables_and_domains_are_diffed() {
        let r = remote(vec![api_remote()]);
        let text = r#"
[services.api]
source = { repo = "acme/api", branch = "dev" }
port = 3000
domains = [{ host = "API.example.com", port = 9000 }, "www.example.com"]
variables = { NODE_ENV = "staging", NEW = 1, TOKEN = { secret = true } }
"#;
        let secrets = BTreeMap::from([("TOKEN".to_owned(), "s3cret".to_owned())]);
        let p = plan(
            "production",
            &spec(text),
            Some(&r),
            &secrets,
            Options::default(),
        );
        let api = &p.services[0];
        assert_eq!(api.action, ServiceAction::Update);
        let keys: Vec<&str> = api.settings.iter().map(|s| s.key.as_str()).collect();
        // healthcheckPath was dropped from the file: back to the default.
        assert_eq!(keys, ["healthcheckPath", "port", "source"]);
        let port = &api.settings[1];
        assert_eq!(
            (port.from.clone(), port.to.clone()),
            (json!(8080), json!(3000))
        );
        assert_eq!(api.settings[0].to, Value::Null);
        assert_eq!(api.config["source"]["branch"], "dev");
        assert!(api.config.get("healthcheckPath").is_none());

        let ops: BTreeMap<&str, VarOp> = api
            .variables
            .iter()
            .map(|v| (v.name.as_str(), v.op))
            .collect();
        assert_eq!(
            ops,
            BTreeMap::from([
                ("NODE_ENV", VarOp::Update),
                ("NEW", VarOp::Add),
                ("TOKEN", VarOp::Add),
                ("OLD", VarOp::Delete),
                ("LEGACY", VarOp::Delete),
                ("SEALED", VarOp::Delete),
            ])
        );
        let node_env = api.variables.iter().find(|v| v.name == "NODE_ENV").unwrap();
        assert_eq!(node_env.from.as_deref(), Some("production"));
        assert_eq!(node_env.to.as_deref(), Some("staging"));
        // Secrets: sent, never shown (not even in JSON or debug output).
        let token = api.variables.iter().find(|v| v.name == "TOKEN").unwrap();
        assert!(token.secret && token.to.is_none());
        assert_eq!(token.value, Some(Secret("s3cret".into())));
        let shown = format!("{} {p:?}", serde_json::to_string(&p).unwrap());
        assert!(!shown.contains("s3cret"), "{shown}");

        assert_eq!(
            api.domains,
            [
                DomainChange::Update {
                    host: "api.example.com".into(),
                    id: "d1".into(),
                    from: None,
                    to: Some(9000),
                    rate_limit: None,
                },
                DomainChange::Add {
                    host: "www.example.com".into(),
                    port: None
                },
            ],
            "the generated domain is never touched"
        );
        let (add, change, delete) = p.counts();
        assert_eq!((add, change, delete), (3, 5, 3));
    }

    #[test]
    fn secrets_need_a_value_unless_they_exist() {
        let r = remote(vec![api_remote()]);
        let text = r#"
[services.api]
source = { repo = "acme/api", branch = "main" }
healthcheckPath = "/health"
preserve = ["OLD", "LEGACY", "NODE_ENV"]
variables = { SEALED = { secret = true }, MISSING = { secret = true } }
"#;
        let p = plan(
            "production",
            &spec(text),
            Some(&r),
            &BTreeMap::new(),
            Options::default(),
        );
        assert_eq!(p.errors.len(), 1, "{:?}", p.errors);
        assert!(p.errors[0].contains("MISSING"));
        assert!(p.services[0].variables.is_empty());

        // A sealed value isn't compared unless asked to.
        let secrets = BTreeMap::from([
            ("SEALED".to_owned(), "new".to_owned()),
            ("MISSING".to_owned(), "m".to_owned()),
        ]);
        let p = plan(
            "production",
            &spec(text),
            Some(&r),
            &secrets,
            Options::default(),
        );
        assert!(p.errors.is_empty());
        let names: Vec<&str> = p.services[0]
            .variables
            .iter()
            .map(|v| v.name.as_str())
            .collect();
        assert_eq!(names, ["MISSING"]);
        let p = plan(
            "production",
            &spec(text),
            Some(&r),
            &secrets,
            Options {
                update_sealed: true,
            },
        );
        let sealed = p.services[0]
            .variables
            .iter()
            .find(|v| v.name == "SEALED")
            .unwrap();
        assert_eq!((sealed.op, sealed.sealed), (VarOp::Update, true));

        // A plain value over a sealed variable: a warning, not a change.
        let text = text.replace("SEALED = { secret = true }", "SEALED = \"plain\"");
        let p = plan(
            "production",
            &spec(&text),
            Some(&r),
            &secrets,
            Options::default(),
        );
        assert!(
            p.warnings[0].contains("SEALED is sealed"),
            "{:?}",
            p.warnings
        );
        assert!(!p.services[0].variables.iter().any(|v| v.name == "SEALED"));
    }

    #[test]
    fn new_services_templates_volumes_and_manual_steps() {
        let pg = RemoteService {
            service_id: "s2".into(),
            service_name: "postgres".into(),
            instance_id: "i2".into(),
            template: Some("postgres".into()),
            template_config: Some(json!({
                "source": { "type": "image", "image": "postgres:17" }, "port": 5432,
                "startCommand": ["postgres", "-D", "/data"],
            })),
            template_variables: vec!["POSTGRES_PASSWORD".into(), "DATABASE_URL".into()],
            config: json!({
                "source": { "type": "image", "image": "postgres:17" }, "port": 5432,
                "startCommand": ["postgres", "-D", "/data"], "healthcheckTimeoutSecs": 300,
            }),
            volume: Some(RemoteVolume {
                id: "vol".into(),
                mount_path: "/var/lib/postgresql/data".into(),
            }),
            tcp_proxy: Some(RemoteTcpProxy {
                id: "t1".into(),
                public_port: 20001,
                target_port: 5432,
            }),
            variables: vec![
                var("POSTGRES_PASSWORD", Some("pw"), false),
                var("DATABASE_URL", Some("postgresql://…"), false),
            ],
            ..Default::default()
        };
        let mut old = api_remote();
        old.service_name = "old".into();
        old.volume = Some(RemoteVolume {
            id: "v9".into(),
            mount_path: "/data".into(),
        });
        let r = remote(vec![pg, old]);
        let text = r#"
[services.postgres]
template = "postgres"
tcpProxy = 5432

[services.web]
source = { image = "nginx:1.27" }
port = 80
volume = "/cache"
generateDomain = true

[services.site]
source = { local = "." }
"#;
        let p = plan(
            "production",
            &spec(text),
            Some(&r),
            &BTreeMap::new(),
            Options::default(),
        );
        let pg = p.services.iter().find(|s| s.service == "postgres").unwrap();
        // Template settings and variables are the template's business.
        assert!(pg.is_empty(), "{pg:?}");
        let web = p.services.iter().find(|s| s.service == "web").unwrap();
        assert_eq!(web.action, ServiceAction::Create);
        assert_eq!(
            web.config,
            json!({ "source": { "type": "image", "image": "nginx:1.27" }, "port": 80 })
        );
        assert_eq!(web.volume.as_deref(), Some("/cache"));
        assert_eq!(web.domains, [DomainChange::Generate]);
        // A local service is created as one; `modula up` deploys it.
        let site = p.services.iter().find(|s| s.service == "site").unwrap();
        assert_eq!(site.action, ServiceAction::Create);
        assert_eq!(site.config, json!({ "source": { "type": "local" } }));
        // Deleting a service or a volume is manual.
        assert_eq!(p.manual.len(), 1, "{:?}", p.manual);
        assert!(
            p.manual
                .iter()
                .any(|m| m.starts_with("old:") && m.contains("dashboard"))
        );

        // A template service described by its image alone keeps the
        // template's settings: only the image changes.
        let pinned = text.replace(
            "template = \"postgres\"",
            "source = { image = \"postgres:16\" }",
        );
        let p = plan(
            "production",
            &spec(&pinned),
            Some(&r),
            &BTreeMap::new(),
            Options::default(),
        );
        let pg = p.services.iter().find(|s| s.service == "postgres").unwrap();
        let keys: Vec<&str> = pg.settings.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["source"], "{:?}", pg.settings);
        assert_eq!(pg.config["port"], 5432);

        // Dropping the TCP proxy deletes it; a new port replaces it.
        let p = plan(
            "production",
            &spec(&text.replace("tcpProxy = 5432", "tcpProxy = 5433")),
            Some(&r),
            &BTreeMap::new(),
            Options::default(),
        );
        let pg = p.services.iter().find(|s| s.service == "postgres").unwrap();
        assert_eq!(pg.tcp_proxy.len(), 2);
        // A missing environment is created; everything in it is new.
        let p = plan(
            "preview",
            &spec(text),
            None,
            &BTreeMap::new(),
            Options::default(),
        );
        assert!(p.create_environment);
        assert!(p.services.iter().all(|s| s.action == ServiceAction::Create));
    }

    #[test]
    fn shared_variables_and_preserve() {
        let mut r = remote(vec![]);
        r.shared = vec![
            var("REGION", Some("eu"), false),
            var("KEEP", Some("1"), false),
        ];
        let text = "[shared]\nvariables = { REGION = \"us\" }\npreserve = [\"KEEP\"]\n";
        let p = plan(
            "production",
            &spec(text),
            Some(&r),
            &BTreeMap::new(),
            Options::default(),
        );
        assert_eq!(p.shared.len(), 1);
        assert_eq!(p.shared[0].op, VarOp::Update);
        assert_eq!(p.shared[0].to.as_deref(), Some("us"));
    }

    #[test]
    fn unlisted_variables_are_left_alone() {
        let mut r = remote(vec![]);
        r.shared = vec![var("REGION", Some("eu"), false)];
        let p = plan(
            "production",
            &spec("[shared]\n"),
            Some(&r),
            &BTreeMap::new(),
            Options::default(),
        );
        assert!(p.shared.is_empty(), "{:?}", p.shared);
    }

    #[test]
    fn normalizing_ignores_defaults() {
        let a = json!({ "source": {"type": "image", "image": "x"}, "port": 8080, "watchPaths": [], "drainingSeconds": 0, "startCommand": null });
        let b = json!({ "source": {"type": "image", "image": "x"} });
        assert_eq!(normalize(&a), normalize(&b));
        assert!(looks_secret("STRIPE_SECRET_KEY") && looks_secret("db_password"));
        assert!(!looks_secret("NODE_ENV"));
    }
}
