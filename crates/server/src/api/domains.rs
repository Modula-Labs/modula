//! Public hostnames. Generated domains live under the instance's apps domain
//! (`MODULA_APPS_DOMAIN`) and route immediately. Custom domains route (and get
//! certificates) only after their TXT ownership record verifies.

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::domains::{self, DnsRecord, bump_serving_nodes};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

const MAX_GENERATED: i64 = 5;
const MAX_CUSTOM: i64 = 20;
/// "Check now" fans out to public resolvers; once per domain per this window.
const CHECK_COOLDOWN_SECS: i64 = 5;

/// Columns of [`Row`], shared by every query that returns domains.
macro_rules! domain_columns {
    () => {
        "id, service_instance_id, hostname, kind, status, target_port, verification_token,
         last_error, dns_target_ok, checked_at, verified_at, rate_limit_per_minute, created_at"
    };
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Id,
    service_instance_id: Id,
    hostname: String,
    kind: String,
    status: String,
    target_port: Option<i32>,
    verification_token: Option<String>,
    last_error: Option<String>,
    dns_target_ok: Option<bool>,
    checked_at: Option<OffsetDateTime>,
    verified_at: Option<OffsetDateTime>,
    rate_limit_per_minute: Option<i32>,
    created_at: OffsetDateTime,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Domain {
    pub id: Id,
    pub hostname: String,
    /// `generated` or `custom`.
    pub kind: String,
    /// `pending`, `verified` or `failed`; generated domains are always verified.
    pub status: String,
    /// None = the service's port.
    pub target_port: Option<i32>,
    pub last_error: Option<String>,
    /// Whether the routing record pointed at us on the last check.
    pub dns_target_ok: Option<bool>,
    /// Records to create at the DNS host (custom domains only).
    pub dns_records: Vec<DnsRecord>,
    pub rate_limit_per_minute: Option<i32>,
    /// False until the edge's Caddy build includes the rate-limit module.
    pub rate_limit_enforced: bool,
    /// Set when automatic DNS at the workspace's provider failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_note: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub checked_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub verified_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

fn to_api(r: Row, target: &domains::Target) -> Domain {
    let dns_records = match (&r.kind[..], &r.verification_token) {
        ("custom", Some(token)) => domains::required_records(&r.hostname, token, target),
        _ => Vec::new(),
    };
    Domain {
        id: r.id,
        hostname: r.hostname,
        kind: r.kind,
        status: r.status,
        target_port: r.target_port,
        last_error: r.last_error,
        dns_target_ok: r.dns_target_ok,
        dns_records,
        rate_limit_per_minute: r.rate_limit_per_minute,
        rate_limit_enforced: false,
        provider_note: None,
        checked_at: r.checked_at,
        verified_at: r.verified_at,
        created_at: r.created_at,
    }
}

async fn fetch_one(state: &AppState, id: Id) -> AppResult<Domain> {
    let row: Row = sqlx::query_as(concat!(
        "SELECT ",
        domain_columns!(),
        " FROM domains WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let target = domains::target(&state.db, row.service_instance_id).await?;
    Ok(to_api(row, &target))
}

pub async fn list(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Vec<Domain>>> {
    let rows: Vec<Row> = sqlx::query_as(concat!(
        "SELECT ",
        domain_columns!(),
        " FROM domains WHERE service_instance_id = $1 ORDER BY created_at"
    ))
    .bind(grant.id())
    .fetch_all(&state.db)
    .await?;
    let target = domains::target(&state.db, grant.id()).await?;
    Ok(Json(rows.into_iter().map(|r| to_api(r, &target)).collect()))
}

/// `<service>-<environment>-<4 random>`, trimmed so the label stays within
/// 63 characters. `None` is the project's default environment, which omits it.
fn generated_label(service: &str, environment: Option<&str>) -> anyhow::Result<String> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let rand = modula_crypto::random_bytes::<4>()?;
    let suffix: String = rand
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    let base = match environment {
        None => service.to_owned(),
        Some(e) => {
            let env: String = e
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect();
            format!("{service}-{env}")
        }
    };
    let base: String = base.chars().take(63 - 5).collect();
    Ok(format!("{}-{suffix}", base.trim_matches('-')))
}

/// Insert a generated domain `<label>.<apps>` for `instance` (see
/// [`generated_label`]; `environment` None = the project's default one).
pub(crate) async fn insert_generated(
    tx: &mut sqlx::PgConnection,
    instance: Id,
    service: &str,
    environment: Option<&str>,
    apps: &str,
) -> AppResult<(Id, String)> {
    let id = Id::new();
    // A collision on the random suffix is astronomically unlikely; retry once.
    for _ in 0..2 {
        let hostname = format!("{}.{apps}", generated_label(service, environment)?);
        let inserted = sqlx::query(
            "INSERT INTO domains (id, service_instance_id, hostname, kind) VALUES ($1, $2, $3, 'generated')
             ON CONFLICT (hostname) DO NOTHING",
        )
        .bind(id)
        .bind(instance)
        .bind(&hostname)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 1 {
            return Ok((id, hostname));
        }
    }
    Err(anyhow::anyhow!("no free generated domain for {service}").into())
}

pub async fn generate(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Domain>> {
    let instance = grant.id();
    let apps = state.apps_domain().ok_or_else(|| {
        AppError::Conflict(
            "generated domains need an apps domain: add one in Settings → Domain (or use a custom domain)".into(),
        )
    })?;
    let (service, environment, default): (String, String, bool) = sqlx::query_as(
        "SELECT s.name, e.name,
                e.id = (SELECT f.id FROM environments f WHERE f.project_id = e.project_id
                        ORDER BY f.created_at, f.id LIMIT 1)
         FROM service_instances i
         JOIN services s ON s.id = i.service_id JOIN environments e ON e.id = i.environment_id
         WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_one(&state.db)
    .await?;
    let mut tx = state.db.begin().await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM domains WHERE service_instance_id = $1 AND kind = 'generated'",
    )
    .bind(instance)
    .fetch_one(&mut *tx)
    .await?;
    if count >= MAX_GENERATED {
        return Err(AppError::Conflict(format!(
            "a service can have at most {MAX_GENERATED} generated domains"
        )));
    }
    let (id, hostname) = insert_generated(
        &mut tx,
        instance,
        &service,
        (!default).then_some(environment.as_str()),
        &apps,
    )
    .await?;
    let nodes = bump_serving_nodes(&mut tx, instance).await?;
    Entry::new("domain.create", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("domain", id)
        .metadata(json!({ "hostname": hostname, "serviceInstance": instance }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&nodes);
    Ok(Json(fetch_one(&state, id).await?))
}

fn check_port(port: Option<i32>) -> AppResult<()> {
    match port {
        Some(p) if !(1..=65535).contains(&p) => {
            Err(AppError::bad_request("the target port must be 1–65535"))
        }
        _ => Ok(()),
    }
}

fn check_rate_limit(limit: Option<i32>) -> AppResult<()> {
    match limit {
        Some(l) if !(1..=1_000_000).contains(&l) => Err(AppError::bad_request(
            "the rate limit must be 1–1,000,000 requests per minute",
        )),
        _ => Ok(()),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateCustom {
    hostname: String,
    #[serde(default)]
    target_port: Option<i32>,
}

pub async fn create_custom(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(body): Json<CreateCustom>,
) -> AppResult<Json<Domain>> {
    let instance = grant.id();
    let workspace = grant.workspace_id()?;
    let host = domains::parse_custom(&body.hostname, &state.cfg, &state.hosts())
        .map_err(AppError::BadRequest)?;
    check_port(body.target_port)?;
    let provider = domains::provider_for(&state, workspace).await;
    if host.wildcard && !provider.as_ref().is_some_and(|p| p.supports_dns01()) {
        return Err(AppError::bad_request(
            "wildcard domains need DNS-01 certificates, which need a DNS provider \
             (Cloudflare) connected to this workspace. Add each subdomain instead.",
        ));
    }
    let token = domains::new_token()?;
    let id = Id::new();
    let mut tx = state.db.begin().await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM domains WHERE service_instance_id = $1 AND kind = 'custom'",
    )
    .bind(instance)
    .fetch_one(&mut *tx)
    .await?;
    if count >= MAX_CUSTOM {
        return Err(AppError::Conflict(format!(
            "a service can have at most {MAX_CUSTOM} custom domains"
        )));
    }
    // Hostnames are unique across the whole instance: one name, one service.
    let inserted = sqlx::query(
        "INSERT INTO domains (id, service_instance_id, hostname, kind, status, target_port, verification_token)
         VALUES ($1, $2, $3, 'custom', 'pending', $4, $5)
         ON CONFLICT (hostname) DO NOTHING",
    )
    .bind(id)
    .bind(instance)
    .bind(&host.name)
    .bind(body.target_port)
    .bind(&token)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(AppError::Conflict(format!(
            "{} is already in use by a service",
            host.name
        )));
    }
    Entry::new("domain.create", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("domain", id)
        .metadata(json!({
            "hostname": host.name,
            "kind": "custom",
            "serviceInstance": instance,
            "targetPort": body.target_port,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    // Unverified: nothing routes yet, so no node needs a new snapshot.
    let mut domain = fetch_one(&state, id).await?;
    if provider.is_some() {
        domain.provider_note = domains::sync_records(
            &state,
            workspace,
            &host.name,
            &domain.dns_records,
            false,
            false,
        )
        .await;
    }
    Ok(Json(domain))
}

/// "Check now": look up the DNS records immediately.
pub async fn check(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Domain>> {
    let id = grant.id();
    let (kind, wait): (String, Option<f64>) = sqlx::query_as(
        "SELECT kind, EXTRACT(EPOCH FROM checked_at + make_interval(secs => $2) - now())::float8
         FROM domains WHERE id = $1",
    )
    .bind(id)
    .bind(CHECK_COOLDOWN_SECS as f64)
    .fetch_one(&state.db)
    .await?;
    if kind != "custom" {
        return Err(AppError::bad_request("generated domains need no DNS check"));
    }
    if let Some(w) = wait.filter(|w| *w > 0.0) {
        return Err(AppError::TooManyRequests {
            retry_after_secs: w.ceil() as u64,
        });
    }
    domains::check(&state, id, Some((&actor, &meta))).await?;
    Ok(Json(fetch_one(&state, id).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateDomain {
    /// Replaces the stored value; null = the service's port.
    target_port: Option<i32>,
    /// Replaces the stored value; null = no limit.
    rate_limit_per_minute: Option<i32>,
}

/// Port and edge settings (both fields replace the stored values).
pub async fn update(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(body): Json<UpdateDomain>,
) -> AppResult<Json<Domain>> {
    let id = grant.id();
    check_port(body.target_port)?;
    check_rate_limit(body.rate_limit_per_minute)?;
    let mut tx = state.db.begin().await?;
    let (instance, hostname): (Id, String) = sqlx::query_as(
        "UPDATE domains SET target_port = $2, rate_limit_per_minute = $3 WHERE id = $1
         RETURNING service_instance_id, hostname",
    )
    .bind(id)
    .bind(body.target_port)
    .bind(body.rate_limit_per_minute)
    .fetch_one(&mut *tx)
    .await?;
    let nodes = bump_serving_nodes(&mut tx, instance).await?;
    Entry::new("domain.update", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("domain", id)
        .metadata(json!({
            "hostname": hostname,
            "targetPort": body.target_port,
            "rateLimitPerMinute": body.rate_limit_per_minute,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&nodes);
    Ok(Json(fetch_one(&state, id).await?))
}

pub async fn delete(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let id = grant.id();
    let workspace = grant.workspace_id()?;
    // The records to clean up at the DNS provider, computed before the row goes.
    let before = fetch_one(&state, id).await?;
    let mut tx = state.db.begin().await?;
    let (instance, hostname): (Id, String) =
        sqlx::query_as("DELETE FROM domains WHERE id = $1 RETURNING service_instance_id, hostname")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let nodes = bump_serving_nodes(&mut tx, instance).await?;
    Entry::new("domain.delete", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("domain", id)
        .metadata(json!({ "hostname": hostname }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&nodes);
    let note = if before.kind == "custom" {
        domains::sync_records(
            &state,
            workspace,
            &hostname,
            &before.dns_records,
            true,
            false,
        )
        .await
    } else {
        None
    };
    Ok(Json(json!({ "ok": true, "providerNote": note })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        let l = generated_label("api", None).unwrap();
        assert!(l.starts_with("api-") && l.len() == 8, "{l}");
        let l = generated_label("api", Some("Staging EU")).unwrap();
        assert!(l.starts_with("api-staging-eu-"), "{l}");
        let long = "a".repeat(63);
        assert!(generated_label(&long, Some(&long)).unwrap().len() <= 63);
    }
}
