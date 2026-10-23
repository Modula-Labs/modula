//! Variable encryption. Every value is sealed with its *environment's* data key
//! (`data_keys.scope = environment id`), so environments never share a key and
//! one can be rotated or destroyed alone.

use std::collections::BTreeMap;

use modula_crypto::envelope::FieldAad;
use modula_id::Id;
use secrecy::{ExposeSecret, SecretString};
use sqlx::PgPool;

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
    use super::refs::{Expander, VarKey};

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
                   AND d.status IN ('active', 'crashed') ORDER BY d.created_at DESC LIMIT 1)
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
    let rows: Vec<(Id, Option<String>, String, Vec<u8>)> = sqlx::query_as(
        "SELECT v.id, s.name, v.name, v.value_sealed FROM variables v
         LEFT JOIN service_instances i ON i.id = v.service_instance_id
         LEFT JOIN services s ON s.id = i.service_id
         WHERE v.environment_id = $1",
    )
    .bind(env)
    .fetch_all(db)
    .await?;

    let mut raw: BTreeMap<VarKey, String> = BTreeMap::new();
    let mut user: Vec<VarKey> = Vec::new();
    for (id, owner, name, sealed) in rows {
        user.push((owner.clone(), name.clone()));
        let value = open_variable(keys, db, env, id, &sealed).await?;
        raw.insert((owner, name), value.expose_secret().to_owned());
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
        let tcp_host = match tcp_port {
            Some(_) => tcp_proxy_host
                .or(node_ip.as_deref())
                .unwrap_or_default()
                .to_owned(),
            None => String::new(),
        };
        let port = config.get("port").and_then(|p| p.as_u64()).unwrap_or(8080);
        let platform = [
            ("PORT", port.to_string()),
            (
                "MODULA_PRIVATE_DOMAIN",
                format!("{service}.modula.internal"),
            ),
            ("MODULA_PUBLIC_DOMAIN", domain.unwrap_or_default()),
            ("MODULA_SERVICE_NAME", service.clone()),
            ("MODULA_SERVICE_ID", service_id.to_string()),
            (
                "MODULA_DEPLOYMENT_ID",
                deployment_id.map(|d| d.to_string()).unwrap_or_default(),
            ),
            ("MODULA_ENVIRONMENT_NAME", env_name),
            ("MODULA_ENVIRONMENT_ID", env.to_string()),
            ("MODULA_PROJECT_ID", project.to_string()),
            ("MODULA_TCP_PROXY_DOMAIN", tcp_host),
            (
                "MODULA_TCP_PROXY_PORT",
                tcp_port.map(|p| p.to_string()).unwrap_or_default(),
            ),
        ];
        for (k, v) in platform {
            // A user PORT overrides the platform one.
            raw.entry((Some(service.clone()), k.to_owned()))
                .or_insert(v);
        }
        if inst == instance {
            own = Some(service);
        }
    }
    let own = own.ok_or(AppError::NotFound)?;

    // Only the instance's own user variables; desired state adds platform ones.
    let names: Vec<VarKey> = user
        .into_iter()
        .filter(|(owner, _)| owner.as_deref() == Some(own.as_str()))
        .collect();
    let mut expander = Expander::new(&raw);
    let mut vars = EnvMap::new();
    for key in names {
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

/// Hides secret values in text shown to users (logs). Matches the raw value
/// and its base64 / URL-encoded forms; very short values are skipped because
/// masking them would destroy unrelated text.
pub struct Redactor {
    needles: Vec<String>,
}

impl Redactor {
    pub const MASK: &'static str = "••••••";
    const MIN_LEN: usize = 6;

    pub fn new(vars: &EnvMap) -> Self {
        use base64::Engine as _;
        let mut needles = Vec::new();
        for v in vars.values() {
            let v = v.expose_secret();
            if v.len() < Self::MIN_LEN {
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
}
