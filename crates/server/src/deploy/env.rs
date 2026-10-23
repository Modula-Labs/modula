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

/// Decrypt every variable of a service instance.
pub async fn resolve(keys: &Keyring, db: &PgPool, env: Id, instance: Id) -> AppResult<EnvMap> {
    let rows: Vec<(Id, String, Vec<u8>)> = sqlx::query_as(
        "SELECT id, name, value_sealed FROM variables WHERE service_instance_id = $1",
    )
    .bind(instance)
    .fetch_all(db)
    .await?;
    let mut out = EnvMap::new();
    for (id, name, sealed) in rows {
        out.insert(name, open_variable(keys, db, env, id, &sealed).await?);
    }
    Ok(out)
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
