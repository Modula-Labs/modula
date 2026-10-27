//! Variable encryption. Every value is sealed with its *environment's* data key
//! (`data_keys.scope = environment id`), so environments never share a key and
//! one can be rotated or destroyed alone.

use std::collections::{BTreeMap, BTreeSet};

use modula_crypto::envelope::FieldAad;
use modula_id::Id;
use secrecy::{ExposeSecret, SecretString};
use sqlx::PgPool;

use super::refs::{Expander, VarKey};
use crate::error::{AppError, AppResult};
use crate::keyring::Keyring;

pub const MAX_VALUE_BYTES: usize = 32 * 1024;
pub const MAX_VARIABLES: usize = 500;

/// Resolved variables for one deployment. Values are secrets: never logged.
pub type EnvMap = BTreeMap<String, SecretString>;

pub fn validate_name(name: &str) -> AppResult<()> {
    let ok = !name.is_empty()
        && name.len() <= 255
        && !name.as_bytes()[0].is_ascii_digit()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !ok {
        return Err(AppError::bad_request(format!(
            "invalid variable name {:?}: use letters, digits and _ (not starting with a digit)",
            name.chars().take(64).collect::<String>()
        )));
    }
    if name.starts_with("MODULA_") {
        return Err(AppError::bad_request(
            "names starting with MODULA_ are reserved",
        ));
    }
    Ok(())
}

pub fn validate_value(value: &str) -> AppResult<()> {
    if value.len() > MAX_VALUE_BYTES || value.contains('\0') {
        return Err(AppError::bad_request(
            "variable values must be under 32 KB and contain no NUL bytes",
        ));
    }
    Ok(())
}

fn var_aad(env: Id, row: &str) -> FieldAad<'_> {
    FieldAad {
        scope: env,
        table: "variables",
        column: "value",
        row,
    }
}

fn snapshot_aad(env: Id, row: &str) -> FieldAad<'_> {
    FieldAad {
        scope: env,
        table: "deployments",
        column: "env",
        row,
    }
}

pub async fn seal_variable(
    keys: &Keyring,
    db: &PgPool,
    env: Id,
    var: Id,
    value: &str,
) -> AppResult<Vec<u8>> {
    keys.seal(db, value.as_bytes(), var_aad(env, &var.to_string()))
        .await
}

pub async fn open_variable(
    keys: &Keyring,
    db: &PgPool,
    env: Id,
    var: Id,
    sealed: &[u8],
) -> AppResult<SecretString> {
    let plain = keys
        .open(db, sealed, var_aad(env, &var.to_string()))
        .await?;
    let s =
        String::from_utf8(plain.to_vec()).map_err(|_| anyhow::anyhow!("variable is not UTF-8"))?;
    Ok(SecretString::from(s))
}

/// A service instance's variables with every `${{…}}` reference expanded.
pub struct Resolved {
    pub vars: EnvMap,
    /// References to variables that don't exist (expanded to "").
    pub missing: Vec<String>,
}

/// Resolve a service instance's variables for a new deployment. Decrypts the
/// environment's variables (references can reach any of them) and adds each
/// service's platform variables (`PORT`, `MODULA_PRIVATE_DOMAIN`, …) so they
/// can be referenced too; only the instance's own variables are returned.
pub async fn resolve(
    keys: &Keyring,
    db: &PgPool,
    tcp_proxy_host: Option<&str>,
    env: Id,
    instance: Id,
) -> AppResult<Resolved> {
    resolve_for_deployment(keys, db, tcp_proxy_host, env, instance, None).await
}

/// What a service's platform variables (`PORT`, `MODULA_*`) are made of.
pub struct Platform<'a> {
    pub port: u64,
    pub project: Id,
    pub environment: Id,
    pub environment_name: &'a str,
    pub service: Id,
    pub service_name: &'a str,
    pub deployment: Option<Id>,
    pub public_domain: Option<&'a str>,
    /// `MODULA_TCP_PROXY_HOST`, else the node's public IP.
    pub tcp_proxy_host: Option<&'a str>,
    /// The public port of the service's TCP proxy, if it has one.
    pub tcp_proxy_port: Option<i32>,
}

impl Platform<'_> {
    /// The platform variables; user variables override them (only `PORT`
    /// can collide, `MODULA_*` names are reserved).
    pub fn vars(&self) -> [(&'static str, String); 11] {
        let tcp_host = match self.tcp_proxy_port {
            Some(_) => self.tcp_proxy_host.unwrap_or_default().to_owned(),
            None => String::new(),
        };
        [
            ("PORT", self.port.to_string()),
            ("MODULA_PROJECT_ID", self.project.to_string()),
            ("MODULA_ENVIRONMENT_ID", self.environment.to_string()),
            ("MODULA_ENVIRONMENT_NAME", self.environment_name.to_owned()),
            ("MODULA_SERVICE_ID", self.service.to_string()),
            ("MODULA_SERVICE_NAME", self.service_name.to_owned()),
            (
                "MODULA_DEPLOYMENT_ID",
                self.deployment.map(|d| d.to_string()).unwrap_or_default(),
            ),
            (
                "MODULA_PRIVATE_DOMAIN",
                format!("{}.modula.internal", self.service_name),
            ),
            (
                "MODULA_PUBLIC_DOMAIN",
                self.public_domain.unwrap_or_default().to_owned(),
            ),
            ("MODULA_TCP_PROXY_DOMAIN", tcp_host),
            (
                "MODULA_TCP_PROXY_PORT",
                self.tcp_proxy_port
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
            ),
        ]
    }
}

/// [`resolve`] for a new deployment `deployment` of `instance`: its own
/// `MODULA_DEPLOYMENT_ID` is that id; other services' is the deployment
/// serving them right now (empty when none is).
pub async fn resolve_for_deployment(
    keys: &Keyring,
    db: &PgPool,
    tcp_proxy_host: Option<&str>,
    env: Id,
    instance: Id,
    deployment: Option<Id>,
) -> AppResult<Resolved> {
    let loaded = load(keys, db, tcp_proxy_host, env, instance, deployment).await?;
    let mut expander = Expander::new(&loaded.raw);
    let mut vars = EnvMap::new();
    for key in loaded.own_user_vars() {
        let value = expander
            .value(&key)
            .map_err(|e| AppError::bad_request(e.to_string()))?
            .unwrap_or_default();
        vars.insert(key.1, SecretString::from(value));
    }
    Ok(Resolved {
        vars,
        missing: expander.missing,
    })
}

/// A service instance's variables for a command run outside the platform
/// (`modula run`): its own variables expanded plus its `MODULA_*` platform
/// variables, except sealed variables and every variable whose value
/// references one (directly or through others): sealed values never leave
/// the server. Those are listed in `withheld`.
pub struct ForRun {
    pub vars: EnvMap,
    pub withheld: Vec<String>,
    pub missing: Vec<String>,
}

pub async fn resolve_for_run(
    keys: &Keyring,
    db: &PgPool,
    tcp_proxy_host: Option<&str>,
    env: Id,
    instance: Id,
) -> AppResult<ForRun> {
    let loaded = load(keys, db, tcp_proxy_host, env, instance, None).await?;
    let mut expander = Expander::new(&loaded.raw);
    let mut vars = EnvMap::new();
    let mut withheld = Vec::new();
    for key in loaded.own_user_vars() {
        if super::refs::depends_on(&loaded.raw, &key, &loaded.sealed) {
            withheld.push(key.1);
            continue;
        }
        let value = expander
            .value(&key)
            .map_err(|e| AppError::bad_request(e.to_string()))?
            .unwrap_or_default();
        vars.insert(key.1, SecretString::from(value));
    }
    for ((owner, name), value) in &loaded.raw {
        if owner.as_deref() == Some(loaded.own.as_str())
            && name.starts_with("MODULA_")
            && !value.is_empty()
        {
            vars.insert(name.clone(), SecretString::from(value.clone()));
        }
    }
    Ok(ForRun {
        vars,
        withheld,
        missing: expander.missing,
    })
}

/// Every variable of an environment, decrypted, with each service's
/// platform variables; what both resolvers expand from.
struct Loaded {
    raw: BTreeMap<VarKey, String>,
    /// User-defined variables (not platform ones).
    user: Vec<VarKey>,
    sealed: BTreeSet<VarKey>,
    /// The instance's service name.
    own: String,
}

impl Loaded {
    fn own_user_vars(&self) -> Vec<VarKey> {
        self.user
            .iter()
            .filter(|(owner, _)| owner.as_deref() == Some(self.own.as_str()))
            .cloned()
            .collect()
    }
}

async fn load(
    keys: &Keyring,
    db: &PgPool,
    tcp_proxy_host: Option<&str>,
    env: Id,
    instance: Id,
    deployment: Option<Id>,
) -> AppResult<Loaded> {
    // (instance, service, service name, environment name, project, config,
    // public domain, TCP proxy port, its node's public IP, serving deployment)
    #[allow(clippy::type_complexity)]
    let services: Vec<(
        Id,
        Id,
        String,
        String,
        Id,
        serde_json::Value,
        Option<String>,
        Option<i32>,
        Option<String>,
        Option<Id>,
    )> = sqlx::query_as(
        "SELECT i.id, s.id, s.name, e.name, s.project_id, i.config,
                (SELECT hostname FROM domains WHERE service_instance_id = i.id AND status = 'verified' ORDER BY created_at LIMIT 1),
                t.public_port, n.public_ip,
                (SELECT d.id FROM deployments d WHERE d.service_instance_id = i.id
                   AND d.status IN ('active', 'sleeping', 'crashed') ORDER BY d.created_at DESC LIMIT 1)
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN environments e ON e.id = i.environment_id
         LEFT JOIN tcp_proxies t ON t.service_instance_id = i.id
         LEFT JOIN nodes n ON n.id = t.node_id
         WHERE i.environment_id = $1",
    )
    .bind(env)
    .fetch_all(db)
    .await?;
    // (id, service name or None for shared, name, value, sealed)
    #[allow(clippy::type_complexity)]
    let rows: Vec<(Id, Option<String>, String, Vec<u8>, bool)> = sqlx::query_as(
        "SELECT v.id, s.name, v.name, v.value_sealed, v.sealed FROM variables v
         LEFT JOIN service_instances i ON i.id = v.service_instance_id
         LEFT JOIN services s ON s.id = i.service_id
         WHERE v.environment_id = $1",
    )
    .bind(env)
    .fetch_all(db)
    .await?;

    let mut raw: BTreeMap<VarKey, String> = BTreeMap::new();
    let mut user: Vec<VarKey> = Vec::new();
    let mut sealed = BTreeSet::new();
    for (id, owner, name, value_sealed, is_sealed) in rows {
        let key = (owner, name);
        user.push(key.clone());
        if is_sealed {
            sealed.insert(key.clone());
        }
        let value = open_variable(keys, db, env, id, &value_sealed).await?;
        raw.insert(key, value.expose_secret().to_owned());
    }
    let mut own = None;
    for (
        inst,
        service_id,
        service,
        env_name,
        project,
        config,
        domain,
        tcp_port,
        node_ip,
        serving,
    ) in services
    {
        let deployment_id = match deployment {
            Some(d) if inst == instance => Some(d),
            _ => serving,
        };
        let platform = Platform {
            port: config.get("port").and_then(|p| p.as_u64()).unwrap_or(8080),
            project,
            environment: env,
            environment_name: &env_name,
            service: service_id,
            service_name: &service,
            deployment: deployment_id,
            public_domain: domain.as_deref(),
            tcp_proxy_host: tcp_proxy_host.or(node_ip.as_deref()),
            tcp_proxy_port: tcp_port,
        };
        for (k, v) in platform.vars() {
            // A user PORT overrides the platform one.
            raw.entry((Some(service.clone()), k.to_owned()))
                .or_insert(v);
        }
        if inst == instance {
            own = Some(service);
        }
    }
    let own = own.ok_or(AppError::NotFound)?;
    Ok(Loaded {
        raw,
        user,
        sealed,
        own,
    })
}

pub async fn seal_snapshot(
    keys: &Keyring,
    db: &PgPool,
    env: Id,
    deployment: Id,
    vars: &EnvMap,
) -> AppResult<Vec<u8>> {
    let plain: BTreeMap<&str, &str> = vars
        .iter()
        .map(|(k, v)| (k.as_str(), v.expose_secret()))
        .collect();
    let json = zeroize::Zeroizing::new(serde_json::to_vec(&plain).map_err(anyhow::Error::from)?);
    keys.seal(db, &json, snapshot_aad(env, &deployment.to_string()))
        .await
}

pub async fn open_snapshot(
    keys: &Keyring,
    db: &PgPool,
    env: Id,
    deployment: Id,
    sealed: &[u8],
) -> AppResult<EnvMap> {
    let plain = keys
        .open(db, sealed, snapshot_aad(env, &deployment.to_string()))
        .await?;
    let map: BTreeMap<String, String> =
        serde_json::from_slice(&plain).map_err(anyhow::Error::from)?;
    Ok(map
        .into_iter()
        .map(|(k, v)| (k, SecretString::from(v)))
        .collect())
}

fn vars_aad(env: Id, row: &str) -> FieldAad<'_> {
    FieldAad {
        scope: env,
        table: "deployments",
        column: "vars",
        row,
    }
}

/// One of an instance's variables as written (references unexpanded).
#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Eq)]
pub struct RawVar {
    pub value: String,
    pub sealed: bool,
}

impl std::fmt::Debug for RawVar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawVar")
            .field("sealed", &self.sealed)
            .finish_non_exhaustive()
    }
}

/// An instance's own variables by name, decrypted. Secrets: never logged.
pub type RawVars = BTreeMap<String, RawVar>;

/// The instance's own variables, decrypted (for snapshots and diffs).
pub async fn raw_variables(
    keys: &Keyring,
    db: &PgPool,
    env: Id,
    instance: Id,
) -> AppResult<RawVars> {
    let rows: Vec<(Id, String, Vec<u8>, bool)> = sqlx::query_as(
        "SELECT id, name, value_sealed, sealed FROM variables WHERE service_instance_id = $1",
    )
    .bind(instance)
    .fetch_all(db)
    .await?;
    let mut out = RawVars::new();
    for (id, name, sealed_value, sealed) in rows {
        let value = open_variable(keys, db, env, id, &sealed_value).await?;
        out.insert(
            name,
            RawVar {
                value: value.expose_secret().to_owned(),
                sealed,
            },
        );
    }
    Ok(out)
}

/// Seal a deployment's copy of its instance's variables as written, so a
/// pending change can later be discarded back to what was deployed.
pub async fn seal_vars_snapshot(
    keys: &Keyring,
    db: &PgPool,
    env: Id,
    deployment: Id,
    vars: &RawVars,
) -> AppResult<Vec<u8>> {
    let json = zeroize::Zeroizing::new(serde_json::to_vec(vars).map_err(anyhow::Error::from)?);
    keys.seal(db, &json, vars_aad(env, &deployment.to_string()))
        .await
}

pub async fn open_vars_snapshot(
    keys: &Keyring,
    db: &PgPool,
    env: Id,
    deployment: Id,
    sealed: &[u8],
) -> AppResult<RawVars> {
    let plain = keys
        .open(db, sealed, vars_aad(env, &deployment.to_string()))
        .await?;
    Ok(serde_json::from_slice(&plain).map_err(anyhow::Error::from)?)
}

/// Whether a variable's value is masked in logs: its name looks like a
/// credential, or the value looks like one (long, letters and digits mixed).
/// Plain settings (`0.0.0.0`, `production`, `info`) stay readable.
pub fn worth_masking(name: &str, value: &str) -> bool {
    const SECRET_WORDS: [&str; 8] = [
        "PASSWORD",
        "PASSWD",
        "SECRET",
        "TOKEN",
        "KEY",
        "PRIVATE",
        "CREDENTIAL",
        "AUTH",
    ];
    if value.len() < Redactor::MIN_LEN {
        return false;
    }
    let n = name.to_ascii_uppercase();
    if SECRET_WORDS.iter().any(|w| n.contains(w)) {
        return true;
    }
    value.len() >= 12
        && value.bytes().any(|b| b.is_ascii_alphabetic())
        && value.bytes().any(|b| b.is_ascii_digit())
        && !value.contains(char::is_whitespace)
}

/// Hides secret values in text shown to users (logs). Matches the raw value
/// and its base64 / URL-encoded forms; only values [`worth_masking`] are
/// matched, because masking plain settings would destroy unrelated text.
pub struct Redactor {
    needles: Vec<String>,
}

impl Redactor {
    pub const MASK: &'static str = "••••••";
    const MIN_LEN: usize = 6;

    pub fn new(vars: &EnvMap) -> Self {
        use base64::Engine as _;
        let mut needles = Vec::new();
        for (name, v) in vars {
            let v = v.expose_secret();
            if !worth_masking(name, v) {
                continue;
            }
            needles.push(v.to_owned());
            needles.push(base64::engine::general_purpose::STANDARD.encode(v));
            needles.push(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v));
            let url = url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();
            needles.push(url);
        }
        // Longest first so a value containing another is masked whole.
        needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        needles.dedup();
        Self { needles }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for n in &self.needles {
            if out.contains(n.as_str()) {
                out = out.replace(n.as_str(), Self::MASK);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["A", "DATABASE_URL", "_x", "port2"] {
            validate_name(ok).unwrap();
        }
        for bad in ["", "1A", "A-B", "A B", "MODULA_TOKEN", "é"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn redacts_raw_base64_and_urlencoded() {
        let mut m = EnvMap::new();
        m.insert("K".into(), SecretString::from("s3cr3t/value+1"));
        m.insert("SHORT".into(), SecretString::from("abc"));
        let r = Redactor::new(&m);
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode("s3cr3t/value+1");
        let line = format!("raw=s3cr3t/value+1 b64={b64} url=s3cr3t%2Fvalue%2B1 abc stays");
        let out = r.redact(&line);
        assert!(!out.contains("s3cr3t"), "{out}");
        assert!(!out.contains(&b64));
        assert!(out.contains("abc stays"));
    }

    #[test]
    fn plain_settings_stay_readable() {
        let mut m = EnvMap::new();
        for (k, v) in [
            ("ANTON_API_HOST", "0.0.0.0"),
            ("NODE_ENV", "production"),
            ("PUBLIC_URL", "https://example.com"),
            ("API_KEY", "abcdefgh"),
            ("DATABASE_URL", "postgres://app:Xy7pQ2rT9w@db:5432/app"),
        ] {
            m.insert(k.into(), SecretString::from(v));
        }
        let out = Redactor::new(&m).redact(
            "listening on 0.0.0.0 in production for https://example.com key=abcdefgh db=postgres://app:Xy7pQ2rT9w@db:5432/app",
        );
        assert!(out.contains("0.0.0.0") && out.contains("production"));
        assert!(out.contains("https://example.com"), "{out}");
        assert!(
            !out.contains("abcdefgh") && !out.contains("Xy7pQ2rT9w"),
            "{out}"
        );
    }
}
