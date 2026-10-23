//! `config init` / `config pull`: write `modula.toml` from the server.
//!
//! Each environment becomes a full spec; what all environments share goes in
//! the base and the rest in `[environments.<name>]` overlays. Secret values
//! never reach the file: sealed variables and names that look like
//! credentials become `{ secret = true }`, and a template's own variables
//! (generated passwords and the URLs built from them) are left out.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde_json::{Map, Value};

use super::diff::{looks_secret, normalize};
use super::model::{CONFIG_KEYS, DomainSpec, ProjectSpec, SERVICE_KEYS, ServiceSpec, VarSpec};
use super::remote::{RemoteEnv, RemoteService, RemoteVar};

/// The generated file and the variables that were turned into secrets.
pub struct Pulled {
    pub text: String,
    /// `service.NAME` (or `shared.NAME`) written as `{ secret = true }`
    /// although not sealed on the server.
    pub guessed_secrets: Vec<String>,
}

fn variables(
    vars: &[RemoteVar],
    skip: &[String],
    scope: &str,
    guessed: &mut BTreeSet<String>,
) -> BTreeMap<String, VarSpec> {
    vars.iter()
        .filter(|v| !skip.contains(&v.name))
        .map(|v| {
            let spec = if v.sealed {
                VarSpec::secret(true)
            } else if looks_secret(&v.name) || v.value.is_none() {
                guessed.insert(format!("{scope}.{}", v.name));
                VarSpec::secret(false)
            } else {
                VarSpec::Value(v.value.clone().unwrap_or_default())
            };
            (v.name.clone(), spec)
        })
        .collect()
}

fn service(r: &RemoteService, guessed: &mut BTreeSet<String>) -> anyhow::Result<ServiceSpec> {
    let mut config = normalize(&r.config);
    let template = r.template_config.as_ref().map(normalize);
    if let Some(t) = &template {
        // Only what differs from the template.
        config.retain(|k, v| t.get(k) != Some(v));
    }
    let mut out = Map::new();
    if let Some(t) = &r.template {
        out.insert("template".into(), t.clone().into());
    }
    if let Some(source) = config.remove("source") {
        let spec = match source.get("type").and_then(Value::as_str) {
            Some("repo") => {
                let mut s = Map::new();
                for (from, to) in [
                    ("repo", "repo"),
                    ("branch", "branch"),
                    ("rootDir", "rootDir"),
                ] {
                    if let Some(v) = source.get(from).filter(|v| !v.is_null()) {
                        s.insert(to.into(), v.clone());
                    }
                }
                Value::Object(s)
            }
            Some("image") => serde_json::json!({ "image": source.get("image") }),
            // Uploaded with `up` (or a kind this CLI doesn't know).
            _ => serde_json::json!({ "local": "." }),
        };
        out.insert("source".into(), spec);
    }
    for k in CONFIG_KEYS {
        if let Some(v) = config.remove(*k) {
            out.insert((*k).into(), v);
        }
    }
    let custom: Vec<Value> = r
        .domains
        .iter()
        .filter(|d| d.kind == "custom")
        .map(|d| match d.target_port {
            Some(port) => serde_json::to_value(DomainSpec::WithPort {
                host: d.hostname.clone(),
                port: Some(port),
            }),
            None => serde_json::to_value(DomainSpec::Host(d.hostname.clone())),
        })
        .collect::<Result<_, _>>()?;
    if !custom.is_empty() {
        out.insert("domains".into(), Value::Array(custom));
    }
    if r.domains.iter().any(|d| d.kind == "generated") {
        out.insert("generateDomain".into(), true.into());
    }
    // A template's volume is the template's.
    if let (Some(v), None) = (&r.volume, &r.template) {
        out.insert("volume".into(), v.mount_path.clone().into());
    }
    if let Some(t) = &r.tcp_proxy {
        out.insert("tcpProxy".into(), t.target_port.into());
    }
    let mut spec: ServiceSpec = serde_json::from_value(Value::Object(out))?;
    spec.variables = variables(
        &r.variables,
        &r.template_variables,
        &r.service_name,
        guessed,
    );
    Ok(spec)
}

/// One environment as a full spec.
pub fn environment_spec(
    env: &RemoteEnv,
    project: &str,
    guessed: &mut BTreeSet<String>,
) -> anyhow::Result<ProjectSpec> {
    let mut spec = ProjectSpec {
        project: Some(project.to_owned()),
        ..Default::default()
    };
    for s in &env.services {
        spec.services
            .insert(s.service_name.clone(), service(s, guessed)?);
    }
    spec.shared.variables = variables(&env.shared, &[], "shared", guessed);
    Ok(spec)
}

/// Split per-environment values into what they share and what differs. A
/// `source` that differs stays in the base (from the first environment) so
/// the base alone is still valid; overlays then hold only real differences.
fn split(
    values: &[Option<&toml::Value>],
    key: &str,
    parent: &str,
) -> (Option<toml::Value>, Vec<Option<toml::Value>>) {
    let n = values.len();
    let all_tables = values
        .iter()
        .all(|v| matches!(v, Some(toml::Value::Table(_))));
    // The root, `services`, each service, `shared` and `variables` tables
    // split key by key; anything below (a source, a secret) is one value.
    let nested = matches!(key, "" | "services" | "shared" | "variables") || parent == "services";
    if all_tables && nested && n > 1 {
        let tables: Vec<&toml::Table> = values
            .iter()
            .filter_map(|v| v.and_then(toml::Value::as_table))
            .collect();
        let keys: BTreeSet<&String> = tables.iter().flat_map(|t| t.keys()).collect();
        let mut base = toml::Table::new();
        let mut overlays = vec![toml::Table::new(); n];
        for k in keys {
            let column: Vec<Option<&toml::Value>> = tables.iter().map(|t| t.get(k)).collect();
            let (b, o) = split(&column, k, key);
            if let Some(b) = b {
                base.insert(k.clone(), b);
            }
            for (i, v) in o.into_iter().enumerate() {
                if let Some(v) = v {
                    overlays[i].insert(k.clone(), v);
                }
            }
        }
        let base = (!base.is_empty()).then_some(toml::Value::Table(base));
        let overlays = overlays
            .into_iter()
            .map(|t| (!t.is_empty()).then_some(toml::Value::Table(t)))
            .collect();
        return (base, overlays);
    }
    let first = values.first().copied().flatten();
    if values.iter().all(|v| *v == first) {
        return (first.cloned(), vec![None; n]);
    }
    if key == "source" && values.iter().all(Option::is_some) {
        let overlays = values
            .iter()
            .map(|v| (*v != first).then(|| v.cloned()).flatten())
            .collect();
        return (first.cloned(), overlays);
    }
    (None, values.iter().map(|v| v.cloned()).collect())
}

/// The file for `envs` (the first is the base's reference).
pub fn generate(project: &str, envs: &[RemoteEnv]) -> anyhow::Result<Pulled> {
    let mut guessed = BTreeSet::new();
    let mut tables = Vec::with_capacity(envs.len());
    for env in envs {
        let spec = environment_spec(env, project, &mut guessed)?;
        let mut t = toml::Table::try_from(&spec)?;
        t.remove("project");
        tables.push(t);
    }
    let roots: Vec<toml::Value> = tables.into_iter().map(toml::Value::Table).collect();
    let refs: Vec<Option<&toml::Value>> = roots.iter().map(Some).collect();
    let (base, overlays) = if refs.len() == 1 {
        (refs[0].cloned(), vec![None])
    } else {
        split(&refs, "", "")
    };

    let mut text = String::from(
        "# Modula Deploy config as code: `modula config plan` shows what\n\
         # `config apply` would change. Secret values are never written here: put\n\
         # them in .modula/secrets.<environment>.env (gitignored) or pass --from-env.\n\n",
    );
    writeln!(
        text,
        "project = {}",
        toml::Value::String(project.to_owned())
    )?;
    let base = base.and_then(|b| b.as_table().cloned()).unwrap_or_default();
    write_root(&mut text, &[], &base)?;
    for (env, overlay) in envs.iter().zip(overlays) {
        if let Some(toml::Value::Table(t)) = overlay {
            write_root(&mut text, &["environments", &env.name], &t)?;
        }
    }
    Ok(Pulled {
        text,
        guessed_secrets: guessed.into_iter().collect(),
    })
}

fn key(k: &str) -> String {
    let bare = !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if bare {
        k.to_owned()
    } else {
        toml::Value::String(k.to_owned()).to_string()
    }
}

/// A value on one line, with inline tables in reading order
/// (`{ repo, branch }`, `{ secret = true, sealed = true }`).
fn inline(v: &toml::Value) -> String {
    const FIRST: &[&str] = &[
        "secret", "sealed", "repo", "branch", "rootDir", "image", "local", "host", "port",
    ];
    match v {
        toml::Value::Table(t) => {
            let mut keys: Vec<&String> = t.keys().collect();
            keys.sort_by_key(|k| FIRST.iter().position(|f| f == k).unwrap_or(FIRST.len()));
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{} = {}", key(k), inline(&t[k.as_str()])))
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
        toml::Value::Array(a) => {
            let parts: Vec<String> = a.iter().map(inline).collect();
            format!("[{}]", parts.join(", "))
        }
        other => other.to_string(),
    }
}

fn header(path: &[&str]) -> String {
    let parts: Vec<String> = path.iter().map(|p| key(p)).collect();
    format!("\n[{}]\n", parts.join("."))
}

/// `shared` and `services` of one root (the base or an overlay).
fn write_root(out: &mut String, prefix: &[&str], root: &toml::Table) -> anyhow::Result<()> {
    if let Some(toml::Value::Table(shared)) = root.get("shared")
        && let Some(toml::Value::Table(vars)) = shared.get("variables")
    {
        let path: Vec<&str> = prefix
            .iter()
            .copied()
            .chain(["shared", "variables"])
            .collect();
        out.push_str(&header(&path));
        for (k, v) in vars {
            writeln!(out, "{} = {}", key(k), inline(v))?;
        }
    }
    if let Some(toml::Value::Table(services)) = root.get("services") {
        for (name, svc) in services {
            let Some(svc) = svc.as_table() else { continue };
            let path: Vec<&str> = prefix.iter().copied().chain(["services", name]).collect();
            // template and source first, then settings, then the rest.
            let order = ["template", "source"]
                .iter()
                .chain(CONFIG_KEYS)
                .chain(SERVICE_KEYS);
            let mut seen = BTreeSet::new();
            let mut lines = String::new();
            for k in order {
                if *k == "variables" || !seen.insert(*k) {
                    continue;
                }
                if let Some(v) = svc.get(*k) {
                    writeln!(lines, "{} = {}", key(k), inline(v))?;
                }
            }
            if !lines.is_empty() || !svc.contains_key("variables") {
                out.push_str(&header(&path));
                out.push_str(&lines);
            }
            if let Some(toml::Value::Table(vars)) = svc.get("variables") {
                let path: Vec<&str> = path.iter().copied().chain(["variables"]).collect();
                out.push_str(&header(&path));
                for (k, v) in vars {
                    writeln!(out, "{} = {}", key(k), inline(v))?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iac::model::ConfigFile;
    use crate::iac::remote::{RemoteDomain, RemoteVolume};
    use serde_json::json;

    fn var(name: &str, value: Option<&str>, sealed: bool) -> RemoteVar {
        RemoteVar {
            id: name.into(),
            name: name.into(),
            sealed,
            value: value.map(Into::into),
        }
    }

    fn env(name: &str, branch: &str, host: &str, extra: Option<(&str, &str)>) -> RemoteEnv {
        let mut vars = vec![
            var("NODE_ENV", Some("production"), false),
            var("API_KEY", Some("leak-me-not"), false),
            var("SEAL", None, true),
        ];
        if let Some((k, v)) = extra {
            vars.push(var(k, Some(v), false));
        }
        RemoteEnv {
            id: name.into(),
            name: name.into(),
            services: vec![
                RemoteService {
                    service_name: "api".into(),
                    config: json!({
                        "source": { "type": "repo", "repo": "acme/api", "branch": branch },
                        "port": 3000, "healthcheckTimeoutSecs": 300, "builder": "dockerfile",
                        "preDeployCommand": ["sh", "migrate.sh"], "autoDeploy": true,
                    }),
                    domains: vec![
                        RemoteDomain {
                            hostname: host.into(),
                            kind: "custom".into(),
                            ..Default::default()
                        },
                        RemoteDomain {
                            hostname: "x.apps.example.com".into(),
                            kind: "generated".into(),
                            ..Default::default()
                        },
                    ],
                    volume: Some(RemoteVolume {
                        id: "v".into(),
                        mount_path: "/data".into(),
                    }),
                    variables: vars,
                    ..Default::default()
                },
                RemoteService {
                    service_name: "postgres".into(),
                    template: Some("postgres".into()),
                    template_config: Some(
                        json!({ "source": { "type": "image", "image": "postgres:17" }, "port": 5432 }),
                    ),
                    template_variables: vec!["POSTGRES_PASSWORD".into()],
                    config: json!({ "source": { "type": "image", "image": "postgres:17" }, "port": 5432, "memoryMb": 512 }),
                    variables: vec![var("POSTGRES_PASSWORD", Some("hunter2"), false)],
                    ..Default::default()
                },
            ],
            shared: vec![var("REGION", Some("eu"), false)],
            ..Default::default()
        }
    }

    #[test]
    fn pulled_files_round_trip_without_secrets() {
        let envs = [
            env("production", "main", "api.example.com", None),
            env(
                "staging",
                "staging",
                "staging.example.com",
                Some(("DEBUG", "1")),
            ),
        ];
        let pulled = generate("shop", &envs).unwrap();
        let text = &pulled.text;
        for secret in ["leak-me-not", "hunter2", "POSTGRES_PASSWORD"] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert_eq!(pulled.guessed_secrets, ["api.API_KEY"]);
        assert!(
            text.contains("SEAL = { secret = true, sealed = true }"),
            "{text}"
        );
        assert!(text.contains("API_KEY = { secret = true }"), "{text}");
        assert!(
            text.contains("[environments.staging.services.api]"),
            "{text}"
        );

        // Each environment resolves to what the server has.
        let file = ConfigFile::parse(std::path::Path::new("modula.toml"), text)
            .unwrap_or_else(|e| panic!("{e:#}\n{text}"));
        let mut guessed = BTreeSet::new();
        for e in &envs {
            let want = environment_spec(e, "shop", &mut guessed).unwrap();
            let got = file.resolve(Some(&e.name)).unwrap();
            assert_eq!(got, want, "{}:\n{text}", e.name);
        }
        let prod = file.resolve(Some("production")).unwrap();
        let pg = &prod.services["postgres"];
        assert_eq!(pg.memory_mb, Some(512));
        assert!(
            pg.source.is_none() && pg.port.is_none(),
            "template defaults stay out"
        );
        let api = &prod.services["api"];
        assert_eq!(api.generate_domain, Some(true));
        assert_eq!(api.volume.as_deref(), Some("/data"));
        assert!(!prod.services["api"].variables.contains_key("DEBUG"));
    }

    #[test]
    fn a_single_environment_has_no_overlays() {
        let pulled = generate("shop", &[env("production", "main", "a.example.com", None)]).unwrap();
        assert!(!pulled.text.contains("[environments"), "{}", pulled.text);
        assert!(
            pulled.text.contains("[shared.variables]\nREGION = \"eu\""),
            "{}",
            pulled.text
        );
    }
}
